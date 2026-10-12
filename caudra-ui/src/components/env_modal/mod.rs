//! `/env`: the variables a shell command of this session receives, and edits that change them
//! from the next command. Values stay masked until one row is revealed, and are never copied.

mod form;
mod local;
mod rows;
mod sandbox;

use std::mem;

use caudra_config::interpolation::holds_reference;
use caudra_config::shell_env::{GlobalShellEnvFile, ShellEnvEdit, ShellEnvError};
use caudra_grab::grab_scope;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use workcell_tool_contract::shell_env::{
    EnvPattern, EnvPatternError, ShellEnvironmentSelection, is_shell_startup_variable,
    is_valid_variable_name, looks_secret, validate_literal,
};

use crate::components::keybindings::key;
use crate::components::list_picker::{FooterBuilder, ListPicker, PickerAction, PickerItem};
use crate::components::tooltip::Tip;
use crate::components::{Hint, Overlay, ascii_key, escape_terminal_controls, plain_char};
use crate::repaint::Cadence;
use crate::theme;

use form::{EnvForm, FormKey, FormKind};
pub(crate) use local::LocalEnvironment;
use local::local_rows;
use rows::{EnvRow, FORWARDED_SECTION, RowAction, Unforward};
pub(crate) use sandbox::GuestListing;
use sandbox::sandbox_rows;

pub(crate) const ADD_LABEL: &str = "a";
pub(crate) const EDIT_LABEL: &str = "e";
pub(crate) const DELETE_LABEL: &str = "d";
pub(crate) const PATTERN_LABEL: &str = "p";
pub(crate) const REVEAL_LABEL: &str = "v";
pub(crate) const FILTER_LABEL: &str = "/";
pub(crate) const CONFIRM_LABEL: &str = "y";
const TOGGLE_KEY: char = ' ';
const ADD_KEY: char = ascii_key(ADD_LABEL);
const EDIT_KEY: char = ascii_key(EDIT_LABEL);
const DELETE_KEY: char = ascii_key(DELETE_LABEL);
pub(crate) const PATTERN_KEY: char = ascii_key(PATTERN_LABEL);
const REVEAL_KEY: char = ascii_key(REVEAL_LABEL);
const FILTER_KEY: char = ascii_key(FILTER_LABEL);
const CONFIRM_KEY: char = ascii_key(CONFIRM_LABEL);
const COMMAND_KEYS: &[char] = &[
    TOGGLE_KEY,
    ADD_KEY,
    EDIT_KEY,
    DELETE_KEY,
    PATTERN_KEY,
    REVEAL_KEY,
    FILTER_KEY,
];
const TITLE: &str = " Environment ";
const LOCAL_TAB: &str = "Local";
const SANDBOX_TAB: &str = "Sandbox";
const TAB_GAP: &str = "  ";
const COUNT_SEPARATOR: &str = " · ";
const LOADING: &str = "Loading…";
const NOTHING: &str = "Nothing to show";
const SAVING: &str = "Saving…";
const SAVED: &str = "Saved. Applies from the next shell command.";
const BUSY: &str = "Wait for the last change to finish.";
const NOT_LOADED: &str = "Wait for the list to load.";
const EDIT_HINT: &str = "Select a set value to edit it.";
const DELETE_HINT: &str = "Select a set value or a pass pattern to delete it.";
const NOTHING_TO_REVEAL: &str = "This row has no value to reveal.";
const LOCAL_SESSION: &str =
    "This session's shell runs on this machine. Changes apply from the next command.";
const NOT_APPLIED: &str = "Not applied to this session";
const UNSUPPORTED: &str =
    "This sandbox's Workcell cannot forward guest variables; update its image.";
const TRUNCATED: &str = "the guest listed only part of its environment";
const CONFIRM_SUFFIX: &str = "Enter/y confirms, Esc cancels.";
const LITERAL_WARNING_BYTES: usize = 24;
const LOCAL_VALUE_HINT: &str = "A literal, {env:NAME} to read Caudra's own environment, or {file:PATH} to read a file. Tab switches fields, Enter saves, Esc cancels.";
const SANDBOX_VALUE_HINT: &str = "A non-secret literal the guest receives as written. Tab switches fields, Enter saves, Esc cancels.";
const LOCAL_PATTERN_HINT: &str =
    "An exact NAME or a NAME_* prefix of variables Caudra can see. Enter adds it, Esc cancels.";
const SANDBOX_PATTERN_HINT: &str = "An exact NAME, a NAME_* prefix, or * for every name the guest admits. Enter adds it, Esc cancels.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EnvTab {
    Local,
    Sandbox,
}

pub(crate) struct SandboxTarget {
    pub(crate) name: String,
    /// The saved profile edits persist to, besides this session.
    pub(crate) profile: Option<String>,
    pub(crate) supported: bool,
}

/// Where this session's shell commands run, which decides the tabs and what the banner says.
pub(crate) enum EnvTarget {
    Local,
    Remote { label: String },
    Sandbox(SandboxTarget),
}

pub(crate) enum EnvRequest {
    LoadLocal,
    EditLocal {
        file: Box<GlobalShellEnvFile>,
        edit: ShellEnvEdit,
    },
    LoadSandbox,
    EditSandbox(ShellEnvironmentSelection),
}

pub(crate) enum EnvReply {
    Local {
        environment: Result<Box<LocalEnvironment>, String>,
        error: Option<String>,
    },
    Sandbox {
        listing: Result<GuestListing, String>,
        error: Option<String>,
    },
}

