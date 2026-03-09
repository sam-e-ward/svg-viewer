/// Top-level egui application.
/// Manages the split-pane layout, file loading, keyboard state,
/// and coordinates between the renderer, spatial index, and elements pane.

use std::collections::HashMap;
use std::path::PathBuf;

use egui::{CentralPanel, Color32, Context, Key, Rect, Sense, SidePanel, TextureHandle, TopBottomPanel, Vec2};

use crate::elements_pane::ElementsPane;
use crate::renderer::{GeometryCache, RenderContext, ViewTransform};
use crate::spatial_index::SpatialIndex;
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

    // --- Spatial index ---
    /// Built once per document load; used for all hit-testing
    spatial_index: Option<SpatialIndex>,
    /// Tessellation cache — built once per load, reused every frame
    geometry_cache: GeometryCache,

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
            spatial_index: None,
            geometry_cache: GeometryCache::new(),
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
                    // Build spatial index and geometry cache
                    let index = SpatialIndex::build(&doc);
                    let cache = GeometryCache::build(&doc);
                    log::info!("Spatial index + geometry cache built for {} nodes", doc.nodes.len());
                    self.spatial_index = Some(index);
                    self.geometry_cache = cache;
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
                        let hit = self
                            .spatial_index
                            .as_ref()
                            .and_then(|idx| idx.hit_test_precise(doc, sx, sy));
                        if let Some(hit) = hit {
                            if self.highlighted != Some(hit) {
                                self.highlighted = Some(hit);
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
                    viewport: rect,
                    highlight: self.highlighted,
                    textures: &self.textures,
                    cache: &self.geometry_cache,
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


