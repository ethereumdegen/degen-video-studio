//! Titles: SVG in, frames out.
//!
//! Titles are SVG documents rather than a bespoke text model, for three reasons that all
//! matter to an agent: SVG is what degen-paint already writes, so a vector document
//! authored there drops straight in; text, shapes, gradients and masks come for free
//! instead of being reimplemented; and the document stays human-readable in
//! `project.json`, so an agent can edit a title by editing text.
//!
//! The rasterizer also answers the questions an agent cannot see the answer to: where each
//! text run actually landed, and whether the font it asked for existed. A title that
//! silently fell back to a different family, or whose text overflowed its box, is the most
//! common way an unattended render comes out wrong.

use dvs_core::error::{Error, Result};
use dvs_media::Frame;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

/// Generic CSS families mapped to one deterministic face each. Leaving them to fontdb's
/// system resolution makes a render depend on what happens to be installed, which breaks
/// golden comparisons across machines.
const GENERIC_FAMILIES: &[&str] = &["sans-serif", "serif", "monospace", "cursive", "fantasy"];

/// System fonts are loaded once: `load_system_fonts` walks the font directories and costs
/// tens of milliseconds, and the set cannot change usefully mid-render.
static FONTS: LazyLock<Arc<fontdb::Database>> = LazyLock::new(|| {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    Arc::new(db)
});

/// What a rendered text run turned out to be. Feeds the digest and the title lints.
#[derive(Debug, Clone, PartialEq)]
pub struct TextReport {
    pub text: String,
    /// `[x, y, width, height]` in frame pixels.
    pub bbox: [f32; 4],
    /// Families the document asked for, in source order.
    pub requested: Vec<String>,
    /// Families the renderer actually drew with, after resolution.
    pub used: Vec<String>,
    /// Set when none of the requested families existed, naming the substitute. An
    /// unnoticed substitution is the most common way an unattended render comes out
    /// looking wrong, so it is reported rather than left to be spotted.
    pub fallback: Option<String>,
    pub font_size: f32,
}

/// Rasterizes title SVGs and caches the result per (document, size).
///
/// The cache is keyed on the *resolved* SVG text, so changing a field invalidates exactly
/// the titles that use it and nothing else.
pub struct Rasterizer {
    fonts: Arc<fontdb::Database>,
    default_family: String,
    cache: Mutex<HashMap<(u64, [u32; 2]), Frame>>,
}

impl Default for Rasterizer {
    fn default() -> Self {
        Rasterizer::new()
    }
}

