/// Elements pane — renders the SVG document tree as a collapsible egui widget.

use egui::{Color32, Ui};
use std::collections::HashSet;

use crate::svg_doc::{NodeId, SvgDocument, SvgNodeKind};

pub struct ElementsPane {
    /// Set of node ids that are currently collapsed (children hidden)
    pub collapsed: HashSet<NodeId>,
    /// The currently selected/highlighted node
    pub selected: Option<NodeId>,
    /// Node to scroll into view on the next frame
    pub scroll_to: Option<NodeId>,
}

impl ElementsPane {
    pub fn new() -> Self {
        ElementsPane {
            collapsed: HashSet::new(),
            selected: None,
            scroll_to: None,
        }
    }

    /// Main render function — call inside a panel or frame.
    /// Returns the NodeId if the user clicked a row.
    pub fn show(&mut self, ui: &mut Ui, doc: &SvgDocument) -> Option<NodeId> {
        let mut clicked = None;

        egui::ScrollArea::vertical()
            .id_salt("elements_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.style_mut().spacing.item_spacing.y = 1.0;
                self.show_node(ui, doc, doc.root, 0, &mut clicked);
            });

        clicked
    }

    fn show_node(
        &mut self,
        ui: &mut Ui,
        doc: &SvgDocument,
        node_id: NodeId,
        depth: usize,
        clicked: &mut Option<NodeId>,
    ) {
        let node = doc.get(node_id);
        let is_leaf = node.children.is_empty();
        let is_collapsed = self.collapsed.contains(&node_id);
        let is_selected = self.selected == Some(node_id);
        let is_container = matches!(
            node.kind,
            SvgNodeKind::Svg { .. }
                | SvgNodeKind::Group
                | SvgNodeKind::Defs
                | SvgNodeKind::ClipPath { .. }
                | SvgNodeKind::Mask { .. }
        );

        let indent = depth as f32 * 14.0;

        // Build label
        let tag_color = tag_color(&node.tag_name);
        let label = build_label(&node.tag_name, &node.attr_summary);

        // Row
        let row_height = 20.0;
        let (rect, response) = ui.allocate_exact_size(
            egui::Vec2::new(ui.available_width(), row_height),
            egui::Sense::click(),
        );

        // Scroll to selected
        let needs_scroll = self.scroll_to == Some(node_id);
        if needs_scroll {
            ui.scroll_to_rect(rect, Some(egui::Align::Center));
            self.scroll_to = None;
        }

        if ui.is_rect_visible(rect) {
            let bg = if is_selected {
                Color32::from_rgba_unmultiplied(30, 120, 255, 60)
            } else if response.hovered() {
                Color32::from_rgba_unmultiplied(100, 100, 100, 30)
            } else {
                Color32::TRANSPARENT
            };

            ui.painter().rect_filled(rect, egui::CornerRadius::ZERO, bg);

            // Collapse triangle
            let triangle_x = rect.left() + indent + 4.0;
            let triangle_center = egui::Pos2::new(triangle_x, rect.center().y);

            if !is_leaf {
                let triangle_color = Color32::from_gray(160);
                if is_collapsed {
                    // Right-pointing triangle ▶
                    ui.painter().add(egui::Shape::convex_polygon(
                        vec![
                            triangle_center + egui::Vec2::new(-4.0, -5.0),
                            triangle_center + egui::Vec2::new(6.0, 0.0),
                            triangle_center + egui::Vec2::new(-4.0, 5.0),
                        ],
                        triangle_color,
                        egui::Stroke::NONE,
                    ));
                } else {
                    // Down-pointing triangle ▼
                    ui.painter().add(egui::Shape::convex_polygon(
                        vec![
                            triangle_center + egui::Vec2::new(-5.0, -3.0),
                            triangle_center + egui::Vec2::new(5.0, -3.0),
                            triangle_center + egui::Vec2::new(0.0, 5.0),
                        ],
                        triangle_color,
                        egui::Stroke::NONE,
                    ));
                }
            }

            // Tag text
            let text_x = triangle_x + 14.0;
            let text_pos = egui::Pos2::new(text_x, rect.center().y);
            ui.painter().text(
                text_pos,
                egui::Align2::LEFT_CENTER,
                &label,
                egui::FontId::monospace(12.0),
                tag_color,
            );
        }

        if response.clicked() {
            *clicked = Some(node_id);
            self.selected = Some(node_id);
            if !is_leaf {
                if self.collapsed.contains(&node_id) {
                    self.collapsed.remove(&node_id);
                } else {
                    self.collapsed.insert(node_id);
                }
            }
        }

        // Recurse into children if not collapsed
        if !is_collapsed && is_container {
            for &child in &node.children {
                self.show_node(ui, doc, child, depth + 1, clicked);
            }
        } else if !is_collapsed && !is_leaf {
            for &child in &node.children {
                self.show_node(ui, doc, child, depth + 1, clicked);
            }
        }
    }

    /// Select a node and schedule it to be scrolled into view.
    pub fn select_and_scroll(&mut self, node_id: NodeId, doc: &SvgDocument) {
        self.selected = Some(node_id);
        self.scroll_to = Some(node_id);
        // Ensure all ancestors are expanded
        self.expand_ancestors(node_id, doc);
    }

    fn expand_ancestors(&mut self, node_id: NodeId, doc: &SvgDocument) {
        let mut current = node_id;
        loop {
            let node = doc.get(current);
            self.collapsed.remove(&current);
            match node.parent {
                Some(p) => current = p,
                None => break,
            }
        }
    }
}

fn tag_color(tag: &str) -> Color32 {
    match tag {
        "svg" => Color32::from_rgb(100, 200, 100),
        "g" | "a" => Color32::from_rgb(150, 180, 255),
        "path" => Color32::from_rgb(255, 180, 80),
        "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon" => {
            Color32::from_rgb(255, 220, 100)
        }
        "text" | "tspan" => Color32::from_rgb(200, 150, 255),
        "defs" | "clipPath" | "mask" => Color32::from_rgb(120, 120, 120),
        "image" => Color32::from_rgb(100, 220, 220),
        "use" => Color32::from_rgb(200, 200, 100),
        _ => Color32::from_gray(180),
    }
}

fn build_label(tag: &str, attrs: &str) -> String {
    if attrs.is_empty() {
        format!("<{tag}>")
    } else {
        format!("<{tag}> {attrs}")
    }
}
