//! Scrollbar geometry for the grid's infinite sheet: where the thumb sits
//! for a scroll offset, and which offset a thumb position means.
//!
//! The sheet has no end, so the scrollable extent can't be "the whole
//! sheet" (the thumb would be a sliver that never moves) or "the used
//! range" (scrolling past the data would push the thumb off the track).
//! Like Excel, the extent is `max(used range, offset + viewport)`: the data
//! when you're inside it, and exactly as far as you've scrolled when you're
//! past it, so the thumb sits at the bottom and shrinks as you go on.
//!
//! While the thumb is dragged the extent is frozen at its value on the
//! press (see [`ScrollBar::with_offset`]), as Excel does: otherwise dragging
//! past the data would grow the extent under the pointer and the thumb
//! would run away from it. It's recomputed when the drag ends.
//!
//! Offsets, extents and viewports are unzoomed px along one axis (the
//! units of `GridView::scroll_x` / `scroll_y`); the track is screen px.

/// Thickness of the bar, screen px (the text editor's scroll bar width).
pub const WIDTH: f32 = 6.0;
/// Thickness while hovered or dragged, and of the band that takes clicks.
pub const HOT_WIDTH: f32 = 10.0;
/// The shortest thumb, so it stays grabbable however long the extent.
pub const MIN_THUMB: f32 = 24.0;

/// One scrollbar, along one axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollBar {
    /// Where the track starts and how long it is, screen px.
    pub track_start: f32,
    pub track_len: f32,
    /// The scrollable extent, the viewport, and the scroll offset.
    pub extent: f64,
    pub view: f64,
    pub offset: f64,
}

/// Where along a bar a press landed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BarHit {
    /// On the track before (above / left of) the thumb.
    Before,
    /// On the thumb, this far past its start (screen px).
    Thumb(f32),
    /// On the track after the thumb.
    After,
}

impl ScrollBar {
    /// A bar for content that ends at `used` (the used range, or the
    /// cursor if it's further), seen through `view` at `offset`.
    pub fn new(track_start: f32, track_len: f32, used: f64, view: f64, offset: f64) -> Self {
        let offset = offset.max(0.0);
        let view = view.max(0.0);
        Self {
            track_start,
            track_len: track_len.max(0.0),
            extent: used.max(offset + view),
            view,
            offset,
        }
    }

    /// The same extent and track at another offset: the bar while its
    /// thumb is dragged.
    pub fn with_offset(self, offset: f64) -> Self {
        Self {
            offset: offset.max(0.0),
            ..self
        }
    }

    /// There's anything to scroll to (the bar is shown).
    pub fn scrollable(&self) -> bool {
        self.extent > self.view + 1.0 && self.track_len > 0.0
    }

    /// The furthest offset the thumb can reach.
    pub fn max_offset(&self) -> f64 {
        (self.extent - self.view).max(0.0)
    }

    pub fn thumb_len(&self) -> f32 {
        let frac = if self.extent > 0.0 {
            (self.view / self.extent).min(1.0)
        } else {
            1.0
        };
        ((frac * self.track_len as f64) as f32)
            .max(MIN_THUMB)
            .min(self.track_len)
    }

    /// How far the thumb's start can move along the track.
    fn travel(&self) -> f32 {
        (self.track_len - self.thumb_len()).max(0.0)
    }

