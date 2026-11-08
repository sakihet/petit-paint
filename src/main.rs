use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, h_flex,
    button::*,
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    slider::{Slider, SliderEvent, SliderState, SliderValue},
    v_flex,
};
use gpui_kit::component::{
    WindowExt as _,
    dialog::DialogFooter,
    input::{InputState, NumberInput},
    notification::Notification,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use canvas::{Canvas, Change, Floating, PixelRect};
use image::{Frame, RgbaImage};
use lyon::tessellation::{LineCap, LineJoin};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use tiny_skia::Pixmap;

mod canvas;

actions!(
    petit_paint,
    [
        Undo,
        Redo,
        SavePng,
        SelectAll,
        CopySelection,
        CutSelection,
        Paste,
        DeleteSelection,
        Deselect,
    ]
);

const DEFAULT_WIDTH: f32 = 4.0;
const DEFAULT_CANVAS_SIZE: Size<Pixels> = Size {
    width: px(800.0),
    height: px(600.0),
};
/// Largest canvas side in pixels. Undo steps that replace the whole canvas
/// (clear, resize) keep a full copy of it, 64MB each at this size.
const MAX_CANVAS_SIDE: u32 = 4096;
/// Number of undo steps kept; older ones are dropped.
const MAX_HISTORY: usize = 100;
/// Side length of the square handles for resizing the canvas.
const HANDLE_SIZE: Pixels = px(8.0);
const KEY_CONTEXT: &str = "PaintApp";
const CANVAS_BACKGROUND: u32 = 0xffffff;
const SELECTION_COLOR: u32 = 0x0078d7;

// KeyBinding::new takes a &'static str and cannot resolve "secondary-", so
// pick the modifier per OS at compile time
#[cfg(target_os = "macos")]
macro_rules! with_modifier {
    ($key:literal) => {
        concat!("cmd-", $key)
    };
}
#[cfg(not(target_os = "macos"))]
macro_rules! with_modifier {
    ($key:literal) => {
        concat!("ctrl-", $key)
    };
}

/// A single brush stroke. Points are in canvas coordinates, with the origin at its top-left.
///
/// While the mouse is held the stroke is previewed as a vector path; on
/// release it is burned into the canvas pixels.
#[derive(Clone)]
struct Stroke {
    points: Vec<Point<Pixels>>,
    color: Hsla,
    width: Pixels,
}

/// The selected rectangle, in canvas pixels.
struct Selection {
    rect: PixelRect,
    /// Pixels lifted from the selection or pasted into it, drawn above the
    /// canvas until dropped.
    floating: Option<Floating>,
}

/// An in-progress drag with the select tool.
#[derive(Clone, Copy)]
enum SelectDrag {
    /// Dragging out a new selection from `anchor`.
    Marquee { anchor: Point<i32> },
    /// Moving the selection; `grab` is the mouse offset from its top-left.
    Move { grab: Point<i32> },
}

/// What the cached view image was rendered from.
#[derive(Clone, Copy, PartialEq)]
struct ViewKey {
    version: u64,
    scale: u32,
    origin: Point<u32>,
    size: Size<u32>,
}

/// The visible part of the canvas as a GPU image, re-rendered only when the
/// canvas changes or a different part of it becomes visible.
#[derive(Default)]
struct ViewCache {
    key: Option<ViewKey>,
    image: Option<Arc<RenderImage>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    /// Selects a rectangle to move, copy, cut or delete.
    Select,
    Pen,
    /// Paints with the canvas background color, so erasing is undoable like any stroke.
    Eraser,
    /// Picks the brush color from the canvas, then switches back to the pen.
    Eyedropper,
    /// Fills the region of matching color under the cursor with the brush color.
    Fill,
}

/// Tools in toolbar order.
const TOOLS: [Tool; 5] = [
    Tool::Select,
    Tool::Pen,
    Tool::Eraser,
    Tool::Eyedropper,
    Tool::Fill,
];

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Pen => "Pen",
            Tool::Eraser => "Eraser",
            Tool::Eyedropper => "Eyedropper",
            Tool::Fill => "Fill",
        }
    }
}

