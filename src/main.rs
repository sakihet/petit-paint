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
use gpui_kit::*;
use lyon::tessellation::{LineCap, LineJoin};
use std::cell::Cell;
use std::rc::Rc;

mod raster;

actions!(petit_paint, [Undo, Redo, SavePng]);

const DEFAULT_WIDTH: f32 = 4.0;
const DEFAULT_CANVAS_SIZE: Size<Pixels> = Size {
    width: px(800.0),
    height: px(600.0),
};
/// Largest canvas side in pixels. Fill and eyedropper rasterize the canvas at
/// the screen's scale factor, so a 2x screen needs about 4 * 8192^2 bytes.
const MAX_CANVAS_SIDE: u32 = 4096;
/// Side length of the square handles for resizing the canvas.
const HANDLE_SIZE: Pixels = px(8.0);
const KEY_CONTEXT: &str = "PaintApp";
const CANVAS_BACKGROUND: u32 = 0xffffff;

// KeyBinding::new cannot resolve "secondary-", so pick the modifier per OS
#[cfg(target_os = "macos")]
const UNDO_KEYS: &[&str] = &["cmd-z"];
#[cfg(target_os = "macos")]
const REDO_KEYS: &[&str] = &["cmd-shift-z"];
#[cfg(target_os = "macos")]
const SAVE_KEY: &str = "cmd-s";
#[cfg(not(target_os = "macos"))]
const UNDO_KEYS: &[&str] = &["ctrl-z"];
#[cfg(not(target_os = "macos"))]
const REDO_KEYS: &[&str] = &["ctrl-shift-z", "ctrl-y"];
#[cfg(not(target_os = "macos"))]
const SAVE_KEY: &str = "ctrl-s";

/// A single brush stroke. Points are in canvas coordinates, with the origin at its top-left.
#[derive(Clone)]
struct Stroke {
    points: Vec<Point<Pixels>>,
    color: Hsla,
    width: Pixels,
}

/// A flood-filled region, stored as rectangles of device pixels.
#[derive(Clone)]
struct Fill {
    color: Hsla,
    /// Window scale factor when the fill was made; `rects` are in its device pixels.
    scale: f32,
    rects: Rc<Vec<raster::PixelRect>>,
}

impl Fill {
    /// Returns `rect` in canvas coordinates.
    fn bounds(&self, rect: &raster::PixelRect) -> Bounds<Pixels> {
        let to_pixels = |device: u32| px(device as f32 / self.scale);
        Bounds::new(
            point(to_pixels(rect.x), to_pixels(rect.y)),
            size(to_pixels(rect.width), to_pixels(rect.height)),
        )
    }
}

/// Something drawn on the canvas, painted in order.
#[derive(Clone)]
enum Item {
    Stroke(Stroke),
    Fill(Fill),
}

/// An undoable change to the drawing.
enum HistoryEntry {
    /// An item was appended to the end of `items`.
    Add,
    /// The canvas was cleared; holds the items that were removed.
    Clear(Vec<Item>),
    /// The canvas was resized; holds the size before the change.
    Resize(Size<Pixels>),
}

/// A change that was undone and can be reapplied.
enum RedoEntry {
    /// Holds the item that was removed by undo.
    Add(Item),
    /// Clears the canvas again.
    Clear,
    /// Holds the size to resize the canvas to again.
    Resize(Size<Pixels>),
}

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Pen,
    /// Paints with the canvas background color, so erasing is undoable like any stroke.
    Eraser,
    /// Picks the brush color from the canvas, then switches back to the pen.
    Eyedropper,
    /// Fills the region of matching color under the cursor with the brush color.
    Fill,
}

/// Tools in toolbar order.
const TOOLS: [Tool; 4] = [Tool::Pen, Tool::Eraser, Tool::Eyedropper, Tool::Fill];

