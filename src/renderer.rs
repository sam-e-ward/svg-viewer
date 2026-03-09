/// SVG renderer — walks the SvgDocument tree and emits egui paint shapes.
/// This is the M3 basic renderer: handles rects, circles, ellipses,
/// lines, polylines, polygons and paths (via lyon tessellation).

use egui::{Color32, Painter, Pos2, Rect, Stroke, TextureHandle, Vec2};
use egui::epaint::StrokeKind;
use std::collections::HashMap;
use lyon::geom::point;
use lyon::math::Point;
use lyon::path::Path as LyonPath;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, StrokeOptions,
    StrokeTessellator, StrokeVertex, VertexBuffers,
};

use crate::svg_doc::*;

/// Screen-space transform: maps SVG user units → egui pixels.
/// Encapsulates the current pan/zoom state.
#[derive(Clone, Debug)]
pub struct ViewTransform {
    /// Top-left of the SVG canvas in screen pixels
    pub offset: egui::Vec2,
    /// Pixels per SVG user unit
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

    pub fn svg_to_screen(&self, x: f32, y: f32) -> Pos2 {
        Pos2::new(
            self.offset.x + x * self.scale,
            self.offset.y + y * self.scale,
        )
    }

    pub fn screen_to_svg(&self, p: Pos2) -> (f32, f32) {
        (
            (p.x - self.offset.x) / self.scale,
            (p.y - self.offset.y) / self.scale,
        )
    }

    pub fn length(&self, v: f32) -> f32 {
        v * self.scale
    }

    pub fn rect(&self, x: f32, y: f32, w: f32, h: f32) -> Rect {
        let tl = self.svg_to_screen(x, y);
        Rect::from_min_size(tl, Vec2::new(w * self.scale, h * self.scale))
    }
}

// ---------------------------------------------------------------------------
// Top-level render call
// ---------------------------------------------------------------------------

pub struct RenderContext<'a> {
    pub doc: &'a SvgDocument,
    pub vt: &'a ViewTransform,
    pub painter: &'a Painter,
    /// NodeId of the highlighted element (None if none)
    pub highlight: Option<NodeId>,
    /// Pre-uploaded image textures keyed by NodeId
    pub textures: &'a HashMap<NodeId, TextureHandle>,
}

pub fn render(ctx: &RenderContext) {
    // Walk from root
    let root = ctx.doc.root;
    render_node(ctx, root, &Transform::identity());
}

fn resolve_paint(paint: &Paint, opacity: f32) -> Option<Color32> {
    match paint {
        Paint::None => None,
        Paint::Color(c) => {
            let a = (c.a as f32 * opacity).round() as u8;
            Some(Color32::from_rgba_unmultiplied(c.r, c.g, c.b, a))
        }
        // Gradient stubs — fall back to a mid-gray
        Paint::LinearGradient(_) | Paint::RadialGradient(_) => {
            Some(Color32::from_rgba_unmultiplied(150, 150, 150, (255.0 * opacity) as u8))
        }
    }
}

fn render_node(ctx: &RenderContext, node_id: NodeId, parent_transform: &Transform) {
    let node = ctx.doc.get(node_id);

    // Combine transforms
    let combined = parent_transform.concat(&node.transform);

    let is_highlight = ctx.highlight == Some(node_id);

    match &node.kind {
        SvgNodeKind::Svg { .. } | SvgNodeKind::Group => {
            for &child in &node.children {
                render_node(ctx, child, &combined);
            }
        }
        SvgNodeKind::Defs
        | SvgNodeKind::ClipPath { .. }
        | SvgNodeKind::Mask { .. }
        | SvgNodeKind::LinearGradient { .. }
        | SvgNodeKind::RadialGradient { .. }
        | SvgNodeKind::Unknown { .. } => {
            // Not rendered directly
        }
        SvgNodeKind::Shape(shape) => {
            render_shape(ctx, node, shape, &combined, is_highlight);
        }
    }
}

