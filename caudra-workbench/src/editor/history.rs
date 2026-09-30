//! Undo and redo, with the coalescing that makes them usable.
//!
//! One `Ctrl+Z` per keystroke is not undo, it is a nuisance. Consecutive edits
//! of the same kind that stay adjacent and land within [`COALESCE_WINDOW`] fold
//! into one entry. A newline, a cursor jump, a save or a pause breaks the run.

use std::time::{Duration, Instant};

use super::buffer::Edit;

const COALESCE_WINDOW: Duration = Duration::from_millis(400);
const MAX_ENTRIES: usize = 2000;

/// Backspace and forward delete look alike in the edit they record and merge in
/// opposite directions, so they are told apart by where the cursor ends up:
/// backspace walks it back over the text it removed, forward delete leaves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gesture {
    Insert,
    DeleteBack,
    DeleteForward,
    Other,
}

impl Gesture {
    fn of(edit: &Edit) -> Self {
        if edit.is_plain_insert() {
            Self::Insert
        } else if edit.is_plain_delete() {
            if edit.cursor_before == edit.cursor_after {
                Self::DeleteForward
            } else {
                Self::DeleteBack
            }
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    edit: Edit,
    gesture: Gesture,
    at: Instant,
    seq: u64,
}

#[derive(Debug, Clone, Default)]
pub struct History {
    undo: Vec<Entry>,
    redo: Vec<Edit>,
    /// Set by a save or a jump, so the next edit starts a fresh group even when
    /// it would otherwise have been folded into the last one.
    barrier: bool,
    next_seq: u64,
    /// The entry on top when the file was last written. Undoing back to it
    /// makes the buffer clean again, which a plain dirty flag cannot express.
    /// A sequence number rather than a depth, because trimming the oldest
    /// entries shifts depths and never touches the top.
    saved_seq: Option<u64>,
}

impl History {
    pub fn record(&mut self, edit: Edit) {
        self.redo.clear();
        let gesture = Gesture::of(&edit);
        let now = Instant::now();

        if !self.barrier
            && gesture != Gesture::Other
            && let Some(last) = self.undo.last_mut()
            && last.gesture == gesture
            && now.duration_since(last.at) < COALESCE_WINDOW
            && edit.cursor_before == last.edit.cursor_after
        {
            merge(&mut last.edit, &edit, gesture);
            last.at = now;
            return;
        }

        self.barrier = false;
        self.next_seq += 1;
        self.undo.push(Entry {
            edit,
            gesture,
            at: now,
            seq: self.next_seq,
        });
        if self.undo.len() > MAX_ENTRIES {
            self.undo.remove(0);
        }
    }

    pub fn mark_saved(&mut self) {
        self.saved_seq = self.undo.last().map(|entry| entry.seq);
        self.barrier = true;
    }

    pub fn is_saved(&self) -> bool {
        self.undo.last().map(|entry| entry.seq) == self.saved_seq
    }

    /// Ends the current group. A save is a landmark the user expects to undo
    /// back to, and a jump means the next edit is somewhere else entirely.
    pub fn break_group(&mut self) {
        self.barrier = true;
    }

    /// The edit to replay to undo, already inverted.
    pub fn undo(&mut self) -> Option<Edit> {
        let entry = self.undo.pop()?;
        self.barrier = true;
        self.redo.push(entry.edit.clone());
        Some(entry.edit.inverted())
    }

    pub fn redo(&mut self) -> Option<Edit> {
        let edit = self.redo.pop()?;
        self.barrier = true;
        self.next_seq += 1;
        self.undo.push(Entry {
            gesture: Gesture::of(&edit),
            at: Instant::now(),
            edit: edit.clone(),
            seq: self.next_seq,
        });
        Some(edit)
    }

    #[cfg(test)]
    fn depth(&self) -> usize {
        self.undo.len()
    }
}

/// Backspace grows the run leftwards, so the new text goes in front of what is
/// already recorded and the edit's anchor moves with it. The other two grow
/// rightwards and leave the anchor alone.
fn merge(last: &mut Edit, next: &Edit, gesture: Gesture) {
    match gesture {
        Gesture::Insert => {
            last.inserted.push_str(&next.inserted);
            last.cursor_after = next.cursor_after;
        }
        Gesture::DeleteBack => {
            last.removed.insert_str(0, &next.removed);
            last.at = next.at;
            last.cursor_after = next.cursor_after;
        }
        Gesture::DeleteForward => last.removed.push_str(&next.removed),
        Gesture::Other => {}
    }
}

#[cfg(test)]
mod tests {
    use super::super::buffer::Cursor;
    use super::{Edit, History};

    const ONE_GESTURE: &str = "a typed word must undo as one gesture, not one keystroke at a time";
    const SEPARATE: &str = "a newline must end the group so undo stops at line boundaries";
    const REDO_CLEARED: &str = "editing after an undo must drop the redo stack";
    const SAVE_BARRIER: &str = "a save must end the group so undo stops at what was saved";
    const CLEAN_AGAIN: &str =
        "a buffer undone back to what is on disk must stop claiming to be dirty";

    fn insert(line: usize, col: usize, text: &str) -> Edit {
        Edit {
            at: Cursor::new(line, col),
            removed: String::new(),
            inserted: text.to_owned(),
            cursor_before: Cursor::new(line, col),
            cursor_after: Cursor::new(line, col + text.chars().count()),
        }
    }

    fn backspace(line: usize, col: usize, text: &str) -> Edit {
        let width = text.chars().count();
        Edit {
            at: Cursor::new(line, col - width),
            removed: text.to_owned(),
            inserted: String::new(),
            cursor_before: Cursor::new(line, col),
            cursor_after: Cursor::new(line, col - width),
        }
    }

    #[test]
    fn typing_a_word_folds_into_one_entry() {
        let mut history = History::default();
        for (index, letter) in "hello".chars().enumerate() {
            history.record(insert(0, index, &letter.to_string()));
        }
        assert_eq!(history.depth(), 1, "{ONE_GESTURE}");

        let undo = history.undo().unwrap();
        assert_eq!(undo.removed, "hello", "{ONE_GESTURE}");
        assert_eq!(undo.cursor_after, Cursor::new(0, 0));
    }

    #[test]
    fn backspacing_a_word_folds_into_one_entry() {
        let mut history = History::default();
        for (offset, letter) in "olleh".chars().enumerate() {
            history.record(backspace(0, 5 - offset, &letter.to_string()));
        }
        assert_eq!(history.depth(), 1, "{ONE_GESTURE}");
        assert_eq!(history.undo().unwrap().inserted, "hello", "{ONE_GESTURE}");
    }

    #[test]
    fn a_newline_starts_a_new_entry() {
        let mut history = History::default();
        history.record(insert(0, 0, "a"));
        history.record(insert(0, 1, "\n"));
        history.record(insert(1, 0, "b"));
        assert_eq!(history.depth(), 3, "{SEPARATE}");
    }

    #[test]
    fn typing_somewhere_else_starts_a_new_entry() {
        let mut history = History::default();
        history.record(insert(0, 0, "a"));
        history.record(insert(5, 9, "b"));
        assert_eq!(history.depth(), 2, "{SEPARATE}");
    }

    #[test]
    fn switching_between_typing_and_deleting_starts_a_new_entry() {
        let mut history = History::default();
        history.record(insert(0, 0, "a"));
        history.record(backspace(0, 1, "a"));
        assert_eq!(history.depth(), 2, "{SEPARATE}");
    }

    #[test]
    fn a_save_ends_the_group() {
        let mut history = History::default();
        history.record(insert(0, 0, "a"));
        history.break_group();
        history.record(insert(0, 1, "b"));
        assert_eq!(history.depth(), 2, "{SAVE_BARRIER}");
    }

    #[test]
    fn undo_and_redo_walk_the_same_path() {
        let mut history = History::default();
        history.record(insert(0, 0, "first"));
        history.break_group();
        history.record(insert(1, 0, "second"));

        assert_eq!(history.undo().unwrap().removed, "second");
        assert_eq!(history.undo().unwrap().removed, "first");
        assert!(history.undo().is_none());

        assert_eq!(history.redo().unwrap().inserted, "first");
        assert_eq!(history.redo().unwrap().inserted, "second");
        assert!(history.redo().is_none());
    }

    #[test]
    fn editing_after_an_undo_drops_the_redo_stack() {
        let mut history = History::default();
        history.record(insert(0, 0, "first"));
        history.undo();

        history.record(insert(0, 0, "different"));
        assert!(history.redo().is_none(), "{REDO_CLEARED}");
    }

    #[test]
    fn an_empty_history_has_nothing_to_undo() {
        let mut history = History::default();
        assert!(history.undo().is_none());
        assert!(history.redo().is_none());
    }

    #[test]
    fn forward_deletes_fold_in_the_order_they_happened() {
        let mut history = History::default();
        for letter in "abc".chars() {
            history.record(Edit {
                at: Cursor::new(0, 2),
                removed: letter.to_string(),
                inserted: String::new(),
                cursor_before: Cursor::new(0, 2),
                cursor_after: Cursor::new(0, 2),
            });
        }
        assert_eq!(history.depth(), 1, "{ONE_GESTURE}");
        assert_eq!(history.undo().unwrap().inserted, "abc", "{ONE_GESTURE}");
    }

    #[test]
    fn a_buffer_is_clean_when_undone_back_to_what_was_saved() {
        let mut history = History::default();
        assert!(history.is_saved(), "{CLEAN_AGAIN}");

        history.record(insert(0, 0, "typed"));
        assert!(!history.is_saved());

        history.mark_saved();
        assert!(history.is_saved(), "{CLEAN_AGAIN}");

        history.record(insert(0, 5, " more"));
        assert!(!history.is_saved());

        history.undo();
        assert!(history.is_saved(), "{CLEAN_AGAIN}");

        history.redo();
        assert!(!history.is_saved(), "{CLEAN_AGAIN}");
    }
}