impl Tool {
    fn label(self) -> &'static str {
        match self {
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
    items: Vec<Item>,
    history: Vec<HistoryEntry>,
    redo_stack: Vec<RedoEntry>,
    canvas_size: Size<Pixels>,
    drawing: bool,
    resize_drag: Option<ResizeDrag>,
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

        Self {
            items: Vec::new(),
            history: Vec::new(),
            redo_stack: Vec::new(),
            canvas_size: DEFAULT_CANVAS_SIZE,
            drawing: false,
            resize_drag: None,
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
        if !self.canvas_bounds.get().contains(&event.position) {
            return;
        }
        let position = self.to_canvas(event.position);
        let color = match self.tool {
            Tool::Pen => self.color,
            Tool::Eraser => rgb(CANVAS_BACKGROUND).into(),
            Tool::Eyedropper => {
                self.pick_color(position, window, cx);
                return;
            }
            Tool::Fill => {
                self.fill_at(position, window, cx);
                return;
            }
        };
        self.drawing = true;
        self.push_item(
            Item::Stroke(Stroke {
                points: vec![position],
                color,
                width: self.width,
            }),
            cx,
        );
    }

    fn to_canvas(&self, position: Point<Pixels>) -> Point<Pixels> {
        position - self.canvas_bounds.get().origin
    }

    fn push_item(&mut self, item: Item, cx: &mut Context<Self>) {
        self.items.push(item);
        self.history.push(HistoryEntry::Add);
        self.redo_stack.clear();
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
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
        if !self.drawing {
            return;
        }
        let position = self.to_canvas(event.position);
        if let Some(Item::Stroke(stroke)) = self.items.last_mut() {
            stroke.points.push(position);
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.drawing = false;
        if let Some(drag) = self.resize_drag.take() {
            // Put the start size back so resize_canvas records it for undo
            let size = std::mem::replace(&mut self.canvas_size, drag.start_size);
            self.resize_canvas(size, cx);
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
        let color = raster::color_at(
            &self.items,
            self.canvas_size,
            rgb(CANVAS_BACKGROUND).into(),
            window.scale_factor(),
            position,
        );
        let Ok(Some(color)) = color else {
            return;
        };
        self.color = color;
        // set_value does not emit a change event, so self.color is updated above
        self.color_picker
            .update(cx, |state, cx| state.set_value(color, window, cx));
        self.tool = Tool::Pen;
        cx.notify();
    }

    fn fill_at(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let scale = window.scale_factor();
        let rects = raster::flood_fill(
            &self.items,
            self.canvas_size,
            rgb(CANVAS_BACKGROUND).into(),
            scale,
            position,
            self.color,
        );
        let Ok(Some(rects)) = rects else {
            return;
        };
        let fill = Fill {
            color: self.color,
            scale,
            rects: Rc::new(rects),
        };
        self.push_item(Item::Fill(fill), cx);
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }
        let removed = std::mem::take(&mut self.items);
        self.history.push(HistoryEntry::Clear(removed));
        self.redo_stack.clear();
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        // Undoing mid-drag would pop the stroke being drawn or fight the resize
        if self.drawing || self.resize_drag.is_some() {
            return;
        }
        let redo = match self.history.pop() {
            Some(HistoryEntry::Add) => match self.items.pop() {
                Some(item) => RedoEntry::Add(item),
                None => return,
            },
            Some(HistoryEntry::Clear(removed)) => {
                self.items = removed;
                RedoEntry::Clear
            }
            Some(HistoryEntry::Resize(size)) => {
                RedoEntry::Resize(std::mem::replace(&mut self.canvas_size, size))
            }
            None => return,
        };
        self.redo_stack.push(redo);
        cx.notify();
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if self.drawing || self.resize_drag.is_some() {
            return;
        }
        let entry = match self.redo_stack.pop() {
            Some(RedoEntry::Add(item)) => {
                self.items.push(item);
                HistoryEntry::Add
            }
            Some(RedoEntry::Clear) => HistoryEntry::Clear(std::mem::take(&mut self.items)),
            Some(RedoEntry::Resize(size)) => {
                HistoryEntry::Resize(std::mem::replace(&mut self.canvas_size, size))
            }
            None => return,
        };
        self.history.push(entry);
        cx.notify();
    }

    /// Resizes the canvas, keeping the drawing anchored at the top-left.
    ///
    /// Items outside the new size are hidden, not removed, so enlarging the
    /// canvas again brings them back.
    fn resize_canvas(&mut self, size: Size<Pixels>, cx: &mut Context<Self>) {
        if size == self.canvas_size {
            return;
        }
        let previous = std::mem::replace(&mut self.canvas_size, size);
        self.history.push(HistoryEntry::Resize(previous));
        self.redo_stack.clear();
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
        // Encode now so the image reflects the drawing at the time of the request
        let png = raster::encode_png(
            &self.items,
            self.canvas_size,
            rgb(CANVAS_BACKGROUND).into(),
            // One image pixel per canvas pixel, so the PNG is exactly the canvas size
            1.0,
        );
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
        // Copy the items so they can be moved into the paint closure
        let items = self.items.clone();

        let theme = cx.theme();

        let toolbar = h_flex()
            .gap_3()
            .px_3()
            .h(px(44.0))
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
                            this.tool = tool;
                            cx.notify();
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
                    .disabled(self.history.is_empty())
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
                    .label(format!(
                        "{} × {}",
                        self.canvas_size.width.as_f32(),
                        self.canvas_size.height.as_f32()
                    ))
                    .tooltip("Canvas size")
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
                    move |bounds, _, window, _| {
                        canvas_bounds.set(bounds);
                        // Keep strokes that run past the edge from drawing outside the canvas
                        let mask = ContentMask { bounds };
                        window.with_content_mask(Some(mask), |window| {
                            for item in &items {
                                match item {
                                    Item::Stroke(stroke) => {
                                        paint_stroke(window, stroke, bounds.origin)
                                    }
                                    Item::Fill(fill) => paint_fill(window, fill, bounds.origin),
                                }
                            }
                        });
                    },
                )
                .size_full(),
            );

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
            .child(canvas_area);

        v_flex()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::save_png))
            .size_full()
            .text_color(theme.foreground)
            .child(toolbar)
            .child(viewport)
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

/// Paints `fill` with the canvas top-left at `origin`, in window coordinates.
fn paint_fill(window: &mut Window, fill: &Fill, origin: Point<Pixels>) {
    for rect in fill.rects.iter() {
        let bounds = fill.bounds(rect);
        let bounds = Bounds::new(origin + bounds.origin, bounds.size);
        window.paint_quad(gpui_kit::fill(bounds, fill.color));
    }
}

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        cx.bind_keys(
            UNDO_KEYS
                .iter()
                .map(|key| KeyBinding::new(key, Undo, Some(KEY_CONTEXT)))
                .chain(
                    REDO_KEYS
                        .iter()
                        .map(|key| KeyBinding::new(key, Redo, Some(KEY_CONTEXT))),
                )
                .chain([KeyBinding::new(SAVE_KEY, SavePng, Some(KEY_CONTEXT))]),
        );
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
