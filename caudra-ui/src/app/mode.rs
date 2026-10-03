use std::path::{Path, PathBuf};

use crate::agent::QueuedMessage;
use crate::chat::plan_source;
use crate::components::Status;
use crate::components::status_bar::ModeLabel;
use crate::theme;
use caudra_agent::mentions;
use caudra_agent::tools::native::plan::{PlanTarget, PlanWriteResult};
use caudra_agent::{AgentInput, AgentMode, CommitRef, Mention, commits};
use caudra_providers::ModelPurpose;
use caudra_providers::model_registry;
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::plans::{self, PlanFile};
use caudra_workspace::{LocalDocumentRef, PlanRef, WorkspaceSession};
use ratatui::style::{Color, Modifier, Style};

use super::App;

const BASH_LABEL: &str = "[BASH]";
const BASH_SHORT_LABEL: &str = "[$]";
const BUILD_LABEL: &str = "[BUILD]";
const BUILD_SHORT_LABEL: &str = "[B]";
const PLAN_LABEL: &str = "[PLAN]";
const PLAN_SHORT_LABEL: &str = "[P]";
const TO_PLAN_LABEL: &str = "[BUILD\u{2192}PLAN]";
const TO_PLAN_SHORT_LABEL: &str = "[B\u{2192}P]";
const TO_BUILD_LABEL: &str = "[PLAN\u{2192}BUILD]";
const TO_BUILD_SHORT_LABEL: &str = "[P\u{2192}B]";
const PLAN_NOT_ACTIVE: &str = "The ready plan is not the executing plan";
const PLAN_EMPTY: &str = "The plan has not been written yet";
const PLAN_STORE_UNAVAILABLE: &str = "The plan document store is unavailable";
pub(super) const PLAN_COPY_FAILED: &str = "Could not copy the plan to the new session";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Build,
    Plan,
}

pub(crate) enum PlanTrigger {
    WriteDone,
    InteractivePrompt,
}

impl Mode {
    pub(crate) fn color(&self) -> Color {
        match self {
            Self::Build => theme::current().mode_build,
            Self::Plan => theme::current().mode_plan,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum PlanState {
    #[default]
    None,
    Drafting(PathBuf),
    Ready(PathBuf),
    RemoteDrafting(PlanRef),
    RemoteReady(PlanRef),
}

pub(super) struct PlanSnapshot {
    pub content: String,
    pub source: String,
    pub target: PlanTarget,
}

impl PlanState {
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::None | Self::RemoteDrafting(_) | Self::RemoteReady(_) => Option::None,
            Self::Drafting(p) | Self::Ready(p) => Some(p),
        }
    }

    pub(crate) fn reference(&self) -> Option<&PlanRef> {
        match self {
            Self::RemoteDrafting(reference) | Self::RemoteReady(reference) => Some(reference),
            Self::None | Self::Drafting(_) | Self::Ready(_) => None,
        }
    }

    pub(crate) fn mark_ready(&mut self) {
        if let Self::Drafting(p) = self {
            *self = Self::Ready(std::mem::take(p));
        } else if let Self::RemoteDrafting(reference) = self {
            *self = Self::RemoteReady(reference.clone());
        }
    }

    pub(crate) fn mark_drafting(&mut self) {
        if let Self::Ready(p) = self {
            *self = Self::Drafting(std::mem::take(p));
        } else if let Self::RemoteReady(reference) = self {
            *self = Self::RemoteDrafting(reference.clone());
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_) | Self::RemoteReady(_))
    }

    /// The session's plan binding, drafting or ready.
    pub(crate) fn target(&self) -> Option<PlanTarget> {
        match self {
            Self::None => None,
            Self::Drafting(path) | Self::Ready(path) => Some(PlanTarget::Local(path.clone())),
            Self::RemoteDrafting(reference) | Self::RemoteReady(reference) => {
                Some(PlanTarget::Remote(reference.clone()))
            }
        }
    }

    pub(crate) fn allocate_path(&mut self, storage: &StateDir, cwd: &Path) {
        if matches!(self, Self::None) {
            *self = Self::Drafting(
                plans::new_plan_path(storage, cwd)
                    .unwrap_or_else(|_| PathBuf::from("plans/plan.md")),
            );
        }
    }

    pub(crate) fn allocate_remote(
        &mut self,
        store: &LocalDocumentStore,
        workspace: &WorkspaceSession,
        session_id: &str,
    ) -> Result<(), String> {
        if matches!(self, Self::None) {
            let reference = store
                .create_plan(workspace.binding().project().key(), session_id)
                .map_err(|error| error.to_string())?;
            *self = Self::RemoteDrafting(reference);
        }
        Ok(())
    }

    pub(crate) fn document_ref(&self) -> Option<LocalDocumentRef> {
        self.reference().cloned().map(LocalDocumentRef::Plan)
    }
}

impl App {
    /// Whether `target` is the plan this session is bound to. A local binding
    /// may be relative to the session's directory.
    fn is_session_plan(&self, target: &PlanTarget) -> bool {
        match target {
            PlanTarget::Local(path) => self.state.plan.path().is_some_and(|bound| {
                bound == path || Path::new(&self.state.session.cwd).join(bound) == *path
            }),
            PlanTarget::Remote(reference) => self.state.plan.reference() == Some(reference),
        }
    }

    /// Takes in the main agent's write of the session plan, bringing an open
    /// tab up to date. A write made while planning also readies the plan, and
    /// the return says so, so the caller can show the plan card. A Build write
    /// leaves readiness alone, and its tool card settles as the document.
    pub(super) fn take_plan_write(&mut self, written: &PlanWriteResult) -> bool {
        if !self.is_session_plan(written.target()) {
            return false;
        }
        self.reload_committed_plan(written);
        let planning = self.execution_agent_mode() == self.agent_mode_for(Mode::Plan);
        if planning {
            self.transition_plan(PlanTrigger::WriteDone);
        }
        planning
    }

