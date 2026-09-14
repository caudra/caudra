use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use caudra_config::steering::{SteeringConfig, SteeringPolicy};
use caudra_providers::{ContentBlock, EMPTY_RESPONSE_MARKER, Message, Model, Role, SteeringKind};
use tracing::info;

use super::tool_dispatch::{ToolObservation, ToolOutcome};
use crate::AgentError;

const EMPTY_AFTER_TOOLS: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task. Always end your turn with a text response.";
const EMPTY_IDLE: &str = "You ended your turn without a response. Continue the task, and always end your turn with a text response summarizing what you did.";
const PROTOCOL_FACT: &str = "The provider indicated tool use, but supplied no tool calls. No tool was executed for this response.";
const PROTOCOL_PROMPT: &str = "Use the native tool-call interface with valid arguments if a tool is needed; otherwise provide a text answer.";
const REPORT_STRUCTURED: &str = "The required structured report has not been captured.";
const REPORT_SUMMARY: &str = "The task ended without a visible summary.";
const REPORT_PROMPT: &str = "Provide the required task report now, using the reporting tool when structured output is required, or a concise text summary otherwise.";
const REPETITION_PROMPT: &str = "Recent responses repeat the same text or tool-call pattern. Reconsider the next useful action and change approach if this repetition is not helping. Legitimate verification or polling may continue.";
const PLANNING_PROMPT: &str = "Recent responses repeatedly use the same tool with repeated calls or errors. Reassess your tool choices and choose a useful next action; change approach if these calls are not helping.";
const NO_TOOL_PROMPT: &str = "Recent assistant responses have not attempted tools. Use available tools when useful and consistent with the user's instructions; otherwise answer directly.";
const HISTORY_SCAN_LIMIT: usize = 16_384;
const TEXT_BYTES_LIMIT: usize = 8_192;
const MIN_REPEATED_TEXT_CHARS: usize = 32;
const MIN_CYCLE: usize = 2;
const EMPTY_RULE: &str = "empty_response";
const PROTOCOL_RULE: &str = "protocol_mismatch";
const REPORT_RULE: &str = "missing_task_report";
const TRUNCATION_RULE: &str = "truncation";
const TOOL_REPAIR_RULE: &str = "tool_repair";
const REPETITION_RULE: &str = "repetition";
const PLANNING_RULE: &str = "tool_planning";
const NO_TOOL_RULE: &str = "no_tool_use";

pub(crate) type SharedSteering = Arc<Mutex<Steering>>;

// Shared only across automatic report corrections in one invocation. Pattern
// resets discard evidence, never allowances; a new external invocation owns a
// new instance instead of reviving an exhausted one.
pub(crate) struct Steering {
    policy: SteeringPolicy,
    recoveries: u32,
    advisories: u32,
    reports: u32,
    protocols: u32,
    responses: u64,
    calls: VecDeque<(u64, ToolObservation)>,
    model: Option<String>,
}

pub(super) enum Recovery {
    Empty { after_tools: bool, nudges: u32 },
    Protocol,
    Truncated,
    ToolRepair,
}

pub(super) enum RecoveryAction {
    Disabled,
    Continue(Option<Box<Message>>),
}

pub(super) fn lock(steering: &SharedSteering) -> MutexGuard<'_, Steering> {
    steering.lock().unwrap_or_else(|error| error.into_inner())
}

impl Steering {
    pub(crate) fn new(policy: SteeringPolicy) -> Self {
        Self {
            policy,
            recoveries: 0,
            advisories: 0,
            reports: 0,
            protocols: 0,
            responses: 0,
            calls: VecDeque::new(),
            model: None,
        }
    }

    pub(super) fn policy(&self) -> &SteeringPolicy {
        &self.policy
    }

    pub(super) fn responses(&self) -> u64 {
        self.responses
    }

    // Hard limits belong to the external invocation, not the short-lived Agent
    // used for each report correction. They also apply when steering is disabled.
    pub(super) fn turn_limit_reached(&self, max_turns: Option<u32>) -> bool {
        max_turns.is_some_and(|max| self.responses >= u64::from(max))
    }

    pub(super) fn repeat_threshold(&self) -> usize {
        if self.policy.enabled && self.policy.rules.repeated_tool_call.enabled {
            self.policy.rules.repeated_tool_call.threshold
        } else {
            0
        }
    }

