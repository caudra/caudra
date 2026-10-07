mod automation_tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use caudra_automation::host::{ActionKind, DeliveryMode};
    use caudra_automation::request::{AutomationRequest, AutomationResponse};
    use caudra_automation::snapshot::{ActionStatus, ArmOrigin};
    use caudra_config::{Feature, FeatureFlags};
    use caudra_providers::provider::{BoxFuture, Provider};
    use caudra_providers::{
        CacheKey, ContentBlock, Message, Model, ModelInfo, ProviderEvent, RequestOptions,
        StopReason, StreamResponse,
    };
    use serde_json::Value;
    use test_case::test_case;

    use super::{
        Agent, History, MockProvider, PendingInput, default_input, make_agent_configured,
        text_response,
    };
    use crate::automation::handle::AutomationHandle;
    use crate::automation::manager::AutomationRuntime;
    use crate::automation::testing::{AutomationFixture, until};
    use crate::tools::ToolAudience;
    use crate::{AgentError, DoneReason, Envelope};

    const GUIDE: &str = "guide";
    const LATE: &str = "late";
    const FIRST_GUIDANCE: &str = "Check the parser first.";
    const SECOND_GUIDANCE: &str = "Then check the lexer.";
    const LATE_GUIDANCE: &str = "Look at the formatter too.";
    const NEXT_WORK: &str = "Write the release notes.";
    const GOAL: &str = "The release notes are written";
    const ARMED: &str = r#"triggers: [#{ kind: "armed" }]"#;
    /// The items [`mixed_body`] queues.
    const QUEUED: usize = 4;
    const TASK_CALL: &str = "call-1";

    const RIDES_ALONG: &str = "guidance must ride with the main run's next request";
    const IN_ORDER: &str = "guidance must reach the model in queue order";
    const RECORDED: &str = "claimed guidance must be recorded delivered, and the rest stay queued";
    const NEXT_WAITS: &str = "next items, goals included, must wait for the session to settle";
    const HELD: &str = "a run that may not claim must leave every item queued";
    const NEVER_EXTENDS: &str =
        "guidance queued during the last response must wait instead of extending the run";
    const NO_FIRING: &str = "the launch firing must be listed";
    const NO_TRACE: &str = "history with a fire_id must answer with the firing's trace";

    /// Queues guidance around a `next` message and a goal.
    fn mixed_body() -> String {
        format!(
            "message(\"{FIRST_GUIDANCE}\", #{{ delivery: \"guide\" }});\n\
             message(\"{NEXT_WORK}\");\n\
             message(\"{SECOND_GUIDANCE}\", #{{ delivery: \"guide\" }});\n\
             set_goal(\"{GOAL}\");"
        )
    }

    /// An agent of the fixture's session with automations on, run as `audience` under the tool
    /// call `root`, which a subagent or a task has.
    fn session_agent<'h>(
        provider: impl Provider + 'static,
        history: &'h mut History,
        fixture: &AutomationFixture,
        audience: ToolAudience,
        root: Option<&str>,
    ) -> (Agent<'h>, flume::Receiver<Envelope>) {
        make_agent_configured(provider, history, |params| {
            params.session_id = Some(fixture.session_id().into());
            params.config.features = FeatureFlags::NONE.with(Feature::Automations);
            params.config.generate_titles = false;
            params.audience = audience;
            params.root_tool_use_id = root.map(str::to_owned);
        })
    }

    fn request_text(request: &[Message]) -> String {
        request
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One run against a runtime whose launch firing queued [`mixed_body`]'s items.
    struct Turn {
        _fixture: AutomationFixture,
        runtime: AutomationRuntime,
        /// The text of the run's only request.
        sent: String,
    }

    async fn turn(audience: ToolAudience, root: Option<&str>, pending_input: bool) -> Turn {
        let fixture = AutomationFixture::default();
        fixture.script(&fixture.user_scripts(), GUIDE, ARMED, &mixed_body());
        let runtime = fixture.spawn(&[GUIDE]).await;
        until(&runtime.handle(), |state| state.outbox.len() == QUEUED).await;
        let provider = MockProvider::new(vec![text_response(StopReason::EndTurn)]);
        let requests = Arc::clone(&provider.captured_messages);
        let mut history = History::default();
        let (mut agent, _events) = session_agent(provider, &mut history, &fixture, audience, root);
        if pending_input {
            agent.interrupt_source = Some(Arc::new(PendingInput(AtomicBool::new(true))));
        }
        assert_eq!(
            agent.run(default_input()).await.unwrap(),
            DoneReason::EndTurn
        );
        let sent = request_text(&requests.lock().unwrap()[0]);
        Turn {
            _fixture: fixture,
            runtime,
            sent,
        }
    }

    /// Each action of the session's one firing, as its summary and status, in call order.
    async fn actions(handle: &AutomationHandle) -> Vec<(String, ActionStatus)> {
        let fire_id = handle
            .state()
            .recent
            .first()
            .expect(NO_FIRING)
            .fire_id
            .clone();
        let trace = AutomationRequest::History {
            name: None,
            fire_id: Some(fire_id),
            limit: None,
        };
        match handle.request(trace).await {
            Ok(AutomationResponse::Firing(detail)) => detail
                .actions
                .iter()
                .map(|action| (action.summary.clone(), action.status))
                .collect(),
            other => panic!("{NO_TRACE}: {other:?}"),
        }
    }

    #[test]
    fn guidance_rides_with_the_main_runs_next_request_in_queue_order() {
        smol::block_on(async {
            let turn = turn(ToolAudience::MAIN, None, false).await;

            let first = turn.sent.find(FIRST_GUIDANCE).expect(RIDES_ALONG);
            let second = turn.sent.find(SECOND_GUIDANCE).expect(RIDES_ALONG);
            assert!(first < second, "{IN_ORDER}");
            assert_eq!(
                actions(&turn.runtime.handle()).await,
                [
                    (FIRST_GUIDANCE.to_owned(), ActionStatus::Delivered),
                    (NEXT_WORK.to_owned(), ActionStatus::Queued),
                    (SECOND_GUIDANCE.to_owned(), ActionStatus::Delivered),
                    (GOAL.to_owned(), ActionStatus::Queued),
                ],
                "{RECORDED}"
            );
            turn.runtime.shutdown().await;
        });
    }

    #[test]
    fn next_items_and_goals_wait_for_the_session_to_settle() {
        smol::block_on(async {
            let turn = turn(ToolAudience::MAIN, None, false).await;

            assert!(!turn.sent.contains(NEXT_WORK), "{NEXT_WAITS}");
            assert!(!turn.sent.contains(GOAL), "{NEXT_WAITS}");
            let waiting: Vec<(ActionKind, DeliveryMode)> = turn
                .runtime
                .handle()
                .state()
                .outbox
                .iter()
                .map(|item| (item.kind, item.delivery))
                .collect();
            assert_eq!(
                waiting,
                [
                    (ActionKind::Message, DeliveryMode::Next),
                    (ActionKind::SetGoal, DeliveryMode::Next),
                ],
                "{NEXT_WAITS}"
            );
            turn.runtime.shutdown().await;
        });
    }

    #[test_case(ToolAudience::GENERAL_SUB, None; "subagent")]
    #[test_case(ToolAudience::MAIN, Some(TASK_CALL); "task")]
    fn a_subagent_or_task_run_claims_nothing(audience: ToolAudience, root: Option<&str>) {
        smol::block_on(async {
            let turn = turn(audience, root, false).await;

            assert!(!turn.sent.contains(FIRST_GUIDANCE), "{HELD}");
            assert_eq!(turn.runtime.handle().state().outbox.len(), QUEUED, "{HELD}");
            turn.runtime.shutdown().await;
        });
    }

    #[test]
    fn nothing_is_claimed_while_input_is_pending() {
        smol::block_on(async {
            let turn = turn(ToolAudience::MAIN, None, true).await;

            assert!(!turn.sent.contains(FIRST_GUIDANCE), "{HELD}");
            assert_eq!(turn.runtime.handle().state().outbox.len(), QUEUED, "{HELD}");
            turn.runtime.shutdown().await;
        });
    }

    /// Arms [`LATE`] while the run's only response streams, and answers once its `armed`
    /// firing queued guidance.
    struct ArmingProvider {
        handle: AutomationHandle,
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
    }

    impl Provider for ArmingProvider {
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
                let arm = AutomationRequest::Arm {
                    name: LATE.to_owned(),
                    args: None,
                    origin: ArmOrigin::Manual,
                };
                self.handle.request(arm).await.unwrap();
                until(&self.handle, |state| !state.outbox.is_empty()).await;
                Ok(text_response(StopReason::EndTurn))
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    #[test]
    fn guidance_queued_during_the_last_response_stays_queued() {
        smol::block_on(async {
            let fixture = AutomationFixture::default();
            let body = format!("message(\"{LATE_GUIDANCE}\", #{{ delivery: \"guide\" }});");
            fixture.script(&fixture.user_scripts(), LATE, ARMED, &body);
            let runtime = fixture.spawn(&[]).await;
            let requests = Arc::default();
            let provider = ArmingProvider {
                handle: runtime.handle(),
                requests: Arc::clone(&requests),
            };
            let mut history = History::default();
            let (mut agent, _events) =
                session_agent(provider, &mut history, &fixture, ToolAudience::MAIN, None);

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );

            let requests = requests.lock().unwrap().clone();
            assert_eq!(requests.len(), 1, "{NEVER_EXTENDS}");
            assert!(
                !request_text(&requests[0]).contains(LATE_GUIDANCE),
                "{NEVER_EXTENDS}"
            );
            let waiting: Vec<DeliveryMode> = runtime
                .handle()
                .state()
                .outbox
                .iter()
                .map(|item| item.delivery)
                .collect();
            assert_eq!(waiting, [DeliveryMode::Guide], "{NEVER_EXTENDS}");
            runtime.shutdown().await;
        });
    }
}
