use std::collections::BTreeSet;

use caudra_workspace::{
    DirectoryPublicationRequest, DirectoryPublicationStatus, OperationState, ResourceRevision,
    TransferPublicationState, WorkspaceError, WorkspacePath,
};
use serde_json::Value;

use super::{
    ApprovedParent, Comparison, ComparisonKind, FileOutcome, InventoryContext, JournalEntry,
    JournalState, LocalAccess, MAX_REVIEW_BYTES, MetadataPolicy, NodeKind, PLAN_DOMAIN, PlanReview,
    PlannedDirectory, RollbackCoverage, Side, TransferAction, TransferError, TransferEvent,
    TransferJournal, TransferPhase, TransferPlan, WorkspaceTransfer, digest,
    execution::{ActiveTransfer, dispatch},
    operation_id,
};
use crate::CancelToken;

impl WorkspaceTransfer {
    pub fn supports_directory_publication(&self) -> bool {
        self.services.local.supports_directory_publication()
            && self.services.remote.supports_directory_publication()
    }

    pub(super) async fn transfer_directory(
        &self,
        plan: &TransferPlan,
        directory: &PlannedDirectory,
        journal: &mut TransferJournal,
        cancel: &CancelToken,
        active: &mut ActiveTransfer,
    ) -> Result<ResourceRevision, TransferError> {
        let mut directory = directory.clone();
        for entry in journal.entries()?.into_iter().filter(|entry| {
            entry.state == JournalState::Confirmed && entry.plan_digest == *plan.digest()
        }) {
            for parent in entry.created_directories {
                if directory.create_directories.contains(&parent.path) {
                    directory
                        .create_directories
                        .retain(|path| *path != parent.path);
                    directory.parents.push(parent);
                }
            }
        }
        let context = &plan.review.context;
        self.validate_context(context, &plan.review.filter_digest, cancel)
            .await?;
        self.validate_directory(context, &directory, cancel).await?;
        let request = DirectoryPublicationRequest {
            publication_id: directory.operation_id.clone(),
            path: directory.path.clone(),
            create_directories: directory.create_directories.clone(),
        };
        self.phase(&directory.path, TransferPhase::Preparing);
        let _lease = if directory.directory_side == Side::Local {
            Some(
                self.bounded(
                    cancel,
                    self.services
                        .buffers
                        .lock_clean(&context.roots.local, &directory.path),
                )
                .await?,
            )
        } else {
            None
        };
        self.bounded(
            cancel,
            self.services.authorization.local(
                &context.roots,
                &directory.path,
                if directory.directory_side == Side::Local {
                    LocalAccess::Write
                } else {
                    LocalAccess::Export
                },
            ),
        )
        .await?;
        active.untracked_lease = true;
        if directory.directory_side == Side::Local {
            let prepared = self
                .workspace(cancel, self.services.local.prepare_directory(&request))
                .await?;
            active.untracked_lease = false;
            active.local_directory = Some(prepared.clone());
            if prepared.request != request {
                return Err(TransferError::Stale);
            }
            journal.update(&directory.operation_id, |entry| {
                if entry.state != JournalState::Reserved {
                    return Err(TransferError::RecoveryRequired);
                }
                entry.local_directory = Some(prepared);
                entry.state = JournalState::Prepared;
                entry.cleanup_pending = true;
                Ok(())
            })?;
        } else {
            let root = &context.roots.remote;
            let prepared = self
                .workspace(
                    cancel,
                    self.services
                        .remote
                        .prepare_directory(&root.binding, &root.cursor, &request),
                )
                .await?;
            active.untracked_lease = false;
            active.remote_directory = Some(prepared.clone());
            if prepared.request != request
                || prepared.binding != root.binding
                || prepared.cursor != root.cursor
                || prepared.cwd_path != root.cwd
                || prepared.operation.invocation_id.is_none()
                || serde_json::to_vec(&prepared.review)
                    .map_err(|_| TransferError::Stale)?
                    .len()
                    > MAX_REVIEW_BYTES
            {
                return Err(TransferError::Stale);
            }
            journal.update(&directory.operation_id, |entry| {
                if entry.state != JournalState::Reserved {
                    return Err(TransferError::RecoveryRequired);
                }
                let mut recovery = prepared.clone();
                recovery.review = Value::Null;
                entry.remote_directory = Some(recovery);
                entry.state = JournalState::Prepared;
                entry.cleanup_pending = true;
                Ok(())
            })?;
            self.phase(&directory.path, TransferPhase::Reviewing);
            self.bounded(
                cancel,
                self.services
                    .authorization
                    .review_remote_directory(plan, &directory, &prepared),
            )
            .await?;
        }
        self.validate_context(context, &plan.review.filter_digest, cancel)
            .await?;
        self.validate_directory(context, &directory, cancel).await?;
        dispatch(journal, &directory.operation_id, active)?;
        self.phase(&directory.path, TransferPhase::Publishing);
        let status = if let Some(prepared) = &active.local_directory {
            self.workspace(cancel, self.services.local.execute_directory(prepared))
                .await?
        } else if let Some(prepared) = &active.remote_directory {
            let status = self
                .workspace(cancel, self.services.remote.execute_directory(prepared))
                .await?;
            if status.handle.preparation_id != prepared.operation.preparation_id
                || status.handle.invocation_id != prepared.operation.invocation_id
            {
                return Err(TransferError::Stale);
            }
            match status.state {
                OperationState::Completed { result, .. } => result,
                OperationState::Failed {
                    side_effects_possible: false,
                    ..
                } => return Err(TransferError::PublicationRejected),
                OperationState::Cancelled {
                    side_effects_possible: false,
                } => return Err(TransferError::PublicationCancelled),
                _ => return Err(WorkspaceError::IndeterminateOutcome.into()),
            }
        } else {
            return Err(TransferError::Stale);
        };
        if status.publication_id != directory.operation_id {
            return Err(TransferError::Stale);
        }
        match status.state {
            TransferPublicationState::Failed => Err(TransferError::PublicationRejected),
            TransferPublicationState::Cancelled => Err(TransferError::PublicationCancelled),
            _ => {
                self.confirm_directory(&directory, &request, &status, journal, cancel)
                    .await
            }
        }
    }

