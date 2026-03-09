/// SVG XML parser — converts raw XML bytes into an SvgDocument.
/// Uses roxmltree for fast, zero-copy XML parsing.

use anyhow::{Context, Result};
use roxmltree::{Document, Node};
use base64::Engine;

use crate::svg_doc::*;

/// After parsing, attempt to load any external image hrefs (non-data-URI)
/// relative to the SVG file's directory.
pub fn resolve_external_images(doc: &mut SvgDocument, svg_dir: Option<&std::path::Path>) {
    let svg_dir = match svg_dir {
        Some(d) => d,
        None => return,
    };
    for node in &mut doc.nodes {
        if let SvgNodeKind::Shape(SvgShape::Image { href, pixels, .. }) = &mut node.kind {
            if pixels.is_none() && !href.is_empty() && !href.starts_with("data:") {
                let candidate = svg_dir.join(href.as_str());
                *pixels = load_image_file(&candidate);
            }
        }
    }
}

pub fn parse_svg(source: &str) -> Result<SvgDocument> {
    let doc = Document::parse(source).context("Failed to parse SVG XML")?;
    let root_elem = doc.root_element();

    // We build nodes into a flat arena vec. Each node gets a NodeId = its index.
    let mut nodes: Vec<SvgNode> = Vec::new();

    let root_id = build_node(&root_elem, None, &mut nodes);

    // Extract top-level dimensions
    let root_node = &nodes[root_id.0];
    let (width, height, view_box) = match &root_node.kind {
        SvgNodeKind::Svg { width, height, view_box } => (*width, *height, *view_box),
        _ => (800.0, 600.0, None),
    };

    Ok(SvgDocument {
        nodes,
        root: root_id,
        width,
        height,
        view_box,
    })
}

fn build_node(
    xml_node: &Node,
    parent: Option<NodeId>,
    nodes: &mut Vec<SvgNode>,
) -> NodeId {
    let id = NodeId(nodes.len());

    let tag_name = xml_node.tag_name().name().to_string();
    let svg_id = xml_node.attribute("id").map(str::to_string);
    let class = xml_node.attribute("class").map(str::to_string);

    let transform = parse_transform(xml_node.attribute("transform").unwrap_or(""));
    let style = parse_style(xml_node);
    let clip_path = xml_node.attribute("clip-path").map(|s| strip_url(s).to_string());
    let mask = xml_node.attribute("mask").map(|s| strip_url(s).to_string());

    let kind = parse_kind(xml_node, &tag_name);
    let attr_summary = build_attr_summary(xml_node, &tag_name);

    // Push a placeholder — we'll fill children after recursing
    nodes.push(SvgNode {
        id,
        svg_id,
        class,
        tag_name: tag_name.clone(),
        kind,
        style,
        transform,
        clip_path,
        mask,
        children: Vec::new(),
        parent,
        attr_summary,
    });

    let child_ids: Vec<NodeId> = xml_node
        .children()
        .filter(|n| n.is_element())
        .map(|child| build_node(&child, Some(id), nodes))
        .collect();

    nodes[id.0].children = child_ids;

    id
}

