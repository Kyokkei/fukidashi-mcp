//! Font shaped comic text layout and raster composition.

mod layout;

pub use layout::{
    FontCandidate, FontRun, LayoutMask, LayoutResult, ShapedGlyph, ShapedLine, fit_text,
    fit_text_with_font_candidates, fit_text_with_font_candidates_masked, fit_text_with_geometry,
};

use anyhow::{Context, Result, anyhow};
use image::{DynamicImage, Rgba, RgbaImage};
use serde_json::json;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::domain::TypesetPayload;
use crate::workflow::CleanArtifact;

const AUTO_INK_LUMA_THRESHOLD: u8 = 110;
const AUTO_INK_SAMPLE_FRACTION: f32 = 0.4;
const AUTO_INK_MIN_WINDOW: i32 = 5;
const AUTO_INK_MAX_WINDOW: i32 = 128;

#[derive(Debug)]
struct FontAsset {
    path: String,
    bytes: Vec<u8>,
    font: fontdue::Font,
}

/// Render translated text over a page image using the requested primary face
/// plus ordered per-grapheme fallbacks. Text is never truncated: an unfit
/// bubble is an error.
pub fn typeset_page(
    image_path: &Path,
    bubbles: &[TypesetPayload],
    output_path: &Path,
) -> Result<serde_json::Value> {
    typeset_page_with_fallbacks(image_path, bubbles, &[], output_path)
}

/// Render a page with an ordered primary/fallback font set. Each grapheme is
/// assigned independently, while adjacent graphemes using one face are shaped
/// as a single run.
pub fn typeset_page_with_fallbacks(
    image_path: &Path,
    bubbles: &[TypesetPayload],
    fallback_font_paths: &[String],
    output_path: &Path,
) -> Result<serde_json::Value> {
    let source = fs::read(image_path)
        .with_context(|| format!("read page image {}", image_path.display()))?;
    let mut image = image::load_from_memory(&source)
        .with_context(|| format!("decode page image {}", image_path.display()))?
        .to_rgba8();
    let source_abs = fs::canonicalize(image_path).unwrap_or_else(|_| image_path.to_path_buf());
    let output_abs = output_path.to_path_buf();
    if source_abs == output_abs.canonicalize().unwrap_or(output_abs.clone()) {
        return Err(anyhow!(
            "typeset output must not overwrite the source image"
        ));
    }

    // Infer every component from the same unmodified page snapshot.  Drawing
    // an earlier bubble must not alter the pixels used to isolate a later,
    // overlapping bubble.
    let mask_source = image.clone();
    let balloon_areas = bubbles
        .iter()
        .map(|payload| {
            payload
                .bubble_bbox
                .filter(|area| valid_balloon_rect(*area))
                .unwrap_or(payload.bbox)
        })
        .collect::<Vec<_>>();
    let mut reports = Vec::with_capacity(bubbles.len());
    for (index, payload) in bubbles.iter().enumerate() {
        payload.validate_text_color()?;
        // A preserved item is deliberately left as source pixels. Empty text
        // is also non-renderable: it must not reach layout fitting, where a
        // legacy state can turn an otherwise valid rerender into a fit error.
        // Keep a report entry so the editor can retain the item and explain
        // why no glyphs were emitted.
        let skip_reason = payload_skip_reason(payload);
        if let Some(skip_reason) = skip_reason {
            reports.push(json!({
                "index": index,
                "id": payload.id,
                "source_text": payload.source_text,
                "kind": payload.kind,
                "preserve_by_default": payload.preserve_by_default,
                "preserve_source": payload.preserve_source,
                "text": payload.text,
                "text_color": payload.text_color,
                "requested_text_color": payload.text_color,
                "resolved_text_color": serde_json::Value::Null,
                "sampled_luminance": serde_json::Value::Null,
                "skipped": true,
                "skip_reason": skip_reason,
            }));
            continue;
        }
        let requested_font_path = payload.font_path.as_ref().ok_or_else(|| {
            anyhow!("bubble {index} has no font_path; a real font is required for typesetting")
        })?;
        let assets = load_font_assets(payload, fallback_font_paths, index)?;
        let candidates = assets
            .iter()
            .map(|asset| FontCandidate {
                id: &asset.path,
                bytes: &asset.bytes,
                font: &asset.font,
            })
            .collect::<Vec<_>>();
        let rect = payload.bbox;
        let min = payload.min_font_size.unwrap_or(8.0);
        let max = payload.max_font_size.unwrap_or(72.0);
        let shape = payload.shape.as_deref().unwrap_or("ellipse");
        let balloon_area = balloon_areas[index];
        let balloon_mask = infer_balloon_mask_owned(
            &mask_source,
            balloon_area,
            payload.text_bbox,
            &balloon_areas,
            index,
        )
        .or_else(|| {
            // A dark or heavily occluded balloon may not yield a reliable
            // connected component.  If its detector rectangle overlaps a
            // neighbor, keep ownership explicit with the requested shape so
            // layout and raster containment cannot fall back to the full
            // shared rectangle.  Non-overlapping failures intentionally use
            // the existing full shape envelope in the solver.
            let overlaps_other = balloon_areas
                .iter()
                .enumerate()
                .any(|(other_index, other)| {
                    other_index != index
                        && valid_balloon_rect(*other)
                        && rects_overlap(balloon_area, *other)
                });
            if overlaps_other {
                synthetic_balloon_mask(&mask_source, balloon_area, shape, &balloon_areas, index)
            } else {
                None
            }
        });
        let layout = fit_text_with_font_candidates_masked(
            &candidates,
            &payload.text,
            rect,
            payload.bubble_bbox,
            payload.text_bbox,
            shape,
            min,
            max,
            payload.padding,
            balloon_mask,
        )
        .with_context(|| format!("fit text for bubble {index}"))?;
        // An explicit override is authoritative and does not need (or report)
        // a clean-image sample. Auto mode alone performs contrast detection.
        let (resolved_text_color, sampled_luminance) = match payload.text_color.as_deref() {
            Some("white") => ("white", None),
            Some("black") => ("black", None),
            _ => {
                let sampled = sample_center_luminance(&mask_source, &layout);
                (resolve_auto_text_color(sampled), sampled)
            }
        };
        raster_layout(&mut image, &candidates, &layout, resolved_text_color)?;
        let ink_bbox = layout_ink_bbox(&layout);
        let mut used = HashSet::new();
        let mut font_runs = Vec::new();
        for line in &layout.lines {
            for run in &line.font_runs {
                used.insert(run.font_index);
                font_runs.push(json!({
                    "font_index": run.font_index,
                    "font_id": run.font_id,
                    "text": run.text,
                }));
            }
        }
        let fallback_fonts_used = candidates
            .iter()
            .enumerate()
            .filter(|(candidate_index, _)| *candidate_index > 0 && used.contains(candidate_index))
            .map(|(_, candidate)| candidate.id.to_owned())
            .collect::<Vec<_>>();
        let fallback_font_paths = candidates
            .iter()
            .skip(1)
            .map(|candidate| candidate.id.to_owned())
            .collect::<Vec<_>>();
        let primary_used = used.contains(&0);
        reports.push(json!({
            "index": index,
            "input_bbox": rect,
            "bubble_bbox": payload.bubble_bbox,
            "text_bbox": payload.text_bbox,
            "safe_bbox": layout.safe_bbox,
            "safe_mask_bbox": layout.safe_mask.as_ref().and_then(LayoutMask::bounds),
            "mask_used": layout.safe_mask.is_some(),
            "padding": layout.padding,
            "placement_center": layout.placement_center,
            "font_size": layout.font_size,
            "lines": layout.lines.iter().map(|line| line.text.clone()).collect::<Vec<_>>(),
            "line_count": layout.lines.len(),
            "ink_bbox": ink_bbox,
            "shape": shape,
            "text_color": payload.text_color,
            "requested_text_color": payload.text_color,
            "resolved_text_color": resolved_text_color,
            "sampled_luminance": sampled_luminance,
            "requested_font_path": requested_font_path,
            "requested_primary_font": requested_font_path,
            // `font_path` remains the requested primary for sidecar/editor
            // compatibility; `resolved_font_path` identifies the sole face
            // only when a page happens to use fallback glyphs exclusively.
            "font_path": requested_font_path,
            "resolved_font_path": if !primary_used && fallback_fonts_used.len() == 1 {
                fallback_fonts_used[0].clone()
            } else {
                requested_font_path.clone()
            },
            // Keep every validated fallback candidate in preference order.
            // `fallback_fonts_used` remains the subset used by this render;
            // editor rerenders need the complete list after a later text edit.
            "fallback_font_paths": fallback_font_paths,
            "fallback_fonts_used": fallback_fonts_used,
            "font_fallback_used": !fallback_fonts_used.is_empty(),
            "mixed_font_fallback_used": primary_used && !fallback_fonts_used.is_empty(),
            "font_runs": font_runs,
        }));
    }

    let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create output directory {}", parent.display()))?;
    let temporary =
        tempfile::NamedTempFile::new_in(parent).context("create atomic typeset output")?;
    DynamicImage::ImageRgba8(image)
        .save_with_format(temporary.path(), image::ImageFormat::Png)
        .context("encode typeset PNG")?;
    temporary
        .persist(output_path)
        .map_err(|e| anyhow!("promote typeset output: {}", e.error))?;
    let absolute = fs::canonicalize(output_path).unwrap_or_else(|_| output_path.to_path_buf());
    Ok(json!({"output_path": absolute, "bubbles": reports}))
}

