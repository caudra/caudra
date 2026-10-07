//! The State and Args sections, and the editors behind `e`: JSON in the
//! workbench's own text field, checked the way the runtime checks it before
//! the save goes out, so a refusal reads here rather than as a failed request.

use std::collections::{HashMap, HashSet};

use caudra_automation::args::{self, ArgDecl, ArgType};
use caudra_automation::request::AutomationRequest;
use caudra_automation::snapshot::{AutomationDetail, AutomationSnapshot, StateView};
use caudra_automation::state::parse_state;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use serde_json::{Map, Value};

use super::text::moment;
use super::{Body, FoldScope, Loading};
use crate::components::text_editor::{EditorKey, EditorMouse, TextEditor};
use crate::components::{escape_terminal_controls, visual_rows};
use crate::theme;

const STATE_TITLE: &str = "State of ";
const ARGS_TITLE: &str = "Args of ";
const AT_REVISION: &str = " at revision ";
pub(super) const ARGS_NOT_JSON: &str = "args are not valid JSON: ";
pub(super) const CONFLICT_PREFIX: &str = "Revision ";
pub(super) const CONFLICT_SUFFIX: &str = " landed while you edited, so the save was refused. The \
     editor now holds it: edit it and save again";
pub(super) const NO_BINDING: &str =
    "Never armed in this session, so it has no state yet: arming it starts one";
const NOT_BOUND_THERE: &str = "Never armed in that session, so it has no state";
const NO_STATE: &str = "No state yet";
const REVISION_LABEL: &str = "Revision: ";
const WRITER_LABEL: &str = "Written by: ";
const FIRING_WRITER: &str = "firing ";
const HUMAN_WRITER: &str = "a human edit or a clear";
const VERSION_LABEL: &str = "Script version: ";
pub(super) const STATE_HINT: &str = "e edits the state as JSON, c clears it";
const NO_ARGS: &str = "Declares no args";
const ARGS_HINT: &str = "e edits the args as JSON; saving arms it again";
const TAKES: &str = "Takes ";
const TYPE_SEPARATOR: &str = ": ";
const REQUIRED: &str = "required";
const DEFAULT_PREFIX: &str = "default ";
const CURRENT_LABEL: &str = "Current: ";
const DEFAULTED: &str = "the default";
const UNSET: &str = "unset";
const DETAIL_SEPARATOR: &str = ", ";
const SEPARATOR: &str = " \u{b7} ";
const EMPTY_OBJECT: &str = "{}";

/// What the text is, and what a save of it sends.
enum Kind {
    /// Saved only at the revision it was loaded from.
    State {
        expected_revision: u64,
    },
    Args {
        declared: Vec<ArgDecl>,
    },
}

pub(super) struct Editor {
    pub(super) name: String,
    kind: Kind,
    field: TextEditor,
    error: Option<String>,
    /// The revision a save lost to, and whether the state it names is still
    /// to replace the text.
    conflict: Option<u64>,
    reload_pending: bool,
}

impl Editor {
    pub(super) fn state(name: String, state: Option<&StateView>) -> Self {
        let mut editor = Self::new(
            name,
            Kind::State {
                expected_revision: 0,
            },
        );
        editor.load_state(state);
        editor
    }

    /// The args as they stand, or every required arg left `null` for the
    /// reader to fill in, since a default needs no mention.
    pub(super) fn args(automation: &AutomationSnapshot) -> Self {
        let args = match &automation.args {
            Some(args) if !args.is_null() => args.clone(),
            _ => Value::Object(
                automation
                    .declared_args
                    .iter()
                    .filter(|decl| decl.spec.default.is_none())
                    .map(|decl| (decl.name.clone(), Value::Null))
                    .collect::<Map<String, Value>>(),
            ),
        };
        let mut editor = Self::new(
            automation.name.clone(),
            Kind::Args {
                declared: automation.declared_args.clone(),
            },
        );
        editor.field.set_text(pretty(&args));
        editor
    }

    fn new(name: String, kind: Kind) -> Self {
        Self {
            name,
            kind,
            field: TextEditor::new(),
            error: None,
            conflict: None,
            reload_pending: false,
        }
    }

    fn load_state(&mut self, state: Option<&StateView>) {
        let (text, revision) = state.map_or_else(
            || (EMPTY_OBJECT.to_owned(), 0),
            |state| (pretty(&state.value), state.revision),
        );
        self.field.set_text(text);
        self.kind = Kind::State {
            expected_revision: revision,
        };
    }

