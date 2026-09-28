use caudra_config::sandbox::TransferPolicy;
use caudra_workspace::{TransferDigest, WorkspacePath};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::{
    FileStamp, InventoryContext, InventoryNode, NodeKind, PAGE_SIZE, Side, TransferError,
    TransferEvent, TransferPhase, WorkspaceTransfer, digest,
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
        self.exclusion(path).is_some()
    }

    pub fn exclusion(&self, path: &WorkspacePath) -> Option<ExclusionReason> {
        if path.is_root() {
            return None;
        }
        let mut prefix = String::new();
        for component in path.as_str().split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if protected_component(component) {
                return Some(ExclusionReason::Protected);
            }
            if self.excludes.is_match(&prefix) || self.excludes.is_match(format!("{prefix}/")) {
                return Some(ExclusionReason::Pattern);
            }
        }
        None
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ExclusionReason {
    Protected,
    Pattern,
    Gitignore,
}

/// Why one side's scan is partial. `Bytes`, `Unreadable` and a file that `Changed` while hashed
/// leave that file undetermined, and the other limits stop folders from being listed. Any limit
/// leaves the side partial, so entries present on the other side only stay undetermined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum ScanLimit {
    Entries,
    Pages,
    Depth,
    Bytes,
    /// Workcell truncated its snapshot without naming the folders it cut short, and refuses to
    /// inspect that side until a snapshot is whole.
    WorkcellIncomplete,
    ListingFailed,
    Changed,
    Unreadable,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScanState {
    pub unsupported: bool,
    pub limits: BTreeSet<ScanLimit>,
}

