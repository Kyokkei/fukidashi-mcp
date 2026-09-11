//! A small authenticated loopback editor server.
//!
//! The server deliberately uses a fixed image and persistence path captured at
//! startup.  No request can select an arbitrary filesystem path.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::domain::{Rect, TypesetPayload};

const MAX_REQUEST: usize = 4 * 1024 * 1024;
const MAX_JSON: usize = 2 * 1024 * 1024;
// Keep the review UI embedded so the loopback editor has no CDN dependency.
const EDITOR_HTML: &str = include_str!("../assets/editor.html");

const REVIEW_FILE: &str = "review.json";
const REVIEW_AUDIT_FILE: &str = "review-audit.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ReviewState {
    review_session_id: String,
    revision: u64,
    status: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    feedback: Vec<Value>,
    #[serde(default)]
    approved_pages: Vec<usize>,
    #[serde(default)]
    consumed: bool,
    #[serde(default)]
    audit: Vec<Value>,
}

#[derive(Debug)]
struct ReviewChannel {
    state: Mutex<ReviewState>,
    notify: Notify,
    root_dir: PathBuf,
}

static REVIEW_CHANNELS: OnceLock<Mutex<std::collections::HashMap<String, Arc<ReviewChannel>>>> =
    OnceLock::new();

fn review_channels() -> &'static Mutex<std::collections::HashMap<String, Arc<ReviewChannel>>> {
    REVIEW_CHANNELS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn review_file(root: &Path) -> PathBuf {
    root.join(REVIEW_FILE)
}

fn review_audit_file(root: &Path) -> PathBuf {
    root.join(REVIEW_AUDIT_FILE)
}

#[derive(Clone)]
struct Session {
    token: String,
    host: String,
    image_path: PathBuf,
    root_dir: PathBuf,
    state_path: PathBuf,
    initial_state: Value,
    /// Exact source files approved by the managed manifest. These may live
    /// outside the jobs directory, while every other filesystem path remains
    /// rejected by the editor boundary.
    allowed_source_paths: Vec<PathBuf>,
    review: Arc<ReviewChannel>,
}

/// Start an editor on an ephemeral loopback port and return immediately.
pub fn serve_editor(image_path: &Path, json_data: Value) -> Result<Value> {
    serve_editor_with_allowed_sources(image_path, json_data, Vec::new())
}

pub fn serve_editor_with_allowed_sources(
    image_path: &Path,
    json_data: Value,
    allowed_source_paths: Vec<PathBuf>,
) -> Result<Value> {
    let image_path = fs::canonicalize(image_path)
        .with_context(|| format!("resolve editor image {}", image_path.display()))?;
    let parent = image_path
        .parent()
        .ok_or_else(|| anyhow!("editor image has no parent"))?;
    let root_dir = managed_editor_root(parent);
    let state_path = root_dir.join("project.json");
    let mut initial_state = normalize_editor_state(json_data)?;
    if state_path.exists()
        && let Ok(bytes) = fs::read(&state_path)
        && let Ok(saved) = serde_json::from_slice::<Value>(&bytes)
    {
        merge_saved_edits(&mut initial_state, &saved);
        let saved_bubbles = bubble_count(&saved);
        let merged_bubbles = bubble_count(&initial_state);
        if merged_bubbles < saved_bubbles {
            bail!(
                "refusing to overwrite managed project: reconstructed editor state lost {saved_bubbles} saved bubbles"
            );
        }
    }
    validate_state(&initial_state)?;
    validate_project_paths_for_root(&root_dir, &initial_state, &allowed_source_paths)?;
    atomic_json_save(&state_path, &initial_state)?;
    let review_path = review_file(&root_dir);
    let mut review_state = if review_path.exists() {
        let bytes = fs::read(&review_path).context("read existing review state")?;
        serde_json::from_slice::<ReviewState>(&bytes).context("parse existing review state")?
    } else {
        ReviewState {
            review_session_id: Uuid::new_v4().simple().to_string(),
            revision: 0,
            status: "new".to_owned(),
            action: None,
            feedback: Vec::new(),
            approved_pages: Vec::new(),
            consumed: false,
            audit: Vec::new(),
        }
    };
    review_state.revision = review_state.revision.saturating_add(1);
    review_state.status = "awaiting_review".to_owned();
    review_state.action = None;
    review_state.feedback.clear();
    review_state.approved_pages.clear();
    review_state.consumed = false;
    save_review(&root_dir, &review_state)?;
    let review = {
        let mut channels = review_channels()
            .lock()
            .map_err(|_| anyhow!("review channel lock poisoned"))?;
        if let Some(channel) = channels.get(&review_state.review_session_id) {
            *channel
                .state
                .lock()
                .map_err(|_| anyhow!("review state lock poisoned"))? = review_state.clone();
            channel.notify.notify_waiters();
            Arc::clone(channel)
        } else {
            let channel = Arc::new(ReviewChannel {
                state: Mutex::new(review_state.clone()),
                notify: Notify::new(),
                root_dir: root_dir.clone(),
            });
            channels.insert(review_state.review_session_id.clone(), Arc::clone(&channel));
            channel
        }
    };
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("bind loopback editor socket")?;
    listener.set_nonblocking(false)?;
    let port = listener.local_addr()?.port();
    let token = Uuid::new_v4().simple().to_string();
    let host = format!("127.0.0.1:{port}");
    let session = Arc::new(Session {
        token: token.clone(),
        host: host.clone(),
        image_path,
        root_dir,
        state_path: state_path.clone(),
        initial_state,
        allowed_source_paths,
        review,
    });
    let worker = Arc::clone(&session);
    thread::Builder::new()
        .name("fukidashi-editor".to_owned())
        .spawn(move || {
            for incoming in listener.incoming() {
                match incoming {
                    Ok(stream) => {
                        let _ = handle_connection(stream, &worker);
                    }
                    Err(_) => break,
                }
            }
        })
        .context("start editor server thread")?;
    Ok(json!({
        "url": format!("http://{host}/{token}/"),
        "persistence_path": state_path,
        "session_token": token,
        "review_session_id": review_state.review_session_id,
        "review_revision": review_state.revision,
        "review_path": review_path,
    }))
}

fn managed_editor_root(artifact_parent: &Path) -> PathBuf {
    let mut candidate = artifact_parent.to_path_buf();
    loop {
        if candidate.join("job.json").is_file() || candidate.join(".fukidashi-job.json").is_file() {
            return candidate;
        }
        let Some(parent) = candidate.parent() else {
            return artifact_parent.to_path_buf();
        };
        if parent == candidate {
            return artifact_parent.to_path_buf();
        }
        candidate = parent.to_path_buf();
    }
}

fn save_review(root: &Path, state: &ReviewState) -> Result<()> {
    let value = serde_json::to_value(state)?;
    atomic_json_save(&review_file(root), &value)?;
    atomic_json_save(
        &review_audit_file(root),
        &json!({
            "review_session_id": state.review_session_id,
            "revision": state.revision,
            "events": state.audit,
        }),
    )?;
    Ok(())
}

fn review_page_count(state: &Value) -> usize {
    state
        .get("pages")
        .and_then(Value::as_array)
        .map_or(1, Vec::len)
}

fn review_page(state: &Value, page: usize) -> Result<&Value> {
    state
        .get("pages")
        .and_then(Value::as_array)
        .and_then(|pages| pages.get(page))
        .ok_or_else(|| anyhow!("review page {page} is out of range"))
}

fn validate_review_feedback(session: &Session, state: &Value, item: &Value) -> Result<Value> {
    let object = item
        .as_object()
        .ok_or_else(|| anyhow!("review feedback must be an object"))?;
    let page = object
        .get("page")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("review feedback page is required"))?;
    let page = usize::try_from(page).map_err(|_| anyhow!("invalid review page"))?;
    let page_value = review_page(state, page)?;
    let source = page_image_path(session, state, page, "source", true)?;
    let (width, height) = image::image_dimensions(&source).context("read review page size")?;
    let origin = object
        .get("origin")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("review feedback origin is required"))?;
    if !matches!(origin, "image-pixels" | "bubble-flag") {
        bail!("review feedback origin must be image-pixels or bubble-flag");
    }
    let issue_type = object
        .get("issue_type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("review feedback issue_type is required"))?;
    if !matches!(
        issue_type,
        "leftover_source_text"
            | "text_overflow"
            | "wrong_translation"
            | "wrong_or_missing_bubble"
            | "damaged_artwork"
            | "font_or_layout"
            | "flagged_bubble"
            | "custom"
    ) {
        bail!("unsupported review issue type");
    }
    let note = object.get("note").and_then(Value::as_str).unwrap_or("");
    if note.len() > 4096 {
        bail!("review note is too long");
    }
    let corrected_text = object
        .get("corrected_text")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if corrected_text
        .as_deref()
        .is_some_and(|text| text.len() > 4096)
    {
        bail!("corrected review text is too long");
    }
    let source_ocr = object
        .get("source_ocr")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if source_ocr.as_deref().is_some_and(|text| text.len() > 4096) {
        bail!("source OCR review text is too long");
    }
    let current_translation = object
        .get("current_translation")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if current_translation
        .as_deref()
        .is_some_and(|text| text.len() > 4096)
    {
        bail!("current translation review text is too long");
    }
    let link_status = object
        .get("link_status")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if link_status
        .as_deref()
        .is_some_and(|status| status.len() > 128)
    {
        bail!("review link status is too long");
    }
    let bbox = match object.get("bbox") {
        Some(value) => {
            let rect: Rect = serde_json::from_value(value.clone())?;
            rect.validate()
                .map_err(|e| anyhow!("invalid review bbox: {e}"))?;
            if rect.x1 < 0.0 || rect.y1 < 0.0 || rect.x2 > width as f32 || rect.y2 > height as f32 {
                bail!("review bbox is outside page bounds");
            }
            Some(rect)
        }
        None => None,
    };
    let bubble_id = object
        .get("bubble_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(id) = bubble_id.as_deref() {
        let found = page_value
            .get("bubbles")
            .and_then(Value::as_array)
            .is_some_and(|bubbles| {
                bubbles
                    .iter()
                    .any(|bubble| bubble.get("id").and_then(Value::as_str) == Some(id))
            });
        if !found {
            bail!("review bubble_id is not present on the page");
        }
    }
    let mut artifact_paths = Vec::new();
    for key in [
        "image_path",
        "cleaned_image_path",
        "cleaned_path",
        "rendered_image_path",
    ] {
        if let Some(raw) = page_value.get(key).and_then(Value::as_str) {
            let path = safe_session_path(session, raw)?;
            artifact_paths.push(path.display().to_string());
        }
    }
    Ok(json!({
        "page": page,
        "bubble_id": bubble_id,
        "bbox": bbox,
        "issue_type": issue_type,
        "note": note,
        "corrected_text": corrected_text,
        "source_ocr": source_ocr,
        "current_translation": current_translation,
        "link_status": link_status,
        "origin": origin,
        "artifact_paths": artifact_paths,
    }))
}

