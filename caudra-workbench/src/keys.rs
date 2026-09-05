//! The workbench keymap.
//!
//! `caudra-ui` dispatches overlays before its own global binds, so these chords
//! are free to look like an editor's even where the transcript spends the same
//! key on something else. Two exceptions are deliberate and live in
//! [`crate::Workbench::handle_key`]: `Ctrl+C` without a selection and `Ctrl+W`
//! inside a buffer both pass through, so quitting and deleting a word never
//! disappear behind an open workbench.
//!
//! `caudra-ui`'s `KEYBINDS` table quotes the `label` fields below, so the help
//! modal and the generated docs cannot drift from what is dispatched here.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

macro_rules! bind {
    ($code:expr, $modifiers:expr, $label:literal) => {
        Bind {
            code: $code,
            modifiers: $modifiers,
            label: $label,
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bind {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub label: &'static str,
}

impl Bind {
    pub fn matches(self, key: KeyEvent) -> bool {
        key.code == self.code && key.modifiers == self.modifiers
    }
}

const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const ALT: KeyModifiers = KeyModifiers::ALT;
const NONE: KeyModifiers = KeyModifiers::NONE;

pub const CLOSE: Bind = bind!(KeyCode::Esc, NONE, "Esc");
pub const TOGGLE_SIDEBAR: Bind = bind!(KeyCode::Char('b'), CTRL, "Ctrl+B");
pub const TOGGLE_HIDDEN: Bind = bind!(KeyCode::Char('h'), CTRL, "Ctrl+H");
pub const VIEW_EXPLORER: Bind = bind!(KeyCode::Char('1'), ALT, "Alt+1");
pub const VIEW_SOURCE_CONTROL: Bind = bind!(KeyCode::Char('2'), ALT, "Alt+2");
pub const VIEW_SEARCH: Bind = bind!(KeyCode::Char('3'), ALT, "Alt+3");
pub const FOCUS_NEXT: Bind = bind!(KeyCode::Tab, NONE, "Tab");
pub const FOCUS_PREV: Bind = bind!(KeyCode::BackTab, KeyModifiers::SHIFT, "Shift+Tab");
pub const QUICK_OPEN: Bind = bind!(KeyCode::Char('p'), CTRL, "Ctrl+P");
pub const REFRESH: Bind = bind!(KeyCode::F(5), NONE, "F5");
pub const SEND_TO_COMPOSER: Bind = bind!(KeyCode::Enter, ALT, "Alt+Enter");

pub const SAVE: Bind = bind!(KeyCode::Char('s'), CTRL, "Ctrl+S");
pub const REVERT: Bind = bind!(KeyCode::Char('r'), CTRL, "Ctrl+R");
pub const UNDO: Bind = bind!(KeyCode::Char('z'), CTRL, "Ctrl+Z");
pub const REDO: Bind = bind!(KeyCode::Char('y'), CTRL, "Ctrl+Y");
pub const FIND: Bind = bind!(KeyCode::Char('f'), CTRL, "Ctrl+F");
pub const GOTO_LINE: Bind = bind!(KeyCode::Char('g'), CTRL, "Ctrl+G");
pub const SELECT_ALL: Bind = bind!(KeyCode::Char('a'), CTRL, "Ctrl+A");
pub const KILL_LINE: Bind = bind!(KeyCode::Char('k'), CTRL, "Ctrl+K");
pub const COPY: Bind = bind!(KeyCode::Char('c'), CTRL, "Ctrl+C");
pub const CUT: Bind = bind!(KeyCode::Char('x'), CTRL, "Ctrl+X");
pub const PASTE: Bind = bind!(KeyCode::Char('v'), CTRL, "Ctrl+V");

pub const PREV_TAB: Bind = bind!(KeyCode::Left, ALT, "Alt+Left");
pub const NEXT_TAB: Bind = bind!(KeyCode::Right, ALT, "Alt+Right");
pub const CLOSE_TAB: Bind = bind!(KeyCode::Char('w'), ALT, "Alt+W");
pub const SHRINK_SIDEBAR: Bind = bind!(KeyCode::Char('-'), ALT, "Alt+-");
pub const GROW_SIDEBAR: Bind = bind!(KeyCode::Char('='), ALT, "Alt+=");

pub const STAGE_TOGGLE: Bind = bind!(KeyCode::Char(' '), NONE, "Space");
pub const OPEN_DIFF: Bind = bind!(KeyCode::Char('d'), NONE, "D");
pub const DISCARD: Bind = bind!(KeyCode::Char('x'), NONE, "X");
pub const TOGGLE_TREE: Bind = bind!(KeyCode::Char('t'), NONE, "T");
pub const SHRINK_SECTION: Bind = bind!(KeyCode::Up, ALT, "Alt+Up");
pub const GROW_SECTION: Bind = bind!(KeyCode::Down, ALT, "Alt+Down");

pub const NEXT_FIELD: Bind = bind!(KeyCode::Char('i'), ALT, "Alt+I");
pub const TOGGLE_CASE: Bind = bind!(KeyCode::Char('c'), ALT, "Alt+C");
pub const TOGGLE_WORD: Bind = bind!(KeyCode::Char('m'), ALT, "Alt+M");
pub const TOGGLE_REGEX: Bind = bind!(KeyCode::Char('r'), ALT, "Alt+R");

/// Binds that must not collide, checked as a set rather than by eye. Panes
/// scope the rest: `Space` only reaches source control, and plain characters
/// only reach a pane with no text field.
#[cfg(test)]
const GLOBAL_BINDS: &[Bind] = &[
    CLOSE,
    TOGGLE_SIDEBAR,
    TOGGLE_HIDDEN,
    VIEW_EXPLORER,
    VIEW_SOURCE_CONTROL,
    VIEW_SEARCH,
    FOCUS_NEXT,
    FOCUS_PREV,
    QUICK_OPEN,
    REFRESH,
    SEND_TO_COMPOSER,
    SAVE,
    REVERT,
    UNDO,
    REDO,
    FIND,
    GOTO_LINE,
    SELECT_ALL,
    KILL_LINE,
    COPY,
    CUT,
    PASTE,
    PREV_TAB,
    NEXT_TAB,
    CLOSE_TAB,
    SHRINK_SIDEBAR,
    GROW_SIDEBAR,
];

/// Binds that only reach the source control pane. They are bare characters, so
/// they are checked against the global set too: a collision there would take
/// the key away from every other pane.
#[cfg(test)]
const SOURCE_CONTROL_BINDS: &[Bind] = &[
    STAGE_TOGGLE,
    OPEN_DIFF,
    DISCARD,
    TOGGLE_TREE,
    SHRINK_SECTION,
    GROW_SECTION,
];

/// Binds that only reach the search pane. Its fields swallow bare characters,
/// so these carry `Alt` and are checked against everything else.
#[cfg(test)]
const SEARCH_BINDS: &[Bind] = &[NEXT_FIELD, TOGGLE_CASE, TOGGLE_WORD, TOGGLE_REGEX];

#[cfg(test)]
mod tests {
    use super::{
        Bind, GLOBAL_BINDS, KeyCode, KeyEvent, KeyModifiers, NONE, SEARCH_BINDS,
        SOURCE_CONTROL_BINDS, STAGE_TOGGLE,
    };

    const DUPLICATE: &str = "two workbench binds must not answer to the same chord";
    const LABEL_EMPTY: &str = "every bind must carry a label for the help modal";
    const MODIFIER_EXACT: &str = "a bind must not answer to a chord carrying extra modifiers";

    fn every_bind() -> Vec<Bind> {
        GLOBAL_BINDS
            .iter()
            .chain(SOURCE_CONTROL_BINDS)
            .chain(SEARCH_BINDS)
            .copied()
            .collect()
    }

    #[test]
    fn no_two_binds_share_a_chord() {
        let binds = every_bind();
        for (i, a) in binds.iter().enumerate() {
            for b in &binds[i + 1..] {
                assert!(
                    a.code != b.code || a.modifiers != b.modifiers,
                    "{DUPLICATE}: {} and {}",
                    a.label,
                    b.label
                );
            }
        }
    }

    #[test]
    fn every_bind_is_labelled() {
        for bind in every_bind() {
            assert!(!bind.label.is_empty(), "{LABEL_EMPTY}");
        }
    }

    #[test]
    fn a_bind_ignores_a_chord_with_extra_modifiers() {
        let loaded = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert!(!STAGE_TOGGLE.matches(loaded), "{MODIFIER_EXACT}");
        assert!(
            STAGE_TOGGLE.matches(KeyEvent::new(KeyCode::Char(' '), NONE)),
            "the bare chord must still match"
        );
    }

    #[test]
    fn a_bind_is_copy_so_tables_can_hold_it_by_value() {
        let bind: Bind = STAGE_TOGGLE;
        let copied = bind;
        assert_eq!(bind, copied);
    }
}