/// A handle on the canvas edge that resizes it when dragged.
#[derive(Clone, Copy, PartialEq)]
enum ResizeHandle {
    Right,
    Bottom,
    Corner,
}

impl ResizeHandle {
    /// Whether dragging this handle changes the width and the height.
    fn axes(self) -> (bool, bool) {
        match self {
            ResizeHandle::Right => (true, false),
            ResizeHandle::Bottom => (false, true),
            ResizeHandle::Corner => (true, true),
        }
    }

    fn cursor(self) -> CursorStyle {
        match self {
            ResizeHandle::Right => CursorStyle::ResizeLeftRight,
            ResizeHandle::Bottom => CursorStyle::ResizeUpDown,
            ResizeHandle::Corner => CursorStyle::ResizeUpLeftDownRight,
        }
    }
}

/// An in-progress drag of a resize handle.
struct ResizeDrag {
    handle: ResizeHandle,
    /// Mouse position in window coordinates when the drag started.
    start_position: Point<Pixels>,
    start_size: Size<Pixels>,
}

struct PaintApp {
    tool: Tool,
    canvas: Canvas,
    undo_stack: Vec<Change>,
    redo_stack: Vec<Change>,
    /// Displayed canvas size. Matches `canvas` except while a resize handle is dragged.
    canvas_size: Size<Pixels>,
    /// The stroke being drawn, not yet burned into `canvas`.
    current_stroke: Option<Stroke>,
    view_cache: Rc<RefCell<ViewCache>>,
    selection: Option<Selection>,
    select_drag: Option<SelectDrag>,
    /// Bumped whenever new floating pixels are created, to refresh their cached image.
    floating_id: u64,
    floating_cache: Rc<RefCell<ViewCache>>,
    /// The part of the canvas inside the viewport, in window coordinates from
    /// the last paint. Pasted pixels land at its top-left.
    visible_bounds: Rc<Cell<Bounds<Pixels>>>,
    resize_drag: Option<ResizeDrag>,
    /// Canvas pixel under the mouse, shown in the status bar.
    cursor_position: Option<Point<i32>>,
    focus_handle: FocusHandle,
    /// Canvas bounds in window coordinates from the last paint, used to map
    /// mouse positions to canvas coordinates.
    canvas_bounds: Rc<Cell<Bounds<Pixels>>>,
    color: Hsla,
    width: Pixels,
    color_picker: Entity<ColorPickerState>,
    width_slider: Entity<SliderState>,
    _subscriptions: Vec<Subscription>,
}

impl PaintApp {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let color: Hsla = rgb(0x222222).into();
        let color_picker = cx.new(|cx| ColorPickerState::new(window, cx).default_value(color));
        let width_slider = cx.new(|_| {
            SliderState::new()
                .min(1.0)
                .max(32.0)
                .step(1.0)
                .default_value(DEFAULT_WIDTH)
        });

        let _subscriptions = vec![
            cx.subscribe_in(&color_picker, window, |this, _, event, _, cx| {
                let ColorPickerEvent::Change(Some(color)) = event else {
                    return;
                };
                this.color = *color;
                cx.notify();
            }),
            cx.subscribe_in(&width_slider, window, |this, _, event, _, cx| {
                let (SliderEvent::Change(SliderValue::Single(width))
                | SliderEvent::Release(SliderValue::Single(width))) = event
                else {
                    return;
                };
                this.width = px(*width);
                cx.notify();
            }),
        ];

        let canvas = Canvas::new(
            DEFAULT_CANVAS_SIZE.width.as_f32() as u32,
            DEFAULT_CANVAS_SIZE.height.as_f32() as u32,
            rgb(CANVAS_BACKGROUND).into(),
        )
        .expect("default canvas size is valid");

