use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow};
use gpui_kit::{Bounds, Hsla, Pixels, Point, Rgba};
use tiny_skia::{
    Color, ColorU8, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Rect, Stroke as SkiaStroke,
    Transform,
};

use crate::{Fill, Item, Stroke};

/// How far each RGBA channel may differ from the clicked pixel and still be
/// filled. Lets the fill reach into the light half of anti-aliased edges.
const FILL_TOLERANCE: u8 = 64;

/// A rectangle of device pixels, relative to the canvas origin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Rasterizes the items inside `bounds` and encodes them as PNG.
pub fn encode_png(
    items: &[Item],
    bounds: Bounds<Pixels>,
    background: Hsla,
    scale: f32,
) -> Result<Vec<u8>> {
    rasterize(items, bounds, background, scale)?
        .encode_png()
        .context("failed to encode PNG")
}

/// Returns the color of the drawing at `position`, in window coordinates.
///
/// Returns `None` when `position` is outside `bounds`.
pub fn color_at(
    items: &[Item],
    bounds: Bounds<Pixels>,
    background: Hsla,
    scale: f32,
    position: Point<Pixels>,
) -> Result<Option<Hsla>> {
    let pixmap = rasterize(items, bounds, background, scale)?;
    let Some((x, y)) = device_position(&pixmap, bounds, scale, position) else {
        return Ok(None);
    };
    let color = pixmap.pixel(x, y).map(|pixel| pixel.demultiply());
    Ok(color.map(|color| {
        Rgba {
            r: color.red() as f32 / 255.0,
            g: color.green() as f32 / 255.0,
            b: color.blue() as f32 / 255.0,
            a: color.alpha() as f32 / 255.0,
        }
        .into()
    }))
}

/// Finds the region connected to `position` whose pixels have the same color,
/// and returns it as rectangles of device pixels.
///
/// Returns `None` when `position` is outside `bounds` or the region already
/// has `color`, since filling it would change nothing.
pub fn flood_fill(
    items: &[Item],
    bounds: Bounds<Pixels>,
    background: Hsla,
    scale: f32,
    position: Point<Pixels>,
    color: Hsla,
) -> Result<Option<Vec<PixelRect>>> {
    let pixmap = rasterize(items, bounds, background, scale)?;
    let Some((seed_x, seed_y)) = device_position(&pixmap, bounds, scale, position) else {
        return Ok(None);
    };
    let (width, height) = (pixmap.width() as usize, pixmap.height() as usize);
    let pixels: Vec<ColorU8> = pixmap.pixels().iter().map(|p| p.demultiply()).collect();
    let target = pixels[seed_y as usize * width + seed_x as usize];
    if target == to_skia_color(color).to_color_u8() {
        return Ok(None);
    }
    let matches = |c: ColorU8| {
        c.red().abs_diff(target.red()) <= FILL_TOLERANCE
            && c.green().abs_diff(target.green()) <= FILL_TOLERANCE
            && c.blue().abs_diff(target.blue()) <= FILL_TOLERANCE
            && c.alpha().abs_diff(target.alpha()) <= FILL_TOLERANCE
    };

    // Scanline fill: each popped seed grows into a horizontal run, then seeds
    // the matching runs directly above and below it
    let mut visited = vec![false; width * height];
    let mut spans = Vec::new();
    let mut stack = vec![(seed_x as usize, seed_y as usize)];
    while let Some((x, y)) = stack.pop() {
        let row = y * width;
        if visited[row + x] || !matches(pixels[row + x]) {
            continue;
        }
        let mut start = x;
        while start > 0 && !visited[row + start - 1] && matches(pixels[row + start - 1]) {
            start -= 1;
        }
        let mut end = x + 1;
        while end < width && !visited[row + end] && matches(pixels[row + end]) {
            end += 1;
        }
        visited[row + start..row + end].fill(true);
        spans.push((y, start, end));

        for next_y in [y.checked_sub(1), Some(y + 1).filter(|&y| y < height)]
            .into_iter()
            .flatten()
        {
            let next_row = next_y * width;
            let mut in_run = false;
            for next_x in start..end {
                let fillable = !visited[next_row + next_x] && matches(pixels[next_row + next_x]);
                if fillable && !in_run {
                    stack.push((next_x, next_y));
                }
                in_run = fillable;
            }
        }
    }

    Ok(Some(merge_spans(spans)))
}

