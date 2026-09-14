use anyhow::{Result, anyhow, bail};
use rustybuzz::{Face, UnicodeBuffer};
use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation;

use crate::domain::Rect;

const LONG_PROSE_GRAPHEME_THRESHOLD: usize = 512;
// A half-point range from 0.5px through 512px needs at most eleven binary
// probes. This keeps even the widest accepted range bounded while preserving
// the same half-point resolution as the short-text path.
const LONG_PROSE_MAX_SIZE_PROBES: usize = 11;
const LONG_PROSE_MAX_WRAP_PROBES: usize = 16;

#[derive(Clone, Debug)]
pub struct ShapedGlyph {
    pub glyph_id: u32,
    /// Index into the font candidates supplied to the shaper.
    pub font_index: usize,
    /// Stable font identity retained for render/report consumers.
    pub font_id: String,
    pub x: f32,
    pub y: f32,
    pub advance: f32,
    pub ink_left: f32,
    pub ink_right: f32,
    pub ink_top: f32,
    pub ink_bottom: f32,
}

#[derive(Clone, Debug)]
pub struct ShapedLine {
    pub text: String,
    pub glyphs: Vec<ShapedGlyph>,
    /// Adjacent grapheme clusters using the same face are coalesced here.
    pub font_runs: Vec<FontRun>,
    pub advance_width: f32,
    pub ink_left: f32,
    pub ink_right: f32,
    pub baseline: f32,
    pub top: f32,
    pub bottom: f32,
}

#[derive(Clone, Debug)]
pub struct FontRun {
    pub font_index: usize,
    pub font_id: String,
    pub text: String,
}

/// Font bytes and raster face borrowed by the per-grapheme shaper. The
/// `bytes` slice is used to construct a rustybuzz face on demand while the
/// fontdue face is retained for rasterization.
#[derive(Clone, Copy, Debug)]
pub struct FontCandidate<'a> {
    pub id: &'a str,
    pub bytes: &'a [u8],
    pub font: &'a fontdue::Font,
}

#[derive(Clone, Debug)]
pub struct LayoutResult {
    pub font_size: f32,
    pub lines: Vec<ShapedLine>,
    pub safe_bbox: Rect,
    pub padding: f32,
    pub placement_center: (f32, f32),
    /// The eroded connected component of the individual speech balloon, when
    /// one could be recovered from the cleaned page.  Raster composition uses
    /// this as the final containment guard as well as the layout interval.
    pub safe_mask: Option<LayoutMask>,
}

/// A compact binary mask in page coordinates.  Balloon masks are kept at the
/// small crop around one component instead of allocating a full-page image for
/// every bubble.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutMask {
    origin_x: i32,
    origin_y: i32,
    width: usize,
    height: usize,
    pixels: Vec<bool>,
    layout_erosion_radius: usize,
}

impl LayoutMask {
    pub(crate) fn from_binary(
        origin_x: i32,
        origin_y: i32,
        width: usize,
        height: usize,
        pixels: Vec<bool>,
    ) -> Result<Self> {
        if width == 0 || height == 0 || pixels.len() != width.saturating_mul(height) {
            bail!("layout mask dimensions do not match its pixel buffer");
        }
        Ok(Self {
            origin_x,
            origin_y,
            width,
            height,
            pixels,
            layout_erosion_radius: 2,
        })
    }

    pub(crate) fn from_binary_geometry(
        origin_x: i32,
        origin_y: i32,
        width: usize,
        height: usize,
        pixels: Vec<bool>,
    ) -> Result<Self> {
        let mut mask = Self::from_binary(origin_x, origin_y, width, height, pixels)?;
        // Geometry ownership already provides the requested shape boundary;
        // the regular four-pixel solver padding is its clearance. A second
        // contour erosion would erase narrow legacy overlap regions.
        mask.layout_erosion_radius = 0;
        Ok(mask)
    }

    fn layout_erosion_radius(&self) -> usize {
        self.layout_erosion_radius
    }

    pub(crate) fn eroded(&self, radius: usize) -> Option<Self> {
        if radius == 0 {
            return Some(self.clone());
        }
        let mut pixels = vec![false; self.pixels.len()];
        for y in 0..self.height {
            for x in 0..self.width {
                if !self.pixels[y * self.width + x] {
                    continue;
                }
                let left = x.checked_sub(radius);
                let right = x.checked_add(radius);
                let top = y.checked_sub(radius);
                let bottom = y.checked_add(radius);
                let Some(left) = left else { continue };
                let Some(right) = right.filter(|right| *right < self.width) else {
                    continue;
                };
                let Some(top) = top else { continue };
                let Some(bottom) = bottom.filter(|bottom| *bottom < self.height) else {
                    continue;
                };
                let mut keep = true;
                'neighborhood: for yy in top..=bottom {
                    for xx in left..=right {
                        if !self.pixels[yy * self.width + xx] {
                            keep = false;
                            break 'neighborhood;
                        }
                    }
                }
                pixels[y * self.width + x] = keep;
            }
        }
        let result = Self {
            origin_x: self.origin_x,
            origin_y: self.origin_y,
            width: self.width,
            height: self.height,
            pixels,
            layout_erosion_radius: self.layout_erosion_radius,
        };
        result.bounds().map(|_| result)
    }

    pub(crate) fn bounds(&self) -> Option<Rect> {
        let mut min_x = self.width;
        let mut min_y = self.height;
        let mut max_x = 0usize;
        let mut max_y = 0usize;
        for y in 0..self.height {
            for x in 0..self.width {
                if self.pixels[y * self.width + x] {
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x + 1);
                    max_y = max_y.max(y + 1);
                }
            }
        }
        (min_x < max_x && min_y < max_y).then_some(Rect {
            x1: self.origin_x as f32 + min_x as f32,
            y1: self.origin_y as f32 + min_y as f32,
            x2: self.origin_x as f32 + max_x as f32,
            y2: self.origin_y as f32 + max_y as f32,
        })
    }

    pub(crate) fn contains_pixel(&self, x: i32, y: i32) -> bool {
        let Some(local_x) = x.checked_sub(self.origin_x) else {
            return false;
        };
        let Some(local_y) = y.checked_sub(self.origin_y) else {
            return false;
        };
        let (Ok(local_x), Ok(local_y)) = (usize::try_from(local_x), usize::try_from(local_y))
        else {
            return false;
        };
        local_x < self.width && local_y < self.height && self.pixels[local_y * self.width + local_x]
    }

    /// Return the largest horizontal interval that is inside the mask for
    /// every raster row touched by a glyph band.  Prefer the interval around
    /// the layout centre so disconnected artwork in a broad detector box can
    /// never become a second writing area.
    fn interval_for_band(&self, top: f32, bottom: f32, preferred_x: f32) -> Option<(f32, f32)> {
        if !top.is_finite() || !bottom.is_finite() || bottom <= top {
            return None;
        }
        let first = (top.floor() as i32).max(self.origin_y);
        let last = (bottom.ceil() as i32).min(self.origin_y + self.height as i32);
        if last <= first {
            return None;
        }
        let mut intersection: Option<Vec<(i32, i32)>> = None;
        for y in first..last {
            let local_y = usize::try_from(y - self.origin_y).ok()?;
            let row = &self.pixels[local_y * self.width..(local_y + 1) * self.width];
            let mut runs = Vec::new();
            let mut start = None;
            for (x, inside) in row.iter().copied().enumerate() {
                if inside && start.is_none() {
                    start = Some(x as i32 + self.origin_x);
                } else if !inside && let Some(start) = start.take() {
                    runs.push((start, x as i32 + self.origin_x));
                }
            }
            if let Some(start) = start {
                runs.push((start, self.origin_x + self.width as i32));
            }
            if runs.is_empty() {
                return None;
            }
            intersection = Some(match intersection {
                None => runs,
                Some(previous) => previous
                    .into_iter()
                    .flat_map(|(left, right)| {
                        runs.iter().filter_map(move |(next_left, next_right)| {
                            let left = left.max(*next_left);
                            let right = right.min(*next_right);
                            (left < right).then_some((left, right))
                        })
                    })
                    .collect(),
            });
            if intersection.as_ref().is_some_and(Vec::is_empty) {
                return None;
            }
        }
        let intervals = intersection?;
        intervals
            .iter()
            .copied()
            .find(|(left, right)| preferred_x >= *left as f32 && preferred_x <= *right as f32)
            .or_else(|| {
                intervals
                    .into_iter()
                    .max_by_key(|(left, right)| right.saturating_sub(*left))
            })
            .map(|(left, right)| (left as f32, right as f32))
    }
}

