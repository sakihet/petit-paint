use canvas::{Brush, Canvas, Change, DEFAULT_FILL_TOLERANCE, Grid, Orientation, StrokeLayer};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::{
    color_picker::{ColorPickerEvent, ColorPickerState},
    slider::{SliderEvent, SliderState, SliderValue},
};
use gpui_kit::*;
use selection::{SelectDrag, Selection};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use text::{FontChoice, TextRenderer};
use tiny_skia::Pixmap;
use tool::{ShapeKind, Tool};
use view::ViewCache;
use zoom::DEFAULT_ZOOM_INDEX;

mod assets;
mod canvas;
mod file;
mod selection;
mod text;
mod tool;
mod ui;
mod view;
mod zoom;

actions!(
    petit_paint,
    [
        Undo,
        Redo,
        NewImage,
        OpenImage,
        Save,
        SaveAs,
        CloseWindow,
        SelectAll,
        CopySelection,
        CutSelection,
        Paste,
        DeleteSelection,
        Deselect,
        FinishPolygon,
        ZoomIn,
        ZoomOut,
        ResetZoom,
    ]
);

const DEFAULT_WIDTH: f32 = 4.0;
const DEFAULT_FONT_SIZE: f32 = 24.0;
const DEFAULT_CANVAS_SIZE: Size<Pixels> = Size {
    width: px(800.0),
    height: px(600.0),
};
/// Largest canvas side in pixels. Undo steps that replace the whole canvas
/// (clear, resize) keep a full copy of it, 64MB each at this size.
const MAX_CANVAS_SIDE: u32 = 4096;
/// Number of undo steps kept; older ones are dropped.
const MAX_HISTORY: usize = 100;
/// How close, in displayed pixels, a click must be to a polygon's first
/// point to close it.
const POLYGON_CLOSE_DISTANCE: f32 = 8.0;
const KEY_CONTEXT: &str = "PaintApp";
/// How many recently used colors the palette bar keeps.
const MAX_RECENT_COLORS: usize = 10;

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

/// A line through `points`: a brush stroke or a shape's outline. Points are
/// in canvas coordinates, with the origin at its top-left.
///
/// While it is being drawn it is previewed as a vector path; when finished
/// it is burned into the canvas pixels.
#[derive(Clone, Default)]
struct Stroke {
    points: Vec<Point<Pixels>>,
    color: Hsla,
    width: Pixels,
    /// Connects the last point back to the first.
    closed: bool,
    /// Fills the inside with `color` as well.
    filled: bool,
    /// Miter joins instead of round ones, for rectangles and polygons.
    sharp_corners: bool,
    brush: Brush,
    /// Seeds the airbrush's spray pattern.
    seed: u64,
}

/// Text being typed, previewed on the canvas until it is placed.
struct TextEdit {
    /// Canvas pixel where the text starts.
    anchor: Point<i32>,
    input: Entity<InputState>,
    /// The rendered text and how far left of `anchor` it starts.
    preview: Option<(Pixmap, u32)>,
    _subscription: Subscription,
}

