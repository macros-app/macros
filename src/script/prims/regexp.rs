//! Regexp primitives over strings and over the buffer, with a small per-
//! thread compile cache.

use regex::Regex;
use ropey::Rope;
use steel::rvals::SteelVal;

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared { snapshot, .. } = ctx;
    reg.section(Section::Regexp);
    // --- Regexp (over strings, and over the buffer from point) ------------
    reg.register_fn("string-match", move |pattern: String, s: String| -> isize {
        regex(&pattern)
            .ok()
            .and_then(|re| re.find(&s).map(|m| m.start() as isize))
            .unwrap_or(-1)
    });
    reg.register_fn(
        "regexp-captures",
        move |pattern: String, s: String| -> Vec<String> {
            // Group 0 = whole match, then each capture group (absent group = "").
            match regex(&pattern).ok().and_then(|re| re.captures(&s)) {
                Some(caps) => caps
                    .iter()
                    .map(|g| g.map(|m| m.as_str().to_string()).unwrap_or_default())
                    .collect(),
                None => Vec::new(),
            }
        },
    );
    reg.register_fn(
        "replace-regexp-in-string",
        move |pattern: String, replacement: String, s: String| -> String {
            // `$1` / `${name}` expansion (regex-crate syntax).
            match regex(&pattern) {
                Ok(re) => re.replace_all(&s, replacement.as_str()).into_owned(),
                Err(_) => s,
            }
        },
    );
    // `(regexp-all-matches regex s)` → every non-empty match of `regex` in the
    // string `s`, in order (`()` for an invalid regex). dabbrev's word scan
    // over other buffers' text.
    reg.register_fn(
        "regexp-all-matches",
        move |pattern: String, s: String| -> Vec<String> {
            regex(&pattern)
                .ok()
                .map(|re| {
                    re.find_iter(&s)
                        .filter(|m| !m.as_str().is_empty())
                        .map(|m| m.as_str().to_string())
                        .collect()
                })
                .unwrap_or_default()
        },
    );
    {
        // `(buffer-search-all regex start end)` → every match of `regex` in the
        // active buffer whose bytes lie within `[start, end)`, as a list of
        // `(mstart mend "match" "group1" …)` records (BYTE offsets; an unmatched
        // group is ""). `^`/`$` are line anchors (multi-line), evaluated against
        // the real line boundaries even when `start`/`end` fall mid-line. An
        // invalid regex returns its error message as a string instead. Reads
        // the published snapshot rope directly — no whole-buffer string crosses
        // into Scheme (query-replace / occur on big files).
        let shared = snapshot.clone();
        reg.register_fn(
            "buffer-search-all",
            move |pattern: String, NonNeg(start): NonNeg, NonNeg(end): NonNeg| -> SteelVal {
                let re = match ml_regex(&pattern) {
                    Ok(re) => re,
                    Err(err) => return SteelVal::StringV(err.to_string().into()),
                };
                let rope = shared.load().text.clone();
                buffer_search_all(&rope, &re, start, end)
            },
        );
    }
    // `(regexp-search-lines regex lines)` → for each string in `lines`, the
    // list of its non-empty matches as `(start end)` BYTE offsets. One call and
    // one regex compile for a whole result buffer (occur / grep highlighting);
    // an invalid regex yields `()` for every line.
    reg.register_fn(
        "regexp-search-lines",
        |pattern: String, StringList(lines): StringList| -> SteelVal {
            let re = ml_regex(&pattern).ok();
            let per_line: Vec<SteelVal> = lines
                .iter()
                .map(|line| {
                    let spans: Vec<SteelVal> = re
                        .iter()
                        .flat_map(|re| re.find_iter(line))
                        .filter(|m| m.end() > m.start())
                        .map(|m| {
                            SteelVal::ListV(
                                vec![
                                    SteelVal::IntV(m.start() as isize),
                                    SteelVal::IntV(m.end() as isize),
                                ]
                                .into(),
                            )
                        })
                        .collect();
                    SteelVal::ListV(spans.into())
                })
                .collect();
            SteelVal::ListV(per_line.into())
        },
    );
    {
        // `(re-search-forward pattern)` → `(start end)` of the next match at/after
        // point, or `()` if none. Scheme drives point/edits from the result.
        let shared = snapshot.clone();
        reg.register_fn("re-search-forward", move |pattern: String| -> Vec<usize> {
            let (rope, p) = {
                let snap = shared.load();
                (snap.text.clone(), snap.point)
            };
            let from = snap_byte(&rope, p);
            match re_search_forward_rope(&rope, from, &pattern) {
                Some((start, end)) => vec![start, end],
                None => Vec::new(),
            }
        });
    }
}

/// The pure core of `re-search-forward`: the first match of `pattern` in
/// `text` starting at byte `from`, as byte offsets into `text`. The whole text
/// is searched (from `from`), not a slice, so `^`, `\b` and lookbehind-ish
/// anchors still see the context before point. `^`/`$` match at line
/// boundaries, as in Emacs. `None` on no match or an invalid pattern.
pub(crate) fn re_search_forward_in(
    text: &str,
    from: usize,
    pattern: &str,
) -> Option<(usize, usize)> {
    let re = ml_regex(pattern).ok()?;
    if from > text.len() || !text.is_char_boundary(from) {
        return None;
    }
    re.find_at(text, from).map(|m| (m.start(), m.end()))
}

