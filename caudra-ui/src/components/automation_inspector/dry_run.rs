//! A dry run `r` asked for: a finished firing's event run again against the
//! script as it is now, which performed nothing and is stored nowhere. The
//! inspector holds it while the selection stays, as the top row of Firings,
//! and opens it as a trace that says how each of its actions was answered.

use caudra_automation::replay::Answer;
use caudra_automation::request::AutomationError;
use caudra_automation::snapshot::{DryRunDetail, FiringSummary};
use ratatui::style::Style;
use ratatui::text::Span;

use super::text::{limit_text, moment};
use super::{Body, Item, LOADING, Loading, firings};
use crate::components::escape_terminal_controls;
use crate::theme;

pub(super) const BADGE: &str = "dry run";
pub(super) const RECORDED: &str = "recorded";
pub(super) const JOURNAL: &str = "journal";
pub(super) const STUBBED: &str = "stubbed";
pub(super) const CUT: &str = "cut";
pub(super) const CANNOT_REPLAY: &str = "Cannot replay: ";
pub(super) const SCRIPT_CHANGED: &str =
    "The script changed since that firing ran; this ran the file as it is now";
pub(super) const RAN_AGAINST: &str = "Ran against state revision ";
pub(super) const LIMITED: &str = "A real firing would have been refused by ";
pub(super) const UNTIL: &str = " until ";
const SEPARATOR: &str = " \u{b7} ";
const GAP: &str = " ";

/// The firing a dry run replays, and what the replay did.
pub(super) struct DryRun {
    pub(super) fire_id: String,
    /// The script version the replayed firing ran, which the dry run's own
    /// differs from once the script changed.
    pub(super) digest: String,
    pub(super) result: Loading<Box<DryRunDetail>>,
    /// Enter opened it as a trace in place of the Firings list.
    pub(super) open: bool,
}

impl DryRun {
    pub(super) fn new(replayed: &FiringSummary) -> Self {
        Self {
            fire_id: replayed.fire_id.clone(),
            digest: replayed.digest.clone(),
            result: Loading::Requested,
            open: false,
        }
    }

    pub(super) fn loaded(&self) -> Option<&DryRunDetail> {
        match &self.result {
            Loading::Loaded(detail) => Some(detail),
            Loading::Requested | Loading::Failed(_) => None,
        }
    }

    pub(super) fn loading(&self) -> bool {
        matches!(self.result, Loading::Requested)
    }

    /// How the action at `index` of its trace was answered.
    pub(super) fn answer(&self, index: usize) -> Option<Answer> {
        self.loaded()?.answers.get(index).copied()
    }
}

/// Why the runtime would not replay a firing: the reason a finished firing
/// cannot be, else the error itself.
pub(super) fn refusal(error: AutomationError) -> String {
    match error {
        AutomationError::NotReplayable { reason, .. } => format!("{CANNOT_REPLAY}{reason}"),
        error => error.to_string(),
    }
}

pub(super) fn answer_badge(answer: Answer) -> (&'static str, Style) {
    let t = theme::current();
    match answer {
        Answer::Recorded => (RECORDED, t.accent),
        Answer::Journal => (JOURNAL, t.tool_success),
        Answer::Stubbed => (STUBBED, t.tool_warning),
        Answer::Cut => (CUT, t.tool_warning),
    }
}

/// What sets a dry run apart from the firing it replays: a script that
/// changed since, the state it ran against, and the limit that would have
/// refused a real firing.
pub(super) fn notes(dry_run: &DryRun, replay: &DryRunDetail, now: i64) -> Vec<(String, Style)> {
    let t = theme::current();
    let mut notes = Vec::new();
    if replay.trace.firing.digest != dry_run.digest {
        notes.push((SCRIPT_CHANGED.to_owned(), t.tool_warning));
    }
    notes.push((
        format!("{RAN_AGAINST}{}", replay.state_revision),
        t.tool_dim,
    ));
    if let Some(limited) = &replay.limited {
        notes.push((
            format!(
                "{LIMITED}{}{UNTIL}{}",
                limit_text(limited.reason),
                moment(limited.until, now)
            ),
            t.tool_warning,
        ));
    }
    notes
}

/// The dry run as the top row of Firings: loading, then what it did as a
/// firing's row tells it, or why it could not run. `merged` names its
/// automation, as the session's rows do.
pub(super) fn row(body: &mut Body, dry_run: &DryRun, merged: bool) {
    let t = theme::current();
    let mut spans = vec![Span::styled(BADGE, t.accent)];
    match &dry_run.result {
        Loading::Requested => {
            spans.push(Span::styled(format!("{SEPARATOR}{LOADING}"), t.tool_dim));
        }
        Loading::Failed(error) => spans.push(Span::styled(
            format!("{SEPARATOR}{}", escape_terminal_controls(error)),
            t.tool_error,
        )),
        Loading::Loaded(replay) => {
            spans.push(Span::raw(GAP));
            spans.extend(firings::row(&replay.trace.firing, merged, None));
        }
    }
    body.item(Item::DryRun, spans);
}
