/// Large SVG element filtering.
///
/// When an SVG contains a huge number of elements (e.g. 300k+), tessellation
/// and rendering can exhaust memory or hang. This module analyses element styles,
/// identifies the dominant groups responsible for the bulk of the element count,
/// and marks a percentage of those elements as `filtered` so downstream stages
/// (spatial index, geometry cache, renderer) can skip them.
///
/// The user's observation: "usually there are one or two styles with 100,000s of
/// elements and the rest of the SVG is fine." We exploit this by only filtering
/// within the largest style groups.

use std::collections::HashMap;

use crate::svg_doc::{NodeId, Paint, SvgDocument, SvgNodeKind};

/// Maximum number of shape elements before filtering kicks in.
const SHAPE_BUDGET: usize = 80_000;

/// Minimum group size before we consider filtering it.
/// Small groups are never touched even if the total is over budget.
const MIN_GROUP_SIZE: usize = 1_000;

/// Summary of what was filtered, for display in the UI.
#[derive(Debug, Clone)]
pub struct FilterReport {
    /// Total shape elements before filtering
    pub total_shapes: usize,
    /// Number of elements marked as filtered
    pub filtered_count: usize,
    /// Per-group details: (signature description, original count, kept count)
    pub groups: Vec<(String, usize, usize)>,
}

/// A style signature used to group visually-identical elements.
/// We hash on tag + fill + stroke + stroke_width + class.
#[derive(Hash, Eq, PartialEq, Clone)]
struct StyleSignature {
    tag: String,
    fill: String,
    stroke: String,
    stroke_width_bits: u32,
    class: String,
}

impl StyleSignature {
    fn from_node(node: &crate::svg_doc::SvgNode) -> Self {
        StyleSignature {
            tag: node.tag_name.clone(),
            fill: paint_key(&node.style.fill),
            stroke: paint_key(&node.style.stroke),
            stroke_width_bits: node.style.stroke_width.to_bits(),
            class: node.class.clone().unwrap_or_default(),
        }
    }

    fn description(&self) -> String {
        let mut parts = vec![format!("<{}>", self.tag)];
        if !self.fill.is_empty() && self.fill != "none" {
            parts.push(format!("fill={}", self.fill));
        }
        if !self.stroke.is_empty() && self.stroke != "none" {
            parts.push(format!("stroke={}", self.stroke));
        }
        if !self.class.is_empty() {
            parts.push(format!(".{}", self.class));
        }
        parts.join(" ")
    }
}

fn paint_key(paint: &Paint) -> String {
    match paint {
        Paint::None => "none".to_string(),
        Paint::Color(c) => format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b),
        Paint::LinearGradient(id) => format!("lg({})", id),
        Paint::RadialGradient(id) => format!("rg({})", id),
    }
}

/// Analyse and filter a parsed SVG document in-place.
///
/// Returns `Some(report)` if filtering was applied, `None` if the document
/// was small enough to keep as-is.
pub fn filter_large_svg(doc: &mut SvgDocument) -> Option<FilterReport> {
    // Count shape nodes and group by style signature.
    let mut groups: HashMap<StyleSignature, Vec<NodeId>> = HashMap::new();
    let mut total_shapes = 0usize;

    for node in &doc.nodes {
        if matches!(&node.kind, SvgNodeKind::Shape(_)) {
            total_shapes += 1;
            let sig = StyleSignature::from_node(node);
            groups.entry(sig).or_default().push(node.id);
        }
    }

    if total_shapes <= SHAPE_BUDGET {
        log::info!(
            "filter_large_svg: {} shapes ≤ budget {}, no filtering needed",
            total_shapes, SHAPE_BUDGET
        );
        return None;
    }

    log::info!(
        "filter_large_svg: {} shapes exceeds budget {}; analysing {} style groups",
        total_shapes, SHAPE_BUDGET, groups.len()
    );

    // Sort groups by size descending.
    let mut sorted_groups: Vec<(StyleSignature, Vec<NodeId>)> = groups.into_iter().collect();
    sorted_groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    // Determine which groups to filter and by how much.
    // Strategy: only filter groups with >= MIN_GROUP_SIZE elements.
    // Reduce those groups proportionally until total is under budget.
    let small_group_total: usize = sorted_groups.iter()
        .filter(|(_, ids)| ids.len() < MIN_GROUP_SIZE)
        .map(|(_, ids)| ids.len())
        .sum();

    let large_groups: Vec<&(StyleSignature, Vec<NodeId>)> = sorted_groups.iter()
        .filter(|(_, ids)| ids.len() >= MIN_GROUP_SIZE)
        .collect();

    let large_group_total: usize = large_groups.iter().map(|(_, ids)| ids.len()).sum();

    // We need to reduce large_group_total so that small_group_total + reduced = SHAPE_BUDGET.
    let target_large = SHAPE_BUDGET.saturating_sub(small_group_total);
    let keep_fraction = if large_group_total > 0 {
        (target_large as f64 / large_group_total as f64).min(1.0)
    } else {
        1.0
    };

    log::info!(
        "filter_large_svg: {} in small groups (untouched), {} in {} large groups → keep {:.1}%",
        small_group_total, large_group_total, large_groups.len(), keep_fraction * 100.0
    );

    let mut filtered_count = 0usize;
    let mut report_groups = Vec::new();

    for (sig, ids) in &sorted_groups {
        if ids.len() < MIN_GROUP_SIZE {
            continue;
        }

        let keep_count = ((ids.len() as f64 * keep_fraction).ceil() as usize).max(1);
        let skip_count = ids.len() - keep_count;

        if skip_count == 0 {
            continue;
        }

        // Deterministic sampling: keep every Nth element to get even spatial distribution.
        // We mark elements to *filter out*, keeping `keep_count` and filtering the rest.
        let step = ids.len() as f64 / skip_count as f64;
        let mut next_filter = step / 2.0; // start offset for centering
        let mut filtered_in_group = 0usize;

        for (idx, &node_id) in ids.iter().enumerate() {
            if filtered_in_group >= skip_count {
                break;
            }
            if idx as f64 >= next_filter {
                doc.nodes[node_id.0].filtered = true;
                filtered_in_group += 1;
                next_filter += step;
            }
        }

        filtered_count += filtered_in_group;
        report_groups.push((
            sig.description(),
            ids.len(),
            ids.len() - filtered_in_group,
        ));

        log::info!(
            "  {} — {}/{} kept ({} filtered)",
            sig.description(), ids.len() - filtered_in_group, ids.len(), filtered_in_group
        );
    }

    // Remove filtered nodes from their parent's children lists.
    // This is the critical optimisation: without this, the render tree walk
    // would still call render_node() 1M+ times (returning immediately each time).
    // By pruning the tree, we eliminate the overhead entirely.
    let filtered_set: std::collections::HashSet<NodeId> = doc.nodes.iter()
        .filter(|n| n.filtered)
        .map(|n| n.id)
        .collect();

    for node in &mut doc.nodes {
        if !node.children.is_empty() {
            node.children.retain(|child_id| !filtered_set.contains(child_id));
        }
    }

    log::info!(
        "filter_large_svg: filtered {}/{} shapes, {} remaining (pruned from tree)",
        filtered_count, total_shapes, total_shapes - filtered_count
    );

    Some(FilterReport {
        total_shapes,
        filtered_count,
        groups: report_groups,
    })
}
