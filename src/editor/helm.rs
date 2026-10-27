//! Helm (generic selection framework) methods for [`Editor`].

use super::*;

/// Convert parsed [`HelmSourceSpec`]s into runtime [`HelmSource`]s.
pub(super) fn helm_sources_from_specs(
    specs: Vec<crate::script::HelmSourceSpec>,
) -> Vec<HelmSource> {
    specs
        .into_iter()
        .map(|s| {
            let (candidates, annotations, dynamic, raw) = match s.candidates {
                crate::script::HelmCandidates::Static(pairs) => {
                    let mut annotations = HashMap::new();
                    let candidates = pairs
                        .into_iter()
                        .map(|(c, ann)| {
                            if let Some(ann) = ann {
                                annotations.insert(c.clone(), ann);
                            }
                            c
                        })
                        .collect();
                    (candidates, annotations, None, false)
                }
                crate::script::HelmCandidates::Dynamic(f) => {
                    (Vec::new(), HashMap::new(), Some(f), false)
                }
                crate::script::HelmCandidates::DynamicRaw(f) => {
                    (Vec::new(), HashMap::new(), Some(f), true)
                }
            };
            HelmSource {
                name: s.name,
                candidates,
                annotations,
                dynamic,
                raw,
                action: HelmAction::Scheme(s.actions),
                preview: s.preview,
            }
        })
        .collect()
}

/// Emacs-style file prompt shortcuts for Helm file sources (`file-name-shadow`).
/// When a file picker is pre-filled with the current directory, typing `/`
/// after that trailing `/` produces `//`, which shadows everything before it
/// and jumps to the filesystem root; typing `~/` there jumps to the home
/// directory, spelled out so candidates (absolute paths) still match. The
/// later of the two shortcuts wins.
pub(super) fn helm_file_shadow_pattern(sources: &[HelmSource], pattern: &str) -> Option<String> {
    let is_file_source = sources
        .iter()
        .any(|s| s.dynamic.as_deref() == Some("find-file-candidates"));
    if !is_file_source {
        return None;
    }
    let root = pattern.rfind("//").map(|at| (at, at + 2, false));
    let home = match pattern.rfind("/~/") {
        Some(at) => Some((at, at + 3, true)),
        None => pattern.starts_with("~/").then_some((0, 2, true)),
    };
    let (_, tail_start, is_home) = [root, home]
        .into_iter()
        .flatten()
        .max_by_key(|(at, ..)| *at)?;
    let tail = &pattern[tail_start..];
    Some(if is_home {
        crate::util::expand_tilde(&format!("~/{tail}"))
    } else {
        format!("/{tail}")
    })
}

/// For Helm file pickers, `RET` on text that is not an exact listed candidate
/// should visit that typed path. This is how `find-file` creates a file-visiting
/// empty buffer for a new file; directories still use candidate completion.
pub(super) fn helm_file_accept_target(
    sources: &[HelmSource],
    pattern: &str,
) -> Option<(HelmAction, String)> {
    let pattern = pattern.trim();
    if pattern.is_empty() || pattern.ends_with('/') {
        return None;
    }
    let source = sources
        .iter()
        .find(|s| s.dynamic.as_deref() == Some("find-file-candidates"))?;
    if source.candidates.iter().any(|c| c == pattern) {
        return None;
    }
    if Path::new(&expand_path(pattern)).is_dir() {
        return None;
    }
    Some((source.action.clone(), pattern.to_string()))
}

/// Cycle a selection index by one step with wraparound over `count` items.
/// `Forward` advances, `Backward` steps back. Returns 0 if `count` is 0.
pub(super) fn wrap_index(selected: usize, count: usize, dir: Direction) -> usize {
    if count == 0 {
        return 0;
    }
    let step = match dir {
        Direction::Forward => 1,
        Direction::Backward => count - 1,
    };
    (selected + step) % count
}

/// The `[start, end)` slice of `matches` (`(source, candidate)` index pairs)
/// to render so that `selected` is visible and the rows (candidates plus a
/// header at each source change) fit in `max_lines`. Scrolls back from the
/// selection as far as fits, then fills forward, so the list uses the whole
/// panel.
pub(super) fn helm_window(
    matches: &[(usize, usize)],
    selected: usize,
    max_lines: usize,
) -> (usize, usize) {
    let total = matches.len();
    if total == 0 {
        return (0, 0);
    }
    // At least the selection and its header, even in a tiny panel.
    let max_lines = max_lines.max(2);
    let selected = selected.min(total - 1);
    // The selected candidate plus its source header.
    let mut lines = 2;
    let mut start = selected;
    while start > 0 {
        let extra = 1 + usize::from(matches[start - 1].0 != matches[start].0);
        if lines + extra > max_lines {
            break;
        }
        lines += extra;
        start -= 1;
    }
    let mut end = selected + 1;
    while end < total {
        let extra = 1 + usize::from(matches[end].0 != matches[end - 1].0);
        if lines + extra > max_lines {
            break;
        }
        lines += extra;
        end += 1;
    }
    (start, end)
}