fn render_shape(
    ctx: &RenderContext,
    node: &SvgNode,
    shape: &SvgShape,
    transform: &Transform,
    highlight: bool,
) {
    let style = &node.style;
    let effective_opacity = style.opacity;

    let fill_color = resolve_paint(&style.fill, style.fill_opacity * effective_opacity);
    let stroke_color = resolve_paint(&style.stroke, style.stroke_opacity * effective_opacity);
    let stroke_width = ctx.vt.length(style.stroke_width).max(0.5);

    let stroke = match stroke_color {
        Some(c) => Stroke::new(stroke_width, c),
        None => Stroke::NONE,
    };

    match shape {
        SvgShape::Rect { x, y, width, height, rx, ry } => {
            let (tx, ty) = apply_transform(transform, *x, *y);
            // Scale width/height by the transform scale (approximate for non-uniform)
            let sw = *width * transform_scale_x(transform) * ctx.vt.scale;
            let sh = *height * transform_scale_y(transform) * ctx.vt.scale;
            let screen_tl = ctx.vt.svg_to_screen(tx, ty);
            // Correct for already-applied scale factor in svg_to_screen
            let screen_tl_raw = Pos2::new(
                ctx.vt.offset.x + tx * ctx.vt.scale,
                ctx.vt.offset.y + ty * ctx.vt.scale,
            );
            let rect = Rect::from_min_size(screen_tl_raw, Vec2::new(sw, sh));
            let rounding = egui::CornerRadius::same(
                ((*rx).max(*ry) * ctx.vt.scale) as u8,
            );

            if let Some(fill) = fill_color {
                ctx.painter.rect_filled(rect, rounding, fill);
            }
            if stroke.width > 0.0 {
                ctx.painter.rect_stroke(rect, rounding, stroke, StrokeKind::Middle);
            }
            if highlight {
                draw_highlight_rect(ctx, rect);
            }
        }

        SvgShape::Circle { cx, cy, r } => {
            let (tx, ty) = apply_transform(transform, *cx, *cy);
            let center = ctx.vt.svg_to_screen(tx, ty);
            let radius = r * transform_scale_x(transform) * ctx.vt.scale;

            if let Some(fill) = fill_color {
                ctx.painter.circle_filled(center, radius, fill);
            }
            if stroke.width > 0.0 {
                ctx.painter.circle_stroke(center, radius, stroke);
            }
            if highlight {
                draw_highlight_circle(ctx, center, radius);
            }
        }

        SvgShape::Ellipse { cx, cy, rx, ry } => {
            let (tx, ty) = apply_transform(transform, *cx, *cy);
            let center = ctx.vt.svg_to_screen(tx, ty);
            let screen_rx = rx * transform_scale_x(transform) * ctx.vt.scale;
            let screen_ry = ry * transform_scale_y(transform) * ctx.vt.scale;
            render_ellipse(ctx, center, screen_rx, screen_ry, fill_color, stroke, highlight);
        }

        SvgShape::Line { x1, y1, x2, y2 } => {
            let (tx1, ty1) = apply_transform(transform, *x1, *y1);
            let (tx2, ty2) = apply_transform(transform, *x2, *y2);
            let p1 = ctx.vt.svg_to_screen(tx1, ty1);
            let p2 = ctx.vt.svg_to_screen(tx2, ty2);
            let eff_stroke = if stroke.width > 0.0 {
                stroke
            } else if let Some(fc) = fill_color {
                Stroke::new(1.0, fc)
            } else {
                return;
            };
            ctx.painter.line_segment([p1, p2], eff_stroke);
            if highlight {
                ctx.painter.line_segment(
                    [p1, p2],
                    Stroke::new(eff_stroke.width + 2.0, highlight_color()),
                );
            }
        }

        SvgShape::Polyline { points } | SvgShape::Polygon { points } => {
            if points.len() < 2 {
                return;
            }
            let screen_pts: Vec<Pos2> = points
                .iter()
                .map(|(px, py)| {
                    let (tx, ty) = apply_transform(transform, *px, *py);
                    ctx.vt.svg_to_screen(tx, ty)
                })
                .collect();

            let is_polygon = matches!(shape, SvgShape::Polygon { .. });

            if let Some(fill) = fill_color {
                if is_polygon && screen_pts.len() >= 3 {
                    let mesh = polygon_to_mesh(&screen_pts, fill);
                    ctx.painter.add(mesh);
                }
            }

            // Draw outline
            let out_stroke = if stroke.width > 0.0 { stroke } else { Stroke::NONE };
            if out_stroke.width > 0.0 {
                let mut pts = screen_pts.clone();
                if is_polygon {
                    pts.push(pts[0]);
                }
                for w in pts.windows(2) {
                    ctx.painter.line_segment([w[0], w[1]], out_stroke);
                }
            }

            if highlight {
                let mut pts = screen_pts.clone();
                if is_polygon {
                    pts.push(pts[0]);
                }
                for w in pts.windows(2) {
                    ctx.painter.line_segment(
                        [w[0], w[1]],
                        Stroke::new(2.0, highlight_color()),
                    );
                }
            }
        }

        SvgShape::Path { data } => {
            render_path(
                ctx,
                data,
                transform,
                fill_color,
                stroke,
                highlight,
            );
        }

        SvgShape::Text { x, y, spans, font_size } => {
            render_text(ctx, node, *x, *y, *font_size, spans, transform, fill_color, highlight);
        }

        SvgShape::Image { x, y, width, height, .. } => {
            let (tx, ty) = apply_transform(transform, *x, *y);
            let sw = width * transform_scale_x(transform) * ctx.vt.scale;
            let sh = height * transform_scale_y(transform) * ctx.vt.scale;
            let rect = Rect::from_min_size(
                Pos2::new(ctx.vt.offset.x + tx * ctx.vt.scale, ctx.vt.offset.y + ty * ctx.vt.scale),
                Vec2::new(sw, sh),
            );

            if let Some(tex) = ctx.textures.get(&node.id) {
                ctx.painter.image(
                    tex.id(),
                    rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            } else {
                // No decoded texture — draw a hatched placeholder
                ctx.painter.rect_filled(
                    rect,
                    egui::CornerRadius::ZERO,
                    Color32::from_rgba_unmultiplied(180, 180, 180, 60),
                );
                ctx.painter.rect_stroke(
                    rect,
                    egui::CornerRadius::ZERO,
                    Stroke::new(1.0, Color32::from_gray(140)),
                    StrokeKind::Middle,
                );
                // Cross lines to indicate "image missing"
                ctx.painter.line_segment([rect.left_top(), rect.right_bottom()], Stroke::new(1.0, Color32::from_gray(160)));
                ctx.painter.line_segment([rect.right_top(), rect.left_bottom()], Stroke::new(1.0, Color32::from_gray(160)));
            }

            if highlight {
                draw_highlight_rect(ctx, rect);
            }
        }

        SvgShape::Use { .. } => {
            // Resolved separately in a later milestone; skip for now
        }
    }
}

// ---------------------------------------------------------------------------
// Text rendering
// ---------------------------------------------------------------------------

fn render_text(
    ctx: &RenderContext,
    node: &SvgNode,
    base_x: f32,
    base_y: f32,
    base_font_size: f32,
    spans: &[TextSpan],
    transform: &Transform,
    default_fill: Option<Color32>,
    highlight: bool,
) {
    // We track a running cursor in SVG units for relative positioning
    let mut cursor_x = base_x;
    let mut cursor_y = base_y;

    for span in spans {
        let font_size = span.font_size.unwrap_or(base_font_size);

        // Resolve fill: span override → node default → black
        let color = span
            .fill
            .as_ref()
            .and_then(|p| resolve_paint(p, node.style.opacity))
            .or(default_fill)
            .unwrap_or(Color32::BLACK);

        // Absolute position overrides cursor; relative is additive
        let sx = span.x.unwrap_or(cursor_x) + span.dx;
        let sy = span.y.unwrap_or(cursor_y) + span.dy;

        let (tx, ty) = apply_transform(transform, sx, sy);
        let pos = ctx.vt.svg_to_screen(tx, ty);
        let screen_size = (font_size * ctx.vt.scale).max(4.0);

        let font_id = match (&span.font_weight, &span.font_style) {
            (FontWeight::Bold, FontStyle::Italic) => egui::FontId::new(screen_size, egui::FontFamily::Proportional),
            (FontWeight::Bold, _) => egui::FontId::new(screen_size, egui::FontFamily::Proportional),
            (_, FontStyle::Italic) => egui::FontId::new(screen_size, egui::FontFamily::Proportional),
            _ => egui::FontId::proportional(screen_size),
        };

        // Measure so we can advance cursor
        let galley = ctx.painter.layout_no_wrap(
            span.content.clone(),
            font_id.clone(),
            color,
        );

        ctx.painter.galley(pos, galley, color);

        // Advance cursor by the text width in SVG units
        let advance = span.content.len() as f32 * font_size * 0.6;
        cursor_x = sx + advance;
        cursor_y = sy;
    }

    if highlight {
        // Draw a highlight underline under the whole text block
        if !spans.is_empty() {
            let first = &spans[0];
            let sx = first.x.unwrap_or(base_x);
            let sy = first.y.unwrap_or(base_y);
            let (tx, ty) = apply_transform(transform, sx, sy);
            let p1 = ctx.vt.svg_to_screen(tx, ty);
            let total_w: f32 = spans
                .iter()
                .map(|s| s.content.len() as f32 * s.font_size.unwrap_or(base_font_size) * 0.6)
                .sum();
            let (tx2, _) = apply_transform(transform, sx + total_w, sy);
            let p2 = ctx.vt.svg_to_screen(tx2, ty);
            ctx.painter.line_segment([p1, p2], Stroke::new(2.0, highlight_color()));
        }
    }
}

// ---------------------------------------------------------------------------
// Path rendering via lyon
// ---------------------------------------------------------------------------

fn render_path(
    ctx: &RenderContext,
    data: &str,
    transform: &Transform,
    fill_color: Option<Color32>,
    stroke: Stroke,
    highlight: bool,
) {
    let lyon_path = match parse_svg_path(data, transform, ctx.vt) {
        Some(p) => p,
        None => return,
    };

    if let Some(fill) = fill_color {
        if let Some(mesh) = tessellate_fill(&lyon_path, fill) {
            ctx.painter.add(mesh);
        }
    }

    if stroke.width > 0.0 {
        if let Some(mesh) = tessellate_stroke(&lyon_path, stroke) {
            ctx.painter.add(mesh);
        }
    }

    if highlight {
        if let Some(mesh) = tessellate_stroke(
            &lyon_path,
            Stroke::new(2.0, highlight_color()),
        ) {
            ctx.painter.add(mesh);
        }
    }
}

fn parse_svg_path(data: &str, transform: &Transform, vt: &ViewTransform) -> Option<LyonPath> {
    let mut builder = LyonPath::builder();
    let mut current = Point::new(0.0, 0.0);
    let mut start = Point::new(0.0, 0.0);
    let mut last_ctrl: Option<Point> = None;

    let tokens = tokenize_path(data);
    let mut i = 0;

    let to_screen = |x: f32, y: f32| -> Point {
        let (tx, ty) = transform.apply(x, y);
        let sp = vt.svg_to_screen(tx, ty);
        point(sp.x, sp.y)
    };

    let to_screen_delta = |dx: f32, dy: f32, from: Point| -> Point {
        // For relative commands, add delta to current SVG point then transform
        let svg_from = {
            // Approximate: invert the screen->svg for current point
            let svgx = (from.x - vt.offset.x) / vt.scale;
            let svgy = (from.y - vt.offset.y) / vt.scale;
            (svgx, svgy)
        };
        to_screen(svg_from.0 + dx, svg_from.1 + dy)
    };

    let mut in_subpath = false;

    while i < tokens.len() {
        let cmd = match &tokens[i] {
            PathToken::Cmd(c) => {
                i += 1;
                *c
            }
            // Implicit lineto if we see numbers without a command
            PathToken::Num(_) => {
                if in_subpath { 'L' } else { 'M' }
            }
        };

        match cmd {
            'M' | 'm' => {
                let relative = cmd == 'm';
                let x = next_num(&tokens, &mut i)?;
                let y = next_num(&tokens, &mut i)?;
                let p = if relative { to_screen_delta(x, y, current) } else { to_screen(x, y) };
                if in_subpath {
                    builder.end(false);
                }
                builder.begin(p);
                current = p;
                start = p;
                in_subpath = true;
                last_ctrl = None;

                // Subsequent coord pairs are implicit LineTo
                while peek_num(&tokens, i) {
                    let x2 = next_num(&tokens, &mut i)?;
                    let y2 = next_num(&tokens, &mut i)?;
                    let p2 = if relative { to_screen_delta(x2, y2, current) } else { to_screen(x2, y2) };
                    builder.line_to(p2);
                    current = p2;
                    last_ctrl = None;
                }
            }

            'L' | 'l' => {
                let relative = cmd == 'l';
                while peek_num(&tokens, i) {
                    let x = next_num(&tokens, &mut i)?;
                    let y = next_num(&tokens, &mut i)?;
                    let p = if relative { to_screen_delta(x, y, current) } else { to_screen(x, y) };
                    if !in_subpath {
                        builder.begin(p);
                        in_subpath = true;
                        start = p;
                    } else {
                        builder.line_to(p);
                    }
                    current = p;
                    last_ctrl = None;
                }
            }

            'H' | 'h' => {
                let relative = cmd == 'h';
                while peek_num(&tokens, i) {
                    let x = next_num(&tokens, &mut i)?;
                    let svg_cur = vt.screen_to_svg(Pos2::new(current.x, current.y));
                    let nx = if relative { svg_cur.0 + x } else { x };
                    let p = to_screen(nx, svg_cur.1);
                    builder.line_to(p);
                    current = p;
                    last_ctrl = None;
                }
            }

            'V' | 'v' => {
                let relative = cmd == 'v';
                while peek_num(&tokens, i) {
                    let y = next_num(&tokens, &mut i)?;
                    let svg_cur = vt.screen_to_svg(Pos2::new(current.x, current.y));
                    let ny = if relative { svg_cur.1 + y } else { y };
                    let p = to_screen(svg_cur.0, ny);
                    builder.line_to(p);
                    current = p;
                    last_ctrl = None;
                }
            }

            'C' | 'c' => {
                let relative = cmd == 'c';
                while peek_num(&tokens, i) {
                    let (x1, y1, x2, y2, x, y) = (
                        next_num(&tokens, &mut i)?,
                        next_num(&tokens, &mut i)?,
                        next_num(&tokens, &mut i)?,
                        next_num(&tokens, &mut i)?,
                        next_num(&tokens, &mut i)?,
                        next_num(&tokens, &mut i)?,
                    );
                    let (cp1, cp2, end) = if relative {
                        (
                            to_screen_delta(x1, y1, current),
                            to_screen_delta(x2, y2, current),
                            to_screen_delta(x, y, current),
                        )
                    } else {
                        (to_screen(x1, y1), to_screen(x2, y2), to_screen(x, y))
                    };
                    builder.cubic_bezier_to(cp1, cp2, end);
                    last_ctrl = Some(cp2);
                    current = end;
                }
            }

            'S' | 's' => {
                let relative = cmd == 's';
                while peek_num(&tokens, i) {
                    let (x2, y2, x, y) = (
                        next_num(&tokens, &mut i)?,
                        next_num(&tokens, &mut i)?,
                        next_num(&tokens, &mut i)?,
                        next_num(&tokens, &mut i)?,
                    );
                    let (cp2, end) = if relative {
                        (to_screen_delta(x2, y2, current), to_screen_delta(x, y, current))
                    } else {
                        (to_screen(x2, y2), to_screen(x, y))
                    };
                    // Reflect last control point
                    let cp1 = match last_ctrl {
                        Some(lc) => point(2.0 * current.x - lc.x, 2.0 * current.y - lc.y),
                        None => current,
                    };
                    builder.cubic_bezier_to(cp1, cp2, end);
                    last_ctrl = Some(cp2);
                    current = end;
                }
            }

            'Q' | 'q' => {
                let relative = cmd == 'q';
                while peek_num(&tokens, i) {
                    let (x1, y1, x, y) = (
                        next_num(&tokens, &mut i)?,
                        next_num(&tokens, &mut i)?,
                        next_num(&tokens, &mut i)?,
                        next_num(&tokens, &mut i)?,
                    );
                    let (cp, end) = if relative {
                        (to_screen_delta(x1, y1, current), to_screen_delta(x, y, current))
                    } else {
                        (to_screen(x1, y1), to_screen(x, y))
                    };
                    builder.quadratic_bezier_to(cp, end);
                    last_ctrl = Some(cp);
                    current = end;
                }
            }

            'T' | 't' => {
                let relative = cmd == 't';
                while peek_num(&tokens, i) {
                    let (x, y) = (next_num(&tokens, &mut i)?, next_num(&tokens, &mut i)?);
                    let end = if relative {
                        to_screen_delta(x, y, current)
                    } else {
                        to_screen(x, y)
                    };
                    let cp = match last_ctrl {
                        Some(lc) => point(2.0 * current.x - lc.x, 2.0 * current.y - lc.y),
                        None => current,
                    };
                    builder.quadratic_bezier_to(cp, end);
                    last_ctrl = Some(cp);
                    current = end;
                }
            }

            'A' | 'a' => {
                // Arc — approximate with line for now (full arc impl is complex)
                let relative = cmd == 'a';
                while peek_num(&tokens, i) {
                    let _rx = next_num(&tokens, &mut i)?;
                    let _ry = next_num(&tokens, &mut i)?;
                    let _x_rot = next_num(&tokens, &mut i)?;
                    let _large = next_num(&tokens, &mut i)?;
                    let _sweep = next_num(&tokens, &mut i)?;
                    let x = next_num(&tokens, &mut i)?;
                    let y = next_num(&tokens, &mut i)?;
                    let end = if relative {
                        to_screen_delta(x, y, current)
                    } else {
                        to_screen(x, y)
                    };
                    builder.line_to(end);
                    current = end;
                    last_ctrl = None;
                }
            }

            'Z' | 'z' => {
                builder.end(true);
                current = start;
                in_subpath = false;
                last_ctrl = None;
            }

            _ => {}
        }
    }

    if in_subpath {
        builder.end(false);
    }

    Some(builder.build())
}

// ---------------------------------------------------------------------------
// Path tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum PathToken {
    Cmd(char),
    Num(f32),
}

