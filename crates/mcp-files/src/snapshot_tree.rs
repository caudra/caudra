//! Descriptor-relative traversal and publication for workspace snapshots.
//!
//! Every name is resolved beneath an already open directory with `RESOLVE_BENEATH |
//! RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV`, so a capture never follows a link or crosses a mount and
//! a restore never writes through one, however the tree changes while either runs. A symlink is
//! data: the walk reports the link itself and a restore recreates the link itself.
//!
//! Only per-directory `.gitignore` files are honoured, for the reasons [`crate::gitignore`] gives.
//! A directory holding its own repository is reported and never entered: it is a different
//! worktree, to git and to a snapshot alike. Nothing here refuses a whole tree because of one entry
//! it cannot capture; that entry is yielded as a skip with its reason instead.

#[cfg(unix)]
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    sync::Arc,
};
use std::{
    fs::{File, Metadata},
    io::{self, Read},
};

#[cfg(unix)]
use rustix::{
    fs::{
        AtFlags, Dir, Mode, OFlags, fchmod, linkat, mkdirat, openat, readlinkat, renameat,
        symlinkat, unlinkat,
    },
    io::Errno,
};
use tokio_util::sync::CancellationToken;
#[cfg(unix)]
use uuid::Uuid;
#[cfg(unix)]
use workcell_host_contract::MAX_SNAPSHOT_DEPTH;
use workcell_host_contract::{SnapshotSkipReason, WorkspacePath};

use crate::WorkspaceSnapshotAccess;
#[cfg(unix)]
use crate::{
    binary::{
        DIRECTORY_FLAGS, PublicationTemporary, open_child, open_root, reject_repository,
        reopen_regular,
    },
    gitignore::{IgnoreBudget, IgnoreScope, IgnoreScratch, admit_scope},
    operations::FilesystemCore,
};

#[cfg(unix)]
const NODE_FLAGS: OFlags = OFlags::PATH.union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
#[cfg(unix)]
const STAGED_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
#[cfg(unix)]
const STAGED_MODE: u32 = 0o600;
#[cfg(unix)]
const DIRECTORY_MODE: u32 = 0o777;
#[cfg(unix)]
const PERMISSION_BITS: u32 = 0o777;
#[cfg(unix)]
const GITIGNORE: &str = ".gitignore";