#[derive(Clone, Copy)]
struct Metrics {
    ascent: f32,
    descent: f32,
    leading: f32,
}

type SolveResult = Option<(f32, Vec<(usize, ShapedLine)>)>;

struct LongProseCache {
    tokens: Vec<String>,
    legal_breaks: Vec<bool>,
    advances: Vec<f32>,
}

struct Shaper<'a> {
    face: &'a Face<'a>,
    font: &'a fontdue::Font,
    size: f32,
    scale: f32,
    metrics: Metrics,
}

trait TextShaper {
    fn metrics_for_text(&self, text: &str) -> Metrics;
    fn shape(&self, text: &str) -> Result<ShapedLine>;
}

impl<'a> Shaper<'a> {
    fn new(face: &'a Face<'a>, font: &'a fontdue::Font, size: f32) -> Result<Self> {
        if !size.is_finite() || size <= 0.0 {
            bail!("font size must be finite and positive");
        }
        let upem = face.units_per_em() as f32;
        if upem <= 0.0 {
            bail!("font has invalid units_per_em");
        }
        let scale = size / upem;
        let ascent = f32::from(face.ascender()) * scale;
        let descent = (-f32::from(face.descender()) * scale).max(0.0);
        let leading = (f32::from(face.line_gap()) * scale).max(0.0);
        Ok(Self {
            face,
            font,
            size,
            scale,
            metrics: Metrics {
                ascent,
                descent,
                leading,
            },
        })
    }

    fn shape(&self, text: &str) -> Result<ShapedLine> {
        let mut missing = Vec::new();
        for (byte, ch) in text.char_indices() {
            if self.face.glyph_index(ch).is_none_or(|gid| gid.0 == 0) {
                missing.push((byte, ch));
            }
        }
        if !missing.is_empty() {
            let chars = missing
                .iter()
                .map(|(_, c)| format!("U+{:04X}", u32::from(*c)))
                .collect::<Vec<_>>()
                .join(", ");
            bail!("font is missing glyphs: {chars}");
        }
        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(text);
        let glyph_buffer = rustybuzz::shape(self.face, &[], buffer);
        let infos = glyph_buffer.glyph_infos();
        let positions = glyph_buffer.glyph_positions();
        let mut x = 0.0;
        let mut glyphs = Vec::with_capacity(infos.len());
        let mut left = f32::INFINITY;
        let mut right = f32::NEG_INFINITY;
        let mut top = f32::INFINITY;
        let mut bottom = f32::NEG_INFINITY;
        for (info, pos) in infos.iter().zip(positions.iter()) {
            let gid = info.glyph_id;
            let gx = x + pos.x_offset as f32 * self.scale;
            let gy = pos.y_offset as f32 * self.scale;
            let (ink_left, ink_right, ink_top, ink_bottom) = if let Some(ext) =
                self.face.glyph_bounding_box(rustybuzz::ttf_parser::GlyphId(
                    u16::try_from(gid).map_err(|_| anyhow!("glyph id exceeds font range"))?,
                )) {
                (
                    gx + f32::from(ext.x_min) * self.scale,
                    gx + f32::from(ext.x_max) * self.scale,
                    gy - f32::from(ext.y_max) * self.scale,
                    gy - f32::from(ext.y_min) * self.scale,
                )
            } else {
                let raster_gid = u16::try_from(gid)
                    .map_err(|_| anyhow!("glyph id exceeds raster font range"))?;
                let (m, _) = self.font.rasterize_indexed(raster_gid, self.size);
                (
                    gx + m.xmin as f32,
                    gx + m.xmin as f32 + m.width as f32,
                    -m.ymin as f32 - m.height as f32 + gy,
                    -m.ymin as f32 + gy,
                )
            };
            left = left.min(ink_left);
            right = right.max(ink_right);
            top = top.min(ink_top);
            bottom = bottom.max(ink_bottom);
            glyphs.push(ShapedGlyph {
                glyph_id: gid,
                font_index: 0,
                font_id: "primary".to_owned(),
                x: gx,
                y: gy,
                advance: pos.x_advance as f32 * self.scale,
                ink_left,
                ink_right,
                ink_top,
                ink_bottom,
            });
            x += pos.x_advance as f32 * self.scale;
        }
        if glyphs.is_empty() {
            left = 0.0;
            right = 0.0;
            top = -self.metrics.ascent;
            bottom = self.metrics.descent;
        }
        Ok(ShapedLine {
            text: text.to_owned(),
            glyphs,
            font_runs: if text.is_empty() {
                Vec::new()
            } else {
                vec![FontRun {
                    font_index: 0,
                    font_id: "primary".to_owned(),
                    text: text.to_owned(),
                }]
            },
            advance_width: x,
            ink_left: left,
            ink_right: right,
            baseline: 0.0,
            top,
            bottom,
        })
    }
}

