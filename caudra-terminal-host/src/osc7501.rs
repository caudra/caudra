//! OSC 7501, the program status any terminal may implement. Caudra writes it
//! only after the terminal itself answered the query, so it is never wrapped
//! for a multiplexer, and it carries fixed text per state rather than anything
//! the session said.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::host::{InBandStatusEncoder, StatusChannel, TerminalHost};
use crate::skill::SkillSource;
use crate::status::{BlockKind, PaneMetadata, PaneState, PaneStatus};
use crate::workspaces::Workspaces;

const HOST: &str = "OSC 7501";
const APP: &str = "caudra";
const STRING_TERMINATOR: &str = "\u{1b}\\";

/// The terminal, which answered the OSC 7501 query.
#[derive(Clone, Copy, Debug)]
pub struct Osc7501Host;

impl TerminalHost for Osc7501Host {
    fn name(&self) -> &'static str {
        HOST
    }

    fn status_channel(&self) -> Option<StatusChannel> {
        Some(StatusChannel::InBand(Box::new(Osc7501Encoder)))
    }

    fn workspaces(&self) -> Option<Arc<dyn Workspaces>> {
        None
    }

    fn skill(&self) -> Option<SkillSource> {
        None
    }

    fn raises_notifications(&self) -> bool {
        true
    }

    fn names_tabs(&self) -> bool {
        false
    }
}

struct Osc7501Encoder;

impl InBandStatusEncoder for Osc7501Encoder {
    fn encode(&self, status: &PaneStatus, _: Option<&PaneMetadata<'_>>) -> String {
        sequence(status.state)
    }

    /// The final status written on exit is the farewell.
    fn release(&self) -> Option<String> {
        None
    }

    fn wraps_for_mux(&self) -> bool {
        false
    }
}

fn sequence(state: PaneState) -> String {
    let (state, kind, message) = match state {
        PaneState::Idle => ("idle", "", "Ready"),
        PaneState::Working => ("working", "", "Working"),
        PaneState::Blocked(None) => ("blocked", "", "Waiting for input"),
        PaneState::Blocked(Some(BlockKind::Permission)) => {
            ("blocked", ":kind=permission", "Waiting for permission")
        }
        PaneState::Blocked(Some(BlockKind::Question)) => {
            ("blocked", ":kind=question", "Waiting for an answer")
        }
        PaneState::Blocked(Some(BlockKind::Auth)) => {
            ("blocked", ":kind=auth", "Authentication required")
        }
        PaneState::Done => ("done", "", "Task complete"),
        PaneState::Error => ("error", "", "Task failed"),
    };
    let message = STANDARD.encode(message);
    format!("\u{1b}]7501;state={state}{kind}:app={APP}:msg={message}{STRING_TERMINATOR}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const LABEL: &str = "Permission requested: rm -rf /secret";
    const TITLE: &str = "Private session title";

    #[test_case(PaneState::Idle, "\x1b]7501;state=idle:app=caudra:msg=UmVhZHk=\x1b\\" ; "idle")]
    #[test_case(PaneState::Working, "\x1b]7501;state=working:app=caudra:msg=V29ya2luZw==\x1b\\" ; "working")]
    #[test_case(PaneState::Blocked(None), "\x1b]7501;state=blocked:app=caudra:msg=V2FpdGluZyBmb3IgaW5wdXQ=\x1b\\" ; "blocked")]
    #[test_case(PaneState::Blocked(Some(BlockKind::Permission)), "\x1b]7501;state=blocked:kind=permission:app=caudra:msg=V2FpdGluZyBmb3IgcGVybWlzc2lvbg==\x1b\\" ; "permission")]
    #[test_case(PaneState::Blocked(Some(BlockKind::Question)), "\x1b]7501;state=blocked:kind=question:app=caudra:msg=V2FpdGluZyBmb3IgYW4gYW5zd2Vy\x1b\\" ; "question")]
    #[test_case(PaneState::Blocked(Some(BlockKind::Auth)), "\x1b]7501;state=blocked:kind=auth:app=caudra:msg=QXV0aGVudGljYXRpb24gcmVxdWlyZWQ=\x1b\\" ; "auth")]
    #[test_case(PaneState::Done, "\x1b]7501;state=done:app=caudra:msg=VGFzayBjb21wbGV0ZQ==\x1b\\" ; "done")]
    #[test_case(PaneState::Error, "\x1b]7501;state=error:app=caudra:msg=VGFzayBmYWlsZWQ=\x1b\\" ; "error")]
    fn each_state_has_fixed_generic_text(state: PaneState, expected: &str) {
        let status = PaneStatus {
            label: Some(LABEL.into()),
            ..PaneStatus::new(state)
        };
        let metadata = PaneMetadata {
            title: TITLE.into(),
            model: "model".into(),
            context_percent: Some(5),
        };

        assert_eq!(Osc7501Encoder.encode(&status, Some(&metadata)), expected);
    }

    #[test]
    fn it_is_never_wrapped_and_has_no_release() {
        assert!(!Osc7501Encoder.wraps_for_mux());
        assert_eq!(Osc7501Encoder.release(), None);
    }
}
