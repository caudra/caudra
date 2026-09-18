use super::segment;
use super::*;
use crate::animation::test_clock::FrozenSpinner;
use crate::chat::{DONE_TEXT, ERROR_TEXT};
use crate::components::prompt_progress::PROMPT_PROGRESS_LABEL;
use crate::components::tool_display::{FOLLOWING, NOTICE_PREFIX, PAUSED};
use crate::repaint::expect::{OWED, QUIET};
use crate::selection::{Selection, SelectionZone};
use caudra_agent::tools::{
    BATCH_TOOL_NAME, FILE_APPLY_PATCH_TOOL_NAME, FILE_EDIT_TOOL_NAME, FILE_GLOB_TOOL_NAME,
    FILE_GREP_TOOL_NAME, FILE_INDEX_TOOL_NAME, FILE_READ_TOOL_NAME, FILE_WRITE_TOOL_NAME,
    IMAGE_GENERATE_TOOL_NAME, MEMORY_TOOL_NAME, PYTHON_EXECUTION_TOOL_NAME, QUESTION_TOOL_NAME,
    SHELL_TOOL_NAME, TASK_TOOL_NAME, TODOWRITE_TOOL_NAME, TOOL_OUTPUT_TOOL_NAME, ToolEffect,
    VIEW_IMAGE_TOOL_NAME,
};
use caudra_agent::{
    CodeGraphRow, GrepFileEntry, GrepMatchGroup, NO_FILES_FOUND, SearchCap, ShellFilterInfo,
    ShellOutput, SnapshotLine, SnapshotSpan, SpanStyle, SubagentActivity, SubagentProgress,
    ToolAccounting, ToolInput, ToolOutput,
};
use caudra_workbench::scroll::SCROLLBAR_THUMB;
use ratatui::backend::TestBackend;
use ratatui::style::Modifier;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use test_case::test_case;
use unicode_width::UnicodeWidthStr;

const SPINNER_GLYPHS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
/// A working directory nothing resolves under, for the hover tests that are
/// not about mentions.
const NO_PROJECT: &str = "/caudra-no-such-project";
const MENTION_PATH: &str = "src/lib.rs";
const MENTION: &str = "@src/lib.rs";
const MENTION_PROSE: &str = "look at @src/lib.rs please";
const MENTION_MISSED: &str = "the pointer sat on a mention the panel did not resolve";
const MENTION_CLAIMED: &str = "a message the reader did not write answered with a mention";
const MENTION_MARKED_GLYPHS: &str = "a hovered mention repainted the message around it";
/// A read-only tool the `always_collapsed` default deliberately leaves out, so
/// a card built on it answers the view mode rather than the reader's collapse
/// list. It shares `file_grep`'s row budget, so a test that moved off grep to
/// keep testing disclosure rests at the same number of rows it always did.
const CODE_MAP_TOOL_NAME: &str = "code_map";
const TOOL_ID: &str = "t1";
const WORKFLOW_TOOL_ID: &str = "workflow:run-1";
const WORKFLOW_RUN_ID: &str = "run-1";
const WORKFLOW_RUN_NAME: &str = "deep-research-run-1";
const WORKFLOW_NAME: &str = "deep-research";
const WORKFLOW_SCRATCH: &str = "/state/workflow_scratch/run-1/report.md";
const CARD_IS_ONE_TARGET: &str =
    "a press anywhere on a workflow card opens its run, so the card marks itself as one target";
const SCRATCH_ROW_IS_ITS_OWN: &str =
    "the row naming the scratch file opens the file, so it marks only itself";
const INJECTED_BODY: &str =
    "<system-reminder>\n# Environment\n\n- Date: 2026-09-10\n</system-reminder>";
const INJECTED_HEADING: &str = "Environment";
const INJECTED_DETAIL: &str = "2026-09-10";
const REASONING_BODY: &str = "weighing the options";
const EAGER_SUMMARY: &str = "executed arguments";
const EAGER_BODY: &str = "execution output";
const EAGER_ANNOTATION: &str = "ranking 12 files";
const EAGER_CHILD_ID: &str = "t1:0";

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
        | FILE_INDEX_TOOL_NAME
        | CODE_MAP_TOOL_NAME
        | VIEW_IMAGE_TOOL_NAME
        | TOOL_OUTPUT_TOOL_NAME => ToolEffect::ReadOnly,
        BATCH_TOOL_NAME | TASK_TOOL_NAME => ToolEffect::Orchestrator,
        PYTHON_EXECUTION_TOOL_NAME | TODOWRITE_TOOL_NAME | QUESTION_TOOL_NAME => {
            ToolEffect::Isolated
        }
        SHELL_TOOL_NAME
        | FILE_WRITE_TOOL_NAME
        | FILE_EDIT_TOOL_NAME
        | FILE_APPLY_PATCH_TOOL_NAME
        | IMAGE_GENERATE_TOOL_NAME
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

#[test]
fn message_action_handles_map_every_source_kind() {
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

    let terminal = render_actions(&mut panel, 80, 24);
    let area = terminal.backend().buffer().area;
    assert!(panel.message_action_at(0, 0).is_none());
    assert!(panel.message_action_at(1, 0).is_some());
    assert!(panel.message_action_at(1, 1).is_some());
    assert!(panel.message_action_at(1, 2).is_some());
    assert!(panel.message_action_at(1, 3).is_none());
    for source in sources {
        assert!((area.y..area.bottom()).any(|row| {
            (area.x..area.right()).any(|col| {
                panel
                    .message_action_at(row, col)
                    .is_some_and(|target| target.source() == source)
            })
        }));
    }
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.symbol() == render::MESSAGE_ACTION_GLYPH)
            .count(),
        sources.len()
    );
}

#[test]
fn message_action_handles_are_disabled_without_main_chat_access() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let mut message = DisplayMessage::new(DisplayRole::Assistant, "reply".into());
    message.source = Some(DisplaySource::AssistantText(
        caudra_storage::id::CaudraId::generate(),
    ));
    panel.push(message);

    let terminal = render(&mut panel, 80, 24);
    let area = terminal.backend().buffer().area;

    assert!(!(area.y..area.bottom()).any(|row| {
        (area.x..area.right()).any(|col| panel.message_action_at(row, col).is_some())
    }));
    assert!(!buffer_text(&terminal).contains(render::MESSAGE_ACTION_GLYPH));
}

#[test]
fn message_action_hover_reverses_only_the_glyph() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let mut message = DisplayMessage::new(DisplayRole::Assistant, "reply".into());
    message.source = Some(DisplaySource::AssistantText(
        caudra_storage::id::CaudraId::generate(),
    ));
    panel.push(message);
    let terminal = render_actions(&mut panel, 80, 24);
    let area = terminal.backend().buffer().area;
    let (row, col) = (area.y..area.bottom())
        .find_map(|row| {
            (area.x..area.right())
                .find(|&col| panel.message_action_at(row, col).is_some())
                .map(|col| (row, col))
        })
        .unwrap();

    panel.update_hover(row, col, area, false, Path::new(NO_PROJECT));
    let terminal = render_actions(&mut panel, 80, 24);
    let glyph = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .find(|cell| cell.symbol() == render::MESSAGE_ACTION_GLYPH)
        .unwrap();

    assert!(glyph.modifier.contains(Modifier::REVERSED));
    assert!(
        !style_of(&terminal, "reply")
            .add_modifier
            .contains(Modifier::REVERSED)
    );
}

#[test]
fn source_less_and_streaming_messages_have_no_action_handle() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::Assistant, "cached".into()));
    panel.text_delta("streaming");

    let terminal = render_actions(&mut panel, 80, 24);
    let area = terminal.backend().buffer().area;

    assert!(!(area.y..area.bottom()).any(|row| {
        (area.x..area.right()).any(|col| panel.message_action_at(row, col).is_some())
    }));
    assert!(!buffer_text(&terminal).contains(render::MESSAGE_ACTION_GLYPH));
}

#[test]
fn clipped_message_keeps_its_action_handle_visible() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let mut message = DisplayMessage::new(DisplayRole::Assistant, "line\n".repeat(20));
    message.source = Some(DisplaySource::AssistantText(
        caudra_storage::id::CaudraId::generate(),
    ));
    panel.push(message);
    render_actions(&mut panel, 40, 5);
    panel.set_scroll_top(5);

    let terminal = render_actions(&mut panel, 40, 5);
    let area = terminal.backend().buffer().area;

    assert_eq!(
        terminal.backend().buffer().cell((1, 0)).unwrap().symbol(),
        render::MESSAGE_ACTION_GLYPH
    );
    assert!(panel.message_action_at(area.y, area.x).is_some());
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        stdout_redraws_collapsed: 0,
        stderr_redraws_collapsed: 0,
        filter: filtered.then(|| ShellFilterInfo {
            stages: vec!["cargo".into()],
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        capped: None,
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    });
    let text = &panel.messages[0].text;
    assert!(!text.contains('\n'), "grep body should not be in msg.text");
    assert!(panel.messages[0].tool_output.is_some());
}

/// "No files found" alone would be a claim the search never established, so
/// every place the miss is stated has to carry how far the search got with it.
/// Grep never opens on its own, which leaves the one row the reader is given
/// answering for the card until they ask for the body.
#[test]
fn a_capped_grep_that_matched_nothing_qualifies_the_absence() {
    const CAP_ANNOTATION: &str = "capped, 40/900 searched";
    const CAP_DETAIL: &str = "searched 40 of 900 files; more matches may exist";
    const CARD_OPENS_ON_ASK: &str = "an always-collapsed card still opens when the reader asks";
    const MISS_RECORDED: &str = "the empty answer is what the card has to qualify, so it is still \
        the text the card was built from";

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", FILE_GREP_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: FILE_GREP_TOOL_NAME.into(),
        output: ToolOutput::GrepResult {
            entries: Vec::new(),
            capped: Some(SearchCap {
                files_scanned: 40,
                files_listed: 900,
            }),
        },
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    });
    render(&mut panel, 80, 24);
    let collapsed = seg_text(&panel, "t1");
    assert!(panel.toggle_expansion("t1"), "{CARD_OPENS_ON_ASK}");
    render(&mut panel, 80, 24);
    let opened = seg_text(&panel, "t1");

    assert!(
        panel.messages[0].text.contains(NO_FILES_FOUND),
        "{MISS_RECORDED}: {:?}",
        panel.messages[0].text
    );
    assert!(collapsed.contains(CAP_ANNOTATION), "{collapsed}");
    assert!(opened.contains(CAP_DETAIL), "{opened}");
    assert!(
        panel.messages[0]
            .annotation
            .as_deref()
            .is_some_and(|annotation| annotation.contains(CAP_ANNOTATION)),
        "{:?}",
        panel.messages[0].annotation
    );
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
            panel.view(f, f.area(), has_selection, false);
        })
        .unwrap();
    terminal
}

fn render_actions(
    panel: &mut MessagesPanel,
    width: u16,
    height: u16,
) -> ratatui::Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| panel.view(frame, frame.area(), false, true))
        .unwrap();
    terminal
}

fn render(panel: &mut MessagesPanel, width: u16, height: u16) -> ratatui::Terminal<TestBackend> {
    render_sel(panel, width, height, false)
}

fn rebuild(panel: &mut MessagesPanel) {
    render(panel, 80, 24);
}

fn workflow_card(
    status: caudra_workflow::RunStatus,
    scratch: Option<&str>,
) -> caudra_agent::types::WorkflowRunCard {
    caudra_agent::types::WorkflowRunCard {
        run_id: WORKFLOW_RUN_ID.into(),
        display_name: WORKFLOW_RUN_NAME.into(),
        workflow_name: WORKFLOW_NAME.into(),
        status,
        phase: None,
        phases: Vec::new(),
        phase_history: Vec::new(),
        agent_budget: 8,
        usage: caudra_workflow::RunUsage::default(),
        roster: Vec::new(),
        logs: Vec::new(),
        result_preview: None,
        scratch_path: scratch.map(str::to_owned),
        pause_message: None,
        error: None,
        created_at: 0,
        updated_at: 0,
    }
}

fn panel_with_a_workflow_card(
    status: caudra_workflow::RunStatus,
    scratch: Option<&str>,
) -> MessagesPanel {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let mut message = DisplayMessage::new(
        DisplayRole::Tool(Box::new(ToolRole {
            id: WORKFLOW_TOOL_ID.into(),
            effect: ToolEffect::Unknown,
            status: ToolStatus::InProgress,
            name: WORKFLOW_NAME.into(),
        })),
        String::new(),
    );
    message.tool_output = Some(Arc::new(ToolOutput::WorkflowRun(Box::new(workflow_card(
        status, scratch,
    )))));
    panel.push(message);
    panel
}

/// The card obeys none of the expand and collapse rules the other tool cards
/// hover by, so the only thing that can answer for it is the click's own
/// function. Every row it would act on has to mark itself as such.
#[test_case(caudra_workflow::RunStatus::Active, None, false ; "a_live_run")]
#[test_case(caudra_workflow::RunStatus::Completed, Some(WORKFLOW_SCRATCH), true ; "a_run_that_wrote_a_report")]
fn a_workflow_card_marks_exactly_what_a_press_would_do(
    status: caudra_workflow::RunStatus,
    scratch: Option<&str>,
    expect_scratch: bool,
) {
    let mut panel = panel_with_a_workflow_card(status, scratch);
    let terminal = render(&mut panel, 80, 24);
    let area = terminal.backend().buffer().area;
    let mut marked_run = false;
    let mut marked_scratch = false;

    for row in area.y..area.bottom() {
        let Some(hit) = panel.workflow_hit_at(row, area) else {
            continue;
        };
        panel.update_hover(row, area.x, area, false, Path::new(NO_PROJECT));
        let feedback = match &panel.hover {
            Some(HoverTarget::Tool { id, feedback }) if id == WORKFLOW_TOOL_ID => *feedback,
            other => panic!("{CARD_IS_ONE_TARGET}: row {row} hovered {other:?}"),
        };
        match hit {
            CardHit::Run(run_id) => {
                assert_eq!(run_id, WORKFLOW_RUN_ID);
                assert_eq!(feedback, HoverFeedback::Chrome, "{CARD_IS_ONE_TARGET}");
                marked_run = true;
            }
            CardHit::ScratchFile(_) => {
                assert!(
                    matches!(feedback, HoverFeedback::Row(_)),
                    "{SCRATCH_ROW_IS_ITS_OWN}: {feedback:?}"
                );
                marked_scratch = true;
            }
        }
    }

    assert!(marked_run, "{CARD_IS_ONE_TARGET}");
    assert_eq!(marked_scratch, expect_scratch, "{SCRATCH_ROW_IS_ITS_OWN}");
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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

    // Live output is drawn by the frame, so the card is read after one. The
    // settled output below needs no frame: a terminal event draws its own card.
    panel.tool_output("t1", "streaming");
    rebuild(&mut panel);
    assert!(seg_text(&panel, "t1").contains("streaming"));

    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: SHELL_TOOL_NAME.into(),
        output: ToolOutput::Plain("done".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        "the splash keeps animating while it is the only thing drawn"
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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

/// A long session runs well past 65535 rows. Both the document height and the
/// scroll offset used to clamp there, which froze the transcript part way and
/// made the scrollbar report a document that had stopped growing.
#[test]
fn a_transcript_past_u16_rows_reaches_both_ends() {
    const ROWS: u32 = u16::MAX as u32 + 500;
    const HEIGHT: u16 = 24;
    const REACHES_TOP: &str = "a document past 65535 rows must still scroll to its first row";
    const REACHES_BOTTOM: &str = "a document past 65535 rows must still scroll to its last row";
    const FULL_HEIGHT: &str = "the document height must not clamp at 65535 rows";

    // Segments straight into the cache: the point is the row arithmetic, and
    // driving 70000 rows through the markdown painter would test that instead.
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::Assistant, "seed".into()));
    render(&mut panel, 80, HEIGHT);
    while panel.last_total_lines < ROWS {
        panel.cache.push(segment::Segment::with_lines(
            vec![Line::raw("x"); HEIGHT as usize],
            String::new(),
            None,
        ));
        panel.last_total_lines += u32::from(HEIGHT);
    }
    render(&mut panel, 80, HEIGHT);

    assert!(panel.last_total_lines >= ROWS, "{FULL_HEIGHT}");
    assert_eq!(panel.win_view().line_count, panel.last_total_lines);
    let bottom = panel.max_scroll();
    assert!(bottom > u32::from(u16::MAX), "{FULL_HEIGHT}");
    assert_eq!(panel.scroll_top(), bottom, "{REACHES_BOTTOM}");

    panel.scroll_to_top();
    render(&mut panel, 80, HEIGHT);
    assert_eq!(panel.scroll_top(), 0, "{REACHES_TOP}");

    panel.enable_auto_scroll();
    render(&mut panel, 80, HEIGHT);
    assert_eq!(panel.scroll_top(), bottom, "{REACHES_BOTTOM}");
}

#[test]
fn win_view_clamps_a_restored_offset_past_the_end() {
    const LINES: u32 = 15;
    const HEIGHT: u16 = 10;

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel
        .streaming_text
        .set_buffer(&"a\n".repeat(LINES as usize));
    render(&mut panel, 80, HEIGHT);

    panel.restore_scroll(u32::MAX, true);

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
fn a_preview_fills_the_pending_header_until_the_real_summary_lands() {
    const PREVIEW: &str = "src/app.rs";
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending("t1".into(), "bash");
    assert_eq!(panel.messages[0].text, "");

    panel.tool_input_preview("t1", Some(PREVIEW.into()), None);
    assert_eq!(panel.messages.len(), 1);
    assert_eq!(panel.messages[0].text, PREVIEW);

    panel.tool_start(start("t1", SHELL_TOOL_NAME));
    assert_eq!(panel.messages.len(), 1);
    assert_eq!(panel.messages[0].text, "t1");
}

/// How far a body has got, as the header reports it while the call streams.
const STREAMED_SIZE: &str = "20+ lines";

#[test]
fn a_streamed_size_lands_in_the_annotation_without_touching_the_header() {
    const HEADER: &str = "src/app.rs";
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending("t1".into(), "file_write");
    panel.tool_input_preview("t1", Some(HEADER.into()), None);

    panel.tool_input_preview("t1", None, Some(STREAMED_SIZE.into()));
    assert_eq!(panel.messages[0].text, HEADER);
    assert_eq!(panel.messages[0].annotation.as_deref(), Some(STREAMED_SIZE));
}

#[test]
fn a_preview_for_an_unknown_call_is_ignored() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_input_preview("t1", Some("src/app.rs".into()), None);
    assert!(panel.messages.is_empty());
}

fn streaming_write(panel: &mut MessagesPanel, fragments: &[&str]) {
    panel.tool_pending(TOOL_ID.into(), FILE_WRITE_TOOL_NAME);
    for fragment in fragments {
        panel.tool_input_body(TOOL_ID, Some((*fragment).into()));
    }
}

/// A write announces its path before any of its body, so the card knows which
/// of the two ways to draw the file before there is anything to draw.
#[test]
fn a_streamed_write_to_a_markdown_file_draws_the_document() {
    const PATH: &str = "plan.md";
    const HEADING: &str = "# Title\n";
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending(TOOL_ID.into(), FILE_WRITE_TOOL_NAME);
    panel.tool_input_preview(TOOL_ID, Some(PATH.into()), None);
    panel.tool_input_body(TOOL_ID, Some(HEADING.into()));

    let shown = buffer_text(&render(&mut panel, 80, 24));
    assert!(shown.contains("Title"), "{shown}");
    assert!(!shown.contains(HEADING.trim_end()), "{shown}");
}

/// A note arrives under a header that names a sub-command rather than a path,
/// so what draws it as a document is the tool, and what puts its verb in the
/// present tense is the row.
#[test]
fn a_streamed_note_draws_the_document_under_a_conjugated_header() {
    const HEADER: &str = "write render-loop-perf.md";
    const CONJUGATED: &str = "writing render-loop-perf.md";
    const HEADING: &str = "# Title\n";
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending(TOOL_ID.into(), MEMORY_TOOL_NAME);
    panel.tool_input_preview(TOOL_ID, Some(HEADER.into()), None);
    panel.tool_input_body(TOOL_ID, Some(HEADING.into()));

    let shown = buffer_text(&render(&mut panel, 80, 24));
    assert!(shown.contains(CONJUGATED), "{shown}");
    assert!(shown.contains("Title"), "{shown}");
    assert!(!shown.contains(HEADING.trim_end()), "{shown}");
}

#[test]
fn a_streamed_write_grows_in_the_card() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    streaming_write(&mut panel, &["fn one() {}\n"]);
    let first = buffer_text(&render(&mut panel, 80, 24));
    assert!(first.contains("fn one() {}"), "{first}");

    panel.tool_input_body(TOOL_ID, Some("fn two() {}\n".into()));
    let grown = buffer_text(&render(&mut panel, 80, 24));
    assert!(grown.contains("fn two() {}"), "{grown}");
}

/// The streamed body is a stand-in for output the call has not produced yet,
/// so the real one has to displace it rather than render underneath it.
#[test]
fn a_streamed_body_gives_way_to_the_real_output() {
    const WRITTEN: &str = "fn done() {}";
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    streaming_write(&mut panel, &["fn streaming() {}\n"]);
    rebuild(&mut panel);

    let mut event = start(TOOL_ID, FILE_WRITE_TOOL_NAME);
    event.output = Some(ToolOutput::WriteCode {
        path: "a.rs".into(),
        byte_count: WRITTEN.len(),
        lines: vec![WRITTEN.into()],
    });
    panel.tool_start(event);

    assert_eq!(panel.messages[0].live_body, None);
    let shown = buffer_text(&render(&mut panel, 80, 24));
    assert!(shown.contains(WRITTEN), "{shown}");
    assert!(!shown.contains("fn streaming"), "{shown}");
}

/// A diff exists nowhere but the output the call has not produced yet, so
/// these two narrate the wait with the header alone.
#[test_case(FILE_EDIT_TOOL_NAME ; "edit")]
#[test_case(FILE_APPLY_PATCH_TOOL_NAME ; "patch")]
fn a_change_that_is_only_a_diff_streams_its_size_and_no_body(tool: &'static str) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending(TOOL_ID.into(), tool);
    panel.tool_input_preview(TOOL_ID, None, Some(STREAMED_SIZE.into()));

    assert_eq!(panel.messages[0].live_body, None);
    assert!(buffer_text(&render(&mut panel, 80, 24)).contains(STREAMED_SIZE));
}

#[test]
fn a_body_for_an_unknown_call_is_ignored() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_input_body(TOOL_ID, Some("orphan".into()));
    assert!(panel.messages.is_empty());
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

fn extract_entire_document(panel: &mut MessagesPanel) -> String {
    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 40;

    render(panel, WIDTH, HEIGHT);
    let area = Rect::new(0, 0, WIDTH, HEIGHT);
    let selection = make_sel(
        area,
        (0, 0),
        (panel.last_total_lines.saturating_sub(1), WIDTH - 1),
    );
    panel.extract_selection_text(&selection, area)
}

#[test]
fn selection_across_messages_becomes_a_markdown_document() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        "Please use **care**.".into(),
    ));
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "# Result\n\nUsed `care`.".into(),
    ));

    assert_eq!(
        extract_entire_document(&mut panel),
        "## User\n\nPlease use **care**.\n\n---\n\n## Assistant\n\n# Result\n\nUsed `care`."
    );
}

#[test]
fn open_thinking_copies_its_visible_markdown_body() {
    const BODY: &str = "Check **both** branches.";

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, "Investigate".into()));
    let mut thought = DisplayMessage::new(
        DisplayRole::Thinking,
        format!("**Tracing selection**\n\n{BODY}"),
    );
    thought.body_open = Some(true);
    panel.push(thought);
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "Finished".into(),
    ));

    let copied = extract_entire_document(&mut panel);

    assert!(
        copied.contains("## Thinking: Tracing selection"),
        "{copied}"
    );
    assert!(copied.contains("_View: open_"), "{copied}");
    assert!(copied.contains(BODY), "{copied}");
    assert!(!copied.contains("**Tracing selection**"), "{copied}");
}

#[test]
fn collapsed_thinking_copies_its_summary_without_hidden_body() {
    const HIDDEN: &str = "hidden reasoning must stay hidden";

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, "Investigate".into()));
    let mut thought = DisplayMessage::new(
        DisplayRole::Thinking,
        format!("**Tracing selection**\n\n{HIDDEN}"),
    );
    thought.body_open = Some(false);
    panel.push(thought);
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "Finished".into(),
    ));

    let copied = extract_entire_document(&mut panel);

    assert!(
        copied.contains("## Thinking: Tracing selection"),
        "{copied}"
    );
    assert!(copied.contains("_View: collapsed_"), "{copied}");
    assert!(!copied.contains(HIDDEN), "{copied}");
}

#[test]
fn settled_thinking_does_not_borrow_the_streaming_duration() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            typewriter_ms_per_char: 0,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.push(DisplayMessage::new(DisplayRole::User, "Investigate".into()));
    let mut settled = DisplayMessage::new(DisplayRole::Thinking, "**Old trace**".into());
    settled.body_open = Some(false);
    panel.push(settled);
    panel.thinking_started = Some(Instant::now() - Duration::from_millis(100));
    panel.streaming_thinking.set_buffer("**Live trace**");

    let copied = extract_entire_document(&mut panel);

    assert_eq!(copied.matches("Duration:").count(), 1, "{copied}");
    assert!(!copied.contains("ms"), "{copied}");
}

#[test]
fn selected_open_thinking_header_still_marks_a_card_boundary() {
    const HIDDEN: &str = "body outside the selection";

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, "Investigate".into()));
    let mut thought = DisplayMessage::new(
        DisplayRole::Thinking,
        format!("**Tracing selection**\n\n{HIDDEN}"),
    );
    thought.body_open = Some(true);
    panel.push(thought);
    render(&mut panel, 80, 20);
    let area = Rect::new(0, 0, 80, 20);
    let heights = panel.segment_heights();
    let thinking_header =
        u32::from(heights[0] + panel.cache.segments()[1].chrome(79).content_start());
    let selection = make_sel(area, (0, 0), (thinking_header, 79));

    let copied = panel.extract_selection_text(&selection, area);

    assert!(copied.contains("## User"), "{copied}");
    assert!(
        copied.contains("## Thinking: Tracing selection"),
        "{copied}"
    );
    assert!(!copied.contains(HIDDEN), "{copied}");
}

