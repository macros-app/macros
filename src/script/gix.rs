use std::collections::HashMap;

/// One commit's metadata row, shared by `gix-log` and `gix-blame`:
/// `"<shorthash>\t<author>\t<YYYY-MM-DD>\t<summary>"`. Tab-separated because
/// author names and commit summaries contain spaces and colons, so the
/// `path:line:text` colon convention `grep-files` uses wouldn't round-trip. The
/// date is in the author's own zone, as `git log --date=short` prints it.
pub(crate) fn gix_commit_row(
    repo: &gix::Repository,
    id: &gix::ObjectId,
) -> Result<String, Box<dyn std::error::Error>> {
    let commit = repo.find_commit(*id)?;
    let short = id.to_hex_with_len(8).to_string();
    let author = commit.author()?;
    let name = author.name.to_string();
    let date = git_date(author.time()?, "%Y-%m-%d");
    let summary = commit.message()?.summary().to_string();
    Ok(format!("{short}\t{name}\t{date}\t{summary}"))
}

/// A git timestamp formatted (strftime-style) in its own UTC offset, as git
/// prints author dates; "" for a time or offset out of range.
fn git_date(time: gix::date::Time, fmt: &str) -> String {
    let (Ok(ts), Ok(off)) = (
        jiff::Timestamp::from_second(time.seconds),
        jiff::tz::Offset::from_seconds(time.offset),
    ) else {
        return String::new();
    };
    let zoned = ts.to_zoned(jiff::tz::TimeZone::fixed(off));
    jiff::fmt::strtime::format(fmt, &zoned).unwrap_or_default()
}

/// One `git log --oneline`-style row: `"<short7>[ (<decorations>)] <summary>"`
/// — 7-char abbreviation matching git's default. Shared by the oneline /
/// decorated / range / file log backends; `decos` is the [`ref_decorations`]
/// map for the decorated variants (`None` = undecorated).
pub(crate) fn gix_oneline_row(
    repo: &gix::Repository,
    id: gix::ObjectId,
    decos: Option<&HashMap<gix::ObjectId, Vec<String>>>,
) -> Result<String, Box<dyn std::error::Error>> {
    let summary = repo.find_commit(id)?.message()?.summary().to_string();
    let deco = decos
        .and_then(|d| d.get(&id))
        .map(|v| format!(" ({})", v.join(", ")))
        .unwrap_or_default();
    Ok(format!("{}{} {}", id.to_hex_with_len(7), deco, summary))
}

/// `(gix-log dir n)` backend: up to `n` commits reachable from HEAD, newest
/// first, each a commit row. Errors (not a repo, unborn HEAD) surface to Scheme
/// as an empty list, the same way `walk-files` yields nothing for a missing dir.
pub(crate) fn gix_log(dir: &str, max: usize) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let repo = gix::discover(dir)?;
    let head = repo.head_id()?;
    let mut rows = Vec::new();
    for info in repo.rev_walk(Some(head.detach())).all()?.take(max) {
        rows.push(gix_commit_row(&repo, &info?.id)?);
    }
    Ok(rows)
}

/// Shared core of [`gix_blame`] / [`gix_blame_text`]: blame `file` (absolute,
/// or relative to `dir`) at HEAD, returning the repo, the file's bytes, and the
/// blame entries sorted by line. `None` when the alignment guard trips (file
/// untracked at HEAD or differing from it) — the caller's cue to fall back to
/// `git blame`.
#[allow(clippy::type_complexity)]
fn blame_head(
    dir: &str,
    file: &str,
) -> Result<
    Option<(gix::Repository, Vec<u8>, Vec<gix::blame::BlameEntry>)>,
    Box<dyn std::error::Error>,
> {
    use gix::bstr::BStr;
    let repo = gix::discover(dir)?;
    // Resolve `file` (absolute, or relative to `dir`) to a repo-root-relative
    // path, the way `git blame -C dir -- file` does — callers (e.g. magit) pass
    // the buffer's path, not a path already relative to the repo root.
    let abs = {
        let p = std::path::Path::new(file);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::path::Path::new(dir).join(p)
        }
    };
    let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
    let rel = match repo.workdir().and_then(|wd| std::fs::canonicalize(wd).ok()) {
        Some(wd) => abs
            .strip_prefix(&wd)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| abs.clone()),
        None => abs.clone(),
    };

    // Bail to git unless the working-tree file matches HEAD's blob byte-for-byte.
    let Some(entry) = repo.head_tree()?.lookup_entry_by_path(&rel)? else {
        return Ok(None); // not tracked at HEAD → let git handle it
    };
    let data = std::fs::read(&abs)?;
    if entry.object()?.data != data {
        return Ok(None); // working tree differs from HEAD → fall back to git
    }
    drop(entry);

    let rel = rel.to_string_lossy();
    let head = repo.head_id()?.detach();
    let opts = gix::repository::blame_file::Options::default();
    let outcome = repo.blame_file(BStr::new(rel.as_bytes()), head, opts)?;
    let mut entries = outcome.entries;
    entries.sort_by_key(|e| e.start_in_blamed_file);
    Ok(Some((repo, data, entries)))
}

/// `(gix-blame dir file)` backend: one row per line of `file` (absolute or
/// relative to `dir`) as committed at HEAD, in file order:
/// `"<1-based-line>\t<sha>\t<author>\t<author-epoch>\t<summary>"` (full 40-hex
/// SHA, author time as Unix seconds — the fields magit's blame UI needs).
///
/// Alignment guard: gix blames HEAD's committed blob, but the buffer shows the
/// *working-tree* file. The two only line up row-for-row when they're
/// byte-identical, so when the on-disk file differs from HEAD (uncommitted
/// edits) — or isn't tracked at HEAD — we return an empty list. magit treats
/// that as "gix can't" and falls back to `git blame`, which blames the working
/// tree (and marks uncommitted lines). This is the "gix where possible, git
/// otherwise" boundary.
pub(crate) fn gix_blame(dir: &str, file: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let Some((repo, _data, entries)) = blame_head(dir, file)? else {
        return Ok(Vec::new());
    };

    // Resolve each commit's "sha\tauthor\tepoch\tsummary" once — adjacent blame
    // hunks often share one.
    let mut cache: HashMap<gix::ObjectId, String> = HashMap::new();
    let mailmap = repo.open_mailmap(); // git blame maps authors through .mailmap
    let mut rows = Vec::new();
    for entry in entries {
        let meta = match cache.get(&entry.commit_id) {
            Some(m) => m.clone(),
            None => {
                let commit = repo.find_commit(entry.commit_id)?;
                let author = commit.author()?;
                let m = format!(
                    "{}\t{}\t{}\t{}",
                    entry.commit_id.to_hex(),
                    mailmap.resolve(author).name,
                    author.seconds(),
                    commit.message()?.summary()
                );
                cache.insert(entry.commit_id, m.clone());
                m
            }
        };
        for i in 0..entry.len.get() {
            // `start_in_blamed_file` is 0-based; editor lines are 1-based.
            rows.push(format!("{}\t{}", entry.start_in_blamed_file + i + 1, meta));
        }
    }
    Ok(rows)
}