fn validate_revision(request: &Value, current: &ReviewState) -> Result<()> {
    let revision = request
        .get("revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("review revision is required"))?;
    if revision != current.revision {
        bail!(
            "stale review revision {revision}; current revision is {}",
            current.revision
        );
    }
    Ok(())
}

fn save_review_draft(session: &Session, request: &Value) -> Result<Value> {
    let mut current = session
        .review
        .state
        .lock()
        .map_err(|_| anyhow!("review state lock poisoned"))?
        .clone();
    validate_revision(request, &current)?;
    let state = load_state(session)?;
    let feedback = request
        .get("feedback")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("review draft feedback must be an array"))?;
    if feedback.len() > 512 {
        bail!("review feedback has too many items");
    }
    let feedback = feedback
        .iter()
        .map(|item| validate_review_feedback(session, &state, item))
        .collect::<Result<Vec<_>>>()?;
    let approved_pages = request
        .get("approved_pages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("approved_pages must be an array"))?
        .iter()
        .map(|value| {
            let page = value
                .as_u64()
                .ok_or_else(|| anyhow!("approved page must be an integer"))?;
            let page = usize::try_from(page).map_err(|_| anyhow!("invalid approved page"))?;
            if page >= review_page_count(&state) {
                bail!("approved page is out of range");
            }
            Ok(page)
        })
        .collect::<Result<Vec<_>>>()?;
    current.feedback = feedback;
    current.approved_pages = approved_pages;
    current
        .audit
        .push(json!({"event":"draft_saved","revision":current.revision}));
    save_review(&session.root_dir, &current)?;
    *session
        .review
        .state
        .lock()
        .map_err(|_| anyhow!("review state lock poisoned"))? = current.clone();
    Ok(serde_json::to_value(current)?)
}

fn submit_review(session: &Session, request: &Value) -> Result<Value> {
    let mut current = session
        .review
        .state
        .lock()
        .map_err(|_| anyhow!("review state lock poisoned"))?
        .clone();
    validate_revision(request, &current)?;
    if current.action.is_some() || current.consumed {
        bail!("review action for this revision was already submitted");
    }
    let action = request
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("review action is required"))?;
    let project = load_state(session)?;
    match action {
        "request_fixes" => {
            let feedback = request
                .get("feedback")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("request_fixes feedback must be an array"))?;
            current.feedback = feedback
                .iter()
                .map(|item| validate_review_feedback(session, &project, item))
                .collect::<Result<Vec<_>>>()?;
            if current.feedback.is_empty() {
                bail!("request_fixes requires at least one feedback item");
            }
            current.status = "fixes_requested".to_owned();
        }
        "approve_export" => {
            // Approval is the operator's explicit override.  Keep collecting
            // these diagnostics for the audit trail, but do not turn advisory
            // flags, drawn issues, or a stale render marker into a second
            // approval lock.  The browser rerenders dirty pages before this
            // request; the export gate trusts this recorded human decision.
            let approved = request
                .get("approved_pages")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("approve_export approved_pages is required"))?;
            let mut pages = approved
                .iter()
                .map(|value| {
                    value
                        .as_u64()
                        .ok_or_else(|| anyhow!("approved page must be an integer"))
                        .and_then(|v| {
                            usize::try_from(v).map_err(|_| anyhow!("invalid approved page"))
                        })
                })
                .collect::<Result<Vec<_>>>()?;
            pages.sort_unstable();
            pages.dedup();
            if pages.len() != review_page_count(&project)
                || pages.iter().enumerate().any(|(index, page)| *page != index)
            {
                bail!("approve_export requires every page to be explicitly approved");
            }
            current.approved_pages = pages;
            current.status = "approved".to_owned();
        }
        _ => bail!("review action must be request_fixes or approve_export"),
    }
    current.action = Some(action.to_owned());
    current.consumed = false;
    current.audit.push(json!({
        "event": action,
        "revision": current.revision,
        "feedback_count": current.feedback.len(),
        "advisory_count": review_blockers(&project).len(),
    }));
    save_review(&session.root_dir, &current)?;
    *session
        .review
        .state
        .lock()
        .map_err(|_| anyhow!("review state lock poisoned"))? = current.clone();
    Ok(serde_json::to_value(current)?)
}

/// Wait for the one browser submission associated with a review revision.
pub async fn wait_for_review(
    review_session_id: &str,
    revision: u64,
    timeout_seconds: u64,
) -> Result<Value> {
    let channel = review_channels()
        .lock()
        .map_err(|_| anyhow!("review channel lock poisoned"))?
        .get(review_session_id)
        .cloned()
        .ok_or_else(|| anyhow!("review session is not active; serve the editor again to resume"))?;
    let timeout = Duration::from_secs(timeout_seconds.clamp(1, 24 * 60 * 60));
    tokio::time::timeout(timeout, async {
        loop {
            let notified = channel.notify.notified();
            let result = {
                let mut state = channel
                    .state
                    .lock()
                    .map_err(|_| anyhow!("review state lock poisoned"))?;
                if state.revision != revision {
                    bail!(
                        "stale review revision {revision}; current revision is {}",
                        state.revision
                    );
                }
                if state.action.is_some() {
                    if state.consumed {
                        bail!("review action for this revision was already consumed");
                    }
                    state.consumed = true;
                    state.status = "consumed".to_owned();
                    state
                        .audit
                        .push(json!({"event":"waiter_consumed","revision":revision}));
                    save_review(&channel.root_dir, &state)?;
                    Some(serde_json::json!({
                        "review_session_id": state.review_session_id,
                        "revision": state.revision,
                        "action": state.action,
                        "feedback": state.feedback,
                        "approved_pages": state.approved_pages,
                        "artifact_paths": [channel.root_dir.display().to_string()],
                        "review_path": review_file(&channel.root_dir),
                    }))
                } else {
                    None
                }
            };
            if let Some(result) = result {
                return Ok(result);
            }
            notified.await;
        }
    })
    .await
    .map_err(|_| anyhow!("review wait timed out"))?
}

