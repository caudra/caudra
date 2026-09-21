//! Snapshot reads and explicitly reviewed lifecycle effects are separate workers.

use caudra_config::sandbox::persistence::{LoadedSandboxes, SandboxStore, SandboxStoreError};
use caudra_config::sandbox::{
    MAX_SANDBOX_FILE_BYTES, ProviderCapabilities, ResolvedLaunch, Revision, SandboxDraft,
    SandboxName, TemplateCatalog,
};
use caudra_sandbox::{
    Controller, CreateReview, Doctor, InstanceRecord, LifecycleAction,
    dto::{Instance, InstanceState},
    local_admin::{AdminRequest, ImageProbe, ProbedImage},
};
use caudra_storage::id::CaudraId;
use caudra_storage::private_file::{FileRevision, PrivateFile, PrivateFileError};
use caudra_storage::{
    StateDir,
    sandbox_auth::{
        SandboxApiKey, SandboxCredentialRef, list_sandbox_credentials, save_sandbox_api_key,
    },
    workspace_binding::StoredWorkspaceBinding,
};
use caudra_workspace::WorkspacePath;
use flume::Receiver;
use std::any::Any;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub const MAX_LIVE_PREVIEW_BYTES: usize = 48 * 1024;
pub mod transfer;
pub(crate) const RULE_TEST_NOTICE: &str = "Rule evaluation only; no DNS lookup or network probe. Operator blocks, DNS resolution, TLS and destination availability can still deny access.";

pub struct SandboxAttachment {
    pub name: SandboxName,
    pub binding: Box<StoredWorkspaceBinding>,
    pub runtime: Box<dyn Any + Send>,
}

pub type SandboxConnector =
    Arc<dyn Fn(SandboxName, Revision) -> Result<SandboxAttachment, String> + Send + Sync>;
