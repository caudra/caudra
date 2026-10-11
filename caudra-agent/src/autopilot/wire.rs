//! The delegate's side of a decision: the kinds it is asked about, the actions
//! Rust takes from it for each, and the strict JSON it answers in. Rust is the
//! authority: an action outside a kind's list is refused, whatever the
//! delegate meant by it.

use caudra_providers::tag_safe_literal;
use caudra_storage::autopilot::StoredAutopilotDecision;
use caudra_storage::sessions::StoredTokenUsage;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::agent::judging::{ONE_JSON_OBJECT_ERROR, correction, json_object, truncate_output};

pub const MAX_DELEGATE_TEXT: usize = 4_000;
pub const MAX_DECISION_ATTEMPTS: u32 = 3;

pub const PERMISSION_KIND: &str = "permission";
pub const QUESTION_KIND: &str = "question";
pub const REPLY_KIND: &str = "reply";
pub const PLAN_KIND: &str = "plan";
pub const REVIEW_KIND: &str = "review";
pub const SECOND_OPINION_KIND: &str = "second_opinion";

pub const ALLOW_ACTION: &str = "allow";
pub const DENY_ACTION: &str = "deny";
pub const ESCALATE_ACTION: &str = "escalate";
pub const ANSWER_ACTION: &str = "answer";
pub const REPLY_ACTION: &str = "reply";
pub const DONE_ACTION: &str = "done";
pub const IMPLEMENT_ACTION: &str = "implement";
pub const REVISE_ACTION: &str = "revise";
pub const CONTINUE_ACTION: &str = "continue";
pub const AGREE_ACTION: &str = "agree";

const ACTION_FIELD: &str = "action";
const REASON_FIELD: &str = "reason";
const GUIDANCE_FIELD: &str = "guidance";
const ANSWERS_FIELD: &str = "answers";
const TEXT_FIELD: &str = "text";
const FEEDBACK_FIELD: &str = "feedback";

const PERMISSION_ACTIONS: &[ActionName] =
    &[ActionName::Allow, ActionName::Deny, ActionName::Escalate];
const RESERVED_PERMISSION_ACTIONS: &[ActionName] = &[ActionName::Deny, ActionName::Escalate];
const QUESTION_ACTIONS: &[ActionName] = &[ActionName::Answer, ActionName::Escalate];
const REPLY_ACTIONS: &[ActionName] = &[ActionName::Reply, ActionName::Done, ActionName::Escalate];
const PLAN_ACTIONS: &[ActionName] = &[
    ActionName::Implement,
    ActionName::Revise,
    ActionName::Escalate,
];
const REVIEW_ACTIONS: &[ActionName] = &[ActionName::Continue, ActionName::Escalate];
const SECOND_OPINION_ACTIONS: &[ActionName] = &[ActionName::Agree, ActionName::Continue];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Permission,
    Question,
    Reply,
    Plan,
    Review,
    SecondOpinion,
}