pub(crate) enum EnvAction {
    Consumed,
    Close,
    Copy(String),
    Request(EnvRequest),
}

/// One edit, before it is turned into the tab's own write.
enum Change {
    Forward(String),
    Unforward(String, Unforward),
    SetValue { name: String, value: String },
    RemoveValue(String),
    AddPattern(EnvPattern),
    RemovePattern(String),
}

struct Pending {
    prompt: String,
    change: Change,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Work {
    Load,
    Write,
}

pub(crate) struct EnvModal {
    picker: ListPicker<EnvRow>,
    target: EnvTarget,
    tab: EnvTab,
    local: Option<LocalEnvironment>,
    guest: Option<GuestListing>,
    form: Option<EnvForm>,
    pending: Option<Pending>,
    filtering: bool,
    revealed: Option<(&'static str, String)>,
    work: Option<Work>,
    message: Option<String>,
    error: Option<String>,
    /// The rows no longer match the data or the reveal, so the next settle rebuilds them.
    stale: bool,
}

impl EnvModal {
    pub(crate) fn new() -> Self {
        Self {
            picker: ListPicker::new(),
            target: EnvTarget::Local,
            tab: EnvTab::Local,
            local: None,
            guest: None,
            form: None,
            pending: None,
            filtering: false,
            revealed: None,
            work: None,
            message: None,
            error: None,
            stale: false,
        }
    }

    pub(crate) fn open(&mut self, target: EnvTarget) -> EnvAction {
        Overlay::close(self);
        self.tab = match target {
            EnvTarget::Sandbox(_) => EnvTab::Sandbox,
            EnvTarget::Local | EnvTarget::Remote { .. } => EnvTab::Local,
        };
        self.target = target;
        self.picker
            .open_selectively_toggleable(Vec::new(), Vec::new(), Vec::new(), TITLE);
        let action = self.load();
        self.rebuild();
        action
    }

    pub(crate) fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub(crate) fn tab(&self) -> EnvTab {
        self.tab
    }

    pub(crate) fn receive(&mut self, reply: EnvReply) {
        if !self.is_open() {
            return;
        }
        let wrote = self.work == Some(Work::Write);
        self.work = None;
        let (failure, error) = match reply {
            EnvReply::Local { environment, error } => (
                environment.map(|local| self.local = Some(*local)).err(),
                error,
            ),
            EnvReply::Sandbox { listing, error } => {
                (listing.map(|guest| self.guest = Some(guest)).err(), error)
            }
        };
        self.error = error.or(failure);
        self.message = (wrote && self.error.is_none()).then(|| SAVED.to_owned());
        self.rebuild();
    }

    pub(crate) fn text_input_active(&self) -> bool {
        self.form.is_some() || self.filtering
    }

    /// Whether typed text edits a field or the filter rather than naming a command.
    pub(crate) fn text_key(&self, key: Option<KeyEvent>) -> bool {
        self.text_input_active()
            || (self.pending.is_none()
                && !key
                    .as_ref()
                    .and_then(plain_char)
                    .is_some_and(|character| COMMAND_KEYS.contains(&character)))
    }

    pub(crate) fn handle_text(&mut self, text: &str, paste: bool) {
        if self.pending.is_some() {
            return;
        }
        match self.form.as_mut() {
            Some(form) => form.handle_text(text, paste),
            None => {
                self.picker.handle_text(text, paste);
            }
        }
        self.settle();
    }

    pub(crate) fn handle_paste(&mut self, text: &str) -> bool {
        if !self.is_open() {
            return false;
        }
        self.handle_text(text, true);
        true
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> EnvAction {
        self.message = None;
        let action = self.dispatch_key(key);
        self.settle();
        action
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> EnvAction {
        if self.form.is_some() || self.pending.is_some() {
            return EnvAction::Consumed;
        }
        let action = match self.picker.handle_mouse(event) {
            PickerAction::Key(key) => return self.handle_key(key),
            action => self.map_picker_action(action),
        };
        self.settle();
        action
    }

    pub(crate) fn contains(&self, position: Position) -> bool {
        self.picker.contains(position)
            || self
                .form
                .as_ref()
                .is_some_and(|form| form.contains(position))
    }

    pub(crate) fn scroll(&mut self, delta: i32) {
        if self.form.is_none() && self.pending.is_none() {
            self.picker.scroll(delta);
        }
    }

    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("env_modal", area);
        let popup = self.picker.view(frame, area);
        if let Some(form) = self.form.as_mut() {
            form.view(frame, area);
        }
        popup
    }

    fn dispatch_key(&mut self, key: KeyEvent) -> EnvAction {
        if let Some(form) = self.form.as_mut() {
            return match form.handle_key(key) {
                FormKey::Consumed => EnvAction::Consumed,
                FormKey::Cancel => {
                    self.form = None;
                    EnvAction::Consumed
                }
                FormKey::Submit => self.submit_form(),
            };
        }
        if let Some(pending) = self.pending.take() {
            self.stale = true;
            return match key.code {
                KeyCode::Enter => self.commit(pending.change),
                KeyCode::Char(CONFIRM_KEY) => self.commit(pending.change),
                KeyCode::Esc => EnvAction::Consumed,
                _ => {
                    self.pending = Some(pending);
                    EnvAction::Consumed
                }
            };
        }
        if self.filtering {
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.picker.clear_search();
                }
                _ => return self.picker_key(key),
            }
            return EnvAction::Consumed;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            return self.switch_tab();
        }
        match plain_char(&key) {
            Some(TOGGLE_KEY) => self.picker_key(key::ENTER.to_key_event()),
            Some(ADD_KEY) => self.open_value_form(false),
            Some(EDIT_KEY) => self.open_value_form(true),
            Some(DELETE_KEY) => self.delete_selected(),
            Some(PATTERN_KEY) => self.open_pattern_form(),
            Some(REVEAL_KEY) => {
                self.toggle_reveal();
                EnvAction::Consumed
            }
            Some(FILTER_KEY) => {
                self.filtering = true;
                EnvAction::Consumed
            }
            _ => self.picker_key(key),
        }
    }

