use caudra_config::sandbox::TransferPolicy;
use caudra_workspace::{TransferDigest, WorkspacePath};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::{
    FileStamp, InventoryContext, InventoryNode, NodeKind, PAGE_SIZE, Side, TransferError,
    TransferPhase, WorkspaceTransfer, digest,
};
use crate::CancelToken;

const FILTER_VERSION: &str = "caudra-transfer-protected-v1";
const MAX_FILTERS: usize = 256;
const MAX_FILTER_BYTES: usize = 512;
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
const REPOSITORY_MARKERS: &[&str] = &[".git", ".hg", ".svn"];

pub(super) fn protected_component(component: &str) -> bool {
    let name = component.to_ascii_lowercase();
    name.starts_with(".env")
        || PROTECTED_NAMES.contains(&name.as_str())
        || PROTECTED_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

pub struct TransferFilters {
    excludes: GlobSet,
    digest: TransferDigest,
    respect_gitignore: bool,
    patterns: Vec<String>,
}

impl TransferFilters {
    pub fn new(
        profile: &TransferPolicy,
        global_excludes: &[String],
    ) -> Result<Self, TransferError> {
        if profile.delete_extraneous || profile.exclude.len() + global_excludes.len() > MAX_FILTERS
        {
            return Err(TransferError::Filter);
        }
        let mut patterns = profile
            .exclude
            .iter()
            .chain(global_excludes)
            .cloned()
            .collect::<Vec<_>>();
        patterns.sort();
        patterns.dedup();
        let mut builder = GlobSetBuilder::new();
        for pattern in &patterns {
            if pattern.is_empty()
                || pattern.len() > MAX_FILTER_BYTES
                || pattern.starts_with(['/', '!'])
                || pattern.contains(['\\', ':'])
                || pattern.chars().any(char::is_control)
                || pattern
                    .split('/')
                    .any(|part| matches!(part, ".." | "." | ""))
            {
                return Err(TransferError::Filter);
            }
            builder.add(
                GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .backslash_escape(false)
                    .build()
                    .map_err(|_| TransferError::Filter)?,
            );
        }
        Ok(Self {
            excludes: builder.build().map_err(|_| TransferError::Filter)?,
            digest: digest(&(FILTER_VERSION, &patterns, profile.respect_gitignore))?,
            respect_gitignore: profile.respect_gitignore,
            patterns,
        })
    }

    pub fn digest(&self) -> &TransferDigest {
        &self.digest
    }

    pub fn inventory_excludes(&self) -> &[String] {
        &self.patterns
    }
    pub fn respects_gitignore(&self) -> bool {
        self.respect_gitignore
    }

    pub fn excludes(&self, path: &WorkspacePath) -> bool {
        if path.is_root() {
            return false;
        }
        let mut prefix = String::new();
        for component in path.as_str().split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if protected_component(component)
                || self.excludes.is_match(&prefix)
                || self.excludes.is_match(format!("{prefix}/"))
            {
                return true;
            }
        }
        false
    }

    pub(super) fn ignored(&self, ignored: Option<bool>) -> Result<bool, TransferError> {
        if self.respect_gitignore {
            ignored.ok_or(TransferError::UnsafeInventory)
        } else {
            Ok(false)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ComparisonKind {
    Equal,
    LocalOnly,
    RemoteOnly,
    Conflict,
    Excluded,
    Unsupported,
    Incomplete,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComparisonRow {
    pub path: WorkspacePath,
    pub kind: ComparisonKind,
    pub local: Option<FileStamp>,
    pub remote: Option<FileStamp>,
    pub local_kind: Option<NodeKind>,
    pub remote_kind: Option<NodeKind>,
}

#[derive(Debug)]
pub struct Comparison {
    pub(super) context: InventoryContext,
    pub(super) filter_digest: TransferDigest,
    pub(super) local: Manifest,
    pub(super) remote: Manifest,
    pub(super) rows: Vec<ComparisonRow>,
}

impl Comparison {
    pub fn context(&self) -> &InventoryContext {
        &self.context
    }
    pub fn filter_digest(&self) -> &TransferDigest {
        &self.filter_digest
    }
    pub fn rows(&self) -> &[ComparisonRow] {
        &self.rows
    }
    pub fn complete(&self) -> bool {
        self.local.complete && self.remote.complete
    }
}

#[derive(Debug, Clone)]
pub(super) struct ManifestEntry {
    pub node: Option<InventoryNode>,
    pub file: Option<FileStamp>,
    pub blocked: Option<ComparisonKind>,
}

#[derive(Debug)]
pub(super) struct Manifest {
    pub entries: BTreeMap<WorkspacePath, ManifestEntry>,
    pub complete: bool,
}

impl Manifest {
    fn block(&mut self, path: WorkspacePath, reason: ComparisonKind) {
        if reason == ComparisonKind::Incomplete {
            self.complete = false;
        }
        self.entries
            .entry(path)
            .or_insert(ManifestEntry {
                node: None,
                file: None,
                blocked: None,
            })
            .blocked = Some(reason);
    }

    fn blocked(&self, path: &WorkspacePath) -> Option<ComparisonKind> {
        let mut ancestor = Some(path.clone());
        while let Some(path) = ancestor {
            if let Some(reason) = self
                .entries
                .get(&path)
                .and_then(|entry| entry.blocked.clone())
            {
                return Some(reason);
            }
            ancestor = path.parent();
        }
        None
    }
}

impl WorkspaceTransfer {
    pub async fn compare(&self, cancel: &CancelToken) -> Result<Comparison, TransferError> {
        let context = self.context(cancel).await?;
        self.bounded(cancel, self.services.authorization.roots(&context.roots))
            .await?;
        let local = self.scan(&context, Side::Local, cancel).await?;
        let remote = self.scan(&context, Side::Remote, cancel).await?;
        self.validate_context(&context, self.filters.digest(), cancel)
            .await?;
        let paths = local
            .entries
            .keys()
            .chain(remote.entries.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut rows = Vec::with_capacity(paths.len());
        for path in paths {
            let left = local.entries.get(&path);
            let right = remote.entries.get(&path);
            let mut kind = local.blocked(&path).or_else(|| remote.blocked(&path));
            if kind.is_none() {
                kind = Some(match (left, right) {
                    (Some(left), Some(right)) => {
                        if left.file.as_ref().map(|f| &f.content)
                            == right.file.as_ref().map(|f| &f.content)
                            && left.node.as_ref().map(|n| &n.kind)
                                == right.node.as_ref().map(|n| &n.kind)
                        {
                            ComparisonKind::Equal
                        } else {
                            ComparisonKind::Conflict
                        }
                    }
                    _ if !local.complete || !remote.complete => ComparisonKind::Incomplete,
                    (Some(_), None) => {
                        self.absent_kind(&Side::Remote, &path, ComparisonKind::LocalOnly, cancel)
                            .await?
                    }
                    (None, Some(_)) => {
                        self.absent_kind(&Side::Local, &path, ComparisonKind::RemoteOnly, cancel)
                            .await?
                    }
                    _ => ComparisonKind::Incomplete,
                });
            }
            rows.push(ComparisonRow {
                path,
                kind: kind.unwrap_or(ComparisonKind::Incomplete),
                local: left.and_then(|entry| entry.file.clone()),
                remote: right.and_then(|entry| entry.file.clone()),
                local_kind: left
                    .and_then(|entry| entry.node.as_ref().map(|node| node.kind.clone())),
                remote_kind: right
                    .and_then(|entry| entry.node.as_ref().map(|node| node.kind.clone())),
            });
        }
        self.validate_context(&context, self.filters.digest(), cancel)
            .await?;
        Ok(Comparison {
            context,
            filter_digest: self.filters.digest().clone(),
            local,
            remote,
            rows,
        })
    }

    async fn absent_kind(
        &self,
        side: &Side,
        path: &WorkspacePath,
        absent: ComparisonKind,
        cancel: &CancelToken,
    ) -> Result<ComparisonKind, TransferError> {
        match self
            .bounded(cancel, self.services.inventory.inspect(side, path))
            .await
        {
            Ok(inspection) => Ok(match self.filters.ignored(inspection.ignored) {
                Ok(true) => ComparisonKind::Excluded,
                Ok(false) if inspection.node.is_none() => absent,
                _ => ComparisonKind::Incomplete,
            }),
            Err(TransferError::Cancelled) => Err(TransferError::Cancelled),
            Err(_) => Ok(ComparisonKind::Incomplete),
        }
    }

    async fn scan(
        &self,
        context: &InventoryContext,
        side: Side,
        cancel: &CancelToken,
    ) -> Result<Manifest, TransferError> {
        let mut manifest = Manifest {
            entries: BTreeMap::new(),
            complete: true,
        };
        let safe = match side {
            Side::Local => context.safe_local_traversal,
            Side::Remote => context.safe_remote_traversal,
        };
        if !safe {
            manifest.complete = false;
            manifest.block(WorkspacePath::root(), ComparisonKind::Unsupported);
            return Ok(manifest);
        }
        let mut queue = VecDeque::from([(WorkspacePath::root(), 0)]);
        let mut pages = 0;
        let mut visited = 0;
        let mut hashed = 0_u64;
        while let Some((directory, depth)) = queue.pop_front() {
            self.phase(&directory, TransferPhase::Scanning);
            if depth >= self.limits.max_depth {
                manifest.block(directory, ComparisonKind::Incomplete);
                continue;
            }
            let mut continuation = None;
            let mut seen_cursors = BTreeSet::new();
            let mut revision = None;
            let mut children = BTreeMap::new();
            let mut complete = true;
            loop {
                if pages >= self.limits.max_pages || visited >= self.limits.max_entries {
                    complete = false;
                    break;
                }
                let limit = PAGE_SIZE.min((self.limits.max_entries - visited) as u32);
                pages += 1;
                let page = match self
                    .bounded(
                        cancel,
                        self.services
                            .inventory
                            .list(&side, &directory, continuation, limit),
                    )
                    .await
                {
                    Ok(page) => page,
                    Err(TransferError::Cancelled) => return Err(TransferError::Cancelled),
                    Err(_) => {
                        complete = false;
                        break;
                    }
                };
                if page.entries.len() > limit as usize
                    || revision.as_ref().is_some_and(|r| r != &page.revision)
                {
                    complete = false;
                    break;
                }
                revision = Some(page.revision);
                complete &= !page.incomplete;
                for node in page.entries {
                    visited += 1;
                    if node.path.parent().as_ref() != Some(&directory)
                        || children.contains_key(&node.path)
                    {
                        complete = false;
                        continue;
                    }
                    children.insert(node.path.clone(), node);
                }
                match page.next {
                    Some(next) if seen_cursors.insert(next.clone()) => continuation = Some(next),
                    Some(_) => {
                        complete = false;
                        break;
                    }
                    None => break,
                }
            }
            if !complete {
                manifest.block(directory.clone(), ComparisonKind::Incomplete);
            }
            if !directory.is_root()
                && children
                    .keys()
                    .any(|path| REPOSITORY_MARKERS.contains(&path.file_name()))
            {
                manifest.block(directory, ComparisonKind::Unsupported);
                continue;
            }
            for (path, node) in children {
                let mut entry = ManifestEntry {
                    node: Some(node.clone()),
                    file: None,
                    blocked: None,
                };
                entry.blocked = if self.filters.excludes(&path) {
                    Some(ComparisonKind::Excluded)
                } else {
                    match self.filters.ignored(node.ignored) {
                        Ok(true) => Some(ComparisonKind::Excluded),
                        Err(_) => Some(ComparisonKind::Incomplete),
                        Ok(false) => None,
                    }
                };
                if entry.blocked.is_none() {
                    match node.kind {
                        NodeKind::Directory if complete => {
                            queue.push_back((path.clone(), depth + 1))
                        }
                        NodeKind::Directory => entry.blocked = Some(ComparisonKind::Incomplete),
                        NodeKind::File => {
                            if let Some(size) = node.size_bytes
                                && size <= self.limits.max_file_bytes
                                && size <= self.limits.max_total_bytes.saturating_sub(hashed)
                                && complete
                            {
                                hashed += size;
                                match self.stamp(context, &side, &node, cancel).await {
                                    Ok(stamp) => entry.file = Some(stamp),
                                    Err(TransferError::Cancelled) => {
                                        return Err(TransferError::Cancelled);
                                    }
                                    Err(_) => entry.blocked = Some(ComparisonKind::Incomplete),
                                }
                            } else {
                                entry.blocked = Some(ComparisonKind::Incomplete);
                            }
                        }
                        _ => entry.blocked = Some(ComparisonKind::Unsupported),
                    }
                }
                if entry.blocked == Some(ComparisonKind::Incomplete) {
                    manifest.complete = false;
                }
                manifest.entries.insert(path, entry);
            }
        }
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::TransferFilters;
    use caudra_config::sandbox::TransferPolicy;
    use caudra_workspace::WorkspacePath;
    use test_case::test_case;

    #[test_case(".env")]
    #[test_case("a/.env.production")]
    #[test_case("a/.git/config")]
    #[test_case("a/.SSH/key")]
    #[test_case("a/credentials.toml")]
    #[test_case("a/key.pem")]
    #[test_case("a/.docker/config.json")]
    fn protected_rules_cannot_be_disabled(name: &str) {
        let policy = TransferPolicy {
            respect_gitignore: false,
            exclude: Vec::new(),
            ..TransferPolicy::default()
        };
        let filters = TransferFilters::new(&policy, &[]).unwrap();
        assert!(filters.excludes(&WorkspacePath::new(name).unwrap()));
    }

    #[test]
    fn profile_and_global_filters_bind_normalized_digest_and_ancestors() {
        let mut profile = TransferPolicy {
            exclude: vec!["cache/**".into(), "generated".into()],
            ..TransferPolicy::default()
        };
        let filters = TransferFilters::new(&profile, &["global/**".into()]).unwrap();
        for name in ["cache", "cache/file", "generated/file", "global/file"] {
            assert!(
                filters.excludes(&WorkspacePath::new(name).unwrap()),
                "{name}"
            );
        }
        profile.exclude.reverse();
        assert_eq!(
            filters.digest(),
            TransferFilters::new(&profile, &["global/**".into()])
                .unwrap()
                .digest()
        );
        assert_ne!(
            filters.digest(),
            TransferFilters::new(&profile, &[]).unwrap().digest()
        );
    }
}
