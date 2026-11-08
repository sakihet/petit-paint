use gpui_kit::*;
use image::{Frame, RgbaImage};
use lyon::tessellation::{LineCap, LineJoin};
use std::sync::Arc;
use tiny_skia::Pixmap;

use crate::Stroke;
use crate::canvas::{self, Canvas, Grid};

/// Side of the checkerboard squares behind transparent pixels, in logical pixels.
const CHECKER_SIZE: f32 = 8.0;

/// What the cached view image was rendered from.
#[derive(Clone, Copy, PartialEq)]
struct ViewKey {
    version: u64,
    grid: Grid,
    /// Device pixels per canvas pixel, as bits so the key can be compared exactly.
    pixels_per_canvas_pixel: u32,
    origin: Point<u32>,
    size: Size<u32>,
}

/// The visible part of the canvas as a GPU image, re-rendered only when the
/// canvas changes or a different part of it becomes visible.
#[derive(Default)]
pub struct ViewCache {
    key: Option<ViewKey>,
    image: Option<Arc<RenderImage>>,
}

/// Paints `stroke` with the canvas top-left at `origin`, in window
/// coordinates, scaled by `zoom`.
pub fn paint_stroke(window: &mut Window, stroke: &Stroke, origin: Point<Pixels>, zoom: f32) {
    let Some(first) = stroke.points.first() else {
        return;
    };
    let to_window = |p: Point<Pixels>| origin + point(p.x * zoom, p.y * zoom);
    let trace = |builder: &mut PathBuilder| {
        builder.move_to(to_window(*first));
        if stroke.points.len() == 1 {
            // Draw a tiny segment so a single click still leaves a visible dot
            builder.line_to(to_window(*first + Point::new(px(0.1), px(0.1))));
        } else {
            for p in &stroke.points[1..] {
                builder.line_to(to_window(*p));
            }
        }
        if stroke.closed {
            builder.close();
        }
    };

    if stroke.filled {
        let mut builder = PathBuilder::fill();
        trace(&mut builder);
        if let Ok(path) = builder.build() {
            window.paint_path(path, stroke.color);
        }
    }
    // Round caps turn a single click into a dot; round joins avoid miter
    // spikes on sharp turns of freehand strokes
    let join = if stroke.sharp_corners {
        LineJoin::Miter
    } else {
        LineJoin::Round
    };
    let options = StrokeOptions::default()
        .with_line_width(stroke.width.as_f32() * zoom)
        .with_line_cap(LineCap::Round)
        .with_line_join(join);
    let mut builder = PathBuilder::default().with_style(PathStyle::Stroke(options));
    trace(&mut builder);
    if let Ok(path) = builder.build() {
        window.paint_path(path, stroke.color);
    }
}

/// Paints floating selection pixels with their top-left at `at`, in window
/// coordinates, sampled like the canvas view so they stay sharp.
pub fn paint_floating(
    window: &mut Window,
    pixels: &Pixmap,
    id: u64,
    at: Point<Pixels>,
    zoom: f32,
    cache: &mut ViewCache,
) {
    let scale = window.scale_factor();
    let pixels_per_canvas_pixel = scale * zoom;
    let key = ViewKey {
        version: id,
        grid: Grid::default(),
        pixels_per_canvas_pixel: pixels_per_canvas_pixel.to_bits(),
        origin: point(0, 0),
        size: size(pixels.width(), pixels.height()),
    };
    if cache.key != Some(key) {
        let (bytes, region) = canvas::render_pixmap(pixels, pixels_per_canvas_pixel);
        let Some(frame) = RgbaImage::from_raw(region.width, region.height, bytes) else {
            return;
        };
        let image = Arc::new(RenderImage::new(vec![Frame::new(frame)]));
        if let Some(previous) = cache.image.replace(image) {
            window.drop_image(previous).ok();
        }
        cache.key = Some(key);
    }
    let Some(image) = cache.image.clone() else {
        return;
    };
    // Snap to device pixels so sampling stays 1:1
    let snap = |pixels: Pixels| px((pixels.as_f32() * scale).round() / scale);
    let image_size = image.size(0);
    let image_bounds = Bounds::new(
        point(snap(at.x), snap(at.y)),
        size(
            px(image_size.width.0 as f32 / scale),
            px(image_size.height.0 as f32 / scale),
        ),
    );
    window
        .paint_image(
            image_bounds,
            image_bounds,
            Corners::default(),
            image,
            0,
            false,
        )
        .ok();
}

/// Paints the visible part of `canvas`, displayed at `bounds` in window
/// coordinates and scaled by `zoom`.
///
/// gpui samples images with linear filtering, which would blur pixels when
/// the canvas is scaled up. So the view is rendered on the CPU at exactly the
/// device resolution and drawn 1:1.
pub fn paint_view(
    window: &mut Window,
    canvas: &Canvas,
    bounds: Bounds<Pixels>,
    zoom: f32,
    grid: Grid,
    cache: &mut ViewCache,
) {
    let scale = window.scale_factor();
    let visible = bounds.intersect(&window.content_mask().bounds);
    if visible.size.width <= px(0.0) || visible.size.height <= px(0.0) {
        return;
    }
    // Device pixels relative to the canvas's top-left, snapped to whole pixels
    let to_device = |pixels: Pixels| pixels.as_f32() * scale;
    let canvas_x = to_device(bounds.origin.x).round();
    let canvas_y = to_device(bounds.origin.y).round();
    let left = (to_device(visible.origin.x).floor() - canvas_x).max(0.0);
    let top = (to_device(visible.origin.y).floor() - canvas_y).max(0.0);
    let right = to_device(visible.origin.x + visible.size.width).ceil() - canvas_x;
    let bottom = to_device(visible.origin.y + visible.size.height).ceil() - canvas_y;
    if right <= left || bottom <= top {
        return;
    }
    let key = ViewKey {
        version: canvas.version(),
        grid,
        pixels_per_canvas_pixel: (scale * zoom).to_bits(),
        origin: point(left as u32, top as u32),
        size: size((right - left) as u32, (bottom - top) as u32),
    };

    if cache.key != Some(key) {
        let bytes = canvas.render_view(
            key.origin,
            key.size,
            scale * zoom,
            CHECKER_SIZE * scale,
            grid,
        );
        let Some(frame) = RgbaImage::from_raw(key.size.width, key.size.height, bytes) else {
            return;
        };
        let image = Arc::new(RenderImage::new(vec![Frame::new(frame)]));
        // Free the previous view's GPU texture
        if let Some(previous) = cache.image.replace(image) {
            window.drop_image(previous).ok();
        }
        cache.key = Some(key);
    }
    let Some(image) = cache.image.clone() else {
        return;
    };

    let image_bounds = Bounds::new(
        point(px((canvas_x + left) / scale), px((canvas_y + top) / scale)),
        size(px((right - left) / scale), px((bottom - top) / scale)),
    );
    window
        .paint_image(
            image_bounds,
            image_bounds,
            Corners::default(),
            image,
            0,
            false,
        )
        .ok();
}
