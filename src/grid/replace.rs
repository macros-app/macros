//! Find & replace over cell text: what `grid-replace`, `grid-query-replace`
//! and the grid's `:s` compute for one cell.
//!
//! A cell is matched by the text Scheme reads and typing recreates (its
//! "input text"): a formula by its source (`=SUM(A1:A3)`), like Excel's
//! Replace with "Look in: Formulas". Every match in the cell is replaced
//! (`:s` without `g`: only the first).

use regex::{Captures, Regex};

/// How the pattern and the replacement are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `grid-replace` / `grid-query-replace`: FROM is literal text and TO is
    /// inserted as is.
    Literal,
    /// `grid-replace-regexp` / `grid-query-replace-regexp`: FROM is a regexp
    /// and TO uses Emacs's syntax (`\&` the match, `\1`…`\9` groups, `\\`).
    Regexp,
    /// The grid's `:s` (already translated from Vim's dialect): a regex-crate
    /// pattern and `${1}` expansion, no smart case; `first` replaces only
    /// the first match per cell (`:s` without `g`).
    Raw { first: bool },
}

impl Mode {
    /// The mode named by Scheme: `literal`, `regexp`, `raw` or `raw-first`.
    pub fn parse(name: &str) -> Option<Mode> {
        match name {
            "literal" => Some(Mode::Literal),
            "regexp" => Some(Mode::Regexp),
            "raw" => Some(Mode::Raw { first: false }),
            "raw-first" => Some(Mode::Raw { first: true }),
            _ => None,
        }
    }
}

/// A compiled FROM → TO.
#[derive(Debug)]
pub struct Replacer {
    re: Regex,
    to: String,
    mode: Mode,
    /// Emacs's `case-replace`: give each replacement the match's case.
    case_adjust: bool,
}

impl Replacer {
    /// Compile `from` → `to`. Under `Literal` and `Regexp` case follows
    /// Emacs's defaults: a FROM without capitals matches any case, and a TO
    /// without capitals then copies the case of each match (FOO → BAR,
    /// Foo → Bar). `Err` is a message to show (empty FROM, a bad regexp).
    pub fn new(from: &str, to: &str, mode: Mode) -> Result<Replacer, String> {
        if from.is_empty() {
            return Err("Nothing to replace".into());
        }
        let fold = !matches!(mode, Mode::Raw { .. }) && !has_upper(from);
        let base = match mode {
            Mode::Literal => regex::escape(from),
            _ => from.to_string(),
        };
        let pattern = if fold { format!("(?i){base}") } else { base };
        let re = Regex::new(&pattern).map_err(|e| format!("Invalid regexp: {e}"))?;
        Ok(Replacer {
            re,
            to: to.to_string(),
            mode,
            case_adjust: fold && !has_upper(to),
        })
    }

    /// `text` with the replacements made, or `None` when nothing matched or
    /// replacing changes nothing.
    pub fn apply(&self, text: &str) -> Option<String> {
        if !self.re.is_match(text) {
            return None;
        }
        let out = match self.mode {
            Mode::Raw { first: true } => self.re.replace(text, self.to.as_str()),
            Mode::Raw { first: false } => self.re.replace_all(text, self.to.as_str()),
            Mode::Literal | Mode::Regexp => self.re.replace_all(text, |caps: &Captures| {
                let rep = match self.mode {
                    Mode::Regexp => expand_emacs(&self.to, caps),
                    _ => self.to.clone(),
                };
                if self.case_adjust {
                    case_adjust(&rep, &caps[0])
                } else {
                    rep
                }
            }),
        };
        (out != text).then(|| out.into_owned())
    }
}

fn has_upper(s: &str) -> bool {
    s.chars().any(char::is_uppercase)
}

