//! The Overview sections: the session's counters against the limits they run
//! against and what keeps it from settling, one script's header, arming,
//! triggers, limits and reach, or what another session's automation is there.

use caudra_automation::catalog::Trust;
use caudra_automation::limits::ROLLING_WINDOW_MS;
use caudra_automation::snapshot::{
    AutomationDetail, AutomationHistoryEntry, AutomationSnapshot, AutomationStatus, Availability,
    SessionControlsView,
};
use ratatui::style::Style;
use ratatui::text::Span;

use super::Body;
use super::list::{last_fired, session_tags, session_title};
use super::text::{
    blocker_text, moment, origin_text, pause_source_text, scope_text, span, trigger_text,
    trust_text,
};
use crate::components::{counted, escape_terminal_controls};
use crate::theme;

const PAUSE_LABEL: &str = "Pause latch: ";
const NOT_PAUSED: &str = "off";
const PAUSED_BY: &str = "paused by ";
const REASON_LABEL: &str = "Reason: ";
const TURNS_LABEL: &str = "Turns this hour: ";
const NEXT_SLOT: &str = "next slot ";
const NO_SLOT: &str = "turns_per_hour is 0, so automations start no turns";
const UNATTENDED_LABEL: &str = "Unattended turns: ";
const NO_CAP: &str = "no cap";
const OF: &str = " of ";
const DELIVERY_BACKOFF_LABEL: &str = "Delivery backoff: ";
const ERRORING_RUN: &str = "erroring run";
const NONE: &str = "none";
const UNTIL: &str = "until ";
const STATUS_LABEL: &str = "Status: ";
const SETTLED: &str = "settled";
const NOT_SETTLED: &str = "not settled";
const BULLET: &str = "\u{2022} ";
const SEPARATOR: &str = " \u{b7} ";
const INVALID_LABEL: &str = "Cannot load: ";
const SCOPE_LABEL: &str = "Scope: ";
const HIDES: &str = "hides the script of the same name in ";
const PATH_LABEL: &str = "Path: ";
const DIGEST_LABEL: &str = "Digest: ";
const TRUST_LABEL: &str = "Trust: ";
const ARMED_LABEL: &str = "Armed: ";
const NOT_ARMED: &str = "not armed in this session";
const STATUS_IDLE: &str = "idle";
const STATUS_RUNNING: &str = "running";
const STATUS_QUEUED: &str = " queued";
const STATUS_DEFERRED: &str = "deferred by a limit until ";
const STATUS_BACKING_OFF: &str = "backing off after a failure until ";
const STATUS_PAUSED: &str = "paused";
const STATUS_FAILED: &str = "its last firing failed";
const TRIGGERS_HEADING: &str = "Triggers";
const TRIGGER_PREFIX: &str = "#";
const NEXT_DUE: &str = "next due ";
const AFTER_UNTIL: &str = "after timer runs out ";
const CONSUMES: &str = "consumes matching messages";
const LIMITS_HEADING: &str = "Limits";
const NO_LIMITS: &str = "No limits declared";
const COOLDOWN_LABEL: &str = "Cooldown: ";
const READY_IN: &str = "ready again ";
const PER_HOUR_LABEL: &str = "Acting firings this hour: ";
const FAILURE_BACKOFF_LABEL: &str = "Failure backoff: ";
const FAILURE: &str = "failure";
const REACH_HEADING: &str = "Capabilities";
const NETWORK_LABEL: &str = "Network: ";
const SECRETS_LABEL: &str = "Secrets: ";
const REPLY_LABEL: &str = "Reply: ";
const SEND_LABEL: &str = "Send: ";
const PUBLISH_LABEL: &str = "Publish: ";
const WORKFLOWS_LABEL: &str = "Workflows: ";
const HOST_FUNCTIONS_LABEL: &str = "Host functions: ";
const YES: &str = "yes";
const NO: &str = "no";
const LIST_SEPARATOR: &str = ", ";
const WARNINGS_HEADING: &str = "Warnings";
const SESSION_LABEL: &str = "Session: ";
const NOT_ARMED_THERE: &str = "not armed in that session";
const NEVER_ARMED_THERE: &str = "never armed in that session";
const ARGS_LABEL: &str = "Args: ";
const LAST_FIRING_LABEL: &str = "Last firing: ";
pub(super) const READ_ONLY_NOTE: &str =
    "Read-only: it belongs to another session, so no key here acts on it";