/// `(gix-blame-text dir file)` backend: `git blame --date=short`-format text
/// for `file` at HEAD — `<hash> (<author> <YYYY-MM-DD> <line>) <content>`,
/// author padded to the widest name and the line number right-aligned like
/// git. Root commits get git's `^` boundary marker. Empty when the alignment
/// guard trips (see [`blame_head`]); the caller falls back to `git blame`.
pub(crate) fn gix_blame_text(dir: &str, file: &str) -> Result<String, Box<dyn std::error::Error>> {
    let Some((repo, data, entries)) = blame_head(dir, file)? else {
        return Ok(String::new());
    };
    // Per commit: (is root, author, date in the author's own UTC offset — what
    // `--date=short` prints).
    let mut meta: HashMap<gix::ObjectId, (bool, String, String)> = HashMap::new();
    // git's hash column: the longest unique abbreviation among the blamed
    // commits (honouring `core.abbrev` / auto-scaling), plus one so a root
    // commit's `^` boundary marker fits in the same width.
    let mut abbrev = 0;
    let mailmap = repo.open_mailmap(); // git blame maps authors through .mailmap
    for entry in &entries {
        if meta.contains_key(&entry.commit_id) {
            continue;
        }
        let commit = repo.find_commit(entry.commit_id)?;
        let author = commit.author()?;
        let time = author.time()?;
        let date = git_date(time, "%Y-%m-%d");
        let is_root = commit.parent_ids().next().is_none();
        abbrev = abbrev.max(commit.id().shorten_or_id().hex_len() + 1);
        let name = mailmap.resolve(author).name.to_string();
        meta.insert(entry.commit_id, (is_root, name, date));
    }
    let abbrev = abbrev.min(
        entries
            .first()
            .map_or(40, |e| e.commit_id.kind().len_in_hex()),
    );
    let lines = diff_lines(&data);
    let author_w = meta
        .values()
        .map(|m| m.1.chars().count())
        .max()
        .unwrap_or(0);
    let line_w = lines.len().to_string().len();
    let mut out = String::new();
    for entry in &entries {
        let (is_root, author, date) = &meta[&entry.commit_id];
        let hash = if *is_root {
            format!("^{}", entry.commit_id.to_hex_with_len(abbrev - 1))
        } else {
            entry.commit_id.to_hex_with_len(abbrev).to_string()
        };
        for i in 0..entry.len.get() {
            let n = (entry.start_in_blamed_file + i) as usize;
            let content = lines.get(n).copied().unwrap_or_default();
            let content = String::from_utf8_lossy(content.strip_suffix(b"\n").unwrap_or(content));
            out.push_str(&format!(
                "{hash} ({author:<author_w$} {date} {:>line_w$}) {content}\n",
                n + 1
            ));
        }
    }
    Ok(out)
}

/// `(gix-git-dir dir)` backend: the absolute git dir of the repo enclosing
/// `dir` (`.git`, or `.git/worktrees/<name>` in a linked worktree), or "" when
/// `dir` isn't in a repo. The native form of `git rev-parse --absolute-git-dir`.
pub(crate) fn gix_git_dir(dir: &str) -> String {
    gix::discover(dir)
        .ok()
        .and_then(|repo| std::path::absolute(repo.git_dir()).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Which ref category [`gix_ref_names`] lists — the `git branch` /
/// `git branch -r` / `git tag` split.
#[derive(Clone, Copy)]
pub(crate) enum RefKind {
    Local,
    Remote,
    Tag,
}

/// Sorted short ref names for one category — backs `gix-branches` (local
/// branches), `gix-remote-branches` (`origin/main`…), and `gix-tags`. `kind`
/// picks the iterator the way `git branch` / `git branch -r` / `git tag` do.
pub(crate) fn gix_ref_names(
    dir: &str,
    kind: RefKind,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let repo = gix::discover(dir)?;
    let platform = repo.references()?;
    let iter = match kind {
        RefKind::Local => platform.local_branches()?,
        RefKind::Remote => platform.remote_branches()?,
        RefKind::Tag => platform.tags()?,
    };
    let mut names: Vec<String> = iter
        .filter_map(|r| r.ok())
        .map(|r| r.name().shorten().to_string())
        .collect();
    names.sort();
    Ok(names)
}

/// `(gix-remotes dir)` backend: configured remote names (`origin`…), sorted.
pub(crate) fn gix_remotes(dir: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let repo = gix::discover(dir)?;
    Ok(repo.remote_names().iter().map(|n| n.to_string()).collect())
}

/// `(gix-state dir)` backend: `"merge"` while a merge is in progress (MERGE_HEAD
/// present), `"rebase"` during a rebase (the rebase-merge/rebase-apply state
/// dirs), else `""`. Replaces magit's `rev-parse --verify MERGE_HEAD/REBASE_HEAD`
/// probes — git records this state on disk in the git dir, so we read it there.
pub(crate) fn gix_state(dir: &str) -> String {
    let Ok(repo) = gix::discover(dir) else {
        return String::new();
    };
    let git_dir = repo.git_dir();
    if git_dir.join("MERGE_HEAD").exists() {
        "merge".into()
    } else if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
        "rebase".into()
    } else {
        String::new()
    }
}

/// `(gix-head-message dir full?)` backend: the HEAD commit's summary (`full` =
/// false → `git log -1 --format=%s`) or full message (`full` = true → `%B`).
/// Empty string when there are no commits yet.
pub(crate) fn gix_head_message(dir: &str, full: bool) -> String {
    let inner = || -> Result<String, Box<dyn std::error::Error>> {
        let repo = gix::discover(dir)?;
        let id = repo.head_id()?.detach();
        let commit = repo.find_commit(id)?;
        if full {
            Ok(commit.message_raw()?.to_string())
        } else {
            Ok(commit.message()?.summary().to_string())
        }
    };
    inner().unwrap_or_default()
}

/// `(gix-log-oneline dir n)` backend: up to `n` commits from HEAD, newest first,
/// each `"<shorthash> <summary>"` — the native form of `git log --oneline -n N`.
/// 7-char abbreviation matches git's default; magit accents the first token as
/// the hash and uses it as a `git show` ref.
pub(crate) fn gix_log_oneline(
    dir: &str,
    max: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let repo = gix::discover(dir)?;
    let head = repo.head_id()?;
    let mut rows = Vec::new();
    for info in repo.rev_walk(Some(head.detach())).all()?.take(max) {
        rows.push(gix_oneline_row(&repo, info?.id, None)?);
    }
    Ok(rows)
}

// --- gix diff generation (git-format unified diffs without a subprocess) -----
// Backs `gix-diff` (index↔worktree) and `gix-diff-cached` (HEAD↔index). The
// output is `git diff`-format text so magit's `build-diff-map`/`parse-diff`
// parse it unchanged AND it round-trips through `git apply --cached` (hunk
// staging reuses the same text) — see the `gix_diff` round-trip test.

/// Split `data` into lines that retain their trailing newline (the final line
/// may lack one) — the exact tokenization gix-imara-diff uses, so our line
/// indices line up with the hunk ranges it returns.
pub(crate) fn diff_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let n = rest
            .iter()
            .position(|&b| b == b'\n')
            .map_or(rest.len(), |i| i + 1);
        let (line, r) = rest.split_at(n);
        out.push(line);
        rest = r;
    }
    out
}

/// Emit one hunk line: `prefix` + content, adding git's
/// `\ No newline at end of file` marker when the source line has no terminator
/// (imara's printer omits it — the round-trip footgun).
pub(crate) fn diff_emit_line(out: &mut String, prefix: char, line: &[u8]) {
    out.push(prefix);
    out.push_str(&String::from_utf8_lossy(line));
    if !line.ends_with(b"\n") {
        out.push('\n');
        out.push_str("\\ No newline at end of file\n");
    }
}

/// A unified `@@` range field: git omits `,1` and writes `start,0` for an empty
/// side (pure insertion/deletion).
pub(crate) fn diff_hunk_range(start0: u32, len: u32) -> String {
    if len == 1 {
        format!("{}", start0 + 1)
    } else if len == 0 {
        format!("{},0", start0)
    } else {
        format!("{},{}", start0 + 1, len)
    }
}

