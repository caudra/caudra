#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
};

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use workcell_mcp_files::{
    FileApplyPatchInput, FileEditInput, FileGlobInput, FileGrepInput, FileReadInput,
    FileReadOutput, FileResourceAccess, FileToolGroup, FileWriteInput, FilesystemError,
    FilesystemLimits, ModelText,
};

const STALE_PUBLICATION: &str = "changed before publication";
const FOREIGN_FILE_GROUP: &str = "different file tool group";
const DIRECTORY_REPLACEMENT_CONTENT: &str = "directory grants must not expose this content";
const DIRECTORY_LISTING_LIMIT: usize = 1;

struct Fixture {
    _temporary: TempDir,
    root: PathBuf,
    outside: PathBuf,
}

fn fixture() -> Fixture {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path().join("root");
    let outside = temporary.path().join("outside");
    fs::create_dir(&root).expect("root");
    fs::create_dir(&outside).expect("outside");
    fs::write(root.join("notes.txt"), "alpha\nbeta\nalpha beta\n").expect("fixture file");
    fs::write(outside.join("secret.txt"), "secret\n").expect("outside file");
    Fixture {
        _temporary: temporary,
        root,
        outside,
    }
}

fn token() -> CancellationToken {
    CancellationToken::new()
}

#[tokio::test]
async fn denies_lexical_and_symlink_escapes_for_reads_and_new_writes() {
    let fixture = fixture();
    symlink(&fixture.outside, fixture.root.join("escape")).expect("escape symlink");
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");

    let lexical = files
        .file_read(
            FileReadInput {
                file_path: "../outside/secret.txt".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await;
    assert!(matches!(lexical, Err(FilesystemError::RootEscape(_))));

    let linked = files
        .file_read(
            FileReadInput {
                file_path: "escape/secret.txt".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await;
    assert!(matches!(linked, Err(FilesystemError::RootEscape(_))));

    let write = files
        .file_write(
            FileWriteInput {
                file_path: "escape/new.txt".into(),
                content: "bad".into(),
            },
            &token(),
        )
        .await;
    assert!(matches!(write, Err(FilesystemError::RootEscape(_))));

    let patch = files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text:
                    "*** Begin Patch\n*** Add File: ../outside/patched.txt\n+bad\n*** End Patch"
                        .into(),
            },
            &token(),
        )
        .await;
    assert!(matches!(patch, Err(FilesystemError::RootEscape(_))));
}

#[tokio::test]
async fn protects_sensitive_names_and_omits_them_from_listing_and_search() {
    let fixture = fixture();
    fs::create_dir(fixture.root.join(".ssh")).expect("ssh");
    fs::create_dir(fixture.root.join(".workcell")).expect("workcell");
    fs::create_dir(fixture.root.join("public")).expect("public");
    fs::write(fixture.root.join(".ssh/config"), "ssh-secret\n").expect("ssh file");
    fs::write(fixture.root.join(".workcell/state.json"), "atlas-secret\n").expect("state");
    fs::write(fixture.root.join(".env.local"), "env-secret\n").expect("env");
    fs::write(fixture.root.join("service.key"), "key-secret\n").expect("key");
    fs::write(fixture.root.join("public/visible.txt"), "visible\n").expect("visible");
    symlink(fixture.root.join("public"), fixture.root.join(".git")).expect("git symlink");
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");

    for protected in [
        ".ssh/config",
        ".workcell/state.json",
        ".env.local",
        "service.key",
        ".git/visible.txt",
    ] {
        let result = files
            .file_read(
                FileReadInput {
                    file_path: protected.into(),
                    offset: None,
                    limit: None,
                },
                &token(),
            )
            .await;
        assert!(
            matches!(result, Err(FilesystemError::ProtectedPath(_))),
            "{protected} should be protected"
        );
    }

    let directory = files
        .file_read(
            FileReadInput {
                file_path: ".".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await
        .expect("directory read");
    let FileReadOutput::Directory {
        entries,
        entry_details,
        ..
    } = directory
    else {
        panic!("expected directory");
    };
    assert_eq!(entries, ["notes.txt", "public/"]);
    assert_eq!(entry_details[0].size_bytes, Some(22));
    assert_eq!(entry_details[0].line_count, Some(4));

    let glob = files
        .file_glob(
            FileGlobInput {
                pattern: "**/*".into(),
                path: None,
            },
            &token(),
        )
        .await
        .expect("glob");
    let names = glob
        .files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"notes.txt"));
    assert!(names.contains(&"public/visible.txt"));
    assert!(names.iter().all(|name| !name.contains("secret")));

    let grep = files
        .file_grep(
            FileGrepInput {
                pattern: "secret".into(),
                path: None,
                include: None,
                ..Default::default()
            },
            &token(),
        )
        .await
        .expect("grep");
    assert!(grep.rows.is_empty());
}

#[tokio::test]
async fn reads_bounded_lines_and_supports_glob_grep_metadata_and_cancellation() {
    let fixture = fixture();
    let limits = FilesystemLimits {
        max_read_lines: 2,
        ..FilesystemLimits::default()
    };
    let files = FileToolGroup::new(&fixture.root, false, Some(limits))
        .await
        .expect("tool group");
    let read = files
        .file_read(
            FileReadInput {
                file_path: "notes.txt".into(),
                offset: Some(2),
                limit: Some(2),
            },
            &token(),
        )
        .await
        .expect("read");
    let FileReadOutput::File {
        numbered_text,
        line_start,
        line_end,
        total_lines,
        truncated,
        ..
    } = read
    else {
        panic!("expected file");
    };
    assert_eq!(numbered_text, "2: beta\n3: alpha beta");
    assert_eq!((line_start, line_end, total_lines), (2, 3, 4));
    assert!(truncated);

    let glob = files
        .file_glob(
            FileGlobInput {
                pattern: "**/*.{txt,md}".into(),
                path: None,
            },
            &token(),
        )
        .await
        .expect("glob");
    assert_eq!(glob.files.len(), 1);
    assert_eq!(glob.files[0].relative_path, "notes.txt");
    assert_eq!(glob.files[0].size_bytes, Some(22));
    assert_eq!(glob.files[0].line_count, Some(4));

    let grep = files
        .file_grep(
            FileGrepInput {
                pattern: "^alpha".into(),
                path: None,
                include: Some("*.txt".into()),
                ..Default::default()
            },
            &token(),
        )
        .await
        .expect("grep");
    assert_eq!(
        grep.rows.iter().map(|row| row.line).collect::<Vec<_>>(),
        [1, 3]
    );

    let cancelled = token();
    cancelled.cancel();
    let result = files
        .file_glob(
            FileGlobInput {
                pattern: "*".into(),
                path: None,
            },
            &cancelled,
        )
        .await;
    assert!(matches!(result, Err(FilesystemError::Aborted)));
}

#[tokio::test]
async fn bounds_regex_lines_file_sizes_results_and_binary_inputs() {
    let fixture = fixture();
    fs::write(fixture.root.join("long.txt"), "abcdeSECRET\n").expect("long");
    fs::write(fixture.root.join("binary.txt"), b"hello\0world").expect("binary");
    let limits = FilesystemLimits {
        max_regex_length: 4,
        max_line_length: 5,
        max_file_bytes: 32,
        max_search_results: 1,
        ..FilesystemLimits::default()
    };
    let files = FileToolGroup::new(&fixture.root, false, Some(limits))
        .await
        .expect("tool group");

    let long_regex = files
        .file_grep(
            FileGrepInput {
                pattern: "12345".into(),
                path: None,
                include: None,
                ..Default::default()
            },
            &token(),
        )
        .await
        .expect_err("regex bound");
    assert!(long_regex.to_string().contains("maximum length of 4"));

    let hidden_suffix = files
        .file_grep(
            FileGrepInput {
                pattern: "SECR".into(),
                path: Some("long.txt".into()),
                include: None,
                ..Default::default()
            },
            &token(),
        )
        .await
        .expect("grep");
    assert!(hidden_suffix.rows.is_empty());
    let prefix = files
        .file_grep(
            FileGrepInput {
                pattern: "^abc".into(),
                path: Some("long.txt".into()),
                include: None,
                ..Default::default()
            },
            &token(),
        )
        .await
        .expect("grep");
    assert_eq!(prefix.rows[0].text, "abcde... (line truncated)");

    let binary = files
        .file_read(
            FileReadInput {
                file_path: "binary.txt".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await
        .expect_err("binary rejected");
    assert!(binary.to_string().contains("binary file"));

    fs::write(fixture.root.join("too-large.txt"), "x".repeat(33)).expect("large");
    let large = files
        .file_read(
            FileReadInput {
                file_path: "too-large.txt".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await
        .expect_err("size rejected");
    assert!(large.to_string().contains("maximum size of 32 bytes"));
}

#[tokio::test]
async fn classifies_file_content_independently_from_extensions_across_operations() {
    let fixture = fixture();
    let text_path = fixture.root.join("editable.bin");
    let binary_path = fixture.root.join("document.txt");
    fs::write(&text_path, "needle\n").expect("misnamed text");
    fs::write(&binary_path, b"%PDF-1.7\nneedle\n").expect("misnamed binary");
    fs::write(fixture.root.join("empty.zip"), []).expect("misnamed empty text");
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");

    files
        .file_read(
            FileReadInput {
                file_path: "editable.bin".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await
        .expect("text extension must not control reads");
    files
        .file_read(
            FileReadInput {
                file_path: "empty.zip".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await
        .expect("empty files are text regardless of extension");

    let binary_read = files
        .file_read(
            FileReadInput {
                file_path: "document.txt".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await
        .expect_err("PDF signature must override a text extension");
    assert!(binary_read.to_string().contains("binary file"));

    let grep = files
        .file_grep(
            FileGrepInput {
                pattern: "needle".into(),
                path: None,
                include: None,
                ..Default::default()
            },
            &token(),
        )
        .await
        .expect("grep");
    assert_eq!(grep.matches, 1);
    assert_eq!(grep.rows[0].relative_path, "editable.bin");

    let directory = files
        .file_read(
            FileReadInput {
                file_path: ".".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await
        .expect("directory read");
    let FileReadOutput::Directory { entry_details, .. } = directory else {
        panic!("expected directory");
    };
    let text_detail = entry_details
        .iter()
        .find(|entry| entry.relative_path == "editable.bin")
        .expect("text detail");
    let binary_detail = entry_details
        .iter()
        .find(|entry| entry.relative_path == "document.txt")
        .expect("binary detail");
    assert_eq!(text_detail.line_count, Some(2));
    assert_eq!(binary_detail.line_count, None);

    files
        .file_write(
            FileWriteInput {
                file_path: "editable.bin".into(),
                content: "alpha\n".into(),
            },
            &token(),
        )
        .await
        .expect("overwrite text with binary-looking extension");
    files
        .file_edit(
            FileEditInput {
                file_path: "editable.bin".into(),
                old_string: "alpha".into(),
                new_string: "beta".into(),
                replace_all: None,
            },
            &token(),
        )
        .await
        .expect("edit text with binary-looking extension");
    files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text:
                    "*** Begin Patch\n*** Update File: editable.bin\n@@\n-beta\n+gamma\n*** End Patch"
                        .into(),
            },
            &token(),
        )
        .await
        .expect("patch text with binary-looking extension");
    assert_eq!(
        fs::read_to_string(&text_path).expect("text result"),
        "gamma\n"
    );

    for result in [
        files
            .file_write(
                FileWriteInput {
                    file_path: "document.txt".into(),
                    content: "replacement\n".into(),
                },
                &token(),
            )
            .await
            .map(|_| ()),
        files
            .file_edit(
                FileEditInput {
                    file_path: "document.txt".into(),
                    old_string: "needle".into(),
                    new_string: "replacement".into(),
                    replace_all: None,
                },
                &token(),
            )
            .await
            .map(|_| ()),
        files
            .file_apply_patch(
                FileApplyPatchInput {
                    patch_text: "*** Begin Patch\n*** Update File: document.txt\n@@\n-%PDF-1.7\n+replacement\n needle\n*** End Patch".into(),
                },
                &token(),
            )
            .await
            .map(|_| ()),
    ] {
        let error = result.expect_err("binary mutation must be rejected");
        assert!(error.to_string().contains("binary file"));
    }
    assert_eq!(
        fs::read(&binary_path).expect("binary remains"),
        b"%PDF-1.7\nneedle\n"
    );
}

#[tokio::test]
async fn pathological_regex_is_linear_and_unsupported_constructs_are_explicit() {
    let fixture = fixture();
    fs::write(
        fixture.root.join("redos.txt"),
        format!("{}!\n", "a".repeat(100_000)),
    )
    .expect("redos input");
    let files = FileToolGroup::new(&fixture.root, false, None)
        .await
        .expect("tool group");

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        files.file_grep(
            FileGrepInput {
                pattern: "(a+)+$".into(),
                path: Some("redos.txt".into()),
                include: None,
                ..Default::default()
            },
            &token(),
        ),
    )
    .await
    .expect("linear regex must remain responsive")
    .expect("grep result");
    assert!(result.rows.is_empty());

    let cancelled = token();
    cancelled.cancel();
    let cancelled_result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        files.file_grep(
            FileGrepInput {
                pattern: "(a+)+$".into(),
                path: Some("redos.txt".into()),
                include: None,
                ..Default::default()
            },
            &cancelled,
        ),
    )
    .await
    .expect("cancelled grep remains responsive");
    assert!(matches!(cancelled_result, Err(FilesystemError::Aborted)));

    let unsupported = files
        .file_grep(
            FileGrepInput {
                pattern: "(?=a)a".into(),
                path: Some("redos.txt".into()),
                include: None,
                ..Default::default()
            },
            &token(),
        )
        .await
        .expect_err("look-around must be rejected");
    assert!(unsupported.to_string().contains("linear-time mode"));
}

#[tokio::test]
async fn directory_scans_stop_at_the_traversal_budget() {
    let fixture = fixture();
    for index in 0..10 {
        fs::write(fixture.root.join(format!("entry-{index}.txt")), "x\n").expect("entry");
    }
    let files = FileToolGroup::new(
        &fixture.root,
        false,
        Some(FilesystemLimits {
            max_traversal_entries: 3,
            max_search_results: 10,
            ..FilesystemLimits::default()
        }),
    )
    .await
    .expect("tool group");
    let result = files
        .file_read(
            FileReadInput {
                file_path: ".".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await
        .expect("directory read");
    let FileReadOutput::Directory {
        entries, truncated, ..
    } = result
    else {
        panic!("expected directory");
    };
    assert_eq!(entries.len(), 3);
    assert!(truncated);
}

/// Write authority is immutable process configuration. A call carries no field
/// that could soften it, so every mutation entry point is denied outright.
#[tokio::test]
async fn is_read_only_by_default_and_offers_no_way_to_mutate() {
    let fixture = fixture();
    let files = FileToolGroup::new(&fixture.root, false, None)
        .await
        .expect("tool group");
    let denied = files
        .file_write(
            FileWriteInput {
                file_path: "new.txt".into(),
                content: "hello\n".into(),
            },
            &token(),
        )
        .await
        .expect_err("read-only");
    assert!(denied.to_string().contains("read-only"));
    assert!(!fixture.root.join("new.txt").exists());

    let patch = "*** Begin Patch\n*** Add File: preview.txt\n+preview\n*** End Patch";
    let denied = files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text: patch.into(),
            },
            &token(),
        )
        .await
        .expect_err("read-only");
    assert!(denied.to_string().contains("read-only"));
    assert!(!fixture.root.join("preview.txt").exists());

    // Planning for host authorization is still allowed, but publication is not,
    // so a prepared patch cannot become a write without startup authority.
    let prepared = files
        .prepare_apply_patch(
            FileApplyPatchInput {
                patch_text: patch.into(),
            },
            &token(),
        )
        .await
        .expect("prepare without write access");
    assert!(!prepared.preview().applied);
    let denied = files
        .execute_prepared_patch(prepared, &token())
        .await
        .expect_err("read-only");
    assert!(denied.to_string().contains("read-only"));
    assert!(!fixture.root.join("preview.txt").exists());
}

#[tokio::test]
async fn atomically_replaces_files_preserves_mode_and_leaves_no_temporary_files() {
    let fixture = fixture();
    let target = fixture.root.join("notes.txt");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).expect("permissions");
    let before = fs::metadata(&target).expect("before");
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");
    files
        .file_write(
            FileWriteInput {
                file_path: "notes.txt".into(),
                content: "replacement\n".into(),
            },
            &token(),
        )
        .await
        .expect("write");
    let after = fs::metadata(&target).expect("after");
    use std::os::unix::fs::MetadataExt;
    assert_ne!(before.ino(), after.ino());
    assert_eq!(after.permissions().mode() & 0o777, 0o640);
    assert_eq!(
        fs::read_to_string(&target).expect("content"),
        "replacement\n"
    );
    assert!(temporary_files(&fixture.root, "notes.txt").is_empty());
}

#[tokio::test]
async fn exclusive_patch_adds_never_overwrite_existing_or_racing_files() {
    let fixture = fixture();
    fs::write(fixture.root.join("existing.txt"), "original\n").expect("existing");
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");
    let existing = files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text:
                    "*** Begin Patch\n*** Add File: existing.txt\n+replacement\n*** End Patch"
                        .into(),
            },
            &token(),
        )
        .await
        .expect_err("existing add rejected");
    assert!(existing.to_string().contains("Cannot add existing file"));
    assert_eq!(
        fs::read_to_string(fixture.root.join("existing.txt")).expect("existing content"),
        "original\n"
    );

    let first = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("first group");
    let second = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("second group");
    let first_patch = FileApplyPatchInput {
        patch_text: "*** Begin Patch\n*** Add File: raced.txt\n+first\n*** End Patch".into(),
    };
    let second_patch = FileApplyPatchInput {
        patch_text: "*** Begin Patch\n*** Add File: raced.txt\n+second\n*** End Patch".into(),
    };
    let first_token = token();
    let second_token = token();
    let (left, right) = tokio::join!(
        first.file_apply_patch(first_patch, &first_token),
        second.file_apply_patch(second_patch, &second_token)
    );
    assert_ne!(left.is_ok(), right.is_ok());
    let content = fs::read_to_string(fixture.root.join("raced.txt")).expect("winner");
    assert!(content == "first\n" || content == "second\n");
    assert!(temporary_files(&fixture.root, "raced.txt").is_empty());
}

/// Exhausting the glob work budget must truncate, not fail.
///
/// The result cap and the traversal cap in the same loop already degrade to a
/// partial result. A work budget that instead returned an error discarded every
/// match already collected, which is what made ordinary wildcard searches fail
/// outright on large repositories.
#[tokio::test]
async fn exhausting_the_glob_work_budget_truncates_instead_of_failing() {
    let fixture = fixture();
    for index in 0..40 {
        fs::write(fixture.root.join(format!("file-{index}.ts")), "x\n").expect("candidate");
    }
    let files = FileToolGroup::new(
        &fixture.root,
        false,
        Some(FilesystemLimits {
            // Enough for a few candidates, far short of the whole corpus.
            max_glob_match_steps: 600,
            ..FilesystemLimits::default()
        }),
    )
    .await
    .expect("tool group");

    let output = files
        .file_glob(
            FileGlobInput {
                pattern: "**/*.ts".into(),
                path: None,
            },
            &token(),
        )
        .await
        .expect("budget exhaustion must not fail the call");

    assert!(output.truncated);
    assert!(!output.scan_complete, "the scan stopped early");
    assert!(
        !output.files.is_empty(),
        "results collected before exhaustion must be kept"
    );
    assert!(output.count < 40);

    let output = files
        .file_grep(
            FileGrepInput {
                pattern: "x".into(),
                path: None,
                include: Some("**/*.ts".into()),
                ..Default::default()
            },
            &token(),
        )
        .await
        .expect("budget exhaustion must not fail the call");
    assert!(output.truncated);
    assert!(output.files_scanned < output.files_listed);
}

/// Counting continues past the returned window so a caller learns how much was
/// withheld, and the model-facing text says so.
#[tokio::test]
async fn truncated_searches_report_totals_and_say_so_in_the_model_text() {
    let fixture = fixture();
    for index in 0..12 {
        fs::write(fixture.root.join(format!("file-{index}.ts")), "needle\n")
            .expect("candidate file");
    }
    let files = FileToolGroup::new(
        &fixture.root,
        false,
        Some(FilesystemLimits {
            max_search_results: 3,
            ..FilesystemLimits::default()
        }),
    )
    .await
    .expect("tool group");

    let output = files
        .file_glob(
            FileGlobInput {
                pattern: "**/*.ts".into(),
                path: None,
            },
            &token(),
        )
        .await
        .expect("glob");
    assert_eq!(output.count, 3);
    assert_eq!(output.total, 12, "counting continues past the shown window");
    assert!(output.scan_complete, "the scan itself completed");
    assert!(output.truncated);

    let output = files
        .file_grep(
            FileGrepInput {
                pattern: "needle".into(),
                path: None,
                include: None,
                ..Default::default()
            },
            &token(),
        )
        .await
        .expect("grep");
    assert_eq!(output.matches, 3);
    assert!(output.truncated);
    assert!(output.files_scanned <= output.files_listed);
    assert_eq!(output.files_listed, 13, "twelve candidates plus notes.txt");
}

/// Ten numbered lines with a hit on 2, 4 and 9, so windows can be made to
/// overlap, to swallow a later hit whole, and to run off the end of the file.
async fn context_fixture() -> (Fixture, FileToolGroup) {
    let fixture = fixture();
    let body = (1..=10)
        .map(|line| {
            if [2, 4, 9].contains(&line) {
                format!("line{line} needle")
            } else {
                format!("line{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(fixture.root.join("ctx.txt"), format!("{body}\n")).expect("context fixture");
    let files = FileToolGroup::new(&fixture.root, false, None)
        .await
        .expect("tool group");
    (fixture, files)
}

async fn context_grep(files: &FileToolGroup, input: FileGrepInput) -> Vec<(usize, bool)> {
    files
        .file_grep(input, &token())
        .await
        .expect("grep")
        .rows
        .iter()
        .filter(|row| row.relative_path == "ctx.txt")
        .map(|row| (row.line, row.matched))
        .collect()
}

fn ctx_input(after: Option<usize>, before: Option<usize>, both: Option<usize>) -> FileGrepInput {
    FileGrepInput {
        pattern: "needle".into(),
        path: Some("ctx.txt".into()),
        context_after: after,
        context_before: before,
        context: both,
        ..Default::default()
    }
}

/// Without context the result is unchanged: one row per hit, every row a match.
#[tokio::test]
async fn a_search_without_context_returns_only_its_hits() {
    let (_fixture, files) = context_fixture().await;
    assert_eq!(
        context_grep(&files, ctx_input(None, None, None)).await,
        vec![(2, true), (4, true), (9, true)]
    );
}

/// Trailing context stops at the last line rather than running past it or
/// reaching into the empty element the terminating newline leaves behind.
#[tokio::test]
async fn trailing_context_is_bounded_by_the_end_of_the_file() {
    let (_fixture, files) = context_fixture().await;
    assert_eq!(
        context_grep(&files, ctx_input(Some(4), None, None)).await,
        vec![
            (2, true),
            (3, false),
            (4, true),
            (5, false),
            (6, false),
            (7, false),
            (8, false),
            (9, true),
            (10, false),
        ],
        "four trailing lines per hit covers the file, and line 10 ends it"
    );
}

/// Leading context never emits a line twice, and a hit inside an earlier
/// window is still reported as a hit.
#[tokio::test]
async fn overlapping_windows_merge_without_repeating_a_line() {
    let (_fixture, files) = context_fixture().await;
    let rows = context_grep(&files, ctx_input(Some(3), Some(3), None)).await;
    let lines: Vec<_> = rows.iter().map(|(line, _)| *line).collect();
    let mut deduped = lines.clone();
    deduped.dedup();
    assert_eq!(lines, deduped, "a merged run repeated a line");
    assert_eq!(lines, (1..=10).collect::<Vec<_>>());
    let matched: Vec<_> = rows
        .iter()
        .filter(|(_, matched)| *matched)
        .map(|(line, _)| *line)
        .collect();
    assert_eq!(matched, vec![2, 4, 9], "every hit kept its mark");
}

/// `-C` sets both sides, and an explicit `-A` or `-B` beats it.
#[tokio::test]
async fn an_explicit_side_overrides_the_combined_context_flag() {
    let (_fixture, files) = context_fixture().await;
    assert_eq!(
        context_grep(&files, ctx_input(None, None, Some(1))).await,
        vec![
            (1, false),
            (2, true),
            (3, false),
            (4, true),
            (5, false),
            (8, false),
            (9, true),
            (10, false),
        ]
    );
    assert_eq!(
        context_grep(&files, ctx_input(Some(0), None, Some(1))).await,
        vec![
            (1, false),
            (2, true),
            (3, false),
            (4, true),
            (8, false),
            (9, true)
        ],
        "-A 0 removes the trailing side that -C asked for"
    );
}

/// The cap counts hits, so context lines cannot crowd matches out of a result,
/// and `head_limit` applies the same cap from the caller's side.
#[tokio::test]
async fn the_result_cap_counts_hits_rather_than_rows() {
    let (_fixture, files) = context_fixture().await;
    let output = files
        .file_grep(
            FileGrepInput {
                head_limit: Some(2),
                ..ctx_input(Some(2), None, None)
            },
            &token(),
        )
        .await
        .expect("grep");
    assert_eq!(output.matches, 2, "two hits, whatever the row count");
    assert!(output.truncated, "the third hit was withheld");
    assert!(
        output.rows.len() > output.matches,
        "context lines are rows without being matches"
    );
    assert!(
        output.rows.iter().all(|row| row.line <= 6),
        "the withheld hit contributed nothing, not even context"
    );
}

/// A caller that learned the parameter name from another search tool still gets
/// a filtered search instead of a silent whole-tree scan.
#[tokio::test]
async fn the_glob_alias_filters_the_same_as_include() {
    let (_fixture, files) = context_fixture().await;
    let input: FileGrepInput =
        serde_json::from_value(serde_json::json!({"pattern": "needle", "glob": "*.md"}))
            .expect("glob alias");
    assert_eq!(input.include.as_deref(), Some("*.md"));
    assert!(
        files
            .file_grep(input, &token())
            .await
            .expect("grep")
            .rows
            .is_empty(),
        "no markdown file holds the pattern"
    );
}

/// Rendering follows grep: `:` after the line number for a hit, `-` for a
/// context line, and `--` between runs that are not adjacent.
#[tokio::test]
async fn model_text_marks_hits_and_separates_runs() {
    let (_fixture, files) = context_fixture().await;
    let output = files
        .file_grep(ctx_input(Some(1), None, None), &token())
        .await
        .expect("grep");
    assert_eq!(
        output.model_text(),
        "ctx.txt:2: line2 needle\n\
         ctx.txt:3- line3\n\
         ctx.txt:4: line4 needle\n\
         ctx.txt:5- line5\n\
         --\n\
         ctx.txt:9: line9 needle\n\
         ctx.txt:10- line10"
    );
}

/// Broad traversal skips regenerable build output, but an explicit path still
/// searches inside it. Dependency source trees are never skipped.
#[tokio::test]
async fn broad_traversal_skips_build_output_but_an_explicit_path_does_not() {
    let fixture = fixture();
    for directory in ["target", "node_modules", ".venv", "dist", "__pycache__"] {
        fs::create_dir_all(fixture.root.join(directory)).expect("skipped directory");
        fs::write(fixture.root.join(directory).join("generated.rs"), "x\n").expect("artifact");
    }
    for directory in ["vendor", "build", "deps", "third_party", "Pods"] {
        fs::create_dir_all(fixture.root.join(directory)).expect("searchable directory");
        fs::write(fixture.root.join(directory).join("source.rs"), "x\n").expect("source");
    }
    fs::write(fixture.root.join("own.rs"), "x\n").expect("own source");
    let files = FileToolGroup::new(&fixture.root, false, None)
        .await
        .expect("tool group");

    let output = files
        .file_glob(
            FileGlobInput {
                pattern: "**/*.rs".into(),
                path: None,
            },
            &token(),
        )
        .await
        .expect("glob");
    let found = output
        .files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        vec![
            "Pods/source.rs",
            "build/source.rs",
            "deps/source.rs",
            "own.rs",
            "third_party/source.rs",
            "vendor/source.rs",
        ],
        "dependency source and ambiguous names stay searchable; build output does not"
    );

    // The skip list is recoverable: naming the directory searches inside it.
    let output = files
        .file_glob(
            FileGlobInput {
                pattern: "**/*.rs".into(),
                path: Some("target".into()),
            },
            &token(),
        )
        .await
        .expect("explicit glob");
    assert_eq!(output.count, 1);
    assert_eq!(output.files[0].relative_path, "generated.rs");
}

#[tokio::test]
async fn multi_file_patch_keeps_changes_published_before_a_later_failure() {
    let fixture = fixture();
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");
    let result = files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text: "*** Begin Patch\n*** Add File: published.txt\n+first\n*** Add File: published.txt/second.txt\n+second\n*** End Patch".into(),
            },
            &token(),
        )
        .await;

    let error = result.expect_err("second publication must fail below the first file");
    assert!(error.to_string().contains("Cannot create directory"));
    assert_eq!(
        fs::read_to_string(fixture.root.join("published.txt")).expect("first publication remains"),
        "first\n"
    );

    // Publication is atomic per file, not transactional across files. Rolling
    // back earlier files could overwrite unrelated concurrent filesystem work.
    assert!(!fixture.root.join("published.txt/second.txt").exists());
}

/// A write to a path whose parents do not exist creates the whole chain.
///
/// The second half is the load-bearing part: directory creation must stay
/// behind path resolution, so a rejected path can never leave directories
/// behind outside the root.
#[tokio::test]
async fn file_write_creates_missing_parent_directories_without_escaping_the_root() {
    let fixture = fixture();
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");

    let output = files
        .file_write(
            FileWriteInput {
                file_path: "deep/nested/new.txt".into(),
                content: "one\ntwo\n".into(),
            },
            &token(),
        )
        .await
        .expect("write into missing directories");

    assert!(!output.existed);
    assert!(output.applied);
    assert_eq!(output.relative_path, "deep/nested/new.txt");
    assert!(fixture.root.join("deep").is_dir());
    assert!(fixture.root.join("deep/nested").is_dir());
    assert_eq!(
        fs::read_to_string(fixture.root.join("deep/nested/new.txt")).expect("written content"),
        "one\ntwo\n"
    );
    assert!(temporary_files(&fixture.root, "new.txt").is_empty());

    let error = files
        .file_write(
            FileWriteInput {
                file_path: "../outside/deep/new.txt".into(),
                content: "escaped\n".into(),
            },
            &token(),
        )
        .await
        .expect_err("escaping write");
    assert!(matches!(error, FilesystemError::RootEscape(_)));
    assert!(
        !fixture.outside.join("deep").exists(),
        "a rejected path must not create directories outside the root"
    );
}

/// A caller can only draw a write as a diff if it is handed the side the write
/// replaced, so that content rides along up to `max_previous_bytes`.
///
/// The empty-file case is why the field is an `Option`: `Some("")` and `None`
/// are different answers, and a sentinel string could not tell them apart.
#[tokio::test]
async fn a_write_carries_the_content_it_replaced_until_that_content_outgrows_its_bound() {
    let fixture = fixture();
    fs::write(fixture.root.join("empty.txt"), "").expect("empty fixture file");
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");

    let created = files
        .file_write(
            FileWriteInput {
                file_path: "fresh.txt".into(),
                content: "one\n".into(),
            },
            &token(),
        )
        .await
        .expect("create");
    assert!(!created.existed);
    assert_eq!(created.previous, None, "a new file has no other side");

    let overwritten = files
        .file_write(
            FileWriteInput {
                file_path: "fresh.txt".into(),
                content: "two\n".into(),
            },
            &token(),
        )
        .await
        .expect("overwrite");
    assert!(overwritten.existed);
    assert_eq!(overwritten.previous.as_deref(), Some("one\n"));

    let emptied = files
        .file_write(
            FileWriteInput {
                file_path: "empty.txt".into(),
                content: "filled\n".into(),
            },
            &token(),
        )
        .await
        .expect("overwrite of an empty file");
    assert_eq!(
        emptied.previous.as_deref(),
        Some(""),
        "a file that was there and empty is not a file that was absent"
    );

    let bounded = FileToolGroup::new(
        &fixture.root,
        true,
        Some(FilesystemLimits {
            max_previous_bytes: 2,
            ..FilesystemLimits::default()
        }),
    )
    .await
    .expect("bounded tool group");
    let dropped = bounded
        .file_write(
            FileWriteInput {
                file_path: "fresh.txt".into(),
                content: "three\n".into(),
            },
            &token(),
        )
        .await
        .expect("overwrite above the bound");
    assert!(dropped.existed);
    assert_eq!(
        dropped.previous, None,
        "an old side above the bound is dropped rather than carried"
    );
}

#[tokio::test]
async fn writes_edits_and_applies_add_update_move_and_delete_patches() {
    let fixture = fixture();
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");
    files
        .file_write(
            FileWriteInput {
                file_path: "new.txt".into(),
                content: "one\ntwo\n".into(),
            },
            &token(),
        )
        .await
        .expect("write");
    files
        .file_edit(
            FileEditInput {
                file_path: "new.txt".into(),
                old_string: "two".into(),
                new_string: "three".into(),
                replace_all: None,
            },
            &token(),
        )
        .await
        .expect("edit");

    let patch_text = "*** Begin Patch\n*** Update File: new.txt\n*** Move to: moved.txt\n@@\n-one\n+ONE\n three\n*** Add File: added.txt\n+added\n*** Delete File: notes.txt\n*** End Patch";
    // Planning is the only way to see a patch before it lands, and dropping the
    // prepared value leaves the filesystem untouched.
    let prepared = files
        .prepare_apply_patch(
            FileApplyPatchInput {
                patch_text: patch_text.into(),
            },
            &token(),
        )
        .await
        .expect("prepare");
    assert!(!prepared.preview().applied);
    assert_eq!(prepared.preview().files.len(), 3);
    drop(prepared);
    assert!(fixture.root.join("new.txt").exists());

    let applied = files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text: patch_text.into(),
            },
            &token(),
        )
        .await
        .expect("apply");
    assert!(applied.applied);
    assert_eq!(
        fs::read_to_string(fixture.root.join("moved.txt")).unwrap(),
        "ONE\nthree\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("added.txt")).unwrap(),
        "added\n"
    );
    assert!(!fixture.root.join("new.txt").exists());
    assert!(!fixture.root.join("notes.txt").exists());
}

#[tokio::test]
async fn exact_edit_requires_unique_match_unless_replace_all_is_set() {
    let fixture = fixture();
    fs::write(fixture.root.join("repeat.txt"), "old\nold\n").expect("repeat");
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");
    let ambiguous = files
        .file_edit(
            FileEditInput {
                file_path: "repeat.txt".into(),
                old_string: "old".into(),
                new_string: "new".into(),
                replace_all: None,
            },
            &token(),
        )
        .await
        .expect_err("ambiguous");
    assert!(ambiguous.to_string().contains("multiple matches"));
    files
        .file_edit(
            FileEditInput {
                file_path: "repeat.txt".into(),
                old_string: "old".into(),
                new_string: "new".into(),
                replace_all: Some(true),
            },
            &token(),
        )
        .await
        .expect("replace all");
    assert_eq!(
        fs::read_to_string(fixture.root.join("repeat.txt")).unwrap(),
        "new\nnew\n"
    );
}

#[tokio::test]
async fn a_scattered_replace_all_previews_the_sites_not_the_span_between_them() {
    let fixture = fixture();
    let mut lines = (0..400)
        .map(|index| format!("filler {index}\n"))
        .collect::<Vec<_>>();
    lines[3] = "target\n".to_owned();
    lines[396] = "target\n".to_owned();
    fs::write(fixture.root.join("scattered.txt"), lines.concat()).expect("fixture file");
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");

    let output = files
        .file_edit(
            FileEditInput {
                file_path: "scattered.txt".into(),
                old_string: "target".into(),
                new_string: "TARGET".into(),
                replace_all: Some(true),
            },
            &token(),
        )
        .await
        .expect("scattered edit");

    // Two one-line sites 393 lines apart. A span-shaped preview would restate
    // every line between them, on both sides.
    assert_eq!((output.diff.additions, output.diff.deletions), (2, 2));
    assert!(!output.diff.truncated);
    assert_eq!(output.diff.patch.matches("@@ -").count(), 2);
    assert!(output.diff.patch.len() < 512);
    assert!(!output.diff.patch.contains("filler 200"));
}

#[tokio::test]
async fn bounds_reported_diffs_without_ever_failing_the_mutation() {
    let fixture = fixture();
    fs::write(fixture.root.join("large.txt"), "old\n".repeat(20_000)).expect("large file");
    let files = FileToolGroup::new(
        &fixture.root,
        true,
        Some(FilesystemLimits {
            max_diff_bytes: 128,
            max_patch_result_bytes: 256,
            ..FilesystemLimits::default()
        }),
    )
    .await
    .expect("tool group");

    // A diff too large to report is truncated for the model rather than failing
    // the mutation, so the edit still lands in full.
    let edited = files
        .file_edit(
            FileEditInput {
                file_path: "large.txt".into(),
                old_string: "old".into(),
                new_string: "new".into(),
                replace_all: Some(true),
            },
            &token(),
        )
        .await
        .expect("bounded edit");
    assert!(edited.applied);
    assert!(edited.diff.truncated);
    assert!(edited.diff.patch.len() <= 128);
    assert_eq!(
        serde_json::to_value(&edited).expect("serialized edit")["diff"]["truncated"],
        serde_json::json!(true)
    );
    assert!(
        fs::read_to_string(fixture.root.join("large.txt"))
            .expect("edited file")
            .starts_with("new\n")
    );

    let bounded_patch_files = FileToolGroup::new(
        &fixture.root,
        true,
        Some(FilesystemLimits {
            max_diff_bytes: 128,
            max_patch_result_bytes: 4 * 1024,
            ..FilesystemLimits::default()
        }),
    )
    .await
    .expect("bounded patch group");
    let bounded_patch = bounded_patch_files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text: "*** Begin Patch\n*** Delete File: large.txt\n*** End Patch".into(),
            },
            &token(),
        )
        .await
        .expect("bounded patch");
    assert!(bounded_patch.truncated);
    assert!(bounded_patch.files[0].truncated);
    assert!(!fixture.root.join("large.txt").exists());

    // A configured result budget shortens the receipt for the model. It never
    // withholds a change the caller asked for and the plan already validated.
    let result = bounded_patch_files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text:
                    "*** Begin Patch\n*** Update File: notes.txt\n@@\n-alpha\n+changed\n*** End Patch"
                        .into(),
            },
            &token(),
        )
        .await
        .expect("a tight result budget shortens rather than refuses");
    assert!(result.applied);
    assert_eq!(result.files.len(), 1);
    assert!(
        fs::read_to_string(fixture.root.join("notes.txt"))
            .expect("patched")
            .starts_with("changed")
    );
}

#[tokio::test]
async fn rejects_zero_limits_at_initialization() {
    let fixture = fixture();
    let error = FileToolGroup::new(
        &fixture.root,
        false,
        Some(FilesystemLimits {
            max_read_bytes: 0,
            ..FilesystemLimits::default()
        }),
    )
    .await
    .expect_err("invalid limits");
    assert_eq!(error.to_string(), "maxReadBytes must be a positive integer");
}

#[tokio::test]
async fn unconfined_native_mode_inspects_and_operates_on_absolute_outside_paths() {
    let fixture = fixture();
    // Read-only hosting: reaching outside the base must not imply write access.
    let files = FileToolGroup::new_unconfined(&fixture.root, false, None)
        .await
        .expect("unconfined group");
    assert!(!files.allow_write());
    let outside_file = fixture.outside.join("secret.txt");
    let input = FileReadInput {
        file_path: outside_file.to_string_lossy().into_owned(),
        offset: None,
        limit: None,
    };

    let resource = files.inspect_read(&input).await.expect("resource");
    assert_eq!(resource.path, outside_file.canonicalize().unwrap());
    let output = files
        .file_read(input, &token())
        .await
        .expect("outside read");
    let FileReadOutput::File { text, .. } = output else {
        panic!("expected file");
    };
    assert_eq!(text, "secret\n");
}

/// Every one of these was documented in the tool description and refused by the validator, which is
/// the worst combination: a caller that reads the contract and obeys it loses a turn. Callers reach
/// for `""` when they mean "no path", and for these fields the meaning was never in doubt, because
/// absence already names the root.
#[tokio::test]
async fn empty_read_scope_and_filter_values_mean_what_the_descriptions_say_they_mean() {
    let fixture = fixture();
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");

    // "An empty filePath is treated as `.` and reads the file root directory."
    let listing = files
        .file_read(
            FileReadInput {
                file_path: String::new(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await
        .expect("empty filePath reads the root");
    let FileReadOutput::Directory { entries, .. } = listing else {
        panic!("expected the root directory listing");
    };
    assert_eq!(entries, ["notes.txt"]);

    // "An empty path is treated as `.`." Asserted against the absent spelling rather than against a
    // hand-written expectation, so the two can never drift apart.
    let absent = files
        .file_glob(
            FileGlobInput {
                pattern: "**/*.txt".into(),
                path: None,
            },
            &token(),
        )
        .await
        .expect("glob without a path");
    let empty = files
        .file_glob(
            FileGlobInput {
                pattern: "**/*.txt".into(),
                path: Some(String::new()),
            },
            &token(),
        )
        .await
        .expect("glob with an empty path");
    assert_eq!(format!("{empty:?}"), format!("{absent:?}"));

    // "An empty path is treated as `.`, and an empty include filter is ignored."
    let absent = files
        .file_grep(
            FileGrepInput {
                pattern: "alpha".into(),
                path: None,
                include: None,
                ..Default::default()
            },
            &token(),
        )
        .await
        .expect("grep without a path or include");
    let empty = files
        .file_grep(
            FileGrepInput {
                pattern: "alpha".into(),
                path: Some(String::new()),
                include: Some(String::new()),
                ..Default::default()
            },
            &token(),
        )
        .await
        .expect("grep with an empty path and include");
    assert_eq!(format!("{empty:?}"), format!("{absent:?}"));

    // The boundary this must not cross. A write names a file, not a scope, so there is no default
    // for an empty path to fold onto and retargeting one at the root would be a silent surprise.
    let refused = files
        .file_write(
            FileWriteInput {
                file_path: String::new(),
                content: "x".into(),
            },
            &token(),
        )
        .await
        .expect_err("empty filePath is still a caller error for a mutation");
    assert_eq!(
        refused.to_string(),
        "Invalid arguments: filePath must not be empty"
    );
}

#[tokio::test]
async fn prepared_native_patch_exposes_every_resource_and_mutates_only_on_execute() {
    let fixture = fixture();
    let source = fixture.outside.join("source.txt");
    let destination = fixture.outside.join("destination.txt");
    fs::write(&source, "old\n").expect("source");
    let files = FileToolGroup::new_unconfined(&fixture.root, true, None)
        .await
        .expect("unconfined group");
    let patch_text = format!(
        "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n-old\n+new\n*** End Patch",
        source.display(),
        destination.display()
    );

    let prepared = files
        .prepare_apply_patch(FileApplyPatchInput { patch_text }, &token())
        .await
        .expect("prepared patch");

    assert!(!prepared.preview().applied);
    assert_eq!(prepared.resources().len(), 2);
    assert_eq!(prepared.resources()[0].path, source.canonicalize().unwrap());
    assert_eq!(prepared.resources()[1].path, destination);
    assert_eq!(fs::read_to_string(&source).unwrap(), "old\n");
    assert!(!destination.exists());

    let output = files
        .execute_prepared_patch(prepared, &token())
        .await
        .expect("execute prepared patch");
    assert!(output.applied);
    assert!(!source.exists());
    assert_eq!(fs::read_to_string(destination).unwrap(), "new\n");
}

#[tokio::test]
async fn expansion_heavy_patch_is_rejected_by_the_preparation_peak_bound() {
    const OLD_LINES: usize = 2 * 1_024 * 1_024;
    const INSERTED_BYTES: usize = 900 * 1_024;
    const PREPARATION_BYTES: usize = 30 * 1_024 * 1_024;

    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("large.txt"), "a\n".repeat(OLD_LINES)).unwrap();
    let group = FileToolGroup::new(temp.path(), true, None).await.unwrap();
    let patch_text = format!(
        "*** Begin Patch\n*** Update File: large.txt\n@@\n-a\n+{}\n*** End Patch",
        "b".repeat(INSERTED_BYTES)
    );

    let result = group
        .prepare_apply_patch_bounded(
            FileApplyPatchInput { patch_text },
            PREPARATION_BYTES,
            &token(),
        )
        .await;
    let Err(error) = result else {
        panic!("expansion-heavy preparation unexpectedly succeeded");
    };

    assert!(error.to_string().contains("content budget"));
}

#[tokio::test]
async fn preparation_is_effect_free_and_exposes_canonical_and_relative_resources() {
    let fixture = fixture();
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");

    let write = files
        .prepare_write(
            FileWriteInput {
                file_path: "nested/new.txt".into(),
                content: "new\n".into(),
            },
            &token(),
        )
        .await
        .expect("prepared write");
    assert!(!write.preview().applied);
    assert!(write.retained_bytes() >= serde_json::to_vec(write.preview()).unwrap().len());
    assert_eq!(write.relative_path(), "nested/new.txt");
    assert_eq!(write.resource().path, fixture.root.join("nested/new.txt"));
    assert!(!fixture.root.join("nested").exists());

    let edit = files
        .prepare_edit(
            FileEditInput {
                file_path: "notes.txt".into(),
                old_string: "alpha\nbeta".into(),
                new_string: "alpha\nchanged".into(),
                replace_all: None,
            },
            &token(),
        )
        .await
        .expect("prepared edit");
    assert!(!edit.preview().applied);
    assert!(edit.retained_bytes() >= serde_json::to_vec(edit.preview()).unwrap().len());
    assert_eq!(
        fs::read_to_string(fixture.root.join("notes.txt")).expect("notes"),
        "alpha\nbeta\nalpha beta\n"
    );

    let patch = files
        .prepare_apply_patch(
            FileApplyPatchInput {
                patch_text: "*** Begin Patch\n*** Add File: patch.txt\n+patch\n*** End Patch"
                    .into(),
            },
            &token(),
        )
        .await
        .expect("prepared patch");
    assert!(!patch.preview().applied);
    assert!(patch.retained_bytes() >= serde_json::to_vec(patch.preview()).unwrap().len());
    assert_eq!(patch.relative_paths(), ["patch.txt"]);
    assert!(!fixture.root.join("patch.txt").exists());
}

#[tokio::test]
async fn prepared_reads_and_searches_keep_the_exact_options_and_scope() {
    let fixture = fixture();
    fs::write(fixture.root.join("other.rs"), "alpha\n").expect("other");
    let files = FileToolGroup::new(&fixture.root, false, None)
        .await
        .expect("tool group");

    let mut read_input = FileReadInput {
        file_path: "notes.txt".into(),
        offset: Some(2),
        limit: Some(1),
    };
    let read = files
        .prepare_read(read_input.clone(), &token())
        .await
        .expect("prepared read");
    assert!(read.retained_bytes() >= read.resource().requested_path.len());
    read_input.offset = Some(1);
    let FileReadOutput::File { text, .. } = files
        .execute_prepared_read(read, &token())
        .await
        .expect("read")
    else {
        panic!("expected file");
    };
    assert_eq!(text, "beta");

    let mut glob_input = FileGlobInput {
        pattern: "*.txt".into(),
        path: None,
    };
    let glob = files
        .prepare_glob(glob_input.clone(), &token())
        .await
        .expect("prepared glob");
    assert!(glob.retained_bytes() >= glob.resource().requested_path.len());
    glob_input.pattern = "*.rs".into();
    let glob = files
        .execute_prepared_glob(glob, &token())
        .await
        .expect("glob");
    assert_eq!(glob.pattern, "*.txt");
    assert_eq!(glob.files.len(), 1);
    assert_eq!(glob.files[0].relative_path, "notes.txt");

    let mut grep_input = FileGrepInput {
        pattern: "beta".into(),
        path: Some("notes.txt".into()),
        include: None,
        ..Default::default()
    };
    let grep = files
        .prepare_grep(grep_input.clone(), &token())
        .await
        .expect("prepared grep");
    assert!(grep.retained_bytes() >= grep.resource().requested_path.len());
    grep_input.pattern = "missing".into();
    grep_input.path = Some("other.rs".into());
    let grep = files
        .execute_prepared_grep(grep, &token())
        .await
        .expect("grep");
    assert_eq!(grep.pattern, "beta");
    assert_eq!(grep.relative_path, "notes.txt");
    assert_eq!(grep.matches, 2);
}

#[tokio::test]
async fn directory_only_execution_refuses_prepared_files_foreign_groups_and_cancelled_calls() {
    let fixture = fixture();
    let files = FileToolGroup::new(&fixture.root, false, None)
        .await
        .unwrap();
    let file = files
        .prepare_read(
            FileReadInput {
                file_path: "notes.txt".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await
        .unwrap();
    assert!(
        files
            .execute_prepared_directory_read(file, &token())
            .await
            .is_err()
    );

    let input = FileReadInput {
        file_path: ".".into(),
        offset: None,
        limit: None,
    };
    let directory = files.prepare_read(input.clone(), &token()).await.unwrap();
    let foreign = FileToolGroup::new(&fixture.root, false, None)
        .await
        .unwrap();
    let error = foreign
        .execute_prepared_directory_read(directory, &token())
        .await
        .unwrap_err();
    assert!(error.to_string().contains(FOREIGN_FILE_GROUP));

    let directory = files.prepare_read(input, &token()).await.unwrap();
    let cancelled = token();
    cancelled.cancel();
    assert!(matches!(
        files
            .execute_prepared_directory_read(directory, &cancelled)
            .await,
        Err(FilesystemError::Aborted)
    ));
}

#[tokio::test]
async fn prepared_directory_read_never_follows_a_replaced_resource() {
    for unconfined in [false, true] {
        for replacement in [
            "file",
            "inside_file",
            "outside_file",
            "inside_directory",
            "outside_directory",
            "protected_directory",
        ] {
            let fixture = fixture();
            let path = fixture.root.join("listed");
            fs::create_dir(&path).unwrap();
            fs::create_dir(fixture.root.join("other")).unwrap();
            fs::create_dir(fixture.root.join(".ssh")).unwrap();
            let files = if unconfined {
                FileToolGroup::new_unconfined(&fixture.root, false, None).await
            } else {
                FileToolGroup::new(&fixture.root, false, None).await
            }
            .unwrap();
            let prepared = files
                .prepare_read(
                    FileReadInput {
                        file_path: "listed".into(),
                        offset: None,
                        limit: None,
                    },
                    &token(),
                )
                .await
                .unwrap();
            assert_eq!(prepared.resource().access, FileResourceAccess::Traverse);
            fs::remove_dir(&path).unwrap();
            match replacement {
                "file" => fs::write(&path, DIRECTORY_REPLACEMENT_CONTENT).unwrap(),
                "inside_file" => symlink(fixture.root.join("notes.txt"), &path).unwrap(),
                "outside_file" => symlink(fixture.outside.join("secret.txt"), &path).unwrap(),
                "inside_directory" => symlink(fixture.root.join("other"), &path).unwrap(),
                "outside_directory" => symlink(&fixture.outside, &path).unwrap(),
                _ => symlink(fixture.root.join(".ssh"), &path).unwrap(),
            }
            let result = files
                .execute_prepared_directory_read(prepared, &token())
                .await;
            assert!(result.is_err(), "{replacement} was followed: {result:?}");
        }
    }
}

#[tokio::test]
async fn prepared_directory_read_does_not_follow_a_retargeted_input_symlink() {
    let fixture = fixture();
    let directory = fixture.root.join("listed");
    let link = fixture.root.join("alias");
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("visible.txt"), DIRECTORY_REPLACEMENT_CONTENT).unwrap();
    symlink(&directory, &link).unwrap();
    let files = FileToolGroup::new(&fixture.root, false, None)
        .await
        .unwrap();
    let prepared = files
        .prepare_read(
            FileReadInput {
                file_path: "alias".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await
        .unwrap();
    fs::remove_file(&link).unwrap();
    symlink(fixture.outside.join("secret.txt"), &link).unwrap();
    let FileReadOutput::Directory { path, entries, .. } = files
        .execute_prepared_directory_read(prepared, &token())
        .await
        .unwrap()
    else {
        panic!("directory-only execution returned file contents");
    };
    assert_eq!(PathBuf::from(path), fs::canonicalize(directory).unwrap());
    assert_eq!(entries, ["visible.txt"]);
}

#[tokio::test]
async fn prepared_directory_read_preserves_listing_bounds_and_path_policy() {
    let fixture = fixture();
    fs::create_dir(fixture.root.join("public")).unwrap();
    fs::create_dir(fixture.root.join(".ssh")).unwrap();
    fs::write(fixture.root.join(".env"), DIRECTORY_REPLACEMENT_CONTENT).unwrap();
    fs::write(
        fixture.root.join("public/nested.txt"),
        DIRECTORY_REPLACEMENT_CONTENT,
    )
    .unwrap();
    symlink(&fixture.outside, fixture.root.join("outside_link")).unwrap();
    symlink(
        fixture.root.join(".ssh"),
        fixture.root.join("protected_link"),
    )
    .unwrap();
    for limited in [false, true] {
        let limits = limited.then(|| FilesystemLimits {
            max_search_results: DIRECTORY_LISTING_LIMIT,
            ..FilesystemLimits::default()
        });
        let files = FileToolGroup::new(&fixture.root, false, limits)
            .await
            .unwrap();
        let prepared = files
            .prepare_read(
                FileReadInput {
                    file_path: ".".into(),
                    offset: None,
                    limit: None,
                },
                &token(),
            )
            .await
            .unwrap();
        let FileReadOutput::Directory {
            entries, truncated, ..
        } = files
            .execute_prepared_directory_read(prepared, &token())
            .await
            .unwrap()
        else {
            panic!("directory-only execution returned file contents");
        };
        assert_eq!(truncated, limited);
        assert_eq!(
            entries,
            if limited {
                vec!["notes.txt"]
            } else {
                vec!["notes.txt", "public/"]
            }
        );
    }
}

#[tokio::test]
async fn prepared_mutations_reject_stale_write_edit_and_patch_plans() {
    let fixture = fixture();
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");

    let write = files
        .prepare_write(
            FileWriteInput {
                file_path: "notes.txt".into(),
                content: "prepared write\n".into(),
            },
            &token(),
        )
        .await
        .expect("prepared write");
    fs::write(fixture.root.join("notes.txt"), "external write\n").expect("external write");
    let error = files
        .execute_prepared_write(write, &token())
        .await
        .expect_err("stale write");
    assert!(error.to_string().contains(STALE_PUBLICATION));

    fs::write(fixture.root.join("notes.txt"), "edit me\n").expect("edit source");
    let edit = files
        .prepare_edit(
            FileEditInput {
                file_path: "notes.txt".into(),
                old_string: "edit".into(),
                new_string: "edited".into(),
                replace_all: None,
            },
            &token(),
        )
        .await
        .expect("prepared edit");
    fs::write(fixture.root.join("notes.txt"), "external edit\n").expect("external edit");
    let error = files
        .execute_prepared_edit(edit, &token())
        .await
        .expect_err("stale edit");
    assert!(error.to_string().contains(STALE_PUBLICATION));

    fs::write(fixture.root.join("notes.txt"), "patch me\n").expect("patch source");
    let patch = files
        .prepare_apply_patch(
            FileApplyPatchInput {
                patch_text: "*** Begin Patch\n*** Update File: notes.txt\n@@\n-patch me\n+patched\n*** End Patch"
                    .into(),
            },
            &token(),
        )
        .await
        .expect("prepared patch");
    fs::write(fixture.root.join("notes.txt"), "external patch\n").expect("external patch");
    let error = files
        .execute_prepared_patch(patch, &token())
        .await
        .expect_err("stale patch");
    assert!(error.to_string().contains(STALE_PUBLICATION));
    assert_eq!(
        fs::read_to_string(fixture.root.join("notes.txt")).expect("notes"),
        "external patch\n"
    );
}

#[tokio::test]
async fn prepared_mutation_rejects_a_retargeted_symlink_and_a_foreign_group() {
    let fixture = fixture();
    let first = fixture.root.join("first.txt");
    let second = fixture.root.join("second.txt");
    let link = fixture.root.join("current.txt");
    fs::write(&first, "first\n").expect("first");
    fs::write(&second, "second\n").expect("second");
    symlink(&first, &link).expect("link");
    let files = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("tool group");
    let prepared = files
        .prepare_write(
            FileWriteInput {
                file_path: "current.txt".into(),
                content: "replacement\n".into(),
            },
            &token(),
        )
        .await
        .expect("prepared write");
    fs::remove_file(&link).expect("remove link");
    symlink(&second, &link).expect("retarget link");

    let error = files
        .execute_prepared_write(prepared, &token())
        .await
        .expect_err("retargeted resource");
    assert!(error.to_string().contains(STALE_PUBLICATION));
    assert_eq!(fs::read_to_string(first).expect("first"), "first\n");
    assert_eq!(fs::read_to_string(second).expect("second"), "second\n");

    let prepared = files
        .prepare_read(
            FileReadInput {
                file_path: "notes.txt".into(),
                offset: None,
                limit: None,
            },
            &token(),
        )
        .await
        .expect("prepared read");
    let other = FileToolGroup::new(&fixture.root, true, None)
        .await
        .expect("other group");
    let error = other
        .execute_prepared_read(prepared, &token())
        .await
        .expect_err("foreign group");
    assert!(error.to_string().contains(FOREIGN_FILE_GROUP));
}

#[tokio::test]
async fn unconfined_read_only_hosting_rejects_mutation() {
    let fixture = fixture();
    let files = FileToolGroup::new_unconfined(&fixture.root, false, None)
        .await
        .expect("unconfined group");
    let outside_file = fixture.outside.join("secret.txt");

    // Reaching outside the base is a confinement decision; writing is a separate one.
    let error = files
        .file_write(
            FileWriteInput {
                file_path: outside_file.to_string_lossy().into_owned(),
                content: "overwritten\n".into(),
            },
            &token(),
        )
        .await
        .expect_err("read-only hosting must reject writes");
    assert_eq!(
        error.to_string(),
        "Filesystem is read-only; restart with write access"
    );
    assert_eq!(fs::read_to_string(&outside_file).unwrap(), "secret\n");
}

#[tokio::test]
async fn traversal_and_read_agree_on_protected_entries_in_both_modes() {
    let fixture = fixture();
    fs::create_dir(fixture.root.join(".ssh")).expect("ssh dir");
    fs::write(fixture.root.join(".ssh/config"), "ssh-secret\n").expect("ssh file");
    fs::write(fixture.root.join(".env.local"), "env-secret\n").expect("env");

    let listed = |files: &FileToolGroup| {
        let files = files.clone();
        async move {
            files
                .file_glob(
                    FileGlobInput {
                        pattern: "**/*".into(),
                        path: None,
                    },
                    &token(),
                )
                .await
                .expect("glob")
                .files
                .into_iter()
                .map(|entry| entry.relative_path)
                .collect::<Vec<_>>()
        }
    };
    let read = |files: &FileToolGroup, path: &str| {
        let files = files.clone();
        let path = path.to_owned();
        async move {
            files
                .file_read(
                    FileReadInput {
                        file_path: path,
                        offset: None,
                        limit: None,
                    },
                    &token(),
                )
                .await
        }
    };

    // Confined: denied by `resolve` and absent from enumeration.
    let confined = FileToolGroup::new(&fixture.root, false, None)
        .await
        .expect("confined group");
    let confined_entries = listed(&confined).await;
    for protected in [".ssh/config", ".env.local"] {
        assert!(
            matches!(
                read(&confined, protected).await,
                Err(FilesystemError::ProtectedPath(_))
            ),
            "{protected} must be denied while confined"
        );
        assert!(
            !confined_entries.iter().any(|entry| entry == protected),
            "{protected} must not be enumerated while confined"
        );
    }

    // Unconfined: readable, and therefore also discoverable. A host cannot authorize what a
    // prepared call would touch if enumeration hides paths that `file_read` will happily return.
    let unconfined = FileToolGroup::new_unconfined(&fixture.root, false, None)
        .await
        .expect("unconfined group");
    let unconfined_entries = listed(&unconfined).await;
    for protected in [".ssh/config", ".env.local"] {
        assert!(
            read(&unconfined, protected).await.is_ok(),
            "{protected} must be readable while unconfined"
        );
        assert!(
            unconfined_entries.iter().any(|entry| entry == protected),
            "{protected} must be enumerated while unconfined; got {unconfined_entries:?}"
        );
    }
}

fn temporary_files(root: &std::path::Path, basename: &str) -> Vec<String> {
    fs::read_dir(root)
        .expect("directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(basename) && name.ends_with(".tmp"))
        .collect()
}