        Self {
            canvas,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            canvas_size: DEFAULT_CANVAS_SIZE,
            current_stroke: None,
            view_cache: Rc::default(),
            selection: None,
            select_drag: None,
            floating_id: 0,
            floating_cache: Rc::default(),
            visible_bounds: Rc::default(),
            resize_drag: None,
            cursor_position: None,
            tool: Tool::Pen,
            focus_handle: cx.focus_handle(),
            canvas_bounds: Rc::new(Cell::new(Bounds::default())),
            color,
            width: px(DEFAULT_WIDTH),
            color_picker,
            width_slider,
            _subscriptions,
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Take focus back from toolbar inputs so shortcuts reach the canvas
        window.focus(&self.focus_handle, cx);
        if self.tool == Tool::Select {
            self.start_select(event.position, cx);
            return;
        }
        if !self.canvas_bounds.get().contains(&event.position) {
            return;
        }
        let position = self.to_canvas(event.position);
        let color = match self.tool {
            Tool::Select => return,
            Tool::Pen => self.color,
            Tool::Eraser => rgb(CANVAS_BACKGROUND).into(),
            Tool::Eyedropper => {
                self.pick_color(position, window, cx);
                return;
            }
            Tool::Fill => {
                self.fill_at(position, cx);
                return;
            }
        };
        self.current_stroke = Some(Stroke {
            points: vec![position],
            color,
            width: self.width,
        });
        cx.notify();
    }

    fn to_canvas(&self, position: Point<Pixels>) -> Point<Pixels> {
        position - self.canvas_bounds.get().origin
    }

    /// The canvas pixel at `position` in window coordinates, which may lie
    /// outside the canvas.
    fn canvas_pixel(&self, position: Point<Pixels>) -> Point<i32> {
        let position = self.to_canvas(position);
        point(
            position.x.as_f32().floor() as i32,
            position.y.as_f32().floor() as i32,
        )
    }

    fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        if tool != Tool::Select {
            self.deselect(cx);
        }
        self.tool = tool;
        cx.notify();
    }

    fn start_select(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let pixel = self.canvas_pixel(position);
        if let Some(selection) = &mut self.selection
            && selection.rect.contains(pixel)
        {
            if selection.floating.is_none() {
                selection.floating = self.canvas.lift(selection.rect);
                self.floating_id += 1;
            }
            let grab = point(pixel.x - selection.rect.x, pixel.y - selection.rect.y);
            self.select_drag = Some(SelectDrag::Move { grab });
            cx.notify();
            return;
        }
        self.deselect(cx);
        if self.canvas_bounds.get().contains(&position) {
            self.select_drag = Some(SelectDrag::Marquee { anchor: pixel });
        }
    }

    fn drag_select(&mut self, drag: SelectDrag, position: Point<Pixels>, cx: &mut Context<Self>) {
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
                self.selection = (rect.width > 0 && rect.height > 0).then_some(Selection {
                    rect,
                    floating: None,
                });
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

    /// Drops floating pixels onto the canvas, recording the move or paste as
    /// one undo step. The selection stays.
    fn commit_selection(&mut self, cx: &mut Context<Self>) {
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

    fn deselect(&mut self, cx: &mut Context<Self>) {
        self.commit_selection(cx);
        if self.selection.take().is_some() {
            cx.notify();
        }
    }

    fn on_deselect(&mut self, _: &Deselect, _: &mut Window, cx: &mut Context<Self>) {
        self.deselect(cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.deselect(cx);
        let size = self.canvas.size();
        self.selection = Some(Selection {
            rect: PixelRect {
                x: 0,
                y: 0,
                width: size.width.as_f32() as u32,
                height: size.height.as_f32() as u32,
            },
            floating: None,
        });
        self.tool = Tool::Select;
        cx.notify();
    }

    fn selected_pixels(&self) -> Option<Pixmap> {
        let selection = self.selection.as_ref()?;
        match &selection.floating {
            Some(floating) => Some(floating.pixels.clone()),
            None => self.canvas.copy(selection.rect),
        }
    }

    /// Copies the selection to the system clipboard as a PNG image, so it can
    /// also be pasted into other apps.
    fn copy_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        let Some(png) = self.selected_pixels().and_then(|pixels| pixels.encode_png().ok()) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
            ImageFormat::Png,
            png,
        )));
    }

    fn cut_selection(&mut self, _: &CutSelection, window: &mut Window, cx: &mut Context<Self>) {
        self.copy_selection(&CopySelection, window, cx);
        self.delete_selection(&DeleteSelection, window, cx);
    }

    fn delete_selection(&mut self, _: &DeleteSelection, _: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.take() else {
            return;
        };
        let undo = match selection.floating {
            // Lifted pixels already left background behind; discarding them deletes them
            Some(floating) => {
                let at = point(selection.rect.x, selection.rect.y);
                self.canvas.drop_floating(floating, at, false)
            }
            None => self.canvas.erase(selection.rect),
        };
        if let Some(undo) = undo {
            self.record(undo, cx);
        }
        cx.notify();
    }

    /// Pastes a PNG image from the system clipboard as floating pixels at the
    /// top-left of the visible part of the canvas.
    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
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
        });
        self.floating_id += 1;
        self.tool = Tool::Select;
        cx.notify();
    }

    /// Records `undo`, the change that reverts an edit just made to the canvas.
    fn record(&mut self, undo: Change, cx: &mut Context<Self>) {
        self.undo_stack.push(undo);
        if self.undo_stack.len() > MAX_HISTORY {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let cursor_position = self
            .canvas_bounds
            .get()
            .contains(&event.position)
            .then(|| {
                let position = self.to_canvas(event.position);
                point(
                    position.x.as_f32().floor() as i32,
                    position.y.as_f32().floor() as i32,
                )
            });
        // Only re-render when the pixel changes, not on every sub-pixel move
        if cursor_position != self.cursor_position {
            self.cursor_position = cursor_position;
            cx.notify();
        }

        if let Some(drag) = self.select_drag {
            self.drag_select(drag, event.position, cx);
            return;
        }
        if let Some(drag) = &self.resize_drag {
            let delta = event.position - drag.start_position;
            let (resize_width, resize_height) = drag.handle.axes();
            let resize = |side: Pixels, delta: Pixels, enabled: bool| {
                if !enabled {
                    return side;
                }
                let side = (side + delta).as_f32().round();
                px(side.clamp(1.0, MAX_CANVAS_SIDE as f32))
            };
            // The canvas follows the drag; history is recorded once on release
            self.canvas_size = size(
                resize(drag.start_size.width, delta.x, resize_width),
                resize(drag.start_size.height, delta.y, resize_height),
            );
            cx.notify();
            return;
        }
        let position = self.to_canvas(event.position);
        if let Some(stroke) = &mut self.current_stroke {
            stroke.points.push(position);
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.select_drag = None;
        if let Some(stroke) = self.current_stroke.take()
            && let Some(undo) = self.canvas.draw_stroke(&stroke)
        {
            self.record(undo, cx);
        }
        if self.resize_drag.take().is_some() {
            self.resize_canvas(self.canvas_size, cx);
        }
        cx.notify();
    }

    fn start_resize(
        &mut self,
        handle: ResizeHandle,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        // Keep the viewport from treating this press as the start of a stroke
        cx.stop_propagation();
        self.resize_drag = Some(ResizeDrag {
            handle,
            start_position: event.position,
            start_size: self.canvas_size,
        });
    }

    fn pick_color(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(color) = self.canvas.color_at(position) else {
            return;
        };
        self.color = color;
        // set_value does not emit a change event, so self.color is updated above
        self.color_picker
            .update(cx, |state, cx| state.set_value(color, window, cx));
        self.tool = Tool::Pen;
        cx.notify();
    }

    fn fill_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if let Some(undo) = self.canvas.flood_fill(position, self.color) {
            self.record(undo, cx);
        }
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.deselect(cx);
        if let Ok(undo) = self.canvas.clear() {
            self.record(undo, cx);
        }
    }

    /// Whether a mouse drag is in progress, during which undo and redo would
    /// conflict with the stroke or resize being made.
    fn dragging(&self) -> bool {
        self.current_stroke.is_some() || self.resize_drag.is_some() || self.select_drag.is_some()
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if self.dragging() {
            return;
        }
        // Dropping floating pixels records their move, which this then undoes
        self.deselect(cx);
        let Some(change) = self.undo_stack.pop() else {
            return;
        };
        self.redo_stack.push(self.canvas.apply(change));
        self.canvas_size = self.canvas.size();
        cx.notify();
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if self.dragging() {
            return;
        }
        self.deselect(cx);
        let Some(change) = self.redo_stack.pop() else {
            return;
        };
        self.undo_stack.push(self.canvas.apply(change));
        self.canvas_size = self.canvas.size();
        cx.notify();
    }

    /// Resizes the canvas, keeping the drawing anchored at the top-left.
    /// Growing adds background; shrinking crops, which undo brings back.
    fn resize_canvas(&mut self, size: Size<Pixels>, cx: &mut Context<Self>) {
        self.deselect(cx);
        if size != self.canvas.size()
            && let Ok(undo) = self.canvas.resize(
                size.width.as_f32() as u32,
                size.height.as_f32() as u32,
            )
        {
            self.record(undo, cx);
        }
        self.canvas_size = self.canvas.size();
        cx.notify();
    }

    fn open_canvas_size_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                this.update(cx, |this, cx| this.resize_canvas(size, cx)).ok();
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
                        .child(
                            Button::new("ok")
                                .primary()
                                .label("OK")
                                .on_click(move |_, window, cx| {
                                    if ok(window, cx) {
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
        });
    }

    fn save_png(&mut self, _: &SavePng, window: &mut Window, cx: &mut Context<Self>) {
        // Include floating pixels, and encode now so the image reflects the
        // drawing at the time of the request
        self.commit_selection(cx);
        let png = self.canvas.encode_png();
        let directory = std::env::home_dir().unwrap_or_default();
        let path = cx.prompt_for_new_path(&directory, Some("untitled.png"));

        cx.spawn_in(window, async move |_, cx| {
            let result = async {
                let Some(path) = path.await?? else {
                    return Ok(None);
                };
                std::fs::write(&path, png?)?;
                anyhow::Ok(Some(path))
            }
            .await;

            let notification = match result {
                Ok(Some(path)) => Notification::success(format!("Saved {}", path.display())),
                Ok(None) => return,
                Err(err) => Notification::error(format!("Failed to save PNG: {err:#}")),
            };
            cx.update(|window, cx| window.push_notification(notification, cx))
                .ok();
        })
        .detach();
    }
}

impl Render for PaintApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        let toolbar = h_flex()
            // Wrap instead of clipping buttons when the window is narrow
            .flex_wrap()
            .gap_3()
            .px_3()
            .py_1p5()
            .min_h(px(44.0))
            .bg(theme.background)
            .border_b_1()
            .border_color(theme.border)
            .child("petit-paint")
            .child(
                ButtonGroup::new("tool")
                    .outline()
                    .children(TOOLS.map(|tool| {
                        Button::new(tool.label())
                            .label(tool.label())
                            .selected(self.tool == tool)
                    }))
                    .on_click(cx.listener(|this, clicks: &Vec<usize>, _, cx| {
                        if let Some(&tool) = clicks.first().and_then(|ix| TOOLS.get(*ix)) {
                            this.set_tool(tool, cx);
                        }
                    })),
            )
            .child(ColorPicker::new(&self.color_picker))
            .child(
                h_flex()
                    .gap_2()
                    .child("Width")
                    .child(div().w(px(160.0)).child(Slider::new(&self.width_slider)))
                    .child(div().w(px(24.0)).child(format!("{}", self.width.as_f32()))),
            )
            .child(
                Button::new("undo")
                    .outline()
                    .label("Undo")
                    .tooltip_with_action("Undo", &Undo, Some(KEY_CONTEXT))
                    .disabled(self.undo_stack.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.undo(&Undo, window, cx))),
            )
            .child(
                Button::new("redo")
                    .outline()
                    .label("Redo")
                    .tooltip_with_action("Redo", &Redo, Some(KEY_CONTEXT))
                    .disabled(self.redo_stack.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.redo(&Redo, window, cx))),
            )
            .child(
                Button::new("clear")
                    .outline()
                    .label("Clear")
                    .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
            )
            .child(
                Button::new("canvas-size")
                    .outline()
                    .label("Canvas Size")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_canvas_size_dialog(window, cx)
                    })),
            )
            .child(
                Button::new("save")
                    .outline()
                    .label("Save PNG")
                    .tooltip_with_action("Save as PNG", &SavePng, Some(KEY_CONTEXT))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.save_png(&SavePng, window, cx)),
                    ),
            );

        let canvas_bounds = self.canvas_bounds.clone();
        let visible_bounds = self.visible_bounds.clone();
        let view_cache = self.view_cache.clone();
        let floating_cache = self.floating_cache.clone();
        let preview = self.current_stroke.clone();
        let app = cx.entity();

        let canvas_size = self.canvas_size;
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
            .w(self.canvas_size.width)
            .h(self.canvas_size.height)
            .flex_none()
            .bg(rgb(CANVAS_BACKGROUND))
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
                            paint_view(window, &app.canvas, bounds, &mut view_cache.borrow_mut());
                            if let Some(selection) = &app.selection
                                && let Some(floating) = &selection.floating
                            {
                                let rect = selection.rect;
                                let at = bounds.origin
                                    + point(px(rect.x as f32), px(rect.y as f32));
                                paint_floating(
                                    window,
                                    &floating.pixels,
                                    app.floating_id,
                                    at,
                                    &mut floating_cache.borrow_mut(),
                                );
                            }
                            if let Some(stroke) = &preview {
                                paint_stroke(window, stroke, bounds.origin);
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
                        .left(px(rect.x as f32))
                        .top(px(rect.y as f32))
                        .w(px(rect.width as f32))
                        .h(px(rect.height as f32))
                        .border_1()
                        .border_dashed()
                        .border_color(rgb(SELECTION_COLOR)),
                )
            });

        // Mouse handlers sit on the viewport so a drag keeps drawing when it
        // leaves the canvas
        let viewport = div()
            .id("viewport")
            .flex()
            .items_start()
            .flex_1()
            .min_h_0()
            .p_3()
            .overflow_scroll()
            .bg(theme.muted)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered && this.cursor_position.take().is_some() {
                    cx.notify();
                }
            }))
            .child(canvas_area);

        let status_bar = h_flex()
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
            });

        v_flex()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::save_png))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy_selection))
            .on_action(cx.listener(Self::cut_selection))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::delete_selection))
            .on_action(cx.listener(Self::on_deselect))
            .size_full()
            .text_color(theme.foreground)
            .child(toolbar)
            .child(viewport)
            .child(status_bar)
    }
}