/// Export may proceed only after the latest review revision was explicitly approved.
/// Review findings remain advisory once the operator has submitted approval.
pub fn export_gate(project_dir: &Path) -> Result<()> {
    let path = review_file(project_dir);
    if !path.exists() {
        return Ok(());
    }
    let state: ReviewState = serde_json::from_slice(&fs::read(&path)?)?;
    let approved = state.action.as_deref() == Some("approve_export")
        && matches!(state.status.as_str(), "approved" | "consumed");
    if !approved {
        bail!("export is blocked until the latest review revision is explicitly approved");
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, session: &Session) -> Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut bytes = Vec::new();
    let mut buf = [0_u8; 8192];
    while bytes.len() < MAX_REQUEST {
        let count = stream.read(&mut buf)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..count]);
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("incomplete HTTP headers"))?;
    let header = std::str::from_utf8(&bytes[..header_end]).context("HTTP headers are not UTF-8")?;
    let mut lines = header.lines();
    let request = lines
        .next()
        .ok_or_else(|| anyhow!("missing HTTP request line"))?;
    let mut request_parts = request.split_whitespace();
    let method = request_parts.next().unwrap_or("");
    let path = request_parts.next().unwrap_or("");
    let mut host = None;
    let mut origin = None;
    let mut content_length = 0usize;
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim().to_ascii_lowercase().as_str() {
                "host" => host = Some(value.trim().to_owned()),
                "origin" => origin = Some(value.trim().to_owned()),
                "content-length" => {
                    content_length = value.trim().parse().context("invalid content length")?
                }
                _ => {}
            }
        }
    }
    if host.as_deref() != Some(session.host.as_str())
        || origin
            .as_deref()
            .is_some_and(|v| v != format!("http://{}", session.host))
    {
        return respond(&mut stream, 403, "text/plain; charset=utf-8", b"forbidden");
    }
    if !path.starts_with(&format!("/{}/", session.token)) {
        return respond(&mut stream, 404, "text/plain", b"not found");
    }
    if content_length > MAX_JSON {
        return respond(&mut stream, 413, "text/plain", b"request too large");
    }
    let body_start = header_end + 4;
    let mut body = bytes[body_start..].to_vec();
    while body.len() < content_length {
        let count = stream.read(&mut buf)?;
        if count == 0 {
            break;
        }
        body.extend_from_slice(&buf[..count]);
        if body.len() > MAX_JSON {
            return respond(&mut stream, 413, "text/plain", b"request too large");
        }
    }
    let relative = &path[session.token.len() + 2..];
    let (relative, query) = relative.split_once('?').unwrap_or((relative, ""));
    match (method, relative) {
        ("GET", "") => respond(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            EDITOR_HTML.as_bytes(),
        ),
        ("GET", "state") => {
            let data = serde_json::to_vec(&load_state(session)?)?;
            respond(&mut stream, 200, "application/json; charset=utf-8", &data)
        }
        ("GET", "review/state") => {
            let review = session
                .review
                .state
                .lock()
                .map_err(|_| anyhow!("review state lock poisoned"))?
                .clone();
            let data = serde_json::to_vec(&review)?;
            respond(&mut stream, 200, "application/json; charset=utf-8", &data)
        }
        ("GET", "image") => {
            let data = fs::read(&session.image_path).context("read editor image")?;
            let kind = image::guess_format(&data)
                .map(|f| f.to_mime_type())
                .unwrap_or("application/octet-stream");
            respond(&mut stream, 200, kind, &data)
        }
        ("GET", route) if route.starts_with("page/") && route.ends_with("/image") => {
            let index = match parse_page_index(route, "/image") {
                Ok(index) => index,
                Err(_) => return respond(&mut stream, 400, "text/plain", b"invalid page index"),
            };
            let state = match load_state(session) {
                Ok(state) => state,
                Err(_) => return respond(&mut stream, 500, "text/plain", b"unable to load state"),
            };
            let variant = query_value(query, "variant").unwrap_or("source");
            let path = match page_image_path(session, &state, index, variant, true) {
                Ok(path) => path,
                Err(_) => {
                    return respond(&mut stream, 404, "text/plain", b"page image unavailable");
                }
            };
            let data = match fs::read(&path) {
                Ok(data) => data,
                Err(_) => {
                    return respond(&mut stream, 404, "text/plain", b"page image unavailable");
                }
            };
            let kind = image::guess_format(&data)
                .map(|f| f.to_mime_type())
                .unwrap_or("application/octet-stream");
            respond(&mut stream, 200, kind, &data)
        }
        ("POST", "save") => {
            if body.len() != content_length {
                return respond(&mut stream, 400, "text/plain", b"incomplete body");
            }
            let value: Value = match serde_json::from_slice(&body) {
                Ok(value) => value,
                Err(_) => return respond(&mut stream, 400, "text/plain", b"invalid editor JSON"),
            };
            let value = match normalize_editor_state(value) {
                Ok(value) => value,
                Err(_) => return respond(&mut stream, 400, "text/plain", b"invalid editor state"),
            };
            let current = match load_state(session) {
                Ok(state) => state,
                Err(_) => {
                    return respond(
                        &mut stream,
                        500,
                        "text/plain",
                        b"unable to load editor state",
                    );
                }
            };
            if !state_revision_matches(&current, &value) {
                return respond(
                    &mut stream,
                    409,
                    "text/plain; charset=utf-8",
                    b"Project changed on disk; reload editor before saving",
                );
            }
            let mut value = value;
            mark_render_dirty_changes(&current, &mut value);
            if validate_state(&value).is_err() || validate_project_paths(session, &value).is_err() {
                return respond(&mut stream, 400, "text/plain", b"invalid editor state");
            }
            let next_revision = state_revision(&current).saturating_add(1);
            set_state_revision(&mut value, next_revision);
            if atomic_json_save(&session.state_path, &value).is_err() {
                return respond(
                    &mut stream,
                    500,
                    "text/plain",
                    b"unable to save editor state",
                );
            }
            let manifest_path = translation_manifest_path(&session.state_path);
            if atomic_json_save(&manifest_path, &translation_manifest(&value)).is_err() {
                return respond(
                    &mut stream,
                    500,
                    "text/plain",
                    b"unable to save translations",
                );
            }
            respond(
                &mut stream,
                200,
                "application/json; charset=utf-8",
                serde_json::to_vec(&json!({
                    "saved": true,
                    "state_revision": next_revision,
                    "persistence_path": session.state_path,
                    "translation_manifest": manifest_path,
                }))?
                .as_slice(),
            )
        }
        ("POST", "review/draft") => {
            if body.len() != content_length {
                return respond(&mut stream, 400, "text/plain", b"incomplete body");
            }
            let request: Value = match serde_json::from_slice(&body) {
                Ok(value) => value,
                Err(_) => return respond(&mut stream, 400, "text/plain", b"invalid review JSON"),
            };
            let result = save_review_draft(session, &request);
            match result {
                Ok(review) => respond(
                    &mut stream,
                    200,
                    "application/json; charset=utf-8",
                    &serde_json::to_vec(&review)?,
                ),
                Err(error) => respond(&mut stream, 400, "text/plain", error.to_string().as_bytes()),
            }
        }
        ("POST", "review/submit") => {
            if body.len() != content_length {
                return respond(&mut stream, 400, "text/plain", b"incomplete body");
            }
            let request: Value = match serde_json::from_slice(&body) {
                Ok(value) => value,
                Err(_) => return respond(&mut stream, 400, "text/plain", b"invalid review JSON"),
            };
            let result = submit_review(session, &request);
            match result {
                Ok(review) => {
                    session.review.notify.notify_waiters();
                    respond(
                        &mut stream,
                        200,
                        "application/json; charset=utf-8",
                        &serde_json::to_vec(&review)?,
                    )
                }
                Err(error) => respond(&mut stream, 400, "text/plain", error.to_string().as_bytes()),
            }
        }
        ("POST", "render") => {
            if body.len() != content_length {
                return respond(&mut stream, 400, "text/plain", b"incomplete body");
            }
            let request: Value = match serde_json::from_slice(&body) {
                Ok(value) => value,
                Err(_) => return respond(&mut stream, 400, "text/plain", b"invalid render JSON"),
            };
            let state = match request.get("state").cloned() {
                Some(state) => state,
                None => return respond(&mut stream, 400, "text/plain", b"render requires state"),
            };
            let state = match normalize_editor_state(state) {
                Ok(state) => state,
                Err(_) => return respond(&mut stream, 400, "text/plain", b"invalid editor state"),
            };
            let current = match load_state(session) {
                Ok(state) => state,
                Err(_) => {
                    return respond(
                        &mut stream,
                        500,
                        "text/plain",
                        b"unable to load editor state",
                    );
                }
            };
            if !state_revision_matches(&current, &state) {
                return respond(
                    &mut stream,
                    409,
                    "text/plain; charset=utf-8",
                    b"Project changed on disk; reload editor before rendering",
                );
            }
            let index = match request.get("page_index").and_then(Value::as_u64) {
                Some(index) => index,
                None => {
                    return respond(
                        &mut stream,
                        400,
                        "text/plain",
                        b"render requires page_index",
                    );
                }
            };
            let index = match usize::try_from(index) {
                Ok(index) => index,
                Err(_) => return respond(&mut stream, 400, "text/plain", b"invalid page index"),
            };
            if validate_state(&state).is_err() || validate_project_paths(session, &state).is_err() {
                return respond(&mut stream, 400, "text/plain", b"invalid editor state");
            }
            let result = match render_page(session, &state, index) {
                Ok(result) => result,
                Err(error) => {
                    let message = format!("unable to render page: {error}");
                    return respond(&mut stream, 400, "text/plain", message.as_bytes());
                }
            };
            respond(
                &mut stream,
                200,
                "application/json; charset=utf-8",
                &serde_json::to_vec(&result)?,
            )
        }
        _ => respond(&mut stream, 404, "text/plain", b"not found"),
    }
}

fn load_state(session: &Session) -> Result<Value> {
    if session.state_path.exists() {
        let bytes = fs::read(&session.state_path).context("read editor state")?;
        normalize_editor_state(serde_json::from_slice(&bytes).context("parse editor state")?)
    } else {
        normalize_editor_state(session.initial_state.clone())
    }
}

fn state_revision(state: &Value) -> u64 {
    state
        .get("state_revision")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn set_state_revision(state: &mut Value, revision: u64) {
    if let Some(object) = state.as_object_mut() {
        object.insert("state_revision".to_owned(), Value::from(revision));
    }
}

fn state_revision_matches(current: &Value, incoming: &Value) -> bool {
    incoming
        .get("state_revision")
        .and_then(Value::as_u64)
        .map_or(state_revision(current) == 0, |revision| {
            revision == state_revision(current)
        })
}

fn bubble_count(state: &Value) -> usize {
    state
        .get("pages")
        .and_then(Value::as_array)
        .map(|pages| {
            pages
                .iter()
                .map(|page| {
                    page.get("bubbles")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                })
                .sum()
        })
        .unwrap_or(0)
}

fn removed_bubble_id(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("id").and_then(Value::as_str))
}

fn page_render_signature(page: &Value) -> Value {
    fn without_review_metadata(value: &Value) -> Value {
        match value {
            Value::Array(values) => Value::Array(
                values
                    .iter()
                    .map(without_review_metadata)
                    .collect::<Vec<_>>(),
            ),
            Value::Object(object) => Value::Object(
                object
                    .iter()
                    .filter(|(key, value)| {
                        !matches!(
                            key.as_str(),
                            "flagged"
                                | "problem"
                                | "needs_review"
                                | "flag_reason"
                                | "problem_reason"
                                | "removed_reason"
                        ) && !value.is_null()
                    })
                    .map(|(key, value)| (key.clone(), without_review_metadata(value)))
                    .collect(),
            ),
            _ => value.clone(),
        }
    }

    json!({
        "bubbles": page
            .get("bubbles")
            .map(without_review_metadata)
            .unwrap_or_else(|| Value::Array(Vec::new())),
        "removed_bubbles": page
            .get("removed_bubbles")
            .map(without_review_metadata)
            .unwrap_or_else(|| Value::Array(Vec::new())),
        "correction_strokes": page
            .get("correction_strokes")
            .map(without_review_metadata)
            .unwrap_or_else(|| Value::Array(Vec::new())),
    })
}

