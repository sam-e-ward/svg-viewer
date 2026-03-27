/// Elements pane — renders the SVG document tree as a collapsible egui widget.
///
/// Uses virtual scrolling: only rows within the visible scroll region are
/// actually laid out. Off-screen rows are replaced by spacers, keeping the
/// per-frame cost proportional to the viewport height rather than the total
/// node count.

use egui::{Color32, Ui};
use std::collections::HashSet;

use crate::svg_doc::{NodeId, SvgDocument};

/// Fixed row height in logical pixels. Must match the actual layout height
/// (triangle_size.y=20 + item_spacing.y=1).
const ROW_HEIGHT: f32 = 21.0;

pub struct ElementsPane {
    /// Set of node ids that are currently collapsed (children hidden)
    pub collapsed: HashSet<NodeId>,
    /// The currently selected/highlighted node
    pub selected: Option<NodeId>,
    /// Node to scroll into view on the next frame
    pub scroll_to: Option<NodeId>,
    /// Flattened list of (NodeId, depth) for all currently visible rows.
    /// Rebuilt when the tree structure changes (expand/collapse/load).
    flat_rows: Vec<(NodeId, usize)>,
    /// Whether flat_rows needs to be rebuilt on the next frame.
    rows_dirty: bool,
}

impl ElementsPane {
    pub fn new() -> Self {
        ElementsPane {
            collapsed: HashSet::new(),
            selected: None,
            scroll_to: None,
            flat_rows: Vec::new(),
            rows_dirty: true,
        }
    }

    /// Mark the flat row list as needing a rebuild (e.g. after expand/collapse).
    fn invalidate_rows(&mut self) {
        self.rows_dirty = true;
    }

    /// Rebuild the flat row list by walking the tree.
    fn rebuild_rows(&mut self, doc: &SvgDocument) {
        self.flat_rows.clear();
        self.collect_visible_rows(doc, doc.root, 0);
        self.rows_dirty = false;
    }

    fn collect_visible_rows(&mut self, doc: &SvgDocument, node_id: NodeId, depth: usize) {
        let node = doc.get(node_id);
        if node.filtered {
            return;
        }
        self.flat_rows.push((node_id, depth));
        if !self.collapsed.contains(&node_id) {
            for &child in &node.children {
                self.collect_visible_rows(doc, child, depth + 1);
            }
        }
    }

    /// Main render function — call inside a panel or frame.
    /// Returns `(clicked, hovered)` NodeIds for this frame.
    pub fn show(&mut self, ui: &mut Ui, doc: &SvgDocument) -> (Option<NodeId>, Option<NodeId>) {
        let mut clicked = None;
        let mut hovered = None;

        if self.rows_dirty {
            self.rebuild_rows(doc);
        }

        // Handle scroll-to request: find the row index and compute the target offset
        let mut scroll_to_offset: Option<f32> = None;
        if let Some(target) = self.scroll_to {
            if let Some(idx) = self.flat_rows.iter().position(|(id, _)| *id == target) {
                scroll_to_offset = Some(idx as f32 * ROW_HEIGHT);
            }
            self.scroll_to = None;
        }

        // Intercept Shift+scroll to drive horizontal scrolling.
        let pointer_over = ui.rect_contains_pointer(ui.available_rect_before_wrap());
        if pointer_over && ui.input(|i| i.modifiers.shift) {
            let v_delta = ui.input(|i| i.smooth_scroll_delta.y);
            if v_delta != 0.0 {
                ui.input_mut(|i| {
                    i.smooth_scroll_delta.x -= i.smooth_scroll_delta.y;
                    i.smooth_scroll_delta.y = 0.0;
                });
            }
        }

        let total_rows = self.flat_rows.len();

        let mut scroll_area = egui::ScrollArea::both()
            .id_salt("elements_scroll")
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded);

        if let Some(offset) = scroll_to_offset {
            scroll_area = scroll_area.vertical_scroll_offset(
                offset - ui.available_height() / 2.0 + ROW_HEIGHT / 2.0,
            );
        }

        scroll_area.show(ui, |ui| {
            ui.style_mut().spacing.item_spacing.y = 1.0;

            // Determine the visible range from the scroll offset and viewport height.
            let scroll_offset = ui.clip_rect().min.y - ui.cursor().min.y;
            let viewport_height = ui.clip_rect().height();

            let first_visible = (scroll_offset / ROW_HEIGHT).floor().max(0.0) as usize;
            let last_visible = ((scroll_offset + viewport_height) / ROW_HEIGHT).ceil() as usize;
            // Add a small buffer to avoid popping
            let first = first_visible.saturating_sub(2);
            let last = (last_visible + 2).min(total_rows);

            // Top spacer for rows above the viewport
            if first > 0 {
                ui.add_space(first as f32 * ROW_HEIGHT);
            }

            // Render only the visible rows
            for idx in first..last {
                if let Some(&(node_id, depth)) = self.flat_rows.get(idx) {
                    self.show_row(ui, doc, node_id, depth, &mut clicked, &mut hovered);
                }
            }

            // Bottom spacer for rows below the viewport
            let remaining = total_rows.saturating_sub(last);
            if remaining > 0 {
                ui.add_space(remaining as f32 * ROW_HEIGHT);
            }
        });

        // If a click expanded/collapsed a node, rebuild the flat list
        if clicked.is_some() {
            self.invalidate_rows();
        }

        (clicked, hovered)
    }

    fn show_row(
        &mut self,
        ui: &mut Ui,
        doc: &SvgDocument,
        node_id: NodeId,
        depth: usize,
        clicked: &mut Option<NodeId>,
        hovered: &mut Option<NodeId>,
    ) {
        let node = doc.get(node_id);
        let is_leaf = node.children.is_empty()
            || node.children.iter().all(|&c| doc.get(c).filtered);
        let is_collapsed = self.collapsed.contains(&node_id);
        let is_selected = self.selected == Some(node_id);

        let indent = depth as f32 * 14.0;
        let tag_color = tag_color(&node.tag_name);
        let label = build_label(&node.tag_name, &node.attr_summary);

        let row_response = ui.horizontal(|ui| {
            ui.set_height(ROW_HEIGHT - 1.0); // -1 for item_spacing
            ui.add_space(indent);

            // Collapse triangle
            let triangle_size = egui::Vec2::new(14.0, ROW_HEIGHT - 1.0);
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

            let rich = egui::RichText::new(&label)
                .monospace()
                .size(12.0)
                .color(tag_color);
            let label_response = ui.add(
                egui::Label::new(rich)
                    .selectable(true)
                    .extend()
            );

            (tri_response, label_response)
        });

        let (tri_response, label_response) = row_response.inner;
        let row_rect = row_response.response.rect;

        // Row background
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

        // Hover / click
        if row_response.response.hovered() || label_response.hovered() {
            *hovered = Some(node_id);
        }

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
    }

    /// Pre-collapse groups with many visible (non-filtered) children.
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
        self.invalidate_rows();
    }

    /// Select a node and schedule it to be scrolled into view.
    pub fn select_and_scroll(&mut self, node_id: NodeId, doc: &SvgDocument) {
        self.selected = Some(node_id);
        self.scroll_to = Some(node_id);
        self.expand_ancestors(node_id, doc);
        // Expanding ancestors changes the flat list
        self.invalidate_rows();
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