    pub(super) fn capture_plan(&self) -> Result<PlanSnapshot, String> {
        if !self.state.plan.is_ready()
            || self.execution_agent_mode() != self.agent_mode_for(Mode::Plan)
        {
            return Err(PLAN_NOT_ACTIVE.into());
        }
        let target = self
            .plan_location()
            .ok_or_else(|| PLAN_NOT_ACTIVE.to_owned())?;
        let content = self.read_plan(&target)?;
        if content.trim().is_empty() {
            return Err(PLAN_EMPTY.into());
        }
        Ok(PlanSnapshot {
            content,
            source: plan_source(&target),
            target,
        })
    }

    /// The session's plan, a local path resolved against the session's
    /// directory.
    fn plan_location(&self) -> Option<PlanTarget> {
        self.state.plan.target().map(|target| match target {
            PlanTarget::Local(path) => {
                PlanTarget::Local(Path::new(&self.state.session.cwd).join(path))
            }
            remote @ PlanTarget::Remote(_) => remote,
        })
    }

    fn plan_store(&self) -> Result<(&WorkspaceSession, &LocalDocumentStore), String> {
        self.workspace_session
            .as_ref()
            .zip(self.local_documents.as_deref())
            .ok_or_else(|| PLAN_STORE_UNAVAILABLE.to_owned())
    }

    fn read_plan(&self, target: &PlanTarget) -> Result<String, String> {
        match target {
            PlanTarget::Local(path) => PlanFile::new(path.clone())
                .and_then(|plan| plan.read())
                .map_err(|error| error.to_string()),
            PlanTarget::Remote(reference) => {
                let (workspace, store) = self.plan_store()?;
                store
                    .validate_binding(workspace.binding())
                    .map_err(|error| error.to_string())?;
                store
                    .read(
                        workspace.binding().project().key(),
                        Some(&self.state.session.id.to_string()),
                        &LocalDocumentRef::Plan(reference.clone()),
                    )
                    .map(|document| document.content)
                    .map_err(|error| error.to_string())
            }
        }
    }

    /// A remote plan document belongs to one session, so a session taking
    /// over another's plan gets a document of its own holding `content`.
    pub(super) fn create_remote_plan(
        &self,
        owner: CaudraId,
        content: &str,
    ) -> Result<PlanRef, String> {
        let (workspace, store) = self.plan_store()?;
        store
            .create_plan_with_content(
                workspace.binding().project().key(),
                &owner.to_string(),
                content,
            )
            .map_err(|error| error.to_string())
    }

    /// The session's plan copied into a new document for `owner`, drafting or
    /// ready as this one is, so a fork revises a plan of its own.
    pub(super) fn copy_plan(&self, owner: CaudraId) -> Result<PlanState, String> {
        let Some(target) = self.plan_location() else {
            return Ok(PlanState::None);
        };
        let content = self.read_plan(&target)?;
        let mut copy = match target {
            PlanTarget::Local(_) => PlanState::Drafting(
                plans::create_plan_file(
                    &self.storage,
                    Path::new(&self.state.session.cwd),
                    &content,
                )
                .map_err(|error| error.to_string())?,
            ),
            PlanTarget::Remote(_) => {
                PlanState::RemoteDrafting(self.create_remote_plan(owner, &content)?)
            }
        };
        if self.state.plan.is_ready() {
            copy.mark_ready();
        }
        Ok(copy)
    }

    pub(crate) fn transition_plan(&mut self, trigger: PlanTrigger) {
        match trigger {
            PlanTrigger::WriteDone => {
                let mode = self.execution_agent_mode();
                if !mode.is_planning()
                    || mode != self.agent_mode_for(Mode::Plan)
                    || self.state.plan.is_ready()
                {
                    return;
                }
                self.state.plan.mark_ready();
                self.plan_form.on_plan_ready();
            }
            PlanTrigger::InteractivePrompt => {
                if self.state.mode == Mode::Plan && self.state.plan.is_ready() {
                    self.state.plan.mark_drafting();
                    self.plan_form.on_plan_drafting();
                }
            }
        }
    }

    /// A remote session's plan lives in the client-owned document store, never
    /// on the host filesystem. A host path left in `PlanState` is announced to
    /// the model as the plan file, which then writes it through the remote
    /// workspace tools, where the path does not exist.
    ///
    /// Session restore cannot decide this itself: the document store is
    /// attached to the app after its state is built, so every path that
    /// rebuilds `SessionState` has to reconcile afterwards.
    pub(crate) fn reconcile_plan_target(&mut self) {
        if self.workspace_session.is_none()
            || self.state.mode != Mode::Plan
            || self.state.plan.reference().is_some()
        {
            return;
        }
        let was_ready = self.state.plan.is_ready();
        let adopted = self
            .state
            .plan
            .path()
            .zip(self.workspace_session.as_ref())
            .zip(self.local_documents.as_ref())
            .and_then(|((path, workspace), store)| {
                store
                    .adopt_legacy_plan(
                        workspace.binding().project().key(),
                        &self.state.session.id.to_string(),
                        path,
                    )
                    .ok()
            });
        self.state.plan = adopted.map_or(PlanState::None, |reference| {
            if was_ready {
                PlanState::RemoteReady(reference)
            } else {
                PlanState::RemoteDrafting(reference)
            }
        });
        if matches!(self.state.plan, PlanState::None) {
            self.enter_plan();
        }
    }

    pub(crate) fn enter_plan(&mut self) {
        if let (Some(store), Some(workspace)) = (&self.local_documents, &self.workspace_session) {
            if let Err(error) = self.state.plan.allocate_remote(
                store,
                workspace,
                &self.state.session.id.to_string(),
            ) {
                self.flash(error);
                return;
            }
        } else {
            let cwd = PathBuf::from(&self.state.session.cwd);
            self.state.plan.allocate_path(&self.storage, &cwd);
        }
        self.state.mode = Mode::Plan;
    }

    pub(super) fn toggle_mode(&mut self) -> Vec<super::Action> {
        match self.state.mode {
            Mode::Build => self.enter_plan(),
            Mode::Plan => self.state.mode = Mode::Build,
        };
        self.remembered_model()
    }

