//! The grid (CSV / spreadsheet) primitives: which files open as a grid, and
//! the grid customization API (see `scheme/grid.scm` for the overview).

use std::sync::Arc;
use steel::rvals::SteelVal;

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared {
        queue, snapshot, ..
    } = ctx;
    reg.section(Section::Editing);
    // Grid buffers: which file extensions open as a grid, and jumping to an
    // address (`C7`, `B2:D9`) from Scheme.
    {
        let q = queue.clone();
        reg.register_fn("grid-extensions", move |StringList(exts): StringList| {
            q.push(EditorCommand::GridExtensions(exts))
        });
    }
    register_unary(
        reg,
        queue,
        "grid-goto-address",
        EditorCommand::GridGotoAddress,
    );

    reg.section(Section::Grid);
    register_grid_api(reg, queue, snapshot);
}

/// The grid customization API (see `scheme/grid.scm` for the overview):
/// reads from the published [`GridSnapshot`], and queued
/// [`GridOp`](crate::grid::cmd::GridOp)s for edits, structure, view, and
/// declarative styling. Column arguments take a 0-based index or a column
/// name (`"C"` / `'C`); rows are 0-based.
fn register_grid_api(reg: &mut Registrar, queue: &Queue, snapshot: &SharedSnapshot) {
    use crate::grid::cmd::GridOp;
    let push = |queue: &Queue| {
        let q = queue.clone();
        move |op: GridOp| q.push(EditorCommand::GridOp(op))
    };
    // --- reads ------------------------------------------------------------
    register_snapshot_read(reg, snapshot, "grid-buffer?", |s| s.grid.is_some());
    register_snapshot_read(reg, snapshot, "grid-cursor", |s| {
        s.grid
            .as_ref()
            .map_or_else(Vec::new, |g| vec![g.cursor.0, g.cursor.1])
    });
    register_snapshot_read(reg, snapshot, "grid-selection", |s| {
        s.grid.as_ref().map_or_else(Vec::new, |g| {
            let (a, b, c, d) = g.selection;
            vec![a, b, c, d]
        })
    });
    register_snapshot_read(reg, snapshot, "grid-dimensions", |s| {
        s.grid
            .as_ref()
            .map_or_else(Vec::new, |g| vec![g.rows, g.cols])
    });
    register_snapshot_read(reg, snapshot, "grid-current-value", |s| {
        s.grid.as_ref().map(|g| g.value.clone()).unwrap_or_default()
    });
    register_snapshot_read(reg, snapshot, "grid-header?", |s| {
        s.grid.as_ref().is_some_and(|g| g.header)
    });
    register_snapshot_read(reg, snapshot, "grid-header-names", |s| {
        s.grid
            .as_ref()
            .map(|g| g.header_names.clone())
            .unwrap_or_default()
    });
    // `(grid-sheets)`: an .xlsx grid's sheet names, in workbook order (empty
    // for a CSV). `(grid-open-sheet name)` shows one.
    register_snapshot_read(reg, snapshot, "grid-sheets", |s| {
        s.grid
            .as_ref()
            .map(|g| g.sheets.clone())
            .unwrap_or_default()
    });
    // `(grid-current-sheet)`: the name of the sheet this grid shows ("" for
    // a CSV).
    register_snapshot_read(reg, snapshot, "grid-current-sheet", |s| {
        s.grid
            .as_ref()
            .and_then(|g| g.sheet.clone())
            .unwrap_or_default()
    });
    register_snapshot_read(reg, snapshot, "grid-frozen-rows", |s| {
        s.grid.as_ref().map_or(0, |g| g.frozen_rows)
    });
    register_snapshot_read(reg, snapshot, "grid-zoom", |s| {
        s.grid.as_ref().map_or(100.0, |g| g.zoom)
    });
    register_snapshot_read(reg, snapshot, "grid-editing?", |s| {
        s.grid.as_ref().is_some_and(|g| g.editing)
    });
    {
        let shared = snapshot.clone();
        reg.register_fn("grid-column-type", move |col: SteelVal| -> String {
            let snap = shared.load();
            match (snap.grid.as_ref(), steel_col(&col)) {
                (Some(g), Some(c)) => g.col_types.get(c).cloned().unwrap_or_else(|| "text".into()),
                _ => String::new(),
            }
        });
    }
    // --- data -------------------------------------------------------------
    let p = push(queue);
    reg.register_fn(
        "grid-set-cell",
        move |row: SteelVal, col: SteelVal, value: SteelVal| {
            p(GridOp::SetCells(vec![match steel_cell_text(&value) {
                Some(text) => (steel_index(&row), steel_col(&col), text),
                None => (None, None, String::new()),
            }]))
        },
    );
    let p = push(queue);
    reg.register_fn("grid-set-cells", move |cells: SteelVal| {
        let items = steel_list(&cells).unwrap_or_default();
        let triples = items
            .iter()
            .map(|item| {
                // An item that isn't `(row col value)` with a value a cell
                // can hold is reported as skipped (invalid position).
                let parts = steel_list(item).unwrap_or_default();
                match (
                    parts.first(),
                    parts.get(1),
                    parts.get(2).and_then(steel_cell_text),
                ) {
                    (Some(r), Some(c), Some(text)) => (steel_index(r), steel_col(c), text),
                    _ => (None, None, String::new()),
                }
            })
            .collect();
        p(GridOp::SetCells(triples))
    });
    let p = push(queue);
    reg.register_fn(
        "grid-insert-rows",
        move |at: SteelVal, NonNeg(n): NonNeg| {
            p(GridOp::InsertRows {
                at: steel_index(&at),
                n,
            })
        },
    );
    let p = push(queue);
    reg.register_fn(
        "grid-delete-rows",
        move |at: SteelVal, NonNeg(n): NonNeg| {
            p(GridOp::DeleteRows {
                at: steel_index(&at),
                n,
            })
        },
    );
    let p = push(queue);
    reg.register_fn(
        "grid-insert-columns",
        move |at: SteelVal, NonNeg(n): NonNeg| {
            p(GridOp::InsertCols {
                at: steel_col(&at),
                n,
            })
        },
    );
    let p = push(queue);
    reg.register_fn(
        "grid-delete-columns",
        move |at: SteelVal, NonNeg(n): NonNeg| {
            p(GridOp::DeleteCols {
                at: steel_col(&at),
                n,
            })
        },
    );
    let p = push(queue);
    reg.register_fn("grid-sort", move |col: SteelVal, descending: bool| {
        p(GridOp::Sort {
            col: steel_col(&col),
            descending,
            kind: crate::grid::model::SortKind::Auto,
        })
    });
    // `(grid-sort-as col descending? 'numeric)` — compare as 'alpha,
    // 'numeric, 'time, or 'auto (the column's inferred type).
    let p = push(queue);
    reg.register_fn(
        "grid-sort-as",
        move |col: SteelVal, descending: bool, kind: SteelVal| -> Result<(), String> {
            use crate::grid::model::SortKind;
            let kind = match steel_name(&kind).as_str() {
                "auto" | "" => SortKind::Auto,
                "alpha" | "text" => SortKind::Alpha,
                "numeric" | "number" => SortKind::Numeric,
                "time" | "date" => SortKind::Time,
                other => {
                    return Err(format!(
                        "grid-sort-as: sort as 'alpha, 'numeric, 'time or 'auto, not {other:?}"
                    ));
                }
            };
            p(GridOp::Sort {
                col: steel_col(&col),
                descending,
                kind,
            });
            Ok(())
        },
    );
    let p = push(queue);
    reg.register_fn(
        "grid-read-range",
        move |range: String, callback: SteelVal| {
            p(GridOp::ReadRange {
                range,
                callback: steel_name(&callback),
            })
        },
    );
    // --- view -------------------------------------------------------------
    let p = push(queue);
    reg.register_fn("grid-move-to", move |row: SteelVal, col: SteelVal| {
        p(GridOp::MoveTo {
            row: steel_index(&row),
            col: steel_col(&col),
        })
    });
    let p = push(queue);
    reg.register_fn(
        "grid-select-range",
        move |r0: SteelVal, c0: SteelVal, r1: SteelVal, c1: SteelVal| {
            p(GridOp::Select {
                r0: steel_index(&r0),
                c0: steel_col(&c0),
                r1: steel_index(&r1),
                c1: steel_col(&c1),
            })
        },
    );
    let p = push(queue);
    reg.register_fn(
        "grid-set-column-width",
        move |col: SteelVal, px: SteelVal| {
            p(GridOp::ColumnWidth {
                col: steel_col(&col),
                px: steel_f64(&px).unwrap_or(f64::NAN),
            })
        },
    );
    let p = push(queue);
    reg.register_fn("grid-set-row-height", move |row: SteelVal, px: SteelVal| {
        p(GridOp::RowHeight {
            row: steel_index(&row),
            px: steel_f64(&px).unwrap_or(f64::NAN),
        })
    });
    let p = push(queue);
    reg.register_fn("grid-freeze-rows", move |NonNeg(n): NonNeg| {
        p(GridOp::Freeze(n))
    });
    let p = push(queue);
    reg.register_fn("grid-set-header", move |on: bool| p(GridOp::SetHeader(on)));
    let p = push(queue);
    reg.register_fn("grid-set-zoom", move |percent: SteelVal| {
        p(GridOp::Zoom(steel_f64(&percent).unwrap_or(f64::NAN)))
    });
    // --- styling ----------------------------------------------------------
    let p = push(queue);
    reg.register_fn("grid-column-face", move |col: SteelVal, face: SteelVal| {
        p(GridOp::ColumnFace {
            col: steel_col(&col),
            face: steel_name(&face),
        })
    });
    let p = push(queue);
    reg.register_fn(
        "grid-format-column",
        move |col: SteelVal, format: String| {
            p(GridOp::FormatColumn {
                col: steel_col(&col),
                format,
            })
        },
    );
    for (name, row) in [
        ("grid-highlight-rule", false),
        ("grid-row-highlight-rule", true),
    ] {
        let p = push(queue);
        reg.register_fn(
            name,
            move |col: SteelVal, op: SteelVal, value: SteelVal, face: SteelVal| {
                p(GridOp::HighlightRule {
                    col: steel_col(&col),
                    op: steel_name(&op),
                    value: steel_value_text(&value),
                    face: steel_name(&face),
                    row,
                })
            },
        );
    }
    let p = push(queue);
    reg.register_fn("grid-clear-styles", move || p(GridOp::ClearStyles));
    let p = push(queue);
    reg.register_fn(
        "grid-set-cell-face",
        move |row: SteelVal, col: SteelVal, face: SteelVal| {
            p(GridOp::CellFace {
                row: steel_index(&row),
                col: steel_col(&col),
                face: steel_name(&face),
            })
        },
    );
    let p = push(queue);
    reg.register_fn("grid-clear-cell-faces", move || p(GridOp::ClearCellFaces));
    // `(grid-set-cell-bold row col #t)`: make a cell bold, or not — hand-set
    // formatting, as `grid-toggle-bold` does. It follows the cell through
    // sorts and row/column edits and undoes; it saves only to .xlsx (a
    // formatted CSV saves as .xlsx).
    let p = push(queue);
    reg.register_fn(
        "grid-set-cell-bold",
        move |row: SteelVal, col: SteelVal, on: bool| {
            p(GridOp::CellFormat {
                row: steel_index(&row),
                col: steel_col(&col),
                bold: Some(on),
                text: None,
                fill: None,
                num: None,
                wrap: None,
                bad_color: None,
            })
        },
    );
    // `(grid-set-cell-wrap row col #t)`: Wrap Text on a cell, or off — as
    // `grid-toggle-wrap` does. The text wraps within the column and the row
    // grows to fit, unless its height was set by hand.
    let p = push(queue);
    reg.register_fn(
        "grid-set-cell-wrap",
        move |row: SteelVal, col: SteelVal, on: bool| {
            p(GridOp::CellFormat {
                row: steel_index(&row),
                col: steel_col(&col),
                bold: None,
                text: None,
                fill: None,
                num: None,
                wrap: Some(on),
                bad_color: None,
            })
        },
    );
    // `(grid-set-cell-text-color row col 'red)` /
    // `(grid-set-cell-fill-color row col 'yellow)`: a cell's hand-set text or
    // fill color, one of red orange yellow green blue purple; #f or 'none
    // removes it. A theme face `grid-<color>` overrides a color.
    for (name, fill) in [
        ("grid-set-cell-text-color", false),
        ("grid-set-cell-fill-color", true),
    ] {
        let p = push(queue);
        reg.register_fn(
            name,
            move |row: SteelVal, col: SteelVal, color: SteelVal| {
                let (color, bad_color) = steel_palette_color(&color);
                p(GridOp::CellFormat {
                    row: steel_index(&row),
                    col: steel_col(&col),
                    bold: None,
                    text: (!fill).then_some(color),
                    fill: fill.then_some(color),
                    num: None,
                    wrap: None,
                    bad_color,
                })
            },
        );
    }
    // `(grid-set-cell-number-format row col "currency")`: a cell's hand-set
    // number format — a preset (general number currency percent date text)
    // or an Excel format code like "#,##0.0" or "d mmm yyyy". It changes
    // only how the cell is shown; "general" removes it.
    let p = push(queue);
    reg.register_fn(
        "grid-set-cell-number-format",
        move |row: SteelVal, col: SteelVal, code: SteelVal| {
            p(GridOp::CellFormat {
                row: steel_index(&row),
                col: steel_col(&col),
                bold: None,
                text: None,
                fill: None,
                num: Some(steel_name(&code)),
                wrap: None,
                bad_color: None,
            })
        },
    );
    // `(grid-set-number-format "currency")`: the same on the selection (or
    // the cursor cell), as one undo step — what `grid-number-format` calls.
    let p = push(queue);
    reg.register_fn("grid-set-number-format", move |code: SteelVal| {
        p(GridOp::NumberFormat(steel_name(&code)))
    });
    // `(grid-open-sheet "Costs")`: show another sheet of this grid's .xlsx
    // workbook (any case). The first sheet is the file's own buffer; others
    // open as buffers named like `report.xlsx<Costs>`, reused while open.
    let p = push(queue);
    reg.register_fn("grid-open-sheet", move |name: String| {
        p(GridOp::OpenSheet(name))
    });
    // `(grid-clear-cell-formats)`: remove all hand-set bold, colors and
    // number formats from this grid (one undo step).
    let p = push(queue);
    reg.register_fn("grid-clear-cell-formats", move || {
        p(GridOp::ClearCellFormats)
    });
    // `(grid-stripe-rows 'face)`, `(grid-stripe-rows #t)` (the `grid-stripe`
    // face), `(grid-stripe-rows #f)` (off).
    let p = push(queue);
    reg.register_fn("grid-stripe-rows", move |face: SteelVal| {
        p(GridOp::Stripe(match &face {
            SteelVal::BoolV(false) => None,
            SteelVal::BoolV(true) => Some("grid-stripe".into()),
            other => Some(steel_name(other)),
        }))
    });
    let p = push(queue);
    reg.register_fn("grid-header-face", move |face: SteelVal| {
        p(GridOp::HeaderFace(steel_name(&face)))
    });
    let p = push(queue);
    reg.register_fn(
        "grid-column-align",
        move |col: SteelVal, align: SteelVal| {
            p(GridOp::Align {
                col: steel_col(&col),
                align: steel_name(&align),
            })
        },
    );
    // `(grid-set-column-hidden col #t)`; the `grid-hide-column` /
    // `grid-show-columns` commands act on the cursor's or selected columns.
    let p = push(queue);
    reg.register_fn(
        "grid-set-column-hidden",
        move |col: SteelVal, hidden: bool| {
            p(GridOp::HideColumn {
                col: steel_col(&col),
                hidden,
            })
        },
    );
    // --- filtering ---------------------------------------------------------
    let p = push(queue);
    reg.register_fn(
        "grid-filter-rows",
        move |col: SteelVal, op: SteelVal, value: SteelVal| {
            p(GridOp::Filter {
                col: steel_col(&col),
                op: steel_name(&op),
                value: steel_value_text(&value),
            })
        },
    );
    register_snapshot_read(reg, snapshot, "grid-filtered?", |s| {
        s.grid.as_ref().is_some_and(|g| g.filter.is_some())
    });
    register_snapshot_read(reg, snapshot, "grid-visible-row-count", |s| {
        s.grid.as_ref().map_or(0, |g| g.visible_rows)
    });
    {
        let shared = snapshot.clone();
        reg.register_fn("grid-row-visible?", move |row: SteelVal| -> bool {
            let snap = shared.load();
            let (Some(g), Some(r)) = (snap.grid.as_ref(), steel_index(&row)) else {
                return false;
            };
            match &g.filter {
                Some(o) => r >= o.end || o.shown.binary_search(&r).is_ok(),
                None => true,
            }
        });
    }
    // --- completion --------------------------------------------------------
    let p = push(queue);
    reg.register_fn(
        "grid-column-choices",
        move |col: SteelVal, StringList(values): StringList| {
            p(GridOp::Choices {
                col: steel_col(&col),
                values,
            })
        },
    );
    // --- synchronous cell reads -------------------------------------------
    // From a copy published with the snapshot (re-made only after an edit),
    // for grids up to a million cells; bigger ones use `grid-read-range`.
    let cells_of = |s: &EditorSnapshot| -> Result<Arc<crate::grid::model::CellsSnapshot>, String> {
        let g = s
            .grid
            .as_ref()
            .ok_or("grid: the current buffer is not a grid")?;
        g.cells.clone().ok_or_else(|| {
            "grid: too large (or still loading) for synchronous reads; use grid-read-range"
                .to_string()
        })
    };
    {
        let shared = snapshot.clone();
        reg.register_fn(
            "grid-cell",
            move |row: SteelVal, col: SteelVal| -> Result<String, String> {
                let cells = cells_of(&shared.load())?;
                match (steel_index(&row), steel_col(&col)) {
                    (Some(r), Some(c)) => Ok(cells.cell(r, c).to_string()),
                    _ => Err("grid-cell: invalid row/column".into()),
                }
            },
        );
    }
    {
        let shared = snapshot.clone();
        // Column values below the header (all rows when there is none).
        reg.register_fn(
            "grid-column-values",
            move |col: SteelVal| -> Result<Vec<String>, String> {
                let snap = shared.load();
                let cells = cells_of(&snap)?;
                let header = snap.grid.as_ref().is_some_and(|g| g.header);
                let c = steel_col(&col).ok_or("grid-column-values: invalid column")?;
                Ok((usize::from(header)..cells.rows())
                    .map(|r| cells.cell(r, c).to_string())
                    .collect())
            },
        );
    }
    {
        let shared = snapshot.clone();
        reg.register_fn(
            "grid-row-values",
            move |row: SteelVal| -> Result<Vec<String>, String> {
                let cells = cells_of(&shared.load())?;
                let r = steel_index(&row).ok_or("grid-row-values: invalid row")?;
                Ok((0..cells.cols())
                    .map(|c| cells.cell(r, c).to_string())
                    .collect())
            },
        );
    }
    // --- editor-wide options ----------------------------------------------
    let option = |queue: &Queue| {
        let q = queue.clone();
        move |o: crate::grid::cmd::GridOption| q.push(EditorCommand::GridOption(o))
    };
    use crate::grid::cmd::GridOption as O;
    let o = option(queue);
    reg.register_fn("grid-function-timeout", move |secs: SteelVal| {
        o(O::FunctionTimeout(steel_f64(&secs).unwrap_or(f64::NAN)))
    });
    // `define-grid-function` / `define-grid-helper` (scheme/grid.scm) hand
    // the definition's datum here; it travels to the grid-function
    // interpreter as source text.
    {
        let q = queue.clone();
        reg.register_fn(
            "grid-register-function",
            move |kind: String, name: String, datum: SteelVal| {
                q.push(EditorCommand::GridRegisterFunction {
                    name,
                    source: crate::grid_fn::write_datum(&datum),
                    helper: kind == "helper",
                })
            },
        );
    }
    register_snapshot_read(reg, snapshot, "grid-formula", |s| {
        s.grid
            .as_ref()
            .and_then(|g| g.formula.clone())
            .map_or(SteelVal::BoolV(false), |f| SteelVal::StringV(f.into()))
    });
    let o = option(queue);
    reg.register_fn("grid-default-column-width", move |px: SteelVal| {
        o(O::DefaultColumnWidth(steel_f64(&px).unwrap_or(f64::NAN)))
    });
    let o = option(queue);
    reg.register_fn("grid-max-autofit-width", move |px: SteelVal| {
        o(O::MaxAutofitWidth(steel_f64(&px).unwrap_or(f64::NAN)))
    });
    // `(grid-auto-header 'detect)` sniffs row 1; #t / #f force it.
    let o = option(queue);
    reg.register_fn("grid-auto-header", move |v: SteelVal| {
        o(O::AutoHeader(match v {
            SteelVal::BoolV(b) => Some(b),
            _ => None,
        }))
    });
    let o = option(queue);
    reg.register_fn("grid-truncate-indicator", move |s: String| {
        o(O::TruncateIndicator(s))
    });
}
