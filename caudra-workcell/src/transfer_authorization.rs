use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use crate::TransferValidity;
use async_trait::async_trait;
use caudra_agent::{
    CancelToken, EventSender,
    permissions::{
        PermissionAuthorityProfile, PermissionExecutorKind, PermissionManager, PermissionResource,
        PermissionResourceAccess, PermissionResourceKind, PermissionRisk, PermissionSubject,
        RemotePermissionIdentity, filesystem_permission_resource,
    },
    tools::{PermissionIntent, PermissionScopes},
    workspace_transfer::{
        LocalAccess, PlanReview, PlannedFile, TransferAction, TransferAuthorization, TransferError,
        TransferPlan, TransferRoots,
    },
};
use caudra_config::ToolKey;
use caudra_storage::id::CaudraId;
use caudra_workspace::{
    LocalTransferAuthorization, LocalTransferCondition, LocalTransferReview,
    PreparedTransferPublication, TransferDigest, WorkspaceError, WorkspacePath,
};
use serde_json::{Value, json};
use smol::lock::Mutex as AsyncMutex;
use workcell::host_contract::{OperationIntent, OperationKind, ResourceAccess};

const LOCAL_CONTRACT: &str = "workspace.transfer.local.v1";
const REMOTE_CONTRACT: &str = "workspace.transfer.remote.v1";
const TOOL: &str = "workspace_transfer";

/// An Execute gesture consents to one immutable review, not to permissions. Each authority
/// is separately evaluated by Caudra, including the server's prepared resource scope.
pub struct NativeTransferAuthorization {
    permissions: Arc<PermissionManager>,
    events: EventSender,
    cancel: CancelToken,
    consent: Mutex<Option<(TransferDigest, PlanReview)>>,
    validity: TransferValidity,
}

impl NativeTransferAuthorization {
    pub fn new(
        permissions: Arc<PermissionManager>,
        events: EventSender,
        cancel: CancelToken,
        validity: TransferValidity,
    ) -> Self {
        Self {
            permissions,
            events,
            cancel,
            consent: Mutex::new(None),
            validity,
        }
    }

    pub fn consent(&self, plan: &TransferPlan) -> Result<(), TransferError> {
        *self.consent.lock().map_err(|_| TransferError::Stale)? =
            Some((plan.digest().clone(), plan.review().clone()));
        Ok(())
    }

    async fn enforce(
        &self,
        roots: &TransferRoots,
        resources: Vec<PermissionResource>,
        input: Value,
        remote: bool,
    ) -> Result<(), TransferError> {
        (self.validity)().map_err(|_| TransferError::Stale)?;
        let intent = PermissionIntent::new(
            PermissionScopes::single(
                serde_json::to_string(&input).map_err(|_| TransferError::Stale)?,
            ),
            resources,
            PermissionRisk::High,
        )
        .with_authority(if remote {
            PermissionAuthorityProfile::RemoteResource
        } else {
            PermissionAuthorityProfile::Filesystem {
                input_pointers: Vec::new(),
            }
        });
        let identity = if remote {
            (
                PermissionSubject::RemoteNative {
                    identity: RemotePermissionIdentity::from_binding(&roots.remote.binding),
                    owner: "caudra".into(),
                    contract: REMOTE_CONTRACT.into(),
                },
                PermissionExecutorKind::RemoteWorkcell,
            )
        } else {
            (
                PermissionSubject::Native {
                    owner: "caudra".into(),
                    contract: LOCAL_CONTRACT.into(),
                },
                PermissionExecutorKind::Native,
            )
        };
        let (_legacy_sender, legacy_receiver) = flume::bounded(1);
        let legacy_receiver = AsyncMutex::new(legacy_receiver);
        self.permissions
            .enforce_with_intent(
                &ToolKey::native(TOOL),
                &intent,
                &input,
                &self.events,
                Some(&legacy_receiver),
                &CaudraId::generate().to_string(),
                &self.cancel,
                None,
                Some(identity),
                false,
            )
            .await
            .map_err(|_| TransferError::Workspace(WorkspaceError::PermissionDenied))?;
        (self.validity)().map_err(|_| TransferError::Stale)
    }

    fn local_resource(
        roots: &TransferRoots,
        path: &WorkspacePath,
        access: PermissionResourceAccess,
    ) -> PermissionResource {
        filesystem_permission_resource(
            if path.is_root() {
                PermissionResourceKind::Directory
            } else {
                PermissionResourceKind::File
            },
            &roots.local.canonical_path().join(path.as_str()),
            access,
            roots.local.canonical_path(),
        )
    }

    fn remote_root(roots: &TransferRoots, access: PermissionResourceAccess) -> PermissionResource {
        PermissionResource {
            kind: PermissionResourceKind::RemoteDirectory {
                identity: RemotePermissionIdentity::from_binding(&roots.remote.binding),
            },
            value: roots
                .remote
                .cursor
                .scope()
                .ancestors()
                .iter()
                .chain([roots.remote.cursor.scope().resource_id()])
                .map(|id| id.as_str())
                .collect::<Vec<_>>()
                .join("\u{1f}"),
            access: Some(access),
            protected: false,
            requires_prompt: true,
            attributes: BTreeMap::from([("display_path".into(), roots.remote.cwd.to_string())]),
        }
    }
}

