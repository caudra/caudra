use caudra_workspace::{
    ByteRange, LocalPublicationState, LocalTransferCondition, LocalTransferDestination,
    LocalTransferPath, LocalTransferRevision, MutationCondition, OperationId, OperationState,
    PreparedLocalTransfer, PreparedTransferPublication, RemoteTransferStage, ResourceRevision,
    TransferContent, TransferPublicationRequest, TransferPublicationState,
    TransferPublicationStatus, WorkspaceError, WorkspacePath,
};
use serde_json::Value;

use super::{
    ApprovedParent, FileOutcome, JournalEntry, JournalState, LocalAccess, MAX_REVIEW_BYTES,
    PlannedFile, RemoteRootIdentity, Side, TransferAction, TransferError, TransferEvent,
    TransferJournal, TransferPhase, TransferPlan, TransferRun, WorkspaceTransfer,
};
use crate::CancelToken;

#[derive(Default)]
struct ActiveTransfer {
    stage: Option<RemoteTransferStage>,
    remote: Option<PreparedTransferPublication>,
    local: Option<PreparedLocalTransfer>,
    dispatched: bool,
    untracked_lease: bool,
}

impl WorkspaceTransfer {
    /// Partial outcomes are returned even if a later entry, journal write or cancellation fails.
    /// The caller must retain this future through cancellation so cleanup can finish.
    pub async fn execute(
        &self,
        plan: &TransferPlan,
        journal: &mut TransferJournal,
        cancel: &CancelToken,
    ) -> TransferRun {
        let mut run = TransferRun::default();
        let approval = async {
            if plan.review.files.is_empty() || plan.review.files.len() > self.limits.max_selected {
                return Err(TransferError::Quota);
            }
            let mut bytes = 0_u64;
            for file in &plan.review.files {
                let size = file.source(&plan.review.action)?.content.size_bytes;
                bytes = bytes.checked_add(size).ok_or(TransferError::Quota)?;
                if size > self.limits.max_file_bytes || bytes > self.limits.max_total_bytes {
                    return Err(TransferError::Quota);
                }
            }
            self.validate_context(&plan.review.context, &plan.review.filter_digest, cancel)
                .await?;
            self.bounded(
                cancel,
                self.services
                    .authorization
                    .roots(&plan.review.context.roots),
            )
            .await?;
            self.bounded(cancel, self.services.authorization.review_plan(plan))
                .await?;
            self.validate_context(&plan.review.context, &plan.review.filter_digest, cancel)
                .await
        }
        .await;
        if let Err(error) = approval {
            run.stopped = Some(error);
            return run;
        }
        for file in &plan.review.files {
            if cancel.is_cancelled() {
                run.stopped = Some(TransferError::Cancelled);
                break;
            }
            match journal.reserve(plan, file) {
                Ok(false) => {
                    run.outcomes
                        .insert(file.operation_id.clone(), FileOutcome::Confirmed);
                    continue;
                }
                Err(error) => {
                    run.stopped = Some(error);
                    break;
                }
                Ok(true) => {}
            }
            let mut active = ActiveTransfer::default();
            let result = self
                .transfer_file(plan, file, journal, cancel, &mut active)
                .await;
            let (outcome, error) = match result {
                Ok(revision) => match journal.confirm(&file.operation_id, revision) {
                    Ok(()) => (FileOutcome::Confirmed, None),
                    // The durable dispatched record deliberately stays blocking.
                    Err(error) => (FileOutcome::Unknown, Some(error)),
                },
                Err(error) => {
                    let outcome = if active.dispatched
                        && !matches!(
                            error,
                            TransferError::PublicationRejected
                                | TransferError::PublicationCancelled
                        ) {
                        FileOutcome::Unknown
                    } else if matches!(
                        error,
                        TransferError::Cancelled
                            | TransferError::PublicationCancelled
                            | TransferError::Workspace(WorkspaceError::Cancelled)
                    ) {
                        FileOutcome::Cancelled
                    } else {
                        FileOutcome::Failed
                    };
                    (outcome, Some(error))
                }
            };
            let cleanup_pending = if outcome == FileOutcome::Unknown {
                true
            } else {
                self.cleanup(&active).await
            };
            let state = match outcome {
                FileOutcome::Confirmed => JournalState::Confirmed,
                FileOutcome::Failed => JournalState::Failed,
                FileOutcome::Cancelled => JournalState::Cancelled,
                FileOutcome::Unknown => JournalState::Unknown,
            };
            let saved = journal.update(&file.operation_id, |entry| {
                if entry.state != JournalState::Confirmed {
                    entry.state = state;
                }
                entry.cleanup_pending = cleanup_pending;
                Ok(())
            });
            self.settled(
                &mut run,
                &file.operation_id,
                &file.path,
                outcome,
                cleanup_pending,
            );
            if let Err(error) = saved {
                run.stopped = Some(error);
                break;
            }
            if let Some(error) = error {
                run.stopped = Some(error);
                break;
            }
        }
        run
    }

