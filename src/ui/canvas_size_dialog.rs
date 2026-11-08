use gpui_kit::component::{
    WindowExt as _,
    button::*,
    dialog::DialogFooter,
    h_flex,
    input::{InputState, NumberInput},
    notification::Notification,
    v_flex,
};
use gpui_kit::*;
use std::rc::Rc;

use crate::{MAX_CANVAS_SIDE, PaintApp};

impl PaintApp {
    pub fn open_canvas_size_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let side_input = |side: Pixels, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(format!("{}", side.as_f32()))
                    .validate(|text, _| text.chars().all(|c| c.is_ascii_digit()))
                    .min(1.0)
                    .max(MAX_CANVAS_SIDE as f64)
            })
        };
        let width_input = side_input(self.canvas_size.width, window, cx);
        let height_input = side_input(self.canvas_size.height, window, cx);
        width_input.update(cx, |input, cx| input.focus(window, cx));

        let this = cx.entity().downgrade();
        // Returns whether the dialog should close
        let apply: Rc<dyn Fn(&mut Window, &mut App) -> bool> = Rc::new({
            let width_input = width_input.clone();
            let height_input = height_input.clone();
            move |window, cx| {
                let parse = |input: &Entity<InputState>| {
                    input
                        .read(cx)
                        .value()
                        .parse::<u32>()
                        .ok()
                        .filter(|side| (1..=MAX_CANVAS_SIDE).contains(side))
                };
                let (Some(width), Some(height)) = (parse(&width_input), parse(&height_input))
                else {
                    window.push_notification(
                        Notification::error(format!(
                            "Width and height must be between 1 and {MAX_CANVAS_SIDE}"
                        )),
                        cx,
                    );
                    return false;
                };
                let size = size(px(width as f32), px(height as f32));
                this.update(cx, |this, cx| this.resize_canvas(size, cx))
                    .ok();
                true
            }
        });

        window.open_dialog(cx, move |dialog, _, _| {
            let row = |label: &'static str, input: &Entity<InputState>| {
                h_flex()
                    .gap_3()
                    .child(div().w(px(56.0)).child(label))
                    .child(NumberInput::new(input).suffix("px").flex_1())
            };
            let on_ok = apply.clone();
            let ok = apply.clone();
            dialog
                .title("Canvas Size")
                .w(px(320.0))
                .on_ok(move |_, window, cx| on_ok(window, cx))
                .child(
                    v_flex()
                        .gap_3()
                        .child(row("Width", &width_input))
                        .child(row("Height", &height_input)),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .outline()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(Button::new("ok").primary().label("OK").on_click(
                            move |_, window, cx| {
                                if ok(window, cx) {
                                    window.close_dialog(cx);
                                }
                            },
                        )),
                )
        });
    }
}
