//! End-to-end behaviour of a command that writes terminal escape sequences.
//!
//! Escape removal is command-independent for the same reason the progress
//! collapse is, and the case that matters cannot be observed in a unit test of
//! the strip: a request that resolves to more than one command scope selects no
//! rule at all, so a rule-gated strip never runs on it. An ad-hoc script with
//! hardcoded colour is exactly such a request.

#![cfg(unix)]

use tokio_util::sync::CancellationToken;
use workcell_mcp_shell::{ShellExecution, ShellInput, ShellPermissionPolicy, ShellToolGroup};

async fn run_with_filter(command: &str, filter: bool) -> (tempfile::TempDir, ShellExecution) {
    let root = tempfile::tempdir().expect("root");
    let group = ShellToolGroup::with_policy(root.path(), ShellPermissionPolicy::yolo())
        .await
        .expect("group")
        .with_output_filter(filter);
    let execution = group
        .execute(
            ShellInput {
                command: command.into(),
                timeout_sec: Some(60),
                workdir: None,
            },
            CancellationToken::new(),
            None,
        )
        .await
        .expect("admitted")
        .expect("completed");
    (root, execution)
}

async fn run(command: &str) -> (tempfile::TempDir, ShellExecution) {
    run_with_filter(command, true).await
}

/// Five coloured status lines from a chain of commands, as a hand-written deploy
/// script produces. Nine escape bytes per line, comfortably more than the notice
/// a filtered rendering pays for.
const COLOURED_SCRIPT: &str = concat!(
    r"printf '\033[33m!\033[0m remote not configured\n';",
    r"printf '\033[33m!\033[0m WEBROOT unset\n';",
    r"printf '\033[36m*\033[0m no local mirror yet\n';",
    r"printf '\033[36m*\033[0m not mounted\n';",
    r"printf '\033[31merror\033[0m remote not configured\n'",
);

#[tokio::test]
async fn a_multi_scope_script_has_its_colour_stripped() {
    let (_root, execution) = run(COLOURED_SCRIPT).await;

    let filter = execution.filter.as_ref().expect("escapes are stripped");
    assert_eq!(filter.stages, ["escapes"]);
    assert!(filter.filtered_utf8_bytes < filter.unfiltered_utf8_bytes);

    assert!(
        !execution.model_text.contains('\u{1b}'),
        "no escape may reach the model: {:?}",
        execution.model_text
    );
    assert!(execution.model_text.contains("! remote not configured"));
    assert!(execution.model_text.contains("error remote not configured"));
    assert!(execution.model_text.ends_with("[filtered: escapes]"));

    // Stripping is a rendering judgement, so the capture keeps the bytes.
    assert!(execution.output.stdout.contains('\u{1b}'));
}

#[tokio::test]
async fn stripping_leaves_every_line_intact() {
    // The strip removes decoration and nothing else. A reduction that dropped,
    // joined, or reordered a line would need announcing far more loudly than a
    // one-word stage name.
    let (_root, execution) = run(COLOURED_SCRIPT).await;
    let body = execution
        .model_text
        .strip_suffix("\n\n[filtered: escapes]")
        .expect("the notice is the last line");
    assert_eq!(
        body.lines().collect::<Vec<_>>(),
        [
            "! remote not configured",
            "! WEBROOT unset",
            "* no local mirror yet",
            "* not mounted",
            "error remote not configured",
        ]
    );
}

#[tokio::test]
async fn even_a_single_sequence_pays_for_its_notice() {
    // Filtering never enlarges a result, and for escapes that guard can never
    // fire: an unfiltered rendering carries the `stdout tail:`/`stderr tail:`
    // framing that a filtered one sheds, which already exceeds the notice. Nine
    // bytes of escape against a twenty-byte notice still comes out smaller.
    let (_root, execution) = run(r"printf '\033[32mok\033[0m\n'").await;
    let filter = execution.filter.as_ref().expect("escapes are stripped");
    assert_eq!(filter.stages, ["escapes"]);
    assert!(filter.filtered_utf8_bytes < filter.unfiltered_utf8_bytes);
    assert!(!execution.model_text.contains('\u{1b}'));
}

#[tokio::test]
async fn disabling_the_filter_keeps_the_escapes() {
    let (_root, execution) = run_with_filter(COLOURED_SCRIPT, false).await;
    assert!(execution.filter.is_none());
    assert!(
        execution.model_text.contains('\u{1b}'),
        "an unfiltered rendering must be what the command wrote"
    );
}

#[tokio::test]
async fn the_caret_form_survives_the_strip() {
    // The documented way to read escapes back: `cat -v` renders ESC as a
    // printable `^[` before the strip sees it. This is the whole reason the
    // shell tool needs no per-call opt-out, so it is asserted end to end.
    let (_root, execution) = run(&format!("{{ {COLOURED_SCRIPT}; }} | cat -v")).await;
    assert!(!execution.model_text.contains('\u{1b}'));
    assert!(
        execution.model_text.contains("^[[33m") && execution.model_text.contains("^[[31m"),
        "the caret form must reach the model: {:?}",
        execution.model_text
    );
}

#[tokio::test]
async fn the_child_environment_asks_tools_not_to_colour() {
    // Preventing the bytes beats deleting them. This is observable only through
    // a real process, which is why it is asserted here as well as in the unit
    // test for the environment itself.
    let (_root, execution) = run(r#"printf '%s/%s\n' "$NO_COLOR" "$CLICOLOR""#).await;
    assert_eq!(execution.output.stdout, "1/0\n");
}
