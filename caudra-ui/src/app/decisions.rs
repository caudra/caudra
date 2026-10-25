//! `/decisions`: the engine as the permission manager holds it, beside the
//! decision log in `caudra.db`, which a blocking thread reads for every
//! scope at once. Also the session's decision transcript rows, kept as they
//! are drawn and read back on restore.

use std::sync::{Arc, LazyLock};

use caudra_agent::automation::clock::{Clock, SystemClock};
use caudra_agent::decisions::{
    DecisionFeature, DecisionNote, Decisions, DecisionsError, endpoint_api_key,
    resolve_decision_endpoint, stats_thresholds,
};
use caudra_config::decisions::{DecisionEndpoint, DecisionThresholds, DecisionsConfig};
use caudra_config::providers::ProvidersConfig;
use caudra_config::{ClockFormat, FeatureMode};
use caudra_storage::StateDir;
use caudra_storage::decision_log::{DecisionFilter, DecisionLog, DecisionLogError};
use caudra_storage::decisions::persist_active_decision_endpoint;
use caudra_storage::engine_decisions::StoredEngineDecision;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::SessionDatabase;
use ratatui::Frame;
use ratatui::layout::Rect;
use tracing::warn;

use super::App;
use crate::components::Action;
use crate::components::decisions_modal::{
    DecisionsAction, DecisionsFetchState, DecisionsModalContext, DecisionsScope, DecisionsSnapshot,
    RECENT_LIMIT, ScopeActivity,
};
use crate::components::model_picker::DecisionChoice;
use crate::components::tool_display::format_timestamp_now;

/// What the modal describes when no service is bound: the engine of an
/// install that never configured one.
static UNCONFIGURED: LazyLock<DecisionsConfig> = LazyLock::new(DecisionsConfig::default);

const DECISIONS_OFF: &str = "Decisions off";
const NO_DECISION_ENDPOINTS: &str = "No decision endpoints configured: add one with /login";
const NO_FEATURE_MODE: &str = "Runs whenever an endpoint is active; it has no mode to set";

type Scopes = [ScopeActivity; DecisionsScope::ALL.len()];

/// Every `[decisions.<name>]` table in name order, with where its key comes
/// from. A table that does not resolve lists with its error.
pub(super) fn decision_choices(
    providers: &ProvidersConfig,
    state_dir: &StateDir,
) -> Vec<DecisionChoice> {
    providers
        .decisions
        .iter()
        .map(|(name, def)| {
            let endpoint = def.resolve(name).map_err(|error| error.to_string());
            let key = endpoint.as_ref().ok().and_then(|endpoint| {
                endpoint_api_key(endpoint, state_dir)
                    .inspect_err(|error| warn!(endpoint = %name, %error, "decision key unreadable"))
                    .ok()
                    .flatten()
                    .map(|key| key.source)
            });
            DecisionChoice {
                name: name.clone(),
                endpoint,
                key,
            }
        })
        .collect()
}

/// The endpoint after `active` among those that resolve, wrapping, or the
/// first when decisions are off or the active one is not listed.
fn next_decision_endpoint(providers: &ProvidersConfig, active: Option<&str>) -> Option<String> {
    let valid: Vec<&String> = providers
        .decisions
        .iter()
        .filter(|(name, def)| def.resolve(name).is_ok())
        .map(|(name, _)| name)
        .collect();
    let next = active
        .and_then(|active| valid.iter().position(|name| *name == active))
        .map_or(0, |index| (index + 1) % valid.len());
    valid.get(next).map(|name| (*name).clone())
}

/// A fresh engine for `name` over the `shared` settings. The active row moves
/// only once it builds, so a failed switch leaves everything as it was.
fn switch_decisions(
    shared: &DecisionsConfig,
    state_dir: &StateDir,
    name: Option<&str>,
    providers: impl FnOnce() -> ProvidersConfig,
) -> Result<Decisions, DecisionsError> {
    let endpoint = name
        .map(|name| {
            resolve_decision_endpoint(&providers(), name)?
                .ok_or_else(|| DecisionsError::UnknownEndpoint(name.into()))
        })
        .transpose()?;
    let decisions = Decisions::new(shared.clone().with_endpoint(endpoint), state_dir)?;
    persist_active_decision_endpoint(state_dir, name).map_err(DecisionsError::ActiveRow)?;
    Ok(decisions)
}