fn mark_render_dirty_changes(current: &Value, incoming: &mut Value) {
    let Some(current_pages) = current.get("pages").and_then(Value::as_array) else {
        return;
    };
    let Some(incoming_pages) = incoming.get_mut("pages").and_then(Value::as_array_mut) else {
        return;
    };
    for (index, incoming_page) in incoming_pages.iter_mut().enumerate() {
        let incoming_signature = page_render_signature(incoming_page);
        let Some(incoming_object) = incoming_page.as_object_mut() else {
            continue;
        };
        let incoming_id = incoming_object.get("id").and_then(Value::as_str);
        let current_page = current_pages
            .iter()
            .find(|candidate| {
                candidate
                    .get("id")
                    .and_then(Value::as_str)
                    .zip(incoming_id)
                    .is_some_and(|(left, right)| left == right)
            })
            .or_else(|| current_pages.get(index));
        let Some(current_page) = current_page else {
            continue;
        };
        if page_render_signature(current_page) != incoming_signature
            || current_page
                .get("render_dirty")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            incoming_object.insert("render_dirty".to_owned(), Value::Bool(true));
        }
    }
}

fn review_blockers(state: &Value) -> Vec<Value> {
    state
        .get("pages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .flat_map(|(page_index, page)| {
            let mut blockers = Vec::new();
            if page
                .get("render_dirty")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                blockers.push(json!({
                    "page": page_index,
                    "kind": "render_dirty",
                    "origin": "server-state",
                }));
            }
            if let Some(issues) = page.get("issues").and_then(Value::as_array) {
                blockers.extend(issues.iter().enumerate().map(|(issue_index, issue)| {
                    json!({
                        "page": page_index,
                        "kind": "issue",
                        "issue_index": issue_index,
                        "issue_type": issue.get("issue_type").cloned().unwrap_or(Value::String("custom".into())),
                        "origin": issue.get("origin").cloned().unwrap_or(Value::String("image-pixels".into())),
                    })
                }));
            }
            if let Some(bubbles) = page.get("bubbles").and_then(Value::as_array) {
                blockers.extend(
                    bubbles
                        .iter()
                        .filter(|bubble| {
                            bubble
                                .get("flagged")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                                || bubble
                                    .get("problem")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                        })
                        .map(|bubble| {
                            let explicit_flag = bubble
                                .get("flagged")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                                || bubble
                                    .get("problem")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false);
                            let source_ocr = bubble
                                .get("source_text")
                                .or_else(|| bubble.get("original_text"))
                                .or_else(|| bubble.get("text"))
                                .cloned()
                                .unwrap_or(Value::Null);
                            let current_translation = bubble
                                .get("translation")
                                .cloned()
                                .unwrap_or_else(|| Value::String(String::new()));
                            json!({
                                "page": page_index,
                                "kind": "flagged_bubble",
                                "bubble_id": bubble.get("id").cloned().unwrap_or(Value::Null),
                                "bbox": bubble.get("bbox").cloned().unwrap_or(Value::Null),
                                "source_ocr": source_ocr,
                                "current_translation": current_translation,
                                "origin": if explicit_flag { "bubble-flag" } else { "bubble-problem" },
                            })
                        }),
                );
            }
            blockers
        })
        .collect()
}

fn parse_page_index(route: &str, suffix: &str) -> Result<usize> {
    let prefix = "page/";
    let index = route
        .strip_prefix(prefix)
        .and_then(|value| value.strip_suffix(suffix))
        .ok_or_else(|| anyhow!("invalid page route"))?;
    index.parse().context("invalid page index")
}

fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|part| {
        let (name, value) = part.split_once('=')?;
        (name == key).then_some(value)
    })
}

