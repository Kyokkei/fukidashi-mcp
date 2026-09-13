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
use std::path::PathBuf;
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

pub struct EditorApp {
    /// `None` only transiently during construction of the surrounding `Box`.
    state: Option<EditorState>,
    review: ReviewState,
    job_dir: PathBuf,
    current_page: usize,
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
    exit_action: Option<ExitAction>,
    error_message: Option<String>,
    exit_state: Arc<Mutex<crate::ExitState>>,
    /// egui context, refreshed each frame in `update`.
    context: Option<Context>,
    pub icon_textures: std::collections::HashMap<&'static str, egui::TextureHandle>,
    notify_message: Option<(String, std::time::Instant)>,
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
        // Background autosave writer. `atomic_write_json` fsyncs, which can
        // block for seconds on slow disks — that must never happen on the egui
        // UI thread, so debounced saves are handed off here. `try_send` drops
        // the snapshot when the writer is still busy with an older one (the
        // next debounce fires anyway).
        {
            let writer_dir = job_dir.clone();
            let writer_epoch = Arc::clone(&save_epoch);
            std::thread::Builder::new()
                .name("editor-autosave".to_owned())
                .spawn(move || {
                    while let Ok((queued_epoch, value)) = save_rx.recv() {
                        // Skip snapshots superseded by a later synchronous save.
                        if *writer_epoch.lock().unwrap() > queued_epoch {
                            continue;
                        }
                        let path = writer_dir.join("project.json");
                        let _ = state::atomic_write_json(&path, &value);
                    }
                })
                .expect("spawn editor autosave thread");
        }
        Ok(EditorApp {
            state: Some(state),
            review,
            job_dir,
            current_page: 0,
            canvas: canvas::CanvasState::default(),
            page_textures: TextureCache::with_capacity(PAGE_TEXTURE_CACHE_CAPACITY),
            thumb_textures: TextureCache::with_capacity(THUMB_TEXTURE_CACHE_CAPACITY),
            thumbnail_decodes: 0,
            dirty_since: None,
            save_tx,
            save_epoch,
            exit_action: None,
            error_message: None,
            exit_state,
            context: None,
            icon_textures: std::collections::HashMap::new(),
            notify_message: None,
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

    /// Synchronous save used by explicit user-triggered one-shots (render,
    /// approve, request fixes, Ctrl+S, exit). Bumps the save epoch so any
    /// queued background snapshot from before this save is discarded — the
    /// in-memory state it carried is now older than what's on disk.
    fn save_project_sync(&self, state: &EditorState) -> anyhow::Result<()> {
        let result = state::save_project(state);
        if result.is_ok() {
            let mut epoch = self.save_epoch.lock().unwrap();
            *epoch = epoch.saturating_add(1);
        }
        result
    }

    fn flush_save_if_due(&mut self) {
        if let Some(since) = self.dirty_since {
            if since.elapsed() >= Duration::from_millis(800) {
                // Snapshot the epoch alongside the value: if a synchronous
                // save lands after this snapshot was taken, the writer sees a
                // newer epoch and discards the stale snapshot.
                let queued_epoch = *self.save_epoch.lock().unwrap();
                let value = self
                    .state
                    .as_ref()
                    .ok_or("editor state is unavailable")
                    .and_then(|state| {
                        self.save_tx
                            .try_send((queued_epoch, state.value.clone()))
                            .map_err(|_| "autosave writer is busy")
                    });
                match value {
                    Ok(()) => self.dirty_since = None,
                    Err(_) => {
                        // Writer busy with an older snapshot; retry on the
                        // next debounce without clearing the deadline.
                        self.dirty_since = Some(Instant::now());
                    }
                }
            }
        }
    }

    fn rerender_current_page(&mut self) {
        if let Err(error) = self.rerender_page_at(self.current_page) {
            self.error_message = Some(format!("render failed: {error}"));
        }
    }

    /// Render one page after saving the current in-memory edits, then reload
    /// the server-normalized state.  Approval uses this same path for every
    /// dirty page so it cannot record a review against stale bitmaps.
    fn rerender_page_at(&mut self, page_index: usize) -> anyhow::Result<()> {
        let image_path = self
            .state
            .as_ref()
            .and_then(|state| state.page(page_index))
            .and_then(|page| {
                page.rendered_image_path
                    .or(Some(page.cleaned_image_path))
                    .or(Some(page.source_image_path))
            })
            .ok_or_else(|| anyhow::anyhow!("page has no image path"))?;
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("editor state is unavailable"))?;
        // Inspector edits are debounced for normal work, but an explicit
        // render must include every edit made immediately before the click.
        self.save_project_sync(state)
            .map_err(|error| anyhow::anyhow!("save failed before render: {error}"))?;
        render::rerender_page(state, page_index, &image_path)?;

        let bytes = std::fs::read(self.job_dir.join("project.json"))
            .map_err(|error| anyhow::anyhow!("reload rendered project: {error}"))?;
        let value = serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| anyhow::anyhow!("parse rendered project: {error}"))?;
        if let Some(state) = self.state.as_mut() {
            state.value = value;
        }
        self.invalidate_page_textures(page_index);
        self.canvas.brush_overlay = None;
        self.canvas.brush_overlay_dirty = true;
        self.dirty_since = None;
        Ok(())
    }

