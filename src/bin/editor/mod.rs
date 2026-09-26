//! Native `egui` QA editor application module.

#![allow(
    clippy::bind_instead_of_map,
    clippy::collapsible_if,
    clippy::items_after_test_module,
    clippy::let_and_return,
    dead_code
)]

pub mod canvas;
pub mod gallery;
pub mod inspector;
pub mod render;
pub mod state;
pub mod toolbar;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, ColorImage, Context, TextureHandle, TextureOptions};
use fukidashi_mcp::editor::{ReviewState, read_review};

use state::{EditorState, PageView};

/// Exit decision written by the inspector buttons and read by `main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitAction {
    Approve,
    RequestFixes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorOperationKind {
    SaveRender,
    RefreshCachedPages,
    Approve,
}

impl EditorOperationKind {
    fn label(self) -> &'static str {
        match self {
            Self::SaveRender => "Save & Render",
            Self::RefreshCachedPages => "Refresh cached pages",
            Self::Approve => "Approve & Export",
        }
    }
}

enum OperationEvent {
    Progress {
        current: usize,
        total: usize,
        message: String,
    },
    Succeeded(Box<OperationSuccess>),
    Failed {
        kind: EditorOperationKind,
        error: String,
    },
}

struct OperationRuntime {
    kind: EditorOperationKind,
    rx: std::sync::mpsc::Receiver<OperationEvent>,
    handle: Option<std::thread::JoinHandle<()>>,
    current: usize,
    total: usize,
    message: String,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

struct OperationRequest {
    kind: EditorOperationKind,
    job_dir: PathBuf,
    state_value: serde_json::Value,
    current_page: usize,
    review: ReviewState,
    save_epoch: Arc<Mutex<u64>>,
    save_lock: Arc<Mutex<()>>,
    repaint: Context,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

pub struct EditorApp {
    /// `None` only transiently during construction of the surrounding `Box`.
    state: Option<EditorState>,
    review: ReviewState,
    job_dir: PathBuf,
    current_page: usize,
    page_jump_input: String,
    /// One-shot request consumed by the gallery after a page-number jump.
    /// Ordinary page navigation leaves the gallery's existing scroll position
    /// alone so browsing does not keep moving the thumbnail list.
    gallery_scroll_target: Option<usize>,
    /// Parsed page-number jumps are applied at the start of the next frame.
    /// Keeping the request separate from the text field prevents the top-bar
    /// widget's focus/value synchronization from restoring the old page
    /// after Go or Enter was handled.
    pending_page_jump: Option<usize>,
    canvas: canvas::CanvasState,
    page_textures: TextureCache,
    thumb_textures: TextureCache,
    thumbnail_decodes: usize,
    dirty_since: Option<Instant>,
    /// Sender for the background autosave writer: `(epoch, project_value)`.
    /// The epoch is the sync-save counter at send time; the writer skips any
    /// queued snapshot whose epoch is behind the latest explicit save, so a
    /// slow background write can never clobber a newer synchronous one.
    save_tx: std::sync::mpsc::SyncSender<(u64, serde_json::Value)>,
    /// Counter bumped by every synchronous save; shared with the writer.
    save_epoch: Arc<Mutex<u64>>,
    /// Set while the background writer owns a queued snapshot. Close requests
    /// wait for this flag to clear before allowing the window to exit.
    save_pending: Arc<std::sync::atomic::AtomicBool>,
    /// Background persistence failures are handed back to the UI rather than
    /// being silently discarded by the writer thread.
    save_error: Arc<Mutex<Option<String>>>,
    /// Error state is published before `save_pending` is cleared. Close
    /// handling checks both atomics so a failure cannot slip between the
    /// frame's error poll and its close decision.
    save_failed: Arc<std::sync::atomic::AtomicBool>,
    /// Serializes every project write, including the autosave writer and the
    /// explicit worker. This prevents a stale debounce snapshot from landing
    /// in the middle of a render or approval operation.
    save_lock: Arc<Mutex<()>>,
    /// At most one heavy editor operation is active. The UI only polls this
    /// channel; all disk/render work happens on the worker thread.
    operation: Option<OperationRuntime>,
    exit_action: Option<ExitAction>,
    close_after_operation: bool,
    close_after_save: bool,
    close_ready: bool,
    error_message: Option<String>,
    error_guidance: Option<String>,
    error_target: Option<(usize, usize)>,
    exit_state: Arc<Mutex<crate::ExitState>>,
    /// egui context, refreshed each frame in `update`.
    context: Option<Context>,
    pub icon_textures: std::collections::HashMap<&'static str, egui::TextureHandle>,
    notify_message: Option<(String, std::time::Instant)>,
    /// Last cached-page reconciliation result, kept visible after its toast
    /// expires so an operator can record reused/dirty counts.
    cache_plan_status: Option<String>,
    /// Bounded snapshots for operator mutations. A gesture keeps its
    /// pre-mutation snapshot pending until it commits, so Escape can cancel
    /// without creating an undo entry.
    undo_history: VecDeque<serde_json::Value>,
    redo_history: VecDeque<serde_json::Value>,
    pending_history: Option<serde_json::Value>,
}

const MAX_OPERATOR_HISTORY: usize = 64;
const PAGE_TEXTURE_CACHE_CAPACITY: usize = 2;
// Enough for a tall desktop viewport while remaining a fixed, small GPU
// footprint: 32 thumbnails at 160px are roughly 3.3 MiB of RGBA pixels.
const THUMB_TEXTURE_CACHE_CAPACITY: usize = 32;
const THUMBNAIL_MAX_EDGE: u32 = 160;
pub(crate) const THUMBNAIL_ROW_HEIGHT: f32 = 100.0;
const AUTOSAVE_BUSY_RETRIES: usize = 40;
const AUTOSAVE_BUSY_DELAY: Duration = Duration::from_millis(50);
const WORKER_RENDER_LOCK_RETRY_DELAY: Duration = Duration::from_millis(50);

fn is_transient_autosave_error(error: &anyhow::Error) -> bool {
    error.to_string().contains("busy: another process owns")
}

fn overflow_bubble_id(error: &str) -> Option<String> {
    let marker = "translated bubble \"";
    let tail = error.split_once(marker)?.1;
    let (bubble_id, rest) = tail.split_once('\"')?;
    (rest.contains("text_overflow") && !bubble_id.is_empty()).then(|| bubble_id.to_owned())
}

fn overflow_number_after(error: &str, marker: &str) -> Option<usize> {
    let tail = error.split_once(marker)?.1;
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn overflow_page_index(error: &str) -> Option<usize> {
    overflow_number_after(error, "page ")?.checked_sub(1)
}

fn overflow_bubble_index(error: &str) -> Option<usize> {
    overflow_number_after(error, "at index ")
}

fn is_busy_render_lease_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("busy: another process owns")
        || message.contains("timed out waiting for another process to finish render job")
}

fn set_autosave_error(
    slot: &Arc<Mutex<Option<String>>>,
    failed: &Arc<std::sync::atomic::AtomicBool>,
    message: String,
) {
    if let Ok(mut error) = slot.lock() {
        *error = Some(message);
    }
    failed.store(true, std::sync::atomic::Ordering::Release);
}

/// Small LRU cache for GPU textures. The gallery can represent thousands of
/// pages, but only the visible thumbnails and the current/previous canvas page
/// need to stay resident. Dropping the handle releases the egui texture after
/// the frame, so memory use follows the bound instead of the project size.
struct TextureCache {
    textures: HashMap<usize, TextureHandle>,
    order: VecDeque<usize>,
    capacity: usize,
}

impl TextureCache {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            textures: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    fn get(&mut self, index: usize) -> Option<TextureHandle> {
        if self.textures.contains_key(&index) {
            self.touch(index);
            self.textures.get(&index).cloned()
        } else {
            None
        }
    }

    fn insert(&mut self, index: usize, texture: TextureHandle) {
        self.textures.insert(index, texture);
        self.touch(index);
        while self.textures.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.textures.remove(&oldest);
        }
    }

    fn remove(&mut self, index: usize) {
        self.textures.remove(&index);
        self.order.retain(|entry| *entry != index);
    }

    fn clear(&mut self) {
        self.textures.clear();
        self.order.clear();
    }

    fn len(&self) -> usize {
        self.textures.len()
    }

    fn touch(&mut self, index: usize) {
        self.order.retain(|entry| *entry != index);
        self.order.push_back(index);
    }
}

fn push_history_snapshot(history: &mut VecDeque<serde_json::Value>, snapshot: serde_json::Value) {
    history.push_back(snapshot);
    while history.len() > MAX_OPERATOR_HISTORY {
        history.pop_front();
    }
}

impl EditorApp {
    pub fn new(
        job_dir: PathBuf,
        review_session_id: Option<String>,
        exit_state: Arc<Mutex<crate::ExitState>>,
    ) -> anyhow::Result<Self> {
        let project_path = job_dir.join("project.json");
        if !project_path.exists() {
            anyhow::bail!(
                "job directory {} has no project.json; run the MCP pipeline first",
                job_dir.display()
            );
        }
        let bytes =
            std::fs::read(&project_path).map_err(|e| anyhow::anyhow!("read project.json: {e}"))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("parse project.json: {e}"))?;

        let image_path = value
            .get("pages")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|p| p.get("image_path"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("project.json has no first-page image_path"))?;
        let image_path = if image_path.is_absolute() {
            image_path
        } else {
            job_dir.join(&image_path)
        };

        let state = EditorState {
            value,
            job_dir: job_dir.clone(),
            image_path,
        };
        let review = state::load_or_create_review(&job_dir, &review_session_id);
        let (save_tx, save_rx) = std::sync::mpsc::sync_channel::<(u64, serde_json::Value)>(1);
        let save_epoch: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let save_lock: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let save_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let save_error = Arc::new(Mutex::new(None));
        let save_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Background autosave writer. `atomic_write_json` fsyncs, which can
        // block for seconds on slow disks — that must never happen on the egui
        // UI thread, so debounced saves are handed off here. `try_send` drops
        // the snapshot when the writer is still busy with an older one (the
        // next debounce fires anyway).
        {
            let writer_dir = job_dir.clone();
            let writer_epoch = Arc::clone(&save_epoch);
            let writer_lock = Arc::clone(&save_lock);
            let writer_pending = Arc::clone(&save_pending);
            let writer_error = Arc::clone(&save_error);
            let writer_failed = Arc::clone(&save_failed);
            std::thread::Builder::new()
                .name("editor-autosave".to_owned())
                .spawn(move || {
                    while let Ok((queued_epoch, value)) = save_rx.recv() {
                        writer_pending.store(true, std::sync::atomic::Ordering::Release);
                        // Keep lock order consistent with approval/render
                        // workers: acquire the cross-process job lease before
                        // the process-local save mutex. Busy is transient, but
                        // filesystem and poisoned-lock failures are surfaced
                        // immediately instead of retrying forever.
                        let mut attempts = 0;
                        let render_lock = loop {
                            match fukidashi_mcp::editor::try_acquire_editor_render_lock(&writer_dir)
                            {
                                Ok(lock) => break Some(lock),
                                Err(error)
                                    if is_transient_autosave_error(&error)
                                        && attempts < AUTOSAVE_BUSY_RETRIES =>
                                {
                                    attempts += 1;
                                    std::thread::sleep(AUTOSAVE_BUSY_DELAY);
                                }
                                Err(error) => {
                                    set_autosave_error(
                                        &writer_error,
                                        &writer_failed,
                                        format!("{error:#}"),
                                    );
                                    break None;
                                }
                            }
                        };
                        let Some(_render_lock) = render_lock else {
                            writer_pending.store(false, std::sync::atomic::Ordering::Release);
                            continue;
                        };
                        let Ok(_save_guard) = writer_lock.lock() else {
                            set_autosave_error(
                                &writer_error,
                                &writer_failed,
                                "editor save lock poisoned".to_owned(),
                            );
                            writer_pending.store(false, std::sync::atomic::Ordering::Release);
                            continue;
                        };
                        // Skip snapshots superseded by a later synchronous save.
                        let superseded = match writer_epoch.lock() {
                            Ok(epoch) => *epoch > queued_epoch,
                            Err(_) => {
                                set_autosave_error(
                                    &writer_error,
                                    &writer_failed,
                                    "editor save epoch lock poisoned".to_owned(),
                                );
                                writer_pending.store(false, std::sync::atomic::Ordering::Release);
                                continue;
                            }
                        };
                        if superseded {
                            writer_pending.store(false, std::sync::atomic::Ordering::Release);
                            continue;
                        }
                        if let Err(error) = ensure_persisted_revision(&writer_dir, &value) {
                            set_autosave_error(&writer_error, &writer_failed, format!("{error:#}"));
                        } else {
                            let path = writer_dir.join("project.json");
                            if let Err(error) = state::atomic_write_json(&path, &value) {
                                set_autosave_error(
                                    &writer_error,
                                    &writer_failed,
                                    format!("{error:#}"),
                                );
                            }
                        }
                        writer_pending.store(false, std::sync::atomic::Ordering::Release);
                    }
                })
                .expect("spawn editor autosave thread");
        }
        Ok(EditorApp {
            state: Some(state),
            review,
            job_dir,
            current_page: 0,
            page_jump_input: "1".to_owned(),
            gallery_scroll_target: None,
            pending_page_jump: None,
            canvas: canvas::CanvasState::default(),
            page_textures: TextureCache::with_capacity(PAGE_TEXTURE_CACHE_CAPACITY),
            thumb_textures: TextureCache::with_capacity(THUMB_TEXTURE_CACHE_CAPACITY),
            thumbnail_decodes: 0,
            dirty_since: None,
            save_tx,
            save_epoch,
            save_lock,
            save_pending,
            save_error,
            save_failed,
            operation: None,
            exit_action: None,
            close_after_operation: false,
            close_after_save: false,
            close_ready: false,
            error_message: None,
            error_guidance: None,
            error_target: None,
            exit_state,
            context: None,
            icon_textures: std::collections::HashMap::new(),
            notify_message: None,
            cache_plan_status: None,
            undo_history: VecDeque::new(),
            redo_history: VecDeque::new(),
            pending_history: None,
        })
    }

    fn ctx(&self) -> Context {
        self.context
            .clone()
            .expect("egui context available during update")
    }

    fn current_page_view(&self) -> Option<PageView> {
        self.state.as_ref()?.page(self.current_page)
    }

    /// Select a page and keep all page-navigation UI in sync. Page indices are
    /// zero-based internally, while the gallery and jump control are 1-based.
    fn set_current_page(&mut self, page_index: usize) {
        let page_count = self
            .state
            .as_ref()
            .map(EditorState::page_count)
            .unwrap_or(0);
        let next = page_index.min(page_count.saturating_sub(1));
        if next != self.current_page {
            self.cancel_drag();
            self.current_page = next;
            self.canvas.selected = None;
            self.canvas.selected_issue = None;
            self.canvas.brush_overlay_dirty = true;
            self.canvas.fit_applied = false;
        }
        self.page_jump_input = (next + 1).to_string();
    }

    fn jump_to_page_input(&mut self) {
        let page_count = self
            .state
            .as_ref()
            .map(EditorState::page_count)
            .unwrap_or(0);
        let next = page_index_from_input(&self.page_jump_input, page_count, self.current_page);
        self.pending_page_jump = Some(next);
    }