/// The unified hunks (everything after the file header) for `old` → `new`, or
/// `None` when identical. Context = 3, hunks within 2*context merged — matching
/// `git diff` closely enough for both display and `git apply`.
pub(crate) fn diff_hunks(old: &[u8], new: &[u8]) -> Option<String> {
    use gix::diff::blob::{Algorithm, Diff, InternedInput};
    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let hunks: Vec<gix::diff::blob::Hunk> = diff.hunks().collect();
    if hunks.is_empty() {
        return None;
    }
    let old_lines = diff_lines(old);
    let new_lines = diff_lines(new);
    let ctx = 3u32;
    let mut out = String::new();
    let mut i = 0;
    while i < hunks.len() {
        // Merge hunks whose gap is within 2*context into one displayed hunk.
        let mut j = i;
        while j + 1 < hunks.len()
            && hunks[j + 1]
                .before
                .start
                .saturating_sub(hunks[j].before.end)
                <= 2 * ctx
        {
            j += 1;
        }
        let bs = hunks[i].before.start.saturating_sub(ctx);
        let be = (hunks[j].before.end + ctx).min(old_lines.len() as u32);
        let a0 = hunks[i].after.start.saturating_sub(ctx);
        let ae = (hunks[j].after.end + ctx).min(new_lines.len() as u32);
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            diff_hunk_range(bs, be - bs),
            diff_hunk_range(a0, ae - a0)
        ));
        let mut bpos = bs;
        for h in &hunks[i..=j] {
            for k in bpos..h.before.start {
                diff_emit_line(&mut out, ' ', old_lines[k as usize]);
            }
            for k in h.before.start..h.before.end {
                diff_emit_line(&mut out, '-', old_lines[k as usize]);
            }
            for k in h.after.start..h.after.end {
                diff_emit_line(&mut out, '+', new_lines[k as usize]);
            }
            bpos = h.before.end;
        }
        for k in bpos..be {
            diff_emit_line(&mut out, ' ', old_lines[k as usize]);
        }
        i = j + 1;
    }
    Some(out)
}

/// git's octal mode string for a committed-tree entry.
pub(crate) fn tree_mode_str(mode: gix::objs::tree::EntryMode) -> &'static str {
    use gix::objs::tree::EntryKind;
    match mode.kind() {
        EntryKind::BlobExecutable => "100755",
        EntryKind::Link => "120000",
        EntryKind::Commit => "160000",
        _ => "100644",
    }
}

/// git's octal mode string for an index entry.
pub(crate) fn index_mode_str(mode: gix::index::entry::Mode) -> &'static str {
    use gix::index::entry::Mode;
    // COMMIT (0o160000) shares SYMLINK's bits, so test it first.
    if mode.is_submodule() {
        "160000"
    } else if mode.contains(Mode::SYMLINK) {
        "120000"
    } else if mode.contains(Mode::FILE_EXECUTABLE) {
        "100755"
    } else {
        "100644"
    }
}

/// git's binary heuristic: a NUL byte in the first 8000 bytes. Running the line
/// differ over binary content would dump raw bytes into the diff (garbling the
/// magit buffer and breaking its file/hunk parsing), so we detect it first.
pub(crate) fn diff_is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|&b| b == 0)
}

/// `diff --git`-format text for one changed file. `old`/`new` are `None` for an
/// addition/deletion respectively; the `&str` is that side's git mode.
pub(crate) fn diff_one_file(
    path: &str,
    old: Option<(&[u8], &str)>,
    new: Option<(&[u8], &str)>,
) -> String {
    let empty: &[u8] = &[];
    let oc = old.map_or(empty, |(c, _)| c);
    let nc = new.map_or(empty, |(c, _)| c);
    // A mode change (e.g. `chmod +x`) gets git's `old mode`/`new mode` header,
    // and is a change on its own even when the content is identical.
    let mode_header = match (old, new) {
        (Some((_, om)), Some((_, nm))) if om != nm => format!("old mode {om}\nnew mode {nm}\n"),
        _ => String::new(),
    };

    // Binary files: emit `Binary files … differ` with no body, exactly like git —
    // no content, so nothing to garble or to fail to parse.
    if oc != nc && (diff_is_binary(oc) || diff_is_binary(nc)) {
        let mut out = format!("diff --git a/{path} b/{path}\n{mode_header}");
        match (old, new) {
            (None, Some((_, mode))) => out.push_str(&format!(
                "new file mode {mode}\nBinary files /dev/null and b/{path} differ\n"
            )),
            (Some((_, mode)), None) => out.push_str(&format!(
                "deleted file mode {mode}\nBinary files a/{path} and /dev/null differ\n"
            )),
            _ => out.push_str(&format!("Binary files a/{path} and b/{path} differ\n")),
        }
        return out;
    }

    let Some(hunks) = diff_hunks(oc, nc) else {
        if !mode_header.is_empty() {
            // Mode-only change: just the header, no hunks (as git prints it).
            return format!("diff --git a/{path} b/{path}\n{mode_header}");
        }
        return String::new(); // identical → nothing (callers pre-filter, but be safe)
    };
    let mut out = format!("diff --git a/{path} b/{path}\n{mode_header}");
    match (old, new) {
        (None, Some((_, mode))) => {
            out.push_str(&format!(
                "new file mode {mode}\n--- /dev/null\n+++ b/{path}\n"
            ));
        }
        (Some((_, mode)), None) => {
            out.push_str(&format!(
                "deleted file mode {mode}\n--- a/{path}\n+++ /dev/null\n"
            ));
        }
        _ => out.push_str(&format!("--- a/{path}\n+++ b/{path}\n")),
    }
    out.push_str(&hunks);
    out
}

