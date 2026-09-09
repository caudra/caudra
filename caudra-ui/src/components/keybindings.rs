use caudra_workbench::keys as wb;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::fmt::Write;
use strum::EnumIter;
use unicode_width::UnicodeWidthStr;

/// Spelled once because three tables quote it.
const SHIFT_TAB_LABEL: &str = "Shift+Tab";

macro_rules! mod_key {
    ($suffix:expr) => {
        concat!("Ctrl+", $suffix)
    };
}

macro_rules! leader_label {
    ($suffix:expr) => {
        concat!(mod_key!("X"), " ", $suffix)
    };
}

/// The second key of a leader chord. It carries no modifiers because the
/// prefix already did, and its label spells the whole chord so the help modal
/// and the generated docs quote one string.
macro_rules! leader_bind {
    ($char:tt) => {
        Bind {
            code: KeyCode::Char($char),
            modifiers: KeyModifiers::NONE,
            label: leader_label!($char),
        }
    };
}

macro_rules! upper {
    ('a') => {
        "A"
    };
    ('b') => {
        "B"
    };
    ('c') => {
        "C"
    };
    ('d') => {
        "D"
    };
    ('e') => {
        "E"
    };
    ('f') => {
        "F"
    };
    ('g') => {
        "G"
    };
    ('h') => {
        "H"
    };
    ('i') => {
        "I"
    };
    ('j') => {
        "J"
    };
    ('k') => {
        "K"
    };
    ('l') => {
        "L"
    };
    ('m') => {
        "M"
    };
    ('n') => {
        "N"
    };
    ('o') => {
        "O"
    };
    ('p') => {
        "P"
    };
    ('q') => {
        "Q"
    };
    ('r') => {
        "R"
    };
    ('s') => {
        "S"
    };
    ('t') => {
        "T"
    };
    ('u') => {
        "U"
    };
    ('v') => {
        "V"
    };
    ('w') => {
        "W"
    };
    ('x') => {
        "X"
    };
    ('y') => {
        "Y"
    };
    ('z') => {
        "Z"
    };
}

