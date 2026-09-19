use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use caudra_config::steering::{SteeringConfig, SteeringPolicy};
use caudra_providers::{
    ContentBlock, EMPTY_RESPONSE_MARKER, Message, Model, Role, SteeringKind, estimate_tokens,
};
use regex::Regex;
use tracing::info;

use super::tool_dispatch::{ToolObservation, ToolOutcome};
use crate::AgentError;

const EMPTY_AFTER_TOOLS: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task. Always end your turn with a text response.";
const EMPTY_IDLE: &str = "You ended your turn without a response. Continue the task, and always end your turn with a text response summarizing what you did.";
const PROTOCOL_FACT: &str = "The provider indicated tool use, but supplied no tool calls. No tool was executed for this response.";
const PROTOCOL_PROMPT: &str = "Use the native tool-call interface with valid arguments if a tool is needed; otherwise provide a text answer.";
const TRUNCATION_FACT: &str = "The previous response was cut off.";
const TRUNCATION_PROMPT: &str = "Continue from where it stopped without repeating text already returned. Do not replay completed tool calls.";
const REPORT_STRUCTURED: &str = "The required structured report has not been captured.";
const REPORT_SUMMARY: &str = "The task ended without a visible summary.";
const REPORT_PROMPT: &str = "Provide the required task report now, using the reporting tool when structured output is required, or a concise text summary otherwise.";
const ABANDONED_FACT: &str =
    "The turn ended on a statement of intent. The work it announced was not performed.";
const ABANDONED_PROMPT: &str = "Carry out what you said you would do now, using tool calls. Do not restate the plan. When the work is done, end with the report the task asked for.";
const REPETITION_PROMPT: &str = "Recent responses repeat the same text or tool-call pattern. Reconsider the next useful action and change approach if this repetition is not helping. Legitimate verification or polling may continue.";
const PLANNING_PROMPT: &str = "Recent responses repeatedly use the same tool with repeated calls or errors. Reassess your tool choices and choose a useful next action; change approach if these calls are not helping.";
const NO_TOOL_PROMPT: &str = "Recent assistant responses have not attempted tools. Use available tools when useful and consistent with the user's instructions; otherwise answer directly.";
const HISTORY_SCAN_LIMIT: usize = 16_384;
const TEXT_BYTES_LIMIT: usize = 8_192;
const MIN_REPEATED_TEXT_CHARS: usize = 32;
const MIN_CYCLE: usize = 2;
/// A closing paragraph that happens to open with `I'll` is a real answer, so
/// only a preamble-sized tail is eligible for abandonment.
const MAX_TRAILING_SENTENCE_TOKENS: u32 = 80;
pub const EMPTY_RULE: &str = "empty_response";
const PROTOCOL_RULE: &str = "protocol_mismatch";
const REPORT_RULE: &str = "missing_task_report";
const ABANDONED_RULE: &str = "abandoned_turn";
const TRUNCATION_RULE: &str = "truncation";
const TOOL_REPAIR_RULE: &str = "tool_repair";
const REPETITION_RULE: &str = "repetition";
const PLANNING_RULE: &str = "tool_planning";
const NO_TOOL_RULE: &str = "no_tool_use";
pub(super) const NO_PROGRESS_RULE: &str = "no_progress";