impl Rasterizer {
    pub fn new() -> Rasterizer {
        let fonts = FONTS.clone();
        let default_family = pick_default_family(&fonts);
        Rasterizer {
            fonts,
            default_family,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The family generic requests resolve to on this machine.
    pub fn default_family(&self) -> &str {
        &self.default_family
    }

    fn options(&self) -> usvg::Options<'static> {
        let mut options = usvg::Options {
            font_family: self.default_family.clone(),
            fontdb: self.fonts.clone(),
            ..usvg::Options::default()
        };
        // `font-family: sans-serif` must land somewhere specific and reproducible.
        let db = options.fontdb_mut();
        for generic in GENERIC_FAMILIES {
            db.set_serif_family(self.default_family.clone());
            db.set_sans_serif_family(self.default_family.clone());
            db.set_cursive_family(self.default_family.clone());
            db.set_fantasy_family(self.default_family.clone());
            let _ = generic;
        }
        if let Some(mono) = pick_family(&self.fonts, &["DejaVu Sans Mono", "Liberation Mono", "Menlo", "Courier New"])
        {
            options.fontdb_mut().set_monospace_family(mono);
        }
        options
    }

    fn parse(&self, svg: &str) -> Result<usvg::Tree> {
        usvg::Tree::from_str(svg, &self.options())
            .map_err(|e| Error::op(format!("title is not parseable SVG: {e}")))
    }

    /// Render a title at `size`, scaling the document to fit (preserving its aspect).
    pub fn rasterize(&self, svg: &str, size: [u32; 2]) -> Result<Frame> {
        let key = (hash_svg(svg), size);
        if let Some(frame) = self.cache.lock().expect("rasterizer cache").get(&key) {
            return Ok(frame.clone());
        }
        let tree = self.parse(svg)?;
        let frame = render_tree(&tree, size)?;
        self.cache
            .lock()
            .expect("rasterizer cache")
            .insert(key, frame.clone());
        Ok(frame)
    }

    /// Where the text ended up and which font was used. Costs a parse, no rasterization.
    ///
    /// Substitution is detected against the *source* declarations, not against what usvg
    /// reports: usvg resolves `font-family` while parsing and rewrites the span to the
    /// family it actually found, so by the time a `Text` node exists the original request
    /// is gone and a naive "is the reported family installed" check can never fail.
    pub fn text_reports(&self, svg: &str, size: [u32; 2]) -> Result<Vec<TextReport>> {
        let tree = self.parse(svg)?;
        let scale = fit_scale(&tree, size);
        let declared = declared_families(svg);
        // A document whose every declared family is either generic or installed got what
        // it asked for; otherwise whatever usvg used is a substitute.
        let satisfied = declared.is_empty()
            || declared
                .iter()
                .any(|family| has_family(&self.fonts, family));
        let mut reports = Vec::new();
        collect_text(
            tree.root(),
            scale,
            &declared,
            satisfied,
            &self.default_family,
            &mut reports,
        );
        Ok(reports)
    }

    /// Intrinsic size of a title document, for placing it without guessing.
    pub fn document_size(&self, svg: &str) -> Result<[f32; 2]> {
        let tree = self.parse(svg)?;
        Ok([tree.size().width(), tree.size().height()])
    }
}

fn hash_svg(svg: &str) -> u64 {
    // A 64-bit content hash is enough for a per-process cache key and much cheaper to
    // compare than the whole document.
    let digest = blake_like(svg.as_bytes());
    digest
}

fn blake_like(bytes: &[u8]) -> u64 {
    // FNV-1a: no dependency, good enough for a cache key, and deterministic.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn fit_scale(tree: &usvg::Tree, size: [u32; 2]) -> f32 {
    let doc = tree.size();
    if doc.width() <= 0.0 || doc.height() <= 0.0 {
        return 1.0;
    }
    (size[0] as f32 / doc.width()).min(size[1] as f32 / doc.height())
}

fn render_tree(tree: &usvg::Tree, size: [u32; 2]) -> Result<Frame> {
    let (width, height) = (size[0].max(1), size[1].max(1));
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| Error::op(format!("cannot allocate a {width}x{height} title raster")))?;
    let scale = fit_scale(tree, size);
    // Center the scaled document, so a 16:9 title in a 1:1 frame is centered rather than
    // pinned to a corner.
    let doc = tree.size();
    let offset_x = (width as f32 - doc.width() * scale) / 2.0;
    let offset_y = (height as f32 - doc.height() * scale) / 2.0;
    let transform = tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, offset_x, offset_y);
    resvg::render(tree, transform, &mut pixmap.as_mut());
    Ok(pixmap_to_frame(&pixmap))
}

/// tiny-skia stores premultiplied sRGB bytes; the compositor wants premultiplied linear
/// floats. Unpremultiply, linearize, re-premultiply — skipping the unpremultiply step
/// darkens every antialiased edge.
fn pixmap_to_frame(pixmap: &tiny_skia::Pixmap) -> Frame {
    let mut pixels = Vec::with_capacity(pixmap.pixels().len() * 4);
    for pixel in pixmap.pixels() {
        let alpha = pixel.alpha() as f32 / 255.0;
        if alpha <= 0.0 {
            pixels.extend_from_slice(&[0.0, 0.0, 0.0, 0.0]);
            continue;
        }
        let demul = |value: u8| (value as f32 / 255.0) / alpha;
        pixels.extend_from_slice(&[
            srgb_to_linear(demul(pixel.red())) * alpha,
            srgb_to_linear(demul(pixel.green())) * alpha,
            srgb_to_linear(demul(pixel.blue())) * alpha,
            alpha,
        ]);
    }
    Frame::from_pixels(pixmap.width(), pixmap.height(), pixels)
}

