//! `/settings`: the `[websearch]` backends with whether each is ready, a tab per group of
//! plain tables of the global `caudra.toml`, and pstack's model roles while pstack is on. Edits
//! go to a background thread that writes the file and keeps its comments; the config watcher
//! puts them in force. pstack's roles live in Caudra's state, which the app writes.

mod fields;
mod form;
mod list_form;
mod plain;
mod pstack;
mod websearch;

use std::mem;
use std::ops::Range;

use caudra_config::interpolation::holds_reference;
use caudra_config::settings_file::{GlobalSettingsFile, ProjectSettings};
use caudra_config::websearch::{
    GlobalWebsearchFile, WebsearchBackendName, WebsearchConfig, WebsearchEdit, WebsearchMode,
    is_secret_key,
};
use caudra_config::{
    AgentConfig, AutomationsConfig, AutopilotConfig, ConfigField, MessagingConfig, ProviderConfig,
    RetentionConfig, SKILL_OPTIONS, SnapshotsConfig, StorageConfig, TOP_LEVEL_FIELDS, UiConfig,
    WorktreesConfig,
};
use caudra_grab::grab_scope;
use caudra_workcell::BackendStatus;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use toml_edit::Value;
use unicode_width::UnicodeWidthStr;

use crate::components::command_text::ellipsize_spans;
use crate::components::docs_link::DocsTopic;
use crate::components::keybindings::key;
use crate::components::modal::{ESC_LABEL, FooterHits, FooterLine, Modal, SEPARATOR};
use crate::components::section_tabs::{SectionTab, digit_index, step_in, tab_strip_of};
use crate::components::{Overlay, ascii_key, plain_char, selectable_row};
use crate::event_loop::config_reload::ApplyTiming;
use crate::theme::{self, Theme};
use fields::{
    DOCS_BADGE, DOCS_BADGE_GAP, FieldKind, FieldRow, FieldState, current_bool, current_choice,
    current_list, field_info, field_row, key_path, list_value, name_cols, next_choice,
};
use form::{FormField, FormKey, SettingsForm};
use list_form::{ListForm, ListKey};
use plain::{Group, PlainRow, filtered_rows, heading, tab_rows, timing_of};
pub(crate) use pstack::{PstackAction, PstackRoles, RoleEdit, apply as apply_role_edit};
use pstack::{PstackRow, next_effort, next_thinking, row_info, row_lines};
use websearch::{
    BackendRow, RowColumns, backend_info, backend_line, backend_rows, literal_warning, mode_line,
};
pub(crate) use websearch::{environment_backends, environment_mode};

pub(crate) const MOVE_UP_LABEL: &str = "[";
pub(crate) const MOVE_DOWN_LABEL: &str = "]";
pub(crate) const MODE_LABEL: &str = "m";
pub(crate) const RESET_LABEL: &str = "d";
pub(crate) const FILTER_LABEL: &str = "/";
pub(crate) const DOCS_LABEL: &str = "?";
pub(crate) const THINKING_LABEL: &str = "t";
pub(crate) const AUTO_LABEL: &str = "R";
const TOGGLE_KEY: char = ' ';
const THINKING_KEY: char = ascii_key(THINKING_LABEL);
const AUTO_KEY: char = ascii_key(AUTO_LABEL);
const MOVE_UP_KEY: char = ascii_key(MOVE_UP_LABEL);
const MOVE_DOWN_KEY: char = ascii_key(MOVE_DOWN_LABEL);
const MODE_KEY: char = ascii_key(MODE_LABEL);
const RESET_KEY: char = ascii_key(RESET_LABEL);
const FILTER_KEY: char = ascii_key(FILTER_LABEL);
const DOCS_KEY: char = ascii_key(DOCS_LABEL);
const TITLE: &str = " Settings ";
const WIDTH_PERCENT: u16 = 90;
const MAX_HEIGHT_PERCENT: u16 = 85;
const H_PAD: u16 = 1;
const INFO_MAX_ROWS: u16 = 9;
const MORE_ABOVE: &str = "▴";
const MORE_BELOW: &str = "▾";
const CURSOR_MARK: &str = "\u{203a} ";
const NO_MARK: &str = "  ";
const UI_TABLE: &[&str] = &["ui"];
const AGENT_TABLE: &[&str] = &["agent"];
const MESSAGING_TABLE: &[&str] = &["agent", "messaging"];
const SKILL_TABLE: &[&str] = &["plugins", "skill"];
const PROVIDER_TABLE: &[&str] = &["provider"];
const STORAGE_TABLE: &[&str] = &["storage"];
const SNAPSHOTS_TABLE: &[&str] = &["storage", "snapshots"];
const RETENTION_TABLE: &[&str] = &["storage", "retention"];
const WORKTREES_TABLE: &[&str] = &["worktrees"];
const AUTOPILOT_TABLE: &[&str] = &["autopilot"];
const AUTOMATIONS_TABLE: &[&str] = &["automations"];
const ROOT_LEAD: &str = "Top-level keys of the global caudra.toml";
const TABLE_LEAD_SUFFIX: &str = " of the global caudra.toml";
const LEAD_SEPARATOR: &str = ", ";
const FILTER_PREFIX: &str = "Filter: ";
const FILTER_CURSOR: &str = "▏";
const FILTER_HINT: &str = "  type a key or table name";
const NO_MATCHES: &str = "No key matches.";
const LOADING: &str = "Loading…";
const SAVING: &str = "Saving…";
const SAVED: &str = "Saved to the global caudra.toml.";
const SAVED_APPLIES: &str = "Saved to the global caudra.toml; it applies ";
const SENTENCE_END: &str = ".";
const BUSY: &str = "Wait for the last change to finish.";
const NOT_LOADED: &str = "Wait for the settings to load.";
pub(crate) const REMOTE: &str =
    "Read-only: web tools run on the remote Workcell, which reads its own environment.";
const NOT_LISTED: &str = "Check the backend first: only listed backends have a place to move.";
const ALREADY_DEFAULT: &str = "backends is not set, so the environment already decides.";
const NO_FIELDS: &str = "Exa MCP needs no settings.";
const BACKEND_HINT: &str = "Each value is a literal, {env:NAME}, or {file:PATH}. An empty value removes the key, so its variable decides. Tab moves, Ctrl+R reveals, Enter saves, Esc cancels.";
const FIELD_HINT: &str =
    "An empty value removes the key, so the default applies. Enter saves, Esc cancels.";
const FORM_TITLE_PREFIX: &str = " ";
const FORM_TITLE_SUFFIX: &str = " ";
const FOOTER_GAPS: [(bool, &str); 3] = [(true, SEPARATOR), (false, "  "), (false, " ")];
const PSTACK_SAVED: &str = "Saved; pstack uses it from its next page load.";
const EFFORT_ALREADY_DEFAULT: &str = "Effort is already the default.";
const ALREADY_AUTO: &str = "This kind is already on auto.";
const NOT_A_SEAT: &str = "Only a Panel or Cross-judge pool seat can be removed.";
const INHERIT_THINKING: &str = "An inherit seat takes no thinking level; pick a model first.";
const NO_LEVELS: &str = "This model takes no thinking level.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    Websearch,
    General,
    Ui,
    Agent,
    Skills,
    Provider,
    Storage,
    Worktrees,
    Autopilot,
    Automations,
    /// Listed only while pstack is on, last so it moves no other tab's digit.
    Pstack,
}

impl SectionTab for SettingsTab {
    const ALL: &'static [Self] = &[
        Self::Websearch,
        Self::General,
        Self::Ui,
        Self::Agent,
        Self::Skills,
        Self::Provider,
        Self::Storage,
        Self::Worktrees,
        Self::Autopilot,
        Self::Automations,
        Self::Pstack,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Websearch => "Web search",
            Self::General => "General",
            Self::Ui => "UI",
            Self::Agent => "Agent",
            Self::Skills => "Skills",
            Self::Provider => "Provider",
            Self::Storage => "Storage",
            Self::Worktrees => "Worktrees",
            Self::Autopilot => "Autopilot",
            Self::Automations => "Automations",
            Self::Pstack => "pstack",
        }
    }
}

const GENERAL_GROUPS: &[Group] = &[Group {
    table: &[],
    fields: TOP_LEVEL_FIELDS,
    docs: DocsTopic::ConfigTopLevel,
}];
const UI_GROUPS: &[Group] = &[Group {
    table: UI_TABLE,
    fields: UiConfig::FIELDS,
    docs: DocsTopic::ConfigUi,
}];
const AGENT_GROUPS: &[Group] = &[
    Group {
        table: AGENT_TABLE,
        fields: AgentConfig::FIELDS,
        docs: DocsTopic::ConfigAgent,
    },
    Group {
        table: MESSAGING_TABLE,
        fields: MessagingConfig::FIELDS,
        docs: DocsTopic::ConfigMessaging,
    },
];
const SKILL_GROUPS: &[Group] = &[Group {
    table: SKILL_TABLE,
    fields: SKILL_OPTIONS,
    docs: DocsTopic::ConfigSkills,
}];
const PROVIDER_GROUPS: &[Group] = &[Group {
    table: PROVIDER_TABLE,
    fields: ProviderConfig::FIELDS,
    docs: DocsTopic::ConfigProvider,
}];
const STORAGE_GROUPS: &[Group] = &[
    Group {
        table: STORAGE_TABLE,
        fields: StorageConfig::FIELDS,
        docs: DocsTopic::ConfigStorage,
    },
    Group {
        table: SNAPSHOTS_TABLE,
        fields: SnapshotsConfig::FIELDS,
        docs: DocsTopic::ConfigSnapshots,
    },
    Group {
        table: RETENTION_TABLE,
        fields: RetentionConfig::FIELDS,
        docs: DocsTopic::ConfigRetention,
    },
];
const WORKTREES_GROUPS: &[Group] = &[Group {
    table: WORKTREES_TABLE,
    fields: WorktreesConfig::FIELDS,
    docs: DocsTopic::ConfigWorktrees,
}];
const AUTOPILOT_GROUPS: &[Group] = &[Group {
    table: AUTOPILOT_TABLE,
    fields: AutopilotConfig::FIELDS,
    docs: DocsTopic::ConfigAutopilot,
}];
const AUTOMATIONS_GROUPS: &[Group] = &[Group {
    table: AUTOMATIONS_TABLE,
    fields: AutomationsConfig::FIELDS,
    docs: DocsTopic::ConfigAutomations,
}];

impl SettingsTab {
    /// The tables a plain tab edits, none for Web search or pstack.
    fn groups(self) -> &'static [Group] {
        match self {
            Self::Websearch | Self::Pstack => &[],
            Self::General => GENERAL_GROUPS,
            Self::Ui => UI_GROUPS,
            Self::Agent => AGENT_GROUPS,
            Self::Skills => SKILL_GROUPS,
            Self::Provider => PROVIDER_GROUPS,
            Self::Storage => STORAGE_GROUPS,
            Self::Worktrees => WORKTREES_GROUPS,
            Self::Autopilot => AUTOPILOT_GROUPS,
            Self::Automations => AUTOMATIONS_GROUPS,
        }
    }
}

/// The global `caudra.toml` as written, read twice: once for plain keys, once for `[websearch]`.
#[derive(Clone)]
pub(crate) struct SettingsFiles {
    pub(crate) settings: GlobalSettingsFile,
    pub(crate) websearch: GlobalWebsearchFile,
}