/// Paints `stroke` with the canvas top-left at `origin`, in window coordinates.
fn paint_stroke(window: &mut Window, stroke: &Stroke, origin: Point<Pixels>) {
    let Stroke {
        points,
        color,
        width,
    } = stroke;
    let Some(first) = points.first() else {
        return;
    };
    // Round caps turn a single click into a dot; round joins avoid miter spikes on sharp turns
    let options = StrokeOptions::default()
        .with_line_width(width.as_f32())
        .with_line_cap(LineCap::Round)
        .with_line_join(LineJoin::Round);
    let mut builder = PathBuilder::default().with_style(PathStyle::Stroke(options));
    builder.move_to(origin + *first);
    if points.len() == 1 {
        // Draw a tiny segment so a single click still leaves a visible dot
        builder.line_to(origin + *first + Point::new(px(0.1), px(0.1)));
    } else {
        for p in &points[1..] {
            builder.line_to(origin + *p);
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, *color);
    }
}

/// Paints floating selection pixels with their top-left at `at`, in window
/// coordinates, sampled like the canvas view so they stay sharp.
fn paint_floating(
    window: &mut Window,
    pixels: &Pixmap,
    id: u64,
    at: Point<Pixels>,
    cache: &mut ViewCache,
) {
    let scale = window.scale_factor();
    let key = ViewKey {
        version: id,
        scale: scale.to_bits(),
        origin: point(0, 0),
        size: size(pixels.width(), pixels.height()),
    };
    if cache.key != Some(key) {
        let (bytes, region) = canvas::render_pixmap(pixels, scale);
        let Some(frame) = RgbaImage::from_raw(region.width, region.height, bytes) else {
            return;
        };
        let image = Arc::new(RenderImage::new(vec![Frame::new(frame)]));
        if let Some(previous) = cache.image.replace(image) {
            window.drop_image(previous).ok();
        }
        cache.key = Some(key);
    }
    let Some(image) = cache.image.clone() else {
        return;
    };
    // Snap to device pixels so sampling stays 1:1
    let snap = |pixels: Pixels| px((pixels.as_f32() * scale).round() / scale);
    let image_size = image.size(0);
    let image_bounds = Bounds::new(
        point(snap(at.x), snap(at.y)),
        size(
            px(image_size.width.0 as f32 / scale),
            px(image_size.height.0 as f32 / scale),
        ),
    );
    window
        .paint_image(
            image_bounds,
            image_bounds,
            Corners::default(),
            image,
            0,
            false,
        )
        .ok();
}

