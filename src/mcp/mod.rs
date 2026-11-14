use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use gpui_kit::{Hsla, Pixels, Point, point, px, rgba};
use images::Images;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::JsonSchema;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::canvas::{
    Brush, Canvas, ClipMask, DEFAULT_FILL_TOLERANCE, Fill, Orientation, PixelRect,
};
use crate::filter::{Adjustment, Filter};
use crate::pattern::{PATTERNS, Pattern};
use crate::stroke::{ArcKind, Gradient, Stroke, roughen, smooth};
use crate::text::{FontChoice, TextRenderer};
use crate::tool::ShapeKind;
use crate::{
    DEFAULT_FONT_SIZE, DEFAULT_WIDTH, MAX_CANVAS_SIDE, MAX_FONT_SIZE, MAX_WIDTH, MIN_FONT_SIZE,
    MIN_WIDTH, tolerance_from_percent, tolerance_percent,
};
use tiny_skia::BlendMode;

mod images;

const INSTRUCTIONS: &str = "Draws raster images with petit-paint, a small paint app. \
Create an image with new_image or open a file with open_image; each gets an id that the \
other tools take. Draw on it with draw_stroke and draw_shape, or many strokes and shapes \
at once with draw; fill regions with fill and write with draw_text. Shapes can be filled \
with gradients, polygons and strokes drawn as smooth curves, and anything blended (add for \
light, multiply for shadow) or clipped. A clip is a rectangle or a mask: a shape, or the \
region around a pixel of about the same color, which keeps shading inside something already \
drawn. Masks combine with all and any, and one with outside is cut out of the rest, as for \
a crescent moon; draw can name masks once for many items. To look drawn by hand rather \
than by a computer: taper or press strokes so lines thin and swell, roughen outlines, \
jitter colors, blend edges with the smudge brush, shade with hatch's strokes instead of \
gradients, and finish with filter's grain, blur or painterly patches. Coordinates are in \
image pixels from the top-left. Colors are #rrggbb or #rrggbbaa. Call view_image to look at the result, and save_image to write \
it to a file. Relative paths are taken from the server's working directory.";

/// Draws images for MCP clients such as coding agents, without a window.
#[derive(Clone, Default)]
pub struct PaintServer {
    images: Arc<Mutex<Images>>,
    /// Made on first use, as creating one scans the installed fonts.
    text_renderer: Arc<Mutex<Option<TextRenderer>>>,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct NewImageParams {
    /// Width in pixels, from 1 to 4096.
    width: u32,
    /// Height in pixels, from 1 to 4096.
    height: u32,
    /// Background color as #rrggbb or #rrggbbaa. Transparent when left out.
    background: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct OpenImageParams {
    /// A PNG, JPEG, BMP, WebP or GIF file. A GIF opens at its first frame.
    path: String,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct SaveImageParams {
    /// The image's id.
    image: u32,
    /// Where to save, ending in .png, .jpg, .jpeg, .bmp, .webp or .gif, which picks the
    /// format. An existing file is overwritten. Left out, the image is saved to the file
    /// it was opened from or last saved to.
    path: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ViewImageParams {
    /// The image's id.
    image: u32,
    /// Two opposite corners of a part to look at closely, as [x, y] pairs. The part is
    /// scaled up as far as fits 1024×1024, up to 16 times, with each pixel a sharp
    /// square. The whole image when left out.
    region: Option<[[i32; 2]; 2]>,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ImageParams {
    /// The image's id.
    image: u32,
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum BrushName {
    /// A smooth, anti-aliased round line.
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
    /// A flat bristle brush loaded with oil paint: streaky, running dry along a long
    /// stroke, and mixing with the wet paint under it.
    Oil,
    /// A wash of watercolor: translucent, bleeding unevenly past the brush, drying to a
    /// darker rim and grainy where the pigment settles into the paper. Washes laid over
    /// each other deepen like glazes.
    Watercolor,
    /// Drags the colors already on the image along the line, like a finger through wet
    /// paint, to blend edges and soften shading. It takes no color.
    Smudge,
}

impl From<BrushName> for Brush {
    fn from(name: BrushName) -> Self {
        match name {
            BrushName::Pen => Brush::Pen,
            BrushName::Pencil => Brush::Pencil,
            BrushName::NaturalPencil => Brush::NaturalPencil,
            BrushName::Crayon => Brush::Crayon,
            BrushName::Marker => Brush::Marker,
            BrushName::Airbrush => Brush::Airbrush,
            BrushName::Calligraphy => Brush::Calligraphy,
            BrushName::Oil => Brush::Oil,
            BrushName::Watercolor => Brush::Watercolor,
            BrushName::Smudge => Brush::Smudge,
        }
    }
}

#[derive(Clone, Copy, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum ShapeName {
    Line,
    Rectangle,
    Ellipse,
    Polygon,
    /// Part of an ellipse's curve.
    Arc,
    /// A slice of an ellipse, closed through its center.
    Pie,
    /// Part of an ellipse, closed by a straight line between the curve's ends.
    Chord,
}

impl ShapeName {
    fn label(self) -> &'static str {
        match self {
            ShapeName::Line => "a line",
            ShapeName::Rectangle => "a rectangle",
            ShapeName::Ellipse => "an ellipse",
            ShapeName::Polygon => "a polygon",
            ShapeName::Arc => "an arc",
            ShapeName::Pie => "a pie",
            ShapeName::Chord => "a chord",
        }
    }
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum BlendName {
    /// Paint over what is there.
    Normal,
    /// Add to the colors under it, so light glows on dark colors.
    Add,
    /// Lighten the colors under it, more gently than add.
    Screen,
    /// Darken the colors under it, as a shadow does.
    Multiply,
}

impl From<BlendName> for BlendMode {
    fn from(name: BlendName) -> Self {
        match name {
            BlendName::Normal => BlendMode::SourceOver,
            BlendName::Add => BlendMode::Plus,
            BlendName::Screen => BlendMode::Screen,
            BlendName::Multiply => BlendMode::Multiply,
        }
    }
}

/// How a stroke or shape lands on the image, which both take.
#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct Placement {
    /// How it combines with the colors under it. Normal when left out.
    blend: Option<BlendName>,
    /// Where to draw, leaving the rest of the image as it is: two opposite corners of a
    /// rectangle as [x, y] pairs, or a mask, which can be a shape, the region around a
    /// pixel, or one of draw's named masks.
    clip: Option<ClipSpec>,
    /// Move the line, or the shape's outline, up to about this many pixels to either
    /// side in gentle waves, as if drawn by hand, from 0 to 16.
    roughen: Option<f32>,
    /// Let the color wander a little from place to place, as paint mixed unevenly
    /// does: how far, in percent from 0 to 100. 5 to 15 looks natural.
    jitter: Option<f32>,
}

impl Placement {
    /// Places `strokes`, which are one stroke or shape, the same `seed`
    /// roughening each alike.
    fn apply(&self, strokes: &mut [Stroke], seed: u64, masks: &Masks) -> Result<()> {
        if let Some(amount) = self.roughen
            && !(0.0..=16.0).contains(&amount)
        {
            bail!("roughen must be between 0 and 16, not {amount}");
        }
        let jitter = self.jitter.unwrap_or(0.0);
        if !(0.0..=100.0).contains(&jitter) {
            bail!("jitter must be between 0 and 100, not {jitter}");
        }
        let (clip, mask) = match &self.clip {
            None => (None, None),
            Some(ClipSpec::Rectangle(rect)) => (Some(to_rect(*rect)?), None),
            Some(ClipSpec::Mask(mask)) => (None, Some(mask.build(masks)?)),
        };
        for stroke in strokes {
            if let Some(amount) = self.roughen {
                let rough = roughen(&stroke.points, stroke.closed, amount, seed);
                // Pressure belongs to the points as they were
                let pressure = Stroke {
                    points: stroke.points.clone(),
                    pressure: std::mem::take(&mut stroke.pressure),
                    ..Default::default()
                };
                stroke.pressure = resample_pressure(&pressure, &rough);
                stroke.points = rough;
            }
            stroke.seed = seed;
            stroke.jitter = jitter / 100.0;
            stroke.blend = self.blend.map_or(BlendMode::SourceOver, BlendMode::from);
            stroke.clip = clip;
            stroke.mask = mask.clone();
        }
        Ok(())
    }
}

/// Where a stroke or shape may draw.
#[derive(JsonSchema)]
#[serde(untagged)]
#[schemars(crate = "rmcp::schemars")]
enum ClipSpec {
    /// Two opposite corners of a rectangle, as [x, y] pairs.
    Rectangle([[i32; 2]; 2]),
    Mask(MaskSpec),
}

// By hand, so a wrong mask is explained rather than reported as matching
// neither a rectangle nor a mask
impl<'de> Deserialize<'de> for ClipSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use rmcp::serde_json::Value;
        match Value::deserialize(deserializer)? {
            rect @ Value::Array(_) => <[[i32; 2]; 2]>::deserialize(rect)
                .map(ClipSpec::Rectangle)
                .map_err(serde::de::Error::custom),
            mask @ Value::Object(_) => MaskSpec::deserialize(mask)
                .map(ClipSpec::Mask)
                .map_err(serde::de::Error::custom),
            other => Err(serde::de::Error::custom(format!(
                "clip is a rectangle's two corners or a mask such as {{\"shape\": ...}}, \
                 not {other}"
            ))),
        }
    }
}

/// An area with soft edges to draw inside of, or outside of. It is one of: a shape,
/// as {"shape": "ellipse", "points": [[x, y], [x, y]]} with the options draw_shape
/// takes; the region around a pixel that has about the same color, as fill finds it,
/// as {"region": [x, y]}; one of draw's named masks, as {"mask": "name"}; or a
/// combination, as {"all": [...]} or {"any": [...]} of rectangles and masks.
#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct MaskSpec {
    /// A rectangle, ellipse, polygon, pie or chord to draw inside of.
    shape: Option<ShapeName>,
    /// The shape's points, as draw_shape takes them.
    points: Option<Vec<[f32; 2]>>,
    /// For a polygon: a smooth curve through its corners.
    smooth: Option<bool>,
    /// For a rectangle: how far its corners are rounded, in pixels.
    radius: Option<f32>,
    /// For a pie or chord: where it starts, in degrees clockwise from pointing right.
    start_angle: Option<f32>,
    /// For a pie or chord: where it ends, in degrees clockwise from start_angle.
    end_angle: Option<f32>,
    /// A pixel, as [x, y], whose region of about the same color to draw inside of. The
    /// region is found in the image as it is before drawing.
    region: Option<[f32; 2]>,
    /// For a region: how different a pixel's color may be and still be in it, in
    /// percent from 0 to 100. 25 when left out.
    tolerance: Option<f32>,
    /// The name of one of draw's masks.
    mask: Option<String>,
    /// Rectangles and masks to draw where every one of them covers, such as the left
    /// half of a face. One with "outside": true is cut out, as for a crescent moon.
    all: Option<Vec<ClipSpec>>,
    /// Rectangles and masks to draw where any one of them covers, such as clouds made of
    /// several ellipses.
    any: Option<Vec<ClipSpec>>,
    /// Draw outside the area instead of inside it.
    outside: Option<bool>,
}

/// What masks are made from: the image as it is before drawing, and draw's
/// named masks.
struct Masks<'a> {
    canvas: &'a Canvas,
    named: &'a BTreeMap<String, Arc<ClipMask>>,
    /// Whether the masks being made are draw's named masks, which cannot
    /// use each other.
    naming: bool,
}

impl ClipSpec {
    fn build(&self, masks: &Masks) -> Result<ClipMask> {
        match self {
            ClipSpec::Rectangle(rect) => {
                let PixelRect {
                    x,
                    y,
                    width,
                    height,
                } = to_rect(*rect)?;
                let corner = |x: i32, y: i32| point(px(x as f32), px(y as f32));
                let outline = Stroke::shape(
                    ShapeKind::Rectangle,
                    corner(x, y),
                    corner(x + width as i32, y + height as i32),
                    Stroke::default(),
                );
                let size = masks.canvas.pixels();
                masks.canvas.shape_mask(&outline).ok_or_else(|| {
                    anyhow::anyhow!(
                        "the rectangle lies entirely outside the {}×{} image",
                        size.width(),
                        size.height()
                    )
                })
            }
            ClipSpec::Mask(spec) => Ok((*spec.build(masks)?).clone()),
        }
    }
}

impl MaskSpec {
    fn build(&self, masks: &Masks) -> Result<Arc<ClipMask>> {
        let kinds = [
            self.shape.is_some(),
            self.region.is_some(),
            self.mask.is_some(),
            self.all.is_some(),
            self.any.is_some(),
        ];
        if kinds.iter().filter(|&&given| given).count() != 1 {
            bail!("a mask takes one of shape, region, mask, all or any");
        }
        let size = masks.canvas.pixels();
        let outside = || {
            anyhow::anyhow!(
                "the mask lies entirely outside the {}×{} image",
                size.width(),
                size.height()
            )
        };
        let has_shape_options = self.points.is_some()
            || self.smooth.is_some()
            || self.radius.is_some()
            || self.start_angle.is_some()
            || self.end_angle.is_some();
        let mask = if let Some(name) = self.shape {
            if self.tolerance.is_some() {
                bail!("tolerance is for region masks");
            }
            if matches!(name, ShapeName::Line | ShapeName::Arc) {
                bail!("{} has no inside to draw in", name.label());
            }
            let points = self
                .points
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("a shape mask takes points"))?;
            let geometry = Geometry {
                name,
                points,
                smooth: self.smooth,
                radius: self.radius,
                start_angle: self.start_angle,
                end_angle: self.end_angle,
            };
            geometry.check()?;
            let outline = geometry.outline(Stroke::default());
            Arc::new(masks.canvas.shape_mask(&outline).ok_or_else(outside)?)
        } else if let Some([x, y]) = self.region {
            if has_shape_options {
                bail!("points, smooth, radius and angles are for shape masks");
            }
            let size = (size.width() as f32, size.height() as f32);
            if x < 0.0 || y < 0.0 || x >= size.0 || y >= size.1 {
                bail!("({x}, {y}) is outside the {}×{} image", size.0, size.1);
            }
            let tolerance = parse_tolerance(self.tolerance)?;
            let mask = masks.canvas.region_mask(point(px(x), px(y)), tolerance);
            Arc::new(mask.ok_or_else(outside)?)
        } else if let Some(name) = &self.mask {
            if has_shape_options || self.tolerance.is_some() {
                bail!("a named mask takes nothing but its name and outside");
            }
            if masks.naming {
                bail!("draw's named masks cannot use other named masks");
            }
            let mask = masks.named.get(name).ok_or_else(|| {
                anyhow::anyhow!("there is no mask named {name:?}; name it in draw's masks")
            })?;
            mask.clone()
        } else {
            let (word, parts) = match (&self.all, &self.any) {
                (Some(parts), _) => ("all", parts),
                (_, Some(parts)) => ("any", parts),
                _ => unreachable!("one of the kinds is given"),
            };
            if has_shape_options || self.tolerance.is_some() {
                bail!("a combined mask takes nothing but {word} and outside");
            }
            if parts.is_empty() {
                bail!("{word} takes one or more rectangles or masks");
            }
            let parts = parts
                .iter()
                .enumerate()
                .map(|(index, part)| {
                    part.build(masks)
                        .with_context(|| format!("mask {} in {word}", index + 1))
                })
                .collect::<Result<Vec<_>>>()?;
            let combined = if word == "all" {
                ClipMask::all(parts)
            } else {
                ClipMask::any(parts)
            };
            Arc::new(
                combined
                    .ok_or_else(|| anyhow::anyhow!("the masks in all have no area in common"))?,
            )
        };
        Ok(if self.outside == Some(true) {
            Arc::new((*mask).clone().inverted())
        } else {
            mask
        })
    }
}

