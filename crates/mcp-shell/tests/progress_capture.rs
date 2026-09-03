//! End-to-end behaviour of a redrawing command, through real process execution.
//!
//! Two properties justify rendering a redraw stream where the capture ring is
//! filled rather than where the model-facing text is built, and neither can be
//! observed in a unit test of the renderer:
//!
//! * Frames must not evict real output from the ring. A bar that redraws for the
//!   length of a build writes far more than the ring holds, so a reader that
//!   rendered afterwards would find the frames intact and everything printed
//!   before them gone.
//! * Progress notifications must stay byte-exact. They are what a host has if it
//!   wants the frames, so rendering the capture is only safe while they carry
//!   what the process actually wrote.

#![cfg(unix)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use workcell_mcp_shell::{
    ShellExecution, ShellInput, ShellPermissionPolicy, ShellProgressChunk, ShellProgressSink,
    ShellToolGroup,
};

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
    command: &str,
    progress: Option<Arc<RecordingSink>>,
) -> (tempfile::TempDir, ShellExecution) {
    let root = tempfile::tempdir().expect("root");
    let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
        .await
        .expect("group");
    let execution = group
        .execute(
            ShellInput {
                command: command.into(),
                timeout: Some(120_000),
                workdir: None,
            },
            CancellationToken::new(),
            progress.map(|sink| sink as Arc<dyn ShellProgressSink>),
        )
        .await
        .expect("admitted")
        .expect("completed");
    (root, execution)
}

/// A shell loop that redraws one row many times, with real output on both sides.
///
/// `printf` rather than a language runtime, so the test measures the capture
/// path on every host rather than whichever progress library happens to be
/// installed. The fixtures under `crates/output-filter/tests/fixtures/progress`
/// cover the real libraries.
fn redrawing_command(frames: usize) -> String {
    // Only `{` and `}` are escapes for `format!`; the per-cent signs below are
    // printf's and are passed through as written.
    format!(
        "printf 'configuration loaded from /etc/app.conf\\n'; \
         for i in $(seq 1 {frames}); do printf 'downloading %d/{frames} [%d%%]\\r' \"$i\" \"$((i * 100 / {frames}))\"; done; \
         printf '\\n'; printf 'wrote 4 artifacts\\n'"
    )
}

#[tokio::test]
async fn a_redrawing_command_reaches_the_model_as_its_final_frame() {
    let (_root, execution) = run(&redrawing_command(2_000), None).await;

    assert_eq!(execution.output.exit_code, Some(0));
    let stdout = &execution.output.stdout;
    assert!(
        !stdout.contains('\r'),
        "capture still holds a control stream"
    );
    assert_eq!(
        stdout,
        "configuration loaded from /etc/app.conf\ndownloading 2000/2000 [100%]\nwrote 4 artifacts\n",
        "expected the rendered rows only"
    );
    assert_eq!(execution.output.stdout_redraws_collapsed, 1_999);
    assert!(
        execution
            .model_text
            .contains("[1999 progress redraws collapsed]")
    );

    // The command wrote far more than survives, and says so.
    assert!(
        execution.output.stdout_utf8_bytes > 40_000,
        "expected the byte counter to report what the process wrote, got {}",
        execution.output.stdout_utf8_bytes
    );
}

#[tokio::test]
async fn frames_do_not_evict_the_output_printed_before_them() {
    // The load-bearing property. This command writes well past the one-megabyte
    // capture ring in frames alone, so without rendering on the way in the first
    // line would be long gone by the time anything could reduce it.
    let (_root, execution) = run(&redrawing_command(60_000), None).await;

    assert!(
        execution.output.stdout_utf8_bytes > 1_500_000,
        "test must overrun the capture ring to be meaningful, wrote {}",
        execution.output.stdout_utf8_bytes
    );
    assert!(
        execution
            .output
            .stdout
            .contains("configuration loaded from /etc/app.conf"),
        "output printed before the bar was evicted by it: {:?}",
        execution.output.stdout
    );
    assert!(execution.output.stdout.contains("wrote 4 artifacts"));
    assert!(
        !execution.output.stdout_capture_truncated,
        "the rendered capture should fit the ring comfortably"
    );
}

#[tokio::test]
async fn progress_notifications_still_carry_every_frame() {
    // Rendering the capture is only defensible while the streaming surface is
    // untouched: that is where a host goes for the frames themselves.
    let sink = Arc::new(RecordingSink::default());
    let (_root, execution) = run(&redrawing_command(500), Some(Arc::clone(&sink))).await;

    let streamed: String = sink
        .chunks
        .lock()
        .expect("sink lock")
        .iter()
        .map(|chunk| chunk.text.clone())
        .collect();

    assert_eq!(
        streamed.matches('\r').count(),
        500,
        "every redraw must reach a streaming host"
    );
    assert!(streamed.contains("downloading 1/500 ["));
    assert!(streamed.contains("downloading 250/500 ["));
    // The same command, two surfaces: exact on the wire, rendered in the result.
    assert!(!execution.output.stdout.contains("downloading 250/500 ["));
    assert_eq!(
        streamed.len() as u64,
        execution.output.stdout_utf8_bytes,
        "the byte counter must describe the streamed bytes"
    );
}

#[tokio::test]
async fn a_command_killed_mid_redraw_still_renders_its_last_frame() {
    // A long training run that hits the timeout ends with no trailing newline and
    // a row still under construction. It must not be dropped.
    let (_root, execution) = run(
        "printf 'starting\\n'; printf 'step 1/3\\rstep 2/3\\rstep 3/3'",
        None,
    )
    .await;
    assert_eq!(execution.output.stdout, "starting\nstep 3/3");
    assert_eq!(execution.output.stdout_redraws_collapsed, 2);
}

