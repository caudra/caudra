//! Whether a goal's continuations still get anywhere. A turn made no progress
//! when the evaluator saw nothing new, when it repeated an earlier turn of the
//! same goal, or when it stopped to ask a person. Progress is read from the
//! conversation and never from local files, so work on a remote system counts
//! as soon as its tool output shows it. A goal the evaluator sees waiting on
//! something outside the session backs off and checks again, up to
//! [`MAX_GOAL_WAITS`] times, before it stalls.

use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

use caudra_providers::{ContentBlock, Message, Role};

use crate::autopilot::retry_delay;

/// Turns in a row without progress that stall a goal.
pub const MAX_STALLED_TURNS: u32 = 3;
/// Back-offs a goal waiting on an outside change gets before it stalls:
/// a minute, doubling, capped at 30 minutes, about two hours in all.
pub const MAX_GOAL_WAITS: u32 = 8;
/// How many earlier turns of a goal a repeat is looked for in.
const REMEMBERED_TURNS: usize = 16;

/// What the evaluator saw of a goal's turn.
#[derive(Debug)]
pub(crate) struct Judgement {
    pub progressed: bool,
    /// The goal waits on something outside the session that may change by
    /// itself.
    pub waiting: bool,
}

/// What one turn of a goal showed.
#[derive(Debug, Default)]
pub(crate) struct GoalTurn {
    /// `None` when the prescreen skipped the evaluator, and whether the goal
    /// waits stays as it last judged.
    pub judgement: Option<Judgement>,
    /// The turn's tool calls, their results and its text, `None` when there is
    /// no turn to read.
    pub fingerprint: Option<u64>,
    /// The turn called no tool and handed back asking a person.
    pub asked_user: bool,
}

/// What a goal does after a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoalPace {
    /// It goes on as usual.
    Going,
    /// It waits on an outside change: the run ends and the goal is checked
    /// again after `delay`. `attempt` counts the waits since it last moved.
    Wait { attempt: u32, delay: Duration },
    /// Its last `turns` turns made no progress, and it may not wait.
    Stalled { turns: u32 },
}

/// The stall count of the active goal, kept per goal generation.
#[derive(Debug, Default)]
pub(crate) struct StallTracker {
    generation: u64,
    stalled: u32,
    waits: u32,
    waiting: bool,
    seen: VecDeque<u64>,
}

impl StallTracker {
    /// The count of goal `generation` brought back from a saved session,
    /// which keeps backing off where it left off when it was `waits` waits
    /// in.
    pub fn resumed(generation: u64, waits: Option<u32>) -> Self {
        let waits = waits.unwrap_or_default();
        Self {
            generation,
            stalled: waits,
            waits,
            waiting: waits > 0,
            ..Self::default()
        }
    }

    /// Records a turn of goal `generation` and says what the goal does next.
    /// A turn the evaluator did not judge keeps the count, unless it repeated
    /// a turn or asked a person. Asking a person never waits.
    pub fn record(&mut self, generation: u64, turn: &GoalTurn) -> GoalPace {
        if self.generation != generation {
            *self = Self {
                generation,
                ..Self::default()
            };
        }
        let repeated = turn
            .fingerprint
            .is_some_and(|fingerprint| self.seen.contains(&fingerprint));
        if let Some(fingerprint) = turn.fingerprint.filter(|_| !repeated) {
            if self.seen.len() >= REMEMBERED_TURNS {
                self.seen.pop_front();
            }
            self.seen.push_back(fingerprint);
        }
        let progressed = turn
            .judgement
            .as_ref()
            .map(|judgement| judgement.progressed);
        if let Some(judgement) = &turn.judgement {
            self.waiting = judgement.waiting;
        }
        if !(repeated || turn.asked_user || progressed == Some(false)) {
            if progressed == Some(true) {
                self.stalled = 0;
                self.waits = 0;
            }
            return GoalPace::Going;
        }
        self.stalled = self.stalled.saturating_add(1);
        if self.waiting && !turn.asked_user {
            self.waits = self.waits.saturating_add(1);
            return match retry_delay(self.waits, MAX_GOAL_WAITS) {
                Some(delay) => GoalPace::Wait {
                    attempt: self.waits,
                    delay,
                },
                None => GoalPace::Stalled {
                    turns: self.stalled,
                },
            };
        }
        if self.stalled >= MAX_STALLED_TURNS {
            GoalPace::Stalled {
                turns: self.stalled,
            }
        } else {
            GoalPace::Going
        }
    }

