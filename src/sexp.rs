//! A small, lenient Lisp reader over raw buffer text.
//!
//! Editor commands that work on s-expressions — `eval-last-sexp`, ANVIL's
//! eval-defun / macroexpand, Lisp indentation, bracket matching, structural
//! motion — need form boundaries that respect the reader: a `)` inside a
//! string, a `;` comment or a `#\)` char literal is not a delimiter, and a quote
//! prefix (`'(1 2)`) belongs to the datum it prefixes. Steel's own parser
//! rejects the half-typed text an editor is usually looking at, so this reader
//! never fails: unterminated strings and lists simply run to the end of the
//! input, and stray closers are skipped.
//!
//! Positions are byte offsets. Every delimiter is ASCII, so all offsets land on
//! UTF-8 char boundaries.

use std::ops::Range;

/// What a datum is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A bracketed list or vector. `open` is the byte of the opening bracket
    /// (after any `#`/`#u8`-style vector prefix); `close` the byte of its
    /// closing bracket, or `None` when the input ended first.
    List { open: usize, close: Option<usize> },
    /// A string literal; `closed` is false when the input ended inside it.
    Str { closed: bool },
    /// Anything else: symbols, numbers, booleans, char literals, `|sym|`.
    Atom,
}

/// One datum with its byte span. `start` includes any quote prefixes (`'`,
/// `` ` ``, `,`, `,@`, `#'`), so the span is what the reader consumes.
#[derive(Debug)]
pub struct Node {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
    pub children: Vec<Node>,
}

/// Dropped iteratively: a pathologically deep tree must not overflow the stack.
impl Drop for Node {
    fn drop(&mut self) {
        let mut pending = std::mem::take(&mut self.children);
        while let Some(mut node) = pending.pop() {
            pending.append(&mut node.children);
        }
    }
}

impl Node {
    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }

    /// Whether the datum is complete (a closed list or string, or any atom).
    pub fn is_complete(&self) -> bool {
        match self.kind {
            Kind::List { close, .. } => close.is_some(),
            Kind::Str { closed } => closed,
            Kind::Atom => true,
        }
    }
}

/// The result of reading a text: its top-level data plus what the reader ran
/// into on the way.
#[derive(Debug, Default)]
pub struct Parsed {
    pub nodes: Vec<Node>,
    /// Number of closers that matched no opener.
    pub stray_closers: usize,
    /// Whether the input ended inside a string or block comment.
    pub ends_in_literal: bool,
}

/// A list being read.
struct Frame {
    start: usize,
    open: usize,
    children: Vec<Node>,
    /// Start of a quote prefix waiting for its datum.
    prefix: Option<usize>,
    /// `#;` datum comments waiting to swallow the next datum.
    skip: usize,
}

impl Frame {
    fn new(start: usize, open: usize) -> Self {
        Self {
            start,
            open,
            children: Vec::new(),
            prefix: None,
            skip: 0,
        }
    }

    /// Add a finished datum, applying any pending prefix and datum comment.
    fn push(&mut self, mut node: Node) {
        if let Some(p) = self.prefix.take() {
            node.start = p;
        }
        if self.skip > 0 {
            self.skip -= 1;
        } else {
            self.children.push(node);
        }
    }
}

/// The closing bracket for opening bracket `open`.
fn closer_for(open: u8) -> Option<u8> {
    match open {
        b'(' => Some(b')'),
        b'[' => Some(b']'),
        b'{' => Some(b'}'),
        _ => None,
    }
}

fn is_delimiter(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'"' | b';')
}

