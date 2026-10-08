use crate::tools::native::question::QuestionTool as QnQuestionTool;
use caudra_storage::id::CaudraId as QnSessionId;
use tempfile::TempDir as QnTempDir;

const QN_REQUEST: &str = "Help me select an interface for this tool.";
const QN_REPLY: &str = "The terminal interface is simpler. Should I use the terminal or a browser?";
const QN_REPEAT: &str = "Please confirm whether I should implement the terminal interface.";
const QN_NEXT_REQUEST: &str = "Now help me choose a storage format.";
const QN_FINAL: &str = "I will use the terminal interface you selected.";
const QN_SUMMARY: &str = "The assistant is waiting for an interface choice.";
const QN_CHOICE: &str = "Terminal";
const QN_ANSWER: &str = r#"[["Terminal"]]"#;
const QN_THRESHOLD: f64 = 0.85;
const QN_POSITIVE: f64 = 0.99;
const QN_BELOW_THRESHOLD: f64 = 0.849;
const QN_TIMEOUT_MS: u64 = 30_000;
const QN_CALL_COUNT: &str = "only eligible handoffs may reach the decision engine";
const QN_NO_REMINDER: &str = "an ineligible or stale handoff must not inject a reminder";
const QN_ONE_REMINDER: &str = "a genuine user-input episode allows one corrective reminder";
const QN_VISIBLE_REMINDER: &str = "the correction must be a visible injected event";

fn qn_config(mode: FeatureMode) -> DecisionsConfig {
    let mut config = DecisionsConfig {
        base_url: Some(DECISION_BASE_URL.parse().unwrap()),
        log: true,
        timeout_ms: QN_TIMEOUT_MS,
        ..DecisionsConfig::default()
    };
    config.features.question_tool_nudge = mode;
    config.thresholds.question_tool_nudge = QN_THRESHOLD;
    config
}

fn qn_service(
    directory: &QnTempDir,
    config: DecisionsConfig,
    engine: impl DecisionEngine + 'static,
) -> Decisions {
    Decisions::with_engine(
        config,
        &StateDir::from_path(directory.path().into()),
        engine,
    )
    .unwrap()
}

fn qn_enable(
    agent: &Agent<'_>,
    directory: &QnTempDir,
    mode: FeatureMode,
    probability: f64,
    fail: bool,
) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    agent.permissions.set_decisions(Some(qn_service(
        directory,
        qn_config(mode),
        FeatureEngine {
            probability,
            confidence: 1.0,
            fail,
            calls: Arc::clone(&calls),
        },
    )));
    calls
}

fn qn_install_native(agent: &Agent<'_>) {
    agent
        .registry
        .register_audited(
            Arc::new(QnQuestionTool),
            ToolSource::Native {
                owner: crate::tools::native::OWNER.into(),
                contract: QUESTION_TOOL_NAME.into(),
                trusted: true,
            },
            ToolEffect::Isolated,
        )
        .unwrap();
}

fn qn_transport(agent: &mut Agent<'_>) -> flume::Sender<String> {
    let (sender, receiver) = flume::unbounded();
    agent.user_response_rx = Some(Arc::new(async_lock::Mutex::new(receiver)));
    sender
}

fn qn_reminders(history: &History) -> usize {
    history
        .transcript_items()
        .iter()
        .filter(|item| {
            matches!(
                &item.kind,
                HistoryItemKind::User { text, origin: UserOrigin::Synthetic, .. }
                    if text == question_tool_nudge::REMINDER
            )
        })
        .count()
}

fn qn_question_input() -> Value {
    json!({"questions": [{
        "question": QN_REPLY,
        "header": "Interface",
        "options": [
            {"label": QN_CHOICE, "description": "A terminal interface"},
            {"label": "Browser", "description": "A browser interface"}
        ]
    }]})
}

