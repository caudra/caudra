//! A verdict as one compact JSON line, or as the human text `--pretty` prints.

use serde::Serialize;
use serde_json::Value;

use super::types::{
    Blocker, CiKind, DraftGate, Event, MergeGateReason, PrSnapshot, ReadyScope, ReviewDecision,
    ReviewThread, Settled, TimeoutReason, Verdict, WaitReason,
};

const NOT_OPEN: &str = "\u{2014}";
const NONE: &str = "None";
const THREAD_BODY_LIMIT: usize = 180;
const STATUS_HEADER: [&str; 2] = ["| PR | CI | Review | Merge |", "| --- | --- | --- | --- |"];
const STATUS_QUERY_ACTION: &str =
    "action=verify current PR context, GitHub authentication, and API availability, then rearm";
const CONFLICTS_ACTION: &str = "action=resolve merge conflicts before waiting for CI";
const READY_SUMMARY: &str =
    "READY: no merge conflicts, no unresolved review threads, no failing or pending checks";
const DRAFT_ALLOWED_NOTE: &str =
    "note=draft allowed (--allow-draft); leave draft \u{2014} do not mark ready";

pub fn render_json(verdict: &Verdict) -> String {
    let json = serde_json::to_string(verdict).unwrap_or_else(|error| error.to_string());
    format!("{json}\n")
}

/// A wire value as JavaScript's template literals print it: strings bare, null as `null`.
fn text(value: impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(text)) => text,
        Ok(other) => other.to_string(),
        Err(error) => error.to_string(),
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn ci_cell(row: &PrSnapshot) -> String {
    let PrSnapshot::Open { ci, .. } = row else {
        return NOT_OPEN.to_owned();
    };
    let was = if ci.had_previous_passing_ci {
        ", was ✅"
    } else {
        ""
    };
    match ci.kind {
        CiKind::Clean => "✅".to_owned(),
        CiKind::Pending => format!("⏳ {} pending{was}", ci.pending.len()),
        CiKind::Failing => {
            let pending = if ci.pending.is_empty() {
                String::new()
            } else {
                format!(", {} pending", ci.pending.len())
            };
            format!("❌ {} failed{pending}{was}", ci.failed.len())
        }
        CiKind::GithubRejected => format!("❌ GitHub reports failing checks{was}"),
    }
}

fn review_cell(row: &PrSnapshot) -> String {
    let PrSnapshot::Open {
        threads,
        review_automation_running,
        ..
    } = row
    else {
        return NOT_OPEN.to_owned();
    };
    match (*review_automation_running, threads.len()) {
        (true, 0) => "🤖 running".to_owned(),
        (true, open) => format!("🤖 running, {open} open"),
        (false, 0) => "✅".to_owned(),
        (false, open) => format!("📝 {open} open"),
    }
}

fn merge_cell(row: &PrSnapshot) -> &'static str {
    let facts = match row {
        PrSnapshot::Merged { .. } => return "✅ merged",
        PrSnapshot::Closed { .. } => return "❌ closed",
        PrSnapshot::Open { facts, .. } => facts,
    };
    if facts.is_draft {
        "⏸ draft"
    } else if facts.review_decision == Some(ReviewDecision::ChangesRequested) {
        "⚠️ changes requested"
    } else if facts.has_conflicts() {
        "⚠️ conflict"
    } else {
        "✅"
    }
}

pub fn render_status_table(rows: &[PrSnapshot]) -> String {
    let mut lines = STATUS_HEADER.map(str::to_owned).to_vec();
    for row in rows {
        let context = row.context();
        let url = format!(
            "https://github.com/{}/{}/pull/{}",
            context.owner, context.repo, context.number
        );
        lines.push(format!(
            "| [#{}]({url}) | {} | {} | {} |",
            context.number,
            ci_cell(row),
            review_cell(row),
            merge_cell(row)
        ));
    }
    format!("{}\n", lines.join("\n"))
}

fn thread_line(thread: &ReviewThread) -> String {
    let comment = thread.first_comment.as_ref();
    let body = comment.map_or("", |comment| comment.body.as_str());
    let first = body.split('\n').next().unwrap_or_default();
    let first: String = first
        .strip_suffix('\r')
        .unwrap_or(first)
        .chars()
        .take(THREAD_BODY_LIMIT)
        .collect();
    [
        thread.id.clone(),
        comment
            .and_then(|comment| comment.path.clone())
            .unwrap_or_else(|| NONE.to_owned()),
        comment
            .and_then(|comment| comment.line)
            .map_or_else(|| NONE.to_owned(), |line| line.to_string()),
        comment
            .and_then(|comment| comment.author_login.clone())
            .unwrap_or_else(|| NONE.to_owned()),
        format!("isBugBot={}", thread.is_bugbot),
        format!("bugbotReviewPasses={}", thread.bugbot_review_passes),
        first,
    ]
    .join(" ")
}