    fn apply_pending_page_jump(&mut self) {
        let Some(next) = self.pending_page_jump.take() else {
            return;
        };
        self.set_current_page(next);
        self.gallery_scroll_target = Some(next);
    }

    fn load_texture_for(&mut self, path: &PathBuf, key: &str) -> TextureHandle {
        let expected = format!("page-{key}");
        if self
            .page_textures
            .get(self.current_page)
            .is_some_and(|tex| tex.name() == expected)
        {
            return self.page_textures.get(self.current_page).unwrap();
        }
        {
            let tex = self.load_image_texture(path, &expected);
            self.page_textures.insert(self.current_page, tex);
        }
        self.page_textures.get(self.current_page).unwrap()
    }

    fn load_image_texture(&self, path: &PathBuf, name: &str) -> TextureHandle {
        let ctx = self.ctx();
        match image::open(path).and_then(|img| Ok(img.to_rgba8())) {
            Ok(rgba) => {
                let size = [rgba.width() as usize, rgba.height() as usize];
                let pixels = rgba.into_raw();
                let color_image = ColorImage::from_rgba_unmultiplied(size, &pixels);
                ctx.load_texture(name, color_image, TextureOptions::default())
            }
            Err(_) => {
                let color_image = ColorImage::from_rgba_unmultiplied([1, 1], &[200, 200, 200, 255]);
                ctx.load_texture(name, color_image, TextureOptions::default())
            }
        }
    }

    fn load_thumbnail(&mut self, index: usize, page: &PageView) -> Option<TextureHandle> {
        if let Some(existing) = self.thumb_textures.get(index) {
            return Some(existing.clone());
        }
        let path = page
            .rendered_image_path
            .clone()
            .or_else(|| Some(page.cleaned_image_path.clone()))
            .filter(|p| p.exists())
            .or_else(|| Some(page.source_image_path.clone()))?;
        self.thumbnail_decodes = self.thumbnail_decodes.saturating_add(1);
        let tex = self.load_thumbnail_texture(&path, &format!("thumb-{index}"));
        self.thumb_textures.insert(index, tex.clone());
        Some(tex)
    }

    fn load_thumbnail_texture(&self, path: &PathBuf, name: &str) -> TextureHandle {
        let ctx = self.ctx();
        match image::open(path) {
            Ok(image) => {
                let rgba = image
                    .thumbnail(THUMBNAIL_MAX_EDGE, THUMBNAIL_MAX_EDGE)
                    .to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let pixels = rgba.into_raw();
                let color_image = ColorImage::from_rgba_unmultiplied(size, &pixels);
                ctx.load_texture(name, color_image, TextureOptions::default())
            }
            Err(_) => {
                let color_image = ColorImage::from_rgba_unmultiplied([1, 1], &[200, 200, 200, 255]);
                ctx.load_texture(name, color_image, TextureOptions::default())
            }
        }
    }

    fn clear_texture_caches(&mut self) {
        self.page_textures.clear();
        self.thumb_textures.clear();
    }

    fn invalidate_page_textures(&mut self, index: usize) {
        self.page_textures.remove(index);
        self.thumb_textures.remove(index);
    }

    fn load_brush_overlay(&mut self, page: &PageView) -> TextureHandle {
        let name = format!("brush-overlay-{}", self.current_page);
        let rebuild = self.canvas.brush_overlay_dirty || self.canvas.brush_overlay.is_none();
        if rebuild {
            let strokes: Vec<serde_json::Value> = page
                .correction_strokes
                .iter()
                .map(|s| s.to_value())
                .collect();
            let corrected = fukidashi_mcp::editor::build_corrected_clean_rgb(
                &page.cleaned_image_path,
                &strokes,
            )
            .unwrap_or_else(|_| {
                image::open(&page.cleaned_image_path)
                    .map(|i| i.to_rgb8())
                    .unwrap_or_else(|_| image::RgbImage::new(1, 1))
            });
            let rgba = image::DynamicImage::ImageRgb8(corrected).to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let pixels = rgba.into_raw();
            let color_image = ColorImage::from_rgba_unmultiplied(size, &pixels);
            let tex = self
                .ctx()
                .load_texture(&name, color_image, TextureOptions::default());
            self.canvas.brush_overlay = Some(tex);
            self.canvas.brush_overlay_dirty = false;
        }
        self.canvas.brush_overlay.clone().unwrap()
    }

    fn schedule_save(&mut self) {
        if self.dirty_since.is_none() {
            self.dirty_since = Some(Instant::now());
        }
    }

