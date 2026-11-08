use cosmic_text::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache};
use gpui_kit::Hsla;
use tiny_skia::{Pixmap, PremultipliedColorU8};

/// Font choices for the text tool. These are the generic CSS families, each
/// mapped to an installed font, so the list works everywhere without bundling
/// font files. Characters a font lacks, such as Japanese in a Latin font,
/// fall back to an installed font that has them.
#[derive(Clone, Copy, PartialEq)]
pub enum FontChoice {
    SansSerif,
    Serif,
    Monospace,
    Cursive,
    Fantasy,
}

/// Fonts in toolbar order.
pub const FONTS: [FontChoice; 5] = [
    FontChoice::SansSerif,
    FontChoice::Serif,
    FontChoice::Monospace,
    FontChoice::Cursive,
    FontChoice::Fantasy,
];

impl FontChoice {
    pub fn label(self) -> &'static str {
        match self {
            FontChoice::SansSerif => "Sans",
            FontChoice::Serif => "Serif",
            FontChoice::Monospace => "Mono",
            FontChoice::Cursive => "Cursive",
            FontChoice::Fantasy => "Fantasy",
        }
    }

    /// Installed fonts to use for this choice, most preferred first, across
    /// macOS, Windows and Linux.
    fn candidates(self) -> &'static [&'static str] {
        match self {
            FontChoice::SansSerif => &[
                "Helvetica Neue",
                "Helvetica",
                "Arial",
                "Segoe UI",
                "DejaVu Sans",
                "Liberation Sans",
                "Noto Sans",
                "Open Sans",
            ],
            FontChoice::Serif => &[
                "Times New Roman",
                "Times",
                "Georgia",
                "DejaVu Serif",
                "Liberation Serif",
                "Noto Serif",
            ],
            FontChoice::Monospace => &[
                "Menlo",
                "Consolas",
                "Courier New",
                "DejaVu Sans Mono",
                "Liberation Mono",
                "Noto Sans Mono",
            ],
            FontChoice::Cursive => &[
                "Apple Chancery",
                "Snell Roundhand",
                "Segoe Script",
                "Comic Sans MS",
                "URW Chancery L",
            ],
            FontChoice::Fantasy => &["Papyrus", "Impact", "Luminari", "Copperplate"],
        }
    }

    fn family(self) -> Family<'static> {
        match self {
            FontChoice::SansSerif => Family::SansSerif,
            FontChoice::Serif => Family::Serif,
            FontChoice::Monospace => Family::Monospace,
            FontChoice::Cursive => Family::Cursive,
            FontChoice::Fantasy => Family::Fantasy,
        }
    }
}

/// Lays out and rasterizes text. Creating one scans the installed fonts,
/// which takes a moment, so it is made on first use and kept.
pub struct TextRenderer {
    font_system: FontSystem,
    glyphs: SwashCache,
}

impl TextRenderer {
    pub fn new() -> Self {
        let mut font_system = FontSystem::new();
        remove_unrenderable_faces(font_system.db_mut());
        // cosmic-text's defaults name Linux fonts, which other systems lack.
        // Point each generic family at the first candidate that text actually
        // renders with: a font can be installed yet never matched, such as
        // one whose only face has an unusual weight.
        for font in FONTS {
            for name in font.candidates() {
                set_family(font_system.db_mut(), font, name);
                if renders_with(&mut font_system, font, name) {
                    break;
                }
            }
        }
        Self {
            font_system,
            glyphs: SwashCache::new(),
        }
    }

    /// Renders `text` on a transparent pixmap just large enough to hold it,
    /// one pixel per canvas pixel. Lines are separated by `\n`. Returns `None`
    /// for text with nothing to draw.
    ///
    /// Also returns the margin left of where the text starts, so the pixmap
    /// can be placed that far left of the point the text should start at.
    pub fn render(
        &mut self,
        text: &str,
        font: FontChoice,
        size: f32,
        color: Hsla,
    ) -> Option<(Pixmap, u32)> {
        let metrics = Metrics::new(size, (size * 1.2).ceil());
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        // No wrapping: each line is as wide as its text
        buffer.set_size(None, None);
        let attrs = Attrs::new().family(font.family());
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.font_system, false);

        let (width, height) = buffer.layout_runs().fold((0.0f32, 0.0f32), |(w, h), run| {
            (w.max(run.line_w), h.max(run.line_top + run.line_height))
        });
        if width <= 0.0 {
            return None;
        }
        // Glyphs such as italics can overhang their advance, so leave a margin
        let margin = (size / 4.0).ceil();
        let mut pixmap = Pixmap::new(
            (width + margin * 2.0).ceil() as u32,
            (height + margin).ceil() as u32,
        )?;

        let rgba = color.to_rgb();
        let to_u8 = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        let color = Color::rgba(to_u8(rgba.r), to_u8(rgba.g), to_u8(rgba.b), to_u8(rgba.a));
        let (pixmap_width, pixmap_height) = (pixmap.width() as i32, pixmap.height() as i32);
        let offset = margin as i32;
        let pixels = pixmap.pixels_mut();
        buffer.draw(
            &mut self.font_system,
            &mut self.glyphs,
            color,
            |x, y, w, h, color| {
                // Called per covered pixel with coverage folded into alpha
                for dy in 0..h as i32 {
                    for dx in 0..w as i32 {
                        let (px, py) = (x + dx + offset, y + dy);
                        if px < 0 || py < 0 || px >= pixmap_width || py >= pixmap_height {
                            continue;
                        }
                        let target = &mut pixels[(py * pixmap_width + px) as usize];
                        *target = source_over(color, *target);
                    }
                }
            },
        );
        Some((pixmap, offset as u32))
    }
}

