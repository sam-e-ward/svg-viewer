/// Spatial index for fast SVG element hit-testing.
///
/// Strategy:
///   1. Walk the document tree once, computing each leaf element's
///      axis-aligned bounding box (AABB) in SVG user-unit space,
///      accumulating the full transform chain.
///   2. Store all (AABB, NodeId) pairs in an R-tree for O(log n) point queries.
///   3. On a hit-test query, get candidates from the R-tree, then run a
///      precise per-shape test (back-to-front order) and return the topmost hit.
///
/// The index is rebuilt once per document load and is read-only during
/// interaction — no locking needed.

use rstar::{RTree, RTreeObject, AABB, PointDistance};

use crate::svg_doc::{
    NodeId, Paint, SvgDocument, SvgNodeKind, SvgShape, Transform,
};
use crate::parser::parse_path_to_commands;

// ---------------------------------------------------------------------------
// R-tree entry
// ---------------------------------------------------------------------------

/// One entry in the R-tree: the world-space AABB of a leaf element, plus
/// its NodeId and the accumulated world transform (needed for precise testing).
#[derive(Clone, Debug)]
pub struct IndexEntry {
    pub node_id: NodeId,
    /// Painter's order index (higher = drawn later = on top)
    pub paint_order: usize,
    /// AABB in SVG user units (world space)
    aabb: AABB<[f32; 2]>,
    /// The full accumulated transform from root → this node
    pub world_transform: Transform,
    /// Stroke width in local SVG units (0 = no stroke)
    pub stroke_width: f32,
    /// Whether the fill is None (affects hit-test strategy)
    pub fill_none: bool,
}

impl RTreeObject for IndexEntry {
    type Envelope = AABB<[f32; 2]>;
    fn envelope(&self) -> Self::Envelope {
        self.aabb
    }
}

impl PointDistance for IndexEntry {
    fn distance_2(&self, point: &[f32; 2]) -> f32 {
        self.aabb.distance_2(point)
    }
}

// ---------------------------------------------------------------------------
// Public index struct
// ---------------------------------------------------------------------------

pub struct SpatialIndex {
    tree: RTree<IndexEntry>,
    /// All entries in paint order (back→front), used for topmost-hit resolution
    entries_by_paint_order: Vec<IndexEntry>,
}

impl SpatialIndex {
    /// Build the index from a parsed SVG document.
    /// This is O(n log n) in the number of leaf elements.
    pub fn build(doc: &SvgDocument) -> Self {
        let mut entries: Vec<IndexEntry> = Vec::new();
        collect_entries(doc, doc.root, &Transform::identity(), &mut entries);

        // Sort by paint order ascending (already in document order from the
        // recursive walk, but make it explicit)
        // entries are already in document/painter order — index == paint_order

        let tree = RTree::bulk_load(entries.clone());

        SpatialIndex {
            tree,
            entries_by_paint_order: entries,
        }
    }

    /// Return the union world-space AABB of all leaf descendants of `node_id`.
    /// Used to draw a bounding box for `<g>` elements.
    pub fn bbox_for_subtree(&self, doc: &SvgDocument, node_id: NodeId) -> Option<[f32; 4]> {
        let mut descendants = Vec::new();
        collect_leaf_ids(doc, node_id, &mut descendants);
        let mut result: Option<[f32; 4]> = None;
        for leaf_id in descendants {
            if let Some(bb) = self.bbox_for_node(leaf_id) {
                result = Some(match result {
                    None => bb,
                    Some([ax, ay, ax2, ay2]) => [
                        ax.min(bb[0]), ay.min(bb[1]),
                        ax2.max(bb[2]), ay2.max(bb[3]),
                    ],
                });
            }
        }
        result
    }

    /// Return the world-space AABB `[min_x, min_y, max_x, max_y]` for a given
    /// node, if it exists in the index.
    pub fn bbox_for_node(&self, node_id: NodeId) -> Option<[f32; 4]> {
        self.entries_by_paint_order
            .iter()
            .find(|e| e.node_id == node_id)
            .map(|e| {
                let lo = e.aabb.lower();
                let hi = e.aabb.upper();
                [lo[0], lo[1], hi[0], hi[1]]
            })
    }

