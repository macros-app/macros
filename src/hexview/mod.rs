//! Hex buffer model: a file's bytes shown and edited as hex, with no UI.
//!
//! - [`model`]: [`HexData`], the bytes plus an undo/redo history and
//!   saved-state tracking. Saving an unedited buffer writes back the exact
//!   bytes it was read from.
//! - [`view`]: [`HexView`], the cursor, selection, layout and scroll state.
//! - [`inspect`]: the data inspector (the bytes at point as integers, floats,
//!   bits and UTF-8).
//! - [`cmd`]: the command vocabulary `scheme/hex.scm` binds.
//!
//! This module also holds the small helpers the editor and the element share:
//! how a byte is classed for colouring, search patterns, and the copy formats.

pub mod cmd;
pub mod inspect;
pub mod model;
pub mod view;

pub use model::HexData;
pub use view::HexView;

/// What kind of byte this is, for colouring the dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteClass {
    /// `00`.
    Null,
    /// Printable ASCII (`0x21..=0x7e`).
    Printable,
    /// Space, tab, newline, carriage return, vertical tab, form feed.
    Whitespace,
    /// The other C0 controls and DEL.
    Control,
    /// Bytes with the high bit set (`0x80..=0xff`).
    High,
}

impl ByteClass {
    pub fn of(b: u8) -> Self {
        match b {
            0 => Self::Null,
            b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c => Self::Whitespace,
            0x21..=0x7e => Self::Printable,
            0x80..=0xff => Self::High,
            _ => Self::Control,
        }
    }
}

/// The character the ASCII column shows for `b`: itself when printable,
/// else `.`.
pub fn ascii_char(b: u8) -> char {
    if (0x20..=0x7e).contains(&b) {
        b as char
    } else {
        '.'
    }
}

/// Whether `bytes` look like a binary file rather than text: not valid
/// UTF-8, or a NUL byte in the first 8 KiB (valid UTF-8, but no text editor
/// wants it).
pub fn looks_binary(bytes: &[u8]) -> bool {
    has_leading_nul(bytes) || std::str::from_utf8(bytes).is_err()
}

/// A NUL byte in the first 8 KiB: the binary test for bytes already known
/// to be UTF-8.
pub fn has_leading_nul(bytes: &[u8]) -> bool {
    memchr::memchr(0, &bytes[..bytes.len().min(8192)]).is_some()
}

/// A search pattern: bytes to match, `None` for a `??` wildcard.
pub type Pattern = Vec<Option<u8>>;

/// Parse what the user typed at a hex search or fill prompt.
///
/// - `"text"` (quoted) is always those bytes as UTF-8; the closing quote is
///   optional.
/// - Otherwise, hex digits with optional spaces, a leading `0x`, and `??`
///   wildcards: `de ad ?? ef`, `deadbeef`, `0x7f454c46`. An odd number of
///   digits (`abc`) is an error: quote it to search for the text.
/// - Anything containing a non-hex character is searched for as text.
pub fn parse_pattern(input: &str) -> Result<Pattern, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty pattern".into());
    }
    if let Some(rest) = s.strip_prefix('"') {
        let text = rest.strip_suffix('"').unwrap_or(rest);
        if text.is_empty() {
            return Err("empty pattern".into());
        }
        return Ok(text.bytes().map(Some).collect());
    }
    match parse_hex_tokens(s) {
        Some(Ok(p)) => Ok(p),
        Some(Err(e)) => Err(e),
        None => Ok(s.bytes().map(Some).collect()),
    }
}

/// `Some` when `s` reads as hex (digits, spaces, `0x` prefixes, `??`):
/// `Ok` with the pattern, or `Err` when it's hex-shaped but malformed (an odd
/// number of digits). `None` when it isn't hex at all.
fn parse_hex_tokens(s: &str) -> Option<Result<Pattern, String>> {
    let mut digits = String::new();
    for word in s.split_whitespace() {
        let word = word
            .strip_prefix("0x")
            .or_else(|| word.strip_prefix("0X"))
            .unwrap_or(word);
        if !word.chars().all(|c| c.is_ascii_hexdigit() || c == '?') {
            return None;
        }
        digits.push_str(word);
    }
    if digits.is_empty() {
        return None;
    }
    if !digits.len().is_multiple_of(2) {
        return Some(Err(format!(
            "{} hex digits: give two per byte",
            digits.len()
        )));
    }
    let chars: Vec<char> = digits.chars().collect();
    let mut out = Vec::with_capacity(chars.len() / 2);
    for pair in chars.chunks(2) {
        match (pair[0], pair[1]) {
            ('?', '?') => out.push(None),
            (a, b) if a != '?' && b != '?' => {
                let v = (a.to_digit(16).unwrap() << 4) | b.to_digit(16).unwrap();
                out.push(Some(v as u8));
            }
            _ => return Some(Err("a wildcard is a whole byte: write ??".into())),
        }
    }
    Some(Ok(out))
}

/// Concrete bytes from a pattern; `Err` if it holds a wildcard.
pub fn pattern_bytes(p: &Pattern) -> Result<Vec<u8>, String> {
    p.iter()
        .map(|b| b.ok_or_else(|| "?? can't be written, only searched for".to_string()))
        .collect()
}