/// A line through points drawn with a brush.
#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct StrokeSpec {
    /// The points the line passes through, in order, as [x, y] pairs. A single point
    /// leaves a dot.
    points: Vec<[f32; 2]>,
    /// The line's color as #rrggbb or #rrggbbaa. The smudge brush takes none.
    color: Option<String>,
    /// Width in pixels, from 1 to 32. 4 when left out.
    width: Option<f32>,
    /// The brush to draw with. The pen when left out.
    brush: Option<BrushName>,
    /// Draw a smooth curve through the points instead of straight lines between them.
    smooth: Option<bool>,
    /// Thin the line toward its ends, as a brush lifted off the paper does: how many
    /// pixels each end takes to reach the full width, as one number for both ends or
    /// [start, end].
    taper: Option<TaperSpec>,
    /// How hard the brush presses at each point, from 0 to 1, scaling the width there,
    /// so the line swells and thins along its length: one number for each point.
    pressure: Option<Vec<f32>>,
    #[serde(flatten)]
    placement: Placement,
}

/// How far a stroke's ends take to reach its full width.
#[derive(JsonSchema)]
#[serde(untagged)]
#[schemars(crate = "rmcp::schemars")]
enum TaperSpec {
    /// The same at both ends.
    Both(f32),
    /// At the start and at the end.
    Ends([f32; 2]),
}

// By hand, so a wrong taper is explained rather than reported as matching
// neither form
impl<'de> Deserialize<'de> for TaperSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = rmcp::serde_json::Value::deserialize(deserializer)?;
        if let Some(both) = value.as_f64() {
            return Ok(TaperSpec::Both(both as f32));
        }
        <[f32; 2]>::deserialize(value)
            .map(TaperSpec::Ends)
            .map_err(|_| serde::de::Error::custom("taper is a number of pixels or [start, end]"))
    }
}

impl TaperSpec {
    fn ends(&self) -> Result<[f32; 2]> {
        let ends = match *self {
            TaperSpec::Both(both) => [both, both],
            TaperSpec::Ends(ends) => ends,
        };
        if ends.iter().any(|&end| end < 0.0) {
            bail!("taper cannot be negative");
        }
        Ok(ends)
    }
}

/// How hard the brush presses at each of `points`, from `given` at each of
/// the `original` points the line was drawn through, and thinned toward the
/// ends by `taper`. Empty when the brush presses fully everywhere.
fn pressure_along(
    original: &[Point<Pixels>],
    points: &[Point<Pixels>],
    given: Option<&[f32]>,
    taper: Option<[f32; 2]>,
) -> Vec<f32> {
    if given.is_none() && taper.is_none() {
        return Vec::new();
    }
    let distances = |points: &[Point<Pixels>]| {
        let mut travelled = 0.0;
        let mut distances = vec![0.0];
        for pair in points.windows(2) {
            let (dx, dy) = (
                (pair[1].x - pair[0].x).as_f32(),
                (pair[1].y - pair[0].y).as_f32(),
            );
            travelled += (dx * dx + dy * dy).sqrt();
            distances.push(travelled);
        }
        distances
    };
    let (from, to) = (distances(original), distances(points));
    let (from_length, length) = (*from.last().unwrap_or(&0.0), *to.last().unwrap_or(&0.0));
    to.iter()
        .map(|&distance| {
            // The same share of the way along the line as drawn
            let along = if length > 0.0 {
                distance / length * from_length
            } else {
                0.0
            };
            let pressed = given.map_or(1.0, |given| {
                let next = from.partition_point(|&d| d < along).min(given.len() - 1);
                if next == 0 {
                    return given[0];
                }
                let (d0, d1) = (from[next - 1], from[next]);
                let t = if d1 > d0 {
                    (along - d0) / (d1 - d0)
                } else {
                    1.0
                };
                given[next - 1] + (given[next] - given[next - 1]) * t.clamp(0.0, 1.0)
            });
            let thinned = taper.map_or(1.0, |[start, end]| {
                let ramp = |d: f32, over: f32| if over > 0.0 { (d / over).min(1.0) } else { 1.0 };
                ramp(distance, start).min(ramp(length - distance, end))
            });
            pressed * thinned
        })
        .collect()
}

/// `stroke`'s pressure at each of `points`, a reshaped copy of its line.
fn resample_pressure(stroke: &Stroke, points: &[Point<Pixels>]) -> Vec<f32> {
    if stroke.pressure.is_empty() {
        return Vec::new();
    }
    pressure_along(&stroke.points, points, Some(&stroke.pressure), None)
}

impl StrokeSpec {
    fn build(&self, seed: u64, masks: &Masks) -> Result<Stroke> {
        if self.points.is_empty() {
            bail!("give at least one point");
        }
        let original = to_points(&self.points);
        let points = if self.smooth == Some(true) {
            smooth(&original, false)
        } else {
            original.clone()
        };
        if let Some(pressure) = &self.pressure {
            if pressure.len() != self.points.len() {
                bail!(
                    "pressure takes one number for each of the {} points, not {}",
                    self.points.len(),
                    pressure.len()
                );
            }
            if pressure.iter().any(|p| !(0.0..=1.0).contains(p)) {
                bail!("pressure must be between 0 and 1");
            }
        }
        let brush = self.brush.map_or(Brush::Pen, Brush::from);
        let color = match (&self.color, brush) {
            (Some(_), Brush::Smudge) => {
                bail!("smudge drags the colors already on the image, so it takes no color")
            }
            (Some(color), _) => parse_color(color)?,
            (None, Brush::Smudge) => gpui_kit::black(),
            (None, _) => bail!("give the line a color"),
        };
        let taper = self.taper.as_ref().map(TaperSpec::ends).transpose()?;
        let pressure = pressure_along(&original, &points, self.pressure.as_deref(), taper);
        let mut stroke = Stroke {
            points,
            pressure,
            color,
            width: parse_width(self.width)?,
            brush,
            seed,
            ..Default::default()
        };
        self.placement
            .apply(std::slice::from_mut(&mut stroke), seed, masks)?;
        Ok(stroke)
    }
}

/// What fills a shape: a color, or a gradient.
#[derive(JsonSchema)]
#[serde(untagged)]
#[schemars(crate = "rmcp::schemars")]
enum FillSpec {
    /// A color as #rrggbb or #rrggbbaa.
    Color(String),
    Gradient(GradientSpec),
}

// By hand, so a wrong gradient is explained rather than reported as matching
// neither a color nor a gradient
impl<'de> Deserialize<'de> for FillSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match rmcp::serde_json::Value::deserialize(deserializer)? {
            rmcp::serde_json::Value::String(color) => Ok(FillSpec::Color(color)),
            other => GradientSpec::deserialize(other)
                .map(FillSpec::Gradient)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// Colors that change across a fill. Each stop is [position, color], with
/// positions from 0 to 1 in order; beyond the ends, the end colors carry on.
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum GradientSpec {
    /// Along the line from `from` to `to`, as {"linear": {"from": [x, y], "to": [x, y],
    /// "stops": [[0, "#rrggbb"], [1, "#rrggbb"]]}}.
    Linear {
        from: [f32; 2],
        to: [f32; 2],
        stops: Vec<(f32, String)>,
    },
    /// Out from `center` to `radius` away, as {"radial": {"center": [x, y], "radius": r,
    /// "stops": [[0, "#rrggbb"], [1, "#rrggbb00"]]}}. The fill ends where the shape
    /// does, so for a glow that fades out, make the shape reach the radius.
    Radial {
        center: [f32; 2],
        radius: f32,
        stops: Vec<(f32, String)>,
    },
}

impl FillSpec {
    /// The fill's color, and the gradient that takes its place, if any.
    fn parse(&self) -> Result<(Hsla, Option<Gradient>)> {
        let gradient = match self {
            FillSpec::Color(color) => return Ok((parse_color(color)?, None)),
            FillSpec::Gradient(gradient) => gradient,
        };
        let stops = match gradient {
            GradientSpec::Linear { stops, .. } | GradientSpec::Radial { stops, .. } => stops,
        };
        if stops.len() < 2 {
            bail!("a gradient takes two or more stops");
        }
        let mut last = 0.0;
        let stops = stops
            .iter()
            .map(|(position, color)| {
                if !(last..=1.0).contains(position) {
                    bail!("stop positions run from 0 to 1 in order, so {position} cannot follow {last}");
                }
                last = *position;
                Ok((*position, parse_color(color)?))
            })
            .collect::<Result<_>>()?;
        let at = |[x, y]: [f32; 2]| point(px(x), px(y));
        let gradient = match *gradient {
            GradientSpec::Linear { from, to, .. } => {
                if from == to {
                    bail!("a linear gradient's from and to must differ");
                }
                Gradient::Linear {
                    from: at(from),
                    to: at(to),
                    stops,
                }
            }
            GradientSpec::Radial { center, radius, .. } => {
                if radius <= 0.0 {
                    bail!("a radial gradient's radius must be more than 0");
                }
                Gradient::Radial {
                    center: at(center),
                    radius,
                    stops,
                }
            }
        };
        // Opaque, as the gradient's colors carry their own transparency
        Ok((gpui_kit::black(), Some(gradient)))
    }
}

/// A line, rectangle, ellipse or polygon with an outline, a fill or both.
#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ShapeSpec {
    shape: ShapeName,
    /// As [x, y] pairs: a line's two ends, two opposite corners of a rectangle or of
    /// the box around an ellipse (for an arc, pie or chord, of its whole ellipse), or a
    /// polygon's three or more corners in order.
    points: Vec<[f32; 2]>,
    /// The outline's color as #rrggbb or #rrggbbaa. No outline when left out.
    outline: Option<String>,
    /// What to fill the inside with: a color as #rrggbb or #rrggbbaa, or a linear or
    /// radial gradient. Not filled when left out. A line or arc has no inside.
    fill: Option<FillSpec>,
    /// The outline's width in pixels, from 1 to 32. 4 when left out.
    width: Option<f32>,
    /// For a polygon: a smooth curve through its corners instead of straight sides.
    smooth: Option<bool>,
    /// For a rectangle: how far its corners are rounded, in pixels.
    radius: Option<f32>,
    /// For an arc, pie or chord: where it starts, in degrees. 0 points right, and
    /// angles run clockwise, so 90 points down.
    start_angle: Option<f32>,
    /// For an arc, pie or chord: where it ends, in degrees, going clockwise from
    /// start_angle.
    end_angle: Option<f32>,
    #[serde(flatten)]
    placement: Placement,
}

/// The form of a shape, which shapes and masks share.
struct Geometry<'a> {
    name: ShapeName,
    points: &'a [[f32; 2]],
    smooth: Option<bool>,
    radius: Option<f32>,
    start_angle: Option<f32>,
    end_angle: Option<f32>,
}

impl Geometry<'_> {
    /// Fails unless the points and options fit the shape.
    fn check(&self) -> Result<()> {
        let name = self.name;
        let count = self.points.len();
        let (takes, fits) = match name {
            ShapeName::Line => ("a line takes its two ends", count == 2),
            ShapeName::Rectangle => ("a rectangle takes two opposite corners", count == 2),
            ShapeName::Ellipse => (
                "an ellipse takes two opposite corners of its box",
                count == 2,
            ),
            ShapeName::Arc | ShapeName::Pie | ShapeName::Chord => (
                "an arc, pie or chord takes two opposite corners of its ellipse's box",
                count == 2,
            ),
            ShapeName::Polygon => ("a polygon takes three or more corners", count >= 3),
        };
        if !fits {
            bail!(
                "{takes}, not {count} {}",
                if count == 1 { "point" } else { "points" }
            );
        }
        let arc = matches!(name, ShapeName::Arc | ShapeName::Pie | ShapeName::Chord);
        if self.smooth.is_some() && name != ShapeName::Polygon {
            bail!("smooth is for polygons, not {}", name.label());
        }
        if self.radius.is_some() && name != ShapeName::Rectangle {
            bail!("radius is for rectangles, not {}", name.label());
        }
        match (self.start_angle, self.end_angle) {
            (Some(_), Some(_)) if arc => {}
            (None, None) if !arc => {}
            _ if arc => bail!("{} takes both start_angle and end_angle", name.label()),
            _ => bail!("start_angle and end_angle are for arcs, pies and chords"),
        }
        if self.radius.is_some_and(|radius| radius < 0.0) {
            bail!("radius cannot be negative");
        }
        Ok(())
    }

    /// The shape's outline, drawn with `base`'s color, width and fill. The
    /// geometry must have passed [`Self::check`].
    fn outline(&self, base: Stroke) -> Stroke {
        let points = to_points(self.points);
        let angles = self.start_angle.zip(self.end_angle).unwrap_or((0.0, 360.0));
        match self.name {
            ShapeName::Polygon if self.smooth == Some(true) => Stroke {
                // Curves join smoothly
                sharp_corners: false,
                ..Stroke::polygon(smooth(&points, true), true, base)
            },
            ShapeName::Polygon => Stroke::polygon(points, true, base),
            ShapeName::Rectangle => match self.radius {
                Some(radius) => Stroke::rounded_rectangle(points[0], points[1], radius, base),
                None => Stroke::shape(ShapeKind::Rectangle, points[0], points[1], base),
            },
            ShapeName::Line => Stroke::shape(ShapeKind::Line, points[0], points[1], base),
            ShapeName::Ellipse => Stroke::shape(ShapeKind::Ellipse, points[0], points[1], base),
            ShapeName::Arc | ShapeName::Pie | ShapeName::Chord => {
                let kind = match self.name {
                    ShapeName::Arc => ArcKind::Arc,
                    ShapeName::Pie => ArcKind::Pie,
                    _ => ArcKind::Chord,
                };
                Stroke::arc(points[0], points[1], angles.0, angles.1, kind, base)
            }
        }
    }
}

