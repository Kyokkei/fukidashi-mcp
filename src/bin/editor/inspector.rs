//! Right-hand inspector: per-bubble editing, brush controls, and review actions.

use eframe::egui::{self, Color32, ComboBox, DragValue, SidePanel, Slider, Ui};

use super::EditorApp;

impl EditorApp {
    pub fn inspector_ui(&mut self, ctx: &egui::Context) {
        SidePanel::right("inspector")
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.heading("Inspector");
                ui.separator();

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
                let mut draw_issue = self.canvas.draw_issue_mode;
                if ui
                    .checkbox(&mut draw_issue, "Draw Issue (primary drag)")
                    .changed()
                {
                    self.cancel_drag();
                    self.canvas.draw_issue_mode = draw_issue;
                    if draw_issue {
                        self.canvas.brush_active = false;
                        self.canvas.eyedropper_active = false;
                        self.canvas.selected = None;
                        self.canvas.selected_issue = None;
                    }
                }
                let issue_count = self
                    .current_page_view()
                    .map(|page| page.issues.len())
                    .unwrap_or(0);
                ui.horizontal(|ui| {
                    ui.label(format!("Issues on page: {issue_count}"));
                    if ui.button("Clear page issues").clicked() && issue_count > 0 {
                        if let Some(state) = self.state.as_mut() {
                            state.clear_issues(self.current_page);
                        }
                        self.canvas.selected_issue = None;
                        self.schedule_save();
                    }
                });

                ui.separator();
                ui.collapsing("Color palette", |ui| {
                    let eye_label = if self.canvas.eyedropper_active {
                        "Eyedropper active (I)"
                    } else {
                        "Eyedropper (I)"
                    };
                    if ui.button(eye_label).clicked() {
                        self.cancel_drag();
                        self.canvas.eyedropper_active = !self.canvas.eyedropper_active;
                        if self.canvas.eyedropper_active {
                            self.canvas.brush_active = false;
                        }
                    }
                    ComboBox::from_label("Sample neighborhood")
                        .selected_text(format!(
                            "{}×{}",
                            self.canvas.sample_size, self.canvas.sample_size
                        ))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.canvas.sample_size, 3, "3×3");
                            ui.selectable_value(&mut self.canvas.sample_size, 5, "5×5");
                        });
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
                ui.label("Font Path");
                let mut font = self
                    .state
                    .as_ref()
                    .and_then(|s| s.font_path())
                    .unwrap_or_default();
                let original_font = font.clone();
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut font).desired_width(180.0));
                    if ui.button("Browse").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("Font", &["ttf", "otf"])
                            .pick_file()
                        {
                            font = path.to_string_lossy().into_owned();
                        }
                    }
                });
                if font != original_font {
                    if let Some(state) = self.state.as_mut() {
                        state.set_font_path(if font.trim().is_empty() {
                            None
                        } else {
                            Some(font)
                        });
                    }
                    self.schedule_save();
                }

                ui.separator();

                // Brush controls (always available; only effective on cleaned variant).
                ui.collapsing("Brush", |ui| {
                    let mut active = self.canvas.brush_active;
                    if ui.checkbox(&mut active, "Brush tool (B)").changed() {
                        self.cancel_drag();
                        self.canvas.brush_active = active;
                        if active {
                            self.canvas.eyedropper_active = false;
                            self.canvas.current_variant = super::canvas::Variant::Cleaned;
                        }
                    }
                    let mut mode = match self.canvas.brush_mode {
                        super::canvas::BrushMode::Cover => "Cover",
                        super::canvas::BrushMode::Restore => "Restore",
                    };
                    ComboBox::from_label("Mode")
                        .selected_text(mode)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut mode, "Cover", "Cover (white-out)");
                            ui.selectable_value(&mut mode, "Restore", "Restore (source)");
                        });
                    self.canvas.brush_mode = match mode {
                        "Restore" => super::canvas::BrushMode::Restore,
                        _ => super::canvas::BrushMode::Cover,
                    };
                    ui.horizontal(|ui| {
                        ui.add(
                            Slider::new(&mut self.canvas.brush_radius, 4.0..=80.0).text("Radius"),
                        );
                        ui.add(
                            DragValue::new(&mut self.canvas.brush_radius)
                                .range(4.0..=80.0)
                                .suffix(" px"),
                        );
                    });
                    if self.canvas.brush_mode == super::canvas::BrushMode::Cover {
                        let s = self.canvas.brush_color.to_srgba_unmultiplied();
                        let mut rgb = [s[0], s[1], s[2]];
                        if ui.color_edit_button_srgb(&mut rgb).changed() {
                            self.canvas.brush_color = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                        }
                    }
                    if ui.button("Undo last stroke (Ctrl+Z)").clicked() {
                        self.undo_stroke();
                    }
                    let hint = if self.current_variant_is_cleaned() {
                        "Active on cleaned variant."
                    } else {
                        "Disabled: switch to the cleaned variant to paint."
                    };
                    ui.label(hint);
                });

                ui.separator();

                // Bubble inspector.
                let sel = self.canvas.selected;
                if let Some(issue_index) = self.canvas.selected_issue {
                    self.issue_inspector(ui, self.current_page, issue_index);
                } else if let Some((pi, bi)) = sel {
                    self.bubble_inspector(ui, pi, bi);
                } else {
                    ui.label("Select a bubble or issue on the canvas to edit it.");
                }

                ui.separator();

                // Review actions.
                ui.heading("Review");
                if ui.button("Re-render page").clicked() {
                    self.rerender_current_page();
                }
                if ui.button("Approve & Export All").clicked() {
                    self.approve_and_export();
                }
                if ui.button("Request Fixes").clicked() {
                    self.request_fixes();
                }
                if let Some(err) = &self.error_message {
                    ui.separator();
                    ui.colored_label(Color32::RED, format!("Error: {err}"));
                }
            });
    }

    fn issue_inspector(&mut self, ui: &mut Ui, page_index: usize, issue_index: usize) {
        let issue = self
            .current_page_view()
            .and_then(|page| page.issues.get(issue_index).cloned());
        let Some(issue) = issue else {
            self.canvas.selected_issue = None;
            return;
        };
        ui.label(format!("Issue {}", issue_index + 1));
        ui.label(format!(
            "{} issue(s) on this page",
            self.current_page_view()
                .map(|p| p.issues.len())
                .unwrap_or(0)
        ));

        let choices = [
            ("leftover_source_text", "Leftover source text"),
            ("text_overflow", "Text overflow"),
            ("wrong_translation", "Wrong translation"),
            ("wrong_or_missing_bubble", "Wrong/missing bubble"),
            ("damaged_artwork", "Damaged artwork"),
            ("font_or_layout", "Font/layout"),
            ("flagged_bubble", "Flagged bubble"),
            ("custom", "Custom"),
        ];
        let mut issue_type = issue.issue_type.clone();
        ComboBox::from_label("Issue type")
            .selected_text(
                choices
                    .iter()
                    .find(|(value, _)| *value == issue_type)
                    .map(|(_, label)| *label)
                    .unwrap_or("Custom"),
            )
            .show_ui(ui, |ui| {
                for (value, label) in choices {
                    ui.selectable_value(&mut issue_type, value.to_owned(), label);
                }
            });
        if issue_type != issue.issue_type {
            if let Some(state) = self.state.as_mut() {
                state.set_issue_type(page_index, issue_index, issue_type);
            }
            self.schedule_save();
        }

        let mut note = issue.note.clone();
        ui.label("Note");
        if ui.text_edit_multiline(&mut note).changed() {
            if let Some(state) = self.state.as_mut() {
                state.set_issue_text(page_index, issue_index, "note", Some(note));
            }
            self.schedule_save();
        }

        let mut corrected_text = issue.corrected_text.clone().unwrap_or_default();
        ui.label("Corrected text (optional)");
        if ui.text_edit_multiline(&mut corrected_text).changed() {
            if let Some(state) = self.state.as_mut() {
                state.set_issue_text(
                    page_index,
                    issue_index,
                    "corrected_text",
                    (!corrected_text.trim().is_empty()).then_some(corrected_text),
                );
            }
            self.schedule_save();
        }

        if ui.button("Delete selected issue").clicked() {
            if let Some(state) = self.state.as_mut() {
                state.remove_issue(page_index, issue_index);
            }
            self.canvas.selected_issue = None;
            self.schedule_save();
        }
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

        // Translation (mutates underlying Value).
        let mut translation = bubble.translation.clone();
        ui.label("Translation");
        let translation_changed = ui.text_edit_multiline(&mut translation).changed();
        if translation_changed || ui.button("Apply translation").clicked() {
            self.state.as_mut().unwrap().set_bubble_translation(
                page_index,
                bubble_index,
                translation.clone(),
            );
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
            self.state
                .as_mut()
                .unwrap()
                .set_bubble_text_color(page_index, bubble_index, next);
            self.schedule_save();
        }

        ui.separator();
        // Font size.
        let mut font_opt = bubble.font_size;
        ui.label("Font size (none = auto)");
        let mut font_val: f32 = font_opt.unwrap_or(18.0);
        ui.horizontal(|ui| {
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
            self.state
                .as_mut()
                .unwrap()
                .set_bubble_font_size(page_index, bubble_index, font_opt);
            self.schedule_save();
        }

        ui.horizontal(|ui| {
            if ui.button("⚡ Re-render Page").clicked() {
                self.rerender_current_page();
            }
            if bubble.render_dirty {
                ui.colored_label(Color32::from_rgb(255, 180, 50), "⚠️ Modified");
            }
        });

        ui.separator();
        // Padding.
        let mut padding = bubble.padding.unwrap_or(4.0);
        ui.add(Slider::new(&mut padding, 0.0..=40.0).text("Padding"));
        if (padding - bubble.padding.unwrap_or(4.0)).abs() > 0.001 {
            self.state
                .as_mut()
                .unwrap()
                .set_bubble_padding(page_index, bubble_index, padding);
            self.schedule_save();
        }

        ui.separator();
        // Flags.
        let mut flagged = bubble.flagged;
        ui.checkbox(&mut flagged, "Flag for review");
        if flagged != bubble.flagged {
            self.state
                .as_mut()
                .unwrap()
                .set_bubble_flagged(page_index, bubble_index, flagged);
            self.schedule_save();
        }
        let mut preserve = bubble.preserve_source;
        ui.checkbox(&mut preserve, "Preserve original pixels");
        if preserve != bubble.preserve_source {
            self.state.as_mut().unwrap().set_bubble_preserve_source(
                page_index,
                bubble_index,
                preserve,
            );
            self.schedule_save();
        }

        let _ = page_index;
        let _ = &page;
    }
}