impl DecisionKind {
    pub const ALL: [Self; 6] = [
        Self::Permission,
        Self::Question,
        Self::Reply,
        Self::Plan,
        Self::Review,
        Self::SecondOpinion,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Permission => PERMISSION_KIND,
            Self::Question => QUESTION_KIND,
            Self::Reply => REPLY_KIND,
            Self::Plan => PLAN_KIND,
            Self::Review => REVIEW_KIND,
            Self::SecondOpinion => SECOND_OPINION_KIND,
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// The actions taken from the delegate for this kind. `reserved` applies
    /// to a permission alone: a reserved prompt can be denied or left to the
    /// human, never allowed.
    pub fn accepted_actions(self, reserved: bool) -> &'static [ActionName] {
        match self {
            Self::Permission if reserved => RESERVED_PERMISSION_ACTIONS,
            Self::Permission => PERMISSION_ACTIONS,
            Self::Question => QUESTION_ACTIONS,
            Self::Reply => REPLY_ACTIONS,
            Self::Plan => PLAN_ACTIONS,
            Self::Review => REVIEW_ACTIONS,
            Self::SecondOpinion => SECOND_OPINION_ACTIONS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionName {
    Allow,
    Deny,
    Escalate,
    Answer,
    Reply,
    Done,
    Implement,
    Revise,
    Continue,
    Agree,
}

impl ActionName {
    pub const ALL: [Self; 10] = [
        Self::Allow,
        Self::Deny,
        Self::Escalate,
        Self::Answer,
        Self::Reply,
        Self::Done,
        Self::Implement,
        Self::Revise,
        Self::Continue,
        Self::Agree,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Allow => ALLOW_ACTION,
            Self::Deny => DENY_ACTION,
            Self::Escalate => ESCALATE_ACTION,
            Self::Answer => ANSWER_ACTION,
            Self::Reply => REPLY_ACTION,
            Self::Done => DONE_ACTION,
            Self::Implement => IMPLEMENT_ACTION,
            Self::Revise => REVISE_ACTION,
            Self::Continue => CONTINUE_ACTION,
            Self::Agree => AGREE_ACTION,
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.name() == name)
    }

    /// The field the action carries beside its reason, and that field's schema.
    fn field(self) -> Option<(&'static str, Value)> {
        match self {
            Self::Deny | Self::Continue => Some((GUIDANCE_FIELD, text_schema())),
            Self::Answer => Some((ANSWERS_FIELD, answers_schema())),
            Self::Reply => Some((TEXT_FIELD, text_schema())),
            Self::Revise => Some((FEEDBACK_FIELD, text_schema())),
            Self::Allow | Self::Escalate | Self::Done | Self::Implement | Self::Agree => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegateAction {
    Allow,
    Deny {
        guidance: String,
    },
    Escalate,
    /// One list of labels per question, in order, as a human answers the form.
    Answer {
        answers: Vec<Vec<String>>,
    },
    Reply {
        text: String,
    },
    Done,
    Implement,
    Revise {
        feedback: String,
    },
    Continue {
        guidance: String,
    },
    Agree,
}

impl DelegateAction {
    pub fn name(&self) -> ActionName {
        match self {
            Self::Allow => ActionName::Allow,
            Self::Deny { .. } => ActionName::Deny,
            Self::Escalate => ActionName::Escalate,
            Self::Answer { .. } => ActionName::Answer,
            Self::Reply { .. } => ActionName::Reply,
            Self::Done => ActionName::Done,
            Self::Implement => ActionName::Implement,
            Self::Revise { .. } => ActionName::Revise,
            Self::Continue { .. } => ActionName::Continue,
            Self::Agree => ActionName::Agree,
        }
    }
}

/// A valid answer from the delegate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateOutput {
    pub action: DelegateAction,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WireError {
    #[error("{}", ONE_JSON_OBJECT_ERROR)]
    NotAnObject,
    #[error("{0}")]
    Json(String),
    #[error("{field} is empty")]
    Empty { field: &'static str },
    #[error(
        "{field} is {chars} characters long, over the limit of {}",
        MAX_DELEGATE_TEXT
    )]
    TooLong { field: &'static str, chars: usize },
    #[error("`{action}` is not an action for a {} decision", .kind.name())]
    Refused { kind: DecisionKind, action: String },
}

impl WireError {
    /// Whether another attempt may fix it. A refused action is an answer, not
    /// a malformed one, so it escalates at once.
    pub fn retryable(&self) -> bool {
        !matches!(self, Self::Refused { .. })
    }
}

/// What follows an attempt whose answer was not valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterInvalid {
    /// Ask again, with this correction.
    Retry(String),
    /// Leave the decision to the human, for this reason.
    Escalate(String),
}

/// The attempts are counted from 1.
pub fn after_invalid(attempt: u32, error: &WireError, output: &str) -> AfterInvalid {
    if !error.retryable() {
        return AfterInvalid::Escalate(error.to_string());
    }
    if attempt >= MAX_DECISION_ATTEMPTS {
        return AfterInvalid::Escalate(format!(
            "no valid answer after {MAX_DECISION_ATTEMPTS} attempts: {error}"
        ));
    }
    let output = tag_safe_literal(&truncate_output(output));
    AfterInvalid::Retry(correction(&error.to_string(), Some(&output)))
}

/// Each variant refuses fields it does not name, so an answer that mixes up
/// two actions is invalid rather than quietly half-read.
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Wire {
    Allow {
        reason: String,
    },
    Deny {
        reason: String,
        guidance: String,
    },
    Escalate {
        reason: String,
    },
    Answer {
        reason: String,
        answers: Vec<Vec<String>>,
    },
    Reply {
        reason: String,
        text: String,
    },
    Done {
        reason: String,
    },
    Implement {
        reason: String,
    },
    Revise {
        reason: String,
        feedback: String,
    },
    Continue {
        reason: String,
        guidance: String,
    },
    Agree {
        reason: String,
    },
}

impl Wire {
    fn validated(self) -> Result<DelegateOutput, WireError> {
        let (reason, action) = match self {
            Self::Allow { reason } => (reason, DelegateAction::Allow),
            Self::Deny { reason, guidance } => (
                reason,
                DelegateAction::Deny {
                    guidance: required_text(GUIDANCE_FIELD, guidance)?,
                },
            ),
            Self::Escalate { reason } => (reason, DelegateAction::Escalate),
            Self::Answer { reason, answers } => {
                for label in answers.iter().flatten() {
                    capped(ANSWERS_FIELD, label)?;
                }
                (reason, DelegateAction::Answer { answers })
            }
            Self::Reply { reason, text } => (
                reason,
                DelegateAction::Reply {
                    text: required_text(TEXT_FIELD, text)?,
                },
            ),
            Self::Done { reason } => (reason, DelegateAction::Done),
            Self::Implement { reason } => (reason, DelegateAction::Implement),
            Self::Revise { reason, feedback } => (
                reason,
                DelegateAction::Revise {
                    feedback: required_text(FEEDBACK_FIELD, feedback)?,
                },
            ),
            Self::Continue { reason, guidance } => (
                reason,
                DelegateAction::Continue {
                    guidance: required_text(GUIDANCE_FIELD, guidance)?,
                },
            ),
            Self::Agree { reason } => (reason, DelegateAction::Agree),
        };
        Ok(DelegateOutput {
            action,
            reason: required_text(REASON_FIELD, reason)?,
        })
    }
}

/// Parses the delegate's answer to a `kind` decision. Over-long text is
/// invalid rather than cut, so a retry can shorten it.
pub fn parse_output(
    output: &str,
    kind: DecisionKind,
    reserved: bool,
) -> Result<DelegateOutput, WireError> {
    let object = json_object(output).ok_or(WireError::NotAnObject)?;
    let value: Value = serde_json::from_str(object).map_err(json_error)?;
    if let Some(action) = value.get(ACTION_FIELD).and_then(Value::as_str)
        && !kind
            .accepted_actions(reserved)
            .iter()
            .any(|accepted| accepted.name() == action)
    {
        return Err(WireError::Refused {
            kind,
            action: action.to_owned(),
        });
    }
    serde_json::from_value::<Wire>(value)
        .map_err(json_error)?
        .validated()
}

/// The schema the request shows the delegate for a `kind` decision: one
/// object per accepted action.
pub fn output_schema(kind: DecisionKind, reserved: bool) -> Value {
    let variants: Vec<Value> = kind
        .accepted_actions(reserved)
        .iter()
        .map(|action| action_schema(*action))
        .collect();
    json!({ "oneOf": variants })
}

fn action_schema(action: ActionName) -> Value {
    let mut properties = Map::new();
    properties.insert(ACTION_FIELD.into(), json!({ "const": action.name() }));
    properties.insert(REASON_FIELD.into(), text_schema());
    let mut required = vec![ACTION_FIELD, REASON_FIELD];
    if let Some((field, schema)) = action.field() {
        properties.insert(field.into(), schema);
        required.push(field);
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn text_schema() -> Value {
    json!({ "type": "string", "minLength": 1, "maxLength": MAX_DELEGATE_TEXT })
}

fn answers_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "array",
            "items": { "type": "string", "maxLength": MAX_DELEGATE_TEXT },
        },
    })
}

fn json_error(error: serde_json::Error) -> WireError {
    WireError::Json(error.to_string())
}

fn required_text(field: &'static str, text: String) -> Result<String, WireError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(WireError::Empty { field });
    }
    capped(field, trimmed)?;
    Ok(if trimmed.len() == text.len() {
        text
    } else {
        trimmed.to_owned()
    })
}

fn capped(field: &'static str, text: &str) -> Result<(), WireError> {
    let chars = text.chars().count();
    if chars > MAX_DELEGATE_TEXT {
        return Err(WireError::TooLong { field, chars });
    }
    Ok(())
}

/// One decision autopilot made, as frontends show it and the session stores
/// it. A refused action or a failed delegate call is recorded as
/// [`ActionName::Escalate`], with the failure as its reason.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AutopilotDecision {
    pub seq: u32,
    /// Unix milliseconds.
    pub at_ms: u64,
    pub kind: DecisionKind,
    pub action: ActionName,
    /// What was decided, such as the tool a prompt names.
    pub subject: String,
    pub reason: String,
    /// The subagent it was made for, `None` for the main agent.
    pub agent: Option<String>,
    pub tool_use_id: Option<String>,
    /// History items the session held when it was made.
    pub history_len: usize,
    pub model: Option<String>,
    /// The delegate's tokens, with its cost filed under whoever paid.
    pub usage: Option<StoredTokenUsage>,
    /// What the delegate cost, wherever pricing is known.
    pub cost: Option<f64>,
}

