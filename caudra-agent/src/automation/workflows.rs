//! `start_workflow()` and `workflow_finished` for automation firings. The runtime starts runs and
//! reads the session's settled runs through a [`Workflows`], which [`WorkflowHandle`] implements.
//! A start runs off the actor. A forwarder turns each run that settles into a `workflow_finished`
//! event keyed by the run and its execution epoch, so each attempt of a run fires once.

use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use caudra_automation::event::{
    MAX_REPORT_BYTES, WorkflowFinishedDetail, WorkflowStatus, cap_text,
};
use caudra_automation::host::{Failure, FailureKind, WorkflowRequest, WorkflowStarted};
use caudra_automation::untrusted::Untrusted;
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::workflow_scratch::WORKFLOW_SCRATCH_DIR;
use caudra_workflow::{
    LaunchRequest, RunSnapshot, RunStatus, WorkflowError, WorkflowRequest as RunRequest,
    WorkflowResponse, WorkflowState,
};
use futures_lite::stream::{self, Boxed, StreamExt};
use serde_json::Value;
use smol::Task;

use super::handle::Command;
use crate::workflow::WorkflowHandle;

const KEY_SEPARATOR: char = ':';
const REPORT_FIELD: &str = "report";
pub const UNEXPECTED_START: &str = "the workflow runtime answered a start with something else";
pub const NO_WORKFLOWS: &str = "workflows are not available in this session";

pub type StartFuture =
    Pin<Box<dyn Future<Output = Result<WorkflowStarted, WorkflowError>> + Send + 'static>>;
/// The runs that settled since the last item, oldest update first.
pub type SettleFeed = Boxed<Vec<RunSnapshot>>;

/// The session's workflow runtime as its automations reach it.
pub trait Workflows: Send + Sync {
    /// Returns at once: the future starts the run and answers its id and display name. It owns
    /// what it needs, so it runs off the actor.
    fn start(&self, request: WorkflowRequest) -> StartFuture;
    /// The runs that settle from now on, until the workflow runtime closes.
    fn observe(&self) -> SettleFeed;
    /// The runs as the workflow runtime publishes them now.
    fn state(&self) -> Arc<WorkflowState>;
}

/// A run that settled as its `workflow_finished` event shows it, with the key the event is
/// deduplicated by.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Settled {
    pub(super) key: String,
    pub(super) detail: WorkflowFinishedDetail,
}

impl Workflows for WorkflowHandle {
    fn start(&self, request: WorkflowRequest) -> StartFuture {
        let handle = self.clone();
        Box::pin(async move {
            let WorkflowRequest {
                name,
                args,
                agent_budget,
            } = request;
            let launch = LaunchRequest {
                name,
                args,
                agent_budget,
            };
            match handle.request(RunRequest::Start(launch)).await? {
                WorkflowResponse::Started(run) => Ok(WorkflowStarted {
                    run_id: run.run_id,
                    name: run.display_name,
                }),
                _ => Err(WorkflowError::Internal(UNEXPECTED_START.to_owned())),
            }
        })
    }

    fn observe(&self) -> SettleFeed {
        stream::unfold(self.observe_settled(), |mut feed| async move {
            let runs = feed.next().await?;
            Some((runs, feed))
        })
        .boxed()
    }

    fn state(&self) -> Arc<WorkflowState> {
        WorkflowHandle::state(self)
    }
}

/// What a start the runtime refused throws: the script's own arguments are `invalid_argument`,
/// a run the runtime will not start is `refused`, and a runtime that cannot answer, or answers
/// what a start never does, is `unavailable`.
pub(super) fn start_failure(error: &WorkflowError) -> Failure {
    let kind = match error {
        WorkflowError::Budget { .. } => FailureKind::InvalidArgument,
        WorkflowError::UnknownWorkflow { .. }
        | WorkflowError::Ambiguous { .. }
        | WorkflowError::TrustRequired { .. }
        | WorkflowError::Invalid { .. }
        | WorkflowError::TooManyRuns { .. }
        | WorkflowError::NotAdmitted(_) => FailureKind::Refused,
        WorkflowError::Unavailable
        | WorkflowError::Storage(_)
        | WorkflowError::Internal(_)
        | WorkflowError::UnknownRun { .. }
        | WorkflowError::InvalidTransition { .. } => FailureKind::Unavailable,
    };
    Failure::new(kind, error.to_string())
}