#[test_case(FeatureMode::Off, QN_POSITIVE, false, false, 0; "off")]
#[test_case(FeatureMode::Shadow, QN_POSITIVE, false, false, 1; "shadow")]
#[test_case(FeatureMode::Advise, QN_POSITIVE, false, true, 1; "positive")]
#[test_case(FeatureMode::Advise, QN_THRESHOLD, false, true, 1; "inclusive_threshold")]
#[test_case(FeatureMode::Advise, QN_BELOW_THRESHOLD, false, false, 1; "below_threshold")]
#[test_case(FeatureMode::Advise, 0.0, false, false, 1; "negative")]
#[test_case(FeatureMode::Advise, QN_POSITIVE, true, false, 1; "timeout")]
#[test_case(FeatureMode::Advise, f64::NAN, false, false, 1; "not_a_number")]
#[test_case(FeatureMode::Advise, f64::INFINITY, false, false, 1; "infinite")]
#[test_case(FeatureMode::Advise, -0.1, false, false, 1; "negative_score")]
#[test_case(FeatureMode::Advise, 1.1, false, false, 1; "score_above_one")]
fn question_nudge_modes_and_effects(
    mode: FeatureMode,
    probability: f64,
    fail: bool,
    nudged: bool,
    expected_calls: usize,
) {
    smol::block_on(async {
        let directory = QnTempDir::new().unwrap();
        let mut history = History::new(vec![Message::user(QN_REQUEST.into())]);
        let (mut agent, events) = make_agent(MockProvider::new(Vec::new()), &mut history);
        qn_install_native(&agent);
        let _answers = qn_transport(&mut agent);
        let calls = qn_enable(&agent, &directory, mode, probability, fail);
        agent.response_text = Some(QN_REPLY.into());

        let outcome = agent.remind_question_tool().await.unwrap();
        assert_eq!(matches!(outcome, Some(TurnOutcome::Continue)), nudged);
        assert_eq!(agent.response_text(), (!nudged).then_some(QN_REPLY));
        assert_eq!(qn_reminders(agent.history), usize::from(nudged));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            expected_calls,
            "{QN_CALL_COUNT}"
        );
        assert_eq!(
            events
                .try_iter()
                .filter(|event| matches!(
                    &event.event,
                    AgentEvent::Injected { text, .. } if text == question_tool_nudge::REMINDER
                ))
                .count(),
            usize::from(nudged),
            "{QN_VISIBLE_REMINDER}"
        );
        assert_decision_effects(
            &directory,
            &vec![
                if nudged {
                    DecisionEffect::Advised
                } else {
                    DecisionEffect::None
                };
                expected_calls
            ],
            json!({question_tool_nudge::QUESTION: true}),
        )
        .await;
    });
}

enum QnGate {
    NoTool,
    NoTransport,
    DisconnectedTransport,
    Filter,
    Ceiling,
    Profile,
    Subagent,
    RootedChild,
    Cancelled,
    PendingInput,
    TerminalReport,
    NoEngine,
    NoEndpoint,
    EmptyReply,
}

