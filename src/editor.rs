//! A small authenticated loopback editor server.
//!
//! The server deliberately uses a fixed image and persistence path captured at
//! startup.  No request can select an arbitrary filesystem path.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::config::{Config, ConfigArgs};
use crate::domain::{Rect, TypesetPayload};

const MAX_REQUEST: usize = 4 * 1024 * 1024;
// Editor state for ad-hoc loopback sessions remains deliberately small.
const MAX_JSON: usize = 2 * 1024 * 1024;
// A managed review is reconstructed from the server-owned manifest and render
// sidecars. The browser still carries that complete state on save/render, so
// large jobs need room for all pages and their OCR metadata. The state still
// has a hard bound so a corrupt manifest cannot allocate forever.
const MAX_MANAGED_STATE_JSON: usize = 16 * 1024 * 1024;
// Keep the review UI embedded so the loopback editor has no CDN dependency.
const EDITOR_HTML: &str = include_str!("../assets/editor.html");

const REVIEW_FILE: &str = "review.json";
const REVIEW_AUDIT_FILE: &str = "review-audit.json";
const EDITOR_LEASE_FILE: &str = ".fukidashi-editor.lock";

#[derive(Debug, Clone)]
struct EditorLeaseInfo {
    pid: u32,
    review_session_id: Option<String>,
    revision: Option<u64>,
}

/// A cross-process lease for one native editor. The MCP process reserves this
/// lease while it transitions the review and starts the child, then hands it
/// to the child PID before returning to the caller. The native binary adopts
/// it and keeps the heartbeat alive for the entire window lifetime.
pub struct NativeEditorLease {
    path: PathBuf,
    review_session_id: String,
    revision: u64,
    stop: Option<mpsc::Sender<()>>,
    heartbeat: Option<thread::JoinHandle<()>>,
    remove_on_drop: bool,
}

impl Drop for NativeEditorLease {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        if self.remove_on_drop {
            remove_editor_lease_if_owner(&self.path, std::process::id());
        }
    }
}

#[derive(Clone)]
struct ActiveNativeEditor {
    session_id: String,
    revision: u64,
    pid: u32,
    alive: Arc<AtomicBool>,
    child: Arc<Mutex<Child>>,
}

static ACTIVE_NATIVE_EDITORS: OnceLock<
    Mutex<std::collections::HashMap<PathBuf, Arc<ActiveNativeEditor>>>,
> = OnceLock::new();
static EDITOR_LIFECYCLE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn active_native_editors()
-> &'static Mutex<std::collections::HashMap<PathBuf, Arc<ActiveNativeEditor>>> {
    ACTIVE_NATIVE_EDITORS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn editor_lifecycle_lock() -> &'static Mutex<()> {
    EDITOR_LIFECYCLE_LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReviewState {
    pub review_session_id: String,
    pub revision: u64,
    pub status: String,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub feedback: Vec<Value>,
    #[serde(default)]
    pub approved_pages: Vec<usize>,
    #[serde(default)]
    pub consumed: bool,
    #[serde(default)]
    pub audit: Vec<Value>,
}

#[derive(Debug)]
struct ReviewChannel {
    state: Mutex<ReviewState>,
    notify: Notify,
    root_dir: PathBuf,
}

static REVIEW_CHANNELS: OnceLock<Mutex<std::collections::HashMap<String, Arc<ReviewChannel>>>> =
    OnceLock::new();
static MANUAL_CLEAN_ENGINE: OnceLock<Mutex<crate::vision::ocr::OcrEngine>> = OnceLock::new();

fn review_channels() -> &'static Mutex<std::collections::HashMap<String, Arc<ReviewChannel>>> {
    REVIEW_CHANNELS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn manual_clean_engine() -> &'static Mutex<crate::vision::ocr::OcrEngine> {
    MANUAL_CLEAN_ENGINE.get_or_init(|| Mutex::new(crate::vision::ocr::OcrEngine::default()))
}

fn review_file(root: &Path) -> PathBuf {
    root.join(REVIEW_FILE)
}

fn review_audit_file(root: &Path) -> PathBuf {
    root.join(REVIEW_AUDIT_FILE)
}

fn editor_lease_file(root: &Path) -> PathBuf {
    root.join(EDITOR_LEASE_FILE)
}

fn write_editor_lease_metadata(
    path: &Path,
    pid: u32,
    review_session_id: &str,
    revision: u64,
) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("write editor lease {}", path.display()))?;
    writeln!(file, "pid={pid}")?;
    writeln!(file, "review_session_id={review_session_id}")?;
    writeln!(file, "revision={revision}")?;
    file.sync_all()?;
    Ok(())
}

fn read_editor_lease_info(path: &Path) -> Option<EditorLeaseInfo> {
    let contents = fs::read_to_string(path).ok()?;
    let pid = contents
        .lines()
        .find_map(|line| line.strip_prefix("pid=")?.trim().parse::<u32>().ok())?;
    let review_session_id = contents
        .lines()
        .find_map(|line| line.strip_prefix("review_session_id=").map(str::to_owned));
    let revision = contents
        .lines()
        .find_map(|line| line.strip_prefix("revision=")?.trim().parse::<u64>().ok());
    Some(EditorLeaseInfo {
        pid,
        review_session_id,
        revision,
    })
}

fn editor_process_is_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut code = 0;
        let result = unsafe { GetExitCodeProcess(handle, &mut code) } != 0;
        unsafe { CloseHandle(handle) };
        return result && code == STILL_ACTIVE as u32;
    }
    #[cfg(target_os = "linux")]
    {
        return Path::new("/proc").join(pid.to_string()).exists();
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        return false;
    }
    #[cfg(not(any(windows, unix)))]
    {
        false
    }
}

fn remove_editor_lease_if_owner(path: &Path, pid: u32) {
    if read_editor_lease_info(path).is_some_and(|info| info.pid == pid) {
        let _ = fs::remove_file(path);
    }
}

fn start_editor_lease_heartbeat(
    path: &Path,
    review_session_id: &str,
    revision: u64,
) -> Result<(mpsc::Sender<()>, thread::JoinHandle<()>)> {
    let (stop, receiver) = mpsc::channel();
    let heartbeat_path = path.to_path_buf();
    let session = review_session_id.to_owned();
    let heartbeat = thread::Builder::new()
        .name("fukidashi-editor-lease".to_owned())
        .spawn(move || {
            while receiver.recv_timeout(Duration::from_secs(30)).is_err() {
                if write_editor_lease_metadata(
                    &heartbeat_path,
                    std::process::id(),
                    &session,
                    revision,
                )
                .is_err()
                {
                    break;
                }
            }
        })
        .context("start editor lease heartbeat")?;
    Ok((stop, heartbeat))
}

fn create_editor_lease(
    path: PathBuf,
    review_session_id: &str,
    revision: u64,
) -> Result<NativeEditorLease> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("reserve native editor lease {}", path.display()))?;
    drop(file);
    if let Err(error) =
        write_editor_lease_metadata(&path, std::process::id(), review_session_id, revision)
    {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    let (stop, heartbeat) = match start_editor_lease_heartbeat(&path, review_session_id, revision) {
        Ok(value) => value,
        Err(error) => {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
    };
    Ok(NativeEditorLease {
        path,
        review_session_id: review_session_id.to_owned(),
        revision,
        stop: Some(stop),
        heartbeat: Some(heartbeat),
        remove_on_drop: true,
    })
}

fn try_reserve_editor_lease(
    root: &Path,
    review_session_id: &str,
    revision: u64,
) -> Result<EditorLeaseReservation> {
    let path = editor_lease_file(root);
    match create_editor_lease(path.clone(), review_session_id, revision) {
        Ok(lease) => Ok(EditorLeaseReservation::Acquired(lease)),
        Err(error)
            if error
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
        {
            if let Some(info) = read_editor_lease_info(&path)
                && editor_process_is_alive(info.pid)
            {
                return Ok(EditorLeaseReservation::Active(info));
            }
            // Match the existing workflow lock's recovery rule: a readable
            // dead PID is reclaimable immediately; malformed metadata is only
            // reclaimed after a long age guard.
            let stale = read_editor_lease_info(&path)
                .is_some_and(|info| !editor_process_is_alive(info.pid))
                || fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(10 * 60));
            if stale {
                let _ = fs::remove_file(&path);
                return try_reserve_editor_lease(root, review_session_id, revision);
            }
            // The owner writes the PID and heartbeat by truncating the small
            // metadata file. A reader can briefly observe the file between
            // truncate and write; wait for that publication instead of
            // reclaiming or reporting a false lease conflict.
            Ok(EditorLeaseReservation::ActiveMetadataPending)
        }
        Err(error) => Err(error),
    }
}

enum EditorLeaseReservation {
    Acquired(NativeEditorLease),
    Active(EditorLeaseInfo),
    ActiveMetadataPending,
}

impl NativeEditorLease {
    fn update_metadata(&mut self, review_session_id: &str, revision: u64) -> Result<()> {
        write_editor_lease_metadata(&self.path, std::process::id(), review_session_id, revision)?;
        self.review_session_id = review_session_id.to_owned();
        self.revision = revision;
        Ok(())
    }

    fn handoff_to_child(mut self, child_pid: u32) -> Result<()> {
        let Some(stop) = self.stop.take() else {
            bail!("native editor lease is already handed off");
        };
        let _ = stop.send(());
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        if let Err(error) = write_editor_lease_metadata(
            &self.path,
            child_pid,
            &self.review_session_id,
            self.revision,
        ) {
            return Err(error);
        }
        self.remove_on_drop = false;
        std::mem::forget(self);
        Ok(())
    }
}

