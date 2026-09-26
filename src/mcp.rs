use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::Context;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use rmcp::{
    ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::{
    config::{Config, ConfigureRequest},
    domain::{Rect, TypesetPayload},
    error::FukidashiError,
    ingress::{PullChapterRequest, SearchMangaRequest},
    workflow::{
        PendingPage, ScopeSpec, Workflow, emit_page_progress, format_page_progress,
        page_progress_json,
    },
};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AnalyzeRequest {
    pub image_path: String,
    pub ocr_mode: Option<String>,
    #[schemars(schema_with = "source_language_schema")]
    pub source_language: Option<String>,
    pub target_language: Option<String>,
    /// Optional vision-model corrections keyed by the stable OCR region id.
    #[serde(default)]
    pub corrected_source_text: Option<HashMap<String, String>>,
    /// `compact` avoids repeating OCR geometry in long agent conversations.
    /// Omitted keeps the original `full` response contract.
    #[serde(default)]
    pub response_detail: Option<String>,
    /// Optional absolute JSON path for the complete analysis. Written atomically.
    #[serde(default)]
    pub checkpoint_path: Option<String>,
    /// Optional first-call inventory scope. Ranges are one-based and follow
    /// natural filename ordering; include_paths supports a non-contiguous
    /// bounded fixture.
    #[serde(default)]
    #[schemars(
        description = "Object with start_page/end_page or include_paths. A JSON-encoded object string is accepted for clients that incorrectly stringify nested arguments."
    )]
    pub scope: Option<AnalyzeScope>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AnalyzeScope {
    #[serde(default)]
    pub start_page: Option<usize>,
    #[serde(default)]
    pub end_page: Option<usize>,
    #[serde(default)]
    pub include_paths: Option<Vec<String>>,
}

impl<'de> Deserialize<'de> for AnalyzeScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            start_page: Option<usize>,
            #[serde(default)]
            end_page: Option<usize>,
            #[serde(default)]
            include_paths: Option<Vec<String>>,
        }

        let value = serde_json::Value::deserialize(deserializer)?;
        let wire: Wire = match value {
            serde_json::Value::Object(_) => {
                serde_json::from_value(value).map_err(serde::de::Error::custom)?
            }
            serde_json::Value::String(encoded) => {
                serde_json::from_str(&encoded).map_err(|error| {
                    serde::de::Error::custom(format!(
                        "scope must be an object or a JSON-encoded object: {error}"
                    ))
                })?
            }
            other => {
                return Err(serde::de::Error::custom(format!(
                    "scope must be an object or a JSON-encoded object, got {}",
                    other
                )));
            }
        };
        Ok(Self {
            start_page: wire.start_page,
            end_page: wire.end_page,
            include_paths: wire.include_paths,
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CleanRequest {
    pub image_path: String,
    pub mask_path: Option<String>,
    pub dilation: Option<u8>,
    /// `full` preserves the original behavior; `crop` requires supplied geometry/checkpoint.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub analysis_path: Option<String>,
    #[serde(default)]
    pub text_regions: Vec<Rect>,
    #[serde(default)]
    pub crop_padding: Option<u32>,
    #[serde(default)]
    pub crop_minimum_size: Option<u32>,
    /// Strict-v1 policy for text detected outside dialogue bubbles. Preserve
    /// is the safe default; replace must be explicitly requested.
    #[serde(default)]
    pub sfx_mode: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TypesetRequest {
    pub image_path: String,
    pub bubbles: Vec<TypesetPayload>,
    /// Defaults used only when the corresponding bubble field is absent.
    /// Use the bundled Comic Neue or Patrick Hand faces for comic text;
    /// installed/configured CJK faces provide Chinese, Korean, and Japanese
    /// glyph coverage when available. Generic Windows UI faces such as Arial,
    /// Calibri, and Segoe UI are substituted when supplied as a primary.
    #[serde(default)]
    pub font_path: Option<String>,
    #[serde(default)]
    pub padding: Option<f32>,
    #[serde(default)]
    pub min_font_size: Option<f32>,
    #[serde(default)]
    pub max_font_size: Option<f32>,
    #[serde(default)]
    pub shape: Option<String>,
    /// Ordered per-grapheme fallbacks used when the primary face lacks a
    /// complete grapheme cluster.
    #[serde(default)]
    pub fallback_font_paths: Vec<String>,
}
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EditorRequest {
    /// A verified rendered artifact. Optional when `job_path` or `job_id` is
    /// supplied; the server then selects the first verified render.
    #[serde(default)]
    pub image_path: Option<String>,
    /// Direct path to a managed job directory or its `job.json` manifest
    /// under the server jobs root. Existing legacy marker files are accepted.
    #[serde(default)]
    pub job_path: Option<String>,
    /// Managed job directory name under the server jobs root.
    #[serde(default)]
    pub job_id: Option<String>,
    /// Optional metadata only. Pages and bubbles are reconstructed from the
    /// managed manifest and render sidecars.
    #[serde(default)]
    #[schemars(schema_with = "json_object_schema")]
    pub json_data: Option<serde_json::Value>,
    /// Reopen a completed review in a new native/loopback review cycle.
    /// Defaults to true for direct editor requests; the combined review/export
    /// flow disables this to preserve its already-approved fast path.
    #[schemars(schema_with = "reopen_completed_schema")]
    pub reopen_completed: bool,
}

impl<'de> Deserialize<'de> for EditorRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            image_path: Option<String>,
            #[serde(default)]
            job_path: Option<String>,
            #[serde(default)]
            job_id: Option<String>,
            #[serde(default)]
            json_data: Option<serde_json::Value>,
            #[serde(default)]
            reopen_completed: Option<bool>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            image_path: wire.image_path,
            job_path: wire.job_path,
            job_id: wire.job_id,
            json_data: wire.json_data,
            reopen_completed: wire
                .reopen_completed
                .unwrap_or_else(default_reopen_completed),
        })
    }
}

fn json_object_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type": "object"})
}

fn source_language_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": ["auto", "ja", "zh", "ko", "en", "latin"],
        "description": "OCR source route. Use auto for mixed pages; pipe-delimited values such as en|latin are legacy aliases normalized to auto."
    })
}

fn reopen_completed_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "boolean",
        "description": "Whether a completed review should be reopened as a new review cycle"
    })
}

fn lore_object_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "description": "Canonical lore. Character entries may be names (for example \"Fuyu\") or objects; name strings are canonicalized to stable {id,names,notes} entries.",
        "additionalProperties": {},
        "properties": {
            "schema": {"type": "integer", "minimum": 1, "default": 1},
            "characters": {
                "type": "array", "maxItems": 4096,
                "items": {"anyOf": [
                    {"type": "string", "minLength": 1},
                    {"type": "object", "required": ["id", "names"], "additionalProperties": {},
                        "properties": {
                            "id": {"type": "string", "minLength": 1, "maxLength": 256},
                            "names": {"type": "array", "minItems": 1, "maxItems": 64, "items": {"type": "string", "minLength": 1, "maxLength": 512}},
                            "notes": {"type": "string", "maxLength": 4096}
                        }
                    }
                ]}
            },
            "pronouns": {"type": "array", "maxItems": 4096, "items": {"type": "object", "required": ["speaker", "addressee", "pair"], "additionalProperties": {},
                "properties": {
                    "speaker": {"type": "string", "minLength": 1, "maxLength": 256},
                    "addressee": {"type": "string", "minLength": 1, "maxLength": 256},
                    "pair": {"type": "string", "minLength": 1, "maxLength": 512}
                }
            }},
            "glossary": {"type": "array", "maxItems": 16384, "items": {"type": "object", "required": ["source", "target"], "additionalProperties": {},
                "properties": {
                    "source": {"type": "string", "minLength": 1, "maxLength": 1024},
                    "target": {"type": "string", "minLength": 1, "maxLength": 1024}
                }
            }}
        },
        "examples": [{"schema": 1, "characters": ["Fuyu", "Kuga", "Sosuke"], "pronouns": [], "glossary": [{"source": "proprietress", "target": "bà chủ"}] }]
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExportRequest {
    pub project_dir: String,
    pub format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WaitReviewRequest {
    pub review_session_id: String,
    pub revision: u64,
    /// Maximum time to wait for the browser to submit fixes or approval.
    #[serde(default = "default_review_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReviewAndExportRequest {
    /// Select a managed job by its server-returned job id or exact job path.
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub job_path: Option<String>,
    /// Compatibility selector for a known rendered artifact.
    #[serde(default)]
    pub image_path: Option<String>,
    /// Export format after explicit browser approval. Defaults to zip.
    #[serde(default = "default_export_format")]
    pub format: String,
    /// Maximum time to keep this combined call pending for the browser.
    #[serde(default = "default_review_timeout")]
    pub timeout_seconds: u64,
}

/// Start or resume the server-owned translation loop.  A new loop may be
/// started from one source image; a resumed loop is selected by the job id or
/// managed job path returned by an earlier call.  The submit call deliberately
/// has no path fields and uses only the opaque token returned here.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct TranslationStartRequest {
    #[serde(default)]
    pub image_path: Option<String>,
    #[serde(default)]
    pub job_path: Option<String>,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub ocr_mode: Option<String>,
    #[serde(default)]
    #[schemars(schema_with = "source_language_schema")]
    pub source_language: Option<String>,
    #[serde(default)]
    pub target_language: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Object with start_page/end_page or include_paths. A JSON-encoded object string is accepted for clients that incorrectly stringify nested arguments."
    )]
    pub scope: Option<AnalyzeScope>,
    /// `preserve` keeps structurally unmatched text out of clean/typeset;
    /// `replace` opts into translating and replacing it. Defaults to preserve.
    #[serde(default)]
    pub sfx_mode: Option<String>,
    /// Cover/title pages are preserved by default. Set true to translate a
    /// cover explicitly after reviewing its detected regions.
    #[serde(default)]
    pub translate_cover: bool,
}

/// Analyze an entire managed page inventory once and persist a compact routing
/// cache.  Translation and rendering remain page-serial; this call removes
/// the repeated start/analyze round trip and keeps one hot OCR session for the
/// bounded preflight job.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct TranslationPreflightRequest {
    #[serde(default)]
    pub image_path: Option<String>,
    #[serde(default)]
    pub job_path: Option<String>,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub ocr_mode: Option<String>,
    #[serde(default)]
    #[schemars(schema_with = "source_language_schema")]
    pub source_language: Option<String>,
    #[serde(default)]
    pub target_language: Option<String>,
    #[serde(default)]
    pub scope: Option<AnalyzeScope>,
    #[serde(default)]
    pub sfx_mode: Option<String>,
    /// Cover/title pages are preserved by default. Set true to route page 1
    /// through OCR and translation explicitly.
    #[serde(default)]
    pub translate_cover: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LoreRequest {
    /// Exact managed job directory name returned by the translation flow.
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PutLoreRequest {
    /// Exact managed job directory name returned by the translation flow.
    pub job_id: String,
    /// Forward-compatible lore object. The server validates known fields and
    /// retains unknown top-level fields for newer clients.
    #[schemars(schema_with = "lore_object_schema")]
    pub lore: serde_json::Value,
}

/// One translation decision keyed by the stable OCR item id.  `keep_source`
/// is the explicit uncertainty-preserving choice; it is never inferred from
/// low OCR confidence.  `needs_review` carries uncertainty through a render
/// so a weak OCR result cannot make a client abandon the page.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TranslationSubmission {
    #[schemars(
        description = "Stable ID from translation_items.required_translation_ids. Submit exactly one decision for every required ID; preserved_items are already explicit source-preservation decisions and are not submitted."
    )]
    pub id: String,
    /// The translated text, preserved byte-for-byte apart from JSON decoding.
    /// `text` is accepted as a compatibility alias for simple clients.
    #[serde(default, alias = "text")]
    pub translation: Option<String>,
    #[serde(default)]
    pub keep_source: Option<bool>,
    #[serde(default)]
    pub needs_review: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TranslationSubmitRequest {
    pub work_token: String,
    pub translations: Vec<TranslationSubmission>,
}

/// One exact reviewed bubble that the user marked for a fresh translation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RetranslationSubmitRequest {
    /// Exact managed job ID returned by the translation workflow.
    pub job_id: String,
    /// Zero-based page index from review feedback.
    pub page: usize,
    /// Existing bubble selected for retranslation. Omit for a flagged missing region.
    #[serde(default)]
    pub bubble_id: Option<String>,
    /// Required with a missing-dialogue flag; copied exactly from its feedback.
    #[serde(default, alias = "region", alias = "bounds", alias = "rect")]
    pub bbox: Option<serde_json::Value>,
    /// Coordinate fallback for clients that flatten the bbox object.
    #[serde(default)]
    pub x1: Option<f32>,
    #[serde(default)]
    pub y1: Option<f32>,
    #[serde(default)]
    pub x2: Option<f32>,
    #[serde(default)]
    pub y2: Option<f32>,
    /// OCR returned by fukidashi_retranslation_source for a missing region.
    #[serde(default)]
    pub source_ocr: Option<String>,
    /// Agent-corrected source text when OCR was empty or inaccurate.
    #[serde(default)]
    pub source_text: Option<String>,
    /// Value returned as current_translation in the retranslate feedback.
    #[serde(default)]
    pub expected_current_translation: Option<String>,
    /// Optional editor state revision observed with the feedback.
    #[serde(default)]
    pub state_revision: Option<u64>,
    /// Format requested for the eventual reviewed export. Defaults to zip.
    #[serde(default)]
    pub format: Option<String>,
    /// Fresh translation supplied by the calling agent.
    pub translation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RetranslationSourceRequest {
    pub job_id: String,
    /// Zero-based page index from review feedback.
    #[serde(alias = "page_index")]
    pub page: usize,
    /// Region in source pixels as {x1,y1,x2,y2}; accepts an array or flat coordinates too.
    #[serde(default, alias = "region", alias = "bounds", alias = "rect")]
    pub bbox: Option<serde_json::Value>,
    /// Coordinate fallback for clients that flatten the bbox object.
    #[serde(default)]
    pub x1: Option<f32>,
    #[serde(default)]
    pub y1: Option<f32>,
    #[serde(default)]
    pub x2: Option<f32>,
    #[serde(default)]
    pub y2: Option<f32>,
}

fn parse_retranslation_bbox(
    bbox: Option<&serde_json::Value>,
    x1: Option<f32>,
    y1: Option<f32>,
    x2: Option<f32>,
    y2: Option<f32>,
) -> anyhow::Result<Rect> {
    let rect = if let Some(value) = bbox {
        let value = if let Some(encoded) = value.as_str() {
            serde_json::from_str::<serde_json::Value>(encoded)
                .map_err(|error| anyhow::anyhow!("bbox string is not JSON: {error}"))?
        } else {
            value.clone()
        };
        match &value {
            serde_json::Value::Array(values) if values.len() == 4 => Rect {
                x1: values[0]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("bbox[0] must be numeric"))?
                    as f32,
                y1: values[1]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("bbox[1] must be numeric"))?
                    as f32,
                x2: values[2]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("bbox[2] must be numeric"))?
                    as f32,
                y2: values[3]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("bbox[3] must be numeric"))?
                    as f32,
            },
            serde_json::Value::Object(object) => {
                if let Ok(rect) = serde_json::from_value::<Rect>(value.clone()) {
                    rect
                } else if let (Some(x), Some(y), Some(width), Some(height)) = (
                    object.get("x").and_then(serde_json::Value::as_f64),
                    object.get("y").and_then(serde_json::Value::as_f64),
                    object
                        .get("width")
                        .or_else(|| object.get("w"))
                        .and_then(serde_json::Value::as_f64),
                    object
                        .get("height")
                        .or_else(|| object.get("h"))
                        .and_then(serde_json::Value::as_f64),
                ) {
                    Rect {
                        x1: x as f32,
                        y1: y as f32,
                        x2: (x + width) as f32,
                        y2: (y + height) as f32,
                    }
                } else {
                    anyhow::bail!(
                        "bbox must be {{x1,y1,x2,y2}}, [x1,y1,x2,y2], or {{x,y,width,height}}"
                    )
                }
            }
            _ => anyhow::bail!(
                "bbox must be {{x1,y1,x2,y2}}, [x1,y1,x2,y2], or {{x,y,width,height}}"
            ),
        }
    } else {
        Rect {
            x1: x1.ok_or_else(|| anyhow::anyhow!("bbox missing; supply bbox or x1,y1,x2,y2"))?,
            y1: y1.ok_or_else(|| anyhow::anyhow!("bbox missing; supply bbox or x1,y1,x2,y2"))?,
            x2: x2.ok_or_else(|| anyhow::anyhow!("bbox missing; supply bbox or x1,y1,x2,y2"))?,
            y2: y2.ok_or_else(|| anyhow::anyhow!("bbox missing; supply bbox or x1,y1,x2,y2"))?,
        }
    };
    rect.validate()
        .map_err(|error| anyhow::anyhow!("invalid bbox: {error}"))
}

fn rects_close(left: Rect, right: Rect) -> bool {
    const EPSILON: f32 = 0.001;
    (left.x1 - right.x1).abs() <= EPSILON
        && (left.y1 - right.y1).abs() <= EPSILON
        && (left.x2 - right.x2).abs() <= EPSILON
        && (left.y2 - right.y2).abs() <= EPSILON
}

fn best_overlapping_empty_bubble(page: &serde_json::Value, region: Rect) -> Option<String> {
    let region_area = (region.x2 - region.x1) * (region.y2 - region.y1);
    if region_area <= 0.0 {
        return None;
    }
    page.get("bubbles")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .filter_map(|bubble| {
            let translation = bubble
                .get("translation")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if !translation.trim().is_empty() {
                return None;
            }
            let rect = bubble
                .get("bbox")
                .or_else(|| bubble.get("bubble_bbox"))
                .cloned()
                .and_then(|value| serde_json::from_value::<Rect>(value).ok())?
                .validate()
                .ok()?;
            let intersection = (region.x2.min(rect.x2) - region.x1.max(rect.x1)).max(0.0)
                * (region.y2.min(rect.y2) - region.y1.max(rect.y1)).max(0.0);
            let region_coverage = intersection / region_area;
            (region_coverage >= 0.25)
                .then(|| Some((region_coverage, bubble.get("id")?.as_str()?.to_owned())))
                .flatten()
        })
        .max_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, id)| id)
}

fn best_overlapping_orphaned_handoff_item(
    workflow: &Workflow,
    source: &Path,
    page: &serde_json::Value,
    region: Rect,
) -> anyhow::Result<Option<String>> {
    let region_area = (region.x2 - region.x1) * (region.y2 - region.y1);
    if region_area <= 0.0 {
        return Ok(None);
    }
    let (analysis_path, _, _, _, _) = workflow.page_artifacts_for_source(source)?;
    if !analysis_path.is_file() {
        return Ok(None);
    }
    let analysis: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&analysis_path).context("read analysis for missing-dialogue replacement")?,
    )
    .context("parse analysis for missing-dialogue replacement")?;
    let bubbles = page
        .get("bubbles")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten();
    let active_ids = bubbles
        .clone()
        .filter_map(|bubble| bubble.get("id").and_then(serde_json::Value::as_str))
        .collect::<BTreeSet<_>>();
    let removed_ids = page
        .get("removed_bubbles")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|removed| {
            removed
                .as_str()
                .or_else(|| removed.get("id").and_then(serde_json::Value::as_str))
        })
        .collect::<BTreeSet<_>>();
    let items = analysis
        .get("translation_handoff")
        .and_then(|handoff| handoff.get("items"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten();
    Ok(items
        .filter_map(|item| {
            let id = item.get("id").and_then(serde_json::Value::as_str)?;
            if active_ids.contains(id) || removed_ids.contains(id) {
                return None;
            }
            let bbox = item
                .get("bbox")
                .or_else(|| item.get("bubble_bbox"))
                .cloned()
                .and_then(|value| serde_json::from_value::<Rect>(value).ok())?
                .validate()
                .ok()?;
            let intersection = (region.x2.min(bbox.x2) - region.x1.max(bbox.x1)).max(0.0)
                * (region.y2.min(bbox.y2) - region.y1.max(bbox.y1)).max(0.0);
            let region_coverage = intersection / region_area;
            (region_coverage >= 0.25).then(|| (region_coverage, id.to_owned()))
        })
        .max_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, id)| id))
}

struct ManagedFileSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
}

fn snapshot_managed_files(
    workflow: &Workflow,
    job: &Path,
    paths: impl IntoIterator<Item = PathBuf>,
) -> anyhow::Result<Vec<ManagedFileSnapshot>> {
    let mut snapshots = Vec::new();
    for path in paths {
        let (path, contents) = if path.exists() {
            let path = workflow.require_owned(&path, "retranslation transaction artifact")?;
            let contents = std::fs::read(&path)?;
            (path, Some(contents))
        } else {
            let mut existing_parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("transaction artifact has no parent"))?;
            while !existing_parent.exists() {
                existing_parent = existing_parent.parent().ok_or_else(|| {
                    anyhow::anyhow!("transaction artifact has no existing parent")
                })?;
            }
            let canonical_parent =
                workflow.require_owned(existing_parent, "retranslation artifact directory")?;
            if !canonical_parent.starts_with(job) {
                anyhow::bail!("retranslation transaction artifact escaped the managed job");
            }
            let suffix = path
                .strip_prefix(existing_parent)
                .map_err(|_| anyhow::anyhow!("transaction artifact escaped its existing parent"))?;
            let path = canonical_parent.join(suffix);
            if !path.starts_with(job) {
                anyhow::bail!("retranslation transaction artifact escaped the managed job");
            }
            (path, None)
        };
        if snapshots
            .iter()
            .any(|snapshot: &ManagedFileSnapshot| snapshot.path == path)
        {
            continue;
        }
        snapshots.push(ManagedFileSnapshot { path, contents });
    }
    Ok(snapshots)
}

fn restore_managed_files(snapshots: &[ManagedFileSnapshot]) -> anyhow::Result<()> {
    for snapshot in snapshots {
        match snapshot.contents.as_deref() {
            Some(contents) => {
                let parent = snapshot
                    .path
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("transaction artifact has no parent"))?;
                let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
                std::io::Write::write_all(temporary.as_file_mut(), contents)?;
                temporary.as_file().sync_all()?;
                temporary.persist(&snapshot.path).map_err(|error| {
                    anyhow::anyhow!("restore {}: {}", snapshot.path.display(), error.error)
                })?;
            }
            None => match std::fs::remove_file(&snapshot.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
        }
    }
    Ok(())
}

fn default_review_timeout() -> u64 {
    3600
}

fn default_export_format() -> String {
    "zip".to_owned()
}

fn default_reopen_completed() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ReleaseModelsRequest {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct GetConfigRequest {}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FukidashiServer {
    pub config: Config,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    /// One heavy local operation at a time keeps CPU/GPU memory bounded.
    slots: Arc<Semaphore>,
    ocr: Arc<std::sync::Mutex<crate::vision::ocr::OcrEngine>>,
    workflow: Arc<Workflow>,
    /// In-process capability claims for the two-call strict translation API.
    /// Tokens are intentionally ephemeral; a restarted server requires a new
    /// start/resume call and never trusts a token supplied by a stale client.
    translation_claims: Arc<std::sync::Mutex<HashMap<String, TranslationClaim>>>,
}

#[derive(Debug, Clone)]
struct TranslationClaim {
    job_dir: PathBuf,
    source_image: PathBuf,
    page_number: usize,
    total_pages: usize,
    analysis_path: PathBuf,
    analysis_sha256: String,
    item_ids: BTreeSet<String>,
    source_language: Option<String>,
    target_language: Option<String>,
    replace_sfx: bool,
    in_progress: bool,
    consumed: bool,
}

impl FukidashiServer {
    pub fn new(config: Config) -> Result<Self, FukidashiError> {
        config.ensure_runtime_dirs().map_err(|error| {
            FukidashiError::RuntimeUnavailable(format!(
                "configured runtime directories are not usable: {error}"
            ))
        })?;
        let workflow = Workflow::new(config.jobs_dir()).map_err(|error| {
            FukidashiError::RuntimeUnavailable(format!(
                "server-owned jobs directory is not usable: {error}"
            ))
        })?;
        Ok(Self {
            config,
            tool_router: Self::tool_router(),
            slots: Arc::new(Semaphore::new(1)),
            ocr: Arc::new(std::sync::Mutex::new(
                crate::vision::ocr::OcrEngine::default(),
            )),
            workflow: Arc::new(workflow),
            translation_claims: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }
}

fn path(s: &str) -> Result<PathBuf, FukidashiError> {
    let p = PathBuf::from(s);
    if !p.is_absolute() {
        return Err(FukidashiError::InvalidInput(
            "paths must be absolute".into(),
        ));
    }
    Ok(p)
}
fn json_result<T: Serialize>(value: &T, is_error: bool) -> CallToolResult {
    let text = serde_json::to_string(value).unwrap_or_else(|e| format!("{{\"error\":{e:?}}}"));
    if is_error {
        CallToolResult::error(vec![ContentBlock::text(text)])
    } else {
        CallToolResult::success(vec![ContentBlock::text(text)])
    }
}

fn add_error_diagnostic(value: &mut serde_json::Value, error: &FukidashiError) {
    let FukidashiError::Diagnostic { details, .. } = error else {
        return;
    };
    value["diagnostic"] = details.clone();
    for key in ["stage", "code", "next_step"] {
        if let Some(detail) = details.get(key) {
            value[key] = detail.clone();
        }
    }
}

fn error_json(error: &FukidashiError) -> serde_json::Value {
    let mut value = serde_json::json!({"error": error.to_string()});
    add_error_diagnostic(&mut value, error);
    value
}

fn attach_page_progress(
    value: &mut serde_json::Value,
    current_page: usize,
    total_pages: usize,
    stage: &str,
) {
    value["current_page"] = serde_json::json!(current_page);
    value["total_pages"] = serde_json::json!(total_pages);
    value["progress"] = page_progress_json(current_page, total_pages, stage);
}

fn launch_default_browser(url: &str) -> Result<(), FukidashiError> {
    #[cfg(windows)]
    {
        Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .creation_flags(0x0800_0000)
            .spawn()
            .map(|_| ())
            .map_err(|error| {
                FukidashiError::RuntimeUnavailable(format!(
                    "unable to open the default browser: {error}"
                ))
            })
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|error| {
                FukidashiError::RuntimeUnavailable(format!(
                    "unable to open the default browser: {error}"
                ))
            })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|error| {
                FukidashiError::RuntimeUnavailable(format!(
                    "unable to open the default browser: {error}"
                ))
            })
    }
}

fn save_json_atomic(
    path: &std::path::Path,
    value: &serde_json::Value,
) -> Result<(), FukidashiError> {
    let parent = path.parent().ok_or_else(|| {
        FukidashiError::InvalidInput("checkpoint_path must have a parent directory".into())
    })?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut file, value)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map(|_| ())
        .map_err(|error| error.error.into())
}