    async fn transfer_file(
        &self,
        plan: &TransferPlan,
        file: &PlannedFile,
        journal: &mut TransferJournal,
        cancel: &CancelToken,
        active: &mut ActiveTransfer,
    ) -> Result<ResourceRevision, TransferError> {
        let mut effective = file.clone();
        for entry in journal.entries()?.into_iter().filter(|entry| {
            entry.state == JournalState::Confirmed && entry.plan_digest == *plan.digest()
        }) {
            for parent in entry.created_directories {
                if effective.create_directories.contains(&parent.path) {
                    effective
                        .create_directories
                        .retain(|path| *path != parent.path);
                    effective.parents.push(parent);
                }
            }
        }
        let file = &effective;
        self.validate_context(&plan.review.context, &plan.review.filter_digest, cancel)
            .await?;
        self.validate_file(&plan.review.context, file, cancel)
            .await?;
        match plan.review.action {
            TransferAction::Seed | TransferAction::Push => {
                self.push(plan, file, journal, cancel, active).await
            }
            TransferAction::Pull => self.pull(plan, file, journal, cancel, active).await,
        }
    }

    async fn push(
        &self,
        plan: &TransferPlan,
        file: &PlannedFile,
        journal: &mut TransferJournal,
        cancel: &CancelToken,
        active: &mut ActiveTransfer,
    ) -> Result<ResourceRevision, TransferError> {
        let context = &plan.review.context;
        let source = file.source(&plan.review.action)?;
        let limits = self.services.remote.limits()?;
        if limits.max_stages == 0
            || limits.max_concurrent_io == 0
            || source.content.size_bytes > limits.max_file_bytes
            || source
                .content
                .size_bytes
                .saturating_add(u64::from(limits.stream_buffer_bytes))
                > limits.max_reserved_bytes
        {
            return Err(TransferError::Quota);
        }
        if limits.atomic_replace_against_external_writers
            != plan.review.atomic_replace_against_external_writers
        {
            return Err(TransferError::Stale);
        }
        self.bounded(
            cancel,
            self.services
                .authorization
                .local(&context.roots, &file.path, LocalAccess::Export),
        )
        .await?;
        self.validate_context(context, &plan.review.filter_digest, cancel)
            .await?;
        let reader = self
            .bounded(
                cancel,
                self.services.inventory.open_local(
                    &context.roots.local,
                    &file.path,
                    &source.revision,
                ),
            )
            .await?;
        self.phase(&file.path, TransferPhase::Staging);
        active.untracked_lease = true;
        let stage = self
            .workspace(
                cancel,
                self.services.remote.stage(
                    &context.roots.remote.binding,
                    &context.roots.remote.cursor,
                    reader,
                    &source.content,
                ),
            )
            .await?;
        active.untracked_lease = false;
        active.stage = Some(stage.clone());
        journal.update(&file.operation_id, |entry| {
            if entry.state != JournalState::Reserved {
                return Err(TransferError::RecoveryRequired);
            }
            entry.stage = Some(stage.clone());
            entry.cleanup_pending = true;
            Ok(())
        })?;
        if stage.binding != context.roots.remote.binding
            || stage.cursor != context.roots.remote.cursor
            || stage.content != source.content
        {
            return Err(TransferError::Stale);
        }
        self.phase(&file.path, TransferPhase::Sealing);
        let sealed = self
            .workspace(cancel, self.services.remote.seal(&stage))
            .await?;
        if sealed.stage != stage {
            return Err(TransferError::Stale);
        }
        let request = TransferPublicationRequest {
            create_directories: file.create_directories.clone(),
            publication_id: file.operation_id.clone(),
            path: file.path.clone(),
            condition: match &file.remote {
                Some(stamp) if plan.review.action != TransferAction::Seed => {
                    MutationCondition::Matches(stamp.revision.clone())
                }
                None => MutationCondition::MustNotExist,
                _ => return Err(TransferError::Selection),
            },
        };
        self.phase(&file.path, TransferPhase::Preparing);
        active.untracked_lease = true;
        let prepared = self
            .workspace(
                cancel,
                self.services.remote.prepare_publication(&sealed, &request),
            )
            .await?;
        active.untracked_lease = false;
        active.remote = Some(prepared.clone());
        if prepared.sealed != sealed
            || prepared.request != request
            || prepared.cwd_path != context.roots.remote.cwd
            || prepared.operation.invocation_id.is_none()
            || serde_json::to_vec(&prepared.review)
                .map_err(|_| TransferError::Stale)?
                .len()
                > MAX_REVIEW_BYTES
        {
            return Err(TransferError::Stale);
        }
        journal.update(&file.operation_id, |entry| {
            if entry.state != JournalState::Reserved {
                return Err(TransferError::RecoveryRequired);
            }
            let mut recovery = prepared.clone();
            recovery.review = Value::Null;
            entry.remote_preparation = Some(recovery);
            entry.state = JournalState::Prepared;
            Ok(())
        })?;
        self.phase(&file.path, TransferPhase::Reviewing);
        self.bounded(
            cancel,
            self.services
                .authorization
                .review_remote_publication(plan, file, &prepared),
        )
        .await?;
        self.validate_context(context, &plan.review.filter_digest, cancel)
            .await?;
        self.validate_file(context, file, cancel).await?;
        dispatch(journal, &file.operation_id, active)?;
        self.phase(&file.path, TransferPhase::Publishing);
        let status = self
            .workspace(cancel, self.services.remote.execute_publication(&prepared))
            .await?;
        if status.handle.preparation_id != prepared.operation.preparation_id
            || status.handle.invocation_id != prepared.operation.invocation_id
        {
            return Err(TransferError::Stale);
        }
        match status.state {
            OperationState::Completed { result, .. } => {
                record_directories(journal, &file.operation_id, &request, &result)?;
                confirmed_revision(
                    &result,
                    &file.operation_id,
                    &context.roots.remote,
                    &file.path,
                    &source.content,
                    false,
                )
            }
            OperationState::Failed {
                side_effects_possible: false,
                ..
            } => Err(TransferError::PublicationRejected),
            OperationState::Cancelled {
                side_effects_possible: false,
            } => Err(TransferError::PublicationCancelled),
            _ => Err(WorkspaceError::IndeterminateOutcome.into()),
        }
    }

