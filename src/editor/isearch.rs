//! Incremental search for [`Editor`], extracted from the editor module for size.
//! These methods extend the same `Editor` type via an additional
//! `impl` block; cross-module entry points are `pub(super)`.

use std::ops::Range;

use gpui::Context;
use regex::Regex;

use super::*;

/// Incremental-search (`C-s`) transient state, held in [`Editor::isearch`]
/// only while an `Isearch` minibuffer is open. `dir` is the current
/// direction (toggled by C-r/C-s), `origin` is where point was when the search
/// began (restored on C-g), `from` is the offset the current attempt searches
/// from, and `matched` is the range of the current hit (the base for the next
/// repeat). `kind` is how the query matches (`M-r` toggles regexp).
pub(super) struct IsearchState {
    pub(super) dir: Direction,
    pub(super) origin: usize,
    pub(super) from: usize,
    pub(super) matched: Option<Range<usize>>,
    pub(super) kind: IsearchKind,
}

/// How an isearch query matches, as in Emacs: `C-s` is literal, `C-M-s` a
/// regexp, and `M-s .` a whole symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IsearchKind {
    Literal,
    Regexp,
    Symbol,
}

impl IsearchKind {
    /// The regex source matching `query` (a symbol's ends are checked
    /// separately; `\b` here only keeps the highlight close).
    fn pattern(self, query: &str) -> String {
        match self {
            Self::Regexp => query.to_string(),
            Self::Literal => regex::escape(query),
            Self::Symbol => {
                let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
                let open = if word(query.chars().next()) {
                    r"\b"
                } else {
                    ""
                };
                let close = if word(query.chars().next_back()) {
                    r"\b"
                } else {
                    ""
                };
                format!("{open}{}{close}", regex::escape(query))
            }
        }
    }

    fn prompt(self, dir: Direction) -> &'static str {
        match (self, dir) {
            (Self::Literal, Direction::Forward) => "I-search: ",
            (Self::Literal, Direction::Backward) => "I-search backward: ",
            (Self::Regexp, Direction::Forward) => "Regexp I-search: ",
            (Self::Regexp, Direction::Backward) => "Regexp I-search backward: ",
            (Self::Symbol, Direction::Forward) => "Symbol I-search: ",
            (Self::Symbol, Direction::Backward) => "Symbol I-search backward: ",
        }
    }
}

impl Editor {
    /// Byte ranges of the active search query's matches *within `[start, end)`*
    /// (the visible region), in absolute buffer offsets. Empty when no search is
    /// active or the regex doesn't compile. Scoped to the viewport so the cost
    /// is O(visible text), not O(document), and stays correct-by-construction
    /// (recomputed from the live buffer each frame — no cache to go stale).
    /// Navigation (`n`/`N`) searches the whole buffer separately in `do_search`.
    pub fn search_match_ranges_in(&self, start: usize, end: usize) -> Vec<Range<usize>> {
        let Some(query) = self.search_query.as_ref().filter(|q| !q.is_empty()) else {
            return Vec::new();
        };
        let Some(regex) = self.compiled_search(query) else {
            return Vec::new();
        };
        let end = end.min(self.active.buffer.len());
        let start = start.min(end);
        // `start`/`end` are line boundaries (char-safe); slice only the visible
        // bytes, then map match offsets back to absolute positions.
        let slice = self.active.buffer.slice_to_string(start..end);
        regex
            .find_iter(&slice)
            .filter(|m| m.end() > m.start()) // skip zero-width matches
            .map(|m| (start + m.start())..(start + m.end()))
            .collect()
    }

    /// `query` compiled, from the cache when it's the query compiled last.
    /// (`Regex` clones share the compiled program.)
    fn compiled_search(&self, query: &str) -> Option<Regex> {
        let mut cache = self.search_regex.borrow_mut();
        if let Some((cached, regex)) = cache.as_ref()
            && cached == query
        {
            return Some(regex.clone());
        }
        let regex = Regex::new(query).ok()?;
        *cache = Some((query.to_string(), regex.clone()));
        Some(regex)
    }

    // --- Incremental search (Emacs `C-s` / `C-r`) -------------------------

    /// Start an incremental search for a literal string. Reads a query in the
    /// minibuffer and matches it live (see `isearch_update`); `C-s`/`C-r`
    /// repeat, `RET` finishes leaving point on the match, and `C-g` aborts back
    /// to where the search began.
    pub(super) fn isearch_start(&mut self, dir: Direction, cx: &mut Context<Self>) {
        self.isearch_start_with(dir, IsearchKind::Literal, "", cx);
    }

    /// [`isearch_start`](Self::isearch_start) matching as `kind`, with the
    /// query pre-filled with `initial` (searched at once when non-empty).
    pub(super) fn isearch_start_with(
        &mut self,
        dir: Direction,
        kind: IsearchKind,
        initial: &str,
        cx: &mut Context<Self>,
    ) {
        let point = self.point();
        self.isearch = Some(IsearchState {
            dir,
            origin: point,
            from: point,
            matched: None,
            kind,
        });
        self.search_query = None;
        self.open_minibuffer(kind.prompt(dir), MinibufferPurpose::Isearch, initial, cx);
        if !initial.is_empty() {
            self.isearch_update(cx);
        }
    }