fn compact_analysis(
    analysis: &crate::vision::ocr::PageAnalysis,
    image_path: &std::path::Path,
    checkpoint_path: Option<&std::path::Path>,
    recycled: bool,
) -> serde_json::Value {
    let items = analysis
        .translation_handoff
        .items
        .iter()
        .map(|item| {
            serde_json::json!({
                "id": item.id,
                "kind": item.kind,
                "preserve_by_default": item.preserve_by_default,
                "source_text": item.source_text,
                "source_language": item.source_language,
                "confidence": item.confidence,
                "bbox": item.bbox,
                "correction_applied": item.correction_applied,
                "status": item.status,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "response_detail": "compact",
        "image_path": image_path,
        "source_language": analysis.source_language,
        "target_language": analysis.target_language,
        "bubble_count": analysis.bubbles.len(),
        "text_line_count": analysis.text_lines.len(),
        "unmatched_text_count": analysis.unmatched_text.len(),
        "translation_items": items,
        "checkpoint_path": checkpoint_path,
        "sessions_recycled": recycled,
        "next_step": "translate translation_items, then call fukidashi_clean_page and fukidashi_typeset",
    })
}

fn preflight_page_kind(analysis: &serde_json::Value) -> &'static str {
    if let Some(kind) = analysis
        .get("preflight_page_kind")
        .and_then(serde_json::Value::as_str)
    {
        return match kind {
            "bubble" => "bubble",
            "prose" => "prose",
            "mixed" => "mixed",
            _ => "skip",
        };
    }
    let bubbles = analysis
        .get("bubbles")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let unmatched = analysis
        .get("unmatched_text")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let prose_groups = analysis
        .get("bubbles")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .any(|region| {
            region.get("recognizer").and_then(serde_json::Value::as_str) == Some("prose-group")
        });
    let meaningful_chars = analysis
        .get("translation_handoff")
        .and_then(|handoff| handoff.get("items"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("source_text").and_then(serde_json::Value::as_str))
        .map(|text| text.chars().filter(|ch| !ch.is_whitespace()).count())
        .sum::<usize>();
    if prose_groups && bubbles > 0 {
        if unmatched > 0 { "mixed" } else { "prose" }
    } else if bubbles > 0 {
        "bubble"
    } else if unmatched > 0
        && meaningful_chars >= 28
        && analysis
            .get("text_lines")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .any(|line| {
                line.get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|text| {
                        text.chars().any(|ch| {
                            matches!(
                                ch,
                                '\u{3040}'..='\u{30ff}'
                                    | '\u{3400}'..='\u{4dbf}'
                                    | '\u{4e00}'..='\u{9fff}'
                            )
                        }) || text.chars().filter(|ch| ".!?。！？".contains(*ch)).count() > 0
                    })
            })
    {
        "prose"
    } else {
        "skip"
    }
}

fn cover_like_analysis(page_number: usize, analysis: &serde_json::Value) -> bool {
    if page_number != 1 {
        return false;
    }
    let Some(bubbles) = analysis
        .get("bubbles")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    if bubbles.is_empty()
        || bubbles.iter().any(|bubble| {
            bubble
                .get("detector_label")
                .and_then(serde_json::Value::as_u64)
                == Some(0)
        })
    {
        return false;
    }
    let text = analysis
        .get("translation_handoff")
        .and_then(|handoff| handoff.get("items"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("source_text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>();
    let compact = text.join(" ").to_ascii_lowercase();
    let chars = compact.chars().filter(|ch| !ch.is_whitespace()).count();
    let cover_signal = [
        "welcome",
        "ようこそ",
        "r18",
        "dojin",
        "成人向け",
        "18歳未満",
        "for adult only",
    ]
    .iter()
    .any(|needle| compact.contains(needle));
    cover_signal && chars <= 180 && text.len() <= 5
}

fn synthetic_skip_analysis(
    source_language: Option<&str>,
    target_language: Option<&str>,
    config: &Config,
    cover: bool,
) -> serde_json::Value {
    let target = target_language
        .map(str::to_owned)
        .unwrap_or_else(|| config.configured_target_language());
    serde_json::json!({
        "source_language": source_language.unwrap_or("auto"),
        "target_language": target,
        "bubbles": [],
        "text_lines": [],
        "unmatched_text": [],
        "translation_handoff": {"target_language": target, "status":"pending", "items":[]},
        "preflight_page_kind": "skip",
        "preflight_cover": cover,
    })
}

fn preflight_page_manifest(
    page_number: usize,
    source: &std::path::Path,
    source_sha256: &str,
    analysis: &serde_json::Value,
    cached: bool,
) -> serde_json::Value {
    let classification = preflight_page_kind(analysis);
    let items = analysis
        .get("translation_handoff")
        .and_then(|handoff| handoff.get("items"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let required = items
        .iter()
        .filter(|item| {
            !item
                .get("preserve_by_default")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    let preserved = items.len().saturating_sub(required);
    let bubble_count = analysis
        .get("bubbles")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .filter(|count| *count > 0)
        .or_else(|| {
            analysis
                .get("thumbnail_bubble_count")
                .and_then(serde_json::Value::as_u64)
                .map(|count| count as usize)
        })
        .unwrap_or(0);
    let text_line_count = analysis
        .get("text_lines")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .filter(|count| *count > 0)
        .or_else(|| {
            analysis
                .get("thumbnail_line_count")
                .and_then(serde_json::Value::as_u64)
                .map(|count| count as usize)
        })
        .unwrap_or(0);
    serde_json::json!({
        "page_number": page_number,
        "source_image": source,
        "source_sha256": source_sha256,
        "classification": classification,
        "analysis_pending": classification == "bubble" && !cached,
        "cached_analysis": cached,
        "cache_hit": false,
        "cache_source": if cached { "within_job_analysis" } else { "none" },
        "pass_through": classification == "skip",
        "skip": classification == "skip",
        "bubble_count": bubble_count,
        "text_line_count": text_line_count,
        "unmatched_text_count": analysis.get("unmatched_text").and_then(serde_json::Value::as_array).map(Vec::len).unwrap_or(0),
        "required_translation_count": required,
        "preserved_count": preserved,
    })
}

fn preflight_page_reused(page: &serde_json::Value) -> bool {
    page.get("cache_hit")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || page
            .get("cached_analysis")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

fn preflight_cache_telemetry(pages: &[serde_json::Value], total_pages: usize) -> serde_json::Value {
    let reused_cached_pages = pages
        .iter()
        .filter(|page| preflight_page_reused(page))
        .count();
    let newly_processed_pages = pages.len().saturating_sub(reused_cached_pages);
    let pass_through_pages = pages
        .iter()
        .filter(|page| {
            page.get("classification")
                .and_then(serde_json::Value::as_str)
                == Some("skip")
                || page
                    .get("pass_through")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
        })
        .count();
    serde_json::json!({
        "reused_cached_pages": reused_cached_pages,
        "newly_processed_pages": newly_processed_pages,
        "total_pages": total_pages,
        "within_job_cached_pages": reused_cached_pages,
        "cross_job_cached_pages": 0,
        "cross_job_cache_available": false,
        "cache_scope": "within_job_only",
        "pass_through_pages": pass_through_pages,
        "skip_pages": pass_through_pages,
        "message": format!(
            "Reused cached pages: {reused_cached_pages}/{total_pages}; newly processed: {newly_processed_pages}/{total_pages}"
        ),
    })
}

fn attach_preflight_cache_telemetry(
    value: &mut serde_json::Value,
    pages: &[serde_json::Value],
    total_pages: usize,
) {
    let telemetry = preflight_cache_telemetry(pages, total_pages);
    value["cache_telemetry"] = telemetry.clone();
    for key in [
        "reused_cached_pages",
        "newly_processed_pages",
        "total_pages",
        "within_job_cached_pages",
        "cross_job_cached_pages",
        "cross_job_cache_available",
        "cache_scope",
        "pass_through_pages",
        "skip_pages",
    ] {
        if let Some(field) = telemetry.get(key) {
            value[key] = field.clone();
            if let Some(progress) = value.get_mut("progress") {
                progress[key] = field.clone();
            }
        }
    }
    if let Some(message) = telemetry.get("message") {
        value["cache_message"] = message.clone();
        if let Some(progress) = value.get_mut("progress") {
            progress["cache_message"] = message.clone();
        }
    }
}

fn emit_preflight_progress(
    current_page: usize,
    total_pages: usize,
    stage: &str,
    pages: &[serde_json::Value],
) {
    let telemetry = preflight_cache_telemetry(pages, total_pages);
    let reused = telemetry["reused_cached_pages"].as_u64().unwrap_or(0);
    let newly = telemetry["newly_processed_pages"].as_u64().unwrap_or(0);
    let message = telemetry["message"].as_str().unwrap_or_default();
    let line = format!(
        "{}; {message}",
        format_page_progress(current_page, total_pages, stage)
    );
    eprintln!("{line}");
    tracing::info!(
        target: "fukidashi.progress",
        current_page,
        total_pages,
        stage,
        reused_cached_pages = reused,
        newly_processed_pages = newly,
        within_job_cached_pages = reused,
        cross_job_cached_pages = 0u64,
        pass_through_pages = telemetry["pass_through_pages"].as_u64().unwrap_or(0),
        "{line}"
    );
}

fn validate_prose_source_coverage(
    analysis: &serde_json::Value,
    source_image: &Path,
    plan: &StrictSubmissionPlan,
) -> Result<(), FukidashiError> {
    let (width, height) = image::image_dimensions(source_image).map_err(|error| {
        FukidashiError::InvalidInput(format!(
            "unable to validate prose coverage for {}: {error}",
            source_image.display()
        ))
    })?;
    let page_area = (width as f32 * height as f32).max(1.0);
    let Some(bubbles) = analysis
        .get("bubbles")
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(());
    };
    let items = analysis
        .get("translation_handoff")
        .and_then(|handoff| handoff.get("items"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let keep_ids = plan
        .selected
        .iter()
        .filter(|selection| selection.keep_source)
        .map(|selection| selection.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    for bubble in bubbles {
        if bubble.get("recognizer").and_then(serde_json::Value::as_str) != Some("prose-group") {
            continue;
        }
        let Some(id) = bubble.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if keep_ids.contains(id) {
            continue;
        }
        let Some(bbox) = bubble.get("bbox") else {
            continue;
        };
        let Some(rect) = serde_json::from_value::<Rect>(bbox.clone())
            .ok()
            .and_then(|rect| rect.validate().ok())
        else {
            continue;
        };
        let area_ratio = ((rect.x2 - rect.x1).max(0.0) * (rect.y2 - rect.y1).max(0.0)) / page_area;
        if area_ratio < 0.30 {
            continue;
        }
        let source_text = items
            .iter()
            .find(|item| item.get("id").and_then(serde_json::Value::as_str) == Some(id))
            .and_then(|item| item.get("source_text").and_then(serde_json::Value::as_str))
            .unwrap_or_default();
        let chars = source_text.chars().filter(|ch| !ch.is_whitespace()).count();
        if chars < 80 {
            return Err(FukidashiError::Diagnostic {
                message: format!(
                    "prose source item {id:?} is implausibly short for its page-spanning region; refusing destructive cleaning"
                ),
                details: serde_json::json!({
                    "stage": "prose_coverage",
                    "code": "collapsed_prose_source",
                    "id": id,
                    "source_chars": chars,
                    "bbox_area_ratio": area_ratio,
                    "next_step": "retry OCR with a corrected source item or set keep_source=true for this prose item; no pixels were cleaned",
                }),
            });
        }
    }
    Ok(())
}

/// Reject overlapping auto-generated prose groups before their rectangles can
/// be used for destructive cleaning or typesetting.  Dense prose commonly has
/// adjacent line boxes, so only substantial overlap relative to the smaller
/// box is treated as an unsafe merge.
fn validate_prose_group_geometry(
    analysis: &serde_json::Value,
    plan: &StrictSubmissionPlan,
) -> Result<(), FukidashiError> {
    let Some(bubbles) = analysis
        .get("bubbles")
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(());
    };
    let keep_ids = plan
        .selected
        .iter()
        .filter(|selection| selection.keep_source)
        .map(|selection| selection.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let selected_ids = plan
        .selected
        .iter()
        .map(|selection| selection.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let groups = bubbles
        .iter()
        .filter_map(|bubble| {
            let id = bubble.get("id")?.as_str()?;
            let recognizer = bubble.get("recognizer")?.as_str()?;
            if !selected_ids.contains(id)
                || (recognizer != "prose-group" && recognizer != "baberu-ocr")
            {
                return None;
            }
            let rect = serde_json::from_value::<Rect>(bubble.get("bbox")?.clone())
                .ok()?
                .validate()
                .ok()?;
            Some((id, recognizer, rect))
        })
        .collect::<Vec<_>>();
    let mut collisions = Vec::new();
    for left_index in 0..groups.len() {
        let (left_id, left_recognizer, left) = groups[left_index];
        for (right_id, right_recognizer, right) in groups.iter().skip(left_index + 1).copied() {
            if left_recognizer != "prose-group" && right_recognizer != "prose-group" {
                continue;
            }
            if !rects_overlap(left, right) {
                continue;
            }
            let intersection_width = (left.x2.min(right.x2) - left.x1.max(right.x1)).max(0.0);
            let intersection_height = (left.y2.min(right.y2) - left.y1.max(right.y1)).max(0.0);
            let intersection = intersection_width * intersection_height;
            let left_area = (left.x2 - left.x1) * (left.y2 - left.y1);
            let right_area = (right.x2 - right.x1) * (right.y2 - right.y1);
            let overlap_ratio = intersection / left_area.min(right_area).max(1.0);
            if overlap_ratio < 0.25 || (keep_ids.contains(left_id) && keep_ids.contains(right_id)) {
                continue;
            }
            collisions.push(serde_json::json!({
                "ids": [left_id, right_id],
                "overlap_ratio": overlap_ratio,
                "bboxes": [
                    { "id": left_id, "bbox": left },
                    { "id": right_id, "bbox": right },
                ],
            }));
        }
    }
    if collisions.is_empty() {
        return Ok(());
    }
    Err(FukidashiError::Diagnostic {
        message: "overlapping automatic prose regions are unsafe to clean or render; preserve both regions or correct the source analysis".into(),
        details: serde_json::json!({
            "stage": "prose_geometry",
            "code": "overlapping_prose_groups",
            "overlap_threshold": 0.25,
            "collisions": collisions,
            "next_step": "Resubmit keep_source=true for both IDs in every reported collision, or correct the OCR grouping and restart this page; no pixels were cleaned or rendered.",
        }),
    })
}

const MAX_SAVED_ANALYSIS_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
struct SavedTranslationItem {
    id: String,
    kind: String,
    preserve_by_default: bool,
    source_text: String,
    source_language: String,
    confidence: f32,
    /// Geometry is intentionally optional while a strict page is waiting for
    /// the client's decision.  Silent OCR false positives can carry null or
    /// degenerate geometry; an all-keep submission must still be able to use
    /// the verified source pass-through.  The normal translation path
    /// validates this field before constructing any typeset payload.
    bbox: Option<Rect>,
    correction_applied: bool,
    status: String,
    translation: Option<String>,
    keep_source: bool,
    needs_review: bool,
}

#[derive(Debug, Clone)]
struct StrictTranslationSelection {
    id: String,
    text: String,
    keep_source: bool,
    needs_review: bool,
}

#[derive(Debug, Clone)]
struct StrictSubmissionPlan {
    all_items: Vec<SavedTranslationItem>,
    items: Vec<SavedTranslationItem>,
    selected: Vec<StrictTranslationSelection>,
}

fn read_saved_analysis(
    workflow: &Workflow,
    path: &std::path::Path,
) -> Result<serde_json::Value, FukidashiError> {
    let path = workflow
        .require_owned(path, "analysis checkpoint")
        .map_err(|error| FukidashiError::InvalidInput(error.to_string()))?;
    let metadata = std::fs::metadata(&path)?;
    if !metadata.is_file() || metadata.len() > MAX_SAVED_ANALYSIS_BYTES {
        return Err(FukidashiError::ResourceLimit(
            "analysis checkpoint is missing or exceeds 16 MiB".into(),
        ));
    }
    Ok(serde_json::from_slice(&std::fs::read(&path)?)?)
}

fn saved_translation_items(
    analysis: &serde_json::Value,
) -> Result<Vec<SavedTranslationItem>, FukidashiError> {
    let Some(items) = analysis
        .get("translation_handoff")
        .and_then(|handoff| handoff.get("items"))
        .and_then(serde_json::Value::as_array)
    else {
        let has_detected_regions =
            ["bubbles", "text_lines", "unmatched_text"]
                .into_iter()
                .any(|key| {
                    analysis
                        .get(key)
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|items| !items.is_empty())
                });
        if !has_detected_regions {
            return Ok(Vec::new());
        }
        return Err(FukidashiError::InvalidInput(
            "analysis checkpoint has no translation_handoff.items array".into(),
        ));
    };
    let mut seen = BTreeSet::new();
    let mut parsed = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let id = item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                FukidashiError::InvalidInput(format!(
                    "translation item {index} has no non-empty stable id"
                ))
            })?
            .to_owned();
        if !seen.insert(id.clone()) {
            return Err(FukidashiError::InvalidInput(format!(
                "analysis checkpoint contains duplicate translation id {id:?}"
            )));
        }
        // Older checkpoints did not carry a structural class.  Their stable
        // IDs still give us a backwards-compatible classification boundary.
        let kind = item
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .filter(|kind| !kind.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if id.starts_with("text-") {
                    "unmatched_text".to_owned()
                } else {
                    "dialogue".to_owned()
                }
            });
        let preserve_by_default = item
            .get("preserve_by_default")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or_else(|| kind == "unmatched_text");
        let source_text = item
            .get("source_text")
            .and_then(serde_json::Value::as_str)
            .or_else(|| item.get("ocr_text").and_then(serde_json::Value::as_str))
            .ok_or_else(|| {
                FukidashiError::InvalidInput(format!("translation item {id:?} has no source_text"))
            })?
            .to_owned();
        // Do not reject geometry while building the waiting-page contract.
        // The strict all-keep path deliberately bypasses geometry, cleaner,
        // and typesetter validation.  A normal/partial submission calls
        // `strict_typeset_payloads`, which reports the original parse or
        // validation error before mutating/persisting the handoff.
        let bbox = item
            .get("bbox")
            .cloned()
            .and_then(|value| serde_json::from_value::<Rect>(value).ok())
            .and_then(|rect| rect.validate().ok());
        let confidence = item
            .get("confidence")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0) as f32;
        if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
            return Err(FukidashiError::InvalidInput(format!(
                "translation item {id:?} has invalid confidence"
            )));
        }
        parsed.push(SavedTranslationItem {
            id,
            kind,
            preserve_by_default,
            source_text,
            source_language: item
                .get("source_language")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("auto")
                .to_owned(),
            confidence,
            bbox,
            correction_applied: item
                .get("correction_applied")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            status: item
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("pending")
                .to_owned(),
            translation: item
                .get("translation")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            keep_source: item
                .get("keep_source")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            needs_review: item
                .get("needs_review")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        });
    }
    validate_analysis_handoff_coverage(analysis, &parsed)?;
    Ok(parsed)
}

fn validate_analysis_handoff_coverage(
    analysis: &serde_json::Value,
    items: &[SavedTranslationItem],
) -> Result<(), FukidashiError> {
    let ids = items
        .iter()
        .map(|item| item.id.as_str())
        .collect::<BTreeSet<_>>();
    for key in ["bubbles", "unmatched_text"] {
        let Some(regions) = analysis.get(key).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for (index, region) in regions.iter().enumerate() {
            let id = region
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    FukidashiError::InvalidInput(format!(
                        "detected {key} region {index} has no stable id in the translation handoff"
                    ))
                })?;
            if !ids.contains(id) {
                return Err(FukidashiError::InvalidInput(format!(
                    "detected {key} region {id:?} is missing from the translation handoff"
                )));
            }
        }
    }
    let has_detected_regions = ["bubbles", "unmatched_text"].into_iter().any(|key| {
        analysis
            .get(key)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|regions| !regions.is_empty())
    });
    if !has_detected_regions && items.is_empty() {
        let target = analysis
            .get("target_language")
            .and_then(serde_json::Value::as_str);
        let prose_line = analysis
            .get("text_lines")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .any(|line| {
                let source_language = line
                    .get("source_language")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("auto");
                let text = line
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                is_vietnamese_target(target) && is_english_prose(source_language, text)
            });
        if prose_line {
            return Err(FukidashiError::InvalidInput(
                "detected sentence-like text has no translation handoff item; refusing a silent pass-through".into(),
            ));
        }
    }
    Ok(())
}

fn strict_item_json(item: &SavedTranslationItem) -> serde_json::Value {
    let mut value = serde_json::json!({
        "id": item.id,
        "kind": item.kind,
        "preserve_by_default": item.preserve_by_default,
        "source_text": item.source_text,
        "source_language": item.source_language,
        "confidence": item.confidence,
        "bbox": item.bbox,
        "correction_applied": item.correction_applied,
        "status": item.status,
        "keep_source": item.keep_source,
        "needs_review": item.needs_review,
    });
    if let Some(translation) = item.translation.as_deref() {
        value["translation"] = serde_json::Value::String(translation.to_owned());
    }
    value
}

fn extract_tool_json(
    result: CallToolResult,
    operation: &str,
) -> Result<serde_json::Value, FukidashiError> {
    let text = result
        .content
        .into_iter()
        .find_map(|block| match block {
            ContentBlock::Text(value) => Some(value.text),
            _ => None,
        })
        .ok_or_else(|| {
            FukidashiError::Inference(format!("{operation} returned no JSON content"))
        })?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    if result.is_error == Some(true) {
        let message = value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("operation failed")
            .to_owned();
        if let Some(details) = value.get("diagnostic").cloned() {
            return Err(FukidashiError::Diagnostic {
                message: format!("{operation}: {message}"),
                details,
            });
        }
        return Err(FukidashiError::Inference(format!("{operation}: {message}")));
    }
    Ok(value)
}

fn strict_response_items(items: &[SavedTranslationItem]) -> Vec<serde_json::Value> {
    items.iter().map(strict_item_json).collect()
}

fn japanese_prose_char_count(text: &str) -> usize {
    text.chars()
        .filter(|character| {
            matches!(
                character,
                '\u{3040}'..='\u{30ff}' | '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}'
            )
        })
        .count()
}

fn metadata_or_art_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let has_email = text.contains('@') && text.contains('.');
    let has_date = text
        .chars()
        .filter(|character| character.is_ascii_digit())
        .count()
        >= 2
        && text
            .chars()
            .any(|character| "/.-年月日".contains(character));
    has_email
        || has_date
        || text
            .chars()
            .all(|character| character.is_whitespace() || ".．．…・~～-—_".contains(character))
        || [
            "発行",
            "発行日",
            "印刷",
            "連絡先",
            "著者",
            "contact",
            "email",
            "date",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// OCR line detection can leave a short paragraph line just outside the
/// detector's broad prose group.  On a dense Japanese prose page those lines
/// are part of the body and must be required source items, otherwise the
/// renderer preserves a visible Japanese fragment beside the translation.
fn is_main_japanese_prose_line(line: &serde_json::Value, items: &[serde_json::Value]) -> bool {
    let text = line
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    if japanese_prose_char_count(text) < 3 || metadata_or_art_text(text) {
        return false;
    }
    let confidence = line
        .get("confidence")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.35);
    if confidence < 0.35 {
        return false;
    }
    let Some(rect) = json_bbox(line) else {
        return false;
    };
    let prose_rects = items.iter().filter_map(|item| {
        if item
            .get("keep_source")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            || item
                .get("preserve_by_default")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        {
            return None;
        }
        let kind = item
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if !matches!(kind, "dialogue" | "prose" | "prose-line") {
            return None;
        }
        json_bbox(item)
    });
    let prose_rects = prose_rects.collect::<Vec<_>>();
    if prose_rects.len() < 2 {
        return false;
    }
    let min_y = prose_rects
        .iter()
        .map(|rect| rect.y1)
        .fold(f32::INFINITY, f32::min);
    let max_y = prose_rects
        .iter()
        .map(|rect| rect.y2)
        .fold(f32::NEG_INFINITY, f32::max);
    // Keep the heading and footer out of this fallback.  The line must sit in
    // the vertical body span and be close to one of the existing groups.
    if rect.y1 <= min_y || rect.y2 > max_y + 60.0 {
        return false;
    }
    prose_rects.iter().any(|group| {
        let vertical_gap = if rect.y1 > group.y2 {
            rect.y1 - group.y2
        } else if group.y1 > rect.y2 {
            group.y1 - rect.y2
        } else {
            0.0
        };
        let horizontal_overlap = rect.x1.max(group.x1) < rect.x2.min(group.x2);
        vertical_gap <= 70.0 && horizontal_overlap
    })
}

/// Reject sparse, page-spanning OCR boxes that bridge separate text columns.
/// These are often chart dividers or graphic artifacts; they must remain
/// explicit preserved detections instead of being promoted as body prose.
fn is_wide_sparse_column_bridge(line: &serde_json::Value, lines: &[serde_json::Value]) -> bool {
    let Some(rect) = json_bbox(line) else {
        return false;
    };
    let span = lines
        .iter()
        .filter_map(json_bbox)
        .fold(None, |bounds: Option<(f32, f32)>, other| {
            Some(match bounds {
                Some((min_x, max_x)) => (min_x.min(other.x1), max_x.max(other.x2)),
                None => (other.x1, other.x2),
            })
        })
        .map(|(min_x, max_x)| max_x - min_x)
        .unwrap_or(0.0);
    let width = rect.x2 - rect.x1;
    let height = rect.y2 - rect.y1;
    let text = line
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let compact_chars = text.chars().filter(|ch| ch.is_alphanumeric()).count();
    if span <= 0.0
        || width / span < 0.75
        || width / height.max(1.0) < 18.0
        || compact_chars as f32 / width.max(1.0) >= 0.025
    {
        return false;
    }
    let neighbor_centers = lines
        .iter()
        .filter_map(|other| {
            let other_rect = json_bbox(other)?;
            let other_text = other.get("text")?.as_str()?.trim();
            (other_rect.x1.max(rect.x1) < other_rect.x2.min(rect.x2)
                && (other_rect.y1.max(rect.y1) - other_rect.y2.min(rect.y2)).max(0.0) <= 120.0
                && japanese_prose_char_count(other_text) >= 8)
                .then_some((other_rect.x1 + other_rect.x2) * 0.5)
        })
        .collect::<Vec<_>>();
    let Some(min_center) = neighbor_centers.iter().copied().reduce(f32::min) else {
        return false;
    };
    let Some(max_center) = neighbor_centers.iter().copied().reduce(f32::max) else {
        return false;
    };
    max_center - min_center >= span * 0.30
}

/// Use the detector's line inventory as page context when the bubble detector
/// found only part of a prose page. Requiring existing translated bubbles here
/// hid the remaining paragraphs on afterwords with weak detector coverage.
fn is_dense_body_line(
    line: &serde_json::Value,
    items: &[serde_json::Value],
    lines: &[serde_json::Value],
) -> bool {
    if is_wide_sparse_column_bridge(line, lines) {
        return false;
    }
    if is_main_japanese_prose_line(line, items) {
        return true;
    }
    let text = line
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    let confidence = line
        .get("confidence")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.35);
    let Some(rect) = json_bbox(line) else {
        return false;
    };
    if japanese_prose_char_count(text) < 8 || metadata_or_art_text(text) || confidence < 0.20 {
        return false;
    }
    let candidates = lines
        .iter()
        .filter_map(|other| {
            let other_text = other
                .get("text")
                .and_then(serde_json::Value::as_str)?
                .trim();
            let other_confidence = other
                .get("confidence")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.35);
            let other_rect = json_bbox(other)?;
            (japanese_prose_char_count(other_text) >= 8
                && !metadata_or_art_text(other_text)
                && other_confidence >= 0.20
                && (other_rect.y1 != rect.y1 || other_rect.y2 != rect.y2)
                && (other_rect.y1.max(rect.y1) - other_rect.y2.min(rect.y2)).max(0.0) <= 120.0
                && other_rect.x1.max(rect.x1) < other_rect.x2.min(rect.x2))
            .then_some(other_rect)
        })
        .collect::<Vec<_>>();
    !candidates.is_empty()
}

/// Keep every detector line represented by the strict handoff. OCR can
/// associate a small line with a broad bubble while the bubble-level
/// recognizer omits that line from `source_text`; allowing it to disappear
/// makes a later destructive clean fail after the client already translated
/// the page. Legacy checkpoints are reconciled lazily: every uncovered line
/// becomes an explicit source-preserved item, including confident lines. This
/// is deliberately fail-safe for resumed jobs: a client can still translate
/// the surrounding items, while the source pixels for an omitted line are
/// never erased. The caller persists this normalization before issuing a
/// work token.
fn augment_missing_detected_text_items(
    analysis: &mut serde_json::Value,
) -> Result<Vec<serde_json::Value>, FukidashiError> {
    let detected_bubbles = analysis
        .get("bubbles")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(lines) = analysis
        .get("text_lines")
        .and_then(serde_json::Value::as_array)
        .cloned()
    else {
        return Ok(Vec::new());
    };
    let Some(items) = analysis
        .get_mut("translation_handoff")
        .and_then(|handoff| handoff.get_mut("items"))
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Ok(Vec::new());
    };
    let mut additions = Vec::new();
    let mut auto_preserved = Vec::new();
    // Some OCR checkpoints already contain the unmatched line as a preserved
    // text-* item.  Promote those items before reconciling text_lines so a
    // line that is present in both arrays cannot be mistaken for an already
    // safe preservation decision.
    let existing_items = items.clone();
    for item_index in 0..items.len() {
        let item = &existing_items[item_index];
        if !(item
            .get("keep_source")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            || item
                .get("preserve_by_default")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false))
            || item.get("kind").and_then(serde_json::Value::as_str) != Some("unmatched_text")
        {
            continue;
        }
        let Some(source_text) = item.get("source_text").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let synthetic_line = serde_json::json!({
            "text": source_text,
            "confidence": item.get("confidence").cloned().unwrap_or(serde_json::Value::Null),
            "bbox": item.get("bbox").cloned().unwrap_or(serde_json::Value::Null),
        });
        if !is_dense_body_line(&synthetic_line, &existing_items, &lines) {
            continue;
        }
        let item = &mut items[item_index];
        item["kind"] = serde_json::Value::String("prose-line".into());
        item["preserve_by_default"] = serde_json::Value::Bool(false);
        item["keep_source"] = serde_json::Value::Bool(false);
        item["needs_review"] = serde_json::Value::Bool(true);
        item["translation"] = serde_json::Value::Null;
        item["status"] = serde_json::Value::String("pending".into());
        item["auto_preserved"] = serde_json::Value::Bool(false);
        item["auto_promoted_prose"] = serde_json::Value::Bool(true);
    }
    for (index, line) in lines.iter().enumerate() {
        let Some(id) = line
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
        else {
            continue;
        };
        let Some(rect) = json_bbox(line) else {
            continue;
        };
        let source_text = line
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim();
        if source_text.is_empty() {
            continue;
        }
        let source_language = line
            .get("source_language")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("auto");
        let body_line = is_dense_body_line(line, items, &lines);
        let matching_id = items.iter().position(|item| {
            item.get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|item_id| item_id == id)
        });
        if let Some(item_index) = matching_id {
            let item = &mut items[item_index];
            if body_line {
                item["kind"] = serde_json::Value::String("prose-line".into());
                item["preserve_by_default"] = serde_json::Value::Bool(false);
                item["keep_source"] = serde_json::Value::Bool(false);
                item["needs_review"] = serde_json::Value::Bool(true);
                item["translation"] = serde_json::Value::Null;
                item["status"] = serde_json::Value::String("pending".into());
                item["auto_preserved"] = serde_json::Value::Bool(false);
                item["auto_promoted_prose"] = serde_json::Value::Bool(true);
                continue;
            }
            if !item
                .get("keep_source")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                item["kind"] = serde_json::Value::String("unmatched_text".into());
                item["preserve_by_default"] = serde_json::Value::Bool(true);
                item["keep_source"] = serde_json::Value::Bool(true);
                item["needs_review"] = serde_json::Value::Bool(true);
                item["translation"] = serde_json::Value::Null;
                item["status"] = serde_json::Value::String("preserved".into());
                let audit = serde_json::json!({
                    "id": id,
                    "detected_text_index": index,
                    "confidence": line.get("confidence").cloned().unwrap_or(serde_json::Value::Null),
                    "source_text": source_text,
                    "reason": "legacy handoff item did not explicitly preserve the detected line",
                });
                auto_preserved.push(audit);
            }
            continue;
        }
        if detected_bubble_handoff_covers_line(
            &detected_bubbles,
            items,
            rect,
            source_text,
            source_language,
            line.get("confidence")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(1.0),
        ) {
            // The bubble-level OCR can differ from its tighter line crop.
            // This is still one speech balloon; use its best OCR for the
            // bubble translation instead of producing a second overlay.
            continue;
        }
        // The OCR handoff may already contain this line under a detector
        // generated text-* ID.  On a dense Japanese prose span that existing
        // preserve item is still part of the body, so promote it in place to
        // keep the stable ID while making the translation decision required.
        if body_line {
            let promoted_index = items.iter().position(|item| {
                (item
                    .get("keep_source")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                    || item
                        .get("preserve_by_default")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false))
                    && item
                        .get("source_text")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|item_text| item_text.trim() == source_text)
                    && json_bbox(item).is_some_and(|item_rect| rects_overlap(rect, item_rect))
            });
            if let Some(item_index) = promoted_index {
                let item = &mut items[item_index];
                item["kind"] = serde_json::Value::String("prose-line".into());
                item["preserve_by_default"] = serde_json::Value::Bool(false);
                item["keep_source"] = serde_json::Value::Bool(false);
                item["needs_review"] = serde_json::Value::Bool(true);
                item["translation"] = serde_json::Value::Null;
                item["status"] = serde_json::Value::String("pending".into());
                item["auto_preserved"] = serde_json::Value::Bool(false);
                item["auto_promoted_prose"] = serde_json::Value::Bool(true);
                continue;
            }
        }
        let already_covered = items.iter().any(|item| {
            let Some(item_rect) = json_bbox(item) else {
                return false;
            };
            let item_source = item
                .get("source_text")
                .and_then(serde_json::Value::as_str)
                .or_else(|| item.get("ocr_text").and_then(serde_json::Value::as_str))
                .unwrap_or_default();
            let item_language = item
                .get("source_language")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("auto");
            rects_overlap(rect, item_rect)
                && source_text_covers_line(item_source, source_text, item_language)
        });
        if already_covered {
            continue;
        }
        let confidence = line
            .get("confidence")
            .and_then(serde_json::Value::as_f64)
            .filter(|confidence| confidence.is_finite())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);
        additions.push(serde_json::json!({
            "id": id,
            "kind": if body_line { "prose-line" } else { "unmatched_text" },
            "preserve_by_default": !body_line,
            "source_text": source_text,
            "ocr_text": source_text,
            "source_language": source_language,
            "confidence": confidence,
            "bbox": rect,
            "correction_applied": false,
            "status": if body_line { "pending" } else { "preserved" },
            "translation": serde_json::Value::Null,
            "keep_source": !body_line,
            "needs_review": true,
            "auto_preserved": !body_line,
            "auto_promoted_prose": body_line,
            "detected_text_index": index,
        }));
        if !body_line {
            auto_preserved.push(serde_json::json!({
                "id": id,
                "detected_text_index": index,
                "confidence": confidence,
                "source_text": source_text,
                "reason": "legacy handoff omitted a detected line; source was preserved automatically",
            }));
        }
    }
    if !additions.is_empty() {
        items.extend(additions);
    }
    if !auto_preserved.is_empty() {
        let strict = analysis
            .as_object_mut()
            .expect("analysis is an object")
            .entry("strict_v1")
            .or_insert_with(|| serde_json::json!({}));
        if !strict.is_object() {
            *strict = serde_json::json!({});
        }
        let audit = strict
            .as_object_mut()
            .expect("strict_v1 is an object")
            .entry("auto_preserved_items")
            .or_insert_with(|| serde_json::json!([]));
        let audit_items = audit.as_array_mut().ok_or_else(|| {
            FukidashiError::InvalidInput("strict_v1.auto_preserved_items must be an array".into())
        })?;
        let existing_audit_ids = audit_items
            .iter()
            .filter_map(|item| {
                item.get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<BTreeSet<_>>();
        for entry in &auto_preserved {
            if entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| !existing_audit_ids.contains(id))
            {
                audit_items.push(entry.clone());
            }
        }
    }
    Ok(auto_preserved)
}

fn resolve_sfx_mode(raw: Option<&str>) -> Result<bool, FukidashiError> {
    match raw.unwrap_or("preserve") {
        "preserve" => Ok(false),
        "replace" => Ok(true),
        value => Err(FukidashiError::InvalidInput(format!(
            "sfx_mode must be preserve or replace, got {value:?}"
        ))),
    }
}

/// Normalize the small OCR route enum accepted by MCP clients. Older
/// clients sometimes copied an inferred analysis value such as `en|latin`
/// back into a request; that is a mixed-page result, not a valid override.
/// Treat it as automatic routing so a later page cannot invalidate an already
/// completed page.
fn normalize_requested_source_language(
    raw: Option<String>,
) -> Result<Option<String>, FukidashiError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() || value == "auto" || value.contains('|') {
        return Ok(None);
    }
    if matches!(value.as_str(), "ja" | "zh" | "ko" | "en" | "latin") {
        Ok(Some(value))
    } else {
        Err(FukidashiError::InvalidInput(format!(
            "unsupported OCR source_language {value:?}; use auto, ja, zh, ko, en, or latin"
        )))
    }
}

fn translatable_items(
    items: &[SavedTranslationItem],
    replace_sfx: bool,
    target_language: Option<&str>,
) -> Vec<SavedTranslationItem> {
    items
        .iter()
        .filter(|item| {
            if item.keep_source {
                return false;
            }
            replace_sfx
                || !item.preserve_by_default
                || (is_vietnamese_target(target_language)
                    && is_english_prose(&item.source_language, &item.source_text))
        })
        .cloned()
        .collect()
}

fn is_vietnamese_target(target_language: Option<&str>) -> bool {
    target_language
        .unwrap_or_default()
        .split(['-', '_'])
        .next()
        .is_some_and(|language| language.eq_ignore_ascii_case("vi"))
}

/// Detect a sentence-like Latin source conservatively enough to leave short
/// labels and sound effects in the default preserve scope.  Long English
/// prose still needs a translation decision when the target is Vietnamese,
/// even if OCR placed it outside a speech-bubble contour.
fn is_english_prose(source_language: &str, text: &str) -> bool {
    let language = source_language.to_ascii_lowercase();
    let language_is_english = language == "en"
        || language.starts_with("en-")
        || language.starts_with("en|")
        || language == "auto"
        || language == "und"
        || language.is_empty();
    if !language_is_english {
        return false;
    }
    let text = text.trim();
    if text.len() < 12 {
        return false;
    }
    let latin_letters = text
        .chars()
        .filter(|character| character.is_ascii_alphabetic())
        .count();
    let letters = text
        .chars()
        .filter(|character| character.is_alphabetic())
        .count();
    if latin_letters < 10 || letters == 0 || latin_letters * 100 < letters * 65 {
        return false;
    }
    let words = text
        .split_whitespace()
        .map(|word| word.trim_matches(|character: char| !character.is_ascii_alphabetic()))
        .filter(|word| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|character| character.is_ascii_alphabetic())
        })
        .collect::<Vec<_>>();
    if words.len() < 3 {
        return false;
    }
    let stopword_hits = words
        .iter()
        .filter(|word| {
            matches!(
                word.to_ascii_lowercase().as_str(),
                "a" | "an"
                    | "and"
                    | "are"
                    | "but"
                    | "for"
                    | "from"
                    | "he"
                    | "i"
                    | "in"
                    | "is"
                    | "it"
                    | "of"
                    | "on"
                    | "she"
                    | "that"
                    | "the"
                    | "this"
                    | "to"
                    | "was"
                    | "we"
                    | "were"
                    | "with"
                    | "you"
            )
        })
        .count();
    let unique_words = words
        .iter()
        .map(|word| word.to_ascii_lowercase())
        .collect::<BTreeSet<_>>()
        .len();
    text.chars()
        .any(|character| ".?!,:;'\"".contains(character))
        || stopword_hits >= 2
        || (words.len() >= 4 && unique_words >= 2)
}

