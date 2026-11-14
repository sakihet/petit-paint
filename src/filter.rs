//! Filters that change pixels already drawn, for a grainy, softened or
//! painted look.

use tiny_skia::{IntRect, Pixmap};

/// A filter that changes the pixels already on the canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Filter {
    /// Grain up to `amount`, from 0 to 1, either side of each color, fixed
    /// by `seed`: in color, or the same on every channel.
    Noise { amount: f32, color: bool, seed: u64 },
    /// A Gaussian blur reaching about `radius` pixels.
    Blur { radius: f32 },
    /// A Kuwahara filter: each pixel takes the average of whichever of the
    /// four squares `radius` pixels across at its corners is most even, so
    /// smooth shading breaks into flat patches while edges stay sharp.
    Painterly { radius: u32 },
    /// Colors shifted around the color wheel by `hue` degrees, then made
    /// warmer (orange) or cooler (blue), more or less saturated, brighter or
    /// darker, and of more or less contrast, each by an amount from -1 to 1
    /// where 0 leaves them as they are.
    Adjust(Adjustment),
}

/// How [`Filter::Adjust`] changes colors.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Adjustment {
    pub hue: f32,
    pub saturation: f32,
    pub brightness: f32,
    pub contrast: f32,
    pub warmth: f32,
}

/// A premultiplied pixel with channels from 0 to 1.
pub type Rgba = [f32; 4];

/// `pixels` inside `rect` after `filter`, row by row. Pixels past `rect`
/// are read too, as far as the filter reaches; past the image's edges, the
/// edge pixels carry on.
pub fn apply(filter: Filter, pixels: &Pixmap, rect: IntRect) -> Vec<Rgba> {
    match filter {
        Filter::Noise {
            amount,
            color,
            seed,
        } => noise(pixels, rect, amount, color, seed),
        Filter::Blur { radius } => blur(pixels, rect, radius),
        Filter::Painterly { radius } => painterly(pixels, rect, radius as i32),
        Filter::Adjust(adjustment) => adjust(pixels, rect, adjustment),
    }
}

/// The pixel at (`x`, `y`), or the nearest edge pixel past the image.
fn at(pixels: &Pixmap, x: i32, y: i32) -> Rgba {
    let x = x.clamp(0, pixels.width() as i32 - 1) as u32;
    let y = y.clamp(0, pixels.height() as i32 - 1) as u32;
    let p = pixels.pixel(x, y).expect("clamped inside the image");
    [p.red(), p.green(), p.blue(), p.alpha()].map(|c| c as f32 / 255.0)
}

fn noise(pixels: &Pixmap, rect: IntRect, amount: f32, color: bool, seed: u64) -> Vec<Rgba> {
    let mut out = Vec::with_capacity((rect.width() * rect.height()) as usize);
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            let pixel = at(pixels, x, y);
            let alpha = pixel[3];
            if alpha == 0.0 {
                out.push(pixel);
                continue;
            }
            // Two draws averaged, so small changes are more common than large ones
            let grain = |layer: u64| {
                let a = crate::canvas::noise(x, y, seed.wrapping_mul(8).wrapping_add(layer));
                let b = crate::canvas::noise(x, y, seed.wrapping_mul(8).wrapping_add(layer + 4));
                (a + b - 1.0) * amount
            };
            let gray = grain(0);
            let mut grained = [0.0; 4];
            for channel in 0..3 {
                let change = if color {
                    grain(channel as u64 + 1)
                } else {
                    gray
                };
                // Grain the color itself, then premultiply again
                let straight = (pixel[channel] / alpha + change).clamp(0.0, 1.0);
                grained[channel] = straight * alpha;
            }
            grained[3] = alpha;
            out.push(grained);
        }
    }
    out
}

