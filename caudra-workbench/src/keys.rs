//! The workbench keymap.
//!
//! `caudra-ui` dispatches overlays before its own global binds, so these chords
//! are free to look like an editor's even where the transcript spends the same
//! key on something else. Three exceptions are deliberate and live in
//! [`crate::Workbench::handle_key`]: [`LEADER`] always, `Ctrl+C` without a
//! selection, and `Ctrl+W` inside a buffer, all pass through, so the leader
//! prefix, quitting and deleting a word never disappear behind an open
//! workbench.
//!
//! [`LEADER_BINDS`] are the second halves of `Ctrl+X` chords, matched by
//! [`crate::Workbench::handle_leader`] against the key that follows the prefix.
//! They carry no modifiers of their own and never meet a bare key event, which
//! is why a letter here may repeat one used by a direct chord.
//!
//! `caudra-ui`'s `KEYBINDS` table quotes the `label` fields below, so the help
//! modal and the generated docs cannot drift from what is dispatched here.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Spelled out here because a `concat!` label needs a literal. `caudra-ui` owns
/// the prefix and asserts the two agree.
pub const LEADER_LABEL: &str = "Ctrl+X";

macro_rules! bind {
    ($code:expr, $modifiers:expr, $label:literal) => {
        Bind {
            code: $code,
            modifiers: $modifiers,
            label: $label,
        }
    };
}