/// Adopt the reservation handed off by the MCP parent process. The native
/// process becomes the lease owner before creating its window.
pub fn adopt_editor_lease(
    path: &Path,
    review_session_id: &str,
    revision: u64,
) -> Result<NativeEditorLease> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let Some(info) = read_editor_lease_info(path) else {
            if Instant::now() >= deadline {
                bail!("native editor lease metadata is missing");
            }
            thread::sleep(Duration::from_millis(10));
            continue;
        };
        if info.pid == std::process::id() {
            if info.review_session_id.as_deref() != Some(review_session_id)
                || info.revision != Some(revision)
            {
                bail!("native editor lease does not match the requested review");
            }
            break;
        }
        if !editor_process_is_alive(info.pid) {
            // The parent can die between spawn and handoff. Reclaiming its
            // dead reservation lets the child still protect its own window.
            write_editor_lease_metadata(path, std::process::id(), review_session_id, revision)?;
            break;
        }
        if Instant::now() >= deadline {
            bail!("native editor lease is owned by process {}", info.pid);
        }
        thread::sleep(Duration::from_millis(10));
    }
    let (stop, heartbeat) = start_editor_lease_heartbeat(path, review_session_id, revision)?;
    Ok(NativeEditorLease {
        path: path.to_path_buf(),
        review_session_id: review_session_id.to_owned(),
        revision,
        stop: Some(stop),
        heartbeat: Some(heartbeat),
        remove_on_drop: true,
    })
}

/// Reserve a lease for a native editor launched outside the MCP parent.
pub fn acquire_native_editor_lease(
    root: &Path,
    review_session_id: &str,
    revision: u64,
) -> Result<NativeEditorLease> {
    match try_reserve_editor_lease(root, review_session_id, revision)? {
        EditorLeaseReservation::Acquired(lease) => Ok(lease),
        EditorLeaseReservation::Active(info) => {
            bail!(
                "native editor lease is already owned by process {}",
                info.pid
            )
        }
        EditorLeaseReservation::ActiveMetadataPending => {
            bail!("native editor lease metadata is being published")
        }
    }
}

fn native_editor_response(
    root_dir: &Path,
    state_path: &Path,
    review_path: &Path,
    review_state: &ReviewState,
    native_binary: Option<&Path>,
    reused: bool,
) -> Value {
    let mut response = json!({
        "editor_kind": "native",
        "url": format!("native://{}", root_dir.display()),
        "native_binary": native_binary.map_or(Value::Null, |path| json!(path)),
        "persistence_path": state_path,
        "session_token": Value::Null,
        "review_session_id": review_state.review_session_id,
        "review_revision": review_state.revision,
        "review_path": review_path,
    });
    if reused {
        response["reused"] = Value::Bool(true);
        response["message"] =
            Value::String("native editor already open; reused the active review".to_owned());
    }
    response
}

fn local_native_editor_for_review(
    root_dir: &Path,
    review_state: &ReviewState,
) -> Option<Arc<ActiveNativeEditor>> {
    let mut active = active_native_editors().lock().ok()?;
    let Some(entry) = active.get(root_dir).cloned() else {
        return None;
    };
    if native_child_has_exited(&entry) {
        active.remove(root_dir);
        return None;
    }
    if entry.session_id == review_state.review_session_id && entry.revision == review_state.revision
    {
        Some(entry)
    } else {
        None
    }
}

fn editor_lease_matches_review(info: &EditorLeaseInfo, review: &ReviewState) -> bool {
    info.review_session_id
        .as_deref()
        .is_none_or(|session| session == review.review_session_id)
        && info
            .revision
            .is_none_or(|revision| revision == review.revision)
}

fn reap_native_editor(root_dir: &Path) {
    if let Ok(mut active) = active_native_editors().lock() {
        if let Some(entry) = active.get(root_dir)
            && native_child_has_exited(entry)
        {
            active.remove(root_dir);
        }
    }
}

fn native_child_has_exited(entry: &ActiveNativeEditor) -> bool {
    if !entry.alive.load(Ordering::Acquire) {
        return true;
    }
    let exited = match entry.child.lock() {
        Ok(mut child) => match child.try_wait() {
            Ok(Some(_)) | Err(_) => true,
            Ok(None) => false,
        },
        Err(_) => true,
    };
    if exited {
        entry.alive.store(false, Ordering::Release);
    }
    exited
}