/// Read `text` into its top-level data. Never fails.
pub fn parse(text: &str) -> Parsed {
    let b = text.as_bytes();
    let len = b.len();
    let mut out = Parsed::default();
    // The top level is frame 0; it is never closed.
    let mut stack = vec![Frame::new(0, 0)];
    let mut i = 0;
    while i < len {
        let c = b[i];
        match c {
            _ if c.is_ascii_whitespace() => i += 1,
            b';' => {
                while i < len && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' if b.get(i + 1) == Some(&b'|') => {
                i += 2;
                let mut depth = 1usize;
                while i < len && depth > 0 {
                    if b[i] == b'#' && b.get(i + 1) == Some(&b'|') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'|' && b.get(i + 1) == Some(&b'#') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                out.ends_in_literal = depth > 0;
            }
            b'#' if b.get(i + 1) == Some(&b';') => {
                stack.last_mut().unwrap().skip += 1;
                i += 2;
            }
            b'\'' | b'`' => {
                let f = stack.last_mut().unwrap();
                f.prefix.get_or_insert(i);
                i += 1;
            }
            b',' => {
                let f = stack.last_mut().unwrap();
                f.prefix.get_or_insert(i);
                i += if b.get(i + 1) == Some(&b'@') { 2 } else { 1 };
            }
            b'#' if matches!(b.get(i + 1), Some(b'\'' | b'`' | b',')) => {
                let f = stack.last_mut().unwrap();
                f.prefix.get_or_insert(i);
                i += if b.get(i + 1) == Some(&b',') && b.get(i + 2) == Some(&b'@') {
                    3
                } else {
                    2
                };
            }
            b'"' => {
                let start = i;
                i += 1;
                let mut closed = false;
                while i < len {
                    match b[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            closed = true;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                let end = i.min(len);
                i = end;
                out.ends_in_literal = !closed;
                stack.last_mut().unwrap().push(Node {
                    start,
                    end,
                    kind: Kind::Str { closed },
                    children: Vec::new(),
                });
            }
            b'(' | b'[' | b'{' => {
                let f = stack.last_mut().unwrap();
                let start = f.prefix.take().unwrap_or(i);
                stack.push(Frame::new(start, i));
                i += 1;
            }
            b')' | b']' | b'}' => {
                // Only the closer that matches the innermost opener closes it:
                // `(a]` is an unclosed list plus a stray `]`, not a pair.
                let matches =
                    stack.len() > 1 && closer_for(b[stack.last().unwrap().open]) == Some(c);
                if matches {
                    let f = stack.pop().unwrap();
                    stack.last_mut().unwrap().push(close_frame(f, Some(i), len));
                } else {
                    out.stray_closers += 1;
                }
                i += 1;
            }
            _ => {
                let start = i;
                let char_lit = c == b'#' && b.get(i + 1) == Some(&b'\\');
                if char_lit {
                    // Char literal: `#\` plus one char, plus the rest of a named
                    // char (`#\space`, `#\x41`) when that char is alphanumeric.
                    i += 2;
                    if let Some(ch) = text.get(i..).and_then(|s| s.chars().next()) {
                        i += ch.len_utf8();
                        if ch.is_alphanumeric() {
                            while i < len && !is_delimiter(b[i]) {
                                i += 1;
                            }
                        }
                    }
                } else {
                    while i < len && !is_delimiter(b[i]) {
                        if b[i] == b'|' {
                            // `|...|` quoted symbol (or a `|` run inside one).
                            i += 1;
                            while i < len && b[i] != b'|' {
                                i += 1;
                            }
                        }
                        i = (i + 1).min(len);
                    }
                }
                // `#(`, `#u8(`, `#hash(`: a vector-style prefix on a list. A char
                // literal (`#\(`, `#\a`) is never one, even when a bracket
                // follows it directly.
                if c == b'#' && !char_lit && matches!(b.get(i), Some(b'(' | b'[')) {
                    let f = stack.last_mut().unwrap();
                    let start = f.prefix.take().unwrap_or(start);
                    stack.push(Frame::new(start, i));
                    i += 1;
                    continue;
                }
                stack.last_mut().unwrap().push(Node {
                    start,
                    end: i,
                    kind: Kind::Atom,
                    children: Vec::new(),
                });
            }
        }
    }
    // Unterminated lists run to the end of the input.
    while stack.len() > 1 {
        let f = stack.pop().unwrap();
        stack.last_mut().unwrap().push(close_frame(f, None, len));
    }
    let top = stack.pop().unwrap();
    // A dangling prefix with no datum (`'` at EOF) is just an atom.
    let mut nodes = top.children;
    if let Some(p) = top.prefix {
        nodes.push(Node {
            start: p,
            end: len,
            kind: Kind::Atom,
            children: Vec::new(),
        });
    }
    out.nodes = nodes;
    out
}

fn close_frame(f: Frame, close: Option<usize>, len: usize) -> Node {
    Node {
        start: f.start,
        end: close.map_or(len, |c| c + 1),
        kind: Kind::List {
            open: f.open,
            close,
        },
        children: f.children,
    }
}

/// The last complete datum before the end of `text` — what Emacs
/// `eval-last-sexp` evaluates with point at the end of `text`. Descends into a
/// list left open at the end (point inside `(foo (bar 1)|`), so the innermost
/// preceding datum wins. Comments are skipped; `None` when nothing precedes or
/// `text` ends inside a string or an empty open list.
pub fn last_sexp(text: &str) -> Option<Range<usize>> {
    let parsed = parse(text);
    if parsed.ends_in_literal {
        return None;
    }
    let mut nodes = &parsed.nodes;
    loop {
        let last = nodes.last()?;
        if last.is_complete() {
            return Some(last.range());
        }
        nodes = &last.children;
    }
}

/// The complete top-level datum containing `point`, else the last complete one
/// ending before it — the unit `anvil-eval-defun` sends.
pub fn top_level_form_at(text: &str, point: usize) -> Option<Range<usize>> {
    let mut before = None;
    for node in parse(text).nodes.into_iter().filter(Node::is_complete) {
        if node.start <= point && point <= node.end {
            return Some(node.range());
        }
        if node.end <= point {
            before = Some(node.range());
        } else {
            break;
        }
    }
    before
}

/// Where the top-level form still open at the end of `text` starts, or `None`
/// when `text` ends at top level. This is the beginning-of-defun the Lisp
/// indenter scans from.
pub fn open_form_start(text: &str) -> Option<usize> {
    parse(text)
        .nodes
        .last()
        .filter(|n| !n.is_complete())
        .map(|n| n.start)
}

/// The operator (head symbol) of the innermost list still open at the end of
/// `text` — the function whose arguments are being typed, for eldoc.
pub fn enclosing_operator(text: &str) -> Option<String> {
    let parsed = parse(text);
    if parsed.ends_in_literal {
        return None;
    }
    let mut open: Option<&Node> = None;
    let mut nodes = &parsed.nodes;
    while let Some(last) = nodes.last().filter(|n| !n.is_complete()) {
        open = Some(last);
        nodes = &last.children;
    }
    let head = open?.children.first()?;
    // Only a symbol that is already complete: still typing it means it
    // continues to the end of `text`.
    (matches!(head.kind, Kind::Atom) && head.end < text.len())
        .then(|| text[head.start..head.end].to_string())
}

// --- Structural motion and editing (paredit-style) -------------------------

/// A structural edit: replacements in original-text offsets, applied from the
/// end of the text backwards so earlier offsets stay valid, after which point
/// goes to `point` (an offset in the edited text).
#[derive(Debug, PartialEq, Eq)]
pub struct StructEdit {
    pub edits: Vec<(Range<usize>, String)>,
    pub point: usize,
}

// --- Language-neutral reading ---------------------------------------------

/// Lisp languages and major modes, where the Lisp reader above applies. Every
/// other buffer reads with [`parse_generic`].
const LISP_LANGUAGES: &[&str] = &[
    "scheme",
    "racket",
    "clojure",
    "commonlisp",
    "lisp",
    "elisp",
    "emacs-lisp",
    "fennel",
    "janet",
];
const LISP_MODES: &[&str] = &[
    "steel",
    "macros-steel",
    "racket",
    "clojure",
    "lisp",
    "commonlisp",
    "elisp",
    "fennel",
    "janet",
];

/// Whether a buffer with this highlighting language and major mode reads as
/// Lisp (`;` comments, `'` quote prefixes) rather than generic code.
pub fn is_lisp(language: &str, major_mode: &str) -> bool {
    LISP_LANGUAGES.contains(&language) || LISP_MODES.contains(&major_mode)
}

/// A span [`parse_generic`] reads as one unit: a string literal (one datum) or
/// a comment (skipped). Taken from the buffer's syntax tree when it has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Literal {
    pub start: usize,
    pub end: usize,
    pub comment: bool,
}

/// Read a buffer the way its language wants: the Lisp reader for Lisp, else
/// [`parse_generic`] with the string and comment spans of `tree` (if any).
pub fn parse_buffer(
    text: &str,
    language: &str,
    major_mode: &str,
    tree: Option<&tree_sitter::Tree>,
) -> Parsed {
    if is_lisp(language, major_mode) {
        return parse(text);
    }
    let literals = tree.map(crate::highlight::tree_literal_spans);
    parse_generic(text, literals.as_deref())
}

/// Identifier bytes for the generic reader. Every non-ASCII byte counts, so a
/// run never splits a UTF-8 character.
fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

/// A C-style char literal at `i` (`'a'`, `'\n'`, `'\u{1F600}'`): its end, or
/// `None` when the quote is something else (a Rust lifetime, an apostrophe).
fn char_literal_end(text: &str, i: usize) -> Option<usize> {
    let rest = text.get(i + 1..)?;
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    if first == '\\' {
        // An escape: up to the closing quote, on this line and nearby.
        let close = rest
            .char_indices()
            .skip(2)
            .take(12)
            .find(|&(_, c)| c == '\'' || c == '\n')?;
        (close.1 == '\'').then_some(i + 1 + close.0 + 1)
    } else {
        let (j, c) = chars.next()?;
        (c == '\'' && first != '\'' && first != '\n').then_some(i + 1 + j + 1)
    }
}

/// End of the double-quoted string starting at `i` (escapes honored), and
/// whether it closed before the input ended.
fn string_end(b: &[u8], i: usize) -> (usize, bool) {
    let mut i = i + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return (i + 1, true),
            _ => i += 1,
        }
    }
    (b.len(), false)
}

