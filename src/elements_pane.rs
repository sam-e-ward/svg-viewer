/// Elements pane — renders the SVG document tree as a collapsible egui widget.

use egui::{Color32, Ui};
use std::collections::HashSet;

use crate::svg_doc::{NodeId, SvgDocument};

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
    /// Returns `(clicked, hovered)` NodeIds for this frame.
    pub fn show(&mut self, ui: &mut Ui, doc: &SvgDocument) -> (Option<NodeId>, Option<NodeId>) {
        let mut clicked = None;
        let mut hovered = None;

        // Intercept Shift+scroll to drive horizontal scrolling.
        // We do this by converting vertical scroll delta → horizontal when shift is held,
        // before the ScrollArea consumes it.
        let pointer_over = ui.rect_contains_pointer(ui.available_rect_before_wrap());
        if pointer_over && ui.input(|i| i.modifiers.shift) {
            let v_delta = ui.input(|i| i.smooth_scroll_delta.y);
            if v_delta != 0.0 {
                // Consume the vertical delta and re-emit it as horizontal
                ui.input_mut(|i| {
                    i.smooth_scroll_delta.x -= i.smooth_scroll_delta.y;
                    i.smooth_scroll_delta.y = 0.0;
                });
            }
        }

        egui::ScrollArea::both()
            .id_salt("elements_scroll")
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
            .show(ui, |ui| {
                ui.style_mut().spacing.item_spacing.y = 1.0;
                self.show_node(ui, doc, doc.root, 0, &mut clicked, &mut hovered);
            });

        (clicked, hovered)
    }

    fn show_node(
        &mut self,
        ui: &mut Ui,
        doc: &SvgDocument,
        node_id: NodeId,
        depth: usize,
        clicked: &mut Option<NodeId>,
        hovered: &mut Option<NodeId>,
    ) {
        let node = doc.get(node_id);

        // Skip filtered nodes entirely — they can number in the hundreds of
        // thousands and would freeze the UI if we tried to lay out rows for all of them.
        if node.filtered {
            return;
        }

        let is_leaf = node.children.is_empty();
        let is_collapsed = self.collapsed.contains(&node_id);
        let is_selected = self.selected == Some(node_id);

        let indent = depth as f32 * 14.0;
        let tag_color = tag_color(&node.tag_name);
        let label = build_label(&node.tag_name, &node.attr_summary);

        // --- Row ---
        // We use a horizontal layout so the label widget can be selectable.
        // The row background and collapse-triangle are painted manually; the text
        // is an egui Label so the user can select/copy it.

        let row_response = ui.horizontal(|ui| {
            // Indent spacer
            ui.add_space(indent);

            // Collapse triangle — a small clickable region
            let triangle_size = egui::Vec2::new(14.0, 20.0);
            let (tri_rect, tri_response) = ui.allocate_exact_size(triangle_size, egui::Sense::click());

            if ui.is_rect_visible(tri_rect) && !is_leaf {
                let triangle_color = Color32::from_gray(160);
                let c = tri_rect.center();
                if is_collapsed {
                    ui.painter().add(egui::Shape::convex_polygon(
                        vec![
                            c + egui::Vec2::new(-3.0, -5.0),
                            c + egui::Vec2::new(5.0, 0.0),
                            c + egui::Vec2::new(-3.0, 5.0),
                        ],
                        triangle_color,
                        egui::Stroke::NONE,
                    ));
                } else {
                    ui.painter().add(egui::Shape::convex_polygon(
                        vec![
                            c + egui::Vec2::new(-5.0, -3.0),
                            c + egui::Vec2::new(5.0, -3.0),
                            c + egui::Vec2::new(0.0, 5.0),
                        ],
                        triangle_color,
                        egui::Stroke::NONE,
                    ));
                }
            }

            // Selectable label — wrapping disabled so the full text stays on one line
            let rich = egui::RichText::new(&label)
                .monospace()
                .size(12.0)
                .color(tag_color);
            let label_response = ui.add(
                egui::Label::new(rich)
                    .selectable(true)
                    .extend()   // single line, no wrap, no truncate
            );

            (tri_response, label_response)
        });

        let (tri_response, label_response) = row_response.inner;
        let row_rect = row_response.response.rect;

        // Scroll to selected
        if self.scroll_to == Some(node_id) {
            ui.scroll_to_rect(row_rect, Some(egui::Align::Center));
            self.scroll_to = None;
        }

        // Row background (drawn behind everything via the painter)
        let bg = if is_selected {
            Color32::from_rgba_unmultiplied(30, 120, 255, 60)
        } else if row_response.response.hovered() || label_response.hovered() {
            Color32::from_rgba_unmultiplied(100, 100, 100, 30)
        } else {
            Color32::TRANSPARENT
        };
        if bg != Color32::TRANSPARENT {
            ui.painter().rect_filled(row_rect, egui::CornerRadius::ZERO, bg);
        }

        // Hover / click handling
        if row_response.response.hovered() || label_response.hovered() {
            *hovered = Some(node_id);
        }

        // Clicking the triangle or the label row selects / collapses
        let row_clicked = row_response.response.clicked()
            || tri_response.clicked()
            || label_response.clicked();

        if row_clicked {
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

        // Recurse
        if !is_collapsed {
            for &child in &node.children.clone() {
                self.show_node(ui, doc, child, depth + 1, clicked, hovered);
            }
        }
    }

    /// Pre-collapse groups with many visible (non-filtered) children.
    /// Called once after loading a large document to prevent the tree from
    /// overwhelming the UI on first render.
    pub fn auto_collapse_large_groups(&mut self, doc: &SvgDocument) {
        const COLLAPSE_THRESHOLD: usize = 200;
        for node in &doc.nodes {
            if !node.children.is_empty() {
                let visible_children = node.children.iter()
                    .filter(|&&c| !doc.get(c).filtered)
                    .count();
                if visible_children > COLLAPSE_THRESHOLD {
                    self.collapsed.insert(node.id);
                }
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