    async fn pull(
        &self,
        plan: &TransferPlan,
        file: &PlannedFile,
        journal: &mut TransferJournal,
        cancel: &CancelToken,
        active: &mut ActiveTransfer,
    ) -> Result<ResourceRevision, TransferError> {
        let context = &plan.review.context;
        let source = file.source(&plan.review.action)?;
        self.bounded(
            cancel,
            self.services
                .authorization
                .local(&context.roots, &file.path, LocalAccess::Write),
        )
        .await?;
        self.phase(&file.path, TransferPhase::Staging);
        active.untracked_lease = true;
        let downloaded = self
            .workspace(
                cancel,
                self.services
                    .remote
                    .download(&Self::remote_file(context, source), None),
            )
            .await?;
        active.untracked_lease = false;
        if !downloaded.whole_file_verified
            || downloaded.content != source.content
            || downloaded.range
                != (ByteRange {
                    start: 0,
                    end_exclusive: source.content.size_bytes,
                })
        {
            return Err(WorkspaceError::TransferIntegrity.into());
        }
        let destination = LocalTransferDestination {
            create_directories: file.create_directories.clone(),
            path: LocalTransferPath::new(file.path.as_str())?,
            condition: match &file.local {
                Some(stamp) => {
                    LocalTransferCondition::Matches(LocalTransferRevision(stamp.revision.clone()))
                }
                None => LocalTransferCondition::MustNotExist,
            },
        };
        self.phase(&file.path, TransferPhase::Preparing);
        active.untracked_lease = true;
        let prepared = self
            .workspace(
                cancel,
                self.services.local.prepare(
                    downloaded.source,
                    destination.clone(),
                    source.content.clone(),
                ),
            )
            .await?;
        active.untracked_lease = false;
        active.local = Some(prepared.clone());
        if prepared.review.destination != destination || prepared.review.content != source.content {
            return Err(TransferError::Stale);
        }
        journal.update(&file.operation_id, |entry| {
            if entry.state != JournalState::Reserved {
                return Err(TransferError::RecoveryRequired);
            }
            entry.local_preparation = Some(prepared.id.clone());
            entry.local_review = Some(prepared.clone());
            entry.state = JournalState::Prepared;
            entry.cleanup_pending = true;
            Ok(())
        })?;
        let _clean = self
            .bounded(
                cancel,
                self.services
                    .buffers
                    .lock_clean(&context.roots.local, &file.path),
            )
            .await?;
        self.validate_context(context, &plan.review.filter_digest, cancel)
            .await?;
        self.validate_file(context, file, cancel).await?;
        dispatch(journal, &file.operation_id, active)?;
        self.phase(&file.path, TransferPhase::Publishing);
        let revision = match self
            .workspace(cancel, self.services.local.execute(&prepared))
            .await
        {
            Ok(revision) => revision,
            Err(error) if local_rejected(&error) => return Err(TransferError::PublicationRejected),
            Err(error) => return Err(error),
        };
        active.local = None;
        self.record_local_directories(journal, &file.operation_id, &prepared, cancel)
            .await?;
        Ok(revision.0)
    }