macro_rules! ctrl_bind {
    ($char:tt) => {
        Bind {
            code: KeyCode::Char($char),
            modifiers: KeyModifiers::CONTROL,
            label: mod_key!(upper!($char)),
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
    pub fn matches(&self, key: KeyEvent) -> bool {
        key.code == self.code && key.modifiers == self.modifiers
    }

    #[cfg(test)]
    pub const fn to_key_event(self) -> KeyEvent {
        KeyEvent {
            code: self.code,
            modifiers: self.modifiers,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }
}

pub mod key {
    use super::Bind;
    use crossterm::event::{KeyCode, KeyModifiers};

    /// Prefix of every two-key chord. `Ctrl+X` carries no control code of its
    /// own, and both emacs and opencode already spend it this way.
    pub const LEADER: Bind = ctrl_bind!('x');

    pub const QUIT: Bind = ctrl_bind!('c');
    pub const EXIT: Bind = ctrl_bind!('d');
    /// `Ctrl+H` is byte 0x08, which no terminal can tell apart from Backspace,
    /// so help answers to a function key and to `Ctrl+X ?`.
    pub const HELP: Bind = Bind {
        code: KeyCode::F(1),
        modifiers: KeyModifiers::NONE,
        label: "F1",
    };
    pub const COMMAND_PALETTE: Bind = ctrl_bind!('p');
    pub const SCROLL_HALF_UP: Bind = ctrl_bind!('u');
    pub const SCROLL_HALF_UP_ALT: Bind = Bind {
        code: KeyCode::PageUp,
        modifiers: KeyModifiers::NONE,
        label: "PageUp",
    };
    pub const SCROLL_HALF_DOWN: Bind = Bind {
        code: KeyCode::PageDown,
        modifiers: KeyModifiers::NONE,
        label: "PageDown",
    };
    /// Shift is the only free arrow modifier: plain, Ctrl, Alt and Super all
    /// move the input cursor, and neither text buffer inspects Shift.
    pub const PAN_LEFT: Bind = Bind {
        code: KeyCode::Left,
        modifiers: KeyModifiers::SHIFT,
        label: "Shift+Left",
    };
    pub const PAN_RIGHT: Bind = Bind {
        code: KeyCode::Right,
        modifiers: KeyModifiers::SHIFT,
        label: "Shift+Right",
    };
    pub const SCROLL_LINE_UP: Bind = ctrl_bind!('y');
    pub const SCROLL_LINE_DOWN: Bind = ctrl_bind!('e');
    pub const SCROLL_TOP: Bind = ctrl_bind!('g');
    pub const SCROLL_BOTTOM: Bind = ctrl_bind!('b');
    /// Ctrl is the only free Home/End modifier: bare Home/End move the input
    /// cursor to the start/end of the line.
    pub const SCROLL_TOP_ALT: Bind = Bind {
        code: KeyCode::Home,
        modifiers: KeyModifiers::CONTROL,
        label: "Ctrl+Home",
    };
    pub const SCROLL_BOTTOM_ALT: Bind = Bind {
        code: KeyCode::End,
        modifiers: KeyModifiers::CONTROL,
        label: "Ctrl+End",
    };
    pub const POP_QUEUE: Bind = ctrl_bind!('q');
    pub const DELETE_WORD: Bind = ctrl_bind!('w');
    pub const SEARCH: Bind = ctrl_bind!('f');
    pub const FILE_PICKER: Bind = ctrl_bind!('s');
    pub const OPEN_EDITOR: Bind = ctrl_bind!('o');
    /// Cycles the reasoning ladder. `t` for thinking, and the same key
    /// opencode spends on its variant cycle, which is the same operation.
    pub const THINKING: Bind = ctrl_bind!('t');
    pub const NEW_SESSION: Bind = ctrl_bind!('n');
    pub const RENAME_SESSION: Bind = ctrl_bind!('r');
    pub const GENERATE_TITLE: Bind = ctrl_bind!('g');
    pub const REFRESH: Bind = ctrl_bind!('r');
    pub const SUSPEND: Bind = ctrl_bind!('z');
    pub const DELETE: Bind = ctrl_bind!('d');
    pub const KILL_LINE: Bind = ctrl_bind!('k');
    pub const LINE_START: Bind = ctrl_bind!('a');
    pub const LINE_END: Bind = ctrl_bind!('e');
}

/// Second keys of the `Ctrl+X` chords. They are a namespace of their own:
/// nothing here is ever matched against a bare key event, only against the one
/// that follows the leader, so a letter may repeat a direct chord's letter.
pub mod leader {
    use super::Bind;
    use crossterm::event::{KeyCode, KeyModifiers};

    pub const TASKS: Bind = leader_bind!('a');
    pub const EDIT_INPUT: Bind = leader_bind!('e');
    pub const FILE_PICKER: Bind = leader_bind!('f');
    pub const STEER_PROMPT: Bind = leader_bind!('g');
    pub const SESSION_PICKER: Bind = leader_bind!('l');
    pub const MODEL_PICKER: Bind = leader_bind!('m');
    pub const NEW_SESSION: Bind = leader_bind!('n');
    pub const PLAN_EDITOR: Bind = leader_bind!('o');
    pub const STASH_POP: Bind = leader_bind!('p');
    pub const POP_QUEUE: Bind = leader_bind!('q');
    pub const REVIEW: Bind = leader_bind!('r');
    pub const STASH_PUSH: Bind = leader_bind!('s');
    pub const PLAN_TOGGLE: Bind = leader_bind!('t');
    pub const VIEW_TOGGLE: Bind = leader_bind!('v');
    pub const WORKBENCH: Bind = leader_bind!('w');
    pub const INTERRUPT_PROMPT: Bind = leader_bind!('x');
    pub const COPY_MESSAGE: Bind = leader_bind!('y');
    pub const HELP: Bind = leader_bind!('?');

    /// Every global chord, in the order the which-key panel lists them.
    pub const ALL: &[Bind] = &[
        TASKS,
        EDIT_INPUT,
        FILE_PICKER,
        STEER_PROMPT,
        SESSION_PICKER,
        MODEL_PICKER,
        NEW_SESSION,
        PLAN_EDITOR,
        STASH_POP,
        POP_QUEUE,
        REVIEW,
        STASH_PUSH,
        PLAN_TOGGLE,
        VIEW_TOGGLE,
        WORKBENCH,
        INTERRUPT_PROMPT,
        COPY_MESSAGE,
        HELP,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter)]
pub enum KeybindContext {
    General,
    Editing,
    Streaming,
    Picker,
    FormInput,
    RewindPicker,
    ThemePicker,
    ModelPicker,
    QueueFocus,
    CommandPalette,
    PasteEditor,
    Review,
    Search,
    Logs,
    FilePicker,
    StashPicker,
    SessionPicker,
    Workbench,
    WorkbenchExplorer,
    WorkbenchEditor,
    WorkbenchSourceControl,
    WorkbenchSearch,
}

impl KeybindContext {
    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Editing => "Editing",
            Self::Streaming => "While Streaming",
            Self::Picker => "Pickers",
            Self::FormInput => "Form",
            Self::RewindPicker => "Rewind Picker",
            Self::ThemePicker => "Theme Picker",
            Self::ModelPicker => "Model Picker",
            Self::QueueFocus => "Queue",
            Self::CommandPalette => "Commands",
            Self::PasteEditor => "Pasted Text",
            Self::Review => "Review",
            Self::Search => "Search",
            Self::Logs => "Logs",
            Self::FilePicker => "File Picker",
            Self::StashPicker => "Stash Picker",
            Self::SessionPicker => "Session Picker",
            Self::Workbench => "Workbench",
            Self::WorkbenchExplorer => "Workbench Explorer",
            Self::WorkbenchEditor => "Workbench Editor",
            Self::WorkbenchSourceControl => "Workbench Source Control",
            Self::WorkbenchSearch => "Workbench Search",
        }
    }

    pub const fn parent(self) -> Option<KeybindContext> {
        match self {
            Self::RewindPicker
            | Self::ThemePicker
            | Self::ModelPicker
            | Self::QueueFocus
            | Self::CommandPalette
            | Self::Search
            | Self::FilePicker
            | Self::StashPicker
            | Self::SessionPicker => Some(Self::Picker),
            Self::WorkbenchExplorer
            | Self::WorkbenchEditor
            | Self::WorkbenchSourceControl
            | Self::WorkbenchSearch => Some(Self::Workbench),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    All,
    UnixOnly,
}

impl Platform {
    pub const fn is_visible(self) -> bool {
        match self {
            Self::All => true,
            Self::UnixOnly => cfg!(unix),
        }
    }
}

pub const ALT_SEP: &str = " / ";

#[derive(Debug, Clone, Copy)]
pub enum KeyLabel {
    Single(&'static str),
    Alt(&'static str, &'static str),
    Multi(&'static [&'static str]),
}

impl KeyLabel {
    pub fn display_width(self) -> usize {
        let sep_w = UnicodeWidthStr::width(ALT_SEP);
        let parts = self.parts();
        parts
            .clone()
            .map(UnicodeWidthStr::width)
            .sum::<usize>()
            .saturating_add(sep_w * parts.count().saturating_sub(1))
    }

    /// The individual chords a label spells, whatever shape it came in.
    pub fn parts(self) -> impl Iterator<Item = &'static str> + Clone {
        let (pair, multi): ([Option<&'static str>; 2], &'static [&'static str]) = match self {
            Self::Single(s) => ([Some(s), None], &[]),
            Self::Alt(a, b) => ([Some(a), Some(b)], &[]),
            Self::Multi(keys) => ([None, None], keys),
        };
        pair.into_iter().flatten().chain(multi.iter().copied())
    }

    #[cfg(test)]
    fn flat_str(&self) -> String {
        self.parts().collect::<Vec<_>>().join("/")
    }
}

/// How every leader chord label starts.
pub const LEADER_PREFIX: &str = mod_key!("X");

/// One row of the which-key panel: the key still to press, and what it does.
pub struct LeaderChord {
    pub key: &'static str,
    pub description: &'static str,
}

fn leader_suffix(label: &'static str) -> Option<&'static str> {
    label
        .strip_prefix(LEADER_PREFIX)?
        .strip_prefix(' ')
        .filter(|suffix| !suffix.is_empty())
}

/// Every chord the leader still reaches from `contexts`, read back out of
/// [`KEYBINDS`] so the panel, the help modal and the generated docs cannot
/// describe the same key differently.
pub fn leader_chords(contexts: &[KeybindContext]) -> Vec<LeaderChord> {
    KEYBINDS
        .iter()
        .filter(|kb| kb.platform.is_visible() && contexts.contains(&kb.context))
        .filter_map(|kb| {
            let key = kb.label.parts().find_map(leader_suffix)?;
            Some(LeaderChord {
                key,
                description: kb.description,
            })
        })
        .collect()
}

/// Strips the shift a terminal reports alongside a printable character, so the
/// second half of a chord matches whether or not the layout needed shift to
/// produce it.
pub fn normalize_leader_key(key: KeyEvent) -> KeyEvent {
    let mut key = key;
    if matches!(key.code, KeyCode::Char(_)) {
        key.modifiers -= KeyModifiers::SHIFT;
    }
    key
}

pub struct Keybind {
    pub label: KeyLabel,
    pub description: &'static str,
    pub context: KeybindContext,
    pub platform: Platform,
}

pub const KEYBINDS: &[Keybind] = &[
    Keybind {
        label: KeyLabel::Single(key::QUIT.label),
        description: "Quit / clear input",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Ctrl+D Ctrl+D"),
        description: "Exit",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::COMMAND_PALETTE.label),
        description: "Command palette",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::LEADER.label),
        description: "Leader: lists the chords below, then runs the one you press",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(key::HELP.label, leader::HELP.label),
        description: "Show keybindings",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::SEARCH.label),
        description: "Search messages",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::COPY_MESSAGE.label),
        description: "Copy last reply as markdown",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::REVIEW.label),
        description: "Review the last reply",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(key::FILE_PICKER.label, leader::FILE_PICKER.label),
        description: "File picker",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(key::OPEN_EDITOR.label, leader::PLAN_EDITOR.label),
        description: "Open plan in editor",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::PLAN_TOGGLE.label),
        description: "Toggle plan / todo panel",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::TASKS.label),
        description: "Open tasks",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::SESSION_PICKER.label),
        description: "Browse sessions",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::NEW_SESSION.label),
        description: "Start a new session",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::VIEW_TOGGLE.label),
        description: "Toggle compact / expanded transcript",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::STASH_PUSH.label),
        description: "Stash the current prompt",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::STASH_POP.label),
        description: "Restore the newest stashed prompt",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::MODEL_PICKER.label),
        description: "Model picker",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::SUSPEND.label),
        description: "Suspend process",
        context: KeybindContext::General,
        platform: Platform::UnixOnly,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Submit prompt",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Multi(&["Shift+Enter", "Ctrl+Enter", "Ctrl+J"]),
        description: "Newline",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Tab"),
        description: "Toggle BUILD/PLAN mode",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(key::THINKING.label, SHIFT_TAB_LABEL),
        description: "Cycle reasoning effort",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("/command"),
        description: "Open command palette",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(key::DELETE_WORD.label, mod_key!("Backspace")),
        description: "Delete word backward",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Multi(&[mod_key!("←"), mod_key!("→")]),
        description: "Move word left / right",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(mod_key!("Del")),
        description: "Delete word forward",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::KILL_LINE.label),
        description: "Delete to end of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::LINE_START.label),
        description: "Jump to start of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("Home", "End"),
        description: "Jump to start/end of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Multi(&[key::SCROLL_HALF_UP.label, key::SCROLL_HALF_UP_ALT.label]),
        description: "Scroll half page up",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::SCROLL_HALF_DOWN.label),
        description: "Scroll half page down",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(key::PAN_LEFT.label, key::PAN_RIGHT.label),
        description: "Pan a wide diagram left / right",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::LINE_END.label),
        description: "Jump to end of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Multi(&[key::SCROLL_TOP.label, key::SCROLL_TOP_ALT.label]),
        description: "Scroll to top",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Multi(&[key::SCROLL_BOTTOM.label, key::SCROLL_BOTTOM_ALT.label]),
        description: "Scroll to bottom",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(key::POP_QUEUE.label, leader::POP_QUEUE.label),
        description: "Pop queue",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc Esc"),
        description: "Rewind",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::EDIT_INPUT.label),
        description: "Edit input in external editor",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Insert newline",
        context: KeybindContext::PasteEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Ctrl+S"),
        description: "Save pasted text",
        context: KeybindContext::PasteEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc"),
        description: "Cancel editing",
        context: KeybindContext::PasteEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Multi(&["j", "k", "g", "G"]),
        description: "Move the row cursor",
        context: KeybindContext::Review,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("v"),
        description: "Extend the passage",
        context: KeybindContext::Review,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Write a note on the passage",
        context: KeybindContext::Review,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Multi(&["e", "d"]),
        description: "Edit or delete the note under the cursor",
        context: KeybindContext::Review,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Multi(&["n", "p"]),
        description: "Jump between notes",
        context: KeybindContext::Review,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Ctrl+S"),
        description: "Send notes to the prompt",
        context: KeybindContext::Review,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("↑", "↓"),
        description: "Navigate input history",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc Esc"),
        description: "Cancel agent",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Send prompt next",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::STEER_PROMPT.label),
        description: "Guide current run",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::INTERRUPT_PROMPT.label),
        description: "Stop and replace current run",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("↑", "↓"),
        description: "Navigate options",
        context: KeybindContext::FormInput,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Select option",
        context: KeybindContext::FormInput,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc"),
        description: "Close",
        context: KeybindContext::FormInput,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("↑", "↓"),
        description: "Move between records",
        context: KeybindContext::Logs,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Expand or collapse the selected record",
        context: KeybindContext::Logs,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Tab"),
        description: "Filter to the selected record's tool call, request, or session",
        context: KeybindContext::Logs,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("f"),
        description: "Follow new records or pause",
        context: KeybindContext::Logs,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("w"),
        description: "Wrap long records onto more rows",
        context: KeybindContext::Logs,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("\u{2190}", "\u{2192}"),
        description: "Pan across a record too wide for the pane",
        context: KeybindContext::Logs,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Home"),
        description: "Back to the left margin",
        context: KeybindContext::Logs,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("l"),
        description: "Cycle the minimum level, or click it in the footer",
        context: KeybindContext::Logs,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("/"),
        description: "Fuzzy filter, space separates terms",
        context: KeybindContext::Logs,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("y", "Y"),
        description: "Copy the record or its raw JSON",
        context: KeybindContext::Logs,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc"),
        description: "Close",
        context: KeybindContext::Logs,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("↑", "↓"),
        description: "Navigate",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Select",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc"),
        description: "Close",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Type"),
        description: "Filter",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("PageUp", "PageDown"),
        description: "Scroll page up / down",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::SCROLL_HALF_UP.label),
        description: "Scroll page up",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("Shift+Up", "Shift+Down"),
        description: "Move item up / down",
        context: KeybindContext::QueueFocus,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Edit item",
        context: KeybindContext::QueueFocus,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("d", "Delete"),
        description: "Delete item",
        context: KeybindContext::QueueFocus,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("m"),
        description: "Move unsent item to Main",
        context: KeybindContext::QueueFocus,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("g"),
        description: "Guide current run",
        context: KeybindContext::QueueFocus,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("n"),
        description: "Move prompt to Up next",
        context: KeybindContext::QueueFocus,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("b"),
        description: "Toggle send together",
        context: KeybindContext::QueueFocus,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("."),
        description: "Open item actions",
        context: KeybindContext::QueueFocus,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("r"),
        description: "Replace current run",
        context: KeybindContext::QueueFocus,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Tab"),
        description: "Complete command",
        context: KeybindContext::CommandPalette,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("Tab", SHIFT_TAB_LABEL),
        description: "Switch model purpose",
        context: KeybindContext::ModelPicker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("R"),
        description: "Reset model purpose",
        context: KeybindContext::ModelPicker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::NEW_SESSION.label),
        description: "New session",
        context: KeybindContext::SessionPicker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::RENAME_SESSION.label),
        description: "Rename session",
        context: KeybindContext::SessionPicker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::GENERATE_TITLE.label),
        description: "Name session with a small model",
        context: KeybindContext::SessionPicker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::DELETE.label),
        description: "Delete session (press twice)",
        context: KeybindContext::SessionPicker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::DELETE.label),
        description: "Delete stash entry (press twice)",
        context: KeybindContext::StashPicker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(leader::WORKBENCH.label),
        description: "Open the workbench",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(wb::CLOSE.label, leader::WORKBENCH.label),
        description: "Back to the transcript",
        context: KeybindContext::Workbench,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::TOGGLE_SIDEBAR.label),
        description: "Show or hide the sidebar",
        context: KeybindContext::Workbench,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(wb::SHRINK_SIDEBAR.label, wb::GROW_SIDEBAR.label),
        description: "Narrow / widen the sidebar",
        context: KeybindContext::Workbench,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Multi(&[
            wb::VIEW_EXPLORER.label,
            wb::VIEW_SOURCE_CONTROL.label,
            wb::VIEW_SEARCH.label,
        ]),
        description: "Explorer / source control / search",
        context: KeybindContext::Workbench,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(wb::FOCUS_NEXT.label, wb::FOCUS_PREV.label),
        description: "Leave the sidebar for the editor",
        context: KeybindContext::Workbench,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::QUICK_OPEN.label),
        description: "Open a file by name",
        context: KeybindContext::Workbench,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::REFRESH.label),
        description: "Reread the tree and the repository",
        context: KeybindContext::Workbench,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::SEND_TO_COMPOSER.label),
        description: "Send the file or selection to the composer",
        context: KeybindContext::Workbench,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::MENU.label),
        description: "Open the context menu for the row or tab under the cursor",
        context: KeybindContext::Workbench,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::TOGGLE_HIDDEN.label),
        description: "Show hidden files",
        context: KeybindContext::WorkbenchExplorer,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::COLLAPSE_ALL.label),
        description: "Fold the tree back to its top level",
        context: KeybindContext::WorkbenchExplorer,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::SAVE.label),
        description: "Save the active file",
        context: KeybindContext::WorkbenchEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::REVERT.label),
        description: "Discard edits and take what is on disk",
        context: KeybindContext::WorkbenchEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(wb::UNDO.label, wb::REDO.label),
        description: "Undo / redo",
        context: KeybindContext::WorkbenchEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(wb::FIND.label, wb::GOTO_LINE.label),
        description: "Find in file / go to line",
        context: KeybindContext::WorkbenchEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(wb::FIND_NEXT.label, wb::FIND_PREV.label),
        description: "Next / previous match, with or without the find bar",
        context: KeybindContext::WorkbenchEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Multi(&[wb::COPY.label, wb::PASTE.label]),
        description: "Copy / paste",
        context: KeybindContext::WorkbenchEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(wb::CUT.label, wb::CUT_CHORD.label),
        description: "Cut the selection",
        context: KeybindContext::WorkbenchEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::SELECT_ALL.label),
        description: "Select the whole buffer",
        context: KeybindContext::WorkbenchEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::KILL_LINE.label),
        description: "Delete to the end of the line",
        context: KeybindContext::WorkbenchEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::TOGGLE_WRAP.label),
        description: "Wrap long lines onto more rows",
        context: KeybindContext::WorkbenchEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(wb::PREV_TAB.label, wb::NEXT_TAB.label),
        description: "Previous / next tab",
        context: KeybindContext::WorkbenchEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::CLOSE_TAB.label),
        description: "Close the active tab",
        context: KeybindContext::WorkbenchEditor,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::STAGE_TOGGLE.label),
        description: "Stage or unstage the file, folder, or whole section",
        context: KeybindContext::WorkbenchSourceControl,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::OPEN_DIFF.label),
        description: "Open the diff, or the commit under the cursor",
        context: KeybindContext::WorkbenchSourceControl,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::DISCARD.label),
        description: "Discard changes (press twice)",
        context: KeybindContext::WorkbenchSourceControl,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::TOGGLE_TREE.label),
        description: "Switch the change sections between tree and flat",
        context: KeybindContext::WorkbenchSourceControl,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(wb::SHRINK_SECTION.label, wb::GROW_SECTION.label),
        description: "Shrink / grow the section the cursor is in",
        context: KeybindContext::WorkbenchSourceControl,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Run the search, then open the file at the match",
        context: KeybindContext::WorkbenchSearch,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::NEXT_FIELD.label),
        description: "Move between the query and the file globs",
        context: KeybindContext::WorkbenchSearch,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::TOGGLE_CASE.label),
        description: "Match case",
        context: KeybindContext::WorkbenchSearch,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::TOGGLE_WORD.label),
        description: "Match whole words",
        context: KeybindContext::WorkbenchSearch,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(wb::TOGGLE_REGEX.label),
        description: "Read the query as a regular expression",
        context: KeybindContext::WorkbenchSearch,
        platform: Platform::All,
    },
];

