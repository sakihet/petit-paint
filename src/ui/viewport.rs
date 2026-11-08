use gpui_kit::component::{ActiveTheme as _, Sizable as _, input::Input};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::view::{paint_floating, paint_stroke, paint_view};
use crate::{PaintApp, ResizeHandle};

/// Side length of the square handles for resizing the canvas.
const HANDLE_SIZE: Pixels = px(8.0);
const SELECTION_COLOR: u32 = 0x0078d7;

impl PaintApp {
    /// The scrollable area holding the canvas.
    pub(super) fn viewport(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let canvas_bounds = self.canvas_bounds.clone();
        let visible_bounds = self.visible_bounds.clone();
        let view_cache = self.view_cache.clone();
        let floating_cache = self.floating_cache.clone();
        let text_cache = self.text_cache.clone();
        let stroke_cache = self.stroke_cache.clone();
        let preview = self.shape_preview();
        let app = cx.entity();

        let zoom = self.zoom();
        // The canvas as displayed, at the current zoom
        let canvas_size = size(
            self.canvas_size.width * zoom,
            self.canvas_size.height * zoom,
        );
        let handle = |handle: ResizeHandle, cx: &Context<Self>| {
            let (on_right, on_bottom) = handle.axes();
            // Centered on the edge midpoint, or on the corner, just outside the canvas
            let left = if on_right {
                canvas_size.width + px(2.0)
            } else {
                canvas_size.width / 2.0 - HANDLE_SIZE / 2.0
            };
            let top = if on_bottom {
                canvas_size.height + px(2.0)
            } else {
                canvas_size.height / 2.0 - HANDLE_SIZE / 2.0
            };
            div()
                .absolute()
                .left(left)
                .top(top)
                .size(HANDLE_SIZE)
                .bg(theme.background)
                .border_1()
                .border_color(theme.foreground)
                .cursor(handle.cursor())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event, _, cx| this.start_resize(handle, event, cx)),
                )
        };
        let handles = [
            handle(ResizeHandle::Right, cx),
            handle(ResizeHandle::Bottom, cx),
            handle(ResizeHandle::Corner, cx),
        ];

        let canvas_area = div()
            .relative()
            .w(canvas_size.width)
            .h(canvas_size.height)
            .flex_none()
            .shadow_md()
            .children(handles)
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| {
                        canvas_bounds.set(bounds);
                        visible_bounds.set(bounds.intersect(&window.content_mask().bounds));
                        // Keep strokes that run past the edge from drawing outside the canvas
                        let mask = ContentMask { bounds };
                        window.with_content_mask(Some(mask), |window| {
                            let app = app.read(cx);
                            paint_view(
                                window,
                                &app.canvas,
                                bounds,
                                zoom,
                                app.grid,
                                &mut view_cache.borrow_mut(),
                            );
                            if let Some(selection) = &app.selection
                                && let Some(floating) = &selection.floating
                            {
                                let rect = selection.rect;
                                let at = bounds.origin
                                    + point(px(rect.x as f32 * zoom), px(rect.y as f32 * zoom));
                                paint_floating(
                                    window,
                                    &floating.pixels,
                                    app.floating_id,
                                    at,
                                    zoom,
                                    &mut floating_cache.borrow_mut(),
                                );
                            }
                            if let Some(edit) = &app.text_edit
                                && let Some((pixels, margin)) = &edit.preview
                            {
                                let x = edit.anchor.x as f32 - *margin as f32;
                                let at = bounds.origin
                                    + point(px(x * zoom), px(edit.anchor.y as f32 * zoom));
                                paint_floating(
                                    window,
                                    pixels,
                                    app.text_preview_id,
                                    at,
                                    zoom,
                                    &mut text_cache.borrow_mut(),
                                );
                            }
                            if let Some(layer) = &app.stroke_preview {
                                let at = bounds.origin
                                    + point(
                                        px(layer.at.x as f32 * zoom),
                                        px(layer.at.y as f32 * zoom),
                                    );
                                paint_floating(
                                    window,
                                    &layer.pixels,
                                    app.stroke_preview_id,
                                    at,
                                    zoom,
                                    &mut stroke_cache.borrow_mut(),
                                );
                            }
                            if let Some(stroke) = &preview {
                                paint_stroke(window, stroke, bounds.origin, zoom);
                            }
                        });
                    },
                )
                .size_full(),
            )
            .when_some(self.selection.as_ref(), |this, selection| {
                let rect = selection.rect;
                this.child(
                    div()
                        .absolute()
                        .left(px(rect.x as f32 * zoom))
                        .top(px(rect.y as f32 * zoom))
                        .w(px(rect.width as f32 * zoom))
                        .h(px(rect.height as f32 * zoom))
                        .border_1()
                        .border_dashed()
                        .border_color(rgb(SELECTION_COLOR)),
                )
            })
            .when_some(self.text_edit.as_ref(), |this, edit| {
                // Below the line of text, so it does not cover the preview
                let line_height = (self.font_size * 1.2).ceil();
                let top = (edit.anchor.y as f32 + line_height) * zoom + 4.0;
                this.child(
                    div()
                        .id("text-editor")
                        .absolute()
                        .left(px(edit.anchor.x as f32 * zoom))
                        .top(px(top))
                        .w(px(240.0))
                        .bg(theme.background)
                        .shadow_md()
                        .rounded_md()
                        // Clicks in the box edit the text rather than reach the canvas
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(Input::new(&edit.input).small()),
                )
            });

        // Fills the viewport and holds the canvas. Zoom gestures are handled
        // here because a child's listeners run before the viewport scrolls.
        let stage = div()
            .flex()
            .items_start()
            .flex_none()
            .min_w_full()
            .min_h_full()
            .p_3()
            .on_scroll(cx.listener(Self::on_scroll))
            .on_pinch(cx.listener(Self::on_pinch))
            .child(canvas_area);

        // Mouse handlers sit on the viewport so a drag keeps drawing when it
        // leaves the canvas
        div()
            .id("viewport")
            .flex()
            .items_start()
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
            .bg(theme.muted)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            // The right button draws with color 2
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered && this.cursor_position.take().is_some() {
                    cx.notify();
                }
            }))
            .child(stage)
    }
}
