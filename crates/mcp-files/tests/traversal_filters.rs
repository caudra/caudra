//! Broad traversal narrowed by `.gitignore` rules and repository boundaries.
//!
//! These properties only a real directory tree can show: what the walk declines to open, and what
//! it reports about having declined. A unit test over compiled patterns cannot tell whether an
//! excluded directory was scanned anyway.

#![cfg(unix)]

use std::{fs, path::Path};

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use workcell_mcp_files::{
    FileGlobInput, FileGrepInput, FileToolGroup, FilesystemLimits, ModelText,
};

fn token() -> CancellationToken {
    CancellationToken::new()
}

/// Pruning is off by default, so a test asking about repository boundaries must turn it on the way
/// the code graph does.
fn pruning_limits() -> FilesystemLimits {
    FilesystemLimits {
        prune_nested_repositories: true,
        ..FilesystemLimits::default()
    }
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent directory");
    }
    fs::write(path, contents).expect("fixture file");
}

/// Marks a directory as a repository root the same way a checkout does.
fn repository(root: &Path, relative: &str) {
    fs::create_dir_all(root.join(relative).join(".git")).expect("repository directory");
}

fn tree() -> TempDir {
    tempfile::tempdir().expect("temporary directory")
}

async fn listing(root: &Path, limits: FilesystemLimits, scope: Option<&str>) -> Vec<String> {
    let group = FileToolGroup::new(root, false, Some(limits))
        .await
        .expect("group");
    let output = group
        .file_glob(
            FileGlobInput {
                pattern: "**/*".to_owned(),
                path: scope.map(str::to_owned),
            },
            &token(),
        )
        .await
        .expect("listing");
    let mut paths = output
        .files
        .into_iter()
        .map(|file| file.relative_path)
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

async fn glob(
    root: &Path,
    limits: FilesystemLimits,
    scope: Option<&str>,
) -> workcell_mcp_files::FileGlobOutput {
    let group = FileToolGroup::new(root, false, Some(limits))
        .await
        .expect("group");
    group
        .file_glob(
            FileGlobInput {
                pattern: "**/*".to_owned(),
                path: scope.map(str::to_owned),
            },
            &token(),
        )
        .await
        .expect("listing")
}

#[tokio::test]
async fn an_ignored_file_is_absent_from_a_listing() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    write(root, ".gitignore", "secret.txt\n");
    write(root, "secret.txt", "hidden\n");
    write(root, "public.txt", "shown\n");

    let paths = listing(root, FilesystemLimits::default(), None).await;
    assert!(paths.contains(&"public.txt".to_owned()));
    assert!(!paths.contains(&"secret.txt".to_owned()), "{paths:?}");
}

#[tokio::test]
async fn disabling_gitignore_restores_the_previous_listing() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    write(root, ".gitignore", "secret.txt\n");
    write(root, "secret.txt", "hidden\n");

    let limits = FilesystemLimits {
        honor_gitignore: false,
        ..FilesystemLimits::default()
    };
    let paths = listing(root, limits, None).await;
    assert!(paths.contains(&"secret.txt".to_owned()), "{paths:?}");
}

/// The saving is the directory that never gets opened, so the observable is the traversal entry
/// budget: an ignored directory holding more entries than the ceiling must not truncate the scan.
#[tokio::test]
async fn an_ignored_directory_is_never_scanned() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    write(root, ".gitignore", "vendor/\n");
    for index in 0..40 {
        write(root, &format!("vendor/file{index}.txt"), "x");
    }
    write(root, "kept.txt", "x");

    let limits = FilesystemLimits {
        max_traversal_entries: 12,
        ..FilesystemLimits::default()
    };
    let output = glob(root, limits, None).await;
    assert!(
        !output.truncated,
        "an unscanned directory must not spend the traversal budget"
    );
    assert_eq!(output.ignored, 1, "the directory itself is the exclusion");
}

#[tokio::test]
async fn a_negation_cannot_re_include_under_an_excluded_directory() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    write(root, ".gitignore", "vendor/\n!vendor/keep.txt\n");
    write(root, "vendor/keep.txt", "x");

    let paths = listing(root, FilesystemLimits::default(), None).await;
    assert!(!paths.contains(&"vendor/keep.txt".to_owned()), "{paths:?}");
}