    async fn record_local_directories(
        &self,
        journal: &mut TransferJournal,
        id: &OperationId,
        prepared: &PreparedLocalTransfer,
        cancel: &CancelToken,
    ) -> Result<(), TransferError> {
        let directories = self
            .workspace(cancel, self.services.local.created_directories(prepared))
            .await?;
        if directories.iter().map(|(path, _)| path).ne(prepared
            .review
            .destination
            .create_directories
            .iter())
        {
            return Err(TransferError::Stale);
        }
        for (path, identity) in &directories {
            let node = self
                .inspect_allowed(&Side::Local, path, cancel)
                .await?
                .ok_or(TransferError::Stale)?;
            if node.kind != super::NodeKind::Directory || node.identity != *identity {
                return Err(TransferError::Stale);
            }
        }
        journal.update(id, |entry| {
            entry.created_directories = directories
                .into_iter()
                .map(|(path, identity)| ApprovedParent {
                    side: Side::Local,
                    path,
                    identity,
                })
                .collect();
            Ok(())
        })
    }

    /// Status-only recovery. Unknown/forgotten outcomes stay blocking; no execute call is replayed.
    pub async fn reconcile(
        &self,
        journal: &mut TransferJournal,
        cancel: &CancelToken,
    ) -> TransferRun {
        let mut run = TransferRun::default();
        let entries = match journal.entries() {
            Ok(entries) => entries,
            Err(error) => {
                run.stopped = Some(error);
                return run;
            }
        };
        let context = match self.context(cancel).await {
            Ok(context) => context,
            Err(error) => {
                run.stopped = Some(error);
                return run;
            }
        };
        for entry in entries.into_iter().filter(|entry| entry.state.blocks()) {
            if entry.roots.local != context.roots.local
                || !same_remote_root(&entry.roots.remote, &context.roots.remote)
            {
                continue;
            }
            self.phase(&entry.path, TransferPhase::Reconciling);
            match self.reconcile_entry(&entry, journal, cancel).await {
                Ok((outcome, pending)) => {
                    self.settled(&mut run, &entry.operation_id, &entry.path, outcome, pending)
                }
                Err(error) => {
                    run.stopped = Some(error);
                    break;
                }
            }
        }
        run
    }

