//! The mode line (status bar) for [`Editor`]: the Emacs `mode-line-format`
//! analogue. Scheme (scheme/modeline.scm) defines an ordered segment list; the
//! live fields are resolved from current editor state per frame in Rust,
//! because the renderer can't call Scheme per frame.

use gpui::SharedString;

use super::*;

/// One field of a mode-line segment — either inert text or a live value the
/// renderer pulls from current editor state every frame (the threading forbids
/// calling Scheme per frame, so volatile values like the modified marker and the
/// mode stack are resolved here, in Rust, not pushed from Scheme).
#[derive(Clone)]
pub enum MlField {
    /// Inert text (separators, parens, labels Scheme bakes into the format).
    Literal(String),
    /// `**` when the buffer is modified, `--` when saved; `%%` (`%*` if
    /// modified) while it's read-only.
    Modified,
    /// The active buffer's name.
    BufferName,
    /// The active mode names, space-joined, then `Narrow` while the buffer
    /// is narrowed (as Emacs shows it).
    Modes,
    /// The evil (modal) state name — "Normal"/"Insert"/…, "" when evil is off.
    EvilMode,
    /// Ready language-server names, space-joined.
    Lsp,
    /// The LSP diagnostic summary ("E:n W:n"), "" when clean.
    Diagnostics,
    /// A grid buffer's range summary (`24 cells · Sum 1,204 · Avg 50.2`),
    /// "" outside a grid or without a multi-cell selection.
    Grid,
    /// The grid cursor's address and column type (`C7 · int`), or the
    /// selected range.
    GridCell,
    /// The grid's active filters and how many rows they show.
    GridFilter,
    /// A named register written from Scheme via `set-mode-line-slot`.
    Slot(String),
}

/// One mode-line segment: a [`MlField`] plus optional styling. `prefix`/`suffix`
/// wrap the resolved value and are emitted ONLY when the value is non-empty, so
/// an absent optional field takes its separators with it. `face` names a theme
/// face for the whole span, or `None` for the default mode-line color.
#[derive(Clone)]
pub struct MlSeg {
    pub field: MlField,
    pub face: Option<SharedString>,
    pub prefix: String,
    pub suffix: String,
}

impl MlSeg {
    pub(super) fn new(field: MlField, face: Option<&str>, prefix: &str, suffix: &str) -> Self {
        MlSeg {
            field,
            face: face.map(|f| SharedString::from(f.to_string())),
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
        }
    }
}

/// The built-in mode-line layout, used before `scheme/modeline.scm` installs its
/// own (and whenever no config does). Mirrors that file's default so behavior is
/// identical with or without Scheme: `-- name (modes) [project] git:branch lsp … E:n W:n`.
pub(super) fn default_mode_line_format() -> Vec<MlSeg> {
    use MlField::*;
    vec![
        MlSeg::new(Modified, None, "", ""),
        MlSeg::new(Literal("  ".into()), None, "", ""),
        MlSeg::new(BufferName, None, "", ""),
        MlSeg::new(Literal("  (".into()), None, "", ""),
        MlSeg::new(Modes, None, "", ""),
        MlSeg::new(Literal(")".into()), None, "", ""),
        MlSeg::new(Slot("project".into()), None, "  [", "]"),
        MlSeg::new(Slot("git".into()), Some("magit-branch"), "  git:", ""),
        MlSeg::new(Lsp, None, "  lsp ", ""),
        MlSeg::new(Slot("lsp-progress".into()), None, "  [", "]"),
        MlSeg::new(Diagnostics, None, "  ", ""),
        MlSeg::new(GridCell, None, "  ", ""),
        MlSeg::new(GridFilter, Some("warning"), "  [", "]"),
        MlSeg::new(Grid, None, "  ", ""),
    ]
}

/// Parse the `set-mode-line-format` payload into renderable segments. Each token
/// is either a bare string (a literal with the default face) or a tab-joined
/// `kind\tpayload\tface\tprefix\tsuffix` produced by modeline.scm's `ml-*`
/// helpers. An unknown `field` name renders as a visible `?name` marker so a
/// typo in config is obvious rather than silent.
pub(super) fn parse_mode_line_format(tokens: Vec<String>) -> Vec<MlSeg> {
    tokens
        .into_iter()
        .map(|token| {
            if !token.contains('\t') {
                return MlSeg::new(MlField::Literal(token), None, "", "");
            }
            let mut parts = token.splitn(5, '\t');
            let kind = parts.next().unwrap_or("");
            let payload = parts.next().unwrap_or("");
            let face = parts.next().unwrap_or("");
            let prefix = parts.next().unwrap_or("");
            let suffix = parts.next().unwrap_or("");
            let field = match kind {
                "field" => match payload {
                    "modified" => MlField::Modified,
                    "buffer-name" => MlField::BufferName,
                    "modes" => MlField::Modes,
                    "evil-mode" => MlField::EvilMode,
                    "lsp" => MlField::Lsp,
                    "diagnostics" => MlField::Diagnostics,
                    "grid" => MlField::Grid,
                    "grid-cell" => MlField::GridCell,
                    "grid-filter" => MlField::GridFilter,
                    other => MlField::Literal(format!("?{other}")),
                },
                "slot" => MlField::Slot(payload.to_string()),
                _ => MlField::Literal(payload.to_string()),
            };
            MlSeg::new(field, (!face.is_empty()).then_some(face), prefix, suffix)
        })
        .collect()
}

