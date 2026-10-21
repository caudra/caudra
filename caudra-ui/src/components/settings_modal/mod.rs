//! `/settings`: the `[websearch]` backends with whether each is ready, and a tab per plain
//! table of the global `caudra.toml`. Edits go to a background thread that writes the file and
//! keeps its comments; the config watcher puts them in force.

mod fields;
mod form;
mod websearch;

use std::mem;

use caudra_config::interpolation::holds_reference;
use caudra_config::settings_file::{GlobalSettingsFile, ProjectSettings};
use caudra_config::websearch::{
    GlobalWebsearchFile, WebsearchBackendName, WebsearchConfig, WebsearchEdit, WebsearchMode,
    is_secret_key,
};
use caudra_config::{
    AgentConfig, ConfigField, StorageConfig, TOP_LEVEL_FIELDS, UiConfig, WorktreesConfig,
};
use caudra_grab::grab_scope;
use caudra_workcell::BackendStatus;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
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
use crate::components::section_tabs::{SectionTab, tab_strip};
use crate::components::{Overlay, ascii_key, hover_style, plain_char};
use crate::theme::{self, Theme};
use fields::{ScalarKind, current_bool, field_info, field_row, name_cols, scalar_fields};
use form::{FormField, FormKey, SettingsForm};
use websearch::{
    BackendRow, RowColumns, backend_info, backend_line, backend_rows, literal_warning, mode_line,
};
pub(crate) use websearch::{environment_backends, environment_mode};

pub(crate) const MOVE_UP_LABEL: &str = "[";
pub(crate) const MOVE_DOWN_LABEL: &str = "]";
pub(crate) const MODE_LABEL: &str = "m";
pub(crate) const RESET_LABEL: &str = "d";
const TOGGLE_KEY: char = ' ';
const MOVE_UP_KEY: char = ascii_key(MOVE_UP_LABEL);
const MOVE_DOWN_KEY: char = ascii_key(MOVE_DOWN_LABEL);
const MODE_KEY: char = ascii_key(MODE_LABEL);
const RESET_KEY: char = ascii_key(RESET_LABEL);
const TITLE: &str = " Settings ";
const WIDTH_PERCENT: u16 = 80;
const MAX_HEIGHT_PERCENT: u16 = 85;
const H_PAD: u16 = 1;
const INFO_MAX_ROWS: u16 = 9;
const CURSOR_MARK: &str = "\u{203a} ";
const NO_MARK: &str = "  ";
const UI_TABLE: &[&str] = &["ui"];
const AGENT_TABLE: &[&str] = &["agent"];
const STORAGE_TABLE: &[&str] = &["storage"];
const WORKTREES_TABLE: &[&str] = &["worktrees"];
const ROOT_LEAD: &str = "Top-level keys of the global caudra.toml";
const TABLE_LEAD_SUFFIX: &str = "] of the global caudra.toml";
const TABLE_LEAD_PREFIX: &str = "[";
const TABLE_SEPARATOR: &str = ".";
const LOADING: &str = "Loading…";
const SAVING: &str = "Saving…";
const SAVED: &str = "Saved to the global caudra.toml.";
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    Websearch,
    General,
    Ui,
    Agent,
    Storage,
    Worktrees,
}

impl SectionTab for SettingsTab {
    const ALL: &'static [Self] = &[
        Self::Websearch,
        Self::General,
        Self::Ui,
        Self::Agent,
        Self::Storage,
        Self::Worktrees,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Websearch => "Web search",
            Self::General => "General",
            Self::Ui => "UI",
            Self::Agent => "Agent",
            Self::Storage => "Storage",
            Self::Worktrees => "Worktrees",
        }
    }
}

impl SettingsTab {
    /// The table a plain tab edits and its keys, `None` for Web search.
    fn table(self) -> Option<(&'static [&'static str], &'static [ConfigField])> {
        match self {
            Self::Websearch => None,
            Self::General => Some((&[], TOP_LEVEL_FIELDS)),
            Self::Ui => Some((UI_TABLE, UiConfig::FIELDS)),
            Self::Agent => Some((AGENT_TABLE, AgentConfig::FIELDS)),
            Self::Storage => Some((STORAGE_TABLE, StorageConfig::FIELDS)),
            Self::Worktrees => Some((WORKTREES_TABLE, WorktreesConfig::FIELDS)),
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
            Self::Section => (key::TAB.label, "Section"),
            Self::Close => (ESC_LABEL, "Close"),
        }
    }
}

/// What an open form saves to.
enum FormTarget {
    Backend(WebsearchBackendName),
    Key {
        table: &'static [&'static str],
        field: &'static ConfigField,
    },
}

