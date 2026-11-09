use gpui_kit::component::{ActiveTheme as _, button::*, color_picker::ColorPicker, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::{KEY_CONTEXT, PaintApp, SwapColors};

/// Colors in the palette bar, after Windows Paint's default palette.
const PALETTE: [u32; 20] = [
    0x000000, 0x7f7f7f, 0x880015, 0xed1c24, 0xff7f27, 0xfff200, 0x22b14c, 0x00a2e8, 0x3f48cc,
    0xa349a4, 0xffffff, 0xc3c3c3, 0xb97a57, 0xffaec9, 0xffc90e, 0xefe4b0, 0xb5e61d, 0x99d9ea,
    0x7092be, 0xc8bfe7,
];
const SWATCH_SIZE: Pixels = px(18.0);

impl PaintApp {
    pub(super) fn palette_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let swatch = |id: (&'static str, usize), color: Hsla, cx: &Context<Self>| {
            div()
                .id(id)
                .size(SWATCH_SIZE)
                .flex_none()
                .rounded_sm()
                .border_1()
                .border_color(theme.border)
                .bg(color)
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| this.set_primary(color, window, cx)),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _, window, cx| this.set_secondary(color, window, cx)),
                )
        };
        h_flex()
            .flex_wrap()
            .gap_1()
            .px_3()
            .py_1()
            .bg(theme.background)
            .border_b_1()
            .border_color(theme.border)
            .child(ColorPicker::new(&self.color_picker))
            .child(ColorPicker::new(&self.secondary_picker))
            .child(
                Button::new("swap-colors")
                    .ghost()
                    .compact()
                    .label("⇄")
                    .tooltip_with_action("Swap color 1 and color 2", &SwapColors, Some(KEY_CONTEXT))
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.swap_colors(&SwapColors, window, cx)
                        }),
                    ),
            )
            .child(div().w(px(8.0)))
            .children(
                PALETTE
                    .iter()
                    .enumerate()
                    .map(|(ix, &color)| swatch(("palette", ix), rgb(color).into(), cx)),
            )
            .when(!self.recent_colors.is_empty(), |this| {
                this.child(div().w(px(8.0))).child("Recent").children(
                    self.recent_colors
                        .iter()
                        .enumerate()
                        .map(|(ix, &color)| swatch(("recent", ix), color, cx)),
                )
            })
            .child(div().w(px(8.0)))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Left click: color 1 · Right click: color 2"),
            )
    }
}
