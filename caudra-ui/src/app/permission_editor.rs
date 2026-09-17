use arc_swap::ArcSwap;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use caudra_agent::AgentMode;
use caudra_agent::permissions::editor::{
    GuardDraft, PermissionEditError, PermissionEditEvidence, PermissionEditOperation,
    PermissionEditPreview, PermissionEditSession, PermissionMatchPreview, PermissionPublication,
    PermissionRuleDraft, ResourceDraft, ResourcesDraft, SelectorDraft, SelectorValue,
    TemplateSource,
};
use caudra_agent::permissions::{
    ActivePolicyRule, PermissionAnswer, PermissionManager, PermissionProjectFilter,
    PermissionResourceAccess, PermissionResourceKind, PermissionRuleRecord,
    VerifiedLocalSourceLocator,
};
use caudra_agent::tools::ToolFilter;
use caudra_config::PermissionReviewCandidate;
use caudra_providers::Model;
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::permission_patterns::PatternDefinition;
use caudra_storage::permission_state::mutation::{
    PermissionCommitReceipt, PermissionMutationError, PermissionOwner, PermissionSnapshot,
    PreparedPermissionMutation,
};
use caudra_storage::sessions::SessionDatabase;
use caudra_workbench::Workbench;
use caudra_workspace::WorkspaceSession;
use flume::{Receiver, TryRecvError};

use crate::components::Overlay;
use crate::components::permission_prompt::PermissionDecision;
use crate::components::permission_scope::editor::{EditorEvent, EditorLaunch, ScopeEditor};
use crate::components::permission_scope::model::ScopeModel;
use crate::components::permissions_picker::PermissionsPicker;
use crate::repaint::Dirty;
use crate::storage_writer::{PermissionMutationWriter, StorageWriter};
use crate::{AppSession, PermissionAuthorityBinding};

use super::App;
use super::workbench_styles;

const PERMISSION_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const PERMISSION_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
pub(super) const PERMISSION_WORKER_BUSY: &str =
    "A permission operation is still awaiting acknowledgment";
const PERMISSION_WORKER_GONE: &str = "Permission worker disconnected; inspect again before saving";
const PERMISSION_STALE: &str =
    "Permission context changed; draft retained. Reopen Permissions to revalidate";
const PERMISSION_SAVED: &str = "Permission change durably saved";
const PERMISSION_SUSPENDED: &str =
    "Permission draft suspended; reopen Permissions to resume and revalidate";
pub(super) const LOCAL_SOURCE_NOTICE: &str =
    "LOCAL policy source · Save changes text only; reload and fresh trust are separate";
const LOCAL_SOURCE_BUSY: &str =
    "Save or discard local source edits and wait for pending operations before returning";
const PERMISSION_EDITOR_DISCONNECTED: &str =
    "Permission editing requires a connected, verified workspace";

pub(crate) struct SessionPermissionPublication {
    storage: StateDir,
    session: CaudraId,
    writer: PermissionMutationWriter,
    snapshot: Arc<ArcSwap<PermissionSnapshot>>,
}

impl PermissionPublication for SessionPermissionPublication {
    fn snapshot(&self) -> Result<PermissionSnapshot, PermissionEditError> {
        let snapshot = SessionDatabase::open_read_only(&self.storage)
            .map_err(|error| PermissionEditError::Storage(error.to_string()))?
            .permission_snapshot(PermissionOwner::Conversation(self.session))?;
        self.snapshot.store(Arc::new(snapshot.clone()));
        Ok(snapshot)
    }

    fn commit(
        &self,
        prepared: &PreparedPermissionMutation,
    ) -> Result<PermissionCommitReceipt, PermissionEditError> {
        let reply = self.writer.submit(prepared.clone())?;
        match reply.recv_timeout(PERMISSION_WRITE_TIMEOUT) {
            Ok(result) => result.map_err(Into::into),
            Err(error) => {
                if let Some(receipt) = self.receipt(prepared.operation_id())? {
                    return Ok(receipt);
                }
                match reply.recv() {
                    Ok(result) => result.map_err(Into::into),
                    Err(_) => self.receipt(prepared.operation_id())?.ok_or_else(|| {
                        PermissionEditError::Storage(format!(
                            "Permission acknowledgment unavailable: {error}"
                        ))
                    }),
                }
            }
        }
    }

    fn receipt(
        &self,
        operation_id: CaudraId,
    ) -> Result<Option<PermissionCommitReceipt>, PermissionEditError> {
        SessionDatabase::open_read_only(&self.storage)
            .map_err(|error| PermissionEditError::Storage(error.to_string()))?
            .permission_receipt(operation_id)
            .map_err(Into::into)
    }
}

pub(crate) fn attach_session_permissions(
    storage: &StateDir,
    writer: &Arc<StorageWriter>,
    session: &mut AppSession,
    manager: &Arc<PermissionManager>,
) -> Result<Arc<ArcSwap<PermissionSnapshot>>, String> {
    let storage = storage.clone();
    let writer = Arc::clone(writer);
    let manager = Arc::clone(manager);
    let initial = Arc::new(session.clone());
    let snapshot = smol::block_on(smol::unblock(move || {
        writer
            .save_sync(Arc::clone(&initial))
            .map_err(|error| error.to_string())?;
        let current = SessionDatabase::open_read_only(&storage)
            .map_err(|error| error.to_string())?
            .permission_snapshot(PermissionOwner::Conversation(initial.id))
            .map_err(|error| error.to_string())?;
        let snapshot = Arc::new(ArcSwap::from_pointee(current));
        let publication = Arc::new(SessionPermissionPublication {
            storage,
            session: initial.id,
            writer: writer.permission_mutation_writer(),
            snapshot: Arc::clone(&snapshot),
        });
        manager
            .attach_permission_publication(publication)
            .map_err(|error| error.to_string())?;
        Ok::<_, String>(snapshot)
    }))?;
    snapshot
        .load()
        .apply_to_meta(session.id, &mut session.meta)
        .map_err(|error| error.to_string())?;
    Ok(snapshot)
}

#[derive(Clone)]
struct EditIntent {
    operation: PermissionEditOperation,
    original: Option<Arc<PermissionRuleRecord>>,
    draft: Option<PermissionRuleDraft>,
}

impl EditIntent {
    fn begin(
        &self,
        manager: &PermissionManager,
    ) -> Result<PermissionEditSession, PermissionEditError> {
        let session = manager
            .begin_permission_edit(self.operation.clone(), PermissionEditEvidence::default())?;
        if self
            .original
            .as_deref()
            .is_some_and(|original| session.original() != Some(original))
        {
            return Err(PermissionEditError::Conflict);
        }
        Ok(session)
    }
}

struct ValidatedDraft {
    revision: u64,
    draft: PermissionRuleDraft,
    preview: Arc<PermissionEditPreview>,
}

#[derive(Clone)]
struct PermissionReplyGuard {
    owner: Weak<PermissionManager>,
    session: CaudraId,
    context: (PathBuf, u64),
    revision: Option<u64>,
    epoch: u64,
}

struct PendingPermissionJob {
    guard: PermissionReplyGuard,
    reply: Receiver<Result<PermissionReply, PermissionEditError>>,
    mutation: bool,
}

struct PermissionInventory {
    records: Vec<PermissionRuleRecord>,
    candidates: Vec<PermissionReviewCandidate>,
    policy: Vec<ActivePolicyRule>,
    needs_trust: bool,
    trusted: bool,
}

impl PermissionInventory {
    fn load(manager: &PermissionManager) -> Result<Self, PermissionEditError> {
        Ok(Self {
            records: manager
                .structured_rule_inventory_filtered(&PermissionProjectFilter::All, true)
                .map_err(|error| PermissionEditError::Storage(error.to_string()))?,
            candidates: manager.review_candidates(),
            policy: manager.active_policy(),
            needs_trust: manager.needs_project_permission_config_trust(),
            trusted: manager.project_permission_config_trusted(),
        })
    }
}

enum PermissionReply {
    Rebound {
        authority: Box<PermissionAuthorityContext>,
        context: (PathBuf, u64),
        reply: Option<Result<Box<PermissionReply>, PermissionEditError>>,
    },
    Inventory(PermissionInventory),
    Begun {
        session: PermissionEditSession,
        intent: EditIntent,
        resume: bool,
    },
    Preview {
        session: PermissionEditSession,
        validated: ValidatedDraft,
    },
    Analyzed {
        revision: u64,
        target: usize,
        result: Result<PatternDefinition, PermissionEditError>,
        source: TemplateSource,
    },
    Saved(PermissionCommitReceipt),
    Answered {
        request: String,
        accepted: bool,
    },
    Refreshed(bool),
    Tested {
        revision: u64,
        test_revision: u64,
        result: Result<PermissionMatchPreview, String>,
    },
    Source(Box<Workbench>),
    CopyRequired(String),
}

fn permission_error_message(error: &PermissionEditError) -> String {
    match error {
        PermissionEditError::Invalid(fields) => fields
            .iter()
            .map(|field| format!("{:?}: {}", field.field, field.message))
            .collect::<Vec<_>>()
            .join("; "),
        _ => error.to_string(),
    }
}

#[derive(Clone)]
struct PermissionAuthorityContext {
    project: PathBuf,
    mode: AgentMode,
    model: String,
    workspace: Option<WorkspaceSession>,
    binding: PermissionAuthorityBinding,
}

impl PermissionAuthorityContext {
    fn matches_runtime(
        &self,
        project: &Path,
        mode: &AgentMode,
        model: &Model,
        workspace: &Option<WorkspaceSession>,
    ) -> bool {
        self.project.as_path() == project
            && self.mode == *mode
            && self.model == model.spec()
            && same_workspace(&self.workspace, workspace)
    }

    fn matches(&self, other: &Self) -> bool {
        self.project == other.project
            && self.mode == other.mode
            && self.model == other.model
            && self.binding.available == other.binding.available
            && self.binding.registry_revision == other.binding.registry_revision
            && same_tool_filter(&self.binding.tool_filter, &other.binding.tool_filter)
            && same_workspace(&self.workspace, &other.workspace)
    }
}

fn same_workspace(left: &Option<WorkspaceSession>, right: &Option<WorkspaceSession>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(previous), Some(current)) => {
            previous.binding() == current.binding()
                && previous.cursor() == current.cursor()
                && previous.workspace().capabilities() == current.workspace().capabilities()
                && ptr::eq(
                    previous.workspace().services(),
                    current.workspace().services(),
                )
        }
        _ => false,
    }
}

fn same_tool_filter(left: &ToolFilter, right: &ToolFilter) -> bool {
    match (left, right) {
        (ToolFilter::All, ToolFilter::All) => true,
        (ToolFilter::Only(left), ToolFilter::Only(right))
        | (ToolFilter::AllExcept(left), ToolFilter::AllExcept(right)) => left == right,
        (ToolFilter::ReadOnly(left), ToolFilter::ReadOnly(right)) => same_tool_filter(left, right),
        _ => false,
    }
}

#[derive(Default)]
pub(super) struct PermissionUi {
    pending: Option<PendingPermissionJob>,
    intent: Option<EditIntent>,
    session: Option<PermissionEditSession>,
    validated: Option<ValidatedDraft>,
    context: Option<(PathBuf, u64)>,
    suspended: Option<PermissionsPicker>,
    last_refresh: Option<Instant>,
    clock: Option<Instant>,
    epoch: u64,
    authority_context: Option<PermissionAuthorityContext>,
    editor_owner: Option<CaudraId>,
    deferred_answer: Option<PermissionDecision>,
    pub(super) show_discovery: bool,
}

impl App {
    #[cfg(test)]
    pub(super) fn finish_permission_jobs(&mut self) {
        while let Some(pending) = &mut self.permission_ui.pending {
            let result = pending
                .reply
                .recv_timeout(PERMISSION_WRITE_TIMEOUT)
                .unwrap_or_else(|_| panic!("{PERMISSION_WORKER_GONE}"));
            let (sender, receiver) = flume::bounded(1);
            assert!(sender.send(result).is_ok());
            pending.reply = receiver;
            let _ = self.poll_permission_editor();
        }
    }

