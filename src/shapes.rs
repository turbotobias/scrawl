//! Lines, arrows and boxes (`shapeTools` experiment). With a tool other than the pen chosen, a
//! ⇧ stroke is that shape from where it began to the pointer. It is kept as the shape's outline,
//! a path of points like a pen stroke's, so everything that draws, crops or measures ink takes it
//! as it is; `Stroke::tool` says which shape it is.

use crate::model::Tool;

/// Modifiers held with ⇧ while a shape is drawn (⇧ itself is the drawing key).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    /// ⌘ (Ctrl on Windows): lines and arrows snap to a multiple of 45°, boxes to squares.
    pub snap: bool,
    /// ⌥ (Alt): the shape grows both ways from where it began, which becomes its centre.
    pub centre: bool,
}

/// An arrowhead's barbs: how long (points, at most 40% of the shaft) and how far off the shaft.
const HEAD: f32 = 16.0;
const HEAD_ANGLE: f32 = 0.5;
/// Outline points are at most this far apart (points), further for a shape too big for
/// `MAX_POINTS`.
const STEP: f32 = 3.0;
const MAX_POINTS: f32 = 2_000.0;

/// The ends of a line or arrow, or a box's opposite corners, dragged from `anchor` to `pointer`.
pub fn ends(tool: Tool, anchor: [f32; 2], pointer: [f32; 2], mods: Mods) -> ([f32; 2], [f32; 2]) {
    let mut d = [pointer[0] - anchor[0], pointer[1] - anchor[1]];
    if mods.snap {
        d = match tool {
            Tool::Rect => {
                let side = d[0].abs().max(d[1].abs());
                [side.copysign(d[0]), side.copysign(d[1])]
            }
            Tool::Line | Tool::Arrow => {
                let step = std::f32::consts::FRAC_PI_4;
                let angle = (d[1].atan2(d[0]) / step).round() * step;
                let (dy, dx) = angle.sin_cos();
                // How far the pointer reaches along the snapped direction.
                let reach = (d[0] * dx + d[1] * dy).max(0.0);
                [dx * reach, dy * reach]
            }
        };
    }
    let from = if mods.centre {
        [anchor[0] - d[0], anchor[1] - d[1]]
    } else {
        anchor
    };
    (from, [anchor[0] + d[0], anchor[1] + d[1]])
}

/// The shape between `from` and `to` as one path, its points at most `STEP` apart. An arrow is
/// its shaft to the tip, then one barb, back to the tip and the other barb.
pub fn outline(tool: Tool, from: [f32; 2], to: [f32; 2]) -> Vec<[f32; 2]> {
    let corners = match tool {
        Tool::Line => vec![from, to],
        Tool::Rect => vec![from, [to[0], from[1]], to, [from[0], to[1]], from],
        Tool::Arrow => {
            let len = distance(from, to);
            if len < 1.0 {
                vec![from, to]
            } else {
                let head = HEAD.min(0.4 * len);
                let back = (from[1] - to[1]).atan2(from[0] - to[0]);
                let barb = |turn: f32| {
                    let (dy, dx) = (back + turn).sin_cos();
                    [to[0] + head * dx, to[1] + head * dy]
                };
                vec![from, to, barb(HEAD_ANGLE), to, barb(-HEAD_ANGLE)]
            }
        }
    };
    let length: f32 = corners.windows(2).map(|w| distance(w[0], w[1])).sum();
    let step = STEP.max(length / MAX_POINTS);
    let mut out = vec![corners[0]];
    for w in corners.windows(2) {
        let n = (distance(w[0], w[1]) / step).ceil().max(1.0) as usize;
        out.extend((1..=n).map(|i| {
            let t = i as f32 / n as f32;
            [
                w[0][0] + (w[1][0] - w[0][0]) * t,
                w[0][1] + (w[1][1] - w[0][1]) * t,
            ]
        }));
    }
    out
}