#[test_case(QnGate::NoTool; "unregistered_question")]
#[test_case(QnGate::NoTransport; "noninteractive_frontend")]
#[test_case(QnGate::DisconnectedTransport; "disconnected_frontend")]
#[test_case(QnGate::Filter; "tool_filter")]
#[test_case(QnGate::Ceiling; "tool_ceiling")]
#[test_case(QnGate::Profile; "profile_policy")]
#[test_case(QnGate::Subagent; "subagent_audience")]
#[test_case(QnGate::RootedChild; "rooted_child")]
#[test_case(QnGate::Cancelled; "cancelled")]
#[test_case(QnGate::PendingInput; "pending_user_input")]
#[test_case(QnGate::TerminalReport; "terminal_report")]
#[test_case(QnGate::NoEngine; "no_engine")]
#[test_case(QnGate::NoEndpoint; "no_endpoint")]
#[test_case(QnGate::EmptyReply; "empty_reply")]
fn question_nudge_ineligible_handoffs_make_no_decision_call(gate: QnGate) {
    smol::block_on(async {
        let directory = QnTempDir::new().unwrap();
        let mut history = History::new(vec![Message::user(QN_REQUEST.into())]);
        let (mut agent, events) = make_agent(MockProvider::new(Vec::new()), &mut history);
        if !matches!(gate, QnGate::NoTool) {
            qn_install_native(&agent);
        }
        let mut answers = Some(qn_transport(&mut agent));
        let calls = qn_enable(&agent, &directory, FeatureMode::Advise, QN_POSITIVE, false);
        agent.response_text = Some(QN_REPLY.into());
        match gate {
            QnGate::NoTool => {}
            QnGate::NoTransport => agent.user_response_rx = None,
            QnGate::DisconnectedTransport => drop(answers.take()),
            QnGate::Filter => agent.tool_filter = ToolFilter::All.excluding(&[QUESTION_TOOL_NAME]),
            QnGate::Ceiling => {
                agent.tool_ceiling = ToolFilter::All.excluding(&[QUESTION_TOOL_NAME])
            }
            QnGate::Profile => {
                agent.profile_tool_policy =
                    Arc::new(serde_json::from_value(json!({"default": "disabled"})).unwrap());
            }
            QnGate::Subagent => agent.audience = ToolAudience::GENERAL_SUB,
            QnGate::RootedChild => agent.root_tool_use_id = Some(QUESTION_TOOL_NAME.into()),
            QnGate::Cancelled => {
                let (trigger, token) = CancelToken::new();
                trigger.cancel();
                agent.cancel = token;
            }
            QnGate::PendingInput => {
                agent.interrupt_source = Some(Arc::new(PendingInput(AtomicBool::new(true))))
            }
            QnGate::TerminalReport => agent.terminal_report = Some(Arc::new(AtomicBool::new(true))),
            QnGate::NoEngine => agent.permissions.set_decisions(None),
            QnGate::NoEndpoint => {
                let mut config = qn_config(FeatureMode::Advise);
                config.base_url = None;
                agent.permissions.set_decisions(Some(qn_service(
                    &directory,
                    config,
                    FeatureEngine {
                        probability: QN_POSITIVE,
                        confidence: 1.0,
                        fail: false,
                        calls: Arc::clone(&calls),
                    },
                )));
            }
            QnGate::EmptyReply => agent.response_text = Some(String::new()),
        }
        assert!(agent.remind_question_tool().await.unwrap().is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 0, "{QN_CALL_COUNT}");
        assert_eq!(qn_reminders(agent.history), 0, "{QN_NO_REMINDER}");
        assert!(
            events
                .try_iter()
                .all(|event| !matches!(event.event, AgentEvent::Injected { .. }))
        );
        assert_decision_effects(&directory, &[], Value::Null).await;
    });
}

