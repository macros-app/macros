//! Window/pane (split) methods for [`Editor`], plus the split-layout tree
//! ([`WindowLayout`]) and its methods.

use super::*;

/// The axis a split divides its area along.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SplitAxis {
    /// Side by side (split right, `C-x 3`): children share the width.
    Horizontal,
    /// Stacked (split below, `C-x 2`): children share the height.
    Vertical,
}

impl SplitAxis {
    /// From the `vertical` flag of a [`LayoutSpec`] (`true` = stacked).
    pub(super) fn from_vertical(vertical: bool) -> Self {
        if vertical {
            Self::Vertical
        } else {
            Self::Horizontal
        }
    }

    /// Whether this is the stacked axis (the [`LayoutSpec`] `vertical` flag).
    pub(super) fn is_vertical(self) -> bool {
        self == Self::Vertical
    }
}

/// Which child of a [`WindowLayout::Split`] a path step descends into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Child {
    A,
    B,
}

/// The split layout: a binary tree whose leaves are pane ids. `ratio` is the
/// fraction of the split's extent given to child `a` (the other gets `1 - ratio`);
/// resize commands nudge it.
pub(super) enum WindowLayout {
    Leaf(usize),
    Split {
        axis: SplitAxis,
        /// Fraction `[0.1, 0.9]` of the area given to `a`.
        ratio: f32,
        a: Box<WindowLayout>,
        b: Box<WindowLayout>,
    },
}

/// Bounding box of a set of rects, or `None` if empty.
pub(super) fn union_bounds(rects: &[Bounds<Pixels>]) -> Option<Bounds<Pixels>> {
    let first = *rects.first()?;
    let mut left = first.left();
    let mut top = first.top();
    let mut right = first.right();
    let mut bottom = first.bottom();
    for r in &rects[1..] {
        left = left.min(r.left());
        top = top.min(r.top());
        right = right.max(r.right());
        bottom = bottom.max(r.bottom());
    }
    Some(Bounds::from_corners(point(left, top), point(right, bottom)))
}

/// A direction for window motion (`C-w h/j/k/l`, `windmove-*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dir {
    Left,
    Down,
    Up,
    Right,
}

/// A pane's rectangle in layout units: `(left, top, right, bottom)` within the
/// unit square — computed from the split tree alone, so it never depends on
/// what was last painted.
pub(super) type URect = (f32, f32, f32, f32);