    /// Full precise hit-test: returns the topmost NodeId where the point is
    /// geometrically inside the shape (not just inside the AABB).
    pub fn hit_test_precise(&self, doc: &SvgDocument, sx: f32, sy: f32, view_scale: f32) -> Option<NodeId> {
        self.hit_test_all(doc, sx, sy, view_scale).into_iter().next()
    }

    /// Returns all NodeIds precisely hit at `(sx, sy)`, sorted topmost-first
    /// (descending paint order). Used for TAB-cycling through stacked elements.
    ///
    /// `view_scale` is the current viewport zoom (screen pixels per SVG unit).
    /// It is used to express the minimum stroke pick radius in screen pixels rather
    /// than SVG units, so hairline strokes stay pickable without creating enormous
    /// hit areas when zoomed in.
    pub fn hit_test_all(&self, doc: &SvgDocument, sx: f32, sy: f32, view_scale: f32) -> Vec<NodeId> {
        let point = [sx, sy];

        let mut candidates: Vec<&IndexEntry> = self
            .tree
            .locate_all_at_point(&point)
            .collect();

        if candidates.is_empty() {
            return Vec::new();
        }

        // Sort by paint order descending — topmost (last-painted) first
        candidates.sort_unstable_by(|a, b| b.paint_order.cmp(&a.paint_order));

        candidates
            .into_iter()
            .filter_map(|entry| {
                let node = doc.get(entry.node_id);
                if let SvgNodeKind::Shape(shape) = &node.kind {
                    if shape_precise_hit(
                        shape,
                        &entry.world_transform,
                        sx, sy,
                        entry.stroke_width,
                        entry.fill_none,
                        view_scale,
                    ) {
                        return Some(entry.node_id);
                    }
                }
                None
            })
            .collect()
    }

}

// ---------------------------------------------------------------------------
// Tree walk — collect IndexEntry for every leaf shape
// ---------------------------------------------------------------------------

