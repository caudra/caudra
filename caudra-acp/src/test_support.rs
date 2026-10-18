use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use agent_client_protocol_schema::v1::{RequestId, SessionId};
use caudra_agent::AgentMode;
use caudra_agent::headless::InteractiveHandle;
use caudra_agent::mcp::call::{ElicitMode, ElicitRequest};
use caudra_agent::permissions::{
    EngineFlag, PermissionAdvisory, PermissionManager, PermissionRequest,
};
use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_config::ModelPolicy;
use caudra_providers::model::Model;
use caudra_providers::{
    HistoryItem, Message, ReasoningOption, ReasoningOptions, ThinkingConfig, ThinkingSupport,
    Timeouts, expand_message,
};
use caudra_storage::StateDir;
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::sessions::{SessionLease, StoredPlanTarget};
use flume::{Receiver, Sender, WeakSender};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::asks::{ClientProfile, ClientUpdates};
use crate::child_work::Children;
use crate::outbound::{AskKind, Frame, Outbound, Pending, PendingState};
use crate::server::{Incoming, Server, SessionSlot};
use crate::session::SessionState;
use crate::turn::{OwedPrompt, PromptIdentity};
use crate::{AcpAccounts, AcpParams, AcpRuntime, AcpRuntimeResolver, locked};

pub(crate) const ANSWERED_ID: i64 = 1001;
pub(crate) const UNKNOWN_ID: i64 = 1002;
pub(crate) const PROMPT_ID: i64 = 7;
pub(crate) const SECOND_PROMPT_ID: i64 = 8;
pub(crate) const CAUDRA_REQUEST_ID: &str = "caudra-permission-1";
pub(crate) const SECOND_CAUDRA_REQUEST_ID: &str = "caudra-permission-2";
pub(crate) const DISCOVERED_SPEC: &str = "openrouter/discovered-model";
pub(crate) const OFFLINE_SPEC: &str = "openai/gpt-5";
pub(crate) const SELECTED_SPEC: &str = "openai/gpt-5.6-sol";
pub(crate) const ADVISORY_PROBABILITY: f64 = 0.875;
pub(crate) const DELETE_CAUTION: &str = "Decision engine caution: May delete files (88%).";
pub(crate) const PLAN_FILE: &str = "plan.md";
pub(crate) const MCP_MESSAGE: &str = "Where should the release go?";
pub(crate) const MCP_TOOL_CALL: &str = "toolu_mcp";
pub(crate) const MCP_FIELD: &str = "branch";
pub(crate) const CANCELLED: &str = "cancelled";
pub(crate) const AVAILABLE_COMMANDS_UPDATE: &str = "available_commands_update";
/// Sells fast mode, which a test cannot make up the way it makes up levels.
pub(crate) const FAST_SPEC: &str = "anthropic/claude-opus-5";
pub(crate) const LEVELS: [&str; 3] = ["low", "medium", "high"];
pub(crate) const LOGIN_MISSING: &str =
    "no saved credentials for 'openai' — run `caudra auth login openai`";
const PROJECT: &str = "/project";

/// A reasoning model declaring exactly `levels`, and the toggle that lets it
/// stop reasoning when `toggle` is set.
pub(crate) fn effort_model(levels: &[&str], toggle: bool) -> Model {
    let effort = ReasoningOption::Effort {
        values: levels.iter().map(|level| (*level).to_owned()).collect(),
    };
    let options = toggle
        .then_some(ReasoningOption::Toggle)
        .into_iter()
        .chain([effort])
        .collect();
    Model {
        reasoning_options: ReasoningOptions::new(options),
        thinking_override: Some(ThinkingSupport::Yes),
        ..Model::from_spec(SELECTED_SPEC).unwrap()
    }
}

/// A prompt the session owes `id` an answer for, as a client without a prompt
/// id would send it.
pub(crate) fn owed(id: i64) -> OwedPrompt {
    OwedPrompt::new(
        RequestId::Number(id),
        PromptIdentity::new(CaudraId::generate().to_string()),
    )
}

/// The request id of the prompt `pending` owes an answer, if any.
pub(crate) fn owed_request(pending: &PendingState) -> Option<RequestId> {
    locked(pending)
        .prompt
        .as_ref()
        .map(|owed| owed.request.clone())
}

