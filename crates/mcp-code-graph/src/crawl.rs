//! Turning a confined directory into the source inputs the engine consumes.
//!
//! This module resolves nothing itself. Every path is produced by the filesystem group's traversal
//! and re-authorized through its resolver before a byte is read, so the process has exactly one
//! confinement implementation and this crate cannot become a second one.
//!
//! The read is a plain whole-file byte read rather than `file_read`, because the engine needs the
//! source exactly as it is on disk. `file_read` returns a model-facing rendering — line-numbered,
//! bounded to a reading window, with long lines elided — which is the right shape for a reader and
//! the wrong shape for a parser. Resolution is delegated; presentation is not.

use std::{collections::BTreeSet, path::Path, time::Instant};

use tokio_util::sync::CancellationToken;
use workcell_code_graph::{Language, SourceInput};
use workcell_mcp_files::{
    FileGlobInput, FileResourceAccess, FileToolGroup, FilesystemError, FilesystemLimits,
};

use crate::limits::CodeGraphLimits;
use crate::progress::{GraphPhase, GraphProgressSink, PROGRESS_FILE_INTERVAL, report};

/// Why a discovered file contributed nothing.
///
/// Reported rather than dropped. A map that silently omits a file cannot be distinguished from a
/// map of a tree that does not contain it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrawlSkip {
    /// The extension maps to no grammar.
    UnknownLanguage,
    /// Larger than [`CodeGraphLimits::max_source_bytes`].
    Oversize,
    /// Not valid UTF-8.
    NotText,
    /// The read failed. Carries no detail: the underlying error embeds a path.
    Unreadable,
}

impl CrawlSkip {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::UnknownLanguage => "unknown_language",
            Self::Oversize => "oversize",
            Self::NotText => "not_text",
            Self::Unreadable => "unreadable",
        }
    }
}

/// What the crawl found, and what it could not finish.
#[derive(Debug)]
pub struct Crawled {
    pub inputs: Vec<SourceInput>,
    /// Files discovered but not ingested, with the reason, sorted by path.
    pub skipped: Vec<(String, CrawlSkip)>,
    /// Total bytes of source actually read.
    pub bytes_read: usize,
    /// Set when a bound stopped the crawl before the tree ran out.
    pub truncated: Option<CrawlTruncation>,
    /// Whether the underlying traversal examined every candidate it produced.
    pub scan_complete: bool,
    /// Entries the tree's own `.gitignore` rules excluded.
    pub files_ignored: usize,
    /// Whether every applicable ignore rule was read.
    ///
    /// False means the exclusions are a subset of what the tree asked for, so the map is a superset
    /// of the intended one. That is a different claim from a truncated scan, which is a subset.
    pub ignore_complete: bool,
    /// Directories not mapped because they are separate repositories, root-relative and sorted.
    ///
    /// Reported for the same reason a skipped file is: a map that silently omits a repository
    /// cannot be told apart from a map of a tree that never held one.
    pub pruned_repositories: Vec<String>,
}

/// Hand-written because two of these fields assert completeness, and `bool`'s default asserts the
/// opposite. An empty crawl examined nothing and hid nothing; it did not fail to read the rules.
impl Default for Crawled {
    fn default() -> Self {
        Self {
            inputs: Vec::new(),
            skipped: Vec::new(),
            bytes_read: 0,
            truncated: None,
            scan_complete: true,
            files_ignored: 0,
            ignore_complete: true,
            pruned_repositories: Vec::new(),
        }
    }
}

/// Which bound stopped the crawl.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrawlTruncation {
    /// [`CodeGraphLimits::max_files`] was reached.
    FileLimit,
    /// [`CodeGraphLimits::max_total_bytes`] was reached.
    ByteLimit,
    /// [`CodeGraphLimits::crawl_deadline`] elapsed.
    Deadline,
}

impl CrawlTruncation {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FileLimit => "file_limit",
            Self::ByteLimit => "byte_limit",
            Self::Deadline => "deadline",
        }
    }
}

/// Filesystem limits sized for a repository-scale traversal.
///
/// The filesystem group's defaults are sized for a model-facing search, where five hundred results
/// is already more than a reader wants. A map of a repository is a different question asked of the
/// same tree, so the traversal ceilings come from this crate's own limits and remain host-owned.
#[must_use]
pub fn crawl_filesystem_limits(limits: &CodeGraphLimits) -> FilesystemLimits {
    FilesystemLimits {
        max_search_results: limits.max_files,
        max_traversal_entries: limits.max_traversal_entries,
        honor_gitignore: limits.honor_gitignore,
        // A map does not cross a repository boundary its root is inside. Ranking is global and
        // normalized, so a vendored repository's symbols compete with the host project's in one
        // vector, and its files spend a budget sized for the project that was asked about. A text
        // search has neither problem and keeps descending, which is why this is set here and not in
        // the defaults.
        prune_nested_repositories: true,
        ..FilesystemLimits::default()
    }
}

