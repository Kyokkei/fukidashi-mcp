//! Editor in-memory state mirroring `project.json` / `review.json`.
//!
//! The editor keeps the raw `serde_json::Value` produced by the MCP server (so
//! every field, including future ones, round-trips untouched) and layers typed
//! accessors on top. Render/gallery paths receive read-only *views*; all
//! mutations flow through index-based helpers on `EditorState` that edit the
//! underlying `Value` in place. Geometry mutations enforce the detector-anchor
//! invariants the renderer relies on.

#![allow(clippy::collapsible_if, clippy::ptr_arg, clippy::unnecessary_map_or)]
// Some helpers are scaffolding for future editor features (revision bumps,
// font overrides, sidecar paths); keep them available without noise.
#![allow(dead_code)]

use std::path::PathBuf;

use fukidashi_mcp::domain::Rect;
use fukidashi_mcp::editor::{ReviewState, read_review, resolved_review_file};

/// Correction brush stroke, matching the loopback editor's `project.json` shape
/// (`correction_strokes[]`).
#[derive(Debug, Clone)]
pub struct CorrectionStroke {
    pub mode: String,  // "cover" | "restore"
    pub color: String, // "#rrggbb"
    pub size: f32,     // diameter (= radius * 2)
    pub points: Vec<(f32, f32)>,
}

impl CorrectionStroke {
    pub fn to_value(&self) -> serde_json::Value {
        let points: Vec<serde_json::Value> = self
            .points
            .iter()
            .map(|(x, y)| serde_json::json!({"x": x, "y": y}))
            .collect();
        serde_json::json!({
            "mode": self.mode,
            "color": self.color,
            "size": self.size,
            "points": points,
        })
    }

