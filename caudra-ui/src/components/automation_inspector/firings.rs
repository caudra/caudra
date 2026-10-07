//! The Firings and Outbox sections: firings grouped by where they are in their
//! life, newest first within a group, and the deliveries that wait for a turn.

use caudra_automation::host::DeliveryMode;
use caudra_automation::snapshot::{FiringStatus, FiringSummary, OutboxItem, WaitReason};
use ratatui::style::Style;
use ratatui::text::Span;

use super::list::{DEFERRED_GLYPH, FAILED_GLYPH, PAUSED_GLYPH, QUEUED_GLYPH, RUNNING_GLYPH};
use super::text::{
    EXPIRES_WAIT, action_text, clock, delivery_text, moment, span, trigger_text, wait_text,
};
use super::{Body, Item};
use crate::components::escape_terminal_controls;
use crate::theme;

pub(super) const GROUP_WAITING: &str = "Waiting";
pub(super) const GROUP_RUNNING: &str = "Running";
pub(super) const GROUP_FINISHED: &str = "Finished";
pub(super) const NO_FIRINGS: &str = "No firings yet";
pub(super) const EMPTY_OUTBOX: &str = "No delivery waits in the outbox";
const COMPLETED_GLYPH: &str = "\u{2713}";
const SKIPPED_GLYPH: &str = "\u{b7}";
const RELEASED_GLYPH: &str = "\u{21a9}";
const LIMITED_GLYPH: &str = "\u{2298}";
const CANCELLED_GLYPH: &str = "\u{2297}";
const DROPPED_GLYPH: &str = "\u{2212}";
const INTERRUPTED_GLYPH: &str = "\u{21af}";
pub(super) const CONSUMED_BADGE: &str = " consumed";
pub(super) const REPEATS_PREFIX: &str = " \u{d7}";
pub(super) const TRIGGER_INDEX: &str = " #";
const WAITING_FOR: &str = "waiting ";
const UNTIL: &str = "until ";
const NO_ACTIONS: &str = "no actions";
const MORE_ACTIONS: &str = " +";
const ERROR_SEPARATOR: &str = ": ";
const GAP: &str = " ";
pub(super) const READY: &str = "ready to deliver";
pub(super) const STARTS_ONCE_CLOSED: &str = "it starts a turn once this window closes";
pub(super) const JOINS_RUNNING_TURN: &str =
    "it joins the running turn before its next model request";
const WAITS: &str = "waits: ";
const SEPARATOR: &str = " \u{b7} ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Waiting,
    Running,
    Finished,
}

impl Group {
    const ALL: [Self; 3] = [Self::Waiting, Self::Running, Self::Finished];

    fn of(status: FiringStatus) -> Self {
        match status {
            FiringStatus::Queued | FiringStatus::Deferred => Self::Waiting,
            FiringStatus::Running => Self::Running,
            FiringStatus::Completed
            | FiringStatus::Skipped
            | FiringStatus::Released
            | FiringStatus::Failed
            | FiringStatus::RateLimited
            | FiringStatus::Cancelled
            | FiringStatus::Paused
            | FiringStatus::Dropped
            | FiringStatus::Interrupted => Self::Finished,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Waiting => GROUP_WAITING,
            Self::Running => GROUP_RUNNING,
            Self::Finished => GROUP_FINISHED,
        }
    }
}

/// Queued or deferred: the firings a human may still drop.
pub(super) fn is_waiting(status: FiringStatus) -> bool {
    Group::of(status) == Group::Waiting
}

pub(super) fn glyph(status: FiringStatus) -> (&'static str, Style) {
    let t = theme::current();
    match status {
        FiringStatus::Queued => (QUEUED_GLYPH, t.tool_warning),
        FiringStatus::Deferred => (DEFERRED_GLYPH, t.tool_warning),
        FiringStatus::Running => (RUNNING_GLYPH, t.todo_in_progress),
        FiringStatus::Completed => (COMPLETED_GLYPH, t.tool_success),
        FiringStatus::Skipped => (SKIPPED_GLYPH, t.tool_dim),
        FiringStatus::Released => (RELEASED_GLYPH, t.tool_dim),
        FiringStatus::Failed => (FAILED_GLYPH, t.tool_error),
        FiringStatus::RateLimited => (LIMITED_GLYPH, t.tool_warning),
        FiringStatus::Cancelled => (CANCELLED_GLYPH, t.tool_dim),
        FiringStatus::Paused => (PAUSED_GLYPH, t.tool_warning),
        FiringStatus::Dropped => (DROPPED_GLYPH, t.tool_dim),
        FiringStatus::Interrupted => (INTERRUPTED_GLYPH, t.tool_warning),
    }
}

/// One line on what a firing did: its error, else why it skipped, released
/// or was dropped, else its first action.
pub(super) fn summary(firing: &FiringSummary) -> String {
    if let Some(error) = &firing.error {
        return format!("{}{ERROR_SEPARATOR}{}", error.kind, error.message);
    }
    if let Some(reason) = &firing.reason {
        return reason.clone();
    }
    match firing.first_action {
        Some(kind) if firing.action_count > 1 => format!(
            "{}{MORE_ACTIONS}{}",
            action_text(kind),
            firing.action_count - 1
        ),
        Some(kind) => action_text(kind).to_owned(),
        None => NO_ACTIONS.to_owned(),
    }
}