    pub(crate) fn sync_permission_authority(&mut self) -> Result<(), PermissionEditError> {
        let Some(factory) = &self.permission_authority_factory else {
            return Ok(());
        };
        let factory = Arc::clone(factory);
        let project = self.permissions.project_cwd();
        let mode = self.agent_mode();
        let model = self.permission_authority_model();
        let model_spec = model.spec();
        let workspace = self.workspace_session.clone();
        let context = match smol::block_on(smol::unblock(move || {
            let binding = factory(project.clone(), mode.clone(), model, workspace.clone())?;
            Ok::<_, PermissionEditError>(PermissionAuthorityContext {
                project,
                mode,
                model: model_spec,
                workspace,
                binding,
            })
        })) {
            Ok(context) => context,
            Err(error) => {
                self.invalidate_permission_authority();
                return Err(error);
            }
        };
        if self
            .permission_ui
            .authority_context
            .as_ref()
            .is_some_and(|previous| previous.matches(&context))
        {
            return Ok(());
        }
        self.permissions
            .set_permission_authority_provider(Arc::clone(&context.binding.provider));
        self.permission_ui.authority_context = Some(context);
        self.suspend_permission_editor();
        self.permission_ui.context = Some(self.permissions.pattern_candidate_context());
        self.sync_pattern_discovery_context();
        Ok(())
    }

    fn permission_authority_model(&self) -> Model {
        self.effective_model_slot
            .as_ref()
            .map(|slot| slot.load().model.clone())
            .unwrap_or_else(|| self.state.model.clone())
    }

    fn refresh_permission_runtime_context(&mut self) {
        if self
            .permission_ui
            .authority_context
            .as_ref()
            .is_some_and(|context| {
                !context.matches_runtime(
                    &self.permissions.project_cwd(),
                    &self.agent_mode(),
                    &self.permission_authority_model(),
                    &self.workspace_session,
                )
            })
        {
            self.invalidate_permission_authority();
        }
    }

    pub(crate) fn invalidate_permission_authority(&mut self) {
        if let Some(context) = self.permission_ui.authority_context.take() {
            self.permissions
                .set_permission_authority_provider(context.binding.provider);
        }
        self.suspend_permission_editor();
    }

    pub(super) fn open_permission_source(&mut self, locator: VerifiedLocalSourceLocator) {
        if self.parked_workbench.is_some() {
            self.flash(LOCAL_SOURCE_BUSY.into());
            return;
        }
        let styles = workbench_styles();
        self.permission_job(None, false, move |manager| {
            let verified_origin = || {
                manager
                    .active_policy()
                    .iter()
                    .any(|policy| policy.verified_local_source_locator.as_ref() == Some(&locator))
            };
            if !verified_origin() {
                return Err(PermissionEditError::Conflict);
            }
            let workbench = Workbench::open_local_source(
                styles,
                locator.path(),
                None,
                &locator,
                |path, bytes, expected| {
                    expected
                        .verify_loaded_bytes(path, bytes)
                        .map_err(|error| error.to_string())
                },
            )
            .map_err(|error| PermissionEditError::Unavailable(error.to_string()))?;
            if !verified_origin() {
                return Err(PermissionEditError::Conflict);
            }
            Ok(PermissionReply::Source(Box::new(workbench)))
        });
    }

    pub(super) fn close_permission_source(&mut self) {
        if self.parked_workbench.is_some() && self.workbench.blocks_workspace_change() {
            self.flash(LOCAL_SOURCE_BUSY.into());
            return;
        }
        self.workbench.close();
        if let Some(parked) = self.parked_workbench.take() {
            self.workbench = parked;
            self.workbench.set_styles(workbench_styles());
            self.flash(LOCAL_SOURCE_NOTICE.into());
        }
    }

    fn permission_job(
        &mut self,
        revision: Option<u64>,
        mutation: bool,
        work: impl FnOnce(Arc<PermissionManager>) -> Result<PermissionReply, PermissionEditError>
        + Send
        + 'static,
    ) -> bool {
        if self.permission_ui.pending.is_some() {
            self.flash(PERMISSION_WORKER_BUSY.into());
            return false;
        }
        let manager = Arc::clone(&self.permissions);
        let guard = PermissionReplyGuard {
            owner: Arc::downgrade(&manager),
            session: self.state.session.id,
            context: manager.pattern_candidate_context(),
            revision,
            epoch: self.permission_ui.epoch,
        };
        let (sender, reply) = flume::bounded(1);
        smol::spawn(async move {
            let result = smol::unblock(move || work(manager)).await;
            let _ = sender.send(result);
        })
        .detach();
        self.permission_ui.pending = Some(PendingPermissionJob {
            guard,
            reply,
            mutation,
        });
        true
    }

    fn permission_editor_job(
        &mut self,
        revision: Option<u64>,
        mutation: bool,
        work: impl FnOnce(Arc<PermissionManager>) -> Result<PermissionReply, PermissionEditError>
        + Send
        + 'static,
    ) -> bool {
        let factory = self.permission_authority_factory.clone();
        let previous = self.permission_ui.authority_context.clone();
        let project = self.permissions.project_cwd();
        let mode = self.agent_mode();
        let model = self.permission_authority_model();
        let workspace = self.workspace_session.clone();
        self.permission_job(revision, mutation, move |manager| {
            if let Some(factory) = factory {
                let model_spec = model.spec();
                let binding = factory(project.clone(), mode.clone(), model, workspace.clone())?;
                let authority = PermissionAuthorityContext {
                    project,
                    mode,
                    model: model_spec,
                    workspace,
                    binding,
                };
                if previous
                    .as_ref()
                    .is_none_or(|previous| !previous.matches(&authority))
                {
                    manager
                        .set_permission_authority_provider(Arc::clone(&authority.binding.provider));
                    let context = manager.pattern_candidate_context();
                    let reply = revision.is_none().then(|| work(manager).map(Box::new));
                    return Ok(PermissionReply::Rebound {
                        authority: Box::new(authority),
                        context,
                        reply,
                    });
                }
                if revision.is_some() && !authority.binding.available {
                    return Err(PermissionEditError::Unavailable(
                        PERMISSION_EDITOR_DISCONNECTED.into(),
                    ));
                }
            }
            work(manager)
        })
    }

    fn permission_template_job(
        &mut self,
        revision: u64,
        target: usize,
        source: TemplateSource,
        analyze: impl FnOnce(
            &PermissionManager,
            &PermissionEditSession,
            &TemplateSource,
        ) -> Result<PatternDefinition, PermissionEditError>
        + Send
        + 'static,
    ) {
        if self.permission_ui.editor_owner != Some(self.state.session.id) {
            self.permission_failure(PERMISSION_STALE);
            return;
        }
        let Some(editor) = self.permissions_picker.editor_mut() else {
            return;
        };
        if editor.revision() != revision || editor.is_suspended() || editor.is_editing() {
            return;
        }
        let Some(session) = self.permission_ui.session.clone() else {
            editor.analysis_failed(revision, PERMISSION_STALE);
            return;
        };
        self.permission_ui.validated = None;
        if !self.permission_editor_job(Some(revision), false, move |manager| {
            let result = analyze(&manager, &session, &source);
            Ok(PermissionReply::Analyzed {
                revision,
                target,
                result,
                source,
            })
        }) && let Some(editor) = self.permissions_picker.editor_mut()
        {
            editor.analysis_failed(revision, PERMISSION_WORKER_BUSY);
        }
    }

    pub(super) fn request_permission_inventory(&mut self) {
        if self.permission_ui.pending.is_some() {
            self.flash(PERMISSION_WORKER_BUSY.into());
            return;
        }
        self.refresh_permission_runtime_context();
        self.sync_pattern_discovery_context();
        if let Some(picker) = self.permission_ui.suspended.take() {
            self.permissions_picker = picker;
            if let Some(editor) = self.permissions_picker.editor_mut() {
                editor.set_visible(true);
            }
            if let Some(intent) = self.permission_ui.intent.clone() {
                self.permission_editor_job(None, false, move |manager| {
                    Ok(PermissionReply::Begun {
                        session: intent.begin(&manager)?,
                        intent,
                        resume: true,
                    })
                });
            }
            return;
        }
        self.permission_job(None, false, |manager| {
            PermissionInventory::load(&manager).map(PermissionReply::Inventory)
        });
    }