impl ShapeSpec {
    fn geometry(&self) -> Geometry<'_> {
        Geometry {
            name: self.shape,
            points: &self.points,
            smooth: self.smooth,
            radius: self.radius,
            start_angle: self.start_angle,
            end_angle: self.end_angle,
        }
    }

    /// The fill, if any, then the outline, if any.
    fn build(&self, seed: u64, masks: &Masks) -> Result<Vec<Stroke>> {
        let name = self.shape;
        let geometry = self.geometry();
        geometry.check()?;
        if self.outline.is_none() && self.fill.is_none() {
            bail!("give an outline color, a fill color or both");
        }
        if matches!(name, ShapeName::Line | ShapeName::Arc) && self.fill.is_some() {
            bail!(
                "{} has no inside to fill; give it an outline color",
                name.label()
            );
        }
        let outline = self.outline.as_deref().map(parse_color).transpose()?;
        let fill = self.fill.as_ref().map(FillSpec::parse).transpose()?;
        let width = parse_width(self.width)?;

        let shape = |base: Stroke| geometry.outline(base);
        let mut strokes = Vec::new();
        if let Some((color, gradient)) = fill {
            strokes.push(shape(Stroke {
                color,
                gradient,
                width: px(0.0),
                filled: true,
                ..Default::default()
            }));
        }
        if let Some(color) = outline {
            strokes.push(shape(Stroke {
                color,
                width,
                ..Default::default()
            }));
        }
        self.placement.apply(&mut strokes, seed, masks)?;
        Ok(strokes)
    }
}

/// Strokes laid across an area at random, for shading and texture.
#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct HatchSpec {
    /// Colors as #rrggbb or #rrggbbaa; each stroke takes one of them at random.
    colors: Vec<String>,
    /// The direction the strokes run, in degrees. 0 points right, and angles run
    /// clockwise, so 90 points down. 45 when left out.
    angle: Option<f32>,
    /// How far each stroke's direction may differ from angle, either way, in degrees.
    /// 10 when left out.
    spread: Option<f32>,
    /// The shortest and longest strokes, in pixels, as [shortest, longest]. [20, 40]
    /// when left out.
    length: Option<[f32; 2]>,
    /// The thinnest and thickest strokes, from 1 to 32 pixels, as [thinnest,
    /// thickest]. [3, 6] when left out.
    width: Option<[f32; 2]>,
    /// How many strokes, from 1 to 5000. Enough to cover the area about once when left
    /// out.
    count: Option<u32>,
    /// The brush to draw with. The pen when left out.
    brush: Option<BrushName>,
    /// Where the light comes from, as [x, y]: strokes gather on the side of the area
    /// away from it, shading it. Spread evenly when left out.
    light: Option<[f32; 2]>,
    /// Thin each stroke toward its ends, as draw_stroke's taper does.
    taper: Option<TaperSpec>,
    /// Fixes where the strokes go, so the same seed draws the same strokes. Different
    /// each time when left out.
    seed: Option<u64>,
    /// The area to lay strokes in, which they are kept inside: two opposite corners of a
    /// rectangle as [x, y] pairs, or a mask. Needed.
    #[serde(flatten)]
    placement: Placement,
}