/// A main loop that has already gone, for a pump nothing reroutes.
pub(crate) fn no_server() -> WeakSender<Incoming> {
    flume::unbounded().0.downgrade()
}

pub(crate) fn stored_local_plan(path: &Path) -> StoredPlanTarget {
    StoredPlanTarget::LocalPath {
        path: path.to_string_lossy().into_owned(),
    }
}

pub(crate) fn advisory_request(flag: EngineFlag, probability: f64) -> PermissionRequest {
    let mut request = PermissionRequest::from_legacy(
        CAUDRA_REQUEST_ID.into(),
        caudra_config::ToolKey::native("shell"),
        vec!["git status".into()],
        json!({"command": "git status"}),
        Path::new(PROJECT),
        false,
    );
    request
        .presentation
        .advisories
        .push(PermissionAdvisory { flag, probability });
    request
}

pub(crate) fn history_items(messages: &[Message]) -> Vec<HistoryItem> {
    let mut items = Vec::new();
    for message in messages {
        let parent_id = items.last().map(|item: &HistoryItem| item.id);
        items.extend(expand_message(message, parent_id));
    }
    items
}

pub(crate) fn allow_once(id: i64) -> Value {
    selected_permission(id, "allow_once")
}

pub(crate) fn selected_permission(id: i64, option_id: &str) -> Value {
    json!({
        "id": id,
        "result": { "outcome": { "outcome": "selected", "optionId": option_id } },
    })
}

pub(crate) fn permission_manager() -> Arc<PermissionManager> {
    Arc::new(PermissionManager::new_nonpersistent(
        caudra_config::PermissionsConfig::default(),
        PathBuf::from(PROJECT),
        Arc::default(),
    ))
}

pub(crate) fn mcp_form() -> ElicitRequest {
    ElicitRequest {
        mode: ElicitMode::Form {
            schema: json!({
                "type": "object",
                "properties": {MCP_FIELD: {"type": "string"}},
            }),
        },
        message: MCP_MESSAGE.into(),
        kind: None,
    }
}

/// Where every test session keeps its lease and its stored record.
pub(crate) fn state_dir() -> StateDir {
    static DIR: OnceLock<TempDir> = OnceLock::new();
    StateDir::from_path(
        DIR.get_or_init(|| TempDir::new().unwrap())
            .path()
            .to_path_buf(),
    )
}

/// Accounts that are signed in or out as the test says, never the machine's.
pub(crate) struct TestAccounts {
    signed_in: AtomicBool,
}

impl TestAccounts {
    pub(crate) fn new(signed_in: bool) -> Arc<Self> {
        Arc::new(Self {
            signed_in: AtomicBool::new(signed_in),
        })
    }

    pub(crate) fn sign_in(&self) {
        self.signed_in.store(true, Ordering::SeqCst);
    }
}

impl AcpAccounts for TestAccounts {
    fn check(&self, _model: &Model) -> Result<(), String> {
        if self.signed_in.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(LOGIN_MISSING.to_owned())
        }
    }

    fn default_model(&self) -> Result<Model, String> {
        Model::from_spec(OFFLINE_SPEC).map_err(|error| error.to_string())
    }

    fn model_specs(&self) -> Vec<String> {
        vec![OFFLINE_SPEC.to_owned()]
    }
}

fn test_params(accounts: Arc<TestAccounts>) -> AcpParams {
    let resolver: AcpRuntimeResolver = Arc::new(|_, _| Ok(AcpRuntime::default()));
    AcpParams {
        model: Model::from_spec(SELECTED_SPEC).unwrap(),
        timeouts: Timeouts::default(),
        thinking: Default::default(),
        prompt_profiles: Arc::new(PromptProfileCatalog::default()),
        system_prompt_profile_override: None,
        permission_mode: None,
        model_policy: Arc::new(ModelPolicy::default()),
        runtime_resolver: resolver,
        accounts,
    }
}

/// A server past `initialize`, its sessions built without an agent behind them.
pub(crate) struct TestServer {
    pub(crate) server: Server,
    pub(crate) frames: Receiver<Frame>,
    pub(crate) answers: Receiver<String>,
    pub(crate) sid: SessionId,
    pub(crate) permissions: Arc<PermissionManager>,
    pub(crate) accounts: Arc<TestAccounts>,
    inbox: Receiver<Incoming>,
    /// Keeps the server's weak sender alive, as the reader task does.
    _events: Sender<Incoming>,
}