fn panel_with_copyable_tool(tool: &'static str, view: ViewMode) -> MessagesPanel {
    let mut panel = panel_with_tools(&[("t1", tool)]);
    panel.tool_done(ToolDoneEvent {
        tool: tool.into(),
        output: ToolOutput::Plain("hidden tool output".into()),
        ..done("t1")
    });
    panel.set_view(view);
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "Finished".into(),
    ));
    panel
}

#[test]
fn collapsed_tool_copies_only_its_visible_header() {
    let mut panel = panel_with_copyable_tool(FILE_READ_TOOL_NAME, ViewMode::Compact);

    let copied = extract_entire_document(&mut panel);

    assert!(copied.contains("## Tool: `file_read`"), "{copied}");
    assert!(
        copied.contains("Status: success | View: collapsed"),
        "{copied}"
    );
    assert!(!copied.contains("hidden tool output"), "{copied}");
}

#[test]
fn open_tool_copies_its_visible_body() {
    let mut panel = panel_with_copyable_tool(CODE_MAP_TOOL_NAME, ViewMode::Expanded);

    let copied = extract_entire_document(&mut panel);

    assert!(copied.contains("## Tool: `code_map`"), "{copied}");
    assert!(copied.contains("Status: success | View: open"), "{copied}");
    assert!(copied.contains("hidden tool output"), "{copied}");
}

#[test]
fn compact_instruction_copy_keeps_its_semantic_label() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Compact);
    panel.tool_start(start("t1", "read"));
    panel.tool_done(ToolDoneEvent {
        id: "t1".into(),
        tool: "read".into(),
        output: read_code_with_instructions(instruction_blocks()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    });
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "Finished".into(),
    ));

    let copied = extract_entire_document(&mut panel);

    assert!(copied.contains("## Instructions: `read`"), "{copied}");
}

#[test]
fn plan_selection_keeps_body_markdown_and_drops_visual_footer() {
    const PLAN: &str = "# Plan\n\nKeep **source Markdown**.";

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::plan(PLAN.into(), "/tmp/plan.md".into()));
    render(&mut panel, 80, 20);
    let area = Rect::new(0, 0, 80, 20);
    let rows = panel.segment_heights()[0];
    let selection = make_sel(area, (0, 0), (u32::from(rows - 1), 79));

    assert_eq!(panel.extract_selection_text(&selection, area), PLAN);
}

#[test]
fn streaming_assistant_participates_in_cross_message_copy() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            typewriter_ms_per_char: 0,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.push(DisplayMessage::new(DisplayRole::User, "Question".into()));
    panel
        .streaming_text
        .set_buffer("# Live\n\nStill **writing**.");

    assert_eq!(
        extract_entire_document(&mut panel),
        "## User\n\nQuestion\n\n---\n\n## Assistant\n\n# Live\n\nStill **writing**."
    );
}

#[test]
fn collapsed_streaming_thinking_does_not_leak_its_body() {
    const HIDDEN: &str = "live hidden reasoning";

    let mut panel = MessagesPanel::new(
        UiConfig {
            typewriter_ms_per_char: 0,
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.push(DisplayMessage::new(DisplayRole::User, "Question".into()));
    panel
        .streaming_thinking
        .set_buffer(&format!("**Live trace**\n\n{HIDDEN}"));

    let copied = extract_entire_document(&mut panel);

    assert!(copied.contains("## Thinking: Live trace"), "{copied}");
    assert!(copied.contains("View: collapsed"), "{copied}");
    assert!(!copied.contains(HIDDEN), "{copied}");
}

#[test]
fn collapsed_streaming_thinking_uses_the_displayed_buffer_title() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.push(DisplayMessage::new(DisplayRole::User, "Question".into()));
    panel.thinking_delta("**Buffered trace**\n\nnot yet revealed");

    let copied = extract_entire_document(&mut panel);

    assert!(copied.contains("## Thinking: Buffered trace"), "{copied}");
    assert!(!copied.contains("not yet revealed"), "{copied}");
}

#[test]
fn open_streaming_thinking_copies_raw_visible_markdown() {
    const BODY: &str = "Still **checking**.";

    let mut panel = MessagesPanel::new(
        UiConfig {
            typewriter_ms_per_char: 0,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.push(DisplayMessage::new(DisplayRole::User, "Question".into()));
    panel.streaming_reasoning_open = Some(true);
    panel
        .streaming_thinking
        .set_buffer(&format!("**Live trace**\n\n{BODY}"));

    let copied = extract_entire_document(&mut panel);

    assert!(copied.contains("## Thinking: Live trace"), "{copied}");
    assert!(copied.contains("View: open"), "{copied}");
    assert!(copied.contains(BODY), "{copied}");
    assert!(!copied.contains("**Live trace**"), "{copied}");
}

#[test]
fn title_only_open_streaming_thinking_still_copies_its_header() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            typewriter_ms_per_char: 0,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.streaming_reasoning_open = Some(true);
    panel.streaming_thinking.set_buffer("**Solo trace**");
    render(&mut panel, 80, 20);
    let area = Rect::new(0, 0, 80, 20);
    let selection = make_sel(area, (0, 0), (panel.last_total_lines.saturating_sub(1), 79));

    assert_eq!(
        panel.extract_selection_text(&selection, area),
        "Thinking: Solo trace"
    );
}

#[test]
fn cross_message_copy_does_not_restore_truncated_tool_output() {
    let mut panel = panel_with_long_tool(CODE_MAP_TOOL_NAME, 200);
    panel.set_view(ViewMode::Expanded);
    render(&mut panel, 80, 24);
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "Finished".into(),
    ));

    let copied = extract_entire_document(&mut panel);

    assert!(copied.contains("## Tool: `code_map`"), "{copied}");
    assert!(copied.contains("line 0"), "{copied}");
    assert!(!copied.contains("line 50"), "{copied}");
}

#[test]
fn tool_fence_outgrows_backticks_in_visible_content() {
    let fenced = fenced_text("before\n```\nafter", None);

    assert!(fenced.starts_with("````text\n"), "{fenced}");
    assert!(fenced.ends_with("\n````"), "{fenced}");
}

#[test]
fn rendered_markdown_fallback_keeps_table_cells_separate() {
    let rendered = rendered_markdown_text("| left | right |\n| --- | --- |\n| one | two |", 80);

    assert!(
        rendered
            .lines()
            .any(|line| line.contains("left") && line.contains('│') && line.contains("right")),
        "{rendered}"
    );
}

#[test]
fn incomplete_message_fence_cannot_swallow_the_next_card() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        "```rust\nfn unfinished() {}".into(),
    ));
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "Visible reply".into(),
    ));

    let copied = extract_entire_document(&mut panel);

    assert!(
        copied.contains("fn unfinished() {}\n```\n\n---\n\n## Assistant"),
        "{copied}"
    );
}

fn assert_generated_document_closed(copied: &str) {
    assert!(unclosed_markdown_block(copied).is_none(), "{copied}");
    assert!(unclosed_caudra_fenced_block(copied).is_none(), "{copied}");
    assert!(copied.contains("## Assistant\n\nVisible reply"), "{copied}");
}

#[test_case("$$\nx + y"; "dollars")]
#[test_case("\\[\nx + y"; "brackets")]
#[test_case("<div>\n$$\nx + y"; "math_inside_html")]
#[test_case("    $$\nx + y"; "indented_math")]
fn incomplete_message_math_cannot_swallow_the_next_card(body: &str) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, body.into()));
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "Visible reply".into(),
    ));

    let copied = extract_entire_document(&mut panel);

    assert_generated_document_closed(&copied);
}

#[test]
fn blank_line_html_context_does_not_hide_a_later_open_fence() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::User,
        "<div>\n```\n\ntext\n````".into(),
    ));
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "Visible reply".into(),
    ));

    let copied = extract_entire_document(&mut panel);

    assert_generated_document_closed(&copied);
}

#[test_case("```\nx\n````"; "caudra_fence_left_open")]
#[test_case("   ```\n````rust\nx"; "distinct_open_fences")]
fn divergent_fence_closures_leave_the_generated_document_closed(body: &str) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, body.into()));
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "Visible reply".into(),
    ));

    let copied = extract_entire_document(&mut panel);

    let rendered_body = rendered_markdown_text(body, 80);
    let rendered_document = rendered_markdown_text(&copied, 80);
    assert!(rendered_document.contains(&rendered_body), "{copied}");
    assert!(!copied.contains(" \\`"), "{copied}");
    assert_generated_document_closed(&copied);
}

#[test_case("<script>\nwindow.alert('x')"; "script")]
#[test_case("<script>\n```\nlooks like a fence"; "fence_inside_script")]
#[test_case("<!-- unfinished"; "comment")]
#[test_case("<pre>\nraw"; "preformatted")]
fn incomplete_raw_html_cannot_swallow_the_next_card(body: &str) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::User, body.into()));
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "Visible reply".into(),
    ));

    let copied = extract_entire_document(&mut panel);

    assert_generated_document_closed(&copied);
}

#[test]
fn inline_metadata_cannot_inject_markdown_blocks() {
    assert_eq!(
        markdown_inline("model\n## forged &copy;"),
        "model \\#\\# forged \\&copy;"
    );
    assert_eq!(markdown_code_span("`odd`"), "<code>`odd`</code>");
    assert_eq!(markdown_code_span("a&b "), "<code>a&amp;b </code>");
    assert_eq!(markdown_code_span("foo\n## forged"), "`foo ## forged`");
    assert!(unclosed_markdown_block("<![cdata[").is_none());
}

#[test]
fn markdown_block_scanner_matches_commonmark_boundaries() {
    assert!(matches!(
        unclosed_markdown_block("<script>\n</style>\n~~~"),
        Some(MarkdownBlock::Fence('~', 3))
    ));
    assert!(matches!(
        unclosed_markdown_block("```\nbody\n```\u{a0}"),
        Some(MarkdownBlock::Fence('`', 3))
    ));
    assert!(unclosed_markdown_block("~~~\rbody\r~~~").is_none());
    assert!(matches!(
        unclosed_markdown_block("<div>\n```\n\ntext\n````"),
        Some(MarkdownBlock::Fence('`', 4))
    ));
    assert!(matches!(
        unclosed_markdown_block("<widget data-label=\"a > b\">\n```\n\ntext\n~~~~"),
        Some(MarkdownBlock::Fence('~', 4))
    ));
    assert!(matches!(
        unclosed_markdown_block("</widget   >\n```\n\ntext\n~~~~"),
        Some(MarkdownBlock::Fence('~', 4))
    ));
    assert!(matches!(
        unclosed_markdown_block("<widget data-label=x   >\n```\n\ntext\n~~~~"),
        Some(MarkdownBlock::Fence('~', 4))
    ));
    assert!(matches!(
        unclosed_markdown_block("<widget !>\n```"),
        Some(MarkdownBlock::Fence('`', 3))
    ));
    assert!(matches!(
        unclosed_markdown_block("paragraph\n<widget>\n```"),
        Some(MarkdownBlock::Fence('`', 3))
    ));
    assert!(matches!(
        unclosed_caudra_fenced_block("<div>\n    $$\nx"),
        Some(MarkdownBlock::Math("$$"))
    ));
}

fn panel_with_long_tool(tool: &'static str, line_count: usize) -> MessagesPanel {
    let body = (0..line_count)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(ToolStartEvent {
        id: "t1".into(),
        effect: ToolEffect::Unknown,
        tool: tool.into(),
        summary: "cmd".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    });
    panel.tool_done(ToolDoneEvent {
        tool: tool.into(),
        output: ToolOutput::Plain(body.into()),
        ..done("t1")
    });
    render(&mut panel, 80, 24);
    panel
}

#[test]
fn toggle_expand_collapse_truncated_tool() {
    let mut panel = panel_with_long_tool(CODE_MAP_TOOL_NAME, 200);
    panel.set_view(ViewMode::Expanded);
    render(&mut panel, 80, 24);
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

/// The same body in the shape the tool really reports it.
///
/// A write that created a file settles into `WriteCode`, and that is the one
/// body exempt from the card's budget — a write reporting plain text is held
/// to its budget like anything else, so a fixture that reported one would be
/// testing a card no write can produce.
fn body_done(tool: &str, id: &str, lines: usize) -> ToolDoneEvent {
    if tool != FILE_WRITE_TOOL_NAME {
        return long_done(id, lines);
    }
    let body: Vec<String> = (0..lines).map(|i| format!("line {i}")).collect();
    ToolDoneEvent {
        output: ToolOutput::WriteCode {
            path: WRITTEN_FILE_PATH.into(),
            byte_count: body.iter().map(String::len).sum(),
            lines: body,
        },
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
    let mut panel = panel_with_tools(&[("t1", SHELL_TOOL_NAME)]);
    panel.set_view(ViewMode::Expanded);
    panel.tool_done(shell_done("t1", true));
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    if raw {
        assert!(panel.toggle_expansion_at(shell_toggle_row(&panel), area));
        render(&mut panel, 80, 24);
    }
    let chosen = seg_text(&panel, "t1").contains("raw_8");
    assert_eq!(chosen, raw, "the test must set up the view it checks");

    panel.set_view(ViewMode::Compact);
    render(&mut panel, 80, 24);
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

    panel.update_hover(row, area.x, area, false, Path::new(NO_PROJECT));

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
    let mut panel = panel_with_long_tool(CODE_MAP_TOOL_NAME, 200);
    let area = Rect::new(0, 0, 80, 24);
    let source = panel.messages[0].text.clone();
    let cached = panel.cache.find_by_tool_id("t1").and_then(|index| {
        panel
            .cache
            .get(index)
            .map(|segment| segment.lines().to_vec())
    });

    panel.update_hover(area.y, area.right(), area, false, Path::new(NO_PROJECT));
    assert!(panel.hover.is_none(), "outside columns are not hoverable");
    panel.update_hover(area.y, area.x, area, false, Path::new(NO_PROJECT));
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
    const MAP_LABEL: &str = "Mapped";

    let mut panel = panel_with_long_tool(CODE_MAP_TOOL_NAME, 200);
    panel.set_view(ViewMode::Expanded);
    let area = Rect::new(0, 0, 80, HEIGHT);
    render(&mut panel, area.width, area.height);
    assert!(panel.toggle_expansion("t1"));
    render(&mut panel, area.width, area.height);

    panel.update_hover(area.y, area.x, area, false, Path::new(NO_PROJECT));
    let terminal = render(&mut panel, area.width, area.height);
    let buffer = terminal.backend().buffer();
    let rail = buffer.cell((area.x, area.y)).unwrap().style();
    let header = style_of(&terminal, MAP_LABEL);

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

    panel.update_hover(area.y, area.x, area, false, Path::new(NO_PROJECT));
    assert!(
        panel.hover.is_none(),
        "ordinary Lua snapshot rows are excluded"
    );

    panel.update_hover(area.y, area.x, area, true, Path::new(NO_PROJECT));
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

    panel.update_hover(area.y, area.x, area, false, Path::new(NO_PROJECT));

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

    panel.update_hover(row, column, area, false, Path::new(NO_PROJECT));
    assert_eq!(panel.hovered_hint(), Some("https://example.com"));
    panel.update_hover(row, column + 4, area, false, Path::new(NO_PROJECT));
    assert_eq!(panel.hovered_hint(), None);
}

/// Places the pointer over the mention in a one-message transcript and reports
/// what the panel makes of it. The project is this crate, so `src/lib.rs`
/// resolves without a temporary directory.
fn mention_hover(role: DisplayRole, text: &str) -> (MessagesPanel, Rect, u16, u16) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(role, text.into()));
    panel.viewport_width = 80;
    panel.rebuild_line_cache();
    panel.set_scroll_top(0);
    let area = Rect::new(5, 7, 80, 5);
    let chrome = panel.cache.get(0).expect("a segment").chrome(80);
    let row = area.y + chrome.content_start();
    let column = area.x + chrome.left + text.find(MENTION).expect("a mention") as u16;
    (panel, area, row, column)
}

#[test]
fn a_mention_in_a_user_message_answers_the_pointer() {
    let (panel, area, row, column) = mention_hover(DisplayRole::User, MENTION_PROSE);

    let mention = panel.mention_at(row, column, area, Path::new(env!("CARGO_MANIFEST_DIR")));

    assert_eq!(
        mention.and_then(|mention| mention.local_path().map(std::path::Path::to_path_buf)),
        Some(std::path::PathBuf::from(MENTION_PATH)),
        "{MENTION_MISSED}"
    );
}

/// A path the model happens to spell with an `@` was never a request to open
/// anything.
#[test]
fn a_mention_the_model_wrote_is_left_alone() {
    let (panel, area, row, column) = mention_hover(DisplayRole::Assistant, MENTION_PROSE);

    let mention = panel.mention_at(row, column, area, Path::new(env!("CARGO_MANIFEST_DIR")));

    assert!(mention.is_none(), "{MENTION_CLAIMED}");
}

/// The status bar is the only sign a mention is there, so hovering one must
/// leave every glyph in the message alone.
#[test]
fn hovering_a_mention_marks_the_status_bar_and_no_glyph() {
    let (mut panel, area, row, column) = mention_hover(DisplayRole::User, MENTION_PROSE);

    panel.update_hover(
        row,
        column,
        area,
        false,
        Path::new(env!("CARGO_MANIFEST_DIR")),
    );

    assert_eq!(panel.hovered_hint(), Some(MENTION), "{MENTION_MISSED}");
    let segment = panel.cache.get(0).expect("a segment");
    assert!(
        panel.hover_feedback_for_segment(segment).is_none(),
        "{MENTION_MARKED_GLYPHS}"
    );
}

#[test]
fn extract_selection_copies_visible_content_only() {
    let panel = panel_with_long_tool(SHELL_TOOL_NAME, 200);
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
    let mut panel = panel_with_long_tool(SHELL_TOOL_NAME, 3);
    panel.set_view(ViewMode::Expanded);
    render(&mut panel, 80, 24);
    let area = Rect::new(0, 0, 80, 24);
    assert!(!panel.toggle_expansion_at(area.y, area));
}

fn panel_with_map_tool(row_count: usize) -> MessagesPanel {
    const HEADLINE: &str = "ranked symbols in .";
    const FOOTER: &str = "[3 files, 9 symbols, 4 edges]";

    let rows = (1..=row_count)
        .map(|i| CodeGraphRow {
            name: format!("symbol_{i}"),
            kind: "function".into(),
            path: "src/main.rs".into(),
            line_start: i,
            line_end: i + 1,
            inbound: Some(i),
            outbound: Some(1),
            hops: None,
            test_scope: false,
        })
        .collect();
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start("t1", CODE_MAP_TOOL_NAME));
    panel.tool_done(ToolDoneEvent {
        tool: CODE_MAP_TOOL_NAME.into(),
        output: ToolOutput::CodeGraph {
            headline: HEADLINE.into(),
            rows,
            source: None,
            footer: FOOTER.into(),
            state: None,
        },
        ..done("t1")
    });
    render(&mut panel, 80, 24);
    panel
}

/// A structured body discloses through the same cycle a plain one does, so the
/// card cannot grow a second way to be opened out of the renderer it happens
/// to use.
#[test]
fn toggle_expand_collapse_map_tool() {
    let mut panel = panel_with_map_tool(8);
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
fn instruction_segment_has_margin_but_no_own_action_handle() {
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    });
    rebuild(&mut panel);

    let inst_id = segment::instruction_id("t1");
    assert!(segment_has_top_margin(&panel, &inst_id));

    panel.messages[0].source = Some(DisplaySource::ToolCall {
        id: caudra_storage::id::CaudraId::generate(),
        result_id: None,
    });
    let terminal = render_actions(&mut panel, 80, 24);
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.symbol() == render::MESSAGE_ACTION_GLYPH)
            .count(),
        1
    );
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
    let mut panel = panel_with_long_tool(SHELL_TOOL_NAME, 200);
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
    panel.set_view(ViewMode::Expanded);
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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

    panel.update_hover(area.y, area.x, area, false, Path::new(NO_PROJECT));
    let terminal = render(&mut panel, area.width, area.height);

    assert!(matches!(panel.hover, Some(HoverTarget::CachedThinking(0))));
    assert!(buffer_text(&terminal).contains("Thought"));
}

/// Reasoning is the model narrating the turn, so it opens with the turn. An
/// injected message is reference material the harness sent on the user's
/// behalf, so it has to fold to its heading or it buries the conversation.
#[test]
fn an_injected_message_folds_to_its_heading_while_reasoning_opens() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: true,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.push(DisplayMessage::new(
        DisplayRole::Injected,
        INJECTED_BODY.into(),
    ));
    panel.thinking_delta(REASONING_BODY);
    panel.flush();

    let text = buffer_text(&render(&mut panel, 80, 24));

    assert!(
        text.contains(INJECTED_HEADING),
        "the injected row should title itself; got: {text}"
    );
    assert!(
        !text.contains(INJECTED_DETAIL),
        "the injected body must stay folded; got: {text}"
    );
    assert!(
        text.contains(REASONING_BODY),
        "reasoning must stay open; got: {text}"
    );
}

/// The heading is all the row shows, so a click is the only way to read what
/// was actually sent, and the only way back.
#[test]
fn clicking_an_injected_heading_opens_and_refolds_the_body() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Injected,
        INJECTED_BODY.into(),
    ));
    panel.flush();
    let area = Rect::new(0, 0, 80, 24);
    rebuild(&mut panel);
    let folded = panel.segment_heights()[0];

    assert!(panel.handle_click(0, area));
    assert!(buffer_text(&render(&mut panel, area.width, area.height)).contains(INJECTED_DETAIL));

    assert!(panel.handle_click(0, area));
    assert_eq!(panel.segment_heights()[0], folded);
}

#[test]
fn an_injected_heading_responds_to_hover() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Injected,
        INJECTED_BODY.into(),
    ));
    panel.flush();
    let area = Rect::new(0, 0, 80, 10);
    render(&mut panel, area.width, area.height);

    panel.update_hover(area.y, area.x, area, false, Path::new(NO_PROJECT));

    assert!(matches!(panel.hover, Some(HoverTarget::CachedThinking(0))));
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

    panel.update_hover(area.y, area.x, area, false, Path::new(NO_PROJECT));
    let terminal = render(&mut panel, area.width, area.height);

    assert!(matches!(panel.hover, Some(HoverTarget::StreamingThinking)));
    assert!(buffer_text(&terminal).contains("Thinking"));
}

#[test]
fn transcript_hover_clears_explicitly_and_on_scroll_or_layout_change() {
    let mut panel = panel_with_long_tool(SHELL_TOOL_NAME, 200);
    let area = Rect::new(0, 0, 80, 24);

    panel.update_hover(area.y, area.x, area, false, Path::new(NO_PROJECT));
    assert!(panel.hover.is_some());
    panel.clear_hover();
    assert!(panel.hover.is_none());

    panel.update_hover(area.y, area.x, area, false, Path::new(NO_PROJECT));
    panel.set_scroll_top(panel.scroll_top());
    assert!(panel.hover.is_none());

    panel.update_hover(area.y, area.x, area, false, Path::new(NO_PROJECT));
    render(&mut panel, 79, area.height);
    assert!(panel.hover.is_none());
}

/// Reasoning is part of how the answer was reached, so the reader sees it
/// without asking. Turning the gate off is what hides it.
#[test]
fn default_shows_streaming_thinking() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.streaming_thinking.set_buffer("visible reasoning");
    let terminal = render(&mut panel, 80, 10);
    let text = buffer_text(&terminal);
    assert!(
        text.contains("Thinking") && text.contains("visible reasoning"),
        "default config should show reasoning; got: {text}"
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

const TITLE_MARKUP_MSG: &str = "a heading's own fence must never be drawn as body text";
const TITLE_ROW_MSG: &str =
    "an unnamed thought is one header row, so nothing jumps when it names itself";

/// The title arrives a fragment at a time, and its `**` used to open a body
/// row that the header took back the moment the fence closed.
#[test]
fn a_streaming_title_never_lands_in_the_body_first() {
    let mut panel = streaming_reasoning_panel();

    for fragment in ["**", "Review", "ing"] {
        panel.thinking_delta(fragment);
        let text = buffer_text(&render(&mut panel, 80, 10));
        assert!(!text.contains('*'), "{TITLE_MARKUP_MSG}; got: {text}");
        assert_eq!(panel.last_total_lines, 1, "{TITLE_ROW_MSG}; got: {text}");
    }

    panel.thinking_delta("**");
    let named = buffer_text(&render(&mut panel, 80, 10));
    assert!(named.contains("Thinking: Reviewing"), "{named}");
    assert_eq!(panel.last_total_lines, 1, "{TITLE_ROW_MSG}; got: {named}");

    panel.thinking_delta("\n\nBody details");
    let bodied = buffer_text(&render(&mut panel, 80, 10));
    assert!(bodied.contains("Thinking: Reviewing"), "{bodied}");
    assert!(bodied.contains("Body details"), "{bodied}");
    assert_eq!(panel.last_total_lines, 3, "{bodied}");
}

/// Bold that opens a sentence is prose, so withholding it would strand the
/// reader on an empty header for the whole block.
#[test]
fn streaming_reasoning_that_never_names_itself_shows_its_body_at_once() {
    let mut panel = streaming_reasoning_panel();
    panel.thinking_delta("**Important:** keep this in the body.");

    let text = buffer_text(&render(&mut panel, 80, 10));

    assert!(!text.contains("Thinking: Important"), "{text}");
    assert!(text.contains("keep this in the body."), "{text}");
}

#[test]
fn an_unfinished_streaming_title_is_not_copied_as_body() {
    let mut panel = streaming_reasoning_panel();
    panel.push(DisplayMessage::new(DisplayRole::User, "Question".into()));
    panel.thinking_delta("**Solo tra");

    let copied = extract_entire_document(&mut panel);

    assert!(copied.contains("## Thinking"), "{copied}");
    assert!(!copied.contains('*'), "{TITLE_MARKUP_MSG}; got: {copied}");
}

/// Reveals the buffer one delta at a time, so a fragment is on screen the
/// frame it arrives and the reveal never runs ahead of the assertions.
fn streaming_reasoning_panel() -> MessagesPanel {
    MessagesPanel::new(
        UiConfig {
            show_thinking: true,
            typewriter_ms_per_char: 0,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    )
}

#[test]
fn streaming_reasoning_shows_a_spinner_and_tenths_timer() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.thinking_delta("working");
    panel.thinking_started = Some(Instant::now() - Duration::from_millis(1_201));

    let text = buffer_text(&render(&mut panel, 80, 5));

    assert!(SPINNER_GLYPHS.chars().any(|glyph| text.contains(glyph)));
    assert!(text.contains("Thinking · 1.2s"));
    // The body is drawn now rather than counted, so the typewriter revealing
    // it is real work and asks for the faster cadence of the two.
    assert_eq!(panel.cadence(), Cadence::SMOOTH);
}

/// A collapsed block reveals nothing, so believing its typewriter would pin
/// the loop at full frame rate for the whole reasoning phase.
#[test]
fn collapsed_streaming_reasoning_only_asks_for_the_spinner() {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.thinking_delta("working");
    render(&mut panel, 80, 5);

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
    m.body_open = Some(false);
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
    let top = panel.scroll_top();
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
    panel.set_scroll_top(TALL_LINES as u32 - 5);
    render(&mut panel, 80, VIEWPORT_HEIGHT);
    assert!(
        !panel.auto_scroll(),
        "the test must start anchored deep inside the tall segment"
    );

    render(&mut panel, 40, VIEWPORT_HEIGHT);

    let top = panel.scroll_top();
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
        .anchor_at(panel.scroll_top(), 79)
        .expect("scroll_top lands inside a segment");

    render(&mut panel, 40, 10);

    let after = panel
        .cache
        .anchor_at(panel.scroll_top(), 39)
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
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
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
        if let Some(byte) = row.find(text) {
            let col = UnicodeWidthStr::width(&row[..byte]) as u16;
            return buf.cell((col, y)).unwrap().style();
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

const NOTICE_TEXT: &str = "Model ended turn without a response, nudging...";
const NOTICE_MARKDOWN: &str = "**not bold** notice";
const PROSE_MSG: &str = "a notice must not be styled like something the model said";

/// The harness speaking about the run, not the model speaking to the user.
/// A notice that paints like assistant prose is indistinguishable from a
/// reply, which is what the dedicated role exists to prevent.
#[test]
fn a_notice_renders_as_dim_italic_chrome() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        FIRST_TEXT.into(),
    ));
    panel.push(DisplayMessage::new(DisplayRole::Notice, NOTICE_TEXT.into()));

    let rendered = render(&mut panel, 80, 24);
    let text = buffer_text(&rendered);
    let notice = style_of(&rendered, NOTICE_TEXT);

    assert!(text.contains(NOTICE_PREFIX), "got: {text}");
    assert!(
        notice.add_modifier.contains(Modifier::ITALIC),
        "got: {notice:?}"
    );
    assert_ne!(notice, style_of(&rendered, FIRST_TEXT), "{PROSE_MSG}");
}

