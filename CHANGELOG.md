## [0.3.0] - 2026-11-14

### 🚀 Features

- Add crayon and natural pencil brushes
- Add pattern fills to the fill tool
- Preview fill patterns at the canvas zoom
- Keep brush strokes straight while shift is held
- Add free-form selection
- Add an mcp subcommand that serves over stdio
- Add mcp tools to create, open, save and view images
- Add mcp tools to draw strokes and shapes
- Add an mcp draw tool for many strokes and shapes at once
- Add mcp tools to fill, write text, and edit and read images
- Add smooth curves, rounded rectangles and arcs to mcp shapes
- Blend and clip mcp strokes and shapes
- Fill mcp shapes with gradients
- Let view_image show part of an image scaled up
- Clip mcp strokes and shapes to shape and region masks
- Combine mcp masks with all and any
- Add an oil paint brush
- Add an mcp filter tool for noise, blur and painterly patches
- Taper mcp strokes and vary their width with pressure
- Roughen mcp strokes and shapes
- Jitter the colors of mcp strokes and shapes
- Add a smudge brush to the mcp server
- Add an mcp hatch tool
- Add a watercolor brush
- Add an mcp adjust tool for hue, saturation, brightness, contrast and warmth

### 🐛 Bug Fixes

- Name the file when an image cannot be read or written

### 📚 Documentation

- Describe the mcp server in the readme
- Describe the richer mcp drawing options
- Describe masks in the readme
- Describe combining masks in the readme
- Describe the hand-drawn tools in the readme
- Describe the watercolor brush and the adjust tool
- Describe the v0.3 brushes, patterns, selection and shift in the readme
- Update the screenshot

### 🚜 Refactor

- Build strokes, shapes and text without a window
- Share undo history and image file io outside the window
- Share the font size range and fill tolerance with the mcp server

### 🧪 Testing

- Run the mcp server over stdio in an integration test
## [0.2.0] - 2026-11-10

### 🚀 Features

- Add a tolerance slider to the fill tool
- Save to the current file and add save as
- Add new image command
- Add single-key tool shortcuts
- Swap colors with x
- Change the brush width with brackets
- Open images dropped on the window
- Pan the canvas by dragging with space held
- Open jpeg, bmp, webp and gif images
- Save in the format named by the file extension
- Crop to selection

### 🐛 Bug Fixes

- Keep the palette bar on one row as recent colors are added

### 📚 Documentation

- Move the remember_color doc comment back to its function
- Add the release commit message to agents.md

### 🚜 Refactor

- Move the palette click hint into swatch tooltips

### ⚙️ Miscellaneous Tasks

- Add github actions workflow for build and test
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
