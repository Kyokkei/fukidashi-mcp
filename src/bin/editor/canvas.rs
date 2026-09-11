//! Central canvas: viewport pan/zoom, bubble hit-testing, 8-handle resize, and
//! the correction brush.

#![allow(
    clippy::collapsible_if,
    clippy::if_same_then_else,
    clippy::unnecessary_map_or
)]

use egui::{Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use fukidashi_mcp::domain::Rect as DomainRect;

use super::EditorApp;
use super::state::{CorrectionStroke, PageView};

/// Resize handle layout (8 handles).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    Tl,
    Tc,
    Tr,
    Ml,
    Mr,
    Bl,
    Bc,
    Br,
}

#[derive(Clone, Copy, PartialEq)]
pub enum BrushMode {
    Cover,
    Restore,
}

/// Which page artifact the canvas is showing. Brush corrections only apply
/// while the cleaned variant is selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Source,
    Cleaned,
    Rendered,
}

#[derive(Clone, PartialEq, Default)]
pub enum DragState {
    #[default]
    None,
    Pan,
    TranslateBubble {
        page_index: usize,
        bubble_index: usize,
        start_bbox: DomainRect,
        start_pos: Pos2,
    },
    ResizeBubble {
        page_index: usize,
        bubble_index: usize,
        start_bbox: DomainRect,
        start_pos: Pos2,
        handle: Handle,
    },
    BrushStroke {
        page_index: usize,
        mode: BrushMode,
        color: String,
        radius: f32,
        points: Vec<(f32, f32)>,
    },
}

pub struct CanvasState {
    pub pan: Vec2,
    pub zoom: f32,
    pub selected: Option<(usize, usize)>,
    pub drag: DragState,
    /// Snapshot used to roll back a canceled bubble gesture.  `egui` can stop
    /// delivering pointer events when a window loses focus, so cleanup cannot
    /// rely on a final `drag_stopped` frame alone.
    pub drag_snapshot: Option<serde_json::Value>,
    pub drag_page_dirty_before: Option<bool>,
    pub drag_moved: bool,
    pub pan_start: Option<Vec2>,
    pub brush_active: bool,
    pub brush_mode: BrushMode,
    pub brush_radius: f32,
    pub brush_color: egui::Color32,
    pub undo_stack: std::collections::VecDeque<usize>,
    pub brush_overlay: Option<egui::TextureHandle>,
    pub brush_overlay_dirty: bool,
    /// False until first auto-fit; prevents re-centering every frame.
    pub fit_applied: bool,
    /// Explicit source / cleaned / rendered picker. Defaults to cleaned so
    /// the brush edits the inpaint base rather than a typeset render.
    pub current_variant: Variant,
}

impl Default for CanvasState {
    fn default() -> Self {
        CanvasState {
            pan: Vec2::ZERO,
            zoom: 1.0,
            selected: None,
            drag: DragState::None,
            drag_snapshot: None,
            drag_page_dirty_before: None,
            drag_moved: false,
            pan_start: None,
            brush_active: false,
            brush_mode: BrushMode::Cover,
            brush_radius: 24.0,
            brush_color: egui::Color32::WHITE,
            undo_stack: std::collections::VecDeque::new(),
            brush_overlay: None,
            brush_overlay_dirty: false,
            fit_applied: false,
            current_variant: Variant::Cleaned,
        }
    }
}

impl CanvasState {
    fn image_to_screen(&self, image_pt: Pos2, canvas_origin: Pos2) -> Pos2 {
        canvas_origin + image_pt.to_vec2() * self.zoom + self.pan
    }

    fn screen_to_image(&self, screen_pt: Pos2, canvas_origin: Pos2) -> Pos2 {
        let v = (screen_pt - canvas_origin - self.pan) / self.zoom;
        Pos2::new(v.x, v.y)
    }