/// A stored decision this build cannot read back.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StoredDecisionError {
    #[error("unknown autopilot decision kind `{0}`")]
    Kind(String),
    #[error("unknown autopilot action `{0}`")]
    Action(String),
    #[error("autopilot decision seq {0} is out of range")]
    Seq(u64),
}

impl From<AutopilotDecision> for StoredAutopilotDecision {
    fn from(decision: AutopilotDecision) -> Self {
        Self {
            seq: u64::from(decision.seq),
            at_ms: decision.at_ms,
            kind: decision.kind.name().to_owned(),
            action: decision.action.name().to_owned(),
            subject: decision.subject,
            reason: decision.reason,
            agent: decision.agent,
            tool_use_id: decision.tool_use_id,
            history_len: decision.history_len,
            model: decision.model,
            usage: decision.usage,
            cost: decision.cost,
        }
    }
}

impl TryFrom<StoredAutopilotDecision> for AutopilotDecision {
    type Error = StoredDecisionError;

    fn try_from(stored: StoredAutopilotDecision) -> Result<Self, Self::Error> {
        Ok(Self {
            seq: u32::try_from(stored.seq).map_err(|_| StoredDecisionError::Seq(stored.seq))?,
            at_ms: stored.at_ms,
            kind: DecisionKind::from_name(&stored.kind)
                .ok_or(StoredDecisionError::Kind(stored.kind))?,
            action: ActionName::from_name(&stored.action)
                .ok_or(StoredDecisionError::Action(stored.action))?,
            subject: stored.subject,
            reason: stored.reason,
            agent: stored.agent,
            tool_use_id: stored.tool_use_id,
            history_len: stored.history_len,
            model: stored.model,
            usage: stored.usage,
            cost: stored.cost,
        })
    }
}