/// A line, rectangle or ellipse being dragged out, in canvas coordinates.
#[derive(Clone, Copy)]
struct ShapeDrag {
    start: Point<Pixels>,
    end: Point<Pixels>,
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
    /// `current_stroke` rendered as it will be burned in, to preview it.
    stroke_preview: Option<StrokeLayer>,
    /// Bumped whenever the stroke preview changes, to refresh its cached image.
    stroke_preview_id: u64,
    stroke_cache: Rc<RefCell<ViewCache>>,
    brush: Brush,
    /// Strokes made so far, to give each airbrush stroke its own spray pattern.
    stroke_count: u64,
    /// While erasing, the canvas as it was when the stroke began.
    erase_base: Option<Pixmap>,
    /// The PNG file last opened or saved, which saving writes to.
    document_path: Option<PathBuf>,
    /// `canvas.version()` when the drawing was last opened or saved.
    saved_version: u64,
    /// Set once the user agreed to close with unsaved changes.
    close_confirmed: bool,
    /// Whether the save-changes prompt is showing; gpui allows only one at a time.
    confirming: bool,
    /// Restores what the eraser stroke so far has removed.
    erase_undo: Option<Change>,
    shape_kind: ShapeKind,
    /// Whether rectangles, ellipses and polygons are filled with the brush color.
    shape_filled: bool,
    shape_drag: Option<ShapeDrag>,
    /// Corners of the polygon being built, in canvas coordinates.
    polygon: Vec<Point<Pixels>>,
    /// Where the mouse is while building a polygon, for previewing the next edge.
    polygon_hover: Option<Point<Pixels>>,
    text_edit: Option<TextEdit>,
    /// Made on first use of the text tool, since it scans the installed fonts.
    text_renderer: Option<TextRenderer>,
    font: FontChoice,
    font_size: f32,
    font_size_slider: Entity<SliderState>,
    /// Bumped whenever the text preview changes, to refresh its cached image.
    text_preview_id: u64,
    text_cache: Rc<RefCell<ViewCache>>,
    view_cache: Rc<RefCell<ViewCache>>,
    selection: Option<Selection>,
    select_drag: Option<SelectDrag>,
    /// Bumped whenever new floating pixels are created, to refresh their cached image.
    floating_id: u64,
    floating_cache: Rc<RefCell<ViewCache>>,
    /// The part of the canvas inside the viewport, in window coordinates from
    /// the last paint. Pasted pixels land at its top-left.
    visible_bounds: Rc<Cell<Bounds<Pixels>>>,
    zoom_index: usize,
    grid: Grid,
    /// Scroll or pinch input not yet turned into a zoom step.
    zoom_input: f32,
    scroll_handle: ScrollHandle,
    resize_drag: Option<ResizeDrag>,
    /// Canvas pixel under the mouse, shown in the status bar.
    cursor_position: Option<Point<i32>>,
    focus_handle: FocusHandle,
    /// Canvas bounds in window coordinates from the last paint, used to map
    /// mouse positions to canvas coordinates.
    canvas_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Color 1, drawn with the left mouse button.
    color: Hsla,
    /// Color 2, drawn with the right mouse button.
    secondary_color: Hsla,
    /// Whether the current mouse press is with the right button, using color 2.
    use_secondary: bool,
    /// Colors drawn with, most recent first.
    recent_colors: Vec<Hsla>,
    width: Pixels,
    color_picker: Entity<ColorPickerState>,
    secondary_picker: Entity<ColorPickerState>,
    width_slider: Entity<SliderState>,
    /// How far each RGBA channel may differ from the clicked pixel for the
    /// fill tool to reach it.
    fill_tolerance: u8,
    /// Sets the fill tolerance in percent.
    fill_tolerance_slider: Entity<SliderState>,
    _subscriptions: Vec<Subscription>,
}

impl PaintApp {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let color: Hsla = rgb(0x222222).into();
        let color_picker = cx.new(|cx| ColorPickerState::new(window, cx).default_value(color));
        let secondary_color: Hsla = rgb(0xffffff).into();
        let secondary_picker =
            cx.new(|cx| ColorPickerState::new(window, cx).default_value(secondary_color));
        let width_slider = cx.new(|_| {
            SliderState::new()
                .min(1.0)
                .max(32.0)
                .step(1.0)
                .default_value(DEFAULT_WIDTH)
        });

        let font_size_slider = cx.new(|_| {
            SliderState::new()
                .min(8.0)
                .max(144.0)
                .step(1.0)
                .default_value(DEFAULT_FONT_SIZE)
        });

