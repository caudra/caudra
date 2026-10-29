//! The log file is the one artefact a user pastes into a bug report, so the
//! privacy defaults have to hold there as well as at the collector: with
//! nothing opted in, prompt text and tool input must never be written. A
//! secret the user hands over must stay out of every artefact as well, while
//! still reaching the shell commands that use it. Owns the process-wide
//! subscriber, so it lives in its own test binary.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use caudra_agent::permissions::PermissionManager;
use caudra_agent::prompt::ResolvedSlots;
use caudra_agent::prompt::profile::{BUILTIN_PROFILE_NAME, PromptProfileCatalog};
use caudra_agent::secrets::scope;
use caudra_agent::secrets::{SecretScope, SecretValue, SecretVaults};
use caudra_agent::template;
use caudra_agent::tools::native::secret::SecretReply;
use caudra_agent::tools::{
    FileReadTracker, PathLocks, REQUEST_SECRET_TOOL_NAME, ToolAudience, ToolFilter, ToolRegistry,
    native,
};
use caudra_agent::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, CancelMap,
    Envelope, EventSender, History, PermissionsConfig, StoredSession, SubagentHistoryStore,
    ToolOutputLines,
};
use caudra_config::config_file::CONFIG_FILE;
use caudra_config::{DefaultEffect, FeatureFlags, ModelPolicy, TelemetryConfig};
use caudra_otel::emit::{self, ToolResult};
use caudra_providers::model::Model;
use caudra_providers::provider::{BoxFuture, Provider};
use caudra_providers::{
    AgentError, CacheKey, ContentBlock, Message, ModelInfo, ProviderEvent, RequestOptions, Role,
    StopReason, StreamResponse, Timeouts, TokenUsage,
};
use caudra_storage::StateDir;
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::log::record::{Entry, Level};
use caudra_storage::sessions::{PermissionMode, SESSIONS_DB_FILE};
use caudra_storage::tool_outputs::ToolOutputStore;
use caudra_workcell::WorkcellHost;
use flume::Sender;
use futures_lite::future;
use serde_json::{Value, json};
use tempfile::TempDir;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Multi-byte on purpose: a length written in bytes would leak the shape of the
/// prompt even when the text itself does not escape.
const SECRET_PROMPT: &str = "the 秘密 prompt";
const SECRET_INPUT: &str = "cat /etc/shadow";
const TOOL_NAME: &str = "shell";
const TOOL_SOURCE: &str = "builtin";
const PROMPT_LENGTH: &str = "prompt_length";
const TOOL_INPUT: &str = "tool_input";
const PROMPT: &str = "prompt";
const NO_RECORDS: &str = "the file layer wrote nothing";

const CANARY: &str = "canary-7f3c9e1d-saved-by-the-card";
const REPLACED: &str = "canary-52ab804e-replaced-in-secrets";
const SECRET_NAME: &str = "CAUDRA_PRIVACY_TOKEN";
const SHOWN_SECRET: &str = "${CAUDRA_PRIVACY_TOKEN}";
const SECRET_LABEL: &str = "Privacy token";
const SECRET_REASON: &str = "the deploy reads it";
const WITNESS_FILE: &str = "witness";
const SECRETS_DIR: &str = "secrets";
/// Enough lines that the result is cut down and its whole text saved.
const FILLER_LINES: usize = 4000;
const MODEL_SPEC: &str = "anthropic/claude-sonnet-4-5";
const DEPLOY_PROMPT: &str = "deploy with my token";
const DONE_TEXT: &str = "done";
const INPUT_TOKENS: u32 = 120;
const OUTPUT_TOKENS: u32 = 8;
const CONSOLE_EXPORTER: &str = "console";
const OTEL_EXPORT: &str = "otel console export";
const OTEL_FLUSH: Duration = Duration::from_secs(5);
const LEAKED: &str = "a secret's value escaped into";
const NOT_INJECTED: &str = "the shell child did not receive the secret";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("capture").clone()).expect("utf-8 log")
    }
}

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("capture").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The process-wide log file, with telemetry exporting to it as the console
/// exporter does, so one capture holds both.
fn capture() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = Capture::default();
        let writer = capture.clone();
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(move || writer.clone())
                    .with_filter(tracing_subscriber::EnvFilter::new(Level::Trace.as_str())),
            )
            .with(caudra_otel::layer())
            .init();
        caudra_otel::init(&TelemetryConfig {
            enabled: Some(true),
            logs_exporter: Some(CONSOLE_EXPORTER.into()),
            metrics_exporter: Some(CONSOLE_EXPORTER.into()),
            ..TelemetryConfig::default()
        })
        .expect("console telemetry");
        capture
    })
}