#[cfg(test)]
mod tests {
    use jsonschema::Validator;
    use test_case::test_case;

    use super::*;

    const ALLOW: &str = r#"{"action":"allow","reason":"reads the repo only"}"#;
    const DENY: &str =
        r#"{"action":"deny","reason":"outside the task","guidance":"stay in the crate"}"#;
    const ESCALATE: &str = r#"{"action":"escalate","reason":"the user must choose"}"#;
    const ANSWER: &str = r#"{"action":"answer","reason":"the plan names it","answers":[["SQLite"],["tests","docs"]]}"#;
    const REPLY: &str = r#"{"action":"reply","reason":"the user said so","text":"Yes, go ahead."}"#;
    const DONE: &str = r#"{"action":"done","reason":"nothing was asked"}"#;
    const IMPLEMENT: &str = r#"{"action":"implement","reason":"the plan covers the request"}"#;
    const REVISE: &str =
        r#"{"action":"revise","reason":"tests are missing","feedback":"add the tests"}"#;
    const CONTINUE: &str =
        r#"{"action":"continue","reason":"work remains","guidance":"finish the parser"}"#;
    const AGREE: &str = r#"{"action":"agree","reason":"the API cannot do it"}"#;
    const MADE_UP: &str = r#"{"action":"approve","reason":"looks fine"}"#;
    const MADE_UP_ACTION: &str = "approve";
    const FENCED_ALLOW: &str =
        "```json\n{\"action\":\"allow\",\"reason\":\"reads the repo only\"}\n```";
    const PROSE: &str = "I would allow it.";
    const TWO_OBJECTS: &str =
        r#"{"action":"done","reason":"one"} {"action":"done","reason":"two"}"#;
    const NO_ACTION: &str = r#"{"reason":"no action named"}"#;
    const NO_REASON: &str = r#"{"action":"done"}"#;
    const BLANK_REASON: &str = r#"{"action":"done","reason":"   "}"#;
    const BLANK_GUIDANCE: &str = r#"{"action":"deny","reason":"no","guidance":" "}"#;
    const BLANK_TEXT: &str = r#"{"action":"reply","reason":"answer","text":""}"#;
    const BLANK_FEEDBACK: &str = r#"{"action":"revise","reason":"redo","feedback":"\n"}"#;
    const ALLOW_WITH_GUIDANCE: &str = r#"{"action":"allow","reason":"fine","guidance":"go"}"#;
    const DENY_WITH_TEXT: &str =
        r#"{"action":"deny","reason":"no","guidance":"stop","text":"stop"}"#;
    const ESCALATE_WITH_EXTRA: &str = r#"{"action":"escalate","reason":"unsure","extra":1}"#;
    const ANSWER_WITH_LABELS: &str =
        r#"{"action":"answer","reason":"picked","answers":[],"labels":[]}"#;
    const DONE_WITH_TEXT: &str = r#"{"action":"done","reason":"over","text":"bye"}"#;
    const PADDED_REASON: &str = "  reads the repo only \n";
    const TRIMMED_REASON: &str = "reads the repo only";
    const LONG_CHAR: char = 'é';
    const SUBJECT: &str = "shell: cargo test";
    const DELEGATE_MODEL: &str = "test/delegate";
    const AT_MS: u64 = 1_767_225_600_000;
    const SEQ: u32 = 7;
    const HISTORY_LEN: usize = 12;
    const COST: f64 = 0.0125;
    const UNKNOWN_NAME: &str = "veto";
    const FIRST_ATTEMPT: u32 = 1;
    const ACCEPTS_SAMPLE: &str = "the schema must accept every answer the parser takes";
    const REFUSES_SAMPLE: &str = "the schema must refuse every answer the parser refuses";
    const RETRY_NAMES_ERROR: &str = "a retry must say what was wrong";
    const RETRY_ECHOES_OUTPUT: &str = "a retry must echo the invalid answer";
    const NEVER_RETRIED: &str = "a refused action must escalate rather than retry";
    const DENY_GUIDANCE: &str = "stay in the crate";
    const ANSWER_LABELS: [&[&str]; 2] = [&["SQLite"], &["tests", "docs"]];
    const REPLY_TEXT: &str = "Yes, go ahead.";
    const REVISE_FEEDBACK: &str = "add the tests";
    const CONTINUE_GUIDANCE: &str = "finish the parser";