    fn screen_rect(&self, bbox: &DomainRect, origin: Pos2) -> Rect {
        let tl = self.image_to_screen(Pos2::new(bbox.x1, bbox.y1), origin);
        let br = self.image_to_screen(Pos2::new(bbox.x2, bbox.y2), origin);
        Rect::from_two_pos(tl, br)
    }

    fn handle_positions(&self, rect: Rect) -> [(Handle, Pos2); 8] {
        let c = rect.center();
        [
            (Handle::Tl, rect.left_top()),
            (Handle::Tc, Pos2::new(c.x, rect.top())),
            (Handle::Tr, rect.right_top()),
            (Handle::Ml, Pos2::new(rect.left(), c.y)),
            (Handle::Mr, Pos2::new(rect.right(), c.y)),
            (Handle::Bl, rect.left_bottom()),
            (Handle::Bc, Pos2::new(c.x, rect.bottom())),
            (Handle::Br, rect.right_bottom()),
        ]
    }

    fn hit_handle(&self, pointer: Pos2, rect: Rect) -> Option<Handle> {
        for (handle, pos) in self.handle_positions(rect) {
            if pointer.distance(pos) <= 12.0 {
                return Some(handle);
            }
        }
        None
    }

    fn hit_bubble(&self, pointer: Pos2, origin: Pos2, page: &PageView) -> Option<usize> {
        let image_pt = self.screen_to_image(pointer, origin);
        for (idx, bubble) in page.bubbles.iter().enumerate().rev() {
            if let Some(bbox) = &bubble.bbox {
                if image_pt.x >= bbox.x1
                    && image_pt.x <= bbox.x2
                    && image_pt.y >= bbox.y1
                    && image_pt.y <= bbox.y2
                {
                    return Some(idx);
                }
            }
        }
        None
    }

    /// Anchor a pan gesture to the press frame. `Response::drag_delta()` is
    /// cumulative, so adding it on every frame produces accelerating drift.
    pub(crate) fn pan_from_drag(start_pan: Vec2, drag_delta: Vec2) -> Vec2 {
        start_pan + drag_delta
    }

    pub(crate) fn clamp_bbox(mut bbox: DomainRect, image_size: Vec2) -> DomainRect {
        const MIN_SIZE: f32 = 2.0;
        let width = (bbox.x2 - bbox.x1)
            .max(MIN_SIZE)
            .min(image_size.x.max(MIN_SIZE));
        let height = (bbox.y2 - bbox.y1)
            .max(MIN_SIZE)
            .min(image_size.y.max(MIN_SIZE));
        bbox.x1 = bbox.x1.clamp(0.0, (image_size.x - width).max(0.0));
        bbox.y1 = bbox.y1.clamp(0.0, (image_size.y - height).max(0.0));
        bbox.x2 = (bbox.x1 + width).min(image_size.x.max(width));
        bbox.y2 = (bbox.y1 + height).min(image_size.y.max(height));
        bbox
    }
}