/// Notice text is composed by the host, so markdown in it is incidental and
/// must stay literal rather than restyle a line the user cannot edit.
#[test]
fn a_notice_never_parses_its_text_as_markdown() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Notice,
        NOTICE_MARKDOWN.into(),
    ));

    let text = buffer_text(&render(&mut panel, 80, 24));

    assert!(text.contains(NOTICE_MARKDOWN), "got: {text}");
}

/// Pins the bug the role was added for: a nudge landing after the reply used
/// to be the newest `Assistant` message, so copying the last reply returned
/// the harness notice instead of what the model wrote.
#[test]
fn a_notice_is_never_the_last_reply() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        FIRST_TEXT.into(),
    ));
    panel.push(DisplayMessage::new(DisplayRole::Notice, NOTICE_TEXT.into()));

    assert_eq!(panel.last_reply_source().as_deref(), Some(FIRST_TEXT));
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

    panel.update_hover(row, 1, area, false, Path::new(NO_PROJECT));
    assert!(
        matches!(panel.hover, Some(HoverTarget::Diagram(_))),
        "{:?}",
        panel.hover
    );

    panel.update_hover(0, 1, area, false, Path::new(NO_PROJECT));
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
    panel.update_hover(row, 1, area, false, Path::new(NO_PROJECT));
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
            panel.update_hover(row, 1, area, false, Path::new(NO_PROJECT));
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
            panel.update_hover(row, 1, area, false, Path::new(NO_PROJECT));
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
const CONTAINER_BODY_MSG: &str = "only a batch keeps its child rows in compact view";
const SHELL_VIEW_STICKY_MSG: &str =
    "re-opening a shell card must restore the last raw/filtered view";
const SHELL_HOVER_MSG: &str = "the raw/filtered switch must highlight itself, not the card header";
const WRAPPED_AIR_MSG: &str = "a row taller than one line must be separated from its neighbours";
const THOUGHT_AIR_MSG: &str = "reasoning must be separated from the call list around it";
const MAX_COMPACT_CLICK_CYCLE: usize = 4;
const TRUNCATING_LINES: usize = 200;
const WIDE_ENOUGH_TO_NOT_WRAP: u16 = 80;
const NARROW_ENOUGH_TO_WRAP: u16 = 40;

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

const DENSE_CARD_BG_MSG: &str = "a dense row is still a card and keeps the card background";
const FLAT_PROSE_MSG: &str = "prose the model wrote is not a card and takes no background";
const EXPANDED_INLINE_FLAT_MSG: &str =
    "expanded reaches inline only for a call that reads as prose, which stays flat";
const CALL_SUMMARY: &str = "needle";
const REPLY_TEXT: &str = "here is what I found";

/// Reads the leftmost column of the row, which is card fill rather than
/// content, so the assertion sees the band and not a span's own styling.
fn card_bg(terminal: &ratatui::Terminal<TestBackend>, text: &str) -> Option<Color> {
    let buf = terminal.backend().buffer();
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width)
            .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol()))
            .collect();
        if row.contains(text) {
            return buf.cell((0, y)).unwrap().style().bg;
        }
    }
    panic!("{text} was never rendered");
}

fn call_with_summary(id: &str, tool: &'static str) -> ToolStartEvent {
    let mut call = start(id, tool);
    call.summary = CALL_SUMMARY.into();
    call
}

/// The dense modes drop the rail and the padding to keep the list tight,
/// which left a call with nothing at all to separate it from the prose
/// around it. The background is the one piece of card chrome the list can
/// still afford, so it is the piece it keeps.
#[test_case(ViewMode::Compact ; "compact")]
#[test_case(ViewMode::Auto ; "auto")]
fn a_dense_call_keeps_the_card_background(view: ViewMode) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(view);
    panel.tool_start(call_with_summary("t1", FILE_GREP_TOOL_NAME));
    finished(&mut panel, &["t1"]);
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        REPLY_TEXT.into(),
    ));
    let terminal = render(&mut panel, WIDE_ENOUGH_TO_NOT_WRAP, 24);

    let panel_bg = theme::current().panel_style().bg;
    assert_eq!(
        card_bg(&terminal, CALL_SUMMARY),
        panel_bg,
        "{DENSE_CARD_BG_MSG}"
    );
    assert_ne!(card_bg(&terminal, REPLY_TEXT), panel_bg, "{FLAT_PROSE_MSG}");
}

/// Expanded classifies a one-line call as inline too, but there it means the
/// call is trivial enough to read as prose rather than that the mode is
/// dense. Giving that row a band would put a stripe on every pending call.
#[test]
fn an_expanded_one_line_call_stays_flat() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Expanded);
    panel.tool_start(call_with_summary("t1", CODE_MAP_TOOL_NAME));
    let terminal = render(&mut panel, WIDE_ENOUGH_TO_NOT_WRAP, 24);

    assert_eq!(
        panel.segment_heights(),
        vec![1],
        "{EXPANDED_INLINE_FLAT_MSG}"
    );
    assert_ne!(
        card_bg(&terminal, CALL_SUMMARY),
        theme::current().panel_style().bg,
        "{EXPANDED_INLINE_FLAT_MSG}"
    );
}

fn margins(panel: &MessagesPanel, width: u16) -> Vec<u16> {
    (0..panel.cache.len())
        .map(|i| panel.cache.get(i).unwrap().chrome(width).margin_top)
        .collect()
}

/// Reasoning is prose the model wrote, not a call it made. Even collapsed to
/// a single line it is a block, and running it flush into the call list made
/// it read as one more tool row.
#[test_case(ViewMode::Compact ; "compact")]
#[test_case(ViewMode::Auto ; "auto")]
fn a_thought_takes_air_from_the_rows_around_it(view: ViewMode) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(view);
    panel.tool_start(start("t1", FILE_GREP_TOOL_NAME));
    panel.tool_done(done("t1"));
    let mut thought = DisplayMessage::new(DisplayRole::Thinking, "planning".into());
    thought.body_open = Some(false);
    panel.push(thought);
    panel.tool_start(start("t2", FILE_GREP_TOOL_NAME));
    panel.tool_done(done("t2"));
    render(&mut panel, WIDE_ENOUGH_TO_NOT_WRAP, 24);

    assert_eq!(
        margins(&panel, WIDE_ENOUGH_TO_NOT_WRAP),
        vec![0, 1, 1],
        "{THOUGHT_AIR_MSG}"
    );
}

/// The reason the mode felt cramped: a row that wraps is no longer a list
/// entry, and running it flush into its neighbours hides where it starts and
/// ends. It takes a blank row on both sides.
#[test_case(ViewMode::Compact ; "compact")]
#[test_case(ViewMode::Auto ; "auto")]
fn a_wrapped_row_takes_air_from_the_rows_around_it(view: ViewMode) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(view);
    let mut thought = DisplayMessage::new(DisplayRole::Thinking, "planning".into());
    thought.body_open = Some(false);
    panel.push(thought);
    let mut long = start("t1", FILE_READ_TOOL_NAME);
    long.summary = "a/very/deeply/nested/path/that/has/to/wrap/at/this/width.md".into();
    panel.tool_start(long);
    panel.tool_done(done("t1"));
    panel.tool_start(start("t2", FILE_GREP_TOOL_NAME));
    panel.tool_done(done("t2"));
    render(&mut panel, NARROW_ENOUGH_TO_WRAP, 24);

    assert!(
        panel.segment_heights()[1] > 2,
        "the long row must wrap: {:?}",
        panel.segment_heights()
    );
    assert_eq!(
        margins(&panel, NARROW_ENOUGH_TO_WRAP),
        vec![0, 1, 1],
        "{WRAPPED_AIR_MSG}"
    );
}

/// Auto opens the newest read-only call, and an open body is a block. It must
/// not run into the dense rows above it.
#[test]
fn the_card_auto_opens_separates_from_the_list_above_it() {
    let mut panel = panel_with_tools(&[("t1", CODE_MAP_TOOL_NAME)]);
    panel.tool_done(done("t1"));
    panel.tool_start(start("t2", CODE_MAP_TOOL_NAME));
    panel.tool_done(long_done("t2", HELD_BODY_LINES));
    render(&mut panel, WIDE_ENOUGH_TO_NOT_WRAP, 24);

    assert!(
        panel.segment_heights()[1] > 1,
        "auto must open the newest call: {:?}",
        panel.segment_heights()
    );
    assert_eq!(
        margins(&panel, WIDE_ENOUGH_TO_NOT_WRAP),
        vec![0, 1],
        "{WRAPPED_AIR_MSG}"
    );
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
    // Both blocks have to be collapsed for the comparison to be about the
    // margin rather than about one of them drawing a body.
    panel.show_thinking = false;
    finished(&mut panel, &["t1"]);
    render(&mut panel, 80, 24);
    let settled = panel.last_total_lines;

    panel.thinking_delta("weighing options");
    render(&mut panel, 80, 24);
    let streaming = panel.last_total_lines;

    let mut msg = DisplayMessage::new(DisplayRole::Thinking, "weighing options".into());
    msg.body_open = Some(false);
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

    assert_eq!(first_line_text(&panel, 0), "⌕ Grepped t1 (1 lines)");
}

/// The past tense asserts the call happened, so a failure has to fall back to
/// the plain verb rather than claim it edited anything.
#[test_case(None, "Grepping" ; "running")]
#[test_case(Some(false), "Grepped" ; "succeeded")]
#[test_case(Some(true), "Grep" ; "failed")]
fn a_compact_label_is_inflected_by_what_the_call_is_doing(outcome: Option<bool>, expected: &str) {
    let mut panel = compact_panel(&[("t1", FILE_GREP_TOOL_NAME)]);
    if let Some(is_error) = outcome {
        let mut event = done("t1");
        event.is_error = is_error;
        panel.tool_done(event);
    }
    rebuild(&mut panel);

    // The leading glyph is a spinner while the call runs, so the label is
    // read as the token after it rather than from the front of the row.
    let row = first_line_text(&panel, 0);
    assert_eq!(row.split_whitespace().nth(1), Some(expected), "{row:?}");
}

const EXPANDED_HEADER_LABEL_MSG: &str =
    "an expanded card says what the call is doing, not what its tool is registered as";
/// How `shell` heads a row, whichever density drew it.
const SHELL_SIGIL: &str = "$";

/// The expanded header wrote the bare tool name and an arrow, so the same
/// shell call read `shell>` on a card and `Ran` on a row one view mode away.
/// Two spellings of one sentence make the modes look like different products,
/// and the arrow form leaks a registry key the reader never has to know.
#[test_case(None, "Running" ; "running")]
#[test_case(Some(false), "Ran" ; "succeeded")]
#[test_case(Some(true), "Run" ; "failed")]
fn an_expanded_card_header_is_inflected_like_its_row(outcome: Option<bool>, expected: &str) {
    let mut panel = panel_with_tools(&[(TOOL_ID, SHELL_TOOL_NAME)]);
    panel.set_view(ViewMode::Expanded);
    if let Some(is_error) = outcome {
        let mut event = done(TOOL_ID);
        event.is_error = is_error;
        panel.tool_done(event);
    }
    rebuild(&mut panel);

    // A running card carries the spinner ahead of its sigil and a finished one
    // holds that slot blank, so the label is found behind the sigil rather than
    // at a fixed offset from the front of the row.
    let header = first_line_text(&panel, 0);
    let tokens: Vec<&str> = header.split_whitespace().collect();
    let sigil = tokens
        .iter()
        .position(|token| *token == SHELL_SIGIL)
        .expect("the header opens on its tool's sigil");
    assert_eq!(
        tokens.get(sigil + 1),
        Some(&expected),
        "{EXPANDED_HEADER_LABEL_MSG}: {header:?}"
    );
}

/// A tool no registry classified, which is every call arriving through an MCP
/// server the effect table has never seen.
const UNCLASSIFIED_TOOL: &str = "mystery_tool";

#[test]
fn an_unknown_tool_falls_back_to_its_registered_name() {
    let mut panel = compact_panel(&[("t1", UNCLASSIFIED_TOOL)]);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);

    assert_eq!(first_line_text(&panel, 0), "⚙ mystery_tool t1 (1 lines)");
}

#[test]
fn a_read_replaces_its_requested_window_with_the_returned_range() {
    const PATH: &str = "src/main.rs";
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Compact);
    let mut event = start("t1", FILE_READ_TOOL_NAME);
    event.summary = PATH.into();
    event.raw_input = Some(serde_json::json!({
        "filePath": PATH,
        "offset": 190,
        "limit": 140,
    }));
    panel.tool_start(event);
    rebuild(&mut panel);

    // Key order follows serde_json's map, which the workspace flips to
    // insertion order via `preserve_order`; only membership is stable.
    let pending = first_line_text(&panel, 0);
    assert!(pending.contains("offset=190"), "{pending}");
    assert!(pending.contains("limit=140"), "{pending}");
    assert!(!pending.contains("filePath"), "{pending}");

    let mut event = done("t1");
    event.tool = FILE_READ_TOOL_NAME.into();
    event.output = ToolOutput::ReadCode {
        path: PATH.into(),
        start_line: 190,
        lines: vec!["x".into(); 140],
        total_lines: 668,
        instructions: None,
    };
    panel.tool_done(event);
    rebuild(&mut panel);

    assert_eq!(
        first_line_text(&panel, 0),
        "→ Read src/main.rs (lines 190–329 of 668)"
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
        panel.update_hover(area.y, area.x, area, false, Path::new(NO_PROJECT));
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

/// A batch's body is the list of the calls it made, so folded it says nothing
/// at all and compact keeps it. Every other container answers for itself like
/// any other call: a task carries a subagent's whole result, which is the bulk
/// the mode was chosen to put away.
#[test_case(BATCH_TOOL_NAME, true; "a batch keeps its list")]
#[test_case(TASK_TOOL_NAME, false; "a task folds like any other call")]
fn compact_keeps_only_a_batchs_child_rows(tool: &'static str, keeps_body: bool) {
    let mut panel = compact_panel(&[("t1", tool)]);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);

    let container = panel.segment_heights()[0];
    let mut plain = compact_panel(&[("t1", FILE_GREP_TOOL_NAME)]);
    finished(&mut plain, &["t1"]);
    rebuild(&mut plain);

    assert_eq!(
        container > plain.segment_heights()[0],
        keeps_body,
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
    let mut panel = compact_panel(&[("t1", CODE_MAP_TOOL_NAME)]);
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
    msg.body_open = Some(false);
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
        line_text(&thought_line(reasoning_summary("**Reviewing**").title, None, false)[0]),
        "Thinking: Reviewing"
    );
    assert_eq!(
        line_text(
            &thought_line(
                reasoning_summary("plain body").title,
                Some(Duration::from_secs(2)),
                true
            )[0]
        ),
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
    msg.body_open = Some(false);
    panel.push(msg);
    rebuild(&mut panel);

    assert_eq!(first_line_text(&panel, 0), "Thought");
}

/// Density is a claim about tool calls. Reasoning is how the answer was
/// reached, so no mode takes it away and switching modes cannot either.
#[test_case(ViewMode::Compact ; "compact")]
#[test_case(ViewMode::Auto ; "auto")]
fn no_mode_closes_reasoning(view: ViewMode) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(
        DisplayRole::Thinking,
        "reasoning".into(),
    ));
    rebuild(&mut panel);
    assert!(panel.body_open(&panel.messages[0]), "{MODE_KEEPS_MSG}");

    panel.set_view(view);
    rebuild(&mut panel);

    assert!(panel.body_open(&panel.messages[0]), "{MODE_KEEPS_MSG}");
}

/// The click is the only way to fold a long block, so it has to work in the
/// mode that draws every card in full as much as in the dense ones.
#[test_case(ViewMode::Expanded ; "expanded")]
#[test_case(ViewMode::Compact ; "compact")]
#[test_case(ViewMode::Auto ; "auto")]
fn a_thought_folds_and_reopens_on_click(view: ViewMode) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(view);
    panel.push(DisplayMessage::new(
        DisplayRole::Thinking,
        THINKING_TEXT.into(),
    ));
    rebuild(&mut panel);
    let area = Rect::new(0, 0, 80, 24);

    assert!(panel.handle_click(0, area), "{THOUGHT_FOLD_MSG}");
    assert!(!panel.body_open(&panel.messages[0]), "{THOUGHT_FOLD_MSG}");

    assert!(panel.handle_click(0, area), "{THOUGHT_FOLD_MSG}");
    assert!(panel.body_open(&panel.messages[0]), "{THOUGHT_FOLD_MSG}");
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
    let line = &thought_line(Some("t"), Some(duration), true)[0];
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert_eq!(text, expected);
}

/// Back-dated far enough that the tenths are stable however slow the host is.
const SHELL_RAN_FOR: Duration = Duration::from_millis(1_201);
const SHELL_LIVE_CLOCK: &str = "· 1.2s";
/// What `shell_done` reports as the command's own time.
const SHELL_MEASURED_CLOCK: &str = "· 10ms";

/// The card is cached after the first render, so a child's clock reaches the
/// screen only if the clock-driven refresh rebuilds the batch. Driven through
/// `tool_start` and `batch_progress` rather than by poking the map, so the
/// roster, the clock and the refresh are all exercised the way the agent
/// drives them.
///
/// Being withheld from the highlight worker is the other half of this, and is
/// covered where the request is built: the worker does not deliver in a test,
/// so a panel test cannot tell the two apart.
#[test]
fn a_running_shell_child_ticks_inside_a_batch() {
    let mut panel = panel_with_tools(&[(TOOL_ID, BATCH_TOOL)]);
    let mut event = start(TOOL_ID, BATCH_TOOL);
    event.output = Some(ToolOutput::Batch {
        entries: vec![pending_child(SHELL_TOOL_NAME)],
        text: String::new(),
    });
    panel.tool_start(event);
    panel.batch_progress(TOOL_ID, 0, running_child(SHELL_TOOL_NAME));
    render(&mut panel, 80, 20);

    let started = panel.batch_child_started[TOOL_ID][&0];
    Arc::make_mut(panel.batch_child_started.get_mut(TOOL_ID).unwrap())
        .insert(0, started - SHELL_RAN_FOR);
    let text = buffer_text(&render(&mut panel, 80, 20));

    assert!(text.contains(SHELL_LIVE_CLOCK), "{text}");
}

/// A roster can arrive with a child already running, so a clock stamped only
/// on the transition would never start for it.
#[test]
fn a_batch_whose_roster_arrives_running_still_gets_a_clock() {
    let panel = panel_with_running_shell();

    assert!(panel.batch_child_started.contains_key("t1"));
}

#[test]
fn a_settled_shell_child_drops_its_wall_clock() {
    let mut panel = panel_with_running_shell();
    assert!(panel.batch_child_started.contains_key("t1"));

    panel.batch_progress("t1", 0, batch_child(SHELL_TOOL_NAME, "a"));

    assert!(
        !panel.batch_child_started.contains_key("t1"),
        "a card with nothing counting must not ask to be repainted"
    );
}

/// The card is cached after the first render, so the clock only advances if
/// the clock-driven refresh knows to rebuild it.
#[test]
fn a_running_shell_card_ticks_after_its_segment_is_cached() {
    let mut panel = panel_with_tools(&[("t1", SHELL_TOOL_NAME)]);
    assert!(panel.messages[0].tool_started.is_some());
    render(&mut panel, 80, 20);

    panel.messages[0].tool_started = Some(Instant::now() - SHELL_RAN_FOR);
    let text = buffer_text(&render(&mut panel, 80, 20));

    assert!(text.contains(SHELL_LIVE_CLOCK), "{text}");
}

#[test]
fn a_settled_shell_card_swaps_the_live_clock_for_the_measured_one() {
    let mut panel = panel_with_tools(&[("t1", SHELL_TOOL_NAME)]);
    panel.messages[0].tool_started = Some(Instant::now() - SHELL_RAN_FOR);
    render(&mut panel, 80, 20);

    panel.tool_done(shell_done("t1", false));
    let text = buffer_text(&render(&mut panel, 80, 20));

    assert!(text.contains(SHELL_MEASURED_CLOCK), "{text}");
    assert!(!text.contains(SHELL_LIVE_CLOCK), "{text}");
}

#[test_case(Duration::from_millis(420), "0.4s" ; "sub_second")]
#[test_case(Duration::from_millis(9_700), "9.7s" ; "under_a_minute")]
#[test_case(Duration::from_millis(125_340), "2m 5.3s" ; "over_a_minute")]
fn live_thinking_duration_always_keeps_tenths(duration: Duration, expected: &str) {
    assert_eq!(format_live_duration(duration), expected);
}

#[test]
fn expanded_density_keeps_the_sigil_and_the_card() {
    // The spinner's slot is held blank once the call lands, so the sigil sits
    // in the column it occupied while the call ran.
    const CARD_HEADER: &str = "  ◇ Mapped ";

    let mut panel = panel_with_tools(&[("t1", CODE_MAP_TOOL_NAME)]);
    panel.set_view(ViewMode::Expanded);
    finished(&mut panel, &["t1"]);
    rebuild(&mut panel);

    assert!(first_line_text(&panel, 0).starts_with(CARD_HEADER));
    assert!(panel.segment_heights()[0] > 1);
}

const MODE_EFFECT_MSG: &str = "auto and expanded decide a read-only card and leave a writing card \
    to itself, and compact decides both";
const AUTO_TAIL_MSG: &str = "auto must leave the newest card open";
const AUTO_HANDOFF_MSG: &str = "the card auto opened must close once a newer one takes its place";
const AUTO_STREAM_MSG: &str = "nothing settled is newest while the model is still writing";
const MANUAL_STICKY_MSG: &str = "a card the reader opened must stay open as the transcript grows";
const REASONING_GATE_MSG: &str = "the gate is the only thing that decides whether reasoning shows";
const REASONING_TAIL_MSG: &str = "a thought must stay open once the transcript grows past it";
const MODE_KEEPS_MSG: &str = "no view mode may take reasoning away";
const THOUGHT_FOLD_MSG: &str = "a click must fold an open thought and open a folded one";