    fn answers() -> Vec<Vec<String>> {
        ANSWER_LABELS
            .iter()
            .map(|labels| labels.iter().map(|label| (*label).to_owned()).collect())
            .collect()
    }

    fn parsed(output: &str, kind: DecisionKind, reserved: bool) -> DelegateAction {
        parse_output(output, kind, reserved).unwrap().action
    }

    fn text_field(output: &str, field: &str, text: &str) -> String {
        let mut value: Value = serde_json::from_str(output).unwrap();
        value[field] = Value::from(text);
        value.to_string()
    }

    fn decision() -> AutopilotDecision {
        AutopilotDecision {
            seq: SEQ,
            at_ms: AT_MS,
            kind: DecisionKind::Permission,
            action: ActionName::Deny,
            subject: SUBJECT.into(),
            reason: TRIMMED_REASON.into(),
            agent: None,
            tool_use_id: None,
            history_len: HISTORY_LEN,
            model: Some(DELEGATE_MODEL.into()),
            usage: Some(StoredTokenUsage {
                cost: Some(COST),
                ..StoredTokenUsage::default()
            }),
            cost: Some(COST),
        }
    }

    #[test_case(DecisionKind::Permission, PERMISSION_KIND ; "permission")]
    #[test_case(DecisionKind::Question, QUESTION_KIND ; "question")]
    #[test_case(DecisionKind::Reply, REPLY_KIND ; "reply")]
    #[test_case(DecisionKind::Plan, PLAN_KIND ; "plan")]
    #[test_case(DecisionKind::Review, REVIEW_KIND ; "review")]
    #[test_case(DecisionKind::SecondOpinion, SECOND_OPINION_KIND ; "second_opinion")]
    fn a_kind_reads_back_from_its_wire_name(kind: DecisionKind, name: &str) {
        assert_eq!(kind.name(), name);
        assert_eq!(DecisionKind::from_name(name), Some(kind));
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
    }

