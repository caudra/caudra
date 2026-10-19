use std::collections::HashMap;
use std::io;
use std::ops::ControlFlow;
use std::sync::Arc;

use agent_client_protocol_schema::v1::SessionId;
use caudra_agent::mcp::McpHandle;
use caudra_config::ModelPolicy;
use caudra_providers::model::Model;
use caudra_providers::provider::fetch_all_models;
use caudra_storage::StateDir;
use color_eyre::eyre::{Context, eyre};
use flume::WeakSender;
use serde_json::Value;
use smol::Unblock;
use smol::io::{AsyncBufRead, AsyncWrite, BufReader};
use tracing::{error, warn};

use crate::AcpParams;
use crate::asks::ClientProfile;
use crate::auth::AuthOutcome;
use crate::catalog::{MODELS_CHANGED, ModelCatalog};
use crate::errors::{LINE_TOO_LONG, invalid_request};
use crate::lifecycle::{CloseOutcome, FailedRuntime, StartOutcome, Started, close_session};
use crate::outbound::Outbound;
use crate::session::SessionState;
use crate::transport::{Inbound, MAX_LINE_BYTES, read_messages, write_frames};

pub(crate) enum Incoming {
    Inbound(Inbound),
    Models(Vec<String>),
    /// The agent moved the session's conversation onto another model.
    ModelRerouted {
        session: SessionId,
        spec: String,
    },
    /// The skills or MCP prompts a session offers may have changed.
    CommandsChanged(SessionId),
    Authenticated(Box<AuthOutcome>),
    Started(Box<StartOutcome>),
    Closed(Box<CloseOutcome>),
    OutputClosed(io::Error),
}

pub(crate) enum SessionSlot {
    /// Requests that arrived while the session started, handled once it settles.
    Starting(Vec<Value>),
    Active(Box<SessionState>),
    /// Requests that arrived while the session closed, handled once it is gone.
    Closing(Vec<Value>),
}

/// The main loop's state. Only the loop touches it, so nothing here locks;
/// work that waits runs in its own task and reports back as [`Incoming`].
pub(crate) struct Server {
    pub(crate) outbound: Outbound,
    pub(crate) params: Arc<AcpParams>,
    pub(crate) storage: StateDir,
    pub(crate) model_specs: Vec<String>,
    /// What a new session opens on: the startup model, until a sign-in
    /// resolves it again.
    pub(crate) default_model: Model,
    /// `None` until the client calls `initialize`.
    pub(crate) client: Option<ClientProfile>,
    pub(crate) sessions: HashMap<SessionId, SessionSlot>,
    /// Runtimes that would not stop, kept against the session they served.
    pub(crate) failed: HashMap<SessionId, FailedRuntime>,
    /// Weak, so the loop ends once stdin and every start have finished.
    pub(crate) events: WeakSender<Incoming>,
}

pub async fn serve(params: AcpParams) -> color_eyre::Result<()> {
    let storage = StateDir::resolve().context("resolve state dir")?;
    serve_on(
        params,
        storage,
        BufReader::new(Unblock::new(std::io::stdin())),
        Unblock::new(std::io::stdout()),
        Discovery::Live,
    )
    .await
}

/// Whether the server lists models over the wire in the background.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Discovery {
    Live,
    #[cfg(all(test, unix))]
    Off,
}