fn mode_panel(view: ViewMode, ids: &[(&str, &'static str)]) -> MessagesPanel {
    let mut panel = panel_with_tools(ids);
    panel.set_view(view);
    for &(id, _) in ids {
        panel.tool_done(done(id));
    }
    rebuild(&mut panel);
    panel
}

/// In auto and expanded a read-only call is the only one the mode may hide: a
/// call that wrote something keeps its body, or the transcript stops showing
/// what happened to the workspace. Compact is the one mode that answers for
/// every tool, because a list with the writes still drawn is not a list.
#[test_case(ViewMode::Expanded, CODE_MAP_TOOL_NAME, true; "expanded opens a map")]
#[test_case(ViewMode::Compact, CODE_MAP_TOOL_NAME, false; "compact closes a map")]
#[test_case(ViewMode::Auto, CODE_MAP_TOOL_NAME, true; "auto opens the newest map")]
#[test_case(ViewMode::Expanded, FILE_WRITE_TOOL_NAME, true; "expanded opens a write")]
#[test_case(ViewMode::Compact, FILE_WRITE_TOOL_NAME, false; "compact closes a write too")]
#[test_case(ViewMode::Auto, FILE_WRITE_TOOL_NAME, true; "auto still opens a write")]
fn the_mode_decides_which_cards_open(view: ViewMode, tool: &'static str, open: bool) {
    let panel = mode_panel(view, &[("t1", tool)]);

    assert_eq!(!panel.card_closed("t1"), open, "{MODE_EFFECT_MSG}");
}

const COMPACT_FOLDS_MSG: &str = "compact answers for every tool, whatever the call did, so its row \
    is all there is until the reader asks for more";
const COMPACT_KEEPS_BATCH_MSG: &str = "a folded batch says nothing at all, so it is the one call \
    compact leaves open";

/// One call per effect class, because the class is what the older rule keyed
/// on and each one reached the mode down a different early return. Compact now
/// answers before any of them.
#[test_case(FILE_WRITE_TOOL_NAME, false ; "mutating")]
#[test_case(PYTHON_EXECUTION_TOOL_NAME, false ; "isolated")]
#[test_case(TASK_TOOL_NAME, false ; "orchestrator")]
#[test_case(UNCLASSIFIED_TOOL, false ; "unclassified")]
#[test_case(CODE_MAP_TOOL_NAME, false ; "read only")]
#[test_case(BATCH_TOOL_NAME, true ; "a batch is the exception")]
fn compact_folds_every_call_but_a_batch(tool: &'static str, open: bool) {
    let panel = mode_panel(ViewMode::Compact, &[("t1", tool)]);
    let message = match open {
        true => COMPACT_KEEPS_BATCH_MSG,
        false => COMPACT_FOLDS_MSG,
    };

    assert_eq!(!panel.card_closed("t1"), open, "{message}");
}

const ONE_CLICK_MSG: &str = "one click must show the whole body, whatever the mode drew before";
const REST_MSG: &str = "a transcript that gives every call a card has no header to close to, so a full card falls \
     back to the budget it rested at";
const BODY_TAIL: &str = "line 7";
const CLICKED_BODY_LINES: usize = 8;

/// The budget is where a card rests, never where a click lands. Walking a
/// reader through it cost a second click to answer the question the first one
/// asked, and cost a different number of clicks per mode.
#[test_case(ViewMode::Compact ; "compact")]
#[test_case(ViewMode::Auto ; "auto")]
#[test_case(ViewMode::Expanded ; "expanded")]
fn one_click_opens_the_whole_body(view: ViewMode) {
    let mut panel = panel_with_tools(&[("t1", FILE_GREP_TOOL_NAME)]);
    panel.set_view(view);
    panel.tool_done(long_done("t1", CLICKED_BODY_LINES));
    rebuild(&mut panel);
    assert!(
        !seg_text(&panel, "t1").contains(BODY_TAIL),
        "{MODE_EFFECT_MSG}"
    );

    assert!(
        panel.handle_click(0, Rect::new(0, 0, 80, 24)),
        "{COMPACT_CLICK_MSG}"
    );
    rebuild(&mut panel);

    assert!(
        seg_text(&panel, "t1").contains(BODY_TAIL),
        "{ONE_CLICK_MSG}"
    );
}

/// Expanded draws every call as its own card, so `close_card` refuses and the
/// way back out of a full body is the resting budget.
#[test]
fn a_full_card_in_expanded_falls_back_to_the_budget() {
    let mut panel = panel_with_tools(&[("t1", CODE_MAP_TOOL_NAME)]);
    panel.set_view(ViewMode::Expanded);
    panel.tool_done(long_done("t1", CLICKED_BODY_LINES));
    rebuild(&mut panel);
    let area = Rect::new(0, 0, 80, 24);

    assert!(panel.handle_click(0, area), "{COMPACT_CLICK_MSG}");
    rebuild(&mut panel);
    assert!(
        seg_text(&panel, "t1").contains(BODY_TAIL),
        "{ONE_CLICK_MSG}"
    );

    assert!(panel.handle_click(0, area), "{REST_MSG}");
    rebuild(&mut panel);

    assert!(!panel.card_closed("t1"), "{REST_MSG}");
    assert!(!seg_text(&panel, "t1").contains(BODY_TAIL), "{REST_MSG}");
}

/// `python_execution` draws a script and an output, which used to be separately
/// disclosed and so took two clicks past the first to open. They are one
/// disclosure now: the window the card rests at already carries both, and a
/// click takes both away or gives both back rather than stepping through them.
#[test]
fn a_script_and_its_output_are_disclosed_together() {
    const SCRIPT_TAIL: &str = "script 7";
    const TOGETHER_MSG: &str =
        "the script and its output are one disclosure, so a card shows both or neither";

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Auto);
    panel.tool_start(ToolStartEvent {
        input: Some(ToolInput::Code {
            language: "python".into(),
            code: (0..CLICKED_BODY_LINES)
                .map(|i| format!("script {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }),
        ..start("t1", "python_execution")
    });
    panel.tool_done(long_done("t1", CLICKED_BODY_LINES));
    rebuild(&mut panel);
    let area = Rect::new(0, 0, 80, 24);

    let resting = seg_text(&panel, "t1");
    assert!(
        resting.contains(SCRIPT_TAIL) && resting.contains(BODY_TAIL),
        "{TOGETHER_MSG}"
    );

    assert!(panel.handle_click(0, area), "{COMPACT_CLICK_MSG}");
    rebuild(&mut panel);
    let closed = seg_text(&panel, "t1");
    assert!(
        !closed.contains(SCRIPT_TAIL) && !closed.contains(BODY_TAIL),
        "{TOGETHER_MSG}"
    );

    assert!(panel.handle_click(0, area), "{COMPACT_CLICK_MSG}");
    rebuild(&mut panel);

    let text = seg_text(&panel, "t1");
    assert!(text.contains(SCRIPT_TAIL), "{ONE_CLICK_MSG}");
    assert!(text.contains(BODY_TAIL), "{ONE_CLICK_MSG}");
}

/// The point of auto: the call being worked on reads in full, and the ones
/// behind it fall back to a row without the reader touching anything.
#[test]
fn auto_opens_the_newest_read_and_closes_the_one_before_it() {
    let mut panel = mode_panel(ViewMode::Auto, &[("t1", CODE_MAP_TOOL_NAME)]);
    assert!(!panel.card_closed("t1"), "{AUTO_TAIL_MSG}");

    panel.tool_start(start("t2", CODE_MAP_TOOL_NAME));
    panel.tool_done(done("t2"));
    rebuild(&mut panel);

    assert!(panel.card_closed("t1"), "{AUTO_HANDOFF_MSG}");
    assert!(!panel.card_closed("t2"), "{AUTO_TAIL_MSG}");
}

/// Live text draws under every settled card, so the last card is no longer
/// the thing being written and has no claim on staying open.
#[test]
fn auto_hands_the_newest_slot_to_the_reply_being_written() {
    let mut panel = mode_panel(ViewMode::Auto, &[("t1", CODE_MAP_TOOL_NAME)]);
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

/// `show_thinking` is the reader saying whether they want reasoning at all,
/// and it is the whole answer: the modes divide tool calls by density and
/// have no say over how the answer was reached.
#[test_case(ViewMode::Expanded, false, false; "expanded obeys the gate")]
#[test_case(ViewMode::Compact, false, false; "compact obeys the gate")]
#[test_case(ViewMode::Auto, false, false; "auto obeys the gate")]
#[test_case(ViewMode::Expanded, true, true; "expanded opens it")]
#[test_case(ViewMode::Compact, true, true; "compact opens it too")]
#[test_case(ViewMode::Auto, true, true; "auto opens it too")]
fn reasoning_answers_to_the_gate_alone(view: ViewMode, show_thinking: bool, open: bool) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.show_thinking = show_thinking;
    panel.set_view(view);
    panel.push(DisplayMessage::new(DisplayRole::Thinking, "hmm".into()));
    rebuild(&mut panel);

    assert_eq!(
        panel.body_open(&panel.messages[0]),
        open,
        "{REASONING_GATE_MSG}"
    );
}

/// Auto governs tool cards by recency. Reasoning is not a card it governs,
/// so a newer message is not a reason to close the thought behind it.
#[test]
fn auto_leaves_older_reasoning_open() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.push(DisplayMessage::new(DisplayRole::Thinking, "first".into()));
    rebuild(&mut panel);
    let open = panel.segment_heights()[0];

    panel.push(DisplayMessage::new(DisplayRole::Thinking, "second".into()));
    rebuild(&mut panel);

    assert!(panel.body_open(&panel.messages[0]), "{REASONING_TAIL_MSG}");
    assert!(panel.body_open(&panel.messages[1]), "{REASONING_TAIL_MSG}");
    assert_eq!(panel.segment_heights()[0], open, "{REASONING_TAIL_MSG}");
}

const BATCH_TOOL: &str = "batch";
const BATCH_CHILD_BODY: &str = "child_body_line";
const EXPECT_BODY_HIDDEN: &str = "a folded child draws no body";
const EXPECT_BODY_SHOWN: &str = "an opened child draws its body";
const EXPECT_OTHERS_KEPT: &str = "the other children are untouched";
const EXPECT_SUMMARIES_KEPT: &str = "a folded batch still lists what it ran";

fn batch_child(tool: &str, marker: &str) -> caudra_agent::BatchToolEntry {
    caudra_agent::BatchToolEntry {
        model_suffix: None,
        tool: tool.into(),
        effect: effect_of(tool),
        summary: format!("{tool} ran"),
        status: caudra_agent::BatchToolStatus::Success,
        input: None,
        raw_input: None,
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
            entries: vec![
                batch_child(FILE_READ_TOOL_NAME, "a"),
                batch_child(FILE_GREP_TOOL_NAME, "b"),
            ],
            text: String::new(),
        },
        ..done("t1")
    });
    render(&mut panel, 80, 24);
    panel
}

/// The first row drawn for a given batch control, as the panel counts rows.
fn batch_row(panel: &MessagesPanel, target: RowTarget) -> u16 {
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
        .find(|row| segment.row_target_at(*row, width) == Some(target))
        .expect("the control was drawn")
}

fn batch_child_row(panel: &MessagesPanel, index: usize) -> u16 {
    batch_row(panel, RowTarget(index))
}

/// Arms a child's window and offers it a wheel burst, which is what a press
/// inside it followed by a notch does. Position alone no longer reaches a
/// window: an unarmed card would eat notches aimed at the transcript behind
/// it. The press lands on the first body row, since the header is the card's
/// own control rather than part of the window.
fn wheel_child(
    panel: &mut MessagesPanel,
    terminal: &ratatui::Terminal<TestBackend>,
    index: usize,
    delta: i32,
) -> i32 {
    let (column, _) = card_bar_rows(terminal)[0];
    let row = batch_child_row(panel, index) + 1;
    assert!(panel.arm_card_at(column, row), "{ARM_MSG}");
    panel.scroll_card_at(column, row, delta)
}

/// The bug: a batch dumped every child body into the transcript, which is
/// what the compact list exists to avoid. A card nobody has touched is the
/// roster of what ran.
#[test_case(ViewMode::Compact ; "compact")]
#[test_case(ViewMode::Auto ; "auto")]
#[test_case(ViewMode::Expanded ; "expanded")]
fn an_untouched_batch_shows_no_child_bodies(view: ViewMode) {
    let mut panel = panel_with_batch();
    panel.set_view(view);
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(text.contains("read ran"), "{EXPECT_SUMMARIES_KEPT}");
    assert!(text.contains("grep ran"), "{EXPECT_SUMMARIES_KEPT}");
    assert!(!text.contains("child_body_line_a"), "{EXPECT_BODY_HIDDEN}");
    assert!(!text.contains("child_body_line_b"), "{EXPECT_BODY_HIDDEN}");
}

const EXPECT_CHANGE_SHOWN: &str = "a child whose body is the only record of what it did draws with \
     the card in auto and expanded, exactly as its own card would";

/// The rule a standalone card runs outside compact: neither auto nor expanded
/// may hide a call that changed something. A batch folded every child
/// regardless, so a whole turn of edits ran inside one and showed not a single
/// diff. Compact is covered separately, where every child folds.
#[test_case(ViewMode::Auto ; "auto")]
#[test_case(ViewMode::Expanded ; "expanded")]
fn a_batch_child_that_changed_something_shows_it_outside_compact(view: ViewMode) {
    let mut panel = panel_with_tools(&[("t1", BATCH_TOOL)]);
    panel.tool_done(ToolDoneEvent {
        tool: BATCH_TOOL.into(),
        output: ToolOutput::Batch {
            entries: vec![
                batch_child(FILE_EDIT_TOOL_NAME, "a"),
                batch_child(FILE_READ_TOOL_NAME, "b"),
            ],
            text: String::new(),
        },
        ..done("t1")
    });
    panel.set_view(view);
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(text.contains("child_body_line_a"), "{EXPECT_CHANGE_SHOWN}");
    assert!(!text.contains("child_body_line_b"), "{EXPECT_BODY_HIDDEN}");
}

const CHILD_FOLDS_IN_COMPACT_MSG: &str = "a batch is open in compact so its list can be read, and a \
    child drawing its own body puts back the bulk the mode was chosen to remove";
const CHILD_OPENS_IN_COMPACT_MSG: &str = "a folded child is still one press from its body, or \
    compact would be the one view that cannot reach a diff at all";

/// The other half of the exception: compact keeps the batch and folds what is
/// inside it. A child that changed something is the case that matters, since
/// every other rule lets that one draw.
#[test]
fn compact_folds_every_batch_child() {
    let mut panel = panel_with_tools(&[("t1", BATCH_TOOL)]);
    panel.tool_done(ToolDoneEvent {
        tool: BATCH_TOOL.into(),
        output: ToolOutput::Batch {
            entries: vec![
                batch_child(FILE_EDIT_TOOL_NAME, "a"),
                batch_child(FILE_READ_TOOL_NAME, "b"),
            ],
            text: String::new(),
        },
        ..done("t1")
    });
    panel.set_view(ViewMode::Compact);
    render(&mut panel, 80, 24);

    let folded = seg_text(&panel, "t1");
    assert!(
        folded.contains("read ran"),
        "{CHILD_FOLDS_IN_COMPACT_MSG}: {folded:?}"
    );
    for body in ["child_body_line_a", "child_body_line_b"] {
        assert!(
            !folded.contains(body),
            "{CHILD_FOLDS_IN_COMPACT_MSG}: {folded:?}"
        );
    }

    assert!(panel.handle_click(batch_child_row(&panel, 0), Rect::new(0, 0, 80, 24)));
    render(&mut panel, 80, 24);
    let opened = seg_text(&panel, "t1");
    assert!(
        opened.contains("child_body_line_a"),
        "{CHILD_OPENS_IN_COMPACT_MSG}: {opened:?}"
    );
    assert!(
        !opened.contains("child_body_line_b"),
        "{CHILD_FOLDS_IN_COMPACT_MSG}: {opened:?}"
    );
}

#[test]
fn clicking_a_batch_child_opens_only_that_child() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);

    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    render(&mut panel, 80, 24);
    let opened = seg_text(&panel, "t1");
    assert!(opened.contains("child_body_line_a"), "{EXPECT_BODY_SHOWN}");
    assert!(
        !opened.contains("child_body_line_b"),
        "{EXPECT_OTHERS_KEPT}"
    );
}

#[test]
fn clicking_an_open_batch_child_folds_it_again() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    render(&mut panel, 80, 24);

    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    render(&mut panel, 80, 24);
    assert!(
        !seg_text(&panel, "t1").contains("child_body_line_a"),
        "{EXPECT_BODY_HIDDEN}"
    );
}

/// Opening a child shifts every row below it, so the second child has to be
/// found where it now is rather than where it started.
#[test]
fn an_open_child_above_does_not_move_the_click_off_the_child_below() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    render(&mut panel, 80, 24);

    assert!(panel.handle_click(batch_child_row(&panel, 1), area));
    render(&mut panel, 80, 24);
    let both = seg_text(&panel, "t1");
    assert!(both.contains("child_body_line_a"), "{EXPECT_BODY_SHOWN}");
    assert!(both.contains("child_body_line_b"), "{EXPECT_BODY_SHOWN}");
}

/// A batch child is a control, so the pointer has to say so before the press.
#[test]
fn hovering_a_batch_child_marks_that_row() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    let row = batch_child_row(&panel, 1);
    panel.update_hover(row, area.x, area, false, Path::new(NO_PROJECT));
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

const CHILD_MARK_MSG: &str = "every row of a child answers for the same control, so the row the \
    pointer marks is the summary row wherever inside the child it sits";
const CHILD_MARK_QUIET_MSG: &str = "the marked row is the label and not the thing, so it takes the \
    accent a card gives its own header rather than being reversed";

/// A child is folded or whole, so its summary row and its body are one control
/// and a press anywhere in it folds all of it. Marking the hovered line instead
/// aimed the loudest cue in the vocabulary at a row that answers for nothing on
/// its own.
#[test]
fn hovering_a_childs_body_marks_its_summary_row() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    let summary = batch_child_row(&panel, 0);
    assert!(panel.handle_click(summary, area), "{CHILD_MARK_MSG}");
    render(&mut panel, 80, 24);

    panel.update_hover(summary, area.x, area, false, Path::new(NO_PROJECT));
    let from_summary = hovered_feedback(&panel);
    panel.update_hover(summary + 1, area.x, area, false, Path::new(NO_PROJECT));

    assert_eq!(from_summary, hovered_feedback(&panel), "{CHILD_MARK_MSG}");
}

fn hovered_feedback(panel: &MessagesPanel) -> HoverFeedback {
    match &panel.hover {
        Some(HoverTarget::Tool { feedback, .. }) => *feedback,
        other => panic!("{CHILD_MARK_MSG}: {other:?}"),
    }
}

/// The same rule a card's header follows, one level down. Reverse video on a
/// body line read as "this line is the control", which it is not.
#[test]
fn a_marked_child_row_is_accented_not_reversed() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    let summary = batch_child_row(&panel, 0);
    panel.update_hover(summary, area.x, area, false, Path::new(NO_PROJECT));
    let terminal = render(&mut panel, area.width, area.height);

    let marked = style_of(&terminal, &format!("{FILE_READ_TOOL_NAME} ran"));
    let sibling = style_of(&terminal, &format!("{FILE_GREP_TOOL_NAME} ran"));
    assert!(
        !marked.add_modifier.contains(Modifier::REVERSED),
        "{CHILD_MARK_QUIET_MSG}"
    );
    assert_ne!(marked.fg, sibling.fg, "{CHILD_MARK_QUIET_MSG}");
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

/// A child view names a tool id from the session that is going away, so
/// carrying it into the next one would open whatever inherits the id.
#[test]
fn loading_a_session_forgets_the_child_views() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    assert!(!panel.batch_views.is_empty());

    panel.load_messages(Vec::new());
    assert!(panel.batch_views.is_empty());
}

const CHILD_ACTIVITY_MSG: &str = "a dispatched child says what it is doing while it runs";
const CHILD_TALLY: &str = "2 tools";
const RUNNING_TOOL: &str = "file_grep";
/// The verb the row shows for `RUNNING_TOOL`, which names itself nowhere.
const RUNNING_LABEL: &str = "Grepping";

fn running_child(tool: &str) -> caudra_agent::BatchToolEntry {
    caudra_agent::BatchToolEntry {
        status: caudra_agent::BatchToolStatus::Running,
        output: None,
        ..batch_child(tool, "x")
    }
}

fn child_report() -> SubagentProgress {
    SubagentProgress {
        activity: SubagentActivity::tool(RUNNING_TOOL.into(), "in src"),
        tools: 2,
        elapsed: Duration::from_secs(3),
    }
}

/// A batch running two subagents, neither finished.
fn panel_with_running_batch() -> MessagesPanel {
    let mut panel = panel_with_tools(&[("t1", BATCH_TOOL)]);
    let mut ev = start("t1", BATCH_TOOL);
    ev.output = Some(ToolOutput::Batch {
        entries: vec![running_child("task"), running_child("task")],
        text: String::new(),
    });
    panel.tool_start(ev);
    render(&mut panel, 80, 24);
    panel
}

/// The reported bug: three dispatched subagents sat on the roster saying only
/// that they were delegating. The report is published the whole time, but its
/// id is the batch's own with an index appended, so it named a header that
/// does not exist and was dropped.
#[test]
fn a_dispatched_child_reports_what_it_is_doing() {
    let mut panel = panel_with_running_batch();

    assert!(panel.set_batch_child_progress("t1", 0, child_report()));
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(
        text.contains(RUNNING_LABEL),
        "{CHILD_ACTIVITY_MSG}: {text:?}"
    );
    assert!(text.contains(CHILD_TALLY), "{CHILD_ACTIVITY_MSG}: {text:?}");
}

const CHILD_STREAM_MSG: &str =
    "a dispatched shell streams its output while it runs, as it does on its own";
const CHILD_STREAM_LINES: usize = 40;
const SETTLED_CHILD_MSG: &str = "what a child returned supersedes what it streamed";
const CHILD_SCROLL_MSG: &str =
    "a wheel over a streaming child moves its window, and spills once the window is at an edge";
const CHILD_FOOTER_MSG: &str = "a windowed child reports both edges and which one it is pinned to";
const SETTLED_SCROLL_MSG: &str = "a settled child scrolls the same as a running one";
const ARM_MSG: &str =
    "a window takes the wheel only once pressed, and only while the pointer is in it";
/// Positive is towards the start of the body, which is a lower offset.
const CHILD_SCROLL_UP: i32 = 3;
const CHILD_SCROLL_SPILL: i32 = 4;

fn shell_stream() -> String {
    (0..CHILD_STREAM_LINES)
        .map(|line| format!("line {line}\n"))
        .collect()
}

/// `lines` numbered from zero, so a test can name the head and the tail of a
/// body without counting.
fn numbered_body(lines: usize) -> String {
    (0..lines).map(|line| format!("line {line}\n")).collect()
}

/// A batch running one shell, with nothing back from it yet.
fn panel_with_running_shell() -> MessagesPanel {
    let mut panel = panel_with_tools(&[("t1", BATCH_TOOL)]);
    let mut ev = start("t1", BATCH_TOOL);
    ev.output = Some(ToolOutput::Batch {
        entries: vec![running_child(SHELL_TOOL_NAME)],
        text: String::new(),
    });
    panel.tool_start(ev);
    render(&mut panel, 80, 24);
    panel
}

/// The reported bug: a shell dispatched inside a batch showed nothing until it
/// finished, while the same call on its own streams. A batch keeps the live
/// row for itself and runs each child under an id of its own, so the output
/// named a header that does not exist and was dropped.
#[test]
fn a_dispatched_shell_streams_what_it_is_printing() {
    let mut panel = panel_with_running_shell();

    assert!(panel.set_batch_child_output("t1", 0, &shell_stream()));
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(
        text.contains(&format!("line {}", CHILD_STREAM_LINES - 1)),
        "{CHILD_STREAM_MSG}: {text:?}"
    );
}

/// The window bounds the stream, so a command that prints a thousand lines
/// costs the roster its window and not the thousand. It follows the tail: the
/// newest output is the reason to be watching.
#[test]
fn a_streaming_child_is_held_to_its_window() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    let first = CHILD_STREAM_LINES - caudra_config::DEFAULT_SCROLL_CARD_LINES as usize;
    assert!(
        text.contains(&format!("line {first}")),
        "{CHILD_STREAM_MSG}: {text:?}"
    );
    assert!(
        !text.contains(&format!("line {}", first - 1)),
        "{CHILD_STREAM_MSG}: {text:?}"
    );
}

/// Compact takes the stream too. A live body is the one thing that argues for
/// drawing a child, and the mode answers that it asked for a list: the row and
/// its spinner say the call is running, and a press opens it.
#[test]
fn compact_folds_a_streaming_child_too() {
    let mut panel = panel_with_running_shell();
    panel.set_view(ViewMode::Compact);
    panel.set_batch_child_output("t1", 0, &shell_stream());
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(
        !text.contains(&format!("line {}", CHILD_STREAM_LINES - 1)),
        "{CHILD_FOLDS_IN_COMPACT_MSG}: {text:?}"
    );
}

/// A shell folds to its summary row once it answers, which is the settled rule
/// and stays. The stream is dropped with it: keeping it would leave the card
/// drawing output the child has already superseded.
#[test]
fn a_settled_child_drops_what_it_streamed() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    render(&mut panel, 80, 24);

    panel.batch_progress("t1", 0, batch_child(SHELL_TOOL_NAME, "a"));
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(
        !text.contains(&format!("line {}", CHILD_STREAM_LINES - 1)),
        "{SETTLED_CHILD_MSG}: {text:?}"
    );
    assert!(panel.batch_child_output.is_empty(), "{SETTLED_CHILD_MSG}");
}

/// The reported follow-up: the first window appeared and then froze until the
/// command finished. A batch reaches the highlight worker like any other card
/// and a rebuild reuses the cached answer whenever the key matches, so every
/// window after the first was spliced straight back over. The stream cannot be
/// in that key: it moves on every chunk.
#[test]
fn a_streaming_child_keeps_up_with_what_it_is_printing() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, "line 0\n");
    render(&mut panel, 80, 24);
    settle_highlights(&mut panel);

    panel.set_batch_child_output("t1", 0, &shell_stream());
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(
        text.contains(&format!("line {}", CHILD_STREAM_LINES - 1)),
        "{CHILD_STREAM_MSG}: {text:?}"
    );
}

/// A child's window said nothing about itself, while the same call outside a
/// batch reports where its window sits. Without it there is no way to tell
/// output still arriving from output that has stopped.
#[test]
fn a_windowed_child_says_where_its_window_sits() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    render(&mut panel, 80, 24);

    let above = CHILD_STREAM_LINES - caudra_config::DEFAULT_SCROLL_CARD_LINES as usize;
    let text = seg_text(&panel, "t1");
    assert!(
        text.contains(&format!("{above} above")),
        "{CHILD_FOOTER_MSG}: {text:?}"
    );
    assert!(text.contains(FOLLOWING), "{CHILD_FOOTER_MSG}: {text:?}");
}

/// Scrolling up pins the window, and the footer has to say so: that is the
/// difference between a card that has stopped printing and one whose newest
/// output the reader has scrolled away from.
#[test]
fn a_scrolled_child_says_it_is_no_longer_following() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    let terminal = render(&mut panel, 80, 24);

    wheel_child(&mut panel, &terminal, 0, CHILD_SCROLL_UP);
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(text.contains(PAUSED), "{CHILD_FOOTER_MSG}: {text:?}");
    assert!(
        text.contains(&format!("{CHILD_SCROLL_UP} below")),
        "{CHILD_FOOTER_MSG}: {text:?}"
    );
}