/// Paints the visible part of `canvas`, displayed at `bounds` in window coordinates.
///
/// gpui samples images with linear filtering, which would blur pixels when
/// the canvas is scaled up. So the view is rendered on the CPU at exactly the
/// device resolution and drawn 1:1.
fn paint_view(window: &mut Window, canvas: &Canvas, bounds: Bounds<Pixels>, cache: &mut ViewCache) {
    let scale = window.scale_factor();
    let visible = bounds.intersect(&window.content_mask().bounds);
    if visible.size.width <= px(0.0) || visible.size.height <= px(0.0) {
        return;
    }
    // Device pixels relative to the canvas's top-left, snapped to whole pixels
    let to_device = |pixels: Pixels| pixels.as_f32() * scale;
    let canvas_x = to_device(bounds.origin.x).round();
    let canvas_y = to_device(bounds.origin.y).round();
    let left = (to_device(visible.origin.x).floor() - canvas_x).max(0.0);
    let top = (to_device(visible.origin.y).floor() - canvas_y).max(0.0);
    let right = to_device(visible.origin.x + visible.size.width).ceil() - canvas_x;
    let bottom = to_device(visible.origin.y + visible.size.height).ceil() - canvas_y;
    if right <= left || bottom <= top {
        return;
    }
    let key = ViewKey {
        version: canvas.version(),
        scale: scale.to_bits(),
        origin: point(left as u32, top as u32),
        size: size((right - left) as u32, (bottom - top) as u32),
    };

    if cache.key != Some(key) {
        let bytes = canvas.render_view(key.origin, key.size, scale);
        let Some(frame) = RgbaImage::from_raw(key.size.width, key.size.height, bytes) else {
            return;
        };
        let image = Arc::new(RenderImage::new(vec![Frame::new(frame)]));
        // Free the previous view's GPU texture
        if let Some(previous) = cache.image.replace(image) {
            window.drop_image(previous).ok();
        }
        cache.key = Some(key);
    }
    let Some(image) = cache.image.clone() else {
        return;
    };

    let image_bounds = Bounds::new(
        point(px((canvas_x + left) / scale), px((canvas_y + top) / scale)),
        size(px((right - left) / scale), px((bottom - top) / scale)),
    );
    window
        .paint_image(
            image_bounds,
            image_bounds,
            Corners::default(),
            image,
            0,
            false,
        )
        .ok();
}

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        let context = Some(KEY_CONTEXT);
        let mut bindings = vec![
            KeyBinding::new(with_modifier!("z"), Undo, context),
            KeyBinding::new(with_modifier!("shift-z"), Redo, context),
            KeyBinding::new(with_modifier!("s"), SavePng, context),
            KeyBinding::new(with_modifier!("a"), SelectAll, context),
            KeyBinding::new(with_modifier!("c"), CopySelection, context),
            KeyBinding::new(with_modifier!("x"), CutSelection, context),
            KeyBinding::new(with_modifier!("v"), Paste, context),
            KeyBinding::new("backspace", DeleteSelection, context),
            KeyBinding::new("delete", DeleteSelection, context),
            KeyBinding::new("escape", Deselect, context),
        ];
        // Windows and Linux apps also redo with Ctrl+Y
        if cfg!(not(target_os = "macos")) {
            bindings.push(KeyBinding::new("ctrl-y", Redo, context));
        }
        cx.bind_keys(bindings);
        let bounds = Bounds::centered(None, size(px(960.0), px(640.0)), cx);
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let app = cx.new(|cx| PaintApp::new(window, cx));
                let focus_handle = app.read(cx).focus_handle.clone();
                window.focus(&focus_handle, cx);
                app
            },
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
