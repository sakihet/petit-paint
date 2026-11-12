use anyhow::{Result, anyhow};
use gpui_kit::{Hsla, Pixels, Point, Rgba, Size, point, px, size};
use std::rc::Rc;
use tiny_skia::{
    BlendMode, Color, ColorU8, FillRule, IntRect, LineCap, LineJoin, Paint, PathBuilder, Pixmap,
    PixmapPaint, PremultipliedColorU8, Stroke as SkiaStroke, Transform,
};

use crate::pattern::Pattern;
use crate::stroke::{Gradient, Stroke};
use crate::text::{FontChoice, TextRenderer};

/// What the fill tool paints: color 1 where the pattern is set and color 2
/// elsewhere.
#[derive(Clone, Copy, Debug)]
pub struct Fill {
    pub pattern: Pattern,
    pub foreground: Hsla,
    pub background: Hsla,
}

/// Grid lines drawn over the canvas view.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Grid {
    /// Lines between pixels, shown once pixels are large enough to see them apart.
    pub pixels: bool,
    /// Lines every this many pixels, for tiles and sprite sheets.
    pub tile: Option<u32>,
}

/// Displayed pixels must be at least this many device pixels wide for the
/// pixel grid to show; smaller, the lines would cover the drawing.
const MIN_PIXEL_GRID_SPACING: f32 = 4.0;
/// Grid line colors as (gray level, opacity out of 255).
const PIXEL_GRID_LINE: (u8, u8) = (128, 80);
const TILE_GRID_LINE: (u8, u8) = (0, 160);

/// Checkerboard shades behind transparent pixels.
const CHECKER_LIGHT: u8 = 0xff;
const CHECKER_DARK: u8 = 0xdd;

/// How far each RGBA channel may differ from the clicked pixel and still be
/// filled, by default. Lets the fill reach into the light half of
/// anti-aliased edges.
pub const DEFAULT_FILL_TOLERANCE: u8 = 64;

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

/// A flip or quarter turn of an image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Orientation {
    FlipHorizontal,
    FlipVertical,
    RotateClockwise,
    RotateCounterClockwise,
}

/// Returns `pixmap` flipped or turned. Quarter turns swap width and height.
pub fn reoriented(pixmap: &Pixmap, orientation: Orientation) -> Pixmap {
    let (pixels, (width, height)) = reoriented_grid(
        pixmap.pixels(),
        pixmap.width(),
        pixmap.height(),
        orientation,
    );
    let mut out = Pixmap::new(width, height).expect("same area as a valid pixmap");
    out.pixels_mut().copy_from_slice(&pixels);
    out
}

/// Flips or turns a `width` by `height` grid of values stored row by row,
/// returning them with their new width and height.
fn reoriented_grid<T: Copy>(
    source: &[T],
    width: u32,
    height: u32,
    orientation: Orientation,
) -> (Vec<T>, (u32, u32)) {
    let (new_width, new_height) = match orientation {
        Orientation::FlipHorizontal | Orientation::FlipVertical => (width, height),
        Orientation::RotateClockwise | Orientation::RotateCounterClockwise => (height, width),
    };
    let mut target = source.to_vec();
    for y in 0..height {
        for x in 0..width {
            // Where pixel (x, y) lands
            let (nx, ny) = match orientation {
                Orientation::FlipHorizontal => (width - 1 - x, y),
                Orientation::FlipVertical => (x, height - 1 - y),
                Orientation::RotateClockwise => (height - 1 - y, x),
                Orientation::RotateCounterClockwise => (y, width - 1 - x),
            };
            target[(ny * new_width + nx) as usize] = source[(y * width + x) as usize];
        }
    }
    (target, (new_width, new_height))
}

/// The pixels a free-form selection covers within its bounding box.
#[derive(Clone)]
pub struct SelectionMask {
    width: u32,
    height: u32,
    inside: Vec<bool>,
    /// Pixel edges between covered and uncovered pixels, from one corner to
    /// another, to draw the selection's outline along.
    edges: Rc<[(Point<u32>, Point<u32>)]>,
}

impl SelectionMask {
    /// Covers the pixels of a `width` by `height` box whose centers lie inside
    /// `outline`, given relative to the box's top-left. Returns `None` when
    /// that is no pixel at all.
    pub fn from_outline(outline: &[Point<f32>], width: u32, height: u32) -> Option<Self> {
        let (first, rest) = outline.split_first()?;
        let mut builder = PathBuilder::new();
        builder.move_to(first.x, first.y);
        for p in rest {
            builder.line_to(p.x, p.y);
        }
        builder.close();
        let path = builder.finish()?;
        let mut mask = tiny_skia::Mask::new(width, height)?;
        // Without anti-aliasing a pixel is either in or out
        mask.fill_path(&path, FillRule::Winding, false, Transform::identity());
        let inside: Vec<bool> = mask.data().iter().map(|&coverage| coverage > 0).collect();
        inside
            .contains(&true)
            .then(|| Self::new(width, height, inside))
    }

    fn new(width: u32, height: u32, inside: Vec<bool>) -> Self {
        let mut mask = Self {
            width,
            height,
            inside,
            edges: Rc::new([]),
        };
        mask.edges = mask.find_edges().into();
        mask
    }

    /// Whether the pixel at (`x`, `y`) from the box's top-left is selected.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        (0..self.width as i32).contains(&x)
            && (0..self.height as i32).contains(&y)
            && self.inside[(y as u32 * self.width + x as u32) as usize]
    }

    pub fn edges(&self) -> Rc<[(Point<u32>, Point<u32>)]> {
        self.edges.clone()
    }

    pub fn reorient(&mut self, orientation: Orientation) {
        let (inside, (width, height)) =
            reoriented_grid(&self.inside, self.width, self.height, orientation);
        *self = Self::new(width, height, inside);
    }

    /// Walks each row and column boundary, joining neighboring unit edges into
    /// one longer edge.
    fn find_edges(&self) -> Vec<(Point<u32>, Point<u32>)> {
        let (width, height) = (self.width as i32, self.height as i32);
        let mut edges = Vec::new();
        let mut collect =
            |lines: i32, length: i32, differs: &dyn Fn(i32, i32) -> bool, horizontal: bool| {
                for line in 0..=lines {
                    let mut start = None;
                    for along in 0..=length {
                        let edge = along < length && differs(line, along);
                        match (edge, start) {
                            (true, None) => start = Some(along),
                            (false, Some(from)) => {
                                let (a, b) = if horizontal {
                                    (point(from, line), point(along, line))
                                } else {
                                    (point(line, from), point(line, along))
                                };
                                edges.push((
                                    point(a.x as u32, a.y as u32),
                                    point(b.x as u32, b.y as u32),
                                ));
                                start = None;
                            }
                            _ => {}
                        }
                    }
                }
            };
        // Above row `y`, between it and the row before
        collect(
            height,
            width,
            &|y, x| self.contains(x, y - 1) != self.contains(x, y),
            true,
        );
        // Left of column `x`
        collect(
            width,
            height,
            &|x, y| self.contains(x - 1, y) != self.contains(x, y),
            false,
        );
        edges
    }
}

/// Pixels lifted off the canvas or pasted, which float above it until dropped.
pub struct Floating {
    pub pixels: Pixmap,
    /// Whether the pixels were flipped or turned since they were lifted, so
    /// dropping them back in place still changes the canvas.
    reoriented: bool,
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
    /// Soft graphite, lighter than the pen and grainy.
    NaturalPencil,
    /// Waxy color with gaps where the paper's grain shows through.
    Crayon,
    /// A flat-ended, half-transparent line.
    Marker,
    /// Dots sprayed at random around the line.
    Airbrush,
    /// A flat nib held at 45°, so the line is thick or thin by direction.
    Calligraphy,
}

/// Brushes in toolbar order.
pub const BRUSHES: [Brush; 7] = [
    Brush::Pen,
    Brush::Pencil,
    Brush::NaturalPencil,
    Brush::Crayon,
    Brush::Marker,
    Brush::Airbrush,
    Brush::Calligraphy,
];

impl Brush {
    pub fn label(self) -> &'static str {
        match self {
            Brush::Pen => "Pen",
            Brush::Pencil => "Pencil",
            Brush::NaturalPencil => "Natural pencil",
            Brush::Crayon => "Crayon",
            Brush::Marker => "Marker",
            Brush::Airbrush => "Airbrush",
            Brush::Calligraphy => "Calligraphy",
        }
    }
}

/// A soft-edged area that a stroke is kept inside of, or outside of. Only
/// the box around the area is kept, so masks stay small on a large canvas.
#[derive(Clone)]
pub struct ClipMask {
    /// The canvas pixel at the top-left of `coverage`.
    x: i32,
    y: i32,
    /// How much of each pixel is inside the area, from 0 to 255.
    coverage: tiny_skia::Mask,
    /// Keeps strokes outside the area instead.
    inverted: bool,
}

impl ClipMask {
    /// The same area, keeping strokes on its other side.
    pub fn inverted(self) -> Self {
        ClipMask {
            inverted: !self.inverted,
            ..self
        }
    }

    /// Lets strokes cover everywhere.
    fn everywhere() -> Self {
        ClipMask {
            x: 0,
            y: 0,
            coverage: tiny_skia::Mask::new(1, 1).expect("1×1 is a valid size"),
            inverted: true,
        }
    }