/// A batch that has settled no longer renders locally, so it goes back to the
/// highlight worker, whose cache key had no window in it. Every notch rebuilt
/// the card and then had the old offset spliced straight back over it, so
/// scrolling worked while the call ran and stopped the moment it finished.
#[test]
fn a_settled_child_can_still_be_scrolled() {
    let mut panel = panel_with_tools(&[("t1", BATCH_TOOL)]);
    let mut ev = start("t1", BATCH_TOOL);
    ev.output = Some(ToolOutput::Batch {
        entries: vec![caudra_agent::BatchToolEntry {
            output: Some(ToolOutput::Plain(shell_stream().into())),
            ..batch_child(SHELL_TOOL_NAME, "x")
        }],
        text: String::new(),
    });
    panel.tool_start(ev);
    let terminal = render(&mut panel, 80, 24);
    settle_highlights(&mut panel);

    let spilled = wheel_child(&mut panel, &terminal, 0, CHILD_SCROLL_UP);
    render(&mut panel, 80, 24);
    settle_highlights(&mut panel);

    assert_eq!(spilled, 0, "{SETTLED_SCROLL_MSG}");
    let text = seg_text(&panel, "t1");
    let first = CHILD_STREAM_LINES
        - caudra_config::DEFAULT_SCROLL_CARD_LINES as usize
        - CHILD_SCROLL_UP as usize;
    assert!(
        text.contains(&format!("line {first} ")),
        "{SETTLED_SCROLL_MSG}: {text:?}"
    );
}

/// The second reported follow-up: the wheel did nothing over a running child.
/// Its window was sized from settled output, which a child that has not
/// answered does not have, so the card reported nothing to scroll for exactly
/// as long as there was a reason to scroll it.
#[test]
fn a_streaming_child_can_be_scrolled() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    let terminal = render(&mut panel, 80, 24);

    let spilled = wheel_child(&mut panel, &terminal, 0, CHILD_SCROLL_UP);
    render(&mut panel, 80, 24);

    assert_eq!(spilled, 0, "{CHILD_SCROLL_MSG}");
    let text = seg_text(&panel, "t1");
    let first = CHILD_STREAM_LINES
        - caudra_config::DEFAULT_SCROLL_CARD_LINES as usize
        - CHILD_SCROLL_UP as usize;
    assert!(
        text.contains(&format!("line {first} ")),
        "{CHILD_SCROLL_MSG}: {text:?}"
    );
}

/// A card must never trap the reader inside it. Once the window is at an edge
/// the rest of the notches belong to the transcript.
#[test]
fn a_child_at_its_edge_gives_the_rest_of_the_wheel_back() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    let terminal = render(&mut panel, 80, 24);

    let reachable = CHILD_STREAM_LINES - caudra_config::DEFAULT_SCROLL_CARD_LINES as usize;
    let spilled = wheel_child(
        &mut panel,
        &terminal,
        0,
        reachable as i32 + CHILD_SCROLL_SPILL,
    );

    assert_eq!(spilled, CHILD_SCROLL_SPILL, "{CHILD_SCROLL_MSG}");
}

/// The reported problem: a card under the pointer took every notch aimed at
/// the transcript behind it. Hovering is not a statement of intent, so an
/// untouched window has to hand the whole burst back.
#[test]
fn an_unarmed_window_leaves_the_wheel_to_the_transcript() {
    let _clock = FrozenSpinner::at(0);
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    let terminal = render(&mut panel, 80, 24);
    let (column, row) = card_bar_rows(&terminal)[0];
    let before = seg_text(&panel, "t1");

    let spilled = panel.scroll_card_at(column, row, CHILD_SCROLL_UP);
    render(&mut panel, 80, 24);

    assert_eq!(spilled, CHILD_SCROLL_UP, "{ARM_MSG}");
    assert_eq!(seg_text(&panel, "t1"), before, "{ARM_MSG}");
}

/// A press on the header is the card's own control and must not arm anything,
/// or the gesture that closes a card would also claim the wheel.
#[test]
fn a_press_on_the_header_arms_nothing() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    let terminal = render(&mut panel, 80, 24);
    let (column, _) = card_bar_rows(&terminal)[0];
    let header = batch_child_row(&panel, 0);

    assert!(!panel.arm_card_at(column, header), "{ARM_MSG}");
}

/// Arming is released when the pointer leaves, so the reader never has to
/// press somewhere else to give the transcript its wheel back.
#[test]
fn a_window_releases_the_wheel_once_the_pointer_leaves() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    let terminal = render(&mut panel, 80, 24);
    let (column, row) = card_bar_rows(&terminal)[0];
    assert!(panel.arm_card_at(column, row), "{ARM_MSG}");

    panel.update_hover(0, 0, Rect::new(0, 0, 80, 24), false, Path::new("/"));

    assert_eq!(
        panel.scroll_card_at(column, row, CHILD_SCROLL_UP),
        CHILD_SCROLL_UP,
        "{ARM_MSG}"
    );
}

const CARD_BAR_MSG: &str = "a window inside a card carries a bar, as the transcript does";
const CARD_BAR_DRAG_MSG: &str = "dragging a card's bar moves that window and nothing else";
const CARD_BAR_SWEPT_MSG: &str = "a bar outlives neither its window nor the drag anchored to it";

/// Thumb rows of every bar drawn inside a card body, by column. The
/// transcript's own bar is excluded: it sits in the last column of the
/// viewport, and a card's sits in the last column of its body.
fn card_bar_rows(terminal: &ratatui::Terminal<TestBackend>) -> Vec<(u16, u16)> {
    let buf = terminal.backend().buffer();
    (0..buf.area.height)
        .flat_map(|y| (0..buf.area.width - 1).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            buf.cell((x, y))
                .is_some_and(|c: &ratatui::buffer::Cell| c.symbol() == SCROLLBAR_THUMB)
        })
        .collect()
}

fn press_at(column: u16, row: u16) -> MouseEvent {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

/// A card's body is a window onto something longer, which is the same thing
/// the transcript is, so it says so the same way.
#[test]
fn a_windowed_child_carries_a_bar() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    let terminal = render(&mut panel, 80, 24);

    let bars = card_bar_rows(&terminal);
    assert!(!bars.is_empty(), "{CARD_BAR_MSG}");
    let columns: Vec<u16> = bars.iter().map(|&(x, _)| x).collect();
    assert!(
        columns.windows(2).all(|pair| pair[0] == pair[1]),
        "{CARD_BAR_MSG}: one column, got {columns:?}"
    );
}

/// A body that fits has no window, so a bar there would claim a hidden
/// remainder that does not exist.
#[test]
fn a_child_that_fits_carries_no_bar() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, "one line\n");
    let terminal = render(&mut panel, 80, 24);

    assert!(card_bar_rows(&terminal).is_empty(), "{CARD_BAR_MSG}");
}

/// The press has to reach the child's window rather than the transcript or a
/// selection sweep, and it has to move that window absolutely: a press on a
/// track names a position, not a delta. The top of the track is the start of
/// the body, wherever the thumb happened to be sitting.
#[test]
fn pressing_a_childs_bar_moves_its_window() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    let terminal = render(&mut panel, 80, 24);
    let (column, _) = card_bar_rows(&terminal)[0];
    let track_top = batch_child_row(&panel, 0) + 1;

    assert!(
        panel.handle_card_scrollbar(&press_at(column, track_top)),
        "{CARD_BAR_DRAG_MSG}"
    );
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(text.contains("line 0 "), "{CARD_BAR_DRAG_MSG}: {text:?}");
    assert!(text.contains(PAUSED), "{CARD_BAR_DRAG_MSG}: {text:?}");
}

/// The thumb is where the window already is, so grabbing it must not move
/// anything: that is what makes a drag start from where the reader is looking
/// instead of jumping under the pointer.
#[test]
fn grabbing_a_childs_thumb_leaves_the_window_alone() {
    let _clock = FrozenSpinner::at(0);
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    let terminal = render(&mut panel, 80, 24);
    let before = seg_text(&panel, "t1");
    let (column, row) = card_bar_rows(&terminal)[0];

    assert!(
        panel.handle_card_scrollbar(&press_at(column, row)),
        "{CARD_BAR_DRAG_MSG}"
    );
    render(&mut panel, 80, 24);

    assert_eq!(seg_text(&panel, "t1"), before, "{CARD_BAR_DRAG_MSG}");
}

/// Placing a bar is also what clears its track, so a window that closes has
/// to take its bar with it or a drag stays anchored to rows nothing draws.
#[test]
fn a_closed_window_takes_its_bar_with_it() {
    let mut panel = panel_with_running_shell();
    panel.set_batch_child_output("t1", 0, &shell_stream());
    render(&mut panel, 80, 24);
    assert!(!panel.card_bars.is_empty(), "{CARD_BAR_SWEPT_MSG}");

    panel.batch_progress("t1", 0, batch_child(SHELL_TOOL_NAME, "a"));
    render(&mut panel, 80, 24);

    assert!(panel.card_bars.is_empty(), "{CARD_BAR_SWEPT_MSG}");
}

/// The index has to name a child of this batch, for the same reason a report
/// does: output addressed to a roster that has already gone would install a
/// tail against a card that cannot draw it, and the caller falls back to the
/// header path on a `false`.
#[test_case(0, true ; "a child of the roster takes it")]
#[test_case(9, false ; "an index past the roster does not")]
fn streamed_output_is_only_taken_for_a_child_that_exists(index: usize, expected: bool) {
    let mut panel = panel_with_running_shell();
    assert_eq!(
        panel.set_batch_child_output("t1", index, &shell_stream()),
        expected
    );
}

/// The index has to name a child of this batch. A report for a roster that has
/// already gone would otherwise install a row against a card that cannot draw
/// it, and the caller falls back to the header path on a `false`.
#[test_case(0, true ; "a child of the roster takes it")]
#[test_case(9, false ; "an index past the roster does not")]
fn a_report_is_only_taken_for_a_child_that_exists(index: usize, expected: bool) {
    let mut panel = panel_with_running_batch();
    assert_eq!(
        panel.set_batch_child_progress("t1", index, child_report()),
        expected
    );
}

/// What a subagent was doing is stale the moment it stops. What it did is the
/// only record of the work its output does not show, so the tally stays.
#[test]
fn a_settled_child_keeps_its_tally_and_drops_its_activity() {
    let mut panel = panel_with_running_batch();
    panel.set_batch_child_progress("t1", 0, child_report());
    render(&mut panel, 80, 24);

    panel.batch_progress("t1", 0, batch_child("task", "a"));
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(
        !text.contains(RUNNING_TOOL),
        "{CHILD_ACTIVITY_MSG}: {text:?}"
    );
    assert!(text.contains(CHILD_TALLY), "{CHILD_ACTIVITY_MSG}: {text:?}");
}

/// The row sits between a child's header and its body, so every row below it
/// shifts by one. A click that lands on the wrong child is what the Lua
/// original had to correct for too.
#[test]
fn a_progress_row_belongs_to_the_child_it_reports_on() {
    let mut panel = panel_with_running_batch();
    panel.set_batch_child_progress("t1", 0, child_report());
    render(&mut panel, 80, 24);

    let first = batch_child_row(&panel, 0);
    let second = batch_child_row(&panel, 1);
    assert_eq!(
        second - first,
        2,
        "the reporting child owns its header and the row under it"
    );
}

/// The trap this design had to avoid. A batch reaches the highlight worker
/// like any other card, and a rebuild reuses the cached answer whenever the
/// key still matches. Progress is not in that key and could not usefully be:
/// it moves on every report. So a second report would rebuild the card and
/// then splice the first report's rows straight back over it, freezing the
/// row on whatever the child was doing when the highlight was cached.
#[test]
fn a_later_report_is_not_overwritten_by_the_cached_one() {
    const LATER_TOOL: &str = "file_read";
    const LATER_LABEL: &str = "Reading";
    let mut panel = panel_with_running_batch();
    panel.set_batch_child_progress("t1", 0, child_report());
    render(&mut panel, 80, 24);
    settle_highlights(&mut panel);
    render(&mut panel, 80, 24);

    panel.set_batch_child_progress(
        "t1",
        0,
        SubagentProgress {
            activity: SubagentActivity::tool(LATER_TOOL.into(), "lib.rs"),
            ..child_report()
        },
    );
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(text.contains(LATER_LABEL), "{CHILD_ACTIVITY_MSG}: {text:?}");
    assert!(
        !text.contains(RUNNING_LABEL),
        "{CHILD_ACTIVITY_MSG}: {text:?}"
    );
}

/// Opening a child is a choice about the body, like the raw/filtered switch,
/// so the reset a mode change performs on every disclosure must leave it
/// alone.
#[test]
fn changing_mode_keeps_a_batch_child_open() {
    let mut panel = panel_with_batch();
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    render(&mut panel, 80, 24);

    panel.set_view(ViewMode::Compact);
    render(&mut panel, 80, 24);
    panel.set_view(ViewMode::Expanded);
    render(&mut panel, 80, 24);
    let text = seg_text(&panel, "t1");
    assert!(text.contains("child_body_line_a"), "{EXPECT_BODY_SHOWN}");
    assert!(!text.contains("child_body_line_b"), "{EXPECT_OTHERS_KEPT}");
}

const CLOSED_CARD_SHRINKS_MSG: &str = "a card auto closes must give its rows back at once";
const SPACER_SETUP_MSG: &str = "the test must start from a card with a body to give up";
const HELD_BODY_LINES: usize = 5;

fn auto_panel_with_body(lines: usize) -> MessagesPanel {
    let mut panel = panel_with_tools(&[("t1", CODE_MAP_TOOL_NAME)]);
    panel.tool_done(long_done("t1", lines));
    rebuild(&mut panel);
    panel
}

fn supersede(panel: &mut MessagesPanel) {
    panel.tool_start(start("t2", CODE_MAP_TOOL_NAME));
    panel.tool_done(done("t2"));
    rebuild(panel);
}

/// The card used to hold its rows as blanks so nothing moved under the
/// reader. Those rows still answered clicks and hover as the card that had
/// given them up, and the transcript kept space it was not drawing into, so
/// the card now shrinks to its header the moment auto closes it.
#[test]
fn a_card_auto_closes_gives_its_rows_back_at_once() {
    let mut panel = auto_panel_with_body(HELD_BODY_LINES);
    assert!(panel.segment_heights()[0] > 1, "{SPACER_SETUP_MSG}");

    supersede(&mut panel);

    assert!(panel.card_closed("t1"), "{AUTO_HANDOFF_MSG}");
    assert_eq!(panel.segment_heights()[0], 1, "{CLOSED_CARD_SHRINKS_MSG}");
}

/// Opening by hand shows the whole body, and it has to match a card that was
/// never closed at all rather than carry anything over from the close.
#[test]
fn opening_a_closed_card_by_hand_matches_a_card_that_never_closed() {
    let area = Rect::new(0, 0, 80, 24);
    let mut fresh = auto_panel_with_body(HELD_BODY_LINES);
    assert!(fresh.handle_click(0, area), "{COMPACT_CLICK_MSG}");
    rebuild(&mut fresh);
    let opened = fresh.segment_heights()[0];

    let mut panel = auto_panel_with_body(HELD_BODY_LINES);
    supersede(&mut panel);

    assert!(panel.handle_click(0, area), "{COMPACT_CLICK_MSG}");
    rebuild(&mut panel);

    assert_eq!(panel.segment_heights()[0], opened, "{COMPACT_CLICK_MSG}");
}

const STREAMING_FLUSH_CLICK_MSG: &str = "a live thought must open on the row it is drawn at";
const LIVE_THOUGHT_LABEL: &str = "Thinking";
const STREAMING_WRAPPED_GAP_MSG: &str =
    "a live thought must take the same air after a wrapped row as a settled one";

fn wrapping_tool(panel: &mut MessagesPanel, id: &'static str) {
    let mut long = start(id, FILE_READ_TOOL_NAME);
    long.summary = "a/very/deeply/nested/path/that/has/to/wrap/at/this/width.md".into();
    panel.tool_start(long);
    panel.tool_done(done(id));
}

/// Hit testing counted the separator itself instead of asking what was
/// drawn, so a thought that sat anywhere but where it guessed swallowed every
/// click. Locating the row in the rendered buffer is the only assertion that
/// cannot drift from the spacing rule.
#[test_case(ViewMode::Compact ; "compact")]
#[test_case(ViewMode::Auto ; "auto")]
fn a_live_thought_opens_on_the_row_it_is_drawn_at(view: ViewMode) {
    let mut panel = MessagesPanel::new(
        UiConfig {
            show_thinking: false,
            ..UiConfig::default()
        },
        EventHandle::disconnected_for_test(),
    );
    panel.set_view(view);
    panel.tool_start(start("t1", FILE_GREP_TOOL_NAME));
    panel.tool_done(done("t1"));
    panel.thinking_delta("weighing options");
    let terminal = render(&mut panel, WIDE_ENOUGH_TO_NOT_WRAP, 24);
    let drawn = buffer_text(&terminal)
        .lines()
        .position(|line| line.contains(LIVE_THOUGHT_LABEL))
        .expect("the live thought must be drawn") as u16;

    assert!(
        panel.handle_click(drawn, Rect::new(0, 0, WIDE_ENOUGH_TO_NOT_WRAP, 24)),
        "{STREAMING_FLUSH_CLICK_MSG}"
    );
    assert!(
        panel.streaming_reasoning_open(),
        "{STREAMING_FLUSH_CLICK_MSG}"
    );
}

#[test]
fn a_live_thought_after_a_wrapped_row_keeps_its_separator() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Compact);
    wrapping_tool(&mut panel, "t1");
    render(&mut panel, NARROW_ENOUGH_TO_WRAP, 24);
    let settled = panel.last_total_lines;

    panel.thinking_delta("weighing options");
    render(&mut panel, NARROW_ENOUGH_TO_WRAP, 24);

    assert!(
        panel.last_total_lines > settled + 1,
        "{STREAMING_WRAPPED_GAP_MSG} (settled: {settled}, now: {})",
        panel.last_total_lines
    );
}

const STALE_ROSTER_MSG: &str =
    "a settled batch must draw the children it really ran, not the roster it started from";

/// The async highlight hands its result back and the next rebuild may reuse
/// it. Nothing reuses anything until that lands, so a test that skips it
/// exercises the fresh path and proves nothing about the cached one.
fn settle_highlights(panel: &mut MessagesPanel) {
    for seg in panel.cache.segments_mut() {
        seg.settle_highlight();
    }
}

fn pending_child(tool: &str) -> caudra_agent::BatchToolEntry {
    caudra_agent::BatchToolEntry {
        model_suffix: None,
        tool: tool.into(),
        effect: effect_of(tool),
        summary: String::new(),
        status: caudra_agent::BatchToolStatus::Pending,
        input: None,
        raw_input: None,
        output: None,
        annotation: None,
    }
}

/// The reported bug: a finished batch drew three pending rows with no titles,
/// and only a click put the real roster on screen.
#[test]
fn a_batch_that_finished_does_not_draw_the_roster_it_started_with() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let mut ev = start("t1", BATCH_TOOL);
    ev.output = Some(ToolOutput::Batch {
        entries: vec![pending_child("file_read"), pending_child("file_read")],
        text: String::new(),
    });
    panel.tool_start(ev);
    render(&mut panel, 80, 24);
    settle_highlights(&mut panel);

    panel.batch_progress("t1", 0, batch_child("file_read", "a"));
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(
        text.contains("file_read ran"),
        "{STALE_ROSTER_MSG}: {text:?}"
    );
}

/// The reported gap: a batch spent its whole stream as the bare word
/// `Batching` and then produced every child at once. The roster read out of
/// the still-arriving arguments draws through the same path the dispatched
/// one does, so a child reads the same before it runs as after.
#[test]
fn a_streamed_roster_draws_its_children_before_the_batch_runs() {
    const STREAMED_PATH: &str = "src/streamed.rs";
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending("t1".into(), BATCH_TOOL);
    let mut child = pending_child("file_read");
    child.summary = STREAMED_PATH.into();
    panel.tool_input_roster("t1", Some(vec![child]));
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(text.contains(STREAMED_PATH), "{text:?}");
    assert!(text.contains(QUEUED_MARK), "{text:?}");
}

/// The streamed roster stands in for one the batch has not published yet, so
/// the real one has to displace it rather than survive alongside it.
#[test]
fn a_streamed_roster_gives_way_to_the_dispatched_one() {
    const STREAMED_PATH: &str = "src/streamed.rs";
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending("t1".into(), BATCH_TOOL);
    let mut child = pending_child("file_read");
    child.summary = STREAMED_PATH.into();
    panel.tool_input_roster("t1", Some(vec![child]));
    render(&mut panel, 80, 24);
    settle_highlights(&mut panel);

    let mut ev = start("t1", BATCH_TOOL);
    ev.output = Some(ToolOutput::Batch {
        entries: vec![pending_child("file_grep")],
        text: String::new(),
    });
    panel.tool_start(ev);
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(!text.contains(STREAMED_PATH), "{text:?}");
}

#[test]
fn a_roster_for_an_unknown_call_is_ignored() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_input_roster("t1", Some(vec![pending_child("file_read")]));
    assert!(panel.messages.is_empty());
}

fn eager_entry(status: BatchToolStatus) -> BatchToolEntry {
    BatchToolEntry {
        model_suffix: None,
        tool: SHELL_TOOL_NAME.into(),
        effect: ToolEffect::Mutating,
        summary: EAGER_SUMMARY.into(),
        status,
        input: Some(ToolInput::Script {
            language: "bash".into(),
            code: EAGER_SUMMARY.into(),
        }),
        raw_input: Some(serde_json::json!({ "command": EAGER_SUMMARY })),
        output: Some(ToolOutput::Plain(EAGER_BODY.into())),
        annotation: Some(EAGER_ANNOTATION.into()),
    }
}

fn roster(panel: &MessagesPanel) -> &[BatchToolEntry] {
    let Some(ToolOutput::Batch { entries, .. }) = panel.messages[0].tool_output.as_deref() else {
        panic!("{STALE_ROSTER_MSG}");
    };
    entries
}

#[test_case(BatchToolStatus::Running, ViewMode::Compact ; "running_compact")]
#[test_case(BatchToolStatus::Success, ViewMode::Compact ; "success_compact")]
#[test_case(BatchToolStatus::Error, ViewMode::Compact ; "error_compact")]
#[test_case(BatchToolStatus::Running, ViewMode::Expanded ; "running_expanded")]
#[test_case(BatchToolStatus::Success, ViewMode::Expanded ; "success_expanded")]
#[test_case(BatchToolStatus::Error, ViewMode::Expanded ; "error_expanded")]
fn streamed_rosters_and_parent_start_preserve_execution(status: BatchToolStatus, view: ViewMode) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(view);
    panel.tool_pending(TOOL_ID.into(), BATCH_TOOL);
    panel.tool_input_roster(TOOL_ID, Some(vec![pending_child("alias")]));
    panel.batch_progress(TOOL_ID, 0, eager_entry(BatchToolStatus::Running));
    panel.set_batch_child_output(TOOL_ID, 0, EAGER_BODY);
    panel.set_batch_child_progress(TOOL_ID, 0, child_report());
    panel.batch_progress(TOOL_ID, 0, eager_entry(status));
    panel.close_tool_card(TOOL_ID);
    panel.toggle_batch_child(TOOL_ID, 0);

    panel.tool_input_roster(
        TOOL_ID,
        Some(vec![
            pending_child("alias"),
            pending_child(FILE_READ_TOOL_NAME),
        ]),
    );
    panel.tool_input_roster(TOOL_ID, Some(vec![pending_child("alias")]));
    for snapshot_status in [BatchToolStatus::Pending, BatchToolStatus::Running] {
        let mut stale = pending_child("alias");
        stale.status = snapshot_status;
        let mut event = start(TOOL_ID, BATCH_TOOL);
        event.output = Some(ToolOutput::Batch {
            entries: vec![stale, pending_child(FILE_READ_TOOL_NAME)],
            text: String::new(),
        });
        panel.tool_start(event);
        panel.batch_progress(TOOL_ID, 0, pending_child("alias"));
        render(&mut panel, 100, 40);

        assert_eq!(roster(&panel).len(), 2);
        assert_eq!(
            serde_json::to_value(&roster(&panel)[0]).unwrap(),
            serde_json::to_value(eager_entry(status)).unwrap(),
            "{STALE_ROSTER_MSG}"
        );
        assert_eq!(roster(&panel)[1].status, BatchToolStatus::Pending);
        assert_eq!(
            panel.batch_child_stream(TOOL_ID, 0),
            (!status.is_terminal()).then_some(EAGER_BODY)
        );
        assert_eq!(
            panel.batch_child_progress[TOOL_ID][&0].is_live(),
            !status.is_terminal()
        );
        assert!(panel.card_closed(TOOL_ID));
        assert!(!panel.batch_views.is_empty());
    }
}

#[test_case(None ; "running")]
#[test_case(Some(false) ; "success")]
#[test_case(Some(true) ; "error")]
fn late_top_level_previews_do_not_replace_execution(terminal: Option<bool>) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending(TOOL_ID.into(), SHELL_TOOL_NAME);
    panel.tool_input_body(TOOL_ID, Some(EAGER_SUMMARY.into()));
    let entry = eager_entry(BatchToolStatus::Running);
    let mut event = start(TOOL_ID, SHELL_TOOL_NAME);
    event.summary = entry.summary;
    event.input = entry.input;
    event.raw_input = entry.raw_input;
    event.output = entry.output;
    event.annotation = entry.annotation;
    panel.tool_start(event);
    panel.tool_output(TOOL_ID, EAGER_BODY);
    if let Some(is_error) = terminal {
        panel.tool_done(ToolDoneEvent {
            is_error,
            ..shell_done(TOOL_ID, false)
        });
    }
    let before = panel.messages[0].clone();
    let owed = panel.dirty_cards.clone();
    panel.tool_input_preview(
        TOOL_ID,
        Some(REASONING_BODY.into()),
        Some(REASONING_BODY.into()),
    );
    panel.tool_input_body(TOOL_ID, Some(REASONING_BODY.into()));
    panel.tool_input_roster(TOOL_ID, Some(vec![pending_child("alias")]));
    panel.tool_pending(TOOL_ID.into(), "alias");
    if terminal.is_some() {
        panel.tool_start(start(TOOL_ID, "alias"));
        panel.tool_output(TOOL_ID, REASONING_BODY);
        panel.tool_annotation(TOOL_ID, REASONING_BODY.into());
        panel.set_tool_progress(TOOL_ID, child_report());
    }
    let after = &panel.messages[0];
    assert_eq!(panel.messages.len(), 1);
    assert_eq!(after.role, before.role);
    assert_eq!(after.text, before.text);
    assert_eq!(after.annotation, before.annotation);
    assert_eq!(after.tool_input, before.tool_input);
    assert_eq!(after.tool_raw_input, before.tool_raw_input);
    assert_eq!(after.live_output, before.live_output);
    assert_eq!(
        serde_json::to_value(&after.tool_output).unwrap(),
        serde_json::to_value(&before.tool_output).unwrap()
    );
    assert!(after.live_body.is_none());
    assert!(after.progress.is_none());
    assert_eq!(panel.dirty_cards, owed, "{NO_LATE_REDRAW_MSG}");
}