    pub(super) fn is_state(&self) -> bool {
        matches!(self.kind, Kind::State { .. })
    }

    #[cfg(test)]
    pub(super) fn expected_revision(&self) -> Option<u64> {
        match self.kind {
            Kind::State { expected_revision } => Some(expected_revision),
            Kind::Args { .. } => None,
        }
    }

    #[cfg(test)]
    pub(super) fn text(&self) -> String {
        self.field.text()
    }

    #[cfg(test)]
    pub(super) fn set_text(&mut self, text: &str) {
        self.field.set_text(text.to_owned());
    }

    #[cfg(test)]
    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The request that saves the text, or `None` with the reason it cannot
    /// be saved shown under it.
    pub(super) fn save(&mut self) -> Option<AutomationRequest> {
        let text = self.field.text();
        let checked = match &self.kind {
            Kind::State { expected_revision } => parse_state(&text)
                .map(|state| AutomationRequest::SetState {
                    name: self.name.clone(),
                    state,
                    expected_revision: *expected_revision,
                })
                .map_err(|error| error.to_string()),
            Kind::Args { declared } => serde_json::from_str::<Value>(&text)
                .map_err(|error| format!("{ARGS_NOT_JSON}{error}"))
                .and_then(|given| {
                    args::resolve(declared, &given)
                        .map(|_| AutomationRequest::SetArgs {
                            name: self.name.clone(),
                            args: given,
                        })
                        .map_err(|error| error.to_string())
                }),
        };
        match checked {
            Ok(request) => {
                self.error = None;
                Some(request)
            }
            Err(error) => {
                self.error = Some(error);
                None
            }
        }
    }

    /// The runtime refused the save, for a reason the local checks could not
    /// see.
    pub(super) fn refused(&mut self, error: String) {
        self.error = Some(error);
    }

    /// A firing or another edit committed `current` first. The next save
    /// expects it, and the state it holds replaces the text once it arrives.
    pub(super) fn conflicted(&mut self, current: u64) {
        self.conflict = Some(current);
        self.reload_pending = true;
        self.error = None;
        if let Kind::State { expected_revision } = &mut self.kind {
            *expected_revision = current;
        }
    }

