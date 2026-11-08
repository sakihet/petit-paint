use anyhow::{Context as _, Result, anyhow};
use gpui_kit::{Bounds, Hsla, Pixels};
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Stroke as SkiaStroke, Transform};

use crate::Stroke;

/// Rasterizes the strokes inside `bounds` and encodes them as PNG.
///
/// `scale` is the window's scale factor, so the image matches the on-screen
/// resolution. The stroke style follows gpui's defaults (butt caps, miter
/// joins), which are also tiny-skia's defaults.
pub fn encode_png(
    strokes: &[Stroke],
    bounds: Bounds<Pixels>,
    background: Hsla,
    scale: f32,
) -> Result<Vec<u8>> {
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
            ..Default::default()
        };
        pixmap.stroke_path(&path, &paint, &style, transform, None);
    }

    pixmap.encode_png().context("failed to encode PNG")
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
}
