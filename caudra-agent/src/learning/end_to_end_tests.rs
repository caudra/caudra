//! One learner run from a forced wake to its applied report, with a scripted
//! provider in place of the model and a runner that makes the `history.search`
//! call a snippet would. It runs in a child process whose XDG directories are
//! its own, because `history` resolves the process's state directory.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use caudra_config::{CODE_TOOL_NAME, FeatureFlags, LearningTarget, PermissionsConfig};
use caudra_providers::provider::{BoxFuture, Provider};
use caudra_providers::{
    AgentError, CacheKey, ContentBlock, Message, Model, ModelInfo, ProviderEvent, RequestOptions,
    Role, StopReason, StreamResponse, TokenUsage,
};
use caudra_storage::StateDir;
use caudra_storage::history_search::{HistoryQuery, HistoryScope};
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::learning::LearningStore;
use caudra_storage::memory_journal::EntryOrigin;
use caudra_storage::paths::canonicalize_clean;
use caudra_storage::sessions::SessionDatabase;
use caudra_storage::usage_ledger::{LedgerPurpose, UsageLedger};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

use super::digest::KINDS;
use super::pump::LEARNED;
use super::runner::Spend;
use super::targets::{BLOCK_OPEN, bullets};
use super::{LearnedNotice, Learner, LearnerActivity, LearnerDeps, LearnerStep};
use crate::AgentMode;
use crate::agent::subagent::STRUCTURED_OUTPUT_TOOL;
use crate::agent::task_runner::{ModelResolver, WorkflowHostContext};
use crate::cancel::CancelMap;
use crate::code::names::HISTORY_MODULE;
use crate::code::runner::{
    RunFuture, ScriptCall, ScriptInvocation, ScriptOutcome, ScriptOutput, ScriptReply, ScriptRun,
    ScriptRunner, ScriptStatus, install,
};
use crate::history::HistoryProject;
use crate::history::tests::{seed, seeded_text, stamped};
use crate::memory::compactor::Ledger;
use crate::memory::store::{MemoryStore, now_ms};
use crate::permissions::{PermissionManager, PermissionMode};
use crate::tools::native;
use crate::tools::test_support::stub_ctx_with_permissions;
use crate::types::{AgentEvent, EventSender};

const ISOLATED_ENV: &str = "CAUDRA_LEARNING_E2E_CHILD";
const TEST_NAME: &str = "learning::end_to_end_tests::a_forced_run_learns_from_two_sessions";
const XDG_DIRS: [&str; 4] = [
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
];
const QUERY: &str = "nextest";
const SEARCH_METHOD: &str = "search";
const SNIPPET: &str = "history.search(\"nextest\")";
const RULE: &str = "Run tests with cargo nextest, never cargo test";
const NOTE: &str = "testing.md";
const NOTE_BODY: &str = "# Testing\n\ncargo nextest is the test runner here.";
const SUMMARY: &str = "learned the zephyrine test runner";
const LEARNER_ONLY_WORD: &str = "zephyrine";
const HAND_WRITTEN: &str = "# Project\n";
const INSTRUCTION_FILE: &str = "AGENTS.md";
const HOUR_MS: u64 = 3_600_000;
const TIMELINE_LIMIT: usize = 64;
const RUN_WAIT: Duration = Duration::from_secs(60);
const STARTED: &str = "started";
const TOOL_STARTED: &str = "tool started";
const INDEX_BUDGET: Duration = Duration::from_secs(5);
const TURN: TokenUsage = TokenUsage {
    input: 100,
    output: 20,
    cache_creation: 0,
    cache_read: 0,
};

