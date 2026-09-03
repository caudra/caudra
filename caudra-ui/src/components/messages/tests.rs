use super::segment;
use super::*;
use crate::chat::{DONE_TEXT, ERROR_TEXT};
use crate::components::scrollbar::SCROLLBAR_THUMB;
use crate::repaint::expect::{OWED, QUIET};
use crate::selection::{Selection, SelectionZone};
use caudra_agent::tools::{
    BATCH_TOOL_NAME, CODE_EXECUTION_TOOL_NAME, FILE_APPLY_PATCH_TOOL_NAME, FILE_EDIT_TOOL_NAME,
    FILE_GLOB_TOOL_NAME, FILE_GREP_TOOL_NAME, FILE_READ_TOOL_NAME, FILE_WRITE_TOOL_NAME,
    INDEX_TOOL_NAME, MEMORY_TOOL_NAME, QUESTION_TOOL_NAME, SHELL_TOOL_NAME, TASK_TOOL_NAME,
    TODOWRITE_TOOL_NAME, TOOL_OUTPUT_GREP_TOOL_NAME, TOOL_OUTPUT_READ_TOOL_NAME, ToolEffect,
    VIEW_IMAGE_TOOL_NAME,
};
use caudra_agent::{
    GrepFileEntry, GrepMatchGroup, ShellFilterInfo, ShellOutput, SnapshotLine, SnapshotSpan,
    SpanStyle, ToolInput, ToolOutput,
};
use ratatui::backend::TestBackend;
use ratatui::style::Modifier;
use std::collections::HashSet;
use std::time::Duration;
use test_case::test_case;

const SPINNER_GLYPHS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

fn snap_line(text: &str) -> SnapshotLine {
    SnapshotLine {
        spans: vec![SnapshotSpan {
            text: text.into(),
            style: SpanStyle::Default,
        }],
    }
}

/// Mirrors what each tool registers as in production, so a card in a test
/// answers the disclosure question the way the same card would at runtime.
fn effect_of(tool: &str) -> ToolEffect {
    match tool {
        FILE_READ_TOOL_NAME
        | FILE_GREP_TOOL_NAME
        | FILE_GLOB_TOOL_NAME
        | INDEX_TOOL_NAME
        | VIEW_IMAGE_TOOL_NAME
        | TOOL_OUTPUT_READ_TOOL_NAME
        | TOOL_OUTPUT_GREP_TOOL_NAME => ToolEffect::ReadOnly,
        BATCH_TOOL_NAME | TASK_TOOL_NAME => ToolEffect::Orchestrator,
        CODE_EXECUTION_TOOL_NAME | TODOWRITE_TOOL_NAME | QUESTION_TOOL_NAME => ToolEffect::Isolated,
        SHELL_TOOL_NAME
        | FILE_WRITE_TOOL_NAME
        | FILE_EDIT_TOOL_NAME
        | FILE_APPLY_PATCH_TOOL_NAME
        | MEMORY_TOOL_NAME => ToolEffect::Mutating,
        _ => ToolEffect::Unknown,
    }
}

fn start(id: &str, tool: &str) -> ToolStartEvent {
    ToolStartEvent {
        id: id.into(),
        effect: effect_of(tool),
        tool: tool.into(),
        summary: id.into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }
}

fn panel_with_tools(ids: &[(&str, &'static str)]) -> MessagesPanel {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for &(id, tool) in ids {
        panel.tool_start(start(id, tool));
    }
    panel
}

#[test]
fn source_at_maps_every_source_kind_and_tool_segment_to_display_message() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let ids = std::array::from_fn::<_, 5, _>(|_| caudra_storage::id::CaudraId::generate());
    let sources = [
        DisplaySource::User(ids[0]),
        DisplaySource::AssistantText(ids[1]),
        DisplaySource::Reasoning(ids[2]),
        DisplaySource::ToolCall {
            id: ids[3],
            result_id: Some(ids[4]),
        },
        DisplaySource::ToolResult(ids[4]),
    ];
    for (index, source) in sources.into_iter().enumerate() {
        let role = match source {
            DisplaySource::User(_) => DisplayRole::User,
            DisplaySource::Reasoning(_) => DisplayRole::Thinking,
            DisplaySource::ToolCall { .. } => DisplayRole::Tool(Box::new(ToolRole {
                id: "tool-row".into(),
                effect: ToolEffect::Unknown,
                status: ToolStatus::Success,
                name: "read".into(),
            })),
            DisplaySource::AssistantText(_) | DisplaySource::ToolResult(_) => {
                DisplayRole::Assistant
            }
        };
        let mut message = DisplayMessage::new(role, format!("message {index}"));
        message.source = Some(source);
        panel.push(message);
    }
    panel.rebuild_line_cache();
    panel.set_scroll_top(0);
    let area = Rect::new(0, 0, 80, 20);

    let mut row = 0;
    for (segment, source) in panel.cache.segments().iter().zip(sources) {
        row += segment.chrome(panel.viewport_width).content_start();
        assert_eq!(panel.source_at(row, area), Some(source));
        row += segment.content_height(panel.viewport_width)
            + segment.chrome(panel.viewport_width).bottom;
    }
}

fn done(id: &str) -> ToolDoneEvent {
    ToolDoneEvent {
        id: id.into(),
        tool: SHELL_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    }
}

fn shell_done(id: &str, filtered: bool) -> ToolDoneEvent {
    let output = ToolOutput::Shell(ShellOutput {
        model_text: (1..=8)
            .map(|line| format!("model_{line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        relative_workdir: ".".into(),
        timeout_ms: 120_000,
        duration_ms: 10,
        exit_code: Some(0),
        signal: None,
        timed_out: false,
        output_limit_exceeded: false,
        final_sequence: 8,
        stdout_utf8_bytes: 47,
        stderr_utf8_bytes: 0,
        stdout: (1..=8)
            .map(|line| format!("raw_{line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        stderr: String::new(),
        stdout_capture_truncated: false,
        stderr_capture_truncated: false,
        stdout_preview_truncated: false,
        stderr_preview_truncated: false,
        filter: filtered.then(|| ShellFilterInfo {
            rule: "cargo".into(),
            unfiltered_utf8_bytes: 200,
            filtered_utf8_bytes: 40,
        }),
    });
    ToolDoneEvent {
        id: id.into(),
        tool: "shell".into(),
        output,
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    }
}

fn finish_with_live_buf(
    panel: &mut MessagesPanel,
    id: &str,
    text: &str,
    is_error: bool,
) -> Arc<caudra_agent::SharedBuf> {
    let buf = Arc::new(caudra_agent::SharedBuf::new());
    buf.set_lines(vec![snap_line(text)]);
    panel.register_live_buf(id.into(), Arc::clone(&buf));
    let mut ev = start(id, SHELL_TOOL_NAME);
    ev.raw_input = Some(serde_json::json!({ "command": "true" }));
    panel.tool_start(ev);
    panel.tool_done(ToolDoneEvent {
        is_error,
        ..done(id)
    });
    buf
}

#[test_case(false, ToolStatus::Success ; "success_updates_start_to_success")]
#[test_case(true,  ToolStatus::Error   ; "error_updates_start_to_error")]
fn tool_done_updates_start_status(is_error: bool, expected: ToolStatus) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "bash"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("output".into()),
        is_error,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });

    assert_eq!(panel.messages.len(), 1);
    assert!(matches!(&panel.messages[0].role, DisplayRole::Tool(t) if t.status == expected));
    assert!(panel.messages[0].text.contains("output"));
}

#[test_case(
    FILE_WRITE_TOOL_NAME,
    ToolOutput::WriteCode { path: "src/main.rs".into(), byte_count: 42, lines: vec!["fn main() {}".into()] },
    Some("1 lines")
    ; "write_bytes"
)]
#[test_case(
    "grep",
    grep_output(2),
    Some("2 matches in 2 files")
    ; "grep_files"
)]
fn tool_done_sets_annotation(tool: &'static str, output: ToolOutput, expected: Option<&str>) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", tool));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: tool.into(),
        output,
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    assert_eq!(panel.messages[0].annotation.as_deref(), expected);
}

#[test_case("line\n".repeat(200).as_str(), Some("2m timeout · 200 lines") ; "merges_start_and_output_annotations")]
#[test_case("ok",                           Some("2m timeout · 1 lines") ; "merges_start_and_short_output")]
fn tool_done_annotation_merge(output: &str, expected: Option<&str>) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let mut event = start("t1", SHELL_TOOL_NAME);
    event.annotation = Some("2m timeout".into());
    panel.tool_start(event);
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: SHELL_TOOL_NAME.into(),
        output: ToolOutput::Plain(output.into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    assert_eq!(panel.messages[0].annotation.as_deref(), expected);
}

fn grep_output(n_files: usize) -> ToolOutput {
    ToolOutput::GrepResult {
        entries: (0..n_files)
            .map(|i| GrepFileEntry {
                path: format!("{i}.rs"),
                groups: vec![GrepMatchGroup::single(1, "")],
            })
            .collect(),
    }
}

#[test]
fn tool_done_grep_shows_matches() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", FILE_GREP_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: FILE_GREP_TOOL_NAME.into(),
        output: grep_output(2),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    let text = &panel.messages[0].text;
    assert!(!text.contains('\n'), "grep body should not be in msg.text");
    assert!(panel.messages[0].tool_output.is_some());
}

#[test]
fn tool_start_flushes_streaming_text() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_text.set_buffer("partial response");

    panel.tool_start(start("t1", "read"));

    assert!(panel.streaming_text.is_empty());
    assert_eq!(panel.messages[0].role, DisplayRole::Assistant);
    assert!(matches!(panel.messages[1].role, DisplayRole::Tool(_)));
}

#[test]
fn thinking_delta_separate_from_text() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.thinking_delta("reasoning");
    assert_eq!(panel.streaming_thinking, "reasoning");
    assert!(panel.streaming_text.is_empty());

    panel.text_delta("output");
    assert!(panel.streaming_thinking.is_empty());
    assert_eq!(panel.streaming_text, "output");
    assert_eq!(panel.messages[0].role, DisplayRole::Thinking);
    assert_eq!(panel.messages[0].text, "reasoning");
}

#[test]
fn scroll_up_pins_viewport_during_streaming() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_text.set_buffer(&"a\n".repeat(30));
    render(&mut panel, 80, 10);

    panel.scroll(1);
    panel.scroll(1);
    render(&mut panel, 80, 10);
    let pinned = panel.scroll_top;

    panel.text_delta("b\nb\nb\n");
    render(&mut panel, 80, 10);

    assert!(!panel.auto_scroll);
    assert_eq!(panel.scroll_top, pinned);
}

fn render_sel(
    panel: &mut MessagesPanel,
    width: u16,
    height: u16,
    has_selection: bool,
) -> ratatui::Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            panel.view(f, f.area(), has_selection);
        })
        .unwrap();
    terminal
}

fn render(panel: &mut MessagesPanel, width: u16, height: u16) -> ratatui::Terminal<TestBackend> {
    render_sel(panel, width, height, false)
}

fn rebuild(panel: &mut MessagesPanel) {
    render(panel, 80, 24);
}

#[test]
fn ctrl_d_to_bottom_re_enables_auto_scroll() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_text.set_buffer(&"a\n".repeat(30));
    render(&mut panel, 80, 10);
    assert!(panel.auto_scroll);

    let half = panel.half_page();
    panel.scroll(half);
    render(&mut panel, 80, 10);
    assert!(!panel.auto_scroll);

    panel.scroll(-half);
    render(&mut panel, 80, 10);
    assert!(panel.auto_scroll);
}