    async fn confirm_directory(
        &self,
        directory: &PlannedDirectory,
        request: &DirectoryPublicationRequest,
        status: &DirectoryPublicationStatus,
        journal: &mut TransferJournal,
        cancel: &CancelToken,
    ) -> Result<ResourceRevision, TransferError> {
        if status.publication_id != directory.operation_id
            || status.state != TransferPublicationState::Completed
        {
            return Err(WorkspaceError::IndeterminateOutcome.into());
        }
        let receipt = status.directory.as_ref().ok_or(TransferError::Stale)?;
        if receipt.path != directory.path
            || receipt
                .created_directories
                .iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>()
                != request.create_directories.iter().collect::<Vec<_>>()
        {
            return Err(TransferError::Stale);
        }
        let mut created = receipt.created_directories.clone();
        created.push((receipt.path.clone(), receipt.resource_id.clone()));
        let mut revision = None;
        for (path, id) in &created {
            let node = self
                .inspect_allowed(&directory.directory_side, path, cancel)
                .await?
                .ok_or(TransferError::Stale)?;
            if node.kind != NodeKind::Directory || node.identity != *id {
                return Err(TransferError::Stale);
            }
            if *path == directory.path {
                revision = Some(node.revision);
            }
        }
        journal.update(&directory.operation_id, |entry| {
            entry.created_directories = created
                .into_iter()
                .map(|(path, identity)| ApprovedParent {
                    side: directory.directory_side.clone(),
                    path,
                    identity,
                })
                .collect();
            Ok(())
        })?;
        revision.ok_or(TransferError::Stale)
    }