/// `<state>/workflow_scratch/<session>`, which holds a directory for each run of the session
/// that wrote a scratch file.
pub(super) fn scratch_root(state_dir: &StateDir, session_id: CaudraId) -> PathBuf {
    state_dir
        .path()
        .join(WORKFLOW_SCRATCH_DIR)
        .join(session_id.to_string())
}

/// Forwards each run that settles to the actor, until the feed ends or the actor stops.
pub(super) fn forward(
    mut feed: SettleFeed,
    scratch_root: PathBuf,
    commands: flume::Sender<Command>,
) -> Task<()> {
    smol::spawn(async move {
        while let Some(runs) = feed.next().await {
            for settled in runs.iter().filter_map(|run| settled(run, &scratch_root)) {
                if commands
                    .send(Command::WorkflowSettled(Box::new(settled)))
                    .is_err()
                {
                    return;
                }
            }
        }
    })
}

/// `run` as its `workflow_finished` event shows it, or `None` while it has not settled.
pub(super) fn settled(run: &RunSnapshot, scratch_root: &Path) -> Option<Settled> {
    let status = match run.status {
        RunStatus::Completed => WorkflowStatus::Completed,
        RunStatus::Failed => WorkflowStatus::Failed,
        RunStatus::Cancelled => WorkflowStatus::Cancelled,
        RunStatus::Interrupted => WorkflowStatus::Interrupted,
        RunStatus::Active | RunStatus::Paused | RunStatus::BudgetLimited => return None,
    };
    let (report, result) = report_and_result(run.result.as_ref());
    Some(Settled {
        key: event_key(&run.run_id, run.execution_epoch),
        detail: WorkflowFinishedDetail {
            run_id: run.run_id.clone(),
            name: run.display_name.clone(),
            workflow: run.workflow_name.clone(),
            status,
            report,
            result,
            error: run.error.clone().map(capped),
            scratch_dir: scratch_dir(scratch_root, &run.run_id),
            agents: run.usage.agents_admitted,
            tokens: run.usage.tokens_used,
        },
    })
}

/// The detail a stored `workflow_finished` event keyed `key` showed, rebuilt from `runs` while
/// the run it names is still terminal at the epoch the key names.
pub(super) fn settled_again(
    runs: &[RunSnapshot],
    key: &str,
    scratch_root: &Path,
) -> Option<WorkflowFinishedDetail> {
    let (run_id, epoch) = key.rsplit_once(KEY_SEPARATOR)?;
    let epoch: u64 = epoch.parse().ok()?;
    runs.iter()
        .find(|run| run.run_id == run_id && run.execution_epoch == epoch)
        .and_then(|run| settled(run, scratch_root))
        .map(|settled| settled.detail)
}

fn event_key(run_id: &str, execution_epoch: u64) -> String {
    format!("{run_id}{KEY_SEPARATOR}{execution_epoch}")
}

/// A string `report` field becomes the capped report, and the rest of the result stays
/// structured unless its JSON outgrows the cap, which leaves capped text of it.
fn report_and_result(result: Option<&Value>) -> (Option<Untrusted>, Option<Untrusted>) {
    let Some(result) = result else {
        return (None, None);
    };
    let mut rest = result.clone();
    let report = rest
        .as_object_mut()
        .filter(|object| object.get(REPORT_FIELD).is_some_and(Value::is_string))
        .and_then(|object| object.remove(REPORT_FIELD))
        .and_then(|report| serde_json::from_value(report).ok())
        .map(capped);
    let text = rest.to_string();
    let rest = if text.len() > MAX_REPORT_BYTES {
        capped(text)
    } else {
        Untrusted::Json(rest)
    };
    (report, Some(rest))
}

fn capped(text: String) -> Untrusted {
    Untrusted::Text(cap_text(text, MAX_REPORT_BYTES).0)
}

