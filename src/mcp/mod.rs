use anyhow::{Result, bail};
use base64::Engine as _;
use gpui_kit::{Hsla, Pixels, Point, point, px, rgba};
use images::Images;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::JsonSchema;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::canvas::{Brush, DEFAULT_FILL_TOLERANCE, Fill, Orientation, PixelRect};
use crate::pattern::{PATTERNS, Pattern};
use crate::stroke::{ArcKind, Gradient, Stroke, smooth};
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
at once with draw; fill regions with fill and write with draw_text. Coordinates are in image \
pixels from the top-left. Colors are #rrggbb or #rrggbbaa. Call view_image to look at the result, and save_image to write \
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
    /// Two opposite corners of a rectangle, as [x, y] pairs, to draw inside of. The
    /// rest of the image is left as it is.
    clip: Option<[[i32; 2]; 2]>,
}

impl Placement {
    fn apply(&self, strokes: &mut [Stroke]) -> Result<()> {
        let clip = self.clip.map(to_rect).transpose()?;
        for stroke in strokes {
            stroke.blend = self.blend.map_or(BlendMode::SourceOver, BlendMode::from);
            stroke.clip = clip;
        }
        Ok(())
    }
}

/// A line through points drawn with a brush.
#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct StrokeSpec {
    /// The points the line passes through, in order, as [x, y] pairs. A single point
    /// leaves a dot.
    points: Vec<[f32; 2]>,
    /// The line's color as #rrggbb or #rrggbbaa.
    color: String,
    /// Width in pixels, from 1 to 32. 4 when left out.
    width: Option<f32>,
    /// The brush to draw with. The pen when left out.
    brush: Option<BrushName>,
    /// Draw a smooth curve through the points instead of straight lines between them.
    smooth: Option<bool>,
    #[serde(flatten)]
    placement: Placement,
}

impl StrokeSpec {
    fn build(&self, seed: u64) -> Result<Stroke> {
        if self.points.is_empty() {
            bail!("give at least one point");
        }
        let mut points = to_points(&self.points);
        if self.smooth == Some(true) {
            points = smooth(&points, false);
        }
        let mut stroke = Stroke {
            points,
            color: parse_color(&self.color)?,
            width: parse_width(self.width)?,
            brush: self.brush.map_or(Brush::Pen, Brush::from),
            seed,
            ..Default::default()
        };
        self.placement.apply(std::slice::from_mut(&mut stroke))?;
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

impl ShapeSpec {
    /// The fill, if any, then the outline, if any.
    fn build(&self) -> Result<Vec<Stroke>> {
        let name = self.shape;
        let points = to_points(&self.points);
        let two = points.len() == 2;
        let (takes, fits) = match name {
            ShapeName::Line => ("a line takes its two ends", two),
            ShapeName::Rectangle => ("a rectangle takes two opposite corners", two),
            ShapeName::Ellipse => ("an ellipse takes two opposite corners of its box", two),
            ShapeName::Arc | ShapeName::Pie | ShapeName::Chord => (
                "an arc, pie or chord takes two opposite corners of its ellipse's box",
                two,
            ),
            ShapeName::Polygon => ("a polygon takes three or more corners", points.len() >= 3),
        };
        if !fits {
            let count = points.len();
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
        let angles = match (self.start_angle, self.end_angle) {
            (Some(from), Some(to)) if arc => Some((from, to)),
            (None, None) if !arc => None,
            _ if arc => bail!("{} takes both start_angle and end_angle", name.label()),
            _ => bail!("start_angle and end_angle are for arcs, pies and chords"),
        };
        if self.radius.is_some_and(|radius| radius < 0.0) {
            bail!("radius cannot be negative");
        }
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

        let shape = |base: Stroke| match (name, angles) {
            (ShapeName::Polygon, _) if self.smooth == Some(true) => Stroke {
                // Curves join smoothly
                sharp_corners: false,
                ..Stroke::polygon(smooth(&points, true), true, base)
            },
            (ShapeName::Polygon, _) => Stroke::polygon(points.clone(), true, base),
            (ShapeName::Rectangle, _) => match self.radius {
                Some(radius) => Stroke::rounded_rectangle(points[0], points[1], radius, base),
                None => Stroke::shape(ShapeKind::Rectangle, points[0], points[1], base),
            },
            (ShapeName::Line, _) => Stroke::shape(ShapeKind::Line, points[0], points[1], base),
            (ShapeName::Arc | ShapeName::Pie | ShapeName::Chord, Some((from, to))) => {
                let kind = match name {
                    ShapeName::Arc => ArcKind::Arc,
                    ShapeName::Pie => ArcKind::Pie,
                    _ => ArcKind::Chord,
                };
                Stroke::arc(points[0], points[1], from, to, kind, base)
            }
            _ => Stroke::shape(ShapeKind::Ellipse, points[0], points[1], base),
        };
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
        self.placement.apply(&mut strokes)?;
        Ok(strokes)
    }
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

/// One thing for draw to draw: a stroke as draw_stroke takes it, or a shape
/// as draw_shape takes it.
#[derive(Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum DrawItem {
    Stroke(StrokeSpec),
    Shape(ShapeSpec),
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DrawParams {
    /// The image's id.
    image: u32,
    /// What to draw, in order, later items over earlier ones. Each is
    /// {"type": "stroke", ...} with draw_stroke's arguments or {"type": "shape", ...}
    /// with draw_shape's, without the image.
    items: Vec<DrawItem>,
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
        description = "Look at an image. It comes back as PNG, scaled down to fit 1024×1024 when \
        it is larger.",
        annotations(read_only_hint = true)
    )]
    fn view_image(
        &self,
        Parameters(params): Parameters<ImageParams>,
    ) -> Result<CallToolResult, String> {
        let images = self.images();
        let size = images.get(params.image).map_err(message)?.size();
        let (png, shown) = images.view(params.image).map_err(message)?;
        let mut description = format!("Image {}, {}×{}", params.image, size.width, size.height);
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
        let stroke = params.stroke.build(image.next_seed()).map_err(message)?;
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
        let strokes = params.shape.build().map_err(message)?;
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
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
        let mut strokes = Vec::new();
        for (index, item) in params.items.iter().enumerate() {
            let built = match item {
                DrawItem::Stroke(stroke) => stroke.build(image.next_seed()).map(|s| vec![s]),
                DrawItem::Shape(shape) => shape.build(),
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
        let percent = params
            .tolerance
            .unwrap_or(tolerance_percent(DEFAULT_FILL_TOLERANCE));
        if !(0.0..=100.0).contains(&percent) {
            return Err(format!(
                "tolerance must be between 0 and 100, not {percent}"
            ));
        }
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
            .flood_fill(point(px(x), px(y)), fill, tolerance_from_percent(percent))
            .ok_or("nothing changed, as the region already has that color")?;
        image.history.record(undo);
        Ok(format!(
            "Filled around ({x}, {y}) on image {}",
            params.image
        ))
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
        }
    }

    fn draw_shape(server: &PaintServer, shape: ShapeSpec) -> Result<String, String> {
        server.draw_shape(Parameters(DrawShapeParams { image: 1, shape }))
    }

    fn stroke(points: &[[f32; 2]]) -> StrokeSpec {
        StrokeSpec {
            points: points.to_vec(),
            color: "#000000".into(),
            width: None,
            brush: None,
            smooth: None,
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
                clip: Some([[0, 0], [100, 50]]),
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
                clip: Some([[0, 60], [10, 70]]),
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