/// What running a Helm candidate does.
#[derive(Clone)]
pub(super) enum HelmAction {
    /// `(label, function-name)` pairs — the first is the default (RET); the rest
    /// are reachable via the M-a action menu. The function is called with the
    /// chosen candidate string.
    Scheme(Vec<(String, String)>),
    /// Treat the candidate itself as a command name and run it (helm-M-x).
    ExecuteCommand,
    /// An action-menu session (M-a): the candidate is an action label, looked
    /// up among `actions` (`(label, fn)` pairs) and run on each of `targets` —
    /// the candidates marked in the menu's source, else the one it was opened on.
    ActionMenu {
        targets: Vec<String>,
        actions: Vec<(String, String)>,
    },
    /// Jump to one of a set of LSP locations. The vec is parallel to the
    /// source's `candidates`; the accepted candidate's index selects the target.
    GotoLocation(Vec<lsp::Location>),
    /// Apply one of a set of LSP code actions. The vec is parallel to the
    /// source's `candidates`; the accepted candidate's index selects the action.
    ApplyCodeAction(Vec<lsp::CodeAction>),
}

/// One group of candidates in a Helm session.
pub(super) struct HelmSource {
    pub(super) name: String,
    /// Current candidates — for a dynamic source, the latest query result.
    pub(super) candidates: Vec<String>,
    /// Optional marginalia-style annotation per candidate (candidate → text),
    /// shown dimmed to the right of the row. Candidates absent from the map have
    /// no annotation.
    pub(super) annotations: HashMap<String, String>,
    /// Name of a Scheme fn that (re)computes candidates from the pattern; `None`
    /// for static sources.
    pub(super) dynamic: Option<String>,
    /// When set, the source's candidates are shown verbatim — Rust does no
    /// pattern filtering (the dynamic fn already did, e.g. via `fuzzy-match`).
    pub(super) raw: bool,
    pub(super) action: HelmAction,
    /// Name of a Scheme fn that returns the preview (as lines) for a candidate;
    /// `None` = this source has no preview pane.
    pub(super) preview: Option<String>,
}

/// An active Helm selection session (a generic completing-read UI): a prompt, a
/// live filter `pattern`, the sources, the selected match index, and the
/// `query_id` that tags in-flight dynamic-candidate requests.
pub(super) struct HelmSession {
    prompt: String,
    /// Change through [`Self::set_pattern`], which refilters `matches`.
    pattern: String,
    sources: Vec<HelmSource>,
    /// The `(source_index, candidate_index)` pairs matching `pattern`, in
    /// display order — recomputed only when the pattern or the candidates
    /// change ([`Self::refilter`]), not on every read.
    matches: Vec<(usize, usize)>,
    selected: usize,
    query_id: u64,
    /// Fuzzy (nucleo) vs substring matching, captured from settings at open.
    fuzzy: bool,
    /// Candidates marked for multi-select (`C-SPC`), as `(source_index,
    /// candidate)` in mark order. When non-empty, accepting runs each one's own
    /// source action instead of acting on the selected candidate.
    marked: Vec<(usize, String)>,
    /// Preview text for the current selection (from the source's preview fn),
    /// shown in a side pane. Empty when the selected source has no preview.
    preview: String,
    /// Query id of the in-flight preview request, so a stale result is dropped.
    preview_query: u64,
}

/// One rendered line of the Helm panel: a source header or a candidate.
pub struct HelmRow {
    pub text: String,
    /// Marginalia-style annotation shown dimmed to the right (candidates only).
    pub annotation: Option<String>,
    pub header: bool,
    pub selected: bool,
    pub marked: bool,
    /// Index into the flattened match list, for click-to-select (candidate rows
    /// only; headers carry `None`).
    pub index: Option<usize>,
}

/// A render snapshot of the Helm panel.
pub struct HelmView {
    pub prompt: String,
    pub pattern: String,
    pub rows: Vec<HelmRow>,
    /// Preview text for the current selection, shown in a side pane (`None` when
    /// the selected source has no preview fn).
    pub preview: Option<String>,
}

/// A candidate tagged with its index in its source, so fuzzy ranking (which
/// reorders) still reports where each match came from.
struct Indexed<'a>(usize, &'a str);

impl AsRef<str> for Indexed<'_> {
    fn as_ref(&self) -> &str {
        self.1
    }
}

impl HelmSession {
    /// The prompt followed by what's been typed (for the MCP `press_keys` report).
    pub(super) fn prompt_and_pattern(&self) -> String {
        format!("{}{}", self.prompt, self.pattern)
    }

