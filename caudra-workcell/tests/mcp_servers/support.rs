//! The pinned servers under `tests/mcp-servers`, each reached the way a user's `mcp.toml` would
//! reach it, and snippets run on them through the real `mcp` gateway and Monty worker.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use caudra_agent::mcp::McpSession;
use caudra_agent::mcp::code::gateway;
use caudra_agent::mcp::code::output::{McpCodeOutcome, McpCodeOutput};
use caudra_agent::mcp::code::runner::install;
use caudra_agent::mcp::code::view::ServerState;
use caudra_agent::mcp::config::{
    McpConfig, McpConfigSource, RawHttpFields, RawServerConfig, RawStdioFields, RawTransport,
};
use caudra_agent::mcp::start_with_config;
use caudra_agent::permissions::{
    PermissionAnswer, PermissionLifetime, PermissionManager, PermissionMode, PermissionRequest,
};
use caudra_agent::tools::ToolContext;
use caudra_agent::tools::test_support::stub_ctx_asking;
use caudra_agent::{AgentEvent, AgentMode, Envelope, EventSender};
use caudra_config::PermissionsConfig;
use caudra_workcell::WorkcellScriptRunner;
use isahc::{ReadResponseExt, Request, RequestExt};
use serde_json::{Value, json};
use tempfile::TempDir;

pub const EVERYTHING: &str = "everything";
pub const EVERYTHING_IMAGE: &str = "caudra-mcp-it/everything";
pub const EXECUTOR: &str = "executor";
pub const EXECUTOR_IMAGE: &str = "caudra-mcp-it/executor";
pub const HOME_ASSISTANT: &str = "home-assistant";
pub const PLAN_FILE: &str = "/tmp/caudra-mcp-it-plan.md";
pub const PLAN_REFUSAL: &str =
    "plan mode allows only the MCP tools a recognised server marks read-only";

const WORKER_ENV: &str = "WORKCELL_MCP_CODE_WORKER";
const HOME_ASSISTANT_URL_ENV: &str = "CAUDRA_IT_HOME_ASSISTANT_URL";
const HOME_ASSISTANT_TOKEN_ENV: &str = "CAUDRA_IT_HOME_ASSISTANT_TOKEN";
const HOME_ASSISTANT_MCP_PATH: &str = "/api/mcp";
const NO_HOME_ASSISTANT: &str = "Home Assistant is not running: run `make test-mcp-servers`, or `eval \"$(python3 scripts/mcp-servers.py up)\"` first";
const AUTHORIZATION: &str = "Authorization";
const CONTENT_TYPE: &str = "Content-Type";
const JSON_TYPE: &str = "application/json";
const DOCKER: &str = "docker";
const DOCKER_RUN: &[&str] = &["run", "--interactive", "--rm"];
const CALL_TIMEOUT_MS: u64 = 120_000;
const PARENT_CALL: &str = "toolu_mcp_servers";
const UNANSWERED: &str = "the snippet asked more often than the test answers";
const NOT_CONNECTED: &str = "the server did not connect";
const WHOLE_TOOL_OPTION: &str = "allow_whole_mcp_tool_conversation";
const SETTLE_POLLS: u32 = 150;
const SETTLE_INTERVAL: Duration = Duration::from_millis(100);

pub struct Server {
    name: &'static str,
    transport: RawTransport,
}

/// A stdio server in a fresh container of `image`, gone when its stdin closes.
pub fn stdio(name: &'static str, image: &str) -> Server {
    let command = [DOCKER]
        .into_iter()
        .chain(DOCKER_RUN.iter().copied())
        .chain([image])
        .map(str::to_owned)
        .collect();
    Server {
        name,
        transport: RawTransport::Stdio(RawStdioFields {
            command,
            environment: HashMap::new(),
        }),
    }
}

/// The Home Assistant `scripts/mcp-servers.py up` started, over streamable HTTP.
pub fn home_assistant() -> Server {
    let HomeAssistant { url, token } = HomeAssistant::running();
    Server {
        name: HOME_ASSISTANT,
        transport: RawTransport::Http(RawHttpFields {
            url: format!("{url}{HOME_ASSISTANT_MCP_PATH}"),
            headers: HashMap::from([(AUTHORIZATION.to_owned(), format!("Bearer {token}"))]),
            oauth: None,
        }),
    }
}

/// Connected servers and one agent's `mcp` session on them, with a permission prompt the test
/// answers.
pub struct Servers {
    session: McpSession,
    permissions: Arc<PermissionManager>,
    ctx: ToolContext,
    events: flume::Receiver<Envelope>,
    _state: TempDir,
}

