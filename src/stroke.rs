use crate::canvas::{Brush, ClipMask, PixelRect};
use crate::tool::ShapeKind;
use gpui_kit::{Hsla, Pixels, Point, point, px};
use std::sync::Arc;
use tiny_skia::BlendMode;

/// A line through `points`: a brush stroke or a shape's outline. Points are
/// in canvas coordinates, with the origin at its top-left.
///
/// While it is being drawn it is previewed as a vector path; when finished
/// it is burned into the canvas pixels.
#[derive(Clone, Default)]
pub struct Stroke {
    pub points: Vec<Point<Pixels>>,
    pub color: Hsla,
    /// Zero draws only the fill, with no outline.
    pub width: Pixels,
    /// How hard the brush presses at each point, from 0 to 1, scaling
    /// `width` there. Empty presses fully everywhere.
    pub pressure: Vec<f32>,
    /// Connects the last point back to the first.
    pub closed: bool,
    /// Fills the inside with `color` as well.
    pub filled: bool,
    /// Miter joins instead of round ones, for rectangles and polygons.
    pub sharp_corners: bool,
    pub brush: Brush,
    /// Seeds the airbrush's spray pattern.
    pub seed: u64,
    /// How the finished stroke combines with the pixels under it.
    pub blend: BlendMode,
    /// Keeps the stroke inside this part of the canvas.
    pub clip: Option<PixelRect>,
    /// Keeps the stroke inside, or outside, an area with soft edges.
    pub mask: Option<Arc<ClipMask>>,
    /// Fills the inside with these colors in place of `color`.
    pub gradient: Option<Gradient>,
}

/// Colors that change across a fill. Each stop is a position from 0 to 1
/// and the color there; beyond the ends, the end colors carry on.
#[derive(Clone, Debug, PartialEq)]
pub enum Gradient {
    /// Along the line from `from` to `to`.
    Linear {
        from: Point<Pixels>,
        to: Point<Pixels>,
        stops: Vec<(f32, Hsla)>,
    },
    /// Out from `center` to `radius` away.
    Radial {
        center: Point<Pixels>,
        radius: f32,
        stops: Vec<(f32, Hsla)>,
    },
}

impl Stroke {
    /// The outline of a `kind` shape dragged from `start` to `end`, drawn
    /// with `base`'s color, width and fill. A polygon's drag is one edge.
    pub fn shape(kind: ShapeKind, start: Point<Pixels>, end: Point<Pixels>, base: Stroke) -> Self {
        let filled = base.filled && kind.fillable();
        let base = Stroke { filled, ..base };
        let (left, right) = (start.x.min(end.x), start.x.max(end.x));
        let (top, bottom) = (start.y.min(end.y), start.y.max(end.y));
        match kind {
            ShapeKind::Line | ShapeKind::Polygon => Stroke {
                points: vec![start, end],
                ..base
            },
            ShapeKind::Rectangle => Stroke {
                points: vec![
                    point(left, top),
                    point(right, top),
                    point(right, bottom),
                    point(left, bottom),
                ],
                closed: true,
                sharp_corners: true,
                ..base
            },
            ShapeKind::Ellipse => {
                let center = point((left + right) / 2.0, (top + bottom) / 2.0);
                let (rx, ry) = ((right - left) / 2.0, (bottom - top) / 2.0);
                // Enough segments that edges stay smooth at the largest zoom
                let segments = ((rx + ry).as_f32() * 2.0).clamp(24.0, 720.0) as usize;
                let points = (0..segments)
                    .map(|i| {
                        let angle = i as f32 / segments as f32 * std::f32::consts::TAU;
                        center + point(rx * angle.cos(), ry * angle.sin())
                    })
                    .collect();
                Stroke {
                    points,
                    closed: true,
                    ..base
                }
            }
        }
    }

    /// A polygon through `points`, drawn with `base`'s color, width and fill.
    /// An unfinished one stays open and unfilled.
    pub fn polygon(points: Vec<Point<Pixels>>, finished: bool, base: Stroke) -> Self {
        Stroke {
            points,
            closed: finished,
            filled: finished && base.filled,
            sharp_corners: true,
            ..base
        }
    }
}

/// Which part of an ellipse [`Stroke::arc`] draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ArcKind {
    /// The curve alone.
    Arc,
    /// A slice, closed through the center.
    Pie,
    /// The curve closed by a straight line between its ends.
    Chord,
}

