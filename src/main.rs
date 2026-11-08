use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, h_flex,
    button::*,
    checkbox::Checkbox,
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    slider::{Slider, SliderEvent, SliderState, SliderValue},
    v_flex,
};
use gpui_kit::component::{
    WindowExt as _,
    dialog::DialogFooter,
    input::{Input, InputEvent, InputState, NumberInput},
    notification::Notification,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use canvas::{BRUSHES, Brush, Canvas, Change, Floating, PixelRect, StrokeLayer};
use image::{Frame, RgbaImage};
use lyon::tessellation::{LineCap, LineJoin};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use text::{FONTS, FontChoice, TextRenderer};
use tiny_skia::Pixmap;

mod canvas;
mod text;

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
/// Zoom levels, as displayed pixels per canvas pixel.
const ZOOM_LEVELS: [f32; 8] = [0.25, 0.5, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0];
const DEFAULT_ZOOM_INDEX: usize = 2;
/// Scroll distance with Cmd/Ctrl held, or pinch amount, per zoom step.
const SCROLL_PER_ZOOM_STEP: f32 = 50.0;
const PINCH_PER_ZOOM_STEP: f32 = 0.15;
/// How close, in displayed pixels, a click must be to a polygon's first
/// point to close it.
const POLYGON_CLOSE_DISTANCE: f32 = 8.0;
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
    /// Device pixels per canvas pixel, as bits so the key can be compared exactly.
    pixels_per_canvas_pixel: u32,
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
enum ShapeKind {
    Line,
    Rectangle,
    Ellipse,
    /// Built by clicking each corner; closes on a double-click, Enter, or a
    /// click on the first corner.
    Polygon,
}

/// Shapes in toolbar order.
const SHAPES: [ShapeKind; 4] = [
    ShapeKind::Line,
    ShapeKind::Rectangle,
    ShapeKind::Ellipse,
    ShapeKind::Polygon,
];

impl ShapeKind {
    fn label(self) -> &'static str {
        match self {
            ShapeKind::Line => "Line",
            ShapeKind::Rectangle => "Rectangle",
            ShapeKind::Ellipse => "Ellipse",
            ShapeKind::Polygon => "Polygon",
        }
    }

    /// Whether the shape has an inside that can be filled.
    fn fillable(self) -> bool {
        self != ShapeKind::Line
    }
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

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    /// Selects a rectangle to move, copy, cut or delete.
    Select,
    /// Draws freehand with the brush chosen in the toolbar.
    Pen,
    /// Paints with the canvas background color, so erasing is undoable like any stroke.
    Eraser,
    /// Picks the brush color from the canvas, then switches back to the pen.
    Eyedropper,
    /// Fills the region of matching color under the cursor with the brush color.
    Fill,
    /// Draws the shape chosen in the toolbar.
    Shape,
    /// Types text onto the canvas at the clicked point.
    Text,
}