    async fn reconcile_entry(
        &self,
        entry: &JournalEntry,
        journal: &mut TransferJournal,
        cancel: &CancelToken,
    ) -> Result<(FileOutcome, bool), TransferError> {
        let current = self.context(cancel).await?;
        if current.roots.local != entry.roots.local
            || !same_remote_root(&current.roots.remote, &entry.roots.remote)
        {
            return Err(TransferError::Stale);
        }
        self.bounded(cancel, self.services.authorization.roots(&current.roots))
            .await?;
        if matches!(entry.state, JournalState::Reserved | JournalState::Prepared) {
            // Dispatched is fsynced before any execute call. A crash before it cannot publish.
            journal.update(&entry.operation_id, |current| {
                if current.state != entry.state {
                    return Err(TransferError::RecoveryRequired);
                }
                current.state = JournalState::Cancelled;
                current.cleanup_pending = true;
                Ok(())
            })?;
            let active = ActiveTransfer {
                stage: entry.stage.clone(),
                remote: entry.remote_preparation.clone(),
                ..ActiveTransfer::default()
            };
            let pending = entry.local_preparation.is_some() || self.cleanup(&active).await;
            journal.update(&entry.operation_id, |entry| {
                entry.cleanup_pending = pending;
                Ok(())
            })?;
            return Ok((FileOutcome::Cancelled, pending));
        }
        if let Some(prepared) = &entry.local_review {
            self.bounded(
                cancel,
                self.services
                    .authorization
                    .local(&current.roots, &entry.path, LocalAccess::Read),
            )
            .await?;
            if entry.action != TransferAction::Pull
                || prepared.review.content != entry.source()?.content
                || prepared.review.destination.path.as_str() != entry.path.as_str()
            {
                return Err(TransferError::Stale);
            }
            match self
                .workspace(cancel, self.services.local.publication_status(prepared))
                .await?
            {
                LocalPublicationState::Completed(revision) => {
                    self.record_local_directories(journal, &entry.operation_id, prepared, cancel)
                        .await?;
                    let (actual_revision, content) = self
                        .workspace(
                            cancel,
                            self.services.local.stat(&prepared.review.destination.path),
                        )
                        .await?;
                    if revision != actual_revision || content != prepared.review.content {
                        return Ok((FileOutcome::Unknown, entry.cleanup_pending));
                    }
                    journal.confirm(&entry.operation_id, revision.0)?;
                    journal.update(&entry.operation_id, |entry| {
                        entry.cleanup_pending = false;
                        Ok(())
                    })?;
                    return Ok((FileOutcome::Confirmed, false));
                }
                LocalPublicationState::NotPublished => {
                    journal.update(&entry.operation_id, |entry| {
                        entry.state = JournalState::Failed;
                        entry.cleanup_pending = false;
                        Ok(())
                    })?;
                    return Ok((FileOutcome::Failed, false));
                }
                _ => return Ok((FileOutcome::Unknown, entry.cleanup_pending)),
            }
        }
        let Some(prepared) = &entry.remote_preparation else {
            return Ok((FileOutcome::Unknown, entry.cleanup_pending));
        };
        let status = self
            .workspace(cancel, self.services.remote.publication_status(prepared))
            .await?;
        if status.publication_id != entry.operation_id {
            return Err(TransferError::Stale);
        }
        match status.state {
            TransferPublicationState::Completed => {
                record_directories(journal, &entry.operation_id, &prepared.request, &status)?;
                let revision = confirmed_revision(
                    &status,
                    &entry.operation_id,
                    &current.roots.remote,
                    &entry.path,
                    &entry.source()?.content,
                    true,
                )?;
                journal.confirm(&entry.operation_id, revision)?;
                let pending = self
                    .cleanup(&ActiveTransfer {
                        stage: entry.stage.clone(),
                        ..ActiveTransfer::default()
                    })
                    .await;
                journal.update(&entry.operation_id, |entry| {
                    entry.cleanup_pending = pending;
                    Ok(())
                })?;
                Ok((FileOutcome::Confirmed, pending))
            }
            // The neutral status API does not carry a no-side-effects proof for failure. Do not
            // infer a safe retry from Failed, Cancelled, Prepared, or an expired status record.
            _ => Ok((FileOutcome::Unknown, entry.cleanup_pending)),
        }
    }

