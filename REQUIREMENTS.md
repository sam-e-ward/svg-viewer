# SVG Viewer - Requirements

## Overview

A high-performance SVG viewing application for macOS, built with Rust. The primary design goal is **speed of element inspection** — specifically the ability to identify and navigate to SVG elements under the cursor, even in very large SVG files where browser DevTools are unusably slow.

## Platform

- **Target OS**: macOS
- **Tech Stack**: Rust, egui (immediate-mode GUI), custom SVG renderer

---

## Functional Requirements

### FR-1: File Loading

- Open an SVG file via a file picker or drag-and-drop
- Parse the SVG into an in-memory element tree
- Support large SVG files (target: 10MB+, stretch: 100MB+)

### FR-2: Split-Pane Layout

- The window is divided into two equal halves side-by-side
- **Left pane**: SVG Viewer — renders the SVG visually
- **Right pane**: Elements Pane — shows the SVG XML element tree as navigable text

### FR-3: SVG Viewer (Left Pane)

- Render the SVG using a custom Rust renderer (via egui's painter / wgpu)
- Support zoom (scroll wheel or pinch gesture) and pan (click-drag)
- Render the following SVG elements at minimum:
  - `<path>`
  - `<rect>`, `<circle>`, `<ellipse>`, `<line>`, `<polyline>`, `<polygon>`
  - `<g>` (group with transforms)
  - `<image>`
  - `<text>` (basic)
  - `<clipPath>`, `<mask>`
  - `<defs>`, `<use>` (symbol instancing)
  - `<svg>` (nested)
  - Transforms: `translate`, `rotate`, `scale`, `matrix`
  - Fill, stroke, opacity, basic gradients (`linearGradient`, `radialGradient`)
- Complex/rare SVG features may be stubbed or ignored to preserve performance

### FR-4: Elements Pane (Right Pane)

- Display the SVG XML tree in a collapsible, scrollable tree view
- Show element tag name, key attributes (id, class, fill, d snippet, etc.)
- Allow expanding/collapsing of group nodes (`<g>`, `<svg>`, etc.)
- Clicking an element in the tree highlights it in the viewer and scrolls/zooms to it

### FR-5: Spacebar — Viewer → Elements Navigation ("Hover to Find")

- While the **spacebar is held**:
  - Hovering the mouse over the SVG viewer performs hit-testing against the rendered element tree
  - The element under the cursor is identified (topmost painted element, respecting clip regions)
  - The Elements Pane auto-scrolls to and highlights that element
- Hit-testing must be fast enough to work in real time (target: <16ms per query on large files)
- Hit-testing strategy: spatial index (e.g. R-tree or bounding-box hierarchy) over bounding boxes, with fallback to precise path hit-testing for ambiguous cases

### FR-6: Spacebar — Elements → Viewer Highlighting ("Hover to Highlight")

- While the **spacebar is held**:
  - Hovering the mouse over an element in the Elements Pane highlights the corresponding element in the SVG viewer
  - Highlight is rendered as a coloured overlay/outline (e.g. semi-transparent blue fill or bright stroke) on top of the SVG

> Note: FR-5 and FR-6 are active simultaneously when spacebar is held. The active behaviour is determined by which pane the mouse is currently in.

---

## Non-Functional Requirements

### NFR-1: Performance

- Target 60fps rendering for SVGs up to ~5,000 elements
- Element hit-testing (FR-5) must complete in <16ms on SVGs with 10,000+ elements
- File parsing should not block the UI; use background thread/async loading
- Rendering should use GPU acceleration where possible (wgpu/Metal)

### NFR-2: Memory Efficiency

- Avoid cloning SVG element data unnecessarily
- Use arena allocation or Rc/Arc-based tree for the element tree
- Tessellated geometry should be cached after first render pass

### NFR-3: Correctness

- Rendering need not be pixel-perfect vs browser output
- Must correctly respect element stacking order for hit-testing
- Clip paths and masks must be accounted for in hit-testing (an element clipped away should not be hit-testable at the clipped region)

### NFR-4: Usability

- The app should feel snappy and responsive at all times
- File loading >500ms should show a progress indicator
- Element highlight in viewer should be immediately visible (bright, unambiguous overlay)

---

## Out of Scope (v1)

- CSS stylesheets (`<style>` blocks, external CSS) — inline styles only
- Animations (`<animate>`, SMIL)
- Filters (`<filter>`, `feGaussianBlur`, etc.)
- Full text layout (wrapping, `<tspan>` complex layout)
- Editing / saving SVGs
- Windows / Linux support (v1 macOS only)

---

## Architecture Notes

### Component Breakdown

| Component | Responsibility |
|-----------|---------------|
| `svg_parser` | Parse SVG XML into typed element tree (`SvgNode` enum) |
| `svg_renderer` | Tessellate paths, build draw lists, render via egui painter or wgpu |
| `spatial_index` | R-tree or BVH over element bounding boxes for fast hit-testing |
| `element_tree_ui` | egui widget: collapsible tree view of SVG elements |
| `app` | Top-level egui App, split pane layout, keyboard/mouse event handling |

### Hit-Testing Strategy

1. Maintain a **flattened, ordered list** of visible elements (painter's order, back to front)
2. Build an **R-tree** over element axis-aligned bounding boxes (recomputed on load/zoom)
3. On hover query:
   a. Query R-tree for candidate elements at cursor point
   b. For each candidate (front to back): test if point is inside the element's precise geometry
   c. Return the first match (topmost)
4. Result is cached per-frame; only recomputed if cursor moves

### Data Flow

```
SVG File
  └─► svg_parser → SvgDocument (element tree + defs)
        ├─► svg_renderer → tessellated mesh cache → GPU draw
        ├─► spatial_index → R-tree (bounding boxes in screen space)
        └─► element_tree_ui → egui tree widget
```

---

## Milestones

| Milestone | Description |
|-----------|-------------|
| M1 | Project scaffold: egui window, file open dialog, split pane layout |
| M2 | SVG parser: load file, build element tree, display in elements pane |
| M3 | Basic renderer: render shapes (rect, circle, path) in viewer pane |
| M4 | Zoom & pan in viewer |
| M5 | Spatial index + hit-testing; spacebar hover-to-find (FR-5) |
| M6 | Hover-to-highlight (FR-6); element click → scroll/zoom viewer |
| M7 | Expanded SVG support: clips, masks, images, gradients, use/defs |
| M8 | Performance pass: GPU rendering, large file testing, profiling |
