use std::{
    fs::FileType,
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::{
    FilesystemError,
    gitignore::{IgnoreBudget, IgnoreScope, IgnoreScratch, extend_scope},
    operations::FilesystemCore,
    text::check_cancelled,
};

/// Directories walked upward looking for the repository that encloses a traversal root.
///
/// Confinement already stops the walk in the ordinary case. This bounds the unconfined one, where
/// there is no root to stop at and an ancestor chain is otherwise as deep as the filesystem.
const MAX_ENCLOSING_ANCESTORS: usize = 64;

/// Directory names excluded from broad traversal.
///
/// These hold machine-generated build output and tool caches that are
/// regenerable from source, so scanning them spends the traversal budget
/// without producing results a caller asked for. Dependency *source* trees are
/// deliberately absent: `vendor`, `Pods`, `deps`, and `third_party` contain
/// readable code that callers legitimately search. Generic names such as
/// `build`, `bin`, `out`, and `env` are also absent, because the match is a
/// bare basename at every depth and those names carry real source in many
/// projects.
///
/// Skipping applies only to broad traversal. An explicit path is never skipped,
/// so a caller can still search inside any of these by naming it directly.
pub(super) const SKIPPED_DIRECTORY_NAMES: &[&str] = &[
    ".dart_tool",
    ".git",
    ".gradle",
    ".mypy_cache",
    ".next",
    ".nuxt",
    ".parcel-cache",
    ".pytest_cache",
    ".ruff_cache",
    ".stack-work",
    ".svelte-kit",
    ".terraform",
    ".tox",
    ".turbo",
    ".venv",
    "__pycache__",
    "dist",
    "node_modules",
    "target",
    "venv",
];

pub(super) struct ListedFiles {
    pub(super) paths: Vec<PathBuf>,
    pub(super) truncated: bool,
    /// Entries excluded by `.gitignore` rules.
    pub(super) ignored: usize,
    /// Whether every applicable ignore rule was read and applied.
    pub(super) ignore_complete: bool,
    /// Repository roots below the traversal root that were not descended into.
    pub(super) pruned_repositories: Vec<PathBuf>,
}

impl ListedFiles {
    /// A listing produced without a traversal, for an explicitly named file.
    pub(super) fn single(path: PathBuf) -> Self {
        Self {
            paths: vec![path],
            truncated: false,
            ignored: 0,
            ignore_complete: true,
            pruned_repositories: Vec::new(),
        }
    }
}

/// One entry waiting to be examined, carrying the ignore rules in force where it was found.
struct Pending {
    path: PathBuf,
    file_type: Option<FileType>,
    scope: Option<Arc<IgnoreScope>>,
}

pub(super) async fn list_files(
    core: &FilesystemCore,
    root: &Path,
    token: &CancellationToken,
) -> Result<ListedFiles, FilesystemError> {
    let mut paths = Vec::new();
    let mut discovered = 0usize;
    let mut truncated = false;
    let mut ignored = 0usize;
    let mut pruned_repositories = Vec::new();
    let allows_protected = core.policy.traversal_allows_protected(root);
    check_cancelled(token)?;
    let mut context = TraversalContext::prepare(core, root).await;
    let mut stack = vec![Pending {
        path: root.to_path_buf(),
        file_type: None,
        scope: context.seed.clone(),
    }];
    while let Some(Pending {
        path: candidate,
        file_type,
        scope,
    }) = stack.pop()
    {
        check_cancelled(token)?;
        let directory = if candidate == root {
            candidate
        } else {
            if !core
                .policy
                .traversal_entry_allowed(allows_protected, &candidate)
            {
                continue;
            }
            let name = candidate.file_name().unwrap_or_default();
            if SKIPPED_DIRECTORY_NAMES
                .iter()
                .any(|skipped| name == *skipped)
            {
                continue;
            }
            // The directory read already carries the entry type on platforms
            // that report it, so the common path costs no extra syscall. Fall
            // back only when the filesystem left it unknown.
            let file_type = match file_type {
                Some(file_type) => file_type,
                None => match fs::symlink_metadata(&candidate).await {
                    Ok(metadata) => metadata.file_type(),
                    Err(_) => {
                        truncated = true;
                        continue;
                    }
                },
            };
            if file_type.is_symlink() {
                continue;
            }
            // Every ancestor was verified canonical and non-symlink, so this
            // path is canonical and needs only the confinement decisions.
            if !core.policy.authorize_canonical_entry(&candidate) {
                continue;
            }
            if !file_type.is_file() && !file_type.is_dir() {
                continue;
            }
            // Ignore rules are applied after authorization, so they can only
            // narrow what a caller was already permitted to see. An excluded
            // directory is never scanned: git cannot re-include a path whose
            // parent is excluded, so nothing below it can change this answer.
            if context.excludes(scope.as_deref(), &candidate, file_type.is_dir()) {
                ignored += 1;
                continue;
            }
            if file_type.is_file() {
                paths.push(candidate);
                continue;
            }
            candidate
        };

        let remaining = core.limits.max_traversal_entries.saturating_sub(discovered);
        if remaining == 0 {
            truncated = true;
            continue;
        }
        let scan = read_directory_entries(&directory, remaining, token).await?;
        discovered += scan.entries.len();
        truncated |= scan.truncated;
        // A repository is recognized from the directory's own entries, which the
        // scan already produced, so detection costs no syscall of its own and
        // sees a submodule or worktree whose `.git` is a file rather than a
        // directory. Checked before the ignore file is read, because a pruned
        // subtree has no use for one.
        if context.prune_nested_repositories && directory != root && scan.holds(".git") {
            pruned_repositories.push(directory);
            continue;
        }
        let scope = if context.honors_gitignore() && scan.holds(".gitignore") {
            context.extend(&directory, scope, core).await
        } else {
            scope
        };
        // Reverse push preserves lexical depth-first processing while each
        // ReadDir is already closed. Final sorting also stabilizes all callers.
        for (path, file_type) in scan.entries.into_iter().rev() {
            stack.push(Pending {
                path,
                file_type,
                scope: scope.clone(),
            });
        }
    }
    paths.sort();
    pruned_repositories.sort();
    Ok(ListedFiles {
        paths,
        truncated,
        ignored,
        ignore_complete: context.budget.complete,
        pruned_repositories,
    })
}

/// The ignore rules and repository boundary in force for one traversal.
///
/// Both decisions need the same answer — which repository, if any, encloses the traversal root — so
/// they are settled together in one bounded ancestor walk rather than twice.
struct TraversalContext {
    /// Directory every relative path and ignore-file prefix is measured from: the enclosing
    /// repository root when there is one, otherwise the traversal root.
    base: PathBuf,
    seed: Option<Arc<IgnoreScope>>,
    budget: IgnoreBudget,
    scratch: IgnoreScratch,
    honor_gitignore: bool,
    prune_nested_repositories: bool,
}

impl TraversalContext {
    async fn prepare(core: &FilesystemCore, root: &Path) -> Self {
        let honor_gitignore = core.limits.honor_gitignore;
        let mut context = Self {
            base: root.to_path_buf(),
            seed: None,
            budget: IgnoreBudget::new(&core.limits),
            scratch: IgnoreScratch::default(),
            honor_gitignore,
            prune_nested_repositories: false,
        };
        if !honor_gitignore && !core.limits.prune_nested_repositories {
            return context;
        }
        // Walk outward from the traversal root until a repository is found. Confinement stops the
        // walk at the configured root, so the answer never depends on anything the process was not
        // already allowed to read.
        let mut chain = Vec::new();
        let mut current = Some(root.to_path_buf());
        let mut enclosing = None;
        while let Some(directory) = current {
            if chain.len() >= MAX_ENCLOSING_ANCESTORS
                || !core.policy.authorize_canonical_entry(&directory)
            {
                break;
            }
            let is_repository = fs::symlink_metadata(directory.join(".git")).await.is_ok();
            current = directory.parent().map(Path::to_path_buf);
            chain.push(directory);
            if is_repository {
                enclosing = chain.last().cloned();
                break;
            }
        }
        // No enclosing repository means no boundary to respect. Pruning then would map a root that
        // merely holds a checkout to nothing at all.
        context.prune_nested_repositories =
            core.limits.prune_nested_repositories && enclosing.is_some();
        let Some(enclosing) = enclosing else {
            return context;
        };
        context.base = enclosing;
        if !honor_gitignore {
            return context;
        }
        // Seed the rules an ancestor already imposed on the traversal root, so scoping a search to a
        // subdirectory does not silently change which files are ignored. The traversal root itself
        // is excluded: the main loop scans it and picks up its own ignore file there.
        // `chain` runs deepest first, so dropping its head drops the traversal root and reversing
        // the rest applies the outermost ignore file before the ones it contains.
        let mut scope = None;
        for directory in chain.iter().skip(1).rev() {
            let prefix = context.relative(directory).unwrap_or_default();
            scope =
                extend_scope(directory, &prefix, scope, &core.limits, &mut context.budget).await;
        }
        context.seed = scope;
        context
    }

    const fn honors_gitignore(&self) -> bool {
        self.honor_gitignore
    }

    /// The candidate relative to the base, `/`-separated.
    fn relative(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.base).ok()?;
        let mut rendered = String::new();
        for component in relative.components() {
            if !rendered.is_empty() {
                rendered.push('/');
            }
            rendered.push_str(&component.as_os_str().to_string_lossy());
        }
        Some(rendered)
    }

    async fn extend(
        &mut self,
        directory: &Path,
        parent: Option<Arc<IgnoreScope>>,
        core: &FilesystemCore,
    ) -> Option<Arc<IgnoreScope>> {
        let Some(prefix) = self.relative(directory) else {
            return parent;
        };
        extend_scope(directory, &prefix, parent, &core.limits, &mut self.budget).await
    }

    fn excludes(&mut self, scope: Option<&IgnoreScope>, path: &Path, is_directory: bool) -> bool {
        if !self.honor_gitignore {
            return false;
        }
        let Some(scope) = scope else {
            return false;
        };
        let Some(relative) = self.relative(path) else {
            return false;
        };
        scope
            .decide(&relative, is_directory, &mut self.budget, &mut self.scratch)
            .unwrap_or(false)
    }
}

struct DirectoryScan {
    entries: Vec<(PathBuf, Option<FileType>)>,
    truncated: bool,
}

impl DirectoryScan {
    /// Whether the directory holds an entry with this exact name, of any type.
    ///
    /// Type-insensitive on purpose: a submodule or linked worktree carries `.git` as a file.
    fn holds(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|(path, _)| path.file_name().is_some_and(|found| found == name))
    }
}

async fn read_directory_entries(
    path: &std::path::Path,
    maximum_entries: usize,
    token: &CancellationToken,
) -> Result<DirectoryScan, FilesystemError> {
    let mut reader = match fs::read_dir(path).await {
        Ok(reader) => reader,
        Err(_) => {
            return Ok(DirectoryScan {
                entries: Vec::new(),
                truncated: true,
            });
        }
    };
    let mut entries = Vec::new();
    let mut truncated = false;
    loop {
        check_cancelled(token)?;
        match reader.next_entry().await {
            Ok(Some(entry)) if entries.len() < maximum_entries => {
                let file_type = entry.file_type().await.ok();
                entries.push((entry.path(), file_type));
            }
            Ok(Some(_)) => {
                truncated = true;
                break;
            }
            Ok(None) => break,
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(DirectoryScan { entries, truncated })
}
