//! What a run did, in the order it did it.
//!
//! A run's record arrives in four pieces that share only a clock: the phases
//! it walked, the calls it made, the lines it logged, and how it settled.
//! Reading any one of them alone answers a different question from the one a
//! reader has, which is what happened. This joins them back into a single
//! ordered list and attributes each call to the phase that was open when it
//! started, rather than to the phase label the roster happens to carry.

use caudra_workflow::{CallKind, CallState, RunDetail, RunEventKind, RunSnapshot, RunStatus};

/// Rows sharing a second are ordered by what opened what: a phase before the
/// calls it dispatched, a call before the lines logged beside it.
const RANK_PHASE: u8 = 0;
const RANK_CALL: u8 = 1;
const RANK_LOG: u8 = 2;

const BAR_FILLED: char = '\u{2588}';
const BAR_EMPTY: char = '\u{2591}';
/// Below this a bar is more noise than proportion, so the row goes without.
pub(crate) const MIN_BAR_WIDTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TimelineRow {
    /// A phase the run entered, holding the clock until the next one started
    /// or the run settled.
    Phase {
        title: String,
        at: u64,
        end: u64,
        agents: usize,
        failed: usize,
    },
    /// A phase the script declares and the run has not reached.
    Pending {
        title: String,
    },
    /// One journaled call, named by its position in the detail's call list.
    Call {
        at: u64,
        end: u64,
        call: usize,
    },
    Log {
        at: u64,
        message: String,
    },
    Settled {
        at: u64,
        status: RunStatus,
    },
}

impl TimelineRow {
    /// Whether the row sits under the phase above it. Phases and the run's
    /// own settlement are the spine; everything else hangs off it.
    pub(crate) const fn is_nested(&self) -> bool {
        matches!(self, Self::Call { .. } | Self::Log { .. })
    }

    /// When the row happened on the run's clock. A phase the run has not
    /// reached has not happened, and so has no reading.
    pub(crate) const fn at(&self) -> Option<u64> {
        match self {
            Self::Phase { at, .. }
            | Self::Call { at, .. }
            | Self::Log { at, .. }
            | Self::Settled { at, .. } => Some(*at),
            Self::Pending { .. } => None,
        }
    }

    /// The span the row occupies on the run's clock, when it has one.
    pub(crate) const fn span(&self) -> Option<(u64, u64)> {
        match self {
            Self::Phase { at, end, .. } | Self::Call { at, end, .. } => Some((*at, *end)),
            _ => None,
        }
    }
}

/// The run's record as one ordered list. `now` ends the span of whatever is
/// still open, so a running phase and a running agent both read as long as
/// they have been going.
pub(crate) fn timeline(run: &RunSnapshot, detail: &RunDetail, now: u64) -> Vec<TimelineRow> {
    let closed = match run.status.is_terminal() {
        true => run.updated_at,
        false => now,
    };
    let mut rows: Vec<(u64, u8, usize, TimelineRow)> = Vec::new();
    for (index, record) in run.phase_history.iter().enumerate() {
        let end = run
            .phase_history
            .get(index + 1)
            .map_or(closed, |next| next.started_at);
        let (agents, failed) = agents_within(detail, record.started_at, end);
        rows.push((
            record.started_at,
            RANK_PHASE,
            index,
            TimelineRow::Phase {
                title: record.title.clone(),
                at: record.started_at,
                end,
                agents,
                failed,
            },
        ));
    }
    for (index, call) in detail.calls.iter().enumerate() {
        rows.push((
            call.started_at,
            RANK_CALL,
            index,
            TimelineRow::Call {
                at: call.started_at,
                end: call.finished_at.unwrap_or(closed),
                call: index,
            },
        ));
    }
    for (index, event) in detail.events.iter().enumerate() {
        if event.kind != RunEventKind::Log {
            continue;
        }
        rows.push((
            event.at,
            RANK_LOG,
            index,
            TimelineRow::Log {
                at: event.at,
                message: event.text.clone(),
            },
        ));
    }
    rows.sort_by_key(|(at, rank, index, _)| (*at, *rank, *index));
    let mut timeline: Vec<TimelineRow> = rows.into_iter().map(|(_, _, _, row)| row).collect();
    timeline.extend(
        run.phases
            .iter()
            .filter(|title| {
                !run.phase_history
                    .iter()
                    .any(|record| record.title == **title)
            })
            .map(|title| TimelineRow::Pending {
                title: title.clone(),
            }),
    );
    if run.status.is_terminal() {
        timeline.push(TimelineRow::Settled {
            at: run.updated_at,
            status: run.status,
        });
    }
    timeline
}