fn tokenize_path(data: &str) -> Vec<PathToken> {
    let mut tokens = Vec::new();
    let mut chars = data.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\n' | '\r' | ',' => {
                chars.next();
            }
            'A'..='Z' | 'a'..='z' => {
                tokens.push(PathToken::Cmd(c));
                chars.next();
            }
            '0'..='9' | '-' | '+' | '.' => {
                let mut s = String::new();
                // Handle sign
                if c == '-' || c == '+' {
                    s.push(c);
                    chars.next();
                }
                // Integer part
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        s.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                // Decimal part
                if let Some(&'.') = chars.peek() {
                    s.push('.');
                    chars.next();
                    while let Some(&d) = chars.peek() {
                        if d.is_ascii_digit() {
                            s.push(d);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
                // Exponent
                if let Some(&e) = chars.peek() {
                    if e == 'e' || e == 'E' {
                        s.push(e);
                        chars.next();
                        if let Some(&sign) = chars.peek() {
                            if sign == '-' || sign == '+' {
                                s.push(sign);
                                chars.next();
                            }
                        }
                        while let Some(&d) = chars.peek() {
                            if d.is_ascii_digit() {
                                s.push(d);
                                chars.next();
                            } else {
                                break;
                            }
                        }
                    }
                }
                if let Ok(n) = s.parse::<f32>() {
                    tokens.push(PathToken::Num(n));
                }
            }
            _ => {
                chars.next();
            }
        }
    }

    tokens
}

