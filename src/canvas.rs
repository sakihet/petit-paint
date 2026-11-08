use anyhow::{Context as _, Result, anyhow};
use gpui_kit::{Hsla, Pixels, Point, Rgba, Size, px, size};
use tiny_skia::{
    Color, ColorU8, IntRect, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint,
    PremultipliedColorU8, Stroke as SkiaStroke, Transform,
};

use crate::Stroke;

/// How far each RGBA channel may differ from the clicked pixel and still be
/// filled. Lets the fill reach into the light half of anti-aliased edges.
const FILL_TOLERANCE: u8 = 64;

/// The drawing as a single image, one pixel per canvas pixel.
pub struct Canvas {
    pixmap: Pixmap,
    background: Color,
    /// Bumped on every change, so a cached view knows when to re-render.
    version: u64,
}

/// A reversible change to the canvas. Applying one returns its inverse.
pub enum Change {
    /// Pixels to write back with their top-left at (`x`, `y`).
    Patch { x: u32, y: u32, pixels: Pixmap },
    /// A whole image to swap in, which may have a different size.
    Replace(Pixmap),
}

impl Canvas {
    pub fn new(width: u32, height: u32, background: Hsla) -> Result<Self> {
        let background = to_skia_color(background);
        Ok(Self {
            pixmap: blank(width, height, background)?,
            background,
            version: 0,
        })
    }

    pub fn size(&self) -> Size<Pixels> {
        size(
            px(self.pixmap.width() as f32),
            px(self.pixmap.height() as f32),
        )
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Burns `stroke` into the canvas and returns the change that undoes it,
    /// or `None` when the stroke lies entirely outside the canvas.
    ///
    /// Strokes use round caps and joins to match the on-screen preview.
    pub fn draw_stroke(&mut self, stroke: &Stroke) -> Option<Change> {
        let (first, rest) = stroke.points.split_first()?;
        let mut builder = PathBuilder::new();
        builder.move_to(first.x.as_f32(), first.y.as_f32());
        if rest.is_empty() {
            // Same tiny segment as the preview, so a single click leaves a dot
            builder.line_to(first.x.as_f32() + 0.1, first.y.as_f32() + 0.1);
        } else {
            for p in rest {
                builder.line_to(p.x.as_f32(), p.y.as_f32());
            }
        }
        let path = builder.finish()?;

        // Anti-aliasing reaches about a pixel past the stroke's edge
        let margin = stroke.width.as_f32() / 2.0 + 2.0;
        let bounds = path.bounds();
        let rect = self.clip(
            bounds.left() - margin,
            bounds.top() - margin,
            bounds.right() + margin,
            bounds.bottom() + margin,
        )?;
        let undo = self.snapshot(rect)?;

        let mut paint = Paint::default();
        paint.set_color(to_skia_color(stroke.color));
        paint.anti_alias = true;
        let style = SkiaStroke {
            width: stroke.width.as_f32(),
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Default::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &style, Transform::identity(), None);
        self.version += 1;
        Some(undo)
    }

    /// Fills the region connected to `position` whose pixels have about the
    /// same color, and returns the change that undoes it.
    ///
    /// Returns `None` when `position` is outside the canvas or the region
    /// already has `color`, since filling it would change nothing.
    pub fn flood_fill(&mut self, position: Point<Pixels>, color: Hsla) -> Option<Change> {
        let (seed_x, seed_y) = self.pixel_at(position)?;
        let (width, height) = (self.pixmap.width() as usize, self.pixmap.height() as usize);
        let target = self.pixmap.pixels()[seed_y * width + seed_x].demultiply();
        let fill = to_skia_color(color).to_color_u8();
        if target == fill {
            return None;
        }
        let matches = |c: ColorU8| {
            c.red().abs_diff(target.red()) <= FILL_TOLERANCE
                && c.green().abs_diff(target.green()) <= FILL_TOLERANCE
                && c.blue().abs_diff(target.blue()) <= FILL_TOLERANCE
                && c.alpha().abs_diff(target.alpha()) <= FILL_TOLERANCE
        };

        // Scanline fill: each popped seed grows into a horizontal run, then seeds
        // the matching runs directly above and below it
        let pixels = self.pixmap.pixels();
        let mut visited = vec![false; width * height];
        let mut spans = Vec::new();
        let mut stack = vec![(seed_x, seed_y)];
        while let Some((x, y)) = stack.pop() {
            let row = y * width;
            let fillable = |x: usize, visited: &[bool]| {
                !visited[row + x] && matches(pixels[row + x].demultiply())
            };
            if !fillable(x, &visited) {
                continue;
            }
            let mut start = x;
            while start > 0 && fillable(start - 1, &visited) {
                start -= 1;
            }
            let mut end = x + 1;
            while end < width && fillable(end, &visited) {
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
                    let fillable = !visited[next_row + next_x]
                        && matches(pixels[next_row + next_x].demultiply());
                    if fillable && !in_run {
                        stack.push((next_x, next_y));
                    }
                    in_run = fillable;
                }
            }
        }

        let left = spans.iter().map(|&(_, start, _)| start).min()?;
        let right = spans.iter().map(|&(_, _, end)| end).max()?;
        let top = spans.iter().map(|&(y, _, _)| y).min()?;
        let bottom = spans.iter().map(|&(y, _, _)| y).max()? + 1;
        let rect = IntRect::from_ltrb(left as i32, top as i32, right as i32, bottom as i32)?;
        let undo = self.snapshot(rect)?;

        let fill = fill.premultiply();
        let pixels = self.pixmap.pixels_mut();
        for (y, start, end) in spans {
            for pixel in &mut pixels[y * width + start..y * width + end] {
                *pixel = source_over(fill, *pixel);
            }
        }
        self.version += 1;
        Some(undo)
    }