impl EditorApp {
    pub fn canvas_ui(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_rect_before_wrap();
        let origin = available.min;

        let page = match self.current_page_view() {
            Some(p) => p,
            None => {
                ui.centered_and_justified(|ui| ui.label("No pages loaded."));
                return;
            }
        };

        // Choose the image from the explicit variant picker (default: cleaned).
        let image_path = match self.canvas.current_variant {
            Variant::Source => Some(page.source_image_path.clone()),
            Variant::Cleaned => Some(page.cleaned_image_path.clone()),
            Variant::Rendered => page.rendered_image_path.clone(),
        }
        .filter(|p| p.exists());
        let Some(image_path) = image_path else {
            ui.centered_and_justified(|ui| ui.label("No image for this page."));
            return;
        };

        let variant_key = match self.canvas.current_variant {
            Variant::Source => "source",
            Variant::Cleaned => "cleaned",
            Variant::Rendered => "rendered",
        };
        let tex = self.load_texture_for(&image_path, &format!("{}-{variant_key}", page.id));
        let image_size = tex.size_vec2();
        // Auto-fit + center on first layout.
        if !self.canvas.fit_applied && image_size.x > 0.0 {
            let fit =
                (available.width() / image_size.x).min(available.height() / image_size.y) * 0.95;
            self.canvas.zoom = fit;
            let base_rect = Rect::from_min_size(origin, image_size * fit);
            self.canvas.pan = Vec2::new(
                (available.width() - base_rect.width()) / 2.0,
                (available.height() - base_rect.height()) / 2.0,
            );
            self.canvas.fit_applied = true;
        }

        let show_brush = self.canvas.brush_active
            && self.current_variant_is_cleaned()
            && !page.correction_strokes.is_empty();

        let response = ui.allocate_rect(available, Sense::click_and_drag());

        self.handle_canvas_input(&response, &page, &image_path, origin, image_size);

        let painter = ui.painter();
        let base_rect =
            Rect::from_min_size(origin + self.canvas.pan, image_size * self.canvas.zoom);
        painter.image(
            tex.id(),
            base_rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            egui::Color32::WHITE,
        );

        if show_brush {
            let overlay = self.load_brush_overlay(&page);
            painter.image(
                overlay.id(),
                base_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }

        // Bubble overlays (culled to the visible viewport).
        for (idx, bubble) in page.bubbles.iter().enumerate() {
            let Some(bbox) = &bubble.bbox else { continue };
            let rect = self.canvas.screen_rect(bbox, origin);
            if rect.min.x > available.max.x
                || rect.max.x < available.min.x
                || rect.min.y > available.max.y
                || rect.max.y < available.min.y
            {
                continue;
            }
            let fill = if bubble.render_dirty {
                egui::Color32::from_rgba_unmultiplied(255, 200, 80, 40)
            } else if bubble.flagged {
                egui::Color32::from_rgba_unmultiplied(255, 90, 90, 40)
            } else {
                egui::Color32::from_rgba_unmultiplied(80, 200, 255, 32)
            };
            painter.rect_filled(rect, 2.0, fill);
            painter.rect_stroke(
                rect,
                2.0_f32,
                Stroke::new(1.5_f32, egui::Color32::from_rgb(80, 200, 255)),
                StrokeKind::Middle,
            );
            if Some((self.current_page, idx)) == self.canvas.selected {
                for (_h, pos) in self.canvas.handle_positions(rect) {
                    painter.rect_filled(
                        Rect::from_center_size(pos, Vec2::splat(8.0)),
                        1.0,
                        egui::Color32::WHITE,
                    );
                    painter.rect_stroke(
                        Rect::from_center_size(pos, Vec2::splat(8.0)),
                        1.0,
                        Stroke::new(1.0_f32, egui::Color32::BLACK),
                        StrokeKind::Middle,
                    );
                }
            }
        }

        // Live brush stroke preview.
        if let DragState::BrushStroke {
            points,
            radius,
            mode,
            color,
            ..
        } = &self.canvas.drag
        {
            let c = hex_to_color32(color.clone()).unwrap_or(egui::Color32::WHITE);
            let draw = match mode {
                BrushMode::Cover => c,
                BrushMode::Restore => egui::Color32::from_rgba_unmultiplied(0, 255, 0, 120),
            };
            for p in points {
                let sp = self.canvas.image_to_screen(Pos2::new(p.0, p.1), origin);
                painter.circle_filled(sp, *radius * self.canvas.zoom, draw);
            }
        }

        // Live brush cursor ring when active on cleaned variant.
        if self.canvas.brush_active && self.current_variant_is_cleaned() {
            if let Some(hover_pos) = response.hover_pos() {
                painter.circle_stroke(
                    hover_pos,
                    self.canvas.brush_radius * self.canvas.zoom,
                    egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(255, 90, 90)),
                );
            }
        }
    }