fn next_num(tokens: &[PathToken], i: &mut usize) -> Option<f32> {
    if let Some(PathToken::Num(n)) = tokens.get(*i) {
        *i += 1;
        Some(*n)
    } else {
        None
    }
}

fn peek_num(tokens: &[PathToken], i: usize) -> bool {
    matches!(tokens.get(i), Some(PathToken::Num(_)))
}

// ---------------------------------------------------------------------------
// Lyon tessellation helpers
// ---------------------------------------------------------------------------

fn tessellate_fill(path: &LyonPath, color: Color32) -> Option<egui::Shape> {
    let mut buffers: VertexBuffers<Pos2, u32> = VertexBuffers::new();
    let mut tessellator = FillTessellator::new();
    tessellator
        .tessellate_path(
            path.as_slice(),
            &FillOptions::default(),
            &mut BuffersBuilder::new(&mut buffers, |v: FillVertex| {
                Pos2::new(v.position().x, v.position().y)
            }),
        )
        .ok()?;

    Some(vertices_to_mesh(buffers, color))
}

fn tessellate_stroke(path: &LyonPath, stroke: Stroke) -> Option<egui::Shape> {
    let mut buffers: VertexBuffers<Pos2, u32> = VertexBuffers::new();
    let mut tessellator = StrokeTessellator::new();
    tessellator
        .tessellate_path(
            path.as_slice(),
            &StrokeOptions::default().with_line_width(stroke.width),
            &mut BuffersBuilder::new(&mut buffers, |v: StrokeVertex| {
                Pos2::new(v.position().x, v.position().y)
            }),
        )
        .ok()?;

    Some(vertices_to_mesh(buffers, stroke.color))
}

