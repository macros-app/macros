#![allow(dead_code)]

use super::export;
use super::model::*;
use super::parse::parse_now;
use super::series::next_in_series;
use super::style::NumFormat;
use super::testgen::Rng;
use super::view;
use super::write::to_bytes;

fn load_bytes(b: &[u8]) -> Workbook {
    parse_now(b.to_vec(), None).unwrap()
}

#[test]
fn time_key_overflow() {
    // Debug: panics with "attempt to multiply with overflow".
    let r = std::panic::catch_unwind(|| time_key("1/1/999999"));
    assert!(r.is_ok(), "time_key panicked on 1/1/999999");
    let r = std::panic::catch_unwind(|| time_key("2024-01-01 999999:00"));
    assert!(r.is_ok(), "time_key panicked on big hour");
}

#[test]
fn sort_time_panics() {
    let mut wb = load_bytes(b"d\n1/1/2024\n1/1/999999\n");
    wb.sort_by_column_as(0, 0, false, true, SortKind::Time);
}

#[test]
#[ignore = "known limitation: a corrupt UTF-16 sequence decodes to U+FFFD, which a save writes back"]
fn utf16_lone_surrogate_round_trip() {
    // UTF-16LE with BOM, "a,b\n" then a lone high surrogate D800 in a cell.
    let mut bytes = b"\xFF\xFE".to_vec();
    for u in [
        b'a' as u16,
        b',' as u16,
        b'b' as u16,
        b'\n' as u16,
        b'x' as u16,
        0xD800,
        b'\n' as u16,
    ] {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    let wb = load_bytes(&bytes);
    assert_eq!(
        to_bytes(&wb),
        bytes,
        "untouched UTF-16 file changed on save"
    );
}

#[test]
#[ignore = "known limitation: a corrupt UTF-16 sequence decodes to U+FFFD, which a save writes back"]
fn utf16_odd_length_round_trip() {
    let mut bytes = b"\xFF\xFE".to_vec();
    for u in "a,b\n1,2\n".encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes.push(b'z'); // stray odd byte
    let wb = load_bytes(&bytes);
    assert_eq!(
        to_bytes(&wb),
        bytes,
        "untouched odd-length UTF-16 file changed on save"
    );
}

#[test]
fn utf8_bom_with_late_invalid_byte() {
    let mut bytes = b"\xEF\xBB\xBFname,v\n".to_vec();
    while bytes.len() < 70_000 {
        bytes.extend_from_slice(b"x,1\n");
    }
    bytes.extend_from_slice(b"caf\xE9,2\n");
    let wb = load_bytes(&bytes);
    assert_eq!(to_bytes(&wb), bytes);
    assert_eq!(wb.cell(0, 0, 0), "name", "BOM decoded into the first cell");
}

#[test]
fn series_big_integer() {
    assert_eq!(
        next_in_series(None, "9007199254740993", 1),
        "9007199254740994"
    );
}

#[test]
fn series_big_integer2() {
    assert_eq!(
        next_in_series(None, "12345678901234567890", 1),
        "12345678901234567891"
    );
}

#[test]
fn numformat_huge() {
    let f = NumFormat::parse("0.00").unwrap();
    assert_eq!(
        f.apply("1e307").as_deref().map(|s| s.contains("inf")),
        Some(false),
        "{:?}",
        f.apply("1e307")
    );
}

#[test]
fn numformat_round_half() {
    let f = NumFormat::parse("0.00").unwrap();
    assert_eq!(f.apply("1.005").as_deref(), Some("1.01"));
}

#[test]
fn org_dash_row_lost() {
    let rows = vec![
        vec!["k".to_string(), "v".to_string()],
        vec!["-".to_string(), "-".to_string()],
        vec!["a".to_string(), "1".to_string()],
    ];
    let org = export::to_org(&rows, true);
    let lines: Vec<&str> = org.lines().collect();
    let (back, _) = export::parse_table(&lines).unwrap();
    assert_eq!(back, rows, "org:\n{org}");
}

#[test]
fn json_key_collision() {
    let rows = vec![
        vec!["B".to_string(), "".to_string()],
        vec!["x".to_string(), "y".to_string()],
    ];
    let json = export::to_json(&rows, true, 0);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v[0].as_object().unwrap().len(), 2, "{json}");
}

#[test]
fn tsv_block_lone_cr() {
    let rows = vec![vec!["a\rb".to_string(), "c".to_string()]];
    assert_eq!(view::parse_tsv_block(&view::to_tsv_block(&rows)), rows);
}