#[test_case(false, false; "native")]
#[test_case(false, true; "native_yolo")]
#[test_case(true, false; "frontend_override")]
#[test_case(true, true; "frontend_override_yolo")]
fn question_nudge_reopens_the_turn_for_an_actual_question(local_override: bool, yolo: bool) {
    smol::block_on(async {
        let directory = QnTempDir::new().unwrap();
        let provider = MockProvider::new(vec![
            answer(QN_REPLY),
            tool_use_response(QUESTION_TOOL_NAME, qn_question_input()),
            answer(QN_FINAL),
        ]);
        let requests = Arc::clone(&provider.captured_messages);
        let mut history = History::default();
        let (mut agent, events) = make_agent(provider, &mut history);
        let mut answers = None;
        let override_calls = Arc::new(AtomicUsize::new(0));
        if local_override {
            let calls = Arc::clone(&override_calls);
            agent.local_tools = Arc::new(HashMap::from([(
                QUESTION_TOOL_NAME.into(),
                local_tool(move |input, _| {
                    let calls = Arc::clone(&calls);
                    Box::pin(async move {
                        assert_eq!(input, qn_question_input());
                        calls.fetch_add(1, Ordering::Relaxed);
                        Ok(QN_CHOICE.into())
                    })
                }),
            )]));
        } else {
            qn_install_native(&agent);
            let sender = qn_transport(&mut agent);
            sender.send(QN_ANSWER.into()).unwrap();
            answers = Some(sender);
        }
        agent
            .permissions
            .set_session_mode(Some(PermissionMode::from(yolo)));
        let calls = qn_enable(&agent, &directory, FeatureMode::Advise, QN_POSITIVE, false);
        assert!(agent.can_nudge_question());
        assert_eq!(
            agent
                .run(AgentInput {
                    message: QN_REQUEST.into(),
                    ..default_input()
                })
                .await
                .unwrap(),
            DoneReason::EndTurn
        );
        assert_eq!(agent.response_text(), Some(QN_FINAL));
        assert_eq!(calls.load(Ordering::Relaxed), 1, "{QN_CALL_COUNT}");
        assert_eq!(
            override_calls.load(Ordering::Relaxed),
            usize::from(local_override)
        );
        drop(agent);
        drop(answers);

        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 3);
            assert!(
                captured[1]
                    .iter()
                    .any(|message| message.first_text_content()
                        == Some(question_tool_nudge::REMINDER))
            );
            assert!(captured[2].iter().flat_map(|message| &message.content).any(|block| matches!(
                block, ContentBlock::ToolResult { content, is_error: false, .. } if content.contains(QN_CHOICE)
            )));
        }
        assert!(history.as_slice().iter().any(|message| {
            matches!(message.role, Role::Assistant)
                && message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text == QN_REPLY))
        }));
        assert_eq!(qn_reminders(&history), 1, "{QN_ONE_REMINDER}");
        let events = drain_events(&events);
        let injected = events.iter().position(|event| matches!(&event.event, AgentEvent::Injected { text, .. } if text == question_tool_nudge::REMINDER)).expect(QN_VISIBLE_REMINDER);
        let done = events
            .iter()
            .position(|event| matches!(event.event, AgentEvent::Done { .. }))
            .unwrap();
        assert!(injected < done);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, AgentEvent::Done { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, AgentEvent::Question(_)))
                .count(),
            usize::from(!local_override)
        );
        assert_decision_effects(
            &directory,
            &[DecisionEffect::Advised],
            json!({question_tool_nudge::QUESTION: true}),
        )
        .await;
    });
}

#[test_case(Some(1), Some(StopReason::EndTurn); "max_turns")]
#[test_case(None, Some(StopReason::MaxTokens); "truncated")]
#[test_case(None, None; "missing_end_turn")]
fn question_nudge_requires_an_unrestricted_end_turn(
    max_turns: Option<u32>,
    stop_reason: Option<StopReason>,
) {
    smol::block_on(async {
        let directory = QnTempDir::new().unwrap();
        let provider = MockProvider::new(vec![StreamResponse {
            stop_reason,
            ..answer(QN_REPLY)
        }]);
        let requests = Arc::clone(&provider.captured_messages);
        let mut history = History::default();
        let (mut agent, events) = make_agent(provider, &mut history);
        qn_install_native(&agent);
        let _answers = qn_transport(&mut agent);
        let calls = qn_enable(&agent, &directory, FeatureMode::Advise, QN_POSITIVE, false);
        agent.config.max_turns = max_turns;
        Arc::make_mut(&mut agent.config.steering).enabled = Some(false);
        assert!(agent.run(default_input()).await.is_ok());
        assert_eq!(agent.response_text(), Some(QN_REPLY));
        assert_eq!(calls.load(Ordering::Relaxed), 0, "{QN_CALL_COUNT}");
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(qn_reminders(agent.history), 0, "{QN_NO_REMINDER}");
        assert!(events.try_iter().all(|event| !matches!(&event.event, AgentEvent::Injected { text, .. } if text == question_tool_nudge::REMINDER)));
    });
}

#[test]
fn question_nudge_cannot_loop_on_repeated_prose() {
    smol::block_on(async {
        let directory = QnTempDir::new().unwrap();
        let mut history = History::default();
        let provider = MockProvider::new(vec![answer(QN_REPLY), answer(QN_REPEAT)]);
        let requests = Arc::clone(&provider.captured_messages);
        let (mut agent, _events) = make_agent(provider, &mut history);
        qn_install_native(&agent);
        let _answers = qn_transport(&mut agent);
        let calls = qn_enable(&agent, &directory, FeatureMode::Advise, QN_POSITIVE, false);
        assert_eq!(
            agent.run(default_input()).await.unwrap(),
            DoneReason::EndTurn
        );
        assert_eq!(agent.response_text(), Some(QN_REPEAT));
        assert_eq!(requests.lock().unwrap().len(), 2);
        assert_eq!(calls.load(Ordering::Relaxed), 1, "{QN_CALL_COUNT}");
        assert_eq!(qn_reminders(agent.history), 1, "{QN_ONE_REMINDER}");
    });
}