impl WindowLayout {
    /// Whether this subtree contains pane `id`.
    pub(super) fn contains(&self, id: usize) -> bool {
        match self {
            WindowLayout::Leaf(x) => *x == id,
            WindowLayout::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    /// In-order list of pane ids (left-to-right / top-to-bottom) — the
    /// `other-window` cycle order.
    pub(super) fn leaves(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<usize>) {
        match self {
            WindowLayout::Leaf(id) => out.push(*id),
            WindowLayout::Split { a, b, .. } => {
                a.collect_leaves(out);
                b.collect_leaves(out);
            }
        }
    }

    /// Nudge the ratio of the deepest `axis`-matching split on the path to
    /// `pane`, enlarging the side that contains it by `delta`. Returns whether a
    /// matching split was found.
    pub(super) fn resize(&mut self, pane: usize, axis: SplitAxis, delta: f32) -> bool {
        let WindowLayout::Split {
            axis: own,
            ratio,
            a,
            b,
        } = self
        else {
            return false;
        };
        let in_a = a.contains(pane);
        let child = if in_a { a } else { b };
        // Prefer a deeper matching split; otherwise adjust this one.
        if child.resize(pane, axis, delta) {
            return true;
        }
        if *own == axis {
            let signed = if in_a { delta } else { -delta };
            *ratio = (*ratio + signed).clamp(0.1, 0.9);
            true
        } else {
            false
        }
    }

    /// The subtree at `path` (a sequence of child descents from the root), or
    /// `None` if the path doesn't lead to a node.
    pub(super) fn node_at(&self, path: &[Child]) -> Option<&WindowLayout> {
        match path.split_first() {
            None => Some(self),
            Some((&child, rest)) => match self {
                WindowLayout::Split { a, b, .. } => match child {
                    Child::A => a.node_at(rest),
                    Child::B => b.node_at(rest),
                },
                WindowLayout::Leaf(_) => None,
            },
        }
    }

    /// Set the ratio of the Split node at `path` (clamped). Returns whether it
    /// hit a Split there.
    pub(super) fn set_ratio_at(&mut self, path: &[Child], new_ratio: f32) -> bool {
        match (path.split_first(), self) {
            (None, WindowLayout::Split { ratio, .. }) => {
                *ratio = new_ratio.clamp(0.1, 0.9);
                true
            }
            (Some((&child, rest)), WindowLayout::Split { a, b, .. }) => match child {
                Child::A => a.set_ratio_at(rest, new_ratio),
                Child::B => b.set_ratio_at(rest, new_ratio),
            },
            (_, WindowLayout::Leaf(_)) => false,
        }
    }

    /// Replace the leaf for `target` with a split of `target` (ratio
    /// `split_ratio`) and `new_id`.
    pub(super) fn split(
        self,
        target: usize,
        axis: SplitAxis,
        new_id: usize,
        split_ratio: f32,
    ) -> WindowLayout {
        match self {
            WindowLayout::Leaf(id) if id == target => WindowLayout::Split {
                axis,
                ratio: split_ratio,
                a: Box::new(WindowLayout::Leaf(id)),
                b: Box::new(WindowLayout::Leaf(new_id)),
            },
            WindowLayout::Leaf(id) => WindowLayout::Leaf(id),
            WindowLayout::Split {
                axis: own,
                ratio,
                a,
                b,
            } => WindowLayout::Split {
                axis: own,
                ratio,
                a: Box::new(a.split(target, axis, new_id, split_ratio)),
                b: Box::new(b.split(target, axis, new_id, split_ratio)),
            },
        }
    }

    /// Remove the leaf `id`, collapsing its parent split to the sibling. `None`
    /// if `id` was the whole layout.
    pub(super) fn remove(self, id: usize) -> Option<WindowLayout> {
        match self {
            WindowLayout::Leaf(x) if x == id => None,
            WindowLayout::Leaf(x) => Some(WindowLayout::Leaf(x)),
            WindowLayout::Split { axis, ratio, a, b } => match (a.remove(id), b.remove(id)) {
                (Some(a), Some(b)) => Some(WindowLayout::Split {
                    axis,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    /// Every leaf's rectangle within the unit square, in-order.
    pub(super) fn rects(&self) -> Vec<(usize, URect)> {
        let mut out = Vec::new();
        self.collect_rects((0.0, 0.0, 1.0, 1.0), &mut out);
        out
    }

    fn collect_rects(&self, r: URect, out: &mut Vec<(usize, URect)>) {
        match self {
            WindowLayout::Leaf(id) => out.push((*id, r)),
            WindowLayout::Split { axis, ratio, a, b } => {
                let (l, t, rt, bt) = r;
                match axis {
                    SplitAxis::Vertical => {
                        let mid = t + (bt - t) * ratio;
                        a.collect_rects((l, t, rt, mid), out);
                        b.collect_rects((l, mid, rt, bt), out);
                    }
                    SplitAxis::Horizontal => {
                        let mid = l + (rt - l) * ratio;
                        a.collect_rects((l, t, mid, bt), out);
                        b.collect_rects((mid, t, rt, bt), out);
                    }
                }
            }
        }
    }

    /// The pane adjacent to `cur` in direction `dir`: one sharing the edge and
    /// overlapping on the other axis — preferring the one level with `cur`'s
    /// top-left corner (Vim uses the cursor; the corner is a stable proxy).
    pub(super) fn neighbor(&self, cur: usize, dir: Dir) -> Option<usize> {
        const EPS: f32 = 1e-4;
        let rects = self.rects();
        let &(_, (cl, ct, cr, cb)) = rects.iter().find(|(id, _)| *id == cur)?;
        // The reference point on the other axis: just inside the top-left
        // corner, so moving right from a tall window lands on the top-most
        // neighbor.
        let (cx, cy) = (cl + 1e-3, ct + 1e-3);
        let overlap = |a0: f32, a1: f32, b0: f32, b1: f32| (a1.min(b1) - a0.max(b0)).max(0.0);
        rects
            .iter()
            .filter(|(id, _)| *id != cur)
            .filter_map(|&(id, (l, t, r, b))| {
                let (adjacent, ov, center_hit) = match dir {
                    Dir::Left => (
                        (r - cl).abs() < EPS,
                        overlap(t, b, ct, cb),
                        t <= cy && cy <= b,
                    ),
                    Dir::Right => (
                        (l - cr).abs() < EPS,
                        overlap(t, b, ct, cb),
                        t <= cy && cy <= b,
                    ),
                    Dir::Up => (
                        (b - ct).abs() < EPS,
                        overlap(l, r, cl, cr),
                        l <= cx && cx <= r,
                    ),
                    Dir::Down => (
                        (t - cb).abs() < EPS,
                        overlap(l, r, cl, cr),
                        l <= cx && cx <= r,
                    ),
                };
                (adjacent && ov > EPS).then_some((id, center_hit, ov))
            })
            .max_by(|a, b| {
                (a.1, a.2)
                    .partial_cmp(&(b.1, b.2))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(id, _, _)| id)
    }

    /// How many windows this subtree stacks along `axis` — a nested split of
    /// the same orientation adds its children, anything else counts once.
    fn count_along(&self, axis: SplitAxis) -> usize {
        match self {
            WindowLayout::Split {
                axis: own, a, b, ..
            } if *own == axis => a.count_along(axis) + b.count_along(axis),
            _ => 1,
        }
    }

    /// `C-w =` / `balance-windows`: make every window in a row/column the same
    /// size.
    pub(super) fn balance(&mut self) {
        if let WindowLayout::Split { axis, ratio, a, b } = self {
            let na = a.count_along(*axis) as f32;
            let nb = b.count_along(*axis) as f32;
            *ratio = na / (na + nb);
            a.balance();
            b.balance();
        }
    }

    /// `C-w _` / `C-w |`: give `cur` as much of `axis` as allowed.
    pub(super) fn maximize(&mut self, cur: usize, axis: SplitAxis) {
        if let WindowLayout::Split {
            axis: own,
            ratio,
            a,
            b,
        } = self
        {
            let in_a = a.contains(cur);
            if *own == axis {
                *ratio = if in_a { 0.9 } else { 0.1 };
            }
            let child = if in_a { a } else { b };
            child.maximize(cur, axis);
        }
    }

    /// `C-w H/J/K/L`: move `cur` to be a full-height/width window at the far
    /// side of everything else.
    pub(super) fn move_far(self, cur: usize, dir: Dir) -> WindowLayout {
        let Some(rest) = self.remove(cur) else {
            return WindowLayout::Leaf(cur);
        };
        let me = Box::new(WindowLayout::Leaf(cur));
        let rest = Box::new(rest);
        let (axis, a, b) = match dir {
            Dir::Left => (SplitAxis::Horizontal, me, rest),
            Dir::Right => (SplitAxis::Horizontal, rest, me),
            Dir::Up => (SplitAxis::Vertical, me, rest),
            Dir::Down => (SplitAxis::Vertical, rest, me),
        };
        WindowLayout::Split {
            axis,
            ratio: 0.5,
            a,
            b,
        }
    }

    /// Rename leaves by `map` (old id → new id) — swap / rotate window places.
    pub(super) fn relabel(&mut self, map: &HashMap<usize, usize>) {
        match self {
            WindowLayout::Leaf(id) => {
                if let Some(&n) = map.get(id) {
                    *id = n;
                }
            }
            WindowLayout::Split { a, b, .. } => {
                a.relabel(map);
                b.relabel(map);
            }
        }
    }
}

impl Editor {
    pub(super) fn pane(&self, id: usize) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    /// The [`Document`] pane `id` displays: the live active buffer for the
    /// selected pane (and any pane showing the active buffer), else the pane's
    /// inactive document — falling back to the active one if it can't be found.
    fn pane_doc(&self, id: usize) -> &Document {
        match self.pane(id) {
            Some(p) if p.buffer != self.active.name => {
                self.inactive_doc(&p.buffer).unwrap_or(&self.active)
            }
            _ => &self.active,
        }
    }

    /// Whether pane `id` is the selected window (where editing/cursor live).
    pub fn pane_is_selected(&self, id: usize) -> bool {
        id == self.selected_pane
    }

    /// The buffer a pane displays: the live active buffer for the selected pane
    /// (and any pane showing the active buffer), else the inactive [`Document`].
    pub fn pane_buffer(&self, id: usize) -> &Buffer {
        &self.pane_doc(id).buffer
    }

    /// The pane's major (topmost) mode name, resolved like [`pane_buffer`] so an
    /// inactive pane reports its own buffer's mode. Drives per-major-mode display
    /// options such as line numbers (`Self::line_numbers_for`).
    pub fn pane_major_mode(&self, id: usize) -> Option<&str> {
        self.pane_doc(id).modes.names().last().copied()
    }

    pub fn pane_point(&self, id: usize) -> usize {
        if id == self.selected_pane {
            self.cursor_offset()
        } else {
            self.pane(id).map_or(0, |p| p.point)
        }
    }

    pub fn pane_selection(&self, id: usize) -> Range<usize> {
        if id == self.selected_pane {
            // Block-wise Visual draws its own rectangular highlight
            // (`pane_visual_block`), so suppress the contiguous range here.
            if matches!(self.evil, EvilState::VisualBlock) {
                return 0..0;
            }
            // Inclusive of the grapheme under the block cursor in evil Visual.
            self.visual_region()
        } else {
            0..0
        }
    }

    pub fn pane_scroll(&self, id: usize) -> Pixels {
        if id == self.selected_pane {
            self.active.scroll_offset
        } else {
            self.pane(id).map_or(px(0.), |p| p.scroll)
        }
    }

    pub fn pane_highlights(&self, id: usize, range: Range<usize>) -> Vec<HlSpan> {
        let doc = self.pane_doc(id);
        match &doc.highlighter {
            Some(h) => h.spans_in(doc.buffer.rope(), range),
            None => Vec::new(),
        }
    }

    /// Virtual suffix text + face for `line` (selected/active buffer only).
    pub fn line_suffix(&self, line: usize) -> Option<(&str, &str)> {
        self.active
            .overlays
            .suffixes
            .get(&line)
            .map(|(t, f)| (t.as_ref(), f.as_ref()))
    }

    /// Whole-line face override for a pane (only the selected/active buffer
    /// carries Scheme-set line faces today).
    pub fn pane_line_face(&self, id: usize, line: usize) -> Option<&str> {
        self.pane_line_faces(id)
            .get(&line)
            .map(SharedString::as_ref)
    }

    /// The line-face map for pane `id` — the live one if selected, else the
    /// pane's inactive document (so a special buffer keeps its colors when it's
    /// not the focused window — e.g. a tree sidebar).
    pub(super) fn pane_line_faces(&self, id: usize) -> &HashMap<usize, SharedString> {
        &self.pane_doc(id).line_faces
    }

    /// Per-line icons `(char_col, asset_name, tint_face)` for pane `id` — same
    /// active/inactive resolution as the faces. A `None` face means "use the
    /// line's face".
    pub fn pane_line_icons(
        &self,
        id: usize,
        line: usize,
    ) -> impl Iterator<Item = (usize, &str, Option<&str>)> {
        let icons = &self.pane_doc(id).line_icons;
        icons
            .get(&line)
            .into_iter()
            .flatten()
            .map(|(col, name, face)| (*col, name.as_ref(), face.as_ref().map(SharedString::as_ref)))
    }

    /// Gutter dot faces (breakpoint signs, `set-gutter-dot`) for pane `id` —
    /// same active/inactive resolution as the faces, so a source file keeps
    /// its breakpoint dots in an unfocused window.
    pub fn pane_gutter_dots(&self, id: usize) -> &HashMap<usize, SharedString> {
        &self.pane_doc(id).gutter_dots
    }

    /// Working-tree-vs-HEAD gutter marks for pane `id` (git-gutter / diff-hl),
    /// resolved active/inactive like the line faces so an unfocused window keeps
    /// its file's marks.
    pub fn pane_diff_marks(&self, id: usize) -> &HashMap<usize, DiffMark> {
        &self.pane_doc(id).diff_marks
    }

    pub fn pane_diagnostics(&self, id: usize) -> Vec<Diagnostic> {
        if id == self.selected_pane {
            self.active_diagnostics()
        } else {
            Vec::new()
        }
    }

    /// Record a pane's painted bounds (called by every pane's element each
    /// frame) so clicks can be routed to the pane under the cursor.
    pub fn report_pane_bounds(&mut self, id: usize, bounds: Bounds<Pixels>) {
        match self.pane_bounds.iter_mut().find(|(pid, _)| *pid == id) {
            Some(entry) => entry.1 = bounds,
            None => self.pane_bounds.push((id, bounds)),
        }
    }

    /// The pane whose painted bounds contain `pos` (existing panes only).
    pub(super) fn pane_at_position(&self, pos: Point<Pixels>) -> Option<usize> {
        self.pane_bounds
            .iter()
            .find(|(id, b)| b.contains(&pos) && self.panes.iter().any(|p| p.id == *id))
            .map(|(id, _)| *id)
    }

    /// Save the live point/scroll into the selected pane before leaving it.
    pub(super) fn save_selected_pane(&mut self) {
        let (point, scroll) = (self.cursor_offset(), self.active.scroll_offset);
        let selected = self.selected_pane;
        if let Some(pane) = self.panes.iter_mut().find(|p| p.id == selected) {
            pane.point = point;
            pane.scroll = scroll;
        }
    }

    /// `C-x 2` / `C-x 3`: split the selected window, the new pane cloning the
    /// current buffer/point/scroll.
    pub(super) fn split_window(&mut self, axis: SplitAxis, cx: &mut Context<Self>) {
        self.split_window_ratio(axis, 0.5, cx);
    }

    /// Like `split_window`, but the existing (selected) pane keeps `ratio` of the
    /// extent — e.g. 0.25 for a narrow sidebar created by `open-file-other-window`.
    pub(super) fn split_window_ratio(
        &mut self,
        axis: SplitAxis,
        ratio: f32,
        cx: &mut Context<Self>,
    ) {
        // Same bounds every other split path uses; NaN falls back to even.
        let ratio = if ratio.is_finite() {
            ratio.clamp(0.1, 0.9)
        } else {
            0.5
        };
        self.save_selected_pane();
        let new_id = self.next_pane_id;
        self.next_pane_id += 1;
        let buffer = self
            .pane(self.selected_pane)
            .map_or_else(|| self.active.name.clone(), |p| p.buffer.clone());
        self.panes.push(Pane {
            id: new_id,
            buffer,
            point: self.cursor_offset(),
            scroll: self.active.scroll_offset,
        });
        let layout = std::mem::replace(&mut self.layout, WindowLayout::Leaf(self.selected_pane));
        self.layout = layout.split(self.selected_pane, axis, new_id, ratio);
        cx.notify();
    }

    /// `C-x o`: select the next pane in cycle order, activating its buffer.
    pub(super) fn other_window(&mut self, cx: &mut Context<Self>) {
        self.cycle_window(1, cx);
    }

    /// Select the window `step` places along the layout order (wrapping).
    fn cycle_window(&mut self, step: isize, cx: &mut Context<Self>) {
        let leaves = self.layout.leaves();
        if leaves.len() < 2 {
            return;
        }
        let pos = leaves
            .iter()
            .position(|&id| id == self.selected_pane)
            .unwrap_or(0);
        let next = (pos as isize + step).rem_euclid(leaves.len() as isize) as usize;
        self.select_pane(leaves[next], cx);
    }

    /// Make pane `id` the selected window: save the old pane, activate the
    /// target's buffer if different, and restore the target's point/scroll.
    pub(super) fn select_pane(&mut self, id: usize, cx: &mut Context<Self>) {
        // An id that no longer names a pane (a stale `select-window` queued
        // before its window was deleted) must not become the selection: no
        // pane would then own the cursor or the keyboard.
        if id == self.selected_pane || self.pane(id).is_none() {
            return;
        }
        self.save_selected_pane();
        self.last_pane = Some(self.selected_pane);
        let target_buffer = self.pane(id).map(|p| p.buffer.clone()).unwrap_or_default();
        let restore = self.pane(id).map(|p| (p.point, p.scroll));
        self.selected_pane = id;
        if target_buffer != self.active.name && !target_buffer.is_empty() {
            // Pane focus moving is not a deliberate buffer switch: the
            // perspectives title bar must not follow a window dance (e.g.
            // compile re-selecting panes) into another project.
            self.switch_to_buffer_ex(&target_buffer, false, cx);
        }
        if let Some((point, scroll)) = restore {
            // The saved point may sit inside a multibyte char of the current
            // text (another window edited the buffer meanwhile): snap it.
            let point = crate::script::snap_byte(self.active.buffer.rope(), point);
            self.move_to(point, cx);
            self.active.scroll_offset = scroll;
        }
        cx.notify();
    }

    /// `C-w h/j/k/l`, `windmove-*`: select the window in a direction.
    pub(super) fn window_move(&mut self, dir: Dir, cx: &mut Context<Self>) {
        match self.layout.neighbor(self.selected_pane, dir) {
            Some(id) => self.select_pane(id, cx),
            None => self.echo("No window in that direction", cx),
        }
    }

    /// `C-w W`: the previous window in cycle order.
    pub(super) fn previous_window(&mut self, cx: &mut Context<Self>) {
        self.cycle_window(-1, cx);
    }

    /// `C-w p`: the previously selected window.
    pub(super) fn last_window(&mut self, cx: &mut Context<Self>) {
        match self.last_pane.filter(|id| self.pane(*id).is_some()) {
            Some(id) => self.select_pane(id, cx),
            None => self.echo("No previous window", cx),
        }
    }

    /// `C-w t` / `C-w b`: the top-left / bottom-right window.
    pub(super) fn window_corner(&mut self, last: bool, cx: &mut Context<Self>) {
        let leaves = self.layout.leaves();
        let target = if last { leaves.last() } else { leaves.first() };
        if let Some(&id) = target {
            self.select_pane(id, cx);
        }
    }

    /// `C-w =` / `balance-windows`.
    pub(super) fn balance_windows(&mut self, cx: &mut Context<Self>) {
        self.layout.balance();
        cx.notify();
    }

    /// `C-w _` (height) / `C-w |` (width).
    pub(super) fn maximize_window(&mut self, axis: SplitAxis, cx: &mut Context<Self>) {
        self.layout.maximize(self.selected_pane, axis);
        cx.notify();
    }

    /// `C-w H/J/K/L`: move the window to the far left/bottom/top/right.
    pub(super) fn move_window_far(&mut self, dir: Dir, cx: &mut Context<Self>) {
        let cur = self.selected_pane;
        let layout = std::mem::replace(&mut self.layout, WindowLayout::Leaf(cur));
        self.layout = layout.move_far(cur, dir);
        cx.notify();
    }

    /// `C-w x`: exchange this window with the next one (the cursor stays with
    /// its buffer, which moves to the other place).
    pub(super) fn exchange_window(&mut self, cx: &mut Context<Self>) {
        let leaves = self.layout.leaves();
        if leaves.len() < 2 {
            return;
        }
        let cur = self.selected_pane;
        let pos = leaves.iter().position(|&id| id == cur).unwrap_or(0);
        let other = leaves[(pos + 1) % leaves.len()];
        let map = HashMap::from([(cur, other), (other, cur)]);
        self.layout.relabel(&map);
        cx.notify();
    }

    /// `C-w r` / `C-w R`: rotate the windows (their places shift by one).
    pub(super) fn rotate_windows(&mut self, downward: bool, cx: &mut Context<Self>) {
        let leaves = self.layout.leaves();
        let n = leaves.len();
        if n < 2 {
            return;
        }
        // The window at place i moves to place i+1 (downward) or i-1.
        let map: HashMap<usize, usize> = (0..n)
            .map(|i| {
                let from = if downward {
                    (i + n - 1) % n
                } else {
                    (i + 1) % n
                };
                (leaves[i], leaves[from])
            })
            .collect();
        self.layout.relabel(&map);
        cx.notify();
    }

    /// `C-w n` / `:new`: split and edit a new, empty buffer.
    pub(super) fn new_window_buffer(&mut self, axis: SplitAxis, cx: &mut Context<Self>) {
        self.split_window(axis, cx);
        let mut n = 1;
        let name = loop {
            let candidate = if n == 1 {
                "[No Name]".to_string()
            } else {
                format!("[No Name]<{n}>")
            };
            if !self.has_buffer(&candidate) {
                break candidate;
            }
            n += 1;
        };
        // Show the new buffer in the NEW pane (the split's second half).
        let leaves = self.layout.leaves();
        if let Some(&newest) = leaves.iter().max() {
            self.select_pane(newest, cx);
        }
        self.switch_to_doc(make_document(name, None, ""));
        cx.notify();
    }

    /// `:wincmd {c}` — the `C-w {c}` window command, by letter.
    pub(super) fn run_wincmd(&mut self, arg: &str, cx: &mut Context<Self>) {
        match arg.trim() {
            "h" => self.window_move(Dir::Left, cx),
            "j" => self.window_move(Dir::Down, cx),
            "k" => self.window_move(Dir::Up, cx),
            "l" => self.window_move(Dir::Right, cx),
            "w" => self.other_window(cx),
            "W" => self.previous_window(cx),
            "p" => self.last_window(cx),
            "t" => self.window_corner(false, cx),
            "b" => self.window_corner(true, cx),
            "s" | "S" => self.split_window(SplitAxis::Vertical, cx),
            "v" => self.split_window(SplitAxis::Horizontal, cx),
            "n" => self.new_window_buffer(SplitAxis::Vertical, cx),
            "c" | "q" => self.delete_window(cx),
            "o" => self.delete_other_windows(cx),
            "=" => self.balance_windows(cx),
            "_" => self.maximize_window(SplitAxis::Vertical, cx),
            "|" => self.maximize_window(SplitAxis::Horizontal, cx),
            "+" => self.resize_window(SplitAxis::Vertical, 0.05, cx),
            "-" => self.resize_window(SplitAxis::Vertical, -0.05, cx),
            ">" => self.resize_window(SplitAxis::Horizontal, 0.05, cx),
            "<" => self.resize_window(SplitAxis::Horizontal, -0.05, cx),
            "H" => self.move_window_far(Dir::Left, cx),
            "J" => self.move_window_far(Dir::Down, cx),
            "K" => self.move_window_far(Dir::Up, cx),
            "L" => self.move_window_far(Dir::Right, cx),
            "x" => self.exchange_window(cx),
            "r" => self.rotate_windows(true, cx),
            "R" => self.rotate_windows(false, cx),
            other => self.echo(format!("Unknown wincmd: {other}"), cx),
        }
    }

    /// `C-x 0`: delete the selected window, selecting a neighbor.
    pub(super) fn delete_window(&mut self, cx: &mut Context<Self>) {
        let leaves = self.layout.leaves();
        if leaves.len() < 2 {
            self.echo("Can't delete the sole window", cx);
            return;
        }
        let removed = self.selected_pane;
        let pos = leaves.iter().position(|&id| id == removed).unwrap_or(0);
        let neighbor = leaves[if pos == 0 { 1 } else { pos - 1 }];
        let layout = std::mem::replace(&mut self.layout, WindowLayout::Leaf(neighbor));
        if let Some(l) = layout.remove(removed) {
            self.layout = l;
        }
        self.panes.retain(|p| p.id != removed);
        self.pane_bounds.retain(|(id, _)| *id != removed);
        self.select_pane(neighbor, cx);
    }

    /// `C-x 1`: delete all other windows, keeping the selected one.
    pub(super) fn delete_other_windows(&mut self, cx: &mut Context<Self>) {
        self.save_selected_pane();
        let keep = self.selected_pane;
        self.panes.retain(|p| p.id == keep);
        self.layout = WindowLayout::Leaf(keep);
        cx.notify();
    }

    /// Enlarge/shrink the selected window along an axis (Emacs `enlarge-window`
    /// etc.): nudge the nearest matching split's ratio toward/away from this pane.
    pub(super) fn resize_window(&mut self, axis: SplitAxis, delta: f32, cx: &mut Context<Self>) {
        if self.layout.resize(self.selected_pane, axis, delta) {
            cx.notify();
        }
    }

    /// Recursively render the split layout into nested flex containers. `path`
    /// (a/b descents from the root) identifies each split so its divider can
    /// start a resize drag.
    pub(super) fn render_layout(
        &self,
        layout: &WindowLayout,
        path: &[Child],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.global::<Theme>();
        let divider = theme.color("window-divider", theme.color("dim", theme.foreground()));
        let divider_w = px(cx.global::<crate::settings::Settings>().divider_width());
        match layout {
            WindowLayout::Leaf(id) => {
                // A terminal pane draws the grid via `TerminalElement`; everything
                // else uses the text `EditorElement`.
                let content = if self.pane_grid(*id).is_some() {
                    super::grid_element::GridElement {
                        editor: cx.entity(),
                        pane: *id,
                    }
                    .into_any_element()
                } else if let Some(tid) = self.pane_terminal_id(*id) {
                    crate::element::TerminalElement {
                        editor: cx.entity(),
                        pane: *id,
                        id: tid,
                    }
                    .into_any_element()
                } else {
                    EditorElement {
                        editor: cx.entity(),
                        pane: *id,
                    }
                    .into_any_element()
                };
                div().size_full().child(content).into_any_element()
            }
            WindowLayout::Split { axis, ratio, a, b } => {
                let axis = *axis;
                let vertical = axis == SplitAxis::Vertical;
                let a = self.render_layout(a, &[path, &[Child::A]].concat(), cx);
                let b = self.render_layout(b, &[path, &[Child::B]].concat(), cx);
                // A grabbable divider (4px hit area) that begins a drag-resize of
                // this split; the editor's mouse-move then tracks the ratio.
                let drag_path = path.to_vec();
                let divider_bar = {
                    let bar = if vertical {
                        div()
                            .h(divider_w)
                            .w_full()
                            .cursor(CursorStyle::ResizeUpDown)
                    } else {
                        div()
                            .w(divider_w)
                            .h_full()
                            .cursor(CursorStyle::ResizeLeftRight)
                    };
                    bar.bg(divider).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, _window, cx| {
                            this.resizing = Some(ResizeDrag {
                                path: drag_path.clone(),
                                axis,
                            });
                            cx.stop_propagation();
                        }),
                    )
                };
                let container = if vertical {
                    div().flex().flex_col()
                } else {
                    div().flex().flex_row()
                };
                // Child `a` takes `ratio` of the extent; `b` fills the remainder.
                let first = if vertical {
                    div().w_full().h(gpui::relative(*ratio))
                } else {
                    div().h_full().w(gpui::relative(*ratio))
                };
                container
                    .size_full()
                    .child(first.min_w(px(0.)).min_h(px(0.)).child(a))
                    .child(divider_bar)
                    .child(div().flex_1().min_w(px(0.)).min_h(px(0.)).child(b))
                    .into_any_element()
            }
        }
    }

    /// Handle a non-text key (Enter/Tab/Backspace/Delete) in a special buffer:
    /// resolve it against the buffer's keymap and run any binding. Returns
    /// whether the active buffer is special (so callers skip the edit fallback).
    pub(super) fn special_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        if self.active.special.is_none() {
            return false;
        }
        // Escape ends a keyboard selection before the buffer sees it.
        if self.special_selecting() && key == "escape" {
            self.special_select_end(cx);
            return true;
        }
        if let Some(expr) = self
            .keymap
            .resolve(&self.active.modes, key)
            .map(str::to_string)
        {
            self.run_script(&expr, cx);
        }
        true // read-only: the key is consumed whether or not it was bound
    }

    /// In a NORMAL (editable) buffer, let an active mode rebind a special key
    /// (Enter/Tab) — Emacs's major modes do this (comint RET, etc.). Returns
    /// whether a binding ran; if not, the caller does the default edit.
    pub(super) fn mode_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        match self
            .keymap
            .resolve(&self.active.modes, key)
            .map(str::to_string)
        {
            Some(expr) => {
                self.run_script(&expr, cx);
                true
            }
            None => false,
        }
    }

    /// Let the active modal keymap claim an OS-special key (Tab, Enter, the
    /// arrows, Home/End, Backspace, Delete).
    ///
    /// Those keys are bound as gpui actions, which dispatch BEFORE the raw
    /// key-down listener, so they never reach [`Editor::on_key_down`] and never
    /// reach the evil/helix keymaps. Without this hook `(define-key
    /// "evil-normal" "tab" …)` is silently dead: it resolves against a keymap
    /// nothing consults. Called first from each `act_*` handler, so a modal
    /// binding wins over the built-in behaviour; unbound falls through
    /// untouched, which keeps every default intact.
    ///
    /// Only consulted while the modal layer is intercepting, so this is inert
    /// under plain Emacs keys.
    pub(super) fn modal_action_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        if !self.evil_intercepts() || self.active.special.is_some() {
            return false;
        }
        let state = self.evil_keymap();
        let major: Option<String> = self.active.modes.names().last().map(|s| s.to_string());
        match self
            .keymap
            .resolve_evil(state, major.as_deref(), key)
            .map(str::to_string)
        {
            Some(expr) => {
                self.dispatch_recorded(&expr, cx);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Child, Dir, SplitAxis, WindowLayout, union_bounds};
    use gpui::{Bounds, point, px};
    use std::collections::HashMap;

    #[test]
    fn window_layout_resize_nudges_nearest_matching_split() {
        // [0 | 1] horizontal split (ratio 0.5).
        let mut l = WindowLayout::Leaf(0).split(0, SplitAxis::Horizontal, 1, 0.5);
        // Enlarge pane 0 horizontally: it's in `a`, so ratio grows.
        assert!(l.resize(0, SplitAxis::Horizontal, 0.1));
        if let WindowLayout::Split { ratio, .. } = &l {
            assert!((*ratio - 0.6).abs() < 1e-6, "ratio grew toward pane 0");
        } else {
            panic!("expected split");
        }
        // Enlarging pane 1 (in `b`) shrinks the ratio.
        assert!(l.resize(1, SplitAxis::Horizontal, 0.1));
        if let WindowLayout::Split { ratio, .. } = &l {
            assert!((*ratio - 0.5).abs() < 1e-6);
        }
        // A vertical resize finds no vertical split here.
        assert!(!l.resize(0, SplitAxis::Vertical, 0.1));
    }

    #[test]
    fn window_layout_split_leaves_and_remove() {
        // One pane.
        let l = WindowLayout::Leaf(0);
        let leaves = l.leaves();
        assert_eq!(leaves, vec![0]);

        // Split 0 below into [0, 1], then split 1 right into [0, [1, 2]].
        let l = l.split(0, SplitAxis::Vertical, 1, 0.5);
        let l = l.split(1, SplitAxis::Horizontal, 2, 0.5);
        let leaves = l.leaves();
        assert_eq!(
            leaves,
            vec![0, 1, 2],
            "in-order traversal is the cycle order"
        );

        // Remove pane 1 — its split collapses to sibling 2.
        let l = l.remove(1).expect("not the whole layout");
        let leaves = l.leaves();
        assert_eq!(leaves, vec![0, 2]);

        // Removing the last leaf yields None.
        let single = WindowLayout::Leaf(7);
        assert!(single.remove(7).is_none());
    }

    #[test]
    fn layout_split_ratio_and_path_helpers() {
        // A narrow split: the existing pane keeps 0.22.
        let l = WindowLayout::Leaf(0).split(0, SplitAxis::Horizontal, 1, 0.22);
        if let WindowLayout::Split { ratio, .. } = &l {
            assert!((*ratio - 0.22).abs() < 1e-6);
        } else {
            panic!("expected split");
        }
        // Nest: [0, [1, 2]] (split 1 below). The inner split is at path [B].
        let mut l = l.split(1, SplitAxis::Vertical, 2, 0.5);
        // node_at: root is a split; [B] is the inner split; [A] is leaf 0.
        assert!(matches!(l.node_at(&[]), Some(WindowLayout::Split { .. })));
        assert!(matches!(
            l.node_at(&[Child::A]),
            Some(WindowLayout::Leaf(0))
        ));
        assert!(matches!(
            l.node_at(&[Child::B]),
            Some(WindowLayout::Split { .. })
        ));
        // set_ratio_at the inner split; clamps to [0.1, 0.9].
        assert!(l.set_ratio_at(&[Child::B], 2.0));
        if let Some(WindowLayout::Split { ratio, .. }) = l.node_at(&[Child::B]) {
            assert!((*ratio - 0.9).abs() < 1e-6, "clamped to 0.9");
        } else {
            panic!("expected inner split");
        }
        // A path into a leaf isn't a split.
        assert!(!l.set_ratio_at(&[Child::A], 0.5));
    }

    #[test]
    fn window_directions_balance_and_moves() {
        use WindowLayout::*;
        // [1 | [2 / 3]]
        let mut l = Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.3,
            a: Box::new(Leaf(1)),
            b: Box::new(Split {
                axis: SplitAxis::Vertical,
                ratio: 0.5,
                a: Box::new(Leaf(2)),
                b: Box::new(Leaf(3)),
            }),
        };
        assert_eq!(l.neighbor(1, Dir::Right), Some(2));
        assert_eq!(l.neighbor(2, Dir::Left), Some(1));
        assert_eq!(l.neighbor(3, Dir::Left), Some(1));
        assert_eq!(l.neighbor(2, Dir::Down), Some(3));
        assert_eq!(l.neighbor(3, Dir::Up), Some(2));
        assert_eq!(l.neighbor(1, Dir::Left), None);
        assert_eq!(l.neighbor(1, Dir::Up), None);
        l.balance();
        if let Split { ratio, .. } = &l {
            assert!((ratio - 0.5).abs() < 1e-6);
        }
        // Three side by side balance to thirds.
        let mut three = Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.8,
            a: Box::new(Leaf(1)),
            b: Box::new(Split {
                axis: SplitAxis::Horizontal,
                ratio: 0.2,
                a: Box::new(Leaf(2)),
                b: Box::new(Leaf(3)),
            }),
        };
        three.balance();
        let rects = three.rects();
        for (_, (l, _, r, _)) in &rects {
            assert!(((r - l) - 1.0 / 3.0).abs() < 1e-5, "{rects:?}");
        }
        // Move 3 to the far left: it spans the full height there.
        let moved = l.move_far(3, Dir::Left);
        let rects = moved.rects();
        let r3 = rects.iter().find(|(id, _)| *id == 3).unwrap().1;
        assert_eq!((r3.0, r3.1, r3.3), (0.0, 0.0, 1.0));
        let leaves = moved.leaves();
        assert_eq!(leaves, vec![3, 1, 2]);
        // Relabel swaps places.
        let mut sw = moved;
        sw.relabel(&HashMap::from([(3, 1), (1, 3)]));
        let leaves = sw.leaves();
        assert_eq!(leaves, vec![1, 3, 2]);
    }

    #[test]
    fn union_bounds_covers_all_rects() {
        let a = Bounds::from_corners(point(px(0.), px(0.)), point(px(10.), px(10.)));
        let b = Bounds::from_corners(point(px(20.), px(5.)), point(px(30.), px(40.)));
        let u = union_bounds(&[a, b]).unwrap();
        assert_eq!(u.left(), px(0.));
        assert_eq!(u.top(), px(0.));
        assert_eq!(u.right(), px(30.));
        assert_eq!(u.bottom(), px(40.));
        assert!(union_bounds(&[]).is_none());
    }
}