    fn rerender_dirty_pages_before_approval(&mut self) -> anyhow::Result<()> {
        let dirty_pages = self
            .state
            .as_ref()
            .map(render_dirty_page_indices)
            .unwrap_or_default();
        if dirty_pages.is_empty() {
            return Ok(());
        }
        let original_page = self.current_page;
        for (position, page_index) in dirty_pages.iter().copied().enumerate() {
            self.current_page = page_index;
            self.error_message = Some(format!(
                "Re-rendering page {} of {} before approval…",
                position + 1,
                dirty_pages.len()
            ));
            if let Some(context) = &self.context {
                context.request_repaint();
            }
            self.rerender_page_at(page_index).map_err(|error| {
                anyhow::anyhow!("page {} could not be re-rendered: {error}", page_index + 1)
            })?;
        }
        let page_count = self
            .state
            .as_ref()
            .map(EditorState::page_count)
            .unwrap_or(0);
        self.current_page = original_page.min(page_count.saturating_sub(1));
        self.error_message = None;
        Ok(())
    }

    fn save_and_render(&mut self) {
        if let Some(state) = self.state.as_ref() {
            if let Err(e) = self.save_project_sync(state) {
                self.error_message = Some(format!("save failed: {e}"));
                return;
            }
        }
        let current_dirty = self
            .state
            .as_ref()
            .and_then(|state| state.page(self.current_page))
            .is_some_and(|page| {
                page.render_dirty || page.bubbles.iter().any(|bubble| bubble.render_dirty)
            });
        if current_dirty {
            if let Err(e) = self.rerender_page_at(self.current_page) {
                self.error_message =
                    Some(format!("render page {} failed: {e}", self.current_page + 1));
                return;
            }
            self.notify_message = Some((
                "✓ Saved and re-rendered current page".to_owned(),
                std::time::Instant::now(),
            ));
            self.invalidate_page_textures(self.current_page);
        } else {
            self.notify_message = Some(("✓ Saved".to_owned(), std::time::Instant::now()));
        }
        self.error_message = None;
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
        if self.review.action.is_some() || self.review.consumed {
            return;
        }
        if !self.review_file_is_current() {
            self.error_message = Some("review changed on disk; reopen this editor".to_owned());
            return;
        }
        if let Err(error) = self.rerender_dirty_pages_before_approval() {
            self.error_message = Some(format!(
                "approval blocked until all dirty pages render successfully: {error}"
            ));
            return;
        }
        self.review.approved_pages =
            (0..self.state.as_ref().map(|s| s.page_count()).unwrap_or(0)).collect();
        self.review.status = "approved".to_owned();
        self.review.action = Some("approve_export".to_owned());
        self.review.audit.push(serde_json::json!({
            "event": "approve_export",
            "review_session_id": self.review.review_session_id.clone(),
            "revision": self.review.revision,
        }));
        if let Some(state) = self.state.as_ref() {
            if let Err(error) = self.save_project_sync(state) {
                self.error_message = Some(format!("save failed before approval: {error}"));
                return;
            }
        }
        if let Err(e) = state::save_review_state(&self.job_dir, &self.review) {
            self.error_message = Some(format!("save review failed: {e}"));
            return;
        }
        self.exit_action = Some(ExitAction::Approve);
        if let Ok(mut s) = self.exit_state.lock() {
            s.action = Some(ExitAction::Approve);
        }
        self.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
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
        self.review.feedback = derived_feedback;
        if self.review.feedback.is_empty() {
            self.error_message = Some(
                "request fixes needs a flagged bubble or an issue on at least one page".to_owned(),
            );
            return;
        }
        self.review.status = "fixes_requested".to_owned();
        self.review.action = Some("request_fixes".to_owned());
        self.review.audit.push(serde_json::json!({
            "event": "request_fixes",
            "review_session_id": self.review.review_session_id.clone(),
            "revision": self.review.revision,
        }));
        if let Some(state) = self.state.as_ref() {
            if let Err(error) = self.save_project_sync(state) {
                self.error_message = Some(format!("save failed before request: {error}"));
                return;
            }
        }
        if let Err(e) = state::save_review_state(&self.job_dir, &self.review) {
            self.error_message = Some(format!("save review failed: {e}"));
            return;
        }
        self.exit_action = Some(ExitAction::RequestFixes);
        if let Ok(mut s) = self.exit_state.lock() {
            s.action = Some(ExitAction::RequestFixes);
        }
        self.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
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
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            self.set_active_tool(canvas::ActiveTool::Select);
                        }
                        egui::Key::O
                            if tool_shortcuts_allowed(text_focus)
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            self.set_active_tool(canvas::ActiveTool::DrawBubble);
                        }
                        egui::Key::T
                            if tool_shortcuts_allowed(text_focus)
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            self.set_active_tool(canvas::ActiveTool::AddText);
                        }
                        egui::Key::OpenBracket
                            if tool_shortcuts_allowed(text_focus)
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            self.canvas.brush_radius =
                                adjust_brush_radius(self.canvas.brush_radius, -4.0);
                        }
                        egui::Key::CloseBracket
                            if tool_shortcuts_allowed(text_focus)
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            self.canvas.brush_radius =
                                adjust_brush_radius(self.canvas.brush_radius, 4.0);
                        }
                        egui::Key::Delete | egui::Key::Backspace
                            if tool_shortcuts_allowed(text_focus)
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
        if ctrl_s && !text_focus {
            self.save_and_render();
        }
        if next != self.current_page {
            self.cancel_drag();
            self.current_page = next;
            self.canvas.selected = None;
            self.canvas.selected_issue = None;
            self.canvas.brush_overlay_dirty = true;
            self.canvas.fit_applied = false;
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

fn tool_shortcuts_allowed(text_focus: bool) -> bool {
    !text_focus
}

fn adjust_brush_radius(radius: f32, delta: f32) -> f32 {
    (radius + delta).clamp(4.0, 80.0)
}

fn render_dirty_page_indices(state: &EditorState) -> Vec<usize> {
    (0..state.page_count())
        .filter(|&index| state.page_has_render_dirty(index))
        .collect()
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
                    "origin": "image-pixels",
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

    #[test]
    fn bracket_radius_changes_by_four_and_clamps() {
        assert_eq!(adjust_brush_radius(20.0, -4.0), 16.0);
        assert_eq!(adjust_brush_radius(4.0, -4.0), 4.0);
        assert_eq!(adjust_brush_radius(80.0, 4.0), 80.0);
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
        let persisted = read_review(&job_dir).unwrap();
        assert!(persisted.action.is_none());
        assert_eq!(persisted.status, "awaiting_review");
        assert!(
            app.error_message
                .as_deref()
                .is_some_and(|message| message.contains("approval blocked"))
        );
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
}

impl eframe::App for EditorApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.context = Some(ctx.clone());

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

                if ui.button("Approve & Export").clicked() {
                    self.approve_and_export();
                }
                if ui.button("⚡ Save & Render  Ctrl+S").clicked() {
                    self.save_and_render();
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
        if let Some(state) = self.state.as_ref() {
            let _ = self.save_project_sync(state);
        }
    }
}