/// A path's blob id + git mode on one side of a diff (a tree or the index).
pub(crate) type GixSide = (gix::ObjectId, &'static str);

/// Git mode of a submodule (gitlink) entry. Its id names a commit in the
/// *submodule's* repo, so it must never be looked up as a blob here.
pub(crate) const GITLINK_MODE: &str = "160000";

/// The bytes git diffs for a gitlink: `Subproject commit <hex>` — exactly what
/// `git diff` shows for a moved submodule, so magit parses it as a normal hunk.
pub(crate) fn gitlink_text(id: &gix::oid) -> Vec<u8> {
    format!("Subproject commit {}\n", id.to_hex()).into_bytes()
}

/// Content of one side of a diff: the blob for files/symlinks, the synthetic
/// `Subproject commit` line for gitlinks.
fn side_content(
    repo: &gix::Repository,
    id: gix::ObjectId,
    mode: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if mode == GITLINK_MODE {
        Ok(gitlink_text(&id))
    } else {
        Ok(repo.find_object(id)?.data.clone())
    }
}

/// Flatten a tree to `path → (id, mode)` for every file it contains.
/// `.files()` records directories (trees) too; we keep only blobs/symlinks and
/// submodules (commits), else a "deleted directory" would diff the raw tree
/// object — bytes that garble the buffer and break magit's parser. Submodule
/// ids are rendered via [`gitlink_text`], never read as objects.
pub(crate) fn tree_side_map(
    tree: &gix::Tree<'_>,
) -> Result<std::collections::BTreeMap<Vec<u8>, GixSide>, Box<dyn std::error::Error>> {
    Ok(tree
        .traverse()
        .breadthfirst
        .files()?
        .into_iter()
        .filter(|e| e.mode.is_blob_or_symlink() || e.mode.is_commit())
        .map(|e| (Vec::from(e.filepath), (e.oid, tree_mode_str(e.mode))))
        .collect())
}

/// `git diff`-format text between two `path → (id, mode)` sides (tree↔tree or
/// tree↔index). Shared by `gix-diff-cached` and `gix-show`.
pub(crate) fn diff_side_maps(
    repo: &gix::Repository,
    old: &std::collections::BTreeMap<Vec<u8>, GixSide>,
    new: &std::collections::BTreeMap<Vec<u8>, GixSide>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut keys: Vec<&Vec<u8>> = old.keys().chain(new.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut out = String::new();
    // Content of an optional side; `Err(())` = unreadable object.
    let content = |side: Option<&GixSide>| -> Result<Option<(Vec<u8>, &'static str)>, ()> {
        match side {
            Some((id, m)) => side_content(repo, *id, m)
                .map(|c| Some((c, *m)))
                .map_err(|_| ()),
            None => Ok(None),
        }
    };
    for k in keys {
        let (o, n) = match (old.get(k), new.get(k)) {
            (Some(o), Some(n)) if o != n => (Some(o), Some(n)),
            (None, Some(n)) => (None, Some(n)),
            (Some(o), None) => (Some(o), None),
            _ => continue,
        };
        // One unreadable object skips that path rather than blanking the diff.
        let (Ok(o), Ok(n)) = (content(o), content(n)) else {
            continue;
        };
        let path = String::from_utf8_lossy(k);
        out.push_str(&diff_one_file(
            &path,
            o.as_ref().map(|(c, m)| (c.as_slice(), *m)),
            n.as_ref().map(|(c, m)| (c.as_slice(), *m)),
        ));
    }
    Ok(out)
}

/// `(gix-diff dir)` / `(gix-diff-cached dir)` backend. `cached` = false diffs the
/// index against the working tree (`git diff`); `cached` = true diffs HEAD
/// against the index (`git diff --cached`). Untracked files are excluded, as git
/// does. Errors (bare repo, etc.) surface as "" → an empty section in magit.
pub(crate) fn gix_diff_text(dir: &str, cached: bool) -> Result<String, Box<dyn std::error::Error>> {
    use std::collections::BTreeMap;
    let repo = gix::discover(dir)?;
    let index = repo.open_index()?;
    let mut out = String::new();

    if cached {
        // HEAD tree vs index. No HEAD yet (unborn) → empty tree, so every staged
        // path reads as an addition (initial commit). Gitlinks are kept on both
        // sides (rendered as `Subproject commit` lines), so a staged submodule
        // bump shows up and an untouched one compares equal by id.
        let head = match repo.head_tree() {
            Ok(t) => tree_side_map(&t)?,
            Err(_) => BTreeMap::new(),
        };
        let mut head = head;
        let mut idx: BTreeMap<Vec<u8>, GixSide> = BTreeMap::new();
        for e in index.entries() {
            if e.stage() != gix::index::entry::Stage::Unconflicted {
                // An unmerged path has nothing staged (git shows it only as
                // unmerged); without this its absent stage-0 entry would read
                // as a staged deletion of the HEAD version.
                head.remove(e.path(&index));
                continue;
            }
            if e.mode.is_sparse() {
                continue;
            }
            idx.insert(e.path(&index).to_vec(), (e.id, index_mode_str(e.mode)));
        }
        out.push_str(&diff_side_maps(&repo, &head, &idx)?);
    } else {
        // index vs working tree.
        let workdir = repo.workdir().ok_or("bare repository")?.to_path_buf();
        let index_time = index.timestamp();
        for e in index.entries() {
            if e.stage() != gix::index::entry::Stage::Unconflicted
                || e.mode.is_sparse()
                || e.flags.contains(gix::index::entry::Flags::SKIP_WORKTREE)
            {
                continue;
            }
            let path = String::from_utf8_lossy(e.path(&index)).into_owned();
            let mode = index_mode_str(e.mode);
            // A single unreadable entry is skipped, never fatal to the diff.
            match worktree_change(
                &repo,
                &workdir,
                e,
                &path,
                e.stat.is_racy(index_time, Default::default()),
            ) {
                Some(WorktreeChange::Deleted) => {
                    if let Ok(o) = side_content(&repo, e.id, mode) {
                        out.push_str(&diff_one_file(&path, Some((&o, mode)), None));
                    }
                }
                Some(WorktreeChange::Modified { old, new, new_mode }) => {
                    out.push_str(&diff_one_file(
                        &path,
                        Some((&old, mode)),
                        Some((&new, new_mode)),
                    ));
                }
                None => {}
            }
        }
    }
    Ok(out)
}

/// How a tracked path's working-tree copy differs from its index entry.
pub(crate) enum WorktreeChange {
    Deleted,
    /// Old (index) and new (worktree) content — blob bytes, a symlink's target,
    /// or a gitlink's `Subproject commit` line.
    Modified {
        old: Vec<u8>,
        new: Vec<u8>,
        /// The worktree file's git mode — differs from the index entry's on a
        /// `chmod ±x`, which is a change even when the bytes are identical.
        new_mode: &'static str,
    },
}

/// Compare one index entry with its working-tree file; `None` when unchanged
/// or unreadable (one bad file must never abort a whole diff/status). Clean
/// files are skipped from the index's stat cache (size/mtime/ino…) without
/// reading them, unless the entry is racy (modified in the same second the
/// index was written), in which case content decides — git's own strategy.
/// Submodules compare their checked-out HEAD; symlinks their target path.
pub(crate) fn worktree_change(
    repo: &gix::Repository,
    workdir: &std::path::Path,
    e: &gix::index::Entry,
    path: &str,
    racy: bool,
) -> Option<WorktreeChange> {
    let stat_opts = gix::index::entry::stat::Options::default();
    let file = workdir.join(path);
    let meta = match gix::index::fs::Metadata::from_path_no_follow(&file) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Some(WorktreeChange::Deleted);
        }
        Err(_) => return None,
    };

    if e.mode.is_submodule() {
        // Uninitialized (empty dir) or unreadable submodules show no change.
        let head = gix::open(&file).ok()?.head_id().ok()?.detach();
        return (head != e.id).then(|| WorktreeChange::Modified {
            old: gitlink_text(&e.id),
            new: gitlink_text(&head),
            new_mode: GITLINK_MODE,
        });
    }

    let is_link = e.mode.contains(gix::index::entry::Mode::SYMLINK);
    let old_mode = index_mode_str(e.mode);
    // The executable bit, as git tracks it: only for regular files, and only
    // when `core.fileMode` is on (it's off on filesystems without exec bits).
    let new_mode = if is_link
        || !meta.is_file()
        || !repo
            .filesystem_options()
            .map(|c| c.executable_bit)
            .unwrap_or(true)
    {
        old_mode
    } else if meta.is_executable() {
        "100755"
    } else {
        "100644"
    };
    let mode_changed = new_mode != old_mode;

    if !racy
        && !mode_changed
        && let Ok(disk_stat) = gix::index::entry::Stat::from_fs(&meta)
        && e.stat.matches(&disk_stat, stat_opts)
    {
        return None;
    }

    // Symlinks diff their target path (what git stores), not the pointee.
    let new = if is_link {
        link_target_bytes(&std::fs::read_link(&file).ok()?)
    } else {
        std::fs::read(&file).ok()?
    };
    let old = repo.find_object(e.id).ok()?.data.clone();
    (mode_changed || new != old).then_some(WorktreeChange::Modified { old, new, new_mode })
}

/// A symlink target as the raw bytes git stores in the link's blob.
fn link_target_bytes(target: &std::path::Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        target.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        target.to_string_lossy().replace('\\', "/").into_bytes()
    }
}