fn parse_kind(node: &Node, tag: &str) -> SvgNodeKind {
    match tag {
        "svg" => {
            let width = parse_length_attr(node, "width").unwrap_or(800.0);
            let height = parse_length_attr(node, "height").unwrap_or(600.0);
            let view_box = node.attribute("viewBox").and_then(parse_view_box);
            SvgNodeKind::Svg { width, height, view_box }
        }
        "g" | "a" | "symbol" => SvgNodeKind::Group,
        "defs" => SvgNodeKind::Defs,
        "clipPath" => SvgNodeKind::ClipPath {
            id: node.attribute("id").unwrap_or("").to_string(),
        },
        "mask" => SvgNodeKind::Mask {
            id: node.attribute("id").unwrap_or("").to_string(),
        },
        "linearGradient" => SvgNodeKind::LinearGradient {
            id: node.attribute("id").unwrap_or("").to_string(),
            x1: parse_length_attr(node, "x1").unwrap_or(0.0),
            y1: parse_length_attr(node, "y1").unwrap_or(0.0),
            x2: parse_length_attr(node, "x2").unwrap_or(1.0),
            y2: parse_length_attr(node, "y2").unwrap_or(0.0),
        },
        "radialGradient" => SvgNodeKind::RadialGradient {
            id: node.attribute("id").unwrap_or("").to_string(),
            cx: parse_length_attr(node, "cx").unwrap_or(0.5),
            cy: parse_length_attr(node, "cy").unwrap_or(0.5),
            r: parse_length_attr(node, "r").unwrap_or(0.5),
        },
        "rect" => SvgNodeKind::Shape(SvgShape::Rect {
            x: parse_length_attr(node, "x").unwrap_or(0.0),
            y: parse_length_attr(node, "y").unwrap_or(0.0),
            width: parse_length_attr(node, "width").unwrap_or(0.0),
            height: parse_length_attr(node, "height").unwrap_or(0.0),
            rx: parse_length_attr(node, "rx").unwrap_or(0.0),
            ry: parse_length_attr(node, "ry").unwrap_or(0.0),
        }),
        "circle" => SvgNodeKind::Shape(SvgShape::Circle {
            cx: parse_length_attr(node, "cx").unwrap_or(0.0),
            cy: parse_length_attr(node, "cy").unwrap_or(0.0),
            r: parse_length_attr(node, "r").unwrap_or(0.0),
        }),
        "ellipse" => SvgNodeKind::Shape(SvgShape::Ellipse {
            cx: parse_length_attr(node, "cx").unwrap_or(0.0),
            cy: parse_length_attr(node, "cy").unwrap_or(0.0),
            rx: parse_length_attr(node, "rx").unwrap_or(0.0),
            ry: parse_length_attr(node, "ry").unwrap_or(0.0),
        }),
        "line" => SvgNodeKind::Shape(SvgShape::Line {
            x1: parse_length_attr(node, "x1").unwrap_or(0.0),
            y1: parse_length_attr(node, "y1").unwrap_or(0.0),
            x2: parse_length_attr(node, "x2").unwrap_or(0.0),
            y2: parse_length_attr(node, "y2").unwrap_or(0.0),
        }),
        "polyline" => SvgNodeKind::Shape(SvgShape::Polyline {
            points: parse_points(node.attribute("points").unwrap_or("")),
        }),
        "polygon" => SvgNodeKind::Shape(SvgShape::Polygon {
            points: parse_points(node.attribute("points").unwrap_or("")),
        }),
        "path" => SvgNodeKind::Shape(SvgShape::Path {
            data: node.attribute("d").unwrap_or("").to_string(),
        }),
        "text" => {
            let x = parse_length_attr(node, "x").unwrap_or(0.0);
            let y = parse_length_attr(node, "y").unwrap_or(0.0);
            let font_size = parse_font_size(node).unwrap_or(16.0);
            let mut spans = Vec::new();
            collect_text_spans(node, font_size, None, None, &mut spans);
            SvgNodeKind::Shape(SvgShape::Text { x, y, spans, font_size })
        }
        "image" => {
            let x = parse_length_attr(node, "x").unwrap_or(0.0);
            let y = parse_length_attr(node, "y").unwrap_or(0.0);
            let width = parse_length_attr(node, "width").unwrap_or(0.0);
            let height = parse_length_attr(node, "height").unwrap_or(0.0);
            let href = node
                .attribute("href")
                .or_else(|| node.attribute("xlink:href"))
                .unwrap_or("")
                .to_string();
            let pixels = decode_image_href(&href);
            SvgNodeKind::Shape(SvgShape::Image { x, y, width, height, href, pixels })
        }
        "use" => SvgNodeKind::Shape(SvgShape::Use {
            x: parse_length_attr(node, "x").unwrap_or(0.0),
            y: parse_length_attr(node, "y").unwrap_or(0.0),
            href: node
                .attribute("href")
                .or_else(|| node.attribute("xlink:href"))
                .unwrap_or("")
                .to_string(),
        }),
        _ => SvgNodeKind::Unknown { tag: tag.to_string() },
    }
}

