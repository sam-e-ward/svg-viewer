/// The in-memory representation of a parsed SVG document.
/// This is the single source of truth shared between the renderer,
/// spatial index, and the elements pane.

#[derive(Debug, Clone, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const NONE: Color = Color { r: 0, g: 0, b: 0, a: 0 };
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };

    pub fn from_rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color { r, g, b, a }
    }

    pub fn to_egui(&self) -> egui::Color32 {
        egui::Color32::from_rgba_unmultiplied(self.r, self.g, self.b, self.a)
    }
}

#[derive(Debug, Clone)]
pub struct Transform {
    /// Column-major 2D affine matrix: [a, b, c, d, e, f]
    /// | a  c  e |
    /// | b  d  f |
    /// | 0  0  1 |
    pub matrix: [f32; 6],
}

impl Transform {
    pub fn identity() -> Self {
        Transform { matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0] }
    }

    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.matrix;
        (a * x + c * y + e, b * x + d * y + f)
    }

    pub fn concat(&self, other: &Transform) -> Transform {
        let [a1, b1, c1, d1, e1, f1] = self.matrix;
        let [a2, b2, c2, d2, e2, f2] = other.matrix;
        Transform {
            matrix: [
                a1 * a2 + c1 * b2,
                b1 * a2 + d1 * b2,
                a1 * c2 + c1 * d2,
                b1 * c2 + d1 * d2,
                a1 * e2 + c1 * f2 + e1,
                b1 * e2 + d1 * f2 + f1,
            ],
        }
    }
}

#[derive(Debug, Clone)]
pub enum Paint {
    None,
    Color(Color),
    LinearGradient(String), // references a def id
    RadialGradient(String),
}

#[derive(Debug, Clone)]
pub struct Style {
    pub fill: Paint,
    pub fill_opacity: f32,
    pub stroke: Paint,
    pub stroke_width: f32,
    pub stroke_opacity: f32,
    pub opacity: f32,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            fill: Paint::Color(Color::BLACK),
            fill_opacity: 1.0,
            stroke: Paint::None,
            stroke_width: 1.0,
            stroke_opacity: 1.0,
            opacity: 1.0,
        }
    }
}

/// Unique identifier for each node in the document tree.
/// Index into SvgDocument::nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

/// A single run of text within a <text> or <tspan> element.
#[derive(Debug, Clone)]
pub struct TextSpan {
    /// Absolute x position, if specified (None = continue from previous span)
    pub x: Option<f32>,
    /// Absolute y position, if specified (None = inherit from parent)
    pub y: Option<f32>,
    /// Relative x offset
    pub dx: f32,
    /// Relative y offset
    pub dy: f32,
    pub content: String,
    pub font_size: Option<f32>,
    pub fill: Option<Paint>,
    pub font_weight: FontWeight,
    pub font_style: FontStyle,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FontWeight {
    Normal,
    Bold,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FontStyle {
    Normal,
    Italic,
}

/// Decoded image data (RGBA8, row-major).
#[derive(Debug, Clone)]
pub struct ImagePixels {
    pub width: u32,
    pub height: u32,
    /// Raw RGBA8 bytes, length = width * height * 4
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum SvgShape {
    Rect {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        rx: f32,
        ry: f32,
    },
    Circle {
        cx: f32,
        cy: f32,
        r: f32,
    },
    Ellipse {
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
    },
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    },
    Polyline {
        points: Vec<(f32, f32)>,
    },
    Polygon {
        points: Vec<(f32, f32)>,
    },
    Path {
        data: String,
    },
    Text {
        /// Anchor position from the <text> element itself
        x: f32,
        y: f32,
        /// Flattened list of spans (direct text + tspan children)
        spans: Vec<TextSpan>,
        font_size: f32,
    },
    Image {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        /// Original href value (kept for display in elements pane)
        href: String,
        /// Decoded pixel data ready to upload as a texture, if available
        pixels: Option<ImagePixels>,
    },
    Use {
        x: f32,
        y: f32,
        href: String,
    },
}

#[derive(Debug, Clone)]
pub enum SvgNodeKind {
    /// Root <svg> element
    Svg {
        width: f32,
        height: f32,
        view_box: Option<[f32; 4]>,
    },
    /// <g> group element
    Group,
    /// A leaf shape element
    Shape(SvgShape),
    /// <defs> container
    Defs,
    /// <clipPath>
    ClipPath { id: String },
    /// <mask>
    Mask { id: String },
    /// <linearGradient>
    LinearGradient {
        id: String,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    },
    /// <radialGradient>
    RadialGradient {
        id: String,
        cx: f32,
        cy: f32,
        r: f32,
    },
    /// Unknown/unsupported element we preserve for the tree view
    Unknown { tag: String },
}

#[derive(Debug, Clone)]
pub struct SvgNode {
    pub id: NodeId,
    /// The element's `id` attribute if present
    pub svg_id: Option<String>,
    /// The element's `class` attribute if present
    pub class: Option<String>,
    /// Tag name as it appears in the source XML
    pub tag_name: String,
    pub kind: SvgNodeKind,
    pub style: Style,
    pub transform: Transform,
    /// clip-path attribute — references a def id
    pub clip_path: Option<String>,
    /// mask attribute — references a def id
    pub mask: Option<String>,
    pub children: Vec<NodeId>,
    pub parent: Option<NodeId>,
    /// A short human-readable attribute summary for display in the elements pane
    pub attr_summary: String,
}

/// The full parsed SVG document.
#[derive(Debug)]
pub struct SvgDocument {
    /// All nodes in document order (arena-style flat vec)
    pub nodes: Vec<SvgNode>,
    /// Root node id (always the outermost <svg>)
    pub root: NodeId,
    /// Canvas size in SVG user units
    pub width: f32,
    pub height: f32,
    /// viewBox if specified
    pub view_box: Option<[f32; 4]>,
}

impl SvgDocument {
    pub fn get(&self, id: NodeId) -> &SvgNode {
        &self.nodes[id.0]
    }
}