/// Answers each request with the next scripted response, keeping what it was sent.
struct ScriptedProvider {
    responses: Mutex<Vec<StreamResponse>>,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl Provider for ScriptedProvider {
    fn stream_message<'a>(
        &'a self,
        _: &'a Model,
        messages: &'a [Message],
        _: &'a str,
        _: &'a Value,
        _: &'a flume::Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&'a CacheKey>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(messages.to_vec());
            let mut responses = self.responses.lock().unwrap();
            match responses.is_empty() {
                true => Err(AgentError::Config {
                    message: "script exhausted".into(),
                }),
                false => Ok(responses.remove(0)),
            }
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// Runs any snippet as one `history.search(QUERY)`, asking again when the first call raises with
/// the module's reference, and returns what the search answered.
struct HistoryRunner;

impl ScriptRunner for HistoryRunner {
    fn run(&self, run: ScriptRun) -> RunFuture {
        Box::pin(async move {
            let search = || ScriptCall {
                namespace: Some(HISTORY_MODULE.to_owned()),
                name: SEARCH_METHOD.to_owned(),
                args: vec![Value::from(QUERY)],
                kwargs: Map::new(),
            };
            let reply = match answer(run.host.call(search())).await {
                Ok(found) => Ok(found),
                Err(_) => answer(run.host.call(search())).await,
            };
            ScriptOutcome {
                output: Some(ScriptOutput {
                    result: reply.unwrap_or_else(|error| Value::from(error.message)),
                    diagnostic: None,
                    ..ScriptOutput::refused(ScriptStatus::Completed, String::new())
                }),
                ..ScriptOutcome::default()
            }
        })
    }
}

async fn answer(invocation: ScriptInvocation) -> ScriptReply {
    match invocation {
        ScriptInvocation::Ready(reply) => reply,
        ScriptInvocation::Deferred(future) => future.await,
    }
}

fn response(content: Vec<ContentBlock>, stop: StopReason) -> StreamResponse {
    StreamResponse {
        message: Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        },
        usage: TURN,
        stop_reason: Some(stop),
        ..Default::default()
    }
}

/// The learner's script: one `code` call that searches history, then its report citing an item
/// from each of the two sessions, then an empty close.
fn script(evidence: &[String]) -> Vec<StreamResponse> {
    let report = json!({
        "memory": [{"name": NOTE, "content": NOTE_BODY, "evidence": evidence}],
        "instructions": {"project": {"bullets": [{"text": RULE, "evidence": evidence}]}},
        "summary": SUMMARY,
    });
    vec![
        response(
            vec![ContentBlock::tool_use(
                "c1",
                CODE_TOOL_NAME,
                json!({"code": SNIPPET}),
            )],
            StopReason::ToolUse,
        ),
        response(
            vec![ContentBlock::tool_use("r1", STRUCTURED_OUTPUT_TOOL, report)],
            StopReason::ToolUse,
        ),
        response(
            vec![ContentBlock::Text {
                text: String::new(),
            }],
            StopReason::EndTurn,
        ),
    ]
}

/// Re-runs this test in a child whose home and XDG directories are `home`.
fn run_isolated(home: &Path) {
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env_clear()
        .env(ISOLATED_ENV, "1")
        .env("HOME", home)
        .current_dir(home);
    for name in XDG_DIRS {
        command.env(name, home);
    }
    assert!(command.status().unwrap().success());
}

/// Two saved sessions of `project`, an hour old, each with two typed messages about `QUERY`.
fn seed_sessions(state: &StateDir, project: &str) -> Vec<CaudraId> {
    let base = u64::try_from(now_ms()).unwrap() - HOUR_MS;
    (0..2)
        .map(|session| {
            let items: Vec<(u64, String)> = (0..2)
                .map(|message| {
                    let index = session * 2 + message;
                    (stamped(base, index), seeded_text(QUERY, index))
                })
                .collect();
            seed(state.path(), project, &items)
        })
        .collect()
}

/// One reference into each seeded session, as the learner would cite them.
fn evidence(state: &StateDir, project: &Path) -> Vec<String> {
    let database = SessionDatabase::open_state(state).unwrap();
    let scope = HistoryScope::Project(HistoryProject::local(project).scope);
    let items = database
        .history_timeline(
            &scope,
            &KINDS,
            0,
            u64::try_from(now_ms()).unwrap(),
            TIMELINE_LIMIT,
        )
        .unwrap();
    let mut cited: Vec<String> = Vec::new();
    let mut sessions = Vec::new();
    for item in items {
        if !sessions.contains(&item.reference.session) {
            sessions.push(item.reference.session);
            cited.push(item.reference.to_string());
        }
    }
    cited
}

fn hits(state: &StateDir, text: &str) -> usize {
    let database = SessionDatabase::open_state(state).unwrap();
    database
        .catch_up_history(&HistoryScope::All, Instant::now() + INDEX_BUDGET)
        .unwrap();
    database
        .search_history(&HistoryQuery::new(text, HistoryScope::All))
        .unwrap()
        .hits
        .len()
}

/// What the learner's `code` call answered, as its tool result.
fn code_results(requests: &[Vec<Message>]) -> String {
    requests
        .iter()
        .flatten()
        .flat_map(|message| &message.content)
        .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .map(|block| serde_json::to_string(block).unwrap())
        .collect()
}

/// A forced run reads two sessions through `history.search`, reports, and lands the report:
/// the project block, a memory note journaled as the run's, a `Learned` event, its spend under
/// `learning`, and an advanced cursor, while its own conversation stays out of history.
#[test]
fn a_forced_run_learns_from_two_sessions() {
    if env::var_os(ISOLATED_ENV).is_none() {
        run_isolated(TempDir::new().unwrap().path());
        return;
    }
    install(Arc::new(HistoryRunner));
    let home = env::var_os("HOME").unwrap();
    let project_dir = TempDir::new_in(&home).unwrap();
    let config_dir = TempDir::new_in(&home).unwrap();
    let project = canonicalize_clean(project_dir.path());
    let project_text = project.display().to_string();
    fs::write(project.join(INSTRUCTION_FILE), HAND_WRITTEN).unwrap();
    let state = StateDir::resolve().unwrap();
    let sessions = seed_sessions(&state, &project_text);
    let cited = evidence(&state, &project);
    assert_eq!(cited.len(), sessions.len());

    let permissions = Arc::new(PermissionManager::new_nonpersistent(
        PermissionsConfig::default(),
        project.clone(),
        Arc::default(),
    ));
    permissions.set_session_mode(Some(PermissionMode::Yolo));
    let mut ctx = stub_ctx_with_permissions(&AgentMode::Build, permissions);
    let requests = Arc::default();
    ctx.provider = Arc::new(ScriptedProvider {
        responses: Mutex::new(script(&cited)),
        requests: Arc::clone(&requests),
    });
    ctx.session_id = Some(SessionRef::from_id(sessions[0]));
    native::register(&ctx.registry, FeatureFlags::default()).unwrap();
    let model: ModelResolver = Arc::new({
        let (provider, model) = (Arc::clone(&ctx.provider), Arc::clone(&ctx.model));
        move || (Arc::clone(&provider), Arc::clone(&model))
    });
    let host = WorkflowHostContext::from_tool_context(
        &ctx,
        model,
        Arc::new(|| AgentMode::Build),
        Arc::new(CancelMap::new()),
    );
    let memory = MemoryStore::for_cwd(&state, &project).unwrap();
    let (events_tx, events) = flume::unbounded();
    let learner = Learner::start(LearnerDeps {
        state: state.clone(),
        cwd: project.clone(),
        config_dir: Some(config_dir.path().to_path_buf()),
        host,
        memory: Some(Arc::clone(&memory)),
        targets: vec![LearningTarget::Project, LearningTarget::Memory],
        spend: Spend {
            events: EventSender::new(events_tx, 0),
            ledger: Some(Ledger {
                state: state.clone(),
                cwd: project_text.clone(),
            }),
        },
    });

    assert!(learner.handle().run_now());
    let mut usage = Vec::new();
    let mut watched = Vec::new();
    let notice: LearnedNotice = loop {
        match events.recv_timeout(RUN_WAIT).unwrap().event {
            AgentEvent::Learned(notice) => break *notice,
            AgentEvent::ModelUsage { purpose, .. } => usage.push(purpose),
            AgentEvent::LearnerActivity(activity) => {
                let LearnerActivity { run_id, step } = *activity;
                let step = match step {
                    LearnerStep::Started { .. } => STARTED.to_owned(),
                    LearnerStep::Event(event) if matches!(*event, AgentEvent::ToolStart(_)) => {
                        TOOL_STARTED.to_owned()
                    }
                    LearnerStep::Event(_) => continue,
                    LearnerStep::Finished { outcome } => outcome,
                };
                watched.push((run_id, step));
            }
            _ => {}
        }
    };

    let store = LearningStore::open(&state).unwrap();
    let run = store.runs(learner.handle().project(), 1).unwrap().remove(0);
    assert_eq!(run.outcome, LEARNED, "{}", run.summary);
    assert_eq!(
        (notice.run_id, notice.project, notice.notes),
        (run.id, 1, 1)
    );
    assert_eq!(notice.summary, SUMMARY);
    assert!(watched.iter().all(|(run_id, _)| *run_id == run.id));
    let steps: Vec<&str> = watched.iter().map(|(_, step)| step.as_str()).collect();
    assert_eq!(steps.first(), Some(&STARTED), "{steps:?}");
    assert!(steps.contains(&TOOL_STARTED), "{steps:?}");
    assert_eq!(steps.last(), Some(&LEARNED), "{steps:?}");
    let transcript = store.transcript(run.id).unwrap().unwrap_or_default();
    assert!(transcript.contains(LEARNER_ONLY_WORD), "{transcript}");
    let searched = code_results(&requests.lock().unwrap());
    assert!(
        cited.iter().all(|cited| searched.contains(cited)),
        "{searched}"
    );

    let written = fs::read_to_string(project.join(INSTRUCTION_FILE)).unwrap();
    assert!(written.starts_with(HAND_WRITTEN) && written.contains(BLOCK_OPEN));
    assert_eq!(bullets(&written), vec![RULE.to_owned()]);
    let entries = memory.journal().current(memory.scope()).unwrap();
    assert_eq!(entries[0].meta.origin, EntryOrigin::Learner(run.id));

    assert!(!usage.is_empty());
    assert!(
        usage
            .iter()
            .all(|purpose| *purpose == LedgerPurpose::Learning)
    );
    let ledger = UsageLedger::open(&state).unwrap().buckets(None).unwrap();
    assert!(!ledger.is_empty());
    assert!(
        ledger
            .iter()
            .all(|bucket| bucket.purpose == LedgerPurpose::Learning.storage_name())
    );

    let cursor = store.project(learner.handle().project()).unwrap().unwrap();
    assert!(cursor.through_ms > 0);
    assert_eq!(cursor.through_ms, run.through_ms);
    assert_eq!(hits(&state, LEARNER_ONLY_WORD), 0);
    assert!(hits(&state, QUERY) > 0);
}