fn vertices_to_mesh(buffers: VertexBuffers<Pos2, u32>, color: Color32) -> egui::Shape {
    let vertices: Vec<egui::epaint::Vertex> = buffers
        .vertices
        .iter()
        .map(|&p| egui::epaint::Vertex {
            pos: p,
            uv: egui::epaint::WHITE_UV,
            color,
        })
        .collect();
    let indices: Vec<u32> = buffers.indices;

    egui::Shape::mesh(egui::Mesh {
        indices,
        vertices,
        texture_id: egui::TextureId::default(),
    })
}

// ---------------------------------------------------------------------------
// Ellipse rendering (approximated with beziers)
// ---------------------------------------------------------------------------

fn render_ellipse(
    ctx: &RenderContext,
    center: Pos2,
    rx: f32,
    ry: f32,
    fill: Option<Color32>,
    stroke: Stroke,
    highlight: bool,
) {
    // Approximate ellipse with 4 cubic beziers
    const K: f32 = 0.5522847498; // 4/3 * (sqrt(2) - 1)
    let (cx, cy) = (center.x, center.y);

    let mut builder = LyonPath::builder();
    builder.begin(point(cx + rx, cy));
    builder.cubic_bezier_to(point(cx + rx, cy - K * ry), point(cx + K * rx, cy - ry), point(cx, cy - ry));
    builder.cubic_bezier_to(point(cx - K * rx, cy - ry), point(cx - rx, cy - K * ry), point(cx - rx, cy));
    builder.cubic_bezier_to(point(cx - rx, cy + K * ry), point(cx - K * rx, cy + ry), point(cx, cy + ry));
    builder.cubic_bezier_to(point(cx + K * rx, cy + ry), point(cx + rx, cy + K * ry), point(cx + rx, cy));
    builder.end(true);
    let path = builder.build();

    if let Some(fill) = fill {
        if let Some(mesh) = tessellate_fill(&path, fill) {
            ctx.painter.add(mesh);
        }
    }
    if stroke.width > 0.0 {
        if let Some(mesh) = tessellate_stroke(&path, stroke) {
            ctx.painter.add(mesh);
        }
    }
    if highlight {
        if let Some(mesh) = tessellate_stroke(&path, Stroke::new(2.0, highlight_color())) {
            ctx.painter.add(mesh);
        }
    }
}