#[tokio::test]
async fn a_negation_re_includes_a_file_whose_parents_are_all_included() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    write(root, ".gitignore", "*.log\n!keep.log\n");
    write(root, "keep.log", "x");
    write(root, "drop.log", "x");

    let paths = listing(root, FilesystemLimits::default(), None).await;
    assert!(paths.contains(&"keep.log".to_owned()), "{paths:?}");
    assert!(!paths.contains(&"drop.log".to_owned()), "{paths:?}");
}

#[tokio::test]
async fn a_deeper_ignore_file_overrides_a_shallower_one() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    write(root, ".gitignore", "*.log\n");
    write(root, "logs/.gitignore", "!*.log\n");
    write(root, "logs/keep.log", "x");
    write(root, "other/drop.log", "x");

    let paths = listing(root, FilesystemLimits::default(), None).await;
    assert!(paths.contains(&"logs/keep.log".to_owned()), "{paths:?}");
    assert!(!paths.contains(&"other/drop.log".to_owned()), "{paths:?}");
}

#[tokio::test]
async fn the_traversal_root_is_searched_even_when_an_ancestor_ignores_it() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    write(root, ".gitignore", "vendor/\n");
    write(root, "vendor/inner.txt", "x");

    let paths = listing(root, FilesystemLimits::default(), Some("vendor")).await;
    assert_eq!(paths, vec!["inner.txt".to_owned()]);
}

#[tokio::test]
async fn ignore_files_above_the_scope_apply_up_to_the_repository_root() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    write(root, ".gitignore", "*.log\n");
    write(root, "src/keep.rs", "x");
    write(root, "src/drop.log", "x");

    let paths = listing(root, FilesystemLimits::default(), Some("src")).await;
    assert_eq!(paths, vec!["keep.rs".to_owned()]);
}

#[tokio::test]
async fn ignore_rules_narrow_a_grep_as_well_as_a_listing() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    write(root, ".gitignore", "secret.txt\n");
    write(root, "secret.txt", "needle\n");
    write(root, "public.txt", "needle\n");

    let group = FileToolGroup::new(root, false, Some(FilesystemLimits::default()))
        .await
        .expect("group");
    let output = group
        .file_grep(
            FileGrepInput {
                pattern: "needle".to_owned(),
                path: None,
                include: None,
            },
            &token(),
        )
        .await
        .expect("grep");
    assert_eq!(output.matches, 1);
    assert_eq!(output.rows[0].relative_path, "public.txt");
    assert_eq!(output.ignored, 1);
}

/// An exclusion the caller cannot see is indistinguishable from an empty tree.
#[tokio::test]
async fn an_exclusion_is_stated_in_the_model_facing_result() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    write(root, ".gitignore", "secret.txt\n");
    write(root, "secret.txt", "hidden\n");

    let output = glob(root, FilesystemLimits::default(), None).await;
    assert_eq!(output.ignored, 1);
    assert!(
        output.model_text().contains("excluded by .gitignore"),
        "{}",
        output.model_text()
    );
}

/// Negative fixture for the dialect split. `file_glob` patterns have never supported bracket
/// expressions, and teaching the shared matcher about them for gitignore must not change that.
///
/// Both patterns are needed. A bracket pattern with no other metacharacter resolves through the
/// literal fast path, which never reaches the tokenizer, so on its own it would stay green while
/// the dialect underneath it changed.
#[tokio::test]
async fn a_bracket_in_a_file_glob_pattern_is_still_literal() {
    let tree = tree();
    let root = tree.path();
    write(root, "log[0-9]", "literal name");
    write(root, "log4", "a range would match this");
    write(root, "log[0-9]x", "literal name, tokenized");
    write(root, "log4x", "a range would match this too");

    let group = FileToolGroup::new(root, false, None).await.expect("group");
    for (pattern, expected) in [("log[0-9]", "log[0-9]"), ("log[0-9]?", "log[0-9]x")] {
        let output = group
            .file_glob(
                FileGlobInput {
                    pattern: pattern.to_owned(),
                    path: None,
                },
                &token(),
            )
            .await
            .expect("listing");
        let paths = output
            .files
            .into_iter()
            .map(|file| file.relative_path)
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![expected.to_owned()], "pattern {pattern}");
    }
}

