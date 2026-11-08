use anyhow::{Context as _, Result, anyhow};
use gpui_kit::{Hsla, Pixels, Point, Rgba, Size, px, size};
use tiny_skia::{
    Color, ColorU8, FillRule, IntRect, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint,
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

/// A rectangle of canvas pixels. It may extend past the canvas, for example
/// while a selection is moved over the edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    pub fn contains(&self, point: Point<i32>) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x < self.x + self.width as i32
            && point.y < self.y + self.height as i32
    }
}

/// Pixels lifted off the canvas or pasted, which float above it until dropped.
pub struct Floating {
    pub pixels: Pixmap,
    /// The canvas before the pixels were lifted or pasted, so the whole move
    /// is recorded as one undo step when they are dropped.
    before: Pixmap,
    /// Where the pixels were lifted from, which the undo step must restore too.
    source: Option<IntRect>,
}

/// How a stroke's line is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Brush {
    /// A smooth, anti-aliased round line.
    #[default]
    Pen,
    /// Hard-edged pixels with no anti-aliasing, for pixel art.
    Pencil,
    /// A flat-ended, half-transparent line.
    Marker,
    /// Dots sprayed at random around the line.
    Airbrush,
    /// A flat nib held at 45°, so the line is thick or thin by direction.
    Calligraphy,
}

/// Brushes in toolbar order.
pub const BRUSHES: [Brush; 5] = [
    Brush::Pen,
    Brush::Pencil,
    Brush::Marker,
    Brush::Airbrush,
    Brush::Calligraphy,
];

impl Brush {
    pub fn label(self) -> &'static str {
        match self {
            Brush::Pen => "Pen",
            Brush::Pencil => "Pencil",
            Brush::Marker => "Marker",
            Brush::Airbrush => "Airbrush",
            Brush::Calligraphy => "Calligraphy",
        }
    }
}

/// A stroke rendered on its own, to blend onto the canvas with its top-left at `at`.
pub struct StrokeLayer {
    pub pixels: Pixmap,
    pub at: Point<i32>,
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
    pub fn draw_stroke(&mut self, stroke: &Stroke) -> Option<Change> {
        let layer = self.render_stroke(stroke)?;
        self.stamp(&layer.pixels, layer.at)
    }