    fn picker_key(&mut self, key: KeyEvent) -> EnvAction {
        let action = self.picker.handle_key(key);
        self.map_picker_action(action)
    }

    fn map_picker_action(&mut self, action: PickerAction<EnvRow>) -> EnvAction {
        match action {
            PickerAction::Toggle(index, _) => {
                self.stale = true;
                self.toggle(index)
            }
            PickerAction::Select(_) | PickerAction::Close => EnvAction::Close,
            PickerAction::Copy(text) => EnvAction::Copy(text),
            PickerAction::Consumed | PickerAction::Key(_) => EnvAction::Consumed,
        }
    }

    fn switch_tab(&mut self) -> EnvAction {
        if !matches!(self.target, EnvTarget::Sandbox(_)) || self.work == Some(Work::Write) {
            return EnvAction::Consumed;
        }
        self.tab = match self.tab {
            EnvTab::Local => EnvTab::Sandbox,
            EnvTab::Sandbox => EnvTab::Local,
        };
        self.revealed = None;
        self.error = None;
        self.picker.clear_search();
        self.picker.select(0);
        self.stale = true;
        self.load()
    }

    fn load(&mut self) -> EnvAction {
        if !self.supported() {
            self.work = None;
            return EnvAction::Consumed;
        }
        self.work = Some(Work::Load);
        EnvAction::Request(match self.tab {
            EnvTab::Local => EnvRequest::LoadLocal,
            EnvTab::Sandbox => EnvRequest::LoadSandbox,
        })
    }

    /// Whether the active tab can list and change anything at all.
    fn supported(&self) -> bool {
        match (&self.target, self.tab) {
            (EnvTarget::Sandbox(sandbox), EnvTab::Sandbox) => sandbox.supported,
            _ => true,
        }
    }

    fn loaded(&self) -> bool {
        match self.tab {
            EnvTab::Local => self.local.is_some(),
            EnvTab::Sandbox => self.guest.is_some(),
        }
    }

    /// Why no edit can start now.
    fn blocked(&self) -> Option<&'static str> {
        if !self.supported() {
            Some(UNSUPPORTED)
        } else if self.work == Some(Work::Write) {
            Some(BUSY)
        } else if !self.loaded() {
            Some(NOT_LOADED)
        } else {
            None
        }
    }

    fn refuse_if_blocked(&mut self) -> bool {
        let blocked = self.blocked();
        if let Some(reason) = blocked {
            self.message = Some(reason.to_owned());
        }
        blocked.is_some()
    }

    fn toggle(&mut self, index: usize) -> EnvAction {
        let Some(row) = self.picker.item(index) else {
            return EnvAction::Consumed;
        };
        let change = match &row.action {
            RowAction::Forward => Change::Forward(row.name.clone()),
            RowAction::Unforward(how) => Change::Unforward(row.name.clone(), how.clone()),
            RowAction::None | RowAction::SetEntry | RowAction::Pattern(_) => {
                return EnvAction::Consumed;
            }
        };
        if self.refuse_if_blocked() {
            return EnvAction::Consumed;
        }
        self.confirm_or_commit(change)
    }

    fn delete_selected(&mut self) -> EnvAction {
        let change = match self.picker.selected_item().map(|row| &row.action) {
            Some(RowAction::SetEntry) => self
                .picker
                .selected_item()
                .map(|row| Change::RemoveValue(row.name.clone())),
            Some(RowAction::Pattern(raw)) => Some(Change::RemovePattern(raw.clone())),
            _ => None,
        };
        let Some(change) = change else {
            self.message = Some(DELETE_HINT.to_owned());
            return EnvAction::Consumed;
        };
        if self.refuse_if_blocked() {
            return EnvAction::Consumed;
        }
        self.confirm_or_commit(change)
    }

    fn open_value_form(&mut self, editing: bool) -> EnvAction {
        if self.refuse_if_blocked() {
            return EnvAction::Consumed;
        }
        let (secret, hint) = match self.tab {
            EnvTab::Local => (true, LOCAL_VALUE_HINT),
            EnvTab::Sandbox => (false, SANDBOX_VALUE_HINT),
        };
        if !editing {
            self.form = Some(EnvForm::value(None, secret, hint));
            return EnvAction::Consumed;
        }
        let existing = self
            .picker
            .selected_item()
            .filter(|row| row.action == RowAction::SetEntry)
            .and_then(|row| Some((row.name.clone(), self.set_value(&row.name)?.to_owned())));
        match existing {
            Some((name, value)) => {
                self.form = Some(EnvForm::value(Some((&name, &value)), secret, hint));
            }
            None => self.message = Some(EDIT_HINT.to_owned()),
        }
        EnvAction::Consumed
    }