    /// Synchronous save retained for the small review/exit paths. Heavy render
    /// and approval actions use the operation worker below.
    fn save_project_sync(&self, state: &EditorState) -> anyhow::Result<()> {
        // The egui thread must not wait behind a long export lease. Autosave
        // and worker operations use the bounded waiting helper; this small
        // synchronous path fails fast and leaves the debounced snapshot for
        // the background writer to retry.
        let _render_lock = fukidashi_mcp::editor::try_acquire_editor_render_lock(&state.job_dir)
            .map_err(|error| anyhow::anyhow!("editor render lease failed: {error:#}"))?;
        let _save_guard = self
            .save_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("editor save lock poisoned"))?;
        ensure_persisted_revision(&state.job_dir, &state.value)?;
        let result = state::atomic_write_json(&state.job_dir.join("project.json"), &state.value);
        if result.is_ok() {
            let mut epoch = self
                .save_epoch
                .lock()
                .map_err(|_| anyhow::anyhow!("editor save epoch lock poisoned"))?;
            *epoch = epoch.saturating_add(1);
        }
        result
    }

    fn flush_save_if_due(&mut self) {
        self.poll_autosave_error();
        // The explicit worker owns the project snapshot until it completes.
        // Queueing the pre-operation UI state here could overwrite a
        // normalized render result after the worker exits.
        if self.operation_active() {
            return;
        }
        if let Some(since) = self.dirty_since {
            if since.elapsed() >= Duration::from_millis(800) {
                // The bounded channel may accept one more snapshot while the
                // writer is still persisting the first one. Do not enqueue
                // that second snapshot: the writer clears the single pending
                // flag when the first write finishes, which would otherwise
                // let close handling mistake the second queued edit for a
                // durable save. Keep the edit dirty and retry next frame.
                if self.save_pending.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                // Snapshot the epoch alongside the value: if a synchronous
                // save lands after this snapshot was taken, the writer sees a
                // newer epoch and discards the stale snapshot.
                let queued_epoch = *self.save_epoch.lock().unwrap();
                let Some(state) = self.state.as_ref() else {
                    self.error_message =
                        Some("autosave failed: editor state is unavailable".to_owned());
                    self.dirty_since = Some(Instant::now());
                    return;
                };
                self.save_pending
                    .store(true, std::sync::atomic::Ordering::Release);
                match self.save_tx.try_send((queued_epoch, state.value.clone())) {
                    Ok(()) => self.dirty_since = None,
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        // Writer busy with an older snapshot; retain the
                        // pending flag and retry on the next debounce.
                        self.dirty_since = Some(Instant::now());
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                        self.save_failed
                            .store(true, std::sync::atomic::Ordering::Release);
                        self.save_pending
                            .store(false, std::sync::atomic::Ordering::Release);
                        self.error_message =
                            Some("autosave failed: writer thread disconnected".to_owned());
                        self.dirty_since = Some(Instant::now());
                    }
                }
            }
        }
    }

    fn poll_autosave_error(&mut self) {
        let error = self
            .save_error
            .lock()
            .ok()
            .and_then(|mut error| error.take());
        if let Some(error) = error {
            self.save_failed
                .store(false, std::sync::atomic::Ordering::Release);
            self.close_after_save = false;
            self.error_message = Some(format!("autosave failed: {error}"));
            // Keep the UI snapshot eligible for a future retry after the
            // operator fixes the underlying filesystem or lock failure.
            self.dirty_since = Some(Instant::now());
        }
    }

    fn rerender_current_page(&mut self) {
        self.start_operation(EditorOperationKind::SaveRender);
    }

    fn save_and_render(&mut self) {
        self.start_operation(EditorOperationKind::SaveRender);
    }

    fn refresh_cached_pages(&mut self) {
        self.start_operation(EditorOperationKind::RefreshCachedPages);
    }

    fn start_operation(&mut self, kind: EditorOperationKind) {
        if let Some(operation) = self.operation.as_ref() {
            self.error_message = Some(format!(
                "{} is already running; wait for it to finish before submitting again",
                operation.kind.label()
            ));
            return;
        }
        let Some(state) = self.state.as_ref() else {
            self.error_message = Some("editor state is unavailable".to_owned());
            return;
        };

        // Take a bounded snapshot on the UI thread, then immediately return
        // control to egui. The worker owns the heavy disk, cleaning, font and
        // typesetting calls from this point onward.
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let request = OperationRequest {
            kind,
            job_dir: self.job_dir.clone(),
            state_value: state.value.clone(),
            current_page: self.current_page,
            review: self.review.clone(),
            save_epoch: Arc::clone(&self.save_epoch),
            save_lock: Arc::clone(&self.save_lock),
            repaint: self.context.clone().unwrap_or_default(),
            cancel: Arc::clone(&cancel),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.cancel_drag();
        // The accepted snapshot is now owned by the explicit operation. A
        // failed operation re-arms the debounce when control returns below.
        self.dirty_since = None;
        self.error_message = None;
        self.error_guidance = None;
        self.error_target = None;
        self.cache_plan_status = None;
        self.operation = Some(OperationRuntime {
            kind,
            rx,
            handle: None,
            current: 0,
            total: 1,
            message: format!("Starting {}…", kind.label()),
            cancel,
        });

        let spawn_result = std::thread::Builder::new()
            .name(format!("editor-{}", kind.label().replace(' ', "-")))
            .spawn(move || {
                let event = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_editor_operation(request, &tx)
                })) {
                    Ok(Ok(success)) => OperationEvent::Succeeded(Box::new(success)),
                    Ok(Err(error)) => OperationEvent::Failed {
                        kind,
                        error: format!("{error:#}"),
                    },
                    Err(payload) => OperationEvent::Failed {
                        kind,
                        error: format!("worker panicked: {}", panic_payload(payload)),
                    },
                };
                let _ = tx.send(event);
            });
        match spawn_result {
            Ok(handle) => {
                if let Some(operation) = self.operation.as_mut() {
                    operation.handle = Some(handle);
                }
            }
            Err(error) => {
                self.operation = None;
                self.dirty_since = Some(Instant::now());
                self.error_message =
                    Some(format!("could not start {} worker: {error}", kind.label()));
            }
        }
    }

    pub(crate) fn operation_active(&self) -> bool {
        self.operation.is_some()
    }

    fn cancel_operation(&mut self) {
        if let Some(operation) = self.operation.as_ref() {
            operation
                .cancel
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.notify_message = Some((
                "Cancelling after the current page; completed pages stay cached…".to_owned(),
                Instant::now(),
            ));
        }
    }

    /// Install the latest persisted `project.json` into the UI state.
    /// Returns true when newer worker-persisted state was loaded. Called
    /// after a failed or cancelled operation so stale UI clones can never
    /// overwrite worker progress on the next autosave (Blocker 8).
    fn reload_persisted_project_state(&mut self) -> bool {
        let Ok(bytes) = std::fs::read(self.job_dir.join("project.json")) else {
            return false;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return false;
        };
        if value
            .get("pages")
            .and_then(|pages| pages.as_array())
            .is_none()
        {
            return false;
        }
        if let Some(state) = self.state.as_mut() {
            // Only move forward to strictly newer persisted state: the UI
            // clone may hold unsaved operator edits at the same revision
            // (the worker never saved), which a reload must not discard.
            let current_revision = state
                .value
                .get("state_revision")
                .and_then(|revision| revision.as_u64())
                .unwrap_or(0);
            let persisted_revision = value
                .get("state_revision")
                .and_then(|revision| revision.as_u64())
                .unwrap_or(0);
            if persisted_revision <= current_revision {
                return false;
            }
            state.value = value;
            self.clear_texture_caches();
            true
        } else {
            false
        }
    }

    fn poll_operation(&mut self, ctx: &Context) {
        let Some(mut operation) = self.operation.take() else {
            return;
        };
        let mut finished = None;
        loop {
            match operation.rx.try_recv() {
                Ok(event) => match event {
                    OperationEvent::Progress {
                        current,
                        total,
                        message,
                    } => {
                        operation.current = current;
                        operation.total = total.max(1);
                        operation.message = message;
                    }
                    OperationEvent::Succeeded(success) => {
                        finished = Some(Ok((
                            success.kind,
                            success.state,
                            success.review,
                            success.rendered_pages,
                            success.message,
                        )));
                        break;
                    }
                    OperationEvent::Failed { kind, error } => {
                        finished = Some(Err(format!("{} failed: {}", kind.label(), error)));
                        break;
                    }
                },
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    finished = Some(Err(format!(
                        "{} worker disconnected before completing; retry the operation",
                        operation.kind.label()
                    )));
                    break;
                }
            }
        }

        if let Some(result) = finished {
            // The worker already sent its terminal event, so it has exited
            // or is exiting; joining here reaps the handle instead of leaving
            // a fire-and-forget thread racing shutdown.
            if let Some(handle) = operation.handle.take() {
                let _ = handle.join();
            }
            match result {
                Ok((kind, value, review, rendered_pages, message)) => {
                    if let Some(state) = self.state.as_mut() {
                        state.value = value;
                    }
                    for page_index in rendered_pages {
                        self.invalidate_page_textures(page_index);
                    }
                    self.canvas.brush_overlay = None;
                    self.canvas.brush_overlay_dirty = true;
                    self.dirty_since = None;
                    self.error_message = None;
                    self.error_guidance = None;
                    self.error_target = None;
                    if kind == EditorOperationKind::RefreshCachedPages {
                        self.cache_plan_status = Some(message.clone());
                    }
                    self.notify_message = Some((
                        if kind == EditorOperationKind::Approve {
                            "✓ Approval saved; closing editor".to_owned()
                        } else {
                            message
                        },
                        Instant::now(),
                    ));
                    if kind == EditorOperationKind::Approve {
                        if let Some(review) = review {
                            self.review = review;
                        }
                        self.exit_action = Some(ExitAction::Approve);
                        if let Ok(mut exit_state) = self.exit_state.lock() {
                            exit_state.action = Some(ExitAction::Approve);
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                Err(error) => {
                    // Keep the in-memory review untouched and the operation
                    // retryable. The worker only returns a review after all
                    // validation and persistence steps succeed.
                    self.present_operation_error(error);
                    // Blocker 8: the worker may have persisted newer
                    // project.json snapshots (per-page renders bump the
                    // state revision) while the UI still holds its older
                    // pre-operation clone. Reload the persisted state so a
                    // resumed autosave can never overwrite newer worker
                    // progress with stale UI state.
                    if self.reload_persisted_project_state() {
                        self.dirty_since = None;
                    } else {
                        // The accepted snapshot may have contained
                        // metadata-only edits, so re-arm autosave only when
                        // no newer persisted state could be loaded.
                        self.dirty_since = Some(Instant::now());
                    }
                }
            }
            if self.close_after_operation {
                self.close_after_operation = false;
                if self.dirty_since.is_none() {
                    // The original close request was canceled while the
                    // worker stopped.  Close on the next frame so the
                    // CancelClose command from that request cannot veto it.
                    self.close_ready = true;
                    ctx.request_repaint();
                }
            }
            return;
        }

        ctx.request_repaint_after(Duration::from_millis(100));
        self.operation = Some(operation);
    }

    fn present_operation_error(&mut self, error: String) {
        let raw = error.clone();
        if let Some(bubble_id) = overflow_bubble_id(&error) {
            self.error_guidance = Some(format!(
                "Translation does not fit in bubble {bubble_id}. Select the highlighted bubble, then enlarge its box, reduce font size or padding, shorten the translation, or flag it for re-translation."
            ));
            let matched_id = self.state.as_ref().and_then(|state| {
                state
                    .value
                    .get("pages")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .find_map(|(page_index, page)| {
                        page.get("bubbles")
                            .and_then(serde_json::Value::as_array)
                            .and_then(|bubbles| {
                                bubbles.iter().position(|bubble| {
                                    bubble.get("id").and_then(serde_json::Value::as_str)
                                        == Some(bubble_id.as_str())
                                })
                            })
                            .map(|bubble_index| (page_index, bubble_index))
                    })
            });
            let located_by_id = matched_id.is_some();
            let target = matched_id.or_else(|| {
                let page_index = overflow_page_index(&error)?;
                let bubble_index = overflow_bubble_index(&error)?;
                self.state
                    .as_ref()?
                    .value
                    .get("pages")
                    .and_then(serde_json::Value::as_array)?
                    .get(page_index)?
                    .get("bubbles")
                    .and_then(serde_json::Value::as_array)?
                    .get(bubble_index)?
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(|_| (page_index, bubble_index))
            });
            if let Some((page_index, bubble_index)) = target {
                self.set_current_page(page_index);
                self.gallery_scroll_target = Some(page_index);
                self.canvas.selected = Some((page_index, bubble_index));
                self.canvas.current_variant = canvas::Variant::Cleaned;
                self.error_target = Some((page_index, bubble_index));
                if !located_by_id {
                    self.error_guidance = Some(format!(
                        "Translation does not fit in bubble {bubble_id}. Its ID was not found, so page {} bubble {} is selected from the render error's location. Verify the bubble before editing; enlarge its box, reduce font size or padding, shorten the translation, or flag it for re-translation.",
                        page_index + 1,
                        bubble_index + 1
                    ));
                }
            } else {
                self.error_guidance = Some(format!(
                    "Translation does not fit in bubble {bubble_id}, but that bubble could not be located in the loaded project. Reopen the job to refresh its editor state."
                ));
            }
        }
        self.error_message = Some(raw);
    }

    fn begin_pending_history(&mut self) {
        if self.pending_history.is_none() {
            self.pending_history = self.state.as_ref().map(|state| state.value.clone());
            self.redo_history.clear();
        }
    }

    fn commit_pending_history(&mut self) {
        if let Some(snapshot) = self.pending_history.take() {
            push_history_snapshot(&mut self.undo_history, snapshot);
            self.redo_history.clear();
        }
    }

    fn record_history_before_mutation(&mut self) {
        if let Some(state) = self.state.as_ref() {
            push_history_snapshot(&mut self.undo_history, state.value.clone());
            self.redo_history.clear();
        }
    }

    fn clear_after_history_restore(&mut self) {
        self.canvas.selected = None;
        self.canvas.selected_issue = None;
        self.canvas.drag = canvas::DragState::None;
        self.canvas.drag_snapshot = None;
        self.canvas.drag_page_dirty_before = None;
        self.canvas.drag_moved = false;
        self.canvas.pan_start = None;
        self.canvas.brush_overlay = None;
        self.canvas.brush_overlay_dirty = true;
        self.clear_texture_caches();
        self.schedule_save();
    }

    fn undo_operator(&mut self) {
        if let Some(previous) = self.undo_history.pop_back() {
            if let Some(state) = self.state.as_mut() {
                push_history_snapshot(&mut self.redo_history, state.value.clone());
                state.value = previous;
                self.clear_after_history_restore();
            }
        }
    }

    fn redo_operator(&mut self) {
        if let Some(next) = self.redo_history.pop_back() {
            if let Some(state) = self.state.as_mut() {
                push_history_snapshot(&mut self.undo_history, state.value.clone());
                state.value = next;
                self.clear_after_history_restore();
            }
        }
    }

    fn approve_and_export(&mut self) {
        if let Some(action) = self.review.action.as_deref() {
            self.error_message = Some(format!(
                "review action '{action}' is already submitted for revision {}; reopen this editor for a new review cycle",
                self.review.revision
            ));
            return;
        }
        if self.review.consumed {
            self.error_message = Some(format!(
                "review revision {} is already consumed; reopen this editor for a new review cycle",
                self.review.revision
            ));
            return;
        }
        if !self.review_file_is_current() {
            self.error_message = Some("review changed on disk; reopen this editor".to_owned());
            return;
        }
        if let Some(requests) = self
            .state
            .as_ref()
            .map(state::EditorState::retranslation_requests)
            && !requests.is_empty()
        {
            let summary = requests
                .iter()
                .map(|(page, bubble)| format!("page {} bubble {bubble}", page + 1))
                .collect::<Vec<_>>()
                .join(", ");
            self.error_message = Some(format!(
                "re-translation requested for {summary}; send the request to the translator before approval"
            ));
            return;
        }
        if self.state.as_ref().is_some_and(|state| {
            state.value["pages"].as_array().is_some_and(|pages| {
                pages.iter().any(|page| {
                    page["issues"].as_array().is_some_and(|issues| {
                        issues
                            .iter()
                            .any(|issue| issue["origin"].as_str() == Some("missing-dialogue-flag"))
                    })
                })
            })
        }) {
            self.error_message = Some(
                "missing dialogue is flagged; send the review request before approval".to_owned(),
            );
            return;
        }
        self.start_operation(EditorOperationKind::Approve);
    }

    fn request_fixes(&mut self) {
        if self.review.action.is_some() || self.review.consumed {
            return;
        }
        if !self.review_file_is_current() {
            self.error_message = Some("review changed on disk; reopen this editor".to_owned());
            return;
        }
        let derived_feedback = native_review_feedback(
            self.state
                .as_ref()
                .map(|state| &state.value)
                .unwrap_or(&serde_json::Value::Null),
        );
        // Native review has one authoritative source: the current page issue
        // rectangles and bubble flags. Never resubmit stale feedback loaded
        // from an earlier draft when the operator has cleared the page.
        let mut proposed_review = self.review.clone();
        proposed_review.feedback = derived_feedback;
        if proposed_review.feedback.is_empty() {
            self.error_message = Some(
                "request fixes needs a flagged bubble or an issue on at least one page".to_owned(),
            );
            return;
        }
        proposed_review.status = "fixes_requested".to_owned();
        proposed_review.action = Some("request_fixes".to_owned());
        proposed_review.audit.push(serde_json::json!({
            "event": "request_fixes",
            "review_session_id": proposed_review.review_session_id.clone(),
            "revision": proposed_review.revision,
        }));
        if let Some(state) = self.state.as_ref() {
            if let Err(error) = self.save_project_sync(state) {
                self.dirty_since = Some(Instant::now());
                self.error_message = Some(format!("save failed before request: {error}"));
                return;
            }
            // The marker and any translation edits are now on disk. Avoid a
            // redundant close-time autosave after the review action is sent.
            self.dirty_since = None;
        }
        if let Err(e) = state::try_save_review_state(&self.job_dir, &proposed_review) {
            self.error_message = Some(format!("save review failed: {e}"));
            return;
        }
        self.review = proposed_review;
        self.exit_action = Some(ExitAction::RequestFixes);
        if let Ok(mut s) = self.exit_state.lock() {
            s.action = Some(ExitAction::RequestFixes);
        }
        self.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
    }

    pub(crate) fn begin_missing_dialogue_flag(&mut self) {
        self.begin_missing_dialogue_flag_at(None);
    }

    pub(crate) fn begin_missing_dialogue_redraw(&mut self, issue_index: usize) {
        self.begin_missing_dialogue_flag_at(Some(issue_index));
    }

    fn begin_missing_dialogue_flag_at(&mut self, edit_target: Option<usize>) {
        self.cancel_drag();
        self.set_active_tool(canvas::ActiveTool::Select);
        self.canvas.current_variant = canvas::Variant::Cleaned;
        self.canvas.draw_issue_mode = true;
        self.canvas.draw_issue_edit_target = edit_target;
        self.canvas.selected = None;
        self.canvas.selected_issue = None;
        self.error_message = None;
        self.error_guidance = None;
    }

    pub(crate) fn cancel_missing_dialogue_flag(&mut self) {
        self.cancel_drag();
        self.canvas.draw_issue_mode = false;
        self.canvas.draw_issue_edit_target = None;
    }

    fn review_file_is_current(&self) -> bool {
        read_review(&self.job_dir).is_some_and(|current| {
            current.review_session_id == self.review.review_session_id
                && current.revision == self.review.revision
                && current.action.is_none()
                && !current.consumed
        })
    }

    fn handle_shortcuts(&mut self, ctx: &Context) {
        let count = self.state.as_ref().map(|s| s.page_count()).unwrap_or(0);
        let mut next = self.current_page;
        let mut undo = false;
        let mut redo = false;
        // egui gives text editors keyboard focus through Memory. Keep editor
        // navigation and brush shortcuts out of text fields/modal-like widgets
        // while keeping document shortcuts from stealing text edits as well.
        let text_focus = ctx.memory(|memory| memory.focused().is_some());
        let operation_active = self.operation_active();
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Key {
                    key,
                    modifiers,
                    pressed,
                    ..
                } = event
                {
                    if !pressed {
                        continue;
                    }
                    match key {
                        egui::Key::A | egui::Key::ArrowLeft
                            if !text_focus && *modifiers == egui::Modifiers::NONE =>
                        {
                            next = next.saturating_sub(1);
                        }
                        egui::Key::D | egui::Key::ArrowRight
                            if !text_focus && *modifiers == egui::Modifiers::NONE =>
                        {
                            next = (next + 1).min(count.saturating_sub(1));
                        }
                        egui::Key::Z if modifiers.ctrl => undo = true,
                        egui::Key::Y if modifiers.ctrl => redo = true,
                        egui::Key::S if modifiers.ctrl => {
                            // handled below (needs &mut self)
                        }
                        egui::Key::B
                            if tool_shortcuts_allowed(text_focus)
                                && !operation_active
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            // toggle Brush / back to Select
                            let next_tool = if self.canvas.active_tool == canvas::ActiveTool::Brush
                            {
                                canvas::ActiveTool::Select
                            } else {
                                canvas::ActiveTool::Brush
                            };
                            self.set_active_tool(next_tool);
                        }
                        egui::Key::E
                            if tool_shortcuts_allowed(text_focus)
                                && !operation_active
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            let next_tool = if self.canvas.active_tool == canvas::ActiveTool::Eraser
                            {
                                canvas::ActiveTool::Select
                            } else {
                                canvas::ActiveTool::Eraser
                            };
                            self.set_active_tool(next_tool);
                        }
                        egui::Key::I
                            if tool_shortcuts_allowed(text_focus)
                                && !operation_active
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            let next_tool =
                                if self.canvas.active_tool == canvas::ActiveTool::Eyedropper {
                                    canvas::ActiveTool::Select
                                } else {
                                    canvas::ActiveTool::Eyedropper
                                };
                            self.set_active_tool(next_tool);
                        }
                        egui::Key::V
                            if tool_shortcuts_allowed(text_focus)
                                && !operation_active
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            self.set_active_tool(canvas::ActiveTool::Select);
                        }
                        egui::Key::O
                            if tool_shortcuts_allowed(text_focus)
                                && !operation_active
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            self.set_active_tool(canvas::ActiveTool::DrawBubble);
                        }
                        egui::Key::T
                            if tool_shortcuts_allowed(text_focus)
                                && !operation_active
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            self.set_active_tool(canvas::ActiveTool::AddText);
                        }
                        egui::Key::OpenBracket
                            if tool_shortcuts_allowed(text_focus)
                                && !operation_active
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            self.canvas.brush_radius =
                                adjust_brush_radius(self.canvas.brush_radius, -4.0);
                        }
                        egui::Key::CloseBracket
                            if tool_shortcuts_allowed(text_focus)
                                && !operation_active
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            self.canvas.brush_radius =
                                adjust_brush_radius(self.canvas.brush_radius, 4.0);
                        }
                        egui::Key::Delete | egui::Key::Backspace
                            if tool_shortcuts_allowed(text_focus)
                                && !operation_active
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            if self.canvas.drag != canvas::DragState::None {
                                self.cancel_drag();
                            } else if let Some((page_index, bubble_index)) = self.canvas.selected {
                                let exists = self
                                    .state
                                    .as_ref()
                                    .and_then(|state| state.bubble_value(page_index, bubble_index))
                                    .is_some();
                                if exists {
                                    self.record_history_before_mutation();
                                    if self.state.as_mut().is_some_and(|state| {
                                        state.delete_bubble(page_index, bubble_index)
                                    }) {
                                        self.canvas.selected = None;
                                        self.schedule_save();
                                    }
                                }
                            }
                        }
                        egui::Key::Escape if !text_focus => {
                            self.cancel_drag();
                            self.set_active_tool(canvas::ActiveTool::Select);
                        }
                        _ => {}
                    }
                }
            }
        });
        // Ctrl+S: save & render (must be outside the ctx.input closure since it needs &mut self).
        let ctrl_s = ctx.input(|i| i.events.iter().any(|e| {
            matches!(e, egui::Event::Key { key: egui::Key::S, modifiers, pressed: true, .. } if modifiers.ctrl)
        }));
        if ctrl_s && !text_focus && !operation_active {
            self.save_and_render();
        }
        if next != self.current_page {
            self.set_current_page(next);
        }
        if undo || redo {
            if self.canvas.drag != canvas::DragState::None {
                self.cancel_drag();
            } else if tool_shortcuts_allowed(text_focus) {
                if undo {
                    self.undo_operator();
                }
                if redo {
                    self.redo_operator();
                }
            }
        }
    }
}

struct OperationSuccess {
    kind: EditorOperationKind,
    state: serde_json::Value,
    review: Option<ReviewState>,
    rendered_pages: Vec<usize>,
    message: String,
}

fn ensure_persisted_revision(job_dir: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    let persisted = std::fs::read(job_dir.join("project.json"))
        .map_err(|error| anyhow::anyhow!("read persisted project before save: {error}"))?;
    let persisted: serde_json::Value = serde_json::from_slice(&persisted)
        .map_err(|error| anyhow::anyhow!("parse persisted project before save: {error}"))?;
    let expected_revision = value
        .get("state_revision")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let persisted_revision = persisted
        .get("state_revision")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if persisted_revision != expected_revision {
        anyhow::bail!(
            "stale editor snapshot: persisted revision {persisted_revision}, snapshot revision {expected_revision}"
        );
    }
    Ok(())
}