fn normalized_source_text(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn translation_is_unchanged(
    item: &SavedTranslationItem,
    translation: &str,
    target: Option<&str>,
) -> bool {
    is_vietnamese_target(target)
        && is_english_prose(&item.source_language, &item.source_text)
        && normalized_source_text(&item.source_text) == normalized_source_text(translation)
}

fn preserved_item_json(item: &SavedTranslationItem) -> serde_json::Value {
    serde_json::json!({
        "id": item.id,
        "kind": item.kind,
        "source_text": item.source_text,
        "bbox": item.bbox,
        "translation_allowed": false,
        "preserved": true,
        "keep_source": true,
        "needs_review": item.needs_review,
    })
}

impl FukidashiServer {
    fn issue_translation_claim(
        &self,
        pending: &PendingPage,
        analysis: &serde_json::Value,
        source_language: Option<String>,
        target_language: Option<String>,
        replace_sfx: bool,
    ) -> Result<String, FukidashiError> {
        let all_items = saved_translation_items(analysis)?;
        let items = translatable_items(&all_items, replace_sfx, target_language.as_deref());
        if items.is_empty() {
            return Err(FukidashiError::InvalidInput(
                "page has no translation items; keep the page outside the translation scope or review it manually instead of fabricating an unchanged clean artifact".into(),
            ));
        }
        let analysis_sha256 = self
            .workflow
            .managed_file_sha256(&pending.analysis_path, "analysis checkpoint")
            .map_err(|error| FukidashiError::InvalidInput(error.to_string()))?;
        let item_ids = items.into_iter().map(|item| item.id).collect();
        let token = format!("strict-v1-{}", uuid::Uuid::new_v4().simple());
        let claim = TranslationClaim {
            job_dir: pending.job_dir.clone(),
            source_image: pending.source_image.clone(),
            page_number: pending.page_number,
            total_pages: pending.total_pages,
            analysis_path: pending.analysis_path.clone(),
            analysis_sha256,
            item_ids,
            source_language,
            target_language,
            replace_sfx,
            in_progress: false,
            consumed: false,
        };
        let mut claims = self.translation_claims.lock().map_err(|_| {
            FukidashiError::RuntimeUnavailable("translation claim lock poisoned".into())
        })?;
        // A fresh start for the same page supersedes an abandoned token.  The
        // old token remains in the map as a stale capability and is rejected.
        claims.retain(|_, existing| {
            existing.job_dir != claim.job_dir || existing.source_image != claim.source_image
        });
        claims.insert(token.clone(), claim);
        Ok(token)
    }

    fn begin_translation_claim(&self, token: &str) -> Result<TranslationClaim, FukidashiError> {
        if token.trim().is_empty() || token.len() > 128 {
            return Err(FukidashiError::InvalidInput(
                "work_token is invalid; call fukidashi_translation_start again".into(),
            ));
        }
        let mut claims = self.translation_claims.lock().map_err(|_| {
            FukidashiError::RuntimeUnavailable("translation claim lock poisoned".into())
        })?;
        let claim = claims.get_mut(token).ok_or_else(|| {
            FukidashiError::InvalidInput(
                "work_token is unknown or expired; call fukidashi_translation_start to resume"
                    .into(),
            )
        })?;
        if claim.consumed {
            return Err(FukidashiError::InvalidInput(
                "work_token was already consumed; call fukidashi_translation_start to resume"
                    .into(),
            ));
        }
        if claim.in_progress {
            return Err(FukidashiError::InvalidInput(
                "work_token is already being processed; wait for that submission to finish".into(),
            ));
        }
        claim.in_progress = true;
        Ok(claim.clone())
    }

    fn reset_translation_claim(&self, token: &str) {
        if let Ok(mut claims) = self.translation_claims.lock()
            && let Some(claim) = claims.get_mut(token)
        {
            claim.in_progress = false;
        }
    }

    fn consume_translation_claim(&self, token: &str) -> Result<(), FukidashiError> {
        let mut claims = self.translation_claims.lock().map_err(|_| {
            FukidashiError::RuntimeUnavailable("translation claim lock poisoned".into())
        })?;
        let claim = claims.get_mut(token).ok_or_else(|| {
            FukidashiError::InvalidInput("work_token disappeared while processing".into())
        })?;
        claim.in_progress = false;
        claim.consumed = true;
        Ok(())
    }

    fn refresh_translation_claim_hash(
        &self,
        token: &str,
        analysis_sha256: String,
    ) -> Result<(), FukidashiError> {
        let mut claims = self.translation_claims.lock().map_err(|_| {
            FukidashiError::RuntimeUnavailable("translation claim lock poisoned".into())
        })?;
        let claim = claims.get_mut(token).ok_or_else(|| {
            FukidashiError::InvalidInput(
                "work_token disappeared while persisting translations".into(),
            )
        })?;
        if claim.consumed || !claim.in_progress {
            return Err(FukidashiError::InvalidInput(
                "work_token is no longer active".into(),
            ));
        }
        claim.analysis_sha256 = analysis_sha256;
        Ok(())
    }

    fn strict_review_ready(
        &self,
        job: &std::path::Path,
    ) -> Result<serde_json::Value, FukidashiError> {
        let job = self
            .workflow
            .resolve_managed_job_path(&job.to_string_lossy())
            .map_err(|error| FukidashiError::InvalidInput(error.to_string()))?;
        let job_id = job
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| FukidashiError::InvalidInput("managed job has no stable id".into()))?;
        Ok(serde_json::json!({
            "protocol": "strict-v1",
            "status": "review_ready",
            "job_id": job_id,
            "next_action": {
                "tool": "fukidashi_review_and_export",
                "arguments": {
                    "job_id": job_id,
                    "format": "zip",
                },
            },
            "compatibility_actions": {
                "serve": {"tool": "fukidashi_serve_editor", "arguments": {"job_id": job_id}},
                "wait": "call fukidashi_wait_for_review with the returned review_session_id and review_revision",
                "export": "call fukidashi_export only after explicit approval",
            },
            "export_after": "the combined review tool exports only after explicit approval",
        }))
    }

    fn strict_page_response(
        &self,
        pending: &PendingPage,
        analysis: &serde_json::Value,
        source_language: Option<String>,
        target_language: Option<String>,
        replace_sfx: bool,
    ) -> Result<serde_json::Value, FukidashiError> {
        let all_items = saved_translation_items(analysis)?;
        let items = translatable_items(&all_items, replace_sfx, target_language.as_deref());
        let auto_preserved_items = analysis
            .get("strict_v1")
            .and_then(|strict| strict.get("auto_preserved_items"))
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let preserved_items = if replace_sfx {
            Vec::new()
        } else {
            all_items
                .iter()
                .filter(|item| {
                    item.preserve_by_default && !items.iter().any(|required| required.id == item.id)
                })
                .map(preserved_item_json)
                .collect::<Vec<_>>()
        };
        let job_id = pending.job_id.clone();
        if items.is_empty() {
            let mut value = serde_json::json!({
                "protocol": "strict-v1",
                "status": "needs_manual_scope",
                "job_id": job_id,
                "page_number": pending.page_number,
                "total_pages": pending.total_pages,
                "translation_items": [],
                "required_translation_ids": [],
            "preserved_items": preserved_items,
            "auto_preserved_items": auto_preserved_items,
            "lore": self.workflow.read_lore(&pending.job_dir).map_err(|error| {
                FukidashiError::InvalidInput(format!("read job lore: {error}"))
            })?,
            "sfx_mode": if replace_sfx { "replace" } else { "preserve" },
                "error": "page has no translation items; the server will not fabricate an unchanged clean artifact",
                "next_action": {
                    "action": "manual_scope_required",
                    "reason": "start a new job with a scope that excludes this page, or review it manually",
                },
            });
            attach_page_progress(
                &mut value,
                pending.page_number,
                pending.total_pages,
                "Manual scope required",
            );
            return Ok(value);
        }
        let token = self.issue_translation_claim(
            pending,
            analysis,
            source_language,
            target_language,
            replace_sfx,
        )?;
        let ids = items.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
        let mut value = serde_json::json!({
            "protocol": "strict-v1",
            "status": "page_ready",
            "job_id": job_id,
            "page_number": pending.page_number,
            "total_pages": pending.total_pages,
            "page_state": pending.state,
            "sfx_mode": if replace_sfx { "replace" } else { "preserve" },
            "translation_items": strict_response_items(&items),
            "preserved_items": preserved_items,
            "auto_preserved_items": auto_preserved_items,
            "lore": self.workflow.read_lore(&pending.job_dir).map_err(|error| {
                FukidashiError::InvalidInput(format!("read job lore: {error}"))
            })?,
            "required_translation_ids": ids,
            "work_token": token,
            "next_action": {
                "tool": "fukidashi_translation_submit",
                "arguments": {
                    "work_token": token,
                },
                "required_ids": ids,
            },
        });
        if !auto_preserved_items.is_empty() {
            value["warnings"] = serde_json::json!([{
                "code": "auto_preserved_unrepresented_text",
                "message": "Legacy analysis had detected text outside the translated handoff. Fukidashi preserved those source pixels automatically so a retry cannot erase them.",
                "items": auto_preserved_items,
            }]);
        }
        emit_page_progress(
            pending.page_number,
            pending.total_pages,
            "Waiting for translations...",
        );
        attach_page_progress(
            &mut value,
            pending.page_number,
            pending.total_pages,
            "Waiting for translations...",
        );
        Ok(value)
    }

    async fn analyze_strict_page(
        &self,
        pending: &PendingPage,
        request: &TranslationStartRequest,
    ) -> Result<serde_json::Value, FukidashiError> {
        let permit = Arc::clone(&self.slots)
            .acquire_owned()
            .await
            .map_err(|error| {
                FukidashiError::RuntimeUnavailable(format!("OCR worker capacity closed: {error}"))
            })?;
        let ocr = Arc::clone(&self.ocr);
        let workflow = Arc::clone(&self.workflow);
        let config = self.config.clone();
        let image_path = pending.source_image.clone();
        let analysis_path = pending.analysis_path.clone();
        let source = request.source_language.clone();
        let target = request.target_language.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut engine = ocr.lock().map_err(|_| {
                FukidashiError::RuntimeUnavailable("OCR session lock poisoned".into())
            })?;
            let operation =
                engine.analyze(&config, &image_path, source.as_deref(), target.as_deref());
            let recycled = engine.finish_heavy_call(config.session_recycle_pages());
            let analysis = match operation {
                Ok(analysis) => analysis,
                Err(error) => {
                    engine.release_sessions();
                    return Err(error);
                }
            };
            // Strict mode hands model memory back after each page.  This is
            // explicit even when the configured recycle boundary already did
            // so, and also covers the boundary value 0.
            let value = match serde_json::to_value(&analysis) {
                Ok(value) => value,
                Err(error) => {
                    engine.release_sessions();
                    return Err(error.into());
                }
            };
            let persist = workflow.write_analysis_artifact(&image_path, &value);
            // Keep detector/recognizer sessions alive until the page's clean
            // and typeset boundary.  The submit path releases them once the
            // page is complete; preflight keeps the same lifecycle across its
            // whole bounded inventory.
            if let Err(error) = persist {
                engine.release_sessions();
                return Err(FukidashiError::Inference(format!(
                    "analysis checkpoint write failed: {error}"
                )));
            }
            let mut value = value;
            value["strict_runtime"] = serde_json::json!({
                "sessions_recycled": recycled,
                "sessions_released": true,
                "analysis_path": analysis_path,
            });
            Ok::<_, FukidashiError>(value)
        })
        .await
        .map_err(|error| FukidashiError::Inference(format!("OCR worker failed: {error}")))?
    }

    async fn complete_preserved_page(
        &self,
        pending: &PendingPage,
        analysis: &serde_json::Value,
        replace_sfx: bool,
    ) -> Result<(), FukidashiError> {
        let mut preserved = analysis.clone();
        if preserved
            .get("translation_handoff")
            .and_then(serde_json::Value::as_object)
            .is_none()
        {
            preserved["translation_handoff"] = serde_json::json!({
                "target_language": preserved
                    .get("target_language")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
                "items": [],
            });
        }
        if preserved["translation_handoff"].get("items").is_none() {
            preserved["translation_handoff"]["items"] = serde_json::json!([]);
        }
        let items = saved_translation_items(&preserved)?;
        if !items.is_empty() {
            let item_values = preserved["translation_handoff"]["items"]
                .as_array_mut()
                .ok_or_else(|| {
                    FukidashiError::InvalidInput(
                        "analysis checkpoint has no mutable translation items".into(),
                    )
                })?;
            for item in item_values {
                item["preserve_by_default"] = serde_json::Value::Bool(true);
                item["keep_source"] = serde_json::Value::Bool(true);
                item["translation"] = serde_json::Value::Null;
                item["status"] = serde_json::Value::String("preserved".into());
            }
        }
        preserved["translation_handoff"]["status"] = serde_json::Value::String("preserved".into());
        let auto_preserved_items = preserved
            .get("strict_v1")
            .and_then(|strict| strict.get("auto_preserved_items"))
            .cloned();
        preserved["strict_v1"] = serde_json::json!({
            "status": "preserved",
            "sfx_mode": if replace_sfx { "replace" } else { "preserve" },
            "preserved_count": items.len(),
        });
        if let Some(auto_preserved_items) = auto_preserved_items {
            preserved["strict_v1"]["auto_preserved_items"] = auto_preserved_items;
        }
        self.workflow
            .write_analysis_artifact(&pending.source_image, &preserved)
            .map_err(|error| {
                FukidashiError::Inference(format!("persist preserved handoff: {error}"))
            })?;

        let (cleaned_path, _, _) = self
            .workflow
            .write_passthrough_clean_artifact(&pending.source_image)
            .map_err(|error| {
                FukidashiError::Inference(format!("create pass-through clean stage: {error}"))
            })?;
        let page = self
            .workflow
            .managed_page(&pending.job_dir, &pending.source_image)
            .map_err(|error| FukidashiError::InvalidInput(error.to_string()))?;
        let workflow = Arc::clone(&self.workflow);
        let job_dir = page.job_dir.clone();
        let rendered_path = page.rendered_image;
        tokio::task::spawn_blocking(move || {
            let _render_lock = workflow
                .acquire_render_lock(&job_dir)
                .map_err(|error| FukidashiError::Inference(error.to_string()))?;
            if rendered_path.is_file() {
                std::fs::remove_file(&rendered_path).map_err(FukidashiError::Io)?;
            }
            let render_sidecar = PathBuf::from(format!(
                "{}{}",
                rendered_path.display(),
                ".fukidashi-render.json"
            ));
            if render_sidecar.is_file() {
                std::fs::remove_file(render_sidecar).map_err(FukidashiError::Io)?;
            }
            let report = crate::typeset::typeset_page(&cleaned_path, &[], &rendered_path).map_err(
                |error| FukidashiError::Inference(format!("pass-through typeset failed: {error}")),
            )?;
            let clean = workflow
                .validate_clean_input(&cleaned_path)
                .map_err(|error| FukidashiError::Inference(error.to_string()))?;
            workflow
                .register_render_locked(
                    &rendered_path,
                    &clean,
                    serde_json::json!({
                        "request_bubbles": [],
                        "report": report,
                    }),
                    serde_json::json!({"strict_v1": true, "passthrough": true}),
                )
                .map_err(|error| {
                    FukidashiError::Inference(format!("register pass-through render: {error}"))
                })?;
            Ok::<_, FukidashiError>(())
        })
        .await
        .map_err(|error| {
            FukidashiError::Inference(format!("pass-through render worker failed: {error}"))
        })??;
        Ok(())
    }

    async fn prepare_strict_page(
        &self,
        pending: PendingPage,
        request: &TranslationStartRequest,
    ) -> Result<serde_json::Value, FukidashiError> {
        let replace_sfx = resolve_sfx_mode(request.sfx_mode.as_deref())?;
        let mut pending = pending;
        let mut auto_preserved_pages = Vec::new();
        loop {
            let analyzing = if pending.analysis_path.is_file() {
                "Resuming saved analysis..."
            } else {
                "Analyzing layout & OCR..."
            };
            emit_page_progress(pending.page_number, pending.total_pages, analyzing);
            let mut analysis = if pending.analysis_path.is_file() {
                read_saved_analysis(&self.workflow, &pending.analysis_path)?
            } else {
                self.analyze_strict_page(&pending, request).await?
            };
            let handoff_before = analysis
                .get("translation_handoff")
                .and_then(|handoff| handoff.get("items"))
                .cloned();
            let auto_preserved = augment_missing_detected_text_items(&mut analysis)?;
            let handoff_changed = handoff_before.as_ref()
                != analysis
                    .get("translation_handoff")
                    .and_then(|handoff| handoff.get("items"));
            if !auto_preserved.is_empty() || handoff_changed {
                self.workflow
                    .write_analysis_artifact(&pending.source_image, &analysis)
                    .map_err(|error| {
                        FukidashiError::Inference(format!(
                            "persist strict source-item handoff: {error}"
                        ))
                    })?;
            }
            let all_items = saved_translation_items(&analysis)?;
            let items = translatable_items(
                &all_items,
                replace_sfx,
                analysis
                    .get("target_language")
                    .and_then(serde_json::Value::as_str),
            );
            if !replace_sfx && items.is_empty() {
                emit_page_progress(
                    pending.page_number,
                    pending.total_pages,
                    "Preserving source page (no dialogue)...",
                );
                self.complete_preserved_page(&pending, &analysis, false)
                    .await?;
                auto_preserved_pages.push(pending.page_number);
                match self.workflow.next_pending_page(&pending.job_dir) {
                    Ok(Some(next)) => {
                        pending = next;
                        continue;
                    }
                    Ok(None) => {
                        // Preserve-only jobs do not pass through the normal
                        // submit boundary, so release any retained OCR
                        // sessions explicitly before exposing review_ready.
                        let _ = self
                            .release_models(Parameters(ReleaseModelsRequest {}))
                            .await;
                        emit_page_progress(
                            pending.total_pages,
                            pending.total_pages,
                            "Review ready",
                        );
                        let mut response = self.strict_review_ready(&pending.job_dir)?;
                        response["auto_preserved_pages"] = serde_json::json!(auto_preserved_pages);
                        attach_page_progress(
                            &mut response,
                            pending.total_pages,
                            pending.total_pages,
                            "Review ready",
                        );
                        return Ok(response);
                    }
                    Err(error) => return Err(FukidashiError::InvalidInput(error.to_string())),
                }
            }
            let source_language = analysis
                .get("source_language")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or_else(|| request.source_language.clone());
            let target_language = analysis
                .get("target_language")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or_else(|| request.target_language.clone());
            let mut response = self.strict_page_response(
                &pending,
                &analysis,
                source_language,
                target_language,
                replace_sfx,
            )?;
            if !auto_preserved_pages.is_empty() {
                response["auto_preserved_pages"] = serde_json::json!(auto_preserved_pages);
            }
            return Ok(response);
        }
    }

    async fn advance_strict_page(
        &self,
        claim: &TranslationClaim,
    ) -> Result<serde_json::Value, FukidashiError> {
        match self.workflow.next_pending_page(&claim.job_dir) {
            Ok(None) => self.strict_review_ready(&claim.job_dir),
            Ok(Some(next)) => {
                let next_request = TranslationStartRequest {
                    image_path: None,
                    job_path: None,
                    job_id: Some(
                        claim
                            .job_dir
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                    ocr_mode: Some("local".into()),
                    source_language: claim.source_language.clone(),
                    target_language: claim.target_language.clone(),
                    scope: None,
                    sfx_mode: Some(if claim.replace_sfx {
                        "replace".into()
                    } else {
                        "preserve".into()
                    }),
                    translate_cover: false,
                };
                self.prepare_strict_page(next, &next_request).await
            }
            Err(error) => Err(FukidashiError::InvalidInput(error.to_string())),
        }
    }

    fn completed_translation_response(
        &self,
        claim: &TranslationClaim,
        response: Result<serde_json::Value, FukidashiError>,
        auto_preserved: bool,
    ) -> CallToolResult {
        match response {
            Ok(mut value) => {
                value["completed_page"] = serde_json::json!(claim.page_number);
                if auto_preserved {
                    value["auto_preserved"] = serde_json::Value::Bool(true);
                    value["pass_through"] = serde_json::Value::Bool(true);
                }
                if value.get("current_page").is_none() {
                    let total = claim.total_pages;
                    emit_page_progress(total, total, "Review ready");
                    attach_page_progress(&mut value, total, total, "Review ready");
                }
                json_result(&value, false)
            }
            Err(error) => {
                let mut value = serde_json::json!({
                    "protocol": "strict-v1",
                    "status": "page_complete",
                    "job_id": claim
                        .job_dir
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default(),
                    "completed_page": claim.page_number,
                    "error": format!("page rendered but next page is not ready: {error}"),
                    "next_action": {
                        "tool": "fukidashi_translation_start",
                        "arguments": {"job_id": claim
                            .job_dir
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()},
                    }
                });
                if auto_preserved {
                    value["auto_preserved"] = serde_json::Value::Bool(true);
                    value["pass_through"] = serde_json::Value::Bool(true);
                }
                attach_page_progress(
                    &mut value,
                    claim.page_number,
                    claim.total_pages,
                    "Page complete",
                );
                // The requested page is already rendered. A failure while
                // preparing the next page is resumable follow-up state, not a
                // failed submission for the completed page.
                json_result(&value, false)
            }
        }
    }

    fn resolve_strict_job(
        &self,
        request: &TranslationStartRequest,
    ) -> Result<PathBuf, FukidashiError> {
        let selectors = [
            request.image_path.is_some(),
            request.job_path.is_some(),
            request.job_id.is_some(),
        ]
        .into_iter()
        .filter(|selected| *selected)
        .count();
        if selectors != 1 {
            return Err(FukidashiError::InvalidInput(
                "provide exactly one of image_path, job_path, or job_id".into(),
            ));
        }
        if let Some(raw) = request.job_path.as_deref() {
            return self
                .workflow
                .resolve_managed_job_path(raw)
                .map_err(|error| FukidashiError::InvalidInput(error.to_string()));
        }
        if let Some(id) = request.job_id.as_deref() {
            return self
                .workflow
                .resolve_managed_job_id(id)
                .map_err(|error| FukidashiError::InvalidInput(error.to_string()));
        }
        let image_path = path(request.image_path.as_deref().unwrap_or_default())?;
        if !image_path.is_file() {
            return Err(FukidashiError::MissingAsset { path: image_path });
        }
        let scope = request
            .scope
            .as_ref()
            .map(resolve_analysis_scope)
            .transpose()?;
        self.workflow
            .register_analysis(&image_path, scope.as_ref())
            .map(|registration| registration.job_dir)
            .map_err(|error| FukidashiError::InvalidInput(error.to_string()))
    }

    fn validate_strict_submissions(
        analysis: &serde_json::Value,
        claim: &TranslationClaim,
        submissions: &[TranslationSubmission],
    ) -> Result<StrictSubmissionPlan, FukidashiError> {
        let all_items = saved_translation_items(analysis)?;
        let items = translatable_items(
            &all_items,
            claim.replace_sfx,
            claim.target_language.as_deref(),
        );
        let index_by_id = items
            .iter()
            .enumerate()
            .map(|(index, item)| (item.id.clone(), index))
            .collect::<BTreeMap<_, _>>();
        if claim.item_ids != index_by_id.keys().cloned().collect() {
            return Err(FukidashiError::InvalidInput(
                "analysis translation IDs changed after the work token was issued; resume with fukidashi_translation_start".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        let mut selected = Vec::with_capacity(items.len());
        for submission in submissions {
            let id = submission.id.trim();
            if id.is_empty() {
                return Err(FukidashiError::InvalidInput(
                    "every translation submission needs a stable id".into(),
                ));
            }
            if submission.id != id {
                return Err(FukidashiError::InvalidInput(format!(
                    "translation id {id:?} must match the server-returned stable id exactly"
                )));
            }
            if !seen.insert(id.to_owned()) {
                return Err(FukidashiError::InvalidInput(format!(
                    "duplicate translation id {id:?}"
                )));
            }
            let index = index_by_id.get(id).ok_or_else(|| {
                FukidashiError::InvalidInput(format!("unknown translation id {id:?}"))
            })?;
            let keep_source = submission.keep_source.unwrap_or(false);
            let needs_review = submission.needs_review.unwrap_or(false);
            if keep_source && submission.translation.is_some() {
                return Err(FukidashiError::InvalidInput(format!(
                    "translation id {id:?} cannot set both keep_source and translation"
                )));
            }
            let text = if keep_source {
                items[*index].source_text.clone()
            } else {
                let text = submission.translation.as_deref().ok_or_else(|| {
                    FukidashiError::InvalidInput(format!(
                        "translation id {id:?} needs translation text or keep_source=true"
                    ))
                })?;
                if text.trim().is_empty() {
                    return Err(FukidashiError::InvalidInput(format!(
                        "translation id {id:?} is empty; use keep_source=true to preserve OCR text"
                    )));
                }
                if translation_is_unchanged(&items[*index], text, claim.target_language.as_deref())
                {
                    return Err(FukidashiError::InvalidInput(format!(
                        "translation id {id:?} is unchanged source-language prose; provide a Vietnamese translation or keep_source=true explicitly"
                    )));
                }
                text.to_owned()
            };
            selected.push(StrictTranslationSelection {
                id: id.to_owned(),
                text,
                keep_source,
                needs_review,
            });
        }
        if seen != claim.item_ids {
            let missing = claim
                .item_ids
                .difference(&seen)
                .cloned()
                .collect::<Vec<_>>();
            let missing_items = missing
                .iter()
                .filter_map(|id| {
                    index_by_id
                        .get(id)
                        .map(|index| serde_json::json!({"id": id, "index": index}))
                })
                .collect::<Vec<_>>();
            return Err(FukidashiError::Diagnostic {
                message: format!(
                    "invalid input: translation submission is incomplete; missing stable ids: {}",
                    missing.join(", ")
                ),
                details: serde_json::json!({
                    "stage": "submit_validation",
                    "code": "missing_translation_items",
                    "missing_items": missing_items,
                    "next_step": "Submit exactly one decision for every required_translation_ids entry; use keep_source=true for any item that should remain unchanged, then retry the same work_token.",
                }),
            });
        }
        Ok(StrictSubmissionPlan {
            all_items,
            items,
            selected,
        })
    }

    fn apply_strict_handoff(
        analysis: &mut serde_json::Value,
        claim: &TranslationClaim,
        plan: &StrictSubmissionPlan,
    ) -> Result<(), FukidashiError> {
        let item_values = analysis
            .get_mut("translation_handoff")
            .and_then(|handoff| handoff.get_mut("items"))
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| {
                FukidashiError::InvalidInput(
                    "analysis checkpoint has no mutable translation items".into(),
                )
            })?;
        for selection in &plan.selected {
            let item = item_values
                .iter_mut()
                .find(|item| {
                    item.get("id").and_then(serde_json::Value::as_str)
                        == Some(selection.id.as_str())
                })
                .ok_or_else(|| {
                    FukidashiError::InvalidInput(format!(
                        "translation id {:?} disappeared",
                        selection.id
                    ))
                })?;
            item["translation"] = serde_json::Value::String(selection.text.clone());
            item["keep_source"] = serde_json::Value::Bool(selection.keep_source);
            item["needs_review"] = serde_json::Value::Bool(selection.needs_review);
            item["status"] = serde_json::Value::String(
                if selection.needs_review {
                    "needs_review"
                } else if selection.keep_source {
                    "keep_source"
                } else {
                    "translated"
                }
                .into(),
            );
        }
        if !claim.replace_sfx {
            for item in item_values.iter_mut() {
                let selected = plan.selected.iter().find(|selection| {
                    selection.id
                        == item
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                });
                if selected.is_some() {
                    continue;
                }
                let preserve = item
                    .get("preserve_by_default")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or_else(|| {
                        item.get("kind")
                            .and_then(serde_json::Value::as_str)
                            .map(|kind| kind == "unmatched_text")
                            .or_else(|| {
                                item.get("id")
                                    .and_then(serde_json::Value::as_str)
                                    .map(|id| id.starts_with("text-"))
                            })
                            .unwrap_or(false)
                    });
                if preserve {
                    item["preserve_by_default"] = serde_json::Value::Bool(true);
                    item["keep_source"] = serde_json::Value::Bool(true);
                    item["translation"] = serde_json::Value::Null;
                    item["status"] = serde_json::Value::String("preserved".into());
                }
            }
        }
        let auto_preserved_items = analysis
            .get("strict_v1")
            .and_then(|strict| strict.get("auto_preserved_items"))
            .cloned();
        analysis["translation_handoff"]["status"] = serde_json::Value::String("submitted".into());
        analysis["strict_v1"] = serde_json::json!({
            "status": "submitted",
            "sfx_mode": if claim.replace_sfx { "replace" } else { "preserve" },
            "preserved_count": if claim.replace_sfx {
                0
            } else {
                plan.all_items
                    .iter()
                    .filter(|item| item.preserve_by_default)
                    .count()
            },
        });
        if let Some(auto_preserved_items) = auto_preserved_items {
            analysis["strict_v1"]["auto_preserved_items"] = auto_preserved_items;
        }
        Ok(())
    }

    fn strict_typeset_payloads(
        analysis: &serde_json::Value,
        plan: &StrictSubmissionPlan,
    ) -> Result<Vec<TypesetPayload>, FukidashiError> {
        let bubble_bboxes = plan
            .items
            .iter()
            .map(|item| {
                analysis_bubble_bbox(analysis, &item.id).map(|bbox| (item.id.clone(), bbox))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let item_values = analysis
            .get("translation_handoff")
            .and_then(|handoff| handoff.get("items"))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                FukidashiError::InvalidInput("analysis checkpoint has no translation items".into())
            })?;
        let mut payloads = Vec::with_capacity(plan.selected.len());
        for selection in &plan.selected {
            let item = item_values
                .iter()
                .find(|item| {
                    item.get("id").and_then(serde_json::Value::as_str)
                        == Some(selection.id.as_str())
                })
                .ok_or_else(|| {
                    FukidashiError::InvalidInput(format!(
                        "translation id {:?} disappeared",
                        selection.id
                    ))
                })?;
            let raw_bbox = item.get("bbox").cloned().unwrap_or(serde_json::Value::Null);
            let bbox = serde_json::from_value::<Rect>(raw_bbox)
                .map_err(|error| {
                    FukidashiError::InvalidInput(format!(
                        "translation item {:?} has invalid bbox: {error}",
                        selection.id
                    ))
                })?
                .validate()?;
            let bubble_bbox = bubble_bboxes.get(&selection.id).copied().flatten();
            payloads.push(TypesetPayload {
                id: Some(selection.id.clone()),
                source_text: item
                    .get("source_text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                kind: item
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                preserve_by_default: item
                    .get("preserve_by_default")
                    .and_then(serde_json::Value::as_bool),
                needs_review: Some(selection.needs_review),
                // Model uncertainty is advisory. Only an explicit editor flag
                // or problem is an approval blocker.
                flagged: None,
                preserve_source: Some(selection.keep_source),
                fallback_font_paths: Vec::new(),
                bbox,
                bubble_bbox,
                text_bbox: None,
                padding: None,
                min_font_size: None,
                max_font_size: None,
                text: selection.text.clone(),
                font_path: None,
                requested_font_path: None,
                text_color: item
                    .get("text_color")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                shape: None,
            });
        }
        Ok(payloads)
    }

    #[cfg(test)]
    fn apply_strict_translations(
        analysis: &mut serde_json::Value,
        claim: &TranslationClaim,
        submissions: &[TranslationSubmission],
    ) -> Result<Vec<TypesetPayload>, FukidashiError> {
        let plan = Self::validate_strict_submissions(analysis, claim, submissions)?;
        // Validate all geometry before mutating the handoff.  This preserves
        // retry semantics for a normal/partial submission while allowing the
        // caller to deliberately skip this step for all-keep pass-through.
        let payloads = Self::strict_typeset_payloads(analysis, &plan)?;
        Self::apply_strict_handoff(analysis, claim, &plan)?;
        Ok(payloads)
    }
}

fn resolve_typeset_request(req: TypesetRequest) -> (String, Vec<TypesetPayload>, Vec<String>) {
    let bubbles = req
        .bubbles
        .into_iter()
        .map(|mut bubble| {
            if bubble.requested_font_path.is_none() && bubble.font_path.is_some() {
                bubble.requested_font_path.clone_from(&bubble.font_path);
            }
            if bubble.font_path.is_none() {
                bubble.font_path.clone_from(&req.font_path);
            }
            if bubble.padding.is_none() {
                bubble.padding = req.padding;
            }
            if bubble.min_font_size.is_none() {
                bubble.min_font_size = req.min_font_size;
            }
            if bubble.max_font_size.is_none() {
                bubble.max_font_size = req.max_font_size;
            }
            if bubble.shape.is_none() {
                bubble.shape.clone_from(&req.shape);
            }
            bubble
        })
        .collect();
    (req.image_path, bubbles, req.fallback_font_paths)
}

/// Resolve the historical effective global font for a typeset request: the
/// managed/materialized path the renderer actually used, not the raw operator
/// request. Generic desktop primaries substitute to the managed bundled Comic
/// Neue (mirroring the per-bubble substitution); other requests materialize
/// into the managed job fonts directory. Returns the raw request when it
/// cannot be materialized (the render then never depended on it because every
/// bubble carried an explicit font or fell back to the bundled default).
fn effective_global_font_path(
    workflow: &Workflow,
    job_root: &std::path::Path,
    requested: Option<&str>,
) -> Option<String> {
    let requested = requested.filter(|path| !path.trim().is_empty())?;
    if crate::workflow::is_generic_desktop_font(std::path::Path::new(requested)) {
        return workflow
            .materialize_bundled_font(job_root, &crate::fonts::COMIC_NEUE_REGULAR)
            .map(|path| path.display().to_string())
            .ok();
    }
    workflow
        .materialize_font_path(job_root, std::path::Path::new(requested))
        .map(|path| path.display().to_string())
        .ok()
        .or_else(|| Some(requested.to_owned()))
}

fn materialize_bundled_typeset_fonts(
    workflow: &Workflow,
    job_root: &std::path::Path,
    bubbles: &mut [TypesetPayload],
) -> anyhow::Result<(Vec<String>, Vec<serde_json::Value>)> {
    if bubbles.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let regular = workflow.materialize_bundled_font(job_root, &crate::fonts::COMIC_NEUE_REGULAR)?;
    let regular_path = regular.display().to_string();
    let mut substitutions = Vec::new();
    for (index, bubble) in bubbles.iter_mut().enumerate() {
        let Some(requested) = bubble.font_path.as_deref() else {
            bubble.font_path = Some(regular_path.clone());
            continue;
        };
        if crate::workflow::is_generic_desktop_font(std::path::Path::new(requested)) {
            substitutions.push(serde_json::json!({
                "index": index,
                "requested_font_path": requested,
                "replacement_primary_font": regular_path,
                "reason": "generic_desktop_primary",
            }));
            bubble.font_path = Some(regular_path.clone());
        }
    }
    let fallbacks = crate::fonts::bundled_fallbacks()
        .into_iter()
        .map(|font| {
            workflow
                .materialize_bundled_font(job_root, font)
                .map(|path| path.display().to_string())
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((fallbacks, substitutions))
}

fn resolve_analysis_scope(scope: &AnalyzeScope) -> Result<ScopeSpec, FukidashiError> {
    let include_paths = scope
        .include_paths
        .as_ref()
        .map(|paths| {
            paths
                .iter()
                .map(|path| path.as_str())
                .map(crate::mcp::path)
                .collect()
        })
        .transpose()?;
    Ok(ScopeSpec {
        start_page: scope.start_page,
        end_page: scope.end_page,
        include_paths,
    })
}

fn rects_overlap(left: Rect, right: Rect) -> bool {
    left.x1 < right.x2 && right.x1 < left.x2 && left.y1 < right.y2 && right.y1 < left.y2
}

fn rect_contains(outer: Rect, inner: Rect) -> bool {
    outer.x1 <= inner.x1 && outer.y1 <= inner.y1 && outer.x2 >= inner.x2 && outer.y2 >= inner.y2
}

fn json_bbox(item: &serde_json::Value) -> Option<Rect> {
    serde_json::from_value::<Rect>(item.get("bbox").cloned()?)
        .ok()
        .and_then(|rect| rect.validate().ok())
}

/// A text line fully inside a detected speech-bubble contour is another OCR
/// view of that bubble only when its OCR text is already represented by the
/// matching handoff item. Requiring the detector contour, item, text, and
/// containment prevents broad rectangles from hiding independent text.
fn detected_bubble_handoff_covers_line(
    bubbles: &[serde_json::Value],
    items: &[serde_json::Value],
    line_rect: Rect,
    line_text: &str,
    line_language: &str,
    line_confidence: f64,
) -> bool {
    bubbles.iter().any(|bubble| {
        if bubble
            .get("detector_label")
            .and_then(serde_json::Value::as_i64)
            != Some(0)
        {
            return false;
        }
        let Some(id) = bubble.get("id").and_then(serde_json::Value::as_str) else {
            return false;
        };
        let Some(bubble_rect) = json_bbox(bubble) else {
            return false;
        };
        if !rect_contains(bubble_rect, line_rect) {
            return false;
        }
        items.iter().any(|item| {
            if item.get("id").and_then(serde_json::Value::as_str) != Some(id) {
                return false;
            }
            if item
                .get("keep_source")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                || item.get("kind").and_then(serde_json::Value::as_str) == Some("unmatched_text")
            {
                return false;
            }
            let source_text = item
                .get("source_text")
                .or_else(|| item.get("ocr_text"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let source_language = item
                .get("source_language")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(line_language);
            let cjk_fragment_match = cjk_fragment_is_covered(source_text, line_text);
            let lower_confidence_crop = line_confidence <= 0.25
                && cjk_text_char_count(source_text) >= 3
                && cjk_text_char_count(line_text) >= 2
                && bubble
                    .get("confidence")
                    .and_then(serde_json::Value::as_f64)
                    .is_some_and(|confidence| confidence >= line_confidence + 0.15);
            (source_text_covers_line(source_text, line_text, source_language)
                || cjk_fragment_match
                || lower_confidence_crop)
                && json_bbox(item).is_some_and(|item_rect| rect_contains(item_rect, line_rect))
        })
    })
}

fn cjk_text_char_count(text: &str) -> usize {
    text.chars()
        .filter(|character| {
            matches!(
                character,
                '\u{3040}'..='\u{30ff}'
                    | '\u{3400}'..='\u{4dbf}'
                    | '\u{4e00}'..='\u{9fff}'
                    | '\u{f900}'..='\u{faff}'
                    | '\u{ac00}'..='\u{d7af}'
            )
        })
        .count()
}

/// Short CJK OCR crops are often a fragment of the better bubble-level OCR.
/// Treat a two-or-more-character substring as represented only after geometry
/// has confirmed that the crop sits inside the same detected bubble and item.
fn cjk_fragment_is_covered(source: &str, line: &str) -> bool {
    let fragment = normalized_source_text(line);
    cjk_text_char_count(line) >= 2
        && cjk_text_char_count(line) == line.chars().filter(|ch| ch.is_alphanumeric()).count()
        && !fragment.is_empty()
        && normalized_source_text(source).contains(&fragment)
}

fn analysis_bubble_bbox(
    analysis: &serde_json::Value,
    id: &str,
) -> Result<Option<Rect>, FukidashiError> {
    let Some(bubbles) = analysis
        .get("bubbles")
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(None);
    };
    let Some(bubble) = bubbles
        .iter()
        .find(|bubble| bubble.get("id").and_then(serde_json::Value::as_str) == Some(id))
    else {
        return Ok(None);
    };
    // Detector label 0 is the speech-bubble contour. Promoted OCR lines use
    // their text rectangle as a synthetic bubble, which must not masquerade
    // as a contour for strict typesetting geometry.
    if bubble
        .get("detector_label")
        .and_then(serde_json::Value::as_i64)
        != Some(0)
    {
        return Ok(None);
    }
    let bbox = bubble.get("bbox").cloned().ok_or_else(|| {
        FukidashiError::InvalidInput(format!("analysis bubble {id:?} has no bbox"))
    })?;
    let bbox = serde_json::from_value::<Rect>(bbox)
        .map_err(|error| {
            FukidashiError::InvalidInput(format!("analysis bubble {id:?} bbox is invalid: {error}"))
        })?
        .validate()?;
    Ok(Some(bbox))
}

fn checkpoint_item_is_preserved(item: &serde_json::Value, replace_sfx: bool) -> bool {
    let keep_source = item
        .get("keep_source")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if keep_source {
        return true;
    }
    if item.get("kind").and_then(serde_json::Value::as_str) == Some("prose-line") {
        return false;
    }
    if replace_sfx {
        return false;
    }
    item.get("preserve_by_default")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || item
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| kind == "unmatched_text")
        || item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| id.starts_with("text-"))
}

fn checkpoint_item_is_translatable_dialogue(
    item: &serde_json::Value,
    replace_sfx: bool,
    target_language: Option<&str>,
) -> bool {
    if item
        .get("keep_source")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }
    if replace_sfx {
        return true;
    }
    if !replace_sfx
        && item
            .get("preserve_by_default")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        && !(is_vietnamese_target(target_language)
            && is_english_prose(
                item.get("source_language")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("auto"),
                item.get("source_text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
            ))
    {
        return false;
    }
    if checkpoint_item_is_preserved(item, replace_sfx)
        && !(is_vietnamese_target(target_language)
            && is_english_prose(
                item.get("source_language")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("auto"),
                item.get("source_text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
            ))
    {
        return false;
    }
    let id = item
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let kind = item
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("dialogue");
    kind == "prose-line"
        || !id.starts_with("text-") && kind != "unmatched_text"
        || is_vietnamese_target(target_language)
            && is_english_prose(
                item.get("source_language")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("auto"),
                item.get("source_text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
            )
}

fn checkpoint_text_regions(
    path: &std::path::Path,
    replace_sfx: bool,
) -> Result<Vec<Rect>, FukidashiError> {
    const MAX_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_CHECKPOINT_BYTES {
        return Err(FukidashiError::ResourceLimit(
            "analysis checkpoint is missing or exceeds 16 MiB".into(),
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let handoff_items = value
        .get("translation_handoff")
        .and_then(|handoff| handoff.get("items"))
        .and_then(serde_json::Value::as_array)
        .map(|items| items.as_slice())
        .unwrap_or(&[]);
    let detected_bubbles = value
        .get("bubbles")
        .and_then(serde_json::Value::as_array)
        .map(|bubbles| bubbles.as_slice())
        .unwrap_or(&[]);
    let mut preserved_bboxes = Vec::new();
    let mut translatable_bboxes = Vec::new();
    let target_language = value
        .get("target_language")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .get("translation_handoff")
                .and_then(|handoff| handoff.get("target_language"))
                .and_then(serde_json::Value::as_str)
        });
    for item in handoff_items {
        let Some(rect) = json_bbox(item) else {
            continue;
        };
        if checkpoint_item_is_translatable_dialogue(item, replace_sfx, target_language) {
            let item_id = item
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown-item")
                .to_owned();
            let source_text = item
                .get("source_text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let source_language = item
                .get("source_language")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("auto")
                .to_owned();
            // Geometry-only coverage is reserved for an unmatched/manual
            // source anchor.  A normal detector dialogue with a broad box
            // still needs matching OCR text, or it could hide an unrelated
            // prose line merely because the rectangles overlap.
            let geometry_anchor = item
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind == "unmatched_text")
                || item
                    .get("preserve_by_default")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                || item
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|id| id.starts_with("text-"));
            translatable_bboxes.push((
                rect,
                item_id,
                source_text,
                source_language,
                geometry_anchor,
            ));
        } else if checkpoint_item_is_preserved(item, replace_sfx) {
            preserved_bboxes.push((
                rect,
                item.get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                item.get("auto_preserved")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            ));
        }
    }
    let lines = value
        .get("text_lines")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            FukidashiError::InvalidInput("analysis checkpoint has no text_lines array".into())
        })?;
    let mut regions = Vec::with_capacity(lines.len());
    for (line_index, line) in lines.iter().enumerate() {
        let rect = serde_json::from_value::<Rect>(line.get("bbox").cloned().unwrap_or_default())
            .map_err(FukidashiError::from)
            .and_then(Rect::validate)?;
        let line_text = line
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let line_language = line
            .get("source_language")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("auto");
        let line_confidence = line
            .get("confidence")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(1.0);
        let bubble_coverage = detected_bubble_handoff_covers_line(
            detected_bubbles,
            handoff_items,
            rect,
            line_text,
            line_language,
            line_confidence,
        );
        let line_id = line
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let automatic_preservation = preserved_bboxes
            .iter()
            .any(|(_, id, automatic)| *automatic && id == line_id);
        // An explicit preserved detector item wins over a broad dialogue
        // rectangle that happens to overlap it.  This is how a low-confidence
        // uncovered line remains source pixels without blocking the rest of
        // the bubble's translated lines from being cleaned. Auto-preserved
        // legacy crops are different: when a better bubble OCR item fully
        // contains the low-confidence crop, the translated bubble owns it.
        if preserved_bboxes.iter().any(|(preserved, _, _)| {
            rects_overlap(rect, *preserved)
                && rect_contains(*preserved, rect)
                && rect_contains(rect, *preserved)
        }) && !(automatic_preservation && bubble_coverage)
        {
            continue;
        }
        let overlapping_translatable = translatable_bboxes
            .iter()
            .filter(|(bubble, _, _, _, _)| rects_overlap(rect, *bubble))
            .collect::<Vec<_>>();
        if !overlapping_translatable.is_empty() {
            if overlapping_translatable.iter().any(
                |(bubble, _, source, language, geometry_anchor)| {
                    source_text_covers_line(source, line_text, language)
                        || bubble_coverage
                        || (*geometry_anchor && rect_contains(*bubble, rect))
                },
            ) {
                regions.push(rect);
                continue;
            }
            let overlapping_item_ids = overlapping_translatable
                .iter()
                .map(|(_, item_id, _, _, _)| item_id.clone())
                .collect::<Vec<_>>();
            let details = serde_json::json!({
                "stage": "strict_clean",
                "code": "unrepresented_detected_text",
                "detected_line_index": line_index,
                "detected_line_id": line.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "detected_text": line_text,
                "source_language": line
                    .get("source_language")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("auto"),
                "confidence": line.get("confidence").cloned().unwrap_or(serde_json::Value::Null),
                "bbox": rect,
                "overlapping_item_ids": overlapping_item_ids,
                "next_step": "Correct the translation handoff for the detected line, or submit keep_source=true for the affected item and retry the same work_token; the server will not erase an unrepresented region.",
            });
            return Err(FukidashiError::Diagnostic {
                message: "invalid input: detected text inside a translated region is not represented by its source item; refusing destructive cleaning".into(),
                details,
            });
        }
        // Low-confidence unmatched detections are commonly art/sfx geometry
        // (for example the dots on a chastity device), not text strokes.  Do
        // not feed those broad boxes into a destructive cleaning mask.
        let confident = line
            .get("confidence")
            .and_then(serde_json::Value::as_f64)
            .map(|confidence| confidence >= 0.5)
            .unwrap_or(true);
        if !confident {
            continue;
        }
        let source_language = line
            .get("source_language")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("auto");
        if is_vietnamese_target(target_language) && is_english_prose(source_language, line_text) {
            let details = serde_json::json!({
                "stage": "strict_clean",
                "code": "unmatched_sentence_like_text",
                "detected_line_index": line_index,
                "detected_line_id": line.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "detected_text": line_text,
                "source_language": source_language,
                "confidence": line.get("confidence").cloned().unwrap_or(serde_json::Value::Null),
                "bbox": rect,
                "next_step": "Add the detected sentence to the translation handoff or explicitly preserve it with keep_source=true, then retry the same work_token; the server will not silently pass it through.",
            });
            return Err(FukidashiError::Diagnostic {
                message: "invalid input: detected sentence-like text has no translated item; refusing destructive cleaning".into(),
                details,
            });
        }
        // A line without a matching item may be a sound effect or artwork.
        // Leave it in the source image; only item-backed lines may enter a
        // destructive clean mask.
    }
    Ok(regions)
}

fn source_text_covers_line(source: &str, line: &str, source_language: &str) -> bool {
    if line.trim().is_empty() || source.trim().is_empty() {
        return false;
    }
    let source_normalized = normalized_source_text(source);
    let line_normalized = normalized_source_text(line);
    if !line_normalized.is_empty() && source_normalized == line_normalized {
        return true;
    }
    if line_normalized.len() < 8 {
        return false;
    }
    if source_normalized.contains(&line_normalized) {
        return true;
    }
    if !is_english_prose(source_language, line) {
        return false;
    }
    let source_words = source
        .split_whitespace()
        .map(|word| word.trim_matches(|character: char| !character.is_alphanumeric()))
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let line_words = line
        .split_whitespace()
        .map(|word| word.trim_matches(|character: char| !character.is_alphanumeric()))
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if line_words.len() < 2 {
        return false;
    }
    let matching = line_words
        .iter()
        .filter(|word| source_words.iter().any(|candidate| candidate == *word))
        .count();
    matching * 4 >= line_words.len() * 3
}

#[tool_router]
impl FukidashiServer {
    #[tool(
        name = "fukidashi_get_lore",
        description = "Read canonical lore for a managed job before the first submit. An untouched job returns {schema:1,characters:[],pronouns:[],glossary:[]}; character strings such as [\"Fuyu\"] are accepted by fukidashi_put_lore and returned as stable {id,names,notes} entries."
    )]
    pub async fn get_lore(&self, Parameters(req): Parameters<LoreRequest>) -> CallToolResult {
        let job = match self.workflow.resolve_managed_job_id(&req.job_id) {
            Ok(job) => job,
            Err(error) => {
                return json_result(&serde_json::json!({"error": error.to_string()}), true);
            }
        };
        match self.workflow.read_lore(&job) {
            Ok(lore) => json_result(
                &serde_json::json!({"job_id": req.job_id, "lore": lore}),
                false,
            ),
            Err(error) => json_result(
                &serde_json::json!({"job_id": req.job_id, "error": error.to_string()}),
                true,
            ),
        }
    }

    #[tool(
        name = "fukidashi_translation_preflight",
        description = "Run one bounded, resumable 1/N OCR preflight over the managed page inventory. Reuses existing analysis checkpoints, keeps one hot OCR session for new pages, reports visible page progress, classifies each page as bubble/prose/mixed/skip, and persists a routing manifest for later translation_start calls. Every result and progress event reports reused_cached_pages, newly_processed_pages, total_pages, within_job_cached_pages, cross_job_cached_pages (always zero because no cross-job cache exists), and pass-through/skip counts; the human cache message is Reused cached pages: X/N; newly processed: Y/N. Cover-like page 1 is skip/preserve by default; pass translate_cover=true to opt in. Dense prose and mixed pages keep every unrepresented source block for explicit preservation. Translation and cleaning still proceed through the serial strict-v1 submit loop. source_language accepts only auto, ja, zh, ko, en, or latin; legacy pipe values such as en|latin are normalized to automatic routing. Call this once before repeated fukidashi_translation_start calls when speed and visible page progress matter."
    )]
    pub async fn translation_preflight(
        &self,
        Parameters(req): Parameters<TranslationPreflightRequest>,
    ) -> CallToolResult {
        let mut req = req;
        if let Some(mode) = req.ocr_mode.as_deref()
            && !matches!(mode, "local" | "auto")
        {
            return json_result(
                &serde_json::json!({"protocol":"strict-v1","error":"ocr_mode must be local or auto"}),
                true,
            );
        }
        if let Err(error) = resolve_sfx_mode(req.sfx_mode.as_deref()) {
            return json_result(
                &serde_json::json!({"protocol":"strict-v1","error":error.to_string()}),
                true,
            );
        }
        req.source_language = match normalize_requested_source_language(req.source_language.take())
        {
            Ok(value) => value,
            Err(error) => {
                return json_result(
                    &serde_json::json!({
                        "protocol": "strict-v1",
                        "error": error.to_string(),
                        "next_step": "use source_language=auto, ja, zh, ko, en, or latin"
                    }),
                    true,
                );
            }
        };
        let selectors = [
            req.image_path.is_some(),
            req.job_path.is_some(),
            req.job_id.is_some(),
        ]
        .into_iter()
        .filter(|selected| *selected)
        .count();
        if selectors != 1 {
            return json_result(
                &serde_json::json!({
                    "protocol":"strict-v1",
                    "error":"provide exactly one of image_path, job_path, or job_id"
                }),
                true,
            );
        }
        if req.scope.is_some() && (req.job_path.is_some() || req.job_id.is_some()) {
            return json_result(
                &serde_json::json!({"protocol":"strict-v1","error":"scope is accepted only with image_path"}),
                true,
            );
        }
        let job = if let Some(raw) = req.image_path.as_deref() {
            let source = match path(raw) {
                Ok(source) if source.is_file() => source,
                Ok(source) => {
                    return json_result(
                        &serde_json::json!({"protocol":"strict-v1","error":FukidashiError::MissingAsset { path: source }.to_string()}),
                        true,
                    );
                }
                Err(error) => {
                    return json_result(&serde_json::json!({"error":error.to_string()}), true);
                }
            };
            let scope = match req.scope.as_ref().map(resolve_analysis_scope).transpose() {
                Ok(scope) => scope,
                Err(error) => {
                    return json_result(&serde_json::json!({"error":error.to_string()}), true);
                }
            };
            match self.workflow.register_analysis(&source, scope.as_ref()) {
                Ok(registration) => registration.job_dir,
                Err(error) => {
                    return json_result(
                        &serde_json::json!({"protocol":"strict-v1","error":format!("unable to register managed page scope: {error}")}),
                        true,
                    );
                }
            }
        } else if let Some(raw) = req.job_path.as_deref() {
            match self.workflow.resolve_managed_job_path(raw) {
                Ok(job) => job,
                Err(error) => {
                    return json_result(&serde_json::json!({"error":error.to_string()}), true);
                }
            }
        } else {
            match self
                .workflow
                .resolve_managed_job_id(req.job_id.as_deref().unwrap_or_default())
            {
                Ok(job) => job,
                Err(error) => {
                    return json_result(&serde_json::json!({"error":error.to_string()}), true);
                }
            }
        };
        let sources = match self.workflow.managed_source_paths_for_editor(&job) {
            Ok(sources) => sources,
            Err(error) => {
                return json_result(&serde_json::json!({"error":error.to_string()}), true);
            }
        };
        let total_pages = sources.len();
        let ocr = Arc::clone(&self.ocr);
        let workflow = Arc::clone(&self.workflow);
        let config = self.config.clone();
        let source_language = req.source_language.clone();
        let target_language = req.target_language.clone();
        let translate_cover = req.translate_cover;
        let effective_target_language = target_language
            .clone()
            .unwrap_or_else(|| config.configured_target_language());
        // Preflight artifacts depend on every setting that can change routing,
        // OCR output, or the generated handoff. Keep the structured context in
        // the manifest/checkpoint so older artifacts without it fail closed.
        let preflight_context = serde_json::json!({
            "source_language": source_language.as_deref().unwrap_or("auto"),
            "target_language": effective_target_language,
            "translate_cover": translate_cover,
            "ocr_mode": req.ocr_mode.as_deref().unwrap_or("auto"),
            "sfx_mode": req.sfx_mode.as_deref().unwrap_or("preserve"),
            "models_dir": config.models_dir.to_string_lossy(),
            "provider": config.provider(),
        });
        let prior_preflight_context = match self.workflow.read_preflight_manifest(&job) {
            Ok(manifest) => {
                manifest.and_then(|manifest| manifest.get("preflight_context").cloned())
            }
            Err(error) => {
                return json_result(
                    &serde_json::json!({"protocol":"strict-v1","error":format!("unable to validate existing preflight context: {error}")}),
                    true,
                );
            }
        };
        if prior_preflight_context.as_ref() != Some(&preflight_context) {
            let has_downstream_stages = match self.workflow.managed_page_records(&job) {
                Ok(pages) => pages.iter().any(|page| {
                    matches!(
                        page.get("state").and_then(serde_json::Value::as_str),
                        Some("cleaned" | "rendered")
                    )
                }),
                Err(error) => {
                    return json_result(
                        &serde_json::json!({"protocol":"strict-v1","error":format!("unable to inspect managed page stages: {error}")}),
                        true,
                    );
                }
            };
            if has_downstream_stages {
                return json_result(
                    &serde_json::json!({
                        "protocol":"strict-v1",
                        "error":"preflight context changed after this job already has cleaned or rendered pages; use a fresh job for the new source/target language, cover, OCR, SFX, or model settings",
                        "next_step":"start a fresh managed job with the desired preflight settings; the existing job and its artifacts are unchanged"
                    }),
                    true,
                );
            }
        }
        let job_for_worker = job.clone();
        let job_id = job
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        let job_id_for_worker = job_id.clone();
        let preflight_context_for_response = preflight_context.clone();
        let permit = match Arc::clone(&self.slots).acquire_owned().await {
            Ok(permit) => permit,
            Err(error) => {
                return json_result(
                    &serde_json::json!({"protocol":"strict-v1","error":format!("OCR worker capacity closed: {error}")}),
                    true,
                );
            }
        };
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut engine = ocr.lock().map_err(|_| {
                FukidashiError::RuntimeUnavailable("OCR session lock poisoned".into())
            })?;
            let previous_manifest = workflow
                .read_preflight_manifest(&job_for_worker)
                .ok()
                .flatten();
            let preflight_context_for_worker = preflight_context.clone();
            let operation = (|| {
                let mut pages = Vec::with_capacity(sources.len());
                // Persist an empty, resumable marker before the first model
                // call. Each completed page is then committed immediately so
                // a client cancellation or model failure never loses all
                // completed routing work.
                let persist_progress = |pages: &[serde_json::Value]| {
                    let mut partial = serde_json::json!({
                        "protocol": "strict-v1",
                        "status": "preflight_partial",
                        "job_id": job_id_for_worker,
                        "total_pages": total_pages,
                        "translate_cover": translate_cover,
                        "preflight_context": preflight_context_for_worker,
                        "pages": pages,
                        "next_action": {"tool":"fukidashi_translation_preflight","arguments":{"job_id":job_id_for_worker}},
                    });
                    attach_preflight_cache_telemetry(&mut partial, pages, total_pages);
                    workflow.write_preflight_manifest(&job_for_worker, &partial)
                    .map(|_| ())
                    .map_err(|error| {
                        FukidashiError::Inference(format!(
                            "persist resumable preflight manifest: {error}"
                        ))
                    })
                };
                persist_progress(&pages)?;
                for (index, source) in sources.iter().enumerate() {
                    let page_number = index + 1;
                    emit_preflight_progress(
                        page_number,
                        total_pages,
                        "Preflight OCR & route...",
                        &pages,
                    );
                    let managed = workflow
                        .managed_page(&job_for_worker, source)
                        .map_err(|error| FukidashiError::InvalidInput(error.to_string()))?;
                    let cached = managed.analysis_path.is_file();
                    let source_hash = workflow
                        .source_file_sha256(source)
                        .map_err(|error| FukidashiError::InvalidInput(error.to_string()))?;
                    let saved = if cached {
                        Some(read_saved_analysis(&workflow, &managed.analysis_path)?)
                    } else {
                        None
                    };
                    let cache_matches_source = saved.as_ref().is_some_and(|analysis| {
                        analysis
                            .get("preflight_source_sha256")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|hash| hash == source_hash)
                            && analysis.get("preflight_context")
                                == Some(&preflight_context_for_worker)
                    });
                    let previous_context_matches = previous_manifest
                        .as_ref()
                        .and_then(|manifest| manifest.get("preflight_context"))
                        == Some(&preflight_context_for_worker);
                    if previous_context_matches && cache_matches_source {
                        if let Some(previous) = previous_manifest
                        .as_ref()
                        .and_then(|manifest| manifest.get("pages"))
                        .and_then(serde_json::Value::as_array)
                        .and_then(|pages| {
                            pages.iter().find(|page| {
                                page.get("page_number").and_then(serde_json::Value::as_u64)
                                    == Some(page_number as u64)
                                    && page
                                        .get("source_sha256")
                                        .and_then(serde_json::Value::as_str)
                                        == Some(source_hash.as_str())
                                    && page.get("preflight_context")
                                        == Some(&preflight_context_for_worker)
                            })
                        })
                        {
                            emit_preflight_progress(
                                page_number,
                                total_pages,
                                "Preflight cache hit...",
                                &pages,
                            );
                            let mut cached_page = previous.clone();
                            cached_page["cache_hit"] = serde_json::Value::Bool(true);
                            cached_page["cache_source"] =
                                serde_json::Value::String("within_job_manifest".into());
                            cached_page["pass_through"] = serde_json::Value::Bool(
                                cached_page.get("classification").and_then(serde_json::Value::as_str)
                                    == Some("skip"),
                            );
                            cached_page["skip"] = cached_page["pass_through"].clone();
                            pages.push(cached_page);
                            persist_progress(&pages)?;
                            continue;
                        }
                    }
                    let reused = cache_matches_source;
                    let mut analysis = if cache_matches_source {
                        saved.expect("cache match implies saved analysis")
                    } else {
                        let route = engine.thumbnail_route(&config, source)?;
                        if route.classification == "bubble" && !cached {
                            let pending = serde_json::json!({
                                "preflight_page_kind": "bubble",
                                "bubbles": [],
                                "text_lines": [],
                                "unmatched_text": [],
                                "translation_handoff": {"items": []},
                            });
                            pages.push(preflight_page_manifest(
                                page_number,
                                source,
                                &source_hash,
                                &pending,
                                false,
                            ));
                            if let Some(page) = pages.last_mut() {
                                page["bubble_count"] = serde_json::json!(route.bubble_count);
                                page["text_line_count"] = serde_json::json!(route.line_count);
                            }
                            persist_progress(&pages)?;
                            continue;
                        }
                        let mut value = if route.classification == "skip" {
                            // A sparse title/cover has no safe text region;
                            // strict preserve mode will create its verified
                            // pass-through stage without a fabricated box.
                            synthetic_skip_analysis(
                                source_language.as_deref(),
                                target_language.as_deref(),
                                &config,
                                false,
                            )
                        } else {
                            let analysis = engine.analyze(
                                &config,
                                source,
                                source_language.as_deref(),
                                target_language.as_deref(),
                            )?;
                            let value = serde_json::to_value(analysis)?;
                            if !translate_cover && cover_like_analysis(page_number, &value) {
                                synthetic_skip_analysis(
                                    source_language.as_deref(),
                                    target_language.as_deref(),
                                    &config,
                                    true,
                                )
                            } else {
                                value
                            }
                        };
                        value["preflight_source_sha256"] =
                            serde_json::Value::String(source_hash.clone());
                        value["preflight_context"] = preflight_context_for_worker.clone();
                        workflow
                            .write_analysis_artifact(source, &value)
                            .map_err(|error| {
                                FukidashiError::Inference(format!(
                                    "analysis checkpoint write failed: {error}"
                                ))
                            })?;
                        value
                    };
                    let handoff_before = analysis
                        .get("translation_handoff")
                        .and_then(|handoff| handoff.get("items"))
                        .cloned();
                    let auto_preserved = augment_missing_detected_text_items(&mut analysis)?;
                    let handoff_changed = handoff_before.as_ref()
                        != analysis
                            .get("translation_handoff")
                            .and_then(|handoff| handoff.get("items"));
                    if !auto_preserved.is_empty() || handoff_changed {
                        workflow
                            .write_analysis_artifact(source, &analysis)
                            .map_err(|error| {
                                FukidashiError::Inference(format!(
                                    "preflight source-item handoff write failed: {error}"
                                ))
                            })?;
                    }
                    if analysis.get("preflight_source_sha256").is_none() {
                        analysis["preflight_source_sha256"] =
                            serde_json::Value::String(source_hash.clone());
                        workflow
                            .write_analysis_artifact(source, &analysis)
                            .map_err(|error| {
                                FukidashiError::Inference(format!(
                                    "analysis cache refresh failed: {error}"
                                ))
                            })?;
                    }
                    analysis["preflight_context"] = preflight_context_for_worker.clone();
                    pages.push(preflight_page_manifest(
                        page_number,
                        source,
                        &source_hash,
                        &analysis,
                        reused,
                    ));
                    if let Some(page) = pages.last_mut() {
                        page["preflight_context"] = preflight_context_for_worker.clone();
                    }
                    persist_progress(&pages)?;
                }
                Ok::<_, FukidashiError>(pages)
            })();
            // One explicit release at the job boundary.  This keeps the
            // detector/recognizer sessions hot across pages but never leaks
            // model memory after a completed or failed preflight.
            engine.release_sessions();
            operation
        })
        .await;
        let pages = match result {
            Ok(Ok(pages)) => pages,
            Ok(Err(error)) => {
                return json_result(
                    &serde_json::json!({"protocol":"strict-v1","error":error.to_string()}),
                    true,
                );
            }
            Err(error) => {
                return json_result(
                    &serde_json::json!({"protocol":"strict-v1","error":format!("preflight worker failed: {error}")}),
                    true,
                );
            }
        };
        let mut manifest = serde_json::json!({
            "protocol": "strict-v1",
            "status": "preflight_ready",
            "job_id": job_id,
            "total_pages": total_pages,
            "translate_cover": translate_cover,
            "preflight_context": preflight_context_for_response,
            "pages": pages,
            "next_action": {"tool":"fukidashi_translation_start","arguments":{"job_id":job.file_name().and_then(|name| name.to_str()).unwrap_or_default(),"translate_cover":translate_cover}},
        });
        attach_preflight_cache_telemetry(&mut manifest, &pages, total_pages);
        let manifest_path = match self.workflow.write_preflight_manifest(&job, &manifest) {
            Ok(path) => path,
            Err(error) => {
                return json_result(
                    &serde_json::json!({"protocol":"strict-v1","error":format!("persist preflight manifest: {error}")}),
                    true,
                );
            }
        };
        emit_page_progress(
            total_pages,
            total_pages,
            "Preflight ready; waiting for page translation...",
        );
        let mut response = manifest;
        response["manifest_path"] = serde_json::Value::String(manifest_path.display().to_string());
        response["progress"] = page_progress_json(total_pages, total_pages, "Preflight ready");
        attach_preflight_cache_telemetry(&mut response, &pages, total_pages);
        json_result(&response, false)
    }

    #[tool(
        name = "fukidashi_put_lore",
        description = "Validate and atomically write lore before the first submit; use {schema:1,characters:[\"Fuyu\",{id:\"kuga\",names:[\"Kuga\"]}],pronouns:[],glossary:[{source:\"proprietress\",target:\"bà chủ\"}]} (character strings are canonicalized to {id,names,notes}). Known fields are checked, unknown top-level fields are retained, and malformed or oversized values return the expected-shape error; this never invokes an LLM."
    )]
    pub async fn put_lore(&self, Parameters(req): Parameters<PutLoreRequest>) -> CallToolResult {
        let job = match self.workflow.resolve_managed_job_id(&req.job_id) {
            Ok(job) => job,
            Err(error) => {
                return json_result(&serde_json::json!({"error": error.to_string()}), true);
            }
        };
        match self.workflow.write_lore(&job, &req.lore) {
            Ok(lore) => json_result(
                &serde_json::json!({"job_id": req.job_id, "lore": lore}),
                false,
            ),
            Err(error) => json_result(
                &serde_json::json!({"job_id": req.job_id, "error": error.to_string()}),
                true,
            ),
        }
    }

    #[tool(
        name = "fukidashi_translation_start",
        description = "Strict-v1 server-owned translation loop. Start from one source image or resume with job_id/job_path. For a fresh multi-page job, call fukidashi_translation_preflight once first; it returns resumable 1/N routing and cached page analysis. The server analyzes or reuses the first unfinished page and returns compact stable translation_items plus explicit preserved_items and an opaque work_token. Every detected source item is either listed in required_translation_ids or listed as preserved; submit exactly one decision for each required ID. Cover-like page 1 is preserved by default; set translate_cover=true during preflight/start to translate it. Dense prose groups are fail-closed when source coverage is missing or implausibly short. source_language accepts auto, ja, zh, ko, en, or latin; legacy pipe values such as en|latin are normalized to automatic routing. sfx_mode=preserve (default) keeps structurally unmatched text-* items out of translation, cleaning, and typesetting; preserve-only pages receive a verified pass-through stage and advance automatically; use replace only explicitly. Do not inspect managed job files or construct artifact paths; the submit call accepts only that token and the exact item IDs."
    )]
    pub async fn translation_start(
        &self,
        Parameters(req): Parameters<TranslationStartRequest>,
    ) -> CallToolResult {
        let mut req = req;
        if let Some(mode) = req.ocr_mode.as_deref()
            && !matches!(mode, "local" | "auto")
        {
            return json_result(
                &serde_json::json!({
                    "protocol": "strict-v1",
                    "error": "ocr_mode must be local or auto",
                    "next_step": "retry fukidashi_translation_start with ocr_mode=local or omit it"
                }),
                true,
            );
        }
        if let Err(error) = resolve_sfx_mode(req.sfx_mode.as_deref()) {
            return json_result(
                &serde_json::json!({
                    "protocol": "strict-v1",
                    "error": error.to_string(),
                    "next_step": "retry fukidashi_translation_start with sfx_mode=preserve or sfx_mode=replace"
                }),
                true,
            );
        }
        req.source_language = match normalize_requested_source_language(req.source_language.take())
        {
            Ok(value) => value,
            Err(error) => {
                return json_result(
                    &serde_json::json!({
                        "protocol": "strict-v1",
                        "error": error.to_string(),
                        "next_step": "use source_language=auto, ja, zh, ko, en, or latin"
                    }),
                    true,
                );
            }
        };
        if req.scope.is_some() && (req.job_path.is_some() || req.job_id.is_some()) {
            return json_result(
                &serde_json::json!({
                    "protocol": "strict-v1",
                    "error": "scope is accepted only when starting from image_path; a resumed job keeps its original inventory"
                }),
                true,
            );
        }
        let job = match self.resolve_strict_job(&req) {
            Ok(job) => job,
            Err(error) => {
                return json_result(
                    &serde_json::json!({
                        "protocol": "strict-v1",
                        "error": error.to_string(),
                        "next_step": "provide exactly one of image_path, job_path, or job_id"
                    }),
                    true,
                );
            }
        };
        // A new multi-page source gets one explicit bulk routing pass before
        // any page-level translation token is issued. Existing job_id/job_path
        // callers retain the legacy per-page resume path when no manifest is
        // available, so old managed jobs remain resumable and untouched.
        if req.image_path.is_some() {
            let total_pages = self.workflow.managed_job_page_count(&job).unwrap_or(0);
            let preflight_ready = self
                .workflow
                .read_preflight_manifest(&job)
                .ok()
                .flatten()
                .is_some_and(|manifest| {
                    manifest.get("status").and_then(serde_json::Value::as_str)
                        == Some("preflight_ready")
                });
            let needs_preflight = total_pages > 1 && !preflight_ready;
            if needs_preflight {
                let job_id = job
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default();
                let mut value = serde_json::json!({
                    "protocol": "strict-v1",
                    "status": "preflight_required",
                    "job_id": job_id,
                    "total_pages": total_pages,
                    "next_action": {
                        "tool": "fukidashi_translation_preflight",
                        "arguments": {"job_id": job_id, "translate_cover": req.translate_cover},
                    },
                    "next_step": "call fukidashi_translation_preflight once; then resume with fukidashi_translation_start using the returned job_id",
                });
                attach_page_progress(&mut value, 0, total_pages, "Preflight required");
                emit_page_progress(0, total_pages, "Preflight required before page translation");
                return json_result(&value, false);
            }
        }
        let pending = match self.workflow.next_pending_page(&job) {
            Ok(Some(page)) => page,
            Ok(None) => {
                return match self.strict_review_ready(&job) {
                    Ok(mut value) => {
                        let total = self.workflow.managed_job_page_count(&job).unwrap_or(0);
                        emit_page_progress(total, total, "Review ready");
                        attach_page_progress(&mut value, total, total, "Review ready");
                        json_result(&value, false)
                    }
                    Err(error) => json_result(
                        &serde_json::json!({"protocol":"strict-v1","error":error.to_string()}),
                        true,
                    ),
                };
            }
            Err(error) => {
                return json_result(
                    &serde_json::json!({
                        "protocol": "strict-v1",
                        "error": format!("unable to select the next managed page: {error}"),
                        "next_step": "resume with the exact job_id returned by Fukidashi; do not shell-read managed state"
                    }),
                    true,
                );
            }
        };
        let page_number = pending.page_number;
        let total_pages = pending.total_pages;
        match self.prepare_strict_page(pending, &req).await {
            Ok(value) => json_result(&value, false),
            Err(error) => {
                let mut value = serde_json::json!({
                    "protocol": "strict-v1",
                    "error": error.to_string(),
                    "next_step": "fix the reported runtime/model issue and call fukidashi_translation_start again with the same job_id"
                });
                add_error_diagnostic(&mut value, &error);
                if let Some(next_step) = value.get("diagnostic").and_then(|diagnostic| {
                    diagnostic
                        .get("next_step")
                        .and_then(serde_json::Value::as_str)
                }) {
                    value["next_step"] = serde_json::Value::String(next_step.to_owned());
                }
                attach_page_progress(&mut value, page_number, total_pages, "Error");
                json_result(&value, true)
            }
        }
    }

    #[tool(
        name = "fukidashi_translation_submit",
        description = "Strict-v1 continuation. Submit exactly one translation decision for every stable ID in required_translation_ids returned by fukidashi_translation_start; preserved_items are already explicit source decisions and must not be omitted from your review. The server owns analysis, cleaning, typesetting, stage reuse, model release, and page advancement; do not send image, analysis, clean, mask, font, or output paths. Use keep_source=true and/or needs_review=true for uncertain OCR instead of aborting the page. When every required item uses keep_source=true, the server records a verified source pass-through, skips cleaning/typesetting with an empty crop list, consumes the token once, and returns completed_page plus the next_action."
    )]
    pub async fn translation_submit(
        &self,
        Parameters(req): Parameters<TranslationSubmitRequest>,
    ) -> CallToolResult {
        let claim = match self.begin_translation_claim(&req.work_token) {
            Ok(claim) => claim,
            Err(error) => {
                return json_result(
                    &serde_json::json!({
                        "protocol": "strict-v1",
                        "error": error.to_string(),
                        "next_step": "call fukidashi_translation_start to obtain a fresh work_token"
                    }),
                    true,
                );
            }
        };
        let fail = |server: &FukidashiServer, error: FukidashiError| {
            server.reset_translation_claim(&req.work_token);
            let mut value = serde_json::json!({
                "protocol": "strict-v1",
                "error": error.to_string(),
                "retryable": true,
                "work_token": req.work_token,
                "next_step": "correct the submission or runtime issue and retry the same work_token; call start again only after a stale-token error"
            });
            add_error_diagnostic(&mut value, &error);
            if let Some(next_step) = value.get("diagnostic").and_then(|diagnostic| {
                diagnostic
                    .get("next_step")
                    .and_then(serde_json::Value::as_str)
            }) {
                value["next_step"] = serde_json::Value::String(next_step.to_owned());
            }
            attach_page_progress(&mut value, claim.page_number, claim.total_pages, "Error");
            json_result(&value, true)
        };

        let pending = match self.workflow.next_pending_page(&claim.job_dir) {
            Ok(Some(page)) => page,
            Ok(None) => {
                return fail(
                    self,
                    FukidashiError::InvalidInput(
                        "work_token is stale because the managed job is already complete".into(),
                    ),
                );
            }
            Err(error) => return fail(self, FukidashiError::InvalidInput(error.to_string())),
        };
        if pending.source_image != claim.source_image
            || pending.page_number != claim.page_number
            || pending.total_pages != claim.total_pages
            || pending.analysis_path != claim.analysis_path
        {
            return fail(
                self,
                FukidashiError::InvalidInput(
                    "work_token is stale or belongs to a different unfinished page; call fukidashi_translation_start again".into(),
                ),
            );
        }
        let current_hash = match self
            .workflow
            .managed_file_sha256(&claim.analysis_path, "analysis checkpoint")
        {
            Ok(hash) => hash,
            Err(error) => return fail(self, FukidashiError::InvalidInput(error.to_string())),
        };
        if current_hash != claim.analysis_sha256 {
            return fail(
                self,
                FukidashiError::InvalidInput(
                    "analysis checkpoint changed after this work_token was issued; call fukidashi_translation_start again".into(),
                ),
            );
        }
        let mut analysis = match read_saved_analysis(&self.workflow, &claim.analysis_path) {
            Ok(value) => value,
            Err(error) => return fail(self, error),
        };
        // Validate stable IDs and decision fields before touching geometry.
        // This lets an exact all-keep submission take the source pass-through
        // even when a silent-page false positive has null/degenerate bboxes.
        let plan = match Self::validate_strict_submissions(&analysis, &claim, &req.translations) {
            Ok(plan) => plan,
            Err(error) => return fail(self, error),
        };
        if let Err(error) = validate_prose_source_coverage(&analysis, &claim.source_image, &plan) {
            return fail(self, error);
        }
        if let Err(error) = validate_prose_group_geometry(&analysis, &plan) {
            return fail(self, error);
        }
        let all_keep_source = !plan.selected.is_empty()
            && plan.selected.iter().all(|selection| selection.keep_source);
        let payloads = if all_keep_source {
            None
        } else {
            match Self::strict_typeset_payloads(&analysis, &plan) {
                Ok(payloads) => Some(payloads),
                Err(error) => return fail(self, error),
            }
        };
        if let Err(error) = Self::apply_strict_handoff(&mut analysis, &claim, &plan) {
            return fail(self, error);
        }
        if let Err(error) = self
            .workflow
            .write_analysis_artifact(&claim.source_image, &analysis)
        {
            return fail(
                self,
                FukidashiError::Inference(format!("persist translated handoff: {error}")),
            );
        }
        let persisted_hash = match self
            .workflow
            .managed_file_sha256(&claim.analysis_path, "analysis checkpoint")
        {
            Ok(hash) => hash,
            Err(error) => return fail(self, FukidashiError::Inference(error.to_string())),
        };
        if let Err(error) = self.refresh_translation_claim_hash(&req.work_token, persisted_hash) {
            return fail(self, error);
        }

        // A page whose every required dialogue decision explicitly keeps the
        // source must not enter crop cleaning.  Silent-page OCR false
        // positives commonly produce exactly this submission, and an empty
        // eligible crop list is a verified pass-through rather than an
        // inpainting request.  The handoff is already persisted above, so a
        // failure here leaves the claim retryable with the same token.
        if all_keep_source {
            emit_page_progress(
                claim.page_number,
                claim.total_pages,
                "Preserving source page (all decisions keep_source)...",
            );
            if let Err(error) = self
                .complete_preserved_page(&pending, &analysis, claim.replace_sfx)
                .await
            {
                return fail(self, error);
            }
            let _ = self
                .release_models(Parameters(ReleaseModelsRequest {}))
                .await;
            if let Err(error) = self.consume_translation_claim(&req.work_token) {
                return json_result(
                    &serde_json::json!({
                        "protocol": "strict-v1",
                        "error": error.to_string(),
                        "next_step": "the page was rendered; resume with fukidashi_translation_start using the job_id"
                    }),
                    true,
                );
            }
            return self.completed_translation_response(
                &claim,
                self.advance_strict_page(&claim).await,
                true,
            );
        }

        // A valid saved clean stage survives an interrupted client turn.  If
        // it is absent or fails provenance validation, the server reruns the
        // clean operation with its canonical analysis path.
        let page = match self
            .workflow
            .managed_page(&claim.job_dir, &claim.source_image)
        {
            Ok(page) => page,
            Err(error) => return fail(self, FukidashiError::InvalidInput(error.to_string())),
        };
        let cleaned_path = if matches!(page.state.as_str(), "cleaned" | "rendered") {
            self.workflow
                .validate_clean_input(&page.cleaned_image)
                .ok()
                .map(|artifact| artifact.cleaned_image)
        } else {
            None
        };
        let cleaned_path = if let Some(path) = cleaned_path {
            path
        } else {
            emit_page_progress(
                claim.page_number,
                claim.total_pages,
                "Inpainting clean mask...",
            );
            let clean_request = CleanRequest {
                image_path: claim.source_image.display().to_string(),
                mask_path: None,
                dilation: Some(3),
                mode: Some("crop".into()),
                analysis_path: Some(claim.analysis_path.display().to_string()),
                text_regions: Vec::new(),
                crop_padding: None,
                crop_minimum_size: None,
                sfx_mode: Some(if claim.replace_sfx {
                    "replace".into()
                } else {
                    "preserve".into()
                }),
            };
            let clean_result = self.clean_page(Parameters(clean_request)).await;
            let _ = self
                .release_models(Parameters(ReleaseModelsRequest {}))
                .await;
            match extract_tool_json(clean_result, "strict clean") {
                Ok(_) => match self
                    .workflow
                    .managed_page(&claim.job_dir, &claim.source_image)
                {
                    Ok(page) => match self.workflow.validate_clean_input(&page.cleaned_image) {
                        Ok(artifact) => artifact.cleaned_image,
                        Err(error) => {
                            return fail(self, FukidashiError::Inference(error.to_string()));
                        }
                    },
                    Err(error) => return fail(self, FukidashiError::Inference(error.to_string())),
                },
                Err(error) => return fail(self, error),
            }
        };
        emit_page_progress(
            claim.page_number,
            claim.total_pages,
            "Typesetting dialogue...",
        );
        let typeset_request = TypesetRequest {
            image_path: cleaned_path.display().to_string(),
            bubbles: payloads.expect("normal strict submissions have payloads"),
            font_path: None,
            padding: None,
            min_font_size: None,
            max_font_size: None,
            shape: None,
            fallback_font_paths: Vec::new(),
        };
        let typeset_result = self.typeset(Parameters(typeset_request)).await;
        if let Err(error) = extract_tool_json(typeset_result, "strict typeset") {
            let _ = self
                .release_models(Parameters(ReleaseModelsRequest {}))
                .await;
            return fail(self, error);
        }
        // Keep the strict boundary explicit even when the configured page
        // recycle count already released sessions during clean.
        let _ = self
            .release_models(Parameters(ReleaseModelsRequest {}))
            .await;
        if let Err(error) = self.consume_translation_claim(&req.work_token) {
            return json_result(
                &serde_json::json!({
                    "protocol": "strict-v1",
                    "error": error.to_string(),
                    "next_step": "the page was rendered; resume with fukidashi_translation_start using the job_id"
                }),
                true,
            );
        }
        self.completed_translation_response(&claim, self.advance_strict_page(&claim).await, false)
    }

    #[tool(
        name = "fukidashi_search_manga",
        description = "Search native MangaDex titles only. source=auto resolves to MangaDex; no aggregator or guessed provider is used. Returns stable manga_id values, display/original language/status metadata, alternate titles, and a suggested latest=true next call. Use an exact returned manga_id when calling fukidashi_pull_chapter; source language is metadata, not a failure."
    )]
    pub async fn search_manga(
        &self,
        Parameters(req): Parameters<SearchMangaRequest>,
    ) -> CallToolResult {
        match crate::ingress::search_manga(req).await {
            Ok(value) => json_result(&value, false),
            Err(error) => json_result(
                &serde_json::json!({
                    "error": error.to_string(),
                    "next_step": "retry with source=auto or source=mangadex and a bounded query"
                }),
                true,
            ),
        }
    }

    #[tool(
        name = "fukidashi_pull_chapter",
        description = "Acquire a chapter into a new server-owned Fukidashi job. For a vague latest request, first search MangaDex, then pass the exact returned manga_id with latest=true; latest selects the highest chapter value from the descending feed, including external releases, and never silently downgrades to an older hosted chapter. MangaDex mode also accepts exact manga_id/chapter_id plus optional exact chapter/language filters; source language is accepted as metadata and data_saver='full' selects full data by default (legacy booleans are accepted). An external or empty hosted release returns imported=false with exact release metadata instead of calling another chapter. Direct mode is first-class: pass one explicit http(s) url and optional bounded job_name; the already installed gallery-dl helper uses bounded transactional staging. If the explicit URL is unsupported or extraction fails, the result says it was not imported and can be retried with another explicit supported http(s) URL; no alternative site is selected automatically. Imported job prefixes use the canonical manga title/chapter or a conservative URL label; labels never choose paths. Imported responses include job_id/job_path/page_count/source metadata and the exact fukidashi_translation_start next step; do not shell-read or invent paths."
    )]
    pub async fn pull_chapter(
        &self,
        Parameters(req): Parameters<PullChapterRequest>,
    ) -> CallToolResult {
        match crate::ingress::pull_chapter(&self.config, &self.workflow, req).await {
            Ok(value) => json_result(&value, false),
            Err(error) => json_result(
                &serde_json::json!({
                    "error": error.to_string(),
                    "next_step": "provide exact MangaDex identifiers or an explicit http(s) URL as required"
                }),
                true,
            ),
        }
    }

    #[tool(
        name = "fukidashi_get_config",
        description = "Show effective Fukidashi storage, model, job, cache, runtime, font and device configuration. Paths come from explicit CLI, environment, saved user config, then platform defaults. This is read-only."
    )]
    pub async fn get_config(
        &self,
        Parameters(_req): Parameters<GetConfigRequest>,
    ) -> CallToolResult {
        json_result(&self.config.config_report(), false)
    }

    #[tool(
        name = "fukidashi_configure",
        description = "Persist Fukidashi paths/preferences for this user. Prefer storage_root: it derives models/jobs/cache/runtime/fonts/exports. Advanced directory overrides are available. Paths must be absolute; no files are downloaded, moved, or deleted. Restart the MCP after changing startup paths or provider."
    )]
    pub async fn configure(&self, Parameters(req): Parameters<ConfigureRequest>) -> CallToolResult {
        match self.config.configure_user(&req) {
            Ok(mut report) => {
                report.restart_required = true;
                json_result(&report, false)
            }
            Err(error) => json_result(
                &serde_json::json!({
                    "error": error.to_string(),
                    "next_step": "Use an absolute writable storage_root such as /data/Fukidashi (Linux) or D:/Fukidashi (Windows), then restart the MCP. Existing legacy jobs remain where they are until explicitly migrated."
                }),
                true,
            ),
        }
    }

    #[tool(
        name = "fukidashi_analyze_page",
        description = "Detect text regions and run local multilingual OCR (ja/zh/ko/en) automatically. Returns OCR text, confidence, stable region IDs and a pending translation handoff. On the first call, optional scope={start_page,end_page} or scope={include_paths} captures the expected page inventory; later review/export requires every expected page. The complete analysis is always written to workflow.analysis_path under the allocated job; use response_detail=compact for bounded-context multi-page runs. An optional checkpoint_path is only a mirror inside that same managed job. Omitted response_detail preserves the full response. ocr_mode local or auto both select local OCR. Translation is supplied by the calling client model."
    )]
    pub async fn analyze_page(
        &self,
        Parameters(req): Parameters<AnalyzeRequest>,
    ) -> CallToolResult {
        let response_detail = req.response_detail.as_deref().unwrap_or("full");
        if !matches!(response_detail, "compact" | "full") {
            return json_result(
                &serde_json::json!({"error":"response_detail must be compact or full"}),
                true,
            );
        }
        let source_language = match normalize_requested_source_language(req.source_language.clone())
        {
            Ok(value) => value,
            Err(error) => {
                return json_result(&serde_json::json!({"error":error.to_string()}), true);
            }
        };
        let requested_checkpoint_path = match req.checkpoint_path.as_deref().map(path).transpose() {
            Ok(value) => value,
            Err(error) => {
                return json_result(&serde_json::json!({"error":error.to_string()}), true);
            }
        };
        let scope = match req.scope.as_ref().map(resolve_analysis_scope).transpose() {
            Ok(scope) => scope,
            Err(error) => {
                return json_result(&serde_json::json!({"error":error.to_string()}), true);
            }
        };
        let result = match path(&req.image_path) {
            Ok(image_path) if image_path.is_file() => {
                let registration = match self
                    .workflow
                    .register_analysis(&image_path, scope.as_ref())
                {
                    Ok(registration) => registration,
                    Err(error) => {
                        return json_result(
                            &serde_json::json!({
                                "error": format!("unable to register managed page scope: {error}"),
                                "next_step": "set scope on the first page analysis, then keep that scope for every page"
                            }),
                            true,
                        );
                    }
                };
                let canonical_checkpoint = match self
                    .workflow
                    .page_artifacts_for_source(&image_path)
                {
                    Ok(paths) => paths.0,
                    Err(error) => {
                        return json_result(
                            &serde_json::json!({
                                "error": format!("unable to resolve canonical analysis checkpoint: {error}"),
                                "next_step": "omit checkpoint_path and retry the page analysis"
                            }),
                            true,
                        );
                    }
                };
                let checkpoint_path = match requested_checkpoint_path.as_deref() {
                    Some(requested) => {
                        if !crate::workflow::path_is_within_public(requested, &registration.job_dir)
                            .unwrap_or(false)
                        {
                            return json_result(
                                &serde_json::json!({
                                    "error": "checkpoint_path must be inside the managed job allocated for this source page",
                                    "next_step": "omit checkpoint_path on the first analysis call; use the returned workflow.analysis_path"
                                }),
                                true,
                            );
                        }
                        match self
                            .workflow
                            .require_output_owned(requested, "checkpoint output")
                        {
                            Ok(path) => Some(path),
                            Err(error) => {
                                return json_result(
                                    &serde_json::json!({
                                        "error": format!("checkpoint output must be inside the managed job: {error}"),
                                        "next_step": "omit checkpoint_path on the first analysis call; use the returned workflow.analysis_path"
                                    }),
                                    true,
                                );
                            }
                        }
                    }
                    None => None,
                };
                if !matches!(req.ocr_mode.as_deref().unwrap_or("local"), "local" | "auto") {
                    Err(FukidashiError::RuntimeUnavailable("ocr_mode external/disabled does not perform local recognition; use local or auto for automatic multilingual OCR".into()))
                } else {
                    let permit = match Arc::clone(&self.slots).acquire_owned().await {
                        Ok(permit) => permit,
                        Err(e) => {
                            return json_result(
                                &serde_json::json!({"error":format!("OCR worker capacity closed: {e}")}),
                                true,
                            );
                        }
                    };
                    let ocr = Arc::clone(&self.ocr);
                    let config = self.config.clone();
                    let source = source_language;
                    let target = req.target_language;
                    let corrections = req.corrected_source_text;
                    let response_detail = response_detail.to_owned();
                    let registration = registration.clone();
                    let canonical_checkpoint = canonical_checkpoint.clone();
                    let workflow = Arc::clone(&self.workflow);
                    match tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        let mut engine = ocr.lock().map_err(|_| {
                            FukidashiError::RuntimeUnavailable("OCR session lock poisoned".into())
                        })?;
                        let operation = (|| {
                            let mut analysis = engine.analyze(
                                &config,
                                &image_path,
                                source.as_deref(),
                                target.as_deref(),
                            )?;
                            if let Some(corrections) = corrections.as_ref() {
                                crate::vision::ocr::apply_source_corrections(
                                    &mut analysis,
                                    corrections,
                                );
                            }
                            Ok::<_, FukidashiError>(analysis)
                        })();
                        let recycled = engine.finish_heavy_call(config.session_recycle_pages());
                        let analysis = operation?;
                        let mut full = serde_json::to_value(&analysis)?;
                        let analysis_path = workflow
                            .write_analysis_artifact(&image_path, &full)
                            .map_err(|error| {
                                FukidashiError::Inference(format!(
                                    "analysis checkpoint write failed: {error}"
                                ))
                            })?;
                        if let Some(checkpoint_path) = checkpoint_path.as_deref()
                            && !crate::workflow::path_is_within_public(
                                checkpoint_path,
                                &canonical_checkpoint,
                            )
                            .unwrap_or(false)
                        {
                            save_json_atomic(checkpoint_path, &full)?;
                        }
                        let response_checkpoint = canonical_checkpoint.clone();
                        let mut response = if response_detail == "full" {
                            full["runtime"] = serde_json::json!({
                                "sessions_recycled": recycled,
                                "checkpoint_path": response_checkpoint,
                            });
                            full
                        } else {
                            compact_analysis(
                                &analysis,
                                &image_path,
                                Some(&response_checkpoint),
                                recycled,
                            )
                        };
                        response["workflow"] = serde_json::json!({
                            "job_dir": registration.job_dir,
                            "analysis_path": analysis_path,
                            "expected_pages": registration.expected_pages,
                            "page_state": registration.page_state,
                            "next_step": "analyze, clean, and typeset every expected page before serving review"
                        });
                        Ok(response)
                    })
                    .await
                    {
                        Ok(result) => result,
                        Err(e) => Err(FukidashiError::Inference(format!("OCR worker failed: {e}"))),
                    }
                }
            }
            Ok(image_path) => Err(FukidashiError::MissingAsset { path: image_path }),
            Err(e) => Err(e),
        };
        match result {
            Ok(v) => json_result(&v, false),
            Err(e) => json_result(&serde_json::json!({"error":e.to_string()}), true),
        }
    }
    #[tool(
        name = "fukidashi_clean_page",
        description = "Remove text strokes with LaMa. mode=full preserves page inference; mode=crop requires mask_path, text_regions, or a full analysis_path and avoids detection/OCR. An explicitly empty crop region list returns an actionable strict keep_source pass-through message instead of starting a heavy model. sfx_mode=preserve (default) excludes structurally unmatched text-* regions from analysis-derived cleaning; use replace explicitly."
    )]
    pub async fn clean_page(&self, Parameters(req): Parameters<CleanRequest>) -> CallToolResult {
        let image_path = match path(&req.image_path) {
            Ok(p) if p.is_file() => p,
            Ok(p) => {
                return json_result(
                    &serde_json::json!({"error": FukidashiError::MissingAsset { path: p }.to_string()}),
                    true,
                );
            }
            Err(e) => return json_result(&serde_json::json!({"error": e.to_string()}), true),
        };
        let mask_path = match req.mask_path.as_deref().map(path).transpose() {
            Ok(p) => p,
            Err(e) => return json_result(&serde_json::json!({"error": e.to_string()}), true),
        };
        let mode = req.mode.as_deref().unwrap_or("full").to_owned();
        if !matches!(mode.as_str(), "full" | "crop") {
            return json_result(
                &serde_json::json!({"error":"clean mode must be full or crop"}),
                true,
            );
        }
        let replace_sfx = match resolve_sfx_mode(req.sfx_mode.as_deref()) {
            Ok(value) => value,
            Err(error) => {
                return json_result(&serde_json::json!({"error": error.to_string()}), true);
            }
        };
        let mut text_regions = req.text_regions;
        if let Some(analysis_path) = req.analysis_path.as_deref() {
            let analysis_path = match path(analysis_path) {
                Ok(path) => path,
                Err(error) => {
                    return json_result(&serde_json::json!({"error":error.to_string()}), true);
                }
            };
            let analysis_path = match self
                .workflow
                .require_analysis_for_source(&image_path, &analysis_path)
            {
                Ok(path) => path,
                Err(error) => {
                    return json_result(
                        &serde_json::json!({
                            "error": format!("analysis checkpoint must belong to this source page: {error}"),
                            "next_step": "use the workflow.analysis_path returned by fukidashi_analyze_page for this image"
                        }),
                        true,
                    );
                }
            };
            match checkpoint_text_regions(&analysis_path, replace_sfx) {
                Ok(mut regions) => text_regions.append(&mut regions),
                Err(error) => {
                    return json_result(&error_json(&error), true);
                }
            }
        }
        if mode == "crop" && mask_path.is_none() && text_regions.is_empty() {
            return json_result(
                &serde_json::json!({
                    "error": "crop cleaning has no eligible regions; provide mask_path/text_regions or use strict fukidashi_translation_submit with keep_source=true for an explicit source pass-through",
                    "next_action": "do not retry crop cleaning with an empty region list"
                }),
                true,
            );
        }
        let dilation = req.dilation.unwrap_or(3);
        let crop_padding = req.crop_padding.unwrap_or(48).clamp(8, 512);
        let crop_minimum_size = req.crop_minimum_size.unwrap_or(256).clamp(64, 2048);
        let permit = match Arc::clone(&self.slots).acquire_owned().await {
            Ok(permit) => permit,
            Err(e) => {
                return json_result(
                    &serde_json::json!({"error": format!("cleaning worker capacity closed: {e}")}),
                    true,
                );
            }
        };
        let cleaner = Arc::clone(&self.ocr);
        let workflow = Arc::clone(&self.workflow);
        let config = self.config.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut engine = cleaner.lock().map_err(|_| {
                FukidashiError::RuntimeUnavailable("OCR/inpainting session lock poisoned".into())
            })?;
            let started = std::time::Instant::now();
            let operation = if mode == "crop" {
                engine
                    .clean_crops(
                        &config,
                        &image_path,
                        mask_path.as_deref(),
                        &text_regions,
                        dilation,
                        crop_padding,
                        crop_minimum_size,
                    )
                    .map(|(cleaned, mask, execution)| {
                        (
                            cleaned,
                            mask,
                            execution.provider,
                            execution.fallback,
                            execution.fallback_reason,
                            execution.crops,
                        )
                    })
            } else {
                let full = if mask_path.is_none() && !text_regions.is_empty() {
                    engine.clean_with_regions(&config, &image_path, &text_regions, dilation)
                } else {
                    engine.clean(&config, &image_path, mask_path.as_deref(), dilation)
                };
                full.map(|(cleaned, mask, execution)| {
                    (
                        cleaned,
                        mask,
                        execution.provider,
                        execution.fallback,
                        execution.fallback_reason,
                        Vec::new(),
                    )
                })
            };
            let recycled = engine.finish_heavy_call(config.session_recycle_pages());
            let (cleaned, mask, provider, fallback, fallback_reason, crops) = operation?;
            let (_, _, mut value) = workflow
                .write_clean_artifact(&image_path, &cleaned, &mask, dilation, &mode)
                .map_err(|error| {
                    FukidashiError::Inference(format!("clean validation failed: {error}"))
                })?;
            value["masked_pixels"] = value["workflow"]["masked_pixels"].clone();
            value["dilation"] = serde_json::json!(dilation);
            value["mode"] = serde_json::json!(mode);
            value["sfx_mode"] = serde_json::json!(if replace_sfx { "replace" } else { "preserve" });
            value["elapsed_seconds"] = serde_json::json!(started.elapsed().as_secs_f64());
            value["crop_count"] = serde_json::json!(crops.len());
            value["crops"] = serde_json::json!(crops);
            value["sessions_recycled"] = serde_json::json!(recycled);
            value["provider"] = serde_json::json!(provider);
            value["provider_fallback"] = serde_json::json!(fallback);
            value["provider_fallback_reason"] = serde_json::json!(fallback_reason);
            Ok::<_, FukidashiError>(value)
        })
        .await;
        let result = match result {
            Ok(result) => result,
            Err(e) => Err(FukidashiError::Inference(format!(
                "cleaning worker failed: {e}"
            ))),
        };
        match result {
            Ok(v) => json_result(&v, false),
            Err(e) => json_result(&error_json(&e), true),
        }
    }
    #[tool(
        name = "fukidashi_release_models",
        description = "Drop cached OCR and inpainting ONNX sessions after a page or chunk to release their per-session CPU/GPU memory. Safe to call between page operations."
    )]
    pub async fn release_models(
        &self,
        Parameters(_req): Parameters<ReleaseModelsRequest>,
    ) -> CallToolResult {
        let permit = match Arc::clone(&self.slots).acquire_owned().await {
            Ok(permit) => permit,
            Err(error) => {
                return json_result(
                    &serde_json::json!({"error":format!("model worker capacity closed: {error}")}),
                    true,
                );
            }
        };
        let ocr = Arc::clone(&self.ocr);
        match tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut engine = ocr.lock().map_err(|_| {
                FukidashiError::RuntimeUnavailable("OCR session lock poisoned".into())
            })?;
            engine.release_sessions();
            Ok::<_, FukidashiError>(())
        })
        .await
        {
            Ok(Ok(())) => json_result(
                &serde_json::json!({"released":true,"scope":"ocr_and_inpainting_sessions"}),
                false,
            ),
            Ok(Err(error)) => json_result(&serde_json::json!({"error":error.to_string()}), true),
            Err(error) => json_result(
                &serde_json::json!({"error":format!("model release worker failed: {error}")}),
                true,
            ),
        }
    }
    #[tool(
        name = "fukidashi_typeset",
        description = "Render supplied translations into speech bubbles using shaped glyph metrics. Use bundled Comic Neue for comic dialogue and Patrick Hand coverage for Vietnamese. Chinese, Korean, and Japanese glyphs use configured or installed system CJK fonts when available. Do not pass generic Windows UI fonts such as Arial, Calibri, Segoe UI, Tahoma, Verdana, Times, or DejaVu Sans as a primary because the server substitutes Comic Neue and reports the substitution."
    )]
    pub async fn typeset(&self, Parameters(req): Parameters<TypesetRequest>) -> CallToolResult {
        let req_global_font = req.font_path.clone();
        let (image_path, bubbles, fallback_font_paths) = resolve_typeset_request(req);
        let image_path = match path(&image_path) {
            Ok(p) => p,
            Err(e) => return json_result(&serde_json::json!({"error":e.to_string()}), true),
        };
        if !image_path.is_file() {
            return json_result(
                &serde_json::json!({"error": FukidashiError::MissingAsset { path: image_path }.to_string()}),
                true,
            );
        }
        let clean_artifact = match self.workflow.validate_clean_input(&image_path) {
            Ok(artifact) => artifact,
            Err(error) => {
                return json_result(
                    &serde_json::json!({
                        "error": format!("typeset requires a validated server-owned clean stage: {error}"),
                        "next_step": "call fukidashi_clean_page and pass its returned cleaned_image_path to fukidashi_typeset"
                    }),
                    true,
                );
            }
        };
        let parent = image_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let workflow = Arc::clone(&self.workflow);
        let job_root = match workflow.managed_job_for_path(&image_path) {
            Ok(job) => job,
            Err(error) => {
                return json_result(
                    &serde_json::json!({
                        "error": format!("typeset input is outside a managed job: {error}"),
                        "next_step": "pass the cleaned_image_path returned by fukidashi_clean_page"
                    }),
                    true,
                );
            }
        };
        let mut bubbles = bubbles;
        let (bundled_fallback_paths, font_substitutions) = match materialize_bundled_typeset_fonts(
            &workflow,
            &job_root,
            &mut bubbles,
        ) {
            Ok(paths) => paths,
            Err(error) => {
                return json_result(
                    &serde_json::json!({
                        "error": format!("bundled comic fonts are not usable: {error}"),
                        "next_step": "check that the managed jobs directory is writable and use the bundled-font release assets"
                    }),
                    true,
                );
            }
        };
        for (index, bubble) in bubbles.iter_mut().enumerate() {
            let Some(font) = bubble.font_path.as_deref() else {
                continue;
            };
            match workflow.materialize_font_path(&job_root, std::path::Path::new(font)) {
                Ok(managed) => bubble.font_path = Some(managed.display().to_string()),
                Err(error) => {
                    return json_result(
                        &serde_json::json!({
                            "error": format!("bubble {index} font {font:?} is not usable: {error}"),
                            "next_step": "use Comic Neue or another legitimate comic font as the primary; generic Windows UI faces are substituted, while fallback faces may provide coverage"
                        }),
                        true,
                    );
                }
            }
        }
        for (index, bubble) in bubbles.iter_mut().enumerate() {
            let requested = std::mem::take(&mut bubble.fallback_font_paths);
            let mut materialized = Vec::with_capacity(requested.len());
            for font in requested {
                match workflow.materialize_font_path(&job_root, std::path::Path::new(&font)) {
                    Ok(managed) => materialized.push(managed.display().to_string()),
                    Err(error) => {
                        return json_result(
                            &serde_json::json!({
                                "error": format!("bubble {index} fallback font {font:?} is not usable: {error}"),
                                "next_step": "use Comic Neue or another legitimate comic font as the primary; generic Windows UI faces are substituted, while fallback faces may provide coverage"
                            }),
                            true,
                        );
                    }
                }
            }
            bubble.fallback_font_paths = crate::workflow::order_fallback_font_paths(materialized);
        }
        let requested_fallbacks = fallback_font_paths;
        let mut fallback_font_paths =
            Vec::with_capacity(requested_fallbacks.len() + bundled_fallback_paths.len());
        for font in requested_fallbacks {
            match workflow.materialize_font_path(&job_root, std::path::Path::new(&font)) {
                Ok(managed) => fallback_font_paths.push(managed.display().to_string()),
                Err(error) => {
                    return json_result(
                        &serde_json::json!({
                            "error": format!("fallback font {font:?} is not usable: {error}"),
                            "next_step": "use a validated TTF/OTF/TTC from an approved font directory or managed job fonts directory"
                        }),
                        true,
                    );
                }
            }
        }
        fallback_font_paths.extend(bundled_fallback_paths);
        fallback_font_paths = crate::workflow::order_fallback_font_paths(fallback_font_paths);
        for bubble in &mut bubbles {
            let mut effective = bubble.fallback_font_paths.clone();
            effective.extend(fallback_font_paths.iter().cloned());
            effective.dedup();
            bubble.fallback_font_paths = crate::workflow::order_fallback_font_paths(effective);
        }
        let output_path = parent.join("rendered.png");
        let render_lock = match workflow.acquire_render_lock(&job_root) {
            Ok(lock) => lock,
            Err(error) => {
                return json_result(
                    &serde_json::json!({
                        "error": format!("typeset job is busy: {error}"),
                        "next_step": "wait for the current render to finish and retry"
                    }),
                    true,
                );
            }
        };
        let permit = match Arc::clone(&self.slots).acquire_owned().await {
            Ok(permit) => permit,
            Err(e) => {
                return json_result(
                    &serde_json::json!({"error":format!("typesetting worker capacity closed: {e}")}),
                    true,
                );
            }
        };
        // Historical effective global font: the managed path the renderer
        // actually used. Operator provenance stays in
        // `requested_global_font_path`; the historical rerender baseline must
        // be a usable managed value, not an external path that may disappear.
        let effective_global_font =
            effective_global_font_path(&workflow, &job_root, req_global_font.as_deref());
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _render_lock = render_lock;
            let mut value = crate::typeset::typeset_page_with_fallbacks(
                &image_path,
                &bubbles,
                &fallback_font_paths,
                &output_path,
            )?;
            if !font_substitutions.is_empty() {
                let mut substitutions = font_substitutions.clone();
                if let Some(reports) = value["bubbles"].as_array_mut() {
                    for substitution in &mut substitutions {
                        let Some(index) = substitution
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .and_then(|index| usize::try_from(index).ok())
                        else {
                            continue;
                        };
                        let Some(report) = reports.get_mut(index).and_then(|v| v.as_object_mut())
                        else {
                            continue;
                        };
                        if let Some(resolved) = report.get("resolved_font_path").cloned() {
                            substitution["resolved_font_path"] = resolved;
                        }
                        for key in ["requested_font_path", "replacement_primary_font", "reason"] {
                            if let Some(value) = substitution.get(key) {
                                report.insert(key.to_owned(), value.clone());
                            }
                        }
                        report.insert("font_substituted".into(), serde_json::Value::Bool(true));
                    }
                }
                value["font_substitutions"] = serde_json::Value::Array(substitutions);
            }
            let mut qa = crate::typeset::post_render_qa(&clean_artifact, &value)?;
            if let Some(qa_obj) = qa.as_object_mut() {
                qa_obj.insert(
                    "global_font_path".to_owned(),
                    effective_global_font
                        .clone()
                        .map(serde_json::Value::String)
                        .unwrap_or(serde_json::Value::Null),
                );
                qa_obj.insert(
                    "requested_global_font_path".to_owned(),
                    req_global_font
                        .clone()
                        .map(serde_json::Value::String)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            let sidecar = workflow
                .register_render_locked(
                    &output_path,
                    &clean_artifact,
                    serde_json::json!({"request_bubbles": bubbles, "report": value.clone()}),
                    qa.clone(),
                )
                .map_err(|error| anyhow::anyhow!("render validation failed: {error}"))?;
            value["workflow"] = serde_json::json!({
                "stage": "rendered",
                "cleaned_image": clean_artifact.cleaned_image,
                "source_image": clean_artifact.source_image,
                "render_sidecar_path": sidecar,
                "qa": qa,
            });
            Ok::<_, anyhow::Error>(value)
        })
        .await;
        match result {
            Ok(Ok(v)) => json_result(&v, false),
            Ok(Err(e)) => json_result(&serde_json::json!({"error":e.to_string()}), true),
            Err(e) => json_result(
                &serde_json::json!({"error":format!("typesetting worker failed: {e}")}),
                true,
            ),
        }
    }

    async fn review_and_export_inner<F>(
        &self,
        req: ReviewAndExportRequest,
        launch: F,
    ) -> CallToolResult
    where
        F: FnOnce(&str) -> Result<(), FukidashiError>,
    {
        if !matches!(req.format.as_str(), "zip" | "epub" | "html_monolith") {
            return json_result(
                &serde_json::json!({
                    "protocol": "review-v1",
                    "error": "format must be zip, epub, or html_monolith"
                }),
                true,
            );
        }
        let served = self
            .serve_editor(Parameters(EditorRequest {
                image_path: req.image_path,
                job_path: req.job_path,
                job_id: req.job_id,
                json_data: None,
                reopen_completed: false,
            }))
            .await;
        let served = match extract_tool_json(served, "serve editor") {
            Ok(value) => value,
            Err(error) => {
                return json_result(
                    &serde_json::json!({
                        "protocol": "review-v1",
                        "error": error.to_string(),
                        "next_step": "finish every page and call fukidashi_review_and_export with the exact managed job id"
                    }),
                    true,
                );
            }
        };
        // A review already approved for export comes back from serve_editor
        // without a live editor session. Skip the browser launch and the
        // review wait entirely and export against the recorded decision —
        // re-waiting would either block on a dead session or consume twice.
        if served
            .get("editor_kind")
            .and_then(serde_json::Value::as_str)
            == Some("already_completed")
        {
            let url = "native://already-completed".to_owned();
            let review = served
                .get("review")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let persistence_path = match served
                .get("persistence_path")
                .and_then(serde_json::Value::as_str)
            {
                Some(value) => PathBuf::from(value),
                None => {
                    return json_result(
                        &serde_json::json!({
                            "protocol":"review-v1","status":"approved_export_blocked",
                            "editor_url":url,
                            "error":"already-completed review returned no persistence path"
                        }),
                        true,
                    );
                }
            };
            let project_dir = match persistence_path.parent() {
                Some(value) => value.to_path_buf(),
                None => {
                    return json_result(
                        &serde_json::json!({
                            "protocol":"review-v1","status":"approved_export_blocked",
                            "editor_url":url,
                            "error":"already-completed review persistence path has no managed job parent"
                        }),
                        true,
                    );
                }
            };
            return self
                .export_approved_project(project_dir, req.format, review, url)
                .await;
        }
        let url = match served.get("url").and_then(serde_json::Value::as_str) {
            Some(value) => value.to_owned(),
            None => {
                return json_result(
                    &serde_json::json!({"protocol":"review-v1","error":"serve editor returned no review URL"}),
                    true,
                );
            }
        };

        let review_session_id = match served
            .get("review_session_id")
            .and_then(serde_json::Value::as_str)
        {
            Some(value) => value.to_owned(),
            None => {
                return json_result(
                    &serde_json::json!({"protocol":"review-v1","error":"serve editor returned no review_session_id","editor_url":url}),
                    true,
                );
            }
        };
        let revision = match served
            .get("review_revision")
            .and_then(serde_json::Value::as_u64)
        {
            Some(value) => value,
            None => {
                return json_result(
                    &serde_json::json!({"protocol":"review-v1","error":"serve editor returned no review_revision","editor_url":url}),
                    true,
                );
            }
        };
        let native_editor = served
            .get("editor_kind")
            .and_then(serde_json::Value::as_str)
            == Some("native");
        if !native_editor && let Err(error) = launch(&url) {
            return json_result(
                &serde_json::json!({
                    "protocol": "review-v1",
                    "status": "browser_launch_failed",
                    "editor_url": url,
                    "review_session_id": review_session_id,
                    "review_revision": revision,
                    "error": error.to_string(),
                    "next_action": {
                        "tool": "fukidashi_wait_for_review",
                        "arguments": {
                            "review_session_id": review_session_id,
                            "revision": revision,
                            "timeout_seconds": req.timeout_seconds,
                        },
                    },
                }),
                true,
            );
        }
        let review =
            match crate::editor::wait_for_review(&review_session_id, revision, req.timeout_seconds)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    let value = serde_json::json!({
                        "protocol": "review-v1",
                        "status": "review_wait_failed",
                        "editor_url": url,
                        "review_session_id": review_session_id,
                        "review_revision": revision,
                        "error": error.to_string(),
                    });
                    return json_result(&value, true);
                }
            };
        let action = review
            .get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if action == "request_fixes" {
            let persistence_path = served
                .get("persistence_path")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from);
            let project_dir = persistence_path.as_ref().and_then(|path| path.parent());
            let job_id = project_dir
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .map(str::to_owned);
            let state_revision = project_dir
                .and_then(|dir| std::fs::read(dir.join("project.json")).ok())
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|state| state.get("state_revision")?.as_u64());
            let value = serde_json::json!({
                "protocol": "review-v1",
                "status": "fixes_requested",
                "editor_url": url,
                "job_id": job_id,
                "format": req.format,
                "state_revision": state_revision,
                "review": review,
                "feedback": review.get("feedback").cloned().unwrap_or(serde_json::Value::Array(Vec::new())),
                "next_action": "Apply the returned feedback and call fukidashi_review_and_export with this job_id and format",
            });
            return json_result(&value, false);
        }
        if action != "approve_export" {
            return json_result(
                &serde_json::json!({
                    "protocol": "review-v1",
                    "status": "review_wait_failed",
                    "editor_url": url,
                    "error": "review returned no supported action"
                }),
                true,
            );
        }
        let persistence_path = match served
            .get("persistence_path")
            .and_then(serde_json::Value::as_str)
        {
            Some(value) => PathBuf::from(value),
            None => {
                return json_result(
                    &serde_json::json!({"protocol":"review-v1","status":"approved_export_blocked","editor_url":url,"error":"serve editor returned no persistence path"}),
                    true,
                );
            }
        };
        let project_dir = match persistence_path.parent() {
            Some(value) => value.to_path_buf(),
            None => {
                return json_result(
                    &serde_json::json!({"protocol":"review-v1","status":"approved_export_blocked","editor_url":url,"error":"review persistence path has no managed job parent"}),
                    true,
                );
            }
        };
        self.export_approved_project(project_dir, req.format, review, url)
            .await
    }

    /// Export an already-approved review's project directory and wrap the
    /// result in the standard review-v1 envelope. Shared by the live-review
    /// path and the already-completed fast path in `review_and_export_inner`.
    ///
    /// The exporter proves it packages the exact artifacts of the approved
    /// frozen semantic snapshot: revision, job identity, snapshot signature,
    /// checkpoint identity, expected page set, completed entries, and current
    /// validated artifact hashes/signatures must ALL agree. Approving snapshot
    /// A and then exporting modified semantics B is rejected even when the
    /// page count is unchanged (Blocker 7).
    fn verify_approval_binding(
        project_dir: &Path,
        review: &serde_json::Value,
    ) -> anyhow::Result<bool> {
        // The wait response is intentionally compact and may omit audit
        // details. Once a review exists on disk, that file is authoritative;
        // the caller-supplied value is only a compatibility fallback for
        // older/unit fixtures that have no persisted review file.
        let persisted_review = project_dir
            .join("review.json")
            .is_file()
            .then(|| std::fs::read(project_dir.join("review.json")))
            .transpose()?
            .map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes))
            .transpose()?;
        let review = persisted_review.as_ref().unwrap_or(review);
        let binding = review
            .get("audit")
            .and_then(|audit| audit.as_array())
            .into_iter()
            .flatten()
            .rev()
            .find(|entry| {
                entry.get("event").and_then(|event| event.as_str()) == Some("approve_export")
                    && entry.get("approval").is_some()
            })
            .and_then(|entry| entry.get("approval"))
            .cloned();
        let Some(binding) = binding else {
            // Legacy approval without a frozen snapshot keeps the old loose
            // semantics for compatibility.
            return Ok(false);
        };
        let revision = review
            .get("revision")
            .and_then(|revision| revision.as_u64());
        let binding_revision = binding
            .get("revision")
            .and_then(|revision| revision.as_u64());
        if revision != binding_revision {
            anyhow::bail!("approval binding revision does not match the review revision");
        }
        let Some(revision) = revision else {
            anyhow::bail!("approval binding review has no revision");
        };
        // approved_pages must be exactly the full 0..N set, not just a
        // matching count.
        let approved_pages = review
            .get("approved_pages")
            .and_then(|pages| pages.as_array())
            .map(|pages| {
                pages
                    .iter()
                    .filter_map(|page| page.as_u64())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let packaged_pages = binding
            .get("packaged_pages")
            .and_then(|pages| pages.as_u64())
            .unwrap_or(0);
        let manifest_bytes = std::fs::read(project_dir.join("project.json"))
            .map_err(|_| anyhow::anyhow!("approval binding cannot read project.json"))?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
            .map_err(|_| anyhow::anyhow!("approval binding cannot parse project.json"))?;
        let manifest_pages = manifest
            .get("pages")
            .and_then(|pages| pages.as_array())
            .cloned()
            .unwrap_or_default();
        let expected_set: Vec<u64> = (0..manifest_pages.len() as u64).collect();
        let mut approved_sorted = approved_pages.clone();
        approved_sorted.sort_unstable();
        if approved_sorted != expected_set
            || packaged_pages as usize != manifest_pages.len()
            || manifest_pages.is_empty()
        {
            anyhow::bail!("approval binding does not cover the approved pages");
        }
        // Job identity: the binding, the checkpoint, and the directory must
        // all name the same job.
        let dir_job_id = project_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let binding_job_id = binding
            .get("job_id")
            .and_then(|job| job.as_str())
            .unwrap_or("");
        if !binding_job_id.is_empty() && binding_job_id != dir_job_id {
            anyhow::bail!("approval binding job does not match this project directory");
        }
        let checkpoint =
            crate::approval::load_approval_checkpoint(project_dir, revision).ok_or_else(|| {
                anyhow::anyhow!(
                    "approval binding has no matching checkpoint for revision {revision}; re-approve the current revision"
                )
            })?;
        if checkpoint.job_id != dir_job_id
            || (!binding_job_id.is_empty() && checkpoint.job_id != binding_job_id)
        {
            anyhow::bail!("approval checkpoint job does not match this project directory");
        }
        let binding_signature = binding
            .get("snapshot_signature")
            .and_then(|signature| signature.as_str())
            .unwrap_or("");
        if binding_signature.is_empty() || checkpoint.snapshot_signature != binding_signature {
            anyhow::bail!(
                "approval checkpoint does not match the approved snapshot; re-approve the current revision"
            );
        }
        // Full-state binding: recompute the post-render semantic signature of
        // every current page. ANY semantic drift since approval — including on
        // pages that were clean at approval time — blocks the export.
        let current_state = crate::editor::approval_state_signature(&manifest, Some(project_dir));
        let current_state_signature = crate::approval::snapshot_signature(&current_state);
        let binding_state = binding
            .get("state_signature")
            .and_then(|signature| signature.as_str())
            .unwrap_or("");
        if !binding_state.is_empty() && binding_state != current_state_signature {
            anyhow::bail!(
                "project semantics changed since approval (snapshot {binding_signature}); re-approve the current revision so the export binds to its frozen snapshot"
            );
        }
        // Per-page proof: every dirty page of the frozen plan must hold a
        // complete checkpoint entry whose semantic signature, input hashes,
        // output path, and live output bytes validate right now.
        let dirty_pages: Vec<usize> = binding
            .get("dirty_pages")
            .and_then(|pages| pages.as_array())
            .map(|pages| {
                pages
                    .iter()
                    .filter_map(|page| page.as_u64().and_then(|index| usize::try_from(index).ok()))
                    .collect()
            })
            .unwrap_or_default();
        let managed = project_dir.join("job.json").is_file()
            || project_dir.join(".fukidashi-job.json").is_file();
        let workflow = if managed {
            project_dir
                .parent()
                .map(|parent| crate::workflow::Workflow::new(parent.to_path_buf()))
                .transpose()
                .map_err(|error| {
                    anyhow::anyhow!("approval binding cannot open workflow: {error}")
                })?
        } else {
            None
        };
        for page_index in dirty_pages {
            let page = manifest_pages.get(page_index).ok_or_else(|| {
                anyhow::anyhow!("approval binding page {page_index} is outside the current project")
            })?;
            let semantic_render_signature =
                crate::editor::post_render_page_signature(&manifest, page);
            let page_id = page
                .get("id")
                .and_then(|id| id.as_str())
                .filter(|id| !id.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("page-{page_index}"));
            let output_path = page
                .get("rendered_image_path")
                .and_then(|path| path.as_str())
                .map(|path| {
                    let candidate = std::path::Path::new(path);
                    if candidate.is_absolute() {
                        candidate.to_path_buf()
                    } else {
                        project_dir.join(candidate)
                    }
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("approval binding page {page_index} has no rendered image path")
                })?;
            let (source_sha256, clean_sha256) = match workflow.as_ref() {
                Some(managed_workflow) => {
                    let corrected = page
                        .get("corrected_cleaned_image_path")
                        .and_then(|path| path.as_str())
                        .map(std::path::PathBuf::from)
                        .filter(|path| {
                            let absolute = if path.is_absolute() {
                                path.clone()
                            } else {
                                project_dir.join(path)
                            };
                            absolute.is_file()
                        });
                    let base = page
                        .get("cleaned_image_path")
                        .and_then(|path| path.as_str())
                        .map(std::path::PathBuf::from);
                    let mut hashes = (String::new(), String::new());
                    for candidate in corrected.into_iter().chain(base) {
                        let absolute = if candidate.is_absolute() {
                            candidate
                        } else {
                            project_dir.join(candidate)
                        };
                        if let Ok(clean) = managed_workflow.validate_clean_input(&absolute) {
                            hashes = (clean.source_sha256, clean.cleaned_sha256);
                            break;
                        }
                    }
                    hashes
                }
                None => (String::new(), String::new()),
            };
            if workflow.as_ref().is_some_and(|managed_workflow| {
                managed_workflow
                    .validate_render_input(&output_path)
                    .is_err()
            }) {
                anyhow::bail!(
                    "approval binding page {page_index} cached render is invalid; re-approve the current revision"
                );
            }
            let expected = crate::approval::ExpectedPageProvenance {
                page_index,
                page_id,
                semantic_render_signature,
                source_sha256,
                clean_sha256,
                output_path,
            };
            let entry = checkpoint.pages.get(&page_index.to_string()).ok_or_else(|| {
                anyhow::anyhow!(
                    "approval binding page {page_index} has no completed checkpoint entry; re-approve the current revision"
                )
            })?;
            let live_hash = crate::approval::sha256_file(&expected.output_path).ok();
            if !crate::approval::checkpoint_entry_valid(entry, &expected, live_hash.as_deref()) {
                anyhow::bail!(
                    "approval binding page {page_index} no longer matches its approved artifact; re-approve the current revision"
                );
            }
        }
        Ok(true)
    }

    /// Revalidate every managed rendered artifact while the caller holds the
    /// job render lease. Approval checkpoints cover dirty pages; export must
    /// also prove that clean/reused pages still point at their own live,
    /// sidecar-backed artifacts before packaging begins.
    fn validate_managed_export_artifacts(
        project_dir: &Path,
        bound_approval: bool,
    ) -> anyhow::Result<()> {
        let managed = project_dir.join("job.json").is_file()
            || project_dir.join(".fukidashi-job.json").is_file();
        if !managed {
            return Ok(());
        }
        let workflow = Workflow::new(
            project_dir
                .parent()
                .ok_or_else(|| anyhow::anyhow!("managed export job has no jobs root"))?
                .to_path_buf(),
        )?;
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project_dir.join("project.json")).map_err(
                |error| anyhow::anyhow!("read project.json for artifact validation: {error}"),
            )?)?;
        let pages = manifest
            .get("pages")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("managed project has no pages"))?;
        // Sidecars written before semantic_render_signature was introduced
        // still contain enough provenance to be checked against the cached
        // editor state.  Defer this comparison until the first legacy page
        // is found; modern sidecars keep the existing fast path.
        let mut legacy_dirty_pages: Option<BTreeSet<usize>> = None;
        let mut legacy_sidecars = Vec::new();
        for (index, page) in pages.iter().enumerate() {
            let rendered = page
                .get("rendered_image_path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("page {index} has no rendered image path"))?;
            let rendered_path = {
                let candidate = Path::new(rendered);
                if candidate.is_absolute() {
                    candidate.to_path_buf()
                } else {
                    project_dir.join(candidate)
                }
            };
            let artifact = workflow
                .validate_render_input(&rendered_path)
                .map_err(|error| {
                    anyhow::anyhow!(
                        "managed export page {index} rendered artifact is invalid: {error}"
                    )
                })?;
            let mut expected_semantic = String::new();
            let mut legacy_signature_missing = false;
            if bound_approval {
                expected_semantic = crate::editor::post_render_page_signature(&manifest, page);
                let actual_semantic = artifact
                    .qa
                    .get("semantic_render_signature")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                match actual_semantic {
                    Some(actual_semantic)
                        if !crate::approval::semantic_render_signatures_equal(
                            &actual_semantic,
                            &expected_semantic,
                        ) =>
                    {
                        anyhow::bail!(
                            "managed export page {index} render sidecar semantic signature is stale; re-render and re-approve"
                        );
                    }
                    Some(_) => {}
                    None => {
                        legacy_signature_missing = true;
                        // A legacy sidecar has no independent semantic claim.
                        // Before upgrading it, compare its cached typeset
                        // payload with every page in the current project. The
                        // workflow comparison ignores renderer-only metadata,
                        // validates typeset completeness, and still forces a
                        // rerender for a changed translation/geometry/stroke.
                        if legacy_dirty_pages.is_none() {
                            let dirty_pages = workflow
                                .editor_render_plan(project_dir, &manifest)
                                .map_err(|error| {
                                    anyhow::anyhow!(
                                        "managed export legacy sidecar verification failed: {error}"
                                    )
                                })?
                                .into_iter()
                                .collect::<BTreeSet<_>>();
                            legacy_dirty_pages = Some(dirty_pages);
                        }
                        let dirty_pages = legacy_dirty_pages
                            .as_ref()
                            .expect("legacy dirty pages were just stored");
                        if dirty_pages.contains(&index) {
                            anyhow::bail!(
                                "managed export page {index} legacy render sidecar does not match current semantics; re-render and re-approve"
                            );
                        }
                    }
                }
                if artifact.rendered_sha256.trim().is_empty() {
                    anyhow::bail!(
                        "managed export page {index} render sidecar has no rendered byte hash; re-render and re-approve"
                    );
                }
                let actual_hash = crate::approval::sha256_file(&rendered_path)?;
                if actual_hash != artifact.rendered_sha256 {
                    anyhow::bail!(
                        "managed export page {index} rendered byte hash is stale; re-render and re-approve"
                    );
                }
            }
            let source = page
                .get("image_path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("page {index} has no source image path"))?;
            let source_path = {
                let candidate = Path::new(source);
                if candidate.is_absolute() {
                    candidate.to_path_buf()
                } else {
                    project_dir.join(candidate)
                }
            };
            let expected_source = std::fs::canonicalize(&source_path).map_err(|error| {
                anyhow::anyhow!("page {index} source image cannot be resolved: {error}")
            })?;
            let actual_source = std::fs::canonicalize(&artifact.source_image).map_err(|error| {
                anyhow::anyhow!("page {index} render source cannot be resolved: {error}")
            })?;
            if expected_source != actual_source {
                anyhow::bail!(
                    "managed export page {index} rendered artifact is bound to the wrong source page"
                );
            }
            if bound_approval {
                let expected_clean = page
                    .get("corrected_cleaned_image_path")
                    .or_else(|| page.get("cleaned_image_path"))
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("page {index} has no clean image path"))?;
                let expected_clean_path = {
                    let candidate = Path::new(expected_clean);
                    if candidate.is_absolute() {
                        candidate.to_path_buf()
                    } else {
                        project_dir.join(candidate)
                    }
                };
                let expected_clean =
                    std::fs::canonicalize(&expected_clean_path).map_err(|error| {
                        anyhow::anyhow!("page {index} clean image cannot be resolved: {error}")
                    })?;
                let actual_clean =
                    std::fs::canonicalize(&artifact.cleaned_image).map_err(|error| {
                        anyhow::anyhow!(
                            "page {index} render clean image cannot be resolved: {error}"
                        )
                    })?;
                let clean_alias = if expected_clean != actual_clean {
                    if !legacy_signature_missing {
                        anyhow::bail!(
                            "managed export page {index} rendered artifact is bound to the wrong clean image"
                        );
                    }
                    let expected_clean_artifact = workflow
                        .validate_clean_input(&expected_clean_path)
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "managed export page {index} legacy clean alias cannot be validated: {error}"
                            )
                        })?;
                    let actual_clean_artifact = workflow
                        .validate_clean_input(&artifact.cleaned_image)
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "managed export page {index} legacy render clean artifact is invalid: {error}"
                            )
                        })?;
                    if expected_clean_artifact.source_sha256 != actual_clean_artifact.source_sha256
                        || expected_clean_artifact.cleaned_sha256
                            != actual_clean_artifact.cleaned_sha256
                        || std::fs::canonicalize(&expected_clean_artifact.source_image)?
                            != std::fs::canonicalize(&actual_clean_artifact.source_image)?
                    {
                        anyhow::bail!(
                            "managed export page {index} legacy clean alias has different provenance; re-render and re-approve"
                        );
                    }
                    Some(expected_clean)
                } else {
                    None
                };
                if legacy_signature_missing {
                    legacy_sidecars.push((
                        PathBuf::from(format!("{}.fukidashi-render.json", rendered_path.display())),
                        expected_semantic,
                        clean_alias,
                    ));
                }
            }
        }
        // Upgrade only after every page has passed the export checks.  Each
        // update is atomic and records the signature derived from the
        // already-bound project state, so a partial failed export cannot
        // create a false approval; a later retry simply sees a modern
        // sidecar. Existing non-missing signatures are never rewritten.
        for (sidecar_path, semantic_signature, clean_alias) in legacy_sidecars {
            let mut sidecar: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&sidecar_path).map_err(|error| {
                    anyhow::anyhow!(
                        "read legacy render sidecar {} for migration: {error}",
                        sidecar_path.display()
                    )
                })?)
                .map_err(|error| {
                    anyhow::anyhow!(
                        "parse legacy render sidecar {} for migration: {error}",
                        sidecar_path.display()
                    )
                })?;
            if let Some(expected_clean_path) = clean_alias {
                sidecar["cleaned_image"] =
                    serde_json::Value::String(expected_clean_path.display().to_string());
                sidecar["clean_sidecar"] = serde_json::Value::String(format!(
                    "{}.fukidashi-clean.json",
                    expected_clean_path.display()
                ));
            }
            {
                let qa = sidecar
                    .get_mut("qa")
                    .and_then(serde_json::Value::as_object_mut)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "legacy render sidecar {} has no QA object; re-render and re-approve",
                            sidecar_path.display()
                        )
                    })?;
                match qa.get("semantic_render_signature") {
                    Some(serde_json::Value::String(existing))
                        if crate::approval::semantic_render_signatures_equal(
                            existing,
                            &semantic_signature,
                        ) =>
                    {
                        continue;
                    }
                    Some(_) => anyhow::bail!(
                        "managed export render sidecar {} changed during validation",
                        sidecar_path.display()
                    ),
                    None => {
                        qa.insert(
                            "semantic_render_signature".to_owned(),
                            serde_json::Value::String(semantic_signature),
                        );
                    }
                }
            }
            crate::editor::atomic_json_save(&sidecar_path, &sidecar).map_err(|error| {
                anyhow::anyhow!(
                    "migrate legacy render sidecar {}: {error}",
                    sidecar_path.display()
                )
            })?;
        }
        Ok(())
    }

    async fn export_approved_project(
        &self,
        project_dir: PathBuf,
        format: String,
        review: serde_json::Value,
        url: String,
    ) -> CallToolResult {
        // The actual export path re-reads the authoritative review while
        // holding the writer lease. This also covers the compact wait result,
        // which intentionally does not carry the full audit trail.
        let export = self
            .export(Parameters(ExportRequest {
                project_dir: project_dir.display().to_string(),
                format,
            }))
            .await;
        match extract_tool_json(export, "review export") {
            Ok(export) => {
                let value = serde_json::json!({
                    "protocol": "review-v1",
                    "status": "exported",
                    "editor_url": url,
                    "review": review,
                    "export": export,
                });
                json_result(&value, false)
            }
            Err(error) => json_result(
                &serde_json::json!({
                    "protocol": "review-v1",
                    "status": "approved_export_blocked",
                    "editor_url": url,
                    "review": review,
                    "error": error.to_string(),
                    "next_action": "resolve the export validation error and call fukidashi_export with the exact managed project directory"
                }),
                true,
            ),
        }
    }

    #[tool(
        name = "fukidashi_review_and_export",
        description = "Serve the managed local editor, open it in the platform default browser, and keep this single MCP call pending until the user submits review. Return actionable feedback when fixes are requested; after explicit approval automatically export the requested format (default zip). The server owns the review session and exact managed paths."
    )]
    pub async fn review_and_export(
        &self,
        Parameters(req): Parameters<ReviewAndExportRequest>,
    ) -> CallToolResult {
        self.review_and_export_inner(req, launch_default_browser)
            .await
    }

    #[tool(
        name = "fukidashi_retranslation_source",
        description = "For a missing-dialogue review flag, crop and OCR the exact bbox from the ORIGINAL managed source page. Use the exact zero-based page. Pass bbox as an object {x1,y1,x2,y2}, a four-number array [x1,y1,x2,y2], or flat numeric x1,y1,x2,y2 fields; region/bounds/rect are accepted aliases. The rendered/cleaned image is never used for source recognition. Returns normalized bbox, source_ocr, and the crop image for visual review; pass that source_ocr and bbox to fukidashi_retranslation_submit."
    )]
    pub async fn retranslation_source(
        &self,
        Parameters(req): Parameters<RetranslationSourceRequest>,
    ) -> CallToolResult {
        let operation = async {
            let job = self.workflow.resolve_managed_job_id(&req.job_id)?;
            let _render_lock = self.workflow.acquire_render_lock(&job)?;
            let source = self
                .workflow
                .managed_source_paths_for_editor(&job)?
                .get(req.page)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "page index {} is outside the managed page inventory",
                        req.page
                    )
                })?;
            let page_state: serde_json::Value =
                serde_json::from_slice(&std::fs::read(job.join("project.json"))?)?;
            let page = page_state
                .get("pages")
                .and_then(serde_json::Value::as_array)
                .and_then(|pages| pages.get(req.page))
                .ok_or_else(|| {
                    anyhow::anyhow!("page index {} is outside editor state", req.page)
                })?;
            let page_image = page
                .get("image_path")
                .or_else(|| page.get("source_image"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("review page has no source image identity"))?;
            let page_image = PathBuf::from(page_image);
            let page_image = if page_image.is_absolute() {
                page_image
            } else {
                job.join(page_image)
            };
            let page_image = std::fs::canonicalize(page_image)?;
            if page_image != source {
                anyhow::bail!(
                    "review page index does not match the managed page inventory; refresh feedback"
                );
            }
            let rect = parse_retranslation_bbox(req.bbox.as_ref(), req.x1, req.y1, req.x2, req.y2)?;
            let has_flag = page
                .get("issues")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|issues| {
                    issues.iter().any(|issue| {
                        issue.get("origin").and_then(serde_json::Value::as_str)
                            == Some("missing-dialogue-flag")
                            && issue.get("issue_type").and_then(serde_json::Value::as_str)
                                == Some("wrong_or_missing_bubble")
                            && issue
                                .get("bbox")
                                .cloned()
                                .and_then(|value| serde_json::from_value::<Rect>(value).ok())
                                .is_some_and(|flag_bbox| rects_close(flag_bbox, rect))
                    })
                });
            if !has_flag {
                anyhow::bail!("bbox does not match a current missing-dialogue flag on this page");
            }
            let (width, height) = image::image_dimensions(&source)?;
            if rect.x1 < 0.0 || rect.y1 < 0.0 || rect.x2 > width as f32 || rect.y2 > height as f32 {
                anyhow::bail!("source bbox is outside the managed page bounds");
            }
            let x_pad = ((rect.x2 - rect.x1) * 0.12).max(12.0);
            let y_pad = ((rect.y2 - rect.y1) * 0.12).max(12.0);
            let crop_bbox = Rect {
                x1: (rect.x1 - x_pad).max(0.0),
                y1: (rect.y1 - y_pad).max(0.0),
                x2: (rect.x2 + x_pad).min(width as f32),
                y2: (rect.y2 + y_pad).min(height as f32),
            };
            let left = crop_bbox.x1.floor() as u32;
            let top = crop_bbox.y1.floor() as u32;
            let right = crop_bbox.x2.ceil().min(width as f32) as u32;
            let bottom = crop_bbox.y2.ceil().min(height as f32) as u32;
            let permit = Arc::clone(&self.slots)
                .acquire_owned()
                .await
                .map_err(|error| anyhow::anyhow!("OCR worker capacity closed: {error}"))?;
            let ocr = Arc::clone(&self.ocr);
            let config = self.config.clone();
            let source_for_worker = source.clone();
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let _permit = permit;
                let original = image::open(&source_for_worker)?.to_rgb8();
                let crop = image::imageops::crop_imm(
                    &original,
                    left,
                    top,
                    right.saturating_sub(left),
                    bottom.saturating_sub(top),
                )
                .to_image();
                let mut crop_file = tempfile::Builder::new().suffix(".png").tempfile()?;
                image::DynamicImage::ImageRgb8(crop.clone())
                    .write_to(crop_file.as_file_mut(), image::ImageFormat::Png)?;
                crop_file.as_file().sync_all()?;
                let crop_bytes = std::fs::read(crop_file.path())?;
                let mut engine = ocr
                    .lock()
                    .map_err(|_| anyhow::anyhow!("OCR session lock poisoned"))?;
                let analysis = engine.analyze(&config, crop_file.path(), None, None);
                let _ = engine.finish_heavy_call(config.session_recycle_pages());
                match analysis {
                    Ok(analysis) => {
                        let texts = analysis
                            .translation_handoff
                            .items
                            .iter()
                            .map(|item| item.source_text.trim())
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>();
                        Ok((crop_bytes, texts.join("\n"), None))
                    }
                    Err(error) => Ok((crop_bytes, String::new(), Some(error.to_string()))),
                }
            })
            .await
            .map_err(|error| anyhow::anyhow!("source crop OCR worker failed: {error}"))??;
            drop(_render_lock);
            Ok::<_, anyhow::Error>((crop_bbox, rect, result.0, result.1, result.2))
        }
        .await;
        match operation {
            Ok((crop_bbox, rect, crop_bytes, source_ocr, ocr_error)) => {
                let value = serde_json::json!({
                    "protocol": "retranslation-v1",
                    "status": if source_ocr.trim().is_empty() { "ocr_empty" } else { "source_ready" },
                    "job_id": req.job_id,
                    "page": req.page,
                    "bbox": rect,
                    "source_crop_bbox": crop_bbox,
                    "source_ocr": source_ocr,
                    "ocr_error": ocr_error,
                    "source": "original_managed_page"
                });
                let encoded =
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, crop_bytes);
                CallToolResult::success(vec![
                    ContentBlock::text(
                        serde_json::to_string(&value)
                            .unwrap_or_else(|error| format!("{{\"error\":{error}}}")),
                    ),
                    ContentBlock::image(encoded, "image/png"),
                ])
            }
            Err(error) => json_result(
                &serde_json::json!({
                    "protocol": "retranslation-v1",
                    "status": "source_ocr_failed",
                    "error": format!("{error:#}"),
                    "next_action": "Verify the page and bbox from missing-dialogue feedback, then retry source OCR"
                }),
                true,
            ),
        }
    }

    #[tool(
        name = "fukidashi_retranslation_submit",
        description = "Submit a fresh translation for one reviewed item. For an existing bubble, pass job_id, zero-based page, bubble_id, expected_current_translation, and translation. For missing-dialogue feedback, pass job_id, page, bbox as {x1,y1,x2,y2}, [x1,y1,x2,y2], or flat numeric x1,y1,x2,y2 fields, plus translation; region/bounds/rect are accepted aliases. First call fukidashi_retranslation_source with the same coordinates to inspect/OCR the original-source crop, then optionally pass source_ocr or corrected source_text. If the flagged region overlaps an existing empty bubble, the server fills that bubble instead of adding a duplicate; otherwise it adds a manual bubble. It then cleans/typesets against the original page. Optionally pass the original export format (zip, epub, or html_monolith). The tool rejects stale or unflagged items and rolls back the page artifacts on render failure."
    )]
    pub async fn retranslation_submit(
        &self,
        Parameters(req): Parameters<RetranslationSubmitRequest>,
    ) -> CallToolResult {
        let result = (|| -> anyhow::Result<serde_json::Value> {
            if req.translation.trim().is_empty() {
                anyhow::bail!("translation must not be empty");
            }
            if req.translation.len() > 4096 {
                anyhow::bail!("translation is too long (maximum 4096 UTF-8 bytes)");
            }
            if req
                .format
                .as_deref()
                .is_some_and(|format| !matches!(format, "zip" | "epub" | "html_monolith"))
            {
                anyhow::bail!("format must be zip, epub, or html_monolith");
            }
            let job = self.workflow.resolve_managed_job_id(&req.job_id)?;
            // Hold the same cross-process lease as editor saves/renders from
            // before reading project.json until the transaction commits or
            // rolls back. This makes the revision check and backups meaningful.
            let _render_lock = self.workflow.acquire_render_lock(&job)?;
            let project_path = job.join("project.json");
            let bytes = std::fs::read(&project_path)
                .map_err(|e| anyhow::anyhow!("read managed editor state: {e}"))?;
            let mut state: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("parse managed editor state: {e}"))?;
            let revision = state
                .get("state_revision")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            if req
                .state_revision
                .is_some_and(|expected| expected != revision)
            {
                anyhow::bail!(
                    "stale editor state revision {}; current revision is {revision}; refresh review feedback",
                    req.state_revision.unwrap()
                );
            }
            let sources = self.workflow.managed_source_paths_for_editor(&job)?;
            let source = sources.get(req.page).cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "page index {} is outside the managed page inventory",
                    req.page
                )
            })?;
            let page = state
                .get_mut("pages")
                .and_then(serde_json::Value::as_array_mut)
                .and_then(|pages| pages.get_mut(req.page))
                .ok_or_else(|| anyhow::anyhow!("page index {} is outside this job", req.page))?;
            let page_source = page
                .get("image_path")
                .or_else(|| page.get("source_image"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("review page has no source image identity"))?;
            let page_source = PathBuf::from(page_source);
            let page_source = if page_source.is_absolute() {
                page_source
            } else {
                job.join(page_source)
            };
            let page_source = std::fs::canonicalize(&page_source)
                .map_err(|e| anyhow::anyhow!("resolve review page source: {e}"))?;
            if page_source != source {
                anyhow::bail!(
                    "review page index does not match the managed page inventory; refresh feedback"
                );
            }
            let target_id = if let Some(bubble_id) = req.bubble_id.as_deref() {
                let bubbles = page
                    .get_mut("bubbles")
                    .and_then(serde_json::Value::as_array_mut)
                    .ok_or_else(|| anyhow::anyhow!("page has no editable bubbles"))?;
                let matches = bubbles
                    .iter_mut()
                    .filter(|bubble| {
                        bubble.get("id").and_then(serde_json::Value::as_str) == Some(bubble_id)
                    })
                    .collect::<Vec<_>>();
                if matches.len() != 1 {
                    anyhow::bail!(
                        "bubble_id must identify exactly one bubble on the requested page"
                    );
                }
                let bubble = &mut matches.into_iter().next().unwrap();
                if bubble
                    .get("retranslate_requested")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
                {
                    anyhow::bail!("bubble is not currently marked for retranslation");
                }
                let current = bubble
                    .get("translation")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if Some(current) != req.expected_current_translation.as_deref() {
                    anyhow::bail!(
                        "bubble translation changed since feedback; refresh review feedback before submitting"
                    );
                }
                let source_text = ["source_ocr", "source_text", "text"]
                    .into_iter()
                    .filter_map(|key| bubble.get(key).and_then(serde_json::Value::as_str))
                    .find(|text| !text.trim().is_empty())
                    .unwrap_or("");
                if source_text.trim().is_empty() {
                    anyhow::bail!(
                        "bubble has no source OCR text; rerun OCR or provide source text in the editor before requesting retranslation"
                    );
                }
                bubble["translation"] = serde_json::Value::String(req.translation.clone());
                bubble["retranslate_requested"] = serde_json::Value::Bool(false);
                // A retranslation request is an explicit decision to replace a
                // preserve-source result for this bubble.
                bubble["preserve_source"] = serde_json::Value::Bool(false);
                bubble["keep_source"] = serde_json::Value::Bool(false);
                bubble_id.to_owned()
            } else {
                let bbox =
                    parse_retranslation_bbox(req.bbox.as_ref(), req.x1, req.y1, req.x2, req.y2)?;
                let source_ocr = req.source_ocr.as_deref().unwrap_or("").trim();
                let source_text = req
                    .source_text
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .unwrap_or(source_ocr);
                if source_ocr.len() > 4096 || source_text.len() > 4096 {
                    anyhow::bail!("source text is too long (maximum 4096 UTF-8 bytes)");
                }
                let (width, height) = image::image_dimensions(&source)?;
                if bbox.x1 < 0.0
                    || bbox.y1 < 0.0
                    || bbox.x2 > width as f32
                    || bbox.y2 > height as f32
                {
                    anyhow::bail!("missing-dialogue bbox is outside the managed source page");
                }
                let flags = page
                    .get("issues")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("page has no missing-dialogue review flag"))?;
                let matches = flags
                    .iter()
                    .filter(|issue| {
                        issue.get("origin").and_then(serde_json::Value::as_str)
                            == Some("missing-dialogue-flag")
                            && issue.get("issue_type").and_then(serde_json::Value::as_str)
                                == Some("wrong_or_missing_bubble")
                            && issue
                                .get("bbox")
                                .cloned()
                                .and_then(|value| serde_json::from_value::<Rect>(value).ok())
                                .is_some_and(|flag_bbox| rects_close(flag_bbox, bbox))
                    })
                    .count();
                if matches != 1 {
                    anyhow::bail!(
                        "bbox must match exactly one current missing-dialogue review flag"
                    );
                }
                let existing_empty_id = best_overlapping_empty_bubble(page, bbox);
                let replaced_handoff_id = if existing_empty_id.is_none() {
                    best_overlapping_orphaned_handoff_item(&self.workflow, &source, page, bbox)?
                } else {
                    None
                };
                let bubble_id = if let Some(existing_id) = existing_empty_id {
                    let bubbles = page
                        .get_mut("bubbles")
                        .and_then(serde_json::Value::as_array_mut)
                        .ok_or_else(|| anyhow::anyhow!("page has no editable bubbles"))?;
                    let bubble = bubbles
                        .iter_mut()
                        .find(|bubble| {
                            bubble.get("id").and_then(serde_json::Value::as_str)
                                == Some(existing_id.as_str())
                        })
                        .ok_or_else(|| anyhow::anyhow!("overlapping empty bubble disappeared"))?;
                    bubble["translation"] = serde_json::Value::String(req.translation.clone());
                    bubble["preserve_source"] = serde_json::Value::Bool(false);
                    bubble["keep_source"] = serde_json::Value::Bool(false);
                    bubble["retranslate_requested"] = serde_json::Value::Bool(false);
                    bubble["render_dirty"] = serde_json::Value::Bool(true);
                    if !source_text.trim().is_empty() {
                        bubble["source_text"] = serde_json::Value::String(source_text.to_owned());
                    }
                    if !source_ocr.is_empty() {
                        bubble["source_ocr"] = serde_json::Value::String(source_ocr.to_owned());
                    }
                    existing_id
                } else {
                    let bubble_id = format!("missing-dialogue-{}", uuid::Uuid::new_v4().simple());
                    let bbox_json = serde_json::to_value(bbox)?;
                    let bubbles = page
                        .get_mut("bubbles")
                        .and_then(serde_json::Value::as_array_mut)
                        .ok_or_else(|| anyhow::anyhow!("page has no editable bubbles"))?;
                    bubbles.push(serde_json::json!({
                        "id": bubble_id,
                        "bbox": bbox_json,
                        "bubble_bbox": bbox_json,
                        "text_bbox": bbox_json,
                        "source_text": source_text,
                        "source_ocr": source_ocr,
                        "translation": req.translation,
                        "kind": "manual_dialogue",
                        "manual": true,
                        "replaces_handoff_ids": replaced_handoff_id.iter().collect::<Vec<_>>(),
                        "preserve_source": false,
                        "keep_source": false,
                        "retranslate_requested": false,
                        "render_dirty": true
                    }));
                    bubble_id
                };
                if let Some(issues) = page
                    .get_mut("issues")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    issues.retain(|issue| {
                        issue.get("origin").and_then(serde_json::Value::as_str)
                            != Some("missing-dialogue-flag")
                            || issue.get("issue_type").and_then(serde_json::Value::as_str)
                                != Some("wrong_or_missing_bubble")
                            || issue
                                .get("bbox")
                                .cloned()
                                .and_then(|value| serde_json::from_value::<Rect>(value).ok())
                                .is_none_or(|flag_bbox| !rects_close(flag_bbox, bbox))
                    });
                }
                bubble_id
            };
            let previous_render = page
                .get("rendered_image_path")
                .and_then(serde_json::Value::as_str)
                .map(|raw| {
                    if std::path::Path::new(raw).is_absolute() {
                        std::path::PathBuf::from(raw)
                    } else {
                        job.join(raw)
                    }
                })
                .ok_or_else(|| anyhow::anyhow!("page has no current rendered image"))?;
            let previous_render = self
                .workflow
                .require_owned(&previous_render, "current rendered page")?;
            let raw_cleaned = page
                .get("cleaned_image_path")
                .or_else(|| page.get("cleaned_path"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("page has no current clean image"))?;
            let raw_cleaned = PathBuf::from(raw_cleaned);
            let actual_cleaned = self.workflow.require_owned(
                &if raw_cleaned.is_absolute() {
                    raw_cleaned
                } else {
                    job.join(raw_cleaned)
                },
                "current clean page",
            )?;
            let (_analysis, mask, cleaned, corrected_clean, rendered_path) =
                self.workflow.page_artifacts_for_source(&source)?;
            let removed_ids = page
                .get("removed_bubbles")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|removed| {
                    removed
                        .as_str()
                        .or_else(|| removed.get("id").and_then(serde_json::Value::as_str))
                })
                .collect::<BTreeSet<_>>();
            let target_request_index = page
                .get("bubbles")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter(|bubble| {
                    bubble
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .is_none_or(|id| !removed_ids.contains(id))
                })
                .position(|bubble| {
                    bubble.get("id").and_then(serde_json::Value::as_str) == Some(target_id.as_str())
                });
            let target_render_path = actual_cleaned
                .parent()
                .ok_or_else(|| anyhow::anyhow!("clean page has no parent directory"))?
                .join("rendered.png");
            let manifest_path = if job.join("job.json").is_file() {
                job.join("job.json")
            } else {
                job.join(".fukidashi-job.json")
            };
            let mut artifacts = vec![
                project_path.clone(),
                job.join("translations.json"),
                manifest_path,
                previous_render.clone(),
                target_render_path.clone(),
                rendered_path.clone(),
                mask,
                cleaned.clone(),
                actual_cleaned.clone(),
                corrected_clean.clone(),
            ];
            for path in [
                previous_render.clone(),
                target_render_path.clone(),
                rendered_path,
                cleaned.clone(),
                corrected_clean,
            ] {
                for suffix in [".fukidashi-render.json", ".fukidashi-clean.json"] {
                    artifacts.push(PathBuf::from(format!("{}{}", path.display(), suffix)));
                }
            }
            artifacts.push(PathBuf::from(format!(
                "{}{}",
                actual_cleaned.display(),
                ".fukidashi-clean.json"
            )));
            // The editor may write a clean derivative for brush strokes or
            // source restoration, keyed by the incoming project revision.
            if let Some(parent) = actual_cleaned.parent() {
                artifacts.push(parent.join(format!("editor-clean-{revision}.png")));
                artifacts
                    .push(parent.join(format!("editor-clean-{revision}.png.fukidashi-clean.json")));
            }
            for font in std::iter::once(&crate::fonts::COMIC_NEUE_REGULAR)
                .chain(crate::fonts::bundled_fallbacks())
            {
                let name = font
                    .file_name
                    .chars()
                    .map(|character| {
                        if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
                        {
                            character
                        } else {
                            '_'
                        }
                    })
                    .collect::<String>();
                artifacts.push(
                    job.join("fonts")
                        .join(format!("{}-{name}", &font.sha256[..16])),
                );
            }
            let snapshots = snapshot_managed_files(&self.workflow, &job, artifacts)?;
            let rendered_result = crate::editor::render_editor_page_locked(
                &self.workflow,
                &job,
                &source,
                &state,
                req.page,
            );
            let rendered = match rendered_result {
                Ok(value) => value,
                Err(error) => {
                    restore_managed_files(&snapshots).map_err(|restore_error| {
                        anyhow::anyhow!(
                            "rerender failed ({error:#}) and restoring job artifacts failed ({restore_error:#})"
                        )
                    })?;
                    return Err(error.context("fresh translation was not saved or committed"));
                }
            };
            let typeset = rendered.get("typeset").unwrap_or(&serde_json::Value::Null);
            let target_report = target_request_index.and_then(|index| {
                typeset
                    .get("bubbles")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|bubbles| {
                        bubbles.iter().find(|bubble| {
                            bubble.get("index").and_then(serde_json::Value::as_u64)
                                == Some(index as u64)
                        })
                    })
            });
            let target_failure = match (target_request_index, target_report) {
                (None, _) => Some("renderer omitted the requested bubble".to_owned()),
                (_, None) => {
                    Some("renderer returned no layout result for the requested bubble".to_owned())
                }
                (Some(_), Some(report))
                    if report.get("skipped").and_then(serde_json::Value::as_bool) == Some(true) =>
                {
                    Some(format!(
                        "reason: {}",
                        report
                            .get("skip_reason")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("skipped")
                    ))
                }
                _ => None,
            };
            if let Some(failure) = target_failure {
                restore_managed_files(&snapshots)?;
                anyhow::bail!(
                    "fresh translation was not committed because the requested bubble did not render ({failure}); the previous translation, render, and managed artifacts were restored; shorten it or adjust the bubble in the editor"
                );
            }
            Ok(serde_json::json!({
                "protocol": "retranslation-v1",
                "status": "rendered",
                "job_id": req.job_id,
                "page": req.page,
                "bubble_id": target_id,
                "rendered_image_path": rendered.get("rendered_image_path"),
                "typeset": typeset,
                "next_action": {
                    "tool": "fukidashi_review_and_export",
                    "arguments": {
                        "job_id": req.job_id,
                        "format": req.format.as_deref().unwrap_or("zip")
                    },
                    "instruction": "Keep this call pending for the operator to review the updated job and approve export"
                }
            }))
        })();
        match result {
            Ok(value) => json_result(&value, false),
            Err(error) => json_result(
                &serde_json::json!({
                    "protocol": "retranslation-v1",
                    "status": "rejected",
                    "error": format!("{error:#}"),
                    "next_action": "Use the exact current feedback coordinates and translation; if the render reports text_overflow, shorten the translation or adjust the bubble in the editor, then submit again"
                }),
                true,
            ),
        }
    }

    #[tool(
        name = "fukidashi_serve_editor",
        description = "Serve the local loopback comic editor. Reopen an existing managed job with job_path (directory or job.json; legacy .fukidashi-job.json is also accepted) or job_id; the server selects a verified render and reconstructs every page. Direct editor requests default to reopen_completed=true, minting a new review session after a completed approval; pass reopen_completed=false to retain the already-completed fast path. image_path remains supported for a known rendered artifact; json_data is metadata only."
    )]
    pub async fn serve_editor(&self, Parameters(req): Parameters<EditorRequest>) -> CallToolResult {
        let inferred_job_path = if req.job_path.is_none() && req.job_id.is_none() {
            req.image_path.as_deref().and_then(|raw| {
                let candidate = std::path::PathBuf::from(raw);
                (candidate.is_dir()
                    && (candidate.join("job.json").is_file()
                        || candidate.join(".fukidashi-job.json").is_file()))
                .then_some(raw)
            })
        } else {
            None
        };
        let job_hint = match (
            req.job_path.as_deref().or(inferred_job_path),
            req.job_id.as_deref(),
        ) {
            (Some(_), Some(_)) => {
                return json_result(
                    &serde_json::json!({
                        "error": "provide only one of job_path or job_id",
                        "accepted": {"job_path": "<jobs-root>/<job-id>", "job_id": "<job-id>"}
                    }),
                    true,
                );
            }
            (Some(path), None) => Some(match self.workflow.resolve_managed_job_path(path) {
                Ok(job) => job,
                Err(error) => {
                    return json_result(
                        &serde_json::json!({
                            "error": format!("invalid managed job_path: {error}"),
                            "next_step": "call fukidashi_serve_editor with the exact managed job directory; do not search or guess artifact paths",
                            "example": {"job_path": "<jobs-root>/<job-id>"}
                        }),
                        true,
                    );
                }
            }),
            (None, Some(id)) => Some(match self.workflow.resolve_managed_job_id(id) {
                Ok(job) => job,
                Err(error) => {
                    return json_result(
                        &serde_json::json!({
                            "error": format!("invalid managed job_id: {error}"),
                            "next_step": "use the job_id returned by an earlier pipeline call; do not search or guess paths"
                        }),
                        true,
                    );
                }
            }),
            (None, None) => None,
        };
        let requested_image = if inferred_job_path.is_some() {
            None
        } else {
            req.image_path.as_deref()
        };
        let image_path = match (job_hint.as_deref(), requested_image) {
            (Some(job), Some(raw)) => {
                let candidate = match path(raw) {
                    Ok(value) => value,
                    Err(error) => {
                        return json_result(&serde_json::json!({"error":error.to_string()}), true);
                    }
                };
                match self.workflow.verified_render_for_job(job, Some(&candidate)) {
                    Ok(value) => value,
                    Err(error) => {
                        return json_result(
                            &serde_json::json!({
                                "error": format!("image_path is not a verified render in this managed job: {error}"),
                                "next_step": "omit image_path and pass job_path (or job_id) so the server selects a verified render"
                            }),
                            true,
                        );
                    }
                }
            }
            (Some(job), None) => match self.workflow.verified_render_for_job(job, None) {
                Ok(value) => value,
                Err(error) => {
                    return json_result(
                        &serde_json::json!({
                            "error": format!("managed job has no verified rendered page: {error}"),
                            "next_step": "finish analyze -> clean -> typeset for every expected page"
                        }),
                        true,
                    );
                }
            },
            (None, Some(raw)) => match path(raw) {
                Ok(value) => value,
                Err(error) => {
                    return json_result(&serde_json::json!({"error":error.to_string()}), true);
                }
            },
            (None, None) => {
                return json_result(
                    &serde_json::json!({
                        "error": "fukidashi_serve_editor needs job_path, job_id, or image_path",
                        "next_step": "for an existing job, pass job_path exactly; do not search or guess a rendered filename",
                        "example": {"job_path": "<jobs-root>/<job-id>"}
                    }),
                    true,
                );
            }
        };
        if let Err(error) = self.workflow.validate_render_input(&image_path) {
            return json_result(
                &serde_json::json!({
                    "error": format!("editor requires a server-owned rendered stage: {error}"),
                    "next_step": "call fukidashi_serve_editor with {\"job_path\":\"<jobs-root>/<job-id>\"}; do not pass a source image or guess a render filename"
                }),
                true,
            );
        }
        let state = match self
            .workflow
            .editor_state(&image_path, req.json_data.as_ref())
        {
            Ok(state) => state,
            Err(error) => {
                return json_result(
                    &serde_json::json!({
                        "error": format!("review cannot start until the managed job is complete: {error}"),
                        "next_step": "finish analyze -> clean -> typeset for every expected page, then serve any rendered path"
                    }),
                    true,
                );
            }
        };
        if let Err(error) = self.workflow.validate_review_state(&image_path, &state) {
            return json_result(
                &serde_json::json!({"error": format!("server-generated review state is invalid: {error}")}),
                true,
            );
        }
        let permit = match Arc::clone(&self.slots).acquire_owned().await {
            Ok(permit) => permit,
            Err(e) => {
                return json_result(
                    &serde_json::json!({"error":format!("editor worker capacity closed: {e}")}),
                    true,
                );
            }
        };
        let allowed_sources = state
            .get("pages")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|page| page.get("source_image").and_then(serde_json::Value::as_str))
            .filter_map(|value| std::fs::canonicalize(value).ok())
            .collect::<Vec<_>>();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            crate::editor::serve_editor_with_allowed_sources_and_options(
                &image_path,
                state,
                allowed_sources,
                req.reopen_completed,
            )
        })
        .await;
        match result {
            Ok(Ok(v)) => json_result(&v, false),
            Ok(Err(e)) => json_result(&serde_json::json!({"error":e.to_string()}), true),
            Err(e) => json_result(
                &serde_json::json!({"error":format!("editor worker failed: {e}")}),
                true,
            ),
        }
    }
    #[tool(
        name = "fukidashi_wait_for_review",
        description = "Wait asynchronously for the matching local editor review submission or approval."
    )]
    pub async fn wait_for_review(
        &self,
        Parameters(req): Parameters<WaitReviewRequest>,
    ) -> CallToolResult {
        if req.review_session_id.is_empty() || req.review_session_id.len() > 128 {
            return json_result(
                &serde_json::json!({"error":"review_session_id is invalid"}),
                true,
            );
        }
        match crate::editor::wait_for_review(
            &req.review_session_id,
            req.revision,
            req.timeout_seconds,
        )
        .await
        {
            Ok(value) => json_result(&value, false),
            Err(error) => json_result(&serde_json::json!({"error":error.to_string()}), true),
        }
    }
    #[tool(
        name = "fukidashi_export",
        description = "Export an ordered local project as zip, epub, or standalone html."
    )]
    pub async fn export(&self, Parameters(req): Parameters<ExportRequest>) -> CallToolResult {
        let project_dir = match path(&req.project_dir) {
            Ok(p) => p,
            Err(e) => return json_result(&serde_json::json!({"error":e.to_string()}), true),
        };
        let project_dir = match self.workflow.require_owned(&project_dir, "export project") {
            Ok(path) => path,
            Err(error) => {
                return json_result(
                    &serde_json::json!({
                        "error": format!("export requires a server-owned managed job: {error}"),
                        "next_step": "complete clean -> typeset -> serve_editor -> review approval before export"
                    }),
                    true,
                );
            }
        };
        if !project_dir.join("review.json").is_file() {
            return json_result(
                &serde_json::json!({
                    "error": "export is blocked: this managed job has not been served through the review editor",
                    "next_step": "call fukidashi_serve_editor, then fukidashi_wait_for_review"
                }),
                true,
            );
        }
        if let Err(error) = self.workflow.validate_export_job(&project_dir) {
            return json_result(
                &serde_json::json!({
                    "error": format!("export is blocked by managed job state: {error}"),
                    "next_step": "render every analyzed page before review and export"
                }),
                true,
            );
        }
        if !matches!(req.format.as_str(), "zip" | "epub" | "html_monolith") {
            return json_result(
                &serde_json::json!({"error":"format must be zip, epub, or html_monolith"}),
                true,
            );
        }
        if let Err(error) = crate::editor::export_gate(&project_dir) {
            return json_result(&serde_json::json!({"error":error.to_string()}), true);
        }
        let format = req.format;
        let exports_dir = self.config.exports_dir();
        let export_workflow = self.workflow.clone();
        let permit = match Arc::clone(&self.slots).acquire_owned().await {
            Ok(permit) => permit,
            Err(e) => {
                return json_result(
                    &serde_json::json!({"error":format!("export worker capacity closed: {e}")}),
                    true,
                );
            }
        };
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // The same job-wide render lease covers authoritative review
            // binding, all-page sidecar/hash validation, and archive reads.
            // This closes the validation-to-package window for both direct
            // fukidashi_export and automatic review_and_export.
            let _render_lock = export_workflow.acquire_render_lock(&project_dir)?;
            if !project_dir.join("review.json").is_file() {
                anyhow::bail!(
                    "export is blocked: this managed job has not been served through the review editor"
                );
            }
            export_workflow.validate_export_job(&project_dir)?;
            crate::editor::export_gate(&project_dir)?;
            let bound_approval =
                FukidashiServer::verify_approval_binding(&project_dir, &serde_json::Value::Null)?;
            FukidashiServer::validate_managed_export_artifacts(&project_dir, bound_approval)?;
            crate::export::export_project_to(&project_dir, &format, Some(&exports_dir))
        })
        .await;
        match result {
            Ok(Ok(v)) => json_result(&v, false),
            Ok(Err(e)) => json_result(&serde_json::json!({"error":e.to_string()}), true),
            Err(e) => json_result(
                &serde_json::json!({"error":format!("export worker failed: {e}")}),
                true,
            ),
        }
    }
}

