use caudra_config::sandbox::{
    LeaseSeconds, ResolvedLaunch, Revision, SandboxName, SandboxProvider,
};
use caudra_storage::{
    StateDir,
    id::CaudraId,
    private_file::{FileRevision, PrivateFile, PrivateFileError},
    workspace_binding::StoredWorkspaceBinding,
};
use caudra_workspace::WorkspacePath;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs::File};

use crate::{
    Error, Result,
    dto::{Create, Expected, Instance, Operation, Policy, Template, timestamp},
};

const STORE_VERSION: u32 = 1;
const MAX_STORE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RECORDS: usize = 512;
const STORE_FILE: &str = "sandboxes/instances.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ownership {
    Owned,
    Borrowed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateIntent {
    pub key: String,
    pub request: Create,
    pub operation: Option<Operation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleIntent {
    pub action: String,
    pub expected: Expected,
    #[serde(deserialize_with = "Option::deserialize")]
    pub lease_seconds: Option<LeaseSeconds>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub observed_revision: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub policy: Option<Policy>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub policy_revision: Option<String>,
    /// Present exactly when a resume or renew asks for a finite lease.
    #[serde(deserialize_with = "Option::deserialize")]
    pub minimum_lease_deadline: Option<String>,
    pub allow_equal_revision: bool,
    pub failure_acknowledged: bool,
}

impl LifecycleIntent {
    fn validate(&self) -> Result<()> {
        let policy = self.action == "policy";
        let lease = matches!(self.action.as_str(), "resume" | "renew");
        if !matches!(
            self.action.as_str(),
            "pause" | "delete" | "resume" | "renew" | "policy" | "cancel-create"
        ) || self.policy.is_some() != policy
            || self.policy_revision.is_some() != policy
            || self.lease_seconds.is_some() != lease
            || self.minimum_lease_deadline.is_some()
                != self.lease_seconds.and_then(LeaseSeconds::finite).is_some()
        {
            return Err(Error::Store);
        }
        if let Some(policy) = &self.policy
            && (policy.canonical()? != *policy || Some(policy.revision()?) != self.policy_revision)
        {
            return Err(Error::Store);
        }
        if let Some(deadline) = &self.minimum_lease_deadline {
            timestamp(deadline)?;
        }
        Ok(())
    }

    pub fn is_pending(&self) -> bool {
        self.observed_revision.is_none() && !self.failure_acknowledged
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceRecord {
    pub id: CaudraId,
    pub name: SandboxName,
    pub ownership: Ownership,
    pub provider_name: SandboxName,
    pub provider: SandboxProvider,
    pub owner_id: String,
    pub cwd: WorkspacePath,
    pub launch: Option<ResolvedLaunch>,
    pub template: Template,
    pub create: Option<CreateIntent>,
    pub instance: Option<Instance>,
    pub lifecycle: Option<LifecycleIntent>,
    pub workcell_binding: Option<StoredWorkspaceBinding>,
    pub detached: bool,
}

impl InstanceRecord {
    pub fn lifecycle_failure_review(&self) -> Result<String> {
        let intent = self
            .lifecycle
            .as_ref()
            .filter(|intent| intent.is_pending())
            .ok_or(Error::Unresolved)?;
        serde_json::to_string_pretty(&serde_json::json!({
            "record": self.name,
            "reviewed_revision": self.revision()?,
            "requested_intent": intent,
            "last_observed_instance": self.instance,
            "effect": "Acknowledge FAILURE, not success. No remote request, rollback, policy application or automatic retry. The last observation may be stale; an accepted request may still complete. Reconcile first when possible. Intent and observed policy/state remain recorded."
        })).map_err(|_| Error::Store)
    }

    pub fn revision(&self) -> Result<Revision> {
        let bytes = serde_json::to_vec(self).map_err(|_| Error::Store)?;
        let digest = Sha256::digest(bytes);
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Revision::parse(&format!("sha256:{hex}"))?)
    }
}

#[derive(Serialize, Deserialize)]
struct Document {
    version: u32,
    records: BTreeMap<SandboxName, InstanceRecord>,
}

impl Document {
    fn validate(&self) -> Result<()> {
        if self.version != STORE_VERSION || self.records.len() > MAX_RECORDS {
            return Err(Error::Store);
        }
        for (name, record) in &self.records {
            if name != &record.name {
                return Err(Error::Store);
            }
            if let Some(intent) = &record.lifecycle {
                intent.validate().map_err(|_| Error::Store)?;
            }
        }
        Ok(())
    }
}

pub struct Store {
    file: PrivateFile,
    state: StateDir,
}

/// A process-lifetime local activity barrier. Drop only releases a file descriptor.
pub struct RuntimeLease {
    _file: File,
}

impl Store {
    pub fn open(state: &StateDir) -> Result<Self> {
        Ok(Self {
            file: PrivateFile::new(state.persistent_path().join(STORE_FILE), MAX_STORE_BYTES)?,
            state: state.clone(),
        })
    }

    pub fn state(&self) -> &StateDir {
        &self.state
    }

    fn load(&self) -> Result<(FileRevision, Document)> {
        let snapshot = self.file.load()?;
        let document = match snapshot.data {
            None => Document {
                version: STORE_VERSION,
                records: BTreeMap::new(),
            },
            Some(bytes) => serde_json::from_slice::<Document>(&bytes).map_err(|_| Error::Store)?,
        };
        document.validate()?;
        Ok((snapshot.revision, document))
    }

    pub fn list(&self) -> Result<Vec<InstanceRecord>> {
        Ok(self.load()?.1.records.into_values().collect())
    }

    pub fn get(&self, name: &SandboxName) -> Result<InstanceRecord> {
        self.load()?.1.records.remove(name).ok_or(Error::Missing)
    }

    pub fn by_id(&self, id: CaudraId) -> Result<InstanceRecord> {
        self.list()?
            .into_iter()
            .find(|record| record.id == id)
            .ok_or(Error::Missing)
    }

    pub(crate) fn reserve(&self, record: InstanceRecord) -> Result<()> {
        let (revision, mut document) = self.load()?;
        if document.records.contains_key(&record.name) {
            return Err(Error::Exists);
        }
        if document.records.len() >= MAX_RECORDS {
            return Err(Error::Store);
        }
        document.records.insert(record.name.clone(), record);
        self.publish(&revision, &document)
    }

    pub(crate) fn replace(&self, previous: &InstanceRecord, next: &InstanceRecord) -> Result<()> {
        let (revision, mut document) = self.load()?;
        let current = document.records.get(&previous.name).ok_or(Error::Missing)?;
        if serde_json::to_vec(current).map_err(|_| Error::Store)?
            != serde_json::to_vec(previous).map_err(|_| Error::Store)?
            || next.id != previous.id
            || next.name != previous.name
        {
            return Err(Error::PrivateFile(PrivateFileError::Conflict));
        }
        document.records.insert(next.name.clone(), next.clone());
        self.publish(&revision, &document)
    }

    fn publish(&self, revision: &FileRevision, document: &Document) -> Result<()> {
        document.validate()?;
        let bytes = serde_json::to_vec(document).map_err(|_| Error::Store)?;
        self.file.compare_exchange(revision, Some(&bytes))?;
        Ok(())
    }

    pub(crate) fn lease(&self, record: &InstanceRecord, exclusive: bool) -> Result<RuntimeLease> {
        self.resource_lease(record, "runtime", exclusive)
    }

    pub(crate) fn control_lease(&self, record: &InstanceRecord) -> Result<RuntimeLease> {
        self.resource_lease(record, "control", true)
    }

    fn resource_lease(
        &self,
        record: &InstanceRecord,
        purpose: &str,
        exclusive: bool,
    ) -> Result<RuntimeLease> {
        let sandbox_id = record
            .instance
            .as_ref()
            .map(|instance| &instance.sandbox_id)
            .or_else(|| {
                record
                    .create
                    .as_ref()?
                    .operation
                    .as_ref()
                    .map(|operation| &operation.sandbox_id)
            })
            .ok_or(Error::NotReady)?;
        let scope =
            serde_json::to_vec(&(&record.provider.api_endpoint, &record.owner_id, sandbox_id))
                .map_err(|_| Error::Store)?;
        let digest = Sha256::digest(scope);
        let scope: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let file = PrivateFile::new(
            self.state
                .persistent_path()
                .join("sandboxes/leases")
                .join(format!("{scope}.{purpose}")),
            0,
        )?;
        let file = file.try_lease(exclusive).map_err(|error| match error {
            PrivateFileError::Busy => Error::Busy,
            other => other.into(),
        })?;
        Ok(RuntimeLease { _file: file })
    }
}