pub type SandboxReadiness = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Default)]
pub(crate) struct SandboxWorkers {
    pub snapshot: Option<Receiver<SnapshotReply>>,
    pub scope: Option<SandboxSnapshotRequest>,
    pub refresh_at: Option<Instant>,
    pub sequence: u64,
    pub queued: Option<Box<LiveRequest>>,
    pub reply: Option<Receiver<LiveReply>>,
    pub connector: Option<SandboxConnector>,
    pub readiness: Option<SandboxReadiness>,
    pub attachment: Option<SandboxAttachment>,
    pub transfer_connector: Option<transfer::TransferConnector>,
    pub transfer_queued: Option<transfer::TransferCommand>,
    pub transfer: Option<transfer::TransferWorker>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxSnapshotRequest {
    pub conversation: CaudraId,
    pub manager_session: u64,
    pub configuration_revision: Revision,
    pub configuration_epoch: u64,
}

#[derive(Clone, Debug)]
pub enum SnapshotState<T> {
    Loading,
    Unavailable(String),
    Ready(T),
}

#[derive(Clone, Debug)]
pub struct SandboxProviderSnapshot {
    pub capabilities: ProviderCapabilities,
    pub catalog: SnapshotState<TemplateCatalog>,
    pub doctor: Option<Doctor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxInstanceState {
    Creating,
    Running,
    Paused,
    Stopped,
    Recovering,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxWorkcellState {
    Pending,
    Ready,
    Unavailable,
}

/// Nonsecret server-reported facts only; no endpoint bearer or lifecycle action.
#[derive(Clone, Debug)]
pub struct SandboxInstanceSnapshot {
    pub id: String,
    pub provider: SandboxName,
    pub state: SandboxInstanceState,
    pub workcell: SandboxWorkcellState,
    pub effective: Option<ResolvedLaunch>,
    pub lease_deadline: Option<String>,
    pub retention_deadline: Option<String>,
    pub blockers: Vec<String>,
    pub record: Option<InstanceRecord>,
    pub live: Option<Instance>,
}

/// Sequence increases within a request. The caller authenticates provider metadata
/// and supplies sanitized, nonsecret unavailable reasons and instance descriptions.
#[derive(Clone, Debug)]
pub struct SandboxSnapshot {
    pub sequence: u64,
    pub instances: SnapshotState<Vec<SandboxInstanceSnapshot>>,
    pub providers: BTreeMap<SandboxName, SandboxProviderSnapshot>,
    pub failures: BTreeMap<SandboxName, String>,
    pub credentials: Vec<SandboxCredentialRef>,
}

pub(crate) struct SnapshotReply {
    pub request: SandboxSnapshotRequest,
    pub snapshot: SandboxSnapshot,
}

pub(crate) fn start_snapshot(
    request: SandboxSnapshotRequest,
    saved: Arc<LoadedSandboxes>,
    storage: StateDir,
    sequence: u64,
    current: Option<StoredWorkspaceBinding>,
) -> Receiver<SnapshotReply> {
    let (sender, receiver) = flume::bounded(1);
    smol::spawn(async move {
        let snapshot =
            smol::unblock(move || load_snapshot(&saved, &storage, sequence, current.as_ref()))
                .await;
        let _ = sender.send(SnapshotReply { request, snapshot });
    })
    .detach();
    receiver
}

fn load_snapshot(
    saved: &LoadedSandboxes,
    storage: &StateDir,
    sequence: u64,
    current: Option<&StoredWorkspaceBinding>,
) -> SandboxSnapshot {
    let mut snapshot = SandboxSnapshot {
        sequence,
        instances: SnapshotState::Loading,
        providers: BTreeMap::new(),
        failures: BTreeMap::new(),
        credentials: Vec::new(),
    };
    let result = (|| -> caudra_sandbox::Result<_> {
        let controller = Controller::new(storage)?;
        snapshot.credentials = list_sandbox_credentials(storage)?;
        let records = controller.snapshots()?;
        let mut remote = Vec::new();
        for (name, provider) in &saved.saved().configuration().providers {
            match smol::block_on(controller.live_snapshot(provider)) {
                Ok(live) => {
                    remote.extend(
                        live.instances
                            .into_iter()
                            .map(|instance| (name.clone(), instance)),
                    );
                    snapshot.providers.insert(
                        name.clone(),
                        SandboxProviderSnapshot {
                            capabilities: live.doctor.capabilities.clone(),
                            catalog: SnapshotState::Ready(live.catalog),
                            doctor: Some(live.doctor),
                        },
                    );
                }
                Err(error) => {
                    snapshot.failures.insert(name.clone(), format!("{error}. Check credential reference, daemon availability and protocol with Doctor."));
                }
            }
        }
        let mut rows = Vec::new();
        for record in records {
            let live = remote.iter().find(|(provider, instance)| {
                provider == &record.provider_name
                    && record
                        .instance
                        .as_ref()
                        .is_some_and(|old| old.sandbox_id == instance.sandbox_id)
            });
            let mut blockers = controller.blockers(&record);
            let live = live.and_then(|(_, instance)| {
                if record.provider
                    != *saved
                        .saved()
                        .configuration()
                        .providers
                        .get(&record.provider_name)?
                    || controller.validate_observed(&record, instance).is_err()
                {
                    blockers.push(
                        "Authority or immutable instance identity changed; actions refused".into(),
                    );
                    None
                } else {
                    Some(instance.clone())
                }
            });
            rows.push(instance_snapshot(
                record.name.to_string(),
                record.provider_name.clone(),
                Some(record),
                live,
                blockers,
                current,
            ));
        }
        for (provider, instance) in remote {
            if !rows.iter().any(|row| {
                row.provider == provider
                    && row
                        .record
                        .as_ref()
                        .and_then(|record| record.instance.as_ref())
                        .is_some_and(|old| old.sandbox_id == instance.sandbox_id)
            }) {
                rows.push(instance_snapshot(
                    format!("{provider}/{}", instance.sandbox_id),
                    provider,
                    None,
                    Some(instance),
                    Vec::new(),
                    current,
                ));
            }
        }
        if rows.len() > caudra_config::sandbox::MAX_SANDBOX_RECORDS {
            return Err(caudra_sandbox::Error::Protocol);
        }
        Ok(rows)
    })();
    snapshot.instances = match result {
        Ok(rows) => SnapshotState::Ready(rows),
        Err(error) => SnapshotState::Unavailable(error.to_string()),
    };
    snapshot
}

fn instance_snapshot(
    id: String,
    provider: SandboxName,
    record: Option<InstanceRecord>,
    live: Option<Instance>,
    blockers: Vec<String>,
    current: Option<&StoredWorkspaceBinding>,
) -> SandboxInstanceSnapshot {
    let observed = live
        .as_ref()
        .or_else(|| record.as_ref().and_then(|record| record.instance.as_ref()));
    let state = match observed.map(|instance| &instance.state) {
        Some(InstanceState::Running) => SandboxInstanceState::Running,
        Some(InstanceState::Paused) => SandboxInstanceState::Paused,
        Some(InstanceState::Creating) => SandboxInstanceState::Creating,
        Some(InstanceState::Deleted) => SandboxInstanceState::Stopped,
        Some(_) => SandboxInstanceState::Recovering,
        None => SandboxInstanceState::Unknown,
    };
    let ready = live.as_ref().is_some_and(|instance| {
        instance.state == InstanceState::Running
            && record.as_ref().is_some_and(|record| {
                current.is_some_and(|binding| {
                    binding.sandbox_record() == Some(record.id)
                        && binding.trust_anchor().as_str()
                            == record.provider.proxy_endpoint.as_str()
                })
            })
            && instance
                .expected_workcell
                .as_ref()
                .is_some_and(|identity| current.is_some_and(|binding| identity.matches(binding)))
    });
    SandboxInstanceSnapshot {
        id,
        provider,
        state,
        workcell: if ready {
            SandboxWorkcellState::Ready
        } else {
            SandboxWorkcellState::Pending
        },
        effective: record.as_ref().and_then(|record| record.launch.clone()),
        lease_deadline: observed.and_then(|instance| instance.lease_deadline.clone()),
        retention_deadline: observed.and_then(|instance| instance.retention.deadline.clone()),
        blockers,
        record,
        live,
    }
}

pub(crate) enum LiveOperation {
    ProbeImage(ImageProbe),
    Create {
        profile: SandboxName,
        name: SandboxName,
        review: CreateReview,
        seed: Option<(PathBuf, WorkspacePath)>,
    },
    Borrow {
        provider: SandboxName,
        instance: Instance,
        name: SandboxName,
        cwd: WorkspacePath,
    },
    Attach {
        name: SandboxName,
        revision: Revision,
    },
    Control {
        name: SandboxName,
        revision: Revision,
        action: LifecycleAction,
    },
    Reconcile {
        name: SandboxName,
    },
    CancelCreate {
        name: SandboxName,
        revision: Revision,
    },
    AcknowledgeFailure {
        name: SandboxName,
        revision: Revision,
    },
    Doctor {
        provider: SandboxName,
    },
    Credential {
        reference: SandboxCredentialRef,
        key: SandboxApiKey,
    },
    Admin {
        provider: SandboxName,
        request: AdminRequest,
    },
}

pub(crate) struct LiveRequest {
    pub ticket: StoreTicket,
    pub scope: SandboxSnapshotRequest,
    pub saved: Arc<LoadedSandboxes>,
    pub operation: LiveOperation,
}

pub struct SandboxControl {
    pub name: SandboxName,
    revision: Revision,
    configuration_revision: Revision,
    action: ControlAction,
}

enum ControlAction {
    Lifecycle(LifecycleAction),
    AcknowledgeFailure,
}

impl LiveRequest {
    pub(crate) fn control(&self) -> Option<SandboxControl> {
        let (name, revision, action) = match &self.operation {
            LiveOperation::Control {
                name,
                revision,
                action,
            } => (name, revision, ControlAction::Lifecycle(action.clone())),
            LiveOperation::AcknowledgeFailure { name, revision } => {
                (name, revision, ControlAction::AcknowledgeFailure)
            }
            _ => return None,
        };
        Some(SandboxControl {
            name: name.clone(),
            revision: revision.clone(),
            configuration_revision: self.saved.saved().revision().clone(),
            action,
        })
    }
}

impl SandboxControl {
    pub fn reconnects(&self) -> bool {
        matches!(
            self.action,
            ControlAction::Lifecycle(
                LifecycleAction::Resume { .. }
                    | LifecycleAction::Extend { .. }
                    | LifecycleAction::ApplyPolicy { .. }
            )
        )
    }

    pub fn execute(self, storage: &StateDir) -> Result<InstanceRecord, String> {
        let execute = || -> color_eyre::Result<InstanceRecord> {
            if SandboxStore::user_global()?.load()?.saved().revision()
                != &self.configuration_revision
            {
                color_eyre::eyre::bail!(
                    "Saved configuration changed. Reload and review; nothing was sent."
                );
            }
            let controller = Controller::new(storage)?;
            Ok(match self.action {
                ControlAction::Lifecycle(action) => {
                    smol::block_on(controller.action_at(&self.name, &self.revision, action))?
                }
                ControlAction::AcknowledgeFailure => {
                    controller.acknowledge_lifecycle_failure(&self.name, &self.revision)?
                }
            })
        };
        execute().map_err(|error| error.to_string())
    }
}

pub(crate) enum LiveOutcome {
    ImageProbe(ProbedImage),
    Report(String),
    Attachment(SandboxAttachment),
    Seed {
        name: SandboxName,
        revision: Revision,
        local: PathBuf,
        remote: WorkspacePath,
    },
}

pub(crate) struct LiveReply {
    pub ticket: StoreTicket,
    pub scope: SandboxSnapshotRequest,
    pub result: Result<LiveOutcome, String>,
}

pub(crate) fn start_live(
    request: LiveRequest,
    storage: StateDir,
    connector: Option<SandboxConnector>,
) -> Receiver<LiveReply> {
    let (sender, receiver) = flume::bounded(1);
    smol::spawn(async move {
        let reply = smol::unblock(move || {
            let result = execute_live(&request, &storage, connector.as_ref());
            LiveReply {
                ticket: request.ticket,
                scope: request.scope,
                result,
            }
        })
        .await;
        let _ = sender.send(reply);
    })
    .detach();
    receiver
}

fn execute_live(
    request: &LiveRequest,
    storage: &StateDir,
    connector: Option<&SandboxConnector>,
) -> Result<LiveOutcome, String> {
    let execute = || -> color_eyre::Result<LiveOutcome> {
        let loaded = SandboxStore::user_global()?.load()?;
        if loaded.saved().revision() != request.saved.saved().revision() {
            color_eyre::eyre::bail!(
                "Saved configuration changed. Reload and review; nothing was sent."
            );
        }
        let controller = Controller::new(storage)?;
        let provider = |name: &SandboxName| {
            loaded
                .saved()
                .configuration()
                .providers
                .get(name)
                .ok_or(caudra_sandbox::Error::Missing)
        };
        let record = match &request.operation {
            LiveOperation::Create {
                profile,
                name,
                review,
                seed,
            } => {
                let record = smol::block_on(controller.create_reviewed(
                    loaded.saved(),
                    profile,
                    name.clone(),
                    review,
                ))?;
                if let Some((local, remote)) = seed {
                    let _verified = connector.ok_or_else(|| {
                        color_eyre::eyre::eyre!(
                            "Workcell verification unavailable; initial seed not started"
                        )
                    })?(name.clone(), record.revision()?)
                    .map_err(|error| color_eyre::eyre::eyre!(error))?;
                    return Ok(LiveOutcome::Seed {
                        name: name.clone(),
                        revision: controller.store().get(name)?.revision()?,
                        local: local.clone(),
                        remote: remote.clone(),
                    });
                }
                Some(record)
            }
            LiveOperation::Borrow {
                provider: name,
                instance,
                name: local,
                cwd,
            } => {
                let record = smol::block_on(controller.borrow_at(
                    local.clone(),
                    name.clone(),
                    provider(name)?.clone(),
                    instance,
                    cwd.clone(),
                ))?;
                return connector.ok_or_else(|| {
                    color_eyre::eyre::eyre!("Workspace transition connector unavailable")
                })?(local.clone(), record.revision()?)
                .map(LiveOutcome::Attachment)
                .map_err(|error| color_eyre::eyre::eyre!(error));
            }
            LiveOperation::Attach { name, revision } => {
                if &controller.store().get(name)?.revision()? != revision {
                    return Err(caudra_sandbox::Error::ReviewChanged.into());
                }
                return connector.ok_or_else(|| {
                    color_eyre::eyre::eyre!("Workspace transition connector unavailable")
                })?(name.clone(), revision.clone())
                .map(LiveOutcome::Attachment)
                .map_err(|error| color_eyre::eyre::eyre!(error));
            }
            LiveOperation::Control {
                name,
                revision,
                action,
            } => Some(smol::block_on(controller.action_at(
                name,
                revision,
                action.clone(),
            ))?),
            LiveOperation::Reconcile { name } => Some(smol::block_on(controller.inspect(name))?),
            LiveOperation::CancelCreate { name, revision } => {
                Some(smol::block_on(controller.cancel_create(name, revision))?)
            }
            LiveOperation::AcknowledgeFailure { name, revision } => {
                let record = controller.acknowledge_lifecycle_failure(name, revision)?;
                return Ok(LiveOutcome::Report(format!(
                    "Failure acknowledged, NOT successful application. No remote request or retry was sent.\n{}",
                    serde_json::to_string_pretty(&record)?
                )));
            }
            LiveOperation::Doctor { provider: name } => {
                let report = smol::block_on(controller.doctor(provider(name)?))?;
                return Ok(LiveOutcome::Report(format!(
                    "{}\n\nVM availability is not Workcell readiness. Attach performs the authenticated handshake. Missing credentials: edit the purpose-store reference. Incompatible image: import/build a Workcell-enabled image offline. Pending recovery: Reconcile, never replay Create.\nLocal KVM: {:?}",
                    serde_json::to_string_pretty(&report)?,
                    caudra_sandbox::local_admin::inspect_local(provider(name)?)
                )));
            }
            LiveOperation::Credential { reference, key } => {
                if key.expose_secret().len() < 32 {
                    return Err(caudra_sandbox::Error::Credential.into());
                }
                save_sandbox_api_key(storage, reference, key)?;
                return Ok(LiveOutcome::Report(format!(
                    "Saved {reference} in the lifecycle purpose store. No VM or workspace action."
                )));
            }
            LiveOperation::ProbeImage(probe) => {
                return Ok(LiveOutcome::ImageProbe(probe.execute_approved()?));
            }
            LiveOperation::Admin {
                provider: name,
                request,
            } => {
                let output = request.command(provider(name)?)?.execute_approved()?;
                return Ok(LiveOutcome::Report(serde_json::to_string_pretty(&output)?));
            }
        };
        Ok(LiveOutcome::Report(format!(
            "{}\n\nSelected workspace is unchanged. Create/Resume does not attach. Close does not cancel. Reconcile observes durable outcomes; never replay an unknown Create.",
            serde_json::to_string_pretty(&record)?
        )))
    };
    execute().map_err(|error| format!("{error}\nDraft and workspace retained. An accepted remote request may still complete: Reconcile before retrying."))
}

pub(crate) enum StoreEffect {
    Load,
    Save {
        baseline: Arc<LoadedSandboxes>,
        draft: SandboxDraft,
    },
    Export {
        path: PathBuf,
        draft: SandboxDraft,
        source: String,
    },
}

pub(crate) enum StoreResult {
    Loaded(Arc<LoadedSandboxes>),
    Saved(Arc<LoadedSandboxes>),
    Conflict(Result<Arc<LoadedSandboxes>, SandboxStoreError>),
    Exported,
    Failed(SandboxStoreError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoreTicket {
    pub session: u64,
    pub operation: u64,
    pub draft_revision: u64,
}

pub(crate) struct StoreReply {
    pub ticket: StoreTicket,
    pub result: StoreResult,
}

pub(crate) fn start_store_effect(ticket: StoreTicket, effect: StoreEffect) -> Receiver<StoreReply> {
    let (sender, receiver) = flume::bounded(1);
    smol::spawn(async move {
        let result =
            smol::unblock(move || execute_store_effect(SandboxStore::user_global(), effect)).await;
        let _ = sender.send(StoreReply { ticket, result });
    })
    .detach();
    receiver
}

pub(crate) fn execute_store_effect(
    store: Result<SandboxStore, SandboxStoreError>,
    effect: StoreEffect,
) -> StoreResult {
    match effect {
        StoreEffect::Load => match store.and_then(|store| store.load()) {
            Ok(loaded) => StoreResult::Loaded(Arc::new(loaded)),
            Err(error) => StoreResult::Failed(error),
        },
        StoreEffect::Save { baseline, draft } => {
            let store = match store {
                Ok(store) => store,
                Err(error) => return StoreResult::Failed(error),
            };
            match store.save(&baseline, &draft) {
                Ok(saved) => StoreResult::Saved(Arc::new(saved)),
                Err(SandboxStoreError::File(PrivateFileError::Conflict)) => {
                    StoreResult::Conflict(store.load().map(Arc::new))
                }
                Err(error) => StoreResult::Failed(error),
            }
        }
        StoreEffect::Export {
            path,
            draft,
            source,
        } => {
            let result = SandboxDraft::import(&source)
                .map_err(SandboxStoreError::from)
                .and_then(|parsed| {
                    if parsed != draft {
                        return Err(caudra_config::sandbox::SandboxError::Document.into());
                    }
                    PrivateFile::new(path, MAX_SANDBOX_FILE_BYTES)?
                        .compare_exchange(&FileRevision::Missing, Some(source.as_bytes()))?;
                    Ok(())
                });
            match result {
                Ok(()) => StoreResult::Exported,
                Err(error) => StoreResult::Failed(error),
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{SandboxSnapshotRequest, SnapshotState, start_snapshot};
    use caudra_config::sandbox::persistence::SandboxStore;
    use caudra_storage::{
        StateDir,
        id::CaudraId,
        sandbox_auth::{SandboxApiKey, SandboxCredentialRef, save_sandbox_api_key},
    };
    use std::{fs::Permissions, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

    const KEY: &str = "purpose-scoped-test-key-0123456789abcdef";
    const NAME: &str = "lifecycle";

    #[test]
    fn asynchronous_snapshot_returns_scope_and_only_credential_references() {
        let temp = tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let reference = SandboxCredentialRef::new(NAME).unwrap();
        save_sandbox_api_key(
            &storage,
            &reference,
            &SandboxApiKey::new(KEY.into()).unwrap(),
        )
        .unwrap();
        let loaded = Arc::new(
            SandboxStore::from_config_dir(&temp.path().join("config"))
                .unwrap()
                .load()
                .unwrap(),
        );
        let request = SandboxSnapshotRequest {
            conversation: CaudraId::generate(),
            manager_session: 4,
            configuration_revision: loaded.saved().revision().clone(),
            configuration_epoch: 2,
        };
        let reply = start_snapshot(request.clone(), loaded, storage, 7, None)
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        assert_eq!(reply.request, request);
        assert_eq!(reply.snapshot.sequence, 7);
        assert_eq!(reply.snapshot.credentials, vec![reference]);
        assert!(matches!(&reply.snapshot.instances, SnapshotState::Ready(rows) if rows.is_empty()));
        assert!(!format!("{:?}", reply.snapshot).contains(KEY));
    }
}