impl HatchSpec {
    fn build(&self, seed: u64, masks: &Masks) -> Result<Vec<Stroke>> {
        if self.colors.is_empty() {
            bail!("give one or more colors");
        }
        let colors = self
            .colors
            .iter()
            .map(|color| parse_color(color))
            .collect::<Result<Vec<_>>>()?;
        let range = |name: &str, given: Option<[f32; 2]>, default: [f32; 2], max: f32| {
            let [low, high] = given.unwrap_or(default);
            let min = if name == "width" { MIN_WIDTH } else { 1.0 };
            if low > high || low < min || high > max {
                bail!(
                    "{name} must be [shortest, longest] from {min} to {max}, not [{low}, {high}]"
                );
            }
            Ok([low, high])
        };
        let lengths = range("length", self.length, [20.0, 40.0], 500.0)?;
        let widths = range("width", self.width, [3.0, 6.0], MAX_WIDTH)?;
        let spread = self.spread.unwrap_or(10.0);
        if !(0.0..=180.0).contains(&spread) {
            bail!("spread must be between 0 and 180, not {spread}");
        }
        if self.count.is_some_and(|count| !(1..=5000).contains(&count)) {
            bail!("count must be between 1 and 5000");
        }
        let taper = self.taper.as_ref().map(TaperSpec::ends).transpose()?;
        let brush = self.brush.map_or(Brush::Pen, Brush::from);
        if brush == Brush::Smudge {
            bail!("hatch lays down color, so it cannot use the smudge brush");
        }

        // Where strokes may go: inside the clip, on the image
        let size = masks.canvas.pixels();
        let (width, height) = (size.width() as f32, size.height() as f32);
        let (inside, (left, top, right, bottom)): (Box<dyn Fn(f32, f32) -> bool>, _) =
            match &self.placement.clip {
                None => bail!("hatch takes a clip: the rectangle or mask to lay strokes in"),
                Some(ClipSpec::Rectangle(rect)) => {
                    let rect = to_rect(*rect)?;
                    let area = (
                        (rect.x as f32).max(0.0),
                        (rect.y as f32).max(0.0),
                        ((rect.x + rect.width as i32) as f32).min(width),
                        ((rect.y + rect.height as i32) as f32).min(height),
                    );
                    (Box::new(|_, _| true), area)
                }
                Some(ClipSpec::Mask(spec)) => {
                    let mask = spec.build(masks)?;
                    let area = mask.limit((0.0, 0.0, width, height));
                    let covers = move |x: f32, y: f32| mask.at(x as i32, y as i32) >= 128;
                    (Box::new(covers), area)
                }
            };
        if right <= left || bottom <= top {
            bail!(
                "the area lies entirely outside the {}×{} image",
                size.width(),
                size.height()
            );
        }
        let mut random = crate::canvas::Random(self.seed.unwrap_or(seed));
        let mut anywhere = || {
            (
                left + random.next() * (right - left),
                top + random.next() * (bottom - top),
            )
        };
        let covered = (0..400).filter(|_| {
            let (x, y) = anywhere();
            inside(x, y)
        });
        let share = covered.count() as f32 / 400.0;
        if share == 0.0 {
            bail!("the area to lay strokes in is empty");
        }
        let count = self.count.unwrap_or_else(|| {
            let area = (right - left) * (bottom - top) * share;
            let stroke = (lengths[0] + lengths[1]) / 2.0 * (widths[0] + widths[1]) / 2.0;
            (area / stroke * 1.2).ceil().clamp(1.0, 5000.0) as u32
        });
        // How dark each place is: 0 on the side facing the light, 1 away from it
        let center = ((left + right) / 2.0, (top + bottom) / 2.0);
        let shade: Box<dyn Fn(f32, f32) -> f32> = match self.light {
            None => Box::new(|_, _| 1.0),
            Some([lx, ly]) => {
                let (dx, dy) = (center.0 - lx, center.1 - ly);
                let norm = (dx * dx + dy * dy).sqrt().max(1e-6);
                let (dx, dy) = (dx / norm, dy / norm);
                let reach = ((right - left) * dx.abs() + (bottom - top) * dy.abs()) / 2.0;
                Box::new(move |x, y| {
                    let along = (x - center.0) * dx + (y - center.1) * dy;
                    (0.5 + along / (2.0 * reach.max(1.0))).clamp(0.0, 1.0)
                })
            }
        };

        let between = |random: &mut crate::canvas::Random, [low, high]: [f32; 2]| {
            low + random.next() * (high - low)
        };
        let base = self.angle.unwrap_or(45.0);
        let mut strokes = Vec::new();
        let mut tries = 0;
        while strokes.len() < count as usize && tries < count * 50 {
            tries += 1;
            let (x, y) = (
                left + random.next() * (right - left),
                top + random.next() * (bottom - top),
            );
            if !inside(x, y) || random.next() > 0.1 + 0.9 * shade(x, y) {
                continue;
            }
            let angle = (base + (random.next() * 2.0 - 1.0) * spread).to_radians();
            let (dx, dy) = (angle.cos(), angle.sin());
            let half = between(&mut random, lengths) / 2.0;
            // A slight bend, as a hand's stroke has
            let bend = (random.next() - 0.5) * half * 0.2;
            let original = to_points(&[
                [x - dx * half, y - dy * half],
                [x - dy * bend, y + dx * bend],
                [x + dx * half, y + dy * half],
            ]);
            let points = smooth(&original, false);
            let pressure = pressure_along(&original, &points, None, taper);
            let color = colors[(random.next() * colors.len() as f32) as usize % colors.len()];
            strokes.push(Stroke {
                points,
                pressure,
                color,
                width: px(between(&mut random, widths)),
                brush,
                ..Default::default()
            });
        }
        if strokes.is_empty() {
            bail!("no strokes fit in the area");
        }
        self.placement.apply(&mut strokes, seed, masks)?;
        // Each stroke its own bristles and spray
        for stroke in &mut strokes {
            stroke.seed = (random.next().to_bits() as u64) << 20 ^ seed;
        }
        Ok(strokes)
    }
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct HatchParams {
    /// The image's id.
    image: u32,
    #[serde(flatten)]
    hatch: HatchSpec,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DrawStrokeParams {
    /// The image's id.
    image: u32,
    #[serde(flatten)]
    stroke: StrokeSpec,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DrawShapeParams {
    /// The image's id.
    image: u32,
    #[serde(flatten)]
    shape: ShapeSpec,
}

/// One thing for draw to draw: a stroke as draw_stroke takes it, a shape
/// as draw_shape takes it, or strokes as hatch lays them.
#[derive(Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum DrawItem {
    Stroke(StrokeSpec),
    Shape(ShapeSpec),
    Hatch(HatchSpec),
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DrawParams {
    /// The image's id.
    image: u32,
    /// What to draw, in order, later items over earlier ones. Each is
    /// {"type": "stroke", ...} with draw_stroke's arguments, {"type": "shape", ...}
    /// with draw_shape's or {"type": "hatch", ...} with hatch's, without the image.
    items: Vec<DrawItem>,
    /// Masks by name, which items use as "clip": {"mask": "name"}, so one outline
    /// serves many items. They are made from the image as it is before drawing.
    masks: Option<BTreeMap<String, MaskSpec>>,
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum FilterName {
    /// Grain, like paint on paper, so flat colors and smooth gradients look less
    /// perfect.
    Noise,
    /// A Gaussian blur, to soften shadows, glows and edges that are far away or out of
    /// focus.
    Blur,
    /// Breaks smooth shading into flat patches like brush strokes, keeping edges sharp.
    Painterly,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct FilterParams {
    /// The image's id.
    image: u32,
    filter: FilterName,
    /// For noise: how strong the grain is, in percent from 0 to 100. 10 when left out.
    amount: Option<f32>,
    /// For noise: grain in color instead of gray.
    color: Option<bool>,
    /// For blur: how far it spreads, from 1 to 64 pixels, 4 when left out. For
    /// painterly: how large the patches are, from 1 to 16 pixels, 4 when left out.
    radius: Option<f32>,
    /// For noise: fixes the grain, so the same seed gives the same grain. Different
    /// each time when left out.
    seed: Option<u64>,
    /// Where to apply it, leaving the rest of the image as it is: two opposite corners
    /// of a rectangle as [x, y] pairs, or a mask, which can be a shape or the region
    /// around a pixel. The whole image when left out.
    clip: Option<ClipSpec>,
}

impl FilterParams {
    fn build(&self, seed: u64) -> Result<Filter> {
        let noise = matches!(self.filter, FilterName::Noise);
        if !noise && (self.amount.is_some() || self.color.is_some() || self.seed.is_some()) {
            bail!("amount, color and seed are for noise");
        }
        let radius = |max: f32| -> Result<f32> {
            let radius = self.radius.unwrap_or(4.0);
            if !(1.0..=max).contains(&radius) {
                bail!("radius must be between 1 and {max}, not {radius}");
            }
            Ok(radius)
        };
        Ok(match self.filter {
            FilterName::Noise => {
                if self.radius.is_some() {
                    bail!("radius is for blur and painterly");
                }
                let amount = self.amount.unwrap_or(10.0);
                if !(0.0..=100.0).contains(&amount) {
                    bail!("amount must be between 0 and 100, not {amount}");
                }
                Filter::Noise {
                    amount: amount / 100.0,
                    color: self.color == Some(true),
                    seed: self.seed.unwrap_or(seed),
                }
            }
            FilterName::Blur => Filter::Blur {
                radius: radius(64.0)?,
            },
            FilterName::Painterly => Filter::Painterly {
                radius: radius(16.0)?.round() as u32,
            },
        })
    }
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct AdjustParams {
    /// The image's id.
    image: u32,
    /// Turns every color around the color wheel, in degrees from -180 to 180: 120
    /// turns red to green, and green to blue.
    hue: Option<f32>,
    /// From -100, gray, to 100, twice as vivid.
    saturation: Option<f32>,
    /// From -100, black, to 100, white.
    brightness: Option<f32>,
    /// From -100, flat gray, to 100, almost only black and white.
    contrast: Option<f32>,
    /// From -100, cool and blue, to 100, warm and orange, as for evening light.
    warmth: Option<f32>,
    /// Where to adjust, leaving the rest of the image as it is: two opposite corners
    /// of a rectangle as [x, y] pairs, or a mask, which can be a shape or the region
    /// around a pixel. The whole image when left out.
    clip: Option<ClipSpec>,
}

impl AdjustParams {
    fn build(&self) -> Result<Adjustment> {
        let percent = |name: &str, value: Option<f32>| -> Result<f32> {
            let value = value.unwrap_or(0.0);
            if !(-100.0..=100.0).contains(&value) {
                bail!("{name} must be between -100 and 100, not {value}");
            }
            Ok(value / 100.0)
        };
        let hue = self.hue.unwrap_or(0.0);
        if !(-180.0..=180.0).contains(&hue) {
            bail!("hue must be between -180 and 180, not {hue}");
        }
        let adjustment = Adjustment {
            hue,
            saturation: percent("saturation", self.saturation)?,
            brightness: percent("brightness", self.brightness)?,
            contrast: percent("contrast", self.contrast)?,
            warmth: percent("warmth", self.warmth)?,
        };
        if adjustment == Adjustment::default() {
            bail!("give at least one of hue, saturation, brightness, contrast and warmth");
        }
        Ok(adjustment)
    }
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct FillParams {
    /// The image's id.
    image: u32,
    /// A pixel in the region to fill, as [x, y]. The region is the pixels connected to
    /// it that have about the same color.
    point: [f32; 2],
    /// The color to fill with as #rrggbb or #rrggbbaa. With a pattern, the color of
    /// its set pixels.
    color: String,
    /// How different a pixel's color may be and still be filled, in percent from 0
    /// to 100. 25 when left out.
    tolerance: Option<f32>,
    /// An 8×8 pattern lined up with the image, so neighboring fills meet seamlessly:
    /// solid, dark, checkerboard, light, sparse_dots, horizontal_lines, vertical_lines,
    /// rising_diagonals, falling_diagonals, grid, crosshatch, bricks, zigzag, polka_dots
    /// or weave. Solid when left out.
    pattern: Option<String>,
    /// The color of the pattern's other pixels, as #rrggbb or #rrggbbaa. Needed for
    /// any pattern but solid.
    second_color: Option<String>,
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum FontName {
    SansSerif,
    Serif,
    Monospace,
    Cursive,
    Fantasy,
}

impl From<FontName> for FontChoice {
    fn from(name: FontName) -> Self {
        match name {
            FontName::SansSerif => FontChoice::SansSerif,
            FontName::Serif => FontChoice::Serif,
            FontName::Monospace => FontChoice::Monospace,
            FontName::Cursive => FontChoice::Cursive,
            FontName::Fantasy => FontChoice::Fantasy,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DrawTextParams {
    /// The image's id.
    image: u32,
    /// The text, with lines separated by \n. Characters the font lacks, such as
    /// Japanese in a Latin font, come from an installed font that has them.
    text: String,
    /// The top-left of where the text starts, as [x, y].
    position: [i32; 2],
    /// The text's color as #rrggbb or #rrggbbaa.
    color: String,
    /// Font size in pixels, from 8 to 144. 24 when left out.
    size: Option<f32>,
    /// The font. Sans serif when left out.
    font: Option<FontName>,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct RectParams {
    /// The image's id.
    image: u32,
    /// Two opposite corners of the rectangle, as [x, y] pairs.
    rect: [[i32; 2]; 2],
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ResizeParams {
    /// The image's id.
    image: u32,
    /// The new width in pixels, from 1 to 4096.
    width: u32,
    /// The new height in pixels, from 1 to 4096.
    height: u32,
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum FlipDirection {
    /// Left and right swap.
    Horizontal,
    /// Top and bottom swap.
    Vertical,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct FlipParams {
    /// The image's id.
    image: u32,
    direction: FlipDirection,
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum TurnDirection {
    Clockwise,
    Counterclockwise,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct TurnParams {
    /// The image's id.
    image: u32,
    /// Which way to turn the image a quarter turn.
    direction: TurnDirection,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct PickColorParams {
    /// The image's id.
    image: u32,
    /// The pixel to read, as [x, y].
    point: [f32; 2],
}

#[tool_router]
impl PaintServer {
    #[tool(description = "Create a blank image and return its id.")]
    fn new_image(&self, Parameters(params): Parameters<NewImageParams>) -> Result<String, String> {
        let background = match &params.background {
            Some(color) => parse_color(color).map_err(message)?,
            None => gpui_kit::transparent_black(),
        };
        let id = self
            .images()
            .create(params.width, params.height, background)
            .map_err(message)?;
        Ok(format!(
            "Created image {id}, {}×{}",
            params.width, params.height
        ))
    }

    #[tool(description = "Open an image file and return its id.")]
    fn open_image(
        &self,
        Parameters(params): Parameters<OpenImageParams>,
    ) -> Result<String, String> {
        let mut images = self.images();
        let id = images.open(Path::new(&params.path)).map_err(message)?;
        let size = images.get(id).map_err(message)?.size();
        Ok(format!("Opened image {id}, {}×{}", size.width, size.height))
    }

    #[tool(
        description = "Save an image to a file. JPEG, BMP and GIF have no transparency, so \
        transparent areas become white."
    )]
    fn save_image(
        &self,
        Parameters(params): Parameters<SaveImageParams>,
    ) -> Result<String, String> {
        let path = params.path.as_deref().map(Path::new);
        let saved = self.images().save(params.image, path).map_err(message)?;
        Ok(format!(
            "Saved image {} to {}",
            params.image,
            saved.display()
        ))
    }

    #[tool(
        description = "Look at an image, or closely at part of it. It comes back as PNG, scaled \
        down to fit 1024×1024 when it is larger.",
        annotations(read_only_hint = true)
    )]
    fn view_image(
        &self,
        Parameters(params): Parameters<ViewImageParams>,
    ) -> Result<CallToolResult, String> {
        let images = self.images();
        let size = images.get(params.image).map_err(message)?.size();
        let region = params.region.map(to_rect).transpose().map_err(message)?;
        let (png, shown) = images.view(params.image, region).map_err(message)?;
        let mut description = format!("Image {}, {}×{}", params.image, size.width, size.height);
        if let Some(region) = params.region {
            let [[x0, y0], [x1, y1]] = region;
            description += &format!(", the part from ({x0}, {y0}) to ({x1}, {y1})");
        }
        if shown != size {
            description += &format!(", shown at {}×{}", shown.width, shown.height);
        }
        let data = base64::engine::general_purpose::STANDARD.encode(png);
        Ok(CallToolResult::success(vec![
            ContentBlock::text(description),
            ContentBlock::image(data, "image/png"),
        ]))
    }

    #[tool(
        description = "List the open images with their sizes and files.",
        annotations(read_only_hint = true)
    )]
    fn list_images(&self) -> String {
        let images = self.images();
        let lines: Vec<String> = images
            .iter()
            .map(|(id, image)| {
                let size = image.size();
                let file = image
                    .path
                    .as_ref()
                    .map_or("not saved".into(), |path| path.display().to_string());
                format!("Image {id}, {}×{}, {file}", size.width, size.height)
            })
            .collect();
        if lines.is_empty() {
            "No images are open".into()
        } else {
            lines.join("\n")
        }
    }

    #[tool(description = "Close an image without saving it, freeing its memory.")]
    fn close_image(&self, Parameters(params): Parameters<ImageParams>) -> Result<String, String> {
        self.images().close(params.image).map_err(message)?;
        Ok(format!("Closed image {}", params.image))
    }

    #[tool(description = "Draw a freehand line through points with a brush.")]
    fn draw_stroke(
        &self,
        Parameters(params): Parameters<DrawStrokeParams>,
    ) -> Result<String, String> {
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let seed = image.next_seed();
        let masks = Masks {
            canvas: &image.canvas,
            named: &BTreeMap::new(),
            naming: false,
        };
        let stroke = params.stroke.build(seed, &masks).map_err(message)?;
        image.draw(&[stroke]).map_err(message)?;
        Ok(format!("Drew a stroke on image {}", params.image))
    }

    #[tool(
        description = "Draw a line, rectangle, ellipse, polygon, arc, pie or chord, with an \
        outline, a fill or both."
    )]
    fn draw_shape(
        &self,
        Parameters(params): Parameters<DrawShapeParams>,
    ) -> Result<String, String> {
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let seed = image.next_seed();
        let masks = Masks {
            canvas: &image.canvas,
            named: &BTreeMap::new(),
            naming: false,
        };
        let strokes = params.shape.build(seed, &masks).map_err(message)?;
        image.draw(&strokes).map_err(message)?;
        Ok(format!(
            "Drew {} on image {}",
            params.shape.shape.label(),
            params.image
        ))
    }

    #[tool(
        description = "Draw several strokes and shapes in one call, as one undo step. Nothing is \
        drawn if any item is wrong."
    )]
    fn draw(&self, Parameters(params): Parameters<DrawParams>) -> Result<String, String> {
        if params.items.is_empty() {
            return Err("give at least one item".into());
        }
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let size = image.size();
        let mut named = BTreeMap::new();
        for (name, spec) in params.masks.iter().flatten() {
            let masks = Masks {
                canvas: &image.canvas,
                named: &BTreeMap::new(),
                naming: true,
            };
            let mask = spec.build(&masks);
            let mask =
                mask.map_err(|error| format!("mask {name:?}: {error:#}; nothing was drawn"))?;
            named.insert(name.clone(), mask);
        }
        let mut strokes = Vec::new();
        for (index, item) in params.items.iter().enumerate() {
            let seed = image.next_seed();
            let masks = Masks {
                canvas: &image.canvas,
                named: &named,
                naming: false,
            };
            let built = match item {
                DrawItem::Stroke(stroke) => stroke.build(seed, &masks).map(|s| vec![s]),
                DrawItem::Shape(shape) => shape.build(seed, &masks),
                DrawItem::Hatch(hatch) => hatch.build(seed, &masks),
            }
            .and_then(|built| {
                if !built.iter().any(|stroke| image.canvas.reaches(stroke)) {
                    bail!(
                        "it lies entirely outside the {}×{} image",
                        size.width,
                        size.height
                    );
                }
                Ok(built)
            })
            .map_err(|error| format!("item {}: {error:#}; nothing was drawn", index + 1))?;
            strokes.extend(built);
        }
        image.draw(&strokes).map_err(message)?;
        let count = params.items.len();
        let items = if count == 1 { "item" } else { "items" };
        Ok(format!("Drew {count} {items} on image {}", params.image))
    }

    #[tool(
        description = "Lay many short strokes at random across a rectangle or mask, as one \
        undo step: hatching, grass, fur, or brushwork on a wall. With light, they gather on \
        the side away from it, shading with strokes instead of a gradient."
    )]
    fn hatch(&self, Parameters(params): Parameters<HatchParams>) -> Result<String, String> {
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let seed = image.next_seed();
        let masks = Masks {
            canvas: &image.canvas,
            named: &BTreeMap::new(),
            naming: false,
        };
        let strokes = params.hatch.build(seed, &masks).map_err(message)?;
        image.draw(&strokes).map_err(message)?;
        Ok(format!(
            "Laid {} strokes on image {}",
            strokes.len(),
            params.image
        ))
    }

    #[tool(
        description = "Fill the region around a pixel that has about the same color, like a \
        paint bucket, with a color or a two-color pattern."
    )]
    fn fill(&self, Parameters(params): Parameters<FillParams>) -> Result<String, String> {
        let foreground = parse_color(&params.color).map_err(message)?;
        let pattern = match &params.pattern {
            Some(name) => find_pattern(name).map_err(message)?,
            None => Pattern::SOLID,
        };
        let background = match (&params.second_color, pattern.is_solid()) {
            (Some(color), _) => parse_color(color).map_err(message)?,
            (None, true) => foreground,
            (None, false) => {
                return Err("a pattern other than solid needs a second_color".into());
            }
        };
        let tolerance = parse_tolerance(params.tolerance).map_err(message)?;
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let [x, y] = params.point;
        check_inside(image, x, y).map_err(message)?;
        let fill = Fill {
            pattern,
            foreground,
            background,
        };
        let undo = image
            .canvas
            .flood_fill(point(px(x), px(y)), fill, tolerance)
            .ok_or("nothing changed, as the region already has that color")?;
        image.history.record(undo);
        Ok(format!(
            "Filled around ({x}, {y}) on image {}",
            params.image
        ))
    }

    #[tool(
        description = "Apply a filter to an image, or to part of it: noise for grain, blur to \
        soften, or painterly to break smooth shading into patches like brush strokes. Use it \
        to make clean drawing look painted by hand."
    )]
    fn filter(&self, Parameters(params): Parameters<FilterParams>) -> Result<String, String> {
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let filter = params.build(image.next_seed()).map_err(message)?;
        apply_filter(image, filter, params.clip.as_ref())?;
        Ok(format!("Filtered image {}", params.image))
    }

    #[tool(
        description = "Adjust the colors of an image, or of part of it: turn the hue, change \
        saturation, brightness and contrast, or make it warmer or cooler. Use it to set the \
        mood of a finished picture, or to tint the shadows or light inside a mask."
    )]
    fn adjust(&self, Parameters(params): Parameters<AdjustParams>) -> Result<String, String> {
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let adjustment = params.build().map_err(message)?;
        apply_filter(image, Filter::Adjust(adjustment), params.clip.as_ref())?;
        Ok(format!("Adjusted image {}", params.image))
    }

    #[tool(description = "Write text on an image.")]
    fn draw_text(&self, Parameters(params): Parameters<DrawTextParams>) -> Result<String, String> {
        if params.text.trim().is_empty() {
            return Err("the text has nothing to draw".into());
        }
        let color = parse_color(&params.color).map_err(message)?;
        let size = params.size.unwrap_or(DEFAULT_FONT_SIZE);
        if !(MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&size) {
            return Err(format!(
                "size must be between {MIN_FONT_SIZE} and {MAX_FONT_SIZE}, not {size}"
            ));
        }
        let font = params.font.map_or(FontChoice::SansSerif, FontChoice::from);
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let mut renderer = self
            .text_renderer
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let renderer = renderer.get_or_insert_with(TextRenderer::new);
        let [x, y] = params.position;
        let undo = image
            .canvas
            .draw_text(renderer, &params.text, font, size, color, point(x, y))
            .ok_or_else(|| outside(image.size()))?;
        image.history.record(undo);
        Ok(format!("Drew text on image {}", params.image))
    }

    #[tool(description = "Erase a rectangle to the image's background.")]
    fn erase(&self, Parameters(params): Parameters<RectParams>) -> Result<String, String> {
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let rect = to_rect(params.rect).map_err(message)?;
        let undo = image
            .canvas
            .erase(rect, None)
            .ok_or_else(|| outside(image.size()))?;
        image.history.record(undo);
        Ok(format!("Erased a rectangle on image {}", params.image))
    }

    #[tool(description = "Erase the whole image to its background.")]
    fn clear(&self, Parameters(params): Parameters<ImageParams>) -> Result<String, String> {
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let undo = image.canvas.clear().map_err(message)?;
        image.history.record(undo);
        Ok(format!("Cleared image {}", params.image))
    }