#[async_trait]
impl TransferAuthorization for NativeTransferAuthorization {
    async fn roots(&self, roots: &TransferRoots) -> Result<(), TransferError> {
        self.enforce(
            roots,
            vec![Self::local_resource(
                roots,
                &WorkspacePath::root(),
                PermissionResourceAccess::Read,
            )],
            json!({"operation":"compare local tree", "roots":roots}),
            false,
        )
        .await?;
        self.enforce(
            roots,
            vec![Self::remote_root(roots, PermissionResourceAccess::Read)],
            json!({"operation":"compare remote tree", "roots":roots}),
            true,
        )
        .await
    }

    async fn local(
        &self,
        roots: &TransferRoots,
        path: &WorkspacePath,
        access: LocalAccess,
    ) -> Result<(), TransferError> {
        let permission = if access == LocalAccess::Write {
            PermissionResourceAccess::Write
        } else {
            PermissionResourceAccess::Read
        };
        self.enforce(
            roots,
            vec![Self::local_resource(roots, path, permission)],
            json!({"operation":format!("{access:?}"), "path":path, "roots":roots}),
            false,
        )
        .await
    }

    async fn review_plan(&self, plan: &TransferPlan) -> Result<(), TransferError> {
        if self
            .consent
            .lock()
            .map_err(|_| TransferError::Stale)?
            .as_ref()
            .map(|(digest, _)| digest)
            != Some(plan.digest())
        {
            return Err(WorkspaceError::PermissionDenied.into());
        }
        let review = plan.review();
        let roots = &review.context.roots;
        let access = if review.action == TransferAction::Pull {
            PermissionResourceAccess::Write
        } else {
            PermissionResourceAccess::Read
        };
        let resources = review
            .files
            .iter()
            .flat_map(|file| {
                let mut resources = vec![Self::local_resource(roots, &file.path, access.clone())];
                if review.action == TransferAction::Pull {
                    resources.extend(file.create_directories.iter().map(|path| {
                        Self::local_resource(roots, path, PermissionResourceAccess::Write)
                    }));
                }
                resources
            })
            .collect();
        self.enforce(
            roots,
            resources,
            json!({"plan_id":plan.digest(), "review":review}),
            false,
        )
        .await?;
        self.enforce(
            roots,
            vec![Self::remote_root(
                roots,
                if review.action == TransferAction::Pull {
                    PermissionResourceAccess::Read
                } else {
                    PermissionResourceAccess::Write
                },
            )],
            json!({"plan_id":plan.digest(), "review":review}),
            true,
        )
        .await
    }

    async fn review_remote_publication(
        &self,
        plan: &TransferPlan,
        file: &PlannedFile,
        prepared: &PreparedTransferPublication,
    ) -> Result<(), TransferError> {
        let intent: OperationIntent =
            serde_json::from_value(prepared.review.clone()).map_err(|_| TransferError::Stale)?;
        if intent.kind != OperationKind::Transfer
            || !intent.mutating
            || intent.resources.is_empty()
            || prepared.request.path != file.path
        {
            return Err(TransferError::Stale);
        }
        let roots = &plan.review().context.roots;
        let identity = RemotePermissionIdentity::from_binding(&roots.remote.binding);
        let resources = intent
            .resources
            .into_iter()
            .map(|resource| {
                if resource.scope.is_empty()
                    || !matches!(
                        resource.access,
                        ResourceAccess::Write
                            | ResourceAccess::ReadWrite
                            | ResourceAccess::Traverse
                            | ResourceAccess::Inspect
                    )
                {
                    return Err(TransferError::Stale);
                }
                Ok(PermissionResource {
                    kind: PermissionResourceKind::RemoteFile {
                        identity: identity.clone(),
                    },
                    value: resource
                        .scope
                        .iter()
                        .map(|id| id.as_str())
                        .collect::<Vec<_>>()
                        .join("\u{1f}"),
                    access: Some(
                        if matches!(
                            resource.access,
                            ResourceAccess::Write | ResourceAccess::ReadWrite
                        ) {
                            PermissionResourceAccess::Write
                        } else {
                            PermissionResourceAccess::Read
                        },
                    ),
                    protected: false,
                    requires_prompt: true,
                    attributes: BTreeMap::from([
                        ("display_path".into(), resource.display.as_str().into()),
                        (
                            "request_digest".into(),
                            prepared.request_digest.as_str().into(),
                        ),
                    ]),
                })
            })
            .collect::<Result<Vec<_>, TransferError>>()?;
        self.enforce(
            roots,
            resources,
            json!({"plan_id":plan.digest(), "prepared":prepared}),
            true,
        )
        .await
    }
}

