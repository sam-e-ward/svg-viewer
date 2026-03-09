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
    NodeId, SvgDocument, SvgNodeKind, SvgShape, Transform,
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

    /// Full precise hit-test: returns the topmost NodeId where the point is
    /// geometrically inside the shape (not just inside the AABB).
    pub fn hit_test_precise(&self, doc: &SvgDocument, sx: f32, sy: f32) -> Option<NodeId> {
        let point = [sx, sy];

        // Gather R-tree candidates
        let mut candidates: Vec<&IndexEntry> = self
            .tree
            .locate_all_at_point(&point)
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Sort by paint order descending — first precise hit wins
        candidates.sort_unstable_by(|a, b| b.paint_order.cmp(&a.paint_order));

        for entry in candidates {
            let node = doc.get(entry.node_id);
            if let SvgNodeKind::Shape(shape) = &node.kind {
                if shape_precise_hit(shape, &entry.world_transform, sx, sy) {
                    return Some(entry.node_id);
                }
            }
        }

        None
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
            if let Some(aabb) = shape_aabb(shape, &world) {
                let paint_order = entries.len();
                entries.push(IndexEntry {
                    node_id,
                    paint_order,
                    aabb,
                    world_transform: world,
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

fn shape_aabb(shape: &SvgShape, world: &Transform) -> Option<AABB<[f32; 2]>> {
    match shape {
        SvgShape::Rect { x, y, width, height, .. } => {
            // Transform all four corners
            let corners = [
                (*x, *y),
                (x + width, *y),
                (*x, y + height),
                (x + width, y + height),
            ];
            Some(corners_aabb(&corners, world))
        }
        SvgShape::Circle { cx, cy, r } => {
            // Transform centre, use radius scaled by max(|sx|, |sy|)
            let (tcx, tcy) = world.apply(*cx, *cy);
            let scale = transform_max_scale(world);
            let tr = r * scale;
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
            Some(corners_aabb(&corners, world))
        }
        SvgShape::Line { x1, y1, x2, y2 } => {
            let corners = [(*x1, *y1), (*x2, *y2)];
            Some(corners_aabb_with_pad(&corners, world, 4.0))
        }
        SvgShape::Polyline { points } | SvgShape::Polygon { points } => {
            if points.is_empty() {
                return None;
            }
            Some(corners_aabb(points, world))
        }
        SvgShape::Path { data } => {
            path_aabb(data, world)
        }
        SvgShape::Text { x, y, spans, font_size } => {
            // Use the first span's x/y if it overrides the base position
            let start_x = spans.first().and_then(|s| s.x).unwrap_or(*x);
            let start_y = spans.first().and_then(|s| s.y).unwrap_or(*y);
            let total_chars: usize = spans.iter().map(|s| s.content.len()).sum();
            // 0.6em per char is an approximation; pad generously for hitbox
            let w = (total_chars as f32 * font_size * 0.65).max(*font_size);
            // ascent ~0.8em above baseline, descender ~0.2em below
            let ascent = font_size * 0.85;
            let descent = font_size * 0.25;
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

/// Scan a path `d` string and collect control points to approximate the AABB.
/// This is not perfectly tight for curves, but is very fast and conservative
/// (may include some extra area around bezier control hulls).
fn path_aabb(data: &str, world: &Transform) -> Option<AABB<[f32; 2]>> {
    let cmds = parse_path_to_commands(data);
    if cmds.is_empty() {
        return None;
    }

    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut current = (0.0f32, 0.0f32);
    let mut start = (0.0f32, 0.0f32);

    for cmd in &cmds {
        for &(x, y) in &cmd.points {
            // Accumulate in local space first, then transform
            let (tx, ty) = world.apply(x, y);
            min_x = min_x.min(tx);
            min_y = min_y.min(ty);
            max_x = max_x.max(tx);
            max_y = max_y.max(ty);
        }
        if let Some(&last) = cmd.points.last() {
            current = last;
        }
        if cmd.is_move {
            start = current;
        }
    }

    if min_x.is_infinite() {
        return None;
    }

    // Small pad for stroke width tolerance
    Some(AABB::from_corners(
        [min_x - 2.0, min_y - 2.0],
        [max_x + 2.0, max_y + 2.0],
    ))
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

fn shape_precise_hit(shape: &SvgShape, world: &Transform, wx: f32, wy: f32) -> bool {
    // Transform the test point into local (element) space
    let (lx, ly) = inverse_transform(world, wx, wy);

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
            point_to_segment_dist(lx, ly, *x1, *y1, *x2, *y2) < 4.0
        }
        SvgShape::Polyline { points } => {
            for w in points.windows(2) {
                if point_to_segment_dist(lx, ly, w[0].0, w[0].1, w[1].0, w[1].1) < 3.0 {
                    return true;
                }
            }
            false
        }
        SvgShape::Polygon { points } => {
            // Ray-cast for filled polygon
            point_in_polygon(lx, ly, points)
        }
        SvgShape::Path { data } => {
            path_hit_test(data, lx, ly)
        }
        SvgShape::Text { x, y, spans, font_size } => {
            let start_x = spans.first().and_then(|s| s.x).unwrap_or(*x);
            let start_y = spans.first().and_then(|s| s.y).unwrap_or(*y);
            let total_chars: usize = spans.iter().map(|s| s.content.len()).sum();
            let w = (total_chars as f32 * font_size * 0.65).max(*font_size);
            let ascent = font_size * 0.85;
            let descent = font_size * 0.25;
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

fn path_hit_test(data: &str, lx: f32, ly: f32) -> bool {
    let cmds = parse_path_to_commands(data);
    if cmds.is_empty() {
        return false;
    }

    // Build a list of line segments from the path commands (approximating curves
    // with polylines), then do an even-odd ray cast.
    let segments = path_to_segments(&cmds);
    even_odd_ray_cast(lx, ly, &segments)
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
