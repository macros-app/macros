//! Pure position motions: compute a new byte offset from a given one without
//! moving.

use super::*;
use steel::rvals::SteelVal;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared { snapshot, .. } = ctx;
    reg.section(Section::Motions);
    // --- Pure position motions (compute, don't move) ----------------------
    // These resolve a *new byte offset* from a given one against the published
    // buffer, reusing the editor's grapheme/word/line logic. Plain motions
    // (`forward-char`, …) move the primary cursor as a side effect; these are
    // pure, so Helix's selection-first commands can compute a fresh head for
    // *every* selection and hand the lot back via `(set-selections …)`.
    macro_rules! register_pos_fn {
        ($name:literal, $f:expr) => {{
            let shared = snapshot.clone();
            reg.register_fn($name, move |pos: isize| -> usize {
                let pos = scheme_pos(pos);
                let buf = crate::buffer::Buffer::from_rope(shared.load().text.clone());
                let f: fn(&crate::buffer::Buffer, usize) -> usize = $f;
                f(&buf, pos.min(buf.len()))
            });
        }};
    }
    register_pos_fn!("grapheme-forward", |b, p| b.next_grapheme(p));
    register_pos_fn!("grapheme-backward", |b, p| b.prev_grapheme(p));
    register_pos_fn!("word-forward", |b, p| b.forward_word(p));
    register_pos_fn!("word-forward-start", |b, p| b.forward_word_start(p));
    register_pos_fn!("word-backward", |b, p| b.backward_word(p));
    register_pos_fn!("word-backward-start", |b, p| b.backward_word_start(p));
    register_pos_fn!("word-forward-end", |b, p| b.forward_word_end(p));
    register_pos_fn!("bigword-forward-start", |b, p| b.forward_bigword_start(p));
    register_pos_fn!("bigword-backward-start", |b, p| b.backward_bigword_start(p));
    register_pos_fn!("bigword-forward-end", |b, p| b.forward_bigword_end(p));
    register_pos_fn!("word-backward-end", |b, p| b.backward_word_end(p));
    register_pos_fn!("bigword-backward-end", |b, p| b.backward_bigword_end(p));
    // --- Sexp positions (compute, don't move) -----------------------------
    // On the same language-aware reader as `forward-sexp`: Lisp buffers read
    // as Lisp, everything else as bracket-balanced code whose strings and
    // comments come from the syntax tree.
    let read_sexps = |snap: &EditorSnapshot| {
        crate::sexp::parse_buffer(
            &snap.text.to_string(),
            &snap.language,
            &snap.major_mode,
            snap.tree.as_ref(),
        )
    };
    // `(lisp-buffer?)` → whether the buffer reads as Lisp for sexp motion.
    register_snapshot_read(reg, snapshot, "lisp-buffer?", |s| {
        crate::sexp::is_lisp(&s.language, &s.major_mode)
    });
    {
        // `(sexp-forward-pos pos)` → end of the sexp at or after pos, or -1.
        let shared = snapshot.clone();
        reg.register_fn("sexp-forward-pos", move |NonNeg(pos): NonNeg| -> isize {
            let snap = shared.load();
            crate::sexp::forward_sexp_in(&read_sexps(&snap), pos).map_or(-1, |p| p as isize)
        });
    }
    {
        // `(sexp-backward-pos pos)` → start of the sexp before pos, or -1.
        let shared = snapshot.clone();
        reg.register_fn("sexp-backward-pos", move |NonNeg(pos): NonNeg| -> isize {
            let snap = shared.load();
            crate::sexp::backward_sexp_in(&read_sexps(&snap), pos).map_or(-1, |p| p as isize)
        });
    }
    {
        // `(sexp-top-level-ranges)` → flat `(start end …)` of every top-level
        // datum: Lisp's defuns, and the fallback for buffers with no tree.
        let shared = snapshot.clone();
        reg.register_fn("sexp-top-level-ranges", move || -> Vec<usize> {
            let snap = shared.load();
            crate::sexp::top_level_ranges(&read_sexps(&snap))
                .into_iter()
                .flat_map(|(s, e)| [s, e])
                .collect()
        });
    }
    {
        // `(buffer-regex-matches pattern start end)` → a flat list
        // `(s0 e0 s1 e1 …)` of the BUFFER byte ranges of every match inside
        // [start, end). Offsets are buffer bytes, so callers never mix the
        // char-indexed string accessors with regex byte offsets.
        let shared = snapshot.clone();
        reg.register_fn(
            "buffer-regex-matches",
            move |pattern: String, NonNeg(start): NonNeg, NonNeg(end): NonNeg| -> Vec<usize> {
                let Ok(re) = regex(&pattern) else {
                    return Vec::new();
                };
                let rope = shared.load().text.clone();
                let (a, b) = (snap_byte(&rope, start), snap_byte(&rope, end));
                if a >= b {
                    return Vec::new();
                }
                let text = rope.byte_slice(a..b).to_string();
                re.find_iter(&text)
                    .flat_map(|m| [a + m.start(), a + m.end()])
                    .collect()
            },
        );
    }
    {
        // `(regex-search-pos pattern from forward?)` → byte offset of the next
        // match strictly after (forward) / before (backward) `from`, wrapping
        // around the buffer like `n`/`N`; -1 when none (or a bad regex).
        let shared = snapshot.clone();
        reg.register_fn(
            "regex-search-pos",
            move |pattern: String, NonNeg(from): NonNeg, forward: bool| -> isize {
                let Ok(re) = regex(&pattern) else {
                    return -1;
                };
                let text = shared.load().text.to_string();
                let starts: Vec<usize> = re.find_iter(&text).map(|m| m.start()).collect();
                let hit = if forward {
                    starts.iter().find(|&&s| s > from).or(starts.first())
                } else {
                    starts.iter().rev().find(|&&s| s < from).or(starts.last())
                };
                hit.map_or(-1, |&h| h as isize)
            },
        );
    }
    register_pos_fn!("line-start-pos", |b, p| b.line_range_at(p).start);
    register_pos_fn!("line-end-pos", |b, p| b.line_range_at(p).end);
    register_pos_fn!("line-of-pos", |b, p| b.line_index_at_byte(p));
    // `(char-column pos)` → the CHARACTER column of `pos` on its line. Vertical
    // motion must carry a char column, never a byte one: a byte column landing
    // on a line with multibyte text points inside a character, and the rope
    // refuses to slice there. Paired with `pos-at-char-column` below.
    register_pos_fn!("char-column", |b, p| b.char_column(p));
    {
        // `(line-start-of-line n)` → first byte of (0-based) line `n`, clamped.
        let shared = snapshot.clone();
        reg.register_fn("line-start-of-line", move |NonNeg(line): NonNeg| -> usize {
            let buf = crate::buffer::Buffer::from_rope(shared.load().text.clone());
            let last = buf.line_count().saturating_sub(1);
            buf.line_range(line.min(last)).start
        });
    }
    {
        // `(pos-at-char-column line col)` → the byte offset of character column
        // `col` on (0-based) `line`, clamped to that line's end. The inverse of
        // `char-column`; together they are how Scheme does vertical motion
        // without ever synthesising a mid-codepoint offset.
        let shared = snapshot.clone();
        reg.register_fn(
            "pos-at-char-column",
            move |NonNeg(line): NonNeg, NonNeg(col): NonNeg| -> usize {
                let buf = crate::buffer::Buffer::from_rope(shared.load().text.clone());
                buf.byte_at_char_column(line, col)
            },
        );
    }
    {
        // `(display-column pos tab-width)` → the DISPLAY column of `pos` on its
        // line (Emacs `current-column`): tabs run to the next tab stop, wide
        // characters count two. Rectangles (C-x r …) work in these columns.
        let shared = snapshot.clone();
        reg.register_fn(
            "display-column",
            move |pos: isize, NonNeg(tab): NonNeg| -> usize {
                let buf = crate::buffer::Buffer::from_rope(shared.load().text.clone());
                let pos = buf.snap(scheme_pos(pos));
                let start = buf.line_range_at(pos).start;
                crate::rect::display_width(&buf.slice_to_string(start..pos), tab)
            },
        );
    }
    {
        // `(rectangle-row line left right tab-width)` → how (0-based) `line`
        // meets the display-column rectangle [left, right): the list
        // `(start end before after pad inside)`. [start, end) is the buffer
        // byte range it touches (widened to whole chars where an edge cuts a
        // tab or wide char); `before`/`after` are the columns of those cut
        // chars outside the rectangle (to put back as spaces); `pad` is how far
        // the line falls short of `left`; `inside` is the rectangle's text on
        // the line, space-padded to exactly `right - left` columns.
        let shared = snapshot.clone();
        reg.register_fn(
            "rectangle-row",
            move |NonNeg(line): NonNeg,
                  NonNeg(left): NonNeg,
                  NonNeg(right): NonNeg,
                  NonNeg(tab): NonNeg|
                  -> SteelVal {
                let buf = crate::buffer::Buffer::from_rope(shared.load().text.clone());
                let last = buf.line_count().saturating_sub(1);
                let lr = buf.line_range(line.min(last));
                let span =
                    crate::rect::row_span(&buf.slice_to_string(lr.clone()), left, right, tab);
                let int = |n: usize| SteelVal::IntV(n as isize);
                SteelVal::ListV(
                    vec![
                        int(lr.start + span.start),
                        int(lr.start + span.end),
                        int(span.before),
                        int(span.after),
                        int(span.pad),
                        SteelVal::StringV(span.inside.into()),
                    ]
                    .into(),
                )
            },
        );
    }
}
