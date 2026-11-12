use crate::canvas::Brush;
use crate::tool::ShapeKind;
use gpui_kit::{Hsla, Pixels, Point, point};

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
    /// Connects the last point back to the first.
    pub closed: bool,
    /// Fills the inside with `color` as well.
    pub filled: bool,
    /// Miter joins instead of round ones, for rectangles and polygons.
    pub sharp_corners: bool,
    pub brush: Brush,
    /// Seeds the airbrush's spray pattern.
    pub seed: u64,
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::px;

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
}
