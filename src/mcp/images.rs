use anyhow::{Result, anyhow, bail};
use gpui_kit::{Hsla, Size, size};
use image::{ImageBuffer, Rgba, imageops};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tiny_skia::{FilterQuality, IntSize, Pixmap, PixmapPaint, Transform};

use crate::MAX_CANVAS_SIDE;
use crate::canvas::{Canvas, PixelRect};
use crate::file::{read_image, write_image};
use crate::history::History;
use crate::stroke::Stroke;

/// Longest side of the picture view_image returns. Larger images are scaled
/// down to it, which keeps the reply small for the agent reading it.
pub const MAX_VIEW_SIDE: u32 = 1024;
/// How far view_image scales up part of an image.
const MAX_ZOOM: u32 = 16;

/// An image open in the server, with its own undo history.
pub struct Image {
    pub canvas: Canvas,
    pub history: History,
    /// The file it was opened from or last saved to.
    pub path: Option<PathBuf>,
    /// Strokes drawn so far, to seed each airbrush stroke differently.
    strokes: u64,
}

impl Image {
    pub fn size(&self) -> Size<u32> {
        let pixels = self.canvas.pixels();
        size(pixels.width(), pixels.height())
    }

    /// Draws `strokes` in order as one undo step.
    pub fn draw(&mut self, strokes: &[Stroke]) -> Result<()> {
        let Some(undo) = self.canvas.draw_strokes(strokes) else {
            let size = self.size();
            bail!(
                "nothing was drawn, as it lies entirely outside the {}×{} image",
                size.width,
                size.height
            );
        };
        self.history.record(undo);
        Ok(())
    }

    /// A seed for the next stroke's airbrush spray, different each time.
    pub fn next_seed(&mut self) -> u64 {
        self.strokes += 1;
        self.strokes
    }
}

/// The images open in the server, by id. Ids count up from 1 and are not
/// reused, so a closed image's id never names another one.
#[derive(Default)]
pub struct Images {
    images: BTreeMap<u32, Image>,
    last_id: u32,
}

impl Images {
    /// Makes a `width`×`height` image filled with `background`.
    pub fn create(&mut self, width: u32, height: u32, background: Hsla) -> Result<u32> {
        let range = 1..=MAX_CANVAS_SIDE;
        if !range.contains(&width) || !range.contains(&height) {
            bail!("width and height must be between 1 and {MAX_CANVAS_SIDE}, not {width}×{height}");
        }
        let canvas = Canvas::new(width, height, background)?;
        Ok(self.insert(canvas, None))
    }

    /// Opens the image file at `path`.
    pub fn open(&mut self, path: &Path) -> Result<u32> {
        let path = std::path::absolute(path)?;
        let pixels = read_image(&path)?;
        // Transparent, like the canvas the app opens images into
        let mut canvas = Canvas::new(1, 1, gpui_kit::transparent_black())?;
        canvas.replace(pixels);
        Ok(self.insert(canvas, Some(path)))
    }

    fn insert(&mut self, canvas: Canvas, path: Option<PathBuf>) -> u32 {
        self.last_id += 1;
        let image = Image {
            canvas,
            history: History::default(),
            path,
            strokes: 0,
        };
        self.images.insert(self.last_id, image);
        self.last_id
    }

    pub fn get(&self, id: u32) -> Result<&Image> {
        self.images.get(&id).ok_or_else(|| missing(id))
    }

    pub fn get_mut(&mut self, id: u32) -> Result<&mut Image> {
        self.images.get_mut(&id).ok_or_else(|| missing(id))
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &Image)> {
        self.images.iter().map(|(&id, image)| (id, image))
    }

    pub fn close(&mut self, id: u32) -> Result<()> {
        self.images
            .remove(&id)
            .map(|_| ())
            .ok_or_else(|| missing(id))
    }

    /// Saves image `id` to `path`, or to its own file when `path` is `None`,
    /// and returns where it went. That file becomes the image's own.
    pub fn save(&mut self, id: u32, path: Option<&Path>) -> Result<PathBuf> {
        let image = self.get_mut(id)?;
        let path = match path {
            Some(path) => std::path::absolute(path)?,
            None => image
                .path
                .clone()
                .ok_or_else(|| anyhow!("image {id} has not been saved yet, so give a path"))?,
        };
        write_image(&path, image.canvas.pixels())?;
        image.path = Some(path.clone());
        Ok(path)
    }