impl ScanState {
    pub fn complete(&self) -> bool {
        !self.unsupported && self.limits.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ComparisonRow {
    pub path: WorkspacePath,
    pub kind: ComparisonKind,
    pub local: Option<FileStamp>,
    pub remote: Option<FileStamp>,
    pub local_kind: Option<NodeKind>,
    pub remote_kind: Option<NodeKind>,
    pub excluded: Option<ExclusionReason>,
    /// A directory that a scan limit or listing error left unlisted or partly listed on at least
    /// one side. Its kind can still be `Equal`, and a `WorkcellIncomplete` side marks no folder.
    pub unlisted: bool,
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
        self.local.complete() && self.remote.complete()
    }
    pub fn scan(&self, side: &Side) -> &ScanState {
        match side {
            Side::Local => &self.local.scan,
            Side::Remote => &self.remote.scan,
        }
    }

    /// Fails early with an actionable error where the inventory would refuse the inspection.
    pub(super) fn ensure_inspectable(&self) -> Result<(), TransferError> {
        if [&self.local.scan, &self.remote.scan]
            .into_iter()
            .any(|scan| scan.limits.contains(&ScanLimit::WorkcellIncomplete))
        {
            return Err(TransferError::PartialInventory);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Block {
    Excluded(ExclusionReason),
    Unsupported,
    Incomplete,
}

impl Block {
    fn kind(self) -> ComparisonKind {
        match self {
            Self::Excluded(_) => ComparisonKind::Excluded,
            Self::Unsupported => ComparisonKind::Unsupported,
            Self::Incomplete => ComparisonKind::Incomplete,
        }
    }

    fn exclusion(self) -> Option<ExclusionReason> {
        match self {
            Self::Excluded(reason) => Some(reason),
            Self::Unsupported | Self::Incomplete => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ManifestEntry {
    pub node: InventoryNode,
    pub file: Option<FileStamp>,
    pub blocked: Option<Block>,
}

/// Never contains the root: a partial or unsupported side is described by `scan`.
#[derive(Debug, Default)]
pub(super) struct Manifest {
    pub entries: BTreeMap<WorkspacePath, ManifestEntry>,
    pub scan: ScanState,
    unlisted: BTreeSet<WorkspacePath>,
}

impl Manifest {
    pub fn complete(&self) -> bool {
        self.scan.complete()
    }

    fn unlist(&mut self, directory: WorkspacePath, limit: ScanLimit) {
        self.scan.limits.insert(limit);
        self.unlisted.insert(directory);
    }

    fn blocked(&self, path: &WorkspacePath) -> Option<Block> {
        if self.scan.unsupported {
            return Some(Block::Unsupported);
        }
        let mut ancestor = Some(path.clone());
        while let Some(path) = ancestor {
            if let Some(block) = self.entries.get(&path).and_then(|entry| entry.blocked) {
                return Some(block);
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
        let complete = local.complete() && remote.complete();
        let mut rows = Vec::with_capacity(paths.len());
        for path in paths {
            let left = local.entries.get(&path);
            let right = remote.entries.get(&path);
            let (kind, excluded) = match local.blocked(&path).or_else(|| remote.blocked(&path)) {
                Some(block) => (block.kind(), block.exclusion()),
                None => match (left, right) {
                    (Some(left), Some(right)) => {
                        let same = left.file.as_ref().map(|file| &file.content)
                            == right.file.as_ref().map(|file| &file.content)
                            && left.node.kind == right.node.kind;
                        let kind = if same {
                            ComparisonKind::Equal
                        } else {
                            ComparisonKind::Conflict
                        };
                        (kind, None)
                    }
                    _ if !complete => (ComparisonKind::Incomplete, None),
                    (Some(_), None) => {
                        self.absent_kind(&Side::Remote, &path, ComparisonKind::LocalOnly, cancel)
                            .await?
                    }
                    (None, Some(_)) => {
                        self.absent_kind(&Side::Local, &path, ComparisonKind::RemoteOnly, cancel)
                            .await?
                    }
                    (None, None) => (ComparisonKind::Incomplete, None),
                },
            };
            rows.push(ComparisonRow {
                unlisted: local.unlisted.contains(&path) || remote.unlisted.contains(&path),
                path,
                kind,
                local: left.and_then(|entry| entry.file.clone()),
                remote: right.and_then(|entry| entry.file.clone()),
                local_kind: left.map(|entry| entry.node.kind.clone()),
                remote_kind: right.map(|entry| entry.node.kind.clone()),
                excluded,
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
    ) -> Result<(ComparisonKind, Option<ExclusionReason>), TransferError> {
        match self
            .bounded(cancel, self.services.inventory.inspect(side, path))
            .await
        {
            Ok(inspection) => Ok(match self.filters.ignored(inspection.ignored) {
                Ok(true) => (ComparisonKind::Excluded, Some(ExclusionReason::Gitignore)),
                Ok(false) if inspection.node.is_none() => (absent, None),
                _ => (ComparisonKind::Incomplete, None),
            }),
            Err(TransferError::Cancelled) => Err(TransferError::Cancelled),
            Err(_) => Ok((ComparisonKind::Incomplete, None)),
        }
    }

    /// A partial listing is still descended and hashed: entries seen on both sides compare by
    /// content, while absence on a partial side stays undetermined.
    async fn scan(
        &self,
        context: &InventoryContext,
        side: Side,
        cancel: &CancelToken,
    ) -> Result<Manifest, TransferError> {
        let mut manifest = Manifest::default();
        let safe = match side {
            Side::Local => context.safe_local_traversal,
            Side::Remote => context.safe_remote_traversal,
        };
        if !safe {
            manifest.scan.unsupported = true;
            return Ok(manifest);
        }
        let mut queue = VecDeque::from([(WorkspacePath::root(), 0)]);
        let mut pages = 0;
        let mut visited = 0;
        let mut hashed = 0_u64;
        while let Some((directory, depth)) = queue.pop_front() {
            self.services.events.emit(TransferEvent::Phase {
                side: Some(side.clone()),
                path: directory.clone(),
                phase: TransferPhase::Scanning,
            });
            if depth >= self.limits.max_depth {
                manifest.unlist(directory, ScanLimit::Depth);
                continue;
            }
            let mut continuation = None;
            let mut seen_cursors = BTreeSet::new();
            let mut revision = None;
            let mut children = BTreeMap::new();
            let mut malformed = false;
            let unfinished = loop {
                if pages >= self.limits.max_pages {
                    break Some(ScanLimit::Pages);
                }
                if visited >= self.limits.max_entries {
                    break Some(ScanLimit::Entries);
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
                    Err(_) => break Some(ScanLimit::ListingFailed),
                };
                if page.entries.len() > limit as usize {
                    break Some(ScanLimit::ListingFailed);
                }
                if revision
                    .as_ref()
                    .is_some_and(|revision| revision != &page.revision)
                {
                    break Some(ScanLimit::Changed);
                }
                revision = Some(page.revision);
                if page.incomplete {
                    manifest.scan.limits.insert(ScanLimit::WorkcellIncomplete);
                }
                for node in page.entries {
                    visited += 1;
                    if node.path.parent().as_ref() != Some(&directory)
                        || children.contains_key(&node.path)
                    {
                        malformed = true;
                        continue;
                    }
                    children.insert(node.path.clone(), node);
                }
                match page.next {
                    Some(next) if seen_cursors.insert(next.clone()) => continuation = Some(next),
                    Some(_) => break Some(ScanLimit::ListingFailed),
                    None => break None,
                }
            };
            if !directory.is_root()
                && children
                    .keys()
                    .any(|path| REPOSITORY_MARKERS.contains(&path.file_name()))
            {
                if let Some(entry) = manifest.entries.get_mut(&directory) {
                    entry.node.kind = NodeKind::NestedRepository;
                    entry.blocked = Some(Block::Unsupported);
                }
                continue;
            }
            if malformed {
                manifest.unlist(directory.clone(), ScanLimit::ListingFailed);
            }
            if let Some(limit) = unfinished {
                manifest.unlist(directory.clone(), limit);
            }
            for (path, node) in children {
                let exclusion = match self.filters.exclusion(&path) {
                    Some(reason) => Ok(Some(reason)),
                    None => self
                        .filters
                        .ignored(node.ignored)
                        .map(|ignored| ignored.then_some(ExclusionReason::Gitignore)),
                };
                let mut entry = ManifestEntry {
                    node,
                    file: None,
                    blocked: None,
                };
                let limit = match exclusion {
                    Ok(Some(reason)) => {
                        entry.blocked = Some(Block::Excluded(reason));
                        None
                    }
                    Err(_) => {
                        if entry.node.kind == NodeKind::Directory {
                            manifest.unlisted.insert(path.clone());
                        }
                        Some(ScanLimit::ListingFailed)
                    }
                    Ok(None) => match entry.node.kind {
                        NodeKind::Directory => {
                            queue.push_back((path.clone(), depth + 1));
                            None
                        }
                        NodeKind::File => match entry.node.size_bytes {
                            Some(size)
                                if size <= self.limits.max_file_bytes
                                    && size
                                        <= self.limits.max_total_bytes.saturating_sub(hashed) =>
                            {
                                hashed += size;
                                match self.stamp(context, &side, &entry.node, cancel).await {
                                    Ok(stamp) => {
                                        entry.file = Some(stamp);
                                        None
                                    }
                                    Err(TransferError::Cancelled) => {
                                        return Err(TransferError::Cancelled);
                                    }
                                    Err(TransferError::Stale) => Some(ScanLimit::Changed),
                                    Err(_) => Some(ScanLimit::Unreadable),
                                }
                            }
                            Some(_) => Some(ScanLimit::Bytes),
                            None => Some(ScanLimit::Unreadable),
                        },
                        _ => {
                            entry.blocked = Some(Block::Unsupported);
                            None
                        }
                    },
                };
                if let Some(limit) = limit {
                    manifest.scan.limits.insert(limit);
                    entry.blocked = Some(Block::Incomplete);
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