fn blur(pixels: &Pixmap, rect: IntRect, radius: f32) -> Vec<Rgba> {
    let sigma = (radius / 2.0).max(0.1);
    let reach = (sigma * 3.0).ceil() as i32;
    let weights: Vec<f32> = (-reach..=reach)
        .map(|k| (-(k * k) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let total: f32 = weights.iter().sum();
    let weights: Vec<f32> = weights.iter().map(|w| w / total).collect();

    // Across first, for the rows the blur down will read
    let (top, bottom) = (rect.top() - reach, rect.bottom() + reach);
    let width = rect.width() as usize;
    let mut across = Vec::with_capacity(width * (bottom - top) as usize);
    for y in top..bottom {
        for x in rect.left()..rect.right() {
            let mut sum = [0.0; 4];
            for (k, weight) in (-reach..=reach).zip(&weights) {
                let pixel = at(pixels, x + k, y);
                for c in 0..4 {
                    sum[c] += pixel[c] * weight;
                }
            }
            across.push(sum);
        }
    }
    let mut out = Vec::with_capacity(width * rect.height() as usize);
    let rows = bottom - top;
    for y in 0..rect.height() as i32 {
        for x in 0..width {
            let mut sum = [0.0; 4];
            for (k, weight) in (-reach..=reach).zip(&weights) {
                // Rows past the image's edges repeat the edge row
                let image_y = (rect.top() + y + k).clamp(0, pixels.height() as i32 - 1);
                let row = (image_y - top).clamp(0, rows - 1) as usize;
                let pixel = across[row * width + x];
                for c in 0..4 {
                    sum[c] += pixel[c] * weight;
                }
            }
            out.push(sum);
        }
    }
    out
}

fn painterly(pixels: &Pixmap, rect: IntRect, radius: i32) -> Vec<Rgba> {
    // Sums of each channel, brightness and brightness squared over every box
    // from the area's top-left, so any square's mean and spread come from
    // four lookups
    let (left, top) = (rect.left() - radius, rect.top() - radius);
    let width = (rect.width() as i32 + 2 * radius) as usize;
    let height = (rect.height() as i32 + 2 * radius) as usize;
    let stride = width + 1;
    let mut sums = vec![[0.0f64; 6]; stride * (height + 1)];
    for y in 0..height {
        let mut row = [0.0f64; 6];
        for x in 0..width {
            let p = at(pixels, left + x as i32, top + y as i32);
            let brightness = (0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2]) as f64;
            let values = [
                p[0] as f64,
                p[1] as f64,
                p[2] as f64,
                p[3] as f64,
                brightness,
                brightness * brightness,
            ];
            for c in 0..6 {
                row[c] += values[c];
                sums[(y + 1) * stride + x + 1][c] = sums[y * stride + x + 1][c] + row[c];
            }
        }
    }
    // The sums over the square from (x0, y0) to (x1, y1), inclusive, in the
    // area's coordinates
    let square = |x0: usize, y0: usize, x1: usize, y1: usize| {
        let mut total = [0.0; 6];
        for (c, value) in total.iter_mut().enumerate() {
            *value = sums[(y1 + 1) * stride + x1 + 1][c]
                - sums[y0 * stride + x1 + 1][c]
                - sums[(y1 + 1) * stride + x0][c]
                + sums[y0 * stride + x0][c];
        }
        total
    };
    let r = radius as usize;
    let count = ((r + 1) * (r + 1)) as f64;
    let mut out = Vec::with_capacity((rect.width() * rect.height()) as usize);
    for y in 0..rect.height() as usize {
        for x in 0..rect.width() as usize {
            // This pixel, in the area's coordinates
            let (cx, cy) = (x + r, y + r);
            let corners = [
                (cx - r, cy - r, cx, cy),
                (cx, cy - r, cx + r, cy),
                (cx - r, cy, cx, cy + r),
                (cx, cy, cx + r, cy + r),
            ];
            let (_, best) = corners
                .iter()
                .map(|&(x0, y0, x1, y1)| {
                    let total = square(x0, y0, x1, y1);
                    let mean = total[4] / count;
                    (total[5] / count - mean * mean, total)
                })
                .fold((f64::MAX, [0.0; 6]), |best, next| {
                    if next.0 < best.0 { next } else { best }
                });
            out.push([0, 1, 2, 3].map(|c| (best[c] / count) as f32));
        }
    }
    out
}

fn adjust(pixels: &Pixmap, rect: IntRect, adjustment: Adjustment) -> Vec<Rgba> {
    let Adjustment {
        hue,
        saturation,
        brightness,
        contrast,
        warmth,
    } = adjustment;
    // Turning the hue around the gray axis, keeping brightness, as CSS's
    // hue-rotate does
    let (sin, cos) = hue.to_radians().sin_cos();
    let turn = [
        [
            0.213 + cos * 0.787 - sin * 0.213,
            0.715 - cos * 0.715 - sin * 0.715,
            0.072 - cos * 0.072 + sin * 0.928,
        ],
        [
            0.213 - cos * 0.213 + sin * 0.143,
            0.715 + cos * 0.285 + sin * 0.140,
            0.072 - cos * 0.072 - sin * 0.283,
        ],
        [
            0.213 - cos * 0.213 - sin * 0.787,
            0.715 - cos * 0.715 + sin * 0.715,
            0.072 + cos * 0.928 + sin * 0.072,
        ],
    ];
    // Full contrast pushes every color most of the way to black or white
    let spread = if contrast >= 0.0 {
        1.0 + contrast * 4.0
    } else {
        1.0 + contrast
    };
    let mut out = Vec::with_capacity((rect.width() * rect.height()) as usize);
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            let pixel = at(pixels, x, y);
            let alpha = pixel[3];
            if alpha == 0.0 {
                out.push(pixel);
                continue;
            }
            let mut c = [0, 1, 2].map(|channel| pixel[channel] / alpha);
            c[0] += warmth * 0.15;
            c[1] += warmth * 0.03;
            c[2] -= warmth * 0.15;
            if hue != 0.0 {
                c = turn.map(|row| row[0] * c[0] + row[1] * c[1] + row[2] * c[2]);
            }
            let gray = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
            let adjusted = c.map(|c| {
                let c = gray + (c - gray) * (1.0 + saturation);
                let c = if brightness >= 0.0 {
                    c + (1.0 - c) * brightness
                } else {
                    c * (1.0 + brightness)
                };
                ((c - 0.5) * spread + 0.5).clamp(0.0, 1.0) * alpha
            });
            out.push([adjusted[0], adjusted[1], adjusted[2], alpha]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::Color;

    /// A 20×10 image, black on the left half and white on the right.
    fn halves() -> Pixmap {
        let mut pixels = Pixmap::new(20, 10).unwrap();
        pixels.fill(Color::WHITE);
        let black = tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0, 255).unwrap();
        for y in 0..10 {
            for x in 0..10 {
                pixels.pixels_mut()[y * 20 + x] = black;
            }
        }
        pixels
    }

    fn whole(pixels: &Pixmap) -> IntRect {
        IntRect::from_xywh(0, 0, pixels.width(), pixels.height()).unwrap()
    }

    #[test]
    fn blur_softens_an_edge_and_keeps_flat_areas() {
        let pixels = halves();
        let out = apply(Filter::Blur { radius: 4.0 }, &pixels, whole(&pixels));
        let gray = |x: usize| out[5 * 20 + x][0];
        assert!(gray(0) < 0.01, "far from the edge stays black");
        assert!(gray(19) > 0.99, "far from the edge stays white");
        assert!((0.1..0.5).contains(&gray(9)), "{}", gray(9));
        assert!((0.5..0.9).contains(&gray(10)), "{}", gray(10));
        assert!(out.iter().all(|p| (p[3] - 1.0).abs() < 1e-4));
    }

    #[test]
    fn noise_is_fixed_by_its_seed() {
        let pixels = halves();
        let rect = whole(&pixels);
        let noise = |seed| {
            let filter = Filter::Noise {
                amount: 0.2,
                color: false,
                seed,
            };
            apply(filter, &pixels, rect)
        };
        assert_eq!(noise(1), noise(1));
        assert_ne!(noise(1), noise(2));
        let grained = noise(1);
        let white = &grained[10..20];
        assert!(white.iter().any(|p| p[0] < 0.95), "white gets grain");
        assert!(white.iter().all(|p| p[0] >= 0.8 - 1e-4));
        assert!(
            white.iter().all(|p| p[0] == p[1] && p[1] == p[2]),
            "gray grain"
        );
    }

    #[test]
    fn painterly_keeps_an_edge_sharp_and_fades_specks() {
        let mut pixels = halves();
        // A speck of gray in the white half
        pixels.pixels_mut()[5 * 20 + 15] =
            tiny_skia::PremultipliedColorU8::from_rgba(128, 128, 128, 255).unwrap();
        let out = apply(Filter::Painterly { radius: 2 }, &pixels, whole(&pixels));
        let gray = |x: usize| out[5 * 20 + x][0];
        assert!(gray(9) < 0.01 && gray(10) > 0.99, "the edge stays sharp");
        // Every square holds the speck, but spread over the square
        assert!(gray(15) > 0.9, "the speck fades: {}", gray(15));
    }

    /// `rgb`, opaque, after `adjustment`.
    fn adjusted(rgb: [u8; 3], adjustment: Adjustment) -> [u8; 3] {
        let mut pixels = Pixmap::new(1, 1).unwrap();
        pixels.pixels_mut()[0] =
            tiny_skia::PremultipliedColorU8::from_rgba(rgb[0], rgb[1], rgb[2], 255).unwrap();
        let out = apply(Filter::Adjust(adjustment), &pixels, whole(&pixels));
        [0, 1, 2].map(|c| (out[0][c] * 255.0).round() as u8)
    }

    #[test]
    fn adjusting_nothing_keeps_colors() {
        assert_eq!(
            adjusted([200, 100, 50], Adjustment::default()),
            [200, 100, 50]
        );
    }

    #[test]
    fn adjusts_hue_saturation_brightness_contrast_and_warmth() {
        let red = [200, 40, 40];
        let turned = adjusted(
            red,
            Adjustment {
                hue: 120.0,
                ..Default::default()
            },
        );
        assert!(turned[1] > turned[0] && turned[1] > turned[2], "{turned:?}");
        let gray = adjusted(
            red,
            Adjustment {
                saturation: -1.0,
                ..Default::default()
            },
        );
        assert!(gray[0] == gray[1] && gray[1] == gray[2], "{gray:?}");
        let vivid = adjusted(
            [150, 100, 100],
            Adjustment {
                saturation: 0.5,
                ..Default::default()
            },
        );
        assert!(vivid[0] > 150 && vivid[1] < 100, "{vivid:?}");
        let brighter = adjusted(
            red,
            Adjustment {
                brightness: 0.5,
                ..Default::default()
            },
        );
        assert_eq!(brighter, [228, 148, 148]);
        let darker = adjusted(
            red,
            Adjustment {
                brightness: -0.5,
                ..Default::default()
            },
        );
        assert_eq!(darker, [100, 20, 20]);
        let flat = adjusted(
            [255, 0, 128],
            Adjustment {
                contrast: -1.0,
                ..Default::default()
            },
        );
        assert_eq!(flat, [128, 128, 128]);
        let punchy = adjusted(
            [160, 96, 128],
            Adjustment {
                contrast: 0.5,
                ..Default::default()
            },
        );
        assert!(punchy[0] > 200 && punchy[1] < 40, "{punchy:?}");
        let warm = adjusted(
            [128, 128, 128],
            Adjustment {
                warmth: 1.0,
                ..Default::default()
            },
        );
        assert!(warm[0] > 128 && warm[2] < 128, "{warm:?}");
        let cool = adjusted(
            [128, 128, 128],
            Adjustment {
                warmth: -1.0,
                ..Default::default()
            },
        );
        assert!(cool[0] < 128 && cool[2] > 128, "{cool:?}");
    }

    #[test]
    fn adjusting_keeps_transparency() {
        let mut pixels = Pixmap::new(2, 1).unwrap();
        pixels.pixels_mut()[1] =
            tiny_skia::PremultipliedColorU8::from_rgba(50, 25, 0, 128).unwrap();
        let brighter = Adjustment {
            brightness: 0.5,
            ..Default::default()
        };
        let out = apply(Filter::Adjust(brighter), &pixels, whole(&pixels));
        assert_eq!(out[0], [0.0; 4]);
        assert_eq!(out[1][3], 128.0 / 255.0);
        assert!(out[1].iter().all(|&c| c <= out[1][3]));
    }

    #[test]
    fn filters_only_the_rectangle() {
        let pixels = halves();
        let rect = IntRect::from_xywh(8, 2, 4, 3).unwrap();
        let out = apply(Filter::Blur { radius: 2.0 }, &pixels, rect);
        assert_eq!(out.len(), 12);
    }
}