    fn open_pattern_form(&mut self) -> EnvAction {
        if self.refuse_if_blocked() {
            return EnvAction::Consumed;
        }
        self.form = Some(EnvForm::pattern(match self.tab {
            EnvTab::Local => LOCAL_PATTERN_HINT,
            EnvTab::Sandbox => SANDBOX_PATTERN_HINT,
        }));
        EnvAction::Consumed
    }

    fn set_value(&self, name: &str) -> Option<&str> {
        match self.tab {
            EnvTab::Local => self.local.as_ref()?.file.set().get(name),
            EnvTab::Sandbox => self.guest.as_ref()?.selection.set.get(name),
        }
        .map(String::as_str)
    }

    fn submit_form(&mut self) -> EnvAction {
        let Some(mut form) = self.form.take() else {
            return EnvAction::Consumed;
        };
        let name = form.name();
        let checked = match form.kind {
            FormKind::Value => {
                let value = form.value_text();
                self.check_value(&name, &value)
                    .map(|warning| (Change::SetValue { name, value }, warning))
            }
            FormKind::Pattern => self
                .check_pattern(&name)
                .map(|pattern| (Change::AddPattern(pattern), None)),
        };
        match checked {
            Err(error) => form.fail(error),
            Ok((change, warning)) => {
                if !warning.is_some_and(|warning| form.warn(warning)) {
                    return self.confirm_or_commit(change);
                }
            }
        }
        self.form = Some(form);
        EnvAction::Consumed
    }

    /// The value's refusal, or the warning a literal that looks like a credential earns.
    fn check_value(&self, name: &str, value: &str) -> Result<Option<String>, String> {
        if self.tab == EnvTab::Sandbox {
            return validate_literal(name, value)
                .map(|()| None)
                .map_err(|error| error.to_string());
        }
        if !is_valid_variable_name(name) {
            return Err(EnvPatternError::Invalid(name.to_owned()).to_string());
        }
        if is_shell_startup_variable(name) {
            return Err(EnvPatternError::Reserved(name.to_owned()).to_string());
        }
        let literal = !holds_reference(value);
        Ok(
            (literal && (looks_secret(name) || value.len() >= LITERAL_WARNING_BYTES))
                .then(|| literal_warning(name)),
        )
    }

    fn check_pattern(&self, text: &str) -> Result<EnvPattern, String> {
        let pattern = EnvPattern::parse(text).map_err(|error| error.to_string())?;
        if pattern == EnvPattern::All && self.tab == EnvTab::Local {
            return Err(ShellEnvError::Everything.to_string());
        }
        if let Some(name) = pattern.reserved_capture() {
            return Err(EnvPatternError::Reserved(name).to_string());
        }
        Ok(pattern)
    }

    fn sensitive_match(&self, pattern: &EnvPattern) -> Option<String> {
        match self.tab {
            EnvTab::Local => self.local.as_ref()?.sensitive_match(pattern),
            EnvTab::Sandbox => self.guest.as_ref()?.sensitive_match(pattern),
        }
    }

    fn confirm_or_commit(&mut self, change: Change) -> EnvAction {
        let prompt = match &change {
            Change::Forward(name) => self
                .sensitive_match(&EnvPattern::Exact(name.clone()))
                .map(|name| sensitive_prompt(&name)),
            Change::AddPattern(pattern) => self
                .sensitive_match(pattern)
                .map(|name| sensitive_prompt(&name)),
            Change::Unforward(name, Unforward::Everything) => Some(format!(
                "Switch from * to an explicit list without {name}? {CONFIRM_SUFFIX}"
            )),
            Change::RemoveValue(name) => Some(format!("Stop setting {name}? {CONFIRM_SUFFIX}")),
            Change::RemovePattern(raw) => Some(format!(
                "Stop forwarding {}? {CONFIRM_SUFFIX}",
                escape_terminal_controls(raw)
            )),
            Change::Unforward(_, Unforward::Exact) | Change::SetValue { .. } => None,
        };
        match prompt {
            Some(prompt) => {
                self.pending = Some(Pending { prompt, change });
                EnvAction::Consumed
            }
            None => self.commit(change),
        }
    }

    fn commit(&mut self, change: Change) -> EnvAction {
        let request = match self.tab {
            EnvTab::Local => self
                .local
                .as_ref()
                .map(|local| EnvRequest::EditLocal {
                    file: Box::new(local.file.clone()),
                    edit: local_edit(change),
                })
                .ok_or_else(|| NOT_LOADED.to_owned()),
            EnvTab::Sandbox => self
                .guest
                .as_ref()
                .ok_or_else(|| NOT_LOADED.to_owned())
                .and_then(|guest| guest.edited(change))
                .map(EnvRequest::EditSandbox),
        };
        match request {
            Ok(request) => {
                self.work = Some(Work::Write);
                self.error = None;
                EnvAction::Request(request)
            }
            Err(error) => {
                self.error = Some(error);
                EnvAction::Consumed
            }
        }
    }

    fn toggle_reveal(&mut self) {
        let Some(row) = self.picker.selected_item() else {
            return;
        };
        if !row.revealable() {
            self.message = Some(NOTHING_TO_REVEAL.to_owned());
            return;
        }
        let identity = (row.section, row.name.clone());
        self.revealed = match self.revealed.take() {
            Some(current) if current == identity => None,
            _ => Some(identity),
        };
        self.stale = true;
    }

