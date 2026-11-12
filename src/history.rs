use crate::canvas::{Canvas, Change};

/// Number of undo steps kept; older ones are dropped.
const MAX_HISTORY: usize = 100;

/// Undo and redo steps for one canvas.
#[derive(Default)]
pub struct History {
    undo: Vec<Change>,
    redo: Vec<Change>,
}

impl History {
    /// Records `undo`, the change that reverts an edit just made to the
    /// canvas. Anything that could be redone is dropped.
    pub fn record(&mut self, undo: Change) {
        self.undo.push(undo);
        if self.undo.len() > MAX_HISTORY {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Takes back the last edit to `canvas`. Returns whether there was one.
    pub fn undo(&mut self, canvas: &mut Canvas) -> bool {
        let Some(change) = self.undo.pop() else {
            return false;
        };
        self.redo.push(canvas.apply(change));
        true
    }

    /// Makes the last undone edit to `canvas` again. Returns whether there
    /// was one.
    pub fn redo(&mut self, canvas: &mut Canvas) -> bool {
        let Some(change) = self.redo.pop() else {
            return false;
        };
        self.undo.push(canvas.apply(change));
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::rgb;

    fn canvas() -> Canvas {
        Canvas::new(4, 4, rgb(0xffffff).into()).unwrap()
    }

    #[test]
    fn undoes_and_redoes_in_order() {
        let mut canvas = canvas();
        let mut history = History::default();
        let blank = canvas.pixels().clone();
        history.record(canvas.resize(8, 8).unwrap());
        history.record(canvas.resize(2, 2).unwrap());

        assert!(history.undo(&mut canvas));
        assert_eq!(canvas.pixels().width(), 8);
        assert!(history.undo(&mut canvas));
        assert_eq!(canvas.pixels().data(), blank.data());
        assert!(!history.undo(&mut canvas));

        assert!(history.redo(&mut canvas));
        assert_eq!(canvas.pixels().width(), 8);
    }

    #[test]
    fn a_new_edit_drops_what_could_be_redone() {
        let mut canvas = canvas();
        let mut history = History::default();
        history.record(canvas.resize(8, 8).unwrap());
        history.undo(&mut canvas);
        assert!(history.can_redo());
        history.record(canvas.resize(6, 6).unwrap());
        assert!(!history.can_redo());
        assert!(!history.redo(&mut canvas));
    }

    #[test]
    fn keeps_only_the_latest_steps() {
        let mut canvas = canvas();
        let mut history = History::default();
        for side in 1..=MAX_HISTORY as u32 + 5 {
            history.record(canvas.resize(side, side).unwrap());
        }
        let mut undone = 0;
        while history.undo(&mut canvas) {
            undone += 1;
        }
        assert_eq!(undone, MAX_HISTORY);
    }
}