/// `(gix-show dir rev)` backend: `git show <rev>`-format text — the commit
/// header (`commit`/`Merge`/`Author`/`Date`, message indented 4 spaces) followed
/// by the diff against its first parent (or the empty tree for a root commit).
/// Display-only; magit colorizes it by line prefix.
pub(crate) fn gix_show(dir: &str, rev: &str) -> Result<String, Box<dyn std::error::Error>> {
    use gix::bstr::BStr;
    use std::collections::BTreeMap;
    let repo = gix::discover(dir)?;
    let id = repo.rev_parse_single(BStr::new(rev))?.detach();
    let commit = repo.find_commit(id)?;
    let parents: Vec<gix::ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();

    let mut out = format!("commit {}\n", id.to_hex());
    if parents.len() > 1 {
        let shorts: Vec<String> = parents
            .iter()
            .map(|p| p.to_hex_with_len(7).to_string())
            .collect();
        out.push_str(&format!("Merge: {}\n", shorts.join(" ")));
    }
    let author = commit.author()?;
    out.push_str(&format!("Author: {} <{}>\n", author.name, author.email));
    let t = author.time()?;
    // git's default date: the day unpadded ("Nov 6", not "Nov  6").
    let date = git_date(t, "%a %b %-d %H:%M:%S %Y %z");
    out.push_str(&format!("Date:   {date}\n\n"));
    for line in commit.message_raw()?.to_string().lines() {
        out.push_str("    ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');

    // Diff against the first parent (empty tree for a root commit).
    let new_map = tree_side_map(&commit.tree()?)?;
    let old_map = match parents.first() {
        Some(p) => tree_side_map(&repo.find_commit(*p)?.tree()?)?,
        None => BTreeMap::new(),
    };
    out.push_str(&diff_side_maps(&repo, &old_map, &new_map)?);
    Ok(out)
}

/// `oid → ["HEAD -> main", "tag: v1", "origin/main"]` for `git log --decorate`.
/// Tags get a `tag:` prefix; the checked-out branch gets `HEAD ->`. Built from
/// the ref-name prefix so no Category import is needed.
pub(crate) fn ref_decorations(
    repo: &gix::Repository,
) -> std::collections::HashMap<gix::ObjectId, Vec<String>> {
    let mut map: std::collections::HashMap<gix::ObjectId, Vec<String>> =
        std::collections::HashMap::new();
    let head_branch = repo
        .head_name()
        .ok()
        .flatten()
        .map(|n| n.shorten().to_string());
    if let Ok(platform) = repo.references()
        && let Ok(iter) = platform.all()
    {
        for r in iter.filter_map(Result::ok) {
            let full = r.name().as_bstr().to_string();
            let short = r.name().shorten().to_string();
            let deco = if let Some(t) = full.strip_prefix("refs/tags/") {
                format!("tag: {t}")
            } else if full.starts_with("refs/heads/") {
                if head_branch.as_deref() == Some(short.as_str()) {
                    format!("HEAD -> {short}")
                } else {
                    short
                }
            } else if full.starts_with("refs/remotes/") {
                short
            } else {
                continue; // HEAD, stash, notes, …
            };
            let mut r = r;
            if let Ok(id) = r.peel_to_id() {
                map.entry(id.detach()).or_default().push(deco);
            }
        }
    }
    map
}

/// `(gix-log-decorated dir all? n)`: up to `n` commits newest-first, each
/// `"<short> [(<decorations>)] <summary>"` — `git log --oneline --decorate
/// [--all]`. magit keeps git for the `--graph` variant.
pub(crate) fn gix_log_decorated(
    dir: &str,
    all: bool,
    max: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    use gix::revision::walk::Sorting;
    use gix::traverse::commit::simple::CommitTimeOrder;
    let repo = gix::discover(dir)?;
    let decos = ref_decorations(&repo);
    let tips: Vec<gix::ObjectId> = if all {
        repo.references()?
            .all()?
            .filter_map(Result::ok)
            .filter_map(|mut r| r.peel_to_id().ok().map(|id| id.detach()))
            // A tag can point at a tree or blob; such a tip would make the
            // whole rev-walk fail, so keep commits only.
            .filter(|id| {
                repo.find_header(*id)
                    .is_ok_and(|h| h.kind() == gix::object::Kind::Commit)
            })
            .collect()
    } else {
        vec![repo.head_id()?.detach()]
    };
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    let mut rows = Vec::new();
    for info in repo
        .rev_walk(tips)
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .all()?
        .take(max)
    {
        rows.push(gix_oneline_row(&repo, info?.id, Some(&decos))?);
    }
    Ok(rows)
}

/// `(gix-log-range dir tip hidden n)`: up to `n` commits reachable from `tip`
/// but not `hidden`, newest-first, as `"<short> <summary>"` — `git log
/// --oneline hidden..tip`, used for the unpushed/unpulled sections.
pub(crate) fn gix_log_range(
    dir: &str,
    tip: &str,
    hidden: &str,
    max: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    use gix::bstr::BStr;
    use gix::revision::walk::Sorting;
    use gix::traverse::commit::simple::CommitTimeOrder;
    let repo = gix::discover(dir)?;
    let tip_id = repo.rev_parse_single(BStr::new(tip))?.detach();
    let hidden_id = repo.rev_parse_single(BStr::new(hidden))?.detach();
    let mut rows = Vec::new();
    for info in repo
        .rev_walk(Some(tip_id))
        .with_hidden(Some(hidden_id))
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .all()?
        .take(max)
    {
        rows.push(gix_oneline_row(&repo, info?.id, None)?);
    }
    Ok(rows)
}

/// `(gix-log-file dir file n)`: up to `n` commits that changed `file` (path
/// relative to the repo root), newest-first, decorated — `git log --oneline
/// --decorate -n N -- <file>`. A commit "changed" the file when its blob differs
/// from the first parent's (including add/remove).
pub(crate) fn gix_log_file(
    dir: &str,
    file: &str,
    max: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    use gix::revision::walk::Sorting;
    use gix::traverse::commit::simple::CommitTimeOrder;
    let repo = gix::discover(dir)?;
    let decos = ref_decorations(&repo);
    let path = std::path::Path::new(file);
    let blob_at = |commit: &gix::Commit<'_>| -> Option<gix::ObjectId> {
        commit
            .tree()
            .ok()?
            .lookup_entry_by_path(path)
            .ok()?
            .map(|e| e.id().detach())
    };
    let head = repo.head_id()?.detach();
    let mut rows = Vec::new();
    for info in repo
        .rev_walk(Some(head))
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .all()?
    {
        if rows.len() >= max {
            break;
        }
        let id = info?.id;
        let commit = repo.find_commit(id)?;
        let here = blob_at(&commit);
        let parent = match commit.parent_ids().next() {
            Some(p) => blob_at(&repo.find_commit(p.detach())?),
            None => None,
        };
        if here != parent {
            rows.push(gix_oneline_row(&repo, id, Some(&decos))?);
        }
    }
    Ok(rows)
}

/// `(gix-stash-list dir)`: `git stash list`-format lines, newest first
/// (`"stash@{0}: WIP on main: …"`). Stashes are entries in the `refs/stash`
/// reflog, whose message is exactly what git prints after the `stash@{N}:` tag.
pub(crate) fn gix_stash_list(dir: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let repo = gix::discover(dir)?;
    let reference = match repo.find_reference("refs/stash") {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()), // no stashes
    };
    let mut platform = reference.log_iter();
    let mut msgs: Vec<String> = Vec::new();
    if let Some(iter) = platform.all()? {
        for line in iter {
            msgs.push(line?.message.to_string());
        }
    }
    // The reflog is oldest-first; `git stash list` is newest-first (stash@{0} =
    // most recent).
    Ok(msgs
        .into_iter()
        .rev()
        .enumerate()
        .map(|(i, msg)| format!("stash@{{{i}}}: {msg}"))
        .collect())
}

// --- gix status (porcelain v1 without a subprocess) --------------------------
// Backs `gix-status`, the last hot-path read magit had on `git`. Produces
// `git status --porcelain --branch --untracked-files=all`-format lines so
// magit's `magit-porcelain-full` parser is unchanged. Renames are NOT detected
// (a rename reads as a delete + add, i.e. matches `git status --no-renames`).

/// Count commits reachable from `tip` but not `base` — git's ahead/behind number.
pub(crate) fn gix_count_excl(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    base: gix::ObjectId,
) -> usize {
    repo.rev_walk(Some(tip))
        .with_hidden(Some(base))
        .all()
        .map(|w| w.filter_map(Result::ok).count())
        .unwrap_or(0)
}

/// The short name of the checked-out branch for the repo enclosing `dir`, e.g.
/// "main". Empty when `dir` isn't in a git work tree or HEAD is detached (no
/// branch). The native form of `git rev-parse --abbrev-ref HEAD`.
pub(crate) fn gix_branch(dir: &str) -> String {
    gix::discover(dir)
        .ok()
        .and_then(|repo| repo.head_name().ok().flatten())
        .map(|n| n.shorten().to_string())
        .unwrap_or_default()
}

/// The `## …` porcelain branch header: `## branch...upstream [ahead N, behind M]`,
/// or the `No commits yet` / `HEAD (no branch)` forms magit-parse-branch-line knows.
pub(crate) fn gix_status_branch_line(repo: &gix::Repository) -> String {
    use gix::bstr::BStr;
    let Some(head_name) = repo.head_name().ok().flatten() else {
        return "## HEAD (no branch)".to_string();
    };
    let branch = head_name.shorten().to_string();
    let head_id = match repo.head_id() {
        Ok(id) => id.detach(),
        Err(_) => return format!("## No commits yet on {branch}"),
    };
    let upstream = repo.head_ref().ok().flatten().and_then(|r| {
        r.remote_tracking_ref_name(gix::remote::Direction::Fetch)
            .and_then(|x| x.ok())
            .map(|n| n.shorten().to_string())
    });
    let Some(up) = upstream else {
        return format!("## {branch}");
    };
    let Some(up_id) = repo
        .rev_parse_single(BStr::new(up.as_bytes()))
        .ok()
        .map(|id| id.detach())
    else {
        return format!("## {branch}...{up} [gone]");
    };
    let (ahead, behind) = (
        gix_count_excl(repo, head_id, up_id),
        gix_count_excl(repo, up_id, head_id),
    );
    if ahead == 0 && behind == 0 {
        return format!("## {branch}...{up}");
    }
    let mut ab = Vec::new();
    if ahead > 0 {
        ab.push(format!("ahead {ahead}"));
    }
    if behind > 0 {
        ab.push(format!("behind {behind}"));
    }
    format!("## {branch}...{up} [{}]", ab.join(", "))
}