/// A row's span drawn against the run's whole clock, so the eye reads how
/// long something took relative to everything else and whether two agents
/// overlapped. A span too short to fill a cell still gets one, because a row
/// that happened must not render as a row that did not.
pub(crate) fn span_bar(span: (u64, u64), window: (u64, u64), width: usize) -> String {
    let (start, end) = span;
    let (from, until) = window;
    let total = until.saturating_sub(from);
    if width < MIN_BAR_WIDTH || total == 0 {
        return String::new();
    }
    let cell = |at: u64| -> usize {
        let offset = at.clamp(from, until).saturating_sub(from);
        ((offset as u128 * width as u128) / total as u128) as usize
    };
    let lead = cell(start).min(width.saturating_sub(1));
    let filled = cell(end).saturating_sub(lead).clamp(1, width - lead);
    let mut bar = String::with_capacity(width);
    bar.extend(std::iter::repeat_n(BAR_EMPTY, lead));
    bar.extend(std::iter::repeat_n(BAR_FILLED, filled));
    bar.extend(std::iter::repeat_n(BAR_EMPTY, width - lead - filled));
    bar
}

/// Agents dispatched inside a phase's span, and how many of them failed. A
/// phase entered twice therefore counts each visit's own agents rather than
/// claiming every agent that ever carried its name.
fn agents_within(detail: &RunDetail, from: u64, until: u64) -> (usize, usize) {
    detail
        .calls
        .iter()
        .filter(|call| call.kind != CallKind::ScratchFile)
        .filter(|call| call.started_at >= from && (call.started_at < until || from == until))
        .fold((0, 0), |(agents, failed), call| {
            (
                agents + 1,
                failed + usize::from(call.state == CallState::Failed),
            )
        })
}

#[cfg(test)]
mod tests {
    use caudra_workflow::{PhaseRecord, RunCall, RunEvent, RunStatus, RunUsage, SourceKind};
    use test_case::test_case;

    use super::*;

    const RUN_ID: &str = "run-1";
    const PLAN: &str = "Plan";
    const RESEARCH: &str = "Research";
    const REPORT: &str = "Report";
    const LOG_TEXT: &str = "dispatching";
    const PHASE_OPENS_ITS_CALLS: &str = "a phase must precede the calls it dispatched";
    const CALLS_PRECEDE_LOGS: &str = "a call must precede a line logged in the same second";
    const A_REVISIT_COUNTS_ITS_OWN: &str = "a phase entered twice must count each visit separately";
    const PENDING_TRAILS_WALKED: &str = "declared phases the run never reached must come last";
    const SETTLEMENT_IS_LAST: &str = "how the run ended must be the final row";
    const NO_WINDOW_NO_BAR: &str = "a run with no duration must not draw a bar";
    const OPEN_SPANS_REACH_NOW: &str = "a phase and a call still open must run to the clock";

    fn run(status: RunStatus, history: &[(&str, u64)], declared: &[&str]) -> RunSnapshot {
        RunSnapshot {
            run_id: RUN_ID.into(),
            display_name: RUN_ID.into(),
            workflow_name: RUN_ID.into(),
            source_kind: SourceKind::User,
            source_path: None,
            objective: None,
            status,
            pause_kind: None,
            pause_message: None,
            revision: 0,
            execution_epoch: 0,
            phase: history.last().map(|(title, _)| (*title).to_owned()),
            phases: declared.iter().map(|title| (*title).to_owned()).collect(),
            phase_history: history
                .iter()
                .map(|(title, at)| PhaseRecord {
                    title: (*title).to_owned(),
                    started_at: *at,
                })
                .collect(),
            agent_budget: 8,
            usage: RunUsage::default(),
            roster: Vec::new(),
            result: None,
            error: None,
            logs: Vec::new(),
            outbox_pending: false,
            created_at: 0,
            updated_at: 100,
        }
    }

    fn call(call_key: u64, at: u64, finished: Option<u64>, state: CallState) -> RunCall {
        RunCall {
            call_key,
            kind: CallKind::Agent,
            state,
            label: None,
            prompt: None,
            task_id: None,
            tokens_used: 0,
            duration_ms: 0,
            started_at: at,
            finished_at: finished,
            result_preview: None,
            error: None,
        }
    }

    fn detail(run: RunSnapshot, calls: Vec<RunCall>, logs: &[u64]) -> RunDetail {
        RunDetail {
            run,
            calls,
            events: logs
                .iter()
                .enumerate()
                .map(|(index, at)| RunEvent {
                    seq: index as u64,
                    at: *at,
                    kind: RunEventKind::Log,
                    text: LOG_TEXT.into(),
                })
                .collect(),
            journal_trimmed: false,
        }
    }