#[test]
fn unknown_tool_id_is_noop() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_output("ghost", "data");
    panel.tool_done(ToolDoneEvent {
        id: "orphan".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    assert!(panel.messages.is_empty());
}

#[test]
fn fail_in_progress_except_preserves_excluded_tool() {
    let mut panel = panel_with_tools(&[("agent", "task"), ("shell", "bash")]);
    let excluded = HashSet::from(["shell".to_string()]);

    panel.fail_in_progress_except("missing completion".into(), &excluded);

    assert_eq!(panel.in_progress_count(), 1);
    assert_eq!(msg_status(&panel, "agent"), ToolStatus::Error);
    assert_eq!(msg_status(&panel, "shell"), ToolStatus::InProgress);
    assert!(panel.messages[0].text.contains("missing completion"));
}

#[test]
fn in_progress_tracking() {
    let mut panel = panel_with_tools(&[("t1", "bash"), ("t2", "read")]);
    assert_eq!(panel.in_progress_count(), 2);

    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("ok".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    assert_eq!(panel.in_progress_count(), 1);

    panel.tool_done(ToolDoneEvent {
        id: "t2".into(),
        tool: "read".into(),
        output: ToolOutput::Plain("ok".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    assert_eq!(panel.in_progress_count(), 0);
}

fn has_scrollbar_thumb(terminal: &ratatui::Terminal<TestBackend>) -> bool {
    let buf = terminal.backend().buffer();
    (0..buf.area.height).any(|y| {
        buf.cell((buf.area.width - 1, y))
            .is_some_and(|c: &ratatui::buffer::Cell| c.symbol() == SCROLLBAR_THUMB)
    })
}

#[test_case(40, true  ; "rendered_when_content_overflows")]
#[test_case(1,  false ; "hidden_when_content_fits")]
fn scrollbar_visibility(line_count: usize, expected: bool) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel
        .streaming_text
        .set_buffer(&"line\n".repeat(line_count));
    let terminal = render(&mut panel, 80, 10);
    assert_eq!(has_scrollbar_thumb(&terminal), expected);
}

fn seg_text(panel: &MessagesPanel, tool_id: &str) -> String {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some(tool_id))
        .unwrap()
        .lines()
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect()
}

fn msg_status(panel: &MessagesPanel, tool_id: &str) -> ToolStatus {
    panel
        .messages
        .iter()
        .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        .map(|m| match &m.role {
            DisplayRole::Tool(t) => t.status,
            _ => unreachable!(),
        })
        .unwrap()
}

fn has_seg(panel: &MessagesPanel, tool_id: &str) -> bool {
    panel
        .cache
        .segments()
        .iter()
        .any(|s| s.tool_id.as_deref() == Some(tool_id))
}

#[test]
fn events_before_cache_built_render_correctly() {
    let mut panel = panel_with_tools(&[("t1", "bash"), ("t2", "bash")]);
    panel.tool_output("t1", "early output");
    panel.tool_done(ToolDoneEvent {
        id: "t2".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("result".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    rebuild(&mut panel);
    assert!(seg_text(&panel, "t1").contains("early output"));
    assert_eq!(msg_status(&panel, "t2"), ToolStatus::Success);
    assert!(seg_text(&panel, "t2").contains("result"));
}

fn bash_code_start(panel: &mut MessagesPanel, id: &str, code: &str) {
    panel.tool_start(ToolStartEvent {
        id: id.into(),
        effect: ToolEffect::Unknown,
        tool: SHELL_TOOL_NAME.into(),
        summary: code.into(),
        annotation: None,
        input: Some(ToolInput::Code {
            language: "bash".into(),
            code: code.into(),
        }),
        raw_input: None,
        output: None,
        render_header: None,
    });
}

#[test]
fn bash_live_output_with_code_input() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    bash_code_start(&mut panel, "t1", "echo hello");
    rebuild(&mut panel);

    panel.tool_output("t1", "streaming");
    assert!(seg_text(&panel, "t1").contains("streaming"));

    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: SHELL_TOOL_NAME.into(),
        output: ToolOutput::Plain("done".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    let text = seg_text(&panel, "t1");
    assert!(text.contains("echo hello") && text.contains("done"));
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Success);
}

#[test_case(true  ; "after_cache_built")]
#[test_case(false ; "before_cache_built")]
fn cancel_in_progress_marks_pending_as_error(cache_built: bool) {
    let mut panel = panel_with_tools(&[("t1", "bash"), ("t2", "read")]);
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("ok".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    if cache_built {
        rebuild(&mut panel);
    }

    panel.cancel_in_progress();

    assert_eq!(panel.in_progress_count(), 0);
    assert_eq!(panel.cadence(), Cadence::IDLE);
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Success);
    assert_eq!(msg_status(&panel, "t2"), ToolStatus::Error);
}

const THINKING_TEXT: &str = "a long chain of reasoning";
const HIGHLIGHTED_CODE: &str = "fn main() {}";
const HIGHLIGHT_DEADLINE: Duration = Duration::from_secs(10);

/// Only `view` advances a typewriter, and collapsed thinking does not draw its
/// body. Its lifecycle header still needs the lower spinner cadence for the
/// glyph and elapsed timer.
#[test_case(true  => Cadence::SMOOTH ; "expanded_thinking_reveals")]
#[test_case(false => Cadence::SPINNER ; "collapsed_thinking_spins")]
fn thinking_animates_only_while_it_is_on_screen(show_thinking: bool) -> Cadence {
    let config = UiConfig {
        show_thinking,
        ..UiConfig::default()
    };
    let mut panel = MessagesPanel::new(config, EventHandle::disconnected_for_test());

    panel.thinking_delta(THINKING_TEXT);

    assert!(
        panel.streaming_thinking.is_animating(),
        "the typewriter is mid-reveal, it just has nowhere to draw"
    );
    panel.cadence()
}

/// A waiting tool used to claim the whole screen was animating, which is what
/// pinned the loop at full frame rate. It draws one spinner glyph, so the
/// glyph rate is all it may ask for. Text arriving beside it earns the smooth
/// budget.
#[test_case(false => Cadence::SPINNER ; "waiting_tool_only_spins")]
#[test_case(true  => Cadence::SMOOTH  ; "streaming_text_beside_it_wins")]
fn cadence_while_a_tool_is_in_progress(text_streaming: bool) -> Cadence {
    let mut panel = panel_with_tools(&[("t1", SHELL_TOOL_NAME)]);
    if text_streaming {
        panel.text_delta("an answer arriving while the tool still runs");
    }
    assert_eq!(
        panel.in_progress_count(),
        1,
        "the spinner source has to be live or this proves nothing"
    );
    panel.cadence()
}

/// Without the `show_idle_splash` gate the splash keeps asking for smooth
/// frames for the rest of the session, long after the first message pushed it
/// off screen.
#[test]
fn splash_stops_driving_cadence_once_a_message_exists() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    assert_eq!(
        panel.cadence(),
        Cadence::SMOOTH,
        "the starfield drifts while the splash is the only thing drawn"
    );

    panel.tool_start(start("t1", SHELL_TOOL_NAME));
    panel.tool_done(done("t1"));

    assert_eq!(panel.cadence(), Cadence::IDLE, "the splash is gone");
}

/// `drain_highlights` moved out of `view`, so `tick` is the only thing feeding
/// the worker now. The wait is the worker's own round trip, not a sleep: the
/// loop ends the moment the result lands, and the deadline only turns a broken
/// drain into a failure instead of a hang.
#[test]
fn tick_drains_the_highlight_worker() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: ToolOutput::ReadCode {
            path: "file.rs".into(),
            start_line: 1,
            lines: vec![HIGHLIGHTED_CODE.into()],
            total_lines: 1,
            instructions: None,
        },
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    rebuild(&mut panel);

    let deadline = Instant::now() + HIGHLIGHT_DEADLINE;
    while panel.tick() == Dirty::NO {
        assert!(
            Instant::now() < deadline,
            "a highlighted tool stays unstyled until some unrelated repaint"
        );
        std::thread::yield_now();
    }

    assert!(
        seg_text(&panel, "t1").contains(HIGHLIGHTED_CODE),
        "the applied result replaces the highlight range in place"
    );
}

#[test]
fn new_tool_after_in_place_update() {
    let mut panel = panel_with_tools(&[("t1", "bash")]);
    rebuild(&mut panel);
    panel.tool_output("t1", "streaming data");

    panel.tool_start(start("t2", "read"));
    rebuild(&mut panel);

    assert!(seg_text(&panel, "t1").contains("streaming data"));
    assert!(has_seg(&panel, "t2"));
}

#[test]
fn tool_done_after_cancel_in_progress_does_not_underflow() {
    let mut panel = panel_with_tools(&[("t1", "bash"), ("t2", "read")]);
    panel.cancel_in_progress();
    assert_eq!(panel.in_progress_count(), 0);

    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "bash".into(),
        output: ToolOutput::Plain("late".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    assert_eq!(panel.in_progress_count(), 0);
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Success);
}

#[test]
fn selection_freezes_viewport_during_auto_scroll() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_text.set_buffer(&"a\n".repeat(30));
    render(&mut panel, 80, 10);
    assert!(panel.auto_scroll);
    let scroll_before = panel.scroll_top;
    assert!(scroll_before > 0);

    panel.streaming_text.set_buffer(&"a\n".repeat(35));
    render_sel(&mut panel, 80, 10, true);
    assert_eq!(panel.scroll_top, scroll_before);
    assert!(panel.auto_scroll);

    render_sel(&mut panel, 80, 10, false);
    assert!(panel.scroll_top > scroll_before);
    assert!(panel.auto_scroll);
}

fn seg_search(panel: &MessagesPanel, tool_id: &str) -> String {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some(tool_id))
        .unwrap()
        .search_text
        .clone()
}

#[test]
fn search_text_grep_result_includes_structured_output() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "grep"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "grep".into(),
        output: grep_output(2),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    rebuild(&mut panel);
    let text = seg_search(&panel, "t1");
    assert!(text.contains("0.rs:") && text.contains("1.rs:"));
}

#[test]
fn search_text_diff_output_includes_hunks() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "edit"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "edit".into(),
        output: ToolOutput::Diff {
            path: "src/main.rs".into(),
            before: "old\n".into(),
            after: "new\n".into(),
            summary: "1 edit".into(),
        },
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    rebuild(&mut panel);
    let text = seg_search(&panel, "t1");
    assert!(text.contains("- old") && text.contains("+ new"));
}

#[test]
fn search_text_bash_with_code_input() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    bash_code_start(&mut panel, "t1", "echo hello");
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: SHELL_TOOL_NAME.into(),
        output: ToolOutput::Plain("hello".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    rebuild(&mut panel);
    let text = seg_search(&panel, "t1");
    assert!(text.contains("echo hello") && text.contains("hello"));
}

#[test]
fn search_text_omits_author_labels() {
    let md = "# Heading\n\nSome **bold** text";
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, "hello".into()));
    panel.push(DisplayMessage::new(DisplayRole::Assistant, md.into()));
    panel.push(DisplayMessage::new(DisplayRole::Thinking, "hmm".into()));
    rebuild(&mut panel);
    let texts = panel.segment_search_texts();
    assert_eq!(texts[0], "hello");
    assert_eq!(texts[1], md);
    assert_eq!(texts[2], "thinking> hmm");
}

#[test]
fn a_compiled_review_renders_as_a_card_instead_of_tags() {
    const COMPILED: &str = "<review>\nAddress each note on my previous message.\n\n\
         <note>\n> - **Independent lap controls**\n> - **Race-distance guidance**\n\
         cool features\n</note>\n</review>";

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, COMPILED.into()));

    let terminal = render(&mut panel, 60, 12);
    let buffer = terminal.backend().buffer();
    let screen = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(screen.contains("Review · 1 note"));
    assert!(screen.contains("Independent lap controls"));
    assert!(screen.contains("cool features"));
    assert!(!screen.contains("<review>"));
    assert!(!screen.contains("<note>"));
}

#[test]
fn author_messages_render_as_a_user_card_and_flat_assistant_prose() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, "hello".into()));
    panel.push(DisplayMessage::new(DisplayRole::Assistant, "world".into()));

    let terminal = render(&mut panel, 40, 10);
    let buffer = terminal.backend().buffer();
    let rows = (0..buffer.area.height)
        .map(|y| {
            let text = (0..buffer.area.width)
                .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
                .collect::<String>();
            (y, text)
        })
        .collect::<Vec<_>>();
    let user_row = rows
        .iter()
        .find(|(_, text)| text.contains("hello"))
        .unwrap()
        .0;
    let assistant_row = rows
        .iter()
        .find(|(_, text)| text.contains("world"))
        .unwrap()
        .0;
    let user_bg = buffer.cell((3, user_row)).unwrap().style().bg;
    let assistant_bg = buffer.cell((3, assistant_row)).unwrap().style().bg;

    assert_ne!(user_bg, assistant_bg);
    assert_eq!(buffer.cell((0, user_row)).unwrap().symbol(), "┃");
    assert_eq!(buffer.cell((0, assistant_row)).unwrap().symbol(), " ");
    assert_eq!(
        buffer
            .cell((panel.viewport_width - 1, user_row))
            .unwrap()
            .style()
            .bg,
        user_bg
    );
    assert_eq!(rows[user_row.saturating_sub(1) as usize].1.trim(), "┃");
    assert!(rows.iter().all(|(_, text)| !text.contains("you>")));
    assert!(rows.iter().all(|(_, text)| !text.contains("caudra>")));
}

#[test]
fn streaming_assistant_uses_the_flat_assistant_inset() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_text.set_buffer("streaming");

    let terminal = render(&mut panel, 24, 8);
    let buffer = terminal.backend().buffer();
    let row = (0..buffer.area.height)
        .find(|&y| {
            (0..buffer.area.width)
                .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
                .collect::<String>()
                .contains("streaming")
        })
        .unwrap();
    assert_eq!(buffer.cell((0, row)).unwrap().symbol(), " ");
    assert_eq!(buffer.cell((1, row)).unwrap().symbol(), " ");
    assert_eq!(buffer.cell((2, row)).unwrap().symbol(), "s");
}

#[test]
fn selecting_only_user_card_padding_copies_nothing() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, "hello".into()));
    render(&mut panel, 80, 10);
    let area = Rect::new(0, 0, 80, 10);
    let selection = make_sel(area, (0, 0), (0, 79));

    assert!(panel.extract_selection_text(&selection, area).is_empty());
}

#[test_case(&["short", &"x".repeat(200)], 80, 4 ; "long_line_wraps")]
#[test_case(&["", "a", ""],                 40, 3 ; "empty_lines_count_as_one")]
#[test_case(&[&"a".repeat(80)],              80, 1 ; "exactly_width_no_wrap")]
#[test_case(&[&"a".repeat(81)],              80, 2 ; "one_over_width_wraps")]
#[test_case(&["hello", "world"],              0, 2 ; "zero_width_returns_line_count")]
#[test_case(&["aaaa bbbb cccc dddd"],         10, 2 ; "word_boundary_wrap")]
#[test_case(&["aaaaaa bbbbbbbbb"],            10, 2 ; "word_straddles_boundary")]
fn wrapped_line_count_cases(input: &[&str], width: u16, expected: u16) {
    let lines: Vec<Line<'static>> = input
        .iter()
        .map(|s| Line::from(Span::raw(s.to_string())))
        .collect();
    assert_eq!(wrapped_line_count(&lines, width), expected);
}

#[test]
fn update_tool_model_sets_annotation() {
    let mut panel = panel_with_tools(&[("t1", "task"), ("t2", "bash")]);
    rebuild(&mut panel);

    panel.update_tool_model("t1", "anthropic/claude-sonnet-4-20250514");

    let msg = &panel.messages[0];
    assert_eq!(
        msg.annotation.as_deref(),
        Some("anthropic/claude-sonnet-4-20250514")
    );
}

#[test]
fn set_tool_turn_usage_updates_exact_tool_and_keeps_annotation() {
    const MODEL: &str = "anthropic/claude-sonnet-4-20250514";
    const USAGE: &str = "1.2k↑ 345↓ $0.010";

    let mut panel = panel_with_tools(&[("t1", "task"), ("t2", "task")]);
    panel.update_tool_model("t1", MODEL);

    panel.set_tool_turn_usage("t1", USAGE.into());

    assert_eq!(panel.tool_turn_usage("t1"), Some(USAGE));
    assert_eq!(panel.tool_turn_usage("t2"), None);
    assert_eq!(panel.messages[0].annotation.as_deref(), Some(MODEL));
}

#[test]
fn win_view_clamps_a_restored_offset_past_the_end() {
    const LINES: u16 = 15;
    const HEIGHT: u16 = 10;

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel
        .streaming_text
        .set_buffer(&"a\n".repeat(LINES as usize));
    render(&mut panel, 80, HEIGHT);

    panel.restore_scroll(u16::MAX, true);

    let view = panel.win_view();
    assert_eq!(view.scroll_top, panel.max_scroll());
    assert_eq!(view.line_count, LINES);
    assert_eq!(view.height, HEIGHT);
    assert!(view.auto_scroll);
}

#[test]
fn scroll_clamps_to_max_scroll() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_text.set_buffer(&"a\n".repeat(15));
    render(&mut panel, 80, 10);
    let max = panel.max_scroll();

    panel.scroll(-3);
    assert_eq!(panel.scroll_top, max);
}

#[test_case("bash", 1, 1 ; "known_tool_creates_message")]
#[test_case("nonexistent_tool", 1, 1 ; "unknown_tool_accepted")]
fn tool_pending(tool: &str, expected_msgs: usize, expected_in_progress: usize) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending("t1".into(), tool);
    assert_eq!(panel.messages.len(), expected_msgs);
    assert_eq!(panel.in_progress_count(), expected_in_progress);
}

#[test]
fn tool_start_upgrades_pending_in_place() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending("t1".into(), "bash");
    assert_eq!(panel.messages.len(), 1);
    assert_eq!(panel.in_progress_count(), 1);

    let mut event = start("t1", SHELL_TOOL_NAME);
    event.annotation = Some("note".into());
    panel.tool_start(event);

    assert_eq!(panel.messages.len(), 1);
    assert_eq!(panel.in_progress_count(), 1);
    assert_eq!(panel.messages[0].text, "t1");
    assert_eq!(panel.messages[0].annotation.as_deref(), Some("note"));
}

#[test]
fn stream_reset_clears_streaming_and_fails_tools() {
    let mut panel = panel_with_tools(&[("t1", "bash")]);
    panel.streaming_thinking.set_buffer("partial thinking");
    panel.streaming_text.set_buffer("partial text");
    rebuild(&mut panel);

    panel.stream_reset();

    assert!(panel.streaming_thinking.is_empty());
    assert!(panel.streaming_text.is_empty());
    assert_eq!(panel.in_progress_count(), 0);
    assert_eq!(msg_status(&panel, "t1"), ToolStatus::Error);
}

const MESSAGE_START_COL: u16 = 3;

fn make_sel(area: Rect, anchor: (u32, u16), cursor: (u32, u16)) -> Selection {
    let mut sel = Selection::start(
        area.y + anchor.0 as u16,
        anchor.1,
        area,
        SelectionZone::Messages,
        0,
    );
    sel.update(area.y + cursor.0 as u16, cursor.1, 0);
    sel
}

fn panel_with_msgs(texts: &[&str], width: u16, height: u16) -> MessagesPanel {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for &text in texts {
        panel.push(DisplayMessage::new(DisplayRole::Assistant, text.into()));
    }
    render(&mut panel, width, height);
    panel
}

#[test]
fn copying_a_review_card_yields_the_source_block() {
    const QUOTE: &str = "> - **Independent lap controls**";
    const COMPILED: &str = "<review>\nAddress each note on my previous message.\n\n\
         <note>\n> - **Independent lap controls**\ncool features\n</note>\n</review>";

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, COMPILED.into()));
    let area = Rect::new(0, 0, 80, 24);
    render(&mut panel, area.width, area.height);

    let rows = panel.cache.segments()[0].drawn_height(panel.viewport_width);
    let sel = make_sel(area, (0, 0), (rows as u32, area.width - 1));
    let copied = panel.extract_selection_text(&sel, area);

    assert!(
        copied.contains(QUOTE),
        "quote source missing from {copied:?}"
    );
    assert!(copied.contains("cool features"));
    assert!(!copied.contains('▏'));
}

