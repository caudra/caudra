mod autopilot_tests {
    use std::iter;
    use std::sync::atomic::AtomicBool;

    use caudra_config::AutopilotConfig;
    #[cfg(unix)]
    use caudra_config::{Feature, FeatureFlags};
    #[cfg(unix)]
    use caudra_providers::tag_safe_literal;
    use caudra_storage::autopilot::{AutopilotBudget, AutopilotStopper};
    use caudra_storage::id::CaudraId;
    use test_case::test_case;

    use super::*;
    use crate::agent::is_run_failure_marker;
    use crate::automation::clock::{Clock, FakeClock};
    use crate::autopilot::review::STREAK_REASON;
    use crate::autopilot::{ActionName, DecisionKind, MAX_CONTEXT_BYTES, MAX_REVIEW_STREAK};
    #[cfg(unix)]
    use crate::peers::held_work;
    #[cfg(unix)]
    use crate::tools::native::peers::WorkAssignment;
    use crate::tools::native::tests::tempdir;
    use crate::types::AutopilotEvent;

    const BIG_TURN_BYTES: usize = 8 * 1024;
    const BIG_TURNS: usize = 64;
    const PRICED_INPUT_TOKENS: u32 = 100_000;
    const PRICED_OUTPUT_TOKENS: u32 = 10_000;
    const MAX_COST: f64 = 1e-9;
    const LOWERED_LIMIT: u32 = 1;
    const PAYMENT_REQUIRED_STATUS: u16 = 402;
    const PAYMENT_REQUIRED_MESSAGE: &str = "Your credit balance is too low";
    const BAD_REQUEST_STATUS: u16 = 400;
    const BAD_REQUEST_MESSAGE: &str = "invalid request";
    const IMPLEMENT_TEXT: &str = "Implement the approved plan.";
    const STOPPED: AutopilotEnd = AutopilotEnd::Stopped {
        by: AutopilotStopper::Command,
    };
    const BOUNDED: &str = "a long history must publish only its newest tail";
    const KEEPS_NEWEST: &str = "the published tail must end on the newest message";
    const UNCHANGED_KEPT: &str = "an unchanged history must not be published again";
    const CHANGED_PUBLISHED: &str = "a changed history must be published again";
    const BEFORE_DISPATCH: &str = "a tool must run after the call that asked for it was published";
    const NOT_PUBLISHED: &str = "only an engaged main session publishes the conversation";
    const ANNOUNCED_ONCE: &str = "an engagement must be announced once";
    const ENDED_ONCE: &str = "an ended engagement must be withdrawn once";
    const NO_HANDLE: &str = "a session without autopilot announces none";
    const RESTATED: &str = "a compaction must restate the engagement it summarized away";
    const COMPACTION_FREE: &str = "the continuation after a compaction must not count";
    const COUNTED_ONCE: &str = "each continuation without a human must count once";
    const RESTATED_IN_RUN: &str = "a continuation inside a run must carry the engagement";
    const BUDGET_ENDS: &str = "a continuation past the budget must end autopilot";
    const CONTINUES_ANYWAY: &str = "the goal must continue whether autopilot ended or not";
    const SPEND_COUNTED: &str = "the turn and the evaluator must both be counted";
    const COST_ENDS: &str = "only API spend past the cost limit ends autopilot";
    const OWNED_ENDS: &str = "only a goal autopilot owns ends it when it finishes";
    const OWNER_AT_FINISH: &str =
        "a goal's end must say whether autopilot owned it before ending autopilot";
    const ERROR_ENDS: &str = "only an unrecoverable error ends autopilot";
    const CANCEL_KEEPS: &str = "a cancelled run must leave autopilot engaged";
    const RETRIES_NOW: &str = "retry now must cut autopilot's backoff short";
    const HUMAN_RESETS: &str = "only the user's own turn breaks the review streak";
    const ORIGIN_KEPT: &str = "an autopilot preamble must keep its origin in history";
    const REPLY_VERDICT: &str =
        r#"{"action":"reply","reason":"the user chose it","text":"Use the terminal."}"#;
    const AUTOPILOT_REPLY: &str = "Use the terminal.";
    const DONE_VERDICT: &str = r#"{"action":"done","reason":"nothing is left to answer"}"#;
    const ESCALATE_VERDICT: &str = r#"{"action":"escalate","reason":"only the user can choose"}"#;
    const IMPLEMENT_VERDICT: &str = r#"{"action":"implement","reason":"the plan covers it"}"#;
    const REVISE_VERDICT: &str =
        r#"{"action":"revise","reason":"tests are missing","feedback":"Add the tests."}"#;
    const REVISION: &str = "Add the tests.";
    const PLAN_REVISED: &str = "The plan now covers the tests.";
    /// The plan write, the reply after it, then the delegate.
    const PLAN_DELEGATE_CALL: usize = 2;
    const MAILED: &str = "The background build finished.";
    const PLAN_FIRST: &str = "the plan review must run before the nudge";
    const REVIEWED: &str = "a reply that asks must go to autopilot only when the nudge leaves it";
    const NOT_RENEWED: &str = "an autopilot reply must not renew the nudge's allowance";
    const ARRIVAL_WINS: &str =
        "what arrives during a review's delegate call must win over its reply";
    const HANDED_BACK: &str = "a reply verdict that does not reply must hand the turn back";
    const GOAL_AFTER_REVIEW: &str = "the goal must be checked only after a review that hands back";
    const REPLY_CONTINUES: &str = "an autopilot reply must continue the run with its text";
    const PLAN_SETTLED: &str = "a plan verdict must settle the turn as its action says";
    const CLAIM_DROPS: &str = "a plan the user claimed must drop the delegate's verdict";
    #[cfg(unix)]
    const WORK_REMINDED: &str = "held work must be reminded once per item while it can be reported";
    #[cfg(unix)]
    const WORK_AFTER_TODOS: &str = "the unreported-work reminder must follow the todo reminder";
    const CONTINUE_VERDICT: &str = r#"{"action":"continue","reason":"the work is close","guidance":"Run the failing test alone."}"#;
    const AGREE_VERDICT: &str = r#"{"action":"agree","reason":"the endpoint is gone"}"#;
    const GUIDANCE: &str = "Run the failing test alone.";
    const IMPOSSIBLE_REASON: &str = "the upload endpoint was removed";
    const LOOP_LIMIT: u32 = 1;
    const TURN_LIMIT: u32 = 1;
    /// The turn, the evaluator and the delegate, then the turn and the
    /// evaluator again when the review goes on.
    const TURN_LIMIT_CALLS: usize = 3;
    const REARMED_TURN_CALLS: usize = 2;
    /// Three evaluations and the two generations' second opinions.
    const ONCE_PER_GENERATION_CALLS: usize = 5;
    const STALL_REVIEWED: &str = "a continued stall must go on with autopilot's guidance and without its event, and an escalated one must stop as it does without autopilot";
    const STREAK_ESCALATES: &str = "the review after a full streak must be left to a human";
    const SECOND_OPINION: &str =
        "an impossible verdict must stand or go on as the second opinion says";
    const ONCE_PER_GOAL: &str = "a goal generation must get one second opinion";
    const START_MS: i64 = 1_790_000_000_000;
    const RETRY_LIMIT: u32 = 3;
    const BACKOFF: [Duration; RETRY_LIMIT as usize] = [
        Duration::from_secs(60),
        Duration::from_secs(2 * 60),
        Duration::from_secs(4 * 60),
    ];
    /// What one attempt asks before the transport's own retries give up.
    const ATTEMPT_REQUESTS: usize = EXPECTED_RETRY_BUDGET as usize + 1;
    const UNAVAILABLE_STATUS: u16 = 503;
    const MODEL_GONE_MESSAGE: &str = "The model does not exist";
    const BACKS_OFF: &str = "retries must wait out the doubling backoff on autopilot's clock";
    const FRESH_BUDGET: &str = "each retry must start a fresh transport budget";
    const NOT_REPLAYED: &str = "a retry must not run a tool that already ran";
    const RETRY_COUNTED: &str = "each retry must count as a continuation";
    const RETRIES_END: &str = "spent retries must fail the run and end autopilot";
    const GOAL_KEPT: &str = "only an unrecoverable error clears the goal";
    const FAILS_AT_ONCE: &str = "an error autopilot does not retry must fail the run at once";
    const AS_TODAY: &str =
        "without an engaged autopilot a retryable error must fail the run as before";

    enum Continuation {
        GoalNotMet,
        PrescreenSkip,
        QuestionNudge,
        OpenTodos,
        ReplyReview,
    }

    #[derive(Clone)]
    enum Arrival {
        PendingInput,
        TerminalReport,
        QueuedInput,
        Mailbox,
    }

    /// Answers as `inner` does, and runs `during` while the call numbered `at`
    /// is in flight.
    struct DuringCall {
        inner: MockProvider,
        at: usize,
        calls: AtomicUsize,
        during: Box<dyn Fn() + Send + Sync>,
    }

    impl DuringCall {
        fn new(inner: MockProvider, at: usize, during: impl Fn() + Send + Sync + 'static) -> Self {
            Self {
                inner,
                at,
                calls: AtomicUsize::new(0),
                during: Box::new(during),
            }
        }
    }

    impl Provider for DuringCall {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            messages: &'a [Message],
            system: &'a str,
            tools: &'a Value,
            events: &'a flume::Sender<ProviderEvent>,
            options: RequestOptions,
            cache_key: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == self.at {
                (self.during)();
            }
            self.inner
                .stream_message(model, messages, system, tools, events, options, cache_key)
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            self.inner.list_models()
        }
    }

    enum Ending {
        Unrecoverable,
        Recoverable,
        Cancelled,
    }

    fn engaged(owns_goal: bool) -> AutopilotHandle {
        let handle = AutopilotHandle::default();
        handle.engage(owns_goal, None);
        handle
    }

    fn continuations(handle: &AutopilotHandle) -> u32 {
        handle.stored().map_or(0, |stored| stored.continuations)
    }

    fn ends(events: &[Envelope]) -> Vec<AutopilotEnd> {
        events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                AgentEvent::Autopilot(AutopilotEvent::Ended { reason, .. }) => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }

    fn notices(history: &History, text: &str) -> usize {
        history
            .as_slice()
            .iter()
            .filter(|message| message.is_observation() && message.user_text() == Some(text))
            .count()
    }

    fn priced(response: StreamResponse) -> StreamResponse {
        StreamResponse {
            usage: TokenUsage {
                input: PRICED_INPUT_TOKENS,
                output: PRICED_OUTPUT_TOKENS,
                ..TokenUsage::default()
            },
            ..response
        }
    }

    fn big_history() -> History {
        let filler = "x".repeat(BIG_TURN_BYTES);
        History::new(
            (0..BIG_TURNS)
                .map(|index| Message::user(format!("{index} {filler}")))
                .collect(),
        )
    }

    #[test]
    fn a_long_history_publishes_a_bounded_snapshot() {
        smol::block_on(async {
            let handle = engaged(false);
            let mut history = big_history();
            let (agent, _events) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            let mut agent = agent.with_autopilot(handle.clone());

            agent.run(default_input()).await.unwrap();
            let items = agent.history.transcript().count();
            let newest = agent.history.as_slice().last().cloned().unwrap();
            drop(agent);

            let snapshot = handle.snapshot().expect(BOUNDED);
            let bytes: usize = snapshot
                .messages
                .iter()
                .filter_map(Message::first_text_content)
                .map(str::len)
                .sum();
            assert!(bytes <= MAX_CONTEXT_BYTES, "{BOUNDED}");
            assert!(snapshot.omitted > 0, "{BOUNDED}");
            assert_eq!(
                snapshot.omitted + snapshot.messages.len(),
                history.len(),
                "{BOUNDED}"
            );
            assert_eq!(
                snapshot
                    .messages
                    .last()
                    .and_then(Message::first_text_content),
                newest.first_text_content(),
                "{KEEPS_NEWEST}"
            );
            assert_eq!(snapshot.history_len, items);
            assert_eq!(snapshot.chat.model.spec(), default_model().spec());
        });
    }

    #[test]
    fn an_unchanged_history_is_not_published_again() {
        let handle = engaged(false);
        let mut history = History::new(vec![Message::user(GO.into())]);
        let (agent, _events) = make_agent(MockProvider::new(Vec::new()), &mut history);
        let mut agent = agent.with_autopilot(handle.clone());

        agent.observe_autopilot();
        let first = handle.snapshot().unwrap().messages;
        agent.observe_autopilot();
        let second = handle.snapshot().unwrap().messages;
        agent.history.push(Message::user(GO.into()));
        agent.observe_autopilot();
        let third = handle.snapshot().unwrap().messages;

        assert!(Arc::ptr_eq(&first, &second), "{UNCHANGED_KEPT}");
        assert!(!Arc::ptr_eq(&second, &third), "{CHANGED_PUBLISHED}");
    }

    #[test]
    fn the_snapshot_is_published_before_tool_dispatch() {
        smol::block_on(async {
            let handle = engaged(false);
            let mut history = History::new(Vec::new());
            let (mut agent, _events) = make_agent(
                MockProvider::new(vec![
                    tool_use_response(TEST_TOOL, json!({})),
                    text_response(StopReason::EndTurn),
                ]),
                &mut history,
            );
            agent.config.eager_tool_dispatch = false;
            agent.tools = json!([{"name": TEST_TOOL, "input_schema": {"type": "object"}}]);
            let seen = Arc::new(Mutex::new(Vec::new()));
            let (observer, seen_by_tool) = (handle.clone(), Arc::clone(&seen));
            agent.local_tools = Arc::new(HashMap::from([(
                TEST_TOOL.into(),
                audited_local_tool(ToolEffect::Isolated, move |_, _| {
                    seen_by_tool.lock().unwrap().push(
                        observer
                            .snapshot()
                            .and_then(|snapshot| snapshot.messages.last().cloned())
                            .is_some_and(|message| message.has_tool_calls()),
                    );
                    Box::pin(async { Ok(TEST_TOOL_RESULT.into()) })
                }),
            )]));
            let mut agent = agent.with_autopilot(handle);

            agent.run(default_input()).await.unwrap();

            assert_eq!(*seen.lock().unwrap(), [true], "{BEFORE_DISPATCH}");
        });
    }

    #[test_case(false, |_| {} ; "not_engaged")]
    #[test_case(true, |agent| agent.root_tool_use_id = Some(TEST_TOOL.into()) ; "a_subagent")]
    #[test_case(true, |agent| agent.audience = ToolAudience::GENERAL_SUB ; "not_the_main_audience")]
    fn nothing_is_published_outside_an_engaged_main_session(
        engage: bool,
        adjust: fn(&mut Agent<'_>),
    ) {
        smol::block_on(async {
            let handle = AutopilotHandle::default();
            if engage {
                handle.engage(false, None);
            }
            let mut history = History::new(Vec::new());
            let (mut agent, _events) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            adjust(&mut agent);
            let mut agent = agent.with_autopilot(handle.clone());

            agent.run(default_input()).await.unwrap();

            assert!(handle.snapshot().is_none(), "{NOT_PUBLISHED}");
        });
    }

    #[test]
    fn an_engagement_is_announced_once_and_withdrawn_once() {
        smol::block_on(async {
            let handle = engaged(false);
            let mut history = History::new(Vec::new());
            for stop in [false, true, false, false] {
                if stop {
                    handle.end(STOPPED);
                }
                let (agent, _events) = make_agent(
                    MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                    &mut history,
                );
                agent
                    .with_autopilot(handle.clone())
                    .run(default_input())
                    .await
                    .unwrap();
            }

            assert_eq!(
                notices(&history, crate::prompt::AUTOPILOT_ENGAGED_PROMPT),
                1,
                "{ANNOUNCED_ONCE}"
            );
            assert_eq!(
                notices(&history, crate::prompt::AUTOPILOT_ENDED_PROMPT),
                1,
                "{ENDED_ONCE}"
            );
            assert_eq!(
                last_announced(history.as_slice(), &[crate::prompt::AUTOPILOT_MARKER]),
                Some(crate::prompt::AUTOPILOT_ENDED_PROMPT),
                "{ENDED_ONCE}"
            );
        });
    }

    #[test]
    fn a_session_without_autopilot_announces_none() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, _events) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );

            agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(
                last_announced(history.as_slice(), &[crate::prompt::AUTOPILOT_MARKER]),
                None,
                "{NO_HANDLE}"
            );
        });
    }

    #[test]
    fn a_compaction_restates_the_engagement_and_never_counts() {
        smol::block_on(async {
            let handle = engaged(false);
            let mut history = History::new(vec![
                turn(),
                Message::observation(crate::prompt::AUTOPILOT_ENGAGED_PROMPT.into()),
                reply(),
            ]);
            let (mut agent, events) = make_agent(
                MockProvider::new(vec![
                    text_response(StopReason::EndTurn),
                    text_response(StopReason::EndTurn),
                ]),
                &mut history,
            );
            agent.auto_compact = true;
            agent.measured = Some(MeasuredContext {
                reported: u32::MAX,
                history_len: agent.history.len(),
                sent: None,
            });
            let mut agent = agent.with_autopilot(handle.clone());
            assert!(agent.try_auto_compact().await.unwrap());

            agent.run(default_input()).await.unwrap();
            drop(agent);

            let restated = drain_events(&events)
                .into_iter()
                .filter(|envelope| {
                    matches!(
                        &envelope.event,
                        AgentEvent::Injected { text, .. }
                            if text == crate::prompt::AUTOPILOT_ENGAGED_PROMPT
                    )
                })
                .count();
            assert_eq!(restated, 1, "{RESTATED}");
            assert_eq!(
                last_announced(history.as_slice(), &[crate::prompt::AUTOPILOT_MARKER]),
                Some(crate::prompt::AUTOPILOT_ENGAGED_PROMPT),
                "{RESTATED}"
            );
            assert_eq!(continuations(&handle), 0, "{COMPACTION_FREE}");
        });
    }

    #[test_case(Continuation::GoalNotMet ; "goal_not_met")]
    #[test_case(Continuation::PrescreenSkip ; "goal_prescreen_skip")]
    #[test_case(Continuation::QuestionNudge ; "question_tool_nudge")]
    #[test_case(Continuation::OpenTodos ; "open_todos")]
    #[test_case(Continuation::ReplyReview ; "reply_review")]
    fn each_continuation_without_a_human_counts_once(kind: Continuation) {
        smol::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let handle = engaged(false);
            let (mut history, responses) = match kind {
                Continuation::GoalNotMet => (
                    History::default(),
                    vec![goal_response(false, false, DECISION_VERDICT)],
                ),
                Continuation::QuestionNudge => (
                    History::new(vec![Message::user(QN_REQUEST.into())]),
                    Vec::new(),
                ),
                Continuation::OpenTodos => (
                    History::default().with_todos(Some(mixed_todos())),
                    Vec::new(),
                ),
                Continuation::PrescreenSkip => (History::default(), Vec::new()),
                Continuation::ReplyReview => (
                    History::new(vec![Message::user(QN_REQUEST.into())]),
                    vec![answer(REPLY_VERDICT)],
                ),
            };
            let (agent, _events) = make_agent(MockProvider::new(responses), &mut history);
            let mut agent = agent.with_autopilot(handle.clone());
            let continued = match kind {
                Continuation::GoalNotMet => {
                    agent.goal.set(DECISION_GOAL).unwrap();
                    matches!(
                        agent.goal_completion(DoneReason::EndTurn).await.unwrap(),
                        TurnOutcome::Continue
                    )
                }
                Continuation::PrescreenSkip => {
                    agent.goal.set(DECISION_GOAL).unwrap();
                    agent.permissions.set_decisions(Some(feature_decisions(
                        &directory,
                        FeatureMode::Enforce,
                        0.0,
                        1.0,
                        false,
                        Arc::new(AtomicUsize::new(0)),
                    )));
                    matches!(
                        agent.goal_completion(DoneReason::EndTurn).await.unwrap(),
                        TurnOutcome::Continue
                    )
                }
                Continuation::QuestionNudge => {
                    qn_install_native(&agent);
                    let _answers = qn_transport(&mut agent);
                    qn_enable(&agent, &directory, FeatureMode::Advise, QN_POSITIVE, false);
                    agent.response_text = Some(QN_REPLY.into());
                    matches!(
                        agent.remind_question_tool().await.unwrap(),
                        Some(TurnOutcome::Continue)
                    )
                }
                Continuation::OpenTodos => {
                    install_todo_tool(&agent);
                    agent.remind_open_todos()
                }
                Continuation::ReplyReview => {
                    agent.observe_autopilot();
                    agent.response_text = Some(QN_REPLY.into());
                    matches!(
                        agent.review_turn_end().await.unwrap(),
                        Some(TurnOutcome::Continue)
                    )
                }
            };
            drop(agent);

            assert!(continued, "{COUNTED_ONCE}");
            assert_eq!(continuations(&handle), 1, "{COUNTED_ONCE}");
            assert_eq!(
                notices(&history, crate::prompt::AUTOPILOT_ENGAGED_PROMPT),
                1,
                "{RESTATED_IN_RUN}"
            );
        });
    }

    #[test_case(LOWERED_LIMIT ; "exhausted")]
    #[test_case(LOWERED_LIMIT + 1 ; "lowered_below_use")]
    fn running_out_of_continuations_ends_autopilot_but_not_the_goal(used: u32) {
        smol::block_on(async {
            let handle = engaged(false);
            for _ in 0..used {
                handle.try_continue(&handle.limits()).unwrap();
            }
            handle.set_limits(AutopilotConfig {
                max_continuations: Some(LOWERED_LIMIT),
                ..AutopilotConfig::default()
            });
            let mut history = History::default();
            let (agent, events) = make_agent(
                MockProvider::new(vec![goal_response(false, false, DECISION_VERDICT)]),
                &mut history,
            );
            let mut agent = agent.with_autopilot(handle.clone());
            agent.goal.set(DECISION_GOAL).unwrap();

            let outcome = agent.goal_completion(DoneReason::EndTurn).await.unwrap();
            drop(agent);

            assert_eq!(
                ends(&drain_events(&events)),
                [AutopilotEnd::Budget {
                    kind: AutopilotBudget::Continuations
                }],
                "{BUDGET_ENDS}"
            );
            assert!(!handle.is_engaged(), "{BUDGET_ENDS}");
            assert!(
                matches!(outcome, TurnOutcome::Continue),
                "{CONTINUES_ANYWAY}"
            );
            assert!(
                history
                    .as_slice()
                    .last()
                    .and_then(Message::first_text_content)
                    .is_some_and(|text| text.contains(DECISION_VERDICT)),
                "{CONTINUES_ANYWAY}"
            );
        });
    }

    #[test]
    fn the_turn_and_the_evaluator_spend_reach_the_budget() {
        smol::block_on(async {
            let handle = engaged(false);
            let mut history = History::default();
            let (agent, _events) = make_agent(
                MockProvider::new(vec![
                    priced(text_response(StopReason::EndTurn)),
                    priced(goal_response(true, false, DECISION_VERDICT)),
                ]),
                &mut history,
            );
            let mut agent = agent.with_autopilot(handle.clone());
            agent.goal.set(DECISION_GOAL).unwrap();
            let goal = agent.goal.clone();

            agent.run(default_input()).await.unwrap();
            drop(agent);

            let Some(GoalStatus::Finished(result)) = goal.status() else {
                panic!("{SPEND_COUNTED}");
            };
            let spent = handle.stored().unwrap().cost_usd;
            assert!(spent > 0.0, "{SPEND_COUNTED}");
            assert_eq!(Some(spent), result.cost, "{SPEND_COUNTED}");
        });
    }

    #[test_case(Billing::Api, true ; "api_spend")]
    #[test_case(Billing::Subscription, false ; "subscription_spend")]
    fn a_cost_limit_ends_autopilot_on_api_spend(billing: Billing, ended: bool) {
        smol::block_on(async {
            let handle = engaged(false);
            handle.set_limits(AutopilotConfig {
                max_cost: Some(MAX_COST),
                ..AutopilotConfig::default()
            });
            let mut history = History::default();
            let (mut agent, events) = make_agent(
                MockProvider::new(vec![priced(text_response(StopReason::EndTurn))]),
                &mut history,
            );
            Arc::make_mut(&mut agent.model).billing = billing;
            let mut agent = agent.with_autopilot(handle.clone());

            agent.run(default_input()).await.unwrap();
            drop(agent);

            let expected = ended
                .then_some(AutopilotEnd::Budget {
                    kind: AutopilotBudget::Cost,
                })
                .into_iter()
                .collect::<Vec<_>>();
            assert_eq!(ends(&drain_events(&events)), expected, "{COST_ENDS}");
            assert_eq!(handle.is_engaged(), !ended, "{COST_ENDS}");
        });
    }

    #[test_case(true, true, false, Some(AutopilotEnd::GoalMet) ; "owned_goal_met")]
    #[test_case(true, false, true, Some(AutopilotEnd::GoalImpossible) ; "owned_goal_impossible")]
    #[test_case(false, true, false, None ; "unowned_goal_met")]
    #[test_case(false, false, true, None ; "unowned_goal_impossible")]
    fn a_finished_goal_ends_autopilot_only_when_owned(
        owned: bool,
        met: bool,
        impossible: bool,
        expected: Option<AutopilotEnd>,
    ) {
        smol::block_on(async {
            let handle = engaged(owned);
            let mut responses = vec![
                text_response(StopReason::EndTurn),
                goal_response(met, impossible, DECISION_VERDICT),
            ];
            responses.extend(impossible.then(|| answer(AGREE_VERDICT)));
            let mut history = History::default();
            let (agent, events) = make_agent(MockProvider::new(responses), &mut history);
            let mut agent = agent.with_autopilot(handle.clone());
            agent.goal.set(DECISION_GOAL).unwrap();

            agent.run(default_input()).await.unwrap();
            drop(agent);

            let events = drain_events(&events);
            assert_eq!(
                ends(&events),
                expected.iter().cloned().collect::<Vec<_>>(),
                "{OWNED_ENDS}"
            );
            assert_eq!(handle.is_engaged(), expected.is_none(), "{OWNED_ENDS}");
            assert!(!handle.owns_goal());
            assert_eq!(goal_owners(&events), [owned], "{OWNER_AT_FINISH}");
        });
    }

    /// Whether autopilot owned each goal that finished or was cleared, as its event says.
    fn goal_owners(events: &[Envelope]) -> Vec<bool> {
        events
            .iter()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::GoalFinished {
                    autopilot_owned, ..
                }
                | AgentEvent::GoalClearedAfterError {
                    autopilot_owned, ..
                } => Some(autopilot_owned),
                _ => None,
            })
            .collect()
    }

    #[test_case(true ; "owned")]
    #[test_case(false ; "not_owned")]
    fn a_goal_an_error_clears_says_whether_autopilot_owned_it(owned: bool) {
        smol::block_on(async {
            let handle = engaged(owned);
            let error = AgentError::api(PAYMENT_REQUIRED_STATUS, PAYMENT_REQUIRED_MESSAGE);
            let mut history = History::default();
            let (agent, events) =
                make_agent(MockProvider::with_results(vec![Err(error)]), &mut history);
            let mut agent = agent.with_autopilot(handle.clone());
            agent.goal.set(DECISION_GOAL).unwrap();

            let _ = agent.run(default_input()).await;
            drop(agent);

            assert!(!handle.is_engaged(), "{ERROR_ENDS}");
            assert_eq!(
                goal_owners(&drain_events(&events)),
                [owned],
                "{OWNER_AT_FINISH}"
            );
        });
    }

    #[test_case(Ending::Unrecoverable ; "unrecoverable_error")]
    #[test_case(Ending::Recoverable ; "recoverable_error")]
    #[test_case(Ending::Cancelled ; "cancelled")]
    fn only_an_unrecoverable_error_ends_autopilot(ending: Ending) {
        smol::block_on(async {
            let handle = engaged(false);
            let error = match ending {
                Ending::Unrecoverable => {
                    AgentError::api(PAYMENT_REQUIRED_STATUS, PAYMENT_REQUIRED_MESSAGE)
                }
                Ending::Recoverable | Ending::Cancelled => {
                    AgentError::api(BAD_REQUEST_STATUS, BAD_REQUEST_MESSAGE)
                }
            };
            let expected = matches!(ending, Ending::Unrecoverable)
                .then(|| AutopilotEnd::Error {
                    message: error.to_string(),
                })
                .into_iter()
                .collect::<Vec<_>>();
            let mut history = History::default();
            let (agent, events) =
                make_agent(MockProvider::with_results(vec![Err(error)]), &mut history);
            let (trigger, cancel) = CancelToken::new();
            if matches!(ending, Ending::Cancelled) {
                trigger.cancel();
            }
            let mut agent = agent.with_autopilot(handle.clone()).with_cancel(cancel);

            let result = agent.run(default_input()).await;
            drop(agent);

            assert_eq!(
                result.is_ok_and(|reason| reason == DoneReason::Cancelled),
                matches!(ending, Ending::Cancelled),
                "{CANCEL_KEEPS}"
            );
            assert_eq!(ends(&drain_events(&events)), expected, "{ERROR_ENDS}");
            assert_eq!(handle.is_engaged(), expected.is_empty(), "{ERROR_ENDS}");
        });
    }

    fn implement_preamble() -> Message {
        Message::autopilot_observation(
            IMPLEMENT_TEXT.into(),
            AutopilotOrigin {
                seq: AUTOPILOT_SEQ,
                kind: AutopilotMessageKind::Implement,
            },
        )
    }

    #[test_case(true ; "a_human_turn")]
    #[test_case(false ; "an_autopilot_preamble")]
    fn only_a_human_turn_breaks_the_review_streak(human: bool) {
        smol::block_on(async {
            let handle = engaged(false);
            for _ in 0..MAX_REVIEW_STREAK {
                assert!(handle.review_continue());
            }
            let input = if human {
                default_input()
            } else {
                AgentInput {
                    message: String::new(),
                    preamble: vec![implement_preamble()],
                    ..default_input()
                }
            };
            let mut history = History::default();
            let (agent, _events) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );

            agent
                .with_autopilot(handle.clone())
                .run(input)
                .await
                .unwrap();

            assert_eq!(handle.review_continue(), human, "{HUMAN_RESETS}");
        });
    }

    #[test]
    fn an_implement_preamble_keeps_its_autopilot_origin() {
        smol::block_on(async {
            let preamble = implement_preamble();
            let origin = preamble.autopilot_event.clone();
            let mut history = History::default();
            let (agent, _events) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );

            agent
                .with_autopilot(engaged(false))
                .run(AgentInput {
                    message: String::new(),
                    preamble: vec![preamble],
                    ..default_input()
                })
                .await
                .unwrap();

            assert!(
                history.active_items().iter().any(|item| matches!(
                    &item.kind,
                    HistoryItemKind::User { autopilot_event, origin: UserOrigin::Observation, .. }
                        if *autopilot_event == origin
                )),
                "{ORIGIN_KEPT}"
            );
            assert!(
                !history.active_items().iter().any(|item| matches!(
                    &item.kind,
                    HistoryItemKind::User {
                        origin: UserOrigin::Turn,
                        ..
                    }
                )),
                "{HUMAN_RESETS}"
            );
        });
    }

    fn asked(events: &[Envelope]) -> Vec<(u32, DecisionKind)> {
        events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                AgentEvent::Autopilot(AutopilotEvent::Deciding { seq, kind, .. }) => {
                    Some((*seq, *kind))
                }
                _ => None,
            })
            .collect()
    }

    fn asked_kinds(events: &[Envelope]) -> Vec<DecisionKind> {
        asked(events).into_iter().map(|(_, kind)| kind).collect()
    }

    fn implemented(events: &[Envelope]) -> Vec<u32> {
        events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                AgentEvent::Autopilot(AutopilotEvent::ImplementPlan { seq }) => Some(*seq),
                _ => None,
            })
            .collect()
    }

    /// What autopilot wrote into `history` as `kind`, oldest first.
    fn written(history: &History, kind: AutopilotMessageKind) -> Vec<String> {
        history
            .as_slice()
            .iter()
            .filter(|message| {
                message
                    .autopilot_event
                    .as_ref()
                    .is_some_and(|origin| origin.kind == kind)
            })
            .filter_map(Message::user_text)
            .map(str::to_owned)
            .collect()
    }

    fn autopilot_says(kind: AutopilotMessageKind, text: &str) -> String {
        review::on_behalf(AUTOPILOT_SEQ, kind, text)
            .user_text()
            .unwrap()
            .to_owned()
    }

    /// An engaged main agent about to hand back [`QN_REPLY`], with the
    /// conversation published for its delegate.
    fn handing_back<'h>(
        provider: impl Provider + 'static,
        history: &'h mut History,
        handle: &AutopilotHandle,
    ) -> (Agent<'h>, flume::Receiver<Envelope>) {
        let (agent, events) = make_agent(provider, history);
        let mut agent = agent.with_autopilot(handle.clone());
        agent.observe_autopilot();
        agent.response_text = Some(QN_REPLY.into());
        (agent, events)
    }

    /// The plan write, the reply asking to implement it, then `verdict`.
    fn plan_written_then(verdict: &str) -> Vec<StreamResponse> {
        vec![qn_plan_write(), answer(QN_PLAN_READY), answer(verdict)]
    }

    #[test]
    fn the_plan_review_runs_before_the_nudge() {
        smol::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let plans = tempdir();
            let root = plans.path();
            let mut history = History::default();
            let (mut agent, events) = qn_planning_agent(
                root,
                MockProvider::new(plan_written_then(ESCALATE_VERDICT)),
                &mut history,
            );
            qn_install_native(&agent);
            let _answers = qn_transport(&mut agent);
            let calls = qn_enable(&agent, &directory, FeatureMode::Advise, QN_POSITIVE, false);
            let mut agent = agent.with_autopilot(engaged(false));

            assert_eq!(
                agent.run(qn_planning_input(root)).await.unwrap(),
                DoneReason::EndTurn
            );
            assert_eq!(qn_reminders(agent.history), 0, "{PLAN_FIRST}");
            drop(agent);

            assert_eq!(calls.load(Ordering::Relaxed), 0, "{PLAN_FIRST}");
            assert_eq!(
                asked_kinds(&drain_events(&events)),
                [DecisionKind::Plan],
                "{PLAN_FIRST}"
            );
        });
    }

    #[test_case(IMPLEMENT_VERDICT, true, false ; "implement")]
    #[test_case(ESCALATE_VERDICT, false, false ; "escalate")]
    #[test_case(REVISE_VERDICT, false, true ; "revise")]
    fn a_plan_verdict_settles_the_turn(verdict: &str, implements: bool, revises: bool) {
        smol::block_on(async {
            let plans = tempdir();
            let root = plans.path();
            let handle = engaged(false);
            let mut responses = plan_written_then(verdict);
            if revises {
                responses.push(answer(PLAN_REVISED));
            }
            let provider = MockProvider::new(responses);
            let requests = Arc::clone(&provider.captured_messages);
            let mut history = History::default();
            let (agent, events) = qn_planning_agent(root, provider, &mut history);
            let mut agent = agent.with_autopilot(handle.clone());

            assert_eq!(
                agent.run(qn_planning_input(root)).await.unwrap(),
                DoneReason::EndTurn
            );
            assert!(agent.mode.is_planning(), "{PLAN_SETTLED}");
            drop(agent);

            let events = drain_events(&events);
            let [(seq, DecisionKind::Plan)] = asked(&events)[..] else {
                panic!("{PLAN_SETTLED}");
            };
            assert_eq!(
                implemented(&events),
                implements.then_some(seq).into_iter().collect::<Vec<_>>(),
                "{PLAN_SETTLED}"
            );
            assert_eq!(
                written(&history, AutopilotMessageKind::Revision),
                revises
                    .then(|| autopilot_says(AutopilotMessageKind::Revision, REVISION))
                    .into_iter()
                    .collect::<Vec<_>>(),
                "{PLAN_SETTLED}"
            );
            assert_eq!(continuations(&handle), u32::from(revises), "{COUNTED_ONCE}");
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                PLAN_DELEGATE_CALL + 1 + usize::from(revises),
                "{PLAN_SETTLED}"
            );
            assert!(
                requests[PLAN_DELEGATE_CALL]
                    .iter()
                    .filter_map(Message::first_text_content)
                    .any(|prompt| QN_PLAN_TEXT.lines().all(|line| prompt.contains(line))),
                "{PLAN_SETTLED}"
            );
        });
    }

    #[test]
    fn a_plan_the_user_claims_drops_the_verdict() {
        smol::block_on(async {
            let plans = tempdir();
            let root = plans.path();
            let handle = engaged(false);
            let inner = MockProvider::new(plan_written_then(REVISE_VERDICT));
            let requests = Arc::clone(&inner.captured_messages);
            let claimer = handle.clone();
            let provider = DuringCall::new(inner, PLAN_DELEGATE_CALL, move || {
                assert!(claimer.claim_plan());
            });
            let mut history = History::default();
            let (agent, _events) = qn_planning_agent(root, provider, &mut history);
            let mut agent = agent.with_autopilot(handle.clone());

            assert_eq!(
                agent.run(qn_planning_input(root)).await.unwrap(),
                DoneReason::EndTurn
            );
            drop(agent);

            assert_eq!(
                requests.lock().unwrap().len(),
                PLAN_DELEGATE_CALL + 1,
                "{CLAIM_DROPS}"
            );
            assert!(
                written(&history, AutopilotMessageKind::Revision).is_empty(),
                "{CLAIM_DROPS}"
            );
            assert!(handle.plan_claimed(), "{CLAIM_DROPS}");
            assert_eq!(continuations(&handle), 0, "{CLAIM_DROPS}");
        });
    }

    #[test_case(FeatureMode::Advise, QN_POSITIVE, true, false, 1, false ; "the_nudge_fires")]
    #[test_case(FeatureMode::Off, QN_POSITIVE, true, false, 0, true ; "nudge_off")]
    #[test_case(FeatureMode::Shadow, QN_POSITIVE, true, false, 1, true ; "nudge_in_shadow")]
    #[test_case(FeatureMode::Shadow, 0.0, true, false, 1, true ; "nudge_in_shadow_scoring_no")]
    #[test_case(FeatureMode::Advise, QN_BELOW_THRESHOLD, true, false, 1, true ; "nudge_under_its_threshold")]
    #[test_case(FeatureMode::Advise, QN_POSITIVE, true, true, 0, true ; "nudge_allowance_used")]
    #[test_case(FeatureMode::Advise, QN_POSITIVE, false, false, 0, true ; "no_question_tool")]
    fn a_reply_goes_to_autopilot_only_when_the_nudge_leaves_it(
        mode: FeatureMode,
        probability: f64,
        question_tool: bool,
        allowance_used: bool,
        nudge_calls: usize,
        reviewed: bool,
    ) {
        smol::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let handle = engaged(false);
            let mut turns = vec![Message::user(QN_REQUEST.into())];
            if allowance_used {
                turns.push(Message::synthetic(question_tool_nudge::REMINDER.into()));
            }
            let mut history = History::new(turns);
            let (mut agent, events) = handing_back(
                MockProvider::new(vec![answer(REPLY_VERDICT)]),
                &mut history,
                &handle,
            );
            if question_tool {
                qn_install_native(&agent);
            }
            let _answers = qn_transport(&mut agent);
            let calls = qn_enable(&agent, &directory, mode, probability, false);

            let outcome = agent.review_turn_end().await.unwrap();
            let reminders = qn_reminders(agent.history);
            assert_eq!(agent.response_text(), None);
            drop(agent);

            assert!(matches!(outcome, Some(TurnOutcome::Continue)));
            assert_eq!(
                calls.load(Ordering::Relaxed),
                nudge_calls,
                "{QN_CALL_COUNT}"
            );
            assert_eq!(reminders, usize::from(allowance_used || !reviewed));
            assert_eq!(
                asked_kinds(&drain_events(&events)),
                reviewed
                    .then_some(DecisionKind::Reply)
                    .into_iter()
                    .collect::<Vec<_>>(),
                "{REVIEWED}"
            );
            assert_eq!(
                written(&history, AutopilotMessageKind::Reply),
                reviewed
                    .then(|| autopilot_says(AutopilotMessageKind::Reply, AUTOPILOT_REPLY))
                    .into_iter()
                    .collect::<Vec<_>>(),
                "{REVIEWED}"
            );
            assert_eq!(continuations(&handle), 1, "{COUNTED_ONCE}");
        });
    }

    #[test]
    fn an_autopilot_reply_never_renews_the_nudge_allowance() {
        smol::block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let handle = engaged(false);
            let mut history = History::new(vec![
                Message::user(QN_REQUEST.into()),
                Message::synthetic(question_tool_nudge::REMINDER.into()),
            ]);
            let (mut agent, events) = handing_back(
                MockProvider::new(vec![answer(REPLY_VERDICT), answer(DONE_VERDICT)]),
                &mut history,
                &handle,
            );
            qn_install_native(&agent);
            let _answers = qn_transport(&mut agent);
            let calls = qn_enable(&agent, &directory, FeatureMode::Advise, QN_POSITIVE, false);

            assert!(matches!(
                agent.review_turn_end().await.unwrap(),
                Some(TurnOutcome::Continue)
            ));
            assert_eq!(question_nudge_request(agent.history), None, "{NOT_RENEWED}");
            agent.response_text = Some(QN_REPLY.into());
            assert!(agent.review_turn_end().await.unwrap().is_none());
            assert_eq!(qn_reminders(agent.history), 1, "{NOT_RENEWED}");
            drop(agent);

            assert_eq!(calls.load(Ordering::Relaxed), 0, "{NOT_RENEWED}");
            assert_eq!(
                asked_kinds(&drain_events(&events)),
                [DecisionKind::Reply, DecisionKind::Reply]
            );
        });
    }

    #[test_case(Arrival::PendingInput ; "pending_input")]
    #[test_case(Arrival::TerminalReport ; "terminal_report")]
    #[test_case(Arrival::QueuedInput ; "queued_input")]
    #[test_case(Arrival::Mailbox ; "background_arrival")]
    fn what_arrives_during_a_review_wins_over_its_reply(arrival: Arrival) {
        smol::block_on(async {
            let handle = engaged(false);
            let pending = Arc::new(PendingInput(AtomicBool::new(false)));
            let queued = MockInterruptSource::new(Vec::new());
            let terminal = Arc::new(AtomicBool::new(false));
            let session = CaudraId::generate();
            let during = {
                let (arrival, pending, queued, terminal) = (
                    arrival.clone(),
                    Arc::clone(&pending),
                    Arc::clone(&queued),
                    Arc::clone(&terminal),
                );
                move || match arrival {
                    Arrival::PendingInput => pending.0.store(true, Ordering::Release),
                    Arrival::TerminalReport => terminal.store(true, Ordering::Release),
                    Arrival::QueuedInput => {
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
                    Arrival::Mailbox => {
                        SessionMailbox::notify(session, MAILED.into(), false).unwrap()
                    }
                }
            };
            let provider =
                DuringCall::new(MockProvider::new(vec![answer(REPLY_VERDICT)]), 0, during);
            let mut history = History::new(vec![Message::user(QN_REQUEST.into())]);
            let (mut agent, _events) = handing_back(provider, &mut history, &handle);
            agent.interrupt_source = Some(if matches!(arrival, Arrival::QueuedInput) {
                queued.clone()
            } else {
                pending.clone()
            });
            agent.terminal_report = Some(Arc::clone(&terminal));
            agent.mailbox = Some(SessionMailbox::register(session));

            let outcome = agent.review_turn_end().await.unwrap();
            let request = question_nudge_request(agent.history).map(str::to_owned);
            let last = agent
                .history
                .as_slice()
                .last()
                .and_then(Message::user_text)
                .map(str::to_owned);
            drop(agent);

            let ends = matches!(arrival, Arrival::PendingInput | Arrival::TerminalReport);
            assert!(
                match outcome {
                    Some(TurnOutcome::Done(DoneReason::EndTurn)) => ends,
                    Some(TurnOutcome::Continue) => !ends,
                    _ => false,
                },
                "{ARRIVAL_WINS}"
            );
            match arrival {
                Arrival::QueuedInput => assert_eq!(request.as_deref(), Some(QN_NEXT_REQUEST)),
                Arrival::Mailbox => assert_eq!(last.as_deref(), Some(MAILED)),
                Arrival::PendingInput | Arrival::TerminalReport => {}
            }
            assert!(
                written(&history, AutopilotMessageKind::Reply).is_empty(),
                "{ARRIVAL_WINS}"
            );
            assert_eq!(continuations(&handle), 0, "{ARRIVAL_WINS}");
        });
    }

    #[test_case(DONE_VERDICT, false, false ; "done_without_a_goal")]
    #[test_case(DONE_VERDICT, true, true ; "done_with_a_goal")]
    #[test_case(ESCALATE_VERDICT, true, false ; "escalate_with_a_goal")]
    fn a_reply_verdict_without_a_reply_hands_the_turn_back(
        verdict: &str,
        goal: bool,
        goal_checked: bool,
    ) {
        smol::block_on(async {
            let mut responses = vec![answer(QN_REPLY), answer(verdict)];
            if goal_checked {
                responses.push(goal_response(true, false, DECISION_VERDICT));
            }
            let provider = MockProvider::new(responses);
            let requests = Arc::clone(&provider.captured_messages);
            let mut history = History::default();
            let (agent, events) = make_agent(provider, &mut history);
            let mut agent = agent.with_autopilot(engaged(false));
            if goal {
                agent.goal.set(DECISION_GOAL).unwrap();
            }

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );
            assert_eq!(agent.response_text(), Some(QN_REPLY), "{HANDED_BACK}");
            drop(agent);

            assert_eq!(
                requests.lock().unwrap().len(),
                2 + usize::from(goal_checked),
                "{HANDED_BACK}"
            );
            assert!(
                written(&history, AutopilotMessageKind::Reply).is_empty(),
                "{HANDED_BACK}"
            );
            let events = drain_events(&events);
            let asked_at = events.iter().position(|envelope| {
                matches!(
                    &envelope.event,
                    AgentEvent::Autopilot(AutopilotEvent::Deciding {
                        kind: DecisionKind::Reply,
                        ..
                    })
                )
            });
            let checked_at = events
                .iter()
                .position(|envelope| matches!(envelope.event, AgentEvent::GoalEvaluating { .. }));
            assert!(asked_at.is_some(), "{HANDED_BACK}");
            assert_eq!(checked_at.is_some(), goal_checked, "{GOAL_AFTER_REVIEW}");
            assert!(
                checked_at.is_none_or(|checked_at| asked_at < Some(checked_at)),
                "{GOAL_AFTER_REVIEW}"
            );
        });
    }

    #[test]
    fn an_autopilot_reply_continues_the_run() {
        smol::block_on(async {
            let handle = engaged(false);
            let provider = MockProvider::new(vec![
                answer(QN_REPLY),
                answer(REPLY_VERDICT),
                answer(QN_FINAL),
            ]);
            let requests = Arc::clone(&provider.captured_messages);
            let mut history = History::default();
            let (agent, _events) = make_agent(provider, &mut history);
            let mut agent = agent.with_autopilot(handle.clone());

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );
            assert_eq!(agent.response_text(), Some(QN_FINAL), "{REPLY_CONTINUES}");
            drop(agent);

            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3, "{REPLY_CONTINUES}");
            assert_eq!(
                requests[2].last().and_then(Message::user_text),
                Some(autopilot_says(AutopilotMessageKind::Reply, AUTOPILOT_REPLY).as_str()),
                "{REPLY_CONTINUES}"
            );
            assert_eq!(continuations(&handle), 1, "{COUNTED_ONCE}");
        });
    }

    /// A main agent of `worker`'s session, with messaging on and the tool that
    /// reports its work.
    #[cfg(unix)]
    fn working_agent<'h>(
        provider: impl Provider + 'static,
        history: &'h mut History,
        worker: &PeerSession,
    ) -> (Agent<'h>, flume::Receiver<Envelope>) {
        let (agent, events) = make_agent_configured(provider, history, |params| {
            params.session_id = Some(worker.session_id().into());
            params.config.features = FeatureFlags::NONE.with(Feature::CrossSessionMessaging);
            params.config.generate_titles = false;
        });
        agent
            .registry
            .register_audited(
                Arc::new(WorkAssignment),
                ToolSource::Native {
                    owner: crate::tools::native::OWNER.into(),
                    contract: WORK_NAME.into(),
                    trusted: true,
                },
                ToolEffect::Mutating,
            )
            .unwrap();
        (agent, events)
    }

    #[cfg(unix)]
    #[test_case(|_| {}, true ; "reportable")]
    #[test_case(|agent| agent.mode = AgentMode::ReadOnly, false ; "read_only")]
    #[test_case(|agent| agent.tool_filter = ToolFilter::All.excluding(&[WORK_NAME]), false ; "filtered_out")]
    #[test_case(|agent| agent.config.features = FeatureFlags::NONE, false ; "messaging_off")]
    #[test_case(|agent| agent.autopilot = None, false ; "no_autopilot")]
    fn held_work_is_reminded_once_while_it_can_be_reported(
        adjust: fn(&mut Agent<'_>),
        reminds: bool,
    ) {
        smol::block_on(async {
            let held = held_work().await;
            let handle = engaged(false);
            let mut history = History::default();
            let (agent, _events) =
                working_agent(MockProvider::new(Vec::new()), &mut history, &held.worker);
            let mut agent = agent.with_autopilot(handle.clone());
            adjust(&mut agent);

            let first = agent.remind_unreported_work();
            let again = agent.remind_unreported_work();
            drop(agent);

            assert_eq!((first, again), (reminds, false), "{WORK_REMINDED}");
            let reminders = written(&history, AutopilotMessageKind::Reminder);
            assert_eq!(reminders.len(), usize::from(reminds), "{WORK_REMINDED}");
            assert!(
                reminders
                    .iter()
                    .all(|text| text.contains(&tag_safe_literal(&held.work))),
                "{WORK_REMINDED}"
            );
            assert_eq!(continuations(&handle), u32::from(reminds), "{COUNTED_ONCE}");
        });
    }

    #[cfg(unix)]
    #[test]
    fn held_work_is_reminded_after_the_open_todos() {
        smol::block_on(async {
            let held = held_work().await;
            let handle = engaged(false);
            let mut history = History::default().with_todos(Some(mixed_todos()));
            let provider = MockProvider::new(vec![
                answer(FIRST_ANSWER),
                answer(SECOND_ANSWER),
                answer(QN_FINAL),
            ]);
            let requests = Arc::clone(&provider.captured_messages);
            let (agent, _events) = working_agent(provider, &mut history, &held.worker);
            install_todo_tool(&agent);
            let mut agent = agent.with_autopilot(handle.clone());

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );
            drop(agent);

            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3, "{WORK_REMINDED}");
            assert_eq!(
                requests[1]
                    .last()
                    .and_then(|message| message.standing_reminder.clone()),
                Some(StandingReminderKind::OpenTodos),
                "{WORK_AFTER_TODOS}"
            );
            assert_eq!(
                requests[2]
                    .last()
                    .and_then(|message| message.autopilot_event.as_ref())
                    .map(|origin| origin.kind),
                Some(AutopilotMessageKind::Reminder),
                "{WORK_AFTER_TODOS}"
            );
            assert_eq!(continuations(&handle), 2, "{COUNTED_ONCE}");
        });
    }

    /// What autopilot's guidance writes into the history when `continues`.
    fn guidance_if(continues: bool) -> Vec<String> {
        continues
            .then(|| autopilot_says(AutopilotMessageKind::Guidance, GUIDANCE))
            .into_iter()
            .collect()
    }

    fn count(events: &[Envelope], event: fn(&AgentEvent) -> bool) -> usize {
        events
            .iter()
            .filter(|envelope| event(&envelope.event))
            .count()
    }

    #[test_case(Some(CONTINUE_VERDICT), true ; "goes_on")]
    #[test_case(Some(ESCALATE_VERDICT), false ; "escalate")]
    #[test_case(None, false ; "not_engaged")]
    fn a_loop_cap_review_goes_on_or_stops_the_goal_loop(verdict: Option<&str>, continues: bool) {
        smol::block_on(async {
            let handle = AutopilotHandle::default();
            if verdict.is_some() {
                handle.engage(false, None);
            }
            let mut responses = vec![goal_response(false, false, DECISION_VERDICT)];
            responses.extend(verdict.map(answer));
            let mut history = History::default();
            let (agent, events) = make_agent(MockProvider::new(responses), &mut history);
            let mut agent = agent.with_autopilot(handle.clone());
            agent.goal.set(DECISION_GOAL).unwrap();
            agent.goal.set_continuation_limit(LOOP_LIMIT);
            agent.goal_blocks = LOOP_LIMIT;
            let goal = agent.goal.clone();

            let outcome = agent.goal_completion(DoneReason::EndTurn).await.unwrap();
            let blocks = agent.goal_blocks;
            drop(agent);

            let events = drain_events(&events);
            assert_eq!(
                asked_kinds(&events),
                verdict
                    .map(|_| DecisionKind::Review)
                    .into_iter()
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                matches!(outcome, TurnOutcome::Continue),
                continues,
                "{STALL_REVIEWED}"
            );
            assert_eq!(
                count(&events, |event| matches!(
                    event,
                    AgentEvent::GoalLoopCap { .. }
                )),
                usize::from(!continues),
                "{STALL_REVIEWED}"
            );
            assert_eq!(
                blocks,
                if continues { 0 } else { LOOP_LIMIT },
                "{STALL_REVIEWED}"
            );
            assert_eq!(
                written(&history, AutopilotMessageKind::Guidance),
                guidance_if(continues),
                "{STALL_REVIEWED}"
            );
            assert!(goal.snapshot().is_some(), "{STALL_REVIEWED}");
            assert_eq!(
                continuations(&handle),
                u32::from(continues),
                "{COUNTED_ONCE}"
            );
        });
    }

    /// A continue buys the stalled goal one turn, never a fresh count, so the
    /// next turn without progress is put to the delegate again.
    #[test_case(Some(CONTINUE_VERDICT), true ; "goes_on")]
    #[test_case(Some(ESCALATE_VERDICT), false ; "escalate")]
    #[test_case(None, false ; "not_engaged")]
    fn a_stalled_goal_review_goes_on_for_one_turn_or_stops_the_run(
        verdict: Option<&str>,
        continues: bool,
    ) {
        smol::block_on(async {
            let handle = AutopilotHandle::default();
            if verdict.is_some() {
                handle.engage(false, None);
            }
            let mut responses = vec![assistant_response(vec![ContentBlock::Text {
                text: json!({
                    "ok": false,
                    "reason": DECISION_VERDICT,
                    "impossible": false,
                    "progress": false,
                })
                .to_string(),
            }])];
            responses.extend(verdict.map(answer));
            let mut history = History::default();
            let (agent, events) = make_agent(MockProvider::new(responses), &mut history);
            let mut agent = agent.with_autopilot(handle.clone());
            let generation = agent.goal.set(DECISION_GOAL).unwrap().generation;
            let stuck = GoalTurn {
                judgement: Some(Judgement {
                    progressed: false,
                    waiting: false,
                }),
                ..GoalTurn::default()
            };
            for _ in 1..MAX_STALLED_TURNS {
                agent.goal.record_turn(generation, &stuck);
            }
            let goal = agent.goal.clone();

            let outcome = agent.goal_completion(DoneReason::EndTurn).await.unwrap();
            drop(agent);

            let events = drain_events(&events);
            assert_eq!(
                asked_kinds(&events),
                verdict
                    .map(|_| DecisionKind::Review)
                    .into_iter()
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                matches!(outcome, TurnOutcome::Continue),
                continues,
                "{STALL_REVIEWED}"
            );
            assert_eq!(
                count(&events, |event| matches!(
                    event,
                    AgentEvent::GoalStalled { .. }
                )),
                usize::from(!continues),
                "{STALL_REVIEWED}"
            );
            assert_eq!(
                written(&history, AutopilotMessageKind::Guidance),
                guidance_if(continues),
                "{STALL_REVIEWED}"
            );
            assert!(goal.snapshot().is_some(), "{STALL_REVIEWED}");
            if continues {
                assert_eq!(
                    goal.record_turn(generation, &stuck),
                    GoalPace::Stalled {
                        turns: MAX_STALLED_TURNS
                    },
                    "{STALL_REVIEWED}"
                );
            }
        });
    }

    #[test_case(CONTINUE_VERDICT, true ; "goes_on")]
    #[test_case(ESCALATE_VERDICT, false ; "escalate")]
    fn a_turn_limit_review_rearms_the_turns_or_stops_the_run(verdict: &str, continues: bool) {
        smol::block_on(async {
            let mut responses = vec![
                text_response(StopReason::EndTurn),
                goal_response(false, false, DECISION_VERDICT),
                answer(verdict),
            ];
            if continues {
                responses.extend([
                    text_response(StopReason::EndTurn),
                    goal_response(true, false, DECISION_VERDICT),
                ]);
            }
            let provider = MockProvider::new(responses);
            let requests = Arc::clone(&provider.captured_messages);
            let mut history = History::default();
            let (mut agent, events) = make_agent(provider, &mut history);
            agent.config.max_turns = Some(TURN_LIMIT);
            let mut agent = agent.with_autopilot(engaged(false));
            agent.goal.set(DECISION_GOAL).unwrap();

            let reason = agent.run(default_input()).await.unwrap();
            drop(agent);

            let events = drain_events(&events);
            assert_eq!(asked_kinds(&events), [DecisionKind::Review]);
            assert_eq!(
                reason,
                if continues {
                    DoneReason::EndTurn
                } else {
                    DoneReason::MaxTurns
                },
                "{STALL_REVIEWED}"
            );
            assert_eq!(
                count(&events, |event| matches!(
                    event,
                    AgentEvent::GoalTurnLimit { .. }
                )),
                usize::from(!continues),
                "{STALL_REVIEWED}"
            );
            assert_eq!(
                requests.lock().unwrap().len(),
                TURN_LIMIT_CALLS + usize::from(continues) * REARMED_TURN_CALLS,
                "{STALL_REVIEWED}"
            );
            assert_eq!(
                written(&history, AutopilotMessageKind::Guidance),
                guidance_if(continues),
                "{STALL_REVIEWED}"
            );
        });
    }

    #[test_case(CONTINUE_VERDICT, true ; "goes_on")]
    #[test_case(ESCALATE_VERDICT, false ; "escalate")]
    fn an_exhausted_steering_review_rearms_the_rule_or_fails_the_run(
        verdict: &str,
        continues: bool,
    ) {
        smol::block_on(async {
            let mut responses: Vec<_> =
                (0..=MAX_IDLE_NUDGES).map(|_| thinking_response()).collect();
            responses.push(answer(verdict));
            if continues {
                responses.extend([thinking_response(), text_response(StopReason::EndTurn)]);
            }
            let mut history = History::default();
            let (agent, events) = make_agent(MockProvider::new(responses), &mut history);
            let mut agent = agent.with_autopilot(engaged(false));

            let result = agent.run(default_input()).await;
            drop(agent);

            assert!(
                match &result {
                    Ok(reason) => continues && *reason == DoneReason::EndTurn,
                    Err(AgentError::SteeringExhausted { rule }) => {
                        !continues && rule == STEERING_EMPTY
                    }
                    Err(_) => false,
                },
                "{STALL_REVIEWED}"
            );
            let events = drain_events(&events);
            assert_eq!(asked_kinds(&events), [DecisionKind::Review]);
            assert_eq!(
                count(&events, |event| matches!(event, AgentEvent::Nudge { .. })),
                usize::try_from(MAX_IDLE_NUDGES).unwrap() + usize::from(continues),
                "{STALL_REVIEWED}"
            );
            assert_eq!(
                written(&history, AutopilotMessageKind::Guidance),
                guidance_if(continues),
                "{STALL_REVIEWED}"
            );
        });
    }

    #[test_case(false ; "without_a_human")]
    #[test_case(true ; "after_a_human_turn")]
    fn a_full_review_streak_leaves_the_next_review_to_a_human(human: bool) {
        smol::block_on(async {
            let streak = usize::try_from(MAX_REVIEW_STREAK).unwrap();
            let mut responses: Vec<_> = (0..streak).map(|_| answer(CONTINUE_VERDICT)).collect();
            if human {
                responses.push(text_response(StopReason::EndTurn));
            }
            responses.push(answer(CONTINUE_VERDICT));
            let mut history = History::new(vec![Message::user(GO.into())]);
            let (agent, events) = make_agent(MockProvider::new(responses), &mut history);
            let mut agent = agent.with_autopilot(engaged(false));
            let stall = || Stall::Steering {
                rule: STEERING_EMPTY,
            };

            for _ in 0..streak {
                assert!(
                    agent.review_stall(stall()).await.unwrap().is_some(),
                    "{STALL_REVIEWED}"
                );
            }
            if human {
                agent.run(default_input()).await.unwrap();
            }
            let last = agent.review_stall(stall()).await.unwrap();
            drop(agent);

            assert_eq!(last.is_some(), human, "{STREAK_ESCALATES}");
            let decided = drain_events(&events)
                .into_iter()
                .filter_map(|envelope| match envelope.event {
                    AgentEvent::Autopilot(AutopilotEvent::Decided(decision)) => Some(decision),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(decided.len(), streak + 1, "{STREAK_ESCALATES}");
            let last = decided.last().unwrap();
            assert_eq!(
                (last.action, last.reason == STREAK_REASON),
                if human {
                    (ActionName::Continue, false)
                } else {
                    (ActionName::Escalate, true)
                },
                "{STREAK_ESCALATES}"
            );
        });
    }

    #[test_case(AGREE_VERDICT, true, Some(AutopilotEnd::GoalImpossible) ; "agree_on_an_owned_goal")]
    #[test_case(AGREE_VERDICT, false, None ; "agree_on_an_unowned_goal")]
    #[test_case(CONTINUE_VERDICT, true, None ; "goes_on")]
    fn a_second_opinion_lets_an_impossible_verdict_stand_or_goes_on(
        verdict: &str,
        owned: bool,
        ended: Option<AutopilotEnd>,
    ) {
        smol::block_on(async {
            let continues = verdict == CONTINUE_VERDICT;
            let handle = engaged(owned);
            let mut history = History::default();
            let (agent, events) = make_agent(
                MockProvider::new(vec![
                    goal_response(false, true, IMPOSSIBLE_REASON),
                    answer(verdict),
                ]),
                &mut history,
            );
            let mut agent = agent.with_autopilot(handle.clone());
            agent.goal.set(DECISION_GOAL).unwrap();
            let goal = agent.goal.clone();

            let outcome = agent.goal_completion(DoneReason::EndTurn).await.unwrap();
            drop(agent);

            let events = drain_events(&events);
            assert_eq!(asked_kinds(&events), [DecisionKind::SecondOpinion]);
            assert_eq!(
                matches!(outcome, TurnOutcome::Continue),
                continues,
                "{SECOND_OPINION}"
            );
            let applied = match goal.status() {
                Some(GoalStatus::Active(goal)) => goal.last_verdict,
                Some(GoalStatus::Finished(result)) => Some(result.verdict),
                None => None,
            };
            assert_eq!(
                applied,
                Some(if continues {
                    GoalVerdict::NotMet
                } else {
                    GoalVerdict::Impossible
                }),
                "{SECOND_OPINION}"
            );
            let logged = if continues {
                GoalEvaluationOutcome::Reconsidered
            } else {
                GoalEvaluationOutcome::Applied
            };
            assert_eq!(logged_outcomes(&goal), [(1, logged)], "{SECOND_OPINION}");
            assert_eq!(
                ends(&events),
                ended.iter().cloned().collect::<Vec<_>>(),
                "{OWNED_ENDS}"
            );
            assert_eq!(handle.is_engaged(), ended.is_none(), "{OWNED_ENDS}");
            assert_eq!(
                written(&history, AutopilotMessageKind::Guidance),
                guidance_if(continues),
                "{SECOND_OPINION}"
            );
            assert_eq!(
                continuations(&handle),
                u32::from(continues),
                "{COUNTED_ONCE}"
            );
        });
    }

    #[test]
    fn a_second_opinion_is_asked_once_per_goal_generation() {
        smol::block_on(async {
            let impossible = || goal_response(false, true, IMPOSSIBLE_REASON);
            let provider = MockProvider::new(vec![
                impossible(),
                answer(CONTINUE_VERDICT),
                impossible(),
                impossible(),
                answer(AGREE_VERDICT),
            ]);
            let requests = Arc::clone(&provider.captured_messages);
            let mut history = History::default();
            let (agent, events) = make_agent(provider, &mut history);
            let mut agent = agent.with_autopilot(engaged(false));
            let goal = agent.goal.clone();

            goal.set(DECISION_GOAL).unwrap();
            let asked = agent.goal_completion(DoneReason::EndTurn).await.unwrap();
            let again = agent.goal_completion(DoneReason::EndTurn).await.unwrap();
            let finished = matches!(goal.status(), Some(GoalStatus::Finished(_)));
            goal.set(DECISION_GOAL).unwrap();
            let next = agent.goal_completion(DoneReason::EndTurn).await.unwrap();
            drop(agent);

            assert!(matches!(asked, TurnOutcome::Continue), "{ONCE_PER_GOAL}");
            assert!(
                finished && matches!(again, TurnOutcome::Done(DoneReason::EndTurn)),
                "{ONCE_PER_GOAL}"
            );
            assert!(
                matches!(next, TurnOutcome::Done(DoneReason::EndTurn)),
                "{ONCE_PER_GOAL}"
            );
            assert_eq!(
                asked_kinds(&drain_events(&events)),
                [DecisionKind::SecondOpinion, DecisionKind::SecondOpinion],
                "{ONCE_PER_GOAL}"
            );
            assert_eq!(
                requests.lock().unwrap().len(),
                ONCE_PER_GENERATION_CALLS,
                "{ONCE_PER_GOAL}"
            );
        });
    }

    /// Answers as `inner` does, noting when on `clock` each request went out.
    struct Clocked {
        inner: MockProvider,
        clock: Arc<FakeClock>,
        requested_at: Arc<Mutex<Vec<Duration>>>,
    }

    impl Provider for Clocked {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            messages: &'a [Message],
            system: &'a str,
            tools: &'a Value,
            events: &'a flume::Sender<ProviderEvent>,
            options: RequestOptions,
            cache_key: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            self.requested_at
                .lock()
                .unwrap()
                .push(self.clock.monotonic());
            self.inner
                .stream_message(model, messages, system, tools, events, options, cache_key)
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            self.inner.list_models()
        }
    }

    /// An engaged autopilot on a clock the test moves, retrying
    /// `error_retries` times.
    fn clocked(error_retries: u32) -> (Arc<FakeClock>, AutopilotHandle) {
        let clock = FakeClock::new(START_MS);
        let handle = AutopilotHandle::new(Arc::clone(&clock) as Arc<dyn Clock>);
        handle.engage(false, None);
        handle.set_limits(AutopilotConfig {
            error_retries,
            ..AutopilotConfig::default()
        });
        (clock, handle)
    }

    /// Every request `attempts` runs make before the transport gives up.
    fn failed_attempts(attempts: u32) -> Vec<Result<StreamResponse, AgentError>> {
        (0..attempts as usize * ATTEMPT_REQUESTS)
            .map(|_| Err(retry_error(Duration::ZERO)))
            .collect()
    }

    /// Runs `agent` while the test plays its clock: `on_retry` answers each
    /// `Retrying` with the delay it announced. Returns the result and every
    /// event the run sent.
    async fn drive(
        agent: &mut Agent<'_>,
        events: &flume::Receiver<Envelope>,
        mut on_retry: impl FnMut(Duration),
    ) -> (Result<DoneReason, AgentError>, Vec<Envelope>) {
        let seen = Mutex::new(Vec::new());
        let result = futures_lite::future::race(Box::pin(agent.run(default_input())), async {
            while let Ok(envelope) = events.recv_async().await {
                if let AgentEvent::Autopilot(AutopilotEvent::Retrying { delay_ms, .. }) =
                    &envelope.event
                {
                    on_retry(Duration::from_millis(*delay_ms));
                }
                seen.lock().unwrap().push(envelope);
            }
            futures_lite::future::pending().await
        })
        .await;
        let mut seen = seen.into_inner().unwrap();
        seen.extend(drain_events(events));
        (result, seen)
    }

    fn retries(events: &[Envelope]) -> Vec<(u32, u32, Duration, String)> {
        events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                AgentEvent::Autopilot(AutopilotEvent::Retrying {
                    attempt,
                    limit,
                    delay_ms,
                    error,
                }) => Some((
                    *attempt,
                    *limit,
                    Duration::from_millis(*delay_ms),
                    error.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    fn transport_retries(events: &[Envelope]) -> Vec<u32> {
        events
            .iter()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::Retry { attempt, .. } => Some(attempt),
                _ => None,
            })
            .collect()
    }

    fn failed_with(result: &Result<DoneReason, AgentError>, expected: u16) -> bool {
        matches!(result, Err(AgentError::Api { status, .. }) if *status == expected)
    }

    #[test]
    fn retries_back_off_on_the_clock_each_with_a_fresh_transport_budget() {
        smol::block_on(async {
            let (clock, handle) = clocked(RETRY_LIMIT);
            let mut responses = failed_attempts(RETRY_LIMIT);
            responses.push(Ok(text_response(StopReason::EndTurn)));
            let provider = Clocked {
                inner: MockProvider::with_results(responses),
                clock: Arc::clone(&clock),
                requested_at: Arc::default(),
            };
            let requested_at = Arc::clone(&provider.requested_at);
            let mut history = History::default();
            let (agent, events) = make_agent(provider, &mut history);
            let mut agent = agent.with_autopilot(handle.clone());

            let (result, seen) = drive(&mut agent, &events, |delay| clock.advance(delay)).await;
            drop(agent);

            assert_eq!(result.unwrap(), DoneReason::EndTurn);
            let reason = retry_error(Duration::ZERO).user_message_without_wait();
            assert_eq!(
                retries(&seen),
                (1..)
                    .zip(BACKOFF)
                    .map(|(attempt, delay)| (attempt, RETRY_LIMIT, delay, reason.clone()))
                    .collect::<Vec<_>>(),
                "{BACKS_OFF}"
            );
            let mut expected_at = Vec::new();
            let mut at = Duration::ZERO;
            for delay in BACKOFF {
                expected_at.extend(iter::repeat_n(at, ATTEMPT_REQUESTS));
                at += delay;
            }
            expected_at.push(at);
            assert_eq!(*requested_at.lock().unwrap(), expected_at, "{BACKS_OFF}");
            assert_eq!(
                transport_retries(&seen),
                (1..=EXPECTED_RETRY_BUDGET)
                    .cycle()
                    .take(BACKOFF.len() * EXPECTED_RETRY_BUDGET as usize)
                    .collect::<Vec<_>>(),
                "{FRESH_BUDGET}"
            );
            assert!(handle.is_engaged(), "{BACKS_OFF}");
            assert_eq!(continuations(&handle), RETRY_LIMIT, "{RETRY_COUNTED}");
        });
    }

    #[test]
    fn a_retry_never_runs_a_tool_that_already_ran() {
        smol::block_on(async {
            let (clock, handle) = clocked(1);
            let mut responses = vec![Ok(tool_use_response(TEST_TOOL, json!({})))];
            responses.extend(failed_attempts(1));
            responses.push(Ok(text_response(StopReason::EndTurn)));
            let provider = MockProvider::with_results(responses);
            let requests = Arc::clone(&provider.captured_messages);
            let mut history = History::default();
            let (mut agent, events) = make_agent(provider, &mut history);
            let executed = install_partial_stream_tool(&mut agent);
            let mut agent = agent.with_autopilot(handle);

            let (result, _) = drive(&mut agent, &events, |delay| clock.advance(delay)).await;
            drop(agent);

            assert_eq!(result.unwrap(), DoneReason::EndTurn);
            assert_eq!(executed.load(Ordering::SeqCst), 1, "{NOT_REPLAYED}");
            let retried = requests.lock().unwrap().last().cloned().unwrap();
            let calls = retried.iter().flat_map(Message::tool_uses).count();
            let answered = retried
                .iter()
                .flat_map(|message| &message.content)
                .filter(|block| {
                    matches!(
                        block,
                        ContentBlock::ToolResult {
                            is_error: false,
                            ..
                        }
                    )
                })
                .count();
            assert_eq!((calls, answered), (1, 1), "{NOT_REPLAYED}");
        });
    }

    #[test]
    fn a_cancel_during_the_backoff_ends_the_run_cancelled_and_engaged() {
        smol::block_on(async {
            let (_clock, handle) = clocked(RETRY_LIMIT);
            let provider = MockProvider::with_results(failed_attempts(1));
            let requests = Arc::clone(&provider.captured_messages);
            let mut history = History::default();
            let (agent, events) = make_agent(provider, &mut history);
            let (trigger, cancel) = CancelToken::new();
            let mut trigger = Some(trigger);
            let mut agent = agent.with_autopilot(handle.clone()).with_cancel(cancel);

            let (result, seen) = drive(&mut agent, &events, |_| {
                if let Some(trigger) = trigger.take() {
                    trigger.cancel();
                }
            })
            .await;
            drop(agent);

            assert_eq!(result.unwrap(), DoneReason::Cancelled, "{CANCEL_KEEPS}");
            assert_eq!(retries(&seen).len(), 1);
            assert_eq!(requests.lock().unwrap().len(), ATTEMPT_REQUESTS);
            assert!(ends(&seen).is_empty(), "{CANCEL_KEEPS}");
            assert!(handle.is_engaged(), "{CANCEL_KEEPS}");
        });
    }

    #[test]
    fn retry_now_during_the_backoff_retries_at_once() {
        smol::block_on(async {
            let (clock, handle) = clocked(RETRY_LIMIT);
            let mut responses = failed_attempts(1);
            responses.push(Ok(text_response(StopReason::EndTurn)));
            let provider = Clocked {
                inner: MockProvider::with_results(responses),
                clock,
                requested_at: Arc::default(),
            };
            let requested_at = Arc::clone(&provider.requested_at);
            let mut history = History::default();
            let (agent, events) = make_agent(provider, &mut history);
            let retry_now = Nudge::default();
            let mut agent = agent
                .with_autopilot(handle.clone())
                .with_retry_now(retry_now.clone());

            let (result, seen) = drive(&mut agent, &events, |_| retry_now.notify()).await;
            drop(agent);

            assert_eq!(result.unwrap(), DoneReason::EndTurn, "{RETRIES_NOW}");
            assert_eq!(retries(&seen).len(), 1, "{RETRIES_NOW}");
            assert_eq!(
                *requested_at.lock().unwrap(),
                vec![Duration::ZERO; ATTEMPT_REQUESTS + 1],
                "{RETRIES_NOW}"
            );
            assert!(handle.is_engaged(), "{RETRIES_NOW}");
        });
    }

    #[test_case(0 ; "no_retries")]
    #[test_case(2 ; "two_retries")]
    fn spent_retries_fail_the_run_and_end_autopilot(error_retries: u32) {
        smol::block_on(async {
            let (clock, handle) = clocked(error_retries);
            let provider = MockProvider::with_results(failed_attempts(error_retries + 1));
            let requests = Arc::clone(&provider.captured_messages);
            let mut history = History::default();
            let (agent, events) = make_agent(provider, &mut history);
            let mut agent = agent.with_autopilot(handle.clone());
            agent.goal.set(DECISION_GOAL).unwrap();
            let goal = agent.goal.clone();

            let (result, seen) = drive(&mut agent, &events, |delay| clock.advance(delay)).await;
            drop(agent);

            assert!(failed_with(&result, RETRY_STATUS), "{RETRIES_END}");
            assert_eq!(
                requests.lock().unwrap().len(),
                (error_retries as usize + 1) * ATTEMPT_REQUESTS,
                "{RETRIES_END}"
            );
            assert_eq!(
                retries(&seen).len(),
                error_retries as usize,
                "{RETRIES_END}"
            );
            assert_eq!(
                ends(&seen),
                [AutopilotEnd::Error {
                    message: retry_error(Duration::ZERO).to_string(),
                }],
                "{RETRIES_END}"
            );
            assert!(!handle.is_engaged(), "{RETRIES_END}");
            assert!(goal.snapshot().is_some(), "{GOAL_KEPT}");
        });
    }

    #[test_case(|| AgentError::api(BAD_REQUEST_STATUS, BAD_REQUEST_MESSAGE), 1, false ; "bad_request")]
    #[test_case(|| AgentError::api(AUTH_ERROR_STATUS, BAD_REQUEST_MESSAGE), 1, true ; "auth")]
    #[test_case(model_gone, ATTEMPT_REQUESTS, true ; "unavailable_model")]
    fn an_error_autopilot_does_not_retry_fails_at_once(
        error: fn() -> AgentError,
        requested: usize,
        unrecoverable: bool,
    ) {
        smol::block_on(async {
            let (clock, handle) = clocked(RETRY_LIMIT);
            let status = error().status().unwrap();
            let provider =
                MockProvider::with_results((0..requested).map(|_| Err(error())).collect());
            let requests = Arc::clone(&provider.captured_messages);
            let mut history = History::default();
            let (agent, events) = make_agent(provider, &mut history);
            let mut agent = agent.with_autopilot(handle.clone());

            let (result, seen) = drive(&mut agent, &events, |delay| clock.advance(delay)).await;
            drop(agent);

            assert!(failed_with(&result, status), "{FAILS_AT_ONCE}");
            assert!(retries(&seen).is_empty(), "{FAILS_AT_ONCE}");
            assert_eq!(requests.lock().unwrap().len(), requested, "{FAILS_AT_ONCE}");
            assert_eq!(handle.is_engaged(), !unrecoverable, "{ERROR_ENDS}");
        });
    }

    fn model_gone() -> AgentError {
        AgentError::Api {
            status: UNAVAILABLE_STATUS,
            message: MODEL_GONE_MESSAGE.into(),
            retry_after: Some(Duration::ZERO),
        }
    }

    #[test_case(false, |_| {} ; "not_engaged")]
    #[test_case(true, |agent| agent.autopilot = None ; "no_autopilot")]
    #[test_case(true, |agent| agent.root_tool_use_id = Some(TEST_TOOL.into()) ; "a_subagent")]
    fn without_an_engaged_autopilot_a_retryable_error_fails_as_before(
        engage: bool,
        adjust: fn(&mut Agent<'_>),
    ) {
        smol::block_on(async {
            let (clock, handle) = clocked(RETRY_LIMIT);
            if !engage {
                handle.end(STOPPED);
            }
            let provider = MockProvider::with_results(failed_attempts(1));
            let requests = Arc::clone(&provider.captured_messages);
            let mut history = History::default();
            let (agent, events) = make_agent(provider, &mut history);
            let mut agent = agent.with_autopilot(handle.clone());
            adjust(&mut agent);

            let (result, seen) = drive(&mut agent, &events, |delay| clock.advance(delay)).await;
            drop(agent);

            assert!(failed_with(&result, RETRY_STATUS), "{AS_TODAY}");
            assert!(retries(&seen).is_empty(), "{AS_TODAY}");
            assert_eq!(
                requests.lock().unwrap().len(),
                ATTEMPT_REQUESTS,
                "{AS_TODAY}"
            );
            assert!(
                history
                    .as_slice()
                    .last()
                    .and_then(Message::first_text_content)
                    .is_some_and(is_run_failure_marker),
                "{AS_TODAY}"
            );
            assert_eq!(handle.is_engaged(), engage, "{AS_TODAY}");
        });
    }

    #[test]
    fn a_retry_past_the_continuation_budget_fails_the_run_and_ends_autopilot() {
        smol::block_on(async {
            let (clock, handle) = clocked(RETRY_LIMIT);
            handle.set_limits(AutopilotConfig {
                max_continuations: Some(LOWERED_LIMIT),
                ..handle.limits()
            });
            handle.try_continue(&handle.limits()).unwrap();
            let mut history = History::default();
            let (agent, events) =
                make_agent(MockProvider::with_results(failed_attempts(1)), &mut history);
            let mut agent = agent.with_autopilot(handle.clone());

            let (result, seen) = drive(&mut agent, &events, |delay| clock.advance(delay)).await;
            drop(agent);

            assert!(failed_with(&result, RETRY_STATUS), "{BUDGET_ENDS}");
            assert!(retries(&seen).is_empty(), "{BUDGET_ENDS}");
            assert_eq!(
                ends(&seen),
                [AutopilotEnd::Budget {
                    kind: AutopilotBudget::Continuations
                }],
                "{BUDGET_ENDS}"
            );
            assert!(!handle.is_engaged(), "{BUDGET_ENDS}");
        });
    }
}