/// [`re_search_forward_in`] over a rope, without materializing the text
/// before point: only `[c, len)` is copied, `c` being the start of the
/// codepoint just before `from`. That is exactly as much context as the whole
/// text would give — every regex-crate look-around (`^`, `$`, `\b`, `\<`, …)
/// inspects at most one codepoint on either side — and since `from` stays
/// past the slice start, `\A` still can't match there (the whole-text search
/// only allows it at byte 0). `from` must be a char boundary.
pub(crate) fn re_search_forward_rope(
    rope: &Rope,
    from: usize,
    pattern: &str,
) -> Option<(usize, usize)> {
    let base = match rope.byte_to_char(from) {
        0 => 0,
        c => rope.char_to_byte(c - 1),
    };
    let text = rope.byte_slice(base..).to_string();
    re_search_forward_in(&text, from - base, pattern).map(|(s, e)| (base + s, base + e))
}

/// How many compiled regexes [`regex`] / [`ml_regex`] keep per thread.
const REGEX_CACHE_SIZE: usize = 8;

/// `pattern` compiled (with `^`/`$` as line anchors when `multi_line`),
/// reusing a recent compile: Scheme calls the regex primitives with the same
/// few patterns over and over (a search loop, a highlighter per keystroke), and
/// compiling dominates a match on short text. A tiny most-recent-first list
/// per thread; errors aren't cached.
fn cached_regex(pattern: &str, multi_line: bool) -> Result<Regex, regex::Error> {
    thread_local! {
        static CACHE: std::cell::RefCell<Vec<(String, bool, Regex)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(i) = cache
            .iter()
            .position(|(p, ml, _)| *ml == multi_line && p == pattern)
        {
            let hit = cache.remove(i);
            let re = hit.2.clone();
            cache.insert(0, hit);
            return Ok(re);
        }
        let re = regex::RegexBuilder::new(pattern)
            .multi_line(multi_line)
            .build()?;
        cache.truncate(REGEX_CACHE_SIZE - 1);
        cache.insert(0, (pattern.to_string(), multi_line, re.clone()));
        Ok(re)
    })
}

/// `pattern` compiled with default flags (see [`cached_regex`]).
pub(crate) fn regex(pattern: &str) -> Result<Regex, regex::Error> {
    cached_regex(pattern, false)
}

/// `pattern` compiled with `^`/`$` matching at line boundaries, as in Emacs
/// (see [`cached_regex`]).
pub(crate) fn ml_regex(pattern: &str) -> Result<Regex, regex::Error> {
    cached_regex(pattern, true)
}

/// Matches of `re` inside bytes `[start, end)` of `rope` — the worker behind
/// `buffer-search-all`. Searches the enclosing whole lines so `^`/`$`/`\b`
/// see real context, then keeps only matches fully inside the window.
fn buffer_search_all(rope: &Rope, re: &Regex, start: usize, end: usize) -> SteelVal {
    let (s, e) = (snap_byte(rope, start), snap_byte(rope, end));
    if s > e {
        return SteelVal::ListV(Vec::<SteelVal>::new().into());
    }
    let from = rope.line_to_byte(rope.byte_to_line(s));
    let to = {
        let line = rope.byte_to_line(e);
        let line_start = rope.line_to_byte(line);
        let line_str = rope.line(line).to_string();
        line_start + line_str.find('\n').unwrap_or(line_str.len())
    }
    .max(e);
    let text = rope.byte_slice(from..to).to_string();
    let out: Vec<SteelVal> = re
        .captures_iter(&text)
        .filter_map(|caps| {
            let m = caps.get(0)?;
            let (ms, me) = (from + m.start(), from + m.end());
            if ms < s || me > e {
                return None;
            }
            let mut rec = vec![SteelVal::IntV(ms as isize), SteelVal::IntV(me as isize)];
            rec.extend(
                caps.iter()
                    .map(|g| SteelVal::StringV(g.map_or("", |g| g.as_str()).into())),
            );
            Some(SteelVal::ListV(rec.into()))
        })
        .collect();
    SteelVal::ListV(out.into())
}

#[cfg(test)]
mod regex_tests {
    use super::*;

    /// The rope search sees the same context as the whole-text one: line and
    /// word anchors right at point, `\A` only at byte 0, multibyte text.
    #[test]
    fn rope_search_matches_whole_text_search() {
        let text = "foo bar\nbaz qux\n\u{e9}t\u{e9} foo\n";
        let rope = Rope::from_str(text);
        let patterns = [
            "^b",
            r"\bar",
            r"\Bar",
            "foo",
            r"\Afoo",
            r"\A",
            "$",
            r"(?m)^\w+$",
            "t\u{e9}",
            r"\b",
            "x*",
            "^",
            r"\z",
            "(",
        ];
        for pattern in patterns {
            for from in (0..=text.len()).filter(|&i| text.is_char_boundary(i)) {
                assert_eq!(
                    re_search_forward_rope(&rope, from, pattern),
                    re_search_forward_in(text, from, pattern),
                    "{pattern:?} from {from}"
                );
            }
        }
    }

    #[test]
    fn regex_cache_keys_on_flags() {
        let ml = ml_regex("^b").unwrap();
        let plain = regex("^b").unwrap();
        assert!(ml.is_match("a\nb"));
        assert!(!plain.is_match("a\nb"));
        assert!(regex("(").is_err());
        for i in 0..20 {
            assert!(regex(&format!("a{{{i}}}")).is_ok());
        }
        assert!(ml_regex("^b").unwrap().is_match("a\nb"));
    }
}