/// Read non-Lisp text into bracket-balanced data, the Emacs `forward-sexp`
/// units of any language: `(…)`, `[…]` and `{…}` groups, identifiers and
/// numbers, and string literals. Operators and other punctuation are not data
/// of their own (motion skips them, as Emacs does). With `literals` (the
/// syntax tree's strings and comments) those spans are exact; without, `"…"`
/// strings, `'c'` char literals and `//` / `/* */` comments are recognised.
pub fn parse_generic(text: &str, literals: Option<&[Literal]>) -> Parsed {
    let b = text.as_bytes();
    let len = b.len();
    let heuristic = literals.is_none();
    let mut spans = literals.unwrap_or(&[]).iter().peekable();
    let mut out = Parsed::default();
    let mut stack = vec![Frame::new(0, 0)];
    let mut i = 0;
    while i < len {
        // Drop spans already behind us (or that a stale tree misplaced).
        while spans.peek().is_some_and(|s| s.start < i) {
            spans.next();
        }
        if let Some(&&lit) = spans.peek().filter(|s| s.start == i) {
            spans.next();
            let end = lit.end.clamp(i + 1, len);
            if !lit.comment {
                stack.last_mut().unwrap().push(Node {
                    start: i,
                    end,
                    kind: Kind::Str { closed: true },
                    children: Vec::new(),
                });
            }
            i = end;
            continue;
        }
        let c = b[i];
        match c {
            _ if c.is_ascii_whitespace() => i += 1,
            b'/' if heuristic && b.get(i + 1) == Some(&b'/') => {
                while i < len && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if heuristic && b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < len && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(len);
            }
            b'"' if heuristic => {
                let (end, closed) = string_end(b, i);
                out.ends_in_literal = !closed;
                stack.last_mut().unwrap().push(Node {
                    start: i,
                    end,
                    kind: Kind::Str { closed },
                    children: Vec::new(),
                });
                i = end;
            }
            b'\'' if heuristic && char_literal_end(text, i).is_some() => {
                let end = char_literal_end(text, i).unwrap_or(i + 1);
                stack.last_mut().unwrap().push(Node {
                    start: i,
                    end,
                    kind: Kind::Str { closed: true },
                    children: Vec::new(),
                });
                i = end;
            }
            b'(' | b'[' | b'{' => {
                stack.push(Frame::new(i, i));
                i += 1;
            }
            b')' | b']' | b'}' => {
                let matches =
                    stack.len() > 1 && closer_for(b[stack.last().unwrap().open]) == Some(c);
                if matches {
                    let f = stack.pop().unwrap();
                    stack.last_mut().unwrap().push(close_frame(f, Some(i), len));
                } else {
                    out.stray_closers += 1;
                }
                i += 1;
            }
            _ if is_word_byte(c) => {
                let start = i;
                while i < len && is_word_byte(b[i]) {
                    i += 1;
                }
                stack.last_mut().unwrap().push(Node {
                    start,
                    end: i,
                    kind: Kind::Atom,
                    children: Vec::new(),
                });
            }
            _ => i += 1,
        }
    }
    while stack.len() > 1 {
        let f = stack.pop().unwrap();
        stack.last_mut().unwrap().push(close_frame(f, None, len));
    }
    out.nodes = stack.pop().unwrap().children;
    out
}