    /// Leaves a stalled goal that autopilot sent on one turn short of stalling
    /// again, so only a turn that makes progress buys it more. A goal that
    /// spent its waits stalls at its next turn without progress.
    pub fn hold(&mut self, generation: u64) {
        if self.generation == generation {
            self.stalled = self.stalled.min(MAX_STALLED_TURNS - 1);
        }
    }
}

/// A hash of what `turn` did and said, blind to whitespace and reasoning.
pub(crate) fn turn_fingerprint(turn: &[Message]) -> Option<u64> {
    if turn.is_empty() {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    for message in turn {
        matches!(message.role, Role::Assistant).hash(&mut hasher);
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    for word in text.split_whitespace() {
                        word.hash(&mut hasher);
                    }
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    name.hash(&mut hasher);
                    input.to_string().hash(&mut hasher);
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    content.hash(&mut hasher);
                    is_error.hash(&mut hasher);
                }
                ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::Image { .. } => {}
            }
        }
    }
    Some(hasher.finish())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const GENERATION: u64 = 7;
    const REPLY: &str = "Step 3 is waiting on your decision.";
    const OTHER_REPLY: &str = "Committed the price change.";
    const TOOL: &str = "shell";
    const STATUS: &str = "HEAD 5fa5c42dc, clean tree";
    const CHANGED_STATUS: &str = "HEAD 9e3e18d42, clean tree";
    const MINUTE: Duration = Duration::from_secs(60);
    const COUNTS: &str = "turns without progress must be counted in a row";
    const SAME_TURN: &str = "a turn must be recognised by what it did and said";
    const REMOTE: &str = "a changed tool result is progress, wherever the state lives";
    const WAITS: &str = "a goal waiting on an outside change must back off before it stalls";

    fn turn(progressed: Option<bool>, fingerprint: Option<u64>) -> GoalTurn {
        GoalTurn {
            judgement: progressed.map(|progressed| Judgement {
                progressed,
                waiting: false,
            }),
            fingerprint,
            ..GoalTurn::default()
        }
    }

    fn waiting(fingerprint: Option<u64>) -> GoalTurn {
        GoalTurn {
            judgement: Some(Judgement {
                progressed: false,
                waiting: true,
            }),
            fingerprint,
            ..GoalTurn::default()
        }
    }

    fn stalled(turns: u32) -> GoalPace {
        GoalPace::Stalled { turns }
    }

    fn tool_turn(reply: &str, result: &str) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: TOOL.into(),
                    input: json!({ "command": "git status" }),
                    thought_signature: None,
                }],
                ..Message::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: result.into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Message::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: reply.into() }],
                ..Message::default()
            },
        ]
    }

    #[test_case(Some(false) ; "judged_no_progress")]
    #[test_case(Some(true) ; "judged_progress")]
    #[test_case(None ; "unjudged")]
    fn a_single_turn_goes_on(progressed: Option<bool>) {
        let mut tracker = StallTracker::default();

        assert_eq!(
            tracker.record(GENERATION, &turn(progressed, None)),
            GoalPace::Going
        );
    }

    #[test]
    fn turns_without_progress_count_in_a_row_and_progress_resets_them() {
        let mut tracker = StallTracker::default();
        for _ in 1..MAX_STALLED_TURNS {
            assert_eq!(
                tracker.record(GENERATION, &turn(Some(false), None)),
                GoalPace::Going,
                "{COUNTS}"
            );
        }
        assert_eq!(
            tracker.record(GENERATION, &turn(Some(false), None)),
            stalled(MAX_STALLED_TURNS),
            "{COUNTS}"
        );
        tracker.record(GENERATION, &turn(Some(true), None));
        assert_eq!(
            tracker.record(GENERATION, &turn(Some(false), None)),
            GoalPace::Going,
            "{COUNTS}"
        );
    }

    #[test]
    fn an_unjudged_turn_keeps_the_count() {
        let mut tracker = StallTracker::default();
        for _ in 1..MAX_STALLED_TURNS {
            tracker.record(GENERATION, &turn(Some(false), None));
        }
        tracker.record(GENERATION, &turn(None, None));

        assert_eq!(
            tracker.record(GENERATION, &turn(Some(false), None)),
            stalled(MAX_STALLED_TURNS),
            "{COUNTS}"
        );
    }

    #[test]
    fn a_repeated_turn_is_no_progress_whatever_the_evaluator_said() {
        let mut tracker = StallTracker::default();
        let fingerprint = turn_fingerprint(&tool_turn(REPLY, STATUS));
        for _ in 0..MAX_STALLED_TURNS {
            tracker.record(GENERATION, &turn(Some(true), fingerprint));
        }

        assert_eq!(
            tracker.record(GENERATION, &turn(None, fingerprint)),
            stalled(MAX_STALLED_TURNS)
        );
    }

    #[test]
    fn asking_a_person_stalls_even_while_waiting() {
        let mut tracker = StallTracker::default();
        let asked = GoalTurn {
            asked_user: true,
            ..waiting(None)
        };
        for _ in 1..MAX_STALLED_TURNS {
            assert_eq!(tracker.record(GENERATION, &asked), GoalPace::Going);
        }

        assert_eq!(
            tracker.record(GENERATION, &asked),
            stalled(MAX_STALLED_TURNS)
        );
    }

    #[test]
    fn a_waiting_goal_backs_off_then_stalls() {
        let mut tracker = StallTracker::default();
        let fingerprint = turn_fingerprint(&tool_turn(REPLY, STATUS));
        let mut delays = Vec::new();
        for attempt in 1..=MAX_GOAL_WAITS {
            let GoalPace::Wait {
                attempt: counted,
                delay,
            } = tracker.record(GENERATION, &waiting(fingerprint))
            else {
                panic!("{WAITS}: wait {attempt} stalled");
            };
            assert_eq!(counted, attempt, "{WAITS}");
            delays.push(delay);
        }

        assert_eq!(delays[..2], [MINUTE, MINUTE * 2], "{WAITS}");
        assert_eq!(
            tracker.record(GENERATION, &waiting(fingerprint)),
            stalled(MAX_GOAL_WAITS + 1),
            "{WAITS}"
        );
    }

    #[test]
    fn a_skipped_check_keeps_waiting_and_progress_resets_the_backoff() {
        let mut tracker = StallTracker::default();
        let fingerprint = turn_fingerprint(&tool_turn(REPLY, STATUS));
        tracker.record(GENERATION, &waiting(fingerprint));

        assert!(
            matches!(
                tracker.record(GENERATION, &turn(None, fingerprint)),
                GoalPace::Wait { attempt: 2, .. }
            ),
            "{WAITS}"
        );
        tracker.record(GENERATION, &turn(Some(true), None));
        assert_eq!(
            tracker.record(GENERATION, &waiting(fingerprint)),
            GoalPace::Wait {
                attempt: 1,
                delay: MINUTE
            },
            "{WAITS}"
        );
    }

    #[test]
    fn a_new_goal_starts_its_own_count() {
        let mut tracker = StallTracker::default();
        let fingerprint = turn_fingerprint(&tool_turn(REPLY, STATUS));
        for _ in 0..MAX_STALLED_TURNS {
            tracker.record(GENERATION, &turn(Some(false), fingerprint));
        }

        assert_eq!(
            tracker.record(GENERATION + 1, &turn(Some(false), fingerprint)),
            GoalPace::Going
        );
    }

    #[test]
    fn a_hold_leaves_the_goal_one_turn_from_stalling() {
        let mut tracker = StallTracker::default();
        for _ in 0..MAX_STALLED_TURNS {
            tracker.record(GENERATION, &turn(Some(false), None));
        }

        tracker.hold(GENERATION);

        assert_eq!(
            tracker.record(GENERATION, &turn(Some(false), None)),
            stalled(MAX_STALLED_TURNS)
        );
    }

    #[test_case(tool_turn(REPLY, STATUS), tool_turn(&format!("  {REPLY}\n"), STATUS), true ; "whitespace_differs")]
    #[test_case(tool_turn(REPLY, STATUS), tool_turn(OTHER_REPLY, STATUS), false ; "reply_differs")]
    fn a_turn_is_known_by_what_it_did_and_said(
        first: Vec<Message>,
        second: Vec<Message>,
        same: bool,
    ) {
        assert_eq!(
            turn_fingerprint(&first) == turn_fingerprint(&second),
            same,
            "{SAME_TURN}"
        );
    }

    #[test]
    fn a_changed_tool_result_is_a_different_turn() {
        assert_ne!(
            turn_fingerprint(&tool_turn(REPLY, STATUS)),
            turn_fingerprint(&tool_turn(REPLY, CHANGED_STATUS)),
            "{REMOTE}"
        );
    }

    #[test]
    fn no_turn_has_no_fingerprint() {
        assert_eq!(turn_fingerprint(&[]), None);
    }
}