pub(crate) struct SettingsData {
    pub(crate) files: SettingsFiles,
    /// The keys this project's `caudra.toml` sets, which win over the global ones.
    pub(crate) project: ProjectSettings,
    /// Every backend as the config and the environment set it up; empty for a remote Workcell.
    pub(crate) statuses: Vec<BackendStatus>,
    /// What the environment lists while `backends` is unset.
    pub(crate) default_backends: Vec<WebsearchBackendName>,
    pub(crate) default_mode: WebsearchMode,
}

pub(crate) enum SettingsEdit {
    Websearch(Vec<WebsearchEdit>),
    /// `None` removes the key.
    Key {
        table: &'static [&'static str],
        key: &'static str,
        value: Option<Value>,
    },
}

pub(crate) enum SettingsRequest {
    Load,
    Edit {
        files: Box<SettingsFiles>,
        edit: Box<SettingsEdit>,
    },
}

pub(crate) struct SettingsReply {
    pub(crate) data: Result<Box<SettingsData>, String>,
    /// Why the edit was refused.
    pub(crate) error: Option<String>,
    /// What the edit left in force, such as the backends web search now uses.
    pub(crate) note: Option<String>,
}

pub(crate) enum SettingsAction {
    Consumed,
    Close,
    Request(SettingsRequest),
    /// Opens the docs over the modal, which stays open beneath.
    OpenDocs(DocsTopic),
    Pstack(PstackAction),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Work {
    Load,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FooterCommand {
    Toggle,
    Edit,
    MoveUp,
    MoveDown,
    Mode,
    Reset,
    Thinking,
    Auto,
    Remove,
    Filter,
    Docs,
    Section,
    Close,
}

impl FooterCommand {
    fn hint(self) -> (&'static str, &'static str) {
        match self {
            Self::Toggle => (key::SPACE.label, "Use"),
            Self::Edit => (key::ENTER.label, "Edit"),
            Self::MoveUp => (MOVE_UP_LABEL, "Up"),
            Self::MoveDown => (MOVE_DOWN_LABEL, "Down"),
            Self::Mode => (MODE_LABEL, "Mode"),
            Self::Reset => (RESET_LABEL, "Reset"),
            Self::Thinking => (THINKING_LABEL, "Thinking"),
            Self::Auto => (AUTO_LABEL, "Auto"),
            Self::Remove => (RESET_LABEL, "Remove"),
            Self::Filter => (FILTER_LABEL, "Filter"),
            Self::Docs => (DOCS_LABEL, "Docs"),
            Self::Section => (key::TAB.label, "Section"),
            Self::Close => (ESC_LABEL, "Close"),
        }
    }
}

/// A plain key a form saves to.
#[derive(Clone, Copy)]
struct KeyTarget {
    table: &'static [&'static str],
    field: &'static ConfigField,
}

/// What an open form saves to.
enum FormTarget {
    Backend(WebsearchBackendName),
    Key(KeyTarget),
}

/// Text that narrows the plain keys of every tab, typed while `focused`.
struct Filter {
    query: String,
    focused: bool,
}

pub(crate) struct SettingsModal {
    open: bool,
    tab: SettingsTab,
    /// Whether this session's web tools run in the local Workcell, which these settings drive.
    local: bool,
    data: Option<SettingsData>,
    /// The backend the Web search tab has selected, followed across a reorder.
    backend: WebsearchBackendName,
    /// The plain row selected, by its place among the rows shown; always a key, never a heading.
    field: usize,
    /// pstack's roles while pstack is on, which lists its tab.
    pstack: Option<PstackRoles>,
    /// The pstack row selected, by its place among the rows shown.
    pstack_row: usize,
    filter: Option<Filter>,
    form: Option<(SettingsForm, FormTarget)>,
    list_form: Option<(ListForm, KeyTarget)>,
    work: Option<Work>,
    /// When the key being written applies, for the message once it is saved.
    saving: Option<ApplyTiming>,
    message: Option<String>,
    error: Option<String>,
    offset: usize,
    info_scroll: u16,
    /// The selection moved, so the next draw scrolls it into view.
    reveal_cursor: bool,
    pointer: Option<Position>,
    popup: Rect,
    list_area: Rect,
    info_area: Rect,
    docs_hit: Option<Rect>,
    row_columns: RowColumns,
    /// Each drawn row's choice chips, by row, as columns after the cursor mark.
    row_chips: Vec<Vec<(Range<u16>, &'static str)>>,
    tab_hits: Vec<(Rect, SettingsTab)>,
    mode_hits: Vec<(Rect, WebsearchMode)>,
    footer: Vec<FooterCommand>,
    footer_hits: FooterHits,
}

impl SettingsModal {
    pub(crate) fn new() -> Self {
        Self {
            open: false,
            tab: SettingsTab::Websearch,
            local: false,
            data: None,
            backend: WebsearchBackendName::ALL[0],
            field: 0,
            pstack: None,
            pstack_row: 0,
            filter: None,
            form: None,
            list_form: None,
            work: None,
            saving: None,
            message: None,
            error: None,
            offset: 0,
            info_scroll: 0,
            reveal_cursor: false,
            pointer: None,
            popup: Rect::default(),
            list_area: Rect::default(),
            info_area: Rect::default(),
            docs_hit: None,
            row_columns: RowColumns::default(),
            row_chips: Vec::new(),
            tab_hits: Vec::new(),
            mode_hits: Vec::new(),
            footer: Vec::new(),
            footer_hits: FooterHits::default(),
        }
    }

