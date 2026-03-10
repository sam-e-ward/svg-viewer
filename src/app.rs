/// Top-level egui application.
/// Manages the split-pane layout, file loading, keyboard state,
/// and coordinates between the renderer, spatial index, and elements pane.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

const MAX_RECENTS: usize = 10;
const RECENTS_FILE: &str = "svg-viewer/recents.json";

use egui::{CentralPanel, Color32, Context, Key, Rect, Sense, SidePanel, TextureHandle, TopBottomPanel, Vec2};
use egui::epaint::StrokeKind;

use crate::clip_index::ClipIndex;
use crate::elements_pane::ElementsPane;
use crate::renderer::{GeometryCache, RenderContext, ViewTransform};
use crate::spatial_index::SpatialIndex;
use crate::svg_doc::{NodeId, SvgDocument, SvgNodeKind, SvgShape};
use crate::{parser, renderer};

/// Which pane currently owns keyboard focus / spacebar behaviour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ActivePane {
    Viewer,
    Elements,
}

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
    /// Which pane has focus for spacebar / keyboard interactions
    active_pane: ActivePane,
    /// Set to true on the frame a pane-activation click is consumed,
    /// so content logic in that pane is skipped for that frame.
    activation_consumed: bool,
    /// True while spacebar is held
    spacebar_held: bool,
    /// Currently highlighted element (from spacebar hover or click)
    highlighted: Option<NodeId>,
    /// True if `highlighted` was set by spacebar-hover (transient); false if set by a click (sticky)
    highlight_transient: bool,
    /// World-space bbox for a hovered/clicked group in the elements pane
    group_highlight_bbox: Option<[f32; 4]>,
    /// All elements under the last spacebar-hover position, topmost first (for TAB cycling)
    tab_candidates: Vec<NodeId>,
    /// Index into `tab_candidates` — which element is currently selected by TAB
    tab_index: usize,
    /// The SVG-space cursor position when `tab_candidates` was last built
    tab_cursor_pos: Option<(f32, f32)>,
    /// True if we're currently panning (dragging)
    panning: bool,
    last_drag_pos: Option<egui::Pos2>,

    // --- Elements pane ---
    elements_pane: ElementsPane,
    /// Viewport rect from the previous frame (for click-to-zoom from elements pane)
    last_viewport: Option<Rect>,

    // --- Spatial index ---
    /// Built once per document load; used for all hit-testing
    spatial_index: Option<SpatialIndex>,
    /// Tessellation cache — built once per load, reused every frame
    geometry_cache: GeometryCache,
    /// Clip-path AABB index — built once per load
    clip_index: ClipIndex,

    // --- Image textures ---
    /// Textures uploaded to egui for <image> nodes, keyed by NodeId
    textures: HashMap<NodeId, TextureHandle>,
    /// egui context used for texture uploads (stored after first frame)
    egui_ctx: Option<Context>,

    // --- Recent files / URLs ---
    recent_items: Vec<String>,

    // --- Remote URL loading ---
    /// Whether the "Open from URL" modal is open
    url_modal_open: bool,
    /// Current text in the URL input field
    url_input: String,
    /// Receives the result of a background HTTP fetch: Ok(svg_text) or Err(message)
    url_fetch_rx: Option<Receiver<Result<String, String>>>,
    /// Displayed while a fetch is in flight
    url_loading: bool,
    /// Error from the last failed fetch
    url_error: Option<String>,
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
            active_pane: ActivePane::Viewer,
            activation_consumed: false,
            spacebar_held: false,
            highlighted: None,
            highlight_transient: false,
            group_highlight_bbox: None,
            tab_candidates: Vec::new(),
            tab_index: 0,
            tab_cursor_pos: None,
            panning: false,
            last_drag_pos: None,
            elements_pane: ElementsPane::new(),
            last_viewport: None,
            spatial_index: None,
            geometry_cache: GeometryCache::new(),
            clip_index: ClipIndex { clips: std::collections::HashMap::new() },
            textures: HashMap::new(),
            egui_ctx: None,
            recent_items: load_recents(),
            url_modal_open: false,
            url_input: String::new(),
            url_fetch_rx: None,
            url_loading: false,
            url_error: None,
        }
    }

    fn load_file(&mut self, path: PathBuf) {
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let svg_dir = path.parent().map(|p| p.to_path_buf());
                // Use the full path as the recent key so it can be reopened
                let full_label = path.to_string_lossy().into_owned();
                self.load_from_svg_text(contents, full_label, svg_dir);
                self.file_path = Some(path);
            }
            Err(e) => {
                self.error = Some(format!("Read error: {e}"));
            }
        }
    }

    /// Core loader — parses SVG text, builds all indexes, and resets view state.
    /// `label` is shown in the title bar.  `svg_dir` is used to resolve relative
    /// image hrefs (pass `None` for remote URLs).
    fn load_from_svg_text(&mut self, contents: String, label: String, svg_dir: Option<PathBuf>) {
        match parser::parse_svg(&contents) {
            Ok(mut doc) => {
                parser::resolve_external_images(&mut doc, svg_dir.as_deref());

                self.textures.clear();
                let index = SpatialIndex::build(&doc);
                let cache = GeometryCache::build(&doc);
                let clips = ClipIndex::build(&doc);
                log::info!("Loaded \"{label}\": {} nodes", doc.nodes.len());
                self.spatial_index = Some(index);
                self.geometry_cache = cache;
                self.clip_index = clips;
                self.doc = Some(doc);
                self.file_path = None; // cleared; caller sets it for local files
                self.error = None;
                self.view_fitted = false;
                self.highlighted = None;
                self.highlight_transient = false;
                self.group_highlight_bbox = None;
                self.tab_candidates.clear();
                self.tab_index = 0;
                self.tab_cursor_pos = None;
                self.elements_pane = ElementsPane::new();
                // Store the label in file_path as a synthetic path so the title bar works
                self.file_path = Some(PathBuf::from(label.clone()));
                self.push_recent(label);
            }
            Err(e) => {
                self.error = Some(format!("Parse error: {e}"));
                self.doc = None;
            }
        }
    }

    /// Start a background HTTP fetch for the given URL.
    fn start_url_fetch(&mut self, url: String, egui_ctx: Context) {
        let (tx, rx) = mpsc::channel();
        self.url_fetch_rx = Some(rx);
        self.url_loading = true;
        self.url_error = None;

        std::thread::spawn(move || {
            let result = ureq::get(&url)
                .set("User-Agent", "svg-viewer/1.0")
                .call()
                .map_err(|e| format!("Request failed: {e}"))
                .and_then(|resp| {
                    resp.into_string()
                        .map_err(|e| format!("Failed to read response: {e}"))
                });
            let _ = tx.send(result);
            egui_ctx.request_repaint();
        });
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

    /// Pan and zoom the viewer so that the given world-space bbox
    /// `[min_x, min_y, max_x, max_y]` is centered with a small margin.
    fn zoom_to_bbox(&mut self, bbox: [f32; 4]) {
        let viewport = match self.last_viewport {
            Some(r) => r,
            None => return,
        };

        let [min_x, min_y, max_x, max_y] = bbox;
        let w = (max_x - min_x).max(1.0);
        let h = (max_y - min_y).max(1.0);
        let margin = 0.15; // 15% padding on each side

        let scale_x = viewport.width() / (w * (1.0 + 2.0 * margin));
        let scale_y = viewport.height() / (h * (1.0 + 2.0 * margin));
        let scale = scale_x.min(scale_y).clamp(MIN_SCALE, MAX_SCALE);

        let cx_svg = (min_x + max_x) / 2.0;
        let cy_svg = (min_y + max_y) / 2.0;

        // After applying scale, the SVG center should map to the viewport center.
        // screen = svg * scale + offset  →  offset = screen_center - svg_center * scale
        self.view.scale = scale;
        self.view.offset = egui::Vec2::new(
            viewport.center().x - cx_svg * scale,
            viewport.center().y - cy_svg * scale,
        );
        self.view_fitted = true; // suppress fit-on-load next frame
    }

    /// Add an item to the top of the recents list and persist it.
    fn push_recent(&mut self, item: String) {
        self.recent_items.retain(|r| r != &item);
        self.recent_items.insert(0, item);
        self.recent_items.truncate(MAX_RECENTS);
        save_recents(&self.recent_items);
    }
}