pub(super) fn session(body: &mut Body, view: &SessionControlsView, now: i64) {
    let t = theme::current();
    match &view.controls.pause {
        Some(latch) => {
            body.field(
                PAUSE_LABEL,
                format!(
                    "{PAUSED_BY}{} {}",
                    pause_source_text(&latch.source),
                    moment(latch.at, now)
                ),
                t.tool_warning,
            );
            if !latch.reason.is_empty() {
                body.field(
                    REASON_LABEL,
                    escape_terminal_controls(&latch.reason),
                    t.tool,
                );
            }
        }
        None => body.field(PAUSE_LABEL, NOT_PAUSED, t.tool),
    }
    let mut turns = format!("{}{OF}{}", view.turns_used(now), view.turns_per_hour);
    let turns_style = match view.next_turn_at(now) {
        Some(i64::MAX) => {
            turns.push_str(&format!("{SEPARATOR}{NO_SLOT}"));
            t.tool_warning
        }
        Some(next) => {
            turns.push_str(&format!("{SEPARATOR}{NEXT_SLOT}{}", moment(next, now)));
            t.tool_warning
        }
        None => t.tool,
    };
    body.field(TURNS_LABEL, turns, turns_style);
    let count = view.controls.unattended.count;
    let (unattended, unattended_style) = match view.max_unattended_turns {
        Some(cap) => (
            format!("{count}{OF}{cap}"),
            match count >= cap {
                true => t.tool_warning,
                false => t.tool,
            },
        ),
        None => (format!("{count}{SEPARATOR}{NO_CAP}"), t.tool),
    };
    body.field(UNATTENDED_LABEL, unattended, unattended_style);
    let backoff = &view.controls.delivery_backoff;
    match backoff.until.filter(|until| *until > now) {
        Some(until) => body.field(
            DELIVERY_BACKOFF_LABEL,
            format!(
                "{}{SEPARATOR}{UNTIL}{}",
                counted(backoff.errors as usize, ERRORING_RUN),
                moment(until, now)
            ),
            t.tool_warning,
        ),
        None => body.field(DELIVERY_BACKOFF_LABEL, NONE, t.tool),
    }
    if view.blockers.is_empty() {
        body.field(STATUS_LABEL, SETTLED, t.tool_success);
        return;
    }
    body.field(STATUS_LABEL, NOT_SETTLED, t.tool_warning);
    for blocker in &view.blockers {
        body.under(vec![Span::styled(
            format!("{BULLET}{}", blocker_text(*blocker)),
            t.tool_dim,
        )]);
    }
}

pub(super) fn automation(body: &mut Body, automation: &AutomationSnapshot, now: i64) {
    let t = theme::current();
    body.line(vec![Span::styled(
        escape_terminal_controls(&automation.name),
        t.bold,
    )]);
    if !automation.description.is_empty() {
        body.text(escape_terminal_controls(&automation.description), t.tool);
    }
    if let Availability::Invalid { reason } = &automation.availability {
        body.field(
            INVALID_LABEL,
            escape_terminal_controls(reason),
            t.tool_error,
        );
    }
    let mut scope = scope_text(automation.scope).to_owned();
    if !automation.shadowed.is_empty() {
        let hidden: Vec<&str> = automation
            .shadowed
            .iter()
            .copied()
            .map(scope_text)
            .collect();
        scope.push_str(&format!(
            "{SEPARATOR}{HIDES}{}",
            hidden.join(LIST_SEPARATOR)
        ));
    }
    body.field(SCOPE_LABEL, scope, t.tool);
    body.field(
        PATH_LABEL,
        escape_terminal_controls(&automation.path.display().to_string()),
        t.tool_path,
    );
    body.field(DIGEST_LABEL, automation.digest.clone(), t.tool_dim);
    let trust_style = match automation.trust {
        Trust::Required => t.tool_warning,
        Trust::Location | Trust::Approved => t.tool,
    };
    body.field(TRUST_LABEL, trust_text(automation.trust), trust_style);
    body.field(
        ARMED_LABEL,
        automation.armed.map_or(NOT_ARMED, origin_text),
        t.tool,
    );
    let (status, status_style) = status_text(automation.status, now);
    body.field(STATUS_LABEL, status, status_style);
    triggers(body, automation, now);
    limits(body, automation, now);
    reach(body, automation);
    if !automation.warnings.is_empty() {
        body.heading(WARNINGS_HEADING);
        for warning in &automation.warnings {
            body.under(vec![Span::styled(
                format!("{BULLET}{}", escape_terminal_controls(warning)),
                t.tool_warning,
            )]);
        }
    }
}

/// Another session's automation, from its detail: the session it belongs to,
/// how that session bound it, and its newest firing. `online` is whether the
/// live peer directory lists the session.
pub(super) fn other(
    body: &mut Body,
    detail: &AutomationDetail,
    session: &AutomationHistoryEntry,
    online: bool,
    now: i64,
) {
    let t = theme::current();
    body.line(vec![Span::styled(
        escape_terminal_controls(&detail.name),
        t.bold,
    )]);
    let mut owner = vec![
        Span::styled(SESSION_LABEL, t.tool_dim),
        Span::styled(session_title(session), t.tool),
    ];
    owner.extend(session_tags(session, online));
    body.line(owner);
    match &detail.binding {
        Some(binding) => {
            body.field(SCOPE_LABEL, scope_text(binding.scope), t.tool);
            body.field(
                ARMED_LABEL,
                match binding.armed {
                    true => origin_text(binding.origin),
                    false => NOT_ARMED_THERE,
                },
                t.tool,
            );
            if binding
                .args
                .as_object()
                .is_some_and(|args| !args.is_empty())
            {
                body.field(
                    ARGS_LABEL,
                    escape_terminal_controls(&binding.args.to_string()),
                    t.tool,
                );
            }
        }
        None => body.field(ARMED_LABEL, NEVER_ARMED_THERE, t.tool),
    }
    let newest = detail.firings.iter().max_by_key(|firing| firing.queued_at);
    body.field(LAST_FIRING_LABEL, last_fired(newest, now), t.tool);
    body.text(READ_ONLY_NOTE, t.tool_dim);
}

