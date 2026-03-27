/// Precomputed visibility state for element type and style toggling.
///
/// Built once per document load, then mutated by the Filter pane.
/// The renderer checks this to decide whether to draw each node.

use std::collections::HashMap;

use crate::svg_doc::{NodeId, Paint, SvgDocument, SvgNodeKind, SvgShape};

// ---------------------------------------------------------------------------
// Element-type visibility
// ---------------------------------------------------------------------------

/// The set of toggleable element types shown in the Filter pane.
/// Only directly-visible shape types for now; clips/masks will be added later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ElementType {
    Path,
    Rect,
    Circle,
    Ellipse,
    Line,
    Polyline,
    Polygon,
    Text,
    Image,
}

impl ElementType {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Rect => "rect",
            Self::Circle => "circle",
            Self::Ellipse => "ellipse",
            Self::Line => "line",
            Self::Polyline => "polyline",
            Self::Polygon => "polygon",
            Self::Text => "text",
            Self::Image => "image",
        }
    }

    fn from_shape(shape: &SvgShape) -> Self {
        match shape {
            SvgShape::Path { .. } => Self::Path,
            SvgShape::Rect { .. } => Self::Rect,
            SvgShape::Circle { .. } => Self::Circle,
            SvgShape::Ellipse { .. } => Self::Ellipse,
            SvgShape::Line { .. } => Self::Line,
            SvgShape::Polyline { .. } => Self::Polyline,
            SvgShape::Polygon { .. } => Self::Polygon,
            SvgShape::Text { .. } => Self::Text,
            SvgShape::Image { .. } => Self::Image,
            SvgShape::Use { .. } => Self::Path, // fallback
        }
    }
}

// ---------------------------------------------------------------------------
// Path style signature — groups paths by visual appearance
// ---------------------------------------------------------------------------

/// A canonical description of a path's visual style, used to group paths
/// that look the same so they can be toggled together.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathStyleKey {
    pub fill: String,
    pub stroke: String,
    pub stroke_width_bits: u32,
}

impl PathStyleKey {
    fn from_style(style: &crate::svg_doc::Style) -> Self {
        Self {
            fill: paint_key(&style.fill),
            stroke: paint_key(&style.stroke),
            stroke_width_bits: style.stroke_width.to_bits(),
        }
    }

    /// Human-readable one-line description.
    pub fn description(&self) -> String {
        let mut parts = Vec::new();
        if self.fill != "none" {
            parts.push(format!("fill: {}", self.fill));
        }
        if self.stroke != "none" {
            let sw = f32::from_bits(self.stroke_width_bits);
            parts.push(format!("stroke: {} ({:.2}px)", self.stroke, sw));
        }
        if parts.is_empty() {
            "fill: none, stroke: none".to_string()
        } else {
            parts.join(", ")
        }
    }
}

fn paint_key(paint: &Paint) -> String {
    match paint {
        Paint::None => "none".to_string(),
        Paint::Color(c) => format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b),
        Paint::LinearGradient(id) => format!("url(#{})", id),
        Paint::RadialGradient(id) => format!("url(#{})", id),
    }
}

// ---------------------------------------------------------------------------
// Path style group — one entry per unique style
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PathStyleGroup {
    pub key: PathStyleKey,
    /// Node IDs of all (non-filtered) paths with this style.
    pub node_ids: Vec<NodeId>,
    /// Whether paths with this style are visible.
    pub visible: bool,
}

// ---------------------------------------------------------------------------
// VisibilityState — the main struct
// ---------------------------------------------------------------------------

pub struct VisibilityState {
    /// Per-element-type toggle and count.
    /// Sorted by count descending for display.
    pub type_toggles: Vec<(ElementType, usize, bool)>,

    /// Per-path-style toggle, sorted by count descending.
    pub path_styles: Vec<PathStyleGroup>,

    /// Fast lookup: NodeId → index into path_styles.
    /// Only populated for <path> elements.
    node_to_style: HashMap<NodeId, usize>,
}

impl VisibilityState {
    /// Build visibility state from a parsed (and possibly filtered) document.
    pub fn build(doc: &SvgDocument) -> Self {
        // --- Element type counts ---
        let mut type_counts: HashMap<ElementType, usize> = HashMap::new();

        // --- Path style grouping ---
        let mut style_map: HashMap<PathStyleKey, Vec<NodeId>> = HashMap::new();

        for node in &doc.nodes {
            if node.filtered {
                continue;
            }
            if let SvgNodeKind::Shape(shape) = &node.kind {
                let etype = ElementType::from_shape(shape);
                *type_counts.entry(etype).or_insert(0) += 1;

                if matches!(shape, SvgShape::Path { .. }) {
                    let key = PathStyleKey::from_style(&node.style);
                    style_map.entry(key).or_default().push(node.id);
                }
            }
        }

        // Sort type toggles by count desc
        let mut type_toggles: Vec<(ElementType, usize, bool)> = type_counts
            .into_iter()
            .map(|(t, c)| (t, c, true))
            .collect();
        type_toggles.sort_by(|a, b| b.1.cmp(&a.1));

        // Sort path styles by count desc
        let mut path_styles: Vec<PathStyleGroup> = style_map
            .into_iter()
            .map(|(key, node_ids)| PathStyleGroup {
                key,
                node_ids,
                visible: true,
            })
            .collect();
        path_styles.sort_by(|a, b| b.node_ids.len().cmp(&a.node_ids.len()));

        // Build node → style index lookup
        let mut node_to_style = HashMap::new();
        for (idx, group) in path_styles.iter().enumerate() {
            for &nid in &group.node_ids {
                node_to_style.insert(nid, idx);
            }
        }

        VisibilityState {
            type_toggles,
            path_styles,
            node_to_style,
        }
    }

    /// Check whether a node should be rendered.
    /// Returns false if the node's element type or path style is toggled off.
    #[inline]
    pub fn is_visible(&self, node_id: NodeId, shape: &SvgShape) -> bool {
        let etype = ElementType::from_shape(shape);

        // Check element-type toggle
        for &(t, _, vis) in &self.type_toggles {
            if t == etype {
                if !vis {
                    return false;
                }
                break;
            }
        }

        // For paths, also check the style-group toggle
        if matches!(shape, SvgShape::Path { .. }) {
            if let Some(&idx) = self.node_to_style.get(&node_id) {
                if !self.path_styles[idx].visible {
                    return false;
                }
            }
        }

        true
    }
}