    /// Image `id` as PNG, scaled down to fit `MAX_VIEW_SIDE`, with the size
    /// it was encoded at.
    ///
    /// Given a `region`, only that part, scaled up by the largest whole
    /// number up to `MAX_ZOOM` that still fits, with each pixel a sharp square.
    pub fn view(&self, id: u32, region: Option<PixelRect>) -> Result<(Vec<u8>, Size<u32>)> {
        let canvas = &self.get(id)?.canvas;
        let pixels = match region {
            None => fit(canvas.pixels(), MAX_VIEW_SIDE),
            Some(region) => {
                let part = canvas.copy(region, None).ok_or_else(|| {
                    let size = canvas.pixels();
                    anyhow!(
                        "the region lies entirely outside the {}×{} image",
                        size.width(),
                        size.height()
                    )
                })?;
                let longest = part.width().max(part.height());
                let zoom = (MAX_VIEW_SIDE / longest).clamp(1, MAX_ZOOM);
                fit(&enlarge(&part, zoom), MAX_VIEW_SIDE)
            }
        };
        let png = pixels.encode_png()?;
        Ok((png, size(pixels.width(), pixels.height())))
    }
}

fn missing(id: u32) -> anyhow::Error {
    anyhow!("there is no image {id}; create or open one first")
}

/// `pixels` scaled up `zoom` times, each pixel becoming a square.
fn enlarge(pixels: &Pixmap, zoom: u32) -> Pixmap {
    if zoom == 1 {
        return pixels.clone();
    }
    let mut enlarged =
        Pixmap::new(pixels.width() * zoom, pixels.height() * zoom).expect("zoomed size is valid");
    enlarged.draw_pixmap(
        0,
        0,
        pixels.as_ref(),
        &PixmapPaint {
            quality: FilterQuality::Nearest,
            ..Default::default()
        },
        Transform::from_scale(zoom as f32, zoom as f32),
        None,
    );
    enlarged
}

