use gpui_kit::Action;
use gpui_kit::assets::IconName;

use crate::canvas::Brush;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tool {
    /// Selects a rectangle to move, copy, cut or delete.
    Select,
    /// Draws freehand with the brush chosen in the toolbar.
    Pen,
    /// Erases to transparency.
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

/// Switches to a tool, from its single-key shortcut.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = petit_paint, no_json)]
pub struct SelectTool(pub Tool);

/// Tools in toolbar order.
pub const TOOLS: [Tool; 7] = [
    Tool::Select,
    Tool::Pen,
    Tool::Eraser,
    Tool::Eyedropper,
    Tool::Fill,
    Tool::Shape,
    Tool::Text,
];

impl Tool {
    pub fn icon(self) -> IconName {
        match self {
            Tool::Select => IconName::SquareDashed,
            Tool::Pen => IconName::Paintbrush,
            Tool::Eraser => IconName::Eraser,
            Tool::Eyedropper => IconName::Pipette,
            Tool::Fill => IconName::PaintBucket,
            Tool::Shape => IconName::Shapes,
            Tool::Text => IconName::Type,
        }
    }

    /// The key that switches to this tool, as in Photoshop.
    pub fn key(self) -> &'static str {
        match self {
            Tool::Select => "v",
            Tool::Pen => "b",
            Tool::Eraser => "e",
            Tool::Eyedropper => "i",
            Tool::Fill => "g",
            Tool::Shape => "u",
            Tool::Text => "t",
        }
    }

    pub fn label(self) -> &'static str {
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

#[derive(Clone, Copy, PartialEq)]
pub enum ShapeKind {
    Line,
    Rectangle,
    Ellipse,
    /// Built by clicking each corner; closes on a double-click, Enter, or a
    /// click on the first corner.
    Polygon,
}

/// Shapes in toolbar order.
pub const SHAPES: [ShapeKind; 4] = [
    ShapeKind::Line,
    ShapeKind::Rectangle,
    ShapeKind::Ellipse,
    ShapeKind::Polygon,
];

impl ShapeKind {
    pub fn label(self) -> &'static str {
        match self {
            ShapeKind::Line => "Line",
            ShapeKind::Rectangle => "Rectangle",
            ShapeKind::Ellipse => "Ellipse",
            ShapeKind::Polygon => "Polygon",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            ShapeKind::Line => IconName::Slash,
            ShapeKind::Rectangle => IconName::Square,
            ShapeKind::Ellipse => IconName::Circle,
            ShapeKind::Polygon => IconName::Pentagon,
        }
    }

    /// Whether the shape has an inside that can be filled.
    pub fn fillable(self) -> bool {
        self != ShapeKind::Line
    }
}

pub fn brush_icon(brush: Brush) -> IconName {
    match brush {
        Brush::Pen => IconName::Pen,
        Brush::Pencil => IconName::Pencil,
        Brush::NaturalPencil => IconName::PencilLine,
        Brush::Crayon => IconName::Brush,
        Brush::Marker => IconName::Highlighter,
        Brush::Airbrush => IconName::SprayCan,
        Brush::Calligraphy => IconName::PenTool,
        Brush::Oil => IconName::Palette,
        Brush::Watercolor => IconName::Droplet,
        Brush::Smudge => IconName::Brush,
    }
}