fn render_blocker(blocker: &Blocker) -> String {
    let lines = match blocker {
        Blocker::MergeConflicts { pr, facts } => vec![
            "BLOCKER: merge-conflicts".to_owned(),
            format!("pr={}", pr.number),
            format!("mergeable={}", text(facts.mergeable)),
            format!("mergeStateStatus={}", text(facts.merge_state_status)),
            CONFLICTS_ACTION.to_owned(),
        ],
        Blocker::ReviewThreads { pr, threads } => {
            let mut lines = vec![
                "BLOCKER: review-threads".to_owned(),
                format!("pr={}", pr.number),
                format!("unresolved={}", threads.len()),
            ];
            lines.extend(threads.iter().map(thread_line));
            lines
        }
        Blocker::FailingChecks { pr, ci } => {
            let failed = if ci.kind == CiKind::Failing {
                ci.failed.as_slice()
            } else {
                &[]
            };
            let mut lines = vec![
                "BLOCKER: failing-checks".to_owned(),
                format!("pr={}", pr.number),
                format!("failed={}", failed.len()),
            ];
            lines.extend(failed.iter().map(|check| {
                format!(
                    "{} {} {} {}",
                    check.name, check.reported_state, check.description, check.link
                )
            }));
            if ci.kind == CiKind::GithubRejected
                && let Some(github) = ci.github
            {
                lines.push(format!(
                    "mergeStateStatus={}",
                    text(github.merge_state_status())
                ));
                lines.push(format!(
                    "headRollupState={}",
                    text(github.head_rollup_state())
                ));
            }
            lines
        }
        Blocker::MergeGate { pr, reason } => {
            let action = match reason {
                MergeGateReason::ClosedWithoutMerge => {
                    "restore or remove the closed PR from the queued stack"
                }
                MergeGateReason::DraftPr => {
                    "mark the PR ready for review before waiting for the merge queue"
                }
                MergeGateReason::ChangesRequested => {
                    "resolve the changes-requested review before waiting for the merge queue"
                }
            };
            vec![
                format!("BLOCKER: {}", text(reason)),
                format!("pr={}", pr.number),
                format!("action={action}"),
            ]
        }
        Blocker::StatusQuery { failures, failure } => vec![
            "BLOCKER: status-query".to_owned(),
            format!("failures={failures}"),
            format!("detail={}", failure.detail()),
            STATUS_QUERY_ACTION.to_owned(),
        ],
    };
    lines.join("\n")
}

fn ready_detail(scope: &ReadyScope) -> String {
    let ReadyScope::Single {
        pr: Settled::Ready(pr),
    } = scope
    else {
        return String::new();
    };
    let gate = &pr.proof.gate;
    let draft_allowed = gate.draft == DraftGate::DraftAllowed;
    let merge_state = pr
        .proof
        .ci
        .github
        .map(|github| text(github.merge_state_status()))
        .unwrap_or_default();
    let note = if draft_allowed {
        format!("\n{DRAFT_ALLOWED_NOTE}")
    } else {
        String::new()
    };
    format!(
        "\nmergeStateStatus={merge_state}\nreviewDecision={}\nisDraft={draft_allowed}{note}",
        text(gate.review_decision)
    )
}

pub fn render_pretty(verdict: &Verdict) -> String {
    match &verdict.event {
        Event::Queue { queue } => {
            let numbers: Vec<String> = queue.iter().map(|pr| format!("#{}", pr.number)).collect();
            format!(
                "QUEUE: captured {} PR{} bottom-to-top: {}\n",
                queue.len(),
                plural(queue.len()),
                numbers.join(",")
            )
        }
        Event::Status { rows, .. } => render_status_table(rows),
        Event::Waiting {
            frontier,
            reason: WaitReason::PendingChecks { pending },
        } => format!(
            "WAITING: frontier=#{}; {} check{} pending\n",
            frontier.number,
            pending.len(),
            plural(pending.len())
        ),
        Event::Waiting {
            frontier,
            reason: WaitReason::MergeQueue { unmerged_count },
        } => format!(
            "WAITING: frontier=#{} is blocker-free; waiting for merge queue ({unmerged_count} PR{} unmerged)\n",
            frontier.number,
            plural(*unmerged_count)
        ),
        Event::Advance {
            merged,
            frontier,
            remaining,
        } => format!(
            "ADVANCE: merged #{}; next=#{}; remaining={remaining}\n",
            merged.number, frontier.number
        ),
        Event::Retry {
            failure,
            retry_in_seconds,
            ..
        } => format!(
            "RETRY: GitHub status query failed; retrying in {retry_in_seconds}s\ndetail={}\n",
            failure.detail()
        ),
        Event::Blocker(blocker) => format!("{}\n", render_blocker(blocker)),
        Event::Ready(scope) => format!("{READY_SUMMARY}{}\n", ready_detail(scope)),
        Event::Complete { queue, .. } => format!(
            "COMPLETE: queued stack merged ({} PR{})\n",
            queue.len(),
            plural(queue.len())
        ),
        Event::Timeout(TimeoutReason::PendingChecks { .. }) => {
            "TIMEOUT: checks still pending\n".to_owned()
        }
        Event::Timeout(TimeoutReason::StatusUnavailable { .. }) => {
            "TIMEOUT: GitHub status remained unavailable\n".to_owned()
        }
        Event::Timeout(TimeoutReason::QueuedStack {
            frontier,
            unmerged_count,
        }) => format!(
            "TIMEOUT: queued stack still has {unmerged_count} PR{} unmerged; frontier=#{}\n",
            plural(*unmerged_count),
            frontier.number
        ),
    }
}

pub fn render(verdict: &Verdict, pretty: bool) -> String {
    if pretty {
        render_pretty(verdict)
    } else {
        render_json(verdict)
    }
}
