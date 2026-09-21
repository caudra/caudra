use std::{collections::BTreeMap, fs::File, io::Read, sync::Arc};

use rustix::fs::{AtFlags, Dir, OFlags, statat};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use workcell_host_contract::{
    ContractVersion, MAX_TRANSFER_DEPTH, MAX_TRANSFER_EXCLUDE_BYTES, MAX_TRANSFER_EXCLUDES,
    MAX_TRANSFER_INVENTORY_BYTES, MAX_TRANSFER_INVENTORY_ENTRIES, ResourceId, Revision,
    TransferInspection, TransferInventoryNode, TransferInventoryPolicy, TransferInventoryResponse,
    TransferNodeKind, WorkspacePath,
};

use crate::{
    BinaryError, FileToolGroup, FilesystemLimits, RootResourceKind,
    binary::{
        DIRECTORY_FLAGS, cancelled, digest_revision, identity, mutation_guard, open_child,
        open_root, regular_file, reject_repository, stamp,
    },
    gitignore::{IgnoreBudget, IgnoreScope, IgnoreScratch, compile_scope},
    glob::{GlobMatcher, MatchOutcome, MatchScratch},
    root_relative_resource_id,
};

const REPOSITORY_NAMES: &[&str] = &[".git", ".hg", ".svn"];
const ENVELOPE_RESERVATION: usize = 16 * 1024;
const EXCLUDE_STEPS: usize = 16 * 1024 * 1024;
const PROTECTED_NAMES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".ssh",
    ".aws",
    ".azure",
    ".gcloud",
    ".kube",
    ".gnupg",
    ".caudra",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".docker",
    ".git-credentials",
    "credentials",
    "credentials.json",
    "credentials.toml",
    "secrets",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_dsa",
];
const PROTECTED_SUFFIXES: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".keystore"];
type NodeMetadata = (TransferNodeKind, (Revision, Revision, Option<u64>));

struct Scan {
    root_prefix: String,
    entries: BTreeMap<String, TransferInventoryNode>,
    scopes: BTreeMap<String, Option<Arc<IgnoreScope>>>,
    ignores: Sha256,
    budget: IgnoreBudget,
    scratch: IgnoreScratch,
    bytes: usize,
    ignore_bytes: usize,
    ignore_files: usize,
    complete: bool,
    excludes: Vec<GlobMatcher>,
    exclude_steps: usize,
    exclude_scratch: MatchScratch,
    respect_gitignore: bool,
}

