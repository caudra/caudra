use std::path::{Path, PathBuf};

use crate::agent::QueuedMessage;
use crate::components::Status;
use crate::components::status_bar::ModeLabel;
use crate::theme;
use caudra_agent::mentions;
use caudra_agent::{AgentInput, AgentMode, CommitRef, Mention, commits};
use caudra_providers::ModelPurpose;
use caudra_providers::model_registry;
use caudra_storage::StateDir;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::plans;
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
    pub(crate) fn transition_plan(&mut self, trigger: PlanTrigger) {
        if self.state.mode != Mode::Plan {
            return;
        }
        match trigger {
            PlanTrigger::WriteDone => {
                if self.state.plan.is_ready() {
                    return;
                }
                self.state.plan.mark_ready();
                self.plan_form.on_plan_ready();
            }
            PlanTrigger::InteractivePrompt => {
                if self.state.plan.is_ready() {
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
        let transition = match self
            .background
            .as_ref()
            .map(|background| background.suspend())
            .transpose()
        {
            Ok(transition) => transition,
            Err(error) => {
                self.flash(error);
                return;
            }
        };
        if self.background.is_some() || self.has_session_work() {
            self.automatic_wakes_suppressed = true;
            self.release_background_claims();
            if let Some(background) = &self.background
                && let Err(error) = smol::block_on(background.stop())
            {
                self.flash(error);
                return;
            }
            if let Err(error) = self.workflow.stop_all() {
                self.flash(error);
                return;
            }
            if let Some(transition) = &transition
                && let Err(error) = smol::block_on(transition.drain())
            {
                self.flash(error);
                return;
            }
        }
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
        match self.state.mode {
            Mode::Plan => match (&self.state.plan.path(), self.state.plan.reference()) {
                (Some(p), _) => AgentMode::Plan((*p).to_path_buf()),
                (_, Some(reference)) => AgentMode::RemotePlan(reference.clone()),
                (None, None) => {
                    debug_assert!(false, "Plan mode without path - invariant violated");
                    AgentMode::Build
                }
            },
            Mode::Build => AgentMode::Build,
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

    /// The one place the mode is committed to the agent, so it is also where a
    /// pending toggle stops being pending. The model the toggle swapped in
    /// settles with it, having reached the agent by the same message.
    pub(crate) fn build_agent_input(&mut self, msg: &QueuedMessage) -> AgentInput {
        self.state.applied_mode = self.state.mode;
        self.state.applied_model = self.state.model.spec();
        AgentInput {
            message: msg.text.clone(),
            mode: self.agent_mode(),
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

    /// A toggle does not reach the agent until the next message carries it, so
    /// a mode that has been switched but not yet handed over reads as a
    /// transition rather than as an accomplished fact.
    pub(super) fn mode_label(&self) -> ModeLabel {
        let (full, short) = if self.is_bash_input() {
            (BASH_LABEL, BASH_SHORT_LABEL)
        } else {
            match (self.state.applied_mode, self.state.mode) {
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

    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, ProjectIdentity, ProjectKey,
        ResourceId, ResourceScope, SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor,
        WorkspaceCapabilities, WorkspaceCursor, WorkspaceHandle, WorkspaceServices,
    };
    use test_case::test_case;

    use super::{AgentMode, LocalDocumentStore, Mode, PathBuf, PlanState, WorkspaceSession};
    use crate::app::tests::test_app;

    const HOST_PLAN: &str = "/home/someone/.caudra/plans/woolly-singing-puppy.md";

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
}