    pub(super) fn new(
        prompt: String,
        pattern: String,
        sources: Vec<HelmSource>,
        query_id: u64,
        fuzzy: bool,
    ) -> Self {
        let mut session = HelmSession {
            prompt,
            pattern,
            sources,
            matches: Vec::new(),
            selected: 0,
            query_id,
            fuzzy,
            marked: Vec::new(),
            preview: String::new(),
            preview_query: 0,
        };
        session.refilter();
        session
    }

    /// Recompute [`Self::matches`]: nucleo fuzzy ranking (best-first within
    /// each source) when `fuzzy`, else case-insensitive substring; an empty
    /// pattern keeps everything in order.
    fn refilter(&mut self) {
        let pattern = self.pattern.as_str();
        let empty = pattern.is_empty();
        // Fuzzy machinery, built once and only when a non-raw source will need it.
        let mut fuzzy_ctx = (!empty && self.fuzzy).then(|| {
            (
                Matcher::new(Config::DEFAULT),
                Pattern::parse(pattern, CaseMatching::Smart, Normalization::Smart),
            )
        });
        let needle = (!empty && !self.fuzzy).then(|| pattern.to_lowercase());
        let mut out = Vec::new();
        for (i, source) in self.sources.iter().enumerate() {
            // An empty pattern keeps everything; a `raw` source self-filtered in
            // Scheme, so it too is shown verbatim, in the order the fn returned.
            if empty || source.raw {
                out.extend((0..source.candidates.len()).map(|j| (i, j)));
            } else if let Some((matcher, parsed)) = fuzzy_ctx.as_mut() {
                // The sort is stable, so equal strings keep their source order.
                let items = source
                    .candidates
                    .iter()
                    .enumerate()
                    .map(|(j, c)| Indexed(j, c));
                for (Indexed(j, _), _score) in parsed.match_list(items, matcher) {
                    out.push((i, j));
                }
            } else if let Some(needle) = &needle {
                for (j, candidate) in source.candidates.iter().enumerate() {
                    if candidate.to_lowercase().contains(needle) {
                        out.push((i, j));
                    }
                }
            }
        }
        self.matches = out;
    }

    /// Replace the filter pattern and refilter.
    fn set_pattern(&mut self, pattern: String) {
        self.pattern = pattern;
        self.refilter();
    }

    /// How many candidates match the pattern.
    pub(super) fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// Match `i` as `(source_index, candidate)`.
    fn match_at(&self, i: usize) -> Option<(usize, &str)> {
        let &(src, j) = self.matches.get(i)?;
        Some((src, self.sources[src].candidates[j].as_str()))
    }

    /// The match under the cursor, as `(source_index, candidate)`.
    fn selected_match(&self) -> Option<(usize, &str)> {
        self.match_at(self.selected)
    }

    /// The index within its source's (unfiltered) `candidates` of match `i` —
    /// native actions use it to pick from their parallel target vec, since
    /// filtering reorders the visible matches. Identical candidate strings (two
    /// LSP locations rendering the same `file:line:col: text` row) each keep
    /// their own index.
    fn candidate_index(&self, i: usize) -> Option<usize> {
        self.matches.get(i).map(|&(_, j)| j)
    }
}

/// A Helm preview computed in Rust (rather than dispatched to a Scheme fn), for
/// the LSP location / code-action pickers.
enum NativePreview {
    Location(crate::lsp::Location),
    CodeAction(crate::lsp::CodeAction),
}

impl Editor {
    pub(super) fn helm_open(
        &mut self,
        prompt: String,
        sources: Vec<HelmSource>,
        initial: String,
        cx: &mut Context<Self>,
    ) {
        self.completion_popup = None;
        self.helm_query_counter += 1;
        let query_id = self.helm_query_counter;
        let fuzzy = cx.global::<crate::settings::Settings>().helm_fuzzy();
        self.helm = Some(HelmSession::new(prompt, initial, sources, query_id, fuzzy));
        self.helm_query_dynamic();
        self.helm_update_preview();
        cx.notify();
    }