    /// A reveal belongs to the row it was asked for, so moving off it masks the value again.
    fn sync_reveal(&mut self) {
        let moved = self.revealed.as_ref().is_some_and(|(section, name)| {
            self.picker
                .selected_item()
                .is_none_or(|row| row.identity() != (*section, name.as_str()))
        });
        if moved {
            self.revealed = None;
            self.stale = true;
        }
    }

    fn settle(&mut self) {
        self.sync_reveal();
        match mem::take(&mut self.stale) {
            true => self.rebuild(),
            false => self.refresh_info(),
        }
    }

    fn rebuild(&mut self) {
        if !self.is_open() {
            return;
        }
        let mut rows = match self.tab {
            EnvTab::Local => self.local.as_ref().map(local_rows),
            EnvTab::Sandbox => self.guest.as_ref().map(sandbox_rows),
        }
        .unwrap_or_default();
        if let Some((section, name)) = &self.revealed
            && let Some(row) = rows
                .iter_mut()
                .find(|row| row.identity() == (*section, name.as_str()))
        {
            row.reveal();
        }
        let checked = rows.iter().map(|row| row.checked).collect();
        let toggleable = rows.iter().map(EnvRow::toggleable).collect();
        self.picker
            .replace_selectively_toggleable(rows, checked, toggleable);
        self.stale = false;
        self.refresh_info();
    }

    fn refresh_info(&mut self) {
        self.picker.set_empty_text(match self.work {
            Some(Work::Load) => LOADING,
            Some(Work::Write) | None => NOTHING,
        });
        self.picker.set_error_text(self.error.clone());
        self.picker.set_info_lines(Some(self.info_lines()));
        self.picker.set_footer_builder(self.footer());
    }

    fn info_lines(&self) -> Vec<Line<'static>> {
        let t = theme::current();
        let mut lines = Vec::new();
        if let EnvTarget::Sandbox(sandbox) = &self.target {
            let style = |tab| match self.tab == tab {
                true => t.item_selected,
                false => t.tool_dim,
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {LOCAL_TAB} "), style(EnvTab::Local)),
                Span::raw(TAB_GAP),
                Span::styled(
                    format!(" {SANDBOX_TAB} {} ", sandbox.name),
                    style(EnvTab::Sandbox),
                ),
            ]));
        }
        lines.push(Line::from(self.banner()));
        if self.loaded() {
            lines.push(Line::from(Span::styled(self.counts(), t.tool_dim)));
        }
        let status = if let Some(pending) = &self.pending {
            Some((pending.prompt.clone(), t.tool_warning))
        } else if self.work == Some(Work::Write) {
            Some((SAVING.to_owned(), t.tool_dim))
        } else if let Some(message) = &self.message {
            Some((message.clone(), t.tool_dim))
        } else {
            self.picker
                .selected_item()
                .and_then(|row| row.note.clone())
                .map(|note| (note, t.item_desc))
        };
        if let Some((text, style)) = status {
            lines.push(Line::from(Span::styled(text, style)));
        }
        lines
    }

    fn banner(&self) -> String {
        match (&self.target, self.tab) {
            (EnvTarget::Local, _) => LOCAL_SESSION.to_owned(),
            (EnvTarget::Remote { label }, _) => format!(
                "{NOT_APPLIED}: the shell runs on {}, whose environment its operator sets. These settings apply to local sessions.",
                escape_terminal_controls(label)
            ),
            (EnvTarget::Sandbox(sandbox), EnvTab::Local) => format!(
                "This session's shell runs in sandbox {}, so these settings apply to local sessions.",
                sandbox.name
            ),
            (EnvTarget::Sandbox(sandbox), EnvTab::Sandbox) if !sandbox.supported => {
                UNSUPPORTED.to_owned()
            }
            (EnvTarget::Sandbox(sandbox), EnvTab::Sandbox) => match &sandbox.profile {
                Some(profile) => format!(
                    "This session's shell runs in sandbox {}. Changes apply from the next command and are saved to profile {profile}.",
                    sandbox.name
                ),
                None => format!(
                    "This session's shell runs in sandbox {}. Changes apply from the next command, for this session only.",
                    sandbox.name
                ),
            },
        }
    }

    fn counts(&self) -> String {
        let rows: Vec<&EnvRow> = (0..).map_while(|index| self.picker.item(index)).collect();
        let forwarded = rows
            .iter()
            .filter(|row| row.section == FORWARDED_SECTION || row.action == RowAction::SetEntry)
            .count();
        let visible = rows.iter().filter(|row| row.is_variable()).count();
        let withheld = rows.iter().filter(|row| row.is_disabled()).count();
        let mut counts = [
            format!("{forwarded} forwarded"),
            format!("{visible} visible"),
            format!("{withheld} not forwardable"),
        ]
        .join(COUNT_SEPARATOR);
        if self.tab == EnvTab::Sandbox && self.guest.as_ref().is_some_and(|guest| guest.truncated) {
            counts.push_str(COUNT_SEPARATOR);
            counts.push_str(TRUNCATED);
        }
        counts
    }

    fn footer(&self) -> FooterBuilder {
        if self.pending.is_some() {
            confirm_footer
        } else if self.filtering {
            filter_footer
        } else if !self.supported() {
            read_only_footer
        } else if matches!(self.target, EnvTarget::Sandbox(_)) {
            tabbed_footer
        } else {
            browse_footer
        }
    }
}