/// A line's or arrow's ends from its outline (`[x, y]` in pixels): where it starts, and the
/// point furthest from there (an arrow's barbs turn back, so that's the tip).
pub fn tip_of(outline: &[[f32; 2]]) -> Option<([f32; 2], [f32; 2])> {
    let from = *outline.first()?;
    let to = outline
        .iter()
        .copied()
        .max_by(|a, b| distance(from, *a).total_cmp(&distance(from, *b)))?;
    Some((from, to))
}

fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    (b[0] - a[0]).hypot(b[1] - a[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Mods = Mods {
        snap: false,
        centre: false,
    };

    fn near(a: [f32; 2], b: [f32; 2]) -> bool {
        distance(a, b) < 0.01
    }

    #[test]
    fn snapping_turns_a_drag_into_45_degree_lines_and_square_boxes() {
        let snap = Mods {
            snap: true,
            ..PLAIN
        };
        // Nearly flat: flat, as far as the pointer reaches along it.
        let (from, to) = ends(Tool::Line, [10.0, 10.0], [110.0, 18.0], snap);
        assert!(
            near(from, [10.0, 10.0]) && near(to, [110.0, 10.0]),
            "{to:?}"
        );
        // Nearly diagonal: exactly diagonal.
        let (_, to) = ends(Tool::Arrow, [0.0, 0.0], [100.0, -90.0], snap);
        assert!((to[0] + to[1]).abs() < 0.01 && to[0] > 90.0, "{to:?}");
        // Nearly upright: upright.
        let (_, to) = ends(Tool::Line, [0.0, 0.0], [-6.0, 80.0], snap);
        assert!(to[0].abs() < 0.01 && (to[1] - 80.0).abs() < 0.5, "{to:?}");
        // A box: a square on the longer side, in the direction dragged.
        let (from, to) = ends(Tool::Rect, [50.0, 50.0], [10.0, 80.0], snap);
        assert!(near(from, [50.0, 50.0]) && near(to, [10.0, 90.0]), "{to:?}");
        // Without ⌘ the pointer is the end.
        assert!(near(
            ends(Tool::Line, [0.0, 0.0], [7.0, 3.0], PLAIN).1,
            [7.0, 3.0]
        ));
    }

    #[test]
    fn from_the_centre_grows_both_ways() {
        let centre = Mods {
            centre: true,
            ..PLAIN
        };
        let (from, to) = ends(Tool::Rect, [100.0, 100.0], [130.0, 120.0], centre);
        assert!(near(from, [70.0, 80.0]) && near(to, [130.0, 120.0]));
        let both = Mods {
            snap: true,
            centre: true,
        };
        let (from, to) = ends(Tool::Line, [0.0, 0.0], [50.0, 4.0], both);
        assert!(
            near(from, [-50.0, 0.0]) && near(to, [50.0, 0.0]),
            "{from:?} {to:?}"
        );
    }

    #[test]
    fn outlines_are_dense_paths_through_the_corners_and_an_arrow_ends_at_its_tip() {
        let rect = outline(Tool::Rect, [0.0, 0.0], [90.0, 30.0]);
        for corner in [[0.0, 0.0], [90.0, 0.0], [90.0, 30.0], [0.0, 30.0]] {
            assert!(rect.iter().any(|p| near(*p, corner)), "{corner:?}");
        }
        assert!(near(rect[0], *rect.last().unwrap()), "a box closes");
        assert!(rect.windows(2).all(|w| distance(w[0], w[1]) <= STEP + 0.01));

        let arrow = outline(Tool::Arrow, [0.0, 0.0], [100.0, 0.0]);
        let (from, to) = tip_of(&arrow).unwrap();
        assert!(near(from, [0.0, 0.0]) && near(to, [100.0, 0.0]));
        // Both barbs reach back from the tip, one either side of the shaft.
        let (above, below) = arrow
            .iter()
            .fold((0.0_f32, 0.0_f32), |(a, b), p| (a.min(p[1]), b.max(p[1])));
        assert!(above < -5.0 && below > 5.0 && arrow.iter().all(|p| p[0] <= 100.01));

        let huge = outline(Tool::Rect, [0.0, 0.0], [20_000.0, 20_000.0]);
        assert!(huge.len() as f32 <= MAX_POINTS + 5.0, "{}", huge.len());
    }
}