#[test]
fn extract_partial_column_selection() {
    let panel = panel_with_msgs(&["Hello world"], 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    let world_start = MESSAGE_START_COL + "Hello ".len() as u16;
    let sel = make_sel(area, (0, world_start), (0, world_start + 4));
    let text = panel.extract_selection_text(&sel, area);
    assert_eq!(text, "world");
}

const MARKDOWN_DOC: &str = "# Title with **bold**\n\nA paragraph with `code` and *italic*.\n\n- first item\n- second item\n\n```rust\nfn main() {}\n```\n\n| Name | Value |\n| --- | --- |\n| foo | 42 |\n\nTrailing text.";

/// Selecting a whole message copies the markdown that produced it, not the
/// glyphs on screen. Without provenance this returns bullets, box-drawing
/// borders and emphasis-stripped text.
#[test_case(MARKDOWN_DOC; "mixed_document")]
#[test_case("# Heading"; "heading_hashes")]
#[test_case("**bold** and _italic_"; "emphasis_delimiters")]
#[test_case("- alpha\n- beta"; "bullets")]
#[test_case("```rust\nfn x() {}\n```"; "code_fences")]
#[test_case("| a | b |\n| --- | --- |\n| 1 | 2 |"; "table_pipes")]
#[test_case("Use `cargo test` now."; "inline_code_backticks")]
#[test_case("Energy $E = mc^2$ today."; "inline_math")]
#[test_case("$$\nE = mc^2\n$$"; "display_math")]
#[test_case("It costs $5 and $10 total."; "currency_is_not_math")]
fn select_all_copies_source_markdown(doc: &str) {
    const WIDTH: u16 = 80;
    let panel = panel_with_msgs(&[doc], WIDTH, 60);
    let area = Rect::new(0, 0, WIDTH, 60);
    let total = panel.segment_heights().iter().sum::<u16>();
    let sel = make_sel(area, (0, 0), (total as u32, WIDTH - 1));
    assert_eq!(panel.extract_selection_text(&sel, area), doc);
}

#[test]
fn select_all_copies_source_at_narrow_widths() {
    for width in [24u16, 40, 80] {
        let panel = panel_with_msgs(&[MARKDOWN_DOC], width, 80);
        let area = Rect::new(0, 0, width, 80);
        let total = panel.segment_heights().iter().sum::<u16>();
        let sel = make_sel(area, (0, 0), (total as u32, width - 1));
        assert_eq!(
            panel.extract_selection_text(&sel, area),
            MARKDOWN_DOC,
            "width {width}"
        );
    }
}

/// Maths renders as Unicode the source never contained, so the rendered
/// glyphs must not be what lands on the clipboard.
#[test]
fn copied_math_is_latex_not_rendered_glyphs() {
    const DOC: &str = "Energy $E = mc^2$ today.";
    let width = 80;
    let panel = panel_with_msgs(&[DOC], width, 24);
    let area = Rect::new(0, 0, width, 24);
    let sel = make_sel(area, (0, 0), (0, width - 1));
    let copied = panel.extract_selection_text(&sel, area);
    assert_eq!(copied, DOC);
    assert!(
        !copied.contains('\u{b2}'),
        "rendered glyph leaked: {copied:?}"
    );
}

/// The role prefix is UI chrome, so it never reaches the clipboard even when
/// the selection starts on top of it.
#[test]
fn select_all_omits_the_role_prefix() {
    let panel = panel_with_msgs(&["plain text"], 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    let sel = make_sel(area, (0, 0), (0, 79));
    assert_eq!(panel.extract_selection_text(&sel, area), "plain text");
}

#[test]
fn expanded_reasoning_selection_uses_body_markdown_provenance() {
    const BODY: &str = "# Review\n\nUse **care**.";
    const WIDTH: u16 = 80;
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: true,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.push(DisplayMessage::new(
        DisplayRole::Thinking,
        format!("**Inspecting**\n\n{BODY}"),
    ));
    render(&mut panel, WIDTH, 20);
    let area = Rect::new(0, 0, WIDTH, 20);
    let total = panel.segment_heights().iter().sum::<u16>();
    let sel = make_sel(area, (0, 0), (u32::from(total), WIDTH - 1));

    assert_eq!(panel.extract_selection_text(&sel, area), BODY);
}

/// Partial selections stay literal for verbatim spans, so dragging over part
/// of a word still yields that part rather than the whole construct.
#[test]
fn partial_selection_inside_bold_keeps_sub_word_precision() {
    let panel = panel_with_msgs(&["**bolded** tail"], 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    let start = MESSAGE_START_COL;
    let sel = make_sel(area, (0, start), (0, start + 2));
    assert_eq!(panel.extract_selection_text(&sel, area), "bol");
}

#[test]
fn extract_skips_out_of_range_segments() {
    let panel = panel_with_msgs(&["seg0", "seg1", "seg2"], 80, 24);
    let heights = panel.segment_heights();
    let total: u16 = heights.iter().sum();
    let mid = total / 2;
    let area = Rect::new(0, 0, 80, 24);
    let sel = make_sel(area, (mid as u32, 0), (mid as u32, 79));
    let text = panel.extract_selection_text(&sel, area);
    assert!(text.contains("seg1"));
    assert!(!text.contains("seg0"));
    assert!(!text.contains("seg2"));
}

#[test]
fn extract_off_screen_rows_via_temp_buffer() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let text = (0..20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    panel.push(DisplayMessage::new(DisplayRole::Assistant, text));
    render(&mut panel, 80, 5);

    let total: u16 = panel.segment_heights().iter().sum();
    assert!(total > 5, "content must exceed viewport");
    let sel_area = Rect::new(0, 0, 80, total);
    let sel = make_sel(sel_area, (1, 0), ((total - 1) as u32, 79));

    let extracted = panel.extract_selection_text(&sel, sel_area);
    assert!(!extracted.contains("line 0"), "first line excluded");
    assert!(extracted.contains("line 1") && extracted.contains("line 19"));
}

#[test]
fn extract_mixed_fully_enclosed_and_partial() {
    let panel = panel_with_msgs(&["full segment", "partial here"], 80, 24);
    let heights = panel.segment_heights().to_vec();
    let area = Rect::new(0, 0, 80, 24);
    let seg1_start = heights[0] + heights[1];
    let sel = make_sel(area, (0, 0), (seg1_start as u32, MESSAGE_START_COL + 6));
    let text = panel.extract_selection_text(&sel, area);
    assert!(text.contains("full segment"));
    assert!(text.contains("partial"));
}

#[test_case(&["line-0\nline-1\nline-2\nline-3"], "line-0", "line-3" ; "single_segment")]
#[test_case(&["seg-A-text", "seg-B-text"],      "seg-A-text", "seg-B-text" ; "across_segments")]
fn extract_partial_col_symmetric(msgs: &[&str], expect_start: &str, expect_end: &str) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for &text in msgs {
        panel.push(DisplayMessage::new(DisplayRole::Assistant, text.into()));
    }
    render(&mut panel, 80, 24);
    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 80, 24);
    let down = make_sel(area, (0, MESSAGE_START_COL), ((total - 1) as u32, 79));
    let up = make_sel(area, ((total - 1) as u32, 79), (0, MESSAGE_START_COL));
    let text_down = panel.extract_selection_text(&down, area);
    let text_up = panel.extract_selection_text(&up, area);
    assert!(text_down.contains(expect_start));
    assert!(text_down.contains(expect_end));
    assert_eq!(text_down, text_up, "direction should not affect result");
}

#[test_case("```\n{L}\n```", (0, 1)  ; "wrapped_code_block")]
#[test_case("short\n{L}",   (0, 0)  ; "wrapped_long_line")]
fn extract_wrapped_no_soft_breaks(template: &str, anchor: (u32, u16)) {
    let long = "x".repeat(200);
    let msg = template.replace("{L}", &long);
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::Assistant, msg));
    render(&mut panel, 40, 30);
    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 40, 30);
    let sel = make_sel(area, anchor, ((total - 1) as u32, 39));
    let text = panel.extract_selection_text(&sel, area);
    assert!(
        text.contains(&long),
        "wrapped line must be copied without newlines: {text:?}"
    );
}

#[test]
fn extract_partial_last_line_truncated() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "first\nABCDEFGHIJKLMNOP".into(),
    ));
    render(&mut panel, 80, 24);
    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, 80, 24);
    let last_row = (total - 1) as u32;
    let sel = make_sel(area, (0, 0), (last_row, MESSAGE_START_COL + 3));
    let text = panel.extract_selection_text(&sel, area);
    assert_eq!(text.lines().last().unwrap(), "ABCD");
}

fn panel_with_long_tool(line_count: usize) -> MessagesPanel {
    let body = (0..line_count)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(ToolStartEvent {
        id: "t1".into(),
        effect: ToolEffect::Unknown,
        tool: SHELL_TOOL_NAME.into(),
        summary: "cmd".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    });
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: SHELL_TOOL_NAME.into(),
        output: ToolOutput::Plain(body.into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    render(&mut panel, 80, 24);
    panel
}

#[test]
fn toggle_expand_collapse_truncated_tool() {
    let mut panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 80, 24);
    assert!(seg_text(&panel, "t1").contains("click to expand"));

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(!seg_text(&panel, "t1").contains("click to expand"));

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(seg_text(&panel, "t1").contains("click to expand"));
}

#[test]
fn completed_shell_output_toggles_between_filtered_and_raw_views() {
    let mut panel = panel_with_tools(&[("t1", "shell")]);
    panel.tool_done(shell_done("t1", true));
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    let segment = panel
        .cache
        .segments()
        .iter()
        .find(|segment| segment.tool_id.as_deref() == Some("t1"))
        .unwrap();
    let toggle_line = segment.shell_toggle_line.unwrap() as u16;
    let toggle_row = segment.chrome(80).content_start() + toggle_line;

    assert!(seg_text(&panel, "t1").contains("model_8"));
    assert!(!seg_text(&panel, "t1").contains("raw_8"));
    assert!(panel.toggle_expansion_at(toggle_row, area));
    render(&mut panel, 80, 24);
    let text = seg_text(&panel, "t1");

    assert!(text.contains("raw_8"));
    assert!(!text.contains("model_8"));
    assert!(text.contains("raw output · click for filtered"));
}

fn long_done(id: &str, lines: usize) -> ToolDoneEvent {
    let body = (0..lines)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    ToolDoneEvent {
        output: ToolOutput::Plain(body.into()),
        ..done(id)
    }
}

fn shell_toggle_row(panel: &MessagesPanel) -> u16 {
    let segment = panel
        .cache
        .segments()
        .iter()
        .find(|segment| segment.tool_id.as_deref() == Some("t1"))
        .unwrap();
    segment.chrome(80).content_start() + segment.shell_toggle_line.unwrap() as u16
}

/// The raw/filtered switch is a choice about the body rather than about how
/// much of the card shows, so the reset that a mode change performs on every
/// disclosure must leave it alone.
#[test_case(true; "raw survives")]
#[test_case(false; "filtered survives")]
fn changing_mode_keeps_a_shell_card_on_its_raw_or_filtered_view(raw: bool) {
    let mut panel = compact_panel(&[("t1", SHELL_TOOL_NAME)]);
    panel.tool_done(shell_done("t1", true));
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    if raw {
        assert!(panel.toggle_expansion_at(shell_toggle_row(&panel), area));
        render(&mut panel, 80, 24);
    }
    let chosen = seg_text(&panel, "t1").contains("raw_8");
    assert_eq!(chosen, raw, "the test must set up the view it checks");

    panel.set_view(ViewMode::Expanded);
    render(&mut panel, 80, 24);

    assert_eq!(
        seg_text(&panel, "t1").contains("raw_8"),
        raw,
        "{SHELL_VIEW_STICKY_MSG}"
    );
}

#[test]
fn hovering_the_raw_switch_reverses_it_instead_of_the_card_header() {
    let mut panel = panel_with_tools(&[("t1", "shell")]);
    panel.tool_done(shell_done("t1", true));
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    let row = shell_toggle_row(&panel);

    panel.update_hover(row, area.x, area, false);

    assert_eq!(
        panel.hover_feedback_for_segment(
            panel
                .cache
                .segments()
                .iter()
                .find(|s| s.tool_id.as_deref() == Some("t1"))
                .unwrap()
        ),
        Some(HoverFeedback::ShellToggle),
        "{SHELL_HOVER_MSG}"
    );
}

#[test]
fn shell_live_output_uses_the_larger_running_budget_and_survives_completion() {
    let mut panel = panel_with_tools(&[("t1", "shell")]);
    let live = (0..20)
        .map(|line| format!("entry_{line}"))
        .collect::<Vec<_>>()
        .join("\n");

    panel.tool_output("t1", &live);
    assert!(panel.messages[0].text.contains("entry_10"));
    assert!(!panel.messages[0].text.contains("entry_7\n"));

    panel.tool_done(shell_done("t1", false));
    assert_eq!(
        panel.messages[0].live_output.as_deref(),
        Some(live.as_str())
    );
}

#[test]
fn native_tool_hover_reverses_only_the_expand_affordance_without_mutating_cache() {
    let mut panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 80, 24);
    let source = panel.messages[0].text.clone();
    let cached = panel.cache.find_by_tool_id("t1").and_then(|index| {
        panel
            .cache
            .get(index)
            .map(|segment| segment.lines().to_vec())
    });

    panel.update_hover(area.y, area.right(), area, false);
    assert!(panel.hover.is_none(), "outside columns are not hoverable");
    panel.update_hover(area.y, area.x, area, false);
    let terminal = render(&mut panel, area.width, area.height);

    assert!(
        style_of(&terminal, EXPAND_AFFORDANCE)
            .add_modifier
            .contains(Modifier::REVERSED)
    );
    assert!(
        !style_of(&terminal, "line 0")
            .add_modifier
            .contains(Modifier::REVERSED),
        "tool output must not be reversed by hover"
    );
    assert_eq!(
        panel
            .cache
            .find_by_tool_id("t1")
            .and_then(|index| panel.cache.get(index))
            .map(|segment| segment.lines()),
        cached.as_deref(),
        "paint-time hover must not rewrite cached lines"
    );
    assert_eq!(panel.messages[0].text, source);
}

#[test]
fn expanded_native_tool_hover_accents_header_and_rail_not_body() {
    const HEIGHT: u16 = 240;

    let mut panel = panel_with_long_tool(200);
    panel.set_view(ViewMode::Expanded);
    let area = Rect::new(0, 0, 80, HEIGHT);
    render(&mut panel, area.width, area.height);
    assert!(panel.toggle_expansion("t1"));
    render(&mut panel, area.width, area.height);

    panel.update_hover(area.y, area.x, area, false);
    let terminal = render(&mut panel, area.width, area.height);
    let buffer = terminal.backend().buffer();
    let rail = buffer.cell((area.x, area.y)).unwrap().style();
    let header = style_of(&terminal, "shell>");

    assert_eq!(header.fg, rail.fg, "header and rail share the hover accent");
    assert!(
        !style_of(&terminal, "line 0")
            .add_modifier
            .contains(Modifier::REVERSED),
        "expanded code/output body must not be reversed"
    );
}

#[test]
fn snapshot_tool_hovers_only_when_caller_confirms_a_known_task_card() {
    let mut panel = bash_tool_with_snapshot("t1");
    let area = Rect::new(0, 0, 80, 24);
    render(&mut panel, area.width, area.height);

    panel.update_hover(area.y, area.x, area, false);
    assert!(
        panel.hover.is_none(),
        "ordinary Lua snapshot rows are excluded"
    );

    panel.update_hover(area.y, area.x, area, true);
    assert!(matches!(
        panel.hover,
        Some(HoverTarget::Tool {
            feedback: HoverFeedback::Chrome,
            ..
        })
    ));
    let terminal = render(&mut panel, area.width, area.height);
    assert!(
        !style_of(&terminal, "rendered")
            .add_modifier
            .contains(Modifier::REVERSED),
        "task hover must leave snapshot body text alone"
    );
}

#[test]
fn ordinary_message_rows_never_become_transcript_hover_controls() {
    let mut panel = panel_with_msgs(&["long-click context"], 80, 24);
    let area = Rect::new(0, 0, 80, 24);

    panel.update_hover(area.y, area.x, area, false);

    assert!(panel.hover.is_none());
}