/// Random edit sequences, then undo everything: bytes and clean flags must be
/// back to the original; redo everything: bytes must match the edited state.
#[test]
fn model_random_undo_redo() {
    let mut rng = Rng::new(0xabcdef);
    for case in 0..300 {
        let rows = [3usize, 10, 4100, 8200][case % 4];
        let cols = 1 + rng.below(4);
        let mut src = String::new();
        if case % 2 == 1 {
            let opts = super::testgen::GenOpts {
                delimiter: b",\t;|"[case % 4],
                blank_lines: rng.chance(50),
                ragged: rng.chance(50),
                lone_cr: rng.chance(20),
                bom: rng.chance(15),
            };
            src = super::testgen::gen_csv(&mut rng, &opts);
        } else {
            for r in 0..rows {
                let fields: Vec<String> = (0..cols)
                    .map(|c| format!("v{}_{}", r * 7 % 13, c))
                    .collect();
                src.push_str(&fields.join(","));
                src.push_str(if rng.chance(20) { "\r\n" } else { "\n" });
            }
        }
        let mut wb = load_bytes(src.as_bytes());
        let orig = to_bytes(&wb);
        let mut log = Vec::new();
        for _ in 0..12 {
            let nr = wb.used_rows(0).max(1);
            let nc = wb.used_cols(0).max(1);
            let op = rng.below(8);
            match op {
                0 => {
                    let cells: Vec<(usize, usize, String)> = (0..1 + rng.below(4))
                        .map(|_| {
                            (
                                rng.below(nr + 3),
                                rng.below(nc + 2),
                                if rng.chance(30) {
                                    String::new()
                                } else if rng.chance(50) {
                                    {
                                        let d = b",\t;|"[rng.below(4)];
                                        super::testgen::gen_field(&mut rng, d)
                                    }
                                } else {
                                    format!("e{}", rng.below(100000000))
                                },
                            )
                        })
                        .collect();
                    wb.set_cells(0, &cells);
                }
                1 => {
                    wb.insert_rows(0, rng.below(nr), 1 + rng.below(5000));
                }
                2 => {
                    wb.delete_rows(0, rng.below(nr), 1 + rng.below(5000));
                }
                3 => {
                    wb.insert_cols(0, rng.below(nc), 1 + rng.below(2));
                }
                4 => {
                    if std::env::var("AUDIT_DELCOL").is_ok() {
                        wb.delete_cols(0, rng.below(nc), 1);
                    }
                }
                5 => {
                    wb.sort_by_column(0, rng.below(nc), rng.chance(50), rng.chance(50));
                }
                6 => {
                    wb.move_col(0, rng.below(nc), rng.below(nc));
                }
                _ => {
                    let rows = wb.used_rows(0);
                    if rows >= 2 {
                        let s = rng.below(rows - 1);
                        wb.permute_rows(0, s, vec![s + 1, s], 0);
                    }
                }
            }
            log.push(op);
            for s in wb.sheets() {
                s.check_layout();
            }
        }
        let edited = to_bytes(&wb);
        let snapshot: Vec<Vec<String>> = (0..wb.used_rows(0))
            .map(|r| {
                (0..wb.used_cols(0))
                    .map(|c| wb.cell(0, r, c).to_string())
                    .collect()
            })
            .collect();
        while wb.undo().is_some() {
            for s in wb.sheets() {
                s.check_layout();
            }
        }
        assert_eq!(
            to_bytes(&wb),
            orig,
            "case {case} undo-all bytes, ops {log:?}"
        );
        assert!(!wb.is_modified());
        let s = wb.sheet(0);
        for r in 0..s.rows() {
            assert!(
                s.row_meta(r).unwrap().is_clean(),
                "case {case} row {r} not clean after undo-all, ops {log:?}"
            );
        }
        while wb.redo().is_some() {}
        assert_eq!(
            to_bytes(&wb),
            edited,
            "case {case} redo-all bytes, ops {log:?}"
        );
        let snap2: Vec<Vec<String>> = (0..wb.used_rows(0))
            .map(|r| {
                (0..wb.used_cols(0))
                    .map(|c| wb.cell(0, r, c).to_string())
                    .collect()
            })
            .collect();
        assert_eq!(snap2, snapshot, "case {case} redo cells, ops {log:?}");
        // Re-parse edited output yields same cells within kept rows.
        let back = load_bytes(&edited);
        let keep = wb.sheet(0).last_kept_row().map_or(0, |r| r + 1);
        for r in 0..keep {
            for c in 0..wb.used_cols(0) {
                assert_eq!(
                    back.cell(0, r, c),
                    wb.cell(0, r, c),
                    "case {case} reparse ({r},{c}) ops {log:?}"
                );
            }
        }
    }
}