#[async_trait]
impl LocalTransferAuthorization for NativeTransferAuthorization {
    async fn authorize(&self, review: &LocalTransferReview) -> Result<(), WorkspaceError> {
        let plan = self
            .consent
            .lock()
            .map_err(|_| WorkspaceError::PermissionDenied)?
            .as_ref()
            .map(|(_, review)| review.clone())
            .ok_or(WorkspaceError::PermissionDenied)?;
        if plan.action != TransferAction::Pull
            || !plan.files.iter().any(|file| {
                file.path.as_str() == review.destination.path.as_str()
                    && file
                        .remote
                        .as_ref()
                        .is_some_and(|stamp| stamp.content == review.content)
                    && match (&file.local, &review.destination.condition) {
                        (None, LocalTransferCondition::MustNotExist) => true,
                        (Some(stamp), LocalTransferCondition::Matches(revision)) => {
                            stamp.revision == revision.0
                        }
                        _ => false,
                    }
                    && review
                        .destination
                        .create_directories
                        .iter()
                        .all(|path| file.create_directories.contains(path))
            })
        {
            return Err(WorkspaceError::PermissionDenied);
        }
        let path = WorkspacePath::new(review.destination.path.as_str())
            .map_err(|_| WorkspaceError::PermissionDenied)?;
        self.enforce(
            &plan.context.roots,
            vec![Self::local_resource(
                &plan.context.roots,
                &path,
                PermissionResourceAccess::Write,
            )],
            json!({"local_publication":review}),
            false,
        )
        .await
        .map_err(|_| WorkspaceError::PermissionDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::NativeTransferAuthorization;
    use caudra_agent::{
        AgentEvent, CancelToken, EventSender,
        permissions::{PermissionAnswer, PermissionManager, PluginRuleStore},
        workspace_transfer::{
            LocalAccess, LocalRootIdentity, RemoteRootIdentity, TransferAuthorization,
            TransferError, TransferRoots,
        },
    };
    use caudra_config::PermissionsConfig;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, ProjectIdentity, ProjectKey,
        ResourceId, ResourceScope, SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor,
        WorkspaceCursor, WorkspaceError, WorkspacePath,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use test_case::test_case;

    const FILE: &str = "file.txt";
    const STALE: &str = "profile changed while permission was open";

    #[test_case(false, false; "explicit_local_grant")]
    #[test_case(true, false; "native_denial")]
    #[test_case(false, true; "profile_changed_during_prompt")]
    fn native_local_grants_are_not_plan_consent_and_recheck_window_validity(
        deny: bool,
        stale: bool,
    ) {
        smol::block_on(async {
            let local = tempfile::tempdir().unwrap();
            let authority = AuthorityIdentity::new(
                SourceTrustAnchor::new("transfer-test").unwrap(),
                "server",
                "workspace",
                "generation",
                "namespace",
            )
            .unwrap();
            let binding = SessionWorkspaceBinding::new(
                SessionBindingId::new("binding").unwrap(),
                authority.clone(),
                AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap(),
                ProjectIdentity::new(authority, ProjectKey::new("project").unwrap()),
            )
            .unwrap();
            let cursor = WorkspaceCursor::new(
                &binding,
                ResourceScope::root(ResourceId::new("root").unwrap()),
                0,
                CwdHandle::new("cwd").unwrap(),
            );
            let roots = TransferRoots {
                local: LocalRootIdentity::capture(local.path()).unwrap(),
                remote: RemoteRootIdentity {
                    binding,
                    cursor,
                    cwd: WorkspacePath::root(),
                },
            };
            let manager = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                local.path().into(),
                Arc::new(PluginRuleStore::default()),
            ));
            let valid = Arc::new(AtomicBool::new(true));
            let (sender, received) = flume::unbounded();
            let authorization = Arc::new(NativeTransferAuthorization::new(
                manager.clone(),
                EventSender::new(sender, 0),
                CancelToken::none(),
                {
                    let valid = valid.clone();
                    Arc::new(move || {
                        if valid.load(Ordering::Acquire) {
                            Ok(())
                        } else {
                            Err(STALE.into())
                        }
                    })
                },
            ));
            let task = smol::spawn(async move {
                authorization
                    .local(
                        &roots,
                        &WorkspacePath::new(FILE).unwrap(),
                        LocalAccess::Write,
                    )
                    .await
            });
            let AgentEvent::PermissionRequest(request) = received.recv_async().await.unwrap().event
            else {
                panic!("native prompt required");
            };
            valid.store(!stale, Ordering::Release);
            assert!(manager.answer(
                &request.id,
                if deny {
                    PermissionAnswer::Deny
                } else {
                    PermissionAnswer::AllowOnce
                }
            ));
            match task.await {
                Ok(()) => assert!(!deny && !stale),
                Err(TransferError::Workspace(WorkspaceError::PermissionDenied)) => assert!(deny),
                Err(TransferError::Stale) => assert!(stale),
                other => panic!("unexpected authorization result: {other:?}"),
            }
        });
    }
}
