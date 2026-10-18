//! Orca's agent status, which it reads out of the terminal's own output as
//! `OSC 9999 ; <json> BEL`. Orca shows the state, the model and what blocks
//! the agent, and resumes a saved session by its id. The status never carries
//! the session title, which Orca sees only as the window title, or anything
//! the model said.

use serde::Serialize;

use super::request::READS_REQUESTS;
use crate::host::InBandStatusEncoder;
use crate::status::{PaneMetadata, PaneState, PaneStatus, PaneSubagent};

const AGENT_TYPE: &str = "caudra";
const PREFIX: &str = "\u{1b}]9999;";
const BELL: &str = "\u{7}";
const SUBAGENT_STATE: &str = "working";
/// Orca drops a subagent whose id is longer, and lists at most this many.
const MAX_SUBAGENT_ID_CHARS: usize = 64;
const MAX_SUBAGENTS: usize = 32;

/// A running subagent, which Orca draws as a child row under the pane.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubagentPayload<'a> {
    id: &'a str,
    agent_type: &'static str,
    description: &'a str,
    state: &'static str,
    #[serde(skip_serializing_if = "is_zero")]
    started_at: u64,
    /// Orca may ask for the row to be opened in the pane, a
    /// [`HostRequest::OpenSubagent`](super::HostRequest::OpenSubagent).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    openable: bool,
}

impl<'a> SubagentPayload<'a> {
    fn listed(subagents: &'a [PaneSubagent]) -> Vec<Self> {
        subagents
            .iter()
            .filter(|subagent| subagent.id.chars().count() <= MAX_SUBAGENT_ID_CHARS)
            .take(MAX_SUBAGENTS)
            .map(|subagent| Self {
                id: &subagent.id,
                agent_type: AGENT_TYPE,
                description: &subagent.name,
                state: SUBAGENT_STATE,
                started_at: subagent.started_at_ms,
                openable: READS_REQUESTS,
            })
            .collect()
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Payload<'a> {
    state: &'static str,
    agent_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    /// What Orca resumes the pane's agent with, `caudra --resume <id>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_input: Option<&'a str>,
    /// A `done` that marks a session starting or ending rather than a
    /// finished turn, so Orca raises no notice for it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    session_boundary: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    subagents: Vec<SubagentPayload<'a>>,
    /// Orca may ask for the main chat, a
    /// [`HostRequest::OpenMain`](super::HostRequest::OpenMain).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    openable: bool,
}

pub(super) struct OrcaStatusEncoder;

impl OrcaStatusEncoder {
    fn frame(payload: &Payload<'_>) -> String {
        let json = serde_json::to_string(payload).unwrap_or_default();
        format!("{PREFIX}{json}{BELL}")
    }
}

impl InBandStatusEncoder for OrcaStatusEncoder {
    fn encode(&self, status: &PaneStatus, metadata: Option<&PaneMetadata<'_>>) -> String {
        let (state, session_boundary) = match status.state {
            PaneState::Idle => ("done", true),
            PaneState::Working => ("working", false),
            PaneState::Blocked(_) => ("blocked", false),
            PaneState::Done | PaneState::Error => ("done", false),
        };
        Self::frame(&Payload {
            state,
            agent_type: AGENT_TYPE,
            model: metadata
                .map(|metadata| metadata.model.as_ref())
                .filter(|model| !model.is_empty()),
            session_id: metadata.and_then(|metadata| metadata.session_id.as_deref()),
            tool_input: status
                .label
                .as_deref()
                .filter(|_| status.state.is_blocked()),
            session_boundary,
            subagents: metadata
                .map(|metadata| SubagentPayload::listed(&metadata.subagents))
                .unwrap_or_default(),
            openable: READS_REQUESTS,
        })
    }

    fn release(&self) -> Option<String> {
        Some(Self::frame(&Payload {
            state: "done",
            agent_type: AGENT_TYPE,
            model: None,
            session_id: None,
            tool_input: None,
            session_boundary: true,
            subagents: Vec::new(),
            openable: false,
        }))
    }

    fn wraps_for_mux(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::BlockKind;
    use test_case::test_case;

    const MODEL: &str = "claude-opus-4-5";
    const TITLE: &str = "Private session title";
    const LABEL: &str = "Permission requested: shell";
    const SESSION: &str = "01990000-0000-7000-8000-000000000001";
    const SUBAGENT_ID: &str = "toolu_01";
    const SUBAGENT_NAME: &str = "Research the parser";
    const STARTED_AT_MS: u64 = 1_760_000_000_000;

    fn metadata() -> PaneMetadata<'static> {
        PaneMetadata {
            title: TITLE.into(),
            model: MODEL.into(),
            context_percent: Some(42),
            session_id: Some(SESSION.into()),
            subagents: Vec::new(),
        }
    }

    /// The frame for this JSON body, with the `openable` flag this build adds.
    fn framed(json: &str) -> String {
        let flag = if READS_REQUESTS {
            r#","openable":true"#
        } else {
            ""
        };
        let body = json.strip_suffix('}').unwrap();
        format!("\u{1b}]9999;{body}{flag}}}\u{7}")
    }

    fn encode(state: PaneState, label: Option<&'static str>) -> String {
        OrcaStatusEncoder.encode(
            &PaneStatus {
                label: label.map(Into::into),
                ..PaneStatus::new(state)
            },
            Some(&metadata()),
        )
    }

