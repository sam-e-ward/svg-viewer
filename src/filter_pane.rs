/// Filter pane — element type and path style toggles.
///
/// Split vertically into two sections:
///   Top:    element type toggles (path, rect, circle, etc.) with counts
///   Bottom: path style groups sorted by frequency, each toggleable

use egui::{Color32, Ui};

use crate::visibility::VisibilityState;

/// Render the filter pane. Returns true if any toggle changed this frame.
pub fn show_filter_pane(ui: &mut Ui, vis: &mut VisibilityState) -> bool {
    let mut changed = false;

    // Use a vertical splitter: top half = element types, bottom half = path styles.
    // We allocate roughly half the space to each, but let the path styles section
    // take whatever is left after the type toggles.

    let available = ui.available_height();
    let type_section_height = (available * 0.35).max(100.0);

    // --- Top: Element type toggles ---
    ui.allocate_ui_with_layout(
        egui::Vec2::new(ui.available_width(), type_section_height),
        egui::Layout::top_down(egui::Align::LEFT),
        |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.strong("Element Types");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("All").clicked() {
                        for (_, _, vis) in &mut vis.type_toggles {
                            *vis = true;
                        }
                        changed = true;
                    }
                    if ui.small_button("None").clicked() {
                        for (_, _, vis) in &mut vis.type_toggles {
                            *vis = false;
                        }
                        changed = true;
                    }
                });
            });
            ui.separator();

            egui::ScrollArea::vertical()
                .id_salt("type_toggles")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (etype, count, visible) in &mut vis.type_toggles {
                        ui.horizontal(|ui| {
                            if ui.checkbox(visible, "").changed() {
                                changed = true;
                            }
                            let tag = etype.label();
                            let color = tag_color(tag);
                            ui.label(
                                egui::RichText::new(format!("<{tag}>"))
                                    .monospace()
                                    .size(12.0)
                                    .color(color),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(format_count(*count))
                                            .monospace()
                                            .size(12.0)
                                            .weak(),
                                    );
                                },
                            );
                        });
                    }
                });
        },
    );

    ui.separator();

    // --- Bottom: Path style groups ---
    ui.horizontal(|ui| {
        ui.strong("Path Styles");
        ui.label(
            egui::RichText::new(format!("({})", vis.path_styles.len()))
                .weak()
                .size(12.0),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("All").clicked() {
                for group in &mut vis.path_styles {
                    group.visible = true;
                }
                changed = true;
            }
            if ui.small_button("None").clicked() {
                for group in &mut vis.path_styles {
                    group.visible = false;
                }
                changed = true;
            }
        });
    });
    ui.separator();

    egui::ScrollArea::vertical()
        .id_salt("path_styles")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.style_mut().spacing.item_spacing.y = 2.0;
            for group in &mut vis.path_styles {
                let count = group.node_ids.len();
                let desc = group.key.description();

                ui.horizontal(|ui| {
                    if ui.checkbox(&mut group.visible, "").changed() {
                        changed = true;
                    }
                    // Color swatch for fill/stroke
                    let swatch_rect = ui.allocate_space(egui::Vec2::new(14.0, 14.0));
                    draw_style_swatch(ui, swatch_rect.1, &group.key);

                    ui.label(
                        egui::RichText::new(&desc).monospace().size(11.0),
                    );
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label(
                                egui::RichText::new(format_count(count))
                                    .monospace()
                                    .size(11.0)
                                    .weak(),
                            );
                        },
                    );
                });
            }
        });

    changed
}

/// Draw a small colour swatch representing the style.
fn draw_style_swatch(ui: &Ui, rect: egui::Rect, key: &crate::visibility::PathStyleKey) {
    let painter = ui.painter();

    let fill_color = parse_swatch_color(&key.fill);
    let stroke_color = parse_swatch_color(&key.stroke);

    if let Some(fc) = fill_color {
        painter.rect_filled(rect, egui::CornerRadius::same(2), fc);
    } else {
        // No fill — draw a hollow box
        painter.rect_filled(rect, egui::CornerRadius::same(2), Color32::from_gray(40));
    }

    if let Some(sc) = stroke_color {
        painter.rect_stroke(
            rect,
            egui::CornerRadius::same(2),
            egui::Stroke::new(2.0, sc),
            egui::epaint::StrokeKind::Inside,
        );
    }
}

fn parse_swatch_color(s: &str) -> Option<Color32> {
    if s == "none" {
        return None;
    }
    if s.starts_with('#') && s.len() == 7 {
        let r = u8::from_str_radix(&s[1..3], 16).ok()?;
        let g = u8::from_str_radix(&s[3..5], 16).ok()?;
        let b = u8::from_str_radix(&s[5..7], 16).ok()?;
        return Some(Color32::from_rgb(r, g, b));
    }
    // Gradient or other — use a grey
    Some(Color32::from_gray(140))
}

fn tag_color(tag: &str) -> Color32 {
    match tag {
        "path" => Color32::from_rgb(255, 180, 80),
        "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon" => {
            Color32::from_rgb(255, 220, 100)
        }
        "text" => Color32::from_rgb(200, 150, 255),
        "image" => Color32::from_rgb(100, 220, 220),
        _ => Color32::from_gray(180),
    }
}

fn format_count(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