impl Overlay for EnvModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.picker.close();
        self.local = None;
        self.guest = None;
        self.form = None;
        self.pending = None;
        self.filtering = false;
        self.revealed = None;
        self.work = None;
        self.message = None;
        self.error = None;
        self.stale = false;
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }

    fn tooltip(&self) -> Option<Tip> {
        self.picker.tooltip()
    }
}

fn local_edit(change: Change) -> ShellEnvEdit {
    match change {
        Change::Forward(name) => ShellEnvEdit::AddPass(EnvPattern::Exact(name)),
        Change::Unforward(name, _) => ShellEnvEdit::RemovePass(name),
        Change::SetValue { name, value } => ShellEnvEdit::SetValue { name, value },
        Change::RemoveValue(name) => ShellEnvEdit::RemoveValue(name),
        Change::AddPattern(pattern) => ShellEnvEdit::AddPass(pattern),
        Change::RemovePattern(raw) => ShellEnvEdit::RemovePass(raw),
    }
}

fn sensitive_prompt(name: &str) -> String {
    format!("Any command the model runs can read {name}. Forward it? {CONFIRM_SUFFIX}")
}

fn literal_warning(name: &str) -> String {
    format!(
        "{name} would sit in caudra.toml in plain text. Put it in the global .env and set {{env:{name}}} instead, or press Enter again to save it."
    )
}

fn browse_hints() -> Vec<Hint> {
    vec![
        Hint::bind(key::SPACE, "forward"),
        Hint::char(ADD_LABEL, "set"),
        Hint::char(EDIT_LABEL, "edit"),
        Hint::char(DELETE_LABEL, "delete"),
        Hint::char(PATTERN_LABEL, "pattern"),
        Hint::char(REVEAL_LABEL, "reveal"),
        Hint::char(FILTER_LABEL, "filter"),
    ]
}

fn browse_footer() -> Vec<Hint> {
    let mut hints = browse_hints();
    hints.push(Hint::bind(key::ESC, "close"));
    hints
}

fn tabbed_footer() -> Vec<Hint> {
    let mut hints = browse_hints();
    hints.push(Hint::bind(key::TAB, "tab"));
    hints.push(Hint::bind(key::ESC, "close"));
    hints
}

fn read_only_footer() -> Vec<Hint> {
    vec![Hint::bind(key::TAB, "tab"), Hint::bind(key::ESC, "close")]
}

fn filter_footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "keep filter"),
        Hint::bind(key::ESC, "clear filter"),
    ]
}