/// How long it has waited, until when a limit defers it, or how long it ran.
pub(super) fn timing(firing: &FiringSummary, now: i64) -> Option<String> {
    match firing.status {
        FiringStatus::Queued => Some(format!(
            "{WAITING_FOR}{}",
            span(now.saturating_sub(firing.queued_at))
        )),
        FiringStatus::Deferred => firing
            .deferred_until
            .map(|until| format!("{UNTIL}{}", moment(until, now))),
        FiringStatus::Running => firing
            .started_at
            .map(|started| span(now.saturating_sub(started))),
        _ => firing
            .started_at
            .zip(firing.finished_at)
            .map(|(started, finished)| span(finished.saturating_sub(started))),
    }
}

/// `firings`, newest first, under the group each belongs to. `merged` names
/// each row's automation, for the session's list of every automation.
pub(super) fn firings(body: &mut Body, firings: &[&FiringSummary], merged: bool, now: i64) {
    if firings.is_empty() {
        body.text(NO_FIRINGS, theme::current().tool_dim);
        return;
    }
    for group in Group::ALL {
        let rows: Vec<&FiringSummary> = firings
            .iter()
            .copied()
            .filter(|firing| Group::of(firing.status) == group)
            .collect();
        if rows.is_empty() {
            continue;
        }
        body.heading(format!("{} ({})", group.label(), rows.len()));
        for firing in rows {
            body.item(
                Item::Firing(firing.fire_id.clone()),
                row(firing, merged, timing(firing, now)),
            );
        }
    }
}

/// `timing` is how long the firing took or waits, which a dry run, its
/// times pinned to the firing it replays, has none of.
pub(super) fn row(
    firing: &FiringSummary,
    merged: bool,
    timing: Option<String>,
) -> Vec<Span<'static>> {
    let t = theme::current();
    let (glyph, glyph_style) = glyph(firing.status);
    let mut spans = vec![
        Span::styled(
            clock(firing.started_at.unwrap_or(firing.queued_at)),
            t.tool_dim,
        ),
        Span::raw(GAP),
    ];
    if merged {
        spans.push(Span::styled(
            escape_terminal_controls(&firing.automation),
            t.tool_path,
        ));
        spans.push(Span::raw(GAP));
    }
    spans.push(Span::styled(
        format!(
            "{}{TRIGGER_INDEX}{}",
            trigger_text(firing.trigger),
            firing.trigger_index
        ),
        t.tool,
    ));
    spans.push(Span::raw(GAP));
    spans.push(Span::styled(glyph, glyph_style));
    if let Some(timing) = timing {
        spans.push(Span::styled(format!("{GAP}{timing}"), t.tool_dim));
    }
    if firing.consumed {
        spans.push(Span::styled(CONSUMED_BADGE, t.accent));
    }
    if firing.repeats > 1 {
        spans.push(Span::styled(
            format!("{REPEATS_PREFIX}{}", firing.repeats),
            t.tool_dim,
        ));
    }
    let summary_style = match firing.error {
        Some(_) => t.tool_error,
        None => t.tool_dim,
    };
    spans.push(Span::styled(
        format!("{SEPARATOR}{}", escape_terminal_controls(&summary(firing))),
        summary_style,
    ));
    spans
}

/// The session's outbox, oldest first, each delivery with what holds it.
/// `settled` is whether nothing keeps the session from settling.
pub(super) fn outbox(body: &mut Body, items: &[OutboxItem], settled: bool, now: i64) {
    let t = theme::current();
    if items.is_empty() {
        body.text(EMPTY_OUTBOX, t.tool_dim);
        return;
    }
    for item in items {
        body.item(
            Item::Outbox {
                fire_id: item.fire_id.clone(),
                seq: item.seq,
            },
            vec![
                Span::styled(clock(item.queued_at), t.tool_dim),
                Span::raw(GAP),
                Span::styled(escape_terminal_controls(&item.automation), t.tool_path),
                Span::raw(GAP),
                Span::styled(
                    format!(
                        "{} ({})",
                        action_text(item.kind),
                        delivery_text(item.delivery)
                    ),
                    t.tool,
                ),
                Span::styled(
                    format!("{SEPARATOR}{}", escape_terminal_controls(&item.summary)),
                    t.tool_dim,
                ),
            ],
        );
        let mut wait = match item.wait {
            Some(reason) => format!("{WAITS}{}", wait_text(reason, now)),
            None => unheld(item.delivery, settled).to_owned(),
        };
        if let Some(at) = item.expires_at
            && !matches!(item.wait, Some(WaitReason::ExpiresAt { .. }))
        {
            wait.push_str(&format!("{SEPARATOR}{EXPIRES_WAIT}{}", moment(at, now)));
        }
        body.under(vec![Span::styled(wait, t.tool_warning)]);
    }
}

/// What a delivery nothing holds does next. This window holds a settled
/// session's turn until it closes; a `guide` item joins a busy one's.
fn unheld(delivery: DeliveryMode, settled: bool) -> &'static str {
    match (settled, delivery) {
        (true, _) => STARTS_ONCE_CLOSED,
        (false, DeliveryMode::Guide) => JOINS_RUNNING_TURN,
        (false, DeliveryMode::Next) => READY,
    }
}