/// Traversal ceilings. Reaching one refuses the walk; it is never silently truncated.
#[derive(Clone, Debug)]
pub struct SnapshotTreeLimits {
    pub max_entries: usize,
    pub max_path_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotTreeLimit {
    Entries,
    PathBytes,
    Depth,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotTreeError {
    #[error("snapshot traversal exceeded its {limit:?} limit of {maximum}")]
    LimitExceeded {
        limit: SnapshotTreeLimit,
        maximum: u64,
    },
    #[error("the workspace's .gitignore rules exceed the snapshot evaluation budget")]
    IgnoreRulesExceeded,
    #[error("snapshot scope is not a plain directory inside the workspace")]
    ScopeUnavailable,
    #[error("path is protected by filesystem policy")]
    Protected,
    #[error("an ancestor of the path is not a plain directory on the workspace filesystem")]
    Blocked,
    #[error("the entry changed after it was observed")]
    Changed,
    #[error("descriptor-relative snapshot traversal is unsupported on this platform")]
    Unsupported,
    #[error("snapshot traversal was cancelled")]
    Cancelled,
    #[error("snapshot filesystem operation failed: {0}")]
    Failed(#[from] io::Error),
    /// The change is in place but could not be made durable or its staging entry removed.
    #[error("snapshot change was published but not settled: {0}")]
    Unsettled(io::Error),
}

/// Identity, size, type and permission bits, and both timestamps of one inode. Equal stamps mean
/// the entry was neither replaced nor written in between.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotTreeStamp {
    identity: (u64, u64),
    size: u64,
    mode: u32,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl SnapshotTreeStamp {
    #[cfg(unix)]
    #[must_use]
    pub fn of(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            identity: (metadata.dev(), metadata.ino()),
            size: metadata.len(),
            mode: metadata.mode(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }

    #[cfg(not(unix))]
    #[must_use]
    pub fn of(metadata: &Metadata) -> Self {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or((0, 0), |duration| {
                (
                    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
                    i64::from(duration.subsec_nanos()),
                )
            });
        Self {
            identity: (0, 0),
            size: metadata.len(),
            mode: u32::from(metadata.permissions().readonly()),
            modified,
            changed: modified,
        }
    }
}

pub struct SnapshotTreeEntry {
    /// Root-relative and `/`-separated. Lossy only for [`SnapshotSkipReason::Unrepresentable`].
    pub path: String,
    pub node: SnapshotTreeNode,
}

pub enum SnapshotTreeNode {
    File(SnapshotTreeFile),
    Symlink(SnapshotTreeLink),
    Skipped(SnapshotSkipReason),
}

/// An open regular inode and its metadata when opened. Compare [`SnapshotTreeStamp::of`] its
/// metadata after reading to know the bytes read were stable.
pub struct SnapshotTreeFile {
    pub file: File,
    pub metadata: Metadata,
}

pub struct SnapshotTreeLink {
    pub target: Vec<u8>,
    pub stamp: SnapshotTreeStamp,
}

pub enum SnapshotTreeObserved {
    Absent,
    File(SnapshotTreeFile),
    Symlink(SnapshotTreeLink),
    Directory,
    /// A special file or a mount point, neither of which a snapshot ever holds.
    Other,
}

pub enum SnapshotTreeExpected {
    Absent,
    Present(SnapshotTreeStamp),
}

pub enum SnapshotTreeContent<'a> {
    /// Staging stops at the first read error, so a source that verifies its own digest at the end
    /// of input keeps unverified bytes from ever being published.
    File {
        source: &'a mut dyn Read,
        mode: u32,
    },
    Symlink {
        target: &'a [u8],
    },
    Absent,
}

/// Depth-first, in byte order of names within each directory.
pub struct SnapshotTreeWalk {
    #[cfg(unix)]
    state: WalkState,
}

impl Iterator for SnapshotTreeWalk {
    type Item = Result<SnapshotTreeEntry, SnapshotTreeError>;

    #[cfg(unix)]
    fn next(&mut self) -> Option<Self::Item> {
        self.state.next_entry()
    }

    #[cfg(not(unix))]
    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

impl WorkspaceSnapshotAccess {
    /// Walks `scope`, a root-relative directory. Protected paths, `exclusions` (root-relative
    /// prefixes) and ignored paths are left out silently; everything else is yielded, if only as a
    /// skip with its reason. Blocking: run it off the async executor.
    #[cfg(unix)]
    pub fn walk_tree(
        &self,
        scope: &str,
        exclusions: Vec<String>,
        limits: SnapshotTreeLimits,
        token: CancellationToken,
    ) -> Result<SnapshotTreeWalk, SnapshotTreeError> {
        WalkState::open(self.core.clone(), scope, exclusions, limits, token)
            .map(|state| SnapshotTreeWalk { state })
    }

    /// The entry at `path` without following it, or where an ancestor blocks reaching it.
    #[cfg(unix)]
    pub fn observe_tree_entry(
        &self,
        path: &WorkspacePath,
    ) -> Result<SnapshotTreeObserved, SnapshotTreeError> {
        let Some((parent, name)) = tree_parent(&self.core, path)? else {
            return Ok(SnapshotTreeObserved::Absent);
        };
        observe_child(&parent, name)
    }

    /// Creates, replaces or removes the entry at `path`, provided it still matches `expected` and
    /// its parent is a plain directory. New content is staged and synced beside it first.
    #[cfg(unix)]
    pub fn publish_tree_entry(
        &self,
        path: &WorkspacePath,
        expected: &SnapshotTreeExpected,
        content: SnapshotTreeContent<'_>,
    ) -> Result<(), SnapshotTreeError> {
        let (parent, name) = tree_parent(&self.core, path)?.ok_or(SnapshotTreeError::Blocked)?;
        let staged_name = format!(".workcell-restore-{}.tmp", Uuid::new_v4().simple());
        let staged = PublicationTemporary {
            parent: &parent,
            name: &staged_name,
        };
        let replacing = match content {
            SnapshotTreeContent::File { source, mode } => {
                stage_file(&parent, &staged_name, source, mode)?;
                true
            }
            SnapshotTreeContent::Symlink { target } => {
                symlinkat(OsStr::from_bytes(target), &parent, staged_name.as_str())
                    .map_err(io::Error::from)?;
                true
            }
            SnapshotTreeContent::Absent => false,
        };
        verify_expected(&parent, name, expected)?;
        match (replacing, expected) {
            (true, SnapshotTreeExpected::Absent) => {
                linkat(
                    &parent,
                    staged_name.as_str(),
                    &parent,
                    name,
                    AtFlags::empty(),
                )
                .map_err(changed_or_failed)?;
            }
            (true, SnapshotTreeExpected::Present(_)) => {
                renameat(&parent, staged_name.as_str(), &parent, name)
                    .map_err(changed_or_failed)?;
            }
            (false, SnapshotTreeExpected::Present(_)) => {
                unlinkat(&parent, name, AtFlags::empty()).map_err(changed_or_failed)?;
            }
            (false, SnapshotTreeExpected::Absent) => return Ok(()),
        }
        staged
            .remove()
            .map_err(io::Error::other)
            .and_then(|()| parent.sync_all())
            .map_err(SnapshotTreeError::Unsettled)
    }

    /// Creates the directory at `path` beneath an existing plain parent. An existing plain
    /// directory is success.
    #[cfg(unix)]
    pub fn create_tree_directory(&self, path: &WorkspacePath) -> Result<(), SnapshotTreeError> {
        let (parent, name) = tree_parent(&self.core, path)?.ok_or(SnapshotTreeError::Blocked)?;
        match mkdirat(&parent, name, Mode::from_raw_mode(DIRECTORY_MODE)) {
            Ok(()) => parent.sync_all().map_err(SnapshotTreeError::Unsettled),
            Err(Errno::EXIST) => open_child(&parent, name, DIRECTORY_FLAGS)
                .map(drop)
                .map_err(|_| SnapshotTreeError::Blocked),
            Err(errno) => Err(io::Error::from(errno).into()),
        }
    }

    #[cfg(not(unix))]
    pub fn walk_tree(
        &self,
        _scope: &str,
        _exclusions: Vec<String>,
        _limits: SnapshotTreeLimits,
        _token: CancellationToken,
    ) -> Result<SnapshotTreeWalk, SnapshotTreeError> {
        Err(SnapshotTreeError::Unsupported)
    }

    #[cfg(not(unix))]
    pub fn observe_tree_entry(
        &self,
        _path: &WorkspacePath,
    ) -> Result<SnapshotTreeObserved, SnapshotTreeError> {
        Err(SnapshotTreeError::Unsupported)
    }

    #[cfg(not(unix))]
    pub fn publish_tree_entry(
        &self,
        _path: &WorkspacePath,
        _expected: &SnapshotTreeExpected,
        _content: SnapshotTreeContent<'_>,
    ) -> Result<(), SnapshotTreeError> {
        Err(SnapshotTreeError::Unsupported)
    }

    #[cfg(not(unix))]
    pub fn create_tree_directory(&self, _path: &WorkspacePath) -> Result<(), SnapshotTreeError> {
        Err(SnapshotTreeError::Unsupported)
    }
}

#[cfg(unix)]
struct WalkState {
    core: Arc<FilesystemCore>,
    exclusions: Vec<String>,
    limits: SnapshotTreeLimits,
    token: CancellationToken,
    frames: Vec<Frame>,
    entries: usize,
    path_bytes: usize,
    budget: IgnoreBudget,
    scratch: IgnoreScratch,
}

#[cfg(unix)]
struct Frame {
    directory: File,
    prefix: String,
    rules: Option<Arc<IgnoreScope>>,
    names: std::vec::IntoIter<OsString>,
}

#[cfg(unix)]
impl WalkState {
    fn open(
        core: Arc<FilesystemCore>,
        scope: &str,
        exclusions: Vec<String>,
        limits: SnapshotTreeLimits,
        token: CancellationToken,
    ) -> Result<Self, SnapshotTreeError> {
        let mut directory =
            open_root(core.root()).map_err(|_| SnapshotTreeError::ScopeUnavailable)?;
        // Probe the resolver even on an empty tree: an unsupported kernel must refuse, not look empty.
        match open_child(&directory, ".", DIRECTORY_FLAGS) {
            Ok(_) => {}
            Err(Errno::NOSYS) => return Err(SnapshotTreeError::Unsupported),
            Err(_) => return Err(SnapshotTreeError::ScopeUnavailable),
        }
        let budget = IgnoreBudget::new(&core.limits);
        let mut state = Self {
            core,
            exclusions,
            limits,
            token,
            frames: Vec::new(),
            entries: 0,
            path_bytes: 0,
            budget,
            scratch: IgnoreScratch::default(),
        };
        let mut prefix = String::new();
        let mut rules = None;
        let components = scope
            .split('/')
            .filter(|component| *component != ".")
            .collect::<Vec<_>>();
        if components.len() > MAX_SNAPSHOT_DEPTH {
            return Err(depth_exceeded());
        }
        for component in components {
            state.check_cancelled()?;
            if component.is_empty() || component == ".." {
                return Err(SnapshotTreeError::ScopeUnavailable);
            }
            rules = state.read_rules(&directory, &prefix, rules)?;
            directory = open_child(&directory, component, DIRECTORY_FLAGS)
                .map_err(|_| SnapshotTreeError::ScopeUnavailable)?;
            prefix = join(&prefix, component);
            if state.core.policy.protects_relative(&prefix) {
                return Err(SnapshotTreeError::ScopeUnavailable);
            }
            // An inner repository answers to its own rules only, as it does to git.
            if reject_repository(&directory).is_err() {
                rules = None;
            }
        }
        state.enter(directory, prefix, rules)?;
        Ok(state)
    }

    fn next_entry(&mut self) -> Option<Result<SnapshotTreeEntry, SnapshotTreeError>> {
        loop {
            if self.token.is_cancelled() {
                self.frames.clear();
                return Some(Err(SnapshotTreeError::Cancelled));
            }
            let frame = self.frames.last_mut()?;
            let Some(name) = frame.names.next() else {
                self.frames.pop();
                continue;
            };
            match self.visit(&name) {
                Ok(Some(entry)) => return Some(Ok(entry)),
                Ok(None) => {}
                Err(error) => {
                    self.frames.clear();
                    return Some(Err(error));
                }
            }
        }
    }

    fn visit(&mut self, name: &OsStr) -> Result<Option<SnapshotTreeEntry>, SnapshotTreeError> {
        let Some(frame) = self.frames.last() else {
            return Ok(None);
        };
        let Some(text) = name.to_str() else {
            let path = join(&frame.prefix, &name.to_string_lossy());
            return Ok(Some(skipped(path, SnapshotSkipReason::Unrepresentable)));
        };
        let relative = join(&frame.prefix, text);
        if self.core.policy.protects_relative(&relative) || self.excluded(&relative) {
            return Ok(None);
        }
        if WorkspacePath::new(relative.as_str()).is_err() {
            return Ok(Some(skipped(relative, SnapshotSkipReason::Unrepresentable)));
        }
        let node = match open_child(&frame.directory, text, NODE_FLAGS) {
            Ok(node) => node,
            Err(Errno::NOENT) => return Ok(None),
            Err(errno) => {
                let mount = errno == Errno::XDEV;
                if ignored(
                    frame.rules.as_deref(),
                    &relative,
                    mount,
                    &mut self.budget,
                    &mut self.scratch,
                )? {
                    return Ok(None);
                }
                let reason = if mount {
                    SnapshotSkipReason::Mount
                } else {
                    SnapshotSkipReason::Unreadable
                };
                return Ok(Some(skipped(relative, reason)));
            }
        };
        let metadata = node.metadata()?;
        let file_type = metadata.file_type();
        if ignored(
            frame.rules.as_deref(),
            &relative,
            file_type.is_dir(),
            &mut self.budget,
            &mut self.scratch,
        )? {
            return Ok(None);
        }
        if file_type.is_symlink() {
            return Ok(Some(match readlinkat(&node, "", Vec::new()) {
                Ok(target) => SnapshotTreeEntry {
                    path: relative,
                    node: SnapshotTreeNode::Symlink(SnapshotTreeLink {
                        target: target.into_bytes(),
                        stamp: SnapshotTreeStamp::of(&metadata),
                    }),
                },
                Err(_) => skipped(relative, SnapshotSkipReason::Unreadable),
            }));
        }
        if file_type.is_file() {
            return Ok(Some(match reopen_regular(&node) {
                Ok(file) => {
                    let metadata = file.metadata()?;
                    SnapshotTreeEntry {
                        path: relative,
                        node: SnapshotTreeNode::File(SnapshotTreeFile { file, metadata }),
                    }
                }
                Err(_) => skipped(relative, SnapshotSkipReason::Unreadable),
            }));
        }
        if !file_type.is_dir() {
            return Ok(Some(skipped(relative, SnapshotSkipReason::Special)));
        }
        let directory = match open_child(&frame.directory, text, DIRECTORY_FLAGS) {
            Ok(directory) => directory,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::XDEV) => return Ok(Some(skipped(relative, SnapshotSkipReason::Mount))),
            Err(_) => return Ok(Some(skipped(relative, SnapshotSkipReason::Unreadable))),
        };
        if reject_repository(&directory).is_err() {
            return Ok(Some(skipped(
                relative,
                SnapshotSkipReason::NestedRepository,
            )));
        }
        let rules = frame.rules.clone();
        self.enter(directory, relative, rules)?;
        Ok(None)
    }

    fn enter(
        &mut self,
        directory: File,
        prefix: String,
        parent: Option<Arc<IgnoreScope>>,
    ) -> Result<(), SnapshotTreeError> {
        if self.frames.len() >= MAX_SNAPSHOT_DEPTH {
            return Err(depth_exceeded());
        }
        let rules = self.read_rules(&directory, &prefix, parent)?;
        let mut names = Vec::new();
        for entry in Dir::read_from(&directory).map_err(io::Error::from)? {
            self.check_cancelled()?;
            let entry = entry.map_err(io::Error::from)?;
            let name = entry.file_name().to_bytes();
            if matches!(name, b"." | b"..") {
                continue;
            }
            self.charge(prefix.len().saturating_add(1).saturating_add(name.len()))?;
            names.push(OsString::from_vec(name.to_vec()));
        }
        names.sort_unstable();
        self.frames.push(Frame {
            directory,
            prefix,
            rules,
            names: names.into_iter(),
        });
        Ok(())
    }

    /// Compiles `<directory>/.gitignore` onto `parent`. Git reads no symlinked or special ignore
    /// file, so neither is a rule source here; a budget that cannot hold the rules refuses the walk.
    fn read_rules(
        &mut self,
        directory: &File,
        prefix: &str,
        parent: Option<Arc<IgnoreScope>>,
    ) -> Result<Option<Arc<IgnoreScope>>, SnapshotTreeError> {
        let Ok(node) = open_child(directory, GITIGNORE, NODE_FLAGS) else {
            return Ok(parent);
        };
        let Ok(file) = reopen_regular(&node) else {
            return Ok(parent);
        };
        let ceiling = self.core.limits.max_gitignore_bytes;
        let mut contents = Vec::new();
        file.take(u64::try_from(ceiling).unwrap_or(u64::MAX).saturating_add(1))
            .read_to_end(&mut contents)?;
        let rules = admit_scope(
            &contents,
            prefix,
            parent,
            &self.core.limits,
            &mut self.budget,
        );
        if !self.budget.complete {
            return Err(SnapshotTreeError::IgnoreRulesExceeded);
        }
        Ok(rules)
    }

    fn charge(&mut self, path_bytes: usize) -> Result<(), SnapshotTreeError> {
        let retained = self.path_bytes.saturating_add(path_bytes);
        if self.entries >= self.limits.max_entries {
            return Err(SnapshotTreeError::LimitExceeded {
                limit: SnapshotTreeLimit::Entries,
                maximum: u64::try_from(self.limits.max_entries).unwrap_or(u64::MAX),
            });
        }
        if retained > self.limits.max_path_bytes {
            return Err(SnapshotTreeError::LimitExceeded {
                limit: SnapshotTreeLimit::PathBytes,
                maximum: u64::try_from(self.limits.max_path_bytes).unwrap_or(u64::MAX),
            });
        }
        self.entries += 1;
        self.path_bytes = retained;
        Ok(())
    }

    fn excluded(&self, relative: &str) -> bool {
        self.exclusions.iter().any(|excluded| {
            relative
                .strip_prefix(excluded.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    }

    fn check_cancelled(&self) -> Result<(), SnapshotTreeError> {
        if self.token.is_cancelled() {
            Err(SnapshotTreeError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[cfg(unix)]
fn ignored(
    rules: Option<&IgnoreScope>,
    relative: &str,
    is_directory: bool,
    budget: &mut IgnoreBudget,
    scratch: &mut IgnoreScratch,
) -> Result<bool, SnapshotTreeError> {
    let decision = rules.and_then(|rules| rules.decide(relative, is_directory, budget, scratch));
    if !budget.complete {
        return Err(SnapshotTreeError::IgnoreRulesExceeded);
    }
    Ok(decision.unwrap_or(false))
}

#[cfg(unix)]
fn tree_parent<'p>(
    core: &FilesystemCore,
    path: &'p WorkspacePath,
) -> Result<Option<(File, &'p str)>, SnapshotTreeError> {
    if core.policy.protects_relative(path.as_str()) {
        return Err(SnapshotTreeError::Protected);
    }
    let mut directory = open_root(core.root()).map_err(|_| SnapshotTreeError::Blocked)?;
    let mut components = path.as_str().split('/').peekable();
    while let Some(component) = components.next() {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(SnapshotTreeError::Blocked);
        }
        if components.peek().is_none() {
            return Ok(Some((directory, component)));
        }
        directory = match open_child(&directory, component, DIRECTORY_FLAGS) {
            Ok(child) => child,
            Err(Errno::NOENT) => return Ok(None),
            Err(_) => return Err(SnapshotTreeError::Blocked),
        };
    }
    Err(SnapshotTreeError::Blocked)
}

#[cfg(unix)]
fn observe_child(parent: &File, name: &str) -> Result<SnapshotTreeObserved, SnapshotTreeError> {
    let node = match open_child(parent, name, NODE_FLAGS) {
        Ok(node) => node,
        Err(Errno::NOENT) => return Ok(SnapshotTreeObserved::Absent),
        Err(Errno::XDEV) => return Ok(SnapshotTreeObserved::Other),
        Err(errno) => return Err(io::Error::from(errno).into()),
    };
    let metadata = node.metadata()?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = readlinkat(&node, "", Vec::new()).map_err(io::Error::from)?;
        return Ok(SnapshotTreeObserved::Symlink(SnapshotTreeLink {
            target: target.into_bytes(),
            stamp: SnapshotTreeStamp::of(&metadata),
        }));
    }
    if file_type.is_file() {
        let file =
            reopen_regular(&node).map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))?;
        let metadata = file.metadata()?;
        return Ok(SnapshotTreeObserved::File(SnapshotTreeFile {
            file,
            metadata,
        }));
    }
    if file_type.is_dir() {
        return Ok(SnapshotTreeObserved::Directory);
    }
    Ok(SnapshotTreeObserved::Other)
}

#[cfg(unix)]
fn verify_expected(
    parent: &File,
    name: &str,
    expected: &SnapshotTreeExpected,
) -> Result<(), SnapshotTreeError> {
    let current = match open_child(parent, name, NODE_FLAGS) {
        Ok(node) => Some(SnapshotTreeStamp::of(&node.metadata()?)),
        Err(Errno::NOENT) => None,
        Err(_) => return Err(SnapshotTreeError::Changed),
    };
    let matches = match expected {
        SnapshotTreeExpected::Absent => current.is_none(),
        SnapshotTreeExpected::Present(stamp) => current.as_ref() == Some(stamp),
    };
    if matches {
        Ok(())
    } else {
        Err(SnapshotTreeError::Changed)
    }
}

#[cfg(unix)]
fn stage_file(
    parent: &File,
    name: &str,
    source: &mut dyn Read,
    mode: u32,
) -> Result<(), SnapshotTreeError> {
    let mut file = File::from(
        openat(parent, name, STAGED_FLAGS, Mode::from_raw_mode(STAGED_MODE))
            .map_err(io::Error::from)?,
    );
    io::copy(source, &mut file)?;
    fchmod(&file, Mode::from_raw_mode(mode & PERMISSION_BITS)).map_err(io::Error::from)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn changed_or_failed(errno: Errno) -> SnapshotTreeError {
    match errno {
        Errno::EXIST | Errno::ISDIR | Errno::NOTEMPTY | Errno::NOENT => SnapshotTreeError::Changed,
        errno => io::Error::from(errno).into(),
    }
}

#[cfg(unix)]
fn depth_exceeded() -> SnapshotTreeError {
    SnapshotTreeError::LimitExceeded {
        limit: SnapshotTreeLimit::Depth,
        maximum: u64::try_from(MAX_SNAPSHOT_DEPTH).unwrap_or(u64::MAX),
    }
}

#[cfg(unix)]
fn skipped(path: String, reason: SnapshotSkipReason) -> SnapshotTreeEntry {
    SnapshotTreeEntry {
        path,
        node: SnapshotTreeNode::Skipped(reason),
    }
}

#[cfg(unix)]
fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        io::{self, Read},
        os::unix::fs::symlink,
        path::Path,
    };

    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;
    use workcell_host_contract::{MAX_SNAPSHOT_DEPTH, WorkspacePath};

    use super::{
        SnapshotTreeContent, SnapshotTreeError, SnapshotTreeExpected, SnapshotTreeLimit,
        SnapshotTreeLimits, SnapshotTreeObserved, SnapshotTreeStamp,
    };
    use crate::{FileToolGroup, WorkspaceSnapshotAccess};

    const ROOT: &str = ".";
    const UNBOUNDED: SnapshotTreeLimits = SnapshotTreeLimits {
        max_entries: usize::MAX,
        max_path_bytes: usize::MAX,
    };
    const FILE_MODE: u32 = 0o644;

    struct FailingSource;

    impl Read for FailingSource {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::InvalidData))
        }
    }

    async fn access(root: &TempDir) -> WorkspaceSnapshotAccess {
        FileToolGroup::new(root.path(), true, None)
            .await
            .unwrap()
            .workspace_snapshot_access()
    }

    fn walk(
        access: &WorkspaceSnapshotAccess,
        limits: SnapshotTreeLimits,
    ) -> Result<Vec<String>, SnapshotTreeError> {
        access
            .walk_tree(ROOT, Vec::new(), limits, CancellationToken::new())?
            .map(|entry| entry.map(|entry| entry.path))
            .collect()
    }

    fn limit(error: SnapshotTreeError) -> (SnapshotTreeLimit, u64) {
        let SnapshotTreeError::LimitExceeded { limit, maximum } = error else {
            panic!("expected a limit refusal, got {error:?}");
        };
        (limit, maximum)
    }

    fn path(value: &str) -> WorkspacePath {
        WorkspacePath::new(value).unwrap()
    }

    fn observed_stamp(access: &WorkspaceSnapshotAccess, relative: &str) -> SnapshotTreeStamp {
        let SnapshotTreeObserved::File(observed) =
            access.observe_tree_entry(&path(relative)).unwrap()
        else {
            panic!("expected a regular file at {relative}");
        };
        SnapshotTreeStamp::of(&observed.metadata)
    }

    fn entries(directory: &Path) -> usize {
        fs::read_dir(directory).unwrap().count()
    }

    #[tokio::test]
    async fn a_walk_past_its_entry_budget_is_refused_rather_than_truncated() {
        let root = tempfile::tempdir().unwrap();
        for name in ["one", "two", "three"] {
            fs::write(root.path().join(name), name).unwrap();
        }
        let access = access(&root).await;

        assert_eq!(
            limit(
                walk(
                    &access,
                    SnapshotTreeLimits {
                        max_entries: 2,
                        ..UNBOUNDED
                    }
                )
                .unwrap_err()
            ),
            (SnapshotTreeLimit::Entries, 2)
        );
        assert_eq!(
            walk(
                &access,
                SnapshotTreeLimits {
                    max_entries: 3,
                    ..UNBOUNDED
                }
            )
            .unwrap(),
            ["one", "three", "two"]
        );
    }

    #[tokio::test]
    async fn a_walk_charges_every_path_it_retains_even_for_empty_directories() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("one/two")).unwrap();
        let access = access(&root).await;
        let retained = "/one".len() + "one/two".len();

