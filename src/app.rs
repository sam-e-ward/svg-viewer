/// Top-level egui application.
/// Manages the split-pane layout, file loading, keyboard state,
/// and coordinates between the renderer, spatial index, and elements pane.

use std::collections::HashMap;
use std::path::PathBuf;

use egui::{CentralPanel, Color32, Context, Key, Rect, Sense, SidePanel, TextureHandle, TopBottomPanel, Vec2};

use crate::elements_pane::ElementsPane;
use crate::renderer::{RenderContext, ViewTransform};
use crate::svg_doc::{NodeId, SvgDocument, SvgNodeKind, SvgShape};
use crate::{parser, renderer};

/// Minimum scale (zoom out limit)
const MIN_SCALE: f32 = 0.01;
/// Maximum scale (zoom in limit)
const MAX_SCALE: f32 = 500.0;

pub struct SvgViewerApp {
    /// Currently loaded document, if any
    doc: Option<SvgDocument>,
    /// File path of the currently open file
    file_path: Option<PathBuf>,
    /// Parse error message if loading failed
    error: Option<String>,

    // --- View state ---
    view: ViewTransform,
    /// Whether the view has been fitted to the document since last load
    view_fitted: bool,

    // --- Interaction state ---
    /// True while spacebar is held
    spacebar_held: bool,
    /// Currently highlighted element (from spacebar hover)
    highlighted: Option<NodeId>,
    /// True if we're currently panning (dragging)
    panning: bool,
    last_drag_pos: Option<egui::Pos2>,

    // --- Elements pane ---
    elements_pane: ElementsPane,

    // --- Image textures ---
    /// Textures uploaded to egui for <image> nodes, keyed by NodeId
    textures: HashMap<NodeId, TextureHandle>,
    /// egui context used for texture uploads (stored after first frame)
    egui_ctx: Option<Context>,
}

impl SvgViewerApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        SvgViewerApp {
            doc: None,
            file_path: None,
            error: None,
            view: ViewTransform {
                offset: Vec2::ZERO,
                scale: 1.0,
            },
            view_fitted: false,
            spacebar_held: false,
            highlighted: None,
            panning: false,
            last_drag_pos: None,
            elements_pane: ElementsPane::new(),
            textures: HashMap::new(),
            egui_ctx: None,
        }
    }

    fn load_file(&mut self, path: PathBuf) {
        match std::fs::read_to_string(&path) {
            Ok(contents) => match parser::parse_svg(&contents) {
                Ok(mut doc) => {
                    // Attempt to resolve external image hrefs relative to the SVG directory
                    let svg_dir = path.parent().map(|p| p.to_path_buf());
                    parser::resolve_external_images(&mut doc, svg_dir.as_deref());

                    self.textures.clear();
                    // Schedule texture upload on next frame (needs egui context)
                    self.doc = Some(doc);
                    self.file_path = Some(path);
                    self.error = None;
                    self.view_fitted = false;
                    self.highlighted = None;
                    self.elements_pane = ElementsPane::new();
                }
                Err(e) => {
                    self.error = Some(format!("Parse error: {e}"));
                    self.doc = None;
                }
            },
            Err(e) => {
                self.error = Some(format!("Read error: {e}"));
            }
        }
    }

    /// Upload any decoded image pixels to egui textures.
    /// Call once per frame until all images are uploaded.
    fn upload_pending_textures(&mut self, ctx: &Context) {
        let doc = match &self.doc {
            Some(d) => d,
            None => return,
        };
        // Collect nodes that need uploading
        let pending: Vec<_> = doc
            .nodes
            .iter()
            .filter_map(|n| {
                if self.textures.contains_key(&n.id) {
                    return None;
                }
                if let SvgNodeKind::Shape(SvgShape::Image { pixels: Some(px), .. }) = &n.kind {
                    Some((n.id, px.clone()))
                } else {
                    None
                }
            })
            .collect();

        for (node_id, px) in pending {
            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                [px.width as usize, px.height as usize],
                &px.rgba,
            );
            let handle = ctx.load_texture(
                format!("svg_image_{}", node_id.0),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            self.textures.insert(node_id, handle);
        }
    }

    fn open_file_dialog(&mut self) {
        // rfd is synchronous on macOS, runs its own event loop
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("SVG files", &["svg"])
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.load_file(path);
        }
    }

    fn fit_view(&mut self, viewport: Rect) {
        if let Some(doc) = &self.doc {
            self.view = ViewTransform::fit(doc.width, doc.height, viewport);
            self.view_fitted = true;
        }
    }

    /// Hit-test: find the topmost visible element at SVG-space point (sx, sy).
    /// Uses a simple back-to-front walk for now; M5 will replace with R-tree.
    fn hit_test(&self, doc: &SvgDocument, sx: f32, sy: f32) -> Option<NodeId> {
        hit_test_node(doc, doc.root, sx, sy, &crate::svg_doc::Transform::identity())
    }
}

