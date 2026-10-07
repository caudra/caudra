//! One automation runtime per session, beside the workflow runtime. Agent loops come and go with
//! every respawn; the runtime follows only a change of session id, so an armed automation keeps
//! its triggers, queue and outbox through a model switch or a revert.

use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use caudra_agent::automation::catalog::Frontend;
use caudra_agent::automation::clock::SystemClock;
use caudra_agent::automation::frontend::{launch_armings, stop_runtime};
use caudra_agent::automation::handle::AutomationHandle;
use caudra_agent::automation::manager::{AutomationRuntime, RuntimeDeps};
use caudra_agent::automation::workflows::Workflows;
use caudra_agent::prompt::profile::SystemPromptProfile;
use caudra_automation::event::SessionView;
use caudra_automation::request::ProfileArming;
use caudra_automation::snapshot::AutomationEvent;
use caudra_config::{AutomationsConfig, Feature, FeatureFlags};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::StoredAutomationControls;
use caudra_workcell::automation_http_client;
use caudra_workspace::WorkspaceSession;
use tracing::{info, warn};

/// What a session's runtime starts from, read from the session as it opens.
pub(crate) struct AutomationSpawn<'a> {
    pub(crate) state_dir: StateDir,
    pub(crate) session_id: CaudraId,
    pub(crate) workspace_session: Option<&'a WorkspaceSession>,
    /// The session runs in a sandbox, so the project's scripts are not this machine's.
    pub(crate) sandbox: bool,
    /// Where a local session works when the process directory cannot be read.
    pub(crate) project_cwd: PathBuf,
    pub(crate) features: FeatureFlags,
    pub(crate) config: AutomationsConfig,
    pub(crate) controls: Option<StoredAutomationControls>,
    pub(crate) profile: Option<&'a SystemPromptProfile>,
    /// `--automation` entries, which only the session focused at launch takes.
    pub(crate) cli: Vec<ProfileArming>,
    pub(crate) facts: SessionView,
    /// The session's workflow runtime, without which `start_workflow()` answers `unavailable`.
    pub(crate) workflows: Option<Arc<dyn Workflows>>,
}

impl AutomationSpawn<'_> {
    /// `None` when automations are off or a remote session's directory cannot be read.
    pub(crate) fn deps(self) -> Option<RuntimeDeps> {
        if !self.features.enabled(Feature::Automations) {
            return None;
        }
        let session_id = self.session_id;
        let cwd = match self.workspace_session {
            Some(workspace) => match smol::block_on(caudra_agent::workspace_logical_cwd(workspace))
            {
                Ok(cwd) => cwd.into(),
                Err(error) => {
                    warn!(%error, %session_id, "remote automation cwd unavailable");
                    return None;
                }
            },
            None => env::current_dir().unwrap_or(self.project_cwd),
        };
        Some(RuntimeDeps {
            state_dir: self.state_dir,
            session_id,
            cwd,
            user_config_dir: None,
            remote: self.workspace_session.is_some() || self.sandbox,
            features: self.features,
            frontend: Frontend::Tui,
            config: self.config,
            controls: self.controls,
            launch: launch_armings(self.profile, self.cli),
            facts: self.facts,
            clock: Arc::new(SystemClock::new()),
            http: automation_http_client(),
            workflows: self.workflows,
        })
    }
}

pub(crate) struct AutomationSession {
    session_id: CaudraId,
    runtime: AutomationRuntime,
    handle: AutomationHandle,
    /// The runtime's only consumer: a second receiver would steal half its events.
    events: flume::Receiver<AutomationEvent>,
}

impl AutomationSession {
    /// `None` when automations are off or the runtime cannot start: the session then runs
    /// without them rather than not at all.
    pub(crate) fn spawn(spawn: AutomationSpawn<'_>) -> Option<Self> {
        spawn.deps().and_then(Self::start)
    }

    pub(crate) fn start(deps: RuntimeDeps) -> Option<Self> {
        let session_id = deps.session_id;
        let runtime = smol::block_on(AutomationRuntime::spawn(deps))
            .map_err(
            |error| warn!(%error, %session_id, "automation runtime unavailable for this session"),
        )
        .ok()?;
        let handle = runtime.handle();
        info!(%session_id, "automation runtime started");
        Some(Self {
            session_id,
            events: handle.events(),
            handle,
            runtime,
        })
    }

    pub(crate) fn session_id(&self) -> CaudraId {
        self.session_id
    }

    pub(crate) fn handle(&self) -> AutomationHandle {
        self.handle.clone()
    }

    pub(crate) fn events(&self) -> &flume::Receiver<AutomationEvent> {
        &self.events
    }

    /// Stops every firing and waits for the store to close, so nothing of this session's
    /// runtime survives into the next one.
    pub(crate) fn shutdown(self) {
        smol::block_on(stop_runtime(self.runtime));
    }
}
