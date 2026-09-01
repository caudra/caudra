//! A native embedding must be able to see both renderings of one command.
//!
//! Output filtering exists to shrink what a model reads, not to discard what a
//! host captured. A host that streams live output, writes a transcript, or
//! reimplements its own presentation needs the raw bytes, so filtering must
//! never be the only form a command's output survives in.
//!
//! These tests use no MCP types. Everything asserted here is reachable from a
//! host that never links a protocol adapter.

#![cfg(unix)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use workcell_mcp_shell::{
    ShellExecution, ShellInput, ShellPermissionPolicy, ShellProgressChunk, ShellProgressSink,
    ShellToolGroup,
};

/// Emits `make[1]:` wrapper lines that the `make` rule strips, around a line it
/// keeps, so filtered and raw renderings are guaranteed to differ.
const MAKEFILE: &str = "all:\n\t@echo \"make[1]: Entering directory '/x'\"\n\t@echo \"real build line\"\n\t@echo \"make[1]: Leaving directory '/x'\"\n";

#[derive(Default)]
struct RecordingSink {
    chunks: Mutex<Vec<ShellProgressChunk>>,
}

#[async_trait]
impl ShellProgressSink for RecordingSink {
    async fn publish(&self, chunk: ShellProgressChunk) -> Result<(), String> {
        self.chunks.lock().expect("sink lock").push(chunk);
        Ok(())
    }
}

async fn run(
    output_filter: bool,
    progress: Option<Arc<RecordingSink>>,
) -> Option<(tempfile::TempDir, ShellExecution)> {
    if std::process::Command::new("make")
        .arg("--version")
        .output()
        .is_err()
    {
        return None;
    }
    let root = tempfile::tempdir().expect("root");
    std::fs::write(root.path().join("Makefile"), MAKEFILE).expect("makefile");
    let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
        .await
        .expect("group")
        .with_output_filter(output_filter);
    let execution = group
        .execute(
            ShellInput {
                command: "make all".into(),
                timeout: Some(60_000),
                workdir: None,
            },
            CancellationToken::new(),
            progress.map(|sink| sink as Arc<dyn ShellProgressSink>),
        )
        .await
        .expect("admitted")
        .expect("completed");
    Some((root, execution))
}

#[tokio::test]
async fn native_hosts_see_the_filtered_rendering_and_the_raw_capture() {
    let Some((_root, execution)) = run(true, None).await else {
        return;
    };

    // The model-facing rendering drops the wrapper lines.
    assert!(
        execution.model_text.contains("real build line"),
        "filtered rendering must keep signal: {:?}",
        execution.model_text
    );
    assert!(
        !execution.model_text.contains("Entering directory"),
        "filtered rendering must drop noise: {:?}",
        execution.model_text
    );

    // The captured tail on the same value is untouched, so a host that wants the
    // real bytes never has to turn filtering off to get them.
    assert!(
        execution.output.stdout.contains("Entering directory"),
        "raw capture must survive filtering: {:?}",
        execution.output.stdout
    );
    assert!(execution.output.stdout.contains("real build line"));
    assert_eq!(execution.output.exit_code, Some(0));
    let filter = execution.filter.as_ref().expect("make output is filtered");
    assert_eq!(filter.rule, "make");
    let unfiltered = format!(
        "stdout tail:\n{}\nstderr tail:\n{}",
        execution.output.stdout, execution.output.stderr
    );
    assert_eq!(filter.unfiltered_utf8_bytes, unfiltered.len());
    assert_eq!(filter.filtered_utf8_bytes, execution.model_text.len());
    assert!(filter.filtered_utf8_bytes < filter.unfiltered_utf8_bytes);

    // Byte accounting describes the process, not the rendering.
    assert!(execution.output.stdout_utf8_bytes >= execution.output.stdout.len() as u64);
}

#[tokio::test]
async fn live_progress_chunks_are_never_filtered() {
    let sink = Arc::new(RecordingSink::default());
    let Some((_root, execution)) = run(true, Some(Arc::clone(&sink))).await else {
        return;
    };

    let streamed = sink
        .chunks
        .lock()
        .expect("sink lock")
        .iter()
        .map(|chunk| chunk.text.clone())
        .collect::<String>();

    // Chunks are published while the process runs, before an exit code exists,
    // so they carry exactly what the process wrote.
    assert!(
        streamed.contains("Entering directory"),
        "live progress must not be filtered: {streamed:?}"
    );
    assert!(streamed.contains("real build line"));
    assert!(!execution.model_text.contains("Entering directory"));
}

#[tokio::test]
async fn disabling_the_filter_makes_both_forms_raw() {
    let Some((_root, execution)) = run(false, None).await else {
        return;
    };
    assert!(execution.model_text.contains("Entering directory"));
    assert!(execution.output.stdout.contains("Entering directory"));
    assert_eq!(execution.filter, None);
}
