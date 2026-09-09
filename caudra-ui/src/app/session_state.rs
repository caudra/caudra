use std::path::{Path, PathBuf};
use std::sync::Arc;

use caudra_agent::prompt::profile::{BUILTIN_PROFILE_NAME, SystemPromptProfile};
use caudra_agent::{GoalHandle, GoalResult};
use caudra_config::ModelPolicy;
use caudra_providers::provider::adjust_model;
use caudra_providers::{
    HistoryItem, HistoryItemKind, Model, ThinkingConfig, Timeouts, TokenUsage, UserOrigin,
    settle_session,
};
use caudra_storage::StateDir;
use caudra_storage::sessions::StoredMode;

use crate::AppSession;

use super::mode::{Mode, PlanState};

pub(crate) struct SessionState {
    /// Shared with the writer thread, so a checkpoint is just a refcount bump.
    pub session: Arc<AppSession>,
    pub model: Model,
    pub token_usage: TokenUsage,
    /// What the session has billed so far: the restored total plus every turn
    /// since. Kept running, because re-deriving it from the counters would
    /// re-price history at today's rates. `None` while nothing was priced.
    pub cost: Option<f64>,
    /// The same running total for turns a subscription covered. Never folded
    /// into `cost`, which is money the user owes.
    pub subscription_cost: Option<f64>,
    pub context_size: u32,
    /// Survives compaction, which throws away the history a count could
    /// otherwise be derived from.
    pub turns: u64,
    pub mode: Mode,
    pub plan: PlanState,
    pub warnings: Vec<String>,
    pub thinking: ThinkingConfig,
    pub fast: bool,
    pub workflow: bool,
    pub system_prompt_profile_name: String,
    pub system_prompt_profile: Option<Arc<SystemPromptProfile>>,
    pub system_prompt_profile_override: bool,
    pub goal: GoalHandle,
}

const PLAN_FILE_MISSING_WARNING: &str = "Plan file was deleted \u{2014} started a new plan";

impl SessionState {
    pub fn from_session(
        mut session: AppSession,
        fallback_model: &Model,
        storage: &StateDir,
        model_policy: &ModelPolicy,
    ) -> Self {
        let mut model = model_policy
            .allows(&session.model)
            .then(|| Model::from_spec(&session.model))
            .transpose()
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                session.model = fallback_model.spec();
                fallback_model.clone()
            });
        // Apply the provider's per-model adjustments (e.g. ZAI's glm-5.2
        // thinking support, or Aperture's routed-provider inheritance) so a
        // resumed session matches one started fresh.
        if let Err(e) = adjust_model(&mut model, Timeouts::default()) {
            tracing::warn!(model = %model.id, error = %e, "failed to adjust resumed model");
        }

        // Plan is where a session that never chose starts, so the agent has to
        // be asked before it may touch the workspace.
        let mode = match session.meta.mode {
            Some(StoredMode::Build) => Mode::Build,
            _ => Mode::Plan,
        };

        let mut warnings = Vec::new();

        let mut plan = match &session.meta.plan_path {
            Some(p) if Path::new(p).exists() => {
                if session.meta.plan_written {
                    PlanState::Ready(PathBuf::from(p))
                } else {
                    PlanState::Drafting(PathBuf::from(p))
                }
            }
            Some(_) => {
                warnings.push(PLAN_FILE_MISSING_WARNING.into());
                PlanState::None
            }
            None => PlanState::None,
        };

        if mode == Mode::Plan {
            plan.allocate_path(storage, Path::new(&session.cwd));
        }

        let fast = session.meta.fast && model.supports_fast();
        let token_usage = session.token_usage;
        let spend = settle_session(&token_usage, session.usage_by_model_mut(), &model, fast);
        let context_size = session.meta.context_size;
        // Sessions saved before the counter existed, and every headless run,
        // carry a zero: recover what the surviving history still shows rather
        // than reporting nothing.
        let turns = match session.meta.turns {
            0 => counted_turns(session.messages()),
            turns => turns,
        };
        let goal = GoalHandle::restored(session.meta.active_goal.as_deref());
        if let Some(stored) = session.meta.goal_result.as_ref() {
            goal.restore_finished(GoalResult {
                condition: Arc::from(stored.condition.as_str()),
                verdict: stored.verdict.into(),
                reason: Arc::from(stored.reason.as_str()),
                evaluations: stored.evaluations,
                duration: std::time::Duration::from_millis(stored.duration_ms),
                usage: stored.usage.into(),
                cost: stored.usage.cost,
                subscription_cost: stored.usage.subscription_cost,
            });
        }

        Self {
            // Saved model may differ from the live one (updated, removed, etc).
            // Reconcile so the UI badge and agent always see the truth.
            // A session that never set a level falls back to the one last
            // chosen anywhere, which is what carries `/thinking` across a
            // restart. `always_thinking` is stamped into the meta before this
            // runs, so config still wins.
            thinking: session
                .meta
                .thinking
                .clone()
                .or_else(|| caudra_storage::thinking::read(storage, &model.spec()))
                .map(Into::into)
                .filter(|_| model.supports_thinking())
                .unwrap_or_default(),
            fast,
            workflow: session.meta.workflow,
            system_prompt_profile_name: BUILTIN_PROFILE_NAME.to_owned(),
            system_prompt_profile: None,
            system_prompt_profile_override: false,
            goal,
            session: Arc::new(session),
            model,
            token_usage,
            cost: spend.billed,
            subscription_cost: spend.subscription,
            context_size,
            turns,
            mode,
            plan,
            warnings,
        }
    }

    pub fn session_mut(&mut self) -> &mut AppSession {
        Arc::make_mut(&mut self.session)
    }

    pub fn update_model(&mut self, model: &Model) {
        if !model.supports_thinking() {
            self.thinking = ThinkingConfig::Off;
        }
        if !model.supports_fast() {
            self.fast = false;
        }
        self.session_mut().set_model(model.spec());
        self.model = model.clone();
    }
}