    /// `M-r` while searching: toggle between a literal and a regexp search,
    /// re-matching the query from where this attempt started.
    pub(super) fn isearch_toggle_regexp(&mut self, cx: &mut Context<Self>) {
        let Some(st) = self.isearch.as_mut() else {
            return;
        };
        st.kind = match st.kind {
            IsearchKind::Regexp => IsearchKind::Literal,
            _ => IsearchKind::Regexp,
        };
        let prompt = st.kind.prompt(st.dir);
        if let Some(mb) = self.minibuffer.as_mut() {
            mb.prompt = prompt.into();
        }
        self.isearch_update(cx);
    }

    /// The highlight / `n` pattern for a finished isearch query.
    pub(super) fn isearch_pattern(&self, query: &str) -> String {
        let kind = self
            .isearch
            .as_ref()
            .map_or(IsearchKind::Literal, |st| st.kind);
        kind.pattern(query)
    }

    /// Re-run the search for the current minibuffer string from `isearch_from`.
    /// Called on every edit of the search string, so matching is incremental.
    pub(super) fn isearch_update(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self.minibuffer.as_ref().map(|mb| mb.input.clone()) else {
            return;
        };
        let Some((dir, from)) = self.isearch.as_ref().map(|st| (st.dir, st.from)) else {
            return;
        };
        self.isearch_run(&query, dir, from, cx);
    }

    /// `C-s` / `C-r` while searching: repeat in direction `dir`. An empty
    /// search string first recalls the previous one (Emacs `C-s C-s`); otherwise
    /// the search advances past the current match.
    pub(super) fn isearch_repeat(&mut self, dir: Direction, cx: &mut Context<Self>) {
        let Some(st) = self.isearch.as_mut() else {
            return;
        };
        st.dir = dir;
        let empty = self
            .minibuffer
            .as_ref()
            .is_some_and(|mb| mb.input.is_empty());
        if empty {
            if let Some(prev) = self.last_isearch.clone()
                && let Some(mb) = self.minibuffer.as_mut()
            {
                mb.set_input(prev);
            }
        } else if let Some(m) = st.matched.clone() {
            // Step off the current match so the next hit is a different one.
            // Backward from a match at byte 0 there's nothing before it: wrap
            // to the end so the search lands on the LAST match (saturating to
            // 0 would just find the same match again).
            st.from = match dir {
                Direction::Forward => m.start + 1,
                Direction::Backward => m.start.checked_sub(1).unwrap_or(usize::MAX),
            };
        }
        let from = self.isearch.as_ref().map_or(0, |st| st.from);
        let query = self
            .minibuffer
            .as_ref()
            .map(|mb| mb.input.clone())
            .unwrap_or_default();
        self.isearch_run(&query, dir, from, cx);
    }

    /// Find the next match of `query` from `from` in the given direction, wrapping
    /// around the buffer; move point onto it (its end going forward, its start
    /// going backward) and record the match. Updates the minibuffer hint on
    /// failure or an invalid regex. An empty query just clears the highlight.
    pub(super) fn isearch_run(
        &mut self,
        query: &str,
        dir: Direction,
        from: usize,
        cx: &mut Context<Self>,
    ) {
        if query.is_empty() {
            self.search_query = None;
            if let Some(st) = self.isearch.as_mut() {
                st.matched = None;
            }
            if let Some(mb) = self.minibuffer.as_mut() {
                mb.message.clear();
            }
            cx.notify();
            return;
        }
        let kind = self
            .isearch
            .as_ref()
            .map_or(IsearchKind::Regexp, |st| st.kind);
        let pattern = kind.pattern(query);
        self.search_query = Some(pattern.clone());
        let regex = match Regex::new(&pattern) {
            Ok(r) => r,
            Err(_) => {
                if let Some(mb) = self.minibuffer.as_mut() {
                    mb.message = " [invalid regex]".into();
                }
                cx.notify();
                return;
            }
        };
        let text = self.active.buffer.full_text();
        // Narrowed: only matches wholly inside the accessible range count, and
        // text outside it doesn't touch a symbol's ends.
        let acc = self.accessible();
        let symbol_edge = |at: Option<char>| at.is_some_and(|c| self.is_symbol_char(c));
        let matches: Vec<Range<usize>> = regex
            .find_iter(&text)
            .filter(|m| m.end() > m.start() && acc.start <= m.start() && m.end() <= acc.end)
            .filter(|m| {
                kind != IsearchKind::Symbol
                    || !(symbol_edge(text[acc.start..m.start()].chars().next_back())
                        || symbol_edge(text[m.end()..acc.end].chars().next()))
            })
            .map(|m| m.start()..m.end())
            .collect();
        let found = match dir {
            Direction::Forward => matches
                .iter()
                .find(|m| m.start >= from)
                .or_else(|| matches.first())
                .cloned(),
            Direction::Backward => matches
                .iter()
                .rev()
                .find(|m| m.start <= from)
                .or_else(|| matches.last())
                .cloned(),
        };
        match found {
            Some(m) => {
                let target = match dir {
                    Direction::Forward => m.end,
                    Direction::Backward => m.start,
                };
                if let Some(st) = self.isearch.as_mut() {
                    st.matched = Some(m);
                }
                if let Some(mb) = self.minibuffer.as_mut() {
                    mb.message.clear();
                }
                self.move_to(target, cx);
            }
            None => {
                if let Some(st) = self.isearch.as_mut() {
                    st.matched = None;
                }
                if let Some(mb) = self.minibuffer.as_mut() {
                    mb.message = " [failing]".into();
                }
                cx.notify();
            }
        }
    }
}