    pub(super) async fn reconcile_directory(
        &self,
        entry: &JournalEntry,
        journal: &mut TransferJournal,
        cancel: &CancelToken,
    ) -> Result<(FileOutcome, bool), TransferError> {
        let directory = entry.directory.as_ref().ok_or(TransferError::Journal)?;
        let mut active = ActiveTransfer::default();
        active.local_directory = entry.local_directory.clone();
        active.remote_directory = entry.remote_directory.clone();
        if !entry.state.blocks() {
            let outcome = match entry.state {
                JournalState::Confirmed => FileOutcome::Confirmed,
                JournalState::Failed => FileOutcome::Failed,
                JournalState::Cancelled => FileOutcome::Cancelled,
                _ => return Err(TransferError::Journal),
            };
            let pending = self.cleanup(&active).await;
            journal.update(&entry.operation_id, |current| {
                if current.state != entry.state {
                    return Err(TransferError::RecoveryRequired);
                }
                current.cleanup_pending = pending;
                Ok(())
            })?;
            return Ok((outcome, pending));
        }
        if matches!(entry.state, JournalState::Reserved | JournalState::Prepared) {
            journal.update(&entry.operation_id, |current| {
                if current.state != entry.state {
                    return Err(TransferError::RecoveryRequired);
                }
                current.state = JournalState::Cancelled;
                current.cleanup_pending = true;
                Ok(())
            })?;
            let pending = self.cleanup(&active).await;
            journal.update(&entry.operation_id, |entry| {
                entry.cleanup_pending = pending;
                Ok(())
            })?;
            return Ok((FileOutcome::Cancelled, pending));
        }
        let (request, status) = if let Some(prepared) = &entry.local_directory {
            self.bounded(
                cancel,
                self.services
                    .authorization
                    .local(&entry.roots, &entry.path, LocalAccess::Read),
            )
            .await?;
            (
                &prepared.request,
                self.workspace(cancel, self.services.local.directory_status(prepared))
                    .await?,
            )
        } else if let Some(prepared) = &entry.remote_directory {
            (
                &prepared.request,
                self.workspace(cancel, self.services.remote.directory_status(prepared))
                    .await?,
            )
        } else {
            return Ok((FileOutcome::Unknown, true));
        };
        if request.publication_id != entry.operation_id || request.path != entry.path {
            return Err(TransferError::Journal);
        }
        if status.publication_id != entry.operation_id {
            return Err(TransferError::Stale);
        }
        if matches!(
            status.state,
            TransferPublicationState::Failed | TransferPublicationState::Cancelled
        ) {
            let (state, outcome) = if status.state == TransferPublicationState::Failed {
                (JournalState::Failed, FileOutcome::Failed)
            } else {
                (JournalState::Cancelled, FileOutcome::Cancelled)
            };
            journal.update(&entry.operation_id, |current| {
                if current.state != entry.state {
                    return Err(TransferError::RecoveryRequired);
                }
                current.state = state;
                current.cleanup_pending = true;
                Ok(())
            })?;
            let pending = self.cleanup(&active).await;
            journal.update(&entry.operation_id, |current| {
                current.cleanup_pending = pending;
                Ok(())
            })?;
            return Ok((outcome, pending));
        }
        if status.state != TransferPublicationState::Completed {
            return Ok((FileOutcome::Unknown, true));
        }
        let revision = self
            .confirm_directory(directory, request, &status, journal, cancel)
            .await?;
        journal.confirm(&entry.operation_id, revision)?;
        let pending = self.cleanup(&active).await;
        journal.update(&entry.operation_id, |entry| {
            entry.cleanup_pending = pending;
            Ok(())
        })?;
        Ok((FileOutcome::Confirmed, pending))
    }

