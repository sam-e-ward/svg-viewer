/// ClipIndex — precomputes the local-space AABB for every `<clipPath>` in the document.
///
/// SVG `clipPathUnits="userSpaceOnUse"` (the default) means clip coordinates are in the
/// same coordinate system as the element that references the clipPath.  We therefore store
/// clip geometry in **local SVG space** (no world transform baked in) and let the renderer
/// apply the referencing element's accumulated world transform at draw time.
///
/// For each clipPath we compute the union AABB of all its child shapes and store it as
/// `[min_x, min_y, max_x, max_y]`.  This is an approximation for non-rectangular clip
/// paths, but it is always a superset of the true clip region — content is never
/// incorrectly hidden, only the clip boundary may be slightly loose.

use std::collections::HashMap;
use crate::svg_doc::{NodeId, SvgDocument, SvgNodeKind, SvgShape, Transform};
use crate::parser::parse_path_to_commands;

/// Maps clipPath svg-id → local-space AABB `[min_x, min_y, max_x, max_y]`.
pub struct ClipIndex {
    /// Keyed by the `id` attribute of the `<clipPath>` element.
    pub clips: HashMap<String, [f32; 4]>,
}

impl ClipIndex {
    pub fn build(doc: &SvgDocument) -> Self {
        let mut clips = HashMap::new();

        for node in &doc.nodes {
            if let SvgNodeKind::ClipPath { id } = &node.kind {
                if id.is_empty() { continue; }
                if let Some(aabb) = subtree_aabb(doc, node.id, &Transform::identity()) {
                    clips.insert(id.clone(), aabb);
                }
            }
        }

        ClipIndex { clips }
    }

    /// Look up the local-space AABB for a clip-path reference string (the value stored in
    /// `SvgNode::clip_path`, already stripped of `url(#...)`).
    pub fn get(&self, clip_id: &str) -> Option<[f32; 4]> {
        self.clips.get(clip_id).copied()
    }
}

// ---------------------------------------------------------------------------
// AABB computation helpers
// ---------------------------------------------------------------------------

/// Compute the union AABB of all descendant shapes of `node_id`, applying the
/// `parent_tf` accumulated transform.  Returns `None` if there are no shapes.
fn subtree_aabb(doc: &SvgDocument, node_id: NodeId, parent_tf: &Transform) -> Option<[f32; 4]> {
    let node = doc.get(node_id);
    let tf = parent_tf.concat(&node.transform);
    let mut result: Option<[f32; 4]> = None;

    match &node.kind {
        SvgNodeKind::Shape(shape) => {
            if let Some(bb) = shape_aabb(shape, &tf) {
                result = Some(union(result, bb));
            }
        }
        _ => {
            for &child in &node.children {
                if let Some(bb) = subtree_aabb(doc, child, &tf) {
                    result = Some(union(result, bb));
                }
            }
        }
    }

    result
}

/// Compute the AABB of a single shape in the given transform's coordinate space.
fn shape_aabb(shape: &SvgShape, tf: &Transform) -> Option<[f32; 4]> {
    match shape {
        SvgShape::Rect { x, y, width, height, .. } => {
            Some(quad_aabb(tf, *x, *y, x + width, y + height))
        }
        SvgShape::Circle { cx, cy, r } => {
            // AABB of circle: transform center, then pad by radius × max scale
            let scale = transform_max_scale(tf);
            let (tx, ty) = tf.apply(*cx, *cy);
            let r_s = r * scale;
            Some([tx - r_s, ty - r_s, tx + r_s, ty + r_s])
        }
        SvgShape::Ellipse { cx, cy, rx, ry } => {
            let (tx, ty) = tf.apply(*cx, *cy);
            let scale = transform_max_scale(tf);
            let rx_s = rx * scale;
            let ry_s = ry * scale;
            Some([tx - rx_s, ty - ry_s, tx + rx_s, ty + ry_s])
        }
        SvgShape::Line { x1, y1, x2, y2 } => {
            let (ax, ay) = tf.apply(*x1, *y1);
            let (bx, by) = tf.apply(*x2, *y2);
            Some([ax.min(bx), ay.min(by), ax.max(bx), ay.max(by)])
        }
        SvgShape::Polyline { points } | SvgShape::Polygon { points } => {
            points_aabb(tf, points)
        }
        SvgShape::Path { data } => {
            path_aabb(tf, data)
        }
        // Text / image / use — treat as zero-area; ignore for clip purposes
        _ => None,
    }
}

/// AABB of the four corners of an axis-aligned rect after transform.
fn quad_aabb(tf: &Transform, x0: f32, y0: f32, x1: f32, y1: f32) -> [f32; 4] {
    let corners = [
        tf.apply(x0, y0),
        tf.apply(x1, y0),
        tf.apply(x0, y1),
        tf.apply(x1, y1),
    ];
    let min_x = corners.iter().map(|c| c.0).fold(f32::INFINITY, f32::min);
    let min_y = corners.iter().map(|c| c.1).fold(f32::INFINITY, f32::min);
    let max_x = corners.iter().map(|c| c.0).fold(f32::NEG_INFINITY, f32::max);
    let max_y = corners.iter().map(|c| c.1).fold(f32::NEG_INFINITY, f32::max);
    [min_x, min_y, max_x, max_y]
}