#[tokio::test]
async fn output_without_redraws_is_unchanged_and_unannounced() {
    let (_root, execution) = run("printf 'alpha\\nbeta  \\ngamma\\n'", None).await;
    assert_eq!(execution.output.stdout, "alpha\nbeta  \ngamma\n");
    assert_eq!(execution.output.stdout_redraws_collapsed, 0);
    assert!(execution.filter.is_none());
    assert!(!execution.model_text.contains("progress"));
}

#[tokio::test]
async fn line_separated_frames_are_collapsed_for_an_unruled_command() {
    // No corpus rule names this command, which is the case that matters: the
    // programs that emit bars are overwhelmingly ones no rule can anticipate.
    let (_root, execution) = run(
        "printf 'begin\\n'; for i in 1 2 3 4 5 6; do printf 'fetch %d/6 [%d%%] 4.8it/s\\n' \"$i\" \"$((i * 100 / 6))\"; done; printf 'end\\n'",
        None,
    )
    .await;

    let filter = execution.filter.as_ref().expect("progress is collapsed");
    assert_eq!(filter.stages, ["progress"]);
    assert!(filter.filtered_utf8_bytes < filter.unfiltered_utf8_bytes);

    assert!(
        execution
            .model_text
            .contains("... (5 progress updates collapsed)")
    );
    assert!(execution.model_text.contains("fetch 6/6 [100%] 4.8it/s"));
    assert!(execution.model_text.contains("begin"));
    assert!(execution.model_text.contains("end"));
    assert!(execution.model_text.ends_with("[filtered: progress]"));

    // Collapsing is a rendering judgement, so the capture keeps every line.
    assert!(execution.output.stdout.contains("fetch 1/6"));
    assert!(execution.output.stdout.contains("fetch 5/6"));
}

#[tokio::test]
async fn disabling_the_filter_leaves_the_line_collapse_off_too() {
    let root = tempfile::tempdir().expect("root");
    let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
        .await
        .expect("group")
        .with_output_filter(false);
    let execution = group
        .execute(
            ShellInput {
                command: "printf 'begin\\n'; for i in 1 2 3 4 5 6; do printf 'fetch %d/6 [%d%%] 4.8it/s\\n' \"$i\" \"$((i * 100 / 6))\"; done".into(),
                timeout: Some(60_000),
                workdir: None,
            },
            CancellationToken::new(),
            None,
        )
        .await
        .expect("admitted")
        .expect("completed");

    assert!(execution.filter.is_none());
    assert!(execution.model_text.contains("fetch 1/6"));
    assert!(execution.model_text.contains("fetch 5/6"));
}

/// Whether a real `tqdm` is importable, so the live case can be skipped rather
/// than turned into a test of whichever Python happens to be installed.
fn tqdm_available() -> bool {
    std::process::Command::new("python3")
        .args(["-c", "import tqdm"])
        .output()
        .is_ok_and(|output| output.status.success())
}

#[tokio::test]
async fn a_real_tqdm_bar_survives_as_its_completed_frame() {
    // The committed fixtures hold captured `tqdm` bytes and the eval harness runs
    // the library live. This case closes the gap between them by driving the real
    // library through real process execution, which is the only place the pipe,
    // the chunking, the ring, and the rendering all meet.
    if !tqdm_available() {
        eprintln!("skipping live tqdm case: python3 cannot import tqdm");
        return;
    }
    let (_root, execution) = run(
        "python3 -c 'import time\nfrom tqdm import tqdm\nfor _ in tqdm(range(300), desc=\"Loading weights\"): time.sleep(0.004)'",
        None,
    )
    .await;

    assert_eq!(execution.output.exit_code, Some(0));
    let stderr = &execution.output.stderr;
    assert!(!stderr.contains('\r'), "got {stderr:?}");
    assert_eq!(stderr.lines().count(), 1, "got {stderr:?}");
    assert!(
        stderr.starts_with("Loading weights: 100%|"),
        "expected the completed frame, got {stderr:?}"
    );
    assert!(stderr.contains("| 300/300 ["), "got {stderr:?}");
    assert!(execution.output.stderr_redraws_collapsed >= 5);
    // tqdm throttles its own redraw rate, so the exact saving varies with speed;
    // the contract is that the reader pays for one frame rather than all of them.
    assert!(
        (stderr.len() as u64) * 4 < execution.output.stderr_utf8_bytes,
        "expected a large reduction, kept {} of {} bytes",
        stderr.len(),
        execution.output.stderr_utf8_bytes
    );
}

#[tokio::test]
async fn rendering_is_not_a_filtering_choice() {
    // Decoding a control stream is not a judgement about content, so it applies
    // even with the rule corpus disabled. A host that turns filtering off wants
    // fewer opinions, not a raw terminal control stream.
    let root = tempfile::tempdir().expect("root");
    let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
        .await
        .expect("group")
        .with_output_filter(false);
    let execution = group
        .execute(
            ShellInput {
                command: "printf 'step 1/3\\rstep 2/3\\rstep 3/3\\n'".into(),
                timeout: Some(60_000),
                workdir: None,
            },
            CancellationToken::new(),
            None,
        )
        .await
        .expect("admitted")
        .expect("completed");

    assert_eq!(execution.output.stdout, "step 3/3\n");
    assert_eq!(execution.output.stdout_redraws_collapsed, 2);
}
