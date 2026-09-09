use anyhow::{Result, anyhow, bail};
use rustybuzz::{Face, UnicodeBuffer};
use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation;

use crate::domain::Rect;

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
}

#[derive(Clone, Copy)]
struct Metrics {
    ascent: f32,
    descent: f32,
    leading: f32,
}

type SolveResult = Option<(f32, Vec<(usize, ShapedLine)>)>;

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
        let mut runs = Vec::<(usize, String)>::new();
        for cluster in UnicodeSegmentation::graphemes(text, true) {
            let index = choose_font_index(self.fonts, cluster)?;
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
    let area = bubble_bbox.filter(|rect| valid_rect(*rect)).unwrap_or(bbox);
    let padding = padding
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or_else(|| (area.x2 - area.x1).min(area.y2 - area.y1) * 0.06);
    let padding = padding.clamp(4.0, 16.0);
    let safe_bbox = inset_rect(area, padding)?;
    let anchor = text_bbox.filter(|rect| valid_rect(*rect)).unwrap_or(area);
    let placement_center = (
        ((anchor.x1 + anchor.x2) * 0.5).clamp(safe_bbox.x1, safe_bbox.x2),
        ((anchor.y1 + anchor.y2) * 0.5).clamp(safe_bbox.y1, safe_bbox.y2),
    );
    let mut size = (max_font_size * 2.0).floor() / 2.0;
    let floor = (min_font_size * 2.0).ceil() / 2.0;
    if size < floor {
        size = max_font_size;
    }
    while size + 1e-4 >= floor {
        let shaper = factory(size)?;
        if let Some(lines) = fit_at_size(&shaper, text, safe_bbox, shape, placement_center)? {
            return Ok(LayoutResult {
                font_size: size,
                lines,
                safe_bbox,
                padding,
                placement_center,
            });
        }
        size -= 0.5;
    }
    Err(anyhow!(
        "TextOverflow: supplied text does not fit at minimum font size {min_font_size:.1}px"
    ))
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
) -> Result<Option<Vec<ShapedLine>>> {
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
            cx,
            cy,
            a,
            b,
            shape,
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
    cx: f32,
    cy: f32,
    a: f32,
    b: f32,
    shape: &str,
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
                cx,
                cy,
                a,
                b,
                shape,
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
            let available = available_width(cx, cy, a, b, top, bottom, shape)?;
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
                cx,
                cy,
                a,
                b,
                shape,
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

fn available_width(
    cx: f32,
    cy: f32,
    a: f32,
    b: f32,
    top: f32,
    bottom: f32,
    shape: &str,
) -> Result<f32> {
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