/// A fresh engine for `config`, or `None` when `current` already runs it and nothing asks
/// to `reread` what the engine loads on its own, such as the permission question override.
pub(crate) fn rebuilt_decisions(
    current: Option<&Decisions>,
    config: DecisionsConfig,
    reread: bool,
    state_dir: &StateDir,
) -> Option<Result<Decisions, DecisionsError>> {
    let running = current.map_or(&*UNCONFIGURED, Decisions::config);
    (reread || *running != config).then(|| Decisions::new(config, state_dir))
}

/// The mode after `current` among `modes`, wrapping; the first when `current` is not one.
fn next_mode(modes: &[FeatureMode], current: &FeatureMode) -> FeatureMode {
    let next = modes
        .iter()
        .position(|mode| mode == current)
        .map_or(0, |index| (index + 1) % modes.len());
    modes.get(next).cloned().unwrap_or_default()
}

/// What the footer says once `requested` is saved for `feature` and the settings reloaded
/// into `running`, which a project file can still hold off.
pub(crate) fn feature_saved(
    feature: &DecisionFeature,
    requested: &FeatureMode,
    running: &DecisionsConfig,
) -> String {
    let label = feature.label();
    match feature.mode(&running.features) {
        mode if mode == requested => format!("{label}: {}", requested.as_str()),
        mode => format!(
            "{label}: saved {}, but {} is in force; the project caudra.toml can turn a feature off",
            requested.as_str(),
            mode.as_str()
        ),
    }
}

fn switched(decisions: &Decisions) -> String {
    decisions.endpoint().map_or_else(
        || DECISIONS_OFF.to_owned(),
        |endpoint| format!("Decisions: {} · {}", endpoint.name, endpoint.model),
    )
}

impl App {
    /// The endpoint this session screens with, `None` while decisions are off.
    pub fn active_decision_endpoint(&self) -> Option<DecisionEndpoint> {
        self.permissions
            .decisions()
            .and_then(|decisions| decisions.endpoint().cloned())
    }

    /// Switches this session to `name`, or off for `None`, and answers the
    /// session-free engine for the caller to hand every other session. A
    /// failure is flashed and changes nothing.
    pub(crate) fn switch_decision_endpoint(
        &mut self,
        name: Option<&str>,
        providers: impl FnOnce() -> ProvidersConfig,
    ) -> Option<Decisions> {
        let shared = self
            .permissions
            .decisions()
            .map_or_else(DecisionsConfig::default, |decisions| {
                decisions.config().clone()
            });
        match switch_decisions(&shared, &self.storage, name, providers) {
            Ok(decisions) => {
                self.adopt_decisions(&decisions);
                self.flash(switched(&decisions));
                Some(decisions)
            }
            Err(error) => {
                warn!(endpoint = name, %error, "decision endpoint switch failed");
                self.flash(error.to_string());
                None
            }
        }
    }

    pub(super) fn open_decision_model_picker(&mut self) -> Vec<Action> {
        self.model_picker.open_decisions(
            &self.state.model,
            &self.model_policy,
            self.active_decision_endpoint(),
        );
        vec![Action::RefreshModels]
    }

    /// `/decisions next`: the endpoint after the active one, in name order.
    pub(super) fn cycle_decision_endpoint(
        &mut self,
        providers: impl FnOnce() -> ProvidersConfig,
    ) -> Vec<Action> {
        let active = self
            .active_decision_endpoint()
            .map(|endpoint| endpoint.name);
        match next_decision_endpoint(&providers(), active.as_deref()) {
            Some(name) => vec![Action::SetDecisionEndpoint(Some(name))],
            None => {
                self.flash(NO_DECISION_ENDPOINTS.into());
                vec![]
            }
        }
    }

    /// Screens with `decisions` from now on. Content this session already flagged stays
    /// flagged, so a rebuilt engine never loosens what the session may do.
    pub(crate) fn adopt_decisions(&self, decisions: &Decisions) {
        let adopted = decisions.for_session(self.state.session.id);
        if self
            .permissions
            .decisions()
            .is_some_and(|current| current.is_tainted())
        {
            adopted.mark_tainted();
        }
        self.permissions.set_decisions(Some(adopted));
    }

    /// Decisions from here on get a row in every chat, or none.
    pub(crate) fn set_show_decisions(&mut self, shown: bool) {
        self.ui_config.show_decisions = shown;
        for chat in &mut self.chats {
            chat.set_show_decisions(shown);
        }
    }

    pub(super) fn execute_decisions(&mut self) {
        self.decisions_modal.open();
        self.refresh_decisions();
    }

