use anyhow::{Context as _, Result, anyhow};
use gpui_kit::{Bounds, Hsla, Pixels, Point, Rgba};
use tiny_skia::{
    Color, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke as SkiaStroke, Transform,
};

use crate::Stroke;

/// Rasterizes the strokes inside `bounds` and encodes them as PNG.
pub fn encode_png(
    strokes: &[Stroke],
    bounds: Bounds<Pixels>,
    background: Hsla,
    scale: f32,
) -> Result<Vec<u8>> {
    rasterize(strokes, bounds, background, scale)?
        .encode_png()
        .context("failed to encode PNG")
}

/// Returns the color of the drawing at `position`, in window coordinates.
///
/// Returns `None` when `position` is outside `bounds`.
pub fn color_at(
    strokes: &[Stroke],
    bounds: Bounds<Pixels>,
    background: Hsla,
    scale: f32,
    position: Point<Pixels>,
) -> Result<Option<Hsla>> {
    let pixmap = rasterize(strokes, bounds, background, scale)?;
    let x = ((position.x - bounds.origin.x).as_f32() * scale).floor();
    let y = ((position.y - bounds.origin.y).as_f32() * scale).floor();
    if x < 0.0 || y < 0.0 {
        return Ok(None);
    }
    let Some(pixel) = pixmap.pixel(x as u32, y as u32) else {
        return Ok(None);
    };
    let color = pixel.demultiply();
    Ok(Some(
        Rgba {
            r: color.red() as f32 / 255.0,
            g: color.green() as f32 / 255.0,
            b: color.blue() as f32 / 255.0,
            a: color.alpha() as f32 / 255.0,
        }
        .into(),
    ))
}

/// Draws the strokes inside `bounds` onto a new pixmap filled with `background`.
///
/// `scale` is the window's scale factor, so the image matches the on-screen
/// resolution. Strokes use round caps and joins to match the on-screen
/// rendering.
fn rasterize(
    strokes: &[Stroke],
    bounds: Bounds<Pixels>,
    background: Hsla,
    scale: f32,
) -> Result<Pixmap> {
    let width = (bounds.size.width.as_f32() * scale).round() as u32;
    let height = (bounds.size.height.as_f32() * scale).round() as u32;
    let mut pixmap =
        Pixmap::new(width, height).ok_or_else(|| anyhow!("invalid image size {width}x{height}"))?;
    pixmap.fill(to_skia_color(background));

    // Strokes are in window coordinates; shift them so the canvas origin is (0, 0)
    let transform = Transform::from_translate(
        -bounds.origin.x.as_f32(),
        -bounds.origin.y.as_f32(),
    )
    .post_scale(scale, scale);

    for stroke in strokes {
        let Some((first, rest)) = stroke.points.split_first() else {
            continue;
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
            continue;
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

    Ok(pixmap)
}

fn to_skia_color(color: Hsla) -> Color {
    let rgba = color.to_rgb();
    Color::from_rgba(rgba.r, rgba.g, rgba.b, rgba.a).unwrap_or(Color::BLACK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{point, px, rgb, size};

    #[test]
    fn crops_to_canvas_and_applies_scale() {
        // Canvas sits below a 44px toolbar, as in the app
        let bounds = Bounds::new(point(px(0.0), px(44.0)), size(px(100.0), px(50.0)));
        let red: Hsla = rgb(0xff0000).into();
        let strokes = [Stroke {
            points: vec![point(px(10.0), px(69.0)), point(px(90.0), px(69.0))],
            color: red,
            width: px(6.0),
        }];

        let png = encode_png(&strokes, bounds, rgb(0xffffff).into(), 2.0).unwrap();
        let pixmap = Pixmap::decode_png(&png).unwrap();

        assert_eq!((pixmap.width(), pixmap.height()), (200, 100));
        // Window y=69 is canvas y=25, i.e. row 50 at 2x
        let on_line = pixmap.pixel(100, 50).unwrap();
        assert_eq!((on_line.red(), on_line.green(), on_line.blue()), (255, 0, 0));
        let background = pixmap.pixel(100, 10).unwrap();
        assert_eq!(
            (background.red(), background.green(), background.blue()),
            (255, 255, 255)
        );
    }

    #[test]
    fn single_click_exports_a_round_dot() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(40.0), px(40.0)));
        let strokes = [Stroke {
            points: vec![point(px(20.0), px(20.0))],
            color: rgb(0x000000).into(),
            width: px(10.0),
        }];

        let png = encode_png(&strokes, bounds, rgb(0xffffff).into(), 1.0).unwrap();
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
        let strokes = [Stroke {
            points: vec![point(px(10.0), px(69.0)), point(px(90.0), px(69.0))],
            color: blue,
            width: px(6.0),
        }];

        let on_line = color_at(&strokes, bounds, white, 2.0, point(px(50.0), px(69.0)));
        assert_eq!(on_line.unwrap().unwrap().to_rgb(), blue.to_rgb());
        let background = color_at(&strokes, bounds, white, 2.0, point(px(50.0), px(50.0)));
        assert_eq!(background.unwrap().unwrap().to_rgb(), white.to_rgb());
        // Above the canvas, on the toolbar
        let outside = color_at(&strokes, bounds, white, 2.0, point(px(50.0), px(10.0)));
        assert_eq!(outside.unwrap(), None);
    }
}
