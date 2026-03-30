# SVG Viewer

A high-performance SVG inspection tool for macOS. Open large SVG files and explore their element tree with real-time hover-to-find, without the slowness of browser DevTools.

## Features

- **Fast rendering** — GPU-accelerated via wgpu with vertex budgets for large files
- **Element tree** — virtualised scrolling pane showing the full SVG structure
- **Spacebar inspect** — hold Space and hover to find elements in the tree or highlight them on canvas
- **Filter pane** — toggle visibility by element type and path style (fill/stroke)
- **Auto-filtering** — large SVGs automatically filter to keep the UI responsive
- **Open from file or URL** — drag-and-drop, file dialog, or paste a URL
- **macOS app bundle** — builds as a native `.app` with icon

## Requirements

- macOS 12+ (Apple Silicon or Intel)
- [Rust](https://rustup.rs) 1.75 or later
- `cargo-bundle` for `.app` packaging: `cargo install cargo-bundle`

## Quick start

```sh
git clone https://github.com/niceguydave/svg-viewer.git
cd svg-viewer
cargo run --release
```

## Building an app bundle

The Makefile handles version bumping, building, bundling, and tagging:

```sh
make bundle              # Build .app at the current version
make bundle-patch        # Bump patch, build, bundle, tag  (0.3.0 → 0.3.1)
make bundle-minor        # Bump minor, build, bundle, tag  (0.3.0 → 0.4.0)
make bundle-major        # Bump major, build, bundle, tag  (0.3.0 → 1.0.0)
```

The bundled app lands in `dist/SVG Viewer.app`.

## Usage

| Action | How |
|---|---|
| Open a file | `File → Open SVG…`, drag-and-drop, or `File → Open URL…` |
| Zoom | Scroll wheel (zooms toward cursor) |
| Pan | Click and drag on the canvas |
| Fit to window | `File → Fit to window` |
| Inspect element | Hold `Space` + hover over the SVG |
| Highlight element | Hold `Space` + hover over an entry in the Elements pane |
| Expand/collapse group | Click a group in the Elements pane |

## Licence

Copyright © 2026 Sam Ward
