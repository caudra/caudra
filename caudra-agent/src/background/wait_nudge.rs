//! What Caudra asks the agent waiting on a background shell once the shell
//! has run for a long time: whether the wait is still worth it, or the
//! command hangs and should be stopped.

use std::time::{Duration, Instant};

use caudra_storage::background::ShellJobMetadata;

use super::command_block;
use crate::SnapshotLine;
use crate::prompt::{
    COMMAND_SLOT, ELAPSED_SLOT, FACTS_SLOT, INTERVAL_SLOT, OUTPUT_SLOT, SHELL_WAIT_NUDGE_PROMPT,
    TASK_ID_SLOT,
};
use crate::tools::{PartialOutputFilter, humanize_duration, output_so_far};

const NO_OUTPUT: &str = "It has printed nothing yet.";

/// How often to ask a background shell's owner whether to keep waiting, and
/// the reduction the command's final result gets, so the output the nudge
/// quotes reads the way that result would.
#[derive(Clone)]
pub struct WaitNudge {
    pub interval: Duration,
    pub reduce: Option<PartialOutputFilter>,
    /// When the command started, which every interval counts from.
    pub started: Instant,
}

impl WaitNudge {
    pub(super) fn tail(&self, lines: &[SnapshotLine]) -> Option<String> {
        output_so_far(lines, |printed| match &self.reduce {
            Some(reduce) => reduce(&printed),
            None => printed,
        })
    }
}

/// The command's latest output, as far as the previous nudge can tell.
#[derive(Debug, PartialEq)]
pub(super) enum Output {
    Nothing,
    Changed(String),
    Unchanged(String),
}

impl Output {
    /// `tail` against what the previous nudge showed.
    pub(super) fn since(tail: Option<String>, previous: Option<&str>) -> Self {
        match tail {
            None => Self::Nothing,
            Some(tail) if previous == Some(tail.as_str()) => Self::Unchanged(tail),
            Some(tail) => Self::Changed(tail),
        }
    }

    pub(super) fn tail(&self) -> Option<&str> {
        match self {
            Self::Nothing => None,
            Self::Changed(tail) | Self::Unchanged(tail) => Some(tail),
        }
    }
}

pub(super) fn render(
    task_id: &str,
    metadata: &ShellJobMetadata,
    elapsed: Duration,
    interval: Duration,
    output: &Output,
) -> String {
    let interval = humanize_duration(interval);
    let mut facts = String::new();
    if let Some(eta) = &metadata.eta {
        facts.push_str(&format!(" It was expected to take {eta}."));
    }
    facts.push_str(&format!(
        " It times out after {}.",
        humanize_duration(Duration::from_millis(metadata.timeout_ms))
    ));
    let output = match output {
        Output::Nothing => NO_OUTPUT.to_owned(),
        Output::Changed(tail) => tail.clone(),
        Output::Unchanged(tail) => {
            format!("Its output has not changed since the last check, {interval} ago.\n\n{tail}")
        }
    };
    fill(
        SHELL_WAIT_NUDGE_PROMPT,
        &[
            (TASK_ID_SLOT, task_id),
            (
                ELAPSED_SLOT,
                &humanize_duration(Duration::from_secs(elapsed.as_secs())),
            ),
            (FACTS_SLOT, &facts),
            (COMMAND_SLOT, &command_block(&metadata.command)),
            (OUTPUT_SLOT, &output),
            (INTERVAL_SLOT, &interval),
        ],
    )
}

/// Fills every slot in one pass, so a command or its output that happens to
/// contain a slot's name is never filled in turn.
fn fill(template: &str, slots: &[(&str, &str)]) -> String {
    let mut filled = String::with_capacity(template.len());
    let mut rest = template;
    while let Some((at, slot, value)) = slots
        .iter()
        .filter_map(|(slot, value)| rest.find(slot).map(|at| (at, *slot, *value)))
        .min_by_key(|(at, ..)| *at)
    {
        filled.push_str(&rest[..at]);
        filled.push_str(value);
        rest = &rest[at + slot.len()..];
    }
    filled.push_str(rest);
    filled
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const TASK_ID: &str = "shell-cargo-1";
    const COMMAND: &str = "cargo build --release";
    const TAIL: &str = "Output so far (last 1 lines):\n```\nCompiling caudra\n```";
    const OTHER_TAIL: &str = "Output so far (last 1 lines):\n```\nLinking caudra\n```";
    const ETA: &str = "~4m";
    const ELAPSED: Duration = Duration::from_secs(30 * 60);
    const INTERVAL: Duration = Duration::from_secs(30 * 60);
    const TIMEOUT_MS: u64 = 2 * 60 * 60 * 1000;
    const UNCHANGED_NOTE: &str = "has not changed since the last check, 30m ago";
    const SLOTS: &[&str] = &[
        TASK_ID_SLOT,
        ELAPSED_SLOT,
        FACTS_SLOT,
        COMMAND_SLOT,
        OUTPUT_SLOT,
        INTERVAL_SLOT,
    ];

    fn metadata(command: &str, eta: Option<&str>) -> ShellJobMetadata {
        ShellJobMetadata {
            call_id: "call".into(),
            root_call_id: "call".into(),
            command: command.into(),
            workdir: ".".into(),
            timeout_ms: TIMEOUT_MS,
            mode: "build".into(),
            eta: eta.map(str::to_owned),
        }
    }

    #[test_case(None, None => Output::Nothing ; "nothing_printed")]
    #[test_case(Some(TAIL), None => Output::Changed(TAIL.into()) ; "first_output")]
    #[test_case(Some(TAIL), Some(OTHER_TAIL) => Output::Changed(TAIL.into()) ; "new_output")]
    #[test_case(Some(TAIL), Some(TAIL) => Output::Unchanged(TAIL.into()) ; "same_output")]
    fn output_compares_with_the_previous_check(
        tail: Option<&str>,
        previous: Option<&str>,
    ) -> Output {
        Output::since(tail.map(str::to_owned), previous)
    }

    #[test]
    fn every_slot_is_filled_with_the_shell_facts() {
        let text = render(
            TASK_ID,
            &metadata(COMMAND, Some(ETA)),
            ELAPSED,
            INTERVAL,
            &Output::Changed(TAIL.into()),
        );
        for slot in SLOTS {
            assert!(!text.contains(slot), "{slot} left unfilled: {text}");
        }
        for fact in [
            TASK_ID,
            COMMAND,
            ETA,
            TAIL,
            "running for 30m",
            "times out after 2h",
        ] {
            assert!(text.contains(fact), "{fact} missing: {text}");
        }
    }

    #[test_case(Output::Nothing, NO_OUTPUT ; "no_output_yet")]
    #[test_case(Output::Unchanged(TAIL.into()), UNCHANGED_NOTE ; "stalled_output")]
    fn the_output_state_is_stated(output: Output, expected: &str) {
        let text = render(
            TASK_ID,
            &metadata(COMMAND, None),
            ELAPSED,
            INTERVAL,
            &output,
        );
        assert!(text.contains(expected), "{text}");
        assert!(!text.contains("expected to take"), "{text}");
    }

    #[test]
    fn slot_names_inside_the_command_stay_literal() {
        let command = format!("echo {OUTPUT_SLOT} {TASK_ID_SLOT}");
        let text = render(
            TASK_ID,
            &metadata(&command, None),
            ELAPSED,
            INTERVAL,
            &Output::Nothing,
        );
        assert!(text.contains(&command), "{text}");
    }
}