/// `pixels` scaled down so neither side exceeds `max_side`, keeping its
/// shape, or a copy when it already fits.
fn fit(pixels: &Pixmap, max_side: u32) -> Pixmap {
    let (width, height) = (pixels.width(), pixels.height());
    let longest = width.max(height);
    if longest <= max_side {
        return pixels.clone();
    }
    let scale = |side: u32| ((side as u64 * max_side as u64) / longest as u64).max(1) as u32;
    // Averaging premultiplied pixels keeps edges from picking up the color
    // of transparent ones
    let source: ImageBuffer<Rgba<u8>, &[u8]> =
        ImageBuffer::from_raw(width, height, pixels.data()).expect("pixmap holds RGBA rows");
    let scaled = imageops::thumbnail(&source, scale(width), scale(height));
    let size = IntSize::from_wh(scaled.width(), scaled.height()).expect("scaled size is not empty");
    Pixmap::from_vec(scaled.into_raw(), size).expect("buffer matches its size")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{point, px, rgb};

    fn white() -> Hsla {
        rgb(0xffffff).into()
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("petit-paint-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ids_count_up_and_are_not_reused() {
        let mut images = Images::default();
        assert_eq!(images.create(10, 10, white()).unwrap(), 1);
        assert_eq!(images.create(10, 10, white()).unwrap(), 2);
        images.close(2).unwrap();
        assert_eq!(images.create(10, 10, white()).unwrap(), 3);
        assert_eq!(images.iter().map(|(id, _)| id).collect::<Vec<_>>(), [1, 3]);
        assert!(images.get(2).is_err());
        assert!(images.close(2).is_err());
    }

    #[test]
    fn refuses_sizes_the_app_refuses() {
        let mut images = Images::default();
        assert!(images.create(0, 10, white()).is_err());
        assert!(images.create(10, MAX_CANVAS_SIDE + 1, white()).is_err());
        assert!(images.create(MAX_CANVAS_SIDE, 1, white()).is_ok());
    }

    #[test]
    fn saves_and_opens_again() {
        let dir = scratch_dir("save");
        let path = dir.join("drawing.png");
        let mut images = Images::default();
        let id = images.create(12, 8, rgb(0xff0000).into()).unwrap();
        assert!(images.save(id, None).is_err(), "nothing to save to yet");
        assert_eq!(images.save(id, Some(&path)).unwrap(), path);
        // Saving again goes to the same file
        std::fs::remove_file(&path).unwrap();
        images.save(id, None).unwrap();

        let opened = images.open(&path).unwrap();
        let image = images.get(opened).unwrap();
        assert_eq!(image.size(), size(12, 8));
        assert_eq!(image.path.as_deref(), Some(path.as_path()));
        let pixel = image.canvas.pixels().pixel(3, 3).unwrap();
        assert_eq!((pixel.red(), pixel.green(), pixel.blue()), (255, 0, 0));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_to_save_formats_it_cannot_write() {
        let dir = scratch_dir("svg");
        let mut images = Images::default();
        let id = images.create(4, 4, white()).unwrap();
        let error = images.save(id, Some(&dir.join("drawing.svg"))).unwrap_err();
        assert!(error.to_string().contains(".png"), "{error}");
        assert!(images.get(id).unwrap().path.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opening_a_missing_file_fails() {
        let mut images = Images::default();
        assert!(images.open(Path::new("/nonexistent/drawing.png")).is_err());
        assert_eq!(images.iter().count(), 0);
    }

    #[test]
    fn drawing_is_one_undo_step() {
        let mut images = Images::default();
        let id = images.create(40, 40, white()).unwrap();
        let image = images.get_mut(id).unwrap();
        let before = image.canvas.pixels().clone();
        let line = |x: f32| Stroke {
            points: vec![point(px(x), px(5.0)), point(px(x), px(35.0))],
            color: rgb(0x000000).into(),
            width: px(4.0),
            ..Default::default()
        };
        image.draw(&[line(10.0), line(30.0)]).unwrap();
        assert_ne!(image.canvas.pixels().data(), before.data());
        assert!(image.history.undo(&mut image.canvas));
        assert_eq!(image.canvas.pixels().data(), before.data());
        assert!(!image.history.can_undo());
    }

    #[test]
    fn drawing_outside_the_image_is_an_error() {
        let mut images = Images::default();
        let id = images.create(40, 30, white()).unwrap();
        let image = images.get_mut(id).unwrap();
        let outside = Stroke {
            points: vec![point(px(100.0), px(100.0))],
            width: px(4.0),
            ..Default::default()
        };
        let error = image.draw(&[outside]).unwrap_err();
        assert!(error.to_string().contains("40×30"), "{error}");
        assert!(!image.history.can_undo());
    }

    #[test]
    fn views_small_images_as_they_are_and_scales_large_ones_down() {
        let mut images = Images::default();
        let small = images.create(300, 200, white()).unwrap();
        let (png, shown) = images.view(small, None).unwrap();
        assert_eq!(shown, size(300, 200));
        assert_eq!(image::guess_format(&png).unwrap(), image::ImageFormat::Png);

        let large = images
            .create(MAX_CANVAS_SIDE, MAX_CANVAS_SIDE / 4, white())
            .unwrap();
        let (png, shown) = images.view(large, None).unwrap();
        assert_eq!(shown, size(MAX_VIEW_SIDE, MAX_VIEW_SIDE / 4));
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!(
            (decoded.width(), decoded.height()),
            (shown.width, shown.height)
        );
    }

    #[test]
    fn scaling_keeps_thin_images_at_least_a_pixel_tall() {
        let pixels = Pixmap::new(4000, 1).unwrap();
        let fitted = fit(&pixels, 100);
        assert_eq!((fitted.width(), fitted.height()), (100, 1));
    }

    #[test]
    fn views_part_of_an_image_scaled_up_with_sharp_pixels() {
        let mut images = Images::default();
        let id = images.create(200, 100, white()).unwrap();
        let image = images.get_mut(id).unwrap();
        image.canvas.draw_stroke(&Stroke {
            points: vec![point(px(10.5), px(10.5))],
            color: rgb(0x000000).into(),
            width: px(1.0),
            brush: crate::canvas::Brush::Pencil,
            ..Default::default()
        });
        let region = PixelRect {
            x: 8,
            y: 8,
            width: 32,
            height: 16,
        };
        let (png, shown) = images.view(id, Some(region)).unwrap();
        // 32 pixels across, 16 times over, fits 1024
        assert_eq!(shown, size(512, 256));
        let decoded = image::load_from_memory(&png).unwrap().into_rgba8();
        // Pixel (10, 10) is the square from (32, 32) to (48, 48), and nothing blurs
        assert_eq!(decoded.get_pixel(40, 40).0, [0, 0, 0, 255]);
        assert_eq!(decoded.get_pixel(32, 32).0, [0, 0, 0, 255]);
        assert_eq!(decoded.get_pixel(31, 40).0, [255, 255, 255, 255]);

        let outside = PixelRect { x: 300, ..region };
        assert!(images.view(id, Some(outside)).is_err());
        // A larger region is scaled up only as far as fits
        let whole = PixelRect {
            x: 0,
            y: 0,
            width: 200,
            height: 100,
        };
        assert_eq!(images.view(id, Some(whole)).unwrap().1, size(1000, 500));
    }
}