#[test]
fn message_link_hit_testing_accounts_for_segment_chrome() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "[docs](https://example.com)".into(),
    ));
    panel.viewport_width = 80;
    panel.rebuild_line_cache();
    panel.set_scroll_top(0);
    let area = Rect::new(5, 7, 80, 5);
    let segment = panel.cache.get(0).expect("assistant segment");
    let row = area.y + segment.chrome(80).content_start();
    let column = area.x + segment.chrome(80).left;

    panel.update_hover(row, column, area, false);
    assert_eq!(panel.hovered_link(), Some("https://example.com"));
    panel.update_hover(row, column + 4, area, false);
    assert_eq!(panel.hovered_link(), None);
}

#[test]
fn extract_selection_copies_visible_content_only() {
    let panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 80, 24);
    let total: u16 = panel.segment_heights().iter().sum();
    let sel = make_sel(area, (0, 0), ((total - 1) as u32, 79));
    let text = panel.extract_selection_text(&sel, area);
    assert!(
        !text.contains("line 50"),
        "truncated line should not be copied"
    );
}

#[test]
fn toggle_returns_false_for_non_expandable() {
    let mut panel = panel_with_long_tool(3);
    let area = Rect::new(0, 0, 80, 24);
    assert!(!panel.toggle_expansion_at(area.y, area));
}

fn panel_with_grep_tool(match_count: usize) -> MessagesPanel {
    let entries = vec![GrepFileEntry {
        path: "src/main.rs".into(),
        groups: (1..=match_count)
            .map(|i| GrepMatchGroup::single(i, format!("match_{i}")))
            .collect(),
    }];
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(ToolStartEvent {
        id: "t1".into(),
        effect: ToolEffect::Unknown,
        tool: FILE_GREP_TOOL_NAME.into(),
        summary: "grep pattern".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    });
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: FILE_GREP_TOOL_NAME.into(),
        output: ToolOutput::GrepResult { entries },
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    render(&mut panel, 80, 24);
    panel
}

#[test]
fn toggle_expand_collapse_grep_tool() {
    let mut panel = panel_with_grep_tool(8);
    panel.set_view(ViewMode::Expanded);
    let area = Rect::new(0, 0, 80, 24);
    render(&mut panel, 80, 24);
    assert!(seg_text(&panel, "t1").contains("click to expand"));

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(!seg_text(&panel, "t1").contains("click to expand"));

    assert!(panel.toggle_expansion_at(area.y, area));
    render(&mut panel, 80, 24);
    assert!(seg_text(&panel, "t1").contains("click to expand"));
}

fn buffer_text(terminal: &ratatui::Terminal<TestBackend>) -> String {
    let buf = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if let Some(cell) = buf.cell((x, y)) {
                text.push_str(cell.symbol());
            }
        }
        text.push('\n');
    }
    text
}

#[test]
fn streaming_with_cached_segments_shows_end_on_auto_scroll() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        "a\n".repeat(20).trim().into(),
    ));
    panel.streaming_text.set_buffer(
        &(0..50)
            .map(|i| format!("stream_{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let terminal = render(&mut panel, 80, 10);
    assert!(panel.auto_scroll);

    let screen = buffer_text(&terminal);
    assert!(screen.contains("stream_49"), "should show end");
    assert!(!screen.contains("stream_0 "), "should not show beginning");
}

#[test]
fn search_text_includes_truncated_bash_output() {
    let full_output = (0..100)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    bash_code_start(&mut panel, "t1", "echo lines");
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: SHELL_TOOL_NAME.into(),
        output: ToolOutput::Plain(full_output.clone().into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    rebuild(&mut panel);
    assert!(seg_search(&panel, "t1").contains(&full_output));
}

fn instruction_blocks() -> Vec<InstructionBlock> {
    vec![InstructionBlock {
        path: "agents.md".into(),
        content: "follow style guide".into(),
    }]
}

fn read_code_with_instructions(blocks: Vec<InstructionBlock>) -> ToolOutput {
    ToolOutput::ReadCode {
        path: "file.rs".into(),
        start_line: 1,
        lines: vec!["fn main() {}".into()],
        total_lines: 1,
        instructions: Some(blocks),
    }
}

fn segment_has_top_margin(panel: &MessagesPanel, tool_id: &str) -> bool {
    let idx = panel.cache.find_by_tool_id(tool_id).unwrap();
    panel
        .cache
        .get(idx)
        .unwrap()
        .chrome(panel.viewport_width)
        .margin_top
        > 0
}

#[test]
fn instruction_segment_has_margin_before_it() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Expanded);
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: read_code_with_instructions(instruction_blocks()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    rebuild(&mut panel);

    let inst_id = segment::instruction_id("t1");
    assert!(segment_has_top_margin(&panel, &inst_id));
}

fn seg_line_count(panel: &MessagesPanel, tool_id: &str) -> usize {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some(tool_id))
        .unwrap()
        .lines()
        .len()
}

#[test]
fn toggle_instruction_segment_expands_and_collapses() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let blocks = vec![InstructionBlock {
        path: "agents.md".into(),
        content: "x\n".repeat(100),
    }];
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: read_code_with_instructions(blocks),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    rebuild(&mut panel);

    let inst_id = segment::instruction_id("t1");
    let collapsed = seg_line_count(&panel, &inst_id);

    panel.toggle_expansion(&inst_id);
    assert!(seg_line_count(&panel, &inst_id) > collapsed);

    panel.toggle_expansion(&inst_id);
    assert_eq!(seg_line_count(&panel, &inst_id), collapsed);
}

#[test]
fn handle_click_returns_nothing_when_no_segment_at_row() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(!panel.handle_click(23, area));
}

#[test]
fn handle_click_on_done_tool_records_click_row() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", SHELL_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: SHELL_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("rendered")])),
        None,
    );
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(panel.lua_clicks.get("t1").map(Vec::len), Some(1));
}

#[test]
fn handle_click_on_running_tool_forwards_live_without_recording() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", SHELL_TOOL_NAME));
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("streaming")])),
        None,
    );
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert!(panel.lua_clicks.is_empty());
}

#[test]
fn handle_click_returns_toggled_for_truncated_tool_without_snapshot() {
    let mut panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
}

#[test]
fn handle_click_non_tool_segment_returns_nothing() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        "user message".into(),
    ));
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(!panel.handle_click(area.y, area));
}

#[test]
fn tool_done_removes_live_buf_and_snapshots_dirty() {
    let buf = Arc::new(caudra_agent::SharedBuf::new());
    buf.set_lines(vec![snap_line("dirty content")]);

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.register_live_buf("t1".into(), Arc::clone(&buf));
    panel.tool_start(start("t1", SHELL_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: SHELL_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "dirty content"
    );
}

/// The handler's buf must supersede the `start` preview: the UI keeps only
/// the last registered buf per tool_use_id.
#[test]
fn second_register_live_buf_replaces_first() {
    let preview = Arc::new(caudra_agent::SharedBuf::new());
    preview.set_lines(vec![snap_line("preview")]);
    let handler = Arc::new(caudra_agent::SharedBuf::new());
    handler.set_lines(vec![snap_line("handler")]);

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", SHELL_TOOL_NAME));
    panel.register_live_buf("t1".into(), Arc::clone(&preview));
    panel.register_live_buf("t1".into(), Arc::clone(&handler));
    let _ = panel.poll_live_bufs();

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "handler"
    );
}

/// Every finished-tool click on a watched buf carries the full recorded
/// click list as a restore fallback: the runtime serves it warm when it
/// can and restores otherwise, so the UI never guesses runtime state.
#[test_case(false ; "success")]
#[test_case(true ; "error_finish")]
fn handle_click_on_watched_tool_sends_click_with_fallback(is_error: bool) {
    let (eh, probe) = caudra_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default(), eh);
    panel.set_restore_channel(Some(EventSender::new(tx, 0)));
    finish_with_live_buf(&mut panel, "t1", "body", is_error);
    assert!(panel.watching("t1"));

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    let recorded = panel.lua_clicks["t1"].clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(probe.try_recv(), Some(("click_fallback", recorded)));
    assert_eq!(probe.try_recv(), None);
}

#[test]
fn tool_done_moves_live_buf_to_watched_polled_but_not_animating() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let buf = finish_with_live_buf(&mut panel, "t1", "before", false);
    assert!(panel.watching("t1"));
    assert_eq!(
        panel.cadence(),
        Cadence::IDLE,
        "a finished tool must not leave a spinner running"
    );

    buf.set_lines(vec![snap_line("after")]);
    assert_eq!(panel.poll_live_bufs(), Dirty::YES, "{OWED}");
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "after"
    );
}

#[test]
fn watched_fifo_evicts_oldest_which_stops_polling_and_restores_with_recorded_clicks() {
    let (eh, probe) = caudra_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default(), eh);
    panel.set_restore_channel(Some(EventSender::new(tx, 0)));
    let buf = finish_with_live_buf(&mut panel, "t0", "before", false);

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(panel.lua_clicks.get("t0").map(Vec::len), Some(1));
    assert_eq!(
        probe.try_recv(),
        Some(("click_fallback", panel.lua_clicks["t0"].clone()))
    );

    for i in 1..=WARM_TOOL_CAP {
        finish_with_live_buf(&mut panel, &format!("t{i}"), "body", false);
    }
    assert_eq!(panel.watched_bufs.len(), WARM_TOOL_CAP);
    assert!(!panel.watching("t0"));

    buf.set_lines(vec![snap_line("after-eviction")]);
    assert_eq!(
        panel.poll_live_bufs(),
        Dirty::NO,
        "evicted buf must no longer be polled"
    );
    let msg = panel.find_tool_msg_mut("t0").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "before",
        "evicted buf must no longer be polled"
    );

    render(&mut panel, 80, 24);
    panel.scroll_to_top();
    assert!(panel.handle_click(area.y, area));
    let recorded = panel.lua_clicks["t0"].clone();
    assert_eq!(recorded.len(), 2);
    assert_eq!(probe.try_recv(), Some(("restore", recorded)));
    assert_eq!(probe.try_recv(), None);
}

#[test]
fn tool_done_without_live_buf_is_not_watched_and_click_restores() {
    let (eh, probe) = caudra_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default(), eh);
    panel.set_restore_channel(Some(EventSender::new(tx, 0)));
    let mut ev = start("t1", SHELL_TOOL_NAME);
    ev.raw_input = Some(serde_json::json!({ "command": "true" }));
    panel.tool_start(ev);
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("body")])),
        None,
    );
    panel.tool_done(done("t1"));
    assert!(!panel.watching("t1"));

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(
        probe.try_recv(),
        Some(("restore", panel.lua_clicks["t1"].clone()))
    );
    assert_eq!(probe.try_recv(), None);
}

/// The stale-run_id filter drops ToolDone events after a cancel, so the
/// cancel path itself must retire live bufs: no spinner left running, and
/// the tool stays clickable through the warm path.
#[test]
fn cancel_in_progress_retires_live_buf_to_watched() {
    let (eh, probe) = caudra_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default(), eh);
    panel.set_restore_channel(Some(EventSender::new(tx, 0)));
    let buf = Arc::new(caudra_agent::SharedBuf::new());
    buf.set_lines(vec![snap_line("body")]);
    let mut ev = start("t1", SHELL_TOOL_NAME);
    ev.raw_input = Some(serde_json::json!({ "command": "true" }));
    panel.tool_start(ev);
    panel.register_live_buf("t1".into(), Arc::clone(&buf));

    panel.cancel_in_progress();
    assert_eq!(
        panel.cadence(),
        Cadence::IDLE,
        "cancel must not leave a tool marked in progress"
    );
    assert!(panel.watching("t1"));

    buf.set_lines(vec![snap_line("after-cancel")]);
    // The tool hands that same repaint to the host as its reply body, and the
    // stale-run_id filter drops it. Taking a body must not cost the screen the
    // last thing a cancelled tool painted.
    let _dropped_reply = buf.take();
    assert_eq!(panel.poll_live_bufs(), Dirty::YES, "{OWED}");
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "after-cancel"
    );

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(probe.try_recv(), Some(("click", vec![])));
    assert_eq!(probe.try_recv(), None);
}

/// A restore reply supersedes the old live view: the buf must stop
/// being watched so its stale content can't overwrite the fresh
/// snapshot, and later clicks must go through restore.
#[test]
fn restore_reply_stops_watching_buf() {
    let (eh, probe) = caudra_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default(), eh);
    panel.set_restore_channel(Some(EventSender::new(tx, 0)));
    let buf = finish_with_live_buf(&mut panel, "t1", "old-theme", false);
    assert!(panel.watching("t1"));

    let baked_gen = panel.snapshot_gen_of("t1").unwrap();
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("rebaked")])),
        Some(baked_gen),
    );
    assert!(!panel.watching("t1"));

    buf.set_lines(vec![snap_line("stale-mutation")]);
    assert_eq!(
        panel.poll_live_bufs(),
        Dirty::NO,
        "unwatched buf must no longer be polled"
    );
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "rebaked",
        "unwatched buf must not overwrite the restored snapshot"
    );

    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(area.y, area));
    assert_eq!(
        probe.try_recv(),
        Some(("restore", panel.lua_clicks["t1"].clone()))
    );
    assert_eq!(probe.try_recv(), None);
}

/// Requesting a rebake already stops watching: clicks inside the
/// request/reply window must restore (with the new theme) instead of
/// mutating the old-theme buf.
#[test]
fn rebake_request_stops_watching_buf() {
    let (eh, probe) = caudra_lua::test_support::probed_event_handle();
    let (tx, _rx) = flume::unbounded();
    let mut panel = MessagesPanel::new(UiConfig::default(), eh);
    panel.set_restore_channel(Some(EventSender::new(tx, 0)));
    finish_with_live_buf(&mut panel, "t1", "old-theme", false);
    assert!(panel.watching("t1"));

    let next_gen = panel.snapshot_gen_of("t1").unwrap() + 1;
    panel.rebake_stale_snapshots(next_gen);
    assert!(!panel.watching("t1"));
    assert_eq!(probe.try_recv(), Some(("restore", vec![])));
    assert_eq!(probe.try_recv(), None);
}

#[test]
fn live_buf_streams_across_clean_polls() {
    let buf = Arc::new(caudra_agent::SharedBuf::new());
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", SHELL_TOOL_NAME));
    panel.register_live_buf("t1".into(), Arc::clone(&buf));

    buf.append(snap_line("first"));
    assert_eq!(panel.poll_live_bufs(), Dirty::YES);
    assert_eq!(panel.poll_live_bufs(), Dirty::NO, "{QUIET}");

    buf.append(snap_line("second"));
    assert_eq!(panel.poll_live_bufs(), Dirty::YES);

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    let snapshot = msg.render_snapshot.as_ref().unwrap();
    assert_eq!(snapshot.lines.len(), 2);
}

#[test]
fn tool_done_without_live_buf_preserves_existing_snapshot() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", SHELL_TOOL_NAME));
    panel.tool_snapshot(
        "t1",
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("pre-existing")])),
        None,
    );
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: SHELL_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert_eq!(
        msg.render_snapshot.as_ref().unwrap().first_line_text(),
        "pre-existing"
    );
}

#[test]
fn tool_done_clean_live_buf_does_not_snapshot() {
    let buf = Arc::new(caudra_agent::SharedBuf::new());

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.register_live_buf("t1".into(), Arc::clone(&buf));
    panel.tool_start(start("t1", SHELL_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: SHELL_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });

    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert!(
        msg.render_snapshot.is_none(),
        "clean (never-written) live buf should not produce a snapshot"
    );
}

const REQUEST_RECORDED_MSG: &str = "a fired re-bake records the requested generation";
const NOT_RESTAMPED_MSG: &str =
    "the re-bake walk must not optimistically stamp the displayed generation";
const NO_REQUEST_MSG: &str = "snapshot-free message must not trigger a re-bake request";
const SUPERSEDED_DROP_MSG: &str =
    "a re-bake reply older than the applied generation must be dropped (monotonic)";