fn collect_entries(
    doc: &SvgDocument,
    node_id: NodeId,
    parent_transform: &Transform,
    entries: &mut Vec<IndexEntry>,
) {
    let node = doc.get(node_id);
    let world = parent_transform.concat(&node.transform);

    match &node.kind {
        SvgNodeKind::Svg { .. } | SvgNodeKind::Group => {
            for &child in &node.children {
                collect_entries(doc, child, &world, entries);
            }
        }
        SvgNodeKind::Shape(shape) => {
            let stroke_width = match &node.style.stroke {
                Paint::None => 0.0,
                _ => node.style.stroke_width,
            };
            let fill_none = matches!(&node.style.fill, Paint::None);
            if let Some(aabb) = shape_aabb(shape, &world, stroke_width) {
                let paint_order = entries.len();
                entries.push(IndexEntry {
                    node_id,
                    paint_order,
                    aabb,
                    world_transform: world,
                    stroke_width,
                    fill_none,
                });
            }
        }
        // Defs, clipPath, mask etc. are not hit-testable
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// AABB computation
// ---------------------------------------------------------------------------

fn shape_aabb(shape: &SvgShape, world: &Transform, stroke_width: f32) -> Option<AABB<[f32; 2]>> {
    // Padding applied in world space: half stroke width transformed by scale.
    // We'll add it after transformation.
    let half_sw = stroke_width / 2.0;
    // Minimum 1-unit pad so shapes are always findable
    let pad = half_sw.max(1.0);

    match shape {
        SvgShape::Rect { x, y, width, height, .. } => {
            let corners = [
                (*x, *y),
                (x + width, *y),
                (*x, y + height),
                (x + width, y + height),
            ];
            Some(corners_aabb_with_pad(&corners, world, pad))
        }
        SvgShape::Circle { cx, cy, r } => {
            let (tcx, tcy) = world.apply(*cx, *cy);
            let scale = transform_max_scale(world);
            let tr = r * scale + pad * scale;
            Some(AABB::from_corners(
                [tcx - tr, tcy - tr],
                [tcx + tr, tcy + tr],
            ))
        }
        SvgShape::Ellipse { cx, cy, rx, ry } => {
            let corners = [
                (cx - rx, cy - ry),
                (cx + rx, cy - ry),
                (cx - rx, cy + ry),
                (cx + rx, cy + ry),
            ];
            Some(corners_aabb_with_pad(&corners, world, pad))
        }
        SvgShape::Line { x1, y1, x2, y2 } => {
            let corners = [(*x1, *y1), (*x2, *y2)];
            // Lines must always have some pickup radius
            let line_pad = half_sw.max(2.0);
            Some(corners_aabb_with_pad(&corners, world, line_pad))
        }
        SvgShape::Polyline { points } | SvgShape::Polygon { points } => {
            if points.is_empty() {
                return None;
            }
            Some(corners_aabb_with_pad(points, world, pad))
        }
        SvgShape::Path { data } => {
            path_aabb(data, world, pad)
        }
        SvgShape::Text { x, y, spans, font_size } => {
            let start_x = spans.first().and_then(|s| s.x).unwrap_or(*x);
            let start_y = spans.first().and_then(|s| s.y).unwrap_or(*y);
            let total_chars: usize = spans.iter().map(|s| s.content.len()).sum();
            // 0.6em per char; pad generously for hitbox
            let w = (total_chars as f32 * font_size * 0.6).max(*font_size);
            let ascent = font_size * 0.8;
            let descent = font_size * 0.2;
            let corners = [
                (start_x, start_y - ascent),
                (start_x + w, start_y - ascent),
                (start_x, start_y + descent),
                (start_x + w, start_y + descent),
            ];
            Some(corners_aabb(&corners, world))
        }
        SvgShape::Image { x, y, width, height, .. } => {
            let corners = [
                (*x, *y),
                (x + width, *y),
                (*x, y + height),
                (x + width, y + height),
            ];
            Some(corners_aabb(&corners, world))
        }
        SvgShape::Use { .. } => None,
    }
}

fn corners_aabb(corners: &[(f32, f32)], world: &Transform) -> AABB<[f32; 2]> {
    corners_aabb_with_pad(corners, world, 0.0)
}

fn corners_aabb_with_pad(corners: &[(f32, f32)], world: &Transform, pad: f32) -> AABB<[f32; 2]> {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;

    for &(x, y) in corners {
        let (tx, ty) = world.apply(x, y);
        min_x = min_x.min(tx);
        min_y = min_y.min(ty);
        max_x = max_x.max(tx);
        max_y = max_y.max(ty);
    }

    AABB::from_corners(
        [min_x - pad, min_y - pad],
        [max_x + pad, max_y + pad],
    )
}

/// Compute a tight bounding box for a path by analytically finding the extrema
/// of each cubic/quadratic Bézier segment, rather than just using control points.
fn path_aabb(data: &str, world: &Transform, pad: f32) -> Option<AABB<[f32; 2]>> {
    let cmds = parse_path_to_commands(data);
    if cmds.is_empty() {
        return None;
    }

    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;

    let mut expand = |x: f32, y: f32| {
        let (tx, ty) = world.apply(x, y);
        if tx < min_x { min_x = tx; }
        if tx > max_x { max_x = tx; }
        if ty < min_y { min_y = ty; }
        if ty > max_y { max_y = ty; }
    };

    let mut current = (0.0f32, 0.0f32);

    for cmd in &cmds {
        match cmd.kind {
            CmdKind::Move | CmdKind::Line => {
                for &(x, y) in &cmd.points {
                    expand(x, y);
                    current = (x, y);
                }
            }
            CmdKind::Cubic => {
                let pts = &cmd.points;
                if pts.len() >= 3 {
                    let (p0, p1, p2, p3) = (current, pts[0], pts[1], pts[2]);
                    cubic_bbox(p0, p1, p2, p3, &mut expand);
                    current = p3;
                }
            }
            CmdKind::Quadratic => {
                let pts = &cmd.points;
                if pts.len() >= 2 {
                    let (p0, p1, p2) = (current, pts[0], pts[1]);
                    quadratic_bbox(p0, p1, p2, &mut expand);
                    current = p2;
                }
            }
            CmdKind::Close => {
                // No new geometry to bound; close just draws a line back
            }
        }
    }

    if min_x.is_infinite() {
        return None;
    }

    Some(AABB::from_corners(
        [min_x - pad, min_y - pad],
        [max_x + pad, max_y + pad],
    ))
}

/// Expand a bounding box to include all extrema of a cubic Bézier.
/// Extrema occur at t=0, t=1, and the roots of the derivative (one quadratic per axis).
fn cubic_bbox<F: FnMut(f32, f32)>(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    p3: (f32, f32),
    expand: &mut F,
) {
    // Always include endpoints
    expand(p0.0, p0.1);
    expand(p3.0, p3.1);

    // Collect interior extremum t-values from both axes
    let mut ts = [f32::NAN; 4];
    let mut nt = 0usize;

    for axis in 0..2usize {
        let v = |p: (f32, f32)| if axis == 0 { p.0 } else { p.1 };
        let a0 = v(p0); let a1 = v(p1); let a2 = v(p2); let a3 = v(p3);
        // d/dt cubic = 3[(-a0+3a1-3a2+a3)t^2 + 2(a0-2a1+a2)t + (a1-a0)]
        let aa = -a0 + 3.0*a1 - 3.0*a2 + a3;
        let bb = 2.0*(a0 - 2.0*a1 + a2);
        let cc = a1 - a0;
        for t in quadratic_roots(aa, bb, cc) {
            if !t.is_nan() && t > 0.0 && t < 1.0 {
                ts[nt] = t;
                nt += 1;
            }
        }
    }

    for &t in &ts[..nt] {
        let (ex, ey) = cubic_eval2(p0, p1, p2, p3, t);
        expand(ex, ey);
    }
}

/// Evaluate a cubic Bézier at parameter t, returning both x and y.
fn cubic_eval2(p0: (f32,f32), p1: (f32,f32), p2: (f32,f32), p3: (f32,f32), t: f32) -> (f32,f32) {
    let u = 1.0 - t;
    let x = u*u*u*p0.0 + 3.0*u*u*t*p1.0 + 3.0*u*t*t*p2.0 + t*t*t*p3.0;
    let y = u*u*u*p0.1 + 3.0*u*u*t*p1.1 + 3.0*u*t*t*p2.1 + t*t*t*p3.1;
    (x, y)
}

/// Evaluate one axis of a cubic Bézier at t (kept for legacy callers — not used).
#[allow(dead_code)]
fn cubic_eval(p0: (f32,f32), p1: (f32,f32), p2: (f32,f32), p3: (f32,f32), t: f32) -> f32 {
    let u = 1.0 - t;
    u*u*u*p0.0 + 3.0*u*u*t*p1.0 + 3.0*u*t*t*p2.0 + t*t*t*p3.0
}

/// Expand a bounding box to include all extrema of a quadratic Bézier.
fn quadratic_bbox<F: FnMut(f32, f32)>(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    expand: &mut F,
) {
    expand(p0.0, p0.1);
    expand(p2.0, p2.1);

    for axis in 0..2 {
        let v = |p: (f32, f32)| if axis == 0 { p.0 } else { p.1 };
        let a0 = v(p0);
        let a1 = v(p1);
        let a2 = v(p2);
        // Derivative of quadratic: 2*(a1-a0) + 2*(a0-2*a1+a2)*t = 0
        // t = (a0 - a1) / (a0 - 2*a1 + a2)
        let denom = a0 - 2.0 * a1 + a2;
        if denom.abs() > 1e-10 {
            let t = (a0 - a1) / denom;
            if t > 0.0 && t < 1.0 {
                let u = 1.0 - t;
                let px = u*u*p0.0 + 2.0*u*t*p1.0 + t*t*p2.0;
                let py = u*u*p0.1 + 2.0*u*t*p1.1 + t*t*p2.1;
                expand(px, py);
            }
        }
    }
}

/// Solve At^2 + Bt + C = 0, returning real roots in [0,1].
fn quadratic_roots(a: f32, b: f32, c: f32) -> [f32; 2] {
    if a.abs() < 1e-10 {
        // Linear
        if b.abs() < 1e-10 {
            return [f32::NAN, f32::NAN];
        }
        return [-c / b, f32::NAN];
    }
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return [f32::NAN, f32::NAN];
    }
    let sq = disc.sqrt();
    [(-b - sq) / (2.0 * a), (-b + sq) / (2.0 * a)]
}

fn transform_max_scale(t: &Transform) -> f32 {
    let [a, b, c, d, ..] = t.matrix;
    let sx = (a * a + b * b).sqrt();
    let sy = (c * c + d * d).sqrt();
    sx.max(sy)
}

// ---------------------------------------------------------------------------
// Precise per-shape hit-tests (world-space point, local-space geometry)
// ---------------------------------------------------------------------------

fn shape_precise_hit(
    shape: &SvgShape,
    world: &Transform,
    wx: f32,
    wy: f32,
    stroke_width: f32,
    fill_none: bool,
    view_scale: f32,
) -> bool {
    // Transform the test point into local (element) space
    let (lx, ly) = inverse_transform(world, wx, wy);

    // Stroke pick threshold in local SVG units.
    //
    // We want the threshold to be:
    //   max(stroke_width / 2,  MIN_SCREEN_PX / total_scale)
    //
    // where total_scale = world_scale * view_scale converts local units → screen px.
    // MIN_SCREEN_PX = 3 keeps any stroke pickable when it is near the cursor,
    // but the threshold shrinks as you zoom in so it doesn't become enormous.
    const MIN_SCREEN_PX: f32 = 3.0;
    let world_scale = transform_max_scale(world).max(1e-6);
    let min_local = MIN_SCREEN_PX / (world_scale * view_scale);
    let stroke_thresh = (stroke_width / 2.0).max(min_local);

    match shape {
        SvgShape::Rect { x, y, width, height, .. } => {
            let inside = lx >= *x && lx <= x + width && ly >= *y && ly <= y + height;
            if fill_none {
                if !inside {
                    return false;
                }
                let dl = (lx - x).abs();
                let dr = (lx - (x + width)).abs();
                let dt = (ly - y).abs();
                let db = (ly - (y + height)).abs();
                dl.min(dr).min(dt).min(db) <= stroke_thresh
            } else {
                inside
            }
        }
        SvgShape::Circle { cx, cy, r } => {
            let dx = lx - cx;
            let dy = ly - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            if fill_none {
                (dist - r).abs() <= stroke_thresh
            } else {
                dist <= *r
            }
        }
        SvgShape::Ellipse { cx, cy, rx, ry } => {
            if *rx == 0.0 || *ry == 0.0 {
                return false;
            }
            let dx = (lx - cx) / rx;
            let dy = (ly - cy) / ry;
            let norm = dx * dx + dy * dy;
            if fill_none {
                let thr_x = stroke_thresh / rx;
                let thr_y = stroke_thresh / ry;
                let thr = thr_x.min(thr_y);
                let inner = (1.0 - thr).max(0.0);
                let outer = 1.0 + thr;
                norm >= inner * inner && norm <= outer * outer
            } else {
                norm <= 1.0
            }
        }
        SvgShape::Line { x1, y1, x2, y2 } => {
            point_to_segment_dist(lx, ly, *x1, *y1, *x2, *y2) <= stroke_thresh
        }
        SvgShape::Polyline { points } => {
            for w in points.windows(2) {
                if point_to_segment_dist(lx, ly, w[0].0, w[0].1, w[1].0, w[1].1) <= stroke_thresh {
                    return true;
                }
            }
            false
        }
        SvgShape::Polygon { points } => {
            if fill_none {
                for w in points.windows(2) {
                    if point_to_segment_dist(lx, ly, w[0].0, w[0].1, w[1].0, w[1].1) <= stroke_thresh {
                        return true;
                    }
                }
                if let (Some(&first), Some(&last)) = (points.first(), points.last()) {
                    if point_to_segment_dist(lx, ly, last.0, last.1, first.0, first.1) <= stroke_thresh {
                        return true;
                    }
                }
                false
            } else {
                point_in_polygon(lx, ly, points)
            }
        }
        SvgShape::Path { data } => {
            path_hit_test(data, lx, ly, fill_none, stroke_thresh)
        }
        SvgShape::Text { x, y, spans, font_size } => {
            let start_x = spans.first().and_then(|s| s.x).unwrap_or(*x);
            let start_y = spans.first().and_then(|s| s.y).unwrap_or(*y);
            let total_chars: usize = spans.iter().map(|s| s.content.len()).sum();
            let w = (total_chars as f32 * font_size * 0.6).max(*font_size);
            let ascent = font_size * 0.8;
            let descent = font_size * 0.2;
            lx >= start_x && lx <= start_x + w
                && ly >= start_y - ascent && ly <= start_y + descent
        }
        SvgShape::Image { x, y, width, height, .. } => {
            lx >= *x && lx <= x + width && ly >= *y && ly <= y + height
        }
        SvgShape::Use { .. } => false,
    }
}

// ---------------------------------------------------------------------------
// Path hit-test: even-odd ray casting in local space
// ---------------------------------------------------------------------------

fn path_hit_test(data: &str, lx: f32, ly: f32, fill_none: bool, stroke_thresh: f32) -> bool {
    let cmds = parse_path_to_commands(data);
    if cmds.is_empty() {
        return false;
    }

    let segments = path_to_segments(&cmds);

    if fill_none {
        // Stroke-only path: hit if within stroke_thresh of any segment
        for &((x1, y1), (x2, y2)) in &segments {
            if point_to_segment_dist(lx, ly, x1, y1, x2, y2) <= stroke_thresh {
                return true;
            }
        }
        false
    } else {
        // Filled path: even-odd interior test; also allow picking near stroke
        even_odd_ray_cast(lx, ly, &segments)
    }
}

/// Flattened segment: (x1,y1) → (x2,y2)
type Seg = ((f32, f32), (f32, f32));

fn path_to_segments(cmds: &[PathCmd]) -> Vec<Seg> {
    let mut segs: Vec<Seg> = Vec::new();
    let mut current = (0.0f32, 0.0f32);
    let mut subpath_start = (0.0f32, 0.0f32);

    for cmd in cmds {
        match cmd.kind {
            CmdKind::Move => {
                if let Some(&pt) = cmd.points.first() {
                    subpath_start = pt;
                    current = pt;
                }
            }
            CmdKind::Line => {
                for &pt in &cmd.points {
                    segs.push((current, pt));
                    current = pt;
                }
            }
            CmdKind::Cubic => {
                // Subdivide cubic into ~8 line segments
                let pts = &cmd.points;
                if pts.len() >= 3 {
                    let (p0, p1, p2, p3) = (current, pts[0], pts[1], pts[2]);
                    flatten_cubic(p0, p1, p2, p3, &mut segs, 3);
                    current = p3;
                }
            }
            CmdKind::Quadratic => {
                let pts = &cmd.points;
                if pts.len() >= 2 {
                    let (p0, p1, p2) = (current, pts[0], pts[1]);
                    flatten_quadratic(p0, p1, p2, &mut segs, 3);
                    current = p2;
                }
            }
            CmdKind::Close => {
                segs.push((current, subpath_start));
                current = subpath_start;
            }
        }
    }

    segs
}

/// Recursive cubic flattening (de Casteljau), `depth` levels deep.
fn flatten_cubic(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    p3: (f32, f32),
    segs: &mut Vec<Seg>,
    depth: u8,
) {
    if depth == 0 {
        segs.push((p0, p3));
        return;
    }
    let mid = |a: (f32, f32), b: (f32, f32)| ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
    let p01 = mid(p0, p1);
    let p12 = mid(p1, p2);
    let p23 = mid(p2, p3);
    let p012 = mid(p01, p12);
    let p123 = mid(p12, p23);
    let p0123 = mid(p012, p123);
    flatten_cubic(p0, p01, p012, p0123, segs, depth - 1);
    flatten_cubic(p0123, p123, p23, p3, segs, depth - 1);
}

fn flatten_quadratic(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    segs: &mut Vec<Seg>,
    depth: u8,
) {
    if depth == 0 {
        segs.push((p0, p2));
        return;
    }
    let mid = |a: (f32, f32), b: (f32, f32)| ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
    let p01 = mid(p0, p1);
    let p12 = mid(p1, p2);
    let p012 = mid(p01, p12);
    flatten_quadratic(p0, p01, p012, segs, depth - 1);
    flatten_quadratic(p012, p12, p2, segs, depth - 1);
}

/// Even-odd ray cast: cast a ray in +X direction from (px, py),
/// count crossings, odd = inside.
fn even_odd_ray_cast(px: f32, py: f32, segs: &[Seg]) -> bool {
    let mut crossings = 0usize;
    for &((x1, y1), (x2, y2)) in segs {
        // Check if the segment crosses the horizontal ray y = py, x >= px
        let above = y1 > py;
        let below = y2 > py;
        if above == below {
            continue; // both on same side, no crossing
        }
        // x coordinate at y = py
        let x_cross = x1 + (py - y1) * (x2 - x1) / (y2 - y1);
        if x_cross >= px {
            crossings += 1;
        }
    }
    crossings % 2 == 1
}

fn point_in_polygon(px: f32, py: f32, points: &[(f32, f32)]) -> bool {
    if points.len() < 3 {
        return false;
    }
    let mut segs: Vec<Seg> = Vec::with_capacity(points.len());
    for w in points.windows(2) {
        segs.push((w[0], w[1]));
    }
    segs.push((*points.last().unwrap(), points[0]));
    even_odd_ray_cast(px, py, &segs)
}

// ---------------------------------------------------------------------------
// Path command representation (shared with parser)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum CmdKind {
    Move,
    Line,
    Cubic,
    Quadratic,
    Close,
}

