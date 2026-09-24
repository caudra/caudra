use caudra_config::{
    sandbox::{
        Architecture, Enforcement, LeaseSeconds, ProviderCapabilities, ResolvedLaunch,
        ResourceRange, Resources as ProfileResources, Revision, SandboxName, SandboxProvider,
        SavedSandboxes, TemplateCatalog, TemplateEntry, TlsMode, persistence::SandboxStore,
    },
    workcell::{ExpectedWorkcellId, RemoteWorkcellSelection, WorkcellEndpoint, WorkcellSourceRef},
};
use caudra_storage::{
    StateDir,
    id::CaudraId,
    remote_operation_journal::RemoteOperationJournal,
    sandbox_auth::load_sandbox_api_key,
    sessions::{SessionDatabase, SessionLease},
    workflow::WorkflowRunStatus,
    workspace_binding::StoredWorkspaceBinding,
};
use caudra_workspace::WorkspacePath;
use serde::Serialize;
use std::{
    num::NonZeroU32,
    result::Result as StdResult,
    time::{Duration, Instant},
};
use uuid::Uuid;

use crate::{
    CreateIntent, Error, InstanceRecord, LifecycleClient, LifecycleIntent, Ownership, Result,
    RuntimeLease, Store,
    dto::{
        Create, Discovery, Expected, Instance, InstanceState, MAX_IMAGE_BYTES, MIB_PER_GIB,
        Operation, OperationStatus, PROTOCOL_VERSION, Policy, Resources, TRANSFER_PROTOCOL,
        Template, timestamp,
    },
};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const READINESS_BUDGET: Duration = Duration::from_secs(120);
const MIB_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumePolicy {
    Refuse,
    Confirmed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartPhase {
    Pause,
    Resume,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "restart stopped during {phase:?}: {source}; inspect before further action, never replay an unknown request"
)]
pub struct RestartFailure {
    pub phase: RestartPhase,
    #[source]
    pub source: Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleAction {
    Pause,
    Resume { lease_seconds: LeaseSeconds },
    Extend { lease_seconds: LeaseSeconds },
    Delete { destroy_borrowed: bool },
    Detach,
    ApplyPolicy { policy: Policy },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkReconcileStatus {
    Applied,
    NoChange,
    Deferred,
    Excluded,
}

#[derive(Debug, Clone, Serialize)]
pub struct Doctor {
    pub discovery: Discovery,
    pub templates: Vec<Template>,
    pub capabilities: ProviderCapabilities,
}

pub struct LiveSnapshot {
    pub doctor: Doctor,
    pub catalog: TemplateCatalog,
    pub instances: Vec<Instance>,
}

#[derive(Clone)]
pub struct CreateReview {
    pub owner_id: String,
    pub image_sha256: Revision,
    pub launch_revision: Revision,
}

pub struct Controller {
    store: Store,
}

/// No bearer is serializable or printable. The caller must complete a live Workcell
/// handshake and call confirm_attachment before publishing tools to an agent.
pub struct AttachTicket {
    pub selection: RemoteWorkcellSelection,
    pub record: InstanceRecord,
    token: String,
    lease: RuntimeLease,
}

impl AttachTicket {
    pub fn take_token(&mut self) -> String {
        std::mem::take(&mut self.token)
    }
}

impl Controller {
    pub fn new(state: &StateDir) -> Result<Self> {
        Ok(Self {
            store: Store::open(state)?,
        })
    }
    pub fn store(&self) -> &Store {
        &self.store
    }
    pub fn snapshots(&self) -> Result<Vec<InstanceRecord>> {
        self.store.list()
    }

    pub fn validate_observed(&self, record: &InstanceRecord, instance: &Instance) -> Result<()> {
        validate_instance(record, instance)
    }

    fn client(&self, provider: &SandboxProvider) -> Result<LifecycleClient> {
        let key = load_sandbox_api_key(self.store.state(), &provider.credential_ref)?
            .ok_or(Error::Credential)?;
        LifecycleClient::new(provider.api_endpoint.clone(), key)
    }

    async fn owned_client(&self, record: &InstanceRecord) -> Result<(LifecycleClient, Discovery)> {
        let client = self.client(&record.provider)?;
        let discovery = client.discover().await?;
        if discovery.owner_id != record.owner_id {
            return Err(Error::Identity);
        }
        Ok((client, discovery))
    }

    pub async fn doctor(&self, provider: &SandboxProvider) -> Result<Doctor> {
        let client = self.client(provider)?;
        let discovery = client.discover().await?;
        let templates = client.templates().await?;
        for template in &templates {
            validate_template(template)?;
        }
        let capabilities = capabilities(provider, &discovery)?;
        Ok(Doctor {
            discovery,
            templates,
            capabilities,
        })
    }

    pub async fn provider_instances(&self, provider: &SandboxProvider) -> Result<Vec<Instance>> {
        let client = self.client(provider)?;
        let discovery = client.discover().await?;
        client.instances(&discovery.owner_id).await
    }

    pub async fn live_snapshot(&self, provider: &SandboxProvider) -> Result<LiveSnapshot> {
        let doctor = self.doctor(provider).await?;
        let catalog = TemplateCatalog::without_guest_layout(
            doctor
                .templates
                .iter()
                .map(template_entry)
                .collect::<Result<_>>()?,
        )?;
        let instances = self
            .client(provider)?
            .instances(&doctor.discovery.owner_id)
            .await?;
        Ok(LiveSnapshot {
            doctor,
            catalog,
            instances,
        })
    }

    pub async fn create(
        &self,
        saved: &SavedSandboxes,
        profile_name: &SandboxName,
        name: SandboxName,
    ) -> Result<InstanceRecord> {
        self.create_with_review(saved, profile_name, name, None)
            .await
    }

    pub async fn create_reviewed(
        &self,
        saved: &SavedSandboxes,
        profile_name: &SandboxName,
        name: SandboxName,
        review: &CreateReview,
    ) -> Result<InstanceRecord> {
        self.create_with_review(saved, profile_name, name, Some(review))
            .await
    }

    async fn create_with_review(
        &self,
        saved: &SavedSandboxes,
        profile_name: &SandboxName,
        name: SandboxName,
        review: Option<&CreateReview>,
    ) -> Result<InstanceRecord> {
        if self.store.list()?.iter().any(|record| record.name == name) {
            return Err(Error::Exists);
        }
        let profile = saved
            .configuration()
            .profiles
            .get(profile_name)
            .ok_or(Error::Missing)?;
        let provider = saved
            .configuration()
            .providers
            .get(&profile.provider)
            .ok_or(Error::Missing)?;
        let client = self.client(provider)?;
        let discovery = client.discover().await?;
        let template = client.template(&profile.template, None).await?;
        let catalog = TemplateCatalog::without_guest_layout(vec![template_entry(&template)?])?;
        let launch =
            saved.resolve_launch(profile_name, &capabilities(provider, &discovery)?, &catalog)?;
        if review.is_some_and(|review| {
            review.owner_id != discovery.owner_id
                || review.image_sha256 != template.image_sha256
                || &review.launch_revision != launch.revision()
        }) {
            return Err(Error::ReviewChanged);
        }
        let request = create_request(&launch)?;
        let key = Uuid::from_bytes(*CaudraId::generate().as_bytes()).to_string();
        let record = InstanceRecord {
            id: CaudraId::generate(),
            name,
            ownership: Ownership::Owned,
            provider_name: profile.provider.clone(),
            provider: provider.clone(),
            owner_id: discovery.owner_id,
            cwd: profile.cwd.clone(),
            launch: Some(launch),
            template,
            create: Some(CreateIntent {
                key: key.clone(),
                request: request.clone(),
                operation: None,
            }),
            instance: None,
            lifecycle: None,
            workcell_binding: None,
            detached: false,
        };
        self.store.reserve(record.clone())?;
        let operation = client.create(&key, &request).await?;
        self.record_operation(&record, operation)
    }

    fn record_operation(
        &self,
        record: &InstanceRecord,
        operation: Operation,
    ) -> Result<InstanceRecord> {
        let intent = record.create.as_ref().ok_or(Error::Store)?;
        if operation.operation_id != intent.key
            || operation.owner_id != record.owner_id
            || operation.request_digest.len() != 64
            || !operation
                .request_digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(Error::Identity);
        }
        if let Some(previous) = &intent.operation
            && (operation.sandbox_id != previous.sandbox_id
                || operation.execution_id != previous.execution_id
                || operation.request_digest != previous.request_digest)
        {
            return Err(Error::Identity);
        }
        let mut next = record.clone();
        if let Some(instance) = &operation.instance {
            if instance.sandbox_id != operation.sandbox_id {
                return Err(Error::Identity);
            }
            validate_instance(record, instance)?;
            next.instance = Some(instance.clone());
        }
        next.create.as_mut().ok_or(Error::Store)?.operation = Some(operation);
        self.store.replace(record, &next)?;
        Ok(next)
    }

    /// Inspection is the only recovery path, including a reservation whose sender
    /// crashed before send. An unavailable history never turns into a new create.
    pub async fn inspect(&self, name: &SandboxName) -> Result<InstanceRecord> {
        let record = self.store.get(name)?;
        let recovering = record
            .lifecycle
            .as_ref()
            .is_some_and(LifecycleIntent::is_pending);
        let _control = recovering
            .then(|| self.store.control_lease(&record))
            .transpose()?;
        let (client, _) = self.owned_client(&record).await?;
        let mut current = if let Some(intent) = &record.create {
            self.record_operation(&record, client.operation(&intent.key).await?)?
        } else {
            let id = &record.instance.as_ref().ok_or(Error::Store)?.sandbox_id;
            self.record_instance(&record, client.instance(id).await?)?
        };
        if recovering && lifecycle_satisfied(&current) {
            let previous = current.clone();
            let revision = current
                .instance
                .as_ref()
                .map(|instance| instance.revision)
                .or_else(|| current.create.as_ref()?.operation.as_ref().map(|_| 0))
                .ok_or(Error::Unresolved)?;
            current
                .lifecycle
                .as_mut()
                .ok_or(Error::Store)?
                .observed_revision = Some(revision);
            self.store.replace(&previous, &current)?;
        }
        Ok(current)
    }

    pub async fn wait_ready(&self, name: &SandboxName) -> Result<InstanceRecord> {
        let deadline = Instant::now() + READINESS_BUDGET;
        loop {
            let record = self.inspect(name).await?;
            let operation = record
                .create
                .as_ref()
                .and_then(|intent| intent.operation.as_ref());
            if record
                .instance
                .as_ref()
                .is_some_and(|instance| instance.state == InstanceState::Running)
                && operation.is_none_or(|op| {
                    op.status == OperationStatus::Succeeded && !op.cancel_requested
                })
                && !record
                    .lifecycle
                    .as_ref()
                    .is_some_and(LifecycleIntent::is_pending)
            {
                return Ok(record);
            }
            if operation.is_none_or(|op| {
                !matches!(
                    op.status,
                    OperationStatus::Creating | OperationStatus::CleanupPending
                )
            }) {
                return Err(Error::NotReady);
            }
            if Instant::now() >= deadline {
                return Err(Error::Unresolved);
            }
            smol::Timer::after(POLL_INTERVAL).await;
        }
    }

    pub async fn borrow(
        &self,
        name: SandboxName,
        provider_name: SandboxName,
        provider: SandboxProvider,
        sandbox_id: &str,
        cwd: WorkspacePath,
    ) -> Result<InstanceRecord> {
        self.borrow_reviewed(name, provider_name, provider, sandbox_id, cwd, None)
            .await
    }

    pub async fn borrow_at(
        &self,
        name: SandboxName,
        provider_name: SandboxName,
        provider: SandboxProvider,
        expected: &Instance,
        cwd: WorkspacePath,
    ) -> Result<InstanceRecord> {
        self.borrow_reviewed(
            name,
            provider_name,
            provider,
            &expected.sandbox_id,
            cwd,
            Some(expected),
        )
        .await
    }

    async fn borrow_reviewed(
        &self,
        name: SandboxName,
        provider_name: SandboxName,
        provider: SandboxProvider,
        sandbox_id: &str,
        cwd: WorkspacePath,
        expected: Option<&Instance>,
    ) -> Result<InstanceRecord> {
        if self.store.list()?.iter().any(|record| record.name == name) {
            return Err(Error::Exists);
        }
        let client = self.client(&provider)?;
        let discovery = client.discover().await?;
        let instance = client.instance(sandbox_id).await?;
        instance.validate(&discovery.owner_id)?;
        if expected.is_some_and(|expected| expected != &instance) {
            return Err(Error::ReviewChanged);
        }
        if instance.sandbox_id != sandbox_id {
            return Err(Error::Identity);
        }
        let revision = Revision::parse(&instance.template.revision)?;
        let template = client
            .template(&instance.template.id, Some(&revision))
            .await?;
        if !template_entry(&template)?.workcell_compatible {
            return Err(Error::Protocol);
        }
        let record = InstanceRecord {
            id: CaudraId::generate(),
            name,
            ownership: Ownership::Borrowed,
            provider_name,
            provider,
            owner_id: discovery.owner_id,
            cwd,
            launch: None,
            template,
            create: None,
            instance: None,
            lifecycle: None,
            workcell_binding: None,
            detached: false,
        };
        validate_instance(&record, &instance)?;
        let record = InstanceRecord {
            instance: Some(instance),
            ..record
        };
        self.store.reserve(record.clone())?;
        Ok(record)
    }

    fn record_instance(
        &self,
        record: &InstanceRecord,
        instance: Instance,
    ) -> Result<InstanceRecord> {
        validate_instance(record, &instance)?;
        let next = InstanceRecord {
            instance: Some(instance),
            ..record.clone()
        };
        self.store.replace(record, &next)?;
        Ok(next)
    }

    pub async fn action(
        &self,
        name: &SandboxName,
        action: LifecycleAction,
    ) -> Result<InstanceRecord> {
        self.action_reviewed(name, None, action).await
    }

    pub async fn action_at(
        &self,
        name: &SandboxName,
        revision: &Revision,
        action: LifecycleAction,
    ) -> Result<InstanceRecord> {
        self.action_reviewed(name, Some(revision), action).await
    }

    /// Cold restart of a reviewed, owned, persistent running instance, never recreation.
    /// Callers must drain runtime leases first. Only authoritative pause success permits
    /// resume; interruption between phases leaves a paused instance for explicit recovery.
    pub async fn restart_at(
        &self,
        name: &SandboxName,
        revision: &Revision,
        lease_seconds: LeaseSeconds,
    ) -> StdResult<InstanceRecord, RestartFailure> {
        let mut phase = RestartPhase::Pause;
        let result = async {
            let record = self.store.get(name)?;
            let _control = self.store.control_lease(&record)?;
            if &record.revision()? != revision {
                return Err(Error::ReviewChanged);
            }
            if record.ownership != Ownership::Owned {
                return Err(Error::Borrowed);
            }
            let instance = record.instance.as_ref().ok_or(Error::NotReady)?;
            if !instance.persistent {
                return Err(Error::PauseUnsupported);
            }
            if record.detached || instance.state != InstanceState::Running {
                return Err(Error::NotReady);
            }
            if record
                .lifecycle
                .as_ref()
                .is_some_and(LifecycleIntent::is_pending)
            {
                return Err(Error::Unresolved);
            }
            let lease = self.store.lease(&record, true)?;
            self.require_quiescent(&record)?;
            let (_, discovery) = self.owned_client(&record).await?;
            check_lease(lease_seconds, &discovery)?;
            let paused = self
                .action_locked(
                    name,
                    Some(revision),
                    LifecycleAction::Pause,
                    None,
                    Some(&lease),
                )
                .await?;
            phase = RestartPhase::Resume;
            self.action_locked(
                name,
                Some(&paused.revision()?),
                LifecycleAction::Resume { lease_seconds },
                None,
                Some(&lease),
            )
            .await
        }
        .await;
        result.map_err(|source| RestartFailure { phase, source })
    }

    async fn action_reviewed(
        &self,
        name: &SandboxName,
        revision: Option<&Revision>,
        action: LifecycleAction,
    ) -> Result<InstanceRecord> {
        let record = self.store.get(name)?;
        let _control = self.store.control_lease(&record)?;
        self.action_locked(name, revision, action, None, None).await
    }

    async fn action_locked(
        &self,
        name: &SandboxName,
        revision: Option<&Revision>,
        action: LifecycleAction,
        saved_guard: Option<(&SandboxStore, &Revision)>,
        held_lease: Option<&RuntimeLease>,
    ) -> Result<InstanceRecord> {
        let record = self.store.get(name)?;
        if revision.is_some_and(|expected| record.revision().ok().as_ref() != Some(expected)) {
            return Err(Error::ReviewChanged);
        }
        if action == LifecycleAction::Pause
            && record
                .instance
                .as_ref()
                .is_some_and(|instance| !instance.persistent)
        {
            return Err(Error::PauseUnsupported);
        }
        let _lease = if held_lease.is_none() {
            Some(self.store.lease(
                &record,
                saved_guard.is_none() && !matches!(action, LifecycleAction::Extend { .. }),
            )?)
        } else {
            None
        };
        if matches!(
            action,
            LifecycleAction::Pause
                | LifecycleAction::Delete { .. }
                | LifecycleAction::Detach
                | LifecycleAction::ApplyPolicy { .. }
        ) {
            if saved_guard.is_some() {
                self.require_resolved_mutations(&record)?;
            } else {
                self.require_quiescent(&record)?;
            }
        }
        if action == LifecycleAction::Detach
            || (matches!(
                action,
                LifecycleAction::Delete {
                    destroy_borrowed: false
                }
            ) && record.ownership == Ownership::Borrowed)
        {
            let next = InstanceRecord {
                detached: true,
                ..record.clone()
            };
            self.store.replace(&record, &next)?;
            return Ok(next);
        }
        if record.ownership == Ownership::Borrowed && action == LifecycleAction::Pause {
            return Err(Error::Borrowed);
        }
        if record
            .lifecycle
            .as_ref()
            .is_some_and(LifecycleIntent::is_pending)
        {
            return Err(Error::Unresolved);
        }
        let (client, discovery) = self.owned_client(&record).await?;
        let instance = record.instance.as_ref().ok_or(Error::NotReady)?;
        let policy = if let LifecycleAction::ApplyPolicy { policy } = &action {
            policy.validate_for(&discovery, &record.template.manifest, Some(instance))?;
            Some(policy.canonical()?)
        } else {
            None
        };
        let (route, lease_seconds) = match &action {
            LifecycleAction::Pause => ("pause", None),
            LifecycleAction::Resume { lease_seconds } => ("resume", Some(*lease_seconds)),
            LifecycleAction::Extend { lease_seconds } => ("renew", Some(*lease_seconds)),
            LifecycleAction::Delete { .. } => ("delete", None),
            LifecycleAction::ApplyPolicy { .. } => {
                if !discovery.capabilities.egress_policy || !instance.egress.enforced {
                    return Err(Error::Protocol);
                }
                ("policy", None)
            }
            LifecycleAction::Detach => return Err(Error::Protocol),
        };
        if let Some(lease) = lease_seconds {
            check_lease(lease, &discovery)?;
        }
        if route == "renew" && instance.lease_deadline.is_none() {
            if instance.state != InstanceState::Running {
                return Err(Error::Lease);
            }
            if lease_seconds.and_then(LeaseSeconds::finite).is_some() {
                return Err(Error::LeaseNoExpiry);
            }
        }
        let minimum_lease_deadline = lease_seconds
            .and_then(LeaseSeconds::finite)
            .map(|seconds| -> Result<String> {
                let requested = timestamp(&discovery.server_time)?
                    .checked_add(Duration::from_secs(u64::from(seconds.get())))
                    .map_err(|_| Error::Lease)?;
                let deadline = if route == "renew" {
                    requested.max(timestamp(
                        instance.lease_deadline.as_deref().ok_or(Error::Lease)?,
                    )?)
                } else {
                    requested
                };
                Ok(deadline.to_string())
            })
            .transpose()?;
        let mut intent = LifecycleIntent {
            action: route.into(),
            expected: instance.expected(),
            lease_seconds,
            observed_revision: None,
            policy_revision: policy.as_ref().map(Policy::revision).transpose()?,
            policy,
            minimum_lease_deadline,
            allow_equal_revision: matches!(route, "pause" | "renew" | "policy"),
            failure_acknowledged: false,
        };
        intent.allow_equal_revision = validate_lifecycle_result(&intent, instance).is_ok();
        let pending = InstanceRecord {
            lifecycle: Some(intent),
            ..record.clone()
        };
        if let Some((store, revision)) = saved_guard
            && store.load()?.saved().revision() != revision
        {
            return Err(Error::ReviewChanged);
        }
        self.store.replace(&record, &pending)?;
        let intent = pending.lifecycle.as_ref().ok_or(Error::Store)?;
        let response = if let Some(policy) = &intent.policy {
            client
                .apply_policy(&instance.sandbox_id, &instance.expected(), policy)
                .await
        } else {
            client
                .control(
                    &instance.sandbox_id,
                    route,
                    &instance.expected(),
                    lease_seconds,
                )
                .await
        };
        let response = self.lifecycle_response(&record, &pending, response)?;
        validate_instance(&record, &response)?;
        validate_lifecycle_result(intent, &response)?;
        if response.revision == instance.revision && response != *instance {
            return Err(Error::Identity);
        }
        let mut next = pending.clone();
        next.lifecycle
            .as_mut()
            .ok_or(Error::Store)?
            .observed_revision = Some(response.revision);
        next.instance = Some(response);
        self.store.replace(&pending, &next)?;
        if let Some((store, revision)) = saved_guard
            && store.load()?.saved().revision() != revision
        {
            return Err(Error::NetworkChanged);
        }
        Ok(next)
    }

    fn lifecycle_response<T>(
        &self,
        previous: &InstanceRecord,
        pending: &InstanceRecord,
        response: Result<T>,
    ) -> Result<T> {
        if let Err(
            refusal @ Error::Daemon {
                outcome_unknown: false,
                ..
            },
        ) = response
        {
            return match self.store.replace(pending, previous) {
                Ok(()) => Err(refusal),
                Err(cleanup) => Err(Error::LifecycleRefusalCleanup {
                    refusal: Box::new(refusal),
                    cleanup: Box::new(cleanup),
                }),
            };
        }
        response
    }

    pub async fn reconcile_saved_network(
        &self,
        name: &SandboxName,
        saved: &SandboxStore,
    ) -> Result<(NetworkReconcileStatus, InstanceRecord)> {
        self.reconcile_network(name, saved, false, None).await
    }

    pub async fn reconcile_saved_network_at(
        &self,
        name: &SandboxName,
        saved: &SandboxStore,
        expected_revision: &Revision,
    ) -> Result<(NetworkReconcileStatus, InstanceRecord)> {
        self.reconcile_network(name, saved, false, Some(expected_revision))
            .await
    }

    async fn reconcile_network(
        &self,
        name: &SandboxName,
        saved: &SandboxStore,
        selected: bool,
        expected_revision: Option<&Revision>,
    ) -> Result<(NetworkReconcileStatus, InstanceRecord)> {
        let record = self.store.get(name)?;
        let _control = self.store.control_lease(&record)?;
        let record = self.store.get(name)?;
        if record.ownership != Ownership::Owned || (!selected && record.detached) {
            return Ok((NetworkReconcileStatus::Excluded, record));
        }
        if record.launch.is_none() {
            return Ok((NetworkReconcileStatus::Excluded, record));
        }
        if record
            .lifecycle
            .as_ref()
            .is_some_and(LifecycleIntent::is_pending)
        {
            return Err(Error::Unresolved);
        }
        let loaded = saved.load()?;
        if expected_revision.is_some_and(|revision| loaded.saved().revision() != revision) {
            return Err(Error::ReviewChanged);
        }
        let (client, _) = self.owned_client(&record).await?;
        let instance = record.instance.as_ref().ok_or(Error::NotReady)?;
        if record
            .create
            .as_ref()
            .and_then(|intent| intent.operation.as_ref())
            .is_some_and(|operation| operation.cancel_requested)
        {
            return Err(Error::NotReady);
        }
        let observed = client.instance(&instance.sandbox_id).await?;
        if saved.load()?.saved().revision() != loaded.saved().revision() {
            return Err(Error::ReviewChanged);
        }
        let record = self.record_instance(&record, observed)?;
        let launch = record.launch.as_ref().ok_or(Error::Store)?;
        let network = loaded
            .saved()
            .configuration()
            .networks
            .get(&launch.configuration().profile.value().network)
            .ok_or(Error::MissingNetwork)?;
        let instance = record.instance.as_ref().ok_or(Error::NotReady)?;
        if instance.state == InstanceState::Paused {
            return Ok((NetworkReconcileStatus::Deferred, record));
        }
        if instance.state != InstanceState::Running {
            return Err(Error::NotReady);
        }
        if network.enforcement != launch.configuration().network.value().enforcement
            || (network.enforcement == Enforcement::Required) != instance.egress.enforced
        {
            return Err(Error::NetworkReview);
        }
        if network.enforcement == Enforcement::Off {
            return Ok((NetworkReconcileStatus::NoChange, record));
        }
        let policy = Policy {
            mode: match network.tls_mode {
                TlsMode::SniOnly => "sni-only",
                TlsMode::Mitm => "mitm",
            }
            .into(),
            domains: network
                .domains
                .iter()
                .map(|rule| rule.as_str().to_owned())
                .collect(),
            cidrs: network
                .cidrs
                .iter()
                .map(|rule| rule.as_str().to_owned())
                .collect(),
        }
        .canonical()?;
        let previous = instance
            .egress
            .policy
            .as_ref()
            .ok_or(Error::NetworkReview)?;
        if previous.mode != policy.mode {
            return Err(Error::NetworkReview);
        }
        let policy_revision = policy.revision()?;
        if previous.canonical()? == policy
            && instance.egress.revision == policy_revision
            && instance.egress.effective_revision.as_deref() == Some(policy_revision.as_str())
        {
            return Ok((NetworkReconcileStatus::NoChange, record));
        }
        let next = self
            .action_locked(
                name,
                Some(&record.revision()?),
                LifecycleAction::ApplyPolicy { policy },
                Some((saved, loaded.saved().revision())),
                None,
            )
            .await?;
        Ok((NetworkReconcileStatus::Applied, next))
    }

    /// Abandon an unresolved request after explicitly reviewing its observed state and intent.
    /// This records failure acknowledgement, not successful application, and sends no request.
    pub fn acknowledge_lifecycle_failure(
        &self,
        name: &SandboxName,
        revision: &Revision,
    ) -> Result<InstanceRecord> {
        let record = self.store.get(name)?;
        let _control = self.store.control_lease(&record)?;
        if &record.revision()? != revision {
            return Err(Error::ReviewChanged);
        }
        let _lease = self.store.lease(&record, true)?;
        self.require_quiescent(&record)?;
        let mut next = record.clone();
        let intent = next
            .lifecycle
            .as_mut()
            .filter(|intent| intent.is_pending())
            .ok_or(Error::Unresolved)?;
        intent.failure_acknowledged = true;
        self.store.replace(&record, &next)?;
        Ok(next)
    }

    fn require_quiescent(&self, record: &InstanceRecord) -> Result<()> {
        self.require_resolved_mutations(record)?;
        self.require_idle_sessions(record)
    }

    fn require_resolved_mutations(&self, record: &InstanceRecord) -> Result<()> {
        let Some(binding) = &record.workcell_binding else {
            return Ok(());
        };
        let journal = RemoteOperationJournal::open(self.store.state()).map_err(|_| Error::Store)?;
        if journal
            .list_pending(binding)
            .map_err(|_| Error::Store)?
            .iter()
            .any(|record| record.reachable_from(binding))
        {
            return Err(Error::Busy);
        }
        Ok(())
    }

    fn require_idle_sessions(&self, record: &InstanceRecord) -> Result<()> {
        let Some(binding) = &record.workcell_binding else {
            return Ok(());
        };
        let database = SessionDatabase::open_state(self.store.state()).map_err(|_| Error::Store)?;
        for session in database
            .list_for_workspace_identity(binding)
            .map_err(|_| Error::Store)?
        {
            let _writer =
                SessionLease::acquire(self.store.state(), session.id).map_err(|_| Error::Busy)?;
            if database
                .load_workflow_runs(session.id)
                .map_err(|_| Error::Store)?
                .iter()
                .any(|run| run.status == WorkflowRunStatus::Active || run.outbox_pending)
            {
                return Err(Error::Busy);
            }
        }
        Ok(())
    }

    pub fn blockers(&self, record: &InstanceRecord) -> Vec<String> {
        let mut blockers = Vec::new();
        if record
            .lifecycle
            .as_ref()
            .is_some_and(LifecycleIntent::is_pending)
        {
            blockers.push("Pending lifecycle postconditions: Reconcile or explicitly acknowledge failure before attaching or another mutation; never replay".into());
        }
        if record
            .create
            .as_ref()
            .and_then(|intent| intent.operation.as_ref())
            .is_some_and(|op| {
                op.cancel_requested
                    && matches!(
                        op.status,
                        OperationStatus::Creating | OperationStatus::CleanupPending
                    )
            })
        {
            blockers.push("Create cancellation accepted; cleanup pending: poll Reconcile, never replay Cancel".into());
        }
        if record.instance.is_some() {
            if let Err(error) = self
                .store
                .lease(record, true)
                .and_then(|_| self.require_quiescent(record))
            {
                blockers.push(error.to_string());
            }
        } else {
            blockers
                .push("Create pending or outcome unknown: Reconcile; never replay Create".into());
        }
        blockers
    }

    pub async fn cancel_create(
        &self,
        name: &SandboxName,
        revision: &Revision,
    ) -> Result<InstanceRecord> {
        let record = self.store.get(name)?;
        let _control = self.store.control_lease(&record)?;
        if &record.revision()? != revision {
            return Err(Error::ReviewChanged);
        }
        let intent = record.create.as_ref().ok_or(Error::Unresolved)?;
        let operation = intent.operation.as_ref().ok_or(Error::Unresolved)?;
        if operation.status != OperationStatus::Creating || operation.cancel_requested {
            return Err(Error::NotReady);
        }
        if record
            .lifecycle
            .as_ref()
            .is_some_and(LifecycleIntent::is_pending)
        {
            return Err(Error::Unresolved);
        }
        let (client, discovery) = self.owned_client(&record).await?;
        if !discovery.capabilities.cancel_create {
            return Err(Error::Protocol);
        }
        let pending = InstanceRecord {
            lifecycle: Some(LifecycleIntent {
                action: "cancel-create".into(),
                expected: Expected {
                    expected_execution_id: operation.execution_id.clone(),
                    expected_revision: record
                        .instance
                        .as_ref()
                        .map_or(0, |instance| instance.revision),
                },
                lease_seconds: None,
                observed_revision: None,
                policy: None,
                policy_revision: None,
                minimum_lease_deadline: None,
                allow_equal_revision: false,
                failure_acknowledged: false,
            }),
            ..record.clone()
        };
        self.store.replace(&record, &pending)?;
        let response = client
            .cancel_create(&intent.key, &operation.execution_id)
            .await;
        let response = self.lifecycle_response(&record, &pending, response)?;
        if !cancellation_accepted(pending.lifecycle.as_ref().ok_or(Error::Store)?, &response) {
            return Err(Error::Identity);
        }
        let acknowledged = self.record_operation(&pending, response)?;
        let mut next = acknowledged.clone();
        next.lifecycle
            .as_mut()
            .ok_or(Error::Store)?
            .observed_revision = Some(
            next.instance
                .as_ref()
                .map_or(0, |instance| instance.revision),
        );
        self.store.replace(&acknowledged, &next)?;
        Ok(next)
    }

    pub async fn prepare_attach(
        &self,
        name: &SandboxName,
        resume: ResumePolicy,
    ) -> Result<AttachTicket> {
        self.prepare_attach_reviewed(name, resume, None, None).await
    }

    pub async fn prepare_attach_at(
        &self,
        name: &SandboxName,
        revision: &Revision,
    ) -> Result<AttachTicket> {
        self.prepare_attach_reviewed(name, ResumePolicy::Refuse, Some(revision), None)
            .await
    }

    async fn prepare_attach_reviewed(
        &self,
        name: &SandboxName,
        resume: ResumePolicy,
        revision: Option<&Revision>,
        saved: Option<&SandboxStore>,
    ) -> Result<AttachTicket> {
        // Pinned against the durable record, the way every other reviewed
        // operation pins. A record's revision digests the whole record,
        // including the instance last observed, so comparing after `inspect`
        // refreshed it would demand that nothing the daemon reports has moved
        // since the review. An egress policy becoming effective after an apply
        // moves exactly that, which turned a healthy reconnect into a refusal.
        let reviewed = self.store.get(name)?;
        if revision.is_some_and(|expected| reviewed.revision().ok().as_ref() != Some(expected)) {
            return Err(Error::ReviewChanged);
        }
        let mut record = self.inspect(name).await?;
        // What the pin still has to guarantee about the live sandbox: it is the
        // execution that was reviewed, refused before asking for credentials
        // rather than after.
        if revision.is_some() && !same_execution(&reviewed, &record) {
            return Err(Error::Identity);
        }
        if record
            .lifecycle
            .as_ref()
            .is_some_and(LifecycleIntent::is_pending)
        {
            return Err(Error::Unresolved);
        }
        if record
            .create
            .as_ref()
            .and_then(|intent| intent.operation.as_ref())
            .is_some_and(|op| op.cancel_requested)
        {
            return Err(Error::NotReady);
        }
        if record
            .instance
            .as_ref()
            .is_some_and(|instance| instance.state == InstanceState::Paused)
        {
            if resume != ResumePolicy::Confirmed {
                return Err(Error::ResumeRequired);
            }
            let lease_seconds = record
                .launch
                .as_ref()
                .ok_or(Error::ResumeRequired)?
                .configuration()
                .profile
                .value()
                .running_ttl_seconds;
            record = self
                .action(name, LifecycleAction::Resume { lease_seconds })
                .await?;
        }
        if record.ownership == Ownership::Owned && record.launch.is_some() {
            let global;
            let saved = if let Some(saved) = saved {
                saved
            } else {
                global = SandboxStore::user_global()?;
                &global
            };
            let (status, reconciled) = self.reconcile_network(name, saved, true, None).await?;
            if status == NetworkReconcileStatus::Deferred {
                return Err(Error::ResumeRequired);
            }
            record = reconciled;
        }
        let lease = self.store.lease(&record, false)?;
        if self.store.get(name)?.revision()? != record.revision()? {
            return Err(Error::ReviewChanged);
        }
        let (client, _) = self.owned_client(&record).await?;
        let instance = record.instance.as_ref().ok_or(Error::NotReady)?;
        if instance.state != InstanceState::Running {
            return Err(Error::NotReady);
        }
        let identity = instance.expected_workcell.as_ref().ok_or(Error::NotReady)?;
        let credentials = client.credentials(instance).await?;
        let selection = RemoteWorkcellSelection {
            source: WorkcellSourceRef::Direct,
            endpoint: WorkcellEndpoint::parse(&format!(
                "{}{}",
                record.provider.proxy_endpoint.as_str(),
                credentials.mcp_path
            ))
            .map_err(|_| Error::Protocol)?,
            cwd: record.cwd.clone(),
            credential_ref: None,
            expected_server_id: Some(
                ExpectedWorkcellId::new(identity.server_id.clone()).map_err(|_| Error::Identity)?,
            ),
            expected_workspace_id: Some(
                ExpectedWorkcellId::new(identity.workspace_id.clone())
                    .map_err(|_| Error::Identity)?,
            ),
        };
        Ok(AttachTicket {
            selection,
            record,
            token: credentials.take_token(),
            lease,
        })
    }

    pub fn confirm_attachment(
        &self,
        ticket: AttachTicket,
        binding: &StoredWorkspaceBinding,
    ) -> Result<(StoredWorkspaceBinding, RuntimeLease)> {
        let record = ticket.record;
        let expected = record
            .instance
            .as_ref()
            .and_then(|instance| instance.expected_workcell.as_ref())
            .ok_or(Error::NotReady)?;
        if !expected.matches(binding)
            || binding.trust_anchor().as_str() != record.provider.proxy_endpoint.as_str()
            || record
                .workcell_binding
                .as_ref()
                .is_some_and(|previous| !previous.same_workspace_identity(binding))
        {
            return Err(Error::Identity);
        }
        let binding = binding
            .clone()
            .with_sandbox_record(record.id)
            .map_err(|_| Error::Identity)?;
        let next = InstanceRecord {
            workcell_binding: Some(binding.clone()),
            detached: false,
            ..record.clone()
        };
        self.store.replace(&record, &next)?;
        Ok((binding, ticket.lease))
    }
}

fn nonzero(value: u32) -> Result<NonZeroU32> {
    NonZeroU32::new(value).ok_or(Error::Protocol)
}

fn range(max: u32) -> Result<ResourceRange> {
    Ok(ResourceRange {
        min: NonZeroU32::MIN,
        max: nonzero(max)?,
        step: NonZeroU32::MIN,
    })
}

fn network_mode(topology: &str) -> Result<Enforcement> {
    match topology {
        "slirp-enforced" => Ok(Enforcement::Required),
        "slirp-unrestricted" | "passt-unrestricted" | "managed-unrestricted" => {
            Ok(Enforcement::Off)
        }
        _ => Err(Error::Protocol),
    }
}

fn capabilities(provider: &SandboxProvider, discovery: &Discovery) -> Result<ProviderCapabilities> {
    Ok(ProviderCapabilities {
        provider_revision: provider.revision()?,
        architecture: Architecture::X86_64,
        cpus: range(discovery.limits.resources.cpu_count)?,
        memory_mib: range(discovery.limits.resources.memory_mb)?,
        disk_gib: range(discovery.limits.resources.disk_size_mb / MIB_PER_GIB)?,
        disk_growth: true,
        persistent: discovery.capabilities.persistent_disk,
        max_ttl_seconds: discovery.limits.max_lease_seconds,
        network_modes: vec![network_mode(&discovery.network_topology)?],
        tls_modes: discovery.tls_modes.clone(),
    })
}

fn check_lease(lease: LeaseSeconds, discovery: &Discovery) -> Result<()> {
    let max = discovery.limits.max_lease_seconds;
    if lease.within(max) {
        Ok(())
    } else {
        Err(Error::LeaseOverCap {
            requested: lease,
            max,
        })
    }
}

fn validate_template(template: &Template) -> Result<()> {
    let manifest = &template.manifest;
    manifest.validate_metadata()?;
    if template.warm_start
        || template.image.format != "qcow2"
        || template.image.backing_policy != "standalone"
        || template.image.file_size_bytes == 0
        || template.image.file_size_bytes > MAX_IMAGE_BYTES
        || template.image.virtual_size_bytes == 0
        || template.image.virtual_size_bytes > u64::from(manifest.minimum.disk_size_mb) * MIB_BYTES
        || manifest.id.as_str().bytes().any(|b| b.is_ascii_uppercase())
    {
        return Err(Error::Protocol);
    }
    network_mode(&manifest.network_topology)?;
    Ok(())
}

pub fn template_entry(template: &Template) -> Result<TemplateEntry> {
    validate_template(template)?;
    let manifest = &template.manifest;
    Ok(TemplateEntry {
        id: manifest.id.clone(),
        revision: template.revision.clone(),
        architecture: Architecture::X86_64,
        minimum_resources: ProfileResources {
            cpus: nonzero(manifest.minimum.cpu_count)?,
            memory_mib: nonzero(manifest.minimum.memory_mb)?,
            disk_gib: nonzero(manifest.minimum.disk_size_mb.div_ceil(MIB_PER_GIB))?,
        },
        workspace_root: manifest.workcell.workspace_root.clone(),
        snapshot_root: manifest.workcell.snapshot_root.clone(),
        workcell_compatible: manifest.workcell.protocol_version == PROTOCOL_VERSION
            && manifest.workcell.transfer_protocol == TRANSFER_PROTOCOL
            && manifest.workcell.remote_workspace
            && manifest.workcell.workspace_snapshots
            && manifest.workcell.reviewed_transfer,
        network_modes: vec![network_mode(&manifest.network_topology)?],
        guest_ca: manifest.guest_ca,
    })
}

fn create_request(launch: &ResolvedLaunch) -> Result<Create> {
    let configuration = launch.configuration();
    let profile = configuration.profile.value();
    let network = configuration.network.value();
    Ok(Create {
        template_id: profile.template.clone(),
        expected_template_revision: configuration.template.revision.clone(),
        resources: Resources {
            cpu_count: profile.cpus.get(),
            memory_mb: profile.memory_mib.get(),
            disk_size_mb: profile
                .disk_gib
                .get()
                .checked_mul(MIB_PER_GIB)
                .ok_or(Error::Protocol)?,
        },
        lease_seconds: profile.running_ttl_seconds,
        persistent: profile.persistent,
        egress: (network.enforcement == Enforcement::Required).then(|| Policy {
            mode: match network.tls_mode {
                TlsMode::SniOnly => "sni-only",
                TlsMode::Mitm => "mitm",
            }
            .into(),
            domains: network
                .domains
                .iter()
                .map(|domain| domain.as_str().to_owned())
                .collect(),
            cidrs: network
                .cidrs
                .iter()
                .map(|cidr| cidr.as_str().to_owned())
                .collect(),
        }),
    })
}

fn cancellation_accepted(intent: &LifecycleIntent, operation: &Operation) -> bool {
    intent.action == "cancel-create"
        && operation.cancel_requested
        && operation.execution_id == intent.expected.expected_execution_id
        && operation.status != OperationStatus::Succeeded
        && operation.instance.as_ref().is_none_or(|instance| {
            instance.execution_id == intent.expected.expected_execution_id
                && instance.revision >= intent.expected.expected_revision
        })
}

fn lifecycle_satisfied(record: &InstanceRecord) -> bool {
    let Some(intent) = &record.lifecycle else {
        return false;
    };
    if intent.action == "cancel-create" {
        record
            .create
            .as_ref()
            .and_then(|create| create.operation.as_ref())
            .is_some_and(|operation| cancellation_accepted(intent, operation))
    } else {
        record
            .instance
            .as_ref()
            .is_some_and(|instance| validate_lifecycle_result(intent, instance).is_ok())
    }
}

fn validate_lifecycle_result(intent: &LifecycleIntent, instance: &Instance) -> Result<()> {
    let same_execution = instance.execution_id == intent.expected.expected_execution_id;
    if instance.revision < intent.expected.expected_revision
        || (instance.revision == intent.expected.expected_revision && !intent.allow_equal_revision)
        || (if intent.action == "resume" {
            same_execution
        } else {
            !same_execution
        })
    {
        return Err(Error::Identity);
    }
    let satisfied = match intent.action.as_str() {
        "pause" => instance.state == InstanceState::Paused && instance.lease_deadline.is_none(),
        "delete" => instance.state == InstanceState::Deleted && instance.lease_deadline.is_none(),
        "resume" | "renew" => {
            let lease = intent.lease_seconds.ok_or(Error::Unresolved)?;
            match (lease.finite(), instance.lease_deadline.as_deref()) {
                (None, None) => {}
                (Some(_), Some(deadline)) => {
                    let minimum = intent
                        .minimum_lease_deadline
                        .as_deref()
                        .ok_or(Error::Unresolved)?;
                    if timestamp(deadline)? < timestamp(minimum)? {
                        return Err(Error::Lease);
                    }
                }
                _ => return Err(Error::Lease),
            }
            instance.state == InstanceState::Running
        }
        "policy" => {
            let policy = intent.policy.as_ref().ok_or(Error::Unresolved)?;
            let revision = intent.policy_revision.as_ref().ok_or(Error::Unresolved)?;
            instance.state == InstanceState::Running
                && instance.egress.enforced
                && instance
                    .egress
                    .policy
                    .as_ref()
                    .map(Policy::canonical)
                    .transpose()?
                    .as_ref()
                    == Some(policy)
                && &policy.revision()? == revision
                && &instance.egress.revision == revision
                && instance.egress.effective_revision.as_ref() == Some(revision)
        }
        _ => false,
    };
    if !satisfied {
        return Err(Error::Identity);
    }
    Ok(())
}

fn same_execution(reviewed: &InstanceRecord, observed: &InstanceRecord) -> bool {
    match (&reviewed.instance, &observed.instance) {
        (Some(reviewed), Some(observed)) => reviewed.execution_id == observed.execution_id,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

fn validate_instance(record: &InstanceRecord, instance: &Instance) -> Result<()> {
    instance.validate(&record.owner_id)?;
    if instance.template.id != record.template.manifest.id
        || instance.template.revision != record.template.revision.as_str()
        || instance.template.image_identity != record.template.image_sha256.as_str()
        || instance.network_topology != record.template.manifest.network_topology
    {
        return Err(Error::Identity);
    }
    if let Some(previous) = &record.instance
        && (instance.sandbox_id != previous.sandbox_id
            || instance.revision < previous.revision
            || instance.resources != previous.resources
            || instance.persistent != previous.persistent
            || (!previous.workspace_generation.is_empty()
                && instance.workspace_generation != previous.workspace_generation))
    {
        return Err(Error::Identity);
    }
    if let Some(intent) = &record.create
        && (instance.resources != intent.request.resources
            || instance.persistent != intent.request.persistent
            || (record.instance.is_none()
                && intent
                    .request
                    .egress
                    .as_ref()
                    .is_some_and(|policy| instance.egress.policy.as_ref() != Some(policy))))
    {
        return Err(Error::Identity);
    }
    if let Some(launch) = &record.launch
        && instance.egress.enforced
            != (launch.configuration().network.value().enforcement == Enforcement::Required)
    {
        return Err(Error::Identity);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Controller, LifecycleAction, NetworkReconcileStatus, RestartPhase, ResumePolicy,
        template_entry, validate_instance,
    };
    use crate::{
        Error, InstanceRecord, LifecycleClient, Ownership,
        dto::{Instance, InstanceState, OperationStatus, Policy, Template},
        generate_api_key,
    };
    use caudra_config::sandbox::{
        LeaseSeconds, ProviderKind, SandboxDraft, SandboxName, SandboxOrigin, SandboxProvider,
        persistence::SandboxStore,
    };
    use caudra_storage::{
        StateDir,
        private_file::PrivateFileError,
        remote_operation_journal::{
            RemoteOperationJournal, RemoteOperationReservation, RequestDigest,
        },
        sandbox_auth::{SandboxCredentialRef, save_sandbox_api_key},
        workspace_binding::StoredWorkspaceBinding,
    };
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, OperationId, ProjectIdentity,
        ProjectKey, SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor, WorkspacePath,
    };
    use futures_lite::future;
    use serde_json::{Value, json};
    use std::os::unix::fs::PermissionsExt;
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        net::{SocketAddr, TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::Duration,
    };
    use tempfile::TempDir;
    use test_case::test_case;

    const OWNER: &str = "owner-test";
    const INSTANCE: &str = "instance-test";
    const EXECUTION: &str = "execution-test";
    const GENERATION: &str = "generation-test";
    const MODEL: &str = "test/model";
    const NOW: &str = "2026-09-20T12:00:00Z";
    const LATER: &str = "2026-09-20T13:00:00Z";
    const EARLIER: &str = "2026-09-20T11:00:00Z";
    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEAD_REVISION: &str =
        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const HEAD_ROUTE: &str = "/daemon/v1/templates/base";
    const REQUEST_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NAME: &str = "saved";
    const PRIVATE_MODE: u32 = 0o700;
    const DOMAIN: &str = "example.test";
    const CHANGED_EXECUTION: &str = "execution-resumed";
    const SETTLED_REVISION: u64 = 2;
    const ENFORCED_TOPOLOGY: &str = "slirp-enforced";
    const SHORT_LEASE: &str = "2026-09-20T12:01:00Z";
    const DENY_REVISION: &str = "fb44c0f5ba3bfc047e139e764fc03a852d08600a8777ea16ef62c2892c9503d9";
    const ALLOW_REVISION: &str = "9702568256299b79c8a8a32db948ac6bba9a0e476cdadbbc7168f6c84394a017";
    const INSTANCE_STORE: &str = "sandboxes/instances.json";
    const LEASE: LeaseSeconds = LeaseSeconds::new(300);
    const LEASE_CAP: LeaseSeconds = LeaseSeconds::new(3600);

    #[test]
    fn interrupted_restart_after_pause_requires_explicit_resume() {
        let (notify, notified) = smol::channel::bounded(1);
        let (release, released) = smol::channel::bounded(1);
        let mut paused = false;
        let server = Server::new(move |method, path, _, headers| {
            assert!(!path.ends_with("/resume"));
            if path.ends_with("/discover") && paused {
                notify.try_send(()).unwrap();
                smol::block_on(released.recv()).unwrap();
                return None;
            }
            if path.ends_with("/pause") {
                paused = true;
                let mut response = instance();
                response["state"] = json!("paused");
                response["leaseDeadline"] = Value::Null;
                response["revision"] = json!(2);
                return Some((200, response));
            }
            regular(method, path, headers)
        });
        let (_temp, controller, saved) = setup(&server);
        let before = create_ready(&controller, &saved);
        let result = smol::block_on(future::or(
            async {
                notified.recv().await.unwrap();
                None
            },
            async {
                Some(
                    controller
                        .restart_at(&name(), &before.revision().unwrap(), LEASE)
                        .await,
                )
            },
        ));
        release.try_send(()).unwrap();
        assert!(result.is_none());
        let recovered = Controller::new(controller.store.state()).unwrap();
        let after = recovered.store.get(&name()).unwrap();
        assert_eq!(
            after.instance.as_ref().unwrap().state,
            InstanceState::Paused
        );
        let intent = after.lifecycle.as_ref().unwrap();
        assert_eq!(intent.action, "pause");
        assert_eq!(intent.observed_revision, Some(2));
        assert!(!intent.failure_acknowledged);
        assert!(matches!(
            smol::block_on(recovered.restart_at(&name(), &after.revision().unwrap(), LEASE)),
            Err(super::RestartFailure {
                source: Error::NotReady,
                ..
            })
        ));
        assert!(recovered.store.lease(&after, true).is_ok());
    }

    #[test_case("success")]
    #[test_case("pause_lost")]
    #[test_case("resume_lost")]
    #[test_case("pause_refused")]
    #[test_case("resume_refused")]
    #[test_case("pause_unknown")]
    #[test_case("pause_invalid")]
    #[test_case("resume_disk_changed")]
    #[test_case("resume_same_execution")]
    #[test_case("refusal_concurrent_change")]
    #[test_case("refusal_concurrent_clear")]
    fn restart_phases_are_conditional_durable_and_never_replayed(scenario: &'static str) {
        let state = Arc::new(Mutex::new(None::<StateDir>));
        let server_state = state.clone();
        let sends = Arc::new(Mutex::new(Vec::new()));
        let server_sends = sends.clone();
        let server = Server::new(move |method, path, body, headers| {
            let pause = path.ends_with("/pause");
            if !pause && !path.ends_with("/resume") {
                return regular(method, path, headers);
            }
            let action = if pause { "pause" } else { "resume" };
            server_sends.lock().unwrap().push(action);
            let controller =
                Controller::new(server_state.lock().unwrap().as_ref().unwrap()).unwrap();
            let pending = controller.store.get(&name()).unwrap();
            let intent = pending.lifecycle.as_ref().unwrap();
            assert!(intent.is_pending());
            assert!(!intent.failure_acknowledged);
            assert_eq!(intent.action, action);
            assert_eq!(body["expectedExecutionID"], EXECUTION);
            assert_eq!(body["expectedRevision"], if pause { 1 } else { 2 });
            assert_eq!(intent.expected.expected_revision, if pause { 1 } else { 2 });
            assert!(matches!(
                controller.store.lease(&pending, false),
                Err(Error::Busy)
            ));
            if scenario.starts_with("refusal_concurrent_") {
                let mut changed = pending.clone();
                changed.detached = true;
                changed.instance.as_mut().unwrap().revision = 2;
                if scenario == "refusal_concurrent_clear" {
                    changed.lifecycle = None;
                }
                controller.store.replace(&pending, &changed).unwrap();
            }
            if (pause
                && matches!(
                    scenario,
                    "pause_refused"
                        | "pause_unknown"
                        | "refusal_concurrent_change"
                        | "refusal_concurrent_clear"
                ))
                || (!pause && scenario == "resume_refused")
            {
                return Some((
                    409,
                    json!({"error":{"code":"template_incompatible","retryable":false,"outcomeUnknown":scenario == "pause_unknown"}}),
                ));
            }
            if (pause && scenario == "pause_lost") || (!pause && scenario == "resume_lost") {
                return None;
            }
            let mut response = instance();
            response["revision"] = json!(if pause { 2 } else { 3 });
            if pause {
                response["state"] = json!("paused");
                response["leaseDeadline"] = Value::Null;
                if scenario == "pause_invalid" {
                    response["revision"] = json!(1);
                }
            } else {
                response["executionID"] = json!(CHANGED_EXECUTION);
                if scenario == "resume_disk_changed" {
                    response["workspaceGeneration"] = json!("different-disk");
                }
                if scenario == "resume_same_execution" {
                    response["executionID"] = json!(EXECUTION);
                }
            }
            Some((200, response))
        });
        let (_temp, controller, saved) = setup(&server);
        *state.lock().unwrap() = Some(controller.store.state().clone());
        let before = create_ready(&controller, &saved);
        let result =
            smol::block_on(controller.restart_at(&name(), &before.revision().unwrap(), LEASE));
        let pause_failed =
            scenario.starts_with("pause_") || scenario.starts_with("refusal_concurrent_");
        assert_eq!(
            *sends.lock().unwrap(),
            if pause_failed {
                vec!["pause"]
            } else {
                vec!["pause", "resume"]
            }
        );
        if scenario == "success" {
            assert!(result.is_ok());
        } else {
            let failure = result.unwrap_err();
            assert_eq!(
                failure.phase,
                if pause_failed {
                    RestartPhase::Pause
                } else {
                    RestartPhase::Resume
                }
            );
            if scenario.ends_with("refused") {
                assert!(matches!(
                    failure.source,
                    Error::Daemon {
                        status: 409,
                        code: crate::dto::FailureCode::TemplateIncompatible,
                        outcome_unknown: false,
                        ..
                    }
                ));
            }
            if scenario.starts_with("refusal_concurrent_") {
                let message = failure.source.to_string();
                let Error::LifecycleRefusalCleanup { refusal, cleanup } = failure.source else {
                    panic!("expected definitive refusal with local cleanup failure");
                };
                assert!(matches!(
                    *refusal,
                    Error::Daemon {
                        status: 409,
                        code: crate::dto::FailureCode::TemplateIncompatible,
                        outcome_unknown: false,
                        ..
                    }
                ));
                assert!(matches!(
                    *cleanup,
                    Error::PrivateFile(PrivateFileError::Conflict)
                ));
                assert!(message.contains(&refusal.to_string()));
                assert!(message.contains(&cleanup.to_string()));
            }
        }
        let recovered = Controller::new(controller.store.state()).unwrap();
        let after = recovered.store.get(&name()).unwrap();
        assert_eq!(after.launch, before.launch);
        let observed = after.instance.as_ref().unwrap();
        let original = before.instance.as_ref().unwrap();
        assert_eq!(observed.sandbox_id, original.sandbox_id);
        assert_eq!(observed.template, original.template);
        assert_eq!(observed.resources, original.resources);
        assert_eq!(observed.workspace_generation, original.workspace_generation);
        assert!(observed.persistent);
        if scenario.starts_with("refusal_concurrent_") {
            assert!(after.detached);
            assert_eq!(observed.revision, 2);
        }
        if scenario == "success" {
            assert_eq!(observed.execution_id, CHANGED_EXECUTION);
            assert_eq!(observed.revision, 3);
        } else if scenario == "pause_refused" {
            assert_eq!(after.revision().unwrap(), before.revision().unwrap());
            assert!(
                smol::block_on(recovered.action_at(
                    &name(),
                    &after.revision().unwrap(),
                    LifecycleAction::Pause
                ))
                .is_err()
            );
            assert_eq!(sends.lock().unwrap().len(), 2);
        } else if scenario == "resume_refused" {
            assert_eq!(observed.state, InstanceState::Paused);
            assert_eq!(after.lifecycle.as_ref().unwrap().action, "pause");
            assert!(!after.lifecycle.as_ref().unwrap().is_pending());
            assert!(
                smol::block_on(recovered.action_at(
                    &name(),
                    &after.revision().unwrap(),
                    LifecycleAction::Resume {
                        lease_seconds: LEASE,
                    }
                ))
                .is_err()
            );
            assert_eq!(sends.lock().unwrap().len(), 3);
        } else if scenario == "refusal_concurrent_clear" {
            assert!(after.lifecycle.is_none());
            assert_eq!(sends.lock().unwrap().len(), 1);
        } else {
            assert!(after.lifecycle.as_ref().unwrap().is_pending());
            assert!(!after.lifecycle.as_ref().unwrap().failure_acknowledged);
            assert!(matches!(
                smol::block_on(recovered.action_at(
                    &name(),
                    &after.revision().unwrap(),
                    LifecycleAction::Resume {
                        lease_seconds: LEASE,
                    }
                )),
                Err(Error::Unresolved)
            ));
            assert_eq!(
                sends.lock().unwrap().len(),
                if pause_failed { 1 } else { 2 }
            );
        }
    }

    #[test_case("stale")]
    #[test_case("ephemeral")]
    #[test_case("borrowed")]
    #[test_case("paused")]
    #[test_case("runtime")]
    #[test_case("no_expiry_over_cap")]
    #[test_case("finite_over_cap")]
    fn restart_preflight_never_dispatches(scenario: &str) {
        let server = Server::new(|method, path, _, headers| {
            assert!(!path.ends_with("/pause") && !path.ends_with("/resume"));
            regular(method, path, headers)
        });
        let (_temp, controller, saved) = setup(&server);
        let original = create_ready(&controller, &saved);
        let mut record = original.clone();
        match scenario {
            "stale" => record.detached = true,
            "ephemeral" => record.instance.as_mut().unwrap().persistent = false,
            "borrowed" => record.ownership = Ownership::Borrowed,
            "paused" => record.instance.as_mut().unwrap().state = InstanceState::Paused,
            _ => {}
        }
        controller.store.replace(&original, &record).unwrap();
        let _lease =
            (scenario == "runtime").then(|| controller.store.lease(&record, false).unwrap());
        let revision = if scenario == "stale" {
            original.revision()
        } else {
            record.revision()
        }
        .unwrap();
        let lease = match scenario {
            "no_expiry_over_cap" => LeaseSeconds::NO_EXPIRY,
            "finite_over_cap" => LeaseSeconds::new(LEASE_CAP.get() + 1),
            _ => LEASE,
        };
        let error = smol::block_on(controller.restart_at(&name(), &revision, lease)).unwrap_err();
        assert_eq!(error.phase, RestartPhase::Pause);
        assert!(match scenario {
            "stale" => matches!(error.source, Error::ReviewChanged),
            "ephemeral" => matches!(error.source, Error::PauseUnsupported),
            "borrowed" => matches!(error.source, Error::Borrowed),
            "paused" => matches!(error.source, Error::NotReady),
            "runtime" => matches!(error.source, Error::Busy),
            "no_expiry_over_cap" | "finite_over_cap" => matches!(
                error.source,
                Error::LeaseOverCap { requested, max } if requested == lease && max == LEASE_CAP
            ),
            _ => false,
        });
        assert_eq!(
            controller.store.get(&name()).unwrap().revision().unwrap(),
            record.revision().unwrap()
        );
    }

    #[test_case("sni-only", false, true, true; "sni_without_ca")]
    #[test_case("mitm", true, true, true; "eligible_mitm")]
    #[test_case("mitm", false, true, false; "mitm_without_guest_ca")]
    #[test_case("mitm", true, false, false; "mitm_not_discovered")]
    fn reviewed_create_propagates_tls_mode_and_fails_closed(
        mode: &str,
        guest_ca: bool,
        offered: bool,
        accepted: bool,
    ) {
        let mode = mode.to_owned();
        let expected_mode = mode.clone();
        let sends = Arc::new(Mutex::new(0));
        let server_sends = sends.clone();
        let server = Server::new(move |method, path, body, headers| {
            if let Some((status, mut value)) = enforced_metadata(path) {
                if path.ends_with("/discover") {
                    value["tlsModes"] = if offered {
                        json!(["sni-only", "mitm"])
                    } else {
                        json!(["sni-only"])
                    };
                } else {
                    value["guestCA"] = json!(guest_ca);
                    value["workcell"]["workspaceRoot"] = json!("/workspace");
                    value["workcell"]["snapshotRoot"] = json!("/snapshots");
                    value["workcell"]["transferRoot"] = json!("/transfers");
                }
                return Some((status, value));
            }
            assert_eq!(method, "POST");
            *server_sends.lock().unwrap() += 1;
            assert_eq!(body["egress"]["mode"], expected_mode);
            let key = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("idempotency-key: ")
                        .map(str::to_owned)
                })
                .unwrap();
            let mut instance = enforced_instance(false);
            instance["egress"]["policy"] = body["egress"].clone();
            Some((200, operation(&key, instance)))
        });
        let (temp, controller, saved) = setup(&server);
        let mut draft = saved.draft();
        let network = draft
            .networks
            .get_mut(&SandboxName::parse("net").unwrap())
            .unwrap();
        network.enforcement = caudra_config::sandbox::Enforcement::Required;
        network.tls_mode = serde_json::from_value(json!(mode)).unwrap();
        let store = SandboxStore::from_config_dir(&temp.path().join("tls-config")).unwrap();
        let saved = store.save(&store.load().unwrap(), &draft).unwrap();
        let result = smol::block_on(controller.create(
            saved.saved(),
            &SandboxName::parse("dev").unwrap(),
            name(),
        ));
        assert_eq!(result.is_ok(), accepted);
        assert_eq!(*sends.lock().unwrap(), usize::from(accepted));
        if accepted {
            let record = result.unwrap();
            assert_eq!(record.create.unwrap().request.egress.unwrap().mode, mode);
            let template = &record.launch.unwrap().configuration().template.clone();
            assert_eq!(template.guest_ca, guest_ca);
            assert_eq!(template.workspace_root, "/workspace");
        } else {
            assert!(matches!(controller.store.get(&name()), Err(Error::Missing)));
        }
    }