impl Editor {
    /// Resolve the mode-line format into rendered `(text, face)` spans, in
    /// order, dropping any segment whose value is empty (so optional fields and
    /// their separators vanish when there's nothing to show). The live fields
    /// (`modified`, `modes`, …) read current editor state, so the bar never goes
    /// stale between the sparse Scheme hooks; `face` is `None` for the default
    /// mode-line color and a face name for the renderer to resolve otherwise.
    pub fn mode_line_segments(&self) -> Vec<(SharedString, Option<SharedString>)> {
        self.mode_line_format
            .iter()
            .filter_map(|seg| {
                let value = match &seg.field {
                    MlField::Literal(s) => s.clone(),
                    // Emacs's `%%` / `%*` for a read-only buffer.
                    MlField::Modified => match (self.active.read_only, self.is_modified()) {
                        (true, false) => "%%",
                        (true, true) => "%*",
                        (false, true) => "**",
                        (false, false) => "--",
                    }
                    .to_string(),
                    MlField::BufferName => self.buffer_name().to_string(),
                    MlField::Modes => {
                        let mut modes = self.active.modes.names();
                        if self.is_narrowed() {
                            modes.push("Narrow");
                        }
                        modes.join(" ")
                    }
                    MlField::EvilMode => self.evil_mode_name().unwrap_or("").to_string(),
                    MlField::Lsp => self.active_lsp_servers().join(" "),
                    MlField::Diagnostics => self.diagnostic_summary().unwrap_or_default(),
                    MlField::Grid => self
                        .active
                        .grid
                        .as_ref()
                        .map(|g| g.summary.clone())
                        .unwrap_or_default(),
                    MlField::GridCell => self
                        .active
                        .grid
                        .as_ref()
                        .map(|g| g.cell_status())
                        .unwrap_or_default(),
                    MlField::GridFilter => self
                        .active
                        .grid
                        .as_ref()
                        .map(|g| g.filter_status())
                        .unwrap_or_default(),
                    MlField::Slot(key) => self
                        .mode_line_slots
                        .get(key)
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                };
                if value.is_empty() {
                    return None;
                }
                let text = format!("{}{value}{}", seg.prefix, seg.suffix);
                Some((text.into(), seg.face.clone()))
            })
            .collect()
    }

    /// Write (or, on empty, clear) a named mode-line register.
    pub(super) fn set_mode_line_slot(&mut self, key: String, value: String) {
        if value.is_empty() {
            self.mode_line_slots.remove(&key);
        } else {
            self.mode_line_slots.insert(key, value.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MlField, default_mode_line_format, parse_mode_line_format};

    #[test]
    fn mode_line_format_parses_literals_fields_slots_and_styling() {
        // A bare string with no tab is an inert literal, default face.
        let segs = parse_mode_line_format(vec!["  (".into()]);
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0].field, MlField::Literal(s) if s == "  ("));
        assert!(segs[0].face.is_none());

        // A tab-encoded field with face/prefix/suffix.
        let segs = parse_mode_line_format(vec!["field\tlsp\tdim\t  lsp \t!".into()]);
        assert!(matches!(segs[0].field, MlField::Lsp));
        assert_eq!(segs[0].face.as_ref().map(|f| f.as_ref()), Some("dim"));
        assert_eq!(segs[0].prefix, "  lsp ");
        assert_eq!(segs[0].suffix, "!");

        // Empty face stays None; a slot carries its key.
        let segs = parse_mode_line_format(vec!["slot\tgit\t\t  git:\t".into()]);
        assert!(matches!(&segs[0].field, MlField::Slot(k) if k == "git"));
        assert!(segs[0].face.is_none());
        assert_eq!(segs[0].prefix, "  git:");

        // An unknown field name surfaces as a visible marker, not silence.
        let segs = parse_mode_line_format(vec!["field\tbogus\t\t\t".into()]);
        assert!(matches!(&segs[0].field, MlField::Literal(s) if s == "?bogus"));
    }

    #[test]
    fn default_mode_line_format_is_the_known_layout() {
        let segs = default_mode_line_format();
        // Leads with the modified marker and ends with the diagnostics field,
        // then the grid fields (all empty outside a grid).
        assert!(matches!(segs.first().unwrap().field, MlField::Modified));
        assert!(matches!(segs[segs.len() - 4].field, MlField::Diagnostics));
        assert!(matches!(segs[segs.len() - 3].field, MlField::GridCell));
        assert!(matches!(segs[segs.len() - 2].field, MlField::GridFilter));
        assert!(matches!(segs.last().unwrap().field, MlField::Grid));
        // The git slot is colored with the magit branch face.
        let git = segs
            .iter()
            .find(|s| matches!(&s.field, MlField::Slot(k) if k == "git"))
            .expect("git slot present");
        assert_eq!(git.face.as_ref().map(|f| f.as_ref()), Some("magit-branch"));
        assert_eq!(git.prefix, "  git:");
    }
}