fn bash_tool_with_snapshot(id: &str) -> MessagesPanel {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start(id, SHELL_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: id.into(),
        tool: SHELL_TOOL_NAME.into(),
        output: ToolOutput::Plain("output".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    panel.tool_snapshot(
        id,
        BufferSnapshot::from_arc(Arc::new(vec![snap_line("rendered")])),
        None,
    );
    panel
}

fn rendered_snapshot() -> BufferSnapshot {
    BufferSnapshot::from_arc(Arc::new(vec![snap_line("rendered")]))
}

#[test]
fn rebake_walk_requests_without_stamping_displayed_generation() {
    let mut panel = bash_tool_with_snapshot("t1");
    panel.find_tool_msg_mut("t1").unwrap().tool_raw_input =
        Some(Arc::new(serde_json::json!({ "command": "echo" })));
    panel.push(DisplayMessage::new(DisplayRole::Assistant, "plain".into()));
    panel.set_restore_channel(Some(test_event_sender()));

    let baked_gen = panel.snapshot_gen_of("t1").unwrap();
    let next_gen = baked_gen + 1;
    panel.rebake_stale_snapshots(next_gen);

    assert_eq!(
        panel.snapshot_gen_of("t1"),
        Some(baked_gen),
        "{NOT_RESTAMPED_MSG}"
    );
    assert_eq!(
        panel.rebake_requested_gen("t1"),
        Some(next_gen),
        "{REQUEST_RECORDED_MSG}"
    );
    assert_eq!(panel.messages[1].snapshot_theme_gen, 0, "{NO_REQUEST_MSG}");
}

#[test]
fn superseded_rebake_reply_is_dropped() {
    let mut panel = bash_tool_with_snapshot("t1");
    let baked = panel.snapshot_gen_of("t1").unwrap();
    let newer = baked + 3;
    panel.tool_snapshot("t1", rendered_snapshot(), Some(newer));
    panel.tool_snapshot("t1", rendered_snapshot(), Some(baked + 1));
    assert_eq!(
        panel.snapshot_gen_of("t1"),
        Some(newer),
        "{SUPERSEDED_DROP_MSG}"
    );
}

fn test_event_sender() -> caudra_agent::EventSender {
    let (tx, _rx) = flume::unbounded();
    caudra_agent::EventSender::new(tx, 0)
}

const RAW_INPUT_SET_MSG: &str = "tool_raw_input must be set from event payload";
const HEADER_GEN_MSG: &str = "header snapshot must stamp the provided generation";
const LIVE_PANEL_GEN_MSG: &str = "live snapshot (None gen) must stamp with panel theme_generation";
const REBAKE_NOOP_MSG: &str = "rebake without channel must be a no-op (no requested gen)";

#[test_case(false ; "fresh_start")]
#[test_case(true  ; "upgrade_from_pending")]
fn tool_start_propagates_raw_input(pre_pending: bool) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    if pre_pending {
        panel.tool_pending("t1".into(), SHELL_TOOL_NAME);
    }
    let mut event = start("t1", SHELL_TOOL_NAME);
    event.raw_input = Some(serde_json::json!({"command": "echo"}));
    panel.tool_start(event);

    let raw = panel
        .find_tool_msg_mut("t1")
        .unwrap()
        .tool_raw_input
        .as_ref();
    assert!(raw.is_some(), "{RAW_INPUT_SET_MSG}");
    assert_eq!(
        raw.unwrap().as_ref(),
        &serde_json::json!({"command": "echo"}),
        "{RAW_INPUT_SET_MSG}"
    );
}

#[test]
fn header_snapshot_stamps_gen_on_top_level() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", SHELL_TOOL_NAME));
    panel.tool_header_snapshot("t1", rendered_snapshot(), Some(5));

    assert_eq!(panel.snapshot_gen_of("t1"), Some(5), "{HEADER_GEN_MSG}");
    let msg = panel.find_tool_msg_mut("t1").unwrap();
    assert!(msg.render_header.is_some(), "render_header must be set");
}

#[test]
fn live_snapshot_uses_panel_generation() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let expected_generation = panel.theme_generation;
    panel.tool_start(start("t1", SHELL_TOOL_NAME));
    panel.tool_snapshot("t1", rendered_snapshot(), None);

    assert_eq!(
        panel.snapshot_gen_of("t1"),
        Some(expected_generation),
        "{LIVE_PANEL_GEN_MSG}"
    );
}

#[test]
fn rebake_without_channel_is_noop() {
    let mut panel = bash_tool_with_snapshot("t1");
    panel.find_tool_msg_mut("t1").unwrap().tool_raw_input =
        Some(Arc::new(serde_json::json!({"command": "echo"})));
    let baked_gen = panel.snapshot_gen_of("t1").unwrap();

    panel.rebake_stale_snapshots(baked_gen + 1);

    assert!(
        panel.rebake_requested_gen("t1").is_none(),
        "{REBAKE_NOOP_MSG}"
    );
}

#[test]
fn hide_collapses_streaming_thinking() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel
        .streaming_thinking
        .set_buffer("**Reviewing changes**\n\nline one\nline two\nline three");
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("Thinking: Reviewing changes"),
        "collapsed view should show the active reasoning header; got: {text}"
    );
    assert!(
        !text.contains("line one"),
        "reasoning must stay hidden; got: {text}"
    );
}

#[test]
fn hide_click_expands_streaming_thinking() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.streaming_thinking.set_buffer("secret reasoning");
    let area = Rect::new(0, 0, 80, 10);
    render(&mut panel, 80, 10);
    assert!(
        panel.handle_click(0, area),
        "clicking collapsed thinking should toggle expand"
    );
    assert!(panel.streaming_reasoning_open());
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("secret reasoning"),
        "expanded view should show reasoning; got: {text}"
    );
    assert!(
        !text.contains("click to expand"),
        "collapsed hint should not appear after expand; got: {text}"
    );
}

#[test]
fn hide_keeps_cached_thinking_as_indicator() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.thinking_delta("reasoning here");
    panel.flush();
    assert!(matches!(
        panel.last_message_role(),
        Some(DisplayRole::Thinking)
    ));
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("Thought"),
        "cached thinking should persist as a lifecycle header; got: {text}"
    );
    assert!(
        !text.contains("reasoning here"),
        "reasoning must stay hidden in the indicator; got: {text}"
    );
}

#[test]
fn cached_collapsed_thinking_header_responds_to_hover() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.thinking_delta("hidden cached reasoning");
    panel.flush();
    let area = Rect::new(0, 0, 80, 10);
    render(&mut panel, area.width, area.height);

    panel.update_hover(area.y, area.x, area, false);
    let terminal = render(&mut panel, area.width, area.height);

    assert!(matches!(panel.hover, Some(HoverTarget::CachedThinking(0))));
    assert!(buffer_text(&terminal).contains("Thought"));
}

#[test]
fn streaming_collapsed_thinking_header_responds_to_hover() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel
        .streaming_thinking
        .set_buffer("hidden streaming reasoning");
    let area = Rect::new(0, 0, 80, 10);
    render(&mut panel, area.width, area.height);

    panel.update_hover(area.y, area.x, area, false);
    let terminal = render(&mut panel, area.width, area.height);

    assert!(matches!(panel.hover, Some(HoverTarget::StreamingThinking)));
    assert!(buffer_text(&terminal).contains("Thinking"));
}

#[test]
fn transcript_hover_clears_explicitly_and_on_scroll_or_layout_change() {
    let mut panel = panel_with_long_tool(200);
    let area = Rect::new(0, 0, 80, 24);

    panel.update_hover(area.y, area.x, area, false);
    assert!(panel.hover.is_some());
    panel.clear_hover();
    assert!(panel.hover.is_none());

    panel.update_hover(area.y, area.x, area, false);
    panel.set_scroll_top(panel.scroll_top());
    assert!(panel.hover.is_none());

    panel.update_hover(area.y, area.x, area, false);
    render(&mut panel, 79, area.height);
    assert!(panel.hover.is_none());
}

#[test]
fn default_collapses_streaming_thinking() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_thinking.set_buffer("visible reasoning");
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("Thinking") && !text.contains("visible reasoning"),
        "default config should collapse reasoning; got: {text}"
    );
}

#[test]
fn expanded_streaming_reasoning_has_an_active_header_and_separate_body() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: true,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel
        .streaming_thinking
        .set_buffer("**Reviewing**\n\nBody details");
    let text = buffer_text(&render(&mut panel, 80, 10));

    assert!(text.contains("Thinking: Reviewing"));
    assert!(text.contains("Body details"));
    assert_eq!(text.matches("Reviewing").count(), 1);
    assert!(!text.contains("thinking>"));
}

#[test]
fn streaming_reasoning_shows_a_spinner_and_tenths_timer() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.thinking_delta("working");
    panel.thinking_started = Some(Instant::now() - Duration::from_millis(1_201));

    let text = buffer_text(&render(&mut panel, 80, 5));

    assert!(SPINNER_GLYPHS.chars().any(|glyph| text.contains(glyph)));
    assert!(text.contains("Thinking · 1.2s"));
    assert_eq!(panel.cadence(), Cadence::SPINNER);
}

#[test]
fn hide_cached_thinking_persists_as_indicator() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    let lines: Vec<String> = (1..=7).map(|n| format!("cached line {n}")).collect();
    panel.thinking_delta(&lines.join("\n"));
    panel.flush();
    assert!(matches!(
        panel.last_message_role(),
        Some(DisplayRole::Thinking)
    ));
    let terminal = render(&mut panel, 80, 12);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("Thought"),
        "cached thinking should persist as a lifecycle header; got: {text}"
    );
    assert!(
        !text.contains("cached line 7"),
        "reasoning must stay hidden in the indicator; got: {text}"
    );
    assert!(
        !text.contains("cached line 1"),
        "reasoning must stay hidden in the indicator; got: {text}"
    );
}

#[test]
fn hide_cached_thinking_click_expands() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.thinking_delta("hidden cached reasoning");
    panel.flush();
    let area = Rect::new(0, 0, 80, 12);
    render(&mut panel, 80, 12);
    assert!(
        panel.handle_click(0, area),
        "clicking persisted thinking should toggle expand"
    );
    let terminal = render(&mut panel, 80, 12);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("hidden cached reasoning"),
        "expanded view shows full reasoning; got: {text}"
    );
    assert!(
        !text.contains("click to expand"),
        "footer should disappear when expanded; got: {text}"
    );
}

#[test]
fn stream_reset_clears_thinking_expand_state() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.streaming_thinking.set_buffer("secret reasoning");
    let area = Rect::new(0, 0, 80, 10);
    render(&mut panel, 80, 10);
    assert!(
        panel.handle_click(0, area),
        "clicking collapsed thinking should toggle expand"
    );
    assert!(panel.streaming_reasoning_open());
    panel.stream_reset();
    assert!(
        !panel.streaming_reasoning_open(),
        "stream_reset must restore the collapsed default so it does not leak into retries"
    );
    panel.streaming_thinking.set_buffer("fresh reasoning");
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("Thinking"),
        "new stream after reset should collapse again; got: {text}"
    );
    assert!(
        !text.contains("fresh reasoning"),
        "new stream must stay hidden; got: {text}"
    );
}

#[test]
fn stale_height_keeps_the_old_width_but_drawn_height_does_not() {
    let long_line = Line::from("x".repeat(77));
    let mut seg = Segment::with_lines(vec![long_line.clone()], "test".into(), None);

    let h_wide = seg.height(80);
    assert_eq!(h_wide, 1, "content fits the assistant inset at width 80");

    // Keeping the old height is what keeps a resize cheap: the document
    // layout stays put until the segment is really reflowed.
    seg.stale = true;
    assert_eq!(
        seg.height(40),
        h_wide,
        "stale segment should return old cached height, not recompute"
    );
    // Callers that re-wrap the lines themselves need the real number.
    assert_eq!(
        seg.drawn_height(40),
        3,
        "drawn_height must report what the lines really take at the new width"
    );

    seg.set_lines(vec![long_line]);
    assert_eq!(
        seg.height(40),
        3,
        "content reflows at the assistant inset width"
    );
}

#[test]
fn copy_after_resize_keeps_offscreen_text() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let body = "x".repeat(60);
    for i in 0..30 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("m{i:02}{body}"),
        ));
    }
    render(&mut panel, 80, 10);
    render(&mut panel, 40, 10);

    let total: u32 = panel.segment_heights().iter().map(|&h| h as u32).sum();
    let area = Rect::new(0, 0, 40, 10);
    let sel = make_sel(area, (0, 0), (total - 1, 39));
    let text = panel.extract_selection_text(&sel, area);

    // The top of the transcript is far off-screen and never gets reflowed.
    // Selection sizes its buffer from `height` and then re-wraps, so a height
    // measured at the old width would clip every line it copies.
    assert!(
        text.contains(&format!("m00{body}")),
        "off-screen message was truncated in the copy: {text:?}"
    );
}

#[test]
fn resize_reflows_only_viewport_segments() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    // 30 messages, each ~60 chars beyond the label — enough to exceed
    // viewport (10) + reflow margin (1 * 10 = 10 lines) so top segments
    // stay out of the reflow range when auto-scrolled to the bottom.
    for i in 0..30 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("message {i:02} {}", "x".repeat(60)),
        ));
    }
    render(&mut panel, 80, 10);
    let seg_count_before = panel.cache.len();
    assert!(seg_count_before > 0);

    render(&mut panel, 40, 10);

    // Cache preserved — no nuke
    assert_eq!(
        panel.cache.len(),
        seg_count_before,
        "resize must not clear the segment cache"
    );

    let segs = panel.cache.segments();

    // Bottom segments (near the auto-scrolled viewport) are reflowed
    let bottom_fresh = segs
        .iter()
        .filter(|s| s.msg_index.is_some())
        .rev()
        .take(5)
        .all(|s| !s.stale);
    assert!(
        bottom_fresh,
        "viewport segments should be reflowed to new width"
    );

    // Top segments (far above the viewport) remain width-stale
    let top_stale = segs
        .iter()
        .filter(|s| s.msg_index.is_some())
        .take(5)
        .all(|s| s.stale);
    assert!(
        top_stale,
        "off-viewport segments should stay width-stale after resize"
    );
}

fn msg_seg_text(panel: &MessagesPanel, msg_idx: usize) -> String {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.msg_index == Some(msg_idx) && s.tool_id.is_none())
        .unwrap()
        .lines()
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect()
}

#[test]
fn reflow_rebuilds_collapsed_thinking_instead_of_only_stamping() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.show_thinking = false;
    let mut m = DisplayMessage::new(
        DisplayRole::Thinking,
        "**First title**\n\none\ntwo".to_string(),
    );
    m.reasoning_open = Some(false);
    panel.push(m);
    render(&mut panel, 80, 10);
    assert!(
        msg_seg_text(&panel, 0).contains("Thought: First title"),
        "indicator should report the initial title"
    );

    // Change what the indicator renders, then mark it stale the way a theme
    // change does. Clearing the flag without rebuilding keeps the old spans.
    panel.messages[0].text = "**Second title**\n\none\ntwo\nthree\nfour".to_string();
    panel.cache.mark_all_width_stale();
    render(&mut panel, 80, 10);

    assert!(
        msg_seg_text(&panel, 0).contains("Thought: Second title"),
        "stale collapsed-thinking segment must be rebuilt, not just stamped; got: {}",
        msg_seg_text(&panel, 0)
    );
}

#[test]
fn reflow_runs_without_a_width_or_scroll_change() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for i in 0..5 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("message {i}"),
        ));
    }
    render(&mut panel, 80, 10);

    // Segments go stale between frames without either trigger firing.
    panel.cache.mark_all_width_stale();
    render(&mut panel, 80, 10);

    assert!(
        panel.cache.segments().iter().all(|s| !s.stale),
        "visible segments must be reflowed even when width and scroll_top are unchanged"
    );
}

#[test]
fn resize_reflows_tool_segment_and_keeps_instruction_segment() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: read_code_with_instructions(instruction_blocks()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    render(&mut panel, 80, 10);

    // Tool and instruction segments are built up front.
    let seg_count = panel.cache.len();
    assert!(seg_count >= 2);

    render(&mut panel, 40, 10);

    // The instruction segment already exists, so reflowing the tool segment
    // updates it in place rather than re-inserting (exercises the to_reflow
    // index path through `rebuild_tool_segment` and the upsert).
    assert_eq!(
        panel.cache.len(),
        seg_count,
        "reflow must reuse the existing instruction segment, not re-insert"
    );
    // The viewport auto-scrolls to the bottom, where the tool and instruction
    // segments sit, so neither stays stale after the resize.
    assert!(
        panel.cache.segments().iter().all(|s| !s.stale),
        "tool and instruction segments in the viewport must be reflowed, not left stale"
    );
}

