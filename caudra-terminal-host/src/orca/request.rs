//! What Orca asks of the agent in a pane, written into the pane's input as
//! `OSC 9999 ; <json> BEL`. Orca sends one only for a child row the status
//! marked `openable`, so an older Caudra never receives it as typing.

use serde::Deserialize;

/// Whether this build reads Orca's requests. The Windows console turns an
/// escape sequence written into a pane's input into keystrokes, so there the
/// request would be typed into the composer.
pub(crate) const READS_REQUESTS: bool = cfg!(not(windows));

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum HostRequest {
    /// The user clicked the child row of this subagent or background task.
    OpenSubagent(String),
    /// The user clicked the agent's own row: `{"openMain":{}}`.
    OpenMain {},
}

impl HostRequest {
    /// The request a host message carries; anything else is ignored rather
    /// than guessed at.
    pub fn parse(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const TASK_ID: &str = "toolu_01";

    #[test]
    fn open_subagent_names_the_task() {
        assert_eq!(
            HostRequest::parse(r#"{"openSubagent":"toolu_01"}"#),
            Some(HostRequest::OpenSubagent(TASK_ID.to_owned()))
        );
    }

    #[test]
    fn open_main_has_no_arguments() {
        assert_eq!(
            HostRequest::parse(r#"{"openMain":{}}"#),
            Some(HostRequest::OpenMain {})
        );
    }

    #[test_case(r#"{"closeSubagent":"toolu_01"}"#; "unknown_request")]
    #[test_case(r#"{"openMain":{"task":"toolu_01"}}"#; "open_main_with_arguments")]
    #[test_case(r#"{"openSubagent":1}"#; "id_not_a_string")]
    #[test_case(r#"{"openSubagent":"toolu_01","extra":true}"#; "extra_field")]
    #[test_case("open toolu_01"; "not_json")]
    fn anything_else_is_ignored(text: &str) {
        assert_eq!(HostRequest::parse(text), None);
    }
}