    /// Returns the color at `position`, or `None` when it is outside the canvas.
    pub fn color_at(&self, position: Point<Pixels>) -> Option<Hsla> {
        let (x, y) = self.pixel_at(position)?;
        let color = self.pixmap.pixel(x as u32, y as u32)?.demultiply();
        Some(
            Rgba {
                r: color.red() as f32 / 255.0,
                g: color.green() as f32 / 255.0,
                b: color.blue() as f32 / 255.0,
                a: color.alpha() as f32 / 255.0,
            }
            .into(),
        )
    }

    /// Paints the whole canvas with the background color and returns the
    /// change that undoes it.
    pub fn clear(&mut self) -> Result<Change> {
        let blank = blank(self.pixmap.width(), self.pixmap.height(), self.background)?;
        Ok(self.apply(Change::Replace(blank)))
    }

    /// Resizes the canvas, keeping the drawing anchored at the top-left, and
    /// returns the change that undoes it. Growing adds background; shrinking
    /// crops.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<Change> {
        let mut resized = blank(width, height, self.background)?;
        resized.draw_pixmap(
            0,
            0,
            self.pixmap.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        Ok(self.apply(Change::Replace(resized)))
    }

    /// Applies `change` and returns its inverse.
    pub fn apply(&mut self, change: Change) -> Change {
        self.version += 1;
        match change {
            Change::Patch { x, y, pixels } => {
                let rect = IntRect::from_xywh(x as i32, y as i32, pixels.width(), pixels.height())
                    .and_then(|rect| self.pixmap.clone_rect(rect))
                    .expect("patch lies inside the canvas it was taken from");
                self.write(x, y, &pixels);
                Change::Patch {
                    x,
                    y,
                    pixels: rect,
                }
            }
            Change::Replace(pixmap) => Change::Replace(std::mem::replace(&mut self.pixmap, pixmap)),
        }
    }

    pub fn encode_png(&self) -> Result<Vec<u8>> {
        self.pixmap.encode_png().context("failed to encode PNG")
    }