/// A user turn expands to one item per text and image block, so the group is
/// the exchange. Groups are contiguous, which is what lets this dedupe against
/// the previous one instead of collecting every id.
fn counted_turns(items: &[HistoryItem]) -> u64 {
    let mut turns = 0;
    let mut counted = None;
    for item in items {
        let user_turn = matches!(
            item.kind,
            HistoryItemKind::User {
                origin: UserOrigin::Turn,
                ..
            }
        );
        if user_turn && counted != Some(item.group_id) {
            turns += 1;
            counted = Some(item.group_id);
        }
    }
    turns
}

impl From<Mode> for StoredMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Build => StoredMode::Build,
            Mode::Plan => StoredMode::Plan,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{test_model, test_pricing};
    use caudra_providers::{
        Billing, ContentBlock, FastPricing, ImageMediaType, ImageSource, Message, ModelPricing,
        Role, ThinkingSupport,
    };
    use caudra_storage::thinking::StoredThinking;
    use std::collections::HashMap;
    use test_case::test_case;

    const RECORDED_COST: f64 = 0.42;
    /// A round million, so a per-million rate reads straight off the bill.
    const MILLION_INPUT: TokenUsage = TokenUsage {
        input: 1_000_000,
        output: 0,
        cache_creation: 0,
        cache_read: 0,
    };
    /// [`MILLION_INPUT`] at `test_pricing`'s standard input rate.
    const LIST_PRICE: f64 = 3.0;
    /// Twice the standard rate, so a resume that ignores `fast` bills half.
    const FAST_INPUT_RATE: f64 = 6.0;
    const UNRESOLVABLE_MODEL: &str = "a-model-no-table-has-ever-heard-of";
    const FAST_FLAG_LOST: &str = "the model has fast pricing, so the flag must survive as stored";
    const IMAGE_DATA: &str = "iVBORw0KGgo=";
    const ITEMS_PER_TURN: &str = "the fixture must spread each exchange over several items";
    const TURNS_NOT_ITEMS: &str = "an exchange is one turn however many items it expands to";
    const MODE_DEFAULT: &str = "plan is where a session opens unless it stored a choice";
    const LEVEL_LOST: &str = "the level last chosen must survive into a session that has none";
    const STORED_LEVEL_LOST: &str = "a level the session stored outranks the remembered one";
    const LEVEL_KEPT: &str = "a model that cannot reason must not be sent a level";
    const FIXTURE_REASONS: &str = "the fixture must be a model that cannot reason";

    fn resumed(session: AppSession, model: &Model) -> SessionState {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        SessionState::from_session(session, model, &storage, &ModelPolicy::default())
    }

    /// An old session: counters, no per-model breakdown.
    fn session_with_counters() -> AppSession {
        let mut session = AppSession::new("test-model", "/tmp");
        session.token_usage = MILLION_INPUT;
        session
    }

    fn make_plan_session(mode: Option<StoredMode>, plan_path: Option<String>) -> AppSession {
        let mut session = AppSession::new("test-model", "/tmp");
        session.meta.mode = mode;
        session.meta.plan_path = plan_path;
        session
    }

    /// A resumed session opens on the bill it ran up, not on its counters
    /// re-priced with whatever model is selected now. The model that recorded
    /// this one prices to nothing, so only the recorded cost can answer.
    #[test]
    fn resumed_session_opens_on_the_cost_its_turns_recorded() {
        let mut session = session_with_counters();
        session.add_model_usage(
            UNRESOLVABLE_MODEL,
            session
                .token_usage
                .billed(Some(RECORDED_COST), Billing::Api),
        );
        let state = resumed(session, &test_model());
        assert_eq!(state.cost, Some(RECORDED_COST));
    }

    /// Fast pricing only counts on Anthropic, so this is the one provider a
    /// fast-rate test can use.
    fn fast_priced_model() -> Model {
        Model {
            pricing: ModelPricing {
                fast: Some(FastPricing {
                    input: FAST_INPUT_RATE,
                    output: test_pricing().output,
                }),
                ..test_pricing()
            },
            ..test_model()
        }
    }

    /// Older sessions kept counters only, and those are priced with the
    /// session's own `fast` flag. A hardcoded `false` would open a resumed
    /// fast session on half its bill.
    ///
    /// Priced through [`settle_session`] rather than a resumed session,
    /// because resuming adjusts the model against its provider, which sets the
    /// payer rather than the rates. That would make the column the price lands
    /// in depend on how the machine running the test happens to be logged in.
    #[test_case(false => Some(LIST_PRICE)      ; "standard_rates")]
    #[test_case(true  => Some(FAST_INPUT_RATE) ; "fast_rates")]
    fn counters_without_a_breakdown_price_at_the_session_rate(fast: bool) -> Option<f64> {
        settle_session(
            &MILLION_INPUT,
            &mut HashMap::new(),
            &fast_priced_model(),
            fast,
        )
        .billed
    }

    /// The flag those rates are chosen with is the session's own, clamped to
    /// what the model supports.
    #[test_case(false ; "standard")]
    #[test_case(true  ; "fast")]
    fn resume_keeps_the_stored_fast_flag(fast: bool) {
        let mut session = session_with_counters();
        session.meta.fast = fast;

        let state = resumed(session, &fast_priced_model());

        assert_eq!(state.fast, fast, "{FAST_FLAG_LOST}");
    }

    #[test]
    fn plan_mode_without_path_allocates_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session = make_plan_session(Some(StoredMode::Plan), None);
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(state.mode, Mode::Plan);
        assert!(state.plan.path().is_some(), "plan path should be allocated");
    }

    #[test]
    fn plan_mode_with_missing_file_allocates_new_path_and_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session =
            make_plan_session(Some(StoredMode::Plan), Some("/nonexistent/plan.md".into()));
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(state.mode, Mode::Plan);
        let path = state.plan.path().expect("plan path should be allocated");
        assert_ne!(path, Path::new("/nonexistent/plan.md"));
        assert_eq!(state.warnings.len(), 1);
        assert_eq!(state.warnings[0], PLAN_FILE_MISSING_WARNING);
    }

    #[test]
    fn plan_mode_with_existing_file_preserves_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let plan_file = tmp.path().join("existing-plan.md");
        std::fs::write(&plan_file, "# Plan").unwrap();
        let session = make_plan_session(
            Some(StoredMode::Plan),
            Some(plan_file.to_string_lossy().into_owned()),
        );
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(state.mode, Mode::Plan);
        assert_eq!(state.plan.path(), Some(plan_file.as_path()));
    }

    #[test]
    fn disallowed_restored_model_uses_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let fallback = test_model();
        let mut session = make_plan_session(Some(StoredMode::Build), None);
        session.model = "openai/gpt-5".into();
        let raw: caudra_config::RawConfig = serde_json::from_value(serde_json::json!({
            "provider": {"allowed_models": [fallback.spec()]}
        }))
        .unwrap();
        let policy = raw.into_config(false).unwrap().provider.model_policy;

        let state = SessionState::from_session(session, &fallback, &storage, &policy);

        assert_eq!(state.model.spec(), fallback.spec());
        assert_eq!(state.session.model, fallback.spec());
    }

    #[test]
    fn build_mode_does_not_allocate_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session = make_plan_session(Some(StoredMode::Build), None);
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(state.mode, Mode::Build);
        assert!(state.plan.path().is_none());
    }

    /// A session that never chose opens in plan, so the agent has to be asked
    /// before it may touch the workspace. One that did chose keeps its choice,
    /// which is what makes resuming mid-implementation land back in build.
    #[test_case(None, Mode::Plan ; "unchosen_opens_in_plan")]
    #[test_case(Some(StoredMode::Plan), Mode::Plan ; "stored_plan_survives")]
    #[test_case(Some(StoredMode::Build), Mode::Build ; "stored_build_survives")]
    fn a_session_opens_in_plan_unless_it_chose_otherwise(
        stored: Option<StoredMode>,
        expected: Mode,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let state = SessionState::from_session(
            make_plan_session(stored, None),
            &test_model(),
            &storage,
            &ModelPolicy::default(),
        );
        assert_eq!(state.mode, expected, "{MODE_DEFAULT}");
    }

    /// `/thinking` has to outlive the session it was typed in, so a session
    /// that never set a level takes the one last chosen for its model.
    #[test]
    fn a_session_without_a_level_takes_the_one_last_chosen() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        caudra_storage::thinking::persist(
            &storage,
            &test_model().spec(),
            &StoredThinking::Adaptive,
        );

        let state = SessionState::from_session(
            AppSession::new("test-model", "/tmp"),
            &test_model(),
            &storage,
            &ModelPolicy::default(),
        );

        assert_eq!(state.thinking, ThinkingConfig::Adaptive, "{LEVEL_LOST}");
    }

    /// `always_thinking` is stamped into the meta before a session is restored,
    /// so a stored level has to beat the remembered one or config would lose.
    #[test]
    fn a_stored_level_beats_the_remembered_one() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        caudra_storage::thinking::persist(
            &storage,
            &test_model().spec(),
            &StoredThinking::Adaptive,
        );
        let mut session = AppSession::new("test-model", "/tmp");
        session.meta.thinking = Some(StoredThinking::Off);

        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());

        assert_eq!(state.thinking, ThinkingConfig::Off, "{STORED_LEVEL_LOST}");
    }

    /// The remembered level outlives the session it was chosen in, so it can
    /// still reach a model that cannot reason at all.
    #[test]
    fn a_remembered_level_a_model_cannot_honor_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        caudra_storage::thinking::persist(
            &storage,
            &test_model().spec(),
            &StoredThinking::Adaptive,
        );
        let model = Model {
            thinking_override: Some(ThinkingSupport::No),
            ..test_model()
        };
        assert!(!model.supports_thinking(), "{FIXTURE_REASONS}");

        let state = SessionState::from_session(
            AppSession::new("test-model", "/tmp"),
            &model,
            &storage,
            &ModelPolicy::default(),
        );

        assert_eq!(state.thinking, ThinkingConfig::default(), "{LEVEL_KEPT}");
    }

    #[test]
    fn from_session_applies_provider_adjust_model() {
        // SAFETY: this test runs single-threaded; no other thread reads the env.
        unsafe { std::env::set_var("APERTURE_HOST", "https://example.com") };
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut session = AppSession::new("aperture/zai/glm-5.2", "/tmp");
        session.meta.thinking = Some(StoredThinking::Adaptive);
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert!(
            state.model.supports_thinking(),
            "resumed aperture/zai/glm-5.2 should inherit thinking support from adjust_model",
        );
        assert_eq!(
            state.thinking,
            ThinkingConfig::Adaptive,
            "resumed thinking config should be preserved when the model supports it",
        );
    }

    /// An answer split around a tool call arrives as several text blocks.
    fn split_answer() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "before the tool".into(),
                },
                ContentBlock::Text {
                    text: "after the tool".into(),
                },
            ],
            ..Default::default()
        }
    }

    fn two_turn_history() -> Vec<HistoryItem> {
        crate::history_items(&[
            Message::user_with_images(
                "look at this".into(),
                vec![ImageSource::new(ImageMediaType::Png, Arc::from(IMAGE_DATA))],
            ),
            split_answer(),
            Message::observation("a file changed on disk".into()),
            Message::user("and again".into()),
            split_answer(),
        ])
    }

    /// A prompt with an image expands to a text item and an image item, and an
    /// answer to one item per block, so items badly overstate the exchanges.
    #[test]
    fn counted_turns_counts_exchanges_not_items() {
        let items = two_turn_history();
        assert!(items.len() > 4, "{ITEMS_PER_TURN}");
        assert_eq!(counted_turns(&items), 2, "{TURNS_NOT_ITEMS}");
    }

    #[test_case(0  => 2  ; "a_session_saved_before_the_counter_recovers_it_from_history")]
    #[test_case(97 => 97 ; "a_stored_count_outlives_the_history_compaction_dropped")]
    fn resume_restores_the_turn_count(stored: u64) -> u64 {
        let mut session = AppSession::new("test-model", "/tmp");
        session.replace_messages(two_turn_history());
        session.meta.turns = stored;
        resumed(session, &test_model()).turns
    }
}