#[test]
fn big_widen_keeps_no_stale_segment_in_the_viewport() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for i in 0..40 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("message {i:02} {}", "x".repeat(150)),
        ));
    }
    render(&mut panel, 80, 30);
    render(&mut panel, 240, 30);

    // 3x widen: content shrinks and the bottom pin pulls up, so a single
    // pre-reflow pass would leave stale segments in the viewport.
    let vh = 30u32;
    let top = panel.scroll_top() as u32;
    let mut offset: u32 = 0;
    for seg in panel.cache.segments() {
        let h = seg.height(240) as u32;
        let in_view = offset < top.saturating_add(vh) && offset + h > top;
        assert!(
            !(in_view && seg.stale),
            "a stale segment overlaps the viewport after a big widen"
        );
        offset += h;
    }
    assert!(
        panel.cache.segments().iter().any(|s| s.stale),
        "off-viewport segments must stay stale so the test exercises convergence"
    );
}

/// `scroll_top` sits inside the anchor segment, so a downward window measured
/// from that segment's start can be consumed entirely by rows above the first
/// visible one, leaving the screen full of segments still wrapped at the old
/// width.
#[test]
fn resize_low_in_a_tall_segment_leaves_no_stale_segment_in_the_viewport() {
    const VIEWPORT_HEIGHT: u16 = 10;
    const TALL_LINES: usize = 100;

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let tall = (0..TALL_LINES)
        .map(|i| format!("line {i:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    panel.push(DisplayMessage::new(DisplayRole::Assistant, tall));
    for i in 0..8 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("tail {i} {}", "x".repeat(60)),
        ));
    }
    render(&mut panel, 80, VIEWPORT_HEIGHT);

    // Five rows from the bottom of the tall first segment: the rest of the
    // viewport is filled by the segments after it.
    panel.set_scroll_top(TALL_LINES as u16 - 5);
    render(&mut panel, 80, VIEWPORT_HEIGHT);
    assert!(
        !panel.auto_scroll(),
        "the test must start anchored deep inside the tall segment"
    );

    render(&mut panel, 40, VIEWPORT_HEIGHT);

    let top = panel.scroll_top() as u32;
    let mut offset: u32 = 0;
    for seg in panel.cache.segments() {
        let h = seg.height(39) as u32;
        let in_view = offset < top + VIEWPORT_HEIGHT as u32 && offset + h > top;
        assert!(
            !(in_view && seg.stale),
            "a stale segment overlaps the viewport after resizing low in a tall segment"
        );
        offset += h;
    }
}

#[test]
fn anchored_resize_keeps_the_topmost_visible_segment() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    for i in 0..40 {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("message {i:02} {}", "x".repeat(60)),
        ));
    }
    render(&mut panel, 80, 10);
    panel.set_scroll_top(panel.max_scroll() / 2); // mid-transcript, unpins
    render(&mut panel, 80, 10);
    assert!(
        !panel.auto_scroll(),
        "the test must start anchored, not pinned"
    );
    let before = panel
        .cache
        .anchor_at(panel.scroll_top() as u32, 79)
        .expect("scroll_top lands inside a segment");

    render(&mut panel, 40, 10);

    let after = panel
        .cache
        .anchor_at(panel.scroll_top() as u32, 39)
        .expect("scroll_top still lands inside a segment after the resize");
    assert_eq!(
        after.0, before.0,
        "narrowing must not slide the anchored topmost segment off the viewport"
    );
    assert!(
        !panel.auto_scroll(),
        "an anchored mid-transcript resize must not flip to the bottom pin"
    );
}

const THEME_CODE: &str = "fn main() { let x = 1; }";
const THEME_CODE_KEYWORDS: [&str; 3] = ["fn", "main", "let"];

fn code_span_styles(panel: &MessagesPanel, tool_id: &str) -> Vec<(String, Style)> {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some(tool_id))
        .unwrap()
        .lines()
        .iter()
        .flat_map(|l| l.spans.iter())
        .filter(|s| THEME_CODE_KEYWORDS.contains(&s.content.trim()))
        .map(|s| (s.content.to_string(), s.style))
        .collect()
}

fn drain_highlight_worker(panel: &mut MessagesPanel) {
    let deadline = Instant::now() + HIGHLIGHT_DEADLINE;
    while panel.tick() == Dirty::NO {
        assert!(
            Instant::now() < deadline,
            "the highlight worker never delivered a result"
        );
        std::thread::yield_now();
    }
    render(panel, 80, 20);
}

/// The unit test above only proves two generations make two keys. This is the
/// wiring: drop `theme_gen` at a call site and the old palette gets spliced
/// straight back in with no test to catch it.
#[test]
fn theme_switch_repaints_highlighted_code() {
    theme::set(theme::load_by_name("dracula").unwrap());
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: ToolOutput::ReadCode {
            path: "file.rs".into(),
            start_line: 1,
            lines: vec![THEME_CODE.into()],
            total_lines: 1,
            instructions: None,
        },
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
    });
    render(&mut panel, 80, 20);
    drain_highlight_worker(&mut panel);
    let dracula = code_span_styles(&panel, "t1");
    assert!(!dracula.is_empty(), "no highlighted keywords to compare");

    theme::set(theme::load_by_name("tokyonight").unwrap());
    render(&mut panel, 80, 20);
    drain_highlight_worker(&mut panel);

    assert_ne!(
        dracula,
        code_span_styles(&panel, "t1"),
        "a theme switch must re-highlight, not splice old-palette lines back"
    );
}

const FIRST_TEXT: &str = "run the migration";
const FOLLOW_UP_TEXT: &str = "and then deploy";
const STALE_BUBBLE_MSG: &str = "the superseded bubble must disappear from the viewport";
const UNTOUCHED_MSG: &str = "a rejected replace must leave the transcript untouched";

fn style_of(terminal: &ratatui::Terminal<TestBackend>, text: &str) -> Style {
    let buf = terminal.backend().buffer();
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width)
            .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol()))
            .collect();
        if let Some(col) = row.find(text) {
            return buf.cell((col as u16, y)).unwrap().style();
        }
    }
    panic!("{text} was never rendered");
}

/// `Chat::mark_finished` corrects a bubble long after it was drawn, with the
/// transcript still growing in between. Unless `replace` throws the baked
/// segments away, the viewport keeps painting a green "Done!" the message
/// vector no longer holds.
#[test]
fn replace_repaints_the_corrected_bubble_in_place() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, FIRST_TEXT.into()));
    let bubble = panel.push(DisplayMessage::new(DisplayRole::Done, DONE_TEXT.into()));
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        FOLLOW_UP_TEXT.into(),
    ));
    let done_style = style_of(&render(&mut panel, 80, 24), DONE_TEXT);

    panel.replace(
        bubble,
        DisplayMessage::new(DisplayRole::Error, ERROR_TEXT.into()),
    );

    let rendered = render(&mut panel, 80, 24);
    let text = buffer_text(&rendered);
    let texts: Vec<&str> = panel.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, [FIRST_TEXT, ERROR_TEXT, FOLLOW_UP_TEXT]);
    assert!(text.contains(ERROR_TEXT), "got: {text}");
    assert!(text.contains(FOLLOW_UP_TEXT), "got: {text}");
    assert!(!text.contains(DONE_TEXT), "{STALE_BUBBLE_MSG}: {text}");
    assert_ne!(
        style_of(&rendered, ERROR_TEXT),
        done_style,
        "the corrected bubble kept the success styling"
    );
}

#[test]
fn replace_past_the_end_is_a_noop() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::Done, DONE_TEXT.into()));
    rebuild(&mut panel);

    panel.replace(
        panel.message_count(),
        DisplayMessage::new(DisplayRole::Error, ERROR_TEXT.into()),
    );

    assert_eq!(panel.message_count(), 1, "{UNTOUCHED_MSG}");
    let text = buffer_text(&render(&mut panel, 80, 10));
    assert!(text.contains(DONE_TEXT), "{UNTOUCHED_MSG}: {text}");
    assert!(!text.contains(ERROR_TEXT), "{UNTOUCHED_MSG}: {text}");
}

const WIDE_CHART: &str = "```mermaid\nflowchart LR\n  A[Ingest events] --> B[Normalise schema] --> C[Enrich metadata] --> D[Write to store]\n```";

#[test]
fn expanded_reasoning_diagram_rows_follow_the_header() {
    let message = DisplayMessage::new(
        DisplayRole::Thinking,
        format!("**Mapping**\n\n{WIDE_CHART}"),
    );
    let built = build_thinking_lines(&message, 40, Vec::new());

    assert!(!built.diagrams.is_empty());
    assert!(built.diagrams[0].rows.start >= 2);
}
const PAN_WIDTH: u16 = 40;
const PAN_HEIGHT: u16 = 20;

fn panel_with_chart() -> MessagesPanel {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        WIDE_CHART.into(),
    ));
    render(&mut panel, PAN_WIDTH, PAN_HEIGHT);
    panel
}

fn diagram_text(panel: &MessagesPanel) -> Vec<String> {
    let segment = panel.cache.get(0).expect("one segment");
    segment
        .diagrams()
        .iter()
        .flat_map(|span| span.rows.clone())
        .filter_map(|row| segment.lines().get(row))
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn a_wide_chart_records_a_diagram_span_wider_than_the_viewport() {
    let panel = panel_with_chart();
    let spans = panel.cache.get(0).expect("one segment").diagrams();
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0].id, 0);
    assert!(
        spans[0].full_width > PAN_WIDTH,
        "fixture must overflow: {spans:?}"
    );
    assert!(!spans[0].rows.is_empty());
}

#[test]
fn panning_moves_the_window_and_keeps_the_height() {
    let mut panel = panel_with_chart();
    let before = diagram_text(&panel);
    let height: u16 = panel.segment_heights().iter().sum();

    assert!(panel.pan_visible_diagram(PAN_STEP_TEST));
    render(&mut panel, PAN_WIDTH, PAN_HEIGHT);

    let after = diagram_text(&panel);
    assert_ne!(before, after, "pan must move the window");
    assert_eq!(before.len(), after.len(), "pan must not change row count");
    assert_eq!(
        height,
        panel.segment_heights().iter().sum::<u16>(),
        "pan must not change segment height"
    );
}

const PAN_STEP_TEST: i32 = 4;

#[test]
fn panning_left_at_the_origin_does_nothing() {
    let mut panel = panel_with_chart();
    assert!(!panel.pan_visible_diagram(-PAN_STEP_TEST));
}

#[test]
fn panning_right_stops_at_the_far_edge() {
    let mut panel = panel_with_chart();
    for _ in 0..200 {
        if !panel.pan_visible_diagram(PAN_STEP_TEST) {
            break;
        }
        render(&mut panel, PAN_WIDTH, PAN_HEIGHT);
    }
    assert!(
        !panel.pan_visible_diagram(PAN_STEP_TEST),
        "pan must clamp at the far edge"
    );
    let rows = diagram_text(&panel);
    assert!(
        rows.iter().all(|row| !row.ends_with('›')),
        "the far edge must be fully revealed: {rows:?}"
    );
}

#[test]
fn a_panned_chart_returns_to_the_origin() {
    let mut panel = panel_with_chart();
    let origin = diagram_text(&panel);
    assert!(panel.pan_visible_diagram(PAN_STEP_TEST));
    render(&mut panel, PAN_WIDTH, PAN_HEIGHT);
    assert!(panel.pan_visible_diagram(-PAN_STEP_TEST));
    render(&mut panel, PAN_WIDTH, PAN_HEIGHT);
    assert_eq!(diagram_text(&panel), origin);
    assert!(
        panel.diagram_pans.is_empty(),
        "returning to the origin must drop the entry"
    );
}

#[test]
fn a_transcript_without_a_diagram_refuses_to_pan() {
    let mut panel = panel_with_msgs(&["just some prose"], PAN_WIDTH, PAN_HEIGHT);
    assert!(!panel.pan_visible_diagram(PAN_STEP_TEST));
    assert!(!panel.pan_visible_diagram(-PAN_STEP_TEST));
}

#[test]
fn hovering_a_diagram_row_targets_it() {
    let mut panel = panel_with_chart();
    let area = Rect::new(0, 0, PAN_WIDTH, PAN_HEIGHT);
    let segment = panel.cache.get(0).expect("one segment");
    let content_start = segment.chrome(panel.viewport_width).content_start();
    let row = content_start + segment.diagrams()[0].rows.start as u16;

    panel.update_hover(row, 1, area, false);
    assert!(
        matches!(panel.hover, Some(HoverTarget::Diagram(_))),
        "{:?}",
        panel.hover
    );

    panel.update_hover(0, 1, area, false);
    assert!(
        !matches!(panel.hover, Some(HoverTarget::Diagram(_))),
        "a prose row is not a diagram: {:?}",
        panel.hover
    );
}

#[test]
fn a_hovered_diagram_pans_and_an_unhovered_one_does_not() {
    let mut panel = panel_with_chart();
    let area = Rect::new(0, 0, PAN_WIDTH, PAN_HEIGHT);
    assert!(!panel.pan_hovered_diagram(PAN_STEP_TEST), "no hover yet");

    let segment = panel.cache.get(0).expect("one segment");
    let content_start = segment.chrome(panel.viewport_width).content_start();
    let row = content_start + segment.diagrams()[0].rows.start as u16;
    panel.update_hover(row, 1, area, false);
    assert!(panel.pan_hovered_diagram(PAN_STEP_TEST));
}

#[test]
fn copying_a_panned_diagram_still_yields_the_source() {
    let mut panel = panel_with_chart();
    assert!(panel.pan_visible_diagram(PAN_STEP_TEST));
    render(&mut panel, PAN_WIDTH, PAN_HEIGHT);

    let total: u16 = panel.segment_heights().iter().sum();
    let area = Rect::new(0, 0, PAN_WIDTH, total.max(1));
    let sel = make_sel(area, (0, 0), ((total.saturating_sub(1)) as u32, 0));
    let copied = panel.extract_selection_text(&sel, area);
    assert!(copied.contains("flowchart LR"), "{copied:?}");
    assert!(copied.contains("```mermaid"), "{copied:?}");
}

const PROSE: &str = "Here is a fairly long sentence of prose that will certainly wrap across several rows at this width, which is exactly the case that separates a naive row calculation from a correct one.";

#[test]
fn the_keyboard_finds_a_diagram_below_wrapping_prose() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, "draw it".into()));
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        format!("{PROSE}\n\n{WIDE_CHART}"),
    ));
    render(&mut panel, PAN_WIDTH, PAN_HEIGHT);

    assert!(
        !panel
            .cache
            .get(1)
            .expect("assistant segment")
            .diagrams()
            .is_empty(),
        "fixture must contain a diagram"
    );
    assert!(
        panel.most_visible_diagram().is_some(),
        "the keyboard must find the diagram under wrapping prose"
    );
    assert!(panel.pan_visible_diagram(PAN_STEP_TEST));
}

#[test]
fn the_keyboard_and_the_pointer_agree_on_the_target() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, "draw it".into()));
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        format!("{PROSE}\n\n{WIDE_CHART}"),
    ));
    render(&mut panel, PAN_WIDTH, PAN_HEIGHT);
    let area = Rect::new(0, 0, PAN_WIDTH, PAN_HEIGHT);

    let hovered: Vec<DiagramKey> = (0..PAN_HEIGHT)
        .filter_map(|row| {
            panel.update_hover(row, 1, area, false);
            match panel.hover {
                Some(HoverTarget::Diagram(key)) => Some(key),
                _ => None,
            }
        })
        .collect();

    assert!(!hovered.is_empty(), "the pointer must reach the diagram");
    assert_eq!(
        panel.most_visible_diagram(),
        Some(hovered[0]),
        "keyboard target must be the diagram the pointer sees"
    );
}

