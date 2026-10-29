//! Where the panel sits on its display. Home is `PANEL_Y` below the top, centred
//! (`screens::pill_origin`); dragging the capsule moves it from there. The drag is kept as an
//! offset from home, so it carries to the next capture and to other displays (and, saved in
//! `capsule-offset` beside the captures, across restarts), and the capsule is always kept inside
//! the display's visible frame (below the menu bar, clear of the Dock), whatever size it grows to.

use std::path::PathBuf;

use super::model::Layout;

/// How far the pointer moves with the button down before a press becomes a drag.
pub const DRAG_SLOP: f32 = 4.0;
/// A drop this close to home goes home, so it's easy to put back exactly.
pub const SNAP: f32 = 24.0;
/// The least room between the capsule and the edge of the visible frame.
pub const MARGIN: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Place {
    /// The display's visible frame: global points, top-left origin.
    pub visible: [f32; 4],
    /// The panel's top-left at home.
    pub home: [f32; 2],
    /// Where it was dragged to, from home.
    pub offset: [f32; 2],
}

impl Place {
    /// The panel's top-left for a capsule at `capsule` (panel points), kept on screen.
    pub fn origin(&self, capsule: &Layout) -> [f32; 2] {
        self.inside(
            [self.home[0] + self.offset[0], self.home[1] + self.offset[1]],
            capsule,
        )
    }

    /// `origin` moved as little as it takes to put `capsule` inside the visible frame. A capsule
    /// wider or taller than the frame is centred on it.
    pub fn inside(&self, origin: [f32; 2], capsule: &Layout) -> [f32; 2] {
        let [vx, vy, vw, vh] = self.visible;
        let fit = |o: f32, lo: f32, hi: f32| {
            if lo > hi { (lo + hi) * 0.5 } else { o.clamp(lo, hi) }
        };
        [
            fit(
                origin[0],
                vx + MARGIN - capsule.l,
                vx + vw - MARGIN - capsule.r,
            ),
            fit(
                origin[1],
                vy + MARGIN - capsule.t,
                vy + vh - MARGIN - capsule.b,
            ),
        ]
    }

    /// Remembers where a drag left the panel; near home it goes home.
    pub fn drop_at(&mut self, origin: [f32; 2]) {
        let offset = [
            (origin[0] - self.home[0]).round(),
            (origin[1] - self.home[1]).round(),
        ];
        self.offset = if offset[0].hypot(offset[1]) < SNAP {
            [0.0, 0.0]
        } else {
            offset
        };
    }
}

/// Beside the captures, as `last-mode` is.
fn saved_path() -> Option<PathBuf> {
    crate::handoff::builtin_captures_dir()
        .parent()
        .map(|dir| dir.join("capsule-offset"))
}

/// The offset a drag left last time Scrawl ran, else none.
pub fn saved_offset() -> [f32; 2] {
    saved_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| parse(&text))
        .unwrap_or_default()
}

/// Keeps `offset` for the next launch; home needs no file.
pub fn save_offset(offset: [f32; 2]) {
    let Some(path) = saved_path() else {
        return;
    };
    let _ = if offset == [0.0, 0.0] {
        std::fs::remove_file(path)
    } else {
        std::fs::write(path, format!("{} {}", offset[0], offset[1]))
    };
}

fn parse(text: &str) -> Option<[f32; 2]> {
    let mut parts = text.split_whitespace().map(str::parse::<f32>);
    let offset = [parts.next()?.ok()?, parts.next()?.ok()?];
    (parts.next().is_none() && offset.iter().all(|v| v.is_finite() && v.abs() < 100_000.0))
        .then_some(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 1512 × 982 display with a 37 pt menu bar; home as `pill_origin` puts it.
    fn place() -> Place {
        Place {
            visible: [0.0, 37.0, 1512.0, 945.0],
            home: [436.0, 34.0],
            offset: [0.0, 0.0],
        }
    }

    fn capsule(w: f32, h: f32) -> Layout {
        Layout::centered(w, h)
    }

    #[test]
    fn home_is_home_and_a_drag_carries_over() {
        let mut p = place();
        let small = capsule(220.0, 40.0);
        assert_eq!(p.origin(&small), p.home);
        p.drop_at([636.2, 533.8]);
        assert_eq!(p.offset, [200.0, 500.0]);
        assert_eq!(p.origin(&small), [636.0, 534.0]);
        // Another display: the same offset from its home.
        p.home = [2000.0, 25.0];
        p.visible = [1512.0, 25.0, 1920.0, 1055.0];
        assert_eq!(p.origin(&small), [2200.0, 525.0]);
    }

    #[test]
    fn a_drop_near_home_goes_home() {
        let mut p = place();
        p.drop_at([p.home[0] + 15.0, p.home[1] + 15.0]);
        assert_eq!(p.offset, [0.0, 0.0]);
        p.drop_at([p.home[0] + 30.0, p.home[1]]);
        assert_eq!(p.offset, [30.0, 0.0]);
    }

    #[test]
    fn the_capsule_stays_on_screen_whatever_its_size() {
        let mut p = place();
        // Dragged far past the bottom-right corner.
        p.drop_at([5000.0, 5000.0]);
        for c in [capsule(120.0, 40.0), capsule(560.0, 40.0), capsule(380.0, 80.0)] {
            let [x, y] = p.origin(&c);
            assert!((x + c.r - (1512.0 - MARGIN)).abs() < 0.01, "{c:?}");
            assert!((y + c.b - (982.0 - MARGIN)).abs() < 0.01, "{c:?}");
        }
        // And past the top-left: never under the menu bar.
        p.drop_at([-5000.0, -5000.0]);
        let c = capsule(300.0, 40.0);
        let [x, y] = p.origin(&c);
        assert!((x + c.l - MARGIN).abs() < 0.01);
        assert!((y + c.t - (37.0 + MARGIN)).abs() < 0.01);
    }

    #[test]
    fn a_saved_offset_reads_back_and_junk_is_ignored() {
        assert_eq!(parse("200 -480"), Some([200.0, -480.0]));
        assert_eq!(parse(" 12.5\t7\n"), Some([12.5, 7.0]));
        for junk in ["", "200", "200 x", "1 2 3", "NaN 0", "inf 0", "1e9 0"] {
            assert_eq!(parse(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn a_capsule_bigger_than_the_screen_is_centred_on_it() {
        let mut p = place();
        p.visible = [0.0, 0.0, 400.0, 300.0];
        let c = capsule(560.0, 40.0);
        let [x, _] = p.inside([0.0, 0.0], &c);
        assert!((x + (c.l + c.r) * 0.5 - 200.0).abs() < 0.01);
    }
}