    /// Reads the log off the UI thread, opened read-only so the session that
    /// writes it never waits on the modal. The endpoint is never asked.
    fn refresh_decisions(&mut self) {
        let thresholds = self
            .permissions
            .decisions()
            .map_or_else(DecisionThresholds::default, |decisions| {
                decisions.config().thresholds.clone()
            });
        let session = self.state.session.id.to_string();
        let project = self.permissions.project_cwd().display().to_string();
        let state_dir = self.storage.clone();
        let clock = self.ui_config.clock_format;
        let slot = Arc::clone(&self.decisions_slot);
        slot.store(Some(Arc::new(DecisionsFetchState::Loading)));
        smol::spawn(async move {
            let state = smol::unblock(move || {
                read_decisions(&state_dir, &thresholds, session, &project, clock)
            })
            .await;
            slot.store(Some(Arc::new(state)));
        })
        .detach();
    }

    pub(super) fn handle_decisions_action(&mut self, action: DecisionsAction) -> Vec<Action> {
        match action {
            DecisionsAction::Consumed => {}
            DecisionsAction::Close => self.decisions_modal.close(),
            DecisionsAction::Refresh => self.refresh_decisions(),
            DecisionsAction::Copy { text, label } => self.copy_labelled(&text, label),
            DecisionsAction::Flash(message) => self.flash(message.into()),
            DecisionsAction::CycleFeature(index) => {
                return self.decision_feature_mode(index, next_mode);
            }
            DecisionsAction::PickFeatureMode { feature, mode } => {
                return self.decision_feature_mode(feature, |_, _| mode);
            }
        }
        Vec::new()
    }

    /// The mode `pick` chooses for the feature from its allowed modes and the one in force,
    /// for the event loop to save. Picking the mode in force saves nothing.
    fn decision_feature_mode(
        &mut self,
        index: usize,
        pick: impl FnOnce(&[FeatureMode], &FeatureMode) -> FeatureMode,
    ) -> Vec<Action> {
        let Some(feature) = DecisionFeature::ALL.get(index) else {
            return Vec::new();
        };
        let Some(setting) = feature.setting() else {
            self.flash(NO_FEATURE_MODE.into());
            return Vec::new();
        };
        let decisions = self.permissions.decisions();
        let config = decisions.as_ref().map_or(&*UNCONFIGURED, Decisions::config);
        let running = feature.mode(&config.features);
        let mode = pick(setting.modes, running);
        if mode == *running || !setting.modes.contains(&mode) {
            return Vec::new();
        }
        vec![Action::SetDecisionFeature {
            feature: feature.clone(),
            mode,
        }]
    }

    pub(super) fn view_decisions_modal(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let decisions = self.permissions.decisions();
        let config = decisions.as_ref().map_or(&*UNCONFIGURED, Decisions::config);
        let status = decisions
            .as_ref()
            .map(Decisions::status)
            .unwrap_or_default();
        let mode = self.permissions.mode();
        let log_path = DecisionLog::file_path(&self.storage);
        let ctx = DecisionsModalContext {
            config,
            status: &status,
            tainted: decisions.as_ref().is_some_and(Decisions::is_tainted),
            mode: &mode,
            key_source: decisions.as_ref().and_then(Decisions::key_source),
            log_path: &log_path,
            clock: self.ui_config.clock_format,
        };
        self.decisions_modal.view(frame, area, &ctx)
    }

    /// Keeps the row a note drew for `agent`, `None` being the main agent,
    /// whatever `show_decisions` says. The session's row goes to the writer
    /// first, so the decision has one to reference.
    pub(super) fn record_engine_decision(&mut self, note: &DecisionNote, agent: Option<String>) {
        self.checkpoint_now();
        let at_ms = u64::try_from(SystemClock::new().now_ms()).unwrap_or_default();
        self.storage_writer
            .record_engine_decision(self.state.session.id, note.stored(at_ms, agent));
    }

    /// The session's rows a restore draws, none while `show_decisions` is off.
    pub(super) fn stored_engine_decisions(&self) -> Vec<StoredEngineDecision> {
        if !self.ui_config.show_decisions {
            return Vec::new();
        }
        load_engine_decisions(&self.storage, self.state.session.id)
    }
}

/// A session's decision rows, or none when they cannot be read: they are for
/// display alone, so a restore goes on without them.
pub(super) fn load_engine_decisions(
    storage: &StateDir,
    session_id: CaudraId,
) -> Vec<StoredEngineDecision> {
    SessionDatabase::open(storage)
        .and_then(|database| database.load_engine_decisions(session_id))
        .unwrap_or_else(|error| {
            warn!(%error, %session_id, "decision rows unreadable; restoring without them");
            Vec::new()
        })
}