    /// Screen position of the thumb's start.
    pub fn thumb_start(&self) -> f32 {
        let max = self.max_offset();
        let t = if max > 0.0 {
            (self.offset / max).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.track_start + (t * self.travel() as f64) as f32
    }

    /// The offset that puts the thumb's start at screen position `start`
    /// (clamped to the track: the inverse of [`Self::thumb_start`]).
    pub fn offset_at(&self, start: f32) -> f64 {
        let travel = self.travel();
        if travel <= 0.0 {
            return 0.0;
        }
        let t = ((start - self.track_start) / travel).clamp(0.0, 1.0) as f64;
        t * self.max_offset()
    }

    /// What a press at screen position `at` along the bar hit.
    pub fn hit(&self, at: f32) -> BarHit {
        let start = self.thumb_start();
        if at < start {
            BarHit::Before
        } else if at < start + self.thumb_len() {
            BarHit::Thumb(at - start)
        } else {
            BarHit::After
        }
    }

    /// One page from the current offset, toward the end when `forward`
    /// (paging forward may run past the extent: the sheet goes on).
    pub fn page(&self, forward: bool) -> f64 {
        if forward {
            self.offset + self.view
        } else {
            (self.offset - self.view).max(0.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(used: f64, offset: f64) -> ScrollBar {
        // A 500 px track over a 500 px viewport: unzoomed, no frozen rows.
        ScrollBar::new(100.0, 500.0, used, 500.0, offset)
    }

    #[test]
    fn extent_is_used_range_or_scrolled_viewport() {
        assert_eq!(bar(2000.0, 0.0).extent, 2000.0);
        assert_eq!(bar(2000.0, 1000.0).extent, 2000.0);
        // Past the data: exactly as far as scrolled.
        assert_eq!(bar(2000.0, 5000.0).extent, 5500.0);
        // Data that fits: no bar until you scroll.
        assert!(!bar(300.0, 0.0).scrollable());
        assert!(bar(300.0, 40.0).scrollable());
    }

    #[test]
    fn thumb_is_proportional_with_a_floor() {
        assert_eq!(bar(1000.0, 0.0).thumb_len(), 250.0);
        // A million rows deep: the floor, not a 1 px sliver.
        assert_eq!(bar(20_000_000.0, 0.0).thumb_len(), MIN_THUMB);
        let b = ScrollBar::new(0.0, 10.0, 1e6, 500.0, 0.0);
        assert_eq!(b.thumb_len(), 10.0, "never longer than the track");
    }

    #[test]
    fn edges_map_to_the_ends_of_the_track() {
        let b = bar(2000.0, 0.0);
        assert_eq!(b.thumb_start(), 100.0);
        let end = b.with_offset(b.max_offset());
        assert_eq!(end.thumb_start() + end.thumb_len(), 600.0);
        // Past the data the thumb rests at the end.
        let past = bar(2000.0, 9000.0);
        assert_eq!(past.thumb_start() + past.thumb_len(), 600.0);
        // Dragged beyond either end of the track: clamped.
        assert_eq!(b.offset_at(-50.0), 0.0);
        assert_eq!(b.offset_at(10_000.0), b.max_offset());
    }

    #[test]
    fn thumb_and_offset_round_trip() {
        for used in [800.0, 2000.0, 1e5, 2e7] {
            let b = bar(used, 0.0);
            for i in 0..=20 {
                let off = b.max_offset() * i as f64 / 20.0;
                let back = b.offset_at(b.with_offset(off).thumb_start());
                let tol = b.max_offset() / b.travel() as f64 * 1e-3 + 1e-6;
                assert!((back - off).abs() <= tol, "used {used}: {off} -> {back}");
            }
        }
    }

    #[test]
    fn dragging_keeps_the_extent_from_the_press() {
        // Scrolled past the data, then the thumb dragged up: the extent
        // stays put so the thumb tracks the pointer.
        let b = bar(2000.0, 3000.0);
        let up = b.with_offset(b.offset_at(b.thumb_start() - 100.0));
        assert_eq!(up.extent, b.extent);
        assert!((up.thumb_start() - (b.thumb_start() - 100.0)).abs() < 1e-3);
    }

    #[test]
    fn hits_and_pages() {
        let b = bar(2000.0, 750.0);
        let (s, l) = (b.thumb_start(), b.thumb_len());
        assert_eq!(b.hit(s - 1.0), BarHit::Before);
        assert_eq!(b.hit(s + 5.0), BarHit::Thumb(5.0));
        assert_eq!(b.hit(s + l + 1.0), BarHit::After);
        assert_eq!(b.page(true), 1250.0);
        assert_eq!(b.page(false), 250.0);
        assert_eq!(bar(2000.0, 100.0).page(false), 0.0);
        // Paging forward from the end goes on into empty sheet.
        assert_eq!(bar(2000.0, 1500.0).page(true), 2000.0);
    }

    #[test]
    fn zero_length_inputs_do_not_divide_by_zero() {
        let b = ScrollBar::new(0.0, 0.0, 0.0, 0.0, 0.0);
        assert!(!b.scrollable());
        assert_eq!(b.offset_at(5.0), 0.0);
        assert_eq!(b.thumb_start(), 0.0);
    }
}