impl TextShaper for Shaper<'_> {
    fn metrics_for_text(&self, _text: &str) -> Metrics {
        self.metrics
    }

    fn shape(&self, text: &str) -> Result<ShapedLine> {
        Shaper::shape(self, text)
    }
}

struct MultiShaper<'a> {
    fonts: &'a [FontCandidate<'a>],
    size: f32,
    metrics: Metrics,
}

impl<'a> MultiShaper<'a> {
    fn new(
        fonts: &'a [FontCandidate<'a>],
        size: f32,
        active_font_indices: &[usize],
    ) -> Result<Self> {
        if fonts.is_empty() {
            bail!("at least one font candidate is required");
        }
        if !size.is_finite() || size <= 0.0 {
            bail!("font size must be finite and positive");
        }
        // Validate every candidate up front. A later text edit may exercise a
        // fallback that the original render did not use, so the full ordered
        // candidate list must remain usable across rerenders.
        for candidate in fonts {
            let face = Face::from_slice(candidate.bytes, 0)
                .ok_or_else(|| anyhow!("unable to parse font {}", candidate.id))?;
            if face.units_per_em() == 0 {
                bail!("font {} has invalid units_per_em", candidate.id);
            }
        }
        let active_font_indices = if active_font_indices.is_empty() {
            &[0][..]
        } else {
            active_font_indices
        };
        let mut metrics = Metrics {
            ascent: 0.0,
            descent: 0.0,
            leading: 0.0,
        };
        for &index in active_font_indices {
            let candidate = fonts
                .get(index)
                .ok_or_else(|| anyhow!("font candidate index {index} is out of range"))?;
            let face = Face::from_slice(candidate.bytes, 0)
                .ok_or_else(|| anyhow!("unable to parse font {}", candidate.id))?;
            let candidate_upem = face.units_per_em() as f32;
            if candidate_upem <= 0.0 {
                bail!("font {} has invalid units_per_em", candidate.id);
            }
            let candidate_scale = size / candidate_upem;
            metrics.ascent = metrics
                .ascent
                .max(f32::from(face.ascender()) * candidate_scale);
            metrics.descent = metrics
                .descent
                .max((-f32::from(face.descender()) * candidate_scale).max(0.0));
            metrics.leading = metrics
                .leading
                .max((f32::from(face.line_gap()) * candidate_scale).max(0.0));
        }
        Ok(Self {
            fonts,
            size,
            metrics,
        })
    }

    fn shape_run(
        &self,
        font_index: usize,
        text: &str,
        x_offset: f32,
    ) -> Result<(Vec<ShapedGlyph>, f32, f32, f32, f32, f32)> {
        let candidate = self
            .fonts
            .get(font_index)
            .ok_or_else(|| anyhow!("font candidate index {font_index} is out of range"))?;
        let face = Face::from_slice(candidate.bytes, 0)
            .ok_or_else(|| anyhow!("unable to parse font {}", candidate.id))?;
        let upem = face.units_per_em() as f32;
        let scale = self.size / upem;
        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(text);
        let glyph_buffer = rustybuzz::shape(&face, &[], buffer);
        let infos = glyph_buffer.glyph_infos();
        let positions = glyph_buffer.glyph_positions();
        let mut x = 0.0;
        let mut glyphs = Vec::with_capacity(infos.len());
        let mut left = f32::INFINITY;
        let mut right = f32::NEG_INFINITY;
        let mut top = f32::INFINITY;
        let mut bottom = f32::NEG_INFINITY;
        for (info, pos) in infos.iter().zip(positions.iter()) {
            let gid = info.glyph_id;
            let gx = x_offset + x + pos.x_offset as f32 * scale;
            let gy = pos.y_offset as f32 * scale;
            let (ink_left, ink_right, ink_top, ink_bottom) = if let Some(ext) = face
                .glyph_bounding_box(rustybuzz::ttf_parser::GlyphId(
                    u16::try_from(gid).map_err(|_| anyhow!("glyph id exceeds font range"))?,
                )) {
                (
                    gx + f32::from(ext.x_min) * scale,
                    gx + f32::from(ext.x_max) * scale,
                    gy - f32::from(ext.y_max) * scale,
                    gy - f32::from(ext.y_min) * scale,
                )
            } else {
                let raster_gid = u16::try_from(gid)
                    .map_err(|_| anyhow!("glyph id exceeds raster font range"))?;
                let (metrics, _) = candidate.font.rasterize_indexed(raster_gid, self.size);
                (
                    gx + metrics.xmin as f32,
                    gx + metrics.xmin as f32 + metrics.width as f32,
                    -metrics.ymin as f32 - metrics.height as f32 + gy,
                    -metrics.ymin as f32 + gy,
                )
            };
            left = left.min(ink_left);
            right = right.max(ink_right);
            top = top.min(ink_top);
            bottom = bottom.max(ink_bottom);
            glyphs.push(ShapedGlyph {
                glyph_id: gid,
                font_index,
                font_id: candidate.id.to_owned(),
                x: gx,
                y: gy,
                advance: pos.x_advance as f32 * scale,
                ink_left,
                ink_right,
                ink_top,
                ink_bottom,
            });
            x += pos.x_advance as f32 * scale;
        }
        if glyphs.is_empty() {
            left = 0.0;
            right = 0.0;
            top = -self.metrics.ascent;
            bottom = self.metrics.descent;
        }
        Ok((glyphs, x, left, right, top, bottom))
    }
    fn choose_base_font_for_text(&self, text: &str) -> usize {
        let alphabetic_chars = text
            .chars()
            .filter(|c| requires_font_glyph(*c) && c.is_alphabetic())
            .collect::<Vec<_>>();
        if alphabetic_chars.is_empty() {
            return 0;
        }
        for (index, candidate) in self.fonts.iter().enumerate() {
            if let Some(face) = Face::from_slice(candidate.bytes, 0) {
                let all_covered = alphabetic_chars
                    .iter()
                    .all(|c| face.glyph_index(*c).is_some_and(|glyph| glyph.0 != 0));
                if all_covered {
                    return index;
                }
            }
        }
        0
    }

    fn font_covers_cluster(&self, font_index: usize, cluster: &str) -> bool {
        let Some(candidate) = self.fonts.get(font_index) else {
            return false;
        };
        let Some(face) = Face::from_slice(candidate.bytes, 0) else {
            return false;
        };
        cluster
            .chars()
            .filter(|c| requires_font_glyph(*c))
            .all(|c| face.glyph_index(c).is_some_and(|glyph| glyph.0 != 0))
    }
}

