//! Font shaped comic text layout and raster composition.

mod layout;

pub use layout::{LayoutResult, ShapedGlyph, ShapedLine, fit_text, fit_text_with_geometry};

use anyhow::{Context, Result, anyhow};
use image::{DynamicImage, Rgba};
use rustybuzz::Face;
use serde_json::json;
use std::fs;
use std::path::Path;

use crate::domain::TypesetPayload;
use crate::workflow::CleanArtifact;

/// Render translated text over a page image using the same font face for shaping
/// and rasterization.  Text is never truncated: an unfit bubble is an error.
pub fn typeset_page(
    image_path: &Path,
    bubbles: &[TypesetPayload],
    output_path: &Path,
) -> Result<serde_json::Value> {
    typeset_page_with_fallbacks(image_path, bubbles, &[], output_path)
}

/// Render a page with ordered whole-font fallbacks. A fallback is selected
/// only when the requested font cannot represent every non-control character.
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

    let mut reports = Vec::with_capacity(bubbles.len());
    for (index, payload) in bubbles.iter().enumerate() {
        let rect = payload.bbox;
        if !rect.x1.is_finite()
            || !rect.y1.is_finite()
            || !rect.x2.is_finite()
            || !rect.y2.is_finite()
            || rect.x2 <= rect.x1
            || rect.y2 <= rect.y1
        {
            return Err(anyhow!("bubble {index} has an invalid bounding box"));
        }
        let requested_font_path = payload.font_path.as_ref().ok_or_else(|| {
            anyhow!("bubble {index} has no font_path; a real font is required for typesetting")
        })?;
        let (font_path, font_bytes) = select_font(
            requested_font_path,
            fallback_font_paths,
            &payload.text,
            index,
        )?;
        let face = Face::from_slice(&font_bytes, 0)
            .ok_or_else(|| anyhow!("unable to parse font {font_path}"))?;
        let font =
            fontdue::Font::from_bytes(font_bytes.as_slice(), fontdue::FontSettings::default())
                .map_err(|e| anyhow!("unable to parse raster font {font_path}: {e}"))?;
        let min = payload.min_font_size.unwrap_or(8.0);
        let max = payload.max_font_size.unwrap_or(72.0);
        let shape = payload.shape.as_deref().unwrap_or("ellipse");
        let layout = fit_text_with_geometry(
            &face,
            &font,
            &payload.text,
            rect,
            payload.bubble_bbox,
            payload.text_bbox,
            shape,
            min,
            max,
            payload.padding,
        )
        .with_context(|| format!("fit text for bubble {index}"))?;
        raster_layout(&mut image, &font, &layout)?;
        let ink_bbox = layout_ink_bbox(&layout);
        reports.push(json!({
            "index": index,
            "input_bbox": rect,
            "bubble_bbox": payload.bubble_bbox,
            "text_bbox": payload.text_bbox,
            "safe_bbox": layout.safe_bbox,
            "padding": layout.padding,
            "placement_center": layout.placement_center,
            "font_size": layout.font_size,
            "lines": layout.lines.iter().map(|line| line.text.clone()).collect::<Vec<_>>(),
            "line_count": layout.lines.len(),
            "ink_bbox": ink_bbox,
            "shape": shape,
            "requested_font_path": requested_font_path,
            "font_path": font_path,
            "font_fallback_used": font_path != *requested_font_path,
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

fn select_font(
    requested: &str,
    configured_fallbacks: &[String],
    text: &str,
    bubble_index: usize,
) -> Result<(String, Vec<u8>)> {
    let mut candidates = Vec::with_capacity(configured_fallbacks.len() + 6);
    candidates.push(requested.to_owned());
    candidates.extend(configured_fallbacks.iter().cloned());
    candidates.extend(
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
            candidates.push(directory.join(name).display().to_string());
        }
    }
    candidates.dedup();

    let mut requested_missing = Vec::new();
    for (candidate_index, candidate) in candidates.into_iter().enumerate() {
        let Ok(bytes) = fs::read(&candidate) else {
            if candidate_index == 0 {
                return Err(anyhow!("read font for bubble {bubble_index}: {candidate}"));
            }
            continue;
        };
        let Some(face) = Face::from_slice(&bytes, 0) else {
            if candidate_index == 0 {
                return Err(anyhow!("unable to parse font {candidate}"));
            }
            continue;
        };
        let missing = missing_characters(&face, text);
        if missing.is_empty() {
            return Ok((candidate, bytes));
        }
        if candidate_index == 0 {
            requested_missing = missing;
        }
    }
    let missing = requested_missing
        .into_iter()
        .map(|character| format!("U+{:04X}", u32::from(character)))
        .collect::<Vec<_>>()
        .join(", ");
    Err(anyhow!(
        "bubble {bubble_index} has no font covering required glyphs: {missing}; configure FUKIDASHI_FONT_PATH or FUKIDASHI_FONT_DIRS with a Unicode-capable TTF/OTF/TTC"
    ))
}

fn missing_characters(face: &Face<'_>, text: &str) -> Vec<char> {
    let mut missing = text
        .chars()
        .filter(|character| !character.is_control())
        .filter(|character| {
            face.glyph_index(*character)
                .is_none_or(|glyph| glyph.0 == 0)
        })
        .collect::<Vec<_>>();
    missing.sort_unstable();
    missing.dedup();
    missing
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
    font: &fontdue::Font,
    layout: &LayoutResult,
) -> Result<()> {
    let cx = layout.placement_center.0;
    let rgba = Rgba([0_u8, 0_u8, 0_u8, 255_u8]);
    for line in &layout.lines {
        let origin_x = cx - (line.ink_left + line.ink_right) * 0.5;
        for glyph in &line.glyphs {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_font_fallback_selects_patrick_hand_for_vietnamese() {
        let directory = tempfile::tempdir().unwrap();
        let requested = directory.path().join("ComicNeue-Regular.ttf");
        let fallback = directory.path().join("PatrickHand-Regular.ttf");
        fs::write(&requested, crate::fonts::COMIC_NEUE_REGULAR.bytes).unwrap();
        fs::write(&fallback, crate::fonts::PATRICK_HAND_REGULAR.bytes).unwrap();
        let fallback_paths = vec![fallback.display().to_string()];

        let (selected, bytes) = select_font(
            &requested.display().to_string(),
            &fallback_paths,
            "Tiếng Việt: ă â đ ê ô ơ ư",
            0,
        )
        .unwrap();

        assert_eq!(selected, fallback.display().to_string());
        assert_eq!(bytes, crate::fonts::PATRICK_HAND_REGULAR.bytes);
    }
}