const NO_LATE_REDRAW_MSG: &str = "a preview for a call that has moved on owes its card nothing, \
    so it cannot be redrawn from state the execution replaced";

#[test_case(false ; "child_completion")]
#[test_case(true ; "parent_cancellation")]
fn child_live_buffers_and_annotations_do_not_resurrect(cancel: bool) {
    let mut panel = panel_with_running_shell();
    let body = Arc::new(SharedBuf::new());
    body.set_lines(vec![snap_line(EAGER_BODY)]);
    panel.register_live_buf(EAGER_CHILD_ID.into(), Arc::clone(&body));
    panel.tool_annotation(EAGER_CHILD_ID, EAGER_ANNOTATION.into());
    panel.set_batch_child_progress(TOOL_ID, 0, child_report());
    let _ = panel.poll_live_bufs();
    assert_eq!(panel.batch_child_stream(TOOL_ID, 0), Some(EAGER_BODY));
    assert_eq!(
        roster(&panel)[0].annotation.as_deref(),
        Some(EAGER_ANNOTATION)
    );
    if cancel {
        panel.cancel_in_progress();
    } else {
        panel.batch_progress(TOOL_ID, 0, eager_entry(BatchToolStatus::Success));
    }
    panel.register_live_buf(EAGER_CHILD_ID.into(), Arc::clone(&body));
    body.set_lines(vec![snap_line(REASONING_BODY)]);
    panel.tool_annotation(EAGER_CHILD_ID, REASONING_BODY.into());
    panel.tool_snapshot(
        EAGER_CHILD_ID,
        BufferSnapshot::plain_text(REASONING_BODY.into()),
        None,
    );
    panel.set_batch_child_output(TOOL_ID, 0, REASONING_BODY);
    panel.set_batch_child_progress(TOOL_ID, 0, child_report());
    panel.batch_progress(TOOL_ID, 0, running_child(SHELL_TOOL_NAME));
    let _ = panel.poll_live_bufs();
    assert!(!panel.live_bufs.contains_key(EAGER_CHILD_ID));
    assert!(panel.batch_child_stream(TOOL_ID, 0).is_none());
    assert!(!panel.batch_child_progress[TOOL_ID][&0].is_live());
    assert_eq!(
        roster(&panel)[0].annotation.as_deref(),
        Some(EAGER_ANNOTATION)
    );
    assert_eq!(
        roster(&panel)[0].status,
        if cancel {
            BatchToolStatus::Error
        } else {
            BatchToolStatus::Success
        }
    );
}

#[test_case(false ; "pending_parent_snapshot")]
#[test_case(true ; "running_parent_snapshot")]
fn parent_snapshot_completion_retires_child_live_buffers(started: bool) {
    let mut panel = panel_with_running_shell();
    let body = Arc::new(SharedBuf::new());
    panel.register_live_buf(EAGER_CHILD_ID.into(), Arc::clone(&body));
    panel.set_batch_child_progress(TOOL_ID, 0, child_report());
    let mut event = start(TOOL_ID, BATCH_TOOL);
    event.output = Some(ToolOutput::Batch {
        entries: vec![eager_entry(BatchToolStatus::Success)],
        text: String::new(),
    });
    panel.tool_start(event);
    let stale = if started {
        running_child(SHELL_TOOL_NAME)
    } else {
        pending_child(SHELL_TOOL_NAME)
    };
    panel.batch_progress(TOOL_ID, 0, stale);
    assert_eq!(roster(&panel)[0].status, BatchToolStatus::Success);
    assert!(!panel.live_bufs.contains_key(EAGER_CHILD_ID));
    assert!(!panel.batch_child_progress[TOOL_ID][&0].is_live());
}

#[test_case(false ; "success")]
#[test_case(true ; "error")]
fn native_shell_completion_retires_live_buffers(is_error: bool) {
    let mut panel = panel_with_tools(&[(TOOL_ID, SHELL_TOOL_NAME)]);
    let body = Arc::new(SharedBuf::new());
    body.set_lines(vec![snap_line(EAGER_BODY)]);
    panel.register_live_buf(TOOL_ID.into(), Arc::clone(&body));
    panel.tool_done(ToolDoneEvent {
        is_error,
        ..shell_done(TOOL_ID, false)
    });
    panel.register_live_buf(TOOL_ID.into(), Arc::clone(&body));
    body.set_lines(vec![snap_line(REASONING_BODY)]);
    panel.tool_snapshot(
        TOOL_ID,
        BufferSnapshot::plain_text(REASONING_BODY.into()),
        None,
    );
    let _ = panel.poll_live_bufs();
    assert!(panel.live_bufs.is_empty());
    assert!(panel.watched_bufs.is_empty());
    assert_eq!(panel.messages[0].live_output.as_deref(), Some(EAGER_BODY));
    assert!(panel.messages[0].render_snapshot.is_none());
}

const FOLD_MARK: &str = "\u{2026}";
const QUEUED_MARK: &str = "(queued)";

/// The mark says a body is being withheld, so a child with nothing to withhold
/// must not carry it. A dispatched child that has not answered yet drew one,
/// which reads as a result the reader is being kept from. A child that draws
/// its own body is withholding nothing either.
#[test_case(FILE_READ_TOOL_NAME, true, true ; "a folded child offers its body")]
#[test_case(FILE_READ_TOOL_NAME, false, false ; "a folded child with none promises nothing")]
#[test_case(TASK_TOOL_NAME, true, false ; "a child drawing its own body hides none of it")]
fn only_a_child_hiding_something_says_so(tool: &str, has_output: bool, expected: bool) {
    let mut panel = panel_with_tools(&[("t1", BATCH_TOOL)]);
    let mut child = batch_child(tool, "a");
    if !has_output {
        child.output = None;
    }
    panel.tool_done(ToolDoneEvent {
        tool: BATCH_TOOL.into(),
        output: ToolOutput::Batch {
            entries: vec![child],
            text: String::new(),
        },
        ..done("t1")
    });
    render(&mut panel, 80, 24);

    assert_eq!(
        seg_text(&panel, "t1").contains(FOLD_MARK),
        expected,
        "the fold mark promises a body"
    );
}

/// A batch cut short leaves children that never ran. They are drawn in the
/// same plain tense a failure is, so with nothing to separate them the roster
/// reads as a batch that went wrong rather than one that stopped early.
#[test_case(caudra_agent::BatchToolStatus::Pending, true ; "queued")]
#[test_case(caudra_agent::BatchToolStatus::Error, false ; "failed")]
fn a_child_that_never_ran_is_not_read_as_one_that_failed(
    status: caudra_agent::BatchToolStatus,
    expected: bool,
) {
    let mut panel = panel_with_tools(&[("t1", BATCH_TOOL)]);
    let child = caudra_agent::BatchToolEntry {
        status,
        output: None,
        ..batch_child("task", "a")
    };
    panel.tool_done(ToolDoneEvent {
        tool: BATCH_TOOL.into(),
        output: ToolOutput::Batch {
            entries: vec![child],
            text: String::new(),
        },
        ..done("t1")
    });
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert_eq!(
        text.contains(QUEUED_MARK),
        expected,
        "a queued child says so: {text:?}"
    );
}

/// A child answering in markdown was shown as source, so a subagent's report
/// arrived with its syntax on screen instead of what it said. Every other
/// body here is painted; this one was lumped in with plain text.
#[test]
fn a_child_that_answered_in_markdown_is_painted_not_quoted() {
    const HEADING: &str = "Findings";
    let mut panel = panel_with_tools(&[("t1", BATCH_TOOL)]);
    let child = caudra_agent::BatchToolEntry {
        output: Some(ToolOutput::Markdown(
            format!("## {HEADING}\n\nthe **answer**").into(),
        )),
        ..batch_child("task", "a")
    };
    panel.tool_done(ToolDoneEvent {
        tool: BATCH_TOOL.into(),
        output: ToolOutput::Batch {
            entries: vec![child],
            text: String::new(),
        },
        ..done("t1")
    });
    let area = Rect::new(0, 0, 80, 24);
    render(&mut panel, 80, 24);
    assert!(panel.handle_click(batch_child_row(&panel, 0), area));
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(text.contains(HEADING), "the heading survives: {text:?}");
    assert!(!text.contains("##"), "the syntax does not: {text:?}");
    assert!(!text.contains("**"), "the syntax does not: {text:?}");
}

const CHILD_ARGS_MSG: &str = "a batch child names the inputs its header omits, as a row does";

/// A child row showed only the header, so the batch hid exactly the arguments
/// that distinguish one call from the next: three reads of three files all
/// read as the same row.
#[test]
fn a_batch_child_lists_the_inputs_its_header_omits() {
    let mut panel = panel_with_tools(&[("t1", BATCH_TOOL)]);
    let mut child = batch_child(FILE_READ_TOOL_NAME, "a");
    child.summary = "caudra-storage/src/lib.rs".into();
    child.raw_input = Some(serde_json::json!({
        "file_path": "caudra-storage/src/lib.rs",
        "offset": 1,
        "limit": 10,
    }));
    panel.tool_done(ToolDoneEvent {
        tool: BATCH_TOOL.into(),
        output: ToolOutput::Batch {
            entries: vec![child],
            text: String::new(),
        },
        ..done("t1")
    });
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(text.contains("offset=1"), "{CHILD_ARGS_MSG}: {text:?}");
    assert!(text.contains("limit=10"), "{CHILD_ARGS_MSG}: {text:?}");
    assert!(
        !text.contains("file_path="),
        "the header already shows the path: {text:?}"
    );
}

#[test]
fn a_completed_batch_read_replaces_its_request_with_the_returned_range() {
    const PATH: &str = "caudra-storage/src/lib.rs";
    let mut panel = panel_with_tools(&[("t1", BATCH_TOOL)]);
    let mut child = batch_child(FILE_READ_TOOL_NAME, "a");
    child.summary = PATH.into();
    child.raw_input = Some(serde_json::json!({
        "file_path": PATH,
        "offset": 190,
        "limit": 140,
    }));
    child.output = Some(ToolOutput::ReadCode {
        path: PATH.into(),
        start_line: 190,
        lines: vec!["x".into(); 140],
        total_lines: 668,
        instructions: None,
    });
    panel.tool_done(ToolDoneEvent {
        tool: BATCH_TOOL.into(),
        output: ToolOutput::Batch {
            entries: vec![child],
            text: String::new(),
        },
        ..done("t1")
    });
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(text.contains("lines 190–329 of 668"), "{text:?}");
    assert!(!text.contains("offset="), "{text:?}");
    assert!(!text.contains("limit="), "{text:?}");
}

const SHELL_COLLAPSE_MSG: &str = "a shell card must obey the view like any other read";
const SHELL_HEADER_KEPT_MSG: &str = "a closed shell card still names the command that ran";
const WRITE_STAYS_OPEN_MSG: &str = "outside compact a write must stay open: its diff is the only \
    record of it";
const WRITE_FOLDS_IN_COMPACT_MSG: &str = "compact answers for every tool, so a write folds to its row \
    like anything else and the diff is one click away";

/// Shell is `Mutating`, so it was exempt from every view rule and stayed open
/// forever. It is the highest-volume tool there is, which made compact and
/// auto worth very little in any session that ran commands.
#[test_case(ViewMode::Compact ; "compact closes it")]
#[test_case(ViewMode::Auto ; "auto closes the one behind")]
fn a_shell_card_closes_like_any_other_read(view: ViewMode) {
    let mut panel = panel_with_long_tool(SHELL_TOOL_NAME, TRUNCATING_LINES);
    panel.set_view(view);
    // Auto keeps the newest card open, so give it a newer one to fall behind.
    panel.push(DisplayMessage::new(DisplayRole::Assistant, "done".into()));
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(!text.contains("line 0"), "{SHELL_COLLAPSE_MSG}: {text:?}");
    assert!(text.contains("cmd"), "{SHELL_HEADER_KEPT_MSG}: {text:?}");
}

/// The line the rule draws outside compact: a shell command is named by its
/// own header, but a diff exists nowhere but the body it would be hidden
/// behind. Auto is the case worth pinning, since the write has already fallen
/// behind a newer message and would fold if the mode alone decided.
#[test_case(FILE_WRITE_TOOL_NAME ; "write")]
#[test_case(FILE_EDIT_TOOL_NAME ; "edit")]
fn a_write_ignores_every_view_but_compact(tool: &'static str) {
    let mut panel = panel_with_tools(&[("t1", tool)]);
    panel.tool_done(long_done("t1", HELD_BODY_LINES));
    panel.push(DisplayMessage::new(DisplayRole::Assistant, "done".into()));
    render(&mut panel, 80, 24);

    assert!(
        seg_text(&panel, "t1").contains("line 0"),
        "{WRITE_STAYS_OPEN_MSG}"
    );
}

/// Compact is the exception, and it is the whole point of the mode: a session
/// of writes was unskimmable while every diff drew itself.
#[test_case(FILE_WRITE_TOOL_NAME ; "write")]
#[test_case(FILE_EDIT_TOOL_NAME ; "edit")]
fn a_write_folds_in_compact(tool: &'static str) {
    let mut panel = compact_panel(&[("t1", tool)]);
    panel.tool_done(long_done("t1", HELD_BODY_LINES));
    render(&mut panel, 80, 24);

    assert!(
        !seg_text(&panel, "t1").contains("line 0"),
        "{WRITE_FOLDS_IN_COMPACT_MSG}"
    );
}

/// The same read wrapped by an MCP server, which is how a call arrives once a
/// server stands between the model and the tool.
const QUALIFIED_READ_TOOL_NAME: &str = "mcp_File_read";
const COLLAPSED_BODY_LINES: usize = 30;
const ONE_ROW: &[u16] = &[1];
const STAYS_ONE_ROW_MSG: &str = "a tool on the collapse list is a window into a document, so no \
    view mode may spend the transcript drawing its alphabetically first lines";
const QUALIFIED_MATCH_MSG: &str = "the list is written in bare names, so the same read behind a \
    server qualifier has to fold with it rather than slip through as a tool nobody listed";
const COLLAPSED_CLICK_MSG: &str = "the fold is a default, not a lock: the reader asking for the \
    body is the one thing that opens it";
const COLLAPSED_RECLOSE_MSG: &str = "a card the reader opened by hand has to shut by hand too, or \
    the only way back to the row is a mode change";
const OPT_OUT_MSG: &str = "an empty list is the documented opt-out, so a read goes back to \
    answering the view mode like any other read-only call";

/// A read is `ReadOnly` however it was registered, so the effect is stated
/// rather than looked up: a qualified name is absent from the test table and
/// would otherwise arrive as `Unknown`, which is uncollapsible for a reason
/// that has nothing to do with the collapse list.
fn read_call_panel(config: UiConfig, view: ViewMode, tool: &'static str) -> MessagesPanel {
    let mut panel = MessagesPanel::new(config, EventHandle::disconnected_for_test());
    panel.set_view(view);
    let mut call = start(TOOL_ID, tool);
    call.effect = ToolEffect::ReadOnly;
    panel.tool_start(call);
    panel.tool_done(long_done(TOOL_ID, COLLAPSED_BODY_LINES));
    rebuild(&mut panel);
    panel
}

fn without_collapse_list() -> UiConfig {
    UiConfig {
        always_collapsed: Vec::new(),
        ..UiConfig::default()
    }
}

/// Expanded is the mode that opens everything, and auto opens the newest call,
/// so without this the list only ever proves itself in compact — where every
/// card is closed anyway and the list decides nothing.
#[test_case(ViewMode::Compact ; "compact")]
#[test_case(ViewMode::Auto ; "auto")]
#[test_case(ViewMode::Expanded ; "expanded")]
fn an_always_collapsed_call_stays_one_row_in_every_mode(view: ViewMode) {
    let panel = read_call_panel(UiConfig::default(), view, FILE_READ_TOOL_NAME);

    assert_eq!(panel.segment_heights(), ONE_ROW, "{STAYS_ONE_ROW_MSG}");
}

/// Matching on the bare name alone would let the identical read open a card
/// the moment it came through a server, so the list would quietly stop working
/// for exactly the sessions that make the most tool calls.
#[test]
fn an_always_collapsed_call_folds_under_its_server_qualifier() {
    let panel = read_call_panel(
        UiConfig::default(),
        ViewMode::Expanded,
        QUALIFIED_READ_TOOL_NAME,
    );
    // Without the list the same qualified call opens, so the fold above is the
    // list matching through the qualifier rather than the card having no body.
    let unlisted = read_call_panel(
        without_collapse_list(),
        ViewMode::Expanded,
        QUALIFIED_READ_TOOL_NAME,
    );

    assert_eq!(panel.segment_heights(), ONE_ROW, "{QUALIFIED_MATCH_MSG}");
    assert!(
        unlisted.segment_heights()[0] > 1,
        "{QUALIFIED_MATCH_MSG}: {:?}",
        unlisted.segment_heights()
    );
}

/// `toggle_expansion` is already covered, but the reader has no such function:
/// they press the row. Nothing proves the press reaches the card, and nothing
/// proves a second press gives the row back rather than stalling on the body
/// the way an expanded card without a header to close to does.
#[test]
fn an_always_collapsed_card_opens_on_a_press_and_shuts_on_the_next() {
    let mut panel = read_call_panel(UiConfig::default(), ViewMode::Expanded, FILE_READ_TOOL_NAME);
    let area = Rect::new(0, 0, 80, 24);

    assert!(panel.handle_click(0, area), "{COLLAPSED_CLICK_MSG}");
    rebuild(&mut panel);
    assert!(
        panel.segment_heights()[0] > 1,
        "{COLLAPSED_CLICK_MSG}: {:?}",
        panel.segment_heights()
    );

    assert!(panel.handle_click(0, area), "{COLLAPSED_RECLOSE_MSG}");
    rebuild(&mut panel);
    assert_eq!(panel.segment_heights(), ONE_ROW, "{COLLAPSED_RECLOSE_MSG}");
}

/// The list is a default, and a default nobody can turn off is a bug. An
/// empty list has to reach the panel as empty rather than be read as unset.
#[test_case(ViewMode::Compact, false ; "compact still closes it")]
#[test_case(ViewMode::Auto, true ; "auto opens the newest")]
#[test_case(ViewMode::Expanded, true ; "expanded opens it")]
fn an_empty_collapse_list_hands_a_read_back_to_the_view(view: ViewMode, open: bool) {
    let panel = read_call_panel(without_collapse_list(), view, FILE_READ_TOOL_NAME);

    assert_eq!(panel.segment_heights()[0] > 1, open, "{OPT_OUT_MSG}");
}

/// `/view` is one key, and a reader leaning on it walks the whole cycle in
/// seconds. A mode that opened these calls would make the shortcut unusable in
/// any session that reads files, which is all of them.
#[test]
fn cycling_the_view_leaves_an_always_collapsed_call_at_one_row() {
    let mut panel = read_call_panel(
        UiConfig::default(),
        ViewMode::default(),
        FILE_READ_TOOL_NAME,
    );
    let mut view = ViewMode::default();

    loop {
        view = view.next();
        panel.set_view(view);
        rebuild(&mut panel);
        assert_eq!(
            panel.segment_heights(),
            ONE_ROW,
            "{STAYS_ONE_ROW_MSG}: {view:?}"
        );
        if view == ViewMode::default() {
            break;
        }
    }
}

const WRITE_OPENS_MSG: &str = "a write is not collapsible by mode, so auto opens it too";
const WRITE_OPENS_WHOLE_MSG: &str = "a press is never answered with a slice: the mode drew no body \
    at all, so the one it draws now is the whole file";
const WRITE_SHUTS_MSG: &str = "hiding a diff nobody asked to hide loses the change, but the reader \
    asking is a different thing, and the row still names the file";
const WRITE_REOPENS_MSG: &str = "a card the reader shut has to come back on the next press, or the \
    diff is gone for the rest of the session";
const BODY_HEAD: &str = "line 0";

/// A write was exempt from every collapse rule, including the reader's own
/// press, so a long diff could not be put away at all. Expanded is left out on
/// purpose: it gives no card a header to fall back to, which is a property of
/// the mode rather than of writes. Compact is left out because a write starts
/// folded there, so the same two presses run the other way round.
#[test_case(ViewMode::Auto ; "auto")]
fn a_write_shuts_on_a_press_and_comes_back_on_the_next(view: ViewMode) {
    let mut panel = panel_with_tools(&[(TOOL_ID, FILE_WRITE_TOOL_NAME)]);
    panel.set_view(view);
    panel.tool_done(long_done(TOOL_ID, HELD_BODY_LINES));
    rebuild(&mut panel);
    let area = Rect::new(0, 0, 80, 24);
    assert!(!panel.card_closed(TOOL_ID), "{WRITE_OPENS_MSG}");

    assert!(panel.handle_click(0, area), "{WRITE_SHUTS_MSG}");
    rebuild(&mut panel);
    assert!(panel.card_closed(TOOL_ID), "{WRITE_SHUTS_MSG}");
    assert!(
        !seg_text(&panel, TOOL_ID).contains(BODY_HEAD),
        "{WRITE_SHUTS_MSG}"
    );

    assert!(panel.handle_click(0, area), "{WRITE_REOPENS_MSG}");
    rebuild(&mut panel);
    assert!(!panel.card_closed(TOOL_ID), "{WRITE_REOPENS_MSG}");
    assert!(
        seg_text(&panel, TOOL_ID).contains(BODY_HEAD),
        "{WRITE_REOPENS_MSG}"
    );
}

/// The same two presses in compact, which start from the other end: a write is
/// folded there, and the press that opens it has to give the whole file rather
/// than the seven rows the `write` budget would allow.
#[test]
fn a_folded_write_opens_whole_and_shuts_again() {
    let mut panel = compact_panel(&[(TOOL_ID, FILE_WRITE_TOOL_NAME)]);
    panel.tool_done(body_done(FILE_WRITE_TOOL_NAME, TOOL_ID, WRITTEN_FILE_LINES));
    rebuild(&mut panel);
    let area = Rect::new(0, 0, 80, 24);
    assert!(panel.card_closed(TOOL_ID), "{WRITE_FOLDS_IN_COMPACT_MSG}");

    assert!(panel.handle_click(0, area), "{WRITE_REOPENS_MSG}");
    rebuild(&mut panel);
    let text = seg_text(&panel, TOOL_ID);
    for line in [
        BODY_HEAD.to_owned(),
        format!("line {}", WRITTEN_FILE_LINES - 1),
    ] {
        assert!(text.contains(&line), "{WRITE_OPENS_WHOLE_MSG}: {text:?}");
    }

    assert!(panel.handle_click(0, area), "{WRITE_SHUTS_MSG}");
    rebuild(&mut panel);
    assert!(panel.card_closed(TOOL_ID), "{WRITE_SHUTS_MSG}");
}

const SETTLED_TAIL_MSG: &str = "a command that has answered has no tail, so its window says where \
    it sits and claims nothing about output still to come";
const RUNNING_TAIL_MSG: &str = "while output can still arrive the window says which edge it is \
    pinned to, so the words track the call rather than being gone";
const WINDOWED_BODY_LINES: usize = 40;

/// `following` and `paused` both promise more output. Once the call has
/// answered the promise is false, and the counts are the only part still true.
/// Checked at both edges because `paused · click to follow` lived at one of
/// them and `following` at the other.
#[test_case(0 ; "pinned at the tail")]
#[test_case(CHILD_SCROLL_UP ; "scrolled up")]
fn a_settled_window_says_nothing_about_a_tail(notches: i32) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start(TOOL_ID, SHELL_TOOL_NAME));
    panel.tool_done(long_done(TOOL_ID, WINDOWED_BODY_LINES));
    rebuild(&mut panel);
    let height = WINDOWED_BODY_LINES as u16 + 8;
    let terminal = render(&mut panel, 80, height);
    if notches != 0 {
        let (column, row) = card_bar_rows(&terminal)[0];
        assert!(panel.arm_card_at(column, row), "{ARM_MSG}");
        panel.scroll_card_at(column, row, notches);
    }

    let shown = buffer_text(&render(&mut panel, 80, height));
    assert!(shown.contains("above"), "{SETTLED_TAIL_MSG}: {shown}");
    assert!(!shown.contains(FOLLOWING), "{SETTLED_TAIL_MSG}: {shown}");
    assert!(!shown.contains(PAUSED), "{SETTLED_TAIL_MSG}: {shown}");
}

/// The same card while the command is still printing.
#[test]
fn a_running_window_still_says_which_edge_it_is_on() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start(TOOL_ID, SHELL_TOOL_NAME));
    panel.tool_output(TOOL_ID, &numbered_body(WINDOWED_BODY_LINES));
    rebuild(&mut panel);

    let shown = buffer_text(&render(&mut panel, 80, WINDOWED_BODY_LINES as u16 + 8));
    assert!(shown.contains(FOLLOWING), "{RUNNING_TAIL_MSG}: {shown}");
}

/// A child answers the same question its own card would, so it loses the words
/// on the same terms.
#[test]
fn a_settled_child_says_nothing_about_a_tail_either() {
    let mut panel = panel_with_tools(&[("t1", BATCH_TOOL)]);
    let mut child = batch_child(SHELL_TOOL_NAME, "a");
    child.output = Some(ToolOutput::Plain(numbered_body(WINDOWED_BODY_LINES).into()));
    panel.tool_done(ToolDoneEvent {
        tool: BATCH_TOOL.into(),
        output: ToolOutput::Batch {
            entries: vec![child],
            text: String::new(),
        },
        ..done("t1")
    });
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    assert!(text.contains("above"), "{SETTLED_TAIL_MSG}: {text:?}");
    assert!(!text.contains(FOLLOWING), "{SETTLED_TAIL_MSG}: {text:?}");
    assert!(!text.contains(PAUSED), "{SETTLED_TAIL_MSG}: {text:?}");
}

