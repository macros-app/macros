//! The data inspector: the bytes at point read as the common fixed-size
//! types, in the chosen byte order.

/// `(label, value)` pairs for the bytes starting at `at`. Types that need
/// more bytes than remain are left out.
pub fn fields(data: &[u8], at: usize, big_endian: bool) -> Vec<(&'static str, String)> {
    let rest = data.get(at..).unwrap_or(&[]);
    let mut out = Vec::new();
    let Some(&b) = rest.first() else {
        return out;
    };
    out.push(("bin", format!("{b:08b}")));
    out.push(("u8", b.to_string()));
    out.push(("i8", (b as i8).to_string()));
    macro_rules! int {
        ($label:literal, $t:ty, $n:literal) => {
            if let Some(arr) = rest.get(..$n).and_then(|s| <[u8; $n]>::try_from(s).ok()) {
                let v = if big_endian {
                    <$t>::from_be_bytes(arr)
                } else {
                    <$t>::from_le_bytes(arr)
                };
                out.push(($label, v.to_string()));
            }
        };
    }
    int!("u16", u16, 2);
    int!("i16", i16, 2);
    int!("u32", u32, 4);
    int!("i32", i32, 4);
    int!("u64", u64, 8);
    int!("i64", i64, 8);
    if let Some(arr) = rest.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) {
        let v = if big_endian {
            f32::from_be_bytes(arr)
        } else {
            f32::from_le_bytes(arr)
        };
        out.push(("f32", format_float(v as f64)));
    }
    if let Some(arr) = rest.get(..8).and_then(|s| <[u8; 8]>::try_from(s).ok()) {
        let v = if big_endian {
            f64::from_be_bytes(arr)
        } else {
            f64::from_le_bytes(arr)
        };
        out.push(("f64", format_float(v)));
    }
    if let Some(c) = utf8_char(rest) {
        let shown = if c.is_control() {
            format!("U+{:04X}", c as u32)
        } else {
            format!("{c} U+{:04X}", c as u32)
        };
        out.push(("utf8", shown));
    }
    out
}

/// The UTF-8 character starting at `bytes[0]`, if it begins one.
fn utf8_char(bytes: &[u8]) -> Option<char> {
    let n = match *bytes.first()? {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    std::str::from_utf8(bytes.get(..n)?)
        .ok()
        .and_then(|s| s.chars().next())
}

fn format_float(v: f64) -> String {
    if v.is_nan() || v.is_infinite() {
        return v.to_string();
    }
    if v != 0.0 && (v.abs() >= 1e9 || v.abs() < 1e-4) {
        format!("{v:.6e}")
    } else {
        let s = format!("{v:.6}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(f: &'a [(&str, String)], k: &str) -> Option<&'a str> {
        f.iter().find(|(l, _)| *l == k).map(|(_, v)| v.as_str())
    }

    #[test]
    fn little_and_big_endian() {
        let data = [0x01, 0x02, 0x03, 0x04];
        let le = fields(&data, 0, false);
        assert_eq!(get(&le, "u8"), Some("1"));
        assert_eq!(get(&le, "u16"), Some("513"));
        assert_eq!(get(&le, "u32"), Some("67305985"));
        assert_eq!(get(&le, "u64"), None);
        let be = fields(&data, 0, true);
        assert_eq!(get(&be, "u16"), Some("258"));
        assert_eq!(get(&be, "u32"), Some("16909060"));
        assert_eq!(get(&be, "bin"), Some("00000001"));
    }

    #[test]
    fn signed_float_and_utf8() {
        let f = fields(&[0xff], 0, false);
        assert_eq!(get(&f, "i8"), Some("-1"));
        let one = 1.5f32.to_le_bytes();
        assert_eq!(get(&fields(&one, 0, false), "f32"), Some("1.5"));
        let e = "é".as_bytes();
        assert_eq!(get(&fields(e, 0, false), "utf8"), Some("é U+00E9"));
        assert!(fields(&[], 0, false).is_empty());
        assert!(fields(&[1], 5, false).is_empty());
    }
}