    #[test]
    fn every_action_reads_back_from_its_wire_name() {
        for action in ActionName::ALL {
            assert_eq!(ActionName::from_name(action.name()), Some(action));
            assert_eq!(serde_json::to_value(action).unwrap(), json!(action.name()));
        }
    }

    #[test_case(DecisionKind::Permission, false, ALLOW, DelegateAction::Allow ; "permission_allow")]
    #[test_case(DecisionKind::Permission, false, DENY, DelegateAction::Deny { guidance: DENY_GUIDANCE.into() } ; "permission_deny")]
    #[test_case(DecisionKind::Permission, false, ESCALATE, DelegateAction::Escalate ; "permission_escalate")]
    #[test_case(DecisionKind::Permission, true, DENY, DelegateAction::Deny { guidance: DENY_GUIDANCE.into() } ; "reserved_permission_deny")]
    #[test_case(DecisionKind::Permission, true, ESCALATE, DelegateAction::Escalate ; "reserved_permission_escalate")]
    #[test_case(DecisionKind::Question, false, ANSWER, DelegateAction::Answer { answers: answers() } ; "question_answer")]
    #[test_case(DecisionKind::Question, false, ESCALATE, DelegateAction::Escalate ; "question_escalate")]
    #[test_case(DecisionKind::Reply, false, REPLY, DelegateAction::Reply { text: REPLY_TEXT.into() } ; "reply_reply")]
    #[test_case(DecisionKind::Reply, false, DONE, DelegateAction::Done ; "reply_done")]
    #[test_case(DecisionKind::Reply, false, ESCALATE, DelegateAction::Escalate ; "reply_escalate")]
    #[test_case(DecisionKind::Plan, false, IMPLEMENT, DelegateAction::Implement ; "plan_implement")]
    #[test_case(DecisionKind::Plan, false, REVISE, DelegateAction::Revise { feedback: REVISE_FEEDBACK.into() } ; "plan_revise")]
    #[test_case(DecisionKind::Plan, false, ESCALATE, DelegateAction::Escalate ; "plan_escalate")]
    #[test_case(DecisionKind::Review, false, CONTINUE, DelegateAction::Continue { guidance: CONTINUE_GUIDANCE.into() } ; "review_continue")]
    #[test_case(DecisionKind::Review, false, ESCALATE, DelegateAction::Escalate ; "review_escalate")]
    #[test_case(DecisionKind::SecondOpinion, false, AGREE, DelegateAction::Agree ; "second_opinion_agree")]
    #[test_case(DecisionKind::SecondOpinion, false, CONTINUE, DelegateAction::Continue { guidance: CONTINUE_GUIDANCE.into() } ; "second_opinion_continue")]
    fn an_accepted_action_parses(
        kind: DecisionKind,
        reserved: bool,
        output: &str,
        expected: DelegateAction,
    ) {
        assert_eq!(parsed(output, kind, reserved), expected);
    }

    #[test_case(DecisionKind::Permission, true, ALLOW, ALLOW_ACTION ; "reserved_permission_allow")]
    #[test_case(DecisionKind::Permission, false, ANSWER, ANSWER_ACTION ; "permission_answer")]
    #[test_case(DecisionKind::Permission, false, DONE, DONE_ACTION ; "permission_done")]
    #[test_case(DecisionKind::Question, false, ALLOW, ALLOW_ACTION ; "question_allow")]
    #[test_case(DecisionKind::Question, false, REPLY, REPLY_ACTION ; "question_reply")]
    #[test_case(DecisionKind::Reply, false, IMPLEMENT, IMPLEMENT_ACTION ; "reply_implement")]
    #[test_case(DecisionKind::Plan, false, CONTINUE, CONTINUE_ACTION ; "plan_continue")]
    #[test_case(DecisionKind::Review, false, AGREE, AGREE_ACTION ; "review_agree")]
    #[test_case(DecisionKind::Review, false, DONE, DONE_ACTION ; "review_done")]
    #[test_case(DecisionKind::SecondOpinion, false, ESCALATE, ESCALATE_ACTION ; "second_opinion_escalate")]
    #[test_case(DecisionKind::SecondOpinion, false, DENY, DENY_ACTION ; "second_opinion_deny")]
    #[test_case(DecisionKind::Permission, false, MADE_UP, MADE_UP_ACTION ; "made_up_action")]
    fn an_action_outside_the_kind_is_refused(
        kind: DecisionKind,
        reserved: bool,
        output: &str,
        action: &str,
    ) {
        let error = parse_output(output, kind, reserved).unwrap_err();

        assert_eq!(
            error,
            WireError::Refused {
                kind,
                action: action.into()
            }
        );
        assert!(!error.retryable(), "{NEVER_RETRIED}");
    }