#[test]
fn prompt_text_and_tool_input_never_reach_the_log_file() {
    let capture = capture();

    emit::user_prompt(SECRET_PROMPT);
    emit::tool_result(&ToolResult {
        tool_name: TOOL_NAME,
        tool_source: TOOL_SOURCE,
        success: true,
        duration: Duration::from_millis(5),
        error_type: None,
        tool_input: Some(SECRET_INPUT),
    });

    let text = capture.text();
    assert!(!text.contains("秘密"), "prompt text leaked: {text}");
    assert!(!text.contains(SECRET_INPUT), "tool input leaked: {text}");

    // Every line the writer produces has to survive the reader the modal and
    // `caudra logs` share, or the log is unreadable in the places that matter.
    let records: Vec<Entry> = text.lines().map(Entry::parse).collect();
    assert!(!records.is_empty(), "{NO_RECORDS}");
    for entry in &records {
        let Entry::Record(record) = entry else {
            panic!("the writer emitted a line the reader cannot parse: {text}");
        };
        assert!(!record.fields.iter().any(|(key, _)| key == PROMPT));
        assert!(!record.fields.iter().any(|(key, _)| key == TOOL_INPUT));
    }

    let fields: Vec<(String, String)> = records
        .iter()
        .filter_map(|entry| match entry {
            Entry::Record(record) => Some(record.fields.clone()),
            Entry::Raw(_) => None,
        })
        .flatten()
        .collect();
    assert!(
        fields.iter().any(|(key, value)| key == PROMPT_LENGTH
            && value == &SECRET_PROMPT.chars().count().to_string()),
        "the prompt length is what makes the redaction useful: {text}"
    );
}

/// One model turn: a tool call, or the closing text.
enum Turn {
    Call { name: &'static str, input: Value },
    Text,
}

/// Plays `turns` in order and keeps everything each request handed the
/// provider to send.
struct ScriptedModel {
    turns: Mutex<VecDeque<Turn>>,
    requests: Mutex<Vec<String>>,
}

impl Provider for ScriptedModel {
    fn stream_message<'a>(
        &'a self,
        _: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&'a CacheKey>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let request = json!({ "system": system, "tools": tools, "messages": messages });
            self.requests.lock().unwrap().push(request.to_string());
            let turn = self.turns.lock().unwrap().pop_front();
            let (content, stop_reason) = match turn.expect("the script ended early") {
                Turn::Call { name, input } => {
                    let id = format!("toolu_{}", self.requests.lock().unwrap().len());
                    let _ = event_tx.send(ProviderEvent::ToolUseStart {
                        id: id.clone(),
                        name: name.into(),
                        source_ordinal: None,
                    });
                    let _ = event_tx.send(ProviderEvent::ToolInputDelta {
                        id: id.clone(),
                        delta: input.to_string(),
                    });
                    let call = ContentBlock::ToolUse {
                        id,
                        name: name.into(),
                        input,
                        thought_signature: None,
                    };
                    (call, StopReason::ToolUse)
                }
                Turn::Text => {
                    let _ = event_tx.send(ProviderEvent::TextDelta {
                        text: DONE_TEXT.into(),
                    });
                    let text = ContentBlock::Text {
                        text: DONE_TEXT.into(),
                    };
                    (text, StopReason::EndTurn)
                }
            };
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![content],
                    ..Default::default()
                },
                usage: TokenUsage {
                    input: INPUT_TOKENS,
                    output: OUTPUT_TOKENS,
                    ..Default::default()
                },
                stop_reason: Some(stop_reason),
                ..Default::default()
            })
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// A shell command that hands its secret to a file only the test reads, prints
/// it where every output path sees it, and is long enough to be saved whole.
fn shell_call(witness: &Path) -> Turn {
    let command = format!(
        "printf %s \"${SECRET_NAME}\" > '{}'; echo \"${SECRET_NAME}\"; seq {FILLER_LINES}; \
         echo \"${SECRET_NAME}\" >&2",
        witness.display()
    );
    Turn::Call {
        name: TOOL_NAME,
        input: json!({ "command": command }),
    }
}