impl Servers {
    pub fn connect(servers: Vec<Server>, mode: AgentMode, permission_mode: PermissionMode) -> Self {
        install(Arc::new(WorkcellScriptRunner::standalone(
            env::var_os(WORKER_ENV).map(PathBuf::from),
        )));
        let config = McpConfig {
            sources: servers
                .iter()
                .map(|server| (server.name.to_owned(), McpConfigSource::Global))
                .collect(),
            mcp: servers
                .into_iter()
                .map(|server| {
                    let raw = RawServerConfig {
                        enabled: true,
                        timeout: CALL_TIMEOUT_MS,
                        transport: server.transport,
                    };
                    (server.name.to_owned(), raw)
                })
                .collect(),
            ..McpConfig::default()
        };
        let handle = start_with_config(config).expect("servers are configured");
        smol::block_on(handle.ready());
        let session = McpSession::new(handle, &[]);
        for server in session.servers() {
            assert_eq!(
                server.state,
                ServerState::Connected,
                "{NOT_CONNECTED}: {}",
                server.name
            );
        }
        let state = TempDir::new().expect("tempdir");
        let permissions = Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            state.path().to_path_buf(),
            Arc::default(),
        ));
        permissions.set_session_mode(Some(permission_mode));
        let (tx, events) = flume::unbounded();
        let mut ctx = stub_ctx_asking(&mode, Arc::clone(&permissions), &EventSender::new(tx, 0));
        ctx.tool_use_id = Some(PARENT_CALL.to_owned());
        Self {
            session,
            permissions,
            ctx,
            events,
            _state: state,
        }
    }

    pub fn run(&self, code: &str) -> McpCodeOutput {
        self.run_answering(code, &[]).0
    }

    pub fn reset_and_run(&self, code: &str) -> McpCodeOutput {
        smol::block_on(gateway::run(
            &self.session,
            &self.ctx,
            code.to_owned(),
            true,
        ))
    }

    /// Runs `code`, giving each permission prompt the next of `answers`, and returns what it
    /// asked about.
    pub fn run_answering(
        &self,
        code: &str,
        answers: &[PermissionAnswer],
    ) -> (McpCodeOutput, Vec<PermissionRequest>) {
        let mut answers = answers.iter().cloned();
        let mut asked = Vec::new();
        let run = gateway::run(&self.session, &self.ctx, code.to_owned(), false);
        let answering = async {
            loop {
                let envelope = self.events.recv_async().await.expect("events stay open");
                if let AgentEvent::PermissionRequest(request) = envelope.event {
                    let answer = answers.next().expect(UNANSWERED);
                    assert!(self.permissions.answer(&request.id, answer));
                    asked.push(*request);
                }
            }
        };
        let output = smol::block_on(smol::future::or(run, answering));
        (output, asked)
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        smol::block_on(self.session.shutdown());
    }
}

/// The prompt's "any arguments" choice: the tool, whatever it is called with, until the
/// conversation ends.
pub fn whole_tool_for_the_session() -> PermissionAnswer {
    PermissionAnswer::AllowOption {
        option_id: WHOLE_TOOL_OPTION.to_owned(),
        lifetime: PermissionLifetime::Conversation,
    }
}

/// The snippet's result, once it ran to the end.
pub fn completed(output: &McpCodeOutput) -> &str {
    assert_eq!(
        output.outcome,
        McpCodeOutcome::Completed,
        "{}",
        output.model_text()
    );
    output.result.as_deref().unwrap_or_default()
}

/// The Home Assistant `scripts/mcp-servers.py up` started, and its REST API, which checks what
/// a snippet changed.
pub struct HomeAssistant {
    url: String,
    token: String,
}

impl HomeAssistant {
    pub fn running() -> Self {
        Self {
            url: env::var(HOME_ASSISTANT_URL_ENV).expect(NO_HOME_ASSISTANT),
            token: env::var(HOME_ASSISTANT_TOKEN_ENV).expect(NO_HOME_ASSISTANT),
        }
    }

    pub fn call(&self, domain: &str, service: &str, entity: &str) {
        let response = Request::post(format!("{}/api/services/{domain}/{service}", self.url))
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header(CONTENT_TYPE, JSON_TYPE)
            .body(json!({"entity_id": entity}).to_string())
            .expect("service request")
            .send()
            .expect("Home Assistant answers");
        assert!(
            response.status().is_success(),
            "{domain}.{service}: {}",
            response.status()
        );
    }

    /// Calls `service` on `entity` and waits for it to reach `state`.
    pub fn set(&self, domain: &str, service: &str, entity: &str, state: &str) {
        self.call(domain, service, entity);
        assert_eq!(
            self.settled(entity, state),
            state,
            "{domain}.{service} {entity}"
        );
    }

    /// `expected` as soon as `entity` shows it, or what it shows when the wait runs out.
    pub fn settled(&self, entity: &str, expected: &str) -> String {
        let mut state = self.state(entity);
        for _ in 0..SETTLE_POLLS {
            if state == expected {
                break;
            }
            thread::sleep(SETTLE_INTERVAL);
            state = self.state(entity);
        }
        state
    }

    pub fn state(&self, entity: &str) -> String {
        let mut response = Request::get(format!("{}/api/states/{entity}", self.url))
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .body(())
            .expect("state request")
            .send()
            .expect("Home Assistant answers");
        let body: Value =
            serde_json::from_str(&response.text().expect("state body")).expect("state JSON");
        body["state"].as_str().expect("a state").to_owned()
    }
}