    #[test_case(DecisionKind::Permission, ALLOW_WITH_GUIDANCE ; "allow_with_guidance")]
    #[test_case(DecisionKind::Permission, DENY_WITH_TEXT ; "deny_with_text")]
    #[test_case(DecisionKind::Permission, ESCALATE_WITH_EXTRA ; "escalate_with_extra")]
    #[test_case(DecisionKind::Question, ANSWER_WITH_LABELS ; "answer_with_labels")]
    #[test_case(DecisionKind::Reply, DONE_WITH_TEXT ; "done_with_text")]
    fn an_unknown_field_is_invalid(kind: DecisionKind, output: &str) {
        let error = parse_output(output, kind, false).unwrap_err();

        assert!(matches!(error, WireError::Json(_)), "{error}");
        assert!(error.retryable());
    }

    #[test_case(DecisionKind::Permission, PROSE, WireError::NotAnObject ; "prose")]
    #[test_case(DecisionKind::Reply, BLANK_REASON, WireError::Empty { field: REASON_FIELD } ; "blank_reason")]
    #[test_case(DecisionKind::Permission, BLANK_GUIDANCE, WireError::Empty { field: GUIDANCE_FIELD } ; "blank_guidance")]
    #[test_case(DecisionKind::Reply, BLANK_TEXT, WireError::Empty { field: TEXT_FIELD } ; "blank_text")]
    #[test_case(DecisionKind::Plan, BLANK_FEEDBACK, WireError::Empty { field: FEEDBACK_FIELD } ; "blank_feedback")]
    fn an_invalid_answer_names_its_fault(kind: DecisionKind, output: &str, expected: WireError) {
        assert_eq!(parse_output(output, kind, false).unwrap_err(), expected);
    }

    #[test_case(TWO_OBJECTS ; "two_objects")]
    #[test_case(NO_ACTION ; "no_action")]
    #[test_case(NO_REASON ; "no_reason")]
    fn a_malformed_answer_is_a_retryable_json_error(output: &str) {
        let error = parse_output(output, DecisionKind::Reply, false).unwrap_err();

        assert!(matches!(error, WireError::Json(_)), "{error}");
        assert!(error.retryable());
    }