fn agent_params(
    provider: Arc<dyn Provider>,
    project: &Path,
    state: &StateDir,
    session: CaudraId,
    registry: Arc<ToolRegistry>,
) -> AgentParams {
    let model = Model::from_spec(MODEL_SPEC).expect("model");
    let permissions = PermissionManager::new_nonpersistent(
        PermissionsConfig {
            default: DefaultEffect::Allow,
            ..PermissionsConfig::default()
        },
        project.to_path_buf(),
        Arc::default(),
    );
    permissions.set_session_mode(Some(PermissionMode::Yolo));
    AgentParams {
        provider: Arc::clone(&provider),
        model: model.clone(),
        chat_provider: provider,
        chat_model: model,
        config: AgentConfig {
            generate_titles: false,
            summarize_memory: false,
            compaction_requirements: false,
            ..AgentConfig::default()
        },
        tool_output_lines: ToolOutputLines::default(),
        tool_output_store: Some(Arc::new(ToolOutputStore::new(state.clone()))),
        permissions: Arc::new(permissions),
        session_id: Some(SessionRef::from(session)),
        secrets: Some(SecretVaults::for_session(state, session)),
        cache_key: None,
        workspace_session: None,
        remote_project_context: None,
        host_cwd: None,
        local_documents: None,
        task_environment: template::env_vars().set("{cwd}", project.display().to_string()),
        root_tool_use_id: None,
        mailbox: None,
        context_publisher: None,
        timeouts: Timeouts::default(),
        file_tracker: FileReadTracker::fresh(),
        path_locks: PathLocks::fresh(),
        changes: None,
        prompt_slots: Arc::new(ResolvedSlots::default()),
        prompt_profiles: Arc::new(PromptProfileCatalog::default()),
        default_task_prompt_profile_name: Arc::from(BUILTIN_PROFILE_NAME),
        active_prompt_profile_name: Some(Arc::from(BUILTIN_PROFILE_NAME)),
        subagent_cancels: Arc::new(CancelMap::new()),
        subagent_history: SubagentHistoryStore::default(),
        registry,
        audience: ToolAudience::MAIN,
        tool_filter: ToolFilter::All,
        tool_ceiling: ToolFilter::All,
        profile_tool_policy: Arc::default(),
        model_policy: Arc::new(ModelPolicy::default()),
        workflow: None,
        background: None,
        jobs: None,
        task_id: None,
    }
}

fn prompt(message: &str) -> AgentInput {
    AgentInput {
        message: message.into(),
        mode: AgentMode::Build,
        plan: None,
        images: Vec::new(),
        mentions: Vec::new(),
        commits: Vec::new(),
        preamble: Vec::new(),
        thinking: Default::default(),
        fast: false,
        prompt: None,
        resume: false,
    }
}

/// Every file under `dir` except the vault's own, which hold the value by design.
fn stored_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).expect("state dir").flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == SECRETS_DIR) {
            continue;
        }
        if path.is_dir() {
            files.extend(stored_files(&path));
        } else {
            files.push(path);
        }
    }
    files
}

fn holds_a_secret(bytes: &[u8]) -> bool {
    [CANARY, REPLACED].iter().any(|secret| {
        bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
    })
}