pub fn all_contexts() -> impl Iterator<Item = KeybindContext> {
    use strum::IntoEnumIterator;
    KeybindContext::iter()
}

pub(crate) fn key_event_to_string(key: &KeyEvent) -> String {
    let mut s = String::new();
    let mods = key.modifiers;
    let is_char = matches!(key.code, KeyCode::Char(_));
    if mods.contains(KeyModifiers::CONTROL) {
        s.push_str("ctrl+");
    }
    if mods.contains(KeyModifiers::ALT) {
        s.push_str("alt+");
    }
    if mods.contains(KeyModifiers::SHIFT) && !is_char {
        s.push_str("shift+");
    }
    match key.code {
        KeyCode::Char(' ') => s.push_str("space"),
        KeyCode::Char(c) => s.push(c),
        KeyCode::Enter => s.push_str("enter"),
        KeyCode::Esc => s.push_str("esc"),
        KeyCode::Tab => s.push_str("tab"),
        KeyCode::BackTab => {
            if !s.contains("shift+") {
                s.insert_str(0, "shift+");
            }
            s.push_str("tab");
        }
        KeyCode::Backspace => s.push_str("backspace"),
        KeyCode::Delete => s.push_str("delete"),
        KeyCode::Up => s.push_str("up"),
        KeyCode::Down => s.push_str("down"),
        KeyCode::Left => s.push_str("left"),
        KeyCode::Right => s.push_str("right"),
        KeyCode::Home => s.push_str("home"),
        KeyCode::End => s.push_str("end"),
        KeyCode::PageUp => s.push_str("pageup"),
        KeyCode::PageDown => s.push_str("pagedown"),
        KeyCode::F(n) => write!(s, "f{n}").unwrap(),
        KeyCode::Insert => s.push_str("insert"),
        _ => {}
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use test_case::test_case;

    const ALT_LABEL: &str = "Alt+";
    const ALT_IS_UNREACHABLE: &str = "macOS never reports Option as Alt, so no default may need it";
    const AMBIGUOUS_CONTROL_CODE: &str =
        "this chord is the same byte as Backspace, Tab or Enter and would steal it";
    const CHORD_COLLISION: &str = "two leader chords in one context answer the same second key";
    const LEADER_DRIFT: &str = "the workbench and the host must spend the same key on the prefix";

    #[test_case(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), "ctrl+d")]
    #[test_case(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT), "alt+x")]
    #[test_case(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT), "shift+tab")]
    #[test_case(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), "shift+tab")]
    #[test_case(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), "space")]
    #[test_case(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE), "f5")]
    #[test_case(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), "a")]
    fn key_event_to_string_cases(input: KeyEvent, expected: &str) {
        assert_eq!(key_event_to_string(&input), expected);
    }

    #[test]
    fn bind_requires_exact_modifiers() {
        let bind = key::OPEN_EDITOR; // Ctrl+O
        let exact = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        let extra = KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let wrong = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT);

        assert!(bind.matches(exact));
        assert!(!bind.matches(extra), "extra modifiers should not match");
        assert!(!bind.matches(wrong), "wrong modifier should not match");
    }

    #[test]
    fn every_context_has_at_least_one_keybind() {
        for ctx in all_contexts() {
            let has_own = KEYBINDS.iter().any(|kb| kb.context == ctx);
            let has_parent = ctx
                .parent()
                .is_some_and(|p| KEYBINDS.iter().any(|kb| kb.context == p));
            assert!(
                has_own || has_parent,
                "context {:?} has no keybinds and no parent with keybinds",
                ctx,
            );
        }
    }

    /// macOS routes Option through the input method, so an `Alt` default is
    /// unreachable for a whole platform. Chords are how Caudra spends that
    /// keyspace instead.
    #[test]
    fn no_default_depends_on_alt() {
        for kb in KEYBINDS {
            for part in kb.label.parts() {
                assert!(
                    !part.contains(ALT_LABEL),
                    "{ALT_IS_UNREACHABLE}: {part} ({})",
                    kb.description
                );
            }
        }
    }

    /// A terminal sends the same byte for `Ctrl+H` and Backspace, `Ctrl+M` and
    /// Enter, `Ctrl+I` and Tab, so binding one of them steals the other.
    #[test_case(mod_key!("H") ; "backspace")]
    #[test_case(mod_key!("I") ; "tab")]
    #[test_case(mod_key!("M") ; "enter")]
    fn no_default_binds_an_ambiguous_control_code(ambiguous: &str) {
        for kb in KEYBINDS {
            for part in kb.label.parts() {
                assert_ne!(
                    part, ambiguous,
                    "{AMBIGUOUS_CONTROL_CODE}: {}",
                    kb.description
                );
            }
        }
    }

    /// The workbench spells its labels from its own copy of the prefix and
    /// hands the key back by its own `Bind`. If the two drifted, every
    /// workbench chord would be documented under a prefix that no longer
    /// reaches it.
    #[test]
    fn the_workbench_agrees_with_the_host_about_the_leader() {
        assert_eq!(wb::LEADER.code, key::LEADER.code, "{LEADER_DRIFT}");
        assert_eq!(
            wb::LEADER.modifiers,
            key::LEADER.modifiers,
            "{LEADER_DRIFT}"
        );
        assert_eq!(wb::LEADER_LABEL, LEADER_PREFIX, "{LEADER_DRIFT}");
    }

    #[test]
    fn no_two_chords_in_one_context_share_a_second_key() {
        for ctx in all_contexts() {
            let mut seen: Vec<&str> = Vec::new();
            for chord in leader_chords(&[ctx]) {
                assert!(
                    !seen.contains(&chord.key),
                    "{CHORD_COLLISION}: {LEADER_PREFIX} {} in {ctx:?}",
                    chord.key
                );
                seen.push(chord.key);
            }
        }
    }

    #[test]
    fn no_duplicate_entries() {
        for (i, a) in KEYBINDS.iter().enumerate() {
            for (j, b) in KEYBINDS.iter().enumerate() {
                if i != j && a.context == b.context {
                    assert!(
                        a.label.flat_str() != b.label.flat_str() || a.description != b.description,
                        "duplicate keybind: {} - {} in {:?}",
                        a.label.flat_str(),
                        a.description,
                        a.context,
                    );
                }
            }
        }
    }
}