/// The run's directory under `scratch_root`, unless its id cannot name one.
fn scratch_dir(scratch_root: &Path, run_id: &str) -> Option<String> {
    (Path::new(run_id).file_name() == Some(OsStr::new(run_id)))
        .then(|| scratch_root.join(run_id).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use caudra_automation::event::CUT_MARKER;
    use caudra_workflow::RunUsage;
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::automation::testing::settled_run;

    const RUN_ID: &str = "run-1";
    const WORKFLOW: &str = "review-changes";
    const DISPLAY_NAME: &str = "review-changes-2";
    const REPORT: &str = "All green.";
    const ERROR: &str = "the reviewer gave up";
    const SCRATCH_ROOT: &str = "/state/workflow_scratch/session";
    const EPOCH: u64 = 2;
    const AGENTS: u32 = 3;
    const TOKENS: u64 = 4_096;
    const VERDICT: &str = "verdict";
    const PASSED: &str = "passed";
    const ONE_CUT: &str = "a capped text must end with the cut marker";

    fn run(status: RunStatus, result: Option<Value>) -> RunSnapshot {
        RunSnapshot {
            display_name: DISPLAY_NAME.to_owned(),
            result,
            error: Some(ERROR.to_owned()),
            usage: RunUsage {
                agents_admitted: AGENTS,
                tokens_used: TOKENS,
            },
            ..settled_run(RUN_ID, WORKFLOW, status, EPOCH)
        }
    }

    fn start_request() -> WorkflowRequest {
        WorkflowRequest {
            name: WORKFLOW.to_owned(),
            args: json!({}),
            agent_budget: None,
        }
    }

    fn detail_of(run: &RunSnapshot) -> Option<WorkflowFinishedDetail> {
        settled(run, Path::new(SCRATCH_ROOT)).map(|settled| settled.detail)
    }

    fn cut(text: &Untrusted) -> bool {
        matches!(text, Untrusted::Text(text) if text.len() == MAX_REPORT_BYTES && text.ends_with(CUT_MARKER))
    }

    #[test]
    fn a_settled_run_maps_every_field_and_keys_by_run_and_epoch() {
        let result = json!({ REPORT_FIELD: REPORT, VERDICT: PASSED });

        let settled = settled(
            &run(RunStatus::Completed, Some(result)),
            Path::new(SCRATCH_ROOT),
        );

        assert_eq!(
            settled,
            Some(Settled {
                key: format!("{RUN_ID}{KEY_SEPARATOR}{EPOCH}"),
                detail: WorkflowFinishedDetail {
                    run_id: RUN_ID.to_owned(),
                    name: DISPLAY_NAME.to_owned(),
                    workflow: WORKFLOW.to_owned(),
                    status: WorkflowStatus::Completed,
                    report: Some(Untrusted::text(REPORT)),
                    result: Some(Untrusted::Json(json!({ VERDICT: PASSED }))),
                    error: Some(Untrusted::text(ERROR)),
                    scratch_dir: Some(format!("{SCRATCH_ROOT}/{RUN_ID}")),
                    agents: AGENTS,
                    tokens: TOKENS,
                },
            })
        );
    }

    #[test_case(RunStatus::Completed => Some(WorkflowStatus::Completed); "completed")]
    #[test_case(RunStatus::Failed => Some(WorkflowStatus::Failed); "failed")]
    #[test_case(RunStatus::Cancelled => Some(WorkflowStatus::Cancelled); "cancelled")]
    #[test_case(RunStatus::Interrupted => Some(WorkflowStatus::Interrupted); "interrupted")]
    #[test_case(RunStatus::Active => None; "active")]
    #[test_case(RunStatus::Paused => None; "paused")]
    #[test_case(RunStatus::BudgetLimited => None; "budget_limited")]
    fn only_a_terminal_status_settles(status: RunStatus) -> Option<WorkflowStatus> {
        detail_of(&run(status, None)).map(|detail| detail.status)
    }

    #[test]
    fn the_report_and_the_error_are_capped() {
        let long = "r".repeat(MAX_REPORT_BYTES * 2);
        let mut settled = run(RunStatus::Failed, Some(json!({ REPORT_FIELD: long })));
        settled.error = Some(long);

        let detail = detail_of(&settled).unwrap();

        assert!(detail.report.as_ref().is_some_and(cut), "{ONE_CUT}");
        assert!(detail.error.as_ref().is_some_and(cut), "{ONE_CUT}");
        assert_eq!(detail.result, Some(Untrusted::Json(json!({}))));
    }

    #[test]
    fn an_oversized_result_becomes_capped_text_of_its_json() {
        let long = "x".repeat(MAX_REPORT_BYTES);
        let detail = detail_of(&run(
            RunStatus::Completed,
            Some(json!({ REPORT_FIELD: REPORT, VERDICT: long })),
        ))
        .unwrap();

        let result = detail.result.unwrap();
        assert!(cut(&result), "{ONE_CUT}");
        assert!(matches!(&result, Untrusted::Text(text) if !text.contains(REPORT_FIELD)));
        assert_eq!(detail.report, Some(Untrusted::text(REPORT)));
    }

    #[test_case(json!({ REPORT_FIELD: 1 }); "a_report_that_is_not_text")]
    #[test_case(json!([REPORT]); "a_result_that_is_not_an_object")]
    #[test_case(json!(REPORT); "a_bare_string_result")]
    fn a_result_without_a_text_report_field_stays_whole(result: Value) {
        let detail = detail_of(&run(RunStatus::Completed, Some(result.clone()))).unwrap();

        assert_eq!(
            (detail.report, detail.result),
            (None, Some(Untrusted::Json(result)))
        );
    }

    #[test_case(RUN_ID => Some(format!("{SCRATCH_ROOT}/{RUN_ID}")); "a_plain_id")]
    #[test_case("a/b" => None; "a_nested_id")]
    #[test_case(".." => None; "the_parent")]
    #[test_case("" => None; "an_empty_id")]
    fn the_scratch_directory_is_the_runs_own(run_id: &str) -> Option<String> {
        scratch_dir(Path::new(SCRATCH_ROOT), run_id)
    }

    #[test_case(RunStatus::Completed, EPOCH, EPOCH => true; "terminal_at_the_same_epoch")]
    #[test_case(RunStatus::Completed, EPOCH + 1, EPOCH => false; "resumed_under_a_new_epoch")]
    #[test_case(RunStatus::Active, EPOCH, EPOCH => false; "running_again")]
    fn a_stored_event_rebuilds_only_while_its_run_is_terminal_at_its_epoch(
        status: RunStatus,
        epoch_now: u64,
        epoch_stored: u64,
    ) -> bool {
        let runs = [settled_run(RUN_ID, WORKFLOW, status, epoch_now)];

        settled_again(
            &runs,
            &event_key(RUN_ID, epoch_stored),
            Path::new(SCRATCH_ROOT),
        )
        .is_some()
    }

    #[test_case("other-run:2"; "an_unknown_run")]
    #[test_case(RUN_ID; "a_key_without_an_epoch")]
    fn a_stored_event_naming_no_run_is_not_rebuilt(key: &str) {
        let runs = [settled_run(RUN_ID, WORKFLOW, RunStatus::Completed, EPOCH)];

        assert_eq!(settled_again(&runs, key, Path::new(SCRATCH_ROOT)), None);
    }

    #[test]
    fn the_handle_starts_a_run_and_answers_its_id_and_display_name() {
        smol::block_on(async {
            let handle = WorkflowHandle::scripted(|request| match request {
                RunRequest::Start(launch) => Ok(WorkflowResponse::Started(Box::new(RunSnapshot {
                    display_name: DISPLAY_NAME.to_owned(),
                    ..settled_run(RUN_ID, &launch.name, RunStatus::Active, 0)
                }))),
                _ => Ok(WorkflowResponse::Ack),
            });

            let started = Workflows::start(&handle, start_request()).await;

            assert_eq!(
                started,
                Ok(WorkflowStarted {
                    run_id: RUN_ID.to_owned(),
                    name: DISPLAY_NAME.to_owned(),
                })
            );
        });
    }

    #[test]
    fn the_handle_reads_any_other_answer_to_a_start_as_internal() {
        smol::block_on(async {
            let handle = WorkflowHandle::scripted(|_| Ok(WorkflowResponse::Ack));

            let started = Workflows::start(&handle, start_request()).await;

            assert_eq!(
                started,
                Err(WorkflowError::Internal(UNEXPECTED_START.to_owned()))
            );
        });
    }
}