    pub(crate) fn open(&mut self, local: bool) -> SettingsAction {
        *self = Self {
            open: true,
            local,
            work: Some(Work::Load),
            ..Self::new()
        };
        SettingsAction::Request(SettingsRequest::Load)
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn receive(&mut self, reply: SettingsReply) {
        if !self.open {
            return;
        }
        let wrote = self.work == Some(Work::Write);
        let saving = self.saving.take();
        self.work = None;
        self.reveal_cursor = true;
        let failure = reply
            .data
            .map(|data| {
                if self.data.is_none() {
                    self.backend = websearch_listed(&data)
                        .first()
                        .copied()
                        .unwrap_or(self.backend);
                }
                self.data = Some(*data);
            })
            .err();
        self.error = reply.error.or(failure);
        self.message = reply.note.or_else(|| {
            (wrote && self.error.is_none()).then(|| match saving {
                Some(timing) => format!("{SAVED_APPLIES}{}{SENTENCE_END}", timing.suffix()),
                None => SAVED.to_owned(),
            })
        });
    }

    /// Whether typed text belongs to a form or the filter, rather than to the list's keys.
    pub(crate) fn text_input_active(&self) -> bool {
        self.form.is_some()
            || self
                .list_form
                .as_ref()
                .is_some_and(|(form, _)| form.takes_text())
            || self.filter.as_ref().is_some_and(|filter| filter.focused)
    }

    pub(crate) fn handle_text(&mut self, text: &str, paste: bool) {
        if let Some((form, _)) = self.form.as_mut() {
            form.handle_text(text, paste);
        } else if let Some((form, _)) = self.list_form.as_mut() {
            form.handle_text(text, paste);
        } else if let Some(filter) = self.filter.as_mut().filter(|filter| filter.focused) {
            filter.query.push_str(text.trim_end_matches(['\n', '\r']));
            self.select_first_key();
        }
    }

    pub(crate) fn handle_paste(&mut self, text: &str) -> bool {
        if !self.open {
            return false;
        }
        self.handle_text(text, true);
        true
    }

    pub(crate) fn contains(&self, position: Position) -> bool {
        self.open
            && (self.popup.contains(position)
                || self
                    .form
                    .as_ref()
                    .is_some_and(|(form, _)| form.contains(position))
                || self
                    .list_form
                    .as_ref()
                    .is_some_and(|(form, _)| form.contains(position)))
    }

    /// The wheel moves the info pane under the pointer, else the list, leaving the selection
    /// where it is.
    pub(crate) fn scroll_at(&mut self, position: Position, delta: i32) {
        if self.form.is_some() || self.list_form.is_some() {
            return;
        }
        let delta = isize::try_from(delta).unwrap_or_default();
        if self.info_area.contains(position) {
            self.info_scroll =
                u16::try_from(usize::from(self.info_scroll).saturating_add_signed(-delta))
                    .unwrap_or(u16::MAX);
        } else {
            self.offset = self.offset.saturating_add_signed(-delta);
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> SettingsAction {
        if let Some((form, _)) = self.form.as_mut() {
            return match form.handle_key(key) {
                FormKey::Consumed => SettingsAction::Consumed,
                FormKey::Cancel => {
                    self.form = None;
                    SettingsAction::Consumed
                }
                FormKey::Submit => self.submit_form(),
            };
        }
        if let Some((form, _)) = self.list_form.as_mut() {
            return match form.handle_key(key) {
                ListKey::Consumed => SettingsAction::Consumed,
                ListKey::Cancel => {
                    self.list_form = None;
                    SettingsAction::Consumed
                }
                ListKey::Save(items) => self.submit_list(items),
            };
        }
        self.message = None;
        if self.filter.as_ref().is_some_and(|filter| filter.focused) {
            return self.handle_filter_key(key);
        }
        if key.code == KeyCode::Esc && self.filter.is_some() {
            self.clear_filter();
            return SettingsAction::Consumed;
        }
        if key.code == KeyCode::Esc || key::QUIT.matches(key) {
            return SettingsAction::Close;
        }
        let shifted = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Tab => self.step_tab(1),
            KeyCode::BackTab => self.step_tab(-1),
            KeyCode::Up if shifted => self.scroll_info(-1),
            KeyCode::Down if shifted => self.scroll_info(1),
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::PageUp => self.step(-self.page()),
            KeyCode::PageDown => self.step(self.page()),
            KeyCode::Home => self.step(isize::MIN),
            KeyCode::End => self.step(isize::MAX),
            KeyCode::Enter => return self.command(FooterCommand::Edit),
            KeyCode::Right if self.selected_pstack_row() == Some(PstackRow::Effort) => {
                return self.command(FooterCommand::Edit);
            }
            _ => {
                return match plain_char(&key) {
                    Some(TOGGLE_KEY) => self.command(FooterCommand::Toggle),
                    Some(MOVE_UP_KEY) => self.command(FooterCommand::MoveUp),
                    Some(MOVE_DOWN_KEY) => self.command(FooterCommand::MoveDown),
                    Some(MODE_KEY) => self.command(FooterCommand::Mode),
                    Some(RESET_KEY) if self.pstack_shown() => self.command(FooterCommand::Remove),
                    Some(RESET_KEY) => self.command(FooterCommand::Reset),
                    Some(THINKING_KEY) => self.command(FooterCommand::Thinking),
                    Some(AUTO_KEY) => self.command(FooterCommand::Auto),
                    Some(FILTER_KEY) => self.command(FooterCommand::Filter),
                    Some(DOCS_KEY) => self.command(FooterCommand::Docs),
                    Some(digit) => {
                        if let Some(tab) =
                            digit_index(digit).and_then(|index| self.tabs().get(index).copied())
                        {
                            self.set_tab(tab);
                        }
                        SettingsAction::Consumed
                    }
                    None => SettingsAction::Consumed,
                };
            }
        }
        SettingsAction::Consumed
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> SettingsAction {
        match key.code {
            KeyCode::Esc => self.clear_filter(),
            KeyCode::Enter | KeyCode::Down => {
                if let Some(filter) = self.filter.as_mut() {
                    filter.focused = false;
                }
            }
            KeyCode::Backspace => {
                if let Some(filter) = self.filter.as_mut() {
                    filter.query.pop();
                }
                self.select_first_key();
            }
            KeyCode::Tab => self.step_tab(1),
            KeyCode::BackTab => self.step_tab(-1),
            KeyCode::Char(typed)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.handle_text(typed.encode_utf8(&mut [0; 4]), false);
            }
            _ => {}
        }
        SettingsAction::Consumed
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> SettingsAction {
        let position = Position::new(event.column, event.row);
        self.pointer = Some(position);
        if self.form.is_some() || self.list_form.is_some() {
            return SettingsAction::Consumed;
        }
        if let Some(index) = self.footer_hits.handle_mouse(event) {
            return match self.footer.get(index) {
                Some(&command) => self.command(command),
                None => SettingsAction::Consumed,
            };
        }
        if event.kind != MouseEventKind::Down(MouseButton::Left) {
            return SettingsAction::Consumed;
        }
        self.message = None;
        if self.docs_hit.is_some_and(|hit| hit.contains(position)) {
            return self.command(FooterCommand::Docs);
        }
        if let Some(&(_, mode)) = self
            .mode_hits
            .iter()
            .find(|(hit, _)| hit.contains(position))
        {
            return self.pick_mode(mode);
        }
        if let Some(&(_, tab)) = self.tab_hits.iter().find(|(hit, _)| hit.contains(position)) {
            self.set_tab(tab);
            return SettingsAction::Consumed;
        }
        if self.list_area.contains(position) {
            return self.click_row(position);
        }
        SettingsAction::Consumed
    }

    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("settings_modal", area);
        let theme = theme::current();
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, area.height);
        self.popup = popup;
        let padded = Rect {
            x: inner.x.saturating_add(H_PAD),
            width: inner.width.saturating_sub(H_PAD.saturating_mul(2)),
            ..inner
        };
        let info = Paragraph::new(self.info_lines(&theme)).wrap(Wrap { trim: false });
        let info_total = u16::try_from(info.line_count(padded.width.max(1))).unwrap_or(u16::MAX);
        let info_rows = info_total.min(INFO_MAX_ROWS);
        let [tabs, lead, _, list, info_area, status, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(info_rows),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(padded);
        let (strip, hits) = tab_strip_of(&self.tabs(), self.tab, self.pointer, tabs);
        self.tab_hits = hits;
        frame.render_widget(Paragraph::new(strip), tabs);
        self.render_lead(frame, lead, &theme);
        self.render_list(frame, list, &theme);
        self.render_info(frame, info_area, info, info_total, &theme);
        if let Some(line) = self.status_line(&theme) {
            frame.render_widget(Paragraph::new(line), status);
        }
        self.render_footer(frame, footer, &theme);
        if let Some((form, _)) = self.form.as_mut() {
            form.view(frame, area);
        }
        if let Some((form, _)) = self.list_form.as_mut() {
            form.view(frame, area);
        }
        popup
    }

    #[cfg(test)]
    pub(crate) fn tab(&self) -> SettingsTab {
        self.tab
    }

    /// Whether the Web search tab shows, which a filter hides.
    fn websearch_shown(&self) -> bool {
        self.tab == SettingsTab::Websearch && self.filter.is_none()
    }

    /// Whether the pstack tab shows, which a filter hides.
    fn pstack_shown(&self) -> bool {
        self.tab == SettingsTab::Pstack && self.filter.is_none()
    }

    /// pstack's roles as the app read them, `None` while pstack is off, which hides its tab.
    pub(crate) fn set_pstack(&mut self, roles: Option<PstackRoles>) {
        if roles.is_none() && self.tab == SettingsTab::Pstack {
            self.set_tab(SettingsTab::Websearch);
        }
        self.pstack = roles;
        self.pstack_row = self
            .pstack_row
            .min(self.pstack_rows().len().saturating_sub(1));
        self.reveal_cursor = true;
    }

    /// The roles read back after an edit, and why it was not saved.
    pub(crate) fn pstack_saved(&mut self, roles: Option<PstackRoles>, error: Option<String>) {
        self.set_pstack(roles);
        self.message = error.is_none().then(|| PSTACK_SAVED.to_owned());
        self.error = error;
    }

    /// The tabs listed: every one, pstack's only while pstack is on.
    fn tabs(&self) -> Vec<SettingsTab> {
        SettingsTab::ALL
            .iter()
            .copied()
            .filter(|tab| *tab != SettingsTab::Pstack || self.pstack.is_some())
            .collect()
    }

    fn step_tab(&mut self, delta: isize) {
        self.set_tab(step_in(&self.tabs(), self.tab, delta));
    }

    fn pstack_rows(&self) -> Vec<PstackRow> {
        self.pstack
            .as_ref()
            .map(|roles| pstack::rows(&roles.table))
            .unwrap_or_default()
    }

    fn selected_pstack_row(&self) -> Option<PstackRow> {
        if !self.pstack_shown() {
            return None;
        }
        self.pstack_rows().get(self.pstack_row).copied()
    }

    fn set_tab(&mut self, tab: SettingsTab) {
        if tab == self.tab && self.filter.is_none() {
            return;
        }
        self.tab = tab;
        self.filter = None;
        self.field = 0;
        self.pstack_row = 0;
        self.offset = 0;
        self.info_scroll = 0;
        self.mode_hits.clear();
        self.footer_hits.clear();
    }

    fn clear_filter(&mut self) {
        self.filter = None;
        self.field = 0;
        self.offset = 0;
        self.info_scroll = 0;
        self.reveal_cursor = true;
    }

    fn page(&self) -> isize {
        isize::try_from(self.list_area.height.max(1)).unwrap_or(isize::MAX)
    }

    fn scroll_info(&mut self, delta: i16) {
        self.info_scroll = self.info_scroll.saturating_add_signed(delta);
    }

    /// The rows a plain tab or the filter shows.
    fn plain_rows(&self) -> Vec<PlainRow> {
        match &self.filter {
            Some(filter) => filtered_rows(
                SettingsTab::ALL.iter().flat_map(|tab| tab.groups()),
                &filter.query,
            ),
            None => tab_rows(self.tab.groups()),
        }
    }

    fn select_first_key(&mut self) {
        self.field = self
            .plain_rows()
            .iter()
            .position(PlainRow::is_key)
            .unwrap_or_default();
        self.offset = 0;
        self.info_scroll = 0;
        self.reveal_cursor = true;
    }

    fn cursor(&self) -> usize {
        if self.pstack_shown() {
            return self.pstack_row;
        }
        match self.websearch_shown() {
            true => self
                .backend_order()
                .iter()
                .position(|backend| *backend == self.backend)
                .unwrap_or_default(),
            false => self.field,
        }
    }

    fn step(&mut self, delta: isize) {
        let fixed = match (self.websearch_shown(), self.pstack_shown()) {
            (true, _) => Some(WebsearchBackendName::ALL.len()),
            (_, true) => Some(self.pstack_rows().len()),
            _ => None,
        };
        if let Some(rows) = fixed {
            let last = rows.saturating_sub(1);
            self.select(self.cursor().saturating_add_signed(delta).min(last));
            return;
        }
        let keys: Vec<usize> = self
            .plain_rows()
            .iter()
            .enumerate()
            .filter(|(_, row)| row.is_key())
            .map(|(index, _)| index)
            .collect();
        let Some(last) = keys.len().checked_sub(1) else {
            return;
        };
        let at = keys
            .iter()
            .position(|index| *index >= self.field)
            .unwrap_or(last);
        self.select(keys[at.saturating_add_signed(delta).min(last)]);
    }

    fn select(&mut self, index: usize) {
        self.reveal_cursor = true;
        self.info_scroll = 0;
        if self.pstack_shown() {
            if index < self.pstack_rows().len() {
                self.pstack_row = index;
            }
            return;
        }
        match self.websearch_shown() {
            true => {
                if let Some(backend) = self.backend_order().get(index) {
                    self.backend = *backend;
                }
            }
            false => {
                if self.plain_rows().get(index).is_some_and(PlainRow::is_key) {
                    self.field = index;
                }
            }
        }
    }

    fn click_row(&mut self, position: Position) -> SettingsAction {
        let index = self.offset + usize::from(position.y - self.list_area.y);
        let rows = match (self.websearch_shown(), self.pstack_shown()) {
            (true, _) => WebsearchBackendName::ALL.len(),
            (_, true) => self.pstack_rows().len(),
            _ => self.plain_rows().len(),
        };
        if index >= rows {
            return SettingsAction::Consumed;
        }
        let selected = index == self.cursor();
        self.select(index);
        if index != self.cursor() {
            return SettingsAction::Consumed;
        }
        let column = (position.x - self.list_area.x)
            .saturating_sub(u16::try_from(CURSOR_MARK.width()).unwrap_or(u16::MAX));
        if self.websearch_shown() {
            let columns = &self.row_columns;
            if columns.check.contains(&column) {
                return self.command(FooterCommand::Toggle);
            }
            if columns.up.contains(&column) {
                return self.command(FooterCommand::MoveUp);
            }
            if columns.down.contains(&column) {
                return self.command(FooterCommand::MoveDown);
            }
        } else if let Some(choice) = self
            .row_chips
            .get(index)
            .and_then(|chips| chips.iter().find(|(columns, _)| columns.contains(&column)))
            .map(|(_, choice)| *choice)
        {
            return self.pick_choice(choice);
        }
        match selected {
            true => self.command(FooterCommand::Edit),
            false => SettingsAction::Consumed,
        }
    }

    fn command(&mut self, command: FooterCommand) -> SettingsAction {
        let websearch = self.websearch_shown();
        match (command, websearch) {
            (FooterCommand::Section, _) => {
                self.step_tab(1);
                SettingsAction::Consumed
            }
            (FooterCommand::Close, _) => SettingsAction::Close,
            (FooterCommand::Docs, _) => match self.docs_topic() {
                Some(topic) => SettingsAction::OpenDocs(topic),
                None => SettingsAction::Consumed,
            },
            (FooterCommand::Filter, _) => {
                match self.filter.as_mut() {
                    Some(filter) => filter.focused = true,
                    None => {
                        self.filter = Some(Filter {
                            query: String::new(),
                            focused: true,
                        });
                        self.select_first_key();
                    }
                }
                SettingsAction::Consumed
            }
            _ if self.refused() => SettingsAction::Consumed,
            _ if self.pstack_shown() => self.pstack_command(command),
            (FooterCommand::Thinking | FooterCommand::Auto | FooterCommand::Remove, _) => {
                SettingsAction::Consumed
            }
            (FooterCommand::Toggle, true) => self.toggle_backend(),
            (FooterCommand::Edit, true) => self.open_backend_form(),
            (FooterCommand::MoveUp, true) => self.move_backend(false),
            (FooterCommand::MoveDown, true) => self.move_backend(true),
            (FooterCommand::Mode, true) => {
                let modes = WebsearchMode::ALL;
                let current = modes
                    .iter()
                    .position(|mode| *mode == self.effective_mode())
                    .unwrap_or_default();
                self.pick_mode(modes[(current + 1) % modes.len()])
            }
            (FooterCommand::Reset, true) => self.reset_backends(),
            (FooterCommand::Toggle | FooterCommand::Edit, false) => self.change_field(),
            (FooterCommand::Reset, false) => self.reset_field(),
            (FooterCommand::MoveUp | FooterCommand::MoveDown | FooterCommand::Mode, false) => {
                SettingsAction::Consumed
            }
        }
    }

    /// Whether an edit cannot start now, saying why.
    fn refused(&mut self) -> bool {
        let reason = if self.work == Some(Work::Write) {
            Some(BUSY)
        } else if self.data.is_none() {
            Some(NOT_LOADED)
        } else if self.websearch_shown() && !self.local {
            Some(REMOTE)
        } else {
            None
        };
        if let Some(reason) = reason {
            self.message = Some(reason.to_owned());
        }
        reason.is_some()
    }

    fn commit(&mut self, edit: SettingsEdit) -> SettingsAction {
        let Some(data) = &self.data else {
            return SettingsAction::Consumed;
        };
        let files = Box::new(data.files.clone());
        self.saving = match &edit {
            SettingsEdit::Key { table, key, .. } => Some(timing_of(table, key)),
            SettingsEdit::Websearch(_) => None,
        };
        self.work = Some(Work::Write);
        self.error = None;
        self.message = None;
        SettingsAction::Request(SettingsRequest::Edit {
            files,
            edit: Box::new(edit),
        })
    }

    fn pstack_command(&mut self, command: FooterCommand) -> SettingsAction {
        let (Some(row), Some(roles)) = (self.selected_pstack_row(), self.pstack.as_ref()) else {
            return SettingsAction::Consumed;
        };
        let refusal = |message: &str| (None, Some(message.to_owned()));
        let (action, message) = match (command, row) {
            (FooterCommand::Edit | FooterCommand::Toggle, PstackRow::Effort) => {
                let effort = next_effort(roles.stored.effort.as_deref());
                (
                    Some(PstackAction::Edit(RoleEdit::Effort(Some(effort)))),
                    None,
                )
            }
            (FooterCommand::Edit | FooterCommand::Toggle, PstackRow::Seat { kind, seat }) => (
                Some(PstackAction::Pick {
                    kind,
                    seat: Some(seat),
                    model: roles.seat_model(kind, seat).map(str::to_owned),
                }),
                None,
            ),
            (FooterCommand::Edit | FooterCommand::Toggle, PstackRow::Add(kind)) => (
                Some(PstackAction::Pick {
                    kind,
                    seat: None,
                    model: None,
                }),
                None,
            ),
            (FooterCommand::Auto, PstackRow::Effort) if roles.stored.effort.is_none() => {
                refusal(EFFORT_ALREADY_DEFAULT)
            }
            (FooterCommand::Auto, PstackRow::Effort) => {
                (Some(PstackAction::Edit(RoleEdit::Effort(None))), None)
            }
            (FooterCommand::Auto, PstackRow::Seat { kind, .. } | PstackRow::Add(kind)) => {
                match roles.stores(kind) {
                    true => (Some(PstackAction::Edit(RoleEdit::Reset(kind))), None),
                    false => refusal(ALREADY_AUTO),
                }
            }
            (FooterCommand::Remove, PstackRow::Seat { kind, seat }) if kind.is_list() => (
                Some(PstackAction::Edit(RoleEdit::Remove { kind, seat })),
                None,
            ),
            (FooterCommand::Remove, _) => refusal(NOT_A_SEAT),
            (FooterCommand::Thinking, PstackRow::Seat { kind, seat }) => {
                match roles.seat_model(kind, seat) {
                    None => refusal(INHERIT_THINKING),
                    Some(model) if roles.levels(model).is_empty() => refusal(NO_LEVELS),
                    Some(model) => {
                        let thinking =
                            next_thinking(roles.stored_thinking(kind, seat), roles.levels(model));
                        (
                            Some(PstackAction::Edit(RoleEdit::Thinking {
                                kind,
                                seat,
                                thinking,
                            })),
                            None,
                        )
                    }
                }
            }
            _ => (None, None),
        };
        self.message = message;
        match action {
            Some(action) => {
                self.error = None;
                SettingsAction::Pstack(action)
            }
            None => SettingsAction::Consumed,
        }
    }

    fn written(&self) -> Option<&WebsearchConfig> {
        self.data
            .as_ref()
            .map(|data| data.files.websearch.written())
    }

    /// The backends in use: the configured list, else the environment's.
    fn listed(&self) -> Vec<WebsearchBackendName> {
        self.data.as_ref().map(websearch_listed).unwrap_or_default()
    }

    fn backend_order(&self) -> Vec<WebsearchBackendName> {
        backend_rows(&self.listed(), &[])
            .into_iter()
            .map(|row| row.backend)
            .collect()
    }

    fn effective_mode(&self) -> WebsearchMode {
        self.data
            .as_ref()
            .map(|data| {
                data.files
                    .websearch
                    .written()
                    .mode
                    .unwrap_or(data.default_mode)
            })
            .unwrap_or_default()
    }

    fn backends_edit(&mut self, backends: Option<Vec<WebsearchBackendName>>) -> SettingsAction {
        self.commit(SettingsEdit::Websearch(vec![WebsearchEdit::Backends(
            backends,
        )]))
    }

    fn toggle_backend(&mut self) -> SettingsAction {
        let mut listed = self.listed();
        match listed.iter().position(|backend| *backend == self.backend) {
            Some(index) => {
                listed.remove(index);
            }
            None => listed.push(self.backend),
        }
        self.backends_edit(Some(listed))
    }

    fn move_backend(&mut self, down: bool) -> SettingsAction {
        let mut listed = self.listed();
        let Some(index) = listed.iter().position(|backend| *backend == self.backend) else {
            self.message = Some(NOT_LISTED.to_owned());
            return SettingsAction::Consumed;
        };
        let target = match down {
            true => index + 1,
            false => index.wrapping_sub(1),
        };
        if target >= listed.len() {
            return SettingsAction::Consumed;
        }
        listed.swap(index, target);
        self.backends_edit(Some(listed))
    }

    fn reset_backends(&mut self) -> SettingsAction {
        if self
            .written()
            .is_some_and(|written| written.backends.is_none())
        {
            self.message = Some(ALREADY_DEFAULT.to_owned());
            return SettingsAction::Consumed;
        }
        self.backends_edit(None)
    }

    fn pick_mode(&mut self, mode: WebsearchMode) -> SettingsAction {
        if self.refused() || mode == self.effective_mode() {
            return SettingsAction::Consumed;
        }
        self.commit(SettingsEdit::Websearch(vec![WebsearchEdit::Mode(Some(
            mode,
        ))]))
    }

    fn open_backend_form(&mut self) -> SettingsAction {
        let backend = self.backend;
        let Some(written) = self.written() else {
            return SettingsAction::Consumed;
        };
        let fields: Vec<FormField> = backend
            .fields()
            .iter()
            .map(|field| {
                FormField::new(
                    field.name,
                    field.env,
                    field.description,
                    is_secret_key(field.name),
                    written
                        .value(backend, field.name)
                        .map(|value| value.expose())
                        .unwrap_or_default(),
                )
            })
            .collect();
        if fields.is_empty() {
            self.message = Some(NO_FIELDS.to_owned());
            return SettingsAction::Consumed;
        }
        let title = format!("{FORM_TITLE_PREFIX}{}{FORM_TITLE_SUFFIX}", backend.label());
        self.form = Some((
            SettingsForm::new(title, fields, BACKEND_HINT),
            FormTarget::Backend(backend),
        ));
        SettingsAction::Consumed
    }

    /// The selected plain key and the section of the reference that documents it.
    fn selected_key(&self) -> Option<(KeyTarget, DocsTopic)> {
        if self.websearch_shown() || self.pstack_shown() {
            return None;
        }
        match self.plain_rows().get(self.field)? {
            &PlainRow::Key { table, field, docs } => Some((KeyTarget { table, field }, docs)),
            PlainRow::Heading(_) => None,
        }
    }

    fn change_field(&mut self) -> SettingsAction {
        let Some((target, _)) = self.selected_key() else {
            return SettingsAction::Consumed;
        };
        let Some(data) = &self.data else {
            return SettingsAction::Consumed;
        };
        let KeyTarget { table, field } = target;
        let written = data.files.settings.get(table, field.name);
        let title = format!("{FORM_TITLE_PREFIX}{}{FORM_TITLE_SUFFIX}", field.name);
        let value = match FieldKind::of(field) {
            Some(FieldKind::Bool) => Value::from(!current_bool(field, written)),
            Some(FieldKind::Choice) => Value::from(next_choice(field, written)),
            Some(FieldKind::List) => {
                let items = current_list(field, written);
                let form = ListForm::new(title, field.description, items, field.choices);
                self.list_form = Some((form, target));
                return SettingsAction::Consumed;
            }
            _ => {
                let initial = written.map(fields::written_text).unwrap_or_default();
                let input =
                    FormField::new(field.name, field.env, field.description, false, &initial);
                self.form = Some((
                    SettingsForm::new(title, vec![input], FIELD_HINT),
                    FormTarget::Key(target),
                ));
                return SettingsAction::Consumed;
            }
        };
        self.commit(SettingsEdit::Key {
            table,
            key: field.name,
            value: Some(value),
        })
    }

    fn pick_choice(&mut self, choice: &'static str) -> SettingsAction {
        let Some((KeyTarget { table, field }, _)) = self.selected_key() else {
            return SettingsAction::Consumed;
        };
        let unchanged = current_choice(
            field,
            self.data
                .as_ref()
                .and_then(|data| data.files.settings.get(table, field.name)),
        ) == Some(choice);
        if unchanged || self.refused() {
            return SettingsAction::Consumed;
        }
        self.commit(SettingsEdit::Key {
            table,
            key: field.name,
            value: Some(Value::from(choice)),
        })
    }

    fn reset_field(&mut self) -> SettingsAction {
        let Some((KeyTarget { table, field }, _)) = self.selected_key() else {
            return SettingsAction::Consumed;
        };
        self.commit(SettingsEdit::Key {
            table,
            key: field.name,
            value: None,
        })
    }

    fn submit_list(&mut self, items: Vec<String>) -> SettingsAction {
        let Some((_, KeyTarget { table, field })) = self.list_form.take() else {
            return SettingsAction::Consumed;
        };
        let written = self
            .data
            .as_ref()
            .and_then(|data| data.files.settings.get(table, field.name));
        if written.is_some() && items == current_list(field, written) {
            return SettingsAction::Consumed;
        }
        self.commit(SettingsEdit::Key {
            table,
            key: field.name,
            value: Some(list_value(&items)),
        })
    }

    fn submit_form(&mut self) -> SettingsAction {
        let Some((mut form, target)) = self.form.take() else {
            return SettingsAction::Consumed;
        };
        let changes = form.changes();
        if changes.is_empty() {
            return SettingsAction::Consumed;
        }
        let edit = match &target {
            FormTarget::Backend(backend) => {
                let warning = changes
                    .iter()
                    .find(|(field, value)| {
                        field.secret
                            && value
                                .as_deref()
                                .is_some_and(|value| !holds_reference(value))
                    })
                    .map(|(field, _)| literal_warning(field.key, field.env));
                let edits = changes
                    .into_iter()
                    .map(|(field, value)| WebsearchEdit::Field {
                        backend: *backend,
                        key: field.key,
                        value,
                    })
                    .collect();
                match warning.is_some_and(|warning| form.warn(warning)) {
                    true => Err(()),
                    false => Ok(SettingsEdit::Websearch(edits)),
                }
            }
            &FormTarget::Key(KeyTarget { table, field }) => {
                let text = changes[0].1.clone().unwrap_or_default();
                match FieldKind::of(field).map(|kind| kind.parse(field, &text)) {
                    Some(Ok(value)) => Ok(SettingsEdit::Key {
                        table,
                        key: field.name,
                        value,
                    }),
                    Some(Err(error)) => {
                        form.fail(error);
                        Err(())
                    }
                    None => Err(()),
                }
            }
        };
        match edit {
            Ok(edit) => self.commit(edit),
            Err(()) => {
                self.form = Some((form, target));
                SettingsAction::Consumed
            }
        }
    }

    fn render_lead(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let hovered = self.pointer.and_then(|at| {
            self.mode_hits
                .iter()
                .find(|(hit, _)| hit.contains(at))
                .map(|(_, mode)| *mode)
        });
        self.mode_hits.clear();
        let line = if let Some(filter) = &self.filter {
            let mut spans = vec![
                Span::styled(FILTER_PREFIX, theme.tool_dim),
                Span::raw(filter.query.clone()),
            ];
            if filter.focused {
                spans.push(Span::styled(FILTER_CURSOR, theme.accent));
                if filter.query.is_empty() {
                    spans.push(Span::styled(FILTER_HINT, theme.input_placeholder));
                }
            }
            Line::from(spans)
        } else if self.tab == SettingsTab::Websearch {
            let (line, chips) = mode_line(self.effective_mode(), hovered, theme);
            if line.width() <= usize::from(area.width) {
                self.mode_hits = chips
                    .into_iter()
                    .map(|(columns, mode)| {
                        let hit = Rect::new(
                            area.x.saturating_add(columns.start),
                            area.y,
                            columns.end.saturating_sub(columns.start),
                            1,
                        );
                        (hit, mode)
                    })
                    .collect();
            }
            line
        } else if let Some(roles) = self.pstack.as_ref().filter(|_| self.pstack_shown()) {
            let style = match roles.table.task_model_choice {
                true => theme.tool_dim,
                false => theme.tool_warning,
            };
            Line::styled(pstack::lead(roles), style)
        } else {
            let tables: Vec<String> = self
                .tab
                .groups()
                .iter()
                .map(|group| heading(group.table))
                .collect();
            match tables.iter().all(String::is_empty) {
                true => Line::styled(ROOT_LEAD, theme.tool_dim),
                false => Line::styled(
                    format!("{}{TABLE_LEAD_SUFFIX}", tables.join(LEAD_SEPARATOR)),
                    theme.tool_dim,
                ),
            }
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn list_rows(&mut self, theme: &Theme) -> Vec<Vec<Span<'static>>> {
        self.row_chips.clear();
        let Some(data) = &self.data else {
            return Vec::new();
        };
        if let Some(roles) = self.pstack.as_ref().filter(|_| self.pstack_shown()) {
            return row_lines(&pstack::rows(&roles.table), roles, theme);
        }
        if self.websearch_shown() {
            let listed = websearch_listed(data);
            let rows = backend_rows(&listed, &data.statuses);
            let mut columns = RowColumns::default();
            let lines = rows
                .iter()
                .map(|row| {
                    let (spans, row_columns) = backend_line(row, listed.len(), theme);
                    columns = row_columns;
                    spans
                })
                .collect();
            self.row_columns = columns;
            return lines;
        }
        let rows = self.plain_rows();
        let width = name_cols(rows.iter().filter_map(|row| match row {
            PlainRow::Key { field, .. } => Some(*field),
            PlainRow::Heading(_) => None,
        }));
        let mut chips = Vec::with_capacity(rows.len());
        let lines = rows
            .iter()
            .map(|row| match *row {
                PlainRow::Heading(table) => {
                    chips.push(Vec::new());
                    let text = match table.is_empty() {
                        true => ROOT_LEAD.to_owned(),
                        false => heading(table),
                    };
                    vec![Span::styled(text, theme.tool_dim)]
                }
                PlainRow::Key { table, field, .. } => {
                    let FieldRow {
                        spans,
                        chips: row_chips,
                    } = field_row(
                        field,
                        data.files.settings.get(table, field.name),
                        data.project.sets(table, field.name),
                        width,
                        theme,
                    );
                    chips.push(row_chips);
                    spans
                }
            })
            .collect();
        self.row_chips = chips;
        lines
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.list_area = area;
        let rows = self.list_rows(theme);
        if rows.is_empty() {
            let text = match (self.work, &self.filter) {
                (Some(Work::Load), _) => Some(LOADING),
                (_, Some(_)) if self.data.is_some() => Some(NO_MATCHES),
                _ => None,
            };
            if let Some(text) = text {
                frame.render_widget(Paragraph::new(Line::styled(text, theme.tool_dim)), area);
            }
            return;
        }
        let height = usize::from(area.height.max(1));
        let cursor = self.cursor();
        if mem::take(&mut self.reveal_cursor) {
            self.offset = self
                .offset
                .min(cursor)
                .max((cursor + 1).saturating_sub(height));
        }
        self.offset = self.offset.min(rows.len().saturating_sub(height));
        let hovered = self
            .pointer
            .filter(|at| area.contains(*at))
            .map(|at| self.offset + usize::from(at.y - area.y));
        let width = usize::from(area.width);
        let lines: Vec<Line<'static>> = rows
            .into_iter()
            .enumerate()
            .skip(self.offset)
            .take(height)
            .map(|(index, row)| {
                let selected = index == cursor;
                let mark = match selected {
                    true => CURSOR_MARK,
                    false => NO_MARK,
                };
                let mut spans = vec![Span::styled(mark, theme.accent)];
                spans.extend(row);
                selectable_row(
                    ellipsize_spans(spans, width),
                    selected.then_some(theme.item_selected),
                    hovered == Some(index),
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_info(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        info: Paragraph<'static>,
        total: u16,
        theme: &Theme,
    ) {
        self.info_area = area;
        self.info_scroll = self.info_scroll.min(total.saturating_sub(area.height));
        frame.render_widget(info.scroll((self.info_scroll, 0)), area);
        if area.width > 0 && area.height > 0 {
            let column = area.right() - 1;
            if self.info_scroll > 0 {
                let at = Rect::new(column, area.y, 1, 1);
                frame.render_widget(Paragraph::new(Span::styled(MORE_ABOVE, theme.tool_dim)), at);
            }
            if self.info_scroll.saturating_add(area.height) < total {
                let at = Rect::new(column, area.bottom() - 1, 1, 1);
                frame.render_widget(Paragraph::new(Span::styled(MORE_BELOW, theme.tool_dim)), at);
            }
        }
        self.docs_hit = self
            .selected_key()
            .filter(|_| self.info_scroll == 0 && area.height > 0)
            .and_then(|(KeyTarget { table, field }, _)| {
                let start = key_path(table, field).width() + DOCS_BADGE_GAP.width();
                let end = start + DOCS_BADGE.width();
                (end <= usize::from(area.width)).then(|| {
                    Rect::new(
                        area.x + u16::try_from(start).unwrap_or(u16::MAX),
                        area.y,
                        u16::try_from(DOCS_BADGE.width()).unwrap_or(u16::MAX),
                        1,
                    )
                })
            });
    }

    fn info_lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        let Some(data) = &self.data else {
            return Vec::new();
        };
        if let (Some(row), Some(roles)) = (self.selected_pstack_row(), &self.pstack) {
            return row_info(row, roles, theme);
        }
        if self.websearch_shown() {
            let listed = websearch_listed(data);
            let rows = backend_rows(&listed, &data.statuses);
            return rows
                .iter()
                .find(|row| row.backend == self.backend)
                .map(|row: &BackendRow| {
                    let written = data.files.websearch.written();
                    backend_info(row, written, written.backends.is_none(), theme)
                })
                .unwrap_or_default();
        }
        self.selected_key()
            .map(|(KeyTarget { table, field }, _)| {
                let state = FieldState {
                    written: data.files.settings.get(table, field.name),
                    overridden: data.project.sets(table, field.name),
                    timing: timing_of(table, field.name),
                    badge_hovered: self
                        .pointer
                        .zip(self.docs_hit)
                        .is_some_and(|(at, hit)| hit.contains(at)),
                };
                field_info(field, table, state, theme)
            })
            .unwrap_or_default()
    }

    fn status_line(&self, theme: &Theme) -> Option<Line<'static>> {
        let (text, style) = if self.work == Some(Work::Write) {
            (SAVING.to_owned(), theme.tool_dim)
        } else if let Some(error) = &self.error {
            (error.clone(), theme.error)
        } else if let Some(message) = &self.message {
            (message.clone(), theme.tool_dim)
        } else if self.websearch_shown() && !self.local {
            (REMOTE.to_owned(), theme.tool_warning)
        } else {
            return None;
        };
        Some(Line::styled(text, style))
    }

    fn footer_commands(&self) -> Vec<FooterCommand> {
        let mut commands = match (self.websearch_shown(), self.pstack_shown()) {
            (true, _) if !self.local => Vec::new(),
            (true, _) => vec![
                FooterCommand::Toggle,
                FooterCommand::MoveUp,
                FooterCommand::MoveDown,
                FooterCommand::Mode,
                FooterCommand::Edit,
                FooterCommand::Reset,
            ],
            (_, true) => vec![
                FooterCommand::Edit,
                FooterCommand::Thinking,
                FooterCommand::Auto,
                FooterCommand::Remove,
            ],
            _ => vec![FooterCommand::Edit, FooterCommand::Reset],
        };
        commands.extend([
            FooterCommand::Filter,
            FooterCommand::Docs,
            FooterCommand::Section,
            FooterCommand::Close,
        ]);
        commands
    }

    fn render_footer(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let commands = self.footer_commands();
        if commands != self.footer {
            self.footer_hits.clear();
        }
        let footer = FOOTER_GAPS
            .iter()
            .map(|&(glossed, gap)| footer_line(&commands, glossed, gap, theme))
            .find(|footer| footer.fits(area.width))
            .unwrap_or_default();
        self.footer_hits.set(footer.hits(area, 0, 1));
        frame.render_widget(
            Paragraph::new(footer.line(self.footer_hits.hovered())),
            area,
        );
        self.footer = commands;
    }
}

impl Default for SettingsModal {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for SettingsModal {
    /// The selected key's section of the reference, else the tab's first table's.
    fn docs_topic(&self) -> Option<DocsTopic> {
        if self.websearch_shown() {
            return Some(DocsTopic::WebSearch);
        }
        if self.pstack_shown() {
            return Some(DocsTopic::ConfigSkills);
        }
        self.selected_key()
            .map(|(_, docs)| docs)
            .or_else(|| self.tab.groups().first().map(|group| group.docs))
    }

    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        *self = Self::new();
    }
}

fn websearch_listed(data: &SettingsData) -> Vec<WebsearchBackendName> {
    data.files
        .websearch
        .written()
        .backends
        .clone()
        .unwrap_or_else(|| data.default_backends.clone())
}

fn footer_line(
    commands: &[FooterCommand],
    glossed: bool,
    gap: &'static str,
    theme: &Theme,
) -> FooterLine {
    let mut footer = FooterLine::default();
    for (index, command) in commands.iter().enumerate() {
        if index > 0 {
            footer.text(gap, Style::default());
        }
        let (label, description) = command.hint();
        footer.command(label, theme.keybind_key);
        if glossed {
            footer.describe(format!(" {description}"), theme.tool_dim);
        }
    }
    footer
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use caudra_agent::pstack::roles::{
        AutoReason, DEFAULT_EFFORT, Kind, ResolvedSeat, RoleRow, RoleTable, Seat, Source,
        StoredRoles,
    };
    use caudra_config::config_file::CONFIG_VERSION;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use fields::editable_fields;

    const CONFIG_NAME: &str = "caudra.toml";
    const PROJECT_CONFIG_NAME: &str = "project.toml";
    const SCREEN_WIDTH: u16 = 120;
    const SCREEN_HEIGHT: u16 = 40;
    const CANARY: &str = "canary-9f3e1c7a";
    const ENV_REFERENCE: &str = "{env:BRAVE_API_KEY}";
    const API_KEY: &str = "api_key";
    const SEARXNG_ONLY: &str = "[websearch]\nbackends = [\"searxng\"]\n";
    const SEARXNG_THEN_EXA: &str = "[websearch]\nbackends = [\"searxng\", \"exa-mcp\"]\n";
    const BRAVE_TABLE: &str = "\n[websearch.brave]\n";
    const BRAVE_ONLY: &str = "[websearch]\nbackends = [\"brave\"]\n";

    const SMALL_WIDTH: u16 = 40;
    /// Room for every tab's name in the strip.
    const WIDE_WIDTH: u16 = 160;
    const CACHE_WARMING: &str = "cache_warming";
    const RESUME_INTERRUPTED: &str = "resume_interrupted";
    const RESUME_CHOICES: &[&str] = &["auto", "ask", "off"];
    const ASK: &str = "ask";
    const DISABLED_TOOLS: &str = "disabled_tools";
    const SHOW_DECISIONS: &str = "show_decisions";
    const UPDATE_CHECK: &str = "update_check";
    const TOOL_NAME: &str = "shell";
    const FILTER_QUERY: &str = "CACHE_WARM";
    const PSTACK: &str = "pstack";
    const TENTH_TAB_KEY: char = '0';
    const PSTACK_TAB: &str = "pstack";
    const OPUS_SPEC: &str = "anthropic/claude-opus-5-5";
    const GROK_SPEC: &str = "xai/grok-4.7";
    const OPUS_LEVELS: &[&str] = &["low", "high", "max"];
    const LOWEST_LEVEL: &str = "low";
    const MAX_EFFORT: &str = "max";
    const EFFORT_ROW: usize = 0;
    const CODE_ROW: usize = 1;
    const HARDEST_ROW: usize = 2;
    const SECOND_PANEL_SEAT_ROW: usize = 4;
    const ADD_PANEL_SEAT_ROW: usize = 5;
    const NOT_WRITABLE: &str = "pstack's roles were not saved: disk full";

    struct Fixture {
        _dir: TempDir,
        path: PathBuf,
        modal: SettingsModal,
    }

    impl Fixture {
        /// Answers the request in flight with the file as it is now.
        fn reload(&mut self) {
            self.modal
                .receive(reply(&self.path, ProjectSettings::default()));
        }
    }

    fn reply(path: &Path, project: ProjectSettings) -> SettingsReply {
        let files = SettingsFiles {
            settings: GlobalSettingsFile::load(path).expect("settings file"),
            websearch: GlobalWebsearchFile::load(path).expect("websearch file"),
        };
        SettingsReply {
            data: Ok(Box::new(SettingsData {
                files,
                project,
                statuses: Vec::new(),
                default_backends: vec![WebsearchBackendName::ExaMcp],
                default_mode: WebsearchMode::Fallback,
            })),
            error: None,
            note: None,
        }
    }

    fn opened(contents: &str, local: bool) -> Fixture {
        opened_in(contents, None, local)
    }

    fn opened_in(contents: &str, project: Option<&str>, local: bool) -> Fixture {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join(CONFIG_NAME);
        fs::write(&path, format!("version = {CONFIG_VERSION}\n{contents}")).expect("write config");
        let project = project.map_or_else(ProjectSettings::default, |project| {
            let path = dir.path().join(PROJECT_CONFIG_NAME);
            fs::write(&path, format!("version = {CONFIG_VERSION}\n{project}"))
                .expect("write project config");
            ProjectSettings::load(&path).expect("project config")
        });
        let mut modal = SettingsModal::new();
        modal.open(local);
        modal.receive(reply(&path, project));
        Fixture {
            _dir: dir,
            path,
            modal,
        }
    }

    /// Selects `table.name` on `tab`, returning its row.
    fn select_key(
        modal: &mut SettingsModal,
        tab: SettingsTab,
        table: &[&str],
        name: &str,
    ) -> usize {
        modal.set_tab(tab);
        let index = modal
            .plain_rows()
            .iter()
            .position(|row| {
                matches!(row, PlainRow::Key { table: shown, field, .. }
                    if *shown == table && field.name == name)
            })
            .expect("the key on its tab");
        modal.select(index);
        index
    }

    fn agent_field(name: &str) -> &'static ConfigField {
        AgentConfig::FIELDS
            .iter()
            .find(|field| field.name == name)
            .expect("an [agent] key")
    }

    fn press(modal: &mut SettingsModal, code: KeyCode) -> SettingsAction {
        modal.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(modal: &mut SettingsModal, text: &str) {
        modal.handle_text(text, false);
    }

    fn click(modal: &mut SettingsModal, at: Position) -> SettingsAction {
        let event = |kind| MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        };
        let pressed = modal.handle_mouse(event(MouseEventKind::Down(MouseButton::Left)));
        let released = modal.handle_mouse(event(MouseEventKind::Up(MouseButton::Left)));
        match released {
            SettingsAction::Consumed => pressed,
            released => released,
        }
    }

    fn screen(modal: &mut SettingsModal) -> String {
        screen_of(modal, SCREEN_WIDTH)
    }

    fn screen_of(modal: &mut SettingsModal, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, SCREEN_HEIGHT)).expect("terminal");
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn requested_edit(action: SettingsAction) -> SettingsEdit {
        match action {
            SettingsAction::Request(SettingsRequest::Edit { edit, .. }) => *edit,
            _ => panic!("expected an edit"),
        }
    }

    fn websearch_edits(action: SettingsAction) -> Vec<WebsearchEdit> {
        match requested_edit(action) {
            SettingsEdit::Websearch(edits) => edits,
            SettingsEdit::Key { .. } => panic!("expected a web search edit"),
        }
    }

    fn key_edit(action: SettingsAction) -> (&'static [&'static str], &'static str, Option<Value>) {
        match requested_edit(action) {
            SettingsEdit::Key { table, key, value } => (table, key, value),
            SettingsEdit::Websearch(_) => panic!("expected a key edit"),
        }
    }

    #[test_case("", 0, &[]; "unchecks_the_default")]
    #[test_case("", 1, &[WebsearchBackendName::ExaMcp, WebsearchBackendName::Searxng]; "checks_an_unlisted_backend")]
    #[test_case(SEARXNG_THEN_EXA, 1, &[WebsearchBackendName::Searxng]; "unchecks_a_listed_backend")]
    fn space_toggles_the_selected_backend(
        contents: &str,
        downs: usize,
        expected: &[WebsearchBackendName],
    ) {
        let mut fixture = opened(contents, true);
        for _ in 0..downs {
            press(&mut fixture.modal, KeyCode::Down);
        }
        let edits = websearch_edits(press(&mut fixture.modal, KeyCode::Char(TOGGLE_KEY)));
        assert_eq!(edits, [WebsearchEdit::Backends(Some(expected.to_vec()))]);
    }

    #[test]
    fn move_down_swaps_the_backend_with_the_next() {
        let mut fixture = opened(SEARXNG_THEN_EXA, true);
        let edits = websearch_edits(press(&mut fixture.modal, KeyCode::Char(MOVE_DOWN_KEY)));
        assert_eq!(
            edits,
            [WebsearchEdit::Backends(Some(vec![
                WebsearchBackendName::ExaMcp,
                WebsearchBackendName::Searxng,
            ]))]
        );
    }

    #[test]
    fn the_first_backend_cannot_move_up() {
        let mut fixture = opened(SEARXNG_THEN_EXA, true);
        let action = press(&mut fixture.modal, KeyCode::Char(MOVE_UP_KEY));
        assert!(matches!(action, SettingsAction::Consumed));
    }

    #[test]
    fn an_unlisted_backend_cannot_move() {
        let mut fixture = opened(SEARXNG_ONLY, true);
        press(&mut fixture.modal, KeyCode::Down);
        let action = press(&mut fixture.modal, KeyCode::Char(MOVE_DOWN_KEY));
        assert!(matches!(action, SettingsAction::Consumed));
        assert_eq!(fixture.modal.message.as_deref(), Some(NOT_LISTED));
    }

    #[test]
    fn the_mode_key_moves_to_the_next_mode() {
        let mut fixture = opened("", true);
        let edits = websearch_edits(press(&mut fixture.modal, KeyCode::Char(MODE_KEY)));
        assert_eq!(edits, [WebsearchEdit::Mode(Some(WebsearchMode::All))]);
    }

    #[test]
    fn reset_unsets_an_explicit_list() {
        let mut fixture = opened(SEARXNG_ONLY, true);
        let edits = websearch_edits(press(&mut fixture.modal, KeyCode::Char(RESET_KEY)));
        assert_eq!(edits, [WebsearchEdit::Backends(None)]);
    }

    #[test]
    fn reset_without_a_list_writes_nothing() {
        let mut fixture = opened("", true);
        let action = press(&mut fixture.modal, KeyCode::Char(RESET_KEY));
        assert!(matches!(action, SettingsAction::Consumed));
        assert_eq!(fixture.modal.message.as_deref(), Some(ALREADY_DEFAULT));
    }

    #[test]
    fn a_second_edit_waits_for_the_first() {
        let mut fixture = opened("", true);
        websearch_edits(press(&mut fixture.modal, KeyCode::Char(MODE_KEY)));
        let action = press(&mut fixture.modal, KeyCode::Char(TOGGLE_KEY));
        assert!(matches!(action, SettingsAction::Consumed));
        assert_eq!(fixture.modal.message.as_deref(), Some(BUSY));
    }

    #[test]
    fn escape_closes_the_form_before_the_modal() {
        let mut fixture = opened(SEARXNG_ONLY, true);
        press(&mut fixture.modal, KeyCode::Enter);
        assert!(fixture.modal.text_input_active());
        let first = press(&mut fixture.modal, KeyCode::Esc);
        assert!(matches!(first, SettingsAction::Consumed));
        assert!(!fixture.modal.text_input_active());
        let second = press(&mut fixture.modal, KeyCode::Esc);
        assert!(matches!(second, SettingsAction::Close));
    }

    #[test]
    fn a_remote_workcell_leaves_web_search_read_only() {
        let mut fixture = opened("", false);
        for key in [TOGGLE_KEY, MODE_KEY, MOVE_DOWN_KEY] {
            let action = press(&mut fixture.modal, KeyCode::Char(key));
            assert!(matches!(action, SettingsAction::Consumed));
        }
        press(&mut fixture.modal, KeyCode::Enter);
        assert!(!fixture.modal.text_input_active());
        assert!(screen(&mut fixture.modal).contains(REMOTE));
    }

    #[test]
    fn a_remote_workcell_still_edits_plain_settings() {
        let mut fixture = opened("", false);
        press(&mut fixture.modal, KeyCode::Tab);
        let bool_index = bool_field_index();
        for _ in 0..bool_index {
            press(&mut fixture.modal, KeyCode::Down);
        }
        key_edit(press(&mut fixture.modal, KeyCode::Char(TOGGLE_KEY)));
    }

    fn bool_field_index() -> usize {
        editable_fields(TOP_LEVEL_FIELDS)
            .position(|field| FieldKind::of(field) == Some(FieldKind::Bool))
            .expect("a top-level bool key")
    }

    fn top_level_field(index: usize) -> &'static ConfigField {
        editable_fields(TOP_LEVEL_FIELDS)
            .nth(index)
            .expect("a top-level key")
    }

    #[test]
    fn space_flips_a_bool_key_from_its_default() {
        let mut fixture = opened("", true);
        press(&mut fixture.modal, KeyCode::Tab);
        assert_eq!(fixture.modal.tab(), SettingsTab::General);
        let index = bool_field_index();
        for _ in 0..index {
            press(&mut fixture.modal, KeyCode::Down);
        }
        let field = top_level_field(index);
        let (table, key, value) = key_edit(press(&mut fixture.modal, KeyCode::Char(TOGGLE_KEY)));
        assert!(table.is_empty());
        assert_eq!(key, field.name);
        assert_eq!(
            value.and_then(|value| value.as_bool()),
            Some(!current_bool(field, None))
        );
    }

    #[test_case(true; "a_key_the_project_sets")]
    #[test_case(false; "no_project_file")]
    fn a_project_override_is_marked(overridden: bool) {
        let key = top_level_field(0).name;
        let project = format!("{key} = 1\n");
        let mut fixture = opened_in("", overridden.then_some(project.as_str()), true);
        press(&mut fixture.modal, KeyCode::Tab);
        assert_eq!(
            screen(&mut fixture.modal).contains(fields::OVERRIDDEN),
            overridden
        );
    }

    #[test]
    fn reset_removes_a_plain_key() {
        let mut fixture = opened("", true);
        press(&mut fixture.modal, KeyCode::Tab);
        let (_, key, value) = key_edit(press(&mut fixture.modal, KeyCode::Char(RESET_KEY)));
        assert_eq!(key, top_level_field(0).name);
        assert!(value.is_none());
    }

    #[test]
    fn a_plain_secret_asks_before_it_saves() {
        let mut fixture = opened(BRAVE_ONLY, true);
        press(&mut fixture.modal, KeyCode::Enter);
        type_text(&mut fixture.modal, CANARY);
        let warned = press(&mut fixture.modal, KeyCode::Enter);
        assert!(matches!(warned, SettingsAction::Consumed));
        assert!(fixture.modal.text_input_active());
        let edits = websearch_edits(press(&mut fixture.modal, KeyCode::Enter));
        assert_eq!(
            edits,
            [WebsearchEdit::Field {
                backend: WebsearchBackendName::Brave,
                key: API_KEY,
                value: Some(CANARY.to_owned()),
            }]
        );
    }

    #[test]
    fn a_reference_saves_without_asking() {
        let mut fixture = opened(BRAVE_ONLY, true);
        press(&mut fixture.modal, KeyCode::Enter);
        type_text(&mut fixture.modal, ENV_REFERENCE);
        let edits = websearch_edits(press(&mut fixture.modal, KeyCode::Enter));
        assert_eq!(
            edits,
            [WebsearchEdit::Field {
                backend: WebsearchBackendName::Brave,
                key: API_KEY,
                value: Some(ENV_REFERENCE.to_owned()),
            }]
        );
    }

    #[test]
    fn a_written_secret_stays_off_screen_until_revealed() {
        let contents = format!("{BRAVE_ONLY}{BRAVE_TABLE}{API_KEY} = \"{CANARY}\"\n");
        let mut fixture = opened(&contents, true);
        assert!(!screen(&mut fixture.modal).contains(CANARY));
        press(&mut fixture.modal, KeyCode::Enter);
        assert!(fixture.modal.text_input_active());
        assert!(!screen(&mut fixture.modal).contains(CANARY));
        fixture
            .modal
            .handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(screen(&mut fixture.modal).contains(CANARY));
    }

    #[test]
    fn clicking_a_tab_switches_to_it() {
        let mut fixture = opened("", true);
        screen(&mut fixture.modal);
        let (hit, _) = fixture
            .modal
            .tab_hits
            .iter()
            .copied()
            .find(|(_, tab)| *tab == SettingsTab::Storage)
            .expect("storage tab");
        click(&mut fixture.modal, hit.as_position());
        assert_eq!(fixture.modal.tab(), SettingsTab::Storage);
    }

    #[test]
    fn clicking_a_mode_switches_to_it() {
        let mut fixture = opened("", true);
        screen(&mut fixture.modal);
        let (hit, _) = fixture
            .modal
            .mode_hits
            .iter()
            .copied()
            .find(|(_, mode)| *mode == WebsearchMode::Single)
            .expect("single chip");
        let edits = websearch_edits(click(&mut fixture.modal, hit.as_position()));
        assert_eq!(edits, [WebsearchEdit::Mode(Some(WebsearchMode::Single))]);
    }

    #[test]
    fn clicking_the_checkbox_toggles_the_row() {
        let mut fixture = opened(SEARXNG_ONLY, true);
        screen(&mut fixture.modal);
        let list = fixture.modal.list_area;
        let column = list.x
            + u16::try_from(CURSOR_MARK.width()).expect("mark width")
            + fixture.modal.row_columns.check.start;
        let edits = websearch_edits(click(&mut fixture.modal, Position::new(column, list.y + 1)));
        assert_eq!(
            edits,
            [WebsearchEdit::Backends(Some(vec![
                WebsearchBackendName::Searxng,
                WebsearchBackendName::ExaMcp,
            ]))]
        );
    }

    #[test]
    fn clicking_the_selected_row_opens_its_form() {
        let mut fixture = opened(SEARXNG_ONLY, true);
        screen(&mut fixture.modal);
        let list = fixture.modal.list_area;
        let row = Position::new(list.right().saturating_sub(1), list.y);
        click(&mut fixture.modal, row);
        assert!(fixture.modal.text_input_active());
    }

    #[test]
    fn clicking_the_close_hint_closes() {
        let mut fixture = opened("", true);
        screen(&mut fixture.modal);
        let index = fixture
            .modal
            .footer
            .iter()
            .position(|command| *command == FooterCommand::Close)
            .expect("close hint");
        let hit = fixture.modal.footer_hits.hit(index);
        assert!(matches!(
            click(&mut fixture.modal, hit.as_position()),
            SettingsAction::Close
        ));
    }

    #[test]
    fn the_docs_badge_follows_the_tab() {
        let mut fixture = opened("", true);
        assert_eq!(fixture.modal.docs_topic(), Some(DocsTopic::WebSearch));
        press(&mut fixture.modal, KeyCode::Tab);
        assert_eq!(fixture.modal.docs_topic(), Some(DocsTopic::ConfigTopLevel));
    }

    #[test]
    fn space_moves_a_choice_key_to_its_next_choice() {
        let mut fixture = opened("", true);
        select_key(
            &mut fixture.modal,
            SettingsTab::Agent,
            AGENT_TABLE,
            CACHE_WARMING,
        );
        let (table, key, value) = key_edit(press(&mut fixture.modal, KeyCode::Char(TOGGLE_KEY)));
        assert_eq!((table, key), (AGENT_TABLE, CACHE_WARMING));
        assert_eq!(
            value.as_ref().and_then(Value::as_str),
            Some(next_choice(agent_field(CACHE_WARMING), None))
        );
    }

    #[test]
    fn resume_interrupted_is_a_choice_on_the_agent_tab() {
        let mut fixture = opened("", true);
        let index = select_key(
            &mut fixture.modal,
            SettingsTab::Agent,
            AGENT_TABLE,
            RESUME_INTERRUPTED,
        );
        screen(&mut fixture.modal);
        let chips: Vec<&str> = fixture.modal.row_chips[index]
            .iter()
            .map(|(_, choice)| *choice)
            .collect();
        assert_eq!(chips, RESUME_CHOICES);
        let (table, key, value) = key_edit(press(&mut fixture.modal, KeyCode::Char(TOGGLE_KEY)));
        assert_eq!((table, key), (AGENT_TABLE, RESUME_INTERRUPTED));
        assert_eq!(value.as_ref().and_then(Value::as_str), Some(ASK));
    }

    #[test]
    fn clicking_a_chip_picks_its_choice() {
        let mut fixture = opened("", true);
        let index = select_key(
            &mut fixture.modal,
            SettingsTab::Agent,
            AGENT_TABLE,
            CACHE_WARMING,
        );
        screen(&mut fixture.modal);
        let current = current_choice(agent_field(CACHE_WARMING), None);
        let (columns, choice) = fixture.modal.row_chips[index]
            .iter()
            .find(|(_, choice)| Some(*choice) != current)
            .cloned()
            .expect("another chip");
        let list = fixture.modal.list_area;
        let at = Position::new(
            list.x + u16::try_from(CURSOR_MARK.width()).expect("mark width") + columns.start,
            list.y + u16::try_from(index - fixture.modal.offset).expect("visible row"),
        );
        let (_, _, value) = key_edit(click(&mut fixture.modal, at));
        assert_eq!(value.as_ref().and_then(Value::as_str), Some(choice));
    }

    #[test]
    fn the_list_editor_saves_a_typed_item() {
        let mut fixture = opened("", true);
        select_key(
            &mut fixture.modal,
            SettingsTab::Agent,
            AGENT_TABLE,
            DISABLED_TOOLS,
        );
        press(&mut fixture.modal, KeyCode::Enter);
        assert!(fixture.modal.text_input_active());
        type_text(&mut fixture.modal, TOOL_NAME);
        press(&mut fixture.modal, KeyCode::Enter);
        let (_, key, value) = key_edit(press(&mut fixture.modal, KeyCode::Enter));
        assert_eq!(key, DISABLED_TOOLS);
        let items: Vec<&str> = value
            .as_ref()
            .and_then(Value::as_array)
            .expect("a list")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(items, [TOOL_NAME]);
    }

    #[test]
    fn the_filter_finds_a_key_on_another_tab() {
        let mut fixture = opened("", true);
        press(&mut fixture.modal, KeyCode::Char(FILTER_KEY));
        assert!(fixture.modal.text_input_active());
        type_text(&mut fixture.modal, FILTER_QUERY);
        let keys: Vec<&str> = fixture
            .modal
            .plain_rows()
            .iter()
            .filter_map(|row| match row {
                PlainRow::Key { field, .. } => Some(field.name),
                PlainRow::Heading(_) => None,
            })
            .collect();
        assert_eq!(keys, [CACHE_WARMING]);
        press(&mut fixture.modal, KeyCode::Enter);
        assert!(!fixture.modal.text_input_active());
        let (table, key, _) = key_edit(press(&mut fixture.modal, KeyCode::Char(TOGGLE_KEY)));
        assert_eq!((table, key), (AGENT_TABLE, CACHE_WARMING));
    }

    #[test]
    fn zero_opens_the_tenth_tab() {
        let mut fixture = opened("", true);
        press(&mut fixture.modal, KeyCode::Char(TENTH_TAB_KEY));
        assert_eq!(Some(&fixture.modal.tab()), SettingsTab::ALL.get(9));
        let label = format!("{TENTH_TAB_KEY} {}", fixture.modal.tab().label());
        assert!(screen_of(&mut fixture.modal, WIDE_WIDTH).contains(&label));
    }

    #[test]
    fn space_turns_on_a_builtin_skill() {
        let mut fixture = opened("", true);
        select_key(&mut fixture.modal, SettingsTab::Skills, SKILL_TABLE, PSTACK);
        let (table, key, value) = key_edit(press(&mut fixture.modal, KeyCode::Char(TOGGLE_KEY)));
        assert_eq!((table, key), (SKILL_TABLE, PSTACK));
        assert_eq!(
            value.and_then(|value| value.as_bool()),
            Some(!caudra_config::DEFAULT_SKILL_PSTACK)
        );
    }

    /// A terminal without enhanced keys sends typing as key presses, with no text.
    #[test]
    fn the_filter_takes_typed_keys() {
        let mut fixture = opened("", true);
        press(&mut fixture.modal, KeyCode::Char(FILTER_KEY));
        for typed in FILTER_QUERY.chars() {
            press(&mut fixture.modal, KeyCode::Char(typed));
        }
        press(&mut fixture.modal, KeyCode::Backspace);
        let query = fixture
            .modal
            .filter
            .as_ref()
            .map(|filter| filter.query.as_str());
        assert_eq!(query, FILTER_QUERY.get(..FILTER_QUERY.len() - 1));
    }

    #[test]
    fn escape_clears_the_filter_before_closing() {
        let mut fixture = opened("", true);
        press(&mut fixture.modal, KeyCode::Char(FILTER_KEY));
        type_text(&mut fixture.modal, FILTER_QUERY);
        press(&mut fixture.modal, KeyCode::Enter);
        assert!(matches!(
            press(&mut fixture.modal, KeyCode::Esc),
            SettingsAction::Consumed
        ));
        assert!(fixture.modal.filter.is_none());
        assert_eq!(fixture.modal.tab(), SettingsTab::Websearch);
        assert!(matches!(
            press(&mut fixture.modal, KeyCode::Esc),
            SettingsAction::Close
        ));
    }

    #[test]
    fn the_docs_key_opens_the_selected_keys_section() {
        let mut fixture = opened("", true);
        let name = editable_fields(SnapshotsConfig::FIELDS)
            .next()
            .expect("a snapshots key")
            .name;
        select_key(
            &mut fixture.modal,
            SettingsTab::Storage,
            SNAPSHOTS_TABLE,
            name,
        );
        assert!(matches!(
            press(&mut fixture.modal, KeyCode::Char(DOCS_KEY)),
            SettingsAction::OpenDocs(DocsTopic::ConfigSnapshots)
        ));
    }

    #[test]
    fn clicking_the_info_badge_opens_the_docs() {
        let mut fixture = opened("", true);
        press(&mut fixture.modal, KeyCode::Tab);
        screen(&mut fixture.modal);
        let hit = fixture.modal.docs_hit.expect("a docs badge");
        assert!(matches!(
            click(&mut fixture.modal, hit.as_position()),
            SettingsAction::OpenDocs(DocsTopic::ConfigTopLevel)
        ));
    }

    #[test_case(SettingsTab::Ui, UI_TABLE, SHOW_DECISIONS, ApplyTiming::Now; "a_key_read_at_once")]
    #[test_case(SettingsTab::Skills, SKILL_TABLE, PSTACK, ApplyTiming::Now; "a_skill_switch")]
    #[test_case(SettingsTab::Agent, AGENT_TABLE, CACHE_WARMING, ApplyTiming::NextTurn; "an_agent_key")]
    #[test_case(SettingsTab::Ui, UI_TABLE, UPDATE_CHECK, ApplyTiming::Reload; "a_key_read_on_reload")]
    fn a_saved_key_says_when_it_applies(
        tab: SettingsTab,
        table: &[&str],
        name: &str,
        timing: ApplyTiming,
    ) {
        let mut fixture = opened("", true);
        select_key(&mut fixture.modal, tab, table, name);
        key_edit(press(&mut fixture.modal, KeyCode::Char(RESET_KEY)));
        fixture.reload();
        assert_eq!(
            fixture.modal.message,
            Some(format!("{SAVED_APPLIES}{}{SENTENCE_END}", timing.suffix()))
        );
    }

    #[test]
    fn shift_down_scrolls_help_too_long_for_the_pane() {
        let mut fixture = opened("", true);
        select_key(
            &mut fixture.modal,
            SettingsTab::Ui,
            UI_TABLE,
            SHOW_DECISIONS,
        );
        let before = screen_of(&mut fixture.modal, SMALL_WIDTH);
        assert!(before.contains(MORE_BELOW));
        assert!(!before.contains(MORE_ABOVE));
        fixture
            .modal
            .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        assert!(screen_of(&mut fixture.modal, SMALL_WIDTH).contains(MORE_ABOVE));
    }

    /// Every kind on auto: Code inherits, Hardest runs on Opus, the Panel on Opus and Grok.
    fn pstack_roles(stored: StoredRoles) -> PstackRoles {
        let seat = |model: &str| ResolvedSeat {
            model: Some(model.to_owned()),
            thinking: None,
        };
        let row = |kind, seats, source| RoleRow {
            kind,
            seats,
            source,
        };
        let table = RoleTable {
            rows: vec![
                row(
                    Kind::Code,
                    vec![ResolvedSeat::default()],
                    Source::Auto(AutoReason::SubagentJob),
                ),
                row(
                    Kind::Hardest,
                    vec![seat(OPUS_SPEC)],
                    Source::Auto(AutoReason::BestFavorite),
                ),
                row(
                    Kind::Panel,
                    vec![seat(OPUS_SPEC), seat(GROK_SPEC)],
                    Source::Auto(AutoReason::PanelFavorites),
                ),
            ],
            task_model_choice: true,
            effort: DEFAULT_EFFORT.to_owned(),
        };
        PstackRoles::new(table, stored, |model| match model {
            OPUS_SPEC => OPUS_LEVELS
                .iter()
                .map(|level| (*level).to_owned())
                .collect(),
            _ => Vec::new(),
        })
    }

    fn on_pstack(stored: StoredRoles, row: usize) -> Fixture {
        let mut fixture = opened("", true);
        fixture.modal.set_pstack(Some(pstack_roles(stored)));
        fixture.modal.set_tab(SettingsTab::Pstack);
        fixture.modal.select(row);
        fixture
    }

    fn pstack_action(action: SettingsAction) -> PstackAction {
        match action {
            SettingsAction::Pstack(action) => action,
            _ => panic!("expected a pstack action"),
        }
    }

    fn refused_with(fixture: &mut Fixture, key: char, message: &str) {
        let action = press(&mut fixture.modal, KeyCode::Char(key));
        assert!(matches!(action, SettingsAction::Consumed));
        assert_eq!(fixture.modal.message.as_deref(), Some(message));
    }

    #[test_case(true, SettingsTab::Pstack ; "listed_last_while_pstack_is_on")]
    #[test_case(false, SettingsTab::Automations ; "hidden_while_pstack_is_off")]
    fn the_pstack_tab_follows_the_switch(on: bool, last: SettingsTab) {
        let mut fixture = opened("", true);
        fixture
            .modal
            .set_pstack(on.then(|| pstack_roles(StoredRoles::default())));
        assert_eq!(
            screen_of(&mut fixture.modal, WIDE_WIDTH).contains(PSTACK_TAB),
            on
        );
        press(&mut fixture.modal, KeyCode::BackTab);
        assert_eq!(fixture.modal.tab(), last);
    }

    #[test]
    fn turning_pstack_off_leaves_its_tab() {
        let mut fixture = on_pstack(StoredRoles::default(), EFFORT_ROW);
        fixture.modal.set_pstack(None);
        assert_eq!(fixture.modal.tab(), SettingsTab::Websearch);
    }

    #[test_case(HARDEST_ROW, Kind::Hardest, Some(0), Some(OPUS_SPEC) ; "a_kind_row_picks_its_seat")]
    #[test_case(SECOND_PANEL_SEAT_ROW, Kind::Panel, Some(1), Some(GROK_SPEC) ; "a_list_seat_picks_itself")]
    #[test_case(ADD_PANEL_SEAT_ROW, Kind::Panel, None, None ; "the_add_row_picks_a_new_seat")]
    fn enter_asks_for_the_model_picker(
        row: usize,
        kind: Kind,
        seat: Option<usize>,
        model: Option<&str>,
    ) {
        let mut fixture = on_pstack(StoredRoles::default(), row);
        assert_eq!(
            pstack_action(press(&mut fixture.modal, KeyCode::Enter)),
            PstackAction::Pick {
                kind,
                seat,
                model: model.map(str::to_owned),
            }
        );
    }

    #[test_case(KeyCode::Enter ; "enter")]
    #[test_case(KeyCode::Right ; "right")]
    fn effort_steps_up_from_the_default(code: KeyCode) {
        let mut fixture = on_pstack(StoredRoles::default(), EFFORT_ROW);
        assert_eq!(
            pstack_action(press(&mut fixture.modal, code)),
            PstackAction::Edit(RoleEdit::Effort(Some(MAX_EFFORT.to_owned())))
        );
    }

    #[test]
    fn auto_returns_a_stored_effort_to_the_default() {
        let stored = StoredRoles {
            effort: Some(MAX_EFFORT.to_owned()),
            ..StoredRoles::default()
        };
        let mut fixture = on_pstack(stored, EFFORT_ROW);
        assert_eq!(
            pstack_action(press(&mut fixture.modal, KeyCode::Char(AUTO_KEY))),
            PstackAction::Edit(RoleEdit::Effort(None))
        );
        let mut fixture = on_pstack(StoredRoles::default(), EFFORT_ROW);
        refused_with(&mut fixture, AUTO_KEY, EFFORT_ALREADY_DEFAULT);
    }

    #[test]
    fn thinking_steps_to_the_models_lowest_level() {
        let mut fixture = on_pstack(StoredRoles::default(), HARDEST_ROW);
        assert_eq!(
            pstack_action(press(&mut fixture.modal, KeyCode::Char(THINKING_KEY))),
            PstackAction::Edit(RoleEdit::Thinking {
                kind: Kind::Hardest,
                seat: 0,
                thinking: Some(LOWEST_LEVEL.to_owned()),
            })
        );
    }

    #[test_case(CODE_ROW, INHERIT_THINKING ; "an_inherit_seat")]
    #[test_case(SECOND_PANEL_SEAT_ROW, NO_LEVELS ; "a_model_without_levels")]
    fn thinking_needs_a_model_with_levels(row: usize, message: &str) {
        let mut fixture = on_pstack(StoredRoles::default(), row);
        refused_with(&mut fixture, THINKING_KEY, message);
    }

    #[test]
    fn auto_returns_a_stored_kind_to_auto() {
        let stored = StoredRoles {
            kinds: BTreeMap::from([(Kind::Hardest, vec![Seat::Inherit])]),
            ..StoredRoles::default()
        };
        let mut fixture = on_pstack(stored, HARDEST_ROW);
        assert_eq!(
            pstack_action(press(&mut fixture.modal, KeyCode::Char(AUTO_KEY))),
            PstackAction::Edit(RoleEdit::Reset(Kind::Hardest))
        );
        let mut fixture = on_pstack(StoredRoles::default(), HARDEST_ROW);
        refused_with(&mut fixture, AUTO_KEY, ALREADY_AUTO);
    }

    #[test]
    fn remove_takes_only_a_list_seat() {
        let mut fixture = on_pstack(StoredRoles::default(), SECOND_PANEL_SEAT_ROW);
        assert_eq!(
            pstack_action(press(&mut fixture.modal, KeyCode::Char(RESET_KEY))),
            PstackAction::Edit(RoleEdit::Remove {
                kind: Kind::Panel,
                seat: 1,
            })
        );
        let mut fixture = on_pstack(StoredRoles::default(), HARDEST_ROW);
        refused_with(&mut fixture, RESET_KEY, NOT_A_SEAT);
    }

    #[test_case(None, Some(PSTACK_SAVED), None ; "saved")]
    #[test_case(Some(NOT_WRITABLE), None, Some(NOT_WRITABLE) ; "refused")]
    fn a_pstack_edit_reports_how_it_went(
        error: Option<&str>,
        message: Option<&str>,
        shown_error: Option<&str>,
    ) {
        let mut fixture = on_pstack(StoredRoles::default(), HARDEST_ROW);
        fixture.modal.pstack_saved(
            Some(pstack_roles(StoredRoles::default())),
            error.map(str::to_owned),
        );
        assert_eq!(fixture.modal.message.as_deref(), message);
        assert_eq!(fixture.modal.error.as_deref(), shown_error);
        assert_eq!(fixture.modal.cursor(), HARDEST_ROW);
    }
}
