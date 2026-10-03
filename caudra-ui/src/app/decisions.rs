//! `/decisions`: the engine as the permission manager holds it, beside the
//! activity `decisions.db` logged, which a blocking thread reads for every
//! scope at once.

use std::sync::{Arc, LazyLock};

use caudra_agent::decisions::{Decisions, stats_thresholds};
use caudra_config::ClockFormat;
use caudra_config::decisions::{DecisionThresholds, DecisionsConfig};
use caudra_storage::StateDir;
use caudra_storage::decision_log::{DecisionFilter, DecisionLog, DecisionLogError};
use ratatui::Frame;
use ratatui::layout::Rect;
use tracing::warn;

use super::{App, DECISIONS_USAGE};
use crate::components::decisions_modal::{
    DecisionsAction, DecisionsFetchState, DecisionsModalContext, DecisionsScope, DecisionsSnapshot,
    RECENT_LIMIT, ScopeActivity,
};
use crate::components::tool_display::format_timestamp_now;

/// What the modal describes when no service is bound: the engine of an
/// install that never configured one.
static UNCONFIGURED: LazyLock<DecisionsConfig> = LazyLock::new(DecisionsConfig::default);

type Scopes = [ScopeActivity; DecisionsScope::ALL.len()];

impl App {
    pub(super) fn execute_decisions(&mut self, args: &str) {
        if !args.trim().is_empty() {
            self.flash(DECISIONS_USAGE.into());
            return;
        }
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

    pub(super) fn handle_decisions_action(&mut self, action: DecisionsAction) {
        match action {
            DecisionsAction::Consumed => {}
            DecisionsAction::Close => self.decisions_modal.close(),
            DecisionsAction::Refresh => self.refresh_decisions(),
            DecisionsAction::Copy { text, label } => self.copy_labelled(&text, label),
            DecisionsAction::Flash(message) => self.flash(message.into()),
        }
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
            api_key_set: matches!(config.api_key(), Ok(Some(_))),
            log_path: &log_path,
            clock: self.ui_config.clock_format,
        };
        self.decisions_modal.view(frame, area, &ctx)
    }
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

/// `None` when nothing was ever logged here.
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

    use async_trait::async_trait;
    use caudra_agent::decisions::{DecisionContext, DecisionFeature, PermissionPurpose};
    use caudra_config::decisions::{DecisionFeatures, FeatureMode};
    use caudra_decision::{DecisionEngine, DecisionError, DecisionRequest, DecisionResponse};
    use caudra_storage::decision_log::{DecisionEffect, DecisionRecord, EndpointKind};
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::agent::shared_queue;
    use crate::app::Msg;
    use crate::app::tests::{DECISIONS_TEST_BASE_URL, click_status, tempdir_app};
    use crate::components::keybindings::key;
    use crate::components::now_secs;
    use crate::components::status_bar::StatusBarHitTarget;

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

    /// The log is one file in the state directory, which `test_app` shares
    /// across the whole run.
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

        app.execute_decisions("");

        assert!(app.decisions_modal.is_open());
        let state = settled(&app);
        assert_eq!(scope_rows(&state), [1, 2, 3]);
        assert_eq!(ready(&state).session, session);
    }

    #[test]
    fn a_missing_log_reads_as_missing_and_is_never_created() {
        let (_tmp, mut app) = isolated_app();

        app.execute_decisions("");

        assert!(matches!(*settled(&app), DecisionsFetchState::Missing));
        assert!(!DecisionLog::file_path(&app.storage).exists());
    }

    #[test]
    fn refresh_reads_the_log_again() {
        let (_tmp, mut app) = isolated_app();
        let project = project(&app);
        log_rows(&app, &[(OTHER_SESSION, OTHER_PROJECT)]);
        app.execute_decisions("");
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
        app.execute_decisions("");
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
            base_url: Some(DECISIONS_TEST_BASE_URL.parse().unwrap()),
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
}
