//! Shared conformance fixtures for the shell tool result contract.
//!
//! The shell result has two forms. `contentText` is what a model reads and is
//! subject to output filtering; `structuredContent` is the canonical capture and
//! never is. These fixtures pin both, so a change to either is a deliberate
//! contract change rather than an incidental one.
//!
//! Cases run a fixture-provided `Makefile` so the command output is fully
//! determined by the corpus rather than by the host. Wall-clock duration and
//! pipe-read chunk count are normalized; progress continuity and the terminal
//! sequence are checked against the delivered chunks.

#![cfg(all(unix, feature = "mcp"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use workcell_mcp_shell::{
    ShellPermissionPolicy, ShellProgressChunk, ShellProgressSink, ShellStream, ShellToolGroup,
};

const CASES: &[&str] = &[
    "filtered-single-scope.json",
    "filtered-empty-success-summary.json",
    "unfiltered-pipeline.json",
    "filter-disabled.json",
    "failure-suppresses-success-summary.json",
    "redraw-rendered-at-capture.json",
    "progress-lines-collapsed.json",
];

#[derive(Default)]
struct RecordingProgress(Mutex<Vec<ShellProgressChunk>>);

#[async_trait]
impl ShellProgressSink for RecordingProgress {
    async fn publish(&self, chunk: ShellProgressChunk) -> Result<(), String> {
        self.0.lock().unwrap().push(chunk);
        Ok(())
    }
}

#[tokio::test]
async fn shell_dispatch_matches_shared_conformance_fixtures() {
    if !make_available() {
        // Cases drive a fixture-provided Makefile. Without `make` the corpus
        // cannot be evaluated, and asserting nothing is better than asserting
        // something the fixture did not describe.
        eprintln!("skipping shell conformance fixtures: `make` is not available");
        return;
    }
    for case in CASES {
        run_case(case).await;
    }
}

fn make_available() -> bool {
    Command::new("make")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

async fn run_case(case: &str) {
    let fixture_root = fixture_root();
    let fixture: Value = serde_json::from_str(
        &fs::read_to_string(fixture_root.join("shell/v1").join(case))
            .unwrap_or_else(|error| panic!("read fixture {case}: {error}")),
    )
    .unwrap_or_else(|error| panic!("parse fixture {case}: {error}"));
    assert_eq!(fixture["fixtureVersion"], json!(1), "{case}");
    assert_eq!(
        fixture["normalization"]["replacements"],
        json!([
            {
                "placeholder": "{{ROOT}}",
                "source": "configured canonical filesystem root"
            },
            {
                "placeholder": "{{DURATION_MS}}",
                "source": "measured command wall-clock duration"
            },
            {
                "placeholder": "{{FINAL_SEQUENCE}}",
                "source": "observed progress chunk count, checked for continuity and byte totals"
            }
        ]),
        "{case} introduced an unreviewed normalization"
    );

    let temporary = TempDir::new().expect("temporary fixture root");
    populate_root(temporary.path(), &fixture_root, &fixture["setup"]["files"]);

    // Shell policy is startup configuration, so a fixture selects a reviewed
    // policy by name rather than carrying rules a tool call could influence.
    let policy = match fixture["configuration"]["shellPolicy"].as_str() {
        Some("yolo") => ShellPermissionPolicy::yolo(),
        Some("restricted") | None => ShellPermissionPolicy::restricted(),
        Some(other) => panic!("{case}: unsupported fixture shell policy {other}"),
    };
    let output_filter = fixture["configuration"]["shellOutputFilter"]
        .as_bool()
        .unwrap_or(true);
    let group = ShellToolGroup::with_policy(temporary.path(), policy)
        .await
        .unwrap_or_else(|error| panic!("initialize {case}: {error}"))
        .with_output_filter(output_filter);

    let progress = Arc::new(RecordingProgress::default());
    let result = group
        .dispatch_with_progress(
            fixture["tool"].as_str().expect("fixture tool"),
            fixture["input"].clone(),
            CancellationToken::new(),
            Some(progress.clone()),
        )
        .await
        .expect("known fixture tool")
        .unwrap_or_else(|error| panic!("dispatch {case}: {error}"));

    assert_progress(case, &result, &progress.0.lock().unwrap());
    assert_result(case, &result, &fixture["expected"], temporary.path());
}

fn assert_progress(case: &str, result: &CallToolResult, chunks: &[ShellProgressChunk]) {
    let structured = result
        .structured_content
        .as_ref()
        .expect("structured content");
    assert_eq!(
        structured["finalSequence"],
        json!(chunks.len()),
        "{case}: terminal progress sequence"
    );
    let mut stdout_bytes = 0;
    let mut stderr_bytes = 0;
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.version, 1, "{case}: progress version");
        assert_eq!(
            chunk.sequence,
            (index + 1) as u64,
            "{case}: progress continuity"
        );
        assert!(!chunk.text.is_empty(), "{case}: empty progress chunk");
        match chunk.stream {
            ShellStream::Stdout => stdout_bytes += chunk.text.len(),
            ShellStream::Stderr => stderr_bytes += chunk.text.len(),
        }
    }
    assert_eq!(
        structured["stdoutUtf8Bytes"],
        json!(stdout_bytes),
        "{case}: stdout progress bytes"
    );
    assert_eq!(
        structured["stderrUtf8Bytes"],
        json!(stderr_bytes),
        "{case}: stderr progress bytes"
    );
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