    /// Renders part of the canvas, scaled up or down with nearest-neighbor
    /// sampling so pixels stay sharp, as BGRA bytes for gpui.
    ///
    /// The canvas is displayed at `pixels_per_canvas_pixel` device pixels per
    /// canvas pixel. `origin` and `region` select the device pixels to render,
    /// relative to the displayed canvas's top-left. Pixels beyond the canvas,
    /// for example while a resize drag is enlarging it, show the background.
    pub fn render_view(
        &self,
        origin: Point<u32>,
        region: Size<u32>,
        pixels_per_canvas_pixel: f32,
    ) -> Vec<u8> {
        let canvas_index = |device: u32, len: u32| {
            let index = ((device as f32 + 0.5) / pixels_per_canvas_pixel) as u32;
            (index < len).then_some(index as usize)
        };
        let columns: Vec<Option<usize>> = (origin.x..origin.x + region.width)
            .map(|x| canvas_index(x, self.pixmap.width()))
            .collect();
        let background = bgra(self.background.premultiply().to_color_u8());
        let width = self.pixmap.width() as usize;
        let pixels = self.pixmap.pixels();

        let mut out = Vec::with_capacity(region.width as usize * region.height as usize * 4);
        for y in origin.y..origin.y + region.height {
            let row = canvas_index(y, self.pixmap.height()).map(|row| &pixels[row * width..][..width]);
            for column in &columns {
                let pixel = match (row, column) {
                    (Some(row), Some(x)) => bgra(row[*x]),
                    _ => background,
                };
                out.extend_from_slice(&pixel);
            }
        }
        out
    }

    /// Converts a canvas position to pixel indices, if it is inside.
    fn pixel_at(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let x = position.x.as_f32().floor();
        let y = position.y.as_f32().floor();
        let inside = x >= 0.0
            && y >= 0.0
            && x < self.pixmap.width() as f32
            && y < self.pixmap.height() as f32;
        inside.then_some((x as usize, y as usize))
    }

    /// Rounds the rectangle outward to whole pixels and clips it to the canvas.
    fn clip(&self, left: f32, top: f32, right: f32, bottom: f32) -> Option<IntRect> {
        let left = left.floor().max(0.0) as i32;
        let top = top.floor().max(0.0) as i32;
        let right = (right.ceil() as i32).min(self.pixmap.width() as i32);
        let bottom = (bottom.ceil() as i32).min(self.pixmap.height() as i32);
        IntRect::from_ltrb(left, top, right, bottom)
    }

    /// Returns a change that restores `rect` to its current pixels.
    fn snapshot(&self, rect: IntRect) -> Option<Change> {
        Some(Change::Patch {
            x: rect.x() as u32,
            y: rect.y() as u32,
            pixels: self.pixmap.clone_rect(rect)?,
        })
    }

    /// Copies `pixels` into the canvas with their top-left at (`x`, `y`),
    /// replacing what is there rather than blending.
    fn write(&mut self, x: u32, y: u32, pixels: &Pixmap) {
        let canvas_width = self.pixmap.width() as usize;
        let patch_width = pixels.width() as usize;
        let source = pixels.pixels();
        let target = self.pixmap.pixels_mut();
        for row in 0..pixels.height() as usize {
            let start = (y as usize + row) * canvas_width + x as usize;
            target[start..start + patch_width]
                .copy_from_slice(&source[row * patch_width..][..patch_width]);
        }
    }
}

fn blank(width: u32, height: u32, background: Color) -> Result<Pixmap> {
    let mut pixmap =
        Pixmap::new(width, height).ok_or_else(|| anyhow!("invalid canvas size {width}x{height}"))?;
    pixmap.fill(background);
    Ok(pixmap)
}