fn register_native_editor(
    root_dir: PathBuf,
    session_id: String,
    revision: u64,
    child: Child,
    lease_path: PathBuf,
) -> Result<()> {
    let pid = child.id();
    let child = Arc::new(Mutex::new(child));
    let alive = Arc::new(AtomicBool::new(true));
    let entry = Arc::new(ActiveNativeEditor {
        session_id,
        revision,
        pid,
        alive: Arc::clone(&alive),
        child: Arc::clone(&child),
    });
    if let Ok(mut active) = active_native_editors().lock() {
        active.insert(root_dir.clone(), Arc::clone(&entry));
    }
    let reaper_child = Arc::clone(&child);
    let reaper_alive = Arc::clone(&alive);
    let reaper_entry = Arc::clone(&entry);
    let reaper_root = root_dir.clone();
    let reaper_lease_path = lease_path.clone();
    let reaper = thread::Builder::new()
        .name("fukidashi-editor-reaper".to_owned())
        .spawn(move || {
            loop {
                if !reaper_alive.load(Ordering::Acquire) {
                    break;
                }
                let exited = match reaper_child.lock() {
                    Ok(mut child) => match child.try_wait() {
                        Ok(Some(_)) | Err(_) => true,
                        Ok(None) => false,
                    },
                    Err(_) => true,
                };
                if exited {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
            reaper_alive.store(false, Ordering::Release);
            if let Ok(mut active) = active_native_editors().lock()
                && active
                    .get(&reaper_root)
                    .is_some_and(|current| Arc::ptr_eq(current, &reaper_entry))
            {
                active.remove(&reaper_root);
            }
            remove_editor_lease_if_owner(&reaper_lease_path, pid);
        })
        .context("start native editor reaper");
    if let Err(error) = reaper {
        if let Ok(mut child) = child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        alive.store(false, Ordering::Release);
        if let Ok(mut active) = active_native_editors().lock()
            && active
                .get(&root_dir)
                .is_some_and(|current| Arc::ptr_eq(current, &entry))
        {
            active.remove(&root_dir);
        }
        remove_editor_lease_if_owner(&lease_path, pid);
        return Err(error);
    }
    Ok(())
}

fn launch_native_editor(
    editor_bin: &Path,
    review_session_id: &str,
    revision: u64,
    root_dir: &Path,
    lease_path: Option<&Path>,
) -> Result<Child> {
    let mut command = Command::new(editor_bin);
    command
        .arg("--review-session-id")
        .arg(review_session_id)
        .arg("--review-revision")
        .arg(revision.to_string());
    if let Some(lease_path) = lease_path {
        command.arg("--editor-lease").arg(lease_path);
    }
    command.arg(root_dir);
    command
        .spawn()
        .with_context(|| format!("launch native editor {}", editor_bin.display()))
}

enum ManagedEditorLease {
    Acquired(NativeEditorLease),
    Reuse(ReviewState),
}

fn acquire_managed_editor_lease(
    root_dir: &Path,
    initial_review: Option<&ReviewState>,
) -> Result<ManagedEditorLease> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match try_reserve_editor_lease(
            root_dir,
            initial_review
                .map(|review| review.review_session_id.as_str())
                .unwrap_or("pending"),
            initial_review.map_or(0, |review| review.revision),
        )? {
            EditorLeaseReservation::Acquired(lease) => {
                return Ok(ManagedEditorLease::Acquired(lease));
            }
            EditorLeaseReservation::Active(info) => {
                if let Some(review) = read_review(root_dir) {
                    if review.action.as_deref() == Some("approve_export") {
                        // A completed review is only reopened after the old
                        // native process releases its lease. Its UI is closing
                        // immediately after approval, so waiting here preserves
                        // the explicit reopen semantics without a duplicate UI.
                    } else if review.status == "awaiting_review"
                        && review.action.is_none()
                        && editor_lease_matches_review(&info, &review)
                    {
                        return Ok(ManagedEditorLease::Reuse(review));
                    }
                }
                if !editor_process_is_alive(info.pid) {
                    let _ = fs::remove_file(editor_lease_file(root_dir));
                    continue;
                }
                if Instant::now() >= deadline {
                    bail!(
                        "timed out waiting for another native editor to finish job {}",
                        root_dir.display()
                    );
                }
                thread::sleep(Duration::from_millis(25));
            }
            EditorLeaseReservation::ActiveMetadataPending => {
                if Instant::now() >= deadline {
                    bail!(
                        "timed out waiting for native editor lease metadata for job {}",
                        root_dir.display()
                    );
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
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
    /// Managed projects are server-reconstructed and may use the larger,
    /// still-bounded state budget. Ad-hoc loopback sessions retain MAX_JSON.
    server_owned_state: bool,
    review: Arc<ReviewChannel>,
}

/// Start an editor on an ephemeral loopback port and return immediately.
pub fn serve_editor(image_path: &Path, json_data: Value) -> Result<Value> {
    serve_editor_impl(image_path, json_data, Vec::new(), false, None)
}

pub fn serve_editor_with_allowed_sources(
    image_path: &Path,
    json_data: Value,
    allowed_source_paths: Vec<PathBuf>,
) -> Result<Value> {
    serve_editor_impl(image_path, json_data, allowed_source_paths, false, None)
}

pub fn serve_editor_with_allowed_sources_and_options(
    image_path: &Path,
    json_data: Value,
    allowed_source_paths: Vec<PathBuf>,
    reopen_completed: bool,
) -> Result<Value> {
    serve_editor_impl(
        image_path,
        json_data,
        allowed_source_paths,
        reopen_completed,
        None,
    )
}

fn serve_editor_impl(
    image_path: &Path,
    json_data: Value,
    allowed_source_paths: Vec<PathBuf>,
    reopen_completed: bool,
    test_launcher: Option<&dyn Fn(&Path, &str, u64, &Path, &Path) -> Result<Child>>,
) -> Result<Value> {
    let image_path = fs::canonicalize(image_path)
        .with_context(|| format!("resolve editor image {}", image_path.display()))?;
    let parent = image_path
        .parent()
        .ok_or_else(|| anyhow!("editor image has no parent"))?;
    let root_dir = managed_editor_root(parent);
    let server_owned_state = is_managed_editor_root(&root_dir);
    track_editor_root(&root_dir);
    let state_path = root_dir.join("project.json");
    let review_path = review_file(&root_dir);
    let editor_bin = test_launcher
        .map(|_| PathBuf::from("fukidashi-editor-test-launcher"))
        .or_else(find_editor_binary);
    let native_requested = editor_bin.is_some();
    // The registry and the review/lease transition must be one critical
    // section. This mutex is process-local; the filesystem lease below closes
    // the same race between independent MCP processes.
    let _lifecycle_guard = if server_owned_state && native_requested {
        Some(
            editor_lifecycle_lock()
                .lock()
                .map_err(|_| anyhow!("editor lifecycle lock poisoned"))?,
        )
    } else {
        None
    };
    reap_native_editor(&root_dir);
    let existing_review = if review_path.exists() {
        Some(
            serde_json::from_slice::<ReviewState>(
                &fs::read(&review_path).context("read existing review state")?,
            )
            .context("parse existing review state")?,
        )
    } else {
        None
    };
    if let Some(existing) = existing_review.as_ref()
        && existing.action.as_deref() == Some("approve_export")
        && !reopen_completed
    {
        return Ok(json!({
            "editor_kind": "already_completed",
            "action": existing.action,
            "status": existing.status,
            "review_session_id": existing.review_session_id,
            "review_revision": existing.revision,
            "review": existing,
            "persistence_path": state_path,
            "review_path": review_path,
        }));
    }
    let mut native_lease = None;
    if server_owned_state && native_requested {
        if let Some(existing) = existing_review.as_ref()
            && existing.status == "awaiting_review"
            && existing.action.is_none()
            && let Some(active) = local_native_editor_for_review(&root_dir, existing)
        {
            let _ = active.pid;
            return Ok(native_editor_response(
                &root_dir,
                &state_path,
                &review_path,
                existing,
                editor_bin.as_deref(),
                true,
            ));
        }
        match acquire_managed_editor_lease(&root_dir, existing_review.as_ref())? {
            ManagedEditorLease::Reuse(review) => {
                return Ok(native_editor_response(
                    &root_dir,
                    &state_path,
                    &review_path,
                    &review,
                    editor_bin.as_deref(),
                    true,
                ));
            }
            ManagedEditorLease::Acquired(lease) => native_lease = Some(lease),
        }
    }
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
    validate_state_for_session(server_owned_state, &initial_state)?;
    validate_project_paths_for_root(&root_dir, &initial_state, &allowed_source_paths)?;
    let mut existing_review = if review_path.exists() {
        Some(
            serde_json::from_slice::<ReviewState>(
                &fs::read(&review_path).context("read existing review state")?,
            )
            .context("parse existing review state")?,
        )
    } else {
        None
    };
    let relaunch_existing = server_owned_state
        && existing_review
            .as_ref()
            .is_some_and(|review| review.status == "awaiting_review" && review.action.is_none());
    if !relaunch_existing {
        atomic_json_save(&state_path, &initial_state)?;
    }
    let mut reopened_completed_review = false;
    let mut review_state = if let Some(existing) = existing_review.take() {
        // A completed review remains exportable by default. An explicit editor
        // reopen starts a new review cycle without erasing the old audit trail.
        // `request_fixes` is deliberately NOT sticky: the fix loop serves the
        // editor again after feedback is applied, and that call must mint a
        // fresh review round via the reset below.
        let mut existing = existing;
        if existing.action.as_deref() == Some("approve_export") && reopen_completed {
            let previous_session_id = existing.review_session_id.clone();
            let previous_revision = existing.revision;
            let next_revision = previous_revision
                .checked_add(1)
                .ok_or_else(|| anyhow!("cannot reopen review at the maximum revision"))?;
            let next_session_id = Uuid::new_v4().simple().to_string();
            existing.review_session_id = next_session_id.clone();
            existing.revision = next_revision;
            existing.audit.push(json!({
                "event": "review_reopened",
                "previous_review_session_id": previous_session_id,
                "previous_revision": previous_revision,
                "review_session_id": next_session_id,
                "revision": next_revision,
            }));
            reopened_completed_review = true;
        }
        existing
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
    if !relaunch_existing && !reopened_completed_review {
        review_state.revision = review_state.revision.saturating_add(1);
    }
    if !relaunch_existing {
        review_state.status = "awaiting_review".to_owned();
        review_state.action = None;
        review_state.feedback.clear();
        review_state.approved_pages.clear();
        review_state.consumed = false;
        save_review(&root_dir, &review_state)?;
    }
    // Prefer one editor surface per review. When the companion native binary
    // is available, it owns the review and writes the same review.json consumed
    // by wait_for_review/export_gate. The HTTP editor is started only when the
    // native process cannot be launched, preventing two competing UIs from
    // submitting actions for one revision.
    if let Some(editor_bin) = editor_bin {
        if let Some(lease) = native_lease.as_mut() {
            lease.update_metadata(&review_state.review_session_id, review_state.revision)?;
        }
        let lease_path = editor_lease_file(&root_dir);
        let launched = if let Some(launcher) = test_launcher {
            launcher(
                &editor_bin,
                &review_state.review_session_id,
                review_state.revision,
                &root_dir,
                &lease_path,
            )
        } else {
            launch_native_editor(
                &editor_bin,
                &review_state.review_session_id,
                review_state.revision,
                &root_dir,
                native_lease.as_ref().map(|_| lease_path.as_path()),
            )
        };
        match launched {
            Ok(child) => {
                if let Some(lease) = native_lease.take() {
                    if let Err(error) = lease.handoff_to_child(child.id()) {
                        let mut child = child;
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error);
                    }
                }
                register_native_editor(
                    root_dir.clone(),
                    review_state.review_session_id.clone(),
                    review_state.revision,
                    child,
                    lease_path,
                )?;
                return Ok(json!({
                    "editor_kind": "native",
                    "url": format!("native://{}", root_dir.display()),
                    "native_binary": editor_bin,
                    "persistence_path": state_path,
                    "session_token": Value::Null,
                    "review_session_id": review_state.review_session_id,
                    "review_revision": review_state.revision,
                    "review_path": review_path,
                }));
            }
            Err(error) => tracing::warn!(%error, "native editor launch failed; using HTTP editor"),
        }
    }
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
        server_owned_state,
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

fn is_managed_editor_root(root: &Path) -> bool {
    root.join("job.json").is_file() || root.join(".fukidashi-job.json").is_file()
}

fn editor_json_limit(server_owned_state: bool) -> usize {
    if server_owned_state {
        MAX_MANAGED_STATE_JSON
    } else {
        MAX_JSON
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
            if session.server_owned_state && session.root_dir.join("pages").is_dir() {
                let jobs_root = session
                    .root_dir
                    .parent()
                    .ok_or_else(|| anyhow!("managed editor job has no jobs root"))?;
                crate::workflow::Workflow::new(jobs_root.to_path_buf())?
                    .validate_editor_completeness(&session.root_dir, &project)
                    .map_err(|error| {
                        anyhow!("managed review completeness validation: {error:#}")
                    })?;
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
    // In-memory channel covers the HTTP loopback editor (same process). Clone the
    // channel out from under the lock first so we never hold a non-`Send`
    // `MutexGuard` across an `.await`.
    let maybe_channel = review_channels()
        .lock()
        .map_err(|_| anyhow!("review channel lock poisoned"))?
        .get(review_session_id)
        .cloned();
    if let Some(channel) = maybe_channel {
        return wait_for_review_via_channel(channel, revision, timeout_seconds).await;
    }
    // The native `fukidashi-editor` binary runs in a separate process and has no
    // in-memory channel here. Poll review.json on disk instead — the same file
    // the editor writes and export_gate reads.
    wait_for_review_via_file(review_session_id, revision, timeout_seconds).await
}

async fn wait_for_review_via_channel(
    channel: Arc<ReviewChannel>,
    revision: u64,
    timeout_seconds: u64,
) -> Result<Value> {
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

async fn wait_for_review_via_file(
    review_session_id: &str,
    revision: u64,
    timeout_seconds: u64,
) -> Result<Value> {
    let timeout = Duration::from_secs(timeout_seconds.clamp(1, 24 * 60 * 60));
    tokio::time::timeout(timeout, async {
        loop {
            // The native editor writes its review.json next to the job; we must
            // discover which job root owns this session id. The loopback server
            // records the mapping in the channel registry, but for a
            // cross-process editor we scan the review file's parent via the
            // session id recorded inside the file itself.
            if let Some(root_dir) = find_review_root_for_session(review_session_id)
                && let Some(state) = read_review(&root_dir)
            {
                if state.revision != revision {
                    bail!(
                        "stale review revision {revision}; current revision is {}",
                        state.revision
                    );
                }
                if state.action.is_some() {
                    let mut consumed = state.clone();
                    if consumed.consumed {
                        bail!("review action for this revision was already consumed");
                    }
                    consumed.consumed = true;
                    consumed.status = "consumed".to_owned();
                    consumed
                        .audit
                        .push(json!({"event":"waiter_consumed","revision":revision}));
                    let _ = save_review(&root_dir, &consumed);
                    return Ok(serde_json::json!({
                        "review_session_id": consumed.review_session_id,
                        "revision": consumed.revision,
                        "action": consumed.action,
                        "feedback": consumed.feedback,
                        "approved_pages": consumed.approved_pages,
                        "artifact_paths": [root_dir.display().to_string()],
                        "review_path": review_file(&root_dir),
                    }));
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("review wait timed out"))?
}

/// Scan candidate job roots for a `review.json` whose `review_session_id`
/// matches. We search the editor's known managed roots (under the user's jobs
/// dir) and any directory recorded by the loopback `serve_editor` calls.
fn find_review_root_for_session(session_id: &str) -> Option<PathBuf> {
    let mut visited = std::collections::HashSet::new();
    // Recently served jobs are tracked by the loopback server; mirror that set.
    recent_editor_roots()
        .into_iter()
        .find(|root| visited.insert(root.clone()) && matches_review_session(root, session_id))
}

fn matches_review_session(root: &Path, session_id: &str) -> bool {
    let path = review_file(root);
    let Ok(bytes) = fs::read(&path) else {
        return false;
    };
    let Ok(state) = serde_json::from_slice::<ReviewState>(&bytes) else {
        return false;
    };
    state.review_session_id == session_id
}

/// Roots recently passed through `serve_editor_with_allowed_sources`, so the
/// cross-process wait can locate a native editor's review.json without a global
/// filesystem scan.
fn recent_editor_roots() -> Vec<PathBuf> {
    RECENT_EDITOR_ROOTS
        .get_or_init(Default::default)
        .lock()
        .map(|set| set.iter().cloned().collect())
        .unwrap_or_default()
}

static RECENT_EDITOR_ROOTS: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();

fn track_editor_root(root: &Path) {
    let _ = RECENT_EDITOR_ROOTS
        .get_or_init(Default::default)
        .lock()
        .map(|mut set| {
            set.insert(root.to_path_buf());
        });
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

// ---------------------------------------------------------------------------
// Native editor bridge
//
// The `fukidashi-editor` binary is a **separate process** from the MCP server.
// For the cross-process review contract to hold, the editor must read and write
// the exact same `review.json` that `wait_for_review` / `export_gate` read, and
// it must render pages through the exact same logic the loopback server used.
// Everything below is the public surface the binary calls into.
// ---------------------------------------------------------------------------

/// File path (relative to the job root) of the review state written by the editor.
pub const REVIEW_FILE_NAME: &str = REVIEW_FILE;

/// Resolve the `review.json` path for a job directory, honoring the same layout
/// the loopback server used.
pub fn resolved_review_file(root: &Path) -> PathBuf {
    review_file(root)
}

/// Read the persisted `ReviewState` for a job directory.
///
/// Returns `None` when no review file exists yet (the editor has not been served).
pub fn read_review(root: &Path) -> Option<ReviewState> {
    let path = review_file(root);
    let bytes = fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist a `ReviewState` to the job directory (atomic write) and mirror it to
/// the audit file, exactly like the loopback server's `save_review`.
pub fn write_review(root: &Path, state: &ReviewState) -> Result<()> {
    validate_review_state_minimal(state)?;
    save_review(root, state)
}

fn validate_review_state_minimal(state: &ReviewState) -> Result<()> {
    if state.review_session_id.trim().is_empty() {
        bail!("review_session_id must not be empty");
    }
    if !matches!(
        state.status.as_str(),
        "new" | "awaiting_review" | "approved" | "fixes_requested" | "consumed"
    ) {
        bail!("unknown review status {:?}", state.status);
    }
    if let Some(action) = &state.action
        && !matches!(action.as_str(), "approve_export" | "request_fixes")
    {
        bail!("unknown review action {action:?}");
    }
    Ok(())
}

/// Render a single page for the native editor.
///
/// This reuses the loopback server's `render_page` logic verbatim — including
/// the brush/restore pipeline, bundled-font materialization, and the
/// `project.json` writeback — so the headless and native editors are
/// indistinguishable to downstream export.
///
/// `session_token`/`review_session_id` are accepted only to keep the on-disk
/// `project.json` shape identical; they do not open any network port.
pub fn render_editor_page(
    root_dir: &Path,
    image_path: &Path,
    state: &Value,
    index: usize,
) -> Result<Value> {
    if !root_dir.exists() {
        bail!(
            "editor job directory does not exist: {}",
            root_dir.display()
        );
    }
    let image_path = fs::canonicalize(image_path)
        .with_context(|| format!("resolve editor image {}", image_path.display()))?;
    let root_dir = managed_editor_root(root_dir);
    let allowed_source_paths = trusted_managed_source_paths(&root_dir)?;
    let session = Arc::new(Session {
        token: "native-editor".to_owned(),
        host: String::new(),
        image_path,
        root_dir: root_dir.clone(),
        state_path: root_dir.join("project.json"),
        initial_state: normalize_editor_state(state.clone())?,
        allowed_source_paths,
        server_owned_state: is_managed_editor_root(&root_dir),
        review: Arc::new(ReviewChannel {
            state: Mutex::new(ReviewState {
                review_session_id: "native".to_owned(),
                revision: 0,
                status: "awaiting_review".to_owned(),
                action: None,
                feedback: Vec::new(),
                approved_pages: Vec::new(),
                consumed: false,
                audit: Vec::new(),
            }),
            notify: Notify::new(),
            root_dir: root_dir.clone(),
        }),
    });
    let current = load_state(&session)?;
    if !state_revision_matches(&current, state) {
        bail!("stale editor state: reload the project before rendering");
    }
    validate_state_for_session(session.server_owned_state, state)
        .context("invalid editor state")?;
    validate_project_paths(&session, state).context("invalid editor paths")?;
    render_page(&session, state, index)
}

/// Reconstruct the native editor's source allowlist from the server-owned
/// managed marker.  Ad hoc editor state is never used to widen this list.
fn trusted_managed_source_paths(root_dir: &Path) -> Result<Vec<PathBuf>> {
    let has_marker =
        root_dir.join("job.json").is_file() || root_dir.join(".fukidashi-job.json").is_file();
    if !has_marker {
        return Ok(Vec::new());
    }
    let jobs_root = root_dir
        .parent()
        .ok_or_else(|| anyhow!("managed editor job has no jobs root"))?;
    crate::workflow::Workflow::new(jobs_root.to_path_buf())?
        .managed_source_paths_for_editor(root_dir)
}

/// Locate a `fukidashi-editor` executable to auto-spawn from the MCP server.
///
/// Resolution order (mirrors `which` + sibling-binary detection from the plan):
/// 1. `fukidashi-editor` / `fukidashi-editor.exe` on `PATH`.
/// 2. A binary next to the currently running `fukidashi-mcp` executable
///    (covers `target/release` and installed bundles).
pub fn find_editor_binary() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        "fukidashi-editor.exe"
    } else {
        "fukidashi-editor"
    };
    let test_process = running_under_cargo_test();
    if let Ok(found) = which::which(base)
        && !(test_process && is_development_artifact(&found))
    {
        return Some(found);
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|parent| parent.join(base)))
        .filter(|candidate| candidate.is_file());
    sibling.filter(|candidate| !(test_process && is_development_artifact(candidate)))
}

fn is_development_artifact(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("target")
    })
}

fn running_under_cargo_test() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .and_then(|parent| parent.file_name().map(|name| name.to_owned()))
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("deps"))
}

/// Re-apply correction strokes to a cleaned image and return the corrected RGB
/// buffer. Used by the native editor to render a live brush overlay (and to
/// rebuild it after undo) without touching the on-disk PNG.
pub fn build_corrected_clean_rgb(
    cleaned: &Path,
    strokes: &[serde_json::Value],
) -> Result<image::RgbImage> {
    if strokes.is_empty() {
        let base = image::open(cleaned)
            .with_context(|| format!("read cleaned image {}", cleaned.display()))?
            .to_rgb8();
        return Ok(base);
    }
    apply_correction_strokes(cleaned, strokes)
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
    let max_body = editor_json_limit(session.server_owned_state);
    if content_length > max_body {
        return respond(&mut stream, 413, "text/plain", b"request too large");
    }
    let body_start = header_end + 4;
    let mut body = bytes[body_start..].to_vec();
    if body.len() > max_body {
        return respond(&mut stream, 413, "text/plain", b"request too large");
    }
    while body.len() < content_length {
        let count = stream.read(&mut buf)?;
        if count == 0 {
            break;
        }
        body.extend_from_slice(&buf[..count]);
        if body.len() > max_body {
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
            if validate_state_for_session(session.server_owned_state, &value).is_err()
                || validate_project_paths(session, &value).is_err()
            {
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
            if validate_state_for_session(session.server_owned_state, &state).is_err()
                || validate_project_paths(session, &state).is_err()
            {
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
    let state = if session.state_path.exists() {
        let bytes = fs::read(&session.state_path).context("read editor state")?;
        normalize_editor_state(serde_json::from_slice(&bytes).context("parse editor state")?)
    } else {
        normalize_editor_state(session.initial_state.clone())
    }?;
    validate_state_for_session(session.server_owned_state, &state)?;
    Ok(state)
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

pub(crate) fn page_render_signature(page: &Value) -> Value {
    fn rect_signature(value: Option<&Value>) -> Value {
        value
            .and_then(|value| serde_json::from_value::<Rect>(value.clone()).ok())
            .map(|rect| json!([rect.x1, rect.y1, rect.x2, rect.y2]))
            .unwrap_or(Value::Null)
    }

    fn field(value: Option<&Value>) -> Value {
        let Some(value) = value.filter(|value| !value.is_null()) else {
            return Value::Null;
        };
        match value {
            Value::Number(number) => number
                .as_f64()
                .map(|number| json!(number))
                .unwrap_or(Value::Null),
            _ => value.clone(),
        }
    }

    fn text_field(value: Option<&Value>) -> Value {
        value
            .and_then(Value::as_str)
            .map(|text| Value::String(text.to_owned()))
            .unwrap_or(Value::Null)
    }

    fn has_renderer_report(bubble: &Value) -> bool {
        [
            "input_bbox",
            "safe_bbox",
            "safe_mask_bbox",
            "lines",
            "line_count",
            "ink_bbox",
            "placement_center",
            "font_runs",
            "sampled_luminance",
            "resolved_text_color",
            "resolved_font_path",
            "rendered_font_path",
            "fallback_fonts_used",
            "font_substituted",
        ]
        .iter()
        .any(|key| bubble.get(*key).is_some())
    }

    fn string_or_null(value: Option<&Value>) -> Value {
        value
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(|text| Value::String(text.to_owned()))
            .unwrap_or(Value::Null)
    }

    fn bool_or_false(value: Option<&Value>) -> Value {
        Value::Bool(value.and_then(Value::as_bool).unwrap_or(false))
    }

    fn kind_field(bubble: &Value) -> Value {
        match bubble.get("kind") {
            None => Value::Null,
            Some(Value::Null) => Value::String(String::new()),
            Some(value) => text_field(Some(value)),
        }
    }

    fn string_array(value: Option<&Value>) -> Value {
        Value::Array(
            value
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .filter(|path| !path.trim().is_empty())
                        .map(|path| Value::String(path.to_owned()))
                        .collect()
                })
                .unwrap_or_default(),
        )
    }

    fn requested_font_path(bubble: &Value) -> Value {
        if bubble
            .get("font_path")
            .and_then(Value::as_str)
            .is_some_and(|path| !path.trim().is_empty())
        {
            if let Some(path) = bubble
                .get("_editor_requested_font_path")
                .and_then(Value::as_str)
                .filter(|path| !path.trim().is_empty())
            {
                return Value::String(path.to_owned());
            }
        }
        // An editor-render payload uses the resolved path required by the
        // typesetter. Without the private request marker it is legacy report
        // state, so treating it as an operator font edit would dirty every
        // reopened page after fallback selection changed.
        if bubble
            .get("_editor_render_payload")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || has_renderer_report(bubble)
        {
            return Value::Null;
        }
        string_or_null(
            bubble
                .get("requested_font_path")
                .or_else(|| bubble.get("font_path")),
        )
    }

    fn requested_page_font_path(page: &Value) -> Value {
        if page
            .get("font_path")
            .and_then(Value::as_str)
            .is_some_and(|path| !path.trim().is_empty())
        {
            if let Some(path) = page
                .get("_editor_requested_global_font_path")
                .and_then(Value::as_str)
                .filter(|path| !path.trim().is_empty())
            {
                return Value::String(path.to_owned());
            }
        }
        string_or_null(page.get("font_path"))
    }

    fn requested_array(bubble: &Value, key: &str, marker: &str) -> Value {
        if bubble.get(marker).is_some_and(Value::is_array) {
            return string_array(bubble.get(marker));
        }
        if has_renderer_report(bubble) {
            return Value::Array(Vec::new());
        }
        string_array(bubble.get(key))
    }

    fn requested_layout_value(bubble: &Value, key: &str, marker: &str) -> Value {
        if let Some(value) = bubble.get(marker).filter(|value| !value.is_null()) {
            return field(Some(value));
        }
        if has_renderer_report(bubble) {
            return Value::Null;
        }
        field(bubble.get(key))
    }

    fn requested_font_range(bubble: &Value) -> Value {
        let min = bubble
            .get("_editor_requested_min_font_size")
            .or_else(|| bubble.get("min_font_size"));
        let max = bubble
            .get("_editor_requested_max_font_size")
            .or_else(|| bubble.get("max_font_size"));
        if bubble.get("_editor_requested_min_font_size").is_none()
            && bubble.get("_editor_requested_max_font_size").is_none()
            && has_renderer_report(bubble)
        {
            return Value::Null;
        }
        json!([
            min.and_then(Value::as_f64)
                .map(|value| json!(value))
                .unwrap_or_else(|| json!(DEFAULT_EDITOR_MIN_FONT)),
            max.and_then(Value::as_f64)
                .map(|value| json!(value))
                .unwrap_or_else(|| json!(DEFAULT_EDITOR_MAX_FONT)),
        ])
    }

    fn bubble_signature(bubble: &Value) -> Value {
        let bbox = bubble.get("bbox").or_else(|| bubble.get("bubble_bbox"));
        let bbox_rect = bbox.and_then(|value| serde_json::from_value::<Rect>(value.clone()).ok());
        let operator_geometry = bbox_rect.is_some_and(|bbox| {
            bubble
                .get("bubble_bbox")
                .and_then(|value| serde_json::from_value::<Rect>(value.clone()).ok())
                .is_some_and(|bubble_bbox| rects_match(bbox, bubble_bbox))
        });
        let preserve_source = bubble_preserve_for_render(bubble);
        json!({
            // IDs and array order are render semantics: overlapping bubbles
            // are painted in this order and tombstones address an ID.
            "id": text_field(bubble.get("id")),
            "bbox": rect_signature(bbox),
            "translation": if preserve_source { Value::Null } else {
                text_field(bubble.get("translation"))
            },
            "preserve_source": preserve_source,
            "preserve_by_default": bool_or_false(bubble.get("preserve_by_default")),
            "keep_source": bool_or_false(bubble.get("keep_source")),
            "manual": bool_or_false(
                bubble
                    .get("manual")
                    .or_else(|| bubble.get("_editor_manual")),
            ),
            "kind": kind_field(bubble),
            // Empty and absent source OCR have the same manual-cleaning
            // behavior; non-empty source text remains part of the request.
            "source_text": string_or_null(bubble.get("source_text")),
            "text_bbox": if preserve_source || operator_geometry {
                Value::Null
            } else {
                rect_signature(bubble.get("text_bbox"))
            },
            "text_color": if preserve_source { Value::Null } else {
                field(bubble.get("text_color"))
            },
            // The request font is a user input. `rendered_font_path` and the
            // report's resolved face are renderer products and are omitted.
            "font_path": if preserve_source {
                Value::Null
            } else {
                requested_font_path(bubble)
            },
            "fallback_font_paths": if preserve_source {
                Value::Null
            } else {
                requested_array(
                    bubble,
                    "fallback_font_paths",
                    "_editor_requested_fallback_font_paths",
                )
            },
            "font_range": if preserve_source {
                Value::Null
            } else {
                requested_font_range(bubble)
            },
            "shape": if preserve_source { Value::Null } else {
                Value::String(bubble.get("shape").and_then(Value::as_str).unwrap_or("ellipse").to_owned())
            },
            // `font_size` and `padding` in a reconstructed editor bubble are
            // usually fitted report values. Native edits leave an explicit
            // marker so those derived values can stay out of this identity.
            "font_size_override": requested_layout_value(
                bubble,
                "font_size",
                "_editor_font_size_override",
            ),
            "padding_override": if bubble.get("_editor_padding_override").is_some() {
                requested_layout_value(bubble, "padding", "_editor_padding_override")
            } else {
                requested_layout_value(bubble, "padding", "_editor_requested_padding")
            },
        })
    }

    fn removed_signature(value: &Value) -> Value {
        json!({
            "id": text_field(value.get("id")),
            "bbox": rect_signature(value.get("bbox").or_else(|| value.get("bubble_bbox"))),
        })
    }

    fn stroke_signature(value: &Value) -> Value {
        let points = value
            .get("points")
            .and_then(Value::as_array)
            .map(|points| {
                points
                    .iter()
                    .map(|point| json!([field(point.get("x")), field(point.get("y")),]))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        json!({
            "mode": value.get("mode").and_then(Value::as_str).unwrap_or("cover"),
            "color": value.get("color").and_then(Value::as_str).unwrap_or("#ffffff"),
            "size": value.get("size").and_then(Value::as_f64).unwrap_or(24.0),
            "points": points,
        })
    }

    let bubbles = page
        .get("bubbles")
        .and_then(Value::as_array)
        .map(|bubbles| bubbles.iter().map(bubble_signature).collect::<Vec<_>>())
        .unwrap_or_default();
    let removed_bubbles = page
        .get("removed_bubbles")
        .and_then(Value::as_array)
        .map(|bubbles| bubbles.iter().map(removed_signature).collect::<Vec<_>>())
        .unwrap_or_default();
    let correction_strokes = page
        .get("correction_strokes")
        .and_then(Value::as_array)
        .map(|strokes| strokes.iter().map(stroke_signature).collect::<Vec<_>>())
        .unwrap_or_default();
    let uses_font = page
        .get("bubbles")
        .and_then(Value::as_array)
        .is_some_and(|bubbles| {
            bubbles
                .iter()
                .any(|bubble| !bubble_preserve_for_render(bubble))
        });
    json!({
        "font_path": if uses_font {
            requested_page_font_path(page)
        } else {
            Value::Null
        },
        "bubbles": bubbles,
        "removed_bubbles": removed_bubbles,
        "correction_strokes": correction_strokes,
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
                            let explicitly_preserved = bubble
                                .get("preserve_source")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                                || bubble
                                    .get("keep_source")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                                || bubble
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
                                    .is_some_and(|id| id.starts_with("text-"));
                            let empty_translation = bubble
                                .get("translation")
                                .and_then(Value::as_str)
                                .is_none_or(|text| text.trim().is_empty());
                            bubble
                                .get("flagged")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                                || bubble
                                    .get("problem")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                                || (empty_translation && !explicitly_preserved)
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
                            let empty_translation = bubble
                                .get("translation")
                                .and_then(Value::as_str)
                                .is_none_or(|text| text.trim().is_empty());
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
                                "kind": if empty_translation && !explicit_flag { "missing_translation" } else { "flagged_bubble" },
                                "bubble_id": bubble.get("id").cloned().unwrap_or(Value::Null),
                                "bbox": bubble.get("bbox").cloned().unwrap_or(Value::Null),
                                "source_ocr": source_ocr,
                                "current_translation": current_translation,
                                "origin": if explicit_flag {
                                    if bubble
                                        .get("flagged")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false)
                                    {
                                        "bubble-flag"
                                    } else {
                                        "bubble-problem"
                                    }
                                } else {
                                    "missing-translation"
                                },
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
                        "source_text",
                        "kind",
                        "manual",
                        "preserve_by_default",
                        "keep_source",
                        "bbox",
                        "bubble_bbox",
                        "text_bbox",
                        "font_size",
                        "_editor_font_size_override",
                        "padding",
                        "_editor_padding_override",
                        "_editor_requested_padding",
                        "font_path",
                        "_editor_requested_font_path",
                        "fallback_font_paths",
                        "_editor_requested_fallback_font_paths",
                        "min_font_size",
                        "max_font_size",
                        "_editor_requested_min_font_size",
                        "_editor_requested_max_font_size",
                        "reading_order",
                        "flagged",
                        "problem",
                        "flag_reason",
                        "problem_reason",
                        "needs_review",
                        "text_color",
                        "preserve_source",
                    ] {
                        if let Some(value) = saved_bubble.get(key) {
                            base_bubble_object.insert(key.to_owned(), value.clone());
                        }
                    }
                    if !saved_bubble.contains_key("translation") {
                        // The old typeset request shape used `text` as the
                        // translation and had no OCR `source_text` or `kind`.
                        // Only that unambiguous shape gets the compatibility
                        // normalization; a modern saved bubble missing its
                        // translation remains visibly dirty.
                        if saved_bubble.get("source_text").is_none()
                            && saved_bubble.get("kind").is_none()
                            && saved_bubble.get("text").and_then(Value::as_str).is_some()
                        {
                            base_bubble_object.insert(
                                "translation".to_owned(),
                                saved_bubble.get("text").cloned().unwrap_or(Value::Null),
                            );
                        } else {
                            base_bubble_object.insert("translation".to_owned(), Value::Null);
                        }
                    }
                    // A missing or legacy renderer-only font field must not
                    // leave a current sidecar's private request marker behind
                    // after an explicit saved edit/removal.
                    if saved_bubble.get("font_path").is_none_or(Value::is_null) {
                        base_bubble_object.remove("_editor_requested_font_path");
                    }
                }
                let original_base_len = base_bubbles.len();
                let base_ids: std::collections::HashSet<String> = base_bubbles
                    .iter()
                    .filter_map(|bubble| {
                        bubble.get("id").and_then(Value::as_str).map(str::to_owned)
                    })
                    .collect();
                let original_base_ids = base_ids.clone();
                for saved_bubble in &saved_bubbles {
                    let Some(id) = saved_bubble.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    if !removed_ids.contains(id)
                        && !base_ids.contains(id)
                        && saved_bubble.get("bbox").is_some()
                    {
                        base_bubbles.push(saved_bubble.clone());
                    }
                }
                // The persisted editor order is part of the render result:
                // later bubbles paint over earlier ones. Rebuild it by saved
                // ID, then append reconstructed bubbles that the old snapshot
                // did not mention.
                let mut ordered = Vec::with_capacity(base_bubbles.len());
                let mut consumed_indices = std::collections::HashSet::new();
                for saved_bubble in &saved_bubbles {
                    let Some(id) = saved_bubble.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    if removed_ids.contains(id) || !original_base_ids.contains(id) {
                        continue;
                    }
                    if let Some((index, bubble)) =
                        base_bubbles.iter().enumerate().find(|(index, bubble)| {
                            !consumed_indices.contains(index)
                                && bubble.get("id").and_then(Value::as_str) == Some(id)
                        })
                    {
                        consumed_indices.insert(index);
                        ordered.push(bubble.clone());
                    }
                }
                for (index, bubble) in base_bubbles.iter().take(original_base_len).enumerate() {
                    if !consumed_indices.contains(&index) {
                        ordered.push(bubble.clone());
                    }
                }
                for saved_bubble in &saved_bubbles {
                    let Some(id) = saved_bubble.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    if !original_base_ids.contains(id) && !removed_ids.contains(id) {
                        ordered.push(saved_bubble.clone());
                    }
                }
                *base_bubbles = ordered;
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

/// A bubble created in the QA editor has no detector provenance.  If the
/// operator fills one with a translation, treat its rectangle as a missed
/// source-text repair region so the source is cleaned before Vietnamese is
/// rasterized over it.  Existing OCR bubbles retain `kind`/`source_text` and
/// continue to reuse their verified clean stage.
fn manual_source_repair_regions(
    bubbles: &[Value],
    removed_ids: &std::collections::HashSet<String>,
) -> Vec<Rect> {
    bubbles
        .iter()
        .filter(|bubble| {
            let id = bubble.get("id").and_then(Value::as_str);
            if id.is_some_and(|id| removed_ids.contains(id)) || bubble_preserve_for_render(bubble) {
                return false;
            }
            let explicit_manual = bubble
                .get("manual")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let source_missing = bubble
                .get("source_text")
                .and_then(Value::as_str)
                .is_none_or(|text| text.trim().is_empty());
            let detector_kind_missing = bubble.get("kind").is_none();
            let has_translation = bubble
                .get("translation")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty());
            has_translation && (explicit_manual || (source_missing && detector_kind_missing))
        })
        .filter_map(|bubble| {
            bubble
                .get("bbox")
                .cloned()
                .and_then(|value| serde_json::from_value::<Rect>(value).ok())
                .and_then(|rect| rect.validate().ok())
        })
        .collect()
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
        // Keep native request markers with the state so a reopened sidecar
        // can prove that its fitted report values came from the same edit.
        // Advisory flags remain separate and are intentionally preserved.
        object.insert("render_dirty".to_owned(), Value::Bool(false));
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
    let mut base_clean = workflow.validate_clean_input(&cleaned)?;
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
    let manual_regions = manual_source_repair_regions(bubbles, &removed_ids);
    if !manual_regions.is_empty() {
        // A newly added editor bubble may cover prose that the detector missed.
        // Clean those source pixels through the same managed LaMa crop path
        // used by strict translation before applying any Vietnamese text.  The
        // existing clean image is retained outside the manual regions so
        // already-cleaned dialogue and artwork remain untouched.
        let config = Config::resolve(&ConfigArgs {
            models_dir: None,
            ort_dylib: None,
        })?;
        let (manual_cleaned, manual_mask) = {
            let mut engine = manual_clean_engine()
                .lock()
                .map_err(|_| anyhow!("manual clean engine lock poisoned"))?;
            let result = engine
                .clean_crops(&config, &source_image, None, &manual_regions, 3, 48, 256)
                .map(|(cleaned, mask, _execution)| (cleaned, mask));
            let _ = engine.finish_heavy_call(config.session_recycle_pages());
            result.map_err(|error| anyhow!("background cleaning missed prose: {error}"))?
        };
        let mut merged_clean = image::open(&base_clean.cleaned_image)
            .with_context(|| {
                format!(
                    "read existing clean image {} before manual repair",
                    base_clean.cleaned_image.display()
                )
            })?
            .to_rgb8();
        let mut merged_mask = image::open(&base_clean.mask_path)
            .with_context(|| {
                format!(
                    "read existing clean mask {} before manual repair",
                    base_clean.mask_path.display()
                )
            })?
            .to_luma8();
        if merged_clean.dimensions() != manual_cleaned.dimensions()
            || merged_mask.dimensions() != manual_mask.dimensions()
        {
            bail!("manual clean stage dimensions do not match the managed page");
        }
        for (x, y, pixel) in manual_mask.enumerate_pixels() {
            if pixel[0] == 0 {
                continue;
            }
            merged_clean.put_pixel(x, y, *manual_cleaned.get_pixel(x, y));
            merged_mask.put_pixel(x, y, image::Luma([255]));
        }
        let (cleaned_path, _, _) = workflow.write_clean_artifact_locked(
            &source_image,
            &merged_clean,
            &merged_mask,
            3,
            "editor-manual",
        )?;
        base_clean = workflow.validate_clean_input(&cleaned_path)?;
    }
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
        let fallback_paths = bubble
            .get("_editor_requested_fallback_font_paths")
            .and_then(Value::as_array)
            .or_else(|| bubble.get("fallback_font_paths").and_then(Value::as_array));
        if let Some(paths) = fallback_paths {
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
        let font_path = bubble.get("font_path").and_then(Value::as_str);
        let requested_marker = bubble
            .get("_editor_requested_font_path")
            .and_then(Value::as_str);
        let rendered_font_path = bubble.get("rendered_font_path").and_then(Value::as_str);
        let requested_font_path = match (font_path, requested_marker, rendered_font_path) {
            (Some(current), Some(_requested), Some(_resolved))
                if crate::workflow::is_generic_desktop_font(std::path::Path::new(current)) =>
            {
                Some(current)
            }
            (Some(current), Some(requested), Some(resolved))
                if current == requested || current == resolved =>
            {
                Some(requested)
            }
            (Some(current), _, _) => Some(current),
            (None, _, Some(resolved)) => Some(resolved),
            (None, _, None) => None,
        }
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
    let mut request_bubbles = serde_json::to_value(&payloads)?;
    if let Some(requests) = request_bubbles.as_array_mut() {
        for request in requests {
            let Some(request_object) = request.as_object_mut() else {
                continue;
            };
            let bubble_id = request_object.get("id").and_then(Value::as_str);
            let source_bubble = bubbles.iter().find(|bubble| {
                bubble
                    .get("id")
                    .and_then(Value::as_str)
                    .zip(bubble_id)
                    .is_some_and(|(left, right)| left == right)
            });
            request_object.insert("_editor_render_payload".to_owned(), Value::Bool(true));
            if let Some(source_bubble) = source_bubble {
                for key in [
                    "_editor_requested_font_path",
                    "_editor_requested_fallback_font_paths",
                    "_editor_requested_min_font_size",
                    "_editor_requested_max_font_size",
                    "_editor_requested_padding",
                ] {
                    if let Some(value) = source_bubble.get(key) {
                        request_object.insert(key.to_owned(), value.clone());
                    }
                }
                if let Some(manual) = source_bubble.get("manual") {
                    request_object.insert("manual".to_owned(), manual.clone());
                }
            }
        }
    }
    let mut report = crate::typeset::typeset_page_with_fallbacks(
        &source,
        &payloads,
        &fallback_font_paths,
        &output,
    )?;
    // The typesetter is intentionally page-agnostic; attach the editor page
    // index at this boundary so persisted warnings identify both coordinates.
    let warning_bubble_indices = report
        .get("warnings")
        .and_then(Value::as_array)
        .map(|warnings| {
            warnings
                .iter()
                .filter_map(|warning| warning.get("bubble_index").and_then(Value::as_u64))
                .filter_map(|value| usize::try_from(value).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(warnings) = report.get_mut("warnings").and_then(Value::as_array_mut) {
        for warning in warnings {
            warning["page"] = Value::from(index);
        }
    }
    if let Some(bubbles) = report.get_mut("bubbles").and_then(Value::as_array_mut) {
        for bubble_index in warning_bubble_indices {
            if let Some(detail) = bubbles
                .get_mut(bubble_index)
                .and_then(|bubble| bubble.get_mut("warning"))
            {
                detail["page"] = Value::from(index);
            }
        }
    }
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
            "request_bubbles": request_bubbles,
            "report": report.clone(),
        }),
        json!({
            "editor_render": true,
            "global_font_path": state
                .get("_editor_requested_global_font_path")
                .or_else(|| state.get("font_path"))
                .cloned()
                .unwrap_or(Value::Null),
            // Keep the actual stroke semantics beside the render artifact so
            // approval can verify a reopened page without blindly rerendering
            // every page that ever used the brush. The count remains useful
            // compatibility metadata for older readers.
            "correction_strokes": strokes,
            "correction_stroke_count": strokes.len(),
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
        page["typeset_warnings"] = report
            .get("warnings")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
    }
    validate_state_for_session(session.server_owned_state, &saved_state)?;
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

fn validate_state_for_session(server_owned_state: bool, value: &Value) -> Result<()> {
    validate_state_with_limit(value, editor_json_limit(server_owned_state))
}

fn validate_state_with_limit(value: &Value, max_json: usize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > max_json {
        bail!("editor state exceeds {} bytes", max_json);
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

    fn test_managed_job() -> (tempfile::TempDir, PathBuf, Value) {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("job.json"), b"{}").expect("write managed job marker");
        let image_path = dir.path().join("page.png");
        RgbImage::from_pixel(8, 8, Rgb([255, 255, 255]))
            .save(&image_path)
            .unwrap();
        let state = json!({
            "schema_version": 1,
            "pages": [{"id": "p1", "image_path": "page.png", "bubbles": []}]
        });
        (dir, image_path, state)
    }

    fn spawn_test_editor() -> Child {
        #[cfg(windows)]
        {
            Command::new("cmd")
                .args(["/C", "ping -n 5 127.0.0.1 > nul"])
                .spawn()
                .unwrap()
        }
        #[cfg(not(windows))]
        {
            Command::new("sh").args(["-c", "sleep 1"]).spawn().unwrap()
        }
    }

    fn spawn_exited_test_editor() -> Child {
        #[cfg(windows)]
        {
            Command::new("cmd").args(["/C", "exit 0"]).spawn().unwrap()
        }
        #[cfg(not(windows))]
        {
            Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap()
        }
    }

    #[test]
    fn managed_reentrant_native_editor_reuses_one_child_and_revision() {
        let (_dir, image_path, state) = test_managed_job();
        let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let launcher: Arc<dyn Fn(&Path, &str, u64, &Path, &Path) -> Result<Child> + Send + Sync> = {
            let launches = Arc::clone(&launches);
            Arc::new(move |_, _, _, _, _| {
                launches.fetch_add(1, Ordering::SeqCst);
                Ok(spawn_test_editor())
            })
        };
        let first_launcher = Arc::clone(&launcher);
        let first_image = image_path.clone();
        let first_state = state.clone();
        let first = thread::spawn(move || {
            serve_editor_impl(
                &first_image,
                first_state,
                Vec::new(),
                false,
                Some(first_launcher.as_ref()),
            )
            .unwrap()
        });
        let second_launcher = Arc::clone(&launcher);
        let second_image = image_path.clone();
        let second_state = state.clone();
        let second = thread::spawn(move || {
            serve_editor_impl(
                &second_image,
                second_state,
                Vec::new(),
                false,
                Some(second_launcher.as_ref()),
            )
            .unwrap()
        });
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert_eq!(launches.load(Ordering::SeqCst), 1);
        assert_eq!(first["review_session_id"], second["review_session_id"]);
        assert_eq!(first["review_revision"], second["review_revision"]);
        assert_eq!(second["reused"], true);
        assert!(second["message"].as_str().unwrap().contains("already open"));
    }

    #[test]
    fn managed_dead_native_child_relaunches_same_review_revision() {
        let (_dir, image_path, state) = test_managed_job();
        let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let launcher: Arc<dyn Fn(&Path, &str, u64, &Path, &Path) -> Result<Child> + Send + Sync> = {
            let launches = Arc::clone(&launches);
            Arc::new(move |_, _, _, _, _| {
                launches.fetch_add(1, Ordering::SeqCst);
                Ok(spawn_exited_test_editor())
            })
        };
        let first = serve_editor_impl(
            &image_path,
            state.clone(),
            Vec::new(),
            false,
            Some(launcher.as_ref()),
        )
        .unwrap();
        let session = first["review_session_id"].as_str().unwrap().to_owned();
        let revision = first["review_revision"].as_u64().unwrap();
        let root = fs::canonicalize(image_path.parent().unwrap()).unwrap();
        for _ in 0..200 {
            reap_native_editor(&root);
            let active = active_native_editors().lock().unwrap().contains_key(&root);
            if !active {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let second = serve_editor_impl(
            &image_path,
            state,
            Vec::new(),
            false,
            Some(launcher.as_ref()),
        )
        .unwrap();
        assert_eq!(launches.load(Ordering::SeqCst), 2, "second={second}");
        assert_eq!(second["review_session_id"], session);
        assert_eq!(second["review_revision"], revision);
        assert_ne!(second["reused"], true);
    }

    #[test]
    fn editor_lease_create_new_is_atomic_and_recovers_dead_owner() {
        let dir = tempdir().unwrap();
        let first = create_editor_lease(editor_lease_file(dir.path()), "session-a", 7).unwrap();
        match try_reserve_editor_lease(dir.path(), "session-b", 8).unwrap() {
            EditorLeaseReservation::Active(info) => {
                assert_eq!(info.pid, std::process::id());
                assert_eq!(info.review_session_id.as_deref(), Some("session-a"));
                assert_eq!(info.revision, Some(7));
            }
            _ => panic!("second reservation unexpectedly acquired the lease"),
        }
        drop(first);
        fs::write(
            editor_lease_file(dir.path()),
            "pid=4294967295\nreview_session_id=stale\nrevision=1\n",
        )
        .unwrap();
        let recovered = match try_reserve_editor_lease(dir.path(), "session-b", 8).unwrap() {
            EditorLeaseReservation::Acquired(lease) => lease,
            _ => panic!("dead lease was not reclaimed"),
        };
        drop(recovered);
    }

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
    fn manual_bubbles_are_marked_for_background_cleaning() {
        let removed = std::collections::HashSet::new();
        let regions = manual_source_repair_regions(
            &[
                json!({
                    "id": "bubble-manual",
                    "bbox": {"x1": 2.0, "y1": 3.0, "x2": 20.0, "y2": 24.0},
                    "source_text": "",
                    "translation": "Bản dịch thủ công",
                }),
                json!({
                    "id": "bubble-detected",
                    "kind": "dialogue",
                    "source_text": "Detected source",
                    "translation": "Bản dịch đã nhận dạng",
                    "bbox": {"x1": 1.0, "y1": 1.0, "x2": 10.0, "y2": 10.0},
                }),
                json!({
                    "id": "bubble-empty",
                    "manual": true,
                    "bbox": {"x1": 1.0, "y1": 1.0, "x2": 10.0, "y2": 10.0},
                    "translation": "",
                }),
            ],
            &removed,
        );
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].x1, 2.0);
        assert_eq!(regions[0].y2, 24.0);
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
    fn managed_state_budget_is_bounded_without_expanding_ad_hoc_budget() {
        let state = json!({
            "schema_version": 1,
            "pages": [],
            "metadata": "x".repeat(MAX_JSON + 1),
        });
        assert!(validate_state_for_session(false, &state).is_err());
        assert!(validate_state_for_session(true, &state).is_ok());

        let oversized = json!({
            "schema_version": 1,
            "pages": [],
            "metadata": "x".repeat(MAX_MANAGED_STATE_JSON + 1),
        });
        assert!(validate_state_for_session(true, &oversized).is_err());
    }

    #[test]
    fn managed_editor_root_requires_a_known_job_marker() {
        let dir = tempdir().unwrap();
        assert!(!is_managed_editor_root(dir.path()));

        fs::write(dir.path().join("job.json"), b"{}").unwrap();
        assert!(is_managed_editor_root(dir.path()));

        fs::remove_file(dir.path().join("job.json")).unwrap();
        fs::write(dir.path().join(".fukidashi-job.json"), b"{}").unwrap();
        assert!(is_managed_editor_root(dir.path()));
    }

    #[test]
    fn request_and_save_validation_share_the_managed_state_budget() {
        let state = json!({
            "state_revision": 0,
            "pages": [],
            "metadata": "x".repeat(MAX_JSON + 1),
        });
        let body = serde_json::to_vec(&state).unwrap();
        assert!(body.len() > MAX_JSON);
        assert!(body.len() < MAX_MANAGED_STATE_JSON);
        assert_eq!(editor_json_limit(false), MAX_JSON);
        assert_eq!(editor_json_limit(true), MAX_MANAGED_STATE_JSON);
        assert!(body.len() > editor_json_limit(false));
        assert!(body.len() <= editor_json_limit(true));
        assert!(validate_state_for_session(false, &state).is_err());
        assert!(validate_state_for_session(true, &state).is_ok());
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
    fn render_dirty_bookkeeping_is_not_part_of_the_render_signature() {
        let clean = json!({
            "bubbles": [{"id": "bubble-1", "bbox": {"x1": 1, "y1": 1, "x2": 8, "y2": 8}}]
        });
        let mut persisted = clean.clone();
        persisted["bubbles"][0]["render_dirty"] = json!(false);
        assert_eq!(
            page_render_signature(&clean),
            page_render_signature(&persisted)
        );
    }

    #[test]
    fn render_signature_uses_semantic_inputs_and_ignores_report_layout() {
        let mut saved = json!({
            "render_dirty": true,
            "bubbles": [{
                "id": "bubble-1",
                "source_text": "OCR source",
                "kind": "dialogue",
                "preserve_source": false,
                "preserve_by_default": false,
                "text": "Bản dịch",
                "translation": "Bản dịch",
                "bbox": {"x1": 10.0, "y1": 12.0, "x2": 90.0, "y2": 70.0},
                "bubble_bbox": {"x1": 10.0, "y1": 12.0, "x2": 90.0, "y2": 70.0},
                "text_bbox": {"x1": 14.0, "y1": 16.0, "x2": 86.0, "y2": 66.0},
                "font_path": "fonts/requested.ttf",
                "min_font_size": 8.0,
                "max_font_size": 72.0,
                "text_color": "white",
                "shape": "ellipse",
                "font_size": 18.0,
                "padding": 4.0
            }]
        });
        let sidecar = json!({
            "bubbles": [{
                "id": "bubble-1",
                "source_text": "OCR source",
                "kind": "dialogue",
                "preserve_source": false,
                "preserve_by_default": false,
                "text": "Bản dịch",
                "translation": "Bản dịch",
                "bbox": {"x1": 10, "y1": 12, "x2": 90, "y2": 70},
                "bubble_bbox": {"x1": 10, "y1": 12, "x2": 90, "y2": 70},
                "text_bbox": {"x1": 14, "y1": 16, "x2": 86, "y2": 66},
                "font_path": "fonts/requested.ttf",
                "min_font_size": 8,
                "max_font_size": 72,
                "text_color": "white",
                "shape": "ellipse",
                "font_size": 23.5,
                "padding": 7.0,
                "input_bbox": {"x1": 10, "y1": 12, "x2": 90, "y2": 70},
                "safe_bbox": {"x1": 18, "y1": 20, "x2": 82, "y2": 62},
                "safe_mask_bbox": {"x1": 19, "y1": 21, "x2": 81, "y2": 61},
                "lines": ["Bản dịch"],
                "line_count": 1,
                "ink_bbox": {"x1": 20, "y1": 22, "x2": 60, "y2": 40},
                "placement_center": {"x": 50, "y": 41},
                "resolved_font_path": "fonts/resolved-fallback.ttf",
                "fallback_font_paths": ["fonts/resolved-fallback.ttf"],
                "fallback_fonts_used": ["fonts/resolved-fallback.ttf"],
                "font_runs": [{"font_index": 1, "text": "Bản dịch"}],
                "sampled_luminance": 38,
                "resolved_text_color": "white",
                "render_dirty": false,
                "needs_review": false
            }],
            "removed_bubbles": [],
            "correction_strokes": []
        });
        // The only differences below are fitted/report values and legacy
        // bookkeeping. They must not make an already rendered page dirty.
        saved["bubbles"][0]["font_size"] = json!(11.5);
        saved["bubbles"][0]["padding"] = json!(2.0);
        saved["bubbles"][0]["rendered_font_path"] = json!("fonts/resolved-fallback.ttf");
        saved["bubbles"][0]["safe_bbox"] = json!({"x1": 99, "y1": 99, "x2": 100, "y2": 100});
        saved["bubbles"][0]
            .as_object_mut()
            .unwrap()
            .remove("translation");
        // A missing translation is not equivalent to the request's `text`;
        // treating it as a fallback would hide a lost user edit.
        assert_ne!(
            page_render_signature(&saved),
            page_render_signature(&sidecar)
        );

        saved["bubbles"][0]["translation"] = json!("Edited");
        assert_ne!(
            page_render_signature(&saved),
            page_render_signature(&sidecar)
        );
        saved["bubbles"][0]["translation"] = json!("Bản dịch");
        saved["bubbles"][0]["_editor_font_size_override"] = json!(12.0);
        assert_ne!(
            page_render_signature(&saved),
            page_render_signature(&sidecar)
        );
    }

    #[test]
    fn render_signature_preserves_bubble_identity_order_and_legacy_override_rule() {
        let bubble = |id: &str| {
            json!({
                "id": id,
                "translation": "dịch",
                "source_text": "source",
                "kind": "dialogue",
                "bbox": {"x1": 1, "y1": 1, "x2": 8, "y2": 8},
            })
        };
        let first = json!({"bubbles": [bubble("a"), bubble("b")]});
        let reordered = json!({"bubbles": [bubble("b"), bubble("a")]});
        assert_ne!(
            page_render_signature(&first),
            page_render_signature(&reordered)
        );

        let mut missing_translation = first.clone();
        missing_translation["bubbles"][0]
            .as_object_mut()
            .unwrap()
            .remove("translation");
        assert_ne!(
            page_render_signature(&first),
            page_render_signature(&missing_translation)
        );

        let mut legacy_baseline = json!({"bubbles": [bubble("a")]});
        legacy_baseline["bubbles"][0]["safe_bbox"] = json!({"x1": 0, "y1": 0, "x2": 8, "y2": 8});
        let mut legacy = legacy_baseline.clone();
        legacy["bubbles"][0]["font_path"] = json!("fonts/old-resolved.ttf");
        legacy["bubbles"][0]["rendered_font_path"] = json!("fonts/new-resolved.ttf");
        legacy["bubbles"][0]["font_size"] = json!(12.0);
        legacy["bubbles"][0]["padding"] = json!(2.0);
        legacy["bubbles"][0]["safe_bbox"] = json!({"x1": 0, "y1": 0, "x2": 8, "y2": 8});
        assert_eq!(
            page_render_signature(&legacy_baseline),
            page_render_signature(&legacy)
        );

        let mut genuine_override = first.clone();
        genuine_override["bubbles"][0]["font_size"] = json!(12.0);
        assert_ne!(
            page_render_signature(&first),
            page_render_signature(&genuine_override)
        );
        let mut font_input = first.clone();
        font_input["bubbles"][0]["font_path"] = json!("fonts/operator.ttf");
        assert_ne!(
            page_render_signature(&first),
            page_render_signature(&font_input)
        );
        let mut global_font = first.clone();
        global_font["font_path"] = json!("fonts/global.ttf");
        assert_ne!(
            page_render_signature(&first),
            page_render_signature(&global_font)
        );
    }

    #[test]
    fn merge_saved_edits_keeps_saved_bubble_order_then_appends_new_bubbles() {
        let bbox = json!({"x1": 1, "y1": 1, "x2": 8, "y2": 8});
        let mut base = normalize_editor_state(json!({
            "pages": [{
                "id": "page-1",
                "bubbles": [
                    {"id": "a", "bbox": bbox.clone(), "translation": "A"},
                    {"id": "b", "bbox": bbox.clone(), "translation": "B"},
                    {"id": "c", "bbox": bbox.clone(), "translation": "C"}
                ]
            }]
        }))
        .unwrap();
        let saved = json!({
            "pages": [{
                "id": "page-1",
                "bubbles": [
                    {"id": "b", "bbox": bbox.clone(), "translation": "B2"},
                    {"id": "a", "bbox": bbox.clone(), "translation": "A2"},
                    {"id": "d", "bbox": bbox.clone(), "translation": "D"}
                ]
            }]
        });
        merge_saved_edits(&mut base, &saved);
        let ids = base["pages"][0]["bubbles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|bubble| bubble["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["b", "a", "c", "d"]);
        assert_eq!(base["pages"][0]["bubbles"][0]["translation"], "B2");
    }

    #[test]
    fn merge_saved_edits_only_normalizes_known_legacy_translation_shape() {
        let bbox = json!({"x1": 1, "y1": 1, "x2": 8, "y2": 8});
        let mut legacy_base = normalize_editor_state(json!({
            "pages": [{
                "id": "page-1",
                "bubbles": [{"id": "a", "bbox": bbox.clone(), "translation": "dịch"}]
            }]
        }))
        .unwrap();
        merge_saved_edits(
            &mut legacy_base,
            &json!({
                "pages": [{
                    "id": "page-1",
                    "bubbles": [{"id": "a", "bbox": bbox.clone(), "text": "dịch"}]
                }]
            }),
        );
        assert_eq!(legacy_base["pages"][0]["render_dirty"], false);

        let mut modern_base = normalize_editor_state(json!({
            "pages": [{
                "id": "page-1",
                "bubbles": [{"id": "a", "bbox": bbox.clone(), "translation": "dịch"}]
            }]
        }))
        .unwrap();
        merge_saved_edits(
            &mut modern_base,
            &json!({
                "pages": [{
                    "id": "page-1",
                    "bubbles": [{
                        "id": "a",
                        "bbox": bbox.clone(),
                        "source_text": "ocr"
                    }]
                }]
            }),
        );
        assert_eq!(modern_base["pages"][0]["render_dirty"], true);
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

    #[test]
    fn native_editor_resolution_ignores_cargo_target_binaries() {
        assert!(is_development_artifact(Path::new(
            "C:/work/target/debug/fukidashi-editor.exe"
        )));
        assert!(!is_development_artifact(Path::new(
            "C:/Users/me/AppData/Local/Fukidashi/bin/fukidashi-editor.exe"
        )));
    }
}