macro_rules! leader {
    ($code:expr, $key:literal) => {
        Bind {
            code: $code,
            modifiers: NONE,
            label: concat!("Ctrl+X ", $key),
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
const NONE: KeyModifiers = KeyModifiers::NONE;

/// The prefix itself. The workbench binds nothing to it and always hands it
/// back, so every chord under it stays reachable from every pane and every
/// selection state.
pub const LEADER: Bind = Bind {
    code: KeyCode::Char('x'),
    modifiers: CTRL,
    label: LEADER_LABEL,
};

pub const CLOSE: Bind = bind!(KeyCode::Esc, NONE, "Esc");
pub const TOGGLE_SIDEBAR: Bind = bind!(KeyCode::Char('b'), CTRL, "Ctrl+B");
pub const FOCUS_NEXT: Bind = bind!(KeyCode::Tab, NONE, "Tab");
pub const FOCUS_PREV: Bind = bind!(KeyCode::BackTab, KeyModifiers::SHIFT, "Shift+Tab");
pub const QUICK_OPEN: Bind = bind!(KeyCode::Char('p'), CTRL, "Ctrl+P");
pub const REFRESH: Bind = bind!(KeyCode::F(5), NONE, "F5");

/// The editor convention, and reachable on every terminal, so tab stepping
/// needs no chord.
pub const PREV_TAB: Bind = bind!(KeyCode::PageUp, CTRL, "Ctrl+PageUp");
pub const NEXT_TAB: Bind = bind!(KeyCode::PageDown, CTRL, "Ctrl+PageDown");

pub const SAVE: Bind = bind!(KeyCode::Char('s'), CTRL, "Ctrl+S");
pub const REVERT: Bind = bind!(KeyCode::Char('r'), CTRL, "Ctrl+R");
pub const UNDO: Bind = bind!(KeyCode::Char('z'), CTRL, "Ctrl+Z");
pub const REDO: Bind = bind!(KeyCode::Char('y'), CTRL, "Ctrl+Y");
pub const FIND: Bind = bind!(KeyCode::Char('f'), CTRL, "Ctrl+F");
pub const FIND_NEXT: Bind = bind!(KeyCode::F(3), NONE, "F3");
pub const FIND_PREV: Bind = bind!(KeyCode::F(3), KeyModifiers::SHIFT, "Shift+F3");
pub const GOTO_LINE: Bind = bind!(KeyCode::Char('g'), CTRL, "Ctrl+G");
pub const SELECT_ALL: Bind = bind!(KeyCode::Char('a'), CTRL, "Ctrl+A");
pub const KILL_LINE: Bind = bind!(KeyCode::Char('k'), CTRL, "Ctrl+K");
/// Emacs' kill-word-backward, the one word delete a terminal always delivers:
/// `Ctrl+Backspace` is byte 0x08 without the kitty protocol, so it reaches a
/// plain terminal as an ordinary Backspace.
pub const DELETE_WORD: Bind = bind!(KeyCode::Char('w'), CTRL, "Ctrl+W");
pub const COPY: Bind = bind!(KeyCode::Char('c'), CTRL, "Ctrl+C");
/// `Ctrl+X` is the leader in every Caudra surface, so cut takes CUA's other
/// standard. `Shift+Delete` predates `Ctrl+X`, carries no control byte, and
/// arrives as an unambiguous `CSI 3;2~` without the kitty protocol.
pub const CUT: Bind = bind!(KeyCode::Delete, KeyModifiers::SHIFT, "Shift+Delete");
pub const PASTE: Bind = bind!(KeyCode::Char('v'), CTRL, "Ctrl+V");

pub const STAGE_TOGGLE: Bind = bind!(KeyCode::Char(' '), NONE, "Space");
pub const OPEN_DIFF: Bind = bind!(KeyCode::Char('d'), NONE, "D");
pub const DISCARD: Bind = bind!(KeyCode::Char('x'), NONE, "X");
pub const TOGGLE_TREE: Bind = bind!(KeyCode::Char('t'), NONE, "T");

pub const COLLAPSE_ALL: Bind = bind!(KeyCode::Char('c'), NONE, "C");

/// `Ctrl+H` is byte 0x08, indistinguishable from Backspace, so the toggle
/// lives under the leader instead.
pub const TOGGLE_HIDDEN: Bind = leader!(KeyCode::Char('h'), "h");
/// Cut's chord form, for a terminal that spends `Shift+Delete` itself.
pub const CUT_CHORD: Bind = leader!(KeyCode::Char('x'), "x");
pub const VIEW_EXPLORER: Bind = leader!(KeyCode::Char('1'), "1");
pub const VIEW_SOURCE_CONTROL: Bind = leader!(KeyCode::Char('2'), "2");
pub const VIEW_SEARCH: Bind = leader!(KeyCode::Char('3'), "3");
pub const SEND_TO_COMPOSER: Bind = leader!(KeyCode::Enter, "Enter");
/// `k` for emacs' kill-buffer, which leaves `w` to the search pane.
pub const CLOSE_TAB: Bind = leader!(KeyCode::Char('k'), "k");
pub const TOGGLE_WRAP: Bind = leader!(KeyCode::Char('z'), "z");
/// The transcript spends `v` on its own compact view, which is behind the
/// workbench while this answers, so the chord only shadows it here.
pub const TOGGLE_RENDERED: Bind = leader!(KeyCode::Char('v'), "v");
pub const SHRINK_SIDEBAR: Bind = leader!(KeyCode::Char('-'), "-");
pub const GROW_SIDEBAR: Bind = leader!(KeyCode::Char('='), "=");
pub const SHRINK_SECTION: Bind = leader!(KeyCode::Up, "↑");
pub const GROW_SECTION: Bind = leader!(KeyCode::Down, "↓");
pub const NEXT_FIELD: Bind = leader!(KeyCode::Char('i'), "i");
/// `m` belongs to the host's model picker, which stays reachable while the
/// workbench is up.
pub const MENU: Bind = leader!(KeyCode::Char('.'), ".");
pub const TOGGLE_CASE: Bind = leader!(KeyCode::Char('c'), "c");
pub const TOGGLE_WORD: Bind = leader!(KeyCode::Char('w'), "w");
pub const TOGGLE_REGEX: Bind = leader!(KeyCode::Char('r'), "r");

/// Direct chords that must not collide, checked as a set rather than by eye.
/// Panes scope the rest: `Space` only reaches source control, and plain
/// characters only reach a pane with no text field.
#[cfg(test)]
const GLOBAL_BINDS: &[Bind] = &[
    CLOSE,
    TOGGLE_SIDEBAR,
    FOCUS_NEXT,
    FOCUS_PREV,
    QUICK_OPEN,
    REFRESH,
    PREV_TAB,
    NEXT_TAB,
    SAVE,
    REVERT,
    UNDO,
    REDO,
    FIND,
    FIND_NEXT,
    FIND_PREV,
    GOTO_LINE,
    SELECT_ALL,
    KILL_LINE,
    DELETE_WORD,
    COPY,
    CUT,
    PASTE,
];

/// Binds that only reach the source control pane. They are bare characters, so
/// they are checked against the global set too: a collision there would take
/// the key away from every other pane.
#[cfg(test)]
const SOURCE_CONTROL_BINDS: &[Bind] = &[STAGE_TOGGLE, OPEN_DIFF, DISCARD, TOGGLE_TREE];

/// Binds that only reach the explorer, checked the same way and for the same
/// reason as the source control set.
#[cfg(test)]
const EXPLORER_BINDS: &[Bind] = &[COLLAPSE_ALL];

/// Second keys of the `Ctrl+X` chords. Checked among themselves only: the
/// prefix keeps them clear of every direct chord, whichever pane is up.
pub const LEADER_BINDS: &[Bind] = &[
    TOGGLE_HIDDEN,
    CUT_CHORD,
    VIEW_EXPLORER,
    VIEW_SOURCE_CONTROL,
    VIEW_SEARCH,
    SEND_TO_COMPOSER,
    CLOSE_TAB,
    TOGGLE_WRAP,
    TOGGLE_RENDERED,
    SHRINK_SIDEBAR,
    GROW_SIDEBAR,
    SHRINK_SECTION,
    GROW_SECTION,
    NEXT_FIELD,
    MENU,
    TOGGLE_CASE,
    TOGGLE_WORD,
    TOGGLE_REGEX,
];

#[cfg(test)]
mod tests {
    use super::{
        Bind, EXPLORER_BINDS, GLOBAL_BINDS, KeyCode, KeyEvent, KeyModifiers, LEADER_BINDS,
        LEADER_LABEL, NONE, SOURCE_CONTROL_BINDS, STAGE_TOGGLE,
    };

    const DUPLICATE: &str = "two workbench binds must not answer to the same chord";
    const LABEL_EMPTY: &str = "every bind must carry a label for the help modal";
    const MODIFIER_EXACT: &str = "a bind must not answer to a chord carrying extra modifiers";
    const LEADER_BARE: &str = "a leader bind carries no modifiers: the prefix already did";
    const LEADER_PREFIXED: &str = "a leader bind's label must spell the whole chord";
    const NO_ALT: &str = "Alt is dead on a default macOS terminal, so no default may depend on it";

    fn direct_binds() -> Vec<Bind> {
        GLOBAL_BINDS
            .iter()
            .chain(SOURCE_CONTROL_BINDS)
            .chain(EXPLORER_BINDS)
            .copied()
            .collect()
    }

    fn every_bind() -> Vec<Bind> {
        direct_binds().into_iter().chain(leader_binds()).collect()
    }

    fn leader_binds() -> Vec<Bind> {
        LEADER_BINDS.to_vec()
    }

    fn assert_no_shared_chord(binds: &[Bind]) {
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
    fn no_two_direct_binds_share_a_chord() {
        assert_no_shared_chord(&direct_binds());
    }

    #[test]
    fn no_two_leader_binds_share_a_second_key() {
        assert_no_shared_chord(&leader_binds());
    }

    #[test]
    fn a_leader_bind_is_a_bare_key_labelled_with_its_prefix() {
        for bind in leader_binds() {
            assert_eq!(bind.modifiers, NONE, "{LEADER_BARE}: {}", bind.label);
            assert!(
                bind.label.starts_with(LEADER_LABEL),
                "{LEADER_PREFIXED}: {}",
                bind.label
            );
        }
    }

    #[test]
    fn no_bind_depends_on_alt() {
        for bind in every_bind() {
            assert!(
                !bind.modifiers.contains(KeyModifiers::ALT),
                "{NO_ALT}: {}",
                bind.label
            );
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