/// Expand Emacs replacement syntax: `\&` the whole match, `\N` group N (an
/// unmatched or missing group is ""), `\\` a backslash, `\n` / `\t` a
/// newline / tab. Any other `\x` stays as written.
fn expand_emacs(rep: &str, caps: &Captures) -> String {
    let mut out = String::with_capacity(rep.len());
    let mut chars = rep.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('&') => out.push_str(&caps[0]),
            Some(d @ '0'..='9') => {
                let n = d as usize - '0' as usize;
                out.push_str(caps.get(n).map_or("", |m| m.as_str()));
            }
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Give `rep` the case pattern of `matched` (Emacs's `case-replace`): all
/// capitals (two or more cased letters) upcases it, an initial capital
/// capitalizes it, anything else leaves it alone.
fn case_adjust(rep: &str, matched: &str) -> String {
    let cased: Vec<char> = matched
        .chars()
        .filter(|c| c.is_uppercase() || c.is_lowercase())
        .collect();
    if cased.len() > 1 && cased.iter().all(|c| c.is_uppercase()) {
        return rep.to_uppercase();
    }
    match matched.chars().next() {
        Some(first) if first.is_uppercase() => {
            let mut cs = rep.chars();
            match cs.next() {
                Some(f) => f.to_uppercase().chain(cs).collect(),
                None => String::new(),
            }
        }
        _ => rep.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(from: &str, to: &str, mode: Mode, text: &str) -> Option<String> {
        Replacer::new(from, to, mode).unwrap().apply(text)
    }

    #[test]
    fn literal_replaces_every_match_and_quotes_metacharacters() {
        assert_eq!(
            rep("a.b", "X", Mode::Literal, "a.b axb a.b"),
            Some("X axb X".into())
        );
        assert_eq!(rep("zz", "X", Mode::Literal, "abc"), None);
        // A literal TO: backslashes are not special.
        assert_eq!(rep("a", r"\1", Mode::Literal, "a"), Some(r"\1".into()));
    }

    #[test]
    fn smart_case_folds_and_copies_case() {
        assert_eq!(
            rep("apple", "pear", Mode::Literal, "apple Apple APPLE"),
            Some("pear Pear PEAR".into())
        );
        // A capital in FROM: exact case only.
        assert_eq!(
            rep("Apple", "pear", Mode::Literal, "apple Apple"),
            Some("apple pear".into())
        );
        // A capital in TO: inserted as typed.
        assert_eq!(
            rep("apple", "Pear", Mode::Literal, "APPLE"),
            Some("Pear".into())
        );
    }

    #[test]
    fn regexp_expands_groups_and_the_whole_match() {
        assert_eq!(
            rep(r"(\w+)@(\w+)", r"\2 at \1 (\&)", Mode::Regexp, "bob@home"),
            Some("home at bob (bob@home)".into())
        );
        assert_eq!(
            rep(r"(x)?y", r"[\1]", Mode::Regexp, "y"),
            Some("[]".into()),
            "an unmatched group is empty"
        );
        assert_eq!(rep("a", r"\\", Mode::Regexp, "a"), Some("\\".into()));
    }

    #[test]
    fn raw_mode_is_vims_translated_substitute() {
        let first = Mode::Raw { first: true };
        let all = Mode::Raw { first: false };
        assert_eq!(rep("o", "0", first, "foo"), Some("f0o".into()));
        assert_eq!(rep("o", "0", all, "foo"), Some("f00".into()));
        assert_eq!(rep("(a)(b)", "${2}${1}", all, "ab"), Some("ba".into()));
        // No smart case.
        assert_eq!(rep("a", "b", all, "A"), None);
    }

    #[test]
    fn formulas_match_by_their_source() {
        assert_eq!(
            rep("A1", "B1", Mode::Literal, "=SUM(A1:A3)*A1"),
            Some("=SUM(B1:A3)*B1".into())
        );
    }

    #[test]
    fn errors_and_no_ops() {
        assert!(Replacer::new("", "x", Mode::Literal).is_err());
        assert!(Replacer::new("(", "x", Mode::Regexp).is_err());
        assert_eq!(rep("a", "a", Mode::Literal, "a"), None, "unchanged");
    }
}