/// Recover the light connected component that contains one bubble's interior.
/// Detector rectangles are deliberately only a crop and a guard: when two
/// rectangles overlap, the component boundary in the cleaned page decides
/// which pixels belong to this bubble.  A broad unbounded white page is
/// rejected and falls back to the existing rectangle/ellipse geometry.
#[cfg(test)]
fn infer_balloon_mask(
    image: &RgbaImage,
    area: crate::domain::Rect,
    anchor: Option<crate::domain::Rect>,
) -> Option<LayoutMask> {
    infer_balloon_mask_owned(image, area, anchor, &[], 0)
}

fn infer_balloon_mask_owned(
    image: &RgbaImage,
    area: crate::domain::Rect,
    anchor: Option<crate::domain::Rect>,
    all_areas: &[crate::domain::Rect],
    current_index: usize,
) -> Option<LayoutMask> {
    let area = area.clip(image.width() as f32, image.height() as f32)?;
    let origin_x = area.x1.floor().max(0.0) as u32;
    let origin_y = area.y1.floor().max(0.0) as u32;
    let end_x = area.x2.ceil().min(image.width() as f32) as u32;
    let end_y = area.y2.ceil().min(image.height() as f32) as u32;
    let width = usize::try_from(end_x.saturating_sub(origin_x)).ok()?;
    let height = usize::try_from(end_y.saturating_sub(origin_y)).ok()?;
    if width < 8 || height < 8 {
        return None;
    }

    let mut luminance = Vec::with_capacity(width.saturating_mul(height));
    let mut brightest = 0_u8;
    for y in origin_y..end_y {
        for x in origin_x..end_x {
            let pixel = image.get_pixel(x, y);
            let value = ((u32::from(pixel[0]) * 299
                + u32::from(pixel[1]) * 587
                + u32::from(pixel[2]) * 114)
                / 1000) as u8;
            brightest = brightest.max(value);
            luminance.push(value);
        }
    }
    // Light speech balloons are the common case.  For a dark/colored balloon
    // use a local high-value threshold, but do not mistake an all-dark crop
    // for a recoverable component.
    if brightest < 96 {
        return None;
    }
    let threshold = if brightest >= 200 {
        180
    } else {
        brightest.saturating_sub(24).max(96)
    };
    let binary = luminance
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let x = origin_x + (index % width) as u32;
            let y = origin_y + (index / width) as u32;
            *value >= threshold && point_in_rect(x, y, area)
        })
        .collect::<Vec<_>>();
    let anchor = anchor
        .filter(|rect| {
            rect.x1.is_finite()
                && rect.y1.is_finite()
                && rect.x2.is_finite()
                && rect.y2.is_finite()
                && rect.x2 > rect.x1
                && rect.y2 > rect.y1
        })
        .unwrap_or(area);
    let center_x = ((anchor.x1 + anchor.x2) * 0.5 - origin_x as f32).clamp(0.0, width as f32 - 1.0);
    let center_y =
        ((anchor.y1 + anchor.y2) * 0.5 - origin_y as f32).clamp(0.0, height as f32 - 1.0);
    let seed = binary
        .iter()
        .enumerate()
        .filter(|(_, inside)| **inside)
        .min_by(|(left, _), (right, _)| {
            let left_x = (*left % width) as f32;
            let left_y = (*left / width) as f32;
            let right_x = (*right % width) as f32;
            let right_y = (*right / width) as f32;
            let left_distance = (left_x - center_x).hypot(left_y - center_y);
            let right_distance = (right_x - center_x).hypot(right_y - center_y);
            left_distance.total_cmp(&right_distance)
        })
        .map(|(index, _)| index)?;

    let mut component = vec![false; binary.len()];
    let mut queue = std::collections::VecDeque::from([seed]);
    component[seed] = true;
    while let Some(index) = queue.pop_front() {
        let x = index % width;
        let y = index / width;
        for (dx, dy) in [(1_i32, 0_i32), (-1, 0), (0, 1), (0, -1)] {
            let next_x = x as i32 + dx;
            let next_y = y as i32 + dy;
            if next_x < 0 || next_y < 0 || next_x >= width as i32 || next_y >= height as i32 {
                continue;
            }
            let next = next_y as usize * width + next_x as usize;
            if binary[next] && !component[next] {
                component[next] = true;
                queue.push_back(next);
            }
        }
    }
    if !all_areas.is_empty() {
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                if component[index]
                    && !pixel_owned_by_center(
                        origin_x + x as u32,
                        origin_y + y as u32,
                        area,
                        all_areas,
                        current_index,
                    )
                {
                    component[index] = false;
                }
            }
        }
    }
    let component_pixels = component.iter().filter(|inside| **inside).count();
    if component_pixels < 24 {
        return None;
    }
    let min_x = component
        .iter()
        .enumerate()
        .filter(|(_, inside)| **inside)
        .map(|(index, _)| index % width)
        .min()?;
    let max_x = component
        .iter()
        .enumerate()
        .filter(|(_, inside)| **inside)
        .map(|(index, _)| index % width)
        .max()?;
    let min_y = component
        .iter()
        .enumerate()
        .filter(|(_, inside)| **inside)
        .map(|(index, _)| index / width)
        .min()?;
    let max_y = component
        .iter()
        .enumerate()
        .filter(|(_, inside)| **inside)
        .map(|(index, _)| index / width)
        .max()?;
    if max_x.saturating_sub(min_x) < 5 || max_y.saturating_sub(min_y) < 5 {
        return None;
    }
    // A component that reaches every crop edge is usually the unbounded page
    // background.  Do not require dark pixels on the crop perimeter: a valid
    // closed contour can sit inside white padding, and one dark perimeter
    // pixel is not evidence that a contour is actually closed.
    let touches_left = (0..height).any(|y| component[y * width]);
    let touches_right = (0..height).any(|y| component[y * width + width - 1]);
    let touches_top = (0..width).any(|x| component[x]);
    let touches_bottom = (0..width).any(|x| component[(height - 1) * width + x]);
    if touches_left && touches_right && touches_top && touches_bottom {
        return None;
    }
    if component_pixels * 100 < width.saturating_mul(height).saturating_mul(4) {
        return None;
    }
    let ellipse_area = std::f32::consts::PI * (area.x2 - area.x1) * (area.y2 - area.y1) * 0.25;
    if (component_pixels as f32) < ellipse_area * 0.35 {
        return None;
    }
    LayoutMask::from_binary(origin_x as i32, origin_y as i32, width, height, component).ok()
}

