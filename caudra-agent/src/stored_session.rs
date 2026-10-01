use caudra_providers::{HistoryItem, TokenUsage};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::{
    Session, SessionCursor, SessionDatabase, SessionError, mark_opened,
};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_workspace::{WorkspacePath, WorkspaceSession};

use crate::ToolOutput;

pub type StoredSession = Session<HistoryItem, TokenUsage, ToolOutput>;

const RESUME_SCOPE_ERROR: &str =
    "session workspace identity changed; fork or explicitly rebind the session";
const RESUME_PATH_ERROR: &str = "persisted remote cwd is not a bounded logical workspace path";
const MAX_RESUME_COMPONENTS: usize = 256;

pub async fn workspace_logical_cwd(workspace: &WorkspaceSession) -> Result<String, String> {
    let service = workspace
        .workspace()
        .services()
        .read
        .as_ref()
        .ok_or("remote directory resolution is unavailable")?;
    let resolved = service
        .resolve_directory(
            workspace.binding(),
            workspace.cursor(),
            &WorkspacePath::root(),
        )
        .await
        .map_err(|error| error.to_string())?;
    resolved
        .resource
        .path
        .map(|path| path.to_string())
        .ok_or_else(|| RESUME_PATH_ERROR.into())
}

pub async fn resolve_resume_workspace(
    stored: Option<&StoredWorkspaceBinding>,
    cwd: &str,
    workspace: &WorkspaceSession,
) -> Result<(WorkspaceSession, StoredWorkspaceBinding), String> {
    let stored = stored
        .filter(|binding| !binding.is_local())
        .ok_or(RESUME_SCOPE_ERROR)?;
    let expected = StoredWorkspaceBinding::new_with_cursor(
        workspace.binding().clone(),
        workspace.cursor().clone(),
        stored.cursor_label().map(str::to_owned),
    )
    .map_err(|error| error.to_string())?;
    if !stored.same_workspace_identity(&expected) {
        return Err(RESUME_SCOPE_ERROR.into());
    }
    let path = WorkspacePath::new(cwd).map_err(|_| RESUME_PATH_ERROR)?;
    if path.as_str().split('/').count() > MAX_RESUME_COMPONENTS {
        return Err(RESUME_PATH_ERROR.into());
    }
    let service = workspace
        .workspace()
        .services()
        .read
        .as_ref()
        .ok_or("remote directory resolution is unavailable")?;
    let root = service
        .resolve_directory(
            workspace.binding(),
            workspace.cursor(),
            &WorkspacePath::root(),
        )
        .await
        .map_err(|error| error.to_string())?;
    let root_path = root
        .resource
        .path
        .as_ref()
        .ok_or(RESUME_PATH_ERROR)?
        .clone();
    let mut current = workspace
        .with_cursor(root)
        .map_err(|error| error.to_string())?;
    let relative = if root_path.is_root() {
        path.as_str()
    } else if root_path == path {
        "."
    } else {
        path.as_str()
            .strip_prefix(root_path.as_str())
            .and_then(|path| path.strip_prefix('/'))
            .ok_or(RESUME_SCOPE_ERROR)?
    };
    let previous = stored.cursor().ok_or(RESUME_SCOPE_ERROR)?;
    if !relative.eq(".") {
        let mut requested = String::new();
        let mut logical = if root_path.is_root() {
            String::new()
        } else {
            root_path.to_string()
        };
        for component in relative.split('/') {
            if !logical.is_empty() {
                logical.push('/');
            }
            logical.push_str(component);
            if !requested.is_empty() {
                requested.push('/');
            }
            requested.push_str(component);
            let candidate = service
                .resolve_directory(
                    current.binding(),
                    current.cursor(),
                    &WorkspacePath::new(&requested).map_err(|_| RESUME_PATH_ERROR)?,
                )
                .await
                .map_err(|error| error.to_string())?;
            if candidate.resource.path.as_ref().map(WorkspacePath::as_str) != Some(logical.as_str())
            {
                return Err(RESUME_PATH_ERROR.into());
            }
            if previous
                .scope()
                .contains(candidate.resource.scope.resource_id())
            {
                current = current
                    .with_cursor(candidate)
                    .map_err(|error| error.to_string())?;
                requested.clear();
            }
        }
        if !requested.is_empty() {
            return Err(RESUME_SCOPE_ERROR.into());
        }
    }
    if current.cursor().scope() != previous.scope()
        || current.cursor().generation() != previous.generation()
    {
        return Err(RESUME_SCOPE_ERROR.into());
    }
    let binding = StoredWorkspaceBinding::new_with_cursor(
        current.binding().clone(),
        current.cursor().clone(),
        stored.cursor_label().map(str::to_owned),
    )
    .map_err(|error| error.to_string())?;
    Ok((current, binding))
}