impl TextShaper for MultiShaper<'_> {
    fn metrics_for_text(&self, _text: &str) -> Metrics {
        self.metrics
    }

    fn shape(&self, text: &str) -> Result<ShapedLine> {
        if text.is_empty() {
            return Ok(ShapedLine {
                text: String::new(),
                glyphs: Vec::new(),
                font_runs: Vec::new(),
                advance_width: 0.0,
                ink_left: 0.0,
                ink_right: 0.0,
                baseline: 0.0,
                top: -self.metrics.ascent,
                bottom: self.metrics.descent,
            });
        }
        let base_font_index = self.choose_base_font_for_text(text);
        let mut runs = Vec::<(usize, String)>::new();
        for cluster in UnicodeSegmentation::graphemes(text, true) {
            let index = if self.font_covers_cluster(base_font_index, cluster) {
                base_font_index
            } else {
                choose_font_index(self.fonts, cluster)?
            };
            if let Some((last_index, last_text)) = runs.last_mut()
                && *last_index == index
            {
                last_text.push_str(cluster);
            } else {
                runs.push((index, cluster.to_owned()));
            }
        }
        let mut glyphs = Vec::new();
        let mut font_runs = Vec::with_capacity(runs.len());
        let mut advance_width = 0.0;
        let mut left = f32::INFINITY;
        let mut right = f32::NEG_INFINITY;
        let mut top = f32::INFINITY;
        let mut bottom = f32::NEG_INFINITY;
        for (font_index, run_text) in runs {
            let (mut run_glyphs, run_advance, run_left, run_right, run_top, run_bottom) =
                self.shape_run(font_index, &run_text, advance_width)?;
            glyphs.append(&mut run_glyphs);
            let font_id = self.fonts[font_index].id.to_owned();
            font_runs.push(FontRun {
                font_index,
                font_id,
                text: run_text,
            });
            advance_width += run_advance;
            left = left.min(run_left);
            right = right.max(run_right);
            top = top.min(run_top);
            bottom = bottom.max(run_bottom);
        }
        Ok(ShapedLine {
            text: text.to_owned(),
            glyphs,
            font_runs,
            advance_width,
            ink_left: left,
            ink_right: right,
            baseline: 0.0,
            top,
            bottom,
        })
    }
}

