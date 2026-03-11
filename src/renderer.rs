/// SVG renderer — walks the SvgDocument tree and emits egui paint shapes.
///
/// Performance design:
///   - Paths are tessellated once in SVG local space and cached in GeometryCache.
///   - On each frame, cached vertices are transformed by (world_transform × ViewTransform)
///     and submitted to egui. No re-tessellation on zoom/pan.
///   - Simple shapes (rect, circle) use egui primitives directly — no tessellation needed.
///   - The highlighted element is re-drawn last with a vivid solid colour.

use egui::{Color32, Painter, Pos2, Rect, Stroke, TextureHandle};
use egui::epaint::StrokeKind;
use std::collections::HashMap;
use crate::clip_index::ClipIndex;
use lyon::geom::point;
use lyon::math::Point;
use lyon::path::Path as LyonPath;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, StrokeOptions,
    StrokeTessellator, StrokeVertex, VertexBuffers,
};

use crate::svg_doc::*;

// ---------------------------------------------------------------------------
// View transform
// ---------------------------------------------------------------------------

/// Screen-space transform: maps SVG user units → egui pixels.
#[derive(Clone, Debug)]
pub struct ViewTransform {
    pub offset: egui::Vec2,
    pub scale: f32,
}

impl ViewTransform {
    pub fn fit(svg_width: f32, svg_height: f32, viewport: Rect) -> Self {
        let sx = viewport.width() / svg_width;
        let sy = viewport.height() / svg_height;
        let scale = sx.min(sy) * 0.95;
        let offset = egui::Vec2::new(
            viewport.left() + (viewport.width() - svg_width * scale) / 2.0,
            viewport.top() + (viewport.height() - svg_height * scale) / 2.0,
        );
        ViewTransform { offset, scale }
    }

    #[inline]
    pub fn svg_to_screen(&self, x: f32, y: f32) -> Pos2 {
        Pos2::new(self.offset.x + x * self.scale, self.offset.y + y * self.scale)
    }

    #[inline]
    pub fn screen_to_svg(&self, p: Pos2) -> (f32, f32) {
        ((p.x - self.offset.x) / self.scale, (p.y - self.offset.y) / self.scale)
    }

    #[inline]
    pub fn length(&self, v: f32) -> f32 {
        v * self.scale
    }

    /// Transform a point from SVG local space to screen space,
    /// applying the world transform first.
    #[inline]
    pub fn world_to_screen(&self, world: &Transform, x: f32, y: f32) -> Pos2 {
        let (wx, wy) = world.apply(x, y);
        self.svg_to_screen(wx, wy)
    }

    /// Apply ViewTransform to an already-world-transformed SVG point.
    #[inline]
    pub fn apply_to_pos(&self, x: f32, y: f32) -> Pos2 {
        Pos2::new(self.offset.x + x * self.scale, self.offset.y + y * self.scale)
    }
}

// ---------------------------------------------------------------------------
// Geometry cache — tessellations in SVG local space
// ---------------------------------------------------------------------------

/// Raw tessellation output in SVG local coordinates.
/// Stored per NodeId, invalidated on document reload.
#[derive(Clone)]
pub struct CachedGeometry {
    /// Fill triangles. Vertices are in SVG local space.
    pub fill: Option<RawMesh>,
    /// Stroke triangles. Vertices are in SVG local space.
    /// Tessellated at unit scale (stroke_width = SVG units).
    pub stroke: Option<RawMesh>,
}

#[derive(Clone)]
pub struct RawMesh {
    pub vertices: Vec<[f32; 2]>, // local SVG coords
    pub indices: Vec<u32>,
    /// Conservative local-space AABB of the vertices (min_x, min_y, max_x, max_y).
    pub local_bounds: [f32; 4],
}

impl RawMesh {
    fn compute_bounds(vertices: &[[f32; 2]]) -> [f32; 4] {
        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for &[x, y] in vertices {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        [min_x, min_y, max_x, max_y]
    }

    /// Check if this mesh's screen-space AABB intersects the viewport.
    pub fn intersects_viewport(&self, world: &Transform, vt: &ViewTransform, viewport: Rect) -> bool {
        let [min_x, min_y, max_x, max_y] = self.local_bounds;
        if min_x.is_infinite() { return false; }
        // Transform the 4 corners of the local AABB to screen space
        let corners = [(min_x, min_y), (max_x, min_y), (min_x, max_y), (max_x, max_y)];
        let mut smin_x = f32::INFINITY;
        let mut smin_y = f32::INFINITY;
        let mut smax_x = f32::NEG_INFINITY;
        let mut smax_y = f32::NEG_INFINITY;
        for &(x, y) in &corners {
            let p = vt.world_to_screen(world, x, y);
            smin_x = smin_x.min(p.x);
            smin_y = smin_y.min(p.y);
            smax_x = smax_x.max(p.x);
            smax_y = smax_y.max(p.y);
        }
        let screen_rect = Rect::from_min_max(Pos2::new(smin_x, smin_y), Pos2::new(smax_x, smax_y));
        viewport.intersects(screen_rect)
    }

    /// Emit this mesh to egui, applying the combined world+view transform
    /// and tinting with `color`.
    pub fn emit(&self, painter: &Painter, world: &Transform, vt: &ViewTransform, color: Color32) {
        if self.vertices.is_empty() {
            return;
        }
        let verts: Vec<egui::epaint::Vertex> = self
            .vertices
            .iter()
            .map(|&[x, y]| {
                let (wx, wy) = world.apply(x, y);
                egui::epaint::Vertex {
                    pos: vt.apply_to_pos(wx, wy),
                    uv: egui::epaint::WHITE_UV,
                    color,
                }
            })
            .collect();
        painter.add(egui::Shape::mesh(egui::Mesh {
            vertices: verts,
            indices: self.indices.clone(),
            texture_id: egui::TextureId::default(),
        }));
    }
}

/// Cache of tessellated geometry keyed by NodeId.
/// Lives in SvgViewerApp and is rebuilt when the document changes.
pub struct GeometryCache {
    pub meshes: HashMap<NodeId, CachedGeometry>,
}

impl GeometryCache {
    pub fn new() -> Self {
        GeometryCache { meshes: HashMap::new() }
    }

