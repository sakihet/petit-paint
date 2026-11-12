use anyhow::Context as _;
use gpui_kit::component::{WindowExt as _, notification::Notification};
use gpui_kit::*;
use image::ImageFormat;
use std::path::{Path, PathBuf};
use tiny_skia::{ColorU8, Pixmap};

use crate::{
    CloseWindow, DEFAULT_CANVAS_SIZE, MAX_CANVAS_SIDE, NewImage, OpenImage, PaintApp, Save, SaveAs,
};

impl PaintApp {
    /// Whether the drawing has changes that were not saved. Undoing back to
    /// the saved state still counts as changed.
    fn is_dirty(&self) -> bool {
        let floating = self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.floating.is_some());
        let typing = self
            .text_edit
            .as_ref()
            .is_some_and(|edit| edit.preview.is_some());
        self.canvas.version() != self.saved_version || floating || typing
    }

    fn document_name(&self) -> String {
        self.document_path
            .as_ref()
            .and_then(|path| path.file_name())
            .map_or("Untitled".into(), |name| {
                name.to_string_lossy().into_owned()
            })
    }

    /// Runs `then` right away when there are no unsaved changes. Otherwise
    /// asks whether to save them first, and runs it unless the user cancels
    /// or the save does not happen.
    fn confirm_discard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if !self.is_dirty() {
            then(self, window, cx);
            return;
        }
        if self.confirming {
            return;
        }
        self.confirming = true;
        let message = format!("Do you want to save changes to {}?", self.document_name());
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some("Your changes will be lost if you don't save them."),
            &["Save", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            this.update(cx, |this, _| this.confirming = false).ok();
            let Ok(answer) = answer else {
                return;
            };
            let proceed = match answer {
                0 => match this.update_in(cx, |this, window, cx| this.save(window, cx)) {
                    Ok(saved) => saved.await,
                    Err(_) => false,
                },
                1 => true,
                _ => false,
            };
            if proceed {
                this.update_in(cx, |this, window, cx| then(this, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Whether the window may close now. With unsaved changes it asks first
    /// and closes the window itself once the user decides.
    pub fn should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.close_confirmed || !self.is_dirty() {
            return true;
        }
        self.confirm_discard(window, cx, |this, window, _| {
            this.close_confirmed = true;
            window.remove_window();
        });
        false
    }

    pub fn close_window(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        if self.should_close(window, cx) {
            window.remove_window();
        }
    }

    /// Starts over with a blank, transparent canvas at the default size, as
    /// the app opens with. Like opening, this is one undo step.
    pub fn new_image(&mut self, _: &NewImage, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_discard(window, cx, |this, _, cx| {
            let pixels = Pixmap::new(
                DEFAULT_CANVAS_SIZE.width.as_f32() as u32,
                DEFAULT_CANVAS_SIZE.height.as_f32() as u32,
            )
            .expect("default canvas size is valid");
            this.load_image(None, pixels, cx);
        });
    }

    pub fn open_image(&mut self, _: &OpenImage, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_discard(window, cx, |this, window, cx| this.choose_image(window, cx));
    }

    /// Asks for an image file and opens it.
    fn choose_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            match paths.await {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        this.update_in(cx, |this, window, cx| this.open_path(path, window, cx))
                            .ok();
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(err)) => {
                    let notification = Notification::error(format!("Failed to open: {err:#}"));
                    cx.update(|window, cx| window.push_notification(notification, cx))
                        .ok();
                }
                // The dialog went away with the window
                Err(_) => {}
            }
        })
        .detach();
    }

    /// Opens the first of the files dropped on the window.
    pub fn open_dropped(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = paths.paths().first().cloned() else {
            return;
        };
        self.confirm_discard(window, cx, move |this, window, cx| {
            this.open_path(path, window, cx)
        });
    }

    /// Opens an image file as the canvas, at its size and with its transparency.
    /// Replacing the canvas is one undo step, so an accidental open can be undone.
    fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        match read_image(&path) {
            Ok(pixels) => self.load_image(Some(path), pixels, cx),
            Err(err) => {
                let notification = Notification::error(format!("Failed to open: {err:#}"));
                window.push_notification(notification, cx);
            }
        }
    }

    /// Swaps in `pixels` as the drawing, from the file at `path` or new when
    /// there is none.
    fn load_image(&mut self, path: Option<PathBuf>, pixels: Pixmap, cx: &mut Context<Self>) {
        self.deselect(cx);
        self.cancel_polygon(cx);
        self.text_edit = None;
        let undo = self.canvas.replace(pixels);
        self.record(undo, cx);
        self.canvas_size = self.canvas.size();
        self.document_path = path;
        self.saved_version = self.canvas.version();
        cx.notify();
    }

    pub fn on_save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        self.save(window, cx).detach();
    }

    pub fn on_save_as(&mut self, _: &SaveAs, window: &mut Window, cx: &mut Context<Self>) {
        self.save_as(window, cx).detach();
    }

    /// Writes to the file last opened or saved, or asks where when there is
    /// none or its format cannot be saved. The task resolves to whether the
    /// drawing was saved.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<bool> {
        match self.document_path.clone() {
            Some(path) if save_format(&path).is_some() => self.write(Some(path), window, cx),
            _ => self.save_as(window, cx),
        }
    }

    /// Asks where to save and writes the image there.
    fn save_as(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<bool> {
        self.write(None, window, cx)
    }

    /// Writes the image to `path`, or to where the user chooses when it is
    /// `None`, in the format its extension names. The task resolves to
    /// whether the drawing was saved.
    fn write(
        &mut self,
        path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        // Include floating pixels, and copy them now so the image reflects
        // the drawing at the time of the request
        self.commit_selection(cx);
        self.commit_text(cx);
        let pixels = self.canvas.pixels().clone();
        let version = self.canvas.version();
        // Suggest the file last opened or saved, so saving again overwrites it
        let directory = self
            .document_path
            .as_ref()
            .and_then(|path| path.parent())
            .map(|dir| dir.to_path_buf())
            .or_else(std::env::home_dir)
            .unwrap_or_default();
        let name = self
            .document_path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned());
        let prompt = path.is_none().then(|| {
            cx.prompt_for_new_path(&directory, Some(name.as_deref().unwrap_or("untitled.png")))
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let path = match prompt {
                    Some(prompt) => {
                        let Some(chosen) = prompt.await?? else {
                            return Ok(None);
                        };
                        let path = with_image_extension(chosen.clone());
                        // The save dialog only confirmed overwriting the name as typed
                        if path != chosen && path.exists() {
                            let message = format!(
                                "{} already exists. Do you want to replace it?",
                                path.file_name().unwrap_or_default().to_string_lossy()
                            );
                            let answer = this.update_in(cx, |this, window, cx| {
                                this.confirming = true;
                                window.prompt(
                                    PromptLevel::Warning,
                                    &message,
                                    None,
                                    &["Replace", "Cancel"],
                                    cx,
                                )
                            })?;
                            let answer = answer.await;
                            this.update(cx, |this, _| this.confirming = false)?;
                            if answer? != 0 {
                                return Ok(None);
                            }
                        }
                        Some(path)
                    }
                    None => path,
                };
                let Some(path) = path else {
                    return Ok(None);
                };
                // Paths here always end in an extension saving writes
                write_image(&path, &pixels)?;
                anyhow::Ok(Some(path))
            }
            .await;

            let (notification, saved) = match result {
                Ok(Some(path)) => {
                    let notification = Notification::success(format!("Saved {}", path.display()));
                    this.update(cx, |this, _| {
                        this.document_path = Some(path);
                        // Changes made while the dialog was open are still unsaved
                        this.saved_version = version;
                    })
                    .ok();
                    (notification, true)
                }
                Ok(None) => return false,
                Err(err) => (
                    Notification::error(format!("Failed to save: {err:#}")),
                    false,
                ),
            };
            cx.update(|window, cx| window.push_notification(notification, cx))
                .ok();
            saved
        })
    }
}

