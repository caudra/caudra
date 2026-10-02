use std::collections::HashSet;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::id::CaudraId;
use crate::sessions::{
    SESSIONS_DB_FILE, SessionDatabase, SessionError, SessionMeta, from_i64, to_i64,
};
use crate::{StateDir, StorageError, now_epoch};

use super::{
    PERMISSION_RULES, PermissionRuleRecord, PermissionStateError, sha256_hex, validate_record,
};
use crate::state::SCOPE_GLOBAL;

pub const PERMISSION_RECEIPT_LIMIT: usize = 1024;
const MAX_MUTATION_BYTES: usize = 8 * 1024 * 1024;
const MAX_MUTATION_OWNERS: usize = 2;
const INVALID_MUTATION: &str = "invalid permission mutation; prepare and review a fresh operation";
const INVALID_RECEIPT_ID: &str = "receipt identity does not match its operation";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionOwner {
    Persistent,
    Conversation(CaudraId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRecordIdentity {
    pub owner: PermissionOwner,
    pub record_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionGeneration {
    pub store_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRevision {
    pub owner: PermissionOwner,
    pub generation: u64,
    pub row_present: bool,
    pub lineage: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionSnapshot {
    pub store_id: String,
    pub revision: PermissionRevision,
    pub records: Vec<PermissionRuleRecord>,
    pub source_fingerprint: String,
}

impl PermissionSnapshot {
    pub fn apply_to_meta(
        &self,
        session_id: CaudraId,
        meta: &mut SessionMeta,
    ) -> Result<(), PermissionMutationError> {
        if self.revision.owner != PermissionOwner::Conversation(session_id)
            || !self.revision.row_present
        {
            return Err(PermissionMutationError::InvalidOperation);
        }
        meta.structured_permission_rules.clone_from(&self.records);
        meta.permission_generation = self.revision.generation;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PermissionMutation {
    Create {
        destination: PermissionOwner,
        records: Box<[PermissionRuleRecord]>,
    },
    Replace {
        source: PermissionRecordIdentity,
        destination: PermissionOwner,
        replacement: Box<PermissionRuleRecord>,
    },
    Revoke {
        source: PermissionRecordIdentity,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreparedPermissionMutation {
    operation_id: CaudraId,
    expected: Vec<PermissionSnapshot>,
    targets: Vec<PermissionSnapshot>,
}

impl PreparedPermissionMutation {
    pub fn operation_id(&self) -> CaudraId {
        self.operation_id
    }

    pub fn expected(&self) -> &[PermissionSnapshot] {
        &self.expected
    }

    pub fn targets(&self) -> &[PermissionSnapshot] {
        &self.targets
    }

    pub fn persistent_only(&self) -> bool {
        self.expected
            .iter()
            .all(|snapshot| snapshot.revision.owner == PermissionOwner::Persistent)
    }

    pub fn committed_snapshots(
        &self,
        receipt: &PermissionCommitReceipt,
    ) -> Result<Vec<PermissionSnapshot>, PermissionMutationError> {
        if receipt.operation_id != self.operation_id
            || receipt.revisions.len() != self.targets.len()
        {
            return Err(PermissionMutationError::InvalidOperation);
        }
        self.targets
            .iter()
            .zip(&receipt.revisions)
            .map(|(target, revision)| {
                if target.store_id != receipt.committed.store_id
                    || target.revision.owner != revision.owner
                {
                    return Err(PermissionMutationError::InvalidOperation);
                }
                let mut snapshot = target.clone();
                snapshot.revision = revision.clone();
                snapshot.source_fingerprint = sha256_hex(&serde_json::to_vec(&snapshot.records)?);
                Ok(snapshot)
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionCommitReceipt {
    pub operation_id: CaudraId,
    pub committed: PermissionGeneration,
    pub revisions: Vec<PermissionRevision>,
}

#[derive(Debug, thiserror::Error)]
pub enum PermissionMutationError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    State(#[from] PermissionStateError),
    #[error("permission state changed for {owner:?}; prepare and review a fresh operation")]
    Conflict { owner: PermissionOwner },
    #[error(
        "atomic permission moves require the same database; use separate Copy and Revoke actions"
    )]
    DifferentDatabase,
    #[error("{INVALID_MUTATION}")]
    InvalidOperation,
    #[error("permission operation identity was already used for a different mutation")]
    OperationIdReused,
}

impl From<rusqlite::Error> for PermissionMutationError {
    fn from(error: rusqlite::Error) -> Self {
        SessionError::from(error).into()
    }
}

impl From<serde_json::Error> for PermissionMutationError {
    fn from(error: serde_json::Error) -> Self {
        SessionError::from(StorageError::from(error)).into()
    }
}

pub fn prepare_mutation(
    expected: Vec<PermissionSnapshot>,
    mutation: PermissionMutation,
) -> Result<PreparedPermissionMutation, PermissionMutationError> {
    prepare_mutations(expected, vec![mutation])
}

/// Several operations that commit together or not at all, applied in order.
pub fn prepare_mutations(
    expected: Vec<PermissionSnapshot>,
    mutations: Vec<PermissionMutation>,
) -> Result<PreparedPermissionMutation, PermissionMutationError> {
    if mutations.is_empty() || expected.is_empty() || expected.len() > MAX_MUTATION_OWNERS {
        return Err(PermissionMutationError::InvalidOperation);
    }
    let store = &expected[0].store_id;
    if expected.iter().any(|snapshot| &snapshot.store_id != store) {
        return Err(PermissionMutationError::DifferentDatabase);
    }
    let mut owners = HashSet::new();
    for snapshot in &expected {
        validate_snapshot(snapshot)?;
        if !owners.insert(snapshot.revision.owner.clone()) {
            return Err(PermissionMutationError::InvalidOperation);
        }
    }
    let mut targets = expected.clone();
    let mut used = HashSet::new();
    for mutation in mutations {
        apply(&mut targets, &mut used, mutation)?;
    }
    targets.retain(|snapshot| used.contains(&snapshot.revision.owner));
    for snapshot in &mut targets {
        snapshot.revision.row_present = true;
        validate_snapshot(snapshot)?;
    }
    let prepared = PreparedPermissionMutation {
        operation_id: CaudraId::generate(),
        expected,
        targets,
    };
    bounded_json(&prepared)?;
    Ok(prepared)
}

fn apply(
    targets: &mut [PermissionSnapshot],
    used: &mut HashSet<PermissionOwner>,
    mutation: PermissionMutation,
) -> Result<(), PermissionMutationError> {
    match mutation {
        PermissionMutation::Create {
            destination,
            records,
        } => {
            if records.is_empty()
                || records
                    .iter()
                    .any(|record| !record.is_active() || record.replaces.is_some())
            {
                return Err(PermissionMutationError::InvalidOperation);
            }
            target(targets, &destination)?.records.extend(records);
            used.insert(destination);
        }
        PermissionMutation::Replace {
            source,
            destination,
            mut replacement,
        } => {
            if !replacement.is_active()
                || replacement.id == source.record_id
                || replacement.replaces.is_some()
            {
                return Err(PermissionMutationError::InvalidOperation);
            }
            retire(targets, &source)?;
            replacement.replaces = Some(source.clone());
            target(targets, &destination)?.records.push(*replacement);
            used.insert(source.owner);
            used.insert(destination);
        }
        PermissionMutation::Revoke { source } => {
            retire(targets, &source)?;
            used.insert(source.owner);
        }
    }
    Ok(())
}

fn target<'a>(
    targets: &'a mut [PermissionSnapshot],
    owner: &PermissionOwner,
) -> Result<&'a mut PermissionSnapshot, PermissionMutationError> {
    let target = targets
        .iter_mut()
        .find(|target| &target.revision.owner == owner)
        .ok_or(PermissionMutationError::InvalidOperation)?;
    if !target.revision.row_present && !matches!(owner, PermissionOwner::Persistent) {
        return Err(PermissionMutationError::Conflict {
            owner: owner.clone(),
        });
    }
    Ok(target)
}

fn retire(
    targets: &mut [PermissionSnapshot],
    source: &PermissionRecordIdentity,
) -> Result<(), PermissionMutationError> {
    let record = target(targets, &source.owner)?
        .records
        .iter_mut()
        .find(|record| record.id == source.record_id && record.is_active())
        .ok_or_else(|| PermissionMutationError::Conflict {
            owner: source.owner.clone(),
        })?;
    record.revoked_at = Some(now_epoch().max(record.created_at));
    Ok(())
}

fn validate_snapshot(snapshot: &PermissionSnapshot) -> Result<(), PermissionMutationError> {
    if !snapshot.revision.row_present && !snapshot.records.is_empty() {
        return Err(PermissionMutationError::InvalidOperation);
    }
    let mut ids = HashSet::new();
    for record in &snapshot.records {
        validate_record(
            record,
            snapshot.revision.owner == PermissionOwner::Persistent,
        )?;
        if !ids.insert(&record.id) {
            return Err(PermissionMutationError::InvalidOperation);
        }
    }
    Ok(())
}

fn bounded_json(value: &impl Serialize) -> Result<String, PermissionMutationError> {
    let encoded = serde_json::to_string(value)?;
    SessionDatabase::validate_len("permission mutation", encoded.len(), MAX_MUTATION_BYTES)?;
    Ok(encoded)
}

impl SessionDatabase {
    pub fn permission_generation(&self) -> Result<PermissionGeneration, PermissionMutationError> {
        generation_on(self.connection())
    }

    pub fn permission_snapshot(
        &self,
        owner: PermissionOwner,
    ) -> Result<PermissionSnapshot, PermissionMutationError> {
        let transaction = self.connection().unchecked_transaction()?;
        let snapshot = snapshot_on(&transaction, owner)?;
        transaction.commit()?;
        Ok(snapshot)
    }

    pub fn permission_snapshots(
        &self,
        owners: &[PermissionOwner],
    ) -> Result<Vec<PermissionSnapshot>, PermissionMutationError> {
        if owners.is_empty() || owners.len() > MAX_MUTATION_OWNERS {
            return Err(PermissionMutationError::InvalidOperation);
        }
        let transaction = self.connection().unchecked_transaction()?;
        let snapshots = owners
            .iter()
            .cloned()
            .map(|owner| snapshot_on(&transaction, owner))
            .collect::<Result<_, _>>()?;
        transaction.commit()?;
        Ok(snapshots)
    }

    pub fn permission_receipt(
        &self,
        operation_id: CaudraId,
    ) -> Result<Option<PermissionCommitReceipt>, PermissionMutationError> {
        receipt_on(self.connection(), operation_id)
            .map(|receipt| receipt.map(|(_, receipt)| receipt))
    }

    pub fn commit_permission_mutation(
        &self,
        prepared: &PreparedPermissionMutation,
    ) -> Result<PermissionCommitReceipt, PermissionMutationError> {
        if !prepared.persistent_only()
            && prepared
                .expected
                .iter()
                .any(|snapshot| snapshot.revision.owner == PermissionOwner::Persistent)
            && !permission_databases_shared(self.state_directory())?
        {
            return Err(PermissionMutationError::DifferentDatabase);
        }
        let fingerprint = sha256_hex(bounded_json(prepared)?.as_bytes());
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        let store_id = generation_on(&transaction)?.store_id;
        if prepared
            .expected
            .iter()
            .any(|snapshot| snapshot.store_id != store_id)
        {
            return Err(PermissionMutationError::DifferentDatabase);
        }
        if let Some((stored, receipt)) = receipt_on(&transaction, prepared.operation_id)? {
            return if stored == fingerprint {
                Ok(receipt)
            } else {
                Err(PermissionMutationError::OperationIdReused)
            };
        }
        for expected in &prepared.expected {
            let current = snapshot_on(&transaction, expected.revision.owner.clone())?;
            if current.store_id != expected.store_id {
                return Err(PermissionMutationError::DifferentDatabase);
            }
            if &current != expected {
                return Err(PermissionMutationError::Conflict {
                    owner: expected.revision.owner.clone(),
                });
            }
        }
        for target in &prepared.targets {
            validate_snapshot(target)?;
            write_target(&transaction, target)?;
        }
        let receipt = PermissionCommitReceipt {
            operation_id: prepared.operation_id,
            committed: generation_on(&transaction)?,
            revisions: prepared
                .targets
                .iter()
                .map(|target| {
                    snapshot_on(&transaction, target.revision.owner.clone())
                        .map(|snapshot| snapshot.revision)
                })
                .collect::<Result<_, _>>()?,
        };
        transaction.execute(
            "INSERT INTO permission_receipts (operation_id, fingerprint, receipt) VALUES (?1, ?2, ?3)",
            params![prepared.operation_id.as_bytes().as_slice(), fingerprint, bounded_json(&receipt)?],
        )?;
        transaction.execute(
            "DELETE FROM permission_receipts WHERE sequence <= (SELECT sequence FROM permission_receipts ORDER BY sequence DESC LIMIT 1 OFFSET ?1)",
            params![to_i64(PERMISSION_RECEIPT_LIMIT, "permission receipt limit")?],
        )?;
        transaction.commit()?;
        Ok(receipt)
    }
}

fn generation_on(connection: &Connection) -> Result<PermissionGeneration, PermissionMutationError> {
    let (mut store_id, generation) = connection.query_row(
        "SELECT store_id, generation FROM permission_clock WHERE singleton = 1",
        [],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let generation = from_i64(generation, "permission_clock.generation")?;
    let path = connection
        .path()
        .ok_or(PermissionMutationError::InvalidOperation)?;
    store_id.push_str(&physical_identity(Path::new(path))?);
    Ok(PermissionGeneration {
        store_id,
        generation,
    })
}

pub fn permission_databases_shared(state_dir: &StateDir) -> Result<bool, PermissionMutationError> {
    let conversation = state_dir.path().join(SESSIONS_DB_FILE);
    let persistent = state_dir.persistent_path().join(SESSIONS_DB_FILE);
    if conversation == persistent {
        return Ok(true);
    }
    Ok(physical_identity(&conversation)? == physical_identity(&persistent)?)
}

fn physical_identity(path: &Path) -> Result<String, PermissionMutationError> {
    let canonical = fs::canonicalize(path)
        .map_err(StorageError::from)
        .map_err(SessionError::from)?;
    let path_digest = sha256_hex(canonical.as_os_str().as_encoded_bytes());
    #[cfg(unix)]
    {
        let metadata = fs::metadata(&canonical)
            .map_err(StorageError::from)
            .map_err(SessionError::from)?;
        Ok(format!(
            ":{}:{}:{path_digest}",
            metadata.dev(),
            metadata.ino()
        ))
    }
    #[cfg(not(unix))]
    {
        Ok(format!(":{path_digest}"))
    }
}

pub(crate) fn snapshot_on(
    connection: &Connection,
    owner: PermissionOwner,
) -> Result<PermissionSnapshot, PermissionMutationError> {
    let store_id = generation_on(connection)?.store_id;
    let (generation, lineage, encoded) = match &owner {
        PermissionOwner::Persistent => {
            let generation = connection.query_row(
                "SELECT persistent_generation FROM permission_clock WHERE singleton = 1",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            let generation = from_i64(generation, "permission_clock.persistent_generation")?;
            let records = connection
                .query_row(
                    "SELECT value FROM state WHERE scope = ?1 AND key = ?2",
                    params![SCOPE_GLOBAL, PERMISSION_RULES.name],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            (generation, None, records)
        }
        PermissionOwner::Conversation(id) => {
            let row = connection.query_row("SELECT permission_generation, permission_lineage, coalesce(metadata -> '$.structured_permission_rules', '[]') FROM sessions WHERE id = ?1", params![id.as_bytes().as_slice()], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))).optional()?;
            match row {
                Some((generation, lineage, records)) => (
                    from_i64(generation, "sessions.permission_generation")?,
                    Some(lineage),
                    Some(records),
                ),
                None => (0, None, None),
            }
        }
    };
    let snapshot = PermissionSnapshot {
        store_id,
        revision: PermissionRevision {
            owner,
            generation,
            row_present: encoded.is_some(),
            lineage,
        },
        source_fingerprint: sha256_hex(encoded.as_deref().unwrap_or_default().as_bytes()),
        records: encoded
            .map(|encoded| serde_json::from_str(&encoded))
            .transpose()?
            .unwrap_or_default(),
    };
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn write_target(
    transaction: &Transaction<'_>,
    target: &PermissionSnapshot,
) -> Result<(), PermissionMutationError> {
    let records = bounded_json(&target.records)?;
    SessionDatabase::validate_payload_json("permission rules", &records)?;
    match &target.revision.owner {
        PermissionOwner::Persistent => {
            transaction.execute("INSERT INTO state (scope, key, value, updated_at) VALUES (?1, ?2, ?3, unixepoch()) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at", params![SCOPE_GLOBAL, PERMISSION_RULES.name, records])?;
        }
        PermissionOwner::Conversation(id) => {
            let metadata: String = transaction.query_row("SELECT json_set(metadata, '$.structured_permission_rules', json(?2)) FROM sessions WHERE id = ?1", params![id.as_bytes().as_slice(), records], |row| row.get(0))?;
            SessionDatabase::validate_payload_json("session metadata", &metadata)?;
            SessionDatabase::validate_len("session metadata", metadata.len(), MAX_MUTATION_BYTES)?;
            let changed = transaction.execute("UPDATE sessions SET metadata = ?2, logical_bytes = logical_bytes + length(CAST(?2 AS BLOB)) - length(CAST(metadata AS BLOB)) WHERE id = ?1", params![id.as_bytes().as_slice(), metadata])?;
            if changed != 1 {
                return Err(PermissionMutationError::Conflict {
                    owner: target.revision.owner.clone(),
                });
            }
        }
    }
    Ok(())
}

fn receipt_on(
    connection: &Connection,
    operation_id: CaudraId,
) -> Result<Option<(String, PermissionCommitReceipt)>, PermissionMutationError> {
    connection
        .query_row(
            "SELECT fingerprint, receipt FROM permission_receipts WHERE operation_id = ?1",
            params![operation_id.as_bytes().as_slice()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .map(|(fingerprint, receipt)| {
            let receipt: PermissionCommitReceipt = serde_json::from_str(&receipt)?;
            if receipt.operation_id != operation_id {
                return Err(SessionError::CorruptDatabaseValue {
                    field: "permission_receipts.receipt",
                    reason: INVALID_RECEIPT_ID.into(),
                }
                .into());
            }
            Ok((fingerprint, receipt))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use rusqlite::params;
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        INVALID_RECEIPT_ID, PERMISSION_RECEIPT_LIMIT, PermissionMutation, PermissionMutationError,
        PermissionOwner, PermissionRecordIdentity, PreparedPermissionMutation,
        permission_databases_shared, prepare_mutation, prepare_mutations,
    };
    use crate::id::CaudraId;
    use crate::permission_state::{
        PERMISSION_LABEL_MAX_BYTES, PERMISSION_RULES, PermissionArgumentConstraint,
        PermissionExecutorKind, PermissionLifetime, PermissionRuleRecord, PermissionState,
        PermissionSubject, StructuredPermissionEffect, StructuredPermissionRule, sha256_hex,
    };
    use crate::sessions::{
        Session, SessionDatabase, SessionError, StoredSubagentTaskSpec, TitleSource, to_i64,
    };
    use crate::state::SCOPE_GLOBAL;
    use crate::{StateDir, now_epoch};

    const MODEL: &str = "permission-test";
    const PROJECT: &str = "/permission-project";
    const INITIAL_MESSAGE: &str = "before edit";
    const LATE_MESSAGE: &str = "chat written after edit";
    const LABEL: &str = "Reviewed display label";
    const FAILURE: &str = "injected permission mutation failure";
    const PERMISSION_FIELD: &str = "structured_permission_rules";
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const NEGATIVE_GENERATION: i64 = -1;
    const NEGATIVE_GENERATION_REASON: &str = "-1";
    const SESSION_GENERATION_FIELD: &str = "sessions.permission_generation";
    const DATABASE_GENERATION_FIELD: &str = "permission_clock.generation";
    const PERSISTENT_GENERATION_FIELD: &str = "permission_clock.persistent_generation";
    const TOOL_ID: &str = "permission-test-tool";
    const SUBAGENT_ID: &str = "permission-test-subagent";

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Message(String);

    impl TitleSource for Message {
        fn first_user_text(&self) -> Option<&str> {
            Some(&self.0)
        }
    }

    type TestSession = Session<Message, Value, Value>;

    fn database() -> (TempDir, StateDir, SessionDatabase, TestSession) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let mut database = SessionDatabase::open(&dir).unwrap();
        let mut session = TestSession::new(MODEL, PROJECT);
        session.push_message(Message(INITIAL_MESSAGE.into()));
        database.save(&session, None).unwrap();
        (temp, dir, database, session)
    }

    fn record(
        lifetime: PermissionLifetime,
        effect: StructuredPermissionEffect,
    ) -> PermissionRuleRecord {
        PermissionRuleRecord {
            id: CaudraId::generate().to_string(),
            project: (lifetime == PermissionLifetime::Project).then(|| PathBuf::from(PROJECT)),
            rule: StructuredPermissionRule {
                subject: PermissionSubject::Native {
                    owner: "workcell".into(),
                    contract: "file.read.v1".into(),
                },
                executor: PermissionExecutorKind::Native,
                resources: Vec::new(),
                arguments: PermissionArgumentConstraint::Unconstrained,
                lifetime,
                effect,
                family: None,
            },
            review: None,
            label: Some(LABEL.into()),
            replaces: None,
            created_at: now_epoch(),
            revoked_at: None,
        }
    }

    fn owner(lifetime: &PermissionLifetime, session: &TestSession) -> PermissionOwner {
        if *lifetime == PermissionLifetime::Conversation {
            PermissionOwner::Conversation(session.id)
        } else {
            PermissionOwner::Persistent
        }
    }

    fn create(
        database: &SessionDatabase,
        owner: PermissionOwner,
        record: PermissionRuleRecord,
    ) -> PreparedPermissionMutation {
        let prepared = prepare_mutation(
            vec![database.permission_snapshot(owner.clone()).unwrap()],
            PermissionMutation::Create {
                destination: owner,
                records: Box::new([record]),
            },
        )
        .unwrap();
        database.commit_permission_mutation(&prepared).unwrap();
        prepared
    }

    fn replacement(
        database: &SessionDatabase,
        source: PermissionOwner,
        destination: PermissionOwner,
        original: &PermissionRuleRecord,
        successor: PermissionRuleRecord,
    ) -> PreparedPermissionMutation {
        let mut owners = vec![source.clone()];
        if destination != source {
            owners.push(destination.clone());
        }
        prepare_mutation(
            database.permission_snapshots(&owners).unwrap(),
            PermissionMutation::Replace {
                source: PermissionRecordIdentity {
                    owner: source,
                    record_id: original.id.clone(),
                },
                destination,
                replacement: Box::new(successor),
            },
        )
        .unwrap()
    }

    fn assert_accounting(database: &SessionDatabase, session: &TestSession) {
        let exact: bool = database.connection().query_row(
            "SELECT logical_bytes = length(CAST(token_usage AS BLOB)) + length(CAST(metadata AS BLOB)) + length(CAST(workspace_binding AS BLOB)) + (SELECT coalesce(sum(byte_count), 0) FROM main_history_items WHERE session_id = ?1) + (SELECT coalesce(sum(byte_count), 0) FROM tool_outputs WHERE session_id = ?1) + (SELECT coalesce(sum(byte_count), 0) FROM subagent_history_items WHERE session_id = ?1) + (SELECT coalesce(sum(length(CAST(task_spec AS BLOB))), 0) FROM subagent_streams WHERE session_id = ?1) FROM sessions WHERE id = ?1",
            params![session.id.as_bytes().as_slice()], |row| row.get(0),
        ).unwrap();
        assert!(exact);
    }

    #[test_case(PermissionLifetime::Conversation, PermissionLifetime::Conversation; "conversation_to_conversation")]
    #[test_case(PermissionLifetime::Conversation, PermissionLifetime::Project; "conversation_to_project")]
    #[test_case(PermissionLifetime::Conversation, PermissionLifetime::Global; "conversation_to_global")]
    #[test_case(PermissionLifetime::Project, PermissionLifetime::Conversation; "project_to_conversation")]
    #[test_case(PermissionLifetime::Project, PermissionLifetime::Project; "project_to_project")]
    #[test_case(PermissionLifetime::Project, PermissionLifetime::Global; "project_to_global")]
    #[test_case(PermissionLifetime::Global, PermissionLifetime::Conversation; "global_to_conversation")]
    #[test_case(PermissionLifetime::Global, PermissionLifetime::Project; "global_to_project")]
    #[test_case(PermissionLifetime::Global, PermissionLifetime::Global; "global_to_global")]
    fn replacements_retire_all_effects_atomically(
        from: PermissionLifetime,
        to: PermissionLifetime,
    ) {
        for effect in [
            StructuredPermissionEffect::Allow,
            StructuredPermissionEffect::Deny,
            StructuredPermissionEffect::Ask,
        ] {
            let (_temp, _dir, database, session) = database();
            let source_owner = owner(&from, &session);
            let destination = owner(&to, &session);
            let original = record(from.clone(), effect);
            create(&database, source_owner.clone(), original.clone());
            let successor = record(to.clone(), StructuredPermissionEffect::Ask);
            let prepared = replacement(
                &database,
                source_owner.clone(),
                destination.clone(),
                &original,
                successor.clone(),
            );
            let before_version = database.write_version(session.id).unwrap();
            let receipt = database.commit_permission_mutation(&prepared).unwrap();
            let source = database.permission_snapshot(source_owner.clone()).unwrap();
            let target = database.permission_snapshot(destination).unwrap();
            let retired = source
                .records
                .iter()
                .find(|record| record.id == original.id)
                .unwrap();
            assert!(!retired.is_active());
            let active = target
                .records
                .iter()
                .find(|record| record.id == successor.id)
                .unwrap();
            assert!(active.is_active());
            assert_eq!(active.rule, successor.rule);
            assert_eq!(
                active.replaces,
                Some(PermissionRecordIdentity {
                    owner: source_owner,
                    record_id: original.id
                })
            );
            assert_eq!(database.write_version(session.id).unwrap(), before_version);
            for snapshot in prepared.committed_snapshots(&receipt).unwrap() {
                assert_eq!(
                    database
                        .permission_snapshot(snapshot.revision.owner.clone())
                        .unwrap(),
                    snapshot
                );
            }
            assert_accounting(&database, &session);
        }
    }

    #[test_case(false, false; "full_replace")]
    #[test_case(true, false; "delta_replace")]
    #[test_case(false, true; "full_revoke")]
    #[test_case(true, true; "delta_revoke")]
    fn approval_then_edit_then_old_snapshot_preserves_new_authority_and_chat(
        delta: bool,
        revoke: bool,
    ) {
        let (_temp, dir, mut database, mut session) = database();
        let conversation = PermissionOwner::Conversation(session.id);
        let original = record(
            PermissionLifetime::Conversation,
            StructuredPermissionEffect::Allow,
        );
        create(&database, conversation.clone(), original.clone());
        database
            .permission_snapshot(conversation.clone())
            .unwrap()
            .apply_to_meta(session.id, &mut session.meta)
            .unwrap();
        session.insert_tool_output(TOOL_ID.into(), json!(INITIAL_MESSAGE));
        session.set_subagent_history(
            SUBAGENT_ID.into(),
            vec![Message(INITIAL_MESSAGE.into())],
            Some(StoredSubagentTaskSpec::generic()),
        );
        let cursor = database.save(&session, None).unwrap();
        let mut late = session.clone();
        late.push_message(Message(LATE_MESSAGE.into()));
        late.meta.input_draft = Some(LATE_MESSAGE.into());
        late.token_usage = json!({"input": 123, "output": 456});
        late.set_subagent_history(
            SUBAGENT_ID.into(),
            vec![
                Message(INITIAL_MESSAGE.into()),
                Message(LATE_MESSAGE.into()),
            ],
            Some(StoredSubagentTaskSpec::generic()),
        );
        let prepared = if revoke {
            prepare_mutation(
                vec![database.permission_snapshot(conversation.clone()).unwrap()],
                PermissionMutation::Revoke {
                    source: PermissionRecordIdentity {
                        owner: conversation.clone(),
                        record_id: original.id.clone(),
                    },
                },
            )
            .unwrap()
        } else {
            replacement(
                &database,
                conversation.clone(),
                conversation.clone(),
                &original,
                record(
                    PermissionLifetime::Conversation,
                    StructuredPermissionEffect::Deny,
                ),
            )
        };
        let external = SessionDatabase::open_state(&dir).unwrap();
        external.commit_permission_mutation(&prepared).unwrap();
        let committed = external.permission_snapshot(conversation.clone()).unwrap();
        database.save(&late, delta.then_some(&cursor)).unwrap();
        let loaded = database.load::<Message, Value, Value>(session.id).unwrap();
        assert_eq!(loaded.messages(), late.messages());
        assert_eq!(loaded.meta.input_draft.as_deref(), Some(LATE_MESSAGE));
        assert_eq!(loaded.meta.structured_permission_rules, committed.records);
        assert_eq!(loaded.token_usage, late.token_usage);
        assert_eq!(loaded.tool_outputs(), late.tool_outputs());
        assert_eq!(loaded.subagent_messages(), late.subagent_messages());
        assert_eq!(loaded.subagent_task_specs(), late.subagent_task_specs());
        assert_eq!(
            loaded.meta.permission_generation,
            committed.revision.generation
        );
        assert_eq!(
            database.permission_snapshot(conversation).unwrap(),
            committed
        );
        assert_accounting(&database, &session);
    }

    #[test_case(StructuredPermissionEffect::Allow; "allow")]
    #[test_case(StructuredPermissionEffect::Deny; "deny")]
    #[test_case(StructuredPermissionEffect::Ask; "ask")]
    fn chat_writes_do_not_conflict_with_permission_cas(effect: StructuredPermissionEffect) {
        let (_temp, _dir, mut database, mut session) = database();
        let conversation = PermissionOwner::Conversation(session.id);
        let original = record(PermissionLifetime::Conversation, effect);
        create(&database, conversation.clone(), original.clone());
        let prepared = replacement(
            &database,
            conversation.clone(),
            conversation.clone(),
            &original,
            record(
                PermissionLifetime::Conversation,
                StructuredPermissionEffect::Ask,
            ),
        );
        session.push_message(Message(LATE_MESSAGE.into()));
        session.meta.turns = 7;
        database.save(&session, None).unwrap();
        assert_eq!(
            database.permission_snapshot(conversation).unwrap(),
            prepared.expected()[0]
        );
        let before = database.write_version(session.id).unwrap();
        database.commit_permission_mutation(&prepared).unwrap();
        assert_eq!(database.write_version(session.id).unwrap(), before);
        let loaded = database.load::<Message, Value, Value>(session.id).unwrap();
        assert_eq!(loaded.messages(), session.messages());
        assert_eq!(loaded.meta.turns, session.meta.turns);
        assert_accounting(&database, &session);
    }

    #[test_case(false; "persistent")]
    #[test_case(true; "conversation")]
    fn competing_edits_and_replays_never_create_a_second_successor(conversation: bool) {
        let (_temp, dir, database, session) = database();
        let lifetime = if conversation {
            PermissionLifetime::Conversation
        } else {
            PermissionLifetime::Global
        };
        let owner = owner(&lifetime, &session);
        let original = record(lifetime.clone(), StructuredPermissionEffect::Deny);
        create(&database, owner.clone(), original.clone());
        let first = replacement(
            &database,
            owner.clone(),
            owner.clone(),
            &original,
            record(lifetime.clone(), StructuredPermissionEffect::Allow),
        );
        let second = replacement(
            &database,
            owner.clone(),
            owner.clone(),
            &original,
            record(lifetime, StructuredPermissionEffect::Ask),
        );
        let receipt = database.commit_permission_mutation(&first).unwrap();
        let external = SessionDatabase::open_state(&dir).unwrap();
        assert!(matches!(
            external.commit_permission_mutation(&second),
            Err(PermissionMutationError::Conflict { .. })
        ));
        assert_eq!(
            external.permission_receipt(first.operation_id()).unwrap(),
            Some(receipt.clone())
        );
        let before = external.permission_snapshot(owner.clone()).unwrap();
        assert_eq!(
            external.commit_permission_mutation(&first).unwrap(),
            receipt
        );
        assert_eq!(external.permission_snapshot(owner).unwrap(), before);
        assert_eq!(
            before
                .records
                .iter()
                .filter(|record| record.is_active())
                .count(),
            1
        );
        assert!(
            external
                .permission_receipt(second.operation_id())
                .unwrap()
                .is_none()
        );
    }

    #[test_case(false; "legacy_approval")]
    #[test_case(true; "legacy_revocation")]
    fn existing_persistent_mutations_advance_the_revision_contract(revoke: bool) {
        let (_temp, dir, database, session) = database();
        let mut state = PermissionState::open(&dir).unwrap();
        let original = state
            .insert(
                None,
                record(
                    PermissionLifetime::Global,
                    StructuredPermissionEffect::Allow,
                )
                .rule,
            )
            .unwrap();
        let prepared = replacement(
            &database,
            PermissionOwner::Persistent,
            PermissionOwner::Conversation(session.id),
            &original,
            record(
                PermissionLifetime::Conversation,
                StructuredPermissionEffect::Allow,
            ),
        );
        let before = state.generation().unwrap();
        if revoke {
            assert!(state.revoke(&original.id).unwrap());
        } else {
            state
                .insert(
                    None,
                    record(PermissionLifetime::Global, StructuredPermissionEffect::Ask).rule,
                )
                .unwrap();
        }
        assert!(state.generation().unwrap().generation > before.generation);
        assert!(matches!(
            database.commit_permission_mutation(&prepared),
            Err(PermissionMutationError::Conflict { .. })
        ));
        assert!(
            database
                .permission_snapshot(PermissionOwner::Conversation(session.id))
                .unwrap()
                .records
                .is_empty()
        );
    }

    #[test_case(false; "persistent_row_deleted")]
    #[test_case(true; "session_row_deleted")]
    fn missing_source_is_a_conflict_not_a_recreation(conversation: bool) {
        let (_temp, _dir, mut database, session) = database();
        let lifetime = if conversation {
            PermissionLifetime::Conversation
        } else {
            PermissionLifetime::Global
        };
        let source = owner(&lifetime, &session);
        let original = record(lifetime.clone(), StructuredPermissionEffect::Allow);
        create(&database, source.clone(), original.clone());
        let prepared = replacement(
            &database,
            source.clone(),
            source.clone(),
            &original,
            record(lifetime, StructuredPermissionEffect::Deny),
        );
        if conversation {
            database.delete(session.id, None).unwrap();
        } else {
            database
                .state_delete(SCOPE_GLOBAL, PERMISSION_RULES.name)
                .unwrap();
        }
        assert!(matches!(
            database.commit_permission_mutation(&prepared),
            Err(PermissionMutationError::Conflict { .. })
        ));
        assert!(
            !database
                .permission_snapshot(source)
                .unwrap()
                .revision
                .row_present
        );
        assert!(
            database
                .permission_receipt(prepared.operation_id())
                .unwrap()
                .is_none()
        );
    }

    #[test_case(false; "empty_row_presence")]
    #[test_case(true; "presence_aba")]
    fn create_checks_empty_row_presence_and_generation(aba: bool) {
        let (_temp, _dir, database, _session) = database();
        let expected = database
            .permission_snapshot(PermissionOwner::Persistent)
            .unwrap();
        let prepared = prepare_mutation(
            vec![expected],
            PermissionMutation::Create {
                destination: PermissionOwner::Persistent,
                records: Box::new([record(
                    PermissionLifetime::Global,
                    StructuredPermissionEffect::Allow,
                )]),
            },
        )
        .unwrap();
        database
            .global_state_set(PERMISSION_RULES.name, &Vec::<PermissionRuleRecord>::new())
            .unwrap();
        if aba {
            database
                .state_delete(SCOPE_GLOBAL, PERMISSION_RULES.name)
                .unwrap();
        }
        assert!(matches!(
            database.commit_permission_mutation(&prepared),
            Err(PermissionMutationError::Conflict { .. })
        ));
    }

    #[test_case(false; "stale_snapshot")]
    #[test_case(true; "stale_generation_forged_to_current")]
    fn complete_record_is_compared_including_label(forge_generation: bool) {
        let (_temp, _dir, database, _session) = database();
        let original = record(
            PermissionLifetime::Global,
            StructuredPermissionEffect::Allow,
        );
        create(&database, PermissionOwner::Persistent, original.clone());
        let mut expected = database
            .permission_snapshot(PermissionOwner::Persistent)
            .unwrap();
        let mut changed = original;
        changed.label = None;
        database
            .global_state_set(PERMISSION_RULES.name, &vec![changed])
            .unwrap();
        if forge_generation {
            expected.revision.generation = database
                .permission_snapshot(PermissionOwner::Persistent)
                .unwrap()
                .revision
                .generation;
        }
        let source = PermissionRecordIdentity {
            owner: PermissionOwner::Persistent,
            record_id: expected.records[0].id.clone(),
        };
        let prepared =
            prepare_mutation(vec![expected], PermissionMutation::Revoke { source }).unwrap();
        assert!(matches!(
            database.commit_permission_mutation(&prepared),
            Err(PermissionMutationError::Conflict { .. })
        ));
    }

    #[test_case("session"; "second_owner_failure")]
    #[test_case("receipt"; "receipt_failure")]
    fn transaction_failure_rolls_back_both_owners_and_clock(fail: &str) {
        let (_temp, _dir, database, session) = database();
        let conversation = PermissionOwner::Conversation(session.id);
        let original = record(
            PermissionLifetime::Conversation,
            StructuredPermissionEffect::Allow,
        );
        create(&database, conversation.clone(), original.clone());
        let expected = database
            .permission_snapshots(&[PermissionOwner::Persistent, conversation.clone()])
            .unwrap();
        let before = database.permission_generation().unwrap();
        let prepared = prepare_mutation(
            expected.clone(),
            PermissionMutation::Replace {
                source: PermissionRecordIdentity {
                    owner: conversation,
                    record_id: original.id,
                },
                destination: PermissionOwner::Persistent,
                replacement: Box::new(record(
                    PermissionLifetime::Global,
                    StructuredPermissionEffect::Allow,
                )),
            },
        )
        .unwrap();
        let trigger = if fail == "session" {
            "BEFORE UPDATE OF metadata ON sessions"
        } else {
            "BEFORE INSERT ON permission_receipts"
        };
        database.connection().execute_batch(&format!("CREATE TRIGGER injected_failure {trigger} BEGIN SELECT RAISE(ABORT, '{FAILURE}'); END;")).unwrap();
        assert!(
            database
                .commit_permission_mutation(&prepared)
                .unwrap_err()
                .to_string()
                .contains(FAILURE)
        );
        assert_eq!(database.permission_generation().unwrap(), before);
        for snapshot in expected {
            assert_eq!(
                database
                    .permission_snapshot(snapshot.revision.owner.clone())
                    .unwrap(),
                snapshot
            );
        }
        assert!(
            database
                .permission_receipt(prepared.operation_id())
                .unwrap()
                .is_none()
        );
        assert_accounting(&database, &session);
    }

    /// One answer can file rules under both owners. They land in one
    /// transaction, so a failure on the second write takes back the first.
    #[test_case(false; "both_owners_commit")]
    #[test_case(true; "a_late_failure_leaves_neither")]
    fn create_spans_both_owners_in_one_transaction(fail: bool) {
        let (_temp, _dir, database, session) = database();
        let conversation = PermissionOwner::Conversation(session.id);
        let expected = database
            .permission_snapshots(&[PermissionOwner::Persistent, conversation.clone()])
            .unwrap();
        let project_rule = record(
            PermissionLifetime::Project,
            StructuredPermissionEffect::Allow,
        );
        let conversation_rule = record(
            PermissionLifetime::Conversation,
            StructuredPermissionEffect::Allow,
        );
        let prepared = prepare_mutations(
            expected.clone(),
            vec![
                PermissionMutation::Create {
                    destination: PermissionOwner::Persistent,
                    records: Box::new([project_rule.clone()]),
                },
                PermissionMutation::Create {
                    destination: conversation.clone(),
                    records: Box::new([conversation_rule.clone()]),
                },
            ],
        )
        .unwrap();
        if fail {
            database.connection().execute_batch(&format!("CREATE TRIGGER injected_failure BEFORE UPDATE OF metadata ON sessions BEGIN SELECT RAISE(ABORT, '{FAILURE}'); END;")).unwrap();
            assert!(
                database
                    .commit_permission_mutation(&prepared)
                    .unwrap_err()
                    .to_string()
                    .contains(FAILURE)
            );
            for snapshot in expected {
                assert_eq!(
                    database
                        .permission_snapshot(snapshot.revision.owner.clone())
                        .unwrap(),
                    snapshot
                );
            }
        } else {
            database.commit_permission_mutation(&prepared).unwrap();
            assert_eq!(
                database
                    .permission_snapshot(PermissionOwner::Persistent)
                    .unwrap()
                    .records,
                [project_rule]
            );
            assert_eq!(
                database.permission_snapshot(conversation).unwrap().records,
                [conversation_rule]
            );
        }
        assert_accounting(&database, &session);
    }

    #[test]
    fn revoke_takes_several_records() {
        let (_temp, _dir, database, _session) = database();
        let rules = [PermissionLifetime::Project, PermissionLifetime::Global]
            .map(|lifetime| record(lifetime, StructuredPermissionEffect::Allow));
        for rule in &rules {
            create(&database, PermissionOwner::Persistent, rule.clone());
        }
        let before = database.permission_generation().unwrap();
        let prepared = prepare_mutations(
            vec![
                database
                    .permission_snapshot(PermissionOwner::Persistent)
                    .unwrap(),
            ],
            rules
                .iter()
                .map(|rule| PermissionMutation::Revoke {
                    source: PermissionRecordIdentity {
                        owner: PermissionOwner::Persistent,
                        record_id: rule.id.clone(),
                    },
                })
                .collect(),
        )
        .unwrap();
        database.commit_permission_mutation(&prepared).unwrap();
        let after = database
            .permission_snapshot(PermissionOwner::Persistent)
            .unwrap();
        assert!(after.records.iter().all(|record| !record.is_active()));
        assert_ne!(database.permission_generation().unwrap(), before);
        assert!(
            database
                .permission_receipt(prepared.operation_id())
                .unwrap()
                .is_some()
        );
    }

    #[test_case(false; "new_rule")]
    #[test_case(true; "replacement")]
    fn metadata_patch_preserves_unknown_fields_accounting_and_cursors(replace: bool) {
        let (_temp, _dir, database, session) = database();
        let conversation = PermissionOwner::Conversation(session.id);
        let original = record(
            PermissionLifetime::Conversation,
            StructuredPermissionEffect::Allow,
        );
        create(&database, conversation.clone(), original.clone());
        database.connection().execute("UPDATE sessions SET metadata = json_set(metadata, '$.future_field', json(?1)), logical_bytes = logical_bytes + length(CAST(json_set(metadata, '$.future_field', json(?1)) AS BLOB)) - length(CAST(metadata AS BLOB)), pinned = 1, last_opened_at = 123 WHERE id = ?2", params![json!({"unknown": [true, "retain"]}).to_string(), session.id.as_bytes().as_slice()]).unwrap();
        let raw_before = database.raw_permission_snapshot().unwrap();
        let before = database.write_version(session.id).unwrap();
        if replace {
            let prepared = replacement(
                &database,
                conversation.clone(),
                conversation,
                &original,
                record(
                    PermissionLifetime::Conversation,
                    StructuredPermissionEffect::Deny,
                ),
            );
            database.commit_permission_mutation(&prepared).unwrap();
        } else {
            create(
                &database,
                conversation,
                record(
                    PermissionLifetime::Conversation,
                    StructuredPermissionEffect::Ask,
                ),
            );
        }
        let mut metadata_before: Value =
            serde_json::from_str(&raw_before.sessions[0].metadata).unwrap();
        let mut metadata_after: Value =
            serde_json::from_str(&database.raw_permission_snapshot().unwrap().sessions[0].metadata)
                .unwrap();
        metadata_before
            .as_object_mut()
            .unwrap()
            .remove(PERMISSION_FIELD);
        metadata_after
            .as_object_mut()
            .unwrap()
            .remove(PERMISSION_FIELD);
        assert_eq!(metadata_before, metadata_after);
        assert_eq!(database.write_version(session.id).unwrap(), before);
        let retained: bool = database
            .connection()
            .query_row(
                "SELECT pinned = 1 AND last_opened_at = 123 FROM sessions WHERE id = ?1",
                params![session.id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(retained);
        assert_accounting(&database, &session);
    }

    #[test_case(false; "stale_rules")]
    #[test_case(true; "stale_empty_rules")]
    fn recreation_retains_canonical_permissions_and_changes_lineage(empty: bool) {
        let (_temp, _dir, mut database, mut session) = database();
        let conversation = PermissionOwner::Conversation(session.id);
        let original = record(
            PermissionLifetime::Conversation,
            StructuredPermissionEffect::Allow,
        );
        create(&database, conversation.clone(), original.clone());
        if !empty {
            session
                .meta
                .structured_permission_rules
                .push(original.clone());
        }
        let prepared = replacement(
            &database,
            conversation.clone(),
            conversation.clone(),
            &original,
            record(
                PermissionLifetime::Conversation,
                StructuredPermissionEffect::Deny,
            ),
        );
        database.commit_permission_mutation(&prepared).unwrap();
        let expected = database.permission_snapshot(conversation.clone()).unwrap();
        let active = expected
            .records
            .iter()
            .find(|record| record.is_active())
            .unwrap();
        let stale_edit = replacement(
            &database,
            conversation.clone(),
            conversation.clone(),
            active,
            record(
                PermissionLifetime::Conversation,
                StructuredPermissionEffect::Allow,
            ),
        );
        let recreation = database.delete(session.id, None).unwrap();
        database.recreate(&session, &recreation).unwrap();
        let current = database.permission_snapshot(conversation).unwrap();
        assert_eq!(current.records, expected.records);
        assert_ne!(current.revision.lineage, expected.revision.lineage);
        assert!(current.revision.generation > expected.revision.generation);
        assert!(matches!(
            database.commit_permission_mutation(&stale_edit),
            Err(PermissionMutationError::Conflict { .. })
        ));
        assert_accounting(&database, &session);
    }

    #[test_case(false; "successful_copy")]
    #[test_case(true; "failed_copy")]
    fn split_database_move_refuses_and_copy_never_revokes_source(fail_copy: bool) {
        let (_temp, dir, database, session) = database();
        let (_other_temp, other_dir, other, _other_session) = self::database();
        let split = StateDir::split(dir.path().to_path_buf(), other_dir.path().to_path_buf());
        assert!(!permission_databases_shared(&split).unwrap());
        let conversation = PermissionOwner::Conversation(session.id);
        let original = record(
            PermissionLifetime::Conversation,
            StructuredPermissionEffect::Allow,
        );
        create(&database, conversation.clone(), original.clone());
        let source = database.permission_snapshot(conversation.clone()).unwrap();
        let destination = other
            .permission_snapshot(PermissionOwner::Persistent)
            .unwrap();
        assert!(matches!(
            prepare_mutation(
                vec![source.clone(), destination.clone()],
                PermissionMutation::Replace {
                    source: PermissionRecordIdentity {
                        owner: conversation.clone(),
                        record_id: original.id.clone()
                    },
                    destination: PermissionOwner::Persistent,
                    replacement: Box::new(record(
                        PermissionLifetime::Global,
                        StructuredPermissionEffect::Allow
                    ))
                }
            ),
            Err(PermissionMutationError::DifferentDatabase)
        ));
        let copy = prepare_mutation(
            vec![destination.clone()],
            PermissionMutation::Create {
                destination: PermissionOwner::Persistent,
                records: Box::new([record(
                    PermissionLifetime::Global,
                    StructuredPermissionEffect::Allow,
                )]),
            },
        )
        .unwrap();
        if fail_copy {
            other.connection().execute_batch(&format!("CREATE TRIGGER injected_copy BEFORE INSERT ON state BEGIN SELECT RAISE(ABORT, '{FAILURE}'); END;")).unwrap();
        }
        assert_eq!(other.commit_permission_mutation(&copy).is_err(), fail_copy);
        assert_eq!(
            database.permission_snapshot(conversation.clone()).unwrap(),
            source
        );
        if fail_copy {
            assert_eq!(
                other
                    .permission_snapshot(PermissionOwner::Persistent)
                    .unwrap(),
                destination
            );
        }
        let revoke = prepare_mutation(
            vec![source],
            PermissionMutation::Revoke {
                source: PermissionRecordIdentity {
                    owner: conversation,
                    record_id: original.id,
                },
            },
        )
        .unwrap();
        database.commit_permission_mutation(&revoke).unwrap();
    }

    #[test_case(""; "empty")]
    #[test_case("\n"; "line_break")]
    #[test_case("\u{202e}display"; "bidi")]
    fn invalid_label_is_not_durable(label: &str) {
        let (_temp, _dir, database, _session) = database();
        let mut invalid = record(
            PermissionLifetime::Global,
            StructuredPermissionEffect::Allow,
        );
        invalid.label = Some(label.into());
        assert!(
            prepare_mutation(
                vec![
                    database
                        .permission_snapshot(PermissionOwner::Persistent)
                        .unwrap()
                ],
                PermissionMutation::Create {
                    destination: PermissionOwner::Persistent,
                    records: Box::new([invalid])
                }
            )
            .is_err()
        );
    }

    #[test_case(PERMISSION_LABEL_MAX_BYTES, true; "limit")]
    #[test_case(PERMISSION_LABEL_MAX_BYTES + 1, false; "over_limit")]
    fn label_size_is_bounded(bytes: usize, accepted: bool) {
        let (_temp, _dir, database, _session) = database();
        let mut labeled = record(
            PermissionLifetime::Global,
            StructuredPermissionEffect::Allow,
        );
        labeled.label = Some("x".repeat(bytes));
        assert_eq!(
            prepare_mutation(
                vec![
                    database
                        .permission_snapshot(PermissionOwner::Persistent)
                        .unwrap()
                ],
                PermissionMutation::Create {
                    destination: PermissionOwner::Persistent,
                    records: Box::new([labeled])
                }
            )
            .is_ok(),
            accepted
        );
    }

    #[test]
    fn receipt_retention_is_bounded_and_expired_retry_conflicts() {
        let (_temp, _dir, database, _session) = database();
        let first = create(
            &database,
            PermissionOwner::Persistent,
            record(
                PermissionLifetime::Global,
                StructuredPermissionEffect::Allow,
            ),
        );
        let receipt = database
            .permission_receipt(first.operation_id())
            .unwrap()
            .unwrap();
        let transaction = database.connection().unchecked_transaction().unwrap();
        for _ in 0..PERMISSION_RECEIPT_LIMIT {
            transaction.execute("INSERT INTO permission_receipts (operation_id, fingerprint, receipt) VALUES (?1, '', ?2)", params![CaudraId::generate().as_bytes().as_slice(), serde_json::to_string(&receipt).unwrap()]).unwrap();
        }
        transaction.commit().unwrap();
        create(
            &database,
            PermissionOwner::Persistent,
            record(PermissionLifetime::Global, StructuredPermissionEffect::Ask),
        );
        let count: i64 = database
            .connection()
            .query_row("SELECT count(*) FROM permission_receipts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            count,
            to_i64(PERMISSION_RECEIPT_LIMIT, "permission receipt limit").unwrap()
        );
        assert!(
            database
                .permission_receipt(first.operation_id())
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            database.commit_permission_mutation(&first),
            Err(PermissionMutationError::Conflict { .. })
        ));
    }

    #[test]
    fn lost_ack_replay_after_later_revocation_does_not_republish_a_grant() {
        let (_temp, _dir, database, _session) = database();
        let original = record(
            PermissionLifetime::Global,
            StructuredPermissionEffect::Allow,
        );
        let first = create(&database, PermissionOwner::Persistent, original.clone());
        let receipt = database
            .permission_receipt(first.operation_id())
            .unwrap()
            .unwrap();
        let revoke = prepare_mutation(
            vec![
                database
                    .permission_snapshot(PermissionOwner::Persistent)
                    .unwrap(),
            ],
            PermissionMutation::Revoke {
                source: PermissionRecordIdentity {
                    owner: PermissionOwner::Persistent,
                    record_id: original.id,
                },
            },
        )
        .unwrap();
        database.commit_permission_mutation(&revoke).unwrap();
        let current = database
            .permission_snapshot(PermissionOwner::Persistent)
            .unwrap();
        assert_eq!(
            database.commit_permission_mutation(&first).unwrap(),
            receipt
        );
        assert_eq!(
            database
                .permission_snapshot(PermissionOwner::Persistent)
                .unwrap(),
            current
        );
        assert!(current.records.iter().all(|record| !record.is_active()));
        let mut reused = revoke;
        reused.operation_id = first.operation_id();
        assert!(matches!(
            database.commit_permission_mutation(&reused),
            Err(PermissionMutationError::OperationIdReused)
        ));
    }

    #[test_case(false; "independent_fork")]
    #[test_case(true; "explicit_move_between_sessions")]
    fn conversation_owners_are_distinct(move_to_fork: bool) {
        let (_temp, _dir, mut database, session) = database();
        let conversation = PermissionOwner::Conversation(session.id);
        let original = record(
            PermissionLifetime::Conversation,
            StructuredPermissionEffect::Allow,
        );
        create(&database, conversation.clone(), original.clone());
        let mut fork = TestSession::new(MODEL, PROJECT);
        fork.meta.structured_permission_rules = vec![original.clone()];
        database.save(&fork, None).unwrap();
        let fork_owner = PermissionOwner::Conversation(fork.id);
        let before = database.permission_snapshot(fork_owner.clone()).unwrap();
        let destination = if move_to_fork {
            fork_owner.clone()
        } else {
            conversation.clone()
        };
        let prepared = replacement(
            &database,
            conversation.clone(),
            destination,
            &original,
            record(
                PermissionLifetime::Conversation,
                StructuredPermissionEffect::Deny,
            ),
        );
        database.commit_permission_mutation(&prepared).unwrap();
        assert!(!database.permission_snapshot(conversation).unwrap().records[0].is_active());
        let after = database.permission_snapshot(fork_owner).unwrap();
        assert!(after.records[0].is_active());
        if move_to_fork {
            assert_eq!(after.records.len(), before.records.len() + 1);
        } else {
            assert_eq!(after, before);
        }
    }

    #[test]
    fn missing_destination_cannot_be_created_by_a_permission_move() {
        let (_temp, _dir, database, _session) = database();
        let original = record(
            PermissionLifetime::Global,
            StructuredPermissionEffect::Allow,
        );
        create(&database, PermissionOwner::Persistent, original.clone());
        let missing = PermissionOwner::Conversation(CaudraId::generate());
        let snapshots = database
            .permission_snapshots(&[PermissionOwner::Persistent, missing.clone()])
            .unwrap();
        assert!(matches!(
            prepare_mutation(
                snapshots,
                PermissionMutation::Replace {
                    source: PermissionRecordIdentity {
                        owner: PermissionOwner::Persistent,
                        record_id: original.id
                    },
                    destination: missing,
                    replacement: Box::new(record(
                        PermissionLifetime::Conversation,
                        StructuredPermissionEffect::Allow
                    )),
                }
            ),
            Err(PermissionMutationError::Conflict { .. })
        ));
    }

    #[test]
    fn simultaneous_connections_cannot_both_replace_the_same_source() {
        let (_temp, dir, database, session) = database();
        let original = record(
            PermissionLifetime::Global,
            StructuredPermissionEffect::Allow,
        );
        create(&database, PermissionOwner::Persistent, original.clone());
        let barrier = Arc::new(Barrier::new(3));
        let handles = [
            StructuredPermissionEffect::Allow,
            StructuredPermissionEffect::Deny,
        ]
        .into_iter()
        .map(|effect| {
            let prepared = replacement(
                &database,
                PermissionOwner::Persistent,
                PermissionOwner::Conversation(session.id),
                &original,
                record(PermissionLifetime::Conversation, effect),
            );
            let dir = dir.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let database = SessionDatabase::open_state(&dir).unwrap();
                database.commit_permission_mutation(&prepared)
            })
        })
        .collect::<Vec<_>>();
        barrier.wait();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(PermissionMutationError::Conflict { .. })))
                .count(),
            1
        );
        assert!(
            !database
                .permission_snapshot(PermissionOwner::Persistent)
                .unwrap()
                .records[0]
                .is_active()
        );
        assert_eq!(
            database
                .permission_snapshot(PermissionOwner::Conversation(session.id))
                .unwrap()
                .records
                .len(),
            1
        );
    }

    #[test]
    fn legacy_approval_does_not_recreate_a_disappeared_inventory() {
        let (_temp, dir, database, _session) = database();
        let mut state = PermissionState::open(&dir).unwrap();
        state
            .insert(
                None,
                record(
                    PermissionLifetime::Global,
                    StructuredPermissionEffect::Allow,
                )
                .rule,
            )
            .unwrap();
        database
            .state_delete(SCOPE_GLOBAL, PERMISSION_RULES.name)
            .unwrap();
        assert!(
            state
                .insert(
                    None,
                    record(
                        PermissionLifetime::Global,
                        StructuredPermissionEffect::Allow
                    )
                    .rule
                )
                .is_err()
        );
        assert!(
            !database
                .permission_snapshot(PermissionOwner::Persistent)
                .unwrap()
                .revision
                .row_present
        );
    }

    #[test_case(false; "unchanged_read_guard")]
    #[test_case(true; "changed_read_guard")]
    fn create_can_compare_a_source_owner_without_writing_it(revoke: bool) {
        let (_temp, _dir, database, session) = database();
        let conversation = PermissionOwner::Conversation(session.id);
        let original = record(
            PermissionLifetime::Conversation,
            StructuredPermissionEffect::Allow,
        );
        create(&database, conversation.clone(), original.clone());
        let snapshots = database
            .permission_snapshots(&[conversation.clone(), PermissionOwner::Persistent])
            .unwrap();
        let before = snapshots[0].clone();
        let prepared = prepare_mutation(
            snapshots,
            PermissionMutation::Create {
                destination: PermissionOwner::Persistent,
                records: Box::new([record(
                    PermissionLifetime::Global,
                    StructuredPermissionEffect::Allow,
                )]),
            },
        )
        .unwrap();
        assert_eq!(prepared.targets().len(), 1);
        if revoke {
            let revoke = prepare_mutation(
                vec![before.clone()],
                PermissionMutation::Revoke {
                    source: PermissionRecordIdentity {
                        owner: conversation.clone(),
                        record_id: original.id,
                    },
                },
            )
            .unwrap();
            database.commit_permission_mutation(&revoke).unwrap();
            assert!(matches!(
                database.commit_permission_mutation(&prepared),
                Err(PermissionMutationError::Conflict { .. })
            ));
            assert!(
                !database
                    .permission_snapshot(PermissionOwner::Persistent)
                    .unwrap()
                    .revision
                    .row_present
            );
        } else {
            database.commit_permission_mutation(&prepared).unwrap();
            assert_eq!(database.permission_snapshot(conversation).unwrap(), before);
        }
    }

    #[test]
    fn copied_database_ids_do_not_make_separate_databases_atomic() {
        let (_temp, dir, database, session) = database();
        database.checkpoint(true).unwrap();
        let copy_dir = TempDir::new().unwrap();
        let copied = StateDir::from_path(copy_dir.path().to_path_buf());
        fs::copy(database.path(), copied.path().join(super::SESSIONS_DB_FILE)).unwrap();
        let copy = SessionDatabase::open_state(&copied).unwrap();
        let conversation = PermissionOwner::Conversation(session.id);
        assert_ne!(
            database.permission_snapshot(conversation).unwrap().store_id,
            copy.permission_snapshot(PermissionOwner::Persistent)
                .unwrap()
                .store_id
        );
        assert!(
            !permission_databases_shared(&StateDir::split(
                dir.path().to_path_buf(),
                copied.path().to_path_buf()
            ))
            .unwrap()
        );
    }

    #[test_case("", EMPTY_SHA256; "empty")]
    #[test_case("abc", ABC_SHA256; "ascii")]
    fn sha256_hex_preserves_the_durable_encoding(input: &str, expected: &str) {
        assert_eq!(sha256_hex(input.as_bytes()), expected);
    }

    fn assert_invalid_generation(error: PermissionMutationError, expected_field: &str) {
        let PermissionMutationError::Session(SessionError::CorruptDatabaseValue { field, reason }) =
            error
        else {
            panic!("{error}")
        };
        assert_eq!(field, expected_field);
        assert_eq!(reason, NEGATIVE_GENERATION_REASON);
    }

    #[test_case("generation", DATABASE_GENERATION_FIELD; "database_generation")]
    #[test_case("persistent_generation", PERSISTENT_GENERATION_FIELD; "persistent_generation")]
    #[test_case("permission_generation", SESSION_GENERATION_FIELD; "conversation_generation")]
    fn negative_generations_are_rejected(column: &str, expected_field: &str) {
        let (_temp, _dir, database, session) = database();
        database
            .connection()
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .unwrap();
        let table = if column == "permission_generation" {
            "sessions"
        } else {
            "permission_clock"
        };
        database
            .connection()
            .execute(
                &format!("UPDATE {table} SET {column} = ?1"),
                params![NEGATIVE_GENERATION],
            )
            .unwrap();
        let error = match column {
            "generation" => database.permission_generation().unwrap_err(),
            "persistent_generation" => database
                .permission_snapshot(PermissionOwner::Persistent)
                .unwrap_err(),
            _ => database
                .permission_snapshot(PermissionOwner::Conversation(session.id))
                .unwrap_err(),
        };
        assert_invalid_generation(error, expected_field);
    }

    #[test]
    fn negative_session_generation_refuses_load_and_delete_without_mutation() {
        let (_temp, _dir, mut database, session) = database();
        let before = database.write_version(session.id).unwrap();
        database
            .connection()
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .unwrap();
        database
            .connection()
            .execute(
                "UPDATE sessions SET permission_generation = ?1",
                params![NEGATIVE_GENERATION],
            )
            .unwrap();
        assert_invalid_generation(
            database
                .load::<Message, Value, Value>(session.id)
                .unwrap_err()
                .into(),
            SESSION_GENERATION_FIELD,
        );
        assert_invalid_generation(
            database.delete(session.id, None).unwrap_err().into(),
            SESSION_GENERATION_FIELD,
        );
        assert_eq!(database.write_version(session.id).unwrap(), before);
        assert!(database.tombstone_version(session.id).unwrap().is_none());
    }

    #[test_case(0; "zero")]
    #[test_case(i64::MAX; "sqlite_maximum")]
    fn sqlite_generation_boundaries_round_trip(value: i64) {
        let (_temp, _dir, database, session) = database();
        database
            .connection()
            .execute(
                "UPDATE permission_clock SET generation = ?1, persistent_generation = ?1",
                params![value],
            )
            .unwrap();
        database
            .connection()
            .execute(
                "UPDATE sessions SET permission_generation = ?1",
                params![value],
            )
            .unwrap();
        let expected = u64::try_from(value).unwrap();
        assert_eq!(
            database.permission_generation().unwrap().generation,
            expected
        );
        assert_eq!(
            database
                .permission_snapshot(PermissionOwner::Persistent)
                .unwrap()
                .revision
                .generation,
            expected
        );
        assert_eq!(
            database
                .permission_snapshot(PermissionOwner::Conversation(session.id))
                .unwrap()
                .revision
                .generation,
            expected
        );
        assert_eq!(
            database
                .load::<Message, Value, Value>(session.id)
                .unwrap()
                .meta
                .permission_generation,
            expected
        );
    }

    #[test]
    fn historical_receipt_cannot_make_a_deleted_inventory_current() {
        let (_temp, dir, database, _session) = database();
        let mut state = PermissionState::open(&dir).unwrap();
        let prepared = prepare_mutation(
            vec![state.snapshot().unwrap()],
            PermissionMutation::Create {
                destination: PermissionOwner::Persistent,
                records: Box::new([record(
                    PermissionLifetime::Global,
                    StructuredPermissionEffect::Allow,
                )]),
            },
        )
        .unwrap();
        let receipt = state.commit_mutation(&prepared).unwrap();
        database
            .state_delete(SCOPE_GLOBAL, PERMISSION_RULES.name)
            .unwrap();
        assert!(state.commit_mutation(&prepared).is_err());
        assert_eq!(
            state.mutation_receipt(prepared.operation_id()).unwrap(),
            Some(receipt)
        );
        assert!(matches!(
            state.snapshot(),
            Err(PermissionMutationError::Conflict { .. })
        ));
        assert!(
            !database
                .permission_snapshot(PermissionOwner::Persistent)
                .unwrap()
                .revision
                .row_present
        );
    }

    #[test_case(PermissionLifetime::Conversation, PermissionLifetime::Project; "conversation_to_project")]
    #[test_case(PermissionLifetime::Conversation, PermissionLifetime::Global; "conversation_to_global")]
    #[test_case(PermissionLifetime::Project, PermissionLifetime::Conversation; "project_to_conversation")]
    #[test_case(PermissionLifetime::Global, PermissionLifetime::Conversation; "global_to_conversation")]
    fn late_chat_snapshots_preserve_both_sides_of_lifetime_moves(
        from: PermissionLifetime,
        to: PermissionLifetime,
    ) {
        for delta in [false, true] {
            let (_temp, dir, mut database, mut session) = database();
            let conversation = PermissionOwner::Conversation(session.id);
            let original = record(from.clone(), StructuredPermissionEffect::Allow);
            let source = owner(&from, &session);
            let destination = owner(&to, &session);
            create(&database, source.clone(), original.clone());
            database
                .permission_snapshot(conversation.clone())
                .unwrap()
                .apply_to_meta(session.id, &mut session.meta)
                .unwrap();
            let cursor = database.save(&session, None).unwrap();
            let prepared = replacement(
                &database,
                source,
                destination,
                &original,
                record(to.clone(), StructuredPermissionEffect::Deny),
            );
            let external = SessionDatabase::open_state(&dir).unwrap();
            external.commit_permission_mutation(&prepared).unwrap();
            let expected = external
                .permission_snapshots(&[conversation.clone(), PermissionOwner::Persistent])
                .unwrap();
            session.push_message(Message(LATE_MESSAGE.into()));
            session.meta.input_draft = Some(LATE_MESSAGE.into());
            database.save(&session, delta.then_some(&cursor)).unwrap();
            assert_eq!(
                database
                    .permission_snapshots(&[conversation, PermissionOwner::Persistent])
                    .unwrap(),
                expected
            );
            let loaded = database.load::<Message, Value, Value>(session.id).unwrap();
            assert_eq!(loaded.messages(), session.messages());
            assert_eq!(loaded.meta.input_draft, session.meta.input_draft);
            assert_accounting(&database, &session);
        }
    }

    #[test_case(false; "inject_allow")]
    #[test_case(true; "drop_deny")]
    fn matching_snapshot_generation_does_not_authorize_permission_changes(drop_deny: bool) {
        let (_temp, _dir, mut database, mut session) = database();
        let conversation = PermissionOwner::Conversation(session.id);
        create(
            &database,
            conversation.clone(),
            record(
                PermissionLifetime::Conversation,
                StructuredPermissionEffect::Deny,
            ),
        );
        let expected = database.permission_snapshot(conversation.clone()).unwrap();
        expected
            .apply_to_meta(session.id, &mut session.meta)
            .unwrap();
        if drop_deny {
            session.meta.structured_permission_rules.clear();
        } else {
            session.meta.structured_permission_rules.push(record(
                PermissionLifetime::Conversation,
                StructuredPermissionEffect::Allow,
            ));
        }
        session.push_message(Message(LATE_MESSAGE.into()));
        database.save(&session, None).unwrap();
        assert_eq!(
            database.permission_snapshot(conversation).unwrap(),
            expected
        );
        assert_eq!(
            database
                .load::<Message, Value, Value>(session.id)
                .unwrap()
                .messages(),
            session.messages()
        );
    }

    #[test]
    fn split_database_commit_refuses_even_when_preparation_uses_one_store() {
        let (_temp, dir, database, session) = database();
        let (_persistent_temp, persistent_dir, persistent, _persistent_session) = self::database();
        let conversation = PermissionOwner::Conversation(session.id);
        let original = record(
            PermissionLifetime::Conversation,
            StructuredPermissionEffect::Allow,
        );
        create(&database, conversation.clone(), original.clone());
        let prepared = replacement(
            &database,
            conversation.clone(),
            PermissionOwner::Persistent,
            &original,
            record(
                PermissionLifetime::Global,
                StructuredPermissionEffect::Allow,
            ),
        );
        let before = database
            .permission_snapshots(&[conversation.clone(), PermissionOwner::Persistent])
            .unwrap();
        let persistent_before = persistent
            .permission_snapshot(PermissionOwner::Persistent)
            .unwrap();
        let split = SessionDatabase::open_state(&StateDir::split(
            dir.path().to_path_buf(),
            persistent_dir.path().to_path_buf(),
        ))
        .unwrap();
        assert!(matches!(
            split.commit_permission_mutation(&prepared),
            Err(PermissionMutationError::DifferentDatabase)
        ));
        assert_eq!(
            database
                .permission_snapshots(&[conversation, PermissionOwner::Persistent])
                .unwrap(),
            before
        );
        assert_eq!(
            persistent
                .permission_snapshot(PermissionOwner::Persistent)
                .unwrap(),
            persistent_before
        );
        assert!(
            database
                .permission_receipt(prepared.operation_id())
                .unwrap()
                .is_none()
        );
        assert!(
            persistent
                .permission_receipt(prepared.operation_id())
                .unwrap()
                .is_none()
        );
    }

    #[test_case(false; "receipt_lookup")]
    #[test_case(true; "commit_replay")]
    fn receipt_payload_cannot_acknowledge_another_operation(replay: bool) {
        let (_temp, _dir, database, _session) = database();
        let prepared = create(
            &database,
            PermissionOwner::Persistent,
            record(
                PermissionLifetime::Global,
                StructuredPermissionEffect::Allow,
            ),
        );
        let expected = database
            .permission_snapshot(PermissionOwner::Persistent)
            .unwrap();
        database.connection().execute("UPDATE permission_receipts SET receipt = json_set(receipt, '$.operation_id', ?1) WHERE operation_id = ?2", params![CaudraId::generate().to_string(), prepared.operation_id().as_bytes().as_slice()]).unwrap();
        let error = if replay {
            database.commit_permission_mutation(&prepared).unwrap_err()
        } else {
            database
                .permission_receipt(prepared.operation_id())
                .unwrap_err()
        };
        let PermissionMutationError::Session(SessionError::CorruptDatabaseValue { reason, .. }) =
            error
        else {
            panic!("{error}")
        };
        assert_eq!(reason, INVALID_RECEIPT_ID);
        assert_eq!(
            database
                .permission_snapshot(PermissionOwner::Persistent)
                .unwrap(),
            expected
        );
    }
}
