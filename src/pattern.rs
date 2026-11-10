/// An 8x8 two-color pattern for the fill tool, tiled from the canvas's
/// top-left so that neighboring fills meet seamlessly. Each row is a byte
/// with the leftmost pixel in the high bit; set bits take color 1 and clear
/// bits color 2.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pattern {
    pub name: &'static str,
    pub rows: [u8; 8],
}

impl Pattern {
    pub const SOLID: Pattern = Pattern {
        name: "Solid",
        rows: [0xff; 8],
    };

    /// Whether the pattern is color 1 alone.
    pub fn is_solid(self) -> bool {
        self.rows == Self::SOLID.rows
    }

    /// Whether the canvas pixel at (`x`, `y`) takes color 1.
    pub fn is_set(self, x: usize, y: usize) -> bool {
        (self.rows[y % 8] >> (7 - x % 8)) & 1 == 1
    }
}

/// Patterns in the order the options bar shows them.
pub const PATTERNS: [Pattern; 15] = [
    Pattern::SOLID,
    Pattern {
        name: "Dark",
        rows: [0x77, 0xdd, 0x77, 0xdd, 0x77, 0xdd, 0x77, 0xdd],
    },
    Pattern {
        name: "Checkerboard",
        rows: [0xaa, 0x55, 0xaa, 0x55, 0xaa, 0x55, 0xaa, 0x55],
    },
    Pattern {
        name: "Light",
        rows: [0x88, 0x22, 0x88, 0x22, 0x88, 0x22, 0x88, 0x22],
    },
    Pattern {
        name: "Sparse dots",
        rows: [0x80, 0x00, 0x08, 0x00, 0x80, 0x00, 0x08, 0x00],
    },
    Pattern {
        name: "Horizontal lines",
        rows: [0xff, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00],
    },
    Pattern {
        name: "Vertical lines",
        rows: [0x88; 8],
    },
    Pattern {
        name: "Rising diagonals",
        rows: [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80],
    },
    Pattern {
        name: "Falling diagonals",
        rows: [0x80, 0x40, 0x20, 0x10, 0x08, 0x04, 0x02, 0x01],
    },
    Pattern {
        name: "Grid",
        rows: [0xff, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80],
    },
    Pattern {
        name: "Crosshatch",
        rows: [0x81, 0x42, 0x24, 0x18, 0x18, 0x24, 0x42, 0x81],
    },
    Pattern {
        name: "Bricks",
        rows: [0xff, 0x80, 0x80, 0x80, 0xff, 0x08, 0x08, 0x08],
    },
    Pattern {
        name: "Zigzag",
        rows: [0x18, 0x24, 0x42, 0x81, 0x00, 0x00, 0x00, 0x00],
    },
    Pattern {
        name: "Polka dots",
        rows: [0x00, 0x18, 0x3c, 0x3c, 0x18, 0x00, 0x00, 0x00],
    },
    Pattern {
        name: "Weave",
        rows: [0xf0, 0x10, 0x10, 0x10, 0x0f, 0x01, 0x01, 0x01],
    },
];