fn status_text(status: AutomationStatus, now: i64) -> (String, Style) {
    let t = theme::current();
    match status {
        AutomationStatus::Idle => (STATUS_IDLE.to_owned(), t.tool),
        AutomationStatus::Running => (STATUS_RUNNING.to_owned(), t.todo_in_progress),
        AutomationStatus::Queued { waiting } => {
            (format!("{waiting}{STATUS_QUEUED}"), t.tool_warning)
        }
        AutomationStatus::Deferred { until } => (
            format!("{STATUS_DEFERRED}{}", moment(until, now)),
            t.tool_warning,
        ),
        AutomationStatus::BackingOff { until } => (
            format!("{STATUS_BACKING_OFF}{}", moment(until, now)),
            t.tool_warning,
        ),
        AutomationStatus::Paused => (STATUS_PAUSED.to_owned(), t.tool_warning),
        AutomationStatus::Failed => (STATUS_FAILED.to_owned(), t.tool_error),
    }
}

fn triggers(body: &mut Body, automation: &AutomationSnapshot, now: i64) {
    if automation.triggers.is_empty() {
        return;
    }
    let t = theme::current();
    body.heading(TRIGGERS_HEADING);
    for trigger in &automation.triggers {
        let mut spans = vec![Span::styled(
            format!(
                "{TRIGGER_PREFIX}{} {}",
                trigger.index,
                trigger_text(trigger.kind)
            ),
            t.tool,
        )];
        if let Some(due) = trigger.next_due {
            spans.push(Span::styled(
                format!("{SEPARATOR}{NEXT_DUE}{}", moment(due, now)),
                t.tool_dim,
            ));
        }
        if let Some(until) = trigger.after_until {
            spans.push(Span::styled(
                format!("{SEPARATOR}{AFTER_UNTIL}{}", moment(until, now)),
                t.tool_warning,
            ));
        }
        if trigger.consumes {
            spans.push(Span::styled(format!("{SEPARATOR}{CONSUMES}"), t.accent));
        }
        body.under(spans);
    }
}

fn limits(body: &mut Body, automation: &AutomationSnapshot, now: i64) {
    let t = theme::current();
    let marks = &automation.limiter;
    body.heading(LIMITS_HEADING);
    match &automation.limits {
        Some(limits) => {
            let cooldown_ms = i64::try_from(limits.cooldown.as_millis()).unwrap_or(i64::MAX);
            let mut cooldown = span(cooldown_ms);
            let ready = marks
                .acting
                .last()
                .map(|last| last.saturating_add(cooldown_ms))
                .filter(|ready| *ready > now);
            if let Some(ready) = ready {
                cooldown.push_str(&format!("{SEPARATOR}{READY_IN}{}", moment(ready, now)));
            }
            body.field(COOLDOWN_LABEL, cooldown, t.tool);
            let window_start = now.saturating_sub(ROLLING_WINDOW_MS);
            let used = marks.acting.iter().filter(|at| **at > window_start).count();
            let per_hour_style = match used >= limits.max_per_hour as usize {
                true => t.tool_warning,
                false => t.tool,
            };
            body.field(
                PER_HOUR_LABEL,
                format!("{used}{OF}{}", limits.max_per_hour),
                per_hour_style,
            );
        }
        None => body.text(NO_LIMITS, t.tool_dim),
    }
    match marks.backoff_until.filter(|until| *until > now) {
        Some(until) => body.field(
            FAILURE_BACKOFF_LABEL,
            format!(
                "{}{SEPARATOR}{UNTIL}{}",
                counted(marks.failure_streak as usize, FAILURE),
                moment(until, now)
            ),
            t.tool_warning,
        ),
        None => body.field(FAILURE_BACKOFF_LABEL, NONE, t.tool),
    }
}

fn reach(body: &mut Body, automation: &AutomationSnapshot) {
    let t = theme::current();
    let caps = &automation.capabilities;
    let listed = |items: &[String]| match items.is_empty() {
        true => NONE.to_owned(),
        false => escape_terminal_controls(&items.join(LIST_SEPARATOR)),
    };
    body.heading(REACH_HEADING);
    body.field(NETWORK_LABEL, listed(&caps.network), t.tool);
    body.field(SECRETS_LABEL, listed(&caps.secrets), t.tool);
    body.field(
        REPLY_LABEL,
        match caps.messaging.reply {
            true => YES,
            false => NO,
        },
        t.tool,
    );
    body.field(SEND_LABEL, listed(&caps.messaging.send), t.tool);
    body.field(PUBLISH_LABEL, listed(&caps.messaging.publish), t.tool);
    body.field(WORKFLOWS_LABEL, listed(&caps.workflows), t.tool);
    body.field(
        HOST_FUNCTIONS_LABEL,
        listed(&automation.host_functions),
        t.tool,
    );
}