/// Every scope's stats and newest rows, or why there are none.
fn read_decisions(
    state_dir: &StateDir,
    thresholds: &DecisionThresholds,
    session: String,
    project: &str,
    clock: ClockFormat,
) -> DecisionsFetchState {
    match read_scopes(state_dir, thresholds, &session, project) {
        Ok(Some(scopes)) => DecisionsFetchState::Ready(Box::new(DecisionsSnapshot {
            scopes,
            session,
            loaded_at: format_timestamp_now(clock),
        })),
        Ok(None) => DecisionsFetchState::Missing,
        Err(error) => {
            warn!(
                %error,
                path = %DecisionLog::file_path(state_dir).display(),
                "decision log unreadable"
            );
            DecisionsFetchState::Failed(error.to_string())
        }
    }
}

/// `None` when the canonical database is absent.
fn read_scopes(
    state_dir: &StateDir,
    thresholds: &DecisionThresholds,
    session: &str,
    project: &str,
) -> Result<Option<Scopes>, DecisionLogError> {
    let Some(log) = DecisionLog::open_read_only(state_dir)? else {
        return Ok(None);
    };
    let mut scopes = Scopes::default();
    for scope in DecisionsScope::ALL {
        let filter = match scope {
            DecisionsScope::Session => DecisionFilter {
                session: Some(session),
                ..DecisionFilter::default()
            },
            DecisionsScope::Project => DecisionFilter {
                project: Some(project),
                ..DecisionFilter::default()
            },
            DecisionsScope::All => DecisionFilter::default(),
        };
        scopes[scope.index()] = ScopeActivity {
            stats: log.stats(&filter, |feature| stats_thresholds(thresholds, feature))?,
            recent: log.recent(&filter, RECENT_LIMIT)?,
        };
    }
    Ok(Some(scopes))
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

    use arc_swap::ArcSwapOption;
    use async_trait::async_trait;
    use caudra_agent::decisions::{DecisionContext, PermissionPurpose};
    use caudra_config::decisions::{
        DecisionEndpoint, DecisionEndpointDef, DecisionFeatures, DecisionProtocol,
    };
    use caudra_decision::{DecisionEngine, DecisionError, DecisionRequest, DecisionResponse};
    use caudra_storage::decision_log::{DecisionEffect, DecisionRecord, EndpointKind};
    use caudra_storage::decisions::read_active_decision_endpoint;
    use caudra_storage::sessions::{SESSIONS_DB_FILE, SessionDatabase};
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::agent::shared_queue;
    use crate::app::Msg;
    use crate::app::tests::{
        DECISIONS_TEST_BASE_URL, click_status, cmd, private_tempdir, tempdir_app,
    };
    use crate::components::Action;
    use crate::components::Overlay;
    use crate::components::keybindings::key;
    use crate::components::login_picker::LoginPickerAction;
    use crate::components::model_picker::{ModelPicker, ModelPickerAction};
    use crate::components::now_secs;
    use crate::components::status_bar::StatusBarHitTarget;

    const DECISION_MODEL_COMMAND: &str = "/decisions model";
    const OTHER_SESSION: &str = "other-session";
    const OTHER_PROJECT: &str = "/elsewhere";
    const QUESTION_SET: &str = "test-questions";
    const QUESTION_SET_VERSION: &str = "1";
    const QUESTION: &str = "destructive";
    const QUESTION_TYPE: &str = "noul";
    const MODEL: &str = "test-decision-model";
    const MODE: &str = "shadow";
    const LATENCY_MS: u64 = 5;
    const RETENTION_DAYS: u64 = 30;
    const READ_DEADLINE: Duration = Duration::from_secs(10);
    const READ_TIMED_OUT: &str = "the decision log read never settled";
    const NOT_READY: &str = "the decision log read did not succeed";

    type ScopeRows = [usize; DecisionsScope::ALL.len()];

    struct UnreachableEngine;

    #[async_trait]
    impl DecisionEngine for UnreachableEngine {
        async fn decide(
            &self,
            _request: &DecisionRequest,
            _deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            Err(DecisionError::Unreachable)
        }
    }

    /// Isolate the canonical database from other apps on this test thread.
    fn isolated_app() -> (TempDir, App) {
        let (tmp, _dir, _writer, mut app) = tempdir_app();
        let (queue, _receiver) = shared_queue::queue();
        app.queue.set_shared(queue);
        (tmp, app)
    }

    fn record(session: &str, project: &str) -> DecisionRecord {
        DecisionRecord {
            timestamp: now_secs(),
            session: Some(session.into()),
            project: Some(project.into()),
            feature: DecisionFeature::PermissionAdvice.name().into(),
            question_set_id: QUESTION_SET.into(),
            question_set_version: QUESTION_SET_VERSION.into(),
            endpoint_kind: EndpointKind::Local,
            model: MODEL.into(),
            state: json!({}),
            questions: json!({ QUESTION: { "type": QUESTION_TYPE } }),
            answers: None,
            error: None,
            latency_ms: LATENCY_MS,
            mode: MODE.into(),
            effect: DecisionEffect::None,
            meta: json!({}),
        }
    }

    fn log_rows(app: &App, rows: &[(&str, &str)]) {
        let log = DecisionLog::open(&app.storage, true, RETENTION_DAYS)
            .unwrap()
            .unwrap();
        for (session, project) in rows {
            log.insert(&record(session, project)).unwrap();
        }
    }

    fn project(app: &App) -> String {
        app.permissions.project_cwd().display().to_string()
    }

    /// Waits out the background read, which nothing but the tick would
    /// otherwise notice.
    fn settled(app: &App) -> Arc<DecisionsFetchState> {
        let deadline = Instant::now() + READ_DEADLINE;
        loop {
            match app.decisions_slot.load_full() {
                Some(state) if !matches!(*state, DecisionsFetchState::Loading) => return state,
                _ => {
                    assert!(Instant::now() < deadline, "{READ_TIMED_OUT}");
                    thread::yield_now();
                }
            }
        }
    }

    fn ready(state: &DecisionsFetchState) -> &DecisionsSnapshot {
        let DecisionsFetchState::Ready(snapshot) = state else {
            panic!("{NOT_READY}");
        };
        snapshot
    }

    /// How many rows each scope read, which its stats must count alike.
    fn scope_rows(state: &DecisionsFetchState) -> ScopeRows {
        ready(state).scopes.each_ref().map(|scope| {
            let counted: u64 = scope.stats.iter().map(|stats| stats.count).sum();
            assert_eq!(counted, scope.recent.len() as u64);
            scope.recent.len()
        })
    }

    #[test]
    fn each_scope_reads_only_its_own_rows() {
        let (_tmp, mut app) = isolated_app();
        let session = app.state.session.id.to_string();
        let project = project(&app);
        log_rows(
            &app,
            &[
                (&session, &project),
                (OTHER_SESSION, &project),
                (OTHER_SESSION, OTHER_PROJECT),
            ],
        );

        app.execute_decisions();

        assert!(app.decisions_modal.is_open());
        let state = settled(&app);
        assert_eq!(scope_rows(&state), [1, 2, 3]);
        assert_eq!(ready(&state).session, session);
    }

    #[test_case(false; "missing_database_is_not_created")]
    #[test_case(true; "empty_canonical_database_is_readable")]
    fn empty_decision_log_reads_the_canonical_database(existing: bool) {
        let tmp = private_tempdir();
        let state = StateDir::from_path(tmp.path().to_path_buf());
        if existing {
            drop(SessionDatabase::open(&state).unwrap());
        }

        let scopes = read_scopes(
            &state,
            &DecisionThresholds::default(),
            OTHER_SESSION,
            OTHER_PROJECT,
        )
        .unwrap();

        assert_eq!(scopes.is_some(), existing);
        if let Some(scopes) = scopes {
            assert!(
                scopes
                    .iter()
                    .all(|scope| scope.stats.is_empty() && scope.recent.is_empty())
            );
        }
        assert_eq!(state.path().join(SESSIONS_DB_FILE).exists(), existing);
    }

    #[test]
    fn refresh_reads_the_log_again() {
        let (_tmp, mut app) = isolated_app();
        let project = project(&app);
        log_rows(&app, &[(OTHER_SESSION, OTHER_PROJECT)]);
        app.execute_decisions();
        assert_eq!(scope_rows(&settled(&app)), [0, 0, 1]);
        log_rows(&app, &[(OTHER_SESSION, &project)]);

        app.update(Msg::Key(key::REFRESH.to_key_event()));

        assert!(app.decisions_modal.is_open());
        assert_eq!(scope_rows(&settled(&app)), [0, 1, 2]);
    }

    #[test]
    fn a_new_session_reads_its_own_rows() {
        let (_tmp, mut app) = isolated_app();
        let decisions = Decisions::new(DecisionsConfig::default(), &app.storage).unwrap();
        app.permissions
            .set_decisions(Some(decisions.for_session(app.state.session.id)));
        let previous = app.state.session.id.to_string();

        app.reset_session();

        let current = app.state.session.id;
        assert_eq!(
            app.permissions
                .decisions()
                .and_then(|decisions| decisions.session()),
            Some(current)
        );
        let project = project(&app);
        log_rows(
            &app,
            &[(&previous, &project), (&current.to_string(), &project)],
        );
        app.execute_decisions();
        let state = settled(&app);
        assert_eq!(scope_rows(&state), [1, 2, 2]);
        let session = &ready(&state).scopes[DecisionsScope::Session.index()].recent[0]
            .record
            .session;
        assert_eq!(session.as_deref(), Some(current.to_string().as_str()));
    }

    /// The chip is the only place the bar admits the engine failed, so a click
    /// opens the inspector that explains it rather than probing again.
    #[test]
    fn clicking_the_offline_chip_opens_the_inspector() {
        let (_tmp, mut app) = isolated_app();
        let config = DecisionsConfig {
            endpoint: Some(DecisionEndpoint::new(
                DecisionProtocol::TypeSafe.as_str(),
                DecisionProtocol::TypeSafe,
                DECISIONS_TEST_BASE_URL.parse().unwrap(),
            )),
            features: DecisionFeatures {
                permission_advice: FeatureMode::Shadow,
                ..Default::default()
            },
            ..Default::default()
        };
        let decisions = Decisions::with_engine(config, &app.storage, UnreachableEngine).unwrap();
        smol::block_on(decisions.permission(
            PermissionPurpose::Advice,
            &json!({}),
            &DecisionContext::default(),
        ));
        app.permissions.set_decisions(Some(decisions));

        assert!(click_status(&mut app, StatusBarHitTarget::Decisions).is_empty());

        assert!(app.decisions_modal.is_open());
        assert_eq!(app.status_hover, None);
    }

    const ENDPOINT: &str = "tunnel";
    const UNKNOWN_ENDPOINT: &str = "gone";
    const SWITCHED_FLASH: &str = "Decisions: tunnel · test-decision-model";
    const LAST_ENDPOINT: &str = "zeta";
    const BROKEN_ENDPOINT: &str = "broken";
    const REMOTE_HTTP_URL: &str = "http://192.0.2.1";

    fn endpoint_def(base_url: &str) -> DecisionEndpointDef {
        DecisionEndpointDef {
            protocol: DecisionProtocol::OpenAI,
            base_url: base_url.into(),
            model: Some(MODEL.into()),
            api_key_env: None,
            allow_http: None,
        }
    }

    fn endpoint_providers() -> ProvidersConfig {
        let mut providers = ProvidersConfig::default();
        providers
            .decisions
            .insert(ENDPOINT.into(), endpoint_def(DECISIONS_TEST_BASE_URL));
        providers
    }

    /// `broken` sorts first but never resolves, so cycling skips it.
    fn cycle_providers() -> ProvidersConfig {
        let mut providers = endpoint_providers();
        providers
            .decisions
            .insert(LAST_ENDPOINT.into(), endpoint_def(DECISIONS_TEST_BASE_URL));
        providers
            .decisions
            .insert(BROKEN_ENDPOINT.into(), endpoint_def(REMOTE_HTTP_URL));
        providers
    }

    fn active_name(app: &App) -> Option<String> {
        app.active_decision_endpoint().map(|endpoint| endpoint.name)
    }

    #[test]
    fn switching_to_an_endpoint_rebuilds_the_session_engine() {
        let (_tmp, mut app) = isolated_app();

        let decisions = app.switch_decision_endpoint(Some(ENDPOINT), endpoint_providers);

        assert!(decisions.is_some_and(|decisions| decisions.session().is_none()));
        assert_eq!(active_name(&app).as_deref(), Some(ENDPOINT));
        assert_eq!(
            app.permissions
                .decisions()
                .and_then(|decisions| decisions.session()),
            Some(app.state.session.id)
        );
        assert_eq!(
            read_active_decision_endpoint(&app.storage).as_deref(),
            Some(ENDPOINT)
        );
        assert_eq!(app.status_bar.flash_text(), Some(SWITCHED_FLASH));
    }

    #[test]
    fn switching_off_drops_the_endpoint() {
        let (_tmp, mut app) = isolated_app();
        app.switch_decision_endpoint(Some(ENDPOINT), endpoint_providers);

        assert!(
            app.switch_decision_endpoint(None, endpoint_providers)
                .is_some()
        );

        assert_eq!(active_name(&app), None);
        assert_eq!(read_active_decision_endpoint(&app.storage), None);
        assert_eq!(app.status_bar.flash_text(), Some(DECISIONS_OFF));
    }

    #[test_case(true; "active")]
    #[test_case(false; "inactive")]
    fn a_configured_endpoint_switches_only_when_active(activate: bool) {
        let (_tmp, mut app) = isolated_app();

        let actions = app.handle_login_picker_action(LoginPickerAction::DecisionConfigured {
            name: ENDPOINT.into(),
            activate,
        });

        let switched: Vec<_> = actions
            .iter()
            .filter_map(|action| match action {
                Action::SetDecisionEndpoint(name) => name.as_deref(),
                _ => None,
            })
            .collect();
        assert_eq!(
            switched,
            activate.then_some(ENDPOINT).into_iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_unknown_endpoint_flashes_and_changes_nothing() {
        let (_tmp, mut app) = isolated_app();
        app.switch_decision_endpoint(Some(ENDPOINT), endpoint_providers);

        assert!(
            app.switch_decision_endpoint(Some(UNKNOWN_ENDPOINT), endpoint_providers)
                .is_none()
        );

        assert_eq!(active_name(&app).as_deref(), Some(ENDPOINT));
        assert_eq!(
            read_active_decision_endpoint(&app.storage).as_deref(),
            Some(ENDPOINT)
        );
        assert_eq!(
            app.status_bar.flash_text(),
            Some(
                DecisionsError::UnknownEndpoint(UNKNOWN_ENDPOINT.into())
                    .to_string()
                    .as_str()
            )
        );
    }

    #[test_case(None, ENDPOINT; "off_picks_the_first_valid")]
    #[test_case(Some(ENDPOINT), LAST_ENDPOINT; "steps_forward")]
    #[test_case(Some(LAST_ENDPOINT), ENDPOINT; "last_wraps_to_first")]
    fn decision_next_cycles_through_valid_endpoints(active: Option<&str>, expected: &str) {
        let (_tmp, mut app) = isolated_app();
        if let Some(name) = active {
            app.switch_decision_endpoint(Some(name), cycle_providers);
        }

        let actions = app.cycle_decision_endpoint(cycle_providers);

        assert!(matches!(
            &actions[..],
            [Action::SetDecisionEndpoint(Some(name))] if name == expected
        ));
    }

    #[test]
    fn decision_next_without_endpoints_says_how_to_add_one() {
        let (_tmp, mut app) = isolated_app();

        assert!(
            app.cycle_decision_endpoint(ProvidersConfig::default)
                .is_empty()
        );

        assert_eq!(app.status_bar.flash_text(), Some(NO_DECISION_ENDPOINTS));
    }

    #[test]
    fn choices_list_every_table_and_explain_the_invalid_one() {
        let (_tmp, app) = isolated_app();

        let choices = decision_choices(&cycle_providers(), &app.storage);

        let names: Vec<_> = choices.iter().map(|choice| choice.name.as_str()).collect();
        assert_eq!(names, [BROKEN_ENDPOINT, ENDPOINT, LAST_ENDPOINT]);
        assert!(choices[0].endpoint.is_err());
        assert!(
            choices[1..]
                .iter()
                .all(|choice| choice.endpoint.is_ok() && choice.key.is_none())
        );
    }

    #[test]
    fn decision_model_opens_the_picker() {
        let (_tmp, mut app) = isolated_app();
        app.model_picker = ModelPicker::new(Arc::new(ArcSwapOption::empty()));

        let actions = app.execute_command(cmd(DECISION_MODEL_COMMAND), 0);

        assert!(app.model_picker.is_open());
        assert!(matches!(&actions[..], [Action::RefreshModels]));
    }

    #[test_case(None; "off")]
    #[test_case(Some(ENDPOINT); "endpoint")]
    fn a_picked_decision_switches_the_endpoint(name: Option<&str>) {
        let (_tmp, mut app) = isolated_app();

        let actions =
            app.handle_model_picker_action(ModelPickerAction::SelectDecision(name.map(Into::into)));

        assert!(matches!(
            &actions[..],
            [Action::SetDecisionEndpoint(picked)] if picked.as_deref() == name
        ));
    }

    const IN_FORCE_FLASH: &str = "memory_search: enforce";
    const HELD_OFF_FLASH: &str = "memory_search: saved enforce, but off is in force; the project caudra.toml can turn a feature off";

    fn memory_search(mode: FeatureMode) -> DecisionsConfig {
        let mut config = DecisionsConfig::default();
        config.features.memory_search = mode;
        config
    }

    #[test_case(FeatureMode::Off, false, false; "unchanged_settings_keep_the_engine")]
    #[test_case(FeatureMode::Off, true, true; "a_reread_rebuilds_it")]
    #[test_case(FeatureMode::Enforce, false, true; "changed_settings_rebuild_it")]
    fn an_engine_is_rebuilt_only_when_something_changed(
        mode: FeatureMode,
        reread: bool,
        rebuilt: bool,
    ) {
        let (_tmp, app) = isolated_app();
        let current = Decisions::new(DecisionsConfig::default(), &app.storage).unwrap();

        let fresh = rebuilt_decisions(
            Some(&current),
            memory_search(mode.clone()),
            reread,
            &app.storage,
        );

        assert_eq!(fresh.is_some(), rebuilt);
        if let Some(fresh) = fresh {
            assert_eq!(fresh.unwrap().config().features.memory_search, mode);
        }
    }

    #[test_case(true; "flagged")]
    #[test_case(false; "clean")]
    fn an_adopted_engine_keeps_what_the_session_flagged(tainted: bool) {
        let (_tmp, app) = isolated_app();
        let running = Decisions::new(DecisionsConfig::default(), &app.storage).unwrap();
        app.adopt_decisions(&running);
        if tainted {
            app.permissions.decisions().unwrap().mark_tainted();
        }

        app.adopt_decisions(
            &Decisions::new(memory_search(FeatureMode::Enforce), &app.storage).unwrap(),
        );

        let adopted = app.permissions.decisions().unwrap();
        assert_eq!(adopted.is_tainted(), tainted);
        assert_eq!(adopted.session(), Some(app.state.session.id));
        assert_eq!(
            adopted.config().features.memory_search,
            FeatureMode::Enforce
        );
    }

    #[test_case(FeatureMode::Off, FeatureMode::Shadow; "off_to_shadow")]
    #[test_case(FeatureMode::Shadow, FeatureMode::Enforce; "shadow_to_enforce")]
    #[test_case(FeatureMode::Enforce, FeatureMode::Off; "enforce_wraps_to_off")]
    fn the_feature_cycles_through_its_allowed_modes(running: FeatureMode, next: FeatureMode) {
        let (_tmp, mut app) = isolated_app();
        app.adopt_decisions(&Decisions::new(memory_search(running), &app.storage).unwrap());
        let index = DecisionFeature::ALL
            .iter()
            .position(|feature| *feature == DecisionFeature::MemorySearch)
            .unwrap();

        let actions = app.handle_decisions_action(DecisionsAction::CycleFeature(index));

        assert!(matches!(
            &actions[..],
            [Action::SetDecisionFeature { feature: DecisionFeature::MemorySearch, mode }] if *mode == next
        ));
    }

    #[test_case(FeatureMode::Shadow, Some(FeatureMode::Enforce); "another_mode_is_saved")]
    #[test_case(FeatureMode::Enforce, None; "the_mode_in_force_saves_nothing")]
    fn a_clicked_mode_is_picked(running: FeatureMode, saved: Option<FeatureMode>) {
        let (_tmp, mut app) = isolated_app();
        app.adopt_decisions(&Decisions::new(memory_search(running), &app.storage).unwrap());
        let index = DecisionFeature::ALL
            .iter()
            .position(|feature| *feature == DecisionFeature::MemorySearch)
            .unwrap();

        let actions = app.handle_decisions_action(DecisionsAction::PickFeatureMode {
            feature: index,
            mode: FeatureMode::Enforce,
        });

        let picked: Vec<_> = actions
            .iter()
            .filter_map(|action| match action {
                Action::SetDecisionFeature { mode, .. } => Some(mode.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(picked, saved.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn a_feature_without_a_mode_is_not_cycled() {
        let (_tmp, mut app) = isolated_app();
        let index = DecisionFeature::ALL
            .iter()
            .position(|feature| feature.setting().is_none())
            .unwrap();

        let actions = app.handle_decisions_action(DecisionsAction::CycleFeature(index));

        assert!(actions.is_empty());
        assert_eq!(app.status_bar.flash_text(), Some(NO_FEATURE_MODE));
    }

    #[test_case(FeatureMode::Enforce, IN_FORCE_FLASH; "in_force")]
    #[test_case(FeatureMode::Off, HELD_OFF_FLASH; "held_off_by_the_project")]
    fn the_saved_mode_names_what_is_in_force(running: FeatureMode, expected: &str) {
        assert_eq!(
            feature_saved(
                &DecisionFeature::MemorySearch,
                &FeatureMode::Enforce,
                &memory_search(running)
            ),
            expected
        );
    }

    #[test]
    fn adding_an_endpoint_opens_login_at_the_decision_section() {
        let (_tmp, mut app) = isolated_app();

        let actions = app.handle_model_picker_action(ModelPickerAction::AddDecisionEndpoint);

        assert!(actions.is_empty());
        assert!(!app.model_picker.is_open());
        assert!(app.login_picker.is_open());
        assert!(app.login_picker.on_decision_row());
    }
}