    pub async fn plan_selection(
        &self,
        comparison: &Comparison,
        action: TransferAction,
        selected: &[WorkspacePath],
        cancel: &CancelToken,
    ) -> Result<TransferPlan, TransferError> {
        if selected.is_empty() || selected.len() > self.limits.max_selected {
            return Err(TransferError::Selection);
        }
        self.validate_context(&comparison.context, &comparison.filter_digest, cancel)
            .await?;
        for path in selected {
            if !comparison.rows.iter().any(|row| row.path == *path) {
                return Err(TransferError::Selection);
            }
        }
        let source_side = if action == TransferAction::Pull {
            Side::Remote
        } else {
            Side::Local
        };
        let destination_side = if action == TransferAction::Pull {
            Side::Local
        } else {
            Side::Remote
        };
        let source = if source_side == Side::Local {
            &comparison.local
        } else {
            &comparison.remote
        };
        let mut files = Vec::new();
        let mut directories = Vec::new();
        let mut skipped = Vec::new();
        for row in comparison
            .rows
            .iter()
            .filter(|row| selected.iter().any(|path| within(&row.path, path)))
        {
            if row.path.is_root() {
                continue;
            }
            let eligible = match action {
                TransferAction::Seed => row.kind == ComparisonKind::LocalOnly,
                TransferAction::Push => matches!(
                    row.kind,
                    ComparisonKind::LocalOnly | ComparisonKind::Conflict
                ),
                TransferAction::Pull => matches!(
                    row.kind,
                    ComparisonKind::RemoteOnly | ComparisonKind::Conflict
                ),
            };
            let source_kind = if source_side == Side::Local {
                &row.local_kind
            } else {
                &row.remote_kind
            };
            let destination_kind = if destination_side == Side::Local {
                &row.local_kind
            } else {
                &row.remote_kind
            };
            match source_kind {
                Some(NodeKind::File)
                    if eligible
                        && destination_kind
                            .as_ref()
                            .is_none_or(|kind| *kind == NodeKind::File) =>
                {
                    files.push(row.path.clone())
                }
                Some(NodeKind::Directory) if eligible && destination_kind.is_none() => {
                    if !source
                        .entries
                        .keys()
                        .any(|path| path != &row.path && within(path, &row.path))
                    {
                        directories.push(
                            self.plan_directory(comparison, &row.path, &destination_side, cancel)
                                .await?,
                        );
                    }
                }
                Some(NodeKind::Directory) if row.kind == ComparisonKind::Equal => {}
                _ => skipped.push(row.path.clone()),
            }
            if files.len() + directories.len() > self.limits.max_selected {
                return Err(TransferError::Quota);
            }
        }
        if files.is_empty() && directories.is_empty() {
            return Err(TransferError::Selection);
        }
        if !directories.is_empty()
            && !match destination_side {
                Side::Local => self.services.local.supports_directory_publication(),
                Side::Remote => self.services.remote.supports_directory_publication(),
            }
        {
            return Err(WorkspaceError::UnsupportedEntry.into());
        }
        let mut review = if files.is_empty() {
            PlanReview {
                context: comparison.context.clone(),
                filter_digest: comparison.filter_digest.clone(),
                action,
                files: Vec::new(),
                directories: Vec::new(),
                skipped: Vec::new(),
                metadata: MetadataPolicy::ContentAndExecutableBitOnly,
                rollback: RollbackCoverage::None,
                atomic_across_files: false,
                atomic_replace_against_external_writers: false,
            }
        } else {
            let mut parents = BTreeSet::new();
            for path in &files {
                let mut parent = path.parent();
                while let Some(path) = parent.filter(|path| !path.is_root()) {
                    parents.insert(path.clone());
                    parent = path.parent();
                }
            }
            self.plan(comparison, action, &files, &parents, cancel)
                .await?
                .review
        };
        review.directories = directories;
        review.skipped = skipped;
        self.validate_context(&review.context, &review.filter_digest, cancel)
            .await?;
        let plan = TransferPlan {
            digest: digest(&(PLAN_DOMAIN, &review))?,
            review,
        };
        self.services.events.emit(TransferEvent::Planned {
            digest: plan.digest.clone(),
            files: plan.review.files.len() + plan.review.directories.len(),
        });
        Ok(plan)
    }

