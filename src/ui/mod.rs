use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::*;

use crate::{KEY_CONTEXT, PaintApp};

mod canvas_size_dialog;
mod options_bar;
mod palette_bar;
mod status_bar;
mod tool_bar;
mod viewport;

impl Render for PaintApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_save_as))
            .on_action(cx.listener(Self::new_image))
            .on_action(cx.listener(Self::open_image))
            .on_action(cx.listener(Self::close_window))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy_selection))
            .on_action(cx.listener(Self::cut_selection))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::delete_selection))
            .on_action(cx.listener(Self::on_deselect))
            .on_action(cx.listener(Self::on_finish_polygon))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::on_select_tool))
            .on_action(cx.listener(Self::swap_colors))
            .on_action(cx.listener(Self::thinner_brush))
            .on_action(cx.listener(Self::thicker_brush))
            .size_full()
            .text_color(theme.foreground)
            .child(self.options_bar(cx))
            .child(self.palette_bar(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.tool_bar(cx))
                    .child(self.viewport(cx)),
            )
            .child(self.status_bar(cx))
    }
}