fn srgb_to_linear(value: f32) -> f32 {
    let v = value.clamp(0.0, 1.0);
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn collect_text(
    group: &usvg::Group,
    scale: f32,
    declared: &[String],
    satisfied: bool,
    substitute: &str,
    out: &mut Vec<TextReport>,
) {
    for node in group.children() {
        match node {
            usvg::Node::Text(text) => {
                let bbox = text.abs_bounding_box();
                // Post-resolution families: what the renderer actually drew with.
                let used: Vec<String> = text
                    .chunks()
                    .iter()
                    .flat_map(|chunk| chunk.spans())
                    .flat_map(|span| span.font().families().iter().map(family_name))
                    .collect();
                let font_size = text
                    .chunks()
                    .iter()
                    .flat_map(|chunk| chunk.spans())
                    .map(|span| span.font_size().get())
                    .fold(0.0f32, f32::max);
                // usvg keeps the *declared* family on the span even when the database has
                // no matching face, so the substitute cannot be read back out of the tree:
                // it is the deterministic default this rasterizer configures.
                let fallback = if satisfied {
                    None
                } else {
                    Some(substitute.to_string())
                };
                out.push(TextReport {
                    // `usvg::Text` carries no flat string; the content lives on its chunks.
                    text: text.chunks().iter().map(|chunk| chunk.text()).collect(),
                    bbox: [
                        bbox.x() * scale,
                        bbox.y() * scale,
                        bbox.width() * scale,
                        bbox.height() * scale,
                    ],
                    requested: if declared.is_empty() {
                        used.clone()
                    } else {
                        declared.to_vec()
                    },
                    used,
                    fallback,
                    font_size: font_size * scale,
                });
            }
            usvg::Node::Group(inner) => {
                collect_text(inner, scale, declared, satisfied, substitute, out)
            }
            _ => {}
        }
    }
}

/// Families the SVG source declares, via `font-family="…"` attributes and `font-family:`
/// inside `style="…"`. Order preserved, duplicates dropped.
fn declared_families(svg: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for (marker, terminators) in [("font-family=\"", "\""), ("font-family:", ";\"")] {
        let mut rest = svg;
        while let Some(start) = rest.find(marker) {
            rest = &rest[start + marker.len()..];
            let end = rest
                .find(|c: char| terminators.contains(c))
                .unwrap_or(rest.len());
            for family in rest[..end].split(',') {
                let family = family.trim().trim_matches(['\'', '"']).to_string();
                if !family.is_empty()
                    && !found.iter().any(|seen| seen.eq_ignore_ascii_case(&family))
                {
                    found.push(family);
                }
            }
            rest = &rest[end..];
        }
    }
    found
}

fn family_name(family: &usvg::FontFamily) -> String {
    match family {
        usvg::FontFamily::Named(name) => name.clone(),
        usvg::FontFamily::Serif => "serif".into(),
        usvg::FontFamily::SansSerif => "sans-serif".into(),
        usvg::FontFamily::Cursive => "cursive".into(),
        usvg::FontFamily::Fantasy => "fantasy".into(),
        usvg::FontFamily::Monospace => "monospace".into(),
    }
}

fn has_family(fonts: &fontdb::Database, family: &str) -> bool {
    if GENERIC_FAMILIES.contains(&family) {
        return true;
    }
    fonts.faces().any(|face| {
        face.families
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(family))
    })
}

fn pick_family(fonts: &fontdb::Database, candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| has_family_exact(fonts, candidate))
        .map(|found| found.to_string())
}

fn has_family_exact(fonts: &fontdb::Database, family: &str) -> bool {
    fonts.faces().any(|face| {
        face.families
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(family))
    })
}