/// The innermost list whose brackets strictly enclose `pos`, and the data `pos`
/// moves among: that list's children, or the top level.
fn container(parsed: &Parsed, pos: usize) -> (Option<&Node>, &[Node]) {
    let mut list: Option<&Node> = None;
    let mut kids: &[Node] = &parsed.nodes;
    'descend: loop {
        for node in kids {
            if let Kind::List { open, close } = node.kind
                && open < pos
                && close.is_none_or(|c| pos <= c)
            {
                list = Some(node);
                kids = &node.children;
                continue 'descend;
            }
        }
        return (list, kids);
    }
}

/// The datum at or after `pos` among its siblings: the one `pos` is inside or
/// in front of.
fn sexp_at_or_after(kids: &[Node], pos: usize) -> Option<&Node> {
    kids.iter().find(|k| k.end > pos)
}

/// Emacs `forward-sexp`: the end of the datum at or after `pos`. `None` at the
/// end of the enclosing list.
#[cfg(test)]
pub fn forward_sexp(text: &str, pos: usize) -> Option<usize> {
    forward_sexp_in(&parse(text), pos)
}

/// [`forward_sexp`] over an already-read text.
pub fn forward_sexp_in(parsed: &Parsed, pos: usize) -> Option<usize> {
    sexp_at_or_after(container(parsed, pos).1, pos).map(|k| k.end)
}