    #[test]
    fn a_fenced_answer_parses() {
        assert_eq!(
            parsed(FENCED_ALLOW, DecisionKind::Permission, false),
            DelegateAction::Allow
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_from_text() {
        let output = text_field(ALLOW, REASON_FIELD, PADDED_REASON);

        assert_eq!(
            parse_output(&output, DecisionKind::Permission, false)
                .unwrap()
                .reason,
            TRIMMED_REASON
        );
    }

    #[test_case(DecisionKind::Permission, DENY, REASON_FIELD ; "reason")]
    #[test_case(DecisionKind::Permission, DENY, GUIDANCE_FIELD ; "guidance")]
    #[test_case(DecisionKind::Reply, REPLY, TEXT_FIELD ; "text")]
    #[test_case(DecisionKind::Plan, REVISE, FEEDBACK_FIELD ; "feedback")]
    fn text_is_capped_by_characters(kind: DecisionKind, output: &str, field: &'static str) {
        let at_cap = LONG_CHAR.to_string().repeat(MAX_DELEGATE_TEXT);
        let over_cap = LONG_CHAR.to_string().repeat(MAX_DELEGATE_TEXT + 1);

        assert!(parse_output(&text_field(output, field, &at_cap), kind, false).is_ok());
        assert_eq!(
            parse_output(&text_field(output, field, &over_cap), kind, false).unwrap_err(),
            WireError::TooLong {
                field,
                chars: MAX_DELEGATE_TEXT + 1
            }
        );
    }

    #[test]
    fn an_answer_label_is_capped_by_characters() {
        let label = LONG_CHAR.to_string().repeat(MAX_DELEGATE_TEXT + 1);
        let output = json!({
            ACTION_FIELD: ANSWER_ACTION,
            REASON_FIELD: TRIMMED_REASON,
            ANSWERS_FIELD: [[label]],
        })
        .to_string();

        assert_eq!(
            parse_output(&output, DecisionKind::Question, false).unwrap_err(),
            WireError::TooLong {
                field: ANSWERS_FIELD,
                chars: MAX_DELEGATE_TEXT + 1
            }
        );
    }

    #[test]
    fn the_schema_agrees_with_the_parser() {
        let samples = [
            ALLOW,
            DENY,
            ESCALATE,
            ANSWER,
            REPLY,
            DONE,
            IMPLEMENT,
            REVISE,
            CONTINUE,
            AGREE,
            MADE_UP,
            NO_REASON,
            ALLOW_WITH_GUIDANCE,
            DENY_WITH_TEXT,
            ESCALATE_WITH_EXTRA,
            ANSWER_WITH_LABELS,
            DONE_WITH_TEXT,
            BLANK_TEXT,
        ];
        for kind in DecisionKind::ALL {
            for reserved in [false, true] {
                let validator: Validator =
                    jsonschema::validator_for(&output_schema(kind, reserved)).unwrap();
                for sample in samples {
                    let value: Value = serde_json::from_str(sample).unwrap();
                    let parses = parse_output(sample, kind, reserved).is_ok();
                    let message = if parses {
                        ACCEPTS_SAMPLE
                    } else {
                        REFUSES_SAMPLE
                    };
                    assert_eq!(
                        validator.is_valid(&value),
                        parses,
                        "{message}: {} {sample}",
                        kind.name()
                    );
                }
            }
        }
    }

    #[test_case(FIRST_ATTEMPT ; "first")]
    #[test_case(MAX_DECISION_ATTEMPTS - 1 ; "second_to_last")]
    fn an_invalid_answer_is_retried_with_a_correction(attempt: u32) {
        let error = parse_output(BLANK_REASON, DecisionKind::Reply, false).unwrap_err();

        let AfterInvalid::Retry(correction) = after_invalid(attempt, &error, BLANK_REASON) else {
            panic!("attempt {attempt} must be retried");
        };

        assert!(
            correction.contains(&error.to_string()),
            "{RETRY_NAMES_ERROR}"
        );
        assert!(
            correction.contains(&tag_safe_literal(BLANK_REASON)),
            "{RETRY_ECHOES_OUTPUT}"
        );
    }

    #[test]
    fn the_last_invalid_attempt_escalates() {
        let error = parse_output(PROSE, DecisionKind::Reply, false).unwrap_err();

        assert!(matches!(
            after_invalid(MAX_DECISION_ATTEMPTS, &error, PROSE),
            AfterInvalid::Escalate(reason) if reason.contains(&error.to_string())
        ));
    }

    #[test]
    fn a_refused_action_escalates_on_the_first_attempt() {
        let error = parse_output(ALLOW, DecisionKind::Permission, true).unwrap_err();

        assert_eq!(
            after_invalid(FIRST_ATTEMPT, &error, ALLOW),
            AfterInvalid::Escalate(error.to_string()),
            "{NEVER_RETRIED}"
        );
    }

    #[test]
    fn a_decision_round_trips_through_storage() {
        let stored = StoredAutopilotDecision::from(decision());

        assert_eq!(stored.kind, PERMISSION_KIND);
        assert_eq!(stored.action, DENY_ACTION);
        assert_eq!(AutopilotDecision::try_from(stored), Ok(decision()));
    }

    #[test_case(|stored| stored.kind = UNKNOWN_NAME.into(), StoredDecisionError::Kind(UNKNOWN_NAME.into()) ; "unknown_kind")]
    #[test_case(|stored| stored.action = UNKNOWN_NAME.into(), StoredDecisionError::Action(UNKNOWN_NAME.into()) ; "unknown_action")]
    #[test_case(|stored| stored.seq = u64::MAX, StoredDecisionError::Seq(u64::MAX) ; "seq_out_of_range")]
    fn a_stored_decision_this_build_cannot_read_is_refused(
        corrupt: fn(&mut StoredAutopilotDecision),
        expected: StoredDecisionError,
    ) {
        let mut stored = StoredAutopilotDecision::from(decision());
        corrupt(&mut stored);

        assert_eq!(AutopilotDecision::try_from(stored), Err(expected));
    }
}
