/// Top-level egui application.
/// Manages the split-pane layout, file loading, keyboard state,
/// and coordinates between the renderer, spatial index, and elements pane.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

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

/// Progress state shared between the background loader and the UI thread.
#[derive(Clone)]
struct LoadProgress {
    /// Human-readable description of the current phase
    phase: String,
    /// 0.0..1.0 for determinate progress, negative for indeterminate
    fraction: f32,
}

impl LoadProgress {
    fn indeterminate(phase: impl Into<String>) -> Self {
        Self { phase: phase.into(), fraction: -1.0 }
    }
    fn determinate(phase: impl Into<String>, fraction: f32) -> Self {
        Self { phase: phase.into(), fraction: fraction.clamp(0.0, 1.0) }
    }
}

type SharedProgress = Arc<Mutex<LoadProgress>>;

/// Everything produced by the background loader thread.
struct LoadedDocument {
    doc: SvgDocument,
    spatial_index: SpatialIndex,
    geometry_cache: GeometryCache,
    clip_index: ClipIndex,
    label: String,
    svg_dir: Option<PathBuf>,
}

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

    // --- Loading (file or URL, always on background thread) ---
    /// Whether the "Open from URL" modal is open
    url_modal_open: bool,
    /// Current text in the URL input field
    url_input: String,
    /// Receives the result of a background load
    load_rx: Option<Receiver<Result<LoadedDocument, String>>>,
    /// True while a background load is in flight — shows the loading overlay
    is_loading: bool,
    /// Shared progress state, updated by the background thread
    load_progress: SharedProgress,
    /// Error from the last failed load
    load_error: Option<String>,
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
            load_rx: None,
            is_loading: false,
            load_progress: Arc::new(Mutex::new(LoadProgress::indeterminate("Idle"))),
            load_error: None,
        }
    }

    /// Kick off a background file read + parse.  Shows the loading overlay immediately.
    fn load_file(&mut self, path: PathBuf, egui_ctx: Context) {
        let label = path.to_string_lossy().into_owned();
        let short = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        self.start_load(format!("Opening {short}…"), egui_ctx, move |progress, ctx| {
            // Phase 1: Read the file with progress
            set_progress_and_repaint(&progress, LoadProgress::indeterminate("Reading file…"), &ctx);
            let file_len = std::fs::metadata(&path).ok().map(|m| m.len());
            let svg_dir = path.parent().map(|p| p.to_path_buf());

            let contents = if let Some(total) = file_len {
                let mut file = std::fs::File::open(&path)
                    .map_err(|e| format!("Read error: {e}"))?;
                read_with_progress(&mut file, total as usize, "Reading file…", &progress, &ctx)?
            } else {
                std::fs::read_to_string(&path)
                    .map_err(|e| format!("Read error: {e}"))?
            };

            // Phases 2-5: parse and build indexes
            parse_and_build(contents, label, svg_dir, &progress, &ctx)
        });
    }

    /// Kick off a background HTTP fetch.  Shows the loading overlay immediately.
    fn start_url_fetch(&mut self, url: String, egui_ctx: Context) {
        let label = url.clone();
        self.url_modal_open = false;
        self.start_load(format!("Downloading…"), egui_ctx, move |progress, ctx| {
            // Phase 1: Download with progress
            set_progress_and_repaint(&progress, LoadProgress::indeterminate("Connecting…"), &ctx);
            let resp = ureq::get(&url)
                .set("User-Agent", "svg-viewer/1.0")
                .call()
                .map_err(|e| format!("Request failed: {e}"))?;

            let content_length: Option<usize> = resp.header("Content-Length")
                .and_then(|s| s.parse().ok());

            let contents = if let Some(total) = content_length {
                read_with_progress(&mut resp.into_reader(), total, "Downloading…", &progress, &ctx)?
            } else {
                set_progress_and_repaint(&progress, LoadProgress::indeterminate("Downloading…"), &ctx);
                let mut buf = String::new();
                resp.into_reader()
                    .read_to_string(&mut buf)
                    .map_err(|e| format!("Read error: {e}"))?;
                buf
            };

            // Phases 2-5: parse and build indexes
            parse_and_build(contents, label, None, &progress, &ctx)
        });
    }

    /// Generic background loader.  `work` runs on a thread and receives shared progress + context.
    fn start_load<F>(&mut self, description: String, egui_ctx: Context, work: F)
    where
        F: FnOnce(SharedProgress, Context) -> Result<LoadedDocument, String> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        self.load_rx = Some(rx);
        self.is_loading = true;
        self.load_error = None;

        let progress = Arc::new(Mutex::new(LoadProgress::indeterminate(&description)));
        self.load_progress = progress.clone();

        let ctx2 = egui_ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(work(progress, ctx2.clone()));
            ctx2.request_repaint();
        });
    }

    /// Install a fully-loaded document produced by the background thread.
    fn install_loaded_document(&mut self, loaded: LoadedDocument) {
        self.textures.clear();
        self.spatial_index = Some(loaded.spatial_index);
        self.geometry_cache = loaded.geometry_cache;
        self.clip_index = loaded.clip_index;
        self.doc = Some(loaded.doc);
        self.error = None;
        self.view_fitted = false;
        self.highlighted = None;
        self.highlight_transient = false;
        self.group_highlight_bbox = None;
        self.tab_candidates.clear();
        self.tab_index = 0;
        self.tab_cursor_pos = None;
        self.elements_pane = ElementsPane::new();
        self.file_path = Some(PathBuf::from(loaded.label.clone()));
        self.push_recent(loaded.label);
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

    fn open_file_dialog(&mut self, egui_ctx: Context) {
        // rfd is synchronous on macOS, runs its own event loop
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("SVG files", &["svg"])
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.load_file(path, egui_ctx);
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

// ---------------------------------------------------------------------------
// Background-thread helpers
// ---------------------------------------------------------------------------

fn set_progress(progress: &SharedProgress, p: LoadProgress) {
    if let Ok(mut guard) = progress.lock() {
        *guard = p;
    }
}

/// Helper: set progress and request a repaint so the UI updates promptly.
fn set_progress_and_repaint(progress: &SharedProgress, p: LoadProgress, ctx: &Context) {
    set_progress(progress, p);
    ctx.request_repaint();
}

/// Format a byte progress like "3.2 / 14.1 MB".
fn format_bytes_progress(done: usize, total: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let d = done as f64;
    let t = total as f64;
    if t >= MB {
        format!("{:.1} / {:.1} MB", d / MB, t / MB)
    } else if t >= KB {
        format!("{:.0} / {:.0} KB", d / KB, t / KB)
    } else {
        format!("{done} / {total} B")
    }
}

/// Read from `reader` in chunks, building a String, reporting progress.
fn read_with_progress(
    reader: &mut dyn Read,
    total: usize,
    phase: &str,
    progress: &SharedProgress,
    ctx: &Context,
) -> Result<String, String> {
    let mut buf = Vec::with_capacity(total);
    let mut chunk = [0u8; 64 * 1024];
    let mut last_repaint = std::time::Instant::now();
    loop {
        let n = reader.read(&mut chunk).map_err(|e| format!("Read error: {e}"))?;
        if n == 0 { break; }
        buf.extend_from_slice(&chunk[..n]);
        let frac = if total > 0 { buf.len() as f32 / total as f32 } else { -1.0 };
        let size_label = format_bytes_progress(buf.len(), total);
        set_progress(progress, LoadProgress::determinate(
            format!("{phase} ({size_label})"), frac,
        ));
        // Throttle repaints to ~30fps to avoid overwhelming the UI
        if last_repaint.elapsed().as_millis() > 33 {
            ctx.request_repaint();
            last_repaint = std::time::Instant::now();
        }
    }
    ctx.request_repaint();
    String::from_utf8(buf).map_err(|e| format!("UTF-8 error: {e}"))
}

/// Parse SVG text and build all indexes, reporting progress phases.
fn parse_and_build(
    contents: String,
    label: String,
    svg_dir: Option<PathBuf>,
    progress: &SharedProgress,
    ctx: &Context,
) -> Result<LoadedDocument, String> {
    // Phase: Parse
    set_progress_and_repaint(progress, LoadProgress::indeterminate("Parsing SVG…"), ctx);
    let mut doc = parser::parse_svg(&contents).map_err(|e| {
        let chain: Vec<String> = e.chain().map(|c| c.to_string()).collect();
        format!("Parse error: {}", chain.join("\n  caused by: "))
    })?;

    // Phase: Resolve external images
    set_progress_and_repaint(progress, LoadProgress::indeterminate("Resolving images…"), ctx);
    parser::resolve_external_images(&mut doc, svg_dir.as_deref());

    // Phase: Build spatial index
    set_progress_and_repaint(progress, LoadProgress::indeterminate("Building spatial index…"), ctx);
    let spatial_index = SpatialIndex::build(&doc);

    // Phase: Build geometry cache
    set_progress_and_repaint(progress, LoadProgress::indeterminate("Tessellating geometry…"), ctx);
    let geometry_cache = GeometryCache::build(&doc);

    // Phase: Build clip index
    set_progress_and_repaint(progress, LoadProgress::indeterminate("Building clip index…"), ctx);
    let clip_index = ClipIndex::build(&doc);

    log::info!("Loaded \"{label}\": {} nodes", doc.nodes.len());
    set_progress_and_repaint(progress, LoadProgress::determinate("Done", 1.0), ctx);

    Ok(LoadedDocument { doc, spatial_index, geometry_cache, clip_index, label, svg_dir })
}

impl eframe::App for SvgViewerApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // -- Poll background load (file or URL) --
        if let Some(rx) = &self.load_rx {
            if let Ok(result) = rx.try_recv() {
                self.load_rx = None;
                self.is_loading = false;
                match result {
                    Ok(loaded) => {
                        self.install_loaded_document(loaded);
                    }
                    Err(e) => {
                        self.load_error = Some(e);
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

                    if let Some(err) = &self.load_error.clone() {
                        ui.colored_label(egui::Color32::RED, err);
                        ui.add_space(4.0);
                    }

                    ui.horizontal(|ui| {
                        let open_clicked = ui.add_enabled(
                            !self.url_input.trim().is_empty(),
                            egui::Button::new("Open"),
                        ).clicked();

                        if open_clicked || submitted {
                            let url = self.url_input.trim().to_string();
                            if !url.is_empty() {
                                self.start_url_fetch(url, ctx.clone());
                            }
                        }

                        if ui.button("Cancel").clicked() {
                            self.url_modal_open = false;
                            self.load_error = None;
                        }
                    });
                });
        }

        // -- Top toolbar --
        TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(4.0);

                // Open buttons
                if ui.button("Open…").clicked() {
                    self.open_file_dialog(ctx.clone());
                }
                if ui.button("Open URL…").clicked() {
                    self.url_modal_open = true;
                    self.load_error = None;
                }
                if self.doc.is_some() {
                    if ui.button("Fit").clicked() {
                        self.view_fitted = false;
                    }
                }

                ui.separator();

                // File / URL label + stats
                if let Some(path) = &self.file_path.clone() {
                    let name = path.file_name()
                        .unwrap_or_else(|| path.as_os_str())
                        .to_string_lossy();
                    ui.label(egui::RichText::new(name.as_ref()).strong());

                    if let Some(doc) = &self.doc {
                        let w = doc.width as u32;
                        let h = doc.height as u32;
                        let n = doc.nodes.len();

                        ui.separator();
                        ui.label(egui::RichText::new(format!("{w} × {h}")).monospace());
                        ui.separator();

                        // Element count with hover breakdown
                        let count_label = ui.label(
                            egui::RichText::new(format!("{n} elements")).monospace()
                        );
                        if count_label.hovered() {
                            // Build sorted tag frequency table
                            let mut counts: std::collections::HashMap<&str, usize> =
                                std::collections::HashMap::new();
                            for node in &doc.nodes {
                                *counts.entry(node.tag_name.as_str()).or_insert(0) += 1;
                            }
                            let mut sorted: Vec<(&str, usize)> = counts.into_iter().collect();
                            sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

                            egui::show_tooltip_at_pointer(ctx, ui.layer_id(), egui::Id::new("elem_breakdown"), |ui| {
                                ui.set_min_width(160.0);
                                for (tag, count) in &sorted {
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(format!("{count}")).monospace().strong());
                                        ui.label(egui::RichText::new(format!("<{tag}>")).monospace().weak());
                                    });
                                }
                            });
                        }
                    }
                }

                // Right-aligned inspect mode indicator
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(4.0);
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

        // While spacebar is held, consume Space so egui can't fire focused buttons with it,
        // and clear widget focus so nothing can be accidentally activated.
        let cycle_pressed_this_frame = self.spacebar_held && ctx.input(|i| i.key_pressed(Key::W));
        if self.spacebar_held {
            ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, Key::Space);
            });
            // Drop widget focus so Space can't activate menus or buttons
            if let Some(id) = ctx.memory(|m| m.focused()) {
                ctx.memory_mut(|m| m.surrender_focus(id));
            }
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
        let dropped_path = ctx.input(|i| {
            i.raw.dropped_files.first()
                .and_then(|f| f.path.clone())
        });
        if let Some(path) = dropped_path {
            self.load_file(path, ctx.clone());
        }

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

            // -- Loading overlay (file read / URL fetch in progress) --
            if self.is_loading {
                let progress = self.load_progress.lock()
                    .map(|g| g.clone())
                    .unwrap_or_else(|_| LoadProgress::indeterminate("Loading…"));

                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);

                        if progress.fraction >= 0.0 {
                            // Determinate progress bar
                            let bar = egui::ProgressBar::new(progress.fraction)
                                .show_percentage();
                            ui.add_sized([300.0, 20.0], bar);
                        } else {
                            // Indeterminate spinner
                            ui.add(egui::Spinner::new().size(40.0));
                        }

                        ui.add_space(16.0);
                        ui.label(egui::RichText::new(&progress.phase).size(16.0));
                    });
                });
                // Keep repainting while loading so the progress updates
                ctx.request_repaint();
                return;
            }

            if let Some(error) = &self.error.clone() {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new(error).color(Color32::RED));
                        ui.add_space(8.0);
                        if ui.button("Dismiss").clicked() {
                            self.error = None;
                        }
                    });
                });
                return;
            }

            if let Some(err) = &self.load_error.clone() {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new(err).color(Color32::RED));
                        ui.add_space(8.0);
                        if ui.button("Dismiss").clicked() {
                            self.load_error = None;
                        }
                    });
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
                        ui.label(egui::RichText::new("Drop an SVG file here, or use the toolbar buttons above").weak());
                        ui.add_space(24.0);
                        if ui.button("Open SVG...").clicked() {
                            self.open_file_dialog(ctx.clone());
                        }
                        ui.add_space(8.0);
                        if ui.button("Open from URL...").clicked() {
                            self.url_modal_open = true;
                            self.load_error = None;
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
                                    self.load_file(PathBuf::from(item), ctx.clone());
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
                let tab_pressed = cycle_pressed_this_frame;

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
                                .map(|idx| idx.hit_test_all(doc_ref, sx, sy, self.view.scale))
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
                    vertices_emitted: std::rc::Rc::new(std::cell::Cell::new(0)),
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