    async fn cleanup(&self, active: &ActiveTransfer) -> bool {
        let cancel = CancelToken::none();
        let mut pending = active.untracked_lease;
        if let Some(prepared) = &active.local {
            pending |= self
                .workspace(&cancel, self.services.local.release(prepared))
                .await
                .is_err();
        }
        if let Some(prepared) = &active.remote {
            pending |= !self
                .workspace(&cancel, self.services.remote.release_publication(prepared))
                .await
                .is_ok_and(|result| result.released);
        }
        if let Some(stage) = &active.stage {
            pending |= self
                .workspace(&cancel, self.services.remote.release_stage(stage))
                .await
                .is_err();
        }
        pending
    }

    fn settled(
        &self,
        run: &mut TransferRun,
        operation_id: &OperationId,
        path: &WorkspacePath,
        outcome: FileOutcome,
        cleanup_pending: bool,
    ) {
        run.outcomes.insert(operation_id.clone(), outcome.clone());
        self.services.events.emit(TransferEvent::Settled {
            operation_id: operation_id.clone(),
            path: path.clone(),
            outcome,
        });
        if cleanup_pending {
            run.cleanup_deferred.insert(operation_id.clone());
            self.services.events.emit(TransferEvent::CleanupDeferred {
                operation_id: operation_id.clone(),
            });
        }
    }
}

fn dispatch(
    journal: &mut TransferJournal,
    id: &OperationId,
    active: &mut ActiveTransfer,
) -> Result<(), TransferError> {
    journal.update(id, |entry| {
        if entry.state != JournalState::Prepared {
            return Err(TransferError::Journal);
        }
        entry.state = JournalState::Dispatched;
        Ok(())
    })?;
    active.dispatched = true;
    Ok(())
}

fn record_directories(
    journal: &mut TransferJournal,
    id: &OperationId,
    request: &TransferPublicationRequest,
    status: &TransferPublicationStatus,
) -> Result<(), TransferError> {
    if status
        .created_directories
        .iter()
        .map(|(path, _)| path)
        .ne(request.create_directories.iter())
    {
        return Err(TransferError::Stale);
    }
    journal.update(id, |entry| {
        entry.created_directories = status
            .created_directories
            .iter()
            .map(|(path, identity)| ApprovedParent {
                side: Side::Remote,
                path: path.clone(),
                identity: identity.clone(),
            })
            .collect();
        Ok(())
    })
}

fn local_rejected(error: &TransferError) -> bool {
    matches!(
        error,
        TransferError::PublicationRejected
            | TransferError::Workspace(
                WorkspaceError::Conflict
                    | WorkspaceError::PermissionDenied
                    | WorkspaceError::PolicyDenied
                    | WorkspaceError::TransferIntegrity
                    | WorkspaceError::TransferQuota
                    | WorkspaceError::Cancelled
            )
    )
}

fn same_remote_root(left: &RemoteRootIdentity, right: &RemoteRootIdentity) -> bool {
    left.binding.authority() == right.binding.authority()
        && left.binding.principal() == right.binding.principal()
        && left.binding.project() == right.binding.project()
        && left.cwd == right.cwd
}

fn confirmed_revision(
    status: &TransferPublicationStatus,
    id: &OperationId,
    root: &RemoteRootIdentity,
    path: &WorkspacePath,
    content: &TransferContent,
    recovery: bool,
) -> Result<ResourceRevision, TransferError> {
    let file = status
        .file
        .as_ref()
        .ok_or(WorkspaceError::IndeterminateOutcome)?;
    let bound = if recovery {
        file.binding.authority() == root.binding.authority()
            && file.binding.principal() == root.binding.principal()
            && file.binding.project() == root.binding.project()
    } else {
        file.binding == root.binding && file.cursor == root.cursor
    };
    if status.publication_id != *id
        || status.state != TransferPublicationState::Completed
        || file.path != *path
        || file.content != *content
        || !bound
    {
        return Err(TransferError::Stale);
    }
    Ok(file.revision.clone())
}