impl Stroke {
    /// A rectangle between opposite corners `start` and `end` with corners
    /// rounded to `radius`, drawn with `base`'s color, width and fill.
    pub fn rounded_rectangle(
        start: Point<Pixels>,
        end: Point<Pixels>,
        radius: f32,
        base: Stroke,
    ) -> Self {
        let (left, right) = (start.x.min(end.x).as_f32(), start.x.max(end.x).as_f32());
        let (top, bottom) = (start.y.min(end.y).as_f32(), start.y.max(end.y).as_f32());
        let radius = radius.min((right - left) / 2.0).min((bottom - top) / 2.0);
        if radius <= 0.0 {
            return Stroke::shape(ShapeKind::Rectangle, start, end, base);
        }
        // Each corner is a quarter turn, clockwise from the top-right
        let corners = [
            (right - radius, top + radius, -90.0),
            (right - radius, bottom - radius, 0.0),
            (left + radius, bottom - radius, 90.0),
            (left + radius, top + radius, 180.0),
        ];
        let segments = arc_segments(radius, radius, 90.0);
        let points = corners
            .iter()
            .flat_map(|&(x, y, from)| {
                (0..=segments).map(move |i| {
                    let angle = (from + 90.0 * i as f32 / segments as f32).to_radians();
                    point(px(x + radius * angle.cos()), px(y + radius * angle.sin()))
                })
            })
            .collect();
        Stroke {
            points,
            closed: true,
            filled: base.filled,
            ..base
        }
    }

    /// Part of the ellipse in the box between opposite corners `start` and
    /// `end`, from `from` to `to` degrees. 0° points right and angles run
    /// clockwise, as y runs down; `to` at or before `from` goes on round past
    /// 360°. An arc is never filled.
    pub fn arc(
        start: Point<Pixels>,
        end: Point<Pixels>,
        from: f32,
        to: f32,
        kind: ArcKind,
        base: Stroke,
    ) -> Self {
        let (left, right) = (start.x.min(end.x).as_f32(), start.x.max(end.x).as_f32());
        let (top, bottom) = (start.y.min(end.y).as_f32(), start.y.max(end.y).as_f32());
        let (cx, cy) = ((left + right) / 2.0, (top + bottom) / 2.0);
        let (rx, ry) = ((right - left) / 2.0, (bottom - top) / 2.0);
        let mut sweep = (to - from).rem_euclid(360.0);
        if sweep == 0.0 {
            sweep = 360.0;
        }
        let segments = arc_segments(rx, ry, sweep);
        let mut points: Vec<Point<Pixels>> = (0..=segments)
            .map(|i| {
                let angle = (from + sweep * i as f32 / segments as f32).to_radians();
                point(px(cx + rx * angle.cos()), px(cy + ry * angle.sin()))
            })
            .collect();
        if kind == ArcKind::Pie {
            points.insert(0, point(px(cx), px(cy)));
        }
        Stroke {
            points,
            closed: kind != ArcKind::Arc,
            filled: base.filled && kind != ArcKind::Arc,
            sharp_corners: kind == ArcKind::Pie,
            ..base
        }
    }
}

/// How many straight segments draw `sweep` degrees of an ellipse with radii
/// `rx` and `ry` smoothly, at the same density as a whole ellipse.
fn arc_segments(rx: f32, ry: f32, sweep: f32) -> usize {
    let whole = ((rx + ry) * 2.0).clamp(24.0, 720.0);
    (whole * sweep / 360.0).ceil().max(2.0) as usize
}