// ---------------------------------------------------------------------------
// Polygon fill (simple fan triangulation — works for convex polygons)
// ---------------------------------------------------------------------------

fn polygon_to_mesh(pts: &[Pos2], color: Color32) -> egui::Shape {
    if pts.len() < 3 {
        return egui::Shape::Noop;
    }
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for &p in pts {
        vertices.push(egui::epaint::Vertex {
            pos: p,
            uv: egui::epaint::WHITE_UV,
            color,
        });
    }
    for i in 1..(pts.len() as u32 - 1) {
        indices.extend_from_slice(&[0, i, i + 1]);
    }
    egui::Shape::mesh(egui::Mesh {
        vertices,
        indices,
        texture_id: egui::TextureId::default(),
    })
}

// ---------------------------------------------------------------------------
// Highlight overlay
// ---------------------------------------------------------------------------

pub fn highlight_color() -> Color32 {
    Color32::from_rgba_unmultiplied(30, 120, 255, 180)
}

fn draw_highlight_rect(ctx: &RenderContext, rect: Rect) {
    ctx.painter.rect(
        rect,
        egui::CornerRadius::ZERO,
        Color32::from_rgba_unmultiplied(30, 120, 255, 40),
        Stroke::new(2.0, highlight_color()),
        StrokeKind::Middle,
    );
}

fn draw_highlight_circle(ctx: &RenderContext, center: Pos2, r: f32) {
    ctx.painter.circle(
        center,
        r,
        Color32::from_rgba_unmultiplied(30, 120, 255, 40),
        Stroke::new(2.0, highlight_color()),
    );
}

// ---------------------------------------------------------------------------
// Transform helpers
// ---------------------------------------------------------------------------

fn apply_transform(t: &Transform, x: f32, y: f32) -> (f32, f32) {
    t.apply(x, y)
}

fn transform_scale_x(t: &Transform) -> f32 {
    let [a, b, ..] = t.matrix;
    (a * a + b * b).sqrt()
}

fn transform_scale_y(t: &Transform) -> f32 {
    let [_, _, c, d, ..] = t.matrix;
    (c * c + d * d).sqrt()
}