#[tool_handler]
impl ServerHandler for FukidashiServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "fukidashi-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Local managed comic-translation workflow. Prefer the strict-v1 two-call loop: call \
                fukidashi_translation_start with one source image, job_path, or job_id, then call \
                fukidashi_translation_preflight once for a multi-page, resumable 1/N OCR routing pass when speed \
                and visible page progress matter; it persists reusable analysis checkpoints and classifies \
                bubble/prose/mixed/skip pages while translation and rendering remain serial. Cover-like page 1 \
                is preserved by default; pass translate_cover=true to opt in. source_language must be auto, ja, \
                zh, ko, en, or latin; legacy pipe values such as en|latin are normalized to automatic routing. \
                Preflight reports reused_cached_pages, newly_processed_pages, total_pages, within_job_cached_pages, \
                cross_job_cached_pages (always zero; no cross-job cache is used), and pass-through/skip counts, \
                with the message Reused cached pages: X/N; newly processed: Y/N. \
                Dense prose source blocks are preserved and the server fails closed when a required prose group \
                is missing or implausibly short. Then call \
                fukidashi_get_lore (or use the page_ready lore template) before the first submit and \
                fukidashi_put_lore for known names, pronouns, and glossary terms; flag unknown speakers \
                with needs_review=true. A compact lore example is {schema:1,characters:[\"Fuyu\"],pronouns:[],glossary:[{source:\"proprietress\",target:\"bà chủ\"}]}; character name strings are returned canonically as {id,names,notes}. Lore is client-authored and this server never invokes an LLM. \
                fukidashi_translation_submit with only the returned work_token and one structured decision for \
                each required_translation_ids entry; preserved_items are explicit source decisions and are \
                not omitted from review. The server owns page selection, analysis, clean, typeset, \
                stage reuse, model release, and advancement. sfx_mode=preserve is the default: structurally \
                unmatched text-* items are reported as preserved audit data and are excluded from required \
                translations, cleaning, and typesetting; preserve-only pages get a verified pass-through stage; \
                use sfx_mode=replace only when explicitly requested. \
                Use keep_source=true and/or needs_review=true for uncertain OCR instead of aborting. Never \
                shell-read managed manifests/checkpoints, import or search for Fukidashi packages, invent artifact \
                paths, or pass artifact paths to the strict submit call. After review_ready, call \
                fukidashi_review_and_export with the exact returned job_id; it opens the local editor and keeps \
                the single MCP call pending until review, returning feedback or exporting zip after approval. \
                fukidashi_serve_editor and fukidashi_wait_for_review remain compatibility tools. Primitive \
                fukidashi_analyze_page, fukidashi_clean_page, and fukidashi_typeset tools remain available for \
                compatibility; pass their exact server-returned paths and stable IDs. For typesetting, use \
                Comic Neue or another legitimate comic face as the primary; never pass generic Windows UI \
                faces such as Arial, Calibri, Segoe UI, Tahoma, Verdana, Times, or DejaVu Sans as a primary. \
                The server substitutes bundled Comic Neue and reports the requested and resolved faces; Patrick \
                Hand covers Vietnamese, Noto Sans Symbols 2 covers symbols, and configured/platform \
                fonts provide CJK glyph coverage when installed. The native editor also registers \
                available system CJK fonts so Chinese, Korean, and Japanese stay readable in its \
                translation field. Use \
                fukidashi_release_models between bounded legacy batches on memory-constrained machines. For \
                acquisition, fukidashi_search_manga searches native MangaDex only and returns an exact manga_id \
                plus a suggested latest=true pull. For a vague latest request, call fukidashi_pull_chapter with \
                source=mangadex, that manga_id, and latest=true; it selects the highest chapter value including \
                external releases and never silently falls back to an older hosted chapter. Source language is \
                metadata that Fukidashi auto-translates. An external or unavailable result is a successful \
                non-import report for that exact release. Direct mode is first-class: use one explicit http(s) URL \
                and optional job_name with a provisioned gallery-dl helper. If extraction fails, report that \
                the explicit URL was not imported and supply another explicit supported http(s) URL; do not \
                switch providers automatically. Follow the returned fukidashi_translation_start next step \
                after an import.",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::Rect,
        vision::ocr::{
            OcrRegion, PageAnalysis, TranslationHandoff, TranslationItem, VisionCorrection,
        },
    };

    #[test]
    fn preflight_manifest_classifies_prose_and_skip_pages() {
        let prose = serde_json::json!({
            "bubbles": [{"recognizer":"prose-group"}],
            "unmatched_text": [{"id":"text-1"}],
            "text_lines": [{"text":"あとがき 本文"}],
            "translation_handoff": {"items": [
                {"source_text":"あとがき 本文", "preserve_by_default":false},
                {"source_text":"author@example.com", "preserve_by_default":true}
            ]}
        });
        assert_eq!(preflight_page_kind(&prose), "mixed");
        let skip = serde_json::json!({
            "bubbles": [],
            "unmatched_text": [{"id":"text-1"}],
            "text_lines": [{"text":"ロゴ"}],
            "translation_handoff": {"items": [
                {"source_text":"ロゴ", "preserve_by_default":true}
            ]}
        });
        assert_eq!(preflight_page_kind(&skip), "skip");
    }

    #[test]
    fn preflight_cache_telemetry_distinguishes_reuse_and_new_work() {
        let page = |cached: bool, classification: &str| {
            serde_json::json!({
                "cached_analysis": cached,
                "cache_hit": false,
                "classification": classification,
                "pass_through": classification == "skip",
            })
        };
        let cold_one = vec![page(false, "prose")];
        let cold_one_telemetry = preflight_cache_telemetry(&cold_one, 1);
        assert_eq!(cold_one_telemetry["reused_cached_pages"], 0);
        assert_eq!(cold_one_telemetry["newly_processed_pages"], 1);
        assert_eq!(
            cold_one_telemetry["message"],
            "Reused cached pages: 0/1; newly processed: 1/1"
        );
        let cached_one = vec![page(true, "prose")];
        let cached_one_telemetry = preflight_cache_telemetry(&cached_one, 1);
        assert_eq!(cached_one_telemetry["reused_cached_pages"], 1);
        assert_eq!(cached_one_telemetry["newly_processed_pages"], 0);
        assert_eq!(
            cached_one_telemetry["message"],
            "Reused cached pages: 1/1; newly processed: 0/1"
        );

        let reused = vec![page(true, "bubble"); 5];
        let telemetry = preflight_cache_telemetry(&reused, 5);
        assert_eq!(telemetry["reused_cached_pages"], 5);
        assert_eq!(telemetry["newly_processed_pages"], 0);
        assert_eq!(telemetry["within_job_cached_pages"], 5);
        assert_eq!(telemetry["cross_job_cached_pages"], 0);
        assert_eq!(telemetry["pass_through_pages"], 0);
        assert_eq!(
            telemetry["message"],
            "Reused cached pages: 5/5; newly processed: 0/5"
        );

        let fresh = vec![page(false, "bubble"); 5];
        let telemetry = preflight_cache_telemetry(&fresh, 5);
        assert_eq!(telemetry["reused_cached_pages"], 0);
        assert_eq!(telemetry["newly_processed_pages"], 5);
        assert_eq!(telemetry["within_job_cached_pages"], 0);
        assert_eq!(
            telemetry["message"],
            "Reused cached pages: 0/5; newly processed: 5/5"
        );

        let partial = vec![
            page(true, "bubble"),
            page(true, "skip"),
            page(false, "prose"),
            page(false, "mixed"),
            page(false, "bubble"),
        ];
        let telemetry = preflight_cache_telemetry(&partial, 5);
        assert_eq!(telemetry["reused_cached_pages"], 2);
        assert_eq!(telemetry["newly_processed_pages"], 3);
        assert_eq!(telemetry["pass_through_pages"], 1);
        assert_eq!(telemetry["skip_pages"], 1);
        assert_eq!(telemetry["cross_job_cache_available"], false);
    }

    #[test]
    fn missing_body_line_is_promoted_inside_dense_prose_span() {
        let items = vec![
            serde_json::json!({
                "id": "heading",
                "kind": "dialogue",
                "keep_source": false,
                "bbox": {"x1": 100.0, "y1": 100.0, "x2": 900.0, "y2": 180.0}
            }),
            serde_json::json!({
                "id": "body",
                "kind": "dialogue",
                "keep_source": false,
                "bbox": {"x1": 100.0, "y1": 220.0, "x2": 900.0, "y2": 500.0}
            }),
            serde_json::json!({
                "id": "footer",
                "kind": "unmatched_text",
                "keep_source": true,
                "bbox": {"x1": 100.0, "y1": 600.0, "x2": 900.0, "y2": 800.0}
            }),
        ];
        let body_line = serde_json::json!({
            "text": "ありすぎゃんは",
            "confidence": 0.44,
            "bbox": {"x1": 120.0, "y1": 515.0, "x2": 500.0, "y2": 545.0}
        });
        assert!(is_main_japanese_prose_line(&body_line, &items));
        let footer_line = serde_json::json!({
            "text": "発行日 2002/5/1",
            "confidence": 0.90,
            "bbox": {"x1": 120.0, "y1": 610.0, "x2": 500.0, "y2": 640.0}
        });
        assert!(!is_main_japanese_prose_line(&footer_line, &items));
    }

    #[test]
    fn existing_unmatched_body_item_is_promoted_by_handoff_normalization() {
        let mut analysis = serde_json::json!({
            "text_lines": [{
                "id": "line-body",
                "text": "ありすぎゃんは",
                "confidence": 0.44,
                "bbox": {"x1": 120.0, "y1": 515.0, "x2": 500.0, "y2": 545.0}
            }],
            "translation_handoff": {"items": [
                {"id":"heading", "kind":"dialogue", "keep_source":false,
                 "bbox":{"x1":100.0,"y1":100.0,"x2":900.0,"y2":180.0}},
                {"id":"body", "kind":"dialogue", "keep_source":false,
                 "bbox":{"x1":100.0,"y1":220.0,"x2":900.0,"y2":500.0}},
                {"id":"text-body", "kind":"unmatched_text", "keep_source":true,
                 "source_text":"ありすぎゃんは", "bbox":{"x1":120.0,"y1":515.0,"x2":500.0,"y2":545.0}}
            ]}
        });
        let audit = augment_missing_detected_text_items(&mut analysis).unwrap();
        assert!(audit.is_empty());
        let item = &analysis["translation_handoff"]["items"][2];
        assert_eq!(item["kind"], "prose-line");
        assert_eq!(item["keep_source"], false);
        assert_eq!(item["status"], "pending");
    }

    #[test]
    fn mixed_source_language_alias_is_safe_and_schema_is_enum() {
        assert_eq!(
            normalize_requested_source_language(Some("en|latin".into())).unwrap(),
            None
        );
        assert_eq!(
            normalize_requested_source_language(Some("auto".into())).unwrap(),
            None
        );
        assert_eq!(
            normalize_requested_source_language(Some("JA".into())).unwrap(),
            Some("ja".into())
        );
        assert!(normalize_requested_source_language(Some("ja|zh".into())).is_ok());
    }

    #[test]
    fn cover_classifier_requires_explicit_opt_in_signal() {
        let cover = serde_json::json!({
            "bubbles": [
                {"detector_label": 2, "bbox": {"x1": 0.0, "y1": 0.0, "x2": 100.0, "y2": 100.0}},
                {"detector_label": 2, "bbox": {"x1": 0.0, "y1": 0.0, "x2": 100.0, "y2": 100.0}}
            ],
            "translation_handoff": {"items": [
                {"source_text": "ようこそ welcome to cafe"},
                {"source_text": "DOJIN R18"}
            ]}
        });
        assert!(cover_like_analysis(1, &cover));
        assert!(!cover_like_analysis(2, &cover));
        let dialogue = serde_json::json!({
            "bubbles": [{"detector_label": 0}],
            "translation_handoff": {"items": [{"source_text": "ようこそ"}]}
        });
        assert!(!cover_like_analysis(1, &dialogue));
    }

    #[test]
    fn completed_page_keeps_success_when_next_page_preparation_fails() {
        let temp = tempfile::tempdir().unwrap();
        let server = FukidashiServer::new(test_config(temp.path())).unwrap();
        let claim = TranslationClaim {
            job_dir: temp.path().join("job"),
            source_image: temp.path().join("page.png"),
            page_number: 1,
            total_pages: 2,
            analysis_path: temp.path().join("analysis.json"),
            analysis_sha256: "hash".into(),
            item_ids: BTreeSet::new(),
            source_language: None,
            target_language: Some("vi".into()),
            replace_sfx: false,
            in_progress: false,
            consumed: true,
        };
        let result = server.completed_translation_response(
            &claim,
            Err(FukidashiError::InvalidInput(
                "next page needs corrected input".into(),
            )),
            false,
        );
        let value = extract_tool_json(result.clone(), "completed page").unwrap();
        assert_eq!(result.is_error, Some(false));
        assert_eq!(value["status"], "page_complete");
        assert_eq!(value["completed_page"], 1);
    }

    #[tokio::test]
    async fn fresh_multi_page_start_requires_resumable_preflight() {
        let temp = tempfile::tempdir().unwrap();
        let source_dir = temp.path().join("comic");
        std::fs::create_dir_all(&source_dir).unwrap();
        for page in [1, 2] {
            image::RgbImage::from_pixel(24, 24, image::Rgb([255, 255, 255]))
                .save(source_dir.join(format!("{page}.png")))
                .unwrap();
        }
        let server = FukidashiServer::new(test_config(temp.path())).unwrap();
        let value = extract_tool_json(
            server
                .translation_start(Parameters(TranslationStartRequest {
                    image_path: Some(source_dir.join("1.png").display().to_string()),
                    job_path: None,
                    job_id: None,
                    ocr_mode: None,
                    source_language: Some("ja".into()),
                    target_language: Some("vi".into()),
                    scope: None,
                    sfx_mode: None,
                    translate_cover: false,
                }))
                .await,
            "preflight gate",
        )
        .unwrap();
        assert_eq!(value["status"], "preflight_required");
        assert_eq!(value["total_pages"], 2);
        assert_eq!(
            value["next_action"]["tool"],
            "fukidashi_translation_preflight"
        );
        assert_eq!(value["progress"]["current_page"], 0);
        assert_eq!(value["progress"]["total_pages"], 2);
    }

    #[test]
    fn server_info_exposes_portable_workflow_instructions() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            models_dir: temp.path().join("models"),
            storage_root: temp.path().join("storage"),
            storage_source: "test".into(),
            models_source: "test".into(),
            ort_source: "test".into(),
            config_file: temp.path().join("config.json"),
            configured_jobs_dir: None,
            configured_cache_dir: None,
            configured_temp_dir: None,
            configured_runtime_dir: None,
            configured_exports_dir: None,
            configured_font_dirs: Vec::new(),
            configured_provider: None,
            ort_dylib: None,
        };
        let server = FukidashiServer::new(config).unwrap();
        let info = server.get_info();
        let instructions = info.instructions.unwrap();
        let tool_names = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<BTreeSet<_>>();

        assert_eq!(info.server_info.name, "fukidashi-mcp");
        assert!(tool_names.contains("fukidashi_search_manga"));
        assert!(tool_names.contains("fukidashi_pull_chapter"));
        assert!(instructions.contains("fukidashi_translation_start"));
        assert!(instructions.contains("fukidashi_translation_submit"));
        assert!(instructions.contains("fukidashi_review_and_export"));
        assert!(instructions.contains("sfx_mode=preserve"));
        assert!(instructions.contains("fukidashi_analyze_page"));
        assert!(instructions.contains("fukidashi_wait_for_review"));
        assert!(instructions.contains("fukidashi_search_manga"));
        assert!(instructions.contains("fukidashi_pull_chapter"));
        assert!(instructions.contains("latest=true"));
        assert!(instructions.contains("external releases"));
        assert!(instructions.contains("Source language"));
        assert!(instructions.contains("after approval"));
        assert!(instructions.contains("characters:[\"Fuyu\"]"));
        assert!(
            instructions
                .contains("character name strings are returned canonically as {id,names,notes}")
        );
    }

    #[test]
    fn opencode_stringified_scope_is_accepted_without_changing_schema() {
        let encoded_scope = serde_json::json!({
            "include_paths": [
                r"C:\comic\1.webp",
                r"C:\comic\2.webp"
            ]
        })
        .to_string();
        let request: TranslationStartRequest = serde_json::from_value(serde_json::json!({
            "image_path": r"C:\comic\1.webp",
            "target_language": "vi",
            "scope": encoded_scope,
        }))
        .unwrap();
        assert_eq!(
            request.scope.unwrap().include_paths,
            Some(vec![
                r"C:\comic\1.webp".to_owned(),
                r"C:\comic\2.webp".to_owned()
            ])
        );

        let object_request: TranslationStartRequest = serde_json::from_value(serde_json::json!({
            "scope": {"start_page": 1, "end_page": 2},
        }))
        .unwrap();
        let scope = object_request.scope.unwrap();
        assert_eq!(scope.start_page, Some(1));
        assert_eq!(scope.end_page, Some(2));
    }

    #[tokio::test]
    async fn opencode_invalid_submit_returns_actionable_json_error() {
        let temp = tempfile::tempdir().unwrap();
        let server = FukidashiServer::new(test_config(temp.path())).unwrap();
        let result = server
            .translation_submit(Parameters(TranslationSubmitRequest {
                work_token: "strict-v1-not-a-live-token".into(),
                translations: Vec::new(),
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        let text = result
            .content
            .into_iter()
            .find_map(|block| match block {
                ContentBlock::Text(value) => Some(value.text),
                _ => None,
            })
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["protocol"], "strict-v1");
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .contains("unknown or expired")
        );
        assert_eq!(
            value["next_step"],
            "call fukidashi_translation_start to obtain a fresh work_token"
        );
    }

    #[test]
    fn approval_binding_accepts_legacy_and_binds_frozen_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().to_path_buf();
        let job_id = project_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let rendered0 = project_dir.join("rendered-0.png");
        let rendered1 = project_dir.join("rendered-1.png");
        std::fs::write(&rendered0, b"approved-bytes-0").unwrap();
        std::fs::write(&rendered1, b"approved-bytes-1").unwrap();
        let state = serde_json::json!({
            "schema_version": 1,
            "font_path": serde_json::Value::Null,
            "pages": [
                {"id": "p0", "image_path": "a.png", "rendered_image_path": "rendered-0.png", "bubbles": []},
                {"id": "p1", "image_path": "b.png", "rendered_image_path": "rendered-1.png", "bubbles": []}
            ]
        });
        std::fs::write(
            project_dir.join("project.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        // Legacy approval without a frozen snapshot keeps loose semantics.
        let legacy = serde_json::json!({
            "revision": 4,
            "approved_pages": [0, 1],
            "audit": [{"event": "approve_export", "revision": 4}]
        });
        assert!(FukidashiServer::verify_approval_binding(&project_dir, &legacy).is_ok());
        // A bound approval must match revision, page coverage, checkpoint
        // identity, and current artifact hashes/signatures.
        let snapshot = crate::approval::ApprovalSnapshot::freeze(
            job_id.clone(),
            4,
            &crate::editor::approval_state_signature(&state, Some(&project_dir)),
            vec![0],
        );
        let mut checkpoint = crate::approval::ApprovalCheckpoint::fresh(&snapshot);
        let expected = crate::approval::ExpectedPageProvenance {
            page_index: 0,
            page_id: "p0".into(),
            semantic_render_signature: crate::editor::post_render_page_signature(
                &state,
                &state["pages"][0],
            ),
            source_sha256: String::new(),
            clean_sha256: String::new(),
            output_path: rendered0.clone(),
        };
        let output_sha256 = crate::approval::sha256_file(&rendered0).unwrap();
        checkpoint.mark_complete(crate::approval::ApprovalPageCheckpoint::new(
            0,
            "p0".into(),
            expected.semantic_render_signature.clone(),
            String::new(),
            String::new(),
            rendered0.display().to_string(),
            output_sha256,
        ));
        crate::approval::save_approval_checkpoint(&project_dir, 4, &checkpoint).unwrap();
        let binding = crate::approval::approval_audit_value(&snapshot, &[0], &[vec![0]], 2, 1);
        let bound = serde_json::json!({
            "revision": 4,
            "approved_pages": [0, 1],
            "audit": [{
                "event": "approve_export",
                "revision": 4,
                "approval": binding.clone(),
            }]
        });
        assert!(FukidashiServer::verify_approval_binding(&project_dir, &bound).is_ok());
        // The live wait response is deliberately compact; export must read
        // the full audit from the authoritative review.json instead of
        // silently downgrading to legacy semantics.
        std::fs::write(
            project_dir.join("review.json"),
            serde_json::to_vec(&bound).unwrap(),
        )
        .unwrap();
        let compact_wait_result = serde_json::json!({
            "review_session_id": "session",
            "revision": 4,
            "action": "approve_export",
            "approved_pages": [0, 1]
        });
        assert!(
            FukidashiServer::verify_approval_binding(&project_dir, &compact_wait_result).is_ok()
        );
        // Approving snapshot A then modifying semantics to B without changing
        // the page count must NOT export under A's approval.
        let mut drifted = state.clone();
        drifted["pages"][1]["bubbles"] = serde_json::json!([{
            "id": "b1",
            "bbox": {"x1": 1, "y1": 1, "x2": 8, "y2": 8},
            "translation": "changed after approval"
        }]);
        std::fs::write(
            project_dir.join("project.json"),
            serde_json::to_vec(&drifted).unwrap(),
        )
        .unwrap();
        assert!(FukidashiServer::verify_approval_binding(&project_dir, &bound).is_err());
        // Restore the approved state: the binding verifies again.
        std::fs::write(
            project_dir.join("project.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        assert!(FukidashiServer::verify_approval_binding(&project_dir, &bound).is_ok());
        // Corrupting the approved artifact also blocks the export.
        std::fs::write(&rendered0, b"tampered-bytes").unwrap();
        assert!(FukidashiServer::verify_approval_binding(&project_dir, &bound).is_err());
        let wrong_revision = serde_json::json!({
            "revision": 5,
            "approved_pages": [0, 1],
            "audit": [{
                "event": "approve_export",
                "revision": 5,
                "approval": binding.clone(),
            }]
        });
        assert!(FukidashiServer::verify_approval_binding(&project_dir, &wrong_revision).is_err());
        let wrong_coverage = serde_json::json!({
            "revision": 4,
            "approved_pages": [0],
            "audit": [{
                "event": "approve_export",
                "revision": 4,
                "approval": binding.clone(),
            }]
        });
        assert!(FukidashiServer::verify_approval_binding(&project_dir, &wrong_coverage).is_err());
    }

    #[test]
    fn effective_global_font_records_managed_path_not_raw_request() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = dir.path().join("jobs");
        let workflow = crate::workflow::Workflow::new(jobs).unwrap();
        let job = workflow.allocate_job().unwrap();
        // No request means no historical global.
        assert_eq!(effective_global_font_path(&workflow, &job, None), None);
        // A generic desktop primary substitutes to a managed bundled font.
        let effective = effective_global_font_path(&workflow, &job, Some("Arial")).unwrap();
        assert_ne!(effective, "Arial");
        assert!(std::path::Path::new(&effective).is_file());
        // Operator provenance is recorded separately by the caller.
        let requested = Some("Arial".to_owned());
        assert_eq!(requested.as_deref(), Some("Arial"));
    }

    #[test]
    fn direct_editor_requests_reopen_completed_reviews_by_default() {
        let request: EditorRequest = serde_json::from_value(serde_json::json!({
            "job_id": "completed-job"
        }))
        .unwrap();
        assert!(request.reopen_completed);

        let request: EditorRequest = serde_json::from_value(serde_json::json!({
            "job_id": "completed-job",
            "reopen_completed": false
        }))
        .unwrap();
        assert!(!request.reopen_completed);
    }

    fn test_config(root: &std::path::Path) -> Config {
        Config {
            models_dir: root.join("storage/models"),
            storage_root: root.join("storage"),
            storage_source: "test".into(),
            models_source: "test".into(),
            ort_source: "test".into(),
            config_file: root.join("config.json"),
            configured_jobs_dir: None,
            configured_cache_dir: None,
            configured_temp_dir: None,
            configured_runtime_dir: None,
            configured_exports_dir: None,
            configured_font_dirs: Vec::new(),
            configured_provider: None,
            ort_dylib: None,
        }
    }

    fn strict_fixture_analysis() -> serde_json::Value {
        serde_json::json!({
            "source_language": "ja",
            "target_language": "vi",
            "bubbles": [{
                "id": "bubble-1",
                "detector_label": 0,
                "bbox": {"x1": 0.5, "y1": 0.5, "x2": 20.0, "y2": 20.0}
            }],
            "translation_handoff": {
                "target_language": "vi",
                "status": "pending",
                "items": [{
                    "id": "bubble-1",
                    "source_text": "原文",
                    "ocr_text": "原文",
                    "source_language": "ja",
                    "confidence": 0.41,
                    "correction_applied": false,
                    "bbox": {"x1": 1.0, "y1": 1.0, "x2": 15.0, "y2": 15.0},
                    "status": "pending"
                }, {
                    "id": "text-sfx",
                    "source_text": "クス",
                    "ocr_text": "クス",
                    "source_language": "ja",
                    "confidence": 0.92,
                    "correction_applied": false,
                    "kind": "unmatched_text",
                    "preserve_by_default": true,
                    "bbox": {"x1": 2.0, "y1": 2.0, "x2": 8.0, "y2": 8.0},
                    "status": "pending"
                }]
            }
        })
    }

    #[test]
    fn incomplete_submission_reports_missing_ids_and_indices() {
        let analysis = strict_fixture_analysis();
        let claim = TranslationClaim {
            job_dir: PathBuf::from("C:/jobs/job"),
            source_image: PathBuf::from("C:/source/page.png"),
            page_number: 1,
            total_pages: 1,
            analysis_path: PathBuf::from("C:/jobs/job/pages/0001/analysis.json"),
            analysis_sha256: "hash".into(),
            item_ids: ["bubble-1".into()].into_iter().collect(),
            source_language: Some("ja".into()),
            target_language: Some("vi".into()),
            replace_sfx: false,
            in_progress: false,
            consumed: false,
        };
        let error =
            FukidashiServer::validate_strict_submissions(&analysis, &claim, &[]).unwrap_err();
        let FukidashiError::Diagnostic { details, .. } = error else {
            panic!("expected missing-item diagnostic");
        };
        assert_eq!(details["stage"], "submit_validation");
        assert_eq!(details["code"], "missing_translation_items");
        assert_eq!(details["missing_items"][0]["id"], "bubble-1");
        assert_eq!(details["missing_items"][0]["index"], 0);
        assert!(
            details["next_step"]
                .as_str()
                .unwrap()
                .contains("keep_source=true")
        );
    }

    #[test]
    fn strict_bbox_ignores_synthetic_promoted_ocr_bubbles() {
        let analysis = serde_json::json!({
            "bubbles": [{
                "id": "missed-line",
                "detector_label": 2,
                "bbox": {"x1": 1.0, "y1": 2.0, "x2": 30.0, "y2": 40.0}
            }, {
                "id": "dialogue",
                "detector_label": 0,
                "bbox": {"x1": 3.0, "y1": 4.0, "x2": 50.0, "y2": 60.0}
            }]
        });
        assert_eq!(
            analysis_bubble_bbox(&analysis, "missed-line").unwrap(),
            None
        );
        assert_eq!(
            analysis_bubble_bbox(&analysis, "dialogue").unwrap(),
            Some(Rect {
                x1: 3.0,
                y1: 4.0,
                x2: 50.0,
                y2: 60.0,
            })
        );
    }

    #[test]
    fn strict_sfx_policy_preserves_legacy_text_ids_by_default() {
        let items = saved_translation_items(&strict_fixture_analysis()).unwrap();
        let preserved = translatable_items(&items, false, Some("vi"));
        let replaced = translatable_items(&items, true, Some("vi"));
        assert_eq!(
            preserved
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["bubble-1"]
        );
        assert_eq!(replaced.len(), 2);
        assert_eq!(items[1].kind, "unmatched_text");
        assert!(items[1].preserve_by_default);
        assert_eq!(preserved_item_json(&items[1])["translation_allowed"], false);
    }

    #[test]
    fn vietnamese_target_promotes_english_prose_outside_bubbles() {
        let analysis = serde_json::json!({
            "source_language": "en",
            "target_language": "vi",
            "bubbles": [],
            "text_lines": [],
            "unmatched_text": [{
                "id": "text-prose",
                "bbox": {"x1": 1.0, "y1": 1.0, "x2": 80.0, "y2": 40.0}
            }],
            "translation_handoff": {"items": [{
                "id": "text-prose",
                "kind": "unmatched_text",
                "preserve_by_default": true,
                "source_text": "The old friend came back and everyone heard the story.",
                "source_language": "en",
                "confidence": 0.9,
                "bbox": {"x1": 1.0, "y1": 1.0, "x2": 80.0, "y2": 40.0}
            }]}
        });
        let items = saved_translation_items(&analysis).unwrap();
        let required = translatable_items(&items, false, Some("vi"));
        assert_eq!(
            required
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["text-prose"]
        );
        assert!(translatable_items(&items, false, Some("en")).is_empty());
    }

    #[test]
    fn detected_regions_must_have_handoff_items() {
        let mut analysis = strict_fixture_analysis();
        analysis["bubbles"] = serde_json::json!([
            {"id":"bubble-1","detector_label":0,"bbox":{"x1":0.5,"y1":0.5,"x2":20.0,"y2":20.0}},
            {"id":"bubble-missing","detector_label":0,"bbox":{"x1":21.0,"y1":1.0,"x2":40.0,"y2":20.0}}
        ]);
        let error = saved_translation_items(&analysis).unwrap_err().to_string();
        assert!(error.contains("bubble-missing"));
    }

    #[test]
    fn clean_mask_rejects_text_inside_a_broad_untranslated_region() {
        let mut analysis = overlapping_double_text_checkpoint();
        analysis["text_lines"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": "line-unrepresented",
                "text": "ANOTHER ENGLISH SENTENCE HERE",
                "source_language": "en",
                "confidence": 0.99,
                "bbox": {"x1": 30.0, "y1": 58.0, "x2": 70.0, "y2": 78.0}
            }));
        let (_dir, path) = write_checkpoint(&analysis);
        let error = checkpoint_text_regions(&path, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not represented") || error.contains("no translated item"));
    }

    #[test]
    fn clean_mask_reports_page5_style_overlap_diagnostic() {
        let analysis = serde_json::json!({
            "target_language": "vi",
            "text_lines": [{
                "id": "line-d42e9cb6664f1873",
                "text": "遠ざからないで！！",
                "source_language": "en",
                "confidence": 0.28843334317207336,
                "bbox": {
                    "x1": 603.8469848632812,
                    "y1": 1162.83837890625,
                    "x2": 716.2561645507812,
                    "y2": 1380.9078369140625
                }
            }],
            "translation_handoff": {"items": [{
                "id": "bubble-fc12e7a6c0bb34c0",
                "kind": "dialogue",
                "source_text": "ありすにしてもらえたら最高だなぁって．．．ね？",
                "source_language": "ja",
                "bbox": {
                    "x1": 688.6068115234375,
                    "y1": 1134.5416259765625,
                    "x2": 891.0330200195312,
                    "y2": 1391.91943359375
                }
            }]}
        });
        let (_dir, path) = write_checkpoint(&analysis);
        let error = checkpoint_text_regions(&path, false).unwrap_err();
        let FukidashiError::Diagnostic { details, .. } = &error else {
            panic!("expected structured strict-clean diagnostic, got {error:?}");
        };
        assert_eq!(details["stage"], "strict_clean");
        assert_eq!(details["code"], "unrepresented_detected_text");
        assert_eq!(details["detected_line_index"], 0);
        assert_eq!(details["detected_line_id"], "line-d42e9cb6664f1873");
        assert_eq!(
            details["overlapping_item_ids"][0],
            "bubble-fc12e7a6c0bb34c0"
        );
        assert_eq!(details["confidence"], 0.28843334317207336);
        assert!(
            details["next_step"]
                .as_str()
                .unwrap()
                .contains("keep_source=true")
        );

        let payload = error_json(&error);
        assert_eq!(payload["stage"], "strict_clean");
        assert_eq!(payload["code"], "unrepresented_detected_text");
        assert_eq!(payload["diagnostic"]["bbox"]["x1"], 603.8469848632812);

        let mut normalized = analysis.clone();
        let preserved = augment_missing_detected_text_items(&mut normalized).unwrap();
        assert_eq!(preserved.len(), 1);
        assert_eq!(preserved[0]["id"], "line-d42e9cb6664f1873");
        assert_eq!(
            normalized["translation_handoff"]["items"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["keep_source"],
            true
        );
        assert_eq!(
            normalized["translation_handoff"]["items"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["id"],
            "line-d42e9cb6664f1873"
        );
        let (_dir, normalized_path) = write_checkpoint(&normalized);
        assert!(checkpoint_text_regions(&normalized_path, false).is_ok());
    }

    #[test]
    fn legacy_unrepresented_confident_line_is_auto_preserved_idempotently() {
        let mut analysis = serde_json::json!({
            "target_language": "vi",
            "text_lines": [{
                "id": "line-dialogue",
                "text": "？",
                "source_language": "ja",
                "confidence": 0.74,
                "bbox": {"x1": 10.0, "y1": 10.0, "x2": 30.0, "y2": 30.0}
            }],
            "translation_handoff": {"items": [{
                "id": "bubble-1",
                "kind": "dialogue",
                "source_text": "こんにちは",
                "source_language": "ja",
                "confidence": 0.99,
                "bbox": {"x1": 1.0, "y1": 1.0, "x2": 40.0, "y2": 40.0},
                "status": "pending"
            }]}
        });
        let first = augment_missing_detected_text_items(&mut analysis).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["detected_text_index"], 0);
        assert_eq!(
            analysis["translation_handoff"]["items"][1]["kind"],
            "unmatched_text"
        );
        assert_eq!(
            analysis["translation_handoff"]["items"][1]["keep_source"],
            true
        );
        assert_eq!(
            analysis["translation_handoff"]["items"][1]["status"],
            "preserved"
        );
        assert_eq!(
            analysis["strict_v1"]["auto_preserved_items"][0]["id"],
            "line-dialogue"
        );
        assert!(
            augment_missing_detected_text_items(&mut analysis)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            analysis["translation_handoff"]["items"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn clean_mask_accepts_one_full_afterword_anchor_and_rejects_partial_overlap() {
        let analysis = serde_json::json!({
            "target_language": "vi",
            "bubbles": [],
            "text_lines": [
                {
                    "text": "Thank you for reading this afterword.",
                    "source_language": "en",
                    "confidence": 0.95,
                    "bbox": {"x1": 10.0, "y1": 10.0, "x2": 90.0, "y2": 30.0}
                },
                {
                    "text": "I hope you look forward to the next one.",
                    "source_language": "en",
                    "confidence": 0.95,
                    "bbox": {"x1": 10.0, "y1": 40.0, "x2": 90.0, "y2": 60.0}
                }
            ],
            "translation_handoff": {"items": [{
                "id": "afterword-anchor",
                "kind": "unmatched_text",
                "preserve_by_default": true,
                "source_text": "full afterword source anchor",
                "source_language": "en",
                "bbox": {"x1": 5.0, "y1": 5.0, "x2": 95.0, "y2": 65.0}
            }]}
        });
        let (_dir, path) = write_checkpoint(&analysis);
        assert_eq!(checkpoint_text_regions(&path, false).unwrap().len(), 2);

        let mut partial = analysis;
        partial["translation_handoff"]["items"][0]["bbox"] = serde_json::json!({
            "x1": 5.0,
            "y1": 5.0,
            "x2": 95.0,
            "y2": 35.0
        });
        let (_dir, path) = write_checkpoint(&partial);
        let error = checkpoint_text_regions(&path, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not represented") || error.contains("no translated item"));
    }

    #[test]
    fn empty_handoff_cannot_silently_preserve_detected_english_prose() {
        let analysis = serde_json::json!({
            "target_language": "vi",
            "bubbles": [],
            "unmatched_text": [],
            "text_lines": [{
                "text": "This long English paragraph was detected on the page.",
                "source_language": "en",
                "confidence": 0.95,
                "bbox": {"x1": 1.0, "y1": 1.0, "x2": 80.0, "y2": 40.0}
            }],
            "translation_handoff": {"items": []}
        });
        let (_dir, path) = write_checkpoint(&analysis);
        let error = saved_translation_items(&analysis).unwrap_err().to_string();
        assert!(error.contains("silent pass-through"));
        let error = checkpoint_text_regions(&path, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no translated item"));
    }

    fn write_checkpoint(value: &serde_json::Value) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("analysis.json");
        std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        (dir, path)
    }

    fn overlapping_double_text_checkpoint() -> serde_json::Value {
        serde_json::json!({
            "bubbles": [{
                "id": "bubble-1",
                "bbox": {"x1": 10.0, "y1": 10.0, "x2": 80.0, "y2": 80.0}
            }],
            "text_lines": [
                {
                    "id": "line-dialogue",
                    "text": "原文",
                    "confidence": 0.91,
                    "bbox": {"x1": 20.0, "y1": 20.0, "x2": 60.0, "y2": 40.0}
                },
                {
                    "id": "line-low-conf-dialogue",
                    "text": "原文",
                    "confidence": 0.21,
                    "bbox": {"x1": 22.0, "y1": 42.0, "x2": 58.0, "y2": 55.0}
                },
                {
                    "id": "line-sfx",
                    "text": "ドン",
                    "confidence": 0.96,
                    "bbox": {"x1": 82.0, "y1": 82.0, "x2": 110.0, "y2": 110.0}
                },
                {
                    "id": "line-noise",
                    "text": "",
                    "confidence": 0.18,
                    "bbox": {"x1": 1.0, "y1": 1.0, "x2": 8.0, "y2": 8.0}
                }
            ],
            "translation_handoff": {
                "items": [{
                    "id": "bubble-1",
                    "kind": "dialogue",
                    "source_text": "原文",
                    "bbox": {"x1": 10.0, "y1": 10.0, "x2": 80.0, "y2": 80.0}
                }, {
                    "id": "text-sfx",
                    "kind": "unmatched_text",
                    "preserve_by_default": true,
                    "source_text": "ドン",
                    "bbox": {"x1": 50.0, "y1": 25.0, "x2": 95.0, "y2": 95.0}
                }]
            }
        })
    }

    #[test]
    fn overlapping_preserved_sfx_does_not_drop_dialogue_inpaint_regions() {
        let (_dir, path) = write_checkpoint(&overlapping_double_text_checkpoint());
        let regions = checkpoint_text_regions(&path, false).unwrap();
        assert!(
            regions.iter().any(|rect| {
                (rect.x1 - 20.0).abs() < f32::EPSILON && (rect.y1 - 20.0).abs() < f32::EPSILON
            }),
            "dialogue text line overlapping preserved SFX must stay in the inpaint mask: {regions:?}"
        );
        assert!(
            regions.iter().any(|rect| {
                (rect.x1 - 22.0).abs() < f32::EPSILON && (rect.y1 - 42.0).abs() < f32::EPSILON
            }),
            "low-confidence dialogue inside a translatable bubble must still be inpainted: {regions:?}"
        );
        assert!(
            !regions.iter().any(|rect| rect.x1 > 80.0 && rect.y1 > 80.0),
            "unmatched SFX outside a bubble must stay out of the inpaint mask: {regions:?}"
        );
        assert!(
            !regions.iter().any(|rect| rect.x2 <= 8.0),
            "low-confidence unmatched noise must stay out of the inpaint mask: {regions:?}"
        );
    }

    #[test]
    fn keep_source_dialogue_is_excluded_from_inpaint_mask() {
        let mut analysis = overlapping_double_text_checkpoint();
        analysis["translation_handoff"]["items"][0]["keep_source"] = serde_json::json!(true);
        let (_dir, path) = write_checkpoint(&analysis);
        let regions = checkpoint_text_regions(&path, false).unwrap();
        assert!(
            !regions.iter().any(|rect| rect.x1 < 80.0 && rect.y1 < 80.0),
            "keep_source dialogue must not be inpainted: {regions:?}"
        );
    }

    #[test]
    fn replace_sfx_keeps_unmatched_text_in_the_inpaint_mask() {
        let (_dir, path) = write_checkpoint(&overlapping_double_text_checkpoint());
        let regions = checkpoint_text_regions(&path, true).unwrap();
        assert!(
            regions.iter().any(|rect| {
                (rect.x1 - 82.0).abs() < f32::EPSILON && (rect.y1 - 82.0).abs() < f32::EPSILON
            }),
            "replace mode must inpaint unmatched SFX: {regions:?}"
        );
    }

    #[test]
    fn strict_submission_updates_dialogue_by_id_when_preserved_text_is_first() {
        let mut analysis = strict_fixture_analysis();
        analysis["strict_v1"]["auto_preserved_items"] = serde_json::json!([{
            "id": "line-legacy",
            "detected_text_index": 4,
            "reason": "legacy handoff omitted a detected line"
        }]);
        let items = analysis["translation_handoff"]["items"]
            .as_array_mut()
            .unwrap();
        items.swap(0, 1);
        let claim = TranslationClaim {
            job_dir: PathBuf::from("C:/jobs/job"),
            source_image: PathBuf::from("C:/source/page.png"),
            page_number: 1,
            total_pages: 1,
            analysis_path: PathBuf::from("C:/jobs/job/pages/0001/analysis.json"),
            analysis_sha256: "hash".into(),
            item_ids: ["bubble-1".into()].into_iter().collect(),
            source_language: Some("ja".into()),
            target_language: Some("vi".into()),
            replace_sfx: false,
            in_progress: true,
            consumed: false,
        };
        FukidashiServer::apply_strict_translations(
            &mut analysis,
            &claim,
            &[TranslationSubmission {
                id: "bubble-1".into(),
                translation: Some("dịch".into()),
                keep_source: Some(false),
                needs_review: Some(false),
            }],
        )
        .unwrap();
        let saved = analysis["translation_handoff"]["items"].as_array().unwrap();
        assert_eq!(saved[0]["id"], "text-sfx");
        assert_eq!(saved[0]["status"], "preserved");
        assert_eq!(saved[1]["id"], "bubble-1");
        assert_eq!(saved[1]["translation"], "dịch");
        assert_eq!(
            analysis["strict_v1"]["auto_preserved_items"][0]["id"],
            "line-legacy"
        );
    }

    #[test]
    fn strict_partial_submission_rejects_invalid_geometry_before_handoff() {
        let mut analysis = strict_fixture_analysis();
        analysis["translation_handoff"]["items"][0]["bbox"] = serde_json::Value::Null;
        let claim = TranslationClaim {
            job_dir: PathBuf::from("C:/jobs/job"),
            source_image: PathBuf::from("C:/source/page.png"),
            page_number: 1,
            total_pages: 1,
            analysis_path: PathBuf::from("C:/jobs/job/pages/0001/analysis.json"),
            analysis_sha256: "hash".into(),
            item_ids: ["bubble-1".into()].into_iter().collect(),
            source_language: Some("ja".into()),
            target_language: Some("vi".into()),
            replace_sfx: false,
            in_progress: true,
            consumed: false,
        };
        let error = FukidashiServer::apply_strict_translations(
            &mut analysis,
            &claim,
            &[TranslationSubmission {
                id: "bubble-1".into(),
                translation: Some("dịch".into()),
                keep_source: Some(false),
                needs_review: Some(false),
            }],
        )
        .expect_err("partial translation must still validate its bbox");
        assert!(error.to_string().contains("invalid bbox"));
        assert_eq!(analysis["translation_handoff"]["status"], "pending");
        assert!(analysis["translation_handoff"]["items"][0]["translation"].is_null());
    }

    #[tokio::test]
    async fn strict_start_resumes_saved_analysis_without_inference_or_artifact_paths() {
        let temp = tempfile::tempdir().unwrap();
        let source_dir = temp.path().join("comic");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("page-2.png");
        image::RgbImage::from_pixel(16, 16, image::Rgb([255, 255, 255]))
            .save(&source)
            .unwrap();
        let config = test_config(temp.path());
        let workflow = Workflow::new(config.jobs_dir()).unwrap();
        let registration = workflow.register_analysis(&source, None).unwrap();
        let analysis = strict_fixture_analysis();
        workflow
            .write_analysis_artifact(&source, &analysis)
            .unwrap();
        let job_id = registration
            .job_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let server = FukidashiServer::new(config).unwrap();
        let initial_lore = extract_tool_json(
            server
                .get_lore(Parameters(LoreRequest {
                    job_id: job_id.clone(),
                }))
                .await,
            "get lore",
        )
        .unwrap();
        assert_eq!(initial_lore["lore"]["schema"], 1);
        let natural_lore = server
            .put_lore(Parameters(PutLoreRequest {
                job_id: job_id.clone(),
                lore: serde_json::json!({
                    "schema": 1,
                    "characters": ["Fuyu", "Kuga", "Sosuke"],
                    "pronouns": [],
                    "glossary": [{"source":"proprietress","target":"bà chủ"}]
                }),
            }))
            .await;
        let natural_lore = extract_tool_json(natural_lore, "natural put lore").unwrap();
        assert_eq!(natural_lore["lore"]["characters"][0]["id"], "fuyu");
        assert_eq!(natural_lore["lore"]["characters"][1]["names"][0], "Kuga");
        let authored_lore = server
            .put_lore(Parameters(PutLoreRequest {
                job_id: job_id.clone(),
                lore: serde_json::json!({
                    "schema": 1,
                    "characters": [{"id":"lisa","names":["Lisa"]}],
                    "future": {"tone": "dry"}
                }),
            }))
            .await;
        let authored_lore = extract_tool_json(authored_lore, "put lore").unwrap();
        assert_eq!(authored_lore["lore"]["future"]["tone"], "dry");
        let result = server
            .translation_start(Parameters(TranslationStartRequest {
                job_id: Some(job_id.clone()),
                ..Default::default()
            }))
            .await;
        let value = extract_tool_json(result, "strict start").unwrap();
        assert_eq!(value["protocol"], "strict-v1");
        assert_eq!(value["status"], "page_ready");
        assert_eq!(value["lore"]["characters"][0]["id"], "lisa");
        assert_eq!(value["sfx_mode"], "preserve");
        assert_eq!(value["translation_items"][0]["id"], "bubble-1");
        assert_eq!(value["current_page"], 1);
        assert_eq!(value["total_pages"], 1);
        assert_eq!(value["progress"]["current_page"], 1);
        assert_eq!(value["progress"]["total_pages"], 1);
        assert!(
            value["progress"]["message"]
                .as_str()
                .unwrap()
                .contains("[FUKIDASHI] [Page 1/1]")
        );
        assert_eq!(
            value["required_translation_ids"],
            serde_json::json!(["bubble-1"])
        );
        assert_eq!(value["preserved_items"][0]["id"], "text-sfx");
        assert!(
            value["work_token"]
                .as_str()
                .unwrap()
                .starts_with("strict-v1-")
        );
        assert!(!value.to_string().contains("analysis.json"));
        assert!(
            !value
                .to_string()
                .contains(source.to_string_lossy().as_ref())
        );
        let replace = server
            .translation_start(Parameters(TranslationStartRequest {
                job_id: Some(job_id),
                sfx_mode: Some("replace".into()),
                ..Default::default()
            }))
            .await;
        let replace = extract_tool_json(replace, "strict replace start").unwrap();
        assert_eq!(replace["sfx_mode"], "replace");
        assert_eq!(replace["translation_items"].as_array().unwrap().len(), 2);
        assert_eq!(replace["preserved_items"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn preserve_only_page_creates_pass_through_render_and_advances() {
        let temp = tempfile::tempdir().unwrap();
        let source_dir = temp.path().join("comic");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("cover.webp");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            24,
            24,
            image::Rgb([240, 240, 240]),
        ))
        .save_with_format(&source, image::ImageFormat::WebP)
        .unwrap();
        let config = test_config(temp.path());
        let workflow = Workflow::new(config.jobs_dir()).unwrap();
        let registration = workflow.register_analysis(&source, None).unwrap();
        let analysis = serde_json::json!({
            "source_language": "ja",
            "target_language": "vi",
            "bubbles": [],
            "text_lines": [],
            "unmatched_text": [],
            "translation_handoff": {"target_language":"vi","status":"pending","items":[]}
        });
        workflow
            .write_analysis_artifact(&source, &analysis)
            .unwrap();
        let job_id = registration
            .job_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let server = FukidashiServer::new(config).unwrap();
        let result = server
            .translation_start(Parameters(TranslationStartRequest {
                job_id: Some(job_id.clone()),
                ..Default::default()
            }))
            .await;
        let value = extract_tool_json(result, "preserve-only start").unwrap();
        assert_eq!(value["status"], "review_ready");
        assert_eq!(value["auto_preserved_pages"], serde_json::json!([1]));
        let page = server
            .workflow
            .managed_page(&registration.job_dir, &source)
            .unwrap();
        assert_eq!(page.state, "rendered");
        let clean = server
            .workflow
            .validate_clean_input(&page.cleaned_image)
            .unwrap();
        assert!(clean.passthrough);
        assert!(
            server
                .workflow
                .validate_render_input(&page.rendered_image)
                .is_ok()
        );
        assert!(
            server
                .workflow
                .next_pending_page(&registration.job_dir)
                .unwrap()
                .is_none()
        );
        let resumed = server
            .translation_start(Parameters(TranslationStartRequest {
                job_id: Some(job_id.clone()),
                ..Default::default()
            }))
            .await;
        let resumed = extract_tool_json(resumed, "preserve-only resume").unwrap();
        assert_eq!(resumed["status"], "review_ready");
        assert!(
            !resumed
                .to_string()
                .contains("clean stage was changed after validation")
        );

        // The same path handles an SFX-only analysis and records it as
        // preserved audit data instead of dead-ending with manual scope.
        let second_source_dir = temp.path().join("sfx-comic");
        std::fs::create_dir_all(&second_source_dir).unwrap();
        let second_source = second_source_dir.join("sfx-only.png");
        image::RgbImage::from_pixel(24, 24, image::Rgb([240, 240, 240]))
            .save(&second_source)
            .unwrap();
        let second = server
            .workflow
            .register_analysis(&second_source, None)
            .unwrap();
        let mut sfx = strict_fixture_analysis();
        let items = sfx["translation_handoff"]["items"].as_array_mut().unwrap();
        items.remove(0);
        sfx["bubbles"] = serde_json::json!([]);
        sfx["text_lines"] = serde_json::json!([]);
        sfx["unmatched_text"] = serde_json::json!([]);
        server
            .workflow
            .write_analysis_artifact(&second_source, &sfx)
            .unwrap();
        let result = server
            .translation_start(Parameters(TranslationStartRequest {
                job_id: Some(
                    second
                        .job_dir
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                ),
                ..Default::default()
            }))
            .await;
        let value = extract_tool_json(result, "sfx-only start").unwrap();
        assert_eq!(value["status"], "review_ready");
        assert_eq!(value["auto_preserved_pages"], serde_json::json!([1]));
    }

    #[tokio::test]
    async fn all_keep_source_with_invalid_geometry_passes_through_and_advances() {
        let temp = tempfile::tempdir().unwrap();
        let config = test_config(temp.path());
        let workflow = Workflow::new(config.jobs_dir()).unwrap();
        let registration = workflow
            .import_ingress_pages(
                "silent-false-positives",
                vec![
                    (
                        "page-1.png".into(),
                        image::RgbImage::from_pixel(24, 24, image::Rgb([240, 240, 240])),
                    ),
                    (
                        "page-2.png".into(),
                        image::RgbImage::from_pixel(24, 24, image::Rgb([240, 240, 240])),
                    ),
                ],
                serde_json::json!({"source":"test"}),
            )
            .unwrap();
        let page_one = registration.expected_pages[0].clone();
        let page_two = registration.expected_pages[1].clone();
        let mut invalid_geometry_analysis = strict_fixture_analysis();
        invalid_geometry_analysis["translation_handoff"]["items"][0]["bbox"] =
            serde_json::Value::Null;
        invalid_geometry_analysis["bubbles"][0]["bbox"] = serde_json::Value::Null;
        workflow
            .write_analysis_artifact(&page_one, &invalid_geometry_analysis)
            .unwrap();
        workflow
            .write_analysis_artifact(&page_two, &strict_fixture_analysis())
            .unwrap();
        let job_id = registration
            .job_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let server = FukidashiServer::new(config).unwrap();
        let started = server
            .translation_start(Parameters(TranslationStartRequest {
                job_id: Some(job_id.clone()),
                ..Default::default()
            }))
            .await;
        let started = extract_tool_json(started, "all-keep start").unwrap();
        assert_eq!(started["status"], "page_ready");
        let token = started["work_token"].as_str().unwrap().to_owned();
        let submitted = server
            .translation_submit(Parameters(TranslationSubmitRequest {
                work_token: token,
                translations: vec![TranslationSubmission {
                    id: "bubble-1".into(),
                    translation: None,
                    keep_source: Some(true),
                    needs_review: Some(true),
                }],
            }))
            .await;
        let submitted = extract_tool_json(submitted, "all-keep submit").unwrap();
        assert_eq!(submitted["status"], "page_ready");
        assert_eq!(submitted["completed_page"], 1);
        assert_eq!(submitted["current_page"], 2);
        assert_eq!(submitted["auto_preserved"], true);
        assert_eq!(submitted["pass_through"], true);
        assert_eq!(
            submitted["next_action"]["tool"],
            "fukidashi_translation_submit"
        );
        let page = server
            .workflow
            .managed_page(&registration.job_dir, &page_one)
            .unwrap();
        assert_eq!(page.state, "rendered");
        assert!(
            server
                .workflow
                .validate_clean_input(&page.cleaned_image)
                .unwrap()
                .passthrough
        );
        assert!(
            server
                .workflow
                .validate_render_input(&page.rendered_image)
                .is_ok()
        );

        let resumed = server
            .translation_start(Parameters(TranslationStartRequest {
                job_id: Some(job_id),
                ..Default::default()
            }))
            .await;
        let resumed = extract_tool_json(resumed, "all-keep resume").unwrap();
        assert_eq!(resumed["status"], "page_ready");
        assert_eq!(resumed["current_page"], 2);

        let second = server
            .translation_submit(Parameters(TranslationSubmitRequest {
                work_token: resumed["work_token"].as_str().unwrap().into(),
                translations: vec![TranslationSubmission {
                    id: "bubble-1".into(),
                    translation: None,
                    keep_source: Some(true),
                    needs_review: Some(false),
                }],
            }))
            .await;
        let second = extract_tool_json(second, "all-keep final submit").unwrap();
        assert_eq!(second["status"], "review_ready");
        assert_eq!(second["completed_page"], 2);
        assert_eq!(second["auto_preserved"], true);
        assert!(
            server
                .workflow
                .next_pending_page(&registration.job_dir)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn combined_review_call_waits_for_approval_then_exports_zip() {
        let temp = tempfile::tempdir().unwrap();
        let source_dir = temp.path().join("comic");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("page-1.png");
        let mut source_image = image::RgbImage::from_pixel(32, 32, image::Rgb([255, 255, 255]));
        source_image.put_pixel(3, 3, image::Rgb([0, 0, 0]));
        source_image.save(&source).unwrap();

        let config = test_config(temp.path());
        let workflow = Workflow::new(config.jobs_dir()).unwrap();
        let registration = workflow.register_analysis(&source, None).unwrap();
        let cleaned = image::RgbImage::from_pixel(32, 32, image::Rgb([255, 255, 255]));
        let mut mask = image::GrayImage::new(32, 32);
        mask.put_pixel(3, 3, image::Luma([255]));
        let (cleaned_path, _, _) = workflow
            .write_clean_artifact(&source, &cleaned, &mask, 3, "full")
            .unwrap();
        let rendered_path = workflow.page_artifacts_for_source(&source).unwrap().4;
        crate::typeset::typeset_page(&cleaned_path, &[], &rendered_path).unwrap();
        let clean = workflow.validate_clean_input(&cleaned_path).unwrap();
        workflow
            .register_render(
                &rendered_path,
                &clean,
                serde_json::json!({"request_bubbles": [], "report": {"bubbles": []}}),
                serde_json::json!({}),
            )
            .unwrap();
        // Seed the strict editor provenance expected by bound approval. The
        // fixture starts from a legacy-looking registration, then upgrades
        // its sidecar exactly as a current renderer would.
        let editor_state = workflow.editor_state(&rendered_path, None).unwrap();
        let semantic =
            crate::editor::post_render_page_signature(&editor_state, &editor_state["pages"][0]);
        let sidecar_path =
            std::path::PathBuf::from(format!("{}.fukidashi-render.json", rendered_path.display()));
        let mut sidecar: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&sidecar_path).unwrap()).unwrap();
        sidecar["qa"]["semantic_render_signature"] = serde_json::Value::String(semantic);
        std::fs::write(&sidecar_path, serde_json::to_vec_pretty(&sidecar).unwrap()).unwrap();

        let job_id = registration
            .job_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let review_path = registration.job_dir.join("review.json");
        let server = FukidashiServer::new(config).unwrap();
        let review_request = ReviewAndExportRequest {
            job_id: Some(job_id),
            job_path: None,
            image_path: None,
            format: "zip".into(),
            timeout_seconds: 10,
        };
        let failed = server
            .review_and_export_inner(review_request.clone(), |_| {
                Err(FukidashiError::RuntimeUnavailable(
                    "test browser unavailable".into(),
                ))
            })
            .await;
        let failed_text = failed
            .content
            .into_iter()
            .find_map(|block| match block {
                ContentBlock::Text(value) => Some(value.text),
                _ => None,
            })
            .unwrap();
        let failed_value: serde_json::Value = serde_json::from_str(&failed_text).unwrap();
        assert_eq!(failed_value["status"], "browser_launch_failed");
        assert!(failed_value["editor_url"].as_str().is_some());
        assert_eq!(
            failed_value["next_action"]["tool"],
            "fukidashi_wait_for_review"
        );
        assert!(
            failed_value["next_action"]["arguments"]["review_session_id"]
                .as_str()
                .is_some()
        );
        let fixes_review_path = review_path.clone();
        let fixes = server
            .review_and_export_inner(review_request.clone(), move |url| {
                let endpoint = url.strip_prefix("http://").ok_or_else(|| {
                    FukidashiError::RuntimeUnavailable("test editor URL is not HTTP".into())
                })?;
                let (host, suffix) = endpoint.split_once('/').ok_or_else(|| {
                    FukidashiError::RuntimeUnavailable("test editor URL has no token".into())
                })?;
                let review: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(&fixes_review_path).map_err(FukidashiError::Io)?,
                )?;
                let revision = review["revision"].as_u64().ok_or_else(|| {
                    FukidashiError::RuntimeUnavailable("test review has no revision".into())
                })?;
                let token = suffix.trim_end_matches('/');
                let request_path = format!("/{token}/review/submit");
                let body = serde_json::json!({
                    "revision": revision,
                    "action": "request_fixes",
                    "feedback": [{
                        "page": 0,
                        "bbox": {"x1": 1, "y1": 1, "x2": 8, "y2": 8},
                        "issue_type": "custom",
                        "note": "second-pass fixture",
                        "origin": "image-pixels"
                    }]
                })
                .to_string();
                let mut stream = std::net::TcpStream::connect(host)?;
                use std::io::{Read, Write};
                write!(
                    stream,
                    "POST {request_path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )?;
                let mut response = Vec::new();
                stream.read_to_end(&mut response)?;
                if !response.starts_with(b"HTTP/1.1 200") {
                    return Err(FukidashiError::RuntimeUnavailable(
                        String::from_utf8_lossy(&response).into_owned(),
                    ));
                }
                Ok(())
            })
            .await;
        let fixes_value = extract_tool_json(fixes, "combined fixes round").unwrap();
        assert_eq!(fixes_value["protocol"], "review-v1");
        assert_eq!(fixes_value["status"], "fixes_requested");
        assert_eq!(fixes_value["review"]["action"], "request_fixes");
        let final_review_path = review_path.clone();
        let result = server
            .review_and_export_inner(
                review_request,
                move |url| {
                    let endpoint = url.strip_prefix("http://").ok_or_else(|| {
                        FukidashiError::RuntimeUnavailable("test editor URL is not HTTP".into())
                    })?;
                    let (host, suffix) = endpoint.split_once('/').ok_or_else(|| {
                        FukidashiError::RuntimeUnavailable("test editor URL has no token".into())
                    })?;
                    let review: serde_json::Value = serde_json::from_slice(
                        &std::fs::read(&final_review_path).map_err(FukidashiError::Io)?,
                    )?;
                    let revision = review["revision"].as_u64().ok_or_else(|| {
                        FukidashiError::RuntimeUnavailable("test review has no revision".into())
                    })?;
                    let token = suffix.trim_end_matches('/');
                    let request_path = format!("/{token}/review/submit");
                    let body = serde_json::json!({
                        "revision": revision,
                        "action": "approve_export",
                        "approved_pages": [0]
                    })
                    .to_string();
                    let mut stream = std::net::TcpStream::connect(host)?;
                    use std::io::{Read, Write};
                    write!(
                        stream,
                        "POST {request_path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )?;
                    let mut response = Vec::new();
                    stream.read_to_end(&mut response)?;
                    if !response.starts_with(b"HTTP/1.1 200") {
                        return Err(FukidashiError::RuntimeUnavailable(
                            String::from_utf8_lossy(&response).into_owned(),
                        ));
                    }
                    Ok(())
                },
            )
            .await;
        let value = extract_tool_json(result, "combined review/export").unwrap();
        assert_eq!(value["protocol"], "review-v1");
        assert_eq!(value["status"], "exported");
        assert_eq!(value["review"]["action"], "approve_export");
        let output = value["export"]["output_path"].as_str().unwrap();
        assert!(std::path::Path::new(output).is_file());
        assert!(output.ends_with(".zip"));
        let persisted_review: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&review_path).unwrap()).unwrap();
        let revision = persisted_review["revision"].as_u64().unwrap();
        let binding = persisted_review["audit"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|entry| entry["event"] == "approve_export")
            .unwrap();
        assert_eq!(binding["approval"]["dirty_pages"], serde_json::json!([]));
        assert!(crate::approval::checkpoint_path(&registration.job_dir, revision).is_file());

        // Direct export must use the same binding and all-page validation as
        // the automatic path. This page was reused (zero dirty pages), so its
        // bytes are covered by the export-time managed artifact check.
        std::fs::write(&rendered_path, b"tampered-after-approval").unwrap();
        let direct = server
            .export(Parameters(ExportRequest {
                project_dir: registration.job_dir.display().to_string(),
                format: "zip".into(),
            }))
            .await;
        let direct_error = extract_tool_json(direct, "tampered direct export").unwrap_err();
        assert!(direct_error.to_string().contains("invalid"));
    }

    #[test]
    fn bound_export_rejects_stale_reused_sidecar_and_missing_render_hash() {
        let temp = tempfile::tempdir().unwrap();
        let source_dir = temp.path().join("comic");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("page-1.png");
        image::RgbImage::from_pixel(16, 16, image::Rgb([255, 255, 255]))
            .save(&source)
            .unwrap();

        let workflow = Workflow::new(temp.path().join("jobs")).unwrap();
        let registration = workflow.register_analysis(&source, None).unwrap();
        let (cleaned_path, _, _) = workflow.write_passthrough_clean_artifact(&source).unwrap();
        let rendered_path = workflow.page_artifacts_for_source(&source).unwrap().4;
        crate::typeset::typeset_page(&cleaned_path, &[], &rendered_path).unwrap();
        let clean = workflow.validate_clean_input(&cleaned_path).unwrap();
        workflow
            .register_render(
                &rendered_path,
                &clean,
                serde_json::json!({"request_bubbles": [], "report": {"bubbles": []}}),
                serde_json::json!({}),
            )
            .unwrap();
        let state = workflow.editor_state(&rendered_path, None).unwrap();
        let sidecar_path =
            std::path::PathBuf::from(format!("{}.fukidashi-render.json", rendered_path.display()));
        let mut sidecar: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&sidecar_path).unwrap()).unwrap();
        // Model a legacy editor-clean alias: it is a separate path with an
        // independently valid clean sidecar, but the same source and bytes
        // as the canonical clean image recorded by project.json.
        let legacy_clean = cleaned_path.with_file_name("legacy-clean.png");
        std::fs::copy(&cleaned_path, &legacy_clean).unwrap();
        let legacy_clean_sidecar =
            std::path::PathBuf::from(format!("{}.fukidashi-clean.json", legacy_clean.display()));
        let mut clean_sidecar: serde_json::Value = serde_json::from_slice(
            &std::fs::read(format!("{}.fukidashi-clean.json", cleaned_path.display())).unwrap(),
        )
        .unwrap();
        clean_sidecar["cleaned_image"] =
            serde_json::Value::String(legacy_clean.display().to_string());
        std::fs::write(
            &legacy_clean_sidecar,
            serde_json::to_vec_pretty(&clean_sidecar).unwrap(),
        )
        .unwrap();
        sidecar["cleaned_image"] = serde_json::Value::String(legacy_clean.display().to_string());
        sidecar["clean_sidecar"] =
            serde_json::Value::String(legacy_clean_sidecar.display().to_string());
        let expected_semantic =
            crate::editor::post_render_page_signature(&state, &state["pages"][0]);
        sidecar["qa"]
            .as_object_mut()
            .unwrap()
            .remove("semantic_render_signature");
        std::fs::write(&sidecar_path, serde_json::to_vec_pretty(&sidecar).unwrap()).unwrap();
        let project_path = registration.job_dir.join("project.json");
        std::fs::write(&project_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        assert!(
            FukidashiServer::validate_managed_export_artifacts(&registration.job_dir, true).is_ok()
        );
        let migrated: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&sidecar_path).unwrap()).unwrap();
        assert_eq!(
            migrated["qa"]["semantic_render_signature"],
            expected_semantic
        );
        assert_eq!(
            std::fs::canonicalize(migrated["cleaned_image"].as_str().unwrap()).unwrap(),
            std::fs::canonicalize(&cleaned_path).unwrap()
        );

        let mut stale = state.clone();
        stale["pages"][0]["bubbles"] = serde_json::json!([{
            "id": "after-approval",
            "bbox": {"x1": 1.0, "y1": 1.0, "x2": 8.0, "y2": 8.0},
            "translation": "stale sidecar"
        }]);
        let mut legacy_sidecar = migrated.clone();
        legacy_sidecar["qa"]
            .as_object_mut()
            .unwrap()
            .remove("semantic_render_signature");
        std::fs::write(
            &sidecar_path,
            serde_json::to_vec_pretty(&legacy_sidecar).unwrap(),
        )
        .unwrap();
        std::fs::write(&project_path, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();
        let legacy_stale_error =
            FukidashiServer::validate_managed_export_artifacts(&registration.job_dir, true)
                .unwrap_err()
                .to_string();
        assert!(legacy_stale_error.contains("legacy render sidecar"));
        let still_legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&sidecar_path).unwrap()).unwrap();
        assert!(
            still_legacy["qa"]
                .get("semantic_render_signature")
                .is_none()
        );

        std::fs::write(&project_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        sidecar["qa"]["semantic_render_signature"] =
            serde_json::Value::String(expected_semantic.clone());
        std::fs::write(&sidecar_path, serde_json::to_vec_pretty(&sidecar).unwrap()).unwrap();
        std::fs::write(&project_path, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();
        let stale_error =
            FukidashiServer::validate_managed_export_artifacts(&registration.job_dir, true)
                .unwrap_err()
                .to_string();
        assert!(stale_error.contains("semantic signature"));

        std::fs::write(&project_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        sidecar["qa"]["semantic_render_signature"] = serde_json::Value::String(
            crate::editor::post_render_page_signature(&state, &state["pages"][0]),
        );
        sidecar["rendered_sha256"] = serde_json::Value::String(String::new());
        std::fs::write(&sidecar_path, serde_json::to_vec_pretty(&sidecar).unwrap()).unwrap();
        let missing_hash_error =
            FukidashiServer::validate_managed_export_artifacts(&registration.job_dir, true)
                .unwrap_err()
                .to_string();
        assert!(missing_hash_error.contains("byte hash"));
    }

    #[test]
    fn strict_submission_requires_exact_ids_and_keeps_uncertainty_explicit() {
        let mut analysis = strict_fixture_analysis();
        let claim = TranslationClaim {
            job_dir: PathBuf::from("C:/jobs/job"),
            source_image: PathBuf::from("C:/source/page.png"),
            page_number: 1,
            total_pages: 1,
            analysis_path: PathBuf::from("C:/jobs/job/pages/0001/analysis.json"),
            analysis_sha256: "hash".into(),
            item_ids: ["bubble-1".into()].into_iter().collect(),
            source_language: Some("ja".into()),
            target_language: Some("vi".into()),
            replace_sfx: false,
            in_progress: true,
            consumed: false,
        };
        let missing =
            FukidashiServer::apply_strict_translations(&mut analysis.clone(), &claim, &[])
                .unwrap_err()
                .to_string();
        assert!(missing.contains("missing stable ids"));
        let unknown = FukidashiServer::apply_strict_translations(
            &mut analysis.clone(),
            &claim,
            &[TranslationSubmission {
                id: "other".into(),
                translation: Some("x".into()),
                keep_source: Some(false),
                needs_review: Some(false),
            }],
        )
        .unwrap_err()
        .to_string();
        assert!(unknown.contains("unknown translation id"));
        let duplicate = FukidashiServer::apply_strict_translations(
            &mut analysis.clone(),
            &claim,
            &[
                TranslationSubmission {
                    id: "bubble-1".into(),
                    translation: Some("x".into()),
                    keep_source: Some(false),
                    needs_review: Some(false),
                },
                TranslationSubmission {
                    id: "bubble-1".into(),
                    translation: Some("y".into()),
                    keep_source: Some(false),
                    needs_review: Some(false),
                },
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(duplicate.contains("duplicate translation id"));
        let payloads = FukidashiServer::apply_strict_translations(
            &mut analysis,
            &claim,
            &[TranslationSubmission {
                id: "bubble-1".into(),
                translation: None,
                keep_source: Some(true),
                needs_review: Some(true),
            }],
        )
        .unwrap();
        assert_eq!(payloads[0].text, "原文");
        assert_eq!(
            payloads[0].bubble_bbox,
            Some(Rect {
                x1: 0.5,
                y1: 0.5,
                x2: 20.0,
                y2: 20.0,
            })
        );
        assert_eq!(payloads[0].needs_review, Some(true));
        assert_eq!(payloads[0].flagged, None);
        assert_eq!(
            analysis["translation_handoff"]["items"][0]["keep_source"],
            true
        );
        assert_eq!(
            analysis["translation_handoff"]["items"][0]["needs_review"],
            true
        );
        assert_eq!(
            analysis["translation_handoff"]["items"][0]["status"],
            "needs_review"
        );
    }

    #[test]
    fn compact_analysis_avoids_duplicate_geometry_and_full_checkpoint_is_atomic() {
        let bbox = Rect {
            x1: 10.0,
            y1: 20.0,
            x2: 30.0,
            y2: 40.0,
        };
        let vision = VisionCorrection {
            image_path: "E:/work/2.webp".into(),
            bbox,
            contract: "source-image-bbox",
        };
        let region = OcrRegion {
            id: "bubble-1".into(),
            bbox,
            text: "原文".into(),
            source_language: "ja".into(),
            script: "han".into(),
            recognizer: "baberu-ocr".into(),
            confidence: 0.9,
            uncertainty: 0.1,
            detector_label: 0,
            detector_confidence: 0.95,
            reading_order: 0,
            vision_correction: vision.clone(),
        };
        let analysis = PageAnalysis {
            source_language: "ja".into(),
            target_language: "vi".into(),
            bubbles: vec![region],
            text_lines: Vec::new(),
            unmatched_text: Vec::new(),
            translation_handoff: TranslationHandoff {
                target_language: "vi".into(),
                status: "pending",
                items: vec![TranslationItem {
                    id: "bubble-1".into(),
                    kind: "dialogue".into(),
                    preserve_by_default: false,
                    source_text: "原文".into(),
                    ocr_text: "原文".into(),
                    confidence: 0.9,
                    corrected_source_text: None,
                    correction_applied: false,
                    source_language: "ja".into(),
                    bbox,
                    translation: None,
                    status: "pending",
                    vision_correction: vision,
                }],
            },
        };
        let compact = compact_analysis(
            &analysis,
            std::path::Path::new("E:/work/2.webp"),
            None,
            true,
        );
        assert_eq!(compact["translation_items"][0]["source_text"], "原文");
        assert!(compact.get("bubbles").is_none());
        assert!(
            compact["translation_items"][0]
                .get("vision_correction")
                .is_none()
        );

        let dir = tempfile::tempdir().unwrap();
        let checkpoint = dir.path().join("2.analysis.json");
        let full = serde_json::to_value(&analysis).unwrap();
        save_json_atomic(&checkpoint, &full).unwrap();
        let replacement = serde_json::json!({"status":"retried"});
        save_json_atomic(&checkpoint, &replacement).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(checkpoint).unwrap()).unwrap();
        assert_eq!(saved, replacement);
    }

    #[test]
    fn request_level_typeset_defaults_fill_only_missing_bubble_values() {
        let bbox = Rect {
            x1: 1.0,
            y1: 2.0,
            x2: 30.0,
            y2: 40.0,
        };
        let request = TypesetRequest {
            image_path: "E:/clean.png".into(),
            bubbles: vec![TypesetPayload {
                id: None,
                source_text: None,
                kind: None,
                preserve_by_default: None,
                needs_review: None,
                flagged: None,
                preserve_source: None,
                fallback_font_paths: Vec::new(),
                bbox,
                bubble_bbox: None,
                text_bbox: None,
                padding: None,
                text: "Xin chào".into(),
                font_path: None,
                requested_font_path: None,
                min_font_size: Some(7.0),
                max_font_size: None,
                text_color: None,
                shape: None,
            }],
            font_path: Some("E:/fonts/default.ttf".into()),
            padding: Some(3.0),
            min_font_size: Some(2.0),
            max_font_size: Some(18.0),
            shape: Some("rectangle".into()),
            fallback_font_paths: vec!["E:/fonts/fallback.ttf".into()],
        };
        let (_, bubbles, fallbacks) = resolve_typeset_request(request);
        assert_eq!(
            bubbles[0].font_path.as_deref(),
            Some("E:/fonts/default.ttf")
        );
        assert_eq!(bubbles[0].padding, Some(3.0));
        assert_eq!(bubbles[0].min_font_size, Some(7.0));
        assert_eq!(bubbles[0].max_font_size, Some(18.0));
        assert_eq!(bubbles[0].shape.as_deref(), Some("rectangle"));
        assert_eq!(fallbacks, ["E:/fonts/fallback.ttf"]);
    }

    #[test]
    fn bundled_typeset_fonts_are_materialized_inside_the_job() {
        let dir = tempfile::tempdir().unwrap();
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let job = workflow.root().join("job-id");
        std::fs::create_dir_all(&job).unwrap();
        let bbox = Rect {
            x1: 1.0,
            y1: 2.0,
            x2: 30.0,
            y2: 40.0,
        };
        let mut bubbles = vec![TypesetPayload {
            id: None,
            source_text: None,
            kind: None,
            preserve_by_default: None,
            needs_review: None,
            flagged: None,
            preserve_source: None,
            fallback_font_paths: Vec::new(),
            bbox,
            bubble_bbox: None,
            text_bbox: None,
            padding: None,
            text: "Tiếng Việt".into(),
            font_path: None,
            requested_font_path: None,
            min_font_size: None,
            max_font_size: None,
            text_color: None,
            shape: None,
        }];
        let (fallbacks, substitutions) =
            materialize_bundled_typeset_fonts(&workflow, &job, &mut bubbles).unwrap();
        assert!(substitutions.is_empty());
        let default = std::path::PathBuf::from(bubbles[0].font_path.as_ref().unwrap());
        assert!(default.starts_with(&job));
        assert_eq!(
            std::fs::read(&default).unwrap(),
            crate::fonts::COMIC_NEUE_REGULAR.bytes
        );
        assert_eq!(fallbacks.len(), 3);
        assert_eq!(
            std::fs::read(&fallbacks[0]).unwrap(),
            crate::fonts::PATRICK_HAND_REGULAR.bytes
        );
        assert_eq!(
            std::fs::read(&fallbacks[1]).unwrap(),
            crate::fonts::NOTO_SANS_SYMBOLS2_REGULAR.bytes
        );
        assert_eq!(
            std::fs::read(&fallbacks[2]).unwrap(),
            crate::fonts::COMIC_NEUE_BOLD.bytes
        );
        assert!(std::path::Path::new(&fallbacks[0]).starts_with(&job));
        assert!(std::path::Path::new(&fallbacks[1]).starts_with(&job));
        assert!(std::path::Path::new(&fallbacks[2]).starts_with(&job));

        let (second, substitutions) =
            materialize_bundled_typeset_fonts(&workflow, &job, &mut bubbles).unwrap();
        assert!(substitutions.is_empty());
        assert_eq!(second, fallbacks);
        assert_eq!(bubbles[0].font_path.as_deref(), default.to_str());
    }

    #[test]
    fn generic_primary_is_substituted_with_bundled_comic_neue() {
        let dir = tempfile::tempdir().unwrap();
        let workflow = Workflow::new(dir.path().join("jobs")).unwrap();
        let job = workflow.root().join("job-id");
        std::fs::create_dir_all(job.join("fonts")).unwrap();
        let requested = job.join("fonts").join("Arial.ttf");
        std::fs::write(&requested, crate::fonts::COMIC_NEUE_REGULAR.bytes).unwrap();
        let mut bubbles = vec![TypesetPayload {
            id: Some("bubble-1".into()),
            source_text: Some("source".into()),
            kind: Some("dialogue".into()),
            preserve_by_default: Some(false),
            needs_review: None,
            flagged: None,
            preserve_source: Some(false),
            fallback_font_paths: Vec::new(),
            bbox: Rect {
                x1: 20.0,
                y1: 20.0,
                x2: 460.0,
                y2: 100.0,
            },
            bubble_bbox: None,
            text_bbox: None,
            padding: None,
            text: "Tiếng Việt ❤".into(),
            font_path: Some(requested.display().to_string()),
            requested_font_path: None,
            min_font_size: None,
            max_font_size: None,
            text_color: None,
            shape: None,
        }];
        let (fallbacks, substitutions) =
            materialize_bundled_typeset_fonts(&workflow, &job, &mut bubbles).unwrap();
        assert_eq!(substitutions.len(), 1);
        assert_eq!(substitutions[0]["reason"], "generic_desktop_primary");
        assert_eq!(
            substitutions[0]["requested_font_path"],
            requested.display().to_string()
        );
        assert!(
            PathBuf::from(bubbles[0].font_path.as_ref().unwrap())
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("-ComicNeue-Regular.ttf"))
        );
        assert_eq!(fallbacks.len(), 3);
        let legacy_arial = job.join("fonts").join("0123456789abcdef-Arial.ttf");
        std::fs::write(&legacy_arial, crate::fonts::COMIC_NEUE_REGULAR.bytes).unwrap();
        let ordered_fallbacks = crate::workflow::order_fallback_font_paths(vec![
            legacy_arial.display().to_string(),
            fallbacks[0].clone(),
            fallbacks[1].clone(),
            fallbacks[2].clone(),
        ]);
        assert!(ordered_fallbacks[0].ends_with("PatrickHand-Regular.ttf"));
        let source = dir.path().join("source.png");
        let output = dir.path().join("rendered.png");
        image::RgbImage::from_pixel(480, 120, image::Rgb([255, 255, 255]))
            .save(&source)
            .unwrap();
        let report = crate::typeset::typeset_page_with_fallbacks(
            &source,
            &bubbles,
            &ordered_fallbacks,
            &output,
        )
        .unwrap();
        let runs = report["bubbles"][0]["font_runs"].as_array().unwrap();
        assert!(runs.iter().any(|run| {
            run["font_id"]
                .as_str()
                .is_some_and(|id| id.ends_with("PatrickHand-Regular.ttf"))
                && run["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("Tiếng"))
        }));
        assert!(runs.iter().any(|run| {
            run["font_id"]
                .as_str()
                .is_some_and(|id| id.ends_with("NotoSansSymbols2-Regular.ttf"))
                && run["text"] == "❤"
        }));
    }

    #[test]
    fn font_provenance_scenarios_6_and_7_global_font_affects_inherited_not_explicit() {
        let req_inherited = TypesetRequest {
            image_path: "clean.png".into(),
            font_path: Some("fonts/new-global.ttf".into()),
            bubbles: vec![TypesetPayload {
                id: Some("bubble-inherited".into()),
                text: "Hello".into(),
                font_path: None,
                requested_font_path: None,
                ..Default::default()
            }],
            padding: None,
            min_font_size: None,
            max_font_size: None,
            shape: None,
            fallback_font_paths: Vec::new(),
        };
        let (_, resolved_inherited, _) = resolve_typeset_request(req_inherited);
        assert_eq!(
            resolved_inherited[0].font_path.as_deref(),
            Some("fonts/new-global.ttf")
        );
        assert_eq!(resolved_inherited[0].requested_font_path, None);

        let req_explicit = TypesetRequest {
            image_path: "clean.png".into(),
            font_path: Some("fonts/new-global.ttf".into()),
            bubbles: vec![TypesetPayload {
                id: Some("bubble-explicit".into()),
                text: "Hello".into(),
                font_path: Some("fonts/bubble-explicit.ttf".into()),
                requested_font_path: Some("fonts/bubble-explicit.ttf".into()),
                ..Default::default()
            }],
            padding: None,
            min_font_size: None,
            max_font_size: None,
            shape: None,
            fallback_font_paths: Vec::new(),
        };
        let (_, resolved_explicit, _) = resolve_typeset_request(req_explicit);
        assert_eq!(
            resolved_explicit[0].font_path.as_deref(),
            Some("fonts/bubble-explicit.ttf")
        );
        assert_eq!(
            resolved_explicit[0].requested_font_path.as_deref(),
            Some("fonts/bubble-explicit.ttf")
        );
    }
}