fn save_editor_snapshot(
    job_dir: &std::path::Path,
    value: &serde_json::Value,
    save_epoch: &Arc<Mutex<u64>>,
    save_lock: &Arc<Mutex<()>>,
    cancel: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<()> {
    // Always take the shared job lease before the local mutex. Approval holds
    // the same lease for its whole operation and re-enters it on this thread.
    // A pending autosave on another thread can hold the same file lease while
    // fsyncing, so worker operations retry here instead of giving up after
    // the lease helper's short ten-second wait.
    let _render_lock = acquire_worker_render_lock(job_dir, cancel)
        .map_err(|error| anyhow::anyhow!("editor render lease failed: {error:#}"))?;
    let _save_guard = save_lock
        .lock()
        .map_err(|_| anyhow::anyhow!("editor save lock poisoned"))?;
    ensure_persisted_revision(job_dir, value)?;
    let result = state::atomic_write_json(&job_dir.join("project.json"), value);
    if result.is_ok() {
        let mut epoch = save_epoch
            .lock()
            .map_err(|_| anyhow::anyhow!("editor save epoch lock poisoned"))?;
        *epoch = epoch.saturating_add(1);
    }
    result
}

/// Wait for a render lease from an operation worker without blocking egui.
///
/// `try_acquire_editor_render_lock` is intentionally fail-fast for UI-thread
/// persistence. Workers use this retrying wrapper so a same-process autosave
/// or a legitimate long-running external render can finish without turning a
/// transient contention into a misleading operation failure. The lock order
/// remains render lease -> process-local save mutex everywhere.
fn acquire_worker_render_lock(
    job_dir: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<Option<fukidashi_mcp::workflow::RenderLock>> {
    loop {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            anyhow::bail!("operation cancelled while waiting for editor render lease");
        }
        match fukidashi_mcp::editor::try_acquire_editor_render_lock(job_dir) {
            Ok(lock) => return Ok(lock),
            Err(error) if is_busy_render_lease_error(&error) => {
                std::thread::sleep(WORKER_RENDER_LOCK_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

fn advance_save_epoch(save_epoch: &Arc<Mutex<u64>>) -> anyhow::Result<()> {
    let mut epoch = save_epoch
        .lock()
        .map_err(|_| anyhow::anyhow!("editor save epoch lock poisoned"))?;
    *epoch = epoch.saturating_add(1);
    Ok(())
}

fn worker_page_image_path(state: &EditorState, page_index: usize) -> anyhow::Result<PathBuf> {
    state
        .page(page_index)
        .and_then(|page| {
            page.rendered_image_path
                .or(Some(page.cleaned_image_path))
                .or(Some(page.source_image_path))
        })
        .ok_or_else(|| anyhow::anyhow!("page {} has no image path", page_index + 1))
}

fn report_operation(
    tx: &std::sync::mpsc::Sender<OperationEvent>,
    repaint: &Context,
    current: usize,
    total: usize,
    message: impl Into<String>,
) {
    let _ = tx.send(OperationEvent::Progress {
        current,
        total: total.max(1),
        message: message.into(),
    });
    repaint.request_repaint();
}

fn run_editor_operation(
    request: OperationRequest,
    tx: &std::sync::mpsc::Sender<OperationEvent>,
) -> anyhow::Result<OperationSuccess> {
    let mut state = EditorState {
        value: request.state_value,
        job_dir: request.job_dir.clone(),
        image_path: PathBuf::new(),
    };
    match request.kind {
        EditorOperationKind::SaveRender => {
            report_operation(
                tx,
                &request.repaint,
                1,
                2,
                "Saving the current editor snapshot…",
            );
            save_editor_snapshot(
                &request.job_dir,
                &state.value,
                &request.save_epoch,
                &request.save_lock,
                &request.cancel,
            )
            .map_err(|error| anyhow::anyhow!("save failed: {error}"))?;

            let mut rendered_pages = Vec::new();
            if state.page_has_render_dirty(request.current_page) {
                report_operation(
                    tx,
                    &request.repaint,
                    2,
                    2,
                    format!("Rendering page {}…", request.current_page + 1),
                );
                let image_path = worker_page_image_path(&state, request.current_page)?;
                // Render takes the shared job lease before the process-local
                // save mutex, matching autosave and approval lock ordering.
                // The locked renderer rechecks the revision after the lease
                // is held and writes the normalized state under that lease.
                let rendered = if state.job_dir.join("job.json").is_file()
                    || state.job_dir.join(".fukidashi-job.json").is_file()
                {
                    let workflow = fukidashi_mcp::workflow::Workflow::new(
                        state
                            .job_dir
                            .parent()
                            .ok_or_else(|| anyhow::anyhow!("editor job has no jobs root"))?
                            .to_path_buf(),
                    )?;
                    let _render_lock = acquire_worker_render_lock(&state.job_dir, &request.cancel)?;
                    let _save_guard = request
                        .save_lock
                        .lock()
                        .map_err(|_| anyhow::anyhow!("editor save lock poisoned"))?;
                    render::rerender_page_with_state_locked(
                        &workflow,
                        &state,
                        request.current_page,
                        &image_path,
                    )?
                } else {
                    let _save_guard = request
                        .save_lock
                        .lock()
                        .map_err(|_| anyhow::anyhow!("editor save lock poisoned"))?;
                    render::rerender_page_with_state(&state, request.current_page, &image_path)?
                };
                state.value = rendered.state;
                advance_save_epoch(&request.save_epoch)?;
                rendered_pages.push(request.current_page);
            }
            let rerendered = !rendered_pages.is_empty();
            Ok(OperationSuccess {
                kind: request.kind,
                state: state.value,
                review: None,
                rendered_pages,
                message: if rerendered {
                    "✓ Saved and re-rendered current page".to_owned()
                } else {
                    "✓ Saved".to_owned()
                },
            })
        }
        EditorOperationKind::RefreshCachedPages => {
            let managed = state.job_dir.join("job.json").is_file()
                || state.job_dir.join(".fukidashi-job.json").is_file();
            if !managed {
                anyhow::bail!("refresh cached pages requires a managed job");
            }

            report_operation(tx, &request.repaint, 1, 2, "Checking managed cached pages…");
            // Keep the authoritative render-plan comparison and the marker
            // cleanup under the same job-wide lease. This closes the race in
            // which a sidecar changes between validation and the snapshot
            // write, while leaving all semantic edits in the UI snapshot.
            let _render_lock = acquire_worker_render_lock(&request.job_dir, &request.cancel)?;
            let (dirty_pages, reused_pages) = approval_render_plan(&state)?;
            let batches = fukidashi_mcp::approval::partition_approval_batches(&dirty_pages);
            let batch_sizes = if batches.is_empty() {
                "none".to_owned()
            } else {
                batches
                    .iter()
                    .map(|batch| batch.len().to_string())
                    .collect::<Vec<_>>()
                    .join("/")
            };
            report_operation(
                tx,
                &request.repaint,
                2,
                2,
                format!(
                    "Cached-page plan: reused {reused_pages}, true dirty {}, batches {batch_sizes}",
                    dirty_pages.len()
                ),
            );
            let markers_cleared = clear_reused_render_dirty_markers(&mut state.value, &dirty_pages);
            if markers_cleared {
                // This helper performs the post-lock persisted-revision check
                // and advances the save epoch so a queued autosave cannot
                // overwrite the reconciled marker state.
                save_editor_snapshot(
                    &request.job_dir,
                    &state.value,
                    &request.save_epoch,
                    &request.save_lock,
                    &request.cancel,
                )
                .map_err(|error| anyhow::anyhow!("save failed: {error}"))?;
            }
            Ok(OperationSuccess {
                kind: request.kind,
                state: state.value,
                review: None,
                rendered_pages: Vec::new(),
                message: format!(
                    "✓ Cached pages refreshed: reused {reused_pages}, true dirty {}, batches {batch_sizes}",
                    dirty_pages.len()
                ),
            })
        }
        EditorOperationKind::Approve => {
            // Single writer exclusion for the whole pipeline: the approval
            // lease holds the existing workflow render lock across every page.
            // Per-page renders run under it via the locked path and never
            // release the exclusion between pages. A second attempt gets a
            // clear busy error instead of starting a competing pipeline.
            let _approval_lease = fukidashi_mcp::approval::ApprovalLease::try_acquire(
                &request.job_dir,
                request.review.revision,
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "approval already running for revision {}; wait for it or retry",
                    request.review.revision
                )
            })?;
            // Freeze the semantic snapshot and compute the dirty plan ONCE.
            let (dirty_pages, reused_pages) = approval_render_plan(&state)?;
            clear_reused_render_dirty_markers(&mut state.value, &dirty_pages);
            let job_id = request
                .job_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| request.job_dir.display().to_string());
            let snapshot = fukidashi_mcp::approval::ApprovalSnapshot::freeze(
                job_id,
                request.review.revision,
                &fukidashi_mcp::editor::approval_state_signature(
                    &state.value,
                    Some(&request.job_dir),
                ),
                dirty_pages.clone(),
            );
            // Restart-safe resume: the snapshot identity excludes the dirty
            // plan, so a restart with unchanged semantics reattaches to the
            // SAME checkpoint and retains the ORIGINAL batch plan.
            let (mut checkpoint, effective_plan) = fukidashi_mcp::approval::prepare_resume(
                &request.job_dir,
                request.review.revision,
                &snapshot,
            );
            // Persist the frozen operation identity before touching the first
            // page. This is required even when the dirty plan is empty: an
            // unchanged reopen still needs a durable checkpoint for export
            // binding and crash-safe approval.
            fukidashi_mcp::approval::save_approval_checkpoint(
                &request.job_dir,
                request.review.revision,
                &checkpoint,
            )
            .map_err(|error| {
                anyhow::anyhow!("approval checkpoint write failed before rendering: {error:#}")
            })?;
            let total = effective_plan.len().saturating_add(3);
            // Iterate the CHECKPOINT's batches: on a resume the freshly
            // recomputed snapshot carries a smaller dirty set, but the
            // original 60/60/22 shape is retained from the checkpoint.
            let batches = checkpoint.batches.clone();
            let batch_count = batches.len();
            report_operation(
                tx,
                &request.repaint,
                1,
                total,
                format!("Saving the approval snapshot (reusing {reused_pages} verified pages)…"),
            );
            save_editor_snapshot(
                &request.job_dir,
                &state.value,
                &request.save_epoch,
                &request.save_lock,
                &request.cancel,
            )
            .map_err(|error| anyhow::anyhow!("save failed before approval: {error}"))?;

            let managed = request.job_dir.join("job.json").is_file()
                || request.job_dir.join(".fukidashi-job.json").is_file();
            let workflow = if managed {
                Some(fukidashi_mcp::workflow::Workflow::new(
                    request
                        .job_dir
                        .parent()
                        .unwrap_or(&request.job_dir)
                        .to_path_buf(),
                )?)
            } else {
                None
            };
            let mut rendered_pages = Vec::new();
            let mut position = 0_usize;
            for (batch_index, batch) in batches.clone().into_iter().enumerate() {
                for (in_batch, page_index) in batch.into_iter().enumerate() {
                    if request.cancel.load(std::sync::atomic::Ordering::SeqCst) {
                        anyhow::bail!(
                            "approval cancelled after {position} of {} pages; completed pages stay cached—retry to resume",
                            effective_plan.len()
                        );
                    }
                    let batch_message = if batch_count > 1 {
                        format!(
                            "{} — ",
                            fukidashi_mcp::approval::batch_progress_message(
                                batch_index,
                                batch_count,
                                in_batch,
                                batches[batch_index].len(),
                            )
                        )
                    } else {
                        String::new()
                    };
                    report_operation(
                        tx,
                        &request.repaint,
                        position + 2,
                        total,
                        format!(
                            "{batch_message}Rendering page {} of {} before approval…",
                            position + 1,
                            effective_plan.len()
                        ),
                    );
                    // Crash-gap recovery + resume: a structurally complete
                    // checkpoint entry is never trusted alone. Page
                    // association, semantic signature, input artifact hashes,
                    // output path, live output bytes, and sidecar validity
                    // must all agree before the render is skipped (Blocker 2).
                    let expected = approval_page_expectation(
                        &state.value,
                        workflow.as_ref(),
                        &request.job_dir,
                        page_index,
                    )?;
                    let mut promoted = false;
                    if let Some(entry) = checkpoint.pages.get(&page_index.to_string()) {
                        let live_hash =
                            fukidashi_mcp::approval::sha256_file(&expected.output_path).ok();
                        if fukidashi_mcp::approval::checkpoint_entry_valid(
                            entry,
                            &expected,
                            live_hash.as_deref(),
                        ) {
                            promoted = true;
                        }
                    }
                    if !promoted {
                        // Artifact committed but checkpoint did not (or no
                        // entry was ever saved): promote the live artifact
                        // ONLY when its sidecar proves it corresponds to the
                        // frozen expected inputs. An old render for superseded
                        // inputs is rejected and the page rerenders.
                        let live = approval_live_evidence(workflow.as_ref(), &expected);
                        if let Some(recovered) =
                            fukidashi_mcp::approval::recover_verified_artifact(&expected, &live)
                        {
                            checkpoint.mark_complete(recovered);
                            // Durability-critical: a failed checkpoint write
                            // fails the operation instead of pretending the
                            // promotion persisted (Blocker 6).
                            fukidashi_mcp::approval::save_approval_checkpoint(
                                &request.job_dir,
                                request.review.revision,
                                &checkpoint,
                            )
                            .map_err(|error| {
                                anyhow::anyhow!(
                                    "approval checkpoint write failed after page {}: {error:#}; committed renders stay cached—retry to resume",
                                    page_index + 1
                                )
                            })?;
                            promoted = true;
                        }
                    }
                    if promoted {
                        position += 1;
                        continue;
                    }
                    let image_path = worker_page_image_path(&state, page_index)?;
                    // Both render paths run under the operation-wide approval
                    // lease. Managed pages use the locked renderer so the
                    // writer exclusion is never released between pages; ad-hoc
                    // jobs keep the legacy locking render (Blocker 4).
                    let _save_guard = request
                        .save_lock
                        .lock()
                        .map_err(|_| anyhow::anyhow!("editor save lock poisoned"))?;
                    let rendered = match workflow.as_ref() {
                        Some(managed) => render::rerender_page_with_state_locked(
                            managed,
                            &state,
                            page_index,
                            &image_path,
                        ),
                        None => render::rerender_page_with_state(&state, page_index, &image_path),
                    }
                    .map_err(|error| {
                        anyhow::anyhow!("page {} could not be re-rendered: {error}", page_index + 1)
                    })?;
                    state.value = rendered.state;
                    advance_save_epoch(&request.save_epoch)?;
                    // Commit with actuals from the post-render sidecar, not
                    // the pre-render expectation, then persist the checkpoint.
                    // A failed write fails the operation; the committed
                    // artifact stays recoverable (Blocker 6).
                    let done_expected = approval_page_expectation(
                        &state.value,
                        workflow.as_ref(),
                        &request.job_dir,
                        page_index,
                    )?;
                    let committed = approval_commit_entry(workflow.as_ref(), &done_expected)
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "page {} rendered but its completion could not be recorded: {error:#}; retry to resume",
                                page_index + 1
                            )
                        })?;
                    checkpoint.mark_complete(committed);
                    fukidashi_mcp::approval::save_approval_checkpoint(
                        &request.job_dir,
                        request.review.revision,
                        &checkpoint,
                    )
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "approval checkpoint write failed after page {}: {error:#}; committed renders stay cached—retry to resume",
                            page_index + 1
                        )
                    })?;
                    rendered_pages.push(page_index);
                    position += 1;
                }
            }

            report_operation(
                tx,
                &request.repaint,
                effective_plan.len() + 2,
                total,
                "Validating translation completeness…",
            );
            if (request.job_dir.join("job.json").is_file()
                || request.job_dir.join(".fukidashi-job.json").is_file())
                && request.job_dir.join("pages").is_dir()
            {
                let workflow = fukidashi_mcp::workflow::Workflow::new(
                    request
                        .job_dir
                        .parent()
                        .unwrap_or(&request.job_dir)
                        .to_path_buf(),
                )?;
                workflow
                    .validate_editor_completeness(&request.job_dir, &state.value)
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "approval blocked by translation completeness validation: {error}"
                        )
                    })?;
            }

            // Packaging is a separate phase: for managed jobs, validate every
            // expected cached artifact in natural order without rerendering,
            // so the later ZIP assembly consumes exactly these files. Ad-hoc
            // jobs without managed render artifacts keep the legacy behavior.
            if workflow.is_some() {
                let page_count = state.page_count();
                report_operation(
                    tx,
                    &request.repaint,
                    total,
                    total,
                    format!("Validating {page_count} cached pages…"),
                );
                let pages = state
                    .value
                    .get("pages")
                    .and_then(|pages| pages.as_array())
                    .ok_or_else(|| anyhow::anyhow!("approval state has no pages"))?;
                if pages.len() != page_count {
                    anyhow::bail!("approval state page count mismatch");
                }
                for (index, page) in pages.iter().enumerate() {
                    let rendered = page
                        .get("rendered_image_path")
                        .and_then(|path| path.as_str())
                        .ok_or_else(|| {
                            anyhow::anyhow!("page {} has no cached render", index + 1)
                        })?;
                    let candidate = std::path::Path::new(rendered);
                    let absolute = if candidate.is_absolute() {
                        candidate.to_path_buf()
                    } else {
                        request.job_dir.join(candidate)
                    };
                    if !absolute.is_file() {
                        anyhow::bail!("page {} cached render is missing", index + 1);
                    }
                    if let Some(workflow) = workflow.as_ref()
                        && workflow.validate_render_input(&absolute).is_err()
                    {
                        anyhow::bail!("page {} cached render is invalid", index + 1);
                    }
                }
            }

            if !review_file_matches(&request.job_dir, &request.review) {
                anyhow::bail!("review changed on disk; reopen this editor");
            }
            let mut proposed_review = request.review.clone();
            proposed_review.approved_pages = (0..state.page_count()).collect();
            proposed_review.status = "approved".to_owned();
            proposed_review.action = Some("approve_export".to_owned());
            let binding = fukidashi_mcp::approval::approval_audit_value(
                &snapshot,
                &checkpoint.effective_dirty_plan(&snapshot),
                &checkpoint.batches,
                state.page_count(),
                reused_pages,
            );
            proposed_review.audit.push(serde_json::json!({
                "event": "approve_export",
                "review_session_id": proposed_review.review_session_id.clone(),
                "revision": proposed_review.revision,
                "approval": binding,
            }));

            report_operation(
                tx,
                &request.repaint,
                total,
                total,
                format!(
                    "Saving approval and closing the editor (reused {reused_pages} verified pages)…"
                ),
            );
            save_editor_snapshot(
                &request.job_dir,
                &state.value,
                &request.save_epoch,
                &request.save_lock,
                &request.cancel,
            )
            .map_err(|error| anyhow::anyhow!("save failed before approval: {error}"))?;
            state::save_review_state(&request.job_dir, &proposed_review)
                .map_err(|error| anyhow::anyhow!("save review failed: {error}"))?;
            Ok(OperationSuccess {
                kind: request.kind,
                state: state.value,
                review: Some(proposed_review),
                rendered_pages,
                message: format!(
                    "✓ Approval saved; closing editor (reused {reused_pages} verified pages)"
                ),
            })
        }
    }
}
fn review_file_matches(job_dir: &std::path::Path, expected: &ReviewState) -> bool {
    read_review(job_dir).is_some_and(|current| {
        current.review_session_id == expected.review_session_id
            && current.revision == expected.revision
            && current.action.is_none()
            && !current.consumed
    })
}