    pub fn from_value(value: &serde_json::Value) -> Option<CorrectionStroke> {
        Some(CorrectionStroke {
            mode: value
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("cover")
                .to_owned(),
            color: value
                .get("color")
                .and_then(|v| v.as_str())
                .unwrap_or("#ffffff")
                .to_owned(),
            size: value
                .get("size")
                .and_then(|v| v.as_f64())
                .map(|v| v as f32)
                .unwrap_or(24.0),
            points: value
                .get("points")
                .and_then(|v| v.as_array())
                .map(|pts| {
                    pts.iter()
                        .filter_map(|p| {
                            let x = p.get("x")?.as_f64()?;
                            let y = p.get("y")?.as_f64()?;
                            Some((x as f32, y as f32))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// Read-only view of a bubble for rendering / inspector display.
#[derive(Clone)]
pub struct BubbleView {
    pub id: String,
    pub bbox: Option<Rect>,
    pub translation: String,
    pub text_color: Option<String>,
    pub font_size: Option<f32>,
    pub padding: Option<f32>,
    pub flagged: bool,
    pub preserve_source: bool,
    pub render_dirty: bool,
}

/// Read-only view of a page (with resolved, on-disk image paths).
pub struct PageView {
    pub id: String,
    pub source_image_path: PathBuf,
    pub cleaned_image_path: PathBuf,
    pub rendered_image_path: Option<PathBuf>,
    pub bubbles: Vec<BubbleView>,
    pub correction_strokes: Vec<CorrectionStroke>,
    pub render_dirty: bool,
}

impl PageView {
    pub fn has_flagged_or_dirty(&self) -> bool {
        self.bubbles.iter().any(|b| b.flagged || b.render_dirty)
    }
}

/// Whole-editor state. Wraps the server-produced `project.json` value plus the
/// job directory and the canonical source image path.
pub struct EditorState {
    pub value: serde_json::Value,
    pub job_dir: PathBuf,
    pub image_path: PathBuf,
}

impl EditorState {
    pub fn pages(&self) -> Vec<PageView> {
        self.value
            .get("pages")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(|page| self.page_view(page)).collect())
            .unwrap_or_default()
    }

    fn page_view(&self, page: &serde_json::Value) -> PageView {
        let source = self.resolve_path(
            page.get("image_path")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
        );
        let cleaned = self.resolve_path(
            page.get("cleaned_image_path")
                .or_else(|| page.get("cleaned_path"))
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
        );
        let rendered = page
            .get("rendered_image_path")
            .and_then(|v| v.as_str())
            .map(|p| self.resolve_path(p));
        let bubbles = page
            .get("bubbles")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|b| BubbleView {
                        id: b
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned(),
                        bbox: b
                            .get("bbox")
                            .and_then(|v| serde_json::from_value::<Rect>(v.clone()).ok()),
                        translation: b
                            .get("translation")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                        text_color: b
                            .get("text_color")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned),
                        font_size: b
                            .get("font_size")
                            .and_then(|v| v.as_f64())
                            .map(|v| v as f32),
                        padding: b.get("padding").and_then(|v| v.as_f64()).map(|v| v as f32),
                        flagged: b.get("flagged").and_then(|v| v.as_bool()).unwrap_or(false),
                        preserve_source: b
                            .get("preserve_source")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                        render_dirty: b
                            .get("render_dirty")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let correction_strokes = page
            .get("correction_strokes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(CorrectionStroke::from_value)
                    .collect()
            })
            .unwrap_or_default();
        PageView {
            id: page
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("page")
                .to_owned(),
            source_image_path: source,
            cleaned_image_path: cleaned,
            rendered_image_path: rendered,
            bubbles,
            correction_strokes,
            render_dirty: page
                .get("render_dirty")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }
    }

    pub fn revision(&self) -> u64 {
        self.value
            .get("state_revision")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    }

    pub fn bump_revision(&mut self) {
        let next = self.revision().saturating_add(1);
        if let Some(obj) = self.value.as_object_mut() {
            obj.insert("state_revision".to_owned(), serde_json::Value::from(next));
        }
    }

    pub fn font_path(&self) -> Option<String> {
        self.value
            .get("font_path")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    pub fn set_font_path(&mut self, path: Option<String>) {
        if let Some(obj) = self.value.as_object_mut() {
            match path {
                Some(p) if !p.trim().is_empty() => {
                    obj.insert("font_path".to_owned(), serde_json::Value::String(p));
                }
                _ => {
                    obj.insert("font_path".to_owned(), serde_json::Value::Null);
                }
            }
        }
    }

    pub fn page_count(&self) -> usize {
        self.value
            .get("pages")
            .and_then(|v| v.as_array())
            .map(Vec::len)
            .unwrap_or(0)
    }

    // ---- Mutations (operate on the underlying Value in place) ----

    fn mark_page_dirty(&mut self, page_index: usize) {
        if let Some(page) = self
            .value
            .get_mut("pages")
            .and_then(|v| v.as_array_mut())
            .and_then(|arr| arr.get_mut(page_index))
        {
            if let Some(obj) = page.as_object_mut() {
                obj.insert("render_dirty".to_owned(), serde_json::Value::Bool(true));
            }
        }
    }

    pub(crate) fn page_render_dirty(&self, page_index: usize) -> bool {
        self.value
            .get("pages")
            .and_then(|pages| pages.as_array())
            .and_then(|pages| pages.get(page_index))
            .and_then(|page| page.get("render_dirty"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    pub(crate) fn set_page_render_dirty(&mut self, page_index: usize, dirty: bool) {
        if let Some(page) = self
            .value
            .get_mut("pages")
            .and_then(|pages| pages.as_array_mut())
            .and_then(|pages| pages.get_mut(page_index))
            && let Some(object) = page.as_object_mut()
        {
            object.insert("render_dirty".to_owned(), serde_json::Value::Bool(dirty));
        }
    }

    fn bubble_mut<F: FnOnce(&mut serde_json::Value)>(
        &mut self,
        page_index: usize,
        bubble_index: usize,
        f: F,
    ) {
        if let Some(bubble) = self
            .value
            .get_mut("pages")
            .and_then(|v| v.as_array_mut())
            .and_then(|arr| arr.get_mut(page_index))
            .and_then(|p| p.get_mut("bubbles"))
            .and_then(|v| v.as_array_mut())
            .and_then(|arr| arr.get_mut(bubble_index))
        {
            f(bubble);
        }
        self.mark_page_dirty(page_index);
    }

    /// Move/resize a bubble. Enforces the invariant: `bbox` AND `bubble_bbox`
    /// become the new rect, and `text_bbox` is cleared so the renderer re-fits
    /// from the bubble's current geometry.
    pub fn set_bubble_bbox(&mut self, page_index: usize, bubble_index: usize, rect: Rect) {
        let rect_value = serde_json::json!({
            "x1": rect.x1, "y1": rect.y1, "x2": rect.x2, "y2": rect.y2
        });
        self.bubble_mut(page_index, bubble_index, |bubble| {
            if let Some(obj) = bubble.as_object_mut() {
                let old = obj
                    .get("bbox")
                    .and_then(|value| serde_json::from_value::<Rect>(value.clone()).ok());
                let text = obj
                    .get("text_bbox")
                    .and_then(|value| serde_json::from_value::<Rect>(value.clone()).ok());
                obj.insert("bbox".to_owned(), rect_value.clone());
                obj.insert("bubble_bbox".to_owned(), rect_value);
                // Preserve a fitted text anchor when possible, transforming
                // it with the operator geometry. A missing/invalid anchor is
                // intentionally left absent so the renderer can fit it.
                if let (Some(old), Some(text)) = (old, text) {
                    let sx = (rect.x2 - rect.x1) / (old.x2 - old.x1).max(0.001);
                    let sy = (rect.y2 - rect.y1) / (old.y2 - old.y1).max(0.001);
                    obj.insert(
                        "text_bbox".to_owned(),
                        serde_json::json!({
                            "x1": rect.x1 + (text.x1 - old.x1) * sx,
                            "y1": rect.y1 + (text.y1 - old.y1) * sy,
                            "x2": rect.x1 + (text.x2 - old.x1) * sx,
                            "y2": rect.y1 + (text.y2 - old.y1) * sy,
                        }),
                    );
                }
            }
        });
    }

    pub(crate) fn bubble_value(
        &self,
        page_index: usize,
        bubble_index: usize,
    ) -> Option<serde_json::Value> {
        self.value
            .get("pages")
            .and_then(|pages| pages.as_array())
            .and_then(|pages| pages.get(page_index))
            .and_then(|page| page.get("bubbles"))
            .and_then(|bubbles| bubbles.as_array())
            .and_then(|bubbles| bubbles.get(bubble_index))
            .cloned()
    }

    pub(crate) fn restore_bubble_value(
        &mut self,
        page_index: usize,
        bubble_index: usize,
        value: serde_json::Value,
    ) {
        if let Some(bubble) = self
            .value
            .get_mut("pages")
            .and_then(|pages| pages.as_array_mut())
            .and_then(|pages| pages.get_mut(page_index))
            .and_then(|page| page.get_mut("bubbles"))
            .and_then(|bubbles| bubbles.as_array_mut())
            .and_then(|bubbles| bubbles.get_mut(bubble_index))
        {
            *bubble = value;
        }
    }

    pub fn set_bubble_translation(&mut self, page_index: usize, bubble_index: usize, text: String) {
        self.bubble_mut(page_index, bubble_index, |bubble| {
            if let Some(obj) = bubble.as_object_mut() {
                obj.insert("translation".to_owned(), serde_json::Value::String(text));
            }
        });
    }

    pub fn set_bubble_text_color(
        &mut self,
        page_index: usize,
        bubble_index: usize,
        color: Option<String>,
    ) {
        self.bubble_mut(page_index, bubble_index, |bubble| {
            if let Some(obj) = bubble.as_object_mut() {
                match color {
                    Some(c) => obj.insert("text_color".to_owned(), serde_json::Value::String(c)),
                    None => obj.insert("text_color".to_owned(), serde_json::Value::Null),
                };
            }
        });
    }

    pub fn set_bubble_font_size(
        &mut self,
        page_index: usize,
        bubble_index: usize,
        size: Option<f32>,
    ) {
        self.bubble_mut(page_index, bubble_index, |bubble| {
            if let Some(obj) = bubble.as_object_mut() {
                match size {
                    Some(s) => obj.insert("font_size".to_owned(), serde_json::json!(s)),
                    None => obj.insert("font_size".to_owned(), serde_json::Value::Null),
                };
            }
        });
    }

    pub fn set_bubble_padding(&mut self, page_index: usize, bubble_index: usize, padding: f32) {
        self.bubble_mut(page_index, bubble_index, |bubble| {
            if let Some(obj) = bubble.as_object_mut() {
                obj.insert("padding".to_owned(), serde_json::json!(padding));
            }
        });
    }

    pub fn set_bubble_flagged(&mut self, page_index: usize, bubble_index: usize, flagged: bool) {
        let was_dirty = self
            .value
            .get("pages")
            .and_then(|pages| pages.as_array())
            .and_then(|pages| pages.get(page_index))
            .and_then(|page| page.get("render_dirty"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        self.bubble_mut(page_index, bubble_index, |bubble| {
            if let Some(obj) = bubble.as_object_mut() {
                obj.insert("flagged".to_owned(), serde_json::Value::Bool(flagged));
            }
        });
        // Flagging does not dirty the render (it is advisory), so reverse the
        // dirty mark the generic helper applied.
        if let Some(page) = self
            .value
            .get_mut("pages")
            .and_then(|v| v.as_array_mut())
            .and_then(|arr| arr.get_mut(page_index))
        {
            if let Some(obj) = page.as_object_mut() {
                obj.insert(
                    "render_dirty".to_owned(),
                    serde_json::Value::Bool(was_dirty),
                );
            }
        }
    }

    pub fn set_bubble_preserve_source(
        &mut self,
        page_index: usize,
        bubble_index: usize,
        preserve: bool,
    ) {
        self.bubble_mut(page_index, bubble_index, |bubble| {
            if let Some(obj) = bubble.as_object_mut() {
                obj.insert(
                    "preserve_source".to_owned(),
                    serde_json::Value::Bool(preserve),
                );
            }
        });
    }

    pub fn push_stroke(&mut self, page_index: usize, stroke: &CorrectionStroke) {
        if let Some(arr) = self
            .value
            .get_mut("pages")
            .and_then(|v| v.as_array_mut())
            .and_then(|arr| arr.get_mut(page_index))
            .and_then(|p| p.get_mut("correction_strokes"))
            .and_then(|v| v.as_array_mut())
        {
            arr.push(stroke.to_value());
        } else if let Some(obj) = self
            .value
            .get_mut("pages")
            .and_then(|v| v.as_array_mut())
            .and_then(|arr| arr.get_mut(page_index))
        {
            obj.as_object_mut().map(|o| {
                o.insert(
                    "correction_strokes".to_owned(),
                    serde_json::Value::Array(vec![stroke.to_value()]),
                )
            });
        }
        self.mark_page_dirty(page_index);
    }

    pub fn pop_stroke(&mut self, page_index: usize) -> Option<CorrectionStroke> {
        let popped = self
            .value
            .get_mut("pages")
            .and_then(|v| v.as_array_mut())
            .and_then(|arr| arr.get_mut(page_index))
            .and_then(|p| p.get_mut("correction_strokes"))
            .and_then(|v| v.as_array_mut())
            .and_then(|arr| arr.pop())
            .as_ref()
            .and_then(CorrectionStroke::from_value);
        if popped.is_some() {
            self.mark_page_dirty(page_index);
        }
        popped
    }

    pub fn cleared(&self, page_index: usize) -> bool {
        self.value
            .get("pages")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.get(page_index))
            .and_then(|p| p.get("correction_strokes"))
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.is_empty())
    }

    fn resolve_path(&self, raw: &str) -> PathBuf {
        let candidate = if std::path::Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.job_dir.join(raw)
        };
        if candidate.exists() {
            candidate
        } else {
            self.job_dir.join(raw)
        }
    }
}

/// Load the persisted `ReviewState`, minting a fresh awaiting_review session when
/// none exists or when no matching `--review-session-id` was supplied.
pub fn load_or_create_review(job_dir: &PathBuf, requested_session: &Option<String>) -> ReviewState {
    if let Some(existing) = read_review(job_dir) {
        if requested_session
            .as_ref()
            .map_or(true, |id| &existing.review_session_id == id)
        {
            return existing;
        }
    }
    ReviewState {
        review_session_id: requested_session
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()),
        revision: 0,
        status: "awaiting_review".to_owned(),
        action: None,
        feedback: Vec::new(),
        approved_pages: Vec::new(),
        consumed: false,
        audit: Vec::new(),
    }
}

/// Persist the project state with an atomic write (mirrors the loopback server).
pub fn save_project(state: &EditorState) -> anyhow::Result<()> {
    let path = state.job_dir.join("project.json");
    atomic_write_json(&path, &state.value)
}

/// Persist the review state (mirrors `save_review` on the MCP side).
pub fn save_review_state(job_dir: &PathBuf, review: &ReviewState) -> anyhow::Result<()> {
    fukidashi_mcp::editor::write_review(job_dir, review)
}

/// Resolve the `review.json` path for the job.
pub fn review_path(job_dir: &PathBuf) -> PathBuf {
    resolved_review_file(job_dir)
}

/// Atomic JSON write: `<path>.tmp` then `rename`. Same pattern as the loopback
/// server's `atomic_json_save`.
pub fn atomic_write_json(path: &std::path::Path, value: &serde_json::Value) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("state path has no parent"))?;
    let temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| anyhow::anyhow!("create temporary editor state: {e}"))?;
    serde_json::to_writer_pretty(temp.as_file(), value)
        .map_err(|e| anyhow::anyhow!("write editor state: {e}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|e| anyhow::anyhow!("flush editor state: {e}"))?;
    temp.persist(path)
        .map_err(|e| anyhow::anyhow!("promote editor state: {}", e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_updates_transform_text_anchor_and_keep_operator_aliases() {
        let mut state = EditorState {
            value: serde_json::json!({
                "pages": [{"bubbles": [{
                    "bbox": {"x1": 10.0, "y1": 20.0, "x2": 30.0, "y2": 40.0},
                    "bubble_bbox": {"x1": 10.0, "y1": 20.0, "x2": 30.0, "y2": 40.0},
                    "text_bbox": {"x1": 15.0, "y1": 25.0, "x2": 25.0, "y2": 35.0}
                }]}]
            }),
            job_dir: PathBuf::from("."),
            image_path: PathBuf::from("image.png"),
        };
        state.set_bubble_bbox(
            0,
            0,
            Rect {
                x1: 20.0,
                y1: 30.0,
                x2: 60.0,
                y2: 70.0,
            },
        );
        let bubble = state.bubble_value(0, 0).unwrap();
        assert_eq!(bubble["bbox"], bubble["bubble_bbox"]);
        assert_eq!(bubble["text_bbox"]["x1"], 30.0);
        assert_eq!(bubble["text_bbox"]["y2"], 60.0);
    }
}