/// Emacs `backward-sexp`: the start of the datum before (or around) `pos`.
#[cfg(test)]
pub fn backward_sexp(text: &str, pos: usize) -> Option<usize> {
    backward_sexp_in(&parse(text), pos)
}

/// [`backward_sexp`] over an already-read text.
pub fn backward_sexp_in(parsed: &Parsed, pos: usize) -> Option<usize> {
    container(parsed, pos)
        .1
        .iter()
        .rev()
        .find(|k| k.start < pos)
        .map(|k| k.start)
}

/// Emacs `backward-up-list`: the start of the enclosing list.
#[cfg(test)]
pub fn backward_up_list(text: &str, pos: usize) -> Option<usize> {
    backward_up_list_in(&parse(text), pos)
}

/// [`backward_up_list`] over an already-read text.
pub fn backward_up_list_in(parsed: &Parsed, pos: usize) -> Option<usize> {
    container(parsed, pos).0.map(|l| l.start)
}

/// Emacs `up-list`: just past the enclosing list's closing bracket.
#[cfg(test)]
pub fn up_list(text: &str, pos: usize) -> Option<usize> {
    up_list_in(&parse(text), pos)
}

/// [`up_list`] over an already-read text.
pub fn up_list_in(parsed: &Parsed, pos: usize) -> Option<usize> {
    container(parsed, pos)
        .0
        .filter(|l| l.is_complete())
        .map(|l| l.end)
}

/// Emacs `down-list`: just inside the next list at or after `pos`.
#[cfg(test)]
pub fn down_list(text: &str, pos: usize) -> Option<usize> {
    down_list_in(&parse(text), pos)
}

/// [`down_list`] over an already-read text.
pub fn down_list_in(parsed: &Parsed, pos: usize) -> Option<usize> {
    container(parsed, pos)
        .1
        .iter()
        .filter(|k| k.end > pos)
        .find_map(|k| match k.kind {
            Kind::List { open, .. } if open >= pos || k.start >= pos => Some(open + 1),
            _ => None,
        })
}

/// Emacs `forward-list`: past the next list at this level, skipping atoms.
pub fn forward_list_in(parsed: &Parsed, pos: usize) -> Option<usize> {
    container(parsed, pos)
        .1
        .iter()
        .find(|k| k.start >= pos && matches!(k.kind, Kind::List { .. }))
        .map(|k| k.end)
}

/// Emacs `backward-list`: to the start of the previous list at this level.
pub fn backward_list_in(parsed: &Parsed, pos: usize) -> Option<usize> {
    container(parsed, pos)
        .1
        .iter()
        .rev()
        .find(|k| k.end <= pos && matches!(k.kind, Kind::List { .. }))
        .map(|k| k.start)
}

/// The `(start, end)` of every top-level datum — Lisp's defuns.
pub fn top_level_ranges(parsed: &Parsed) -> Vec<(usize, usize)> {
    parsed.nodes.iter().map(|n| (n.start, n.end)).collect()
}

/// The enclosing list of `pos` when it is properly closed: (list, open, close).
fn closed_container(parsed: &Parsed, pos: usize) -> Option<(&Node, usize, usize)> {
    let list = container(parsed, pos).0?;
    match list.kind {
        Kind::List {
            open,
            close: Some(close),
        } => Some((list, open, close)),
        _ => None,
    }
}

/// paredit `forward-slurp-sexp`: pull the datum after the enclosing list into
/// it — `(a |b) c` → `(a |b c)`.
pub fn slurp_forward(text: &str, pos: usize) -> Option<StructEdit> {
    let parsed = parse(text);
    let (list, _, close) = closed_container(&parsed, pos)?;
    let next = container(&parsed, list.start)
        .1
        .iter()
        .find(|k| k.start >= list.end)?;
    let closer = text[close..close + 1].to_string();
    Some(StructEdit {
        edits: vec![
            (next.end..next.end, closer),
            (close..close + 1, String::new()),
        ],
        point: pos,
    })
}

/// paredit `forward-barf-sexp`: push the enclosing list's last datum out —
/// `(a |b c)` → `(a |b) c`.
pub fn barf_forward(text: &str, pos: usize) -> Option<StructEdit> {
    let parsed = parse(text);
    let (list, open, close) = closed_container(&parsed, pos)?;
    let kids = &list.children;
    let at = match kids.len() {
        0 => return None,
        1 => open + 1,
        n => kids[n - 2].end,
    };
    let closer = text[close..close + 1].to_string();
    Some(StructEdit {
        edits: vec![(close..close + 1, String::new()), (at..at, closer)],
        point: if pos > at { pos + 1 } else { pos },
    })
}