    pub(super) fn handle_permission_editor(&mut self, event: EditorEvent) {
        self.refresh_permission_runtime_context();
        match event {
            EditorEvent::Begin(launch) => {
                let (operation, original, draft) = match launch {
                    EditorLaunch::New => (PermissionEditOperation::Create, None, None),
                    EditorLaunch::Edit(record) => (
                        PermissionEditOperation::Replace(record.id.clone()),
                        Some(record),
                        None,
                    ),
                    EditorLaunch::Duplicate(record) => (
                        PermissionEditOperation::Duplicate(record.id.clone()),
                        Some(record),
                        None,
                    ),
                    EditorLaunch::Copy { source, draft } => (
                        PermissionEditOperation::Copy(source.id.clone()),
                        Some(source),
                        draft.map(|draft| *draft),
                    ),
                    EditorLaunch::Discover(candidate) => {
                        let mut draft = PermissionRuleDraft::blank();
                        draft.resources = ResourcesDraft::Constrained(vec![ResourceDraft {
                            original_index: None,
                            kind: PermissionResourceKind::Command,
                            selector: SelectorDraft::Replace(SelectorValue::CommandTemplate {
                                definition: Box::new(candidate.definition.clone()),
                                source: None,
                            }),
                            access: GuardDraft::Equals(PermissionResourceAccess::Execute),
                            protected: GuardDraft::Equals(false),
                            attributes: Default::default(),
                        }]);
                        (
                            PermissionEditOperation::ActivateDiscovery,
                            None,
                            Some(draft),
                        )
                    }
                };
                self.begin_permission_intent(EditIntent {
                    operation,
                    original,
                    draft,
                });
            }
            EditorEvent::Preview { revision, draft } => {
                if self.permission_ui.editor_owner != Some(self.state.session.id) {
                    self.permission_failure("This draft belongs to a different conversation; cancel it and explicitly create or copy a rule here");
                    return;
                }
                let Some(intent) = self.permission_ui.intent.clone() else {
                    return;
                };
                let Some(editor) = self.permissions_picker.editor_mut() else {
                    return;
                };
                if revision != editor.revision()
                    || editor.is_suspended()
                    || &*draft != editor.draft()
                {
                    return;
                }
                self.permission_ui.validated = None;
                let session = self.permission_ui.session.clone();
                self.permission_editor_job(Some(revision), false, move |manager| {
                    let session = match session {
                        Some(session) => session,
                        None => intent.begin(&manager)?,
                    };
                    let preview = match manager.preview_permission_edit(&session, &draft) {
                        Ok(preview) => Arc::new(preview),
                        Err(PermissionEditError::Unavailable(reason))
                            if reason == PermissionMutationError::DifferentDatabase.to_string() =>
                        {
                            return Ok(PermissionReply::CopyRequired(reason));
                        }
                        Err(error) => return Err(error),
                    };
                    Ok(PermissionReply::Preview {
                        session,
                        validated: ValidatedDraft {
                            revision,
                            draft: *draft,
                            preview,
                        },
                    })
                });
            }
            EditorEvent::Analyze {
                revision,
                target,
                authority_key,
                source,
                proposed,
            } => {
                self.permission_template_job(
                    revision,
                    target,
                    source,
                    move |manager, session, source| {
                        manager.analyze_permission_template(
                            session,
                            &authority_key,
                            source,
                            &proposed,
                        )
                    },
                );
            }
            EditorEvent::Seed {
                revision,
                target,
                authority_key,
                source,
                name,
            } => {
                self.permission_template_job(
                    revision,
                    target,
                    source,
                    move |manager, session, source| {
                        manager.seed_permission_template(session, &authority_key, source, &name)
                    },
                );
            }
            EditorEvent::Test {
                revision,
                test_revision,
                example,
            } => {
                let Some(editor) = self.permissions_picker.editor_mut() else {
                    return;
                };
                let Some(validated) = &self.permission_ui.validated else {
                    editor.receive_test(
                        revision,
                        test_revision,
                        Err("Preview the current draft before testing".into()),
                    );
                    return;
                };
                if editor.revision() != revision
                    || editor.is_suspended()
                    || validated.revision != revision
                    || editor.draft() != &validated.draft
                {
                    return;
                }
                let preview = Arc::clone(&validated.preview);
                self.permission_editor_job(Some(revision), false, move |manager| {
                    let result = manager
                        .preview_permission_example(
                            &preview,
                            &example.authority_key,
                            &example.input,
                        )
                        .map_err(|error| error.to_string());
                    Ok(PermissionReply::Tested {
                        revision,
                        test_revision,
                        result,
                    })
                });
            }
            EditorEvent::Save {
                revision,
                acknowledged,
            } => {
                let Some(editor) = self.permissions_picker.editor_mut() else {
                    return;
                };
                let Some(validated) = &self.permission_ui.validated else {
                    self.permission_failure("Validate and review the current draft before saving");
                    return;
                };
                if revision != editor.revision()
                    || editor.is_suspended()
                    || revision != validated.revision
                    || editor.draft() != &validated.draft
                    || editor.is_editing()
                {
                    self.permission_failure("The draft changed; preview and review again");
                    return;
                }
                let confirmation = match validated.preview.confirm(&acknowledged) {
                    Ok(confirmation) => confirmation,
                    Err(error) => {
                        self.permission_failure(&error.to_string());
                        return;
                    }
                };
                let preview = Arc::clone(&validated.preview);
                let draft = validated.draft.clone();
                self.permission_editor_job(Some(revision), true, move |manager| {
                    let receipt =
                        match manager.commit_permission_edit(&preview, &confirmation, &draft) {
                            Ok(receipt) => receipt,
                            Err(error) => {
                                manager.permission_edit_receipt(&preview)?.ok_or(error)?
                            }
                        };
                    manager
                        .refresh_permission_state()
                        .map_err(|error| PermissionEditError::Storage(error.to_string()))?;
                    Ok(PermissionReply::Saved(receipt))
                });
            }
            EditorEvent::Cancel => {
                if self
                    .permission_ui
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.mutation)
                {
                    self.flash(PERMISSION_WORKER_BUSY.into());
                    return;
                }
                self.permissions_picker.close_editor();
                self.permission_ui.epoch = self.permission_ui.epoch.saturating_add(1);
                self.permission_ui.session = None;
                self.permission_ui.intent = None;
                self.permission_ui.validated = None;
            }
        }
    }

    fn begin_permission_intent(&mut self, intent: EditIntent) {
        if self
            .permissions_picker
            .editor_mut()
            .is_some_and(|editor| editor.is_dirty())
            && !matches!(intent.operation, PermissionEditOperation::Copy(_))
        {
            self.flash(
                "Save or cancel the current draft before starting another permission edit".into(),
            );
            return;
        }
        self.permission_editor_job(None, false, move |manager| {
            Ok(PermissionReply::Begun {
                session: intent.begin(&manager)?,
                intent,
                resume: false,
            })
        });
    }

    pub(super) fn request_permission_revoke(&mut self, id: String) {
        self.begin_permission_intent(EditIntent {
            operation: PermissionEditOperation::Revoke(id),
            original: None,
            draft: None,
        });
    }

    pub(super) fn request_permission_answer(&mut self, decision: PermissionDecision) {
        if self
            .permissions
            .pending_request(&decision.request_id)
            .is_none()
        {
            self.permission_prompt.resolve(&decision.request_id);
            return;
        }
        if self.permission_ui.pending.is_some() {
            if self.permission_ui.deferred_answer.is_none() {
                self.permission_ui.deferred_answer = Some(decision);
            }
            return;
        }
        self.permission_job(None, true, move |manager| {
            let transient = matches!(
                decision.answer,
                PermissionAnswer::AllowOnce
                    | PermissionAnswer::Deny
                    | PermissionAnswer::DenyWithGuidance(_)
            );
            let accepted = manager.answer(&decision.request_id, decision.answer)
                || transient
                || manager.pending_request(&decision.request_id).is_none();
            Ok(PermissionReply::Answered {
                request: decision.request_id,
                accepted,
            })
        });
    }

    fn permission_failure(&mut self, message: &str) {
        self.permission_ui.validated = None;
        if let Some(editor) = self.permissions_picker.editor_mut() {
            editor.save_failed(message);
        }
        self.flash(message.into());
    }

    pub(super) fn suspend_permission_editor(&mut self) {
        self.permission_ui.epoch = self.permission_ui.epoch.saturating_add(1);
        if self.permissions_picker.editor_mut().is_some() {
            if let Some(editor) = self.permissions_picker.editor_mut() {
                editor.suspend();
            }
            self.permission_ui.validated = None;
            self.permission_ui.session = None;
            self.permission_ui.suspended = Some(std::mem::replace(
                &mut self.permissions_picker,
                PermissionsPicker::new(),
            ));
            self.flash(PERMISSION_SUSPENDED.into());
        } else {
            self.permissions_picker.close();
        }
    }

    pub(super) fn permission_job_pending(&self) -> bool {
        self.permission_ui.pending.is_some()
    }

    pub(super) fn permission_mutation_pending(&self) -> bool {
        self.permission_ui
            .pending
            .as_ref()
            .is_some_and(|pending| pending.mutation)
    }

    pub(crate) fn workbench_blocks_workspace_change(&self) -> bool {
        self.parked_workbench.is_some() || self.workbench.blocks_workspace_change()
    }

    pub(super) fn poll_permission_editor(&mut self) -> Dirty {
        self.refresh_permission_runtime_context();
        let context = self.permissions.pattern_candidate_context();
        if self.permission_ui.pending.is_none()
            && self
                .permission_ui
                .context
                .as_ref()
                .is_some_and(|previous| previous != &context)
        {
            self.suspend_permission_editor();
            self.sync_pattern_discovery_context();
        }
        let Some(pending) = &self.permission_ui.pending else {
            self.permission_ui.context = Some(context.clone());
            if let Some(decision) = self.permission_ui.deferred_answer.take() {
                self.request_permission_answer(decision);
                return Dirty::YES;
            }
            let elapsed = self
                .permission_ui
                .clock
                .get_or_insert_with(Instant::now)
                .elapsed();
            if self.permissions_picker.is_open()
                && let Some(event) = self
                    .permissions_picker
                    .editor_mut()
                    .and_then(|editor| editor.poll_preview(elapsed))
            {
                self.handle_permission_editor(event);
                return Dirty::YES;
            }
            if self
                .permission_ui
                .last_refresh
                .is_none_or(|last| last.elapsed() >= PERMISSION_REFRESH_INTERVAL)
            {
                self.permission_ui.last_refresh = Some(Instant::now());
                self.permission_editor_job(None, false, |manager| {
                    manager
                        .poll_permission_changes()
                        .map(PermissionReply::Refreshed)
                        .map_err(|error| PermissionEditError::Storage(error.to_string()))
                });
            }
            return Dirty::NO;
        };
        let result = match pending.reply.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return Dirty::NO,
            Err(TryRecvError::Disconnected) => Err(PermissionEditError::Unavailable(
                PERMISSION_WORKER_GONE.into(),
            )),
        };
        let Some(pending) = self.permission_ui.pending.take() else {
            return Dirty::NO;
        };
        let valid_authority = match &result {
            Ok(PermissionReply::Rebound {
                authority,
                context: published,
                ..
            }) => {
                *published == context
                    && authority.matches_runtime(
                        &self.permissions.project_cwd(),
                        &self.agent_mode(),
                        &self.permission_authority_model(),
                        &self.workspace_session,
                    )
            }
            _ => pending.guard.context == context,
        };
        let valid_context = pending.guard.session == self.state.session.id
            && pending
                .guard
                .owner
                .ptr_eq(&Arc::downgrade(&self.permissions))
            && valid_authority;
        if valid_context && let Ok(PermissionReply::Rebound { authority, .. }) = &result {
            self.permission_ui.authority_context = Some(authority.as_ref().clone());
        }
        let valid_epoch = pending.mutation || pending.guard.epoch == self.permission_ui.epoch;
        let valid_revision = pending.guard.revision.is_none_or(|revision| {
            self.permissions_picker
                .editor_mut()
                .is_some_and(|editor| !editor.is_suspended() && editor.revision() == revision)
        });
        if !valid_context || !valid_revision || !valid_epoch {
            if pending.guard.context != context {
                self.suspend_permission_editor();
                self.sync_pattern_discovery_context();
                self.permission_ui.context = Some(context);
            }
            if pending.mutation {
                self.flash(match result {
                    Ok(PermissionReply::Saved(_)) => "Previously reviewed permission was saved; the current draft needs fresh review".into(),
                    _ => PERMISSION_STALE.into(),
                });
            }
            return Dirty::YES;
        }
        self.permission_ui.context = Some(context.clone());
        let mut rebound = false;
        let result = match result {
            Ok(PermissionReply::Rebound {
                authority, reply, ..
            }) => {
                self.permission_ui.authority_context = Some(*authority);
                self.suspend_permission_editor();
                self.sync_pattern_discovery_context();
                rebound = true;
                match reply {
                    Some(reply) => {
                        if matches!(&reply, Ok(reply) if matches!(reply.as_ref(), PermissionReply::Begun { resume: true, .. }))
                            && let Some(picker) = self.permission_ui.suspended.take()
                        {
                            self.permissions_picker = picker;
                        }
                        reply.map(|reply| *reply)
                    }
                    None => {
                        self.flash(PERMISSION_STALE.into());
                        return Dirty::YES;
                    }
                }
            }
            result => result,
        };
        match result {
            Ok(PermissionReply::Rebound { .. }) => self.permission_failure(PERMISSION_STALE),
            Err(error) => {
                let message = permission_error_message(&error);
                self.permission_failure(&message);
            }
            Ok(PermissionReply::Inventory(inventory)) => {
                self.permissions_picker.set_current_project(Some(context.0));
                self.permissions_picker.open(
                    inventory.records,
                    &inventory.candidates,
                    &inventory.policy,
                    inventory.needs_trust,
                    inventory.trusted,
                );
                self.refresh_permission_suggestions();
                if self.permission_ui.show_discovery {
                    self.permissions_picker.show_discovery();
                    self.permission_ui.show_discovery = false;
                }
            }
            Ok(PermissionReply::Begun {
                session,
                intent,
                resume,
            }) => {
                self.permissions_picker.set_current_project(Some(context.0));
                if resume {
                    if let Some(editor) = self.permissions_picker.editor_mut() {
                        editor.resume(session.catalog().clone());
                    }
                } else {
                    self.permission_ui.suspended = None;
                    let editor = match &intent.draft {
                        Some(draft) => ScopeEditor::new(
                            draft.clone(),
                            session.catalog().clone(),
                            session
                                .original()
                                .map(|record| ScopeModel::record(Arc::new(record.clone()))),
                        ),
                        None => ScopeEditor::from_session(&session),
                    };
                    self.permissions_picker.set_editor(editor);
                    self.permission_ui.editor_owner = Some(self.state.session.id);
                }
                self.permission_ui.session = Some(session);
                self.permission_ui.intent = Some(intent);
                self.permission_ui.validated = None;
                self.flash(format!("Editor ready · {} pending requests may be released by a saved grant; samples are never executed", self.permissions.pending_count()));
            }
            Ok(PermissionReply::Preview { session, validated }) => {
                if let Some(editor) = self.permissions_picker.editor_mut() {
                    editor.receive_backend_preview(validated.revision, &validated.preview);
                }
                self.permission_ui.session = Some(session);
                self.permission_ui.validated = Some(validated);
            }
            Ok(PermissionReply::Analyzed {
                revision,
                target,
                result,
                source,
            }) => match result {
                Ok(definition) => {
                    if let Some(editor) = self.permissions_picker.editor_mut() {
                        editor.set_analyzed_template(revision, target, definition, source);
                    }
                }
                Err(error) => {
                    let message = permission_error_message(&error);
                    if let Some(editor) = self.permissions_picker.editor_mut() {
                        editor.analysis_failed(revision, &message);
                    }
                    self.flash(message);
                }
            },
            Ok(PermissionReply::Tested {
                revision,
                test_revision,
                result,
            }) => {
                if let Some(editor) = self.permissions_picker.editor_mut() {
                    editor.receive_test(revision, test_revision, result);
                }
            }
            Ok(PermissionReply::Source(workbench)) => {
                if self.parked_workbench.is_none() {
                    self.permissions_picker.close();
                    self.parked_workbench =
                        Some(std::mem::replace(&mut self.workbench, *workbench));
                    self.flash(LOCAL_SOURCE_NOTICE.into());
                }
            }
            Ok(PermissionReply::CopyRequired(reason)) => {
                self.permission_ui.validated = None;
                if let Some(editor) = self.permissions_picker.editor_mut() {
                    editor.offer_copy(&reason);
                }
            }
            Ok(PermissionReply::Saved(_receipt)) => {
                self.permissions_picker.close_editor();
                self.permission_ui.session = None;
                self.permission_ui.intent = None;
                self.permission_ui.validated = None;
                self.permission_ui.editor_owner = None;
                self.request_permission_inventory();
                self.flash(PERMISSION_SAVED.into());
            }
            Ok(PermissionReply::Answered { request, accepted }) => {
                if accepted {
                    self.permission_prompt.resolve(&request);
                } else {
                    self.flash("Could not save permission decision".into());
                }
            }
            Ok(PermissionReply::Refreshed(changed)) => {
                if !changed {
                    return Dirty::from(rebound);
                }
                if self.permissions_picker.editor_mut().is_some() {
                    self.suspend_permission_editor();
                } else if self.permissions_picker.is_open() {
                    self.request_permission_inventory();
                }
            }
        }
        Dirty::YES
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Instant;

    use async_trait::async_trait;
    use caudra_agent::permissions::editor::{
        ArgumentMode, ArgumentsDraft, AuthorityCatalog, EditableAuthorityDescriptor, IdentityDraft,
        PermissionAuthorityLease, PermissionAuthorityProvider, PermissionEditError,
        PermissionExampleAnalysis, PermissionPublication, PermissionRuleDraft, ProjectDraft,
        ResourceCapability, SelectorMode, TemplateAnalysis, TemplateSource,
    };
    use caudra_agent::permissions::{
        PermissionLifetime, PermissionManager, PermissionResourceAccess, PermissionResourceKind,
        PermissionResourceSelector, PluginRuleStore, StructuredPermissionEffect,
        VerifiedLocalSourceLocator,
    };
    use caudra_agent::tools::registry::{
        ParseError, RegisteredTool, Tool, ToolEffect, ToolInvocation, ToolSource, TrustedToolSource,
    };
    use caudra_agent::tools::{DescriptionContext, ToolFilter};
    use caudra_config::{Effect, PermissionRule, PermissionsConfig, ToolKey};
    use caudra_storage::id::CaudraId;
    use caudra_storage::permission_patterns::{ArgumentRole, PatternToken, SlotCombinations};
    use caudra_storage::permission_state::mutation::{
        PermissionCommitReceipt, PermissionOwner, PermissionSnapshot, PreparedPermissionMutation,
    };
    use caudra_storage::sessions::SessionDatabase;
    use caudra_storage::workspace_binding::StoredWorkspaceBinding;
    use caudra_workbench::{MutationGate, keys as workbench_keys};
    use caudra_workspace::{
        AuthenticatedPrincipalId, ByteContent, ByteRange, CancellationResult, CollectionRevision,
        CwdHandle, ListPage, ListRequest, Mutation, MutationCondition, MutationEntryResult,
        MutationKind, MutationRequest, MutationResult, OperationHandle, OperationId,
        OperationState, OperationStatus, ReadBytesRequest, ReadTextRequest,
        ResolvedWorkspaceDirectory, ResourceId, ResourceKind, ResourceRevision, ResourceScope,
        ResourceSelector, SearchPage, SearchRequest, SequenceMetadata, SessionBindingId,
        SessionWorkspaceBinding, TextContent, WorkspaceCapabilities, WorkspaceCapability,
        WorkspaceCursor, WorkspaceError, WorkspaceHandle, WorkspaceMutationService, WorkspacePath,
        WorkspaceReadService, WorkspaceResource, WorkspaceSearchService, WorkspaceServices,
        WorkspaceSession, WriteContent,
    };
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;
    use unicode_width::UnicodeWidthStr;

    use crate::app::Msg;
    use crate::app::tests::{pattern_suggestion_candidate, remote_workspace_session, test_app};
    use crate::components::buffer_text;
    use crate::components::permission_scope::editor::{
        EditorEvent, EditorLaunch, EditorTestExample, ScopeEditor,
    };
    use crate::components::permissions_picker::PermissionsPickerAction;
    use crate::{AppSession, PermissionAuthorityBinding};

    use super::{
        App, GuardDraft, PERMISSION_SAVED, PermissionReply, PermissionUi, ResourceDraft,
        ResourcesDraft, SelectorDraft, SelectorValue, SessionPermissionPublication,
        attach_session_permissions,
    };

    const TOOL: &str = "editor-test-shell";
    const OWNER: &str = "workcell";
    const CONTRACT: &str = "shell.execution.v1";
    const COMMAND: &str = "git show *";
    const LABEL: &str = "Explicitly reviewed rule";
    const NEXT_LABEL: &str = "Changed display label";
    const NEVER_EXECUTE: &str = "The permission editor must never invoke a tool";
    const WRITE_FAILED: &str = "injected permission writer failure";
    const SOURCE_FILE: &str = "policy.lua";
    const REMOTE_OTHER_FILE: &str = "other.txt";
    const SOURCE_CONTENT: &str = "return 'loaded policy'\n";
    const SOURCE_CHANGE: &str = "-- saved locally, not loaded or trusted\n";
    const REMOTE_CONTENT: &str = "remote content\n";
    const REMOTE_CWD: &str = ".";
    const REMOTE_EDIT: &str = "unsaved remote edit ";
    const REMOTE_REVISION: &str = "remote-before";
    const REMOTE_SAVED_REVISION: &str = "remote-after";
    const REMOTE_OPERATION: &str = "source-test-write";
    const SOURCE_PLUGIN: &str = "source-test-plugin";
    const REMOTE_TIMEOUT: &str = "remote workbench did not settle";
    const SOURCE_WIDTH: u16 = 120;
    const SOURCE_HEIGHT: u16 = 30;
    const OTHER_BINDING: &str = "permission-other-binding";
    const OTHER_PRINCIPAL: &str = "permission-other-principal";
    const OTHER_CURSOR: &str = "permission-other-cursor";
    const EXAMPLE_UNAVAILABLE: &str = "example recorded without executing";
    const REGISTRY_REVISION: u64 = 1;
    const SEED_COMMAND: &str = "git show 'two words' ''";
    const SEED_ARGV: [&str; 4] = ["git", "show", "two words", ""];
    const SEED_REFUSED: &str = "Template analysis refused";
    const WORKDIR_ATTRIBUTE: &str = "workdir";
    const EDITOR_WIDTH: u16 = 80;
    const EDITOR_HEIGHT: u16 = 32;
    const SEED_ACTION: &str = "Create template";
    const PREVIEW_ACTION: &str = "[Preview ^P]";
    const SAVE_ACTION: &str = "[Save ^S]";
    const CONFIRM_ACTION: &str = "Confirm all";

    struct SourceRemoteFiles {
        files: Mutex<BTreeMap<String, (String, ResourceRevision)>>,
        writes: Mutex<Vec<(SessionWorkspaceBinding, WorkspaceCursor, MutationRequest)>>,
        write_gate: Mutex<Option<flume::Receiver<()>>>,
        write_started: flume::Sender<()>,
    }

    impl SourceRemoteFiles {
        fn resource(
            &self,
            binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
            path: &WorkspacePath,
        ) -> Result<WorkspaceResource, WorkspaceError> {
            if path == &WorkspacePath::root() {
                return Ok(WorkspaceResource {
                    project: binding.project().clone(),
                    scope: cursor.scope().clone(),
                    path: None,
                    kind: ResourceKind::ProjectRoot,
                    revision: None,
                    size_bytes: None,
                });
            }
            let files = self.files.lock().unwrap();
            let (content, revision) = files
                .get(path.as_str())
                .ok_or(WorkspaceError::Unavailable)?;
            Ok(WorkspaceResource {
                project: binding.project().clone(),
                scope: ResourceScope::new(
                    vec![cursor.scope().resource_id().clone()],
                    ResourceId::new(path.as_str()).unwrap(),
                )
                .unwrap(),
                path: Some(path.clone()),
                kind: ResourceKind::File,
                revision: Some(revision.clone()),
                size_bytes: Some(content.len() as u64),
            })
        }
    }

    #[async_trait]
    impl WorkspaceReadService for SourceRemoteFiles {
        async fn resolve(
            &self,
            binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
            path: &WorkspacePath,
        ) -> Result<WorkspaceResource, WorkspaceError> {
            self.resource(binding, cursor, path)
        }
        async fn resolve_directory(
            &self,
            _: &SessionWorkspaceBinding,
            _: &WorkspaceCursor,
            _: &WorkspacePath,
        ) -> Result<ResolvedWorkspaceDirectory, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }
        async fn stat(
            &self,
            binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
            resource: &ResourceSelector,
        ) -> Result<WorkspaceResource, WorkspaceError> {
            let path = match resource {
                ResourceSelector::Current => WorkspacePath::root(),
                ResourceSelector::Path(path) => path.clone(),
                ResourceSelector::Id(id) => WorkspacePath::new(id.as_str()).unwrap(),
            };
            self.resource(binding, cursor, &path)
        }
        async fn list(
            &self,
            binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
            _: &ListRequest,
        ) -> Result<ListPage, WorkspaceError> {
            Ok(ListPage {
                revision: CollectionRevision::new(REMOTE_REVISION).unwrap(),
                resources: [SOURCE_FILE, REMOTE_OTHER_FILE]
                    .into_iter()
                    .map(|name| self.resource(binding, cursor, &WorkspacePath::new(name).unwrap()))
                    .collect::<Result<_, _>>()?,
                truncated: false,
                incomplete: false,
                continuation: None,
            })
        }
        async fn read_text(
            &self,
            _: &SessionWorkspaceBinding,
            _: &WorkspaceCursor,
            _: &ReadTextRequest,
        ) -> Result<TextContent, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }
        async fn read_bytes(
            &self,
            _: &SessionWorkspaceBinding,
            _: &WorkspaceCursor,
            request: &ReadBytesRequest,
        ) -> Result<ByteContent, WorkspaceError> {
            let ResourceSelector::Id(id) = &request.resource else {
                return Err(WorkspaceError::Unavailable);
            };
            let files = self.files.lock().unwrap();
            let (content, revision) = files.get(id.as_str()).ok_or(WorkspaceError::Unavailable)?;
            assert_eq!(request.if_revision.as_ref(), Some(revision));
            Ok(ByteContent {
                bytes: content.as_bytes().to_vec(),
                resource_id: id.clone(),
                revision: revision.clone(),
                range: ByteRange {
                    start: 0,
                    end_exclusive: content.len() as u64,
                },
                total_bytes: Some(content.len() as u64),
                truncated: false,
                next_byte_offset: None,
            })
        }
    }

    #[async_trait]
    impl WorkspaceMutationService for SourceRemoteFiles {
        async fn execute(
            &self,
            binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
            request: &MutationRequest,
        ) -> Result<OperationStatus<MutationResult>, WorkspaceError> {
            let gate = self.write_gate.lock().unwrap().take();
            let _ = self.write_started.try_send(());
            if let Some(gate) = gate {
                gate.recv_async().await.unwrap();
            }
            let mut files = self.files.lock().unwrap();
            let mut results = Vec::new();
            for mutation in &request.mutations {
                let Mutation::Write {
                    path,
                    content: WriteContent::Text(content),
                    condition,
                } = mutation
                else {
                    return Err(WorkspaceError::Unavailable);
                };
                let (_, revision) = files
                    .get(path.as_str())
                    .ok_or(WorkspaceError::Unavailable)?;
                assert_eq!(condition, &MutationCondition::Matches(revision.clone()));
                let revision = ResourceRevision::new(REMOTE_SAVED_REVISION).unwrap();
                files.insert(path.as_str().into(), (content.clone(), revision.clone()));
                results.push(MutationEntryResult {
                    kind: MutationKind::Write,
                    path: path.clone(),
                    destination: None,
                    revision: Some(revision),
                });
            }
            self.writes
                .lock()
                .unwrap()
                .push((binding.clone(), cursor.clone(), request.clone()));
            Ok(OperationStatus {
                handle: OperationHandle {
                    preparation_id: OperationId::new(REMOTE_OPERATION).unwrap(),
                    invocation_id: None,
                    execution_id: None,
                    expires_at_unix_ms: None,
                },
                state: OperationState::Completed {
                    result: MutationResult {
                        committed: true,
                        rolled_back: false,
                        atomic_across_files: true,
                        results,
                    },
                    side_effects_possible: true,
                },
                progress: Vec::new(),
                progress_metadata: SequenceMetadata {
                    first_retained_sequence: None,
                    next_sequence: 0,
                    gap_before_first: false,
                },
            })
        }
        async fn status(
            &self,
            _: &SessionWorkspaceBinding,
            _: &WorkspaceCursor,
            _: &OperationHandle,
        ) -> Result<OperationStatus<MutationResult>, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }
        async fn cancel(
            &self,
            _: &SessionWorkspaceBinding,
            _: &WorkspaceCursor,
            _: &OperationHandle,
        ) -> Result<CancellationResult, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }
    }

    #[async_trait]
    impl WorkspaceSearchService for SourceRemoteFiles {
        async fn search(
            &self,
            _: &SessionWorkspaceBinding,
            _: &WorkspaceCursor,
            _: &SearchRequest,
        ) -> Result<SearchPage, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }
    }

    fn source_remote_workspace() -> (
        WorkspaceSession,
        Arc<SourceRemoteFiles>,
        flume::Receiver<()>,
    ) {
        let base = remote_workspace_session();
        let (write_started, writes) = flume::bounded(1);
        let service = Arc::new(SourceRemoteFiles {
            files: Mutex::new(
                [SOURCE_FILE, REMOTE_OTHER_FILE]
                    .into_iter()
                    .map(|name| {
                        (
                            name.into(),
                            (
                                REMOTE_CONTENT.into(),
                                ResourceRevision::new(REMOTE_REVISION).unwrap(),
                            ),
                        )
                    })
                    .collect(),
            ),
            writes: Mutex::new(Vec::new()),
            write_gate: Mutex::new(None),
            write_started,
        });
        let handle = WorkspaceHandle::new(
            base.binding().authority().clone(),
            WorkspaceCapabilities::new([
                WorkspaceCapability::Resolve,
                WorkspaceCapability::List,
                WorkspaceCapability::ReadBytes,
                WorkspaceCapability::MutationExecute,
                WorkspaceCapability::Search,
            ]),
            WorkspaceServices {
                read: Some(service.clone()),
                mutation: Some(service.clone()),
                search: Some(service.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        (
            WorkspaceSession::new(handle, base.binding().clone(), base.cursor().clone()).unwrap(),
            service,
            writes,
        )
    }

    fn source_screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(SOURCE_WIDTH, SOURCE_HEIGHT)).unwrap();
        terminal.draw(|frame| app.view(frame)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn settle_source_workbenches(app: &mut App) {
        let deadline = Instant::now() + super::PERMISSION_WRITE_TIMEOUT;
        while app.workbench.is_busy()
            || app
                .parked_workbench
                .as_ref()
                .is_some_and(|workbench| workbench.is_busy())
        {
            assert!(Instant::now() < deadline, "{REMOTE_TIMEOUT}");
            let _ = app.tick_workbench();
            thread::yield_now();
        }
    }

    fn source_key(app: &mut App, binding: workbench_keys::Bind) {
        app.update(Msg::Key(KeyEvent::new(binding.code, binding.modifiers)));
    }

    struct NeverExecute;

    impl Tool for NeverExecute {
        fn name(&self) -> &str {
            TOOL
        }
        fn description(&self, _: &DescriptionContext) -> Cow<'_, str> {
            panic!("{NEVER_EXECUTE}")
        }
        fn schema(&self) -> Value {
            panic!("{NEVER_EXECUTE}")
        }
        fn parse(&self, _: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            panic!("{NEVER_EXECUTE}")
        }
    }

    struct Authority(AuthorityCatalog);

    impl PermissionAuthorityProvider for Authority {
        fn acquire(
            &self,
            _: &Path,
        ) -> Result<Box<dyn PermissionAuthorityLease + '_>, PermissionEditError> {
            Ok(Box::new(Authority(self.0.clone())))
        }
    }

    impl PermissionAuthorityLease for Authority {
        fn catalog(&self) -> &AuthorityCatalog {
            &self.0
        }
    }

    #[derive(Clone)]
    struct ExampleAuthority {
        catalog: AuthorityCatalog,
        inputs: Arc<Mutex<Vec<Value>>>,
    }

    impl PermissionAuthorityProvider for ExampleAuthority {
        fn acquire(
            &self,
            _: &Path,
        ) -> Result<Box<dyn PermissionAuthorityLease + '_>, PermissionEditError> {
            Ok(Box::new(self.clone()))
        }
    }

    impl PermissionAuthorityLease for ExampleAuthority {
        fn catalog(&self) -> &AuthorityCatalog {
            &self.catalog
        }
        fn analyze_example(
            &self,
            _: &EditableAuthorityDescriptor,
            input: &Value,
        ) -> Result<PermissionExampleAnalysis, PermissionEditError> {
            self.inputs.lock().unwrap().push(input.clone());
            Err(PermissionEditError::Unavailable(EXAMPLE_UNAVAILABLE.into()))
        }
    }

    #[derive(Clone)]
    struct TemplateAuthority {
        catalog: AuthorityCatalog,
        analysis: TemplateAnalysis,
        sources: Arc<Mutex<Vec<TemplateSource>>>,
        refuse: bool,
    }

    impl PermissionAuthorityProvider for TemplateAuthority {
        fn acquire(
            &self,
            _: &Path,
        ) -> Result<Box<dyn PermissionAuthorityLease + '_>, PermissionEditError> {
            Ok(Box::new(self.clone()))
        }
    }

    impl PermissionAuthorityLease for TemplateAuthority {
        fn catalog(&self) -> &AuthorityCatalog {
            &self.catalog
        }

        fn analyze_template(
            &self,
            _: &EditableAuthorityDescriptor,
            source: &TemplateSource,
        ) -> Result<TemplateAnalysis, PermissionEditError> {
            self.sources.lock().unwrap().push(source.clone());
            if self.refuse {
                Err(PermissionEditError::Unavailable(SEED_REFUSED.into()))
            } else {
                Ok(self.analysis.clone())
            }
        }
    }

    fn authority() -> Arc<Authority> {
        let registered = RegisteredTool {
            tool: Arc::new(NeverExecute),
            source: ToolSource::Native {
                owner: OWNER.into(),
                contract: CONTRACT.into(),
                trusted: true,
            },
            effect: ToolEffect::Unknown,
        };
        Arc::new(Authority(AuthorityCatalog {
            revision: CONTRACT.into(),
            authorities: vec![EditableAuthorityDescriptor {
                key: TOOL.into(),
                source: TrustedToolSource::from_registered(&registered, None).unwrap(),
                resources: vec![ResourceCapability {
                    kind: PermissionResourceKind::Command,
                    selectors: vec![SelectorMode::CommandPattern],
                    access: vec![PermissionResourceAccess::Execute],
                    wildcard_access: false,
                    wildcard_protection: false,
                    attributes: Default::default(),
                }],
                arguments: vec![ArgumentMode::Unconstrained],
                families: Vec::new(),
                unrestricted_resources: false,
                unavailable: None,
            }],
        }))
    }

    fn durable_app() -> App {
        let mut app = test_app();
        app.permissions = Arc::new(PermissionManager::new_persistent_in(
            PermissionsConfig::default(),
            app.permissions.project_cwd(),
            Arc::default(),
            app.storage.clone(),
        ));
        app.permissions
            .set_permission_authority_provider(authority());
        let mut session = app.state.session.as_ref().clone();
        app.permission_snapshot = Some(
            attach_session_permissions(
                &app.storage,
                &app.storage_writer,
                &mut session,
                &app.permissions,
            )
            .unwrap(),
        );
        app.state.session = Arc::new(session);
        app.permission_ui = PermissionUi::default();
        app.open_permissions_picker().unwrap();
        app.finish_permission_jobs();
        app
    }

    fn draft(lifetime: PermissionLifetime) -> PermissionRuleDraft {
        PermissionRuleDraft {
            identity: IdentityDraft::Registered {
                key: TOOL.into(),
                family: None,
            },
            effect: StructuredPermissionEffect::Allow,
            project: if lifetime == PermissionLifetime::Project {
                ProjectDraft::Current
            } else {
                ProjectDraft::None
            },
            lifetime,
            resources: ResourcesDraft::Constrained(vec![ResourceDraft {
                original_index: None,
                kind: PermissionResourceKind::Command,
                selector: SelectorDraft::Replace(SelectorValue::CommandPattern(COMMAND.into())),
                access: GuardDraft::Equals(PermissionResourceAccess::Execute),
                protected: GuardDraft::Equals(false),
                attributes: Default::default(),
            }]),
            arguments: ArgumentsDraft::Unconstrained,
            label: Some(LABEL.into()),
        }
    }

    fn preview(app: &mut App, draft: PermissionRuleDraft) {
        let session = app.permission_ui.session.as_ref().unwrap();
        let revision = app
            .permissions_picker
            .editor_mut()
            .unwrap()
            .revision
            .saturating_add(1);
        let mut editor = ScopeEditor::new(draft.clone(), session.catalog().clone(), None);
        editor.revision = revision;
        app.permissions_picker.set_editor(editor);
        app.handle_permission_editor(EditorEvent::Preview {
            revision,
            draft: Box::new(draft),
        });
        app.finish_permission_jobs();
    }

    fn begin(app: &mut App, launch: EditorLaunch) {
        app.handle_permission_editor(EditorEvent::Begin(launch));
        app.finish_permission_jobs();
    }

    fn seed_app(refuse: bool) -> (App, Arc<TemplateAuthority>) {
        let mut app = durable_app();
        let mut catalog = authority().0.clone();
        let capability = &mut catalog.authorities[0].resources[0];
        capability.selectors.push(SelectorMode::CommandTemplate);
        capability
            .attributes
            .insert(WORKDIR_ATTRIBUTE.into(), vec![SelectorMode::Exact]);
        let mut context = pattern_suggestion_candidate(&app.permissions.project_cwd())
            .definition
            .context;
        context.tool_identity = TOOL.into();
        context.executable_identity = SEED_ARGV[0].into();
        let host = Arc::new(TemplateAuthority {
            catalog,
            analysis: TemplateAnalysis {
                context,
                argv: SEED_ARGV.into_iter().map(str::to_owned).collect(),
                roles: vec![
                    ArgumentRole::Executable,
                    ArgumentRole::Operation,
                    ArgumentRole::Data,
                    ArgumentRole::Data,
                ],
                option_like_data: BTreeSet::new(),
            },
            sources: Arc::default(),
            refuse,
        });
        app.permissions
            .set_permission_authority_provider(host.clone());
        app.open_permissions_picker().unwrap();
        app.finish_permission_jobs();
        begin(&mut app, EditorLaunch::New);
        (app, host)
    }

    fn click_editor_label(app: &mut App, label: &str) -> Option<EditorEvent> {
        let mut terminal = Terminal::new(TestBackend::new(EDITOR_WIDTH, EDITOR_HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                app.permissions_picker.view(frame, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (column, row) = (buffer.area.y..buffer.area.bottom())
            .find_map(|row| {
                let line: String = (buffer.area.x..buffer.area.right())
                    .map(|column| buffer[(column, row)].symbol())
                    .collect();
                line.find(label)
                    .map(|column| (buffer.area.x + line[..column].width() as u16, row))
            })
            .unwrap_or_else(|| panic!("Missing editor control {label}: {}", buffer_text(buffer)));
        let event = |kind| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            app.permissions_picker
                .handle_mouse(event(MouseEventKind::Down(MouseButton::Left))),
            PermissionsPickerAction::Consumed
        ));
        match app
            .permissions_picker
            .handle_mouse(event(MouseEventKind::Up(MouseButton::Left)))
        {
            PermissionsPickerAction::Consumed => None,
            PermissionsPickerAction::Editor(event) => Some(event),
            _ => panic!("unexpected picker action"),
        }
    }

    fn enter_editor_text(app: &mut App, label: &str, text: &str) {
        assert!(click_editor_label(app, label).is_none());
        let editor = app.permissions_picker.editor_mut().unwrap();
        assert!(editor.is_editing());
        editor.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        editor.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        editor.handle_paste(text);
        assert!(editor.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert!(!editor.is_editing());
    }

    fn seed_from_new_controls(app: &mut App, name: &str) -> EditorEvent {
        for label in [
            "Registered target",
            "[Targets]",
            "Add target",
            "ALL OF access",
            "ALL OF protection",
            "[Arguments]",
            "Whole-rule input",
            "[Targets]",
            "Command template",
        ] {
            assert!(click_editor_label(app, label).is_none());
        }
        enter_editor_text(app, "Source for analysis", SEED_COMMAND);
        let workdir = app.permissions.project_cwd().to_string_lossy().into_owned();
        enter_editor_text(app, "Analysis workdir", &workdir);
        enter_editor_text(app, "Template name", name);
        let event = click_editor_label(app, SEED_ACTION).unwrap();
        assert!(matches!(&event, EditorEvent::Seed { .. }));
        event
    }

    fn save(app: &mut App) {
        let validated = app.permission_ui.validated.as_ref().unwrap();
        app.handle_permission_editor(EditorEvent::Save {
            revision: validated.revision,
            acknowledged: validated.preview.requirements().clone(),
        });
        app.finish_permission_jobs();
    }

    #[test_case(PermissionLifetime::Conversation; "conversation")]
    #[test_case(PermissionLifetime::Project; "project")]
    #[test_case(PermissionLifetime::Global; "global")]
    fn new_permission_requires_current_review_and_durable_ack(lifetime: PermissionLifetime) {
        let mut app = durable_app();
        let before = app.permissions.structured_rule_inventory().unwrap().len();
        begin(&mut app, EditorLaunch::New);
        preview(&mut app, draft(lifetime));
        let validated = app.permission_ui.validated.as_ref().unwrap();
        assert!(!validated.preview.requirements().is_empty());
        app.handle_permission_editor(EditorEvent::Save {
            revision: validated.revision,
            acknowledged: BTreeSet::new(),
        });
        app.finish_permission_jobs();
        assert_eq!(
            app.permissions.structured_rule_inventory().unwrap().len(),
            before
        );
        let current = app.permissions_picker.editor_mut().unwrap().draft().clone();
        preview(&mut app, current);
        save(&mut app);
        assert_eq!(
            app.permissions.structured_rule_inventory().unwrap().len(),
            before + 1
        );
        assert_eq!(app.status_bar.flash_text(), Some(PERMISSION_SAVED));
        app.checkpoint_now();
        app.storage_writer
            .save_sync(Arc::clone(&app.state.session))
            .unwrap();
        let snapshot = app.permissions.conversation_permission_snapshot().unwrap();
        assert_eq!(
            app.state.session.meta.permission_generation,
            snapshot.revision.generation
        );
        assert_eq!(
            app.state.session.meta.structured_permission_rules,
            snapshot.records
        );
    }

    #[test_case(false; "revision_changed")]
    #[test_case(true; "project_changed")]
    fn stale_preview_reply_never_validates_a_different_draft(project_changed: bool) {
        let mut app = durable_app();
        begin(&mut app, EditorLaunch::New);
        let draft = draft(PermissionLifetime::Conversation);
        let catalog = app
            .permission_ui
            .session
            .as_ref()
            .unwrap()
            .catalog()
            .clone();
        app.permissions_picker
            .set_editor(ScopeEditor::new(draft.clone(), catalog, None));
        app.handle_permission_editor(EditorEvent::Preview {
            revision: 0,
            draft: Box::new(draft),
        });
        if project_changed {
            app.permissions.set_project(&app.permissions.project_cwd());
        } else {
            app.permissions_picker.editor_mut().unwrap().revision += 1;
        }
        app.finish_permission_jobs();
        assert!(app.permission_ui.validated.is_none());
    }

    #[test_case(PermissionLifetime::Project; "move_to_project")]
    #[test_case(PermissionLifetime::Global; "move_to_global")]
    fn edit_replaces_conversation_rule_and_checkpoint_cannot_resurrect_it(
        lifetime: PermissionLifetime,
    ) {
        let mut app = durable_app();
        begin(&mut app, EditorLaunch::New);
        preview(&mut app, draft(PermissionLifetime::Conversation));
        save(&mut app);
        let record = app
            .permissions
            .structured_conversation_rules_snapshot()
            .into_iter()
            .find(|record| record.is_active())
            .unwrap();
        begin(&mut app, EditorLaunch::Edit(Arc::new(record.clone())));
        let mut replacement = draft(lifetime);
        replacement.label = Some(NEXT_LABEL.into());
        preview(&mut app, replacement);
        save(&mut app);
        app.checkpoint_now();
        app.storage_writer
            .save_sync(Arc::clone(&app.state.session))
            .unwrap();
        let database = SessionDatabase::open_read_only(&app.storage).unwrap();
        let conversation = database
            .permission_snapshot(PermissionOwner::Conversation(app.state.session.id))
            .unwrap();
        assert!(
            !conversation
                .records
                .iter()
                .any(|rule| rule.id == record.id && rule.is_active())
        );
        assert!(
            app.permissions
                .structured_rule_inventory()
                .unwrap()
                .iter()
                .any(|rule| rule.label.as_deref() == Some(NEXT_LABEL) && rule.is_active())
        );
    }

    struct FailingPublication(SessionPermissionPublication);

    impl PermissionPublication for FailingPublication {
        fn snapshot(&self) -> Result<PermissionSnapshot, PermissionEditError> {
            self.0.snapshot()
        }
        fn commit(
            &self,
            _: &PreparedPermissionMutation,
        ) -> Result<PermissionCommitReceipt, PermissionEditError> {
            Err(PermissionEditError::Storage(WRITE_FAILED.into()))
        }
        fn receipt(
            &self,
            _: CaudraId,
        ) -> Result<Option<PermissionCommitReceipt>, PermissionEditError> {
            Ok(None)
        }
    }

    #[test_case(false; "cancel")]
    #[test_case(true; "writer_failure")]
    fn failed_or_cancelled_edit_never_publishes(fail: bool) {
        let mut app = durable_app();
        let before = app.permissions.conversation_permission_snapshot().unwrap();
        if fail {
            app.permissions
                .attach_permission_publication(Arc::new(FailingPublication(
                    SessionPermissionPublication {
                        storage: app.storage.clone(),
                        session: app.state.session.id,
                        writer: app.storage_writer.permission_mutation_writer(),
                        snapshot: Arc::clone(app.permission_snapshot.as_ref().unwrap()),
                    },
                )))
                .unwrap();
            app.permission_ui.context = Some(app.permissions.pattern_candidate_context());
        }
        begin(&mut app, EditorLaunch::New);
        preview(&mut app, draft(PermissionLifetime::Conversation));
        if fail {
            save(&mut app);
            assert!(app.permissions_picker.editor_mut().is_some());
            assert!(app.status_bar.flash_text().unwrap().contains(WRITE_FAILED));
        } else {
            app.handle_permission_editor(EditorEvent::Cancel);
        }
        assert_eq!(
            app.permissions.conversation_permission_snapshot().unwrap(),
            before
        );
    }

    #[test_case(false; "prompt_preemption")]
    #[test_case(true; "new_session")]
    fn suspended_draft_survives_but_cannot_transfer_conversation_authority(new_session: bool) {
        let mut app = durable_app();
        let old = app.state.session.id;
        begin(&mut app, EditorLaunch::New);
        preview(&mut app, draft(PermissionLifetime::Conversation));
        let expected = app.permissions_picker.editor_mut().unwrap().draft().clone();
        if new_session {
            assert!(!app.reset_session().is_empty());
        } else {
            app.suspend_permission_editor();
        }
        assert!(app.permission_ui.validated.is_none());
        assert!(app.permission_ui.suspended.is_some());
        app.open_permissions_picker().unwrap();
        app.finish_permission_jobs();
        assert_eq!(
            app.permissions_picker.editor_mut().unwrap().draft(),
            &expected
        );
        if new_session {
            assert_ne!(app.state.session.id, old);
            assert_eq!(
                app.permissions
                    .conversation_permission_snapshot()
                    .unwrap()
                    .revision
                    .owner,
                PermissionOwner::Conversation(app.state.session.id)
            );
            preview(&mut app, expected);
            assert!(app.permission_ui.validated.is_none());
        }
    }

    #[test_case((); "initial_empty_session")]
    fn empty_session_row_is_durable_before_publication_and_is_not_pruned(_: ()) {
        let mut app = durable_app();
        app.checkpoint_now();
        let database = SessionDatabase::open_read_only(&app.storage).unwrap();
        let snapshot = database
            .permission_snapshot(PermissionOwner::Conversation(app.state.session.id))
            .unwrap();
        assert!(snapshot.revision.row_present);
        assert!(snapshot.records.is_empty());
        assert_eq!(
            app.permissions.conversation_permission_snapshot(),
            Some(snapshot)
        );
    }

    #[test_case(PermissionLifetime::Project; "project_copy")]
    #[test_case(PermissionLifetime::Global; "global_copy")]
    fn copy_leaves_source_and_revoke_is_a_separate_review(lifetime: PermissionLifetime) {
        let mut app = durable_app();
        begin(&mut app, EditorLaunch::New);
        preview(&mut app, draft(PermissionLifetime::Conversation));
        save(&mut app);
        let source = Arc::new(
            app.permissions
                .structured_conversation_rules_snapshot()
                .into_iter()
                .find(|record| record.is_active())
                .unwrap(),
        );
        begin(
            &mut app,
            EditorLaunch::Copy {
                source: Arc::clone(&source),
                draft: None,
            },
        );
        preview(&mut app, draft(lifetime));
        save(&mut app);
        assert!(
            app.permissions
                .structured_conversation_rules_snapshot()
                .iter()
                .any(|record| record.id == source.id && record.is_active())
        );
        app.request_permission_revoke(source.id.clone());
        app.finish_permission_jobs();
        let draft = app.permissions_picker.editor_mut().unwrap().draft().clone();
        preview(&mut app, draft);
        assert!(
            app.permissions
                .structured_conversation_rules_snapshot()
                .iter()
                .any(|record| record.id == source.id && record.is_active())
        );
        save(&mut app);
        assert!(
            !app.permissions
                .structured_conversation_rules_snapshot()
                .iter()
                .any(|record| record.id == source.id && record.is_active())
        );
    }

    #[test_case((); "discovery_is_evidence_only")]
    fn discovery_requires_current_explicit_authority_and_never_runs_the_sample(_: ()) {
        let mut app = durable_app();
        let before = app.permissions.conversation_permission_snapshot().unwrap();
        let candidate = pattern_suggestion_candidate(&app.permissions.project_cwd());
        begin(&mut app, EditorLaunch::Discover(Arc::new(candidate)));
        let proposed = app.permissions_picker.editor_mut().unwrap().draft().clone();
        preview(&mut app, proposed);
        assert!(app.permission_ui.validated.is_none());
        assert_eq!(
            app.permissions.conversation_permission_snapshot().unwrap(),
            before
        );
        preview(&mut app, draft(PermissionLifetime::Conversation));
        assert_eq!(
            app.permissions.conversation_permission_snapshot().unwrap(),
            before
        );
        save(&mut app);
        assert_eq!(
            app.permissions
                .structured_conversation_rules_snapshot()
                .iter()
                .filter(|record| record.is_active())
                .count(),
            1
        );
    }

    #[test_case(false; "verified_source_save_preserves_dirty_remote_tabs_and_pending_save")]
    #[test_case(true; "changed_source_refuses_without_rebinding_dirty_remote_tabs")]
    fn local_policy_source_round_trip_preserves_remote_identity_and_policy(changed_source: bool) {
        let directory = TempDir::new().unwrap();
        let project = directory.path().canonicalize().unwrap();
        let path = project.join(SOURCE_FILE);
        fs::write(&path, SOURCE_CONTENT).unwrap();
        let locator =
            VerifiedLocalSourceLocator::from_loaded_entrypoint(&path, SOURCE_CONTENT.as_bytes())
                .unwrap();
        let plugins = Arc::new(PluginRuleStore::default());
        let rule = PermissionRule {
            tool: ToolKey::native(TOOL),
            scope: Some(COMMAND.into()),
            effect: Effect::Deny,
        };
        plugins.replace_with_source(SOURCE_PLUGIN, vec![rule.clone()], Some(locator.clone()));
        let mut app = test_app();
        app.permissions = Arc::new(PermissionManager::new_persistent_in(
            PermissionsConfig {
                project_allow_rules: vec![PermissionRule {
                    effect: Effect::Allow,
                    ..rule.clone()
                }],
                ..Default::default()
            },
            project,
            plugins,
            app.storage.clone(),
        ));
        let (workspace, remote, write_started) = source_remote_workspace();
        let stored = StoredWorkspaceBinding::new_with_cursor(
            workspace.binding().clone(),
            workspace.cursor().clone(),
            None,
        )
        .unwrap();
        let mut session =
            AppSession::new_with_workspace(&app.state.session.model, REMOTE_CWD, stored.clone());
        app.permission_snapshot = Some(
            attach_session_permissions(
                &app.storage,
                &app.storage_writer,
                &mut session,
                &app.permissions,
            )
            .unwrap(),
        );
        app.state.session = Arc::new(session);
        app.workspace_session = Some(workspace.clone());
        app.workbench
            .bind_workspace_with_gate(workspace.clone(), MutationGate::allow())
            .unwrap();
        app.toggle_workbench();
        settle_source_workbenches(&mut app);
        for name in [SOURCE_FILE, REMOTE_OTHER_FILE] {
            app.workbench
                .open_remote_at(WorkspacePath::new(name).unwrap(), None);
            settle_source_workbenches(&mut app);
            app.update(Msg::Paste(REMOTE_EDIT.into()));
            assert!(source_screen(&mut app).contains(REMOTE_EDIT));
        }
        assert!(app.workbench.blocks_workspace_change());
        let policy_before: Vec<_> = app
            .permissions
            .active_policy()
            .into_iter()
            .map(|entry| {
                (
                    entry.source,
                    entry.rule,
                    entry.verified_local_source_locator,
                )
            })
            .collect();
        let permission_before = app.permissions.conversation_permission_snapshot().unwrap();
        let rules_before = app.permissions.structured_rule_inventory().unwrap();
        let context_before = app.permissions.pattern_candidate_context();
        assert!(!app.permissions.project_permission_config_trusted());

        if changed_source {
            fs::write(&path, SOURCE_CHANGE).unwrap();
            app.handle_permissions_picker_action(PermissionsPickerAction::EditSource(locator));
            app.finish_permission_jobs();
            assert!(app.parked_workbench.is_none());
            assert!(app.workbench.blocks_workspace_change());
            assert!(source_screen(&mut app).contains(REMOTE_EDIT));
            assert!(remote.writes.lock().unwrap().is_empty());
            assert_eq!(
                app.permissions.conversation_permission_snapshot().unwrap(),
                permission_before
            );
            return;
        }

        let (release, held) = flume::bounded(1);
        *remote.write_gate.lock().unwrap() = Some(held);
        source_key(&mut app, workbench_keys::SAVE);
        write_started
            .recv_timeout(super::PERMISSION_WRITE_TIMEOUT)
            .unwrap();
        assert!(app.workbench.is_busy());
        app.handle_permissions_picker_action(PermissionsPickerAction::EditSource(locator));
        app.finish_permission_jobs();
        assert!(app.parked_workbench.as_ref().unwrap().is_busy());
        assert!(
            app.parked_workbench
                .as_ref()
                .unwrap()
                .blocks_workspace_change()
        );
        let local = source_screen(&mut app);
        assert!(local.contains(SOURCE_CONTENT.trim()));
        assert!(local.contains(super::LOCAL_SOURCE_NOTICE));
        assert!(!local.contains(REMOTE_EDIT));
        app.update(Msg::Paste(SOURCE_CHANGE.into()));
        source_key(&mut app, workbench_keys::SAVE);
        let saved_local = format!("{SOURCE_CHANGE}{SOURCE_CONTENT}");
        assert_eq!(fs::read_to_string(&path).unwrap(), saved_local);
        assert!(remote.writes.lock().unwrap().is_empty());
        assert!(!app.permissions.project_permission_config_trusted());
        assert_eq!(app.permissions.pattern_candidate_context(), context_before);
        assert_eq!(
            app.permissions.conversation_permission_snapshot().unwrap(),
            permission_before
        );
        assert_eq!(
            app.permissions.structured_rule_inventory().unwrap(),
            rules_before
        );
        assert_eq!(
            app.permissions
                .active_policy()
                .into_iter()
                .map(|entry| (
                    entry.source,
                    entry.rule,
                    entry.verified_local_source_locator
                ))
                .collect::<Vec<_>>(),
            policy_before
        );

        release.send(()).unwrap();
        settle_source_workbenches(&mut app);
        assert!(!app.parked_workbench.as_ref().unwrap().is_busy());
        assert!(
            app.parked_workbench
                .as_ref()
                .unwrap()
                .blocks_workspace_change()
        );
        source_key(&mut app, workbench_keys::CLOSE);
        assert!(app.parked_workbench.is_none());
        assert!(app.workbench.is_open());
        assert!(source_screen(&mut app).contains(REMOTE_EDIT));
        assert_eq!(app.state.session.workspace_binding(), Some(&stored));
        assert_eq!(
            app.workspace_session.as_ref().unwrap().binding(),
            workspace.binding()
        );
        assert_eq!(
            app.workspace_session.as_ref().unwrap().cursor(),
            workspace.cursor()
        );
        source_key(&mut app, workbench_keys::PREV_TAB);
        assert!(source_screen(&mut app).contains(SOURCE_FILE));
        assert!(app.workbench.blocks_workspace_change());
        source_key(&mut app, workbench_keys::UNDO);
        assert!(!source_screen(&mut app).contains(REMOTE_EDIT));
        source_key(&mut app, workbench_keys::REDO);
        assert!(source_screen(&mut app).contains(REMOTE_EDIT));
        source_key(&mut app, workbench_keys::SAVE);
        settle_source_workbenches(&mut app);
        assert!(!app.workbench.blocks_workspace_change());
        let writes = remote.writes.lock().unwrap();
        assert_eq!(writes.len(), [SOURCE_FILE, REMOTE_OTHER_FILE].len());
        for (binding, cursor, request) in writes.iter() {
            assert_eq!(binding, workspace.binding());
            assert_eq!(cursor, workspace.cursor());
            let [
                Mutation::Write {
                    condition,
                    content: WriteContent::Text(content),
                    ..
                },
            ] = request.mutations.as_slice()
            else {
                panic!("{NEVER_EXECUTE}")
            };
            assert_eq!(
                condition,
                &MutationCondition::Matches(ResourceRevision::new(REMOTE_REVISION).unwrap())
            );
            assert_eq!(content, &format!("{REMOTE_EDIT}{REMOTE_CONTENT}"));
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), saved_local);
    }

    #[test_case("unchanged", false; "same_trusted_facts_keep_preview")]
    #[test_case("binding", true; "session_binding_changed")]
    #[test_case("principal", true; "authenticated_principal_changed")]
    #[test_case("cursor", true; "cursor_generation_changed")]
    #[test_case("capabilities", true; "negotiated_capabilities_changed")]
    #[test_case("backend", true; "new_handle_with_identical_labels")]
    #[test_case("filter", true; "effective_tool_filter_changed_without_model_spec_change")]
    #[test_case("registry", true; "registered_capability_changed")]
    #[test_case("read_only", true; "strict_read_only_filter_changed")]
    #[test_case("disconnected", true; "connection_became_unavailable")]
    #[test_case("reconnect", true; "explicit_reconnect_invalidates_before_network_work")]
    fn authority_context_changes_invalidate_backend_seals_and_preserve_drafts(
        change: &str,
        invalidated: bool,
    ) {
        let mut app = durable_app();
        let (workspace, service, _) = source_remote_workspace();
        app.workspace_session = Some(workspace.clone());
        let filter_changed = Arc::new(AtomicBool::new(false));
        let available = Arc::new(AtomicBool::new(true));
        let read_only = Arc::new(AtomicBool::new(false));
        let registry_revision = Arc::new(AtomicU64::new(REGISTRY_REVISION));
        app.permission_authority_factory = Some({
            let filter_changed = Arc::clone(&filter_changed);
            let available = Arc::clone(&available);
            let read_only = Arc::clone(&read_only);
            let registry_revision = Arc::clone(&registry_revision);
            Arc::new(move |_, _, _, _| {
                let tool_filter = if filter_changed.load(Ordering::Acquire) {
                    ToolFilter::AllExcept(vec![TOOL.into()])
                } else {
                    ToolFilter::All
                };
                Ok(PermissionAuthorityBinding {
                    provider: authority(),
                    tool_filter: if read_only.load(Ordering::Acquire) {
                        ToolFilter::ReadOnly(Box::new(tool_filter))
                    } else {
                        tool_filter
                    },
                    available: available.load(Ordering::Acquire),
                    registry_revision: registry_revision.load(Ordering::Acquire),
                })
            })
        });
        app.sync_permission_authority().unwrap();
        app.open_permissions_picker().unwrap();
        app.finish_permission_jobs();
        begin(&mut app, EditorLaunch::New);
        let expected = draft(PermissionLifetime::Conversation);
        preview(&mut app, expected.clone());
        let sealed = Arc::clone(&app.permission_ui.validated.as_ref().unwrap().preview);
        let context_before = app.permissions.pattern_candidate_context();
        let policy_before = app.permissions.conversation_permission_snapshot().unwrap();
        match change {
            "binding" | "principal" => {
                let binding = SessionWorkspaceBinding::new(
                    if change == "binding" {
                        SessionBindingId::new(OTHER_BINDING).unwrap()
                    } else {
                        workspace.binding().binding_id().clone()
                    },
                    workspace.binding().authority().clone(),
                    if change == "principal" {
                        AuthenticatedPrincipalId::new(
                            workspace.binding().authority().clone(),
                            OTHER_PRINCIPAL,
                        )
                        .unwrap()
                    } else {
                        workspace.binding().principal().clone()
                    },
                    workspace.binding().project().clone(),
                )
                .unwrap();
                let cursor = WorkspaceCursor::new(
                    &binding,
                    workspace.cursor().scope().clone(),
                    workspace.cursor().generation(),
                    CwdHandle::new(OTHER_CURSOR).unwrap(),
                );
                app.workspace_session = Some(
                    WorkspaceSession::new(workspace.workspace().clone(), binding, cursor).unwrap(),
                );
            }
            "cursor" => {
                let cursor = WorkspaceCursor::new(
                    workspace.binding(),
                    workspace.cursor().scope().clone(),
                    workspace.cursor().generation() + 1,
                    CwdHandle::new(OTHER_CURSOR).unwrap(),
                );
                app.workspace_session = Some(
                    WorkspaceSession::new(
                        workspace.workspace().clone(),
                        workspace.binding().clone(),
                        cursor,
                    )
                    .unwrap(),
                );
            }
            "capabilities" | "backend" => {
                let capabilities = if change == "capabilities" {
                    WorkspaceCapabilities::default()
                } else {
                    workspace.workspace().capabilities().clone()
                };
                let handle = WorkspaceHandle::new(
                    workspace.binding().authority().clone(),
                    capabilities,
                    WorkspaceServices {
                        read: Some(service.clone()),
                        mutation: Some(service.clone()),
                        search: Some(service.clone()),
                        ..Default::default()
                    },
                )
                .unwrap();
                app.workspace_session = Some(
                    WorkspaceSession::new(
                        handle,
                        workspace.binding().clone(),
                        workspace.cursor().clone(),
                    )
                    .unwrap(),
                );
            }
            "filter" => filter_changed.store(true, Ordering::Release),
            "registry" => {
                registry_revision.fetch_add(1, Ordering::AcqRel);
            }
            "read_only" => read_only.store(true, Ordering::Release),
            "disconnected" => available.store(false, Ordering::Release),
            "reconnect" => app.invalidate_permission_authority(),
            "unchanged" => {}
            _ => panic!("{NEVER_EXECUTE}"),
        }
        app.sync_permission_authority().unwrap();
        assert_eq!(
            app.permissions.pattern_candidate_context() != context_before,
            invalidated
        );
        assert_eq!(app.permission_ui.validated.is_none(), invalidated);
        if invalidated {
            assert_eq!(
                app.permission_ui
                    .suspended
                    .as_mut()
                    .unwrap()
                    .editor_mut()
                    .unwrap()
                    .draft(),
                &expected
            );
            let confirmation = sealed.confirm(sealed.requirements()).unwrap();
            assert!(matches!(
                app.permissions
                    .commit_permission_edit(&sealed, &confirmation, &expected),
                Err(PermissionEditError::Conflict)
            ));
        }
        assert_eq!(
            app.permissions.conversation_permission_snapshot().unwrap(),
            policy_before
        );
    }

    #[test_case(json!({"command": "git show alpha", "workdir": "/different", "optional": null}); "json_workdir_and_null_are_forwarded_unchanged")]
    #[test_case(json!({"command": "git show alpha"}); "absent_fields_are_not_invented")]
    fn example_test_uses_typed_input_without_a_separate_workdir(input: Value) {
        let mut app = durable_app();
        let inputs = Arc::new(Mutex::new(Vec::new()));
        app.permissions
            .set_permission_authority_provider(Arc::new(ExampleAuthority {
                catalog: authority().0.clone(),
                inputs: Arc::clone(&inputs),
            }));
        app.open_permissions_picker().unwrap();
        app.finish_permission_jobs();
        begin(&mut app, EditorLaunch::New);
        preview(&mut app, draft(PermissionLifetime::Conversation));
        let before = app.permissions.conversation_permission_snapshot().unwrap();
        let revision = app.permission_ui.validated.as_ref().unwrap().revision;
        app.handle_permission_editor(EditorEvent::Test {
            revision,
            test_revision: 0,
            example: EditorTestExample {
                authority_key: TOOL.into(),
                input: input.clone(),
            },
        });
        app.finish_permission_jobs();
        assert_eq!(*inputs.lock().unwrap(), vec![input]);
        assert_eq!(
            app.permissions.conversation_permission_snapshot().unwrap(),
            before
        );
    }

    #[test_case(false, false; "filter_changed_before_save_worker")]
    #[test_case(true, false; "connection_lost_before_save_worker")]
    #[test_case(false, true; "filter_reply_after_discovery_preemption")]
    #[test_case(true, true; "connection_reply_after_discovery_preemption")]
    fn save_rechecks_factory_off_ui_thread_before_using_a_sealed_preview(
        disconnected: bool,
        preempt: bool,
    ) {
        let mut app = durable_app();
        let changed = Arc::new(AtomicBool::new(false));
        let ui_thread = thread::current().id();
        app.permission_authority_factory = Some({
            let changed = Arc::clone(&changed);
            Arc::new(move |_, _, _, _| {
                assert_ne!(thread::current().id(), ui_thread);
                let changed = changed.load(Ordering::Acquire);
                Ok(PermissionAuthorityBinding {
                    provider: authority(),
                    tool_filter: if changed && !disconnected {
                        ToolFilter::AllExcept(vec![TOOL.into()])
                    } else {
                        ToolFilter::All
                    },
                    available: !changed || !disconnected,
                    registry_revision: REGISTRY_REVISION,
                })
            })
        });
        app.sync_permission_authority().unwrap();
        app.open_permissions_picker().unwrap();
        app.finish_permission_jobs();
        begin(&mut app, EditorLaunch::New);
        let draft = draft(PermissionLifetime::Conversation);
        preview(&mut app, draft.clone());
        let before = app.permissions.conversation_permission_snapshot().unwrap();
        let validated = app.permission_ui.validated.as_ref().unwrap();
        let event = EditorEvent::Save {
            revision: validated.revision,
            acknowledged: validated.preview.requirements().clone(),
        };
        changed.store(true, Ordering::Release);
        app.handle_permission_editor(event);
        if preempt {
            let pending = app.permission_ui.pending.as_mut().unwrap();
            let result = pending
                .reply
                .recv_timeout(super::PERMISSION_WRITE_TIMEOUT)
                .unwrap();
            let (sender, receiver) = flume::bounded(1);
            assert!(sender.send(result).is_ok());
            pending.reply = receiver;
            app.sync_pattern_discovery_context();
        }
        app.finish_permission_jobs();
        assert!(app.permission_ui.validated.is_none());
        assert_eq!(
            app.permission_ui
                .suspended
                .as_mut()
                .unwrap()
                .editor_mut()
                .unwrap()
                .draft(),
            &draft
        );
        assert_eq!(
            app.permissions.conversation_permission_snapshot().unwrap(),
            before
        );
        let context = app.permission_ui.authority_context.as_ref().unwrap();
        assert_eq!(context.binding.available, !disconnected);
        assert_eq!(context.binding.tool_filter.matches(TOOL), disconnected);
    }

    #[test_case(LABEL; "named_literal_template")]
    #[test_case(NEXT_LABEL; "another_template_name")]
    fn first_template_from_new_controls_requires_review_before_durable_commit(name: &str) {
        let (mut app, host) = seed_app(false);
        let before = app.permissions.conversation_permission_snapshot().unwrap();
        let event = seed_from_new_controls(&mut app, name);
        let EditorEvent::Seed {
            source, revision, ..
        } = &event
        else {
            panic!("expected seed");
        };
        let source = source.clone();
        let seed_revision = *revision;
        app.handle_permission_editor(event);
        app.finish_permission_jobs();
        assert!(app.permission_ui.validated.is_none());
        let draft = app.permissions_picker.editor_mut().unwrap().draft().clone();
        let ResourcesDraft::Constrained(resources) = &draft.resources else {
            panic!("expected command target");
        };
        let SelectorDraft::Replace(SelectorValue::CommandTemplate {
            definition,
            source: retained,
        }) = &resources[0].selector
        else {
            panic!("expected host seed");
        };
        assert_eq!(retained.as_ref(), Some(&source));
        assert_eq!(definition.name, name);
        assert_eq!(definition.context, host.analysis.context);
        assert!(definition.slots.is_empty());
        assert_eq!(definition.combinations, SlotCombinations::Independent);
        assert_eq!(
            definition.argv,
            host.analysis
                .argv
                .iter()
                .zip(&host.analysis.roles)
                .map(|(value, role)| PatternToken::Exact {
                    value: value.clone(),
                    role: role.clone()
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(
            resources[0].attributes.get(WORKDIR_ATTRIBUTE),
            Some(&SelectorDraft::Replace(SelectorValue::Exact(
                definition.context.effective_workdir.clone()
            )))
        );
        assert!(app.permissions_picker.editor_mut().unwrap().revision() > seed_revision);
        assert_eq!(
            app.permissions.conversation_permission_snapshot().unwrap(),
            before
        );
        assert!(click_editor_label(&mut app, SAVE_ACTION).is_none());
        let preview = click_editor_label(&mut app, PREVIEW_ACTION).unwrap();
        app.handle_permission_editor(preview);
        app.finish_permission_jobs();
        assert!(app.permission_ui.validated.is_some());
        assert!(click_editor_label(&mut app, SAVE_ACTION).is_none());
        assert_eq!(
            app.permissions.conversation_permission_snapshot().unwrap(),
            before
        );
        assert!(click_editor_label(&mut app, CONFIRM_ACTION).is_none());
        let save = click_editor_label(&mut app, SAVE_ACTION).unwrap();
        app.handle_permission_editor(save);
        app.finish_permission_jobs();
        let snapshot = app.permissions.conversation_permission_snapshot().unwrap();
        assert_eq!(snapshot.records.len(), before.records.len() + 1);
        assert_eq!(
            snapshot.records.last().unwrap().rule.resources[0].selector,
            PermissionResourceSelector::CommandTemplate {
                definition: definition.clone()
            }
        );
        assert!(app.permissions.pattern_proposal_inventory().2.is_empty());
        let analyzed = host.sources.lock().unwrap();
        assert!(analyzed.len() >= 2);
        assert!(analyzed.iter().all(|analyzed| analyzed == &source));
    }

    #[test_case(false; "host_refusal")]
    #[test_case(true; "empty_template_name")]
    fn seed_failure_retains_source_and_can_be_retried(invalid_name: bool) {
        let (mut app, _host) = seed_app(!invalid_name);
        let before = app.permissions.conversation_permission_snapshot().unwrap();
        let name = if invalid_name { "" } else { LABEL };
        let event = seed_from_new_controls(&mut app, name);
        let EditorEvent::Seed { source, .. } = &event else {
            panic!("expected seed");
        };
        let source = source.clone();
        let draft = app.permissions_picker.editor_mut().unwrap().draft().clone();
        app.handle_permission_editor(event);
        app.finish_permission_jobs();
        assert_eq!(app.permissions_picker.editor_mut().unwrap().draft(), &draft);
        assert!(app.permission_ui.validated.is_none());
        assert!(app.status_bar.flash_text().is_some());
        if !invalid_name {
            assert!(app.status_bar.flash_text().unwrap().contains(SEED_REFUSED));
        }
        assert!(
            matches!(click_editor_label(&mut app, SEED_ACTION), Some(EditorEvent::Seed { source: retained, .. }) if retained == source)
        );
        assert_eq!(
            app.permissions.conversation_permission_snapshot().unwrap(),
            before
        );
    }

    #[test_case(false; "changed_revision")]
    #[test_case(true; "suspended_editor")]
    fn completed_seed_reply_cannot_apply_after_preemption(suspend: bool) {
        let (mut app, _host) = seed_app(false);
        let event = seed_from_new_controls(&mut app, LABEL);
        let draft = app.permissions_picker.editor_mut().unwrap().draft().clone();
        let before = app.permissions.conversation_permission_snapshot().unwrap();
        app.handle_permission_editor(event);
        let pending = app.permission_ui.pending.as_mut().unwrap();
        let result = pending
            .reply
            .recv_timeout(super::PERMISSION_WRITE_TIMEOUT)
            .unwrap();
        assert!(matches!(
            &result,
            Ok(PermissionReply::Analyzed { result: Ok(_), .. })
        ));
        let (sender, receiver) = flume::bounded(1);
        assert!(sender.send(result).is_ok());
        pending.reply = receiver;
        if suspend {
            app.suspend_permission_editor();
        } else {
            app.permissions_picker.editor_mut().unwrap().revision += 1;
        }
        app.finish_permission_jobs();
        let editor = if suspend {
            app.permission_ui
                .suspended
                .as_mut()
                .unwrap()
                .editor_mut()
                .unwrap()
        } else {
            app.permissions_picker.editor_mut().unwrap()
        };
        assert_eq!(editor.draft(), &draft);
        assert!(app.permission_ui.validated.is_none());
        assert_eq!(
            app.permissions.conversation_permission_snapshot().unwrap(),
            before
        );
    }

    #[test_case((); "busy_worker")]
    fn seed_rejected_by_busy_worker_can_be_resubmitted(_: ()) {
        let (mut app, host) = seed_app(false);
        let event = seed_from_new_controls(&mut app, LABEL);
        assert!(app.permission_job(None, false, |_| Ok(PermissionReply::Refreshed(false))));
        app.handle_permission_editor(event);
        app.finish_permission_jobs();
        assert!(host.sources.lock().unwrap().is_empty());
        let retry = click_editor_label(&mut app, SEED_ACTION).unwrap();
        app.handle_permission_editor(retry);
        app.finish_permission_jobs();
        assert_eq!(host.sources.lock().unwrap().len(), 1);
        let ResourcesDraft::Constrained(resources) = &app
            .permissions_picker
            .editor_mut()
            .unwrap()
            .draft()
            .resources
        else {
            panic!("expected command target");
        };
        assert!(matches!(
            resources[0].selector,
            SelectorDraft::Replace(SelectorValue::CommandTemplate { .. })
        ));
    }
}