/// Enough wrapped prose above the chart that a row calculation which mistakes
/// line indices for display rows lands outside the viewport entirely.
#[test]
fn the_keyboard_finds_a_chart_under_heavily_wrapped_prose() {
    let prose = (0..24)
        .map(|i| format!("Paragraph {i} is long enough to wrap several times at this narrow width, which is what pushes the chart away from its line index."))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        format!("{prose}\n\n{WIDE_CHART}"),
    ));
    render(&mut panel, PAN_WIDTH, PAN_HEIGHT);
    panel.enable_auto_scroll();
    render(&mut panel, PAN_WIDTH, PAN_HEIGHT);

    let area = Rect::new(0, 0, PAN_WIDTH, PAN_HEIGHT);
    let seen: Vec<DiagramKey> = (0..PAN_HEIGHT)
        .filter_map(|row| {
            panel.update_hover(row, 1, area, false);
            match panel.hover {
                Some(HoverTarget::Diagram(key)) => Some(key),
                _ => None,
            }
        })
        .collect();
    assert!(
        !seen.is_empty(),
        "the chart must be on screen to be pannable"
    );

    assert_eq!(
        panel.most_visible_diagram(),
        Some(seen[0]),
        "the keyboard must agree with the pointer"
    );
    assert!(panel.pan_visible_diagram(PAN_STEP_TEST));
}

#[test]
fn two_equally_visible_charts_hand_the_keys_to_the_later_one() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        WIDE_CHART.into(),
    ));
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        WIDE_CHART.into(),
    ));
    render(&mut panel, PAN_WIDTH, 60);

    let key = panel.most_visible_diagram().expect("a chart is on screen");
    assert_eq!(key.msg_index, 1, "the later message wins an equal split");

    assert!(panel.pan_visible_diagram(PAN_STEP_TEST));
    assert_eq!(
        panel.diagram_pans.keys().copied().collect::<Vec<_>>(),
        vec![key],
        "only the targeted chart moves"
    );
}

const NARROW_CHART: &str = "```mermaid\nflowchart TD\n  A[Go] --> B[Ok]\n```";

/// The pointer can single out a chart, a key cannot. Targeting the chart with
/// the most rows strands the keys when that chart already fits.
#[test]
fn the_keyboard_skips_a_chart_that_already_fits() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        WIDE_CHART.into(),
    ));
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        format!("{NARROW_CHART}\n\n{NARROW_CHART}"),
    ));
    render(&mut panel, PAN_WIDTH, 60);

    let wide = DiagramKey {
        msg_index: 0,
        id: 0,
    };
    assert!(
        panel.pan_range(wide).is_some_and(|(_, max)| max > 0),
        "the fixture's first chart must overflow"
    );
    let fitting: Vec<DiagramKey> = (0..2).map(|id| DiagramKey { msg_index: 1, id }).collect();
    for key in &fitting {
        assert_eq!(
            panel.pan_range(*key).map(|(_, max)| max),
            Some(0),
            "the later message's charts must already fit, so rows alone would strand the keys"
        );
    }

    let panned = panel.pan_visible_diagram(PAN_STEP_TEST);
    assert!(
        panned,
        "a pannable chart is on screen, so the key must move it"
    );
    assert_eq!(
        panel.diagram_pans.keys().copied().collect::<Vec<_>>(),
        vec![wide],
        "the keys must reach the only chart that can move"
    );
}

#[test]
fn a_chart_that_fits_leaves_the_arrow_keys_alone() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        NARROW_CHART.into(),
    ));
    render(&mut panel, 120, PAN_HEIGHT);

    assert!(panel.most_visible_diagram().is_none());
    assert!(!panel.pan_visible_diagram(PAN_STEP_TEST));
    assert!(!panel.pan_visible_diagram(-PAN_STEP_TEST));
}

const COMPACT_ROW_MSG: &str = "a compact tool call must occupy exactly one row";
const COMPACT_GAPLESS_MSG: &str = "consecutive compact rows must stack without a blank line";
const STREAMING_GAP_MSG: &str = "a live compact thought must occupy the same rows as a settled one";
const COMPACT_CLICK_MSG: &str = "a compact row must open on click";
const COMPACT_RESET_MSG: &str = "flipping density must drop every per-item override";
const COMPACT_REHIDE_MSG: &str = "clicking through a compact row must end back at its header";
const COMPACT_HOVER_MSG: &str = "a compact row must highlight exactly when a click would act";
const CONTAINER_BODY_MSG: &str = "a container must keep its child rows in compact view";
const SHELL_VIEW_STICKY_MSG: &str =
    "re-opening a shell card must restore the last raw/filtered view";
const SHELL_HOVER_MSG: &str = "the raw/filtered switch must highlight itself, not the card header";
const MAX_COMPACT_CLICK_CYCLE: usize = 4;
const TRUNCATING_LINES: usize = 200;

fn compact_panel(ids: &[(&str, &'static str)]) -> MessagesPanel {
    let mut panel = panel_with_tools(ids);
    panel.set_view(ViewMode::Compact);
    panel
}

fn finished(panel: &mut MessagesPanel, ids: &[&str]) {
    for id in ids {
        panel.tool_done(done(id));
    }
}

fn first_line_text(panel: &MessagesPanel, seg: usize) -> String {
    panel.cache.get(seg).unwrap().lines()[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[test]
fn a_compact_tool_call_collapses_to_one_row() {
    let mut panel = compact_panel(&[("t1", FILE_GREP_TOOL_NAME)]);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);

    assert_eq!(panel.segment_heights(), vec![1], "{COMPACT_ROW_MSG}");
}

#[test]
fn compact_rows_stack_without_separator_lines() {
    let mut panel = compact_panel(&[("t1", FILE_GREP_TOOL_NAME), ("t2", FILE_READ_TOOL_NAME)]);
    finished(&mut panel, &["t1", "t2"]);
    rebuild(&mut panel);

    assert_eq!(panel.segment_heights(), vec![1, 1], "{COMPACT_GAPLESS_MSG}");
}

#[test]
fn reasoning_and_wrapped_rows_join_the_compact_list() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Compact);
    let mut thought = DisplayMessage::new(DisplayRole::Thinking, "planning".into());
    thought.reasoning_open = Some(false);
    panel.push(thought);
    let mut long = start("t1", FILE_READ_TOOL_NAME);
    long.summary = "a/very/deeply/nested/path/that/has/to/wrap/at/this/width.md".into();
    panel.tool_start(long);
    panel.tool_done(done("t1"));
    panel.tool_start(start("t2", FILE_GREP_TOOL_NAME));
    panel.tool_done(done("t2"));
    render(&mut panel, 40, 24);

    assert!(
        panel.segment_heights()[1] > 1,
        "the long row must wrap: {:?}",
        panel.segment_heights()
    );
    let margins: Vec<u16> = (0..panel.cache.len())
        .map(|i| panel.cache.get(i).unwrap().chrome(40).margin_top)
        .collect();
    assert_eq!(margins, vec![0, 0, 0], "{COMPACT_GAPLESS_MSG}");
}

#[test]
fn a_user_message_still_separates_from_the_compact_list() {
    let mut panel = compact_panel(&[("t1", FILE_GREP_TOOL_NAME)]);
    finished(&mut panel, &["t1"]);
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        "next question".into(),
    ));
    rebuild(&mut panel);

    assert!(
        panel.segment_heights()[1] > 1,
        "a user bubble keeps its card padding"
    );
}

/// The gap used to appear while the model was thinking and disappear the
/// moment the block settled, because only settled blocks reach the margin pass.
#[test]
fn a_streaming_thought_takes_the_same_room_as_a_settled_one() {
    let mut panel = compact_panel(&[("t1", FILE_GREP_TOOL_NAME)]);
    finished(&mut panel, &["t1"]);
    render(&mut panel, 80, 24);
    let settled = panel.last_total_lines;

    panel.thinking_delta("weighing options");
    render(&mut panel, 80, 24);
    let streaming = panel.last_total_lines;

    let mut msg = DisplayMessage::new(DisplayRole::Thinking, "weighing options".into());
    msg.reasoning_open = Some(false);
    panel.streaming_thinking.clear();
    panel.push(msg);
    render(&mut panel, 80, 24);

    assert_eq!(
        streaming, panel.last_total_lines,
        "{STREAMING_GAP_MSG} (settled without a thought: {settled})"
    );
}

#[test]
fn an_expanded_streaming_thought_keeps_its_separator() {
    let mut panel = compact_panel(&[("t1", FILE_GREP_TOOL_NAME)]);
    panel.set_view(ViewMode::Expanded);
    finished(&mut panel, &["t1"]);
    render(&mut panel, 80, 24);
    let settled = panel.last_total_lines;

    panel.thinking_delta("weighing options");
    render(&mut panel, 80, 24);

    assert!(
        panel.last_total_lines > settled + 1,
        "expanded reasoning must keep the blank line above it"
    );
}

#[test]
fn a_compact_row_names_its_tool_with_a_sigil_and_label() {
    let mut panel = compact_panel(&[("t1", FILE_GREP_TOOL_NAME)]);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);

    assert_eq!(first_line_text(&panel, 0), "✱ Grep t1 (1 lines)");
}

#[test]
fn an_unknown_tool_falls_back_to_its_registered_name() {
    let mut panel = compact_panel(&[("t1", "mystery_tool")]);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);

    assert_eq!(first_line_text(&panel, 0), "⚙ mystery_tool t1 (1 lines)");
}

#[test]
fn a_compact_row_lists_the_inputs_its_header_omits() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Compact);
    let mut event = start("t1", FILE_READ_TOOL_NAME);
    event.summary = "src/main.rs".into();
    event.raw_input = Some(serde_json::json!({
        "filePath": "src/main.rs",
        "offset": 1,
        "limit": 260,
    }));
    panel.tool_start(event);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);

    // Key order follows serde_json's map, which the workspace flips to
    // insertion order via `preserve_order`; only membership is stable.
    let line = first_line_text(&panel, 0);
    assert!(line.contains("offset=1"), "{line}");
    assert!(line.contains("limit=260"), "{line}");
    assert!(
        !line.contains("filePath"),
        "the header already shows the path: {line}"
    );
}

#[test]
fn a_click_opens_one_compact_row_and_leaves_its_neighbour_alone() {
    let mut panel = compact_panel(&[("t1", FILE_GREP_TOOL_NAME), ("t2", FILE_GREP_TOOL_NAME)]);
    finished(&mut panel, &["t1", "t2"]);
    rebuild(&mut panel);

    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(0, area), "{COMPACT_CLICK_MSG}");
    rebuild(&mut panel);

    assert!(
        panel.segment_heights()[0] > 1,
        "{COMPACT_CLICK_MSG}: {:?}",
        panel.segment_heights()
    );
    assert_eq!(
        panel.cache.get(1).unwrap().lines().len(),
        1,
        "{COMPACT_ROW_MSG}"
    );
}

#[test]
fn flipping_density_clears_opened_rows() {
    let mut panel = compact_panel(&[("t1", SHELL_TOOL_NAME)]);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);
    panel.handle_click(0, Rect::new(0, 0, 80, 24));
    assert!(!panel.card_closed("t1"));

    panel.set_view(ViewMode::Expanded);

    assert!(panel.disclosure.is_empty(), "{COMPACT_RESET_MSG}");
}

/// Hover feedback promises the click will do something, so the two must agree
/// at every step of the compact cycle, not just on the closed header.
#[test_case(FILE_GREP_TOOL_NAME; "short row")]
#[test_case(SHELL_TOOL_NAME; "truncated row")]
fn compact_hover_tracks_clickability_through_the_whole_cycle(tool: &'static str) {
    let mut panel = compact_panel(&[("t1", tool)]);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);
    let area = Rect::new(0, 0, 80, 24);

    for _ in 0..MAX_COMPACT_CLICK_CYCLE {
        panel.update_hover(area.y, area.x, area, false);
        let hovered = panel.hover.is_some();
        assert_eq!(
            hovered,
            panel.handle_click(0, area),
            "{COMPACT_HOVER_MSG} (expanded={:?})",
            panel.disclosure.get("t1")
        );
        rebuild(&mut panel);
    }
}

/// A container's body is its list of child rows, and those already collapse
/// to their own headers. Hiding it behind one more click would bury the
/// structure the row exists to show.
#[test_case(TASK_TOOL_NAME; "task")]
#[test_case(BATCH_TOOL_NAME; "batch")]
fn a_container_row_keeps_its_body_in_compact_view(tool: &'static str) {
    let mut panel = compact_panel(&[("t1", tool)]);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);

    let container = panel.segment_heights()[0];
    let mut plain = compact_panel(&[("t1", FILE_GREP_TOOL_NAME)]);
    finished(&mut plain, &["t1"]);
    rebuild(&mut plain);

    assert!(
        container > plain.segment_heights()[0],
        "{CONTAINER_BODY_MSG}: {container} rows"
    );
}

/// A short row opens on the first click and has nothing further to give, so
/// the next click has to take it back to its header rather than do nothing.
#[test]
fn clicking_an_opened_compact_row_returns_it_to_its_header() {
    let mut panel = compact_panel(&[("t1", FILE_GREP_TOOL_NAME)]);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);
    let area = Rect::new(0, 0, 80, 24);
    let header = panel.segment_heights()[0];

    assert!(panel.handle_click(0, area));
    assert!(panel.segment_heights()[0] > header, "the row should open");

    assert!(panel.handle_click(0, area));
    assert_eq!(panel.segment_heights()[0], header, "{COMPACT_REHIDE_MSG}");
    assert!(panel.card_closed("t1"), "{COMPACT_REHIDE_MSG}");
}

/// A row with a truncated body has a middle state, and the cycle still has to
/// end back at the header instead of stalling on the fully expanded body.
#[test]
fn a_truncated_compact_row_cycles_back_to_its_header() {
    let mut panel = compact_panel(&[("t1", FILE_GREP_TOOL_NAME)]);
    panel.tool_done(long_done("t1", TRUNCATING_LINES));
    rebuild(&mut panel);
    let area = Rect::new(0, 0, 80, 24);
    let header = panel.segment_heights()[0];

    for _ in 0..MAX_COMPACT_CLICK_CYCLE {
        assert!(panel.handle_click(0, area));
        if panel.card_closed("t1") {
            assert_eq!(panel.segment_heights()[0], header, "{COMPACT_REHIDE_MSG}");
            return;
        }
    }
    panic!("{COMPACT_REHIDE_MSG}");
}

#[test]
fn an_expanded_row_stays_put_when_it_has_nothing_left_to_open() {
    let mut panel = compact_panel(&[("t1", FILE_GREP_TOOL_NAME)]);
    panel.set_view(ViewMode::Expanded);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);

    assert!(!panel.handle_click(0, Rect::new(0, 0, 80, 24)));
}

#[test]
fn a_compact_row_with_nothing_to_show_ignores_clicks() {
    let mut panel = compact_panel(&[("t1", FILE_GREP_TOOL_NAME)]);
    rebuild(&mut panel);

    assert!(!panel.handle_click(0, Rect::new(0, 0, 80, 24)));
}

#[test]
fn compact_reasoning_reports_its_summary_and_duration() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Compact);
    let mut msg = DisplayMessage::new(DisplayRole::Thinking, "**Weighing options**\n\nbody".into());
    msg.reasoning_open = Some(false);
    msg.thinking_duration = Some(Duration::from_millis(9_700));
    panel.push(msg);
    rebuild(&mut panel);

    assert_eq!(panel.segment_heights(), vec![1]);
    assert_eq!(
        first_line_text(&panel, 0),
        "Thought: Weighing options · 9.7s"
    );
}

#[test_case(
    "**Continuing Quality Review**\n\nDetails.\n\n**Next section**\n\nMore.",
    Some("Continuing Quality Review"),
    "Details.\n\n**Next section**\n\nMore."
    ; "leading_title_block"
)]
#[test_case("**Continuing Quality Review**", Some("Continuing Quality Review"), "" ; "title_only")]
#[test_case("**Important:** keep this in the body.", None, "**Important:** keep this in the body." ; "inline_bold_is_body")]
#[test_case("Details only.", None, "Details only." ; "plain_body")]
fn reasoning_titles_require_a_leading_standalone_bold_block(
    text: &str,
    expected_title: Option<&str>,
    expected_body: &str,
) {
    let summary = reasoning_summary(text);
    assert_eq!(summary.title, expected_title);
    assert_eq!(summary.body, expected_body);
}