    fn handle_canvas_input(
        &mut self,
        response: &egui::Response,
        page: &PageView,
        _image_path: &std::path::Path,
        origin: Pos2,
        image_size: Vec2,
    ) {
        let ctx = self.ctx();
        let pointer = response.interact_pointer_pos().unwrap_or(origin);

        // A focus loss can strand a captured pointer. Roll back an uncommitted
        // bubble transform and discard an unfinished brush stroke.
        let unfocused = ctx.input(|i| i.viewport().focused == Some(false));
        if unfocused {
            self.cancel_drag();
            return;
        }

        // Context cursor feedback.
        if matches!(self.canvas.drag, DragState::Pan) {
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if matches!(self.canvas.drag, DragState::TranslateBubble { .. }) {
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if let DragState::ResizeBubble { handle, .. } = &self.canvas.drag {
            ctx.set_cursor_icon(match handle {
                Handle::Tl | Handle::Br => egui::CursorIcon::ResizeNwSe,
                Handle::Tr | Handle::Bl => egui::CursorIcon::ResizeNeSw,
                Handle::Tc | Handle::Bc => egui::CursorIcon::ResizeVertical,
                Handle::Ml | Handle::Mr => egui::CursorIcon::ResizeHorizontal,
            });
        } else if self.canvas.brush_active && self.current_variant_is_cleaned() {
            ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
        } else if let Some((pi, bi)) = self.canvas.selected {
            if pi == self.current_page {
                if let Some(bubble) = page.bubbles.get(bi) {
                    let rect = self
                        .canvas
                        .screen_rect(bubble.bbox.as_ref().unwrap_or(&ZERO_RECT), origin);
                    if let Some(handle) = self.canvas.hit_handle(pointer, rect) {
                        ctx.set_cursor_icon(match handle {
                            Handle::Tl | Handle::Br => egui::CursorIcon::ResizeNwSe,
                            Handle::Tr | Handle::Bl => egui::CursorIcon::ResizeNeSw,
                            Handle::Tc | Handle::Bc => egui::CursorIcon::ResizeVertical,
                            Handle::Ml | Handle::Mr => egui::CursorIcon::ResizeHorizontal,
                        });
                    } else if self.canvas.hit_bubble(pointer, origin, page) == Some(bi) {
                        ctx.set_cursor_icon(egui::CursorIcon::Grab);
                    }
                }
            }
        } else if self.canvas.hit_bubble(pointer, origin, page).is_some() {
            ctx.set_cursor_icon(egui::CursorIcon::Grab);
        }

        // Reset any in-progress drag on release (except brush, handled below).
        if response.drag_stopped() {
            self.finish_drag();
            return;
        }
        if !ctx.input(|i| i.pointer.any_down()) && self.canvas.drag != DragState::None {
            self.finish_drag();
            return;
        }

        // A right-button gesture is Restore while brush is active. Otherwise
        // secondary and middle drags are viewport pan gestures.
        let right_brush = self.canvas.brush_active
            && self.current_variant_is_cleaned()
            && (response.dragged_by(egui::PointerButton::Secondary)
                || response.drag_started_by(egui::PointerButton::Secondary));
        if right_brush {
            self.paint_at(pointer, origin, BrushMode::Restore, response);
            return;
        }
        if response.drag_started_by(egui::PointerButton::Middle)
            || response.drag_started_by(egui::PointerButton::Secondary)
            || response.dragged_by(egui::PointerButton::Middle)
            || response.dragged_by(egui::PointerButton::Secondary)
        {
            if !matches!(self.canvas.drag, DragState::Pan) {
                self.canvas.drag = DragState::Pan;
                self.canvas.pan_start = Some(self.canvas.pan);
            }
            self.canvas.pan = CanvasState::pan_from_drag(
                self.canvas.pan_start.unwrap_or(self.canvas.pan),
                response.drag_delta(),
            );
            return;
        }

        // Zoom toward pointer on scroll.
        let (scroll, zoom_modifier) = ctx.input(|i| {
            (
                i.raw_scroll_delta.y,
                i.modifiers.ctrl || i.modifiers.command,
            )
        });
        if scroll != 0.0 && zoom_modifier {
            let factor = if scroll > 0.0 { 1.1 } else { 1.0 / 1.1 };
            let mouse = response.interact_pointer_pos().unwrap_or(origin);
            let before = self.canvas.screen_to_image(mouse, origin);
            self.canvas.zoom = (self.canvas.zoom * factor).clamp(0.25, 4.0);
            let after = self.canvas.image_to_screen(before, origin);
            self.canvas.pan += mouse - after;
            ctx.request_repaint();
            return;
        }

        // Brush painting (only when active and on the cleaned variant).
        if self.canvas.brush_active
            && self.current_variant_is_cleaned()
            && (response.dragged_by(egui::PointerButton::Primary)
                || response.clicked_by(egui::PointerButton::Primary))
        {
            let click_only = response.clicked_by(egui::PointerButton::Primary)
                && !response.dragged_by(egui::PointerButton::Primary);
            self.paint_at(pointer, origin, self.canvas.brush_mode, response);
            if click_only {
                self.finish_drag();
            }
            return;
        }

        // Selection / translate / resize while dragging a (possibly new) bubble.
        if response.drag_started_by(egui::PointerButton::Primary)
            || response.dragged_by(egui::PointerButton::Primary)
        {
            // Start a drag if none is in progress.
            if self.canvas.drag == DragState::None {
                let press = ctx.input(|i| i.pointer.press_origin()).unwrap_or(pointer);
                let mut started = false;
                if let Some((pi, bi)) = self.canvas.selected {
                    if pi == self.current_page {
                        if let Some(bubble) = page.bubbles.get(bi) {
                            let rect = self
                                .canvas
                                .screen_rect(bubble.bbox.as_ref().unwrap_or(&ZERO_RECT), origin);
                            if let Some(handle) = self.canvas.hit_handle(press, rect) {
                                self.canvas.drag = DragState::ResizeBubble {
                                    page_index: pi,
                                    bubble_index: bi,
                                    start_bbox: bubble.bbox.unwrap_or(ZERO_RECT),
                                    start_pos: press,
                                    handle,
                                };
                                started = true;
                            } else if self.canvas.hit_bubble(press, origin, page) == Some(bi) {
                                self.canvas.drag = DragState::TranslateBubble {
                                    page_index: pi,
                                    bubble_index: bi,
                                    start_bbox: bubble.bbox.unwrap_or(ZERO_RECT),
                                    start_pos: press,
                                };
                                started = true;
                            }
                        }
                    }
                }
                if !started {
                    if let Some(bi_hit) = self.canvas.hit_bubble(press, origin, page) {
                        self.canvas.selected = Some((self.current_page, bi_hit));
                        self.canvas.drag = DragState::TranslateBubble {
                            page_index: self.current_page,
                            bubble_index: bi_hit,
                            start_bbox: page.bubbles[bi_hit].bbox.unwrap_or(ZERO_RECT),
                            start_pos: press,
                        };
                        self.begin_bubble_snapshot(self.current_page, bi_hit);
                    }
                }
                if started {
                    self.begin_bubble_snapshot(
                        self.current_page,
                        self.canvas.selected.map_or(0, |(_, bi)| bi),
                    );
                }
            }

            match &self.canvas.drag {
                DragState::TranslateBubble {
                    page_index,
                    bubble_index,
                    start_bbox,
                    start_pos,
                    ..
                } => {
                    let delta = pointer - *start_pos;
                    let mut bbox = *start_bbox;
                    bbox.x1 += delta.x / self.canvas.zoom;
                    bbox.y1 += delta.y / self.canvas.zoom;
                    bbox.x2 += delta.x / self.canvas.zoom;
                    bbox.y2 += delta.y / self.canvas.zoom;
                    let next = CanvasState::clamp_bbox(bbox, image_size);
                    if (next.x1 - start_bbox.x1).abs() > 0.01
                        || (next.y1 - start_bbox.y1).abs() > 0.01
                        || (next.x2 - start_bbox.x2).abs() > 0.01
                        || (next.y2 - start_bbox.y2).abs() > 0.01
                    {
                        self.canvas.drag_moved = true;
                        self.state.as_mut().unwrap().set_bubble_bbox(
                            *page_index,
                            *bubble_index,
                            next,
                        );
                    }
                    return;
                }
                DragState::ResizeBubble {
                    page_index,
                    bubble_index,
                    start_bbox,
                    start_pos,
                    handle,
                    ..
                } => {
                    let delta = pointer - *start_pos;
                    let mut bbox = *start_bbox;
                    let dx = delta.x / self.canvas.zoom;
                    let dy = delta.y / self.canvas.zoom;
                    match handle {
                        Handle::Tl => {
                            bbox.x1 += dx;
                            bbox.y1 += dy;
                        }
                        Handle::Tc => {
                            bbox.y1 += dy;
                        }
                        Handle::Tr => {
                            bbox.x2 += dx;
                            bbox.y1 += dy;
                        }
                        Handle::Ml => {
                            bbox.x1 += dx;
                        }
                        Handle::Mr => {
                            bbox.x2 += dx;
                        }
                        Handle::Bl => {
                            bbox.x1 += dx;
                            bbox.y2 += dy;
                        }
                        Handle::Bc => {
                            bbox.y2 += dy;
                        }
                        Handle::Br => {
                            bbox.x2 += dx;
                            bbox.y2 += dy;
                        }
                    }
                    let next = CanvasState::clamp_bbox(bbox, image_size);
                    if next.x2 - next.x1 > 1.0 && next.y2 - next.y1 > 1.0 {
                        self.canvas.drag_moved = true;
                        self.state.as_mut().unwrap().set_bubble_bbox(
                            *page_index,
                            *bubble_index,
                            next,
                        );
                    }
                    return;
                }
                _ => {}
            }
        }

        if response.clicked() && self.canvas.drag == DragState::None {
            if let Some(bi) = self.canvas.hit_bubble(pointer, origin, page) {
                self.canvas.selected = Some((self.current_page, bi));
            } else if !self.canvas.brush_active {
                self.canvas.selected = None;
            }
        }
    }

    fn begin_bubble_snapshot(&mut self, page_index: usize, bubble_index: usize) {
        self.canvas.drag_snapshot = self
            .state
            .as_ref()
            .and_then(|state| state.bubble_value(page_index, bubble_index));
        self.canvas.drag_page_dirty_before = self.state.as_ref().map_or(Some(false), |state| {
            Some(state.page_render_dirty(page_index))
        });
        self.canvas.drag_moved = false;
    }

    fn paint_at(
        &mut self,
        pointer: Pos2,
        origin: Pos2,
        mode: BrushMode,
        _response: &egui::Response,
    ) {
        let img = self.canvas.screen_to_image(pointer, origin);
        if let DragState::BrushStroke { points, radius, .. } = &mut self.canvas.drag {
            if points.last().map_or(true, |last| {
                (last.0 - img.x).hypot(last.1 - img.y) > *radius / self.canvas.zoom / 4.0
            }) {
                points.push((img.x, img.y));
            }
        } else {
            self.canvas.drag = DragState::BrushStroke {
                page_index: self.current_page,
                mode,
                color: color32_to_hex(self.canvas.brush_color),
                radius: self.canvas.brush_radius,
                points: vec![(img.x, img.y)],
            };
        }
    }

    fn finish_drag(&mut self) {
        match std::mem::replace(&mut self.canvas.drag, DragState::None) {
            DragState::BrushStroke {
                page_index,
                points,
                color,
                radius,
                mode,
            } => {
                if !points.is_empty() {
                    let stroke = CorrectionStroke {
                        mode: match mode {
                            BrushMode::Cover => "cover".to_owned(),
                            BrushMode::Restore => "restore".to_owned(),
                        },
                        color,
                        size: radius * 2.0,
                        points,
                    };
                    self.state
                        .as_mut()
                        .unwrap()
                        .push_stroke(page_index, &stroke);
                    self.canvas.undo_stack.push_back(page_index);
                    self.canvas.brush_overlay_dirty = true;
                    self.schedule_save();
                }
            }
            DragState::TranslateBubble { .. } | DragState::ResizeBubble { .. } => {
                if self.canvas.drag_moved {
                    self.schedule_save();
                }
                self.canvas.drag_snapshot = None;
                self.canvas.drag_page_dirty_before = None;
                self.canvas.drag_moved = false;
            }
            DragState::Pan => self.canvas.pan_start = None,
            DragState::None => {}
        }
    }

    pub(crate) fn cancel_drag(&mut self) {
        let drag = std::mem::replace(&mut self.canvas.drag, DragState::None);
        let bubble_indices = match drag {
            DragState::TranslateBubble {
                page_index,
                bubble_index,
                ..
            }
            | DragState::ResizeBubble {
                page_index,
                bubble_index,
                ..
            } => Some((page_index, bubble_index)),
            _ => None,
        };
        if let Some((page_index, bubble_index)) = bubble_indices {
            if let (Some(state), Some(snapshot)) =
                (self.state.as_mut(), self.canvas.drag_snapshot.take())
            {
                state.restore_bubble_value(page_index, bubble_index, snapshot);
                if let Some(was_dirty) = self.canvas.drag_page_dirty_before.take() {
                    state.set_page_render_dirty(page_index, was_dirty);
                }
            }
        } else {
            self.canvas.drag_snapshot = None;
            self.canvas.drag_page_dirty_before = None;
        }
        self.canvas.drag_snapshot = None;
        self.canvas.drag_page_dirty_before = None;
        self.canvas.pan_start = None;
        self.canvas.drag_moved = false;
    }

    /// Whether the currently displayed variant is the editable cleaned base
    /// (brush corrections only apply on the cleaned variant — the renderer
    /// composites strokes onto the cleaned image before typesetting).
    pub(crate) fn current_variant_is_cleaned(&self) -> bool {
        self.canvas.current_variant == Variant::Cleaned
    }
}

const ZERO_RECT: DomainRect = DomainRect {
    x1: 0.0,
    y1: 0.0,
    x2: 1.0,
    y2: 1.0,
};

fn hex_to_color32(hex: String) -> Option<egui::Color32> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() == 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        Some(egui::Color32::from_rgb(r, g, b))
    } else {
        None
    }
}

fn color32_to_hex(color: egui::Color32) -> String {
    let [r, g, b, _] = color.to_srgba_unmultiplied();
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pan_anchor_does_not_accumulate_cumulative_drag_delta() {
        let start = Vec2::new(10.0, -3.0);
        let cumulative = Vec2::new(24.0, 8.0);
        assert_eq!(
            CanvasState::pan_from_drag(start, cumulative),
            Vec2::new(34.0, 5.0)
        );
    }

    #[test]
    fn bbox_clamp_keeps_minimum_size_inside_image() {
        let rect = DomainRect {
            x1: -9.0,
            y1: 63.0,
            x2: 2.0,
            y2: 90.0,
        };
        let clamped = CanvasState::clamp_bbox(rect, Vec2::new(64.0, 64.0));
        assert!(clamped.x1 >= 0.0 && clamped.y1 >= 0.0);
        assert!(clamped.x2 <= 64.0 && clamped.y2 <= 64.0);
        assert!(clamped.x2 - clamped.x1 >= 2.0);
        assert!(clamped.y2 - clamped.y1 >= 2.0);
    }
}
