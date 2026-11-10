use gpui_kit::*;
use tiny_skia::Pixmap;

use gpui_kit::assets::IconName;

use crate::canvas::{Floating, PixelRect, SelectionMask};
use crate::extend_stroke;
use crate::tool::Tool;
use crate::{
    CopySelection, CropToSelection, CutSelection, DeleteSelection, Deselect, PaintApp, Paste,
    SelectAll,
};

/// The selected rectangle, in canvas pixels.
pub struct Selection {
    pub rect: PixelRect,
    /// Pixels lifted from the selection or pasted into it, drawn above the
    /// canvas until dropped.
    pub floating: Option<Floating>,
    /// The pixels within `rect` a free-form selection covers. A rectangular
    /// selection has none and covers all of `rect`.
    pub mask: Option<SelectionMask>,
}

impl Selection {
    fn rectangle(rect: PixelRect) -> Self {
        Self {
            rect,
            floating: None,
            mask: None,
        }
    }

    /// Whether the canvas pixel `pixel` is selected.
    pub fn contains(&self, pixel: Point<i32>) -> bool {
        self.rect.contains(pixel)
            && self
                .mask
                .as_ref()
                .is_none_or(|mask| mask.contains(pixel.x - self.rect.x, pixel.y - self.rect.y))
    }
}

/// How the select tool picks out pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SelectShape {
    Rectangle,
    /// Inside an outline dragged freehand.
    FreeForm,
}

/// Selection shapes in toolbar order.
pub const SELECT_SHAPES: [SelectShape; 2] = [SelectShape::Rectangle, SelectShape::FreeForm];

impl SelectShape {
    pub fn label(self) -> &'static str {
        match self {
            SelectShape::Rectangle => "Rectangle",
            SelectShape::FreeForm => "Free-form",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            SelectShape::Rectangle => IconName::SquareDashed,
            SelectShape::FreeForm => IconName::Lasso,
        }
    }
}

/// An in-progress drag with the select tool.
#[derive(Clone, Copy)]
pub enum SelectDrag {
    /// Dragging out a new selection from `anchor`.
    Marquee { anchor: Point<i32> },
    /// Dragging a free-form outline, whose points so far are in `lasso`.
    Lasso,
    /// Moving the selection; `grab` is the mouse offset from its top-left.
    Move { grab: Point<i32> },
}

impl PaintApp {
    pub fn start_select(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let pixel = self.canvas_pixel(position);
        if let Some(selection) = &mut self.selection
            && selection.contains(pixel)
        {
            if selection.floating.is_none() {
                selection.floating = self.canvas.lift(selection.rect, selection.mask.as_ref());
                self.floating_id += 1;
            }
            let grab = point(pixel.x - selection.rect.x, pixel.y - selection.rect.y);
            self.select_drag = Some(SelectDrag::Move { grab });
            cx.notify();
            return;
        }
        self.deselect(cx);
        if !self.canvas_bounds.get().contains(&position) {
            return;
        }
        self.select_drag = Some(match self.select_shape {
            SelectShape::Rectangle => SelectDrag::Marquee { anchor: pixel },
            SelectShape::FreeForm => {
                self.lasso = vec![self.to_canvas(position)];
                self.straight_from = None;
                SelectDrag::Lasso
            }
        });
    }

    /// Follows a select drag to `position`. With `straight` (Shift), a
    /// free-form outline's latest edge stays straight.
    pub fn drag_select(
        &mut self,
        drag: SelectDrag,
        position: Point<Pixels>,
        straight: bool,
        cx: &mut Context<Self>,
    ) {
        let pixel = self.canvas_pixel(position);
        match drag {
            SelectDrag::Marquee { anchor } => {
                let size = self.canvas.size();
                // Pixel edges, so the rectangle can reach the canvas's far edge
                let end = point(
                    pixel.x.clamp(0, size.width.as_f32() as i32),
                    pixel.y.clamp(0, size.height.as_f32() as i32),
                );
                let rect = PixelRect {
                    x: anchor.x.min(end.x),
                    y: anchor.y.min(end.y),
                    width: anchor.x.abs_diff(end.x),
                    height: anchor.y.abs_diff(end.y),
                };
                self.selection =
                    (rect.width > 0 && rect.height > 0).then(|| Selection::rectangle(rect));
            }
            SelectDrag::Lasso => {
                let position = self.to_canvas(position);
                extend_stroke(&mut self.lasso, &mut self.straight_from, position, straight);
            }
            SelectDrag::Move { grab } => {
                if let Some(selection) = &mut self.selection {
                    selection.rect.x = pixel.x - grab.x;
                    selection.rect.y = pixel.y - grab.y;
                }
            }
        }
        cx.notify();
    }