/// A secret travels the card, a shell command and an edit in `/secrets`, as the
/// TUI drives them, and only the shell child ever sees its value.
#[test]
fn a_secret_reaches_the_shell_and_nothing_else() {
    let capture = capture();
    let project = TempDir::new().expect("project");
    let state_root = TempDir::new().expect("state");
    let state = StateDir::from_path(state_root.path().to_path_buf());
    let witness = project.path().join(WITNESS_FILE);
    let session = CaudraId::generate();
    let mut stored = StoredSession::new(MODEL_SPEC, &project.path().display().to_string());
    stored.id = session;
    stored.save(&state).expect("session row");

    let registry = Arc::new(ToolRegistry::new());
    native::register(&registry, FeatureFlags::default()).expect("native tools");
    let host = WorkcellHost::new(project.path(), None).expect("Workcell host");
    host.register(&registry).expect("Workcell tools");
    let model = Arc::new(ScriptedModel {
        turns: Mutex::new(VecDeque::from([
            Turn::Call {
                name: REQUEST_SECRET_TOOL_NAME,
                input: json!({ "name": SECRET_NAME, "label": SECRET_LABEL, "reason": SECRET_REASON }),
            },
            shell_call(&witness),
            Turn::Text,
            shell_call(&witness),
            Turn::Text,
        ])),
        requests: Mutex::default(),
    });
    let params = agent_params(
        Arc::clone(&model) as Arc<dyn Provider>,
        project.path(),
        &state,
        session,
        registry,
    );
    let secrets = params.secrets.clone().expect("vault");
    let (raw_tx, events) = flume::unbounded::<Envelope>();
    let (answers, answer_rx) = flume::unbounded();
    let mut history = History::new(Vec::new());
    let mut agent = Agent::new(
        params,
        AgentRunParams {
            history: &mut history,
            system: String::new(),
            environment: None,
            instructions: None,
            memory: None,
            configuration: None,
            mode_notice: None,
            event_tx: EventSender::new(raw_tx, 0),
            tools: json!([]),
            tools_fixed: false,
            memory_store: None,
        },
    )
    .with_user_response_rx(Arc::new(async_lock::Mutex::new(answer_rx)));
    let seen = Mutex::new(Vec::new());
    let card = || async {
        loop {
            let envelope = events.recv_async().await.expect("agent events");
            if matches!(envelope.event, AgentEvent::Question(_)) {
                let reply = SecretReply::Save {
                    value: SecretValue::new(CANARY.to_owned()),
                    scope: SecretScope::Session,
                };
                answers.send(reply.encode()).expect("card answer");
            }
            seen.lock().unwrap().push(envelope.event);
        }
    };

    smol::block_on(future::or(agent.run(prompt(DEPLOY_PROMPT)), card())).expect("first run");
    assert_eq!(
        fs::read_to_string(&witness).unwrap(),
        CANARY,
        "{NOT_INJECTED}"
    );
    scope::replace(
        &secrets,
        &project.path().join(CONFIG_FILE),
        SECRET_NAME,
        SecretScope::Session,
        SecretValue::new(REPLACED.to_owned()),
    )
    .expect("replace in /secrets");
    smol::block_on(future::or(agent.run(prompt(DEPLOY_PROMPT)), card())).expect("second run");
    assert_eq!(
        fs::read_to_string(&witness).unwrap(),
        REPLACED,
        "{NOT_INJECTED}"
    );
    drop(agent);
    let mut seen = seen.into_inner().unwrap();
    seen.extend(events.drain().map(|envelope| envelope.event));

    let mut saved_outputs = 0;
    for event in &seen {
        let rendered = format!("{event:?}");
        assert!(
            !holds_a_secret(rendered.as_bytes()),
            "{LEAKED} an event: {rendered}"
        );
        if let AgentEvent::ToolDone(done) = event {
            saved_outputs += usize::from(done.output_ref.is_some());
            stored.insert_tool_output(done.id.clone(), done.output.clone());
        }
    }
    assert!(saved_outputs > 0, "no shell output was saved whole");
    stored.replace_messages(history.into_items());
    stored.save(&state).expect("session save");

    let requests = model.requests.lock().unwrap();
    assert!(
        requests
            .last()
            .is_some_and(|request| request.contains(SHOWN_SECRET)),
        "the shell output never reached the provider: {requests:?}"
    );
    for request in requests.iter() {
        assert!(
            !holds_a_secret(request.as_bytes()),
            "{LEAKED} a request: {request}"
        );
    }
    let files = stored_files(state_root.path());
    assert!(
        files.iter().any(|file| file
            .file_name()
            .is_some_and(|name| name == SESSIONS_DB_FILE)),
        "the session never reached its database: {files:?}"
    );
    for file in files {
        let bytes = fs::read(&file).expect("stored file");
        assert!(!holds_a_secret(&bytes), "{LEAKED} {}", file.display());
    }
    caudra_otel::shutdown(OTEL_FLUSH);
    let log = capture.text();
    assert!(log.contains(OTEL_EXPORT), "telemetry exported nothing");
    assert!(!holds_a_secret(log.as_bytes()), "{LEAKED} the log: {log}");
}