    /// Where every one of `masks` lets strokes cover, or `None` when they
    /// have no area in common.
    pub fn all(masks: Vec<ClipMask>) -> Option<ClipMask> {
        if masks.iter().all(|mask| mask.inverted) {
            // Outside every area is outside all of them joined
            return Some(Self::any(masks.into_iter().map(Self::inverted).collect())?.inverted());
        }
        // Only where every area that is not inverted overlaps
        let (left, top, right, bottom) = masks
            .iter()
            .filter(|mask| !mask.inverted)
            .map(ClipMask::bounds)
            .reduce(|a, b| (a.0.max(b.0), a.1.max(b.1), a.2.min(b.2), a.3.min(b.3)))?;
        Self::combine(&masks, (left, top, right, bottom), |coverages| {
            coverages.fold(1.0, |covered, coverage| covered * coverage)
        })
    }

    /// Where any one of `masks` lets strokes cover.
    pub fn any(masks: Vec<ClipMask>) -> Option<ClipMask> {
        if masks.iter().any(|mask| mask.inverted) {
            // Everywhere but where they all leave uncovered
            let uncovered = Self::all(masks.into_iter().map(Self::inverted).collect());
            return Some(uncovered.map_or_else(Self::everywhere, Self::inverted));
        }
        let (left, top, right, bottom) = masks
            .iter()
            .map(ClipMask::bounds)
            .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))?;
        Self::combine(&masks, (left, top, right, bottom), |coverages| {
            1.0 - coverages.fold(1.0, |uncovered, coverage| uncovered * (1.0 - coverage))
        })
    }

    /// A mask over the box from (`left`, `top`) to (`right`, `bottom`)
    /// whose coverage `mix` makes from each of `masks`' coverages, from 0 to
    /// 1. Returns `None` for an empty box.
    fn combine(
        masks: &[ClipMask],
        (left, top, right, bottom): (i32, i32, i32, i32),
        mix: impl Fn(&mut dyn Iterator<Item = f32>) -> f32,
    ) -> Option<ClipMask> {
        if right <= left || bottom <= top {
            return None;
        }
        let (width, height) = ((right - left) as u32, (bottom - top) as u32);
        let coverage = (top..bottom)
            .flat_map(|y| (left..right).map(move |x| (x, y)))
            .map(|(x, y)| {
                let mut coverages = masks.iter().map(|mask| mask.at(x, y) as f32 / 255.0);
                (mix(&mut coverages) * 255.0).round() as u8
            })
            .collect();
        Some(ClipMask {
            x: left,
            y: top,
            coverage: tiny_skia::Mask::from_vec(
                coverage,
                tiny_skia::IntSize::from_wh(width, height)?,
            )?,
            inverted: false,
        })
    }

    /// The box `coverage` fills, as left, top, right and bottom.
    fn bounds(&self) -> (i32, i32, i32, i32) {
        let (width, height) = (self.coverage.width() as i32, self.coverage.height() as i32);
        (self.x, self.y, self.x + width, self.y + height)
    }

    /// How much of the canvas pixel at (`x`, `y`) a stroke may cover, from 0
    /// to 255.
    fn at(&self, x: i32, y: i32) -> u8 {
        let (mx, my) = (x - self.x, y - self.y);
        let (width, height) = (self.coverage.width() as i32, self.coverage.height() as i32);
        let inside = if mx >= 0 && my >= 0 && mx < width && my < height {
            self.coverage.data()[(my * width + mx) as usize]
        } else {
            0
        };
        if self.inverted { 255 - inside } else { inside }
    }

    /// Limits the box from (`left`, `top`) to (`right`, `bottom`) to the part
    /// a stroke may cover.
    fn limit(&self, (left, top, right, bottom): (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
        if self.inverted {
            return (left, top, right, bottom);
        }
        let (x, y) = (self.x as f32, self.y as f32);
        (
            left.max(x),
            top.max(y),
            right.min(x + self.coverage.width() as f32),
            bottom.min(y + self.coverage.height() as f32),
        )
    }
}

/// A stroke rendered on its own, to blend onto the canvas with its top-left at `at`.
pub struct StrokeLayer {
    pub pixels: Pixmap,
    pub at: Point<i32>,
}