/// paredit `raise-sexp`: replace the enclosing list with the datum at point —
/// `(foo (bar |baz))` → `(foo |baz)`.
pub fn raise_sexp(text: &str, pos: usize) -> Option<StructEdit> {
    let parsed = parse(text);
    let (list, ..) = closed_container(&parsed, pos)?;
    let sexp = sexp_at_or_after(&list.children, pos)?;
    Some(StructEdit {
        edits: vec![(list.range(), text[sexp.range()].to_string())],
        point: list.start,
    })
}

/// paredit `splice-sexp`: remove the enclosing list's brackets (and any quote
/// prefix) — `(a (b |c) d)` → `(a b |c d)`.
pub fn splice_sexp(text: &str, pos: usize) -> Option<StructEdit> {
    let parsed = parse(text);
    let (list, open, close) = closed_container(&parsed, pos)?;
    Some(StructEdit {
        edits: vec![
            (close..close + 1, String::new()),
            (list.start..open + 1, String::new()),
        ],
        point: pos - (open + 1 - list.start),
    })
}

/// paredit `wrap-round`: wrap the datum at point in a new list, point just
/// inside it — `(a |b)` → `(a (|b))`. With nothing to wrap, insert `()`.
pub fn wrap_round(text: &str, pos: usize) -> StructEdit {
    let parsed = parse(text);
    match sexp_at_or_after(container(&parsed, pos).1, pos) {
        Some(sexp) => StructEdit {
            edits: vec![
                (sexp.end..sexp.end, ")".into()),
                (sexp.start..sexp.start, "(".into()),
            ],
            point: sexp.start + 1,
        },
        None => StructEdit {
            edits: vec![(pos..pos, "()".into())],
            point: pos + 1,
        },
    }
}

/// Apply a [`StructEdit`] to a string (tests, and a model of what the editor
/// does with its buffer).
#[cfg(test)]
pub fn apply(text: &str, edit: &StructEdit) -> String {
    let mut edits: Vec<_> = edit.edits.iter().collect();
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = text.to_string();
    for (range, with) in edits {
        out.replace_range(range.clone(), with);
    }
    out
}