#[test]
fn active_and_completed_reasoning_have_distinct_headers() {
    let line_text = |line: &Line<'_>| {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };
    assert_eq!(
        line_text(&thought_line("**Reviewing**", None, false)[0]),
        "Thinking: Reviewing"
    );
    assert_eq!(
        line_text(&thought_line("plain body", Some(Duration::from_secs(2)), true)[0]),
        "Thought · 2.0s"
    );
}

#[test]
fn expanded_reasoning_keeps_the_title_out_of_the_body() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: true,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.push(DisplayMessage::new(
        DisplayRole::Thinking,
        "**Reviewing**\n\nBody details".into(),
    ));
    rebuild(&mut panel);

    let text = msg_seg_text(&panel, 0);
    assert!(text.contains("Thought: Reviewing"));
    assert!(text.contains("Body details"));
    assert_eq!(text.matches("Reviewing").count(), 1);
}

#[test]
fn untimed_reasoning_drops_the_duration_suffix() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Compact);
    let mut msg = DisplayMessage::new(DisplayRole::Thinking, "just a thought".into());
    msg.reasoning_open = Some(false);
    panel.push(msg);
    rebuild(&mut panel);

    assert_eq!(first_line_text(&panel, 0), "Thought");
}

#[test]
fn switching_to_compact_collapses_reasoning_that_show_thinking_had_open() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: true,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.push(DisplayMessage::new(
        DisplayRole::Thinking,
        "reasoning".into(),
    ));
    rebuild(&mut panel);
    assert!(panel.reasoning_open(&panel.messages[0], 0));

    panel.set_view(ViewMode::Compact);
    rebuild(&mut panel);
    assert!(!panel.reasoning_open(&panel.messages[0], 0));

    panel.set_view(ViewMode::Expanded);
    assert!(
        panel.reasoning_open(&panel.messages[0], 0),
        "{COMPACT_RESET_MSG}"
    );
}

#[test]
fn a_compact_thought_reopens_on_click_even_when_show_thinking_is_on() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: true,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.push(DisplayMessage::new(
        DisplayRole::Thinking,
        THINKING_TEXT.into(),
    ));
    panel.set_view(ViewMode::Compact);
    rebuild(&mut panel);

    assert!(panel.handle_click(0, Rect::new(0, 0, 80, 24)));
    assert!(panel.reasoning_open(&panel.messages[0], 0));
}

#[test]
fn a_live_reasoning_block_records_how_long_it_ran() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.thinking_delta("reasoning");
    panel.text_delta("answer");

    assert!(panel.messages[0].thinking_duration.is_some());
    assert!(panel.thinking_started.is_none());
}

#[test]
fn reasoning_boundary_completes_one_block_and_starts_another() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.thinking_delta("**First**\n\nbody");
    panel.thinking_boundary();
    panel.thinking_delta("**Second**\n\nbody");

    assert_eq!(panel.messages.len(), 1);
    assert_eq!(panel.messages[0].text, "**First**\n\nbody");
    assert!(panel.messages[0].thinking_duration.is_some());
    assert_eq!(panel.streaming_thinking.buffer(), "**Second**\n\nbody");
}

#[test_case(Duration::from_millis(420), "Thought: t · 420ms" ; "sub_second_stays_in_millis")]
#[test_case(Duration::from_millis(9_700), "Thought: t · 9.7s" ; "seconds_keep_one_decimal")]
#[test_case(Duration::from_secs(125), "Thought: t · 2m 5s" ; "minutes_split_from_seconds")]
fn thought_durations_are_formatted_by_magnitude(duration: Duration, expected: &str) {
    let line = &thought_line("**t**", Some(duration), true)[0];
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert_eq!(text, expected);
}

#[test_case(Duration::from_millis(420), "0.4s" ; "sub_second")]
#[test_case(Duration::from_millis(9_700), "9.7s" ; "under_a_minute")]
#[test_case(Duration::from_millis(125_340), "2m 5.3s" ; "over_a_minute")]
fn live_thinking_duration_always_keeps_tenths(duration: Duration, expected: &str) {
    assert_eq!(format_live_duration(duration), expected);
}

#[test]
fn expanded_density_keeps_the_status_dot_and_the_card() {
    let mut panel = panel_with_tools(&[("t1", FILE_GREP_TOOL_NAME)]);
    panel.set_view(ViewMode::Expanded);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);

    assert!(first_line_text(&panel, 0).starts_with("● file_grep> "));
    assert!(panel.segment_heights()[0] > 1);
}

const MODE_EFFECT_MSG: &str = "the mode decides a read-only card; a writing card decides itself";
const AUTO_TAIL_MSG: &str = "auto must leave the newest card open";
const AUTO_HANDOFF_MSG: &str = "the card auto opened must close once a newer one takes its place";
const AUTO_STREAM_MSG: &str = "nothing settled is newest while the model is still writing";
const MANUAL_STICKY_MSG: &str = "a card the reader opened must stay open as the transcript grows";
const REASONING_GATE_MSG: &str =
    "the gate decides whether reasoning may show; the mode only decides when";
const REASONING_TAIL_MSG: &str = "auto must open the newest reasoning and close the one before it";

fn mode_panel(view: ViewMode, ids: &[(&str, &'static str)]) -> MessagesPanel {
    let mut panel = panel_with_tools(ids);
    panel.set_view(view);
    for &(id, _) in ids {
        panel.tool_done(done(id));
    }
    rebuild(&mut panel);
    panel
}

/// Read-only calls are the only ones the mode is allowed to hide: a call that
/// wrote something keeps its body in every mode, or the transcript stops
/// showing what happened to the workspace.
#[test_case(ViewMode::Expanded, FILE_GREP_TOOL_NAME, true; "expanded opens a read")]
#[test_case(ViewMode::Compact, FILE_GREP_TOOL_NAME, false; "compact closes a read")]
#[test_case(ViewMode::Auto, FILE_GREP_TOOL_NAME, true; "auto opens the newest read")]
#[test_case(ViewMode::Expanded, FILE_WRITE_TOOL_NAME, true; "expanded opens a write")]
#[test_case(ViewMode::Compact, FILE_WRITE_TOOL_NAME, true; "compact still opens a write")]
#[test_case(ViewMode::Auto, FILE_WRITE_TOOL_NAME, true; "auto still opens a write")]
fn the_mode_only_decides_cards_that_changed_nothing(
    view: ViewMode,
    tool: &'static str,
    open: bool,
) {
    let panel = mode_panel(view, &[("t1", tool)]);

    assert_eq!(!panel.card_closed("t1"), open, "{MODE_EFFECT_MSG}");
}

/// The point of auto: the call being worked on reads in full, and the ones
/// behind it fall back to a row without the reader touching anything.
#[test]
fn auto_opens_the_newest_read_and_closes_the_one_before_it() {
    let mut panel = mode_panel(ViewMode::Auto, &[("t1", FILE_GREP_TOOL_NAME)]);
    assert!(!panel.card_closed("t1"), "{AUTO_TAIL_MSG}");

    panel.tool_start(start("t2", FILE_GREP_TOOL_NAME));
    panel.tool_done(done("t2"));
    rebuild(&mut panel);

    assert!(panel.card_closed("t1"), "{AUTO_HANDOFF_MSG}");
    assert!(!panel.card_closed("t2"), "{AUTO_TAIL_MSG}");
}

/// Live text draws under every settled card, so the last card is no longer
/// the thing being written and has no claim on staying open.
#[test]
fn auto_hands_the_newest_slot_to_the_reply_being_written() {
    let mut panel = mode_panel(ViewMode::Auto, &[("t1", FILE_GREP_TOOL_NAME)]);
    assert!(!panel.card_closed("t1"), "{AUTO_TAIL_MSG}");

    panel.streaming_text.set_buffer("answering");
    rebuild(&mut panel);

    assert!(panel.card_closed("t1"), "{AUTO_STREAM_MSG}");
}

/// Auto may close what auto opened. A card the reader opened is a decision,
/// and the next tool call is not an argument against it.
#[test]
fn a_card_the_reader_opened_survives_the_next_one() {
    let mut panel = mode_panel(
        ViewMode::Auto,
        &[("t1", FILE_GREP_TOOL_NAME), ("t2", FILE_GREP_TOOL_NAME)],
    );
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(0, area), "{COMPACT_CLICK_MSG}");
    rebuild(&mut panel);
    assert!(!panel.card_closed("t1"), "{COMPACT_CLICK_MSG}");

    panel.tool_start(start("t3", FILE_GREP_TOOL_NAME));
    panel.tool_done(done("t3"));
    rebuild(&mut panel);

    assert!(!panel.card_closed("t1"), "{MANUAL_STICKY_MSG}");
    assert!(panel.card_closed("t2"), "{AUTO_HANDOFF_MSG}");
}

/// `show_thinking` is the reader saying they do not want reasoning at all. No
/// mode overrules that; the modes only choose among reasoning they may show,
/// which is why the same three modes answer differently once it is on.
#[test_case(ViewMode::Expanded, false, false; "expanded obeys the gate")]
#[test_case(ViewMode::Compact, false, false; "compact obeys the gate")]
#[test_case(ViewMode::Auto, false, false; "auto obeys the gate")]
#[test_case(ViewMode::Expanded, true, true; "expanded opens what it may")]
#[test_case(ViewMode::Compact, true, false; "compact closes what it may")]
#[test_case(ViewMode::Auto, true, true; "auto opens the newest")]
fn reasoning_answers_to_the_gate_before_the_mode(view: ViewMode, show_thinking: bool, open: bool) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.show_thinking = show_thinking;
    panel.set_view(view);
    panel.push(DisplayMessage::new(DisplayRole::Thinking, "hmm".into()));
    rebuild(&mut panel);

    assert_eq!(
        panel.reasoning_open(&panel.messages[0], 0),
        open,
        "{REASONING_GATE_MSG}"
    );
}

#[test]
fn auto_follows_the_newest_reasoning_too() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.show_thinking = true;
    panel.push(DisplayMessage::new(DisplayRole::Thinking, "first".into()));
    rebuild(&mut panel);
    assert!(
        panel.reasoning_open(&panel.messages[0], 0),
        "{REASONING_TAIL_MSG}"
    );

    panel.push(DisplayMessage::new(DisplayRole::Thinking, "second".into()));
    rebuild(&mut panel);

    assert!(
        !panel.reasoning_open(&panel.messages[0], 0),
        "{REASONING_TAIL_MSG}"
    );
    assert!(
        panel.reasoning_open(&panel.messages[1], 1),
        "{REASONING_TAIL_MSG}"
    );
}

const BATCH_TOOL: &str = "batch";
const BATCH_CHILD_BODY: &str = "child_body_line";
const EXPECT_BODY_HIDDEN: &str = "folding a child hides its body";
const EXPECT_OTHERS_KEPT: &str = "the other children are untouched";

fn batch_child(tool: &str, marker: &str) -> caudra_agent::BatchToolEntry {
    caudra_agent::BatchToolEntry {
        tool: tool.into(),
        summary: format!("{tool} ran"),
        status: caudra_agent::BatchToolStatus::Success,
        input: None,
        output: Some(ToolOutput::Plain(
            format!("{BATCH_CHILD_BODY}_{marker}").into(),
        )),
        annotation: None,
    }
}

fn panel_with_batch() -> MessagesPanel {
    let mut panel = panel_with_tools(&[("t1", BATCH_TOOL)]);
    panel.tool_done(ToolDoneEvent {
        tool: BATCH_TOOL.into(),
        output: ToolOutput::Batch {
            entries: vec![batch_child("read", "a"), batch_child("grep", "b")],
            text: String::new(),
        },
        ..done("t1")
    });
    render(&mut panel, 80, 24);
    panel
}

/// The row a child's summary was drawn on, as the panel counts rows.
fn batch_child_row(panel: &MessagesPanel, index: usize) -> u16 {
    let segment = panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some("t1"))
        .unwrap();
    let width = segment.chrome(80).content_width(80);
    let start = segment.chrome(80).content_start();
    (0..segment.content_height(80))
        .map(|row| start + row)
        .find(|row| segment.row_target_at(*row, width) == Some(RowTarget::BatchChild(index)))
        .expect("the child was drawn")
}

#[test]
fn clicking_a_batch_child_folds_only_that_child() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    let before = seg_text(&panel, "t1");
    assert!(before.contains("child_body_line_a"));
    assert!(before.contains("child_body_line_b"));

    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    render(&mut panel, 80, 24);
    let folded = seg_text(&panel, "t1");
    assert!(
        !folded.contains("child_body_line_a"),
        "{EXPECT_BODY_HIDDEN}"
    );
    assert!(folded.contains("child_body_line_b"), "{EXPECT_OTHERS_KEPT}");
    assert!(folded.contains("read ran"), "the summary stays");
}

#[test]
fn clicking_a_folded_batch_child_unfolds_it() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    render(&mut panel, 80, 24);

    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    render(&mut panel, 80, 24);
    assert!(seg_text(&panel, "t1").contains("child_body_line_a"));
}

/// A fold shifts every row below it, so the second child has to be found
/// where it now is rather than where it started.
#[test]
fn a_fold_above_does_not_move_the_click_off_the_child_below() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    render(&mut panel, 80, 24);

    assert!(panel.handle_click(batch_child_row(&panel, 1), area));
    render(&mut panel, 80, 24);
    let both = seg_text(&panel, "t1");
    assert!(!both.contains("child_body_line_a"), "{EXPECT_BODY_HIDDEN}");
    assert!(!both.contains("child_body_line_b"), "{EXPECT_BODY_HIDDEN}");
}

/// A batch child is a control, so the pointer has to say so before the press.
#[test]
fn hovering_a_batch_child_marks_that_row() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    let row = batch_child_row(&panel, 1);
    panel.update_hover(row, area.x, area, false);
    let segment = panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some("t1"))
        .unwrap();
    let line = segment
        .source_line_at(row, segment.chrome(80).content_width(80))
        .unwrap();
    assert_eq!(
        panel.hover,
        Some(HoverTarget::Tool {
            id: "t1".into(),
            feedback: HoverFeedback::Row(line),
        })
    );
}

const EXPECT_FILTERED: &str = "a card the reader never switched shows the filtered view";

/// The raw/filtered switch names a tool id from the session that is going
/// away. The panel outlives the session, so carrying the choice over opens
/// whatever inherits the id in a view nobody asked for.
#[test]
fn loading_a_session_forgets_the_raw_view() {
    let mut panel = panel_with_tools(&[("t1", SHELL_TOOL_NAME)]);
    panel.tool_done(shell_done("t1", true));
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.toggle_expansion_at(shell_toggle_row(&panel), area));
    render(&mut panel, 80, 24);
    assert!(seg_text(&panel, "t1").contains("raw_8"));

    panel.load_messages(Vec::new());
    panel.tool_start(start("t1", SHELL_TOOL_NAME));
    panel.tool_done(shell_done("t1", true));
    render(&mut panel, 80, 24);
    let text = seg_text(&panel, "t1");
    assert!(text.contains("model_8"), "{EXPECT_FILTERED}");
    assert!(!text.contains("raw_8"), "{EXPECT_FILTERED}");
}

/// A fold names a tool id from the session that is going away, so carrying it
/// into the next one would fold whatever inherits the id.
#[test]
fn loading_a_session_forgets_the_folds() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    assert!(!panel.batch_folds.is_empty());

    panel.load_messages(Vec::new());
    assert!(panel.batch_folds.is_empty());
}

/// Folding is a choice about the body, like the raw/filtered switch, so the
/// reset a mode change performs on every disclosure must leave it alone.
#[test]
fn changing_mode_keeps_a_batch_child_folded() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    render(&mut panel, 80, 24);

    panel.set_view(ViewMode::Compact);
    render(&mut panel, 80, 24);
    panel.set_view(ViewMode::Expanded);
    render(&mut panel, 80, 24);
    assert!(!seg_text(&panel, "t1").contains("child_body_line_a"));
    assert!(seg_text(&panel, "t1").contains("child_body_line_b"));
}