pub async fn resume_workspace_session(
    session: &mut StoredSession,
    workspace: &WorkspaceSession,
) -> Result<WorkspaceSession, String> {
    let (workspace, binding) =
        resolve_resume_workspace(session.workspace_binding(), &session.cwd, workspace).await?;
    session
        .replace_workspace_cursor(binding)
        .map_err(|error| error.to_string())?;
    Ok(workspace)
}

pub fn load_stored_session(
    id: CaudraId,
    storage: &StateDir,
) -> Result<StoredSession, SessionError> {
    StoredSession::load(id, storage)
}

/// The same open, keeping the cursor the load produced. A caller that will
/// write the session back should hand this to its writer: without it the first
/// save has no cursor to diff against and rewrites every payload.
pub fn open_stored_session_with_cursor(
    id: CaudraId,
    storage: &StateDir,
) -> Result<(StoredSession, SessionCursor), SessionError> {
    let loaded = SessionDatabase::open(storage)?.load_with_cursor(id)?;
    mark_opened(id, storage)?;
    Ok(loaded)
}

/// A load that is the user opening the session: resume, `--continue`, the
/// picker, an ACP `session/load`. Records the activity retention keys on.
pub fn open_stored_session(
    id: CaudraId,
    storage: &StateDir,
) -> Result<StoredSession, SessionError> {
    let session = load_stored_session(id, storage)?;
    mark_opened(id, storage)?;
    Ok(session)
}

pub fn latest_stored_session(
    cwd: &str,
    storage: &StateDir,
) -> Result<Option<StoredSession>, SessionError> {
    StoredSession::latest(cwd, storage)
}

#[cfg(test)]
pub(crate) mod tests {
    use async_trait::async_trait;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, ByteContent, CollectionRevision, CwdHandle,
        ListPage, ListRequest, OperationId, ProjectAsset, ProjectAssetContent, ProjectAssetKind,
        ProjectAssetManifest, ProjectAssetTrust, ProjectIdentity, ProjectKey, ReadBytesRequest,
        ReadTextRequest, ResolvedWorkspaceDirectory, ResourceId, ResourceKind, ResourceRevision,
        ResourceScope, ResourceSelector, SessionBindingId, SessionWorkspaceBinding,
        SourceTrustAnchor, TextContent, WorkspaceAssetService, WorkspaceCursor, WorkspaceError,
        WorkspaceHandle, WorkspaceReadService, WorkspaceResource, WorkspaceServices,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::types::{Answer, QuestionOption};
    use crate::{
        IndexDirectoryEntry, IndexDirectoryEntryKind, IndexLine, IndexLineSemantic, IndexOutput,
        IndexSourceRange, ShellFilterInfo, ShellOutput,
    };
    use caudra_storage::sessions::SessionDatabase;

    const CWD: &str = "/repo";
    const MODEL: &str = "anthropic/test";
    const LOAD_IS_NOT_ACTIVITY: &str = "a recovery scan or retitle must not count as opening";
    const OPEN_IS_ACTIVITY: &str = "opening a session must record last_opened_at";
    const ANSWER_CALL: &str = "question-call";
    const ANSWER_HEADER: &str = "Transfer channel";
    const ANSWER_QUESTION: &str = "How should bytes move?";
    const ANSWER_LABEL: &str = "Signed URLs";
    const ANSWER_DESCRIPTION: &str = "Mint short-lived URLs instead of returning bytes";
    const ANSWERS_EXPECTED: &str = "the transcript keeps the structured answers";
    const FORM_NOT_PERSISTED: &str = "the form lives in the tool call input, not the result";
    const FORM_COMES_FROM_INPUT: &str = "a bare answer is filled in from the input on restore";

