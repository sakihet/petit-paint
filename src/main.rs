use gpui::{
    canvas, div, prelude::*, px, rgb, size, App, Application, Bounds, Context, Hsla,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point,
    Window, WindowBounds, WindowOptions,
};

/// A single brush stroke. Points are stored in window coordinates.
struct Stroke {
    points: Vec<Point<Pixels>>,
    color: Hsla,
    width: Pixels,
}

struct PaintApp {
    strokes: Vec<Stroke>,
    drawing: bool,
    color: Hsla,
    width: Pixels,
}

impl PaintApp {
    fn new() -> Self {
        Self {
            strokes: Vec::new(),
            drawing: false,
            color: rgb(0x222222).into(),
            width: px(4.0),
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.drawing = true;
        self.strokes.push(Stroke {
            points: vec![event.position],
            color: self.color,
            width: self.width,
        });
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
        self.strokes.clear();
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

        let toolbar = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(40.0))
            .bg(rgb(0xeeeeee))
            .border_b_1()
            .border_color(rgb(0xcccccc))
            .child("petit-paint")
            .child(
                div()
                    .id("clear")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(0xffffff))
                    .border_1()
                    .border_color(rgb(0xbbbbbb))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0xf5f5f5)))
                    .child("Clear")
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

        div()
            .flex()
            .flex_col()
            .size_full()
            .text_color(rgb(0x333333))
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
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.0), px(640.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| PaintApp::new()),
        )
        .unwrap();
        cx.activate(true);
    });
}
