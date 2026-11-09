use gpui_kit::component::{ActiveTheme as _, Selectable as _, button::*, v_flex};
use gpui_kit::*;

use crate::tool::{SelectTool, TOOLS};
use crate::{KEY_CONTEXT, PaintApp};

impl PaintApp {
    /// Tools, as a column of icons left of the canvas.
    pub(super) fn tool_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_1()
            .p_1()
            .bg(theme.background)
            .border_r_1()
            .border_color(theme.border)
            .children(TOOLS.map(|tool| {
                Button::new(tool.label())
                    .ghost()
                    .icon(tool.icon())
                    .tooltip_with_action(tool.label(), &SelectTool(tool), Some(KEY_CONTEXT))
                    .selected(self.tool == tool)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_tool(tool, cx)))
            }))
    }
}
