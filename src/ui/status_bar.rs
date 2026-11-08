use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, button::*,
    checkbox::Checkbox, h_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::zoom::ZOOM_LEVELS;
use crate::{KEY_CONTEXT, PaintApp, ResetZoom, ZoomIn, ZoomOut};

/// Tile grid sizes offered in the status bar, in canvas pixels.
const TILE_SIZES: [u32; 4] = [8, 16, 32, 64];

impl PaintApp {
    pub(super) fn status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let zoom = self.zoom();
        h_flex()
            .gap_4()
            .px_3()
            .h(px(28.0))
            .text_sm()
            .text_color(theme.muted_foreground)
            .bg(theme.background)
            .border_t_1()
            .border_color(theme.border)
            .child(format!(
                "{} × {}px",
                self.canvas_size.width.as_f32(),
                self.canvas_size.height.as_f32()
            ))
            .when_some(self.cursor_position, |this, position| {
                this.child(format!("{}, {}px", position.x, position.y))
            })
            .when_some(self.selection.as_ref(), |this, selection| {
                let rect = selection.rect;
                this.child(format!("Selection {} × {}px", rect.width, rect.height))
            })
            .child(div().flex_1())
            .child(
                Checkbox::new("pixel-grid")
                    .label("Pixel grid")
                    .tooltip("Lines between pixels, from 400% zoom")
                    .checked(self.grid.pixels)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.grid.pixels = *checked;
                        cx.notify();
                    })),
            )
            .child(
                h_flex().gap_1().child("Tiles").child(
                    ButtonGroup::new("tile-grid")
                        .outline()
                        .xsmall()
                        .child(
                            Button::new("tiles-off")
                                .label("Off")
                                .selected(self.grid.tile.is_none()),
                        )
                        .children(TILE_SIZES.map(|tile| {
                            Button::new(("tiles", tile as usize))
                                .label(tile.to_string())
                                .selected(self.grid.tile == Some(tile))
                        }))
                        .on_click(cx.listener(|this, clicks: &Vec<usize>, _, cx| {
                            if let Some(&ix) = clicks.first() {
                                // The first button turns the tile grid off
                                this.grid.tile =
                                    ix.checked_sub(1).and_then(|ix| TILE_SIZES.get(ix).copied());
                                cx.notify();
                            }
                        })),
                ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("zoom-out")
                            .ghost()
                            .compact()
                            .label("−")
                            .tooltip_with_action("Zoom out", &ZoomOut, Some(KEY_CONTEXT))
                            .disabled(self.zoom_index == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_step(false, cx))),
                    )
                    .child(
                        Button::new("reset-zoom")
                            .ghost()
                            .compact()
                            .label(format!("{}%", (zoom * 100.0).round()))
                            .tooltip_with_action("Reset zoom", &ResetZoom, Some(KEY_CONTEXT))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_zoom(&ResetZoom, window, cx)
                            })),
                    )
                    .child(
                        Button::new("zoom-in")
                            .ghost()
                            .compact()
                            .label("+")
                            .tooltip_with_action("Zoom in", &ZoomIn, Some(KEY_CONTEXT))
                            .disabled(self.zoom_index == ZOOM_LEVELS.len() - 1)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_step(true, cx))),
                    ),
            )
    }
}