    /// Renders `stroke` on its own transparent layer, clipped to the canvas,
    /// exactly as [`Self::draw_stroke`] will blend it in. Used to preview the
    /// stroke being drawn.
    ///
    /// The whole stroke is drawn opaque and then made translucent at once, so
    /// a translucent stroke does not darken where it crosses itself.
    pub fn render_stroke(&self, stroke: &Stroke) -> Option<StrokeLayer> {
        let width = stroke.width.as_f32();
        // How far the brush reaches past the points it passes through.
        // Anti-aliasing adds about a pixel; miter joins reach up to twice the
        // width (tiny-skia's miter limit is 4)
        let reach = match stroke.brush {
            Brush::Airbrush => width,
            Brush::Calligraphy => width / 2.0,
            _ if stroke.sharp_corners => width * 2.0,
            _ => width / 2.0,
        } + 2.0;
        let (mut left, mut top, mut right, mut bottom) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in &stroke.points {
            left = left.min(p.x.as_f32());
            top = top.min(p.y.as_f32());
            right = right.max(p.x.as_f32());
            bottom = bottom.max(p.y.as_f32());
        }
        let rect = self.clip(left - reach, top - reach, right + reach, bottom + reach)?;
        let mut pixels = Pixmap::new(rect.width(), rect.height())?;
        let transform = Transform::from_translate(-rect.x() as f32, -rect.y() as f32);

        let color = to_skia_color(stroke.color);
        let mut opaque = color;
        opaque.set_alpha(1.0);
        match stroke.brush {
            Brush::Pen | Brush::Pencil | Brush::Marker => {
                draw_line(&mut pixels, stroke, opaque, transform)
            }
            Brush::Airbrush => spray(&mut pixels, stroke, opaque, transform),
            Brush::Calligraphy => draw_nib(&mut pixels, stroke, opaque, transform),
        }

        let opacity = color.alpha() * if stroke.brush == Brush::Marker { 0.5 } else { 1.0 };
        if opacity < 1.0 {
            for pixel in pixels.pixels_mut() {
                let scale = |c: u8| (c as f32 * opacity).round() as u8;
                *pixel = PremultipliedColorU8::from_rgba(
                    scale(pixel.red()),
                    scale(pixel.green()),
                    scale(pixel.blue()),
                    scale(pixel.alpha()),
                )
                .unwrap_or(*pixel);
            }
        }
        Some(StrokeLayer {
            pixels,
            at: Point::new(rect.x(), rect.y()),
        })
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

    /// Returns a copy of the pixels in `rect`, clipped to the canvas.
    pub fn copy(&self, rect: PixelRect) -> Option<Pixmap> {
        self.pixmap.clone_rect(self.clip_rect(rect)?)
    }

    /// Paints `rect` with the background color and returns the change that undoes it.
    pub fn erase(&mut self, rect: PixelRect) -> Option<Change> {
        let rect = self.clip_rect(rect)?;
        let undo = self.snapshot(rect)?;
        self.pixmap.fill_rect(
            rect.to_rect(),
            &Paint {
                shader: tiny_skia::Shader::SolidColor(self.background),
                blend_mode: tiny_skia::BlendMode::Source,
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
        self.version += 1;
        Some(undo)
    }

    /// Draws `pixels` over the canvas with their top-left at `at`, blending
    /// by their alpha, and returns the change that undoes it.
    pub fn stamp(&mut self, pixels: &Pixmap, at: Point<i32>) -> Option<Change> {
        let rect = self.clip_rect(PixelRect {
            x: at.x,
            y: at.y,
            width: pixels.width(),
            height: pixels.height(),
        })?;
        let undo = self.snapshot(rect)?;
        self.pixmap.draw_pixmap(
            at.x,
            at.y,
            pixels.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        self.version += 1;
        Some(undo)
    }

    /// Lifts the pixels in `rect` off the canvas, leaving background behind.
    pub fn lift(&mut self, rect: PixelRect) -> Option<Floating> {
        let source = self.clip_rect(rect)?;
        let pixels = self.pixmap.clone_rect(source)?;
        let before = self.pixmap.clone();
        self.erase(rect);
        Some(Floating {
            pixels,
            before,
            source: Some(source),
        })
    }

    /// Floats `pixels` above the canvas, as when pasting.
    pub fn float(&self, pixels: Pixmap) -> Floating {
        Floating {
            pixels,
            before: self.pixmap.clone(),
            source: None,
        }
    }

    /// Drops `floating` onto the canvas with its top-left at `at`, or discards
    /// it when `keep` is false, and returns the change that undoes the whole
    /// lift-and-move or paste.
    ///
    /// Returns `None` when the canvas ends up as it was, such as when lifted
    /// pixels are dropped back where they came from.
    pub fn drop_floating(
        &mut self,
        floating: Floating,
        at: Point<i32>,
        keep: bool,
    ) -> Option<Change> {
        let target = PixelRect {
            x: at.x,
            y: at.y,
            width: floating.pixels.width(),
            height: floating.pixels.height(),
        };
        let target = keep.then(|| self.clip_rect(target)).flatten();
        if keep {
            self.pixmap.draw_pixmap(
                at.x,
                at.y,
                floating.pixels.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
            self.version += 1;
        }
        if keep && target == floating.source {
            return None;
        }
        // Restore both where the pixels came from and where they landed
        let changed = match (floating.source, target) {
            (Some(a), Some(b)) => IntRect::from_ltrb(
                a.left().min(b.left()),
                a.top().min(b.top()),
                a.right().max(b.right()),
                a.bottom().max(b.bottom()),
            )?,
            (Some(rect), None) | (None, Some(rect)) => rect,
            (None, None) => return None,
        };
        Some(Change::Patch {
            x: changed.x() as u32,
            y: changed.y() as u32,
            pixels: floating.before.clone_rect(changed)?,
        })
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
        let background = bgra(self.background.premultiply().to_color_u8());
        render_scaled(
            &self.pixmap,
            background,
            origin,
            region,
            pixels_per_canvas_pixel,
        )
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

    fn clip_rect(&self, rect: PixelRect) -> Option<IntRect> {
        let right = rect.x as f32 + rect.width as f32;
        let bottom = rect.y as f32 + rect.height as f32;
        self.clip(rect.x as f32, rect.y as f32, right, bottom)
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

/// Renders all of `pixmap` like [`Canvas::render_view`], returning the BGRA
/// bytes and their size in device pixels.
pub fn render_pixmap(pixmap: &Pixmap, pixels_per_canvas_pixel: f32) -> (Vec<u8>, Size<u32>) {
    let region = size(
        (pixmap.width() as f32 * pixels_per_canvas_pixel).ceil() as u32,
        (pixmap.height() as f32 * pixels_per_canvas_pixel).ceil() as u32,
    );
    let transparent = [0; 4];
    let bytes = render_scaled(
        pixmap,
        transparent,
        Point::default(),
        region,
        pixels_per_canvas_pixel,
    );
    (bytes, region)
}

/// Samples `pixmap` with nearest-neighbor scaling into BGRA bytes, using
/// `padding` for device pixels past its edge.
fn render_scaled(
    pixmap: &Pixmap,
    padding: [u8; 4],
    origin: Point<u32>,
    region: Size<u32>,
    pixels_per_canvas_pixel: f32,
) -> Vec<u8> {
    let canvas_index = |device: u32, len: u32| {
        let index = ((device as f32 + 0.5) / pixels_per_canvas_pixel) as u32;
        (index < len).then_some(index as usize)
    };
    let columns: Vec<Option<usize>> = (origin.x..origin.x + region.width)
        .map(|x| canvas_index(x, pixmap.width()))
        .collect();
    let width = pixmap.width() as usize;
    let pixels = pixmap.pixels();

    let mut out = Vec::with_capacity(region.width as usize * region.height as usize * 4);
    for y in origin.y..origin.y + region.height {
        let row = canvas_index(y, pixmap.height()).map(|row| &pixels[row * width..][..width]);
        for column in &columns {
            let pixel = match (row, column) {
                (Some(row), Some(x)) => bgra(row[*x]),
                _ => padding,
            };
            out.extend_from_slice(&pixel);
        }
    }
    out
}

/// Draws the pen, pencil or marker line, and the fill of closed shapes.
fn draw_line(pixels: &mut Pixmap, stroke: &Stroke, color: Color, transform: Transform) {
    let Some((first, rest)) = stroke.points.split_first() else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color);
    // The pencil draws whole pixels only
    paint.anti_alias = stroke.brush != Brush::Pencil;

    if rest.is_empty() && stroke.brush == Brush::Pencil {
        // A click colors the square of pixels under the brush
        let side = stroke.width.as_f32().round().max(1.0);
        let x = (first.x.as_f32() - side / 2.0).round();
        let y = (first.y.as_f32() - side / 2.0).round();
        if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, side, side) {
            pixels.fill_rect(rect, &paint, transform, None);
        }
        return;
    }

    let mut builder = PathBuilder::new();
    builder.move_to(first.x.as_f32(), first.y.as_f32());
    if rest.is_empty() {
        // A tiny segment, so a single click leaves a round dot
        builder.line_to(first.x.as_f32() + 0.1, first.y.as_f32() + 0.1);
    } else {
        for p in rest {
            builder.line_to(p.x.as_f32(), p.y.as_f32());
        }
    }
    if stroke.closed {
        builder.close();
    }
    let Some(path) = builder.finish() else {
        return;
    };
    if stroke.filled {
        pixels.fill_path(&path, &paint, FillRule::Winding, transform, None);
    }
    let style = SkiaStroke {
        width: stroke.width.as_f32(),
        // Markers have flat tips; round caps make the rest join smoothly
        line_cap: if stroke.brush == Brush::Marker {
            LineCap::Square
        } else {
            LineCap::Round
        },
        line_join: if stroke.sharp_corners {
            LineJoin::Miter
        } else {
            LineJoin::Round
        },
        ..Default::default()
    };
    pixels.stroke_path(&path, &paint, &style, transform, None);
}

/// Sprays one-pixel dots within `width` of the line. Dots are placed at
/// fixed steps along it and seeded per step from the stroke's seed, so the
/// part already drawn stays the same as the stroke grows.
fn spray(pixels: &mut Pixmap, stroke: &Stroke, color: Color, transform: Transform) {
    let radius = stroke.width.as_f32().max(1.0);
    let spacing = (radius / 2.0).max(1.0);
    let dots_per_step = (radius * radius * 0.1).ceil().clamp(1.0, 200.0) as usize;
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = false;

    let mut spray_at = |step: u64, x: f32, y: f32| {
        let mut random = Random(stroke.seed ^ step.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        for _ in 0..dots_per_step {
            let angle = random.next() * std::f32::consts::TAU;
            // sqrt spreads the dots evenly over the disc instead of bunching at the center
            let distance = random.next().sqrt() * radius;
            let dot_x = (x + angle.cos() * distance).floor();
            let dot_y = (y + angle.sin() * distance).floor();
            if let Some(rect) = tiny_skia::Rect::from_xywh(dot_x, dot_y, 1.0, 1.0) {
                pixels.fill_rect(rect, &paint, transform, None);
            }
        }
    };

    let Some(first) = stroke.points.first() else {
        return;
    };
    let mut step = 0;
    spray_at(step, first.x.as_f32(), first.y.as_f32());
    // Distance along the line since the last step
    let mut travelled = 0.0;
    for pair in stroke.points.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let (dx, dy) = ((to.x - from.x).as_f32(), (to.y - from.y).as_f32());
        let length = (dx * dx + dy * dy).sqrt();
        let mut along = spacing - travelled;
        while along <= length {
            step += 1;
            let t = along / length;
            spray_at(step, from.x.as_f32() + dx * t, from.y.as_f32() + dy * t);
            along += spacing;
        }
        travelled = length - (along - spacing);
    }
}

/// Sweeps a flat nib, `width` long and tilted 45°, along the line.
fn draw_nib(pixels: &mut Pixmap, stroke: &Stroke, color: Color, transform: Transform) {
    let half = stroke.width.as_f32() / 2.0 * std::f32::consts::FRAC_1_SQRT_2;
    // From the nib's center to its upper-right end
    let (nx, ny) = (half, -half);
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;

    let xy = |p: &Point<Pixels>| (p.x.as_f32(), p.y.as_f32());
    let segments: Vec<((f32, f32), (f32, f32))> = match stroke.points.as_slice() {
        [] => return,
        // A click prints the nib itself, one pixel wide
        [only] => {
            let (x, y) = xy(only);
            vec![((x - 0.5, y - 0.5), (x + 0.5, y + 0.5))]
        }
        points => points.windows(2).map(|pair| (xy(&pair[0]), xy(&pair[1]))).collect(),
    };

    // One path for the whole stroke, so neighboring segments merge instead
    // of leaving faint seams where their anti-aliased edges meet. Every
    // segment's quad winds the same way, so overlaps add up rather than
    // cancel out under the nonzero fill rule
    let mut builder = PathBuilder::new();
    for (a, b) in segments {
        let mut corners = [
            (a.0 + nx, a.1 + ny),
            (b.0 + nx, b.1 + ny),
            (b.0 - nx, b.1 - ny),
            (a.0 - nx, a.1 - ny),
        ];
        let cross = (b.0 - a.0) * ny - (b.1 - a.1) * nx;
        if cross < 0.0 {
            corners.reverse();
        }
        builder.move_to(corners[0].0, corners[0].1);
        for corner in &corners[1..] {
            builder.line_to(corner.0, corner.1);
        }
        builder.close();
    }
    if let Some(path) = builder.finish() {
        pixels.fill_path(&path, &paint, FillRule::Winding, transform, None);
    }
}

/// A small deterministic random number generator (SplitMix64).
struct Random(u64);

impl Random {
    /// Returns a number in `0.0..1.0`.
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        (z >> 40) as f32 / (1u64 << 24) as f32
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
            ..Default::default()
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
    fn filled_closed_stroke_paints_its_inside() {
        let mut canvas = canvas(40, 40);
        let triangle = Stroke {
            closed: true,
            filled: true,
            sharp_corners: true,
            ..stroke(&[(5.0, 5.0), (35.0, 5.0), (5.0, 35.0)], 0x00ff00, 1.0)
        };
        canvas.draw_stroke(&triangle);
        assert_eq!(rgb_at(&canvas, 10, 10), (0, 255, 0));
        // Past the diagonal edge stays white
        assert_eq!(rgb_at(&canvas, 30, 30), (255, 255, 255));
    }

    #[test]
    fn closed_outline_without_fill_leaves_the_inside() {
        let mut canvas = canvas(40, 40);
        let square = Stroke {
            closed: true,
            sharp_corners: true,
            ..stroke(&[(5.0, 5.0), (35.0, 5.0), (35.0, 35.0), (5.0, 35.0)], 0x000000, 2.0)
        };
        canvas.draw_stroke(&square);
        assert_eq!(rgb_at(&canvas, 20, 20), (255, 255, 255));
        // The closing edge from (5, 35) back to (5, 5) is drawn
        assert_eq!(rgb_at(&canvas, 5, 20), (0, 0, 0));
        // Miter joins keep the corner square
        assert_eq!(rgb_at(&canvas, 4, 4), (0, 0, 0));
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

    fn rect(x: i32, y: i32, width: u32, height: u32) -> PixelRect {
        PixelRect {
            x,
            y,
            width,
            height,
        }
    }

    /// A 20x20 canvas with a black 4x4 block at (2, 2).
    fn canvas_with_block() -> Canvas {
        let mut canvas = canvas(20, 20);
        let black = PremultipliedColorU8::from_rgba(0, 0, 0, 255).unwrap();
        for y in 2..6 {
            for x in 2..6 {
                canvas.pixmap.pixels_mut()[y * 20 + x] = black;
            }
        }
        canvas
    }

    #[test]
    fn moving_a_selection_is_one_undo_step() {
        let mut canvas = canvas_with_block();
        let floating = canvas.lift(rect(2, 2, 4, 4)).unwrap();
        // Lifting leaves background behind
        assert_eq!(rgb_at(&canvas, 3, 3), (255, 255, 255));

        let undo = canvas.drop_floating(floating, point(10, 10), true).unwrap();
        assert_eq!(rgb_at(&canvas, 3, 3), (255, 255, 255));
        assert_eq!(rgb_at(&canvas, 11, 11), (0, 0, 0));

        canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 3, 3), (0, 0, 0));
        assert_eq!(rgb_at(&canvas, 11, 11), (255, 255, 255));
    }

    #[test]
    fn dropping_lifted_pixels_in_place_records_nothing() {
        let mut canvas = canvas_with_block();
        let floating = canvas.lift(rect(2, 2, 4, 4)).unwrap();
        assert!(canvas.drop_floating(floating, point(2, 2), true).is_none());
        assert_eq!(rgb_at(&canvas, 3, 3), (0, 0, 0));
    }

    #[test]
    fn discarding_lifted_pixels_deletes_them() {
        let mut canvas = canvas_with_block();
        let floating = canvas.lift(rect(2, 2, 4, 4)).unwrap();
        let undo = canvas.drop_floating(floating, point(2, 2), false).unwrap();
        assert_eq!(rgb_at(&canvas, 3, 3), (255, 255, 255));
        canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 3, 3), (0, 0, 0));
    }

    #[test]
    fn pasting_clips_to_the_canvas_and_can_be_undone() {
        let mut canvas = canvas_with_block();
        let pixels = canvas.copy(rect(2, 2, 4, 4)).unwrap();
        let floating = canvas.float(pixels);
        // Partly past the bottom-right corner
        let undo = canvas.drop_floating(floating, point(18, 18), true).unwrap();
        assert_eq!(rgb_at(&canvas, 19, 19), (0, 0, 0));
        canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 19, 19), (255, 255, 255));
        assert_eq!(rgb_at(&canvas, 3, 3), (0, 0, 0));
    }

    #[test]
    fn erase_paints_background_and_can_be_undone() {
        let mut canvas = canvas_with_block();
        let undo = canvas.erase(rect(0, 0, 4, 4)).unwrap();
        assert_eq!(rgb_at(&canvas, 3, 3), (255, 255, 255));
        // Outside the erased rect stays
        assert_eq!(rgb_at(&canvas, 5, 5), (0, 0, 0));
        canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 3, 3), (0, 0, 0));
    }

    #[test]
    fn stamp_blends_transparent_pixels_and_can_be_undone() {
        let mut canvas = canvas_with_block();
        // A 2x1 image: opaque red, then fully transparent
        let mut pixels = Pixmap::new(2, 1).unwrap();
        pixels.pixels_mut()[0] = PremultipliedColorU8::from_rgba(255, 0, 0, 255).unwrap();
        let undo = canvas.stamp(&pixels, point(2, 2)).unwrap();
        assert_eq!(rgb_at(&canvas, 2, 2), (255, 0, 0));
        // The transparent pixel leaves the block showing through
        assert_eq!(rgb_at(&canvas, 3, 2), (0, 0, 0));
        canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 2, 2), (0, 0, 0));
    }

    fn brush_stroke(brush: Brush, points: &[(f32, f32)], width: f32) -> Stroke {
        Stroke {
            brush,
            seed: 7,
            ..stroke(points, 0x000000, width)
        }
    }

    /// Distinct non-white colors on the canvas.
    fn shades(canvas: &Canvas) -> std::collections::BTreeSet<(u8, u8, u8)> {
        canvas
            .pixmap
            .pixels()
            .iter()
            .map(|p| (p.red(), p.green(), p.blue()))
            .filter(|&c| c != (255, 255, 255))
            .collect()
    }

    #[test]
    fn pencil_draws_only_solid_pixels() {
        let mut pencil = canvas(40, 40);
        pencil.draw_stroke(&brush_stroke(Brush::Pencil, &[(3.0, 5.0), (35.0, 31.0)], 1.0));
        // No anti-aliased in-between shades, unlike the pen
        assert_eq!(shades(&pencil), [(0, 0, 0)].into());

        let mut pen = canvas(40, 40);
        pen.draw_stroke(&brush_stroke(Brush::Pen, &[(3.0, 5.0), (35.0, 31.0)], 1.0));
        assert!(shades(&pen).len() > 1);
    }

    #[test]
    fn pencil_click_colors_one_pixel() {
        let mut canvas = canvas(10, 10);
        canvas.draw_stroke(&brush_stroke(Brush::Pencil, &[(4.5, 4.5)], 1.0));
        let colored = canvas.pixmap.pixels().iter().filter(|p| p.red() == 0).count();
        assert_eq!(colored, 1);
        assert_eq!(rgb_at(&canvas, 4, 4), (0, 0, 0));
    }

    #[test]
    fn marker_is_translucent_and_does_not_darken_where_it_overlaps() {
        let mut canvas = canvas(40, 40);
        // Crosses itself at (20, 20)
        let x = brush_stroke(
            Brush::Marker,
            &[(5.0, 5.0), (35.0, 35.0), (35.0, 5.0), (5.0, 35.0)],
            6.0,
        );
        canvas.draw_stroke(&x);
        let crossing = rgb_at(&canvas, 20, 20);
        let single = rgb_at(&canvas, 10, 10);
        assert_eq!(crossing, single);
        assert!(crossing.0 > 100 && crossing.0 < 160, "{crossing:?} is not half gray");
    }

    #[test]
    fn airbrush_sprays_sparse_dots_within_its_radius() {
        let mut canvas = canvas(60, 60);
        let spray = brush_stroke(Brush::Airbrush, &[(30.0, 30.0)], 10.0);
        canvas.draw_stroke(&spray);
        let dots: Vec<_> = (0..60u32)
            .flat_map(|y| (0..60u32).map(move |x| (x, y)))
            .filter(|&(x, y)| rgb_at(&canvas, x, y) != (255, 255, 255))
            .collect();
        assert!(!dots.is_empty());
        // Far fewer dots than the disc's area
        assert!(dots.len() < 100);
        for (x, y) in dots {
            let distance = ((x as f32 - 30.0).powi(2) + (y as f32 - 30.0).powi(2)).sqrt();
            assert!(distance <= 12.0, "dot at ({x}, {y}) is outside the radius");
        }
    }

    #[test]
    fn airbrush_keeps_earlier_dots_as_the_stroke_grows() {
        let canvas = canvas(100, 40);
        let short = brush_stroke(Brush::Airbrush, &[(10.0, 20.0), (40.0, 20.0)], 6.0);
        let long = brush_stroke(Brush::Airbrush, &[(10.0, 20.0), (40.0, 20.0), (90.0, 20.0)], 6.0);
        let short = canvas.render_stroke(&short).unwrap();
        let long = canvas.render_stroke(&long).unwrap();
        // Compare the columns both layers cover, left of where the stroke grew
        for y in 0..short.pixels.height() {
            for x in 0..30 {
                let a = short.pixels.pixel(x, y).unwrap().alpha();
                let b = long.pixels.pixel(x, y).unwrap().alpha();
                assert_eq!(a, b, "({x}, {y})");
            }
        }
    }

    #[test]
    fn calligraphy_is_thick_or_thin_by_direction() {
        let thickness = |points: &[(f32, f32)], probe: (u32, u32), step: (i32, i32)| {
            let mut canvas = canvas(60, 60);
            canvas.draw_stroke(&brush_stroke(Brush::Calligraphy, points, 12.0));
            // Count dark pixels across the line through `probe`
            (-20..=20)
                .filter(|i| {
                    let x = probe.0 as i32 + step.0 * i;
                    let y = probe.1 as i32 + step.1 * i;
                    (0..60).contains(&x) && (0..60).contains(&y)
                        && rgb_at(&canvas, x as u32, y as u32).0 < 128
                })
                .count()
        };
        // The nib runs from lower-left to upper-right, so a stroke along
        // that diagonal is a hairline and one across it is full width
        let along_nib = thickness(&[(10.0, 50.0), (50.0, 10.0)], (30, 30), (1, 1));
        let across_nib = thickness(&[(10.0, 10.0), (50.0, 50.0)], (30, 30), (1, -1));
        assert!(along_nib <= 2, "{along_nib}");
        assert!(across_nib >= 7, "{across_nib}");
    }

    #[test]
    fn calligraphy_has_no_seams_between_segments() {
        let mut canvas = canvas(60, 60);
        // Many short segments across the nib, as a slow mouse drag makes
        let points: Vec<_> = (0..=40).map(|i| (10.0 + i as f32, 10.0 + i as f32)).collect();
        canvas.draw_stroke(&brush_stroke(Brush::Calligraphy, &points, 12.0));
        // Down the middle of the stroke every pixel is fully covered
        for i in 5..35 {
            let (x, y) = (10 + i, 10 + i);
            assert_eq!(rgb_at(&canvas, x, y), (0, 0, 0), "seam at ({x}, {y})");
        }
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