fn panic_payload(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".to_owned())
}

fn tool_shortcuts_allowed(text_focus: bool) -> bool {
    !text_focus
}

fn adjust_brush_radius(radius: f32, delta: f32) -> f32 {
    (radius + delta).clamp(4.0, 80.0)
}

/// Convert the user-facing 1-based page number into the editor's bounded
/// zero-based index. Invalid input leaves the current page selected; numeric
/// values outside the document are clamped to the nearest page.
fn page_index_from_input(input: &str, page_count: usize, current_page: usize) -> usize {
    if page_count == 0 {
        return 0;
    }
    let fallback = current_page.min(page_count - 1);
    input
        .trim()
        .parse::<usize>()
        .map(|page_number| page_number.saturating_sub(1).min(page_count - 1))
        .unwrap_or(fallback)
}

fn render_dirty_page_indices(state: &EditorState) -> Vec<usize> {
    (0..state.page_count())
        .filter(|&index| state.page_has_render_dirty(index))
        .collect()
}

fn approval_render_plan(state: &EditorState) -> anyhow::Result<(Vec<usize>, usize)> {
    let managed = state.job_dir.join("job.json").is_file()
        || state.job_dir.join(".fukidashi-job.json").is_file();
    let rerender = if managed {
        let jobs_root = state
            .job_dir
            .parent()
            .unwrap_or(&state.job_dir)
            .to_path_buf();
        fukidashi_mcp::workflow::Workflow::new(jobs_root)?
            .editor_render_plan(&state.job_dir, &state.value)?
    } else {
        render_dirty_page_indices(state)
    };
    let reused = state.page_count().saturating_sub(rerender.len());
    Ok((rerender, reused))
}

fn approval_state_page(state: &serde_json::Value, page_index: usize) -> serde_json::Value {
    state
        .get("pages")
        .and_then(|pages| pages.as_array())
        .and_then(|pages| pages.get(page_index))
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

fn approval_page_id(state: &serde_json::Value, page_index: usize) -> String {
    approval_state_page(state, page_index)
        .get("id")
        .and_then(|id| id.as_str())
        .filter(|id| !id.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("page-{page_index}"))
}

fn approval_output_path(
    job_dir: &std::path::Path,
    state: &serde_json::Value,
    page_index: usize,
) -> Option<PathBuf> {
    approval_state_page(state, page_index)
        .get("rendered_image_path")
        .and_then(|path| path.as_str())
        .map(|path| {
            let candidate = std::path::Path::new(path);
            if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                job_dir.join(candidate)
            }
        })
}

/// Frozen per-page expectation with real input provenance. The semantic
/// signature is the POST-render form recorded in render sidecars, so the
/// expectation agrees with both checkpoint commits and crash-gap evidence.
/// Source/clean hashes come from the currently effective clean artifact
/// (brush-corrected derivative when present, otherwise the base clean). When
/// the clean stage cannot be validated the hashes stay empty: the entry can
/// never validate, the page rerenders, and the render itself reports the
/// underlying problem.
fn approval_page_expectation(
    state: &serde_json::Value,
    workflow: Option<&fukidashi_mcp::workflow::Workflow>,
    job_dir: &std::path::Path,
    page_index: usize,
) -> anyhow::Result<fukidashi_mcp::approval::ExpectedPageProvenance> {
    let page = approval_state_page(state, page_index);
    let semantic_render_signature = fukidashi_mcp::editor::post_render_page_signature(state, &page);
    let output_path = approval_output_path(job_dir, state, page_index)
        .ok_or_else(|| anyhow::anyhow!("page {} has no rendered image path", page_index + 1))?;
    let (source_sha256, clean_sha256) = match workflow {
        Some(managed) => {
            let corrected = page
                .get("corrected_cleaned_image_path")
                .and_then(|path| path.as_str())
                .map(std::path::PathBuf::from)
                .filter(|path| {
                    let absolute = if path.is_absolute() {
                        path.clone()
                    } else {
                        job_dir.join(path)
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
                    job_dir.join(candidate)
                };
                if let Ok(clean) = managed.validate_clean_input(&absolute) {
                    hashes = (clean.source_sha256, clean.cleaned_sha256);
                    break;
                }
            }
            hashes
        }
        None => (String::new(), String::new()),
    };
    Ok(fukidashi_mcp::approval::ExpectedPageProvenance {
        page_index,
        page_id: approval_page_id(state, page_index),
        semantic_render_signature,
        source_sha256,
        clean_sha256,
        output_path,
    })
}

/// Gather live evidence for crash-gap recovery: render-sidecar validity, the
/// sidecar-recorded semantic signature and input hashes, and a fresh output
/// hash. Managed jobs prove correspondence through the sidecar chain; ad-hoc
/// jobs have no sidecars, so their evidence never promotes (their checkpoint
/// entries still validate normally).
fn approval_live_evidence(
    workflow: Option<&fukidashi_mcp::workflow::Workflow>,
    expected: &fukidashi_mcp::approval::ExpectedPageProvenance,
) -> fukidashi_mcp::approval::LiveArtifactEvidence {
    let output_sha256 = fukidashi_mcp::approval::sha256_file(&expected.output_path).ok();
    let Some(managed) = workflow else {
        return fukidashi_mcp::approval::LiveArtifactEvidence {
            sidecar_valid: false,
            sidecar_semantic_signature: None,
            source_sha256: String::new(),
            clean_sha256: String::new(),
            output_sha256,
        };
    };
    let Ok(artifact) = managed.validate_render_input(&expected.output_path) else {
        return fukidashi_mcp::approval::LiveArtifactEvidence {
            sidecar_valid: false,
            sidecar_semantic_signature: None,
            source_sha256: String::new(),
            clean_sha256: String::new(),
            output_sha256,
        };
    };
    let sidecar_semantic_signature = artifact
        .qa
        .get("semantic_render_signature")
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    let (source_sha256, clean_sha256) = managed
        .validate_clean_input(&artifact.cleaned_image)
        .map(|clean| (clean.source_sha256, clean.cleaned_sha256))
        .unwrap_or_default();
    fukidashi_mcp::approval::LiveArtifactEvidence {
        sidecar_valid: true,
        sidecar_semantic_signature,
        source_sha256,
        clean_sha256,
        output_sha256,
    }
}

/// Build the checkpoint commit for a freshly rendered page from the ACTUAL
/// post-render sidecar (semantic signature and input hashes) plus a fresh
/// output hash. Fails loudly when the just-committed sidecar cannot be read
/// back, so the operation retries instead of recording fiction.
fn approval_commit_entry(
    workflow: Option<&fukidashi_mcp::workflow::Workflow>,
    expected: &fukidashi_mcp::approval::ExpectedPageProvenance,
) -> anyhow::Result<fukidashi_mcp::approval::ApprovalPageCheckpoint> {
    let output_sha256 = fukidashi_mcp::approval::sha256_file(&expected.output_path)?;
    let (semantic_render_signature, source_sha256, clean_sha256) = match workflow {
        Some(managed) => {
            let artifact = managed.validate_render_input(&expected.output_path)?;
            let semantic = artifact
                .qa
                .get("semantic_render_signature")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "managed render sidecar has no semantic_render_signature; rerender the page"
                    )
                })?;
            if !fukidashi_mcp::approval::semantic_render_signatures_equal(
                &semantic,
                &expected.semantic_render_signature,
            ) {
                anyhow::bail!(
                    "managed render semantic signature does not match the frozen page expectation"
                );
            }
            let clean = managed.validate_clean_input(&artifact.cleaned_image)?;
            if clean.source_sha256 != expected.source_sha256
                || clean.cleaned_sha256 != expected.clean_sha256
            {
                anyhow::bail!(
                    "managed render source/clean hashes do not match the frozen page expectation"
                );
            }
            (semantic, clean.source_sha256, clean.cleaned_sha256)
        }
        None => (
            expected.semantic_render_signature.clone(),
            String::new(),
            String::new(),
        ),
    };
    Ok(fukidashi_mcp::approval::ApprovalPageCheckpoint::new(
        expected.page_index,
        expected.page_id.clone(),
        semantic_render_signature,
        source_sha256,
        clean_sha256,
        expected.output_path.display().to_string(),
        output_sha256,
    ))
}

fn clear_reused_render_dirty_markers(state: &mut serde_json::Value, rerender: &[usize]) -> bool {
    let Some(pages) = state
        .get_mut("pages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return false;
    };
    let mut needs_render = vec![false; pages.len()];
    for &index in rerender {
        if let Some(marker) = needs_render.get_mut(index) {
            *marker = true;
        }
    }
    let mut changed = false;
    for (index, page) in pages.iter_mut().enumerate() {
        if needs_render[index] {
            let Some(object) = page.as_object_mut() else {
                continue;
            };
            if !object
                .get("render_dirty")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                object.insert("render_dirty".to_owned(), serde_json::Value::Bool(true));
                changed = true;
            }
            continue;
        }
        let Some(object) = page.as_object_mut() else {
            continue;
        };
        if object
            .get("render_dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            changed = true;
        }
        object.insert("render_dirty".to_owned(), serde_json::Value::Bool(false));
        if let Some(bubbles) = object
            .get_mut("bubbles")
            .and_then(serde_json::Value::as_array_mut)
        {
            for bubble in bubbles {
                if let Some(bubble) = bubble.as_object_mut() {
                    if bubble
                        .get("render_dirty")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                    {
                        changed = true;
                    }
                    bubble.insert("render_dirty".to_owned(), serde_json::Value::Bool(false));
                }
            }
        }
    }
    changed
}

