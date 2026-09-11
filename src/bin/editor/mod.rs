//! Native `egui` QA editor application module.

#![allow(
    clippy::bind_instead_of_map,
    clippy::collapsible_if,
    clippy::items_after_test_module,
    clippy::let_and_return
)]

pub mod canvas;
pub mod gallery;
pub mod inspector;
pub mod render;
pub mod state;

use std::collections::HashMap;
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
    page_textures: HashMap<usize, TextureHandle>,
    thumb_textures: HashMap<usize, TextureHandle>,
    dirty_since: Option<Instant>,
    exit_action: Option<ExitAction>,
    error_message: Option<String>,
    exit_state: Arc<Mutex<crate::ExitState>>,
    /// egui context, refreshed each frame in `update`.
    context: Option<Context>,
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
        Ok(EditorApp {
            state: Some(state),
            review,
            job_dir,
            current_page: 0,
            canvas: canvas::CanvasState::default(),
            page_textures: HashMap::new(),
            thumb_textures: HashMap::new(),
            dirty_since: None,
            exit_action: None,
            error_message: None,
            exit_state,
            context: None,
        })
    }

    fn ctx(&self) -> Context {
        self.context
            .clone()
            .expect("egui context available during update")
    }

    fn current_page_view(&self) -> Option<PageView> {
        self.state
            .as_ref()?
            .pages()
            .into_iter()
            .nth(self.current_page)
    }

    fn load_texture_for(&mut self, path: &PathBuf, key: &str) -> TextureHandle {
        let expected = format!("page-{key}");
        let cached = self.page_textures.get(&self.current_page);
        let needs_reload = match cached {
            Some(tex) => tex.name() != expected,
            None => true,
        };
        if needs_reload {
            let tex = self.load_image_texture(path, &expected);
            self.page_textures.insert(self.current_page, tex);
        }
        self.page_textures.get(&self.current_page).cloned().unwrap()
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
        if let Some(existing) = self.thumb_textures.get(&index) {
            return Some(existing.clone());
        }
        let path = page
            .rendered_image_path
            .clone()
            .or_else(|| Some(page.cleaned_image_path.clone()))
            .filter(|p| p.exists())
            .or_else(|| Some(page.source_image_path.clone()))?;
        let tex = self.load_image_texture(&path, &format!("thumb-{index}"));
        self.thumb_textures.insert(index, tex.clone());
        Some(tex)
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

    fn flush_save_if_due(&mut self) {
        if let Some(since) = self.dirty_since {
            if since.elapsed() >= Duration::from_millis(800) {
                if let Some(state) = self.state.as_ref() {
                    if let Err(e) = state::save_project(state) {
                        self.error_message = Some(format!("save failed: {e}"));
                    } else {
                        self.dirty_since = None;
                    }
                }
            }
        }
    }

    fn rerender_current_page(&mut self) {
        let image_path = match self.state.as_ref().and_then(|s| {
            s.pages().into_iter().nth(self.current_page).map(|p| {
                let path = p
                    .rendered_image_path
                    .clone()
                    .or_else(|| Some(p.cleaned_image_path.clone()))
                    .or_else(|| Some(p.source_image_path.clone()));
                path
            })
        }) {
            Some(Some(path)) => path,
            _ => {
                self.error_message = Some("page has no image path".to_owned());
                return;
            }
        };
        // Inspector edits are debounced for normal work, but an explicit
        // render must include every edit made immediately before the click.
        if let Some(state) = self.state.as_ref()
            && let Err(error) = state::save_project(state)
        {
            self.error_message = Some(format!("save failed before render: {error}"));
            return;
        }
        let state_ref = self.state.as_ref().unwrap();
        match render::rerender_page(state_ref, self.current_page, &image_path) {
            Ok(_) => {
                if let Ok(bytes) = std::fs::read(self.job_dir.join("project.json")) {
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        if let Some(s) = self.state.as_mut() {
                            s.value = value;
                        }
                    }
                }
                self.page_textures.clear();
                self.thumb_textures.clear();
                self.canvas.brush_overlay = None;
                self.canvas.brush_overlay_dirty = true;
                self.error_message = None;
                self.dirty_since = None;
            }
            Err(e) => {
                self.error_message = Some(format!("render failed: {e}"));
            }
        }
    }

    fn undo_stroke(&mut self) {
        if let Some(state) = self.state.as_mut() {
            if state.pop_stroke(self.current_page).is_some() {
                self.canvas.brush_overlay_dirty = true;
                self.schedule_save();
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
            if let Err(error) = state::save_project(state) {
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
        self.review.status = "fixes_requested".to_owned();
        self.review.action = Some("request_fixes".to_owned());
        self.review.audit.push(serde_json::json!({
            "event": "request_fixes",
            "review_session_id": self.review.review_session_id.clone(),
            "revision": self.review.revision,
        }));
        if let Some(state) = self.state.as_ref() {
            if let Err(error) = state::save_project(state) {
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
        let mut save = false;
        // egui gives text editors keyboard focus through Memory. Keep editor
        // navigation and brush shortcuts out of text fields/modal-like widgets
        // while allowing Ctrl+Z/S to retain their normal document semantics.
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
                            if *modifiers == egui::Modifiers::NONE =>
                        {
                            next = next.saturating_sub(1);
                        }
                        egui::Key::D | egui::Key::ArrowRight
                            if *modifiers == egui::Modifiers::NONE =>
                        {
                            next = (next + 1).min(count.saturating_sub(1));
                        }
                        egui::Key::Z if modifiers.ctrl => undo = true,
                        egui::Key::S if modifiers.ctrl => save = true,
                        egui::Key::B
                            if tool_shortcuts_allowed(text_focus)
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            self.canvas.brush_active = !self.canvas.brush_active;
                            if self.canvas.brush_active {
                                self.canvas.current_variant = canvas::Variant::Cleaned;
                            }
                        }
                        egui::Key::V
                            if tool_shortcuts_allowed(text_focus)
                                && *modifiers == egui::Modifiers::NONE =>
                        {
                            self.canvas.brush_active = false;
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
                        egui::Key::Escape => {
                            self.cancel_drag();
                            self.canvas.brush_active = false;
                        }
                        _ => {}
                    }
                }
            }
        });
        if next != self.current_page {
            self.cancel_drag();
            self.current_page = next;
            self.canvas.selected = None;
            self.canvas.brush_overlay_dirty = true;
            self.canvas.fit_applied = false;
            self.page_textures.clear();
        }
        if undo {
            self.undo_stroke();
        }
        if save {
            if let Some(state) = self.state.as_ref() {
                let _ = state::save_project(state);
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
}

impl eframe::App for EditorApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.context = Some(ctx.clone());
        self.handle_shortcuts(ctx);

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("Fukidashi · {}", self.job_dir.display()));
                ui.separator();
                if ui.button("Re-render page").clicked() {
                    self.rerender_current_page();
                }
                if ui.button("Approve & Export").clicked() {
                    self.approve_and_export();
                }
                if ui.button("Request Fixes").clicked() {
                    self.request_fixes();
                }
                let dirty = self
                    .current_page_view()
                    .is_some_and(|page| page.render_dirty || page.has_flagged_or_dirty());
                if dirty {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 180, 50),
                        "⚠️ Changes need re-render",
                    );
                }
            });
        });

        self.gallery_ui(ctx);
        self.inspector_ui(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            self.canvas_ui(ui);
        });

        self.flush_save_if_due();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(state) = self.state.as_ref() {
            let _ = state::save_project(state);
        }
    }
}