/// Reads the image file at `path`, if it fits on the canvas.
pub fn read_image(path: &Path) -> anyhow::Result<Pixmap> {
    let bytes =
        std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    let pixels = decode_image(&bytes).map_err(|_| {
        anyhow::anyhow!(
            "{} is not a PNG, JPEG, BMP, WebP or GIF image",
            path.display()
        )
    })?;
    if pixels.width() > MAX_CANVAS_SIDE || pixels.height() > MAX_CANVAS_SIDE {
        anyhow::bail!(
            "{}×{} is larger than the {MAX_CANVAS_SIDE}×{MAX_CANVAS_SIDE} limit",
            pixels.width(),
            pixels.height()
        );
    }
    Ok(pixels)
}

/// Writes `pixels` to `path` in the format its extension names, refusing
/// extensions that saving does not write.
pub fn write_image(path: &Path, pixels: &Pixmap) -> anyhow::Result<()> {
    let format = save_format(path).ok_or_else(|| {
        anyhow::anyhow!(
            "{} does not end in .png, .jpg, .jpeg, .bmp, .webp or .gif",
            path.display()
        )
    })?;
    std::fs::write(path, encode_image(pixels, format)?)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

/// Decodes a PNG, JPEG, BMP, WebP or GIF image, taking a GIF's first frame.
fn decode_image(bytes: &[u8]) -> anyhow::Result<Pixmap> {
    let rgba = image::load_from_memory(bytes)?.into_rgba8();
    let mut pixels = Pixmap::new(rgba.width(), rgba.height())
        .ok_or_else(|| anyhow::anyhow!("the image is empty"))?;
    for (pixel, &image::Rgba([r, g, b, a])) in pixels.pixels_mut().iter_mut().zip(rgba.pixels()) {
        *pixel = ColorU8::from_rgba(r, g, b, a).premultiply();
    }
    Ok(pixels)
}

/// File extensions that saving writes, with the format of each.
const SAVE_FORMATS: [(&str, ImageFormat); 6] = [
    ("png", ImageFormat::Png),
    ("jpg", ImageFormat::Jpeg),
    ("jpeg", ImageFormat::Jpeg),
    ("bmp", ImageFormat::Bmp),
    ("webp", ImageFormat::WebP),
    ("gif", ImageFormat::Gif),
];

/// The format to save `path` in, from its extension, ignoring case.
fn save_format(path: &Path) -> Option<ImageFormat> {
    let extension = path.extension()?.to_str()?;
    SAVE_FORMATS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(extension))
        .map(|&(_, format)| format)
}