    pub(super) fn reset_patterns(&mut self) {
        self.calls.clear();
    }

    pub(super) fn bind_model(&mut self, model: &Model, config: &SteeringConfig) -> bool {
        let spec = model.spec();
        let changed = self
            .model
            .as_ref()
            .is_some_and(|previous| previous != &spec);
        if changed {
            self.reset_patterns();
            self.policy = config.resolve(&spec);
        }
        self.model = Some(spec);
        changed
    }

    pub(super) fn observation_window(&self) -> usize {
        if !self.policy.enabled {
            return 0;
        }
        let rules = &self.policy.rules;
        let repetition = if rules.repetition.enabled {
            rules.repetition.window
        } else {
            0
        };
        let planning = if rules.tool_planning.enabled {
            rules.tool_planning.after_calls
        } else {
            0
        };
        repetition.max(planning)
    }

    pub(super) fn observe(&mut self, calls: Vec<ToolObservation>, protocol: bool) {
        self.responses = self.responses.saturating_add(1);
        if !protocol {
            self.protocols = 0;
        }
        if !self.policy.enabled {
            return;
        }
        for call in calls {
            self.calls.push_back((self.responses, call));
            if self.calls.len() > self.observation_window() {
                self.calls.pop_front();
            }
        }
    }

    fn charge(&mut self, rule: &str, episode: u32, limit: u32) -> Result<(), AgentError> {
        if self.recoveries >= self.policy.max_recoveries || episode >= limit {
            info!(
                rule,
                action = "exhausted",
                recoveries = self.recoveries,
                episode,
                limit
            );
            return Err(AgentError::SteeringExhausted { rule: rule.into() });
        }
        self.recoveries += 1;
        info!(
            rule,
            action = "recovery",
            recoveries = self.recoveries,
            episode = episode + 1,
            limit
        );
        Ok(())
    }

    pub(crate) fn report_correction(
        &mut self,
        validating: bool,
    ) -> Result<Option<Message>, AgentError> {
        if !self.policy.enabled || !self.policy.rules.missing_task_report.enabled {
            return Ok(None);
        }
        self.charge(
            REPORT_RULE,
            self.reports,
            self.policy.rules.missing_task_report.max_attempts,
        )?;
        self.reports += 1;
        let fact = if validating {
            REPORT_STRUCTURED
        } else {
            REPORT_SUMMARY
        };
        let guidance = self
            .policy
            .rules
            .missing_task_report
            .prompt
            .as_deref()
            .unwrap_or(REPORT_PROMPT);
        Ok(Some(Message::steering(
            format!("{fact}\n\n{guidance}"),
            REPORT_RULE,
            SteeringKind::Recovery,
        )))
    }

    pub(super) fn recover(&mut self, recovery: Recovery) -> Result<RecoveryAction, AgentError> {
        if !self.policy.enabled {
            return Ok(if matches!(recovery, Recovery::Truncated) {
                RecoveryAction::Continue(None)
            } else {
                RecoveryAction::Disabled
            });
        }
        let (rule, episode, limit, prompt) = match recovery {
            Recovery::Empty {
                after_tools,
                nudges,
            } => {
                let policy = &self.policy.rules.empty_response;
                if !policy.enabled {
                    return Ok(RecoveryAction::Disabled);
                }
                let limit = if after_tools {
                    policy.max_after_tools
                } else {
                    policy.max_idle
                };
                let prompt = policy.prompt.as_deref().unwrap_or(if after_tools {
                    EMPTY_AFTER_TOOLS
                } else {
                    EMPTY_IDLE
                });
                (EMPTY_RULE, nudges, limit, Some(prompt.to_owned()))
            }
            Recovery::Protocol => {
                let policy = &self.policy.rules.protocol_mismatch;
                if !policy.enabled {
                    return Ok(RecoveryAction::Disabled);
                }
                (
                    PROTOCOL_RULE,
                    self.protocols,
                    policy.max_attempts,
                    Some(format!(
                        "{PROTOCOL_FACT}\n\n{}",
                        policy.prompt.as_deref().unwrap_or(PROTOCOL_PROMPT)
                    )),
                )
            }
            Recovery::Truncated => (TRUNCATION_RULE, 0, u32::MAX, None),
            Recovery::ToolRepair => (TOOL_REPAIR_RULE, 0, u32::MAX, None),
        };
        self.charge(rule, episode, limit)?;
        if rule == PROTOCOL_RULE {
            self.protocols += 1;
        }
        Ok(RecoveryAction::Continue(prompt.map(|text| {
            Box::new(Message::steering(text, rule, SteeringKind::Recovery))
        })))
    }

