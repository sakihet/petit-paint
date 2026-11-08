use std::borrow::Cow;

use gpui_kit::assets::{Assets, icon_assets};
use gpui_kit::{AssetSource, Result, SharedString};

// Lucide icons the toolbar uses, embedded on their own rather than the whole
// catalog of almost two thousand
icon_assets!(
    ToolbarIcons,
    [
        SquareDashed,
        Paintbrush,
        Eraser,
        Pipette,
        PaintBucket,
        Shapes,
        Type,
        Pen,
        Pencil,
        Highlighter,
        SprayCan,
        PenTool,
        Slash,
        Square,
        Circle,
        Pentagon,
        Undo2,
        Redo2,
        FlipHorizontal2,
        FlipVertical2,
        RotateCcw,
        RotateCw,
        Scaling,
        Trash,
        FolderOpen,
        Save,
    ]
);

/// The toolbar's icons, plus gpui-kit's own for its components.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ToolbarIcons.load(path)? {
            return Ok(Some(bytes));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ToolbarIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