/// Merges runs that repeat on consecutive rows into taller rectangles, so a
/// plain region becomes a handful of quads instead of one per row.
fn merge_spans(mut spans: Vec<(usize, usize, usize)>) -> Vec<PixelRect> {
    spans.sort_unstable();
    let mut rects: Vec<PixelRect> = Vec::new();
    // Rectangles that reach the previous row, keyed by their horizontal extent
    let mut open: HashMap<(usize, usize), usize> = HashMap::new();
    let mut current: HashMap<(usize, usize), usize> = HashMap::new();
    let mut current_y = None;
    for (y, start, end) in spans {
        if current_y != Some(y) {
            open = if current_y.is_some_and(|prev| prev + 1 == y) {
                std::mem::take(&mut current)
            } else {
                current.clear();
                HashMap::new()
            };
            current_y = Some(y);
        }
        let index = match open.remove(&(start, end)) {
            Some(index) => {
                rects[index].height += 1;
                index
            }
            None => {
                rects.push(PixelRect {
                    x: start as u32,
                    y: y as u32,
                    width: (end - start) as u32,
                    height: 1,
                });
                rects.len() - 1
            }
        };
        current.insert((start, end), index);
    }
    rects
}

/// Converts a window position to a pixel in `pixmap`, if it is inside.
fn device_position(
    pixmap: &Pixmap,
    bounds: Bounds<Pixels>,
    scale: f32,
    position: Point<Pixels>,
) -> Option<(u32, u32)> {
    let x = ((position.x - bounds.origin.x).as_f32() * scale).floor();
    let y = ((position.y - bounds.origin.y).as_f32() * scale).floor();
    if x < 0.0 || y < 0.0 || x >= pixmap.width() as f32 || y >= pixmap.height() as f32 {
        return None;
    }
    Some((x as u32, y as u32))
}

/// Draws the items inside `bounds` onto a new pixmap filled with `background`.
///
/// `scale` is the window's scale factor, so the image matches the on-screen
/// resolution.
fn rasterize(
    items: &[Item],
    bounds: Bounds<Pixels>,
    background: Hsla,
    scale: f32,
) -> Result<Pixmap> {
    let width = (bounds.size.width.as_f32() * scale).round() as u32;
    let height = (bounds.size.height.as_f32() * scale).round() as u32;
    let mut pixmap =
        Pixmap::new(width, height).ok_or_else(|| anyhow!("invalid image size {width}x{height}"))?;
    pixmap.fill(to_skia_color(background));

    // Items are in window coordinates; shift them so the canvas origin is (0, 0)
    let transform = Transform::from_translate(
        -bounds.origin.x.as_f32(),
        -bounds.origin.y.as_f32(),
    )
    .post_scale(scale, scale);

    for item in items {
        match item {
            Item::Stroke(stroke) => draw_stroke(&mut pixmap, stroke, transform),
            Item::Fill(fill) => draw_fill(&mut pixmap, fill, transform),
        }
    }

    Ok(pixmap)
}

/// Strokes use round caps and joins to match the on-screen rendering.
fn draw_stroke(pixmap: &mut Pixmap, stroke: &Stroke, transform: Transform) {
    let Some((first, rest)) = stroke.points.split_first() else {
        return;
    };
    let mut builder = PathBuilder::new();
    builder.move_to(first.x.as_f32(), first.y.as_f32());
    if rest.is_empty() {
        // Same tiny segment as on screen, so a single click is exported too
        builder.line_to(first.x.as_f32() + 0.1, first.y.as_f32() + 0.1);
    } else {
        for p in rest {
            builder.line_to(p.x.as_f32(), p.y.as_f32());
        }
    }
    let Some(path) = builder.finish() else {
        return;
    };

    let mut paint = Paint::default();
    paint.set_color(to_skia_color(stroke.color));
    paint.anti_alias = true;
    let style = SkiaStroke {
        width: stroke.width.as_f32(),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    };
    pixmap.stroke_path(&path, &paint, &style, transform, None);
}

fn draw_fill(pixmap: &mut Pixmap, fill: &Fill, transform: Transform) {
    let mut paint = Paint::default();
    paint.set_color(to_skia_color(fill.color));
    // Rects share edges exactly, so anti-aliasing would only add seams
    paint.anti_alias = false;
    for rect in fill.rects.iter() {
        let bounds = fill.window_bounds(rect);
        let Some(rect) = Rect::from_xywh(
            bounds.origin.x.as_f32(),
            bounds.origin.y.as_f32(),
            bounds.size.width.as_f32(),
            bounds.size.height.as_f32(),
        ) else {
            continue;
        };
        pixmap.fill_rect(rect, &paint, transform, None);
    }
}

