//! Deterministic random CSV for the grid's property-style tests (no extra
//! dev-dependencies: a tiny xorshift PRNG is all the tests need).

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    /// xorshift64*.
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

pub struct GenOpts {
    pub delimiter: u8,
    /// Emit empty lines between records.
    pub blank_lines: bool,
    /// Vary the field count per record.
    pub ragged: bool,
    /// Use a bare `\r` as a terminator sometimes.
    pub lone_cr: bool,
    /// Start with a UTF-8 BOM (as text, U+FEFF).
    pub bom: bool,
}

const PLAIN: &[&str] = &[
    "a", "b", "z", "Q", "x", "é", "日", "😀", " ", "-", "_", "0", "7", "ü", "ok",
];

fn plain(rng: &mut Rng, max: usize) -> String {
    (0..1 + rng.below(max)).map(|_| *rng.pick(PLAIN)).collect()
}

/// One field as it appears in the file (quoted and escaped as needed).
pub fn gen_field(rng: &mut Rng, delimiter: u8) -> String {
    let delim = (delimiter as char).to_string();
    match rng.below(9) {
        0 => String::new(),
        1 => rng
            .pick(&["007", "1e5", "12345678901234567890", "-3.5", "42", "0"])
            .to_string(),
        2 | 3 => plain(rng, 6),
        4 => plain(rng, 30),
        5 | 6 => {
            // Quoted, with anything inside: delimiters, quotes, newlines.
            let inner: String = (0..rng.below(12))
                .map(|_| match rng.below(8) {
                    0 => delim.clone(),
                    1 => "\"\"".to_string(),
                    2 => "\n".to_string(),
                    3 => "\r\n".to_string(),
                    4 => ",".to_string(),
                    _ => plain(rng, 4),
                })
                .collect();
            format!("\"{inner}\"")
        }
        7 => format!("\"{}\"", plain(rng, 12)),
        _ => "\"\"".to_string(),
    }
}

/// A random CSV document.
pub fn gen_csv(rng: &mut Rng, opts: &GenOpts) -> String {
    let mut out = String::new();
    if opts.bom {
        out.push('\u{feff}');
    }
    let rows = rng.below(25);
    let cols = 1 + rng.below(5);
    let delim = (opts.delimiter as char).to_string();
    for r in 0..rows {
        let mut line = if opts.blank_lines && rng.chance(8) {
            String::new()
        } else {
            let n = if opts.ragged && rng.chance(15) {
                1 + rng.below(6)
            } else {
                cols
            };
            (0..n)
                .map(|_| gen_field(rng, opts.delimiter))
                .collect::<Vec<_>>()
                .join(&delim)
        };
        if !opts.blank_lines && line.is_empty() {
            line.push('x');
        }
        out.push_str(&line);
        let last = r + 1 == rows;
        if last && rng.chance(30) {
            break; // no final newline
        }
        let terms: &[&str] = if opts.lone_cr {
            &["\n", "\r\n", "\r"]
        } else {
            &["\n", "\r\n"]
        };
        out.push_str(rng.pick(terms));
    }
    out
}

/// A large CSV for Remove duplicates: `email,name,n`, then `rows` records,
/// each derived from a key drawn from `distinct` values — so two records
/// are duplicates exactly when their keys match, though they're written
/// differently (the email's case varies, `n` is `7` or `7.0`). Returns the
/// CSV and how many records repeat an earlier one.
pub fn gen_dup_csv(rng: &mut Rng, rows: usize, distinct: usize) -> (String, usize) {
    use std::fmt::Write;
    let mut out = String::with_capacity(rows * 32);
    out.push_str("email,name,n\n");
    let mut seen = vec![false; distinct];
    let mut dups = 0;
    for _ in 0..rows {
        let k = rng.below(distinct);
        dups += usize::from(std::mem::replace(&mut seen[k], true));
        let user = if rng.chance(50) { "User" } else { "user" };
        let n = if rng.chance(50) { ".0" } else { "" };
        let _ = writeln!(out, "{user}{k}@example.com,Name {k},{k}{n}");
    }
    (out, dups)
}