fn confirm_footer() -> Vec<Hint> {
    vec![
        Hint::char(CONFIRM_LABEL, "confirm"),
        Hint::bind(key::ESC, "cancel"),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};
    use std::fs;

    use caudra_config::EnvSource;
    use caudra_workcell::host_contract::{EnvironmentEntry, EnvironmentEntryState};
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::local::{HostVariable, NOT_FORWARDABLE_SECTION, PROJECT_NOTE};
    use super::rows::{BUILT_IN_STATE, MASK};
    use super::sandbox::{STRIPPED_DETAIL, STRIPPED_SECTION};
    use super::*;
    use crate::components::{buffer_text, key};

    const CONFIG_FILE: &str = "caudra.toml";
    const SECRET_NAME: &str = "DEPLOY_TOKEN";
    const SECRET_VALUE: &str = "sk-live-0123456789abcdef";
    const OTHER_NAME: &str = "BUILD_FLAVOR";
    const OTHER_VALUE: &str = "release-candidate-2026";
    const PLAIN_NAME: &str = "MY_TOOL_FLAG";
    const BUILT_IN_NAME: &str = "PATH";
    const REMOTE_LABEL: &str = "build-box";
    const SANDBOX_NAME: &str = "dev";
    const STRIPPED_NAME: &str = "WORKCELL_API_KEY";
    const FIRST_GUEST: &str = "GUEST_ONE";
    const SECOND_GUEST: &str = "GUEST_TWO";
    const SCREEN_WIDTH: u16 = 160;
    const SCREEN_HEIGHT: u16 = 40;
    const SHOWN_UNREVEALED: &str = "a value must stay masked until its row is revealed";
    const REVEAL_LEAKED: &str = "revealing one row must not reveal another";
    const REVEAL_STUCK: &str = "moving off a revealed row masks it again";
    const TOGGLED_FIXED: &str = "a fixed row must not turn into an edit";

    struct LocalFixture {
        _directory: TempDir,
        modal: EnvModal,
    }

    fn local_fixture(config: &str, variables: &[(&str, &str, EnvSource)]) -> LocalFixture {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(CONFIG_FILE);
        fs::write(&path, config).unwrap();
        let environment = LocalEnvironment {
            file: GlobalShellEnvFile::load(&path).unwrap(),
            variables: variables
                .iter()
                .map(|(name, value, source)| {
                    (
                        (*name).to_owned(),
                        HostVariable {
                            value: (*value).to_owned(),
                            source: *source,
                        },
                    )
                })
                .collect(),
            credentials: HashSet::new(),
        };
        let mut modal = EnvModal::new();
        assert!(matches!(
            modal.open(EnvTarget::Local),
            EnvAction::Request(EnvRequest::LoadLocal)
        ));
        modal.receive(EnvReply::Local {
            environment: Ok(Box::new(environment)),
            error: None,
        });
        LocalFixture {
            _directory: directory,
            modal,
        }
    }

    fn sandbox_modal(supported: bool, listing: GuestListing) -> EnvModal {
        let mut modal = EnvModal::new();
        modal.open(EnvTarget::Sandbox(SandboxTarget {
            name: SANDBOX_NAME.to_owned(),
            profile: None,
            supported,
        }));
        if supported {
            modal.receive(EnvReply::Sandbox {
                listing: Ok(listing),
                error: None,
            });
        }
        modal
    }

    fn guest(name: &str, value: Option<&str>, state: EnvironmentEntryState) -> EnvironmentEntry {
        EnvironmentEntry {
            name: name.to_owned(),
            value: value.map(str::to_owned),
            state,
        }
    }

    fn render(modal: &mut EnvModal) -> String {
        let mut terminal = Terminal::new(TestBackend::new(SCREEN_WIDTH, SCREEN_HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn select(modal: &mut EnvModal, name: &str) {
        assert!(
            modal
                .picker
                .select_item_by(|row| row.name == name && row.is_variable())
        );
    }

    fn row<'a>(modal: &'a EnvModal, name: &str) -> &'a EnvRow {
        (0..)
            .map_while(|index| modal.picker.item(index))
            .find(|row| row.name == name && row.is_variable())
            .unwrap()
    }

    fn info(modal: &EnvModal) -> String {
        modal
            .info_lines()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(modal: &mut EnvModal, code: KeyCode) -> EnvAction {
        modal.handle_key(key(code))
    }

    fn local_edit_of(action: EnvAction) -> ShellEnvEdit {
        match action {
            EnvAction::Request(EnvRequest::EditLocal { edit, .. }) => edit,
            _ => panic!("expected a local edit"),
        }
    }

    #[test_case(SECRET_VALUE, true; "literal")]
    #[test_case("{env:CAUDRA_DEPLOY_TOKEN}", false; "variable_reference")]
    #[test_case("{file:~/.secrets/deploy-token}", false; "file_reference")]
    fn only_a_literal_credential_earns_the_plain_text_warning(value: &str, warned: bool) {
        let fixture = local_fixture("", &[]);
        let warning = fixture.modal.check_value(SECRET_NAME, value).unwrap();
        assert_eq!(warning.is_some(), warned, "{warning:?}");
    }

    #[test]
    fn values_stay_masked_until_one_row_is_revealed() {
        let mut fixture = local_fixture(
            "",
            &[
                (SECRET_NAME, SECRET_VALUE, EnvSource::Process),
                (OTHER_NAME, OTHER_VALUE, EnvSource::Process),
            ],
        );
        let modal = &mut fixture.modal;
        let screen = render(modal);
        assert!(!screen.contains(SECRET_VALUE), "{SHOWN_UNREVEALED}");
        assert!(!screen.contains(OTHER_VALUE), "{SHOWN_UNREVEALED}");
        assert!(screen.contains(&rows::masked(SECRET_VALUE)), "{screen}");

        select(modal, SECRET_NAME);
        press(modal, KeyCode::Char(REVEAL_KEY));
        let screen = render(modal);
        assert!(screen.contains(SECRET_VALUE));
        assert!(!screen.contains(OTHER_VALUE), "{REVEAL_LEAKED}");

        let other = (0..)
            .map_while(|index| modal.picker.item(index))
            .position(|row| row.name == OTHER_NAME)
            .unwrap();
        let secret = (0..)
            .map_while(|index| modal.picker.item(index))
            .position(|row| row.name == SECRET_NAME)
            .unwrap();
        press(
            modal,
            if other < secret {
                KeyCode::Up
            } else {
                KeyCode::Down
            },
        );
        assert!(!render(modal).contains(SECRET_VALUE), "{REVEAL_STUCK}");
    }

    #[test]
    fn a_short_value_is_fully_masked() {
        let fixture = local_fixture("", &[(PLAIN_NAME, "short", EnvSource::Process)]);
        assert_eq!(row(&fixture.modal, PLAIN_NAME).detail(), Some(MASK));
    }

    #[test]
    fn built_in_rows_cannot_be_toggled() {
        let mut fixture = local_fixture("", &[(BUILT_IN_NAME, "/usr/bin", EnvSource::Process)]);
        let modal = &mut fixture.modal;
        assert_eq!(row(modal, BUILT_IN_NAME).suffix(), Some(BUILT_IN_STATE));
        assert!(!row(modal, BUILT_IN_NAME).toggleable());
        select(modal, BUILT_IN_NAME);
        assert!(
            matches!(press(modal, KeyCode::Char(TOGGLE_KEY)), EnvAction::Consumed),
            "{TOGGLED_FIXED}"
        );
        assert!(modal.pending.is_none());
    }

    #[test_case("" => ShellEnvEdit::AddPass(EnvPattern::Exact(PLAIN_NAME.into())) ; "forward")]
    #[test_case("[agent.shell_env]\npass = [\"MY_TOOL_FLAG\"]\n" => ShellEnvEdit::RemovePass(PLAIN_NAME.into()) ; "unforward")]
    fn toggling_a_variable_edits_the_pass_list(config: &str) -> ShellEnvEdit {
        let mut fixture = local_fixture(config, &[(PLAIN_NAME, "on", EnvSource::Process)]);
        select(&mut fixture.modal, PLAIN_NAME);
        local_edit_of(press(&mut fixture.modal, KeyCode::Char(TOGGLE_KEY)))
    }

    #[test]
    fn forwarding_a_credential_asks_first() {
        let mut fixture = local_fixture("", &[(SECRET_NAME, SECRET_VALUE, EnvSource::Process)]);
        let modal = &mut fixture.modal;
        select(modal, SECRET_NAME);
        assert!(matches!(
            press(modal, KeyCode::Char(TOGGLE_KEY)),
            EnvAction::Consumed
        ));
        assert!(info(modal).contains(&sensitive_prompt(SECRET_NAME)));
        assert_eq!(
            local_edit_of(press(modal, KeyCode::Char(CONFIRM_KEY))),
            ShellEnvEdit::AddPass(EnvPattern::Exact(SECRET_NAME.into()))
        );
    }

    #[test]
    fn escape_cancels_a_sensitive_forward() {
        let mut fixture = local_fixture("", &[(SECRET_NAME, SECRET_VALUE, EnvSource::Process)]);
        let modal = &mut fixture.modal;
        select(modal, SECRET_NAME);
        press(modal, KeyCode::Char(TOGGLE_KEY));
        assert!(matches!(press(modal, KeyCode::Esc), EnvAction::Consumed));
        assert!(modal.pending.is_none());
        assert!(modal.is_open());
    }

    #[test_case("*", ShellEnvError::Everything.to_string() ; "bare_star")]
    #[test_case("BASH_ENV", EnvPatternError::Reserved("BASH_ENV".into()).to_string() ; "reserved_name")]
    fn local_patterns_refuse_everything_and_bash_startup_names(pattern: &str, error: String) {
        let mut fixture = local_fixture("", &[]);
        let modal = &mut fixture.modal;
        press(modal, KeyCode::Char(PATTERN_KEY));
        modal.handle_text(pattern, false);
        assert!(matches!(press(modal, KeyCode::Enter), EnvAction::Consumed));
        assert!(modal.form.is_some());
        assert_eq!(modal.check_pattern(pattern).unwrap_err(), error);
    }

    #[test]
    fn a_project_env_variable_is_not_forwardable() {
        let mut fixture = local_fixture("", &[(PLAIN_NAME, "repo", EnvSource::ProjectEnvFile)]);
        let modal = &mut fixture.modal;
        let project = row(modal, PLAIN_NAME);
        assert_eq!(project.section, NOT_FORWARDABLE_SECTION);
        assert!(project.is_disabled());
        assert_eq!(project.note.as_deref(), Some(PROJECT_NOTE));
        select(modal, PLAIN_NAME);
        assert!(matches!(
            press(modal, KeyCode::Char(TOGGLE_KEY)),
            EnvAction::Consumed
        ));
    }

    #[test]
    fn a_remote_session_says_the_settings_do_not_apply() {
        let mut modal = EnvModal::new();
        modal.open(EnvTarget::Remote {
            label: REMOTE_LABEL.to_owned(),
        });
        let banner = modal.banner();
        assert!(banner.starts_with(NOT_APPLIED));
        assert!(banner.contains(REMOTE_LABEL));
        assert_eq!(modal.tab(), EnvTab::Local);
    }

    #[test]
    fn withheld_guest_rows_show_no_value() {
        let modal = sandbox_modal(
            true,
            GuestListing {
                selection: ShellEnvironmentSelection::everything(),
                entries: vec![guest(STRIPPED_NAME, None, EnvironmentEntryState::Stripped)],
                truncated: false,
            },
        );
        let stripped = row(&modal, STRIPPED_NAME);
        assert_eq!(stripped.section, STRIPPED_SECTION);
        assert_eq!(stripped.detail(), Some(STRIPPED_DETAIL));
        assert!(!stripped.revealable());
        assert!(stripped.is_disabled());
    }

    #[test]
    fn turning_off_one_name_under_star_confirms_an_explicit_list() {
        let mut modal = sandbox_modal(
            true,
            GuestListing {
                selection: ShellEnvironmentSelection::everything(),
                entries: vec![
                    guest(FIRST_GUEST, Some("1"), EnvironmentEntryState::Forwarded),
                    guest(SECOND_GUEST, Some("2"), EnvironmentEntryState::Forwarded),
                ],
                truncated: false,
            },
        );
        select(&mut modal, FIRST_GUEST);
        assert!(matches!(
            press(&mut modal, KeyCode::Char(TOGGLE_KEY)),
            EnvAction::Consumed
        ));
        assert!(info(&modal).contains(&format!(
            "Switch from * to an explicit list without {FIRST_GUEST}?"
        )));
        match press(&mut modal, KeyCode::Char(CONFIRM_KEY)) {
            EnvAction::Request(EnvRequest::EditSandbox(selection)) => assert_eq!(
                selection,
                ShellEnvironmentSelection {
                    pass: vec![EnvPattern::Exact(SECOND_GUEST.into())],
                    set: BTreeMap::new(),
                }
            ),
            _ => panic!("expected a sandbox edit"),
        }
    }

    #[test]
    fn an_unsupported_sandbox_is_read_only() {
        let mut modal = sandbox_modal(
            false,
            GuestListing {
                selection: ShellEnvironmentSelection::default(),
                entries: Vec::new(),
                truncated: false,
            },
        );
        assert_eq!(modal.banner(), UNSUPPORTED);
        assert!(matches!(
            press(&mut modal, KeyCode::Char(ADD_KEY)),
            EnvAction::Consumed
        ));
        assert!(modal.form.is_none());
        assert!(info(&modal).contains(UNSUPPORTED));
    }
}
