## [0.1.0] - 2026-11-09

### 🚀 Features

- Add gpui paint app skeleton
- Add undo
- Add redo
- Add eraser tool
- Add PNG export
- Use round caps and joins for strokes
- Add eyedropper tool
- Add fill tool
- Give the canvas a fixed size
- Export PNG at the canvas size
- Add canvas size dialog
- Resize the canvas by dragging its edges
- Add status bar with canvas size and cursor position
- Add select tool
- Add canvas zoom
- Add shape tool
- Add text tool
- Add brush types
- Start with a transparent canvas
- Open PNG files
- Warn before discarding unsaved changes
- Add pixel and tile grids
- Add color 2 and a palette bar
- Flip and rotate images and selections
- Register gpui-kit's icon assets
- Reorganize the toolbar with icons
- Add finer zoom levels

### 🐛 Bug Fixes

- Add png extension when saving

### 📚 Documentation

- License under MIT
- Add commit message guidelines to AGENTS.md
- Import AGENTS.md from CLAUDE.md
- Add readme
- Add keyboard shortcuts to readme
- Note that the app is only tested on macos
- Add screenshot to readme
- Add package metadata and icon credits

### 🚜 Refactor

- Migrate to gpui-kit
- Store drawing in canvas coordinates
- Store the drawing as pixels
- Move tool definitions into tool.rs
- Move canvas view painting into view.rs
- Move zoom handling into zoom.rs
- Move selection handling into selection.rs
- Move opening and saving into file.rs
- Split rendering into ui modules
- Fix clippy warnings

### 🎨 Styling

- Format with rustfmt

### ⚙️ Miscellaneous Tasks

- Ignore TODO.txt
- Add rustfmt and clippy to toolchain components
