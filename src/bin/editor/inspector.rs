//! Right-hand inspector: per-bubble editing and variant/font controls.
//! AI-review scaffolding (DrawIssue, issue count, Flag, Preserve, Request Fixes)
//! has been removed — this is the manual operator QA surface.

use super::EditorApp;
use eframe::egui::{self, Color32, ComboBox, SidePanel, Slider, Ui};

impl EditorApp {
    pub fn inspector_ui(&mut self, ctx: &egui::Context) {
        SidePanel::right("inspector")
            .resizable(true)
            .default_width(300.0)
            .show(ctx, |ui| {
                ui.heading("Inspector");
                ui.separator();

                // Variant picker.
                let mut variant = self.canvas.current_variant;
                ComboBox::from_label("Variant")
                    .selected_text(match variant {
                        super::canvas::Variant::Source => "Source",
                        super::canvas::Variant::Cleaned => "Cleaned",
                        super::canvas::Variant::Rendered => "Rendered",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut variant, super::canvas::Variant::Source, "Source");
                        ui.selectable_value(
                            &mut variant,
                            super::canvas::Variant::Cleaned,
                            "Cleaned",
                        );
                        ui.selectable_value(
                            &mut variant,
                            super::canvas::Variant::Rendered,
                            "Rendered",
                        );
                    });
                if variant != self.canvas.current_variant {
                    self.cancel_drag();
                    self.canvas.current_variant = variant;
                }

                ui.separator();

                // Color palette (recent brush colors).
                ui.collapsing("Color palette", |ui| {
                    ui.label("Defaults and recent colors");
                    ui.horizontal_wrapped(|ui| {
                        for (label, color) in [
                            ("White", egui::Color32::WHITE),
                            ("Black", egui::Color32::BLACK),
                        ] {
                            if ui
                                .add(
                                    egui::Button::new(label)
                                        .fill(color)
                                        .min_size(egui::vec2(48.0, 24.0)),
                                )
                                .clicked()
                            {
                                self.canvas.brush_color = color;
                            }
                        }
                        for color in self.canvas.recent_colors.clone() {
                            if ui
                                .add(
                                    egui::Button::new("Recent")
                                        .fill(color)
                                        .min_size(egui::vec2(48.0, 24.0)),
                                )
                                .clicked()
                            {
                                self.canvas.brush_color = color;
                            }
                        }
                    });
                    ui.label(format!(
                        "Current: {}",
                        super::canvas::color32_to_hex(self.canvas.brush_color)
                    ));
                });

                ui.separator();

                // Bubble inspector or prompt.
                let sel = self.canvas.selected;
                if let Some((pi, bi)) = sel {
                    self.bubble_inspector(ui, pi, bi);
                } else {
                    ui.label("Click a bubble on the canvas to edit it.");
                }

                // Error message at the very bottom.
                if let Some(err) = &self.error_message {
                    ui.separator();
                    ui.colored_label(Color32::RED, format!("Error: {err}"));
                }
            });
    }

    fn bubble_inspector(&mut self, ui: &mut Ui, page_index: usize, bubble_index: usize) {
        let page = self.current_page_view();
        let Some(page) = page else {
            return;
        };
        let Some(bubble) = page.bubbles.get(bubble_index) else {
            return;
        };

        ui.label(format!("Bubble: {}", bubble.id));
        ui.separator();

        // Translation.
        let mut translation = bubble.translation.clone();
        ui.label("Translation");
        let changed = ui.text_edit_multiline(&mut translation).changed();
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() || changed {
                if let Some(state) = self.state.as_mut() {
                    state.set_bubble_translation(page_index, bubble_index, translation.clone());
                }
                self.schedule_save();
            }
            if ui.button("⚡ Re-render Page").clicked() {
                self.rerender_current_page();
            }
            if bubble.render_dirty {
                ui.colored_label(Color32::from_rgb(255, 180, 50), "⚠ Modified");
            }
            if ui.button("Delete Bubble").clicked() {
                self.record_history_before_mutation();
                if self
                    .state
                    .as_mut()
                    .is_some_and(|state| state.delete_bubble(page_index, bubble_index))
                {
                    self.canvas.selected = None;
                    self.schedule_save();
                }
            }
        });

        ui.separator();

        // Font size.
        let mut font_opt = bubble.font_size;
        let mut font_val: f32 = font_opt.unwrap_or(18.0);
        ui.horizontal(|ui| {
            ui.label("Font size:");
            if ui
                .add(Slider::new(&mut font_val, 8.0..=72.0).step_by(0.5))
                .changed()
            {
                font_opt = Some(font_val);
            }
            if ui.button("Auto").clicked() {
                font_opt = None;
            }
        });
        if font_opt != bubble.font_size {
            if let Some(state) = self.state.as_mut() {
                state.set_bubble_font_size(page_index, bubble_index, font_opt);
            }
            self.schedule_save();
        }

        ui.separator();

        // Padding.
        let mut padding = bubble.padding.unwrap_or(4.0);
        ui.add(Slider::new(&mut padding, 0.0..=40.0).text("Padding"));
        if (padding - bubble.padding.unwrap_or(4.0)).abs() > 0.001 {
            if let Some(state) = self.state.as_mut() {
                state.set_bubble_padding(page_index, bubble_index, padding);
            }
            self.schedule_save();
        }

        ui.separator();

        // Text color.
        let current = bubble
            .text_color
            .clone()
            .unwrap_or_else(|| "auto".to_owned());
        let mut color_choice = current.clone();
        ComboBox::from_label("Text color")
            .selected_text(&color_choice)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut color_choice, "auto".to_owned(), "Auto");
                ui.selectable_value(&mut color_choice, "black".to_owned(), "Black");
                ui.selectable_value(&mut color_choice, "white".to_owned(), "White");
            });
        if color_choice != current {
            let next = if color_choice == "auto" {
                None
            } else {
                Some(color_choice.clone())
            };
            if let Some(state) = self.state.as_mut() {
                state.set_bubble_text_color(page_index, bubble_index, next);
            }
            self.schedule_save();
        }

        let _ = page_index;
        let _ = &page;
    }
}