/// Points along a smooth curve through `points`, a Catmull-Rom spline, for
/// strokes and polygons drawn as curves. A `closed` curve also runs from the
/// last point back to the first.
pub fn smooth(points: &[Point<Pixels>], closed: bool) -> Vec<Point<Pixels>> {
    let n = points.len();
    if n < 3 {
        return points.to_vec();
    }
    let at = |i: isize| {
        let i = if closed {
            i.rem_euclid(n as isize)
        } else {
            i.clamp(0, n as isize - 1)
        };
        let p = points[i as usize];
        (p.x.as_f32(), p.y.as_f32())
    };
    let spans = if closed { n } else { n - 1 };
    let mut curve = Vec::new();
    for i in 0..spans as isize {
        let (p0, p1, p2, p3) = (at(i - 1), at(i), at(i + 1), at(i + 2));
        // About one segment every 4 pixels
        let length = ((p2.0 - p1.0).powi(2) + (p2.1 - p1.1).powi(2)).sqrt();
        let steps = (length / 4.0).ceil().clamp(4.0, 64.0) as usize;
        for step in 0..steps {
            let t = step as f32 / steps as f32;
            let blend = |a: f32, b: f32, c: f32, d: f32| {
                0.5 * (2.0 * b
                    + (c - a) * t
                    + (2.0 * a - 5.0 * b + 4.0 * c - d) * t * t
                    + (3.0 * b - a - 3.0 * c + d) * t * t * t)
            };
            curve.push(point(
                px(blend(p0.0, p1.0, p2.0, p3.0)),
                px(blend(p0.1, p1.1, p2.1, p3.1)),
            ));
        }
    }
    if !closed {
        curve.push(points[n - 1]);
    }
    curve
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f32, y: f32) -> Point<Pixels> {
        point(px(x), px(y))
    }

    fn filled() -> Stroke {
        Stroke {
            filled: true,
            ..Default::default()
        }
    }

    #[test]
    fn rectangle_runs_clockwise_from_the_top_left_whichever_way_it_is_dragged() {
        let rect = Stroke::shape(
            ShapeKind::Rectangle,
            at(50.0, 40.0),
            at(10.0, 20.0),
            filled(),
        );
        assert_eq!(
            rect.points,
            [
                at(10.0, 20.0),
                at(50.0, 20.0),
                at(50.0, 40.0),
                at(10.0, 40.0)
            ]
        );
        assert!(rect.closed && rect.sharp_corners && rect.filled);
    }

    #[test]
    fn ellipse_stays_inside_its_box() {
        let ellipse = Stroke::shape(ShapeKind::Ellipse, at(0.0, 0.0), at(40.0, 20.0), filled());
        assert!(ellipse.closed && !ellipse.sharp_corners);
        assert!(ellipse.points.len() >= 24);
        for p in &ellipse.points {
            assert!(p.x >= px(-0.01) && p.x <= px(40.01));
            assert!(p.y >= px(-0.01) && p.y <= px(20.01));
        }
    }

    #[test]
    fn lines_are_never_filled() {
        let line = Stroke::shape(ShapeKind::Line, at(0.0, 0.0), at(10.0, 10.0), filled());
        assert_eq!(line.points, [at(0.0, 0.0), at(10.0, 10.0)]);
        assert!(!line.closed && !line.filled);
    }

    #[test]
    fn unfinished_polygon_stays_open_and_unfilled() {
        let points = vec![at(0.0, 0.0), at(10.0, 0.0), at(5.0, 8.0)];
        let open = Stroke::polygon(points.clone(), false, filled());
        assert!(!open.closed && !open.filled);
        let done = Stroke::polygon(points, true, filled());
        assert!(done.closed && done.filled && done.sharp_corners);
    }

    #[test]
    fn smooth_curves_pass_through_every_point() {
        let points = vec![at(0.0, 0.0), at(40.0, 30.0), at(80.0, 0.0), at(120.0, 30.0)];
        let curve = smooth(&points, false);
        assert!(curve.len() > 20);
        for p in &points {
            assert!(curve.iter().any(|c| (*c - *p).magnitude() < 0.01), "{p:?}");
        }
        assert_eq!(curve.first(), points.first());
        assert_eq!(curve.last(), points.last());
    }

    #[test]
    fn short_lines_stay_as_they_are() {
        let points = vec![at(0.0, 0.0), at(10.0, 10.0)];
        assert_eq!(smooth(&points, false), points);
    }

    #[test]
    fn rounded_corners_are_cut_inside_the_box() {
        let rect = Stroke::rounded_rectangle(at(0.0, 0.0), at(100.0, 60.0), 10.0, filled());
        assert!(rect.closed && rect.filled && !rect.sharp_corners);
        for p in &rect.points {
            assert!(p.x >= px(-0.01) && p.x <= px(100.01) && p.y >= px(-0.01) && p.y <= px(60.01));
        }
        // No point reaches the corner itself
        assert!(
            rect.points
                .iter()
                .all(|p| (*p - at(0.0, 0.0)).magnitude() > 2.0)
        );
        // A radius of zero is a plain rectangle
        let plain = Stroke::rounded_rectangle(at(0.0, 0.0), at(100.0, 60.0), 0.0, filled());
        assert_eq!(plain.points.len(), 4);
    }

    #[test]
    fn arcs_run_clockwise_from_their_start_angle() {
        let half = Stroke::arc(
            at(0.0, 0.0),
            at(100.0, 100.0),
            180.0,
            360.0,
            ArcKind::Arc,
            filled(),
        );
        assert!(!half.closed && !half.filled);
        let (first, last) = (half.points[0], *half.points.last().unwrap());
        assert!((first - at(0.0, 50.0)).magnitude() < 0.01, "{first:?}");
        assert!((last - at(100.0, 50.0)).magnitude() < 0.01, "{last:?}");
        // Through the top, where y is smallest
        assert!(half.points.iter().all(|p| p.y <= px(50.01)));

        let pie = Stroke::arc(
            at(0.0, 0.0),
            at(100.0, 100.0),
            0.0,
            90.0,
            ArcKind::Pie,
            filled(),
        );
        assert!(pie.closed && pie.filled);
        assert_eq!(pie.points[0], at(50.0, 50.0));
        let chord = Stroke::arc(
            at(0.0, 0.0),
            at(100.0, 100.0),
            0.0,
            90.0,
            ArcKind::Chord,
            filled(),
        );
        assert!(chord.closed && chord.filled);
        assert_eq!(chord.points.len() + 1, pie.points.len());
    }

    #[test]
    fn an_arc_ending_before_it_starts_goes_on_round() {
        let arc = Stroke::arc(
            at(0.0, 0.0),
            at(100.0, 100.0),
            270.0,
            90.0,
            ArcKind::Arc,
            filled(),
        );
        // From the top, round the right side, to the bottom
        assert!(arc.points.iter().all(|p| p.x >= px(49.99)));
        let whole = Stroke::arc(
            at(0.0, 0.0),
            at(100.0, 100.0),
            30.0,
            30.0,
            ArcKind::Arc,
            filled(),
        );
        assert!(whole.points.iter().any(|p| p.x < px(1.0)));
    }
}