/// Lists and reads every source file under `relative_path`.
///
/// `group` supplies confinement. `relative_path` is interpreted by that group, not by this
/// function, so a caller cannot reach outside the root by constructing one here.
pub async fn crawl(
    group: &FileToolGroup,
    relative_path: Option<&str>,
    limits: &CodeGraphLimits,
    progress: Option<&dyn GraphProgressSink>,
    token: &CancellationToken,
) -> Result<Crawled, FilesystemError> {
    let listing = group
        .file_glob(
            FileGlobInput {
                pattern: "**/*".to_owned(),
                path: relative_path.map(str::to_owned),
            },
            token,
        )
        .await?;

    let mut crawled = Crawled {
        scan_complete: listing.scan_complete,
        files_ignored: listing.ignored,
        ignore_complete: listing.ignore_complete,
        // The listing caps itself at the same file ceiling, so a tree larger than the ceiling is
        // truncated before this loop ever runs. Reading the flag here is what keeps that from
        // looking like a complete map of a small repository.
        truncated: (listing.truncated || listing.total > listing.files.len())
            .then_some(CrawlTruncation::FileLimit),
        ..Crawled::default()
    };

    // Listing paths are relative to the searched directory, not the root. Prefixing restores the
    // root-relative form, so a scoped map and a whole-tree map name the same file the same way and
    // a path from one can be pasted into the other.
    let scope = listing.relative_path.trim_end_matches('/').to_owned();
    let qualify = |relative: String| -> String {
        if scope.is_empty() || scope == "." {
            relative
        } else {
            format!("{scope}/{relative}")
        }
    };

    crawled.pruned_repositories = listing
        .pruned_repositories
        .into_iter()
        .map(&qualify)
        .collect();

    // The traversal already returns a stable order, but the engine's determinism contract starts
    // with a byte sort over paths and it must not depend on that promise holding elsewhere.
    let paths: BTreeSet<String> = listing
        .files
        .into_iter()
        .map(|file| qualify(file.relative_path))
        .collect();

    let started = Instant::now();
    for path in paths {
        if token.is_cancelled() {
            return Err(FilesystemError::Aborted);
        }
        if crawled.inputs.len() >= limits.max_files {
            crawled.truncated = Some(CrawlTruncation::FileLimit);
            break;
        }
        if crawled.bytes_read >= limits.max_total_bytes {
            crawled.truncated = Some(CrawlTruncation::ByteLimit);
            break;
        }
        if started.elapsed() >= limits.crawl_deadline {
            crawled.truncated = Some(CrawlTruncation::Deadline);
            break;
        }

        if Language::from_path(Path::new(&path)).is_none() {
            crawled.skipped.push((path, CrawlSkip::UnknownLanguage));
            continue;
        }

        // Re-authorized here rather than trusted from the listing. Enumeration and resolution are
        // separate decisions, and a path that appeared in a traversal is not thereby readable.
        let resource = match group.inspect_path(&path, FileResourceAccess::Read).await {
            Ok(resource) => resource,
            Err(_) => {
                crawled.skipped.push((path, CrawlSkip::Unreadable));
                continue;
            }
        };

        match read_source(&resource.path, limits.max_source_bytes) {
            Ok(source) => {
                crawled.bytes_read = crawled.bytes_read.saturating_add(source.len());
                crawled.inputs.push(SourceInput { path, source });
                if crawled.inputs.len().is_multiple_of(PROGRESS_FILE_INTERVAL) {
                    report(progress, GraphPhase::Crawl, crawled.inputs.len()).await;
                }
            }
            Err(skip) => crawled.skipped.push((path, skip)),
        }
    }

    crawled.skipped.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(crawled)
}