        let fill_tolerance_slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(100.0)
                .step(1.0)
                .default_value(tolerance_percent(DEFAULT_FILL_TOLERANCE))
        });

        // Ask before closing the window over unsaved changes
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            this.update(cx, |this, cx| this.should_close(window, cx))
                .unwrap_or(true)
        });

        let _subscriptions = vec![
            cx.subscribe_in(&color_picker, window, |this, _, event, _, cx| {
                let ColorPickerEvent::Change(Some(color)) = event else {
                    return;
                };
                this.color = *color;
                this.update_text_preview(cx);
                cx.notify();
            }),
            cx.subscribe_in(&secondary_picker, window, |this, _, event, _, cx| {
                let ColorPickerEvent::Change(Some(color)) = event else {
                    return;
                };
                this.secondary_color = *color;
                cx.notify();
            }),
            cx.subscribe_in(&font_size_slider, window, |this, _, event, _, cx| {
                let (SliderEvent::Change(SliderValue::Single(size))
                | SliderEvent::Release(SliderValue::Single(size))) = event
                else {
                    return;
                };
                this.font_size = *size;
                this.update_text_preview(cx);
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
            cx.subscribe_in(&fill_tolerance_slider, window, |this, _, event, _, cx| {
                let (SliderEvent::Change(SliderValue::Single(percent))
                | SliderEvent::Release(SliderValue::Single(percent))) = event
                else {
                    return;
                };
                this.fill_tolerance = (percent / 100.0 * 255.0).round() as u8;
                cx.notify();
            }),
        ];

        let canvas = Canvas::new(
            DEFAULT_CANVAS_SIZE.width.as_f32() as u32,
            DEFAULT_CANVAS_SIZE.height.as_f32() as u32,
            // Transparent, for drawing game sprites and other assets
            transparent_black(),
        )
        .expect("default canvas size is valid");

        Self {
            canvas,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            canvas_size: DEFAULT_CANVAS_SIZE,
            current_stroke: None,
            stroke_preview: None,
            stroke_preview_id: 0,
            stroke_cache: Rc::default(),
            brush: Brush::Pen,
            stroke_count: 0,
            erase_base: None,
            document_path: None,
            saved_version: 0,
            close_confirmed: false,
            confirming: false,
            erase_undo: None,
            shape_kind: ShapeKind::Rectangle,
            shape_filled: false,
            shape_drag: None,
            polygon: Vec::new(),
            polygon_hover: None,
            text_edit: None,
            text_renderer: None,
            font: FontChoice::SansSerif,
            font_size: DEFAULT_FONT_SIZE,
            font_size_slider,
            text_preview_id: 0,
            text_cache: Rc::default(),
            view_cache: Rc::default(),
            selection: None,
            select_drag: None,
            floating_id: 0,
            floating_cache: Rc::default(),
            visible_bounds: Rc::default(),
            zoom_index: DEFAULT_ZOOM_INDEX,
            grid: Grid::default(),
            zoom_input: 0.0,
            scroll_handle: ScrollHandle::new(),
            resize_drag: None,
            cursor_position: None,
            tool: Tool::Pen,
            focus_handle: cx.focus_handle(),
            canvas_bounds: Rc::new(Cell::new(Bounds::default())),
            color,
            width: px(DEFAULT_WIDTH),
            color_picker,
            secondary_picker,
            secondary_color,
            use_secondary: false,
            recent_colors: Vec::new(),
            width_slider,
            fill_tolerance: DEFAULT_FILL_TOLERANCE,
            fill_tolerance_slider,
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
        self.use_secondary = event.button == MouseButton::Right;
        // The right button only paints; it neither selects nor types
        if self.use_secondary && matches!(self.tool, Tool::Select | Tool::Text) {
            return;
        }
        if self.tool == Tool::Select {
            self.start_select(event.position, cx);
            return;
        }
        if !self.canvas_bounds.get().contains(&event.position) {
            return;
        }
        let position = self.to_canvas(event.position);
        let (color, brush) = match self.tool {
            Tool::Select => return,
            Tool::Pen => (self.active_color(), self.brush),
            Tool::Eraser => {
                // The eraser's shape is all that matters; it removes what is under it
                self.erase_base = Some(self.canvas.pixels().clone());
                (black(), Brush::Pen)
            }
            Tool::Eyedropper => {
                self.pick_color(position, window, cx);
                return;
            }
            Tool::Fill => {
                self.fill_at(position, cx);
                return;
            }
            Tool::Shape => {
                self.start_shape(position, event.click_count, cx);
                return;
            }
            Tool::Text => {
                self.start_text(position, window, cx);
                return;
            }
        };
        self.stroke_count += 1;
        self.current_stroke = Some(Stroke {
            points: vec![position],
            color,
            width: self.width,
            brush,
            seed: self.stroke_count,
            ..Default::default()
        });
        self.update_stroke_preview(cx);
    }

    /// The color the current mouse press draws with: color 2 for the right button.
    fn active_color(&self) -> Hsla {
        if self.use_secondary {
            self.secondary_color
        } else {
            self.color
        }
    }

    /// Notes `color` as just drawn with, for the recent colors in the palette bar.
    fn remember_color(&mut self, color: Hsla) {
        remember_color(&mut self.recent_colors, color);
    }

    fn set_primary(&mut self, color: Hsla, window: &mut Window, cx: &mut Context<Self>) {
        self.color = color;
        // set_value does not emit a change event, so the color is set above
        self.color_picker
            .update(cx, |state, cx| state.set_value(color, window, cx));
        self.update_text_preview(cx);
        cx.notify();
    }

    fn set_secondary(&mut self, color: Hsla, window: &mut Window, cx: &mut Context<Self>) {
        self.secondary_color = color;
        self.secondary_picker
            .update(cx, |state, cx| state.set_value(color, window, cx));
        cx.notify();
    }

    fn swap_colors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (primary, secondary) = (self.color, self.secondary_color);
        self.set_primary(secondary, window, cx);
        self.set_secondary(primary, window, cx);
    }

    fn update_stroke_preview(&mut self, cx: &mut Context<Self>) {
        if let (Some(stroke), Some(base)) = (&self.current_stroke, &self.erase_base) {
            // Transparency cannot be layered on top, so the eraser applies directly
            self.erase_undo = self.canvas.erase_stroke(base, stroke);
        } else {
            self.stroke_preview = self
                .current_stroke
                .as_ref()
                .and_then(|stroke| self.canvas.render_stroke(stroke));
            self.stroke_preview_id += 1;
        }
        cx.notify();
    }

    /// Opens a text box at `position`, or places the text being typed if
    /// there is one, as clicking outside a text box does in Windows Paint.
    fn start_text(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if self.text_edit.is_some() {
            self.commit_text(cx);
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Type, then press Enter"));
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                InputEvent::Change => this.update_text_preview(cx),
                InputEvent::PressEnter { .. } => {
                    this.commit_text(cx);
                    window.focus(&this.focus_handle, cx);
                }
                _ => {}
            });
        input.update(cx, |input, cx| input.focus(window, cx));
        self.text_edit = Some(TextEdit {
            anchor: point(
                position.x.as_f32().floor() as i32,
                position.y.as_f32().floor() as i32,
            ),
            input,
            preview: None,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Re-renders the text being typed with the current font, size and color.
    fn update_text_preview(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self
            .text_edit
            .as_ref()
            .map(|edit| edit.input.read(cx).value().to_string())
        else {
            return;
        };
        let renderer = self.text_renderer.get_or_insert_with(TextRenderer::new);
        let preview = renderer.render(&text, self.font, self.font_size, self.color);
        if let Some(edit) = &mut self.text_edit {
            edit.preview = preview;
        }
        self.text_preview_id += 1;
        cx.notify();
    }

    /// Burns the text being typed into the canvas as one undo step.
    fn commit_text(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.text_edit.take() else {
            return;
        };
        if let Some((pixels, margin)) = &edit.preview {
            let at = point(edit.anchor.x - *margin as i32, edit.anchor.y);
            if let Some(undo) = self.canvas.stamp(pixels, at) {
                self.record(undo, cx);
                self.remember_color(self.color);
            }
        }
        cx.notify();
    }

    fn start_shape(&mut self, position: Point<Pixels>, click_count: usize, cx: &mut Context<Self>) {
        if self.shape_kind != ShapeKind::Polygon {
            self.shape_drag = Some(ShapeDrag {
                start: position,
                end: position,
            });
            cx.notify();
            return;
        }
        let closes = self.polygon.first().is_some_and(|first| {
            let distance = (position - *first).magnitude() as f32 * self.zoom();
            self.polygon.len() >= 3 && distance <= POLYGON_CLOSE_DISTANCE
        });
        // The first click of a double-click already added this corner
        if click_count >= 2 || closes {
            self.finish_polygon(cx);
            return;
        }
        self.polygon.push(position);
        cx.notify();
    }

    /// The outline of a shape dragged from `start` to `end`.
    fn shape_stroke(&self, start: Point<Pixels>, end: Point<Pixels>) -> Stroke {
        let base = Stroke {
            color: self.active_color(),
            width: self.width,
            filled: self.shape_filled && self.shape_kind.fillable(),
            ..Default::default()
        };
        let (left, right) = (start.x.min(end.x), start.x.max(end.x));
        let (top, bottom) = (start.y.min(end.y), start.y.max(end.y));
        match self.shape_kind {
            ShapeKind::Line | ShapeKind::Polygon => Stroke {
                points: vec![start, end],
                ..base
            },
            ShapeKind::Rectangle => Stroke {
                points: vec![
                    point(left, top),
                    point(right, top),
                    point(right, bottom),
                    point(left, bottom),
                ],
                closed: true,
                sharp_corners: true,
                ..base
            },
            ShapeKind::Ellipse => {
                let center = point((left + right) / 2.0, (top + bottom) / 2.0);
                let (rx, ry) = ((right - left) / 2.0, (bottom - top) / 2.0);
                // Enough segments that edges stay smooth at the largest zoom
                let segments = ((rx + ry).as_f32() * 2.0).clamp(24.0, 720.0) as usize;
                let points = (0..segments)
                    .map(|i| {
                        let angle = i as f32 / segments as f32 * std::f32::consts::TAU;
                        center + point(rx * angle.cos(), ry * angle.sin())
                    })
                    .collect();
                Stroke {
                    points,
                    closed: true,
                    ..base
                }
            }
        }
    }

    /// The polygon so far, with an edge to the mouse, or the finished polygon.
    fn polygon_stroke(&self, finished: bool) -> Stroke {
        let mut points = self.polygon.clone();
        if !finished {
            points.extend(self.polygon_hover);
        }
        Stroke {
            points,
            color: self.active_color(),
            width: self.width,
            closed: finished,
            filled: finished && self.shape_filled,
            sharp_corners: true,
            ..Default::default()
        }
    }

    /// The shape being drawn, to preview over the canvas.
    fn shape_preview(&self) -> Option<Stroke> {
        if let Some(drag) = self.shape_drag {
            return Some(self.shape_stroke(drag.start, drag.end));
        }
        (!self.polygon.is_empty()).then(|| self.polygon_stroke(false))
    }

    fn finish_polygon(&mut self, cx: &mut Context<Self>) {
        if self.polygon.len() >= 3
            && let Some(undo) = self.canvas.draw_stroke(&self.polygon_stroke(true))
        {
            self.record(undo, cx);
            self.remember_color(self.active_color());
        }
        self.cancel_polygon(cx);
    }

    fn on_finish_polygon(&mut self, _: &FinishPolygon, _: &mut Window, cx: &mut Context<Self>) {
        self.finish_polygon(cx);
    }

    fn cancel_polygon(&mut self, cx: &mut Context<Self>) {
        self.polygon.clear();
        self.polygon_hover = None;
        cx.notify();
    }

    fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        if tool != Tool::Select {
            self.deselect(cx);
        }
        self.cancel_polygon(cx);
        self.commit_text(cx);
        self.tool = tool;
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
        if self.shape_drag.is_some() {
            let end = self.to_canvas(event.position);
            let kind = self.shape_kind;
            if let Some(drag) = &mut self.shape_drag {
                drag.end = if event.modifiers.shift {
                    constrain(kind, drag.start, end)
                } else {
                    end
                };
            }
            cx.notify();
        }
        if !self.polygon.is_empty() {
            self.polygon_hover = Some(self.to_canvas(event.position));
            cx.notify();
        }

        let cursor_position = self.canvas_bounds.get().contains(&event.position).then(|| {
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
            let delta = point(delta.x / self.zoom(), delta.y / self.zoom());
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
            self.update_stroke_preview(cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.select_drag = None;
        if let Some(drag) = self.shape_drag.take()
            && drag.start != drag.end
            && let Some(undo) = self
                .canvas
                .draw_stroke(&self.shape_stroke(drag.start, drag.end))
        {
            self.record(undo, cx);
            self.remember_color(self.active_color());
        }
        self.stroke_preview = None;
        let stroke = self.current_stroke.take();
        if self.erase_base.take().is_some() {
            if let Some(undo) = self.erase_undo.take() {
                self.record(undo, cx);
            }
        } else if let Some(stroke) = stroke
            && let Some(undo) = self.canvas.draw_stroke(&stroke)
        {
            self.record(undo, cx);
            self.remember_color(stroke.color);
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
        // Picking a fully transparent pixel would leave a brush that draws nothing
        let Some(color) = self.canvas.color_at(position).filter(|color| color.a > 0.0) else {
            return;
        };
        // The right button picks color 2
        if self.use_secondary {
            self.set_secondary(color, window, cx);
        } else {
            self.set_primary(color, window, cx);
        }
        self.tool = Tool::Pen;
        cx.notify();
    }

    fn fill_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let color = self.active_color();
        if let Some(undo) = self.canvas.flood_fill(position, color, self.fill_tolerance) {
            self.record(undo, cx);
            self.remember_color(color);
        }
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.deselect(cx);
        self.text_edit = None;
        if let Ok(undo) = self.canvas.clear() {
            self.record(undo, cx);
        }
    }

    /// Whether a mouse drag is in progress, during which undo and redo would
    /// conflict with the stroke or resize being made.
    fn dragging(&self) -> bool {
        self.current_stroke.is_some()
            || self.resize_drag.is_some()
            || self.select_drag.is_some()
            || self.shape_drag.is_some()
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if self.dragging() {
            return;
        }
        // Undo first takes back an unfinished polygon
        if !self.polygon.is_empty() {
            self.cancel_polygon(cx);
            return;
        }
        // Placing typed text records it, which this then undoes
        self.commit_text(cx);
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
        self.commit_text(cx);
        self.deselect(cx);
        let Some(change) = self.redo_stack.pop() else {
            return;
        };
        self.undo_stack.push(self.canvas.apply(change));
        self.canvas_size = self.canvas.size();
        cx.notify();
    }

    /// Flips or turns the selection, or the whole canvas when nothing is selected.
    fn reorient(&mut self, orientation: Orientation, cx: &mut Context<Self>) {
        if self.dragging() {
            return;
        }
        self.commit_text(cx);
        self.cancel_polygon(cx);
        if let Some(selection) = &mut self.selection {
            // Lift the selection so the change is one undo step with any move
            if selection.floating.is_none() {
                selection.floating = self.canvas.lift(selection.rect);
            }
            if let Some(floating) = &mut selection.floating {
                floating.reorient(orientation);
                let (width, height) = (floating.pixels.width(), floating.pixels.height());
                // Turn about the selection's center
                let rect = &mut selection.rect;
                rect.x += (rect.width as i32 - width as i32) / 2;
                rect.y += (rect.height as i32 - height as i32) / 2;
                rect.width = width;
                rect.height = height;
                self.floating_id += 1;
            }
            cx.notify();
            return;
        }
        let undo = self.canvas.reorient(orientation);
        self.record(undo, cx);
        self.canvas_size = self.canvas.size();
    }

    /// Resizes the canvas, keeping the drawing anchored at the top-left.
    /// Growing adds background; shrinking crops, which undo brings back.
    fn resize_canvas(&mut self, size: Size<Pixels>, cx: &mut Context<Self>) {
        self.deselect(cx);
        self.commit_text(cx);
        if size != self.canvas.size()
            && let Ok(undo) = self
                .canvas
                .resize(size.width.as_f32() as u32, size.height.as_f32() as u32)
        {
            self.record(undo, cx);
        }
        self.canvas_size = self.canvas.size();
        cx.notify();
    }
}

/// A fill tolerance as a percentage of the full channel range.
fn tolerance_percent(tolerance: u8) -> f32 {
    (tolerance as f32 / 255.0 * 100.0).round()
}

/// Moves `color` to the front of `recent`, adding it if new and dropping the
/// oldest past [`MAX_RECENT_COLORS`].
fn remember_color(recent: &mut Vec<Hsla>, color: Hsla) {
    recent.retain(|&c| c.to_rgb() != color.to_rgb());
    recent.insert(0, color);
    recent.truncate(MAX_RECENT_COLORS);
}

/// Snaps a dragged shape so lines run at multiples of 45° and rectangles and
/// ellipses come out square and circular, as Shift does in most paint apps.
fn constrain(kind: ShapeKind, start: Point<Pixels>, end: Point<Pixels>) -> Point<Pixels> {
    let delta = end - start;
    match kind {
        ShapeKind::Line | ShapeKind::Polygon => {
            let angle = delta.y.as_f32().atan2(delta.x.as_f32());
            let step = std::f32::consts::FRAC_PI_4;
            let angle = (angle / step).round() * step;
            let length = delta.magnitude() as f32;
            start + point(px(length * angle.cos()), px(length * angle.sin()))
        }
        ShapeKind::Rectangle | ShapeKind::Ellipse => {
            let side = delta.x.abs().max(delta.y.abs());
            let signed = |d: Pixels| if d < px(0.0) { -side } else { side };
            start + point(signed(delta.x), signed(delta.y))
        }
    }
}

fn main() {
    // The toolbar's icons and those gpui-kit's components draw, such as the
    // checkbox's check mark
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(|cx| {
            gpui_kit::init(cx);
            let context = Some(KEY_CONTEXT);
            let mut bindings = vec![
                KeyBinding::new(with_modifier!("z"), Undo, context),
                KeyBinding::new(with_modifier!("shift-z"), Redo, context),
                KeyBinding::new(with_modifier!("s"), Save, context),
                KeyBinding::new(with_modifier!("shift-s"), SaveAs, context),
                KeyBinding::new(with_modifier!("n"), NewImage, context),
                KeyBinding::new(with_modifier!("o"), OpenImage, context),
                KeyBinding::new(with_modifier!("w"), CloseWindow, context),
                KeyBinding::new(with_modifier!("a"), SelectAll, context),
                KeyBinding::new(with_modifier!("c"), CopySelection, context),
                KeyBinding::new(with_modifier!("x"), CutSelection, context),
                KeyBinding::new(with_modifier!("v"), Paste, context),
                KeyBinding::new(with_modifier!("="), ZoomIn, context),
                KeyBinding::new(with_modifier!("-"), ZoomOut, context),
                KeyBinding::new(with_modifier!("0"), ResetZoom, context),
                KeyBinding::new("backspace", DeleteSelection, context),
                KeyBinding::new("delete", DeleteSelection, context),
                KeyBinding::new("escape", Deselect, context),
                KeyBinding::new("enter", FinishPolygon, context),
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
            // Quit with the last window, as there is no way to open another
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}

#[cfg(test)]
mod tests {
    // Not a glob: `gpui_kit::*` would bring in gpui's `test` macro in place of Rust's
    use super::{MAX_RECENT_COLORS, remember_color};
    use gpui_kit::{Hsla, rgb};

    #[test]
    fn recent_colors_put_the_latest_first_without_repeats() {
        let mut recent = Vec::new();
        let color = |c: u32| -> Hsla { rgb(c).into() };
        remember_color(&mut recent, color(0xff0000));
        remember_color(&mut recent, color(0x00ff00));
        remember_color(&mut recent, color(0xff0000));
        let rgbs: Vec<_> = recent.iter().map(|c| c.to_rgb()).collect();
        assert_eq!(
            rgbs,
            vec![color(0xff0000).to_rgb(), color(0x00ff00).to_rgb()]
        );

        for c in 0..20 {
            remember_color(&mut recent, color(c));
        }
        assert_eq!(recent.len(), MAX_RECENT_COLORS);
        assert_eq!(recent[0].to_rgb(), color(19).to_rgb());
    }
}