impl TestServer {
    pub(crate) fn initialized() -> Self {
        Self::with_accounts(TestAccounts::new(true))
    }

    /// A server whose every provider lacks credentials.
    pub(crate) fn signed_out() -> Self {
        Self::with_accounts(TestAccounts::new(false))
    }

    fn with_accounts(accounts: Arc<TestAccounts>) -> Self {
        let (outbound, frames) = Outbound::channel();
        let (events, inbox) = flume::unbounded();
        let params = test_params(Arc::clone(&accounts));
        let server = Server {
            outbound,
            default_model: params.model.clone(),
            params: Arc::new(params),
            storage: state_dir(),
            model_specs: Vec::new(),
            client: Some(ClientProfile::default()),
            sessions: HashMap::new(),
            failed: HashMap::new(),
            events: events.downgrade(),
        };
        Self {
            server,
            frames,
            answers: flume::unbounded().1,
            sid: SessionId::from(String::new()),
            permissions: permission_manager(),
            accounts,
            inbox,
            _events: events,
        }
    }

    pub(crate) fn with_asks(
        permissions: Arc<PermissionManager>,
        asks: HashMap<i64, AskKind>,
    ) -> Self {
        let mut test = Self::initialized();
        test.permissions = permissions;
        test.sid = test.add_session();
        locked(&test.session().pending).asks = asks;
        test
    }

    pub(crate) fn with_ask(kind: AskKind) -> Self {
        Self::with_asks(permission_manager(), HashMap::from([(ANSWERED_ID, kind)]))
    }

    pub(crate) fn with_session() -> Self {
        Self::with_asks(permission_manager(), HashMap::new())
    }

    /// Adds a live session sharing this server's permission broker; the first
    /// one added answers its elicitations on [`Self::answers`].
    pub(crate) fn add_session(&mut self) -> SessionId {
        let (answer_tx, answer_rx) = flume::unbounded();
        if self.server.sessions.is_empty() {
            self.answers = answer_rx;
        }
        let session = SessionRef::from(CaudraId::generate());
        let lease = Arc::new(SessionLease::acquire(&state_dir(), session.id()).unwrap());
        let handle = InteractiveHandle::for_test(
            state_dir(),
            lease,
            Arc::clone(&self.permissions),
            answer_tx,
        );
        let state = SessionState {
            handle,
            mcp: None,
            current_mode: AgentMode::Build,
            plan: None,
            model: Model::from_spec(SELECTED_SPEC).unwrap(),
            thinking: ThinkingConfig::Off,
            fast: false,
            boolean_options: false,
            updates: ClientUpdates::default(),
            child_tier: self
                .server
                .client
                .as_ref()
                .map(|client| client.child_tier)
                .unwrap_or_default(),
            children: Children::default(),
            announced: Vec::new(),
            commands: Vec::new(),
            pending: Arc::new(Mutex::new(Pending::default())),
            cwd: PathBuf::new(),
            remote: false,
            runtime: AcpRuntime::default(),
        };
        let sid = state.id();
        self.server
            .sessions
            .insert(sid.clone(), SessionSlot::Active(Box::new(state)));
        sid
    }

    pub(crate) fn active(&self, sid: &SessionId) -> &SessionState {
        match self.server.sessions.get(sid) {
            Some(SessionSlot::Active(state)) => state,
            _ => panic!("session {sid} is not active"),
        }
    }

    pub(crate) fn session(&self) -> &SessionState {
        self.active(&self.sid)
    }

    pub(crate) fn session_mut(&mut self) -> &mut SessionState {
        match self.server.sessions.get_mut(&self.sid) {
            Some(SessionSlot::Active(state)) => state,
            _ => panic!("the test session is not active"),
        }
    }

    /// Hands the server what the next start or close it spawned reported.
    pub(crate) fn settle_next(&mut self) {
        let incoming = self.inbox.recv().expect("a spawned task reports back");
        assert!(self.server.handle_incoming(incoming).is_continue());
    }

    pub(crate) fn next_message(&self) -> Value {
        self.frames.try_recv().expect("a queued message").message
    }
}