/// Tools in toolbar order.
const TOOLS: [Tool; 7] = [
    Tool::Select,
    Tool::Pen,
    Tool::Eraser,
    Tool::Eyedropper,
    Tool::Fill,
    Tool::Shape,
    Tool::Text,
];

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Pen => "Brush",
            Tool::Eraser => "Eraser",
            Tool::Eyedropper => "Eyedropper",
            Tool::Fill => "Fill",
            Tool::Shape => "Shape",
            Tool::Text => "Text",
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
    /// `current_stroke` rendered as it will be burned in, to preview it.
    stroke_preview: Option<StrokeLayer>,
    /// Bumped whenever the stroke preview changes, to refresh its cached image.
    stroke_preview_id: u64,
    stroke_cache: Rc<RefCell<ViewCache>>,
    brush: Brush,
    /// Strokes made so far, to give each airbrush stroke its own spray pattern.
    stroke_count: u64,
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

        let font_size_slider = cx.new(|_| {
            SliderState::new()
                .min(8.0)
                .max(144.0)
                .step(1.0)
                .default_value(DEFAULT_FONT_SIZE)
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
            stroke_preview: None,
            stroke_preview_id: 0,
            stroke_cache: Rc::default(),
            brush: Brush::Pen,
            stroke_count: 0,
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
        let (color, brush) = match self.tool {
            Tool::Select => return,
            Tool::Pen => (self.color, self.brush),
            Tool::Eraser => (rgb(CANVAS_BACKGROUND).into(), Brush::Pen),
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

    fn update_stroke_preview(&mut self, cx: &mut Context<Self>) {
        self.stroke_preview = self
            .current_stroke
            .as_ref()
            .and_then(|stroke| self.canvas.render_stroke(stroke));
        self.stroke_preview_id += 1;
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
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            match event {
                InputEvent::Change => this.update_text_preview(cx),
                InputEvent::PressEnter { .. } => {
                    this.commit_text(cx);
                    window.focus(&this.focus_handle, cx);
                }
                _ => {}
            }
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
            color: self.color,
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
            color: self.color,
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

    fn zoom(&self) -> f32 {
        ZOOM_LEVELS[self.zoom_index]
    }

    fn to_canvas(&self, position: Point<Pixels>) -> Point<Pixels> {
        let position = position - self.canvas_bounds.get().origin;
        point(position.x / self.zoom(), position.y / self.zoom())
    }

    /// Zooms to `ZOOM_LEVELS[index]`, keeping the canvas point under `anchor`
    /// (in window coordinates) in place by scrolling the viewport.
    fn zoom_to(&mut self, index: usize, anchor: Point<Pixels>, cx: &mut Context<Self>) {
        let index = index.min(ZOOM_LEVELS.len() - 1);
        if index == self.zoom_index {
            return;
        }
        let canvas_point = self.to_canvas(anchor);
        let zoom = ZOOM_LEVELS[index];
        let canvas_origin = self.canvas_bounds.get().origin;
        let new_origin = anchor - point(canvas_point.x * zoom, canvas_point.y * zoom);
        let offset = self.scroll_handle.offset() + (new_origin - canvas_origin);
        // Offsets are negative when scrolled; past zero would leave a gap at the start
        self.scroll_handle
            .set_offset(point(offset.x.min(px(0.0)), offset.y.min(px(0.0))));
        self.zoom_index = index;
        cx.notify();
    }

    /// Zooms one step in or out around the middle of the visible canvas.
    fn zoom_step(&mut self, zoom_in: bool, cx: &mut Context<Self>) {
        let visible = self.visible_bounds.get();
        let anchor = visible.origin + point(visible.size.width / 2.0, visible.size.height / 2.0);
        let index = if zoom_in {
            self.zoom_index + 1
        } else {
            self.zoom_index.saturating_sub(1)
        };
        self.zoom_to(index, anchor, cx);
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_step(true, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_step(false, cx);
    }

    fn reset_zoom(&mut self, _: &ResetZoom, _: &mut Window, cx: &mut Context<Self>) {
        let visible = self.visible_bounds.get();
        self.zoom_to(DEFAULT_ZOOM_INDEX, visible.origin, cx);
    }

    /// Accumulates scroll or pinch input and zooms a step around `anchor`
    /// each time it passes `per_step`.
    fn zoom_by_input(
        &mut self,
        amount: f32,
        per_step: f32,
        anchor: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.zoom_input += amount;
        while self.zoom_input.abs() >= per_step {
            let zoom_in = self.zoom_input > 0.0;
            self.zoom_input -= per_step.copysign(self.zoom_input);
            let index = if zoom_in {
                self.zoom_index + 1
            } else {
                self.zoom_index.saturating_sub(1)
            };
            self.zoom_to(index, anchor, cx);
        }
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !event.modifiers.secondary(cx) {
            return;
        }
        // Zoom instead of scrolling the viewport
        cx.stop_propagation();
        let delta = event.delta.pixel_delta(px(16.0)).y.as_f32();
        self.zoom_by_input(delta, SCROLL_PER_ZOOM_STEP, event.position, cx);
    }

    fn on_pinch(&mut self, event: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by_input(event.delta, PINCH_PER_ZOOM_STEP, event.position, cx);
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
        self.cancel_polygon(cx);
        self.commit_text(cx);
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

    /// Escape: discards text being typed, cancels a polygon being built, or
    /// drops the selection.
    fn on_deselect(&mut self, _: &Deselect, window: &mut Window, cx: &mut Context<Self>) {
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
            && let Some(undo) = self.canvas.draw_stroke(&self.shape_stroke(drag.start, drag.end))
        {
            self.record(undo, cx);
        }
        self.stroke_preview = None;
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

    /// Resizes the canvas, keeping the drawing anchored at the top-left.
    /// Growing adds background; shrinking crops, which undo brings back.
    fn resize_canvas(&mut self, size: Size<Pixels>, cx: &mut Context<Self>) {
        self.deselect(cx);
        self.commit_text(cx);
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
        self.commit_text(cx);
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
            .when(self.tool == Tool::Pen, |this| {
                this.child(
                    ButtonGroup::new("brush")
                        .outline()
                        .children(BRUSHES.map(|brush| {
                            Button::new(brush.label())
                                .label(brush.label())
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
                                .label(shape.label())
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
                        .child(div().w(px(120.0)).child(Slider::new(&self.font_size_slider)))
                        .child(div().w(px(28.0)).child(format!("{}", self.font_size))),
                )
            })
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
        let text_cache = self.text_cache.clone();
        let stroke_cache = self.stroke_cache.clone();
        let preview = self.shape_preview();
        let app = cx.entity();

        let zoom = self.zoom();
        // The canvas as displayed, at the current zoom
        let canvas_size = size(self.canvas_size.width * zoom, self.canvas_size.height * zoom);
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
            .w(canvas_size.width)
            .h(canvas_size.height)
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
                            paint_view(
                                window,
                                &app.canvas,
                                bounds,
                                zoom,
                                &mut view_cache.borrow_mut(),
                            );
                            if let Some(selection) = &app.selection
                                && let Some(floating) = &selection.floating
                            {
                                let rect = selection.rect;
                                let at = bounds.origin
                                    + point(px(rect.x as f32 * zoom), px(rect.y as f32 * zoom));
                                paint_floating(
                                    window,
                                    &floating.pixels,
                                    app.floating_id,
                                    at,
                                    zoom,
                                    &mut floating_cache.borrow_mut(),
                                );
                            }
                            if let Some(edit) = &app.text_edit
                                && let Some((pixels, margin)) = &edit.preview
                            {
                                let x = edit.anchor.x as f32 - *margin as f32;
                                let at = bounds.origin
                                    + point(px(x * zoom), px(edit.anchor.y as f32 * zoom));
                                paint_floating(
                                    window,
                                    pixels,
                                    app.text_preview_id,
                                    at,
                                    zoom,
                                    &mut text_cache.borrow_mut(),
                                );
                            }
                            if let Some(layer) = &app.stroke_preview {
                                let at = bounds.origin
                                    + point(
                                        px(layer.at.x as f32 * zoom),
                                        px(layer.at.y as f32 * zoom),
                                    );
                                paint_floating(
                                    window,
                                    &layer.pixels,
                                    app.stroke_preview_id,
                                    at,
                                    zoom,
                                    &mut stroke_cache.borrow_mut(),
                                );
                            }
                            if let Some(stroke) = &preview {
                                paint_stroke(window, stroke, bounds.origin, zoom);
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
                        .left(px(rect.x as f32 * zoom))
                        .top(px(rect.y as f32 * zoom))
                        .w(px(rect.width as f32 * zoom))
                        .h(px(rect.height as f32 * zoom))
                        .border_1()
                        .border_dashed()
                        .border_color(rgb(SELECTION_COLOR)),
                )
            })
            .when_some(self.text_edit.as_ref(), |this, edit| {
                // Below the line of text, so it does not cover the preview
                let line_height = (self.font_size * 1.2).ceil();
                let top = (edit.anchor.y as f32 + line_height) * zoom + 4.0;
                this.child(
                    div()
                        .id("text-editor")
                        .absolute()
                        .left(px(edit.anchor.x as f32 * zoom))
                        .top(px(top))
                        .w(px(240.0))
                        .bg(theme.background)
                        .shadow_md()
                        .rounded_md()
                        // Clicks in the box edit the text rather than reach the canvas
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(Input::new(&edit.input).small()),
                )
            });

        // Fills the viewport and holds the canvas. Zoom gestures are handled
        // here because a child's listeners run before the viewport scrolls.
        let stage = div()
            .flex()
            .items_start()
            .flex_none()
            .min_w_full()
            .min_h_full()
            .p_3()
            .on_scroll(cx.listener(Self::on_scroll))
            .on_pinch(cx.listener(Self::on_pinch))
            .child(canvas_area);

        // Mouse handlers sit on the viewport so a drag keeps drawing when it
        // leaves the canvas
        let viewport = div()
            .id("viewport")
            .flex()
            .items_start()
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
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
            .child(stage);

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
            })
            .child(div().flex_1())
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("zoom-out")
                            .ghost()
                            .compact()
                            .label("−")
                            .tooltip_with_action("Zoom out", &ZoomOut, Some(KEY_CONTEXT))
                            .disabled(self.zoom_index == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_step(false, cx))),
                    )
                    .child(
                        Button::new("reset-zoom")
                            .ghost()
                            .compact()
                            .label(format!("{}%", (zoom * 100.0).round()))
                            .tooltip_with_action("Reset zoom", &ResetZoom, Some(KEY_CONTEXT))
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.reset_zoom(&ResetZoom, window, cx)
                                }),
                            ),
                    )
                    .child(
                        Button::new("zoom-in")
                            .ghost()
                            .compact()
                            .label("+")
                            .tooltip_with_action("Zoom in", &ZoomIn, Some(KEY_CONTEXT))
                            .disabled(self.zoom_index == ZOOM_LEVELS.len() - 1)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_step(true, cx))),
                    ),
            );

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
            .on_action(cx.listener(Self::on_finish_polygon))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .size_full()
            .text_color(theme.foreground)
            .child(toolbar)
            .child(viewport)
            .child(status_bar)
    }
}

