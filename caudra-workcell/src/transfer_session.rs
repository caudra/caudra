use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

use caudra_agent::{
    CancelToken, EventSender,
    permissions::{PermissionManager, canonical_json_sha256},
    workspace_transfer::{
        Comparison, OrchestrationLimits, PullBufferGuard, RemoteRootIdentity, TransferAction,
        TransferError, TransferEvents, TransferFilters, TransferJournal, TransferPlan,
        TransferPreview, WorkspaceTransfer,
    },
};
use caudra_config::sandbox::TransferPolicy;
use caudra_storage::{StateDir, id::CaudraId};
use caudra_workspace::{TransferDigest, WorkspacePath};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    NativeTransferAuthorization, RemoteWorkcellClient, ReviewedTransferHost,
    reviewed_workspace_transfer,
};

pub type TransferValidity = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

pub struct TransferSessionHost {
    pub permissions: Arc<PermissionManager>,
    pub permission_events: EventSender,
    pub buffers: Arc<dyn PullBufferGuard>,
    pub progress: Arc<dyn TransferEvents>,
    pub cancel: CancelToken,
    pub validity: TransferValidity,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferReport {
    pub result_id: String,
    pub plan_id: Option<TransferDigest>,
    pub outcomes: Value,
    pub stopped: Option<String>,
    pub cleanup_deferred: Value,
    pub recovery: Value,
    pub audit: Value,
}

pub struct TransferSession {
    engine: WorkspaceTransfer,
    authorization: Arc<NativeTransferAuthorization>,
    journal: TransferJournal,
    comparison: Option<Arc<Comparison>>,
    plan: Option<Arc<TransferPlan>>,
}

impl TransferSession {
    pub fn supports_directory_publication(&self) -> bool {
        self.engine.supports_directory_publication()
    }

    pub async fn open(
        local_root: PathBuf,
        remote: RemoteWorkcellClient,
        remote_root: RemoteRootIdentity,
        policy: &TransferPolicy,
        state: &StateDir,
        host: TransferSessionHost,
    ) -> Result<Self, TransferError> {
        if !local_root.is_absolute() {
            return Err(TransferError::LocalRoot);
        }
        let local_root = local_root
            .canonicalize()
            .map_err(|_| TransferError::LocalRoot)?;
        let state_root = state
            .persistent_path()
            .canonicalize()
            .map_err(|_| TransferError::Journal)?;
        if state_root.starts_with(&local_root) {
            return Err(TransferError::Journal);
        }
        let key = canonical_json_sha256(&json!(local_root));
        let journal = TransferJournal::new(state_root.join("workspace-transfers.json"))?;
        let authorization = Arc::new(NativeTransferAuthorization::new(
            host.permissions,
            host.permission_events,
            host.cancel,
            host.validity,
        ));
        let engine = reviewed_workspace_transfer(
            local_root,
            state_root.join(format!("transfer-{key}.local.json")),
            remote,
            remote_root,
            TransferFilters::new(policy, &[])?,
            OrchestrationLimits::default(),
            ReviewedTransferHost {
                authorization: authorization.clone(),
                local_publication: authorization.clone(),
                buffers: host.buffers,
                events: host.progress,
            },
        )
        .await?;
        Ok(Self {
            engine,
            authorization,
            journal,
            comparison: None,
            plan: None,
        })
    }

    pub async fn compare(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<Arc<Comparison>, TransferError> {
        self.plan = None;
        self.comparison = None;
        let comparison = Arc::new(self.engine.compare(cancel).await?);
        self.comparison = Some(comparison.clone());
        Ok(comparison)
    }

    pub async fn preview(
        &self,
        path: &WorkspacePath,
        cancel: &CancelToken,
    ) -> Result<TransferPreview, TransferError> {
        let comparison = self.comparison.as_ref().ok_or(TransferError::Stale)?;
        self.engine.inspect_preview(comparison, path, cancel).await
    }

    pub async fn review(
        &mut self,
        action: TransferAction,
        paths: &[WorkspacePath],
        cancel: &CancelToken,
    ) -> Result<Arc<TransferPlan>, TransferError> {
        self.plan = None;
        let comparison = self.comparison.as_ref().ok_or(TransferError::Stale)?;
        let mut parents = BTreeSet::new();
        for path in paths {
            let mut parent = path.parent();
            while let Some(path) = parent.filter(|path| !path.is_root()) {
                parents.insert(path.clone());
                parent = path.parent();
            }
        }
        let plan = Arc::new(
            self.engine
                .plan(comparison, action, paths, &parents, cancel)
                .await?,
        );
        self.plan = Some(plan.clone());
        Ok(plan)
    }

    pub async fn review_selection(
        &mut self,
        action: TransferAction,
        paths: &[WorkspacePath],
        cancel: &CancelToken,
    ) -> Result<Arc<TransferPlan>, TransferError> {
        self.plan = None;
        let comparison = self.comparison.as_ref().ok_or(TransferError::Stale)?;
        let plan = Arc::new(
            self.engine
                .plan_selection(comparison, action, paths, cancel)
                .await?,
        );
        self.plan = Some(plan.clone());
        Ok(plan)
    }

    pub async fn execute(
        &mut self,
        digest: &TransferDigest,
        cancel: &CancelToken,
    ) -> Result<TransferReport, TransferError> {
        if self
            .plan
            .as_ref()
            .is_none_or(|plan| plan.digest() != digest)
        {
            return Err(TransferError::Stale);
        }
        let plan = self.plan.take().ok_or(TransferError::Stale)?;
        self.authorization.consent(&plan)?;
        let run = self.engine.execute(&plan, &mut self.journal, cancel).await;
        self.comparison = None;
        self.report(run, Some(digest.clone()))
    }

    pub async fn reconcile(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<TransferReport, TransferError> {
        self.plan = None;
        let run = self.engine.reconcile(&mut self.journal, cancel).await;
        self.report(run, None)
    }

    pub fn recovery(&self) -> Result<Value, TransferError> {
        serde_json::to_value(self.journal.entries()?).map_err(|_| TransferError::Journal)
    }

    fn report(
        &self,
        run: caudra_agent::workspace_transfer::TransferRun,
        plan_id: Option<TransferDigest>,
    ) -> Result<TransferReport, TransferError> {
        Ok(TransferReport {
            result_id: CaudraId::generate().to_string(),
            plan_id,
            outcomes: json!(run.outcomes),
            stopped: run.stopped.map(|error| error.to_string()),
            cleanup_deferred: json!(run.cleanup_deferred),
            recovery: self.recovery().unwrap_or_else(
                |error| json!({"unavailable": error.to_string(), "replay_forbidden": true}),
            ),
            audit: self
                .journal
                .audit()
                .map(|audit| json!(audit))
                .unwrap_or_else(
                    |error| json!({"unavailable": error.to_string(), "replay_forbidden": true}),
                ),
        })
    }
}