fn to_skia_color(color: Hsla) -> Color {
    let rgba = color.to_rgb();
    Color::from_rgba(rgba.r, rgba.g, rgba.b, rgba.a).unwrap_or(Color::BLACK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{point, px, rgb, size};
    use std::rc::Rc;

    fn stroke(points: &[(f32, f32)], color: u32, width: f32) -> Item {
        Item::Stroke(Stroke {
            points: points.iter().map(|&(x, y)| point(px(x), px(y))).collect(),
            color: rgb(color).into(),
            width: px(width),
        })
    }

    fn rgb_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8) {
        let p = pixmap.pixel(x, y).unwrap();
        (p.red(), p.green(), p.blue())
    }

    #[test]
    fn crops_to_canvas_and_applies_scale() {
        // Canvas sits below a 44px toolbar, as in the app
        let bounds = Bounds::new(point(px(0.0), px(44.0)), size(px(100.0), px(50.0)));
        let items = [stroke(&[(10.0, 69.0), (90.0, 69.0)], 0xff0000, 6.0)];

        let png = encode_png(&items, bounds, rgb(0xffffff).into(), 2.0).unwrap();
        let pixmap = Pixmap::decode_png(&png).unwrap();

        assert_eq!((pixmap.width(), pixmap.height()), (200, 100));
        // Window y=69 is canvas y=25, i.e. row 50 at 2x
        assert_eq!(rgb_at(&pixmap, 100, 50), (255, 0, 0));
        assert_eq!(rgb_at(&pixmap, 100, 10), (255, 255, 255));
    }

    #[test]
    fn single_click_exports_a_round_dot() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(40.0), px(40.0)));
        let items = [stroke(&[(20.0, 20.0)], 0x000000, 10.0)];

        let png = encode_png(&items, bounds, rgb(0xffffff).into(), 1.0).unwrap();
        let pixmap = Pixmap::decode_png(&png).unwrap();

        // 3px from the center is inside the 5px radius, in every direction
        for (x, y) in [(23, 20), (17, 20), (20, 23), (20, 17)] {
            assert_eq!(pixmap.pixel(x, y).unwrap().red(), 0, "({x}, {y})");
        }
        // The corner of the bounding square stays white because the cap is round
        assert_eq!(pixmap.pixel(15, 15).unwrap().red(), 255);
    }

    #[test]
    fn color_at_picks_stroke_and_background() {
        let bounds = Bounds::new(point(px(0.0), px(44.0)), size(px(100.0), px(50.0)));
        let blue: Hsla = rgb(0x0000ff).into();
        let white: Hsla = rgb(0xffffff).into();
        let items = [stroke(&[(10.0, 69.0), (90.0, 69.0)], 0x0000ff, 6.0)];

        let on_line = color_at(&items, bounds, white, 2.0, point(px(50.0), px(69.0)));
        assert_eq!(on_line.unwrap().unwrap().to_rgb(), blue.to_rgb());
        let background = color_at(&items, bounds, white, 2.0, point(px(50.0), px(50.0)));
        assert_eq!(background.unwrap().unwrap().to_rgb(), white.to_rgb());
        // Above the canvas, on the toolbar
        let outside = color_at(&items, bounds, white, 2.0, point(px(50.0), px(10.0)));
        assert_eq!(outside.unwrap(), None);
    }

    #[test]
    fn flood_fill_stays_inside_a_closed_outline() {
        let bounds = Bounds::new(point(px(0.0), px(44.0)), size(px(100.0), px(100.0)));
        let white: Hsla = rgb(0xffffff).into();
        let red: Hsla = rgb(0xff0000).into();
        let square = stroke(
            &[
                (20.0, 64.0),
                (80.0, 64.0),
                (80.0, 124.0),
                (20.0, 124.0),
                (20.0, 64.0),
            ],
            0x000000,
            4.0,
        );
        let mut items = vec![square];

        let rects = flood_fill(&items, bounds, white, 2.0, point(px(50.0), px(94.0)), red)
            .unwrap()
            .unwrap();
        // The inside of the square is a plain region, so rows merge into one rect
        assert_eq!(rects.len(), 1);
        items.push(Item::Fill(Fill {
            color: red,
            origin: bounds.origin,
            scale: 2.0,
            rects: Rc::new(rects),
        }));

        let png = encode_png(&items, bounds, white, 2.0).unwrap();
        let pixmap = Pixmap::decode_png(&png).unwrap();
        // Canvas (50, 50) is inside the square, (5, 5) is outside it
        assert_eq!(rgb_at(&pixmap, 100, 100), (255, 0, 0));
        assert_eq!(rgb_at(&pixmap, 10, 10), (255, 255, 255));
        // The outline itself is not filled
        assert_eq!(rgb_at(&pixmap, 40, 100), (0, 0, 0));
    }

    #[test]
    fn flood_fill_with_the_same_color_does_nothing() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(10.0), px(10.0)));
        let white: Hsla = rgb(0xffffff).into();
        let result = flood_fill(&[], bounds, white, 1.0, point(px(5.0), px(5.0)), white);
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn merge_spans_joins_only_matching_consecutive_rows() {
        let rects = merge_spans(vec![(0, 0, 4), (1, 0, 4), (2, 1, 3), (4, 1, 3)]);
        assert_eq!(
            rects,
            vec![
                PixelRect { x: 0, y: 0, width: 4, height: 2 },
                PixelRect { x: 1, y: 2, width: 2, height: 1 },
                PixelRect { x: 1, y: 4, width: 2, height: 1 },
            ]
        );
    }
}
