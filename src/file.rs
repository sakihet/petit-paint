use gpui_kit::component::{WindowExt as _, notification::Notification};
use gpui_kit::*;
use std::path::PathBuf;
use tiny_skia::Pixmap;

use crate::{CloseWindow, MAX_CANVAS_SIDE, OpenImage, PaintApp, SavePng};

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

    pub fn open_image(&mut self, _: &OpenImage, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_discard(window, cx, |this, window, cx| this.choose_image(window, cx));
    }

    /// Opens a PNG file as the canvas, at its size and with its transparency.
    /// Replacing the canvas is one undo step, so an accidental open can be undone.
    fn choose_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let Some(path) = paths.await??.and_then(|paths| paths.into_iter().next()) else {
                    return Ok(None);
                };
                let bytes = std::fs::read(&path)?;
                let pixels = Pixmap::decode_png(&bytes)
                    .map_err(|_| anyhow::anyhow!("{} is not a PNG image", path.display()))?;
                if pixels.width() > MAX_CANVAS_SIDE || pixels.height() > MAX_CANVAS_SIDE {
                    anyhow::bail!(
                        "{}×{} is larger than the {MAX_CANVAS_SIDE}×{MAX_CANVAS_SIDE} limit",
                        pixels.width(),
                        pixels.height()
                    );
                }
                anyhow::Ok(Some((path, pixels)))
            }
            .await;

            match result {
                Ok(Some((path, pixels))) => {
                    this.update(cx, |this, cx| this.load_image(path, pixels, cx))
                        .ok();
                }
                Ok(None) => {}
                Err(err) => {
                    let notification = Notification::error(format!("Failed to open: {err:#}"));
                    cx.update(|window, cx| window.push_notification(notification, cx))
                        .ok();
                }
            }
        })
        .detach();
    }

    fn load_image(&mut self, path: PathBuf, pixels: Pixmap, cx: &mut Context<Self>) {
        self.deselect(cx);
        self.cancel_polygon(cx);
        self.text_edit = None;
        let undo = self.canvas.replace(pixels);
        self.record(undo, cx);
        self.canvas_size = self.canvas.size();
        self.document_path = Some(path);
        self.saved_version = self.canvas.version();
        cx.notify();
    }

    pub fn save_png(&mut self, _: &SavePng, window: &mut Window, cx: &mut Context<Self>) {
        self.save(window, cx).detach();
    }

    /// Asks where to save and writes the PNG there. The task resolves to
    /// whether the drawing was saved.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<bool> {
        // Include floating pixels, and encode now so the image reflects the
        // drawing at the time of the request
        self.commit_selection(cx);
        self.commit_text(cx);
        let png = self.canvas.encode_png();
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
        let path =
            cx.prompt_for_new_path(&directory, Some(name.as_deref().unwrap_or("untitled.png")));

        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let Some(path) = path.await?? else {
                    return Ok(None);
                };
                let chosen = path.clone();
                let path = with_png_extension(path);
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
                std::fs::write(&path, png?)?;
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
                    Notification::error(format!("Failed to save PNG: {err:#}")),
                    false,
                ),
            };
            cx.update(|window, cx| window.push_notification(notification, cx))
                .ok();
            saved
        })
    }
}

/// Appends `.png` unless the path already ends with it, ignoring case.
fn with_png_extension(path: PathBuf) -> PathBuf {
    let is_png = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
    if is_png {
        return path;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".png");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::with_png_extension;
    use std::path::{Path, PathBuf};

    fn png_path(path: &str) -> PathBuf {
        with_png_extension(Path::new(path).to_path_buf())
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
}