/// The face generic families resolve to. Preference order picks a widely available,
/// metrically stable sans before falling back to whatever the system has first.
fn pick_default_family(fonts: &fontdb::Database) -> String {
    pick_family(
        fonts,
        &[
            "Inter",
            "DejaVu Sans",
            "Liberation Sans",
            "Noto Sans",
            "Helvetica Neue",
            "Helvetica",
            "Arial",
        ],
    )
    .or_else(|| {
        fonts
            .faces()
            .next()
            .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
    })
    .unwrap_or_else(|| "sans-serif".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOWER_THIRD: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="640" height="360">
  <rect x="40" y="260" width="380" height="72" rx="8" fill="#101418" fill-opacity="0.85"/>
  <text x="60" y="300" font-family="sans-serif" font-size="28" fill="#ffffff">Andy Mazzola</text>
  <text x="60" y="322" font-family="sans-serif" font-size="16" fill="#9fb3c8">degen labs</text>
</svg>"##;

    #[test]
    fn a_title_rasterizes_with_content_where_the_svg_put_it() {
        let rasterizer = Rasterizer::new();
        let frame = rasterizer.rasterize(LOWER_THIRD, [640, 360]).unwrap();
        assert_eq!(frame.size(), [640, 360]);
        // The bar is in the lower third and the upper area is untouched: a title that
        // rendered at the wrong scale or offset would fail both halves of this.
        assert!(frame.pixel(200, 290)[3] > 0.5, "bar should be painted");
        assert!(frame.pixel(200, 40)[3] < 0.01, "top of frame should be clear");
        assert!(frame.alpha_coverage() > 0.05 && frame.alpha_coverage() < 0.9);
    }

    #[test]
    fn rasterizing_at_a_larger_size_scales_the_document() {
        let rasterizer = Rasterizer::new();
        let small = rasterizer.rasterize(LOWER_THIRD, [320, 180]).unwrap();
        let large = rasterizer.rasterize(LOWER_THIRD, [1280, 720]).unwrap();
        assert_eq!(small.size(), [320, 180]);
        assert_eq!(large.size(), [1280, 720]);
        // Coverage is scale-invariant for a vector document; a raster upscale would blur
        // but keep roughly the same proportion, a broken scale would not.
        let ratio = large.alpha_coverage() / small.alpha_coverage();
        assert!((ratio - 1.0).abs() < 0.2, "coverage ratio {ratio}");
    }

    #[test]
    fn text_reports_name_the_runs_and_their_boxes() {
        let rasterizer = Rasterizer::new();
        let reports = rasterizer.text_reports(LOWER_THIRD, [640, 360]).unwrap();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].text, "Andy Mazzola");
        assert!(reports[0].bbox[2] > 50.0, "text should have width: {:?}", reports[0]);
        assert!(reports[0].bbox[1] > 180.0, "text is in the lower half");
        assert_eq!(reports[0].requested, vec!["sans-serif".to_string()]);
        assert!(reports[0].fallback.is_none(), "generic families are not a fallback");
    }

    #[test]
    fn a_missing_font_is_reported_rather_than_silently_substituted() {
        let rasterizer = Rasterizer::new();
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100">
            <text x="10" y="50" font-family="NoSuchFontExists-Regular" font-size="20">hi</text></svg>"#;
        let reports = rasterizer.text_reports(svg, [200, 100]).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].fallback.as_deref(),
            Some(rasterizer.default_family()),
            "a requested family that does not exist must be reported"
        );
    }

    #[test]
    fn malformed_svg_is_an_error_naming_the_problem() {
        let rasterizer = Rasterizer::new();
        let err = rasterizer.rasterize("<svg><not-closed>", [64, 64]).unwrap_err();
        assert!(err.to_string().contains("parseable SVG"), "{err}");
    }

    #[test]
    fn antialiased_edges_are_not_darkened_by_the_color_conversion() {
        // A white shape on transparent: every partially covered edge pixel must still be
        // white once unpremultiplied. Skipping the unpremultiply step in the sRGB→linear
        // conversion is what turns antialiased white text into gray-fringed text.
        let rasterizer = Rasterizer::new();
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64">
            <circle cx="32" cy="32" r="20" fill="#ffffff"/></svg>"##;
        let frame = rasterizer.rasterize(svg, [64, 64]).unwrap();
        let mut checked = 0;
        for y in 0..64u32 {
            for x in 0..64u32 {
                let pixel = frame.pixel(x, y);
                if pixel[3] > 0.05 && pixel[3] < 0.95 {
                    let unpremul = pixel[0] / pixel[3];
                    assert!(
                        unpremul > 0.9,
                        "edge pixel at {x},{y} unpremultiplied to {unpremul}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 10, "expected antialiased edge pixels, found {checked}");
    }

    #[test]
    fn the_cache_returns_the_same_pixels_for_a_repeated_request() {
        let rasterizer = Rasterizer::new();
        let first = rasterizer.rasterize(LOWER_THIRD, [320, 180]).unwrap();
        let second = rasterizer.rasterize(LOWER_THIRD, [320, 180]).unwrap();
        assert_eq!(first.pixels(), second.pixels());
        let different = rasterizer
            .rasterize(&LOWER_THIRD.replace("Andy Mazzola", "Someone Else"), [320, 180])
            .unwrap();
        assert_ne!(first.pixels(), different.pixels(), "cache key must include content");
    }
}