// ---------------------------------------------------------------------------
// Style parsing
// ---------------------------------------------------------------------------

fn parse_style(node: &Node) -> Style {
    let mut style = Style::default();

    // Parse presentation attributes directly
    if let Some(v) = node.attribute("fill") {
        style.fill = parse_paint(v);
    }
    if let Some(v) = node.attribute("fill-opacity") {
        style.fill_opacity = v.trim().parse().unwrap_or(1.0);
    }
    if let Some(v) = node.attribute("stroke") {
        style.stroke = parse_paint(v);
    }
    if let Some(v) = node.attribute("stroke-width") {
        style.stroke_width = parse_length(v).unwrap_or(1.0);
    }
    if let Some(v) = node.attribute("stroke-opacity") {
        style.stroke_opacity = v.trim().parse().unwrap_or(1.0);
    }
    if let Some(v) = node.attribute("opacity") {
        style.opacity = v.trim().parse().unwrap_or(1.0);
    }

    // Override with inline style= attribute (simple key:value pairs only)
    if let Some(style_str) = node.attribute("style") {
        for decl in style_str.split(';') {
            let parts: Vec<&str> = decl.splitn(2, ':').collect();
            if parts.len() != 2 {
                continue;
            }
            let prop = parts[0].trim();
            let val = parts[1].trim();
            match prop {
                "fill" => style.fill = parse_paint(val),
                "fill-opacity" => style.fill_opacity = val.parse().unwrap_or(1.0),
                "stroke" => style.stroke = parse_paint(val),
                "stroke-width" => style.stroke_width = parse_length(val).unwrap_or(1.0),
                "stroke-opacity" => style.stroke_opacity = val.parse().unwrap_or(1.0),
                "opacity" => style.opacity = val.parse().unwrap_or(1.0),
                _ => {}
            }
        }
    }

    style
}

fn parse_paint(value: &str) -> Paint {
    let v = value.trim();
    match v {
        "none" | "transparent" => Paint::None,
        s if s.starts_with("url(#") => {
            let id = s
                .trim_start_matches("url(#")
                .trim_end_matches(')')
                .to_string();
            // We don't know if it's linear/radial at this point; renderer will resolve
            Paint::LinearGradient(id)
        }
        s => {
            if let Some(c) = parse_color(s) {
                Paint::Color(c)
            } else {
                Paint::None
            }
        }
    }
}

pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.starts_with('#') {
        parse_hex_color(s)
    } else if s.starts_with("rgb(") {
        parse_rgb_color(s)
    } else {
        named_color(s)
    }
}

fn parse_hex_color(s: &str) -> Option<Color> {
    let hex = s.trim_start_matches('#');
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?;
            Some(Color::from_rgba(r, g, b, 255))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color::from_rgba(r, g, b, 255))
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(Color::from_rgba(r, g, b, a))
        }
        _ => None,
    }
}

fn parse_rgb_color(s: &str) -> Option<Color> {
    let inner = s.trim_start_matches("rgb(").trim_end_matches(')');
    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    let parse_component = |p: &str| -> Option<u8> {
        let p = p.trim();
        if p.ends_with('%') {
            let pct: f32 = p.trim_end_matches('%').parse().ok()?;
            Some((pct / 100.0 * 255.0).round() as u8)
        } else {
            p.parse().ok()
        }
    };
    Some(Color::from_rgba(
        parse_component(parts[0])?,
        parse_component(parts[1])?,
        parse_component(parts[2])?,
        255,
    ))
}

