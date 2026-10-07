mod owned_jobs_tests {
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::{Pin, pin};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use caudra_config::ExecutionMode;
    use caudra_providers::provider::{BoxFuture, Provider};
    use caudra_providers::{
        CacheKey, ContentBlock, Message, Model, ModelInfo, ProviderEvent, RequestOptions, Role,
        StandingReminderKind, StopReason, StreamResponse, estimate_tokens,
    };
    use caudra_storage::background::ShellJobMetadata;
    use caudra_storage::id::CaudraId;
    use caudra_storage::sessions::{SessionDatabase, SessionError, SessionLease};
    use caudra_storage::tool_outputs::ToolOutputStore;
    use caudra_storage::{StateDir, random_task_id};
    use futures_lite::future::{or, poll_once};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        Agent, History, MockProvider, default_input, empty_response, install_todo_tool, make_agent,
        make_agent_with_output_store, mixed_todos, text_response, todo_reminders,
        tool_use_response,
    };
    use crate::agent::subagent::TaskIdentity;
    use crate::agent::task_runner::{
        ModelResolver, SubagentTaskRunner, TaskRequest, TaskRunner, WorkflowHostContext, run_task,
    };
    use crate::background::{BackgroundTasks, JobScope};
    use crate::cancel::CancelMap;
    use crate::tools::registry::{
        ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolInvocation, ToolSource,
    };
    use crate::tools::{
        DescriptionContext, SHELL_TOOL_NAME, ToolAudience, ToolContext, ToolEffect, local_tool,
    };
    use crate::{
        AgentError, AgentEvent, AgentMode, CancelToken, DoneReason, Envelope, StoredSession,
        SubagentHistoryStore, TextOutput, ToolDoneEvent, ToolOutput,
    };

    const OWNER: &str = "child-shell-invocation";
    const OTHER_OWNER: &str = "other-child-shell-invocation";
    const CALL: &str = "t1";
    const OTHER_CALL: &str = "t2";
    const LOCAL_TOOL: &str = "launch_owned_test_job";
    const COMMAND: &str = "printf owned-result";
    const ADMITTED: &str = "command admitted";
    const OUTPUT: &str = "owned-result";
    const EARLY: &str = "The command has been admitted.";
    const FINAL: &str = "The command finished and I inspected its result.";
    const MODEL: &str = "test-model";
    const TIMEOUT_MS: u64 = 120_000;
    const WATCHDOG: Duration = Duration::from_secs(15);
    const STALLED: &str = "owned job test did not reach its channel barrier";
    const PREMATURE_DONE: &str = "child finished before its owned command was observed or drained";
    const UNEXPECTED_MODELS: &str = "owned job fixture must not request model discovery";
    const CANCELLED: &str = "command cancelled and cleanup completed";
    const JOB_REEXECUTED: &str = "the child launched the same controlled command twice";
    const LARGE_OUTPUT_LINES: usize = 4096;
    const UNACCEPTED_RECEIPT: &str = "the child shell receipt must be accepted from its own history before another provider request";
    const SHELL_HOLDS_REMINDER: &str =
        "a running background shell hands control back without a todo reminder";
    const REPORT_BEFORE_REMINDER: &str =
        "the finished shell's report reaches the model before the todo reminder";
    const SETTLING_DOES_NOT_HOLD: &str =
        "a report still awaiting acknowledgement does not hold the reminder back";

    struct OutputShell;

    impl Tool for OutputShell {
        fn name(&self) -> &str {
            SHELL_TOOL_NAME
        }

        fn description(&self, _: &DescriptionContext) -> Cow<'_, str> {
            Cow::Borrowed(COMMAND)
        }

        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]})
        }

        fn parse(&self, _: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(Self))
        }
    }

    impl ToolInvocation for OutputShell {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain(COMMAND.into()))
        }

        fn shell_timeout(&self) -> Option<Duration> {
            Some(Duration::from_millis(TIMEOUT_MS))
        }

        fn execute<'a>(self: Box<Self>, _: &'a ToolContext) -> ExecFuture<'a> {
            Box::pin(async {
                Ok(ToolOutput::Plain(TextOutput {
                    text: format!("{OUTPUT}\n").repeat(LARGE_OUTPUT_LINES),
                    instructions: None,
                    state: None,
                    lua_provenance: None,
                }))
                .into()
            })
        }
    }

    #[test_case(false; "task_child")]
    #[test_case(true; "workflow_child")]
    fn constructed_children_persist_owned_shell_output_in_the_session_store(workflow: bool) {
        smol::block_on(bounded(async {
            let fixture = Fixture::new().await;
            let store = Arc::new(ToolOutputStore::new(fixture.dir.clone()));
            let (provider, requests, responses) = provider();
            let mut history = History::default();
            let (mut agent, _events) =
                make_agent_with_output_store(provider, &mut history, Some(store));
            agent.session_id = Some(fixture.session.id.into());
            agent.jobs = Some(fixture.tasks.main_scope());
            agent.config.shell_execution = ExecutionMode::Async;
            agent
                .registry
                .register_audited(
                    Arc::new(OutputShell),
                    ToolSource::Native {
                        owner: SHELL_TOOL_NAME.into(),
                        contract: SHELL_TOOL_NAME.into(),
                        trusted: true,
                    },
                    ToolEffect::ReadOnly,
                )
                .unwrap();
            let ctx = agent.tool_context();
            let model: ModelResolver = Arc::new({
                let provider = Arc::clone(&ctx.provider);
                let model = Arc::clone(&ctx.model);
                move || (Arc::clone(&provider), Arc::clone(&model))
            });
            let runner = SubagentTaskRunner::new(Arc::new(WorkflowHostContext::from_tool_context(
                &ctx,
                model,
                Arc::new(|| AgentMode::Build),
                Arc::new(CancelMap::new()),
            )));
            let request = TaskRequest {
                prompt: Some(COMMAND.into()),
                label: OWNER.into(),
                task: TaskIdentity::Fresh(fixture.task_id.clone()),
                mode: None,
                profile: None,
                model_job: None,
                output_schema: None,
                call_id: OWNER.into(),
                provenance: None,
            };
            let mut run = pin!(async {
                if workflow {
                    runner
                        .run(request, CancelToken::none(), ctx.event_tx.clone())
                        .await
                } else {
                    run_task(&ctx, request).await
                }
            });
            drive(run.as_mut(), requests.recv_async()).await.unwrap();
            responses
                .send(tool_use_response(
                    SHELL_TOOL_NAME,
                    json!({"command": COMMAND}),
                ))
                .unwrap();
            loop {
                let request = drive(run.as_mut(), requests.recv_async()).await.unwrap();
                let records = SessionDatabase::open(&fixture.dir)
                    .unwrap()
                    .background_tasks(fixture.session.id)
                    .unwrap();
                assert_eq!(records.len(), 1);
                assert!(
                    records[0].receipt_accepted,
                    "{UNACCEPTED_RECEIPT}: {:?}",
                    records[0].payload
                );
                if request.iter().any(|message| message.task_event.is_some()) {
                    responses.send(response(FINAL)).unwrap();
                    break;
                }
                responses.send(response(EARLY)).unwrap();
            }
            let outcome = run.await;
            assert!(outcome.success, "{outcome:?}");
            assert_eq!(outcome.output, json!(FINAL));
            fixture.tasks.shutdown().await.unwrap();
            let restarted = BackgroundTasks::spawn(fixture.dir.clone(), fixture.session.id)
                .await
                .unwrap();
            let records = SessionDatabase::open(&fixture.dir)
                .unwrap()
                .background_tasks(fixture.session.id)
                .unwrap();
            assert_eq!(records.len(), 1);
            let reference = records[0].output_ref.as_ref().unwrap();
            let expected = format!("{OUTPUT}\n").repeat(LARGE_OUTPUT_LINES);
            let reopened = ToolOutputStore::new(fixture.dir.clone());
            assert_eq!(
                reopened
                    .load_text(fixture.session.id, reference.id.clone())
                    .unwrap(),
                expected
            );
            let checkpoint = fixture.checkpoint(OWNER);
            assert!(
                checkpoint
                    .iter()
                    .filter(|message| message.task_event.is_some())
                    .flat_map(|message| &message.retained_output_refs)
                    .any(|retained| retained == reference)
            );
            let fork = CaudraId::generate();
            reopened
                .copy_session_outputs(fixture.session.id, fork, std::slice::from_ref(reference))
                .unwrap();
            assert_eq!(
                reopened.load_text(fork, reference.id.clone()).unwrap(),
                expected
            );
            restarted.shutdown().await.unwrap();
        }));
    }

    #[test_case(0; "state_changes_only")]
    #[test_case(1; "each_response_group")]
    fn child_reminders_are_owner_scoped_and_restored_after_compaction(cadence: u32) {
        smol::block_on(bounded(async {
            let fixture = Fixture::new().await;
            let scopes = [
                fixture.tasks.child_scope(OWNER),
                fixture.tasks.child_scope(OTHER_OWNER),
                fixture.tasks.main_scope(),
            ];
            let mut jobs = Vec::new();
            let mut controls = Vec::new();
            for scope in &scopes {
                let (execution, control) = execution();
                let job = scope
                    .admit_shell(metadata(), &fixture.history, move |cancel, _| {
                        execution.run(cancel)
                    })
                    .await
                    .unwrap();
                loop {
                    let revision = scope.revision();
                    if scope.status(&job.task_id).unwrap().state == "running" {
                        break;
                    }
                    scope.wait_for_change(revision).await.unwrap();
                }
                jobs.push(job);
                controls.push(control);
            }
            let provider = MockProvider::new((0..4).map(|_| response(FINAL)).collect());
            let captured = Arc::clone(&provider.captured_messages);
            let mut history = History::new(vec![Message::user(COMMAND.into())]);
            let (mut agent, _events) = make_agent(provider, &mut history);
            fixture.bind(&mut agent, OWNER);
            agent.root_tool_use_id = Some(OWNER.into());
            agent.config.background_reminder_turns = cadence;
            agent.turn().await.unwrap();
            agent.turn().await.unwrap();
            {
                let requests = captured.lock().unwrap();
                for (index, request) in requests.iter().enumerate() {
                    let reminders = request
                        .iter()
                        .filter(|message| {
                            message.standing_reminder == Some(StandingReminderKind::BackgroundWork)
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(reminders.len(), 1 + index * usize::from(cadence != 0));
                    for reminder in reminders {
                        let text = reminder.first_text_content().unwrap();
                        assert!(text.contains(&jobs[0].task_id));
                        assert!(!text.contains(&jobs[1].task_id));
                        assert!(!text.contains(&jobs[2].task_id));
                    }
                }
            }
            agent.do_compact().await.unwrap();
            agent.turn().await.unwrap();
            {
                let requests = captured.lock().unwrap();
                let reminders = requests
                    .last()
                    .unwrap()
                    .iter()
                    .filter(|message| {
                        message.standing_reminder == Some(StandingReminderKind::BackgroundWork)
                    })
                    .collect::<Vec<_>>();
                assert_eq!(reminders.len(), 1);
                let text = reminders[0].first_text_content().unwrap();
                assert!(text.contains(&jobs[0].task_id));
                assert!(!text.contains(&jobs[1].task_id));
                assert!(!text.contains(&jobs[2].task_id));
            }
            for control in controls {
                control.finish.send(()).unwrap();
            }
            for scope in &scopes {
                terminal(scope).await;
            }
            fixture.tasks.shutdown().await.unwrap();
        }));
    }

    #[test]
    fn a_running_background_shell_holds_back_the_todo_reminder() {
        smol::block_on(bounded(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.main_scope();
            let (execution, control) = execution();
            let job = scope
                .admit_shell(metadata(), &fixture.history, move |cancel, _| {
                    execution.run(cancel)
                })
                .await
                .unwrap();
            loop {
                let revision = scope.revision();
                if scope.status(&job.task_id).unwrap().state == "running" {
                    break;
                }
                scope.wait_for_change(revision).await.unwrap();
            }
            let mut history = receipt_history().with_todos(Some(mixed_todos()));
            let reminded = |request: &[Message]| {
                request.iter().any(|message| {
                    message.standing_reminder == Some(StandingReminderKind::OpenTodos)
                })
            };

            let requests = run_main(&mut history, &fixture.tasks, 1).await;
            assert_eq!(requests.len(), 1, "{SHELL_HOLDS_REMINDER}");
            assert_eq!(todo_reminders(&history), 0, "{SHELL_HOLDS_REMINDER}");

            control.finish.send(()).unwrap();
            terminal(&scope).await;
            let requests = run_main(&mut history, &fixture.tasks, 3).await;
            assert_eq!(requests.len(), 3);
            assert!(
                requests[1]
                    .iter()
                    .any(|message| message.task_event.is_some())
                    && !reminded(&requests[1]),
                "{REPORT_BEFORE_REMINDER}"
            );
            assert!(reminded(&requests[2]));
            assert_eq!(todo_reminders(&history), 1);
            assert!(fixture.tasks.work().settling, "{SETTLING_DOES_NOT_HOLD}");
            fixture.tasks.shutdown().await.unwrap();
        }));
    }

    async fn run_main(
        history: &mut History,
        tasks: &BackgroundTasks,
        answers: usize,
    ) -> Vec<Vec<Message>> {
        let provider = MockProvider::new((0..answers).map(|_| response(FINAL)).collect());
        let requests = Arc::clone(&provider.captured_messages);
        let (mut agent, _events) = make_agent(provider, history);
        install_todo_tool(&agent);
        agent.background = Some(tasks.clone());
        assert_eq!(
            agent.run(default_input()).await.unwrap(),
            DoneReason::EndTurn
        );
        drop(agent);
        std::mem::take(&mut *requests.lock().unwrap())
    }

    struct Fixture {
        _lease: SessionLease,
        _temp: TempDir,
        dir: StateDir,
        session: StoredSession,
        task_id: String,
        tasks: BackgroundTasks,
        history: SubagentHistoryStore,
    }

    impl Fixture {
        async fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let dir = StateDir::from_path(temp.path().to_owned());
            let mut session = StoredSession::new(MODEL, temp.path().to_str().unwrap());
            session.save(&dir).unwrap();
            let lease = SessionLease::acquire(&dir, session.id).unwrap();
            let tasks = BackgroundTasks::spawn(dir.clone(), session.id)
                .await
                .unwrap();
            Self {
                _lease: lease,
                _temp: temp,
                dir,
                session,
                task_id: random_task_id().unwrap(),
                tasks,
                history: SubagentHistoryStore::default(),
            }
        }

        fn bind(&self, agent: &mut Agent<'_>, owner: &str) {
            agent.subagent_history = self.history.clone();
            agent.jobs = Some(self.tasks.child_scope(owner));
            agent.task_id = Some(self.task_id.clone());
            agent.session_id = Some(self.session.id.into());
            agent.tool_output_store = Some(Arc::new(ToolOutputStore::new(self.dir.clone())));
            agent.audience = ToolAudience::GENERAL_SUB;
            agent.auto_compact = false;
        }

        fn checkpoint(&self, owner: &str) -> Vec<Message> {
            let items = SessionDatabase::open(&self.dir)
                .unwrap()
                .load_job_owner_checkpoint(self.session.id, owner, &self.task_id)
                .unwrap()
                .unwrap();
            History::restored(items).unwrap().into_vec()
        }
    }

    struct ControlledProvider {
        requests: flume::Sender<Vec<Message>>,
        responses: flume::Receiver<StreamResponse>,
    }

    impl Provider for ControlledProvider {
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
                self.requests.send_async(messages.to_vec()).await.unwrap();
                Ok(self.responses.recv_async().await.unwrap())
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async {
                Err(AgentError::Config {
                    message: UNEXPECTED_MODELS.into(),
                })
            })
        }
    }

    fn provider() -> (
        ControlledProvider,
        flume::Receiver<Vec<Message>>,
        flume::Sender<StreamResponse>,
    ) {
        let (request_tx, requests) = flume::bounded(1);
        let (responses, response_rx) = flume::bounded(1);
        (
            ControlledProvider {
                requests: request_tx,
                responses: response_rx,
            },
            requests,
            responses,
        )
    }

    struct Execution {
        finish: flume::Receiver<()>,
        cancelled: flume::Sender<()>,
        allow_cleanup: flume::Receiver<()>,
        cleaned: Arc<AtomicBool>,
    }

    struct Control {
        finish: flume::Sender<()>,
        cancelled: flume::Receiver<()>,
        allow_cleanup: flume::Sender<()>,
        cleaned: Arc<AtomicBool>,
    }

    fn execution() -> (Execution, Control) {
        let (finish_tx, finish_rx) = flume::bounded(1);
        let (cancelled_tx, cancelled_rx) = flume::bounded(1);
        let (cleanup_tx, cleanup_rx) = flume::bounded(1);
        let cleaned = Arc::new(AtomicBool::new(false));
        (
            Execution {
                finish: finish_rx,
                cancelled: cancelled_tx,
                allow_cleanup: cleanup_rx,
                cleaned: Arc::clone(&cleaned),
            },
            Control {
                finish: finish_tx,
                cancelled: cancelled_rx,
                allow_cleanup: cleanup_tx,
                cleaned,
            },
        )
    }

    impl Execution {
        async fn run(self, cancel: CancelToken) -> ToolDoneEvent {
            let cancelled = cancel.race(self.finish.recv_async()).await.is_err();
            if cancelled {
                self.cancelled.send_async(()).await.unwrap();
                self.allow_cleanup.recv_async().await.unwrap();
            }
            self.cleaned.store(true, Ordering::Release);
            let mut done =
                ToolDoneEvent::error(CALL.into(), if cancelled { CANCELLED } else { OUTPUT });
            done.is_error = cancelled;
            done
        }
    }

    fn metadata() -> ShellJobMetadata {
        ShellJobMetadata {
            call_id: CALL.into(),
            root_call_id: CALL.into(),
            command: COMMAND.into(),
            workdir: ".".into(),
            timeout_ms: TIMEOUT_MS,
            mode: "build".into(),
        }
    }

    fn receipt_history() -> History {
        receipt_history_for(CALL)
    }

    fn receipt_history_for(call: &str) -> History {
        History::new(vec![
            Message::user(COMMAND.into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(call, LOCAL_TOOL, json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: call.into(),
                    content: ADMITTED.into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
        ])
    }

    fn response(text: &str) -> StreamResponse {
        let mut response = text_response(StopReason::EndTurn);
        response.message.content = vec![ContentBlock::Text { text: text.into() }];
        response
    }

    async fn bounded<T>(future: impl Future<Output = T>) -> T {
        estimate_tokens(COMMAND);
        or(future, async {
            smol::Timer::after(WATCHDOG).await;
            panic!("{STALLED}")
        })
        .await
    }

    async fn drive<F: Future, T>(run: Pin<&mut F>, next: impl Future<Output = T>) -> T {
        or(
            async {
                run.await;
                panic!("{PREMATURE_DONE}")
            },
            next,
        )
        .await
    }

    async fn turns(events: &flume::Receiver<Envelope>, count: usize) {
        let mut completed = 0;
        while completed < count {
            match events.recv_async().await.unwrap().event {
                AgentEvent::TurnComplete(_) => completed += 1,
                AgentEvent::Done { .. } => panic!("{PREMATURE_DONE}"),
                _ => {}
            }
        }
    }

    fn no_done(events: &flume::Receiver<Envelope>) {
        assert!(
            !events
                .try_iter()
                .any(|event| matches!(event.event, AgentEvent::Done { .. })),
            "{PREMATURE_DONE}"
        );
    }

    async fn terminal(scope: &JobScope) {
        loop {
            let revision = scope.revision();
            if scope
                .list()
                .iter()
                .all(|job| !matches!(job.state.as_str(), "queued" | "running" | "cancelling"))
            {
                return;
            }
            scope.wait_for_change(revision).await.unwrap();
        }
    }

    async fn task_terminal(scope: &JobScope, task_id: &str) {
        loop {
            let revision = scope.revision();
            if !matches!(
                scope.status(task_id).unwrap().state.as_str(),
                "queued" | "running" | "cancelling"
            ) {
                return;
            }
            scope.wait_for_change(revision).await.unwrap();
        }
    }

    fn no_completion_or_success(events: &flume::Receiver<Envelope>) {
        assert!(
            !events.try_iter().any(|event| matches!(
                event.event,
                AgentEvent::Done { .. }
                    | AgentEvent::Injected {
                        task_event: Some(_),
                        ..
                    }
            )),
            "{PREMATURE_DONE}"
        );
    }

    #[test_case(OWNER; "checkpoint_binding_refusal")]
    fn failed_owner_checkpoint_releases_claim_and_drains_without_success(owner: &str) {
        smol::block_on(bounded(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.child_scope(owner);
            let (completed_execution, completed_control) = execution();
            let completed = scope
                .admit_shell(metadata(), &fixture.history, move |cancel, _| {
                    completed_execution.run(cancel)
                })
                .await
                .unwrap();
            let (active_execution, active_control) = execution();
            let active_metadata = ShellJobMetadata {
                call_id: OTHER_CALL.into(),
                ..metadata()
            };
            scope
                .admit_shell(active_metadata, &fixture.history, move |cancel, _| {
                    active_execution.run(cancel)
                })
                .await
                .unwrap();
            let mut history = receipt_history();
            let (mut agent, events) = make_agent(MockProvider::new(Vec::new()), &mut history);
            fixture.bind(&mut agent, owner);
            agent.checkpoint_owned_jobs().await.unwrap();
            let previous = serde_json::to_value(fixture.checkpoint(owner)).unwrap();
            let conflicting_task = loop {
                let candidate = random_task_id().unwrap();
                if candidate != fixture.task_id {
                    break candidate;
                }
            };
            let expected_error = SessionError::JobOwnerTaskMismatch {
                invocation_id: owner.into(),
                task_id: conflicting_task.clone(),
            }
            .to_string();
            agent.task_id = Some(conflicting_task);
            agent.report_ready = Some(Arc::new(AtomicBool::new(true)));
            completed_control.finish.send(()).unwrap();
            task_terminal(&scope, &completed.task_id).await;

            let error = agent.inject_owned_results().await.unwrap_err();
            assert!(matches!(error, AgentError::Tool { message, .. } if message == expected_error));
            assert_eq!(
                serde_json::to_value(agent.history.as_slice()).unwrap(),
                previous
            );
            no_completion_or_success(&events);
            let reclaimed = scope.claim_messages().unwrap();
            let completion = reclaimed
                .iter()
                .find_map(|message| message.task_event.as_ref())
                .unwrap();
            assert_eq!(completion.invocation_id, completed.invocation_id);
            let event_id = completion.event_id.clone();
            scope.release_messages(&reclaimed);
            assert!(
                !SessionDatabase::open(&fixture.dir)
                    .unwrap()
                    .background_event_accepted(fixture.session.id, &event_id)
                    .unwrap()
            );

            {
                let mut run = pin!(agent.run(default_input()));
                drive(run.as_mut(), active_control.cancelled.recv_async())
                    .await
                    .unwrap();
                assert!(poll_once(run.as_mut()).await.is_none(), "{PREMATURE_DONE}");
                assert!(!active_control.cleaned.load(Ordering::Acquire));
                no_completion_or_success(&events);
                active_control.allow_cleanup.send(()).unwrap();
                assert!(
                    matches!(run.await, Err(AgentError::Tool { message, .. }) if message == expected_error)
                );
            }
            assert!(active_control.cleaned.load(Ordering::Acquire));
            assert!(completed_control.cleaned.load(Ordering::Acquire));
            assert_eq!(fixture.tasks.active_count(), 0);
            assert_eq!(agent.response_text(), None);
            assert!(
                agent
                    .history
                    .as_slice()
                    .iter()
                    .all(|message| message.task_event.is_none())
            );
            no_completion_or_success(&events);
            assert!(scope.claim_messages().unwrap().is_empty());
            let database = SessionDatabase::open(&fixture.dir).unwrap();
            assert!(
                !database
                    .background_event_accepted(fixture.session.id, &event_id)
                    .unwrap()
            );
            let records = database.background_tasks(fixture.session.id).unwrap();
            assert_eq!(records.len(), 2);
            assert!(records.iter().all(|record| {
                !record.events.is_empty()
                    && record
                        .events
                        .iter()
                        .all(|event| event.suppressed && !event.accepted)
            }));
            assert_eq!(
                serde_json::to_value(fixture.checkpoint(owner)).unwrap(),
                previous
            );
            fixture.tasks.shutdown().await.unwrap();
        }));
    }

    #[test_case(OWNER, OTHER_OWNER; "continued_friendly_task_id")]
    fn reused_child_task_keeps_checkpoint_and_cancellation_scoped_to_invocation(
        previous_owner: &str,
        next_owner: &str,
    ) {
        smol::block_on(bounded(async {
            let fixture = Fixture::new().await;
            let previous_scope = fixture.tasks.child_scope(previous_owner);
            let (previous_execution, previous_control) = execution();
            let previous_job = previous_scope
                .admit_shell(metadata(), &fixture.history, move |cancel, _| {
                    previous_execution.run(cancel)
                })
                .await
                .unwrap();
            let mut previous_history = receipt_history();
            let (mut previous_agent, _previous_events) =
                make_agent(MockProvider::new(Vec::new()), &mut previous_history);
            fixture.bind(&mut previous_agent, previous_owner);
            previous_agent.checkpoint_owned_jobs().await.unwrap();
            previous_control.finish.send(()).unwrap();
            terminal(&previous_scope).await;
            assert!(previous_agent.inject_owned_results().await.unwrap());
            let previous_checkpoint = fixture.checkpoint(previous_owner);
            let previous_json = serde_json::to_value(&previous_checkpoint).unwrap();
            previous_scope.cancel_and_drain().await.unwrap();

            let next_scope = fixture.tasks.child_scope(next_owner);
            let (next_execution, next_control) = execution();
            let next_metadata = ShellJobMetadata {
                call_id: OTHER_CALL.into(),
                root_call_id: OTHER_CALL.into(),
                ..metadata()
            };
            let next_job = next_scope
                .admit_shell(next_metadata, &fixture.history, move |cancel, _| {
                    next_execution.run(cancel)
                })
                .await
                .unwrap();
            let mut continued_messages = previous_checkpoint;
            continued_messages.extend_from_slice(receipt_history_for(OTHER_CALL).as_slice());
            let mut next_history = History::new(continued_messages);
            let (mut next_agent, _next_events) =
                make_agent(MockProvider::new(Vec::new()), &mut next_history);
            fixture.bind(&mut next_agent, next_owner);
            assert_eq!(previous_agent.task_id, next_agent.task_id);
            next_agent.checkpoint_owned_jobs().await.unwrap();
            assert!(previous_scope.status(&next_job.task_id).is_err());
            assert!(next_scope.status(&previous_job.task_id).is_err());
            previous_scope.cancel_and_drain().await.unwrap();
            assert!(!next_control.cleaned.load(Ordering::Acquire));
            assert!(next_control.cancelled.try_recv().is_err());
            assert!(next_scope.pending());
            next_control.finish.send(()).unwrap();
            terminal(&next_scope).await;
            assert!(!previous_agent.inject_owned_results().await.unwrap());
            assert!(next_agent.inject_owned_results().await.unwrap());
            assert!(!next_scope.pending());
            assert_eq!(
                serde_json::to_value(fixture.checkpoint(previous_owner)).unwrap(),
                previous_json
            );
            let next_checkpoint = fixture.checkpoint(next_owner);
            let origins = next_checkpoint
                .iter()
                .filter_map(|message| message.task_event.as_ref())
                .collect::<Vec<_>>();
            assert_eq!(origins.len(), 2);
            assert!(
                origins
                    .iter()
                    .any(|origin| origin.invocation_id == previous_job.invocation_id)
            );
            assert!(
                origins
                    .iter()
                    .any(|origin| origin.invocation_id == next_job.invocation_id)
            );
            let database = SessionDatabase::open(&fixture.dir).unwrap();
            for origin in origins {
                assert!(
                    database
                        .background_event_accepted(fixture.session.id, &origin.event_id)
                        .unwrap()
                );
            }
            fixture.tasks.shutdown().await.unwrap();
        }));
    }

    #[test_case(false; "early_prose")]
    #[test_case(true; "early_structured_report")]
    fn child_parks_without_polling_and_observes_completion_before_success(structured: bool) {
        smol::block_on(bounded(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.child_scope(OWNER);
            let (provider, requests, responses) = provider();
            let (execution, control) = execution();
            let execution = Arc::new(Mutex::new(Some(execution)));
            let mut history = History::default();
            let (mut agent, events) = make_agent(provider, &mut history);
            fixture.bind(&mut agent, OWNER);
            let ready = Arc::new(AtomicBool::new(structured));
            agent.report_ready = Some(Arc::clone(&ready));
            agent.tools = json!([{"name": LOCAL_TOOL, "input_schema": {"type": "object"}}]);
            let tool_scope = scope.clone();
            agent.local_tools = Arc::new(HashMap::from([(
                LOCAL_TOOL.into(),
                local_tool(move |_, ctx| {
                    let scope = tool_scope.clone();
                    let history = ctx.subagent_history.clone();
                    let execution = execution.lock().unwrap().take().expect(JOB_REEXECUTED);
                    Box::pin(async move {
                        scope
                            .admit_shell(metadata(), &history, move |cancel, _| {
                                execution.run(cancel)
                            })
                            .await?;
                        Ok(ADMITTED.into())
                    })
                }),
            )]));
            {
                let mut run = pin!(agent.run(default_input()));
                drive(run.as_mut(), requests.recv_async()).await.unwrap();
                responses
                    .send(tool_use_response(LOCAL_TOOL, json!({})))
                    .unwrap();
                drive(run.as_mut(), requests.recv_async()).await.unwrap();
                let checkpoint = fixture.checkpoint(OWNER);
                assert!(checkpoint.iter().flat_map(|message| &message.content).any(|block| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == CALL)));
                let records = SessionDatabase::open(&fixture.dir)
                    .unwrap()
                    .background_tasks(fixture.session.id)
                    .unwrap();
                assert_eq!(records.len(), 1);
                assert!(records[0].receipt_accepted);
                responses
                    .send(if structured {
                        empty_response()
                    } else {
                        response(EARLY)
                    })
                    .unwrap();
                drive(run.as_mut(), turns(&events, 2)).await;
                assert!(poll_once(run.as_mut()).await.is_none(), "{PREMATURE_DONE}");
                assert!(requests.try_recv().is_err());
                no_done(&events);
                assert!(scope.pending());
                control.finish.send(()).unwrap();
                let request = drive(run.as_mut(), requests.recv_async()).await.unwrap();
                let event = request
                    .iter()
                    .find_map(|message| message.task_event.as_ref())
                    .unwrap();
                assert!(
                    SessionDatabase::open(&fixture.dir)
                        .unwrap()
                        .background_event_accepted(fixture.session.id, &event.event_id)
                        .unwrap()
                );
                assert!(fixture.checkpoint(OWNER).iter().any(|message| {
                    message
                        .task_event
                        .as_ref()
                        .is_some_and(|origin| origin.event_id == event.event_id)
                }));
                assert!(!ready.load(Ordering::Acquire));
                assert!(!scope.pending());
                responses.send(response(FINAL)).unwrap();
                assert_eq!(run.await.unwrap(), DoneReason::EndTurn);
            }
            assert_eq!(agent.response_text(), Some(FINAL));
            assert_eq!(agent.num_turns, 3);
            assert!(control.cleaned.load(Ordering::Acquire));
            assert!(events.try_iter().any(|event| matches!(
                event.event,
                AgentEvent::Done {
                    reason: DoneReason::EndTurn,
                    ..
                }
            )));
            fixture.tasks.shutdown().await.unwrap();
        }));
    }

    #[test_case(OWNER, OTHER_OWNER; "child_and_main_isolation")]
    fn child_injection_cannot_claim_another_owner(owned: &str, other: &str) {
        smol::block_on(bounded(async {
            let fixture = Fixture::new().await;
            let own_scope = fixture.tasks.child_scope(owned);
            let other_scope = fixture.tasks.child_scope(other);
            let mut other_history = receipt_history();
            let (mut other_agent, _other_events) =
                make_agent(MockProvider::new(Vec::new()), &mut other_history);
            fixture.bind(&mut other_agent, other);
            other_scope
                .admit_shell(metadata(), &fixture.history, |_, _| async {
                    let mut done = ToolDoneEvent::error(CALL.into(), OUTPUT);
                    done.is_error = false;
                    done
                })
                .await
                .unwrap();
            other_agent.checkpoint_owned_jobs().await.unwrap();
            terminal(&other_scope).await;
            let mut history = History::default();
            let (mut agent, _events) = make_agent(MockProvider::new(Vec::new()), &mut history);
            fixture.bind(&mut agent, owned);
            assert!(!agent.inject_owned_results().await.unwrap());
            assert!(fixture.tasks.claim_messages().unwrap().is_empty());
            assert!(!own_scope.pending());
            assert!(other_scope.pending());
            assert!(other_agent.inject_owned_results().await.unwrap());
            assert!(!other_scope.pending());
            assert!(
                agent
                    .history
                    .as_slice()
                    .iter()
                    .all(|message| message.task_event.is_none())
            );
            assert_eq!(
                other_agent
                    .history
                    .as_slice()
                    .iter()
                    .filter(|message| message.task_event.is_some())
                    .count(),
                1
            );
            fixture.tasks.shutdown().await.unwrap();
        }));
    }

    #[test_case(DoneReason::Cancelled, false; "cancelled")]
    #[test_case(DoneReason::MaxTurns, false; "hard_turn_limit")]
    #[test_case(DoneReason::EndTurn, true; "terminal_blocked_report")]
    fn child_terminal_exit_drains_execution_before_done(expected: DoneReason, blocked: bool) {
        smol::block_on(bounded(async {
            let fixture = Fixture::new().await;
            let scope = fixture.tasks.child_scope(OWNER);
            let (execution, control) = execution();
            scope
                .admit_shell(metadata(), &fixture.history, move |cancel, _| {
                    execution.run(cancel)
                })
                .await
                .unwrap();
            let (provider, requests, responses) = provider();
            let mut history = receipt_history();
            let (mut agent, events) = make_agent(provider, &mut history);
            fixture.bind(&mut agent, OWNER);
            agent.checkpoint_owned_jobs().await.unwrap();
            agent.config.max_turns = Some(1);
            agent.terminal_report = Some(Arc::new(AtomicBool::new(blocked)));
            let (cancel, token) = CancelToken::new();
            agent.cancel = token;
            {
                let mut run = pin!(agent.run(default_input()));
                drive(run.as_mut(), requests.recv_async()).await.unwrap();
                if expected == DoneReason::Cancelled {
                    cancel.cancel();
                } else {
                    responses.send(response(EARLY)).unwrap();
                }
                drive(run.as_mut(), control.cancelled.recv_async())
                    .await
                    .unwrap();
                assert!(!control.cleaned.load(Ordering::Acquire));
                assert!(poll_once(run.as_mut()).await.is_none(), "{PREMATURE_DONE}");
                no_done(&events);
                assert!(requests.try_recv().is_err());
                control.allow_cleanup.send(()).unwrap();
                assert_eq!(run.await.unwrap(), expected);
            }
            assert!(control.cleaned.load(Ordering::Acquire));
            assert_eq!(fixture.tasks.active_count(), 0);
            assert!(events.try_iter().any(
                |event| matches!(event.event, AgentEvent::Done { reason, .. } if reason == expected)
            ));
            assert!(
                scope
                    .admit_shell(metadata(), &fixture.history, |_, _| async {
                        ToolDoneEvent::error(CALL.into(), OUTPUT)
                    })
                    .await
                    .is_err()
            );
            fixture.tasks.shutdown().await.unwrap();
        }));
    }
}