/// The server loop over any byte streams: requests in on `input`, frames out on `output`.
pub(crate) async fn serve_on(
    params: AcpParams,
    storage: StateDir,
    input: impl AsyncBufRead + Unpin + Send + 'static,
    output: impl AsyncWrite + Unpin + Send + 'static,
    discovery: Discovery,
) -> color_eyre::Result<()> {
    let (outbound, frames) = Outbound::channel();
    let (events, inbox) = flume::unbounded::<Incoming>();

    let writer_events = events.downgrade();
    let writer = smol::spawn(async move {
        if let Err(error) = write_frames(frames, output).await
            && let Some(events) = writer_events.upgrade()
        {
            let _ = events.send(Incoming::OutputClosed(error));
        }
    });
    let reader_events = events.clone();
    let reader = smol::spawn(read_messages(input, MAX_LINE_BYTES, move |inbound| {
        reader_events.send(Incoming::Inbound(inbound)).is_ok()
    }));
    if discovery == Discovery::Live {
        discover_models(Arc::clone(&params.model_policy), events.downgrade());
    }

    let mut server = Server {
        outbound,
        model_specs: params.accounts.model_specs(),
        default_model: params.model.clone(),
        params: Arc::new(params),
        storage,
        client: None,
        sessions: HashMap::new(),
        failed: HashMap::new(),
        events: events.downgrade(),
    };
    drop(events);

    let mut output_error = None;
    while let Ok(incoming) = inbox.recv_async().await {
        if let ControlFlow::Break(error) = server.handle_incoming(incoming) {
            output_error = Some(error);
            break;
        }
    }
    // A start still running when output failed must not wait on this loop.
    drop(inbox);

    let failures = server.close_all().await;
    drop(server);
    for failure in &failures {
        error!(session = %failure.session, error = %failure.error, "ACP runtime shutdown failed");
    }
    match output_error {
        Some(error) if error.kind() != io::ErrorKind::BrokenPipe => {
            return Err(error).context("write stdout");
        }
        Some(error) => warn!(%error, "ACP client stopped reading; shutting down"),
        None => {
            writer.await;
            reader.await.context("read stdin")?;
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    Err(eyre!(
        "ACP runtime shutdown failed for {} session(s)",
        failures.len()
    ))
}

/// Static manifests miss providers that only list their models over the wire
/// (OpenRouter and friends), so the same discovery the TUI runs happens here,
/// in the background. Each batch leaves the moment it lands: the slowest source
/// is a cold catalog download, and a provider the client could already pick from
/// should not wait behind it.
fn discover_models(policy: Arc<ModelPolicy>, events: WeakSender<Incoming>) {
    smol::spawn(async move {
        fetch_all_models(
            &policy,
            |batch| {
                if let Some(events) = events.upgrade() {
                    let _ = events.send(Incoming::Models(batch.models));
                }
            },
            None,
        )
        .await;
    })
    .detach();
}

/// MCP servers connect after the session opens, and their prompts become
/// commands once they have.
fn announce_when_ready(mcp: McpHandle, session: SessionId, events: WeakSender<Incoming>) {
    smol::spawn(async move {
        mcp.ready().await;
        if let Some(events) = events.upgrade() {
            let _ = events.send(Incoming::CommandsChanged(session));
        }
    })
    .detach();
}

impl Server {
    pub(crate) fn handle_incoming(&mut self, incoming: Incoming) -> ControlFlow<io::Error> {
        match incoming {
            Incoming::Inbound(Inbound::Message(raw)) => self.handle_message(raw),
            Incoming::Inbound(Inbound::Oversized(id)) => self
                .outbound
                .respond(id, Err(invalid_request(LINE_TOO_LONG))),
            Incoming::Models(specs) => self.refresh_models(specs),
            Incoming::ModelRerouted { session, spec } => self.follow_reroute(&session, &spec),
            Incoming::CommandsChanged(session) => {
                if let Some(SessionSlot::Active(state)) = self.sessions.get_mut(&session) {
                    state.announce_commands(&self.outbound);
                }
            }
            Incoming::Authenticated(outcome) => self.settle_authenticate(*outcome),
            Incoming::Started(outcome) => self.settle_start(*outcome),
            Incoming::Closed(outcome) => self.settle_close(*outcome),
            Incoming::OutputClosed(error) => return ControlFlow::Break(error),
        }
        ControlFlow::Continue(())
    }

    /// Discovery lands in batches after the client built its selector from the
    /// offline list, so every batch that adds something announces the fuller
    /// list, to each session and, once initialized, as the model catalog.
    pub(crate) fn refresh_models(&mut self, batch: Vec<String>) {
        let known = self.model_specs.len();
        for spec in batch {
            if !self.model_specs.contains(&spec) {
                self.model_specs.push(spec);
            }
        }
        if self.model_specs.len() == known {
            return;
        }
        // Merged even with no session yet, since session/new builds its selector from this list.
        for slot in self.sessions.values_mut() {
            if let SessionSlot::Active(session) = slot {
                session.announce_options(&self.outbound, &self.model_specs);
            }
        }
        if self.client.is_some() {
            match serde_json::to_value(self.model_catalog()) {
                Ok(catalog) => self.outbound.notify_ext(MODELS_CHANGED, catalog),
                Err(error) => warn!(%error, "model catalog does not serialize"),
            }
        }
    }

    pub(crate) fn model_catalog(&self) -> ModelCatalog {
        ModelCatalog::new(&self.model_specs, &self.default_model)
    }

    /// The agent already runs the new model, so the session only follows it
    /// and tells the client.
    pub(crate) fn follow_reroute(&mut self, session: &SessionId, spec: &str) {
        let Some(SessionSlot::Active(state)) = self.sessions.get_mut(session) else {
            return;
        };
        match Model::from_spec(spec) {
            Ok(model) => {
                state.follow_model(model);
                state.announce_options(&self.outbound, &self.model_specs);
            }
            Err(error) => warn!(%session, spec, %error, "rerouted model does not resolve"),
        }
    }

    pub(crate) fn settle_start(&mut self, outcome: StartOutcome) {
        let StartOutcome {
            request_id,
            session,
            result,
            failure,
        } = outcome;
        let queued = match self.sessions.remove(&session) {
            Some(SessionSlot::Starting(queued)) => queued,
            _ => Vec::new(),
        };
        match result {
            Ok(Started {
                mut state,
                response,
                initial_cost,
                updates,
            }) => {
                let announced = self.outbound.respond_written(request_id, Ok(response));
                // Queued behind the response, so the client reads them after it.
                state.announce_commands(&self.outbound);
                for update in updates {
                    self.outbound.session_update(&session, update);
                }
                if let Some(mcp) = state.mcp.clone() {
                    announce_when_ready(mcp, session.clone(), self.events.clone());
                }
                state
                    .event_pump(self.outbound.clone(), self.events.clone(), initial_cost)
                    .spawn(announced);
                self.sessions
                    .insert(session, SessionSlot::Active(Box::new(state)));
            }
            Err(error) => self.outbound.respond(request_id, Err(error)),
        }
        if let Some(failure) = failure {
            self.retain_failed(failure);
        }
        for raw in queued {
            self.handle_message(raw);
        }
    }

    /// The session is gone by now, so what waited on it sees that.
    pub(crate) fn settle_close(&mut self, outcome: CloseOutcome) {
        let CloseOutcome {
            request_id,
            session,
            result,
            failure,
        } = outcome;
        let queued = match self.sessions.remove(&session) {
            Some(SessionSlot::Closing(queued)) => queued,
            _ => Vec::new(),
        };
        if let Some(failure) = failure {
            self.retain_failed(failure);
        }
        self.outbound.respond(request_id, result);
        for raw in queued {
            self.handle_message(raw);
        }
    }

    pub(crate) fn retain_failed(&mut self, failure: FailedRuntime) {
        error!(
            session = %failure.session,
            error = %failure.error,
            "session runtime did not shut down; refusing that session until restart"
        );
        self.failed.insert(failure.session.clone(), failure);
    }

    /// Cancels every running turn, answers every prompt still owed, and stops
    /// each agent, its MCP servers, and its runtime.
    async fn close_all(&mut self) -> Vec<FailedRuntime> {
        let mut failures = Vec::new();
        for (_, entry) in self.sessions.drain() {
            if let SessionSlot::Active(state) = entry {
                failures.extend(close_session(*state, &self.outbound).await);
            }
        }
        failures
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::{AgentResponse, RequestId};
    use caudra_agent::Envelope;
    use caudra_agent::types::AgentEvent;
    use caudra_providers::accounts::{Reroute, RerouteReason};
    use serde_json::Value;

    use super::{Incoming, SessionSlot};
    use crate::catalog::MODELS_CHANGED;
    use crate::commands::COMPACT_COMMAND;
    use crate::lifecycle::{StartOutcome, Started};
    use crate::outbound::{AskKind, Written};
    use crate::test_support::{
        AVAILABLE_COMMANDS_UPDATE, CANCELLED, DISCOVERED_SPEC, OFFLINE_SPEC, PROMPT_ID,
        SECOND_PROMPT_ID, SELECTED_SPEC, TestServer, owed,
    };
    use crate::{locked, methods, translate};

    const START_REQUEST_ID: i64 = 3;
    const TITLE: &str = "Rename foo to bar";
    const RESTORED_USED: u32 = 1_200;
    const RESTORED_WINDOW: u32 = 200_000;
    const RESTORED_COST: f64 = 0.5;
    const USAGE_UPDATE: &str = "usage_update";

    #[test]
    fn discovered_models_are_pushed_to_the_client() {
        let mut test = TestServer::with_ask(AskKind::Elicitation);
        test.server.model_specs = vec![OFFLINE_SPEC.to_owned()];
        let batch = vec![DISCOVERED_SPEC.to_owned()];

        test.server.refresh_models(batch.clone());
        let update = test.next_message();
        let option = &update["params"]["update"]["configOptions"][0];
        assert_eq!(option["id"], methods::MODEL_CONFIG_ID);
        let selectable: Vec<&str> = option["options"]
            .as_array()
            .expect("the option is a grouped select")
            .iter()
            .flat_map(|group| group["options"].as_array().into_iter().flatten())
            .filter_map(|o| o["value"].as_str())
            .collect();
        assert!(
            selectable.contains(&OFFLINE_SPEC) && selectable.contains(&DISCOVERED_SPEC),
            "a batch is merged into the offline list, not swapped for it: {selectable:?}"
        );
        let changed = test.next_message();
        assert_eq!(changed["method"], MODELS_CHANGED);
        let catalog: Vec<&Value> = changed["params"]["models"]
            .as_array()
            .expect("the catalog lists models")
            .iter()
            .map(|model| &model["id"])
            .collect();
        assert!(
            catalog.contains(&&Value::from(DISCOVERED_SPEC)),
            "{changed}"
        );

        test.server.refresh_models(batch);
        assert!(
            test.frames.is_empty(),
            "a batch adding nothing is not announced"
        );
    }

    /// The agent moves first, so the session follows the event rather than
    /// telling the agent to switch again.
    #[test]
    fn a_reroute_moves_the_model_option_to_where_the_agent_went() {
        let mut test = TestServer::with_session();
        let (event_tx, events) = flume::unbounded();
        let mut pump = test.session().event_pump(
            test.server.outbound.clone(),
            test.server.events.clone(),
            None,
        );
        pump.events = events;
        pump.spawn(Written::settled());
        let reroute = Reroute {
            from: SELECTED_SPEC.into(),
            to: OFFLINE_SPEC.into(),
            reason: RerouteReason::Home,
            cached_tokens: 0,
        };
        event_tx
            .send(Envelope {
                event: AgentEvent::ModelRerouted(Box::new(reroute)),
                task: None,
                subagent: None,
                run_id: 0,
                workflow: None,
            })
            .unwrap();

        test.settle_next();

        let update = test.next_message();
        let model = &update["params"]["update"]["configOptions"][0];
        assert_eq!(model["id"], methods::MODEL_CONFIG_ID);
        assert_eq!(model["currentValue"], OFFLINE_SPEC);
        assert_eq!(test.session().model.spec(), OFFLINE_SPEC);
    }

    #[test]
    fn a_started_session_learns_its_commands_title_and_usage_after_its_response() {
        let mut test = TestServer::initialized();
        let sid = test.add_session();
        let Some(SessionSlot::Active(state)) = test
            .server
            .sessions
            .insert(sid.clone(), SessionSlot::Starting(Vec::new()))
        else {
            panic!("the session was active");
        };
        test.server.settle_start(StartOutcome {
            request_id: RequestId::Number(START_REQUEST_ID),
            session: sid.clone(),
            result: Ok(Started {
                state: *state,
                response: AgentResponse::NewSessionResponse(methods::new_session_response(&sid.0)),
                initial_cost: None,
                updates: vec![
                    translate::session_info(TITLE.into(), None),
                    translate::context_usage(RESTORED_USED, RESTORED_WINDOW, Some(RESTORED_COST)),
                ],
            }),
            failure: None,
        });

        assert_eq!(test.next_message()["id"], START_REQUEST_ID);
        let commands = &test.next_message()["params"]["update"];
        assert_eq!(commands["sessionUpdate"], AVAILABLE_COMMANDS_UPDATE);
        assert_eq!(commands["availableCommands"][0]["name"], COMPACT_COMMAND);
        assert_eq!(test.next_message()["params"]["update"]["title"], TITLE);
        let usage = &test.next_message()["params"]["update"];
        assert_eq!(usage["sessionUpdate"], USAGE_UPDATE);
        assert_eq!(usage["used"], RESTORED_USED);
        assert_eq!(usage["size"], RESTORED_WINDOW);
        assert_eq!(usage["cost"]["amount"], RESTORED_COST);
    }

    #[test]
    fn unchanged_commands_are_not_announced_again() {
        let mut test = TestServer::with_session();
        let sid = test.sid.clone();
        let changed = || Incoming::CommandsChanged(sid.clone());
        assert!(test.server.handle_incoming(changed()).is_continue());
        assert_eq!(
            test.next_message()["params"]["update"]["sessionUpdate"],
            AVAILABLE_COMMANDS_UPDATE
        );

        assert!(test.server.handle_incoming(changed()).is_continue());

        assert!(test.frames.is_empty());
    }

    #[test]
    fn shutdown_answers_owed_prompts_and_stops_every_session() {
        let mut test = TestServer::with_session();
        let second = test.add_session();
        for (sid, prompt_id) in [(test.sid.clone(), PROMPT_ID), (second, SECOND_PROMPT_ID)] {
            locked(&test.active(&sid).pending).prompt = Some(owed(prompt_id));
        }

        let failures = smol::block_on(test.server.close_all());

        assert!(failures.is_empty());
        assert!(test.server.sessions.is_empty());
        let mut answered: Vec<i64> = test
            .frames
            .drain()
            .map(|frame| {
                assert_eq!(frame.message["result"]["stopReason"], CANCELLED);
                frame.message["id"].as_i64().unwrap()
            })
            .collect();
        answered.sort_unstable();
        assert_eq!(answered, [PROMPT_ID, SECOND_PROMPT_ID]);
    }
}
