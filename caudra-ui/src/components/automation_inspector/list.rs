//! The left pane: one row for the session, then this session's catalog grouped
//! by what each script may do here, then other sessions' automations, each
//! session's under a row of its own. Groups are ordered by their enum.

use caudra_agent::peers::handle_address;
use caudra_automation::request::AutomationRequest;
use caudra_automation::snapshot::{
    AutomationHistoryEntry, AutomationSnapshot, AutomationState, AutomationStatus, Availability,
    FiringStatus, FiringSummary,
};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::text::{relative, scope_text};
use crate::components::command_text::ellipsize_spans;
use crate::components::escape_terminal_controls;
use crate::theme;

const GROUP_SESSION: &str = "Session";
const GROUP_ARMED: &str = "Armed";
const GROUP_AVAILABLE: &str = "Available";
const GROUP_NEEDS_TRUST: &str = "Needs trust";
const GROUP_INVALID: &str = "Invalid";
const GROUP_OTHER_SESSIONS: &str = "Other sessions";
pub(super) const SESSION_ROW: &str = "This session";
pub(super) const ONLINE: &str = "online";
const PAUSED_STATE: &str = "paused";
const BUSY_STATE: &str = "busy";
const SETTLED_STATE: &str = "settled";
const QUEUED_UNIT: &str = " queued";
const NEVER_FIRED: &str = "never fired";
const CANNOT_LOAD: &str = "cannot load";
const SEPARATOR: &str = " \u{b7} ";
const GLYPH_GAP: &str = " ";
/// What a row under another session's row starts with, so it reads as that
/// session's.
const NESTED: &str = "  ";
pub(super) const IDLE_GLYPH: &str = "\u{25cb}";
pub(super) const RUNNING_GLYPH: &str = "\u{25b8}";
pub(super) const QUEUED_GLYPH: &str = "\u{25f7}";
pub(super) const DEFERRED_GLYPH: &str = "\u{25d4}";
pub(super) const BACKING_OFF_GLYPH: &str = "\u{21bb}";
pub(super) const PAUSED_GLYPH: &str = "\u{2016}";
pub(super) const FAILED_GLYPH: &str = "\u{2717}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Group {
    Session,
    Armed,
    Available,
    NeedsTrust,
    Invalid,
    OtherSessions,
}

impl Group {
    fn of(availability: &Availability) -> Self {
        match availability {
            Availability::Armed => Self::Armed,
            Availability::Available => Self::Available,
            Availability::NeedsTrust => Self::NeedsTrust,
            Availability::Invalid { .. } => Self::Invalid,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Session => GROUP_SESSION,
            Self::Armed => GROUP_ARMED,
            Self::Available => GROUP_AVAILABLE,
            Self::NeedsTrust => GROUP_NEEDS_TRUST,
            Self::Invalid => GROUP_INVALID,
            Self::OtherSessions => GROUP_OTHER_SESSIONS,
        }
    }
}

/// What the right pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Selection {
    Session,
    Automation(String),
    /// Another session's automation, which the inspector only reads. Its
    /// session is part of its name: this session may have one of the same name.
    Other {
        session_id: String,
        name: String,
    },
}

impl Selection {
    /// The question that reads the selected automation's binding, state and
    /// firings, in the session it belongs to.
    pub(super) fn inspect(&self) -> Option<AutomationRequest> {
        match self {
            Self::Session => None,
            Self::Automation(name) => Some(AutomationRequest::Inspect {
                name: name.clone(),
                session_id: None,
            }),
            Self::Other { session_id, name } => Some(AutomationRequest::Inspect {
                name: name.clone(),
                session_id: Some(session_id.clone()),
            }),
        }
    }

    /// Whether an `Inspect` of `name` in `session_id`, or in this session when
    /// that is `None`, asked about this selection.
    pub(super) fn inspected_by(&self, name: &str, session_id: Option<&str>) -> bool {
        match (self, session_id) {
            (Self::Automation(selected), None) => selected == name,
            (
                Self::Other {
                    session_id: selected_session,
                    name: selected,
                },
                Some(session_id),
            ) => selected_session == session_id && selected == name,
            (Self::Session | Self::Automation(_) | Self::Other { .. }, _) => false,
        }
    }
}

/// One row of the list the selection can land on, in the order the list
/// draws them.
pub(super) enum Entry<'a> {
    Session,
    Automation(&'a AutomationSnapshot),
    /// Another session's automation, by the name its bindings or firings give.
    Other {
        session: &'a AutomationHistoryEntry,
        name: &'a str,
    },
}