    /// Re-request the preview for the current selection, if its source defines a
    /// preview fn. The fn receives the candidate and returns the preview as a
    /// list of lines. Clears the pane when the source has no preview.
    pub(super) fn helm_update_preview(&mut self) {
        // Native (Rust-computed) preview for the LSP location / code-action
        // pickers. Clone the target out so the `self.helm` borrow is released
        // before reading files.
        let Some(helm) = self.helm.as_ref() else {
            return;
        };
        let native = match helm.selected_match() {
            Some((src, _)) => {
                let source = &helm.sources[src];
                let idx = helm.candidate_index(helm.selected);
                match (&source.action, idx) {
                    (HelmAction::GotoLocation(locs), Some(i)) => {
                        locs.get(i).cloned().map(NativePreview::Location)
                    }
                    (HelmAction::ApplyCodeAction(acts), Some(i)) => {
                        acts.get(i).cloned().map(NativePreview::CodeAction)
                    }
                    _ => None,
                }
            }
            None => None,
        };
        if let Some(native) = native {
            let text = match native {
                NativePreview::Location(loc) => self.location_preview(&loc),
                NativePreview::CodeAction(action) => Self::code_action_preview(&action),
            };
            if let Some(helm) = self.helm.as_mut() {
                helm.preview = text;
            }
            return;
        }
        let Some(helm) = self.helm.as_ref() else {
            return;
        };
        let selected = helm.selected_match();
        let preview_fn = selected.and_then(|(src, _)| helm.sources[src].preview.clone());
        let candidate = selected.map(|(_, c)| c.to_string());
        match (preview_fn, candidate) {
            (Some(function), Some(candidate)) => {
                self.helm_query_counter += 1;
                let id = self.helm_query_counter;
                if let Some(helm) = self.helm.as_mut() {
                    helm.preview_query = id;
                }
                // `usize::MAX - 1` tags preview results (distinct from dynamic
                // candidates' `usize::MAX` and real Helm source indices).
                self.script
                    .dispatch_candidates(usize::MAX - 1, id, &function, &candidate);
            }
            _ => {
                if let Some(helm) = self.helm.as_mut() {
                    helm.preview.clear();
                }
            }
        }
    }

    /// Install a preview result if it's still current.
    pub(super) fn helm_set_preview(
        &mut self,
        query_id: u64,
        lines: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(helm) = self.helm.as_mut()
            && helm.preview_query == query_id
        {
            helm.preview = lines.join("\n");
            cx.notify();
        }
    }

    /// Built-in `helm-M-x`: every command name, running the chosen one.
    pub(super) fn helm_mx(&mut self, cx: &mut Context<Self>) {
        let source = HelmSource {
            name: "Commands".to_string(),
            // Recently run commands first (savehist's command history).
            candidates: self.commands_by_recency(self.script.command_names()),
            annotations: HashMap::new(),
            dynamic: None,
            raw: false,
            action: HelmAction::ExecuteCommand,
            preview: None,
        };
        self.helm_open("M-x ".to_string(), vec![source], String::new(), cx);
    }

    /// How many candidates match the live pattern (0 with no session). Dynamic
    /// sources are filtered too — their fn supplies the pool.
    pub(super) fn helm_match_count(&self) -> usize {
        self.helm.as_ref().map_or(0, HelmSession::match_count)
    }

    /// (Re)compute every dynamic source's candidates for the current pattern, on
    /// the engine thread. Results return via `helm_set_candidates`.
    pub(super) fn helm_query_dynamic(&self) {
        let Some(helm) = self.helm.as_ref() else {
            return;
        };
        for (i, source) in helm.sources.iter().enumerate() {
            if let Some(function) = &source.dynamic {
                self.script
                    .dispatch_candidates(i, helm.query_id, function, &helm.pattern);
            }
        }
    }

    /// On a pattern change, bump the query id and re-query dynamic sources.
    pub(super) fn helm_requery(&mut self) {
        let has_dynamic = self
            .helm
            .as_ref()
            .is_some_and(|h| h.sources.iter().any(|s| s.dynamic.is_some()));
        if has_dynamic {
            self.helm_query_counter += 1;
            let id = self.helm_query_counter;
            if let Some(helm) = self.helm.as_mut() {
                helm.query_id = id;
            }
            self.helm_query_dynamic();
        }
    }

