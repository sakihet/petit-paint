# petit-paint

A small paint app written in Rust with [GPUI](https://www.gpui.rs/).

It has only been tested on macOS.

![Screenshot](docs/screenshot.png)

## Features

- Tools: select, brush, eraser, eyedropper, fill, shape, text
- Undo and redo
- Copy, cut, paste and crop to selections
- Zoom
- Open PNG, JPEG, BMP, WebP and GIF images, also by dropping them on the window
- Save as PNG, JPEG, BMP, WebP or GIF, chosen by the file extension
- An MCP server, so coding agents can draw images with it

## Keyboard shortcuts

On Windows and Linux, use `Ctrl` in place of `Cmd`.

| Action | Shortcut |
| --- | --- |
| Select / Brush / Eraser / Eyedropper / Fill / Shape / Text | `V` / `B` / `E` / `I` / `G` / `U` / `T` |
| Swap color 1 and color 2 | `X` |
| Thinner / thicker brush | `[` / `]` |
| Undo | `Cmd+Z` |
| Redo | `Cmd+Shift+Z` (also `Ctrl+Y` on Windows and Linux) |
| New | `Cmd+N` |
| Open | `Cmd+O` |
| Save | `Cmd+S` |
| Save as | `Cmd+Shift+S` |
| Close window | `Cmd+W` |
| Select all | `Cmd+A` |
| Copy / Cut / Paste | `Cmd+C` / `Cmd+X` / `Cmd+V` |
| Delete selection | `Delete` / `Backspace` |
| Crop to selection | `Cmd+Shift+X` |
| Deselect | `Esc` |
| Finish polygon | `Enter` |
| Zoom in / out | `Cmd+=` / `Cmd+-` |
| Reset zoom | `Cmd+0` |
| Pan | Drag with `Space` held |

## Run

```sh
cargo run
```

## Drawing from coding agents (MCP)

`petit-paint mcp` runs an [MCP](https://modelcontextprotocol.io/) server over stdio instead of opening a window, so coding agents such as Claude Code can draw images with petit-paint.

To use it with Claude Code, install petit-paint and add the server:

```sh
cargo install --path .
claude mcp add petit-paint -- petit-paint mcp
```

Then start Claude Code again and ask it to draw, for example "Draw a sunset over the sea with petit-paint and save it as sunset.png". Relative paths are taken from the directory Claude Code runs in.

Images are kept in memory by id until they are saved. Coordinates are in image pixels from the top-left, and colors are `#rrggbb` or `#rrggbbaa`.

| Tool | What it does |
| --- | --- |
| `new_image` | Create a blank image, transparent or with a background color |
| `open_image` / `save_image` | Open or save a PNG, JPEG, BMP, WebP or GIF file |
| `view_image` | Return the image, or part of it scaled up, as PNG so the agent can look at it |
| `list_images` / `close_image` | List the open images, or close one to free its memory |
| `draw_stroke` | Draw a line through points with a brush |
| `draw_shape` | Draw a line, rectangle, ellipse, polygon, arc, pie or chord with an outline, a fill or both |
| `draw` | Draw many strokes, shapes and hatchings in one call, as one undo step |
| `hatch` | Lay many short strokes at random across an area, gathering away from the light to shade it |
| `fill` | Fill a region with a color or a pattern, like the fill tool |
| `filter` | Add grain, blur, or break shading into painterly patches, over the image or part of it |
| `draw_text` | Write text |
| `erase` / `clear` | Erase a rectangle or the whole image to the background |
| `resize` / `crop` / `flip` / `turn` | Change the image's size, shape or orientation |
| `pick_color` | Read the color of a pixel |
| `undo` / `redo` | Take back or redo changes, separately for each image |

Shapes can be filled with linear or radial gradients and rectangles can have rounded corners. Strokes and polygons can be drawn as smooth curves through their points. Any stroke or shape can be blended with what is under it (normal, add, screen or multiply) and clipped: to a rectangle, to a shape, or to the region around a pixel that has about the same color, keeping inside or outside it with soft edges. Masks combine with `all` and `any`, so one can be cut out of another, as for a crescent moon. `draw` can name masks once for many of its items to use.

To look drawn by hand: strokes can taper toward their ends and swell and thin with pressure at each point, outlines can be roughened, colors jittered from place to place, and edges blended with the smudge brush, which drags the colors already there.

## License

MIT

Toolbar icons are from [Lucide](https://lucide.dev/), licensed under the ISC License.