impl Entry<'_> {
    pub(super) fn group(&self) -> Group {
        match self {
            Self::Session => Group::Session,
            Self::Automation(automation) => Group::of(&automation.availability),
            Self::Other { .. } => Group::OtherSessions,
        }
    }

    pub(super) fn selection(&self) -> Selection {
        match self {
            Self::Session => Selection::Session,
            Self::Automation(automation) => Selection::Automation(automation.name.clone()),
            Self::Other { session, name } => Selection::Other {
                session_id: session.session_id.clone(),
                name: (*name).to_owned(),
            },
        }
    }
}

/// The session first, whatever the filter, then every script whose name or
/// description holds `filter`, grouped and in catalog order within a group,
/// then each other session's automations, all of them when the session's
/// title or `@name` holds it, else those whose name does.
pub(super) fn entries<'a>(
    state: &'a AutomationState,
    sessions: &'a [AutomationHistoryEntry],
    filter: &str,
) -> Vec<Entry<'a>> {
    let needle = filter.to_lowercase();
    let holds = |text: &str| needle.is_empty() || text.to_lowercase().contains(&needle);
    let mut automations: Vec<Entry<'a>> = state
        .automations
        .iter()
        .filter(|automation| holds(&automation.name) || holds(&automation.description))
        .map(Entry::Automation)
        .collect();
    automations.sort_by_key(Entry::group);
    let mut entries = vec![Entry::Session];
    entries.extend(automations);
    for session in sessions {
        let named = holds(&session.title) || holds(&handle_address(&session.handle));
        entries.extend(
            names(session)
                .into_iter()
                .filter(|name| named || holds(name))
                .map(|name| Entry::Other { session, name }),
        );
    }
    entries
}

/// The automations another session lists: those it bound, then any its
/// firings name that no binding does.
fn names(session: &AutomationHistoryEntry) -> Vec<&str> {
    let mut names: Vec<&str> = session
        .bindings
        .iter()
        .map(|binding| binding.name.as_str())
        .collect();
    for firing in &session.firings {
        if !names.contains(&firing.automation.as_str()) {
            names.push(&firing.automation);
        }
    }
    names
}

/// The glyph an automation row leads with, and its colour. A queue says how
/// long it is.
pub(super) fn status_glyph(status: AutomationStatus) -> (String, Style) {
    let t = theme::current();
    match status {
        AutomationStatus::Idle => (IDLE_GLYPH.to_owned(), t.tool_dim),
        AutomationStatus::Running => (RUNNING_GLYPH.to_owned(), t.todo_in_progress),
        AutomationStatus::Queued { waiting } => {
            (format!("{QUEUED_GLYPH}{waiting}"), t.tool_warning)
        }
        AutomationStatus::Deferred { .. } => (DEFERRED_GLYPH.to_owned(), t.tool_warning),
        AutomationStatus::BackingOff { .. } => (BACKING_OFF_GLYPH.to_owned(), t.tool_warning),
        AutomationStatus::Paused => (PAUSED_GLYPH.to_owned(), t.tool_warning),
        AutomationStatus::Failed => (FAILED_GLYPH.to_owned(), t.tool_error),
    }
}

/// Where another session's automation stands, as far as its listed firings
/// tell, read in the order its own runtime reads its queue: running, deferred,
/// queued, else failed when its newest finished firing failed.
pub(super) fn listed_status(firings: &[&FiringSummary]) -> AutomationStatus {
    let with = |status: FiringStatus| firings.iter().filter(move |firing| firing.status == status);
    if with(FiringStatus::Running).next().is_some() {
        AutomationStatus::Running
    } else if let Some(until) =
        with(FiringStatus::Deferred).find_map(|firing| firing.deferred_until)
    {
        AutomationStatus::Deferred { until }
    } else if let waiting @ 1.. = with(FiringStatus::Queued).count() {
        AutomationStatus::Queued {
            waiting: u32::try_from(waiting).unwrap_or(u32::MAX),
        }
    } else if firings
        .iter()
        .find(|firing| !firing.status.is_pending())
        .is_some_and(|firing| firing.status == FiringStatus::Failed)
    {
        AutomationStatus::Failed
    } else {
        AutomationStatus::Idle
    }
}

/// The session's glyph and the word for where it stands: paused, busy with
/// something that keeps it from settling, or settled.
pub(super) fn session_state(state: &AutomationState) -> (&'static str, &'static str, Style) {
    let t = theme::current();
    if state.session.controls.pause.is_some() {
        (PAUSED_GLYPH, PAUSED_STATE, t.tool_warning)
    } else if !state.session.blockers.is_empty() {
        (RUNNING_GLYPH, BUSY_STATE, t.todo_in_progress)
    } else {
        (IDLE_GLYPH, SETTLED_STATE, t.tool_dim)
    }
}