/// Bytes a paste writes: hex when the clipboard text reads as whole hex
/// bytes (`de ad be ef`, `12`), else the text's UTF-8 bytes (`100`, `hi`).
pub fn paste_bytes(text: &str) -> Vec<u8> {
    match parse_hex_tokens(text.trim()) {
        Some(Ok(p)) if p.iter().all(Option::is_some) => p.into_iter().flatten().collect(),
        _ => text.as_bytes().to_vec(),
    }
}

fn matches_at(hay: &[u8], at: usize, pat: &Pattern) -> bool {
    hay.len() - at >= pat.len()
        && pat
            .iter()
            .zip(&hay[at..])
            .all(|(p, &b)| p.is_none_or(|p| p == b))
}

/// The next match of `pat` in `hay` starting at or after `from` (forward)
/// or strictly before `from` (backward), wrapping around the ends. Returns
/// the match's offset and whether the search wrapped.
pub fn find(hay: &[u8], pat: &Pattern, from: usize, forward: bool) -> Option<(usize, bool)> {
    if pat.is_empty() || pat.len() > hay.len() {
        return None;
    }
    let last = hay.len() - pat.len();
    let from = from.min(hay.len());
    let exact: Option<Vec<u8>> = pat.iter().copied().collect();
    if forward {
        let scan = |lo: usize, hi: usize| -> Option<usize> {
            // `lo..=hi` are candidate starts.
            if lo > hi {
                return None;
            }
            match &exact {
                Some(needle) => {
                    memchr::memmem::find(&hay[lo..hi + needle.len()], needle).map(|i| i + lo)
                }
                None => (lo..=hi).find(|&i| matches_at(hay, i, pat)),
            }
        };
        if from <= last
            && let Some(i) = scan(from, last)
        {
            return Some((i, false));
        }
        // Wrapped: the candidates before `from`.
        let end = from.min(last + 1);
        if end == 0 {
            return None;
        }
        scan(0, end - 1).map(|i| (i, true))
    } else {
        let scan = |lo: usize, hi: usize| -> Option<usize> {
            if lo > hi {
                return None;
            }
            match &exact {
                Some(needle) => {
                    memchr::memmem::rfind(&hay[lo..hi + needle.len()], needle).map(|i| i + lo)
                }
                None => (lo..=hi).rev().find(|&i| matches_at(hay, i, pat)),
            }
        };
        if from > 0
            && let Some(i) = scan(0, (from - 1).min(last))
        {
            return Some((i, false));
        }
        // Wrapped: the candidates at or after `from`.
        if from > last {
            return None;
        }
        scan(from, last).map(|i| (i, true))
    }
}

/// Parse an offset typed at the goto prompt, relative to `cursor` in a buffer
/// of `len` bytes:
///
/// - `0x1f0`, `1f0h`, `$1f0` — hex; `496` — decimal;
/// - `+0x10` / `-16` — relative to point;
/// - `50%` — that far into the buffer.
pub fn parse_offset(input: &str, cursor: usize, len: usize) -> Result<usize, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("no offset given".into());
    }
    if let Some(pct) = s.strip_suffix('%') {
        let p: f64 = pct
            .trim()
            .parse()
            .map_err(|_| format!("not a percentage: {s}"))?;
        if !(0.0..=100.0).contains(&p) {
            return Err("a percentage is 0–100".into());
        }
        return Ok(((len as f64) * p / 100.0).floor() as usize);
    }
    let (sign, body) = match s.as_bytes()[0] {
        b'+' => (1i8, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => (0, s),
    };
    let body = body.trim().replace('_', "");
    let value = if let Some(h) = body
        .strip_prefix("0x")
        .or_else(|| body.strip_prefix("0X"))
        .or_else(|| body.strip_prefix('$'))
        .or_else(|| body.strip_suffix('h'))
        .or_else(|| body.strip_suffix('H'))
    {
        u64::from_str_radix(h, 16).map_err(|_| format!("not a hex offset: {s}"))?
    } else {
        body.parse::<u64>()
            .map_err(|_| format!("not an offset: {s} (0x1f0, 496, +16, 50%)"))?
    };
    let value = usize::try_from(value).map_err(|_| "offset too large".to_string())?;
    Ok(match sign {
        1 => cursor.saturating_add(value),
        -1 => cursor.saturating_sub(value),
        _ => value,
    })
}

/// How a copied selection is written out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    /// `de ad be ef`
    Hex,
    /// `{ 0xde, 0xad, 0xbe, 0xef }`, wrapped every 12 bytes.
    CArray,
    /// Standard base64.
    Base64,
    /// The bytes as text (invalid UTF-8 becomes U+FFFD).
    Text,
}