pub(crate) struct SettingsModal {
    open: bool,
    tab: SettingsTab,
    /// Whether this session's web tools run in the local Workcell, which these settings drive.
    local: bool,
    data: Option<SettingsData>,
    /// The backend the Web search tab has selected, followed across a reorder.
    backend: WebsearchBackendName,
    /// The key a plain tab has selected, by its place among the tab's scalar keys.
    field: usize,
    form: Option<(SettingsForm, FormTarget)>,
    work: Option<Work>,
    message: Option<String>,
    error: Option<String>,
    offset: usize,
    /// The selection moved, so the next draw scrolls it into view.
    reveal_cursor: bool,
    pointer: Option<Position>,
    popup: Rect,
    list_area: Rect,
    row_columns: RowColumns,
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
            form: None,
            work: None,
            message: None,
            error: None,
            offset: 0,
            reveal_cursor: false,
            pointer: None,
            popup: Rect::default(),
            list_area: Rect::default(),
            row_columns: RowColumns::default(),
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
        self.message = reply
            .note
            .or_else(|| (wrote && self.error.is_none()).then(|| SAVED.to_owned()));
    }

    pub(crate) fn text_input_active(&self) -> bool {
        self.form.is_some()
    }

    pub(crate) fn handle_text(&mut self, text: &str, paste: bool) {
        if let Some((form, _)) = self.form.as_mut() {
            form.handle_text(text, paste);
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
                    .is_some_and(|(form, _)| form.contains(position)))
    }

    /// The wheel moves the list, leaving the selection where it is.
    pub(crate) fn scroll(&mut self, delta: i32) {
        if self.form.is_none() {
            let delta = isize::try_from(delta).unwrap_or_default();
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
        self.message = None;
        if key.code == KeyCode::Esc || key::QUIT.matches(key) {
            return SettingsAction::Close;
        }
        match key.code {
            KeyCode::Tab => self.set_tab(self.tab.step(1)),
            KeyCode::BackTab => self.set_tab(self.tab.step(-1)),
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::PageUp => self.step(-self.page()),
            KeyCode::PageDown => self.step(self.page()),
            KeyCode::Home => self.step(isize::MIN),
            KeyCode::End => self.step(isize::MAX),
            KeyCode::Enter => return self.command(FooterCommand::Edit),
            _ => {
                return match plain_char(&key) {
                    Some(TOGGLE_KEY) => self.command(FooterCommand::Toggle),
                    Some(MOVE_UP_KEY) => self.command(FooterCommand::MoveUp),
                    Some(MOVE_DOWN_KEY) => self.command(FooterCommand::MoveDown),
                    Some(MODE_KEY) => self.command(FooterCommand::Mode),
                    Some(RESET_KEY) => self.command(FooterCommand::Reset),
                    Some(digit) => {
                        if let Some(tab) = SettingsTab::from_digit(digit) {
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

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> SettingsAction {
        let position = Position::new(event.column, event.row);
        self.pointer = Some(position);
        if self.form.is_some() {
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
        let info_rows = u16::try_from(info.line_count(padded.width.max(1)))
            .unwrap_or(u16::MAX)
            .min(INFO_MAX_ROWS);
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
        let (strip, hits) = tab_strip(self.tab, self.pointer, tabs);
        self.tab_hits = hits;
        frame.render_widget(Paragraph::new(strip), tabs);
        self.render_lead(frame, lead, &theme);
        self.render_list(frame, list, &theme);
        frame.render_widget(info, info_area);
        if let Some(line) = self.status_line(&theme) {
            frame.render_widget(Paragraph::new(line), status);
        }
        self.render_footer(frame, footer, &theme);
        if let Some((form, _)) = self.form.as_mut() {
            form.view(frame, area);
        }
        popup
    }

    #[cfg(test)]
    pub(crate) fn tab(&self) -> SettingsTab {
        self.tab
    }

    fn set_tab(&mut self, tab: SettingsTab) {
        if tab == self.tab {
            return;
        }
        self.tab = tab;
        self.field = 0;
        self.offset = 0;
        self.mode_hits.clear();
        self.footer_hits.clear();
    }

    fn page(&self) -> isize {
        isize::try_from(self.list_area.height.max(1)).unwrap_or(isize::MAX)
    }

    fn cursor(&self) -> usize {
        match self.tab {
            SettingsTab::Websearch => self
                .backend_order()
                .iter()
                .position(|backend| *backend == self.backend)
                .unwrap_or_default(),
            _ => self.field,
        }
    }

    fn row_count(&self) -> usize {
        match self.tab.table() {
            None => WebsearchBackendName::ALL.len(),
            Some((_, fields)) => scalar_fields(fields).len(),
        }
    }

    fn step(&mut self, delta: isize) {
        let count = self.row_count();
        if count == 0 {
            return;
        }
        self.select(self.cursor().saturating_add_signed(delta).min(count - 1));
    }

    fn select(&mut self, index: usize) {
        self.reveal_cursor = true;
        match self.tab {
            SettingsTab::Websearch => {
                if let Some(backend) = self.backend_order().get(index) {
                    self.backend = *backend;
                }
            }
            _ => self.field = index.min(self.row_count().saturating_sub(1)),
        }
    }

    fn click_row(&mut self, position: Position) -> SettingsAction {
        let index = self.offset + usize::from(position.y - self.list_area.y);
        if index >= self.row_count() {
            return SettingsAction::Consumed;
        }
        let selected = index == self.cursor();
        self.select(index);
        if self.tab == SettingsTab::Websearch {
            let column = (position.x - self.list_area.x)
                .saturating_sub(u16::try_from(CURSOR_MARK.width()).unwrap_or(u16::MAX));
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
        }
        match selected {
            true => self.command(FooterCommand::Edit),
            false => SettingsAction::Consumed,
        }
    }

    fn command(&mut self, command: FooterCommand) -> SettingsAction {
        match (command, self.tab) {
            (FooterCommand::Section, _) => {
                self.set_tab(self.tab.step(1));
                SettingsAction::Consumed
            }
            (FooterCommand::Close, _) => SettingsAction::Close,
            _ if self.refused() => SettingsAction::Consumed,
            (FooterCommand::Toggle, SettingsTab::Websearch) => self.toggle_backend(),
            (FooterCommand::Edit, SettingsTab::Websearch) => self.open_backend_form(),
            (FooterCommand::MoveUp, SettingsTab::Websearch) => self.move_backend(false),
            (FooterCommand::MoveDown, SettingsTab::Websearch) => self.move_backend(true),
            (FooterCommand::Mode, SettingsTab::Websearch) => {
                let modes = WebsearchMode::ALL;
                let current = modes
                    .iter()
                    .position(|mode| *mode == self.effective_mode())
                    .unwrap_or_default();
                self.pick_mode(modes[(current + 1) % modes.len()])
            }
            (FooterCommand::Reset, SettingsTab::Websearch) => self.reset_backends(),
            (FooterCommand::Toggle | FooterCommand::Edit, _) => self.change_field(),
            (FooterCommand::Reset, _) => self.reset_field(),
            (FooterCommand::MoveUp | FooterCommand::MoveDown | FooterCommand::Mode, _) => {
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
        } else if self.tab == SettingsTab::Websearch && !self.local {
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
        self.work = Some(Work::Write);
        self.error = None;
        self.message = None;
        SettingsAction::Request(SettingsRequest::Edit {
            files,
            edit: Box::new(edit),
        })
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

    /// The selected key of a plain tab, the table it sits in, and what the file holds for it.
    fn selected_field(&self) -> Option<(&'static [&'static str], &'static ConfigField)> {
        let (table, fields) = self.tab.table()?;
        scalar_fields(fields)
            .get(self.field)
            .map(|field| (table, *field))
    }

    fn change_field(&mut self) -> SettingsAction {
        let Some((table, field)) = self.selected_field() else {
            return SettingsAction::Consumed;
        };
        let Some(data) = &self.data else {
            return SettingsAction::Consumed;
        };
        let written = data.files.settings.get(table, field.name);
        if ScalarKind::of(field) == Some(ScalarKind::Bool) {
            let value = Value::from(!current_bool(field, written));
            return self.commit(SettingsEdit::Key {
                table,
                key: field.name,
                value: Some(value),
            });
        }
        let initial = written.map(fields::written_text).unwrap_or_default();
        let title = format!("{FORM_TITLE_PREFIX}{}{FORM_TITLE_SUFFIX}", field.name);
        let input = FormField::new(field.name, field.env, field.description, false, &initial);
        self.form = Some((
            SettingsForm::new(title, vec![input], FIELD_HINT),
            FormTarget::Key { table, field },
        ));
        SettingsAction::Consumed
    }

    fn reset_field(&mut self) -> SettingsAction {
        let Some((table, field)) = self.selected_field() else {
            return SettingsAction::Consumed;
        };
        self.commit(SettingsEdit::Key {
            table,
            key: field.name,
            value: None,
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
            &FormTarget::Key { table, field } => {
                let text = changes[0].1.clone().unwrap_or_default();
                match ScalarKind::of(field).map(|kind| kind.parse(field, &text)) {
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
        let line = match self.tab.table() {
            None => {
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
            }
            Some(([], _)) => Line::styled(ROOT_LEAD, theme.tool_dim),
            Some((table, _)) => Line::styled(
                format!(
                    "{TABLE_LEAD_PREFIX}{}{TABLE_LEAD_SUFFIX}",
                    table.join(TABLE_SEPARATOR)
                ),
                theme.tool_dim,
            ),
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn list_rows(&mut self, theme: &Theme) -> Vec<Vec<Span<'static>>> {
        let Some(data) = &self.data else {
            return Vec::new();
        };
        match self.tab.table() {
            None => {
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
                lines
            }
            Some((table, fields)) => {
                let fields = scalar_fields(fields);
                let width = name_cols(&fields);
                fields
                    .iter()
                    .map(|field| {
                        field_row(
                            field,
                            data.files.settings.get(table, field.name),
                            data.project.sets(table, field.name),
                            width,
                            theme,
                        )
                    })
                    .collect()
            }
        }
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.list_area = area;
        let rows = self.list_rows(theme);
        if rows.is_empty() {
            if self.work == Some(Work::Load) {
                frame.render_widget(Paragraph::new(Line::styled(LOADING, theme.tool_dim)), area);
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
                let style = match selected {
                    true => theme.item_selected,
                    false => Style::default(),
                };
                Line::from(ellipsize_spans(spans, width))
                    .style(hover_style(style, hovered == Some(index)))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn info_lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        let Some(data) = &self.data else {
            return Vec::new();
        };
        match self.tab.table() {
            None => {
                let listed = websearch_listed(data);
                let rows = backend_rows(&listed, &data.statuses);
                rows.iter()
                    .find(|row| row.backend == self.backend)
                    .map(|row: &BackendRow| {
                        let written = data.files.websearch.written();
                        backend_info(row, written, written.backends.is_none(), theme)
                    })
                    .unwrap_or_default()
            }
            Some((table, _)) => self
                .selected_field()
                .map(|(_, field)| {
                    field_info(
                        field,
                        table,
                        data.files.settings.get(table, field.name),
                        data.project.sets(table, field.name),
                        theme,
                    )
                })
                .unwrap_or_default(),
        }
    }

    fn status_line(&self, theme: &Theme) -> Option<Line<'static>> {
        let (text, style) = if self.work == Some(Work::Write) {
            (SAVING.to_owned(), theme.tool_dim)
        } else if let Some(error) = &self.error {
            (error.clone(), theme.error)
        } else if let Some(message) = &self.message {
            (message.clone(), theme.tool_dim)
        } else if self.tab == SettingsTab::Websearch && !self.local {
            (REMOTE.to_owned(), theme.tool_warning)
        } else {
            return None;
        };
        Some(Line::styled(text, style))
    }

    fn footer_commands(&self) -> Vec<FooterCommand> {
        let mut commands = match self.tab {
            SettingsTab::Websearch if !self.local => Vec::new(),
            SettingsTab::Websearch => vec![
                FooterCommand::Toggle,
                FooterCommand::MoveUp,
                FooterCommand::MoveDown,
                FooterCommand::Mode,
                FooterCommand::Edit,
                FooterCommand::Reset,
            ],
            _ => vec![FooterCommand::Edit, FooterCommand::Reset],
        };
        commands.extend([FooterCommand::Section, FooterCommand::Close]);
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
    fn docs_topic(&self) -> Option<DocsTopic> {
        Some(match self.tab {
            SettingsTab::Websearch => DocsTopic::WebSearch,
            _ => DocsTopic::Settings,
        })
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
    use std::fs;

    use caudra_config::config_file::CONFIG_VERSION;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

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

    struct Fixture {
        _dir: TempDir,
        modal: SettingsModal,
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
        let files = SettingsFiles {
            settings: GlobalSettingsFile::load(&path).expect("settings file"),
            websearch: GlobalWebsearchFile::load(&path).expect("websearch file"),
        };
        modal.receive(SettingsReply {
            data: Ok(Box::new(SettingsData {
                files,
                project,
                statuses: Vec::new(),
                default_backends: vec![WebsearchBackendName::ExaMcp],
                default_mode: WebsearchMode::Fallback,
            })),
            error: None,
            note: None,
        });
        Fixture { _dir: dir, modal }
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
        let mut terminal =
            Terminal::new(TestBackend::new(SCREEN_WIDTH, SCREEN_HEIGHT)).expect("terminal");
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
        scalar_fields(TOP_LEVEL_FIELDS)
            .iter()
            .position(|field| ScalarKind::of(field) == Some(ScalarKind::Bool))
            .expect("a top-level bool key")
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
        let field = scalar_fields(TOP_LEVEL_FIELDS)[index];
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
        let key = scalar_fields(TOP_LEVEL_FIELDS)[0].name;
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
        assert_eq!(key, scalar_fields(TOP_LEVEL_FIELDS)[0].name);
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
        assert_eq!(fixture.modal.docs_topic(), Some(DocsTopic::Settings));
    }
}
