use gpui_kit::*;

use crate::{PaintApp, ResetZoom, ZoomIn, ZoomOut};

/// Zoom levels, as displayed pixels per canvas pixel.
pub const ZOOM_LEVELS: [f32; 8] = [0.25, 0.5, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0];
pub const DEFAULT_ZOOM_INDEX: usize = 2;
/// Scroll distance with Cmd/Ctrl held, or pinch amount, per zoom step.
const SCROLL_PER_ZOOM_STEP: f32 = 50.0;
const PINCH_PER_ZOOM_STEP: f32 = 0.15;

impl PaintApp {
    pub fn zoom(&self) -> f32 {
        ZOOM_LEVELS[self.zoom_index]
    }

    pub fn to_canvas(&self, position: Point<Pixels>) -> Point<Pixels> {
        let position = position - self.canvas_bounds.get().origin;
        point(position.x / self.zoom(), position.y / self.zoom())
    }

    /// Zooms to `ZOOM_LEVELS[index]`, keeping the canvas point under `anchor`
    /// (in window coordinates) in place by scrolling the viewport.
    fn zoom_to(&mut self, index: usize, anchor: Point<Pixels>, cx: &mut Context<Self>) {
        let index = index.min(ZOOM_LEVELS.len() - 1);
        if index == self.zoom_index {
            return;
        }
        let canvas_point = self.to_canvas(anchor);
        let zoom = ZOOM_LEVELS[index];
        let canvas_origin = self.canvas_bounds.get().origin;
        let new_origin = anchor - point(canvas_point.x * zoom, canvas_point.y * zoom);
        let offset = self.scroll_handle.offset() + (new_origin - canvas_origin);
        // Offsets are negative when scrolled; past zero would leave a gap at the start
        self.scroll_handle
            .set_offset(point(offset.x.min(px(0.0)), offset.y.min(px(0.0))));
        self.zoom_index = index;
        cx.notify();
    }

    /// Zooms one step in or out around the middle of the visible canvas.
    pub fn zoom_step(&mut self, zoom_in: bool, cx: &mut Context<Self>) {
        let visible = self.visible_bounds.get();
        let anchor = visible.origin + point(visible.size.width / 2.0, visible.size.height / 2.0);
        let index = if zoom_in {
            self.zoom_index + 1
        } else {
            self.zoom_index.saturating_sub(1)
        };
        self.zoom_to(index, anchor, cx);
    }

    pub fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_step(true, cx);
    }

    pub fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_step(false, cx);
    }

    pub fn reset_zoom(&mut self, _: &ResetZoom, _: &mut Window, cx: &mut Context<Self>) {
        let visible = self.visible_bounds.get();
        self.zoom_to(DEFAULT_ZOOM_INDEX, visible.origin, cx);
    }

    /// Accumulates scroll or pinch input and zooms a step around `anchor`
    /// each time it passes `per_step`.
    fn zoom_by_input(
        &mut self,
        amount: f32,
        per_step: f32,
        anchor: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.zoom_input += amount;
        while self.zoom_input.abs() >= per_step {
            let zoom_in = self.zoom_input > 0.0;
            self.zoom_input -= per_step.copysign(self.zoom_input);
            let index = if zoom_in {
                self.zoom_index + 1
            } else {
                self.zoom_index.saturating_sub(1)
            };
            self.zoom_to(index, anchor, cx);
        }
    }

    pub fn on_scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !event.modifiers.secondary(cx) {
            return;
        }
        // Zoom instead of scrolling the viewport
        cx.stop_propagation();
        let delta = event.delta.pixel_delta(px(16.0)).y.as_f32();
        self.zoom_by_input(delta, SCROLL_PER_ZOOM_STEP, event.position, cx);
    }

    pub fn on_pinch(&mut self, event: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by_input(event.delta, PINCH_PER_ZOOM_STEP, event.position, cx);
    }

    /// The canvas pixel at `position` in window coordinates, which may lie
    /// outside the canvas.
    pub fn canvas_pixel(&self, position: Point<Pixels>) -> Point<i32> {
        let position = self.to_canvas(position);
        point(
            position.x.as_f32().floor() as i32,
            position.y.as_f32().floor() as i32,
        )
    }
}
