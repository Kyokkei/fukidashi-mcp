use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

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
    workflow::{PendingPage, ScopeSpec, Workflow, emit_page_progress, page_progress_json},
};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AnalyzeRequest {
    pub image_path: String,
    pub ocr_mode: Option<String>,
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
    pub scope: Option<AnalyzeScope>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AnalyzeScope {
    #[serde(default)]
    pub start_page: Option<usize>,
    #[serde(default)]
    pub end_page: Option<usize>,
    #[serde(default)]
    pub include_paths: Option<Vec<String>>,
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
    /// generic Windows UI faces such as Arial, Calibri, and Segoe UI are
    /// substituted when supplied as a primary.
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
}

fn json_object_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type": "object"})
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
    pub source_language: Option<String>,
    #[serde(default)]
    pub target_language: Option<String>,
    #[serde(default)]
    pub scope: Option<AnalyzeScope>,
    /// `preserve` keeps structurally unmatched text out of clean/typeset;
    /// `replace` opts into translating and replacing it. Defaults to preserve.
    #[serde(default)]
    pub sfx_mode: Option<String>,
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
    #[schemars(schema_with = "json_object_schema")]
    pub lore: serde_json::Value,
}

/// One translation decision keyed by the stable OCR item id.  `keep_source`
/// is the explicit uncertainty-preserving choice; it is never inferred from
/// low OCR confidence.  `needs_review` carries uncertainty through a render
/// so a weak OCR result cannot make a client abandon the page.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TranslationSubmission {
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

fn default_review_timeout() -> u64 {
    3600
}

fn default_export_format() -> String {
    "zip".to_owned()
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

const MAX_SAVED_ANALYSIS_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
struct SavedTranslationItem {
    id: String,
    kind: String,
    preserve_by_default: bool,
    source_text: String,
    source_language: String,
    confidence: f32,
    bbox: Rect,
    correction_applied: bool,
    status: String,
    translation: Option<String>,
    keep_source: bool,
    needs_review: bool,
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
        let bbox = serde_json::from_value::<Rect>(
            item.get("bbox").cloned().unwrap_or(serde_json::Value::Null),
        )
        .map_err(|error| {
            FukidashiError::InvalidInput(format!(
                "translation item {id:?} has invalid bbox: {error}"
            ))
        })?
        .validate()?;
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
    Ok(parsed)
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
        return Err(FukidashiError::Inference(format!("{operation}: {message}")));
    }
    Ok(value)
}