impl FileToolGroup {
    pub async fn transfer_inventory(
        &self,
        cwd: &ResourceId,
        inspect: Option<WorkspacePath>,
        policy: TransferInventoryPolicy,
        token: &CancellationToken,
    ) -> Result<TransferInventoryResponse, BinaryError> {
        let base = self
            .workspace_directory_path(cwd)
            .await
            .map_err(|_| BinaryError::Conflict)?;
        let base = if base == "." { String::new() } else { base };
        if base.split('/').count() > MAX_TRANSFER_DEPTH {
            return Err(BinaryError::Inaccessible);
        }
        let core = self.core.clone();
        if policy.excludes.len() > MAX_TRANSFER_EXCLUDES
            || policy
                .excludes
                .iter()
                .any(|p| p.len() > MAX_TRANSFER_EXCLUDE_BYTES)
        {
            return Err(BinaryError::Integrity);
        }
        let mut excludes = Vec::with_capacity(policy.excludes.len());
        let mut retained = 0usize;
        for pattern in &policy.excludes {
            cancelled(token)?;
            let matcher =
                GlobMatcher::new(pattern, &core.limits).map_err(|_| BinaryError::Integrity)?;
            retained = retained.saturating_add(matcher.retained_bytes());
            if retained > MAX_TRANSFER_INVENTORY_BYTES {
                return Err(BinaryError::Integrity);
            }
            excludes.push(matcher);
        }
        let token = token.clone();
        let guard = mutation_guard(&core, &token).await?;
        let result = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let mut root = open_root(core.root())?;
            // Probe the resolver even on empty trees; do not attest support on old kernels.
            let _probe =
                open_child(&root, ".", DIRECTORY_FLAGS).map_err(|_| BinaryError::Inaccessible)?;
            let root_identity = identity(&root.metadata().map_err(|_| BinaryError::Inaccessible)?);
            let mut scan = Scan {
                root_prefix: if base.is_empty() {
                    String::new()
                } else {
                    format!("{base}/")
                },
                entries: BTreeMap::new(),
                scopes: BTreeMap::new(),
                ignores: Sha256::new(),
                budget: IgnoreBudget::new(&core.limits),
                scratch: IgnoreScratch::default(),
                bytes: ENVELOPE_RESERVATION,
                ignore_bytes: 0,
                ignore_files: 0,
                complete: true,
                excludes,
                exclude_steps: EXCLUDE_STEPS,
                exclude_scratch: MatchScratch::default(),
                respect_gitignore: policy.respect_gitignore,
            };
            let mut prefix = String::new();
            let mut scope = None;
            let mut ancestors = Vec::new();
            for component in base.split('/').filter(|part| !part.is_empty()) {
                cancelled(&token)?;
                scope = scan.ignore_scope(&root, &prefix, scope, &core.limits, &token)?;
                scan.scopes.insert(prefix.clone(), scope.clone());
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                if scan.excluded(&prefix, component)?
                    || (scan.respect_gitignore
                        && scope
                            .as_ref()
                            .and_then(|scope| {
                                scope.decide(&prefix, true, &mut scan.budget, &mut scan.scratch)
                            })
                            .unwrap_or(false))
                {
                    return Err(BinaryError::Inaccessible);
                }
                root = open_child(&root, component, DIRECTORY_FLAGS)
                    .map_err(|_| BinaryError::Inaccessible)?;
                reject_repository(&root)?;
                ancestors.push(identity(
                    &root.metadata().map_err(|_| BinaryError::Inaccessible)?,
                ));
            }
            scan.directory(&root, &base, scope, false, 0, &core.limits, &token)?;
            let inspection = inspect
                .map(|path| {
                    let path = if base.is_empty() {
                        path
                    } else {
                        WorkspacePath::new(format!("{base}/{}", path.as_str()))
                            .map_err(|_| BinaryError::Inaccessible)?
                    };
                    scan.inspect(path)
                })
                .transpose()?;
            let mut current = open_root(core.root())?;
            if root_identity
                != identity(&current.metadata().map_err(|_| BinaryError::Inaccessible)?)
            {
                return Err(BinaryError::Conflict);
            }
            for (component, expected) in base
                .split('/')
                .filter(|part| !part.is_empty())
                .zip(ancestors)
            {
                current = open_child(&current, component, DIRECTORY_FLAGS)
                    .map_err(|_| BinaryError::Conflict)?;
                reject_repository(&current)?;
                if identity(&current.metadata().map_err(|_| BinaryError::Inaccessible)?) != expected
                {
                    return Err(BinaryError::Conflict);
                }
            }
            let relative = |path: &WorkspacePath| {
                WorkspacePath::new(
                    path.as_str()
                        .strip_prefix(&scan.root_prefix)
                        .ok_or(BinaryError::Conflict)?,
                )
                .map_err(|_| BinaryError::Conflict)
            };
            let entries = scan
                .entries
                .into_values()
                .map(|mut node| {
                    node.path = relative(&node.path)?;
                    Ok(node)
                })
                .collect::<Result<Vec<_>, BinaryError>>()?;
            let inspection = inspection
                .map(|mut inspected| {
                    inspected.path = relative(&inspected.path)?;
                    if let Some(node) = &mut inspected.node {
                        node.path = relative(&node.path)?;
                    }
                    Ok(inspected)
                })
                .transpose()?;
            let revision = digest_revision(Sha256::digest(
                serde_json::to_vec(&entries).map_err(|_| BinaryError::Integrity)?,
            ))?;
            Ok(TransferInventoryResponse {
                version: ContractVersion::V1,
                entries,
                revision,
                ignore_digest: digest_revision(scan.ignores.finalize())?,
                complete: scan.complete && scan.budget.complete,
                inspection,
            })
        })
        .await
        .map_err(|_| BinaryError::Inaccessible)??;
        self.workspace_directory_path(cwd)
            .await
            .map_err(|_| BinaryError::Conflict)?;
        Ok(result)
    }
}