fn native_review_feedback(state: &serde_json::Value) -> Vec<serde_json::Value> {
    let Some(pages) = state.get("pages").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut feedback = Vec::new();
    for (page_index, page) in pages.iter().enumerate() {
        if let Some(issues) = page.get("issues").and_then(serde_json::Value::as_array) {
            for issue in issues {
                let issue_type = issue
                    .get("issue_type")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| {
                        matches!(
                            *value,
                            "leftover_source_text"
                                | "text_overflow"
                                | "wrong_translation"
                                | "wrong_or_missing_bubble"
                                | "damaged_artwork"
                                | "font_or_layout"
                                | "flagged_bubble"
                                | "custom"
                        )
                    })
                    .unwrap_or("custom");
                let mut item = serde_json::json!({
                    "page": page_index,
                    "issue_type": issue_type,
                    "origin": issue
                        .get("origin")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("image-pixels"),
                });
                copy_feedback_string(&mut item, issue, "note");
                copy_feedback_string(&mut item, issue, "corrected_text");
                copy_feedback_string(&mut item, issue, "source_ocr");
                copy_feedback_string(&mut item, issue, "current_translation");
                if !item.get("note").is_some_and(serde_json::Value::is_string) {
                    item["note"] = serde_json::Value::String(String::new());
                }
                if !item.get("corrected_text").is_some() {
                    item["corrected_text"] = serde_json::Value::Null;
                }
                copy_feedback_bbox(&mut item, issue);
                if issue_type == "wrong_translation" {
                    link_wrong_translation_feedback(&mut item, page);
                }
                feedback.push(item);
            }
        }
        if let Some(bubbles) = page.get("bubbles").and_then(serde_json::Value::as_array) {
            for bubble in bubbles {
                if bubble
                    .get("retranslate_requested")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    let mut item = serde_json::json!({
                        "page": page_index,
                        "issue_type": "wrong_translation",
                        "origin": "retranslate-flag",
                        "note": "Please provide a fresh translation for this bubble; the operator cannot supply a correction.",
                        "corrected_text": null,
                    });
                    if let Some(id) = bubble.get("id").and_then(serde_json::Value::as_str) {
                        item["bubble_id"] = serde_json::Value::String(id.to_owned());
                    }
                    if let Some(source) = bubble
                        .get("source_ocr")
                        .or_else(|| bubble.get("source_text"))
                        .or_else(|| bubble.get("text"))
                        .and_then(serde_json::Value::as_str)
                    {
                        item["source_ocr"] = serde_json::Value::String(source.to_owned());
                    }
                    if let Some(translation) = bubble
                        .get("translation")
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| {
                            bubble
                                .get("current_translation")
                                .and_then(serde_json::Value::as_str)
                        })
                    {
                        item["current_translation"] =
                            serde_json::Value::String(translation.to_owned());
                    }
                    copy_feedback_bbox(&mut item, bubble);
                    feedback.push(item);
                }
                let flagged = bubble
                    .get("flagged")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                    || bubble
                        .get("problem")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                if !flagged {
                    continue;
                }
                let mut item = serde_json::json!({
                    "page": page_index,
                    "issue_type": "flagged_bubble",
                    "origin": "bubble-flag",
                });
                for key in ["bubble_id", "note", "corrected_text"] {
                    copy_feedback_string(&mut item, bubble, key);
                }
                if let Some(id) = bubble.get("id").and_then(serde_json::Value::as_str) {
                    item["bubble_id"] = serde_json::Value::String(id.to_owned());
                }
                if let Some(source) = bubble
                    .get("source_ocr")
                    .or_else(|| bubble.get("source_text"))
                    .or_else(|| bubble.get("text"))
                    .and_then(serde_json::Value::as_str)
                {
                    item["source_ocr"] = serde_json::Value::String(source.to_owned());
                }
                if let Some(translation) = bubble
                    .get("current_translation")
                    .or_else(|| bubble.get("translation"))
                    .and_then(serde_json::Value::as_str)
                {
                    item["current_translation"] = serde_json::Value::String(translation.to_owned());
                }
                if !item.get("note").is_some_and(serde_json::Value::is_string) {
                    let note = bubble
                        .get("flag_reason")
                        .or_else(|| bubble.get("problem_reason"))
                        .and_then(serde_json::Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or("Flagged bubble requires review");
                    item["note"] = serde_json::Value::String(note.to_owned());
                }
                if !item.get("corrected_text").is_some() {
                    item["corrected_text"] = serde_json::Value::Null;
                }
                copy_feedback_bbox(&mut item, bubble);
                feedback.push(item);
            }
        }
    }
    feedback
}

fn copy_feedback_string(target: &mut serde_json::Value, source: &serde_json::Value, key: &str) {
    let Some(value) = source.get(key).and_then(serde_json::Value::as_str) else {
        return;
    };
    if value.len() <= 4096 {
        target[key] = serde_json::Value::String(value.to_owned());
    }
}

fn copy_feedback_bbox(target: &mut serde_json::Value, source: &serde_json::Value) {
    let Some(value) = source.get("bbox").or_else(|| source.get("bubble_bbox")) else {
        return;
    };
    if serde_json::from_value::<fukidashi_mcp::domain::Rect>(value.clone())
        .ok()
        .and_then(|rect| rect.validate().ok())
        .is_some()
    {
        target["bbox"] = value.clone();
    }
}

fn link_wrong_translation_feedback(target: &mut serde_json::Value, page: &serde_json::Value) {
    let Some(issue_bbox) = target
        .get("bbox")
        .cloned()
        .and_then(|value| serde_json::from_value::<fukidashi_mcp::domain::Rect>(value).ok())
    else {
        target["link_status"] = serde_json::Value::String("no_overlap".to_owned());
        return;
    };
    let candidates = page
        .get("bubbles")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|bubble| {
            let bbox = bubble
                .get("bbox")
                .or_else(|| bubble.get("bubble_bbox"))
                .cloned()
                .and_then(|value| {
                    serde_json::from_value::<fukidashi_mcp::domain::Rect>(value).ok()
                })?;
            (rects_overlap(issue_bbox, bbox)).then_some(bubble)
        })
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [bubble] => {
            target["link_status"] = serde_json::Value::String("linked".to_owned());
            if let Some(id) = bubble.get("id").and_then(serde_json::Value::as_str) {
                target["bubble_id"] = serde_json::Value::String(id.to_owned());
            }
            if let Some(source) = bubble
                .get("source_ocr")
                .or_else(|| bubble.get("source_text"))
                .or_else(|| bubble.get("text"))
                .and_then(serde_json::Value::as_str)
            {
                target["source_ocr"] = serde_json::Value::String(source.to_owned());
            }
            if let Some(translation) = bubble
                .get("current_translation")
                .or_else(|| bubble.get("translation"))
                .and_then(serde_json::Value::as_str)
            {
                target["current_translation"] = serde_json::Value::String(translation.to_owned());
            }
        }
        [] => target["link_status"] = serde_json::Value::String("no_overlap".to_owned()),
        _ => target["link_status"] = serde_json::Value::String("ambiguous".to_owned()),
    }
}