    /// Ends a select drag, closing a free-form outline into a selection.
    pub fn end_select(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.select_drag.take(), Some(SelectDrag::Lasso)) {
            return;
        }
        let outline = std::mem::take(&mut self.lasso);
        self.selection = self.lasso_selection(&outline);
        cx.notify();
    }

    /// The pixels inside `outline`, given in canvas coordinates, within the
    /// part of the canvas it spans.
    fn lasso_selection(&self, outline: &[Point<Pixels>]) -> Option<Selection> {
        let size = self.canvas.size();
        let (mut left, mut top) = (f32::MAX, f32::MAX);
        let (mut right, mut bottom) = (f32::MIN, f32::MIN);
        for p in outline {
            left = left.min(p.x.as_f32());
            top = top.min(p.y.as_f32());
            right = right.max(p.x.as_f32());
            bottom = bottom.max(p.y.as_f32());
        }
        let left = left.floor().max(0.0) as i32;
        let top = top.floor().max(0.0) as i32;
        let right = (right.ceil() as i32).min(size.width.as_f32() as i32);
        let bottom = (bottom.ceil() as i32).min(size.height.as_f32() as i32);
        if right <= left || bottom <= top {
            return None;
        }
        let rect = PixelRect {
            x: left,
            y: top,
            width: (right - left) as u32,
            height: (bottom - top) as u32,
        };
        let outline: Vec<_> = outline
            .iter()
            .map(|p| point(p.x.as_f32() - left as f32, p.y.as_f32() - top as f32))
            .collect();
        let mask = SelectionMask::from_outline(&outline, rect.width, rect.height)?;
        Some(Selection {
            rect,
            floating: None,
            mask: Some(mask),
        })
    }

    /// Drops floating pixels onto the canvas, recording the move or paste as
    /// one undo step. The selection stays.
    pub fn commit_selection(&mut self, cx: &mut Context<Self>) {
        let Some((floating, at)) = self.selection.as_mut().and_then(|selection| {
            let at = point(selection.rect.x, selection.rect.y);
            Some((selection.floating.take()?, at))
        }) else {
            return;
        };
        if let Some(undo) = self.canvas.drop_floating(floating, at, true) {
            self.record(undo, cx);
        }
        cx.notify();
    }

    pub fn deselect(&mut self, cx: &mut Context<Self>) {
        self.commit_selection(cx);
        if self.selection.take().is_some() {
            cx.notify();
        }
    }

    /// Escape: discards text being typed, cancels a polygon being built, or
    /// drops the selection.
    pub fn on_deselect(&mut self, _: &Deselect, window: &mut Window, cx: &mut Context<Self>) {
        if self.text_edit.take().is_some() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
            return;
        }
        if !self.polygon.is_empty() {
            self.cancel_polygon(cx);
            return;
        }
        self.deselect(cx);
    }

    pub fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.deselect(cx);
        let size = self.canvas.size();
        self.selection = Some(Selection::rectangle(PixelRect {
            x: 0,
            y: 0,
            width: size.width.as_f32() as u32,
            height: size.height.as_f32() as u32,
        }));
        self.tool = Tool::Select;
        cx.notify();
    }

    fn selected_pixels(&self) -> Option<Pixmap> {
        let selection = self.selection.as_ref()?;
        match &selection.floating {
            Some(floating) => Some(floating.pixels.clone()),
            None => self.canvas.copy(selection.rect, selection.mask.as_ref()),
        }
    }

    /// Copies the selection to the system clipboard as a PNG image, so it can
    /// also be pasted into other apps.
    pub fn copy_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        let Some(png) = self
            .selected_pixels()
            .and_then(|pixels| pixels.encode_png().ok())
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
            ImageFormat::Png,
            png,
        )));
    }

    pub fn cut_selection(&mut self, _: &CutSelection, window: &mut Window, cx: &mut Context<Self>) {
        self.copy_selection(&CopySelection, window, cx);
        self.delete_selection(&DeleteSelection, window, cx);
    }

    /// Shrinks the canvas to the selection, including any floating pixels,
    /// as one undo step.
    pub fn crop_to_selection(
        &mut self,
        _: &CropToSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dragging() {
            return;
        }
        self.commit_selection(cx);
        // A selection moved past the edge crops to the part still on the canvas
        let Some(pixels) = self
            .selection
            .as_ref()
            .and_then(|selection| self.canvas.copy(selection.rect, selection.mask.as_ref()))
        else {
            return;
        };
        self.selection = None;
        let undo = self.canvas.replace(pixels);
        self.record(undo, cx);
        self.canvas_size = self.canvas.size();
        cx.notify();
    }

    pub fn delete_selection(
        &mut self,
        _: &DeleteSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = self.selection.take() else {
            return;
        };
        let undo = match selection.floating {
            // Lifted pixels already left background behind; discarding them deletes them
            Some(floating) => {
                let at = point(selection.rect.x, selection.rect.y);
                self.canvas.drop_floating(floating, at, false)
            }
            None => self.canvas.erase(selection.rect, selection.mask.as_ref()),
        };
        if let Some(undo) = undo {
            self.record(undo, cx);
        }
        cx.notify();
    }

    /// Pastes a PNG image from the system clipboard as floating pixels at the
    /// top-left of the visible part of the canvas.
    pub fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(pixels) = item.entries().iter().find_map(|entry| match entry {
            ClipboardEntry::Image(image) if image.format == ImageFormat::Png => {
                Pixmap::decode_png(&image.bytes).ok()
            }
            _ => None,
        }) else {
            return;
        };
        self.deselect(cx);
        let visible = self.canvas_pixel(self.visible_bounds.get().origin);
        self.selection = Some(Selection {
            rect: PixelRect {
                x: visible.x.max(0),
                y: visible.y.max(0),
                width: pixels.width(),
                height: pixels.height(),
            },
            floating: Some(self.canvas.float(pixels)),
            mask: None,
        });
        self.floating_id += 1;
        self.tool = Tool::Select;
        cx.notify();
    }
}