impl eframe::App for SvgViewerApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // -- Top menu bar --
        TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open SVG...").clicked() {
                        ui.close_menu();
                        self.open_file_dialog();
                    }
                    if self.doc.is_some() {
                        if ui.button("Fit to window").clicked() {
                            ui.close_menu();
                            self.view_fitted = false; // will refit next frame
                        }
                    }
                });

                ui.separator();

                // Status info
                if let Some(path) = &self.file_path {
                    ui.label(
                        egui::RichText::new(
                            path.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .as_ref(),
                        )
                        .weak(),
                    );
                    if let Some(doc) = &self.doc {
                        ui.label(
                            egui::RichText::new(format!(
                                "  {}×{}  |  {} elements",
                                doc.width as u32,
                                doc.height as u32,
                                doc.nodes.len()
                            ))
                            .weak(),
                        );
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.spacebar_held {
                        ui.label(
                            egui::RichText::new("INSPECT MODE")
                                .color(Color32::from_rgb(30, 150, 255))
                                .strong(),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("Hold SPACE to inspect")
                                .weak()
                                .small(),
                        );
                    }
                });
            });
        });

        // -- Keyboard state --
        ctx.input(|i| {
            self.spacebar_held = i.key_down(Key::Space);
        });

        // -- Drop file support --
        ctx.input(|i| {
            if let Some(dropped) = i.raw.dropped_files.first() {
                if let Some(path) = &dropped.path {
                    self.load_file(path.clone());
                }
            }
        });

        // -- Right: elements pane --
        SidePanel::right("elements_panel")
            .resizable(true)
            .default_width(400.0)
            .min_width(200.0)
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    ui.add_space(4.0);
                    ui.heading("Elements");
                    ui.separator();

                    if let Some(doc) = &self.doc {
                        // Clone the highlight state so we can pass to show()
                        let mouse_in_elements = ui.rect_contains_pointer(ui.max_rect());

                        if self.spacebar_held && mouse_in_elements {
                            // FR-6: hover over elements pane → highlight in viewer
                            if let Some(hover_pos) = ui.input(|i| i.pointer.hover_pos()) {
                                // We'll resolve which node is under the mouse
                                // in a future milestone with proper row tracking.
                                // For M1 the spacebar just keeps the selection visible.
                                let _ = hover_pos;
                            }
                        }

                        // We need a mutable borrow of elements_pane and immutable doc.
                        // Temporarily take the doc out to satisfy borrow checker.
                        let doc_ref = doc as *const SvgDocument;
                        let doc_ref = unsafe { &*doc_ref };

                        if let Some(clicked) = self.elements_pane.show(ui, doc_ref) {
                            // Element clicked → highlight it
                            self.highlighted = Some(clicked);
                            // TODO M6: scroll viewer to element bbox
                        }
                    } else if self.error.is_some() {
                        // Error shown in main panel
                    } else {
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                egui::RichText::new("Open an SVG file to begin")
                                    .weak()
                                    .italics(),
                            );
                        });
                    }
                });
            });

        // -- Left: SVG viewer --
        CentralPanel::default().show(ctx, |ui| {
            let viewport = ui.max_rect();

            // Fit on first load
            if !self.view_fitted && self.doc.is_some() {
                self.fit_view(viewport);
            }

            if let Some(error) = &self.error {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new(error).color(Color32::RED));
                });
                return;
            }

            if self.doc.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.label(
                            egui::RichText::new("SVG Viewer")
                                .size(32.0)
                                .weak(),
                        );
                        ui.add_space(16.0);
                        ui.label(egui::RichText::new("Drop an SVG file here, or use File → Open").weak());
                        ui.add_space(24.0);
                        if ui.button("Open SVG...").clicked() {
                            self.open_file_dialog();
                        }
                    });
                });
                return;
            }

            // Allocate the full panel area as interactive
            let (rect, response) =
                ui.allocate_exact_size(viewport.size(), Sense::click_and_drag());

            // -- Zoom (scroll wheel) --
            let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
            if response.hovered() && scroll_delta.y != 0.0 {
                let zoom_factor = (scroll_delta.y * 0.002).exp();
                // Zoom toward the mouse cursor
                let mouse_pos = ui
                    .input(|i| i.pointer.hover_pos())
                    .unwrap_or(rect.center());
                let before = self.view.screen_to_svg(mouse_pos);
                self.view.scale = (self.view.scale * zoom_factor).clamp(MIN_SCALE, MAX_SCALE);
                let after_pos = self.view.svg_to_screen(before.0, before.1);
                self.view.offset += mouse_pos.to_vec2() - after_pos.to_vec2();
            }

            // -- Pan (drag) --
            if response.drag_started() {
                self.panning = true;
            }
            if self.panning {
                let delta = response.drag_delta();
                self.view.offset += delta;
            }
            if response.drag_stopped() {
                self.panning = false;
            }

            // -- Spacebar hover → hit-test (FR-5) --
            if self.spacebar_held && response.hovered() {
                if let Some(hover_pos) = ui.input(|i| i.pointer.hover_pos()) {
                    if let Some(doc) = &self.doc {
                        let (sx, sy) = self.view.screen_to_svg(hover_pos);
                        if let Some(hit) = self.hit_test(doc, sx, sy) {
                            if self.highlighted != Some(hit) {
                                self.highlighted = Some(hit);
                                // Tell elements pane to scroll to this node
                                let doc_ptr = doc as *const SvgDocument;
                                let doc_ref = unsafe { &*doc_ptr };
                                self.elements_pane.select_and_scroll(hit, doc_ref);
                            }
                        }
                    }
                }
            }

            // Upload any pending image textures
            self.upload_pending_textures(ctx);

            // -- Render the SVG --
            let painter = ui.painter_at(rect);
            // Draw canvas background
            painter.rect_filled(rect, egui::CornerRadius::ZERO, Color32::from_gray(240));

            if let Some(doc) = &self.doc {
                let render_ctx = RenderContext {
                    doc,
                    vt: &self.view,
                    painter: &painter,
                    highlight: self.highlighted,
                    textures: &self.textures,
                };
                renderer::render(&render_ctx);
            }

            // Cursor hint
            if self.spacebar_held {
                ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Simple recursive hit-tester (no spatial index yet — replaced in M5)
// ---------------------------------------------------------------------------

fn hit_test_node(
    doc: &SvgDocument,
    node_id: NodeId,
    sx: f32,
    sy: f32,
    parent_transform: &crate::svg_doc::Transform,
) -> Option<NodeId> {
    let node = doc.get(node_id);
    let combined = parent_transform.concat(&node.transform);

    match &node.kind {
        crate::svg_doc::SvgNodeKind::Svg { .. }
        | crate::svg_doc::SvgNodeKind::Group => {
            // Walk children back-to-front, return first (topmost) hit
            for &child in node.children.iter().rev() {
                if let Some(hit) = hit_test_node(doc, child, sx, sy, &combined) {
                    return Some(hit);
                }
            }
            None
        }
        crate::svg_doc::SvgNodeKind::Shape(shape) => {
            if shape_hit_test(shape, &combined, sx, sy) {
                Some(node_id)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn shape_hit_test(
    shape: &crate::svg_doc::SvgShape,
    transform: &crate::svg_doc::Transform,
    sx: f32,
    sy: f32,
) -> bool {
    use crate::svg_doc::SvgShape;

    // Transform the test point into the element's local space
    // (approximate inverse for uniform-ish transforms)
    let (lx, ly) = inverse_transform_point(transform, sx, sy);

    match shape {
        SvgShape::Rect { x, y, width, height, .. } => {
            lx >= *x && lx <= x + width && ly >= *y && ly <= y + height
        }
        SvgShape::Circle { cx, cy, r } => {
            let dx = lx - cx;
            let dy = ly - cy;
            dx * dx + dy * dy <= r * r
        }
        SvgShape::Ellipse { cx, cy, rx, ry } => {
            if *rx == 0.0 || *ry == 0.0 {
                return false;
            }
            let dx = (lx - cx) / rx;
            let dy = (ly - cy) / ry;
            dx * dx + dy * dy <= 1.0
        }
        SvgShape::Line { x1, y1, x2, y2 } => {
            // Point within ~4 units of the line
            point_to_segment_dist(lx, ly, *x1, *y1, *x2, *y2) < 4.0
        }
        SvgShape::Path { data } => {
            // Bounding box hit test only — precise test deferred to M5
            path_bbox_hit(data, lx, ly)
        }
        SvgShape::Polyline { points } | SvgShape::Polygon { points } => {
            if points.is_empty() {
                return false;
            }
            // Bounding box test
            let min_x = points.iter().map(|(x, _)| *x).fold(f32::INFINITY, f32::min);
            let max_x = points.iter().map(|(x, _)| *x).fold(f32::NEG_INFINITY, f32::max);
            let min_y = points.iter().map(|(_, y)| *y).fold(f32::INFINITY, f32::min);
            let max_y = points.iter().map(|(_, y)| *y).fold(f32::NEG_INFINITY, f32::max);
            lx >= min_x && lx <= max_x && ly >= min_y && ly <= max_y
        }
        SvgShape::Text { x, y, spans, font_size } => {
            // Approximate bounding box across all spans
            let total_chars: usize = spans.iter().map(|s| s.content.len()).sum();
            let w = total_chars as f32 * font_size * 0.6;
            let h = *font_size;
            lx >= *x && lx <= x + w && ly >= y - h && ly <= *y
        }
        SvgShape::Image { x, y, width, height, .. } => {
            lx >= *x && lx <= x + width && ly >= *y && ly <= y + height
        }
        SvgShape::Use { .. } => false,
    }
}

fn inverse_transform_point(t: &crate::svg_doc::Transform, x: f32, y: f32) -> (f32, f32) {
    // Compute inverse of 2D affine matrix
    let [a, b, c, d, e, f] = t.matrix;
    let det = a * d - b * c;
    if det.abs() < 1e-10 {
        return (x, y);
    }
    let inv_det = 1.0 / det;
    let ia = d * inv_det;
    let ib = -b * inv_det;
    let ic = -c * inv_det;
    let id = a * inv_det;
    let ie = (c * f - d * e) * inv_det;
    let if_ = (b * e - a * f) * inv_det;
    (ia * x + ic * y + ie, ib * x + id * y + if_)
}

fn point_to_segment_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;
    if len_sq < 1e-10 {
        let dpx = px - ax;
        let dpy = py - ay;
        return (dpx * dpx + dpy * dpy).sqrt();
    }
    let t = ((px - ax) * dx + (py - ay) * dy) / len_sq;
    let t = t.clamp(0.0, 1.0);
    let qx = ax + t * dx;
    let qy = ay + t * dy;
    let dpx = px - qx;
    let dpy = py - qy;
    (dpx * dpx + dpy * dpy).sqrt()
}

fn path_bbox_hit(data: &str, lx: f32, ly: f32) -> bool {
    // Quick scan of path data numbers to get approximate bounding box
    let nums: Vec<f32> = data
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if nums.len() < 2 {
        return false;
    }
    // Separate x/y coords (rough — every other number, starting from first pair)
    let xs: Vec<f32> = nums.iter().copied().step_by(2).collect();
    let ys: Vec<f32> = nums.iter().skip(1).copied().step_by(2).collect();
    let min_x = xs.iter().copied().fold(f32::INFINITY, f32::min);
    let max_x = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let min_y = ys.iter().copied().fold(f32::INFINITY, f32::min);
    let max_y = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);

    // Add some tolerance
    let pad = 4.0;
    lx >= min_x - pad && lx <= max_x + pad && ly >= min_y - pad && ly <= max_y + pad
}
