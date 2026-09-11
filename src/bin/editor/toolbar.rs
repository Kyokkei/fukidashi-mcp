//! Left tool strip (icon buttons) and contextual top options bar.

use super::EditorApp;
use eframe::egui::{self, Color32, ColorImage, SidePanel, TextureHandle, TextureOptions, Vec2};

struct IconEntry {
    id: &'static str,
    bytes: &'static [u8],
}

macro_rules! icon {
    ($id:expr, $path:expr) => {
        IconEntry {
            id: $id,
            bytes: include_bytes!($path),
        }
    };
}

// paths relative to src/bin/editor/
static ICONS: &[IconEntry] = &[
    icon!("move", "../../../assets/editor-icons/move.png"),
    icon!("ellipse", "../../../assets/editor-icons/ellipse.png"),
    icon!("htype", "../../../assets/editor-icons/htype.png"),
    icon!("brush", "../../../assets/editor-icons/brush.png"),
    icon!("eraser", "../../../assets/editor-icons/eraser.png"),
    icon!("eyedropper", "../../../assets/editor-icons/eyedropper.png"),
];

pub fn load_icons(ctx: &egui::Context) -> std::collections::HashMap<&'static str, TextureHandle> {
    let mut map = std::collections::HashMap::new();
    for entry in ICONS {
        let tex = load_png_texture(ctx, entry.id, entry.bytes);
        map.insert(entry.id, tex);
    }
    map
}

fn load_png_texture(ctx: &egui::Context, name: &str, bytes: &[u8]) -> TextureHandle {
    match image::load_from_memory(bytes).map(|img| img.to_rgba8()) {
        Ok(mut rgba) => {
            // Source icons are monochrome masks; tint them at draw time.
            for pixel in rgba.pixels_mut() {
                if pixel[3] != 0 {
                    pixel[0] = 255;
                    pixel[1] = 255;
                    pixel[2] = 255;
                }
            }
            let size = [rgba.width() as usize, rgba.height() as usize];
            let color_image = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            ctx.load_texture(name, color_image, TextureOptions::LINEAR)
        }
        Err(_) => {
            let color_image = ColorImage::from_rgba_unmultiplied([1, 1], &[120, 120, 120, 255]);
            ctx.load_texture(name, color_image, TextureOptions::LINEAR)
        }
    }
}

struct ToolDef {
    tool: super::canvas::ActiveTool,
    icon: &'static str,
    tooltip: &'static str,
    hotkey: &'static str,
}

static TOOLS: &[ToolDef] = &[
    ToolDef {
        tool: super::canvas::ActiveTool::Select,
        icon: "move",
        tooltip: "Select / Move",
        hotkey: "V",
    },
    ToolDef {
        tool: super::canvas::ActiveTool::DrawBubble,
        icon: "ellipse",
        tooltip: "Draw Bubble",
        hotkey: "O",
    },
    ToolDef {
        tool: super::canvas::ActiveTool::AddText,
        icon: "htype",
        tooltip: "Add Text",
        hotkey: "T",
    },
    ToolDef {
        tool: super::canvas::ActiveTool::Brush,
        icon: "brush",
        tooltip: "Brush (Cover)",
        hotkey: "B",
    },
    ToolDef {
        tool: super::canvas::ActiveTool::Eraser,
        icon: "eraser",
        tooltip: "Eraser (Restore)",
        hotkey: "E",
    },
    ToolDef {
        tool: super::canvas::ActiveTool::Eyedropper,
        icon: "eyedropper",
        tooltip: "Eyedropper",
        hotkey: "I",
    },
];

const ICON_SIZE: f32 = 28.0;
const STRIP_WIDTH: f32 = 44.0;
const ACTIVE_COLOR: Color32 = Color32::from_rgb(0, 229, 255);