/// Encodes `pixels` as `format`. JPEG, BMP and GIF are saved without
/// transparency, with transparent areas turned white.
fn encode_image(pixels: &Pixmap, format: ImageFormat) -> anyhow::Result<Vec<u8>> {
    if format == ImageFormat::Png {
        return Ok(pixels.encode_png()?);
    }
    let (width, height) = (pixels.width(), pixels.height());
    let pixel = |x: u32, y: u32| pixels.pixels()[(y * width + x) as usize];
    let mut bytes = std::io::Cursor::new(Vec::new());
    if format == ImageFormat::WebP {
        let rgba = image::RgbaImage::from_fn(width, height, |x, y| {
            let c = pixel(x, y).demultiply();
            image::Rgba([c.red(), c.green(), c.blue(), c.alpha()])
        });
        rgba.write_to(&mut bytes, format)?;
    } else {
        // Premultiplied, so adding the uncovered part of white blends over it
        let rgb = image::RgbImage::from_fn(width, height, |x, y| {
            let c = pixel(x, y);
            let white = 255 - c.alpha();
            image::Rgb([c.red() + white, c.green() + white, c.blue() + white])
        });
        rgb.write_to(&mut bytes, format)?;
    }
    Ok(bytes.into_inner())
}

/// Appends `.png` unless the path already ends with an extension that
/// saving writes, ignoring case.
fn with_image_extension(path: PathBuf) -> PathBuf {
    if save_format(&path).is_some() {
        return path;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".png");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::{decode_image, encode_image, with_image_extension};
    use image::ImageFormat;
    use std::path::{Path, PathBuf};
    use tiny_skia::{ColorU8, Pixmap};

    #[test]
    fn decodes_formats_other_than_png() {
        let mut source = image::RgbaImage::new(2, 1);
        source.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        source.put_pixel(1, 0, image::Rgba([0, 0, 255, 255]));
        let mut bmp = std::io::Cursor::new(Vec::new());
        source.write_to(&mut bmp, image::ImageFormat::Bmp).unwrap();

        let pixels = decode_image(bmp.get_ref()).unwrap();
        assert_eq!((pixels.width(), pixels.height()), (2, 1));
        let rgb = |x| {
            let p = pixels.pixel(x, 0).unwrap();
            (p.red(), p.green(), p.blue(), p.alpha())
        };
        assert_eq!(rgb(0), (255, 0, 0, 255));
        assert_eq!(rgb(1), (0, 0, 255, 255));
    }

    #[test]
    fn rejects_data_that_is_not_an_image() {
        assert!(decode_image(b"not an image").is_err());
    }

    #[test]
    fn saves_each_format_and_turns_transparency_white_without_alpha() {
        // Blue on the left, transparent on the right, wide enough that JPEG's
        // blurring of color does not reach the far right
        let mut pixels = Pixmap::new(32, 8).unwrap();
        for (i, pixel) in pixels.pixels_mut().iter_mut().enumerate() {
            if i % 32 < 8 {
                *pixel = ColorU8::from_rgba(0, 0, 255, 255).premultiply();
            }
        }
        for format in [
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::Bmp,
            ImageFormat::WebP,
            ImageFormat::Gif,
        ] {
            let bytes = encode_image(&pixels, format).unwrap();
            assert_eq!(image::guess_format(&bytes).unwrap(), format);
            let saved = decode_image(&bytes).unwrap();
            let transparent = saved.pixel(31, 4).unwrap().demultiply();
            let expected = match format {
                ImageFormat::Png | ImageFormat::WebP => (0, 0, 0, 0),
                _ => (255, 255, 255, 255),
            };
            // JPEG is lossy, so allow a little difference
            let close = |a: u8, b: u8| a.abs_diff(b) <= 8;
            let actual = (
                transparent.red(),
                transparent.green(),
                transparent.blue(),
                transparent.alpha(),
            );
            assert!(
                close(actual.0, expected.0)
                    && close(actual.1, expected.1)
                    && close(actual.2, expected.2)
                    && close(actual.3, expected.3),
                "{format:?}: {actual:?}"
            );
        }
    }

    fn png_path(path: &str) -> PathBuf {
        with_image_extension(Path::new(path).to_path_buf())
    }

    #[test]
    fn appends_png_extension_when_missing() {
        assert_eq!(png_path("/tmp/drawing"), Path::new("/tmp/drawing.png"));
        assert_eq!(
            png_path("/tmp/my.drawing"),
            Path::new("/tmp/my.drawing.png")
        );
    }

    #[test]
    fn keeps_existing_png_extension() {
        assert_eq!(png_path("/tmp/drawing.png"), Path::new("/tmp/drawing.png"));
        assert_eq!(png_path("/tmp/drawing.PNG"), Path::new("/tmp/drawing.PNG"));
    }

    #[test]
    fn keeps_other_saved_extensions() {
        assert_eq!(png_path("/tmp/photo.jpg"), Path::new("/tmp/photo.jpg"));
        assert_eq!(png_path("/tmp/photo.JPEG"), Path::new("/tmp/photo.JPEG"));
        assert_eq!(png_path("/tmp/icon.webp"), Path::new("/tmp/icon.webp"));
    }
}