    #[test_case("mitm", "mitm", true, false, false, true; "same_mode_mitm_rules")]
    #[test_case("mitm", "mitm", false, false, false, false; "same_mode_missing_ca")]
    #[test_case("sni-only", "mitm", true, false, true, false; "transition_not_advertised")]
    #[test_case("sni-only", "mitm", true, true, false, false; "transition_ca_not_ready")]
    #[test_case("sni-only", "mitm", true, true, true, true; "reviewed_ready_transition")]
    #[test_case("mitm", "sni-only", true, false, true, false; "downgrade_also_requires_capability")]
    fn live_tls_policy_requires_reviewed_capability_and_ca_state(
        previous: &str,
        next: &str,
        guest_ca: bool,
        transition: bool,
        ready: bool,
        accepted: bool,
    ) {
        let mut discovered = enforced_metadata("/daemon/v1/discover").unwrap().1;
        discovered["capabilities"]["liveTlsModeChange"] = json!(transition);
        let discovery = serde_json::from_value(discovered).unwrap();
        let mut template: Template =
            serde_json::from_value(enforced_metadata("/daemon/v1/templates/base").unwrap().1)
                .unwrap();
        template.manifest.guest_ca = guest_ca;
        let mut instance: Instance = serde_json::from_value(enforced_instance(false)).unwrap();
        instance.egress.policy.as_mut().unwrap().mode = previous.into();
        instance.egress.guest_ca_ready = ready;
        let policy = Policy {
            mode: next.into(),
            domains: Vec::new(),
            cidrs: Vec::new(),
        };
        assert_eq!(
            policy
                .validate_for(&discovery, &template.manifest, Some(&instance))
                .is_ok(),
            accepted
        );
        if previous != next {
            instance.egress.effective_revision = None;
            assert!(
                policy
                    .validate_for(&discovery, &template.manifest, Some(&instance))
                    .is_err()
            );
        }
        assert!(
            serde_json::from_value::<Policy>(
                json!({"mode":next,"domains":[],"cidrs":[],"ports":[443]})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<Policy>(
                json!({"mode":next,"domains":[],"cidrs":[],"methods":["GET"]})
            )
            .is_err()
        );
    }

    fn policy(allow: bool) -> Policy {
        Policy {
            mode: "sni-only".into(),
            domains: if allow {
                vec![DOMAIN.into()]
            } else {
                Vec::new()
            },
            cidrs: Vec::new(),
        }
    }

    #[test_case("apply")]
    #[test_case("startup")]
    #[test_case("resume")]
    #[test_case("startup_tls")]
    #[test_case("startup_lost")]
    #[test_case("same")]
    #[test_case("paused")]
    #[test_case("borrowed")]
    #[test_case("unmanaged")]
    #[test_case("missing")]
    #[test_case("detached")]
    #[test_case("tls")]
    #[test_case("enforcement")]
    #[test_case("lost")]
    #[test_case("drift_before")]
    #[test_case("drift_after")]
    #[test_case("stale_commit")]
    fn saved_network_reconciliation_is_narrow_and_conditional(scenario: &str) {
        let remote = Arc::new(Mutex::new(enforced_instance(false)));
        let server_remote = remote.clone();
        let sends = Arc::new(Mutex::new(Vec::new()));
        let server_sends = sends.clone();
        let lost = matches!(scenario, "lost" | "startup_lost");
        let drift_before = scenario == "drift_before";
        let drift_after = scenario == "drift_after";
        let config_slot = Arc::new(Mutex::new(None::<SandboxStore>));
        let server_config = config_slot.clone();
        let mut discoveries = 0;
        let server = Server::new(move |method, path, body, headers| {
            if path.ends_with("/discover") {
                discoveries += 1;
            }
            if (drift_before && path.ends_with("/discover") && discoveries == 3)
                || (drift_after && path.ends_with("/policy"))
            {
                let config = server_config.lock().unwrap();
                let config = config.as_ref().unwrap();
                let loaded = config.load().unwrap();
                let mut draft = loaded.draft();
                draft
                    .networks
                    .get_mut(&SandboxName::parse("net").unwrap())
                    .unwrap()
                    .domains
                    .clear();
                config.save(&loaded, &draft).unwrap();
            }
            if let Some(metadata) = enforced_metadata(path) {
                return Some(metadata);
            }
            let mut current = server_remote.lock().unwrap();
            if path.ends_with("/policy") {
                server_sends.lock().unwrap().push("policy");
                assert_eq!(method, "PUT");
                assert_eq!(body["expectedRevision"], current["revision"]);
                if lost {
                    return None;
                }
                let revision = current["revision"].as_u64().unwrap() + 1;
                current["egress"] = enforced_instance(true)["egress"].clone();
                current["revision"] = json!(revision);
                return Some((200, current.clone()));
            }
            if path.ends_with("/resume") {
                server_sends.lock().unwrap().push("resume");
                *current = enforced_instance(false);
                current["revision"] = json!(3);
                current["executionID"] = json!(CHANGED_EXECUTION);
                return Some((200, current.clone()));
            }
            if path.ends_with("/credentials") {
                server_sends.lock().unwrap().push("credentials");
                return Some((
                    200,
                    json!({"instance":current.clone(),"trafficAccessToken":"b".repeat(64),"mcpPath":format!("/sandboxes/{INSTANCE}/mcp"),"filesPath":"/files","credentialScope":"sandbox_lifetime"}),
                ));
            }
            if method == "POST" {
                let key = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("idempotency-key: ")
                            .map(str::to_owned)
                    })
                    .unwrap();
                return Some((200, operation(&key, current.clone())));
            }
            if path.contains("/operations/") {
                return Some((
                    200,
                    operation(path.rsplit('/').next().unwrap(), current.clone()),
                ));
            }
            Some((200, current.clone()))
        });
        let (temp, controller, saved) = setup(&server);
        let config = SandboxStore::from_config_dir(&temp.path().join("config")).unwrap();
        *config_slot.lock().unwrap() =
            Some(SandboxStore::from_config_dir(&temp.path().join("config")).unwrap());
        let mut draft = saved.draft();
        let net = SandboxName::parse("net").unwrap();
        draft.networks.get_mut(&net).unwrap().enforcement =
            caudra_config::sandbox::Enforcement::Required;
        let saved = config.save(&config.load().unwrap(), &draft).unwrap();
        let original = create_ready(&controller, saved.saved());
        let mut record = original.clone();
        if scenario == "borrowed" {
            record.ownership = Ownership::Borrowed;
        }
        if scenario == "unmanaged" {
            record.launch = None;
        }
        if scenario == "detached" || scenario == "startup" {
            record.detached = true;
        }
        if matches!(scenario, "paused" | "resume") {
            let mut current = remote.lock().unwrap();
            current["state"] = json!("paused");
            current["leaseDeadline"] = Value::Null;
            current["revision"] = json!(2);
        }
        controller.store.replace(&original, &record).unwrap();
        let loaded = config.load().unwrap();
        let mut draft = loaded.draft();
        if scenario != "same" {
            draft.networks.get_mut(&net).unwrap().domains =
                vec![caudra_config::sandbox::DomainRule::parse(DOMAIN).unwrap()];
        }
        if matches!(scenario, "tls" | "startup_tls") {
            draft.networks.get_mut(&net).unwrap().tls_mode = caudra_config::sandbox::TlsMode::Mitm;
        }
        if scenario == "enforcement" {
            draft.networks.get_mut(&net).unwrap().enforcement =
                caudra_config::sandbox::Enforcement::Off;
            draft.networks.get_mut(&net).unwrap().domains.clear();
        }
        let other = SandboxName::parse("other").unwrap();
        draft.networks.insert(
            other.clone(),
            saved.saved().configuration().networks[&net].clone(),
        );
        let profile = draft
            .profiles
            .get_mut(&SandboxName::parse("dev").unwrap())
            .unwrap();
        profile.network = other;
        profile.cpus = super::nonzero(4).unwrap();
        if scenario == "missing" {
            draft.networks.remove(&net);
        }
        draft
            .providers
            .get_mut(&SandboxName::parse("daemon").unwrap())
            .unwrap()
            .api_endpoint = SandboxOrigin::parse("https://changed.test").unwrap();
        let committed = config.save(&loaded, &draft).unwrap();
        if matches!(
            scenario,
            "startup" | "resume" | "startup_tls" | "startup_lost"
        ) {
            let result = smol::block_on(controller.prepare_attach_reviewed(
                &name(),
                ResumePolicy::Confirmed,
                None,
                Some(&config),
            ));
            if scenario == "startup_tls" {
                assert!(matches!(result, Err(Error::NetworkReview)));
                assert!(sends.lock().unwrap().is_empty());
                return;
            }
            if scenario == "startup_lost" {
                assert!(result.is_err());
                assert_eq!(*sends.lock().unwrap(), vec!["policy"]);
                return;
            }
            let ticket = result.unwrap();
            assert_eq!(ticket.record.launch, original.launch);
            let expected = if scenario == "resume" {
                vec!["resume", "policy", "credentials"]
            } else {
                vec!["policy", "credentials"]
            };
            assert_eq!(*sends.lock().unwrap(), expected);
            return;
        }
        let _runtime = controller.store.lease(&record, false).unwrap();
        let expected_revision = if scenario == "stale_commit" {
            saved.saved().revision()
        } else {
            committed.saved().revision()
        };
        let result = smol::block_on(controller.reconcile_saved_network_at(
            &name(),
            &config,
            expected_revision,
        ));
        match scenario {
            "drift_before" | "stale_commit" => {
                assert!(matches!(result, Err(Error::ReviewChanged)));
                assert!(sends.lock().unwrap().is_empty());
                assert!(controller.store.get(&name()).unwrap().lifecycle.is_none());
            }
            "drift_after" => {
                assert!(matches!(result, Err(Error::NetworkChanged)));
                assert_eq!(*sends.lock().unwrap(), vec!["policy"]);
                assert!(
                    !controller
                        .store
                        .get(&name())
                        .unwrap()
                        .lifecycle
                        .unwrap()
                        .is_pending()
                );
            }
            "missing" => assert!(matches!(result, Err(Error::MissingNetwork))),
            "tls" | "enforcement" => assert!(matches!(result, Err(Error::NetworkReview))),
            "lost" => {
                assert!(result.is_err());
                assert!(matches!(
                    smol::block_on(controller.reconcile_saved_network(&name(), &config)),
                    Err(Error::Unresolved)
                ));
                assert_eq!(*sends.lock().unwrap(), vec!["policy"]);
            }
            _ => {
                let (status, next) = result.unwrap();
                let expected = match scenario {
                    "same" => NetworkReconcileStatus::NoChange,
                    "paused" => NetworkReconcileStatus::Deferred,
                    "borrowed" | "detached" | "unmanaged" => NetworkReconcileStatus::Excluded,
                    _ => NetworkReconcileStatus::Applied,
                };
                assert_eq!(status, expected);
                assert_eq!(next.launch, record.launch);
                assert_eq!(next.provider, original.provider);
                assert_eq!(next.cwd, original.cwd);
                assert_eq!(
                    serde_json::to_value(&next.create).unwrap(),
                    serde_json::to_value(&original.create).unwrap()
                );
                if scenario == "apply" {
                    assert_eq!(
                        smol::block_on(controller.reconcile_saved_network(&name(), &config))
                            .unwrap()
                            .0,
                        NetworkReconcileStatus::NoChange
                    );
                    assert_eq!(*sends.lock().unwrap(), vec!["policy"]);
                } else {
                    assert!(sends.lock().unwrap().is_empty());
                }
            }
        }
    }

    fn enforced_instance(allow: bool) -> Value {
        let mut value = instance();
        value["networkTopology"] = json!(ENFORCED_TOPOLOGY);
        let revision = if allow { ALLOW_REVISION } else { DENY_REVISION };
        value["egress"] = json!({"enforced":true,"revision":revision,"effectiveRevision":revision,"policy":policy(allow)});
        value
    }

    fn enforced_metadata(path: &str) -> Option<(u16, Value)> {
        let mut value = if path.ends_with("/discover") {
            let mut value = discovery(OWNER);
            value["capabilities"]["egressPolicy"] = json!(true);
            value["tlsModes"] = json!(["sni-only", "mitm"]);
            value
        } else if path.starts_with("/daemon/v1/templates/") {
            template()
        } else {
            return None;
        };
        value["networkTopology"] = json!(ENFORCED_TOPOLOGY);
        Some((200, value))
    }

    fn borrow_ready(controller: &Controller, server: &Server) -> InstanceRecord {
        smol::block_on(controller.borrow(
            name(),
            SandboxName::parse("daemon").unwrap(),
            provider(server),
            INSTANCE,
            WorkspacePath::new(".").unwrap(),
        ))
        .unwrap()
    }

    #[test_case("before_send", false; "crash_before_policy_send")]
    #[test_case("unapplied", false; "policy_not_applied")]
    #[test_case("applied", true; "policy_applied_response_lost")]
    #[test_case("wrong_policy", false; "unrelated_revision_is_not_success")]
    #[test_case("wrong_revision", false; "requested_policy_revision_must_match")]
    #[test_case("ineffective", false; "requested_policy_must_be_effective")]
    #[test_case("wrong_execution", false; "requested_execution_must_match")]
    #[test_case("unchanged_revision", false; "tightening_requires_revision_change")]
    fn policy_recovery_requires_persisted_postconditions(outcome: &str, resolved: bool) {
        let outcome = outcome.to_owned();
        let before_send = outcome == "before_send";
        let mutations = Arc::new(Mutex::new(0));
        let sends = mutations.clone();
        let mut current = enforced_instance(true);
        let server = Server::new(move |method, path, _, _| {
            if let Some(response) = enforced_metadata(path) {
                return Some(response);
            }
            if method == "PUT" {
                *sends.lock().unwrap() += 1;
                if outcome != "unapplied" {
                    current = enforced_instance(outcome == "wrong_policy");
                    current["revision"] = json!(if outcome == "unchanged_revision" {
                        1
                    } else {
                        2
                    });
                    if outcome == "wrong_revision" {
                        current["egress"]["revision"] = json!(ALLOW_REVISION);
                        current["egress"]["effectiveRevision"] = json!(ALLOW_REVISION);
                    } else if outcome == "ineffective" {
                        current["egress"]["effectiveRevision"] = Value::Null;
                    } else if outcome == "wrong_execution" {
                        current["executionID"] = json!(CHANGED_EXECUTION);
                    }
                }
                return None;
            }
            Some((200, current.clone()))
        });
        let (_temp, controller, _) = setup(&server);
        let record = borrow_ready(&controller, &server);
        if before_send {
            let mut pending = record.clone();
            pending.lifecycle = Some(
                serde_json::from_value(json!({
                    "action":"policy", "expected":record.instance.as_ref().unwrap().expected(),
                    "lease_seconds":null, "observed_revision":null,
                    "policy":policy(false), "policy_revision":DENY_REVISION,
                    "minimum_lease_deadline":null, "allow_equal_revision":false,
                    "failure_acknowledged":false
                }))
                .unwrap(),
            );
            controller.store.replace(&record, &pending).unwrap();
        } else {
            assert!(matches!(
                smol::block_on(controller.action(
                    &name(),
                    LifecycleAction::ApplyPolicy {
                        policy: policy(false)
                    }
                )),
                Err(Error::Transport(_))
            ));
        }
        let recovered = Controller::new(controller.store.state()).unwrap();
        let inspected = smol::block_on(recovered.inspect(&name())).unwrap();
        assert_eq!(
            inspected
                .lifecycle
                .as_ref()
                .unwrap()
                .observed_revision
                .is_some(),
            resolved
        );
        let intent = serde_json::to_value(inspected.lifecycle.as_ref().unwrap()).unwrap();
        assert_eq!(intent["policy"], json!(policy(false)));
        assert_eq!(intent["policy_revision"], DENY_REVISION);
        if !resolved {
            assert!(!recovered.blockers(&inspected).is_empty());
            assert!(matches!(
                smol::block_on(recovered.prepare_attach(&name(), ResumePolicy::Refuse)),
                Err(Error::Unresolved)
            ));
            assert!(matches!(
                smol::block_on(recovered.action(
                    &name(),
                    LifecycleAction::ApplyPolicy {
                        policy: policy(false)
                    }
                )),
                Err(Error::Unresolved)
            ));
        }
        assert_eq!(*mutations.lock().unwrap(), usize::from(!before_send));
    }

    #[test_case(false; "already_paused")]
    #[test_case(true; "nonshortening_renewal_already_satisfied")]
    fn equal_revision_controls_accept_real_no_ops(renew: bool) {
        let server = Server::new(move |method, path, _, headers| {
            let mut current = instance();
            if !renew {
                current["state"] = json!("paused");
                current["leaseDeadline"] = Value::Null;
            }
            if path.ends_with("/pause") || path.ends_with("/renew") {
                return Some((200, current));
            }
            let (status, mut response) = regular(method, path, headers).unwrap();
            if response.get("instance").is_some() {
                response["instance"] = current;
            }
            Some((status, response))
        });
        let (_temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let action = if renew {
            LifecycleAction::Extend {
                lease_seconds: LEASE,
            }
        } else {
            LifecycleAction::Pause
        };
        let result = smol::block_on(controller.action(&name(), action)).unwrap();
        assert_eq!(result.instance, record.instance);
        assert_eq!(result.lifecycle.unwrap().observed_revision, Some(1));
    }

    #[test_case(false; "unchanged_policy")]
    #[test_case(true; "unchanged_canonical_policy")]
    fn equal_revision_policy_accepts_real_no_op(duplicates: bool) {
        let server = Server::new(move |method, path, body, _| {
            if let Some(response) = enforced_metadata(path) {
                return Some(response);
            }
            if method == "PUT" {
                assert_eq!(body["egress"], json!(policy(true)));
            }
            Some((200, enforced_instance(true)))
        });
        let (_temp, controller, _) = setup(&server);
        let record = borrow_ready(&controller, &server);
        let mut requested = policy(true);
        if duplicates {
            requested.domains.push(DOMAIN.into());
        }
        let result = smol::block_on(
            controller.action(&name(), LifecycleAction::ApplyPolicy { policy: requested }),
        )
        .unwrap();
        assert_eq!(result.instance, record.instance);
        assert_eq!(result.lifecycle.unwrap().observed_revision, Some(1));
    }

    #[test_case("pause", "no_change"; "pause_must_change_running_state")]
    #[test_case("pause", "equal_transition"; "pause_transition_requires_revision")]
    #[test_case("resume", "equal_transition"; "resume_requires_revision")]
    #[test_case("resume", "no_change"; "resume_requires_new_execution")]
    #[test_case("delete", "no_change"; "delete_must_delete")]
    #[test_case("delete", "equal_transition"; "delete_requires_revision")]
    #[test_case("renew", "insufficient_lease"; "renewal_no_op_must_satisfy_duration")]
    #[test_case("renew", "wrong_execution"; "renewal_cannot_change_execution")]
    #[test_case("renew", "rollback"; "renewal_cannot_roll_back_revision")]
    fn control_responses_must_satisfy_the_requested_action(route: &str, failure: &str) {
        let route = route.to_owned();
        let action = match route.as_str() {
            "pause" => LifecycleAction::Pause,
            "resume" => LifecycleAction::Resume {
                lease_seconds: LEASE,
            },
            "renew" => LifecycleAction::Extend {
                lease_seconds: LEASE,
            },
            _ => LifecycleAction::Delete {
                destroy_borrowed: false,
            },
        };
        let failure = failure.to_owned();
        let mut mutated = false;
        let server = Server::new(move |method, path, _, headers| {
            let mut current = instance();
            current["revision"] = json!(2);
            if failure == "insufficient_lease" {
                current["leaseDeadline"] = json!(SHORT_LEASE);
            }
            let control = path.ends_with(&format!("/{route}")) || method == "DELETE";
            mutated |= control;
            if mutated {
                if failure == "no_change" {
                    current["revision"] = json!(3);
                } else if failure == "equal_transition" {
                    match route.as_str() {
                        "pause" => {
                            current["state"] = json!("paused");
                            current["leaseDeadline"] = Value::Null;
                        }
                        "resume" => current["executionID"] = json!(CHANGED_EXECUTION),
                        _ => {
                            current["state"] = json!("deleted");
                            current["leaseDeadline"] = Value::Null;
                        }
                    }
                } else if failure == "wrong_execution" {
                    current["executionID"] = json!(CHANGED_EXECUTION);
                } else if failure == "rollback" {
                    current["revision"] = json!(1);
                }
            }
            if control {
                return Some((200, current));
            }
            let (status, mut response) = regular(method, path, headers).unwrap();
            if response.get("instance").is_some() {
                response["instance"] = current;
            }
            Some((status, response))
        });
        let (_temp, controller, saved) = setup(&server);
        create_ready(&controller, &saved);
        assert!(matches!(
            smol::block_on(controller.action(&name(), action)),
            Err(Error::Identity | Error::Lease)
        ));
        let _ = smol::block_on(controller.inspect(&name()));
        assert!(
            controller
                .store
                .get(&name())
                .unwrap()
                .lifecycle
                .unwrap()
                .is_pending()
        );
    }

    #[test_case("policy", "policy", false; "missing_policy")]
    #[test_case("policy", "policy_revision", false; "missing_policy_revision")]
    #[test_case("renew", "minimum_lease_deadline", false; "missing_lease_deadline")]
    #[test_case("policy", "allow_equal_revision", false; "missing_revision_postcondition")]
    #[test_case("policy", "failure_acknowledged", false; "missing_acknowledgement")]
    #[test_case("policy", "policy", true; "null_policy")]
    #[test_case("renew", "minimum_lease_deadline", true; "null_lease_deadline")]
    fn incomplete_lifecycle_intents_are_rejected_without_rewriting(
        action: &str,
        field: &str,
        null: bool,
    ) {
        let server = Server::new(|_, path, _, _| {
            enforced_metadata(path).or_else(|| Some((200, enforced_instance(true))))
        });
        let (_temp, controller, _) = setup(&server);
        let record = borrow_ready(&controller, &server);
        let path = controller
            .store
            .state()
            .persistent_path()
            .join(INSTANCE_STORE);
        let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut intent = json!({
            "action":action,"expected":record.instance.as_ref().unwrap().expected(),
            "lease_seconds":null,"observed_revision":null,"policy":policy(false),
            "policy_revision":DENY_REVISION,"minimum_lease_deadline":null,
            "allow_equal_revision":false,"failure_acknowledged":false
        });
        if action == "renew" {
            intent["lease_seconds"] = json!(60);
            intent["minimum_lease_deadline"] = json!(LATER);
            intent["policy"] = Value::Null;
            intent["policy_revision"] = Value::Null;
        }
        if null {
            intent[field] = Value::Null;
        } else {
            intent.as_object_mut().unwrap().remove(field);
        }
        document["records"][NAME]["lifecycle"] = intent;
        let before = serde_json::to_vec(&document).unwrap();
        fs::write(&path, &before).unwrap();
        let reopened = Controller::new(controller.store.state()).unwrap();
        assert!(matches!(reopened.store.list(), Err(Error::Store)));
        assert!(matches!(
            reopened.acknowledge_lifecycle_failure(&name(), &record.revision().unwrap()),
            Err(Error::Store)
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn policy_failure_requires_explicit_revision_pinned_acknowledgement() {
        let mut failed = false;
        let server = Server::new(move |method, path, _, _| {
            if let Some(response) = enforced_metadata(path) {
                return Some(response);
            }
            if method == "PUT" {
                if !failed {
                    failed = true;
                    return None;
                }
                let mut applied = enforced_instance(false);
                applied["revision"] = json!(2);
                return Some((200, applied));
            }
            if path.ends_with("/credentials") {
                return Some((
                    200,
                    json!({"instance":enforced_instance(true),"trafficAccessToken":"b".repeat(64),"mcpPath":format!("/sandboxes/{INSTANCE}/mcp"),"filesPath":"/files","credentialScope":"sandbox_lifetime"}),
                ));
            }
            Some((200, enforced_instance(true)))
        });
        let (_temp, controller, _) = setup(&server);
        let original = borrow_ready(&controller, &server);
        assert!(
            smol::block_on(controller.action(
                &name(),
                LifecycleAction::ApplyPolicy {
                    policy: policy(false)
                }
            ))
            .is_err()
        );
        let inspected = smol::block_on(controller.inspect(&name())).unwrap();
        assert!(inspected.lifecycle.as_ref().unwrap().is_pending());
        assert!(matches!(
            smol::block_on(controller.prepare_attach(&name(), ResumePolicy::Refuse)),
            Err(Error::Unresolved)
        ));
        assert!(matches!(
            controller.acknowledge_lifecycle_failure(&name(), &original.revision().unwrap()),
            Err(Error::ReviewChanged)
        ));
        let acknowledged = controller
            .acknowledge_lifecycle_failure(&name(), &inspected.revision().unwrap())
            .unwrap();
        let intent = acknowledged.lifecycle.as_ref().unwrap();
        assert!(intent.failure_acknowledged);
        assert_eq!(intent.observed_revision, None);
        assert_eq!(intent.policy, inspected.lifecycle.as_ref().unwrap().policy);
        let restarted = Controller::new(controller.store.state()).unwrap();
        assert!(restarted.blockers(&acknowledged).is_empty());
        let ticket =
            smol::block_on(restarted.prepare_attach(&name(), ResumePolicy::Refuse)).unwrap();
        assert!(
            ticket
                .record
                .lifecycle
                .as_ref()
                .unwrap()
                .failure_acknowledged
        );
        drop(ticket);
        let retried = smol::block_on(restarted.action_at(
            &name(),
            &acknowledged.revision().unwrap(),
            LifecycleAction::ApplyPolicy {
                policy: policy(false),
            },
        ))
        .unwrap();
        assert_eq!(retried.lifecycle.unwrap().observed_revision, Some(2));
        assert_eq!(retried.instance.unwrap().egress.policy, Some(policy(false)));
    }

    #[test_case(false; "idle_current_runtime_released")]
    #[test_case(true; "other_runtime_holder_remains_busy")]
    fn exclusive_control_after_runtime_detach(other_holder: bool) {
        let server = Server::new(|method, path, _, headers| {
            if path.ends_with("/pause") {
                let mut paused = instance();
                paused["state"] = json!("paused");
                paused["leaseDeadline"] = Value::Null;
                paused["revision"] = json!(2);
                return Some((200, paused));
            }
            regular(method, path, headers)
        });
        let (temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let config = SandboxStore::from_config_dir(&temp.path().join("config")).unwrap();
        let current = smol::block_on(controller.prepare_attach_reviewed(
            &name(),
            ResumePolicy::Refuse,
            None,
            Some(&config),
        ))
        .unwrap();
        let other = other_holder.then(|| {
            smol::block_on(controller.prepare_attach_reviewed(
                &name(),
                ResumePolicy::Refuse,
                None,
                Some(&config),
            ))
            .unwrap()
        });
        assert!(matches!(
            smol::block_on(controller.action_at(
                &name(),
                &record.revision().unwrap(),
                LifecycleAction::Pause
            )),
            Err(Error::Busy)
        ));
        drop(current);
        let result = smol::block_on(controller.action_at(
            &name(),
            &record.revision().unwrap(),
            LifecycleAction::Pause,
        ));
        if other_holder {
            assert!(matches!(result, Err(Error::Busy)));
            assert!(controller.store.get(&name()).unwrap().lifecycle.is_none());
        } else {
            assert_eq!(
                result.unwrap().instance.unwrap().state,
                InstanceState::Paused
            );
        }
        drop(other);
    }

    #[test_case(false; "nonpersistent_pause_never_reserves_intent")]
    fn unsupported_pause_is_rejected_before_durable_intent(persistent: bool) {
        let server = Server::new(|method, path, _, headers| regular(method, path, headers));
        let (_temp, controller, saved) = setup(&server);
        let original = create_ready(&controller, &saved);
        let mut record = original.clone();
        record.instance.as_mut().unwrap().persistent = persistent;
        controller.store.replace(&original, &record).unwrap();
        assert!(matches!(
            smol::block_on(controller.action_at(
                &name(),
                &record.revision().unwrap(),
                LifecycleAction::Pause
            )),
            Err(Error::PauseUnsupported)
        ));
        assert_eq!(
            controller.store.get(&name()).unwrap().revision().unwrap(),
            record.revision().unwrap()
        );
    }

    #[test]
    fn catalog_routes_omit_initial_after_and_follow_nonempty_cursor() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = requests.clone();
        let server = Server::new(move |method, path, _, _| {
            assert_eq!(method, "GET");
            received.lock().unwrap().push(path.to_owned());
            match path {
                "/daemon/v1/templates" => {
                    Some((200, json!({"items":[template()],"nextAfter":"base"})))
                }
                "/daemon/v1/templates?after=base" => {
                    Some((200, json!({"items":[],"nextAfter":""})))
                }
                "/daemon/v1/instances" => {
                    Some((200, json!({"items":[instance()],"nextAfter":INSTANCE})))
                }
                "/daemon/v1/instances?after=instance-test" => {
                    Some((200, json!({"items":[],"nextAfter":""})))
                }
                _ => Some((
                    400,
                    json!({"error":{"code":"invalid_request","retryable":false,"outcomeUnknown":false}}),
                )),
            }
        });
        let client = LifecycleClient::new(server.origin(), generate_api_key().unwrap()).unwrap();
        assert_eq!(smol::block_on(client.templates()).unwrap().len(), 1);
        assert_eq!(smol::block_on(client.instances(OWNER)).unwrap().len(), 1);
        assert_eq!(
            *requests.lock().unwrap(),
            [
                "/daemon/v1/templates",
                "/daemon/v1/templates?after=base",
                "/daemon/v1/instances",
                "/daemon/v1/instances?after=instance-test"
            ]
        );
    }

    #[test_case(false; "reviewed_create")]
    #[test_case(true; "owner_changed_before_create")]
    fn create_review_pins_catalog_digest_launch_and_owner(changed: bool) {
        let sends = Arc::new(Mutex::new(0));
        let requests = sends.clone();
        let server = Server::new(move |method, path, _, headers| {
            if method == "POST" {
                *requests.lock().unwrap() += 1;
            }
            if path == "/daemon/v1/templates" {
                return Some((200, json!({"items":[template()],"nextAfter":""})));
            }
            if path == "/daemon/v1/instances" && method == "GET" {
                return Some((200, json!({"items":[],"nextAfter":""})));
            }
            regular(method, path, headers)
        });
        let (_temp, controller, saved) = setup(&server);
        let snapshot = smol::block_on(controller.live_snapshot(&provider(&server))).unwrap();
        let profile = SandboxName::parse("dev").unwrap();
        let launch = saved
            .resolve_launch(&profile, &snapshot.doctor.capabilities, &snapshot.catalog)
            .unwrap();
        let review = super::CreateReview {
            owner_id: if changed {
                "different-owner".into()
            } else {
                OWNER.into()
            },
            image_sha256: snapshot.doctor.templates[0].image_sha256.clone(),
            launch_revision: launch.revision().clone(),
        };
        let result = smol::block_on(controller.create_reviewed(&saved, &profile, name(), &review));
        assert_eq!(result.is_err(), changed);
        assert_eq!(*sends.lock().unwrap(), usize::from(!changed));
        assert_eq!(controller.snapshots().unwrap().is_empty(), changed);
    }

    /// A replaced execution is an identity failure, not a stale review: no
    /// amount of refreshing brings the reviewed sandbox back.
    #[test]
    fn reviewed_attach_refuses_changed_live_execution_without_credentials() {
        let server = Server::new(move |method, path, _, headers| {
            assert!(!path.ends_with("/credentials"));
            if path.contains("/operations/") {
                let mut current = instance();
                current["revision"] = json!(2);
                current["executionID"] = json!(CHANGED_EXECUTION);
                return Some((200, operation(path.rsplit('/').next().unwrap(), current)));
            }
            regular(method, path, headers)
        });
        let (_temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        assert!(matches!(
            smol::block_on(controller.prepare_attach_at(&name(), &record.revision().unwrap())),
            Err(Error::Identity)
        ));
    }

    /// A reviewed revision pins the durable record, not the daemon's next
    /// observation of the sandbox. Applying a network policy from the TUI ends
    /// the run, applies, and reconnects pinned to the record the apply wrote;
    /// a daemon that has since recorded its own transition reports a newer
    /// instance, and refusing that ended the attached session for nothing.
    #[test]
    fn a_reviewed_attach_tolerates_a_daemon_observation_newer_than_the_review() {
        let server = Server::new(|method, path, _, headers| {
            let mut current = instance();
            current["revision"] = json!(SETTLED_REVISION);
            if path.ends_with("/credentials") {
                return Some((200, credentials(current)));
            }
            if method == "GET"
                && !path.ends_with("/discover")
                && !path.starts_with("/daemon/v1/templates/")
            {
                return Some(if path.contains("/operations/") {
                    (200, operation(path.rsplit('/').next().unwrap(), current))
                } else {
                    (200, current)
                });
            }
            regular(method, path, headers)
        });
        let (temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let config = SandboxStore::from_config_dir(&temp.path().join("config")).unwrap();

        let ticket = smol::block_on(controller.prepare_attach_reviewed(
            &name(),
            ResumePolicy::Refuse,
            Some(&record.revision().unwrap()),
            Some(&config),
        ))
        .unwrap();

        assert_eq!(ticket.record.instance.unwrap().revision, SETTLED_REVISION);
    }

    #[test_case("pause"; "pause")]
    #[test_case("resume"; "cold_resume")]
    #[test_case("renew"; "extend")]
    #[test_case("delete"; "owned_delete")]
    fn reviewed_controls_send_exact_conditions(route: &str) {
        let route = route.to_owned();
        let expected_route = route.clone();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let requests = calls.clone();
        let server = Server::new(move |method, path, body, headers| {
            if path.ends_with(&format!("/{expected_route}")) || method == "DELETE" {
                requests.lock().unwrap().push(body.clone());
                let mut response = instance();
                response["revision"] = json!(2);
                match expected_route.as_str() {
                    "pause" => {
                        response["state"] = json!("paused");
                        response["leaseDeadline"] = Value::Null;
                    }
                    "resume" => response["executionID"] = json!(CHANGED_EXECUTION),
                    "delete" => {
                        response["state"] = json!("deleted");
                        response["leaseDeadline"] = Value::Null;
                    }
                    _ => {}
                }
                return Some((200, response));
            }
            regular(method, path, headers)
        });
        let (_temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let action = match route.as_str() {
            "pause" => LifecycleAction::Pause,
            "resume" => LifecycleAction::Resume {
                lease_seconds: LEASE,
            },
            "renew" => LifecycleAction::Extend {
                lease_seconds: LEASE,
            },
            _ => LifecycleAction::Delete {
                destroy_borrowed: false,
            },
        };
        let result =
            smol::block_on(controller.action_at(&name(), &record.revision().unwrap(), action))
                .unwrap();
        assert_eq!(result.instance.unwrap().revision, 2);
        assert_eq!(result.lifecycle.unwrap().observed_revision, Some(2));
        let requests = calls.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["expectedRevision"], 1);
        assert_eq!(requests[0]["expectedExecutionID"], EXECUTION);
    }

    #[test_case(false; "local_record_conflict")]
    #[test_case(true; "remote_revision_conflict")]
    fn stale_review_never_retargets_or_replays(remote: bool) {
        let mutations = Arc::new(Mutex::new(0));
        let sent = mutations.clone();
        let server = Server::new(move |method, path, _, headers| {
            if path.ends_with("/pause") {
                *sent.lock().unwrap() += 1;
                return Some((
                    412,
                    json!({"error":{"code":"precondition_failed","retryable":false,"outcomeUnknown":false}}),
                ));
            }
            regular(method, path, headers)
        });
        let (_temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        if !remote {
            let changed = crate::InstanceRecord {
                detached: true,
                ..record.clone()
            };
            controller.store.replace(&record, &changed).unwrap();
        }
        let result = smol::block_on(controller.action_at(
            &name(),
            &record.revision().unwrap(),
            LifecycleAction::Pause,
        ));
        assert!(matches!(
            result,
            Err(Error::ReviewChanged) | Err(Error::Daemon { status: 412, .. })
        ));
        if remote {
            assert_eq!(
                controller.store.get(&name()).unwrap().revision().unwrap(),
                record.revision().unwrap()
            );
            smol::block_on(controller.inspect(&name())).unwrap();
        }
        assert_eq!(*mutations.lock().unwrap(), usize::from(remote));
    }

    #[test_case(false, false; "cancel_acknowledged")]
    #[test_case(true, false; "cancel_outcome_unknown")]
    #[test_case(false, true; "cleanup_pending_accepted")]
    #[test_case(true, true; "cleanup_pending_response_lost")]
    fn cancellation_is_durable_and_reconciled_without_replay(lost: bool, cleanup: bool) {
        let key = Arc::new(Mutex::new(String::new()));
        let operation_key = key.clone();
        let cancels = Arc::new(Mutex::new(0));
        let requests = cancels.clone();
        let server = Server::new(move |method, path, body, headers| {
            if method == "POST" && path == "/daemon/v1/instances" {
                let (_, mut response) = regular(method, path, headers).unwrap();
                *operation_key.lock().unwrap() =
                    response["operationID"].as_str().unwrap().to_owned();
                response["status"] = json!("creating");
                response["instance"] = Value::Null;
                return Some((202, response));
            }
            if path.ends_with("/cancel") {
                assert_eq!(body["expectedExecutionID"], EXECUTION);
                *requests.lock().unwrap() += 1;
                if lost {
                    return None;
                }
            }
            if path.contains("/operations/") {
                let mut response = operation(&operation_key.lock().unwrap(), Value::Null);
                response["status"] = json!(if cleanup {
                    "cleanup_pending"
                } else {
                    "creating"
                });
                response["cancelRequested"] = json!(true);
                return Some((if method == "POST" { 202 } else { 200 }, response));
            }
            regular(method, path, headers)
        });
        let (_temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let result = smol::block_on(controller.cancel_create(&name(), &record.revision().unwrap()));
        assert_eq!(result.is_err(), lost);
        if lost {
            assert!(
                controller
                    .store
                    .get(&name())
                    .unwrap()
                    .lifecycle
                    .unwrap()
                    .observed_revision
                    .is_none()
            );
        }
        let recovered = smol::block_on(controller.inspect(&name())).unwrap();
        assert_eq!(
            recovered.lifecycle.as_ref().unwrap().observed_revision,
            Some(0)
        );
        assert_eq!(
            recovered
                .create
                .as_ref()
                .unwrap()
                .operation
                .as_ref()
                .unwrap()
                .status,
            if cleanup {
                OperationStatus::CleanupPending
            } else {
                OperationStatus::Creating
            }
        );
        assert!(
            recovered
                .create
                .as_ref()
                .unwrap()
                .operation
                .as_ref()
                .unwrap()
                .cancel_requested
        );
        assert!(
            smol::block_on(controller.cancel_create(&name(), &recovered.revision().unwrap()))
                .is_err()
        );
        assert_eq!(*cancels.lock().unwrap(), 1);
    }

    #[test_case(false; "unacknowledged_cancel_response")]
    #[test_case(true; "cancel_crash_before_send")]
    fn cancellation_does_not_resolve_without_daemon_acknowledgement(before_send: bool) {
        let calls = Arc::new(Mutex::new(0));
        let cancels = calls.clone();
        let mut key = String::new();
        let server = Server::new(move |method, path, _, headers| {
            if path.ends_with("/discover") {
                return Some((200, discovery(OWNER)));
            }
            if path.starts_with("/daemon/v1/templates/") {
                return Some((200, template()));
            }
            if path == "/daemon/v1/instances" && method == "POST" {
                key = regular(method, path, headers).unwrap().1["operationID"]
                    .as_str()
                    .unwrap()
                    .into();
            }
            if path.ends_with("/cancel") {
                *cancels.lock().unwrap() += 1;
            }
            let mut response = operation(&key, Value::Null);
            response["status"] = json!("creating");
            Some((if method == "POST" { 202 } else { 200 }, response))
        });
        let (_temp, controller, saved) = setup(&server);
        let original = create_ready(&controller, &saved);
        if before_send {
            let mut pending = original.clone();
            pending.lifecycle = Some(serde_json::from_value(json!({"action":"cancel-create","expected":{"expectedExecutionID":EXECUTION,"expectedRevision":0},"lease_seconds":null,"observed_revision":null,"policy":null,"policy_revision":null,"minimum_lease_deadline":null,"allow_equal_revision":false,"failure_acknowledged":false})).unwrap());
            controller.store.replace(&original, &pending).unwrap();
        } else {
            assert!(matches!(
                smol::block_on(controller.cancel_create(&name(), &original.revision().unwrap())),
                Err(Error::Identity)
            ));
        }
        let restarted = Controller::new(controller.store.state()).unwrap();
        let inspected = smol::block_on(restarted.inspect(&name())).unwrap();
        assert!(inspected.lifecycle.as_ref().unwrap().is_pending());
        assert!(matches!(
            smol::block_on(restarted.cancel_create(&name(), &inspected.revision().unwrap())),
            Err(Error::Unresolved)
        ));
        assert_eq!(*calls.lock().unwrap(), usize::from(!before_send));
    }

    #[test]
    fn accepted_cancellation_is_persisted_and_polled_through_cleanup() {
        let calls = Arc::new(Mutex::new(0));
        let cancels = calls.clone();
        let polls = Arc::new(Mutex::new(0));
        let lookups = polls.clone();
        let mut key = String::new();
        let server = Server::new(move |method, path, _, headers| {
            if path.ends_with("/discover") {
                return Some((200, discovery(OWNER)));
            }
            if path.starts_with("/daemon/v1/templates/") {
                return Some((200, template()));
            }
            if path == "/daemon/v1/instances" && method == "POST" {
                key = regular(method, path, headers).unwrap().1["operationID"]
                    .as_str()
                    .unwrap()
                    .into();
                let mut response = operation(&key, Value::Null);
                response["status"] = json!("creating");
                return Some((202, response));
            }
            assert_eq!(
                path,
                if method == "POST" {
                    format!("/daemon/v1/operations/{key}/cancel")
                } else {
                    format!("/daemon/v1/operations/{key}")
                }
            );
            let mut current = instance();
            let mut status = OperationStatus::CleanupPending;
            if method == "POST" {
                *cancels.lock().unwrap() += 1;
            } else {
                let mut count = lookups.lock().unwrap();
                *count += 1;
                if *count > 1 {
                    status = OperationStatus::Deleted;
                    current["state"] = json!("deleted");
                    current["revision"] = json!(2);
                    current["leaseDeadline"] = Value::Null;
                }
            }
            let mut response = operation(&key, current);
            response["status"] = json!(status);
            response["cancelRequested"] = json!(true);
            Some((if method == "POST" { 202 } else { 200 }, response))
        });
        let (_temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let accepted =
            smol::block_on(controller.cancel_create(&name(), &record.revision().unwrap())).unwrap();
        let restarted = Controller::new(controller.store.state()).unwrap();
        assert_eq!(
            accepted.revision().unwrap(),
            restarted.store.get(&name()).unwrap().revision().unwrap()
        );
        assert!(!restarted.blockers(&accepted).is_empty());
        assert!(matches!(
            smol::block_on(restarted.wait_ready(&name())),
            Err(Error::NotReady)
        ));
        let terminal = restarted.store.get(&name()).unwrap();
        assert_eq!(
            terminal.create.unwrap().operation.unwrap().status,
            OperationStatus::Deleted
        );
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(*polls.lock().unwrap(), 2);
    }

    #[test_case(false; "network_applied")]
    #[test_case(true; "network_wrong_effective_revision")]
    fn network_updates_pin_revision_and_report_effective_policy(mismatch: bool) {
        let policy = crate::dto::Policy {
            mode: "sni-only".into(),
            domains: vec![DOMAIN.into()],
            cidrs: Vec::new(),
        };
        let proposed = policy.clone();
        let server = Server::new(move |method, path, body, _headers| {
            if path.ends_with("/discover") {
                let mut value = discovery(OWNER);
                value["networkTopology"] = json!(ENFORCED_TOPOLOGY);
                value["capabilities"]["egressPolicy"] = json!(true);
                value["tlsModes"] = json!(["sni-only", "mitm"]);
                return Some((200, value));
            }
            if path.contains("/templates/") {
                let mut value = template();
                value["networkTopology"] = json!(ENFORCED_TOPOLOGY);
                return Some((200, value));
            }
            let mut value = instance();
            value["networkTopology"] = json!(ENFORCED_TOPOLOGY);
            value["egress"] = json!({"enforced":true,"revision":DENY_REVISION,"effectiveRevision":DENY_REVISION,"policy":{"mode":"sni-only","domains":[],"cidrs":[]}});
            if method == "PUT" {
                assert!(path.ends_with("/policy"));
                assert_eq!(body["expectedRevision"], 1);
                assert_eq!(body["expectedExecutionID"], EXECUTION);
                assert_eq!(body["egress"], serde_json::to_value(&proposed).unwrap());
                value["revision"] = json!(2);
                value["egress"] = json!({"enforced":true,"revision":ALLOW_REVISION,"effectiveRevision":if mismatch {DENY_REVISION} else {ALLOW_REVISION},"policy":proposed});
            }
            Some((200, value))
        });
        let (_temp, controller, _) = setup(&server);
        let record = smol::block_on(controller.borrow(
            name(),
            SandboxName::parse("daemon").unwrap(),
            provider(&server),
            INSTANCE,
            caudra_workspace::WorkspacePath::new(".").unwrap(),
        ))
        .unwrap();
        let result = smol::block_on(controller.action_at(
            &name(),
            &record.revision().unwrap(),
            LifecycleAction::ApplyPolicy {
                policy: policy.clone(),
            },
        ));
        if mismatch {
            assert!(matches!(result, Err(Error::Identity)));
        } else {
            assert_eq!(
                result.unwrap().instance.unwrap().egress.policy,
                Some(policy)
            );
        }
    }

    fn tempdir() -> TempDir {
        tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(PRIVATE_MODE))
            .tempdir()
            .unwrap()
    }

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct Message;
    impl caudra_storage::sessions::TitleSource for Message {
        fn first_user_text(&self) -> Option<&str> {
            None
        }
    }

    struct Server {
        address: SocketAddr,
        stopped: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }

    impl Server {
        fn new(
            mut handler: impl FnMut(&str, &str, &Value, &str) -> Option<(u16, Value)> + Send + 'static,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let stopped = Arc::new(AtomicBool::new(false));
            let stop = stopped.clone();
            let worker = thread::spawn(move || {
                for stream in listener.incoming() {
                    let mut stream = stream.unwrap();
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut headers = String::new();
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                        headers.push_str(&line);
                    }
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|n| n.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let body = if body.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&body).unwrap()
                    };
                    let mut parts = headers.lines().next().unwrap().split_whitespace();
                    if let Some((status, value)) = handler(
                        parts.next().unwrap(),
                        parts.next().unwrap(),
                        &body,
                        &headers,
                    ) {
                        let body = serde_json::to_vec(&value).unwrap();
                        write!(stream, "HTTP/1.1 {status} response\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                        let _ = stream.write_all(&body);
                    }
                }
            });
            Self {
                address,
                stopped,
                worker: Some(worker),
            }
        }
        fn origin(&self) -> SandboxOrigin {
            SandboxOrigin::parse(&format!("http://{}", self.address)).unwrap()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            let _ = TcpStream::connect(self.address);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }

    fn name() -> SandboxName {
        SandboxName::parse(NAME).unwrap()
    }
    fn template() -> Value {
        json!({"schemaVersion":1,"id":"base","architecture":"x86_64","machine":"q35",
            "minimum":{"cpuCount":1,"memoryMB":512,"diskSizeMB":1024},"defaults":{"cpuCount":2,"memoryMB":1024,"diskSizeMB":1024},
            "networkTopology":"slirp-unrestricted","workcell":{"version":"test","sha256":"","protocolVersion":"2026-07-28","transferProtocol":"workcell-reviewed-v1","remoteWorkspace":true,"workspaceSnapshots":true,"reviewedTransfer":true},
            "build":{"recipe":"import","recipeSHA256":"","sourceRevision":""},"revision":DIGEST,"imageSHA256":DIGEST,"warmStart":false,
            "image":{"format":"qcow2","fileSizeBytes":1024,"virtualSizeBytes":1073741824_u64,"clusterSize":65536,"backingPolicy":"standalone"}})
    }
    fn discovery(owner: &str) -> Value {
        json!({"apiVersion":"1","ownerID":owner,"serverTime":NOW,"authentication":"api_key_namespace","templateID":"base","networkTopology":"slirp-unrestricted",
            "capabilities":{"idempotentCreate":true,"operationLookup":true,"conditionalMutations":true,"explicitCredentials":true,"persistentDisk":true,"memoryPause":false,"egressPolicy":false,"cancelCreate":true,"templateCatalog":true,"conditionalTemplateCreate":true,"warmStart":false,"localTemplateAdmin":true,"httpTemplateAdmin":false},
            "limits":{"maxLeaseSeconds":LEASE_CAP,"runtimeAdmission":4,"operationJournalEntries":4096,"listPageSize":100,"resources":{"cpuCount":4,"memoryMB":4096,"diskSizeMB":8192},"newKeyMaxAgeSeconds":300,"newKeyFutureSkewSeconds":30},
            "retention":{"pausedDiskMaxAgeSeconds":0,"operationHistorySeconds":86400,"historyStartsAfter":"instance_removed"},"idempotencyKey":"uuidv7","recovery":"query_operation_never_replay_unknown","credentialScope":"sandbox_lifetime","proxyOrigin":"client_configured"})
    }
    fn instance() -> Value {
        json!({"ownerID":OWNER,"sandboxID":INSTANCE,"executionID":EXECUTION,"revision":1,"state":"running","workspaceGeneration":GENERATION,
            "expectedWorkcell":{"serverID":INSTANCE,"workspaceID":INSTANCE,"workspaceGeneration":GENERATION,"projectID":INSTANCE,"principalID":OWNER},
            "template":{"id":"base","revision":DIGEST,"imageIdentity":DIGEST},"resources":{"cpuCount":2,"memoryMB":1024,"diskSizeMB":1024},"networkTopology":"slirp-unrestricted","persistent":true,"pauseUnclean":false,"leaseDeadline":LATER,
            "retention":{"pausedDiskMaxAgeSeconds":0,"deadline":null},"egress":{"enforced":false,"revision":"unrestricted","effectiveRevision":"unrestricted","policy":null}})
    }
    fn credentials(instance: Value) -> Value {
        json!({"instance":instance,"trafficAccessToken":"b".repeat(64),"mcpPath":format!("/sandboxes/{INSTANCE}/mcp"),"filesPath":"/files","credentialScope":"sandbox_lifetime"})
    }
    fn operation(key: &str, instance: Value) -> Value {
        json!({"operationID":key,"ownerID":OWNER,"sandboxID":INSTANCE,"executionID":EXECUTION,"requestDigest":REQUEST_DIGEST,"status":"succeeded","cancelRequested":false,"createdAt":NOW,"updatedAt":NOW,"historyDeadline":null,"instance":instance})
    }
    fn provider(server: &Server) -> SandboxProvider {
        SandboxProvider {
            kind: ProviderKind::E2bLibvirt,
            api_endpoint: server.origin(),
            proxy_endpoint: server.origin(),
            credential_ref: SandboxCredentialRef::new("lifecycle").unwrap(),
        }
    }
    fn setup(server: &Server) -> (TempDir, Controller, caudra_config::sandbox::SavedSandboxes) {
        let temp = tempdir();
        let state = StateDir::from_path(temp.path().join("state"));
        let provider = provider(server);
        save_sandbox_api_key(
            &state,
            &provider.credential_ref,
            &generate_api_key().unwrap(),
        )
        .unwrap();
        let draft: SandboxDraft = serde_json::from_value(json!({"providers":{"daemon":provider},"networks":{"net":{"enforcement":"off"}},"transfers":{"transfer":{}},
            "profiles":{"dev":{"provider":"daemon","template":"base","cpus":2,"memory_mib":1024,"disk_gib":1,"cwd":".","network":"net","transfer":"transfer","persistent":true,"running_ttl_seconds":300,"on_exit":"detach"}}})).unwrap();
        let store = SandboxStore::from_config_dir(&temp.path().join("config")).unwrap();
        let loaded = store.load().unwrap();
        let saved = store.save(&loaded, &draft).unwrap();
        (
            temp,
            Controller::new(&state).unwrap(),
            saved.saved().clone(),
        )
    }

    #[test]
    fn create_launches_the_catalog_head_it_validated() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_for_server = calls.clone();
        let server = Server::new(move |method, path, body, _| {
            calls_for_server.lock().unwrap().push((
                method.to_owned(),
                path.to_owned(),
                body.clone(),
            ));
            if path.ends_with("/discover") {
                return Some((200, discovery(OWNER)));
            }
            if path.starts_with("/daemon/v1/templates/") {
                let mut head = template();
                head["revision"] = json!(HEAD_REVISION);
                return Some((200, head));
            }
            None
        });
        let (_temp, controller, saved) = setup(&server);
        let profile = SandboxName::parse("dev").unwrap();
        assert!(smol::block_on(controller.create(&saved, &profile, name())).is_err());
        let calls = calls.lock().unwrap();
        let template_routes: Vec<&str> = calls
            .iter()
            .filter(|(_, path, _)| path.contains("/templates/"))
            .map(|(_, path, _)| path.as_str())
            .collect();
        assert_eq!(template_routes, [HEAD_ROUTE]);
        let (_, _, create) = calls
            .iter()
            .find(|(method, _, _)| method == "POST")
            .unwrap();
        assert_eq!(create["expectedTemplateRevision"], HEAD_REVISION);
        let reserved = controller.store.get(&name()).unwrap();
        assert_eq!(reserved.template.revision.as_str(), HEAD_REVISION);
    }

    #[test_case(false; "lost_response")]
    #[test_case(true; "unknown_history")]
    fn create_reserves_owner_key_and_snapshot_before_send_never_replays(unknown: bool) {
        let state_path = Arc::new(Mutex::new(None::<StateDir>));
        let state_for_server = state_path.clone();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_for_server = calls.clone();
        let server = Server::new(move |method, path, body, headers| {
            calls_for_server
                .lock()
                .unwrap()
                .push((method.to_owned(), path.to_owned()));
            assert!(headers.to_ascii_lowercase().contains("x-api-key: "));
            if path.ends_with("/discover") {
                return Some((200, discovery(OWNER)));
            }
            if path.starts_with("/daemon/v1/templates/") {
                return Some((200, template()));
            }
            if method == "POST" {
                let state = state_for_server.lock().unwrap().clone().unwrap();
                let saved = Controller::new(&state).unwrap().store.get(&name()).unwrap();
                let intent = saved.create.unwrap();
                assert_eq!(saved.owner_id, OWNER);
                assert_eq!(&serde_json::to_value(intent.request).unwrap(), body);
                let key = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("idempotency-key: ")
                            .map(str::to_owned)
                    })
                    .unwrap();
                assert_eq!(intent.key, key);
                assert_eq!(uuid::Uuid::parse_str(&key).unwrap().get_version_num(), 7);
                assert!(saved.launch.is_some());
                return None;
            }
            let key = path.rsplit('/').next().unwrap();
            Some(if unknown {
                (
                    410,
                    json!({"error":{"code":"history_unavailable","retryable":false,"outcomeUnknown":true}}),
                )
            } else {
                (200, operation(key, instance()))
            })
        });
        let (_temp, controller, saved) = setup(&server);
        *state_path.lock().unwrap() = Some(controller.store.state().clone());
        let profile = SandboxName::parse("dev").unwrap();
        assert!(smol::block_on(controller.create(&saved, &profile, name())).is_err());
        let recovered = Controller::new(controller.store.state()).unwrap();
        let inspected = smol::block_on(recovered.inspect(&name()));
        assert_eq!(inspected.is_err(), unknown);
        assert!(matches!(
            smol::block_on(recovered.create(&saved, &profile, name())),
            Err(Error::Exists)
        ));
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(method, _)| method == "POST")
                .count(),
            1
        );
    }

    fn create_ready(
        controller: &Controller,
        saved: &caudra_config::sandbox::SavedSandboxes,
    ) -> crate::InstanceRecord {
        smol::block_on(controller.create(saved, &SandboxName::parse("dev").unwrap(), name()))
            .unwrap()
    }

    fn regular(method: &str, path: &str, headers: &str) -> Option<(u16, Value)> {
        if path.ends_with("/discover") {
            Some((200, discovery(OWNER)))
        } else if path.starts_with("/daemon/v1/templates/") {
            Some((200, template()))
        } else if path.ends_with("/credentials") {
            Some((200, credentials(instance())))
        } else if method == "POST" {
            let key = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("idempotency-key: ")
                        .map(str::to_owned)
                })
                .unwrap();
            Some((200, operation(&key, instance())))
        } else if path.contains("/operations/") {
            Some((200, operation(path.rsplit('/').next().unwrap(), instance())))
        } else {
            Some((200, instance()))
        }
    }

    fn uncapped(method: &str, path: &str, headers: &str) -> Option<(u16, Value)> {
        if path.ends_with("/discover") {
            let mut discovered = discovery(OWNER);
            discovered["limits"]["maxLeaseSeconds"] = json!(LeaseSeconds::NO_EXPIRY);
            return Some((200, discovered));
        }
        regular(method, path, headers)
    }

    #[test]
    fn create_sends_a_lease_with_no_expiry_to_an_uncapped_daemon() {
        let sent = Arc::new(Mutex::new(None));
        let sent_for_server = sent.clone();
        let server = Server::new(move |method, path, body, headers| {
            if method == "POST" && path == "/daemon/v1/instances" {
                *sent_for_server.lock().unwrap() = Some(body["leaseSeconds"].clone());
            }
            uncapped(method, path, headers)
        });
        let (temp, controller, _) = setup(&server);
        let config = SandboxStore::from_config_dir(&temp.path().join("config")).unwrap();
        let loaded = config.load().unwrap();
        let mut draft = loaded.draft();
        draft
            .profiles
            .get_mut(&SandboxName::parse("dev").unwrap())
            .unwrap()
            .running_ttl_seconds = LeaseSeconds::NO_EXPIRY;
        let saved = config.save(&loaded, &draft).unwrap();
        create_ready(&controller, saved.saved());
        assert_eq!(*sent.lock().unwrap(), Some(json!(LeaseSeconds::NO_EXPIRY)));
    }

    #[test_case("renew", LeaseSeconds::NO_EXPIRY, None, true; "renew_to_no_expiry")]
    #[test_case("resume", LeaseSeconds::NO_EXPIRY, None, true; "resume_with_no_expiry")]
    #[test_case("renew", LeaseSeconds::NO_EXPIRY, Some(LATER), false; "no_expiry_answered_with_a_deadline")]
    #[test_case("resume", LEASE, None, false; "finite_answered_without_a_deadline")]
    fn no_expiry_lease_is_sent_and_verified(
        route: &'static str,
        lease: LeaseSeconds,
        deadline: Option<&'static str>,
        accepted: bool,
    ) {
        let server = Server::new(move |method, path, body, headers| {
            if path.ends_with(&format!("/{route}")) {
                assert_eq!(body["leaseSeconds"], json!(lease));
                let mut current = instance();
                current["revision"] = json!(SETTLED_REVISION);
                current["leaseDeadline"] = json!(deadline);
                if route == "resume" {
                    current["executionID"] = json!(CHANGED_EXECUTION);
                }
                return Some((200, current));
            }
            uncapped(method, path, headers)
        });
        let (_temp, controller, saved) = setup(&server);
        let created = create_ready(&controller, &saved);
        let action = if route == "resume" {
            let mut paused = created.clone();
            let instance = paused.instance.as_mut().unwrap();
            instance.state = InstanceState::Paused;
            instance.lease_deadline = None;
            controller.store.replace(&created, &paused).unwrap();
            LifecycleAction::Resume {
                lease_seconds: lease,
            }
        } else {
            LifecycleAction::Extend {
                lease_seconds: lease,
            }
        };
        let result = smol::block_on(controller.action(&name(), action));
        if !accepted {
            assert!(matches!(result, Err(Error::Lease)));
            return;
        }
        let record = result.unwrap();
        let instance = record.instance.unwrap();
        assert_eq!(instance.state, InstanceState::Running);
        assert_eq!(instance.lease_deadline, None);
        let intent = record.lifecycle.unwrap();
        assert_eq!(intent.lease_seconds, Some(LeaseSeconds::NO_EXPIRY));
        assert_eq!(intent.minimum_lease_deadline, None);
        assert_eq!(intent.observed_revision, Some(SETTLED_REVISION));
    }

    #[test]
    fn no_expiry_lease_needs_an_uncapped_daemon() {
        let server = Server::new(|method, path, _, headers| {
            assert!(!path.ends_with("/renew"));
            regular(method, path, headers)
        });
        let (_temp, controller, saved) = setup(&server);
        let created = create_ready(&controller, &saved);
        assert!(matches!(
            smol::block_on(controller.action(
                &name(),
                LifecycleAction::Extend {
                    lease_seconds: LeaseSeconds::NO_EXPIRY
                }
            )),
            Err(Error::LeaseOverCap { requested, max })
                if requested == LeaseSeconds::NO_EXPIRY && max == LEASE_CAP
        ));
        assert_eq!(
            controller.store.get(&name()).unwrap().revision().unwrap(),
            created.revision().unwrap()
        );
    }

    #[test_case(InstanceState::Running, LEASE, Error::LeaseNoExpiry; "finite_never_shortens_no_expiry")]
    #[test_case(InstanceState::Paused, LeaseSeconds::NO_EXPIRY, Error::Lease; "paused_no_expiry")]
    #[test_case(InstanceState::Paused, LEASE, Error::Lease; "paused_finite")]
    fn extend_without_a_deadline_never_dispatches(
        state: InstanceState,
        lease: LeaseSeconds,
        expected: Error,
    ) {
        let server = Server::new(|method, path, _, headers| {
            assert!(!path.ends_with("/renew"));
            uncapped(method, path, headers)
        });
        let (_temp, controller, saved) = setup(&server);
        let created = create_ready(&controller, &saved);
        let mut current = created.clone();
        let instance = current.instance.as_mut().unwrap();
        instance.state = state;
        instance.lease_deadline = None;
        controller.store.replace(&created, &current).unwrap();
        assert_eq!(
            smol::block_on(controller.action(
                &name(),
                LifecycleAction::Extend {
                    lease_seconds: lease
                }
            ))
            .unwrap_err()
            .to_string(),
            expected.to_string()
        );
        assert_eq!(
            controller.store.get(&name()).unwrap().revision().unwrap(),
            current.revision().unwrap()
        );
    }

    #[test_case(LeaseSeconds::NO_EXPIRY, None, true; "no_expiry_without_minimum")]
    #[test_case(LeaseSeconds::NO_EXPIRY, Some(LATER), false; "no_expiry_with_minimum")]
    #[test_case(LEASE, None, false; "finite_without_minimum")]
    fn lease_intents_carry_a_minimum_deadline_exactly_when_finite(
        lease: LeaseSeconds,
        minimum: Option<&str>,
        valid: bool,
    ) {
        let server = Server::new(|method, path, _, headers| regular(method, path, headers));
        let (_temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let path = controller
            .store
            .state()
            .persistent_path()
            .join(INSTANCE_STORE);
        let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        document["records"][NAME]["lifecycle"] = json!({
            "action":"renew","expected":record.instance.as_ref().unwrap().expected(),
            "lease_seconds":lease,"observed_revision":null,"policy":null,"policy_revision":null,
            "minimum_lease_deadline":minimum,"allow_equal_revision":false,"failure_acknowledged":false
        });
        fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let reopened = Controller::new(controller.store.state()).unwrap();
        let listed = reopened.store.list();
        if valid {
            assert!(listed.is_ok());
        } else {
            assert!(matches!(listed, Err(Error::Store)));
        }
    }

    #[test]
    fn profile_drift_does_not_rebase_saved_instance_and_borrowed_delete_is_detach() {
        let server = Server::new(|method, path, _, headers| regular(method, path, headers));
        let (temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let config = SandboxStore::from_config_dir(&temp.path().join("config")).unwrap();
        let loaded = config.load().unwrap();
        let mut draft = loaded.draft();
        draft
            .profiles
            .get_mut(&SandboxName::parse("dev").unwrap())
            .unwrap()
            .cpus = super::nonzero(4).unwrap();
        draft
            .providers
            .get_mut(&SandboxName::parse("daemon").unwrap())
            .unwrap()
            .api_endpoint = SandboxOrigin::parse("https://changed.test").unwrap();
        config.save(&loaded, &draft).unwrap();
        assert_eq!(
            smol::block_on(controller.inspect(&name())).unwrap().launch,
            record.launch
        );
        let mut borrowed = record.clone();
        borrowed.ownership = Ownership::Borrowed;
        controller.store.replace(&record, &borrowed).unwrap();
        let before = controller.store.get(&name()).unwrap();
        let detached = smol::block_on(controller.action(
            &name(),
            LifecycleAction::Delete {
                destroy_borrowed: false,
            },
        ))
        .unwrap();
        assert!(detached.detached);
        assert_eq!(before.instance, detached.instance);
        assert_eq!(before.launch, detached.launch);
        assert!(matches!(
            controller.store.replace(&record, &borrowed),
            Err(Error::PrivateFile(PrivateFileError::Conflict))
        ));
    }

    fn binding(record: &crate::InstanceRecord, part: &str) -> StoredWorkspaceBinding {
        let value = |key, original: &str| {
            if part == key {
                "mismatch".to_owned()
            } else {
                original.to_owned()
            }
        };
        let anchor = if part == "origin" {
            "https://different.test"
        } else {
            record.provider.proxy_endpoint.as_str()
        };
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new(anchor).unwrap(),
            value("server", INSTANCE),
            value("workspace", INSTANCE),
            value("generation", GENERATION),
            value("namespace", "1"),
        )
        .unwrap();
        let principal =
            AuthenticatedPrincipalId::new(authority.clone(), value("principal", OWNER)).unwrap();
        let project = ProjectIdentity::new(
            authority.clone(),
            ProjectKey::new(value("project", INSTANCE)).unwrap(),
        );
        StoredWorkspaceBinding::new(
            SessionWorkspaceBinding::new(
                SessionBindingId::new("binding").unwrap(),
                authority,
                principal,
                project,
            )
            .unwrap(),
            CwdHandle::new("cwd").unwrap(),
            None,
        )
        .unwrap()
    }

    #[test_case("origin")]
    #[test_case("server")]
    #[test_case("workspace")]
    #[test_case("generation")]
    #[test_case("principal")]
    #[test_case("project")]
    #[test_case("namespace")]
    fn attachment_pins_full_authority_and_never_saves_bearer(part: &str) {
        let server = Server::new(|method, path, _, headers| regular(method, path, headers));
        let (temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let config = SandboxStore::from_config_dir(&temp.path().join("config")).unwrap();
        let ticket = smol::block_on(controller.prepare_attach_reviewed(
            &name(),
            ResumePolicy::Refuse,
            None,
            Some(&config),
        ))
        .unwrap();
        let (stored, lease) = controller
            .confirm_attachment(ticket, &binding(&record, ""))
            .unwrap();
        assert_eq!(stored.sandbox_record(), Some(record.id));
        assert!(matches!(
            smol::block_on(controller.action(&name(), LifecycleAction::Pause)),
            Err(Error::Busy)
        ));
        drop(lease);
        let ticket = smol::block_on(controller.prepare_attach_reviewed(
            &name(),
            ResumePolicy::Refuse,
            None,
            Some(&config),
        ))
        .unwrap();
        assert!(matches!(
            controller.confirm_attachment(ticket, &binding(&record, part)),
            Err(Error::Identity)
        ));
        let data =
            std::fs::read_to_string(temp.path().join("state/sandboxes/instances.json")).unwrap();
        assert!(!data.contains(&"b".repeat(64)));
        assert!(!data.contains("trafficAccessToken"));
    }

    #[test_case("ownerID", json!("other-owner"))]
    #[test_case("executionID", json!(-1))]
    #[test_case("revision", json!(-1))]
    #[test_case("revision", json!(0))]
    #[test_case("workspaceGeneration", json!("other-generation"))]
    fn invalid_identity_and_signed_conditions_are_rejected(field: &str, replacement: Value) {
        let mut value = instance();
        value[field] = replacement;
        assert!(
            !serde_json::from_value::<Instance>(value)
                .is_ok_and(|instance| instance.validate(OWNER).is_ok())
        );
    }

    #[test_case("protocolVersion", json!("old"))]
    #[test_case("transferProtocol", json!("workcell-raw-v1"); "raw_transfer_is_not_compatible")]
    #[test_case("remoteWorkspace", json!(false))]
    #[test_case("workspaceSnapshots", json!(false))]
    #[test_case("reviewedTransfer", json!(false))]
    fn incompatible_images_cannot_launch(field: &str, value: Value) {
        let mut template = template();
        template["workcell"][field] = value;
        assert!(
            !template_entry(&serde_json::from_value::<Template>(template).unwrap())
                .unwrap()
                .workcell_compatible
        );
    }

    #[test_case("transferProtocol", true; "current_wire_field")]
    #[test_case("transfer", false; "removed_wire_field_is_not_an_alias")]
    fn catalog_transfer_protocol_has_one_wire_spelling(field: &str, accepted: bool) {
        let mut value = template();
        let workcell = value["workcell"].as_object_mut().unwrap();
        let protocol = workcell.remove("transferProtocol").unwrap();
        workcell.insert(field.into(), protocol);
        let decoded = serde_json::from_value::<Template>(value);
        assert_eq!(decoded.is_ok(), accepted);
        if let Ok(template) = decoded {
            assert!(template.manifest.validate().is_ok());
            assert!(
                serde_json::to_value(template).unwrap()["workcell"]
                    .get("transfer")
                    .is_none()
            );
        }
    }

    #[test_case("/transfers", true; "disjoint_transfer_store")]
    #[test_case("", false; "layout_requires_transfer_store")]
    #[test_case("/workspace/private", false; "inside_workspace")]
    #[test_case("/snapshots", false; "same_as_snapshot_store")]
    #[test_case("/", false; "ancestor_of_other_roots")]
    fn catalog_transfer_layout_is_validated(root: &str, accepted: bool) {
        let mut value = template();
        value["workcell"]["workspaceRoot"] = json!("/workspace");
        value["workcell"]["snapshotRoot"] = json!("/snapshots");
        value["workcell"]["transferRoot"] = json!(root);
        let template = serde_json::from_value::<Template>(value).unwrap();
        assert_eq!(template.manifest.validate().is_ok(), accepted);
    }

    #[test_case(false; "conditional_pause")]
    #[test_case(true; "non_shortening_renewal")]
    fn controls_reserve_conditions_and_refuse_shortening(shorten: bool) {
        let state = Arc::new(Mutex::new(None::<StateDir>));
        let server_state = state.clone();
        let server = Server::new(move |method, path, body, headers| {
            if path.ends_with("/pause") || path.ends_with("/renew") {
                assert_eq!(body["expectedExecutionID"], EXECUTION);
                assert_eq!(body["expectedRevision"], 1);
                let controller =
                    Controller::new(server_state.lock().unwrap().as_ref().unwrap()).unwrap();
                assert!(
                    controller
                        .store
                        .get(&name())
                        .unwrap()
                        .lifecycle
                        .unwrap()
                        .observed_revision
                        .is_none()
                );
                let mut response = instance();
                response["revision"] = json!(2);
                if shorten {
                    response["leaseDeadline"] = json!(EARLIER);
                } else {
                    response["state"] = json!("paused");
                    response["leaseDeadline"] = Value::Null;
                }
                return Some((200, response));
            }
            regular(method, path, headers)
        });
        let (_temp, controller, saved) = setup(&server);
        *state.lock().unwrap() = Some(controller.store.state().clone());
        let record = create_ready(&controller, &saved);
        let _active = shorten.then(|| controller.store.lease(&record, false).unwrap());
        let action = if shorten {
            LifecycleAction::Extend {
                lease_seconds: LEASE,
            }
        } else {
            LifecycleAction::Pause
        };
        let result = smol::block_on(controller.action(&name(), action));
        if shorten {
            assert!(matches!(result, Err(Error::Lease)));
        } else {
            assert_eq!(
                result.unwrap().instance.unwrap().state,
                InstanceState::Paused
            );
        }
    }

    #[test]
    fn session_writer_blocks_even_without_runtime_holder() {
        let server = Server::new(|method, path, _, headers| regular(method, path, headers));
        let (_temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let mut bound = record.clone();
        bound.workcell_binding = Some(binding(&record, ""));
        controller.store.replace(&record, &bound).unwrap();
        let mut session = caudra_storage::sessions::Session::<Message, (), ()>::new_with_workspace(
            MODEL,
            ".",
            bound.workcell_binding.clone().unwrap(),
        );
        session.save(controller.store.state()).unwrap();
        let _writer =
            caudra_storage::sessions::SessionLease::acquire(controller.store.state(), session.id)
                .unwrap();
        assert!(matches!(
            smol::block_on(controller.action(&name(), LifecycleAction::Pause)),
            Err(Error::Busy)
        ));
    }

    #[test]
    fn missing_selection_never_allocates() {
        let temp = tempdir();
        let controller = Controller::new(&StateDir::from_path(temp.path().to_path_buf())).unwrap();
        assert!(matches!(
            smol::block_on(controller.prepare_attach(&name(), ResumePolicy::Refuse)),
            Err(Error::Missing)
        ));
        assert!(matches!(
            smol::block_on(controller.action(
                &name(),
                LifecycleAction::Extend {
                    lease_seconds: LeaseSeconds::NO_EXPIRY
                }
            )),
            Err(Error::Missing)
        ));
        assert!(controller.snapshots().unwrap().is_empty());
    }

    #[test]
    fn paused_attach_never_resumes_or_acquires_credentials_without_confirmation() {
        let server = Server::new(|method, path, _, headers| {
            assert!(!path.ends_with("/resume") && !path.ends_with("/credentials"));
            let (status, mut response) = regular(method, path, headers).unwrap();
            if response.get("instance").is_some() {
                response["instance"]["state"] = json!("paused");
                response["instance"]["leaseDeadline"] = Value::Null;
                response["instance"]["retention"]["deadline"] = json!(LATER);
            }
            Some((status, response))
        });
        let (_temp, controller, saved) = setup(&server);
        create_ready(&controller, &saved);
        assert!(matches!(
            smol::block_on(controller.prepare_attach(&name(), ResumePolicy::Refuse)),
            Err(Error::ResumeRequired)
        ));
        assert_eq!(
            controller
                .store
                .get(&name())
                .unwrap()
                .instance
                .unwrap()
                .retention
                .deadline
                .as_deref(),
            Some(LATER)
        );
    }

    #[test]
    fn owner_rotation_refuses_even_operation_lookup() {
        let rotated = Arc::new(AtomicBool::new(false));
        let server_rotated = rotated.clone();
        let server = Server::new(move |method, path, _, headers| {
            if server_rotated.load(Ordering::Acquire) {
                assert!(path.ends_with("/discover"));
                Some((200, discovery("different-owner")))
            } else {
                regular(method, path, headers)
            }
        });
        let (_temp, controller, saved) = setup(&server);
        create_ready(&controller, &saved);
        rotated.store(true, Ordering::Release);
        assert!(matches!(
            smol::block_on(controller.inspect(&name())),
            Err(Error::Identity)
        ));
    }

    fn pending_reservation(binding: StoredWorkspaceBinding) -> RemoteOperationReservation {
        RemoteOperationReservation {
            operation_id: OperationId::new("pending-operation").unwrap(),
            invocation_id: OperationId::new("pending-invocation").unwrap(),
            preparation_id: OperationId::new("pending-preparation").unwrap(),
            binding,
            operation_kind: "test".into(),
            request_digest: RequestDigest::sha256(DIGEST).unwrap(),
            created_at: 1,
            publication_cwd: None,
            publication_id: None,
            host_instance_id: "test-instance".to_owned(),
        }
    }

    #[test]
    fn unresolved_remote_mutations_block_disk_controls_but_allow_recovery_resume() {
        let server = Server::new(|method, path, _, headers| {
            if path.ends_with("/resume") {
                let mut resumed = instance();
                resumed["revision"] = json!(2);
                resumed["executionID"] = json!("resumed-execution");
                return Some((200, resumed));
            }
            let (status, mut response) = regular(method, path, headers).unwrap();
            if response.get("instance").is_some() {
                response["instance"]["state"] = json!("paused");
                response["instance"]["leaseDeadline"] = Value::Null;
            }
            Some((status, response))
        });
        let (_temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let binding = binding(&record, "");
        let mut bound = record.clone();
        bound.workcell_binding = Some(binding.clone());
        controller.store.replace(&record, &bound).unwrap();
        RemoteOperationJournal::open(controller.store.state())
            .unwrap()
            .reserve_before_send(&pending_reservation(binding))
            .unwrap();
        assert!(matches!(
            smol::block_on(controller.action(
                &name(),
                LifecycleAction::Delete {
                    destroy_borrowed: false
                }
            )),
            Err(Error::Busy)
        ));
        let resumed = smol::block_on(controller.action(
            &name(),
            LifecycleAction::Resume {
                lease_seconds: LEASE,
            },
        ))
        .unwrap();
        assert_eq!(resumed.instance.unwrap().state, InstanceState::Running);
    }

    /// An operation recorded against an earlier workspace generation can never
    /// be reconciled, so it must not hold back the disk controls for good.
    #[test_case("", true; "current_generation_blocks")]
    #[test_case("generation", false; "earlier_generation_does_not_block")]
    fn only_a_reconcilable_remote_mutation_blocks_disk_controls(recorded: &str, blocks: bool) {
        let server = Server::new(|method, path, _, headers| regular(method, path, headers));
        let (_temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let mut bound = record.clone();
        bound.workcell_binding = Some(binding(&record, ""));
        controller.store.replace(&record, &bound).unwrap();
        RemoteOperationJournal::open(controller.store.state())
            .unwrap()
            .reserve_before_send(&pending_reservation(binding(&record, recorded)))
            .unwrap();
        match controller.require_resolved_mutations(&bound) {
            Err(Error::Busy) => assert!(blocks),
            Ok(()) => assert!(!blocks),
            Err(other) => panic!("{other:?}"),
        }
    }

    #[test]
    fn lost_delete_recovers_from_operation_tombstone_without_replaying_delete() {
        let mut deleted = false;
        let server = Server::new(move |method, path, _, headers| {
            if method == "DELETE" {
                assert!(!deleted);
                deleted = true;
                return None;
            }
            let (status, mut response) = regular(method, path, headers).unwrap();
            if deleted && !path.ends_with("/discover") {
                assert!(path.starts_with("/daemon/v1/operations/"));
                response["status"] = json!("deleted");
                response["instance"]["state"] = json!("deleted");
                response["instance"]["revision"] = json!(2);
                response["instance"]["leaseDeadline"] = Value::Null;
            }
            Some((status, response))
        });
        let (_temp, controller, saved) = setup(&server);
        create_ready(&controller, &saved);
        assert!(
            smol::block_on(controller.action(
                &name(),
                LifecycleAction::Delete {
                    destroy_borrowed: false
                }
            ))
            .is_err()
        );
        let recovered = smol::block_on(controller.inspect(&name())).unwrap();
        assert_eq!(recovered.instance.unwrap().state, InstanceState::Deleted);
        assert_eq!(recovered.lifecycle.unwrap().observed_revision, Some(2));
    }

    #[test]
    fn template_digest_and_resource_drift_are_not_accepted() {
        let server = Server::new(|method, path, _, headers| regular(method, path, headers));
        let (_temp, controller, saved) = setup(&server);
        let record = create_ready(&controller, &saved);
        let mut changed = record.instance.clone().unwrap();
        changed.template.image_identity =
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        assert!(matches!(
            validate_instance(&record, &changed),
            Err(Error::Identity)
        ));
        changed = record.instance.clone().unwrap();
        changed.resources.cpu_count += 1;
        assert!(matches!(
            validate_instance(&record, &changed),
            Err(Error::Identity)
        ));
    }
}