#[test]
fn question_nudge_does_not_charge_its_budget_for_a_legitimate_question_call() {
    smol::block_on(async {
        let directory = QnTempDir::new().unwrap();
        let provider = MockProvider::new(vec![tool_use_response(
            QUESTION_TOOL_NAME,
            qn_question_input(),
        )]);
        let mut history = History::new(vec![Message::user(QN_REQUEST.into())]);
        let (mut agent, events) = make_agent(provider, &mut history);
        qn_install_native(&agent);
        let answers = qn_transport(&mut agent);
        answers.send(QN_ANSWER.into()).unwrap();
        let calls = qn_enable(&agent, &directory, FeatureMode::Advise, QN_POSITIVE, false);

        assert!(matches!(agent.turn().await.unwrap(), TurnOutcome::Continue));
        assert_eq!(calls.load(Ordering::Relaxed), 0, "{QN_CALL_COUNT}");
        assert_eq!(qn_reminders(agent.history), 0, "{QN_NO_REMINDER}");
        assert_eq!(question_nudge_request(agent.history), Some(QN_REQUEST));
        assert_eq!(
            events
                .try_iter()
                .filter(|event| matches!(event.event, AgentEvent::Question(_)))
                .count(),
            1
        );

        agent.response_text = Some(QN_REPEAT.into());
        assert!(matches!(
            agent.remind_question_tool().await.unwrap(),
            Some(TurnOutcome::Continue)
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1, "{QN_CALL_COUNT}");
        assert_eq!(qn_reminders(agent.history), 1, "{QN_ONE_REMINDER}");
    });
}

#[test_case(true; "resume")]
#[test_case(false; "fresh_user_input")]
fn question_nudge_restored_run_only_refills_on_genuine_input(resume: bool) {
    smol::block_on(async {
        let directory = QnTempDir::new().unwrap();
        let history = History::new(vec![
            Message::user(QN_REQUEST.into()),
            answer(QN_REPLY).message,
            Message::synthetic(question_tool_nudge::REMINDER.into()),
        ]);
        let mut history = History::restored(history.transcript_items()).unwrap();
        let mut responses = vec![answer(QN_REPEAT)];
        if !resume {
            responses.push(answer(QN_FINAL));
        }
        let provider = MockProvider::new(responses);
        let requests = Arc::clone(&provider.captured_messages);
        let (mut agent, _events) = make_agent(provider, &mut history);
        qn_install_native(&agent);
        let _answers = qn_transport(&mut agent);
        let calls = qn_enable(&agent, &directory, FeatureMode::Advise, QN_POSITIVE, false);
        let input = AgentInput {
            message: if resume {
                String::new()
            } else {
                QN_NEXT_REQUEST.into()
            },
            resume,
            ..default_input()
        };
        assert_eq!(agent.run(input).await.unwrap(), DoneReason::EndTurn);
        assert_eq!(requests.lock().unwrap().len(), 1 + usize::from(!resume));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            usize::from(!resume),
            "{QN_CALL_COUNT}"
        );
        assert_eq!(
            qn_reminders(agent.history),
            1 + usize::from(!resume),
            "{QN_ONE_REMINDER}"
        );
    });
}

enum QnHistoryForm {
    Live,
    Restored,
    Compacted,
    Archived,
}