impl EditorApp {
    pub fn toolbar_ui(&mut self, ctx: &egui::Context) {
        SidePanel::left("tool_strip")
            .resizable(false)
            .exact_width(STRIP_WIDTH)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(6.0);
                    for def in TOOLS {
                        let active = self.canvas.active_tool == def.tool;
                        let icon_tex = self.icon_textures.get(def.icon).cloned();
                        let (rect, response) = ui.allocate_exact_size(
                            Vec2::splat(ICON_SIZE + 8.0),
                            egui::Sense::click(),
                        );
                        if active {
                            ui.painter().rect_filled(
                                rect,
                                4.0,
                                Color32::from_rgba_unmultiplied(0, 229, 255, 45),
                            );
                            ui.painter().rect_stroke(
                                rect,
                                4.0,
                                egui::Stroke::new(1.5_f32, ACTIVE_COLOR),
                                egui::StrokeKind::Middle,
                            );
                        }
                        if let Some(tex) = icon_tex {
                            let icon_rect =
                                egui::Rect::from_center_size(rect.center(), Vec2::splat(ICON_SIZE));
                            ui.painter().image(
                                tex.id(),
                                icon_rect,
                                egui::Rect::from_min_max(
                                    egui::Pos2::ZERO,
                                    egui::Pos2::new(1.0, 1.0),
                                ),
                                if active {
                                    ACTIVE_COLOR
                                } else {
                                    Color32::from_gray(190)
                                },
                            );
                        } else {
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                def.hotkey,
                                egui::FontId::monospace(12.0),
                                if active {
                                    Color32::WHITE
                                } else {
                                    Color32::from_gray(160)
                                },
                            );
                        }
                        let resp = response.on_hover_ui(|ui| {
                            ui.label(format!("{} ({})", def.tooltip, def.hotkey));
                        });
                        if resp.clicked() {
                            self.set_active_tool(def.tool);
                        }
                        ui.add_space(2.0);
                    }
                });
            });
    }

    pub fn tool_options_ui(&mut self, ui: &mut egui::Ui) {
        use super::canvas::{ActiveTool, BrushMode};
        match self.canvas.active_tool {
            ActiveTool::DrawBubble => {
                ui.label("Shape: Ellipse");
            }
            ActiveTool::AddText => {
                ui.label("Default font size:");
                ui.add(
                    egui::DragValue::new(&mut self.canvas.new_bubble_font_size)
                        .range(8.0..=72.0)
                        .suffix(" px"),
                );
            }
            ActiveTool::Brush => {
                let mut mode_str = match self.canvas.brush_mode {
                    BrushMode::Cover => "Cover",
                    BrushMode::Restore => "Restore",
                };
                egui::ComboBox::from_id_salt("brush_mode_top")
                    .selected_text(mode_str)
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut mode_str, "Cover", "Cover (white-out)");
                        ui.selectable_value(&mut mode_str, "Restore", "Restore (source)");
                    });
                self.canvas.brush_mode = match mode_str {
                    "Restore" => BrushMode::Restore,
                    _ => BrushMode::Cover,
                };
                ui.label("Radius:");
                ui.add(egui::Slider::new(&mut self.canvas.brush_radius, 4.0..=80.0).text("r"));
                ui.add(
                    egui::DragValue::new(&mut self.canvas.brush_radius)
                        .range(4.0..=80.0)
                        .suffix(" px"),
                );
                if self.canvas.brush_mode == BrushMode::Cover {
                    let s = self.canvas.brush_color.to_srgba_unmultiplied();
                    let mut rgb = [s[0], s[1], s[2]];
                    if ui.color_edit_button_srgb(&mut rgb).changed() {
                        self.canvas.brush_color = egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                    }
                }
                ui.separator();
                if ui.small_button("Undo (Ctrl+Z)").clicked() {
                    self.undo_operator();
                }
                if !self.current_variant_is_cleaned() {
                    ui.colored_label(
                        Color32::from_rgb(255, 120, 40),
                        "⚠ Switch to Cleaned variant",
                    );
                }
            }
            ActiveTool::Eraser => {
                ui.label("Radius:");
                ui.add(egui::Slider::new(&mut self.canvas.brush_radius, 4.0..=80.0).text("r"));
                ui.add(
                    egui::DragValue::new(&mut self.canvas.brush_radius)
                        .range(4.0..=80.0)
                        .suffix(" px"),
                );
            }
            ActiveTool::Eyedropper => {
                ui.label("Sample:");
                egui::ComboBox::from_id_salt("eye_sample_top")
                    .selected_text(format!(
                        "{}×{}",
                        self.canvas.sample_size, self.canvas.sample_size
                    ))
                    .width(70.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.canvas.sample_size, 1, "1×1");
                        ui.selectable_value(&mut self.canvas.sample_size, 3, "3×3");
                        ui.selectable_value(&mut self.canvas.sample_size, 5, "5×5");
                        ui.selectable_value(&mut self.canvas.sample_size, 9, "9×9");
                    });
            }
            ActiveTool::Select => {}
        }
    }

    pub fn set_active_tool(&mut self, tool: super::canvas::ActiveTool) {
        if self.canvas.active_tool == tool {
            return;
        }
        self.cancel_drag();
        if matches!(
            tool,
            super::canvas::ActiveTool::Brush | super::canvas::ActiveTool::Eraser
        ) {
            self.canvas.current_variant = super::canvas::Variant::Cleaned;
        }
        self.canvas.active_tool = tool;
    }
}
