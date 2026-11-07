//! Wide screens (Settings > Appearance): where a page's content goes across the middle area. Centred
//! keeps it at most 1800 wide in the middle, Fill uses the whole width, Split puts Home, Profile and
//! the song lists in two columns once there are 1900 to use. Normal windows look the same in all three.

use crate::config::WideScreens;

/// Centred: the widest a page's content gets.
pub const MAX_W: f32 = 1800.0;
/// Split: the content width from which a page splits in two.
pub const SPLIT_W: f32 = 1900.0;
/// Split: between the two columns.
pub const GAP: f32 = 48.0;
/// Up to this content width cards keep their normal smallest size.
const GROW_FROM: f32 = 1500.0;

/// Where a page's content goes: from `left`, `width` wide; `split` in two (see [`Column::halves`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Column {
    pub left: f32,
    pub width: f32,
    pub split: bool,
}

impl Column {
    /// A page's content in the middle area from `x`, `w` wide, with the page's own `pad` at each side:
    /// Centred keeps it at most [`MAX_W`] wide with the same room on both sides; Split splits it when
    /// the page can (`two`: Home, Profile and the song lists) and it is at least [`SPLIT_W`] wide.
    pub fn new(wide: WideScreens, x: f32, w: f32, pad: f32, two: bool) -> Column {
        let width = (w - pad * 2.0).max(0.0);
        match wide {
            WideScreens::Centred if width > MAX_W => Column { left: x + (w - MAX_W) / 2.0, width: MAX_W, split: false },
            _ => Column { left: x + pad, width, split: wide == WideScreens::Split && two && width >= SPLIT_W },
        }
    }

    pub fn right(&self) -> f32 {
        self.left + self.width
    }

    /// Split's two columns as (left, width): the first `first` wide, the second from [`GAP`] after
    /// it to the right edge.
    pub fn halves(&self, first: f32) -> ((f32, f32), (f32, f32)) {
        let second = self.left + first + GAP;
        ((self.left, first), (second, (self.right() - second).max(0.0)))
    }
}

/// A card's smallest size on a page whose content is `page_w` wide: `normal` up to 1500, so normal
/// windows look as before, growing to `big` (the mockups') at 1800 and staying there.
pub fn grow(page_w: f32, normal: f32, big: f32) -> f32 {
    normal + (big - normal) * ((page_w - GROW_FROM) / (MAX_W - GROW_FROM)).clamp(0.0, 1.0)
}

