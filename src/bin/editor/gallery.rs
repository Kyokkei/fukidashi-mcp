//! Left page gallery: scrollable thumbnails with current/flag/dirty indicators.

use eframe::egui::{self, SidePanel, StrokeKind};

use super::EditorApp;

impl EditorApp {
    pub fn gallery_ui(&mut self, ctx: &egui::Context) {
        SidePanel::left("gallery")
            .resizable(true)
            .default_width(120.0)
            .show(ctx, |ui| {
                ui.heading("Pages");
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let pages = self.state.as_ref().map(|s| s.pages()).unwrap_or_default();
                    for (index, page) in pages.iter().enumerate() {
                        let thumb = self.load_thumbnail(index, page);
                        let is_current = index == self.current_page;
                        let flagged = page.has_bubble_flags();
                        let has_issues = !page.issues.is_empty();
                        let render_dirty = page.has_render_dirty();

                        let size = egui::vec2(96.0, 96.0);
                        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
                        if let Some(tex) = thumb {
                            ui.painter().image(
                                tex.id(),
                                rect,
                                egui::Rect::from_min_max(
                                    egui::Pos2::ZERO,
                                    egui::Pos2::new(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        } else {
                            ui.painter()
                                .rect_filled(rect, 2.0, egui::Color32::DARK_GRAY);
                        }
                        if is_current {
                            ui.painter().rect_stroke(
                                rect,
                                2.0_f32,
                                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(80, 200, 255)),
                                StrokeKind::Middle,
                            );
                        }
                        if flagged {
                            ui.painter()
                                .circle_filled(rect.right_top(), 6.0, egui::Color32::RED);
                        }
                        if has_issues {
                            ui.painter().circle_filled(
                                rect.right_top() + egui::vec2(-14.0, 0.0),
                                6.0,
                                egui::Color32::from_rgb(255, 160, 40),
                            );
                        }
                        if render_dirty {
                            ui.painter().rect_stroke(
                                rect,
                                2.0_f32,
                                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 210, 70)),
                                StrokeKind::Middle,
                            );
                        }
                        ui.painter().text(
                            rect.min + egui::vec2(2.0, 2.0),
                            egui::Align2::LEFT_TOP,
                            format!("{}", index + 1),
                            egui::FontId::default(),
                            egui::Color32::WHITE,
                        );
                        if response.clicked() {
                            self.cancel_drag();
                            self.current_page = index;
                            self.canvas.selected = None;
                            self.canvas.selected_issue = None;
                            self.canvas.brush_overlay_dirty = true;
                            self.canvas.fit_applied = false;
                            self.page_textures.clear();
                        }
                    }
                });
            });
    }
}