        assert_eq!(
            limit(
                walk(
                    &access,
                    SnapshotTreeLimits {
                        max_path_bytes: retained - 1,
                        ..UNBOUNDED
                    }
                )
                .unwrap_err()
            ),
            (
                SnapshotTreeLimit::PathBytes,
                u64::try_from(retained - 1).unwrap()
            )
        );
        assert!(
            walk(
                &access,
                SnapshotTreeLimits {
                    max_path_bytes: retained,
                    ..UNBOUNDED
                }
            )
            .unwrap()
            .is_empty()
        );
    }

    #[tokio::test]
    async fn a_walk_deeper_than_the_depth_ceiling_is_refused() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(vec!["d"; MAX_SNAPSHOT_DEPTH].join("/"))).unwrap();
        let access = access(&root).await;

        assert_eq!(
            limit(walk(&access, UNBOUNDED).unwrap_err()),
            (
                SnapshotTreeLimit::Depth,
                u64::try_from(MAX_SNAPSHOT_DEPTH).unwrap()
            )
        );
    }

    #[tokio::test]
    async fn publication_refuses_an_entry_that_changed_after_it_was_observed() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file"), "one").unwrap();
        let access = access(&root).await;
        let stamp = observed_stamp(&access, "file");
        fs::write(root.path().join("file"), "edited").unwrap();

        let error = access
            .publish_tree_entry(
                &path("file"),
                &SnapshotTreeExpected::Present(stamp),
                SnapshotTreeContent::File {
                    source: &mut b"restored".as_slice(),
                    mode: FILE_MODE,
                },
            )
            .unwrap_err();
        assert!(matches!(error, SnapshotTreeError::Changed), "{error:?}");
        assert_eq!(
            fs::read_to_string(root.path().join("file")).unwrap(),
            "edited"
        );
        assert_eq!(entries(root.path()), 1);
    }

    #[tokio::test]
    async fn publication_expecting_absence_refuses_an_entry_that_appeared() {
        let root = tempfile::tempdir().unwrap();
        let access = access(&root).await;
        fs::write(root.path().join("file"), "appeared").unwrap();

        let error = access
            .publish_tree_entry(
                &path("file"),
                &SnapshotTreeExpected::Absent,
                SnapshotTreeContent::Symlink {
                    target: b"elsewhere",
                },
            )
            .unwrap_err();
        assert!(matches!(error, SnapshotTreeError::Changed), "{error:?}");
        assert_eq!(
            fs::read_to_string(root.path().join("file")).unwrap(),
            "appeared"
        );
        assert_eq!(entries(root.path()), 1);
    }

    #[tokio::test]
    async fn nothing_is_observed_or_published_through_a_symlinked_ancestor() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("file"), "outside").unwrap();
        symlink(outside.path(), root.path().join("linked")).unwrap();
        let access = access(&root).await;

        for error in [
            access.observe_tree_entry(&path("linked/file")).map(drop),
            access.publish_tree_entry(
                &path("linked/new"),
                &SnapshotTreeExpected::Absent,
                SnapshotTreeContent::File {
                    source: &mut b"escaped".as_slice(),
                    mode: FILE_MODE,
                },
            ),
            access.create_tree_directory(&path("linked/directory")),
        ]
        .map(Result::unwrap_err)
        {
            assert!(matches!(error, SnapshotTreeError::Blocked), "{error:?}");
        }
        assert_eq!(entries(outside.path()), 1);
    }

    #[tokio::test]
    async fn a_source_that_fails_to_read_publishes_nothing() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file"), "one").unwrap();
        let access = access(&root).await;
        let stamp = observed_stamp(&access, "file");

        let error = access
            .publish_tree_entry(
                &path("file"),
                &SnapshotTreeExpected::Present(stamp),
                SnapshotTreeContent::File {
                    source: &mut FailingSource,
                    mode: FILE_MODE,
                },
            )
            .unwrap_err();
        let SnapshotTreeError::Failed(error) = error else {
            panic!("expected the read failure, got {error:?}");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(root.path().join("file")).unwrap(), "one");
        assert_eq!(entries(root.path()), 1);
    }
}