    pub(crate) struct ResumeService {
        pub revision: AtomicUsize,
        pub requested: Mutex<Vec<(String, String)>>,
        source: &'static str,
    }

    impl ResumeService {
        fn path(
            &self,
            binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
        ) -> Result<String, WorkspaceError> {
            if cursor.binding_id() != binding.binding_id() {
                return Err(WorkspaceError::IdentityMismatch);
            }
            cursor
                .cwd_handle()
                .as_str()
                .strip_prefix(&format!("{}@", binding.binding_id().as_str()))
                .map(str::to_owned)
                .ok_or(WorkspaceError::StaleCursor)
        }

        fn asset(&self) -> ProjectAsset {
            ProjectAsset {
                path: WorkspacePath::new(".caudra/workflows/echo.rhai").unwrap(),
                resource_id: ResourceId::new("workflow-echo").unwrap(),
                revision: ResourceRevision::new(format!(
                    "revision-{}-{}",
                    self.source.len(),
                    self.revision.load(Ordering::SeqCst)
                ))
                .unwrap(),
                kind: ProjectAssetKind::Workflow,
                trust: ProjectAssetTrust::ClientApprovalRequired,
                size_bytes: self.source.len() as u64,
            }
        }
    }

    #[async_trait]
    impl WorkspaceReadService for ResumeService {
        async fn resolve_directory(
            &self,
            binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
            path: &WorkspacePath,
        ) -> Result<ResolvedWorkspaceDirectory, WorkspaceError> {
            let cwd = self.path(binding, cursor)?;
            self.requested
                .lock()
                .unwrap()
                .push((binding.binding_id().as_str().into(), path.to_string()));
            let logical = if path.is_root() {
                cwd
            } else if cwd == "." {
                path.to_string()
            } else {
                format!("{cwd}/{path}")
            };
            let scope = if path.is_root() {
                cursor.scope().clone()
            } else {
                let mut ancestors = cursor.scope().ancestors().to_vec();
                ancestors.push(cursor.scope().resource_id().clone());
                ResourceScope::new(
                    ancestors,
                    ResourceId::new(format!("resource:{logical}")).unwrap(),
                )
                .unwrap()
            };
            Ok(ResolvedWorkspaceDirectory {
                resource: WorkspaceResource {
                    project: binding.project().clone(),
                    scope: scope.clone(),
                    path: Some(WorkspacePath::new(&logical).unwrap()),
                    kind: ResourceKind::Directory,
                    revision: None,
                    size_bytes: None,
                },
                cursor: WorkspaceCursor::new(
                    binding,
                    scope,
                    cursor.generation(),
                    CwdHandle::new(format!("{}@{logical}", binding.binding_id().as_str())).unwrap(),
                ),
            })
        }
        async fn resolve(
            &self,
            binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
            path: &WorkspacePath,
        ) -> Result<WorkspaceResource, WorkspaceError> {
            Ok(self
                .resolve_directory(binding, cursor, path)
                .await?
                .resource)
        }
        async fn stat(
            &self,
            _: &SessionWorkspaceBinding,
            _: &WorkspaceCursor,
            _: &ResourceSelector,
        ) -> Result<WorkspaceResource, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }
        async fn list(
            &self,
            _: &SessionWorkspaceBinding,
            _: &WorkspaceCursor,
            _: &ListRequest,
        ) -> Result<ListPage, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
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
            _: &ReadBytesRequest,
        ) -> Result<ByteContent, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }
    }

    #[async_trait]
    impl WorkspaceAssetService for ResumeService {
        async fn discover(
            &self,
            binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
        ) -> Result<ProjectAssetManifest, WorkspaceError> {
            self.path(binding, cursor)?;
            Ok(ProjectAssetManifest {
                version: OperationId::new("project-assets.v1").unwrap(),
                revision: CollectionRevision::new(format!(
                    "revision-{}-{}",
                    self.source.len(),
                    self.revision.load(Ordering::SeqCst)
                ))
                .unwrap(),
                assets: if self.source.is_empty() {
                    Vec::new()
                } else {
                    vec![self.asset()]
                },
                unreadable: Vec::new(),
            })
        }
        async fn read(
            &self,
            binding: &SessionWorkspaceBinding,
            cursor: &WorkspaceCursor,
            asset: &ProjectAsset,
            _: u32,
        ) -> Result<ProjectAssetContent, WorkspaceError> {
            self.path(binding, cursor)?;
            Ok(ProjectAssetContent {
                asset: asset.clone(),
                content: self.source.into(),
                truncated: false,
            })
        }
    }

    pub(crate) fn remote_workspace(
        id: &str,
        source: &'static str,
    ) -> (WorkspaceSession, Arc<ResumeService>) {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("https://resume.example").unwrap(),
            "server",
            "workspace",
            "generation",
            "namespace",
        )
        .unwrap();
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new(id).unwrap(),
            authority.clone(),
            AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap(),
            ProjectIdentity::new(authority.clone(), ProjectKey::new("project").unwrap()),
        )
        .unwrap();
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("resource:.").unwrap()),
            1,
            CwdHandle::new(format!("{id}@.")).unwrap(),
        );
        let service = Arc::new(ResumeService {
            revision: AtomicUsize::new(1),
            requested: Mutex::new(Vec::new()),
            source,
        });
        let workspace = WorkspaceHandle::new(
            authority,
            Default::default(),
            WorkspaceServices {
                read: Some(service.clone()),
                assets: Some(service.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        (
            WorkspaceSession::new(workspace, binding, cursor).unwrap(),
            service,
        )
    }

    #[test_case(false)]
    #[test_case(true)]
    fn nested_resume_rebinds_fresh_handles_without_changing_durable_scope(in_steps: bool) {
        smol::block_on(async {
            let (mut old, _) = remote_workspace("old", "");
            let paths = if in_steps {
                vec!["a", "b"]
            } else {
                vec!["a/b"]
            };
            for path in paths {
                let directory = old
                    .workspace()
                    .services()
                    .read
                    .as_ref()
                    .unwrap()
                    .resolve_directory(
                        old.binding(),
                        old.cursor(),
                        &WorkspacePath::new(path).unwrap(),
                    )
                    .await
                    .unwrap();
                old = old.with_cursor(directory).unwrap();
            }
            let stored = StoredWorkspaceBinding::new_with_cursor(
                old.binding().clone(),
                old.cursor().clone(),
                None,
            )
            .unwrap();
            let (fresh, service) = remote_workspace("fresh", "");
            let (resumed, binding) = resolve_resume_workspace(Some(&stored), "a/b", &fresh)
                .await
                .unwrap();
            assert_eq!(resumed.cursor().scope(), old.cursor().scope());
            assert_ne!(resumed.cursor().cwd_handle(), old.cursor().cwd_handle());
            assert_eq!(binding.binding().binding_id(), fresh.binding().binding_id());
            let next = service
                .resolve_directory(
                    resumed.binding(),
                    resumed.cursor(),
                    &WorkspacePath::new("c").unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(next.resource.path.unwrap().as_str(), "a/b/c");
            assert!(
                service
                    .requested
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(id, _)| id == "fresh")
            );
        });
    }

    #[test_case("/client/canary")]
    #[test_case("../escape")]
    #[test_case("a/../../escape")]
    fn resume_refuses_nonlogical_paths_before_rpc(path: &str) {
        smol::block_on(async {
            let (workspace, service) = remote_workspace("fresh", "");
            let stored = StoredWorkspaceBinding::new_with_cursor(
                workspace.binding().clone(),
                workspace.cursor().clone(),
                None,
            )
            .unwrap();
            assert_eq!(
                resolve_resume_workspace(Some(&stored), path, &workspace)
                    .await
                    .unwrap_err(),
                RESUME_PATH_ERROR
            );
            assert!(service.requested.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn resume_rejects_a_changed_resource_at_the_persisted_path() {
        smol::block_on(async {
            let (workspace, _) = remote_workspace("old", "");
            let old = workspace
                .workspace()
                .services()
                .read
                .as_ref()
                .unwrap()
                .resolve_directory(
                    workspace.binding(),
                    workspace.cursor(),
                    &WorkspacePath::new("old-resource").unwrap(),
                )
                .await
                .unwrap();
            let stored = StoredWorkspaceBinding::new_with_cursor(
                workspace.binding().clone(),
                old.cursor,
                None,
            )
            .unwrap();
            let (fresh, _) = remote_workspace("fresh", "");
            assert_eq!(
                resolve_resume_workspace(Some(&stored), "replacement", &fresh)
                    .await
                    .unwrap_err(),
                RESUME_SCOPE_ERROR
            );
        });
    }

    #[test]
    fn resume_persists_the_fresh_binding_and_keeps_the_logical_path() {
        smol::block_on(async {
            let temp = TempDir::new().unwrap();
            let storage = StateDir::from_path(temp.path().into());
            let (workspace, _) = remote_workspace("old", "");
            let old = workspace
                .workspace()
                .services()
                .read
                .as_ref()
                .unwrap()
                .resolve_directory(
                    workspace.binding(),
                    workspace.cursor(),
                    &WorkspacePath::new("nested").unwrap(),
                )
                .await
                .unwrap();
            let binding = StoredWorkspaceBinding::new_with_cursor(
                workspace.binding().clone(),
                old.cursor,
                None,
            )
            .unwrap();
            let mut session = StoredSession::new_with_workspace(MODEL, "nested", binding);
            session.save(&storage).unwrap();
            let (fresh, _) = remote_workspace("fresh", "");
            let resumed = resume_workspace_session(&mut session, &fresh)
                .await
                .unwrap();
            session.save(&storage).unwrap();
            let loaded = StoredSession::load(session.id, &storage).unwrap();
            assert_eq!(loaded.cwd, "nested");
            assert_eq!(
                loaded.workspace_binding().unwrap().cursor(),
                Some(resumed.cursor())
            );
            assert_eq!(
                loaded.workspace_binding().unwrap().binding().binding_id(),
                fresh.binding().binding_id()
            );
        });
    }

    #[test]
    fn only_opening_a_session_counts_as_activity() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let mut session = StoredSession::new(MODEL, CWD);
        let id = session.id;
        session.save(&storage).unwrap();
        let last_opened_at = || {
            SessionDatabase::open(&storage)
                .unwrap()
                .session_facts(None)
                .unwrap()[0]
                .last_opened_at
        };

        load_stored_session(id, &storage).unwrap();
        assert_eq!(last_opened_at(), None, "{LOAD_IS_NOT_ACTIVITY}");

        open_stored_session(id, &storage).unwrap();
        assert!(last_opened_at().is_some(), "{OPEN_IS_ACTIVITY}");
    }

    fn native_file_index() -> ToolOutput {
        ToolOutput::Index(IndexOutput::File {
            path: "/repo/src/lib.rs".into(),
            relative_path: "src/lib.rs".into(),
            language: "rust".into(),
            skeleton: "fns:\n  pub run() [2]".into(),
            lines: vec![IndexLine {
                output_line: 2,
                text: "  pub run() [2]".into(),
                semantic: IndexLineSemantic::Item,
                body: Some("  pub run()".into()),
                source_range: Some(IndexSourceRange {
                    start_line: 2,
                    end_line: 2,
                }),
            }],
            source_line_count: 2,
            parse_error: false,
            truncated: false,
            instructions: None,
            state: Some(serde_json::json!({"kind": "file", "language": "rust"})),
        })
    }

    fn native_directory_index() -> ToolOutput {
        ToolOutput::Index(IndexOutput::Directory {
            path: "/repo".into(),
            relative_path: ".".into(),
            entries: vec![IndexDirectoryEntry {
                name: "src".into(),
                kind: IndexDirectoryEntryKind::Directory,
            }],
            total_count: 2,
            truncated: true,
            listing: "src/\n[truncated]".into(),
            instructions: None,
            state: Some(serde_json::json!({
                "kind": "directory",
                "listing": "src/",
                "truncated": true
            })),
        })
    }

    #[test_case(native_file_index() ; "file")]
    #[test_case(native_directory_index() ; "directory")]
    fn native_index_output_survives_persisted_session_roundtrip(output: ToolOutput) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let expected = serde_json::to_value(&output).unwrap();
        let mut session = StoredSession::new(MODEL, CWD);
        let id = session.id;
        session.insert_tool_output("index-call".into(), output);
        session.save(&storage).unwrap();

        let loaded = load_stored_session(id, &storage).unwrap();
        let actual = loaded.tool_outputs().get("index-call").unwrap();

        assert_eq!(serde_json::to_value(actual.as_ref()).unwrap(), expected);
        assert!(matches!(actual.as_ref(), ToolOutput::Index(_)));
    }

    #[test]
    fn native_shell_output_survives_persisted_session_roundtrip() {
        let output = ToolOutput::Shell(ShellOutput {
            model_text: "filtered\n\n[shell status: exit code 0]".into(),
            relative_workdir: ".".into(),
            timeout_ms: 120_000,
            duration_ms: 10,
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            output_limit_exceeded: false,
            final_sequence: 1,
            stdout_utf8_bytes: 3,
            stderr_utf8_bytes: 0,
            stdout: "raw".into(),
            stderr: String::new(),
            stdout_capture_truncated: false,
            stderr_capture_truncated: false,
            stdout_preview_truncated: false,
            stderr_preview_truncated: false,
            stdout_redraws_collapsed: 190,
            stderr_redraws_collapsed: 0,
            filter: Some(ShellFilterInfo {
                stages: vec!["make".into(), "progress".into()],
                unfiltered_utf8_bytes: 100,
                filtered_utf8_bytes: 20,
            }),
        });
        let expected = serde_json::to_value(&output).unwrap();
        let mut session = StoredSession::new(MODEL, CWD);
        let id = session.id;
        session.insert_tool_output("shell-call".into(), output);

        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        session.save(&storage).unwrap();
        let loaded = load_stored_session(id, &storage).unwrap();
        let actual = loaded.tool_outputs().get("shell-call").unwrap();

        assert_eq!(serde_json::to_value(actual.as_ref()).unwrap(), expected);
        assert_eq!(actual.as_text(), "filtered\n\n[shell status: exit code 0]");
    }

    fn answered_call(storage: &StateDir, output: ToolOutput) -> Arc<ToolOutput> {
        let mut session = StoredSession::new(MODEL, CWD);
        let id = session.id;
        session.insert_tool_output(ANSWER_CALL.into(), output);
        session.save(storage).unwrap();
        load_stored_session(id, storage)
            .unwrap()
            .tool_outputs()
            .get(ANSWER_CALL)
            .unwrap()
            .clone()
    }

    /// The form is the tool call's input, and an answer is restored against it.
    /// Persisting it a second time under the result would only let the two
    /// disagree, so the stored payload stays the picks alone.
    #[test]
    fn a_stored_answer_keeps_the_picks_and_not_the_form() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let output = ToolOutput::Answers(vec![Answer {
            header: ANSWER_HEADER.into(),
            labels: vec![ANSWER_LABEL.into()],
            question: ANSWER_QUESTION.into(),
            options: vec![QuestionOption {
                label: ANSWER_LABEL.into(),
                description: ANSWER_DESCRIPTION.into(),
            }],
        }]);

        let actual = answered_call(&storage, output);

        let ToolOutput::Answers(answers) = actual.as_ref() else {
            panic!("{ANSWERS_EXPECTED}");
        };
        assert_eq!(answers[0].header, ANSWER_HEADER);
        assert_eq!(answers[0].labels, [ANSWER_LABEL]);
        assert!(answers[0].question.is_empty(), "{FORM_NOT_PERSISTED}");
        assert!(answers[0].options.is_empty(), "{FORM_NOT_PERSISTED}");
        assert_eq!(
            serde_json::to_value(actual.as_ref()).unwrap(),
            serde_json::json!({ "Answers": [{ "header": ANSWER_HEADER, "labels": [ANSWER_LABEL] }] }),
            "{FORM_NOT_PERSISTED}"
        );
    }

    /// Every answer written before the card drew the form is stored this way,
    /// so the bare shape has to keep loading.
    #[test]
    fn an_answer_stored_without_its_form_still_loads() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let bare: ToolOutput = serde_json::from_value(
            serde_json::json!({ "Answers": [{ "header": ANSWER_HEADER, "labels": [ANSWER_LABEL] }] }),
        )
        .unwrap();

        let actual = answered_call(&storage, bare);

        let ToolOutput::Answers(answers) = actual.as_ref() else {
            panic!("{ANSWERS_EXPECTED}");
        };
        assert_eq!(answers[0].labels, [ANSWER_LABEL]);
        assert!(answers[0].question.is_empty(), "{FORM_COMES_FROM_INPUT}");
        assert!(answers[0].options.is_empty(), "{FORM_COMES_FROM_INPUT}");
    }
}
