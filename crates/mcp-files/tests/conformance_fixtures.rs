#![cfg(all(unix, feature = "mcp"))]

use std::fs;
use std::path::{Path, PathBuf};

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use workcell_mcp_files::FileToolGroup;

const CASES: &[&str] = &[
    "read-file.json",
    "read-directory.json",
    "glob.json",
    "grep.json",
    "write.json",
    "write-without-write-access.json",
    "edit.json",
    "apply-patch.json",
    #[cfg(feature = "index")]
    "index.json",
];

#[tokio::test]
async fn filesystem_dispatch_matches_shared_conformance_fixtures() {
    for case in CASES {
        run_case(case).await;
    }
}

async fn run_case(case: &str) {
    let fixture_root = fixture_root();
    let fixture: Value = serde_json::from_str(
        &fs::read_to_string(fixture_root.join("filesystem/v1").join(case))
            .unwrap_or_else(|error| panic!("read fixture {case}: {error}")),
    )
    .unwrap_or_else(|error| panic!("parse fixture {case}: {error}"));
    assert_eq!(fixture["fixtureVersion"], json!(1), "{case}");
    assert_eq!(
        fixture["normalization"]["replacements"],
        json!([{
            "placeholder": "{{ROOT}}",
            "source": "configured canonical filesystem root"
        }]),
        "{case} introduced an unreviewed normalization"
    );

    let temporary = TempDir::new().expect("temporary fixture root");
    populate_root(temporary.path(), &fixture_root, &fixture["setup"]["files"]);
    let allow_write = fixture["configuration"]["allowWrite"]
        .as_bool()
        .unwrap_or(false);
    let group = FileToolGroup::new(temporary.path(), allow_write, None)
        .await
        .unwrap_or_else(|error| panic!("initialize {case}: {error}"));
    let input = resolve_assets(fixture["input"].clone(), &fixture_root);
    let tool = fixture["tool"].as_str().expect("fixture tool");
    let dispatched = group.dispatch(tool, input, CancellationToken::new()).await;

    // A case may assert that the configuration removes the tool entirely. The
    // group must then neither advertise nor answer for the name.
    if fixture["expected"]["toolAvailable"] == json!(false) {
        assert!(
            dispatched.is_none(),
            "{case}: {tool} must not be dispatchable"
        );
        assert!(
            !group.catalog().iter().any(|listed| listed.name == tool),
            "{case}: {tool} must not be advertised"
        );
    } else {
        let result = dispatched
            .expect("known fixture tool")
            .unwrap_or_else(|error| panic!("dispatch {case}: {error}"));
        assert_result(case, &result, &fixture["expected"], temporary.path());
    }
    assert_post_filesystem(case, temporary.path(), &fixture_root, &fixture["expected"]);
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/mcp-conformance")
        .canonicalize()
        .expect("canonical conformance fixture root")
}

fn populate_root(root: &Path, fixture_root: &Path, files: &Value) {
    for file in files.as_array().expect("setup files") {
        let destination = root.join(file["path"].as_str().expect("setup path"));
        fs::create_dir_all(destination.parent().expect("setup parent")).expect("setup directory");
        fs::copy(
            fixture_root.join(file["asset"].as_str().expect("setup asset")),
            destination,
        )
        .expect("copy setup asset");
    }
}

fn resolve_assets(value: Value, fixture_root: &Path) -> Value {
    match value {
        Value::Object(mut object) if object.contains_key("$asset") => {
            assert_eq!(object.remove("encoding"), Some(json!("utf8")));
            let asset = object
                .remove("$asset")
                .and_then(|value| value.as_str().map(str::to_owned))
                .expect("input asset");
            assert!(object.is_empty(), "unsupported input asset fields");
            Value::String(
                fs::read_to_string(fixture_root.join(asset)).expect("read UTF-8 input asset"),
            )
        }
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, resolve_assets(value, fixture_root)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| resolve_assets(value, fixture_root))
                .collect(),
        ),
        other => other,
    }
}

fn assert_result(case: &str, result: &CallToolResult, expected: &Value, root: &Path) {
    assert_ne!(result.is_error, Some(true), "{case}");
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("{case}: expected text content")
    };
    let root = root.to_string_lossy();

    // {{ROOT}} is the sole allowlisted filesystem normalization. Replacing the
    // exact canonical root in strings leaves all semantic fields and text intact.
    assert_eq!(
        content.text.replace(root.as_ref(), "{{ROOT}}"),
        expected["contentText"].as_str().expect("expected text"),
        "{case}: content text"
    );
    let actual = normalize_root(
        result
            .structured_content
            .clone()
            .expect("structured content"),
        root.as_ref(),
    );
    assert_eq!(
        actual, expected["structuredContent"],
        "{case}: structured content"
    );
}

fn normalize_root(value: Value, root: &str) -> Value {
    match value {
        Value::String(value) => Value::String(value.replace(root, "{{ROOT}}")),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| normalize_root(value, root))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, normalize_root(value, root)))
                .collect(),
        ),
        other => other,
    }
}

fn assert_post_filesystem(case: &str, root: &Path, fixture_root: &Path, expected: &Value) {
    let expected_files = expected["postFilesystem"]
        .as_array()
        .expect("expected post-filesystem");
    let expected_paths = expected_files
        .iter()
        .map(|file| file["path"].as_str().expect("post path").to_owned())
        .collect::<Vec<_>>();
    let mut actual_paths = Vec::new();
    collect_files(root, root, &mut actual_paths);
    actual_paths.sort();
    assert_eq!(actual_paths, expected_paths, "{case}: complete file state");

    for file in expected_files {
        let relative = file["path"].as_str().expect("post path");
        let expected_content =
            fs::read(fixture_root.join(file["asset"].as_str().expect("post asset")))
                .expect("read expected post asset");
        assert_eq!(
            fs::read(root.join(relative)).expect("read actual post file"),
            expected_content,
            "{case}: post-state content for {relative}"
        );
    }
}

fn collect_files(root: &Path, directory: &Path, output: &mut Vec<String>) {
    for entry in fs::read_dir(directory).expect("read post-state directory") {
        let entry = entry.expect("post-state entry");
        let path = entry.path();
        if entry.file_type().expect("post-state file type").is_dir() {
            collect_files(root, &path, output);
        } else {
            output.push(
                path.strip_prefix(root)
                    .expect("post-state relative path")
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
}