#[derive(Debug, Clone)]
pub struct PathCmd {
    pub kind: CmdKind,
    /// For Move/Line: [endpoint]
    /// For Cubic: [cp1, cp2, endpoint]
    /// For Quadratic: [cp, endpoint]
    /// For Close: []
    pub points: Vec<(f32, f32)>,
    pub is_move: bool,
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

fn inverse_transform(t: &Transform, x: f32, y: f32) -> (f32, f32) {
    let [a, b, c, d, e, f] = t.matrix;
    let det = a * d - b * c;
    if det.abs() < 1e-10 {
        return (x, y);
    }
    let inv = 1.0 / det;
    let ia = d * inv;
    let ib = -b * inv;
    let ic = -c * inv;
    let id = a * inv;
    let ie = (c * f - d * e) * inv;
    let if_ = (b * e - a * f) * inv;
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

// ---------------------------------------------------------------------------
// Subtree leaf collection
// ---------------------------------------------------------------------------

/// Collect all leaf (shape) node ids that are descendants of `node_id`.
fn collect_leaf_ids(doc: &SvgDocument, node_id: NodeId, out: &mut Vec<NodeId>) {
    let node = doc.get(node_id);
    match &node.kind {
        SvgNodeKind::Shape(_) => out.push(node_id),
        _ => {
            for &child in &node.children {
                collect_leaf_ids(doc, child, out);
            }
        }
    }
}