    pub(super) fn advisory(
        &mut self,
        history: &[Message],
        model: &Model,
        has_tools: bool,
    ) -> Option<Message> {
        let rules = &self.policy.rules;
        if !self.policy.enabled
            || self.advisories >= self.policy.max_advisories
            || !has_tools
            || !(rules.repetition.enabled
                || rules.tool_planning.enabled
                || rules.no_tool_use.enabled)
        {
            return None;
        }
        // Provenance, rather than prompt text, restores advisory cadence on the
        // active branch. Synthetic input also cuts off the retained tail behind
        // a compaction continuation, including when a fresh Agent reads it.
        let history: Vec<_> = history
            .iter()
            .rev()
            .take(HISTORY_SCAN_LIMIT)
            .take_while(|message| {
                !message.is_compaction_summary
                    && !synthetic_boundary(message)
                    && message.reasoning_source.as_ref().is_none_or(|source| {
                        source.provider == model.provider.as_ref() && source.model == model.id
                    })
            })
            .collect();
        let text: Vec<_> = history
            .iter()
            .filter(|message| eligible_response(message))
            .take(rules.repetition.text_window)
            .filter_map(|message| normalized_message(message))
            .collect();
        let text_repeat = text.first().is_some_and(|latest| {
            text.iter().filter(|text| *text == latest).count() >= rules.repetition.text_repeats
        });
        let cycle = self.has_cycle();
        let candidate = if rules.repetition.enabled
            && (cycle || text_repeat)
            && cooldown_ready(&history, REPETITION_RULE, rules.repetition.cooldown)
        {
            Some((
                REPETITION_RULE,
                rules
                    .repetition
                    .prompt
                    .as_deref()
                    .unwrap_or(REPETITION_PROMPT),
            ))
        } else if rules.tool_planning.enabled
            && self.needs_planning()
            && cooldown_ready(&history, PLANNING_RULE, rules.tool_planning.cooldown)
        {
            Some((
                PLANNING_RULE,
                rules
                    .tool_planning
                    .prompt
                    .as_deref()
                    .unwrap_or(PLANNING_PROMPT),
            ))
        } else if rules.no_tool_use.enabled
            && history
                .iter()
                .filter(|message| eligible_response(message))
                .take(rules.no_tool_use.window)
                .take_while(|message| !message.has_tool_calls())
                .count()
                >= rules.no_tool_use.after_responses
            && cooldown_ready(&history, NO_TOOL_RULE, rules.no_tool_use.cooldown)
        {
            Some((
                NO_TOOL_RULE,
                rules
                    .no_tool_use
                    .prompt
                    .as_deref()
                    .unwrap_or(NO_TOOL_PROMPT),
            ))
        } else {
            None
        };
        let (rule, prompt) = candidate?;
        self.advisories += 1;
        info!(
            rule,
            action = "advisory",
            advisories = self.advisories,
            responses = self.responses
        );
        Some(Message::steering(
            prompt.to_owned(),
            rule,
            SteeringKind::Advisory,
        ))
    }

    fn has_cycle(&self) -> bool {
        if self
            .calls
            .back()
            .is_none_or(|(response, _)| *response != self.responses)
        {
            return false;
        }
        let policy = &self.policy.rules.repetition;
        (MIN_CYCLE..=policy.max_cycle).any(|period| {
            let length = period * policy.cycle_repeats;
            self.calls.len() >= length
                && (0..length).all(|offset| {
                    let end = self.calls.len() - 1;
                    same_call(
                        &self.calls[end - offset].1,
                        &self.calls[end - offset % period].1,
                    )
                })
        })
    }