/// Code spans and quoted prose carry other people's sentences. Matching inside
/// them reads an example as the model's own intent.
static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`[^`\n]*`").expect("literal"));
static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new("\"[^\"\n]*\"").expect("literal"));
/// Anchored: the tail has to open on the intent, not merely contain one.
static PROMISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:(?:ok(?:ay)?|alright|now|next|then|first|finally|so|continuing|moving on)[,:]?\s+)*(?:let(?:'|’)?s\b|let me\b|i(?:'|’)?ll\b|i will\b|i(?:'|’)?m going to\b|i am going to\b|i need to\b|i should\b|going to\b|time to\b|back to\b|continuing\b|proceeding\b|re-?running\b|running\b|checking\b|writing\b|reading\b)",
    )
    .expect("literal")
});
/// Conversational uses of the same openers, and the forms that hand back.
static CONVERSATIONAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:let me know|let me explain|let me clarify|let me summari[sz]e|i(?:'|’)?ll wait|i(?:'|’)?ll hold|i(?:'|’)?ll stop|would you like|do you want|want me to|should i\b|if you(?:'|’)?d like|tell me|let me have|just say)\b",
    )
    .expect("literal")
});
static COMPLETION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:done|finished|complete[d]?|no changes|in summary|all set|ready for|nothing else|that(?:'|’)?s it|as requested)\b",
    )
    .expect("literal")
});
/// A promise scheduled behind another event reports what happens next; it is
/// not work the model walked away from.
static DEFERRED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:as soon as|once |when (?:the|you|it|that)|during|afterwards?|later|next time|in the meantime|meanwhile|if (?:you|needed|necessary)|on request|upon)\b",
    )
    .expect("literal")
});
/// A turn that asks the user for something gave control back on purpose,
/// whatever its last sentence promises.
static SOLICITS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:\?|\b(?:when|whenever) you(?:'|’)?re ready\b|\bjust (?:say|tell|drop)\b|\blet me know\b|\bwhat would you like\b|\bsay the word\b)",
    )
    .expect("literal")
});

pub(crate) type SharedSteering = Arc<Mutex<Steering>>;

// Shared across automatic report corrections in one invocation. Pattern
// resets discard evidence, never allowances; a new external invocation owns a
// new instance instead of reviving an exhausted one.
pub(crate) struct Steering {
    policy: SteeringPolicy,
    recoveries: u32,
    advisories: u32,
    reports: u32,
    protocols: u32,
    truncations: u32,
    abandons: u32,
    responses: u64,
    stalled: u32,
    calls: VecDeque<(u64, ToolObservation)>,
    model: Option<String>,
}

pub(super) enum Recovery {
    Empty {
        after_tools: bool,
        nudges: u32,
        /// The provider returned no content at all, not merely no text.
        barren: bool,
    },
    Protocol,
    Truncated,
    ToolRepair,
    /// Ended on an announcement of work the response never performed.
    Abandoned,
}

/// What one response did, as the budgets need to see it.
pub(super) struct Observed {
    pub calls: Vec<ToolObservation>,
    pub protocol: bool,
    /// Carried a tool call or visible text, so the turn moved the task.
    pub productive: bool,
    /// Announced work instead of doing it, per [`abandons_turn`].
    pub abandoned: bool,
}

pub(super) enum RecoveryAction {
    Disabled,
    Continue(Intervention),
}

/// One charged attempt, with the place it holds in its rule's allowance so a
/// watcher can see the budget draining instead of a line repeating.
pub(super) struct Intervention {
    pub message: Option<Box<Message>>,
    pub attempt: u32,
    pub limit: u32,
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
            truncations: 0,
            abandons: 0,
            responses: 0,
            stalled: 0,
            calls: VecDeque::new(),
            model: None,
        }
    }

    pub(super) fn policy(&self) -> &SteeringPolicy {
        &self.policy
    }

    #[cfg(test)]
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

    /// A run that has produced neither a tool call nor visible text for this
    /// many turns is no longer being steered, it is being repeated. Each rule
    /// bounds its own interventions; nothing bounded the turns they share, so
    /// the backstop counts turns rather than rules.
    pub(super) fn no_progress(&self) -> bool {
        self.policy.enabled
            && self.policy.max_stalled_turns > 0
            && self.stalled >= self.policy.max_stalled_turns
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

    pub(super) fn observe(&mut self, observed: Observed) {
        self.responses = self.responses.saturating_add(1);
        if !observed.protocol {
            self.protocols = 0;
        }
        if !observed.abandoned {
            self.abandons = 0;
        }
        self.stalled = if observed.productive {
            0
        } else {
            self.stalled.saturating_add(1)
        };
        if !self.policy.enabled {
            return;
        }
        for call in observed.calls {
            self.calls.push_back((self.responses, call));
            if self.calls.len() > self.observation_window() {
                self.calls.pop_front();
            }
        }
    }

    fn exhausted(&self, episode: u32, limit: u32) -> bool {
        self.recoveries >= self.policy.max_recoveries || episode >= limit
    }

    fn charge(&mut self, rule: &str, episode: u32, limit: u32) -> Result<(), AgentError> {
        if self.exhausted(episode, limit) {
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
            return Ok(RecoveryAction::Disabled);
        }
        let (rule, episode, limit, prompt) = match recovery {
            Recovery::Empty {
                after_tools,
                nudges,
                barren,
            } => {
                let policy = &self.policy.rules.empty_response;
                if !policy.enabled {
                    return Ok(RecoveryAction::Disabled);
                }
                let situational = if after_tools {
                    policy.max_after_tools
                } else {
                    policy.max_idle
                };
                // A response with no content at all was not a near miss. Asking
                // again changes nothing about the request, so the allowance for
                // recovering from one is far shorter than for a turn that at
                // least reasoned.
                let limit = if barren {
                    policy.max_barren.min(situational)
                } else {
                    situational
                };
                let prompt = policy.prompt.as_deref().unwrap_or(if after_tools {
                    EMPTY_AFTER_TOOLS
                } else {
                    EMPTY_IDLE
                });
                // The count is not decoration: identical copies of one sentence
                // are what a stalled model reads as the pattern to continue.
                (
                    EMPTY_RULE,
                    nudges,
                    limit,
                    Some(format!("{prompt}\n\nAttempt {} of {limit}.", nudges + 1)),
                )
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
            Recovery::Truncated => {
                let policy = &self.policy.rules.truncation;
                if !policy.enabled {
                    return Ok(RecoveryAction::Disabled);
                }
                (
                    TRUNCATION_RULE,
                    self.truncations,
                    policy.max_attempts,
                    Some(format!(
                        "{TRUNCATION_FACT}\n\n{}",
                        policy.prompt.as_deref().unwrap_or(TRUNCATION_PROMPT)
                    )),
                )
            }
            Recovery::ToolRepair => (TOOL_REPAIR_RULE, 0, u32::MAX, None),
            // Alone among the recoveries this one gives up quietly. The turn
            // it is correcting produced real text, so spending the budget is
            // reason to accept that text, never to fail the run over it.
            Recovery::Abandoned => {
                let policy = &self.policy.rules.abandoned_turn;
                if !policy.enabled || self.exhausted(self.abandons, policy.max_attempts) {
                    return Ok(RecoveryAction::Disabled);
                }
                (
                    ABANDONED_RULE,
                    self.abandons,
                    policy.max_attempts,
                    Some(format!(
                        "{ABANDONED_FACT}\n\n{}",
                        policy.prompt.as_deref().unwrap_or(ABANDONED_PROMPT)
                    )),
                )
            }
        };
        self.charge(rule, episode, limit)?;
        if rule == PROTOCOL_RULE {
            self.protocols += 1;
        } else if rule == TRUNCATION_RULE {
            self.truncations += 1;
        } else if rule == ABANDONED_RULE {
            self.abandons += 1;
        }
        Ok(RecoveryAction::Continue(Intervention {
            message: prompt
                .map(|text| Box::new(Message::steering(text, rule, SteeringKind::Recovery))),
            attempt: episode + 1,
            limit,
        }))
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

/// Whether a terminal response announced work instead of performing it.
///
/// Two signals, both read off the tail. A text stopping on a bare colon is a
/// lead-in whose list never arrived, which is what a model emitting its end
/// token one sentence early looks like. Otherwise the last sentence has to
/// open on an intent, and must not be a question, a hand-back, a completion,
/// or a promise deferred behind some other event.
///
/// Measured over 2,571 terminal responses from this project's own sessions:
/// 35 of 386 qwen3.8-27b turns, and none of the 2,185 from claude and gpt.
pub(super) fn abandons_turn(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    if text.ends_with(':') {
        return true;
    }
    if SOLICITS.is_match(&sanitize(text)) {
        return false;
    }
    let Some(line) = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
    else {
        return false;
    };
    let line = sanitize(line);
    let sentence = trailing_sentence(&line);
    if sentence.is_empty() || estimate_tokens(sentence) > MAX_TRAILING_SENTENCE_TOKENS {
        return false;
    }
    if CONVERSATIONAL.is_match(sentence)
        || COMPLETION.is_match(sentence)
        || DEFERRED.is_match(sentence)
    {
        return false;
    }
    PROMISE.is_match(sentence)
}

fn sanitize(text: &str) -> String {
    QUOTED
        .replace_all(&INLINE_CODE.replace_all(text, " "), " ")
        .into_owned()
}

/// The last sentence of `line`, split on terminators the regex crate cannot
/// look behind for. Every byte compared is ASCII, so the index is a boundary.
fn trailing_sentence(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut start = 0;
    for index in 0..bytes.len().saturating_sub(1) {
        if matches!(bytes[index], b'.' | b'!' | b'?') && bytes[index + 1].is_ascii_whitespace() {
            start = index + 1;
        }
    }
    line[start..].trim()
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
    use caudra_config::steering::SteeringConfig;
    use caudra_providers::{ContentBlock, Message, Model, ReasoningSource, Role, SteeringKind};
    use test_case::test_case;

    use super::{
        ABANDONED_FACT, ABANDONED_RULE, EMPTY_IDLE, EMPTY_RULE, Intervention, NO_TOOL_RULE,
        Observed, PLANNING_RULE, PROTOCOL_FACT, PROTOCOL_RULE, REPETITION_RULE, REPORT_RULE,
        REPORT_STRUCTURED, REPORT_SUMMARY, Recovery, RecoveryAction, Steering, TEXT_BYTES_LIMIT,
        TRUNCATION_FACT, TRUNCATION_PROMPT, TRUNCATION_RULE, abandons_turn, eligible_response,
        normalized_message,
    };
    use crate::AgentError;
    use crate::agent::tool_dispatch::{ToolObservation, ToolOutcome};

    const MODEL: &str = "anthropic/claude-sonnet-4-6";
    const TEXT: &str = "This is the same substantial response repeated for testing.";
    const CUSTOM: &str = "Custom guidance, not a rule identity.";
    const TOOL: &str = "file_read";
    const EXPECTED_CONTINUE: &str = "a charged recovery continues";
    const OTHER_MODEL: &str = "other-model";
    const TRUNCATION_ATTEMPTS: u32 = 3;
    const ABANDONED_ATTEMPTS: u32 = 2;

    fn default_state() -> Steering {
        Steering::new(SteeringConfig::default().resolve(MODEL))
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
            ..Default::default()
        }
    }

    fn observe_calls(state: &mut Steering, calls: Vec<ToolObservation>) {
        let productive = !calls.is_empty();
        state.observe(Observed {
            calls,
            protocol: false,
            productive,
            abandoned: false,
        });
    }

    fn call(fingerprint: u64, outcome: ToolOutcome) -> ToolObservation {
        ToolObservation {
            name: TOOL.into(),
            fingerprint,
            outcome,
        }
    }

    #[test_case(false, false, 2; "idle")]
    #[test_case(true, false, 3; "after_tools")]
    #[test_case(true, true, 1; "barren_after_tools")]
    #[test_case(false, true, 1; "barren_idle")]
    fn empty_episode_limits(after_tools: bool, barren: bool, limit: u32) {
        let mut state = default_state();
        for nudges in 0..limit {
            assert!(matches!(
                state.recover(Recovery::Empty {
                    after_tools,
                    nudges,
                    barren
                }),
                Ok(RecoveryAction::Continue(Intervention {
                    message: Some(_),
                    ..
                }))
            ));
        }
        assert!(
            matches!(state.recover(Recovery::Empty { after_tools, nudges: limit, barren }), Err(AgentError::SteeringExhausted { rule }) if rule == EMPTY_RULE)
        );
        assert_eq!(state.recoveries, limit);
    }

    /// Each rule bounds its own interventions; interleaved rules can still
    /// spend turn after turn between them, so the backstop counts turns.
    #[test]
    fn the_no_progress_guard_counts_turns_whichever_rule_spoke() {
        let mut state = default_state();
        let limit = state.policy.max_stalled_turns;
        for turn in 1..limit {
            observe_calls(&mut state, Vec::new());
            assert!(!state.no_progress(), "stopped at turn {turn} of {limit}");
        }
        observe_calls(&mut state, Vec::new());
        assert!(state.no_progress());

        observe_calls(&mut state, vec![call(0, ToolOutcome::Success)]);
        assert!(!state.no_progress());
    }

    #[test_case(true; "master_disabled")]
    #[test_case(false; "budget_zero")]
    fn a_disabled_guard_never_stops_a_run(master: bool) {
        let mut state = default_state();
        if master {
            state.policy.enabled = false;
        } else {
            state.policy.max_stalled_turns = 0;
        }
        for _ in 0..=state.policy.max_stalled_turns.saturating_add(1) {
            observe_calls(&mut state, Vec::new());
        }
        assert!(!state.no_progress());
    }

    #[test_case(1, 3; "first")]
    #[test_case(3, 3; "last")]
    fn an_empty_prompt_counts_its_attempt(attempt: u32, limit: u32) {
        let mut state = default_state();
        let RecoveryAction::Continue(intervention) = state
            .recover(Recovery::Empty {
                after_tools: true,
                nudges: attempt - 1,
                barren: false,
            })
            .unwrap()
        else {
            panic!("{EXPECTED_CONTINUE}");
        };
        assert_eq!((intervention.attempt, intervention.limit), (attempt, limit));
        assert!(
            intervention
                .message
                .unwrap()
                .first_text_content()
                .unwrap()
                .contains(&format!("Attempt {attempt} of {limit}"))
        );
    }

    #[test_case(false; "tool_feedback")]
    #[test_case(true; "reset_empty_episodes")]
    fn default_combined_limit_bounds_all_recoverable_transitions(empty: bool) {
        let mut state = default_state();
        for _ in 0..32 {
            let recovery = if empty {
                Recovery::Empty {
                    after_tools: false,
                    nudges: 0,
                    barren: false,
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
        let mut state = default_state();
        state.policy.max_recoveries = 2;
        assert!(matches!(
            state.recover(Recovery::ToolRepair),
            Ok(RecoveryAction::Continue(Intervention { message: None, .. }))
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
        let mut state = default_state();
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
    fn disabling_suppresses_recovery_without_charging(master: bool) {
        let mut state = default_state();
        if master {
            state.policy.enabled = false;
        } else {
            state.policy.rules.empty_response.enabled = false;
            state.policy.rules.protocol_mismatch.enabled = false;
            state.policy.rules.missing_task_report.enabled = false;
            state.policy.rules.repeated_tool_call.enabled = false;
            state.policy.rules.truncation.enabled = false;
        }
        assert!(matches!(
            state.recover(Recovery::Empty {
                after_tools: false,
                nudges: 0,
                barren: false
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
            Ok(RecoveryAction::Disabled)
        ));
        assert_eq!(state.recoveries, 0);
        assert_eq!(state.truncations, 0);
    }

    #[test_case(false; "default")]
    #[test_case(true; "custom")]
    fn truncation_allowance_survives_responses_and_pattern_resets(custom: bool) {
        let mut state = default_state();
        if custom {
            state.policy.rules.truncation.prompt = Some(CUSTOM.into());
        }
        for attempt in 0..TRUNCATION_ATTEMPTS {
            observe_calls(
                &mut state,
                vec![call(u64::from(attempt), ToolOutcome::Success)],
            );
            observe_calls(&mut state, Vec::new());
            state.reset_patterns();
            let RecoveryAction::Continue(Intervention {
                message: Some(message),
                ..
            }) = state.recover(Recovery::Truncated).unwrap()
            else {
                panic!()
            };
            let guidance = if custom { CUSTOM } else { TRUNCATION_PROMPT };
            assert_eq!(
                message.first_text_content(),
                Some(format!("{TRUNCATION_FACT}\n\n{guidance}").as_str())
            );
            assert!(message.is_observation());
            let origin = message.steering.unwrap();
            assert_eq!(origin.rule, TRUNCATION_RULE);
            assert_eq!(origin.kind, SteeringKind::Recovery);
        }
        assert!(matches!(
            state.recover(Recovery::Truncated),
            Err(AgentError::SteeringExhausted { rule }) if rule == TRUNCATION_RULE
        ));
        assert_eq!(state.truncations, TRUNCATION_ATTEMPTS);
        assert_eq!(state.recoveries, TRUNCATION_ATTEMPTS);
        assert_eq!(state.advisories, 0);
    }

    #[test_case(0; "no_combined_allowance")]
    #[test_case(1; "one_combined_recovery")]
    #[test_case(32; "rule_limit")]
    fn truncation_charges_both_allowances_only_on_success(budget: u32) {
        let mut state = default_state();
        state.policy.max_recoveries = budget;
        let attempts = budget.min(TRUNCATION_ATTEMPTS);
        for _ in 0..attempts {
            assert!(matches!(
                state.recover(Recovery::Truncated),
                Ok(RecoveryAction::Continue(Intervention {
                    message: Some(_),
                    ..
                }))
            ));
        }
        for _ in 0..2 {
            assert!(matches!(
                state.recover(Recovery::Truncated),
                Err(AgentError::SteeringExhausted { rule }) if rule == TRUNCATION_RULE
            ));
        }
        assert_eq!(state.truncations, attempts);
        assert_eq!(state.recoveries, attempts);
    }

    #[test_case(false; "default")]
    #[test_case(true; "custom")]
    fn protocol_limit_and_facts(custom: bool) {
        let mut state = default_state();
        if custom {
            state.policy.rules.protocol_mismatch.prompt = Some(CUSTOM.into());
        }
        for _ in 0..2 {
            state.observe(Observed {
                calls: Vec::new(),
                protocol: true,
                productive: false,
                abandoned: false,
            });
            let RecoveryAction::Continue(Intervention {
                message: Some(message),
                ..
            }) = state.recover(Recovery::Protocol).unwrap()
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
        observe_calls(&mut state, Vec::new());
        assert!(state.recover(Recovery::Protocol).is_ok());
        assert_eq!(state.recoveries, 3);
    }

    #[test_case(2; "period_two")]
    #[test_case(3; "period_three")]
    #[test_case(4; "period_four")]
    fn exact_cycles_use_ordered_leaf_facts(period: u64) {
        let mut state = default_state();
        let model = Model::from_spec(MODEL).unwrap();
        for index in 0..period * 3 - 1 {
            observe_calls(&mut state, vec![call(index % period, ToolOutcome::Success)]);
        }
        assert!(!state.has_cycle());
        observe_calls(&mut state, vec![call(period - 1, ToolOutcome::Success)]);
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
        let mut state = default_state();
        state.policy.rules.repetition.enabled = false;
        let model = Model::from_spec(MODEL).unwrap();
        for response in 0..3 {
            observe_calls(
                &mut state,
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
        let mut state = default_state();
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
        let mut state = default_state();
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
        let RecoveryAction::Continue(Intervention {
            message: Some(message),
            ..
        }) = state
            .recover(Recovery::Empty {
                after_tools: false,
                nudges: 0,
                barren: false,
            })
            .unwrap()
        else {
            panic!()
        };
        assert!(
            message
                .first_text_content()
                .is_some_and(|text| text.starts_with(EMPTY_IDLE))
        );
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
        let mut state = default_state();
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
        assert!(default_state().advisory(&history, &model, true).is_none());
    }

    #[test_case(0, false; "retained_tail_not_new_evidence")]
    #[test_case(3, true; "new_responses_after_compaction")]
    fn compacted_tail_cannot_reconstruct_discarded_patterns(new_responses: usize, expected: bool) {
        let mut state = default_state();
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
            ..Default::default()
        };
        let mut state = Steering::new(config.resolve(MODEL));
        let mut model = Model::from_spec(MODEL).unwrap();
        state.bind_model(&model, &config);
        observe_calls(&mut state, vec![call(0, ToolOutcome::Success)]);
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
            let mut state = default_state();
            let advisory = state
                .advisory(&vec![message; 3], &Model::from_spec(MODEL).unwrap(), true)
                .unwrap();
            assert_eq!(advisory.steering.unwrap().rule, REPETITION_RULE);
        } else {
            assert!(normalized_message(&assistant(&TEXT.repeat(TEXT_BYTES_LIMIT))).is_none());
        }
    }

    // Verbatim tails from this project's own qwen3.8-27b sessions. Paraphrasing
    // them would test the regex against itself rather than against the model.
    #[test_case("Let me look at the workcell dependency and provider core files.", true ; "promise_only")]
    #[test_case("Let me verify the out-of-project permission handling for the code-graph crawl root, then I'll have everything for the report.", true ; "promise_with_a_trailing_intention")]
    #[test_case("The pinned workcell rev has a cargo checkout there. I can read that for the native tool implementations. Now back to caudra-lua internals:", true ; "dangling_colon")]
    #[test_case("Now the nuclei tuple:", true ; "bare_lead_in")]
    #[test_case("Spot-check confirms: escalated system prompt and a 9-phase kill chain. All work is done — committing this session's fixes:", true ; "a_colon_outranks_completion_words")]
    #[test_case("The script transformed all 10 families. Now let me verify syntax and rendering, then handle CWE.", true ; "promise_after_a_finding")]
    #[test_case("The 10 families transformed but not yet verified. Continuing: verify syntax + render, then CWE, profiles, hash.", true ; "resumption_opener")]
    #[test_case("`fromtimestamp` isn't in the isolated interpreter — computing via `timedelta` instead:", true ; "colon_after_inline_code")]
    // Real hand-backs from the same corpus, and from the claude and gpt
    // sessions the rule must leave alone.
    #[test_case("Done — `sleep 20` ran and exited cleanly (code 0, no output).", false ; "completion")]
    #[test_case("Nothing much — the `sleep 30` command finished cleanly (exit 0). What's next?", false ; "question_to_the_user")]
    #[test_case("Understood — I'll hold here. When you're ready, just tell me which alternative to plan, and I'll write the plan file.", false ; "offer_awaiting_the_user")]
    #[test_case("I have not saved the root cause to memory yet, since plan mode limits me to the plan file. I will do that during implementation.", false ; "promise_deferred_to_a_later_phase")]
    #[test_case("I deliberately did not weaken the guard. I'll report the exact differing field as soon as the run writes its result.", false ; "promise_deferred_behind_an_event")]
    #[test_case("Awesome. Whenever you're ready, drop a task. I'll investigate, ask clarifying questions if needed, and write up a plan for you.", false ; "conditional_offer")]
    #[test_case("Wrote a long joke to `joke.txt`.", false ; "plain_report")]
    #[test_case("The fix is on line 40. Quoting the guidance verbatim: \"Let me check the other callers first.\"", false ; "promise_inside_a_quotation")]
    #[test_case("Everything passes. The failing assertion was `assert!(let me run this)`.", false ; "promise_inside_inline_code")]
    #[test_case("", false ; "empty")]
    fn abandonment_reads_the_tail_of_a_response(text: &str, expected: bool) {
        assert_eq!(abandons_turn(text), expected);
    }

    /// The one recovery that gives up quietly: the response it corrects had
    /// real text, so exhaustion accepts that text instead of failing the run.
    #[test]
    fn an_exhausted_abandonment_stops_intervening_without_an_error() {
        let mut state = default_state();
        for attempt in 0..ABANDONED_ATTEMPTS {
            let RecoveryAction::Continue(Intervention {
                message: Some(message),
                ..
            }) = state.recover(Recovery::Abandoned).unwrap()
            else {
                panic!("{EXPECTED_CONTINUE}");
            };
            let origin = message.steering.as_ref().unwrap();
            assert_eq!(origin.rule, ABANDONED_RULE);
            assert_eq!(origin.kind, SteeringKind::Recovery);
            assert!(
                message
                    .first_text_content()
                    .unwrap()
                    .starts_with(ABANDONED_FACT)
            );
            assert_eq!(state.abandons, attempt + 1);
        }
        assert!(matches!(
            state.recover(Recovery::Abandoned).unwrap(),
            RecoveryAction::Disabled
        ));
    }

    /// The counter tracks a streak, so a response that reports normally lets
    /// the model spend the allowance again later in the same run.
    #[test_case(true, 0 ; "a_clean_response_resets_the_streak")]
    #[test_case(false, 1 ; "a_repeated_abandonment_keeps_it")]
    fn abandonment_is_counted_per_streak(clean: bool, expected: u32) {
        let mut state = default_state();
        assert!(matches!(
            state.recover(Recovery::Abandoned).unwrap(),
            RecoveryAction::Continue(_)
        ));
        state.observe(Observed {
            calls: Vec::new(),
            protocol: false,
            productive: true,
            abandoned: !clean,
        });
        assert_eq!(state.abandons, expected);
    }

    #[test]
    fn a_disabled_abandonment_rule_leaves_the_turn_alone() {
        let mut state = default_state();
        state.policy.rules.abandoned_turn.enabled = false;
        assert!(matches!(
            state.recover(Recovery::Abandoned).unwrap(),
            RecoveryAction::Disabled
        ));
    }
}
