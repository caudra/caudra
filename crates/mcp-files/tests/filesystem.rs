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
    FileReadOutput, FileToolGroup, FileWriteInput, FilesystemError, FilesystemLimits,
};

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
                dry_run: None,
            },
            &token(),
        )
        .await;
    assert!(matches!(write, Err(FilesystemError::RootEscape(_))));

    let patch = files
        .file_apply_patch(
            FileApplyPatchInput {
                dry_run: Some(true),
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
                dry_run: None,
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
                dry_run: None,
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
                dry_run: None,
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
                    dry_run: Some(true),
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
                    dry_run: Some(true),
                },
                &token(),
            )
            .await
            .map(|_| ()),
        files
            .file_apply_patch(
                FileApplyPatchInput {
                    patch_text: "*** Begin Patch\n*** Update File: document.txt\n@@\n-%PDF-1.7\n+replacement\n needle\n*** End Patch".into(),
                    dry_run: Some(true),
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

#[tokio::test]
async fn is_read_only_by_default_but_permits_mutation_previews() {
    let fixture = fixture();
    let files = FileToolGroup::new(&fixture.root, false, None)
        .await
        .expect("tool group");
    let preview = files
        .file_write(
            FileWriteInput {
                file_path: "new.txt".into(),
                content: "hello\n".into(),
                dry_run: Some(true),
            },
            &token(),
        )
        .await
        .expect("preview");
    assert!(!preview.applied);
    assert!(!fixture.root.join("new.txt").exists());

    let denied = files
        .file_write(
            FileWriteInput {
                file_path: "new.txt".into(),
                content: "hello\n".into(),
                dry_run: None,
            },
            &token(),
        )
        .await
        .expect_err("read-only");
    assert!(denied.to_string().contains("read-only"));

    let patch = files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text: "*** Begin Patch\n*** Add File: preview.txt\n+preview\n*** End Patch"
                    .into(),
                dry_run: Some(true),
            },
            &token(),
        )
        .await
        .expect("patch preview");
    assert!(!patch.applied);
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
                dry_run: None,
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
                dry_run: None,
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
        dry_run: None,
    };
    let second_patch = FileApplyPatchInput {
        patch_text: "*** Begin Patch\n*** Add File: raced.txt\n+second\n*** End Patch".into(),
        dry_run: None,
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
                dry_run: None,
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
                dry_run: None,
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
                dry_run: None,
            },
            &token(),
        )
        .await
        .expect("edit");

    let patch_text = "*** Begin Patch\n*** Update File: new.txt\n*** Move to: moved.txt\n@@\n-one\n+ONE\n three\n*** Add File: added.txt\n+added\n*** Delete File: notes.txt\n*** End Patch";
    let preview = files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text: patch_text.into(),
                dry_run: Some(true),
            },
            &token(),
        )
        .await
        .expect("preview");
    assert!(!preview.applied);
    assert_eq!(preview.files.len(), 3);
    assert!(fixture.root.join("new.txt").exists());

    let applied = files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text: patch_text.into(),
                dry_run: None,
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
                dry_run: None,
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
                dry_run: None,
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
async fn bounds_diff_previews_and_rejects_oversized_patch_results_before_publication() {
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

    let preview = files
        .file_edit(
            FileEditInput {
                file_path: "large.txt".into(),
                old_string: "old".into(),
                new_string: "new".into(),
                replace_all: Some(true),
                dry_run: Some(true),
            },
            &token(),
        )
        .await
        .expect("bounded preview");
    assert!(preview.diff.truncated);
    assert!(preview.diff.patch.len() <= 128);
    assert_eq!(
        serde_json::to_value(&preview).expect("serialized preview")["diff"]["truncated"],
        serde_json::json!(true)
    );

    let patch_preview_files = FileToolGroup::new(
        &fixture.root,
        true,
        Some(FilesystemLimits {
            max_diff_bytes: 128,
            max_patch_result_bytes: 4 * 1024,
            ..FilesystemLimits::default()
        }),
    )
    .await
    .expect("patch preview group");
    let patch_preview = patch_preview_files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text: "*** Begin Patch\n*** Delete File: large.txt\n*** End Patch".into(),
                dry_run: Some(true),
            },
            &token(),
        )
        .await
        .expect("bounded patch preview");
    assert!(patch_preview.truncated);
    assert!(patch_preview.files[0].truncated);

    let original = fs::read_to_string(fixture.root.join("notes.txt")).expect("original");
    let result = files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text:
                    "*** Begin Patch\n*** Update File: notes.txt\n@@\n-alpha\n+changed\n*** End Patch"
                        .into(),
                dry_run: None,
            },
            &token(),
        )
        .await
        .expect_err("result budget must reject before publication");
    assert!(
        result
            .to_string()
            .contains("Patch result exceeds maximum size")
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("notes.txt")).expect("unchanged"),
        original
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

fn temporary_files(root: &std::path::Path, basename: &str) -> Vec<String> {
    fs::read_dir(root)
        .expect("directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(basename) && name.ends_with(".tmp"))
        .collect()
}