fn strict_response_items(items: &[SavedTranslationItem]) -> Vec<serde_json::Value> {
    items.iter().map(strict_item_json).collect()
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

fn translatable_items(
    items: &[SavedTranslationItem],
    replace_sfx: bool,
) -> Vec<SavedTranslationItem> {
    items
        .iter()
        .filter(|item| replace_sfx || !item.preserve_by_default)
        .cloned()
        .collect()
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
        let items = translatable_items(&all_items, replace_sfx);
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
        let items = translatable_items(&all_items, replace_sfx);
        let preserved_items = if replace_sfx {
            Vec::new()
        } else {
            all_items
                .iter()
                .filter(|item| item.preserve_by_default)
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
            engine.release_sessions();
            persist.map_err(|error| {
                FukidashiError::Inference(format!("analysis checkpoint write failed: {error}"))
            })?;
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
        preserved["strict_v1"] = serde_json::json!({
            "status": "preserved",
            "sfx_mode": "preserve",
            "preserved_count": items.len(),
        });
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
            let analysis = if pending.analysis_path.is_file() {
                read_saved_analysis(&self.workflow, &pending.analysis_path)?
            } else {
                self.analyze_strict_page(&pending, request).await?
            };
            let all_items = saved_translation_items(&analysis)?;
            let items = translatable_items(&all_items, replace_sfx);
            if !replace_sfx && items.is_empty() {
                emit_page_progress(
                    pending.page_number,
                    pending.total_pages,
                    "Preserving source page (no dialogue)...",
                );
                self.complete_preserved_page(&pending, &analysis).await?;
                auto_preserved_pages.push(pending.page_number);
                match self.workflow.next_pending_page(&pending.job_dir) {
                    Ok(Some(next)) => {
                        pending = next;
                        continue;
                    }
                    Ok(None) => {
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

    fn apply_strict_translations(
        analysis: &mut serde_json::Value,
        claim: &TranslationClaim,
        submissions: &[TranslationSubmission],
    ) -> Result<Vec<TypesetPayload>, FukidashiError> {
        let all_items = saved_translation_items(analysis)?;
        let items = translatable_items(&all_items, claim.replace_sfx);
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
                text.to_owned()
            };
            selected.push((id.to_owned(), text, keep_source, needs_review));
        }
        if seen != claim.item_ids {
            let missing = claim
                .item_ids
                .difference(&seen)
                .cloned()
                .collect::<Vec<_>>();
            return Err(FukidashiError::InvalidInput(format!(
                "translation submission is incomplete; missing stable ids: {}",
                missing.join(", ")
            )));
        }
        let bubble_bboxes = items
            .iter()
            .map(|item| {
                analysis_bubble_bbox(analysis, &item.id).map(|bbox| (item.id.clone(), bbox))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let item_values = analysis
            .get_mut("translation_handoff")
            .and_then(|handoff| handoff.get_mut("items"))
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| {
                FukidashiError::InvalidInput(
                    "analysis checkpoint has no mutable translation items".into(),
                )
            })?;
        let mut payloads = Vec::with_capacity(selected.len());
        for (id, text, keep_source, needs_review) in selected {
            let item = item_values
                .iter_mut()
                .find(|item| {
                    item.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str())
                })
                .ok_or_else(|| {
                    FukidashiError::InvalidInput(format!("translation id {id:?} disappeared"))
                })?;
            item["translation"] = serde_json::Value::String(text.clone());
            item["keep_source"] = serde_json::Value::Bool(keep_source);
            item["needs_review"] = serde_json::Value::Bool(needs_review);
            item["status"] = serde_json::Value::String(
                if needs_review {
                    "needs_review"
                } else if keep_source {
                    "keep_source"
                } else {
                    "translated"
                }
                .into(),
            );
            let bbox = serde_json::from_value::<Rect>(item["bbox"].clone())?.validate()?;
            let bubble_bbox = bubble_bboxes.get(&id).copied().flatten();
            payloads.push(TypesetPayload {
                id: Some(id.clone()),
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
                needs_review: Some(needs_review),
                // Model uncertainty is advisory. Only an explicit editor flag
                // or problem is an approval blocker.
                flagged: None,
                preserve_source: Some(keep_source),
                fallback_font_paths: Vec::new(),
                bbox,
                bubble_bbox,
                text_bbox: None,
                padding: None,
                text,
                font_path: None,
                min_font_size: None,
                max_font_size: None,
                text_color: item
                    .get("text_color")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                shape: None,
            });
        }
        if !claim.replace_sfx {
            for item in item_values.iter_mut() {
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
        analysis["translation_handoff"]["status"] = serde_json::Value::String("submitted".into());
        analysis["strict_v1"] = serde_json::json!({
            "status": "submitted",
            "sfx_mode": if claim.replace_sfx { "replace" } else { "preserve" },
            "preserved_count": if claim.replace_sfx {
                0
            } else {
                all_items
                    .iter()
                    .filter(|item| item.preserve_by_default)
                    .count()
            },
        });
        Ok(payloads)
    }
}

fn resolve_typeset_request(req: TypesetRequest) -> (String, Vec<TypesetPayload>, Vec<String>) {
    let bubbles = req
        .bubbles
        .into_iter()
        .map(|mut bubble| {
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

fn json_bbox(item: &serde_json::Value) -> Option<Rect> {
    serde_json::from_value::<Rect>(item.get("bbox").cloned()?)
        .ok()
        .and_then(|rect| rect.validate().ok())
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

fn checkpoint_item_is_translatable_dialogue(item: &serde_json::Value, replace_sfx: bool) -> bool {
    if checkpoint_item_is_preserved(item, replace_sfx) {
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
    !id.starts_with("text-") && kind != "unmatched_text"
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
    let mut preserved_bboxes = Vec::new();
    let mut translatable_bboxes = Vec::new();
    let mut preserved_ids = BTreeSet::new();
    for item in handoff_items {
        let Some(rect) = json_bbox(item) else {
            continue;
        };
        let id = item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if checkpoint_item_is_preserved(item, replace_sfx) {
            preserved_bboxes.push(rect);
            if !id.is_empty() {
                preserved_ids.insert(id.to_owned());
            }
        } else if checkpoint_item_is_translatable_dialogue(item, replace_sfx) {
            translatable_bboxes.push(rect);
        }
    }
    // Dialogue strokes must stay in the inpaint mask even when a nearby
    // preserved SFX/unmatched box overlaps them. Dropping those lines leaves
    // source glyphs under the typeset translation.
    if let Some(bubbles) = value.get("bubbles").and_then(serde_json::Value::as_array) {
        for bubble in bubbles {
            let Some(rect) = json_bbox(bubble) else {
                continue;
            };
            let id = bubble
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if preserved_ids.contains(id) {
                preserved_bboxes.push(rect);
            } else if !id.starts_with("text-") {
                translatable_bboxes.push(rect);
            }
        }
    }
    let lines = value
        .get("text_lines")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            FukidashiError::InvalidInput("analysis checkpoint has no text_lines array".into())
        })?;
    let mut regions = Vec::with_capacity(lines.len());
    for line in lines {
        let rect = serde_json::from_value::<Rect>(line.get("bbox").cloned().unwrap_or_default())
            .map_err(FukidashiError::from)
            .and_then(Rect::validate)?;
        let belongs_to_dialogue = translatable_bboxes
            .iter()
            .any(|bubble| rects_overlap(rect, *bubble));
        if belongs_to_dialogue {
            regions.push(rect);
            continue;
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
        if preserved_bboxes
            .iter()
            .any(|preserved| rects_overlap(rect, *preserved))
        {
            continue;
        }
        regions.push(rect);
    }
    Ok(regions)
}

#[tool_router]
impl FukidashiServer {
    #[tool(
        name = "fukidashi_get_lore",
        description = "Read the canonical lore context for a managed job. Call this before the first translation submit; an untouched job returns an empty valid template."
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
        name = "fukidashi_put_lore",
        description = "Validate and atomically write canonical lore context for a managed job. This stores client-authored names, pronouns, glossary, and forward-compatible fields; it never invokes an LLM."
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
        description = "Strict-v1 server-owned translation loop. Start from one source image or resume with job_id/job_path. The server analyzes or reuses the first unfinished page and returns compact stable translation items plus an opaque work_token. sfx_mode=preserve (default) keeps structurally unmatched text-* items out of translation, cleaning, and typesetting; preserve-only pages receive a verified pass-through stage and advance automatically; use replace only explicitly. Do not inspect managed job files or construct artifact paths; the submit call accepts only that token and the exact item IDs."
    )]
    pub async fn translation_start(
        &self,
        Parameters(req): Parameters<TranslationStartRequest>,
    ) -> CallToolResult {
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
                attach_page_progress(&mut value, page_number, total_pages, "Error");
                json_result(&value, true)
            }
        }
    }

    #[tool(
        name = "fukidashi_translation_submit",
        description = "Strict-v1 continuation. Submit exactly one translation decision for every stable ID returned by fukidashi_translation_start. The server owns analysis, cleaning, typesetting, stage reuse, model release, and page advancement; do not send image, analysis, clean, mask, font, or output paths. Use keep_source=true and/or needs_review=true for uncertain OCR instead of aborting the page."
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
        let payloads =
            match Self::apply_strict_translations(&mut analysis, &claim, &req.translations) {
                Ok(payloads) => payloads,
                Err(error) => return fail(self, error),
            };
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
            bubbles: payloads,
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
        let response = match self.workflow.next_pending_page(&claim.job_dir) {
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
                };
                self.prepare_strict_page(next, &next_request).await
            }
            Err(error) => Err(FukidashiError::InvalidInput(error.to_string())),
        };
        match response {
            Ok(mut value) => {
                value["completed_page"] = serde_json::json!(claim.page_number);
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
                attach_page_progress(
                    &mut value,
                    claim.page_number,
                    claim.total_pages,
                    "Page complete",
                );
                json_result(&value, true)
            }
        }
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
                    let source = req.source_language;
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
        description = "Remove text strokes with LaMa. mode=full preserves page inference; mode=crop requires mask_path, text_regions, or a full analysis_path and avoids detection/OCR. sfx_mode=preserve (default) excludes structurally unmatched text-* regions from analysis-derived cleaning; use replace explicitly."
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
                    return json_result(&serde_json::json!({"error":error.to_string()}), true);
                }
            }
        }
        if mode == "crop" && mask_path.is_none() && text_regions.is_empty() {
            return json_result(
                &serde_json::json!({"error":"crop cleaning requires mask_path, text_regions, or analysis_path"}),
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
            Err(e) => json_result(&serde_json::json!({"error":e.to_string()}), true),
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
        description = "Render supplied translations into speech bubbles using shaped glyph metrics. Use bundled Comic Neue for comic dialogue and Patrick Hand coverage for Vietnamese; do not pass generic Windows UI fonts such as Arial, Calibri, Segoe UI, Tahoma, Verdana, Times, or DejaVu Sans as a primary because the server substitutes Comic Neue and reports the substitution."
    )]
    pub async fn typeset(&self, Parameters(req): Parameters<TypesetRequest>) -> CallToolResult {
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
            let qa = crate::typeset::post_render_qa(&clean_artifact, &value)?;
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
            let value = serde_json::json!({
                "protocol": "review-v1",
                "status": "fixes_requested",
                "editor_url": url,
                "review": review,
                "feedback": review.get("feedback").cloned().unwrap_or(serde_json::Value::Array(Vec::new())),
                "next_action": "apply the returned feedback, serve the editor again, and call fukidashi_review_and_export",
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
    async fn export_approved_project(
        &self,
        project_dir: PathBuf,
        format: String,
        review: serde_json::Value,
        url: String,
    ) -> CallToolResult {
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
        name = "fukidashi_serve_editor",
        description = "Serve the local loopback comic editor. Reopen an existing managed job with job_path (directory or job.json; legacy .fukidashi-job.json is also accepted) or job_id; the server selects a verified render and reconstructs every page. image_path remains supported for a known rendered artifact; json_data is metadata only."
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
            crate::editor::serve_editor_with_allowed_sources(&image_path, state, allowed_sources)
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
                fukidashi_get_lore (or use the page_ready lore template) before the first submit and \
                fukidashi_put_lore for known names, pronouns, and glossary terms; flag unknown speakers \
                with needs_review=true. Lore is client-authored and this server never invokes an LLM. \
                fukidashi_translation_submit with only the returned work_token and one structured decision for \
                each required_translation_ids entry. The server owns page selection, analysis, clean, typeset, \
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
                Hand covers Vietnamese and Noto Sans Symbols 2 is reserved for symbols. Use \
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
        let preserved = translatable_items(&items, false);
        let replaced = translatable_items(&items, true);
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
                    "confidence": 0.91,
                    "bbox": {"x1": 20.0, "y1": 20.0, "x2": 60.0, "y2": 40.0}
                },
                {
                    "id": "line-low-conf-dialogue",
                    "confidence": 0.21,
                    "bbox": {"x1": 22.0, "y1": 42.0, "x2": 58.0, "y2": 55.0}
                },
                {
                    "id": "line-sfx",
                    "confidence": 0.96,
                    "bbox": {"x1": 82.0, "y1": 82.0, "x2": 110.0, "y2": 110.0}
                },
                {
                    "id": "line-noise",
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
                        &std::fs::read(&review_path).map_err(FukidashiError::Io)?,
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
}