const SCROLLING_OFF: u32 = 0;
const BUDGET_SHAPE_MSG: &str = "with scrolling off a shell body is abridged again: it keeps its \
    head, drops its tail, and offers the rest behind the notice";
const NO_WINDOW_MSG: &str = "a card that is not a window must not report one, or the footer \
    promises a wheel that goes nowhere";
const UNABRIDGED_WRITE_MSG: &str = "with scrolling off a write has no budget at all, so the file \
    is drawn whole rather than cut at either end";

fn without_card_scrolling() -> UiConfig {
    UiConfig {
        scroll_card_lines: SCROLLING_OFF,
        ..UiConfig::default()
    }
}

fn long_card_text(config: UiConfig, tool: &'static str, lines: usize) -> String {
    let mut panel = MessagesPanel::new(config, EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Expanded);
    panel.tool_start(start(TOOL_ID, tool));
    panel.tool_done(body_done(tool, TOOL_ID, lines));
    rebuild(&mut panel);
    seg_text(&panel, TOOL_ID)
}

/// `0` is the escape hatch for readers who want the old card back, so it has
/// to restore the whole of the old shape and not merely stop scrolling. The
/// two shapes are opposites — a budget keeps the head and hides the tail, a
/// window keeps the tail — so the head and the notice are what tell them apart.
#[test]
fn scrolling_off_returns_a_shell_body_to_its_budget() {
    let budget = UiConfig::default().tool_output_lines.get(SHELL_TOOL_NAME);
    let text = long_card_text(without_card_scrolling(), SHELL_TOOL_NAME, TRUNCATING_LINES);
    let tail = format!("line {}", TRUNCATING_LINES - 1);

    assert!(text.contains(BODY_HEAD), "{BUDGET_SHAPE_MSG}: {text:?}");
    assert!(!text.contains(&tail), "{BUDGET_SHAPE_MSG}: {text:?}");
    assert!(
        text.contains(&crate::markdown::truncation_notice(
            TRUNCATING_LINES - budget
        )),
        "{BUDGET_SHAPE_MSG}: {text:?}"
    );
    assert!(!text.contains(FOLLOWING), "{NO_WINDOW_MSG}: {text:?}");
    assert!(!text.contains(PAUSED), "{NO_WINDOW_MSG}: {text:?}");
}

/// A write's body is the file it wrote, so it is neither windowed nor
/// abridged. Both settings are checked because the point is that
/// `scroll_card_lines` does not reach a write at all: the tool sat in the
/// scroll set once, and a window pinned to the tail hid the head of the file
/// it had just written.
#[test_case(UiConfig::default() ; "at the default window height")]
#[test_case(without_card_scrolling() ; "with scrolling off")]
fn a_write_is_whole_whatever_the_scroll_setting(config: UiConfig) {
    let text = long_card_text(config, FILE_WRITE_TOOL_NAME, WRITTEN_FILE_LINES);
    let tail = format!("line {}", WRITTEN_FILE_LINES - 1);

    assert!(text.contains(BODY_HEAD), "{UNABRIDGED_WRITE_MSG}: {text:?}");
    assert!(text.contains(&tail), "{UNABRIDGED_WRITE_MSG}: {text:?}");
    assert!(
        !text.contains(crate::markdown::EXPAND_AFFORDANCE),
        "{UNABRIDGED_WRITE_MSG}: {text:?}"
    );
    assert!(!text.contains(FOLLOWING), "{NO_WINDOW_MSG}: {text:?}");
}

const WRITE_WHOLE_MSG: &str = "a whole-file write is the file: abridging it to seven rows behind a \
    notice buys a click and hides what the card is for";
const EDIT_BUDGETED_MSG: &str = "a diff is already only the part that changed, so it keeps its \
    tool's row budget";
/// Comfortably past the `write` budget, so a card drawing every row can only
/// be one that spends no budget at all.
const WRITTEN_FILE_LINES: usize = 40;
const WRITTEN_FILE_PATH: &str = "notes.txt";

/// A write draws its file whole, an edit rests at its budget. Both are checked
/// together because they share the one `write` budget in the config, so the
/// rule cannot be the budget itself.
#[test_case(FILE_WRITE_TOOL_NAME, false ; "a_write_draws_the_whole_file")]
#[test_case(FILE_EDIT_TOOL_NAME, true ; "an_edit_rests_at_its_budget")]
fn a_writes_body_is_not_abridged(tool: &'static str, expect_notice: bool) {
    let mut panel = panel_with_tools(&[("t1", tool)]);
    panel.tool_done(body_done(tool, "t1", WRITTEN_FILE_LINES));
    render(&mut panel, 80, 24);

    let text = seg_text(&panel, "t1");
    let message = if expect_notice {
        EDIT_BUDGETED_MSG
    } else {
        WRITE_WHOLE_MSG
    };
    assert_eq!(
        text.contains(crate::markdown::EXPAND_AFFORDANCE),
        expect_notice,
        "{message}: {text:?}"
    );
    assert_eq!(
        text.contains(&format!("line {}", WRITTEN_FILE_LINES - 1)),
        !expect_notice,
        "{message}: {text:?}"
    );
    // A window pinned to the tail satisfies every assertion above, which is
    // how a write stayed abridged unnoticed. Only the head rules one out, and
    // an abridged edit draws its head too.
    assert!(text.contains(BODY_HEAD), "{message}: {text:?}");
}

const LIVE_WHOLE_MSG: &str = "a file arriving is still the file, so every line it has written so \
    far is on screen and no window footer sits under it";
const NO_LIVE_NOTICE_MSG: &str = "nothing is hidden behind a click while the rest of the file has \
    not arrived";
const LIVE_BODY_DRAWN_MSG: &str = "the frame is what draws the body, so the card has grown past \
    its header by the time it is painted";
const PER_FRAME_MSG: &str = "the body is rebuilt once a frame rather than once a fragment: per \
    fragment costs the file's length squared";

/// The live body is what the settled card will rest at, and for a write that
/// is the whole file. Windowing it to the tail hid the head of the file while
/// it was being written, which is the half a reader checks.
#[test]
fn a_streaming_write_is_drawn_whole() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let body: String = (0..WRITTEN_FILE_LINES)
        .map(|i| format!("line {i}\n"))
        .collect();
    streaming_write(&mut panel, &[&body]);

    let shown = buffer_text(&render(&mut panel, 80, WRITTEN_FILE_LINES as u16 + 8));
    for line in [0, WRITTEN_FILE_LINES - 1] {
        assert!(
            shown.contains(&format!("line {line}")),
            "{LIVE_WHOLE_MSG}: {shown}"
        );
    }
    assert!(!shown.contains(FOLLOWING), "{LIVE_WHOLE_MSG}: {shown}");
    assert!(
        !shown.contains(crate::markdown::EXPAND_AFFORDANCE),
        "{NO_LIVE_NOTICE_MSG}: {shown}"
    );
}

const CHILD_SCRIPT: &str = "for f in *.rs\ndo\n  echo $f\ndone";
const CHILD_SCRIPT_TOKENS: [&str; 3] = ["for", "do", "done"];
const CHILD_SCRIPT_MSG: &str =
    "a child draws its script the way its own card does, highlighted and numbered";
const CHILD_ONE_LINER_MSG: &str =
    "a one-line script is drawn in the body and given up by the summary row";
const CHILD_LIVE_SCRIPT_MSG: &str =
    "a child still running shows what it is running, not only what it has printed";

/// Styles of the shell keywords in a child's script, which are plain until the
/// highlighter has run over them.
fn script_token_styles(panel: &MessagesPanel, tool_id: &str) -> Vec<Style> {
    panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some(tool_id))
        .unwrap()
        .lines()
        .iter()
        .flat_map(|l| l.spans.iter())
        .filter(|s| CHILD_SCRIPT_TOKENS.contains(&s.content.trim()))
        .map(|s| s.style)
        .collect()
}

/// A batch whose one child is a shell call carrying `script` and answering
/// with `output`.
fn panel_with_script_child(script: &str, output: ToolOutput) -> MessagesPanel {
    let mut panel = panel_with_tools(&[(TOOL_ID, BATCH_TOOL)]);
    let mut ev = start(TOOL_ID, BATCH_TOOL);
    ev.output = Some(ToolOutput::Batch {
        entries: vec![caudra_agent::BatchToolEntry {
            input: Some(ToolInput::Script {
                language: "bash".into(),
                code: script.into(),
            }),
            output: Some(output),
            ..batch_child(SHELL_TOOL_NAME, "x")
        }],
        text: String::new(),
    });
    panel.tool_start(ev);
    panel.set_view(ViewMode::Expanded);
    panel
}

fn shell_child_output() -> ToolOutput {
    shell_done(TOOL_ID, false).output
}

/// The reported gap: a shell child had no syntax highlighting while a read
/// child did. A child answering with text took an arm of `child_body` that
/// drew the output alone, so its script was never rendered and the highlighter
/// had nothing to run over. Both text arms are covered: a shell child and the
/// plain answer a python child gives.
#[test_case(shell_child_output() ; "shell output")]
#[test_case(ToolOutput::Plain(BATCH_CHILD_BODY.into()) ; "plain output")]
fn a_child_draws_its_script_highlighted(output: ToolOutput) {
    let mut panel = panel_with_script_child(CHILD_SCRIPT, output);
    render(&mut panel, 100, 40);
    drain_highlight_worker(&mut panel);

    let text = seg_text(&panel, TOOL_ID);
    for line in CHILD_SCRIPT.lines() {
        assert!(text.contains(line.trim()), "{CHILD_SCRIPT_MSG}: {text:?}");
    }
    let styles = script_token_styles(&panel, TOOL_ID);
    assert!(!styles.is_empty(), "{CHILD_SCRIPT_MSG}: {text:?}");
    assert!(
        styles.iter().any(|style| style.fg.is_some()),
        "{CHILD_SCRIPT_MSG}: {styles:?}"
    );
}

/// A one-line command is the common case and it was the one that showed no
/// highlighting, so line count must not decide whether the script is drawn.
/// The summary row then gives the command up rather than printing it twice,
/// which is the trade a card's header already makes.
#[test]
fn a_one_line_child_script_moves_into_the_body() {
    const ONE_LINER: &str = "echo hello";

    let mut panel = panel_with_script_child(ONE_LINER, shell_child_output());
    render(&mut panel, 100, 40);
    settle_highlights(&mut panel);

    let text = seg_text(&panel, TOOL_ID);
    assert!(
        text.contains(&format!("1 {ONE_LINER}")),
        "{CHILD_ONE_LINER_MSG}: {text:?}"
    );
    assert_eq!(
        text.matches(ONE_LINER).count(),
        1,
        "{CHILD_ONE_LINER_MSG}: {text:?}"
    );
}

/// The reported case: a child still running took the live arm of `child_body`,
/// which drew the streaming tail and nothing else, so the one card where the
/// reader most wants to know what is running showed only what it had printed.
#[test]
fn a_running_child_draws_the_script_above_its_live_tail() {
    let mut panel = panel_with_running_shell();
    let loop_command = CHILD_SCRIPT.lines().next().unwrap();
    let mut ev = start(TOOL_ID, BATCH_TOOL);
    ev.output = Some(ToolOutput::Batch {
        entries: vec![caudra_agent::BatchToolEntry {
            status: caudra_agent::BatchToolStatus::Running,
            summary: loop_command.into(),
            input: Some(ToolInput::Script {
                language: "bash".into(),
                code: CHILD_SCRIPT.into(),
            }),
            output: None,
            ..batch_child(SHELL_TOOL_NAME, "x")
        }],
        text: String::new(),
    });
    panel.tool_start(ev);
    panel.set_batch_child_output(TOOL_ID, 0, &shell_stream());
    render(&mut panel, 100, 40);

    let text = seg_text(&panel, TOOL_ID);
    for line in CHILD_SCRIPT.lines() {
        assert!(
            text.contains(line.trim()),
            "{CHILD_LIVE_SCRIPT_MSG}: {text:?}"
        );
    }
    assert!(
        text.contains(FOLLOWING),
        "{CHILD_LIVE_SCRIPT_MSG}: {text:?}"
    );
}

const SCRIPT_HEAD: &str = "set -euo pipefail";
const SCRIPT_TAIL_LINES: usize = 24;
const RAN_LABEL: &str = "Ran";
const HEADER_ONCE_MSG: &str =
    "an open card names the command once, in the copy that is numbered and highlighted";
const WHOLE_SCRIPT_MSG: &str =
    "the script is the record of what ran, so an open card draws all of it";

/// A shell call whose script runs past any output budget, with the header the
/// agent sends for it: the script's first line.
fn shell_script_card(panel: &mut MessagesPanel) {
    let script: String = std::iter::once(SCRIPT_HEAD.to_owned())
        .chain((0..SCRIPT_TAIL_LINES).map(|i| format!("echo step_{i}")))
        .collect::<Vec<_>>()
        .join("\n");
    panel.tool_start(ToolStartEvent {
        summary: SCRIPT_HEAD.into(),
        input: Some(ToolInput::Script {
            language: "bash".into(),
            code: script,
        }),
        ..start(TOOL_ID, SHELL_TOOL_NAME)
    });
    panel.tool_done(shell_done(TOOL_ID, false));
}

/// The reported duplication: an expanded shell card printed the command in its
/// header and again as line 1 of the script. The header defers to the body,
/// which is the copy worth reading.
#[test]
fn an_open_shell_card_leaves_the_command_to_its_script() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Expanded);
    shell_script_card(&mut panel);
    render(&mut panel, 100, 40);

    let text = seg_text(&panel, TOOL_ID);
    assert_eq!(
        text.matches(SCRIPT_HEAD).count(),
        1,
        "{HEADER_ONCE_MSG}: {text:?}"
    );
    assert!(text.contains(RAN_LABEL), "{HEADER_ONCE_MSG}: {text:?}");
}

/// A row with no body has nothing to defer to, so dropping the command there
/// would leave the reader a bare verb.
#[test]
fn a_closed_shell_row_still_names_the_command() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Compact);
    shell_script_card(&mut panel);
    render(&mut panel, 100, 40);

    let text = seg_text(&panel, TOOL_ID);
    assert!(text.contains(SCRIPT_HEAD), "{HEADER_ONCE_MSG}: {text:?}");
}

/// The command as its fragments arrive, and the one-line header the agent
/// publishes alongside them: every newline is a space, so the header can never
/// be the script.
const COMMAND_FRAGMENTS: &[&str] = &[SCRIPT_HEAD, "\necho one", "\necho two"];
const STREAMED_COMMAND_HEADER: &str = "set -euo pipefail echo one echo two";
const LIVE_COMMAND_MSG: &str =
    "an open card draws the command line by line while it streams, not once the call has run";

/// Feeds a shell call the way the agent does: a pending row, then the header
/// and body of each fragment.
fn streaming_shell(panel: &mut MessagesPanel) {
    panel.tool_pending(TOOL_ID.into(), SHELL_TOOL_NAME);
    let mut arrived = String::new();
    for fragment in COMMAND_FRAGMENTS {
        arrived.push_str(fragment);
        let header = arrived.split_whitespace().collect::<Vec<_>>().join(" ");
        panel.tool_input_preview(TOOL_ID, Some(header), None);
        panel.tool_input_body(TOOL_ID, Some((*fragment).into()));
    }
}

/// The reported wait: an open card showed the space-joined header and nothing
/// else until the whole command had arrived. It draws the script instead, and
/// the settled card that replaces it draws the same thing, so nothing moves.
#[test]
fn an_open_shell_card_draws_its_command_as_it_streams() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Expanded);
    streaming_shell(&mut panel);
    render(&mut panel, 100, 40);

    let text = seg_text(&panel, TOOL_ID);
    for line in COMMAND_FRAGMENTS.iter().map(|f| f.trim()) {
        assert!(text.contains(line), "{LIVE_COMMAND_MSG}: {text:?}");
    }
    assert!(
        !text.contains(STREAMED_COMMAND_HEADER),
        "{HEADER_ONCE_MSG}: {text:?}"
    );
}

/// A closed row is the one state with no body to defer to, so it keeps the
/// header it has always had.
#[test]
fn a_closed_shell_row_names_a_streaming_command() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Compact);
    streaming_shell(&mut panel);
    render(&mut panel, 100, 40);

    let text = seg_text(&panel, TOOL_ID);
    assert!(
        text.contains(STREAMED_COMMAND_HEADER),
        "{HEADER_ONCE_MSG}: {text:?}"
    );
}

/// The card the streamed one becomes: the same script, from the call's real
/// input, and the command still named exactly once.
#[test]
fn a_started_shell_card_replaces_what_it_streamed() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Expanded);
    streaming_shell(&mut panel);
    render(&mut panel, 100, 40);
    shell_script_card(&mut panel);
    render(&mut panel, 100, 40);

    let text = seg_text(&panel, TOOL_ID);
    assert_eq!(
        text.matches(SCRIPT_HEAD).count(),
        1,
        "{HEADER_ONCE_MSG}: {text:?}"
    );
    assert!(
        !text.contains(COMMAND_FRAGMENTS[1].trim()),
        "{LIVE_COMMAND_MSG}: {text:?}, the streamed body outlived the call"
    );
}

/// The reported truncation: a long inline script was cut to the output budget
/// and offered a "click to expand" inside a card that was already open. The
/// output keeps its budget, since a tool can print without limit, but the
/// script is bounded by what the model wrote.
#[test]
fn an_open_shell_card_draws_its_whole_script() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Expanded);
    shell_script_card(&mut panel);
    render(&mut panel, 100, 60);

    let text = seg_text(&panel, TOOL_ID);
    for line in [0, SCRIPT_TAIL_LINES - 1] {
        assert!(
            text.contains(&format!("echo step_{line}")),
            "{WHOLE_SCRIPT_MSG}: {text:?}"
        );
    }
}

/// The screen row of a card's own window footer. `push_card_scroll_span`
/// measures the window before the footer is pushed, so the footer is the row
/// just past it, and `handle_click` resolves it by source line as this does.
fn scroll_footer_row(panel: &MessagesPanel, tool_id: &str) -> u16 {
    let segment = panel
        .cache
        .segments()
        .iter()
        .find(|s| s.tool_id.as_deref() == Some(tool_id))
        .unwrap();
    let chrome = segment.chrome(80);
    let width = chrome.content_width(80);
    let footer = segment
        .scroll_footer_line
        .expect("the window drew a footer");
    (0..segment.content_height(80))
        .map(|row| chrome.content_start() + row)
        .find(|row| segment.source_line_at(*row, width) == Some(footer))
        .expect("the footer was drawn")
}

/// Pausing a window has to be reversible through the affordance that says it
/// is paused. That footer sits just past the window on purpose: a press inside
/// the window arms the scroller instead of clicking anything, so a footer
/// counted as part of the window would strand the reader off the tail.
///
/// The command is left running, because a settled one has no tail to pause
/// against and its footer says so by reporting only the counts.
#[test]
fn clicking_a_paused_windows_footer_follows_the_tail_again() {
    const FOLLOW_CLICK_MSG: &str =
        "the footer that reports a paused window is what takes it back to the tail";
    const PAUSED_BODY_LINES: usize = 40;

    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start(TOOL_ID, SHELL_TOOL_NAME));
    panel.tool_output(TOOL_ID, &numbered_body(PAUSED_BODY_LINES));
    rebuild(&mut panel);
    let height = PAUSED_BODY_LINES as u16 + 8;
    let area = Rect::new(0, 0, 80, height);
    let terminal = render(&mut panel, 80, height);
    let (column, row) = card_bar_rows(&terminal)[0];

    assert!(panel.arm_card_at(column, row), "{ARM_MSG}");
    panel.scroll_card_at(column, row, CHILD_SCROLL_UP);
    let paused = buffer_text(&render(&mut panel, 80, height));
    assert!(paused.contains(PAUSED), "{FOLLOW_CLICK_MSG}: {paused}");

    let footer = scroll_footer_row(&panel, TOOL_ID);
    assert!(panel.handle_click(footer, area), "{FOLLOW_CLICK_MSG}");

    let shown = buffer_text(&render(&mut panel, 80, height));
    assert!(shown.contains(FOLLOWING), "{FOLLOW_CLICK_MSG}: {shown}");
}

/// The cost of a write is what made this deferred, so the rate is the point,
/// not an implementation detail: fragments accumulate text and the frame draws
/// it. Without this the card is rebuilt once per token.
#[test]
fn a_streaming_write_redraws_once_a_frame_not_once_a_fragment() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending(TOOL_ID.into(), FILE_WRITE_TOOL_NAME);
    render(&mut panel, 80, 24);

    let rows = |panel: &MessagesPanel| {
        panel
            .cache
            .segments()
            .iter()
            .find(|segment| segment.tool_id.as_deref() == Some(TOOL_ID))
            .map_or(0, |segment| segment.lines().len())
    };
    let settled = rows(&panel);

    for i in 0..WRITTEN_FILE_LINES {
        panel.tool_input_body(TOOL_ID, Some(format!("line {i}\n")));
    }
    assert_eq!(rows(&panel), settled, "{PER_FRAME_MSG}");

    render(&mut panel, 80, WRITTEN_FILE_LINES as u16 + 8);
    assert!(rows(&panel) > settled, "{LIVE_BODY_DRAWN_MSG}");
}

const EAGER_STATE_MSG: &str = "ingestion is eager, so the message carries every event before \
    anything is drawn";
const BACKGROUND_QUIET_MSG: &str =
    "a chat nobody is drawing must not rebuild its card once per event";
const BACKGROUND_DRAWN_MSG: &str = "the frame that draws the chat shows what every event left";
const NOTHING_OWED_MSG: &str = "a drawn frame leaves no card owed a redraw";
const OWED_MSG: &str = "a live event owes its card a redraw";
const TERMINAL_EAGER_MSG: &str = "a terminal event owns the card that restore, search and export \
    read, so it draws itself and takes the owed redraw with it";
const PREVIEW_GONE_MSG: &str = "what the call turned out to be replaces what it was spelling out";
const RESET_DROPS_MSG: &str = "a reset that drops the cache drops what the cache was owed";
const RESET_KEEPS_MSG: &str = "dropping the owed redraw must not drop the state behind it";
const OFF_SCREEN_MSG: &str = "the card has to be off screen for the flush to be the only thing \
    that can redraw it";
const OFF_SCREEN_FLUSHED_MSG: &str = "a theme change reflows only around the viewport, so an \
    off-screen card is redrawn by the flush or not at all";
const RESTORE_CLAMP_MSG: &str = "the flush runs before the scroll resolves, so a restored offset \
    clamps against the height this frame draws";

/// How many live events a backgrounded chat takes before anyone draws it. Long
/// enough that a rebuild per event would be the card's length squared.
const BACKGROUND_EVENTS: usize = 24;
/// Tall enough for a card of `BACKGROUND_EVENTS` rows and its chrome.
const DRAWN_HEIGHT: u16 = BACKGROUND_EVENTS as u16 + 8;
/// Shorter than the documents below, so what is on screen is a choice of
/// scroll offset rather than everything there is.
const CLIPPED_HEIGHT: u16 = 12;

/// A subagent's chat that the reader opened once and then left: its cache is
/// warm, so `rebuild_tool_lines` no longer misses and every forwarded event
/// would otherwise pay for the whole card.
fn backgrounded_write() -> MessagesPanel {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_pending(TOOL_ID.into(), FILE_WRITE_TOOL_NAME);
    render(&mut panel, 80, 24);
    panel
}

/// The forwarded stream a background chat is about to start seeing. The cost
/// of drawing it is the whole point, so the deferral is what is tested, not
/// the lines it eventually produces.
#[test]
fn a_backgrounded_chat_defers_a_streamed_write_until_it_is_drawn() {
    let mut panel = backgrounded_write();
    let opened = seg_text(&panel, TOOL_ID);

    for line in 0..BACKGROUND_EVENTS {
        panel.tool_input_preview(
            TOOL_ID,
            Some(WRITTEN_FILE_PATH.into()),
            Some(format!("{line} lines")),
        );
        panel.tool_input_body(TOOL_ID, Some(format!("line {line}\n")));
    }
    let tail = format!("line {}", BACKGROUND_EVENTS - 1);

    assert_eq!(
        panel.messages[0].text, WRITTEN_FILE_PATH,
        "{EAGER_STATE_MSG}"
    );
    assert!(
        panel.messages[0]
            .live_body
            .as_deref()
            .is_some_and(|body| body.contains(&tail)),
        "{EAGER_STATE_MSG}"
    );
    assert_eq!(seg_text(&panel, TOOL_ID), opened, "{BACKGROUND_QUIET_MSG}");
    assert!(panel.dirty_cards.contains(TOOL_ID), "{OWED_MSG}");

    let shown = buffer_text(&render(&mut panel, 80, DRAWN_HEIGHT));
    for line in ["line 0", tail.as_str()] {
        assert!(shown.contains(line), "{BACKGROUND_DRAWN_MSG}: {shown}");
    }
    assert!(panel.dirty_cards.is_empty(), "{NOTHING_OWED_MSG}");
}

#[test]
fn a_backgrounded_chat_defers_live_output_and_annotations_until_it_is_drawn() {
    let mut panel = panel_with_tools(&[(TOOL_ID, SHELL_TOOL_NAME)]);
    render(&mut panel, 80, 24);
    let opened = seg_text(&panel, TOOL_ID);

    for event in 1..=BACKGROUND_EVENTS {
        panel.tool_output(TOOL_ID, &numbered_body(event));
        panel.tool_annotation(TOOL_ID, format!("{event} lines"));
    }
    let tail = format!("line {}", BACKGROUND_EVENTS - 1);
    let annotation = format!("{BACKGROUND_EVENTS} lines");

    assert!(panel.messages[0].text.contains(&tail), "{EAGER_STATE_MSG}");
    assert_eq!(
        panel.messages[0].annotation.as_deref(),
        Some(annotation.as_str()),
        "{EAGER_STATE_MSG}"
    );
    assert_eq!(seg_text(&panel, TOOL_ID), opened, "{BACKGROUND_QUIET_MSG}");

    render(&mut panel, 80, DRAWN_HEIGHT);

    let drawn = seg_text(&panel, TOOL_ID);
    assert!(drawn.contains(&tail), "{BACKGROUND_DRAWN_MSG}: {drawn:?}");
    assert!(
        drawn.contains(&annotation),
        "{BACKGROUND_DRAWN_MSG}: {drawn:?}"
    );
    assert!(panel.dirty_cards.is_empty(), "{NOTHING_OWED_MSG}");
}