fn rects_overlap(left: fukidashi_mcp::domain::Rect, right: fukidashi_mcp::domain::Rect) -> bool {
    left.x1.max(right.x1) < left.x2.min(right.x2) && left.y1.max(right.y1) < left.y2.min(right.y2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_pages(page_count: usize) -> (tempfile::TempDir, EditorApp) {
        let directory = tempfile::tempdir().unwrap();
        let pages = (0..page_count)
            .map(|index| {
                serde_json::json!({
                    "id": format!("page-{index}"),
                    "image_path": "source.png",
                    "bubbles": []
                })
            })
            .collect::<Vec<_>>();
        state::atomic_write_json(
            &directory.path().join("project.json"),
            &serde_json::json!({"state_revision": 0, "pages": pages}),
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let app = EditorApp::new(directory.path().to_path_buf(), None, exit_state).unwrap();
        (directory, app)
    }

    fn wait_for_operation(app: &mut EditorApp) {
        let context = Context::default();
        for _ in 0..200 {
            app.poll_operation(&context);
            if !app.operation_active() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("editor operation did not finish during the test window");
    }

    #[test]
    fn bracket_radius_changes_by_four_and_clamps() {
        assert_eq!(adjust_brush_radius(20.0, -4.0), 16.0);
        assert_eq!(adjust_brush_radius(4.0, -4.0), 4.0);
        assert_eq!(adjust_brush_radius(80.0, 4.0), 80.0);
    }

    #[test]
    fn page_jump_input_is_one_based_and_bounded() {
        assert_eq!(page_index_from_input("1", 142, 87), 0);
        assert_eq!(page_index_from_input("77", 142, 0), 76);
        assert_eq!(page_index_from_input("142", 142, 87), 141);
        assert_eq!(page_index_from_input("0", 142, 76), 0);
        assert_eq!(page_index_from_input("143", 142, 87), 141);
        assert_eq!(page_index_from_input("999", 142, 76), 141);
        assert_eq!(page_index_from_input("not a page", 142, 76), 76);
        assert_eq!(page_index_from_input("77", 0, 0), 0);
    }

    #[test]
    fn go_page_jump_moves_from_page_88_to_86_after_frame_handoff() {
        let (_directory, mut app) = app_with_pages(142);
        app.set_current_page(87);
        app.page_jump_input = "86".to_owned();

        // Go stages the parsed target; update() applies it before the next
        // frame's canvas/gallery rendering.
        app.jump_to_page_input();
        assert_eq!(app.current_page, 87);
        assert_eq!(app.pending_page_jump, Some(85));
        app.apply_pending_page_jump();
        assert_eq!(app.current_page, 85);
        assert_eq!(app.page_jump_input, "86");
        assert_eq!(app.gallery_scroll_target, Some(85));
    }

    #[test]
    fn enter_page_jump_repeats_without_reverting_the_canvas_page() {
        let (_directory, mut app) = app_with_pages(142);
        app.set_current_page(87);

        for _ in 0..2 {
            app.page_jump_input = "86".to_owned();
            app.jump_to_page_input();
            app.apply_pending_page_jump();
            assert_eq!(app.current_page, 85);
            assert_eq!(app.page_jump_input, "86");
        }
    }

    #[test]
    fn thumbnail_click_after_jump_keeps_the_clicked_page_selected() {
        let (_directory, mut app) = app_with_pages(142);
        app.set_current_page(87);
        app.page_jump_input = "86".to_owned();
        app.jump_to_page_input();
        app.apply_pending_page_jump();

        // The gallery consumes the one-shot scroll request during its frame.
        let context = Context::default();
        app.context = Some(context.clone());
        let _ = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 800.0),
                )),
                ..Default::default()
            },
            |ctx| app.gallery_ui(ctx),
        );
        assert_eq!(app.gallery_scroll_target, None);

        // Clicking thumbnail 88 (1-based) after the jump must select it and
        // must not reapply the prior jump target.
        app.set_current_page(87);
        assert_eq!(app.current_page, 87);
        assert_eq!(app.page_jump_input, "88");
        assert_eq!(app.gallery_scroll_target, None);
    }

    #[test]
    fn page_jump_requests_gallery_scroll_once() {
        let directory = tempfile::tempdir().unwrap();
        let image_path = directory.path().join("page.png");
        image::ImageBuffer::<image::Rgb<u8>, _>::from_pixel(16, 16, image::Rgb([220, 220, 220]))
            .save(&image_path)
            .unwrap();
        let pages = (0..142)
            .map(|index| {
                serde_json::json!({
                    "id": format!("page-{index}"),
                    "image_path": "page.png",
                    "cleaned_image_path": "missing-clean.png",
                    "bubbles": []
                })
            })
            .collect::<Vec<_>>();
        state::atomic_write_json(
            &directory.path().join("project.json"),
            &serde_json::json!({"state_revision": 0, "pages": pages}),
        )
        .unwrap();

        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(directory.path().to_path_buf(), None, exit_state).unwrap();
        app.page_jump_input = "77".to_owned();
        app.jump_to_page_input();
        // Stage the request so the same frame cannot let the text field's
        // focus synchronization put the old canvas page back.
        assert_eq!(app.current_page, 0);
        assert_eq!(app.pending_page_jump, Some(76));
        assert_eq!(app.gallery_scroll_target, None);

        app.apply_pending_page_jump();
        assert_eq!(app.current_page, 76);
        assert_eq!(app.pending_page_jump, None);
        assert_eq!(app.gallery_scroll_target, Some(76));

        let context = Context::default();
        app.context = Some(context.clone());
        let _ = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 800.0),
                )),
                ..Default::default()
            },
            |ctx| app.gallery_ui(ctx),
        );
        assert_eq!(app.gallery_scroll_target, None);

        app.set_current_page(77);
        assert_eq!(app.gallery_scroll_target, None);
    }

    #[test]
    fn tool_shortcuts_are_blocked_by_text_focus() {
        assert!(tool_shortcuts_allowed(false));
        assert!(!tool_shortcuts_allowed(true));
    }

    #[test]
    fn operator_history_is_bounded_and_keeps_latest_snapshot() {
        let mut history = VecDeque::new();
        for value in 0..(MAX_OPERATOR_HISTORY + 3) {
            push_history_snapshot(&mut history, serde_json::json!({"revision": value}));
        }
        assert_eq!(history.len(), MAX_OPERATOR_HISTORY);
        assert_eq!(history.front().unwrap()["revision"], 3);
        assert_eq!(
            history.back().unwrap()["revision"],
            MAX_OPERATOR_HISTORY + 2
        );
    }

    #[test]
    fn texture_cache_evicts_old_pages_at_a_fixed_bound() {
        let context = Context::default();
        let mut cache = TextureCache::with_capacity(PAGE_TEXTURE_CACHE_CAPACITY);
        for index in 0..142 {
            let texture = context.load_texture(
                format!("page-{index}"),
                ColorImage::from_rgba_unmultiplied([1, 1], &[255, 255, 255, 255]),
                TextureOptions::default(),
            );
            cache.insert(index, texture);
        }

        assert_eq!(cache.len(), PAGE_TEXTURE_CACHE_CAPACITY);
        assert!(cache.get(141).is_some());
        assert!(cache.get(140).is_some());
        assert!(cache.get(0).is_none());
    }

    #[test]
    fn large_gallery_loads_only_visible_downscaled_thumbnails() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        let image_path = job_dir.join("page.png");
        image::ImageBuffer::<image::Rgb<u8>, _>::from_pixel(
            1024,
            1536,
            image::Rgb([220, 220, 220]),
        )
        .save(&image_path)
        .unwrap();
        let pages = (0..142)
            .map(|index| {
                serde_json::json!({
                    "id": format!("page-{index}"),
                    "image_path": "page.png",
                    "cleaned_image_path": "missing-clean.png",
                    "bubbles": []
                })
            })
            .collect::<Vec<_>>();
        state::atomic_write_json(
            &job_dir.join("project.json"),
            &serde_json::json!({"state_revision": 0, "pages": pages}),
        )
        .unwrap();

        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir, None, exit_state).unwrap();
        let context = Context::default();
        app.context = Some(context.clone());
        let _ = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 800.0),
                )),
                ..Default::default()
            },
            |ctx| app.gallery_ui(ctx),
        );

        assert!(app.thumbnail_decodes < 142);
        assert!(app.thumb_textures.len() <= THUMB_TEXTURE_CACHE_CAPACITY);
        let thumbnail = app.thumb_textures.get(0).unwrap();
        assert!(thumbnail.size_vec2().x <= THUMBNAIL_MAX_EDGE as f32);
        assert!(thumbnail.size_vec2().y <= THUMBNAIL_MAX_EDGE as f32);
    }

    #[test]
    fn approval_preflight_finds_page_and_bubble_render_dirty() {
        let state = EditorState {
            value: serde_json::json!({
                "pages": [
                    {"render_dirty": false, "bubbles": [{"render_dirty": true}]},
                    {"render_dirty": true, "bubbles": []},
                    {"render_dirty": false, "bubbles": [{"render_dirty": false}]}
                ]
            }),
            job_dir: std::path::PathBuf::new(),
            image_path: std::path::PathBuf::new(),
        };
        assert_eq!(render_dirty_page_indices(&state), vec![0, 1]);
    }

    #[test]
    fn refresh_cached_pages_clears_only_reused_markers() {
        let mut state = serde_json::json!({
            "pages": [
                {
                    "id": "page-1",
                    "render_dirty": true,
                    "bubbles": [{"id": "bubble-1", "render_dirty": true}]
                },
                {
                    "id": "page-2",
                    "render_dirty": false,
                    "issues": [{"issue_type": "font_or_layout"}],
                    "correction_strokes": [{"mode": "cover", "points": [{"x": 1, "y": 2}]}],
                    "bubbles": [{
                        "id": "bubble-2",
                        "translation": "keep this edit",
                        "flagged": true,
                        "render_dirty": false
                    }]
                }
            ]
        });

        assert!(clear_reused_render_dirty_markers(&mut state, &[1]));
        assert_eq!(state["pages"][0]["render_dirty"], false);
        assert_eq!(state["pages"][0]["bubbles"][0]["render_dirty"], false);
        assert_eq!(state["pages"][1]["render_dirty"], true);
        assert_eq!(state["pages"][1]["bubbles"][0]["render_dirty"], false);
        assert_eq!(
            state["pages"][1]["bubbles"][0]["translation"],
            "keep this edit"
        );
        assert_eq!(state["pages"][1]["bubbles"][0]["flagged"], true);
        assert_eq!(
            state["pages"][1]["issues"][0]["issue_type"],
            "font_or_layout"
        );
        assert_eq!(state["pages"][1]["correction_strokes"][0]["mode"], "cover");
    }

    #[test]
    fn refresh_cached_pages_reports_exact_approval_batch_sizes() {
        let dirty: Vec<usize> = (0..142).collect();
        let batches = fukidashi_mcp::approval::partition_approval_batches(&dirty);
        assert_eq!(dirty.len(), 142);
        assert_eq!(
            batches.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![60, 60, 22]
        );

        let reused = 142usize.saturating_sub(dirty.len());
        assert_eq!(reused, 0);
        assert_eq!(batches.into_iter().flatten().collect::<Vec<_>>(), dirty);
    }

    #[test]
    fn native_request_fixes_serializes_flagged_bubbles_and_issues() {
        let feedback = native_review_feedback(&serde_json::json!({
            "pages": [{
                "issues": [
                    {"issue_type": "font_or_layout", "bbox": {"x1":1,"y1":2,"x2":8,"y2":9}},
                    {"issue_type": "wrong_translation", "bbox": {"x1":12,"y1":12,"x2":18,"y2":18}, "note":"Use the polite form", "corrected_text":"Xin chào"}
                ],
                "bubbles": [{"id":"b1","flagged":true,"bbox":{"x1":2,"y1":3,"x2":9,"y2":10},"text":"OCR","translation":"Dịch"}]
            }]
        }));
        assert_eq!(feedback.len(), 3);
        assert_eq!(feedback[0]["origin"], "image-pixels");
        assert_eq!(feedback[1]["page"], 0);
        assert_eq!(feedback[1]["bbox"]["x1"], 12);
        assert_eq!(feedback[1]["link_status"], "no_overlap");
        assert_eq!(feedback[1]["note"], "Use the polite form");
        assert_eq!(feedback[1]["corrected_text"], "Xin chào");
        assert_eq!(feedback[2]["issue_type"], "flagged_bubble");
        assert_eq!(feedback[2]["bubble_id"], "b1");
        assert_eq!(feedback[2]["source_ocr"], "OCR");
        assert_eq!(feedback[2]["current_translation"], "Dịch");
    }

    #[test]
    fn wrong_translation_feedback_links_only_a_unique_overlap() {
        let one = native_review_feedback(&serde_json::json!({
            "pages": [{
                "issues": [{"issue_type":"wrong_translation", "bbox":{"x1":5,"y1":5,"x2":15,"y2":15}}],
                "bubbles": [{"id":"b1","bbox":{"x1":10,"y1":10,"x2":20,"y2":20},"source_text":"One","translation":"Một"}]
            }]
        }));
        assert_eq!(one[0]["link_status"], "linked");
        assert_eq!(one[0]["bubble_id"], "b1");
        assert_eq!(one[0]["source_ocr"], "One");
        assert_eq!(one[0]["current_translation"], "Một");

        let ambiguous = native_review_feedback(&serde_json::json!({
            "pages": [{
                "issues": [{"issue_type":"wrong_translation", "bbox":{"x1":5,"y1":5,"x2":25,"y2":25}}],
                "bubbles": [
                    {"id":"b1","bbox":{"x1":0,"y1":0,"x2":15,"y2":15}},
                    {"id":"b2","bbox":{"x1":15,"y1":15,"x2":30,"y2":30}}
                ]
            }]
        }));
        assert_eq!(ambiguous[0]["link_status"], "ambiguous");
    }

    #[test]
    fn approval_keeps_review_unchanged_when_dirty_render_fails() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        let session = "native-approval-test".to_owned();
        let project = serde_json::json!({
            "state_revision": 0,
            "pages": [{
                "id": "page-1",
                "image_path": "missing-source.png",
                "cleaned_image_path": "missing-clean.png",
                "rendered_image_path": "missing-rendered.png",
                "render_dirty": true,
                "bubbles": []
            }]
        });
        state::atomic_write_json(&job_dir.join("project.json"), &project).unwrap();
        let review = ReviewState {
            review_session_id: session.clone(),
            revision: 4,
            status: "awaiting_review".to_owned(),
            action: None,
            feedback: Vec::new(),
            approved_pages: Vec::new(),
            consumed: false,
            audit: Vec::new(),
        };
        state::save_review_state(&job_dir, &review).unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir.clone(), Some(session), exit_state).unwrap();
        app.approve_and_export();
        wait_for_operation(&mut app);
        let persisted = read_review(&job_dir).unwrap();
        assert!(persisted.action.is_none());
        assert_eq!(persisted.status, "awaiting_review");
        assert!(
            app.error_message
                .as_deref()
                .is_some_and(|message| message.contains("could not be re-rendered"))
        );
    }

    #[test]
    fn approval_project_write_failure_remains_retryable() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        let session = "native-approval-retry-test".to_owned();
        let project = serde_json::json!({
            "state_revision": 0,
            "pages": [{
                "id": "page-1",
                "image_path": "source.png",
                "render_dirty": false,
                "bubbles": []
            }]
        });
        state::atomic_write_json(&job_dir.join("project.json"), &project).unwrap();
        let review = ReviewState {
            review_session_id: session.clone(),
            revision: 7,
            status: "awaiting_review".to_owned(),
            action: None,
            feedback: Vec::new(),
            approved_pages: Vec::new(),
            consumed: false,
            audit: Vec::new(),
        };
        state::save_review_state(&job_dir, &review).unwrap();

        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir.clone(), Some(session), exit_state).unwrap();
        let project_path = job_dir.join("project.json");
        let backup_path = job_dir.join("project-backup.json");
        std::fs::rename(&project_path, &backup_path).unwrap();
        std::fs::create_dir(&project_path).unwrap();

        app.approve_and_export();
        wait_for_operation(&mut app);

        assert!(app.review.action.is_none());
        assert_eq!(app.review.status, "awaiting_review");
        assert!(app.exit_action.is_none());
        assert!(
            app.error_message
                .as_deref()
                .is_some_and(|message| message.contains("save failed before approval"))
        );
        assert!(app.dirty_since.is_some());

        std::fs::remove_dir(&project_path).unwrap();
        std::fs::rename(&backup_path, &project_path).unwrap();
        app.context = Some(Context::default());
        app.approve_and_export();
        wait_for_operation(&mut app);

        assert_eq!(app.review.action.as_deref(), Some("approve_export"));
        assert_eq!(app.review.status, "approved");
        assert_eq!(app.exit_action, Some(ExitAction::Approve));
        let persisted = read_review(&job_dir).unwrap();
        assert_eq!(persisted.action.as_deref(), Some("approve_export"));
        assert_eq!(persisted.status, "approved");
    }

    #[test]
    fn save_render_dispatch_is_nonblocking_and_reentry_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        state::atomic_write_json(
            &job_dir.join("project.json"),
            &serde_json::json!({
                "state_revision": 0,
                "pages": [{
                    "id": "page-1",
                    "image_path": "source.png",
                    "render_dirty": false,
                    "bubbles": []
                }]
            }),
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir, None, exit_state).unwrap();
        app.context = Some(Context::default());

        app.save_and_render();
        assert!(app.operation_active());
        app.save_and_render();
        assert!(
            app.error_message
                .as_deref()
                .is_some_and(|message| message.contains("already running"))
        );
        wait_for_operation(&mut app);
        assert!(!app.operation_active());
    }

    #[test]
    fn autosave_does_not_enqueue_a_stale_snapshot_during_explicit_operation() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        state::atomic_write_json(
            &job_dir.join("project.json"),
            &serde_json::json!({
                "state_revision": 0,
                "pages": [{
                    "id": "page-1",
                    "image_path": "source.png",
                    "render_dirty": true,
                    "bubbles": []
                }]
            }),
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir, None, exit_state).unwrap();
        let (_tx, rx) = std::sync::mpsc::channel();
        app.operation = Some(OperationRuntime {
            kind: EditorOperationKind::SaveRender,
            rx,
            handle: None,
            current: 1,
            total: 2,
            message: "Rendering…".to_owned(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        app.dirty_since = Some(Instant::now() - Duration::from_secs(2));

        app.flush_save_if_due();

        assert!(app.dirty_since.is_some());
    }

    #[test]
    fn disconnected_worker_fails_operation_and_leaves_it_retryable() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        state::atomic_write_json(
            &job_dir.join("project.json"),
            &serde_json::json!({
                "state_revision": 0,
                "pages": [{
                    "id": "page-1",
                    "image_path": "source.png",
                    "render_dirty": false,
                    "bubbles": []
                }]
            }),
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir, None, exit_state).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        drop(sender);
        app.operation = Some(OperationRuntime {
            kind: EditorOperationKind::SaveRender,
            rx: receiver,
            handle: None,
            current: 0,
            total: 1,
            message: "Saving…".to_owned(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });

        app.poll_operation(&Context::default());

        assert!(!app.operation_active());
        assert!(
            app.error_message
                .as_deref()
                .is_some_and(|message| { message.contains("worker disconnected") })
        );
        assert!(app.dirty_since.is_some());
    }

    #[test]
    fn cancelled_operation_reloads_newer_worker_state_before_autosave() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        // UI starts from the older pre-operation revision A.
        state::atomic_write_json(
            &job_dir.join("project.json"),
            &serde_json::json!({
                "state_revision": 2,
                "pages": [{
                    "id": "page-1",
                    "image_path": "source.png",
                    "render_dirty": false,
                    "bubbles": []
                }]
            }),
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir.clone(), None, exit_state).unwrap();
        assert_eq!(app.state.as_ref().unwrap().value["state_revision"], 2);
        // The worker persists newer state B after rendering pages.
        state::atomic_write_json(
            &job_dir.join("project.json"),
            &serde_json::json!({
                "state_revision": 5,
                "pages": [{
                    "id": "page-1",
                    "image_path": "source.png",
                    "rendered_image_path": "rendered.png",
                    "render_dirty": false,
                    "bubbles": []
                }]
            }),
        )
        .unwrap();
        // The operation then fails/cancels while the UI still holds A.
        let (sender, receiver) = std::sync::mpsc::channel();
        drop(sender);
        app.operation = Some(OperationRuntime {
            kind: EditorOperationKind::Approve,
            rx: receiver,
            handle: None,
            current: 0,
            total: 1,
            message: "Saving…".to_owned(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        app.poll_operation(&Context::default());
        // The UI must install B, not re-arm an autosave of stale A.
        assert!(!app.operation_active());
        assert_eq!(app.state.as_ref().unwrap().value["state_revision"], 5);
        assert!(app.dirty_since.is_none());
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(job_dir.join("project.json")).unwrap()).unwrap();
        assert_eq!(persisted["state_revision"], 5);
    }

    #[test]
    fn request_fixes_writes_feedback_for_a_flagged_bubble() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        let session = "native-request-test".to_owned();
        let project = serde_json::json!({
            "state_revision": 0,
            "pages": [{
                "id": "page-1",
                "image_path": "source.png",
                "render_dirty": false,
                "bubbles": [{
                    "id": "bubble-1",
                    "bbox": {"x1": 1, "y1": 2, "x2": 10, "y2": 12},
                    "text": "OCR",
                    "translation": "Dịch",
                    "flagged": true
                }]
            }]
        });
        state::atomic_write_json(&job_dir.join("project.json"), &project).unwrap();
        state::save_review_state(
            &job_dir,
            &ReviewState {
                review_session_id: session.clone(),
                revision: 2,
                status: "awaiting_review".to_owned(),
                action: None,
                feedback: Vec::new(),
                approved_pages: Vec::new(),
                consumed: false,
                audit: Vec::new(),
            },
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir.clone(), Some(session), exit_state).unwrap();
        app.context = Some(egui::Context::default());
        app.request_fixes();
        let persisted = read_review(&job_dir).unwrap();
        assert_eq!(persisted.action.as_deref(), Some("request_fixes"));
        assert_eq!(persisted.feedback.len(), 1);
        assert_eq!(persisted.feedback[0]["bubble_id"], "bubble-1");
        assert_eq!(persisted.feedback[0]["issue_type"], "flagged_bubble");
        assert_eq!(
            persisted.feedback[0]["note"],
            "Flagged bubble requires review"
        );
        assert_eq!(persisted.feedback[0]["source_ocr"], "OCR");
        assert_eq!(persisted.feedback[0]["current_translation"], "Dịch");
    }

    #[test]
    fn request_fixes_does_not_mutate_review_when_project_save_fails() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        let session = "native-request-save-failure".to_owned();
        let project = serde_json::json!({
            "state_revision": 0,
            "pages": [{
                "id": "page-1",
                "image_path": "source.png",
                "render_dirty": false,
                "bubbles": [{
                    "id": "bubble-1",
                    "bbox": {"x1": 1, "y1": 2, "x2": 10, "y2": 12},
                    "text": "OCR",
                    "translation": "Dịch",
                    "flagged": true
                }]
            }]
        });
        let project_path = job_dir.join("project.json");
        state::atomic_write_json(&project_path, &project).unwrap();
        state::save_review_state(
            &job_dir,
            &ReviewState {
                review_session_id: session.clone(),
                revision: 2,
                status: "awaiting_review".to_owned(),
                action: None,
                feedback: Vec::new(),
                approved_pages: Vec::new(),
                consumed: false,
                audit: Vec::new(),
            },
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir.clone(), Some(session), exit_state).unwrap();
        std::fs::rename(&project_path, job_dir.join("project-backup.json")).unwrap();
        std::fs::create_dir(&project_path).unwrap();

        app.request_fixes();

        assert!(app.review.action.is_none());
        assert_eq!(app.review.status, "awaiting_review");
        assert!(app.exit_action.is_none());
        assert!(app.dirty_since.is_some());
        assert!(
            app.error_message
                .as_deref()
                .is_some_and(|message| message.contains("save failed before request"))
        );
    }

    #[test]
    fn close_request_is_canceled_until_save_or_worker_is_safe() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        state::atomic_write_json(
            &job_dir.join("project.json"),
            &serde_json::json!({
                "state_revision": 0,
                "pages": [{"id": "page-1", "image_path": "source.png", "bubbles": []}]
            }),
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir, None, exit_state).unwrap();
        app.dirty_since = Some(Instant::now());
        let ctx = Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .events
            .push(egui::ViewportEvent::Close);
        let _ = ctx.run(input, |ctx| {
            assert!(app.handle_close_request(ctx));
        });
        assert!(app.close_ready);
        assert!(app.dirty_since.is_none());

        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(app.handle_close_request(ctx));
        });
        assert!(!app.close_ready);
    }

    #[test]
    fn autosave_surfaces_permanent_write_failure_and_rearms_dirty_state() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        let project_path = job_dir.join("project.json");
        state::atomic_write_json(
            &project_path,
            &serde_json::json!({
                "state_revision": 0,
                "pages": [{"id": "page-1", "image_path": "source.png", "bubbles": []}]
            }),
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir, None, exit_state).unwrap();
        std::fs::rename(&project_path, app.job_dir.join("project-backup.json")).unwrap();
        std::fs::create_dir(&project_path).unwrap();
        app.dirty_since = Some(Instant::now() - Duration::from_secs(2));
        app.flush_save_if_due();

        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(10));
            app.poll_autosave_error();
            if app.error_message.is_some() {
                break;
            }
        }
        assert!(
            app.error_message
                .as_deref()
                .is_some_and(|message| message.contains("autosave failed"))
        );
        assert!(app.dirty_since.is_some());
        assert!(!app.save_pending.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn autosave_does_not_queue_a_second_snapshot_while_one_is_pending() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        let project_path = job_dir.join("project.json");
        state::atomic_write_json(
            &project_path,
            &serde_json::json!({
                "state_revision": 0,
                "pages": [{"id": "page-1", "image_path": "source.png", "bubbles": []}]
            }),
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir, None, exit_state).unwrap();
        app.state.as_mut().unwrap().value["state_revision"] = 1.into();
        app.dirty_since = Some(Instant::now() - Duration::from_secs(2));
        app.save_pending
            .store(true, std::sync::atomic::Ordering::Release);

        app.flush_save_if_due();

        assert!(app.dirty_since.is_some());
        app.save_pending
            .store(false, std::sync::atomic::Ordering::Release);
        std::thread::sleep(Duration::from_millis(50));
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project_path).unwrap()).unwrap();
        assert_eq!(persisted["state_revision"], 0);
    }

    #[test]
    fn close_request_stays_blocked_when_autosave_fails_between_frames() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        state::atomic_write_json(
            &job_dir.join("project.json"),
            &serde_json::json!({
                "state_revision": 0,
                "pages": [{"id": "page-1", "image_path": "source.png", "bubbles": []}]
            }),
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir, None, exit_state).unwrap();
        app.close_after_save = true;
        *app.save_error.lock().unwrap() = Some("disk write failed".to_owned());
        app.save_failed
            .store(true, std::sync::atomic::Ordering::Release);
        app.save_pending
            .store(false, std::sync::atomic::Ordering::Release);
        let ctx = Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .events
            .push(egui::ViewportEvent::Close);

        let _ = ctx.run(input, |ctx| {
            assert!(app.handle_close_request(ctx));
        });

        assert!(!app.close_ready);
        assert!(app.close_after_save);
        app.poll_autosave_error();
        assert!(app.dirty_since.is_some());
    }

    #[test]
    fn native_snapshot_rechecks_revision_after_waiting_for_render_lease() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        std::fs::write(job_dir.join("job.json"), b"{}").unwrap();
        let project_path = job_dir.join("project.json");
        state::atomic_write_json(
            &project_path,
            &serde_json::json!({
                "state_revision": 2,
                "pages": [{"id": "page-1", "image_path": "source.png", "bubbles": []}]
            }),
        )
        .unwrap();
        let holder = fukidashi_mcp::editor::acquire_editor_render_lock(&job_dir)
            .unwrap()
            .unwrap();
        let save_epoch = Arc::new(Mutex::new(0));
        let save_lock = Arc::new(Mutex::new(()));
        let writer_dir = job_dir.clone();
        let writer_epoch = Arc::clone(&save_epoch);
        let writer_lock = Arc::clone(&save_lock);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = std::thread::spawn(move || {
            save_editor_snapshot(
                &writer_dir,
                &serde_json::json!({
                    "state_revision": 1,
                    "pages": [{"id": "page-1", "image_path": "source.png", "bubbles": []}]
                }),
                &writer_epoch,
                &writer_lock,
                &cancel,
            )
        });
        std::thread::sleep(Duration::from_millis(50));
        drop(holder);
        let error = writer
            .join()
            .unwrap()
            .expect_err("stale snapshot must be rejected after lease acquisition");
        assert!(error.to_string().contains("stale editor snapshot"));
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project_path).unwrap()).unwrap();
        assert_eq!(persisted["state_revision"], 2);
    }

    #[test]
    fn worker_render_lock_retries_same_process_contention() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        std::fs::write(job_dir.join("job.json"), b"{}").unwrap();
        let holder = fukidashi_mcp::editor::acquire_editor_render_lock(&job_dir)
            .unwrap()
            .unwrap();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker_dir = job_dir.clone();
        let worker =
            std::thread::spawn(move || acquire_worker_render_lock(&worker_dir, &worker_cancel));

        std::thread::sleep(Duration::from_millis(100));
        drop(holder);
        let lease = worker
            .join()
            .unwrap()
            .expect("worker should keep retrying a busy lease")
            .expect("managed job should return a lease");
        drop(lease);
        assert!(!job_dir.join(".fukidashi-render.lock").exists());
    }

    #[test]
    fn autosave_rejects_a_snapshot_behind_persisted_revision() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        let project_path = job_dir.join("project.json");
        state::atomic_write_json(
            &project_path,
            &serde_json::json!({
                "state_revision": 2,
                "pages": [{"id": "page-1", "image_path": "source.png", "bubbles": []}]
            }),
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir, None, exit_state).unwrap();
        app.state.as_mut().unwrap().value["state_revision"] = 1.into();
        app.dirty_since = Some(Instant::now() - Duration::from_secs(2));
        app.flush_save_if_due();

        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(10));
            app.poll_autosave_error();
            if app.error_message.is_some() {
                break;
            }
        }
        assert!(
            app.error_message
                .as_deref()
                .is_some_and(|message| message.contains("stale editor snapshot"))
        );
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project_path).unwrap()).unwrap();
        assert_eq!(persisted["state_revision"], 2);
        assert!(app.dirty_since.is_some());
    }

    #[test]
    fn synchronous_project_save_rejects_a_snapshot_behind_persisted_revision() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        let project_path = job_dir.join("project.json");
        state::atomic_write_json(
            &project_path,
            &serde_json::json!({
                "state_revision": 2,
                "pages": [{"id": "page-1", "image_path": "source.png", "bubbles": []}]
            }),
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let app = EditorApp::new(job_dir, None, exit_state).unwrap();
        let current = app.state.as_ref().unwrap();
        let mut stale = EditorState {
            value: current.value.clone(),
            job_dir: current.job_dir.clone(),
            image_path: current.image_path.clone(),
        };
        stale.value["state_revision"] = 1.into();
        let error = app
            .save_project_sync(&stale)
            .expect_err("stale synchronous save must be rejected");
        assert!(error.to_string().contains("stale editor snapshot"));
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project_path).unwrap()).unwrap();
        assert_eq!(persisted["state_revision"], 2);
    }

    #[test]
    fn native_request_fixes_fails_fast_when_export_holds_render_lease() {
        let directory = tempfile::tempdir().unwrap();
        let job_dir = directory.path().to_path_buf();
        std::fs::write(job_dir.join("job.json"), b"{}").unwrap();
        let project = serde_json::json!({
            "state_revision": 0,
            "pages": [{
                "id": "page-1",
                "image_path": "source.png",
                "render_dirty": false,
                "bubbles": [{
                    "id": "bubble-1",
                    "bbox": {"x1": 1, "y1": 2, "x2": 10, "y2": 12},
                    "text": "OCR",
                    "translation": "Dịch",
                    "flagged": true
                }]
            }]
        });
        state::atomic_write_json(&job_dir.join("project.json"), &project).unwrap();
        let session = "native-request-busy".to_owned();
        state::save_review_state(
            &job_dir,
            &ReviewState {
                review_session_id: session.clone(),
                revision: 2,
                status: "awaiting_review".to_owned(),
                action: None,
                feedback: Vec::new(),
                approved_pages: Vec::new(),
                consumed: false,
                audit: Vec::new(),
            },
        )
        .unwrap();
        let exit_state = Arc::new(Mutex::new(crate::ExitState::default()));
        let mut app = EditorApp::new(job_dir.clone(), Some(session), exit_state).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder_dir = job_dir.clone();
        let holder_thread = std::thread::spawn(move || {
            let holder = fukidashi_mcp::editor::acquire_editor_render_lock(&holder_dir)
                .unwrap()
                .unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(holder);
        });
        ready_rx.recv().unwrap();
        let started = Instant::now();
        app.request_fixes();
        let elapsed = started.elapsed();
        let action_is_none = app.review.action.is_none();
        let surfaced_busy = app
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("save failed before request"));
        release_tx.send(()).unwrap();
        holder_thread.join().unwrap();
        assert!(elapsed < Duration::from_secs(2));
        assert!(action_is_none);
        assert!(surfaced_busy);
    }
}