/// Build a geometry-only ownership mask when pixels cannot identify a
/// balloon. The detector supplies rectangles, so the requested shape is the
/// only safe envelope available. Overlap pixels are assigned by nearest
/// detector centre, matching connected-component ownership above.
fn synthetic_balloon_mask(
    image: &RgbaImage,
    area: crate::domain::Rect,
    shape: &str,
    all_areas: &[crate::domain::Rect],
    current_index: usize,
) -> Option<LayoutMask> {
    if shape != "ellipse" && shape != "rectangle" {
        return None;
    }
    let area = area.clip(image.width() as f32, image.height() as f32)?;
    let origin_x = area.x1.floor().max(0.0) as u32;
    let origin_y = area.y1.floor().max(0.0) as u32;
    let end_x = area.x2.ceil().min(image.width() as f32) as u32;
    let end_y = area.y2.ceil().min(image.height() as f32) as u32;
    let width = usize::try_from(end_x.saturating_sub(origin_x)).ok()?;
    let height = usize::try_from(end_y.saturating_sub(origin_y)).ok()?;
    if width < 8 || height < 8 {
        return None;
    }
    let center = ((area.x1 + area.x2) * 0.5, (area.y1 + area.y2) * 0.5);
    let radius = ((area.x2 - area.x1) * 0.5, (area.y2 - area.y1) * 0.5);
    let mut pixels = vec![false; width.saturating_mul(height)];
    for y in origin_y..end_y {
        for x in origin_x..end_x {
            let inside_shape = if shape == "rectangle" {
                point_in_rect(x, y, area)
            } else {
                let dx = (x as f32 + 0.5 - center.0) / radius.0;
                let dy = (y as f32 + 0.5 - center.1) / radius.1;
                dx * dx + dy * dy <= 1.0
            };
            if inside_shape && pixel_owned_by_center(x, y, area, all_areas, current_index) {
                let local_x = (x - origin_x) as usize;
                let local_y = (y - origin_y) as usize;
                pixels[local_y * width + local_x] = true;
            }
        }
    }
    LayoutMask::from_binary_geometry(origin_x as i32, origin_y as i32, width, height, pixels).ok()
}