/// Blends premultiplied `source` over `target`.
fn source_over(source: PremultipliedColorU8, target: PremultipliedColorU8) -> PremultipliedColorU8 {
    let keep = 255 - source.alpha() as u16;
    let blend = |s: u8, t: u8| (s as u16 + (t as u16 * keep + 127) / 255) as u8;
    PremultipliedColorU8::from_rgba(
        blend(source.red(), target.red()),
        blend(source.green(), target.green()),
        blend(source.blue(), target.blue()),
        blend(source.alpha(), target.alpha()),
    )
    .unwrap_or(source)
}

fn bgra(pixel: PremultipliedColorU8) -> [u8; 4] {
    [pixel.blue(), pixel.green(), pixel.red(), pixel.alpha()]
}

fn to_skia_color(color: Hsla) -> Color {
    let rgba = color.to_rgb();
    Color::from_rgba(rgba.r, rgba.g, rgba.b, rgba.a).unwrap_or(Color::BLACK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{point, rgb};

    const WHITE: u32 = 0xffffff;

    fn canvas(width: u32, height: u32) -> Canvas {
        Canvas::new(width, height, rgb(WHITE).into()).unwrap()
    }

    fn stroke(points: &[(f32, f32)], color: u32, width: f32) -> Stroke {
        Stroke {
            points: points.iter().map(|&(x, y)| point(px(x), px(y))).collect(),
            color: rgb(color).into(),
            width: px(width),
        }
    }

    fn rgb_at(canvas: &Canvas, x: u32, y: u32) -> (u8, u8, u8) {
        let p = canvas.pixmap.pixel(x, y).unwrap();
        (p.red(), p.green(), p.blue())
    }

    fn square_outline() -> Stroke {
        stroke(
            &[
                (20.0, 20.0),
                (80.0, 20.0),
                (80.0, 80.0),
                (20.0, 80.0),
                (20.0, 20.0),
            ],
            0x000000,
            4.0,
        )
    }

    #[test]
    fn draws_strokes_into_pixels() {
        let mut canvas = canvas(100, 50);
        canvas.draw_stroke(&stroke(&[(10.0, 25.0), (90.0, 25.0)], 0xff0000, 6.0));
        assert_eq!(rgb_at(&canvas, 50, 25), (255, 0, 0));
        assert_eq!(rgb_at(&canvas, 50, 5), (255, 255, 255));
    }

    #[test]
    fn single_click_draws_a_round_dot() {
        let mut canvas = canvas(40, 40);
        canvas.draw_stroke(&stroke(&[(20.0, 20.0)], 0x000000, 10.0));
        // 3px from the center is inside the 5px radius, in every direction
        for (x, y) in [(23, 20), (17, 20), (20, 23), (20, 17)] {
            assert_eq!(rgb_at(&canvas, x, y).0, 0, "({x}, {y})");
        }
        // The corner of the bounding square stays white because the cap is round
        assert_eq!(rgb_at(&canvas, 15, 15).0, 255);
    }

    #[test]
    fn undoing_a_stroke_restores_the_pixels_and_redo_reapplies_it() {
        let mut canvas = canvas(100, 50);
        let undo = canvas
            .draw_stroke(&stroke(&[(10.0, 25.0), (90.0, 25.0)], 0xff0000, 6.0))
            .unwrap();
        let redo = canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 50, 25), (255, 255, 255));
        canvas.apply(redo);
        assert_eq!(rgb_at(&canvas, 50, 25), (255, 0, 0));
    }

    #[test]
    fn stroke_outside_the_canvas_changes_nothing() {
        let mut canvas = canvas(10, 10);
        assert!(
            canvas
                .draw_stroke(&stroke(&[(50.0, 50.0), (60.0, 60.0)], 0x000000, 2.0))
                .is_none()
        );
    }

    #[test]
    fn color_at_picks_pixels_inside_the_canvas() {
        let mut canvas = canvas(100, 50);
        canvas.draw_stroke(&stroke(&[(10.0, 25.0), (90.0, 25.0)], 0x0000ff, 6.0));
        let blue: Hsla = rgb(0x0000ff).into();
        let white: Hsla = rgb(WHITE).into();
        let at = |x: f32, y: f32| canvas.color_at(point(px(x), px(y)));
        assert_eq!(at(50.0, 25.0).unwrap().to_rgb(), blue.to_rgb());
        assert_eq!(at(50.0, 6.0).unwrap().to_rgb(), white.to_rgb());
        assert_eq!(at(50.0, -10.0), None);
        assert_eq!(at(100.0, 25.0), None);
    }

    #[test]
    fn flood_fill_stays_inside_a_closed_outline() {
        let mut canvas = canvas(100, 100);
        canvas.draw_stroke(&square_outline());
        let undo = canvas
            .flood_fill(point(px(50.0), px(50.0)), rgb(0xff0000).into())
            .unwrap();

        assert_eq!(rgb_at(&canvas, 50, 50), (255, 0, 0));
        assert_eq!(rgb_at(&canvas, 5, 5), (255, 255, 255));
        // The outline itself is not filled
        assert_eq!(rgb_at(&canvas, 20, 50), (0, 0, 0));

        canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 50, 50), (255, 255, 255));
    }

    #[test]
    fn flood_fill_with_the_same_color_does_nothing() {
        let mut canvas = canvas(10, 10);
        let result = canvas.flood_fill(point(px(5.0), px(5.0)), rgb(WHITE).into());
        assert!(result.is_none());
    }

    #[test]
    fn resize_keeps_the_top_left_and_undo_restores_the_crop() {
        let mut canvas = canvas(100, 100);
        canvas.draw_stroke(&stroke(&[(10.0, 10.0), (90.0, 90.0)], 0x000000, 4.0));

        let undo = canvas.resize(50, 200).unwrap();
        assert_eq!(canvas.size(), size(px(50.0), px(200.0)));
        assert_eq!(rgb_at(&canvas, 10, 10), (0, 0, 0));
        assert_eq!(rgb_at(&canvas, 10, 150), (255, 255, 255));

        canvas.apply(undo);
        assert_eq!(canvas.size(), size(px(100.0), px(100.0)));
        // The part that was cropped away comes back
        assert_eq!(rgb_at(&canvas, 90, 90), (0, 0, 0));
    }

    #[test]
    fn clear_paints_the_background_and_can_be_undone() {
        let mut canvas = canvas(20, 20);
        canvas.draw_stroke(&stroke(&[(0.0, 10.0), (20.0, 10.0)], 0x000000, 4.0));
        let undo = canvas.clear().unwrap();
        assert_eq!(rgb_at(&canvas, 10, 10), (255, 255, 255));
        canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 10, 10), (0, 0, 0));
    }

    #[test]
    fn png_is_exactly_the_canvas_size() {
        let canvas = canvas(80, 60);
        let pixmap = Pixmap::decode_png(&canvas.encode_png().unwrap()).unwrap();
        assert_eq!((pixmap.width(), pixmap.height()), (80, 60));
    }

    #[test]
    fn render_view_scales_with_nearest_neighbor_and_pads_with_background() {
        let mut canvas = canvas(2, 1);
        canvas.pixmap.pixels_mut()[0] = PremultipliedColorU8::from_rgba(255, 0, 0, 255).unwrap();

        // At 2x each canvas pixel covers 2x2 device pixels; column 4+ is past the canvas
        let view = canvas.render_view(point(0, 0), size(5, 2), 2.0);
        let pixel = |x: usize, y: usize| &view[(y * 5 + x) * 4..][..4];
        let red = [0, 0, 255, 255];
        let white = [255, 255, 255, 255];
        assert_eq!(pixel(0, 0), red);
        assert_eq!(pixel(1, 1), red);
        assert_eq!(pixel(2, 0), white);
        assert_eq!(pixel(4, 1), white);
    }
}