fn named_color(name: &str) -> Option<Color> {
    // A selection of the most common SVG named colors
    match name {
        "black" => Some(Color::from_rgba(0, 0, 0, 255)),
        "white" => Some(Color::from_rgba(255, 255, 255, 255)),
        "red" => Some(Color::from_rgba(255, 0, 0, 255)),
        "green" => Some(Color::from_rgba(0, 128, 0, 255)),
        "blue" => Some(Color::from_rgba(0, 0, 255, 255)),
        "yellow" => Some(Color::from_rgba(255, 255, 0, 255)),
        "orange" => Some(Color::from_rgba(255, 165, 0, 255)),
        "purple" => Some(Color::from_rgba(128, 0, 128, 255)),
        "pink" => Some(Color::from_rgba(255, 192, 203, 255)),
        "gray" | "grey" => Some(Color::from_rgba(128, 128, 128, 255)),
        "lightgray" | "lightgrey" => Some(Color::from_rgba(211, 211, 211, 255)),
        "darkgray" | "darkgrey" => Some(Color::from_rgba(169, 169, 169, 255)),
        "cyan" => Some(Color::from_rgba(0, 255, 255, 255)),
        "magenta" | "fuchsia" => Some(Color::from_rgba(255, 0, 255, 255)),
        "lime" => Some(Color::from_rgba(0, 255, 0, 255)),
        "navy" => Some(Color::from_rgba(0, 0, 128, 255)),
        "teal" => Some(Color::from_rgba(0, 128, 128, 255)),
        "maroon" => Some(Color::from_rgba(128, 0, 0, 255)),
        "silver" => Some(Color::from_rgba(192, 192, 192, 255)),
        "gold" => Some(Color::from_rgba(255, 215, 0, 255)),
        "coral" => Some(Color::from_rgba(255, 127, 80, 255)),
        "salmon" => Some(Color::from_rgba(250, 128, 114, 255)),
        "khaki" => Some(Color::from_rgba(240, 230, 140, 255)),
        "indigo" => Some(Color::from_rgba(75, 0, 130, 255)),
        "violet" => Some(Color::from_rgba(238, 130, 238, 255)),
        "brown" => Some(Color::from_rgba(165, 42, 42, 255)),
        "beige" => Some(Color::from_rgba(245, 245, 220, 255)),
        "turquoise" => Some(Color::from_rgba(64, 224, 208, 255)),
        "transparent" => Some(Color::from_rgba(0, 0, 0, 0)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Transform parsing
// ---------------------------------------------------------------------------

pub fn parse_transform(s: &str) -> Transform {
    let s = s.trim();
    if s.is_empty() {
        return Transform::identity();
    }

    let mut result = Transform::identity();

    // Split on ')' boundaries to handle chained transforms
    let mut rest = s;
    while let Some(paren_open) = rest.find('(') {
        let func = rest[..paren_open].trim().to_lowercase();
        let after_open = &rest[paren_open + 1..];
        let paren_close = match after_open.find(')') {
            Some(i) => i,
            None => break,
        };
        let args_str = &after_open[..paren_close];
        let args: Vec<f32> = args_str
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();

        let t = match func.as_str() {
            "translate" => {
                let tx = args.first().copied().unwrap_or(0.0);
                let ty = args.get(1).copied().unwrap_or(0.0);
                Transform { matrix: [1.0, 0.0, 0.0, 1.0, tx, ty] }
            }
            "scale" => {
                let sx = args.first().copied().unwrap_or(1.0);
                let sy = args.get(1).copied().unwrap_or(sx);
                Transform { matrix: [sx, 0.0, 0.0, sy, 0.0, 0.0] }
            }
            "rotate" => {
                let angle = args.first().copied().unwrap_or(0.0).to_radians();
                let cx = args.get(1).copied().unwrap_or(0.0);
                let cy = args.get(2).copied().unwrap_or(0.0);
                let cos = angle.cos();
                let sin = angle.sin();
                // Rotate around (cx, cy)
                Transform {
                    matrix: [
                        cos,
                        sin,
                        -sin,
                        cos,
                        cx - cx * cos + cy * sin,
                        cy - cx * sin - cy * cos,
                    ],
                }
            }
            "skewx" => {
                let angle = args.first().copied().unwrap_or(0.0).to_radians();
                Transform { matrix: [1.0, 0.0, angle.tan(), 1.0, 0.0, 0.0] }
            }
            "skewy" => {
                let angle = args.first().copied().unwrap_or(0.0).to_radians();
                Transform { matrix: [1.0, angle.tan(), 0.0, 1.0, 0.0, 0.0] }
            }
            "matrix" if args.len() >= 6 => Transform {
                matrix: [args[0], args[1], args[2], args[3], args[4], args[5]],
            },
            _ => Transform::identity(),
        };

        result = result.concat(&t);
        rest = &after_open[paren_close + 1..];
    }

    result
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_length_attr(node: &Node, attr: &str) -> Option<f32> {
    node.attribute(attr).and_then(|v| parse_length(v))
}

pub fn parse_length(s: &str) -> Option<f32> {
    let s = s.trim();
    // Strip common units — we treat everything as px/user units for now
    let s = s
        .trim_end_matches("px")
        .trim_end_matches("pt")
        .trim_end_matches("em")
        .trim_end_matches("rem")
        .trim_end_matches('%');
    s.parse().ok()
}

fn parse_view_box(s: &str) -> Option<[f32; 4]> {
    let nums: Vec<f32> = s
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() >= 4 {
        Some([nums[0], nums[1], nums[2], nums[3]])
    } else {
        None
    }
}

fn parse_points(s: &str) -> Vec<(f32, f32)> {
    let nums: Vec<f32> = s
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    nums.chunks_exact(2).map(|c| (c[0], c[1])).collect()
}

fn strip_url(s: &str) -> &str {
    s.trim()
        .trim_start_matches("url(#")
        .trim_start_matches("url(")
        .trim_end_matches(')')
        .trim_start_matches('#')
}

fn build_attr_summary(node: &Node, tag: &str) -> String {
    let mut parts = Vec::new();

    // Always include id if present
    if let Some(id) = node.attribute("id") {
        parts.push(format!("#{id}"));
    }
    if let Some(class) = node.attribute("class") {
        parts.push(format!(".{}", class.split_whitespace().next().unwrap_or(class)));
    }

    // Tag-specific key attributes
    match tag {
        "rect" => {
            if let Some(w) = node.attribute("width") {
                if let Some(h) = node.attribute("height") {
                    parts.push(format!("{w}×{h}"));
                }
            }
        }
        "circle" => {
            if let Some(r) = node.attribute("r") {
                parts.push(format!("r={r}"));
            }
        }
        "path" => {
            if let Some(d) = node.attribute("d") {
                let snippet: String = d.chars().take(24).collect();
                parts.push(format!("d=\"{snippet}…\""));
            }
        }
        "text" => {
            if let Some(t) = node.text() {
                let snippet: String = t.chars().take(20).collect();
                parts.push(format!("\"{snippet}\""));
            }
        }
        "use" => {
            let href = node.attribute("href").or_else(|| node.attribute("xlink:href")).unwrap_or("");
            parts.push(format!("→{href}"));
        }
        _ => {}
    }

    parts.join(" ")
}

// ---------------------------------------------------------------------------
// Text span collection
// ---------------------------------------------------------------------------

/// Recursively collect text runs from a <text> or <tspan> node.
/// `parent_font_size` is the inherited size; `parent_x/y` are the inherited
/// absolute positions so that a bare <tspan dy="1.2em"> still lands somewhere.
fn collect_text_spans(
    node: &Node,
    parent_font_size: f32,
    parent_x: Option<f32>,
    parent_y: Option<f32>,
    spans: &mut Vec<TextSpan>,
) {
    let font_size = parse_font_size(node).unwrap_or(parent_font_size);

    // Resolve fill from this node's style/attributes
    let fill = {
        let mut f = None;
        if let Some(v) = node.attribute("fill") {
            f = Some(parse_paint(v));
        }
        if let Some(style_str) = node.attribute("style") {
            for decl in style_str.split(';') {
                let parts: Vec<&str> = decl.splitn(2, ':').collect();
                if parts.len() == 2 && parts[0].trim() == "fill" {
                    f = Some(parse_paint(parts[1].trim()));
                }
            }
        }
        f
    };

    let font_weight = parse_font_weight(node);
    let font_style_attr = parse_font_style_attr(node);

    // x/y on this node — if absent, inherit from parent
    let this_x = parse_length_attr(node, "x").or(parent_x);
    let this_y = parse_length_attr(node, "y").or(parent_y);
    let dx = parse_length_attr(node, "dx").unwrap_or(0.0);
    let dy = parse_length_attr(node, "dy").unwrap_or(0.0);

    // Walk child nodes: text nodes and <tspan> elements
    for child in node.children() {
        if child.is_text() {
            let text = child.text().unwrap_or("").trim();
            if !text.is_empty() {
                spans.push(TextSpan {
                    x: this_x,
                    y: this_y,
                    dx,
                    dy,
                    content: text.to_string(),
                    font_size: Some(font_size),
                    fill: fill.clone(),
                    font_weight: font_weight.clone(),
                    font_style: font_style_attr.clone(),
                });
            }
        } else if child.is_element() && child.tag_name().name() == "tspan" {
            collect_text_spans(&child, font_size, this_x, this_y, spans);
        }
    }

    // If no children produced spans but the node itself has direct text
    if spans.is_empty() {
        if let Some(text) = node.text() {
            let text = text.trim();
            if !text.is_empty() {
                spans.push(TextSpan {
                    x: this_x,
                    y: this_y,
                    dx,
                    dy,
                    content: text.to_string(),
                    font_size: Some(font_size),
                    fill: fill.clone(),
                    font_weight: font_weight.clone(),
                    font_style: font_style_attr.clone(),
                });
            }
        }
    }
}

fn parse_font_size(node: &Node) -> Option<f32> {
    // Check attribute first, then inline style
    if let Some(v) = node.attribute("font-size") {
        if let Some(f) = parse_length(v) {
            return Some(f);
        }
    }
    if let Some(style_str) = node.attribute("style") {
        for decl in style_str.split(';') {
            let parts: Vec<&str> = decl.splitn(2, ':').collect();
            if parts.len() == 2 && parts[0].trim() == "font-size" {
                if let Some(f) = parse_length(parts[1].trim()) {
                    return Some(f);
                }
            }
        }
    }
    None
}

fn parse_font_weight(node: &Node) -> FontWeight {
    let from_attr = node.attribute("font-weight").map(str::to_string);
    let from_style = style_prop(node, "font-weight");
    let val = from_attr.or(from_style).unwrap_or_default();
    match val.trim() {
        "bold" | "700" | "800" | "900" => FontWeight::Bold,
        _ => FontWeight::Normal,
    }
}

fn parse_font_style_attr(node: &Node) -> FontStyle {
    let from_attr = node.attribute("font-style").map(str::to_string);
    let from_style = style_prop(node, "font-style");
    let val = from_attr.or(from_style).unwrap_or_default();
    match val.trim() {
        "italic" | "oblique" => FontStyle::Italic,
        _ => FontStyle::Normal,
    }
}

/// Extract a single property value from an inline `style="..."` attribute.
fn style_prop(node: &Node, prop: &str) -> Option<String> {
    let style = node.attribute("style")?;
    for decl in style.split(';') {
        let mut parts = decl.splitn(2, ':');
        let key = parts.next()?.trim();
        let val = parts.next()?.trim();
        if key == prop {
            return Some(val.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Image decoding
// ---------------------------------------------------------------------------

/// Attempt to decode an image href:
/// - data:image/...;base64,<data>  → decode inline
/// - relative/absolute file paths  → load from disk (best-effort)
fn decode_image_href(href: &str) -> Option<ImagePixels> {
    if href.starts_with("data:") {
        decode_data_uri(href)
    } else if !href.is_empty() {
        // External file — attempt to load relative to cwd.
        // The app will also retry with the SVG file's directory at load time.
        load_image_file(std::path::Path::new(href))
    } else {
        None
    }
}

fn decode_data_uri(uri: &str) -> Option<ImagePixels> {
    // Format: data:<mediatype>;base64,<data>
    let rest = uri.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let meta = &rest[..comma];
    let data = &rest[comma + 1..];

    if !meta.contains("base64") {
        return None;
    }

    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
    decode_image_bytes(&bytes)
}

fn load_image_file(path: &std::path::Path) -> Option<ImagePixels> {
    let bytes = std::fs::read(path).ok()?;
    decode_image_bytes(&bytes)
}

fn decode_image_bytes(bytes: &[u8]) -> Option<ImagePixels> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some(ImagePixels {
        width,
        height,
        rgba: rgba.into_raw(),
    })
}

// ---------------------------------------------------------------------------
// Path command parser — shared with spatial_index for AABB + hit-test
// ---------------------------------------------------------------------------

use crate::spatial_index::{CmdKind, PathCmd};

/// Parse an SVG path `d` string into a flat list of abstract commands.
/// All coordinates are kept in local SVG space (no transform applied).
/// Relative commands are resolved to absolute coordinates here.
pub fn parse_path_to_commands(data: &str) -> Vec<PathCmd> {
    let tokens = tokenize_path_data(data);
    let mut cmds: Vec<PathCmd> = Vec::new();
    let mut i = 0;
    let mut current = (0.0f32, 0.0f32);
    let mut start = (0.0f32, 0.0f32);
    let mut last_ctrl: Option<(f32, f32)> = None;
    let mut in_subpath = false;

    while i < tokens.len() {
        let cmd = match &tokens[i] {
            PToken::Cmd(c) => { i += 1; *c }
            PToken::Num(_) => if in_subpath { 'L' } else { 'M' },
        };

        match cmd {
            'M' | 'm' => {
                let rel = cmd == 'm';
                let x = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                let y = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                let pt = if rel { (current.0 + x, current.1 + y) } else { (x, y) };
                cmds.push(PathCmd { kind: CmdKind::Move, points: vec![pt], is_move: true });
                current = pt;
                start = pt;
                in_subpath = true;
                last_ctrl = None;
                // Implicit lineto pairs
                while ppeek(&tokens, i) {
                    let x2 = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let y2 = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let pt2 = if rel { (current.0 + x2, current.1 + y2) } else { (x2, y2) };
                    cmds.push(PathCmd { kind: CmdKind::Line, points: vec![pt2], is_move: false });
                    current = pt2;
                    last_ctrl = None;
                }
            }
            'L' | 'l' => {
                let rel = cmd == 'l';
                while ppeek(&tokens, i) {
                    let x = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let y = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let pt = if rel { (current.0 + x, current.1 + y) } else { (x, y) };
                    cmds.push(PathCmd { kind: CmdKind::Line, points: vec![pt], is_move: false });
                    current = pt;
                    last_ctrl = None;
                }
            }
            'H' | 'h' => {
                let rel = cmd == 'h';
                while ppeek(&tokens, i) {
                    let x = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let nx = if rel { current.0 + x } else { x };
                    let pt = (nx, current.1);
                    cmds.push(PathCmd { kind: CmdKind::Line, points: vec![pt], is_move: false });
                    current = pt;
                    last_ctrl = None;
                }
            }
            'V' | 'v' => {
                let rel = cmd == 'v';
                while ppeek(&tokens, i) {
                    let y = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let ny = if rel { current.1 + y } else { y };
                    let pt = (current.0, ny);
                    cmds.push(PathCmd { kind: CmdKind::Line, points: vec![pt], is_move: false });
                    current = pt;
                    last_ctrl = None;
                }
            }
            'C' | 'c' => {
                let rel = cmd == 'c';
                while ppeek(&tokens, i) {
                    let x1 = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let y1 = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let x2 = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let y2 = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let x  = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let y  = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let (cp1, cp2, ep) = if rel {
                        ((current.0+x1, current.1+y1), (current.0+x2, current.1+y2), (current.0+x, current.1+y))
                    } else {
                        ((x1,y1),(x2,y2),(x,y))
                    };
                    cmds.push(PathCmd { kind: CmdKind::Cubic, points: vec![cp1, cp2, ep], is_move: false });
                    last_ctrl = Some(cp2);
                    current = ep;
                }
            }
            'S' | 's' => {
                let rel = cmd == 's';
                while ppeek(&tokens, i) {
                    let x2 = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let y2 = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let x  = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let y  = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let (cp2, ep) = if rel {
                        ((current.0+x2, current.1+y2), (current.0+x, current.1+y))
                    } else {
                        ((x2,y2),(x,y))
                    };
                    let cp1 = match last_ctrl {
                        Some(lc) => (2.0*current.0 - lc.0, 2.0*current.1 - lc.1),
                        None => current,
                    };
                    cmds.push(PathCmd { kind: CmdKind::Cubic, points: vec![cp1, cp2, ep], is_move: false });
                    last_ctrl = Some(cp2);
                    current = ep;
                }
            }
            'Q' | 'q' => {
                let rel = cmd == 'q';
                while ppeek(&tokens, i) {
                    let x1 = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let y1 = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let x  = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let y  = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let (cp, ep) = if rel {
                        ((current.0+x1, current.1+y1), (current.0+x, current.1+y))
                    } else {
                        ((x1,y1),(x,y))
                    };
                    cmds.push(PathCmd { kind: CmdKind::Quadratic, points: vec![cp, ep], is_move: false });
                    last_ctrl = Some(cp);
                    current = ep;
                }
            }
            'T' | 't' => {
                let rel = cmd == 't';
                while ppeek(&tokens, i) {
                    let x = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let y = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let ep = if rel { (current.0+x, current.1+y) } else { (x,y) };
                    let cp = match last_ctrl {
                        Some(lc) => (2.0*current.0 - lc.0, 2.0*current.1 - lc.1),
                        None => current,
                    };
                    cmds.push(PathCmd { kind: CmdKind::Quadratic, points: vec![cp, ep], is_move: false });
                    last_ctrl = Some(cp);
                    current = ep;
                }
            }
            'A' | 'a' => {
                // Arc approximated as a line to endpoint
                let rel = cmd == 'a';
                while ppeek(&tokens, i) {
                    let _rx = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let _ry = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let _xr = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let _la = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let _sw = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let x  = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let y  = match pnext(&tokens, &mut i) { Some(v) => v, None => break };
                    let ep = if rel { (current.0+x, current.1+y) } else { (x,y) };
                    cmds.push(PathCmd { kind: CmdKind::Line, points: vec![ep], is_move: false });
                    current = ep;
                    last_ctrl = None;
                }
            }
            'Z' | 'z' => {
                cmds.push(PathCmd { kind: CmdKind::Close, points: vec![], is_move: false });
                current = start;
                in_subpath = false;
                last_ctrl = None;
            }
            _ => {}
        }
    }

    cmds
}

#[derive(Debug, Clone)]
enum PToken {
    Cmd(char),
    Num(f32),
}

fn tokenize_path_data(data: &str) -> Vec<PToken> {
    let mut tokens = Vec::new();
    let mut chars = data.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\n' | '\r' | ',' => { chars.next(); }
            'A'..='Z' | 'a'..='z' => {
                tokens.push(PToken::Cmd(c));
                chars.next();
            }
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
                if let Ok(n) = s.parse::<f32>() {
                    tokens.push(PToken::Num(n));
                }
            }
            _ => { chars.next(); }
        }
    }
    tokens
}

fn pnext(tokens: &[PToken], i: &mut usize) -> Option<f32> {
    if let Some(PToken::Num(n)) = tokens.get(*i) {
        *i += 1;
        Some(*n)
    } else {
        None
    }
}

fn ppeek(tokens: &[PToken], i: usize) -> bool {
    matches!(tokens.get(i), Some(PToken::Num(_)))
}
