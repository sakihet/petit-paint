use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, button::*, checkbox::Checkbox, h_flex,
    slider::Slider,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::canvas::{BRUSHES, Orientation};
use crate::text::FONTS;
use crate::tool::{SHAPES, Tool, brush_icon};
use crate::{
    CropToSelection, KEY_CONTEXT, NewImage, OpenImage, PaintApp, Redo, Save, Undo,
    tolerance_percent,
};

impl PaintApp {
    /// The current tool's options, which give way to the actions when the
    /// window is too narrow for both.
    fn tool_options(&self, cx: &Context<Self>) -> impl IntoElement {
        let uses_width = matches!(self.tool, Tool::Pen | Tool::Eraser | Tool::Shape);

        h_flex()
            .gap_3()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .when(self.tool == Tool::Pen, |this| {
                this.child(
                    ButtonGroup::new("brush")
                        .outline()
                        .children(BRUSHES.map(|brush| {
                            Button::new(brush.label())
                                .icon(brush_icon(brush))
                                .tooltip(brush.label())
                                .selected(self.brush == brush)
                        }))
                        .on_click(cx.listener(|this, clicks: &Vec<usize>, _, cx| {
                            if let Some(&brush) = clicks.first().and_then(|ix| BRUSHES.get(*ix)) {
                                this.brush = brush;
                                cx.notify();
                            }
                        })),
                )
            })
            .when(self.tool == Tool::Shape, |this| {
                this.child(
                    ButtonGroup::new("shape")
                        .outline()
                        .children(SHAPES.map(|shape| {
                            Button::new(shape.label())
                                .icon(shape.icon())
                                .tooltip(shape.label())
                                .selected(self.shape_kind == shape)
                        }))
                        .on_click(cx.listener(|this, clicks: &Vec<usize>, _, cx| {
                            if let Some(&shape) = clicks.first().and_then(|ix| SHAPES.get(*ix)) {
                                this.cancel_polygon(cx);
                                this.shape_kind = shape;
                                cx.notify();
                            }
                        })),
                )
                .child(
                    Checkbox::new("shape-filled")
                        .label("Filled")
                        .checked(self.shape_filled)
                        .disabled(!self.shape_kind.fillable())
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.shape_filled = *checked;
                            cx.notify();
                        })),
                )
            })
            .when(self.tool == Tool::Text, |this| {
                this.child(
                    ButtonGroup::new("font")
                        .outline()
                        .children(FONTS.map(|font| {
                            Button::new(font.label())
                                .label(font.label())
                                .selected(self.font == font)
                        }))
                        .on_click(cx.listener(|this, clicks: &Vec<usize>, _, cx| {
                            if let Some(&font) = clicks.first().and_then(|ix| FONTS.get(*ix)) {
                                this.font = font;
                                this.update_text_preview(cx);
                            }
                        })),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child("Size")
                        .child(
                            div()
                                .w(px(100.0))
                                .child(Slider::new(&self.font_size_slider)),
                        )
                        .child(div().w(px(28.0)).child(format!("{}", self.font_size))),
                )
            })
            .when(self.tool == Tool::Fill, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child("Tolerance")
                        .child(
                            div()
                                .w(px(120.0))
                                .child(Slider::new(&self.fill_tolerance_slider)),
                        )
                        .child(
                            div()
                                .w(px(36.0))
                                .child(format!("{}%", tolerance_percent(self.fill_tolerance))),
                        ),
                )
            })
            .when(uses_width, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child("Width")
                        .child(div().w(px(120.0)).child(Slider::new(&self.width_slider)))
                        .child(div().w(px(24.0)).child(format!("{}", self.width.as_f32()))),
                )
            })
    }

    /// Tool options on the left, actions on the right.
    pub(super) fn options_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let icon_button = |id: &'static str, icon: IconName| Button::new(id).ghost().icon(icon);
        let separator = || div().w(px(1.0)).h(px(20.0)).mx_1().bg(theme.border);

        h_flex()
            .gap_3()
            .px_3()
            .h(px(44.0))
            .flex_none()
            .bg(theme.background)
            .border_b_1()
            .border_color(theme.border)
            .child(self.tool_options(cx))
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .child(
                        icon_button("undo", IconName::Undo2)
                            .tooltip_with_action("Undo", &Undo, Some(KEY_CONTEXT))
                            .disabled(self.undo_stack.is_empty())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.undo(&Undo, window, cx)),
                            ),
                    )
                    .child(
                        icon_button("redo", IconName::Redo2)
                            .tooltip_with_action("Redo", &Redo, Some(KEY_CONTEXT))
                            .disabled(self.redo_stack.is_empty())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.redo(&Redo, window, cx)),
                            ),
                    )
                    .child(separator())
                    .children(
                        [
                            (
                                "flip-horizontal",
                                IconName::FlipHorizontal2,
                                "Flip left to right",
                                Orientation::FlipHorizontal,
                            ),
                            (
                                "flip-vertical",
                                IconName::FlipVertical2,
                                "Flip top to bottom",
                                Orientation::FlipVertical,
                            ),
                            (
                                "rotate-left",
                                IconName::RotateCcw,
                                "Turn counterclockwise",
                                Orientation::RotateCounterClockwise,
                            ),
                            (
                                "rotate-right",
                                IconName::RotateCw,
                                "Turn clockwise",
                                Orientation::RotateClockwise,
                            ),
                        ]
                        .map(|(id, icon, tooltip, orientation)| {
                            icon_button(id, icon)
                                .tooltip(format!("{tooltip}: the selection, or the whole image"))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.reorient(orientation, cx)
                                    }),
                                )
                        }),
                    )
                    .child(separator())
                    .child(
                        icon_button("crop", IconName::Crop)
                            .tooltip_with_action(
                                "Crop to the selection",
                                &CropToSelection,
                                Some(KEY_CONTEXT),
                            )
                            .disabled(self.selection.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.crop_to_selection(&CropToSelection, window, cx)
                            })),
                    )
                    .child(
                        icon_button("canvas-size", IconName::Scaling)
                            .tooltip("Canvas size")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_canvas_size_dialog(window, cx)
                            })),
                    )
                    .child(
                        icon_button("clear", IconName::Trash)
                            .tooltip("Clear the canvas")
                            .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                    )
                    .child(separator())
                    .child(
                        icon_button("new", IconName::FilePlus)
                            .tooltip_with_action("New image", &NewImage, Some(KEY_CONTEXT))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_image(&NewImage, window, cx)
                            })),
                    )
                    .child(
                        icon_button("open", IconName::FolderOpen)
                            .tooltip_with_action("Open an image", &OpenImage, Some(KEY_CONTEXT))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_image(&OpenImage, window, cx)
                            })),
                    )
                    .child(
                        icon_button("save", IconName::Save)
                            .tooltip_with_action("Save", &Save, Some(KEY_CONTEXT))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.on_save(&Save, window, cx)),
                            ),
                    ),
            )
    }
}
