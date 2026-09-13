//! Central canvas: viewport pan/zoom, bubble hit-testing, 8-handle resize, and
//! the correction brush.

#![allow(
    clippy::collapsible_if,
    clippy::if_same_then_else,
    clippy::unnecessary_map_or,
    dead_code
)]

use egui::{Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use fukidashi_mcp::domain::Rect as DomainRect;
use image::RgbImage;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ActiveTool {
    #[default]
    Select,
    DrawBubble,
    AddText,
    Brush,
    Eraser,
    Eyedropper,
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
    DrawIssue {
        page_index: usize,
        start_pos: Pos2,
        current_pos: Pos2,
        image_size: Vec2,
    },
    BrushStroke {
        page_index: usize,
        mode: BrushMode,
        color: String,
        radius: f32,
        points: Vec<(f32, f32)>,
    },
    DrawNewBubble {
        page_index: usize,
        start_img: egui::Pos2,
        current_img: egui::Pos2,
        image_size: Vec2,
    },
}

pub struct CanvasState {
    pub pan: Vec2,
    pub zoom: f32,
    pub selected: Option<(usize, usize)>,
    pub selected_issue: Option<usize>,
    pub draw_issue_mode: bool,
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
    pub eyedropper_active: bool,
    pub sample_size: u32,
    pub recent_colors: Vec<egui::Color32>,
    pub undo_stack: std::collections::VecDeque<usize>,
    pub brush_overlay: Option<egui::TextureHandle>,
    pub brush_overlay_dirty: bool,
    /// False until first auto-fit; prevents re-centering every frame.
    pub fit_applied: bool,
    /// Explicit source / cleaned / rendered picker. Defaults to cleaned so
    /// the brush edits the inpaint base rather than a typeset render.
    pub current_variant: Variant,
    pub active_tool: ActiveTool,
    pub new_bubble_font_size: f32,
}

impl Default for CanvasState {
    fn default() -> Self {
        CanvasState {
            pan: Vec2::ZERO,
            zoom: 1.0,
            selected: None,
            selected_issue: None,
            draw_issue_mode: false,
            drag: DragState::None,
            drag_snapshot: None,
            drag_page_dirty_before: None,
            drag_moved: false,
            pan_start: None,
            brush_active: false,
            brush_mode: BrushMode::Cover,
            brush_radius: 24.0,
            brush_color: egui::Color32::WHITE,
            eyedropper_active: false,
            sample_size: 5,
            recent_colors: Vec::new(),
            undo_stack: std::collections::VecDeque::new(),
            brush_overlay: None,
            brush_overlay_dirty: false,
            fit_applied: false,
            current_variant: Variant::Cleaned,
            active_tool: ActiveTool::default(),
            new_bubble_font_size: 18.0,
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

    fn hit_issue(&self, pointer: Pos2, origin: Pos2, page: &PageView) -> Option<usize> {
        let image_pt = self.screen_to_image(pointer, origin);
        page.issues
            .iter()
            .enumerate()
            .rev()
            .find_map(|(idx, issue)| {
                issue
                    .bbox
                    .filter(|bbox| {
                        image_pt.x >= bbox.x1
                            && image_pt.x <= bbox.x2
                            && image_pt.y >= bbox.y1
                            && image_pt.y <= bbox.y2
                    })
                    .map(|_| idx)
            })
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

    pub(crate) fn clamp_translate_bbox(bbox: DomainRect, image_size: Vec2) -> DomainRect {
        let width = (bbox.x2 - bbox.x1).max(2.0).min(image_size.x.max(2.0));
        let height = (bbox.y2 - bbox.y1).max(2.0).min(image_size.y.max(2.0));
        let x1 = bbox.x1.clamp(0.0, (image_size.x - width).max(0.0));
        let y1 = bbox.y1.clamp(0.0, (image_size.y - height).max(0.0));
        DomainRect {
            x1,
            y1,
            x2: x1 + width,
            y2: y1 + height,
        }
    }

    pub(crate) fn clamp_resize_bbox(
        mut candidate: DomainRect,
        anchor: DomainRect,
        handle: Handle,
        image_size: Vec2,
    ) -> DomainRect {
        let max_x = image_size.x.max(2.0);
        let max_y = image_size.y.max(2.0);
        match handle {
            Handle::Tl => {
                candidate.x1 = candidate.x1.clamp(0.0, anchor.x2 - 2.0);
                candidate.y1 = candidate.y1.clamp(0.0, anchor.y2 - 2.0);
                candidate.x2 = anchor.x2;
                candidate.y2 = anchor.y2;
            }
            Handle::Tc => {
                candidate.y1 = candidate.y1.clamp(0.0, anchor.y2 - 2.0);
                candidate.x1 = anchor.x1;
                candidate.x2 = anchor.x2;
                candidate.y2 = anchor.y2;
            }
            Handle::Tr => {
                candidate.x2 = candidate.x2.clamp(anchor.x1 + 2.0, max_x);
                candidate.y1 = candidate.y1.clamp(0.0, anchor.y2 - 2.0);
                candidate.x1 = anchor.x1;
                candidate.y2 = anchor.y2;
            }
            Handle::Ml => {
                candidate.x1 = candidate.x1.clamp(0.0, anchor.x2 - 2.0);
                candidate.y1 = anchor.y1;
                candidate.x2 = anchor.x2;
                candidate.y2 = anchor.y2;
            }
            Handle::Mr => {
                candidate.x2 = candidate.x2.clamp(anchor.x1 + 2.0, max_x);
                candidate.x1 = anchor.x1;
                candidate.y1 = anchor.y1;
                candidate.y2 = anchor.y2;
            }
            Handle::Bl => {
                candidate.x1 = candidate.x1.clamp(0.0, anchor.x2 - 2.0);
                candidate.y2 = candidate.y2.clamp(anchor.y1 + 2.0, max_y);
                candidate.x2 = anchor.x2;
                candidate.y1 = anchor.y1;
            }
            Handle::Bc => {
                candidate.y2 = candidate.y2.clamp(anchor.y1 + 2.0, max_y);
                candidate.x1 = anchor.x1;
                candidate.x2 = anchor.x2;
                candidate.y1 = anchor.y1;
            }
            Handle::Br => {
                candidate.x2 = candidate.x2.clamp(anchor.x1 + 2.0, max_x);
                candidate.y2 = candidate.y2.clamp(anchor.y1 + 2.0, max_y);
                candidate.x1 = anchor.x1;
                candidate.y1 = anchor.y1;
            }
        }
        candidate
    }

    pub(crate) fn bbox_from_points(start: Pos2, end: Pos2, image_size: Vec2) -> DomainRect {
        CanvasState::clamp_bbox(
            DomainRect {
                x1: start.x.min(end.x),
                y1: start.y.min(end.y),
                x2: start.x.max(end.x),
                y2: start.y.max(end.y),
            },
            image_size,
        )
    }

    /// Convert a deliberate Draw Bubble gesture into a valid operator box.
    /// Fifteen pixels is the accidental-click discard threshold; accepted
    /// bubbles are expanded to the editor's useful minimum while remaining
    /// wholly inside the image.
    pub(crate) fn new_bubble_bbox(start: Pos2, end: Pos2, image_size: Vec2) -> Option<DomainRect> {
        if !image_size.x.is_finite()
            || !image_size.y.is_finite()
            || image_size.x <= 0.0
            || image_size.y <= 0.0
        {
            return None;
        }
        let raw_width = (end.x - start.x).abs();
        let raw_height = (end.y - start.y).abs();
        if raw_width < 15.0 || raw_height < 15.0 {
            return None;
        }
        let width = raw_width.max(30.0).min(image_size.x);
        let height = raw_height.max(20.0).min(image_size.y);
        let center = Pos2::new((start.x + end.x) * 0.5, (start.y + end.y) * 0.5);
        let x1 = (center.x - width * 0.5).clamp(0.0, (image_size.x - width).max(0.0));
        let y1 = (center.y - height * 0.5).clamp(0.0, (image_size.y - height).max(0.0));
        Some(DomainRect {
            x1,
            y1,
            x2: x1 + width,
            y2: y1 + height,
        })
    }

    pub(crate) fn click_text_bbox(center: Pos2, image_size: Vec2) -> DomainRect {
        let width = 120.0_f32.min(image_size.x.max(1.0));
        let height = 60.0_f32.min(image_size.y.max(1.0));
        let x1 = (center.x - width * 0.5).clamp(0.0, (image_size.x - width).max(0.0));
        let y1 = (center.y - height * 0.5).clamp(0.0, (image_size.y - height).max(0.0));
        DomainRect {
            x1,
            y1,
            x2: x1 + width,
            y2: y1 + height,
        }
    }

    pub(crate) fn average_color_in_neighborhood(
        image: &RgbImage,
        center: Pos2,
        sample_size: u32,
    ) -> Option<egui::Color32> {
        if image.width() == 0
            || image.height() == 0
            || !center.x.is_finite()
            || !center.y.is_finite()
        {
            return None;
        }
        let size = if sample_size >= 5 { 5 } else { 3 };
        let half = size / 2;
        let cx = center.x.floor() as i32;
        let cy = center.y.floor() as i32;
        let max_x = image.width() as i32 - 1;
        let max_y = image.height() as i32 - 1;
        let x1 = (cx - half).clamp(0, max_x);
        let x2 = (cx + half).clamp(0, max_x);
        let y1 = (cy - half).clamp(0, max_y);
        let y2 = (cy + half).clamp(0, max_y);
        if x1 > x2 || y1 > y2 {
            return None;
        }
        let mut sum = [0_u64; 3];
        let mut count = 0_u64;
        for y in y1..=y2 {
            for x in x1..=x2 {
                let pixel = image.get_pixel(x as u32, y as u32).0;
                sum[0] += u64::from(pixel[0]);
                sum[1] += u64::from(pixel[1]);
                sum[2] += u64::from(pixel[2]);
                count += 1;
            }
        }
        (count > 0).then(|| {
            egui::Color32::from_rgb(
                (sum[0] / count) as u8,
                (sum[1] / count) as u8,
                (sum[2] / count) as u8,
            )
        })
    }

    pub(crate) fn push_recent_color(colors: &mut Vec<egui::Color32>, color: egui::Color32) {
        colors.retain(|recent| *recent != color);
        colors.insert(0, color);
        colors.truncate(4);
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

        let show_brush = matches!(
            self.canvas.active_tool,
            super::canvas::ActiveTool::Brush | super::canvas::ActiveTool::Eraser
        ) && self.current_variant_is_cleaned()
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

        // Review issue overlays use image coordinates, so they remain aligned
        // through pan and zoom and can be selected for inspector editing.
        for (idx, issue) in page.issues.iter().enumerate() {
            let Some(bbox) = &issue.bbox else { continue };
            let rect = self.canvas.screen_rect(bbox, origin);
            if rect.min.x > available.max.x
                || rect.max.x < available.min.x
                || rect.min.y > available.max.y
                || rect.max.y < available.min.y
            {
                continue;
            }
            let selected = self.canvas.selected_issue == Some(idx);
            painter.rect_filled(
                rect,
                2.0,
                egui::Color32::from_rgba_unmultiplied(255, 150, 40, if selected { 65 } else { 35 }),
            );
            painter.rect_stroke(
                rect,
                2.0,
                Stroke::new(
                    if selected { 3.0_f32 } else { 2.0_f32 },
                    egui::Color32::from_rgb(255, 140, 30),
                ),
                StrokeKind::Middle,
            );
        }

        if let DragState::DrawIssue {
            start_pos,
            current_pos,
            ..
        } = &self.canvas.drag
        {
            let start = self.canvas.image_to_screen(*start_pos, origin);
            let current = self.canvas.image_to_screen(*current_pos, origin);
            let rect = Rect::from_two_pos(start, current);
            painter.rect_filled(
                rect,
                2.0,
                egui::Color32::from_rgba_unmultiplied(255, 150, 40, 45),
            );
            painter.rect_stroke(
                rect,
                2.0,
                Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 140, 30)),
                StrokeKind::Middle,
            );
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

        // Live preview for new bubble being drawn (dashed cyan ellipse).
        if let DragState::DrawNewBubble {
            start_img,
            current_img,
            ..
        } = &self.canvas.drag
        {
            let tl = self.canvas.image_to_screen(*start_img, origin);
            let br = self.canvas.image_to_screen(*current_img, origin);
            let rect = egui::Rect::from_two_pos(tl, br);
            let center = rect.center();
            let rx = rect.width() / 2.0;
            let ry = rect.height() / 2.0;
            // Draw dashed ellipse as 32 small segments.
            let segments = 32usize;
            let color = egui::Color32::from_rgba_unmultiplied(0, 200, 220, 200);
            let mut prev = center + egui::vec2(rx, 0.0);
            for i in 1..=segments {
                let angle = (i as f32) / (segments as f32) * std::f32::consts::TAU;
                let next = center + egui::vec2(rx * angle.cos(), ry * angle.sin());
                if i % 2 == 0 {
                    painter.line_segment([prev, next], egui::Stroke::new(2.0_f32, color));
                }
                prev = next;
            }
            painter.rect_stroke(
                rect,
                0.0_f32,
                egui::Stroke::new(
                    0.5_f32,
                    egui::Color32::from_rgba_unmultiplied(0, 200, 220, 60),
                ),
                egui::StrokeKind::Middle,
            );
        }

        // Live brush/eyedropper cursor ring.  The eyedropper ring shows the
        // neighborhood that will be averaged, while brush radius remains the
        // actual correction diameter.
        if matches!(
            self.canvas.active_tool,
            ActiveTool::Brush | ActiveTool::Eraser | ActiveTool::Eyedropper
        ) {
            if let Some(hover_pos) = response.hover_pos() {
                painter.circle_stroke(
                    hover_pos,
                    if matches!(self.canvas.active_tool, ActiveTool::Eyedropper) {
                        self.canvas.sample_size as f32 * self.canvas.zoom / 2.0
                    } else {
                        self.canvas.brush_radius * self.canvas.zoom
                    },
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

        // Context cursor feedback — tool-aware.
        use ActiveTool;
        match self.canvas.active_tool {
            ActiveTool::Brush | ActiveTool::Eraser | ActiveTool::Eyedropper => {
                ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
            }
            ActiveTool::DrawBubble => {
                ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
            }
            ActiveTool::AddText => {
                ctx.set_cursor_icon(egui::CursorIcon::Text);
            }
            ActiveTool::Select => {
                // existing select/bubble/handle cursor logic
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
            }
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

        // Space + primary drag and middle drag pan the viewport. Space owns
        // the primary gesture before brush, eyedropper, or bubble handling.
        let space_pan = ctx.input(|i| i.key_down(egui::Key::Space));
        let space_primary_drag = space_pan
            && (response.drag_started_by(egui::PointerButton::Primary)
                || response.dragged_by(egui::PointerButton::Primary));
        let space_primary_click = space_pan && response.clicked_by(egui::PointerButton::Primary);
        if space_primary_drag || space_primary_click {
            if !matches!(self.canvas.drag, DragState::Pan) {
                self.canvas.drag = DragState::Pan;
                self.canvas.pan_start = Some(self.canvas.pan);
            }
            if response.dragged_by(egui::PointerButton::Primary) {
                self.canvas.pan = CanvasState::pan_from_drag(
                    self.canvas.pan_start.unwrap_or(self.canvas.pan),
                    response.drag_delta(),
                );
            }
            return;
        }

        // Middle drag pans. A secondary gesture is reserved for Brush restore;
        // in Select it must never move a bubble or unexpectedly pan the page.
        let is_brush = matches!(self.canvas.active_tool, ActiveTool::Brush);
        let right_brush = is_brush
            && self.current_variant_is_cleaned()
            && (response.dragged_by(egui::PointerButton::Secondary)
                || response.drag_started_by(egui::PointerButton::Secondary));
        if response.drag_started_by(egui::PointerButton::Middle)
            || response.dragged_by(egui::PointerButton::Middle)
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

        // Scroll gestures read raw input. Only process scrolling when the cursor is
        // hovering over the canvas viewport so we don't steal wheel events from
        // the inspector or gallery panels.
        if response.hovered() {
            let (scroll_y, scroll_x, zoom_mod, alt_mod, shift_mod, hover_pos) = ctx.input(|i| {
                (
                    i.raw_scroll_delta.y,
                    i.raw_scroll_delta.x,
                    i.modifiers.ctrl || i.modifiers.command,
                    i.modifiers.alt,
                    i.modifiers.shift,
                    i.pointer.hover_pos(),
                )
            });

            // Ctrl/Meta + Scroll -> Zoom toward pointer
            if scroll_y != 0.0 && zoom_mod {
                let factor = if scroll_y > 0.0 { 1.1 } else { 1.0 / 1.1 };
                let mouse = hover_pos.unwrap_or(origin);
                let before = self.canvas.screen_to_image(mouse, origin);
                self.canvas.zoom = (self.canvas.zoom * factor).clamp(0.05, 20.0);
                let after = self.canvas.image_to_screen(before, origin);
                self.canvas.pan += mouse - after;
                ctx.request_repaint();
                return;
            }

            // Alt+Scroll or Shift+Scroll -> Horizontal pan (Photoshop / Photopea style)
            if scroll_y != 0.0 && (alt_mod || shift_mod) {
                self.canvas.pan.x += scroll_y * 1.5;
                ctx.request_repaint();
                return;
            }

            // Horizontal wheel / trackpad scroll
            if scroll_x != 0.0 {
                self.canvas.pan.x += scroll_x * 1.5;
                ctx.request_repaint();
                return;
            }

            // Plain scroll -> Vertical pan
            if scroll_y != 0.0 && !alt_mod && !zoom_mod {
                self.canvas.pan.y += scroll_y * 1.5;
                ctx.request_repaint();
                return;
            }
        }

        // Heavy save/render and approval operations run from a snapshot. Keep
        // the canvas available for pan/zoom above, but do not let a new brush,
        // bubble or transform mutation race that snapshot.
        if self.operation_active() {
            return;
        }

        // Tool-based primary input routing.
        match self.canvas.active_tool {
            ActiveTool::Eyedropper => {
                if response.clicked_by(egui::PointerButton::Primary) {
                    self.sample_at(pointer, origin, image_size, _image_path);
                }
            }
            ActiveTool::Brush => {
                let alt_sampling = ctx.input(|i| i.modifiers.alt);
                if alt_sampling {
                    if response.clicked_by(egui::PointerButton::Primary) {
                        self.sample_at(pointer, origin, image_size, _image_path);
                    }
                } else if self.current_variant_is_cleaned()
                    && (response.dragged_by(egui::PointerButton::Primary)
                        || response.clicked_by(egui::PointerButton::Primary))
                {
                    let click_only = response.clicked_by(egui::PointerButton::Primary)
                        && !response.dragged_by(egui::PointerButton::Primary);
                    self.paint_at(pointer, origin, BrushMode::Cover, response);
                    if click_only {
                        self.finish_drag();
                    }
                } else if right_brush {
                    self.paint_at(pointer, origin, BrushMode::Restore, response);
                }
            }
            ActiveTool::Eraser => {
                if self.current_variant_is_cleaned()
                    && (response.dragged_by(egui::PointerButton::Primary)
                        || response.clicked_by(egui::PointerButton::Primary))
                {
                    let click_only = response.clicked_by(egui::PointerButton::Primary)
                        && !response.dragged_by(egui::PointerButton::Primary);
                    self.paint_at(pointer, origin, BrushMode::Restore, response);
                    if click_only {
                        self.finish_drag();
                    }
                }
            }
            ActiveTool::DrawBubble => {
                let image_point = self.canvas.screen_to_image(pointer, origin);
                if response.drag_started_by(egui::PointerButton::Primary) {
                    let press = ctx.input(|i| i.pointer.press_origin()).unwrap_or(pointer);
                    let start_img = self.canvas.screen_to_image(press, origin);
                    self.canvas.drag = DragState::DrawNewBubble {
                        page_index: self.current_page,
                        start_img,
                        current_img: image_point,
                        image_size,
                    };
                } else if let DragState::DrawNewBubble { current_img, .. } = &mut self.canvas.drag {
                    *current_img = image_point;
                }
            }
            ActiveTool::AddText => {
                if response.clicked_by(egui::PointerButton::Primary)
                    && self.canvas.drag == DragState::None
                {
                    if let Some(bi) = self.canvas.hit_bubble(pointer, origin, page) {
                        self.canvas.selected = Some((self.current_page, bi));
                        self.canvas.selected_issue = None;
                        // TODO: request_focus on translation TextEdit — deferred to inspector integration
                    } else {
                        // Create bubble at click position with default size.
                        let img_pt = self.canvas.screen_to_image(pointer, origin);
                        let bbox = CanvasState::click_text_bbox(img_pt, image_size);
                        self.record_history_before_mutation();
                        if let Some(state) = self.state.as_mut() {
                            let new_idx = state.push_new_bubble(self.current_page, bbox);
                            self.canvas.selected = Some((self.current_page, new_idx));
                            self.canvas.selected_issue = None;
                            self.schedule_save();
                        }
                        self.set_active_tool(ActiveTool::Select);
                    }
                }
            }
            ActiveTool::Select => {
                // Selection / translate / resize while dragging a (possibly new) bubble.
                if response.drag_started_by(egui::PointerButton::Primary)
                    || response.dragged_by(egui::PointerButton::Primary)
                {
                    if self.canvas.drag == DragState::None {
                        let press = ctx.input(|i| i.pointer.press_origin()).unwrap_or(pointer);
                        let mut started = false;
                        if let Some((pi, bi)) = self.canvas.selected {
                            if pi == self.current_page {
                                if let Some(bubble) = page.bubbles.get(bi) {
                                    let rect = self.canvas.screen_rect(
                                        bubble.bbox.as_ref().unwrap_or(&ZERO_RECT),
                                        origin,
                                    );
                                    if let Some(handle) = self.canvas.hit_handle(press, rect) {
                                        self.canvas.drag = DragState::ResizeBubble {
                                            page_index: pi,
                                            bubble_index: bi,
                                            start_bbox: bubble.bbox.unwrap_or(ZERO_RECT),
                                            start_pos: press,
                                            handle,
                                        };
                                        started = true;
                                    } else if self.canvas.hit_bubble(press, origin, page)
                                        == Some(bi)
                                    {
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
                    match self.canvas.drag.clone() {
                        DragState::TranslateBubble {
                            page_index,
                            bubble_index,
                            start_bbox,
                            start_pos,
                            ..
                        } => {
                            let delta = pointer - start_pos;
                            let mut bbox = start_bbox;
                            bbox.x1 += delta.x / self.canvas.zoom;
                            bbox.y1 += delta.y / self.canvas.zoom;
                            bbox.x2 += delta.x / self.canvas.zoom;
                            bbox.y2 += delta.y / self.canvas.zoom;
                            let next = CanvasState::clamp_translate_bbox(bbox, image_size);
                            if (next.x1 - start_bbox.x1).abs() > 0.01
                                || (next.y1 - start_bbox.y1).abs() > 0.01
                            {
                                if !self.canvas.drag_moved {
                                    self.begin_pending_history();
                                }
                                self.canvas.drag_moved = true;
                                self.state.as_mut().unwrap().set_bubble_bbox(
                                    page_index,
                                    bubble_index,
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
                            let delta = pointer - start_pos;
                            let mut bbox = start_bbox;
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
                            let next = CanvasState::clamp_resize_bbox(
                                bbox, start_bbox, handle, image_size,
                            );
                            if next.x2 - next.x1 > 1.0 && next.y2 - next.y1 > 1.0 {
                                if !self.canvas.drag_moved {
                                    self.begin_pending_history();
                                }
                                self.canvas.drag_moved = true;
                                self.state.as_mut().unwrap().set_bubble_bbox(
                                    page_index,
                                    bubble_index,
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
                        self.canvas.selected_issue = None;
                    } else {
                        self.canvas.selected = None;
                        self.canvas.selected_issue = None;
                    }
                }
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
            self.begin_pending_history();
            self.canvas.drag = DragState::BrushStroke {
                page_index: self.current_page,
                mode,
                color: color32_to_hex(self.canvas.brush_color),
                radius: self.canvas.brush_radius,
                points: vec![(img.x, img.y)],
            };
        }
    }

    fn sample_at(
        &mut self,
        pointer: Pos2,
        origin: Pos2,
        image_size: Vec2,
        image_path: &std::path::Path,
    ) {
        let image_point = self.canvas.screen_to_image(pointer, origin);
        if image_point.x < 0.0
            || image_point.y < 0.0
            || image_point.x >= image_size.x
            || image_point.y >= image_size.y
        {
            return;
        }
        let image = match image::open(image_path) {
            Ok(image) => image.to_rgb8(),
            Err(error) => {
                self.error_message = Some(format!("eyedropper could not read image: {error}"));
                return;
            }
        };
        let Some(color) = CanvasState::average_color_in_neighborhood(
            &image,
            image_point,
            self.canvas.sample_size,
        ) else {
            self.error_message = Some("eyedropper found no pixels at that location".to_owned());
            return;
        };
        self.canvas.brush_color = color;
        CanvasState::push_recent_color(&mut self.canvas.recent_colors, color);
        self.error_message = None;
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
                    self.commit_pending_history();
                    self.schedule_save();
                }
            }
            DragState::DrawIssue {
                page_index,
                start_pos,
                current_pos,
                image_size,
            } => {
                let bbox = CanvasState::bbox_from_points(start_pos, current_pos, image_size);
                if bbox.x2 - bbox.x1 >= 3.0 && bbox.y2 - bbox.y1 >= 3.0 {
                    let issue_index = self
                        .state
                        .as_mut()
                        .map(|state| state.push_issue(page_index, bbox));
                    if let Some(issue_index) = issue_index {
                        self.canvas.selected_issue = Some(issue_index);
                        self.canvas.selected = None;
                        self.schedule_save();
                    }
                }
            }
            DragState::TranslateBubble { .. } | DragState::ResizeBubble { .. } => {
                if self.canvas.drag_moved {
                    self.commit_pending_history();
                    self.schedule_save();
                }
                self.canvas.drag_snapshot = None;
                self.canvas.drag_page_dirty_before = None;
                self.canvas.drag_moved = false;
            }
            DragState::DrawNewBubble {
                page_index,
                start_img,
                current_img,
                image_size,
            } => {
                if let Some(bbox) = CanvasState::new_bubble_bbox(start_img, current_img, image_size)
                {
                    self.record_history_before_mutation();
                    let idx = self
                        .state
                        .as_mut()
                        .map(|s| s.push_new_bubble(page_index, bbox))
                        .unwrap_or(0);
                    self.canvas.selected = Some((page_index, idx));
                    self.canvas.selected_issue = None;
                    self.schedule_save();
                    self.set_active_tool(ActiveTool::Select);
                }
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
        self.pending_history = None;
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

pub(crate) fn color32_to_hex(color: egui::Color32) -> String {
    let [r, g, b, _] = color.to_srgba_unmultiplied();
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

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

    #[test]
    fn coordinate_conversion_and_resize_clamp_preserve_expected_edges() {
        let canvas = CanvasState {
            pan: Vec2::new(11.0, -7.0),
            zoom: 2.0,
            ..CanvasState::default()
        };
        let origin = Pos2::new(100.0, 80.0);
        let image_point = Pos2::new(13.0, 9.0);
        let screen = canvas.image_to_screen(image_point, origin);
        assert_eq!(canvas.screen_to_image(screen, origin), image_point);

        let anchor = DomainRect {
            x1: 10.0,
            y1: 10.0,
            x2: 30.0,
            y2: 30.0,
        };
        let resized = CanvasState::clamp_resize_bbox(
            DomainRect {
                x1: -20.0,
                y1: 10.0,
                x2: 30.0,
                y2: 30.0,
            },
            anchor,
            Handle::Ml,
            Vec2::new(40.0, 40.0),
        );
        assert_eq!(resized.x1, 0.0);
        assert_eq!(resized.x2, anchor.x2);
        let translated = CanvasState::clamp_translate_bbox(
            DomainRect {
                x1: 35.0,
                y1: 35.0,
                x2: 55.0,
                y2: 55.0,
            },
            Vec2::new(40.0, 40.0),
        );
        assert_eq!(translated.x2 - translated.x1, 20.0);
        assert_eq!(translated.y2 - translated.y1, 20.0);
        assert_eq!(translated.x2, 40.0);
        assert_eq!(translated.y2, 40.0);
    }

    #[test]
    fn eyedropper_averages_bounded_3x3_and_5x5_neighborhoods() {
        let image = RgbImage::from_pixel(3, 3, Rgb([10, 20, 30]));
        let color =
            CanvasState::average_color_in_neighborhood(&image, Pos2::new(0.0, 0.0), 5).unwrap();
        assert_eq!(color, egui::Color32::from_rgb(10, 20, 30));
        let mut image = RgbImage::from_pixel(7, 7, Rgb([0, 0, 0]));
        image.put_pixel(3, 3, Rgb([255, 255, 255]));
        let three =
            CanvasState::average_color_in_neighborhood(&image, Pos2::new(3.0, 3.0), 3).unwrap();
        let five =
            CanvasState::average_color_in_neighborhood(&image, Pos2::new(3.0, 3.0), 5).unwrap();
        assert!(three.r() > five.r());
    }

    #[test]
    fn recent_colors_are_deduplicated_most_recent_first_and_bounded() {
        let mut recent = Vec::new();
        for channel in 0..=4 {
            CanvasState::push_recent_color(
                &mut recent,
                egui::Color32::from_rgb(channel * 20, 0, 0),
            );
        }
        CanvasState::push_recent_color(&mut recent, egui::Color32::from_rgb(40, 0, 0));
        assert_eq!(recent.len(), 4);
        assert_eq!(recent[0], egui::Color32::from_rgb(40, 0, 0));
        assert!(!recent.contains(&egui::Color32::from_rgb(0, 0, 0)));
    }

    #[test]
    fn new_bubble_requires_intent_and_expands_to_useful_minimum() {
        assert!(
            CanvasState::new_bubble_bbox(
                Pos2::new(10.0, 10.0),
                Pos2::new(20.0, 40.0),
                Vec2::new(200.0, 100.0),
            )
            .is_none()
        );
        let bbox = CanvasState::new_bubble_bbox(
            Pos2::new(180.0, 80.0),
            Pos2::new(199.0, 99.0),
            Vec2::new(200.0, 100.0),
        )
        .unwrap();
        assert!(bbox.x2 - bbox.x1 >= 30.0);
        assert!(bbox.y2 - bbox.y1 >= 20.0);
        assert!(bbox.x1 >= 0.0 && bbox.y1 >= 0.0 && bbox.x2 <= 200.0 && bbox.y2 <= 100.0);
    }

    #[test]
    fn click_text_bbox_is_in_image_bounds() {
        let bbox = CanvasState::click_text_bbox(Pos2::new(2.0, 3.0), Vec2::new(40.0, 30.0));
        assert!(bbox.x1 >= 0.0 && bbox.y1 >= 0.0);
        assert!(bbox.x2 <= 40.0 && bbox.y2 <= 30.0);
        assert!(bbox.x2 > bbox.x1 && bbox.y2 > bbox.y1);
    }

    #[test]
    fn canvas_zoom_clamp_bounds_support_deep_kanji_inspection() {
        let mut zoom = 1.0_f32;
        zoom = (zoom * 0.001).clamp(0.05, 20.0);
        assert_eq!(zoom, 0.05);
        zoom = (zoom * 1000.0).clamp(0.05, 20.0);
        assert_eq!(zoom, 20.0);
    }
}