impl Floating {
    pub fn reorient(&mut self, orientation: Orientation) {
        self.pixels = reoriented(&self.pixels, orientation);
        self.reoriented = true;
    }
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
        self.draw_strokes(std::slice::from_ref(stroke))
    }

    /// Burns `strokes` into the canvas in order, as one change, and returns
    /// the change that undoes them all. Returns `None` when every stroke
    /// lies outside the canvas.
    pub fn draw_strokes(&mut self, strokes: &[Stroke]) -> Option<Change> {
        let layers: Vec<(StrokeLayer, BlendMode)> = strokes
            .iter()
            .filter_map(|stroke| Some((self.render_stroke(stroke)?, stroke.blend)))
            .collect();
        let left = layers.iter().map(|(layer, _)| layer.at.x).min()?;
        let top = layers.iter().map(|(layer, _)| layer.at.y).min()?;
        let right = layers
            .iter()
            .map(|(layer, _)| layer.at.x + layer.pixels.width() as i32)
            .max()?;
        let bottom = layers
            .iter()
            .map(|(layer, _)| layer.at.y + layer.pixels.height() as i32)
            .max()?;
        // Layers are clipped to the canvas, so this is too
        let undo = self.snapshot(IntRect::from_ltrb(left, top, right, bottom)?)?;
        for (layer, blend_mode) in &layers {
            self.pixmap.draw_pixmap(
                layer.at.x,
                layer.at.y,
                layer.pixels.as_ref(),
                &PixmapPaint {
                    blend_mode: *blend_mode,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
        }
        self.version += 1;
        Some(undo)
    }

    /// Renders `stroke` on its own transparent layer, clipped to the canvas,
    /// exactly as [`Self::draw_stroke`] will blend it in. Used to preview the
    /// stroke being drawn.
    ///
    /// The whole stroke is drawn opaque and then made translucent at once, so
    /// a translucent stroke does not darken where it crosses itself.
    pub fn render_stroke(&self, stroke: &Stroke) -> Option<StrokeLayer> {
        let rect = self.stroke_rect(stroke)?;
        let mut pixels = Pixmap::new(rect.width(), rect.height())?;
        let transform = Transform::from_translate(-rect.x() as f32, -rect.y() as f32);

        let color = to_skia_color(stroke.color);
        let mut opaque = color;
        opaque.set_alpha(1.0);
        match stroke.brush {
            Brush::Pen | Brush::Pencil | Brush::NaturalPencil | Brush::Crayon | Brush::Marker => {
                draw_line(&mut pixels, stroke, opaque, transform)
            }
            Brush::Airbrush => spray(&mut pixels, stroke, opaque, transform),
            Brush::Calligraphy => draw_nib(&mut pixels, stroke, opaque, transform),
        }
        if matches!(stroke.brush, Brush::NaturalPencil | Brush::Crayon) {
            add_grain(&mut pixels, Point::new(rect.x(), rect.y()), stroke.brush);
        }

        let opacity = color.alpha()
            * if stroke.brush == Brush::Marker {
                0.5
            } else {
                1.0
            };
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
        if let Some(mask) = &stroke.mask {
            let width = pixels.width() as i32;
            for (i, pixel) in pixels.pixels_mut().iter_mut().enumerate() {
                let (x, y) = (rect.x() + i as i32 % width, rect.y() + i as i32 / width);
                let coverage = mask.at(x, y) as u16;
                if coverage < 255 {
                    let scale = |c: u8| ((c as u16 * coverage + 127) / 255) as u8;
                    *pixel = PremultipliedColorU8::from_rgba(
                        scale(pixel.red()),
                        scale(pixel.green()),
                        scale(pixel.blue()),
                        scale(pixel.alpha()),
                    )
                    .unwrap_or(PremultipliedColorU8::TRANSPARENT);
                }
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
    /// Returns `None` when `position` is outside the canvas or filling would
    /// change nothing, as when the region already has the solid fill's color.
    pub fn flood_fill(
        &mut self,
        position: Point<Pixels>,
        fill: Fill,
        tolerance: u8,
    ) -> Option<Change> {
        let (seed_x, seed_y) = self.pixel_at(position)?;
        let width = self.pixmap.width() as usize;
        let target = self.pixmap.pixels()[seed_y * width + seed_x].demultiply();
        let foreground = to_skia_color(fill.foreground).to_color_u8();
        let background = to_skia_color(fill.background).to_color_u8();
        if fill.pattern.is_solid() && target == foreground {
            return None;
        }
        let spans = self.region(seed_x, seed_y, tolerance);

        let left = spans.iter().map(|&(_, start, _)| start).min()?;
        let right = spans.iter().map(|&(_, _, end)| end).max()?;
        let top = spans.iter().map(|&(y, _, _)| y).min()?;
        let bottom = spans.iter().map(|&(y, _, _)| y).max()? + 1;
        let rect = IntRect::from_ltrb(left as i32, top as i32, right as i32, bottom as i32)?;
        let undo = self.snapshot(rect)?;

        let (foreground, background) = (foreground.premultiply(), background.premultiply());
        let pixels = self.pixmap.pixels_mut();
        let mut changed = false;
        for (y, start, end) in spans {
            for x in start..end {
                let pixel = &mut pixels[y * width + x];
                let color = if fill.pattern.is_set(x, y) {
                    foreground
                } else {
                    background
                };
                let filled = source_over(color, *pixel);
                changed |= filled != *pixel;
                *pixel = filled;
            }
        }
        // Refilling a pattern with itself leaves every pixel as it was
        if !changed {
            return None;
        }
        self.version += 1;
        Some(undo)
    }

    /// The pixels connected to (`seed_x`, `seed_y`) whose color is within
    /// `tolerance` of its color in every channel, as rows of (y, start, end)
    /// with `end` past the last pixel.
    fn region(&self, seed_x: usize, seed_y: usize, tolerance: u8) -> Vec<(usize, usize, usize)> {
        let (width, height) = (self.pixmap.width() as usize, self.pixmap.height() as usize);
        let target = self.pixmap.pixels()[seed_y * width + seed_x].demultiply();
        let matches = |c: ColorU8| {
            c.red().abs_diff(target.red()) <= tolerance
                && c.green().abs_diff(target.green()) <= tolerance
                && c.blue().abs_diff(target.blue()) <= tolerance
                && c.alpha().abs_diff(target.alpha()) <= tolerance
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

        spans
    }

    /// A mask over the inside of `outline`, closed from its last point back
    /// to its first, with anti-aliased edges. Returns `None` when it lies
    /// entirely outside the canvas.
    pub fn shape_mask(&self, outline: &Stroke) -> Option<ClipMask> {
        let (first, rest) = outline.points.split_first()?;
        let mut builder = PathBuilder::new();
        builder.move_to(first.x.as_f32(), first.y.as_f32());
        for p in rest {
            builder.line_to(p.x.as_f32(), p.y.as_f32());
        }
        builder.close();
        let path = builder.finish()?;
        let bounds = path.bounds();
        let rect = self.clip(
            bounds.left() - 1.0,
            bounds.top() - 1.0,
            bounds.right() + 1.0,
            bounds.bottom() + 1.0,
        )?;
        let mut coverage = tiny_skia::Mask::new(rect.width(), rect.height())?;
        let transform = Transform::from_translate(-rect.x() as f32, -rect.y() as f32);
        coverage.fill_path(&path, FillRule::Winding, true, transform);
        Some(ClipMask {
            x: rect.x(),
            y: rect.y(),
            coverage,
            inverted: false,
        })
    }

    /// A mask over the region around `position` that has about the same
    /// color, as [`Self::flood_fill`] finds it, softened by a pixel at its
    /// edges. Returns `None` when `position` is outside the canvas.
    pub fn region_mask(&self, position: Point<Pixels>, tolerance: u8) -> Option<ClipMask> {
        let (seed_x, seed_y) = self.pixel_at(position)?;
        let spans = self.region(seed_x, seed_y, tolerance);
        // One pixel round the region, for the softened edge
        let left = spans
            .iter()
            .map(|&(_, start, _)| start)
            .min()?
            .saturating_sub(1);
        let top = spans.iter().map(|&(y, _, _)| y).min()?.saturating_sub(1);
        let right =
            (spans.iter().map(|&(_, _, end)| end).max()? + 1).min(self.pixmap.width() as usize);
        let bottom =
            (spans.iter().map(|&(y, _, _)| y).max()? + 2).min(self.pixmap.height() as usize);
        let (width, height) = (right - left, bottom - top);
        let mut inside = vec![0u8; width * height];
        for (y, start, end) in spans {
            let row = (y - top) * width;
            inside[row + start - left..row + end - left].fill(255);
        }
        // Average each pixel with its neighbors, so the edge fades over a pixel
        let at = |x: isize, y: isize| {
            if x < 0 || y < 0 || x >= width as isize || y >= height as isize {
                0
            } else {
                inside[y as usize * width + x as usize] as u32
            }
        };
        let softened = (0..height as isize)
            .flat_map(|y| (0..width as isize).map(move |x| (x, y)))
            .map(|(x, y)| {
                let sum: u32 = (-1..=1)
                    .flat_map(|dy| (-1..=1).map(move |dx| (dx, dy)))
                    .map(|(dx, dy)| at(x + dx, y + dy))
                    .sum();
                (sum / 9) as u8
            })
            .collect();
        let size = tiny_skia::IntSize::from_wh(width as u32, height as u32)?;
        Some(ClipMask {
            x: left as i32,
            y: top as i32,
            coverage: tiny_skia::Mask::from_vec(softened, size)?,
            inverted: false,
        })
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

    /// Flips or turns the whole canvas, and returns the change that undoes it.
    pub fn reorient(&mut self, orientation: Orientation) -> Change {
        let pixels = reoriented(&self.pixmap, orientation);
        self.replace(pixels)
    }

    /// Replaces the whole canvas with `pixels`, taking on their size, and
    /// returns the change that undoes it.
    pub fn replace(&mut self, pixels: Pixmap) -> Change {
        self.apply(Change::Replace(pixels))
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
                Change::Patch { x, y, pixels: rect }
            }
            Change::Replace(pixmap) => Change::Replace(std::mem::replace(&mut self.pixmap, pixmap)),
        }
    }

    /// Returns a copy of the pixels in `rect`, clipped to the canvas. With a
    /// `mask`, the pixels it leaves out are transparent.
    pub fn copy(&self, rect: PixelRect, mask: Option<&SelectionMask>) -> Option<Pixmap> {
        let source = self.clip_rect(rect)?;
        let mut pixels = self.pixmap.clone_rect(source)?;
        if let Some(mask) = mask {
            let width = pixels.width();
            for (i, pixel) in pixels.pixels_mut().iter_mut().enumerate() {
                let (x, y) = in_selection(rect, source, i as u32, width);
                if !mask.contains(x, y) {
                    *pixel = PremultipliedColorU8::TRANSPARENT;
                }
            }
        }
        Some(pixels)
    }

    /// Paints `rect` with the background color, or only the pixels `mask`
    /// covers, and returns the change that undoes it.
    pub fn erase(&mut self, rect: PixelRect, mask: Option<&SelectionMask>) -> Option<Change> {
        let source = self.clip_rect(rect)?;
        let undo = self.snapshot(source)?;
        let Some(mask) = mask else {
            self.pixmap.fill_rect(
                source.to_rect(),
                &Paint {
                    shader: tiny_skia::Shader::SolidColor(self.background),
                    blend_mode: tiny_skia::BlendMode::Source,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
            self.version += 1;
            return Some(undo);
        };
        let background = self.background.premultiply().to_color_u8();
        let canvas_width = self.pixmap.width() as usize;
        let pixels = self.pixmap.pixels_mut();
        for i in 0..source.width() * source.height() {
            let (x, y) = in_selection(rect, source, i, source.width());
            if mask.contains(x, y) {
                let canvas_x = (rect.x + x) as usize;
                let canvas_y = (rect.y + y) as usize;
                pixels[canvas_y * canvas_width + canvas_x] = background;
            }
        }
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

    /// Draws text rendered by `TextRenderer::render` so it starts at
    /// `anchor`, and returns the change that undoes it.
    pub fn stamp_text(
        &mut self,
        (pixels, margin): &(Pixmap, u32),
        anchor: Point<i32>,
    ) -> Option<Change> {
        self.stamp(pixels, point(anchor.x - *margin as i32, anchor.y))
    }

    /// Draws `text` starting at `anchor`, its top-left, and returns the
    /// change that undoes it. Lines are separated by `\n`.
    pub fn draw_text(
        &mut self,
        renderer: &mut TextRenderer,
        text: &str,
        font: FontChoice,
        size: f32,
        color: Hsla,
        anchor: Point<i32>,
    ) -> Option<Change> {
        let rendered = renderer.render(text, font, size, color)?;
        self.stamp_text(&rendered, anchor)
    }

    /// Lifts the pixels in `rect` off the canvas, or only those `mask`
    /// covers, leaving background behind.
    pub fn lift(&mut self, rect: PixelRect, mask: Option<&SelectionMask>) -> Option<Floating> {
        let source = self.clip_rect(rect)?;
        let pixels = self.copy(rect, mask)?;
        let before = self.pixmap.clone();
        self.erase(rect, mask);
        Some(Floating {
            pixels,
            reoriented: false,
            before,
            source: Some(source),
        })
    }

    /// Floats `pixels` above the canvas, as when pasting.
    pub fn float(&self, pixels: Pixmap) -> Floating {
        Floating {
            pixels,
            reoriented: false,
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
        if keep && target == floating.source && !floating.reoriented {
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
    /// sampling so pixels stay sharp, as opaque BGRA bytes for gpui.
    ///
    /// The canvas is displayed at `pixels_per_canvas_pixel` device pixels per
    /// canvas pixel. `origin` and `region` select the device pixels to render,
    /// relative to the displayed canvas's top-left. Pixels beyond the canvas,
    /// for example while a resize drag is enlarging it, show the background.
    ///
    /// Transparency shows as a checkerboard of `checker_size` device pixel
    /// squares, which stay the same size on screen at any zoom, and `grid`
    /// lines are drawn on top.
    pub fn render_view(
        &self,
        origin: Point<u32>,
        region: Size<u32>,
        pixels_per_canvas_pixel: f32,
        checker_size: f32,
        grid: Grid,
    ) -> Vec<u8> {
        let background = bgra(self.background.premultiply().to_color_u8());
        let mut bytes = render_scaled(
            &self.pixmap,
            background,
            origin,
            region,
            pixels_per_canvas_pixel,
        );
        let checker = |device: u32| (device as f32 / checker_size) as u32 % 2;
        for (i, pixel) in bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let alpha = pixel[3];
            if alpha == 255 {
                continue;
            }
            let x = origin.x + (i as u32 % region.width);
            let y = origin.y + (i as u32 / region.width);
            let square = if checker(x) == checker(y) {
                CHECKER_LIGHT
            } else {
                CHECKER_DARK
            };
            // Premultiplied pixel over the opaque checker
            let keep = 255 - alpha as u16;
            for channel in &mut pixel[..3] {
                *channel = (*channel as u16 + (square as u16 * keep + 127) / 255) as u8;
            }
            pixel[3] = 255;
        }
        self.draw_grid(&mut bytes, origin, region, pixels_per_canvas_pixel, grid);
        bytes
    }

    /// Darkens the device pixels where a canvas pixel or tile begins, so each
    /// line is one device pixel wide. The canvas's own edges need no line.
    fn draw_grid(
        &self,
        bytes: &mut [u8],
        origin: Point<u32>,
        region: Size<u32>,
        pixels_per_canvas_pixel: f32,
        grid: Grid,
    ) {
        let show_pixels = grid.pixels && pixels_per_canvas_pixel >= MIN_PIXEL_GRID_SPACING;
        if !show_pixels && grid.tile.is_none() {
            return;
        }
        // The canvas pixel each device pixel samples, as in `render_scaled`
        let index = |device: u32| ((device as f32 + 0.5) / pixels_per_canvas_pixel) as u32;
        // Which line, if any, runs along device pixel `device`
        let line = |device: u32| {
            let current = index(device);
            // The canvas's first pixel lies on its edge
            let starts_pixel = current > 0 && index(device - 1) != current;
            if !starts_pixel {
                return None;
            }
            let on_tile = grid.tile.is_some_and(|tile| current % tile == 0);
            if on_tile {
                Some(TILE_GRID_LINE)
            } else if show_pixels {
                Some(PIXEL_GRID_LINE)
            } else {
                None
            }
        };
        let (width, height) = (self.pixmap.width(), self.pixmap.height());
        let columns: Vec<_> = (origin.x..origin.x + region.width).map(&line).collect();
        for (row, y) in (origin.y..origin.y + region.height).enumerate() {
            let row_line = line(y);
            // No lines past the canvas, as while a resize drag enlarges it
            let row_inside = index(y) < height;
            for (column, x) in columns.iter().enumerate() {
                let inside = row_inside && index(origin.x + column as u32) < width;
                // The tile line wins where it crosses a pixel line
                let color = match (row_line, *x) {
                    (Some(a), Some(b)) => Some(if a.1 >= b.1 { a } else { b }),
                    (a, b) => a.or(b),
                };
                let Some((gray, alpha)) = color.filter(|_| inside) else {
                    continue;
                };
                let pixel = &mut bytes[(row * region.width as usize + column) * 4..][..3];
                for channel in pixel {
                    *channel = ((*channel as u16 * (255 - alpha as u16)
                        + gray as u16 * alpha as u16
                        + 127)
                        / 255) as u8;
                }
            }
        }
    }

    /// The canvas image.
    pub fn pixels(&self) -> &Pixmap {
        &self.pixmap
    }

    /// Erases along `stroke` to transparency, starting from `base`, the canvas
    /// as it was when the stroke began. Erasing is applied straight to the
    /// canvas so it can be seen as it is drawn; repeating it from `base` as
    /// the stroke grows keeps each pixel erased once.
    ///
    /// Returns the change that restores `base`, covering the stroke so far.
    pub fn erase_stroke(&mut self, base: &Pixmap, stroke: &Stroke) -> Option<Change> {
        let layer = self.render_stroke(stroke)?;
        let rect = IntRect::from_xywh(
            layer.at.x,
            layer.at.y,
            layer.pixels.width(),
            layer.pixels.height(),
        )?;
        let before = base.clone_rect(rect)?;
        let (x, y) = (rect.x() as u32, rect.y() as u32);
        self.write(x, y, &before);
        self.pixmap.draw_pixmap(
            rect.x(),
            rect.y(),
            layer.pixels.as_ref(),
            &PixmapPaint {
                blend_mode: tiny_skia::BlendMode::DestinationOut,
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
        self.version += 1;
        Some(Change::Patch {
            x,
            y,
            pixels: before,
        })
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

    /// Whether `stroke` would draw anywhere on the canvas.
    pub fn reaches(&self, stroke: &Stroke) -> bool {
        self.stroke_rect(stroke).is_some()
    }

    /// The part of the canvas `stroke` can draw on, or `None` when it lies
    /// entirely outside.
    fn stroke_rect(&self, stroke: &Stroke) -> Option<IntRect> {
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
        let (mut left, mut top, mut right, mut bottom) =
            (left - reach, top - reach, right + reach, bottom + reach);
        // Rendering only this much of the stroke keeps it inside its clip
        if let Some(clip) = stroke.clip {
            left = left.max(clip.x as f32);
            top = top.max(clip.y as f32);
            right = right.min((clip.x + clip.width as i32) as f32);
            bottom = bottom.min((clip.y + clip.height as i32) as f32);
        }
        if let Some(mask) = &stroke.mask {
            (left, top, right, bottom) = mask.limit((left, top, right, bottom));
        }
        self.clip(left, top, right, bottom)
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

/// Where the `i`th pixel of `source`, part of the selection `rect` clipped to
/// the canvas and `width` pixels wide, lies from the selection's top-left.
fn in_selection(rect: PixelRect, source: IntRect, i: u32, width: u32) -> (i32, i32) {
    (
        source.x() - rect.x + (i % width) as i32,
        source.y() - rect.y + (i / width) as i32,
    )
}

/// A `width` by `height` piece painted with `fill`, starting at the pattern's
/// top-left, to preview the fill.
pub fn fill_swatch(fill: Fill, width: u32, height: u32) -> Option<Pixmap> {
    let mut pixmap = Pixmap::new(width, height)?;
    let foreground = to_skia_color(fill.foreground).premultiply().to_color_u8();
    let background = to_skia_color(fill.background).premultiply().to_color_u8();
    for (i, pixel) in pixmap.pixels_mut().iter_mut().enumerate() {
        let (x, y) = (i % width as usize, i / width as usize);
        *pixel = if fill.pattern.is_set(x, y) {
            foreground
        } else {
            background
        };
    }
    Some(pixmap)
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
        let mut fill = paint.clone();
        if let Some(shader) = stroke.gradient.as_ref().and_then(gradient_shader) {
            fill.shader = shader;
        }
        pixels.fill_path(&path, &fill, FillRule::Winding, transform, None);
    }
    // A filled shape with no outline
    if stroke.width <= px(0.0) {
        return;
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

/// Roughens a natural pencil or crayon line drawn on `pixels`, whose top-left
/// is at `at` on the canvas. The grain belongs to the canvas position rather
/// than the stroke, like paper's, so the part already drawn stays the same as
/// the stroke grows.
fn add_grain(pixels: &mut Pixmap, at: Point<i32>, brush: Brush) {
    let width = pixels.width() as i32;
    for (i, pixel) in pixels.pixels_mut().iter_mut().enumerate() {
        if pixel.alpha() == 0 {
            continue;
        }
        let x = at.x + i as i32 % width;
        let y = at.y + i as i32 / width;
        let fine = noise(x, y, 0);
        let coarse = noise(x.div_euclid(3), y.div_euclid(3), 1);
        let coverage = match brush {
            // Graphite never covers fully, and is lighter where the grain is low
            Brush::NaturalPencil => 0.3 + 0.5 * (0.8 * fine + 0.2 * coarse),
            // Wax covers fully but skips the low spots, which come in clumps
            _ if 0.4 * fine + 0.6 * coarse < 0.3 => 0.0,
            _ => 1.0,
        };
        let scale = |c: u8| (c as f32 * coverage).round() as u8;
        *pixel = PremultipliedColorU8::from_rgba(
            scale(pixel.red()),
            scale(pixel.green()),
            scale(pixel.blue()),
            scale(pixel.alpha()),
        )
        .unwrap_or(*pixel);
    }
}

/// Returns a number in `0.0..1.0` fixed for the pixel at (`x`, `y`) and the
/// `layer` of noise.
fn noise(x: i32, y: i32, layer: u64) -> f32 {
    let position = ((x as u32 as u64) << 32) | y as u32 as u64;
    Random(position ^ layer.wrapping_mul(0xd6e8_feb8_6659_fd93)).next()
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
        points => points
            .windows(2)
            .map(|pair| (xy(&pair[0]), xy(&pair[1])))
            .collect(),
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

/// A shader painting `gradient`, in canvas coordinates.
fn gradient_shader(gradient: &Gradient) -> Option<tiny_skia::Shader<'static>> {
    let stops = |stops: &[(f32, Hsla)]| {
        stops
            .iter()
            .map(|&(position, color)| tiny_skia::GradientStop::new(position, to_skia_color(color)))
            .collect()
    };
    let at = |p: Point<Pixels>| tiny_skia::Point::from_xy(p.x.as_f32(), p.y.as_f32());
    match gradient {
        Gradient::Linear { from, to, stops: s } => tiny_skia::LinearGradient::new(
            at(*from),
            at(*to),
            stops(s),
            tiny_skia::SpreadMode::Pad,
            Transform::identity(),
        ),
        Gradient::Radial {
            center,
            radius,
            stops: s,
        } => tiny_skia::RadialGradient::new(
            at(*center),
            0.0,
            at(*center),
            *radius,
            stops(s),
            tiny_skia::SpreadMode::Pad,
            Transform::identity(),
        ),
    }
}

fn blank(width: u32, height: u32, background: Color) -> Result<Pixmap> {
    let mut pixmap = Pixmap::new(width, height)
        .ok_or_else(|| anyhow!("invalid canvas size {width}x{height}"))?;
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

    use crate::pattern::PATTERNS;

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

    fn solid(color: Hsla) -> Fill {
        Fill {
            pattern: Pattern::SOLID,
            foreground: color,
            background: color,
        }
    }

    fn pattern(name: &str) -> Pattern {
        *PATTERNS.iter().find(|p| p.name == name).unwrap()
    }

    fn pattern_fill(pattern: Pattern) -> Fill {
        Fill {
            pattern,
            foreground: rgb(0x000000).into(),
            background: rgb(0xff0000).into(),
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
    fn draws_several_strokes_as_one_change() {
        let mut canvas = canvas(100, 100);
        let before = canvas.pixmap.clone();
        let fill = Stroke {
            width: px(0.0),
            filled: true,
            closed: true,
            ..stroke(
                &[(20.0, 20.0), (80.0, 20.0), (80.0, 80.0), (20.0, 80.0)],
                0xff0000,
                0.0,
            )
        };
        let outline = Stroke {
            closed: true,
            sharp_corners: true,
            ..stroke(
                &[(20.0, 20.0), (80.0, 20.0), (80.0, 80.0), (20.0, 80.0)],
                0x0000ff,
                6.0,
            )
        };
        let undo = canvas.draw_strokes(&[fill, outline]).unwrap();
        // The outline is drawn over the fill
        assert_eq!(rgb_at(&canvas, 50, 50), (255, 0, 0));
        assert_eq!(rgb_at(&canvas, 50, 20), (0, 0, 255));
        assert_eq!(rgb_at(&canvas, 50, 10), (255, 255, 255));
        canvas.apply(undo);
        assert_eq!(canvas.pixmap.data(), before.data());
    }

    #[test]
    fn a_fill_without_an_outline_stays_inside_its_points() {
        let mut canvas = canvas(100, 100);
        let fill = Stroke {
            width: px(0.0),
            filled: true,
            closed: true,
            ..stroke(
                &[(20.0, 20.0), (80.0, 20.0), (80.0, 80.0), (20.0, 80.0)],
                0xff0000,
                0.0,
            )
        };
        canvas.draw_stroke(&fill).unwrap();
        assert_eq!(rgb_at(&canvas, 21, 50), (255, 0, 0));
        assert_eq!(rgb_at(&canvas, 18, 50), (255, 255, 255));
    }

    #[test]
    fn adding_brightens_and_multiplying_darkens() {
        let mut canvas = Canvas::new(30, 10, rgb(0x404040).into()).unwrap();
        let dot = |x: f32, color: u32, blend_mode: BlendMode| Stroke {
            blend: blend_mode,
            ..stroke(&[(x, 5.0)], color, 8.0)
        };
        canvas
            .draw_strokes(&[
                dot(5.0, 0x404040, BlendMode::Plus),
                dot(15.0, 0x808080, BlendMode::Multiply),
                dot(25.0, 0x808080, BlendMode::SourceOver),
            ])
            .unwrap();
        assert_eq!(rgb_at(&canvas, 5, 5), (128, 128, 128));
        assert_eq!(rgb_at(&canvas, 15, 5), (32, 32, 32));
        assert_eq!(rgb_at(&canvas, 25, 5), (128, 128, 128));
    }

    #[test]
    fn clipped_strokes_stay_inside_their_clip() {
        let mut canvas = canvas(100, 100);
        let clipped = Stroke {
            clip: Some(PixelRect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            }),
            ..stroke(&[(50.0, 10.0), (50.0, 90.0)], 0x000000, 10.0)
        };
        canvas.draw_stroke(&clipped).unwrap();
        assert_eq!(rgb_at(&canvas, 50, 49), (0, 0, 0));
        assert_eq!(rgb_at(&canvas, 50, 50), (255, 255, 255));
        assert!(canvas.reaches(&clipped));

        let elsewhere = Stroke {
            clip: Some(PixelRect {
                x: 0,
                y: 60,
                width: 20,
                height: 20,
            }),
            ..stroke(&[(50.0, 10.0), (50.0, 40.0)], 0x000000, 10.0)
        };
        assert!(!canvas.reaches(&elsewhere));
        assert!(canvas.draw_stroke(&elsewhere).is_none());
    }

    #[test]
    fn fills_with_gradients() {
        let mut canvas = canvas(100, 100);
        let square = |gradient| Stroke {
            width: px(0.0),
            filled: true,
            closed: true,
            gradient: Some(gradient),
            ..stroke(
                &[(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (0.0, 100.0)],
                0x000000,
                0.0,
            )
        };
        let (red, blue) = (rgb(0xff0000).into(), rgb(0x0000ff).into());
        canvas.draw_stroke(&square(Gradient::Linear {
            from: point(px(0.0), px(0.0)),
            to: point(px(100.0), px(0.0)),
            stops: vec![(0.0, red), (1.0, blue)],
        }));
        let (left, middle, right) = (
            rgb_at(&canvas, 1, 50),
            rgb_at(&canvas, 50, 50),
            rgb_at(&canvas, 98, 50),
        );
        assert!(left.0 > 240 && left.2 < 15, "{left:?}");
        assert!(right.2 > 240 && right.0 < 15, "{right:?}");
        assert!(
            middle.0.abs_diff(127) < 10 && middle.2.abs_diff(127) < 10,
            "{middle:?}"
        );

        canvas.draw_stroke(&square(Gradient::Radial {
            center: point(px(50.0), px(50.0)),
            radius: 50.0,
            stops: vec![(0.0, red), (1.0, gpui_kit::transparent_black())],
        }));
        assert!(rgb_at(&canvas, 50, 50).0 > 240);
        // Past the radius the last stop, transparent, leaves the linear fill
        assert_eq!(rgb_at(&canvas, 98, 98), right);
    }

    fn square(x: f32, y: f32, side: f32) -> Stroke {
        Stroke {
            closed: true,
            ..stroke(
                &[(x, y), (x + side, y), (x + side, y + side), (x, y + side)],
                0x000000,
                0.0,
            )
        }
    }

    fn masked(mask: ClipMask) -> Stroke {
        Stroke {
            width: px(0.0),
            filled: true,
            mask: Some(std::sync::Arc::new(mask)),
            ..square(0.0, 0.0, 100.0)
        }
    }

    #[test]
    fn shape_masks_keep_strokes_inside_with_soft_edges() {
        let mut canvas = canvas(100, 100);
        let circle = Stroke::shape(
            crate::tool::ShapeKind::Ellipse,
            point(px(20.0), px(20.0)),
            point(px(80.0), px(80.0)),
            Stroke::default(),
        );
        let mask = canvas.shape_mask(&circle).unwrap();
        canvas.draw_stroke(&masked(mask.clone())).unwrap();
        assert_eq!(rgb_at(&canvas, 50, 50), (0, 0, 0));
        assert_eq!(rgb_at(&canvas, 22, 22), (255, 255, 255));
        // Where the edge cuts across pixels, they are neither black nor white
        let edge: Vec<u8> = (26..32).map(|i| rgb_at(&canvas, i, i).0).collect();
        assert!(edge.iter().any(|&v| v > 0 && v < 255), "{edge:?}");

        let mut outside = self::canvas(100, 100);
        outside.draw_stroke(&masked(mask.inverted())).unwrap();
        assert_eq!(rgb_at(&outside, 50, 50), (255, 255, 255));
        assert_eq!(rgb_at(&outside, 22, 22), (0, 0, 0));
    }

    #[test]
    fn region_masks_follow_what_is_drawn() {
        let mut canvas = canvas(100, 100);
        // A red square to shade, inside a white canvas
        canvas.draw_stroke(&Stroke {
            width: px(0.0),
            filled: true,
            color: rgb(0xff0000).into(),
            ..square(30.0, 30.0, 40.0)
        });
        let mask = canvas.region_mask(point(px(50.0), px(50.0)), 10).unwrap();
        canvas
            .draw_stroke(&Stroke {
                color: rgb(0x0000ff).into(),
                ..masked(mask)
            })
            .unwrap();
        assert_eq!(rgb_at(&canvas, 50, 50), (0, 0, 255));
        assert_eq!(rgb_at(&canvas, 32, 32), (0, 0, 255));
        assert_eq!(rgb_at(&canvas, 10, 10), (255, 255, 255));
        assert_eq!(rgb_at(&canvas, 28, 50), (255, 255, 255));
        assert!(canvas.region_mask(point(px(150.0), px(50.0)), 10).is_none());
    }

    fn mask_at(mask: &ClipMask, x: i32, y: i32) -> u8 {
        mask.at(x, y)
    }

    #[test]
    fn combines_masks_where_all_or_any_cover() {
        let canvas = canvas(100, 100);
        let left = canvas.shape_mask(&square(0.0, 0.0, 60.0)).unwrap();
        let right = canvas.shape_mask(&square(40.0, 40.0, 60.0)).unwrap();

        let both = ClipMask::all(vec![left.clone(), right.clone()]).unwrap();
        assert_eq!(mask_at(&both, 50, 50), 255);
        assert_eq!(mask_at(&both, 10, 10), 0);
        assert_eq!(mask_at(&both, 90, 90), 0);
        // Kept as small as the overlap
        assert_eq!(both.bounds(), (39, 39, 61, 61));

        let either = ClipMask::any(vec![left.clone(), right.clone()]).unwrap();
        assert_eq!(mask_at(&either, 10, 10), 255);
        assert_eq!(mask_at(&either, 90, 90), 255);
        assert_eq!(mask_at(&either, 90, 10), 0);

        let apart = canvas.shape_mask(&square(80.0, 0.0, 10.0)).unwrap();
        assert!(ClipMask::all(vec![left.clone(), apart]).is_none());
    }

    #[test]
    fn cuts_one_mask_out_of_another() {
        let canvas = canvas(100, 100);
        let moon = Stroke::shape(
            crate::tool::ShapeKind::Ellipse,
            point(px(10.0), px(10.0)),
            point(px(90.0), px(90.0)),
            Stroke::default(),
        );
        let bite = Stroke::shape(
            crate::tool::ShapeKind::Ellipse,
            point(px(30.0), px(0.0)),
            point(px(110.0), px(80.0)),
            Stroke::default(),
        );
        let crescent = ClipMask::all(vec![
            canvas.shape_mask(&moon).unwrap(),
            canvas.shape_mask(&bite).unwrap().inverted(),
        ])
        .unwrap();
        // The lower left of the moon is left; the bitten part and the sky are not
        assert_eq!(mask_at(&crescent, 20, 60), 255);
        assert_eq!(mask_at(&crescent, 60, 40), 0);
        assert_eq!(mask_at(&crescent, 5, 5), 0);
    }

    #[test]
    fn combining_outsides_follows_de_morgan() {
        let canvas = canvas(100, 100);
        let a = canvas.shape_mask(&square(0.0, 0.0, 30.0)).unwrap();
        let b = canvas.shape_mask(&square(70.0, 70.0, 30.0)).unwrap();
        // Outside both: neither square
        let outside_both = ClipMask::all(vec![a.clone().inverted(), b.clone().inverted()]).unwrap();
        assert_eq!(mask_at(&outside_both, 50, 50), 255);
        assert_eq!(mask_at(&outside_both, 10, 10), 0);
        assert_eq!(mask_at(&outside_both, 90, 90), 0);
        // Outside either: everywhere, as the squares do not overlap
        let outside_either = ClipMask::any(vec![a.inverted(), b.inverted()]).unwrap();
        for (x, y) in [(10, 10), (50, 50), (90, 90)] {
            assert_eq!(mask_at(&outside_either, x, y), 255);
        }
    }

    #[test]
    fn strokes_outside_a_mask_do_not_reach_the_canvas() {
        let canvas = canvas(100, 100);
        let mask = canvas.shape_mask(&square(0.0, 0.0, 20.0)).unwrap();
        let far = Stroke {
            mask: Some(std::sync::Arc::new(mask.clone())),
            ..stroke(&[(60.0, 60.0), (90.0, 60.0)], 0x000000, 4.0)
        };
        assert!(!canvas.reaches(&far));
        let everywhere_else = Stroke {
            mask: Some(std::sync::Arc::new(mask.inverted())),
            ..far
        };
        assert!(canvas.reaches(&everywhere_else));
        assert!(canvas.shape_mask(&square(200.0, 200.0, 20.0)).is_none());
    }

    #[test]
    fn strokes_all_outside_draw_nothing() {
        let mut canvas = canvas(50, 50);
        let outside = stroke(&[(200.0, 200.0), (300.0, 200.0)], 0x000000, 4.0);
        assert!(canvas.draw_strokes(&[outside]).is_none());
        assert!(canvas.draw_strokes(&[]).is_none());
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
            ..stroke(
                &[(5.0, 5.0), (35.0, 5.0), (35.0, 35.0), (5.0, 35.0)],
                0x000000,
                2.0,
            )
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
            .flood_fill(
                point(px(50.0), px(50.0)),
                solid(rgb(0xff0000).into()),
                DEFAULT_FILL_TOLERANCE,
            )
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
        let result = canvas.flood_fill(
            point(px(5.0), px(5.0)),
            solid(rgb(WHITE).into()),
            DEFAULT_FILL_TOLERANCE,
        );
        assert!(result.is_none());
    }

    #[test]
    fn flood_fill_paints_the_pattern_in_both_colors() {
        let mut canvas = canvas(16, 16);
        let checkerboard = pattern("Checkerboard");
        canvas
            .flood_fill(point(px(5.0), px(5.0)), pattern_fill(checkerboard), 0)
            .unwrap();
        for y in 0..16 {
            for x in 0..16 {
                let expected = if checkerboard.is_set(x as usize, y as usize) {
                    (0, 0, 0)
                } else {
                    (255, 0, 0)
                };
                assert_eq!(rgb_at(&canvas, x, y), expected, "({x}, {y})");
            }
        }
    }

    #[test]
    fn pattern_lines_up_with_the_canvas_across_separate_fills() {
        // A wall splits the canvas into two regions filled one at a time
        let mut canvas = canvas(20, 8);
        let mut paint = Paint::default();
        paint.set_color_rgba8(0, 0, 255, 255);
        canvas.pixmap.fill_rect(
            tiny_skia::Rect::from_xywh(9.0, 0.0, 1.0, 8.0).unwrap(),
            &paint,
            Transform::identity(),
            None,
        );
        let diagonals = pattern("Rising diagonals");
        canvas
            .flood_fill(point(px(2.0), px(2.0)), pattern_fill(diagonals), 0)
            .unwrap();
        canvas
            .flood_fill(point(px(15.0), px(2.0)), pattern_fill(diagonals), 0)
            .unwrap();
        // Both sides follow the same tiling, as if filled at once
        for y in 0..8 {
            for x in (0..20).filter(|&x| x != 9) {
                let black = rgb_at(&canvas, x, y) == (0, 0, 0);
                assert_eq!(
                    black,
                    diagonals.is_set(x as usize, y as usize),
                    "({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn fill_swatch_matches_a_fill_of_the_canvas() {
        let bricks = pattern("Bricks");
        let swatch = fill_swatch(pattern_fill(bricks), 20, 12).unwrap();
        let mut canvas = canvas(20, 12);
        canvas
            .flood_fill(point(px(3.0), px(3.0)), pattern_fill(bricks), 0)
            .unwrap();
        assert_eq!(swatch.pixels(), canvas.pixmap.pixels());
    }

    #[test]
    fn refilling_a_pattern_with_itself_does_nothing() {
        let mut canvas = canvas(16, 16);
        let stripes = pattern("Horizontal lines");
        canvas
            .flood_fill(point(px(0.0), px(1.0)), pattern_fill(stripes), 0)
            .unwrap();
        // The red run between the stripes is already what the pattern paints there
        let again = canvas.flood_fill(point(px(0.0), px(1.0)), pattern_fill(stripes), 0);
        assert!(again.is_none());
    }

    #[test]
    fn flood_fill_tolerance_decides_whether_similar_colors_are_filled() {
        // White on the left, light gray on the right, 0x20 apart per channel
        let two_tone = || {
            let mut canvas = canvas(10, 10);
            let mut paint = Paint::default();
            paint.set_color_rgba8(0xdf, 0xdf, 0xdf, 0xff);
            canvas.pixmap.fill_rect(
                tiny_skia::Rect::from_xywh(5.0, 0.0, 5.0, 10.0).unwrap(),
                &paint,
                Transform::identity(),
                None,
            );
            canvas
        };
        let fill = |canvas: &mut Canvas, tolerance| {
            canvas.flood_fill(
                point(px(1.0), px(1.0)),
                solid(rgb(0xff0000).into()),
                tolerance,
            )
        };

        let mut exact = two_tone();
        fill(&mut exact, 0).unwrap();
        assert_eq!(rgb_at(&exact, 1, 1), (255, 0, 0));
        assert_eq!(rgb_at(&exact, 8, 1), (0xdf, 0xdf, 0xdf));

        let mut loose = two_tone();
        fill(&mut loose, 0x20).unwrap();
        assert_eq!(rgb_at(&loose, 8, 1), (255, 0, 0));
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
        let floating = canvas.lift(rect(2, 2, 4, 4), None).unwrap();
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
        let floating = canvas.lift(rect(2, 2, 4, 4), None).unwrap();
        assert!(canvas.drop_floating(floating, point(2, 2), true).is_none());
        assert_eq!(rgb_at(&canvas, 3, 3), (0, 0, 0));
    }

    #[test]
    fn discarding_lifted_pixels_deletes_them() {
        let mut canvas = canvas_with_block();
        let floating = canvas.lift(rect(2, 2, 4, 4), None).unwrap();
        let undo = canvas.drop_floating(floating, point(2, 2), false).unwrap();
        assert_eq!(rgb_at(&canvas, 3, 3), (255, 255, 255));
        canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 3, 3), (0, 0, 0));
    }

    #[test]
    fn pasting_clips_to_the_canvas_and_can_be_undone() {
        let mut canvas = canvas_with_block();
        let pixels = canvas.copy(rect(2, 2, 4, 4), None).unwrap();
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
        let undo = canvas.erase(rect(0, 0, 4, 4), None).unwrap();
        assert_eq!(rgb_at(&canvas, 3, 3), (255, 255, 255));
        // Outside the erased rect stays
        assert_eq!(rgb_at(&canvas, 5, 5), (0, 0, 0));
        canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 3, 3), (0, 0, 0));
    }

    /// A mask over a 4x4 box covering the triangle at its top-left corner.
    fn corner_triangle() -> SelectionMask {
        let outline = [point(0.0, 0.0), point(4.0, 0.0), point(0.0, 4.0)];
        SelectionMask::from_outline(&outline, 4, 4).unwrap()
    }

    #[test]
    fn mask_covers_the_pixels_centered_inside_the_outline() {
        let mask = corner_triangle();
        // Pixel (1, 1) is centered at (1.5, 1.5), inside the triangle's
        // diagonal; pixel (2, 2) at (2.5, 2.5) is past it
        assert!(mask.contains(0, 0));
        assert!(mask.contains(1, 1));
        assert!(mask.contains(3, 0));
        assert!(!mask.contains(2, 2));
        assert!(!mask.contains(3, 3));
        assert!(!mask.contains(-1, 0));
    }

    #[test]
    fn mask_with_nothing_inside_is_none() {
        // A sliver along the box's top edge reaches no pixel center
        let outline = [point(0.0, 0.0), point(4.0, 0.0), point(4.0, 0.2)];
        assert!(SelectionMask::from_outline(&outline, 4, 4).is_none());
    }

    #[test]
    fn mask_edges_trace_its_outline() {
        let outline = [
            point(1.0, 1.0),
            point(3.0, 1.0),
            point(3.0, 3.0),
            point(1.0, 3.0),
        ];
        let mask = SelectionMask::from_outline(&outline, 4, 4).unwrap();
        let mut edges: Vec<_> = mask
            .edges()
            .iter()
            .map(|(a, b)| ((a.x, a.y), (b.x, b.y)))
            .collect();
        edges.sort();
        // One edge per side of the 2x2 square, not one per pixel
        assert_eq!(
            edges,
            [
                ((1, 1), (1, 3)),
                ((1, 1), (3, 1)),
                ((1, 3), (3, 3)),
                ((3, 1), (3, 3)),
            ]
        );
    }

    #[test]
    fn mask_flips_and_turns_with_the_pixels() {
        let mut mask = corner_triangle();
        mask.reorient(Orientation::FlipHorizontal);
        assert!(mask.contains(3, 0));
        assert!(!mask.contains(0, 3));
        mask.reorient(Orientation::RotateClockwise);
        // The top-right corner turns to the bottom-right
        assert!(mask.contains(3, 3));
        assert!(!mask.contains(0, 0));
    }

    #[test]
    fn free_form_lift_takes_only_the_pixels_inside() {
        let mut canvas = canvas_with_block();
        let mask = corner_triangle();
        let floating = canvas.lift(rect(2, 2, 4, 4), Some(&mask)).unwrap();
        // The inside leaves background behind, the rest of the block stays
        assert_eq!(rgb_at(&canvas, 2, 2), (255, 255, 255));
        assert_eq!(rgb_at(&canvas, 5, 5), (0, 0, 0));
        // Outside the outline the lifted pixels are transparent
        assert_eq!(floating.pixels.pixel(3, 3).unwrap().alpha(), 0);

        let undo = canvas.drop_floating(floating, point(10, 10), true).unwrap();
        assert_eq!(rgb_at(&canvas, 10, 10), (0, 0, 0));
        // The transparent part leaves the canvas under it as it was
        assert_eq!(rgb_at(&canvas, 13, 13), (255, 255, 255));
        canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 2, 2), (0, 0, 0));
        assert_eq!(rgb_at(&canvas, 10, 10), (255, 255, 255));
    }

    #[test]
    fn free_form_copy_and_erase_keep_to_the_outline() {
        let mut canvas = canvas_with_block();
        let mask = corner_triangle();
        let pixels = canvas.copy(rect(2, 2, 4, 4), Some(&mask)).unwrap();
        assert_eq!(pixels.pixel(0, 0).unwrap().alpha(), 255);
        assert_eq!(pixels.pixel(3, 3).unwrap().alpha(), 0);

        let undo = canvas.erase(rect(2, 2, 4, 4), Some(&mask)).unwrap();
        assert_eq!(rgb_at(&canvas, 2, 2), (255, 255, 255));
        assert_eq!(rgb_at(&canvas, 5, 5), (0, 0, 0));
        canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 2, 2), (0, 0, 0));
    }

    #[test]
    fn free_form_selection_past_the_edge_lines_up_with_its_mask() {
        let mut canvas = canvas(8, 8);
        canvas.pixmap.fill(Color::BLACK);
        let mask = corner_triangle();
        // The selection's box starts two pixels left of the canvas
        canvas.erase(rect(-2, 2, 4, 4), Some(&mask)).unwrap();
        // Selection pixel (2, 0), inside the triangle, is canvas pixel (0, 2);
        // selection pixel (2, 3), outside it, is canvas pixel (0, 5)
        assert_eq!(rgb_at(&canvas, 0, 2), (255, 255, 255));
        assert_eq!(rgb_at(&canvas, 0, 5), (0, 0, 0));
        assert_eq!(rgb_at(&canvas, 2, 2), (0, 0, 0));
    }

    #[test]
    fn draws_text_from_its_anchor_and_can_be_undone() {
        let mut canvas = canvas(200, 60);
        let before = canvas.pixmap.clone();
        let mut renderer = TextRenderer::new();
        let black = rgb(0x000000).into();
        let undo = canvas
            .draw_text(
                &mut renderer,
                "Hi",
                FontChoice::SansSerif,
                32.0,
                black,
                point(20, 10),
            )
            .unwrap();
        let drawn: Vec<(u32, u32)> = (0..60)
            .flat_map(|y| (0..200).map(move |x| (x, y)))
            .filter(|&(x, y)| rgb_at(&canvas, x, y) != (255, 255, 255))
            .collect();
        assert!(!drawn.is_empty());
        assert!(drawn.iter().all(|&(x, y)| x >= 20 && y >= 10));
        canvas.apply(undo);
        assert_eq!(canvas.pixmap.data(), before.data());
    }

    #[test]
    fn empty_text_draws_nothing() {
        let mut canvas = canvas(20, 20);
        let mut renderer = TextRenderer::new();
        let black = rgb(0x000000).into();
        assert!(
            canvas
                .draw_text(
                    &mut renderer,
                    "",
                    FontChoice::Serif,
                    16.0,
                    black,
                    point(0, 0)
                )
                .is_none()
        );
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
        pencil.draw_stroke(&brush_stroke(
            Brush::Pencil,
            &[(3.0, 5.0), (35.0, 31.0)],
            1.0,
        ));
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
        let colored = canvas
            .pixmap
            .pixels()
            .iter()
            .filter(|p| p.red() == 0)
            .count();
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
        assert!(
            crossing.0 > 100 && crossing.0 < 160,
            "{crossing:?} is not half gray"
        );
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
        let long = brush_stroke(
            Brush::Airbrush,
            &[(10.0, 20.0), (40.0, 20.0), (90.0, 20.0)],
            6.0,
        );
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

    /// Pixels the stroke left on the canvas, and how many of them are black.
    fn coverage(canvas: &Canvas) -> (usize, usize) {
        let pixels = canvas.pixmap.pixels();
        let marked = pixels.iter().filter(|p| p.red() < 255).count();
        let black = pixels.iter().filter(|p| p.red() == 0).count();
        (marked, black)
    }

    #[test]
    fn crayon_leaves_gaps_inside_the_line() {
        let mut canvas = canvas(60, 30);
        canvas.draw_stroke(&brush_stroke(
            Brush::Crayon,
            &[(5.0, 15.0), (55.0, 15.0)],
            10.0,
        ));
        // Down the middle, where the pen would cover every pixel
        let row: Vec<_> = (10..50).map(|x| rgb_at(&canvas, x, 15)).collect();
        let gaps = row.iter().filter(|&&c| c == (255, 255, 255)).count();
        assert!(gaps > 0, "no gaps");
        assert!(gaps < row.len() / 2, "{gaps} of {} are gaps", row.len());
        // The wax itself is solid
        assert!(row.contains(&(0, 0, 0)));
    }

    #[test]
    fn natural_pencil_is_lighter_than_the_pen_and_grainy() {
        let mut canvas = canvas(60, 30);
        canvas.draw_stroke(&brush_stroke(
            Brush::NaturalPencil,
            &[(5.0, 15.0), (55.0, 15.0)],
            6.0,
        ));
        let (marked, black) = coverage(&canvas);
        assert!(marked > 0);
        assert_eq!(black, 0);
        // Many shades down the middle, not one even gray
        let row: std::collections::BTreeSet<_> = (10..50).map(|x| rgb_at(&canvas, x, 15)).collect();
        assert!(row.len() > 5, "{row:?}");
        assert!(!row.contains(&(255, 255, 255)));
    }

    #[test]
    fn grain_stays_put_as_the_stroke_grows() {
        for brush in [Brush::Crayon, Brush::NaturalPencil] {
            let mut short = canvas(100, 30);
            short.draw_stroke(&brush_stroke(brush, &[(10.0, 15.0), (40.0, 15.0)], 8.0));
            let mut long = canvas(100, 30);
            long.draw_stroke(&brush_stroke(
                brush,
                &[(10.0, 15.0), (40.0, 15.0), (90.0, 15.0)],
                8.0,
            ));
            for y in 0..30 {
                for x in 0..36 {
                    assert_eq!(
                        rgb_at(&short, x, y),
                        rgb_at(&long, x, y),
                        "{brush:?} ({x}, {y})"
                    );
                }
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
                    (0..60).contains(&x)
                        && (0..60).contains(&y)
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
        let points: Vec<_> = (0..=40)
            .map(|i| (10.0 + i as f32, 10.0 + i as f32))
            .collect();
        canvas.draw_stroke(&brush_stroke(Brush::Calligraphy, &points, 12.0));
        // Down the middle of the stroke every pixel is fully covered
        for i in 5..35 {
            let (x, y) = (10 + i, 10 + i);
            assert_eq!(rgb_at(&canvas, x, y), (0, 0, 0), "seam at ({x}, {y})");
        }
    }

    fn transparent_canvas(width: u32, height: u32) -> Canvas {
        Canvas::new(width, height, gpui_kit::transparent_black()).unwrap()
    }

    #[test]
    fn transparent_canvas_exports_transparent_png() {
        let mut canvas = transparent_canvas(20, 20);
        canvas.draw_stroke(&stroke(&[(0.0, 10.0), (20.0, 10.0)], 0xff0000, 4.0));
        let png = Pixmap::decode_png(&canvas.pixels().encode_png().unwrap()).unwrap();
        assert_eq!(png.pixel(10, 2).unwrap().alpha(), 0);
        assert_eq!(png.pixel(10, 10).unwrap().alpha(), 255);
    }

    #[test]
    fn eraser_makes_pixels_transparent_and_can_be_undone() {
        let mut canvas = transparent_canvas(40, 20);
        canvas.draw_stroke(&stroke(&[(0.0, 10.0), (40.0, 10.0)], 0x000000, 10.0));
        let base = canvas.pixels().clone();

        let eraser = stroke(&[(20.0, 0.0), (20.0, 20.0)], 0x000000, 6.0);
        let undo = canvas.erase_stroke(&base, &eraser).unwrap();
        assert_eq!(canvas.pixmap.pixel(20, 10).unwrap().alpha(), 0);
        // Outside the eraser the line stays
        assert_eq!(canvas.pixmap.pixel(5, 10).unwrap().alpha(), 255);

        canvas.apply(undo);
        assert_eq!(canvas.pixmap.pixel(20, 10).unwrap().alpha(), 255);
    }

    #[test]
    fn erasing_again_as_the_stroke_grows_does_not_restore_erased_pixels() {
        let mut canvas = transparent_canvas(40, 20);
        canvas.draw_stroke(&stroke(&[(0.0, 10.0), (40.0, 10.0)], 0x000000, 10.0));
        let base = canvas.pixels().clone();
        let mut eraser = stroke(&[(5.0, 10.0), (15.0, 10.0)], 0x000000, 6.0);
        canvas.erase_stroke(&base, &eraser);
        eraser.points.push(point(px(35.0), px(10.0)));
        canvas.erase_stroke(&base, &eraser);
        assert_eq!(canvas.pixmap.pixel(10, 10).unwrap().alpha(), 0);
        assert_eq!(canvas.pixmap.pixel(30, 10).unwrap().alpha(), 0);
    }

    #[test]
    fn transparency_shows_as_a_checkerboard() {
        let canvas = transparent_canvas(4, 1);
        // Squares of 2 device pixels
        let view = canvas.render_view(point(0, 0), size(4, 1), 1.0, 2.0, Grid::default());
        let shade = |x: usize| view[x * 4];
        assert_eq!(shade(0), CHECKER_LIGHT);
        assert_eq!(shade(1), CHECKER_LIGHT);
        assert_eq!(shade(2), CHECKER_DARK);
        assert!(view.chunks(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn opening_a_png_replaces_the_canvas_and_can_be_undone() {
        let mut source = transparent_canvas(30, 10);
        source.draw_stroke(&stroke(&[(0.0, 5.0), (30.0, 5.0)], 0x00ff00, 4.0));
        let opened = Pixmap::decode_png(&source.pixels().encode_png().unwrap()).unwrap();

        let mut canvas = canvas(80, 60);
        let undo = canvas.replace(opened);
        assert_eq!(canvas.size(), size(px(30.0), px(10.0)));
        assert_eq!(rgb_at(&canvas, 15, 5), (0, 255, 0));
        // Transparency survives the round trip through PNG
        assert_eq!(canvas.pixmap.pixel(15, 0).unwrap().alpha(), 0);

        canvas.apply(undo);
        assert_eq!(canvas.size(), size(px(80.0), px(60.0)));
    }

    /// Which of the first `count` device columns of row 1 the grid darkened.
    fn grid_columns(canvas: &Canvas, scale: f32, grid: Grid, count: u32) -> Vec<u32> {
        let plain = canvas.render_view(point(0, 1), size(count, 1), scale, 1000.0, Grid::default());
        let view = canvas.render_view(point(0, 1), size(count, 1), scale, 1000.0, grid);
        (0..count)
            .filter(|&x| view[x as usize * 4] != plain[x as usize * 4])
            .collect()
    }

    #[test]
    fn pixel_grid_lines_each_pixel_once_it_is_large_enough() {
        let canvas = canvas(8, 8);
        let grid = Grid {
            pixels: true,
            tile: None,
        };
        // At 4 device pixels per canvas pixel, a line starts each pixel after the first
        assert_eq!(grid_columns(&canvas, 4.0, grid, 16), vec![4, 8, 12]);
        // At 2x the lines would bury the drawing, so none show
        assert!(grid_columns(&canvas, 2.0, grid, 16).is_empty());
    }

    #[test]
    fn tile_grid_shows_at_any_zoom() {
        let canvas = canvas(32, 32);
        let grid = Grid {
            pixels: false,
            tile: Some(8),
        };
        assert_eq!(grid_columns(&canvas, 1.0, grid, 32), vec![8, 16, 24]);
        assert_eq!(grid_columns(&canvas, 2.0, grid, 40), vec![16, 32]);
    }

    #[test]
    fn tile_lines_are_darker_than_pixel_lines() {
        let canvas = canvas(16, 16);
        let grid = Grid {
            pixels: true,
            tile: Some(2),
        };
        let view = canvas.render_view(point(0, 1), size(16, 1), 4.0, 1000.0, grid);
        // Device column 4 starts pixel 1 (a pixel line), column 8 starts pixel 2 (a tile line)
        assert!(view[8 * 4] < view[4 * 4]);
    }

    /// A 3x2 image whose pixels are numbered 1-6 in their red channel.
    fn numbered() -> Pixmap {
        let mut pixmap = Pixmap::new(3, 2).unwrap();
        for (i, pixel) in pixmap.pixels_mut().iter_mut().enumerate() {
            *pixel = PremultipliedColorU8::from_rgba(i as u8 + 1, 0, 0, 255).unwrap();
        }
        pixmap
    }

    fn numbers(pixmap: &Pixmap) -> (u32, u32, Vec<u8>) {
        let reds = pixmap.pixels().iter().map(|p| p.red()).collect();
        (pixmap.width(), pixmap.height(), reds)
    }

    #[test]
    fn reorienting_moves_pixels_as_expected() {
        // 1 2 3
        // 4 5 6
        let image = numbered();
        let result = |o| numbers(&reoriented(&image, o));
        assert_eq!(
            result(Orientation::FlipHorizontal),
            (3, 2, vec![3, 2, 1, 6, 5, 4])
        );
        assert_eq!(
            result(Orientation::FlipVertical),
            (3, 2, vec![4, 5, 6, 1, 2, 3])
        );
        // 4 1
        // 5 2
        // 6 3
        assert_eq!(
            result(Orientation::RotateClockwise),
            (2, 3, vec![4, 1, 5, 2, 6, 3])
        );
        // 3 6
        // 2 5
        // 1 4
        assert_eq!(
            result(Orientation::RotateCounterClockwise),
            (2, 3, vec![3, 6, 2, 5, 1, 4])
        );
    }

    #[test]
    fn four_quarter_turns_restore_the_image() {
        let mut image = numbered();
        for _ in 0..4 {
            image = reoriented(&image, Orientation::RotateClockwise);
        }
        assert_eq!(numbers(&image), numbers(&numbered()));
    }

    #[test]
    fn rotating_the_canvas_swaps_its_size_and_can_be_undone() {
        let mut canvas = canvas(80, 60);
        let undo = canvas.reorient(Orientation::RotateClockwise);
        assert_eq!(canvas.size(), size(px(60.0), px(80.0)));
        canvas.apply(undo);
        assert_eq!(canvas.size(), size(px(80.0), px(60.0)));
    }

    #[test]
    fn flipping_a_selection_in_place_is_recorded() {
        let mut canvas = canvas_with_block();
        // Lift the left half of the block plus empty space to its left
        let mut floating = canvas.lift(rect(0, 2, 4, 4), None).unwrap();
        floating.reorient(Orientation::FlipHorizontal);
        let undo = canvas.drop_floating(floating, point(0, 2), true).unwrap();
        // The block's left half (columns 2-3) now sits at columns 0-1
        assert_eq!(rgb_at(&canvas, 0, 3), (0, 0, 0));
        assert_eq!(rgb_at(&canvas, 2, 3), (255, 255, 255));
        canvas.apply(undo);
        assert_eq!(rgb_at(&canvas, 0, 3), (255, 255, 255));
        assert_eq!(rgb_at(&canvas, 2, 3), (0, 0, 0));
    }

    #[test]
    fn png_is_exactly_the_canvas_size() {
        let canvas = canvas(80, 60);
        let pixmap = Pixmap::decode_png(&canvas.pixels().encode_png().unwrap()).unwrap();
        assert_eq!((pixmap.width(), pixmap.height()), (80, 60));
    }

    #[test]
    fn render_view_scales_with_nearest_neighbor_and_pads_with_background() {
        let mut canvas = canvas(2, 1);
        canvas.pixmap.pixels_mut()[0] = PremultipliedColorU8::from_rgba(255, 0, 0, 255).unwrap();

        // At 2x each canvas pixel covers 2x2 device pixels; column 4+ is past the canvas
        let view = canvas.render_view(point(0, 0), size(5, 2), 2.0, 16.0, Grid::default());
        let pixel = |x: usize, y: usize| &view[(y * 5 + x) * 4..][..4];
        let red = [0, 0, 255, 255];
        let white = [255, 255, 255, 255];
        assert_eq!(pixel(0, 0), red);
        assert_eq!(pixel(1, 1), red);
        assert_eq!(pixel(2, 0), white);
        assert_eq!(pixel(4, 1), white);
    }
}