    /// Install fresh dynamic candidates for `source` if they're still current.
    pub(super) fn helm_set_candidates(
        &mut self,
        source: usize,
        query_id: u64,
        candidates: Vec<(String, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        let current = {
            let Some(helm) = self.helm.as_mut() else {
                return;
            };
            if helm.query_id != query_id {
                return; // stale
            }
            if let Some(src) = helm.sources.get_mut(source) {
                src.annotations.clear();
                src.candidates = candidates
                    .into_iter()
                    .map(|(c, ann)| {
                        if let Some(ann) = ann {
                            src.annotations.insert(c.clone(), ann);
                        }
                        c
                    })
                    .collect();
            }
            helm.refilter();
            true
        };
        if current {
            if let Some(helm) = self.helm.as_mut() {
                let count = helm.match_count();
                if helm.selected >= count {
                    helm.selected = count.saturating_sub(1);
                }
            }
            self.helm_update_preview();
            cx.notify();
        }
    }

    pub(super) fn helm_input(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(helm) = self.helm.as_mut() {
            let mut pattern = format!("{}{text}", helm.pattern);
            if let Some(shadowed) = helm_file_shadow_pattern(&helm.sources, &pattern) {
                pattern = shadowed;
            }
            helm.set_pattern(pattern);
            helm.selected = 0;
        }
        self.helm_requery();
        self.helm_update_preview();
        cx.notify();
    }

    pub(super) fn helm_backspace(&mut self, cx: &mut Context<Self>) {
        if let Some(helm) = self.helm.as_mut() {
            let mut pattern = std::mem::take(&mut helm.pattern);
            pattern.pop();
            helm.set_pattern(pattern);
            helm.selected = 0;
        }
        self.helm_requery();
        self.helm_update_preview();
        cx.notify();
    }

    pub(super) fn helm_next(&mut self, cx: &mut Context<Self>) {
        self.helm_cycle(Direction::Forward, cx);
    }

    pub(super) fn helm_previous(&mut self, cx: &mut Context<Self>) {
        self.helm_cycle(Direction::Backward, cx);
    }

    fn helm_cycle(&mut self, dir: Direction, cx: &mut Context<Self>) {
        if let Some(helm) = self.helm.as_mut() {
            helm.selected = wrap_index(helm.selected, helm.match_count(), dir);
        }
        self.helm_update_preview();
        cx.notify();
    }

    /// The `(source_index, candidate)` currently under the Helm cursor, if any.
    fn helm_selected(&self) -> Option<(usize, String)> {
        let (src, candidate) = self.helm.as_ref()?.selected_match()?;
        Some((src, candidate.to_string()))
    }

    /// A mouse click on candidate row `index`: select it and run its default
    /// action (a directory descends, mirroring RET).
    pub fn helm_click(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.helm_match_count() {
            return;
        }
        if let Some(helm) = self.helm.as_mut() {
            helm.selected = index;
        }
        self.helm_accept(cx);
    }

    pub(super) fn helm_abort(&mut self, cx: &mut Context<Self>) {
        self.helm = None;
        cx.notify();
    }

    /// `C-SPC`: toggle the selected candidate's mark, then advance — so repeated
    /// presses mark a run. Accepting then runs the action for every marked item.
    pub(super) fn helm_toggle_mark(&mut self, cx: &mut Context<Self>) {
        let Some(helm) = self.helm.as_mut() else {
            return;
        };
        let Some(mark) = helm.selected_match().map(|(s, c)| (s, c.to_string())) else {
            return;
        };
        if mark.1.ends_with('/') {
            return; // directories descend, not mark
        }
        if let Some(pos) = helm.marked.iter().position(|m| m == &mark) {
            helm.marked.remove(pos);
        } else {
            helm.marked.push(mark);
        }
        helm.selected = wrap_index(helm.selected, helm.match_count(), Direction::Forward);
        cx.notify();
    }

    /// `TAB`: complete the live input to the selected candidate. A directory
    /// candidate ends with `/`, so a dynamic file source then lists its contents
    /// — i.e. TAB descends into the matched folder.
    pub(super) fn helm_complete(&mut self, cx: &mut Context<Self>) {
        let Some((_, candidate)) = self.helm_selected() else {
            return;
        };
        if let Some(helm) = self.helm.as_mut() {
            if helm.pattern == candidate {
                return; // nothing to expand
            }
            helm.set_pattern(candidate);
            helm.selected = 0;
        }
        self.helm_requery(); // dynamic sources relist for the new path
        cx.notify();
    }

    /// Run the selected candidate's default action, then close the session.
    ///
    /// RET acts on the highlighted candidate (the top match), mirroring Emacs
    /// Helm — so a partial pattern still opens the selected file. Only when the
    /// match list is empty does a file picker fall back to visiting the typed
    /// path (find-file's create-a-new-file behaviour).
    pub(super) fn helm_accept(&mut self, cx: &mut Context<Self>) {
        let Some((source_index, candidate)) = self.helm_selected() else {
            // Nothing matches the pattern: for a file picker, visit the typed
            // path so RET can create a new (not-yet-existing) file.
            if let Some((action, target)) = self
                .helm
                .as_ref()
                .and_then(|helm| helm_file_accept_target(&helm.sources, &helm.pattern))
            {
                self.helm = None;
                match action {
                    HelmAction::Scheme(actions) => {
                        if let Some((_, function)) = actions.first() {
                            self.script.dispatch_action(function, Some(&target));
                        }
                    }
                    HelmAction::ExecuteCommand => self.script.dispatch_action(&target, None),
                    _ => {}
                }
                cx.notify();
            }
            return;
        };
        // A directory (trailing slash): RET descends into it rather than acting.
        if candidate.ends_with('/') {
            self.helm_complete(cx);
            return;
        }
        let Some(helm) = self.helm.as_ref() else {
            return;
        };
        let action = helm.sources[source_index].action.clone();
        // The accepted candidate's index within the (unfiltered) source — native
        // actions use it to pick from their parallel target vec, since Helm
        // filters and reorders the visible matches.
        let candidate_index = helm.candidate_index(helm.selected);
        // Multi-select: act on every marked candidate (in mark order), each with
        // its own source's default action. With none marked, fall back to the
        // single candidate under the cursor.
        let targets: Vec<(String, String)> = if helm.marked.is_empty() {
            match &action {
                HelmAction::Scheme(actions) => actions
                    .first()
                    .map(|(_, f)| (f.clone(), candidate.clone()))
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            }
        } else {
            helm.marked
                .iter()
                .filter_map(|(source, c)| match &helm.sources.get(*source)?.action {
                    HelmAction::Scheme(actions) => Some((actions.first()?.1.clone(), c.clone())),
                    _ => None,
                })
                .collect()
        };
        self.helm = None;
        // Actions are Steel functions, dispatched by name to the engine thread;
        // their commands arrive via `apply_script_outcome`.
        match action {
            HelmAction::Scheme(_) => {
                for (function, target) in &targets {
                    // helm M-x (help.scm): note the command for `C-x z`.
                    if function == "execute-command" {
                        self.emacs_note_mx(target);
                    }
                    self.script.dispatch_action(function, Some(target));
                }
            }
            HelmAction::ExecuteCommand => {
                self.note_command(&candidate);
                self.script.dispatch_action(&candidate, None);
            }
            HelmAction::ActionMenu { targets, actions } => {
                if let Some((_, function)) = actions.iter().find(|(label, _)| label == &candidate) {
                    for target in &targets {
                        self.script.dispatch_action(function, Some(target));
                    }
                }
            }
            HelmAction::GotoLocation(locations) => {
                if let Some(loc) = candidate_index.and_then(|i| locations.get(i)).cloned() {
                    self.goto_file_position(loc.uri, loc.start.line, loc.start.character, cx);
                }
            }
            HelmAction::ApplyCodeAction(actions) => {
                if let Some(action) = candidate_index.and_then(|i| actions.get(i)).cloned() {
                    self.apply_code_action(action, cx);
                }
            }
        }
        cx.notify();
    }

    /// `M-a` (`helm-actions`): with multiple actions, open an action-menu
    /// sub-Helm; otherwise run the default action.
    pub(super) fn helm_action_menu(&mut self, cx: &mut Context<Self>) {
        let Some((source_index, candidate)) = self.helm_selected() else {
            return;
        };
        let actions = self
            .helm
            .as_ref()
            .and_then(|h| match &h.sources[source_index].action {
                HelmAction::Scheme(a) if a.len() > 1 => Some(a.clone()),
                _ => None,
            });
        let Some(actions) = actions else {
            self.helm_accept(cx); // single action / M-x: just run it
            return;
        };
        let labels: Vec<String> = actions.iter().map(|(label, _)| label.clone()).collect();
        // The menu acts on the candidates marked in this source (other sources'
        // actions differ), else on the one under the cursor.
        let mut targets: Vec<String> = self
            .helm
            .as_ref()
            .map(|h| {
                h.marked
                    .iter()
                    .filter(|(s, _)| *s == source_index)
                    .map(|(_, c)| c.clone())
                    .collect()
            })
            .unwrap_or_default();
        if targets.is_empty() {
            targets.push(candidate);
        }
        self.helm_open(
            "Action: ".to_string(),
            vec![HelmSource {
                name: "Actions".to_string(),
                candidates: labels,
                annotations: HashMap::new(),
                dynamic: None,
                raw: false,
                action: HelmAction::ActionMenu { targets, actions },
                preview: None,
            }],
            String::new(),
            cx,
        );
    }

    /// A render snapshot of the Helm panel (windowed around the selection, with
    /// a header whenever the source changes). `max_lines` is how many rows the
    /// panel can show; source headers count against it too.
    pub fn helm_view(&self, max_lines: usize) -> Option<HelmView> {
        let helm = self.helm.as_ref()?;
        let (start, end) = helm_window(&helm.matches, helm.selected, max_lines);

        let mut rows = Vec::new();
        let mut last_source = None;
        for i in start..end {
            let Some((source_index, candidate)) = helm.match_at(i) else {
                break;
            };
            if last_source != Some(source_index) {
                rows.push(HelmRow {
                    text: helm.sources[source_index].name.clone(),
                    annotation: None,
                    header: true,
                    selected: false,
                    marked: false,
                    index: None,
                });
                last_source = Some(source_index);
            }
            rows.push(HelmRow {
                text: candidate.to_string(),
                annotation: helm.sources[source_index]
                    .annotations
                    .get(candidate)
                    .cloned(),
                header: false,
                selected: i == helm.selected,
                marked: helm
                    .marked
                    .iter()
                    .any(|(s, c)| *s == source_index && c == candidate),
                index: Some(i),
            });
        }
        Some(HelmView {
            prompt: helm.prompt.clone(),
            pattern: helm.pattern.clone(),
            rows,
            preview: (!helm.preview.is_empty()).then(|| helm.preview.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn helm_window_fills_available_lines() {
        use super::helm_window;
        let one: Vec<_> = (0..40).map(|i| (0, i)).collect();
        // Header + 29 candidates fill a 30-line panel from the top.
        assert_eq!(helm_window(&one, 0, 30), (0, 29));
        // Scrolled past the bottom: the selection is the last visible row.
        assert_eq!(helm_window(&one, 35, 30), (7, 36));
        // Fewer candidates than lines: show them all.
        assert_eq!(helm_window(&one[..5], 2, 30), (0, 5));
        // Each source change costs a header line.
        let two: Vec<_> = (0..10).map(|i| (i / 5, i)).collect();
        assert_eq!(helm_window(&two, 0, 7), (0, 5));
        assert_eq!(helm_window(&two, 0, 8), (0, 6));
        assert_eq!(helm_window(&[], 0, 30), (0, 0));
    }

    #[test]
    fn duplicate_candidates_map_to_their_own_index() {
        use super::{HelmAction, HelmSession, HelmSource};
        // Two LSP locations rendering the same row (same basename/line/col in
        // different directories) must still select distinct targets.
        let sources = || {
            vec![HelmSource {
                name: "References".into(),
                candidates: vec![
                    "mod.rs:3:1: x".into(),
                    "lib.rs:9:2: y".into(),
                    "mod.rs:3:1: x".into(),
                ],
                annotations: std::collections::HashMap::new(),
                dynamic: None,
                raw: false,
                action: HelmAction::ExecuteCommand,
                preview: None,
            }]
        };
        let all = HelmSession::new(String::new(), String::new(), sources(), 0, true);
        let idx: Vec<_> = (0..3).map(|i| all.candidate_index(i)).collect();
        assert_eq!(idx, vec![Some(0), Some(1), Some(2)]);
        // Filtered (fuzzy + substring): copies keep their source order.
        for fuzzy in [true, false] {
            let m = HelmSession::new(String::new(), "mod".into(), sources(), 0, fuzzy);
            assert_eq!(m.match_count(), 2);
            assert_eq!(m.candidate_index(0), Some(0));
            assert_eq!(m.candidate_index(1), Some(2));
            assert_eq!(m.candidate_index(2), None);
        }
    }

    #[test]
    fn helm_fuzzy_ranks_and_filters() {
        use super::{HelmAction, HelmSession, HelmSource};
        let sources = || {
            vec![HelmSource {
                name: "Files".into(),
                candidates: vec![
                    "foo/bar.rs".into(),
                    "fxxxbxxxr.rs".into(),
                    "unrelated.txt".into(),
                ],
                annotations: std::collections::HashMap::new(),
                dynamic: None,
                raw: false,
                action: HelmAction::ExecuteCommand,
                preview: None,
            }]
        };

        let filter = |pattern: &str, fuzzy| {
            let s = HelmSession::new(String::new(), pattern.into(), sources(), 0, fuzzy);
            (0..s.match_count())
                .map(|i| s.match_at(i).unwrap().1.to_string())
                .collect::<Vec<_>>()
        };
        // Empty pattern: everything, in original order.
        let all = filter("", true);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], "foo/bar.rs");

        // Fuzzy "fbr": subsequence matches only; tighter match ranks first, and
        // the non-matching candidate is dropped.
        let fuzzy = filter("fbr", true);
        assert!(
            fuzzy.iter().all(|c| c != "unrelated.txt"),
            "non-match filtered"
        );
        assert_eq!(fuzzy[0], "foo/bar.rs", "the tighter match ranks first");

        // Substring mode: "bar" is a literal substring of only one candidate.
        let subs = filter("bar", false);
        assert_eq!(subs, ["foo/bar.rs"]);
    }

    #[test]
    fn helm_filter_shows_raw_sources_verbatim() {
        use super::{HelmAction, HelmSession, HelmSource};
        // A `raw` (self-filtered) source ignores the pattern entirely: its
        // candidates pass through unchanged and in order — the Scheme fn that
        // produced them is the sole filter authority.
        let sources = vec![HelmSource {
            name: "Scheme-filtered".into(),
            candidates: vec!["zebra".into(), "apple".into()],
            annotations: std::collections::HashMap::new(),
            dynamic: Some("my-filter".into()),
            raw: true,
            action: HelmAction::ExecuteCommand,
            preview: None,
        }];
        let out = HelmSession::new(String::new(), "nomatch".into(), sources, 0, true);
        assert_eq!(
            (0..out.match_count())
                .map(|i| out.match_at(i).unwrap().1)
                .collect::<Vec<_>>(),
            ["zebra", "apple"],
        );
    }

    #[test]
    fn helm_file_shadow_pattern_jumps_to_root_for_file_sources() {
        use super::{HelmAction, HelmSource, helm_file_shadow_pattern};
        let sources = vec![HelmSource {
            name: "Files".into(),
            candidates: Vec::new(),
            annotations: std::collections::HashMap::new(),
            dynamic: Some("find-file-candidates".into()),
            raw: false,
            action: HelmAction::ExecuteCommand,
            preview: None,
        }];

        assert_eq!(
            helm_file_shadow_pattern(&sources, "/Users/me/project//"),
            Some("/".into())
        );
        assert_eq!(
            helm_file_shadow_pattern(&sources, "/Users/me/project//etc"),
            Some("/etc".into())
        );
        assert_eq!(
            helm_file_shadow_pattern(&sources, "/Users//me//project"),
            Some("/project".into())
        );
        assert_eq!(
            helm_file_shadow_pattern(&sources, "/Users/me/project/"),
            None
        );
    }

    #[test]
    fn helm_file_shadow_pattern_jumps_home_for_tilde() {
        use super::{HelmAction, HelmSource, helm_file_shadow_pattern};
        let sources = vec![HelmSource {
            name: "Files".into(),
            candidates: Vec::new(),
            annotations: std::collections::HashMap::new(),
            dynamic: Some("find-file-candidates".into()),
            raw: false,
            action: HelmAction::ExecuteCommand,
            preview: None,
        }];
        let home = |tail: &str| crate::util::expand_tilde(&format!("~/{tail}"));

        assert_eq!(
            helm_file_shadow_pattern(&sources, "/Users/me/project/~/"),
            Some(home(""))
        );
        assert_eq!(
            helm_file_shadow_pattern(&sources, "/Users/me/project/~/notes.org"),
            Some(home("notes.org"))
        );
        assert_eq!(
            helm_file_shadow_pattern(&sources, "~/src"),
            Some(home("src"))
        );
        // The later shortcut wins.
        assert_eq!(
            helm_file_shadow_pattern(&sources, "/a/~/b//etc"),
            Some("/etc".into())
        );
        assert_eq!(
            helm_file_shadow_pattern(&sources, "/a//b/~/c"),
            Some(home("c"))
        );
        // `~` inside a name is just a character.
        assert_eq!(helm_file_shadow_pattern(&sources, "/a/backup~/x"), None);
        assert_eq!(helm_file_shadow_pattern(&sources, "/a/~b"), None);
    }

    #[test]
    fn helm_file_shadow_pattern_ignores_generic_helm_sources() {
        use super::{HelmAction, HelmSource, helm_file_shadow_pattern};
        let sources = vec![HelmSource {
            name: "Commands".into(),
            candidates: Vec::new(),
            annotations: std::collections::HashMap::new(),
            dynamic: None,
            raw: false,
            action: HelmAction::ExecuteCommand,
            preview: None,
        }];

        assert_eq!(helm_file_shadow_pattern(&sources, "foo//bar"), None);
    }

    #[test]
    fn helm_file_accept_target_uses_typed_new_file_path() {
        use super::{HelmAction, HelmSource, helm_file_accept_target};
        let sources = vec![HelmSource {
            name: "Files".into(),
            candidates: vec!["/tmp/existing.rs".into()],
            annotations: std::collections::HashMap::new(),
            dynamic: Some("find-file-candidates".into()),
            raw: false,
            action: HelmAction::Scheme(vec![("open".into(), "open-file".into())]),
            preview: None,
        }];

        let Some((action, target)) = helm_file_accept_target(&sources, "/tmp/new-file.rs") else {
            panic!("new file path should be accepted");
        };
        assert_eq!(target, "/tmp/new-file.rs");
        assert!(matches!(action, HelmAction::Scheme(actions) if actions[0].1 == "open-file"));
    }

    #[test]
    fn helm_file_accept_target_keeps_existing_candidates_and_dirs_normal() {
        use super::{HelmAction, HelmSource, helm_file_accept_target};
        let sources = vec![HelmSource {
            name: "Files".into(),
            candidates: vec!["/tmp/existing.rs".into()],
            annotations: std::collections::HashMap::new(),
            dynamic: Some("find-file-candidates".into()),
            raw: false,
            action: HelmAction::Scheme(vec![("open".into(), "open-file".into())]),
            preview: None,
        }];

        assert_eq!(
            helm_file_accept_target(&sources, "/tmp/existing.rs").map(|(_, t)| t),
            None
        );
        assert_eq!(
            helm_file_accept_target(&sources, "/tmp/").map(|(_, t)| t),
            None
        );
    }
}