// ---------------------------------------------------------------------------
// Recent items persistence — plain JSON, no serde dependency
// ---------------------------------------------------------------------------

fn recents_path() -> Option<PathBuf> {
    // ~/.config/svg-viewer/recents.json
    dirs_next().map(|d| d.join(RECENTS_FILE))
}

/// Returns the platform config directory, or ~/.config as a fallback.
fn dirs_next() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(p));
    }
    // macOS: ~/Library/Application Support  (but ~/.config also works and is simpler)
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config"))
}

fn load_recents() -> Vec<String> {
    let path = match recents_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    parse_json_string_array(&text)
}

fn save_recents(items: &[String]) {
    let path = match recents_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = format_json_string_array(items);
    let _ = std::fs::write(&path, json);
}

/// Minimal JSON string-array serialiser — no serde needed.
fn format_json_string_array(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{}\"", escaped)
    }).collect();
    format!("[\n  {}\n]\n", inner.join(",\n  "))
}

/// Minimal JSON string-array parser — handles the output of the above.
fn parse_json_string_array(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut chars = text.chars().peekable();
    // Walk through, collecting quoted strings
    while let Some(c) = chars.next() {
        if c == '"' {
            let mut s = String::new();
            loop {
                match chars.next() {
                    Some('\\') => {
                        match chars.next() {
                            Some('"')  => s.push('"'),
                            Some('\\') => s.push('\\'),
                            Some('n')  => s.push('\n'),
                            Some(other) => { s.push('\\'); s.push(other); }
                            None => break,
                        }
                    }
                    Some('"') => break,
                    Some(ch) => s.push(ch),
                    None => break,
                }
            }
            if !s.is_empty() {
                items.push(s);
            }
        }
    }
    items
}