impl EditorApp {
    fn handle_close_request(&mut self, ctx: &Context) -> bool {
        if self.close_after_save
            && !self.save_pending.load(std::sync::atomic::Ordering::Acquire)
            && !self.save_failed.load(std::sync::atomic::Ordering::Acquire)
            && self.dirty_since.is_none()
        {
            self.close_after_save = false;
            self.close_ready = true;
            ctx.request_repaint();
        }
        if self.close_ready {
            self.close_ready = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return true;
        }
        if !ctx.input(|input| input.viewport().close_requested()) {
            return false;
        }
        if self.operation_active() {
            self.close_after_operation = true;
            self.cancel_operation();
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.request_repaint_after(Duration::from_millis(100));
            return true;
        }
        if self.save_pending.load(std::sync::atomic::Ordering::Acquire)
            || self.save_failed.load(std::sync::atomic::Ordering::Acquire)
        {
            // The writer owns the snapshot already; waiting in the UI thread
            // would freeze egui behind fsync. Keep the window alive and poll
            // for completion or a surfaced failure on the next frame.
            self.close_after_save = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.request_repaint_after(Duration::from_millis(50));
            return true;
        }
        if self.dirty_since.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            let result = self
                .state
                .as_ref()
                .map(|state| self.save_project_sync(state));
            match result {
                Some(Ok(())) | None => {
                    self.dirty_since = None;
                    self.close_ready = true;
                    ctx.request_repaint();
                }
                Some(Err(error)) => {
                    self.error_message = Some(format!("save failed before close: {error}"));
                    self.dirty_since = Some(Instant::now());
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
            }
            return true;
        }
        false
    }
}

impl eframe::App for EditorApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.context = Some(ctx.clone());
        self.poll_autosave_error();
        if self.handle_close_request(ctx) {
            return;
        }
        self.poll_operation(ctx);
        self.apply_pending_page_jump();

        // Lazy-load icon textures on first frame.
        if self.icon_textures.is_empty() {
            self.icon_textures = toolbar::load_icons(ctx);
        }

        self.handle_shortcuts(ctx);

        // Top bar: branding + approve + Ctrl+S hint + dirty indicator + notify toast.
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("Fukidashi · {}", self.job_dir.display()));
                ui.separator();

                // Tool options (context-sensitive, right of the separator).
                self.tool_options_ui(ui);
                ui.separator();

                let page_count = self
                    .state
                    .as_ref()
                    .map(EditorState::page_count)
                    .unwrap_or(0);
                ui.label("Page");
                let page_input = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.page_jump_input)
                            .desired_width(42.0)
                            .hint_text("1"),
                    )
                    .on_hover_text("Type a page number and press Enter");
                // Enter can move focus away from a single-line TextEdit in
                // the same frame that it delivers the key event. Include
                // `lost_focus` so keyboard submission cannot be missed.
                let jump_on_enter = (page_input.has_focus() || page_input.lost_focus())
                    && ui.input(|input| input.key_pressed(egui::Key::Enter));
                if jump_on_enter || ui.button("Go").clicked() {
                    self.jump_to_page_input();
                }
                ui.label(format!("/ {page_count}"));
                // A Go click moves focus away from the input. Do not restore
                // the old page number before the staged target is applied at
                // the next frame.
                if self.pending_page_jump.is_none()
                    && !page_input.has_focus()
                    && !page_input.lost_focus()
                {
                    self.page_jump_input = (self.current_page + 1).to_string();
                }
                ui.separator();

                let operation_active = self.operation.is_some();
                if self.canvas.draw_issue_mode {
                    if ui
                        .add_enabled(
                            !operation_active,
                            egui::Button::new("Cancel missing-dialogue flag"),
                        )
                        .clicked()
                    {
                        self.cancel_missing_dialogue_flag();
                    }
                } else if ui
                    .add_enabled(
                        !operation_active,
                        egui::Button::new("Flag missing dialogue"),
                    )
                    .on_hover_text(
                        "Drag a rectangle around the missing balloon text, then review and send it",
                    )
                    .clicked()
                {
                    self.begin_missing_dialogue_flag();
                }
                if self.canvas.draw_issue_mode {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 180, 50),
                        "Drag over missing dialogue, then review the flag and send",
                    );
                }
                let has_retranslation_requests = self
                    .state
                    .as_ref()
                    .is_some_and(|state| !state.retranslation_requests().is_empty());
                let has_review_feedback = self
                    .state
                    .as_ref()
                    .is_some_and(|state| !native_review_feedback(&state.value).is_empty());
                if has_review_feedback
                    && ui
                        .add_enabled(
                            !operation_active,
                            egui::Button::new(if has_retranslation_requests {
                                "↻ Send request"
                            } else {
                                "Send request"
                            }),
                        )
                        .on_hover_text("Save this review feedback and return it to the translator")
                        .clicked()
                {
                    self.request_fixes();
                }
                if ui
                    .add_enabled(!operation_active, egui::Button::new("Approve & Export"))
                    .clicked()
                {
                    self.approve_and_export();
                }
                if ui
                    .add_enabled(
                        !operation_active,
                        egui::Button::new("↻ Refresh cached pages"),
                    )
                    .on_hover_text(
                        "Reconcile stale yellow markers against validated managed render caches",
                    )
                    .clicked()
                {
                    self.refresh_cached_pages();
                }
                if ui
                    .add_enabled(
                        !operation_active,
                        egui::Button::new("⚡ Save & Render  Ctrl+S"),
                    )
                    .clicked()
                {
                    self.save_and_render();
                }
                if self.operation.is_some() {
                    ui.separator();
                    let progress = self
                        .operation
                        .as_ref()
                        .map(|operation| {
                            format!(
                                "{} · {}/{}",
                                operation.message, operation.current, operation.total
                            )
                        })
                        .unwrap_or_default();
                    ui.colored_label(egui::Color32::LIGHT_BLUE, progress);
                    if ui.button("Cancel").clicked() {
                        self.cancel_operation();
                    }
                }
                let dirty = self
                    .current_page_view()
                    .is_some_and(|page| page.has_render_dirty());
                if dirty {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 180, 50),
                        "⚠ Changes need re-render",
                    );
                }

                if let Some(status) = &self.cache_plan_status {
                    ui.separator();
                    ui.colored_label(egui::Color32::LIGHT_BLUE, status);
                }

                // Notify toast — auto-dismiss after 3 s.
                if let Some((msg, at)) = &self.notify_message {
                    if at.elapsed().as_secs_f32() < 3.0 {
                        ui.separator();
                        ui.colored_label(egui::Color32::from_rgb(80, 220, 100), msg);
                    } else {
                        self.notify_message = None;
                    }
                }
            });
        });

        self.toolbar_ui(ctx);
        self.gallery_ui(ctx);
        self.inspector_ui(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            self.canvas_ui(ui);
        });

        self.flush_save_if_due();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // `handle_close_request` keeps the root viewport open until active
        // workers and queued saves have reached a terminal state. This is a
        // defensive fallback for platform shutdown events that bypass the
        // normal close event: never leave a worker detached for main() to
        // terminate underneath.
        if let Some(mut operation) = self.operation.take() {
            operation
                .cancel
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(handle) = operation.handle.take() {
                let _ = handle.join();
            }
        }
    }
}