impl Scan {
    #[allow(clippy::too_many_arguments)]
    fn directory(
        &mut self,
        directory: &File,
        prefix: &str,
        parent: Option<Arc<IgnoreScope>>,
        parent_ignored: bool,
        depth: usize,
        limits: &FilesystemLimits,
        token: &CancellationToken,
    ) -> Result<(), BinaryError> {
        cancelled(token)?;
        if depth >= MAX_TRANSFER_DEPTH {
            self.complete = false;
            return Ok(());
        }
        let before = stamp(
            &directory
                .metadata()
                .map_err(|_| BinaryError::Inaccessible)?,
        );
        let scope = self.ignore_scope(directory, prefix, parent, limits, token)?;
        self.scopes.insert(prefix.to_owned(), scope.clone());
        let reader = Dir::read_from(directory).map_err(|_| BinaryError::Inaccessible)?;
        let mut names = Vec::new();
        for entry in reader {
            cancelled(token)?;
            let entry = entry.map_err(|_| BinaryError::Inaccessible)?;
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| BinaryError::Inaccessible)?;
            if matches!(name, "." | "..") {
                continue;
            }
            if names.len() + self.entries.len() >= MAX_TRANSFER_INVENTORY_ENTRIES
                || self.bytes.saturating_add(name.len()) > MAX_TRANSFER_INVENTORY_BYTES
            {
                self.complete = false;
                break;
            }
            self.bytes += name.len();
            names.push(name.to_owned());
        }
        names.sort();
        for name in names {
            cancelled(token)?;
            if self.entries.len() >= MAX_TRANSFER_INVENTORY_ENTRIES {
                self.complete = false;
                break;
            }
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let path = WorkspacePath::new(&relative).map_err(|_| BinaryError::Inaccessible)?;
            let (kind, metadata) = node_metadata(directory, &name)?;
            let is_directory = matches!(
                kind,
                TransferNodeKind::Directory | TransferNodeKind::NestedRepository
            );
            let ignored = parent_ignored
                || scope
                    .as_ref()
                    .and_then(|scope| {
                        scope.decide(&relative, is_directory, &mut self.budget, &mut self.scratch)
                    })
                    .unwrap_or(false);
            let resource_id = if is_directory {
                ResourceId::new(format!("directory:{}", metadata.0.as_str()))
                    .map_err(|_| BinaryError::Inaccessible)?
            } else {
                root_relative_resource_id(RootResourceKind::Path, &relative)
                    .map_err(|_| BinaryError::Inaccessible)?
            };
            let node = TransferInventoryNode {
                path,
                resource_id,
                revision: metadata.1,
                kind: kind.clone(),
                size_bytes: metadata.2,
                ignored,
            };
            let size = serde_json::to_vec(&node)
                .map_err(|_| BinaryError::Integrity)?
                .len();
            if self.bytes.saturating_add(size) > MAX_TRANSFER_INVENTORY_BYTES {
                self.complete = false;
                break;
            }
            self.bytes += size;
            self.entries.insert(relative.clone(), node);
            if kind == TransferNodeKind::Directory
                && !REPOSITORY_NAMES.contains(&name.as_str())
                && !(ignored && self.respect_gitignore)
                && !self.excluded(&relative, &name)?
            {
                let child = open_child(directory, &name, DIRECTORY_FLAGS)
                    .map_err(|_| BinaryError::Inaccessible)?;
                reject_repository(&child)?;
                let reopened = digest_revision(Sha256::digest(format!(
                    "{:?}",
                    identity(&child.metadata().map_err(|_| BinaryError::Inaccessible)?)
                )))?;
                if reopened != metadata.0 {
                    return Err(BinaryError::Conflict);
                }
                self.directory(
                    &child,
                    &relative,
                    scope.clone(),
                    ignored,
                    depth + 1,
                    limits,
                    token,
                )?;
            }
        }
        if before
            != stamp(
                &directory
                    .metadata()
                    .map_err(|_| BinaryError::Inaccessible)?,
            )
        {
            self.complete = false;
        }
        Ok(())
    }

    fn excluded(&mut self, relative: &str, name: &str) -> Result<bool, BinaryError> {
        let relative = relative.strip_prefix(&self.root_prefix).unwrap_or(relative);
        let name = name.to_ascii_lowercase();
        if name.starts_with(".env")
            || PROTECTED_NAMES.contains(&name.as_str())
            || PROTECTED_SUFFIXES
                .iter()
                .any(|suffix| name.ends_with(suffix))
        {
            return Ok(true);
        }
        for pattern in &self.excludes {
            for value in [relative.to_owned(), format!("{relative}/")] {
                match pattern
                    .try_match(&value, &mut self.exclude_steps, &mut self.exclude_scratch)
                    .map_err(|_| BinaryError::Integrity)?
                {
                    MatchOutcome::Matched => return Ok(true),
                    MatchOutcome::Missed => (),
                    MatchOutcome::BudgetExhausted => {
                        self.complete = false;
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    fn ignore_scope(
        &mut self,
        directory: &File,
        prefix: &str,
        parent: Option<Arc<IgnoreScope>>,
        limits: &FilesystemLimits,
        token: &CancellationToken,
    ) -> Result<Option<Arc<IgnoreScope>>, BinaryError> {
        match statat(directory, ".gitignore", AtFlags::SYMLINK_NOFOLLOW) {
            Err(Errno::NOENT) => Ok(parent),
            Err(_) => Err(BinaryError::Inaccessible),
            Ok(_) => {
                let file = regular_file(directory, ".gitignore")?;
                let before = file.metadata().map_err(|_| BinaryError::Inaccessible)?;
                if !before.is_file()
                    || before.len() > limits.max_gitignore_bytes as u64
                    || self.ignore_files >= limits.max_gitignore_files
                    || self.ignore_bytes.saturating_add(before.len() as usize)
                        > limits.max_gitignore_retained_bytes
                {
                    self.complete = false;
                    return Ok(parent);
                }
                cancelled(token)?;
                let mut bytes = Vec::new();
                (&file)
                    .take(limits.max_gitignore_bytes as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| BinaryError::Inaccessible)?;
                if bytes.len() > limits.max_gitignore_bytes
                    || stamp(&before)
                        != stamp(&file.metadata().map_err(|_| BinaryError::Inaccessible)?)
                {
                    return Err(BinaryError::Conflict);
                }
                let text = String::from_utf8(bytes).map_err(|_| BinaryError::Inaccessible)?;
                self.ignore_files += 1;
                self.ignore_bytes += text.len();
                self.ignores.update((prefix.len() as u64).to_le_bytes());
                self.ignores.update(prefix);
                self.ignores.update((text.len() as u64).to_le_bytes());
                self.ignores.update(&text);
                Ok(compile_scope(
                    &text,
                    prefix,
                    parent,
                    limits,
                    &mut self.budget,
                ))
            }
        }
    }

    fn inspect(&mut self, path: WorkspacePath) -> Result<TransferInspection, BinaryError> {
        if !self.complete || !self.budget.complete || path.as_str() == "." {
            return Err(BinaryError::Inaccessible);
        }
        let parts = path.as_str().split('/').collect::<Vec<_>>();
        let mut prefix = String::new();
        let mut scope = self.scopes.get("").cloned().flatten();
        let mut ignored = false;
        for (index, part) in parts.iter().enumerate() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if self.excluded(&prefix, part)? {
                return Err(BinaryError::Inaccessible);
            }
            let node = self.entries.get(&prefix);
            let directory = index + 1 < parts.len()
                || node.is_none_or(|n| n.kind == TransferNodeKind::Directory);
            if index + 1 < parts.len()
                && node.is_some_and(|n| n.kind != TransferNodeKind::Directory)
            {
                return Err(BinaryError::Inaccessible);
            }
            ignored |= node.is_some_and(|n| n.ignored)
                || scope
                    .as_ref()
                    .and_then(|s| s.decide(&prefix, directory, &mut self.budget, &mut self.scratch))
                    .unwrap_or(false);
            if let Some(current) = self.scopes.get(&prefix) {
                scope = current.clone();
            }
        }
        if !self.budget.complete {
            return Err(BinaryError::Inaccessible);
        }
        Ok(TransferInspection {
            node: self.entries.get(path.as_str()).cloned(),
            path,
            ignored,
        })
    }
}

fn node_metadata(parent: &File, name: &str) -> Result<NodeMetadata, BinaryError> {
    #[cfg(target_os = "linux")]
    let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = DIRECTORY_FLAGS;
    let file = match open_child(parent, name, flags) {
        Ok(file) => file,
        Err(Errno::XDEV) => {
            let revision = digest_revision(Sha256::digest(name))?;
            return Ok((TransferNodeKind::Mount, (revision.clone(), revision, None)));
        }
        Err(_) => return Err(BinaryError::Inaccessible),
    };
    let metadata = file.metadata().map_err(|_| BinaryError::Inaccessible)?;
    let kind = if metadata.is_symlink() {
        TransferNodeKind::Symlink
    } else if metadata.is_dir() {
        if reject_repository(&file).is_err() {
            TransferNodeKind::NestedRepository
        } else {
            TransferNodeKind::Directory
        }
    } else if metadata.is_file() {
        TransferNodeKind::File
    } else {
        TransferNodeKind::Special
    };
    Ok((
        kind,
        (
            digest_revision(Sha256::digest(format!("{:?}", identity(&metadata))))?,
            digest_revision(Sha256::digest(format!("{:?}", stamp(&metadata))))?,
            metadata.is_file().then_some(metadata.len()),
        ),
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{
        FileToolGroup, MAX_TRANSFER_INVENTORY_ENTRIES, TransferInventoryPolicy, TransferNodeKind,
        WorkspacePath,
    };
    use crate::BinaryError;
    use crate::binary::{DIRECTORY_FLAGS, open_child, open_root};
    use rustix::fs::{AtFlags, StatxFlags, statx};
    use rustix::io::Errno;
    use std::{
        fs,
        os::unix::fs::{MetadataExt, symlink},
    };
    use tokio_util::sync::CancellationToken;
    use workcell_host_contract::DirectoryNavigation;

    #[tokio::test]
    async fn nested_cursor_inventory_keeps_ancestor_ignores_and_rooted_resource_ids() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("outer/sub/cache")).unwrap();
        fs::write(
            root.path().join(".gitignore"),
            "/outer/sub/ancestor-ignored\n",
        )
        .unwrap();
        fs::write(root.path().join("outer/.gitignore"), "sub/missing/**\n").unwrap();
        fs::write(root.path().join("outer/sub/.gitignore"), "own-ignored\n").unwrap();
        for path in [
            "outer/sub/ok",
            "outer/sub/ancestor-ignored",
            "outer/sub/own-ignored",
            "outer/sub/cache/no",
            "outside",
        ] {
            fs::write(root.path().join(path), b"contents").unwrap();
        }
        symlink(
            root.path().join("outside"),
            root.path().join("outer/sub/link"),
        )
        .unwrap();
        let files = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let root_handle = files.workspace_root().await.unwrap().handle;
        let cwd = files
            .workspace_resolve_directory(
                &root_handle,
                &DirectoryNavigation::new("outer/sub").unwrap(),
            )
            .await
            .unwrap()
            .handle;
        let policy = TransferInventoryPolicy {
            excludes: vec!["cache/**".into()],
            respect_gitignore: true,
        };
        let token = CancellationToken::new();
        let snapshot = files
            .transfer_inventory(
                &cwd,
                Some(WorkspacePath::new("missing/new").unwrap()),
                policy.clone(),
                &token,
            )
            .await
            .unwrap();
        assert!(snapshot.complete);
        assert!(snapshot.inspection.unwrap().ignored);
        assert!(
            !snapshot
                .entries
                .iter()
                .any(|node| node.path.as_str().starts_with("outer/")
                    || node.path.as_str() == "outside"
                    || node.path.as_str() == "cache/no")
        );
        for path in ["ancestor-ignored", "own-ignored"] {
            assert!(
                snapshot
                    .entries
                    .iter()
                    .find(|node| node.path.as_str() == path)
                    .unwrap()
                    .ignored
            );
        }
        let file = files
            .open_binary(&cwd, &WorkspacePath::new("ok").unwrap(), 1024, &token)
            .await
            .unwrap();
        assert_eq!(
            snapshot
                .entries
                .iter()
                .find(|node| node.path.as_str() == "ok")
                .unwrap()
                .resource_id,
            file.metadata.resource_id
        );
        assert_eq!(
            snapshot
                .entries
                .iter()
                .find(|node| node.path.as_str() == "link")
                .unwrap()
                .kind,
            TransferNodeKind::Symlink
        );
        fs::write(root.path().join(".gitignore"), "/outer/sub/another\n").unwrap();
        let changed = files
            .transfer_inventory(&cwd, None, policy, &token)
            .await
            .unwrap();
        assert_ne!(snapshot.ignore_digest, changed.ignore_digest);
    }

    #[tokio::test]
    async fn explicit_subroot_cannot_bypass_a_protected_or_repository_ancestor() {
        let root = tempfile::tempdir().unwrap();
        for path in [".ssh/sub", "repo/sub", "ignored/sub"] {
            fs::create_dir_all(root.path().join(path)).unwrap();
        }
        fs::write(root.path().join("repo/.git"), "gitdir: /outside").unwrap();
        fs::write(root.path().join(".gitignore"), "ignored/\n").unwrap();
        let files = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let root_handle = files.workspace_root().await.unwrap().handle;
        for path in [".ssh/sub", "repo/sub", "ignored/sub"] {
            match files
                .workspace_resolve_directory(&root_handle, &DirectoryNavigation::new(path).unwrap())
                .await
            {
                Ok(cwd) => assert!(
                    files
                        .transfer_inventory(
                            &cwd.handle,
                            None,
                            TransferInventoryPolicy {
                                excludes: vec![],
                                respect_gitignore: true
                            },
                            &CancellationToken::new()
                        )
                        .await
                        .is_err()
                ),
                Err(_) => assert_eq!(path, ".ssh/sub"),
            }
        }
    }

    #[tokio::test]
    async fn inventory_prunes_boundaries_and_filters_without_claiming_a_snapshot() {
        let root = tempfile::tempdir().unwrap();
        for directory in ["src", "other-repo", "ignored", "target", ".SSH"] {
            fs::create_dir(root.path().join(directory)).unwrap();
        }
        fs::write(root.path().join("other-repo/.git"), "gitdir: /outside").unwrap();
        fs::write(
            root.path().join(".gitignore"),
            "ignored/\nmissing/\n*.secret\n",
        )
        .unwrap();
        fs::write(root.path().join("src/.gitignore"), "hidden.txt\n").unwrap();
        for name in [
            "src/ok",
            "src/hidden.txt",
            "ignored/no",
            "other-repo/no",
            "target/no",
            ".SSH/no",
        ] {
            fs::write(root.path().join(name), b"content").unwrap();
        }
        symlink("/etc/passwd", root.path().join("link")).unwrap();
        let files = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let cwd = files.workspace_root().await.unwrap().handle;
        let policy = TransferInventoryPolicy {
            excludes: vec!["**/target/**".into()],
            respect_gitignore: true,
        };
        let snapshot = files
            .transfer_inventory(
                &cwd,
                Some(WorkspacePath::new("missing/deep/file").unwrap()),
                policy.clone(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(snapshot.complete);
        assert!(snapshot.inspection.unwrap().ignored);
        let find = |path: &str| {
            snapshot
                .entries
                .iter()
                .find(|node| node.path.as_str() == path)
                .unwrap()
        };
        assert_eq!(find("link").kind, TransferNodeKind::Symlink);
        assert_eq!(find("other-repo").kind, TransferNodeKind::NestedRepository);
        assert!(find("src/hidden.txt").ignored);
        for path in ["ignored/no", "other-repo/no", "target/no", ".SSH/no"] {
            assert!(
                !snapshot
                    .entries
                    .iter()
                    .any(|entry| entry.path.as_str() == path)
            );
        }
        assert!(
            files
                .open_binary(
                    &cwd,
                    &WorkspacePath::new("other-repo/no").unwrap(),
                    100,
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
        let before = snapshot.ignore_digest;
        fs::write(root.path().join("src/.gitignore"), "different\n").unwrap();
        let next = files
            .transfer_inventory(&cwd, None, policy.clone(), &CancellationToken::new())
            .await
            .unwrap();
        assert_ne!(before, next.ignore_digest);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            files.transfer_inventory(&cwd, None, policy, &cancel).await,
            Err(BinaryError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn incomplete_enumeration_and_unsafe_ignore_files_never_establish_absence() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=MAX_TRANSFER_INVENTORY_ENTRIES {
            fs::write(root.path().join(format!("file-{index}")), b"").unwrap();
        }
        let files = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let cwd = files.workspace_root().await.unwrap().handle;
        let result = files
            .transfer_inventory(
                &cwd,
                None,
                TransferInventoryPolicy::default(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!result.complete);
        assert!(result.entries.len() <= MAX_TRANSFER_INVENTORY_ENTRIES);
        assert!(
            files
                .transfer_inventory(
                    &cwd,
                    Some(WorkspacePath::new("absent").unwrap()),
                    TransferInventoryPolicy::default(),
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
        symlink("/etc/passwd", root.path().join(".gitignore")).unwrap();
        assert!(
            files
                .transfer_inventory(
                    &cwd,
                    None,
                    TransferInventoryPolicy::default(),
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn real_existing_mounts_are_classified_without_traversal_or_special_file_reads() {
        let files = FileToolGroup::new("/dev", false, None).await.unwrap();
        let cwd = files.workspace_root().await.unwrap().handle;
        let snapshot = files
            .transfer_inventory(
                &cwd,
                None,
                TransferInventoryPolicy::default(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            snapshot
                .entries
                .iter()
                .any(|entry| entry.path.as_str() == "pts" && entry.kind == TransferNodeKind::Mount)
        );
        assert!(
            !snapshot
                .entries
                .iter()
                .any(|entry| entry.path.as_str().starts_with("pts/"))
        );
        assert!(
            files
                .open_binary(
                    &cwd,
                    &WorkspacePath::new("null").unwrap(),
                    100,
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert!(
            files
                .open_binary(
                    &cwd,
                    &WorkspacePath::new("shm/file").unwrap(),
                    100,
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
    }

    #[test]
    fn same_device_proc_bind_fixture_is_not_treated_as_an_ordinary_directory() {
        let root = open_root(std::path::Path::new("/proc")).unwrap();
        let parent = statx(&root, ".", AtFlags::empty(), StatxFlags::MNT_ID).unwrap();
        let child = statx(&root, "sys", AtFlags::empty(), StatxFlags::MNT_ID).unwrap();
        if parent.stx_mnt_id == child.stx_mnt_id {
            // Bare hosts need not bind /proc/sys; the container fixture does. Never create a mount.
            return;
        }
        assert_eq!(
            fs::metadata("/proc").unwrap().dev(),
            fs::metadata("/proc/sys").unwrap().dev()
        );
        assert_eq!(
            open_child(&root, "sys", DIRECTORY_FLAGS).unwrap_err(),
            Errno::XDEV
        );
    }
}