/// When `firing` ended, or was queued while it has not, and how.
pub(super) fn last_fired(firing: Option<&FiringSummary>, now: i64) -> String {
    match firing {
        Some(firing) => format!(
            "{} {}",
            relative(firing.finished_at.unwrap_or(firing.queued_at), now),
            firing.status
        ),
        None => NEVER_FIRED.to_owned(),
    }
}

pub(super) fn row(
    entry: &Entry<'_>,
    state: &AutomationState,
    style: Style,
    now: i64,
) -> Line<'static> {
    match entry {
        Entry::Session => this_session_row(state, style),
        Entry::Automation(automation) => automation_row(automation, style, now),
        Entry::Other { session, name } => other_row(session, name, style, now),
    }
}

fn this_session_row(state: &AutomationState, style: Style) -> Line<'static> {
    let (glyph, word, glyph_style) = session_state(state);
    let mut detail = format!("{SEPARATOR}{word}");
    if !state.outbox.is_empty() {
        detail.push_str(&format!("{SEPARATOR}{}{QUEUED_UNIT}", state.outbox.len()));
    }
    Line::from(vec![
        Span::styled(glyph, glyph_style),
        Span::raw(GLYPH_GAP),
        Span::styled(SESSION_ROW, style),
        Span::styled(detail, theme::current().tool_dim),
    ])
}

fn automation_row(automation: &AutomationSnapshot, style: Style, now: i64) -> Line<'static> {
    let (glyph, glyph_style) = status_glyph(automation.status);
    let last = match &automation.availability {
        Availability::Invalid { .. } => CANNOT_LOAD.to_owned(),
        _ => last_fired(automation.last_firing.as_ref(), now),
    };
    Line::from(vec![
        Span::styled(glyph, glyph_style),
        Span::raw(GLYPH_GAP),
        Span::styled(escape_terminal_controls(&automation.name), style),
        Span::styled(
            format!(
                "{SEPARATOR}{}{SEPARATOR}{last}",
                scope_text(automation.scope)
            ),
            theme::current().tool_dim,
        ),
    ])
}

/// Another session's automation as its entry lists it: where its firings leave
/// it, the scope that session bound it in, and its newest firing.
fn other_row(
    session: &AutomationHistoryEntry,
    name: &str,
    style: Style,
    now: i64,
) -> Line<'static> {
    let firings: Vec<&FiringSummary> = session
        .firings
        .iter()
        .filter(|firing| firing.automation == name)
        .collect();
    let (glyph, glyph_style) = status_glyph(listed_status(&firings));
    let mut detail = String::new();
    if let Some(binding) = session.bindings.iter().find(|binding| binding.name == name) {
        detail.push_str(&format!("{SEPARATOR}{}", scope_text(binding.scope)));
    }
    detail.push_str(&format!(
        "{SEPARATOR}{}",
        last_fired(firings.first().copied(), now)
    ));
    Line::from(vec![
        Span::raw(NESTED),
        Span::styled(glyph, glyph_style),
        Span::raw(GLYPH_GAP),
        Span::styled(escape_terminal_controls(name), style),
        Span::styled(detail, theme::current().tool_dim),
    ])
}

/// The row another session's automations sit under: its title, its `@name`,
/// whether the live peer directory lists it, and its last activity. The title
/// gives way first when `width` is short.
pub(super) fn session_row(
    session: &AutomationHistoryEntry,
    online: bool,
    now: i64,
    width: u16,
) -> Line<'static> {
    let mut tail = session_tags(session, online);
    tail.push(Span::styled(
        format!("{SEPARATOR}{}", relative(session.last_activity_at, now)),
        theme::current().tool_dim,
    ));
    let room = usize::from(width).saturating_sub(tail.iter().map(Span::width).sum());
    let mut spans = ellipsize_spans(
        vec![Span::styled(session_title(session), theme::current().bold)],
        room,
    );
    spans.extend(tail);
    Line::from(spans)
}

/// A session's title, or its id when it has none.
pub(super) fn session_title(session: &AutomationHistoryEntry) -> String {
    match session.title.is_empty() {
        true => session.session_id.clone(),
        false => escape_terminal_controls(&session.title),
    }
}

/// What follows a session's title: its `@name`, and the online mark when the
/// live peer directory lists it.
pub(super) fn session_tags(session: &AutomationHistoryEntry, online: bool) -> Vec<Span<'static>> {
    let t = theme::current();
    let mut tags = vec![Span::styled(
        format!(
            "{GLYPH_GAP}{}",
            escape_terminal_controls(&handle_address(&session.handle))
        ),
        t.accent,
    )];
    if online {
        tags.push(Span::styled(format!("{SEPARATOR}{ONLINE}"), t.tool_success));
    }
    tags
}