fn page_image_path(
    session: &Session,
    state: &Value,
    index: usize,
    variant: &str,
    require_existing: bool,
) -> Result<PathBuf> {
    let page = match state.get("pages").and_then(Value::as_array) {
        Some(pages) => {
            if pages.is_empty() {
                None
            } else {
                Some(
                    pages
                        .get(index)
                        .ok_or_else(|| anyhow!("page index is out of range"))?,
                )
            }
        }
        None => None,
    };
    let raw = match variant {
        "source" => page
            .and_then(|page| page.get("image_path"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| session.image_path.display().to_string()),
        "cleaned" => page
            .and_then(|page| {
                page.get("cleaned_image_path")
                    .or_else(|| page.get("cleaned_path"))
            })
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("page has no cleaned image"))?
            .to_owned(),
        "rendered" => page
            .and_then(|page| page.get("rendered_image_path"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("page has no rendered image"))?
            .to_owned(),
        _ => bail!("unsupported image variant {variant:?}"),
    };
    let resolved = safe_session_path(session, &raw)?;
    if require_existing && !resolved.is_file() {
        bail!("page image does not exist: {}", resolved.display());
    }
    Ok(resolved)
}

fn safe_project_path(root: &Path, raw: &str) -> Result<PathBuf> {
    safe_project_path_with_allowlist(root, raw, &[])
}

fn safe_font_path(root: &Path, raw: &str) -> Result<PathBuf> {
    let input = PathBuf::from(raw);
    if input.is_file()
        && let Some(ext) = input.extension().and_then(|e| e.to_str())
    {
        let ext = ext.to_ascii_lowercase();
        if matches!(ext.as_str(), "ttf" | "otf" | "ttc" | "woff" | "woff2") {
            return Ok(input);
        }
    }
    safe_project_path(root, raw)
}

fn safe_project_path_with_allowlist(
    root: &Path,
    raw: &str,
    allowed_external: &[PathBuf],
) -> Result<PathBuf> {
    let input = PathBuf::from(raw);
    let candidate = if input.is_absolute() {
        input
    } else {
        root.join(input)
    };
    let resolved = if candidate.exists() {
        fs::canonicalize(&candidate)?
    } else {
        candidate
    };
    if !crate::workflow::path_is_within_public(&resolved, root)?
        && !allowed_external.iter().any(|allowed| {
            fs::canonicalize(allowed)
                .ok()
                .is_some_and(|allowed| allowed == resolved)
        })
    {
        bail!("editor path escapes the project directory");
    }
    Ok(resolved)
}

fn safe_session_path(session: &Session, raw: &str) -> Result<PathBuf> {
    safe_project_path_with_allowlist(&session.root_dir, raw, &session.allowed_source_paths)
}

fn validate_project_paths(session: &Session, state: &Value) -> Result<()> {
    validate_project_paths_for_root(&session.root_dir, state, &session.allowed_source_paths)
}

fn validate_project_paths_for_root(
    root: &Path,
    state: &Value,
    allowed_external: &[PathBuf],
) -> Result<()> {
    if let Some(pages) = state.get("pages").and_then(Value::as_array) {
        for page in pages {
            let object = page
                .as_object()
                .ok_or_else(|| anyhow!("project page must be an object"))?;
            let image_path = object
                .get("image_path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("project page image_path is required"))?;
            safe_project_path_with_allowlist(root, image_path, allowed_external)?;
            for key in ["cleaned_image_path", "cleaned_path", "rendered_image_path"] {
                if let Some(path) = object.get(key).and_then(Value::as_str) {
                    safe_project_path_with_allowlist(root, path, allowed_external)?;
                }
            }
        }
    }
    Ok(())
}

/// Normalize the small, browser-editable portion of the project schema at the
/// server boundary. The model can omit legacy arrays, but it cannot replace
/// the server-owned page/artifact paths with an arbitrary shape.
fn normalize_editor_state(mut state: Value) -> Result<Value> {
    let object = state
        .as_object_mut()
        .ok_or_else(|| anyhow!("editor state must be an object"))?;
    let revision = object
        .entry("state_revision")
        .or_insert_with(|| Value::from(0_u64));
    if !revision.is_u64() {
        bail!("editor state_revision must be a non-negative integer");
    }
    let pages = object
        .entry("pages")
        .or_insert_with(|| Value::Array(Vec::new()));
    let pages = pages
        .as_array_mut()
        .ok_or_else(|| anyhow!("editor pages must be an array"))?;
    for (index, page) in pages.iter_mut().enumerate() {
        let page = page
            .as_object_mut()
            .ok_or_else(|| anyhow!("editor page must be an object"))?;
        page.entry("id")
            .or_insert_with(|| Value::String(format!("page-{}", index + 1)));
        for key in ["bubbles", "issues", "correction_strokes", "removed_bubbles"] {
            page.entry(key).or_insert_with(|| Value::Array(Vec::new()));
            if !page.get(key).is_some_and(Value::is_array) {
                bail!("editor page {key} must be an array");
            }
        }
        page.entry("render_dirty")
            .or_insert_with(|| Value::Bool(false));
        if !page.get("render_dirty").is_some_and(Value::is_boolean) {
            bail!("editor page render_dirty must be a boolean");
        }
        page.entry("rendered_state_revision")
            .or_insert_with(|| Value::from(0_u64));
        if !page
            .get("rendered_state_revision")
            .is_some_and(Value::is_u64)
        {
            bail!("editor page rendered_state_revision must be a non-negative integer");
        }
    }
    Ok(state)
}

fn merge_saved_edits(base: &mut Value, saved: &Value) {
    if let Some(revision) = saved.get("state_revision").and_then(Value::as_u64) {
        set_state_revision(base, revision);
    }
    let Some(base_pages) = base.get_mut("pages").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(saved_pages) = saved.get("pages").and_then(Value::as_array) else {
        return;
    };
    for (index, base_page) in base_pages.iter_mut().enumerate() {
        let baseline_signature = page_render_signature(base_page);
        let base_id = base_page
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let saved_page = saved_pages
            .iter()
            .find(|candidate| {
                candidate
                    .get("id")
                    .and_then(Value::as_str)
                    .zip(base_id.as_deref())
                    .is_some_and(|(left, right)| left == right)
            })
            .or_else(|| saved_pages.get(index));
        let Some(saved_object) = saved_page.and_then(Value::as_object) else {
            continue;
        };
        let saved_removed_bubbles = saved_object
            .get("removed_bubbles")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let saved_bubbles = saved_object
            .get("bubbles")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        {
            let Some(base_object) = base_page.as_object_mut() else {
                continue;
            };
            if let Some(issues) = saved_object.get("issues").filter(|value| value.is_array()) {
                base_object.insert("issues".to_owned(), issues.clone());
            }
            if let Some(strokes) = saved_object
                .get("correction_strokes")
                .filter(|value| value.is_array())
            {
                base_object.insert("correction_strokes".to_owned(), strokes.clone());
            }
            if let Some(rendered_revision) = saved_object
                .get("rendered_state_revision")
                .filter(|value| value.is_u64())
            {
                base_object.insert(
                    "rendered_state_revision".to_owned(),
                    rendered_revision.clone(),
                );
            }
            let removed_ids: std::collections::HashSet<String> = saved_removed_bubbles
                .iter()
                .filter_map(removed_bubble_id)
                .map(str::to_owned)
                .collect();
            base_object.insert(
                "removed_bubbles".to_owned(),
                Value::Array(saved_removed_bubbles.clone()),
            );
            if let Some(base_bubbles) = base_object.get_mut("bubbles").and_then(Value::as_array_mut)
            {
                base_bubbles.retain(|bubble| {
                    bubble
                        .get("id")
                        .and_then(Value::as_str)
                        .is_none_or(|id| !removed_ids.contains(id))
                });
                for base_bubble in base_bubbles.iter_mut() {
                    let Some(base_bubble_object) = base_bubble.as_object_mut() else {
                        continue;
                    };
                    let id = base_bubble_object
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let saved_bubble = saved_bubbles.iter().find(|candidate| {
                        candidate
                            .get("id")
                            .and_then(Value::as_str)
                            .zip(id.as_deref())
                            .is_some_and(|(left, right)| left == right)
                    });
                    let Some(saved_bubble) = saved_bubble.and_then(Value::as_object) else {
                        continue;
                    };
                    for key in [
                        "translation",
                        "bbox",
                        "bubble_bbox",
                        "text_bbox",
                        "font_size",
                        "padding",
                        "reading_order",
                        "flagged",
                        "problem",
                        "flag_reason",
                        "problem_reason",
                        "needs_review",
                        "text_color",
                    ] {
                        if let Some(value) = saved_bubble.get(key) {
                            base_bubble_object.insert(key.to_owned(), value.clone());
                        }
                    }
                }
                let base_ids: std::collections::HashSet<String> = base_bubbles
                    .iter()
                    .filter_map(|bubble| {
                        bubble.get("id").and_then(Value::as_str).map(str::to_owned)
                    })
                    .collect();
                for saved_bubble in saved_bubbles {
                    let Some(id) = saved_bubble.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    if !removed_ids.contains(id)
                        && !base_ids.contains(id)
                        && saved_bubble.get("bbox").is_some()
                    {
                        base_bubbles.push(saved_bubble);
                    }
                }
            }
        }
        let render_dirty = page_render_signature(base_page) != baseline_signature;
        if let Some(base_object) = base_page.as_object_mut() {
            base_object.insert("render_dirty".to_owned(), Value::Bool(render_dirty));
        }
    }
}

fn translation_manifest_path(state_path: &Path) -> PathBuf {
    state_path.with_file_name("translations.json")
}

fn translation_manifest(state: &Value) -> Value {
    let pages = state
        .get("pages")
        .and_then(Value::as_array)
        .map(|pages| {
            pages
                .iter()
                .map(|page| {
                    json!({
                        "id": page.get("id").and_then(Value::as_str).unwrap_or(""),
                        "image_path": page.get("image_path").and_then(Value::as_str).unwrap_or(""),
                        "removed_bubbles": page.get("removed_bubbles").cloned().unwrap_or_else(|| Value::Array(Vec::new())),
                        "render_dirty": page.get("render_dirty").cloned().unwrap_or(Value::Bool(false)),
                        "bubbles": page.get("bubbles").and_then(Value::as_array).map(|bubbles| bubbles.iter().map(|bubble| json!({
                            "id": bubble.get("id").and_then(Value::as_str).unwrap_or(""),
                            "translation": bubble.get("translation").cloned().unwrap_or(Value::Null),
                            "reading_order": bubble.get("reading_order").cloned().unwrap_or(json!(0)),
                        })).collect::<Vec<_>>()).unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({"schema_version": 1, "pages": pages})
}

fn bubble_preserve_for_render(bubble: &Value) -> bool {
    let text_is_empty = bubble
        .get("translation")
        .and_then(Value::as_str)
        .is_none_or(|text| text.trim().is_empty());
    if text_is_empty {
        return true;
    }
    if let Some(explicit) = bubble.get("preserve_source").and_then(Value::as_bool) {
        return explicit;
    }
    if bubble
        .get("keep_source")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    bubble
        .get("preserve_by_default")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || bubble
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "unmatched_text")
        || bubble
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with("text-"))
}

fn rects_match(left: Rect, right: Rect) -> bool {
    const EPSILON: f32 = 0.001;
    (left.x1 - right.x1).abs() <= EPSILON
        && (left.y1 - right.y1).abs() <= EPSILON
        && (left.x2 - right.x2).abs() <= EPSILON
        && (left.y2 - right.y2).abs() <= EPSILON
}

const DEFAULT_EDITOR_MIN_FONT: f32 = 8.0;
const DEFAULT_EDITOR_MAX_FONT: f32 = 72.0;

fn finite_font_size(value: Option<f32>) -> Option<f32> {
    value.filter(|size| size.is_finite() && *size >= 0.5 && *size <= 512.0)
}

/// Inspector `font_size` is a preferred cap, not a locked floor.
/// Using the last fitted size as both min and max made any drag/resize
/// overflow instead of shrinking to the new box.
fn editor_font_size_range(bubble: &Value) -> (f32, f32) {
    let font_size = finite_font_size(
        bubble
            .get("font_size")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
    );
    let explicit_min = finite_font_size(
        bubble
            .get("min_font_size")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
    );
    let explicit_max = finite_font_size(
        bubble
            .get("max_font_size")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
    );
    let max_font_size = font_size
        .or(explicit_max)
        .unwrap_or(DEFAULT_EDITOR_MAX_FONT);
    let mut min_font_size = explicit_min.unwrap_or(DEFAULT_EDITOR_MIN_FONT);
    if min_font_size > max_font_size {
        min_font_size = max_font_size;
    }
    (min_font_size, max_font_size)
}

/// A browser geometry edit copies `bbox` into `bubble_bbox`. That gives the
/// render boundary an explicit marker that any detector-era text anchor is
/// stale; legacy state with a distinct detector box keeps its old anchor.
fn editor_text_bbox(bubble: &Value, bbox: Rect) -> Result<Option<Rect>> {
    let operator_geometry = bubble
        .get("bubble_bbox")
        .cloned()
        .and_then(|value| serde_json::from_value::<Rect>(value).ok())
        .is_some_and(|bubble_bbox| rects_match(bubble_bbox, bbox));
    if operator_geometry {
        return Ok(None);
    }
    bubble
        .get("text_bbox")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(Into::into)
}

fn synchronize_rendered_bubbles(page: &mut Value) {
    let Some(bubbles) = page.get_mut("bubbles").and_then(Value::as_array_mut) else {
        return;
    };
    for bubble in bubbles {
        let Some(object) = bubble.as_object_mut() else {
            continue;
        };
        if let Some(bbox) = object.get("bbox").cloned() {
            object.insert("bubble_bbox".to_owned(), bbox);
        }
    }
}

fn render_page(session: &Session, state: &Value, index: usize) -> Result<Value> {
    let cleaned = page_image_path(session, state, index, "cleaned", true)?;
    let source_image = page_image_path(session, state, index, "source", true)?;
    let jobs_root = session
        .root_dir
        .parent()
        .ok_or_else(|| anyhow!("editor job has no jobs root"))?;
    let workflow = crate::workflow::Workflow::new(jobs_root.to_path_buf())?;
    let _render_lock = workflow.acquire_render_lock(&session.root_dir)?;
    let base_clean = workflow.validate_clean_input(&cleaned)?;
    let page = state
        .get("pages")
        .and_then(Value::as_array)
        .and_then(|pages| pages.get(index));
    let bubbles = page
        .and_then(|page| page.get("bubbles"))
        .or_else(|| state.get("bubbles"))
        .and_then(Value::as_array)
        .map_or(&[][..], |bubbles| bubbles.as_slice());
    let removed_bubbles = page
        .and_then(|page| page.get("removed_bubbles"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let strokes = page
        .and_then(|page| page.get("correction_strokes"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let removed_ids: std::collections::HashSet<String> = removed_bubbles
        .iter()
        .filter_map(removed_bubble_id)
        .map(str::to_owned)
        .collect();
    let preserved_bubbles = bubbles
        .iter()
        .filter(|bubble| {
            bubble
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(|id| !removed_ids.contains(id))
        })
        .filter(|bubble| bubble_preserve_for_render(bubble))
        .cloned()
        .collect::<Vec<_>>();
    if !strokes.is_empty() && bubbles.is_empty() {
        bail!(
            "legacy page is missing saved typeset data; import or recover its bubbles before applying brush and rendering"
        );
    }
    let (source, corrected_cleaned) = if strokes.is_empty()
        && removed_bubbles.is_empty()
        && preserved_bubbles.is_empty()
    {
        (cleaned.clone(), None)
    } else {
        let mut corrected = if strokes.is_empty() {
            image::open(&cleaned)
                .with_context(|| format!("read cleaned image {}", cleaned.display()))?
                .to_rgb8()
        } else {
            apply_correction_strokes(&cleaned, &strokes)?
        };
        if !removed_bubbles.is_empty() || !preserved_bubbles.is_empty() {
            let original = image::open(&source_image)
                .with_context(|| format!("read source image {}", source_image.display()))?
                .to_rgb8();
            let mut source_preservations = removed_bubbles.clone();
            source_preservations.extend(preserved_bubbles.iter().cloned());
            restore_source_bubbles(&mut corrected, &original, &source_preservations)?;
        }
        let output = cleaned
            .parent()
            .unwrap_or(&session.root_dir)
            .join(format!("editor-clean-{}.png", state_revision(state)));
        if output.exists() {
            fs::remove_file(&output)
                .with_context(|| format!("replace editor clean derivative {}", output.display()))?;
        }
        let derived = workflow.write_derived_clean_artifact(&base_clean, &output, &corrected)?;
        (derived.cleaned_image, Some(output))
    };
    let global_font = state.get("font_path").and_then(Value::as_str);
    let mut fallback_font_paths = Vec::new();
    for bubble in bubbles {
        if let Some(paths) = bubble.get("fallback_font_paths").and_then(Value::as_array) {
            fallback_font_paths.extend(paths.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        if let Some(path) = bubble.get("rendered_font_path").and_then(Value::as_str) {
            fallback_font_paths.push(path.to_owned());
        }
    }
    let mut unique_fallback_font_paths = Vec::with_capacity(fallback_font_paths.len());
    for path in fallback_font_paths {
        if !unique_fallback_font_paths.iter().any(|seen| seen == &path) {
            unique_fallback_font_paths.push(path);
        }
    }
    let fallback_font_paths = unique_fallback_font_paths;
    let bundled_primary =
        workflow.materialize_bundled_font(&session.root_dir, &crate::fonts::COMIC_NEUE_REGULAR)?;
    let bundled_fallback_paths = crate::fonts::bundled_fallbacks()
        .into_iter()
        .map(|font| {
            workflow
                .materialize_bundled_font(&session.root_dir, font)
                .map(|path| path.display().to_string())
        })
        .collect::<Result<Vec<_>>>()?;
    let bundled_primary = bundled_primary.display().to_string();
    let mut fallback_font_paths = fallback_font_paths;
    fallback_font_paths.extend(bundled_fallback_paths);
    fallback_font_paths.dedup();
    let fallback_font_paths = crate::workflow::order_fallback_font_paths(fallback_font_paths);
    let mut font_substitutions = Vec::new();
    let mut payloads = Vec::with_capacity(bubbles.len());
    for (bubble_index, bubble) in bubbles.iter().enumerate() {
        let bubble_id = bubble.get("id").and_then(Value::as_str);
        if bubble_id.is_some_and(|id| removed_ids.contains(id)) {
            continue;
        }
        let bbox: Rect = serde_json::from_value(
            bubble
                .get("bbox")
                .cloned()
                .ok_or_else(|| anyhow!("bubble {bubble_index} has no bbox"))?,
        )?;
        let text = bubble
            .get("translation")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let preserve_source = bubble_preserve_for_render(bubble);
        if preserve_source {
            payloads.push(TypesetPayload {
                id: bubble_id.map(str::to_owned),
                source_text: bubble
                    .get("source_text")
                    .and_then(Value::as_str)
                    .or_else(|| bubble.get("text").and_then(Value::as_str))
                    .map(str::to_owned),
                kind: bubble
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                preserve_by_default: bubble.get("preserve_by_default").and_then(Value::as_bool),
                needs_review: bubble.get("needs_review").and_then(Value::as_bool),
                flagged: bubble.get("flagged").and_then(Value::as_bool),
                preserve_source: Some(true),
                fallback_font_paths: crate::workflow::order_fallback_font_paths(
                    bubble
                        .get("fallback_font_paths")
                        .and_then(Value::as_array)
                        .map(|paths| {
                            paths
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                ),
                bbox,
                // The operator's current bbox is authoritative during an
                // editor render; stale detector geometry must not snap text
                // back into its original balloon.
                bubble_bbox: Some(bbox),
                text_bbox: editor_text_bbox(bubble, bbox)?,
                padding: bubble
                    .get("padding")
                    .and_then(Value::as_f64)
                    .map(|v| v as f32),
                text,
                font_path: None,
                min_font_size: None,
                max_font_size: None,
                text_color: bubble
                    .get("text_color")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                shape: bubble
                    .get("shape")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
            continue;
        }
        if text.trim().is_empty() {
            bail!(
                "bubble {bubble_index} has an empty translation; use Preserve original / remove translation box"
            );
        }
        let requested_font_path = bubble
            .get("font_path")
            .and_then(Value::as_str)
            .or_else(|| bubble.get("rendered_font_path").and_then(Value::as_str))
            .or(global_font)
            .filter(|path| !path.trim().is_empty());
        let (font_path, substitution_reason) = match requested_font_path {
            Some(path) if crate::workflow::is_generic_desktop_font(std::path::Path::new(path)) => (
                PathBuf::from(&bundled_primary),
                Some("generic_desktop_primary"),
            ),
            Some(path) => (safe_font_path(&session.root_dir, path)?, None),
            None => (PathBuf::from(&bundled_primary), Some("missing_primary")),
        };
        let (min_font_size, max_font_size) = editor_font_size_range(bubble);
        let render_index = payloads.len();
        if let Some(reason) = substitution_reason {
            let mut substitution = serde_json::json!({
                "index": render_index,
                "replacement_primary_font": font_path.display().to_string(),
                "reason": reason,
            });
            if let Some(requested) = requested_font_path {
                substitution["requested_font_path"] = Value::String(requested.to_owned());
            }
            font_substitutions.push(substitution);
        }
        payloads.push(TypesetPayload {
            id: bubble_id.map(str::to_owned),
            source_text: bubble
                .get("source_text")
                .and_then(Value::as_str)
                .or_else(|| bubble.get("text").and_then(Value::as_str))
                .map(str::to_owned),
            kind: bubble
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_owned),
            preserve_by_default: bubble.get("preserve_by_default").and_then(Value::as_bool),
            needs_review: bubble.get("needs_review").and_then(Value::as_bool),
            flagged: bubble.get("flagged").and_then(Value::as_bool),
            preserve_source: Some(false),
            fallback_font_paths: crate::workflow::order_fallback_font_paths(
                bubble
                    .get("fallback_font_paths")
                    .and_then(Value::as_array)
                    .map(|paths| {
                        paths
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            bbox,
            // Keep the containing geometry synchronized with the editable
            // bbox so rerendering honors a move or resize.
            bubble_bbox: Some(bbox),
            text_bbox: editor_text_bbox(bubble, bbox)?,
            padding: bubble
                .get("padding")
                .and_then(Value::as_f64)
                .map(|v| v as f32),
            text,
            font_path: Some(font_path.display().to_string()),
            min_font_size: Some(min_font_size),
            max_font_size: Some(max_font_size),
            text_color: bubble
                .get("text_color")
                .and_then(Value::as_str)
                .map(str::to_owned),
            shape: bubble
                .get("shape")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    let output = cleaned
        .parent()
        .unwrap_or(&session.root_dir)
        .join("rendered.png");
    let mut report = crate::typeset::typeset_page_with_fallbacks(
        &source,
        &payloads,
        &fallback_font_paths,
        &output,
    )?;
    if !font_substitutions.is_empty() {
        let mut substitutions = font_substitutions.clone();
        if let Some(reports) = report["bubbles"].as_array_mut() {
            for substitution in &mut substitutions {
                let Some(index) = substitution
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                else {
                    continue;
                };
                let Some(bubble_report) = reports.get_mut(index).and_then(Value::as_object_mut)
                else {
                    continue;
                };
                if let Some(resolved) = bubble_report.get("resolved_font_path").cloned() {
                    substitution["resolved_font_path"] = resolved;
                }
                for key in ["requested_font_path", "replacement_primary_font", "reason"] {
                    if let Some(value) = substitution.get(key) {
                        bubble_report.insert(key.to_owned(), value.clone());
                    }
                }
                bubble_report.insert("font_substituted".into(), Value::Bool(true));
            }
        }
        report["font_substitutions"] = Value::Array(substitutions);
    }
    workflow.register_render_locked(
        &output,
        &workflow.validate_clean_input(&source)?,
        json!({
            "request_bubbles": payloads,
            "report": report.clone(),
        }),
        json!({
            "editor_render": true,
            "correction_strokes": strokes.len(),
            "removed_bubbles": removed_bubbles,
        }),
    )?;
    let mut saved_state = state.clone();
    set_state_revision(&mut saved_state, state_revision(state).saturating_add(1));
    if let Some(page) = saved_state
        .get_mut("pages")
        .and_then(Value::as_array_mut)
        .and_then(|pages| pages.get_mut(index))
    {
        synchronize_rendered_bubbles(page);
        page["rendered_image_path"] = Value::String(
            output
                .strip_prefix(&session.root_dir)
                .unwrap_or(&output)
                .display()
                .to_string(),
        );
        page["corrected_cleaned_image_path"] = Value::String(
            corrected_cleaned
                .as_deref()
                .unwrap_or(&cleaned)
                .display()
                .to_string(),
        );
        page["render_dirty"] = Value::Bool(false);
        page["rendered_state_revision"] = Value::from(state_revision(state).saturating_add(1));
    }
    validate_state(&saved_state)?;
    atomic_json_save(&session.state_path, &saved_state)?;
    atomic_json_save(
        &translation_manifest_path(&session.state_path),
        &translation_manifest(&saved_state),
    )?;
    Ok(
        json!({"saved": true, "page_index": index, "rendered_image_path": output, "typeset": report, "state": saved_state}),
    )
}

fn restore_source_bubbles(
    output: &mut image::RgbImage,
    source: &image::RgbImage,
    removed_bubbles: &[Value],
) -> Result<()> {
    if output.dimensions() != source.dimensions() {
        bail!("source and cleaned page dimensions do not match while restoring a bubble");
    }
    let (width, height) = output.dimensions();
    for (index, removed) in removed_bubbles.iter().enumerate() {
        let bbox: Rect = serde_json::from_value(
            removed
                .get("bbox")
                .cloned()
                .ok_or_else(|| anyhow!("removed bubble {index} has no bbox"))?,
        )?;
        let bbox = bbox
            .validate()
            .map_err(|error| anyhow!("removed bubble {index} has invalid bbox: {error}"))?
            .clip(width as f32, height as f32)
            .ok_or_else(|| anyhow!("removed bubble {index} bbox is outside the source image"))?;
        let left = bbox.x1.floor().max(0.0) as u32;
        let top = bbox.y1.floor().max(0.0) as u32;
        let right = bbox.x2.ceil().min(width as f32) as u32;
        let bottom = bbox.y2.ceil().min(height as f32) as u32;
        for y in top..bottom {
            for x in left..right {
                *output.get_pixel_mut(x, y) = *source.get_pixel(x, y);
            }
        }
    }
    Ok(())
}

fn parse_stroke_color(stroke: &Value) -> image::Rgb<u8> {
    if let Some(raw) = stroke.get("color").and_then(Value::as_str) {
        let raw = raw.trim();
        let hex = raw.strip_prefix('#').unwrap_or(raw);
        if hex.len() == 6 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&hex[0..2], 16),
                u8::from_str_radix(&hex[2..4], 16),
                u8::from_str_radix(&hex[4..6], 16),
            ) {
                return image::Rgb([r, g, b]);
            }
        } else if hex.len() == 3
            && let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&hex[0..1], 16),
                u8::from_str_radix(&hex[1..2], 16),
                u8::from_str_radix(&hex[2..3], 16),
            )
        {
            return image::Rgb([r * 17, g * 17, b * 17]);
        }
    }
    image::Rgb([255, 255, 255])
}

fn apply_correction_strokes(cleaned: &Path, strokes: &[Value]) -> Result<image::RgbImage> {
    let base = image::open(cleaned)
        .with_context(|| format!("read cleaned image {}", cleaned.display()))?
        .to_rgb8();
    let mut output = base.clone();
    for stroke in strokes {
        let mode = stroke
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("cover");
        if mode != "cover" && mode != "restore" {
            bail!("correction stroke mode must be cover or restore");
        }
        let color = parse_stroke_color(stroke);
        let radius = (stroke.get("size").and_then(Value::as_f64).unwrap_or(24.0) / 2.0)
            .clamp(1.0, 80.0) as f32;
        let points = stroke
            .get("points")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("correction stroke points are required"))?;
        let points = points
            .iter()
            .map(|point| {
                Ok((
                    point
                        .get("x")
                        .and_then(Value::as_f64)
                        .ok_or_else(|| anyhow!("correction point x is required"))?
                        as f32,
                    point
                        .get("y")
                        .and_then(Value::as_f64)
                        .ok_or_else(|| anyhow!("correction point y is required"))?
                        as f32,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        for pair in points.windows(2) {
            let distance = (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1);
            let steps = (distance / radius.max(1.0)).ceil() as usize;
            for step in 0..=steps.max(1) {
                let t = step as f32 / steps.max(1) as f32;
                paint_circle(
                    &mut output,
                    &base,
                    pair[0].0 + (pair[1].0 - pair[0].0) * t,
                    pair[0].1 + (pair[1].1 - pair[0].1) * t,
                    radius,
                    mode == "restore",
                    color,
                );
            }
        }
        if let Some(&(x, y)) = points.first() {
            paint_circle(&mut output, &base, x, y, radius, mode == "restore", color);
        }
    }
    Ok(output)
}

fn paint_circle(
    output: &mut image::RgbImage,
    base: &image::RgbImage,
    x: f32,
    y: f32,
    radius: f32,
    restore: bool,
    color: image::Rgb<u8>,
) {
    let left = (x - radius).floor().max(0.0) as u32;
    let top = (y - radius).floor().max(0.0) as u32;
    let right = (x + radius).ceil().min(output.width() as f32 - 1.0) as u32;
    let bottom = (y + radius).ceil().min(output.height() as f32 - 1.0) as u32;
    let radius_sq = radius * radius;
    for py in top..=bottom {
        for px in left..=right {
            let dx = px as f32 - x;
            let dy = py as f32 - y;
            if dx * dx + dy * dy <= radius_sq {
                *output.get_pixel_mut(px, py) = if restore {
                    *base.get_pixel(px, py)
                } else {
                    color
                };
            }
        }
    }
}

fn respond(stream: &mut TcpStream, code: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    Ok(())
}

fn validate_state(value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_JSON {
        bail!("editor state exceeds {} bytes", MAX_JSON);
    }
    fn visit(value: &Value) -> Result<()> {
        match value {
            Value::Object(map) => {
                if let Some(bbox) = map.get("bbox") {
                    let obj = bbox
                        .as_object()
                        .ok_or_else(|| anyhow!("bbox must be an object"))?;
                    let read = |key: &str| {
                        obj.get(key)
                            .and_then(Value::as_f64)
                            .ok_or_else(|| anyhow!("bbox missing numeric {key}"))
                    };
                    let x1 = read("x1")?;
                    let y1 = read("y1")?;
                    let x2 = read("x2")?;
                    let y2 = read("y2")?;
                    if !x1.is_finite()
                        || !y1.is_finite()
                        || !x2.is_finite()
                        || !y2.is_finite()
                        || x2 <= x1
                        || y2 <= y1
                    {
                        bail!("invalid bbox coordinates");
                    }
                }
                for child in map.values() {
                    visit(child)?;
                }
            }
            Value::Array(items) => {
                for child in items {
                    visit(child)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(value)
}

fn atomic_json_save(path: &Path, value: &Value) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("state path has no parent"))?;
    let temp = tempfile::NamedTempFile::new_in(parent).context("create temporary editor state")?;
    serde_json::to_writer_pretty(temp.as_file(), value).context("write editor state")?;
    temp.as_file().sync_all().context("flush editor state")?;
    temp.persist(path)
        .map_err(|e| anyhow!("promote editor state: {}", e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use tempfile::tempdir;

    #[test]
    fn correction_strokes_cover_and_restore_clean_pixels() {
        let dir = tempdir().unwrap();
        let clean = dir.path().join("clean.png");
        let mut image = RgbImage::from_pixel(12, 12, Rgb([12, 34, 56]));
        image.put_pixel(3, 3, Rgb([90, 80, 70]));
        image.save(&clean).unwrap();
        let strokes = vec![
            json!({"mode":"cover","size":2,"points":[{"x":3,"y":3}]}),
            json!({"mode":"restore","size":2,"points":[{"x":3,"y":3}]}),
            json!({"mode":"cover","size":2,"points":[{"x":8,"y":8}]}),
        ];
        let corrected = apply_correction_strokes(&clean, &strokes).unwrap();
        assert_eq!(*corrected.get_pixel(3, 3), Rgb([90, 80, 70]));
        assert_eq!(*corrected.get_pixel(8, 8), Rgb([255, 255, 255]));
        assert_eq!(
            *image::open(&clean).unwrap().to_rgb8().get_pixel(8, 8),
            Rgb([12, 34, 56])
        );
    }

    #[test]
    fn correction_canvas_is_hidden_outside_cleaned_variant() {
        assert!(EDITOR_HTML.contains("const visible=$('variant').value==='cleaned'"));
        assert!(EDITOR_HTML.contains("paintCanvas.hidden=true"));
        assert!(EDITOR_HTML.contains("redrawPaint();$('brush').disabled=chosen!=='cleaned'"));
    }

    #[test]
    fn fitted_font_size_is_a_cap_not_a_locked_floor() {
        let fitted = json!({"font_size": 47.5});
        assert_eq!(
            editor_font_size_range(&fitted),
            (DEFAULT_EDITOR_MIN_FONT, 47.5)
        );
        let explicit = json!({"min_font_size": 1.0, "max_font_size": 38.0, "font_size": 12.0});
        assert_eq!(editor_font_size_range(&explicit), (1.0, 12.0));
        let tiny = json!({"font_size": 4.0});
        assert_eq!(editor_font_size_range(&tiny), (4.0, 4.0));
        let defaults = json!({});
        assert_eq!(
            editor_font_size_range(&defaults),
            (DEFAULT_EDITOR_MIN_FONT, DEFAULT_EDITOR_MAX_FONT)
        );
        let range_only = json!({"min_font_size": 1.0, "max_font_size": 38.0});
        assert_eq!(editor_font_size_range(&range_only), (1.0, 38.0));
    }

    #[test]
    fn page_gallery_has_an_independent_vertical_scroller() {
        assert!(EDITOR_HTML.contains("id=\"pagesPanel\" class=\"panel\""));
        assert!(EDITOR_HTML.contains(
            "#pagesPanel{display:grid;grid-template-rows:auto minmax(0,1fr);min-height:0}"
        ));
        assert!(EDITOR_HTML.contains("#gallery{min-height:0;overflow:auto;"));
        assert!(EDITOR_HTML.contains(
            "grid-template-columns:repeat(auto-fill,minmax(75px,1fr));grid-auto-rows:max-content;overflow-y:auto;overflow-x:hidden"
        ));
        assert!(EDITOR_HTML.contains(".stage-wrap{overflow:auto;"));
        assert!(EDITOR_HTML.contains("#inspector{overflow:auto;"));
    }

    #[test]
    fn stale_state_revision_blocks_save_and_render_and_browser_offers_reload() {
        let current = json!({
            "state_revision": 2,
            "pages": [{"id": "page-1", "bubbles": [{"id": "bubble-1"}]}]
        });
        let stale = json!({
            "state_revision": 1,
            "pages": [{"id": "page-1", "bubbles": []}]
        });
        assert_eq!(state_revision(&current), 2);
        assert!(!state_revision_matches(&current, &stale));
        let mut advanced = current.clone();
        set_state_revision(&mut advanced, 3);
        assert!(!state_revision_matches(&advanced, &stale));
        assert!(EDITOR_HTML.contains("Project changed—reload this page"));
        assert!(EDITOR_HTML.contains("id=\"reload\""));
        assert!(EDITOR_HTML.contains("if(dirty&&!stale)save().catch(()=>{})"));
        assert!(EDITOR_HTML.contains("renderQueue=null"));
        assert!(EDITOR_HTML.contains("const previousRender=renderQueue"));
        assert!(EDITOR_HTML.contains("releaseRender();if(renderQueue===queue)renderQueue=null"));
        assert!(EDITOR_HTML.contains("await save(true)"));
        assert!(
            EDITOR_HTML.contains(
                "while(saveInFlight||renderInFlight){await(saveInFlight||renderInFlight)"
            )
        );
        assert!(EDITOR_HTML.contains("generation===editGeneration"));
        assert!(EDITOR_HTML.contains("if(renderInFlight===request)renderInFlight=null"));
        assert!(
            EDITOR_HTML
                .contains("const renderState=JSON.stringify({page_index:renderPageIndex,state})")
        );
        assert!(EDITOR_HTML.contains("e.status===409"));
    }

    #[test]
    fn operator_geometry_drops_stale_text_anchor_and_render_normalizes_bbox() {
        let bbox = Rect {
            x1: 24.0,
            y1: 12.0,
            x2: 88.0,
            y2: 100.0,
        };
        let stale_text_bbox = json!({
            "x1": 8.0,
            "y1": 8.0,
            "x2": 40.0,
            "y2": 30.0
        });
        let operator_bubble = json!({
            "bbox": bbox,
            "bubble_bbox": bbox,
            "text_bbox": stale_text_bbox
        });
        assert_eq!(editor_text_bbox(&operator_bubble, bbox).unwrap(), None);

        let legacy_bubble = json!({
            "bbox": bbox,
            "bubble_bbox": {"x1": 8.0, "y1": 8.0, "x2": 72.0, "y2": 92.0},
            "text_bbox": stale_text_bbox
        });
        assert_eq!(
            editor_text_bbox(&legacy_bubble, bbox).unwrap(),
            Some(Rect {
                x1: 8.0,
                y1: 8.0,
                x2: 40.0,
                y2: 30.0
            })
        );

        let mut page = json!({
            "bubbles": [{
                "bbox": bbox,
                "bubble_bbox": {"x1": 8.0, "y1": 8.0, "x2": 72.0, "y2": 92.0}
            }]
        });
        synchronize_rendered_bubbles(&mut page);
        assert_eq!(page["bubbles"][0]["bubble_bbox"], json!(bbox));

        let original_bbox = json!({"x1": 8.0, "y1": 8.0, "x2": 72.0, "y2": 92.0});
        let mut reopened = normalize_editor_state(json!({
            "state_revision": 3,
            "pages": [{
                "id": "page-1",
                "bubbles": [{
                    "id": "bubble-1",
                    "bbox": original_bbox,
                    "bubble_bbox": {"x1": 8.0, "y1": 8.0, "x2": 72.0, "y2": 92.0},
                    "text_bbox": stale_text_bbox
                }]
            }]
        }))
        .unwrap();
        let saved = json!({
            "state_revision": 4,
            "pages": [{
                "id": "page-1",
                "bubbles": [{
                    "id": "bubble-1",
                    "bbox": bbox,
                    "bubble_bbox": bbox,
                    "text_bbox": {"x1": 30.0, "y1": 20.0, "x2": 62.0, "y2": 50.0}
                }]
            }]
        });
        merge_saved_edits(&mut reopened, &saved);
        assert_eq!(
            reopened["pages"][0]["bubbles"][0]["bubble_bbox"],
            json!(bbox)
        );
        assert_eq!(
            reopened["pages"][0]["bubbles"][0]["text_bbox"],
            json!({"x1": 30.0, "y1": 20.0, "x2": 62.0, "y2": 50.0})
        );
    }

    #[test]
    fn editor_keeps_advisories_visible_without_counting_them_as_auto_flags() {
        let flag_function = EDITOR_HTML
            .split("function isFlagged(i){")
            .nth(1)
            .and_then(|tail| tail.split("function fileName").next())
            .expect("editor isFlagged function");
        assert!(flag_function.contains("bubbleIsFlagged"));
        assert!(flag_function.contains("p.issues"));
        assert!(!flag_function.contains("render_dirty"));
        assert!(!flag_function.contains("needs_review"));
        assert!(EDITOR_HTML.contains("does not block approval"));
        assert!(
            EDITOR_HTML.contains(
                "$('approveExport').disabled=!!(review&&review.status==='fixes_requested')"
            )
        );
    }

    #[test]
    fn removed_bubble_tombstone_survives_reconstruction_and_blocks_approval() {
        let bbox = json!({"x1": 1.0, "y1": 1.0, "x2": 8.0, "y2": 8.0});
        let mut base = json!({
            "state_revision": 0,
            "pages": [{
                "id": "page-1",
                "bubbles": [{"id":"bubble-1","bbox":bbox.clone(),"text":"原文","translation":"Bản dịch"}]
            }]
        });
        let saved = json!({
            "state_revision": 2,
            "pages": [{
                "id": "page-1",
                "bubbles": [{"id":"bubble-1","bbox":bbox,"text":"原文","translation":"Bản dịch"}],
                "removed_bubbles": [{"id":"bubble-1","bbox":bbox,"source_text":"原文","translation":"Bản dịch"}],
                "render_dirty": true
            }]
        });
        base = normalize_editor_state(base).unwrap();
        merge_saved_edits(&mut base, &saved);
        assert!(base["pages"][0]["bubbles"].as_array().unwrap().is_empty());
        assert_eq!(base["pages"][0]["removed_bubbles"][0]["id"], "bubble-1");
        assert_eq!(base["pages"][0]["render_dirty"], true);
        let blockers = review_blockers(&json!({
            "pages": [{
                "render_dirty": true,
                "issues": [{"issue_type":"custom"}],
                "bubbles": [{"id":"bubble-2","flagged":true,"bbox":bbox}]
            }]
        }));
        assert_eq!(blockers.len(), 3);
        assert!(blockers.iter().any(|item| item["kind"] == "render_dirty"));
        assert!(blockers.iter().any(|item| item["kind"] == "issue"));
        assert!(blockers.iter().any(|item| item["kind"] == "flagged_bubble"));
        let flagged = blockers
            .iter()
            .find(|item| item["kind"] == "flagged_bubble")
            .unwrap();
        assert_eq!(flagged["source_ocr"], Value::Null);
        assert_eq!(flagged["current_translation"], "");
    }

    #[test]
    fn reopening_a_matching_sidecar_clears_stale_dirty_without_blocking_advisory() {
        let bbox = json!({"x1": 1.0, "y1": 1.0, "x2": 8.0, "y2": 8.0});
        let mut base = normalize_editor_state(json!({
            "state_revision": 3,
            "pages": [{
                "id": "page-1",
                "bubbles": [{
                    "id": "bubble-1",
                    "bbox": bbox,
                    "bubble_bbox": null,
                    "translation": "Bản dịch",
                    "needs_review": true
                }]
            }]
        }))
        .unwrap();
        let saved = json!({
            "state_revision": 4,
            "pages": [{
                "id": "page-1",
                "bubbles": [{
                    "id": "bubble-1",
                    "bbox": {"x1": 1.0, "y1": 1.0, "x2": 8.0, "y2": 8.0},
                    "translation": "Bản dịch",
                    "needs_review": true
                }],
                "render_dirty": true
            }]
        });
        merge_saved_edits(&mut base, &saved);
        assert_eq!(base["pages"][0]["render_dirty"], false);
        assert!(review_blockers(&base).is_empty());

        let mut flagged = saved;
        flagged["pages"][0]["bubbles"][0]["flagged"] = json!(true);
        merge_saved_edits(&mut base, &flagged);
        let blockers = review_blockers(&base);
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0]["kind"], "flagged_bubble");
    }

    #[test]
    fn restoring_removed_bubble_copies_source_pixels_into_render_base() {
        let mut cleaned = RgbImage::from_pixel(10, 10, Rgb([255, 255, 255]));
        let source = RgbImage::from_pixel(10, 10, Rgb([20, 30, 40]));
        restore_source_bubbles(
            &mut cleaned,
            &source,
            &[json!({"id":"bubble-1","bbox":{"x1":2.0,"y1":3.0,"x2":5.0,"y2":6.0}})],
        )
        .unwrap();
        assert_eq!(*cleaned.get_pixel(2, 3), Rgb([20, 30, 40]));
        assert_eq!(*cleaned.get_pixel(4, 5), Rgb([20, 30, 40]));
        assert_eq!(*cleaned.get_pixel(1, 3), Rgb([255, 255, 255]));
    }

    #[test]
    fn editor_exposes_explicit_bubble_restore_and_actionable_flag_feedback() {
        assert!(EDITOR_HTML.contains("Preserve original / remove translation box"));
        assert!(EDITOR_HTML.contains("id=\"restoreBubble\""));
        assert!(EDITOR_HTML.contains("issue_type:'flagged_bubble'"));
        assert!(EDITOR_HTML.contains("source_ocr:b.source_text"));
        assert!(!EDITOR_HTML.contains("b.confidence<.65"));
        assert!(
            EDITOR_HTML.contains("for(let i=0;i<pages().length;i++){if(pages()[i].render_dirty)")
        );
    }

    #[test]
    fn bubble_double_click_editor_keeps_manual_and_ai_lanes_distinct() {
        assert!(EDITOR_HTML.contains("id=\"bubbleModal\""));
        assert!(EDITOR_HTML.contains("ondblclick=e=>{e.stopPropagation();openBubbleEditor(i)}"));
        assert!(EDITOR_HTML.contains("Save &amp; rerender this page"));
        assert!(EDITOR_HTML.contains("Source OCR (read only)"));
        assert!(EDITOR_HTML.contains("link_status='linked'"));
        assert!(EDITOR_HTML.contains("link_status='ambiguous'"));
        assert!(
            EDITOR_HTML
                .contains("No saved bubbles on this page — recover/import typeset data first")
        );
    }
}