    #[tool(
        description = "Change an image's size, keeping the drawing at the top-left. Growing adds \
        background; shrinking cuts off the right and bottom."
    )]
    fn resize(&self, Parameters(params): Parameters<ResizeParams>) -> Result<String, String> {
        let (width, height) = (params.width, params.height);
        let range = 1..=MAX_CANVAS_SIDE;
        if !range.contains(&width) || !range.contains(&height) {
            return Err(format!(
                "width and height must be between 1 and {MAX_CANVAS_SIDE}, not {width}×{height}"
            ));
        }
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let undo = image.canvas.resize(width, height).map_err(message)?;
        image.history.record(undo);
        Ok(format!(
            "Resized image {} to {width}×{height}",
            params.image
        ))
    }

    #[tool(description = "Cut an image down to a rectangle.")]
    fn crop(&self, Parameters(params): Parameters<RectParams>) -> Result<String, String> {
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let rect = to_rect(params.rect).map_err(message)?;
        let pixels = image
            .canvas
            .copy(rect, None)
            .ok_or_else(|| outside(image.size()))?;
        let undo = image.canvas.replace(pixels);
        image.history.record(undo);
        let size = image.size();
        Ok(format!(
            "Cropped image {} to {}×{}",
            params.image, size.width, size.height
        ))
    }

    #[tool(description = "Flip an image horizontally or vertically.")]
    fn flip(&self, Parameters(params): Parameters<FlipParams>) -> Result<String, String> {
        let orientation = match params.direction {
            FlipDirection::Horizontal => Orientation::FlipHorizontal,
            FlipDirection::Vertical => Orientation::FlipVertical,
        };
        self.reorient(params.image, orientation)?;
        Ok(format!("Flipped image {}", params.image))
    }

    #[tool(description = "Turn an image a quarter turn, swapping its width and height.")]
    fn turn(&self, Parameters(params): Parameters<TurnParams>) -> Result<String, String> {
        let orientation = match params.direction {
            TurnDirection::Clockwise => Orientation::RotateClockwise,
            TurnDirection::Counterclockwise => Orientation::RotateCounterClockwise,
        };
        self.reorient(params.image, orientation)?;
        Ok(format!("Turned image {}", params.image))
    }

    #[tool(
        description = "Read the color of a pixel as #rrggbbaa.",
        annotations(read_only_hint = true)
    )]
    fn pick_color(
        &self,
        Parameters(params): Parameters<PickColorParams>,
    ) -> Result<String, String> {
        let images = self.images();
        let image = images.get(params.image).map_err(message)?;
        let [x, y] = params.point;
        check_inside(image, x, y).map_err(message)?;
        let pixel = image
            .canvas
            .pixels()
            .pixel(x as u32, y as u32)
            .ok_or_else(|| outside(image.size()))?
            .demultiply();
        Ok(format!(
            "#{:02x}{:02x}{:02x}{:02x}",
            pixel.red(),
            pixel.green(),
            pixel.blue(),
            pixel.alpha()
        ))
    }

    #[tool(description = "Take back the last change to an image.")]
    fn undo(&self, Parameters(params): Parameters<ImageParams>) -> Result<String, String> {
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        if !image.history.undo(&mut image.canvas) {
            return Err(format!("image {} has nothing to undo", params.image));
        }
        Ok(format!("Undid the last change to image {}", params.image))
    }

    #[tool(description = "Make the last undone change to an image again.")]
    fn redo(&self, Parameters(params): Parameters<ImageParams>) -> Result<String, String> {
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        if !image.history.redo(&mut image.canvas) {
            return Err(format!("image {} has nothing to redo", params.image));
        }
        Ok(format!(
            "Redid the last undone change to image {}",
            params.image
        ))
    }
}

#[tool_handler]
impl ServerHandler for PaintServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "petit-paint",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

impl PaintServer {
    fn reorient(&self, id: u32, orientation: Orientation) -> Result<(), String> {
        let mut images = self.images();
        let image = images.get_mut(id).map_err(message)?;
        let undo = image.canvas.reorient(orientation);
        image.history.record(undo);
        Ok(())
    }

    fn images(&self) -> MutexGuard<'_, Images> {
        // A tool that panicked leaves the images as they were, which is still
        // better than refusing every later call
        self.images.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Serves MCP over stdin and stdout until the client disconnects.
pub fn run() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let server = PaintServer::default()
            .serve(rmcp::transport::stdio())
            .await?;
        server.waiting().await?;
        Ok(())
    })
}

/// Parses `#rrggbb` or `#rrggbbaa`.
fn parse_color(text: &str) -> Result<Hsla> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    // from_str_radix also takes a leading sign
    let value = u32::from_str_radix(hex, 16)
        .ok()
        .filter(|_| hex.chars().all(|c| c.is_ascii_hexdigit()));
    match (hex.len(), value) {
        (6, Some(value)) => Ok(rgba(value << 8 | 0xff).into()),
        (8, Some(value)) => Ok(rgba(value).into()),
        _ => bail!("{text:?} is not a color; use #rrggbb or #rrggbbaa"),
    }
}

/// A fill tolerance from a percentage from 0 to 100, the fill tool's
/// default when left out.
fn parse_tolerance(percent: Option<f32>) -> Result<u8> {
    let percent = percent.unwrap_or(tolerance_percent(DEFAULT_FILL_TOLERANCE));
    if !(0.0..=100.0).contains(&percent) {
        bail!("tolerance must be between 0 and 100, not {percent}");
    }
    Ok(tolerance_from_percent(percent))
}

/// A width in pixels from `MIN_WIDTH` to `MAX_WIDTH`, `DEFAULT_WIDTH` when
/// left out.
fn parse_width(width: Option<f32>) -> Result<Pixels> {
    let width = width.unwrap_or(DEFAULT_WIDTH);
    if !(MIN_WIDTH..=MAX_WIDTH).contains(&width) {
        bail!("width must be between {MIN_WIDTH} and {MAX_WIDTH}, not {width}");
    }
    Ok(px(width))
}

/// The pattern named `name`, as the fill tool's names are written in
/// snake_case.
fn find_pattern(name: &str) -> Result<Pattern> {
    let snake_case = |pattern: &Pattern| pattern.name.to_lowercase().replace(' ', "_");
    PATTERNS
        .iter()
        .find(|pattern| snake_case(pattern) == name)
        .copied()
        .ok_or_else(|| {
            let names: Vec<String> = PATTERNS.iter().map(snake_case).collect();
            anyhow::anyhow!("there is no {name:?} pattern; use {}", names.join(", "))
        })
}

/// A rectangle from two opposite corners.
fn to_rect([[x0, y0], [x1, y1]]: [[i32; 2]; 2]) -> Result<PixelRect> {
    if x0 == x1 || y0 == y1 {
        bail!("the rectangle from ({x0}, {y0}) to ({x1}, {y1}) is empty");
    }
    Ok(PixelRect {
        x: x0.min(x1),
        y: y0.min(y1),
        width: x0.abs_diff(x1),
        height: y0.abs_diff(y1),
    })
}

/// Fails unless (`x`, `y`) is a pixel of `image`.
fn check_inside(image: &images::Image, x: f32, y: f32) -> Result<()> {
    let size = image.size();
    if x < 0.0 || y < 0.0 || x >= size.width as f32 || y >= size.height as f32 {
        bail!(
            "({x}, {y}) is outside the {}×{} image",
            size.width,
            size.height
        );
    }
    Ok(())
}

/// Applies `filter` to `image` inside `clip`, or to the whole image, as one
/// undo step.
fn apply_filter(
    image: &mut images::Image,
    filter: Filter,
    clip: Option<&ClipSpec>,
) -> Result<(), String> {
    let masks = Masks {
        canvas: &image.canvas,
        named: &BTreeMap::new(),
        naming: false,
    };
    let (clip, mask) = match clip {
        None => (None, None),
        Some(ClipSpec::Rectangle(rect)) => (Some(to_rect(*rect).map_err(message)?), None),
        Some(ClipSpec::Mask(mask)) => (None, Some(mask.build(&masks).map_err(message)?)),
    };
    let undo = image
        .canvas
        .filter(filter, clip, mask.as_deref())
        .ok_or_else(|| outside(image.size()))?;
    image.history.record(undo);
    Ok(())
}

fn outside(size: gpui_kit::Size<u32>) -> String {
    format!(
        "nothing was changed, as it lies entirely outside the {}×{} image",
        size.width, size.height
    )
}

fn to_points(points: &[[f32; 2]]) -> Vec<Point<Pixels>> {
    points.iter().map(|&[x, y]| point(px(x), px(y))).collect()
}