    /// Asks for the model the new mode was last used with, so plan and build
    /// each keep the one they were left on.
    ///
    /// Silent when the Plan job carries a binding: that binding already decides
    /// what a plan run uses, whatever the selection says, so moving the
    /// selection here would only make the status bar name a model the run
    /// ignores.
    fn remembered_model(&self) -> Vec<super::Action> {
        if model_registry::binding(ModelPurpose::Plan).is_some() {
            return vec![];
        }
        caudra_storage::model::read_model(&self.storage, self.state.mode.into())
            .filter(|spec| *spec != self.state.model.spec() && self.model_policy.allows(spec))
            .map(super::Action::ChangeModel)
            .into_iter()
            .collect()
    }

    pub(super) fn agent_mode(&self) -> AgentMode {
        self.agent_mode_for(self.state.mode)
    }

    pub(super) fn agent_mode_for(&self, mode: Mode) -> AgentMode {
        match mode {
            Mode::Plan => match (&self.state.plan.path(), self.state.plan.reference()) {
                (Some(p), _) => AgentMode::Plan((*p).to_path_buf()),
                (_, Some(reference)) => AgentMode::RemotePlan(reference.clone()),
                (None, None) => AgentMode::ReadOnly,
            },
            Mode::Build => AgentMode::Build,
        }
    }

    pub(crate) fn execution_agent_mode(&self) -> AgentMode {
        self.execution_mode.as_ref().map_or_else(
            || self.agent_mode_for(self.state.applied_mode),
            |mode| AgentMode::clone(&mode.load()),
        )
    }

    pub(super) fn sync_execution_mode(&mut self) {
        let Some(mode) = &self.execution_mode else {
            return;
        };
        self.state.applied_mode = if mode.load().is_planning() {
            Mode::Plan
        } else {
            Mode::Build
        };
        if let Some(slot) = &self.effective_model_slot {
            self.state.applied_model = slot.load().model.spec();
        }
    }

    /// Mentions in text the composer did not hand us already resolved: a queue
    /// entry restored from a previous session, where the paths were checked
    /// against a working directory that may since have changed.
    pub(crate) fn scan_mentions(&self, text: &str) -> Vec<Mention> {
        if self.workspace_session.is_some() {
            return mentions::scan_remote(text)
                .into_iter()
                .map(|(_, mention)| mention)
                .collect();
        }
        let root = Path::new(&self.state.session.cwd);
        mentions::scan(text, |path| root.join(path).exists())
            .into_iter()
            .map(|(_, mention)| mention)
            .collect()
    }

    /// Commit references in text the composer did not hand us already resolved.
    /// Validated against the loaded log window, so a queue entry restored into
    /// a different project resolves nothing rather than the wrong revision.
    pub(crate) fn scan_commits(&self, text: &str) -> Vec<CommitRef> {
        let index = self.input_box.commit_index();
        commits::scan(text, |id| index.resolves(id))
            .into_iter()
            .map(|(_, commit)| commit)
            .collect()
    }

    pub(crate) fn build_agent_input(&self, msg: &QueuedMessage) -> AgentInput {
        AgentInput {
            message: msg.text.clone(),
            mode: self.agent_mode(),
            plan: self.state.plan.target(),
            images: msg.images.clone(),
            mentions: msg.mentions.clone(),
            commits: msg.commits.clone(),
            preamble: Vec::new(),
            thinking: self.state.thinking.clone(),
            fast: self.state.fast,
            prompt: None,
            resume: false,
        }
    }

    pub(super) fn continuation_input(&self) -> AgentInput {
        AgentInput {
            message: String::new(),
            mode: self.execution_agent_mode(),
            plan: self.state.plan.target(),
            images: Vec::new(),
            mentions: Vec::new(),
            commits: Vec::new(),
            preamble: Vec::new(),
            thinking: self.state.thinking.clone(),
            fast: self.state.fast,
            prompt: None,
            resume: false,
        }
    }

    /// A toggle does not reach the agent until the next message carries it, so
    /// a mode that has been switched but not yet handed over reads as a
    /// transition rather than as an accomplished fact.
    pub(super) fn mode_label(&self) -> ModeLabel {
        let (full, short) = if self.is_bash_input() {
            (BASH_LABEL, BASH_SHORT_LABEL)
        } else {
            let applied = if self.execution_agent_mode().is_planning() {
                Mode::Plan
            } else {
                Mode::Build
            };
            match (applied, self.state.mode) {
                (Mode::Build, Mode::Build) => (BUILD_LABEL, BUILD_SHORT_LABEL),
                (Mode::Plan, Mode::Plan) => (PLAN_LABEL, PLAN_SHORT_LABEL),
                (Mode::Build, Mode::Plan) => (TO_PLAN_LABEL, TO_PLAN_SHORT_LABEL),
                (Mode::Plan, Mode::Build) => (TO_BUILD_LABEL, TO_BUILD_SHORT_LABEL),
            }
        };
        ModeLabel {
            full: full.into(),
            short: short.into(),
            style: Style::new()
                .fg(self.effective_mode_color())
                .add_modifier(Modifier::BOLD),
        }
    }

    pub(crate) fn is_bash_input(&self) -> bool {
        self.input_box.buffer.starts_with_shell_prefix()
    }

    pub(super) fn effective_mode_color(&self) -> Color {
        if self.is_bash_input() {
            theme::current().mode_bash
        } else {
            self.state.mode.color()
        }
    }