fn valid_balloon_rect(rect: crate::domain::Rect) -> bool {
    rect.x1.is_finite()
        && rect.y1.is_finite()
        && rect.x2.is_finite()
        && rect.y2.is_finite()
        && rect.x2 > rect.x1
        && rect.y2 > rect.y1
}

fn point_in_rect(x: u32, y: u32, rect: crate::domain::Rect) -> bool {
    let x = x as f32 + 0.5;
    let y = y as f32 + 0.5;
    x >= rect.x1 && x < rect.x2 && y >= rect.y1 && y < rect.y2
}

fn pixel_owned_by_center(
    x: u32,
    y: u32,
    current_area: crate::domain::Rect,
    all_areas: &[crate::domain::Rect],
    current_index: usize,
) -> bool {
    if !point_in_rect(x, y, current_area) {
        return false;
    }
    let px = x as f32 + 0.5;
    let py = y as f32 + 0.5;
    let current_center = (
        (current_area.x1 + current_area.x2) * 0.5,
        (current_area.y1 + current_area.y2) * 0.5,
    );
    let current_distance = (px - current_center.0).powi(2) + (py - current_center.1).powi(2);
    let mut owner = current_index;
    let mut owner_distance = current_distance;
    for (index, other) in all_areas.iter().copied().enumerate() {
        if index == current_index
            || !valid_balloon_rect(other)
            || !point_in_rect(x, y, other)
            || !rects_overlap(current_area, other)
        {
            continue;
        }
        let center = ((other.x1 + other.x2) * 0.5, (other.y1 + other.y2) * 0.5);
        let distance = (px - center.0).powi(2) + (py - center.1).powi(2);
        if distance < owner_distance || (distance == owner_distance && index < owner) {
            owner = index;
            owner_distance = distance;
        }
    }
    owner == current_index
}

fn rects_overlap(left: crate::domain::Rect, right: crate::domain::Rect) -> bool {
    left.x1 < right.x2 && left.x2 > right.x1 && left.y1 < right.y2 && left.y2 > right.y1
}

fn payload_skip_reason(payload: &TypesetPayload) -> Option<&'static str> {
    if payload.text.trim().is_empty() {
        return Some("empty_text");
    }
    if let Some(explicit) = payload.preserve_source {
        return explicit.then_some("preserve_source");
    }
    if payload.preserve_by_default.unwrap_or(false) {
        return Some("preserve_by_default");
    }
    if payload
        .kind
        .as_deref()
        .is_some_and(|kind| kind == "unmatched_text")
        || payload
            .id
            .as_deref()
            .is_some_and(|id| id.starts_with("text-"))
    {
        return Some("structural_unmatched_text");
    }
    None
}

fn load_font_assets(
    payload: &TypesetPayload,
    fallback_font_paths: &[String],
    bubble_index: usize,
) -> Result<Vec<FontAsset>> {
    let requested = payload.font_path.as_ref().ok_or_else(|| {
        anyhow!("bubble {bubble_index} has no font_path; a real font is required for typesetting")
    })?;
    let mut paths =
        Vec::with_capacity(1 + payload.fallback_font_paths.len() + fallback_font_paths.len() + 8);
    paths.push(requested.clone());
    paths.extend(payload.fallback_font_paths.iter().cloned());
    paths.extend(fallback_font_paths.iter().cloned());
    paths.extend(
        crate::workflow::configured_font_paths()
            .into_iter()
            .map(|path| path.display().to_string()),
    );
    let common_names = [
        "segoeui.ttf",
        "arial.ttf",
        "calibri.ttf",
        "seguisym.ttf",
        "DejaVuSans.ttf",
        "NotoSans-Regular.ttf",
    ];
    for directory in crate::workflow::font_search_dirs() {
        for name in common_names {
            paths.push(directory.join(name).display().to_string());
        }
    }
    paths.dedup();

    let mut assets = Vec::with_capacity(paths.len());
    for (candidate_index, path) in paths.into_iter().enumerate() {
        let Ok(bytes) = fs::read(&path) else {
            if candidate_index == 0 {
                return Err(anyhow!("read font for bubble {bubble_index}: {path}"));
            }
            continue;
        };
        let font =
            match fontdue::Font::from_bytes(bytes.as_slice(), fontdue::FontSettings::default()) {
                Ok(font) => font,
                Err(error) => {
                    if candidate_index == 0 {
                        return Err(anyhow!(
                            "unable to parse raster font {path} for bubble {bubble_index}: {error}"
                        ));
                    }
                    continue;
                }
            };
        if rustybuzz::Face::from_slice(&bytes, 0).is_none() {
            if candidate_index == 0 {
                return Err(anyhow!(
                    "unable to parse font {path} for bubble {bubble_index}"
                ));
            }
            continue;
        }
        assets.push(FontAsset { path, bytes, font });
    }
    if assets.is_empty() {
        return Err(anyhow!(
            "no usable font candidates remain for bubble {bubble_index}"
        ));
    }
    Ok(assets)
}