    /// The reloaded state after a conflict. An answer from before the
    /// conflict, or any after the first, leaves the text alone, so a refresh
    /// cannot wipe what the reader typed since.
    pub(super) fn reload(&mut self, state: Option<&StateView>) {
        let revision = state.map_or(0, |state| state.revision);
        if self.reload_pending && self.conflict.is_some_and(|current| revision >= current) {
            self.reload_pending = false;
            self.load_state(state);
        }
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> EditorKey {
        self.field.handle_key(key)
    }

    pub(super) fn handle_paste(&mut self, text: &str) {
        self.field.handle_paste(text);
    }

    pub(super) fn handle_mouse(&mut self, event: &MouseEvent) -> EditorMouse {
        self.field.handle_mouse(event)
    }

    pub(super) fn scroll(&mut self, delta: i32) {
        self.field.scroll(delta);
    }

    /// The title and, for args, what each one takes; the text; then why the
    /// last save did not go.
    pub(super) fn render(&mut self, frame: &mut Frame, area: Rect) {
        let t = theme::current();
        let (title, revision) = match &self.kind {
            Kind::State { expected_revision } => {
                (STATE_TITLE, format!("{AT_REVISION}{expected_revision}"))
            }
            Kind::Args { .. } => (ARGS_TITLE, String::new()),
        };
        let mut header = vec![Line::from(vec![
            Span::styled(title, t.tool_dim),
            Span::styled(escape_terminal_controls(&self.name), t.bold),
            Span::styled(revision, t.tool_dim),
        ])];
        if let Kind::Args { declared } = &self.kind {
            header.push(Line::styled(declared_summary(declared), t.tool_dim));
        }
        let mut notes = Vec::new();
        if let Some(current) = self.conflict {
            notes.push(Line::styled(
                format!("{CONFLICT_PREFIX}{current}{CONFLICT_SUFFIX}"),
                t.tool_warning,
            ));
        }
        if let Some(error) = &self.error {
            notes.push(Line::styled(escape_terminal_controls(error), t.tool_error));
        }
        let [head, text, foot] = Layout::vertical([
            Constraint::Length(visual_rows(&header, area.width).total),
            Constraint::Fill(1),
            Constraint::Length(match notes.is_empty() {
                true => 0,
                false => visual_rows(&notes, area.width).total,
            }),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(header).wrap(Wrap { trim: false }), head);
        self.field.view_json(frame, text);
        frame.render_widget(Paragraph::new(notes).wrap(Wrap { trim: false }), foot);
    }
}

/// The State section: the committed JSON as a tree, with the firing that
/// wrote it, the script version it ran, and when. Only an `editable` state,
/// this session's, says how to edit it.
pub(super) fn state_section(
    body: &mut Body,
    detail: Option<&Loading<Box<AutomationDetail>>>,
    folded: &HashMap<FoldScope, HashSet<usize>>,
    now: i64,
    editable: bool,
) {
    let t = theme::current();
    let Some(detail) = body.landed(detail) else {
        return;
    };
    if detail.binding.is_none() {
        let unbound = match editable {
            true => NO_BINDING,
            false => NOT_BOUND_THERE,
        };
        body.text(unbound, t.tool_dim);
        return;
    }
    match &detail.state {
        Some(state) => committed(body, state, folded, now),
        None => body.text(NO_STATE, t.tool_dim),
    }
    if editable {
        body.text(STATE_HINT, t.tool_dim);
    }
}

fn committed(
    body: &mut Body,
    state: &StateView,
    folded: &HashMap<FoldScope, HashSet<usize>>,
    now: i64,
) {
    let t = theme::current();
    body.field(REVISION_LABEL, state.revision.to_string(), t.tool);
    let mut writer = state.writer.as_deref().map_or_else(
        || HUMAN_WRITER.to_owned(),
        |fire_id| format!("{FIRING_WRITER}{}", escape_terminal_controls(fire_id)),
    );
    if let Some(at) = state.written_at {
        writer.push_str(&format!("{SEPARATOR}{}", moment(at, now)));
    }
    body.field(WRITER_LABEL, writer, t.tool);
    if let Some(digest) = &state.digest {
        body.field(VERSION_LABEL, digest.clone(), t.tool_dim);
    }
    body.tree(
        FoldScope::State,
        &state.value,
        folded.get(&FoldScope::State),
    );
}

/// The Args section: each declared arg with its type, default, description
/// and the value this session armed it with.
pub(super) fn args_section(body: &mut Body, automation: &AutomationSnapshot) {
    let t = theme::current();
    if automation.declared_args.is_empty() {
        body.text(NO_ARGS, t.tool_dim);
        return;
    }
    let given = automation.args.as_ref().and_then(Value::as_object);
    for decl in &automation.declared_args {
        body.line(vec![
            Span::styled(escape_terminal_controls(&decl.name), t.accent),
            Span::styled(format!("{TYPE_SEPARATOR}{}", spec_text(decl)), t.tool_dim),
        ]);
        if let Some(description) = &decl.spec.description {
            body.under(vec![Span::styled(
                escape_terminal_controls(description),
                t.tool,
            )]);
        }
        let current = match given.and_then(|given| given.get(&decl.name)) {
            Some(value) if !value.is_null() => escape_terminal_controls(&value.to_string()),
            _ if decl.spec.default.is_some() => DEFAULTED.to_owned(),
            _ => UNSET.to_owned(),
        };
        body.under(vec![
            Span::styled(CURRENT_LABEL, t.tool_dim),
            Span::styled(current, t.tool),
        ]);
    }
    body.text(ARGS_HINT, t.tool_dim);
}

/// `interval: int, default 5 · topic: string, required`.
fn declared_summary(declared: &[ArgDecl]) -> String {
    if declared.is_empty() {
        return NO_ARGS.to_owned();
    }
    let args: Vec<String> = declared
        .iter()
        .map(|decl| {
            format!(
                "{}{TYPE_SEPARATOR}{}",
                escape_terminal_controls(&decl.name),
                spec_text(decl)
            )
        })
        .collect();
    format!("{TAKES}{}", args.join(SEPARATOR))
}

fn spec_text(decl: &ArgDecl) -> String {
    let requirement = match &decl.spec.default {
        Some(default) => format!(
            "{DEFAULT_PREFIX}{}",
            escape_terminal_controls(&default.to_string())
        ),
        None => REQUIRED.to_owned(),
    };
    format!(
        "{}{DETAIL_SEPARATOR}{requirement}",
        type_text(decl.spec.kind)
    )
}

fn type_text(kind: ArgType) -> &'static str {
    match kind {
        ArgType::String => "string",
        ArgType::Int => "int",
        ArgType::Float => "float",
        ArgType::Bool => "bool",
        ArgType::List => "list",
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| EMPTY_OBJECT.to_owned())
}
