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

    #[test]
    fn filters_only_the_rectangle() {
        let pixels = halves();
        let rect = IntRect::from_xywh(8, 2, 4, 3).unwrap();
        let out = apply(Filter::Blur { radius: 2.0 }, &pixels, rect);
        assert_eq!(out.len(), 12);
    }
}