    pub(super) fn separator_style(&self) -> Style {
        if self.status == Status::Streaming {
            theme::current().input_border
        } else {
            Style::new().fg(self.effective_mode_color())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use caudra_agent::tools::native::plan::{self, PlanTarget, PlanWriteResult};
    use caudra_agent::{AgentEvent, PromptAdmission, ToolDoneEvent};
    use caudra_providers::{ImageMediaType, ImageSource, Message};
    use caudra_storage::id::CaudraId;
    use caudra_storage::sessions::StoredMode;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, ProjectIdentity, ProjectKey,
        ResourceId, ResourceScope, SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor,
        WorkspaceCapabilities, WorkspaceCursor, WorkspaceHandle, WorkspaceServices,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    use super::{
        AgentMode, App, LocalDocumentStore, Mode, PathBuf, PlanRef, PlanState, PlanTrigger,
        WorkspaceSession,
    };
    use crate::agent::shared_queue::{self, QueueItem, QueuedMessage};
    use crate::app::queue::{EMPTY_PROMPT_ERR, SubmitOutcome};
    use crate::app::tests::{agent_msg, test_app};
    use crate::components::mode_submission::{ModeSubmissionAction, ModeSubmissionChoice};
    use crate::components::{Action, Status};

    const HOST_PLAN: &str = "/home/someone/.caudra/plans/woolly-singing-puppy.md";
    const OTHER_HOST_PLAN: &str = "/home/someone/.caudra/plans/other.md";
    const REMOTE_PLAN: &str = "plan-current";
    const OTHER_REMOTE_PLAN: &str = "plan-other";
    const PLAN_WRITE_ID: &str = "plan-write";
    const PLAN_WRITTEN: &str = "wrote plan";
    const RUN_ID: u64 = 1;
    const PROMPT: &str = "Review the implementation in Plan";
    const NEXT_PROMPT: &str = "Keep this queued work";
    const GOAL: &str = "Existing goal remains active";
    const PROPOSED_GOAL: &str = "Review the design before changing files";
    const MAILBOX_RESULT: &str = "Background work completed";
    const RICH_PASTE: &str = "first pasted line\nsecond pasted line\nthird pasted line";
    const IMAGE_DATA: &str = "dGVzdA==";
    const EXPECTED_SEND: &str = "expected one automatic continuation";
    const EXPECTED_COMPOSED_SEND: &str = "expected the composed prompt to start a run";
    const EXPECTED_QUEUED_INPUT: &str = "expected the submitted Plan input in the queue";

    fn app_in_mode(mode: Mode) -> App {
        let mut app = test_app();
        app.state.plan = PlanState::Drafting(PathBuf::from(HOST_PLAN));
        app.state.mode = mode;
        app.state.applied_mode = mode;
        app.execution_mode = Some(Arc::new(ArcSwap::from_pointee(app.agent_mode())));
        app.status = Status::Streaming;
        app.run_id = RUN_ID;
        app
    }

    fn queued_message(text: &str) -> QueuedMessage {
        QueuedMessage {
            text: text.into(),
            images: Vec::new(),
            mentions: Vec::new(),
            commits: Vec::new(),
            paste_ranges: Vec::new(),
        }
    }

    fn drafting_plan(remote: bool) -> PlanState {
        if remote {
            PlanState::RemoteDrafting(PlanRef::new(REMOTE_PLAN).unwrap())
        } else {
            PlanState::Drafting(PathBuf::from(HOST_PLAN))
        }
    }

    fn plan_write_done(plan: &PlanState) -> AgentEvent {
        let mut event = ToolDoneEvent::error(PLAN_WRITE_ID.into(), PLAN_WRITTEN);
        event.is_error = false;
        event.tool = plan::NAME.into();
        let target = match plan.reference() {
            Some(reference) => PlanTarget::Remote(reference.clone()),
            None => PlanTarget::Local(plan.path().unwrap().to_path_buf()),
        };
        event.annotation = Some(
            PlanWriteResult::new(target, PLAN_WRITTEN.into())
                .annotation()
                .unwrap(),
        );
        AgentEvent::ToolDone(Box::new(event))
    }

    fn workspace() -> WorkspaceSession {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("origin").expect("anchor"),
            "server",
            "workspace",
            "generation",
            "namespace",
        )
        .expect("authority");
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("binding").expect("binding id"),
            authority.clone(),
            AuthenticatedPrincipalId::new(authority.clone(), "principal").expect("principal"),
            ProjectIdentity::new(
                authority.clone(),
                ProjectKey::new("project").expect("project"),
            ),
        )
        .expect("binding");
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").expect("root")),
            1,
            CwdHandle::new("cwd").expect("cwd"),
        );
        let handle = WorkspaceHandle::new(
            authority,
            WorkspaceCapabilities::new([]),
            WorkspaceServices::default(),
        )
        .expect("handle");
        WorkspaceSession::new(handle, binding, cursor).expect("workspace")
    }

    /// A restored host path would be announced to the model as the plan file,
    /// which then writes it through the remote workspace tools, where it does
    /// not exist.
    #[test_case(PlanState::Drafting(PathBuf::from(HOST_PLAN)) ; "drafting_host_path")]
    #[test_case(PlanState::Ready(PathBuf::from(HOST_PLAN)) ; "ready_host_path")]
    #[test_case(PlanState::None ; "no_target_at_all")]
    fn a_remote_plan_session_never_keeps_a_host_plan_path(restored: PlanState) {
        let mut app = test_app();
        let owner = workspace();
        app.local_documents = Some(Arc::new(LocalDocumentStore::remote(
            app.storage.clone(),
            owner.binding(),
        )));
        app.workspace_session = Some(owner);
        app.state.mode = Mode::Plan;
        app.state.plan = restored;

        app.reconcile_plan_target();

        assert!(app.state.plan.path().is_none());
        assert!(app.state.plan.reference().is_some());
        assert!(matches!(app.agent_mode(), AgentMode::RemotePlan(_)));
    }

    /// A plan document is filed under a session id, so allocating one before
    /// the replacement session is installed hands it to the session being
    /// retired, and every later read looks for it under the wrong id.
    #[test]
    fn a_reset_remote_session_files_its_plan_under_the_replacement() {
        let mut app = test_app();
        let owner = workspace();
        let store = Arc::new(LocalDocumentStore::remote(
            app.storage.clone(),
            owner.binding(),
        ));
        app.local_documents = Some(Arc::clone(&store));
        app.workspace_session = Some(owner);
        app.state.mode = Mode::Plan;
        let retired = app.state.session.id.to_string();

        app.reset_session();

        let reference = app.state.plan.document_ref().expect("remote plan");
        let project = store.project_key();
        let owner_id = app.state.session.id.to_string();
        assert_ne!(owner_id, retired);
        assert!(store.read(project, Some(&owner_id), &reference).is_ok());
        assert!(store.read(project, Some(&retired), &reference).is_err());
    }

    #[test]
    fn a_local_plan_session_keeps_its_host_plan_path() {
        let mut app = test_app();
        let plan = PathBuf::from(HOST_PLAN);
        app.state.mode = Mode::Plan;
        app.state.plan = PlanState::Drafting(plan.clone());

        app.reconcile_plan_target();

        assert_eq!(app.state.plan, PlanState::Drafting(plan));
    }

    #[test_case(false, Mode::Plan, true; "local_active_plan_selected_build")]
    #[test_case(true, Mode::Plan, true; "remote_active_plan_selected_build")]
    #[test_case(false, Mode::Build, true; "local_active_build_selected_plan")]
    #[test_case(true, Mode::Build, true; "remote_active_build_selected_plan")]
    #[test_case(false, Mode::Plan, false; "local_applied_plan_selected_build")]
    #[test_case(true, Mode::Plan, false; "remote_applied_plan_selected_build")]
    #[test_case(false, Mode::Build, false; "local_applied_build_selected_plan")]
    #[test_case(true, Mode::Build, false; "remote_applied_build_selected_plan")]
    fn plan_write_completion_follows_execution_after_toggling(
        remote: bool,
        execution: Mode,
        shared: bool,
    ) {
        let mut app = app_in_mode(execution);
        app.state.plan = drafting_plan(remote);
        let mode = app.agent_mode();
        app.execution_mode = shared.then(|| Arc::new(ArcSwap::from_pointee(mode.clone())));
        app.toggle_mode();
        let selected = app.state.mode;
        if shared {
            app.state.applied_mode = selected;
        }
        let mut expected = app.state.plan.clone();
        let write = plan_write_done(&expected);
        if execution == Mode::Plan {
            expected.mark_ready();
        }

        app.update(agent_msg(write));

        assert_eq!(app.state.plan, expected);
        assert_eq!(app.plan_form.is_visible(), execution == Mode::Plan);
        assert_eq!(app.state.mode, selected);
        assert_eq!(app.execution_agent_mode(), mode);
        assert_eq!(
            app.main_chat().last_message_is_plan(),
            execution == Mode::Plan
        );
        assert!(!app.plan_form_active());
        if execution == Mode::Plan {
            app.toggle_mode();
            assert!(app.plan_form_active());
            app.plan_form.hide();
            app.update(agent_msg(plan_write_done(&expected)));
            assert_eq!(app.state.plan, expected);
            assert!(!app.plan_form.is_visible());
        }
    }

    #[test_case(false, false; "local_write_to_selected_target")]
    #[test_case(false, true; "local_write_to_execution_target")]
    #[test_case(true, false; "remote_write_to_selected_target")]
    #[test_case(true, true; "remote_write_to_execution_target")]
    fn plan_write_cannot_complete_a_different_target(remote: bool, writes_execution: bool) {
        let mut app = app_in_mode(Mode::Plan);
        app.state.plan = drafting_plan(remote);
        app.execution_mode = Some(Arc::new(ArcSwap::from_pointee(app.agent_mode())));
        let executing_plan = app.state.plan.clone();
        let selected_plan = if remote {
            PlanState::RemoteDrafting(PlanRef::new(OTHER_REMOTE_PLAN).unwrap())
        } else {
            PlanState::Drafting(PathBuf::from(OTHER_HOST_PLAN))
        };
        app.state.plan = selected_plan.clone();
        let write = plan_write_done(if writes_execution {
            &executing_plan
        } else {
            &selected_plan
        });

        app.update(agent_msg(write));

        assert_eq!(app.state.plan, selected_plan);
        assert!(!app.plan_form.is_visible());
        assert!(!app.main_chat().last_message_is_plan());

        app.transition_plan(PlanTrigger::WriteDone);

        assert_eq!(app.state.plan, selected_plan);
        assert!(!app.plan_form.is_visible());
    }

    #[test_case(AgentMode::Build; "build")]
    #[test_case(AgentMode::ReadOnly; "read_only")]
    fn non_plan_execution_cannot_complete_a_selected_plan(mode: AgentMode) {
        let mut app = app_in_mode(Mode::Plan);
        app.execution_mode = Some(Arc::new(ArcSwap::from_pointee(mode)));
        let expected = app.state.plan.clone();

        app.transition_plan(PlanTrigger::WriteDone);

        assert_eq!(app.state.plan, expected);
        assert!(!app.plan_form.is_visible());

        app.update(agent_msg(plan_write_done(&expected)));

        assert_eq!(app.state.plan, expected);
        assert!(!app.plan_form.is_visible());
    }

    #[test_case(Mode::Build, Mode::Plan; "build_execution_selected_plan")]
    #[test_case(Mode::Plan, Mode::Build; "plan_execution_selected_build")]
    #[test_case(Mode::Plan, Mode::Plan; "plan_execution_selected_plan")]
    #[test_case(Mode::Build, Mode::Build; "build_execution_selected_build")]
    fn interactive_plan_prompt_follows_selection(execution: Mode, selected: Mode) {
        let mut app = app_in_mode(execution);
        app.state.mode = selected;
        app.state.plan.mark_ready();
        app.plan_form.on_plan_ready();

        app.transition_plan(PlanTrigger::InteractivePrompt);

        assert_eq!(app.state.plan.is_ready(), selected == Mode::Build);
        assert_eq!(app.plan_form.is_visible(), selected == Mode::Build);
        assert_eq!(app.state.mode, selected);
        assert_eq!(app.execution_agent_mode(), app.agent_mode_for(execution));
    }

    #[test_case(Mode::Build, false; "build_to_plan")]
    #[test_case(Mode::Plan, false; "plan_to_build")]
    #[test_case(Mode::Build, true; "suppressed_build_to_plan")]
    #[test_case(Mode::Plan, true; "suppressed_plan_to_build")]
    fn toggling_and_preparing_input_do_not_change_execution(mode: Mode, suppressed: bool) {
        let mut app = app_in_mode(mode);
        app.automatic_wakes_suppressed = suppressed;
        let execution = app.execution_agent_mode();
        let epoch = app.background_delivery.fence.epoch();
        let jobs = app.background_delivery.jobs_started();
        let applied_model = app.state.applied_model.clone();

        let actions = app.toggle_mode();

        assert!(
            actions
                .iter()
                .all(|action| matches!(action, Action::ChangeModel(_)))
        );
        assert_ne!(app.state.mode, mode);
        assert_eq!(app.state.applied_mode, mode);
        assert_eq!(app.execution_agent_mode(), execution);
        assert_eq!(app.automatic_wakes_suppressed, suppressed);
        assert_eq!(app.background_delivery.fence.epoch(), epoch);

        let input = app.build_agent_input(&queued_message(PROMPT));

        assert_eq!(input.mode, app.agent_mode());
        assert_ne!(input.mode, execution);
        assert_eq!(app.state.applied_mode, mode);
        assert_eq!(app.state.applied_model, applied_model);
        assert_eq!(app.execution_agent_mode(), execution);
        assert_eq!(app.automatic_wakes_suppressed, suppressed);
        assert_eq!(app.background_delivery.fence.epoch(), epoch);
        assert_eq!(app.background_delivery.jobs_started(), jobs);
        assert_eq!(app.run_id, RUN_ID);
        assert!(app.cancelling_run.is_none());
        assert!(app.queue.is_empty());
        assert!(!app.mode_submission.is_open());
    }

    #[test_case(Mode::Build, false, false; "mailbox_applied_build")]
    #[test_case(Mode::Plan, false, false; "mailbox_applied_plan")]
    #[test_case(Mode::Build, true, false; "mailbox_actual_build")]
    #[test_case(Mode::Plan, true, false; "mailbox_actual_plan")]
    #[test_case(Mode::Build, false, true; "goal_applied_build")]
    #[test_case(Mode::Plan, false, true; "goal_applied_plan")]
    #[test_case(Mode::Build, true, true; "goal_actual_build")]
    #[test_case(Mode::Plan, true, true; "goal_actual_plan")]
    fn automatic_continuations_keep_the_execution_mode(mode: Mode, shared: bool, goal: bool) {
        let mut app = app_in_mode(mode);
        let expected = app.execution_agent_mode();
        app.toggle_mode();
        if shared {
            app.state.applied_mode = app.state.mode;
        } else {
            app.execution_mode = None;
        }
        app.status = Status::Idle;
        assert_eq!(app.continuation_input().mode, expected);

        let actions = if goal {
            app.state.goal.set(GOAL).unwrap();
            app.start_goal_checkin()
        } else {
            app.start_mailbox_run(vec![Message::synthetic(MAILBOX_RESULT.into())])
        };

        let [Action::SendMessage(input)] = actions.as_slice() else {
            panic!("{EXPECTED_SEND}");
        };
        assert_eq!(input.mode, expected);
        assert!(input.message.is_empty());
        assert!(input.preamble.iter().any(|message| {
            message
                .first_text_content()
                .is_some_and(|text| text.contains(if goal { GOAL } else { MAILBOX_RESULT }))
        }));
        assert!(!app.mode_submission.is_open());
    }

    /// Composed and automatic runs alike name the session's plan, so a Build
    /// run can still read and revise the plan the session drew up.
    #[test_case(false; "local")]
    #[test_case(true; "remote")]
    fn build_runs_carry_the_session_plan(remote: bool) {
        let mut app = app_in_mode(Mode::Build);
        app.state.plan = drafting_plan(remote);
        app.status = Status::Idle;
        let bound = app.state.plan.target();
        assert!(bound.is_some());

        let actions = app.submit_or_queue(queued_message(PROMPT));
        let [Action::SendMessage(composed)] = actions.as_slice() else {
            panic!("{EXPECTED_COMPOSED_SEND}");
        };
        assert_eq!(composed.mode, AgentMode::Build);
        assert_eq!(composed.plan, bound);

        app.status = Status::Idle;
        let actions = app.start_mailbox_run(vec![Message::synthetic(MAILBOX_RESULT.into())]);
        let [Action::SendMessage(automatic)] = actions.as_slice() else {
            panic!("{EXPECTED_SEND}");
        };
        assert_eq!(automatic.mode, AgentMode::Build);
        assert_eq!(automatic.plan, bound);
    }

    #[test_case(Mode::Build, Mode::Plan, Status::Streaming, true; "active_build_to_plan")]
    #[test_case(Mode::Build, Mode::Plan, Status::Idle, false; "idle_build_to_plan")]
    #[test_case(Mode::Build, Mode::Build, Status::Streaming, false; "same_build_mode")]
    #[test_case(Mode::Plan, Mode::Plan, Status::Streaming, false; "same_plan_mode")]
    #[test_case(Mode::Plan, Mode::Build, Status::Streaming, false; "plan_to_build")]
    fn only_conflicting_build_submission_opens_the_picker(
        execution: Mode,
        selected: Mode,
        status: Status,
        conflict: bool,
    ) {
        let mut app = app_in_mode(execution);
        app.state.mode = selected;
        app.status = status;
        let streaming = app.status == Status::Streaming;

        let actions = app.submit_or_queue(queued_message(PROMPT));

        assert_eq!(app.mode_submission.is_open(), conflict);
        assert_eq!(app.pending_plan_submission.is_some(), conflict);
        if conflict {
            assert!(actions.is_empty());
            assert!(app.queue.is_empty());
            assert_eq!(app.run_id, RUN_ID);
            assert!(!app.automatic_wakes_suppressed);
        } else if streaming {
            assert!(actions.is_empty());
            assert_eq!(app.queue.len(), 1);
        } else {
            assert!(matches!(actions.as_slice(), [Action::SendMessage(_)]));
        }
    }

    #[test_case(Mode::Build, true; "build_results_still_processing")]
    #[test_case(Mode::Plan, false; "plan_results_still_processing")]
    fn processing_results_count_as_conflicting_build_work(execution: Mode, conflict: bool) {
        let mut app = app_in_mode(execution);
        let (sender, receiver) = shared_queue::queue();
        app.queue.set_shared(sender);
        assert!(app.queue_and_notify(queued_message(NEXT_PROMPT)));
        assert_eq!(receiver.claim_idle(RUN_ID).len(), 1);
        assert!(app.queue.is_processing());
        app.status = Status::Idle;
        app.state.mode = Mode::Plan;

        assert!(app.submit_or_queue(queued_message(PROMPT)).is_empty());

        assert_eq!(app.mode_submission.is_open(), conflict);
        assert_eq!(app.pending_plan_submission.is_some(), conflict);
        assert_eq!(app.queue.is_empty(), conflict);
        assert_eq!(app.run_id, RUN_ID);
    }

    #[test_case(KeyCode::Enter; "keep_editing")]
    #[test_case(KeyCode::Esc; "escape")]
    fn dismissing_the_choice_preserves_the_rich_composer_draft(key: KeyCode) {
        let mut app = app_in_mode(Mode::Build);
        app.toggle_mode();
        app.input_box.buffer.insert_text("  review ");
        app.input_box.buffer.insert_paste(RICH_PASTE);
        app.input_box.buffer.insert_text("  ");
        let image = ImageSource::new(ImageMediaType::Png, Arc::from(IMAGE_DATA));
        app.input_box.attach_image(image.clone());
        let draft = app.input_box.draft();
        let display = app.input_box.buffer.display_text();
        let epoch = app.background_delivery.fence.epoch();
        let submission = app.input_box.take_submission().unwrap();

        assert!(app.handle_submit(submission).is_empty());
        assert!(app.mode_submission.is_open());
        assert_eq!(app.input_box.draft(), draft);
        let action = app
            .mode_submission
            .handle_key(KeyEvent::new(key, KeyModifiers::NONE));
        assert!(app.handle_mode_submission(action).is_empty());

        assert_eq!(app.input_box.draft(), draft);
        assert_eq!(app.input_box.buffer.display_text(), display);
        assert_eq!(app.input_box.pending_images(), [image]);
        assert!(!app.mode_submission.is_open());
        assert!(app.pending_plan_submission.is_none());
        assert!(app.queue.is_empty());
        assert_eq!(app.run_id, RUN_ID);
        assert_eq!(app.status, Status::Streaming);
        assert_eq!(app.background_delivery.fence.epoch(), epoch);
        assert!(!app.automatic_wakes_suppressed);
    }

    #[test_case(PromptAdmission::Queue; "original_queue_request")]
    #[test_case(PromptAdmission::Steer; "original_guide_request")]
    fn queued_plan_input_does_not_follow_later_mode_selection(admission: PromptAdmission) {
        let mut app = app_in_mode(Mode::Build);
        app.toggle_mode();
        assert!(
            app.submit_or_queue_with_admission(queued_message(PROMPT), admission)
                .is_empty()
        );
        assert!(app.mode_submission.is_open());

        assert!(
            app.handle_mode_submission(ModeSubmissionAction::Select(ModeSubmissionChoice::Queue))
                .is_empty()
        );
        app.toggle_mode();

        assert_eq!(app.state.mode, Mode::Build);
        assert_eq!(app.execution_agent_mode(), AgentMode::Build);
        let prompts = app.queue.pending_prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].text, PROMPT);
        assert_eq!(prompts[0].mode, Some(StoredMode::Plan));
        assert_eq!(prompts[0].admission, PromptAdmission::Queue);
        let id = app.queue.panel_entries()[0].id;
        let Some(QueueItem::Message { input, .. }) = app.queue.take_id(id) else {
            panic!("{EXPECTED_QUEUED_INPUT}");
        };
        assert_eq!(input.mode, AgentMode::Plan(PathBuf::from(HOST_PLAN)));
        assert_eq!(app.run_id, RUN_ID);
        assert!(!app.mode_submission.is_open());
        assert!(app.pending_plan_submission.is_none());
        assert!(app.input_box.is_empty());
    }

    #[test_case(true; "active_agent")]
    #[test_case(false; "startup_not_claimed")]
    fn stop_replaces_work_without_dropping_the_next_queue(active: bool) {
        let mut app = app_in_mode(Mode::Build);
        let (sender, receiver) = shared_queue::queue();
        app.queue.set_shared(sender);
        if active {
            receiver.set_active_run(RUN_ID);
        }
        assert!(app.queue_and_notify(queued_message(NEXT_PROMPT)));
        let next_id = app.queue.panel_entries()[0].id;
        app.toggle_mode();
        assert!(app.submit_or_queue(queued_message(PROMPT)).is_empty());

        let actions =
            app.handle_mode_submission(ModeSubmissionAction::Select(ModeSubmissionChoice::Stop));

        assert!(matches!(
            actions.as_slice(),
            [Action::CancelAgent { run_id: RUN_ID }]
        ));
        assert_eq!(app.run_id, RUN_ID + 1);
        assert_eq!(app.cancelling_run, active.then_some(RUN_ID));
        assert!(app.automatic_wakes_suppressed);
        assert!(app.replacement_item.is_some());
        assert!(
            app.queue
                .panel_entries()
                .iter()
                .any(|entry| entry.id == next_id)
        );
        let prompts = app.queue.pending_prompts();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0].text, NEXT_PROMPT);
        assert_eq!(prompts[0].admission, PromptAdmission::Queue);
        assert_eq!(prompts[0].mode, Some(StoredMode::Build));
        assert_eq!(prompts[1].text, PROMPT);
        assert_eq!(prompts[1].admission, PromptAdmission::Interrupt);
        assert_eq!(prompts[1].mode, Some(StoredMode::Plan));
        assert!(!app.mode_submission.is_open());
        assert!(app.pending_plan_submission.is_none());
    }

    #[test_case("", true; "empty_prompt")]
    #[test_case(" \n ", true; "whitespace_prompt")]
    #[test_case(PROMPT, false; "missing_plan_target")]
    fn invalid_submission_does_not_offer_a_mode_decision(text: &str, valid_target: bool) {
        let mut app = app_in_mode(Mode::Build);
        app.state.mode = Mode::Plan;
        if !valid_target {
            app.state.plan = PlanState::None;
        }

        assert!(app.submit_or_queue(queued_message(text)).is_empty());

        if valid_target {
            assert_eq!(app.status_bar.flash_text(), Some(EMPTY_PROMPT_ERR));
        } else {
            assert!(app.status_bar.flash_text().is_some());
        }
        assert!(!app.mode_submission.is_open());
        assert!(app.pending_plan_submission.is_none());
        assert!(app.queue.is_empty());
        assert_eq!(app.run_id, RUN_ID);
        assert!(!app.automatic_wakes_suppressed);
    }

    #[test_case(ModeSubmissionChoice::Queue, false; "queue_local_target")]
    #[test_case(ModeSubmissionChoice::Stop, false; "stop_local_target")]
    #[test_case(ModeSubmissionChoice::Queue, true; "queue_remote_target")]
    #[test_case(ModeSubmissionChoice::Stop, true; "stop_remote_target")]
    fn changed_plan_target_rejects_a_pending_choice(choice: ModeSubmissionChoice, remote: bool) {
        let mut app = app_in_mode(Mode::Build);
        app.state.mode = Mode::Plan;
        app.state.plan = drafting_plan(remote);
        app.submit_or_queue(queued_message(PROMPT));
        assert!(app.mode_submission.is_open());
        let draft = app.input_box.draft();
        let epoch = app.background_delivery.fence.epoch();
        app.state.plan = if remote {
            PlanState::RemoteDrafting(PlanRef::new(OTHER_REMOTE_PLAN).unwrap())
        } else {
            PlanState::Drafting(OTHER_HOST_PLAN.into())
        };

        assert!(
            app.handle_mode_submission(ModeSubmissionAction::Select(choice))
                .is_empty()
        );

        assert!(app.status_bar.flash_text().is_some());
        assert_eq!(app.input_box.draft(), draft);
        assert_eq!(app.background_delivery.fence.epoch(), epoch);
        assert!(app.queue.is_empty());
        assert!(!app.automatic_wakes_suppressed);
    }

    #[test_case(ModeSubmissionChoice::Queue; "queue")]
    #[test_case(ModeSubmissionChoice::Stop; "stop")]
    fn stale_session_choice_cannot_submit(choice: ModeSubmissionChoice) {
        let mut app = app_in_mode(Mode::Build);
        app.toggle_mode();
        app.submit_or_queue(queued_message(PROMPT));
        assert!(app.mode_submission.is_open());
        app.state.session_mut().id = CaudraId::generate();

        assert!(
            app.handle_mode_submission(ModeSubmissionAction::Select(choice))
                .is_empty()
        );

        assert!(app.queue.is_empty());
        assert_eq!(app.run_id, RUN_ID);
        assert!(!app.automatic_wakes_suppressed);
    }

    #[test_case(ModeSubmissionChoice::Queue; "queue")]
    #[test_case(ModeSubmissionChoice::Stop; "stop")]
    fn rejected_goal_choice_keeps_the_existing_goal(choice: ModeSubmissionChoice) {
        let mut app = app_in_mode(Mode::Build);
        app.state.goal.set(GOAL).unwrap();
        let before = app.state.goal.snapshot().unwrap();
        app.toggle_mode();
        app.submit_goal(PROPOSED_GOAL);
        assert!(app.mode_submission.is_open());
        app.queue.disconnect();

        assert!(
            app.handle_mode_submission(ModeSubmissionAction::Select(choice))
                .is_empty()
        );

        let after = app.state.goal.snapshot().unwrap();
        assert_eq!(after.condition, before.condition);
        assert_eq!(after.started_at, before.started_at);
        assert_eq!(
            app.input_box.expanded_text(),
            format!("/goal {PROPOSED_GOAL}")
        );
    }

    #[test_case(ModeSubmissionAction::Select(ModeSubmissionChoice::KeepEditing); "keep_goal")]
    #[test_case(ModeSubmissionAction::Close; "escape_goal")]
    fn declining_a_goal_submission_does_not_replace_the_existing_goal(
        action: ModeSubmissionAction,
    ) {
        let mut app = app_in_mode(Mode::Build);
        app.state.goal.set(GOAL).unwrap();
        let before = app.state.goal.snapshot().unwrap();
        app.toggle_mode();
        let command = format!("/goal {PROPOSED_GOAL}");

        assert!(app.run_cmdline(&command, 0).unwrap().is_empty());
        assert!(app.mode_submission.is_open());
        assert_eq!(app.state.goal.snapshot().unwrap().condition.as_ref(), GOAL);
        assert!(app.handle_mode_submission(action).is_empty());

        let after = app.state.goal.snapshot().unwrap();
        assert_eq!(after.condition, before.condition);
        assert_eq!(after.started_at, before.started_at);
        assert_eq!(after.evaluations, before.evaluations);
        assert_eq!(after.usage, before.usage);
        assert_eq!(app.input_box.expanded_text(), command);
        assert!(app.queue.is_empty());
        assert_eq!(app.run_id, RUN_ID);
        assert!(!app.mode_submission.is_open());
        assert!(app.pending_plan_submission.is_none());
    }

    #[test_case(PromptAdmission::Queue; "queue")]
    #[test_case(PromptAdmission::Steer; "guide")]
    #[test_case(PromptAdmission::Interrupt; "interrupt")]
    fn programmatic_submission_returns_a_decision_without_executing(admission: PromptAdmission) {
        let mut app = app_in_mode(Mode::Build);
        app.toggle_mode();
        app.input_box.set_input(NEXT_PROMPT.into());
        let epoch = app.background_delivery.fence.epoch();

        let outcome = app.submit_prompt_with_admission(queued_message(PROMPT), admission);

        assert!(matches!(outcome, SubmitOutcome::NeedsModeDecision(_)));
        assert!(!app.mode_submission.is_open());
        assert!(app.pending_plan_submission.is_none());
        assert_eq!(app.input_box.expanded_text(), NEXT_PROMPT);
        assert!(app.queue.is_empty());
        assert_eq!(app.run_id, RUN_ID);
        assert!(app.cancelling_run.is_none());
        assert_eq!(app.status, Status::Streaming);
        assert_eq!(app.state.applied_mode, Mode::Build);
        assert_eq!(app.execution_agent_mode(), AgentMode::Build);
        assert_eq!(app.background_delivery.fence.epoch(), epoch);
        assert!(!app.automatic_wakes_suppressed);
    }
}
