//! The control surface a workflow runtime exposes to its callers. Requests,
//! responses, and errors all serialize, so an SDK can carry them unchanged.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::catalog::{LaunchRequest, WorkflowCatalog};
use crate::snapshot::{
    RunCallBody, RunDetail, RunHistoryEntry, RunSnapshot, RunStatus, SourceKind,
};

const SCOPE_SEPARATOR: &str = ", ";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum WorkflowRequest {
    List,
    Validate {
        name: String,
    },
    Start(LaunchRequest),
    Status {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
    },
    /// One run with its journal and timeline, from any session.
    Inspect {
        run_id: String,
    },
    /// Untruncated request and result text for one journaled call, or for
    /// every call of the run when `call_key` is absent.
    CallBodies {
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_key: Option<u64>,
    },
    /// Recent runs of other sessions, newest first.
    History {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    Pause {
        run_id: String,
    },
    Resume {
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_budget: Option<u32>,
    },
    Stop {
        run_id: String,
    },
    Trust {
        name: String,
        digest: String,
    },
    /// Marks the completion notice for `revision` delivered. A run that has
    /// moved past `revision` keeps its notice pending and answers `Acked(false)`.
    AckCompletion {
        run_id: String,
        revision: u64,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum WorkflowResponse {
    Catalog(WorkflowCatalog),
    Validation {
        name: String,
        ok: bool,
        report: String,
    },
    Started(Box<RunSnapshot>),
    Runs(Vec<RunSnapshot>),
    Run(Box<RunSnapshot>),
    Detail(Box<RunDetail>),
    CallBodies(Vec<RunCallBody>),
    History(Vec<RunHistoryEntry>),
    Trusted {
        name: String,
    },
    Acked(bool),
    Ack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum WorkflowError {
    #[error("workflow runtime is unavailable")]
    Unavailable,
    #[error("unknown workflow {name:?}")]
    UnknownWorkflow { name: String },
    #[error("workflow {name:?} is declared more than once in the {} scope", scope_list(.scopes))]
    Ambiguous {
        name: String,
        scopes: Vec<SourceKind>,
    },
    #[error("workflow {name:?} at {} is not trusted (digest {digest})", path.display())]
    TrustRequired {
        name: String,
        digest: String,
        path: PathBuf,
    },
    #[error("workflow {name:?} is invalid: {error}")]
    Invalid { name: String, error: String },
    #[error("agent budget {requested} exceeds the maximum of {max}")]
    Budget { requested: u32, max: u32 },
    #[error("at most {max} workflow runs may be active at once")]
    TooManyRuns { max: usize },
    #[error("unknown workflow run {run_id:?}")]
    UnknownRun { run_id: String },
    #[error("workflow run {run_id:?} is {status} and cannot take that action")]
    InvalidTransition { run_id: String, status: RunStatus },
    #[error("workflow storage: {0}")]
    Storage(String),
    #[error("workflow runtime: {0}")]
    Internal(String),
}

fn scope_list(scopes: &[SourceKind]) -> String {
    scopes
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>()
        .join(SCOPE_SEPARATOR)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NAME: &str = "review";
    const AMBIGUOUS_MESSAGE: &str =
        "workflow \"review\" is declared more than once in the project, user scope";

    #[test]
    fn requests_are_adjacently_tagged() {
        let request = WorkflowRequest::Start(LaunchRequest {
            name: NAME.into(),
            args: json!({"branch": "main"}),
            agent_budget: Some(4),
        });

        let json = serde_json::to_value(&request).unwrap();

        assert_eq!(json["kind"], "start");
        assert_eq!(json["detail"]["name"], NAME);
        let back: WorkflowRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, request);
        assert_eq!(
            serde_json::to_value(WorkflowRequest::List).unwrap(),
            json!({"kind": "list"})
        );
    }

    #[test]
    fn responses_carry_sequences() {
        let response = WorkflowResponse::Runs(Vec::new());

        let json = serde_json::to_value(&response).unwrap();

        assert_eq!(json, json!({"kind": "runs", "detail": []}));
        let back: WorkflowResponse = serde_json::from_value(json).unwrap();
        assert_eq!(back, response);
    }

    #[test]
    fn errors_round_trip_and_read_well() {
        let error = WorkflowError::Ambiguous {
            name: NAME.into(),
            scopes: vec![SourceKind::Project, SourceKind::User],
        };

        let json = serde_json::to_value(&error).unwrap();

        assert_eq!(error.to_string(), AMBIGUOUS_MESSAGE);
        assert_eq!(json["kind"], "ambiguous");
        assert_eq!(json["detail"]["scopes"], json!(["project", "user"]));
        let back: WorkflowError = serde_json::from_value(json).unwrap();
        assert_eq!(back, error);
        assert_eq!(
            serde_json::to_value(WorkflowError::Storage("locked".into())).unwrap(),
            json!({"kind": "storage", "detail": "locked"})
        );
    }
}