fn assert_result(case: &str, result: &CallToolResult, expected: &Value, root: &Path) {
    assert_ne!(result.is_error, Some(true), "{case}");
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("{case}: expected text content")
    };
    let root = root.to_string_lossy();
    let structured = normalize(
        result
            .structured_content
            .clone()
            .expect("structured content"),
        root.as_ref(),
    );

    // Invariant cases exist where a value is stable in meaning but not in bytes.
    // A failing `make` reports its own diagnostic, whose exact wording varies by
    // implementation and version, so the assertion is what the contract
    // guarantees: a failed command is never rendered as a success summary.
    if let Some(invariants) = expected.get("invariants") {
        let text = content.text.replace(root.as_ref(), "{{ROOT}}");
        for needle in invariants["contentIncludes"]
            .as_array()
            .expect("contentIncludes")
        {
            let needle = needle.as_str().expect("include needle");
            assert!(
                text.contains(needle),
                "{case}: content must include {needle}"
            );
        }
        for needle in invariants["contentExcludes"]
            .as_array()
            .expect("contentExcludes")
        {
            let needle = needle.as_str().expect("exclude needle");
            assert!(
                !text.contains(needle),
                "{case}: content must not include {needle}"
            );
        }
        let stdout = structured["stdout"].as_str().expect("structured stdout");
        for needle in invariants["structuredStdoutIncludes"]
            .as_array()
            .expect("structuredStdoutIncludes")
        {
            let needle = needle.as_str().expect("stdout needle");
            assert!(
                stdout.contains(needle),
                "{case}: the unfiltered capture must retain {needle}"
            );
        }
        let zero = structured["exitCode"] == json!(0);
        assert_eq!(
            zero,
            invariants["exitCodeZero"].as_bool().expect("exitCodeZero"),
            "{case}: exit-code classification"
        );
        return;
    }

    assert_eq!(
        content.text.replace(root.as_ref(), "{{ROOT}}"),
        expected["contentText"].as_str().expect("expected text"),
        "{case}: content text"
    );
    assert_eq!(
        structured, expected["structuredContent"],
        "{case}: structured content"
    );
}

/// Chunk boundaries depend on pipe scheduling; their sequence is checked before normalization.
fn normalize(value: Value, root: &str) -> Value {
    match value {
        Value::String(value) => Value::String(value.replace(root, "{{ROOT}}")),
        Value::Array(values) => {
            Value::Array(values.into_iter().map(|v| normalize(v, root)).collect())
        }
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| {
                    if key == "durationMs" {
                        (key, Value::String("{{DURATION_MS}}".into()))
                    } else if key == "finalSequence" {
                        (key, Value::String("{{FINAL_SEQUENCE}}".into()))
                    } else {
                        (key, normalize(value, root))
                    }
                })
                .collect(),
        ),
        other => other,
    }
}