/// The bracket matching the one at byte `pos` (an opener's closer, or a
/// closer's opener), honoring strings, comments and char literals.
pub fn matching_bracket(text: &str, pos: usize) -> Option<usize> {
    let parsed = parse(text);
    let mut stack: Vec<&Node> = parsed.nodes.iter().collect();
    while let Some(node) = stack.pop() {
        if !(node.start <= pos && pos < node.end) {
            continue;
        }
        if let Kind::List { open, close } = node.kind {
            if open == pos {
                return close;
            }
            if close == Some(pos) {
                return Some(open);
            }
        }
        stack.extend(node.children.iter());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last(s: &str) -> Option<&str> {
        last_sexp(s).map(|r| &s[r])
    }

    #[test]
    fn last_sexp_basic_forms() {
        assert_eq!(last("(foo 1)\n(bar 2)"), Some("(bar 2)"));
        assert_eq!(last("(a)\n  "), Some("(a)"));
        assert_eq!(last("foo bar"), Some("bar"));
        assert_eq!(last("(+ (* 2 3) 4)"), Some("(+ (* 2 3) 4)"));
        assert_eq!(last("   "), None);
    }

    #[test]
    fn last_sexp_honors_the_reader() {
        // Quote and quasiquote prefixes belong to the datum.
        assert_eq!(last("'(1 2 3)"), Some("'(1 2 3)"));
        assert_eq!(last("`(a ,b ,@c)"), Some("`(a ,b ,@c)"));
        assert_eq!(last("'sym"), Some("'sym"));
        // Vectors.
        assert_eq!(last("#(1 2)"), Some("#(1 2)"));
        assert_eq!(last("#u8(1 2)"), Some("#u8(1 2)"));
        // A paren inside a string is not a delimiter.
        assert_eq!(last("(display \"a)\")"), Some("(display \"a)\")"));
        // Char literals.
        assert_eq!(last("(list #\\()"), Some("(list #\\()"));
        assert_eq!(last("(list #\\))"), Some("(list #\\))"));
        assert_eq!(last("#\\space"), Some("#\\space"));
        // A char literal directly followed by a list is two data, not a
        // `#(`-style vector prefix.
        assert_eq!(last("x #\\((b)"), Some("(b)"));
        assert_eq!(last("x #\\a(b)"), Some("(b)"));
        // A trailing comment is skipped.
        assert_eq!(last("(foo 1) ; done"), Some("(foo 1)"));
        assert_eq!(last("(foo 1) #| (x |#"), Some("(foo 1)"));
        // A string is one datum.
        assert_eq!(last("\"hello world\""), Some("\"hello world\""));
        // Square brackets.
        assert_eq!(last("[1 2]"), Some("[1 2]"));
        assert_eq!(last("(let ([x 1]) x)"), Some("(let ([x 1]) x)"));
        // A comment containing a paren inside the form.
        assert_eq!(last("(f ; )\n 1)"), Some("(f ; )\n 1)"));
        // `#;` comments out the next datum.
        assert_eq!(last("(a) #;(b)"), Some("(a)"));
        // Point inside an open list: the innermost preceding datum.
        assert_eq!(last("(foo (bar 1)"), Some("(bar 1)"));
        // Ending inside a string: nothing sensible to evaluate.
        assert_eq!(last("(f \"abc"), None);
    }

    #[test]
    fn top_level_form_at_respects_strings() {
        let src = "(define a 1)\n(define s \")\")\n(define b 2)";
        let inside = src.find("s \"").unwrap();
        assert_eq!(
            top_level_form_at(src, inside).map(|r| &src[r]),
            Some("(define s \")\")")
        );
        let src = "(define a 1)\n(define (f x)\n  (+ x 1))\n(define b 2)";
        let inside = src.find("+ x").unwrap();
        assert_eq!(
            top_level_form_at(src, inside).map(|r| &src[r]),
            Some("(define (f x)\n  (+ x 1))")
        );
        assert_eq!(
            top_level_form_at(src, 2).map(|r| &src[r]),
            Some("(define a 1)")
        );
        let gap = src.find("\n(define b").unwrap();
        assert_eq!(
            top_level_form_at(src, gap).map(|r| &src[r]),
            Some("(define (f x)\n  (+ x 1))")
        );
        // Point inside a comment between forms: the form before.
        let src = "(a)\n; (b)\n(c)";
        assert_eq!(top_level_form_at(src, 6).map(|r| &src[r]), Some("(a)"));
    }

    #[test]
    fn literals_and_open_forms() {
        let src = "(define (f)\n  \"doc\n(f) returns 1\")\n";
        let line = src.find("(f) returns").unwrap();
        assert!(
            parse(&src[..line]).ends_in_literal,
            "a docstring line is literal content"
        );
        let doc = src.find("  \"doc").unwrap();
        assert!(!parse(&src[..doc]).ends_in_literal);
        assert_eq!(open_form_start("(a)\n(define (f)\n  (g"), Some(4));
        assert_eq!(open_form_start("(a)\n"), None);
        assert!(parse("(f \"abc").ends_in_literal);
        assert!(parse("#| x").ends_in_literal);
        assert!(!parse("(f \"abc\")").ends_in_literal);
    }

    #[test]
    fn mismatched_closers_do_not_pair() {
        assert_eq!(matching_bracket("(a]", 0), None);
        assert_eq!(parse("(a]").stray_closers, 1);
        assert_eq!(matching_bracket("[a)]", 0), Some(3));
    }

    #[test]
    fn matching_bracket_skips_literals() {
        let src = "(a \")\" #\\( [b])";
        assert_eq!(matching_bracket(src, 0), Some(src.len() - 1));
        assert_eq!(matching_bracket(src, src.len() - 1), Some(0));
        let sq = src.find('[').unwrap();
        assert_eq!(matching_bracket(src, sq), Some(sq + 2));
        // The paren in the string matches nothing.
        assert_eq!(matching_bracket(src, 4), None);
    }

    #[test]
    fn enclosing_operator_is_the_open_head() {
        assert_eq!(
            enclosing_operator("(string-append \"a\" ").as_deref(),
            Some("string-append")
        );
        assert_eq!(
            enclosing_operator("(map (lambda (x) x) ").as_deref(),
            Some("map")
        );
        assert_eq!(enclosing_operator("(map (car ").as_deref(), Some("car"));
        // Still typing the head itself.
        assert_eq!(enclosing_operator("(string-app"), None);
        assert_eq!(enclosing_operator("(f 1)"), None);
        assert_eq!(enclosing_operator("(f \"open"), None);
    }

    /// `|` in `src` marks point; returns the text and point after `op`.
    fn run(src: &str, op: impl Fn(&str, usize) -> Option<StructEdit>) -> Option<String> {
        let pos = src.find('|').unwrap();
        let text = src.replace('|', "");
        let edit = op(&text, pos)?;
        let mut out = apply(&text, &edit);
        out.insert(edit.point, '|');
        Some(out)
    }

    fn motion(src: &str, op: impl Fn(&str, usize) -> Option<usize>) -> Option<String> {
        let pos = src.find('|').unwrap();
        let mut text = src.replace('|', "");
        let to = op(&text, pos)?;
        text.insert(to, '|');
        Some(text)
    }

    fn generic(op: fn(&Parsed, usize) -> Option<usize>) -> impl Fn(&str, usize) -> Option<usize> {
        move |text, pos| op(&parse_generic(text, None), pos)
    }

    #[test]
    fn generic_reader_motions() {
        let fwd = generic(forward_sexp_in);
        // Punctuation is skipped; `;` and `'` aren't Lisp syntax here.
        assert_eq!(
            motion("|x = f(a); y", &fwd).as_deref(),
            Some("x| = f(a); y")
        );
        assert_eq!(
            motion("x |= f(a); y", &fwd).as_deref(),
            Some("x = f|(a); y")
        );
        assert_eq!(motion("f|(a); y", &fwd).as_deref(), Some("f(a)|; y"));
        assert_eq!(motion("f(a)|; y", &fwd).as_deref(), Some("f(a); y|"));
        // Strings, char literals and comments hide their brackets.
        assert_eq!(
            motion("|(\")\" ')' /* ) */ // )\n)", &fwd).as_deref(),
            Some("(\")\" ')' /* ) */ // )\n)|")
        );
        // A lifetime is not a char literal.
        assert_eq!(motion("|<'a>(x)", &fwd).as_deref(), Some("<'a|>(x)"));
        assert_eq!(
            motion("{ a |b }", generic(up_list_in)).as_deref(),
            Some("{ a b }|")
        );
        assert_eq!(
            motion("|x [a] y (b)", generic(forward_list_in)).as_deref(),
            Some("x [a]| y (b)")
        );
        assert_eq!(
            motion("x [a] y (b)|", generic(backward_list_in)).as_deref(),
            Some("x [a] y |(b)")
        );
        // Literal spans from a syntax tree win over the heuristics.
        let text = "f(# ) \n)";
        let lits = [Literal {
            start: 2,
            end: 5,
            comment: true,
        }];
        assert_eq!(
            forward_sexp_in(&parse_generic(text, Some(&lits)), 1),
            Some(8)
        );
        assert!(is_lisp("scheme", "") && is_lisp("", "steel") && !is_lisp("rust", "rust"));
    }

    #[test]
    fn sexp_motions() {
        assert_eq!(
            motion("|(a b) c", forward_sexp).as_deref(),
            Some("(a b)| c")
        );
        assert_eq!(
            motion("(a |\"b)\" c)", forward_sexp).as_deref(),
            Some("(a \"b)\"| c)")
        );
        assert_eq!(motion("(a b|)", forward_sexp), None);
        assert_eq!(
            motion("(a b) c|", backward_sexp).as_deref(),
            Some("(a b) |c")
        );
        assert_eq!(
            motion("x '(a b)|", backward_sexp).as_deref(),
            Some("x |'(a b)")
        );
        assert_eq!(
            motion("(a (b |c))", backward_up_list).as_deref(),
            Some("(a |(b c))")
        );
        assert_eq!(motion("(a (b |c))", up_list).as_deref(), Some("(a (b c)|)"));
        assert_eq!(motion("|x (a b)", down_list).as_deref(), Some("x (|a b)"));
        // `#\)` doesn't end the list.
        assert_eq!(
            motion("(a #\\) |b)", up_list).as_deref(),
            Some("(a #\\) b)|")
        );
    }

    #[test]
    fn paredit_edits() {
        assert_eq!(run("(a |b) c", slurp_forward).as_deref(), Some("(a |b c)"));
        assert_eq!(
            run("[a |b] (c d)", slurp_forward).as_deref(),
            Some("[a |b (c d)]")
        );
        assert_eq!(run("(a |b) ", slurp_forward), None);
        assert_eq!(run("(a |b c)", barf_forward).as_deref(), Some("(a |b) c"));
        assert_eq!(run("(a b |c)", barf_forward).as_deref(), Some("(a b) |c"));
        assert_eq!(
            run("(foo (bar |baz))", raise_sexp).as_deref(),
            Some("(foo |baz)")
        );
        assert_eq!(
            run("(a (b |c) d)", splice_sexp).as_deref(),
            Some("(a b |c d)")
        );
        assert_eq!(
            run("(a '(b |c) d)", splice_sexp).as_deref(),
            Some("(a b |c d)")
        );
        assert_eq!(
            run("(a |b)", |t, p| Some(wrap_round(t, p))).as_deref(),
            Some("(a (|b))")
        );
        assert_eq!(
            run("(a |)", |t, p| Some(wrap_round(t, p))).as_deref(),
            Some("(a (|))")
        );
    }

    #[test]
    fn deep_nesting_does_not_recurse() {
        let deep = "(".repeat(100_000) + &")".repeat(100_000);
        assert_eq!(last_sexp(&deep), Some(0..deep.len()));
    }
}