    fn kinds(rows: &[TimelineRow]) -> Vec<&'static str> {
        rows.iter()
            .map(|row| match row {
                TimelineRow::Phase { .. } => "phase",
                TimelineRow::Pending { .. } => "pending",
                TimelineRow::Call { .. } => "call",
                TimelineRow::Log { .. } => "log",
                TimelineRow::Settled { .. } => "settled",
            })
            .collect()
    }

    #[test]
    fn one_second_orders_a_phase_then_its_calls_then_its_logs() {
        let snapshot = run(RunStatus::Active, &[(PLAN, 10)], &[PLAN]);
        let detail = detail(
            snapshot.clone(),
            vec![call(1, 10, Some(20), CallState::Completed)],
            &[10],
        );

        let rows = timeline(&snapshot, &detail, 50);

        assert_eq!(
            kinds(&rows),
            ["phase", "call", "log"],
            "{CALLS_PRECEDE_LOGS}"
        );
        assert!(
            matches!(rows[0], TimelineRow::Phase { .. }),
            "{PHASE_OPENS_ITS_CALLS}"
        );
    }

    #[test]
    fn a_phase_entered_twice_counts_only_the_agents_of_that_visit() {
        let snapshot = run(
            RunStatus::Active,
            &[(RESEARCH, 0), (REPORT, 10), (RESEARCH, 20)],
            &[RESEARCH, REPORT],
        );
        let detail = detail(
            snapshot.clone(),
            vec![
                call(1, 0, Some(5), CallState::Completed),
                call(2, 20, Some(25), CallState::Failed),
                call(3, 21, Some(26), CallState::Completed),
            ],
            &[],
        );

        let rows = timeline(&snapshot, &detail, 50);

        let tallies: Vec<(usize, usize)> = rows
            .iter()
            .filter_map(|row| match row {
                TimelineRow::Phase { agents, failed, .. } => Some((*agents, *failed)),
                _ => None,
            })
            .collect();
        assert_eq!(
            tallies,
            [(1, 0), (0, 0), (2, 1)],
            "{A_REVISIT_COUNTS_ITS_OWN}"
        );
    }

    #[test]
    fn unreached_phases_trail_the_walked_ones_and_settlement_ends_it() {
        let snapshot = run(
            RunStatus::Completed,
            &[(PLAN, 0)],
            &[PLAN, RESEARCH, REPORT],
        );
        let detail = detail(snapshot.clone(), Vec::new(), &[]);

        let rows = timeline(&snapshot, &detail, 50);

        assert_eq!(
            kinds(&rows),
            ["phase", "pending", "pending", "settled"],
            "{PENDING_TRAILS_WALKED}"
        );
        assert!(
            matches!(rows.last(), Some(TimelineRow::Settled { .. })),
            "{SETTLEMENT_IS_LAST}"
        );
    }

    #[test_case((0, 100), 8 => "████████".to_owned(); "a_span_of_the_whole_run_fills_it")]
    #[test_case((0, 50), 8 => "████░░░░".to_owned(); "a_leading_span_fills_the_left")]
    #[test_case((50, 100), 8 => "░░░░████".to_owned(); "a_trailing_span_fills_the_right")]
    #[test_case((25, 75), 8 => "░░████░░".to_owned(); "a_middle_span_sits_between")]
    #[test_case((40, 40), 8 => "░░░█░░░░".to_owned(); "an_instant_still_gets_a_cell")]
    #[test_case((100, 100), 8 => "░░░░░░░█".to_owned(); "an_instant_at_the_end_stays_inside")]
    #[test_case((0, 100), MIN_BAR_WIDTH - 1 => String::new(); "a_narrow_pane_goes_without")]
    fn a_bar_reads_as_its_share_of_the_run(span: (u64, u64), width: usize) -> String {
        span_bar(span, (0, 100), width)
    }

    #[test]
    fn a_run_with_no_duration_has_no_bar_to_draw() {
        assert_eq!(
            span_bar((5, 5), (5, 5), 20),
            String::new(),
            "{NO_WINDOW_NO_BAR}"
        );
    }

    #[test_case(RunStatus::Active, 80 => (80, 80); "running_reaches_the_clock")]
    #[test_case(RunStatus::Completed, 80 => (100, 100); "settled_stops_at_its_last_update")]
    fn an_open_span_ends_where_the_run_does(status: RunStatus, now: u64) -> (u64, u64) {
        let snapshot = run(status, &[(PLAN, 0)], &[PLAN]);
        let detail = detail(
            snapshot.clone(),
            vec![call(1, 0, None, CallState::Started)],
            &[],
        );

        let rows = timeline(&snapshot, &detail, now);

        let ends: Vec<u64> = rows
            .iter()
            .filter_map(|row| row.span())
            .map(|(_, end)| end)
            .collect();
        assert_eq!(ends.len(), 2, "{OPEN_SPANS_REACH_NOW}");
        (ends[0], ends[1])
    }
}