#[test_case(QnHistoryForm::Live; "live")]
#[test_case(QnHistoryForm::Restored; "restored")]
#[test_case(QnHistoryForm::Compacted; "compacted_user_copy")]
#[test_case(QnHistoryForm::Archived; "restored_archive")]
fn question_nudge_budget_survives_history_rebuilds(form: QnHistoryForm) {
    smol::block_on(async {
        let directory = QnTempDir::new().unwrap();
        let mut history = History::new(vec![
            Message::user(QN_REQUEST.into()),
            answer(QN_REPLY).message,
            Message::synthetic(question_tool_nudge::REMINDER.into()),
        ]);
        match form {
            QnHistoryForm::Live => {}
            QnHistoryForm::Restored => {
                history = History::restored(history.transcript_items()).unwrap()
            }
            QnHistoryForm::Compacted => {
                let head = history.item_at_message_boundary(1);
                let mut messages = vec![Message::synthetic(QN_SUMMARY.into())];
                messages.extend_from_slice(&history.as_slice()[1..]);
                history.replace_superseding(messages, head);
                assert!(
                    history
                        .active_items()
                        .iter()
                        .all(|item| item.stands_for.is_some())
                );
            }
            QnHistoryForm::Archived => {
                history = History::new(vec![Message::synthetic(QN_SUMMARY.into())])
                    .with_archived(history.transcript_items());
            }
        }
        assert_eq!(question_nudge_request(&history), None);
        let (mut agent, _events) = make_agent(MockProvider::new(Vec::new()), &mut history);
        qn_install_native(&agent);
        let _answers = qn_transport(&mut agent);
        let calls = qn_enable(&agent, &directory, FeatureMode::Advise, QN_POSITIVE, false);
        agent.response_text = Some(QN_REPEAT.into());
        assert!(agent.remind_question_tool().await.unwrap().is_none());
        agent
            .history
            .push(Message::synthetic(QN_NEXT_REQUEST.into()));
        assert!(agent.remind_question_tool().await.unwrap().is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 0, "{QN_CALL_COUNT}");
        agent.history.push(Message::user(QN_NEXT_REQUEST.into()));
        assert_eq!(question_nudge_request(agent.history), Some(QN_NEXT_REQUEST));
        assert!(matches!(
            agent.remind_question_tool().await.unwrap(),
            Some(TurnOutcome::Continue)
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1, "{QN_CALL_COUNT}");
        assert_eq!(qn_reminders(agent.history), 2);
    });
}

#[test_case(false; "ordinary_request")]
#[test_case(true; "quoted_reminder_is_a_user_turn")]
fn question_nudge_uses_genuine_user_provenance(quoted_reminder: bool) {
    let request = if quoted_reminder {
        question_tool_nudge::REMINDER
    } else {
        QN_REQUEST
    };
    let mut history = History::new(vec![
        Message::user(request.into()),
        Message::synthetic(QN_SUMMARY.into()),
        answer(QN_REPLY).message,
    ]);
    assert_eq!(question_nudge_request(&history), Some(request));
    history.push(Message::synthetic(format!(
        "{}\n{QN_SUMMARY}",
        question_tool_nudge::REMINDER
    )));
    assert_eq!(question_nudge_request(&history), Some(request));
    history.push(Message::synthetic(question_tool_nudge::REMINDER.into()));
    assert_eq!(question_nudge_request(&history), None);
}

#[test]
fn question_nudge_reads_user_display_text_across_compaction() {
    let mut history = History::new(vec![
        Message::user(QN_NEXT_REQUEST.into()),
        Message::synthetic(question_tool_nudge::REMINDER.into()),
        Message::user_display_with_images(QN_SUMMARY.into(), QN_REQUEST.into(), Vec::new()),
    ]);
    let head = history.item_at_message_boundary(2);
    let mut messages = vec![Message::synthetic(QN_SUMMARY.into())];
    messages.extend_from_slice(&history.as_slice()[2..]);
    history.replace_superseding(messages, head);
    assert!(history.active_items().last().unwrap().stands_for.is_some());
    assert_eq!(question_nudge_request(&history), Some(QN_REQUEST));
    assert_eq!(
        question_nudge_request(&History::new(vec![Message::synthetic(QN_REQUEST.into())])),
        None
    );
}

#[test]
fn question_nudge_preserved_tail_survives_repeated_compaction_and_restore() {
    let mut history = History::new(vec![
        Message::user(QN_REQUEST.into()),
        answer(QN_REPLY).message,
        Message::synthetic(question_tool_nudge::REMINDER.into()),
    ]);
    for _ in 0..2 {
        let head = history.item_at_message_boundary(1);
        let mut messages = vec![Message::synthetic(QN_SUMMARY.into())];
        messages.extend_from_slice(&history.as_slice()[1..]);
        history.replace_superseding(messages, head);
        assert_eq!(question_nudge_request(&history), None);
        let mut archive = history.transcript_items();
        archive.truncate(archive.len() - history.active_items().len());
        history = History::restored(history.active_items().to_vec())
            .unwrap()
            .with_archived(archive);
        assert_eq!(question_nudge_request(&history), None);
        assert_eq!(qn_reminders(&history), 1);
    }
}

struct QnControlledEngine {
    started: flume::Sender<()>,
    released: flume::Receiver<()>,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl DecisionEngine for QnControlledEngine {
    async fn decide(
        &self,
        request: &DecisionRequest,
        _deadline: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert!(
            request
                .questions
                .contains_key(question_tool_nudge::QUESTION)
        );
        assert_eq!(
            &request.state,
            question_tool_nudge::state(QN_REQUEST, QN_REPLY)
                .unwrap()
                .value()
        );
        self.started.send_async(()).await.unwrap();
        self.released.recv_async().await.unwrap();
        Ok(DecisionResponse {
            model: request.model.clone(),
            answers: [(
                question_tool_nudge::QUESTION.into(),
                Answer::Noul(NoulAnswer { noul: QN_POSITIVE }),
            )]
            .into(),
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            cache_hit: false,
        })
    }
}

#[derive(Clone)]
enum QnRace {
    PendingInput,
    Mailbox,
    PendingInputWithMailbox,
    QueuedInput,
    Cancel,
    Service,
    Project,
    TerminalReport,
    DisconnectedTransport,
    EnterYolo,
    YoloRoundTrip,
}

#[test_case(QnRace::PendingInput; "pending_input")]
#[test_case(QnRace::Mailbox; "mailbox_arrival")]
#[test_case(QnRace::PendingInputWithMailbox; "pending_input_with_mailbox")]
#[test_case(QnRace::QueuedInput; "queued_input")]
#[test_case(QnRace::Cancel; "cancel")]
#[test_case(QnRace::Service; "replace_service")]
#[test_case(QnRace::Project; "replace_project")]
#[test_case(QnRace::TerminalReport; "terminal_report")]
#[test_case(QnRace::DisconnectedTransport; "disconnect_transport")]
#[test_case(QnRace::EnterYolo; "enter_yolo")]
#[test_case(QnRace::YoloRoundTrip; "yolo_round_trip")]
fn question_nudge_revalidates_after_inference(race: QnRace) {
    smol::block_on(async {
        let directory = QnTempDir::new().unwrap();
        let project = QnTempDir::new().unwrap();
        let mut history = History::new(vec![Message::user(QN_REQUEST.into())]);
        let (mut agent, events) = make_agent(MockProvider::new(Vec::new()), &mut history);
        qn_install_native(&agent);
        let answers = qn_transport(&mut agent);
        let calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = flume::bounded(1);
        let (release_tx, release_rx) = flume::bounded(1);
        let session = QnSessionId::generate();
        agent.mailbox = Some(SessionMailbox::register(session));
        let decisions = qn_service(
            &directory,
            qn_config(FeatureMode::Advise),
            QnControlledEngine {
                started: started_tx,
                released: release_rx,
                calls: Arc::clone(&calls),
            },
        );
        agent.permissions.set_decisions(Some(decisions));
        agent.response_text = Some(QN_REPLY.into());
        let permissions = Arc::clone(&agent.permissions);
        let pending = Arc::new(PendingInput(AtomicBool::new(false)));
        let queued = MockInterruptSource::new(Vec::new());
        agent.interrupt_source = Some(if matches!(race, QnRace::QueuedInput) {
            queued.clone()
        } else {
            pending.clone()
        });
        let terminal = Arc::new(AtomicBool::new(false));
        agent.terminal_report = Some(Arc::clone(&terminal));
        let (cancel, token) = CancelToken::new();
        agent.cancel = token;
        let change = race.clone();
        let control = async move {
            let mut answers = Some(answers);
            let mut cancel = Some(cancel);
            started_rx.recv_async().await.unwrap();
            match change {
                QnRace::PendingInput => pending.0.store(true, Ordering::Release),
                QnRace::Mailbox => {
                    SessionMailbox::notify(session, QN_SUMMARY.into(), false).unwrap()
                }
                QnRace::PendingInputWithMailbox => {
                    SessionMailbox::notify(session, QN_SUMMARY.into(), false).unwrap();
                    pending.0.store(true, Ordering::Release);
                }
                QnRace::QueuedInput => {
                    queued
                        .commands
                        .lock()
                        .unwrap()
                        .push_back(ExtractedCommand::Interrupt(
                            Box::new(AgentInput {
                                message: QN_NEXT_REQUEST.into(),
                                ..default_input()
                            }),
                            0,
                            QueueItemId::new(),
                        ))
                }
                QnRace::Cancel => cancel.take().unwrap().cancel(),
                QnRace::Service => permissions.set_decisions(None),
                QnRace::Project => permissions.set_project(project.path()),
                QnRace::TerminalReport => terminal.store(true, Ordering::Release),
                QnRace::DisconnectedTransport => drop(answers.take()),
                QnRace::EnterYolo => permissions.set_session_mode(Some(PermissionMode::Yolo)),
                QnRace::YoloRoundTrip => {
                    permissions.set_session_mode(Some(PermissionMode::Yolo));
                    permissions.set_session_mode(Some(PermissionMode::Ask));
                }
            }
            let _ = release_tx.send(());
            (answers, cancel)
        };
        let (result, _guards) =
            futures_lite::future::zip(agent.remind_question_tool(), control).await;
        let nudged = matches!(race, QnRace::EnterYolo | QnRace::YoloRoundTrip);
        match race {
            QnRace::Cancel => assert!(matches!(result, Err(AgentError::Cancelled))),
            QnRace::TerminalReport => assert!(matches!(
                result.unwrap(),
                Some(TurnOutcome::Done(DoneReason::EndTurn))
            )),
            QnRace::QueuedInput => {
                assert!(matches!(result.unwrap(), Some(TurnOutcome::Continue)));
                assert_eq!(question_nudge_request(agent.history), Some(QN_NEXT_REQUEST));
            }
            QnRace::Mailbox => {
                assert!(matches!(result.unwrap(), Some(TurnOutcome::Continue)));
                assert_eq!(
                    agent.history.as_slice().last().unwrap().user_text(),
                    Some(QN_SUMMARY)
                );
            }
            _ if nudged => assert!(matches!(result.unwrap(), Some(TurnOutcome::Continue))),
            _ => assert!(result.unwrap().is_none()),
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1, "{QN_CALL_COUNT}");
        assert_eq!(
            qn_reminders(agent.history),
            usize::from(nudged),
            "{QN_NO_REMINDER}"
        );
        assert_eq!(events.try_iter().filter(|event| matches!(&event.event, AgentEvent::Injected { text, .. } if text == question_tool_nudge::REMINDER)).count(), usize::from(nudged));
        if nudged {
            assert_eq!(agent.response_text(), None);
            assert_decision_effects(
                &directory,
                &[DecisionEffect::Advised],
                json!({question_tool_nudge::QUESTION: true}),
            )
            .await;
        } else {
            let state = StateDir::from_path(directory.path().into());
            if let Some(log) = DecisionLog::open_existing(&state).unwrap() {
                let mut exported = Vec::new();
                log.export_jsonl(&mut exported, None).unwrap();
                for line in String::from_utf8(exported).unwrap().lines() {
                    let row: Value = serde_json::from_str(line).unwrap();
                    assert_eq!(
                        serde_json::from_value::<DecisionEffect>(row["caudra"]["effect"].clone())
                            .unwrap(),
                        DecisionEffect::None
                    );
                }
            }
        }
    });
}
