//! Orca's agent status, which it reads out of the terminal's own output as
//! `OSC 9999 ; <json> BEL`. Orca shows the state, the model and what blocks
//! the agent; it is never told a session title or anything the model said.

use serde::Serialize;

use crate::host::InBandStatusEncoder;
use crate::status::{PaneMetadata, PaneState, PaneStatus};

const AGENT_TYPE: &str = "caudra";
const PREFIX: &str = "\u{1b}]9999;";
const BELL: &str = "\u{7}";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Payload<'a> {
    state: &'static str,
    agent_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_input: Option<&'a str>,
    /// A `done` that marks a session starting or ending rather than a
    /// finished turn, so Orca raises no notice for it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    session_boundary: bool,
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
            tool_input: status
                .label
                .as_deref()
                .filter(|_| status.state.is_blocked()),
            session_boundary,
        })
    }

    fn release(&self) -> Option<String> {
        Some(Self::frame(&Payload {
            state: "done",
            agent_type: AGENT_TYPE,
            model: None,
            tool_input: None,
            session_boundary: true,
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

    fn metadata() -> PaneMetadata<'static> {
        PaneMetadata {
            title: TITLE.into(),
            model: MODEL.into(),
            context_percent: Some(42),
        }
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

    #[test_case(PaneState::Idle, r#"{"state":"done","agentType":"caudra","model":"claude-opus-4-5","sessionBoundary":true}"# ; "idle_is_a_session_boundary")]
    #[test_case(PaneState::Working, r#"{"state":"working","agentType":"caudra","model":"claude-opus-4-5"}"# ; "working")]
    #[test_case(PaneState::Blocked(Some(BlockKind::Permission)), r#"{"state":"blocked","agentType":"caudra","model":"claude-opus-4-5","toolInput":"Permission requested: shell"}"# ; "blocked_names_the_blocker")]
    #[test_case(PaneState::Done, r#"{"state":"done","agentType":"caudra","model":"claude-opus-4-5"}"# ; "done")]
    #[test_case(PaneState::Error, r#"{"state":"done","agentType":"caudra","model":"claude-opus-4-5"}"# ; "error_is_done")]
    fn each_state_is_one_frame_without_title_or_context(state: PaneState, json: &str) {
        assert_eq!(
            encode(state, Some(LABEL)),
            format!("\u{1b}]9999;{json}\u{7}")
        );
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

        assert_eq!(
            frame,
            "\u{1b}]9999;{\"state\":\"working\",\"agentType\":\"caudra\"}\u{7}"
        );
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
