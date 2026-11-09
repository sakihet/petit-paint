use gpui_kit::*;

use crate::PaintApp;

/// An in-progress drag that scrolls the viewport, made with Space held.
#[derive(Clone, Copy)]
pub struct PanDrag {
    /// Mouse position in window coordinates when the drag started.
    start_position: Point<Pixels>,
    start_offset: Point<Pixels>,
}

impl PaintApp {
    /// Holding Space turns the mouse into a hand that drags the canvas around,
    /// unless a text field has focus, where Space types.
    pub fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if is_space(&event.keystroke) && !self.space_held && self.focus_handle.is_focused(window) {
            self.space_held = true;
            cx.notify();
        }
    }

    pub fn on_key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "space" && self.space_held {
            self.space_held = false;
            cx.notify();
        }
    }

    pub fn start_pan(&mut self, position: Point<Pixels>) {
        self.pan_drag = Some(PanDrag {
            start_position: position,
            start_offset: self.scroll_handle.offset(),
        });
    }

    /// Scrolls so the canvas follows the mouse during a pan.
    pub fn drag_pan(&mut self, drag: PanDrag, position: Point<Pixels>, cx: &mut Context<Self>) {
        let offset = drag.start_offset + (position - drag.start_position);
        // Offsets run from zero down to minus the most the viewport can scroll
        let max = self.scroll_handle.max_offset();
        self.scroll_handle.set_offset(point(
            offset.x.clamp(-max.x, px(0.0)),
            offset.y.clamp(-max.y, px(0.0)),
        ));
        cx.notify();
    }

    /// The mouse cursor over the viewport while panning or ready to.
    pub fn pan_cursor(&self) -> Option<CursorStyle> {
        if self.pan_drag.is_some() {
            Some(CursorStyle::ClosedHand)
        } else if self.space_held {
            Some(CursorStyle::OpenHand)
        } else {
            None
        }
    }
}

/// Space on its own, as the modified keys are left for shortcuts.
fn is_space(keystroke: &Keystroke) -> bool {
    keystroke.key == "space" && !keystroke.modifiers.modified()
}