/// A settled card is what restore, search and export read, so it is rebuilt as
/// the event lands. The owed redraw goes with it: the card it was owed for has
/// been replaced by the one the call turned out to be.
#[test]
fn tool_done_draws_its_own_card_and_evicts_the_owed_redraw() {
    let mut panel = panel_with_tools(&[(TOOL_ID, SHELL_TOOL_NAME)]);
    render(&mut panel, 80, 24);
    panel.tool_output(TOOL_ID, &numbered_body(BACKGROUND_EVENTS));
    assert!(panel.dirty_cards.contains(TOOL_ID), "{OWED_MSG}");
    assert!(
        seg_text(&panel, TOOL_ID).contains(SHELL_RUNNING_LABEL),
        "{BACKGROUND_QUIET_MSG}"
    );

    panel.tool_done(shell_done(TOOL_ID, false));

    assert!(panel.dirty_cards.is_empty(), "{TERMINAL_EAGER_MSG}");
    let settled = seg_text(&panel, TOOL_ID);
    assert!(
        settled.contains(SHELL_SETTLED_LABEL) && !settled.contains(SHELL_RUNNING_LABEL),
        "{TERMINAL_EAGER_MSG}: {settled:?}"
    );
}

/// The last thing the call was spelling out is not what it ran, so a start
/// both redraws the card and drops what the previews were owed.
#[test]
fn tool_start_draws_its_own_card_and_evicts_the_owed_redraw() {
    let mut panel = backgrounded_write();
    panel.tool_input_preview(TOOL_ID, Some(PREVIEW_PATH.into()), None);
    panel.tool_input_body(TOOL_ID, Some(PREVIEW_BODY.into()));
    assert!(panel.dirty_cards.contains(TOOL_ID), "{OWED_MSG}");

    let mut event = start(TOOL_ID, FILE_WRITE_TOOL_NAME);
    event.summary = WRITTEN_FILE_PATH.into();
    panel.tool_start(event);

    assert!(panel.dirty_cards.is_empty(), "{TERMINAL_EAGER_MSG}");
    let started = seg_text(&panel, TOOL_ID);
    assert!(started.contains(WRITTEN_FILE_PATH), "{TERMINAL_EAGER_MSG}");
    for stale in [PREVIEW_PATH, PREVIEW_BODY] {
        assert!(!started.contains(stale), "{PREVIEW_GONE_MSG}: {started:?}");
    }
}

const PREVIEW_PATH: &str = "src/spelled_out.rs";
const PREVIEW_BODY: &str = "fn spelled_out() {}";
/// How a shell card heads itself while it runs, and once it has landed. Which
/// of the two it draws says which state its lines were built from.
const SHELL_RUNNING_LABEL: &str = "Running";
const SHELL_SETTLED_LABEL: &str = "Ran";

enum Reset {
    View,
    Load,
    Cancel,
}

/// Each of these ends or replaces the card an owed redraw named, so a surviving
/// entry would rebuild a card that had moved on.
#[test_case(Reset::View ; "set_view")]
#[test_case(Reset::Load ; "load_messages")]
#[test_case(Reset::Cancel ; "cancel_in_progress")]
fn a_reset_drops_every_owed_redraw(reset: Reset) {
    let mut panel = panel_with_tools(&[(TOOL_ID, SHELL_TOOL_NAME)]);
    render(&mut panel, 80, 24);
    panel.tool_output(TOOL_ID, &numbered_body(BACKGROUND_EVENTS));
    assert!(panel.dirty_cards.contains(TOOL_ID), "{OWED_MSG}");

    let keeps_card = !matches!(reset, Reset::Load);
    match reset {
        Reset::View => panel.set_view(ViewMode::Expanded),
        Reset::Load => panel.load_messages(Vec::new()),
        Reset::Cancel => panel.cancel_in_progress(),
    }
    assert!(panel.dirty_cards.is_empty(), "{RESET_DROPS_MSG}");

    render(&mut panel, 80, DRAWN_HEIGHT);
    assert_eq!(has_seg(&panel, TOOL_ID), keeps_card, "{RESET_KEEPS_MSG}");
    if keeps_card {
        let tail = format!("line {}", BACKGROUND_EVENTS - 1);
        let text = seg_text(&panel, TOOL_ID);
        assert!(text.contains(&tail), "{RESET_KEEPS_MSG}: {text:?}");
    }
    assert!(panel.dirty_cards.is_empty(), "{NOTHING_OWED_MSG}");
}

/// Rows of prose after the card, enough to push it clear of the viewport.
const OFF_SCREEN_FILLER: usize = 40;
const OFF_SCREEN_SUMMARY: &str = "ranked while nobody was looking";

/// The reason the flush cannot ask whether a rebuild is worth it: a theme
/// change marks every segment stale but only reflows the ones the viewport
/// reaches, so an off-screen card is left holding its old lines.
#[test]
fn a_theme_change_still_flushes_an_off_screen_card() {
    theme::set(theme::load_by_name("dracula").unwrap());
    let mut panel = panel_with_tools(&[(TOOL_ID, CODE_MAP_TOOL_NAME)]);
    for row in 0..OFF_SCREEN_FILLER {
        panel.push(DisplayMessage::new(
            DisplayRole::Assistant,
            format!("filler {row}"),
        ));
    }
    render(&mut panel, 80, CLIPPED_HEIGHT);

    panel.update_tool_summary(TOOL_ID, OFF_SCREEN_SUMMARY);
    theme::set(theme::load_by_name("tokyonight").unwrap());
    let shown = buffer_text(&render(&mut panel, 80, CLIPPED_HEIGHT));

    assert!(!shown.contains(OFF_SCREEN_SUMMARY), "{OFF_SCREEN_MSG}");
    let text = seg_text(&panel, TOOL_ID);
    assert!(
        text.contains(OFF_SCREEN_SUMMARY),
        "{OFF_SCREEN_FLUSHED_MSG}: {text:?}"
    );
}

/// A restored offset is clamped to the document, and the document is only the
/// right height once the owed redraws have been paid. Flushing after the
/// scroll resolved would pin a grown card to the height of its header.
#[test]
fn a_restored_scroll_clamps_against_the_flushed_height() {
    let mut panel = backgrounded_write();
    panel.tool_input_body(TOOL_ID, Some(numbered_body(WRITTEN_FILE_LINES)));
    panel.restore_scroll(u32::MAX, false);

    let shown = buffer_text(&render(&mut panel, 80, CLIPPED_HEIGHT));

    let tail = format!("line {}", WRITTEN_FILE_LINES - 1);
    assert!(shown.contains(&tail), "{RESTORE_CLAMP_MSG}: {shown}");
    assert_eq!(
        panel.scroll_top(),
        panel.max_scroll(),
        "{RESTORE_CLAMP_MSG}"
    );
}

const HOVER_KEPT_MSG: &str = "a redraw the frame owes a card cannot cancel the pointer the reader \
    is holding, or nothing under a streaming chat could ever be pointed at";
const HOVER_PAINTED_MSG: &str =
    "a pointer that survives the flush has to reach the paint of the same frame";

const HOVER_SETUP_MSG: &str = "a document that changed height cancels the pointer on its own, so \
    the card has to restate its output at the height it already settled at";
/// Lines of live output the card settles at before the pointer lands on it.
const STEADY_LINES: usize = 5;

/// The flush runs before the frame reads hover, so clearing it there lands
/// after the mouse handler that set it and the mark never paints.
#[test]
fn flushing_a_dirty_card_leaves_the_pointer_where_the_reader_put_it() {
    let mut panel = panel_with_tools(&[(TOOL_ID, SHELL_TOOL_NAME)]);
    let area = Rect::new(0, 0, 80, DRAWN_HEIGHT);
    panel.tool_output(TOOL_ID, &numbered_body(STEADY_LINES));
    render(&mut panel, area.width, area.height);
    let settled = panel.segment_heights();

    panel.update_hover(area.y, area.x, area, false, Path::new(NO_PROJECT));
    let Some(HoverTarget::Tool { feedback, .. }) = &panel.hover else {
        panic!("the test must hover the card it dirties: {:?}", panel.hover);
    };
    let held = HoverTarget::Tool {
        id: TOOL_ID.into(),
        feedback: *feedback,
    };

    let restated: String = (0..STEADY_LINES)
        .map(|line| format!("tick {line}\n"))
        .collect();
    panel.tool_output(TOOL_ID, &restated);
    let marked = render(&mut panel, area.width, area.height);

    assert_eq!(panel.segment_heights(), settled, "{HOVER_SETUP_MSG}");
    assert_eq!(panel.hover, Some(held), "{HOVER_KEPT_MSG}");
    let marked = style_of(&marked, TOOL_ID);
    panel.clear_hover();
    let plain = style_of(&render(&mut panel, area.width, area.height), TOOL_ID);
    assert_ne!(marked, plain, "{HOVER_PAINTED_MSG}");
}

/// How a roster row names the step it is on, so the card says which of them it
/// was built from.
const ROSTER_MARK: &str = "step_";

enum ChildEvent {
    Output,
    Progress,
    Roster,
}

/// Drives one step of a streaming child and hands back the text its card draws
/// once the frame that owes it lands.
fn stream_child(panel: &mut MessagesPanel, event: &ChildEvent, step: usize) -> String {
    match event {
        ChildEvent::Output => {
            panel.set_batch_child_output(TOOL_ID, 0, &numbered_body(step));
            format!("line {}", step - 1)
        }
        ChildEvent::Progress => {
            panel.set_batch_child_progress(TOOL_ID, 0, child_report());
            RUNNING_LABEL.to_owned()
        }
        ChildEvent::Roster => {
            let entry = BatchToolEntry {
                summary: format!("{ROSTER_MARK}{step}"),
                ..running_child(SHELL_TOOL_NAME)
            };
            panel.batch_progress(TOOL_ID, 0, entry);
            format!("{ROSTER_MARK}{step}")
        }
    }
}

/// The volume the deferral was built for: a batch runs its children under ids
/// of its own, so every one of them redraws the single card the batch owns.
#[test_case(ChildEvent::Output ; "streamed_output")]
#[test_case(ChildEvent::Progress ; "activity_report")]
#[test_case(ChildEvent::Roster ; "roster_entry")]
fn a_backgrounded_batch_defers_its_children_until_it_is_drawn(event: ChildEvent) {
    let mut panel = panel_with_running_shell();
    let opened = seg_text(&panel, TOOL_ID);

    let mut mark = String::new();
    for step in 1..=BACKGROUND_EVENTS {
        mark = stream_child(&mut panel, &event, step);
    }

    assert!(!opened.contains(&mark), "the test must assert on a change");
    assert_eq!(seg_text(&panel, TOOL_ID), opened, "{BACKGROUND_QUIET_MSG}");
    assert!(panel.dirty_cards.contains(TOOL_ID), "{OWED_MSG}");

    render(&mut panel, 80, DRAWN_HEIGHT);

    let drawn = seg_text(&panel, TOOL_ID);
    assert!(drawn.contains(&mark), "{BACKGROUND_DRAWN_MSG}: {drawn:?}");
    assert!(panel.dirty_cards.is_empty(), "{NOTHING_OWED_MSG}");
}

const SNAPSHOT_EAGER_MSG: &str = "a rendered chunk draws its own card, which is what the runtime \
    handed a snapshot back for";
const SNAPSHOT_OWED_MSG: &str = "a card the chunk just drew owes the next frame nothing, or every \
    chunk buys a second rebuild of what is already on screen";

/// A live shell chunk reaches the card twice: once as the text `tool_output`
/// marks dirty, once as the snapshot that draws it. The second has to take the
/// mark with it.
#[test]
fn a_live_shell_chunk_leaves_no_redundant_redraw_owed() {
    let mut panel = panel_with_tools(&[(TOOL_ID, SHELL_TOOL_NAME)]);
    render(&mut panel, 80, DRAWN_HEIGHT);

    panel.tool_snapshot(
        TOOL_ID,
        BufferSnapshot::plain_text(numbered_body(BACKGROUND_EVENTS)),
        None,
    );

    let tail = format!("line {}", BACKGROUND_EVENTS - 1);
    let drawn = seg_text(&panel, TOOL_ID);
    assert!(drawn.contains(&tail), "{SNAPSHOT_EAGER_MSG}: {drawn:?}");
    assert!(panel.dirty_cards.is_empty(), "{SNAPSHOT_OWED_MSG}");
}

const RATE_CLEARED_MSG: &str = "a finished prefill leaves no rate behind";
const NARROW_RATE_MSG: &str = "a narrow viewport keeps the bar and drops the detail";
const BAR_ALWAYS_MSG: &str = "the progress bar is drawn at every width";

#[test]
fn clearing_prompt_progress_drops_the_rate() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    let start = std::time::Instant::now();
    panel.prompt_rate.sample(0, start);
    panel
        .prompt_rate
        .sample(1_000, start + Duration::from_millis(500));
    panel.set_prompt_progress(None);

    assert_eq!(panel.prompt_rate.label(), None, "{RATE_CLEARED_MSG}");
}

/// The bar is the answer and survives every width; the rate is the detail.
#[test_case(120, true ; "wide_viewport_shows_both")]
#[test_case(24, false ; "narrow_viewport_shows_the_bar_alone")]
fn a_prefilling_prompt_draws_its_rate_beside_the_bar(width: u16, shows_rate: bool) {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    // An empty transcript draws the splash instead, and the splash owns the
    // whole viewport.
    panel.streaming_text.set_buffer("prefilling\n");
    let start = std::time::Instant::now();
    panel.prompt_rate.sample(0, start);
    panel
        .prompt_rate
        .sample(1_000, start + Duration::from_millis(500));
    panel.set_prompt_progress(Some(PromptProgress {
        processed: 1_000,
        total: 4_000,
        cache: 0,
    }));

    let text = buffer_text(&render(&mut panel, width, 8));

    assert!(
        text.contains(PROMPT_PROGRESS_LABEL.trim()),
        "{BAR_ALWAYS_MSG}"
    );
    assert_eq!(text.contains("2.0k tok/s"), shows_rate, "{NARROW_RATE_MSG}");
}

const COPY_SCRIPT: &str = "printf 'one'\nprintf 'two'";
const COPY_LANGUAGE: &str = "bash";
const COPY_FILE_PATH: &str = "src/f.rs";
const COPY_FILE: &str = "fn main() {\n    println!(\"hi\");\n}";
const COPY_WIDTH: u16 = 80;
const COPY_HEIGHT: u16 = 40;
const SCRIPT_UNGUTTERED_MSG: &str =
    "a selection inside a code block copies the source, never the line-number gutter";
const CARD_FENCED_MSG: &str =
    "a selection past the code block fences it and leaves the output rows beside it";
const READ_UNGUTTERED_MSG: &str = "a read body copies as the file, not as a numbered listing";
const READ_TOOL: &str = "read";
const SHELL_FIRST_LINE: &str = "raw_1";
const SHELL_LAST_LINE: &str = "raw_8";

/// The document rows a card's code block occupies, and the rows of the whole
/// card, so a selection can be aimed at either.
fn code_block_and_card_rows(
    panel: &MessagesPanel,
    width: u16,
) -> (std::ops::Range<u32>, std::ops::Range<u32>) {
    let mut start = 0u32;
    for segment in panel.cache.segments() {
        let height = u32::from(segment.height(width));
        if let Some((rows, _)) = segment.code_blocks(width).into_iter().next() {
            let first = start + u32::from(segment.chrome(width).content_start());
            return (
                first + u32::from(rows.start)..first + u32::from(rows.end),
                start..start + height,
            );
        }
        start += height;
    }
    panic!("no card recorded a code block");
}

fn shell_script_panel() -> MessagesPanel {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.set_view(ViewMode::Expanded);
    panel.tool_start(ToolStartEvent {
        summary: COPY_SCRIPT.lines().next().unwrap().into(),
        input: Some(ToolInput::Script {
            language: COPY_LANGUAGE.into(),
            code: COPY_SCRIPT.into(),
        }),
        ..start(TOOL_ID, SHELL_TOOL_NAME)
    });
    panel.tool_done(shell_done(TOOL_ID, false));
    panel
}

/// The reported defect: selecting a shell card's script put the gutter on the
/// clipboard, so what was copied could not be run.
///
/// Both passes are covered because the highlighted render splits each row into
/// one span per token. Provenance built by the unhighlighted pass describes
/// lines that no longer exist once the worker's result is spliced in, and a
/// mismatch there falls back to scraping, which is the defect again.
#[test_case(false ; "before_the_highlight_worker_runs")]
#[test_case(true ; "after_the_highlight_worker_runs")]
fn copying_a_script_omits_the_line_number_gutter(highlighted: bool) {
    let mut panel = shell_script_panel();
    let area = Rect::new(0, 0, COPY_WIDTH, COPY_HEIGHT);
    render(&mut panel, COPY_WIDTH, COPY_HEIGHT);
    if highlighted {
        drain_highlight_worker(&mut panel);
    }

    let (code, _) = code_block_and_card_rows(&panel, COPY_WIDTH);
    let sel = make_sel(area, (code.start, 0), (code.end - 1, COPY_WIDTH - 1));

    assert_eq!(
        panel.extract_selection_text(&sel, area),
        COPY_SCRIPT,
        "{SCRIPT_UNGUTTERED_MSG}"
    );
}

/// A selection that ran past the script is a mixture, so the script is fenced
/// where it sits and the command output stays as it was drawn.
#[test]
fn copying_a_whole_card_fences_the_script_and_keeps_the_output() {
    let mut panel = shell_script_panel();
    let area = Rect::new(0, 0, COPY_WIDTH, COPY_HEIGHT);
    render(&mut panel, COPY_WIDTH, COPY_HEIGHT);

    let (_, card) = code_block_and_card_rows(&panel, COPY_WIDTH);
    let sel = make_sel(area, (card.start, 0), (card.end - 1, COPY_WIDTH - 1));
    let copied = panel.extract_selection_text(&sel, area);

    assert!(
        copied.starts_with(&format!("```{COPY_LANGUAGE}\n{COPY_SCRIPT}\n```\n")),
        "{CARD_FENCED_MSG}: {copied:?}"
    );
    for line in [SHELL_FIRST_LINE, SHELL_LAST_LINE] {
        assert!(copied.contains(line), "{CARD_FENCED_MSG}: {copied:?}");
    }
    assert!(!copied.contains("  1 "), "{CARD_FENCED_MSG}: {copied:?}");
}

/// A read card's body is drawn behind the same gutter, so it copied the same
/// way and is fixed by the same provenance.
#[test]
fn copying_a_read_body_omits_the_line_number_gutter() {
    let mut panel = MessagesPanel::new(UiConfig::default(), EventHandle::disconnected_for_test());
    panel.tool_start(start(TOOL_ID, READ_TOOL));
    panel.tool_done(ToolDoneEvent {
        id: TOOL_ID.into(),
        tool: READ_TOOL.into(),
        output: ToolOutput::ReadCode {
            path: COPY_FILE_PATH.into(),
            start_line: 1,
            lines: COPY_FILE.lines().map(str::to_owned).collect(),
            total_lines: COPY_FILE.lines().count(),
            instructions: None,
        },
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: Vec::new(),
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    });
    let area = Rect::new(0, 0, COPY_WIDTH, COPY_HEIGHT);
    render(&mut panel, COPY_WIDTH, COPY_HEIGHT);
    drain_highlight_worker(&mut panel);

    let (code, _) = code_block_and_card_rows(&panel, COPY_WIDTH);
    let sel = make_sel(area, (code.start, 0), (code.end - 1, COPY_WIDTH - 1));

    assert_eq!(
        panel.extract_selection_text(&sel, area),
        COPY_FILE,
        "{READ_UNGUTTERED_MSG}"
    );
}

/// `Provenance::extract` needs one row per painted line and gives up on the
/// whole card otherwise, which is a silent return to scraping the gutter.
#[test_case(false ; "before_the_highlight_worker_runs")]
#[test_case(true ; "after_the_highlight_worker_runs")]
fn a_cards_source_rows_stay_parallel_to_its_lines(highlighted: bool) {
    const PARALLEL_MSG: &str = "every painted line of a card needs a source row of its own";

    let mut panel = shell_script_panel();
    render(&mut panel, COPY_WIDTH, COPY_HEIGHT);
    if highlighted {
        drain_highlight_worker(&mut panel);
    }

    let segment = panel
        .cache
        .segments()
        .iter()
        .find(|segment| segment.tool_id.as_deref() == Some(TOOL_ID))
        .expect("the card was built");
    let provenance = segment.provenance().expect("the card recorded its source");

    assert_eq!(
        provenance
            .lines_in(0..segment.lines().len())
            .map(|r| r.len()),
        Some(segment.lines().len()),
        "{PARALLEL_MSG}"
    );
}

/// A card's output may be windowed, so the ranges a copy records have to index
/// the rows that were drawn rather than the line numbers they came from.
#[test]
fn copying_a_windowed_output_takes_the_rows_that_were_drawn() {
    const WINDOWED_MSG: &str = "a windowed card copies the rows it drew, not the ones it hid";
    const OUTPUT_LINES: usize = 40;
    const OUTPUT_PREFIX: &str = "line ";

    let mut panel = panel_with_long_tool(SHELL_TOOL_NAME, OUTPUT_LINES);
    let area = Rect::new(0, 0, COPY_WIDTH, COPY_HEIGHT);
    render(&mut panel, COPY_WIDTH, COPY_HEIGHT);

    let segment = panel
        .cache
        .segments()
        .iter()
        .find(|segment| segment.tool_id.as_deref() == Some(TOOL_ID))
        .expect("the card was built");
    assert!(segment.scroll_footer_line.is_some(), "{WINDOWED_MSG}");
    let drawn: Vec<String> = segment
        .lines()
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .filter(|text| text.starts_with(OUTPUT_PREFIX))
        .collect();
    assert!(drawn.len() < OUTPUT_LINES, "{WINDOWED_MSG}");

    let total = panel.segment_heights().iter().sum::<u16>();
    let sel = make_sel(area, (0, 0), (u32::from(total), COPY_WIDTH - 1));
    let copied: Vec<String> = panel
        .extract_selection_text(&sel, area)
        .lines()
        .map(str::to_owned)
        .collect();

    assert_eq!(copied, drawn, "{WINDOWED_MSG}");
}

/// The reported defect: ten calls inside a batch copied as one undivided
/// block, with nothing left saying which output belonged to which call. Each
/// child is a section of the card, and its body is fenced where it sits so a
/// tool's printed lines do not reflow into the prose around them.
#[test]
fn copying_a_batch_keeps_a_section_per_child() {
    const SECTION_MSG: &str =
        "a batch copies as one section per child, or its calls run together into one block";
    const FENCE: &str = "```";
    const CHILDREN: [&str; 2] = [FILE_WRITE_TOOL_NAME, FILE_EDIT_TOOL_NAME];

    let mut panel = panel_with_tools(&[(TOOL_ID, BATCH_TOOL)]);
    panel.tool_done(ToolDoneEvent {
        tool: BATCH_TOOL.into(),
        output: ToolOutput::Batch {
            entries: CHILDREN
                .iter()
                .enumerate()
                .map(|(index, tool)| batch_child(tool, &index.to_string()))
                .collect(),
            text: String::new(),
        },
        ..done(TOOL_ID)
    });
    // A lone fragment copies raw, so the card needs a neighbour before the
    // selection becomes the markdown document this is about.
    panel.push(DisplayMessage::new(
        DisplayRole::Assistant,
        "Finished".into(),
    ));

    let copied = extract_entire_document(&mut panel);
    let card = copied
        .split("## Tool:")
        .nth(1)
        .unwrap_or_else(|| panic!("{SECTION_MSG}: {copied}"));
    let first = card
        .find("### 1.")
        .unwrap_or_else(|| panic!("{SECTION_MSG}: {copied}"));

    // A heading inside a fence is literal text, so the card must not open one.
    assert!(!card[..first].contains(FENCE), "{SECTION_MSG}: {copied}");
    for (index, tool) in CHILDREN.iter().enumerate() {
        assert!(
            card.contains(&format!("### {}. `{tool}`", index + 1)),
            "{SECTION_MSG}: {copied}"
        );
        assert!(
            card.contains(&format!("{BATCH_CHILD_BODY}_{index}")),
            "{SECTION_MSG}: {copied}"
        );
    }
    assert!(card.contains(FENCE), "{SECTION_MSG}: {copied}");
}

/// A renderer that names no source has to keep copying by scraping the screen,
/// which is all a diff or a grep can do. A batch card names one as far as its
/// children do, and falls back to scraping the moment any of them cannot.
#[test]
fn a_card_that_records_no_source_still_copies_by_scraping() {
    const SCRAPE_MSG: &str = "a card whose body records no source still copies what it drew";
    const ADDED: &str = "gamma";

    let mut panel = panel_with_tools(&[(TOOL_ID, FILE_EDIT_TOOL_NAME)]);
    panel.tool_done(ToolDoneEvent {
        tool: FILE_EDIT_TOOL_NAME.into(),
        output: ToolOutput::Diff {
            path: COPY_FILE_PATH.into(),
            before: "alpha\nbeta\n".into(),
            after: format!("alpha\n{ADDED}\n"),
            summary: "1 edit".into(),
        },
        ..done(TOOL_ID)
    });
    panel.set_view(ViewMode::Expanded);
    let area = Rect::new(0, 0, COPY_WIDTH, COPY_HEIGHT);
    render(&mut panel, COPY_WIDTH, COPY_HEIGHT);

    let segment = panel
        .cache
        .segments()
        .iter()
        .find(|segment| segment.tool_id.as_deref() == Some(TOOL_ID))
        .expect("the card was built");
    assert!(segment.provenance().is_none(), "{SCRAPE_MSG}");

    let total = panel.segment_heights().iter().sum::<u16>();
    let sel = make_sel(area, (0, 0), (u32::from(total), COPY_WIDTH - 1));

    assert!(
        panel.extract_selection_text(&sel, area).contains(ADDED),
        "{SCRAPE_MSG}"
    );
}
