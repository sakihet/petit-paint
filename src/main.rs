use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, h_flex,
    button::*,
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    slider::{Slider, SliderEvent, SliderState, SliderValue},
    v_flex,
};
use gpui_kit::*;

actions!(petit_paint, [Undo]);

const DEFAULT_WIDTH: f32 = 4.0;
const KEY_CONTEXT: &str = "PaintApp";

// KeyBinding::new cannot resolve "secondary-", so pick the modifier per OS
#[cfg(target_os = "macos")]
const UNDO_KEY: &str = "cmd-z";
#[cfg(not(target_os = "macos"))]
const UNDO_KEY: &str = "ctrl-z";

/// A single brush stroke. Points are stored in window coordinates.
struct Stroke {
    points: Vec<Point<Pixels>>,
    color: Hsla,
    width: Pixels,
}

/// An undoable change to the drawing.
enum HistoryEntry {
    /// A stroke was appended to the end of `strokes`.
    Stroke,
    /// The canvas was cleared; holds the strokes that were removed.
    Clear(Vec<Stroke>),
}

struct PaintApp {
    strokes: Vec<Stroke>,
    history: Vec<HistoryEntry>,
    drawing: bool,
    focus_handle: FocusHandle,
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
            strokes: Vec::new(),
            history: Vec::new(),
            drawing: false,
            focus_handle: cx.focus_handle(),
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
        self.drawing = true;
        self.strokes.push(Stroke {
            points: vec![event.position],
            color: self.color,
            width: self.width,
        });
        self.history.push(HistoryEntry::Stroke);
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.drawing {
            return;
        }
        if let Some(stroke) = self.strokes.last_mut() {
            stroke.points.push(event.position);
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.drawing = false;
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        if self.strokes.is_empty() {
            return;
        }
        let removed = std::mem::take(&mut self.strokes);
        self.history.push(HistoryEntry::Clear(removed));
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        // Undoing mid-drag would pop the stroke being drawn
        if self.drawing {
            return;
        }
        match self.history.pop() {
            Some(HistoryEntry::Stroke) => {
                self.strokes.pop();
            }
            Some(HistoryEntry::Clear(removed)) => self.strokes = removed,
            None => return,
        }
        cx.notify();
    }
}

impl Render for PaintApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Copy the stroke data so it can be moved into the paint closure
        let strokes: Vec<(Vec<Point<Pixels>>, Hsla, Pixels)> = self
            .strokes
            .iter()
            .map(|s| (s.points.clone(), s.color, s.width))
            .collect();

        let theme = cx.theme();

        let toolbar = h_flex()
            .gap_3()
            .px_3()
            .h(px(44.0))
            .bg(theme.background)
            .border_b_1()
            .border_color(theme.border)
            .child("petit-paint")
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
                Button::new("clear")
                    .outline()
                    .label("Clear")
                    .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
            );

        let canvas_area = div()
            .flex_1()
            .size_full()
            .bg(rgb(0xffffff))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        window.paint_layer(bounds, |window| {
                            for (points, color, width) in &strokes {
                                paint_stroke(window, points, *color, *width);
                            }
                        });
                    },
                )
                .size_full(),
            );

        v_flex()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::undo))
            .size_full()
            .text_color(theme.foreground)
            .child(toolbar)
            .child(canvas_area)
    }
}

fn paint_stroke(window: &mut Window, points: &[Point<Pixels>], color: Hsla, width: Pixels) {
    let Some(first) = points.first() else {
        return;
    };
    let mut builder = PathBuilder::stroke(width);
    builder.move_to(*first);
    if points.len() == 1 {
        // Draw a tiny segment so a single click still leaves a visible dot
        builder.line_to(*first + Point::new(px(0.1), px(0.1)));
    } else {
        for p in &points[1..] {
            builder.line_to(*p);
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        cx.bind_keys([KeyBinding::new(UNDO_KEY, Undo, Some(KEY_CONTEXT))]);
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