    /// Populate the cache by walking the document tree.
    /// Call once after parsing; zoom/pan do not invalidate it.
    pub fn build(doc: &SvgDocument) -> Self {
        let mut cache = GeometryCache::new();
        cache.populate(doc, doc.root, &Transform::identity());
        cache
    }

    fn populate(&mut self, doc: &SvgDocument, node_id: NodeId, parent_tf: &Transform) {
        let node = doc.get(node_id);
        let world = parent_tf.concat(&node.transform);

        match &node.kind {
            SvgNodeKind::Svg { .. } | SvgNodeKind::Group => {
                for &child in &node.children {
                    self.populate(doc, child, &world);
                }
            }
            SvgNodeKind::Shape(SvgShape::Path { data }) => {
                let geom = tessellate_path_local(data, &node.style);
                self.meshes.insert(node_id, geom);
            }
            SvgNodeKind::Shape(SvgShape::Ellipse { cx, cy, rx, ry }) => {
                let geom = tessellate_ellipse_local(*cx, *cy, *rx, *ry, &node.style);
                self.meshes.insert(node_id, geom);
            }
            SvgNodeKind::Shape(SvgShape::Polygon { points }) => {
                let geom = tessellate_polygon_local(points, &node.style);
                self.meshes.insert(node_id, geom);
            }
            SvgNodeKind::Shape(SvgShape::Polyline { points }) => {
                let geom = tessellate_polyline_local(points, &node.style);
                self.meshes.insert(node_id, geom);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Render context
// ---------------------------------------------------------------------------

pub struct RenderContext<'a> {
    pub doc: &'a SvgDocument,
    pub vt: &'a ViewTransform,
    pub painter: &'a Painter,
    /// The visible screen rect — used for viewport culling.
    pub viewport: Rect,
    pub highlight: Option<NodeId>,
    /// Group bbox highlight — drawn as a semi-transparent rect over the viewer.
    /// `[min_x, min_y, max_x, max_y]` in SVG world space.
    pub group_highlight_bbox: Option<[f32; 4]>,
    pub textures: &'a HashMap<NodeId, TextureHandle>,
    pub cache: &'a GeometryCache,
    pub clips: &'a ClipIndex,
}

// ---------------------------------------------------------------------------
// Main render entry
// ---------------------------------------------------------------------------

pub fn render(ctx: &RenderContext) {
    render_node(ctx, ctx.doc.root, &Transform::identity());

    // Draw highlighted element last — on top of everything
    if let Some(h) = ctx.highlight {
        render_highlight(ctx, h);
    }

    // Draw group bbox highlight (from elements pane hover/click on a <g>)
    if let Some([min_x, min_y, max_x, max_y]) = ctx.group_highlight_bbox {
        // Transform the two corners through the identity world (bbox is already world-space)
        let id = Transform::identity();
        let tl = ctx.vt.world_to_screen(&id, min_x, min_y);
        let br = ctx.vt.world_to_screen(&id, max_x, max_y);
        let rect = Rect::from_two_pos(tl, br);
        let fill = Color32::from_rgba_unmultiplied(30, 140, 255, 35);
        let stroke_color = Color32::from_rgba_unmultiplied(30, 140, 255, 200);
        ctx.painter.rect(rect, egui::CornerRadius::ZERO, fill,
            Stroke::new(1.5, stroke_color), StrokeKind::Outside);
    }
}

fn render_node(ctx: &RenderContext, node_id: NodeId, parent_tf: &Transform) {
    let node = ctx.doc.get(node_id);
    let world = parent_tf.concat(&node.transform);

    // If this node references a clipPath, restrict the painter to its screen-space AABB.
    // We build a clipped painter and a derived RenderContext that uses it.
    // The clip AABB is in local SVG space (clipPathUnits=userSpaceOnUse default),
    // so we transform it using this node's accumulated world transform.
    let clipped_painter;
    let clipped_ctx;
    let ctx: &RenderContext = if let Some(clip_id) = &node.clip_path {
        if let Some(local_bb) = ctx.clips.get(clip_id) {
            let [lx0, ly0, lx1, ly1] = local_bb;
            // Transform all four corners of the clip AABB to screen space.
            let corners = [
                ctx.vt.world_to_screen(&world, lx0, ly0),
                ctx.vt.world_to_screen(&world, lx1, ly0),
                ctx.vt.world_to_screen(&world, lx0, ly1),
                ctx.vt.world_to_screen(&world, lx1, ly1),
            ];
            let min_x = corners.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
            let min_y = corners.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
            let max_x = corners.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max);
            let max_y = corners.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max);
            let clip_screen = Rect::from_min_max(
                egui::pos2(min_x, min_y),
                egui::pos2(max_x, max_y),
            ).intersect(ctx.viewport);

            clipped_painter = ctx.painter.with_clip_rect(clip_screen);
            clipped_ctx = RenderContext {
                doc: ctx.doc,
                vt: ctx.vt,
                painter: &clipped_painter,
                viewport: clip_screen,
                highlight: ctx.highlight,
                group_highlight_bbox: ctx.group_highlight_bbox,
                textures: ctx.textures,
                cache: ctx.cache,
                clips: ctx.clips,
            };
            &clipped_ctx
        } else {
            ctx
        }
    } else {
        ctx
    };

    match &node.kind {
        SvgNodeKind::Svg { .. } | SvgNodeKind::Group => {
            for &child in &node.children {
                render_node(ctx, child, &world);
            }
        }
        SvgNodeKind::Shape(shape) => {
            // Viewport cull: skip shapes whose screen-space AABB doesn't
            // intersect the visible viewport. This is the critical optimisation
            // for high zoom levels where most elements are off-screen.
            if !shape_screen_aabb_intersects_viewport(shape, &world, ctx) {
                return;
            }
            render_shape(ctx, node, shape, &world);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Viewport culling
// ---------------------------------------------------------------------------

/// Returns false if the shape is entirely outside the visible viewport.
/// Uses a conservative (over-approximate) screen-space AABB.
/// A false negative (returning true for an off-screen shape) is safe — it
/// just means we draw it anyway. A false positive (culling a visible shape)
/// would be a rendering bug, so we err on the side of inclusion.
fn shape_screen_aabb_intersects_viewport(
    shape: &SvgShape,
    world: &Transform,
    ctx: &RenderContext,
) -> bool {
    let vp = ctx.viewport;
    let vt = ctx.vt;

    // Convert a set of SVG local-space corners to a screen-space Rect.
    let corners_to_screen_rect = |corners: &[(f32, f32)]| -> Rect {
        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for &(x, y) in corners {
            let p = vt.world_to_screen(world, x, y);
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
        Rect::from_min_max(Pos2::new(min_x, min_y), Pos2::new(max_x, max_y))
    };

    let shape_rect = match shape {
        SvgShape::Rect { x, y, width, height, .. } => {
            corners_to_screen_rect(&[
                (*x, *y), (x + width, *y),
                (*x, y + height), (x + width, y + height),
            ])
        }
        SvgShape::Circle { cx, cy, r } => {
            let center = vt.world_to_screen(world, *cx, *cy);
            let radius = r * transform_max_scale(world) * vt.scale;
            Rect::from_center_size(center, egui::Vec2::splat(radius * 2.0))
        }
        SvgShape::Ellipse { cx, cy, rx, ry } => {
            corners_to_screen_rect(&[
                (cx - rx, cy - ry), (cx + rx, cy - ry),
                (cx - rx, cy + ry), (cx + rx, cy + ry),
            ])
        }
        SvgShape::Line { x1, y1, x2, y2 } => {
            corners_to_screen_rect(&[(*x1, *y1), (*x2, *y2)])
        }
        SvgShape::Polyline { points } | SvgShape::Polygon { points } => {
            if points.is_empty() { return false; }
            corners_to_screen_rect(points)
        }
        SvgShape::Path { .. } => {
            // Path culling is handled per-mesh inside render_shape using
            // RawMesh::intersects_viewport. Return true here so render_shape runs.
            return true;
        }
        SvgShape::Text { x, y, spans, font_size } => {
            let total_chars: usize = spans.iter().map(|s| s.content.len()).sum();
            let w = total_chars as f32 * font_size * 0.7;
            corners_to_screen_rect(&[
                (*x, y - font_size), (x + w, y - font_size),
                (*x, *y), (x + w, *y),
            ])
        }
        SvgShape::Image { x, y, width, height, .. } => {
            corners_to_screen_rect(&[
                (*x, *y), (x + width, *y),
                (*x, y + height), (x + width, y + height),
            ])
        }
        SvgShape::Use { .. } => return false,
    };

    vp.intersects(shape_rect)
}

// ---------------------------------------------------------------------------
// Shape rendering
// ---------------------------------------------------------------------------

fn resolve_paint(paint: &Paint, opacity: f32) -> Option<Color32> {
    match paint {
        Paint::None => None,
        Paint::Color(c) => {
            let a = (c.a as f32 * opacity).round() as u8;
            Some(Color32::from_rgba_unmultiplied(c.r, c.g, c.b, a))
        }
        Paint::LinearGradient(_) | Paint::RadialGradient(_) => {
            Some(Color32::from_rgba_unmultiplied(150, 150, 150, (255.0 * opacity) as u8))
        }
    }
}

fn render_shape(ctx: &RenderContext, node: &SvgNode, shape: &SvgShape, world: &Transform) {
    let style = &node.style;
    let opacity = style.opacity;
    let fill_color = resolve_paint(&style.fill, style.fill_opacity * opacity);
    let stroke_color = resolve_paint(&style.stroke, style.stroke_opacity * opacity);
    let stroke_w_svg = style.stroke_width; // in SVG units

    match shape {
        SvgShape::Rect { x, y, width, height, rx, ry } => {
            let tl = ctx.vt.world_to_screen(world, *x, *y);
            let br = ctx.vt.world_to_screen(world, x + width, y + height);
            let rect = Rect::from_two_pos(tl, br);
            let rounding = egui::CornerRadius::same((rx.max(*ry) * ctx.vt.scale) as u8);
            if let Some(fill) = fill_color {
                ctx.painter.rect_filled(rect, rounding, fill);
            }
            if let Some(sc) = stroke_color {
                let sw = (stroke_w_svg * ctx.vt.scale).max(0.5);
                ctx.painter.rect_stroke(rect, rounding, Stroke::new(sw, sc), StrokeKind::Middle);
            }
        }

        SvgShape::Circle { cx, cy, r } => {
            let center = ctx.vt.world_to_screen(world, *cx, *cy);
            let radius = r * transform_max_scale(world) * ctx.vt.scale;
            if let Some(fill) = fill_color {
                ctx.painter.circle_filled(center, radius, fill);
            }
            if let Some(sc) = stroke_color {
                let sw = (stroke_w_svg * ctx.vt.scale).max(0.5);
                ctx.painter.circle_stroke(center, radius, Stroke::new(sw, sc));
            }
        }

        SvgShape::Ellipse { .. } | SvgShape::Polyline { .. }
        | SvgShape::Polygon { .. } | SvgShape::Path { .. } => {
            if let Some(geom) = ctx.cache.meshes.get(&node.id) {
                // Use fill mesh bounds for the viewport cull (fill and stroke share
                // the same geometry extent, so checking one is enough).
                let visible = geom.fill.as_ref()
                    .map(|m| m.intersects_viewport(world, ctx.vt, ctx.viewport))
                    .or_else(|| geom.stroke.as_ref()
                        .map(|m| m.intersects_viewport(world, ctx.vt, ctx.viewport)))
                    .unwrap_or(false);

                if !visible { return; }

                if let Some(fill) = fill_color {
                    if let Some(m) = &geom.fill { m.emit(ctx.painter, world, ctx.vt, fill); }
                }
                if let Some(sc) = stroke_color {
                    if let Some(m) = &geom.stroke { m.emit(ctx.painter, world, ctx.vt, sc); }
                }
            }
        }

        SvgShape::Line { x1, y1, x2, y2 } => {
            let p1 = ctx.vt.world_to_screen(world, *x1, *y1);
            let p2 = ctx.vt.world_to_screen(world, *x2, *y2);
            let c = stroke_color.or(fill_color).unwrap_or(Color32::BLACK);
            let sw = (stroke_w_svg * ctx.vt.scale).max(1.0);
            ctx.painter.line_segment([p1, p2], Stroke::new(sw, c));
        }

        SvgShape::Text { x, y, spans, font_size } => {
            render_text_spans(ctx, node, *x, *y, *font_size, spans, world, fill_color);
        }

        SvgShape::Image { x, y, width, height, .. } => {
            let tl = ctx.vt.world_to_screen(world, *x, *y);
            let br = ctx.vt.world_to_screen(world, x + width, y + height);
            let rect = Rect::from_two_pos(tl, br);
            if let Some(tex) = ctx.textures.get(&node.id) {
                ctx.painter.image(
                    tex.id(),
                    rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            } else {
                ctx.painter.rect_filled(rect, egui::CornerRadius::ZERO,
                    Color32::from_rgba_unmultiplied(180, 180, 180, 60));
                ctx.painter.rect_stroke(rect, egui::CornerRadius::ZERO,
                    Stroke::new(1.0, Color32::from_gray(140)), StrokeKind::Middle);
                ctx.painter.line_segment([rect.left_top(), rect.right_bottom()],
                    Stroke::new(1.0, Color32::from_gray(160)));
                ctx.painter.line_segment([rect.right_top(), rect.left_bottom()],
                    Stroke::new(1.0, Color32::from_gray(160)));
            }
        }

        SvgShape::Use { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Highlight — re-render the element in a smart contrasting colour
// ---------------------------------------------------------------------------

/// Candidate highlight colours: magenta and cyan.
/// We pick whichever has the highest perceptual contrast against the element.
const MAGENTA_FILL:   Color32 = Color32::from_rgba_premultiplied(220, 0,   220, 150);
const MAGENTA_STROKE: Color32 = Color32::from_rgba_premultiplied(255, 0,   255, 255);
const CYAN_FILL:      Color32 = Color32::from_rgba_premultiplied(0,   200, 220, 150);
const CYAN_STROKE:    Color32 = Color32::from_rgba_premultiplied(0,   240, 255, 255);

pub fn highlight_color() -> Color32 { MAGENTA_STROKE }

/// Perceptual relative luminance (sRGB, ITU-R BT.709).
fn luminance(c: Color32) -> f32 {
    let lin = |v: u8| -> f32 {
        let s = v as f32 / 255.0;
        if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// Choose magenta or cyan based on which contrasts more with the element's colours.
fn pick_highlight(node: &SvgNode) -> (Color32, Color32) {
    // Collect all opaque-ish colours from fill and stroke
    let mut samples: Vec<Color32> = Vec::new();

    let push_paint = |paint: &Paint, samples: &mut Vec<Color32>| {
        if let Paint::Color(c) = paint {
            if c.a > 20 {
                samples.push(Color32::from_rgb(c.r, c.g, c.b));
            }
        }
    };
    push_paint(&node.style.fill, &mut samples);
    push_paint(&node.style.stroke, &mut samples);

    // Average luminance of the element's colours (or 0.5 if none)
    let element_lum = if samples.is_empty() {
        0.5
    } else {
        samples.iter().map(|&c| luminance(c)).sum::<f32>() / samples.len() as f32
    };

    // Magenta luminance ≈ 0.28, cyan luminance ≈ 0.60
    // Pick whichever is further from the element luminance
    let mag_contrast = (element_lum - 0.28_f32).abs();
    let cyn_contrast = (element_lum - 0.60_f32).abs();

    if cyn_contrast > mag_contrast {
        (CYAN_FILL, CYAN_STROKE)
    } else {
        (MAGENTA_FILL, MAGENTA_STROKE)
    }
}

fn render_highlight(ctx: &RenderContext, node_id: NodeId) {
    let node = ctx.doc.get(node_id);
    let world = compute_world_transform(ctx.doc, node_id);

    let shape = match &node.kind {
        SvgNodeKind::Shape(s) => s,
        _ => return,
    };

    let (hl_fill, hl_stroke) = pick_highlight(node);

    match shape {
        SvgShape::Rect { x, y, width, height, rx, ry } => {
            let tl = ctx.vt.world_to_screen(&world, *x, *y);
            let br = ctx.vt.world_to_screen(&world, x + width, y + height);
            let rect = Rect::from_two_pos(tl, br);
            let rounding = egui::CornerRadius::same((rx.max(*ry) * ctx.vt.scale) as u8);
            ctx.painter.rect(rect, rounding, hl_fill, Stroke::new(2.0, hl_stroke), StrokeKind::Outside);
        }
        SvgShape::Circle { cx, cy, r } => {
            let center = ctx.vt.world_to_screen(&world, *cx, *cy);
            let radius = r * transform_max_scale(&world) * ctx.vt.scale;
            ctx.painter.circle(center, radius, hl_fill, Stroke::new(2.0, hl_stroke));
        }
        SvgShape::Ellipse { .. } | SvgShape::Path { .. }
        | SvgShape::Polygon { .. } | SvgShape::Polyline { .. } => {
            if let Some(geom) = ctx.cache.meshes.get(&node_id) {
                if let Some(m) = &geom.fill {
                    m.emit(ctx.painter, &world, ctx.vt, hl_fill);
                }
                if let Some(m) = &geom.stroke {
                    m.emit(ctx.painter, &world, ctx.vt, hl_stroke);
                }
                // Stroke-only shapes with no cached fill mesh: draw the stroke in highlight colour
                if geom.fill.is_none() && geom.stroke.is_none() {
                    // nothing tessellated (e.g. empty path) — nothing to draw
                }
            }
        }
        SvgShape::Line { x1, y1, x2, y2 } => {
            let p1 = ctx.vt.world_to_screen(&world, *x1, *y1);
            let p2 = ctx.vt.world_to_screen(&world, *x2, *y2);
            ctx.painter.line_segment([p1, p2], Stroke::new(3.0, hl_stroke));
        }
        SvgShape::Text { x, y, spans, font_size } => {
            render_text_spans(ctx, node, *x, *y, *font_size, spans, &world, Some(hl_stroke));
        }
        SvgShape::Image { x, y, width, height, .. } => {
            let tl = ctx.vt.world_to_screen(&world, *x, *y);
            let br = ctx.vt.world_to_screen(&world, x + width, y + height);
            let rect = Rect::from_two_pos(tl, br);
            ctx.painter.rect(rect, egui::CornerRadius::ZERO, hl_fill,
                Stroke::new(2.0, hl_stroke), StrokeKind::Outside);
        }
        SvgShape::Use { .. } => {}
    }
}

/// Walk ancestors to rebuild the world transform for a given node.
fn compute_world_transform(doc: &SvgDocument, node_id: NodeId) -> Transform {
    // Collect ancestor chain
    let mut chain = Vec::new();
    let mut cur = node_id;
    loop {
        let node = doc.get(cur);
        chain.push(node.transform.clone());
        match node.parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    // Apply from root → node
    let mut world = Transform::identity();
    for t in chain.iter().rev() {
        world = world.concat(t);
    }
    world
}

// ---------------------------------------------------------------------------
// Text rendering
// ---------------------------------------------------------------------------

fn render_text_spans(
    ctx: &RenderContext,
    node: &SvgNode,
    base_x: f32,
    base_y: f32,
    base_font_size: f32,
    spans: &[TextSpan],
    world: &Transform,
    default_fill: Option<Color32>,
) {
    let mut cursor_x = base_x;
    let mut cursor_y = base_y;

    for span in spans {
        let font_size = span.font_size.unwrap_or(base_font_size);
        let color = span
            .fill
            .as_ref()
            .and_then(|p| resolve_paint(p, node.style.opacity))
            .or(default_fill)
            .unwrap_or(Color32::BLACK);

        let sx = span.x.unwrap_or(cursor_x) + span.dx;
        let sy = span.y.unwrap_or(cursor_y) + span.dy;

        let pos = ctx.vt.world_to_screen(world, sx, sy);
        // Font size must account for both the viewport zoom and any scale baked into
        // the element's world transform (e.g. transform="scale(2)").
        let [a, b, c, d, ..] = world.matrix;
        let world_scale = ((a * a + b * b).sqrt()).max((c * c + d * d).sqrt());
        let screen_size = (font_size * world_scale * ctx.vt.scale).max(4.0);
        let font_id = egui::FontId::proportional(screen_size);

        let galley = ctx.painter.layout_no_wrap(span.content.clone(), font_id, color);
        let advance_px = galley.size().x;
        // SVG `y` is the text baseline, but egui galley pos is the top-left corner.
        // Approximate ascent as 85% of line height (matches spatial_index hitbox formula)
        // and offset draw position upward so the baseline lands at `pos.y`.
        let ascent = galley.size().y * 0.85;
        let draw_pos = egui::pos2(pos.x, pos.y - ascent);
        ctx.painter.galley(draw_pos, galley, color);

        // Advance cursor in SVG units using the actual rendered width
        let advance_svg = advance_px / ctx.vt.scale;
        cursor_x = sx + advance_svg;
        cursor_y = sy;
    }
}

// ---------------------------------------------------------------------------
// Local-space tessellation (for cache)
// ---------------------------------------------------------------------------

/// Tessellate a path `d` string in SVG local coordinates (no ViewTransform).
fn tessellate_path_local(data: &str, style: &Style) -> CachedGeometry {
    let lyon_path = match parse_svg_path_local(data) {
        Some(p) => p,
        None => return CachedGeometry { fill: None, stroke: None },
    };

    let fill = if !matches!(style.fill, Paint::None) {
        tessellate_fill_raw(&lyon_path)
    } else {
        None
    };

    let stroke = if !matches!(style.stroke, Paint::None) && style.stroke_width > 0.0 {
        tessellate_stroke_raw(&lyon_path, style.stroke_width)
    } else {
        None
    };

    CachedGeometry { fill, stroke }
}

fn tessellate_ellipse_local(cx: f32, cy: f32, rx: f32, ry: f32, style: &Style) -> CachedGeometry {
    const K: f32 = 0.5522847498;
    let mut builder = LyonPath::builder();
    builder.begin(point(cx + rx, cy));
    builder.cubic_bezier_to(point(cx + rx, cy - K * ry), point(cx + K * rx, cy - ry), point(cx, cy - ry));
    builder.cubic_bezier_to(point(cx - K * rx, cy - ry), point(cx - rx, cy - K * ry), point(cx - rx, cy));
    builder.cubic_bezier_to(point(cx - rx, cy + K * ry), point(cx - K * rx, cy + ry), point(cx, cy + ry));
    builder.cubic_bezier_to(point(cx + K * rx, cy + ry), point(cx + rx, cy + K * ry), point(cx + rx, cy));
    builder.end(true);
    let path = builder.build();

    let fill = if !matches!(style.fill, Paint::None) { tessellate_fill_raw(&path) } else { None };
    let stroke = if !matches!(style.stroke, Paint::None) && style.stroke_width > 0.0 {
        tessellate_stroke_raw(&path, style.stroke_width)
    } else { None };

    CachedGeometry { fill, stroke }
}

fn tessellate_polygon_local(points: &[(f32, f32)], style: &Style) -> CachedGeometry {
    if points.len() < 2 { return CachedGeometry { fill: None, stroke: None }; }
    let mut builder = LyonPath::builder();
    builder.begin(point(points[0].0, points[0].1));
    for &(x, y) in &points[1..] { builder.line_to(point(x, y)); }
    builder.end(true);
    let path = builder.build();

    let fill = if !matches!(style.fill, Paint::None) { tessellate_fill_raw(&path) } else { None };
    let stroke = if !matches!(style.stroke, Paint::None) && style.stroke_width > 0.0 {
        tessellate_stroke_raw(&path, style.stroke_width)
    } else { None };
    CachedGeometry { fill, stroke }
}

fn tessellate_polyline_local(points: &[(f32, f32)], style: &Style) -> CachedGeometry {
    if points.len() < 2 { return CachedGeometry { fill: None, stroke: None }; }
    let mut builder = LyonPath::builder();
    builder.begin(point(points[0].0, points[0].1));
    for &(x, y) in &points[1..] { builder.line_to(point(x, y)); }
    builder.end(false);
    let path = builder.build();

    let stroke = if !matches!(style.stroke, Paint::None) && style.stroke_width > 0.0 {
        tessellate_stroke_raw(&path, style.stroke_width)
    } else { None };
    CachedGeometry { fill: None, stroke }
}

// ---------------------------------------------------------------------------
// Lyon helpers — raw (local-space) tessellation
// ---------------------------------------------------------------------------

fn tessellate_fill_raw(path: &LyonPath) -> Option<RawMesh> {
    let mut buffers: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let mut tess = FillTessellator::new();
    tess.tessellate_path(
        path.as_slice(),
        &FillOptions::default(),
        &mut BuffersBuilder::new(&mut buffers, |v: FillVertex| {
            [v.position().x, v.position().y]
        }),
    ).ok()?;
    let bounds = RawMesh::compute_bounds(&buffers.vertices);
    Some(RawMesh { vertices: buffers.vertices, indices: buffers.indices, local_bounds: bounds })
}

fn tessellate_stroke_raw(path: &LyonPath, width: f32) -> Option<RawMesh> {
    let mut buffers: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let mut tess = StrokeTessellator::new();
    tess.tessellate_path(
        path.as_slice(),
        &StrokeOptions::default().with_line_width(width),
        &mut BuffersBuilder::new(&mut buffers, |v: StrokeVertex| {
            [v.position().x, v.position().y]
        }),
    ).ok()?;
    let bounds = RawMesh::compute_bounds(&buffers.vertices);
    Some(RawMesh { vertices: buffers.vertices, indices: buffers.indices, local_bounds: bounds })
}

// ---------------------------------------------------------------------------
// SVG path parser — local space (no ViewTransform)
// ---------------------------------------------------------------------------

fn parse_svg_path_local(data: &str) -> Option<LyonPath> {
    let mut builder = LyonPath::builder();
    let mut current = Point::new(0.0, 0.0);
    let mut start = Point::new(0.0, 0.0);
    let mut last_ctrl: Option<Point> = None;
    let mut in_subpath = false;

    let tokens = tokenize_path(data);
    let mut i = 0;

    let abs = |x, y| point(x, y);
    let rel = |x, y, cur: Point| point(cur.x + x, cur.y + y);

    while i < tokens.len() {
        let cmd = match &tokens[i] {
            PathToken::Cmd(c) => { i += 1; *c }
            PathToken::Num(_) => if in_subpath { 'L' } else { 'M' },
        };

        match cmd {
            'M' | 'm' => {
                let r = cmd == 'm';
                let x = next_num(&tokens, &mut i)?;
                let y = next_num(&tokens, &mut i)?;
                let p = if r { rel(x, y, current) } else { abs(x, y) };
                if in_subpath { builder.end(false); }
                builder.begin(p);
                current = p; start = p; in_subpath = true; last_ctrl = None;
                while peek_num(&tokens, i) {
                    let x2 = next_num(&tokens, &mut i)?;
                    let y2 = next_num(&tokens, &mut i)?;
                    let p2 = if r { rel(x2, y2, current) } else { abs(x2, y2) };
                    builder.line_to(p2); current = p2; last_ctrl = None;
                }
            }
            'L' | 'l' => {
                let r = cmd == 'l';
                while peek_num(&tokens, i) {
                    let x = next_num(&tokens, &mut i)?;
                    let y = next_num(&tokens, &mut i)?;
                    let p = if r { rel(x, y, current) } else { abs(x, y) };
                    if !in_subpath { builder.begin(p); in_subpath = true; start = p; }
                    else { builder.line_to(p); }
                    current = p; last_ctrl = None;
                }
            }
            'H' | 'h' => {
                let r = cmd == 'h';
                while peek_num(&tokens, i) {
                    let x = next_num(&tokens, &mut i)?;
                    let p = point(if r { current.x + x } else { x }, current.y);
                    builder.line_to(p); current = p; last_ctrl = None;
                }
            }
            'V' | 'v' => {
                let r = cmd == 'v';
                while peek_num(&tokens, i) {
                    let y = next_num(&tokens, &mut i)?;
                    let p = point(current.x, if r { current.y + y } else { y });
                    builder.line_to(p); current = p; last_ctrl = None;
                }
            }
            'C' | 'c' => {
                let r = cmd == 'c';
                while peek_num(&tokens, i) {
                    let (x1,y1,x2,y2,x,y) = (
                        next_num(&tokens,&mut i)?, next_num(&tokens,&mut i)?,
                        next_num(&tokens,&mut i)?, next_num(&tokens,&mut i)?,
                        next_num(&tokens,&mut i)?, next_num(&tokens,&mut i)?,
                    );
                    let (cp1,cp2,ep) = if r {
                        (rel(x1,y1,current), rel(x2,y2,current), rel(x,y,current))
                    } else { (abs(x1,y1), abs(x2,y2), abs(x,y)) };
                    builder.cubic_bezier_to(cp1, cp2, ep);
                    last_ctrl = Some(cp2); current = ep;
                }
            }
            'S' | 's' => {
                let r = cmd == 's';
                while peek_num(&tokens, i) {
                    let (x2,y2,x,y) = (
                        next_num(&tokens,&mut i)?, next_num(&tokens,&mut i)?,
                        next_num(&tokens,&mut i)?, next_num(&tokens,&mut i)?,
                    );
                    let (cp2,ep) = if r { (rel(x2,y2,current), rel(x,y,current)) }
                                   else { (abs(x2,y2), abs(x,y)) };
                    let cp1 = match last_ctrl {
                        Some(lc) => point(2.0*current.x - lc.x, 2.0*current.y - lc.y),
                        None => current,
                    };
                    builder.cubic_bezier_to(cp1, cp2, ep);
                    last_ctrl = Some(cp2); current = ep;
                }
            }
            'Q' | 'q' => {
                let r = cmd == 'q';
                while peek_num(&tokens, i) {
                    let (x1,y1,x,y) = (
                        next_num(&tokens,&mut i)?, next_num(&tokens,&mut i)?,
                        next_num(&tokens,&mut i)?, next_num(&tokens,&mut i)?,
                    );
                    let (cp,ep) = if r { (rel(x1,y1,current), rel(x,y,current)) }
                                  else { (abs(x1,y1), abs(x,y)) };
                    builder.quadratic_bezier_to(cp, ep);
                    last_ctrl = Some(cp); current = ep;
                }
            }
            'T' | 't' => {
                let r = cmd == 't';
                while peek_num(&tokens, i) {
                    let (x,y) = (next_num(&tokens,&mut i)?, next_num(&tokens,&mut i)?);
                    let ep = if r { rel(x,y,current) } else { abs(x,y) };
                    let cp = match last_ctrl {
                        Some(lc) => point(2.0*current.x - lc.x, 2.0*current.y - lc.y),
                        None => current,
                    };
                    builder.quadratic_bezier_to(cp, ep);
                    last_ctrl = Some(cp); current = ep;
                }
            }
            'A' | 'a' => {
                let r = cmd == 'a';
                while peek_num(&tokens, i) {
                    let _rx = next_num(&tokens,&mut i)?;
                    let _ry = next_num(&tokens,&mut i)?;
                    let _xr = next_num(&tokens,&mut i)?;
                    let _la = next_num(&tokens,&mut i)?;
                    let _sw = next_num(&tokens,&mut i)?;
                    let x  = next_num(&tokens,&mut i)?;
                    let y  = next_num(&tokens,&mut i)?;
                    let ep = if r { rel(x,y,current) } else { abs(x,y) };
                    builder.line_to(ep);
                    current = ep; last_ctrl = None;
                }
            }
            'Z' | 'z' => {
                builder.end(true);
                current = start; in_subpath = false; last_ctrl = None;
            }
            _ => {}
        }
    }
    if in_subpath { builder.end(false); }
    Some(builder.build())
}

// ---------------------------------------------------------------------------
// Path tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum PathToken { Cmd(char), Num(f32) }

fn tokenize_path(data: &str) -> Vec<PathToken> {
    let mut tokens = Vec::new();
    let mut chars = data.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\n' | '\r' | ',' => { chars.next(); }
            'A'..='Z' | 'a'..='z' => { tokens.push(PathToken::Cmd(c)); chars.next(); }
            '0'..='9' | '-' | '+' | '.' => {
                let mut s = String::new();
                if c == '-' || c == '+' { s.push(c); chars.next(); }
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() { s.push(d); chars.next(); } else { break; }
                }
                if let Some(&'.') = chars.peek() {
                    s.push('.'); chars.next();
                    while let Some(&d) = chars.peek() {
                        if d.is_ascii_digit() { s.push(d); chars.next(); } else { break; }
                    }
                }
                if let Some(&e) = chars.peek() {
                    if e == 'e' || e == 'E' {
                        s.push(e); chars.next();
                        if let Some(&sg) = chars.peek() {
                            if sg == '-' || sg == '+' { s.push(sg); chars.next(); }
                        }
                        while let Some(&d) = chars.peek() {
                            if d.is_ascii_digit() { s.push(d); chars.next(); } else { break; }
                        }
                    }
                }
                if let Ok(n) = s.parse::<f32>() { tokens.push(PathToken::Num(n)); }
            }
            _ => { chars.next(); }
        }
    }
    tokens
}

fn next_num(tokens: &[PathToken], i: &mut usize) -> Option<f32> {
    if let Some(PathToken::Num(n)) = tokens.get(*i) { *i += 1; Some(*n) } else { None }
}
fn peek_num(tokens: &[PathToken], i: usize) -> bool {
    matches!(tokens.get(i), Some(PathToken::Num(_)))
}

// ---------------------------------------------------------------------------
// Transform helpers
// ---------------------------------------------------------------------------

fn transform_max_scale(t: &Transform) -> f32 {
    let [a, b, c, d, ..] = t.matrix;
    let sx = (a * a + b * b).sqrt();
    let sy = (c * c + d * d).sqrt();
    sx.max(sy)
}
