# SVG Viewer

A high-performance SVG inspection tool for macOS. Open large SVG files and explore their element tree with real-time hover-to-find, without the slowness of browser DevTools.

## Requirements

- macOS (Apple Silicon or Intel)
- [Rust](https://rustup.rs) 1.94 or later

## Installation

### 1. Install Rust

If you don't have Rust installed:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then restart your terminal (or run `source "$HOME/.cargo/env"`).

### 2. Clone the repository

```sh
git clone <repo-url>
cd svg-viewer
```

### 3. Build and run

**Debug build** (faster to compile, slower to run):
```sh
cargo run
```

**Release build** (recommended for large SVG files):
```sh
cargo run --release
```

The release build takes longer to compile the first time but is significantly faster at runtime.

## Usage

- **Open a file**: Use `File → Open SVG...` or drag and drop an SVG onto the window
- **Zoom**: Scroll wheel (zooms toward the cursor)
- **Pan**: Click and drag in the viewer
- **Fit to window**: `File → Fit to window`
- **Inspect element**: Hold `Space` and hover over the SVG — the Elements pane scrolls to the element under the cursor
- **Highlight element**: Hold `Space` and hover over an entry in the Elements pane — it highlights in the viewer
- **Navigate tree**: Click any element in the Elements pane to select and highlight it; click group elements to expand/collapse

## Project status

This is an early-stage project. See [REQUIREMENTS.md](REQUIREMENTS.md) for the full feature roadmap.

Current milestone: **M5 — spatial index & hit-testing**.