fn message(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[cfg(test)]
mod tests {
    use super::*;

    impl From<&str> for FillSpec {
        fn from(color: &str) -> Self {
            FillSpec::Color(color.into())
        }
    }

    /// A server with one white 100×100 image, id 1.
    fn server() -> PaintServer {
        let server = PaintServer::default();
        server
            .new_image(Parameters(NewImageParams {
                width: 100,
                height: 100,
                background: Some("#ffffff".into()),
            }))
            .unwrap();
        server
    }

    fn rgb_at(server: &PaintServer, x: u32, y: u32) -> (u8, u8, u8) {
        let images = server.images();
        let pixel = images.get(1).unwrap().canvas.pixels().pixel(x, y).unwrap();
        (pixel.red(), pixel.green(), pixel.blue())
    }

    fn shape(shape: ShapeName, points: &[[f32; 2]]) -> ShapeSpec {
        ShapeSpec {
            shape,
            points: points.to_vec(),
            outline: None,
            fill: None,
            width: None,
            smooth: None,
            radius: None,
            start_angle: None,
            end_angle: None,
            placement: placement(),
        }
    }

    fn placement() -> Placement {
        Placement {
            blend: None,
            clip: None,
            roughen: None,
            jitter: None,
        }
    }

    fn draw_shape(server: &PaintServer, shape: ShapeSpec) -> Result<String, String> {
        server.draw_shape(Parameters(DrawShapeParams { image: 1, shape }))
    }

    fn stroke(points: &[[f32; 2]]) -> StrokeSpec {
        StrokeSpec {
            points: points.to_vec(),
            color: Some("#000000".into()),
            width: None,
            brush: None,
            smooth: None,
            taper: None,
            pressure: None,
            placement: placement(),
        }
    }

    fn draw_stroke(server: &PaintServer, stroke: StrokeSpec) -> Result<String, String> {
        server.draw_stroke(Parameters(DrawStrokeParams { image: 1, stroke }))
    }

    fn draw(server: &PaintServer, items: rmcp::serde_json::Value) -> Result<String, String> {
        let params = rmcp::serde_json::json!({ "image": 1, "items": items });
        server.draw(Parameters(rmcp::serde_json::from_value(params).unwrap()))
    }

    #[test]
    fn draws_a_list_in_order_as_one_undo_step() {
        let server = server();
        let drew = draw(
            &server,
            rmcp::serde_json::json!([
                {"type": "shape", "shape": "rectangle", "points": [[20, 20], [80, 80]], "fill": "#ff0000"},
                {"type": "stroke", "points": [[10, 50], [90, 50]], "color": "#0000ff", "width": 6},
                {"type": "shape", "shape": "ellipse", "points": [[40, 60], [60, 76]], "fill": "#00ff00", "outline": "#000000"},
            ]),
        )
        .unwrap();
        assert_eq!(drew, "Drew 3 items on image 1");
        assert_eq!(rgb_at(&server, 30, 30), (255, 0, 0));
        // Later items cover earlier ones
        assert_eq!(rgb_at(&server, 50, 50), (0, 0, 255));
        assert_eq!(rgb_at(&server, 50, 68), (0, 255, 0));

        server.undo(Parameters(ImageParams { image: 1 })).unwrap();
        for (x, y) in [(30, 30), (50, 50), (50, 68)] {
            assert_eq!(rgb_at(&server, x, y), (255, 255, 255));
        }
        assert!(server.undo(Parameters(ImageParams { image: 1 })).is_err());
    }

    #[test]
    fn a_wrong_item_is_named_and_nothing_is_drawn() {
        let server = server();
        let error = draw(
            &server,
            rmcp::serde_json::json!([
                {"type": "shape", "shape": "rectangle", "points": [[20, 20], [80, 80]], "fill": "#ff0000"},
                {"type": "stroke", "points": [[10, 50], [90, 50]], "color": "#0000ff"},
                {"type": "shape", "shape": "polygon", "points": [[0, 0], [5, 5]], "outline": "#000000"},
            ]),
        )
        .unwrap_err();
        assert!(error.starts_with("item 3: a polygon"), "{error}");
        assert_eq!(rgb_at(&server, 30, 30), (255, 255, 255));
        assert!(server.undo(Parameters(ImageParams { image: 1 })).is_err());
    }

    #[test]
    fn an_item_outside_the_image_is_wrong() {
        let server = server();
        let error = draw(
            &server,
            rmcp::serde_json::json!([
                {"type": "stroke", "points": [[10, 50], [90, 50]], "color": "#0000ff"},
                {"type": "stroke", "points": [[300, 300], [400, 300]], "color": "#0000ff"},
            ]),
        )
        .unwrap_err();
        assert!(
            error.starts_with("item 2: it lies entirely outside the 100×100 image"),
            "{error}"
        );
        assert_eq!(rgb_at(&server, 50, 50), (255, 255, 255));
    }

    #[test]
    fn refuses_an_empty_list_and_unknown_types() {
        let server = server();
        assert!(draw(&server, rmcp::serde_json::json!([])).is_err());
        let unknown = rmcp::serde_json::json!({
            "image": 1,
            "items": [{"type": "star", "points": [[1, 1]]}],
        });
        assert!(rmcp::serde_json::from_value::<DrawParams>(unknown).is_err());
    }

    fn image(id: u32) -> Parameters<ImageParams> {
        Parameters(ImageParams { image: id })
    }

    fn size(server: &PaintServer) -> (u32, u32) {
        let size = server.images().get(1).unwrap().size();
        (size.width, size.height)
    }

    fn fill(color: &str, point: [f32; 2]) -> FillParams {
        FillParams {
            image: 1,
            point,
            color: color.into(),
            tolerance: None,
            pattern: None,
            second_color: None,
        }
    }

    #[test]
    fn fills_the_region_inside_an_outline() {
        let server = server();
        let outline = ShapeSpec {
            outline: Some("#000000".into()),
            ..shape(ShapeName::Rectangle, &[[20.0, 20.0], [80.0, 80.0]])
        };
        draw_shape(&server, outline).unwrap();
        server
            .fill(Parameters(fill("#ff0000", [50.0, 50.0])))
            .unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (255, 0, 0));
        assert_eq!(rgb_at(&server, 5, 5), (255, 255, 255));
        server.undo(image(1)).unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (255, 255, 255));
    }

    #[test]
    fn fills_with_a_pattern() {
        let server = server();
        let params = FillParams {
            pattern: Some("checkerboard".into()),
            second_color: Some("#0000ff".into()),
            ..fill("#ff0000", [0.0, 0.0])
        };
        server.fill(Parameters(params)).unwrap();
        assert_eq!(rgb_at(&server, 0, 0), (255, 0, 0));
        assert_eq!(rgb_at(&server, 1, 0), (0, 0, 255));
    }

    #[test]
    fn refuses_bad_fills() {
        let server = server();
        let cases = [
            fill("#ff0000", [100.0, 50.0]),
            fill("#ff0000", [-1.0, 50.0]),
            // The image is already white
            fill("#ffffff", [50.0, 50.0]),
            FillParams {
                tolerance: Some(101.0),
                ..fill("#ff0000", [50.0, 50.0])
            },
            FillParams {
                pattern: Some("stripes".into()),
                ..fill("#ff0000", [50.0, 50.0])
            },
            FillParams {
                pattern: Some("bricks".into()),
                ..fill("#ff0000", [50.0, 50.0])
            },
        ];
        for params in cases {
            assert!(server.fill(Parameters(params)).is_err());
        }
        assert!(server.undo(image(1)).is_err());
    }

    #[test]
    fn every_pattern_has_a_name_fill_takes() {
        for pattern in PATTERNS {
            let name = pattern.name.to_lowercase().replace(' ', "_");
            assert_eq!(find_pattern(&name).unwrap(), pattern);
        }
    }

    fn text(text: &str, position: [i32; 2]) -> DrawTextParams {
        DrawTextParams {
            image: 1,
            text: text.into(),
            position,
            color: "#000000".into(),
            size: Some(32.0),
            font: None,
        }
    }

    #[test]
    fn draws_text_and_undoes_it() {
        let server = server();
        server.draw_text(Parameters(text("Hi", [10, 10]))).unwrap();
        let images = server.images();
        let pixels = images.get(1).unwrap().canvas.pixels();
        let inked = (0..100)
            .flat_map(|y| (0..100).map(move |x| (x, y)))
            .filter(|&(x, y)| pixels.pixel(x, y).unwrap().red() < 128)
            .count();
        assert!(inked > 20, "{inked}");
        drop(images);
        server.undo(image(1)).unwrap();
        assert_eq!(rgb_at(&server, 20, 25), (255, 255, 255));
    }

    #[test]
    fn refuses_bad_text() {
        let server = server();
        for params in [
            text("  ", [10, 10]),
            text("Hi", [500, 500]),
            DrawTextParams {
                size: Some(200.0),
                ..text("Hi", [10, 10])
            },
        ] {
            assert!(server.draw_text(Parameters(params)).is_err());
        }
    }

    #[test]
    fn erases_a_rectangle_and_clears_the_image() {
        let server = server();
        server
            .fill(Parameters(fill("#ff0000", [0.0, 0.0])))
            .unwrap();
        let rect = |rect| Parameters(RectParams { image: 1, rect });
        server.erase(rect([[60, 60], [20, 20]])).unwrap();
        assert_eq!(rgb_at(&server, 40, 40), (255, 255, 255));
        assert_eq!(rgb_at(&server, 10, 10), (255, 0, 0));
        assert!(server.erase(rect([[20, 20], [20, 60]])).is_err());
        assert!(server.erase(rect([[200, 200], [300, 300]])).is_err());

        server.clear(image(1)).unwrap();
        assert_eq!(rgb_at(&server, 10, 10), (255, 255, 255));
        server.undo(image(1)).unwrap();
        assert_eq!(rgb_at(&server, 10, 10), (255, 0, 0));
    }

    #[test]
    fn resizes_and_crops() {
        let server = server();
        let resize = |width, height| {
            server.resize(Parameters(ResizeParams {
                image: 1,
                width,
                height,
            }))
        };
        resize(200, 50).unwrap();
        assert_eq!(size(&server), (200, 50));
        assert!(resize(0, 50).is_err());
        assert!(resize(200, MAX_CANVAS_SIDE + 1).is_err());

        let crop = |rect| server.crop(Parameters(RectParams { image: 1, rect }));
        crop([[10, 10], [40, 30]]).unwrap();
        assert_eq!(size(&server), (30, 20));
        // Only the part on the image is kept
        crop([[20, 10], [100, 100]]).unwrap();
        assert_eq!(size(&server), (10, 10));
        assert!(crop([[50, 50], [60, 60]]).is_err());

        server.undo(image(1)).unwrap();
        server.undo(image(1)).unwrap();
        server.undo(image(1)).unwrap();
        assert_eq!(size(&server), (100, 100));
    }

    #[test]
    fn flips_and_turns() {
        let server = server();
        server
            .resize(Parameters(ResizeParams {
                image: 1,
                width: 100,
                height: 40,
            }))
            .unwrap();
        server
            .fill(Parameters(fill("#ff0000", [0.0, 0.0])))
            .unwrap();
        server
            .erase(Parameters(RectParams {
                image: 1,
                rect: [[50, 0], [100, 40]],
            }))
            .unwrap();
        server
            .flip(Parameters(FlipParams {
                image: 1,
                direction: FlipDirection::Horizontal,
            }))
            .unwrap();
        assert_eq!(rgb_at(&server, 90, 20), (255, 0, 0));
        assert_eq!(rgb_at(&server, 10, 20), (255, 255, 255));
        server
            .turn(Parameters(TurnParams {
                image: 1,
                direction: TurnDirection::Clockwise,
            }))
            .unwrap();
        assert_eq!(size(&server), (40, 100));
        assert_eq!(rgb_at(&server, 20, 90), (255, 0, 0));
    }

    #[test]
    fn picks_colors() {
        let server = server();
        let pick = |point| server.pick_color(Parameters(PickColorParams { image: 1, point }));
        assert_eq!(pick([5.0, 5.0]).unwrap(), "#ffffffff");
        server
            .fill(Parameters(fill("#12345680", [0.0, 0.0])))
            .unwrap();
        assert!(pick([99.5, 0.0]).unwrap().starts_with("#"));
        assert!(pick([100.0, 0.0]).is_err());
        let empty = PaintServer::default();
        empty
            .new_image(Parameters(NewImageParams {
                width: 4,
                height: 4,
                background: None,
            }))
            .unwrap();
        let transparent = empty.pick_color(Parameters(PickColorParams {
            image: 1,
            point: [0.0, 0.0],
        }));
        assert_eq!(transparent.unwrap(), "#00000000");
    }

    #[test]
    fn smooth_strokes_curve_between_their_points() {
        let server = server();
        let zigzag = [[10.0, 80.0], [50.0, 20.0], [90.0, 80.0]];
        draw_stroke(&server, stroke(&zigzag)).unwrap();
        let straight = server.images().get(1).unwrap().canvas.pixels().clone();
        server.undo(image(1)).unwrap();
        let curved = StrokeSpec {
            smooth: Some(true),
            ..stroke(&zigzag)
        };
        draw_stroke(&server, curved).unwrap();
        let images = server.images();
        let curved = images.get(1).unwrap().canvas.pixels();
        assert_ne!(curved.data(), straight.data());
        // The curve still passes through the middle point
        let pixel = curved.pixel(50, 21).unwrap();
        assert!(pixel.red() < 128, "{pixel:?}");
    }

    #[test]
    fn rounds_a_rectangle_and_draws_a_smooth_polygon() {
        let server = server();
        let rounded = ShapeSpec {
            fill: Some("#ff0000".into()),
            radius: Some(20.0),
            ..shape(ShapeName::Rectangle, &[[10.0, 10.0], [90.0, 90.0]])
        };
        draw_shape(&server, rounded).unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (255, 0, 0));
        assert_eq!(rgb_at(&server, 50, 11), (255, 0, 0));
        // The corner is cut away
        assert_eq!(rgb_at(&server, 11, 11), (255, 255, 255));

        let blob = ShapeSpec {
            fill: Some("#0000ff".into()),
            smooth: Some(true),
            ..shape(
                ShapeName::Polygon,
                &[[30.0, 30.0], [70.0, 30.0], [70.0, 70.0], [30.0, 70.0]],
            )
        };
        draw_shape(&server, blob).unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (0, 0, 255));
        // The curve passes through the corners and bulges out between them
        assert_eq!(rgb_at(&server, 50, 27), (0, 0, 255));
        assert_eq!(rgb_at(&server, 50, 22), (255, 0, 0));
    }

    #[test]
    fn draws_arcs_pies_and_chords() {
        let server = server();
        let box_ = [[10.0, 10.0], [90.0, 90.0]];
        // The top half of a circle, like a sun on the horizon
        let pie = ShapeSpec {
            fill: Some("#ff0000".into()),
            start_angle: Some(180.0),
            end_angle: Some(360.0),
            ..shape(ShapeName::Chord, &box_)
        };
        draw_shape(&server, pie).unwrap();
        assert_eq!(rgb_at(&server, 50, 30), (255, 0, 0));
        assert_eq!(rgb_at(&server, 50, 70), (255, 255, 255));

        let slice = ShapeSpec {
            fill: Some("#00ff00".into()),
            start_angle: Some(0.0),
            end_angle: Some(90.0),
            ..shape(ShapeName::Pie, &box_)
        };
        draw_shape(&server, slice).unwrap();
        assert_eq!(rgb_at(&server, 65, 65), (0, 255, 0));
        assert_eq!(rgb_at(&server, 35, 65), (255, 255, 255));

        let arc = ShapeSpec {
            outline: Some("#000000".into()),
            start_angle: Some(90.0),
            end_angle: Some(180.0),
            ..shape(ShapeName::Arc, &box_)
        };
        draw_shape(&server, arc).unwrap();
        assert_eq!(rgb_at(&server, 50, 90), (0, 0, 0));
        assert_eq!(rgb_at(&server, 35, 65), (255, 255, 255));
    }

    #[test]
    fn refuses_options_that_do_not_fit_the_shape() {
        let server = server();
        let box_ = [[10.0, 10.0], [90.0, 90.0]];
        let black = || Some("#000000".to_string());
        let cases = [
            ShapeSpec {
                outline: black(),
                ..shape(ShapeName::Arc, &box_)
            },
            ShapeSpec {
                outline: black(),
                start_angle: Some(0.0),
                ..shape(ShapeName::Pie, &box_)
            },
            ShapeSpec {
                outline: black(),
                start_angle: Some(0.0),
                end_angle: Some(90.0),
                ..shape(ShapeName::Ellipse, &box_)
            },
            ShapeSpec {
                fill: Some("#000000".into()),
                start_angle: Some(0.0),
                end_angle: Some(90.0),
                ..shape(ShapeName::Arc, &box_)
            },
            ShapeSpec {
                outline: black(),
                radius: Some(5.0),
                ..shape(ShapeName::Ellipse, &box_)
            },
            ShapeSpec {
                outline: black(),
                radius: Some(-5.0),
                ..shape(ShapeName::Rectangle, &box_)
            },
            ShapeSpec {
                outline: black(),
                smooth: Some(true),
                ..shape(ShapeName::Rectangle, &box_)
            },
        ];
        for params in cases {
            assert!(draw_shape(&server, params).is_err());
        }
    }

    #[test]
    fn adds_light_and_clips_to_a_rectangle() {
        let server = server();
        let dark = ShapeSpec {
            fill: Some("#202020".into()),
            ..shape(ShapeName::Rectangle, &[[0.0, 0.0], [100.0, 100.0]])
        };
        draw_shape(&server, dark).unwrap();
        let glow = ShapeSpec {
            fill: Some("#404040".into()),
            placement: Placement {
                blend: Some(BlendName::Add),
                clip: Some(ClipSpec::Rectangle([[0, 0], [100, 50]])),
                ..placement()
            },
            ..shape(ShapeName::Ellipse, &[[20.0, 20.0], [80.0, 80.0]])
        };
        draw_shape(&server, glow).unwrap();
        // Added onto the dark color above the clip's edge, untouched below it
        assert_eq!(rgb_at(&server, 50, 40), (96, 96, 96));
        assert_eq!(rgb_at(&server, 50, 60), (32, 32, 32));

        let outside = ShapeSpec {
            fill: Some("#ffffff".into()),
            placement: Placement {
                blend: None,
                clip: Some(ClipSpec::Rectangle([[0, 60], [10, 70]])),
                ..placement()
            },
            ..shape(ShapeName::Ellipse, &[[20.0, 20.0], [80.0, 40.0]])
        };
        assert!(
            draw_shape(&server, outside)
                .unwrap_err()
                .contains("outside")
        );
    }

    #[test]
    fn placement_works_in_draw() {
        let server = server();
        let drew = draw(
            &server,
            rmcp::serde_json::json!([
                {"type": "stroke", "points": [[50, 0], [50, 100]], "color": "#000000", "width": 10,
                 "clip": [[0, 0], [100, 50]]},
                {"type": "shape", "shape": "rectangle", "points": [[0, 0], [20, 20]], "fill": "#808080",
                 "blend": "multiply"},
            ]),
        );
        drew.unwrap();
        assert_eq!(rgb_at(&server, 50, 40), (0, 0, 0));
        assert_eq!(rgb_at(&server, 50, 60), (255, 255, 255));
        assert_eq!(rgb_at(&server, 10, 10), (128, 128, 128));
    }

    #[test]
    fn fills_shapes_with_gradients() {
        let server = server();
        let drew = draw(
            &server,
            rmcp::serde_json::json!([
                {"type": "shape", "shape": "rectangle", "points": [[0, 0], [100, 100]],
                 "fill": {"linear": {"from": [0, 0], "to": [0, 100],
                                     "stops": [[0, "#000000"], [1, "#0000ff"]]}}},
                {"type": "shape", "shape": "ellipse", "points": [[20, 20], [80, 80]],
                 "fill": {"radial": {"center": [50, 50], "radius": 30,
                                     "stops": [[0, "#ffff00"], [1, "#ffff0000"]]}}},
            ]),
        );
        drew.unwrap();
        let (top, bottom) = (rgb_at(&server, 5, 1), rgb_at(&server, 5, 98));
        assert!(top.2 < 10 && bottom.2 > 245, "{top:?} {bottom:?}");
        let center = rgb_at(&server, 50, 50);
        assert!(center.0 > 245 && center.1 > 245, "{center:?}");
    }

    #[test]
    fn explains_wrong_gradients() {
        let server = server();
        let wrong = |fill: rmcp::serde_json::Value| {
            let item = rmcp::serde_json::json!({
                "type": "shape", "shape": "rectangle", "points": [[0, 0], [10, 10]], "fill": fill,
            });
            let params = rmcp::serde_json::json!({"image": 1, "items": [item]});
            match rmcp::serde_json::from_value::<DrawParams>(params) {
                Err(error) => error.to_string(),
                Ok(params) => server.draw(Parameters(params)).unwrap_err(),
            }
        };
        let stops = rmcp::serde_json::json!([[0, "#000000"], [1, "#ffffff"]]);
        let cases = [
            (rmcp::serde_json::json!({"conic": {}}), "conic"),
            (
                rmcp::serde_json::json!({"linear": {"from": [0, 0], "stops": stops}}),
                "to",
            ),
            (
                rmcp::serde_json::json!({"linear": {"from": [0, 0], "to": [0, 0], "stops": stops}}),
                "differ",
            ),
            (
                rmcp::serde_json::json!({"radial": {"center": [0, 0], "radius": 0, "stops": stops}}),
                "radius",
            ),
            (
                rmcp::serde_json::json!({"linear": {"from": [0, 0], "to": [9, 0], "stops": [[0, "#000000"]]}}),
                "two or more",
            ),
            (
                rmcp::serde_json::json!({"linear": {"from": [0, 0], "to": [9, 0],
                                                    "stops": [[0.5, "#000000"], [0.2, "#ffffff"]]}}),
                "in order",
            ),
            (
                rmcp::serde_json::json!({"linear": {"from": [0, 0], "to": [9, 0],
                                                    "stops": [[0, "black"], [1, "#ffffff"]]}}),
                "not a color",
            ),
        ];
        for (fill, expected) in cases {
            let error = wrong(fill);
            assert!(error.contains(expected), "{expected}: {error}");
        }
    }

    fn filter(server: &PaintServer, params: rmcp::serde_json::Value) -> Result<String, String> {
        let mut params = params;
        params["image"] = 1.into();
        server.filter(Parameters(rmcp::serde_json::from_value(params).unwrap()))
    }

    #[test]
    fn blurs_inside_a_clip_and_undoes_it() {
        let server = server();
        let left = ShapeSpec {
            fill: Some("#000000".into()),
            ..shape(ShapeName::Rectangle, &[[0.0, 0.0], [50.0, 100.0]])
        };
        draw_shape(&server, left).unwrap();
        let clip = rmcp::serde_json::json!({"shape": "rectangle", "points": [[0, 0], [100, 50]]});
        filter(
            &server,
            rmcp::serde_json::json!({"filter": "blur", "radius": 6, "clip": clip}),
        )
        .unwrap();
        let (softened, sharp) = (rgb_at(&server, 50, 20), rgb_at(&server, 50, 80));
        assert!((40..220).contains(&softened.0), "{softened:?}");
        assert_eq!(sharp, (255, 255, 255));
        server.undo(image(1)).unwrap();
        assert_eq!(rgb_at(&server, 50, 20), (255, 255, 255));
    }

    #[test]
    fn grain_is_the_same_for_the_same_seed() {
        let grained = |seed: u64| {
            let server = server();
            let params = rmcp::serde_json::json!({"filter": "noise", "amount": 20, "seed": seed});
            filter(&server, params).unwrap();
            (0..100).map(|x| rgb_at(&server, x, 10)).collect::<Vec<_>>()
        };
        assert_eq!(grained(1), grained(1));
        assert_ne!(grained(1), grained(2));
        assert!(grained(1).iter().any(|&color| color != (255, 255, 255)));
    }

    #[test]
    fn refuses_options_that_do_not_fit_the_filter() {
        let server = server();
        let wrong = [
            (
                rmcp::serde_json::json!({"filter": "blur", "amount": 10}),
                "amount, color and seed are for noise",
            ),
            (
                rmcp::serde_json::json!({"filter": "noise", "radius": 2}),
                "radius is for blur and painterly",
            ),
            (
                rmcp::serde_json::json!({"filter": "noise", "amount": 150}),
                "amount must be between 0 and 100, not 150",
            ),
            (
                rmcp::serde_json::json!({"filter": "painterly", "radius": 40}),
                "radius must be between 1 and 16, not 40",
            ),
            (
                rmcp::serde_json::json!({"filter": "blur", "clip": [[200, 200], [300, 300]]}),
                "nothing was changed, as it lies entirely outside the 100×100 image",
            ),
        ];
        for (params, error) in wrong {
            assert_eq!(filter(&server, params).unwrap_err(), error);
        }
    }

    fn adjust(server: &PaintServer, params: rmcp::serde_json::Value) -> Result<String, String> {
        let mut params = params;
        params["image"] = 1.into();
        server.adjust(Parameters(rmcp::serde_json::from_value(params).unwrap()))
    }

    #[test]
    fn adjusts_colors_inside_a_mask_and_undoes_it() {
        let server = server();
        let red = ShapeSpec {
            fill: Some("#c83232".into()),
            ..shape(ShapeName::Rectangle, &[[0.0, 0.0], [100.0, 100.0]])
        };
        draw_shape(&server, red).unwrap();
        let clip = rmcp::serde_json::json!({"shape": "rectangle", "points": [[0, 0], [100, 50]]});
        adjust(
            &server,
            rmcp::serde_json::json!({"saturation": -100, "clip": clip}),
        )
        .unwrap();
        let gray = rgb_at(&server, 50, 20);
        assert!(gray.0 == gray.1 && gray.1 == gray.2, "{gray:?}");
        assert_eq!(rgb_at(&server, 50, 80), (200, 50, 50));
        server.undo(image(1)).unwrap();
        assert_eq!(rgb_at(&server, 50, 20), (200, 50, 50));
        adjust(&server, rmcp::serde_json::json!({"warmth": -100})).unwrap();
        let cool = rgb_at(&server, 50, 80);
        assert!(cool.0 < 200 && cool.2 > 50, "{cool:?}");
    }

    #[test]
    fn refuses_adjustments_out_of_range_or_missing() {
        let server = server();
        let wrong = [
            (
                rmcp::serde_json::json!({}),
                "give at least one of hue, saturation, brightness, contrast and warmth",
            ),
            (
                rmcp::serde_json::json!({"hue": 200}),
                "hue must be between -180 and 180, not 200",
            ),
            (
                rmcp::serde_json::json!({"contrast": -150}),
                "contrast must be between -100 and 100, not -150",
            ),
            (
                rmcp::serde_json::json!({"brightness": 10, "clip": [[200, 200], [300, 300]]}),
                "nothing was changed, as it lies entirely outside the 100×100 image",
            ),
        ];
        for (params, error) in wrong {
            assert_eq!(adjust(&server, params).unwrap_err(), error);
        }
    }

    /// How many pixels down column `x` are darker than light gray.
    fn thickness(server: &PaintServer, x: u32) -> usize {
        (0..100).filter(|&y| rgb_at(server, x, y).0 < 200).count()
    }

    #[test]
    fn tapers_and_presses_strokes() {
        for brush in ["pen", "oil", "calligraphy", "airbrush"] {
            let server = server();
            let line = rmcp::serde_json::json!([
                {"type": "stroke", "points": [[10, 50], [90, 50]], "color": "#000000", "width": 20,
                 "brush": brush, "taper": [40, 0]},
            ]);
            draw(&server, line).unwrap();
            let (start, middle) = (thickness(&server, 14), thickness(&server, 70));
            assert!(
                start * 2 < middle,
                "{brush}: {start} at the start, {middle} later"
            );
        }
        let server = server();
        let swelling = rmcp::serde_json::json!([
            {"type": "stroke", "points": [[10, 50], [50, 50], [90, 50]], "color": "#000000",
             "width": 20, "pressure": [0.2, 1, 0.2], "smooth": true},
        ]);
        draw(&server, swelling).unwrap();
        let (end, middle) = (thickness(&server, 15), thickness(&server, 50));
        assert!(
            end * 2 < middle,
            "{end} near the end, {middle} in the middle"
        );
    }

    #[test]
    fn refuses_wrong_pressure_and_taper() {
        let server = server();
        let wrong = [
            (
                rmcp::serde_json::json!({"pressure": [1]}),
                "item 1: pressure takes one number for each of the 2 points, not 1; nothing was drawn",
            ),
            (
                rmcp::serde_json::json!({"pressure": [1, 2]}),
                "item 1: pressure must be between 0 and 1; nothing was drawn",
            ),
            (
                rmcp::serde_json::json!({"taper": -1}),
                "item 1: taper cannot be negative; nothing was drawn",
            ),
        ];
        for (options, error) in wrong {
            let mut item = rmcp::serde_json::json!({"type": "stroke", "points": [[10, 50], [90, 50]], "color": "#000000"});
            item.as_object_mut()
                .unwrap()
                .extend(options.as_object().unwrap().clone());
            assert_eq!(
                draw(&server, rmcp::serde_json::json!([item])).unwrap_err(),
                error
            );
        }
        let params = rmcp::serde_json::json!({"image": 1, "items": [
            {"type": "stroke", "points": [[10, 50], [90, 50]], "color": "#000000", "taper": "long"},
        ]});
        let error = rmcp::serde_json::from_value::<DrawParams>(params)
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("taper is a number of pixels or [start, end]"),
            "{error}"
        );
    }

    #[test]
    fn roughens_outlines_and_lines() {
        let server = server();
        let rough = rmcp::serde_json::json!([
            {"type": "shape", "shape": "rectangle", "points": [[20, 20], [80, 80]], "fill": "#000000",
             "outline": "#000000", "width": 2, "roughen": 4},
        ]);
        draw(&server, rough).unwrap();
        // The top edge wanders above and below where it was
        let top = |x: u32| (0..50).find(|&y| rgb_at(&server, x, y).0 < 128).unwrap();
        let tops: Vec<u32> = (30..70).map(top).collect();
        let (highest, lowest) = (*tops.iter().min().unwrap(), *tops.iter().max().unwrap());
        assert!(
            highest < 18 && lowest > 19,
            "the top edge runs from {highest} to {lowest}"
        );
        assert!(lowest - highest <= 10, "{highest} to {lowest}");
        assert_eq!(rgb_at(&server, 50, 50), (0, 0, 0));

        let error = draw(
            &server,
            rmcp::serde_json::json!([
                {"type": "stroke", "points": [[10, 10], [90, 90]], "color": "#000000", "roughen": 20},
            ]),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "item 1: roughen must be between 0 and 16, not 20; nothing was drawn"
        );
    }

    #[test]
    fn jitters_colors_from_place_to_place() {
        let server = server();
        let uneven = rmcp::serde_json::json!([
            {"type": "shape", "shape": "rectangle", "points": [[0, 0], [100, 100]], "fill": "#808080",
             "jitter": 20},
        ]);
        draw(&server, uneven).unwrap();
        let reds: Vec<u8> = (0..100).map(|x| rgb_at(&server, x, 50).0).collect();
        let (low, high) = (*reds.iter().min().unwrap(), *reds.iter().max().unwrap());
        assert!(low < 115 && high > 141, "from {low} to {high}");
        assert!(low >= 77 && high <= 179, "from {low} to {high}");
        // Blotches, not speckles: neighbors are close
        assert!(
            reds.windows(2).all(|pair| pair[0].abs_diff(pair[1]) <= 8),
            "{reds:?}"
        );

        let error = draw(
            &server,
            rmcp::serde_json::json!([
                {"type": "stroke", "points": [[10, 10], [90, 90]], "color": "#000000", "jitter": 120},
            ]),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "item 1: jitter must be between 0 and 100, not 120; nothing was drawn"
        );
    }

    #[test]
    fn smudges_what_was_drawn_before_it_in_the_same_call() {
        let server = server();
        let smeared = rmcp::serde_json::json!([
            {"type": "shape", "shape": "rectangle", "points": [[0, 0], [50, 100]], "fill": "#000000"},
            {"type": "stroke", "points": [[35, 50], [85, 50]], "width": 20, "brush": "smudge"},
        ]);
        draw(&server, smeared).unwrap();
        let (dragged, far) = (rgb_at(&server, 60, 50), rgb_at(&server, 84, 50));
        assert!(
            dragged.0 < 160,
            "black is dragged into the white: {dragged:?}"
        );
        assert!(far.0 > dragged.0, "and fades along the stroke: {far:?}");
        assert_eq!(rgb_at(&server, 60, 20), (255, 255, 255));
        assert_eq!(rgb_at(&server, 20, 20), (0, 0, 0));

        let wrong = [
            (
                rmcp::serde_json::json!({"type": "stroke", "points": [[0, 0], [9, 9]], "brush": "smudge", "color": "#000000"}),
                "item 1: smudge drags the colors already on the image, so it takes no color; nothing was drawn",
            ),
            (
                rmcp::serde_json::json!({"type": "stroke", "points": [[0, 0], [9, 9]]}),
                "item 1: give the line a color; nothing was drawn",
            ),
        ];
        for (item, error) in wrong {
            assert_eq!(
                draw(&server, rmcp::serde_json::json!([item])).unwrap_err(),
                error
            );
        }
    }

    fn hatch(server: &PaintServer, params: rmcp::serde_json::Value) -> Result<String, String> {
        let mut params = params;
        params["image"] = 1.into();
        server.hatch(Parameters(rmcp::serde_json::from_value(params).unwrap()))
    }

    /// How many of the pixels from (`x0`, `y0`) to (`x1`, `y1`) are not white.
    fn painted(server: &PaintServer, (x0, y0): (u32, u32), (x1, y1): (u32, u32)) -> usize {
        (y0..y1)
            .flat_map(|y| (x0..x1).map(move |x| (x, y)))
            .filter(|&(x, y)| rgb_at(server, x, y) != (255, 255, 255))
            .count()
    }

    #[test]
    fn hatches_inside_the_clip_the_same_for_the_same_seed() {
        let params = rmcp::serde_json::json!({
            "colors": ["#000000", "#404040"], "clip": [[20, 20], [80, 80]], "seed": 3,
        });
        let first = server();
        let laid = hatch(&first, params.clone()).unwrap();
        assert!(laid.starts_with("Laid "), "{laid}");
        assert!(painted(&first, (20, 20), (80, 80)) > 1800);
        assert_eq!(painted(&first, (0, 0), (100, 19)), 0);
        assert_eq!(painted(&first, (81, 0), (100, 100)), 0);
        let second = server();
        hatch(&second, params).unwrap();
        let pixels = |server: &PaintServer| server.images().get(1).unwrap().canvas.pixels().clone();
        assert!(pixels(&first) == pixels(&second));
    }

    #[test]
    fn hatching_gathers_away_from_the_light() {
        let server = server();
        let params = rmcp::serde_json::json!({
            "colors": ["#000000"], "clip": [[0, 0], [100, 100]], "width": [1, 2],
            "length": [6, 10], "count": 400, "light": [0, 50], "seed": 1,
        });
        hatch(&server, params).unwrap();
        let (lit, shaded) = (
            painted(&server, (0, 0), (30, 100)),
            painted(&server, (70, 0), (100, 100)),
        );
        assert!(lit * 2 < shaded, "{lit} lit, {shaded} shaded");
    }

    #[test]
    fn hatches_in_draw_with_a_named_mask() {
        let server = server();
        let params = rmcp::serde_json::json!({"image": 1, "items": [
            {"type": "hatch", "colors": ["#000000"], "clip": {"mask": "ball"}, "brush": "oil", "width": [4, 8],
             "taper": 4, "jitter": 10},
        ], "masks": {"ball": {"shape": "ellipse", "points": [[30, 30], [70, 70]]}}});
        server
            .draw(Parameters(rmcp::serde_json::from_value(params).unwrap()))
            .unwrap();
        assert!(painted(&server, (40, 40), (60, 60)) > 200);
        assert_eq!(painted(&server, (0, 0), (25, 25)), 0);
    }

    #[test]
    fn refuses_wrong_hatching() {
        let server = server();
        let wrong = [
            (
                rmcp::serde_json::json!({"colors": ["#000000"]}),
                "hatch takes a clip: the rectangle or mask to lay strokes in",
            ),
            (
                rmcp::serde_json::json!({"colors": [], "clip": [[0, 0], [9, 9]]}),
                "give one or more colors",
            ),
            (
                rmcp::serde_json::json!({"colors": ["#000000"], "clip": [[0, 0], [9, 9]], "brush": "smudge"}),
                "hatch lays down color, so it cannot use the smudge brush",
            ),
            (
                rmcp::serde_json::json!({"colors": ["#000000"], "clip": [[0, 0], [9, 9]], "length": [40, 20]}),
                "length must be [shortest, longest] from 1 to 500, not [40, 20]",
            ),
            (
                rmcp::serde_json::json!({"colors": ["#000000"], "clip": [[200, 200], [300, 300]]}),
                "the area lies entirely outside the 100×100 image",
            ),
            (
                rmcp::serde_json::json!({"colors": ["#000000"], "clip": [[0, 0], [9, 9]], "count": 0}),
                "count must be between 1 and 5000",
            ),
        ];
        for (params, error) in wrong {
            assert_eq!(hatch(&server, params).unwrap_err(), error);
        }
    }

    #[test]
    fn keeps_shading_inside_and_outside_a_shape() {
        let server = server();
        draw(
            &server,
            rmcp::serde_json::json!([
                {"type": "shape", "shape": "rectangle", "points": [[0, 0], [100, 100]], "fill": "#000000",
                 "clip": {"shape": "ellipse", "points": [[20, 20], [80, 80]]}},
                {"type": "shape", "shape": "rectangle", "points": [[0, 0], [100, 100]], "fill": "#ff0000",
                 "clip": {"shape": "ellipse", "points": [[20, 20], [80, 80]], "outside": true}},
            ]),
        )
        .unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (0, 0, 0));
        assert_eq!(rgb_at(&server, 22, 22), (255, 0, 0));
        assert_eq!(rgb_at(&server, 50, 5), (255, 0, 0));
    }

    #[test]
    fn region_masks_shade_what_is_already_drawn() {
        let server = server();
        let face = ShapeSpec {
            fill: Some("#f0c8a0".into()),
            ..shape(ShapeName::Ellipse, &[[20.0, 10.0], [80.0, 90.0]])
        };
        draw_shape(&server, face).unwrap();
        // A wide shadow across the bottom stays on the face
        let shadow = rmcp::serde_json::json!([
            {"type": "shape", "shape": "rectangle", "points": [[0, 60], [100, 100]], "fill": "#80604080",
             "blend": "multiply", "clip": {"region": [50, 50], "tolerance": 10}},
        ]);
        draw(&server, shadow).unwrap();
        let (lit, shaded) = (rgb_at(&server, 50, 40), rgb_at(&server, 50, 80));
        assert_eq!(lit, (240, 200, 160));
        assert!(shaded.0 < 220, "{shaded:?}");
        assert_eq!(rgb_at(&server, 5, 80), (255, 255, 255));
    }

    #[test]
    fn named_masks_serve_many_items() {
        let server = server();
        let params = rmcp::serde_json::json!({
            "image": 1,
            "masks": {"top": {"shape": "rectangle", "points": [[0, 0], [100, 50]]}},
            "items": [
                {"type": "stroke", "points": [[20, 0], [20, 100]], "color": "#000000", "width": 6,
                 "clip": {"mask": "top"}},
                {"type": "stroke", "points": [[80, 0], [80, 100]], "color": "#0000ff", "width": 6,
                 "clip": {"mask": "top", "outside": true}},
            ],
        });
        server
            .draw(Parameters(rmcp::serde_json::from_value(params).unwrap()))
            .unwrap();
        assert_eq!(rgb_at(&server, 20, 30), (0, 0, 0));
        assert_eq!(rgb_at(&server, 20, 70), (255, 255, 255));
        assert_eq!(rgb_at(&server, 80, 30), (255, 255, 255));
        assert_eq!(rgb_at(&server, 80, 70), (0, 0, 255));
    }

    #[test]
    fn cuts_a_crescent_moon_out_of_a_circle() {
        let server = server();
        draw(
            &server,
            rmcp::serde_json::json!([
                {"type": "shape", "shape": "rectangle", "points": [[0, 0], [100, 100]], "fill": "#ffff00",
                 "clip": {"all": [
                     {"shape": "ellipse", "points": [[10, 10], [90, 90]]},
                     {"shape": "ellipse", "points": [[30, 0], [110, 80]], "outside": true},
                 ]}},
            ]),
        )
        .unwrap();
        assert_eq!(rgb_at(&server, 20, 60), (255, 255, 0));
        assert_eq!(rgb_at(&server, 60, 40), (255, 255, 255));
        assert_eq!(rgb_at(&server, 5, 5), (255, 255, 255));
    }

    #[test]
    fn joins_masks_and_nests_them_with_named_masks() {
        let server = server();
        let params = rmcp::serde_json::json!({
            "image": 1,
            "masks": {"cloud": {"any": [
                {"shape": "ellipse", "points": [[10, 30], [50, 60]]},
                {"shape": "ellipse", "points": [[40, 20], [80, 60]]},
            ]}},
            "items": [
                {"type": "shape", "shape": "rectangle", "points": [[0, 0], [100, 100]], "fill": "#0000ff",
                 "clip": {"mask": "cloud"}},
                // The left half of the cloud only
                {"type": "shape", "shape": "rectangle", "points": [[0, 0], [100, 100]], "fill": "#ff0000",
                 "clip": {"all": [{"mask": "cloud"}, [[0, 0], [45, 100]]]}},
            ],
        });
        server
            .draw(Parameters(rmcp::serde_json::from_value(params).unwrap()))
            .unwrap();
        assert_eq!(rgb_at(&server, 20, 45), (255, 0, 0));
        assert_eq!(rgb_at(&server, 60, 40), (0, 0, 255));
        assert_eq!(rgb_at(&server, 60, 80), (255, 255, 255));
        assert_eq!(rgb_at(&server, 20, 80), (255, 255, 255));
    }

    #[test]
    fn explains_which_mask_in_a_combination_is_wrong() {
        let server = server();
        let wrong = |masks: rmcp::serde_json::Value, clip: rmcp::serde_json::Value| {
            let params = rmcp::serde_json::json!({
                "image": 1,
                "masks": masks,
                "items": [{"type": "stroke", "points": [[10, 10], [90, 90]], "color": "#000000", "clip": clip}],
            });
            match rmcp::serde_json::from_value::<DrawParams>(params) {
                Err(error) => error.to_string(),
                Ok(params) => server.draw(Parameters(params)).unwrap_err(),
            }
        };
        let none = rmcp::serde_json::json!({});
        let ellipse = rmcp::serde_json::json!({"shape": "ellipse", "points": [[0, 0], [20, 20]]});
        let cases = [
            (
                none.clone(),
                rmcp::serde_json::json!({"all": []}),
                "one or more",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({"any": [ellipse, {"shape": "ellipse"}]}),
                "mask 2 in any: a shape mask takes points",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({"all": [{"any": [ellipse, {"region": [500, 0]}]}]}),
                "mask 1 in all: mask 2 in any: (500, 0) is outside",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({"all": [ellipse, {"shape": "ellipse", "points": [[60, 60], [90, 90]]}]}),
                "no area in common",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({"all": [ellipse], "points": [[0, 0], [1, 1]]}),
                "nothing but all",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({"all": [[[200, 200], [300, 300]]]}),
                "rectangle lies entirely outside",
            ),
            (
                rmcp::serde_json::json!({"a": {"shape": "rectangle", "points": [[0, 0], [9, 9]]},
                                         "b": {"any": [{"mask": "a"}]}}),
                rmcp::serde_json::json!({"mask": "b"}),
                "mask \"b\": mask 1 in any: draw's named masks cannot use other named masks",
            ),
        ];
        for (masks, clip, expected) in cases {
            let error = wrong(masks, clip.clone());
            assert!(error.contains(expected), "{clip}: {expected}: {error}");
        }
        assert!(server.undo(image(1)).is_err());
    }

    #[test]
    fn explains_wrong_masks_and_draws_nothing() {
        let server = server();
        let wrong = |masks: rmcp::serde_json::Value, clip: rmcp::serde_json::Value| {
            let params = rmcp::serde_json::json!({
                "image": 1,
                "masks": masks,
                "items": [{"type": "stroke", "points": [[10, 10], [90, 90]], "color": "#000000", "clip": clip}],
            });
            match rmcp::serde_json::from_value::<DrawParams>(params) {
                Err(error) => error.to_string(),
                Ok(params) => server.draw(Parameters(params)).unwrap_err(),
            }
        };
        let none = rmcp::serde_json::json!({});
        let cases = [
            (
                none.clone(),
                rmcp::serde_json::json!({"mask": "face"}),
                "no mask named",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({}),
                "one of shape, region, mask, all or any",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({"shape": "ellipse", "region": [5, 5]}),
                "one of shape, region, mask, all or any",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({"shape": "ellipse"}),
                "takes points",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({"shape": "arc", "points": [[0, 0], [9, 9]], "start_angle": 0, "end_angle": 90}),
                "no inside",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({"shape": "ellipse", "points": [[200, 200], [300, 300]]}),
                "outside the 100×100 image",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({"region": [500, 5]}),
                "outside",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({"region": [5, 5], "points": [[0, 0], [1, 1]]}),
                "for shape masks",
            ),
            (
                none.clone(),
                rmcp::serde_json::json!({"region": [5, 5], "tolerance": 200}),
                "tolerance",
            ),
            (none.clone(), rmcp::serde_json::json!("everywhere"), "clip"),
            (
                rmcp::serde_json::json!({"a": {"mask": "b"}}),
                rmcp::serde_json::json!({"mask": "a"}),
                "mask \"a\": draw's named masks cannot use other named masks",
            ),
        ];
        for (masks, clip, expected) in cases {
            let error = wrong(masks, clip.clone());
            assert!(error.contains(expected), "{clip}: {expected}: {error}");
        }
        assert_eq!(rgb_at(&server, 50, 50), (255, 255, 255));
        assert!(server.undo(image(1)).is_err());
    }

    #[test]
    fn draws_a_stroke_and_undoes_it() {
        let server = server();
        draw_stroke(&server, stroke(&[[10.0, 50.0], [90.0, 50.0]])).unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (0, 0, 0));
        server.undo(Parameters(ImageParams { image: 1 })).unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (255, 255, 255));
        server.redo(Parameters(ImageParams { image: 1 })).unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (0, 0, 0));
    }

    #[test]
    fn a_single_point_leaves_a_dot() {
        let server = server();
        draw_stroke(&server, stroke(&[[30.0, 30.0]])).unwrap();
        assert_eq!(rgb_at(&server, 30, 30), (0, 0, 0));
    }

    #[test]
    fn refuses_bad_strokes() {
        let server = server();
        assert!(draw_stroke(&server, stroke(&[])).is_err());
        let wide = StrokeSpec {
            width: Some(33.0),
            ..stroke(&[[10.0, 10.0]])
        };
        assert!(draw_stroke(&server, wide).unwrap_err().contains("32"));
        let outside = stroke(&[[500.0, 500.0], [600.0, 500.0]]);
        let error = draw_stroke(&server, outside).unwrap_err();
        assert!(error.contains("outside"), "{error}");
        let missing = DrawStrokeParams {
            image: 7,
            stroke: stroke(&[[10.0, 10.0]]),
        };
        assert!(server.draw_stroke(Parameters(missing)).is_err());
    }

    #[test]
    fn fills_and_outlines_a_rectangle_as_one_step() {
        let server = server();
        let params = ShapeSpec {
            outline: Some("#0000ff".into()),
            fill: Some("#ff0000".into()),
            width: Some(6.0),
            ..shape(ShapeName::Rectangle, &[[80.0, 80.0], [20.0, 20.0]])
        };
        draw_shape(&server, params).unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (255, 0, 0));
        assert_eq!(rgb_at(&server, 50, 20), (0, 0, 255));
        assert_eq!(rgb_at(&server, 50, 10), (255, 255, 255));
        server.undo(Parameters(ImageParams { image: 1 })).unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (255, 255, 255));
        assert_eq!(rgb_at(&server, 50, 20), (255, 255, 255));
    }

    #[test]
    fn fills_an_ellipse_without_an_outline() {
        let server = server();
        let params = ShapeSpec {
            fill: Some("#00ff00".into()),
            ..shape(ShapeName::Ellipse, &[[10.0, 30.0], [90.0, 70.0]])
        };
        draw_shape(&server, params).unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (0, 255, 0));
        // Inside the box but outside the ellipse
        assert_eq!(rgb_at(&server, 12, 32), (255, 255, 255));
    }

    #[test]
    fn outlines_a_polygon() {
        let server = server();
        let params = ShapeSpec {
            outline: Some("#000000".into()),
            ..shape(
                ShapeName::Polygon,
                &[[10.0, 90.0], [50.0, 10.0], [90.0, 90.0]],
            )
        };
        draw_shape(&server, params).unwrap();
        // The closing edge along the bottom, and the empty inside
        assert_eq!(rgb_at(&server, 50, 90), (0, 0, 0));
        assert_eq!(rgb_at(&server, 50, 60), (255, 255, 255));
    }

    #[test]
    fn refuses_shapes_with_the_wrong_points_or_colors() {
        let server = server();
        let black = || Some("#000000".to_string());
        let cases = [
            ShapeSpec {
                outline: black(),
                ..shape(ShapeName::Line, &[[0.0, 0.0]])
            },
            ShapeSpec {
                outline: black(),
                ..shape(ShapeName::Rectangle, &[[0.0, 0.0], [5.0, 5.0], [9.0, 9.0]])
            },
            ShapeSpec {
                outline: black(),
                ..shape(ShapeName::Polygon, &[[0.0, 0.0], [5.0, 5.0]])
            },
            shape(ShapeName::Rectangle, &[[0.0, 0.0], [5.0, 5.0]]),
            ShapeSpec {
                fill: Some("#000000".into()),
                ..shape(ShapeName::Line, &[[0.0, 0.0], [5.0, 5.0]])
            },
            ShapeSpec {
                outline: Some("black".into()),
                ..shape(ShapeName::Rectangle, &[[0.0, 0.0], [5.0, 5.0]])
            },
        ];
        for params in cases {
            assert!(draw_shape(&server, params).is_err());
        }
        assert_eq!(rgb_at(&server, 0, 0), (255, 255, 255));
        assert!(server.undo(Parameters(ImageParams { image: 1 })).is_err());
    }

    #[test]
    fn parses_colors_with_and_without_alpha() {
        let red: Hsla = rgba(0xff0000ff).into();
        assert_eq!(parse_color("#ff0000").unwrap(), red);
        assert_eq!(parse_color("#FF0000").unwrap(), red);
        assert_eq!(parse_color("#ff000080").unwrap(), rgba(0xff000080).into());
    }

    #[test]
    fn refuses_what_is_not_a_color() {
        for text in [
            "",
            "#",
            "#fff",
            "#ff00",
            "#gg0000",
            "#+f0000",
            "red",
            "#ff0000ff00",
        ] {
            assert!(parse_color(text).is_err(), "{text:?}");
        }
    }
}