fn set_family(db: &mut cosmic_text::fontdb::Database, font: FontChoice, name: &str) {
    let name = name.to_string();
    match font {
        FontChoice::SansSerif => db.set_sans_serif_family(name),
        FontChoice::Serif => db.set_serif_family(name),
        FontChoice::Monospace => db.set_monospace_family(name),
        FontChoice::Cursive => db.set_cursive_family(name),
        FontChoice::Fantasy => db.set_fantasy_family(name),
    }
}

/// Whether text in `font` is laid out with the font family `name`.
fn renders_with(font_system: &mut FontSystem, font: FontChoice, name: &str) -> bool {
    let mut buffer = Buffer::new(font_system, Metrics::new(16.0, 20.0));
    buffer.set_text("a", &Attrs::new().family(font.family()), Shaping::Advanced, None);
    buffer.shape_until_scroll(font_system, false);
    let font_id = buffer
        .layout_runs()
        .next()
        .and_then(|run| run.glyphs.first())
        .map(|glyph| glyph.font_id);
    font_id
        .and_then(|id| font_system.db().face(id))
        .is_some_and(|face| face.families.iter().any(|(family, _)| family == name))
}

/// Drops fonts whose glyphs cannot be drawn, such as macOS's GB18030Bitmap,
/// which only has monochrome bitmaps. Fallback can otherwise pick one for
/// characters like Japanese, and the text comes out empty.
fn remove_unrenderable_faces(db: &mut cosmic_text::fontdb::Database) {
    let unrenderable: Vec<_> = db
        .faces()
        .filter(|face| {
            let drawable = db.with_face_data(face.id, |data, index| {
                let Ok(face) = ttf_parser::Face::parse(data, index) else {
                    return false;
                };
                let tables = face.tables();
                tables.glyf.is_some()
                    || tables.cff.is_some()
                    || tables.cff2.is_some()
                    // Color bitmaps, as in emoji fonts
                    || tables.sbix.is_some()
                    || tables.cbdt.is_some()
            });
            drawable != Some(true)
        })
        .map(|face| face.id)
        .collect();
    for id in unrenderable {
        db.remove_face(id);
    }
}

/// Blends straight-alpha `color` over premultiplied `target`.
fn source_over(color: Color, target: PremultipliedColorU8) -> PremultipliedColorU8 {
    let alpha = color.a() as u16;
    let keep = 255 - alpha;
    let blend = |c: u8, t: u8| ((c as u16 * alpha + t as u16 * keep + 127) / 255) as u8;
    PremultipliedColorU8::from_rgba(
        blend(color.r(), target.red()),
        blend(color.g(), target.green()),
        blend(color.b(), target.blue()),
        (alpha + (target.alpha() as u16 * keep + 127) / 255) as u8,
    )
    .unwrap_or(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::rgb;

    fn coverage(pixmap: &Pixmap) -> usize {
        pixmap.pixels().iter().filter(|p| p.alpha() > 0).count()
    }

    fn render(text: &str, font: FontChoice, size: f32) -> Option<Pixmap> {
        let black = rgb(0x000000).into();
        let (pixmap, _) = TextRenderer::new().render(text, font, size, black)?;
        Some(pixmap)
    }

    #[test]
    fn renders_latin_and_japanese_text_in_every_font() {
        for font in FONTS {
            let latin = render("Hello", font, 32.0).unwrap();
            assert!(coverage(&latin) > 0, "{}", font.label());
            // Japanese falls back to an installed font that has the glyphs
            let japanese = render("こんにちは", font, 32.0).unwrap();
            assert!(coverage(&japanese) > 0, "{}", font.label());
        }
    }

    #[test]
    fn size_scales_the_image() {
        let small = render("Hi", FontChoice::Serif, 16.0).unwrap();
        let large = render("Hi", FontChoice::Serif, 64.0).unwrap();
        assert!(large.width() > small.width() * 3);
        assert!(large.height() > small.height() * 3);
    }

    #[test]
    fn multiple_lines_stack_vertically() {
        let one = render("A", FontChoice::Monospace, 20.0).unwrap();
        let two = render("A\nA", FontChoice::Monospace, 20.0).unwrap();
        assert!(two.height() > one.height());
    }

    #[test]
    fn every_font_choice_renders_in_its_own_font() {
        let mut renderer = TextRenderer::new();
        for font in FONTS {
            let name = renderer.font_system.db().family_name(&font.family()).to_string();
            assert!(
                renders_with(&mut renderer.font_system, font, &name),
                "{} renders in a fallback font instead of {name}",
                font.label()
            );
        }
        let images: Vec<_> = FONTS
            .iter()
            .map(|&font| render("Hamburgefonts", font, 32.0).unwrap().data().to_vec())
            .collect();
        for (i, a) in images.iter().enumerate() {
            for b in &images[i + 1..] {
                assert_ne!(a, b, "two font choices look the same");
            }
        }
    }

    #[test]
    fn empty_text_draws_nothing() {
        assert!(render("", FontChoice::SansSerif, 20.0).is_none());
    }
}