#[test]
fn delete_col_undo_after_row_relayout() {
    let src: String = (0..5000).map(|i| format!("{i},x{i}\n")).collect();
    let mut wb = load_bytes(src.as_bytes());
    assert!(wb.delete_cols(0, 1, 1));
    assert!(wb.insert_rows(0, 0, 5000)); // splits chunk 0
    eprintln!("after insert {:?}", wb.sheet(0).starts);
    wb.undo(); // removes them again: chunks re-merged into a different layout
    eprintln!("after undo {:?}", wb.sheet(0).starts);
    wb.undo(); // puts column 1 back with its old chunk layout
    assert_eq!(wb.cell(0, 4999, 1), "x4999");
    assert_eq!(to_bytes(&wb), src.as_bytes());
}

#[test]
fn delete_col_undo_after_far_set_cell() {
    let src: String = (0..5000).map(|i| format!("{i},x{i}\n")).collect();
    let mut wb = load_bytes(src.as_bytes());
    assert!(wb.delete_cols(0, 1, 1));
    assert!(wb.set_cell(0, 9000, 0, "far")); // grows: last chunk splits
    wb.undo(); // truncates back: re-merged differently
    wb.undo();
    assert_eq!(wb.cell(0, 4999, 1), "x4999");
}

#[test]
fn doubled_dialect_value_with_backslash_quote_reload() {
    let mut wb = load_bytes(b"a,b\n1,2\n");
    wb.set_cell(0, 1, 1, "path C:\\\" end");
    let out = to_bytes(&wb);
    let back = load_bytes(&out);
    assert_eq!(
        back.cell(0, 1, 1),
        "path C:\\\" end",
        "saved: {:?}",
        String::from_utf8_lossy(&out)
    );
}

#[test]
fn write_1252_unrepresentable() {
    let mut wb = load_bytes(b"name;city\nZo\xEB;K\xF6ln\n");
    wb.set_cell(0, 1, 1, "\u{65e5}");
    // A character the encoding can't hold is caught before a save.
    assert_eq!(
        super::write::unencodable(&wb, wb.dialect()),
        Some('\u{65e5}')
    );
    wb.set_cell(0, 1, 1, "K\u{f6}ln!");
    assert_eq!(super::write::unencodable(&wb, wb.dialect()), None);
}

#[test]
fn set_cell_in_deleted_all_cols_then_undo() {
    let mut wb = load_bytes(b"a,b\n1,2\n");
    assert!(wb.delete_cols(0, 0, 2));
    assert!(wb.set_cell(0, 0, 3, "x"));
    wb.undo();
    wb.undo();
    assert_eq!(to_bytes(&wb), b"a,b\n1,2\n");
}

/// Remove duplicates stays linear: a million rows, half of them repeats,
/// found and deleted (and the delete undone) in a few seconds even in a
/// debug build. An O(n²) pass — comparing rows pairwise, or deleting them
/// one at a time — would take hours.
#[test]
fn remove_duplicates_perf_guard_1m_rows() {
    use super::dedup::duplicate_rows;
    use std::time::{Duration, Instant};
    let rows = 1_000_000;
    let (csv, expected) = super::testgen::gen_dup_csv(&mut Rng::new(7), rows, rows / 2);
    let mut wb = load_bytes(csv.as_bytes());
    assert_eq!(wb.used_rows(0), rows + 1);
    let t = Instant::now();
    let dups = duplicate_rows(1..=rows, &[0, 1, 2], |r, c| wb.cell(0, r, c));
    assert_eq!(dups.len(), expected);
    assert!(wb.delete_row_set(0, &dups));
    let took = t.elapsed();
    assert_eq!(wb.used_rows(0), rows + 1 - expected);
    // What's left is unique.
    assert!(duplicate_rows(1..wb.used_rows(0), &[0], |r, c| wb.cell(0, r, c)).is_empty());
    let t = Instant::now();
    wb.undo();
    let undo = t.elapsed();
    assert_eq!(wb.used_rows(0), rows + 1);
    assert!(
        took < Duration::from_secs(60) && undo < Duration::from_secs(30),
        "remove duplicates took {took:?}, undo {undo:?}"
    );
}