impl eframe::App for SvgViewerApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // -- Poll background URL fetch --
        if let Some(rx) = &self.url_fetch_rx {
            if let Ok(result) = rx.try_recv() {
                self.url_fetch_rx = None;
                self.url_loading = false;
                match result {
                    Ok(svg_text) => {
                        let label = self.url_input.clone();
                        self.load_from_svg_text(svg_text, label, None);
                        self.url_modal_open = false;
                    }
                    Err(e) => {
                        self.url_error = Some(e);
                    }
                }
            }
        }

        // -- URL modal --
        if self.url_modal_open {
            egui::Window::new("Open from URL")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .fixed_size([480.0, 140.0])
                .show(ctx, |ui| {
                    ui.add_space(8.0);
                    ui.label("Enter the URL of an SVG file:");
                    ui.add_space(4.0);

                    let submitted = ui.add_sized(
                        [ui.available_width(), 24.0],
                        egui::TextEdit::singleline(&mut self.url_input)
                            .hint_text("https://example.com/file.svg")
                            .desired_width(f32::INFINITY),
                    ).lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));

                    ui.add_space(6.0);

                    if let Some(err) = &self.url_error {
                        ui.colored_label(egui::Color32::RED, err);
                        ui.add_space(4.0);
                    }

                    ui.horizontal(|ui| {
                        let fetch_clicked = ui.add_enabled(
                            !self.url_loading && !self.url_input.trim().is_empty(),
                            egui::Button::new(if self.url_loading { "Loading..." } else { "Open" }),
                        ).clicked();

                        if (fetch_clicked || submitted) && !self.url_loading {
                            let url = self.url_input.trim().to_string();
                            if !url.is_empty() {
                                self.start_url_fetch(url, ctx.clone());
                            }
                        }

                        if ui.button("Cancel").clicked() {
                            self.url_modal_open = false;
                            self.url_loading = false;
                            self.url_fetch_rx = None;
                            self.url_error = None;
                        }
                    });
                });
        }

        // -- Top menu bar --
        TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open SVG...").clicked() {
                        ui.close_menu();
                        self.open_file_dialog();
                    }
                    if ui.button("Open from URL...").clicked() {
                        ui.close_menu();
                        self.url_modal_open = true;
                        self.url_error = None;
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
        let prev_spacebar = self.spacebar_held;
        ctx.input(|i| {
            self.spacebar_held = i.key_down(Key::Space);
        });
        // When spacebar is held, consume Tab before egui can use it for focus traversal.
        // Capture whether it was pressed this frame, then consume it unconditionally.
        let tab_pressed_this_frame = self.spacebar_held && ctx.input(|i| i.key_pressed(Key::Tab));
        if self.spacebar_held {
            ctx.input_mut(|i| { i.consume_key(egui::Modifiers::NONE, Key::Tab); });
        }
        // When spacebar is released, clear any transient hover-highlight
        if prev_spacebar && !self.spacebar_held && self.highlight_transient {
            self.highlighted = None;
            self.highlight_transient = false;
        }

        // -- Active pane: resolve pointer-down before panels are drawn --
        // We read the pointer position and check which pane rect it falls in.
        // last_viewport holds the viewer rect from the previous frame (good enough for
        // one-frame lag — pane rects don't move between frames in practice).
        // The elements panel occupies the right side; everything else is the viewer.
        self.activation_consumed = false;
        let primary_pressed = ctx.input(|i| i.pointer.primary_pressed());
        if primary_pressed {
            if let Some(pos) = ctx.input(|i| i.pointer.press_origin()) {
                // Determine which pane the click landed in.
                // We use last_viewport (viewer rect). If the click is outside it, it's elements.
                let in_viewer = self.last_viewport.map(|r| r.contains(pos)).unwrap_or(false);
                let target = if in_viewer { ActivePane::Viewer } else { ActivePane::Elements };
                if target != self.active_pane {
                    self.active_pane = target;
                    self.activation_consumed = true;
                    // Clear transient state when switching panes
                    if self.highlight_transient {
                        self.highlighted = None;
                        self.highlight_transient = false;
                    }
                    self.tab_candidates.clear();
                    self.tab_index = 0;
                    self.tab_cursor_pos = None;
                }
            }
        }

        // -- Drop file support --
        ctx.input(|i| {
            if let Some(dropped) = i.raw.dropped_files.first() {
                if let Some(path) = &dropped.path {
                    self.load_file(path.clone());
                }
            }
        });

        // -- Right: elements pane --
        let elements_response = SidePanel::right("elements_panel")
            .resizable(true)
            .default_width(400.0)
            .min_width(200.0)
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    ui.add_space(4.0);
                    ui.heading("Elements");
                    ui.separator();

                    if let Some(doc) = &self.doc {
                        // We need a mutable borrow of elements_pane and immutable doc.
                        // Temporarily take the doc out to satisfy borrow checker.
                        let doc_ref = doc as *const SvgDocument;
                        let doc_ref = unsafe { &*doc_ref };

                        let (clicked, hovered) = self.elements_pane.show(ui, doc_ref);

                        // Update group bbox highlight based on what's hovered (no spacebar needed)
                        self.group_highlight_bbox = None;
                        if let Some(h) = hovered {
                            let is_group = matches!(
                                doc_ref.get(h).kind,
                                SvgNodeKind::Group | SvgNodeKind::Svg { .. }
                                    | SvgNodeKind::Defs | SvgNodeKind::ClipPath { .. }
                                    | SvgNodeKind::Mask { .. }
                            );
                            if is_group {
                                self.group_highlight_bbox = self.spatial_index
                                    .as_ref()
                                    .and_then(|idx| idx.bbox_for_subtree(doc_ref, h));
                            }
                        }

                        // Spacebar + hover → highlight leaf in viewer (only when Elements is active)
                        if self.spacebar_held && self.active_pane == ActivePane::Elements {
                            if let Some(h) = hovered {
                                let is_leaf = matches!(doc_ref.get(h).kind, SvgNodeKind::Shape(_));
                                if is_leaf {
                                    self.highlighted = Some(h);
                                    self.highlight_transient = true;
                                }
                            }
                        }

                        // Click → highlight + zoom (suppressed on activation frame)
                        if !self.activation_consumed {
                            if let Some(clicked_id) = clicked {
                                let is_group = matches!(
                                    doc_ref.get(clicked_id).kind,
                                    SvgNodeKind::Group | SvgNodeKind::Svg { .. }
                                        | SvgNodeKind::Defs | SvgNodeKind::ClipPath { .. }
                                        | SvgNodeKind::Mask { .. }
                                );
                                if is_group {
                                    if let Some(idx) = &self.spatial_index {
                                        if let Some(bbox) = idx.bbox_for_subtree(doc_ref, clicked_id) {
                                            self.group_highlight_bbox = Some(bbox);
                                            self.zoom_to_bbox(bbox);
                                        }
                                    }
                                } else {
                                    self.highlighted = Some(clicked_id);
                                    self.highlight_transient = false;
                                    self.group_highlight_bbox = None;
                                    if let Some(idx) = &self.spatial_index {
                                        if let Some(bbox) = idx.bbox_for_node(clicked_id) {
                                            self.zoom_to_bbox(bbox);
                                        }
                                    }
                                }
                            }
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

        // Draw active-pane outline on the elements panel
        if self.active_pane == ActivePane::Elements {
            let outline_color = Color32::from_rgb(30, 120, 255);
            ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("elements_outline")))
                .rect_stroke(elements_response.response.rect, egui::CornerRadius::ZERO,
                    egui::Stroke::new(2.0, outline_color), StrokeKind::Inside);
        }

        // -- Left: SVG viewer --
        CentralPanel::default().show(ctx, |ui| {
            let viewport = ui.max_rect();
            self.last_viewport = Some(viewport);

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
                        ui.add_space(8.0);
                        if ui.button("Open from URL...").clicked() {
                            self.url_modal_open = true;
                            self.url_error = None;
                        }

                        // Recent items
                        if !self.recent_items.is_empty() {
                            ui.add_space(32.0);
                            ui.label(egui::RichText::new("Recent").weak());
                            ui.add_space(6.0);

                            let recents = self.recent_items.clone();
                            let mut open_item: Option<String> = None;

                            for item in &recents {
                                let is_url = item.starts_with("http://") || item.starts_with("https://");
                                // Display label: for files show just the filename; for URLs show the full URL
                                let file_name_buf;
                                let display = if is_url {
                                    item.as_str()
                                } else {
                                    file_name_buf = PathBuf::from(item);
                                    file_name_buf
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or(item.as_str())
                                };
                                // Truncate long strings
                                let display = if display.len() > 60 {
                                    format!("…{}", &display[display.len() - 57..])
                                } else {
                                    display.to_string()
                                };

                                let icon = if is_url { "🌐 " } else { "📄 " };
                                let response = ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!("{icon}{display}"))
                                            .monospace()
                                            .size(12.0)
                                    )
                                    .sense(egui::Sense::click())
                                );
                                if response.hovered() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                let response = if !is_url {
                                    response.on_hover_text(item.as_str())
                                } else {
                                    response
                                };
                                if response.clicked() {
                                    open_item = Some(item.clone());
                                }
                            }

                            if let Some(item) = open_item {
                                let is_url = item.starts_with("http://") || item.starts_with("https://");
                                if is_url {
                                    self.url_input = item.clone();
                                    self.start_url_fetch(item, ctx.clone());
                                } else {
                                    self.load_file(PathBuf::from(item));
                                }
                            }
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

            // -- Spacebar hover → hit-test (FR-5) + TAB cycling (only when Viewer is active) --
            if self.spacebar_held && self.active_pane == ActivePane::Viewer && response.hovered() {
                let tab_pressed = tab_pressed_this_frame;

                if let Some(hover_pos) = ui.input(|i| i.pointer.hover_pos()) {
                    if let Some(doc) = &self.doc {
                        let (sx, sy) = self.view.screen_to_svg(hover_pos);

                        // Rebuild candidate list if cursor moved significantly
                        let cursor_moved = self.tab_cursor_pos
                            .map(|(px, py)| {
                                let dx = sx - px;
                                let dy = sy - py;
                                // Threshold: 2 SVG units, scaled by view
                                (dx * dx + dy * dy) > (2.0 / self.view.scale).powi(2)
                            })
                            .unwrap_or(true);

                        if cursor_moved {
                            let doc_ptr = doc as *const SvgDocument;
                            let doc_ref = unsafe { &*doc_ptr };
                            self.tab_candidates = self
                                .spatial_index
                                .as_ref()
                                .map(|idx| idx.hit_test_all(doc_ref, sx, sy))
                                .unwrap_or_default();
                            self.tab_index = 0;
                            self.tab_cursor_pos = Some((sx, sy));
                        }

                        // TAB advances the cycle index
                        if tab_pressed && !self.tab_candidates.is_empty() {
                            self.tab_index = (self.tab_index + 1) % self.tab_candidates.len();
                        }

                        if let Some(&hit) = self.tab_candidates.get(self.tab_index) {
                            if self.highlighted != Some(hit) {
                                self.highlighted = Some(hit);
                                self.highlight_transient = true;
                                let doc_ptr = doc as *const SvgDocument;
                                let doc_ref = unsafe { &*doc_ptr };
                                self.elements_pane.select_and_scroll(hit, doc_ref);
                            }
                        } else {
                            // Nothing under cursor — clear transient highlight
                            if self.highlight_transient {
                                self.highlighted = None;
                            }
                            self.tab_cursor_pos = None;
                        }
                    }
                }
            } else if !self.spacebar_held {
                // Spacebar released — reset TAB state
                self.tab_candidates.clear();
                self.tab_index = 0;
                self.tab_cursor_pos = None;
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
                    group_highlight_bbox: self.group_highlight_bbox,
                    textures: &self.textures,
                    cache: &self.geometry_cache,
                    clips: &self.clip_index,
                };
                renderer::render(&render_ctx);
            }

            // Cursor hint
            if self.spacebar_held && self.active_pane == ActivePane::Viewer {
                ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
            }

            // Draw active-pane outline on viewer
            if self.active_pane == ActivePane::Viewer {
                let outline_color = Color32::from_rgb(30, 120, 255);
                ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("viewer_outline")))
                    .rect_stroke(rect, egui::CornerRadius::ZERO,
                        egui::Stroke::new(2.0, outline_color), StrokeKind::Inside);
            }
        });
    }
}