/// Deterministic checks that run after rasterization and before the artifact is
/// exposed to the review UI.  Geometry failures become review items instead of
/// silently producing a page that looks finished in the agent transcript.
pub fn post_render_qa(
    clean: &CleanArtifact,
    rendered: &serde_json::Value,
) -> Result<serde_json::Value> {
    let mut issues = Vec::new();
    if clean.changed_ratio < 0.15 {
        issues.push(json!({
            "issue_type": "leftover_source_text",
            "severity": "error",
            "message": format!("clean stage changed only {:.1}% of masked pixels", clean.changed_ratio * 100.0),
        }));
    }
    if clean.source_dark_pixels > 0 && clean.dark_pixel_reduction_ratio < 0.10 {
        issues.push(json!({
            "issue_type": "leftover_source_text",
            "severity": "error",
            "message": format!(
                "dark pixels in the clean mask fell by only {:.1}% ({}/{})",
                clean.dark_pixel_reduction_ratio * 100.0,
                clean.cleaned_dark_pixels,
                clean.source_dark_pixels
            ),
        }));
    }
    for bubble in rendered
        .get("bubbles")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if bubble
            .get("skipped")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let safe = bubble
            .get("safe_bbox")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value::<crate::domain::Rect>)
            .transpose()
            .context("typeset QA safe_bbox is malformed")?;
        let ink = bubble
            .get("ink_bbox")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value::<crate::domain::Rect>)
            .transpose()
            .context("typeset QA ink_bbox is malformed")?;
        if let (Some(safe), Some(ink)) = (safe, ink) {
            let inside =
                ink.x1 >= safe.x1 && ink.y1 >= safe.y1 && ink.x2 <= safe.x2 && ink.y2 <= safe.y2;
            if !inside {
                issues.push(json!({
                    "issue_type": "text_overflow",
                    "severity": "error",
                    "bubble_index": bubble.get("index"),
                    "safe_bbox": safe,
                    "ink_bbox": ink,
                    "message": "rasterized glyph bounds escape the safe inset region",
                }));
            }
        } else {
            issues.push(json!({
                "issue_type": "font_or_layout",
                "severity": "error",
                "bubble_index": bubble.get("index"),
                "message": "typeset report has no measurable ink bounds",
            }));
        }
    }
    Ok(json!({
        "status": if issues.is_empty() { "pass" } else { "needs_review" },
        "issue_count": issues.len(),
        "issues": issues,
        "clean_changed_ratio": clean.changed_ratio,
        "source_dark_pixels": clean.source_dark_pixels,
        "cleaned_dark_pixels": clean.cleaned_dark_pixels,
        "dark_pixel_reduction_ratio": clean.dark_pixel_reduction_ratio,
    }))
}

fn layout_ink_bbox(layout: &crate::typeset::LayoutResult) -> Option<crate::domain::Rect> {
    let mut x1 = f32::INFINITY;
    let mut y1 = f32::INFINITY;
    let mut x2 = f32::NEG_INFINITY;
    let mut y2 = f32::NEG_INFINITY;
    for line in &layout.lines {
        let origin_x = layout.placement_center.0 - (line.ink_left + line.ink_right) * 0.5;
        x1 = x1.min(origin_x + line.ink_left);
        y1 = y1.min(line.top);
        x2 = x2.max(origin_x + line.ink_right);
        y2 = y2.max(line.bottom);
    }
    (x1.is_finite() && y1.is_finite() && x2.is_finite() && y2.is_finite())
        .then_some(crate::domain::Rect { x1, y1, x2, y2 })
}

fn raster_layout(
    image: &mut image::RgbaImage,
    fonts: &[FontCandidate<'_>],
    layout: &LayoutResult,
    resolved_text_color: &str,
) -> Result<()> {
    let cx = layout.placement_center.0;
    let rgba = if resolved_text_color == "white" {
        Rgba([255_u8, 255_u8, 255_u8, 255_u8])
    } else {
        Rgba([0_u8, 0_u8, 0_u8, 255_u8])
    };
    for line in &layout.lines {
        let origin_x = cx - (line.ink_left + line.ink_right) * 0.5;
        for glyph in &line.glyphs {
            let font = fonts
                .get(glyph.font_index)
                .ok_or_else(|| anyhow!("glyph font index {} is out of range", glyph.font_index))?
                .font;
            let gid = u16::try_from(glyph.glyph_id)
                .map_err(|_| anyhow!("glyph id exceeds fontdue range"))?;
            let (metrics, bitmap) = font.rasterize_indexed(gid, layout.font_size);
            let px = (origin_x + glyph.x + metrics.xmin as f32).round() as i32;
            let py = (line.baseline - glyph.y - metrics.ymin as f32 - metrics.height as f32).round()
                as i32;
            for row in 0..metrics.height {
                for col in 0..metrics.width {
                    let alpha = bitmap[row * metrics.width + col];
                    if alpha == 0 {
                        continue;
                    }
                    let x = px + col as i32;
                    let y = py + row as i32;
                    if x < 0 || y < 0 || x >= image.width() as i32 || y >= image.height() as i32 {
                        continue;
                    }
                    // The layout solver already rejects bands wider than the
                    // eroded component.  Keep the same mask as a final pixel
                    // guard for antialiased glyph bearings and irregular
                    // contours that cannot be represented by one rectangle.
                    if layout
                        .safe_mask
                        .as_ref()
                        .is_some_and(|mask| !mask.contains_pixel(x, y))
                    {
                        continue;
                    }
                    let dst = image.get_pixel_mut(x as u32, y as u32);
                    let a = (u16::from(alpha) * u16::from(rgba[3]) / 255) as u8;
                    let inv = 255u16 - u16::from(a);
                    for channel in 0..3 {
                        dst[channel] = ((u16::from(rgba[channel]) * u16::from(a)
                            + u16::from(dst[channel]) * inv)
                            / 255) as u8;
                    }
                    dst[3] = (u16::from(a) + u16::from(dst[3]) * inv / 255) as u8;
                }
            }
        }
    }
    Ok(())
}

fn resolve_auto_text_color(sampled_luminance: Option<u8>) -> &'static str {
    if sampled_luminance.is_some_and(|luma| luma < AUTO_INK_LUMA_THRESHOLD) {
        "white"
    } else {
        "black"
    }
}