    async fn plan_directory(
        &self,
        comparison: &Comparison,
        path: &WorkspacePath,
        destination: &Side,
        cancel: &CancelToken,
    ) -> Result<PlannedDirectory, TransferError> {
        let source = if *destination == Side::Local {
            &comparison.remote
        } else {
            &comparison.local
        };
        let source = source
            .entries
            .get(path)
            .map(|entry| entry.node.clone())
            .ok_or(TransferError::Selection)?;
        let mut directory = PlannedDirectory {
            operation_id: operation_id()?,
            path: path.clone(),
            source,
            parents: Vec::new(),
            create_directories: Vec::new(),
            directory_side: destination.clone(),
        };
        let mut parent = path.parent();
        while let Some(path) = parent.filter(|path| !path.is_root()) {
            for (side, manifest) in [
                (Side::Local, &comparison.local),
                (Side::Remote, &comparison.remote),
            ] {
                match manifest.entries.get(&path) {
                    Some(entry) if entry.blocked.is_none() => {
                        let node = &entry.node;
                        if node.kind != NodeKind::Directory {
                            return Err(TransferError::Parent);
                        }
                        directory.parents.push(ApprovedParent {
                            side,
                            path: path.clone(),
                            identity: node.identity.clone(),
                        });
                    }
                    None if side == *destination && manifest.complete() => {
                        directory.create_directories.push(path.clone())
                    }
                    _ => return Err(TransferError::Parent),
                }
            }
            parent = path.parent();
        }
        directory.create_directories.reverse();
        self.validate_directory(&comparison.context, &directory, cancel)
            .await?;
        Ok(directory)
    }

    pub(super) async fn validate_directory(
        &self,
        context: &InventoryContext,
        directory: &PlannedDirectory,
        cancel: &CancelToken,
    ) -> Result<(), TransferError> {
        if !context.safe_local_traversal || !context.safe_remote_traversal {
            return Err(TransferError::UnsafeInventory);
        }
        let source_side = if directory.directory_side == Side::Local {
            Side::Remote
        } else {
            Side::Local
        };
        for parent in &directory.parents {
            let node = self
                .inspect_allowed(&parent.side, &parent.path, cancel)
                .await?
                .ok_or(TransferError::Stale)?;
            if node.kind != NodeKind::Directory || node.identity != parent.identity {
                return Err(TransferError::Stale);
            }
        }
        for path in directory.create_directories.iter().chain([&directory.path]) {
            if self
                .inspect_allowed(&directory.directory_side, path, cancel)
                .await?
                .is_some()
            {
                return Err(TransferError::Stale);
            }
        }
        if self
            .inspect_allowed(&source_side, &directory.path, cancel)
            .await?
            .as_ref()
            != Some(&directory.source)
        {
            return Err(TransferError::Stale);
        }
        // The inspection above unpinned the source side's listing, so this page is current.
        let page = self
            .bounded(
                cancel,
                self.services
                    .inventory
                    .list(&source_side, &directory.path, None, 1),
            )
            .await?;
        if page.incomplete
            || page.next.is_some()
            || !page.entries.is_empty()
            || self
                .inspect_allowed(&source_side, &directory.path, cancel)
                .await?
                .as_ref()
                != Some(&directory.source)
        {
            return Err(TransferError::Stale);
        }
        Ok(())
    }
}

fn within(path: &WorkspacePath, parent: &WorkspacePath) -> bool {
    path == parent
        || parent.is_root()
        || path
            .as_str()
            .strip_prefix(parent.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}