/// A row of cards at least `min` wide and `gap` apart across `w`: how many fit, and how wide each is
/// for the row to end exactly at both edges (one card as wide as `w` when not even one fits).
pub fn row_fit(w: f32, min: f32, gap: f32) -> (usize, f32) {
    // A hair over, so a row that fits exactly isn't lost to rounding.
    let n = (((w + gap) / (min + gap) + 1e-4).floor() as usize).max(1);
    (n, ((w - gap * (n - 1) as f32) / n as f32).max(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WideScreens::{Centred, Fill, Split};

    #[test]
    fn each_choice_at_each_width() {
        // (content width, Centred's left edge and width, Split splits): no padding, so the
        // middle area is the content.
        let want = [
            (600.0, 0.0, 600.0, false),
            (1200.0, 0.0, 1200.0, false),
            (1799.0, 0.0, 1799.0, false),
            (1800.0, 0.0, 1800.0, false),
            (1900.0, 50.0, 1800.0, true),
            (2800.0, 500.0, 1800.0, true),
            (4400.0, 1300.0, 1800.0, true),
        ];
        for (w, left, width, split) in want {
            assert_eq!(Column::new(Centred, 0.0, w, 0.0, true), Column { left, width, split: false }, "Centred at {w}");
            assert_eq!(Column::new(Fill, 0.0, w, 0.0, true), Column { left: 0.0, width: w, split: false }, "Fill at {w}");
            assert_eq!(Column::new(Split, 0.0, w, 0.0, true), Column { left: 0.0, width: w, split }, "Split at {w}");
            // A page that never splits fills, like Fill.
            assert_eq!(Column::new(Split, 0.0, w, 0.0, false), Column::new(Fill, 0.0, w, 0.0, true), "Split, one column, at {w}");
        }
    }

    #[test]
    fn centred_keeps_the_same_room_on_both_sides() {
        // A 34" window: the middle area 2800 wide from 300, 24 at the sides.
        let c = Column::new(Centred, 300.0, 2800.0, 24.0, true);
        assert_eq!((c.left, c.width), (800.0, 1800.0));
        assert_eq!(c.left - 300.0, 300.0 + 2800.0 - c.right());
        // Up to 1800 of content it fills as before; a hair over, it starts to centre without a jump.
        assert_eq!(Column::new(Centred, 0.0, 1848.0, 24.0, true), Column::new(Fill, 0.0, 1848.0, 24.0, true));
        let over = Column::new(Centred, 0.0, 1849.0, 24.0, true);
        assert_eq!((over.left, over.width), (24.5, 1800.0));
        for w in [600.0, 1200.0, 1799.0, 1800.0, 1900.0, 2800.0, 4400.0] {
            let c = Column::new(Centred, 10.0, w, 24.0, true);
            assert!(c.width <= MAX_W && c.left >= 10.0 + 24.0 - 0.01 && c.right() <= 10.0 + w - 24.0 + 0.01, "{c:?} in {w}");
        }
    }

    #[test]
    fn split_needs_1900_of_content() {
        // With 24 at the sides the area must be 1948 wide.
        assert!(!Column::new(Split, 0.0, 1947.0, 24.0, true).split);
        assert!(Column::new(Split, 0.0, 1948.0, 24.0, true).split);
        // Too narrow to have any content: nothing, not less than nothing.
        assert_eq!(Column::new(Split, 0.0, 30.0, 24.0, true).width, 0.0);
        let c = Column::new(Split, 300.0, 2800.0, 24.0, true);
        let ((l, lw), (r, rw)) = c.halves(1060.0);
        assert_eq!((l, lw, r, rw), (324.0, 1060.0, 1432.0, 1644.0));
        assert_eq!(r + rw, c.right());
        // A first column wider than the page leaves the second nothing.
        assert_eq!(c.halves(4000.0).1.1, 0.0);
    }

    #[test]
    fn a_row_ends_exactly_at_both_edges() {
        for w in [368.0, 600.0, 1059.0, 1200.0, 1644.0, 1800.0, 2752.0, 3440.0, 4432.0, 5120.0] {
            for (min, gap) in [(169.0, 14.0), (136.0, 18.0), (68.0, 28.0), (96.0, 12.0), (132.0, 16.0)] {
                let (n, cw) = row_fit(w, min, gap);
                let row = n as f32 * cw + (n - 1) as f32 * gap;
                assert!((row - w).abs() < 0.01, "{n} of {cw} in {w} end at {row}");
                assert!(cw >= min, "{cw} under {min} in {w}");
                // One more would not fit at its smallest.
                assert!((n + 1) as f32 * min + n as f32 * gap > w, "{n} of {min} in {w}: one more fits");
            }
        }
        // Exactly fitting: the smallest size.
        assert_eq!(row_fit(2.0 * 100.0 + 20.0, 100.0, 20.0), (2, 100.0));
        // Home's mixes (169 with the record, 14 apart) at 1800: 9, each a little wider.
        let (n, cw) = row_fit(1800.0, 169.0, 14.0);
        assert_eq!(n, 9);
        assert!((cw - 187.56).abs() < 0.01, "{cw}");
        // Narrower than one card: one, as wide as there is room.
        assert_eq!(row_fit(80.0, 100.0, 20.0), (1, 80.0));
        assert_eq!(row_fit(0.0, 100.0, 20.0), (1, 1.0));
    }

    #[test]
    fn cards_grow_from_1500_to_1800_then_stay() {
        // Normal windows (and no room at all) as before; the mockups' size from 1800 on.
        for w in [0.0, 300.0, 1200.0, 1452.0, 1499.0, 1500.0] {
            assert_eq!(grow(w, 68.0, 150.0), 68.0, "{w}");
        }
        for w in [1800.0, 1900.0, 2752.0, 3440.0, 4432.0, 5120.0, 1e9] {
            assert_eq!(grow(w, 68.0, 150.0), 150.0, "{w}");
        }
        assert_eq!(grow(1650.0, 68.0, 150.0), 109.0);
        // In between it never jumps or shrinks.
        let mut was = grow(1400.0, 96.0, 190.0);
        let mut w = 1400.0;
        while w <= 1900.0 {
            let now = grow(w, 96.0, 190.0);
            assert!(now >= was && now - was < 0.5, "{was} to {now} at {w}");
            was = now;
            w += 1.0;
        }
    }
}