/// Paints `stroke` with the canvas top-left at `origin`, in window
/// coordinates, scaled by `zoom`.
fn paint_stroke(window: &mut Window, stroke: &Stroke, origin: Point<Pixels>, zoom: f32) {
    let Some(first) = stroke.points.first() else {
        return;
    };
    let to_window = |p: Point<Pixels>| origin + point(p.x * zoom, p.y * zoom);
    let trace = |builder: &mut PathBuilder| {
        builder.move_to(to_window(*first));
        if stroke.points.len() == 1 {
            // Draw a tiny segment so a single click still leaves a visible dot
            builder.line_to(to_window(*first + Point::new(px(0.1), px(0.1))));
        } else {
            for p in &stroke.points[1..] {
                builder.line_to(to_window(*p));
            }
        }
        if stroke.closed {
            builder.close();
        }
    };

    if stroke.filled {
        let mut builder = PathBuilder::fill();
        trace(&mut builder);
        if let Ok(path) = builder.build() {
            window.paint_path(path, stroke.color);
        }
    }
    // Round caps turn a single click into a dot; round joins avoid miter
    // spikes on sharp turns of freehand strokes
    let join = if stroke.sharp_corners {
        LineJoin::Miter
    } else {
        LineJoin::Round
    };
    let options = StrokeOptions::default()
        .with_line_width(stroke.width.as_f32() * zoom)
        .with_line_cap(LineCap::Round)
        .with_line_join(join);
    let mut builder = PathBuilder::default().with_style(PathStyle::Stroke(options));
    trace(&mut builder);
    if let Ok(path) = builder.build() {
        window.paint_path(path, stroke.color);
    }
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

/// Paints floating selection pixels with their top-left at `at`, in window
/// coordinates, sampled like the canvas view so they stay sharp.
fn paint_floating(
    window: &mut Window,
    pixels: &Pixmap,
    id: u64,
    at: Point<Pixels>,
    zoom: f32,
    cache: &mut ViewCache,
) {
    let scale = window.scale_factor();
    let pixels_per_canvas_pixel = scale * zoom;
    let key = ViewKey {
        version: id,
        pixels_per_canvas_pixel: pixels_per_canvas_pixel.to_bits(),
        origin: point(0, 0),
        size: size(pixels.width(), pixels.height()),
    };
    if cache.key != Some(key) {
        let (bytes, region) = canvas::render_pixmap(pixels, pixels_per_canvas_pixel);
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

/// Paints the visible part of `canvas`, displayed at `bounds` in window
/// coordinates and scaled by `zoom`.
///
/// gpui samples images with linear filtering, which would blur pixels when
/// the canvas is scaled up. So the view is rendered on the CPU at exactly the
/// device resolution and drawn 1:1.
fn paint_view(
    window: &mut Window,
    canvas: &Canvas,
    bounds: Bounds<Pixels>,
    zoom: f32,
    cache: &mut ViewCache,
) {
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
        pixels_per_canvas_pixel: (scale * zoom).to_bits(),
        origin: point(left as u32, top as u32),
        size: size((right - left) as u32, (bottom - top) as u32),
    };

    if cache.key != Some(key) {
        let bytes = canvas.render_view(key.origin, key.size, scale * zoom);
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
        cx.activate(true);
    });
}