/// One porcelain status row: a `path` with its staged (`x`) and unstaged (`y`)
/// codes. `x`/`y` are `' '` (unchanged), `M`/`A`/`D`, or `?` (untracked).
pub(crate) struct StatusEntry {
    path: String,
    x: char,
    y: char,
}

/// `XY`-classified change rows for `gix-status`: tracked changes sorted by
/// path, then untracked (`??`) ones, as git orders them. X = index-vs-HEAD
/// (staged), Y = worktree-vs-index (unstaged); unmerged paths carry git's
/// two-letter conflict code (`UU`, `AA`, `DU`, …).
pub(crate) fn gix_status_entries(
    repo: &gix::Repository,
) -> Result<Vec<StatusEntry>, Box<dyn std::error::Error>> {
    use std::collections::{BTreeMap, HashSet};
    let index = repo.open_index()?;
    let workdir = repo.workdir().ok_or("bare repository")?.to_path_buf();

    // index (stage 0) path→(oid, mode), plus every index path (any stage) for
    // the untracked test, plus the stages present for each unmerged path.
    let mut idx: BTreeMap<Vec<u8>, GixSide> = BTreeMap::new();
    let mut idx_paths: HashSet<Vec<u8>> = HashSet::new();
    let mut conflicts: BTreeMap<Vec<u8>, u8> = BTreeMap::new();
    // Submodule checkouts: their files belong to another repo, never untracked.
    let mut gitlink_dirs: HashSet<std::path::PathBuf> = HashSet::new();
    for e in index.entries() {
        idx_paths.insert(e.path(&index).to_vec());
        if e.mode.is_submodule() {
            gitlink_dirs.insert(workdir.join(String::from_utf8_lossy(e.path(&index)).as_ref()));
        }
        match e.stage() {
            gix::index::entry::Stage::Unconflicted => {
                idx.insert(e.path(&index).to_vec(), (e.id, index_mode_str(e.mode)));
            }
            stage => *conflicts.entry(e.path(&index).to_vec()).or_default() |= 1 << stage as u8,
        }
    }
    let head = match repo.head_tree() {
        Ok(t) => tree_side_map(&t)?,
        Err(_) => BTreeMap::new(),
    };

    let mut codes: BTreeMap<Vec<u8>, [char; 2]> = BTreeMap::new();

    // Unmerged paths get git's two-letter conflict code instead of X/Y.
    for (k, stages) in &conflicts {
        codes.insert(k.clone(), unmerged_code(*stages));
    }

    // X — staged (HEAD tree vs index). Mode counts too (`chmod +x` + add).
    let mut staged: Vec<&Vec<u8>> = head.keys().chain(idx.keys()).collect();
    staged.sort();
    staged.dedup();
    for k in staged {
        if conflicts.contains_key(k) {
            continue;
        }
        let x = match (head.get(k), idx.get(k)) {
            (None, Some(_)) => 'A',
            (Some(_), None) => 'D',
            (Some(h), Some(i)) if h != i => 'M',
            _ => continue,
        };
        codes.entry(k.clone()).or_insert([' ', ' '])[0] = x;
    }

    // Y — unstaged (index vs working tree). Skip reading content when the
    // index's stat cache says the file is clean, the way git does.
    let index_time = index.timestamp();
    for e in index.entries() {
        if e.stage() != gix::index::entry::Stage::Unconflicted
            || e.mode.is_sparse()
            || e.flags.contains(gix::index::entry::Flags::SKIP_WORKTREE)
        {
            continue;
        }
        let rel = e.path(&index).to_vec();
        let path = String::from_utf8_lossy(&rel).into_owned();
        let y = match worktree_change(
            repo,
            &workdir,
            e,
            &path,
            e.stat.is_racy(index_time, Default::default()),
        ) {
            Some(WorktreeChange::Deleted) => 'D',
            Some(WorktreeChange::Modified { .. }) => 'M',
            None => continue,
        };
        codes.entry(rel).or_insert([' ', ' '])[1] = y;
    }

    // Untracked — gitignore-aware worktree walk; files absent from the index
    // (at any stage: `git rm --cached foo` leaves foo in HEAD but untracked).
    // Only git's ignore rules apply — not ripgrep's `.ignore` files. A nested
    // repository that isn't a submodule is one untracked `dir/` entry, as git
    // reports it, rather than every file inside it.
    let nested: std::sync::Arc<std::sync::Mutex<Vec<std::path::PathBuf>>> = Default::default();
    let nested_w = nested.clone();
    let walker = ignore::WalkBuilder::new(&workdir)
        .hidden(false) // dotfiles are tracked by git
        .ignore(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(move |d| {
            if d.depth() == 0 {
                return true;
            }
            if d.file_name() == ".git" || gitlink_dirs.contains(d.path()) {
                return false;
            }
            if d.file_type().is_some_and(|t| t.is_dir()) && d.path().join(".git").exists() {
                nested_w.lock().unwrap().push(d.path().to_path_buf());
                return false;
            }
            true
        })
        .build();
    // Kept apart from `codes`: a path can be both (`git rm --cached foo` is a
    // staged `D ` *and* an untracked `??`), and git lists untracked last.
    let mut untracked: std::collections::BTreeSet<Vec<u8>> = Default::default();
    let rel_of = |path: &std::path::Path| -> Option<Vec<u8>> {
        let rel = path.strip_prefix(&workdir).ok()?;
        Some(rel.to_string_lossy().replace('\\', "/").into_bytes())
    };
    for entry in walker.filter_map(Result::ok) {
        if !entry
            .file_type()
            .is_some_and(|t| t.is_file() || t.is_symlink())
        {
            continue;
        }
        let Some(rel) = rel_of(entry.path()) else {
            continue;
        };
        if !idx_paths.contains(&rel) {
            untracked.insert(rel);
        }
    }
    for dir in nested.lock().unwrap().iter() {
        if let Some(mut rel) = rel_of(dir) {
            rel.push(b'/');
            untracked.insert(rel);
        }
    }

    Ok(codes
        .into_iter()
        .chain(untracked.into_iter().map(|p| (p, ['?', '?'])))
        .map(|(p, [x, y])| StatusEntry {
            path: String::from_utf8_lossy(&p).into_owned(),
            x,
            y,
        })
        .collect())
}

/// git's porcelain code for an unmerged path, from the bitmask of index stages
/// present (bit 1 = base, 2 = ours, 3 = theirs).
fn unmerged_code(stages: u8) -> [char; 2] {
    const BASE: u8 = 1 << 1;
    const OURS: u8 = 1 << 2;
    const THEIRS: u8 = 1 << 3;
    match stages & (BASE | OURS | THEIRS) {
        BASE => ['D', 'D'],
        OURS => ['A', 'U'],
        x if x == BASE | OURS => ['U', 'D'],
        THEIRS => ['U', 'A'],
        x if x == BASE | THEIRS => ['D', 'U'],
        x if x == OURS | THEIRS => ['A', 'A'],
        _ => ['U', 'U'],
    }
}

/// `(gix-status dir)` backend: porcelain v1 lines, header first.
pub(crate) fn gix_status(dir: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let repo = gix::discover(dir)?;
    let mut out = vec![gix_status_branch_line(&repo)];
    for e in gix_status_entries(&repo)? {
        out.push(format!("{}{} {}", e.x, e.y, e.path));
    }
    Ok(out)
}

/// Working-tree-vs-HEAD gutter marks (git-gutter / diff-hl) for `file`, diffing
/// the supplied `text` — the live buffer rope, so marks update before a save —
/// against the file's blob as committed at HEAD. Returns `(0-based line, mark)`
/// for changed lines only; an empty vec when `file` is outside a repo or its
/// parent can't be resolved.
///
/// A file not tracked at HEAD (brand new, or never committed) diffs against an
/// empty HEAD, so every line reads as `Added` — matching how diff-hl shows a new
/// file. Binary / non-UTF-8 HEAD blobs likewise fall back to "all added", which
/// is harmless for the text buffers this runs on.
pub(crate) fn git_diff_marks(
    file: &std::path::Path,
    text: &str,
) -> Vec<(usize, crate::editor::DiffMark)> {
    let Some(dir) = file.parent() else {
        return Vec::new();
    };
    let Ok(repo) = gix::discover(dir) else {
        return Vec::new();
    };
    // Resolve `file` to a repo-root-relative path the way `gix_blame` does.
    let abs = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let Some(wd) = repo.workdir().and_then(|wd| std::fs::canonicalize(wd).ok()) else {
        return Vec::new();
    };
    let Ok(rel) = abs.strip_prefix(&wd) else {
        return Vec::new();
    };
    // HEAD's blob for this path, or "" when it isn't tracked there (→ all added).
    let head_text = (|| -> Option<String> {
        let entry = repo.head_tree().ok()?.lookup_entry_by_path(rel).ok()??;
        String::from_utf8(entry.object().ok()?.data.clone()).ok()
    })()
    .unwrap_or_default();

    diff_line_marks(&head_text, text)
}

/// The pure core of [`git_diff_marks`]: a line-level diff of `head` → `text`
/// mapped to gutter marks. Insert → `Added`, Replace → `Modified`, Delete →
/// a single `Removed` on the line that now sits where the text was removed
/// (clamped to the last line for an end-of-file deletion). Split out so the
/// mapping is testable without a git repo.
pub(crate) fn diff_line_marks(head: &str, text: &str) -> Vec<(usize, crate::editor::DiffMark)> {
    use crate::editor::DiffMark;
    let new_total = text.lines().count();
    let diff = similar::TextDiff::from_lines(head, text);
    let mut marks = Vec::new();
    for op in diff.ops() {
        let (tag, _old, new) = op.as_tag_tuple();
        match tag {
            similar::DiffTag::Equal => {}
            similar::DiffTag::Insert => {
                marks.extend((new.start..new.end).map(|l| (l, DiffMark::Added)));
            }
            similar::DiffTag::Replace => {
                marks.extend((new.start..new.end).map(|l| (l, DiffMark::Modified)));
            }
            similar::DiffTag::Delete => {
                let line = new.start.min(new_total.saturating_sub(1));
                marks.push((line, DiffMark::Removed));
            }
        }
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn git_dates_use_the_authors_offset() {
        // 2026-11-26 03:04:05 UTC, authored at -08:00: still the 25th there.
        let t = gix::date::Time::new(1_795_662_245, -8 * 3600);
        assert_eq!(git_date(t, "%Y-%m-%d"), "2026-11-25");
        assert_eq!(
            git_date(t, "%a %b %-d %H:%M:%S %Y %z"),
            "Wed Nov 25 19:04:05 2026 -0800"
        );
        let early = gix::date::Time::new(1_793_928_616, 0);
        assert_eq!(
            git_date(early, "%a %b %-d %H:%M:%S %Y %z"),
            "Fri Nov 6 01:30:16 2026 +0000"
        );
        // An offset no real zone has yields "" rather than a wrong date.
        assert_eq!(git_date(gix::date::Time::new(0, 100 * 3600), "%Y"), "");
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "protocol.file.allow=always"])
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// Submodules (gitlinks) and symlinks must diff like git, and one
    /// unreadable file must not blank the whole diff.
    #[cfg(unix)]
    #[test]
    fn diff_handles_submodules_symlinks_and_unreadable_files() {
        let root = std::env::temp_dir().join(format!("macros-gix-sub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (sub_src, top) = (root.join("sub-src"), root.join("top"));
        std::fs::create_dir_all(&sub_src).unwrap();
        std::fs::create_dir_all(&top).unwrap();

        git(&sub_src, &["init", "-q"]);
        std::fs::write(sub_src.join("s.txt"), "s\n").unwrap();
        git(&sub_src, &["add", "."]);
        git(&sub_src, &["commit", "-qm", "sub init"]);

        git(&top, &["init", "-q"]);
        std::fs::write(top.join("a.txt"), "a\n").unwrap();
        std::fs::write(top.join("locked.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink("a.txt", top.join("link")).unwrap();
        git(&top, &["add", "."]);
        git(
            &top,
            &["submodule", "add", "-q", sub_src.to_str().unwrap(), "sub"],
        );
        git(&top, &["commit", "-qm", "init"]);
        let d = top.to_str().unwrap();

        // Clean tree: nothing unstaged, nothing staged, no submodule files
        // reported as untracked.
        assert_eq!(gix_diff_text(d, false).unwrap(), "");
        assert_eq!(gix_diff_text(d, true).unwrap(), "");
        let repo = gix::discover(d).unwrap();
        assert!(gix_status_entries(&repo).unwrap().is_empty());

        // Move the submodule's HEAD, retarget the symlink, edit a file, and
        // make another modified file unreadable.
        std::fs::write(top.join("sub/s.txt"), "s2\n").unwrap();
        git(&top.join("sub"), &["commit", "-qam", "sub bump"]);
        std::fs::remove_file(top.join("link")).unwrap();
        std::os::unix::fs::symlink("elsewhere", top.join("link")).unwrap();
        std::fs::write(top.join("a.txt"), "a\nb\n").unwrap();
        let locked = top.join("locked.txt");
        std::fs::write(&locked, "changed!\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let diff = gix_diff_text(d, false).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(diff.contains("diff --git a/a.txt b/a.txt\n"), "{diff}");
        assert!(diff.contains("+b\n"), "{diff}");
        assert!(diff.contains("diff --git a/link b/link\n"), "{diff}");
        assert!(
            diff.contains("-a.txt\n") && diff.contains("+elsewhere\n"),
            "{diff}"
        );
        assert!(diff.contains("diff --git a/sub b/sub\n"), "{diff}");
        let sub_head = gix::open(top.join("sub"))
            .unwrap()
            .head_id()
            .unwrap()
            .detach();
        assert!(
            diff.contains(&format!("+Subproject commit {}\n", sub_head.to_hex())),
            "{diff}"
        );

        let status = gix_status_entries(&repo).unwrap();
        let y_of = |p: &str| status.iter().find(|e| e.path == p).map(|e| e.y);
        assert_eq!(y_of("sub"), Some('M'));
        assert_eq!(y_of("link"), Some('M'));
        assert!(status.iter().all(|e| !e.path.starts_with("sub/")));

        // Staged submodule bump renders as a Subproject hunk, never a blob read.
        git(&top, &["add", "sub"]);
        let cached = gix_diff_text(d, true).unwrap();
        assert!(cached.contains("diff --git a/sub b/sub\n"), "{cached}");
        assert!(
            cached.contains(&format!("+Subproject commit {}\n", sub_head.to_hex())),
            "{cached}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn status_map(d: &str) -> std::collections::BTreeMap<String, String> {
        status_rows(d)
            .into_iter()
            .map(|r| (r[3..].to_string(), r[..2].to_string()))
            .collect()
    }

    fn status_rows(d: &str) -> Vec<String> {
        let repo = gix::discover(d).unwrap();
        gix_status_entries(&repo)
            .unwrap()
            .into_iter()
            .map(|e| format!("{}{} {}", e.x, e.y, e.path))
            .collect()
    }

    fn fresh_repo(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("macros-gix-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        root
    }

    /// Unmerged paths are `UU`/`AA` (not a staged `D`), and the staged diff
    /// shows nothing for them.
    #[test]
    fn status_reports_conflicts_like_git() {
        let root = fresh_repo("conflict");
        let d = root.to_str().unwrap();
        std::fs::write(root.join("both.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "base"]);
        git(&root, &["checkout", "-qb", "other"]);
        std::fs::write(root.join("both.txt"), "theirs\n").unwrap();
        std::fs::write(root.join("new.txt"), "theirs\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "theirs"]);
        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("both.txt"), "ours\n").unwrap();
        std::fs::write(root.join("new.txt"), "ours\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "ours"]);
        // The merge conflicts (non-zero exit), so don't assert success.
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "merge",
                "-q",
                "other",
            ])
            .output();

        let st = status_map(d);
        assert_eq!(st.get("both.txt").map(String::as_str), Some("UU"), "{st:?}");
        assert_eq!(st.get("new.txt").map(String::as_str), Some("AA"), "{st:?}");
        let cached = gix_diff_text(d, true).unwrap();
        assert!(!cached.contains("both.txt"), "{cached}");
        assert!(!cached.contains("new.txt"), "{cached}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `git rm --cached` → untracked; `.ignore` files don't hide anything;
    /// a nested repository collapses to one `dir/` entry.
    #[test]
    fn status_untracked_matches_git() {
        let root = fresh_repo("untracked");
        let d = root.to_str().unwrap();
        std::fs::write(root.join("foo"), "x\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        git(&root, &["rm", "-q", "--cached", "foo"]);
        std::fs::write(root.join(".ignore"), "hidden.txt\n").unwrap();
        std::fs::write(root.join("hidden.txt"), "x\n").unwrap();
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "-q"]);
        std::fs::write(nested.join("inner.txt"), "x\n").unwrap();

        // git prints the staged deletion and the untracked copy as two rows,
        // untracked last.
        let rows = status_rows(d);
        let del = rows.iter().position(|r| r == "D  foo");
        let unt = rows.iter().position(|r| r == "?? foo");
        assert!(del.is_some() && unt.is_some() && del < unt, "{rows:?}");
        let st = status_map(d);
        assert_eq!(
            st.get("hidden.txt").map(String::as_str),
            Some("??"),
            "{st:?}"
        );
        assert_eq!(st.get("nested/").map(String::as_str), Some("??"), "{st:?}");
        assert!(!st.contains_key("nested/inner.txt"), "{st:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `chmod +x` with identical content is a change, unstaged and staged.
    #[cfg(unix)]
    #[test]
    fn mode_only_changes_show_in_status_and_diff() {
        use std::os::unix::fs::PermissionsExt;
        let root = fresh_repo("mode");
        let d = root.to_str().unwrap();
        let f = root.join("run.sh");
        std::fs::write(&f, "echo hi\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(status_map(d).get("run.sh").map(String::as_str), Some(" M"));
        let diff = gix_diff_text(d, false).unwrap();
        assert_eq!(
            diff,
            "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n"
        );

        git(&root, &["add", "run.sh"]);
        assert_eq!(status_map(d).get("run.sh").map(String::as_str), Some("M "));
        assert_eq!(gix_diff_text(d, false).unwrap(), "");
        let cached = gix_diff_text(d, true).unwrap();
        assert_eq!(
            cached,
            "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A tag pointing at a tree must not blank `--all` log output.
    #[test]
    fn log_decorated_all_skips_non_commit_tips() {
        let root = fresh_repo("tagtree");
        let d = root.to_str().unwrap();
        std::fs::write(root.join("a"), "a\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "first"]);
        git(&root, &["tag", "treetag", "HEAD^{tree}"]);
        let rows = gix_log_decorated(d, true, 10).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].ends_with("first"), "{rows:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn index_mode_str_distinguishes_gitlink_from_symlink() {
        use gix::index::entry::Mode;
        assert_eq!(index_mode_str(Mode::COMMIT), "160000");
        assert_eq!(index_mode_str(Mode::SYMLINK), "120000");
        assert_eq!(index_mode_str(Mode::FILE_EXECUTABLE), "100755");
        assert_eq!(index_mode_str(Mode::FILE), "100644");
    }

    /// Run git with a hermetic config (no user/system settings like
    /// `blame.showRoot` or `core.abbrev` leaking in) and return stdout.
    fn git_out(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .envs(env.iter().copied())
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// `gix-blame-text` matches `git blame --date=short` byte-for-byte: root
    /// boundary marker, author padding, the author's own timezone for the date,
    /// right-aligned line numbers, `.mailmap`, and a final line with no newline.
    #[test]
    fn blame_text_matches_git() {
        let root =
            std::env::temp_dir().join(format!("macros-gix-blametext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git_out(&root, &[], &["init", "-q", "-b", "main"]);
        let commit = |name: &str, date: &str, msg: &str| {
            let env = [
                ("GIT_AUTHOR_NAME", name),
                ("GIT_AUTHOR_EMAIL", "a@a"),
                ("GIT_AUTHOR_DATE", date),
                ("GIT_COMMITTER_NAME", name),
                ("GIT_COMMITTER_EMAIL", "a@a"),
                ("GIT_COMMITTER_DATE", date),
            ];
            git_out(&root, &env, &["add", "-A"]);
            git_out(&root, &env, &["commit", "-qm", msg]);
        };
        let body: String = (1..=11).map(|i| format!("line {i}\n")).collect();
        std::fs::write(root.join("f.txt"), &body).unwrap();
        // 23:30 at -05:00 is already the next day in UTC: the date must be the
        // author's local one.
        commit("Al", "2024-03-01T23:30:00-05:00", "root");
        let edited = body.replace("line 5\n", "five\n") + "tail";
        std::fs::write(root.join("f.txt"), &edited).unwrap();
        commit("Bartholomew", "2024-06-15T08:00:00+02:00", "edit");

        // A committed .mailmap renames "Al" — git blame shows the mapped name.
        std::fs::write(root.join(".mailmap"), "Alfred Mapped <a@a> Al <a@a>\n").unwrap();
        commit("Bartholomew", "2024-06-16T08:00:00+02:00", "mailmap");

        let d = root.to_str().unwrap();
        let want = git_out(&root, &[], &["blame", "--date=short", "--", "f.txt"]);
        assert!(want.contains("Alfred Mapped"), "{want}");
        assert_eq!(gix_blame_text(d, "f.txt").unwrap(), want);
        assert!(gix_blame(d, "f.txt").unwrap()[0].contains("\tAlfred Mapped\t"));

        // Modified file → "" so magit falls back to git's working-tree blame.
        std::fs::write(root.join("f.txt"), "dirty\n").unwrap();
        assert_eq!(gix_blame_text(d, "f.txt").unwrap(), "");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `gix-git-dir` matches `git rev-parse --absolute-git-dir`, including from a
    /// subdirectory and in a linked worktree (where `.git` is a file).
    #[test]
    fn git_dir_matches_rev_parse() {
        let root = fresh_repo("gitdir");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("f.txt"), "x\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        let wt = root.with_file_name(format!("macros-gix-gitdir-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wt);
        git(
            &root,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wt"],
        );

        let canon = |p: &str| std::fs::canonicalize(p.trim_end()).unwrap();
        for dir in [root.clone(), root.join("sub"), wt.clone()] {
            let want = git_out(&dir, &[], &["rev-parse", "--absolute-git-dir"]);
            let got = gix_git_dir(dir.to_str().unwrap());
            assert!(std::path::Path::new(&got).is_absolute(), "{got}");
            assert_eq!(canon(&got), canon(&want), "{dir:?}");
        }
        assert!(gix_git_dir(wt.to_str().unwrap()).contains("worktrees"));
        let outside = std::env::temp_dir().join(format!("macros-gix-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        // temp_dir itself could sit inside a repo on odd setups; only assert
        // when git agrees there is none.
        let in_repo = std::process::Command::new("git")
            .arg("-C")
            .arg(&outside)
            .args(["rev-parse", "--git-dir"])
            .output()
            .unwrap()
            .status
            .success();
        if !in_repo {
            assert_eq!(gix_git_dir(outside.to_str().unwrap()), "");
        }
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }
}