    #[test_case(PaneState::Idle, r#"{"state":"done","agentType":"caudra","model":"claude-opus-4-5","sessionId":"01990000-0000-7000-8000-000000000001","sessionBoundary":true}"# ; "idle_is_a_session_boundary")]
    #[test_case(PaneState::Working, r#"{"state":"working","agentType":"caudra","model":"claude-opus-4-5","sessionId":"01990000-0000-7000-8000-000000000001"}"# ; "working")]
    #[test_case(PaneState::Blocked(Some(BlockKind::Permission)), r#"{"state":"blocked","agentType":"caudra","model":"claude-opus-4-5","sessionId":"01990000-0000-7000-8000-000000000001","toolInput":"Permission requested: shell"}"# ; "blocked_names_the_blocker")]
    #[test_case(PaneState::Done, r#"{"state":"done","agentType":"caudra","model":"claude-opus-4-5","sessionId":"01990000-0000-7000-8000-000000000001"}"# ; "done")]
    #[test_case(PaneState::Error, r#"{"state":"done","agentType":"caudra","model":"claude-opus-4-5","sessionId":"01990000-0000-7000-8000-000000000001"}"# ; "error_is_done")]
    fn each_state_is_one_frame_without_title_or_context(state: PaneState, json: &str) {
        assert_eq!(encode(state, Some(LABEL)), framed(json));
    }

    #[test]
    fn control_bytes_in_a_label_stay_inside_the_json() {
        let frame = encode(PaneState::Blocked(None), Some("a\u{7}b\u{1b}]9999;c"));

        let body = frame
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.strip_suffix(BELL))
            .unwrap();
        assert!(!body.contains(['\u{7}', '\u{1b}']), "{body:?}");
        let payload: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(payload["toolInput"], "a\u{7}b\u{1b}]9999;c");
    }

    #[test]
    fn an_unknown_model_is_left_out() {
        let frame = OrcaStatusEncoder.encode(&PaneStatus::new(PaneState::Working), None);

        assert_eq!(frame, framed(r#"{"state":"working","agentType":"caudra"}"#));
    }

    #[test]
    fn an_unsaved_session_sends_no_id() {
        let metadata = PaneMetadata {
            session_id: None,
            ..metadata()
        };

        let frame = OrcaStatusEncoder.encode(&PaneStatus::new(PaneState::Working), Some(&metadata));

        assert_eq!(
            frame,
            framed(r#"{"state":"working","agentType":"caudra","model":"claude-opus-4-5"}"#)
        );
    }

    fn subagent(id: String, started_at_ms: u64) -> PaneSubagent {
        PaneSubagent {
            id,
            name: SUBAGENT_NAME.to_owned(),
            started_at_ms,
        }
    }

    fn subagents_of(metadata: &PaneMetadata<'_>) -> serde_json::Value {
        let frame = OrcaStatusEncoder.encode(&PaneStatus::new(PaneState::Working), Some(metadata));
        let body = frame
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.strip_suffix(BELL))
            .unwrap();
        serde_json::from_str::<serde_json::Value>(body).unwrap()["subagents"].take()
    }

    #[cfg(not(windows))]
    #[test]
    fn running_subagents_are_child_rows() {
        let metadata = PaneMetadata {
            subagents: vec![
                subagent(SUBAGENT_ID.to_owned(), STARTED_AT_MS),
                subagent(format!("{SUBAGENT_ID}2"), 0),
            ],
            ..metadata()
        };

        assert_eq!(
            subagents_of(&metadata),
            serde_json::json!([
                {"id": SUBAGENT_ID, "agentType": "caudra", "description": SUBAGENT_NAME, "state": "working", "startedAt": STARTED_AT_MS, "openable": true},
                {"id": format!("{SUBAGENT_ID}2"), "agentType": "caudra", "description": SUBAGENT_NAME, "state": "working", "openable": true},
            ])
        );
    }

    #[test]
    fn subagents_are_openable_only_where_requests_are_read() {
        let metadata = PaneMetadata {
            subagents: vec![subagent(SUBAGENT_ID.to_owned(), 0)],
            ..metadata()
        };

        assert_eq!(
            subagents_of(&metadata)[0]["openable"],
            serde_json::json!(READS_REQUESTS.then_some(true))
        );
    }

    #[test]
    fn subagents_orca_would_drop_are_left_out() {
        let mut subagents = vec![subagent("x".repeat(MAX_SUBAGENT_ID_CHARS + 1), 0)];
        subagents.extend((0..=MAX_SUBAGENTS).map(|index| subagent(index.to_string(), 0)));
        let metadata = PaneMetadata {
            subagents,
            ..metadata()
        };

        let listed = subagents_of(&metadata);

        assert_eq!(listed.as_array().unwrap().len(), MAX_SUBAGENTS);
        assert_eq!(listed[0]["id"], "0");
    }

    #[test]
    fn release_is_a_session_boundary_and_is_wrapped() {
        assert_eq!(
            OrcaStatusEncoder.release().unwrap(),
            "\u{1b}]9999;{\"state\":\"done\",\"agentType\":\"caudra\",\"sessionBoundary\":true}\u{7}"
        );
        assert!(OrcaStatusEncoder.wraps_for_mux());
    }
}
