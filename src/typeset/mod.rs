//! Font shaped comic text layout and raster composition.

mod layout;

pub use layout::{
    FontCandidate, FontRun, LayoutResult, ShapedGlyph, ShapedLine, fit_text,
    fit_text_with_font_candidates, fit_text_with_geometry,
};

use anyhow::{Context, Result, anyhow};
use image::{DynamicImage, Rgba};
use serde_json::json;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::domain::TypesetPayload;
use crate::workflow::CleanArtifact;

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

    let mut reports = Vec::with_capacity(bubbles.len());
    for (index, payload) in bubbles.iter().enumerate() {
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
        let layout = fit_text_with_font_candidates(
            &candidates,
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
        raster_layout(&mut image, &candidates, &layout)?;
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
            "padding": layout.padding,
            "placement_center": layout.placement_center,
            "font_size": layout.font_size,
            "lines": layout.lines.iter().map(|line| line.text.clone()).collect::<Vec<_>>(),
            "line_count": layout.lines.len(),
            "ink_bbox": ink_bbox,
            "shape": shape,
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
) -> Result<()> {
    let cx = layout.placement_center.0;
    let rgba = Rgba([0_u8, 0_u8, 0_u8, 255_u8]);
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
    use crate::domain::Rect;

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
}