/// Reads one file, refusing anything above `max_bytes` or not valid UTF-8.
///
/// The size is checked before the read, so an oversize file is never brought into memory to be
/// rejected afterwards.
fn read_source(path: &Path, max_bytes: usize) -> Result<String, CrawlSkip> {
    let metadata = std::fs::metadata(path).map_err(|_| CrawlSkip::Unreadable)?;
    if !metadata.is_file() {
        return Err(CrawlSkip::Unreadable);
    }
    if usize::try_from(metadata.len()).is_ok_and(|len| len > max_bytes) {
        return Err(CrawlSkip::Oversize);
    }
    let bytes = std::fs::read(path).map_err(|_| CrawlSkip::Unreadable)?;
    if bytes.len() > max_bytes {
        return Err(CrawlSkip::Oversize);
    }
    String::from_utf8(bytes).map_err(|_| CrawlSkip::NotText)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tree(files: &[(&str, &str)]) -> TempDir {
        let directory = TempDir::new().expect("temp dir");
        for (path, contents) in files {
            let full = directory.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(full, contents).expect("write");
        }
        directory
    }

    async fn group(root: &Path, limits: &CodeGraphLimits) -> FileToolGroup {
        FileToolGroup::new(root, false, Some(crawl_filesystem_limits(limits)))
            .await
            .expect("group")
    }

    #[tokio::test]
    async fn source_files_are_read_and_unknown_extensions_are_reported() {
        let limits = CodeGraphLimits::default();
        let directory = tree(&[
            ("src/a.rs", "fn alpha() {}"),
            ("src/b.py", "def beta(): pass"),
            ("notes.xyz", "not a language"),
        ]);
        let crawled = crawl(
            &group(directory.path(), &limits).await,
            None,
            &limits,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("crawl");

        let paths: Vec<_> = crawled
            .inputs
            .iter()
            .map(|input| input.path.as_str())
            .collect();
        assert_eq!(paths, ["src/a.rs", "src/b.py"]);
        assert_eq!(
            crawled.skipped,
            vec![("notes.xyz".to_owned(), CrawlSkip::UnknownLanguage)],
            "a file the map cannot read must be named, not silently absent"
        );
    }

    #[tokio::test]
    async fn a_subdirectory_scopes_the_crawl() {
        let limits = CodeGraphLimits::default();
        let directory = tree(&[("src/a.rs", "fn a() {}"), ("vendor/b.rs", "fn b() {}")]);
        let crawled = crawl(
            &group(directory.path(), &limits).await,
            Some("src"),
            &limits,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("crawl");
        assert_eq!(crawled.inputs.len(), 1);
        assert_eq!(crawled.inputs[0].path, "src/a.rs");
    }

    #[tokio::test]
    async fn an_oversize_file_is_skipped_with_a_reason_rather_than_truncated() {
        // Truncating source would produce a parse of something that is not the file, and every
        // span after the cut would point at the wrong text.
        let limits = CodeGraphLimits {
            max_source_bytes: 32,
            ..CodeGraphLimits::default()
        };
        let directory = tree(&[("big.rs", &"fn padding_function_name() {}\n".repeat(10))]);
        let crawled = crawl(
            &group(directory.path(), &limits).await,
            None,
            &limits,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("crawl");
        assert!(crawled.inputs.is_empty());
        assert_eq!(
            crawled.skipped,
            vec![("big.rs".to_owned(), CrawlSkip::Oversize)]
        );
    }

    #[tokio::test]
    async fn binary_content_is_reported_as_not_text() {
        let directory = TempDir::new().expect("temp dir");
        std::fs::write(directory.path().join("a.rs"), [0xff, 0xfe, 0x00]).expect("write");
        let limits = CodeGraphLimits::default();
        let crawled = crawl(
            &group(directory.path(), &limits).await,
            None,
            &limits,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("crawl");
        assert_eq!(
            crawled.skipped,
            vec![("a.rs".to_owned(), CrawlSkip::NotText)]
        );
    }

    #[tokio::test]
    async fn the_file_ceiling_is_disclosed_rather_than_silently_applied() {
        let limits = CodeGraphLimits {
            max_files: 2,
            ..CodeGraphLimits::default()
        };
        let directory = tree(&[
            ("a.rs", "fn a() {}"),
            ("b.rs", "fn b() {}"),
            ("c.rs", "fn c() {}"),
        ]);
        let crawled = crawl(
            &group(directory.path(), &limits).await,
            None,
            &limits,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("crawl");
        assert_eq!(crawled.inputs.len(), 2);
        assert_eq!(crawled.truncated, Some(CrawlTruncation::FileLimit));
    }

    #[tokio::test]
    async fn the_crawl_cannot_leave_the_root() {
        let outer = TempDir::new().expect("temp dir");
        std::fs::write(outer.path().join("secret.rs"), "fn secret() {}").expect("write");
        let inner = outer.path().join("inner");
        std::fs::create_dir_all(&inner).expect("mkdir");
        std::fs::write(inner.join("a.rs"), "fn a() {}").expect("write");

        let limits = CodeGraphLimits::default();
        let confined = group(&inner, &limits).await;
        let escaped = crawl(
            &confined,
            Some(".."),
            &limits,
            None,
            &CancellationToken::new(),
        )
        .await;
        assert!(
            escaped.is_err(),
            "the filesystem group owns confinement and must refuse this"
        );

        let crawled = crawl(&confined, None, &limits, None, &CancellationToken::new())
            .await
            .expect("crawl");
        assert_eq!(crawled.inputs.len(), 1);
        assert_eq!(crawled.inputs[0].path, "a.rs");
    }

    /// Marks a directory as a repository root the way a checkout does.
    fn repository(root: &Path, relative: &str) {
        std::fs::create_dir_all(root.join(relative).join(".git")).expect("repository");
    }

    #[tokio::test]
    async fn a_nested_repository_is_not_mapped_with_the_project_around_it() {
        let limits = CodeGraphLimits::default();
        let directory = tree(&[
            ("own.rs", "fn own() {}"),
            ("vendor/inner/foreign.rs", "fn foreign() {}"),
        ]);
        repository(directory.path(), ".");
        repository(directory.path(), "vendor/inner");

        let crawled = crawl(
            &group(directory.path(), &limits).await,
            None,
            &limits,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("crawl");
        let paths: Vec<_> = crawled
            .inputs
            .iter()
            .map(|input| input.path.clone())
            .collect();
        assert_eq!(paths, vec!["own.rs".to_owned()]);
        assert_eq!(crawled.pruned_repositories, vec!["vendor/inner".to_owned()]);
    }

    /// A pruned path is named relative to the root, not to the scope, so it can be pasted straight
    /// back in as the next call's `path`.
    #[tokio::test]
    async fn a_pruned_repository_is_named_the_way_the_scope_would_be() {
        let limits = CodeGraphLimits::default();
        let directory = tree(&[("vendor/inner/foreign.rs", "fn foreign() {}")]);
        repository(directory.path(), ".");
        repository(directory.path(), "vendor/inner");

        let crawled = crawl(
            &group(directory.path(), &limits).await,
            Some("vendor"),
            &limits,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("crawl");
        assert_eq!(crawled.pruned_repositories, vec!["vendor/inner".to_owned()]);

        let mapped = crawl(
            &group(directory.path(), &limits).await,
            Some("vendor/inner"),
            &limits,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("crawl");
        assert_eq!(mapped.inputs.len(), 1);
        assert!(mapped.pruned_repositories.is_empty());
    }

    /// Negative fixture for the enclosing-repository gate. A root that only holds checkouts has no
    /// boundary to respect, and pruning there would map it to nothing at all.
    #[tokio::test]
    async fn a_root_that_merely_holds_a_checkout_still_maps_it() {
        let limits = CodeGraphLimits::default();
        let directory = tree(&[("project/main.rs", "fn main() {}")]);
        repository(directory.path(), "project");

        let crawled = crawl(
            &group(directory.path(), &limits).await,
            None,
            &limits,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("crawl");
        assert_eq!(crawled.inputs.len(), 1);
        assert!(crawled.pruned_repositories.is_empty());
    }

    #[tokio::test]
    async fn an_ignored_source_file_is_absent_from_the_map() {
        let limits = CodeGraphLimits::default();
        let directory = tree(&[
            (".gitignore", "generated.rs\n"),
            ("generated.rs", "fn generated() {}"),
            ("written.rs", "fn written() {}"),
        ]);
        repository(directory.path(), ".");

        let crawled = crawl(
            &group(directory.path(), &limits).await,
            None,
            &limits,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("crawl");
        let paths: Vec<_> = crawled
            .inputs
            .iter()
            .map(|input| input.path.clone())
            .collect();
        assert_eq!(paths, vec!["written.rs".to_owned()]);
        assert_eq!(crawled.files_ignored, 1);
        assert!(crawled.ignore_complete);
    }

    #[tokio::test]
    async fn disabling_gitignore_restores_the_ignored_file() {
        let limits = CodeGraphLimits {
            honor_gitignore: false,
            ..CodeGraphLimits::default()
        };
        let directory = tree(&[
            (".gitignore", "generated.rs\n"),
            ("generated.rs", "fn generated() {}"),
        ]);
        repository(directory.path(), ".");

        let crawled = crawl(
            &group(directory.path(), &limits).await,
            None,
            &limits,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("crawl");
        assert_eq!(crawled.inputs.len(), 1);
        assert_eq!(crawled.files_ignored, 0);
    }
}
