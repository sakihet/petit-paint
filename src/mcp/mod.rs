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

use crate::canvas::Brush;
use crate::stroke::Stroke;
use crate::tool::ShapeKind;
use crate::{DEFAULT_WIDTH, MAX_WIDTH, MIN_WIDTH};

mod images;

const INSTRUCTIONS: &str = "Draws raster images with petit-paint, a small paint app. \
Create an image with new_image or open a file with open_image; each gets an id that the \
other tools take. Draw on it with draw_stroke and draw_shape. Coordinates are in image \
pixels from the top-left. Colors are #rrggbb or #rrggbbaa. Call view_image to look at the result, and save_image to write \
it to a file. Relative paths are taken from the server's working directory.";

/// Draws images for MCP clients such as coding agents, without a window.
#[derive(Clone, Default)]
pub struct PaintServer {
    images: Arc<Mutex<Images>>,
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

#[derive(Deserialize, JsonSchema)]
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

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum ShapeName {
    Line,
    Rectangle,
    Ellipse,
    Polygon,
}

impl From<ShapeName> for ShapeKind {
    fn from(name: ShapeName) -> Self {
        match name {
            ShapeName::Line => ShapeKind::Line,
            ShapeName::Rectangle => ShapeKind::Rectangle,
            ShapeName::Ellipse => ShapeKind::Ellipse,
            ShapeName::Polygon => ShapeKind::Polygon,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DrawStrokeParams {
    /// The image's id.
    image: u32,
    /// The points the line passes through, in order, as [x, y] pairs. A single point
    /// leaves a dot.
    points: Vec<[f32; 2]>,
    /// The line's color as #rrggbb or #rrggbbaa.
    color: String,
    /// Width in pixels, from 1 to 32. 4 when left out.
    width: Option<f32>,
    /// The brush to draw with. The pen when left out.
    brush: Option<BrushName>,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct DrawShapeParams {
    /// The image's id.
    image: u32,
    shape: ShapeName,
    /// As [x, y] pairs: a line's two ends, two opposite corners of a rectangle or of
    /// the box around an ellipse, or a polygon's three or more corners in order.
    points: Vec<[f32; 2]>,
    /// The outline's color as #rrggbb or #rrggbbaa. No outline when left out.
    outline: Option<String>,
    /// The color to fill the inside with, as #rrggbb or #rrggbbaa. Not filled when
    /// left out. A line has no inside.
    fill: Option<String>,
    /// The outline's width in pixels, from 1 to 32. 4 when left out.
    width: Option<f32>,
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
        if params.points.is_empty() {
            return Err("give at least one point".into());
        }
        let color = parse_color(&params.color).map_err(message)?;
        let width = parse_width(params.width).map_err(message)?;
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        let stroke = Stroke {
            points: to_points(&params.points),
            color,
            width,
            brush: params.brush.map_or(Brush::Pen, Brush::from),
            seed: image.next_seed(),
            ..Default::default()
        };
        image.draw(&[stroke]).map_err(message)?;
        Ok(format!("Drew a stroke on image {}", params.image))
    }

    #[tool(
        description = "Draw a line, rectangle, ellipse or polygon, with an outline, a fill or \
        both."
    )]
    fn draw_shape(
        &self,
        Parameters(params): Parameters<DrawShapeParams>,
    ) -> Result<String, String> {
        let kind = ShapeKind::from(params.shape);
        let points = to_points(&params.points);
        let (name, fits) = match kind {
            ShapeKind::Line => ("a line takes its two ends", points.len() == 2),
            ShapeKind::Rectangle => ("a rectangle takes two opposite corners", points.len() == 2),
            ShapeKind::Ellipse => (
                "an ellipse takes two opposite corners of its box",
                points.len() == 2,
            ),
            ShapeKind::Polygon => ("a polygon takes three or more corners", points.len() >= 3),
        };
        if !fits {
            return Err(format!("{name}, not {} points", points.len()));
        }
        if params.outline.is_none() && params.fill.is_none() {
            return Err("give an outline color, a fill color or both".into());
        }
        if kind == ShapeKind::Line && params.fill.is_some() {
            return Err("a line has no inside to fill; give it an outline color".into());
        }
        let outline = params
            .outline
            .as_deref()
            .map(parse_color)
            .transpose()
            .map_err(message)?;
        let fill = params
            .fill
            .as_deref()
            .map(parse_color)
            .transpose()
            .map_err(message)?;
        let width = parse_width(params.width).map_err(message)?;

        let shape = |base: Stroke| match kind {
            ShapeKind::Polygon => Stroke::polygon(points.clone(), true, base),
            _ => Stroke::shape(kind, points[0], points[1], base),
        };
        let mut strokes = Vec::new();
        if let Some(color) = fill {
            strokes.push(shape(Stroke {
                color,
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
        let mut images = self.images();
        let image = images.get_mut(params.image).map_err(message)?;
        image.draw(&strokes).map_err(message)?;
        Ok(format!(
            "Drew {} on image {}",
            shape_name(kind),
            params.image
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

fn to_points(points: &[[f32; 2]]) -> Vec<Point<Pixels>> {
    points.iter().map(|&[x, y]| point(px(x), px(y))).collect()
}

fn shape_name(kind: ShapeKind) -> &'static str {
    match kind {
        ShapeKind::Line => "a line",
        ShapeKind::Rectangle => "a rectangle",
        ShapeKind::Ellipse => "an ellipse",
        ShapeKind::Polygon => "a polygon",
    }
}

fn message(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn shape(shape: ShapeName, points: &[[f32; 2]]) -> DrawShapeParams {
        DrawShapeParams {
            image: 1,
            shape,
            points: points.to_vec(),
            outline: None,
            fill: None,
            width: None,
        }
    }

    fn draw_shape(server: &PaintServer, params: DrawShapeParams) -> Result<String, String> {
        server.draw_shape(Parameters(params))
    }

    fn stroke(points: &[[f32; 2]]) -> DrawStrokeParams {
        DrawStrokeParams {
            image: 1,
            points: points.to_vec(),
            color: "#000000".into(),
            width: None,
            brush: None,
        }
    }

    #[test]
    fn draws_a_stroke_and_undoes_it() {
        let server = server();
        server
            .draw_stroke(Parameters(stroke(&[[10.0, 50.0], [90.0, 50.0]])))
            .unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (0, 0, 0));
        server.undo(Parameters(ImageParams { image: 1 })).unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (255, 255, 255));
        server.redo(Parameters(ImageParams { image: 1 })).unwrap();
        assert_eq!(rgb_at(&server, 50, 50), (0, 0, 0));
    }

    #[test]
    fn a_single_point_leaves_a_dot() {
        let server = server();
        server
            .draw_stroke(Parameters(stroke(&[[30.0, 30.0]])))
            .unwrap();
        assert_eq!(rgb_at(&server, 30, 30), (0, 0, 0));
    }

    #[test]
    fn refuses_bad_strokes() {
        let server = server();
        assert!(server.draw_stroke(Parameters(stroke(&[]))).is_err());
        let wide = DrawStrokeParams {
            width: Some(33.0),
            ..stroke(&[[10.0, 10.0]])
        };
        assert!(
            server
                .draw_stroke(Parameters(wide))
                .unwrap_err()
                .contains("32")
        );
        let outside = stroke(&[[500.0, 500.0], [600.0, 500.0]]);
        let error = server.draw_stroke(Parameters(outside)).unwrap_err();
        assert!(error.contains("outside"), "{error}");
        let missing = DrawStrokeParams {
            image: 7,
            ..stroke(&[[10.0, 10.0]])
        };
        assert!(server.draw_stroke(Parameters(missing)).is_err());
    }

    #[test]
    fn fills_and_outlines_a_rectangle_as_one_step() {
        let server = server();
        let params = DrawShapeParams {
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
        let params = DrawShapeParams {
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
        let params = DrawShapeParams {
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
            DrawShapeParams {
                outline: black(),
                ..shape(ShapeName::Line, &[[0.0, 0.0]])
            },
            DrawShapeParams {
                outline: black(),
                ..shape(ShapeName::Rectangle, &[[0.0, 0.0], [5.0, 5.0], [9.0, 9.0]])
            },
            DrawShapeParams {
                outline: black(),
                ..shape(ShapeName::Polygon, &[[0.0, 0.0], [5.0, 5.0]])
            },
            shape(ShapeName::Rectangle, &[[0.0, 0.0], [5.0, 5.0]]),
            DrawShapeParams {
                fill: black(),
                ..shape(ShapeName::Line, &[[0.0, 0.0], [5.0, 5.0]])
            },
            DrawShapeParams {
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
