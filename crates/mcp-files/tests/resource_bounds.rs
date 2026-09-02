use std::{fs, path::Path};

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use workcell_mcp_files::{FileApplyPatchInput, FileGlobInput, FileToolGroup, FilesystemLimits};

fn root() -> TempDir {
    tempfile::tempdir().expect("temporary root")
}

fn token() -> CancellationToken {
    CancellationToken::new()
}

#[tokio::test]
async fn protocol_ceiling_rejects_patch_before_publication() {
    let root = root();
    let large = "old\n".repeat(20_000);
    fs::write(root.path().join("one.txt"), &large).expect("first file");
    fs::write(root.path().join("two.txt"), &large).expect("second file");
    let files = FileToolGroup::new(
        root.path(),
        true,
        Some(FilesystemLimits {
            // A configured limit cannot loosen the protocol ceiling.
            max_patch_result_bytes: usize::MAX,
            ..FilesystemLimits::default()
        }),
    )
    .await
    .expect("tool group");

    let error = files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text: "*** Begin Patch\n*** Delete File: one.txt\n*** Delete File: two.txt\n*** End Patch".into(),
            },
            &token(),
        )
        .await
        .expect_err("wire result must exceed the hard ceiling");

    assert!(error.to_string().contains("maximum size of 64000 bytes"));
    assert_eq!(
        fs::read_to_string(root.path().join("one.txt")).unwrap(),
        large
    );
    assert_eq!(
        fs::read_to_string(root.path().join("two.txt")).unwrap(),
        large
    );
}

#[tokio::test]
async fn aggregate_plan_budget_fails_before_retained_changes_publish() {
    let root = root();
    let original = format!("old\n{}", "x\n".repeat(600));
    fs::write(root.path().join("one.txt"), &original).expect("first file");
    fs::write(root.path().join("two.txt"), &original).expect("second file");
    let files = FileToolGroup::new(
        root.path(),
        true,
        Some(FilesystemLimits {
            max_patch_plan_bytes: 3_000,
            ..FilesystemLimits::default()
        }),
    )
    .await
    .expect("tool group");
    let patch = "*** Begin Patch\n*** Update File: one.txt\n@@\n-old\n+new\n*** Update File: two.txt\n@@\n-old\n+new\n*** End Patch";

    let error = files
        .file_apply_patch(
            FileApplyPatchInput {
                patch_text: patch.into(),
            },
            &token(),
        )
        .await
        .expect_err("second retained update must exceed the plan budget");

    assert!(error.to_string().contains("content budget of 3000 bytes"));
    assert_eq!(
        fs::read_to_string(root.path().join("one.txt")).unwrap(),
        original
    );
    assert_eq!(
        fs::read_to_string(root.path().join("two.txt")).unwrap(),
        original
    );
}

#[tokio::test]
async fn rejects_zero_aggregate_plan_budget() {
    let root = root();
    let error = FileToolGroup::new(
        root.path(),
        false,
        Some(FilesystemLimits {
            max_patch_plan_bytes: 0,
            ..FilesystemLimits::default()
        }),
    )
    .await
    .expect_err("zero plan budget");

    assert_eq!(
        error.to_string(),
        "maxPatchPlanBytes must be a positive integer"
    );
}

#[tokio::test]
async fn traverses_deep_trees_with_deterministic_output_order() {
    let root = root();
    let mut directory = root.path().to_path_buf();
    for _ in 0..300 {
        directory.push("d");
        fs::create_dir(&directory).expect("deep directory");
    }
    fs::write(directory.join("leaf.txt"), "leaf\n").expect("deep leaf");
    write_file(root.path(), "z.txt");
    write_file(root.path(), "a.txt");
    let files = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("tool group");

    let output = files
        .file_glob(
            FileGlobInput {
                pattern: "**/*.txt".into(),
                path: None,
            },
            &token(),
        )
        .await
        .expect("deep glob");
    let paths = output
        .files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect::<Vec<_>>();

    assert!(!output.truncated);
    assert_eq!(paths.first(), Some(&"a.txt"));
    assert_eq!(paths.last(), Some(&"z.txt"));
    assert!(paths.iter().any(|path| path.ends_with("d/leaf.txt")));
}

fn write_file(root: &Path, relative: &str) {
    fs::write(root.join(relative), relative).expect("fixture file");
}