#[tokio::test]
async fn a_nested_repository_is_not_traversed_when_the_root_is_inside_one() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    repository(root, "vendor/inner");
    write(root, "vendor/inner/foreign.txt", "x");
    write(root, "own.txt", "x");

    let output = glob(root, pruning_limits(), None).await;
    let paths = output
        .files
        .iter()
        .map(|file| file.relative_path.clone())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"own.txt".to_owned()), "{paths:?}");
    assert!(
        !paths.iter().any(|path| path.starts_with("vendor/inner/")),
        "{paths:?}"
    );
    assert_eq!(output.pruned_repositories, vec!["vendor/inner".to_owned()]);
}

/// Negative fixture for the enclosing-repository gate. A root that merely holds checkouts has no
/// boundary to respect, and pruning there would map it to nothing at all.
#[tokio::test]
async fn a_sibling_repository_is_traversed_when_the_root_is_not_inside_one() {
    let tree = tree();
    let root = tree.path();
    repository(root, "project");
    write(root, "project/main.rs", "x");

    let output = glob(root, pruning_limits(), None).await;
    let paths = output
        .files
        .iter()
        .map(|file| file.relative_path.clone())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"project/main.rs".to_owned()), "{paths:?}");
    assert!(output.pruned_repositories.is_empty());
}

#[tokio::test]
async fn a_submodule_whose_dot_git_is_a_file_is_pruned_like_a_directory() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    write(root, "module/.git", "gitdir: ../.git/modules/module\n");
    write(root, "module/source.rs", "x");

    let output = glob(root, pruning_limits(), None).await;
    assert_eq!(output.pruned_repositories, vec!["module".to_owned()]);
}

#[tokio::test]
async fn naming_a_nested_repository_as_the_scope_traverses_it() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    repository(root, "vendor/inner");
    write(root, "vendor/inner/foreign.txt", "x");

    let paths = listing(root, pruning_limits(), Some("vendor/inner")).await;
    assert_eq!(paths, vec!["foreign.txt".to_owned()]);
}

/// Negative fixture for the scope decision: pruning belongs to the code graph, whose ranking a
/// foreign tree distorts. A text search has no such problem and must keep descending.
#[tokio::test]
async fn a_text_search_still_descends_into_a_nested_repository() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    repository(root, "vendor/inner");
    write(root, "vendor/inner/foreign.txt", "x");

    let output = glob(root, FilesystemLimits::default(), None).await;
    let paths = output
        .files
        .iter()
        .map(|file| file.relative_path.clone())
        .collect::<Vec<_>>();
    assert!(
        paths.contains(&"vendor/inner/foreign.txt".to_owned()),
        "{paths:?}"
    );
    assert!(output.pruned_repositories.is_empty());
}

#[tokio::test]
async fn an_ordinary_listing_discloses_nothing_about_filters() {
    let tree = tree();
    let root = tree.path();
    write(root, "a.txt", "x");

    let output = glob(root, FilesystemLimits::default(), None).await;
    assert_eq!(output.ignored, 0);
    assert!(output.ignore_complete);
    assert!(output.pruned_repositories.is_empty());
    assert_eq!(output.model_text(), "a.txt");
}

#[tokio::test]
async fn ignore_rules_do_not_change_the_order_of_the_listing() {
    let tree = tree();
    let root = tree.path();
    repository(root, ".");
    write(root, ".gitignore", "b.txt\n");
    for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        write(root, name, "x");
    }

    let paths = listing(root, FilesystemLimits::default(), None).await;
    assert_eq!(
        paths,
        vec![
            ".gitignore".to_owned(),
            "a.txt".to_owned(),
            "c.txt".to_owned(),
            "d.txt".to_owned()
        ]
    );
}