fn active_font_indices(fonts: &[FontCandidate<'_>], text: &str) -> Result<Vec<usize>> {
    let mut active = Vec::new();
    for cluster in UnicodeSegmentation::graphemes(text, true) {
        let index = choose_font_index(fonts, cluster)?;
        if !active.contains(&index) {
            active.push(index);
        }
    }
    if active.is_empty() {
        active.push(0);
    }
    Ok(active)
}

fn choose_font_index(fonts: &[FontCandidate<'_>], cluster: &str) -> Result<usize> {
    let required = cluster
        .chars()
        .filter(|character| requires_font_glyph(*character))
        .collect::<Vec<_>>();
    if required.is_empty() {
        return Ok(0);
    }
    let mut missing_by_font = Vec::new();
    for (index, candidate) in fonts.iter().enumerate() {
        let face = Face::from_slice(candidate.bytes, 0)
            .ok_or_else(|| anyhow!("unable to parse font {}", candidate.id))?;
        let missing = required
            .iter()
            .copied()
            .filter(|character| {
                face.glyph_index(*character)
                    .is_none_or(|glyph| glyph.0 == 0)
            })
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(index);
        }
        missing_by_font.push((candidate.id, missing));
    }
    let details = missing_by_font
        .iter()
        .map(|(font, missing)| {
            format!(
                "{font}: {}",
                missing
                    .iter()
                    .map(|character| format!("U+{:04X}", u32::from(*character)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    bail!(
        "grapheme cluster {cluster:?} has no single font covering it; missing code points by candidate: {details}"
    )
}

fn requires_font_glyph(character: char) -> bool {
    !character.is_control()
        && !matches!(
            character,
            '\u{200C}' | '\u{200D}' | '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}'
        )
}

/// Fit a complete string in a rectangle or ellipse using actual glyph shaping.
/// Candidate sizes are checked on a finite descending grid; no truncation occurs.
pub fn fit_text(
    face: &Face<'_>,
    font: &fontdue::Font,
    text: &str,
    bbox: Rect,
    shape: &str,
    min_font_size: f32,
    max_font_size: f32,
) -> Result<LayoutResult> {
    fit_text_with_geometry(
        face,
        font,
        text,
        bbox,
        None,
        None,
        shape,
        min_font_size,
        max_font_size,
        None,
    )
}

/// Fit text inside detector geometry while keeping a visible inset from the
/// bubble/panel boundary. `bubble_bbox` is the preferred containing geometry;
/// `text_bbox` is only an optional placement anchor for vision clients.
#[allow(clippy::too_many_arguments)]
pub fn fit_text_with_geometry(
    face: &Face<'_>,
    font: &fontdue::Font,
    text: &str,
    bbox: Rect,
    bubble_bbox: Option<Rect>,
    text_bbox: Option<Rect>,
    shape: &str,
    min_font_size: f32,
    max_font_size: f32,
    padding: Option<f32>,
) -> Result<LayoutResult> {
    fit_text_with_factory(
        bbox,
        bubble_bbox,
        text_bbox,
        shape,
        min_font_size,
        max_font_size,
        padding,
        None,
        text,
        |size| Shaper::new(face, font, size),
    )
}

/// Fit text with a primary font followed by per-grapheme fallback candidates.
/// Each Unicode grapheme is assigned to one candidate, so combining marks and
/// variation selectors never get split across faces. Adjacent assignments are
/// shaped as one run for stable advances and ligatures.
#[allow(clippy::too_many_arguments)]
pub fn fit_text_with_font_candidates(
    fonts: &[FontCandidate<'_>],
    text: &str,
    bbox: Rect,
    bubble_bbox: Option<Rect>,
    text_bbox: Option<Rect>,
    shape: &str,
    min_font_size: f32,
    max_font_size: f32,
    padding: Option<f32>,
) -> Result<LayoutResult> {
    if fonts.is_empty() {
        bail!("at least one font candidate is required");
    }
    // Font coverage is independent of point size. Resolve the complete text
    // once so each size candidate computes line metrics from the faces that
    // actually participate in this text, rather than every available
    // fallback face.
    let active_font_indices = active_font_indices(fonts, text)?;
    fit_text_with_factory(
        bbox,
        bubble_bbox,
        text_bbox,
        shape,
        min_font_size,
        max_font_size,
        padding,
        None,
        text,
        |size| MultiShaper::new(fonts, size, &active_font_indices),
    )
}

/// Fit text while using a connected component mask for the individual
/// balloon.  The mask is eroded before it reaches the solver, leaving a
/// measured border around the contour even when detector rectangles overlap.
#[allow(clippy::too_many_arguments)]
pub fn fit_text_with_font_candidates_masked(
    fonts: &[FontCandidate<'_>],
    text: &str,
    bbox: Rect,
    bubble_bbox: Option<Rect>,
    text_bbox: Option<Rect>,
    shape: &str,
    min_font_size: f32,
    max_font_size: f32,
    padding: Option<f32>,
    mask: Option<LayoutMask>,
) -> Result<LayoutResult> {
    if fonts.is_empty() {
        bail!("at least one font candidate is required");
    }
    let active_font_indices = active_font_indices(fonts, text)?;
    fit_text_with_factory(
        bbox,
        bubble_bbox,
        text_bbox,
        shape,
        min_font_size,
        max_font_size,
        padding,
        mask,
        text,
        |size| MultiShaper::new(fonts, size, &active_font_indices),
    )
}

#[allow(clippy::too_many_arguments)]
fn fit_text_with_factory<F, S>(
    bbox: Rect,
    bubble_bbox: Option<Rect>,
    text_bbox: Option<Rect>,
    shape: &str,
    min_font_size: f32,
    max_font_size: f32,
    padding: Option<f32>,
    mask: Option<LayoutMask>,
    text: &str,
    factory: F,
) -> Result<LayoutResult>
where
    F: Fn(f32) -> Result<S>,
    S: TextShaper,
{
    if !bbox.x1.is_finite()
        || !bbox.y1.is_finite()
        || !bbox.x2.is_finite()
        || !bbox.y2.is_finite()
        || bbox.x2 <= bbox.x1
        || bbox.y2 <= bbox.y1
    {
        bail!("invalid typeset rectangle");
    }
    if !min_font_size.is_finite()
        || !max_font_size.is_finite()
        || min_font_size < 0.5
        || max_font_size < min_font_size
        || max_font_size > 512.0
    {
        bail!("font size range is invalid");
    }
    if shape != "ellipse" && shape != "rectangle" {
        bail!("unsupported text shape {shape:?}");
    }
    if text.graphemes(true).count() > 4_096 {
        bail!("text exceeds the 4,096 grapheme limit");
    }
    // A mask is authoritative for the individual balloon, while the
    // detector geometry still bounds any accidental component recovery.
    // Pixel-inferred masks use a 5x5 Chebyshev erosion (radius 2), matching
    // the visible contour clearance. Geometry-only overlap masks already have
    // a shape boundary and use their four-pixel solver padding as clearance.
    let mask = match mask {
        Some(mask) => Some(
            mask.eroded(mask.layout_erosion_radius())
                .ok_or_else(|| anyhow!("balloon mask has no safe interior after erosion"))?,
        ),
        None => None,
    };
    let detector_area = bubble_bbox.filter(|rect| valid_rect(*rect)).unwrap_or(bbox);
    let area = mask
        .as_ref()
        .and_then(|mask| {
            mask.bounds()
                .and_then(|bounds| rect_intersection(bounds, detector_area))
        })
        .unwrap_or(detector_area);
    let padding = padding
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or_else(|| (area.x2 - area.x1).min(area.y2 - area.y1) * 0.06);
    let padding = padding.clamp(4.0, 16.0);
    let safe_bbox = inset_rect(area, padding)?;
    let anchor = text_bbox.filter(|rect| valid_rect(*rect)).unwrap_or(area);
    let mask_center = mask
        .as_ref()
        .and_then(LayoutMask::bounds)
        .map(|bounds| ((bounds.x1 + bounds.x2) * 0.5, (bounds.y1 + bounds.y2) * 0.5));
    let placement_center = (
        mask_center
            .map(|center| center.0)
            .unwrap_or((anchor.x1 + anchor.x2) * 0.5)
            .clamp(safe_bbox.x1, safe_bbox.x2),
        mask_center
            .map(|center| center.1)
            .unwrap_or((anchor.y1 + anchor.y2) * 0.5)
            .clamp(safe_bbox.y1, safe_bbox.y2),
    );
    let mut size = (max_font_size * 2.0).floor() / 2.0;
    let floor = (min_font_size * 2.0).ceil() / 2.0;
    if size < floor {
        size = max_font_size;
    }
    let long_prose = text.graphemes(true).count() > LONG_PROSE_GRAPHEME_THRESHOLD;
    if long_prose {
        // The old half-point descent is useful for short dialogue because it
        // preserves its exact best-fit choice. Long prose gets a bounded
        // monotonic search: each probe uses the linear greedy wrapper below,
        // then the final successful size is shaped once per output line.
        let min_units = (floor * 2.0).ceil() as i32;
        let max_units = (size * 2.0).floor() as i32;
        let base_units = min_units + (max_units - min_units) / 2;
        let base_size = base_units as f32 / 2.0;
        let base_shaper = factory(base_size)?;
        let cache = long_prose_cache(&base_shaper, text)?;
        let mut low = min_units;
        let mut high = max_units;
        let mut best = None;
        let mut probes = 0;
        while low <= high && probes < LONG_PROSE_MAX_SIZE_PROBES {
            let units = low + (high - low) / 2;
            let candidate_size = units as f32 / 2.0;
            let shaper = factory(candidate_size)?;
            match fit_long_prose_at_size(
                &shaper,
                text,
                safe_bbox,
                shape,
                placement_center,
                mask.as_ref(),
                &cache,
                candidate_size / base_size,
            )? {
                Some(lines) => {
                    best = Some((candidate_size, lines));
                    low = units + 1;
                }
                None => high = units - 1,
            }
            probes += 1;
        }
        if let Some((font_size, lines)) = best {
            return Ok(LayoutResult {
                font_size,
                lines,
                safe_bbox,
                padding,
                placement_center,
                safe_mask: mask,
            });
        }
        return Err(crate::error::FukidashiError::TextOverflow(format!(
            "supplied text does not fit at minimum font size {min_font_size:.1}px"
        ))
        .into());
    }
    while size + 1e-4 >= floor {
        let shaper = factory(size)?;
        if let Some(lines) = fit_at_size(
            &shaper,
            text,
            safe_bbox,
            shape,
            placement_center,
            mask.as_ref(),
        )? {
            return Ok(LayoutResult {
                font_size: size,
                lines,
                safe_bbox,
                padding,
                placement_center,
                safe_mask: mask,
            });
        }
        size -= 0.5;
    }
    Err(crate::error::FukidashiError::TextOverflow(format!(
        "supplied text does not fit at minimum font size {min_font_size:.1}px"
    ))
    .into())
}

fn rect_intersection(left: Rect, right: Rect) -> Option<Rect> {
    let result = Rect {
        x1: left.x1.max(right.x1),
        y1: left.y1.max(right.y1),
        x2: left.x2.min(right.x2),
        y2: left.y2.min(right.y2),
    };
    valid_rect(result).then_some(result)
}

fn valid_rect(rect: Rect) -> bool {
    rect.x1.is_finite()
        && rect.y1.is_finite()
        && rect.x2.is_finite()
        && rect.y2.is_finite()
        && rect.x2 > rect.x1
        && rect.y2 > rect.y1
}

fn inset_rect(rect: Rect, padding: f32) -> Result<Rect> {
    let safe = Rect {
        x1: rect.x1 + padding,
        y1: rect.y1 + padding,
        x2: rect.x2 - padding,
        y2: rect.y2 - padding,
    };
    if safe.x2 <= safe.x1 || safe.y2 <= safe.y1 {
        bail!("typeset rectangle is too small for the required edge padding");
    }
    Ok(safe)
}

fn fit_at_size(
    shaper: &impl TextShaper,
    text: &str,
    bbox: Rect,
    shape: &str,
    placement_center: (f32, f32),
    mask: Option<&LayoutMask>,
) -> Result<Option<Vec<ShapedLine>>> {
    if text.graphemes(true).count() > LONG_PROSE_GRAPHEME_THRESHOLD {
        let cache = long_prose_cache(shaper, text)?;
        return fit_long_prose_at_size(
            shaper,
            text,
            bbox,
            shape,
            placement_center,
            mask,
            &cache,
            1.0,
        );
    }
    let metrics = shaper.metrics_for_text(text);
    let tokens = UnicodeSegmentation::graphemes(text, true)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut offsets = Vec::with_capacity(tokens.len() + 1);
    offsets.push(0usize);
    let mut offset = 0usize;
    for token in &tokens {
        offset += token.len();
        offsets.push(offset);
    }
    let mut legal = vec![false; tokens.len() + 1];
    legal[0] = true;
    legal[tokens.len()] = true;
    for (byte, _) in unicode_linebreak::linebreaks(text) {
        if let Some(index) = offsets.iter().position(|candidate| *candidate == byte) {
            legal[index] = true;
        }
    }
    let n = tokens.len();
    let (cx, cy) = placement_center;
    let (a, b) = (
        (bbox.x2 - bbox.x1) * 0.5 - 1.0,
        (bbox.y2 - bbox.y1) * 0.5 - 1.0,
    );
    if shape == "ellipse" && (a <= 0.0 || b <= 0.0) {
        bail!("ellipse radii are non-positive after inset");
    }
    let min_lines = 1;
    let max_lines = n.saturating_add(1).clamp(1, 256);
    for line_count in min_lines..=max_lines {
        let block_height = line_count as f32 * (metrics.ascent + metrics.descent)
            + (line_count.saturating_sub(1) as f32) * metrics.leading;
        if block_height > 2.0 * b + 1e-3 {
            continue;
        }
        let first_baseline = cy - block_height * 0.5 + metrics.ascent;
        let mut memo = HashMap::<(usize, usize), SolveResult>::new();
        if let Some((_, mut chosen)) = solve_lines(
            shaper,
            &tokens,
            &legal,
            0,
            0,
            line_count,
            first_baseline,
            metrics.ascent,
            metrics.descent,
            metrics.leading,
            bbox,
            cx,
            cy,
            a,
            b,
            shape,
            mask,
            &mut memo,
        )? {
            chosen.sort_by_key(|(line, _)| *line);
            let mut lines = chosen.into_iter().map(|(_, line)| line).collect::<Vec<_>>();
            for (i, line) in lines.iter_mut().enumerate() {
                let baseline = first_baseline
                    + i as f32 * (metrics.ascent + metrics.descent + metrics.leading);
                line.baseline = baseline;
                line.top += baseline;
                line.bottom += baseline;
            }
            return Ok(Some(lines));
        }
    }
    Ok(None)
}

fn long_prose_cache(shaper: &impl TextShaper, text: &str) -> Result<LongProseCache> {
    let (tokens, legal_breaks) = long_prose_break_data(text);
    let advances = tokens
        .iter()
        .map(|token| {
            if token == "\n" {
                Ok(0.0)
            } else {
                Ok(shaper.shape(token)?.advance_width.max(0.0))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(LongProseCache {
        tokens,
        legal_breaks,
        advances,
    })
}

fn long_prose_break_data(text: &str) -> (Vec<String>, Vec<bool>) {
    let tokens = UnicodeSegmentation::graphemes(text, true)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut offsets = Vec::with_capacity(tokens.len() + 1);
    offsets.push(0usize);
    let mut offset = 0usize;
    for token in &tokens {
        offset += token.len();
        offsets.push(offset);
    }
    let offset_to_index = offsets
        .iter()
        .enumerate()
        .map(|(index, offset)| (*offset, index))
        .collect::<HashMap<_, _>>();
    let mut legal = vec![false; tokens.len() + 1];
    legal[0] = true;
    legal[tokens.len()] = true;
    for (byte, _) in unicode_linebreak::linebreaks(text) {
        if let Some(&index) = offset_to_index.get(&byte) {
            legal[index] = true;
        }
    }
    (tokens, legal)
}

#[allow(clippy::too_many_arguments)]
fn fit_long_prose_at_size(
    shaper: &impl TextShaper,
    text: &str,
    bbox: Rect,
    shape: &str,
    placement_center: (f32, f32),
    mask: Option<&LayoutMask>,
    cache: &LongProseCache,
    advance_scale: f32,
) -> Result<Option<Vec<ShapedLine>>> {
    let metrics = shaper.metrics_for_text(text);
    let line_height = metrics.ascent + metrics.descent + metrics.leading;
    if !line_height.is_finite() || line_height <= 0.0 {
        bail!("text metrics have a non-positive line height");
    }
    // Shaping advances scale linearly with point size for a fixed font. The
    // cache is measured once per fit, so every binary-search probe only scans
    // the grapheme array and shapes its final candidate lines.
    let advances = cache
        .advances
        .iter()
        .map(|advance| advance * advance_scale)
        .collect::<Vec<_>>();
    let tokens = &cache.tokens;
    let legal = &cache.legal_breaks;
    let (cx, cy) = placement_center;
    let (a, b) = (
        (bbox.x2 - bbox.x1) * 0.5 - 1.0,
        (bbox.y2 - bbox.y1) * 0.5 - 1.0,
    );
    if shape == "ellipse" && (a <= 0.0 || b <= 0.0) {
        bail!("ellipse radii are non-positive after inset");
    }
    let minimum_lines = tokens.iter().filter(|token| token.as_str() == "\n").count() + 1;
    let maximum_lines = tokens.len().saturating_add(1).min(256);
    let maximum_by_height = ((2.0 * b + metrics.leading) / line_height).floor().max(0.0) as usize;
    let maximum_lines = maximum_lines.min(maximum_by_height);
    if maximum_lines < minimum_lines {
        return Ok(None);
    }
    let center_width = available_width(
        bbox,
        cx,
        cy,
        a,
        b,
        cy - metrics.ascent - 1.0,
        cy + metrics.descent + 1.0,
        shape,
        mask,
    )?;
    let total_advance = advances.iter().sum::<f32>();
    let width_hint = center_width.max(1.0) * 0.85;
    let estimated_lines = (total_advance / width_hint).ceil() as usize;
    let mut target_lines = estimated_lines.clamp(minimum_lines, maximum_lines);
    for _ in 0..LONG_PROSE_MAX_WRAP_PROBES {
        let Some(ranges) = greedy_line_ranges(
            &tokens,
            &legal,
            &advances,
            target_lines,
            metrics,
            bbox,
            cx,
            cy,
            a,
            b,
            shape,
            mask,
        )?
        else {
            return Ok(None);
        };
        let actual_lines = ranges.len();
        if actual_lines == target_lines {
            return shape_long_prose_lines(
                shaper,
                &tokens,
                ranges,
                target_lines,
                metrics,
                bbox,
                cx,
                cy,
                a,
                b,
                shape,
                mask,
            );
        }
        if actual_lines > maximum_lines {
            return Ok(None);
        }
        let next_target = actual_lines.clamp(minimum_lines, maximum_lines);
        if next_target == target_lines {
            return Ok(None);
        }
        target_lines = next_target;
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn greedy_line_ranges(
    tokens: &[String],
    legal: &[bool],
    advances: &[f32],
    target_lines: usize,
    metrics: Metrics,
    bbox: Rect,
    cx: f32,
    cy: f32,
    a: f32,
    b: f32,
    shape: &str,
    mask: Option<&LayoutMask>,
) -> Result<Option<Vec<(usize, usize)>>> {
    let block_height = target_lines as f32 * (metrics.ascent + metrics.descent)
        + target_lines.saturating_sub(1) as f32 * metrics.leading;
    let first_baseline = cy - block_height * 0.5 + metrics.ascent;
    let mut ranges = Vec::with_capacity(target_lines);
    let mut start = 0usize;
    while start < tokens.len() {
        let line = ranges.len();
        if tokens[start] == "\n" {
            ranges.push((start, start));
            start += 1;
            continue;
        }
        let baseline =
            first_baseline + line as f32 * (metrics.ascent + metrics.descent + metrics.leading);
        let available = available_width(
            bbox,
            cx,
            cy,
            a,
            b,
            baseline - metrics.ascent - 1.0,
            baseline + metrics.descent + 1.0,
            shape,
            mask,
        )?;
        let last_end = tokens
            .iter()
            .skip(start)
            .position(|token| token == "\n")
            .map_or(tokens.len(), |index| start + index);
        let mut width = 0.0;
        let mut chosen_end = None;
        for end in (start + 1)..=last_end {
            width += advances[end - 1];
            if !legal[end] && end != last_end {
                continue;
            }
            if width + 2.0 <= available + 1e-3 {
                chosen_end = Some(end);
            } else if chosen_end.is_some() {
                break;
            }
        }
        let Some(end) = chosen_end else {
            return Ok(None);
        };
        ranges.push((start, end));
        start = if end < tokens.len() && tokens[end] == "\n" {
            end + 1
        } else {
            end
        };
    }
    if tokens.last().is_some_and(|token| token == "\n") {
        ranges.push((tokens.len(), tokens.len()));
    }
    Ok(Some(ranges))
}

#[allow(clippy::too_many_arguments)]
fn shape_long_prose_lines(
    shaper: &impl TextShaper,
    tokens: &[String],
    ranges: Vec<(usize, usize)>,
    line_count: usize,
    metrics: Metrics,
    bbox: Rect,
    cx: f32,
    cy: f32,
    a: f32,
    b: f32,
    shape: &str,
    mask: Option<&LayoutMask>,
) -> Result<Option<Vec<ShapedLine>>> {
    let block_height = line_count as f32 * (metrics.ascent + metrics.descent)
        + line_count.saturating_sub(1) as f32 * metrics.leading;
    let first_baseline = cy - block_height * 0.5 + metrics.ascent;
    let line_step = metrics.ascent + metrics.descent + metrics.leading;
    let mut lines = Vec::with_capacity(ranges.len());
    for (line, (start, end)) in ranges.into_iter().enumerate() {
        let segment = tokens[start..end].concat();
        let mut shaped = shaper.shape(&segment)?;
        let baseline = first_baseline + line as f32 * line_step;
        shaped.baseline = baseline;
        let top = baseline + shaped.top - 1.0;
        let bottom = baseline + shaped.bottom + 1.0;
        let available = available_width(bbox, cx, cy, a, b, top, bottom, shape, mask)?;
        let ink_width = shaped.ink_right - shaped.ink_left + 2.0;
        if ink_width > available + 1e-3 {
            return Ok(None);
        }
        shaped.top += baseline;
        shaped.bottom += baseline;
        lines.push(shaped);
    }
    Ok(Some(lines))
}

#[allow(clippy::too_many_arguments)]
fn solve_lines(
    shaper: &impl TextShaper,
    tokens: &[String],
    legal: &[bool],
    line: usize,
    start: usize,
    line_count: usize,
    first_baseline: f32,
    ascent: f32,
    descent: f32,
    leading: f32,
    bbox: Rect,
    cx: f32,
    cy: f32,
    a: f32,
    b: f32,
    shape: &str,
    mask: Option<&LayoutMask>,
    memo: &mut HashMap<(usize, usize), SolveResult>,
) -> Result<SolveResult> {
    if line == line_count {
        return Ok((start == tokens.len()).then_some((0.0, Vec::new())));
    }
    if let Some(value) = memo.get(&(line, start)) {
        return Ok(value.clone());
    }
    let remaining_lines = line_count - line;
    let mut best: Option<(f32, Vec<(usize, ShapedLine)>)> = None;
    if start == tokens.len() {
        if remaining_lines == 1 {
            let mut empty = shaper.shape("")?;
            let baseline = first_baseline + line as f32 * (ascent + descent + leading);
            empty.baseline = baseline;
            best = Some((0.0, vec![(line, empty)]));
        }
        memo.insert((line, start), best.clone());
        return Ok(best);
    }
    let last_end = tokens
        .iter()
        .skip(start)
        .position(|t| t == "\n")
        .map(|i| start + i)
        .unwrap_or(tokens.len());
    if tokens[start] == "\n" {
        if remaining_lines > 1 {
            let tail = solve_lines(
                shaper,
                tokens,
                legal,
                line + 1,
                start + 1,
                line_count,
                first_baseline,
                ascent,
                descent,
                leading,
                bbox,
                cx,
                cy,
                a,
                b,
                shape,
                mask,
                memo,
            )?;
            if let Some((cost, mut lines)) = tail {
                let mut empty = shaper.shape("")?;
                empty.baseline = first_baseline + line as f32 * (ascent + descent + leading);
                lines.push((line, empty));
                best = Some((cost, lines));
            }
        }
    } else {
        for end in (start + 1)..=last_end {
            if !legal[end] && end != last_end {
                continue;
            }
            let segment = tokens[start..end].concat();
            let mut shaped = shaper.shape(&segment)?;
            let baseline = first_baseline + line as f32 * (ascent + descent + leading);
            shaped.baseline = baseline;
            let q = 1.0;
            let top = baseline + shaped.top - q;
            let bottom = baseline + shaped.bottom + q;
            let available = available_width(bbox, cx, cy, a, b, top, bottom, shape, mask)?;
            let ink_width = shaped.ink_right - shaped.ink_left + 2.0 * q;
            if ink_width > available + 1e-3 {
                continue;
            }
            let next = if end < tokens.len() && tokens[end] == "\n" {
                end + 1
            } else {
                end
            };
            if line + 1 == line_count && next != tokens.len() {
                continue;
            }
            let tail = solve_lines(
                shaper,
                tokens,
                legal,
                line + 1,
                next,
                line_count,
                first_baseline,
                ascent,
                descent,
                leading,
                bbox,
                cx,
                cy,
                a,
                b,
                shape,
                mask,
                memo,
            )?;
            if let Some((cost, mut lines)) = tail {
                let ragged = (available - ink_width).max(0.0);
                let total = cost + ragged * ragged;
                lines.push((line, shaped));
                if best.as_ref().is_none_or(|(old, _)| total < *old) {
                    best = Some((total, lines));
                }
            }
        }
    }
    memo.insert((line, start), best.clone());
    Ok(best)
}

#[allow(clippy::too_many_arguments)]
fn available_width(
    bbox: Rect,
    cx: f32,
    cy: f32,
    a: f32,
    b: f32,
    top: f32,
    bottom: f32,
    shape: &str,
    mask: Option<&LayoutMask>,
) -> Result<f32> {
    if let Some(mask) = mask {
        let Some((mask_left, mask_right)) = mask.interval_for_band(top, bottom, cx) else {
            return Ok(0.0);
        };
        // The line is centred on `cx`, so use the narrower side of the
        // interval.  Intersect with the detector safe box as well; a mask
        // may contain a little more contour than the requested text box.
        let left = mask_left.max(bbox.x1);
        let right = mask_right.min(bbox.x2);
        let half_width = (cx - left).min(right - cx);
        let result = 2.0 * half_width - 2.0;
        return Ok(if result.is_finite() && result > 0.0 {
            result
        } else {
            0.0
        });
    }
    if shape == "rectangle" {
        return Ok(2.0 * a);
    }
    let d = (top - cy).abs().max((bottom - cy).abs());
    if d > b {
        return Ok(0.0);
    }
    let half = a * (1.0 - (d / b).powi(2)).max(0.0).sqrt();
    let result = 2.0 * (half - 1.0);
    if !result.is_finite() || result <= 0.0 {
        return Ok(0.0);
    }
    let _ = cx;
    Ok(result)
}