impl CopyFormat {
    pub fn render(self, bytes: &[u8]) -> String {
        match self {
            Self::Hex => hex_string(bytes),
            Self::CArray => {
                let mut out = String::from("{");
                for (i, chunk) in bytes.chunks(12).enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str("\n    ");
                    let row: Vec<String> = chunk.iter().map(|b| format!("0x{b:02x}")).collect();
                    out.push_str(&row.join(", "));
                }
                out.push_str("\n}");
                out
            }
            Self::Base64 => {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode(bytes)
            }
            Self::Text => String::from_utf8_lossy(bytes).into_owned(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Hex => "hex",
            Self::CArray => "a C array",
            Self::Base64 => "base64",
            Self::Text => "text",
        }
    }
}

/// `de ad be ef`.
pub fn hex_string(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes() {
        assert_eq!(ByteClass::of(0), ByteClass::Null);
        assert_eq!(ByteClass::of(b'A'), ByteClass::Printable);
        assert_eq!(ByteClass::of(b' '), ByteClass::Whitespace);
        assert_eq!(ByteClass::of(b'\n'), ByteClass::Whitespace);
        assert_eq!(ByteClass::of(0x07), ByteClass::Control);
        assert_eq!(ByteClass::of(0x7f), ByteClass::Control);
        assert_eq!(ByteClass::of(0x80), ByteClass::High);
        assert_eq!(ascii_char(b'z'), 'z');
        assert_eq!(ascii_char(0), '.');
        assert_eq!(ascii_char(b' '), ' ');
    }

    #[test]
    fn binary_detection() {
        assert!(!looks_binary(b"hello\nworld\n"));
        assert!(!looks_binary("héllo".as_bytes()));
        assert!(looks_binary(b"\x7fELF\x02\x01\x01\x00"));
        assert!(looks_binary(&[0xff, 0xfe, 0x41]));
        assert!(!looks_binary(b""));
    }

    #[test]
    fn patterns() {
        assert_eq!(
            parse_pattern("de ad ?? ef").unwrap(),
            vec![Some(0xde), Some(0xad), None, Some(0xef)]
        );
        assert_eq!(
            parse_pattern("0x7f45").unwrap(),
            vec![Some(0x7f), Some(0x45)]
        );
        assert_eq!(parse_pattern("CAFE").unwrap(), vec![Some(0xca), Some(0xfe)]);
        assert_eq!(
            parse_pattern("\"cafe\"").unwrap(),
            b"cafe".iter().copied().map(Some).collect::<Vec<_>>()
        );
        assert_eq!(
            parse_pattern("ELF").unwrap(),
            b"ELF".iter().copied().map(Some).collect::<Vec<_>>()
        );
        assert!(parse_pattern("abc").is_err());
        assert!(parse_pattern("a?").is_err());
        assert!(parse_pattern("  ").is_err());
    }

    #[test]
    fn paste_reads_hex_or_text() {
        assert_eq!(paste_bytes("de ad"), vec![0xde, 0xad]);
        assert_eq!(paste_bytes("hello"), b"hello".to_vec());
        assert_eq!(paste_bytes("abc"), b"abc".to_vec());
    }

    #[test]
    fn find_forward_backward_and_wrap() {
        let hay = b"ab..ab..ab";
        let p = parse_pattern("\"ab").unwrap();
        assert_eq!(find(hay, &p, 0, true), Some((0, false)));
        assert_eq!(find(hay, &p, 1, true), Some((4, false)));
        assert_eq!(find(hay, &p, 9, true), Some((0, true)));
        assert_eq!(find(hay, &p, 8, true), Some((8, false)));
        assert_eq!(find(hay, &p, 8, false), Some((4, false)));
        assert_eq!(find(hay, &p, 0, false), Some((8, true)));
        let wild = vec![Some(b'a'), None, Some(b'.')];
        assert_eq!(find(hay, &wild, 1, true), Some((4, false)));
        assert_eq!(find(hay, &wild, 5, true), Some((0, true)));
        assert_eq!(find(hay, &wild, 4, false), Some((0, false)));
        assert_eq!(find(b"", &p, 0, true), None);
        assert_eq!(find(b"xyz", &p, 0, true), None);
    }

    #[test]
    fn offsets() {
        assert_eq!(parse_offset("0x10", 0, 100), Ok(16));
        assert_eq!(parse_offset("10h", 0, 100), Ok(16));
        assert_eq!(parse_offset("$10", 0, 100), Ok(16));
        assert_eq!(parse_offset("16", 0, 100), Ok(16));
        assert_eq!(parse_offset("+4", 10, 100), Ok(14));
        assert_eq!(parse_offset("-0x20", 10, 100), Ok(0));
        assert_eq!(parse_offset("50%", 0, 100), Ok(50));
        assert!(parse_offset("zz", 0, 100).is_err());
        assert!(parse_offset("150%", 0, 100).is_err());
    }

    #[test]
    fn copy_formats() {
        let b = [0xde, 0xad, 0xbe, 0xef];
        assert_eq!(CopyFormat::Hex.render(&b), "de ad be ef");
        assert_eq!(
            CopyFormat::CArray.render(&b),
            "{\n    0xde, 0xad, 0xbe, 0xef\n}"
        );
        assert_eq!(CopyFormat::Base64.render(&b), "3q2+7w==");
        assert_eq!(CopyFormat::Text.render(b"hi"), "hi");
    }
}