/// Auto ink samples an adaptive central window around the layout anchor from
/// the clean page. The window covers about 40% of each usable safe dimension,
/// with a 128-pixel cap. Median integer luma and the 110 threshold resist
/// border pixels and broad detector padding; the renderer never uses the
/// balloon mask as a contrast detector.
fn sample_center_luminance(image: &RgbaImage, layout: &LayoutResult) -> Option<u8> {
    if image.width() == 0 || image.height() == 0 {
        return None;
    }
    if ![
        layout.safe_bbox.x1,
        layout.safe_bbox.y1,
        layout.safe_bbox.x2,
        layout.safe_bbox.y2,
        layout.placement_center.0,
        layout.placement_center.1,
    ]
    .into_iter()
    .all(|value| value.is_finite())
    {
        return None;
    }
    if layout.safe_bbox.x2 <= layout.safe_bbox.x1 || layout.safe_bbox.y2 <= layout.safe_bbox.y1 {
        return None;
    }
    let cx = layout.placement_center.0.round() as i32;
    let cy = layout.placement_center.1.round() as i32;
    let safe_x1 = layout.safe_bbox.x1.ceil() as i32;
    let safe_y1 = layout.safe_bbox.y1.ceil() as i32;
    let safe_x2 = layout.safe_bbox.x2.floor() as i32 - 1;
    let safe_y2 = layout.safe_bbox.y2.floor() as i32 - 1;
    let safe_width = (safe_x2 - safe_x1 + 1).max(1);
    let safe_height = (safe_y2 - safe_y1 + 1).max(1);
    let window_width = ((safe_width as f32 * AUTO_INK_SAMPLE_FRACTION).round() as i32)
        .clamp(AUTO_INK_MIN_WINDOW, AUTO_INK_MAX_WINDOW)
        .min(safe_width);
    let window_height = ((safe_height as f32 * AUTO_INK_SAMPLE_FRACTION).round() as i32)
        .clamp(AUTO_INK_MIN_WINDOW, AUTO_INK_MAX_WINDOW)
        .min(safe_height);
    let x1 = (cx - window_width / 2)
        .max(safe_x1)
        .max(0)
        .min(image.width() as i32 - 1);
    let y1 = (cy - window_height / 2)
        .max(safe_y1)
        .max(0)
        .min(image.height() as i32 - 1);
    let x2 = (x1 + window_width - 1)
        .min(safe_x2)
        .min(image.width() as i32 - 1);
    let y2 = (y1 + window_height - 1)
        .min(safe_y2)
        .min(image.height() as i32 - 1);
    if x1 > x2 || y1 > y2 {
        return None;
    }
    let mut values = Vec::with_capacity(((x2 - x1 + 1) * (y2 - y1 + 1)) as usize);
    for y in y1..=y2 {
        for x in x1..=x2 {
            let pixel = image.get_pixel(x as u32, y as u32);
            values.push(
                ((u32::from(pixel[0]) * 299
                    + u32::from(pixel[1]) * 587
                    + u32::from(pixel[2]) * 114)
                    / 1000) as u8,
            );
        }
    }
    values.sort_unstable();
    values.get(values.len() / 2).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Rect;

    #[test]
    fn auto_ink_uses_one_documented_mid_gray_threshold() {
        assert_eq!(resolve_auto_text_color(Some(109)), "white");
        assert_eq!(resolve_auto_text_color(Some(110)), "black");
        assert_eq!(resolve_auto_text_color(Some(180)), "black");
        assert_eq!(resolve_auto_text_color(None), "black");
    }

    #[test]
    fn auto_ink_sampling_rejects_empty_images_and_invalid_safe_geometry() {
        let layout = LayoutResult {
            font_size: 12.0,
            lines: Vec::new(),
            safe_bbox: Rect {
                x1: 8.0,
                y1: 8.0,
                x2: 8.0,
                y2: 8.0,
            },
            padding: 0.0,
            placement_center: (8.0, 8.0),
            safe_mask: None,
        };
        assert_eq!(
            sample_center_luminance(&RgbaImage::new(0, 0), &layout),
            None
        );
        assert_eq!(
            sample_center_luminance(
                &RgbaImage::from_pixel(16, 16, Rgba([0, 0, 0, 255])),
                &layout
            ),
            None
        );
    }

    #[test]
    fn per_grapheme_fallback_keeps_primary_and_uses_fallback_for_missing_cluster() {
        let directory = tempfile::tempdir().unwrap();
        let requested = directory.path().join("ComicNeue-Regular.ttf");
        let fallback = directory.path().join("NotoSansSymbols2-Regular.ttf");
        fs::write(&requested, crate::fonts::COMIC_NEUE_REGULAR.bytes).unwrap();
        fs::write(&fallback, crate::fonts::NOTO_SANS_SYMBOLS2_REGULAR.bytes).unwrap();
        let requested_bytes = fs::read(&requested).unwrap();
        let fallback_bytes = fs::read(&fallback).unwrap();
        let requested_font =
            fontdue::Font::from_bytes(requested_bytes.as_slice(), fontdue::FontSettings::default())
                .unwrap();
        let fallback_font =
            fontdue::Font::from_bytes(fallback_bytes.as_slice(), fontdue::FontSettings::default())
                .unwrap();
        let candidates = vec![
            FontCandidate {
                id: requested.to_str().unwrap(),
                bytes: &requested_bytes,
                font: &requested_font,
            },
            FontCandidate {
                id: fallback.to_str().unwrap(),
                bytes: &fallback_bytes,
                font: &fallback_font,
            },
        ];
        let layout = fit_text_with_font_candidates(
            &candidates,
            "Hello ❤",
            Rect {
                x1: 0.0,
                y1: 0.0,
                x2: 240.0,
                y2: 80.0,
            },
            None,
            None,
            "rectangle",
            8.0,
            24.0,
            None,
        )
        .unwrap();
        assert!(layout.lines[0].font_runs.len() >= 2);
        assert_eq!(layout.lines[0].font_runs[0].font_index, 0);
        assert_eq!(layout.lines[0].font_runs[1].font_index, 1);
        assert!(
            layout.lines[0]
                .glyphs
                .iter()
                .any(|glyph| glyph.font_index == 1)
        );
    }

    #[test]
    fn vietnamese_text_unifies_to_fallback_font_without_midword_splitting() {
        let primary_bytes = crate::fonts::COMIC_NEUE_REGULAR.bytes;
        let fallback_bytes = crate::fonts::PATRICK_HAND_REGULAR.bytes;
        let primary_font =
            fontdue::Font::from_bytes(primary_bytes, fontdue::FontSettings::default()).unwrap();
        let fallback_font =
            fontdue::Font::from_bytes(fallback_bytes, fontdue::FontSettings::default()).unwrap();
        let candidates = [
            FontCandidate {
                id: "comic-neue",
                bytes: primary_bytes,
                font: &primary_font,
            },
            FontCandidate {
                id: "patrick-hand",
                bytes: fallback_bytes,
                font: &fallback_font,
            },
        ];
        let layout = fit_text_with_font_candidates(
            &candidates,
            "Thằng nhóc dễ thương ghê~",
            Rect {
                x1: 0.0,
                y1: 0.0,
                x2: 500.0,
                y2: 200.0,
            },
            None,
            None,
            "rectangle",
            8.0,
            24.0,
            None,
        )
        .unwrap();
        // The entire Vietnamese sentence must be unified into Patrick Hand (font index 1)
        // rather than splitting Latin and accented graphemes across Comic Neue and Patrick Hand.
        for line in &layout.lines {
            for run in &line.font_runs {
                assert_eq!(
                    run.font_index, 1,
                    "all font runs in Vietnamese dialogue must use the Vietnamese font: {run:?}"
                );
            }
        }
    }

    #[test]
    fn unused_fallback_does_not_change_primary_metrics() {
        let primary_bytes = crate::fonts::COMIC_NEUE_REGULAR.bytes;
        let fallback_bytes = crate::fonts::NOTO_SANS_SYMBOLS2_REGULAR.bytes;
        let primary_font =
            fontdue::Font::from_bytes(primary_bytes, fontdue::FontSettings::default()).unwrap();
        let fallback_font =
            fontdue::Font::from_bytes(fallback_bytes, fontdue::FontSettings::default()).unwrap();
        let primary_only = [FontCandidate {
            id: "primary",
            bytes: primary_bytes,
            font: &primary_font,
        }];
        let with_unused_fallback = [
            primary_only[0],
            FontCandidate {
                id: "symbols",
                bytes: fallback_bytes,
                font: &fallback_font,
            },
        ];
        let bbox = Rect {
            x1: 0.0,
            y1: 0.0,
            x2: 320.0,
            y2: 100.0,
        };
        let first = fit_text_with_font_candidates(
            &primary_only,
            "A normal Vietnamese sentence",
            bbox,
            None,
            None,
            "rectangle",
            8.0,
            32.0,
            None,
        )
        .unwrap();
        let second = fit_text_with_font_candidates(
            &with_unused_fallback,
            "A normal Vietnamese sentence",
            bbox,
            None,
            None,
            "rectangle",
            8.0,
            32.0,
            None,
        )
        .unwrap();
        assert_eq!(first.font_size, second.font_size);
        assert_eq!(first.lines.len(), second.lines.len());
        for (left, right) in first.lines.iter().zip(second.lines.iter()) {
            assert!((left.advance_width - right.advance_width).abs() < f32::EPSILON);
            assert!((left.top - right.top).abs() < f32::EPSILON);
            assert!((left.bottom - right.bottom).abs() < f32::EPSILON);
            assert!(right.glyphs.iter().all(|glyph| glyph.font_index == 0));
        }
    }

    #[test]
    fn report_retains_full_fallback_list_for_later_symbol_edits() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.png");
        let first_output = directory.path().join("first.png");
        let second_output = directory.path().join("second.png");
        image::RgbaImage::from_pixel(500, 140, image::Rgba([255, 255, 255, 255]))
            .save(&source)
            .unwrap();
        let primary = directory.path().join("ComicNeue-Regular.ttf");
        let symbols = directory.path().join("NotoSansSymbols2-Regular.ttf");
        fs::write(&primary, crate::fonts::COMIC_NEUE_REGULAR.bytes).unwrap();
        fs::write(&symbols, crate::fonts::NOTO_SANS_SYMBOLS2_REGULAR.bytes).unwrap();
        let payload = |text: &str| TypesetPayload {
            id: Some("bubble-1".into()),
            source_text: Some("source".into()),
            kind: Some("dialogue".into()),
            preserve_by_default: Some(false),
            needs_review: None,
            flagged: None,
            preserve_source: None,
            fallback_font_paths: vec![symbols.display().to_string()],
            bbox: Rect {
                x1: 20.0,
                y1: 20.0,
                x2: 480.0,
                y2: 120.0,
            },
            bubble_bbox: None,
            text_bbox: None,
            padding: None,
            text: text.into(),
            font_path: Some(primary.display().to_string()),
            min_font_size: Some(8.0),
            max_font_size: Some(28.0),
            text_color: None,
            shape: Some("rectangle".into()),
        };
        let first =
            typeset_page_with_fallbacks(&source, &[payload("Hello")], &[], &first_output).unwrap();
        let report = &first["bubbles"][0];
        let retained_fallbacks = report["fallback_font_paths"].clone();
        assert_eq!(retained_fallbacks[0], symbols.display().to_string());
        assert!(!retained_fallbacks.as_array().unwrap().is_empty());
        assert_eq!(report["fallback_fonts_used"], json!([]));
        let second =
            typeset_page_with_fallbacks(&source, &[payload("Hello ❤")], &[], &second_output)
                .unwrap();
        assert_eq!(
            second["bubbles"][0]["fallback_font_paths"],
            retained_fallbacks
        );
        assert_eq!(
            second["bubbles"][0]["fallback_fonts_used"],
            json!([symbols.display().to_string()])
        );
    }

    fn closed_balloon_fixture(width: u32, height: u32) -> RgbaImage {
        let mut image = RgbaImage::from_pixel(width, height, Rgba([255, 255, 255, 255]));
        for x in 15..(width - 15) {
            image.put_pixel(x, 12, Rgba([0, 0, 0, 255]));
            image.put_pixel(x, height - 13, Rgba([0, 0, 0, 255]));
        }
        for y in 12..(height - 12) {
            image.put_pixel(15, y, Rgba([0, 0, 0, 255]));
            image.put_pixel(width - 16, y, Rgba([0, 0, 0, 255]));
        }
        image
    }

    #[test]
    fn balloon_mask_accepts_a_closed_contour_inside_white_crop_padding() {
        let image = closed_balloon_fixture(96, 64);
        let area = Rect {
            x1: 4.0,
            y1: 4.0,
            x2: 92.0,
            y2: 60.0,
        };
        let anchor = Rect {
            x1: 35.0,
            y1: 24.0,
            x2: 55.0,
            y2: 40.0,
        };
        let mask = infer_balloon_mask(&image, area, Some(anchor))
            .expect("white component enclosed by an interior contour");
        let bounds = mask.bounds().expect("enclosed component bounds");
        assert!(bounds.x1 > area.x1);
        assert!(bounds.y1 > area.y1);
        assert!(bounds.x2 < area.x2);
        assert!(bounds.y2 < area.y2);
        assert!(mask.contains_pixel(48, 32));
        assert!(!mask.contains_pixel(15, 32));
    }

    #[test]
    fn balloon_mask_rejects_unbounded_white_crop_with_a_black_speck() {
        let mut image = RgbaImage::from_pixel(96, 64, Rgba([255, 255, 255, 255]));
        image.put_pixel(48, 32, Rgba([0, 0, 0, 255]));
        let area = Rect {
            x1: 4.0,
            y1: 4.0,
            x2: 92.0,
            y2: 60.0,
        };
        assert!(infer_balloon_mask(&image, area, None).is_none());
    }

    fn draw_ellipse(image: &mut RgbaImage, bbox: Rect) {
        let center = ((bbox.x1 + bbox.x2) * 0.5, (bbox.y1 + bbox.y2) * 0.5);
        let radius = ((bbox.x2 - bbox.x1) * 0.5, (bbox.y2 - bbox.y1) * 0.5);
        for y in bbox.y1 as u32..bbox.y2 as u32 {
            for x in bbox.x1 as u32..bbox.x2 as u32 {
                let dx = (x as f32 + 0.5 - center.0) / radius.0;
                let dy = (y as f32 + 0.5 - center.1) / radius.1;
                let distance = dx * dx + dy * dy;
                if distance <= 1.0 {
                    image.put_pixel(x, y, Rgba([255, 255, 255, 255]));
                }
                if (0.92..=1.08).contains(&distance) {
                    image.put_pixel(x, y, Rgba([0, 0, 0, 255]));
                }
            }
        }
    }

    #[test]
    fn overlapping_ellipse_masks_split_pixels_by_nearest_center() {
        let upper = Rect {
            x1: 20.0,
            y1: 18.0,
            x2: 120.0,
            y2: 88.0,
        };
        let lower = Rect {
            x1: 70.0,
            y1: 45.0,
            x2: 170.0,
            y2: 115.0,
        };
        let mut image = RgbaImage::from_pixel(192, 132, Rgba([24, 24, 24, 255]));
        draw_ellipse(&mut image, upper);
        draw_ellipse(&mut image, lower);
        let areas = [upper, lower];
        let upper_mask = infer_balloon_mask_owned(&image, upper, None, &areas, 0)
            .expect("upper ellipse should have a safe interior");
        let lower_mask = infer_balloon_mask_owned(&image, lower, None, &areas, 1)
            .expect("lower ellipse should have a safe interior");
        // This point lies inside both detector rectangles, but closer to the
        // lower centre. It must not remain available to the upper layout.
        assert!(!upper_mask.contains_pixel(100, 65));
        assert!(lower_mask.contains_pixel(100, 65));
        assert!(upper_mask.bounds().expect("upper bounds").x2 <= upper.x2);
        assert!(lower_mask.bounds().expect("lower bounds").x1 >= lower.x1);
    }

    #[test]
    fn rasterized_text_stays_inside_the_eroded_balloon_component() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.png");
        let output_path = directory.path().join("rendered.png");
        let source = closed_balloon_fixture(96, 64);
        source.save(&source_path).unwrap();
        let font_path = directory.path().join("ComicNeue-Regular.ttf");
        fs::write(&font_path, crate::fonts::COMIC_NEUE_REGULAR.bytes).unwrap();
        let area = Rect {
            x1: 4.0,
            y1: 4.0,
            x2: 92.0,
            y2: 60.0,
        };
        let text_bbox = Rect {
            x1: 28.0,
            y1: 20.0,
            x2: 68.0,
            y2: 44.0,
        };
        let payload = TypesetPayload {
            id: Some("bubble-1".into()),
            source_text: Some("source".into()),
            kind: Some("dialogue".into()),
            preserve_by_default: Some(false),
            needs_review: Some(false),
            flagged: Some(false),
            preserve_source: Some(false),
            fallback_font_paths: Vec::new(),
            bbox: area,
            bubble_bbox: Some(area),
            text_bbox: Some(text_bbox),
            padding: Some(4.0),
            text: "Mask text".into(),
            font_path: Some(font_path.display().to_string()),
            min_font_size: Some(8.0),
            max_font_size: Some(18.0),
            text_color: None,
            shape: Some("rectangle".into()),
        };
        let report =
            typeset_page_with_fallbacks(&source_path, &[payload], &[], &output_path).unwrap();
        assert_eq!(report["bubbles"][0]["mask_used"], true);
        let safe_mask = infer_balloon_mask(&source, area, Some(text_bbox))
            .and_then(|mask| mask.eroded(2))
            .expect("same mask used by the renderer");
        let rendered = image::open(&output_path).unwrap().to_rgba8();
        let mut changed_text_pixels = 0usize;
        for y in 0..rendered.height() {
            for x in 0..rendered.width() {
                let before = source.get_pixel(x, y);
                let after = rendered.get_pixel(x, y);
                if before[0] >= 240 && after[0] < 240 {
                    changed_text_pixels += 1;
                    assert!(
                        safe_mask.contains_pixel(x as i32, y as i32),
                        "text escaped eroded mask at ({x}, {y})"
                    );
                }
            }
        }
        assert!(changed_text_pixels > 0);
    }
}
