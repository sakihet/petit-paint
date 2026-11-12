use anyhow::{Result, bail};
use base64::Engine as _;
use gpui_kit::{Hsla, rgba};
use images::Images;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::JsonSchema;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

mod images;

const INSTRUCTIONS: &str = "Draws raster images with petit-paint, a small paint app. \
Create an image with new_image or open a file with open_image; each gets an id that the \
other tools take. Coordinates are in image pixels from the top-left. Colors are \
#rrggbb or #rrggbbaa. Call view_image to look at the result, and save_image to write \
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

fn message(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[cfg(test)]
mod tests {
    use super::parse_color;
    use gpui_kit::{Hsla, rgba};

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