    fn needs_planning(&self) -> bool {
        let Some((response, latest)) = self.calls.back() else {
            return false;
        };
        if *response != self.responses {
            return false;
        }
        let calls: Vec<_> = self
            .calls
            .iter()
            .filter(|(_, call)| call.name == latest.name)
            .collect();
        let mut responses: Vec<_> = calls.iter().map(|(response, _)| *response).collect();
        responses.dedup();
        let repeated = calls
            .iter()
            .filter(|(_, call)| same_call(call, latest))
            .count()
            >= MIN_CYCLE;
        let errors = calls
            .iter()
            .filter(|(_, call)| call.outcome != ToolOutcome::Success)
            .count()
            >= MIN_CYCLE;
        calls.len() >= self.policy.rules.tool_planning.after_calls
            && responses.len() >= self.policy.rules.tool_planning.after_responses
            && (repeated || errors)
    }
}

fn same_call(left: &ToolObservation, right: &ToolObservation) -> bool {
    left.name == right.name && left.fingerprint == right.fingerprint
}

fn eligible_response(message: &Message) -> bool {
    matches!(message.role, Role::Assistant)
        && !message.is_compaction_summary
        && !message.is_observation()
        && message.steering.is_none()
        && message.display_text.as_deref() != Some("")
        && (message.has_tool_calls() || text_blocks(message).next().is_some())
        && !message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

fn synthetic_boundary(message: &Message) -> bool {
    matches!(message.role, Role::User)
        && message.display_text.as_deref() == Some("")
        && message.steering.is_none()
        && message
            .content
            .iter()
            .all(|block| matches!(block, ContentBlock::Text { .. }))
}

pub(super) fn visible_text(message: &Message) -> Option<String> {
    let text = text_blocks(message).collect::<Vec<_>>().join("\n");
    (!text.is_empty()).then_some(text)
}

fn text_blocks(message: &Message) -> impl Iterator<Item = &str> {
    message.content.iter().filter_map(|block| match block {
        ContentBlock::Text { text } if text != EMPTY_RESPONSE_MARKER && !text.trim().is_empty() => {
            Some(text.as_str())
        }
        _ => None,
    })
}

fn normalized_message(message: &Message) -> Option<String> {
    let mut bytes = 0usize;
    for text in text_blocks(message) {
        bytes = bytes.saturating_add(text.len()).saturating_add(1);
        if bytes > TEXT_BYTES_LIMIT {
            return None;
        }
    }
    normalize_text(&visible_text(message)?)
}

fn normalize_text(text: &str) -> Option<String> {
    if text.len() > TEXT_BYTES_LIMIT {
        return None;
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (text.chars().count() >= MIN_REPEATED_TEXT_CHARS).then_some(text)
}

fn cooldown_ready(history: &[&Message], rule: &str, cooldown: u32) -> bool {
    let mut responses = 0;
    for message in history {
        if message
            .steering
            .as_ref()
            .is_some_and(|origin| origin.rule == rule && origin.kind == SteeringKind::Advisory)
        {
            return responses >= cooldown;
        }
        if matches!(message.role, Role::Assistant) {
            responses += 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use caudra_config::steering::{SteeringConfig, SteeringPreset};
    use caudra_providers::{ContentBlock, Message, Model, ReasoningSource, Role, SteeringKind};
    use test_case::test_case;

    use super::{
        EMPTY_IDLE, EMPTY_RULE, NO_TOOL_RULE, PLANNING_RULE, PROTOCOL_FACT, PROTOCOL_RULE,
        REPETITION_RULE, REPORT_RULE, REPORT_STRUCTURED, REPORT_SUMMARY, Recovery, RecoveryAction,
        Steering, TEXT_BYTES_LIMIT, eligible_response, normalized_message,
    };
    use crate::AgentError;
    use crate::agent::tool_dispatch::{ToolObservation, ToolOutcome};

    const MODEL: &str = "anthropic/claude-sonnet-4-6";
    const TEXT: &str = "This is the same substantial response repeated for testing.";
    const CUSTOM: &str = "Custom guidance, not a rule identity.";
    const TOOL: &str = "file_read";
    const OTHER_MODEL: &str = "other-model";

    fn enhanced() -> Steering {
        Steering::new(
            SteeringConfig {
                preset: Some(SteeringPreset::Enhanced),
                ..Default::default()
            }
            .resolve(MODEL),
        )
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
            ..Default::default()
        }
    }

    fn call(fingerprint: u64, outcome: ToolOutcome) -> ToolObservation {
        ToolObservation {
            name: TOOL.into(),
            fingerprint,
            outcome,
        }
    }

    #[test_case(false, 2; "idle")]
    #[test_case(true, 20; "after_tools")]
    fn empty_episode_limits(after_tools: bool, limit: u32) {
        let mut state = enhanced();
        for nudges in 0..limit {
            assert!(matches!(
                state.recover(Recovery::Empty {
                    after_tools,
                    nudges
                }),
                Ok(RecoveryAction::Continue(Some(_)))
            ));
        }
        assert!(
            matches!(state.recover(Recovery::Empty { after_tools, nudges: limit }), Err(AgentError::SteeringExhausted { rule }) if rule == EMPTY_RULE)
        );
        assert_eq!(state.recoveries, limit);
    }

    #[test_case(false; "tool_feedback")]
    #[test_case(true; "reset_empty_episodes")]
    fn default_combined_limit_bounds_all_recoverable_transitions(empty: bool) {
        let mut state = enhanced();
        for _ in 0..32 {
            let recovery = if empty {
                Recovery::Empty {
                    after_tools: false,
                    nudges: 0,
                }
            } else {
                Recovery::ToolRepair
            };
            assert!(state.recover(recovery).is_ok());
        }
        assert!(matches!(
            state.recover(Recovery::Truncated),
            Err(AgentError::SteeringExhausted { .. })
        ));
        assert_eq!(state.recoveries, 32);
        assert_eq!(state.advisories, 0);
    }

    #[test_case(false; "summary")]
    #[test_case(true; "structured")]
    fn report_corrections_share_combined_allowance(validating: bool) {
        let mut state = enhanced();
        state.policy.max_recoveries = 2;
        assert!(matches!(
            state.recover(Recovery::ToolRepair),
            Ok(RecoveryAction::Continue(None))
        ));
        let message = state.report_correction(validating).unwrap().unwrap();
        assert!(message.is_observation());
        assert!(
            message
                .first_text_content()
                .unwrap()
                .contains(if validating {
                    REPORT_STRUCTURED
                } else {
                    REPORT_SUMMARY
                })
        );
        assert!(
            matches!(state.report_correction(validating), Err(AgentError::SteeringExhausted { rule }) if rule == REPORT_RULE)
        );
        assert_eq!(state.recoveries, 2);
    }

    #[test_case(false; "summary")]
    #[test_case(true; "structured")]
    fn report_episode_limit_survives_pattern_reset(validating: bool) {
        let mut state = enhanced();
        for _ in 0..2 {
            assert!(state.report_correction(validating).unwrap().is_some());
            state.reset_patterns();
        }
        assert!(
            matches!(state.report_correction(validating), Err(AgentError::SteeringExhausted { rule }) if rule == REPORT_RULE)
        );
    }

    #[test_case(true; "master_disabled")]
    #[test_case(false; "rules_disabled")]
    fn disabling_does_not_disable_truncation(master: bool) {
        let mut state = enhanced();
        if master {
            state.policy.enabled = false;
        } else {
            state.policy.rules.empty_response.enabled = false;
            state.policy.rules.protocol_mismatch.enabled = false;
            state.policy.rules.missing_task_report.enabled = false;
            state.policy.rules.repeated_tool_call.enabled = false;
        }
        assert!(matches!(
            state.recover(Recovery::Empty {
                after_tools: false,
                nudges: 0
            }),
            Ok(RecoveryAction::Disabled)
        ));
        assert!(matches!(
            state.recover(Recovery::Protocol),
            Ok(RecoveryAction::Disabled)
        ));
        assert!(state.report_correction(true).unwrap().is_none());
        assert_eq!(state.repeat_threshold(), 0);
        assert!(matches!(
            state.recover(Recovery::Truncated),
            Ok(RecoveryAction::Continue(None))
        ));
        assert_eq!(state.recoveries, u32::from(!master));
    }

    #[test_case(false; "default")]
    #[test_case(true; "custom")]
    fn protocol_limit_and_facts(custom: bool) {
        let mut state = enhanced();
        if custom {
            state.policy.rules.protocol_mismatch.prompt = Some(CUSTOM.into());
        }
        for _ in 0..2 {
            state.observe(Vec::new(), true);
            let RecoveryAction::Continue(Some(message)) =
                state.recover(Recovery::Protocol).unwrap()
            else {
                panic!()
            };
            assert!(
                message
                    .first_text_content()
                    .unwrap()
                    .starts_with(PROTOCOL_FACT)
            );
            if custom {
                assert!(message.first_text_content().unwrap().ends_with(CUSTOM));
            }
        }
        assert!(
            matches!(state.recover(Recovery::Protocol), Err(AgentError::SteeringExhausted { rule }) if rule == PROTOCOL_RULE)
        );
        state.observe(Vec::new(), false);
        assert!(state.recover(Recovery::Protocol).is_ok());
        assert_eq!(state.recoveries, 3);
    }

    #[test_case(2; "period_two")]
    #[test_case(3; "period_three")]
    #[test_case(4; "period_four")]
    fn exact_cycles_use_ordered_leaf_facts(period: u64) {
        let mut state = enhanced();
        let model = Model::from_spec(MODEL).unwrap();
        for index in 0..period * 3 - 1 {
            state.observe(vec![call(index % period, ToolOutcome::Success)], false);
        }
        assert!(!state.has_cycle());
        state.observe(vec![call(period - 1, ToolOutcome::Success)], false);
        assert!(state.has_cycle());
        let message = state.advisory(&[], &model, true).unwrap();
        assert_eq!(message.steering.unwrap().rule, REPETITION_RULE);
        state.reset_patterns();
        assert!(!state.has_cycle());
    }

    #[test_case(false, false, false; "different_successful_reads")]
    #[test_case(true, false, true; "repeated_calls")]
    #[test_case(false, true, true; "repeated_errors")]
    fn planning_requires_evidence_across_responses(repeat: bool, errors: bool, expected: bool) {
        let mut state = enhanced();
        state.policy.rules.repetition.enabled = false;
        let model = Model::from_spec(MODEL).unwrap();
        for response in 0..3 {
            state.observe(
                (0..2)
                    .map(|index| {
                        call(
                            if repeat { 0 } else { response * 2 + index },
                            if errors {
                                ToolOutcome::Failure
                            } else {
                                ToolOutcome::Success
                            },
                        )
                    })
                    .collect(),
                false,
            );
        }
        assert_eq!(state.needs_planning(), expected);
        let advisory = state.advisory(&[], &model, true);
        assert_eq!(
            advisory
                .as_ref()
                .and_then(|message| message.steering.as_ref())
                .map(|origin| origin.rule.as_str()),
            expected.then_some(PLANNING_RULE)
        );
    }

    #[test_case(0, false; "no_responses")]
    #[test_case(2, false; "before_boundary")]
    #[test_case(3, true; "exact_boundary")]
    fn advisory_cooldown_restores_from_metadata(responses: usize, expected: bool) {
        let mut state = enhanced();
        state.policy.rules.no_tool_use.prompt = Some(CUSTOM.into());
        state.policy.rules.repetition.enabled = false;
        let model = Model::from_spec(MODEL).unwrap();
        let mut history = vec![assistant(TEXT); 3];
        history.push(Message::steering(
            CUSTOM.into(),
            NO_TOOL_RULE,
            SteeringKind::Advisory,
        ));
        history.extend((0..responses).map(|_| assistant(TEXT)));
        assert_eq!(state.advisory(&history, &model, true).is_some(), expected);
    }

    #[test_case(false; "no_inventory")]
    #[test_case(true; "inventory")]
    fn no_tool_advisory_has_a_separate_allowance(has_tools: bool) {
        let mut state = enhanced();
        state.policy.rules.repetition.enabled = false;
        let model = Model::from_spec(MODEL).unwrap();
        let history = vec![assistant(TEXT); 3];
        for _ in 0..4 {
            assert_eq!(
                state.advisory(&history, &model, has_tools).is_some(),
                has_tools
            );
        }
        assert!(state.advisory(&history, &model, has_tools).is_none());
        assert_eq!(state.recoveries, 0);
        let RecoveryAction::Continue(Some(message)) = state
            .recover(Recovery::Empty {
                after_tools: false,
                nudges: 0,
            })
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(message.first_text_content(), Some(EMPTY_IDLE));
    }

    #[test_case(Message::empty_marker(); "empty_marker")]
    #[test_case(Message::synthetic(TEXT.into()); "synthetic_input")]
    #[test_case(Message::observation(TEXT.into()); "observation")]
    #[test_case(Message::user(TEXT.into()); "user")]
    #[test_case(Message { is_compaction_summary: true, ..assistant(TEXT) }; "summary")]
    #[test_case(Message { content: vec![ContentBlock::thinking(TEXT.into(), None)], ..assistant(TEXT) }; "reasoning_only")]
    #[test_case(assistant(" \n\t "); "whitespace")]
    fn no_tool_history_excludes_host_and_invisible_messages(message: Message) {
        assert!(!eligible_response(&message));
        let mut state = enhanced();
        assert!(
            state
                .advisory(&vec![message; 3], &Model::from_spec(MODEL).unwrap(), true)
                .is_none()
        );
    }

    #[test_case(false; "compaction")]
    #[test_case(true; "different_model")]
    fn history_boundaries_reset_patterns(not_compaction: bool) {
        let model = Model::from_spec(MODEL).unwrap();
        let mut boundary = assistant(TEXT);
        if not_compaction {
            boundary.reasoning_source = Some(ReasoningSource {
                provider: model.provider.to_string(),
                model: OTHER_MODEL.into(),
                transport: Default::default(),
            });
        } else {
            boundary.is_compaction_summary = true;
        }
        let mut history = vec![assistant(TEXT); 3];
        history.push(boundary);
        history.push(assistant(TEXT));
        assert!(enhanced().advisory(&history, &model, true).is_none());
    }

    #[test_case(0, false; "retained_tail_not_new_evidence")]
    #[test_case(3, true; "new_responses_after_compaction")]
    fn compacted_tail_cannot_reconstruct_discarded_patterns(new_responses: usize, expected: bool) {
        let mut state = enhanced();
        let mut history = vec![assistant(TEXT); 3];
        state.report_correction(false).unwrap();
        state.reset_patterns();
        history.push(Message::synthetic(CUSTOM.into()));
        history.extend((0..new_responses).map(|_| assistant(TEXT)));
        assert_eq!(
            state
                .advisory(&history, &Model::from_spec(MODEL).unwrap(), true)
                .is_some(),
            expected
        );
        assert_eq!(state.recoveries, 1);
    }

    #[test_case(false; "within_combined_budget")]
    #[test_case(true; "combined_budget_exhausted")]
    fn model_changes_discard_patterns_without_refilling(exhausted: bool) {
        let config = SteeringConfig {
            max_recoveries: Some(1),
            preset: Some(SteeringPreset::Enhanced),
            ..Default::default()
        };
        let mut state = Steering::new(config.resolve(MODEL));
        let mut model = Model::from_spec(MODEL).unwrap();
        state.bind_model(&model, &config);
        state.observe(vec![call(0, ToolOutcome::Success)], false);
        assert!(!state.calls.is_empty());
        if exhausted {
            state.report_correction(false).unwrap();
        }
        model.id = OTHER_MODEL.into();
        assert!(state.bind_model(&model, &config));
        assert!(state.calls.is_empty());
        assert_eq!(state.report_correction(false).is_err(), exhausted);
    }

    #[test_case(true; "normalized_whitespace")]
    #[test_case(false; "bounded_text")]
    fn text_repetition_is_deterministic_and_bounded(normal: bool) {
        let message = assistant(if normal { TEXT } else { CUSTOM });
        if normal {
            assert_eq!(
                normalized_message(&message),
                normalized_message(&assistant(&TEXT.replace(' ', "  \n")))
            );
            let mut state = enhanced();
            let advisory = state
                .advisory(&vec![message; 3], &Model::from_spec(MODEL).unwrap(), true)
                .unwrap();
            assert_eq!(advisory.steering.unwrap().rule, REPETITION_RULE);
        } else {
            assert!(normalized_message(&assistant(&TEXT.repeat(TEXT_BYTES_LIMIT))).is_none());
        }
    }
}