fn points_aabb(tf: &Transform, points: &[(f32, f32)]) -> Option<[f32; 4]> {
    if points.is_empty() { return None; }
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for &(px, py) in points {
        let (tx, ty) = tf.apply(px, py);
        min_x = min_x.min(tx); min_y = min_y.min(ty);
        max_x = max_x.max(tx); max_y = max_y.max(ty);
    }
    Some([min_x, min_y, max_x, max_y])
}

fn path_aabb(tf: &Transform, data: &str) -> Option<[f32; 4]> {
    use crate::spatial_index::CmdKind;
    let cmds = parse_path_to_commands(data);
    if cmds.is_empty() { return None; }

    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut current = (0.0f32, 0.0f32);

    let mut expand = |px: f32, py: f32| {
        let (tx, ty) = tf.apply(px, py);
        if tx < min_x { min_x = tx; }
        if tx > max_x { max_x = tx; }
        if ty < min_y { min_y = ty; }
        if ty > max_y { max_y = ty; }
    };

    for cmd in &cmds {
        match cmd.kind {
            CmdKind::Move | CmdKind::Line => {
                for &(px, py) in &cmd.points {
                    expand(px, py);
                    current = (px, py);
                }
            }
            CmdKind::Cubic => {
                if cmd.points.len() >= 3 {
                    let (p0, p1, p2, p3) = (current, cmd.points[0], cmd.points[1], cmd.points[2]);
                    cubic_bbox_expand(p0, p1, p2, p3, &mut expand);
                    current = p3;
                }
            }
            CmdKind::Quadratic => {
                if cmd.points.len() >= 2 {
                    let (p0, p1, p2) = (current, cmd.points[0], cmd.points[1]);
                    quadratic_bbox_expand(p0, p1, p2, &mut expand);
                    current = p2;
                }
            }
            CmdKind::Close => {}
        }
    }

    if min_x == f32::INFINITY { None } else { Some([min_x, min_y, max_x, max_y]) }
}

fn cubic_bbox_expand<F: FnMut(f32, f32)>(
    p0: (f32,f32), p1: (f32,f32), p2: (f32,f32), p3: (f32,f32),
    expand: &mut F,
) {
    expand(p0.0, p0.1);
    expand(p3.0, p3.1);
    let mut ts = [f32::NAN; 4];
    let mut nt = 0usize;
    for axis in 0..2usize {
        let v = |p: (f32,f32)| if axis == 0 { p.0 } else { p.1 };
        let (a0,a1,a2,a3) = (v(p0),v(p1),v(p2),v(p3));
        let aa = -a0 + 3.0*a1 - 3.0*a2 + a3;
        let bb = 2.0*(a0 - 2.0*a1 + a2);
        let cc = a1 - a0;
        for t in solve_quadratic(aa, bb, cc) {
            if !t.is_nan() && t > 0.0 && t < 1.0 { ts[nt] = t; nt += 1; }
        }
    }
    for &t in &ts[..nt] {
        let u = 1.0 - t;
        let x = u*u*u*p0.0 + 3.0*u*u*t*p1.0 + 3.0*u*t*t*p2.0 + t*t*t*p3.0;
        let y = u*u*u*p0.1 + 3.0*u*u*t*p1.1 + 3.0*u*t*t*p2.1 + t*t*t*p3.1;
        expand(x, y);
    }
}

fn quadratic_bbox_expand<F: FnMut(f32, f32)>(
    p0: (f32,f32), p1: (f32,f32), p2: (f32,f32),
    expand: &mut F,
) {
    expand(p0.0, p0.1);
    expand(p2.0, p2.1);
    for axis in 0..2usize {
        let v = |p: (f32,f32)| if axis == 0 { p.0 } else { p.1 };
        let (a0,a1,a2) = (v(p0),v(p1),v(p2));
        let denom = a0 - 2.0*a1 + a2;
        if denom.abs() > 1e-10 {
            let t = (a0 - a1) / denom;
            if t > 0.0 && t < 1.0 {
                let u = 1.0 - t;
                let x = u*u*p0.0 + 2.0*u*t*p1.0 + t*t*p2.0;
                let y = u*u*p0.1 + 2.0*u*t*p1.1 + t*t*p2.1;
                expand(x, y);
            }
        }
    }
}

fn solve_quadratic(a: f32, b: f32, c: f32) -> [f32; 2] {
    if a.abs() < 1e-10 {
        if b.abs() < 1e-10 { return [f32::NAN, f32::NAN]; }
        return [-c / b, f32::NAN];
    }
    let disc = b*b - 4.0*a*c;
    if disc < 0.0 { return [f32::NAN, f32::NAN]; }
    let sq = disc.sqrt();
    [(-b - sq) / (2.0*a), (-b + sq) / (2.0*a)]
}

fn union(existing: Option<[f32; 4]>, bb: [f32; 4]) -> [f32; 4] {
    match existing {
        None => bb,
        Some([ax, ay, ax2, ay2]) => [
            ax.min(bb[0]), ay.min(bb[1]),
            ax2.max(bb[2]), ay2.max(bb[3]),
        ],
    }
}

fn transform_max_scale(t: &Transform) -> f32 {
    let [a, b, c, d, ..] = t.matrix;
    let sx = (a * a + b * b).sqrt();
    let sy = (c * c + d * d).sqrt();
    sx.max(sy)
}
