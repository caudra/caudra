use super::*;
use crate::agent::shared_queue::{self, QueueReceiver};
use crate::agent::tools_preview_source;
use crate::app::permission_editor::tests::workbench_workspace;
use crate::app::sandbox::attached_sandbox_instance;
use crate::app::tasks::{MAIN_TASK_ID, TaskStatus};
use crate::chat::{CANCELLED_TEXT, DONE_TEXT, ERROR_TEXT};
use crate::components::command::{BUILTIN_COMMANDS, CommandPalette, ParsedCommand};
use crate::components::context_modal::{
    EXPANDED_TITLE as CONTEXT_EXPANDED_TITLE, TITLE as CONTEXT_TITLE,
};
use crate::components::docs_modal::fixture as docs_fixture;
use crate::components::file_walk::UNREADABLE_DIR_MSG;
use crate::components::goal_modal::GoalTarget;
use crate::components::help_modal::HelpTarget;
use crate::components::input::ChordHint;
use crate::components::keybindings::{Bind, KeybindContext, key as kb, leader as chord};
use crate::components::messages::{ASSISTANT_LABEL, ReviewTarget};
use crate::components::projection_modal::UNPREPARED as PROJECTION_UNPREPARED;
use crate::components::queue_actions::QueueActionKind;
use crate::components::queue_panel::{QueueAction, QueueHit, QueueHitTarget};
use crate::components::rewind_picker::RewindEntry;
use crate::components::status_bar::StatusBarHitTarget;
use crate::components::storage_modal::{
    EXPANDED_TITLE as STORAGE_EXPANDED_TITLE, TITLE as STORAGE_TITLE,
};
use crate::components::stream_modal::{StreamDone, StreamEvent, StreamFooter, StreamUsage};
use crate::components::usage_modal::SCOPE_KEY;
use crate::components::{
    DisplaySource, ExitRequest, ForkedSession, SubscriptionProvider, ToolProgress, buffer_text,
    key, test_model,
};
use crate::repaint::expect::{OWED, QUIET};
use crate::selection::{SelectableZone, SelectionState, SelectionZone};
use crate::test_pattern_discovery_report;
use arc_swap::ArcSwap;
use caudra_agent::command::CustomCommand;
use caudra_agent::commits::repo::CommitSummary;
use caudra_agent::context::{
    ContextBuiltinState, ContextInventory, ContextModel, ContextReadiness, ContextReserve,
    ContextUsage, ContextWindow,
};
use caudra_agent::decisions::Decisions;
use caudra_agent::mcp::config::{McpConfigSource, McpReviewSummary};
use caudra_agent::permissions::pattern_recognition::{
    CandidateEvidence, InvocationOutcome, ObservationProvenance, PatternCandidate, SupportCount,
};
use caudra_agent::permissions::{
    PermissionManager, PermissionRequest, PermissionResourceSelector, PermissionRuleRecord,
    RuleOrigin,
};
use caudra_agent::tools::native::plan::{self, PlanTarget, PlanWriteResult};
use caudra_agent::tools::profile_policy::PLAN_REQUIRED;
use caudra_agent::tools::{
    SHELL_TOOL_NAME, TODOWRITE_TOOL_NAME, ToolEffect, ToolRegistry, VIEW_IMAGE_TOOL_NAME, native,
};
use caudra_agent::types::{AskedQuestion, QuestionOption, TodoItem, TodoPriority, TodoStatus};
use caudra_agent::{
    BatchProgressEvent, BatchToolEntry, BatchToolStatus, CallStage, DoneReason, GoalResult,
    GoalStatus, GoalVerdict, HistorySnapshot, ImageMediaType, McpConfigErrors, McpServerInfo,
    McpServerStatus, McpSnapshot, McpSnapshotReader, SubagentActivity, SubagentProgress,
    TextOutput, ToolAccounting, ToolDoneEvent, ToolOutput, ToolStartEvent, TurnCompleteEvent,
};
use caudra_config::decisions::DecisionsConfig;
use caudra_config::sandbox::SandboxName;
use caudra_config::{
    Effect, FeatureFlags, InboundPolicy, PermissionReviewCandidate, PermissionReviewKind,
    PermissionRule, PermissionSource, PermissionsConfig, ToolKey, UiConfig,
};
use caudra_lua::test_support::{HintWriterHandle, hint_writer_pair};
use caudra_lua::{BuiltinAction, HintReader, KeymapReader, LuaCommandInfo, LuaCommandReader};
use caudra_providers::model_registry::{self, Binding};
use caudra_providers::{
    Billing, ContentBlock, HistoryItem, HistoryItemKind, Message, RequestOptions, Role,
    THINKING_USAGE, TaskEventOrigin, TokenUsage, UserOrigin, expand_message, merge_history_items,
    project_messages,
};
use caudra_storage::StateClass;
use caudra_storage::id::CaudraId;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::permission_patterns::{
    ArgumentRole, PATTERN_SCHEMA_VERSION, PatternContext, PatternDefinition, PatternToken,
    SlotCombinations,
};
use caudra_storage::plans::PlanFile;
use caudra_storage::prompt_stash::{PromptStash, StashEntry};
use caudra_storage::sessions::{
    PermissionMode, SessionLocation, SessionMeta, StoredActiveGoal, StoredGoalVerdict, StoredImage,
    StoredInboundPolicy, StoredMode, StoredPasteRange, StoredPeerControls, StoredPromptAdmission,
    StoredQueuedDraft, StoredQueuedPrompt, StoredSubagent, StoredSubagentOutcome, StoredTokenUsage,
    UnrecordedCall,
};
use caudra_storage::state::{self as stored_state, SCOPE_GLOBAL, StateKey};
use caudra_storage::thinking::StoredThinking;
use caudra_storage::tool_outputs::{ToolOutputError, ToolOutputStore};
use caudra_storage::usage_ledger::{LedgerPurpose, TurnUsage, UsageLedger};
use caudra_storage::view::ViewMode;
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_workbench::keys as workbench_keys;
use caudra_workspace::{
    CollectionRevision, LocalDocumentRef, OperationId, PlanRef, ProjectAsset, ProjectAssetContent,
    ProjectAssetManifest, SessionWorkspaceBinding, WorkspaceAssetService, WorkspaceCapabilities,
    WorkspaceCursor, WorkspaceError, WorkspaceHandle, WorkspaceServices, WorkspaceSession,
};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::slice::from_ref;
use std::time::Duration;
use tempfile::{Builder, TempDir};
use test_case::test_case;

const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const LONG_FLASH_DURATION: Duration = Duration::from_secs(60);
const CONFIRMATION_TIMINGS: [(Duration, bool); 4] = [
    (Duration::from_millis(1500), true),
    (Duration::from_secs(2), true),
    (Duration::from_secs(3), false),
    (Duration::from_secs(4), false),
];
const CONFIRMATION_USER_TEXT: &str = "confirmation target";
const SELECTED_BUILD_TOOLS: &str = "Selected Build · next user message";
const SELECTED_PLAN_TOOLS: &str = "Selected Plan · next user message";
const EXECUTING_BUILD_TOOLS: &str = "Executing Build · Selected Plan pending";
const EXECUTING_PLAN_TOOLS: &str = "Executing Plan · Selected Build pending";
const PREVIEW_PLAN_FILE: &str = "preview-only-plan.md";
const PREVIEW_PERMISSION_ID: &str = "preview-permission";
const PREVIEW_PERMISSION_COMMAND: &str = "pwd";
const PREVIEW_QUEUED_TEXT: &str = "leave this queued request unchanged";
const PREVIEW_VISION_MODEL: &str = "aperture/mistral/mistral-medium-latest";
#[cfg(unix)]
const PRIVATE_TEST_DIRECTORY_MODE: u32 = 0o700;
const HANDOFF_PLAN_FILE: &str = "test-plan.md";
const HANDOFF_PLAN_CONTENT: &str = "# Captured plan\n\nImplement without reading files.";
const HANDOFF_PLAN_CHANGED: &str = "# A later plan revision";
const IMPLEMENT_REVISION: &str = "revision";
const COMMITTED_PLAN_PATH: &str = "/unreadable/committed-plan.md";
const STALE_PLAN_PATH: &str = "/unreadable/stale-plan.md";
const COMMITTED_PLAN_REF: &str = "plan-committed";
const STALE_PLAN_REF: &str = "plan-stale";
const COMMITTED_PLAN_CALL: &str = "committed-plan-write";
const PEER_DELIVERED: usize = 11;
const PEER_SENT: usize = 9;
const PEER_CONTROL_PROMPT: &str = "Keep the peer controls across a reload.";
const PEER_MANAGER_TITLE: &str = "Peers";
const PEER_MANAGER_DRAFT: &str = "Preserve this unsent prompt.";
const PEER_MANAGER_PASTE: &str = "peer filter";
const PEER_MANAGER_IMAGE_PATH: &str = "/tmp/peer-manager-image.png";
const PEER_MANAGER_MESSAGE: &str = "amber-brook";
const PEER_MANAGER_ERROR: &str = "Peer manager test discovery failure";
const PEER_MANAGER_OPEN_COUNT: usize = 1;
const PEER_MANAGER_QUESTION_LINES: usize = 48;
const REJECTED_DRAFT_PREFIX: &str = "  review ";
const REJECTED_DRAFT_PASTE: &str = "pasted context\nsecond line";
const REJECTED_DRAFT_IMAGE: &str = "iVBORw0KGgo=";
const OPENAI_SETUP_MODEL: &str = "openai/gpt-6.1-sol";
const SETUP_MEDIUM_EFFORT: &str = "medium";
const SETUP_HIGH_EFFORT: &str = "high";
const SETUP_OFF: &str = "off";
const DECISIONS_COMMAND: &str = "/decisions";
pub(crate) const DECISIONS_TEST_BASE_URL: &str = "http://127.0.0.1:9";
const DECISIONS_TEST_MODEL: &str = "test-decision-model";
const PERMISSION_TEST_TIMEOUT: Duration = Duration::from_secs(10);
const PATTERN_TEST_ANALYSIS: &str = "test-analysis/v1";
const PATTERN_TEST_SOURCE: &str = "history-test";
const PATTERN_TEST_COMMAND: &str = "fixture-command";
const PATTERN_TEST_OTHER_COMMAND: &str = "fixture-second";
const PERMISSIONS_MODE_RULE_COUNT: usize = 240;
const PROJECT_PERMISSION_TEST_SCOPE: &str = "cargo check -p caudra-ui";
const REPORTED_PERMISSION_FIRST: &str = "physical-first";
const REPORTED_PERMISSION_SECOND: &str = "physical-second";
const REPORTED_PERMISSION_COMMAND: &str = "cargo check";
const PERMISSION_TITLE: &str = "Allow shell command?";
const PERMISSION_ALLOW_HINT: &str = "1. Yes";
const PERMISSION_GUIDANCE_KEY: char = 'n';
const PERMISSION_GUIDANCE_PROMPT: &str = "› ";
const PERMISSION_EDITOR_DRAFT: &str = "unsaved editor text";
const REPEAT_FIELD_TEXT: &str = "repeat field";
const REPEAT_WORDS: &str = "alpha beta";
const REPEAT_WORDS_ONE_LEFT: &str = "alpha ";
const REPEAT_WORDS_CARET_HOME: &str = "Xalpha beta";
const REPEAT_FIELD_EDITED: &str = "repeat fiel";
const REPEAT_EDITOR_FILE: &str = "repeat-input.txt";
const RELOCATION_DESTINATION: &str = "destination with spaces";
const RELOCATION_DRAFT: &str = "keep this draft";
const RELOCATION_WRITE_VERSION: i64 = 7;
const RELOCATION_OTHER_OPEN_COUNT: usize = 2;
const RELOCATION_CONFIRM_TITLE: &str = "Confirm session relocation";
const RELOCATION_CONFIRM: &str = "Confirm relocation";
const RELOCATION_PROJECT_USAGE: &str = "Include historical project usage";
const PROJECTION_QUESTION: &str = "list the files";
const PROJECTION_CALL_ID: &str = "call-running";
const PROJECTION_TOOL: &str = "bash";
const PROJECTION_CALL_CLOSED: &str =
    "a call still running must stay open, not be answered with a stand-in";
const PROJECTION_PROMPT_REBUILT: &str = "the view must hold the prompt the run bound";
const PROJECTION_KEY_WRONG: &str = "the wire body must name the conversation's own cache";
const PROJECTION_OPTIONS_STALE: &str =
    "the wire body must carry the options the next message will, not the last run's";
/// What [`test_model`] answers as, so a turn recorded in a test lands under
/// the session's own provider the way a real one does.
const TEST_PROVIDER: &str = "anthropic";
const MISSING_ZONE: &str = "the transcript must have registered a zone";
/// The instance from the reported bug, named the way `/sandbox` lists it.
const SANDBOX_INSTANCE_NAME: &str = "prowlix-instance";
const UNNAMED_SANDBOX_MSG: &str = "the footer must name the sandbox the runtime attached to";
const MANAGER_LOADED_MSG: &str = "naming the footer must not build the sandbox manager";
const DAEMON_POLLED_MSG: &str = "naming the footer must not start a live instance snapshot";
const BAR_IGNORED: &str = "the pointer moved the view somewhere it should not have";
const ARMING_LEADER_IS_INERT_MSG: &str = "arming the leader waits for a second key, it never acts";
const MENTION_POPUP_CLOSED: &str = "typing @ over a matching path must open the popup";
const MENTION_POPUP_LINGERED: &str = "choosing a file must close the popup";
const MENTION_ROW_MISSING: &str = "the popup drew no row for the path under test";
const MENTIONED_FILE: &str = "target.rs";
const MENTION_NOT_OPENED: &str = "clicking a mention must open the workbench at the file it names";
const MENTION_OPENED: &str = "the workbench opened for a press and release that named nothing";
const MENTION_ATE_SELECTION: &str = "a drag across a mention must still select text";
const MENTION_UNRESOLVED: &str = "a completed path must resolve to a mention";
const MENTION_DROPPED_A_PASTE: &str =
    "completing a mention must splice a range, not replace the buffer";
const COMMIT_ID: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
const COMMIT_SHORT_ID: &str = "a1b2c3d";
const COMMIT_SUBJECT: &str = "Fix login crash";
const COMMIT_AUTHOR: &str = "Ada Lovelace";
const COMMIT_UNIX_SECONDS: i64 = 1_700_003_600;
const COMMIT_COMPLETION_PREFIX: &str = "landed in ";
const COMMIT_COMPLETION_SUFFIX: &str = " yesterday";
const COMMIT_POPUP_CLOSED: &str = "typing # over a loaded log must open the popup";
const COMMIT_POPUP_LINGERED: &str = "choosing a commit must close the popup";
const COMMIT_UNRESOLVED: &str = "a completed hash must resolve to a commit";
const COMMIT_NOT_OPENED: &str = "clicking a commit must open the workbench on source control";
const COMMIT_LOADING_CAPTURES: &str = "a popup with no rows must not capture keys or clicks";
const COMMIT_LOADING_LINGERED: &str = "the spinner outlived the window it was waiting for";
const COMMIT_WINDOW_STALE: &str = "a fresh read must replace the window, not be discarded";
const COMMIT_READ_LOCALLY: &str = "a remote log must be read through the workbench";
const COMMIT_RELOAD_STALLED: &str = "the log read never landed";
const COMMIT_RELOAD_TIMEOUT: Duration = Duration::from_secs(10);
const OTHER_PROVIDER: &str = "openrouter";
const QUEUE_MENU_MISSING: &str = "the three-dot affordance must open the queue menu";
const QUEUE_MENU_ENTRY_MISSING: &str = "the queue menu did not offer the action under test";
const QUEUE_ROW_MISSING: &str = "the queue panel drew no row";
const QUEUE_TEXT_OFFSET: u16 = 2;
/// Six content rows plus the header and the trailing edge.
const QUEUE_PANEL_MAX_HEIGHT: u16 = 8;
const REPLACEMENT_PROMPT: &str = "replace the running turn";
const RESUME_PROMPT_TEXT: &str = "keep going";
const RESUME_PARTIAL_TEXT: &str = "half an answer";
const LEDGER_PROVIDER: &str = "anthropic";
const LEDGER_MODEL: &str = "claude-opus-5";
const LEDGER_CWD: &str = "/home/dev/caudra";
const LEDGER_COST: f64 = 0.25;
const LIFETIME_IS_NOT_READ_UNTIL_ASKED_FOR: &str =
    "opening the session view must not touch the ledger";
const GOAL_REASON: &str = "lint has not run";
const GOAL_INPUT: u32 = 400;
const GOAL_OUTPUT: u32 = 90;
const GOAL_COST: f64 = 0.125;
const GOAL_EVALUATIONS: u32 = 3;
const GOAL_ELAPSED_MS: u64 = 90_000;
const GOAL_PROGRESS_SURVIVES: &str =
    "an active goal must resume with the spend and count it already had";
const GOAL_SPEND_IS_ITS_OWN: &str = "goal evaluation must not be billed as the conversation";
const TITLE_SPEND_IS_RECORDED: &str = "a title costs real tokens and must not be dropped";
const TITLE_TEXT: &str = "Sqlite ledger work";
const GENERATED_TITLE: &str = "Named on demand";
const PICKER_MISSED_IT: &str = "the picker lists stored sessions from this workspace";
const PICKER_KEPT_IT: &str = "a session erased from the store must leave the list with it";
const PICKER_NEEDS_ROWS: &str = "the list has to be longer than one row for an end to exist";
/// Stored sessions beyond the one this process has open.
const PICKER_ROWS: usize = 3;
const TITLE_CONTEXT_SIZE: u32 = 1234;
const TASK_ID: &str = "task1";
pub(crate) const RESEARCH_NAME: &str = "research";
const SUB_TOOL_ID: &str = "sub_t1";
const TOOL_OUTPUT_LINE: &str = "hello from the subagent";
const LATE_MODEL_SPEC: &str = "zai/glm-5";
const VIEW_DEFAULT_MSG: &str = "an app with no recognized stored mode opens every card";
const VIEW_PERSIST_MSG: &str = "a chosen mode must survive the app that chose it";
const VIEW_CYCLE_MSG: &str = "the shortcut must reach every mode and come back";
const VIEW_STATE_KEY: StateKey = StateKey {
    name: "ui.view",
    class: StateClass::Persistent,
};
const UNKNOWN_VIEW_MODE: &str = "unknown-view";
const HINT_PLUGIN: &str = "statusline";
const HINT_TEXT: &str = "2/4 staged";
const HINT_STYLE: &str = "fg";
const RETRY_MESSAGE: &str = "overloaded";
const RETRY_ATTEMPT: u32 = 2;
const RETRY_DELAY_MS: u64 = 604_800_000;
const RETRY_DELAY: Duration = Duration::from_millis(RETRY_DELAY_MS);
const RETRY_SIBLING_TASK_ID: &str = "retry-sibling";
const MAIN_RETRY_WIPED: &str = "a subagent's event must not clear the main chat's backoff";
const MAIN_RETRY_STUCK: &str = "the main chat's own next event ends its backoff";
const SUBAGENT_RETRY_MISSING: &str = "a task's own chat must show the backoff it is waiting out";
const SUBAGENT_RETRY_LEAKED: &str = "a task's backoff is not the main conversation's";
const BACKOFF_SLEPT: &str = "a backgrounded task's countdown has to keep the loop awake";
/// Asserted without the seconds, which tick down between the event and the
/// render.
const RETRY_COUNTDOWN_PREFIX: &str = "retrying in";
const COUNTDOWN_DRAWN_FOR_THE_WRONG_CHAT: &str = "the bar draws the backoff of the chat on screen";
const BAR_CLAIMS_THE_COUNTDOWN: &str = "the bar borrows a countdown to draw, it does not own it";
const RETRY_CONTROL_MISPLACED: &str = "only the main chat's countdown can be clicked";
const TASK_THINKING: &str = "xhigh";
const FAST_CHIP: &str = "[fast]";
/// Wide enough that no chip has to abbreviate, so a footer assertion reads the
/// level rather than the ladder.
const FOOTER_WIDTH: u16 = 120;
const TASK_LEVEL_MISSING: &str = "a task footer must name the level that task runs at";
const TASK_FAST_MISSING: &str = "a task footer must report the task's own fast flag";
const TASK_UNSTORED: &str = "a live task must reach the session record";
const TASK_UNRESTORED: &str = "a stored task must come back as a chat";
const MISSING_DIR: &str = "gone";
const RESUMED_PROMPT: &str = "carry me over";
const CONVERSATION_PERMISSION_PATTERN: &str = "just *";
const FIRST_TURN_PROMPT: &str = "the turn that earns a row";
const SONNET_SPEC: &str = "anthropic/claude-sonnet-4-5";
const OPUS_SPEC: &str = "anthropic/claude-opus-4-8";
const PLAIN_MODEL_SPEC: &str = "ollama/qwen3";
const WALK_TIMEOUT: Duration = Duration::from_secs(5);
/// Stands in for a size the provider measured, baseline included.
const MEASURED_CONTEXT: u32 = 100_000;
/// The rewind fixture holds a few dozen bytes of chat, far below this, so it
/// doubles as the window the gauge is allowed to land in.
const SMALL_HISTORY: u32 = 1_000;
const GOAL_CONDITION: &str = "all focused tests pass";
const GOAL_CHIP_PREFIX: &str = "[goal \u{b7}";
const CONTEXT_COMMAND: &str = "/context";
const LOGS_COMMAND: &str = "/logs";
const DOCS_COMMAND: &str = "/docs";
const DOCS_ADDRESS: &str = "permissions#modes";
const DOCS_QUERY: &str = "shell timeout";
const IMAGE_PATH_PASTE: &str = "file:///tmp/nonexistent.png";
const IMAGE_UNDER_MODAL: &str = "a paste into an open modal reached the composer behind it";
const CONTEXT_UPPERCASE_COMMAND: &str = "/CONTEXT";
const CONTEXT_TRAILING_COMMAND: &str = "/context   ";
const CONTEXT_ALL_COMMAND: &str = "/context all";
const CONTEXT_ALL_UPPERCASE_COMMAND: &str = "/context ALL";
const CONTEXT_ALL_TRAILING_COMMAND: &str = "/context all   ";
const CONTEXT_INVALID_COMMAND: &str = "/context everything";
const CONTEXT_EXCESS_ARGS_COMMAND: &str = "/context all extra";
const CONTEXTUAL_PROMPT: &str = "/contextual";
const STORAGE_COMMAND: &str = "/storage";
const STORAGE_ALL_COMMAND: &str = "/storage ALL";
const STORAGE_INVALID_COMMAND: &str = "/storage everything";
const MISSING_STORAGE_REFRESH: &str = "opening /storage must request a measurement";
const TOOLS_COMMAND: &str = "/tools";
const TOOLS_UPPERCASE_COMMAND: &str = "/TOOLS";
const TOOLS_EXCESS_ARGS_COMMAND: &str = "/tools all";
const TOOLSMITH_PROMPT: &str = "/toolsmith";
const SKILLS_COMMAND: &str = "/skills";
const SKILLS_EXCESS_ARGS_COMMAND: &str = "/skills all";
const CONTEXT_EXISTING_MESSAGE: &str = "existing conversation";
const MAIN_CONTEXT_SPEC: &str = "test/main-context";
const PLAN_CONTEXT_SPEC: &str = "anthropic/plan-context";
const PLAN_CONTEXT_MODEL_ID: &str = "plan-context";
const PLAN_CONTEXT_WINDOW_LABEL: &str = "128k";
const PLAN_STATUS_MODEL_MISSING: &str = "status bar must show the running Plan model";
const PLAN_STATUS_WINDOW_MISSING: &str = "status bar must show the running Plan context window";
const TEST_MODEL_SPEC: &str = "anthropic/test-model";
const OTHER_MODEL_ID: &str = "other-model";
const OTHER_MODEL_SPEC: &str = "anthropic/other-model";
const MODEL_UNASKED: &str = "a toggle must ask for the model its mode was left on";
const MODEL_CHURNED: &str = "a toggle must not ask for the model already selected";
const BINDING_OVERRIDDEN: &str = "a bound Plan job must decide what a plan run uses";
const POLICY_IGNORED: &str = "a model the policy refuses must not be asked for";
const BASELINE_EARLY: &str = "a swapped model must stay pending until a message carries it";
const BASELINE_UNSETTLED: &str = "a message must settle the model it carried";
const BASELINE_WRONG: &str = "the baseline must be the model the next turn runs on";
const PICK_MISFILED: &str = "a choice must be remembered against the mode it was made in";
const TASK_CONTEXT_SPEC: &str = "test/task-context";
const INITIAL_CONTEXT_PROVIDER: &str = "Initial context provider";
const UPDATED_CONTEXT_PROVIDER: &str = "Updated context provider";
const CHAT_CONTEXT_WINDOW: u32 = 128_000;
const COMPACTION_CONTEXT_WINDOW: u32 = 1_000_000;

fn set_zone(app: &mut App, zone: SelectionZone, area: Rect) {
    app.zones.push(SelectableZone { area, zone });
}

fn build_app(dir: StateDir, writer: Arc<StorageWriter>) -> App {
    build_app_with_lua(dir, writer, LuaCommandReader::empty())
}

fn build_app_with_lua(
    dir: StateDir,
    writer: Arc<StorageWriter>,
    lua_commands: LuaCommandReader,
) -> App {
    let model = test_model();
    let mut session = AppSession::new("test-model", "");
    let workspace = dir
        .path()
        .join("caudra-ui-test-workspaces")
        .join(session.id.to_string());
    std::fs::create_dir_all(&workspace).unwrap();
    #[cfg(unix)]
    for path in [workspace.parent().unwrap(), &workspace] {
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(PRIVATE_TEST_DIRECTORY_MODE),
        )
        .unwrap();
    }
    session.set_cwd(workspace.to_string_lossy().into_owned());
    App::new(
        &model,
        session,
        dir,
        Arc::new(ArcSwapOption::empty()),
        McpSnapshotReader::empty(),
        McpConfigErrors::new(PathBuf::new()),
        lua_commands,
        KeymapReader::empty(),
        HintReader::empty(),
        writer,
        UiConfig::default(),
        100,
        caudra_storage::log::DEFAULT_MAX_FILES,
        docs_fixture::library,
        Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig {
                rules: vec![],
                decision_engine: true,
                ..Default::default()
            },
            PathBuf::from("/tmp"),
            Arc::default(),
        )),
        Arc::from([]),
        caudra_lua::EventHandle::disconnected_for_test(),
        Arc::new(caudra_config::ModelPolicy::default()),
        Arc::new(caudra_agent::prompt::profile::PromptProfileCatalog::default()),
        None,
        FeatureFlags::all(),
    )
}

fn test_writer(dir: StateDir) -> StorageWriter {
    StorageWriter::new(dir, flume::unbounded().0)
}

thread_local! {
    /// Retiring or checkpointing a session writes to `caudra.sqlite` under
    /// the state dir, so pointing every app at `env::temp_dir()` put the whole
    /// suite on one database. Under load the write lock timed out and
    /// `retire_current_session` failed, taking `/new` and session loading down
    /// with it. Tests on one thread still share a dir, which is what a second
    /// app in the same test wants; tests that run at the same time never can.
    static TEST_STATE_DIR: TempDir = private_tempdir();
}

pub(crate) fn private_tempdir() -> TempDir {
    let mut builder = Builder::new();
    #[cfg(unix)]
    builder.permissions(fs::Permissions::from_mode(PRIVATE_TEST_DIRECTORY_MODE));
    builder
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap()
}

fn test_state_dir() -> StateDir {
    TEST_STATE_DIR.with(|dir| StateDir::from_path(dir.path().to_path_buf()))
}

pub(crate) fn test_app() -> App {
    let dir = test_state_dir();
    let mut app = build_app(dir.clone(), Arc::new(test_writer(dir)));
    let (shared_queue, _rx) = shared_queue::queue();
    app.queue.set_shared(shared_queue);
    app
}

/// A `test_app` past its idle splash, whose animation would mask every
/// other cadence.
fn app_without_splash() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::TextDelta { text: "hi".into() }));
    app.update(done_event());
    app
}

/// Hands back the slot providers publish their model lists into, since the app
/// keeps no handle to it once the picker owns it.
fn app_with_model_slot() -> (App, Arc<ArcSwapOption<Vec<String>>>) {
    let models = Arc::new(ArcSwapOption::empty());
    let mut app = test_app();
    app.model_picker = ModelPicker::new(Arc::clone(&models));
    (app, models)
}

/// Hands back the end a plugin publishes hints through. That is the Lua thread
/// in production, and this test here. Seeding the watch from the new reader is
/// what `App::new` does, and skipping it would make the first poll report the
/// swap itself.
fn app_with_hints() -> (App, HintWriterHandle) {
    let (writer, reader) = hint_writer_pair();
    let mut app = test_app();
    app.hints = Watch::seeded(reader.load_full());
    app.hint_reader = reader;
    (app, writer)
}

pub(crate) fn tempdir_app() -> (TempDir, StateDir, Arc<StorageWriter>, App) {
    let tmp = private_tempdir();
    let dir = StateDir::from_path(tmp.path().to_path_buf());
    let writer = Arc::new(test_writer(dir.clone()));
    let app = build_app(dir.clone(), Arc::clone(&writer));
    (tmp, dir, writer, app)
}

pub(crate) fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Msg {
    Msg::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

pub(crate) fn agent_msg(event: AgentEvent) -> Msg {
    agent_msg_with_run_id(event, 1)
}

fn agent_msg_with_run_id(event: AgentEvent, run_id: u64) -> Msg {
    Msg::Agent(Box::new(Envelope {
        task: None,
        event,
        subagent: None,
        run_id,
        workflow: None,
    }))
}

pub(crate) fn permission_event(id: &str, command: &str) -> AgentEvent {
    AgentEvent::PermissionRequest(Box::new(PermissionRequest::from_legacy(
        id.into(),
        ToolKey::native("bash"),
        vec![command.into()],
        serde_json::json!({"command": command}),
        Path::new("/tmp"),
        true,
    )))
}

fn done() -> AgentEvent {
    AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        reason: DoneReason::EndTurn,
    }
}

fn done_event() -> Msg {
    agent_msg(done())
}

pub(crate) fn end_turn(app: &mut App) {
    app.update(done_event());
}

fn subagent_info(parent_id: &str, name: &str) -> SubagentInfo {
    subagent_info_with_tx(parent_id, name, None)
}

fn subagent_info_with_tx(
    parent_id: &str,
    name: &str,
    answer_tx: Option<flume::Sender<String>>,
) -> SubagentInfo {
    SubagentInfo {
        parent_tool_use_id: parent_id.into(),
        task_id: parent_id.into(),
        name: name.into(),
        prompt: None,
        model: None,
        thinking: None,
        fast: false,
        answer_tx,
        steer_tx: None,
    }
}

fn subagent_msg(event: AgentEvent, parent_id: &str, name: Option<&str>) -> Msg {
    subagent_msg_with_run_id(event, parent_id, name, 1)
}

fn subagent_msg_with_run_id(
    event: AgentEvent,
    parent_id: &str,
    name: Option<&str>,
    run_id: u64,
) -> Msg {
    Msg::Agent(Box::new(Envelope {
        task: None,
        event,
        subagent: Some(subagent_info(parent_id, name.unwrap_or("Agent"))),
        run_id,
        workflow: None,
    }))
}

fn subagent_msg_with_prompt(
    event: AgentEvent,
    parent_id: &str,
    name: Option<&str>,
    prompt: Option<&str>,
) -> Msg {
    let mut info = subagent_info(parent_id, name.unwrap_or("Agent"));
    info.prompt = prompt.map(String::from);
    Msg::Agent(Box::new(Envelope {
        task: None,
        event,
        subagent: Some(info),
        run_id: 1,
        workflow: None,
    }))
}

fn subagent_msg_with_model(event: AgentEvent, parent_id: &str, name: &str, model: &str) -> Msg {
    let mut info = subagent_info(parent_id, name);
    info.model = Some(model.into());
    Msg::Agent(Box::new(Envelope {
        task: None,
        event,
        subagent: Some(info),
        run_id: 1,
        workflow: None,
    }))
}

fn subagent_msg_with_info(event: AgentEvent, subagent: SubagentInfo) -> Msg {
    Msg::Agent(Box::new(Envelope {
        task: None,
        event,
        subagent: Some(subagent),
        run_id: 1,
        workflow: None,
    }))
}

fn tool_start(id: &str, tool: &str) -> AgentEvent {
    AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: id.into(),
        effect: ToolEffect::Unknown,
        tool: tool.into(),
        summary: id.into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))
}

fn turn_complete(usage: TokenUsage, model: &str, cost: Option<f64>) -> AgentEvent {
    turn_complete_from(TEST_PROVIDER, usage, model, cost, LedgerPurpose::Chat, 0)
}

fn turn_complete_from(
    provider: &str,
    usage: TokenUsage,
    model: &str,
    cost: Option<f64>,
    purpose: LedgerPurpose,
    context_window: u32,
) -> AgentEvent {
    AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
        message: Default::default(),
        usage,
        model: model.into(),
        provider: provider.into(),
        purpose,
        cost,
        billing: Billing::Api,
        context_size: None,
        context_window,
    }))
}

fn context_snapshot(spec: &str, window: u32) -> ContextSnapshot {
    ContextSnapshot {
        readiness: ContextReadiness::PreparedNextRequest,
        mode: AgentMode::Build,
        audience: ToolAudience::MAIN,
        model: ContextModel {
            spec: spec.to_owned(),
            provider_display_name: TEST_PROVIDER.to_owned(),
        },
        window: ContextWindow {
            tokens: window,
            reserve: ContextReserve::Disabled,
        },
        usage: ContextUsage::default(),
        measured: None,
        inventory: ContextInventory::default(),
    }
}

fn current_context_snapshot(app: &App) -> ContextSnapshot {
    let mut snapshot = context_snapshot(&app.state.model.spec(), app.state.model.context_window);
    snapshot.mode = app.execution_agent_mode();
    snapshot
}

fn tools_preview_app() -> App {
    let mut app = test_app();
    app.state.mode = Mode::Build;
    app.state.applied_mode = Mode::Build;
    let policy = serde_json::from_value(serde_json::json!({"overrides":{"plan":"eager"}})).unwrap();
    app.tools_preview_source = Some(Arc::new(tools_preview_source(Default::default(), policy)));
    app.state.plan = PlanState::Drafting(Path::new(&app.state.session.cwd).join(PREVIEW_PLAN_FILE));
    app.execution_mode = Some(Arc::new(ArcSwap::from_pointee(AgentMode::Build)));
    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(HistorySnapshot::default())));
    app
}

#[test_case(false; "local")]
#[test_case(true; "remote")]
fn tools_preview_same_model_toggles_are_isolated_and_cached(remote: bool) {
    let mut app = tools_preview_app();
    if remote {
        app.state.plan = PlanState::RemoteDrafting(PlanRef::new(COMMITTED_PLAN_REF).unwrap());
    }
    let plan_before = app.state.plan.clone();
    let mode_before = app.execution_mode.as_ref().unwrap().load_full();
    let applied_before = (app.state.applied_mode, app.state.applied_model.clone());
    let history_before = app.shared_history.as_ref().unwrap().load_full();
    let store = ContextStore::new();
    let mut committed = current_context_snapshot(&app);
    committed.measured = Some(CHAT_CONTEXT_WINDOW);
    store.publisher(ContextKey::Main).publish(committed.clone());
    app.context_store = Some(store.clone());
    assert!(app.queue_and_notify(queued_msg(PREVIEW_QUEUED_TEXT)));
    let queued = app.queue.pending_prompts();
    for (mode, basis) in [
        (Mode::Build, SELECTED_BUILD_TOOLS),
        (Mode::Plan, SELECTED_PLAN_TOOLS),
        (Mode::Build, SELECTED_BUILD_TOOLS),
        (Mode::Plan, SELECTED_PLAN_TOOLS),
    ] {
        app.state.mode = mode;
        app.execute_tools();
        assert_eq!(app.tools_basis, basis);
        let snapshot = app.tools_snapshot.held().unwrap();
        assert_eq!(snapshot.mode, app.agent_mode());
        assert_eq!(
            snapshot.inventory.builtins.tools[0].state,
            ContextBuiltinState::Declared
        );
        assert_eq!(
            snapshot.inventory.builtins.request_tokens(),
            snapshot.usage.system_tools
        );
        assert_eq!(snapshot.measured, None);
        assert_eq!(app.poll_tools_snapshot(), Dirty::NO);
        assert!(Arc::ptr_eq(&snapshot, &app.tools_snapshot.held().unwrap()));
        app.tools_modal.close();
        app.execute_tools();
        assert!(Arc::ptr_eq(&snapshot, &app.tools_snapshot.held().unwrap()));
        assert_eq!(
            store.latest(&ContextKey::Main).unwrap().as_ref(),
            &committed
        );
        assert!(Arc::ptr_eq(
            &mode_before,
            &app.execution_mode.as_ref().unwrap().load_full()
        ));
        assert!(Arc::ptr_eq(
            &history_before,
            &app.shared_history.as_ref().unwrap().load_full()
        ));
        assert_eq!(
            (app.state.applied_mode, app.state.applied_model.clone()),
            applied_before
        );
        assert_eq!(app.state.plan, plan_before);
        assert_eq!(app.queue.pending_prompts(), queued);
        assert_eq!(app.continuation_input().mode, AgentMode::Build);
    }
    if let Some(path) = app.state.plan.path() {
        assert!(!path.exists());
    }
}

#[test_case(Mode::Build, Mode::Plan, false, EXECUTING_BUILD_TOOLS; "running_build")]
#[test_case(Mode::Plan, Mode::Build, false, EXECUTING_PLAN_TOOLS; "running_plan")]
#[test_case(Mode::Build, Mode::Plan, true, EXECUTING_BUILD_TOOLS; "permission_build")]
#[test_case(Mode::Plan, Mode::Build, true, EXECUTING_PLAN_TOOLS; "permission_plan")]
fn tools_preview_active_turn_uses_committed_mode(
    executing: Mode,
    selected: Mode,
    permission: bool,
    basis: &str,
) {
    let mut app = tools_preview_app();
    app.state.mode = executing;
    app.execution_mode
        .as_ref()
        .unwrap()
        .store(Arc::new(app.agent_mode()));
    app.state.applied_mode = executing;
    app.status = Status::Streaming;
    app.run_id = 1;
    let snapshot = app.tools_preview_source.as_ref().unwrap().snapshot(
        &app.state.model,
        &app.state.model,
        &app.state.thinking,
        &app.execution_agent_mode(),
        true,
        &app.state.session.cwd,
        &[],
    );
    let store = ContextStore::new();
    store.publisher(ContextKey::Main).publish(snapshot);
    app.context_store = Some(store.clone());
    let committed = store.latest(&ContextKey::Main).unwrap();
    app.state.mode = selected;
    if permission {
        app.update(agent_msg(permission_event(
            PREVIEW_PERMISSION_ID,
            PREVIEW_PERMISSION_COMMAND,
        )));
        assert!(app.permission_prompt.is_open());
    }
    app.execute_tools();
    assert_eq!(app.tools_basis, basis);
    assert!(Arc::ptr_eq(&app.tools_snapshot.held().unwrap(), &committed));
    assert!(app.tools_preview_cache.snapshot.is_none());
    app.state.mode = executing;
    assert_eq!(app.poll_tools_snapshot(), Dirty::YES);
    assert!(!app.tools_basis.contains("pending"));
    assert!(Arc::ptr_eq(&app.tools_snapshot.held().unwrap(), &committed));
    app.state.mode = selected;
    assert_eq!(app.poll_tools_snapshot(), Dirty::YES);
    assert_eq!(app.tools_basis, basis);
}

#[test]
fn tools_preview_missing_target_stays_readonly_without_allocating() {
    let mut app = tools_preview_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::None;
    app.execute_tools();
    let snapshot = app.tools_snapshot.get().unwrap();
    assert_eq!(app.tools_basis, SELECTED_PLAN_TOOLS);
    assert_eq!(snapshot.mode, AgentMode::ReadOnly);
    assert_eq!(
        snapshot.inventory.builtins.tools[0].state,
        ContextBuiltinState::Disabled
    );
    assert_eq!(
        snapshot.inventory.builtins.tools[0].reason,
        Some(PLAN_REQUIRED)
    );
    assert_eq!(app.state.plan, PlanState::None);
    assert_eq!(app.execution_agent_mode(), AgentMode::Build);
}

/// The idle preview is the next message's run: the selected mode with the
/// session's plan binding, whatever mode created it.
#[test_case(true, ContextBuiltinState::Declared, None; "bound")]
#[test_case(false, ContextBuiltinState::Disabled, Some(PLAN_REQUIRED); "unbound")]
fn tools_preview_idle_build_follows_the_session_plan(
    bound: bool,
    state: ContextBuiltinState,
    reason: Option<&str>,
) {
    let mut app = tools_preview_app();
    if !bound {
        app.state.plan = PlanState::None;
    }
    app.execute_tools();
    assert_eq!(app.tools_basis, SELECTED_BUILD_TOOLS);
    let snapshot = app.tools_snapshot.held().unwrap();
    assert_eq!(snapshot.mode, AgentMode::Build);
    assert_eq!(snapshot.inventory.builtins.tools[0].state, state);
    assert_eq!(snapshot.inventory.builtins.tools[0].reason, reason);
    app.state.plan = if bound {
        PlanState::None
    } else {
        PlanState::RemoteDrafting(PlanRef::new(COMMITTED_PLAN_REF).unwrap())
    };
    assert_eq!(app.poll_tools_snapshot(), Dirty::YES);
    assert_ne!(
        app.tools_snapshot.held().unwrap().inventory.builtins.tools[0].state,
        state
    );
}

#[test]
fn tools_preview_refreshes_selection_model_history_and_policy_source() {
    let mut app = tools_preview_app();
    app.execute_tools();
    app.state.mode = Mode::Plan;
    assert_eq!(app.poll_tools_snapshot(), Dirty::YES);
    assert_eq!(app.tools_basis, SELECTED_PLAN_TOOLS);
    app.state.model = Model::from_spec("openai/gpt-6-astra").unwrap();
    assert_eq!(app.poll_tools_snapshot(), Dirty::YES);
    assert_eq!(
        app.tools_snapshot.get().unwrap().model.spec,
        app.state.model.spec()
    );
    app.shared_history
        .as_ref()
        .unwrap()
        .store(Arc::new(HistorySnapshot::default()));
    assert_eq!(app.poll_tools_snapshot(), Dirty::YES);
    let policy =
        serde_json::from_value(serde_json::json!({"overrides":{"plan":"disabled"}})).unwrap();
    app.tools_preview_source = Some(Arc::new(tools_preview_source(Default::default(), policy)));
    assert_eq!(app.poll_tools_snapshot(), Dirty::YES);
    assert_eq!(
        app.tools_snapshot.get().unwrap().inventory.builtins.tools[0].state,
        ContextBuiltinState::Disabled
    );
    assert_eq!(app.poll_tools_snapshot(), Dirty::NO);
}

#[test]
fn tools_preview_bound_plan_model_uses_provider_capabilities() {
    let mut app = tools_preview_app();
    app.state.mode = Mode::Plan;
    let selected = app.state.model.spec();
    let applied = app.state.applied_model.clone();
    let execution = app.execution_mode.as_ref().unwrap().load_full();
    let source = Arc::get_mut(app.tools_preview_source.as_mut().unwrap()).unwrap();
    source.registry = Arc::new(ToolRegistry::new());
    native::register(&source.registry, FeatureFlags::default()).unwrap();
    let previous = model_registry::binding(ModelPurpose::Plan);
    model_registry::set_binding_and_persist(
        ModelPurpose::Plan,
        Binding::Exact(PREVIEW_VISION_MODEL.into()),
        &app.storage,
    )
    .unwrap();
    app.execute_tools();
    restore_plan_binding(previous, &app.storage);

    let snapshot = app.tools_snapshot.get().unwrap();
    assert_eq!(snapshot.model.spec, PREVIEW_VISION_MODEL);
    let image = snapshot
        .inventory
        .builtins
        .tools
        .iter()
        .find(|tool| tool.name == VIEW_IMAGE_TOOL_NAME)
        .unwrap();
    assert_ne!(image.state, ContextBuiltinState::Disabled);
    assert!(image.tokens > 0);
    assert_eq!(app.state.model.spec(), selected);
    assert_eq!(app.state.applied_model, applied);
    assert!(Arc::ptr_eq(
        &execution,
        &app.execution_mode.as_ref().unwrap().load_full()
    ));
    assert!(!app.state.plan.path().unwrap().exists());
}

#[test_case(ModelPurpose::Subagent; "task_model")]
#[test_case(ModelPurpose::Best; "task_model_indirection")]
fn tools_preview_refreshes_changed_purpose_bindings(purpose: ModelPurpose) {
    let mut app = tools_preview_app();
    let previous = model_registry::binding(purpose);
    model_registry::clear_binding_and_persist(purpose, &app.storage).unwrap();
    app.execute_tools();
    let before = app.tools_snapshot.held().unwrap();
    let binding = Binding::Exact(PREVIEW_VISION_MODEL.into());
    model_registry::set_binding_and_persist(purpose, binding, &app.storage).unwrap();
    let rebound = app.poll_tools_snapshot();
    let after_bind = app.tools_snapshot.held().unwrap();
    app.tools_modal.close();
    model_registry::clear_binding_and_persist(purpose, &app.storage).unwrap();
    app.execute_tools();
    let after_unbind = app.tools_snapshot.held().unwrap();
    let unchanged = app.poll_tools_snapshot();
    if let Some(previous) = previous {
        model_registry::set_binding_and_persist(purpose, previous, &app.storage).unwrap();
    }

    assert_eq!(rebound, Dirty::YES);
    assert!(!Arc::ptr_eq(&before, &after_bind));
    assert!(!Arc::ptr_eq(&after_bind, &after_unbind));
    assert_eq!(before.model, after_bind.model);
    assert_eq!(before.mode, after_bind.mode);
    assert_eq!(app.execution_agent_mode(), AgentMode::Build);
    assert_eq!(unchanged, Dirty::NO);
}

#[test]
fn tools_preview_rejects_same_model_stale_admission_snapshot() {
    let mut app = tools_preview_app();
    let store = ContextStore::new();
    store
        .publisher(ContextKey::Main)
        .publish(current_context_snapshot(&app));
    app.context_store = Some(store.clone());
    app.status = Status::Streaming;
    app.state.mode = Mode::Plan;
    app.execution_mode
        .as_ref()
        .unwrap()
        .store(Arc::new(app.agent_mode()));
    app.execute_tools();
    assert!(app.tools_snapshot.get().is_none());
    assert!(app.main_context_snapshot().is_none());
    store
        .publisher(ContextKey::Main)
        .publish(current_context_snapshot(&app));
    assert_eq!(app.poll_tools_snapshot(), Dirty::YES);
    assert_eq!(app.tools_snapshot.get().unwrap().mode, app.agent_mode());
}

#[test]
fn tools_preview_task_uses_own_mode_and_audience() {
    let mut app = priced_subagent_app();
    let store = ContextStore::new();
    let mut snapshot = context_snapshot(TASK_CONTEXT_SPEC, CHAT_CONTEXT_WINDOW);
    snapshot.mode = AgentMode::ReadOnly;
    snapshot.audience = ToolAudience::GENERAL_SUB;
    store
        .publisher(ContextKey::task(TASK_ID))
        .publish(snapshot.clone());
    app.context_store = Some(store);
    app.execute_tools();
    for mode in [Mode::Plan, Mode::Build] {
        app.state.mode = mode;
        assert_eq!(app.poll_tools_snapshot(), Dirty::NO);
        assert_eq!(app.tools_snapshot.get(), Some(&snapshot));
        assert_eq!(app.tools_basis, "Task · Read-only");
    }
}

fn tool_results_submitted() -> AgentEvent {
    AgentEvent::ToolResultsSubmitted {
        message: Box::new(Message::user(String::new())),
    }
}

#[test]
fn typing_and_submit() {
    let mut app = test_app();
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(&actions[0], Action::SendMessage(s) if s.message == "hi"));
    assert_eq!(app.status, Status::Streaming);
    // Regression check: the bubble has to be on screen the same frame we
    // submit, otherwise it briefly sits one row too high before snapping down.
    assert_eq!(
        app.main_chat().last_message_role(),
        Some(&DisplayRole::User),
    );
    assert_eq!(app.main_chat().last_message_text(), "hi");
}

#[test]
fn mailbox_wake_starts_without_an_empty_user_bubble() {
    let mut app = test_app();
    let actions = app.start_mailbox_run(vec![Message::observation("failed".into())]);

    assert!(matches!(
        &actions[..],
        [Action::SendMessage(input)]
            if input.message.is_empty()
                && input.preamble.len() == 1
                && input.preamble[0].is_observation()
    ));
    assert_eq!(app.status, Status::Streaming);
    assert!(app.main_chat().segment_search_texts().is_empty());
}

#[test_case(false; "peer_admission_wake")]
#[test_case(true; "suppressed_peer_wake")]
fn empty_mailbox_wake_leaves_peer_claim_to_admission(suppressed: bool) {
    let mut app = test_app();
    app.automatic_wakes_suppressed = suppressed;
    let run_id = app.run_id;
    let actions = app.start_mailbox_run(Vec::new());
    if suppressed {
        assert!(actions.is_empty());
        assert_eq!(app.run_id, run_id);
    } else {
        assert!(matches!(actions.as_slice(), [Action::SendMessage(input)]
            if input.message.is_empty()
                && input.preamble.is_empty()
                && input.images.is_empty()
                && !input.resume));
        assert_eq!(app.run_id, run_id + 1);
    }
    assert!(app.main_chat().segment_search_texts().is_empty());
    assert_eq!(app.automatic_wakes_suppressed, suppressed);
}

#[test_case(false; "report_after_final_answer")]
#[test_case(true; "stop_suppresses_report")]
fn normal_done_keeps_background_continuation_eligible(stopped: bool) {
    const REPORT: &str = "The audit found a missing validation check.";
    const FINAL: &str = "Implementation is ready; the audit is still running.";
    let mut app = streaming_app();
    app.main_chat()
        .push(DisplayMessage::new(DisplayRole::Assistant, FINAL.into()));
    end_turn(&mut app);
    assert_eq!(app.status, Status::Idle);
    assert!(app.state.goal.status().is_none());
    let run_id = app.run_id;
    if stopped {
        app.handle_cancel();
        assert!(app.cancelling_run.is_none());
    }
    let observation = Message::observation(REPORT.into());
    let actions = app.start_mailbox_run(vec![observation.clone()]);
    if stopped {
        assert!(actions.is_empty());
        assert!(app.automatic_wakes_suppressed);
        type_and_submit(&mut app, REPORT);
        assert!(!app.automatic_wakes_suppressed);
    } else {
        assert_eq!(app.run_id, run_id + 1);
        assert!(matches!(actions.as_slice(), [Action::SendMessage(input)]
            if input.message.is_empty() && input.preamble.len() == 1 && input.preamble[0].first_text_content() == observation.first_text_content()));
        assert_eq!(app.main_chat().last_message_text(), FINAL);
        app.update(agent_msg_with_run_id(done(), app.run_id));
        assert!(
            !app.start_mailbox_run(vec![Message::observation(REPORT.into())])
                .is_empty()
        );
    }
}

#[test_case(DoneReason::MaxTurns; "turn_budget")]
#[test_case(DoneReason::MaxTokens; "token_budget")]
#[test_case(DoneReason::Cancelled; "cancelled")]
fn terminal_limits_suppress_background_continuation(reason: DoneReason) {
    const REPORT: &str = "A later report must not restart an exhausted turn.";
    let mut app = streaming_app();
    app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        reason,
    }));
    assert!(
        app.start_mailbox_run(vec![Message::observation(REPORT.into())])
            .is_empty()
    );
}

mod background_runtime {
    use std::path::Path;
    use std::sync::{Arc, Once};
    use std::time::Duration;

    use arc_swap::ArcSwap;
    use caudra_agent::background::ShellJobMetadata;
    use caudra_agent::prompt::profile::PromptProfileCatalog;
    use caudra_agent::tools::native::task::configure_background;
    use caudra_agent::tools::{ToolRegistry, native};
    use caudra_agent::types::BACKGROUND_EVENT_RUN_ID;
    use caudra_agent::{
        AgentConfig, AgentEvent, DoneReason, Envelope, McpConfigErrors, SubagentHistoryStore,
        SubagentInfo, TaskProvenance,
    };
    use caudra_agent::{BatchToolStatus, SubagentActivity, ToolDoneEvent, ToolOutput};
    use caudra_config::{Feature, FeatureFlags, ModelPolicy};
    use caudra_lua::EventHandle;
    use caudra_providers::provider::{BoxFuture, Provider};
    use caudra_providers::{
        AgentError, CacheKey, ContentBlock, Message, Model, ModelInfo, ProviderEvent,
        RequestOptions, Role, StandingReminderKind, StopReason, StreamResponse, Timeouts,
        project_messages,
    };
    use caudra_storage::id::{CaudraId, SessionRef};
    use caudra_storage::sessions::PermissionMode;
    use caudra_workflow::{
        LaunchRequest, RunStatus, WorkflowEvent, WorkflowRequest, WorkflowResponse,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
    use futures_lite::future::poll_once;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{App, Msg, Status, subagent_info, test_app};
    use crate::AppSession;
    use crate::agent::shared_queue::QueueItem;
    use crate::agent::{AgentHandles, ModelSlot, QueuedMessage, reserve_background_transition};
    use crate::app::SubmitOutcome;
    use crate::app::tasks::TaskActivity;
    use crate::chat::Chat;
    use crate::components::mode_submission::{ModeSubmissionAction, ModeSubmissionChoice};
    use crate::components::status_bar::StatusBarHitTarget;
    use crate::components::{Action, DisplayRole, key};
    use crate::repaint::{Dirty, expect::OWED};

    const PARENT: &str = "Implement the change and launch the independent audit.";
    const CHILD: &str = "Audit the implementation independently.";
    const TASK: &str = "background-audit";
    const SUCCEEDED: &str = "succeeded";
    const REPORT_CALL: &str = "audit-report";
    const REPORT: &str = "The audit found a missing input validation check.";
    const SHELL_CALL: &str = "owned-shell";
    const SHELL_COMMAND: &str = "printf shell-output";
    const SHELL_PROGRESS: &str = "shell-owner-progress";
    const STALE_SHELL_PROGRESS: &str = "stale-shell-progress";
    const SHELL_TIMEOUT_MS: u64 = 120_000;
    const FINAL: &str = "Implementation is ready; the audit is still running.";
    const SUMMARY: &str = "The audit was delegated and was last observed running.";
    const EMPTY_BACKGROUND: &str = "No active background work";
    const ENVIRONMENT_RESTATED: &str =
        "the summary took the only environment block, so compaction must restate it";
    const SNAPSHOT_STAYS_LAST: &str =
        "restated reminders land before the background snapshot, which stays last";
    const COMPACT_COMMAND: &str = "/compact";
    const REVIEW_WORKFLOW: &str = "review-changes";
    const WORKFLOW_BUDGET: u32 = 1;
    const TIMEOUT: Duration = Duration::from_secs(30);
    const REPEATED_CHECKPOINTS: usize = 16;
    static REGISTER: Once = Once::new();

    struct Request {
        messages: Vec<Message>,
        reply: flume::Sender<Result<StreamResponse, AgentError>>,
    }

    struct ControlledProvider(flume::Sender<Request>);

    impl Provider for ControlledProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let (reply, result) = flume::bounded(1);
                self.0
                    .send_async(Request {
                        messages: messages.to_vec(),
                        reply,
                    })
                    .await
                    .map_err(|_| AgentError::Channel)?;
                result.recv_async().await.map_err(|_| AgentError::Channel)?
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn response(content: ContentBlock, stop: StopReason) -> Result<StreamResponse, AgentError> {
        Ok(StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![content],
                ..Default::default()
            },
            stop_reason: Some(stop),
            ..Default::default()
        })
    }

    fn final_response() -> Result<StreamResponse, AgentError> {
        response(
            ContentBlock::Text { text: FINAL.into() },
            StopReason::EndTurn,
        )
    }

    struct Fixture {
        app: Box<App>,
        handles: AgentHandles,
        history: SubagentHistoryStore,
        requests: flume::Receiver<Request>,
        child: Request,
    }

    impl Fixture {
        async fn after_final() -> Self {
            Self::after_final_with_reminders(true).await
        }

        async fn after_final_with_reminders(show_reminders: bool) -> Self {
            REGISTER.call_once(|| {
                native::register(ToolRegistry::global(), FeatureFlags::all()).unwrap()
            });
            let mut app = Box::new(test_app());
            app.ui_config.show_reminders = show_reminders;
            app.chats[0] = Chat::new(
                app.chats[0].name.clone(),
                Path::new(&app.state.session.cwd),
                app.ui_config.clone(),
                EventHandle::disconnected_for_test(),
            );
            app.state.mode = super::Mode::Build;
            app.permissions.set_session_mode(Some(PermissionMode::Yolo));
            app.storage_writer
                .save_sync(Arc::clone(&app.state.session))
                .unwrap();
            let (requests, received) = flume::unbounded();
            let slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
                model: app.state.model.clone(),
                provider: Arc::new(ControlledProvider(requests)),
            }));
            let history = SubagentHistoryStore::default();
            let handles = AgentHandles::spawn(
                &slot,
                Vec::new(),
                Vec::new(),
                None,
                AgentConfig {
                    generate_titles: false,
                    compaction_requirements: false,
                    features: FeatureFlags::NONE.with(Feature::Workflows),
                    ..Default::default()
                },
                app.ui_config.tool_output_lines,
                &app.permissions,
                Some(SessionRef::from(app.state.session.id)),
                None,
                Timeouts::default(),
                EventHandle::disconnected_for_test(),
                None,
                McpConfigErrors::new(Default::default()),
                Arc::new(ModelPolicy::default()),
                app.state.goal.clone(),
                history.clone(),
                None,
                Arc::new(PromptProfileCatalog::default()),
                Some(app.storage.clone()),
                Arc::clone(&app.change_recorder),
                None,
                None,
                None,
                None,
                true,
            );
            handles.apply_to_app(&mut app);
            let SubmitOutcome::Started(actions) = app.submit_prompt(QueuedMessage {
                text: PARENT.into(),
                images: Vec::new(),
                mentions: Vec::new(),
                commits: Vec::new(),
                paste_ranges: Vec::new(),
            }) else {
                panic!("parent prompt was not admitted");
            };
            enqueue(&app, &handles, actions);
            received
                .recv_async()
                .await
                .unwrap()
                .reply
                .send(response(
                    ContentBlock::ToolUse {
                        id: TASK.into(),
                        name: "task".into(),
                        input: json!({"description": TASK, "prompt": CHILD, "background": true}),
                        thought_signature: None,
                    },
                    StopReason::ToolUse,
                ))
                .unwrap();
            let mut child = None;
            for _ in 0..2 {
                let request = received.recv_async().await.unwrap();
                for message in &request.messages {
                    for block in &message.content {
                        if let ContentBlock::ToolResult {
                            is_error: true,
                            content,
                            ..
                        } = block
                        {
                            panic!("task admission failed: {content}");
                        }
                    }
                }
                if request.messages.iter().any(|message| {
                    message
                        .first_text_content()
                        .is_some_and(|text| text.starts_with(CHILD))
                }) {
                    child = Some(request);
                } else {
                    request.reply.send(final_response()).unwrap();
                }
            }
            let mut fixture = Self {
                app,
                handles,
                history,
                requests: received,
                child: child.expect("child request"),
            };
            fixture.drain_parent().await;
            assert_eq!(fixture.app.status, Status::Idle);
            assert_eq!(
                fixture.handles.background.as_ref().unwrap().active_count(),
                1
            );
            fixture
        }

        async fn compact(&mut self) -> Request {
            assert_eq!(self.app.status, Status::Idle);
            let actions = self.app.execute_command(super::cmd(COMPACT_COMMAND), 0);
            assert!(matches!(actions.as_slice(), [Action::Compact]));
            self.handles.queue.push(QueueItem::Compact {
                run_id: self.app.run_id,
            });
            self.requests.recv_async().await.unwrap()
        }

        async fn drain_parent(&mut self) {
            loop {
                let envelope = self.handles.agent_rx.recv_async().await.unwrap();
                if envelope.run_id == self.app.run_id
                    && matches!(envelope.event, AgentEvent::QueueDrained)
                {
                    break;
                }
                self.app.update(Msg::Agent(Box::new(envelope)));
            }
            self.app.flush_background_delivery(true).await.unwrap();
        }

        async fn drain_response(&mut self) {
            loop {
                let envelope = self.handles.agent_rx.recv_async().await.unwrap();
                let done = envelope.subagent.is_none()
                    && envelope.task.is_none()
                    && envelope.run_id == self.app.run_id
                    && matches!(envelope.event, AgentEvent::Done { .. });
                self.app.update(Msg::Agent(Box::new(envelope)));
                if done {
                    break;
                }
            }
            self.app.flush_background_delivery(true).await.unwrap();
        }

        fn envelope(&self, event: AgentEvent) -> Envelope {
            let task = self
                .handles
                .background
                .as_ref()
                .unwrap()
                .list()
                .into_iter()
                .find(|task| task.call_id == TASK)
                .unwrap();
            Envelope {
                event,
                subagent: Some(SubagentInfo {
                    task_id: task.task_id.clone(),
                    ..subagent_info(TASK, TASK)
                }),
                run_id: BACKGROUND_EVENT_RUN_ID,
                workflow: None,
                task: Some(Arc::new(TaskProvenance {
                    session_id: self.app.state.session.id,
                    task_id: task.task_id,
                    invocation_id: task.invocation_id,
                })),
            }
        }

        async fn close(self) {
            self.handles
                .background
                .as_ref()
                .unwrap()
                .shutdown()
                .await
                .unwrap();
            self.handles.cancel();
        }
    }

    fn enqueue(app: &App, handles: &AgentHandles, actions: Vec<Action>) {
        for action in actions {
            let Action::SendMessage(input) = action else {
                panic!("expected parent run");
            };
            handles.queue.push(QueueItem::Message {
                text: input.message.clone(),
                image_count: input.images.len(),
                paste_ranges: Vec::new(),
                input,
                run_id: app.run_id,
                admission: caudra_agent::PromptAdmission::Queue,
                displayed: true,
            });
        }
    }

    async fn bounded(test: impl Future<Output = ()>) {
        futures_lite::future::or(test, async {
            smol::Timer::after(TIMEOUT).await;
            panic!("background TUI test did not reach its event barrier");
        })
        .await;
    }

    #[test_case(false, false; "hidden_task")]
    #[test_case(true, false; "visible_task")]
    #[test_case(false, true; "hidden_task_and_workflow")]
    #[test_case(true, true; "visible_task_and_workflow")]
    fn idle_compact_samples_finished_background_work_before_done(
        show_reminders: bool,
        with_workflow: bool,
    ) {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final_with_reminders(show_reminders).await;
            let background = fixture.handles.background.as_ref().unwrap().clone();
            let workflow = fixture.handles.workflow_handle().unwrap();
            let workflow_child = if with_workflow {
                let WorkflowResponse::Started(run) = workflow
                    .request(WorkflowRequest::Start(LaunchRequest {
                        name: REVIEW_WORKFLOW.into(),
                        args: json!({"scope": PARENT}),
                        agent_budget: Some(WORKFLOW_BUDGET),
                    }))
                    .await
                    .unwrap()
                else {
                    panic!("workflow was not started");
                };
                Some((run.run_id, fixture.requests.recv_async().await.unwrap()))
            } else {
                None
            };
            let request = fixture.compact().await;
            let before = fixture.handles.history.load_full();
            while let Ok(envelope) = fixture.handles.agent_rx.try_recv() {
                fixture.app.update(Msg::Agent(Box::new(envelope)));
            }
            assert_eq!(
                fixture.app.task_activity(),
                TaskActivity {
                    agents: 1 + usize::from(with_workflow),
                    shells: 0
                }
            );
            fixture.child.reply.send(final_response()).unwrap();
            background.notified().await;
            if let Some((run_id, child)) = workflow_child {
                child.reply.send(Err(AgentError::Channel)).unwrap();
                loop {
                    let envelope = fixture.handles.agent_rx.recv_async().await.unwrap();
                    let finished = matches!(
                        &envelope.event,
                        AgentEvent::Workflow(event) if matches!(
                            event.as_ref(),
                            WorkflowEvent::Snapshot(run) if run.run_id == run_id && run.status == RunStatus::Completed
                        )
                    );
                    fixture.app.update(Msg::Agent(Box::new(envelope)));
                    if finished {
                        break;
                    }
                }
            }
            assert_eq!(workflow.active_count(), 0);
            assert_eq!(background.active_count(), 0);
            assert_eq!(fixture.app.task_activity(), TaskActivity::default());
            assert!(background.has_pending());
            assert!(fixture.handles.history.load().messages == before.messages);
            request
                .reply
                .send(response(
                    ContentBlock::Text {
                        text: SUMMARY.into(),
                    },
                    StopReason::EndTurn,
                ))
                .unwrap();
            let mut reminder = None;
            let mut summary_seen = false;
            let mut restated = false;
            let mut done = false;
            loop {
                let envelope = fixture.handles.agent_rx.recv_async().await.unwrap();
                if envelope.subagent.is_none() && envelope.run_id == fixture.app.run_id {
                    match &envelope.event {
                        AgentEvent::TurnComplete(turn) => {
                            assert!(turn.message.is_compaction_summary);
                            assert!(!done);
                            summary_seen = true;
                        }
                        AgentEvent::Injected {
                            text,
                            task_event: None,
                            peer_event: None,
                        } if text.contains(caudra_agent::prompt::ENVIRONMENT_MARKER) => {
                            assert!(summary_seen);
                            assert!(reminder.is_none(), "{SNAPSHOT_STAYS_LAST}");
                            restated = true;
                        }
                        AgentEvent::Injected {
                            text,
                            task_event: None,
                            peer_event: None,
                        } => {
                            assert!(summary_seen);
                            assert!(!done);
                            assert!(reminder.is_none());
                            assert!(text.contains(EMPTY_BACKGROUND), "{text}");
                            let snapshot = fixture.handles.history.load_full();
                            let messages = project_messages(&snapshot.messages).unwrap();
                            let last = messages.last().unwrap();
                            assert_eq!(last.first_text_content(), Some(text.as_str()));
                            assert_eq!(
                                last.standing_reminder,
                                Some(StandingReminderKind::BackgroundWork)
                            );
                            assert!(last.task_event.is_none());
                            reminder = Some(text.clone());
                        }
                        AgentEvent::Done { .. } => {
                            assert!(reminder.is_some());
                            assert!(!done);
                            done = true;
                        }
                        AgentEvent::QueueDrained => break,
                        AgentEvent::Error { message } => panic!("{message}"),
                        _ => {}
                    }
                }
                assert!(
                    fixture
                        .app
                        .update(Msg::Agent(Box::new(envelope)))
                        .is_empty()
                );
                fixture.app.checkpoint();
            }
            assert!(done);
            assert!(restated, "{ENVIRONMENT_RESTATED}");
            assert_eq!(fixture.app.status, Status::Idle);
            assert!(fixture.requests.is_empty());
            assert!(fixture.handles.queue.is_empty());
            assert!(background.has_pending());
            fixture.app.flush_background_delivery(true).await.unwrap();
            let saved =
                AppSession::load(fixture.app.state.session.id, &fixture.app.storage).unwrap();
            let history = crate::active_session_history(&saved).unwrap();
            let messages = project_messages(&history).unwrap();
            let last = messages.last().unwrap();
            assert_eq!(
                last.standing_reminder,
                Some(StandingReminderKind::BackgroundWork)
            );
            assert_eq!(last.first_text_content(), reminder.as_deref());
            assert!(last.task_event.is_none());
            let live = fixture.app.main_chat();
            assert_eq!(
                (0..live.message_count())
                    .filter(|&index| {
                        let row = live.message_at(index).unwrap();
                        row.role == DisplayRole::Injected
                            && Some(row.text.as_str()) == reminder.as_deref()
                    })
                    .count(),
                usize::from(show_reminders),
            );
            fixture.close().await;
        }));
    }

    #[test_case(Some(AgentError::Channel); "failed")]
    #[test_case(Some(AgentError::Cancelled); "cancelled")]
    #[test_case(None; "empty")]
    fn idle_compact_does_not_snapshot_an_unsuccessful_summary(error: Option<AgentError>) {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let request = fixture.compact().await;
            let before = fixture.handles.history.load_full();
            request
                .reply
                .send(match error {
                    Some(error) => Err(error),
                    None => Ok(StreamResponse::default()),
                })
                .unwrap();
            loop {
                let envelope = fixture.handles.agent_rx.recv_async().await.unwrap();
                let failed = envelope.subagent.is_none()
                    && matches!(envelope.event, AgentEvent::Error { .. });
                assert!(!matches!(
                    envelope.event,
                    AgentEvent::Injected { .. } | AgentEvent::Done { .. }
                ));
                fixture.app.update(Msg::Agent(Box::new(envelope)));
                fixture.app.checkpoint();
                if failed {
                    break;
                }
            }
            assert_eq!(fixture.handles.history.load().messages, before.messages);
            assert_eq!(
                crate::active_session_history(&fixture.app.state.session).unwrap(),
                *before.messages
            );
            assert!(fixture.requests.is_empty());
            fixture.close().await;
        }));
    }

    #[test_case(true; "prepared_tools_already_capability_configured")]
    fn prepared_task_tools_match_session_capability(supported: bool) {
        smol::block_on(bounded(async {
            let fixture = Fixture::after_final().await;
            assert_eq!(fixture.handles.background.is_some(), supported);
            let prepared = fixture.handles.btw_prompt.load();
            let mut configured = prepared.tools.clone();
            configure_background(&mut configured, supported);
            assert_eq!(prepared.tools, configured);
            let definitions = prepared.tools.as_array().unwrap();
            let task = definitions
                .iter()
                .find(|tool| tool["name"] == "task")
                .unwrap();
            assert!(
                task["input_schema"]["properties"]
                    .get("background")
                    .is_some()
            );
            assert!(
                definitions
                    .iter()
                    .any(|tool| tool["name"] == "task_control")
            );
            fixture.close().await;
        }));
    }

    #[test_case(false, false; "next_after_result")]
    #[test_case(true, false; "guide_wakes_parked_parent")]
    #[test_case(false, true; "queued_plan_after_build_result")]
    fn parked_parent_keeps_next_until_background_result_is_processed(guide: bool, plan: bool) {
        const NEXT: &str = "Start the next independent change.";
        const GUIDE: &str = "Include the audit finding in the current change.";
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let background = fixture.handles.background.as_ref().unwrap().clone();
            assert!(fixture.app.waiting_for_background());
            let prompt = |text: &str| QueuedMessage {
                text: text.into(),
                images: Vec::new(),
                mentions: Vec::new(),
                commits: Vec::new(),
                paste_ranges: Vec::new(),
            };
            if plan {
                fixture.app.enter_plan();
                let outcome = fixture.app.submit_prompt(prompt(NEXT));
                assert!(matches!(outcome, SubmitOutcome::NeedsModeDecision(_)));
                fixture.app.handle_submit_outcome(outcome);
                fixture
                    .app
                    .handle_mode_submission(ModeSubmissionAction::Select(
                        ModeSubmissionChoice::Queue,
                    ));
                fixture.app.toggle_mode();
            } else {
                assert!(matches!(
                    fixture.app.submit_prompt(prompt(NEXT)),
                    SubmitOutcome::Queued
                ));
            }
            assert_eq!(fixture.handles.queue.len(), 1);
            assert!(fixture.requests.is_empty());
            if guide {
                assert!(matches!(
                    fixture.app.submit_prompt_with_admission(
                        prompt(GUIDE),
                        caudra_agent::PromptAdmission::Steer
                    ),
                    SubmitOutcome::Queued
                ));
                let request = fixture.requests.recv_async().await.unwrap();
                assert!(request.messages.iter().any(|message| {
                    message
                        .first_text_content()
                        .is_some_and(|text| text.contains(GUIDE))
                }));
                request.reply.send(final_response()).unwrap();
                fixture.drain_response().await;
                assert_eq!(background.active_count(), 1);
                assert_eq!(fixture.handles.queue.len(), 1);
            }
            fixture.child.reply.send(final_response()).unwrap();
            background.notified().await;
            let messages = background.claim_messages().unwrap();
            assert!(!messages.is_empty());
            let actions = fixture.app.start_mailbox_run(messages);
            enqueue(&fixture.app, &fixture.handles, actions);
            let request = fixture.requests.recv_async().await.unwrap();
            assert!(!fixture.handles.execution_mode.load().is_planning());
            assert!(
                request
                    .messages
                    .iter()
                    .any(|message| message.task_event.is_some())
            );
            assert!(
                !request
                    .messages
                    .iter()
                    .any(|message| message.first_text_content() == Some(NEXT))
            );
            assert_eq!(fixture.handles.queue.len(), 1);
            request.reply.send(final_response()).unwrap();
            fixture.drain_response().await;
            let _ = fixture.app.reconcile_tasks();
            assert!(!fixture.app.has_session_work());
            fixture.handles.queue.allow_next_turn(true);
            let request = fixture.requests.recv_async().await.unwrap();
            assert_eq!(fixture.handles.execution_mode.load().is_planning(), plan);
            assert!(request.messages.iter().any(|message| {
                message
                    .first_text_content()
                    .is_some_and(|text| text.contains(NEXT))
            }));
            request.reply.send(final_response()).unwrap();
            fixture.drain_parent().await;
            fixture.close().await;
        }));
    }

    #[test_case(false; "report_after_final_while_child_runs")]
    #[test_case(true; "stop_suppresses_postfinal_report")]
    fn real_report_after_final_resumes_only_an_armed_parent(stopped: bool) {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let background = fixture.handles.background.as_ref().unwrap().clone();
            fixture
                .child
                .reply
                .send(response(
                    ContentBlock::ToolUse {
                        id: REPORT_CALL.into(),
                        name: "report_to_parent".into(),
                        input: json!({"message": REPORT}),
                        thought_signature: None,
                    },
                    StopReason::ToolUse,
                ))
                .unwrap();
            fixture.child = fixture.requests.recv_async().await.unwrap();
            background.notified().await;
            assert_eq!(background.active_count(), 1);
            if stopped {
                fixture.app.handle_cancel();
                background.stop().await.unwrap();
                assert!(background.claim_messages().unwrap().is_empty());
                assert!(!background.has_pending());
            } else {
                let messages = background.claim_messages().unwrap();
                assert!(messages.iter().any(|message| message.task_event.is_some()));
                let actions = fixture.app.start_mailbox_run(messages);
                enqueue(&fixture.app, &fixture.handles, actions);
                let request = fixture.requests.recv_async().await.unwrap();
                assert!(
                    request
                        .messages
                        .iter()
                        .any(|message| message.task_event.is_some())
                );
                request.reply.send(final_response()).unwrap();
                fixture.drain_parent().await;
                assert!(!background.has_pending());
                assert_eq!(background.active_count(), 1);
            }
            fixture.close().await;
        }));
    }

    #[test_case("valid", true; "current_child_survives_main_done")]
    #[test_case("session", false; "foreign_session")]
    #[test_case("invocation", false; "stale_invocation")]
    #[test_case("child", false; "mismatched_child")]
    #[test_case("missing", false; "sentinel_without_provenance")]
    fn session_event_provenance_is_validated(change: &str, accepted: bool) {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let mut envelope = fixture.envelope(AgentEvent::AuthRequired);
            match change {
                "session" => {
                    Arc::make_mut(envelope.task.as_mut().unwrap()).session_id = CaudraId::generate()
                }
                "invocation" => {
                    Arc::make_mut(envelope.task.as_mut().unwrap()).invocation_id =
                        CaudraId::generate().to_string()
                }
                "child" => {
                    envelope.subagent.as_mut().unwrap().parent_tool_use_id = REPORT_CALL.into()
                }
                "missing" => envelope.task = None,
                _ => {}
            }
            fixture.app.run_id += 1;
            fixture.app.update(Msg::Agent(Box::new(envelope)));
            assert_eq!(
                fixture.app.pending_input != super::PendingInput::None,
                accepted
            );
            fixture.close().await;
        }));
    }

    #[test]
    fn workspace_rollback_releases_pending_reports_without_rearming() {
        smol::block_on(bounded(async {
            let fixture = Fixture::after_final().await;
            let background = fixture.handles.background.as_ref().unwrap().clone();
            fixture.child.reply.send(final_response()).unwrap();
            background.notified().await;
            assert!(background.has_pending());
            let transition = reserve_background_transition(&background).unwrap();
            background.rearm();
            assert!(background.claim_messages().unwrap().is_empty());
            let task_id = background.list()[0].task_id.clone();
            assert!(background.promote(&task_id).await.is_err());
            drop(transition);
            assert!(background.has_pending());
            let messages = background.claim_messages().unwrap();
            assert!(messages.iter().any(|message| message.task_event.is_some()));
            background.release_messages(&messages);
            fixture.close().await;
        }));
    }

    #[test_case(false; "unsaved_claim_released_after_cached_final_save")]
    #[test_case(true; "compacted_claim_accepted_after_final_save")]
    fn final_idle_save_reconciles_background_claims_outside_projection(saved: bool) {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let background = fixture.handles.background.as_ref().unwrap().clone();
            fixture.child.reply.send(final_response()).unwrap();
            background.notified().await;
            let messages = background.claim_messages().unwrap();
            fixture.app.background_claims = messages.clone();
            if saved {
                fixture.app.shared_history = None;
                fixture.app.state.session_mut().meta.history_head = None;
                fixture
                    .app
                    .state
                    .session_mut()
                    .replace_messages(crate::history_items(&messages));
                fixture
                    .app
                    .storage_writer
                    .save_sync(Arc::clone(&fixture.app.state.session))
                    .unwrap();
                fixture.app.state.session_mut().replace_messages(Vec::new());
            }
            fixture.app.flush_background_delivery(false).await.unwrap();
            assert!(!background.has_pending());
            let revision = fixture.app.background_saved_revision;
            fixture.app.flush_background_delivery(true).await.unwrap();
            assert_eq!(fixture.app.background_saved_revision, revision);
            assert!(fixture.app.background_claims.is_empty());
            assert_eq!(background.has_pending(), !saved);
            fixture.close().await;
        }));
    }

    #[test_case(false; "identical_checkpoints_coalesce")]
    #[test_case(true; "only_latest_revision_is_queued")]
    fn parked_admission_does_not_block_checkpoints_or_input(latest: bool) {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let background = fixture.handles.background.as_ref().unwrap().clone();
            let admission = background.workflow_admission().await.unwrap();
            fixture.app.background_delivery.invalidate();
            let started = fixture.app.background_delivery.jobs_started();
            fixture.app.persist_background_delivery(true).unwrap();
            if latest {
                fixture.app.shared_history = None;
                fixture.app.state.session_mut().meta.history_head = None;
                fixture
                    .app
                    .state
                    .session_mut()
                    .replace_messages(crate::history_items(&[Message::user(REPORT.into())]));
            }
            for _ in 0..REPEATED_CHECKPOINTS {
                fixture.app.persist_background_delivery(true).unwrap();
            }
            assert_eq!(fixture.app.background_delivery.jobs_started(), started + 1);
            assert!(fixture.app.background_delivery.pending());
            fixture.app.update(Msg::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )));
            assert!(!fixture.app.input_box.is_empty());
            drop(admission);
            fixture.app.flush_background_delivery(true).await.unwrap();
            assert_eq!(
                fixture.app.background_delivery.jobs_started(),
                started + 1 + u64::from(latest)
            );
            fixture.app.persist_background_delivery(true).unwrap();
            assert!(!fixture.app.background_delivery.pending());
            fixture.close().await;
        }));
    }

    #[test]
    fn stale_final_worker_cannot_release_a_newer_turns_claim() {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let background = fixture.handles.background.as_ref().unwrap().clone();
            fixture.child.reply.send(final_response()).unwrap();
            background.notified().await;
            let old_claim = background.claim_messages().unwrap();
            let admission = background.workflow_admission().await.unwrap();
            fixture.app.background_delivery.invalidate();
            fixture.app.persist_background_delivery(true).unwrap();
            fixture.app.run_id += 1;
            fixture.app.rearm_background();
            let fence = Arc::clone(&fixture.app.background_delivery.fence);
            let parent = fence.enter().await;
            background.release_messages(&old_claim);
            let new_claim = background.claim_messages().unwrap();
            assert!(!new_claim.is_empty());
            fixture.app.background_claims = new_claim;
            drop(admission);
            fixture.app.flush_background_delivery(false).await.unwrap();
            assert!(!background.has_pending());
            assert!(!fixture.app.background_claims.is_empty());
            drop(parent);
            fixture.close().await;
        }));
    }

    #[test]
    fn stop_during_parked_reconciliation_is_responsive_and_rearm_waits_for_drain() {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let background = fixture.handles.background.as_ref().unwrap().clone();
            let admission = background.workflow_admission().await.unwrap();
            fixture.app.background_delivery.invalidate();
            fixture.app.persist_background_delivery(true).unwrap();
            fixture.app.handle_cancel();
            fixture.app.stop_background_work();
            fixture.app.rearm_background();
            fixture.app.update(Msg::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )));
            assert!(!fixture.app.input_box.is_empty());
            let fence = Arc::clone(&fixture.app.background_delivery.fence);
            assert!(poll_once(fence.enter()).await.is_none());
            drop(admission);
            fixture.app.flush_background_delivery(true).await.unwrap();
            assert_eq!(background.active_count(), 0);
            assert!(!fixture.app.automatic_wakes_suppressed);
            assert!(poll_once(fence.enter()).await.is_some());
            assert!(background.workflow_admission().await.is_ok());
            fixture.app.poll_background_delivery().unwrap();
            fixture.close().await;
        }));
    }

    #[test]
    fn workflow_runtime_is_bound_to_the_session_supervisor_before_tools() {
        smol::block_on(bounded(async {
            let fixture = Fixture::after_final().await;
            let background = fixture.handles.background.as_ref().unwrap().clone();
            let workflow = fixture.handles.workflow_handle().unwrap();
            assert!(workflow.bind_background(background).await.is_err());
            fixture.close().await;
        }));
    }

    #[test_case(false; "live_invocation_receives_auth_reply")]
    #[test_case(true; "stopped_invocation_rejects_queued_auth_reply")]
    fn interaction_response_rechecks_invocation_liveness(stopped: bool) {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let background = fixture.handles.background.as_ref().unwrap().clone();
            let (sender, receiver) = flume::unbounded();
            let mut envelope = fixture.envelope(AgentEvent::AuthRequired);
            envelope.subagent.as_mut().unwrap().answer_tx = Some(sender);
            fixture.app.update(Msg::Agent(Box::new(envelope)));
            if stopped {
                background.stop().await.unwrap();
                background.rearm();
            }
            fixture.app.handle_submit_with_admission(
                super::Submission::empty(),
                caudra_agent::PromptAdmission::Queue,
            );
            assert_eq!(receiver.try_recv().is_ok(), !stopped);
            fixture.close().await;
        }));
    }

    #[test_case(false; "invocation_changed")]
    #[test_case(true; "generation_changed_under_gate")]
    fn task_picker_mutations_are_async_and_fenced(generation_changed: bool) {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let background = fixture.handles.background.as_ref().unwrap().clone();
            let mut task = background.list().remove(0);
            let id = task.task_id.clone();
            let before = background.status(&id).unwrap().state;
            let admission = background.workflow_admission().await.unwrap();
            if !generation_changed {
                task.invocation_id.push_str("-obsolete");
            }
            fixture.app.handle_task_picker_action(
                crate::components::task_picker::TaskPickerAction::Control {
                    task: Box::new(task),
                    promote: false,
                },
            );
            fixture.app.update(Msg::Key(key(KeyCode::Char('x'))));
            assert!(!fixture.app.input_box.is_empty());
            if generation_changed {
                background.suppress_wakes();
                background.rearm();
            }
            drop(admission);
            fixture.app.flush_task_controls().await;
            assert_eq!(background.status(&id).unwrap().state, before);
            fixture.close().await;
        }));
    }

    #[test_case(false; "main_idle")]
    #[test_case(true; "child_view")]
    fn activity_counts_runtime_admission_and_settlement_without_stale_chats(child_view: bool) {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let runtime = fixture.handles.background.as_ref().unwrap().clone();
            let task = runtime.list().remove(0);
            let mut chats = fixture.app.chats.split_off(1);
            assert_eq!(
                fixture.app.task_activity(),
                TaskActivity {
                    agents: 1,
                    shells: 0
                }
            );
            assert_eq!(test_app().task_activity(), TaskActivity::default());
            fixture.app.chats.append(&mut chats);
            if child_view {
                fixture.app.focus_task(&task.task_id).unwrap();
            }
            let active_chat = fixture.app.active_chat;
            assert_eq!(fixture.app.status, Status::Idle);
            assert!(super::rendered(&mut fixture.app).contains("[tasks · 1]"));
            fixture.child.reply.send(final_response()).unwrap();
            loop {
                runtime.notified().await;
                if !runtime.status(&task.task_id).unwrap().active() {
                    break;
                }
            }
            assert_eq!(fixture.app.task_activity(), TaskActivity::default());
            assert_eq!(fixture.app.reconcile_tasks(), Dirty::YES, "{OWED}");
            assert!(!super::rendered(&mut fixture.app).contains("[tasks ·"));
            assert!(
                fixture
                    .app
                    .status_hits
                    .iter()
                    .all(|hit| hit.target != StatusBarHitTarget::Tasks)
            );
            assert_eq!(fixture.app.active_chat, active_chat);
            assert!(fixture.app.task_hint_text().is_some());
            fixture.close().await;
        }));
    }

    #[test_case(false, false; "main_shell")]
    #[test_case(true, false; "child_owned_shell_in_child_chat")]
    #[test_case(false, true; "stale_main_shell")]
    #[test_case(true, true; "stale_child_shell")]
    fn activity_shell_click_opens_shell_modal_without_switching_chat(child: bool, stale: bool) {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let runtime = fixture.handles.background.as_ref().unwrap().clone();
            let owner = runtime.list().remove(0);
            let scope = if child {
                fixture.app.focus_task(&owner.task_id).unwrap();
                runtime.child_scope(&owner.invocation_id)
            } else {
                runtime.main_scope()
            };
            let card = scope
                .admit_shell(
                    ShellJobMetadata {
                        call_id: SHELL_CALL.into(),
                        root_call_id: SHELL_CALL.into(),
                        command: SHELL_COMMAND.into(),
                        workdir: ".".into(),
                        timeout_ms: SHELL_TIMEOUT_MS,
                        mode: "build".into(),
                    },
                    &fixture.history,
                    |cancel, _| async move {
                        cancel.cancelled().await;
                        ToolDoneEvent::error(SHELL_CALL.into(), SHELL_PROGRESS)
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                fixture.app.task_activity(),
                TaskActivity {
                    agents: 1,
                    shells: 1
                }
            );
            let index = fixture.app.active_chat;
            let chats = fixture.app.chats.len();
            let hit = super::status_hit(&mut fixture.app, StatusBarHitTarget::Shells);
            fixture.app.update(super::mouse_event(
                MouseEventKind::Moved,
                hit.area.x,
                hit.area.y,
            ));
            assert_eq!(fixture.app.status_hover, Some(StatusBarHitTarget::Shells));
            if stale {
                runtime.stop().await.unwrap();
                assert_eq!(fixture.app.task_activity(), TaskActivity::default());
            }
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                assert!(
                    fixture
                        .app
                        .update(super::mouse_event(kind, hit.area.x, hit.area.y))
                        .is_empty()
                );
            }
            assert!(fixture.app.shell_modal.is_open());
            assert!(!fixture.app.task_picker.is_open());
            assert_eq!(fixture.app.status_hover, None);
            assert_eq!(fixture.app.active_chat, index);
            assert_eq!(fixture.app.chats.len(), chats);
            assert_eq!(fixture.app.status, Status::Idle);
            assert_eq!(
                fixture.app.shell_modal.selected_id().as_deref(),
                Some(card.task_id.as_str())
            );
            assert!(super::rendered_wide(&mut fixture.app, 140).contains(SHELL_COMMAND));
            if !stale {
                assert!(runtime.status(&card.task_id).unwrap().active());
                fixture.app.update(Msg::Key(KeyEvent::new(
                    KeyCode::Char('k'),
                    KeyModifiers::CONTROL,
                )));
                fixture.app.flush_task_controls().await;
                assert!(matches!(
                    runtime.status(&card.task_id).unwrap().state.as_str(),
                    "cancelling" | "cancelled"
                ));
                assert!(
                    runtime.status(&owner.task_id).unwrap().active(),
                    "stopping a shell leaves its owner running"
                );
                assert!(fixture.app.shell_modal.is_open());
                runtime.stop().await.unwrap();
            }
            fixture.app.shell_modal.close();
            assert_eq!(fixture.app.task_activity(), TaskActivity::default());
            super::rendered(&mut fixture.app);
            assert!(fixture.app.status_hits.iter().all(|hit| {
                !matches!(
                    hit.target,
                    StatusBarHitTarget::Tasks | StatusBarHitTarget::Shells
                )
            }));
            fixture.close().await;
        }));
    }

    #[test_case(false, false; "main_shell")]
    #[test_case(false, true; "main_batched_shell")]
    #[test_case(true, false; "child_shell")]
    #[test_case(true, true; "child_batched_shell")]
    fn shell_jobs_route_after_receipt_without_creating_chats(child: bool, batch: bool) {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let runtime = fixture.handles.background.as_ref().unwrap().clone();
            let owner = runtime.list().remove(0);
            let scope = if child {
                runtime.child_scope(&owner.invocation_id)
            } else {
                runtime.main_scope()
            };
            let call_id = if batch {
                format!("{SHELL_CALL}:0")
            } else {
                SHELL_CALL.into()
            };
            let card = scope
                .admit_shell(
                    ShellJobMetadata {
                        call_id: call_id.clone(),
                        root_call_id: SHELL_CALL.into(),
                        command: SHELL_COMMAND.into(),
                        workdir: ".".into(),
                        timeout_ms: SHELL_TIMEOUT_MS,
                        mode: "build".into(),
                    },
                    &fixture.history,
                    |cancel, _| async move {
                        cancel.cancelled().await;
                        ToolDoneEvent::error(SHELL_CALL.into(), SHELL_PROGRESS)
                    },
                )
                .await
                .unwrap();
            let index = if child {
                fixture
                    .app
                    .chats
                    .iter()
                    .position(|chat| {
                        chat.task_id()
                            .is_some_and(|id| id.as_ref() == owner.task_id)
                    })
                    .unwrap()
            } else {
                0
            };
            let chats = fixture.app.chats.len();
            let start = if batch {
                super::batch_roster(SHELL_CALL, 1)
            } else {
                super::tool_start(SHELL_CALL, "shell")
            };
            let mut output = ToolOutput::Tasks(vec![card.clone()]);
            if let AgentEvent::ToolStart(start) = &start
                && let Some(ToolOutput::Batch { entries, .. }) = &start.output
            {
                let mut entries = entries.clone();
                entries[0].tool = "shell".into();
                entries[0].status = BatchToolStatus::Success;
                entries[0].output = Some(output);
                output = ToolOutput::Batch {
                    entries,
                    text: String::new(),
                };
            }
            fixture.app.chats[index].handle_event(start, None);
            let provenance = Arc::new(TaskProvenance {
                session_id: runtime.session_id(),
                task_id: card.task_id.clone(),
                invocation_id: card.invocation_id.clone(),
            });
            let owner_info =
                child.then(|| fixture.envelope(AgentEvent::AuthRequired).subagent.unwrap());
            let envelope = |event| {
                Msg::Agent(Box::new(Envelope {
                    event,
                    subagent: owner_info.clone(),
                    run_id: BACKGROUND_EVENT_RUN_ID,
                    workflow: None,
                    task: Some(Arc::clone(&provenance)),
                }))
            };
            fixture
                .app
                .update(envelope(AgentEvent::TaskAdmitted(card.clone())));
            let mut done = ToolDoneEvent::error(SHELL_CALL.into(), SHELL_PROGRESS);
            done.is_error = false;
            done.output = output;
            fixture.app.chats[index].handle_event(AgentEvent::ToolDone(Box::new(done)), None);
            fixture.app.run_id += 1;
            fixture.app.update(envelope(super::progress_event(
                SubagentActivity::tool(Arc::from("shell"), SHELL_PROGRESS),
                1,
            )));
            fixture.app.active_chat = index;
            assert!(super::rendered(&mut fixture.app).contains(SHELL_PROGRESS));
            let mut stale = provenance.as_ref().clone();
            stale.invocation_id.push_str("-stale");
            fixture.app.update(Msg::Agent(Box::new(Envelope {
                event: super::progress_event(
                    SubagentActivity::tool(Arc::from("shell"), STALE_SHELL_PROGRESS),
                    2,
                ),
                subagent: owner_info.clone(),
                run_id: BACKGROUND_EVENT_RUN_ID,
                workflow: None,
                task: Some(Arc::new(stale)),
            })));
            assert!(!super::rendered(&mut fixture.app).contains(STALE_SHELL_PROGRESS));
            let turns = fixture.app.state.turns;
            fixture.app.update(envelope(AgentEvent::Done {
                usage: Default::default(),
                num_turns: 0,
                reason: DoneReason::EndTurn,
            }));
            assert_eq!(fixture.app.state.turns, turns);
            assert_eq!(fixture.app.chats.len(), chats);
            assert!(
                fixture
                    .app
                    .tasks()
                    .iter()
                    .any(|task| task.id.as_ref() == card.task_id)
            );
            fixture
                .app
                .execute_task_control(&format!("status {}", card.task_id));
            fixture.app.update(Msg::Key(key(KeyCode::Enter)));
            assert!(fixture.app.shell_modal.is_open());
            assert!(!fixture.app.task_picker.is_open());
            assert_eq!(
                fixture.app.shell_modal.selected_id().as_deref(),
                Some(card.task_id.as_str())
            );
            assert_eq!(fixture.app.active_chat, index);
            assert_eq!(fixture.app.chats.len(), chats);
            assert_eq!(fixture.app.status, Status::Idle);
            fixture
                .app
                .execute_task_control(&format!("cancel {}", card.task_id));
            fixture.app.flush_task_controls().await;
            assert!(matches!(
                runtime.status(&card.task_id).unwrap().state.as_str(),
                "cancelling" | "cancelled"
            ));
            fixture.close().await;
            assert!(!runtime.status(&card.task_id).unwrap().active());
        }));
    }

    #[test]
    fn mode_selection_preserves_owned_children_and_result_delivery() {
        smol::block_on(bounded(async {
            let mut fixture = Fixture::after_final().await;
            let background = fixture.handles.background.as_ref().unwrap().clone();
            assert!(reserve_background_transition(&background).is_err());
            let task_id = background.list()[0].task_id.clone();
            let generation = background.generation();
            assert!(background.promote(&task_id).await.is_ok());
            fixture.app.enter_plan();
            assert_eq!(fixture.app.state.mode, super::Mode::Plan);
            assert_eq!(background.active_count(), 1);
            assert_eq!(background.generation(), generation);
            assert!(!fixture.app.automatic_wakes_suppressed);
            assert!(reserve_background_transition(&background).is_err());
            fixture.child.reply.send(final_response()).unwrap();
            background.notified().await;
            let messages = background.claim_messages().unwrap();
            assert!(!messages.is_empty());
            let actions = fixture.app.start_mailbox_run(messages);
            enqueue(&fixture.app, &fixture.handles, actions);
            let request = fixture.requests.recv_async().await.unwrap();
            assert!(!fixture.handles.execution_mode.load().is_planning());
            request.reply.send(final_response()).unwrap();
            fixture.drain_parent().await;
            assert_eq!(background.status(&task_id).unwrap().state, SUCCEEDED);
            assert_eq!(fixture.app.state.mode, super::Mode::Plan);
            fixture.close().await;
        }));
    }
}

fn with_text(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
}

/// The composer's select-all lives in the workbench keymap: the app never
/// matches it, the document does, so there is no `key::` const to reach for.
fn select_all_key() -> KeyEvent {
    KeyEvent::new(
        caudra_workbench::keys::SELECT_ALL.code,
        caudra_workbench::keys::SELECT_ALL.modifiers,
    )
}

fn with_image(app: &mut App) {
    let img = ImageSource::new(ImageMediaType::Png, Arc::from("dGVzdA=="));
    app.input_box.attach_image(img);
}

#[test_case(with_text as fn(&mut App)  ; "clears_text")]
#[test_case(with_image as fn(&mut App) ; "clears_image")]
fn ctrl_c_clears_nonempty_input(setup: fn(&mut App)) {
    let mut app = test_app();
    setup(&mut app);
    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(actions.is_empty());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.input_box.is_empty());
}

#[test]
fn ctrl_c_quits_when_input_empty() {
    let mut app = test_app();
    app.status = Status::Idle;
    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::Success);
    assert!(matches!(actions.as_slice(), [Action::ManualExit]));
}

/// Select-all puts the whole draft under the next keystroke, so copy has to
/// reach the clipboard rather than the discard path behind it.
#[test]
fn ctrl_c_copies_a_selection_instead_of_clearing_the_draft() {
    let mut app = test_app();
    with_text(&mut app);
    app.update(Msg::Key(select_all_key()));

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));

    assert!(actions.is_empty());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert_eq!(app.input_box.buffer.value(), "hi");
}

#[test]
fn shift_delete_cuts_the_selection_out_of_the_draft() {
    let mut app = test_app();
    with_text(&mut app);
    app.update(Msg::Key(select_all_key()));

    app.update(Msg::Key(kb::CUT.to_key_event()));

    assert!(app.input_box.buffer.value().is_empty());
}

#[test]
fn ctrl_d_exits_on_second_press_within_timeout() {
    let mut app = test_app();

    let actions = app.update(Msg::Key(kb::EXIT.to_key_event()));
    assert!(actions.is_empty());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.last_exit.is_some());
    assert_eq!(app.status_bar.flash_text(), Some(FLASH_EXIT));

    let actions = app.update(Msg::Key(kb::EXIT.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::Success);
    assert!(matches!(actions.as_slice(), [Action::ManualExit]));
}

#[test]
fn expired_ctrl_d_press_rearms_exit() {
    let mut app = test_app();
    app.last_exit = Some(Instant::now().checked_sub(Duration::from_secs(10)).unwrap());

    let actions = app.update(Msg::Key(kb::EXIT.to_key_event()));

    assert!(actions.is_empty());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.last_exit.is_some());
    assert_eq!(app.status_bar.flash_text(), Some(FLASH_EXIT));
}

#[test_case(Duration::ZERO ; "no_ordinary_flash")]
#[test_case(LONG_FLASH_DURATION ; "long_ordinary_flash")]
fn ctrl_d_confirmation_uses_fixed_window(flash_duration: Duration) {
    for (elapsed, confirmed) in CONFIRMATION_TIMINGS {
        let mut app = test_app();
        app.status_bar.flash_duration = flash_duration;

        assert!(app.update(Msg::Key(kb::EXIT.to_key_event())).is_empty());
        assert_eq!(app.status_bar.clear_expired_hint(), Dirty::NO);
        assert_eq!(app.status_bar.flash_text(), Some(FLASH_EXIT));

        let pressed_at = Instant::now().checked_sub(elapsed).unwrap();
        app.last_exit = Some(pressed_at);
        let actions = app.update(Msg::Key(kb::EXIT.to_key_event()));

        if confirmed {
            assert_eq!(app.exit_request, ExitRequest::Success);
            assert!(matches!(actions.as_slice(), [Action::ManualExit]));
            assert!(app.last_exit.is_none());
        } else {
            assert_eq!(app.exit_request, ExitRequest::None);
            assert!(actions.is_empty());
            assert!(
                app.last_exit
                    .is_some_and(|rearmed_at| rearmed_at > pressed_at)
            );
            assert_eq!(app.status_bar.flash_text(), Some(FLASH_EXIT));
        }
    }
}

#[test]
fn another_key_interrupts_ctrl_d_sequence() {
    let mut app = test_app();
    app.update(Msg::Key(kb::EXIT.to_key_event()));

    app.update(Msg::Key(key(KeyCode::Left)));
    assert!(app.last_exit.is_none());

    let actions = app.update(Msg::Key(kb::EXIT.to_key_event()));
    assert!(actions.is_empty());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.last_exit.is_some());
}

#[test_case(with_text as fn(&mut App)  ; "text")]
#[test_case(with_image as fn(&mut App) ; "image")]
fn ctrl_d_does_not_arm_with_nonempty_input(setup: fn(&mut App)) {
    let mut app = test_app();
    setup(&mut app);

    let actions = app.update(Msg::Key(kb::EXIT.to_key_event()));

    assert!(actions.is_empty());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.last_exit.is_none());
    assert!(!app.input_box.is_empty());
}

#[test]
fn ctrl_d_does_not_arm_while_streaming() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    for _ in 0..2 {
        assert!(app.update(Msg::Key(kb::EXIT.to_key_event())).is_empty());
    }

    assert_eq!(app.status, Status::Streaming);
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.last_exit.is_none());
}

#[test_case(done(), ExitRequest::Success ; "done_exits_success")]
#[test_case(AgentEvent::Error { message: "boom".into() }, ExitRequest::Error ; "error_exits_error")]
fn exit_on_done_flag_triggers_exit(event: AgentEvent, expected: ExitRequest) {
    let mut app = test_app();
    app.exit_on_done = true;
    app.status = Status::Streaming;
    app.run_id = 1;
    let actions = app.update(agent_msg(event));
    assert_eq!(app.exit_request, expected);
    assert!(actions.is_empty());
}

#[test]
fn reset_session_clears_exit_request_source() {
    let mut app = test_app();
    app.exit_on_done = true;
    app.last_exit = Some(Instant::now());
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        reason: DoneReason::EndTurn,
    }));

    app.reset_session();

    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.last_exit.is_none());
}

const TURNS_UNCOUNTED: &str = "a finished exchange must move the session's turn counter";
const TURNS_UNSAVED: &str =
    "the exit summary reads the counter off the session, so it must be saved";
const LEVEL_UNSET: &str = "no level has been chosen yet";
const LEVEL_UNSAVED: &str = "the chosen level must reach disk to survive a restart";
const LEVEL_LEAKED: &str = "one model's depth must not follow the user onto another";
const LEVEL_LOST_ON_RETURN: &str = "returning to a model must restore the level chosen there";

/// Compaction discards the history a count could be derived from, so the
/// exit summary reads a stored counter instead. A deferred goal keeps the
/// turn open, and its extra `Done` must not bill the same exchange twice.
#[test_case(false, 1 ; "a_finished_turn_counts_once")]
#[test_case(true,  0 ; "a_deferred_goal_leaves_the_turn_open")]
fn completed_turns_reach_the_stored_counter(deferred: bool, expected: u64) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.goal_deferred = deferred;

    end_turn(&mut app);
    app.checkpoint();

    assert_eq!(app.state.turns, expected, "{TURNS_UNCOUNTED}");
    assert_eq!(app.state.session.meta.turns, expected, "{TURNS_UNSAVED}");
}

#[test]
fn toggle_mode_state_machine() {
    let tab = |app: &mut App| app.update(Msg::Key(key(KeyCode::Tab)));

    let mut app = test_app();
    assert_eq!(app.state.mode, Mode::Plan);
    let first_path = app.state.plan.path().unwrap().to_path_buf();
    assert!(first_path.to_str().unwrap().contains("plans"));

    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Build);
    assert!(!app.state.plan.is_ready());

    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(app.state.plan.path().unwrap(), first_path);

    app.state.plan.mark_ready();
    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Build);
    assert!(app.state.plan.is_ready());
    assert_eq!(app.state.plan.path().unwrap(), first_path);

    app.state.mode = Mode::Build;
    app.status = Status::Streaming;
    app.run_id = 1;
    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(app.state.plan.path().unwrap(), first_path);
}

#[test]
fn fresh_session_uses_local_plan_until_matching_tool_completion() {
    const PLAN_NOT_LOCAL: &str = "a fresh plan must be allocated under local persistent storage";
    const PLAN_NOT_FORWARDED: &str = "the agent plan path must be the one handed to the agent";
    const PLAN_NOT_COMPLETED: &str = "a successful write to the plan path must complete the plan";

    let (_temp, storage, _writer, mut app) = tempdir_app();
    let plan_path = app
        .state
        .plan
        .path()
        .expect("fresh plan path")
        .to_path_buf();

    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(
        app.state.plan,
        PlanState::Drafting(plan_path.clone()),
        "{PLAN_NOT_LOCAL}"
    );
    assert!(
        plan_path.starts_with(storage.persistent_path()),
        "{PLAN_NOT_LOCAL}"
    );
    assert!(!plan_path.exists());
    assert!(
        matches!(app.agent_mode(), AgentMode::Plan(ref path) if path == &plan_path),
        "{PLAN_NOT_FORWARDED}"
    );

    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "plan-write".into(),
        tool: "file_write".into(),
        output: ToolOutput::Plain("wrote plan".into()),
        is_error: false,
        annotation: None,
        written_path: None,
        written_paths: vec![plan_path.to_string_lossy().into_owned()],
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    }))));

    assert_eq!(
        app.state.plan,
        PlanState::Ready(plan_path),
        "{PLAN_NOT_COMPLETED}"
    );
    assert!(app.plan_form.is_visible(), "{PLAN_NOT_COMPLETED}");
}

#[test]
fn the_model_chord_opens_the_model_picker() {
    let mut app = test_app();

    let actions = press_chord(&mut app, chord::MODEL_PICKER);

    assert!(app.model_picker.is_open());
    assert!(matches!(&actions[..], [Action::RefreshModels]));
}

#[test]
fn the_model_chord_reaches_through_a_visible_plan_form() {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.plan_form.toggle();

    let actions = press_chord(&mut app, chord::MODEL_PICKER);

    assert!(app.model_picker.is_open());
    assert!(matches!(&actions[..], [Action::RefreshModels]));
}

#[test_case(ToolOutput::Plain("wrote 100 bytes to /tmp/plans/test.md".into()), Some("/tmp/plans/test.md".into()), true  ; "write_matching")]
#[test_case(ToolOutput::Diff { path: "/tmp/plans/test.md".into(), before: String::new(), after: String::new(), summary: String::new() }, None, true  ; "edit_matching")]
#[test_case(ToolOutput::Plain("wrote 100 bytes to /tmp/other.rs".into()), Some("/tmp/other.rs".into()), false ; "write_non_matching")]
fn tool_done_transitions_plan_to_ready(
    output: ToolOutput,
    written_path: Option<String>,
    expect_ready: bool,
) {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("/tmp/plans/test.md"));
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write".into(),
        output,
        is_error: false,
        annotation: None,
        written_path,
        written_paths: Vec::new(),
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    }))));

    assert_eq!(app.state.plan.is_ready(), expect_ready);
}

#[test]
fn tool_done_completes_only_the_matching_remote_plan() {
    let expected =
        caudra_workspace::PlanRef::new(format!("plan-{}", "a".repeat(32))).expect("plan ref");
    let other =
        caudra_workspace::PlanRef::new(format!("plan-{}", "b".repeat(32))).expect("plan ref");
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::RemoteDrafting(expected.clone());
    app.state.applied_mode = Mode::Plan;
    app.status = Status::Streaming;
    app.run_id = 1;

    let event = |reference: &caudra_workspace::PlanRef| {
        agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: reference.as_str().into(),
            tool: plan::NAME.into(),
            output: ToolOutput::Plain("wrote plan".into()),
            is_error: false,
            annotation: Some(
                PlanWriteResult::new(
                    PlanTarget::Remote(reference.clone()),
                    HANDOFF_PLAN_CONTENT.into(),
                )
                .annotation()
                .unwrap(),
            ),
            written_path: None,
            written_paths: Vec::new(),
            remote_written_paths: false,
            output_ref: None,
            output_limits: None,
            model_suffix: None,
            model_output: None,
            model_output_from_ref: false,
            accounting: ToolAccounting::default(),
        })))
    };

    app.update(event(&other));
    assert_eq!(app.state.plan, PlanState::RemoteDrafting(expected.clone()));
    app.update(event(&expected));
    assert_eq!(app.state.plan, PlanState::RemoteReady(expected));
}

#[test]
fn altgr_chars_not_swallowed_by_ctrl_handler() {
    let mut app = test_app();
    let altgr_backslash = KeyEvent {
        code: KeyCode::Char('\\'),
        modifiers: KeyModifiers::CONTROL | KeyModifiers::ALT,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    };
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    app.update(Msg::Key(altgr_backslash));
    assert_eq!(app.input_box.buffer.value(), "hi\\");
}

#[test_case(Status::Idle      ; "idle")]
#[test_case(Status::Streaming ; "streaming")]
fn paste_works_regardless_of_status(status: Status) {
    let mut app = test_app();
    app.status = status;
    app.update(Msg::Paste("pasted".into()));
    assert_eq!(app.input_box.buffer.value(), "pasted");
}

#[test_case("a\rb\rc",       "a\nb\nc",      "[Pasted 3 lines] " ; "bare_cr")]
#[test_case("a\r\nb\r\nc",   "a\nb\nc",      "[Pasted 3 lines] " ; "crlf")]
#[test_case("a\r\nb\rc\nd",  "a\nb\nc\nd",   "[Pasted 4 lines] " ; "mixed")]
fn paste_normalizes_line_endings(input: &str, expected: &str, display: &str) {
    let mut app = test_app();
    app.update(Msg::Paste(input.into()));
    assert_eq!(app.input_box.buffer.value(), format!("{expected} "));
    assert_eq!(app.input_box.buffer.display_text(), display);
}

#[test]
fn summarized_paste_can_be_edited_in_modal() {
    let mut app = test_app();
    app.update(Msg::Paste("a\nb\nc".into()));
    app.update(Msg::Key(key(KeyCode::Left)));
    app.update(Msg::Key(key(KeyCode::Left)));
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.paste_editor.is_open());

    app.update(Msg::Key(key(KeyCode::End)));
    app.update(Msg::Key(key(KeyCode::Char('!'))));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    )));

    assert!(!app.paste_editor.is_open());
    assert_eq!(app.input_box.buffer.display_text(), "[Pasted 3 lines] ");
    assert_eq!(app.input_box.buffer.expanded_text(), "a!\nb\nc ");
}

#[test]
fn switching_tasks_closes_paste_editor() {
    let mut app = app_with_subagent();
    app.update(Msg::Paste("a\nb\nc".into()));
    app.update(Msg::Key(key(KeyCode::Left)));
    app.update(Msg::Key(key(KeyCode::Left)));
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.paste_editor.is_open());

    app.active_chat = 1;
    app.update(Msg::Paste("new task text".into()));
    assert!(!app.paste_editor.is_open());
    assert_eq!(app.input_box.buffer.expanded_text(), "a\nb\nc ");
}

#[test]
fn hidden_paste_cannot_trigger_command_palette() {
    let mut app = test_app();
    app.update(Msg::Paste("/new\na\nb".into()));
    assert!(!app.command_palette.is_active());
}

#[test]
fn hidden_paste_cannot_trigger_exit() {
    let mut app = test_app();
    app.update(Msg::Paste("exit\n\n".into()));

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(actions.as_slice(), [Action::SendMessage(_)]));
    assert_eq!(app.exit_request, ExitRequest::None);
}

#[test]
fn enter_edits_focused_paste_before_executing_palette_command() {
    let mut app = test_app();
    for character in "/goal ".chars() {
        app.update(Msg::Key(key(KeyCode::Char(character))));
    }
    app.update(Msg::Paste("a\nb\nc".into()));
    app.update(Msg::Key(key(KeyCode::Left)));
    app.update(Msg::Key(key(KeyCode::Left)));
    assert!(app.command_palette.is_active());

    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    assert!(app.paste_editor.is_open());
    assert!(app.state.goal.snapshot().is_none());
}

#[test]
fn summarized_draft_checkpoints_and_restores() {
    let mut app = test_app();
    app.update(Msg::Paste("a\nb\nc".into()));
    app.checkpoint_with(Duration::ZERO);

    assert_eq!(
        app.state.session.meta.input_draft.as_deref(),
        Some("a\nb\nc ")
    );
    assert_eq!(app.state.session.meta.input_draft_pastes.len(), 1);
    assert_eq!(
        app.state.session.meta.input_draft_pastes.first(),
        Some(&StoredPasteRange { start: 0, end: 5 })
    );

    app.input_box.discard();
    app.restore_display();
    assert_eq!(app.input_box.buffer.display_text(), "[Pasted 3 lines] ");
    assert_eq!(app.input_box.buffer.expanded_text(), "a\nb\nc ");
}

#[test]
fn shell_paste_expands_before_execution() {
    let mut app = test_app();
    app.update(Msg::Paste("! printf test\na\nb".into()));
    assert!(app.is_bash_input());

    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    assert!(!app.input_box.has_pastes());
    assert_eq!(app.input_box.buffer.display_text(), "! printf test\na\nb ");

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(actions.as_slice(), [Action::ShellCommand { .. }]));
}

#[test]
fn paste_file_path_triggers_image_load() {
    let mut app = test_app();
    app.update(Msg::Paste(IMAGE_PATH_PASTE.into()));
    assert!(!app.image_paste_rx.is_empty());
    assert_eq!(app.input_box.buffer.value(), "");
}

/// An empty paste is how a terminal hands over an image on the clipboard.
#[test_case(open_logs_modal, IMAGE_PATH_PASTE ; "path_into_logs")]
#[test_case(open_docs_modal, IMAGE_PATH_PASTE ; "path_into_docs")]
#[test_case(open_logs_modal, ""               ; "clipboard_into_logs")]
#[test_case(open_docs_modal, ""               ; "clipboard_into_docs")]
fn an_image_paste_into_a_modal_stays_out_of_the_composer(open: fn(&mut App), text: &str) {
    let mut app = test_app();
    open(&mut app);
    app.update(Msg::Paste(text.into()));
    assert!(app.image_paste_rx.is_empty(), "{IMAGE_UNDER_MODAL}");
    assert_eq!(app.input_box.buffer.value(), "", "{IMAGE_UNDER_MODAL}");
}

#[test]
fn submit_during_streaming_queues_message() {
    let mut app = test_app();
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(&actions[0], Action::SendMessage(_)));
    assert_eq!(app.status, Status::Streaming);

    app.update(Msg::Key(key(KeyCode::Char('b'))));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(actions.is_empty());
    assert_eq!(app.queue.len(), 1);
    assert_eq!(
        app.queue.pending_prompts()[0].admission,
        caudra_agent::PromptAdmission::Queue
    );
}

#[test]
fn the_steer_chord_steers_the_active_run() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    app.input_box.set_input("guide this run".into());

    let actions = press_chord(&mut app, chord::STEER_PROMPT);

    assert!(actions.is_empty());
    assert_eq!(
        app.queue.pending_prompts(),
        [shared_queue::PendingPrompt {
            mode: Some(StoredMode::Plan),
            text: "guide this run".into(),
            images: Vec::new(),
            paste_ranges: Vec::new(),
            admission: caudra_agent::PromptAdmission::Steer,
        }]
    );
    assert!(rendered(&mut app).contains("Guide"));
    assert!(!rendered(&mut app).contains("Mode:"));
}

#[test]
fn the_interrupt_chord_replaces_active_run_and_preserves_pending_queue() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    let (sender, receiver) = shared_queue::queue();
    app.queue.set_shared(sender);
    receiver.set_active_run(1);
    app.queue_and_notify(queued_msg("still needed"));
    app.input_box.set_input("replace now".into());

    let actions = press_chord(&mut app, chord::INTERRUPT_PROMPT);

    assert!(matches!(
        actions.as_slice(),
        [Action::CancelAgent { run_id: 1 }]
    ));
    assert_eq!(app.run_id, 2);
    assert_eq!(app.cancelling_run, Some(1));
    assert_eq!(
        app.queue
            .pending_prompts()
            .into_iter()
            .map(|prompt| prompt.admission)
            .collect::<Vec<_>>(),
        [
            caudra_agent::PromptAdmission::Queue,
            caudra_agent::PromptAdmission::Interrupt,
        ]
    );
    assert!(rendered(&mut app).contains("Replacing"));

    let replacement_id = app
        .queue
        .panel_entries()
        .into_iter()
        .find(|entry| entry.admission == Some(caudra_agent::PromptAdmission::Interrupt))
        .unwrap()
        .id;
    app.on_queue_item_consumed(replacement_id, "replace now", 0);

    assert_eq!(app.cancelling_run, None);
    assert_eq!(app.status, Status::Streaming);
}

#[test]
fn newest_pending_replacement_wins() {
    let mut app = test_app();
    let (sender, receiver) = shared_queue::queue();
    app.queue.set_shared(sender);
    app.status = Status::Streaming;
    app.run_id = 1;
    receiver.set_active_run(1);
    assert!(matches!(
        app.submit_prompt_with_admission(
            queued_msg("first replacement"),
            caudra_agent::PromptAdmission::Interrupt,
        ),
        SubmitOutcome::Replacing(_)
    ));

    let outcome = app.submit_prompt_with_admission(
        queued_msg("latest replacement"),
        caudra_agent::PromptAdmission::Interrupt,
    );

    assert!(matches!(outcome, SubmitOutcome::Replacing(ref actions) if actions.is_empty()));
    let replacements = app
        .queue
        .pending_prompts()
        .into_iter()
        .filter(|prompt| prompt.admission == caudra_agent::PromptAdmission::Interrupt)
        .collect::<Vec<_>>();
    assert_eq!(replacements.len(), 1);
    assert_eq!(replacements[0].text, "latest replacement");
    assert_eq!(app.cancelling_run, Some(1));
}

#[test]
fn replacement_claimed_before_ui_consumption_is_cancelled_by_newer_replacement() {
    let mut app = test_app();
    let (sender, receiver) = shared_queue::queue();
    app.queue.set_shared(sender);
    app.status = Status::Streaming;
    app.run_id = 1;
    receiver.set_active_run(1);
    assert!(matches!(
        app.submit_prompt_with_admission(
            queued_msg("first replacement"),
            caudra_agent::PromptAdmission::Interrupt,
        ),
        SubmitOutcome::Replacing(_)
    ));
    let claimed = receiver.claim_idle(0);
    assert_eq!(claimed.len(), 1);

    let outcome = app.submit_prompt_with_admission(
        queued_msg("newer replacement"),
        caudra_agent::PromptAdmission::Interrupt,
    );

    assert!(matches!(
        outcome,
        SubmitOutcome::Replacing(ref actions)
            if matches!(actions.as_slice(), [Action::CancelAgent { run_id: 2 }])
    ));
    assert_eq!(app.run_id, 3);
    assert_eq!(app.cancelling_run, Some(2));
    assert_eq!(
        app.queue.pending_prompts()[0].admission,
        caudra_agent::PromptAdmission::Interrupt
    );
}

#[test]
fn rejected_second_replacement_preserves_input() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.cancelling_run = Some(1);
    app.input_box.set_input("keep this replacement".into());

    let actions = press_chord(&mut app, chord::INTERRUPT_PROMPT);

    assert!(actions.is_empty());
    assert_eq!(app.input_box.buffer.value(), "keep this replacement");
    assert_eq!(app.status_bar.flash_text(), Some(queue::REPLACE_BUSY_ERR));
}

#[test]
fn replacement_without_an_active_run_starts_without_waiting_for_a_terminal_event() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    let SubmitOutcome::Replacing(actions) = app.submit_prompt_with_admission(
        queued_msg("replace queued startup work"),
        caudra_agent::PromptAdmission::Interrupt,
    ) else {
        panic!("expected replacement");
    };

    assert!(matches!(
        actions.as_slice(),
        [Action::CancelAgent { run_id: 1 }]
    ));
    assert_eq!(app.cancelling_run, None);
    assert_eq!(app.run_id, 2);
}

#[test]
fn queue_item_consumed_pushes_deferred_user_message() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    assert_eq!(app.main_chat().message_count(), 1);

    app.queue_and_notify(queued_msg("queued"));
    assert_eq!(
        app.main_chat().message_count(),
        1,
        "queueing while streaming must not render the bubble yet",
    );

    app.update(agent_msg_with_run_id(
        AgentEvent::QueueItemConsumed {
            id: caudra_agent::QueueItemId::new(),
            text: "queued".into(),
            image_count: 0,
        },
        app.run_id,
    ));

    assert_eq!(app.main_chat().message_count(), 2);
    assert_eq!(app.main_chat().last_message_text(), "queued");
    assert_eq!(
        app.main_chat().last_message_role(),
        Some(&DisplayRole::User),
    );
}

#[test]
fn deleting_replacement_still_resumes_a_late_superseded_error() {
    let mut app = test_app();
    let (sender, receiver) = shared_queue::queue();
    app.queue.set_shared(sender);
    app.status = Status::Streaming;
    app.run_id = 1;
    receiver.set_active_run(1);
    let outcome = app.submit_prompt_with_admission(
        queued_msg("replacement"),
        caudra_agent::PromptAdmission::Interrupt,
    );
    assert!(matches!(outcome, SubmitOutcome::Replacing(_)));
    let replacement = app
        .queue
        .panel_entries()
        .into_iter()
        .find(|entry| entry.admission == Some(caudra_agent::PromptAdmission::Interrupt))
        .unwrap();
    assert!(app.delete_active_queue_item(replacement.id));
    assert_eq!(app.cancelling_run, Some(1));
    assert!(matches!(
        app.submit_prompt_with_admission(
            queued_msg("another replacement"),
            caudra_agent::PromptAdmission::Interrupt,
        ),
        SubmitOutcome::Rejected(queue::REPLACE_BUSY_ERR)
    ));
    app.queue_and_notify(queued_msg("new work"));
    receiver.pause();

    app.update(agent_msg_with_run_id(
        AgentEvent::Error {
            message: "cancel race".into(),
        },
        1,
    ));

    assert_eq!(receiver.claim_idle(0).len(), 1);
}

/// Restored queue items start runs without `start_run`, so the consumed
/// event is the only signal that the agent went busy: it must flip status
/// or the busy-guard and esc-to-cancel stay off during the whole run.
#[test]
fn queue_item_consumed_marks_agent_streaming() {
    let mut app = test_app();
    assert_eq!(app.status, Status::Idle);

    app.update(agent_msg_with_run_id(
        AgentEvent::QueueItemConsumed {
            id: caudra_agent::QueueItemId::new(),
            text: "restored".into(),
            image_count: 0,
        },
        app.run_id,
    ));

    assert_eq!(app.status, Status::Streaming);
}

#[test_case(error_app as fn(&mut App) ; "error")]
#[test_case(cancel_app as fn(&mut App) ; "cancel")]
fn clears_queue(terminate: fn(&mut App)) {
    let mut app = app_with_queued_message();
    terminate(&mut app);
    assert!(app.queue.is_empty());
}

#[test_case("/compact" ; "slash_command")]
#[test_case("exit" ; "exit_keyword")]
#[test_case("!ls" ; "shell_prefix")]
fn submit_prompt_never_interprets_text(text: &str) {
    let mut app = test_app();
    match app.submit_prompt(queued_msg(text)) {
        SubmitOutcome::Started(actions) => {
            assert!(matches!(&actions[0], Action::SendMessage(_)))
        }
        _ => panic!("raw prompt must start the agent"),
    }
}

#[test]
fn submit_prompt_queues_while_streaming() {
    let mut app = test_app();
    app.status = Status::Streaming;
    assert!(matches!(
        app.submit_prompt(queued_msg("hi")),
        SubmitOutcome::Queued
    ));
    assert_eq!(app.queue.len(), 1);
}

#[test_case(test_app as fn() -> App, "   ", queue::EMPTY_PROMPT_ERR ; "blank_text")]
#[test_case(streaming_app_without_queue, "hi", queue::NO_QUEUE_ERR ; "streaming_without_shared_queue")]
fn submit_prompt_rejects(mk: fn() -> App, text: &str, expected: &str) {
    let mut app = mk();
    match app.submit_prompt(queued_msg(text)) {
        SubmitOutcome::Rejected(e) => assert_eq!(e, expected),
        _ => panic!("expected rejection"),
    }
}

#[test_case(PromptAdmission::Queue; "queue")]
#[test_case(PromptAdmission::Steer; "guide")]
#[test_case(PromptAdmission::Interrupt; "replacement")]
fn rejected_submission_preserves_rich_draft(admission: PromptAdmission) {
    let mut app = streaming_app_without_queue();
    app.input_box.buffer.insert_text(REJECTED_DRAFT_PREFIX);
    app.input_box.buffer.insert_paste(REJECTED_DRAFT_PASTE);
    let image = ImageSource::new(ImageMediaType::Png, Arc::from(REJECTED_DRAFT_IMAGE));
    app.input_box.attach_image(image.clone());
    let draft = app.input_box.draft();
    let display = app.input_box.buffer.display_text();
    let submission = app.input_box.take_submission().unwrap();

    assert!(
        app.handle_submit_with_admission(submission, admission)
            .is_empty()
    );

    assert_eq!(app.status_bar.flash_text(), Some(queue::NO_QUEUE_ERR));
    assert_eq!(app.input_box.draft(), draft);
    assert_eq!(app.input_box.buffer.display_text(), display);
    assert_eq!(app.input_box.pending_images(), [image]);
}

fn streaming_app_without_queue() -> App {
    let dir = test_state_dir();
    let mut app = build_app(dir.clone(), Arc::new(test_writer(dir)));
    app.status = Status::Streaming;
    app
}

fn attach_processing_queue(app: &mut App) -> QueueReceiver {
    let (sender, receiver) = shared_queue::queue();
    receiver.set_active_run(app.run_id);
    app.queue.set_shared(sender);
    receiver
}

fn queued_msg(text: &str) -> QueuedMessage {
    QueuedMessage {
        text: text.into(),
        images: vec![],
        mentions: Vec::new(),
        commits: Vec::new(),
        paste_ranges: Vec::new(),
    }
}

fn stored_queued_prompt(text: &str) -> StoredQueuedPrompt {
    StoredQueuedPrompt {
        mode: Some(StoredMode::Plan),
        text: text.into(),
        images: Vec::new(),
        paste_ranges: Vec::new(),
    }
}

fn app_with_queued_message() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue_and_notify(queued_msg("queued"));
    app
}

/// Arms the leader and plays the chord's second key, returning what the chord
/// itself produced. Arming never yields actions of its own.
pub(crate) fn press_chord(app: &mut App, chord: Bind) -> Vec<Action> {
    let armed = app.update(Msg::Key(kb::LEADER.to_key_event()));
    assert!(armed.is_empty(), "{ARMING_LEADER_IS_INERT_MSG}");
    app.update(Msg::Key(chord.to_key_event()))
}

fn type_and_submit(app: &mut App, text: &str) -> Vec<Action> {
    for c in text.chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Enter)))
}

#[test_case("/tasks"; "plural")]
#[test_case("/tasks list"; "plural_list")]
fn task_commands_open_local_picker_without_transcript(command: &str) {
    let mut app = app_with_subagent();
    let before = app.chats[0].message_count();
    let actions = type_and_submit(&mut app, command);
    assert!(actions.is_empty());
    assert!(app.task_picker.is_open());
    assert_eq!(app.chats[0].message_count(), before);
}

const ONE_MODAL: &str = "opening Tasks or Shell must close the other";

/// `/shells` needs no running command, and the two modals never stack. Shell
/// taking over from a previewing task picker returns to the chat the picker
/// was opened from, and an outside press leaves Shell without moving it.
#[test]
fn shell_and_task_modals_replace_each_other_over_the_same_chat() {
    let mut app = app_with_subagent();
    let before = app.chats[0].message_count();
    assert!(type_and_submit(&mut app, "/shells").is_empty());
    assert!(app.shell_modal.is_open());
    assert_eq!(app.chats[0].message_count(), before);
    let _ = rendered(&mut app);
    let (column, row) = OUTSIDE_MODAL;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    assert!(!app.shell_modal.is_open(), "{LEFT_STANDING}");
    assert_eq!(app.active_chat, 0);

    app.focus_task(TASK_ID).unwrap();
    app.tasks_browse();
    let _ = rendered(&mut app);
    app.update(Msg::Key(key(KeyCode::Up)));
    assert_eq!(app.active_chat, 0, "the preview should have moved to Main");
    app.shells_browse();
    assert!(app.shell_modal.is_open());
    assert!(!app.task_picker.is_open(), "{ONE_MODAL}");
    assert_eq!(app.active_chat, 1, "{ORIGIN_RESTORED}");

    app.tasks_browse();
    assert!(app.task_picker.is_open());
    assert!(!app.shell_modal.is_open(), "{ONE_MODAL}");
}

#[test_case("/tasks status"; "missing_id")]
#[test_case("/tasks list extra"; "extra_list_id")]
#[test_case("/tasks cancel id extra"; "extra_cancel_argument")]
#[test_case("/tasks invalid id"; "invalid_operation")]
#[test_case("/tasks status id extra more"; "many_extra_arguments")]
fn malformed_task_commands_never_send_or_steer(command: &str) {
    let mut app = app_with_subagent();
    app.active_chat = 1;
    let before = app.chats[1].message_count();
    let actions = type_and_submit(&mut app, command);
    assert!(actions.is_empty());
    assert_eq!(app.chats[1].message_count(), before);
    assert!(!app.task_picker.is_open());
}

#[test_case("/tasks"; "plural")]
fn task_status_selects_without_preview_then_enter_opens(command: &str) {
    let mut app = app_with_subagent();
    assert!(type_and_submit(&mut app, &format!("{command} status {TASK_ID}")).is_empty());
    assert!(app.task_picker.is_open());
    assert_eq!(app.active_chat, 0);
    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    assert_eq!(app.active_chat, 1);
    assert!(!app.task_picker.is_open());
}

#[test_case(false, "standalone"; "without_streamed_prediction")]
#[test_case(true, "standalone"; "adopt_streamed_prediction")]
#[test_case(false, "entry"; "batch_without_prediction")]
#[test_case(true, "entry"; "batch_adopts_prediction")]
#[test_case(false, "root"; "batch_root_parameters")]
#[test_case(false, "flat"; "batch_root_flat_parameters")]
fn task_admission_creates_navigable_chat_before_first_child_event(predicted: bool, shape: &str) {
    const ASSIGNED_TASK: &str = "neat-wanted-cowbird";
    const ASSIGNED_PROMPT: &str = "Wait for a permit, then inspect the source";
    let batch = shape != "standalone";
    let mut app = streaming_app();
    let call_id = if batch {
        format!("{TASK_ID}:0")
    } else {
        TASK_ID.to_owned()
    };
    let mut start = if batch {
        batch_roster(TASK_ID, 1)
    } else {
        tool_start(TASK_ID, "task")
    };
    if let AgentEvent::ToolStart(event) = &mut start {
        let input = Some(serde_json::json!({ "prompt": ASSIGNED_PROMPT }));
        if shape == "root" {
            event.raw_input = Some(
                serde_json::json!({ "tool_calls": [{ "tool": "task", "parameters": input }] }),
            );
        } else if shape == "flat" {
            event.raw_input = Some(
                serde_json::json!({ "tool_calls": [{ "tool": "task", "prompt": ASSIGNED_PROMPT }] }),
            );
        } else if let Some(ToolOutput::Batch { entries, .. }) = &mut event.output {
            entries[0].raw_input = input;
        } else {
            event.raw_input = input;
        }
    }
    app.update(agent_msg(start));
    if predicted {
        app.delegation_delta(caudra_agent::Delegation {
            parent_tool_use_id: call_id.clone(),
            name: Some(RESEARCH_NAME.into()),
            prompt: Some(ASSIGNED_PROMPT.into()),
            task_id: None,
        });
    }
    let card = serde_json::from_value(serde_json::json!({
        "task_id": ASSIGNED_TASK, "invocation_id": "private-invocation",
        "call_id": call_id, "root_call_id": TASK_ID, "label": RESEARCH_NAME,
        "state": "queued", "background": true, "mode": "build",
        "generation": 1, "created_at": 1, "updated_at": 1
    }))
    .unwrap();
    app.update(agent_msg(AgentEvent::TaskAdmitted(card)));
    app.discard_pending_delegations_under(TASK_ID);
    assert!(app.focus_task(ASSIGNED_TASK).is_ok());
    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.chats[1].name, RESEARCH_NAME);
    assert_eq!(app.chats[1].message_at(0).unwrap().text, ASSIGNED_PROMPT);
}

pub(crate) fn cancel_app(app: &mut App) {
    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
}

pub(crate) fn error_app(app: &mut App) {
    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));
}

fn cmd(name: &str) -> ParsedCommand {
    ParsedCommand {
        name: name.to_string(),
        args: String::new(),
    }
}

fn type_slash(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('/'))));
}

#[test]
fn typing_filters_palette() {
    let mut app = test_app();
    type_slash(&mut app);
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(key(KeyCode::Char('z'))));
    assert!(!app.command_palette.is_active());
}

#[test]
fn enter_executes_new_command() {
    let mut app = test_app();
    type_slash(&mut app);
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(&actions[0], Action::RequestNewSession));
    assert!(!app.command_palette.is_active());
}

#[test]
fn ctrl_c_closes_palette() {
    let mut app = test_app();
    type_slash(&mut app);
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(!app.command_palette.is_active());
}

/// The event exists so plugins can drop what belonged to the session that
/// ended. Naming its replacement makes every such handler a no-op.
#[test]
fn session_reset_names_the_session_that_ended() {
    let mut app = test_app();
    let (handle, probe) = caudra_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;
    let ended = app.state.session.id.to_string();

    app.reset_session();

    let (event, data) = probe.try_recv_autocmd().expect("SessionReset fired");
    assert_eq!(event, "SessionReset");
    assert_eq!(data["session_id"], serde_json::json!(ended));
    assert_ne!(
        app.state.session.id.to_string(),
        ended,
        "reset must have installed a different session, or this proves nothing"
    );
}

#[test]
fn reset_session_clears_plan() {
    let mut app = test_app();
    app.state.token_usage.input = 500;
    app.chats[0].context_size = 1000;
    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Ready(PathBuf::from("plan.md"));
    app.queue_and_notify(queued_msg("q"));
    app.queue.set_focus_at(0);
    app.help_modal.toggle();
    let (_tx, rx) = flume::bounded::<StreamEvent>(1);
    let (trigger, _cancel) = caudra_agent::CancelToken::new();
    app.stream_modal
        .open(" /btw ", "q".into(), StreamFooter::FollowUp, rx, trigger);
    let actions = app.reset_session();
    assert!(matches!(&actions[0], Action::NewSession(_)));
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.state.token_usage.input, 0);
    assert_eq!(app.chats[0].context_size, 0);
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan, PlanState::None);
    assert!(app.queue.is_empty());
    assert!(app.recoverable_queue.is_empty());
    assert_eq!(app.chats.len(), 1);
    assert_eq!(app.chats[0].name, "Main");
    assert_eq!(app.active_chat, 0);
    assert!(app.chat_index.is_empty());
    assert!(app.queue.focus().is_none());
    assert!(!app.help_modal.is_open());
    assert!(!app.stream_modal.is_open());
}

#[test]
fn reset_session_assigns_new_plan_path_in_plan_mode() {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("old-plan.md"));
    app.reset_session();
    assert_eq!(app.state.mode, Mode::Plan);
    assert!(app.state.plan.path().is_some());
    assert_ne!(app.state.plan.path(), Some(Path::new("old-plan.md")));
}

#[test]
fn reset_session_clears_drafting_plan_in_build_mode() {
    let mut app = test_app();
    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Drafting(PathBuf::from("leftover.md"));
    app.reset_session();
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan, PlanState::None);
}

#[test]
fn load_session_clears_plan() {
    let (_tmp, _dir, _writer, mut app) = tempdir_app();
    crate::push_history_message(app.state.session_mut(), Message::user("test".into()));
    app.state.session_mut().meta.mode = Some(StoredMode::Build);
    app.state.session_mut().save(&app.storage).unwrap();
    let id = app.state.session.id;
    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Ready(PathBuf::from("old-plan.md"));
    app.load_session(id);
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan.path(), None);
}

#[test]
fn tool_lifecycle_events_name_the_session_and_tool() {
    let mut app = streaming_app();
    let (handle, probe) = caudra_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;
    let session_id = app.state.session.id.to_string();

    app.update(agent_msg(tool_start("tool-1", "bash")));

    let (event, data) = probe.try_recv_autocmd().expect("ToolStart fired");
    assert_eq!(event, "ToolStart");
    assert_eq!(data["session_id"], serde_json::json!(session_id));
    assert_eq!(data["tool_id"], "tool-1");
    assert_eq!(data["tool"], "bash");

    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "tool-1".into(),
        tool: "bash".into(),
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
    }))));

    let (event, data) = probe.try_recv_autocmd().expect("ToolDone fired");
    assert_eq!(event, "ToolDone");
    assert_eq!(data["session_id"], serde_json::json!(session_id));
    assert_eq!(data["tool_id"], "tool-1");
    assert_eq!(data["tool"], "bash");

    app.run_id += 1;
    app.update(agent_msg_with_run_id(tool_start("stale", "read"), 1));
    assert!(probe.try_recv_autocmd().is_none());
}

#[test]
fn tab_in_palette_completes_command() {
    let mut app = test_app();
    type_slash(&mut app);
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(key(KeyCode::Tab)));
    let val = app.input_box.buffer.value();
    assert!(val.starts_with('/'));
}

#[test]
fn chat_navigation_actions() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "sub".into() },
        TASK_ID,
        Some("research"),
    ));
    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.active_chat, 0);

    app.run_builtin(BuiltinAction::NextChat);
    assert_eq!(app.active_chat, 1);

    app.run_builtin(BuiltinAction::NextChat);
    assert_eq!(app.active_chat, 1);

    app.run_builtin(BuiltinAction::PrevChat);
    assert_eq!(app.active_chat, 0);

    app.run_builtin(BuiltinAction::PrevChat);
    assert_eq!(app.active_chat, 0);
}

#[test]
fn subagents_get_descriptive_names() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "a".into() },
        TASK_ID,
        Some("first"),
    ));
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "b".into() },
        "task2",
        Some("second"),
    ));
    assert_eq!(app.chats.len(), 3);
    assert_eq!(app.chats[1].name, "first");
    assert_eq!(app.chats[2].name, "second");
}

#[test]
fn subagent_prompt_shown_once_and_not_duplicated() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg_with_prompt(
        AgentEvent::TextDelta { text: "a".into() },
        TASK_ID,
        Some("research"),
        Some("Find all TODO comments"),
    ));
    assert_eq!(app.chats[1].message_count(), 1);
    assert_eq!(app.chats[1].last_message_text(), "Find all TODO comments");

    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "b".into() },
        TASK_ID,
        Some("research"),
    ));
    app.chats[1].flush();
    assert_eq!(app.chats[1].message_count(), 2);
    assert_eq!(app.chats[1].last_message_text(), "ab");
}

const DELEGATE_ID: &str = "toolu_delegate";
const DELEGATE_NAME: &str = "Rename the workflow";
const DELEGATE_PROMPT: &str = "Rename it everywhere.";

fn delegation(
    parent_id: &str,
    name: Option<&str>,
    prompt: Option<&str>,
) -> caudra_agent::Delegation {
    caudra_agent::Delegation {
        parent_tool_use_id: parent_id.into(),
        name: name.map(String::from),
        prompt: prompt.map(String::from),
        task_id: None,
    }
}

fn delegation_msg(delegations: Vec<caudra_agent::Delegation>) -> Msg {
    agent_msg(AgentEvent::ToolInputDelta {
        id: DELEGATE_ID.into(),
        name: caudra_agent::tools::TASK_TOOL_NAME.into(),
        delta: String::new(),
        preview: None,
        size: None,
        body: None,
        roster: None,
        delegations,
        complete: false,
    })
}

fn tool_done_msg(id: &str) -> Msg {
    agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: id.into(),
        tool: caudra_agent::tools::TASK_TOOL_NAME.into(),
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
    })))
}

/// A streaming task opens its chat before anything runs, so the reader can
/// walk into it and watch the brief being written.
#[test]
fn a_streaming_delegation_opens_an_enterable_chat_before_its_call_runs() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(delegation_msg(vec![delegation(
        DELEGATE_ID,
        Some(DELEGATE_NAME),
        Some("Rename it "),
    )]));
    app.update(delegation_msg(vec![delegation(
        DELEGATE_ID,
        None,
        Some("everywhere."),
    )]));

    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.chats[1].name, DELEGATE_NAME);
    app.chats[1].flush();
    assert_eq!(app.chats[1].last_message_text(), DELEGATE_PROMPT);
    assert!(
        app.chat_index.is_empty(),
        "nothing routes a subagent's events to a chat with no subagent behind it"
    );
    app.focus_task(DELEGATE_ID).expect("the task is reachable");
    assert_eq!(app.active_chat, 1);
    assert_eq!(
        app.tasks()
            .into_iter()
            .map(|task| (task.id.to_string(), task.status))
            .collect::<Vec<_>>(),
        [
            (MAIN_TASK_ID.to_owned(), None),
            (DELEGATE_ID.to_owned(), Some(TaskStatus::Working)),
        ]
    );
}

/// The prediction becomes the real chat: one transcript, one copy of the
/// instruction, and the routing cache finally pointing at it.
#[test]
fn the_subagent_adopts_the_chat_its_brief_already_opened() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(delegation_msg(vec![delegation(
        DELEGATE_ID,
        Some(DELEGATE_NAME),
        Some(DELEGATE_PROMPT),
    )]));
    app.update(subagent_msg_with_prompt(
        AgentEvent::TextDelta { text: "a".into() },
        DELEGATE_ID,
        Some(DELEGATE_NAME),
        Some(DELEGATE_PROMPT),
    ));

    assert_eq!(
        app.chats.len(),
        2,
        "the prediction was adopted, not doubled"
    );
    assert_eq!(app.chats[1].message_count(), 1);
    assert_eq!(app.chats[1].last_message_text(), DELEGATE_PROMPT);
    assert_eq!(app.chat_index.get(DELEGATE_ID), Some(&1));
    assert!(app.pending_delegations.is_empty());
}

/// The reserved id differs from the predicted one when the call collided with
/// its own history. The chat follows the real id rather than being shadowed.
#[test]
fn a_subagent_reserved_under_another_id_still_adopts_the_chat() {
    const RESERVED: &str = "session-fresh";
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(delegation_msg(vec![delegation(
        DELEGATE_ID,
        Some(DELEGATE_NAME),
        Some(DELEGATE_PROMPT),
    )]));
    let mut info = subagent_info(DELEGATE_ID, DELEGATE_NAME);
    info.task_id = RESERVED.into();
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta { text: "a".into() },
        info,
    ));

    assert_eq!(app.chats.len(), 2);
    assert_eq!(
        app.chats[1].task_id().map(|id| id.to_string()).as_deref(),
        Some(RESERVED)
    );
    assert_eq!(app.chat_index.get(RESERVED), Some(&1));
}

/// A batch writes its children under ids that extend its own, and each brief
/// has to land in the chat that child will run in.
#[test]
fn every_batched_delegation_opens_its_own_chat() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(delegation_msg(vec![
        delegation("batch1:0", Some("first"), Some("do one")),
        delegation("batch1:1", Some("second"), Some("do two")),
    ]));

    assert_eq!(app.chats.len(), 3);
    for (idx, (name, prompt)) in [("first", "do one"), ("second", "do two")]
        .into_iter()
        .enumerate()
    {
        let chat = &mut app.chats[idx + 1];
        chat.flush();
        assert_eq!(chat.name, name);
        assert_eq!(chat.last_message_text(), prompt);
    }
}

#[test_case(
    |app: &mut App| { app.update(agent_msg(AgentEvent::StreamReset)); }
    ; "a_reset_stream"
)]
#[test_case(
    |app: &mut App| { app.update(tool_done_msg(DELEGATE_ID)); }
    ; "a_call_that_never_opened_a_subagent"
)]
#[test_case(end_turn ; "the_end_of_the_turn")]
fn an_unclaimed_delegation_is_dropped_by(abandon: fn(&mut App)) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(delegation_msg(vec![delegation(
        DELEGATE_ID,
        Some(DELEGATE_NAME),
        Some(DELEGATE_PROMPT),
    )]));
    app.active_chat = 1;

    abandon(&mut app);

    assert_eq!(app.chats.len(), 1, "the prediction left nothing behind");
    assert_eq!(app.active_chat, 0, "focus followed the chat that went");
    assert!(app.pending_delegations.is_empty());
}

/// Removing a chat shifts every index behind it, and `chat_index` is a map of
/// indices, so a real subagent must not be left pointing at the wrong one.
#[test]
fn dropping_a_prediction_keeps_the_surviving_chats_addressable() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(delegation_msg(vec![delegation(
        DELEGATE_ID,
        Some(DELEGATE_NAME),
        Some(DELEGATE_PROMPT),
    )]));
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "a".into() },
        TASK_ID,
        Some("real"),
    ));
    assert_eq!(app.chat_index.get(TASK_ID), Some(&2));

    app.update(tool_done_msg(DELEGATE_ID));

    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.chat_index.get(TASK_ID), Some(&1));
    assert_eq!(app.chats[1].name, "real");
}

/// A prediction is not session state: a snapshot taken while one is on screen
/// must not record a subagent that may never exist.
#[test]
fn a_checkpoint_taken_over_a_prediction_records_no_subagent() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(delegation_msg(vec![delegation(
        DELEGATE_ID,
        Some(DELEGATE_NAME),
        Some(DELEGATE_PROMPT),
    )]));
    app.checkpoint();
    assert!(app.state.session.subagents().is_empty());
}

/// A continuation names a chat that already holds a transcript. Its
/// instruction lands there whole when the call runs, so the prediction opened
/// beside it goes rather than showing the brief twice.
#[test]
fn a_continuation_drops_the_prediction_and_leaves_its_chat_alone() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "a".into() },
        TASK_ID,
        Some("earlier"),
    ));
    app.update(delegation_msg(vec![delegation(
        DELEGATE_ID,
        Some(DELEGATE_NAME),
        Some("more work"),
    )]));
    assert_eq!(app.chats.len(), 3);

    app.update(delegation_msg(vec![caudra_agent::Delegation {
        task_id: Some(TASK_ID.into()),
        ..delegation(DELEGATE_ID, None, None)
    }]));

    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.chats[1].name, "earlier");
    assert!(app.pending_delegations.is_empty());
}

#[test]
fn turn_complete_tracks_usage_and_context_per_chat() {
    let mut app = app_with_subagent();

    let main_usage = TokenUsage {
        input: 100,
        output: 50,
        ..Default::default()
    };
    app.update(agent_msg(turn_complete(main_usage, "test", None)));

    let sub_usage = TokenUsage {
        input: 200,
        output: 75,
        ..Default::default()
    };
    app.update(subagent_msg(
        turn_complete(sub_usage, "test", None),
        TASK_ID,
        None,
    ));

    assert_eq!(app.state.token_usage.input, 300);
    assert_eq!(app.state.token_usage.output, 125);
    assert_eq!(app.chats[0].context_size, main_usage.context_tokens());
    assert_eq!(app.chats[1].context_size, sub_usage.context_tokens());
}

#[test]
fn active_context_snapshot_uses_current_main_then_unfiltered_active_task() {
    let mut app = app_with_subagent();
    let main_spec = app.state.model.spec();
    let store = ContextStore::new();
    let main = store.publisher(ContextKey::Main);
    main.publish(current_context_snapshot(&app));
    main.for_task(TASK_ID)
        .publish(context_snapshot(TASK_CONTEXT_SPEC, CHAT_CONTEXT_WINDOW));
    app.context_store = Some(store);

    let snapshot = app.active_context_snapshot().unwrap();
    assert_eq!(snapshot.model.spec, main_spec);

    app.run_builtin(BuiltinAction::NextChat);
    let snapshot = app.active_context_snapshot().unwrap();
    assert_eq!(snapshot.model.spec, TASK_CONTEXT_SPEC);
    assert_eq!(snapshot.window.tokens, CHAT_CONTEXT_WINDOW);
}

#[test]
fn active_context_snapshot_rejects_stale_main_model() {
    let mut app = test_app();
    let mut snapshot = current_context_snapshot(&app);
    snapshot.model.spec = MAIN_CONTEXT_SPEC.into();
    let store = ContextStore::new();
    store.publisher(ContextKey::Main).publish(snapshot);
    app.context_store = Some(store);

    assert!(app.active_context_snapshot().is_none());
}

#[test]
fn active_context_snapshot_rejects_stale_main_window() {
    let mut app = test_app();
    let mut snapshot = current_context_snapshot(&app);
    snapshot.window.tokens = CHAT_CONTEXT_WINDOW;
    let store = ContextStore::new();
    store.publisher(ContextKey::Main).publish(snapshot);
    app.context_store = Some(store);

    assert!(app.active_context_snapshot().is_none());
}

#[test]
fn active_context_snapshot_accepts_the_running_plan_model() {
    let mut app = test_app();
    let mut model = test_model();
    model.id = PLAN_CONTEXT_MODEL_ID.into();
    model.context_window = CHAT_CONTEXT_WINDOW;
    let provider = caudra_providers::provider::from_model_fallback(
        &mut model,
        caudra_providers::Timeouts::default(),
    );
    app.effective_model_slot = Some(Arc::new(ArcSwap::from_pointee(crate::agent::ModelSlot {
        model,
        provider: Arc::from(provider),
    })));
    app.status = Status::Streaming;
    let store = ContextStore::new();
    let mut snapshot = context_snapshot(PLAN_CONTEXT_SPEC, CHAT_CONTEXT_WINDOW);
    snapshot.mode = app.execution_agent_mode();
    store.publisher(ContextKey::Main).publish(snapshot);
    app.context_store = Some(store);

    let snapshot = app.active_context_snapshot().unwrap();
    assert_ne!(app.state.model.spec(), PLAN_CONTEXT_SPEC);
    assert_eq!(snapshot.model.spec, PLAN_CONTEXT_SPEC);
    assert_eq!(snapshot.window.tokens, CHAT_CONTEXT_WINDOW);
    let screen = rendered_wide(&mut app, 160);
    assert!(
        screen.contains(PLAN_CONTEXT_SPEC),
        "{PLAN_STATUS_MODEL_MISSING}"
    );
    assert!(
        screen.contains(PLAN_CONTEXT_WINDOW_LABEL),
        "{PLAN_STATUS_WINDOW_MISSING}"
    );
}

#[test]
fn idle_plan_status_uses_the_last_validated_effective_snapshot() {
    let mut app = test_app();
    let previous = model_registry::binding(ModelPurpose::Plan);
    model_registry::set_binding_and_persist(
        ModelPurpose::Plan,
        Binding::Exact(PLAN_CONTEXT_SPEC.into()),
        &app.storage,
    )
    .unwrap();
    let mut model = test_model();
    model.id = PLAN_CONTEXT_MODEL_ID.into();
    model.context_window = CHAT_CONTEXT_WINDOW;
    let provider = caudra_providers::provider::from_model_fallback(
        &mut model,
        caudra_providers::Timeouts::default(),
    );
    app.effective_model_slot = Some(Arc::new(ArcSwap::from_pointee(crate::agent::ModelSlot {
        model,
        provider: Arc::from(provider),
    })));
    app.state.mode = Mode::Plan;
    app.state.applied_mode = Mode::Plan;
    let store = ContextStore::new();
    let mut snapshot = context_snapshot(PLAN_CONTEXT_SPEC, CHAT_CONTEXT_WINDOW);
    snapshot.mode = app.execution_agent_mode();
    store.publisher(ContextKey::Main).publish(snapshot);
    app.context_store = Some(store);

    let snapshot = app.active_context_snapshot();
    let screen = rendered_wide(&mut app, 160);
    match previous {
        Some(binding) => {
            model_registry::set_binding_and_persist(ModelPurpose::Plan, binding, &app.storage)
        }
        None => model_registry::clear_binding_and_persist(ModelPurpose::Plan, &app.storage),
    }
    .unwrap();

    assert_eq!(
        snapshot
            .as_ref()
            .map(|snapshot| snapshot.model.spec.as_str()),
        Some(PLAN_CONTEXT_SPEC)
    );
    assert!(
        screen.contains(PLAN_CONTEXT_SPEC),
        "{PLAN_STATUS_MODEL_MISSING}"
    );
    assert!(
        screen.contains(PLAN_CONTEXT_WINDOW_LABEL),
        "{PLAN_STATUS_WINDOW_MISSING}"
    );
}

#[test]
fn compaction_turn_does_not_replace_the_chat_context_window() {
    let mut app = streaming_app();
    app.update(agent_msg(turn_complete_from(
        TEST_PROVIDER,
        TokenUsage::default(),
        MAIN_MODEL,
        None,
        LedgerPurpose::Chat,
        CHAT_CONTEXT_WINDOW,
    )));
    assert_eq!(app.chats[0].context_window, CHAT_CONTEXT_WINDOW);

    app.update(agent_msg(turn_complete_from(
        OTHER_PROVIDER,
        TokenUsage::default(),
        LEDGER_MODEL,
        None,
        LedgerPurpose::Compaction,
        COMPACTION_CONTEXT_WINDOW,
    )));

    assert_eq!(app.chats[0].context_window, CHAT_CONTEXT_WINDOW);
}

const SUBAGENT_NAME: &str = "research";
const SUB_TOKENS: TokenUsage = TokenUsage {
    input: 1_000,
    output: 200,
    cache_creation: 300,
    cache_read: 400,
};
const SUB_COST: Option<f64> = Some(0.007);
const MAIN_TOKENS: TokenUsage = TokenUsage {
    input: 500,
    output: 100,
    cache_creation: 0,
    cache_read: 0,
};
const MAIN_COST: Option<f64> = Some(0.002);
const MAIN_MODEL: &str = "main-model";

fn main_turn() -> Msg {
    agent_msg(turn_complete(MAIN_TOKENS, MAIN_MODEL, MAIN_COST))
}

fn sub_turn_complete() -> Msg {
    subagent_msg(
        turn_complete(SUB_TOKENS, "child-model", SUB_COST),
        TASK_ID,
        Some(SUBAGENT_NAME),
    )
}

/// Built with the header's own formatter: these tests pin which tool gets the
/// usage, not how it is spelled (caudra-providers covers the spelling).
fn sub_usage_text() -> String {
    SUB_TOKENS.format_sum_cost(SUB_COST)
}

/// Each turn bills at the rates of the model that ran it, subagent tiers
/// included, so the session total is the sum of what the turns recorded.
#[test]
fn session_cost_sums_what_each_model_recorded() {
    let mut app = streaming_app();
    app.update(main_turn());
    app.update(agent_msg(tool_start(TASK_ID, "task")));
    app.update(sub_turn_complete());

    let expected = MAIN_COST.unwrap() + SUB_COST.unwrap();
    assert_eq!(app.state.cost, Some(expected));
    let stored: f64 = app
        .state
        .session
        .usage_by_model()
        .values()
        .filter_map(|u| u.cost)
        .sum();
    assert_eq!(stored, expected);
}

const RESTORED_COST: f64 = 0.42;
/// Counters big enough that re-pricing them could never land on
/// [`RESTORED_COST`], so a total derived from them stands out.
const RESTORED_TOKENS: TokenUsage = TokenUsage {
    input: 1_000_000,
    output: 0,
    cache_creation: 0,
    cache_read: 0,
};
const RESTORED_MODEL: &str = "model-that-ran-before";
const SIGMA_MISSING: &str = "the status bar must draw the session total";
const SIGMA_BAR_WIDTH: u16 = 140;
const THINKING_OFF: &str = "off";
const THINKING_MINIMAL: &str = "minimal";
const THINKING_TITLE: &str = crate::components::thinking_picker::TITLE;
const THINKING_PICKER_SHUT: &str = "the thinking chip must open the picker";
const THINKING_MOVED: &str = "opening the picker must not change the setting on its own";
const FLASH_NOT_DRAWN: &str = "the cycle must flash the picker as feedback";
const FLASH_STOLE_KEYS: &str = "a flashed preview must not answer to input";
const COST_WAS_NOT_BILLED: &str = "the turn must bill something for the reset to prove anything";

/// A new session opens on a clean bill. The total is never re-derived from the
/// counters, so anything left behind here follows the user forever.
#[test]
fn reset_session_clears_the_bill_and_the_model_breakdown() {
    let mut app = streaming_app();
    app.update(main_turn());
    assert_eq!(app.state.cost, MAIN_COST, "{COST_WAS_NOT_BILLED}");

    app.reset_session();

    assert_eq!(app.state.cost, None);
    assert!(app.state.session.usage_by_model().is_empty());
}

/// `None` is what hides the cost, so an unpriced turn must leave the total
/// alone. `Some(0.0)` would advertise a free session.
#[test_case(None, None ; "unpriced_turns_only")]
#[test_case(MAIN_COST, MAIN_COST ; "priced_turn_after_an_unpriced_one")]
fn session_cost_counts_only_priced_turns(second: Option<f64>, expected: Option<f64>) {
    let mut app = streaming_app();
    // How an unpriced session opens; `session_state` covers the seeding.
    app.state.cost = None;
    app.update(agent_msg(turn_complete(MAIN_TOKENS, MAIN_MODEL, None)));

    app.update(agent_msg(turn_complete(MAIN_TOKENS, MAIN_MODEL, second)));

    assert_eq!(app.state.cost, expected);
}

/// The restored bill is a running total later turns add to, so a resumed
/// session shows what it paid back then plus what it pays now, never its
/// counters re-priced at today's rates.
#[test]
fn resumed_session_keeps_adding_to_the_restored_bill() {
    let mut app = test_app();
    let mut stored = AppSession::new("test-model", "/tmp");
    stored.token_usage = RESTORED_TOKENS;
    stored.add_model_usage(
        RESTORED_MODEL,
        RESTORED_TOKENS.billed(Some(RESTORED_COST), Billing::Api),
    );

    app.apply_loaded_session(stored, &test_model()).unwrap();
    assert_eq!(app.state.cost, Some(RESTORED_COST));
    assert_eq!(app.chats[0].cost, Some(RESTORED_COST));

    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(main_turn());

    assert_eq!(app.state.cost, Some(RESTORED_COST + MAIN_COST.unwrap()));
}

/// The sigma the status bar draws once subagents split the bill is the session
/// total itself, so it cannot drift from what `/usage` sums. The session total
/// is the first thing a narrow bar sheds, so this asks for a bar wide enough to
/// still be carrying it.
#[test]
fn status_bar_sigma_draws_the_session_cost() {
    let mut app = app_with_subagent();
    app.update(main_turn());
    app.update(sub_turn_complete());

    let total = app.state.cost.expect("both turns were priced");
    let sigma = format!("\u{03a3}${total:.3}");
    assert!(
        rendered_wide(&mut app, SIGMA_BAR_WIDTH).contains(&sigma),
        "{SIGMA_MISSING}: {sigma}"
    );
}

const SUBAGENT_ELAPSED: Duration = Duration::from_millis(63_400);

fn progress_event(activity: SubagentActivity, tools: u32) -> AgentEvent {
    AgentEvent::SubagentProgress {
        progress: SubagentProgress {
            activity,
            tools,
            elapsed: SUBAGENT_ELAPSED,
        },
    }
}

fn parent_progress(app: &App, index: usize) -> Option<(&str, Option<&str>, u32)> {
    let report = &app.chats[0].message_at(index)?.progress.as_ref()?.report;
    Some((
        report.activity.label(),
        report.activity.detail(),
        report.tools,
    ))
}

/// The subagent's own work goes to its transcript; only the digest of it
/// belongs on the parent header, and only on the header it came from.
#[test]
fn subagent_progress_lands_on_its_own_parent_header() {
    let mut app = streaming_app();
    app.update(agent_msg(tool_start(TASK_ID, "task")));
    app.update(agent_msg(tool_start("task2", "task")));

    app.update(subagent_msg(
        progress_event(
            SubagentActivity::tool(Arc::from("shell"), "cargo nextest run"),
            3,
        ),
        TASK_ID,
        Some(SUBAGENT_NAME),
    ));

    assert_eq!(
        parent_progress(&app, 0),
        Some(("shell", Some("cargo nextest run"), 3))
    );
    assert_eq!(parent_progress(&app, 1), None);
}

/// The tally has to survive the call it describes, or the transcript loses
/// the only record of how much the subagent actually did.
#[test]
fn a_finished_subagent_keeps_the_tally_it_ended_on() {
    let mut app = streaming_app();
    app.update(agent_msg(tool_start(TASK_ID, "task")));
    app.update(subagent_msg(
        progress_event(SubagentActivity::Responding, 7),
        TASK_ID,
        Some(SUBAGENT_NAME),
    ));

    finish_subagent(&mut app, TASK_ID, false);

    let progress = app.chats[0].message_at(0).unwrap().progress.as_ref();
    assert_eq!(progress.map(|p| p.report.tools), Some(7));
    assert_eq!(progress.map(ToolProgress::is_live), Some(false));
    assert!(
        progress.is_some_and(|p| p.elapsed() >= SUBAGENT_ELAPSED),
        "a settled report keeps the clock it stopped at"
    );
}

/// An id that names neither a header nor a roster still has to fall through
/// quietly rather than stamp the main chat.
#[test]
fn a_progress_report_for_no_known_call_is_dropped() {
    let mut app = streaming_app();
    app.update(agent_msg(tool_start(TASK_ID, "batch")));

    app.update(subagent_msg(
        progress_event(SubagentActivity::Thinking { title: None }, 0),
        "session-generated",
        Some(SUBAGENT_NAME),
    ));

    assert_eq!(parent_progress(&app, 0), None);
}

const DISPATCHED_TOOL: &str = "file_grep";
/// The verb the row shows for `DISPATCHED_TOOL`, which names itself nowhere.
const DISPATCHED_LABEL: &str = "Grepping";

fn batch_roster(id: &str, children: usize) -> AgentEvent {
    let AgentEvent::ToolStart(mut start) = tool_start(id, "batch") else {
        unreachable!("tool_start builds a ToolStart")
    };
    start.output = Some(ToolOutput::Batch {
        entries: (0..children)
            .map(|_| caudra_agent::BatchToolEntry {
                model_suffix: None,
                tool: "task".into(),
                effect: ToolEffect::Orchestrator,
                summary: "research".into(),
                status: caudra_agent::BatchToolStatus::Running,
                input: None,
                raw_input: None,
                output: None,
                annotation: None,
            })
            .collect(),
        text: String::new(),
    });
    AgentEvent::ToolStart(start)
}

/// The reported regression. A dispatched child's id is the batch's own with an
/// index appended, so the report named a header that does not exist and was
/// dropped, leaving three subagents on the roster saying only that they had
/// been dispatched. It belongs to the row the batch drew for that child.
#[test]
fn a_dispatched_subagents_progress_lands_on_its_row() {
    let mut app = streaming_app();
    app.update(agent_msg(batch_roster(TASK_ID, 2)));

    app.update(subagent_msg(
        progress_event(
            SubagentActivity::tool(Arc::from(DISPATCHED_TOOL), "in src"),
            4,
        ),
        &format!("{TASK_ID}:1"),
        Some(SUBAGENT_NAME),
    ));

    // Never the batch's own header: the card reports that it ran a roster,
    // not what one member of it is doing.
    assert_eq!(parent_progress(&app, 0), None);
    let rendered = rendered(&mut app);
    assert!(
        rendered.contains(DISPATCHED_LABEL),
        "a dispatched child says what it is doing: {rendered}"
    );
}

const ARM_ROUTE_MSG: &str =
    "a press routed through the real mouse path arms the window it landed in";
const ARM_WHEEL_MSG: &str = "an armed window takes the wheel that follows the press";
const ARM_SWEEP_MSG: &str = "arming leaves the press free to start a selection sweep";
const SCROLL_CARD_BODY_LINES: usize = 40;
const SCROLL_CARD_TOOL: &str = "shell";
const FOLLOWING_FOOTER: &str = "following";
const PAUSED_FOOTER: &str = "below";
/// Positive is towards the start of the body, as everywhere else.
const CARD_WHEEL_NOTCHES: i32 = 3;

/// An app holding one open shell card whose body is long enough to be a
/// window. The command is still running, because only a running one reports
/// which edge its window is pinned to: a settled card has no tail left to
/// follow and its footer reports the counts alone.
fn app_with_scroll_card() -> App {
    let mut app = app_without_splash();
    app.update(agent_msg(tool_start(SCROLL_CARD_ID, SCROLL_CARD_TOOL)));
    app.update(agent_msg(AgentEvent::ToolOutput {
        id: SCROLL_CARD_ID.into(),
        content: (0..SCROLL_CARD_BODY_LINES)
            .map(|line| format!("line {line}\n"))
            .collect::<String>(),
    }));
    app
}

const SCROLL_CARD_ID: &str = "scroll-card";

/// The first cell inside the card's drawn window.
fn card_window_cell(app: &mut App) -> (u16, u16) {
    let area = app.msg_area();
    (area.y..area.bottom())
        .flat_map(|row| (area.x..area.right()).map(move |column| (column, row)))
        .find(|&(column, row)| app.chats[0].card_window_key_at(column, row).is_some())
        .expect("the card drew a window")
}

/// The reported defect: pressing inside a card's window did not arm it, so the
/// wheel still went to the transcript. Every panel-level arming test calls the
/// panel directly and so cannot see the routing, which is where it broke.
///
/// Touch is the case that was actually broken and is covered here too: a tap
/// reports no held drag, so `selection_state` never becomes `Dragging` and the
/// whole release-side click dispatch is skipped. Arming therefore has to
/// happen on the press.
#[test_case(false ; "mouse")]
#[test_case(true ; "touch")]
fn a_press_in_a_card_window_arms_it_for_the_wheel(touch: bool) {
    caudra_workbench::scroll::set_touch(touch);
    let mut app = app_with_scroll_card();
    let before = rendered(&mut app);
    assert!(
        before.contains(FOLLOWING_FOOTER),
        "{ARM_ROUTE_MSG}: {before:?}"
    );
    let (column, row) = card_window_cell(&mut app);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        column,
        row,
    ));
    assert!(app.chats[0].armed_card_key().is_some(), "{ARM_ROUTE_MSG}");

    app.update(Msg::Scroll {
        column,
        row,
        delta: CARD_WHEEL_NOTCHES,
    });
    let after = rendered(&mut app);
    assert!(after.contains(PAUSED_FOOTER), "{ARM_WHEEL_MSG}: {after:?}");
    caudra_workbench::scroll::set_touch(false);
}

/// Arming must not consume the press, or a card body's own text could no
/// longer be swept and copied, which is the other thing that body is for.
#[test]
fn arming_a_window_still_lets_a_sweep_start_in_it() {
    let mut app = app_with_scroll_card();
    let _ = rendered(&mut app);
    let (column, row) = card_window_cell(&mut app);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));

    assert!(app.selection_state.is_some(), "{ARM_SWEEP_MSG}");
}

/// Renders, then clicks the roster row `child_id` was drawn on. The row has to
/// be found after the frame it is measured in, since the roster only takes its
/// shape once the card is laid out.
fn click_roster_row(app: &mut App, child_id: &str) {
    let _ = rendered(app);
    let area = app.msg_area();
    let row = (area.y..area.bottom())
        .find(|&row| app.chats[0].dispatched_id_at(row, area).as_deref() == Some(child_id))
        .expect("the roster drew a row for the child");
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x + 2,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        area.x + 2,
        row,
    ));
}

/// A batch child's row sits inside the batch's own card, so the card answers
/// for it and every row on the roster opened the same transcript. Docs promise
/// that clicking a task call opens the subagent it dispatched.
#[test]
fn clicking_a_dispatched_child_opens_that_childs_transcript() {
    let mut app = streaming_app();
    let child_id = format!("{TASK_ID}:1");
    app.update(agent_msg(batch_roster(TASK_ID, 2)));
    app.update(subagent_msg(
        progress_event(SubagentActivity::Responding, 1),
        &child_id,
        Some(SUBAGENT_NAME),
    ));

    click_roster_row(&mut app, &child_id);

    assert_eq!(
        app.chats[app.active_chat].task_id().map(|id| &**id),
        Some(&*child_id),
        "the click opened the subagent the child dispatched"
    );
}

/// Only the children that dispatched a subagent have a transcript to open.
/// The rest keep the fold they always had, even while a sibling on the same
/// roster is holding a chat of its own.
#[test]
fn clicking_a_child_that_dispatched_nothing_still_folds_it() {
    let mut app = streaming_app();
    app.update(agent_msg(batch_roster(TASK_ID, 2)));
    app.update(subagent_msg(
        progress_event(SubagentActivity::Responding, 1),
        &format!("{TASK_ID}:1"),
        Some(SUBAGENT_NAME),
    ));

    click_roster_row(&mut app, &format!("{TASK_ID}:0"));

    assert_eq!(app.active_chat, 0, "an undispatched child opens no chat");
}

/// A batch child's brief streams into a chat before the child runs, and that
/// row is the only way the pointer reaches it. Nothing has dispatched yet, so
/// the row has to answer for itself rather than through `parent_task_ids`.
#[test]
fn clicking_a_delegating_child_opens_the_chat_it_is_still_being_given() {
    let mut app = streaming_app();
    let child_id = format!("{TASK_ID}:1");
    app.update(delegation_msg(vec![delegation(
        &child_id,
        Some(DELEGATE_NAME),
        Some(DELEGATE_PROMPT),
    )]));
    app.update(agent_msg(batch_roster(TASK_ID, 2)));

    click_roster_row(&mut app, &child_id);

    assert_eq!(
        app.chats[app.active_chat].task_id().map(|id| &**id),
        Some(&*child_id),
        "the click opened the chat the brief is being written into"
    );
    app.chats[app.active_chat].flush();
    assert_eq!(
        app.chats[app.active_chat].last_message_text(),
        DELEGATE_PROMPT,
        "the brief was already on screen when the chat opened"
    );
}

/// The child dispatching for real adopts the chat its brief opened, so the row
/// keeps pointing at the same transcript instead of gaining a second one.
#[test]
fn a_predicted_child_row_survives_adoption_as_the_same_target() {
    let mut app = streaming_app();
    let child_id = format!("{TASK_ID}:1");
    app.update(delegation_msg(vec![delegation(
        &child_id,
        Some(DELEGATE_NAME),
        Some(DELEGATE_PROMPT),
    )]));
    app.update(agent_msg(batch_roster(TASK_ID, 2)));
    click_roster_row(&mut app, &child_id);
    let predicted = app.active_chat;
    app.active_chat = 0;

    app.update(subagent_msg(
        progress_event(SubagentActivity::Responding, 1),
        &child_id,
        Some(SUBAGENT_NAME),
    ));
    click_roster_row(&mut app, &child_id);

    assert_eq!(
        app.active_chat, predicted,
        "the dispatched child reused the chat its brief opened"
    );
    assert_eq!(app.chats.len(), 2, "adoption left no second chat behind");
}

#[test]
fn subagent_turn_complete_updates_matching_parent_header_with_last_turn() {
    let mut app = streaming_app();
    app.update(agent_msg(tool_start(TASK_ID, "task")));
    app.update(agent_msg(tool_start("task2", "task")));

    app.update(sub_turn_complete());
    // The second turn's tokens differ, so a sum or a stale first turn would fail.
    let last = TokenUsage {
        input: 42,
        ..SUB_TOKENS
    };
    app.update(subagent_msg(
        turn_complete(last, "child-model", SUB_COST),
        TASK_ID,
        Some(SUBAGENT_NAME),
    ));

    let expected = last.format_sum_cost(SUB_COST.map(|cost| cost * 2.0));
    assert_eq!(
        app.chats[0].tool_turn_usage(TASK_ID),
        Some(expected.as_str())
    );
    assert_eq!(app.chats[0].tool_turn_usage("task2"), None);
}

#[test_case(false ; "plain_tool_takes_the_parent_turn")]
#[test_case(true  ; "subagent_stamp_is_not_overwritten")]
fn parent_turn_flush_stamps_the_last_unstamped_tool(subagent_ran: bool) {
    let mut app = streaming_app();
    app.update(main_turn());
    app.update(agent_msg(tool_start(TASK_ID, "task")));
    if subagent_ran {
        app.update(sub_turn_complete());
    }

    app.update(agent_msg(tool_results_submitted()));

    let expected = if subagent_ran {
        sub_usage_text()
    } else {
        MAIN_TOKENS.format(MAIN_COST)
    };
    assert_eq!(
        app.chats[0].tool_turn_usage(TASK_ID),
        Some(expected.as_str())
    );
}

#[test]
fn tool_inside_subagent_chat_gets_its_turn_usage() {
    const TOOL_ID: &str = "sub_bash";
    let mut app = streaming_app();
    app.update(agent_msg(tool_start(TASK_ID, "task")));
    app.update(subagent_msg(
        tool_start(TOOL_ID, "bash"),
        TASK_ID,
        Some(SUBAGENT_NAME),
    ));
    app.update(sub_turn_complete());

    app.update(subagent_msg(
        tool_results_submitted(),
        TASK_ID,
        Some(SUBAGENT_NAME),
    ));

    assert_eq!(
        app.chats[1].tool_turn_usage(TOOL_ID),
        Some(SUB_TOKENS.format(SUB_COST).as_str())
    );
}

#[test]
fn turn_complete_accumulates_usage_by_model() {
    let mut app = app_with_subagent();

    app.update(agent_msg(turn_complete(
        TokenUsage {
            input: 100,
            output: 50,
            cache_read: 10,
            ..Default::default()
        },
        "main-model",
        None,
    )));
    app.update(subagent_msg(
        turn_complete(
            TokenUsage {
                input: 200,
                output: 75,
                ..Default::default()
            },
            "sub-model",
            None,
        ),
        TASK_ID,
        None,
    ));

    let by_model = app.state.session.usage_by_model();
    assert_eq!(by_model.len(), 2);
    let main = &by_model[&format!("{TEST_PROVIDER}/main-model")];
    assert_eq!(main.input, 100);
    assert_eq!(main.output, 50);
    assert_eq!(main.cache_read, 10);
    let sub = &by_model[&format!("{TEST_PROVIDER}/sub-model")];
    assert_eq!(sub.input, 200);
    assert_eq!(sub.output, 75);
}

#[test]
fn cancel_resets_all_chats_and_indices() {
    let mut app = app_with_subagent();
    let _receiver = attach_processing_queue(&mut app);
    app.update(subagent_msg(
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: "sub_t1".into(),
            effect: ToolEffect::Unknown,
            tool: "bash".into(),
            summary: "running".into(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        })),
        TASK_ID,
        None,
    ));
    let buf = Arc::new(caudra_agent::SharedBuf::new());
    app.update(subagent_msg(
        AgentEvent::LiveToolBuf {
            id: "sub_t1".into(),
            body: buf,
        },
        "task1",
        None,
    ));

    let actions = app.handle_cancel();
    assert!(matches!(actions.as_slice(), [Action::CancelAgent { .. }]));
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(app.chats[1].is_finished());
    assert!(app.chat_index.is_empty());
    assert_eq!(app.status, Status::Streaming);
    assert_eq!(app.cancelling_run, Some(1));
    assert!(app.cadence().moves());
}

/// What a subagent's own session sends when it closes, which the `task` tool
/// does before it reports success or failure.
pub(crate) fn close_subagent_transcript(app: &mut App, id: &str) {
    app.update(agent_msg(AgentEvent::SubagentHistory {
        task_id: id.into(),
        parent_tool_use_id: id.into(),
        root_tool_use_id: id.into(),
        name: "task".into(),
        model: SONNET_SPEC.into(),
        messages: vec![],
        spec: None,
    }));
}

pub(crate) fn finish_subagent(app: &mut App, id: &str, is_error: bool) {
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: id.into(),
        tool: "task".into(),
        output: ToolOutput::Plain("result".into()),
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
    }))));
}

fn finish_subagent_task(app: &mut App, is_error: bool) {
    finish_subagent(app, TASK_ID, is_error);
}

#[test]
fn focused_running_subagent_composer_steers_at_the_child_queue() {
    const STEER: &str = "focus on the authentication failure";

    let mut app = test_app();
    app.run_id = 1;
    let (steer_tx, _steer_rx) = caudra_agent::steering_queue();
    let mut info = subagent_info(TASK_ID, RESEARCH_NAME);
    info.steer_tx = Some(steer_tx.clone());
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta {
            text: "working".into(),
        },
        info.clone(),
    ));
    app.active_chat = 1;
    app.sync_subagent_input_target();
    app.subagent_input_box.set_input(STEER.into());

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(actions.is_empty());
    let queued = steer_tx.entries();
    assert_eq!(queued[0].text, STEER);
    assert_eq!(app.chats[1].message_count(), 0, "wait for consumption ack");
    assert!(
        rendered(&mut app).contains(STEER),
        "pending steer must be visible in the task queue panel"
    );

    app.update(subagent_msg_with_info(
        AgentEvent::QueueItemConsumed {
            id: queued[0].id,
            text: STEER.into(),
            image_count: 0,
        },
        info,
    ));
    let last = app.chats[1]
        .message_at(app.chats[1].message_count() - 1)
        .expect("steer bubble missing");
    assert_eq!(last.role, DisplayRole::User);
    assert_eq!(last.text, STEER);
    assert!(!app.pending_subagent_steers.contains_key(TASK_ID));
}

#[test]
fn subagent_paste_submits_expanded() {
    let mut app = test_app();
    app.run_id = 1;
    let (steer_tx, _steer_rx) = caudra_agent::steering_queue();
    let mut info = subagent_info(TASK_ID, RESEARCH_NAME);
    info.steer_tx = Some(steer_tx.clone());
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta {
            text: "working".into(),
        },
        info,
    ));
    app.active_chat = 1;
    app.sync_subagent_input_target();

    app.update(Msg::Paste("a\nb\nc".into()));
    assert_eq!(
        app.subagent_input_box.buffer.display_text(),
        "[Pasted 3 lines] "
    );
    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    assert_eq!(steer_tx.entries()[0].text, "a\nb\nc");
}

/// A running task focused with its composer live, handing back the child
/// queue so a test can see what the composer actually steered.
pub(crate) fn focused_task_composer() -> (App, caudra_agent::SteeringQueue) {
    let mut app = test_app();
    app.run_id = 1;
    let (steer_tx, _steer_rx) = caudra_agent::steering_queue();
    let mut info = subagent_info(TASK_ID, RESEARCH_NAME);
    info.steer_tx = Some(steer_tx.clone());
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta {
            text: "working".into(),
        },
        info,
    ));
    app.active_chat = 1;
    app.sync_subagent_input_target();
    (app, steer_tx)
}

/// A steer becomes the task's latest input, so it has to name the session
/// plan the task inherited or the task loses it mid-run.
#[test]
fn a_task_steer_keeps_the_session_plan() {
    const STEER: &str = "check the plan's second step";

    let (mut app, steer_queue) = focused_task_composer();
    app.state.plan = PlanState::Drafting(PathBuf::from(HANDOFF_PLAN_FILE));
    app.subagent_input_box.set_input(STEER.into());

    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());

    let id = steer_queue.entries()[0].id;
    let steer = steer_queue.remove(id).unwrap();
    assert_eq!(steer.plan, app.state.plan.target());
}

#[test]
fn slash_opens_the_palette_in_a_task_composer() {
    let (mut app, _steer_tx) = focused_task_composer();

    type_slash(&mut app);

    assert!(app.command_palette.is_active());
}

#[test]
fn task_composer_executes_an_allowed_command() {
    let (mut app, steer_tx) = focused_task_composer();

    type_and_submit(&mut app, "/help");

    assert!(app.help_modal.is_open());
    assert!(steer_tx.entries().is_empty(), "command must not steer");
    assert!(app.subagent_input_box.is_empty());
}

#[test_case("/compact" ; "compact")]
#[test_case("/continue" ; "resume")]
#[test_case("/model" ; "model")]
#[test_case("/goal" ; "goal")]
#[test_case("/btw" ; "btw")]
fn task_composer_blocks_a_main_only_command(command: &str) {
    let (mut app, steer_tx) = focused_task_composer();

    let actions = type_and_submit(&mut app, command);

    assert_eq!(app.status_bar.flash_text(), Some(MAIN_ONLY_CMD_MSG));
    assert!(actions.is_empty(), "{command} must not act on the main run");
    assert!(steer_tx.entries().is_empty(), "{command} must not steer");
}

#[test]
fn main_only_commands_still_run_from_the_main_composer() {
    let mut app = test_app();

    let actions = type_and_submit(&mut app, "/compact");

    assert!(actions.iter().any(|a| matches!(a, Action::Compact)));
}

#[test_case("/workflow", Feature::Workflows; "workflow")]
#[test_case("/workflows", Feature::Workflows; "workflows")]
#[test_case("/peers", Feature::CrossSessionMessaging; "peers")]
#[test_case("/messages", Feature::CrossSessionMessaging; "messages")]
#[test_case("/deep-research", Feature::Workflows; "workflow_shortcut")]
#[test_case("/sandbox", Feature::Sandboxes; "sandbox")]
#[test_case("/decisions", Feature::DecisionEngine; "decisions")]
#[test_case("/auto", Feature::DecisionEngine; "auto")]
fn a_command_whose_experiment_is_off_names_the_switch(command: &str, feature: Feature) {
    let mut app = test_app();
    app.features = FeatureFlags::all().without(feature);
    let expected = FeatureDisabled(feature).to_string();

    assert!(app.execute_command(cmd(command), 0).is_empty());
    assert_eq!(app.status_bar.flash_text(), Some(expected.as_str()));
    assert_eq!(app.run_cmdline(command, 0).err(), Some(expected));
    assert!(!app.sandbox_manager.is_open());
    assert!(!app.peer_manager.is_open());
    assert_eq!(app.permissions.mode(), PermissionMode::Ask);
}

#[test_case("/peers", false; "sessions")]
#[test_case("/messages", true; "held")]
fn peer_commands_leave_runtime_eligibility_to_the_event_loop(command: &str, held: bool) {
    let mut app = test_app();
    let actions = app.run_cmdline(command, 0).unwrap();
    if held {
        assert!(matches!(&actions[..], [Action::PeerMessages(args)] if args.is_empty()));
    } else {
        assert!(matches!(&actions[..], [Action::ListPeers]));
    }
    assert!(!app.peer_manager.is_open());
    assert!(app.state.session.messages().is_empty());
}

#[test_case(PeerView::Sessions; "sessions")]
#[test_case(PeerView::Held; "held")]
fn peer_manager_is_modal_and_replaces_browse_overlays(view: PeerView) {
    let mut app = test_app();
    app.input_box.set_input(PEER_MANAGER_DRAFT.into());
    app.help_modal.toggle();
    app.which_key.arm();
    app.automatic_wakes_suppressed = true;
    app.open_peer_manager(view);

    assert!(app.peer_manager.is_open());
    assert!(app.any_overlay_open());
    assert!(app.has_modal_overlay());
    assert!(!app.composer_holds_keys());
    assert!(!app.help_modal.is_open());
    assert!(!app.which_key.is_armed());
    assert_eq!(
        app.overlays()
            .iter()
            .filter(|overlay| overlay.is_open())
            .count(),
        PEER_MANAGER_OPEN_COUNT
    );
    assert!(rendered(&mut app).contains(PEER_MANAGER_TITLE));
    assert_eq!(app.input_box.buffer.value(), PEER_MANAGER_DRAFT);
    assert!(app.automatic_wakes_suppressed);
}

#[test_case(PeerView::Sessions, false, PEER_MANAGER_PASTE; "sessions_navigation")]
#[test_case(PeerView::Sessions, true, PEER_MANAGER_PASTE; "sessions_filter")]
#[test_case(PeerView::Held, false, PEER_MANAGER_PASTE; "held_navigation")]
#[test_case(PeerView::Held, true, PEER_MANAGER_PASTE; "held_filter")]
#[test_case(PeerView::Sessions, false, ""; "empty_clipboard")]
#[test_case(PeerView::Sessions, true, PEER_MANAGER_IMAGE_PATH; "image_path_filter")]
fn peer_manager_captures_paste_before_the_hidden_prompt_and_image_detection(
    view: PeerView,
    filtering: bool,
    text: &str,
) {
    let mut app = test_app();
    app.input_box.set_input(PEER_MANAGER_DRAFT.into());
    app.open_peer_manager(view);
    rendered(&mut app);
    if filtering {
        assert!(app.update(Msg::Key(key(KeyCode::Char('/')))).is_empty());
    }
    assert!(app.update(Msg::Paste(text.into())).is_empty());
    assert!(app.peer_manager.is_open());
    assert_eq!(app.input_box.buffer.value(), PEER_MANAGER_DRAFT);
    assert!(app.image_paste_rx.is_empty());
    assert!(app.input_box.pending_images().is_empty());
    if filtering {
        assert!(rendered(&mut app).contains(text));
    }
    app.route_text_paste(PEER_MANAGER_PASTE);
    assert_eq!(app.input_box.buffer.value(), PEER_MANAGER_DRAFT);
}

#[test_case(false, KeyFocus::Composer; "escape_to_composer")]
#[test_case(false, KeyFocus::Transcript; "escape_to_transcript")]
#[test_case(true, KeyFocus::Composer; "close_all_to_composer")]
fn closing_peer_manager_preserves_prompt_and_focus(close_all: bool, focus: KeyFocus) {
    let mut app = test_app();
    app.input_box.set_input(PEER_MANAGER_DRAFT.into());
    app.key_focus = focus;
    app.open_peer_manager(PeerView::Sessions);
    rendered(&mut app);
    if close_all {
        app.close_all_overlays();
    } else {
        assert!(app.update(Msg::Key(key(KeyCode::Esc))).is_empty());
    }
    assert!(!app.peer_manager.is_open());
    assert!(!app.any_overlay_open());
    assert_eq!(app.input_box.buffer.value(), PEER_MANAGER_DRAFT);
    assert_eq!(app.key_focus, focus);
    assert_eq!(app.composer_holds_keys(), focus == KeyFocus::Composer);
    assert_eq!(app.exit_request, ExitRequest::None);
}

#[test_case(KeyEventKind::Press; "press")]
#[test_case(KeyEventKind::Repeat; "repeat")]
#[test_case(KeyEventKind::Release; "release")]
fn peer_manager_owns_keys_instead_of_the_hidden_prompt(kind: KeyEventKind) {
    let mut app = test_app();
    app.input_box.set_input(PEER_MANAGER_DRAFT.into());
    app.open_peer_manager(PeerView::Sessions);
    rendered(&mut app);
    for event in [
        key(KeyCode::Char('z')),
        key(KeyCode::Enter),
        kb::HELP.to_key_event(),
        kb::LEADER.to_key_event(),
        kb::EXIT.to_key_event(),
    ] {
        assert!(dispatch_reported_key(&mut app, event, kind).is_empty());
    }
    assert_eq!(app.input_box.buffer.value(), PEER_MANAGER_DRAFT);
    assert!(app.peer_manager.is_open());
    assert!(!app.which_key.is_armed());
    assert!(!app.help_modal.is_open());
    assert_eq!(app.exit_request, ExitRequest::None);
}

#[test_case(MouseEventKind::Down(MouseButton::Middle); "middle_click")]
#[test_case(MouseEventKind::Down(MouseButton::Right); "right_click")]
#[test_case(MouseEventKind::Drag(MouseButton::Left); "drag")]
#[test_case(MouseEventKind::Up(MouseButton::Left); "release")]
#[test_case(MouseEventKind::Moved; "hover")]
#[test_case(MouseEventKind::ScrollUp; "wheel_up")]
#[test_case(MouseEventKind::ScrollDown; "wheel_down")]
#[test_case(MouseEventKind::ScrollLeft; "pan_left")]
#[test_case(MouseEventKind::ScrollRight; "pan_right")]
fn peer_manager_owns_pointer_events_outside_its_surface(kind: MouseEventKind) {
    let mut app = test_app();
    app.input_box.set_input(PEER_MANAGER_DRAFT.into());
    app.open_peer_manager(PeerView::Sessions);
    rendered(&mut app);
    app.chats[0].restore_scroll(OVERLAY_SEED_SCROLL, false);
    let (column, row) = OUTSIDE_MODAL;
    assert!(app.update(mouse_event(kind, column, row)).is_empty());
    assert!(
        app.update(Msg::Scroll {
            column,
            row,
            delta: EDGE_SCROLL_LINES
        })
        .is_empty()
    );
    assert_eq!(app.chats[0].scroll_top(), OVERLAY_SEED_SCROLL);
    assert!(!app.chats[0].auto_scroll());
    assert!(app.peer_manager.is_open());
    assert!(app.selection_state.is_none());
    assert!(app.autoscroll.is_none());
    assert_eq!(app.input_box.buffer.value(), PEER_MANAGER_DRAFT);
}

#[test]
fn peer_manager_discovery_cadence_and_reset_use_the_overlay_registry() {
    let mut app = app_without_splash();
    app.open_peer_manager(PeerView::Sessions);
    app.peer_manager.start_discovery();
    assert_eq!(app.cadence(), app.peer_manager.cadence());
    assert_eq!(app.cadence(), Cadence::PENDING);
    app.peer_manager.set_sessions(Ok(Vec::new()));
    assert_eq!(app.cadence(), Cadence::IDLE);
    app.peer_manager.start_discovery();
    assert_eq!(app.cadence(), Cadence::PENDING);
    app.close_all_overlays();
    assert_eq!(app.cadence(), Cadence::IDLE);
    app.open_peer_manager(PeerView::Held);
    app.reset_ui_chrome();
    assert!(!app.peer_manager.is_open());
}

#[test]
fn peer_manager_operational_actions_are_handed_to_the_event_loop() {
    let mut app = test_app();
    app.open_peer_manager(PeerView::Sessions);
    assert!(matches!(
        &app.handle_peer_manager_action(PeerManagerAction::Refresh)[..],
        [Action::RefreshPeers]
    ));
    assert!(matches!(
        &app.handle_peer_manager_action(PeerManagerAction::Review(PEER_MANAGER_MESSAGE.into()))[..],
        [Action::ReviewPeerMessage(message)] if message == PEER_MANAGER_MESSAGE
    ));
    assert!(matches!(
        &app.handle_peer_manager_action(PeerManagerAction::SetInbound(InboundPolicy::Hold))[..],
        [Action::SetPeerInbound(InboundPolicy::Hold)]
    ));
    assert!(app.peer_manager.is_open());
    assert!(app.state.session.messages().is_empty());
}

#[test]
fn a_permission_prompt_suppresses_peer_manager_without_discarding_it() {
    let mut app = test_app();
    app.open_peer_manager(PeerView::Sessions);
    app.peer_manager.set_error(PEER_MANAGER_ERROR.into());
    assert!(rendered(&mut app).contains(PEER_MANAGER_ERROR));
    app.permission_prompt.open(
        REPORTED_PERMISSION_FIRST.into(),
        ToolKey::native("bash"),
        vec![REPORTED_PERMISSION_COMMAND.into()],
        None,
    );
    let screen = rendered(&mut app);
    assert!(screen.contains(PERMISSION_TITLE));
    assert!(!screen.contains(PEER_MANAGER_ERROR));
    assert!(app.update(Msg::Key(kb::REFRESH.to_key_event())).is_empty());
    assert!(app.peer_manager.is_open());
    app.permission_prompt.close();
    assert!(rendered(&mut app).contains(PEER_MANAGER_ERROR));
}

#[test]
fn peer_manager_captures_wheel_above_a_question_that_arrives_later() {
    let mut app = test_app();
    app.open_peer_manager(PeerView::Sessions);
    app.question_form.open(vec![AskedQuestion {
        question: (0..PEER_MANAGER_QUESTION_LINES)
            .map(|index| format!("{index}: {QUESTION_TEXT}"))
            .collect::<Vec<_>>()
            .join("\n"),
        header: QUESTION_HEADER.into(),
        options: vec![QuestionOption {
            label: QUESTION_OPTION.into(),
            description: String::new(),
        }],
        multiple: false,
    }]);
    rendered(&mut app);
    let (_, bottom, ..) = app.layout_geometry(TEST_AREA);
    let position = (bottom.y..bottom.bottom())
        .flat_map(|row| (bottom.x..bottom.right()).map(move |column| Position::new(column, row)))
        .find(|position| {
            app.peer_manager.contains(*position) && app.question_form.contains(*position)
        })
        .unwrap();
    let render_question = |app: &mut App| {
        let mut terminal =
            Terminal::new(TestBackend::new(TEST_AREA.width, TEST_AREA.height)).unwrap();
        terminal
            .draw(|frame| app.question_form.view(frame, bottom))
            .unwrap();
        buffer_text(terminal.backend().buffer())
    };
    let before = render_question(&mut app);
    for delta in [EDGE_SCROLL_LINES, -EDGE_SCROLL_LINES] {
        assert!(
            app.update(Msg::Scroll {
                column: position.x,
                row: position.y,
                delta,
            })
            .is_empty()
        );
        assert_eq!(render_question(&mut app), before);
    }
    assert!(app.peer_manager.is_open());
    assert!(app.question_form.is_open());
}

#[test]
fn a_typed_hidden_command_names_the_switch_instead_of_prompting() {
    let mut app = test_app();
    app.features = FeatureFlags::NONE;

    let actions = type_and_submit(&mut app, "/sandbox");

    let expected = FeatureDisabled(Feature::Sandboxes).to_string();
    assert_eq!(app.status_bar.flash_text(), Some(expected.as_str()));
    assert!(actions.is_empty());
    assert!(app.state.session.messages().is_empty());
}

fn remote_binding(sandbox: bool) -> StoredWorkspaceBinding {
    let workspace = remote_workspace_session();
    let binding = StoredWorkspaceBinding::new_with_cursor(
        workspace.binding().clone(),
        workspace.cursor().clone(),
        None,
    )
    .unwrap();
    if sandbox {
        binding.with_sandbox_record(CaudraId::generate()).unwrap()
    } else {
        binding
    }
}

#[test_case(true, Feature::Sandboxes; "managed_sandbox")]
#[test_case(false, Feature::RemoteWorkcell; "direct_remote")]
fn remote_control_follows_the_session_source(sandbox: bool, feature: Feature) {
    let mut app = test_app();
    app.state.session = Arc::new(AppSession::new_with_workspace(
        "test",
        ".",
        remote_binding(sandbox),
    ));
    let other = if sandbox {
        Feature::RemoteWorkcell
    } else {
        Feature::Sandboxes
    };
    app.features = FeatureFlags::NONE.with(other);

    assert!(app.execute_command(cmd("/remote"), 0).is_empty());
    let expected = FeatureDisabled(feature).to_string();
    assert_eq!(app.status_bar.flash_text(), Some(expected.as_str()));

    app.features = FeatureFlags::NONE.with(feature);
    assert!(matches!(
        app.execute_command(cmd("/remote"), 0).as_slice(),
        [Action::RemoteControl(_)]
    ));
}

#[test]
fn run_cmdline_reports_main_only_commands_in_a_task_view() {
    let (mut app, _steer_tx) = focused_task_composer();

    let error = app.run_cmdline("/compact", 0).err();

    assert_eq!(error.as_deref(), Some(MAIN_ONLY_CMD_MSG));
}

#[test]
fn slash_noncommand_still_steers_a_task() {
    const NOT_A_COMMAND: &str = "/nonexistent";
    let (mut app, steer_tx) = focused_task_composer();

    type_and_submit(&mut app, NOT_A_COMMAND);

    assert!(app.status_bar.flash_text().is_none());
    assert_eq!(steer_tx.entries()[0].text, NOT_A_COMMAND);
}

fn with_custom_command(app: &mut App, name: &str, content: &str) {
    app.command_palette = CommandPalette::new(
        Arc::from([CustomCommand {
            name: name.to_string(),
            description: String::new(),
            content: content.to_string(),
            scope: caudra_agent::command::CommandScope::Project,
            accepts_args: false,
            source: caudra_agent::command::CommandSource::Local(PathBuf::from(
                "/project/.caudra/commands/custom.md",
            )),
        }]),
        McpSnapshotReader::empty(),
        LuaCommandReader::empty(),
        FeatureFlags::all(),
    );
}

#[test]
fn custom_command_steers_the_focused_task() {
    const RENDERED: &str = "check the retry path";
    let (mut app, steer_tx) = focused_task_composer();
    with_custom_command(&mut app, "audit", RENDERED);

    let actions = type_and_submit(&mut app, "/project:audit");

    assert!(actions.is_empty(), "the main session must not run it");
    assert_eq!(steer_tx.entries()[0].text, RENDERED);
}

/// A finished task has no composer, so the modal palette is the only way in.
/// The template has nowhere to steer and must not be swallowed.
#[test]
fn custom_command_falls_back_to_main_when_the_task_cannot_be_steered() {
    const RENDERED: &str = "check the retry path";
    let (mut app, _steer_tx) = focused_task_composer();
    with_custom_command(&mut app, "audit", RENDERED);
    app.subagent_steers.remove(TASK_ID);
    app.chats[app.active_chat].mark_finished(TaskOutcome::Done, DONE_TEXT);

    let actions = app.execute_command(cmd("/project:audit"), 0);

    assert!(actions.iter().any(|a| matches!(a, Action::SendMessage(..))));
}

#[test]
fn file_picker_inserts_into_the_focused_task_composer() {
    const PATH: &str = "src/main.rs";
    let (mut app, _steer_tx) = focused_task_composer();

    app.handle_file_picker_action(FilePickerModalAction::Select(PATH.into()));

    assert!(app.subagent_input_box.buffer.value().contains(PATH));
    assert!(app.input_box.is_empty(), "the main draft must be untouched");
}

#[test]
fn pasted_image_steers_the_focused_task() {
    const STEER: &str = "look at this";
    let (mut app, steer_tx) = focused_task_composer();
    app.subagent_input_box
        .attach_image(ImageSource::new(ImageMediaType::Png, Arc::from("aGVsbG8=")));
    app.subagent_input_box.set_input(STEER.into());

    app.update(Msg::Key(key(KeyCode::Enter)));

    assert_eq!(steer_tx.entries()[0].text, STEER);
    assert!(app.subagent_input_box.is_empty());
}

#[test]
fn ctrl_shortcuts_reach_a_focused_task_composer() {
    let (mut app, _steer_tx) = focused_task_composer();

    app.update(Msg::Key(kb::FILE_PICKER.to_key_event()));
    assert!(app.file_picker.is_open(), "Ctrl+S");
    app.file_picker.close();

    app.update(Msg::Key(kb::SEARCH.to_key_event()));
    assert!(app.search_modal.is_open(), "Ctrl+F");
}

/// A finished task draws no composer, so the chords that only make sense with
/// one stay inert while the transcript-wide ones keep working.
#[test]
fn a_read_only_task_takes_search_but_not_the_file_picker() {
    let mut app = read_only_task_app();

    app.update(Msg::Key(kb::FILE_PICKER.to_key_event()));
    assert!(!app.file_picker.is_open(), "nothing to insert a path into");

    app.update(Msg::Key(kb::SEARCH.to_key_event()));
    assert!(app.search_modal.is_open());
}

#[test]
fn switching_tasks_closes_the_palette() {
    let (mut app, _steer_tx) = focused_task_composer();
    type_slash(&mut app);
    assert!(app.command_palette.is_active());

    app.focus_task(MAIN_TASK_ID).unwrap();
    app.sync_subagent_input_target();

    assert!(!app.command_palette.is_active());
}

#[test]
fn unconsumed_subagent_steer_remains_unsent_on_close() {
    const STEER: &str = "do not change the public API";

    let mut app = test_app();
    app.run_id = 1;
    let (steer_tx, _steer_rx) = caudra_agent::steering_queue();
    let mut info = subagent_info(TASK_ID, RESEARCH_NAME);
    info.steer_tx = Some(steer_tx);
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta {
            text: "working".into(),
        },
        info,
    ));
    app.active_chat = 1;
    app.sync_subagent_input_target();
    app.subagent_input_box.set_input(STEER.into());
    app.update(Msg::Key(key(KeyCode::Enter)));
    close_subagent_transcript(&mut app, TASK_ID);

    assert_eq!(app.subagent_input_box.buffer.value(), "");
    assert!(!app.subagent_steers.contains_key(TASK_ID));
    assert!(!app.pending_subagent_steers.contains_key(TASK_ID));
    assert_eq!(app.unsent_subagent_steers[TASK_ID][0].text, STEER);
}

#[test]
fn main_error_preserves_unconsumed_subagent_steers_as_unsent() {
    const STEER: &str = "keep this guidance";
    let mut app = test_app();
    app.run_id = 1;
    let (steer_queue, _receiver) = caudra_agent::steering_queue();
    let mut info = subagent_info(TASK_ID, RESEARCH_NAME);
    info.steer_tx = Some(steer_queue);
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta {
            text: "working".into(),
        },
        info,
    ));
    app.active_chat = 1;
    app.sync_subagent_input_target();
    app.subagent_input_box.set_input(STEER.into());
    app.update(Msg::Key(key(KeyCode::Enter)));
    app.active_chat = 0;

    app.update(agent_msg(AgentEvent::Error {
        message: "provider failed".into(),
    }));

    assert_eq!(app.unsent_subagent_steers[TASK_ID][0].text, STEER);
    assert!(!app.pending_subagent_steers.contains_key(TASK_ID));
}

#[test]
fn continuation_reuses_stable_task_chat_and_new_parent_tool_id() {
    const FIRST_TOOL_ID: &str = "tool-first";
    const NEXT_TOOL_ID: &str = "tool-next";
    const CONTINUED_PROMPT: &str = "check the failing test now";

    let mut app = test_app();
    app.run_id = 1;
    let mut first = subagent_info(FIRST_TOOL_ID, RESEARCH_NAME);
    first.task_id = TASK_ID.into();
    first.prompt = Some("inspect the tests".into());
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta {
            text: "first result".into(),
        },
        first,
    ));
    close_subagent_transcript(&mut app, TASK_ID);
    finish_subagent(&mut app, FIRST_TOOL_ID, false);
    assert!(app.chats[1].is_finished());

    let mut continued = subagent_info(NEXT_TOOL_ID, RESEARCH_NAME);
    continued.task_id = TASK_ID.into();
    continued.prompt = Some(CONTINUED_PROMPT.into());
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta {
            text: "continued result".into(),
        },
        continued,
    ));

    assert_eq!(app.chats.len(), 2);
    assert!(!app.chats[1].is_finished());
    assert_eq!(
        app.parent_task_ids.get(NEXT_TOOL_ID).map(String::as_str),
        Some(TASK_ID)
    );
    assert!(
        (0..app.chats[1].message_count()).any(|idx| {
            app.chats[1]
                .message_at(idx)
                .is_some_and(|message| message.text == CONTINUED_PROMPT)
        }),
        "continued prompt missing from reused chat"
    );

    close_subagent_transcript(&mut app, TASK_ID);
    finish_subagent(&mut app, NEXT_TOOL_ID, false);
    assert!(app.chats[1].is_finished());
}

#[test]
fn subagent_done_only_in_subagent_chat() {
    let mut app = app_with_subagent();
    finish_subagent_task(&mut app, false);
    assert_ne!(app.chats[0].last_message_role(), Some(&DisplayRole::Done));
}

#[test_case(|app: &mut App| finish_subagent_task(app, false), DONE_TEXT,      &DisplayRole::Done  ; "task_success")]
#[test_case(|app: &mut App| finish_subagent_task(app, true),  ERROR_TEXT,     &DisplayRole::Error ; "task_failure")]
#[test_case(cancel_app as fn(&mut App),                       CANCELLED_TEXT, &DisplayRole::Error ; "cancel")]
#[test_case(error_app  as fn(&mut App),                       ERROR_TEXT,     &DisplayRole::Error ; "main_error")]
fn subagent_terminal_marker(
    terminate: fn(&mut App),
    expected_text: &str,
    expected_role: &DisplayRole,
) {
    let mut app = app_with_subagent();
    terminate(&mut app);
    assert_eq!(app.chats[1].last_message_text(), expected_text);
    assert_eq!(app.chats[1].last_message_role(), Some(expected_role));
}

#[test_case(error_app  as fn(&mut App) ; "error")]
#[test_case(cancel_app as fn(&mut App) ; "cancel")]
fn subagent_already_done_not_double_marked(terminate: fn(&mut App)) {
    let mut app = app_with_subagent();
    finish_subagent_task(&mut app, false);
    let count_before = app.chats[1].message_count();
    terminate(&mut app);
    assert_eq!(app.chats[1].message_count(), count_before);
    assert_eq!(app.chats[1].last_message_text(), DONE_TEXT);
}

#[test_case(false, DONE_TEXT,  &DisplayRole::Done  ; "batch_subagent_success")]
#[test_case(true,  ERROR_TEXT, &DisplayRole::Error ; "batch_subagent_failure")]
fn batch_subagent_done_marker(is_error: bool, expected_text: &str, expected_role: &DisplayRole) {
    let mut app = app_with_subagent_id("batch1__0");
    finish_subagent(&mut app, "batch1__0", is_error);
    assert_eq!(app.chats[1].last_message_text(), expected_text);
    assert_eq!(app.chats[1].last_message_role(), Some(expected_role));
}

pub(crate) fn streaming_app() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app
}

pub(crate) fn start_subagent(app: &mut App, id: &str, name: &str) {
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "x".into() },
        id,
        Some(name),
    ));
}

pub(crate) fn app_with_subagent_id(id: &str) -> App {
    let mut app = streaming_app();
    start_subagent(&mut app, id, RESEARCH_NAME);
    app
}

fn app_with_subagent() -> App {
    app_with_subagent_id(TASK_ID)
}

/// The shape the picker filters on: the main chat first without a status, then
/// every status a subagent can report, spelled the way Lua reads it.
#[test]
fn tasks_report_main_chat_then_subagent_outcomes() {
    let mut app = app_with_subagent_id("task1");
    for (id, name) in [("task2", "build"), ("task3", "deploy")] {
        app.update(subagent_msg(
            AgentEvent::TextDelta { text: "y".into() },
            id,
            Some(name),
        ));
    }
    finish_subagent(&mut app, "task1", false);
    finish_subagent(&mut app, "task2", true);

    let tasks = serde_json::to_value(app.tasks()).unwrap();
    assert_eq!(
        tasks,
        serde_json::json!([
            { "id": "main", "name": "Main", "focused": true },
            { "id": "task1", "name": "research", "status": "done", "focused": false },
            { "id": "task2", "name": "build", "status": "error", "focused": false },
            { "id": "task3", "name": "deploy", "status": "working", "focused": false },
        ])
    );
}

/// Escaping out of a subagent takes the single-chat cancel path instead of the
/// sweep over the whole turn, and that path has to land the task in `error`
/// too, or it spins forever.
#[test]
fn cancelling_from_inside_a_subagent_reports_error() {
    let mut app = app_with_subagent();
    app.focus_task(TASK_ID).unwrap();
    cancel_app(&mut app);
    assert_eq!(
        serde_json::to_value(app.tasks()).unwrap()[1]["status"],
        serde_json::json!("error")
    );
}

const OVERLAY_BLOCKED_KEYS: &[KeyEvent] = &[
    kb::EXIT.to_key_event(),
    kb::SCROLL_HALF_UP.to_key_event(),
    kb::PAGE_UP.to_key_event(),
    kb::PAGE_DOWN.to_key_event(),
    kb::DOC_TOP.to_key_event(),
    kb::DOC_BOTTOM.to_key_event(),
    kb::HELP.to_key_event(),
];

fn open_help(app: &mut App) {
    app.help_modal.toggle();
}

fn open_search(app: &mut App) {
    app.search_modal.open(0, true);
}

fn focus_queue(app: &mut App) {
    app.active_chat = 0;
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue_and_notify(queued_msg("q"));
    app.queue.set_focus_at(0);
}

/// Off both ends of the document so every blocked key would visibly move it:
/// the top and half-page binds clamp it to 0, and unpinning is observable.
const OVERLAY_SEED_SCROLL: u32 = 5;

#[test_case(open_help as fn(&mut App) ; "help_modal")]
#[test_case(open_search               ; "search_modal")]
#[test_case(open_peer_manager         ; "peer_manager")]
#[test_case(focus_queue               ; "queue_focus")]
fn overlay_blocks_ctrl_shortcuts(setup: fn(&mut App)) {
    let mut app = app_with_subagent();
    setup(&mut app);
    let before = app.active_chat;
    app.chats[before].restore_scroll(OVERLAY_SEED_SCROLL, false);

    for k in OVERLAY_BLOCKED_KEYS {
        app.update(Msg::Key(*k));
    }

    assert_eq!(
        app.active_chat, before,
        "active_chat changed through overlay"
    );
    assert_eq!(
        app.chats[app.active_chat].scroll_top(),
        OVERLAY_SEED_SCROLL,
        "scroll changed through overlay"
    );
    assert!(
        !app.chats[app.active_chat].auto_scroll(),
        "auto-scroll re-armed through overlay"
    );
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.last_exit.is_none());
}

#[test]
fn compact_command_sets_streaming() {
    let mut app = test_app();
    let actions = app.execute_command(cmd("/compact"), 0);
    assert!(matches!(&actions[0], Action::Compact));
    assert_eq!(app.status, Status::Streaming);
}

/// A resume must reach the agent carrying nothing but its flag: no bubble,
/// no text, so the model sees only the history it was already working from.
#[test]
fn continue_command_starts_a_run_with_no_message() {
    let mut app = test_app();
    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(HistorySnapshot::new(
        crate::history_items(&[
            Message::user(RESUME_PROMPT_TEXT.into()),
            assistant_message(RESUME_PARTIAL_TEXT),
        ]),
    ))));
    let before = app.main_chat().message_count();

    let actions = app.execute_command(cmd("/continue"), 0);

    let [Action::SendMessage(input)] = actions.as_slice() else {
        panic!("continue must start a run");
    };
    assert!(input.message.is_empty());
    assert!(input.resume);
    assert_eq!(app.main_chat().message_count(), before);
}

/// A turn killed mid-run keeps whatever it wrote, so the error has to name the one command that
/// picks it back up. Nothing else in the UI does.
#[test]
fn an_error_after_a_turn_died_names_the_resume() {
    const FAILED_MARKER: &str = "[Run failed: inference engine is unavailable]";
    const PROVIDER_ERROR: &str = "provider failed";
    let mut app = test_app();
    app.run_id = 1;
    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(HistorySnapshot::new(
        crate::history_items(&[
            Message::user(RESUME_PROMPT_TEXT.into()),
            Message::synthetic(FAILED_MARKER.into()),
        ]),
    ))));

    app.update(agent_msg(AgentEvent::Error {
        message: PROVIDER_ERROR.into(),
    }));

    let Status::Error { message, .. } = &app.status else {
        panic!("an error must leave the session in Status::Error");
    };
    assert!(message.contains(PROVIDER_ERROR), "got: {message}");
    assert!(message.contains(queue::CONTINUE_HINT), "got: {message}");
}

/// An error that landed before the turn wrote anything leaves no marker, and offering a resume
/// there would point at a command that refuses.
#[test]
fn an_error_with_nothing_to_resume_offers_no_hint() {
    const PROVIDER_ERROR: &str = "provider failed";
    let mut app = test_app();
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::Error {
        message: PROVIDER_ERROR.into(),
    }));

    let Status::Error { message, .. } = &app.status else {
        panic!("an error must leave the session in Status::Error");
    };
    assert!(!message.contains(queue::CONTINUE_HINT), "got: {message}");
}

#[test]
fn continue_command_is_refused_while_streaming() {
    let mut app = test_app();
    app.status = Status::Streaming;

    let actions = app.execute_command(cmd("/continue"), 0);

    assert!(actions.is_empty());
    assert_eq!(app.status_bar.flash_text(), Some(queue::CONTINUE_BUSY_ERR));
}

#[test]
fn continue_command_is_refused_with_nothing_to_resume() {
    let mut app = test_app();

    let actions = app.execute_command(cmd("/continue"), 0);

    assert!(actions.is_empty());
    assert_eq!(app.status_bar.flash_text(), Some(queue::CONTINUE_EMPTY_ERR));
}

#[test]
fn compact_during_streaming_queues_item() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    let actions = app.execute_command(cmd("/compact"), 0);
    assert!(actions.is_empty());
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.panel_entries()[0].text, "/compact");
}

#[test]
fn cancel_clears_pending_input() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.pending_input = PendingInput::AuthRetry {
        waiters: HashSet::from([None]),
    };
    cancel_app(&mut app);
    assert_eq!(app.pending_input, PendingInput::None);
}

#[test]
fn scroll_disables_auto_scroll() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().enable_auto_scroll();

    app.update(Msg::Scroll {
        column: 10,
        row: 10,
        delta: 3,
    });
    assert!(!app.chats[0].auto_scroll());
}

#[test]
fn scroll_outside_msg_area_ignored() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().enable_auto_scroll();

    app.update(Msg::Scroll {
        column: 10,
        row: 25,
        delta: 3,
    });
    assert!(app.chats[0].auto_scroll());
}

#[test]
fn scroll_shortcuts_toggle_auto_scroll() {
    let mut app = test_app();
    app.active_chat().enable_auto_scroll();
    app.update(Msg::Key(kb::SCROLL_TOP.to_key_event()));
    assert!(!app.chats[0].auto_scroll());
    app.update(Msg::Key(kb::SCROLL_BOTTOM.to_key_event()));
    assert!(app.chats[0].auto_scroll());
}

const RESUME_PINS_BOTTOM: &str = "resuming must put the transcript back on its last line";
const RESUME_NEEDS_SCROLLBACK: &str = "the transcript must outgrow its viewport to scroll at all";

const TRANSCRIPT_LINES: usize = 50;
const TRANSCRIPT_AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 80,
    height: 20,
};

const FOCUS_KEPT: &str = "the composer still holds the navigation keys";
const FOCUS_TAKEN: &str = "the transcript holds the navigation keys";
const NAV_KEYS_ARE_BARE: &str = "a modifier would put the key back out of the composer's reach";
const TRANSCRIPT_STILL: &str = "the draft claimed the key, so the transcript stayed put";
const DRAFT_OVERFLOWS: &str = "the draft has to outgrow the composer for the page keys to matter";
const DRAFT_PAGED: &str = "the page key moved the draft";
/// Taller than any composer the test terminal can give a draft.
const TALL_DRAFT_LINES: usize = 20;

/// The area a zone occupies once the app has actually drawn it.
fn rendered_zone(app: &mut App, zone: SelectionZone) -> Rect {
    let _ = rendered(app);
    app.zones.find(zone).expect("zone was not drawn").area
}

/// A transcript long enough to scroll, drawn once so it has a viewport.
fn fill_transcript(app: &mut App) {
    for i in 0..TRANSCRIPT_LINES {
        app.active_chat()
            .push(DisplayMessage::new(DisplayRole::User, format!("line {i}")));
    }
    let backend = ratatui::backend::TestBackend::new(TRANSCRIPT_AREA.width, TRANSCRIPT_AREA.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            app.active_chat().view(frame, TRANSCRIPT_AREA, false, false);
        })
        .unwrap();
}

fn main_chat_app() -> App {
    test_app()
}

fn read_only_task_app() -> App {
    let mut app = app_with_subagent();
    app.focus_task(TASK_ID).unwrap();
    app
}

/// A steerable task owns a second input box, the one path a transcript key
/// could still be swallowed by.
fn steerable_task_app() -> App {
    let mut app = streaming_app();
    let (steer_tx, _steer_rx) = caudra_agent::steering_queue();
    let mut info = subagent_info(TASK_ID, RESEARCH_NAME);
    info.steer_tx = Some(steer_tx);
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta { text: "x".into() },
        info,
    ));
    app.focus_task(TASK_ID).unwrap();
    assert!(app.active_subagent_can_steer());
    app
}

/// Scrolling is claimed in `handle_global_key`, ahead of the main/subagent
/// split, so one press moves whichever transcript is focused.
#[test_case(main_chat_app as fn() -> App ; "main_chat")]
#[test_case(read_only_task_app           ; "read_only_task")]
#[test_case(steerable_task_app           ; "steerable_task")]
fn transcript_scroll_keys_reach_every_chat(build: fn() -> App) {
    let mut app = build();
    fill_transcript(&mut app);
    let half = app.active_chat().half_page() as u32;

    // The first page press is what hands a composer-owning chat its focus.
    app.update(Msg::Key(kb::PAGE_UP.to_key_event()));
    app.update(Msg::Key(kb::DOC_TOP.to_key_event()));
    assert_eq!(app.active_chat().scroll_top(), 0, "Home");
    assert!(!app.chats[app.active_chat].auto_scroll(), "Home must unpin");

    app.update(Msg::Key(kb::PAGE_DOWN.to_key_event()));
    assert_eq!(app.active_chat().scroll_top(), half, "PageDown");

    app.update(Msg::Key(kb::PAGE_UP.to_key_event()));
    assert_eq!(app.active_chat().scroll_top(), 0, "PageUp");

    app.update(Msg::Key(kb::DOC_BOTTOM.to_key_event()));
    assert!(app.chats[app.active_chat].auto_scroll(), "End");
}

/// The Ctrl binds answer wherever the focus sits, so a full draft never has to
/// give the keyboard up to reach the top of the chat.
#[test]
fn ctrl_scroll_binds_ignore_the_focus() {
    let mut app = main_chat_app();
    fill_transcript(&mut app);
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert_eq!(app.key_focus, KeyFocus::Composer);

    app.update(Msg::Key(kb::SCROLL_TOP.to_key_event()));
    assert_eq!(app.active_chat().scroll_top(), 0);
    assert_eq!(app.key_focus, KeyFocus::Composer, "{FOCUS_KEPT}");

    app.update(Msg::Key(kb::SCROLL_BOTTOM.to_key_event()));
    assert!(app.chats[0].auto_scroll());
    assert_eq!(app.key_focus, KeyFocus::Composer, "{FOCUS_KEPT}");
}

#[test]
fn transcript_scroll_binds_use_the_navigation_keys() {
    assert_eq!(kb::PAGE_UP.code, KeyCode::PageUp);
    assert_eq!(kb::PAGE_DOWN.code, KeyCode::PageDown);
    assert_eq!(kb::DOC_TOP.code, KeyCode::Home);
    assert_eq!(kb::DOC_BOTTOM.code, KeyCode::End);
    for bind in [kb::PAGE_UP, kb::PAGE_DOWN, kb::DOC_TOP, kb::DOC_BOTTOM] {
        assert_eq!(bind.modifiers, KeyModifiers::NONE, "{NAV_KEYS_ARE_BARE}");
    }
}

const QUESTION_TEXT: &str = "Which one?";
const QUESTION_HEADER: &str = "Pick";
const QUESTION_OPTION: &str = "First";
const QUESTION_ANSWER: &str = "typed answer";
const QUESTION_ANSWER_HEAD: char = '>';
const EXPECT_RUN_LEFT_ALONE: &str = "only a cancel may stop the run that asked";

fn open_question(app: &mut App) {
    app.question_form
        .open(vec![caudra_agent::types::AskedQuestion {
            question: QUESTION_TEXT.into(),
            header: QUESTION_HEADER.into(),
            options: vec![caudra_agent::types::QuestionOption {
                label: QUESTION_OPTION.into(),
                description: String::new(),
            }],
            multiple: false,
        }]);
}

/// A full transcript with the question form waiting under it.
fn question_app() -> App {
    let mut app = test_app();
    for i in 0..TRANSCRIPT_LINES {
        app.active_chat()
            .push(DisplayMessage::new(DisplayRole::User, format!("line {i}")));
    }
    open_question(&mut app);
    app
}

/// The form is docked under the transcript rather than drawn over it, so the
/// chat behind it can still be read while it waits for an answer.
#[test]
fn the_transcript_scrolls_by_key_while_a_question_is_open() {
    let mut app = question_app();
    let _ = rendered(&mut app);
    app.active_chat().enable_auto_scroll();
    let half = app.active_chat().half_page() as u32;

    app.update(Msg::Key(kb::DOC_TOP.to_key_event()));
    assert_eq!(app.active_chat().scroll_top(), 0, "Home");
    assert!(!app.chats[0].auto_scroll(), "Home must unpin");

    app.update(Msg::Key(kb::PAGE_DOWN.to_key_event()));
    assert_eq!(app.active_chat().scroll_top(), half, "PageDown");

    app.update(Msg::Key(kb::DOC_BOTTOM.to_key_event()));
    assert!(app.chats[0].auto_scroll(), "End");
    assert!(app.question_form.is_open(), "and the question still stands");
}

/// An answer being typed keeps `Home` and `End` for its caret, while the page
/// keys go on moving the transcript behind the form.
#[test]
fn an_answer_being_typed_keeps_home_and_end() {
    let mut app = question_app();
    let _ = rendered(&mut app);
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.question_form.text_input_active());
    app.update(Msg::Paste(QUESTION_ANSWER.into()));
    app.active_chat().enable_auto_scroll();

    app.update(Msg::Key(kb::DOC_TOP.to_key_event()));
    assert!(app.chats[0].auto_scroll(), "Home stays with the answer");
    app.update(Msg::Key(key(KeyCode::Char(QUESTION_ANSWER_HEAD))));
    assert!(rendered(&mut app).contains(&format!("{QUESTION_ANSWER_HEAD}{QUESTION_ANSWER}")));

    app.update(Msg::Key(kb::PAGE_UP.to_key_event()));
    assert!(
        !app.chats[0].auto_scroll(),
        "PageUp still moves the transcript"
    );
    assert!(app.question_form.text_input_active());
}

/// The keys the form owns must not be handed over with them.
#[test]
fn the_form_still_answers_its_own_keys() {
    let mut app = question_app();
    let _ = rendered(&mut app);

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        !app.question_form.is_open(),
        "Enter picks the option under the cursor and answers"
    );
}

/// Dismissing hands the tool a reply it reads as a refusal, so the agent has
/// something to go on and carries on with it.
#[test]
fn escape_dismisses_the_question_without_stopping_the_run() {
    let mut app = question_app();
    let _ = rendered(&mut app);

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty(), "{EXPECT_RUN_LEFT_ALONE}");
    assert!(!app.question_form.is_open());
}

/// Cancelling sends nothing back, so nothing moves until the user does.
#[test]
fn ctrl_c_on_a_question_cancels_the_run() {
    let mut app = question_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let _ = rendered(&mut app);

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(matches!(
        actions.as_slice(),
        [Action::CancelAgent { run_id: 1 }]
    ));
    assert!(!app.question_form.is_open());
}

/// The form is docked over whichever chat is on screen, so the subagent it
/// belongs to is found by id and cancelled on its own.
#[test]
fn ctrl_c_on_a_subagent_question_cancels_only_that_subagent() {
    let mut app = app_with_subagent();
    open_question(&mut app);
    app.question_subagent = Some(TASK_ID.to_owned());
    let run_id = app.run_id;
    let _ = rendered(&mut app);

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(matches!(
        actions.as_slice(),
        [Action::CancelSubagent { tool_use_id }] if tool_use_id == TASK_ID
    ));
    assert_eq!(app.run_id, run_id, "{EXPECT_RUN_LEFT_ALONE}");
    assert!(app.chats[1].is_finished());
    assert!(!app.question_form.is_open());
}

/// Where the pointer is decides whose wheel event it is.
#[test]
fn the_wheel_goes_to_whichever_of_the_two_it_lands_on() {
    let mut app = question_app();
    let _ = rendered(&mut app);
    let (msg_area, bottom_area, ..) = app.layout_geometry(TEST_AREA);

    app.active_chat().enable_auto_scroll();
    app.update(Msg::Scroll {
        column: msg_area.x + 1,
        row: msg_area.y + 1,
        delta: 3,
    });
    assert!(
        !app.chats[0].auto_scroll(),
        "the wheel over the chat has to move the chat"
    );

    app.active_chat().enable_auto_scroll();
    app.update(Msg::Scroll {
        column: bottom_area.x + 1,
        row: bottom_area.y + 1,
        delta: 3,
    });
    assert!(
        app.chats[0].auto_scroll(),
        "the wheel over the form is the form's"
    );
}

#[test]
fn the_transcript_can_be_selected_while_a_question_is_open() {
    let mut app = question_app();
    let _ = rendered(&mut app);
    let (msg_area, ..) = app.layout_geometry(TEST_AREA);
    let row = msg_area.y + 1;

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        msg_area.x + 1,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        msg_area.x + 6,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        msg_area.x + 6,
        row,
    ));
    assert!(
        matches!(
            app.selection_state,
            Some(SelectionState::PendingCopy { .. })
        ),
        "a drag across the transcript has to end in a selection"
    );
}

/// Docked means the form takes rows from the chat instead of covering them,
/// and leaves the status bar where it was.
#[test]
fn the_question_form_docks_between_the_transcript_and_the_status_bar() {
    let (msg_before, ..) = test_app().layout_geometry(TEST_AREA);
    let mut app = question_app();
    let (msg_after, bottom, status, ..) = app.layout_geometry(TEST_AREA);

    assert!(
        msg_after.height < msg_before.height,
        "the chat shrinks to make room for the form"
    );
    assert_eq!(bottom.bottom(), status.y, "the status bar keeps its row");

    let backend = ratatui::backend::TestBackend::new(TEST_AREA.width, TEST_AREA.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.view(frame)).unwrap();
    assert!(
        app.question_form
            .contains(Position::new(bottom.x, bottom.y))
            && !app
                .question_form
                .contains(Position::new(bottom.x, status.y)),
        "the form drew in the rows the layout reserved and nowhere else"
    );
    assert!(
        app.zones.find(SelectionZone::Input).is_none(),
        "the composer is not drawn under a pending question"
    );
}

/// Home and End cannot be taken from a draft by anything but a deliberate page
/// press or a click, so typing never has to wonder where they will land.
#[test]
fn bare_home_and_end_keep_moving_the_input_cursor() {
    let mut app = test_app();
    for c in "abc".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }

    app.update(Msg::Key(key(KeyCode::Home)));
    app.update(Msg::Key(key(KeyCode::Char('!'))));
    assert_eq!(app.input_box.buffer.value(), "!abc");

    app.update(Msg::Key(key(KeyCode::End)));
    app.update(Msg::Key(key(KeyCode::Char('?'))));
    assert_eq!(app.input_box.buffer.value(), "!abc?");
}

/// The round trip the whole design rests on: one page press hands the keys
/// over, and one keystroke of typing hands them back.
#[test]
fn paging_hands_the_navigation_keys_over_and_typing_takes_them_back() {
    let mut app = main_chat_app();
    fill_transcript(&mut app);
    for c in "abc".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.active_chat().enable_auto_scroll();

    app.update(Msg::Key(kb::PAGE_UP.to_key_event()));
    assert_eq!(app.key_focus, KeyFocus::Transcript, "{FOCUS_TAKEN}");
    assert!(!app.chats[0].auto_scroll());

    app.update(Msg::Key(kb::DOC_TOP.to_key_event()));
    assert_eq!(app.active_chat().scroll_top(), 0);
    assert_eq!(
        app.input_box.buffer.value(),
        "abc",
        "the draft is untouched"
    );

    app.update(Msg::Key(key(KeyCode::Char('!'))));
    assert_eq!(app.key_focus, KeyFocus::Composer, "{FOCUS_KEPT}");
    app.update(Msg::Key(key(KeyCode::Home)));
    app.update(Msg::Key(key(KeyCode::Char('?'))));
    assert_eq!(app.input_box.buffer.value(), "?abc!");
}

/// A draft taller than the composer keeps the page keys for itself, so the
/// only text you cannot see is never the text you cannot reach.
#[test]
fn a_draft_that_overflows_pages_itself_and_keeps_the_focus() {
    let mut app = main_chat_app();
    fill_transcript(&mut app);
    for _ in 0..TALL_DRAFT_LINES {
        app.update(Msg::Key(key(KeyCode::Char('x'))));
        app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)));
    }
    let _ = rendered(&mut app);
    app.active_chat().enable_auto_scroll();
    let before = app.active_chat().scroll_top();
    let draft_scroll = app.input_box.scroll_y();
    assert!(draft_scroll > 0, "{DRAFT_OVERFLOWS}");

    app.update(Msg::Key(kb::PAGE_UP.to_key_event()));
    assert_eq!(app.key_focus, KeyFocus::Composer, "{FOCUS_KEPT}");
    assert_eq!(app.active_chat().scroll_top(), before, "{TRANSCRIPT_STILL}");
    assert!(app.input_box.scroll_y() < draft_scroll, "{DRAFT_PAGED}");
}

/// The wheel already scrolls what the pointer is over, so letting it move the
/// focus as well would disarm Home in a half-typed draft.
#[test]
fn the_wheel_scrolls_the_transcript_without_taking_the_focus() {
    let mut app = main_chat_app();
    fill_transcript(&mut app);
    let msg_area = rendered_zone(&mut app, SelectionZone::Messages);
    app.active_chat().enable_auto_scroll();

    app.update(Msg::Scroll {
        column: msg_area.x,
        row: msg_area.y,
        delta: 1,
    });
    assert!(
        !app.chats[0].auto_scroll(),
        "the wheel moved the transcript"
    );
    assert_eq!(app.key_focus, KeyFocus::Composer, "{FOCUS_KEPT}");
}

/// A click is the mouse's way in, and it works in both directions.
#[test_case(SelectionZone::Messages, KeyFocus::Transcript ; "transcript")]
#[test_case(SelectionZone::Input,    KeyFocus::Composer   ; "composer")]
fn clicking_a_surface_gives_it_the_navigation_keys(zone: SelectionZone, expected: KeyFocus) {
    let mut app = main_chat_app();
    fill_transcript(&mut app);
    app.key_focus = match expected {
        KeyFocus::Composer => KeyFocus::Transcript,
        KeyFocus::Transcript => KeyFocus::Composer,
    };
    let area = rendered_zone(&mut app, zone);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x,
        area.y,
    ));
    assert_eq!(app.key_focus, expected);
}

/// Esc backs out of reading without arming the rewind it would otherwise flash.
#[test]
fn esc_returns_the_navigation_keys_to_the_composer() {
    let mut app = main_chat_app();
    fill_transcript(&mut app);
    app.update(Msg::Key(kb::PAGE_UP.to_key_event()));
    assert_eq!(app.key_focus, KeyFocus::Transcript, "{FOCUS_TAKEN}");

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.key_focus, KeyFocus::Composer, "{FOCUS_KEPT}");
    assert!(app.last_esc.is_none(), "the rewind must stay unarmed");
}

/// Focus belongs to the chat it was taken in, so a switch cannot leave the
/// next composer answering keys the reader aimed at a transcript.
#[test]
fn switching_chats_returns_the_navigation_keys_to_the_composer() {
    let mut app = steerable_task_app();
    fill_transcript(&mut app);
    app.update(Msg::Key(kb::PAGE_UP.to_key_event()));
    assert_eq!(app.key_focus, KeyFocus::Transcript, "{FOCUS_TAKEN}");

    app.focus_task(MAIN_TASK_ID).unwrap();
    assert_eq!(app.key_focus, KeyFocus::Composer, "{FOCUS_KEPT}");
}

#[test]
fn mouse_drag_updates_selection() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 20, 10));

    let state = app.selection_state.as_ref().unwrap();
    let (_, end) = state.sel().normalized();
    assert_eq!(end.row, 10);
    assert_eq!(end.col, 20);
}

#[test]
fn right_click_opens_message_actions_without_changing_selection() {
    let mut app = test_app();
    let items = crate::history_items(&[Message::user("hello".into())]);
    app.state.session_mut().replace_messages(items);
    app.restore_display();
    let (column, row) = message_action_position(&mut app);
    let area = app.msg_area();
    let body_column = area.x + 3;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        body_column,
        row,
    ));
    let before = *app.selection_state.as_ref().unwrap().sel();

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Right),
        column,
        row,
    ));

    assert!(app.message_actions.is_open());
    let after = app.selection_state.as_ref().unwrap().sel();
    assert_eq!(after.normalized(), before.normalized());
    assert_eq!(after.area, before.area);
    assert_eq!(after.zone, before.zone);
}

#[test]
fn live_rows_receive_sources_when_history_is_merged() {
    let (_temp, _, _, mut app) = tempdir_app();
    app.main_chat().push_user_message("hello");
    let items = crate::history_items(&[Message::user("hello".into())]);
    publish_history(&mut app, items.clone());

    app.checkpoint_with(Duration::ZERO);

    assert_eq!(
        app.main_chat().message_at(0).unwrap().source,
        Some(DisplaySource::User(items[0].id))
    );
}

const RUN_END_PROMPT: &str = "fix the flaky test";
const RUN_END_REPLY: &str = "the fix is in";
const RUN_END_ERROR: &str = "the connection dropped";
const RUN_END_MSG: &str = "the reply a run ends on must be bound to its history";
const IMAGE_PROMPT: &str = "what does this chart show";
const IMAGE_DATA: &str = "aW1hZ2U=";
const IMAGE_BOUND_MSG: &str = "a prompt with images must be bound to its history";
const IMAGE_RESTORED_MSG: &str = "a restored prompt must read as it was drawn";
const MCP_PROMPT_SERVER_NAME: &str = "review:diff";
const MCP_PROMPT_QUALIFIED_NAME: &str = "review/diff";
const MCP_PROMPT_DESCRIPTION: &str = "Review a diff";
const MCP_PROMPT_ARGS: &str = "main";
const MCP_PROMPT_BODY: &str = "Review the diff against main.";
const MCP_BOUND_MSG: &str = "an MCP prompt must be bound to the message the agent records";
const EARLIER_PROMPT: &str = "the turn the summary replaces";
const EARLIER_REPLY: &str = "an answer the summary covers";
const KEPT_PROMPT: &str = "the turn compaction keeps";
const KEPT_REPLY: &str = "an answer compaction keeps";
const COMPACTION_SUMMARY: &str = "## Objective";
const SEAM_BOUND_MSG: &str = "every row with history must stay bound across a compaction";
const SENT_MSG: &str = "the submission must send one message to the agent";

fn publish_history(app: &mut App, items: Vec<HistoryItem>) {
    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(HistorySnapshot::new(items))));
}

fn main_chat_sources(app: &mut App) -> Vec<Option<DisplaySource>> {
    let chat = app.main_chat();
    (0..chat.message_count())
        .filter_map(|index| chat.message_at(index))
        .map(|message| message.source)
        .collect()
}

pub(crate) fn sent_input(actions: &[Action]) -> &AgentInput {
    let [Action::SendMessage(input)] = actions else {
        panic!("{SENT_MSG}");
    };
    input
}

/// The agent publishes its history before the event that ends the run, so a
/// merge can land while the reply is still streaming and has no row to bind.
#[test_case(done() ; "after_done")]
#[test_case(AgentEvent::Error { message: RUN_END_ERROR.into() } ; "after_error")]
fn the_reply_a_run_ends_on_is_bound_after_an_earlier_merge(end: AgentEvent) {
    let (_temp, _, _, mut app) = tempdir_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.main_chat().push_user_message(RUN_END_PROMPT);
    app.update(agent_msg(AgentEvent::TextDelta {
        text: RUN_END_REPLY.into(),
    }));
    let items = crate::history_items(&[
        Message::user(RUN_END_PROMPT.into()),
        assistant_message(RUN_END_REPLY),
    ]);
    publish_history(&mut app, items.clone());
    app.checkpoint_with(Duration::ZERO);

    app.update(agent_msg(end));

    assert_eq!(
        main_chat_sources(&mut app)[..2],
        [
            Some(DisplaySource::User(items[0].id)),
            Some(DisplaySource::AssistantText(items[1].id)),
        ],
        "{RUN_END_MSG}"
    );
}

#[test]
fn a_prompt_with_images_is_bound_and_restores_as_drawn() {
    let (_temp, _, _, mut app) = tempdir_app();
    let actions = app.start_from_queue(&QueuedMessage {
        images: vec![ImageSource::new(ImageMediaType::Png, Arc::from(IMAGE_DATA))],
        ..queued_msg(IMAGE_PROMPT)
    });
    let input = sent_input(&actions);
    let items = crate::history_items(&[Message::user_with_images(
        input.message.clone(),
        input.images.clone(),
    )]);
    publish_history(&mut app, items.clone());

    app.checkpoint_with(Duration::ZERO);

    assert_eq!(
        main_chat_sources(&mut app),
        [Some(DisplaySource::User(items[0].id))],
        "{IMAGE_BOUND_MSG}"
    );
    let drawn = app.main_chat().message_at(0).unwrap().text.clone();
    app.restore_display();
    assert_eq!(
        app.main_chat().message_at(0).map(|message| &message.text),
        Some(&drawn),
        "{IMAGE_RESTORED_MSG}"
    );
}

/// The agent records the prompt's own messages first and then the command the
/// user typed, which is the text the bubble shows.
#[test]
fn an_mcp_prompt_is_bound_to_the_message_the_agent_records() {
    let (_temp, _, _, mut app) = tempdir_app();
    app.command_palette = CommandPalette::new(
        Arc::from([]),
        McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: Vec::new(),
            prompts: vec![McpPromptInfo {
                display_name: MCP_PROMPT_SERVER_NAME.into(),
                qualified_name: MCP_PROMPT_QUALIFIED_NAME.into(),
                description: MCP_PROMPT_DESCRIPTION.into(),
                arguments: Vec::new(),
            }],
            pids: Vec::new(),
            generation: 0,
        }),
        LuaCommandReader::empty(),
        FeatureFlags::all(),
    );
    let actions = app.execute_mcp_prompt(&format!("/{MCP_PROMPT_SERVER_NAME}"), MCP_PROMPT_ARGS);
    let input = sent_input(&actions);
    let items = crate::history_items(&[
        Message::user(MCP_PROMPT_BODY.into()),
        Message::user(input.message.clone()),
    ]);
    publish_history(&mut app, items.clone());

    app.checkpoint_with(Duration::ZERO);

    assert_eq!(
        main_chat_sources(&mut app),
        [Some(DisplaySource::User(items[1].id))],
        "{MCP_BOUND_MSG}"
    );
}

/// A compaction draws its summary below the turns it keeps, and the history
/// re-adds those turns below the summary with fresh ids. Turns the summary
/// replaced keep the sources they had.
#[test]
fn rows_with_history_stay_bound_across_a_compaction() {
    let (_temp, _, _, mut app) = tempdir_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    for (role, text) in [
        (DisplayRole::User, EARLIER_PROMPT),
        (DisplayRole::Assistant, EARLIER_REPLY),
        (DisplayRole::User, KEPT_PROMPT),
        (DisplayRole::Assistant, KEPT_REPLY),
    ] {
        app.main_chat().push(DisplayMessage::new(role, text.into()));
    }
    let turns = crate::history_items(&[
        Message::user(EARLIER_PROMPT.into()),
        assistant_message(EARLIER_REPLY),
        Message::user(KEPT_PROMPT.into()),
        assistant_message(KEPT_REPLY),
    ]);
    publish_history(&mut app, turns.clone());
    app.checkpoint_with(Duration::ZERO);

    app.update(agent_msg(AgentEvent::Compacting));
    app.update(agent_msg(AgentEvent::TextDelta {
        text: COMPACTION_SUMMARY.into(),
    }));
    app.update(agent_msg(AgentEvent::CompactionDone));
    let mut compacted = crate::history_items(&[
        Message::synthetic(caudra_agent::COMPACTION_ANCHOR.into()),
        Message {
            is_compaction_summary: true,
            ..assistant_message(COMPACTION_SUMMARY)
        },
        Message::user(KEPT_PROMPT.into()),
        assistant_message(KEPT_REPLY),
    ]);
    compacted[0].supersedes = Some(turns[1].id);
    publish_history(&mut app, compacted.clone());
    app.checkpoint_with(Duration::ZERO);

    assert_eq!(
        main_chat_sources(&mut app),
        [
            Some(DisplaySource::User(turns[0].id)),
            Some(DisplaySource::AssistantText(turns[1].id)),
            Some(DisplaySource::User(compacted[2].id)),
            Some(DisplaySource::AssistantText(compacted[3].id)),
            None,
            Some(DisplaySource::AssistantText(compacted[1].id)),
        ],
        "{SEAM_BOUND_MSG}"
    );
}

#[test]
fn left_click_keeps_existing_message_interaction_path() {
    let mut app = test_app();
    let items = crate::history_items(&[Message::user("hello".into())]);
    app.state.session_mut().replace_messages(items);
    app.restore_display();
    let (_, row) = message_action_position(&mut app);
    let body_column = app.msg_area().x + 3;

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        body_column,
        row,
    ));

    assert!(matches!(
        app.selection_state,
        Some(SelectionState::Dragging { .. })
    ));
    assert!(!app.message_actions.is_open());
}

#[test]
fn left_click_message_action_handle_opens_message_actions() {
    let mut app = test_app();
    let items = crate::history_items(&[Message::user("hello".into())]);
    app.state.session_mut().replace_messages(items);
    app.restore_display();
    let (column, row) = message_action_position(&mut app);
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        app.msg_area().x + 3,
        row,
    ));
    let before = *app.selection_state.as_ref().unwrap().sel();

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        column,
        row,
    ));

    assert!(app.message_actions.is_open());
    let after = app.selection_state.as_ref().unwrap().sel();
    assert_eq!(after.normalized(), before.normalized());
    assert_eq!(after.area, before.area);
    assert_eq!(after.zone, before.zone);
}

#[test]
fn message_action_handle_requires_matching_press_and_release() {
    let mut app = test_app();
    let items = crate::history_items(&[Message::user("hello".into())]);
    app.state.session_mut().replace_messages(items);
    app.restore_display();
    let (column, row) = message_action_position(&mut app);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        column + 3,
        row,
    ));

    assert!(!app.message_actions.is_open());
    assert!(app.message_action_mouse_down.is_none());
}

#[test]
fn overlay_zone_wins_over_a_message_action_handle() {
    let mut app = test_app();
    let items = crate::history_items(&[Message::user("hello".into())]);
    app.state.session_mut().replace_messages(items);
    app.restore_display();
    let (column, row) = message_action_position(&mut app);
    set_zone(
        &mut app,
        SelectionZone::Overlay,
        Rect::new(column, row, 1, 1),
    );

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        column,
        row,
    ));

    assert!(!app.message_actions.is_open());
    assert!(app.message_action_mouse_down.is_none());
}

#[test]
fn left_click_message_body_does_not_open_message_actions() {
    let mut app = test_app();
    let items = crate::history_items(&[Message::user("hello".into())]);
    app.state.session_mut().replace_messages(items);
    app.restore_display();
    let (_, row) = message_action_position(&mut app);
    let body_column = app.msg_area().x + 3;

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        body_column,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        body_column,
        row,
    ));

    assert!(!app.message_actions.is_open());
}

#[test]
fn transcript_links_are_encoded_for_the_terminal_host() {
    const URL: &str = "https://example.com/docs";
    let mut app = test_app();
    app.main_chat().push_user_message(format!("[docs]({URL})"));
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            app.view(frame);
            app.apply_terminal_links(frame.buffer_mut());
        })
        .unwrap();

    let linked = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .filter(|cell| cell.symbol().contains(URL))
        .collect::<Vec<_>>();
    assert_eq!(linked.len(), "docs".len());
    assert!(linked.iter().all(|cell| matches!(
        cell.diff_option,
        CellDiffOption::ForcedWidth(width) if width.get() == 1
    )));
}

#[test]
fn matching_link_click_uses_the_local_fallback_only_when_available() {
    const URL: &str = "https://example.com/docs";
    let mut app = test_app();
    app.main_chat().push_user_message(format!("[docs]({URL})"));
    let _ = rendered(&mut app);
    let area = app.msg_area();
    let (row, column) = (area.y..area.bottom())
        .flat_map(|row| (area.x..area.right()).map(move |column| (row, column)))
        .find(|&(row, column)| app.chats[0].link_at(row, column, area).as_deref() == Some(URL))
        .expect("rendered link cell");

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    let actions = app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        column,
        row,
    ));

    if crate::terminal::local_url_opener_available() {
        assert!(matches!(&actions[..], [Action::OpenUrl(target)] if target == URL));
    } else {
        assert!(actions.is_empty());
    }
}

#[test]
fn dragging_cancels_message_action_handle() {
    let mut app = test_app();
    let items = crate::history_items(&[Message::user("hello".into())]);
    app.state.session_mut().replace_messages(items);
    app.restore_display();
    let (column, row) = message_action_position(&mut app);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        column + 8,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        column + 8,
        row,
    ));

    assert!(!app.message_actions.is_open());
    assert!(app.message_action_mouse_down.is_none());
}

#[test]
fn right_click_streaming_row_flashes_unavailable() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::TextDelta {
        text: "partial".into(),
    }));
    let _ = rendered(&mut app);
    let area = app.msg_area();

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Right),
        area.x + 2,
        area.y,
    ));

    assert!(!app.message_actions.is_open());
    assert_eq!(
        app.status_bar.flash_text(),
        Some("Message actions unavailable here")
    );
}

#[test]
fn mouse_drag_clamps_to_area() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        100,
        50,
    ));

    let state = app.selection_state.as_ref().unwrap();
    let (_, end) = state.sel().normalized();
    assert_eq!(end.col, 79);
    assert_eq!(end.row, 19, "clamped to area bottom");
    assert!(
        app.selection_state.as_ref().unwrap().is_edge_scrolling(),
        "outside area triggers edge scroll"
    );
}

#[test_case(Rect::new(0, 2, 80, 20), (10, 12), (10, 1),  Some(EDGE_SCROLL_LINES)  ; "top_edge")]
#[test_case(Rect::new(0, 2, 80, 20), (10, 10), (10, 22), Some(-EDGE_SCROLL_LINES) ; "bottom_edge")]
#[test_case(Rect::new(0, 2, 80, 20), (10, 10), (20, 15), None                     ; "middle_no_scroll")]
fn edge_scroll_direction(zone: Rect, down: (u16, u16), drag: (u16, u16), expected: Option<i32>) {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        down.0,
        down.1,
    ));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        drag.0,
        drag.1,
    ));

    let state = app.selection_state.as_ref().unwrap();
    let edge_dir = match state {
        SelectionState::Dragging { edge_scroll, .. } => edge_scroll.as_ref().map(|es| es.dir),
        _ => None,
    };
    assert_eq!(edge_dir, expected);
}

#[test]
fn mouse_up_clears_edge_scroll() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 2, 80, 20));
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 1));
    assert!(app.selection_state.as_ref().unwrap().is_edge_scrolling());

    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 10, 1));
    let state = app.selection_state.as_ref().unwrap();
    assert!(state.is_pending_copy());
}

#[test]
fn double_esc_cancels_flushes_and_fails_tools() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let _receiver = attach_processing_queue(&mut app);
    app.update(agent_msg(AgentEvent::TextDelta {
        text: "partial".into(),
    }));
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "t1".into(),
        effect: ToolEffect::Unknown,
        tool: "bash".into(),
        summary: "running".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());

    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(matches!(&actions[0], Action::CancelAgent { .. }));
    assert_eq!(app.status, Status::Streaming);
    assert_eq!(app.cancelling_run, Some(1));
    assert_eq!(app.chats[0].in_progress_count(), 0);
}

#[test]
fn double_esc_idle_opens_rewind_picker() {
    let mut app = test_app();
    type_and_submit(&mut app, "hello");
    app.status = Status::Idle;
    app.run_id = 1;
    crate::push_history_message(app.state.session_mut(), Message::user("hello".into()));

    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.rewind_picker.is_open());
}

#[test]
fn double_esc_idle_no_user_turns_flashes_error() {
    let mut app = test_app();
    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.rewind_picker.is_open());
}

#[test_case(Status::Idle, Duration::ZERO ; "rewind_without_ordinary_flash")]
#[test_case(Status::Idle, LONG_FLASH_DURATION ; "rewind_with_long_ordinary_flash")]
#[test_case(Status::Streaming, Duration::ZERO ; "cancel_without_ordinary_flash")]
#[test_case(Status::Streaming, LONG_FLASH_DURATION ; "cancel_with_long_ordinary_flash")]
fn main_esc_confirmation_uses_fixed_window(status: Status, flash_duration: Duration) {
    for (elapsed, confirmed) in CONFIRMATION_TIMINGS {
        let mut app = test_app();
        app.status = status.clone();
        app.run_id = 1;
        let _receiver = attach_processing_queue(&mut app);
        app.status_bar.flash_duration = flash_duration;
        crate::push_history_message(
            app.state.session_mut(),
            Message::user(CONFIRMATION_USER_TEXT.into()),
        );
        let prompt = if status == Status::Streaming {
            FLASH_CANCEL
        } else {
            FLASH_REWIND
        };

        assert!(app.update(Msg::Key(key(KeyCode::Esc))).is_empty());
        assert_eq!(app.status_bar.clear_expired_hint(), Dirty::NO);
        assert_eq!(app.status_bar.flash_text(), Some(prompt));

        let pressed_at = Instant::now().checked_sub(elapsed).unwrap();
        app.last_esc = Some(pressed_at);
        let actions = app.update(Msg::Key(key(KeyCode::Esc)));

        assert_eq!(
            app.rewind_picker.is_open(),
            confirmed && status == Status::Idle
        );
        if confirmed && status == Status::Streaming {
            assert!(matches!(
                actions.as_slice(),
                [Action::CancelAgent { run_id: 1 }]
            ));
            assert_eq!(app.cancelling_run, Some(1));
        } else {
            assert!(actions.is_empty());
            assert_eq!(app.cancelling_run, None);
        }
        if confirmed {
            assert!(app.last_esc.is_none());
        } else {
            assert!(
                app.last_esc
                    .is_some_and(|rearmed_at| rearmed_at > pressed_at)
            );
            assert_eq!(app.status, status);
            assert_eq!(app.status_bar.flash_text(), Some(prompt));
        }
    }
}

#[test]
fn ctrl_c_while_streaming_cancels_instead_of_quitting() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let _receiver = attach_processing_queue(&mut app);

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(matches!(&actions[0], Action::CancelAgent { .. }));
    assert_eq!(app.status, Status::Streaming);
    assert_eq!(app.cancelling_run, Some(1));
    assert_ne!(app.exit_request, ExitRequest::Success);
}

/// The whole point of issue 778: a settled session paints nothing at all. Any
/// poller that starts reporting a change on every tick trips this.
#[test]
fn settled_app_owes_no_frame_and_does_not_animate() {
    let mut app = app_without_splash();

    assert_eq!(app.cadence(), Cadence::IDLE);
    assert_eq!(app.tick(), Dirty::NO, "{QUIET}");
}

/// Nothing wakes the loop when a background thread drops an answer into a
/// shared slot, so `tick` has to go and look. The tick that first sees it is
/// also the only one allowed to claim a frame: `tick` runs on every turn of the
/// loop, so a poller that keeps saying yes never lets it sleep again.
#[track_caller]
fn assert_owes_one_frame(app: &mut App, arrival: impl FnOnce()) {
    assert_eq!(app.tick(), Dirty::NO, "{QUIET}");
    arrival();
    assert_eq!(app.tick(), Dirty::YES, "{OWED}");
    assert_eq!(app.tick(), Dirty::NO, "{QUIET}");
}

/// `/usage` spawns a detached fetch that stores its answer with nothing
/// listening, so an unpolled modal sits on `Loading` until the user presses
/// some unrelated key.
#[test]
fn usage_quota_arriving_in_the_background_owes_a_frame() {
    let mut app = test_app();
    app.execute_command(cmd("/usage"), 0);
    let slot = Arc::clone(&app.usage_slot);

    assert_owes_one_frame(&mut app, || {
        slot.store(Some(Arc::new(UsageFetchState::Loading)));
    });
}

/// Providers publish their model list into a shared slot that wakes nothing,
/// so an open picker keeps showing the stale list until the user happens to
/// press a key.
#[test]
fn model_list_arriving_in_the_background_owes_a_frame() {
    let (mut app, models) = app_with_model_slot();
    app.execute_command(cmd("/model"), 0);
    assert!(app.model_picker.is_open());

    assert_owes_one_frame(&mut app, || {
        models.store(Some(Arc::new(vec![LATE_MODEL_SPEC.into()])));
    });
}

#[test]
fn system_prompt_command_opens_the_viewer() {
    let mut app = test_app();
    app.execute_command(cmd("/system-prompt"), 0);
    assert!(app.system_prompt_modal.is_open());
    assert!(!app.prompt_profile_picker.is_open());
}

/// The viewer is where a profile switch now starts, so its footer has to reach
/// the picker the command used to open directly.
#[test]
fn the_system_prompt_viewer_hands_off_to_the_profile_picker() {
    let mut app = test_app();
    app.execute_command(cmd("/system-prompt"), 0);
    app.update(Msg::Key(key(KeyCode::Char('p'))));

    assert!(!app.system_prompt_modal.is_open());
    assert!(app.prompt_profile_picker.is_open());
}

/// Before the agent binds a prompt no request exists, and the modal says so
/// rather than refusing to open.
#[test]
fn projection_before_a_prompt_is_bound_opens_on_a_notice() {
    let mut app = test_app();
    open_projection_modal(&mut app);

    assert!(app.projection_modal.is_open());
    assert!(app.projection_modal.projection().is_none());
    assert!(rendered(&mut app).contains(PROJECTION_UNPREPARED));
}

/// The next request carries whatever the trailing turn's calls return, so a
/// call still running is shown open rather than repaired with a placeholder.
#[test]
fn projection_holds_the_bound_prompt_and_leaves_a_running_call_open() {
    let mut app = btw_ready_app();
    let running = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::tool_use(
            PROJECTION_CALL_ID,
            PROJECTION_TOOL,
            serde_json::json!({}),
        )],
        ..Default::default()
    };
    let history = [Message::user(PROJECTION_QUESTION.into()), running];
    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(HistorySnapshot::new(
        crate::history_items(&history),
    ))));
    app.state.fast = true;

    open_projection_modal(&mut app);

    let projection = app.projection_modal.projection().unwrap();
    let bound = app.btw_prompt.as_ref().unwrap().load_full();
    assert_eq!(
        projection.opts,
        RequestOptions {
            thinking: app.state.thinking.clone(),
            fast: true,
        },
        "{PROJECTION_OPTIONS_STALE}"
    );
    assert!(!bound.opts.fast, "{PROJECTION_OPTIONS_STALE}");
    assert!(
        Arc::ptr_eq(&projection.prompt, &bound),
        "{PROJECTION_PROMPT_REBUILT}"
    );
    assert_eq!(
        projection.messages.len(),
        history.len(),
        "{PROJECTION_CALL_CLOSED}"
    );
    assert_eq!(projection.running_calls, 1, "{PROJECTION_CALL_CLOSED}");
    assert_eq!(
        projection.cache_key,
        CacheKey::session(&SessionRef::from(app.state.session.id)),
        "{PROJECTION_KEY_WRONG}"
    );
}

#[test]
fn projection_of_an_unreadable_history_flashes_and_opens_nothing() {
    let mut app = btw_ready_app();
    let mut items = crate::history_items(&[Message::user(PROJECTION_QUESTION.into())]);
    items.push(items[0].clone());
    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(HistorySnapshot::new(items))));

    open_projection_modal(&mut app);

    assert!(!app.projection_modal.is_open());
    assert!(
        app.status_bar
            .flash_text()
            .is_some_and(|flash| flash.starts_with(HISTORY_UNREADABLE))
    );
}

/// Tool output streams into a subagent's chat while the parent chat is the one
/// on screen. Draining only the active chat would lose it, and the task picker
/// and a later switch would show nothing.
#[test]
fn tick_drains_live_bufs_of_background_chats() {
    let mut app = test_app();
    app.run_id = 1;
    app.update(agent_msg(tool_start(TASK_ID, "task")));
    app.update(subagent_msg(tool_start(SUB_TOOL_ID, "bash"), TASK_ID, None));
    let buf = Arc::new(caudra_agent::SharedBuf::new());
    app.update(subagent_msg(
        AgentEvent::LiveToolBuf {
            id: SUB_TOOL_ID.into(),
            body: Arc::clone(&buf),
        },
        TASK_ID,
        None,
    ));
    assert_eq!(app.active_chat, 0, "the subagent's chat is the hidden one");

    assert_owes_one_frame(&mut app, || {
        buf.append(caudra_agent::SnapshotLine::plain(TOOL_OUTPUT_LINE.into()));
    });
}

/// A plugin publishes hints from the Lua thread, and the loop never hears back
/// from that thread. The footer they draw in is on screen the whole time, so a
/// publish nobody polled for shows up on some later, unrelated keypress, or
/// never.
#[test]
fn status_hints_published_by_a_plugin_reach_the_screen() {
    let (mut app, plugin) = app_with_hints();
    plugin.publish(vec![(
        Arc::from(HINT_PLUGIN),
        vec![(HINT_TEXT.into(), HINT_STYLE.into())],
    )]);

    assert!(
        !rendered(&mut app).contains(HINT_TEXT),
        "a hint no poller has seen must not be on screen"
    );
    assert_eq!(app.tick(), Dirty::YES, "{OWED}");
    assert!(rendered(&mut app).contains(HINT_TEXT));

    plugin.publish(vec![]);
    assert_eq!(app.tick(), Dirty::YES, "{OWED}");
    assert!(!rendered(&mut app).contains(HINT_TEXT));
}

#[test]
fn open_context_modal_repaints_once_and_renders_the_watched_snapshot() {
    let mut app = app_without_splash();
    let store = ContextStore::new();
    let publisher = store.publisher(ContextKey::Main);
    let mut initial = current_context_snapshot(&app);
    initial.model.provider_display_name = INITIAL_CONTEXT_PROVIDER.into();
    publisher.publish(initial);
    app.context_store = Some(store);
    app.execute_command(cmd(CONTEXT_COMMAND), 0);

    let initial_frame = rendered(&mut app);
    assert!(initial_frame.contains(INITIAL_CONTEXT_PROVIDER));
    assert_eq!(app.tick(), Dirty::NO, "{QUIET}");

    let mut updated = current_context_snapshot(&app);
    updated.model.provider_display_name = UPDATED_CONTEXT_PROVIDER.into();
    publisher.publish(updated);

    let before_poll = rendered(&mut app);
    assert!(before_poll.contains(INITIAL_CONTEXT_PROVIDER));
    assert!(!before_poll.contains(UPDATED_CONTEXT_PROVIDER));
    assert_eq!(app.tick(), Dirty::YES, "{OWED}");

    let after_poll = rendered(&mut app);
    assert!(!after_poll.contains(INITIAL_CONTEXT_PROVIDER));
    assert!(after_poll.contains(UPDATED_CONTEXT_PROVIDER));
    assert_eq!(app.tick(), Dirty::NO, "{QUIET}");
}

#[test]
fn the_logs_command_opens_the_modal_and_esc_closes_it() {
    let mut app = app_without_splash();
    app.execute_command(cmd(LOGS_COMMAND), 0);
    assert!(app.logs_modal.is_open());

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.logs_modal.is_open(), "{LEFT_STANDING}");
}

#[test]
fn a_closed_logs_modal_never_owes_a_repaint() {
    let mut app = app_without_splash();
    app.execute_command(cmd(LOGS_COMMAND), 0);
    app.logs_modal.close();
    assert_eq!(app.tick(), Dirty::NO, "{QUIET}");
}

#[test_case(DOCS_ADDRESS, false ; "an_address_opens_the_reader")]
#[test_case(DOCS_QUERY, true ; "other_words_open_the_search")]
fn the_docs_command_opens_where_its_argument_points(args: &str, searching: bool) {
    let mut app = app_without_splash();
    app.execute_command(
        ParsedCommand {
            name: DOCS_COMMAND.into(),
            args: args.into(),
        },
        0,
    );
    assert!(app.docs_modal.is_open());
    assert_eq!(app.docs_modal.text_input_active(), searching);
}

const HELP_FOOTER_MISSING: &str = "the help footer must offer /docs";
const DOCS_UNOPENED: &str = "a click on /docs in the help footer must open the docs";
const HELP_KEPT_KEYS: &str = "help keeps every key, so it must close for the docs to be read";
const PRESS_SWEPT: &str = "a press on a footer control must not start a sweep";

#[test]
fn a_click_on_docs_in_the_help_footer_opens_the_docs() {
    let mut app = app_without_splash();
    app.help_modal.toggle();
    let _ = rendered(&mut app);
    let hit = app.help_modal.footer_hit(HelpTarget::Command(DOCS_COMMAND));
    assert!(!hit.is_empty(), "{HELP_FOOTER_MISSING}");

    let actions = click_at(&mut app, hit);

    assert!(actions.is_empty());
    assert!(app.docs_modal.is_open(), "{DOCS_UNOPENED}");
    assert!(!app.help_modal.is_open(), "{HELP_KEPT_KEYS}");
    assert!(app.selection_state.is_none(), "{PRESS_SWEPT}");
}

#[test]
fn hidden_context_modal_does_not_repaint_for_publications() {
    let mut app = app_without_splash();
    let store = ContextStore::new();
    let publisher = store.publisher(ContextKey::Main);
    publisher.publish(current_context_snapshot(&app));
    app.context_store = Some(store);
    app.execute_command(cmd(CONTEXT_COMMAND), 0);
    app.context_modal.close();
    assert_eq!(app.tick(), Dirty::NO, "{QUIET}");

    let mut updated = current_context_snapshot(&app);
    updated.model.provider_display_name = UPDATED_CONTEXT_PROVIDER.into();
    publisher.publish(updated);

    assert_eq!(app.tick(), Dirty::NO, "{QUIET}");
    app.execute_command(cmd(CONTEXT_COMMAND), 0);
    assert!(rendered(&mut app).contains(UPDATED_CONTEXT_PROVIDER));
    assert_eq!(app.tick(), Dirty::NO, "{QUIET}");
}

pub(crate) fn rendered(app: &mut App) -> String {
    rendered_wide(app, 80)
}

fn rendered_wide(app: &mut App, width: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.view(frame)).unwrap();
    buffer_text(terminal.backend().buffer())
}

fn message_action_position(app: &mut App) -> (u16, u16) {
    let _ = rendered(app);
    let area = app.msg_area();
    (area.y..area.bottom())
        .flat_map(|row| (area.x..area.right()).map(move |column| (column, row)))
        .find(|&(column, row)| app.chats[0].message_action_at(row, column).is_some())
        .expect("message action handle was not rendered")
}

fn click_queue_action(app: &mut App, action: QueueAction) {
    let _ = rendered(app);
    let hit = app
        .queue_hits
        .iter()
        .find(|hit| {
            matches!(
                hit.target,
                QueueHitTarget::Item {
                    action: hit_action,
                    ..
                } if hit_action == action
            )
        })
        .copied()
        .expect("queue action was not rendered");
    let column = hit.area.x + hit.area.width.saturating_sub(1) / 2;
    let row = hit.area.y;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        column,
        row,
    ));
}

fn queue_select_hit(app: &mut App) -> QueueHit {
    let _ = rendered(app);
    app.queue_hits
        .iter()
        .find(|hit| {
            matches!(
                hit.target,
                QueueHitTarget::Item {
                    action: QueueAction::Select,
                    ..
                }
            )
        })
        .copied()
        .expect(QUEUE_ROW_MISSING)
}

/// A column the affordance does not claim, so a press there is about the row.
fn queue_row_column(hit: QueueHit) -> u16 {
    hit.area.x + QUEUE_TEXT_OFFSET
}

fn open_queue_menu(app: &mut App) {
    click_queue_action(app, QueueAction::Menu);
    assert!(app.queue_actions.is_open(), "{QUEUE_MENU_MISSING}");
}

/// Walks the menu to `kind` rather than assuming a row, so a menu that offers
/// fewer entries for a less capable item still drives the same action.
fn pick_queue_action(app: &mut App, kind: QueueActionKind) -> Vec<Action> {
    let index = app
        .queue_actions
        .kinds()
        .iter()
        .position(|offered| *offered == kind)
        .expect(QUEUE_MENU_ENTRY_MISSING);
    for _ in 0..index {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    app.update(Msg::Key(key(KeyCode::Enter)))
}

fn queue_menu_action(app: &mut App, kind: QueueActionKind) -> Vec<Action> {
    open_queue_menu(app);
    pick_queue_action(app, kind)
}

fn click_queue_delivery_toggle(app: &mut App) {
    let _ = rendered(app);
    let hit = app
        .queue_hits
        .iter()
        .find(|hit| hit.target == QueueHitTarget::ToggleTogether)
        .copied()
        .expect("queue delivery toggle was not rendered");
    let column = hit.area.x + hit.area.width.saturating_sub(1) / 2;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        hit.area.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        column,
        hit.area.y,
    ));
}

fn admission_hit(
    app: &mut App,
    admission: caudra_agent::PromptAdmission,
) -> crate::components::input::AdmissionHit {
    let _ = rendered(app);
    app.admission_hits
        .iter()
        .find(|hit| hit.admission == admission)
        .copied()
        .expect("admission control was not rendered")
}

fn click_admission(app: &mut App, admission: caudra_agent::PromptAdmission) -> Vec<Action> {
    let hit = admission_hit(app, admission);
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        hit.area.x,
        hit.area.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        hit.area.x,
        hit.area.y,
    ))
}

const HINT_MISSING: &str = "the composer drew no chord hint";
const WRONG_HINT: &str = "another hint took the composer's top row";

/// The hint only reaches the composer's top row once nothing louder wants it,
/// so every chord-hint test renders first and aims at where the frame put it.
fn chord_hint(app: &mut App, target: ChordHint) -> Rect {
    app.status = Status::Idle;
    let _ = rendered(app);
    let hit = app.chord_hint_hit.expect(HINT_MISSING);
    assert_eq!(hit.target, target, "{WRONG_HINT}");
    assert!(hit.area.width > 0, "{HINT_MISSING}");
    hit.area
}

fn click_at(app: &mut App, hit: Rect) -> Vec<Action> {
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        hit.x,
        hit.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        hit.x,
        hit.y,
    ))
}

fn app_with_todos() -> App {
    let mut app = test_app();
    app.todo_panel.set_items(vec![TodoItem {
        content: "write the test".into(),
        status: TodoStatus::Pending,
        priority: TodoPriority::default(),
    }]);
    app
}

#[test]
fn clicking_the_task_hint_opens_the_task_picker() {
    let mut app = app_with_subagent();
    let hit = chord_hint(&mut app, ChordHint::Tasks);

    assert!(click_at(&mut app, hit).is_empty());

    assert!(app.task_picker.is_open());
}

#[test]
fn releasing_off_the_task_hint_leaves_the_picker_shut() {
    let mut app = app_with_subagent();
    let hit = chord_hint(&mut app, ChordHint::Tasks);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        hit.x,
        hit.y,
    ));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 0, 0));

    assert!(!app.task_picker.is_open());
}

#[test]
fn clicking_the_todo_hint_opens_the_panel() {
    let mut app = app_with_todos();
    let hit = chord_hint(&mut app, ChordHint::PlanOrTodo);

    assert!(click_at(&mut app, hit).is_empty());

    assert!(app.todo_panel.is_visible());
}

#[test]
fn releasing_off_the_todo_hint_leaves_the_panel_closed() {
    let mut app = app_with_todos();
    let hit = chord_hint(&mut app, ChordHint::PlanOrTodo);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        hit.x,
        hit.y,
    ));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 0, 0));

    assert!(!app.todo_panel.is_visible());
}

/// A dismissed plan is reachable from the same row the todo count uses, and a
/// click on it means what the chord means.
#[test]
fn clicking_the_plan_hint_reopens_the_dismissed_form() {
    let mut app = plan_app();
    app.plan_form.hide();
    let hit = chord_hint(&mut app, ChordHint::PlanOrTodo);

    assert!(click_at(&mut app, hit).is_empty());

    assert!(app.plan_form.is_visible());
}

#[test_case(ChordHint::Tasks, app_with_subagent ; "tasks")]
#[test_case(ChordHint::PlanOrTodo, app_with_todos ; "todos")]
fn chord_hint_hover_excludes_the_padding_around_it(target: ChordHint, build: fn() -> App) {
    let mut app = build();
    let hit = chord_hint(&mut app, target);

    app.update(mouse_event(MouseEventKind::Moved, hit.x, hit.y));
    assert_eq!(app.chord_hint_hover, Some(target));

    app.update(mouse_event(
        MouseEventKind::Moved,
        hit.x.saturating_sub(1),
        hit.y,
    ));
    assert_eq!(app.chord_hint_hover, None);

    app.update(mouse_event(MouseEventKind::Moved, hit.right(), hit.y));
    assert_eq!(app.chord_hint_hover, None);
}

/// Streaming hands the same row to the admission controls, so the stale hit
/// has to go with the hint that no longer renders.
#[test]
fn the_chord_hint_stops_taking_clicks_once_streaming_takes_the_row() {
    let mut app = app_with_subagent();
    let _ = chord_hint(&mut app, ChordHint::Tasks);

    app.status = Status::Streaming;
    let _ = rendered(&mut app);

    assert!(app.chord_hint_hit.is_none());
}

/// The row holds one control at a time, so a press taken against one hint can
/// never press the chord of whichever hint replaced it before the release.
#[test]
fn a_press_never_fires_the_hint_that_replaced_the_one_it_landed_on() {
    const FIRED_ANYWAY: &str = "a press outlived the hint it was taken against";
    let mut app = app_with_subagent();
    let pressed = chord_hint(&mut app, ChordHint::Tasks);
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        pressed.x,
        pressed.y,
    ));

    app.todo_panel.set_items(vec![TodoItem {
        content: "take the row".into(),
        status: TodoStatus::Pending,
        priority: TodoPriority::default(),
    }]);
    let hit = chord_hint(&mut app, ChordHint::PlanOrTodo);
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        hit.x,
        hit.y,
    ));

    assert!(!app.task_picker.is_open(), "{FIRED_ANYWAY}");
    assert!(!app.todo_panel.is_visible(), "{FIRED_ANYWAY}");
}

pub(crate) fn status_hit(
    app: &mut App,
    target: StatusBarHitTarget,
) -> crate::components::status_bar::StatusBarHit {
    let _ = rendered(app);
    app.status_hits
        .iter()
        .find(|hit| hit.target == target)
        .copied()
        .expect("status control was not rendered")
}

/// The bar spells the chip `[thinking: xhigh]` or just `[xhigh]` depending on
/// how many columns it has, so tests read the control's own glyphs instead of
/// asserting on one of the two spellings.
fn thinking_chip(app: &mut App) -> String {
    let hit = status_hit(app, StatusBarHitTarget::Thinking);
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.view(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (hit.area.x..hit.area.right())
        .filter_map(|column| buffer.cell((column, hit.area.y)))
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

pub(crate) fn click_status(app: &mut App, target: StatusBarHitTarget) -> Vec<Action> {
    let hit = status_hit(app, target);
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        hit.area.x,
        hit.area.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        hit.area.x,
        hit.area.y,
    ))
}

#[test]
fn clicking_status_mode_toggles_build_and_plan() {
    let mut app = test_app();

    assert!(click_status(&mut app, StatusBarHitTarget::Mode).is_empty());
    assert_eq!(app.state.mode, Mode::Build);
    assert!(click_status(&mut app, StatusBarHitTarget::Mode).is_empty());
    assert_eq!(app.state.mode, Mode::Plan);
}

#[test]
fn clicking_status_model_opens_picker_and_refreshes() {
    let mut app = test_app();

    let actions = click_status(&mut app, StatusBarHitTarget::Model);

    assert!(app.model_picker.is_open());
    assert!(matches!(&actions[..], [Action::RefreshModels]));
}

#[test]
fn opening_model_picker_clears_footer_hover() {
    let mut app = test_app();
    let hit = status_hit(&mut app, StatusBarHitTarget::Model);
    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
    assert_eq!(app.status_hover, Some(StatusBarHitTarget::Model));

    click_status(&mut app, StatusBarHitTarget::Model);

    assert!(app.model_picker.is_open());
    assert_eq!(app.status_hover, None);
}

/// The label is drawn only while the transcript has stopped following, so the
/// click that resumes it also takes the control away.
#[test]
fn clicking_the_paused_footer_resumes_auto_scroll() {
    let mut app = test_app();
    fill_transcript(&mut app);
    let _ = rendered(&mut app);
    let bottom = app.active_chat().scroll_top();
    assert!(bottom > 0, "{RESUME_NEEDS_SCROLLBACK}");

    app.update(Msg::Key(kb::SCROLL_TOP.to_key_event()));
    assert!(!app.chats[0].auto_scroll());
    let hit = status_hit(&mut app, StatusBarHitTarget::ResumeAutoScroll);
    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
    assert_eq!(app.status_hover, Some(StatusBarHitTarget::ResumeAutoScroll));

    assert!(click_status(&mut app, StatusBarHitTarget::ResumeAutoScroll).is_empty());

    assert!(app.chats[0].auto_scroll());
    assert_eq!(app.status_hover, None);
    let _ = rendered(&mut app);
    assert_eq!(
        app.active_chat().scroll_top(),
        bottom,
        "{RESUME_PINS_BOTTOM}"
    );
    assert!(
        app.status_hits
            .iter()
            .all(|hit| hit.target != StatusBarHitTarget::ResumeAutoScroll)
    );
}

#[test]
fn a_resumed_transcript_follows_new_output() {
    let mut app = test_app();
    fill_transcript(&mut app);
    app.update(Msg::Key(kb::SCROLL_TOP.to_key_event()));
    click_status(&mut app, StatusBarHitTarget::ResumeAutoScroll);
    let _ = rendered(&mut app);
    let bottom = app.active_chat().scroll_top();

    app.active_chat()
        .push(DisplayMessage::new(DisplayRole::User, "one more".into()));
    let _ = rendered(&mut app);

    assert!(app.active_chat().scroll_top() > bottom);
}

/// A task transcript pauses on its own, so resuming it must leave the main
/// chat waiting behind it exactly as it was.
#[test]
fn a_subagent_footer_resumes_only_its_own_transcript() {
    let mut app = read_only_task_app();
    fill_transcript(&mut app);
    app.update(Msg::Key(kb::SCROLL_TOP.to_key_event()));
    assert!(!app.chats[app.active_chat].auto_scroll());
    let main_scroll = app.chats[0].scroll_top();

    click_status(&mut app, StatusBarHitTarget::ResumeAutoScroll);

    assert!(app.chats[app.active_chat].auto_scroll());
    assert_eq!(app.chats[0].scroll_top(), main_scroll);
}

/// A press that leaves the label before it is released is not a click, so the
/// transcript stays where the reader put it.
#[test]
fn releasing_off_the_resume_control_leaves_the_transcript_paused() {
    let mut app = test_app();
    fill_transcript(&mut app);
    app.update(Msg::Key(kb::SCROLL_TOP.to_key_event()));
    let hit = status_hit(&mut app, StatusBarHitTarget::ResumeAutoScroll);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        hit.area.x,
        hit.area.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        hit.area.right(),
        hit.area.y,
    ));

    assert!(!app.chats[0].auto_scroll());
}

fn bypassing_app() -> App {
    let app = test_app();
    app.permissions.set_session_mode(Some(PermissionMode::Yolo));
    app
}

/// The chip warns that prompts are being skipped, so clicking it is the way
/// back to them and the warning goes with the state it described.
#[test]
fn clicking_the_yolo_chip_turns_it_off() {
    let mut app = bypassing_app();
    let hit = status_hit(&mut app, StatusBarHitTarget::Yolo);
    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
    assert_eq!(app.status_hover, Some(StatusBarHitTarget::Yolo));

    assert!(click_status(&mut app, StatusBarHitTarget::Yolo).is_empty());

    assert!(!app.permissions.is_yolo());
    assert_eq!(app.status_hover, None);
    assert_eq!(app.status_bar.flash_text(), Some(YOLO_OFF_MSG));
    let _ = rendered(&mut app);
    assert!(
        app.status_hits
            .iter()
            .all(|hit| hit.target != StatusBarHitTarget::Yolo)
    );
}

/// The click is the same explicit choice `/yolo` makes, so a resume must not
/// hand the session its bypass back.
#[test]
fn turning_yolo_off_from_the_footer_is_remembered() {
    let mut app = bypassing_app();

    click_status(&mut app, StatusBarHitTarget::Yolo);
    app.checkpoint();

    assert_eq!(
        app.state.session.meta.permission_mode,
        Some(PermissionMode::Ask)
    );
}

/// Permissions are the session's, so a task footer switches off the bypass the
/// whole session was running under.
#[test]
fn a_subagent_footer_turns_yolo_off() {
    let mut app = read_only_task_app();
    app.permissions.set_session_mode(Some(PermissionMode::Yolo));

    click_status(&mut app, StatusBarHitTarget::Yolo);

    assert!(!app.permissions.is_yolo());
}

#[test_case(false; "main_chat")]
#[test_case(true; "subagent_chat")]
fn auto_footer_returns_to_ask_and_remembers_it(subagent: bool) {
    let mut app = if subagent {
        read_only_task_app()
    } else {
        test_app()
    };
    app.permissions.set_seed_mode(PermissionMode::Auto);
    let hit = status_hit(&mut app, StatusBarHitTarget::Auto);
    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
    assert_eq!(app.status_hover, Some(StatusBarHitTarget::Auto));

    assert!(click_status(&mut app, StatusBarHitTarget::Auto).is_empty());

    assert_eq!(app.permissions.mode(), PermissionMode::Ask);
    assert_eq!(app.status_hover, None);
    assert_eq!(app.status_bar.flash_text(), Some(AUTO_OFF_MSG));
    app.checkpoint();
    assert_eq!(
        app.state.session.meta.permission_mode,
        Some(PermissionMode::Ask)
    );
    let _ = rendered(&mut app);
    assert!(app.status_hits.iter().all(|hit| !matches!(
        hit.target,
        StatusBarHitTarget::Auto | StatusBarHitTarget::Yolo
    )));
}

/// A session bound to a sandbox, `connected` saying whether this runtime's own
/// authenticated connection is up. Both halves are the ones the sandbox manager
/// reads, so the footer cannot call a session attached on weaker evidence.
fn sandboxed_app(connected: bool) -> App {
    let mut app = test_app();
    let workspace = remote_workspace_session();
    let binding = StoredWorkspaceBinding::new_with_cursor(
        workspace.binding().clone(),
        workspace.cursor().clone(),
        None,
    )
    .unwrap()
    .with_sandbox_record(CaudraId::generate())
    .unwrap();
    app.state.session = Arc::new(AppSession::new_with_workspace("test", ".", binding));
    app.workspace_session = Some(workspace);
    app.sandbox_live.readiness = Some(Arc::new(move || connected));
    app
}

/// What `caudra --sandbox NAME` hands the runtime: a session attached to the
/// named instance, with nothing having opened `/sandbox`.
fn attached_at_startup() -> App {
    let mut app = sandboxed_app(true);
    app.sandbox_live.name = Some(SandboxName::parse(SANDBOX_INSTANCE_NAME).unwrap());
    app
}

fn footer_sandbox(app: &App) -> Option<&str> {
    attached_sandbox_instance(&app.state.session, &app.sandbox_live)
}

/// Both halves of the manager's own readiness have to hold: a sandbox that is
/// merely running, with no authenticated connection to this runtime, is not one
/// the footer may call attached.
#[test_case(false, false ; "no_sandbox_binding")]
#[test_case(true,  false ; "sandbox_running_but_not_connected")]
#[test_case(true,  true  ; "attached")]
fn the_footer_names_a_sandbox_only_once_it_is_attached(bound: bool, connected: bool) {
    let mut app = if bound {
        sandboxed_app(connected)
    } else {
        test_app()
    };

    let _ = rendered(&mut app);

    assert_eq!(
        app.status_hits
            .iter()
            .any(|hit| hit.target == StatusBarHitTarget::Sandbox),
        bound && connected
    );
    assert_eq!(footer_sandbox(&app).is_some(), bound && connected);
}

/// The reported bug. `caudra --sandbox prowlix-instance --sandbox-resume`
/// attaches before anything opens `/sandbox`, and the footer still has to name
/// the instance rather than the opaque server id the session is bound by.
#[test]
fn a_session_attached_at_startup_names_its_sandbox_without_opening_the_manager() {
    let mut app = attached_at_startup();

    let frame = rendered(&mut app);

    assert!(!app.sandbox_manager.allocated(), "{MANAGER_LOADED_MSG}");
    assert_eq!(footer_sandbox(&app), Some(SANDBOX_INSTANCE_NAME));
    assert!(
        frame.contains(&format!("[sandbox: {SANDBOX_INSTANCE_NAME}]")),
        "{UNNAMED_SANDBOX_MSG}"
    );
}

/// The name is local: it came from the selection that built the runtime. The
/// live instance state is a daemon query, and naming the footer must not start
/// one, so the snapshot poll stays gated on the manager being open.
#[test]
fn naming_the_sandbox_never_queries_the_daemon() {
    let mut app = attached_at_startup();

    let frame = rendered(&mut app);

    assert_eq!(app.poll_sandbox(), Dirty::NO);
    assert!(
        frame.contains(SANDBOX_INSTANCE_NAME),
        "{UNNAMED_SANDBOX_MSG}"
    );
    assert!(!app.sandbox_manager.is_open());
    assert!(app.sandbox_live.snapshot.is_none(), "{DAEMON_POLLED_MSG}");
    assert!(app.sandbox_live.scope.is_none(), "{DAEMON_POLLED_MSG}");
}

/// A runtime handed no name is still attached, so the footer says so with the
/// id it does have. Dropping the chip would claim a detachment that is not
/// there.
#[test]
fn a_runtime_without_a_name_falls_back_to_the_instance_id() {
    let app = sandboxed_app(true);
    let expected = app.state.session.workspace_binding().unwrap().server_id();

    assert_eq!(footer_sandbox(&app), Some(expected));
}

/// The chip opens what `/sandbox` opens, so there is one way into the manager
/// and the keyboard reaches it without the footer claiming a global key.
#[test]
fn clicking_the_sandbox_chip_opens_the_manager() {
    let mut app = sandboxed_app(true);

    assert!(click_status(&mut app, StatusBarHitTarget::Sandbox).is_empty());

    assert!(app.sandbox_manager.is_open());
    assert_eq!(app.status_hover, None);
}

/// The pointer passing over a control says where it is, never what to do, so a
/// move hovers the chip and leaves the manager shut.
#[test]
fn moving_over_the_sandbox_chip_hovers_without_opening_the_manager() {
    let mut app = sandboxed_app(true);
    let hit = status_hit(&mut app, StatusBarHitTarget::Sandbox);

    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));

    assert_eq!(app.status_hover, Some(StatusBarHitTarget::Sandbox));
    assert!(!app.sandbox_manager.is_open());
}

/// A press that travels off the chip before it is released is not a click, so
/// the release lands on nothing and the manager stays shut.
#[test]
fn releasing_off_the_sandbox_chip_opens_nothing() {
    let mut app = sandboxed_app(true);
    let hit = status_hit(&mut app, StatusBarHitTarget::Sandbox);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        hit.area.x,
        hit.area.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        hit.area.right(),
        hit.area.y,
    ));

    assert!(!app.sandbox_manager.is_open());
}

/// A task whose subagent resolved a level and a fast flag of its own, neither
/// of which the session is running under.
fn task_app_at_level() -> App {
    let mut app = streaming_app();
    let mut info = subagent_info(TASK_ID, RESEARCH_NAME);
    info.thinking = Some(TASK_THINKING.into());
    info.fast = true;
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta { text: "x".into() },
        info,
    ));
    app
}

/// A task resolves its level against its own model, and a profile can override
/// it, so a task footer names what that task carries rather than what the
/// session asked for.
#[test]
fn a_subagent_footer_names_the_level_its_own_task_runs_at() {
    let mut app = task_app_at_level();
    let main = thinking_chip(&mut app);
    assert!(main.contains(THINKING_OFF), "{main}");
    assert!(!rendered_wide(&mut app, FOOTER_WIDTH).contains(FAST_CHIP));

    app.focus_task(TASK_ID).unwrap();
    let footer = rendered_wide(&mut app, FOOTER_WIDTH);

    assert!(
        footer.contains(&format!("[{TASK_THINKING}]")),
        "{TASK_LEVEL_MISSING}: {footer}"
    );
    assert!(footer.contains(FAST_CHIP), "{TASK_FAST_MISSING}: {footer}");
}

/// What a task ran at is part of its record, so reopening the session leaves
/// its footer naming the same level rather than nothing.
#[test]
fn a_restored_task_keeps_the_level_it_ran_at() {
    let mut app = task_app_at_level();
    close_subagent_transcript(&mut app, TASK_ID);
    let stored = app
        .state
        .session
        .subagents()
        .iter()
        .find(|subagent| subagent.tool_use_id == TASK_ID)
        .expect(TASK_UNSTORED)
        .clone();
    assert_eq!(stored.thinking.as_deref(), Some(TASK_THINKING));
    assert!(stored.fast);

    let mut restored = test_app();
    restored.state.session = Arc::clone(&app.state.session);
    restored.restore_display();

    let chat = restored
        .chats
        .iter()
        .find(|chat| chat.task_id().is_some_and(|id| id.as_ref() == TASK_ID))
        .expect(TASK_UNRESTORED);
    assert_eq!(chat.thinking.as_deref(), Some(TASK_THINKING));
    assert!(chat.fast);
}

/// The footer draws no price until a turn has been billed, and no price means
/// no control to click.
fn priced_app() -> App {
    let mut app = test_app();
    app.chats[0].cost = MAIN_COST;
    app
}

/// The two figures on the right of the footer are the shortest form of the
/// answers `/context` and `/usage` give, so clicking one opens the view behind
/// it rather than making the reader type the command.
#[test]
fn clicking_the_context_figure_opens_the_context_modal() {
    let mut app = test_app();

    assert!(click_status(&mut app, StatusBarHitTarget::Context).is_empty());

    assert!(app.context_modal.is_open());
}

#[test]
fn clicking_the_spend_figure_opens_usage_with_its_lifetime_totals() {
    let mut app = priced_app();

    let actions = click_status(&mut app, StatusBarHitTarget::Usage);

    assert!(app.usage_modal.is_open());
    assert!(matches!(&actions[..], [Action::RefreshUsage]));
}

#[test_case(StatusBarHitTarget::Context ; "context")]
#[test_case(StatusBarHitTarget::Usage   ; "usage")]
fn opening_a_figure_clears_footer_hover(target: StatusBarHitTarget) {
    let mut app = priced_app();
    let hit = status_hit(&mut app, target);
    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
    assert_eq!(app.status_hover, Some(target));

    click_status(&mut app, target);

    assert_eq!(app.status_hover, None);
}

#[test]
fn a_clipped_chat_name_hovers_but_does_not_press() {
    let mut app = app_with_subagent();
    app.chats[0].name = "a-main-chat-name-longer-than-the-footer-can-afford".into();
    let hit = status_hit(&mut app, StatusBarHitTarget::ChatName);

    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
    assert_eq!(app.status_hover, Some(StatusBarHitTarget::ChatName));
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        hit.area.x,
        hit.area.y,
    ));

    assert!(app.status_mouse_down.is_none());
}

#[test]
fn clicking_status_thinking_opens_the_picker_on_the_visible_off_state() {
    let mut app = test_app();
    let chip = thinking_chip(&mut app);
    assert!(chip.contains(THINKING_OFF), "{chip}");

    assert!(click_status(&mut app, StatusBarHitTarget::Thinking).is_empty());

    assert!(app.thinking_picker.is_open(), "{THINKING_PICKER_SHUT}");
    assert_eq!(app.thinking_picker.selected_label(), Some(THINKING_OFF));
    assert_eq!(app.state.thinking, ThinkingConfig::Off, "{THINKING_MOVED}");
}

/// A focused task whose own turn has been billed, so the spend figure it owns
/// is drawn and can be clicked.
fn priced_subagent_app() -> App {
    let mut app = app_with_subagent();
    app.chats[1].cost = SUB_COST;
    app.focus_task(TASK_ID).unwrap();
    app
}

/// The session's model, reasoning level and goal stay on a task's bar for
/// reference, but a click on them would move state the task does not own.
#[test_case(StatusBarHitTarget::Mode  ; "mode")]
#[test_case(StatusBarHitTarget::Model ; "model")]
#[test_case(StatusBarHitTarget::Goal  ; "goal")]
fn session_controls_are_read_only_in_subagent_chat(target: StatusBarHitTarget) {
    let mut app = priced_subagent_app();
    app.state.goal.set(GOAL_CONDITION).unwrap();

    assert!(rendered(&mut app).contains(GOAL_CHIP_PREFIX));
    assert!(app.status_hits.iter().all(|hit| hit.target != target));
}

/// `/context` reports the transcript in front of you, and its snapshot is the
/// task's, so the figure that abbreviates it has to open the same view rather
/// than sending the reader back to Main to type the command.
#[test]
fn clicking_the_context_figure_in_a_task_opens_that_task_context() {
    let mut app = priced_subagent_app();
    let store = ContextStore::new();
    let main = store.publisher(ContextKey::Main);
    main.publish(current_context_snapshot(&app));
    main.for_task(TASK_ID)
        .publish(context_snapshot(TASK_CONTEXT_SPEC, CHAT_CONTEXT_WINDOW));
    app.context_store = Some(store);

    assert!(click_status(&mut app, StatusBarHitTarget::Context).is_empty());

    assert!(app.context_modal.is_open());
    assert_eq!(
        app.context_snapshot.get().unwrap().model.spec,
        TASK_CONTEXT_SPEC
    );
}

#[test]
fn clicking_the_spend_figure_in_a_task_opens_usage() {
    let mut app = priced_subagent_app();

    let actions = click_status(&mut app, StatusBarHitTarget::Usage);

    assert!(app.usage_modal.is_open());
    assert!(matches!(&actions[..], [Action::RefreshUsage]));
}

#[test]
fn clicking_the_goal_chip_opens_the_goal_modal() {
    let mut app = test_app();
    app.state.goal.set(GOAL_CONDITION).unwrap();

    assert!(click_status(&mut app, StatusBarHitTarget::Goal).is_empty());

    assert!(app.goal_modal.is_open());
}

#[test_case(false, false; "main")]
#[test_case(true, false; "child")]
#[test_case(false, true; "stale_main")]
#[test_case(true, true; "stale_child")]
fn activity_task_chip_opens_picker_and_clears_hover_in_any_chat(child: bool, stale: bool) {
    let mut app = app_with_subagent();
    if child {
        app.focus_task(TASK_ID).unwrap();
    }
    let active_chat = app.active_chat;
    let hit = status_hit(&mut app, StatusBarHitTarget::Tasks);
    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
    assert_eq!(app.status_hover, Some(StatusBarHitTarget::Tasks));
    if stale {
        finish_subagent(&mut app, TASK_ID, false);
    }
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        assert!(
            app.update(mouse_event(kind, hit.area.x, hit.area.y))
                .is_empty()
        );
    }
    assert!(app.task_picker.is_open());
    assert_eq!(app.active_chat, active_chat);
    assert_eq!(app.status_hover, None);
    assert!(app.task_hint_text().is_some());
}

#[test]
fn activity_task_hits_clear_on_resize_and_completion_but_composer_hint_remains() {
    let mut app = app_with_subagent();
    status_hit(&mut app, StatusBarHitTarget::Tasks);
    rendered_wide(&mut app, 1);
    assert!(
        app.status_hits
            .iter()
            .all(|hit| hit.target != StatusBarHitTarget::Tasks)
    );
    status_hit(&mut app, StatusBarHitTarget::Tasks);
    finish_subagent(&mut app, TASK_ID, false);
    assert_eq!(app.task_activity().agents, 0);
    rendered(&mut app);
    assert!(
        app.status_hits
            .iter()
            .all(|hit| hit.target != StatusBarHitTarget::Tasks)
    );
    assert!(app.task_hint_text().is_some());
    app.tasks_browse();
    assert!(app.task_picker.is_open());
}

#[test]
fn goal_modal_adjusts_the_session_continuation_limit() {
    let mut app = test_app();
    let initial = app.state.goal.continuation_limit();
    app.goal_modal.open();

    app.update(Msg::Key(key(KeyCode::Right)));

    assert_eq!(app.state.goal.continuation_limit(), initial + 1);
}

#[test]
fn opening_the_goal_modal_clears_footer_hover() {
    let mut app = test_app();
    app.state.goal.set(GOAL_CONDITION).unwrap();
    let hit = status_hit(&mut app, StatusBarHitTarget::Goal);
    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
    assert_eq!(app.status_hover, Some(StatusBarHitTarget::Goal));

    click_status(&mut app, StatusBarHitTarget::Goal);

    assert!(app.goal_modal.is_open());
    assert_eq!(app.status_hover, None);
}

/// A model that cannot stop reasoning draws `minimal` while the setting still
/// says `off`, so the picker has to open on what the chip shows rather than on
/// a row it does not even offer.
#[test]
fn required_thinking_click_opens_on_the_effective_minimal() {
    let mut app = test_app();
    app.state.model.thinking_override = Some(caudra_providers::ThinkingSupport::Required);
    app.state.thinking = ThinkingConfig::Off;
    let chip = thinking_chip(&mut app);
    assert!(chip.contains(THINKING_MINIMAL), "{chip}");

    click_status(&mut app, StatusBarHitTarget::Thinking);

    assert!(app.thinking_picker.is_open(), "{THINKING_PICKER_SHUT}");
    assert_eq!(app.thinking_picker.selected_label(), Some(THINKING_MINIMAL));
}

#[test]
fn hovering_status_control_tracks_highlight_target() {
    let mut app = test_app();
    let hit = status_hit(&mut app, StatusBarHitTarget::Model);

    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
    assert_eq!(app.status_hover, Some(StatusBarHitTarget::Model));

    app.update(mouse_event(MouseEventKind::Moved, 0, hit.area.y));
    assert_eq!(app.status_hover, None);
}

#[test_case(caudra_agent::PromptAdmission::Queue ; "next")]
#[test_case(caudra_agent::PromptAdmission::Steer ; "guide")]
fn clicking_streaming_admission_submits_with_selected_delivery(
    admission: caudra_agent::PromptAdmission,
) {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    app.input_box.set_input("follow up".into());

    assert!(click_admission(&mut app, admission).is_empty());

    assert_eq!(app.queue.pending_prompts().len(), 1);
    assert_eq!(app.queue.pending_prompts()[0].admission, admission);
    assert_eq!(app.queue.pending_prompts()[0].text, "follow up");
}

#[test]
fn clicking_replace_admission_cancels_active_run() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    let (sender, receiver) = shared_queue::queue();
    app.queue.set_shared(sender);
    receiver.set_active_run(app.run_id);
    app.input_box.set_input("replace by mouse".into());

    let actions = click_admission(&mut app, caudra_agent::PromptAdmission::Interrupt);

    assert!(matches!(actions.as_slice(), [Action::CancelAgent { .. }]));
    assert_eq!(
        app.queue.pending_prompts()[0].admission,
        caudra_agent::PromptAdmission::Interrupt
    );
}

#[test]
fn admission_hover_excludes_leading_separator() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    let hit = admission_hit(&mut app, caudra_agent::PromptAdmission::Queue);

    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
    assert_eq!(
        app.admission_hover,
        Some(caudra_agent::PromptAdmission::Queue)
    );

    app.update(mouse_event(
        MouseEventKind::Moved,
        hit.area.x.saturating_sub(1),
        hit.area.y,
    ));
    assert_eq!(app.admission_hover, None);
}

#[test]
fn admission_requires_matching_press_and_release() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    app.input_box.set_input("keep me".into());
    let pressed = admission_hit(&mut app, caudra_agent::PromptAdmission::Queue);
    let released = admission_hit(&mut app, caudra_agent::PromptAdmission::Steer);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        pressed.area.x,
        pressed.area.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        released.area.x,
        released.area.y,
    ));

    assert!(app.queue.pending_prompts().is_empty());
    assert_eq!(app.input_box.buffer.value(), "keep me");
}

#[test]
fn queue_edit_hides_streaming_admission_controls() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    app.queue_and_notify(queued_msg("queued"));
    let id = app.queue.panel_entries()[0].id;
    app.begin_queue_edit(id);

    let _ = rendered(&mut app);

    assert!(app.admission_hits.is_empty());
}

#[test]
fn bash_status_has_no_mode_toggle() {
    let mut app = test_app();
    app.input_box.set_input("! ls".into());
    let _ = rendered(&mut app);

    assert!(
        app.status_hits
            .iter()
            .all(|hit| hit.target != StatusBarHitTarget::Mode)
    );
}

#[test]
fn cached_mode_hit_revalidates_bash_state() {
    let mut app = test_app();
    let hit = status_hit(&mut app, StatusBarHitTarget::Mode);
    app.input_box.set_input("! ls".into());

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        hit.area.x,
        hit.area.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        hit.area.x,
        hit.area.y,
    ));

    assert_eq!(app.state.mode, Mode::Plan);
}

/// When the picker gives up on a directory it cannot list, the flash is the
/// only trace the user gets. Forwarding it moved from `view` into `tick`, and
/// dropping that hop closes the picker with no explanation at all. The loop
/// ends the moment the walker thread answers; the deadline only turns a
/// missing hop into a failure instead of a hang.
#[test]
fn tick_forwards_the_file_picker_flash_to_the_status_bar() {
    let tmp = TempDir::new().unwrap();
    let mut app = test_app();
    app.file_picker
        .open(&tmp.path().join(MISSING_DIR).to_string_lossy());

    let deadline = Instant::now() + WALK_TIMEOUT;
    while app.status_bar.flash_text().is_none() {
        assert!(Instant::now() < deadline, "the picker never flashed");
        let _ = app.tick();
        std::thread::yield_now();
    }

    assert_eq!(app.status_bar.flash_text(), Some(UNREADABLE_DIR_MSG));
    assert!(!app.file_picker.is_open());
}

/// A waiting tool draws a spinner, which changes once per `SPINNER_FRAME`.
/// Claiming `SMOOTH` here paints five identical frames for every visible one,
/// for as long as the tool runs.
#[test]
fn waiting_tool_animates_at_the_spinner_rate() {
    let mut app = app_without_splash();
    app.update(agent_msg(tool_start("t1", "bash")));

    assert_eq!(app.cadence(), Cadence::SPINNER);
}

/// The bar spins for a whole streaming turn and again while a restore is in
/// flight. The old `is_animating` only knew about the restore, so a streaming
/// turn froze mid flight. The countdown used to be a third case here and is
/// now the chat's, which
/// [`a_retry_countdown_reaches_app_cadence_through_the_chat_that_owns_it`]
/// covers.
#[test_case(Status::Streaming, false => Cadence::SPINNER ; "streaming_turn")]
#[test_case(Status::Idle, true => Cadence::SPINNER ; "restoring_session")]
#[test_case(Status::Idle, false => Cadence::IDLE ; "nothing_in_flight")]
fn status_bar_motion_reaches_app_cadence(status: Status, restoring: bool) -> Cadence {
    let mut app = app_without_splash();
    app.status = status;
    app.restoring.store(restoring, Ordering::Relaxed);
    app.cadence()
}

fn retry_info() -> RetryInfo {
    RetryInfo {
        attempt: RETRY_ATTEMPT,
        message: RETRY_MESSAGE.into(),
        deadline: Instant::now() + RETRY_DELAY,
    }
}

/// A countdown belongs to the chat sitting it out, not to the bar that borrows
/// it to draw. The bar is only ever handed the chat on screen, so a
/// backgrounded task's frames would be dropped if the bar still claimed them.
#[test]
fn a_retry_countdown_reaches_app_cadence_through_the_chat_that_owns_it() {
    let mut app = app_without_splash();
    assert_eq!(app.cadence(), Cadence::IDLE);

    app.chats[0].set_retry(retry_info());

    assert_eq!(app.chats[0].cadence(), Cadence::SPINNER);
    assert_eq!(
        app.status_bar.cadence(
            &app.status,
            false,
            app.state.goal.snapshot().is_some(),
            false
        ),
        Cadence::IDLE,
        "{BAR_CLAIMS_THE_COUNTDOWN}"
    );
    assert_eq!(app.cadence(), Cadence::SPINNER);
}

/// `App::cadence` asks `overlays()` as a group, so a moving overlay only
/// reaches the loop through that fold.
#[test]
fn open_overlay_motion_reaches_app_cadence() {
    let mut app = app_without_splash();
    assert_eq!(app.cadence(), Cadence::IDLE);

    let (event_tx, _event_rx) = flume::bounded::<caudra_lua::WinEvent>(8);
    let (_cmd_tx, cmd_rx) = flume::bounded::<caudra_lua::WinCommand>(8);
    app.float_mgr.open(
        Arc::new(caudra_agent::SharedBuf::new()),
        caudra_lua::FloatConfig::default(),
        true,
        event_tx,
        cmd_rx,
    );
    assert_eq!(
        app.cadence(),
        Cadence::SPINNER,
        "an open float's spinners only turn if the app keeps painting"
    );

    app.close_all_overlays();
    assert_eq!(app.cadence(), Cadence::IDLE);
}

#[test]
fn edge_scroll_makes_app_animating() {
    let mut app = app_without_splash();
    assert_eq!(app.cadence(), Cadence::IDLE);
    let zone = Rect::new(0, 2, 80, 20);
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 1));
    assert_eq!(
        app.cadence(),
        Cadence::SMOOTH,
        "an edge-scrolling drag advances on a timer, with no events to wake us"
    );
}

#[test]
fn empty_click_clears_selection() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 5, 5));
    assert!(app.selection_state.is_none());
}

#[test]
fn clicking_completed_task_in_main_chat_focuses_its_stable_chat() {
    const PARENT_TOOL_ID: &str = "outer-task-tool";
    let mut app = streaming_app();
    app.update(agent_msg(tool_start(PARENT_TOOL_ID, "task")));
    let mut info = subagent_info(PARENT_TOOL_ID, RESEARCH_NAME);
    info.task_id = TASK_ID.into();
    app.update(Msg::Agent(Box::new(Envelope {
        task: None,
        event: AgentEvent::TextDelta { text: "hi".into() },
        subagent: Some(info),
        run_id: 1,
        workflow: None,
    })));
    finish_subagent(&mut app, PARENT_TOOL_ID, false);

    let area = Rect::new(0, 0, 80, 20);
    set_zone(&mut app, SelectionZone::Messages, area);
    let backend = ratatui::backend::TestBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| app.chats[0].view(frame, area, false, false))
        .unwrap();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 0));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 5, 0));

    assert_eq!(
        app.chats[app.active_chat].task_id().map(|id| id.as_ref()),
        Some(TASK_ID)
    );
}

#[test_case(false; "live")]
#[test_case(true; "restored")]
fn clicking_task_delivery_opens_chat_named_by_typed_origin(restored: bool) {
    const DELIVERY_TEXT: &str = "Task friendly-test-wren: success.\n\nUseful delivered result.";
    const INVOCATION: &str = "private-invocation";
    const EVENT: &str = "private-event";
    let mut app = app_with_subagent();
    let origin = TaskEventOrigin {
        task_id: TASK_ID.into(),
        invocation_id: INVOCATION.into(),
        event_id: EVENT.into(),
    };
    if restored {
        let history =
            crate::history_items(&[Message::task_observation(DELIVERY_TEXT.into(), origin)]);
        let (display, _) =
            crate::chat::history_to_display(&history, &HashMap::new(), &Default::default(), false);
        app.chats[0].load_messages(display);
    } else {
        app.chats[0].handle_event(
            AgentEvent::Injected {
                text: DELIVERY_TEXT.into(),
                task_event: Some(origin),
                peer_event: None,
            },
            None,
        );
    }
    let area = Rect::new(0, 0, 60, 30);
    set_zone(&mut app, SelectionZone::Messages, area);
    let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
    terminal
        .draw(|frame| app.chats[0].view(frame, area, false, false))
        .unwrap();
    let row = (0..area.height)
        .find(|&row| {
            let text: String = (0..area.width)
                .map(|column| terminal.backend().buffer()[(column, row)].symbol())
                .collect();
            text.contains("open chat")
        })
        .unwrap();
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, row));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 5, row));
    assert_eq!(
        app.chats[app.active_chat].task_id().map(|id| id.as_ref()),
        Some(TASK_ID)
    );
}

#[test]
fn task_status_back_button_focuses_main_chat() {
    let mut app = app_with_subagent();
    app.focus_task(TASK_ID).unwrap();
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.view(frame)).unwrap();

    assert!(buffer_text(terminal.backend().buffer()).contains("[< Main] research"));
    let area = app
        .status_hits
        .iter()
        .find(|hit| hit.target == StatusBarHitTarget::BackToMain)
        .expect("task back button should be visible")
        .area;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x,
        area.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        area.x,
        area.y,
    ));

    assert_eq!(app.active_chat, 0);
}

#[test]
fn modal_blocks_task_status_back_button() {
    let mut app = app_with_subagent();
    app.focus_task(TASK_ID).unwrap();
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.view(frame)).unwrap();
    let area = app
        .status_hits
        .iter()
        .find(|hit| hit.target == StatusBarHitTarget::BackToMain)
        .expect("task back button should be visible")
        .area;
    app.help_modal.toggle();

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        area.x,
        area.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        area.x,
        area.y,
    ));

    assert_eq!(app.active_chat, 1);
}

fn make_pending_copy(app: &mut App) {
    set_zone(app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 10, 10));
}

const DRAG_ROW: u16 = 5;
const DRAG_COL: u16 = 5;
const SCROLL_LINES: i32 = 3;
const DRAG_ZONE_AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 80,
    height: 10,
};
const OTHER_ZONE_AREA: Rect = Rect {
    x: 0,
    y: DRAG_ZONE_AREA.height,
    width: 80,
    height: 10,
};

fn send_key(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('a'))));
}

fn send_scroll_outside_drag_zone(app: &mut App) {
    app.update(Msg::Scroll {
        column: DRAG_COL,
        row: OTHER_ZONE_AREA.y + 1,
        delta: SCROLL_LINES,
    });
}

#[test_case(send_key as fn(&mut App) ; "key")]
#[test_case(send_scroll_outside_drag_zone as fn(&mut App) ; "scroll_outside_drag_zone")]
fn interrupt_clears_dragging_but_preserves_pending_copy(interrupt: fn(&mut App)) {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, DRAG_ZONE_AREA);
    set_zone(&mut app, SelectionZone::Input, OTHER_ZONE_AREA);
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        DRAG_COL,
        DRAG_ROW,
    ));
    interrupt(&mut app);
    assert!(app.selection_state.is_none(), "clears dragging");

    make_pending_copy(&mut app);
    interrupt(&mut app);
    assert!(
        app.selection_state.as_ref().unwrap().is_pending_copy(),
        "preserves pending copy"
    );
}

#[test]
fn scroll_preserves_dragging_and_updates_cursor() {
    let mut app = test_app();
    for i in 0..50 {
        app.active_chat()
            .push(DisplayMessage::new(DisplayRole::User, format!("line {i}")));
    }

    let area = Rect::new(0, 0, 80, 20);
    set_zone(&mut app, SelectionZone::Messages, area);

    let backend = ratatui::backend::TestBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            app.active_chat().view(frame, area, false, false);
        })
        .unwrap();

    let max_scroll = app.active_chat().scroll_top();
    assert!(
        max_scroll > 0,
        "scroll_top should be non-zero after rendering scrollable content"
    );

    app.update(Msg::Scroll {
        column: DRAG_COL,
        row: DRAG_ROW,
        delta: SCROLL_LINES,
    });
    let scroll_before = app.active_chat().scroll_top();
    assert!(
        scroll_before < max_scroll,
        "scroll up should move scroll_top away from max_scroll"
    );

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        DRAG_COL,
        DRAG_ROW,
    ));

    app.update(Msg::Scroll {
        column: DRAG_COL,
        row: DRAG_ROW,
        delta: -SCROLL_LINES,
    });

    assert!(
        matches!(
            app.selection_state.as_ref().unwrap(),
            SelectionState::Dragging { .. }
        ),
        "scroll keeps dragging"
    );

    let (start, end) = app.selection_state.as_ref().unwrap().sel().normalized();
    let anchor_row = scroll_before + DRAG_ROW as u32;
    assert_eq!(start.row, anchor_row, "anchor keeps its doc row");
    assert_eq!(
        end.row,
        anchor_row + SCROLL_LINES as u32,
        "cursor re-projects by the scrolled lines"
    );
    assert_eq!(start.col, DRAG_COL, "anchor column is unchanged");
    assert_eq!(end.col, DRAG_COL, "cursor column is unchanged");

    make_pending_copy(&mut app);
    app.update(Msg::Scroll {
        column: DRAG_COL,
        row: DRAG_ROW,
        delta: -SCROLL_LINES,
    });
    assert!(
        app.selection_state.as_ref().unwrap().is_pending_copy(),
        "scroll preserves pending copy"
    );
}

#[test]
fn new_mouse_down_replaces_pending_copy_with_dragging() {
    let mut app = test_app();
    make_pending_copy(&mut app);

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 15, 15));
    assert!(matches!(
        app.selection_state.as_ref().unwrap(),
        SelectionState::Dragging { .. }
    ));
}

#[test]
fn pending_copy_ignores_drag_and_tick() {
    let mut app = test_app();
    make_pending_copy(&mut app);

    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 50, 50));
    assert!(app.selection_state.as_ref().unwrap().is_pending_copy());

    let _ = app.tick_edge_scroll();
    assert!(app.selection_state.as_ref().unwrap().is_pending_copy());
}

/// The bar is the fast way down a long transcript, and it has to beat the
/// selection to the press or dragging it would sweep text instead.
#[test]
fn dragging_the_transcript_bar_scrolls_and_selects_nothing() {
    let mut app = test_app();
    for index in 0..60 {
        app.main_chat().push_user_message(format!("line {index}"));
    }
    let _ = rendered(&mut app);
    let area = app
        .zones
        .find(SelectionZone::Messages)
        .expect(MISSING_ZONE)
        .area;
    let bar = area.right() - 1;
    app.active_chat().scroll_to_top();

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        bar,
        area.y + 1,
    ));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        bar,
        area.bottom() - 1,
    ));

    assert!(app.chats[0].scroll_top() > 0, "{BAR_IGNORED}");
    assert!(app.selection_state.is_none(), "{BAR_IGNORED}");
}

/// A middle press anchors, and every tick after it carries the view further
/// the further the pointer has wandered. Rows per tick, so no clock is needed.
#[test]
fn a_middle_press_scrolls_until_it_is_pressed_again() {
    let mut app = test_app();
    for index in 0..60 {
        app.main_chat().push_user_message(format!("line {index}"));
    }
    let _ = rendered(&mut app);
    let area = app
        .zones
        .find(SelectionZone::Messages)
        .expect(MISSING_ZONE)
        .area;
    app.active_chat().scroll_to_top();

    let origin = area.y + 2;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Middle),
        area.x + 1,
        origin,
    ));
    app.update(mouse_event(
        MouseEventKind::Moved,
        area.x + 1,
        area.bottom() - 1,
    ));
    for _ in 0..3 {
        assert_eq!(app.tick_autoscroll(), Dirty::YES, "{BAR_IGNORED}");
    }
    let reached = app.chats[0].scroll_top();
    assert!(reached > 0, "{BAR_IGNORED}");

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Middle),
        area.x + 1,
        origin,
    ));
    assert_eq!(app.tick_autoscroll(), Dirty::NO, "{BAR_IGNORED}");
    assert_eq!(app.chats[0].scroll_top(), reached, "{BAR_IGNORED}");
}

/// Parking the pointer on the origin has to hold the view still, or the
/// gesture would start moving the moment it was armed.
#[test]
fn autoscroll_holds_still_inside_its_dead_zone() {
    let mut app = test_app();
    for index in 0..60 {
        app.main_chat().push_user_message(format!("line {index}"));
    }
    let _ = rendered(&mut app);
    let area = app
        .zones
        .find(SelectionZone::Messages)
        .expect(MISSING_ZONE)
        .area;
    app.active_chat().scroll_to_top();

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Middle),
        area.x + 1,
        area.y + 4,
    ));

    assert_eq!(app.tick_autoscroll(), Dirty::NO, "{BAR_IGNORED}");
    assert_eq!(app.chats[0].scroll_top(), 0, "{BAR_IGNORED}");
}

#[test]
fn pending_copy_not_animating() {
    let mut app = app_without_splash();
    make_pending_copy(&mut app);
    assert_eq!(app.cadence(), Cadence::IDLE);
}

#[test]
fn edge_scroll_direction_switches_on_drag_reversal() {
    let mut app = test_app();
    let zone = Rect::new(0, 5, 80, 10);
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 8));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 4));

    if let Some(SelectionState::Dragging { edge_scroll, .. }) = &app.selection_state {
        assert!(
            edge_scroll.as_ref().unwrap().dir > 0,
            "scrolling up (positive dir)"
        );
    } else {
        panic!("expected Dragging");
    }

    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 16));
    if let Some(SelectionState::Dragging { edge_scroll, .. }) = &app.selection_state {
        assert!(
            edge_scroll.as_ref().unwrap().dir < 0,
            "scrolling down after reversal"
        );
    } else {
        panic!("expected Dragging");
    }
}

#[test]
fn drag_back_into_area_clears_edge_scroll() {
    let mut app = test_app();
    let zone = Rect::new(0, 5, 80, 10);
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 8));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 4));
    assert!(app.selection_state.as_ref().unwrap().is_edge_scrolling());

    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 10));
    assert!(
        !app.selection_state.as_ref().unwrap().is_edge_scrolling(),
        "dragging back into area must stop edge scroll"
    );
}

#[test]
fn mouse_down_outside_all_zones_ignored() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 40, 10));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 50, 15));
    assert!(
        app.selection_state.is_none(),
        "click outside zones must not create selection"
    );
}

#[test_case(true  ; "non_empty")]
#[test_case(false ; "empty")]
fn queue_command_sets_focus(has_queue: bool) {
    let mut app = if has_queue {
        app_with_queued_message()
    } else {
        test_app()
    };
    app.execute_command(cmd("/queue"), 0);
    assert_eq!(app.queue.focus().is_some(), has_queue);
}

#[test]
fn queue_boundary_clamps() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.queue.set_focus_at(0);
    app.update(Msg::Key(key(KeyCode::Up)));
    assert_eq!(app.queue.focus(), Some(0), "up at top clamps");
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    )));
    assert_eq!(app.queue.focus(), Some(0), "modified down is ignored");
    assert_eq!(app.queue.panel_entries()[0].text, "queued");
    app.queue.set_focus_at(1);
    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.queue.focus(), Some(1), "down at bottom clamps");
}

#[test]
fn shift_arrows_reorder_main_lane_and_preserve_selection_and_delivery_order() {
    let mut app = test_app();
    let (sender, receiver) = shared_queue::queue();
    app.queue.set_shared(sender);
    app.status = Status::Streaming;
    app.run_id = 1;
    assert!(app.queue_with_admission(
        queued_msg("first next"),
        caudra_agent::PromptAdmission::Queue,
    ));
    assert!(app.queue_with_admission(queued_msg("guide"), caudra_agent::PromptAdmission::Steer,));
    assert!(app.queue_with_admission(
        queued_msg("second next"),
        caudra_agent::PromptAdmission::Queue,
    ));
    let entries = app.queue.panel_entries();
    let first = entries
        .iter()
        .find(|entry| entry.text == "first next")
        .unwrap()
        .id;
    let guide = entries
        .iter()
        .find(|entry| entry.text == "guide")
        .unwrap()
        .id;
    let second = entries
        .iter()
        .find(|entry| entry.text == "second next")
        .unwrap()
        .id;
    app.queue.select(second);

    app.update(Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)));

    assert_eq!(app.queue.focus(), Some(1));
    assert_eq!(
        app.queue
            .panel_entries()
            .iter()
            .map(|entry| entry.text.as_ref())
            .collect::<Vec<_>>(),
        ["guide", "second next", "first next"]
    );
    app.update(Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT)));
    assert_eq!(app.queue.focus(), Some(2));
    app.update(Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)));
    assert_eq!(receiver.claim_idle(0)[0].0, guide);
    assert_eq!(receiver.claim_idle(0)[0].0, second);
    assert_eq!(receiver.claim_idle(0)[0].0, first);
}

#[test]
fn main_queue_movement_stops_at_lane_and_compact_boundaries() {
    let mut app = app_with_queued_message();
    app.queue_with_admission(
        queued_msg("guide before"),
        caudra_agent::PromptAdmission::Steer,
    );
    app.queue_compact();
    app.queue_and_notify(queued_msg("after compact"));
    app.queue_with_admission(
        queued_msg("guide after"),
        caudra_agent::PromptAdmission::Steer,
    );
    let entries = app.queue.panel_entries();
    let first_next = entries.iter().find(|entry| entry.text == "queued").unwrap();
    let second_next = entries
        .iter()
        .find(|entry| entry.text == "after compact")
        .unwrap();
    let guides = entries
        .iter()
        .filter(|entry| entry.admission == Some(caudra_agent::PromptAdmission::Steer))
        .collect::<Vec<_>>();

    assert!(guides.iter().all(|entry| !entry.can_move_up));
    assert!(guides.iter().all(|entry| !entry.can_move_down));
    assert!(!first_next.can_move_up);
    assert!(!first_next.can_move_down);
    assert!(!second_next.can_move_up);
    assert!(!second_next.can_move_down);
    app.queue.select(first_next.id);
    app.update(Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)));
    assert_eq!(app.queue.panel_entries()[0].text, "guide before");
}

#[test]
fn reordered_item_stays_visible_in_long_main_queue() {
    let mut app = app_with_queued_message();
    for index in 1..6 {
        app.queue_and_notify(queued_msg(&format!("queued {index}")));
    }
    let id = app.queue.panel_entries()[5].id;
    app.queue.select(id);
    assert_eq!(app.queue.viewport(), 2);

    for _ in 0..5 {
        app.update(Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)));
    }

    assert_eq!(app.queue.focus(), Some(0));
    assert_eq!(app.queue.viewport(), 0);
    assert_eq!(app.queue.panel_entries()[0].id, id);
}

#[test]
fn queue_enter_edits_selected_in_place() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.queue_editor_active());
    assert_eq!(app.input_box.buffer.value(), "queued");

    app.input_box.set_input("edited".into());
    app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(!app.queue_editor_active());
    assert_eq!(app.queue.len(), 2);
    assert_eq!(app.queue.panel_entries()[0].text, "edited");
    assert_eq!(app.queue.panel_entries()[1].text, "second");
    assert_eq!(app.queue.focus(), Some(0));
}

#[test]
fn queue_delete_key_deletes_last_and_unfocuses() {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Char('d'))));
    assert!(app.queue.is_empty());
    assert!(app.queue.focus().is_none());
}

#[test]
fn queue_esc_unfocuses_without_removing() {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.queue.focus().is_none());
    assert_eq!(app.queue.len(), 1);
}

#[test]
fn ctrl_q_pops_front() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.update(Msg::Key(kb::POP_QUEUE.to_key_event()));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.panel_entries()[0].text, "second");
    assert!(app.queue.focus().is_none(), "unfocused stays unfocused");

    app.queue_and_notify(queued_msg("third"));
    app.queue.set_focus_at(1);
    app.update(Msg::Key(kb::POP_QUEUE.to_key_event()));
    assert_eq!(
        app.queue.focus(),
        Some(0),
        "focus adjusted when item removed"
    );
}

#[test]
fn queue_batch_key_toggles_one_shot_delivery() {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Char('b'))));
    assert_eq!(
        app.queue.delivery(),
        caudra_agent::QueueDelivery::TogetherNextTurn
    );
    assert!(rendered(&mut app).contains("Mode: Together"));

    app.update(Msg::Key(key(KeyCode::Char('b'))));
    assert_eq!(app.queue.delivery(), caudra_agent::QueueDelivery::Separate);
}

#[test]
fn focused_prompt_moves_between_up_next_and_guide() {
    let mut app = app_with_queued_message();
    let id = app.queue.panel_entries()[0].id;
    app.queue.select(id);

    app.update(Msg::Key(key(KeyCode::Char('g'))));

    assert_eq!(
        app.queue.panel_entries()[0].admission,
        Some(caudra_agent::PromptAdmission::Steer)
    );
    assert!(rendered(&mut app).contains("Guide"));
    assert_eq!(app.active_queue_delivery(), None);

    app.update(Msg::Key(key(KeyCode::Char('n'))));

    assert_eq!(
        app.queue.panel_entries()[0].admission,
        Some(caudra_agent::PromptAdmission::Queue)
    );
    assert!(rendered(&mut app).contains("Up next"));
}

#[test]
fn deleting_last_next_prompt_resets_hidden_together_mode() {
    let mut app = app_with_queued_message();
    app.queue_with_admission(queued_msg("guide"), caudra_agent::PromptAdmission::Steer);
    app.queue
        .set_delivery(caudra_agent::QueueDelivery::TogetherNextTurn);
    let queued = app
        .queue
        .panel_entries()
        .into_iter()
        .find(|entry| entry.admission == Some(caudra_agent::PromptAdmission::Queue))
        .unwrap();

    assert!(app.delete_active_queue_item(queued.id));

    assert_eq!(app.queue.delivery(), caudra_agent::QueueDelivery::Separate);
    assert_eq!(app.active_queue_delivery(), None);
}

#[test]
fn queue_title_mouse_toggle_switches_delivery_mode() {
    let mut app = app_with_queued_message();

    assert!(rendered(&mut app).contains("Mode: Separate"));
    click_queue_delivery_toggle(&mut app);
    assert_eq!(
        app.queue.delivery(),
        caudra_agent::QueueDelivery::TogetherNextTurn
    );
    assert!(rendered(&mut app).contains("Mode: Together"));
    click_queue_delivery_toggle(&mut app);
    assert_eq!(app.queue.delivery(), caudra_agent::QueueDelivery::Separate);
}

#[test]
fn consumed_batch_renders_separate_grouped_user_bubbles() {
    let mut app = test_app();
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::QueueBatchConsumed {
        items: vec![
            caudra_agent::QueueConsumedItem {
                id: caudra_agent::QueueItemId::new(),
                text: "first".into(),
                image_count: 0,
            },
            caudra_agent::QueueConsumedItem {
                id: caudra_agent::QueueItemId::new(),
                text: "second".into(),
                image_count: 0,
            },
        ],
    }));

    assert_eq!(app.main_chat().message_count(), 2);
    assert_eq!(app.main_chat().message_at(0).unwrap().text, "first");
    assert_eq!(app.main_chat().message_at(1).unwrap().text, "second");
    assert_eq!(app.status, Status::Streaming);
}

#[test]
fn mouse_selects_and_edits_queue_without_losing_composer_draft() {
    let mut app = app_with_queued_message();
    app.input_box.set_input("keep this draft".into());

    click_queue_action(&mut app, QueueAction::Select);
    assert_eq!(app.queue.focus(), Some(0));

    queue_menu_action(&mut app, QueueActionKind::Edit);
    assert!(app.queue_editor_active());
    assert_eq!(app.input_box.buffer.value(), "queued");

    app.input_box.set_input("edited with mouse".into());
    app.update(Msg::Key(key(KeyCode::Enter)));

    assert_eq!(app.queue.panel_entries()[0].text, "edited with mouse");
    assert_eq!(app.input_box.buffer.value(), "keep this draft");
}

#[test]
fn mouse_delete_is_explicit_and_atomic() {
    let mut app = app_with_queued_message();

    click_queue_action(&mut app, QueueAction::Select);
    assert_eq!(app.queue.len(), 1, "selection is not destructive");
    queue_menu_action(&mut app, QueueActionKind::Delete);

    assert!(app.queue.is_empty());
    assert!(app.queue.focus().is_none());
}

#[test]
fn menu_reorders_selected_queue_item() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    click_queue_action(&mut app, QueueAction::Select);

    queue_menu_action(&mut app, QueueActionKind::MoveDown);

    assert_eq!(
        app.queue
            .panel_entries()
            .iter()
            .map(|entry| entry.text.as_ref())
            .collect::<Vec<_>>(),
        ["second", "queued"]
    );
    assert_eq!(app.queue.focus(), Some(1));
}

#[test]
fn dot_opens_the_menu_for_the_focused_queue_item() {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Char('.'))));

    assert!(app.queue_actions.is_open(), "{QUEUE_MENU_MISSING}");
    assert_eq!(
        app.queue_actions.kinds(),
        [
            QueueActionKind::Edit,
            QueueActionKind::Guide,
            QueueActionKind::Replace,
            QueueActionKind::Delete,
        ]
    );
}

#[test_case(QueueActionKind::Guide, caudra_agent::PromptAdmission::Steer ; "queued_prompt_moves_to_guide")]
#[test_case(QueueActionKind::Next, caudra_agent::PromptAdmission::Queue ; "guidance_moves_to_up_next")]
fn menu_moves_a_prompt_between_lanes(
    kind: QueueActionKind,
    expected: caudra_agent::PromptAdmission,
) {
    let mut app = app_with_queued_message();
    if kind == QueueActionKind::Next {
        let id = app.queue.panel_entries()[0].id;
        app.set_queue_admission(id, caudra_agent::PromptAdmission::Steer);
    }
    click_queue_action(&mut app, QueueAction::Select);

    queue_menu_action(&mut app, kind);

    assert_eq!(app.queue.pending_prompts()[0].admission, expected);
}

#[test]
fn menu_replace_takes_the_prompt_out_of_the_queue_and_cancels_the_run() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    let (sender, receiver) = shared_queue::queue();
    app.queue.set_shared(sender);
    receiver.set_active_run(1);
    app.queue_and_notify(queued_msg(REPLACEMENT_PROMPT));
    click_queue_action(&mut app, QueueAction::Select);

    let actions = queue_menu_action(&mut app, QueueActionKind::Replace);

    assert!(matches!(
        actions.as_slice(),
        [Action::CancelAgent { run_id: 1 }]
    ));
    assert_eq!(app.run_id, 2);
    assert_eq!(
        app.queue
            .pending_prompts()
            .into_iter()
            .map(|prompt| (prompt.text, prompt.admission))
            .collect::<Vec<_>>(),
        [(
            REPLACEMENT_PROMPT.to_owned(),
            caudra_agent::PromptAdmission::Interrupt
        )]
    );
}

#[test]
fn rejected_menu_replace_returns_the_prompt_to_its_lane() {
    let mut app = app_with_queued_message();
    app.cancelling_run = Some(1);
    click_queue_action(&mut app, QueueAction::Select);

    let actions = queue_menu_action(&mut app, QueueActionKind::Replace);

    assert!(actions.is_empty());
    assert_eq!(app.status_bar.flash_text(), Some(queue::REPLACE_BUSY_ERR));
    assert_eq!(
        app.queue
            .pending_prompts()
            .into_iter()
            .map(|prompt| (prompt.text, prompt.admission))
            .collect::<Vec<_>>(),
        [("queued".to_owned(), caudra_agent::PromptAdmission::Queue)]
    );
}

#[test]
fn r_replaces_the_focused_queue_item() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    let (sender, receiver) = shared_queue::queue();
    app.queue.set_shared(sender);
    receiver.set_active_run(1);
    app.queue_and_notify(queued_msg(REPLACEMENT_PROMPT));
    app.queue.set_focus_at(0);

    let actions = app.update(Msg::Key(key(KeyCode::Char('r'))));

    assert!(matches!(
        actions.as_slice(),
        [Action::CancelAgent { run_id: 1 }]
    ));
    assert_eq!(
        app.queue.pending_prompts()[0].admission,
        caudra_agent::PromptAdmission::Interrupt
    );
}

#[test]
fn hovering_queue_row_tracks_exact_target() {
    let mut app = app_with_queued_message();
    let hit = queue_select_hit(&mut app);

    app.update(mouse_event(
        MouseEventKind::Moved,
        queue_row_column(hit),
        hit.area.y,
    ));

    assert_eq!(app.queue_hover, Some(hit.target));
    assert_eq!(app.status_hover, None);
}

#[test]
fn dragging_cancels_pending_queue_activation_immediately() {
    let mut app = app_with_queued_message();
    let hit = queue_select_hit(&mut app);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        queue_row_column(hit),
        hit.area.y,
    ));
    assert_eq!(app.queue_mouse_down, Some(hit));

    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        queue_row_column(hit).saturating_add(1),
        hit.area.y,
    ));

    assert_eq!(app.queue_mouse_down, None);
    assert_eq!(app.queue_hover, None);
}

#[test]
fn dragging_queue_text_does_not_select_or_delete_item() {
    let mut app = app_with_queued_message();
    let hit = queue_select_hit(&mut app);
    let row = hit.area.y;
    let start = queue_row_column(hit);
    let end = (start + 3).min(hit.area.right().saturating_sub(1));

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        start,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        end,
        row,
    ));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), end, row));

    assert!(app.selection_state.is_some());
    assert!(app.queue.focus().is_none());
    assert_eq!(app.queue.len(), 1);
}

#[test]
fn wheel_over_long_queue_scrolls_its_bounded_viewport() {
    let mut app = app_with_queued_message();
    for index in 1..6 {
        app.queue_and_notify(queued_msg(&format!("queued {index}")));
    }
    let hit = queue_select_hit(&mut app);

    app.update(Msg::Scroll {
        column: hit.area.x,
        row: hit.area.y,
        delta: -3,
    });

    assert_eq!(app.queue.viewport(), 2);
    assert_eq!(
        crate::components::queue_panel::height(&app.queue.panel_entries()),
        QUEUE_PANEL_MAX_HEIGHT
    );
}

#[test]
fn subagent_queue_edit_updates_the_real_interrupt_queue() {
    const STEER: &str = "inspect auth";
    let mut app = test_app();
    app.run_id = 1;
    let (steer_queue, _receiver) = caudra_agent::steering_queue();
    let mut info = subagent_info(TASK_ID, RESEARCH_NAME);
    info.steer_tx = Some(steer_queue.clone());
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta {
            text: "working".into(),
        },
        info,
    ));
    app.active_chat = 1;
    app.sync_subagent_input_target();
    app.subagent_input_box.set_input(STEER.into());
    app.update(Msg::Key(key(KeyCode::Enter)));

    app.focus_active_queue();
    app.update(Msg::Key(key(KeyCode::Enter)));
    app.subagent_input_box
        .set_input("inspect session auth".into());
    app.update(Msg::Key(key(KeyCode::Enter)));

    assert_eq!(steer_queue.entries()[0].text, "inspect session auth");
    assert_eq!(
        app.pending_subagent_steers[TASK_ID][0].text,
        "inspect session auth"
    );

    app.update(Msg::Key(key(KeyCode::Char('d'))));
    assert!(steer_queue.entries().is_empty());
    assert!(!app.pending_subagent_steers.contains_key(TASK_ID));
}

#[test]
fn live_subagent_reorder_updates_real_queue_and_ui_mirror() {
    let mut app = test_app();
    app.run_id = 1;
    let (steer_queue, _receiver) = caudra_agent::steering_queue();
    let mut info = subagent_info(TASK_ID, RESEARCH_NAME);
    info.steer_tx = Some(steer_queue.clone());
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta {
            text: "working".into(),
        },
        info,
    ));
    app.active_chat = 1;
    app.sync_subagent_input_target();
    for text in ["first", "second"] {
        app.subagent_input_box.set_input(text.into());
        app.update(Msg::Key(key(KeyCode::Enter)));
    }
    app.focus_active_queue();
    app.move_active_queue_focus(1);

    app.update(Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)));

    assert_eq!(app.active_queue_focus(), Some(0));
    assert_eq!(
        steer_queue
            .entries()
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["second", "first"]
    );
    assert_eq!(
        app.pending_subagent_steers[TASK_ID]
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        ["second", "first"]
    );
    let claimed = steer_queue.remove(steer_queue.entries()[0].id).unwrap();
    assert_eq!(claimed.message, "second");
    app.update(Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT)));
    assert_eq!(app.active_queue_focus(), Some(0));
    assert_eq!(app.pending_subagent_steers[TASK_ID][0].text, "second");
}

#[test]
fn subagent_together_delivery_is_scoped_to_that_task() {
    let mut app = test_app();
    app.run_id = 1;
    let (steer_queue, _receiver) = caudra_agent::steering_queue();
    let mut info = subagent_info(TASK_ID, RESEARCH_NAME);
    info.steer_tx = Some(steer_queue.clone());
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta {
            text: "working".into(),
        },
        info,
    ));
    app.active_chat = 1;
    app.sync_subagent_input_target();
    app.subagent_input_box.set_input("first".into());
    app.update(Msg::Key(key(KeyCode::Enter)));
    app.focus_active_queue();

    app.update(Msg::Key(key(KeyCode::Char('b'))));

    assert_eq!(
        steer_queue.delivery(),
        caudra_agent::QueueDelivery::TogetherNextTurn
    );
    assert_eq!(app.queue.delivery(), caudra_agent::QueueDelivery::Separate);
}

#[test]
fn finished_task_mouse_action_moves_unsent_item_to_main() {
    const STEER: &str = "report the failure";
    let mut app = test_app();
    app.run_id = 1;
    let (steer_queue, _receiver) = caudra_agent::steering_queue();
    let mut info = subagent_info(TASK_ID, RESEARCH_NAME);
    info.steer_tx = Some(steer_queue);
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta {
            text: "working".into(),
        },
        info,
    ));
    app.active_chat = 1;
    app.sync_subagent_input_target();
    app.subagent_input_box.set_input(STEER.into());
    app.update(Msg::Key(key(KeyCode::Enter)));
    close_subagent_transcript(&mut app, TASK_ID);

    click_queue_action(&mut app, QueueAction::Select);
    queue_menu_action(&mut app, QueueActionKind::MoveMain);

    assert!(!app.unsent_subagent_steers.contains_key(TASK_ID));
    assert_eq!(app.queue.panel_entries()[0].text, STEER);
}

#[test]
fn finished_task_reorders_unsent_items() {
    let mut app = test_app();
    app.run_id = 1;
    let (steer_queue, _receiver) = caudra_agent::steering_queue();
    let mut info = subagent_info(TASK_ID, RESEARCH_NAME);
    info.steer_tx = Some(steer_queue);
    app.update(subagent_msg_with_info(
        AgentEvent::TextDelta {
            text: "working".into(),
        },
        info,
    ));
    app.active_chat = 1;
    app.sync_subagent_input_target();
    for text in ["first", "second"] {
        app.subagent_input_box.set_input(text.into());
        app.update(Msg::Key(key(KeyCode::Enter)));
    }
    close_subagent_transcript(&mut app, TASK_ID);
    app.focus_active_queue();
    app.move_active_queue_focus(1);

    app.update(Msg::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)));

    assert_eq!(app.active_queue_focus(), Some(0));
    assert_eq!(
        app.unsent_subagent_steers[TASK_ID]
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        ["second", "first"]
    );
    app.checkpoint();
    let stored = app.state.session.meta.unsent_subagent_messages.clone();
    let mut restored = test_app();
    restored.state.session_mut().meta.unsent_subagent_messages = stored;
    restored.restore_display();
    assert_eq!(
        restored.unsent_subagent_steers[TASK_ID]
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        ["second", "first"]
    );
}

#[test_case(cancel_app as fn(&mut App) ; "cancel")]
#[test_case(error_app as fn(&mut App)  ; "error")]
fn clears_queue_focus_on_terminate(terminate: fn(&mut App)) {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);
    terminate(&mut app);
    assert!(app.queue.focus().is_none());
}

#[test]
fn stale_events_ignored_after_run_id_increment() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let receiver = attach_processing_queue(&mut app);

    cancel_app(&mut app);
    let current_run = app.run_id;
    receiver.clear_active_run();
    smol::block_on(app.flush_background_delivery(false)).unwrap();
    app.update(agent_msg_with_run_id(
        AgentEvent::Done {
            usage: TokenUsage::default(),
            num_turns: 1,
            reason: DoneReason::Cancelled,
        },
        1,
    ));
    let actions = type_and_submit(&mut app, "new prompt");
    assert!(matches!(&actions[0], Action::SendMessage(i) if i.message == "new prompt"));
    let active_run = app.run_id;

    app.update(agent_msg_with_run_id(
        AgentEvent::TextDelta {
            text: "stale text".into(),
        },
        current_run,
    ));
    assert_eq!(app.chats[0].last_message_text(), "new prompt");

    app.update(agent_msg_with_run_id(
        AgentEvent::TextDelta {
            text: "new text".into(),
        },
        active_run,
    ));
    app.chats[0].flush();
    assert_eq!(app.chats[0].last_message_text(), "new text");
}

#[test]
fn stale_done_does_not_drain_queue() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let _receiver = attach_processing_queue(&mut app);

    cancel_app(&mut app);
    app.queue_and_notify(queued_msg("next"));

    app.update(agent_msg_with_run_id(done(), 1));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.status, Status::Streaming);
    assert_eq!(app.cancelling_run, None);
}

#[test]
fn mouse_down_in_input_creates_input_zone_selection() {
    let mut app = test_app();
    let input = Rect::new(0, 15, 80, 5);
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 15));
    set_zone(&mut app, SelectionZone::Input, input);

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 16));
    let state = app.selection_state.as_ref().unwrap();
    assert_eq!(state.sel().zone, SelectionZone::Input);
    assert_eq!(state.sel().area, input);
}

#[test]
fn resolve_or_create_chat_sets_model_id_and_annotation() {
    const MODEL: &str = "anthropic/claude-sonnet-4-20250514";

    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: TASK_ID.into(),
        effect: ToolEffect::Unknown,
        tool: "task".into(),
        summary: "research".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));

    app.update(subagent_msg_with_model(
        AgentEvent::TextDelta { text: "hi".into() },
        TASK_ID,
        "research",
        MODEL,
    ));
    app.update(subagent_msg_with_model(
        AgentEvent::TextDelta {
            text: " again".into(),
        },
        TASK_ID,
        "research",
        MODEL,
    ));

    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.chats[1].model_id.as_deref(), Some(MODEL));
    assert_eq!(
        app.chats[0].message_at(0).unwrap().annotation.as_deref(),
        Some(MODEL)
    );
}

#[test]
fn help_toggles_modal() {
    let mut app = test_app();
    assert!(!app.help_modal.is_open());
    app.update(Msg::Key(kb::HELP.to_key_event()));
    assert!(app.help_modal.is_open());
    app.execute_command(cmd("/help"), 0);
    assert!(!app.help_modal.is_open());
}

#[test]
fn command_palette_key_opens_the_modal() {
    let mut app = test_app();
    assert!(!app.command_modal.is_open());
    app.update(Msg::Key(kb::COMMAND_PALETTE.to_key_event()));
    assert!(app.command_modal.is_open());
}

/// The composer keeps its draft: unlike the inline `/` dropdown, nothing
/// the user typed was part of invoking the modal.
#[test]
fn command_palette_key_preserves_the_draft() {
    let mut app = test_app();
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    app.update(Msg::Key(kb::COMMAND_PALETTE.to_key_event()));
    assert!(app.command_modal.is_open());
    assert_eq!(app.input_box.buffer.value(), "hi");
}

#[test]
fn command_palette_runs_the_selected_command() {
    let mut app = test_app();
    app.update(Msg::Key(kb::COMMAND_PALETTE.to_key_event()));
    for c in "help".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.command_modal.is_open());
    assert!(app.help_modal.is_open());
}

/// Every source the inline dropdown enumerates has to reach the modal too,
/// including the plugin commands that only exist at runtime.
#[test]
fn command_palette_lists_builtin_and_plugin_commands() {
    let mut app = test_app();
    let names: Vec<String> = app
        .command_palette
        .rows(false)
        .into_iter()
        .map(|row| row.name)
        .collect();
    assert!(names.iter().any(|n| n == "/help"));
    assert!(names.iter().any(|n| n == "/model"));
}

#[test]
fn context_command_is_discoverable_with_an_argument() {
    let mut app = test_app();
    let command = app
        .command_palette
        .rows(false)
        .into_iter()
        .find(|row| row.name == CONTEXT_COMMAND)
        .unwrap();

    assert!(command.takes_args());
}

#[test_case(None ; "missing")]
#[test_case(Some(UNKNOWN_VIEW_MODE) ; "unknown")]
fn an_unrecognized_view_defaults_to_expanded(stored: Option<&str>) {
    let tmp = private_tempdir();
    let dir = StateDir::from_path(tmp.path().to_path_buf());
    if let Some(mode) = stored {
        stored_state::set(&dir, SCOPE_GLOBAL, VIEW_STATE_KEY, &mode).unwrap();
    }

    let app = build_app(dir.clone(), Arc::new(test_writer(dir)));

    assert_eq!(app.view, ViewMode::Expanded, "{VIEW_DEFAULT_MSG}");
}

#[test]
fn the_view_shortcut_cycles_every_mode() {
    let (_tmp, _dir, _writer, mut app) = tempdir_app();
    assert_eq!(app.view, ViewMode::Expanded, "{VIEW_DEFAULT_MSG}");

    let mut seen = Vec::new();
    for _ in 0..3 {
        press_chord(&mut app, chord::VIEW_TOGGLE);
        seen.push(app.view);
    }

    assert_eq!(
        seen,
        vec![ViewMode::Auto, ViewMode::Compact, ViewMode::Expanded],
        "{VIEW_CYCLE_MSG}"
    );
}

#[test]
fn the_view_command_cycles_like_the_keybinding() {
    let (_tmp, _dir, _writer, mut app) = tempdir_app();

    app.execute_command(cmd("/view"), 0);
    assert_eq!(app.view, ViewMode::Auto);

    app.execute_command(cmd("/view"), 0);
    assert_eq!(app.view, ViewMode::Compact);

    app.execute_command(cmd("/view"), 0);
    assert_eq!(app.view, ViewMode::Expanded);
}

/// `/compact` rewrites history and `/view` only changes rendering, so the
/// palette must not offer the destructive one first when the view is meant.
#[test]
fn view_command_does_not_shadow_compact() {
    let names: Vec<&str> = BUILTIN_COMMANDS.iter().map(|cmd| cmd.name).collect();
    assert!(names.contains(&"/view"));
    assert!(
        !names.iter().any(|name| *name != "/compact"
            && (name.starts_with("/compact") || "/compact".starts_with(name))),
        "no command may prefix-collide with /compact: {names:?}"
    );
}

#[test]
fn view_toggle_is_reachable_from_lua() {
    let (_tmp, _dir, _writer, mut app) = tempdir_app();
    app.run_builtin(BuiltinAction::ViewToggle);
    assert_eq!(app.view, ViewMode::Auto);
}

/// The mode is a reading preference, not a per-session one, so it is picked
/// once and then stays picked. Its own state dir, because the shared one
/// would carry the choice into every other app built on this thread.
#[test_case(1, ViewMode::Auto ; "auto")]
#[test_case(2, ViewMode::Compact ; "compact")]
#[test_case(3, ViewMode::Expanded ; "expanded")]
fn a_view_mode_outlives_the_app_that_chose_it(toggles: usize, expected: ViewMode) {
    let tmp = TempDir::new().expect("state dir");
    let dir = StateDir::from_path(tmp.path().to_path_buf());

    let mut app = build_app(dir.clone(), Arc::new(test_writer(dir.clone())));
    assert_eq!(app.view, ViewMode::Expanded, "{VIEW_DEFAULT_MSG}");
    for _ in 0..toggles {
        app.run_builtin(BuiltinAction::ViewToggle);
    }

    let restarted = build_app(dir.clone(), Arc::new(test_writer(dir)));
    assert_eq!(restarted.view, expected, "{VIEW_PERSIST_MSG}");
}

#[test]
fn help_modal_consumes_keys_and_esc_closes() {
    let mut app = test_app();
    app.update(Msg::Key(kb::HELP.to_key_event()));

    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert_eq!(app.input_box.buffer.value(), "");

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.help_modal.is_open());
}

#[test_case(
    |_: &mut App| {},
    &[KeybindContext::General, KeybindContext::Editing],
    &[KeybindContext::Streaming]
    ; "idle"
)]
#[test_case(
    |app: &mut App| { app.status = Status::Streaming; },
    &[KeybindContext::General, KeybindContext::Streaming, KeybindContext::Editing],
    &[]
    ; "streaming"
)]
#[test_case(
    |app: &mut App| { app.state.mode = Mode::Plan; app.plan_form.on_plan_ready(); },
    &[KeybindContext::FormInput],
    &[KeybindContext::Editing]
    ; "plan_form"
)]
#[test_case(
    |app: &mut App| { app.status = Status::Streaming; app.run_id = 1; app.queue_and_notify(queued_msg("q")); app.queue.set_focus_at(0); },
    &[KeybindContext::QueueFocus],
    &[KeybindContext::Editing]
    ; "queue_focus"
)]
#[test_case(
    |app: &mut App| {
        crate::push_history_message(app.state.session_mut(), Message::user("test".into()));
        app.open_rewind_picker();
    },
    &[KeybindContext::RewindPicker],
    &[KeybindContext::Editing]
    ; "rewind_picker"
)]
fn active_contexts(setup: fn(&mut App), expected: &[KeybindContext], absent: &[KeybindContext]) {
    let mut app = test_app();
    setup(&mut app);
    let contexts = app.active_keybind_contexts();
    for ctx in expected {
        assert!(contexts.contains(ctx), "{ctx:?} should be present");
    }
    for ctx in absent {
        assert!(!contexts.contains(ctx), "{ctx:?} should be absent");
    }
}

#[test]
fn submit_exit_quits() {
    let mut app = test_app();
    let actions = app.handle_submit(Submission::from_text("exit".into()));
    assert_eq!(app.exit_request, ExitRequest::Success);
    assert!(matches!(actions.as_slice(), [Action::ManualExit]));
}

#[test]
fn session_has_content_covers_each_branch() {
    let mut session = AppSession::new("test-model", "/tmp/test");
    assert!(!session_has_content(&session));

    session.meta.input_draft = Some("draft".into());
    assert!(session_has_content(&session));
    session.meta.input_draft = None;

    session.meta.queued_messages = vec![stored_queued_prompt("queued")];
    assert!(session_has_content(&session));
    session.meta.queued_messages.clear();

    session.meta.unsent_subagent_messages.insert(
        TASK_ID.into(),
        vec![StoredQueuedDraft {
            text: "unsent".into(),
            paste_ranges: Vec::new(),
        }],
    );
    assert!(session_has_content(&session));
    session.meta.unsent_subagent_messages.clear();

    session.meta.system_prompt_profile = Some("review".into());
    assert!(session_has_content(&session));
    session.meta.system_prompt_profile = Some("builtin".into());
    assert!(!session_has_content(&session));

    session.meta.mode = Some(StoredMode::Build);
    assert!(session_has_content(&session));
    session.meta.mode = Some(StoredMode::Plan);
    assert!(!session_has_content(&session));

    crate::push_history_message(&mut session, Message::user("hello".into()));
    assert!(session_has_content(&session));
}

#[test]
fn checkpoint_and_restore_preserve_separate_unsent_task_messages() {
    let mut app = test_app();
    app.unsent_subagent_steers.insert(
        TASK_ID.into(),
        ["first", "second"]
            .into_iter()
            .map(|text| PendingSteer {
                id: caudra_agent::QueueItemId::new(),
                text: text.into(),
                draft: InputDraft {
                    text: text.into(),
                    paste_ranges: Vec::new(),
                },
            })
            .collect(),
    );

    app.checkpoint();
    let stored = app.state.session.meta.unsent_subagent_messages.clone();
    assert_eq!(
        stored[TASK_ID]
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );

    let mut restored = test_app();
    restored.state.session_mut().meta.unsent_subagent_messages = stored;
    restored.restore_display();
    assert_eq!(
        restored.unsent_subagent_steers[TASK_ID]
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
}

#[test]
fn checkpoint_syncs_ephemeral_content_into_meta() {
    let mut app = test_app();
    app.checkpoint();
    assert!(!session_has_content(&app.state.session));
    assert_eq!(
        app.state.session.meta.system_prompt_profile.as_deref(),
        Some("builtin")
    );

    app.state.system_prompt_profile_name = "review".into();
    app.checkpoint();
    assert!(session_has_content(&app.state.session));
    app.state.system_prompt_profile_name = "builtin".into();
    app.checkpoint();
    assert!(!session_has_content(&app.state.session));

    app.update(Msg::Key(key(KeyCode::Char('x'))));
    app.checkpoint();
    assert!(session_has_content(&app.state.session));

    app.update(Msg::Key(key(KeyCode::Backspace)));
    app.checkpoint();
    assert!(app.state.session.meta.input_draft.is_none());
    assert!(!session_has_content(&app.state.session));

    app.update(Msg::Key(key(KeyCode::Tab)));
    app.checkpoint();
    assert_eq!(app.state.session.meta.mode, Some(StoredMode::Build));
    assert!(session_has_content(&app.state.session));

    let mut queued = app_with_queued_message();
    queued.checkpoint();
    let session = &queued.state.session;
    assert!(session.messages().is_empty());
    assert!(session.meta.input_draft.is_none());
    assert_eq!(session.meta.mode, Some(StoredMode::Plan));
    assert_eq!(
        session.meta.queued_messages,
        [stored_queued_prompt("queued")]
    );
    assert!(session_has_content(session));
}

#[test]
fn invocation_profile_override_does_not_replace_stored_selection() {
    let mut app = test_app();
    app.state.session_mut().meta.system_prompt_profile = Some("stored".into());
    app.state.system_prompt_profile_name = "override".into();
    app.state.system_prompt_profile_override = true;

    app.checkpoint();

    assert_eq!(
        app.state.session.meta.system_prompt_profile.as_deref(),
        Some("stored")
    );
}

#[test]
fn checkpoint_and_restore_preserve_together_delivery() {
    let mut app = app_with_queued_message();
    app.queue
        .set_delivery(caudra_agent::QueueDelivery::TogetherNextTurn);
    app.checkpoint();

    assert!(app.state.session.meta.queued_messages_together);
    let meta = app.state.session.meta.clone();
    let mut restored = test_app();
    restored.state.session_mut().meta = meta;
    restored.flush_restored_queue();

    assert_eq!(
        restored.queue.delivery(),
        caudra_agent::QueueDelivery::TogetherNextTurn
    );
    assert_eq!(restored.queue.panel_entries().len(), 1);
}

#[test]
fn checkpoint_persists_observations_without_using_them_as_title() {
    let mut app = test_app();
    let initial_title = app.state.session.title.clone();
    let _history = attach_live_history(
        &mut app,
        vec![
            Message::observation("build failed".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "I will fix it".into(),
                }],
                ..Default::default()
            },
        ],
    );

    app.checkpoint();

    assert_eq!(app.state.session.messages().len(), 2);
    assert!(matches!(
        app.state.session.messages()[0].kind,
        HistoryItemKind::User {
            origin: UserOrigin::Observation,
            ..
        }
    ));
    assert_eq!(app.state.session.title, initial_title);
}

fn drain_writer(app: App, writer: Arc<StorageWriter>) {
    drop(app);
    Arc::try_unwrap(writer)
        .ok()
        .expect("app must hold the only other writer reference")
        .shutdown(WRITER_DRAIN_TIMEOUT);
}

fn stored_peer_controls() -> StoredPeerControls {
    StoredPeerControls {
        inbound: Some(StoredInboundPolicy::Hold),
        delivered: PEER_DELIVERED,
        sends: PEER_SENT,
    }
}

#[test_case(false; "experiment_disabled")]
#[test_case(true; "experiment_enabled")]
fn peer_controls_survive_checkpoint_and_reload(enabled: bool) {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    app.features = if enabled {
        FeatureFlags::NONE.with(Feature::CrossSessionMessaging)
    } else {
        FeatureFlags::NONE
    };
    app.state.session_mut().meta.peer_controls = Some(stored_peer_controls());
    crate::push_history_message(
        app.state.session_mut(),
        Message::user(PEER_CONTROL_PROMPT.into()),
    );
    app.checkpoint_now();
    let id = app.state.session.id;
    let features = app.features;
    drain_writer(app, writer);
    let loaded = AppSession::load(id, &dir).unwrap();
    assert_eq!(loaded.meta.peer_controls, Some(stored_peer_controls()));

    let mut restored = test_app();
    restored.features = features;
    restored
        .apply_loaded_session(loaded, &test_model())
        .unwrap();
    restored.checkpoint_now();
    assert_eq!(
        restored.state.session.meta.peer_controls,
        Some(stored_peer_controls())
    );
}

#[test]
fn peer_controls_alone_do_not_persist_a_blank_session() {
    let mut app = test_app();
    app.state.session_mut().meta.peer_controls = Some(stored_peer_controls());
    app.checkpoint_now();
    assert_eq!(
        app.state.session.meta.peer_controls,
        Some(stored_peer_controls())
    );
    assert!(!app.has_content());
    assert!(!app.state.session.is_persisted());
}

#[test_case(false; "new_session")]
#[test_case(true; "fork")]
fn peer_controls_are_not_inherited_by_new_session_ids(fork: bool) {
    let mut app = test_app();
    app.suppress_background_wakes();
    app.state.session_mut().meta.peer_controls = Some(stored_peer_controls());
    crate::push_history_message(
        app.state.session_mut(),
        Message::user(PEER_CONTROL_PROMPT.into()),
    );
    app.checkpoint_now();
    let old_id = app.state.session.id;
    if fork {
        let source = app.state.session.messages()[0].id;
        let child = app.fork_at(DisplaySource::User(source)).unwrap();
        assert_ne!(child.session.id, old_id);
        assert_eq!(child.session.meta.peer_controls, None);
        assert!(!child.session.meta.automatic_wakes_suppressed);
    } else {
        assert!(!app.reset_session().is_empty());
        app.checkpoint_now();
        assert_ne!(app.state.session.id, old_id);
        assert_eq!(app.state.session.meta.peer_controls, None);
        assert!(!app.automatic_wakes_suppressed);
        assert!(!app.state.session.meta.automatic_wakes_suppressed);
    }
}

#[test]
fn peer_controls_survive_conversation_rewind() {
    let mut app = test_app();
    app.state.session_mut().meta.peer_controls = Some(stored_peer_controls());
    let _history = attach_live_history(
        &mut app,
        vec![
            Message::user(PEER_CONTROL_PROMPT.into()),
            Message::user(PEER_CONTROL_PROMPT.into()),
        ],
    );
    app.checkpoint_now();
    assert!(
        !app.rewind_to(RewindEntry {
            turn_index: 1,
            prompt_preview: PEER_CONTROL_PROMPT.into(),
        })
        .is_empty()
    );
    app.checkpoint_now();
    assert_eq!(
        app.state.session.meta.peer_controls,
        Some(stored_peer_controls())
    );
    assert!(app.automatic_wakes_suppressed);
    assert!(app.state.session.meta.automatic_wakes_suppressed);
}

#[test_case(DoneReason::Cancelled, true, true; "cancelled_process_resume")]
#[test_case(DoneReason::MaxTurns, true, true; "exhausted_process_resume")]
#[test_case(DoneReason::Cancelled, false, true; "cancelled_loaded_session")]
#[test_case(DoneReason::MaxTurns, false, true; "exhausted_loaded_session")]
#[test_case(DoneReason::Cancelled, true, false; "disabled_process_resume")]
#[test_case(DoneReason::MaxTurns, false, false; "disabled_loaded_session")]
fn automatic_wake_suppression_survives_reload(reason: DoneReason, startup: bool, messaging: bool) {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    crate::push_history_message(
        app.state.session_mut(),
        Message::user(PEER_CONTROL_PROMPT.into()),
    );
    app.status = Status::Streaming;
    app.update(agent_msg_with_run_id(
        AgentEvent::Done {
            usage: TokenUsage::default(),
            num_turns: 1,
            reason,
        },
        app.run_id,
    ));
    assert!(app.automatic_wakes_suppressed);
    app.checkpoint_now();
    let id = app.state.session.id;
    drain_writer(app, writer);
    let loaded = AppSession::load(id, &dir).unwrap();
    assert!(loaded.meta.automatic_wakes_suppressed);

    let mut restored = test_app();
    restored.features = if messaging {
        FeatureFlags::NONE.with(Feature::CrossSessionMessaging)
    } else {
        FeatureFlags::NONE
    };
    if startup {
        restored.state = SessionState::from_session(
            loaded,
            &test_model(),
            &restored.storage,
            &restored.model_policy,
        );
        restored.restore_resumed_session();
    } else {
        restored
            .apply_loaded_session(loaded, &test_model())
            .unwrap();
    }
    assert!(restored.automatic_wakes_suppressed);
    assert!(restored.start_mailbox_run(Vec::new()).is_empty());
    restored.checkpoint_now();
    assert!(restored.state.session.meta.automatic_wakes_suppressed);

    restored.shared_history = Some(Arc::new(ArcSwap::from_pointee(HistorySnapshot::new(
        crate::active_session_history(&restored.state.session).unwrap(),
    ))));
    let actions = restored.execute_command(cmd("/continue"), 0);
    assert!(matches!(actions.as_slice(), [Action::SendMessage(input)] if input.resume));
    assert!(!restored.automatic_wakes_suppressed);
    restored.checkpoint_now();
    assert!(!restored.state.session.meta.automatic_wakes_suppressed);
}

#[test]
fn reload_persists_session_with_content_to_disk() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    crate::push_history_message(app.state.session_mut(), Message::user("hello".into()));
    let actions = app.execute_command(cmd("/reload"), 0);
    assert_eq!(app.exit_request, ExitRequest::Reload);
    assert!(matches!(actions.as_slice(), [Action::ManualExit]));
    app.checkpoint();
    let id = app.state.session.id;
    drain_writer(app, writer);

    assert_eq!(AppSession::load(id, &dir).unwrap().messages().len(), 1);
}

#[test]
fn reload_leaves_empty_session_unpersisted_on_disk() {
    let (tmp, _dir, writer, mut app) = tempdir_app();
    app.execute_command(cmd("/reload"), 0);
    drain_writer(app, writer);

    let sessions_dir = tmp.path().join(caudra_storage::sessions::SESSIONS_DIR);
    let entries = std::fs::read_dir(&sessions_dir)
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(entries, 0);
}

#[test]
fn checkpoint_persists_queued_submission_images_and_paste_ranges() {
    const QUEUED_TEXT: &str = "pasted prompt";

    let (_tmp, dir, writer, mut app) = tempdir_app();
    let (queue, _receiver) = shared_queue::queue();
    app.queue.set_shared(queue);
    app.status = Status::Streaming;
    app.run_id = 1;
    let draft_text = format!("  {QUEUED_TEXT}  ");
    let image = ImageSource::new(ImageMediaType::Png, Arc::from("aW1hZ2U="));

    let actions = app.handle_submit_with_admission(
        Submission {
            text: QUEUED_TEXT.into(),
            images: vec![image],
            mentions: Vec::new(),
            commits: Vec::new(),
            draft: InputDraft {
                text: draft_text.clone(),
                paste_ranges: std::iter::once(0..draft_text.len()).collect(),
            },
        },
        caudra_agent::PromptAdmission::Steer,
    );
    app.checkpoint_now();

    assert!(actions.is_empty());
    let id = app.state.session.id;
    drain_writer(app, writer);
    let saved = AppSession::load(id, &dir).unwrap();
    assert_eq!(
        saved.meta.queued_messages,
        [StoredQueuedPrompt {
            mode: Some(StoredMode::Plan),
            text: QUEUED_TEXT.into(),
            images: vec![StoredImage {
                media_type: "image/png".into(),
                data: "aW1hZ2U=".into(),
            }],
            paste_ranges: vec![StoredPasteRange {
                start: 0,
                end: QUEUED_TEXT.len(),
            }],
        }]
    );
    assert_eq!(
        saved.meta.queued_message_admissions,
        [StoredPromptAdmission::Steer]
    );
}

#[test]
fn restore_resumed_session_flushes_complete_queued_prompts_and_round_trips() {
    let mut app = test_app();
    let stored_prompts = vec![
        StoredQueuedPrompt {
            mode: Some(StoredMode::Plan),
            text: "q1".into(),
            images: vec![StoredImage {
                media_type: "image/png".into(),
                data: "aW1hZ2U=".into(),
            }],
            paste_ranges: vec![StoredPasteRange { start: 0, end: 2 }],
        },
        stored_queued_prompt("q2"),
    ];
    app.state.session_mut().meta.queued_messages = stored_prompts.clone();
    app.state.session_mut().meta.queued_message_admissions =
        vec![StoredPromptAdmission::Steer, StoredPromptAdmission::Queue];

    app.restore_resumed_session();
    assert_eq!(
        app.queue.pending_prompts(),
        [
            shared_queue::PendingPrompt {
                mode: Some(StoredMode::Plan),
                text: "q1".into(),
                images: vec![ImageSource::new(ImageMediaType::Png, Arc::from("aW1hZ2U="))],
                paste_ranges: std::iter::once(0..2).collect(),
                admission: caudra_agent::PromptAdmission::Steer,
            },
            shared_queue::PendingPrompt {
                mode: Some(StoredMode::Plan),
                text: "q2".into(),
                images: Vec::new(),
                paste_ranges: Vec::new(),
                admission: caudra_agent::PromptAdmission::Queue,
            },
        ]
    );
    assert_eq!(app.status, Status::Streaming);

    app.checkpoint();
    assert_eq!(app.state.session.meta.queued_messages, stored_prompts);
    assert_eq!(
        app.state.session.meta.queued_message_admissions,
        [StoredPromptAdmission::Steer, StoredPromptAdmission::Queue]
    );
}

#[test]
fn apply_loaded_session_defers_queued_messages_until_respawn() {
    let mut app = test_app();
    let mut session = AppSession::new("test-model", &app.state.session.cwd);
    session.meta.queued_messages = vec![stored_queued_prompt("deferred")];
    crate::push_history_message(&mut session, Message::user("hello".into()));

    let model = app.state.model.clone();
    app.apply_loaded_session(session, &model).unwrap();

    assert!(app.queue.is_empty());
    assert_eq!(
        app.state.session.meta.queued_messages,
        [stored_queued_prompt("deferred")]
    );
}

#[test_case(false, PermissionMode::Ask; "main_ask")]
#[test_case(false, PermissionMode::Yolo; "main_yolo")]
#[test_case(true, PermissionMode::Ask; "task_ask")]
#[test_case(true, PermissionMode::Yolo; "task_yolo")]
fn decisions_command_opens_the_modal_without_changing_permissions(
    task: bool,
    mode: PermissionMode,
) {
    let mut app = if task {
        read_only_task_app()
    } else {
        test_app()
    };
    app.permissions.set_session_mode(Some(mode.clone()));
    assert!(app.run_cmdline("decisions", 0).unwrap().is_empty());
    assert!(app.decisions_modal.is_open());
    assert_eq!(app.permissions.mode(), mode);
}

#[test_case(PermissionMode::Ask; "ask")]
#[test_case(PermissionMode::Yolo; "yolo")]
fn decisions_command_reads_configuration_without_probing(mode: PermissionMode) {
    let mut app = test_app();
    let config = DecisionsConfig {
        base_url: Some(DECISIONS_TEST_BASE_URL.parse().unwrap()),
        model: DECISIONS_TEST_MODEL.into(),
        ..Default::default()
    };
    let decisions = Decisions::new(config, &app.storage).unwrap();
    decisions.mark_tainted();
    app.permissions.set_decisions(Some(decisions.clone()));
    app.permissions.set_session_mode(Some(mode.clone()));
    assert!(app.execute_command(cmd(DECISIONS_COMMAND), 0).is_empty());
    let screen = rendered(&mut app);
    for expected in [DECISIONS_TEST_BASE_URL, DECISIONS_TEST_MODEL] {
        assert!(screen.contains(expected), "{expected}: {screen}");
    }
    assert_eq!(decisions.status().reachable, None);
    assert_eq!(decisions.status().last_error, None);
    assert!(decisions.is_tainted());
    assert_eq!(app.permissions.mode(), mode);
}

#[test_case(DECISIONS_COMMAND, None; "status")]
#[test_case("/DECISIONS", None; "uppercase")]
#[test_case("/decisions unexpected", Some(DECISIONS_USAGE); "argument")]
fn decisions_command_submission_stays_local(command: &str, expected_flash: Option<&str>) {
    for mut app in [test_app(), steerable_task_app()] {
        let actions = type_and_submit(&mut app, command);
        assert!(actions.is_empty());
        assert_eq!(app.status_bar.flash_text(), expected_flash);
        assert_eq!(app.decisions_modal.is_open(), expected_flash.is_none());
        assert!(app.queue.is_empty());
        if let Some(steers) = app.subagent_steers.get(TASK_ID) {
            assert!(steers.entries().is_empty());
        }
    }
}

#[test]
fn yolo_toggle() {
    let mut app = test_app();
    assert!(!app.permissions.is_yolo());
    app.execute_command(cmd("/yolo"), 0);
    assert!(app.permissions.is_yolo());
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.contains("enabled"), "flash={flash:?}");
    app.execute_command(cmd("/yolo"), 0);
    assert!(!app.permissions.is_yolo());
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.contains("disabled"), "flash={flash:?}");
}

#[test_case(PermissionMode::Ask; "ask")]
#[test_case(PermissionMode::Auto; "auto")]
#[test_case(PermissionMode::Yolo; "yolo")]
fn auto_command_toggles_only_ask_and_auto(initial: PermissionMode) {
    let mut app = test_app();
    app.permissions.set_session_mode(Some(initial.clone()));
    app.run_cmdline("auto", 0).unwrap();
    let enabled = initial != PermissionMode::Auto;
    let expected = if enabled {
        PermissionMode::Auto
    } else {
        PermissionMode::Ask
    };
    assert_eq!(app.permissions.mode(), expected);
    assert_eq!(
        app.status_bar.flash_text(),
        Some(if enabled { AUTO_ON_MSG } else { AUTO_OFF_MSG })
    );
    app.checkpoint();
    assert_eq!(app.state.session.meta.permission_mode, Some(expected));
}

#[test_case(false; "ask")]
#[test_case(true; "auto")]
fn yolo_command_still_requires_explicit_opt_in(auto: bool) {
    let mut app = test_app();
    if auto {
        app.execute_command(cmd("/auto"), 0);
    }
    app.execute_command(cmd("/yolo"), 0);
    assert_eq!(app.permissions.mode(), PermissionMode::Yolo);
    assert_eq!(app.status_bar.flash_text(), Some(YOLO_ON_MSG));
    app.execute_command(cmd("/yolo"), 0);
    assert_eq!(app.permissions.mode(), PermissionMode::Ask);
    assert_eq!(app.status_bar.flash_text(), Some(YOLO_OFF_MSG));
}

/// The toggle is session state like mode and thinking, so a checkpoint has to
/// mirror it or a resume silently downgrades the session's permissions.
#[test]
fn checkpoint_mirrors_the_yolo_toggle_into_meta() {
    let mut app = test_app();
    app.checkpoint();
    assert_eq!(app.state.session.meta.permission_mode, None);

    app.execute_command(cmd("/yolo"), 0);
    app.checkpoint();
    assert_eq!(
        app.state.session.meta.permission_mode,
        Some(PermissionMode::Yolo)
    );

    app.execute_command(cmd("/yolo"), 0);
    app.checkpoint();
    assert_eq!(
        app.state.session.meta.permission_mode,
        Some(PermissionMode::Ask)
    );
}

fn conversation_permission_record() -> caudra_agent::permissions::PermissionRuleRecord {
    use caudra_agent::permissions::{
        PermissionArgumentConstraint, PermissionExecutorKind, PermissionLifetime,
        PermissionResourceAccess, PermissionResourceConstraint, PermissionResourceKind,
        PermissionResourceSelector, PermissionRuleRecord, PermissionSubject,
        StructuredPermissionEffect, StructuredPermissionRule,
    };
    PermissionRuleRecord::conversation(StructuredPermissionRule {
        subject: PermissionSubject::Native {
            owner: "workcell".into(),
            contract: "shell.execution.v1".into(),
        },
        executor: PermissionExecutorKind::Native,
        resources: vec![PermissionResourceConstraint {
            kind: PermissionResourceKind::Command,
            selector: PermissionResourceSelector::CommandPattern {
                pattern: CONVERSATION_PERMISSION_PATTERN.into(),
            },
            access: Some(PermissionResourceAccess::Execute),
            protected: Some(false),
            attributes: Default::default(),
        }],
        arguments: PermissionArgumentConstraint::Unconstrained,
        lifetime: PermissionLifetime::Conversation,
        effect: StructuredPermissionEffect::Allow,
        family: None,
    })
    .unwrap()
}

/// Conversation rules carry the real grants, so a dropped restore call has to
/// fail here rather than silently re-prompting for everything next session.
#[test]
fn checkpoint_and_resume_restore_structured_conversation_rules() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    crate::push_history_message(
        app.state.session_mut(),
        Message::user(RESUMED_PROMPT.into()),
    );
    let record = conversation_permission_record();
    app.permissions
        .load_structured_conversation_rules(vec![record.clone()]);
    app.checkpoint();
    let id = app.state.session.id;
    drain_writer(app, writer);

    let stored = AppSession::load(id, &dir).unwrap();
    assert_eq!(stored.meta.structured_permission_rules.len(), 1);
    let resumed_writer = Arc::new(test_writer(dir.clone()));
    let mut resumed = build_app(dir, Arc::clone(&resumed_writer));
    resumed.state.session = Arc::new(stored);
    resumed.restore_resumed_session();

    let restored = resumed.permissions.structured_conversation_rules_snapshot();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].id, record.id);
    assert_eq!(restored[0].rule, record.rule);
    drain_writer(resumed, resumed_writer);
}

/// A session whose only content is a permission grant still has to restore, so
/// `session_has_content` must keep counting the structured rules.
#[test]
fn a_session_holding_only_permission_grants_still_restores() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    app.permissions
        .load_structured_conversation_rules(vec![conversation_permission_record()]);
    app.checkpoint();
    let id = app.state.session.id;
    drain_writer(app, writer);

    let stored = AppSession::load(id, &dir).unwrap();
    assert!(crate::app::session::session_has_content(&stored));
}

/// A tab the way the event loop spawns a fresh one: durable, but owed no row
/// until it holds something.
fn pending_app() -> (TempDir, StateDir, Arc<StorageWriter>, App) {
    let (tmp, dir, writer, mut app) = tempdir_app();
    app.conversation_permissions = ConversationPermissions::Pending;
    (tmp, dir, writer, app)
}

fn saved_sessions(app: &App, dir: &StateDir) -> usize {
    AppSession::list(&app.state.session.cwd, dir).unwrap().len()
}

/// Starting Caudra and quitting must leave nothing behind, which it only does
/// while the conversation permissions have yet to publish.
#[test]
fn a_pristine_session_reaches_shutdown_without_a_row() {
    let (_tmp, dir, writer, mut app) = pending_app();
    app.checkpoint_now();
    let cwd = app.state.session.cwd.clone();
    drain_writer(app, writer);

    assert!(AppSession::list(&cwd, &dir).unwrap().is_empty());
}

/// The turn is what earns the row, so the publication that fences conversation
/// grants against it can only attach here.
#[test]
fn the_first_run_creates_the_row_and_publishes_permissions() {
    let (_tmp, dir, writer, mut app) = pending_app();
    let actions = app.submit_prompt(queued_msg(FIRST_TURN_PROMPT));

    assert!(matches!(actions, SubmitOutcome::Started(_)));
    assert!(app.conversation_permissions.is_published());
    assert!(app.permissions.conversation_permission_snapshot().is_some());
    assert_eq!(saved_sessions(&app, &dir), 1);
    drain_writer(app, writer);
}

/// Without a published row a conversation grant has nothing to be fenced
/// against, so the turn is refused rather than run unprotected.
#[test]
fn a_failed_publication_rejects_the_run() {
    let (_tmp, dir, writer, mut app) = pending_app();
    // A directory where the database belongs fails the open whoever runs the
    // suite, which a read-only state dir does not.
    let database = dir.path().join(caudra_storage::sessions::SESSIONS_DB_FILE);
    fs::remove_file(&database).unwrap();
    fs::create_dir(&database).unwrap();

    let actions = app.submit_prompt(queued_msg(FIRST_TURN_PROMPT));

    assert!(matches!(actions, SubmitOutcome::Started(rejected) if rejected.is_empty()));
    assert!(!app.conversation_permissions.is_published());
    assert!(
        app.status_bar
            .flash_text()
            .is_some_and(|flash| flash.contains(queue::PERMISSION_PUBLISH_ERR))
    );
    assert_eq!(app.status, Status::Idle);
    drain_writer(app, writer);
}

/// A draft is worth a row of its own, and clearing it takes that row back. The
/// delete is only reachable while nothing has published against the session.
#[test]
fn a_cleared_draft_removes_the_row_it_created() {
    let (_tmp, dir, writer, mut app) = pending_app();
    app.update(Msg::Key(key(KeyCode::Char('x'))));
    app.checkpoint_now();
    writer.save_sync(Arc::clone(&app.state.session)).unwrap();
    assert_eq!(saved_sessions(&app, &dir), 1);

    app.update(Msg::Key(key(KeyCode::Backspace)));
    app.checkpoint_now();
    let cwd = app.state.session.cwd.clone();
    drain_writer(app, writer);

    assert!(AppSession::list(&cwd, &dir).unwrap().is_empty());
}

/// A grant made before the first turn is persisted by the ordinary checkpoint,
/// so the attach has to adopt it instead of reading it as a conflict.
#[test]
fn a_grant_made_before_the_first_run_survives_publication() {
    let (_tmp, _dir, writer, mut app) = pending_app();
    let record = conversation_permission_record();
    app.permissions
        .load_structured_conversation_rules(vec![record.clone()]);
    app.checkpoint_now();

    app.publish_conversation_permissions().unwrap();

    let published = app.conversation_permissions.published().unwrap().load();
    assert_eq!(published.records.len(), 1);
    assert_eq!(published.records[0].id, record.id);
    drain_writer(app, writer);
}

/// `/new` hands the tab a fresh session, which owes a row no sooner than the
/// one it replaced did.
#[test]
fn reset_session_writes_no_row_until_its_first_run() {
    let (_tmp, dir, writer, mut app) = pending_app();
    app.submit_prompt(queued_msg(FIRST_TURN_PROMPT));
    crate::push_history_message(
        app.state.session_mut(),
        Message::user(FIRST_TURN_PROMPT.into()),
    );
    let retired = app.state.session.id;

    app.reset_session();
    app.checkpoint_now();

    assert!(matches!(
        app.conversation_permissions,
        ConversationPermissions::Pending
    ));
    assert_ne!(app.state.session.id, retired);
    let saved = AppSession::list(&app.state.session.cwd, &dir).unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].id, retired);
    drain_writer(app, writer);
}

#[test_case(PermissionMode::Ask, None, PermissionMode::Ask; "inherit_ask")]
#[test_case(PermissionMode::Auto, None, PermissionMode::Auto; "inherit_auto")]
#[test_case(PermissionMode::Yolo, None, PermissionMode::Yolo; "inherit_yolo")]
#[test_case(PermissionMode::Ask, Some(PermissionMode::Yolo), PermissionMode::Yolo; "restore_yolo")]
#[test_case(PermissionMode::Yolo, Some(PermissionMode::Yolo), PermissionMode::Yolo; "preserve_yolo")]
#[test_case(PermissionMode::Yolo, Some(PermissionMode::Ask), PermissionMode::Ask; "ask_overrides_yolo")]
#[test_case(PermissionMode::Ask, Some(PermissionMode::Auto), PermissionMode::Auto; "restore_auto")]
#[test_case(PermissionMode::Yolo, Some(PermissionMode::Auto), PermissionMode::Auto; "auto_overrides_yolo")]
#[test_case(PermissionMode::Auto, Some(PermissionMode::Ask), PermissionMode::Ask; "ask_overrides_auto")]
#[test_case(PermissionMode::Auto, Some(PermissionMode::Yolo), PermissionMode::Yolo; "yolo_overrides_auto")]
fn permission_mode_restore_and_checkpoint(
    seed: PermissionMode,
    stored: Option<PermissionMode>,
    expected: PermissionMode,
) {
    for resume in [false, true] {
        let mut app = test_app();
        app.permissions.set_seed_mode(seed.clone());
        let mut session = AppSession::new("test-model", &app.state.session.cwd);
        session.meta.permission_mode = stored.clone();
        crate::push_history_message(&mut session, Message::user(RESUMED_PROMPT.into()));
        if resume {
            app.state.session = Arc::new(session);
            app.restore_resumed_session();
        } else {
            let model = app.state.model.clone();
            app.apply_loaded_session(session, &model).unwrap();
        }
        assert_eq!(app.permissions.mode(), expected);
        app.checkpoint();
        assert_eq!(app.state.session.meta.permission_mode, stored);
        let meta: SessionMeta =
            serde_json::from_str(&serde_json::to_string(&app.state.session.meta).unwrap()).unwrap();
        assert_eq!(meta.permission_mode, stored);
    }
}

#[test_case(PermissionMode::Ask, PermissionMode::Yolo; "ask_seed")]
#[test_case(PermissionMode::Auto, PermissionMode::Ask; "auto_seed")]
#[test_case(PermissionMode::Yolo, PermissionMode::Ask; "yolo_seed")]
fn reset_permission_mode_returns_to_seed(seed: PermissionMode, stored: PermissionMode) {
    let mut app = test_app();
    app.permissions.set_seed_mode(seed.clone());
    app.state.session_mut().meta.permission_mode = Some(stored.clone());
    app.restore_resumed_session();
    assert_eq!(app.permissions.mode(), stored);
    app.checkpoint();
    app.reset_session();
    app.checkpoint();
    assert_eq!(app.permissions.mode(), seed);
    assert_eq!(app.state.session.meta.permission_mode, None);
}

#[test_case(PermissionMode::Ask; "ask")]
#[test_case(PermissionMode::Auto; "auto")]
#[test_case(PermissionMode::Yolo; "yolo")]
fn permission_mode_survives_durable_checkpoint_and_resume(expected: PermissionMode) {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    crate::push_history_message(
        app.state.session_mut(),
        Message::user(RESUMED_PROMPT.into()),
    );
    app.state.session_mut().meta.permission_mode = Some(expected.clone());
    app.permissions.set_seed_mode(PermissionMode::Auto);
    app.restore_resumed_session();
    app.checkpoint();
    let id = app.state.session.id;
    drain_writer(app, writer);

    let stored = AppSession::load(id, &dir).unwrap();
    assert_eq!(stored.meta.permission_mode, Some(expected.clone()));
    let resumed_writer = Arc::new(test_writer(dir.clone()));
    let mut resumed = build_app(dir, Arc::clone(&resumed_writer));
    resumed.permissions.set_seed_mode(PermissionMode::Yolo);
    resumed.state.session = Arc::new(stored);
    resumed.restore_resumed_session();
    assert_eq!(resumed.permissions.mode(), expected);
    drain_writer(resumed, resumed_writer);
}

#[test]
fn resetting_the_session_clears_conversation_rules() {
    let mut app = test_app();
    app.permissions
        .load_structured_conversation_rules(vec![conversation_permission_record()]);

    app.reset_session();
    app.checkpoint();

    assert!(
        app.permissions
            .structured_conversation_rules_snapshot()
            .is_empty()
    );
    assert!(
        app.state
            .session
            .meta
            .structured_permission_rules
            .is_empty()
    );
}

#[test]
fn the_lifetime_view_reads_spend_the_current_session_never_produced() {
    let (_tmp, dir, _writer, mut app) = tempdir_app();
    let ledger = UsageLedger::open(&dir).unwrap();
    ledger
        .record(&TurnUsage {
            provider: LEDGER_PROVIDER.into(),
            model: LEDGER_MODEL.into(),
            cwd: LEDGER_CWD.into(),
            purpose: LedgerPurpose::Chat,
            input: 1,
            output: 1,
            cache_creation: 0,
            cache_read: 0,
            cost: Some(LEDGER_COST),
            subscription: false,
        })
        .unwrap();

    app.execute_command(cmd("/usage"), 0);
    assert!(
        app.lifetime_usage.is_none(),
        "{LIFETIME_IS_NOT_READ_UNTIL_ASKED_FOR}"
    );

    app.handle_key(SCOPE_KEY.to_key_event());

    let lifetime = app.lifetime_usage.as_ref().expect("ledger should be read");
    assert_eq!(lifetime.cost, LEDGER_COST);
    assert_eq!(
        lifetime.by_model[0].label,
        format!("{LEDGER_PROVIDER}/{LEDGER_MODEL}")
    );
}

#[test]
fn usage_command_toggles_modal() {
    let mut app = test_app();
    assert!(!app.usage_modal.is_open());
    let open_actions = app.execute_command(cmd("/usage"), 0);
    assert!(app.usage_modal.is_open());
    assert!(
        open_actions
            .iter()
            .any(|a| matches!(a, Action::RefreshUsage)),
        "opening should request a quota refresh"
    );
    let close_actions = app.execute_command(cmd("/usage"), 0);
    assert!(!app.usage_modal.is_open());
    assert!(
        !close_actions
            .iter()
            .any(|a| matches!(a, Action::RefreshUsage)),
        "closing should not trigger a refresh"
    );
}

#[test]
fn goal_command_sets_condition_and_starts_work() {
    let mut app = test_app();
    let actions = app.run_cmdline("/goal all focused tests pass", 0).unwrap();

    assert_eq!(
        app.state.goal.snapshot().unwrap().condition.as_ref(),
        "all focused tests pass"
    );
    assert_eq!(app.status_bar.flash_text(), Some(queue::GOAL_SET));
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::SendMessage(input) if input.message == "all focused tests pass"
    )));
}

#[test_case("clear")]
#[test_case("STOP")]
#[test_case("off")]
#[test_case("reset")]
#[test_case("none")]
#[test_case("cancel")]
fn goal_clear_aliases_leave_no_active_goal(alias: &str) {
    let mut app = test_app();
    app.state.goal.set("ship it").unwrap();

    let actions = app.run_cmdline(&format!("/goal {alias}"), 0).unwrap();

    assert!(actions.is_empty());
    assert!(app.state.goal.snapshot().is_none());
    assert_eq!(app.status_bar.flash_text(), Some("Goal cleared: ship it"));
}

#[test]
fn goal_clear_registered_command_stops_active_goal() {
    let mut app = test_app();
    app.state.goal.set("ship it").unwrap();

    let actions = app.run_cmdline("/goal-clear", 0).unwrap();

    assert!(actions.is_empty());
    assert!(app.state.goal.snapshot().is_none());
    assert_eq!(app.status_bar.flash_text(), Some("Goal cleared: ship it"));
}

#[test]
fn goal_without_arguments_opens_status_modal() {
    let mut app = test_app();
    assert!(!app.goal_modal.is_open());
    assert!(app.run_cmdline("/goal", 0).unwrap().is_empty());
    assert!(app.goal_modal.is_open());
}

#[test]
fn goal_loop_cap_notice_separates_run_continuations_from_total_evaluations() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::GoalLoopCap {
        evaluations: 23,
        continuations: 16,
        limit: 16,
    }));

    assert_eq!(
        app.main_chat().last_message_role(),
        Some(&DisplayRole::Notice)
    );
    assert_eq!(
        app.main_chat().last_message_text(),
        "Goal remains active after 16 automatic continuations in this run (23 total evaluations); the session limit is 16. Send another message to resume."
    );
}

#[test_case("/goal-model"; "registered_command")]
#[test_case("/goal MODEL"; "legacy_subcommand")]
fn goal_model_opens_dedicated_picker_without_starting_goal(command: &str) {
    let mut app = test_app();

    let actions = app.run_cmdline(command, 0).unwrap();

    assert!(app.model_picker.is_open());
    assert!(app.state.goal.snapshot().is_none());
    assert!(matches!(&actions[..], [Action::RefreshModels]));
}

/// The evaluator can be served by a provider this session never chose. Billing
/// that to the chat provider would make `--group-by provider` describe a
/// session that never happened.
#[test]
fn goal_evaluation_is_billed_to_its_own_provider_and_purpose() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    app.state.goal.set(GOAL_CONDITION).unwrap();
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::GoalEvaluation {
        verdict: GoalVerdict::NotMet,
        reason: GOAL_REASON.into(),
        evaluation: 1,
        applied: true,
        usage: TokenUsage {
            input: GOAL_INPUT,
            output: GOAL_OUTPUT,
            ..Default::default()
        },
        cost: Some(GOAL_COST),
        billing: Billing::Api,
        model: format!("{OTHER_PROVIDER}/{LEDGER_MODEL}"),
    }));
    drain_writer(app, writer);

    let rows = UsageLedger::open(&dir).unwrap().buckets(None).unwrap();
    assert_eq!(rows.len(), 1, "{GOAL_SPEND_IS_ITS_OWN}");
    assert_eq!(rows[0].provider, OTHER_PROVIDER, "{GOAL_SPEND_IS_ITS_OWN}");
    assert_eq!(rows[0].model, LEDGER_MODEL, "{GOAL_SPEND_IS_ITS_OWN}");
    assert_eq!(
        rows[0].purpose,
        LedgerPurpose::Goal.storage_name(),
        "{GOAL_SPEND_IS_ITS_OWN}"
    );
    assert_eq!(rows[0].cost, GOAL_COST, "{GOAL_SPEND_IS_ITS_OWN}");
}

/// Compaction runs on its own model and is not the conversation, so it must
/// reach the ledger under its own provider and purpose.
#[test]
fn compaction_is_billed_to_its_own_provider_and_purpose() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(turn_complete_from(
        OTHER_PROVIDER,
        TokenUsage {
            input: GOAL_INPUT,
            output: GOAL_OUTPUT,
            ..Default::default()
        },
        LEDGER_MODEL,
        Some(GOAL_COST),
        LedgerPurpose::Compaction,
        0,
    )));
    drain_writer(app, writer);

    let rows = UsageLedger::open(&dir).unwrap().buckets(None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].provider, OTHER_PROVIDER);
    assert_eq!(rows[0].purpose, LedgerPurpose::Compaction.storage_name());
}

#[test_case(false, false; "main")]
#[test_case(true, false; "subagent")]
#[test_case(false, true; "cancelled_run")]
fn repair_accounting_persists_once_without_changing_context(subagent: bool, stale: bool) {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    app.status = Status::Streaming;
    app.run_id = if stale { 2 } else { 1 };
    app.state.goal.set(GOAL_CONDITION).unwrap();
    let usage = TokenUsage {
        input: GOAL_INPUT,
        output: GOAL_OUTPUT,
        ..Default::default()
    };
    let chat_idx = if subagent {
        app.resolve_or_create_chat(&subagent_info(TASK_ID, RESEARCH_NAME))
    } else {
        app.state
            .goal
            .record_external_usage(usage, Some(GOAL_COST), Billing::Api);
        0
    };
    app.state.context_size = TITLE_CONTEXT_SIZE;
    app.chats[chat_idx].context_size = TITLE_CONTEXT_SIZE;
    app.chats[chat_idx].context_window = CHAT_CONTEXT_WINDOW;
    let messages = app.chats[chat_idx].message_count();
    let event = AgentEvent::ModelUsage {
        usage,
        cost: Some(GOAL_COST),
        billing: Billing::Api,
        provider: OTHER_PROVIDER.into(),
        model: LEDGER_MODEL.into(),
        purpose: LedgerPurpose::ToolJsonRepair,
    };
    let message = if subagent {
        subagent_msg(event, TASK_ID, Some(RESEARCH_NAME))
    } else {
        agent_msg(event)
    };
    app.update(message);
    assert_eq!(app.state.token_usage, usage);
    assert_eq!(app.state.goal.snapshot().unwrap().usage, usage);
    assert_eq!(app.state.cost, Some(GOAL_COST));
    assert_eq!(app.chats[chat_idx].cost, Some(GOAL_COST));
    assert_eq!(app.state.context_size, TITLE_CONTEXT_SIZE);
    assert_eq!(app.chats[chat_idx].context_size, TITLE_CONTEXT_SIZE);
    assert_eq!(app.chats[chat_idx].context_window, CHAT_CONTEXT_WINDOW);
    assert_eq!(app.chats[chat_idx].message_count(), messages);
    assert_eq!(
        app.state
            .session
            .usage_by_model()
            .values()
            .filter_map(|usage| usage.cost)
            .sum::<f64>(),
        GOAL_COST
    );
    app.update(agent_msg_with_run_id(
        AgentEvent::Done {
            usage,
            num_turns: 0,
            reason: DoneReason::Cancelled,
        },
        app.run_id,
    ));
    assert_eq!(app.state.token_usage, usage);
    assert_eq!(app.state.cost, Some(GOAL_COST));
    drain_writer(app, writer);
    let rows = UsageLedger::open(&dir).unwrap().buckets(None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].purpose,
        LedgerPurpose::ToolJsonRepair.storage_name()
    );
    assert_eq!(rows[0].provider, OTHER_PROVIDER);
    assert_eq!(rows[0].model, LEDGER_MODEL);
    assert_eq!(rows[0].input, u64::from(GOAL_INPUT));
    assert_eq!(rows[0].cost, GOAL_COST);
}

const GAP_CALL: &str = "unrecorded-call";
const GAP_REASON: &str = "a workspace quota is exhausted";
const GAP_UNSAVED: &str = "a call that ran unrecorded must stay on the session it changed";

/// A gap outlives the run that hit it: a file revert of that session later
/// leaves the call's changes in place, and has to be able to say so.
#[test_case(true ; "the_first_gap_is_told")]
#[test_case(false ; "a_later_gap_is_only_kept")]
fn an_unrecorded_call_is_kept_with_its_session(notice: bool) {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    crate::push_history_message(
        app.state.session_mut(),
        Message::user(FIRST_TURN_PROMPT.into()),
    );
    let gap = UnrecordedCall {
        at: CaudraId::generate(),
        call_id: GAP_CALL.into(),
        reason: GAP_REASON.into(),
    };
    let told = format!("{UNRECORDED_MSG}: {GAP_REASON}");

    let actions = app.update(agent_msg(AgentEvent::Unrecorded {
        gap: gap.clone(),
        notice,
    }));

    assert!(actions.is_empty());
    assert_eq!(app.status_bar.flash_text(), notice.then_some(told.as_str()));
    app.checkpoint();
    let id = app.state.session.id;
    drain_writer(app, writer);
    assert_eq!(
        AppSession::load(id, &dir).unwrap().meta.unrecorded,
        [gap],
        "{GAP_UNSAVED}"
    );
}

/// The title request never enters the conversation, so its spend is recorded
/// while the context size it would otherwise report is ignored.
#[test]
fn title_spend_reaches_the_ledger_without_moving_the_context_size() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    app.chats[0].context_size = TITLE_CONTEXT_SIZE;
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::SessionTitle {
        title: Some(TITLE_TEXT.into()),
        usage: TokenUsage {
            input: GOAL_INPUT,
            output: GOAL_OUTPUT,
            ..Default::default()
        },
        cost: Some(GOAL_COST),
        billing: Billing::Api,
        model: LEDGER_MODEL.into(),
        provider: OTHER_PROVIDER.into(),
    }));
    assert_eq!(
        app.chats[0].context_size, TITLE_CONTEXT_SIZE,
        "a title is not in context and must not resize it"
    );
    assert_eq!(app.state.session.title, TITLE_TEXT);
    drain_writer(app, writer);

    let rows = UsageLedger::open(&dir).unwrap().buckets(None).unwrap();
    assert_eq!(rows.len(), 1, "{TITLE_SPEND_IS_RECORDED}");
    assert_eq!(
        rows[0].purpose,
        LedgerPurpose::Title.storage_name(),
        "{TITLE_SPEND_IS_RECORDED}"
    );
    assert_eq!(
        rows[0].provider, OTHER_PROVIDER,
        "{TITLE_SPEND_IS_RECORDED}"
    );
    assert_eq!(rows[0].cost, GOAL_COST, "{TITLE_SPEND_IS_RECORDED}");
}

/// An unusable answer was still billed, so the spend lands even though no
/// title does.
#[test]
fn an_unusable_title_still_records_what_it_cost() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let original = app.state.session.title.clone();
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::SessionTitle {
        title: None,
        usage: TokenUsage {
            input: GOAL_INPUT,
            ..Default::default()
        },
        cost: Some(GOAL_COST),
        billing: Billing::Api,
        model: LEDGER_MODEL.into(),
        provider: TEST_PROVIDER.into(),
    }));
    assert_eq!(app.state.session.title, original);
    drain_writer(app, writer);

    let rows = UsageLedger::open(&dir).unwrap().buckets(None).unwrap();
    assert_eq!(rows.len(), 1, "{TITLE_SPEND_IS_RECORDED}");
    assert_eq!(rows[0].cost, GOAL_COST, "{TITLE_SPEND_IS_RECORDED}");
}

#[test_case("anthropic", "claude-haiku", "anthropic/claude-haiku" ; "own_provider_is_still_named")]
#[test_case("openrouter", "vendor/model", "openrouter/vendor/model" ; "a_nested_model_id_gains_one_prefix")]
fn the_session_usage_key_always_names_its_provider(provider: &str, model: &str, expected: &str) {
    assert_eq!(session_usage_model(provider, model), expected);
}

#[test]
fn deferred_goal_builds_an_automatic_checkin() {
    let mut app = test_app();
    app.state.goal.set("background result reviewed").unwrap();
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::GoalDeferred {
        active_background_tasks: 1,
    }));
    app.update(done_event());

    assert!(app.goal_checkin_due());
    let actions = app.start_goal_checkin();
    let input = actions
        .iter()
        .find_map(|action| match action {
            Action::SendMessage(input) => Some(input),
            _ => None,
        })
        .expect("check-in should start a run");
    assert!(input.message.is_empty());
    assert!(input.preamble.iter().any(|message| {
        message
            .first_text_content()
            .is_some_and(|text| text.contains("Goal check-in"))
    }));
    assert!(!app.goal_checkin_due());
}

#[test]
fn active_goal_round_trips_through_session_metadata() {
    let continuation_limit = 24;
    let mut app = test_app();
    app.state.goal.set(GOAL_CONDITION).unwrap();
    app.state.goal.set_continuation_limit(continuation_limit);
    app.checkpoint_with(Duration::ZERO);
    assert_eq!(
        app.state
            .session
            .meta
            .active_goal
            .as_ref()
            .map(|goal| goal.condition.as_str()),
        Some(GOAL_CONDITION)
    );
    assert_eq!(
        app.state.session.meta.goal_continuation_limit,
        Some(continuation_limit)
    );

    let model = app.state.model.clone();
    let state = SessionState::from_session(
        Arc::unwrap_or_clone(app.state.session),
        &model,
        &app.storage,
        &app.model_policy,
    );
    assert_eq!(
        state.goal.snapshot().unwrap().condition.as_ref(),
        GOAL_CONDITION
    );
    assert_eq!(state.goal.continuation_limit(), continuation_limit);
}

/// The panel reports spend, evaluations, elapsed time and the latest reason.
/// A resume that dropped them would report a goal that had done nothing, so
/// the whole progress record has to survive a write and a read.
#[test]
fn a_resumed_active_goal_keeps_its_spend_evaluations_and_clock() {
    let mut app = test_app();
    app.state.session_mut().meta.active_goal = Some(Box::new(StoredActiveGoal {
        condition: GOAL_CONDITION.into(),
        evaluations: GOAL_EVALUATIONS,
        elapsed_ms: GOAL_ELAPSED_MS,
        usage: StoredTokenUsage {
            input: GOAL_INPUT,
            output: GOAL_OUTPUT,
            cost: Some(GOAL_COST),
            ..Default::default()
        },
        last_verdict: Some(StoredGoalVerdict::NotMet),
        last_reason: Some(GOAL_REASON.into()),
    }));

    let model = app.state.model.clone();
    let mut resumed = test_app();
    resumed.state = SessionState::from_session(
        Arc::unwrap_or_clone(app.state.session),
        &model,
        &resumed.storage,
        &resumed.model_policy,
    );

    let snapshot = resumed.state.goal.snapshot().expect(GOAL_PROGRESS_SURVIVES);
    assert_eq!(snapshot.condition.as_ref(), GOAL_CONDITION);
    assert_eq!(snapshot.usage.input, GOAL_INPUT, "{GOAL_PROGRESS_SURVIVES}");
    assert_eq!(snapshot.cost, Some(GOAL_COST), "{GOAL_PROGRESS_SURVIVES}");
    assert_eq!(
        snapshot.evaluations, GOAL_EVALUATIONS,
        "{GOAL_PROGRESS_SURVIVES}"
    );
    assert_eq!(snapshot.last_verdict, Some(GoalVerdict::NotMet));
    assert_eq!(snapshot.last_reason.as_deref(), Some(GOAL_REASON));
    assert!(
        snapshot.elapsed() >= Duration::from_millis(GOAL_ELAPSED_MS),
        "the clock must carry on from what was stored, not restart"
    );

    // Spend that lands after the resume adds to what was restored rather than
    // replacing it, and the next checkpoint keeps the sum.
    resumed.state.goal.record_external_usage(
        TokenUsage {
            input: GOAL_INPUT,
            ..Default::default()
        },
        Some(GOAL_COST),
        Billing::Api,
    );
    resumed.checkpoint_with(Duration::ZERO);

    let stored = resumed
        .state
        .session
        .meta
        .active_goal
        .clone()
        .expect(GOAL_PROGRESS_SURVIVES);
    assert_eq!(
        stored.usage.input,
        GOAL_INPUT * 2,
        "{GOAL_PROGRESS_SURVIVES}"
    );
    assert_eq!(
        stored.usage.cost,
        Some(GOAL_COST * 2.0),
        "{GOAL_PROGRESS_SURVIVES}"
    );
    assert_eq!(
        stored.evaluations, GOAL_EVALUATIONS,
        "{GOAL_PROGRESS_SURVIVES}"
    );
    assert_eq!(
        stored.last_verdict,
        Some(StoredGoalVerdict::NotMet),
        "an unmet verdict must not be stored as impossible"
    );
    assert_eq!(stored.last_reason.as_deref(), Some(GOAL_REASON));
    assert!(
        stored.elapsed_ms >= GOAL_ELAPSED_MS,
        "the stored clock must never go backwards across a resume"
    );
}

/// A resumed goal is status, not a trigger. Reopening a session must not spend
/// money on a turn the user did not ask for.
#[test]
fn resuming_an_active_goal_starts_no_work_on_its_own() {
    let mut app = test_app();
    app.state.goal.set(GOAL_CONDITION).unwrap();
    app.checkpoint_with(Duration::ZERO);

    let session = Arc::unwrap_or_clone(app.state.session.clone());
    let model = app.state.model.clone();
    let mut resumed = test_app();
    resumed.state =
        SessionState::from_session(session, &model, &resumed.storage, &resumed.model_policy);
    resumed.restore_resumed_session();

    assert!(resumed.state.goal.snapshot().is_some());
    assert_eq!(
        resumed.status,
        Status::Idle,
        "a resumed goal must wait for the next message"
    );
}

#[test]
fn new_session_clears_goal_and_restores_the_default_continuation_limit() {
    let mut app = test_app();
    app.state.goal.set("old session only").unwrap();
    app.state.goal.set_continuation_limit(24);

    app.reset_session();

    assert!(app.state.goal.status().is_none());
    assert_eq!(
        app.state.goal.continuation_limit(),
        caudra_agent::DEFAULT_GOAL_CONTINUATION_LIMIT
    );
    assert!(app.state.session.meta.active_goal.is_none());
}

#[test]
fn completed_goal_round_trips_through_session_metadata() {
    let mut app = test_app();
    app.state.goal.restore_finished(GoalResult {
        condition: Arc::from("persist result"),
        verdict: GoalVerdict::Met,
        reason: Arc::from("verified"),
        evaluations: 3,
        duration: Duration::from_secs(42),
        usage: TokenUsage {
            input: 100,
            output: 20,
            ..Default::default()
        },
        cost: Some(0.25),
        subscription_cost: None,
    });
    app.checkpoint_with(Duration::ZERO);
    assert!(app.state.session.meta.goal_result.is_some());

    let model = app.state.model.clone();
    let state = SessionState::from_session(
        Arc::unwrap_or_clone(app.state.session),
        &model,
        &app.storage,
        &app.model_policy,
    );
    let Some(GoalStatus::Finished(result)) = state.goal.status() else {
        panic!("completed goal was not restored");
    };
    assert_eq!(result.condition.as_ref(), "persist result");
    assert_eq!(result.reason.as_ref(), "verified");
    assert_eq!(result.cost, Some(0.25));
}

#[test]
fn ctrl_r_refreshes_usage_while_modal_open() {
    let mut app = test_app();
    app.execute_command(cmd("/usage"), 0);
    assert!(app.usage_modal.is_open());

    let actions = app.update(Msg::Key(kb::REFRESH.to_key_event()));
    assert!(
        actions.iter().any(|a| matches!(a, Action::RefreshUsage)),
        "Ctrl+R should emit RefreshUsage"
    );
    assert!(app.usage_modal.is_open(), "modal should stay open");
}

#[test]
fn cd_command_behavior() {
    let mut app = test_app();
    let original_cwd = app.state.session.cwd.clone();
    let actions = app.execute_command(
        ParsedCommand {
            name: "/cd".into(),
            args: "/tmp".into(),
        },
        0,
    );
    let [Action::ChangeWorkingDirectory(resolved)] = actions.as_slice() else {
        panic!("expected process-wide cwd action");
    };
    assert_eq!(resolved, &std::fs::canonicalize("/tmp").unwrap());
    assert_eq!(app.state.session.cwd, original_cwd);

    app.execute_command(
        ParsedCommand {
            name: "/cd".into(),
            args: "/nonexistent_path_12345".into(),
        },
        0,
    );
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.starts_with("cd: "), "error flash={flash:?}");
}

pub(super) fn remote_workspace_session() -> caudra_workspace::WorkspaceSession {
    let authority = caudra_workspace::AuthorityIdentity::new(
        caudra_workspace::SourceTrustAnchor::new("test-source").unwrap(),
        "authority",
        "workspace",
        "generation",
        "namespace",
    )
    .unwrap();
    let binding = caudra_workspace::SessionWorkspaceBinding::new(
        caudra_workspace::SessionBindingId::new("tab").unwrap(),
        authority.clone(),
        caudra_workspace::AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap(),
        caudra_workspace::ProjectIdentity::new(
            authority.clone(),
            caudra_workspace::ProjectKey::new("project").unwrap(),
        ),
    )
    .unwrap();
    let cursor = caudra_workspace::WorkspaceCursor::new(
        &binding,
        caudra_workspace::ResourceScope::root(caudra_workspace::ResourceId::new("root").unwrap()),
        1,
        caudra_workspace::CwdHandle::new("remote-cwd").unwrap(),
    );
    let handle = caudra_workspace::WorkspaceHandle::new(
        authority,
        caudra_workspace::WorkspaceCapabilities::default(),
        caudra_workspace::WorkspaceServices::default(),
    )
    .unwrap();
    caudra_workspace::WorkspaceSession::new(handle, binding, cursor).unwrap()
}

struct EmptyRemoteAssets;

#[derive(Default)]
struct TestRemoteControl(std::sync::Mutex<Vec<caudra_workspace::WorkspaceControlCommand>>);

#[async_trait::async_trait]
impl caudra_workspace::WorkspaceControlService for TestRemoteControl {
    async fn execute(
        &self,
        command: caudra_workspace::WorkspaceControlCommand,
    ) -> Result<String, WorkspaceError> {
        self.0.lock().unwrap().push(command);
        Ok("Remote control response".to_owned())
    }
}

#[test_case("status", true; "status")]
#[test_case("pending", true; "pending")]
#[test_case("reconnect", true; "reconnect")]
#[test_case("reconcile", true; "reconcile")]
#[test_case("acknowledge operation", false; "confirmation_required")]
#[test_case("acknowledge operation --accept-possible-effects", true; "confirmed")]
fn remote_ui_command_uses_the_dedicated_controller(args: &str, dispatched: bool) {
    let mut app = test_app();
    let original = remote_workspace_session();
    let control = Arc::new(TestRemoteControl::default());
    app.workspace_session = Some(
        WorkspaceSession::new(
            WorkspaceHandle::new(
                original.binding().authority().clone(),
                WorkspaceCapabilities::default(),
                WorkspaceServices {
                    control: Some(control.clone()),
                    ..Default::default()
                },
            )
            .unwrap(),
            original.binding().clone(),
            original.cursor().clone(),
        )
        .unwrap(),
    );
    let actions = app.execute_command(
        ParsedCommand {
            name: "/remote".into(),
            args: args.into(),
        },
        0,
    );
    let [Action::RemoteControl(args)] = actions.as_slice() else {
        panic!("expected dedicated control action");
    };
    let (tx, rx) = flume::bounded(1);
    super::shell::spawn_remote_control(app.workspace_session.clone(), args.clone(), tx);
    app.handle_shell_event(smol::block_on(rx.recv_async()).unwrap());
    assert_eq!(!control.0.lock().unwrap().is_empty(), dispatched);
}

#[async_trait::async_trait]
impl WorkspaceAssetService for EmptyRemoteAssets {
    async fn discover(
        &self,
        _: &SessionWorkspaceBinding,
        _: &WorkspaceCursor,
    ) -> Result<ProjectAssetManifest, WorkspaceError> {
        Ok(ProjectAssetManifest {
            version: OperationId::new("project-assets.v1").unwrap(),
            revision: CollectionRevision::new("empty").unwrap(),
            assets: Vec::new(),
            unreadable: Vec::new(),
        })
    }

    async fn read(
        &self,
        _: &SessionWorkspaceBinding,
        _: &WorkspaceCursor,
        _: &ProjectAsset,
        _: u32,
    ) -> Result<ProjectAssetContent, WorkspaceError> {
        Err(WorkspaceError::Unavailable)
    }
}

#[test]
fn remote_cd_persistence_failure_preserves_live_state() {
    const SAVE_FAILED: &str = "cd: session persistence failed; directory was not changed";
    let mut app = test_app();
    let original = remote_workspace_session();
    let workspace = WorkspaceSession::new(
        WorkspaceHandle::new(
            original.binding().authority().clone(),
            WorkspaceCapabilities::default(),
            WorkspaceServices {
                assets: Some(Arc::new(EmptyRemoteAssets)),
                ..Default::default()
            },
        )
        .unwrap(),
        original.binding().clone(),
        original.cursor().clone(),
    )
    .unwrap();
    let binding = StoredWorkspaceBinding::new_with_cursor(
        workspace.binding().clone(),
        workspace.cursor().clone(),
        None,
    )
    .unwrap();
    app.state.session = Arc::new(AppSession::new_with_workspace("test", ".", binding.clone()));
    app.workspace_session = Some(workspace.clone());
    let old_session = Arc::clone(&app.state.session);
    let old_context = smol::block_on(
        caudra_agent::remote_project_context::load_remote_project_context(
            &workspace,
            FeatureFlags::all(),
        ),
    )
    .unwrap();
    app.remote_project_context = Some(Arc::clone(&old_context));
    let old_policy: Vec<_> = app
        .permissions
        .active_policy()
        .into_iter()
        .map(|entry| (entry.origin, entry.rule))
        .collect();
    let temp = TempDir::new().unwrap();
    let blocked = temp.path().join("not-a-directory");
    std::fs::write(&blocked, b"blocked").unwrap();
    app.storage_writer = Arc::new(test_writer(StateDir::from_path(blocked)));
    let candidate_cursor = WorkspaceCursor::new(
        workspace.binding(),
        workspace.cursor().scope().clone(),
        workspace.cursor().generation(),
        caudra_workspace::CwdHandle::new("candidate").unwrap(),
    );
    let binding = binding.with_cursor(candidate_cursor.clone()).unwrap();
    let workspace = WorkspaceSession::new(
        workspace.workspace().clone(),
        workspace.binding().clone(),
        candidate_cursor,
    )
    .unwrap();
    let error = app
        .install_remote_working_directory(super::shell::RemoteDirectoryChange {
            workspace,
            binding,
            context: Arc::clone(&old_context),
            display_path: "nested".into(),
        })
        .unwrap_err();
    assert_eq!(error, SAVE_FAILED);
    assert!(Arc::ptr_eq(&app.state.session, &old_session));
    assert!(Arc::ptr_eq(
        app.remote_project_context.as_ref().unwrap(),
        &old_context
    ));
    assert_eq!(
        app.workspace_session.as_ref().unwrap().cursor(),
        original.cursor()
    );
    assert_eq!(
        app.permissions
            .active_policy()
            .into_iter()
            .map(|entry| (entry.origin, entry.rule))
            .collect::<Vec<_>>(),
        old_policy
    );
}

#[test]
fn remote_cd_never_resolves_a_same_named_local_directory() {
    let directory = TempDir::new().unwrap();
    std::fs::create_dir(directory.path().join("canary")).unwrap();
    let mut app = test_app();
    app.workspace_session = Some(remote_workspace_session());
    let original_cwd = app.state.session.cwd.clone();

    let actions = app.execute_command(
        ParsedCommand {
            name: "/cd".into(),
            args: "canary".into(),
        },
        0,
    );

    assert!(matches!(
        actions.as_slice(),
        [Action::ChangeRemoteWorkingDirectory(path)] if path.as_str() == "canary"
    ));
    assert_eq!(app.state.session.cwd, original_cwd);
    assert!(directory.path().join("canary").is_dir());
}

#[test]
fn remote_cd_defers_parent_navigation_to_the_server_without_changing_the_cursor() {
    let mut app = test_app();
    let workspace = remote_workspace_session();
    let original_cursor = workspace.cursor().clone();
    app.workspace_session = Some(workspace);

    let actions = app.execute_command(
        ParsedCommand {
            name: "/cd".into(),
            args: "../outside".into(),
        },
        0,
    );

    assert!(
        matches!(actions.as_slice(), [Action::ChangeRemoteWorkingDirectory(path)] if path.as_str() == "../outside")
    );
    assert_eq!(
        app.workspace_session.as_ref().unwrap().cursor(),
        &original_cursor
    );
}

#[test]
fn cd_lifecycle_guard_is_deferred_to_the_multi_session_event_loop() {
    let mut cancelling = test_app();
    let cancelling_cwd = cancelling.state.session.cwd.clone();
    cancelling.status = Status::Streaming;
    cancelling.run_id = 1;
    cancelling.handle_cancel();

    let actions = cancelling.execute_command(
        ParsedCommand {
            name: "/cd".into(),
            args: "/".into(),
        },
        0,
    );

    assert_eq!(cancelling.state.session.cwd, cancelling_cwd);
    assert!(matches!(
        actions.as_slice(),
        [Action::ChangeWorkingDirectory(_)]
    ));

    let mut reverted = build_rewind_app();
    reverted.rewind_to(rewind_to_second_turn());
    let reverted_cwd = reverted.state.session.cwd.clone();
    let actions = reverted.execute_command(
        ParsedCommand {
            name: "/cd".into(),
            args: "/".into(),
        },
        0,
    );

    assert_eq!(reverted.state.session.cwd, reverted_cwd);
    assert!(matches!(
        actions.as_slice(),
        [Action::ChangeWorkingDirectory(_)]
    ));
}

#[test]
fn typed_slash_command_executes() {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, "/help");
    assert!(actions.is_empty());
    assert!(app.help_modal.is_open());
}

#[test_case(CONTEXT_COMMAND, Some(false), None ; "summary")]
#[test_case(CONTEXT_UPPERCASE_COMMAND, Some(false), None ; "uppercase_command")]
#[test_case(CONTEXT_TRAILING_COMMAND, Some(false), None ; "summary_trailing_whitespace")]
#[test_case(CONTEXT_ALL_COMMAND, Some(true), None ; "all")]
#[test_case(CONTEXT_ALL_UPPERCASE_COMMAND, Some(true), None ; "case_insensitive_all")]
#[test_case(CONTEXT_ALL_TRAILING_COMMAND, Some(true), None ; "all_trailing_whitespace")]
#[test_case(CONTEXT_INVALID_COMMAND, None, Some(CONTEXT_USAGE) ; "invalid_args")]
#[test_case(CONTEXT_EXCESS_ARGS_COMMAND, None, Some(CONTEXT_USAGE) ; "excess_args")]
fn context_command_is_local_and_does_not_mutate_the_chat(
    command: &str,
    expected_expanded: Option<bool>,
    expected_flash: Option<&str>,
) {
    let mut app = test_app();
    crate::push_history_message(
        app.state.session_mut(),
        Message::user(CONTEXT_EXISTING_MESSAGE.into()),
    );
    app.restore_display();
    let history_before = app.state.session.messages().to_vec();
    let display_count_before = app.main_chat().message_count();
    let display_text_before = app.main_chat().last_message_text().to_owned();

    let actions = type_and_submit(&mut app, command);

    assert!(actions.is_empty());
    assert_eq!(app.state.session.messages(), history_before.as_slice());
    assert_eq!(app.main_chat().message_count(), display_count_before);
    assert_eq!(app.main_chat().last_message_text(), display_text_before);
    assert_eq!(app.status_bar.flash_text(), expected_flash);
    let Some(expanded) = expected_expanded else {
        assert!(!app.context_modal.is_open());
        return;
    };
    assert!(app.context_modal.is_open());
    let frame = rendered(&mut app);
    let expected_title = if expanded {
        CONTEXT_EXPANDED_TITLE
    } else {
        CONTEXT_TITLE
    };
    assert!(frame.contains(expected_title.trim()), "frame={frame:?}");
    assert_eq!(
        frame.contains(CONTEXT_EXPANDED_TITLE.trim()),
        expanded,
        "frame={frame:?}"
    );
}

/// Opening must ask for a measurement, not take one: the walk that sizes the
/// change record stores is far too slow to run on the command's own frame.
#[test_case(STORAGE_COMMAND, Some(false), None ; "summary")]
#[test_case(STORAGE_ALL_COMMAND, Some(true), None ; "case_insensitive_all")]
#[test_case(STORAGE_INVALID_COMMAND, None, Some(STORAGE_USAGE) ; "invalid_args")]
fn storage_command_opens_the_modal_and_asks_for_a_measurement(
    command: &str,
    expected_expanded: Option<bool>,
    expected_flash: Option<&str>,
) {
    let mut app = test_app();

    let actions = type_and_submit(&mut app, command);

    assert_eq!(app.status_bar.flash_text(), expected_flash);
    let Some(expanded) = expected_expanded else {
        assert!(!app.storage_modal.is_open());
        assert!(actions.is_empty());
        return;
    };
    assert!(app.storage_modal.is_open());
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, Action::RefreshStorage)),
        "{MISSING_STORAGE_REFRESH}"
    );
    let frame = rendered(&mut app);
    let expected_title = if expanded {
        STORAGE_EXPANDED_TITLE
    } else {
        STORAGE_TITLE
    };
    assert!(frame.contains(expected_title.trim()), "frame={frame:?}");
    assert_eq!(
        frame.contains(STORAGE_EXPANDED_TITLE.trim()),
        expanded,
        "frame={frame:?}"
    );
}

#[test_case(CONTEXT_COMMAND, true, None ; "summary")]
#[test_case(CONTEXT_UPPERCASE_COMMAND, true, None ; "uppercase_command")]
#[test_case(CONTEXT_ALL_TRAILING_COMMAND, true, None ; "all_trailing_whitespace")]
#[test_case(CONTEXT_EXCESS_ARGS_COMMAND, false, Some(CONTEXT_USAGE) ; "excess_args")]
fn focused_task_context_command_is_local(
    command: &str,
    expected_open: bool,
    expected_flash: Option<&str>,
) {
    let mut app = steerable_task_app();

    let actions = type_and_submit(&mut app, command);

    assert!(actions.is_empty());
    assert!(app.subagent_steers[TASK_ID].entries().is_empty());
    assert_eq!(app.context_modal.is_open(), expected_open);
    assert_eq!(app.status_bar.flash_text(), expected_flash);
}

#[test_case(TOOLS_COMMAND, true, None ; "summary")]
#[test_case(TOOLS_UPPERCASE_COMMAND, true, None ; "uppercase_command")]
#[test_case(TOOLS_EXCESS_ARGS_COMMAND, false, Some(TOOLS_USAGE) ; "excess_args")]
fn tools_command_is_local_in_both_the_main_chat_and_a_task(
    command: &str,
    expected_open: bool,
    expected_flash: Option<&str>,
) {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, command);
    assert!(actions.is_empty());
    assert_eq!(app.tools_modal.is_open(), expected_open);
    assert_eq!(app.status_bar.flash_text(), expected_flash);

    let mut task = steerable_task_app();
    let actions = type_and_submit(&mut task, command);
    assert!(actions.is_empty());
    assert!(task.subagent_steers[TASK_ID].entries().is_empty());
    assert_eq!(task.tools_modal.is_open(), expected_open);
}

#[test_case(SKILLS_COMMAND, true, None ; "summary")]
#[test_case(SKILLS_EXCESS_ARGS_COMMAND, false, Some(SKILLS_USAGE) ; "excess_args")]
fn skills_command_is_local_in_both_the_main_chat_and_a_task(
    command: &str,
    expected_open: bool,
    expected_flash: Option<&str>,
) {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, command);
    assert!(actions.is_empty());
    assert_eq!(app.skills_modal.is_open(), expected_open);
    assert_eq!(app.status_bar.flash_text(), expected_flash);

    let mut task = steerable_task_app();
    let actions = type_and_submit(&mut task, command);
    assert!(actions.is_empty());
    assert!(task.subagent_steers[TASK_ID].entries().is_empty());
    assert_eq!(task.skills_modal.is_open(), expected_open);
}

#[test]
fn toolsmith_is_not_intercepted_as_the_tools_command() {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, TOOLSMITH_PROMPT);
    assert!(matches!(
        actions.as_slice(),
        [Action::SendMessage(input)] if input.message == TOOLSMITH_PROMPT
    ));
    assert!(!app.tools_modal.is_open());
}

#[test]
fn contextual_is_not_intercepted_as_the_context_command() {
    let mut main = test_app();
    let actions = type_and_submit(&mut main, CONTEXTUAL_PROMPT);
    assert!(matches!(
        actions.as_slice(),
        [Action::SendMessage(input)] if input.message == CONTEXTUAL_PROMPT
    ));
    assert!(!main.context_modal.is_open());

    let mut task = steerable_task_app();
    let actions = type_and_submit(&mut task, CONTEXTUAL_PROMPT);
    assert!(actions.is_empty());
    let steers = task.subagent_steers[TASK_ID].entries();
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0].text, CONTEXTUAL_PROMPT);
    assert!(!task.context_modal.is_open());
}

#[test]
fn permission_request_closes_context_modal_before_taking_input() {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, CONTEXT_COMMAND);
    assert!(actions.is_empty());
    assert!(app.context_modal.is_open());
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(permission_event("request", "cargo check")));

    assert!(!app.context_modal.is_open());
    assert!(app.permission_prompt.is_open());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.permission_prompt.is_open());
}

#[test]
fn context_footer_click_switches_views() {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, CONTEXT_COMMAND);
    assert!(actions.is_empty());
    let _ = rendered(&mut app);
    let hit = app.context_modal.footer_hit();
    assert!(!hit.is_empty());

    app.update(mouse_event(MouseEventKind::Moved, hit.x, hit.y));
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        hit.x,
        hit.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        hit.x,
        hit.y,
    ));

    let frame = rendered(&mut app);
    assert!(
        frame.contains(CONTEXT_EXPANDED_TITLE.trim()),
        "frame={frame:?}"
    );
}

const GOAL_MODEL_COMMAND: &str = "/goal-model";
const GOAL_CLEAR_COMMAND: &str = "/goal-clear";

fn click_goal_footer(app: &mut App, command: &'static str) -> Vec<Action> {
    let _ = rendered(app);
    let hit = app.goal_modal.footer_hit(GoalTarget::Command(command));
    assert!(!hit.is_empty(), "command={command}");
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        hit.x,
        hit.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        hit.x,
        hit.y,
    ))
}

/// The picker is drawn under the goal modal and outranks it in `dismiss_at`, so
/// a footer click that left the modal standing would hide the very thing it
/// opened.
#[test]
fn goal_footer_click_opens_the_model_picker() {
    let mut app = test_app();
    app.state.goal.set(GOAL_CONDITION).unwrap();
    app.goal_modal.open();

    let actions = click_goal_footer(&mut app, GOAL_MODEL_COMMAND);

    assert!(app.model_picker.is_open());
    assert!(!app.goal_modal.is_open());
    assert!(matches!(&actions[..], [Action::RefreshModels]));
}

#[test]
fn goal_footer_click_clears_the_goal() {
    let mut app = test_app();
    app.state.goal.set(GOAL_CONDITION).unwrap();
    app.goal_modal.open();

    let actions = click_goal_footer(&mut app, GOAL_CLEAR_COMMAND);

    assert!(actions.is_empty());
    assert!(app.state.goal.snapshot().is_none());
    assert!(!app.goal_modal.is_open());
    assert_eq!(
        app.status_bar.flash_text(),
        Some(format!("Goal cleared: {GOAL_CONDITION}").as_str())
    );
}

/// A goal that is over cannot be stopped, so the footer offers no way to try.
#[test]
fn a_finished_goal_footer_has_no_clear() {
    let mut app = test_app();
    app.goal_modal.open();
    let _ = rendered(&mut app);

    assert!(
        app.goal_modal
            .footer_hit(GoalTarget::Command(GOAL_CLEAR_COMMAND))
            .is_empty()
    );
    assert!(
        !app.goal_modal
            .footer_hit(GoalTarget::Command(GOAL_MODEL_COMMAND))
            .is_empty()
    );
}

const LUA_COMMAND_RAN: &str = "lua command with args must reach the plugin";
const LUA_COMMAND_NOT_SENT: &str = "lua command with args must not reach the model";

/// The palette hides a lua command once the typed words pass its `max_args`,
/// and a hidden command falls through to `handle_submit`, so a multi word
/// `nargs` command must still be routed to its plugin.
#[test]
fn typed_lua_command_with_args_executes() {
    let dir = test_state_dir();
    let mut app = build_app_with_lua(
        dir.clone(),
        Arc::new(test_writer(dir)),
        LuaCommandReader::from_commands(vec![LuaCommandInfo {
            name: "/deploy".into(),
            description: "Deploy the project".into(),
            plugin: "deploy_plugin".into(),
            max_args: usize::MAX,
        }]),
    );
    let (handle, probe) = caudra_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;

    let actions = type_and_submit(&mut app, "/deploy staging now");

    assert!(actions.is_empty(), "{LUA_COMMAND_NOT_SENT}");
    assert!(probe.try_recv().is_some(), "{LUA_COMMAND_RAN}");
}

const RUN_CMDLINE_REJECTED: &str = "a rejected cmdline must not run anything";

#[test_case("/new" ; "plain")]
#[test_case("/NEW" ; "uppercase")]
#[test_case("  /new  " ; "surrounding_whitespace")]
#[test_case("new" ; "missing_slash")]
fn run_cmdline_executes_builtin(cmdline: &str) {
    let mut app = test_app();

    let actions = app.run_cmdline(cmdline, 0).unwrap();

    assert!(matches!(&actions[..], [Action::RequestNewSession]));
}

#[test]
fn run_cmdline_splits_args_off_the_name() {
    let mut app = test_app();

    let actions = app.run_cmdline("/btw what is rust?", 0).unwrap();

    assert!(matches!(&actions[..], [Action::Btw(q)] if q == "what is rust?"));
}

/// Only the typed path clears the input, so a keybind or autocmd reaching for
/// `run_command` cannot eat a half-written message.
#[test]
fn run_cmdline_keeps_typed_input() {
    let mut app = test_app();
    app.input_box.set_input("half written".into());

    app.run_cmdline("/usage", 0).unwrap();

    assert_eq!(app.input_box.buffer.value(), "half written");
}

#[test]
fn run_cmdline_unknown_name_errors_without_dispatching() {
    let mut app = test_app();
    let (handle, probe) = caudra_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;

    let Err(err) = app.run_cmdline("/nope", 0) else {
        panic!("{RUN_CMDLINE_REJECTED}");
    };

    assert!(err.contains("/nope"), "err={err:?}");
    assert!(probe.try_recv_command().is_none(), "{RUN_CMDLINE_REJECTED}");
}

#[test]
fn run_cmdline_rejects_past_max_depth() {
    let mut app = test_app();

    let Err(err) = app.run_cmdline("/new", crate::app::MAX_COMMAND_DEPTH + 1) else {
        panic!("{RUN_CMDLINE_REJECTED}");
    };

    assert_eq!(err, crate::app::COMMAND_DEPTH_MSG);
    assert!(
        app.run_cmdline("/new", crate::app::MAX_COMMAND_DEPTH)
            .is_ok(),
        "the cap itself must still run"
    );
}

/// A Lua command reached through an alias carries the hop count onward, or a
/// cycle of Lua aliases would never trip the cap. It goes out spelled as
/// registered, since only that spelling dispatches.
#[test]
fn run_cmdline_forwards_depth_to_lua_command() {
    let dir = test_state_dir();
    let mut app = build_app_with_lua(
        dir.clone(),
        Arc::new(test_writer(dir)),
        LuaCommandReader::from_commands(vec![LuaCommandInfo {
            name: "/Deploy".into(),
            description: "Deploy the project".into(),
            plugin: "deploy_plugin".into(),
            max_args: 0,
        }]),
    );
    let (handle, probe) = caudra_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;

    app.run_cmdline("/deploy", 3).unwrap();

    assert_eq!(
        probe.try_recv_command(),
        Some(("/Deploy".to_string(), String::new(), 3))
    );
}

#[test]
fn slash_noncommand_sends_as_prompt() {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, "/nonexistent");
    assert!(app.status_bar.flash_text().is_none());
    assert!(actions.iter().any(|a| matches!(a, Action::SendMessage(..))));
}

fn build_rewind_app() -> App {
    let mut app = test_app();

    app.state
        .session_mut()
        .replace_messages(crate::history_items(&[
            Message::user("first prompt".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "response 1".into(),
                    },
                    ContentBlock::tool_use("tool-1", "bash", serde_json::json!({})),
                ],
                ..Default::default()
            },
            Message::user("second prompt".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "response 2".into(),
                }],
                ..Default::default()
            },
            Message::user("third prompt".into()),
        ]));
    app.state
        .session_mut()
        .insert_tool_output("tool-1".into(), ToolOutput::Plain("output".into()));
    app
}

fn rewind_to_second_turn() -> RewindEntry {
    RewindEntry {
        turn_index: 3,
        prompt_preview: "2: second".into(),
    }
}

#[test]
fn rewind_to_middle_stages_head_move_and_populates_input() {
    let mut app = build_rewind_app();
    let old_run_id = app.run_id;
    let original_head = crate::session_history_head(&app.state.session);
    let actions = app.rewind_to(rewind_to_second_turn());

    assert_eq!(app.state.session.messages().len(), 6);
    assert_eq!(
        crate::active_session_history(&app.state.session)
            .unwrap()
            .len(),
        3
    );
    assert!(app.state.session.tool_outputs().contains_key("tool-1"));
    assert_eq!(app.input_box.buffer.value(), "second prompt");
    assert_eq!(app.run_id, old_run_id);
    let pending = app.state.session.meta.pending_revert.as_ref().unwrap();
    assert_eq!(pending.original_head, original_head);
    assert_eq!(pending.target_head, app.state.session.meta.history_head);

    let Action::LoadSession(ref loaded) = actions[0] else {
        panic!("expected LoadSession");
    };
    assert_eq!(loaded.messages.len(), 3);
}

#[test]
fn rewind_picker_defers_restore_to_event_loop_quiescence_dispatch() {
    let mut app = build_rewind_app();
    app.open_rewind_picker();

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(matches!(actions.as_slice(), [Action::RewindSession(_)]));
    assert!(app.state.session.meta.pending_revert.is_none());
}

/// Dropping two short messages may shave a few tokens off the gauge, never the
/// baseline underneath it. A session that never ran a turn has no baseline, so
/// there the rough estimate is all we get.
#[test_case(MEASURED_CONTEXT, MEASURED_CONTEXT - SMALL_HISTORY ; "keeps_measured_baseline")]
#[test_case(0,                0                                ; "falls_back_to_estimate")]
fn rewind_recomputes_context_size(measured: u32, floor: u32) {
    let mut app = build_rewind_app();
    app.state.context_size = measured;
    app.rewind_to(rewind_to_second_turn());

    let size = app.state.context_size;
    assert!(
        size > floor && size < floor + SMALL_HISTORY,
        "context {size} left the {floor}..{} window",
        floor + SMALL_HISTORY
    );
    assert_eq!(app.chats[0].context_size, size);
}

#[test]
fn rewind_to_first_turn_selects_empty_root_without_deleting_state() {
    let mut app = build_rewind_app();
    app.state.context_size = MEASURED_CONTEXT;
    app.state.token_usage.input = 500;
    app.state.token_usage.output = 200;
    let entry = RewindEntry {
        turn_index: 0,
        prompt_preview: "1: first".into(),
    };
    let actions = app.rewind_to(entry);

    assert_eq!(app.state.session.messages().len(), 6);
    assert!(
        crate::active_session_history(&app.state.session)
            .unwrap()
            .is_empty()
    );
    assert!(app.state.session.tool_outputs().contains_key("tool-1"));
    assert_eq!(app.state.token_usage.input, 500);
    assert_eq!(app.state.token_usage.output, 200);
    assert_eq!(app.state.context_size, 0);
    assert_eq!(app.chats[0].context_size, 0);
    assert!(matches!(&actions[0], Action::LoadSession(_)));
}

/// The reminders a turn is given trail it, so cutting the turn cuts them too.
/// Leaving them behind opened the transcript on two orphaned blocks, put the
/// next message underneath them, and let the stale announcements suppress the
/// fresh ones the new turn was owed.
#[test]
fn reverting_the_first_turn_takes_its_reminders_with_it() {
    const ORPHANED_MSG: &str = "a reverted turn must leave no reminder behind";
    let mut app = test_app();
    let items = crate::history_items(&[
        Message::user("first prompt".into()),
        Message::observation(caudra_agent::prompt::ENVIRONMENT_MARKER.into()),
        Message::observation(caudra_agent::prompt::PLAN_MODE_MARKER.into()),
        assistant_message("response"),
    ]);
    let first_user = items[0].id;
    app.state.session_mut().replace_messages(items);

    app.revert_to(first_user, RestoreMode::Conversation);

    assert!(
        crate::active_session_history(&app.state.session)
            .unwrap()
            .is_empty(),
        "{ORPHANED_MSG}"
    );
    assert_eq!(app.input_box.buffer.value(), "first prompt");
}

/// The other half of the same rule. A shell result landed before the prompt and
/// nothing else still holds it, so the rewind has to spare it; the environment
/// is re-sent whenever the transcript lacks it, so the rewind takes it.
#[test]
fn reverting_a_turn_keeps_the_arrivals_that_preceded_it() {
    const KEPT_MSG: &str = "an arrival is the only copy left, so a rewind must spare it";
    const SHELL_RESULT: &str = "I ran: $ ls\n\nOutput:\na.rs";
    let mut app = test_app();
    let items = crate::history_items(&[
        Message::user(SHELL_RESULT.into()),
        Message::user("first prompt".into()),
        Message::observation(caudra_agent::prompt::ENVIRONMENT_MARKER.into()),
        assistant_message("response"),
    ]);
    let prompt = items[1].id;
    app.state.session_mut().replace_messages(items);

    app.revert_to(prompt, RestoreMode::Conversation);

    let active = crate::active_session_history(&app.state.session).unwrap();
    let texts: Vec<_> = active
        .iter()
        .filter_map(|item| match &item.kind {
            caudra_providers::HistoryItemKind::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, [SHELL_RESULT], "{KEPT_MSG}");
    assert_eq!(app.input_box.buffer.value(), "first prompt");
}

#[test]
fn unrevert_restores_exact_original_head_and_clears_the_rewound_draft() {
    let mut app = build_rewind_app();
    let original_head = crate::session_history_head(&app.state.session);
    let expected = crate::active_session_history(&app.state.session).unwrap();
    app.rewind_to(rewind_to_second_turn());

    let actions = app.unrevert();

    assert_eq!(
        crate::session_history_head(&app.state.session),
        original_head
    );
    assert_eq!(
        crate::active_session_history(&app.state.session).unwrap(),
        expected
    );
    assert_eq!(app.state.session.meta.pending_revert, None);
    assert!(app.input_box.buffer.value().is_empty());
    let Action::LoadSession(loaded) = &actions[0] else {
        panic!("expected LoadSession");
    };
    assert_eq!(loaded.messages, expected);
}

fn assistant_message(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text { text: text.into() }],
        ..Default::default()
    }
}

/// A route that never answers, for side requests a test settles by hand.
struct PendingProvider;

impl caudra_providers::provider::Provider for PendingProvider {
    fn stream_message<'a>(
        &'a self,
        _: &'a caudra_providers::Model,
        _: &'a [Message],
        _: &'a str,
        _: &'a serde_json::Value,
        _: &'a flume::Sender<caudra_providers::ProviderEvent>,
        _: caudra_providers::RequestOptions,
        _: Option<&'a caudra_providers::CacheKey>,
    ) -> caudra_providers::provider::BoxFuture<
        'a,
        Result<caudra_providers::StreamResponse, caudra_providers::AgentError>,
    > {
        Box::pin(std::future::pending())
    }

    fn list_models(
        &self,
    ) -> caudra_providers::provider::BoxFuture<
        '_,
        Result<Vec<caudra_providers::ModelInfo>, caudra_providers::AgentError>,
    > {
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[test]
fn user_revert_restores_display_text_and_images_as_draft() {
    let mut app = test_app();
    let image = ImageSource::new(ImageMediaType::Png, Arc::from("dGVzdA=="));
    let items = crate::history_items(&[Message::user_display_with_images(
        "expanded prompt".into(),
        "typed prompt".into(),
        vec![image.clone()],
    )]);
    let user_id = items[0].id;
    app.state.session_mut().replace_messages(items);

    app.revert_to(user_id, RestoreMode::Conversation);
    let submission = app.input_box.submit().unwrap();

    assert_eq!(submission.text, "typed prompt");
    assert_eq!(submission.images, [image]);
}

#[test_case(true  ; "completed_call_targets_result")]
#[test_case(false ; "incomplete_call_targets_call")]
fn tool_revert_uses_last_atomic_completed_item(completed: bool) {
    let mut app = test_app();
    let mut messages = vec![
        Message::user("run tool".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "call-1",
                "read",
                serde_json::json!({}),
            )],
            ..Default::default()
        },
    ];
    if completed {
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call-1".into(),
                content: "result".into(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        });
    }
    let items = crate::history_items(&messages);
    let call_id = items
        .iter()
        .find(|item| matches!(item.kind, HistoryItemKind::ToolCall { .. }))
        .unwrap()
        .id;
    let expected_head = items.last().unwrap().id;
    app.state.session_mut().replace_messages(items);

    app.revert_to(call_id, RestoreMode::Conversation);

    assert_eq!(
        crate::session_history_head(&app.state.session),
        Some(expected_head)
    );
}

#[test]
fn fork_and_revert_through_parallel_result_include_the_whole_result_group() {
    let (_temp, _, _, mut app) = tempdir_app();
    let items = crate::history_items(&[
        Message::user("run tools".into()),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::tool_use("call-1", "read", serde_json::json!({})),
                ContentBlock::tool_use("call-2", "read", serde_json::json!({})),
            ],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "one".into(),
                    is_error: false,
                    output_ref: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call-2".into(),
                    content: "two".into(),
                    is_error: false,
                    output_ref: None,
                },
            ],
            ..Default::default()
        },
    ]);
    let first_result = items[3].id;
    let last_result = items[4].id;
    app.state.session_mut().replace_messages(items.clone());

    let forked = app
        .fork_at(DisplaySource::ToolResult(first_result))
        .unwrap();
    assert_eq!(forked.session.messages(), items);
    assert_eq!(
        project_messages(forked.session.messages()).unwrap()[2]
            .content
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
            .count(),
        2
    );

    app.revert_to(first_result, RestoreMode::Conversation);
    assert_eq!(
        crate::session_history_head(&app.state.session),
        Some(last_result)
    );
    assert_eq!(
        project_messages(&crate::active_session_history(&app.state.session).unwrap()).unwrap()[2]
            .content
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
            .count(),
        2
    );
}

#[test]
fn fork_targets_every_display_source_with_user_before_and_other_items_inclusive() {
    let (_temp, _, _, mut app) = tempdir_app();
    let image = ImageSource::new(ImageMediaType::Png, Arc::from("aW1hZ2U="));
    let items = crate::history_items(&[
        Message::user_display_with_images(
            "expanded".into(),
            "editable".into(),
            vec![image.clone()],
        ),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::thinking("reason".into(), None),
                ContentBlock::Text {
                    text: "answer".into(),
                },
                ContentBlock::tool_use("call-1", "read", serde_json::json!({})),
            ],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call-1".into(),
                content: "result".into(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        },
    ]);
    let sources = [
        (DisplaySource::User(items[0].id), 0),
        (DisplaySource::Reasoning(items[1].id), 2),
        (DisplaySource::AssistantText(items[2].id), 3),
        (
            DisplaySource::ToolCall {
                id: items[3].id,
                result_id: Some(items[4].id),
            },
            5,
        ),
        (DisplaySource::ToolResult(items[4].id), 5),
    ];
    app.state.session_mut().replace_messages(items.clone());

    for (source, expected_len) in sources {
        let forked = app.fork_at(source).unwrap();
        assert_eq!(forked.session.messages(), &items[..expected_len]);
        if matches!(source, DisplaySource::User(_)) {
            let draft = forked.draft.unwrap();
            assert_eq!(draft.text, "editable");
            assert_eq!(draft.images.as_slice(), std::slice::from_ref(&image));
        } else {
            assert!(forked.draft.is_none());
        }
    }
}

#[test]
fn fork_at_unfinished_tool_is_inclusive_and_does_not_continue_automatically() {
    let (_temp, _, _, mut app) = tempdir_app();
    let items = crate::history_items(&[
        Message::user("run".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "call-1",
                "read",
                serde_json::json!({}),
            )],
            ..Default::default()
        },
    ]);
    let source = DisplaySource::ToolCall {
        id: items[1].id,
        result_id: None,
    };
    app.state.session_mut().replace_messages(items.clone());
    app.message_actions.open(source, false, None);
    app.update(Msg::Key(key(KeyCode::Down)));

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(matches!(actions.as_slice(), [Action::ForkSession(_)]));
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::SendMessage(_)))
    );
    let Action::ForkSession(forked) = &actions[0] else {
        unreachable!()
    };
    assert_eq!(forked.session.messages(), items);
}

#[test]
fn fork_copies_only_reachable_tool_and_subagent_state_without_mutating_source() {
    let (_temp, _, _, mut app) = tempdir_app();
    let items = crate::history_items(&[
        Message::user("delegate".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "task-live",
                "task",
                serde_json::json!({}),
            )],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "task-live".into(),
                content: "done".into(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        },
        Message::user("later".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "task-late",
                "task",
                serde_json::json!({}),
            )],
            ..Default::default()
        },
    ]);
    let subagent = crate::history_items(&[
        Message::user("inspect".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "nested-tool",
                "read",
                serde_json::json!({}),
            )],
            ..Default::default()
        },
    ]);
    let session = app.state.session_mut();
    session.replace_messages(items.clone());
    session.set_subagent_messages("task-live".into(), subagent.clone());
    session.set_subagent_messages(
        "task-late".into(),
        crate::history_items(&[Message::user("late".into())]),
    );
    for id in ["task-live", "nested-tool", "task-late"] {
        session.insert_tool_output(id.into(), ToolOutput::Plain(id.into()));
    }
    session.set_subagents(vec![
        StoredSubagent {
            tool_use_id: "task-live".into(),
            parent_tool_use_id: Some("task-live".into()),
            root_tool_use_id: Some("task-live".into()),
            name: "live".into(),
            model: Some("test-model".into()),
            thinking: None,
            fast: false,
            outcome: StoredSubagentOutcome::Done,
        },
        StoredSubagent {
            tool_use_id: "task-late".into(),
            parent_tool_use_id: Some("task-late".into()),
            root_tool_use_id: Some("task-late".into()),
            name: "late".into(),
            model: None,
            thinking: None,
            fast: false,
            outcome: StoredSubagentOutcome::Unknown,
        },
    ]);
    let before = serde_json::to_value(&*app.state.session).unwrap();

    let forked = app
        .fork_at(DisplaySource::ToolCall {
            id: items[1].id,
            result_id: Some(items[2].id),
        })
        .unwrap();

    assert_eq!(serde_json::to_value(&*app.state.session).unwrap(), before);
    assert_eq!(forked.session.messages(), &items[..3]);
    assert_eq!(
        forked.session.subagent_messages()["task-live"].as_ref(),
        &subagent
    );
    assert!(!forked.session.subagent_messages().contains_key("task-late"));
    assert_eq!(forked.session.subagents().len(), 1);
    assert!(forked.session.tool_outputs().contains_key("task-live"));
    assert!(forked.session.tool_outputs().contains_key("nested-tool"));
    assert!(!forked.session.tool_outputs().contains_key("task-late"));
    assert_eq!(forked.session.token_usage, TokenUsage::default());
    assert!(forked.session.meta.pending_revert.is_none());
    assert!(forked.session.meta.queued_messages.is_empty());
    assert!(forked.session.meta.active_goal.is_none());
}

#[test]
fn fork_selects_the_subagent_history_version_on_its_branch() {
    let (_temp, _, _, mut app) = tempdir_app();
    let items = crate::history_items(&[
        Message::user("delegate".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "task-root",
                "task",
                serde_json::json!({}),
            )],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "task-root".into(),
                content: "first".into(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        },
        Message::user("continue".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "continuation-call",
                "task",
                serde_json::json!({ "task_id": "task-root" }),
            )],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "continuation-call".into(),
                content: "second".into(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        },
    ]);
    let first = crate::history_items(&[Message::user("first transcript".into())]);
    let second = crate::history_items(&[Message::user("continued transcript".into())]);
    let session = app.state.session_mut();
    session.replace_messages(items.clone());
    session.set_subagent_history(
        "task-root".into(),
        first.clone(),
        Some(caudra_storage::sessions::StoredSubagentTaskSpec::default()),
    );
    session.set_subagent_history(
        "continuation-call".into(),
        second.clone(),
        Some(caudra_storage::sessions::StoredSubagentTaskSpec::version()),
    );

    let before_continuation = app
        .fork_at(DisplaySource::ToolCall {
            id: items[1].id,
            result_id: Some(items[2].id),
        })
        .unwrap();
    let after_continuation = app
        .fork_at(DisplaySource::ToolCall {
            id: items[4].id,
            result_id: Some(items[5].id),
        })
        .unwrap();

    assert_eq!(
        before_continuation.session.subagent_messages()["task-root"].as_ref(),
        &first
    );
    assert_eq!(
        after_continuation.session.subagent_messages()["task-root"].as_ref(),
        &second
    );
    assert!(
        after_continuation
            .session
            .subagent_messages()
            .contains_key("continuation-call")
    );
}

#[test_case(false; "typed_direct_launch")]
#[test_case(true; "typed_batch_launch")]
fn typed_task_outputs_select_launch_versions_for_restore_and_fork(batch: bool) {
    const TASK: &str = "neat-wanted-cowbird";
    const FIRST_CALL: &str = "first-launch";
    const NEXT_CALL: &str = "next-launch";
    const FIRST_TEXT: &str = "initial investigation";
    const NEXT_TEXT: &str = "continued investigation";
    const PRIVATE_INVOCATION: &str = "not-a-history-version";
    let (_temp, _, _, mut app) = tempdir_app();
    let tool = if batch { "batch" } else { "task" };
    let mut messages = Vec::new();
    for call in [FIRST_CALL, NEXT_CALL] {
        messages.push(Message::user(call.into()));
        messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(call, tool, serde_json::json!({}))],
            ..Default::default()
        });
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: call.into(),
                content: String::new(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        });
        let launch = if batch {
            format!("{call}:0")
        } else {
            call.into()
        };
        let card = serde_json::from_value(serde_json::json!({
            "task_id": TASK, "invocation_id": PRIVATE_INVOCATION,
            "call_id": launch, "root_call_id": call, "label": RESEARCH_NAME,
            "state": "succeeded", "background": true, "mode": "build",
            "generation": 1, "created_at": 1, "updated_at": 2
        }))
        .unwrap();
        let mut output = ToolOutput::Tasks(vec![card]);
        if batch {
            let AgentEvent::ToolStart(event) = batch_roster(call, 1) else {
                unreachable!()
            };
            let Some(ToolOutput::Batch { mut entries, .. }) = event.output else {
                unreachable!()
            };
            entries[0].output = Some(output);
            entries[0].status = caudra_agent::BatchToolStatus::Success;
            output = ToolOutput::Batch {
                entries,
                text: String::new(),
            };
        }
        app.state
            .session_mut()
            .insert_tool_output(call.into(), output);
        app.state.session_mut().set_subagent_history(
            launch,
            crate::history_items(&[Message::user(
                if call == FIRST_CALL {
                    FIRST_TEXT
                } else {
                    NEXT_TEXT
                }
                .into(),
            )]),
            Some(caudra_storage::sessions::StoredSubagentTaskSpec::version()),
        );
    }
    let items = crate::history_items(&messages);
    app.state.session_mut().replace_messages(items.clone());
    app.state.session_mut().set_subagent_history(
        TASK.into(),
        crate::history_items(&[Message::user(FIRST_TEXT.into())]),
        Some(caudra_storage::sessions::StoredSubagentTaskSpec::default()),
    );
    app.state.session_mut().set_subagents(vec![StoredSubagent {
        tool_use_id: TASK.into(),
        parent_tool_use_id: Some(if batch {
            format!("{NEXT_CALL}:0")
        } else {
            NEXT_CALL.into()
        }),
        root_tool_use_id: Some(NEXT_CALL.into()),
        name: RESEARCH_NAME.into(),
        model: None,
        thinking: None,
        fast: false,
        outcome: StoredSubagentOutcome::Done,
    }]);
    for (index, expected) in [(1, FIRST_TEXT), (4, NEXT_TEXT)] {
        let forked = app
            .fork_at(DisplaySource::ToolCall {
                id: items[index].id,
                result_id: Some(items[index + 1].id),
            })
            .unwrap();
        let histories = crate::agent::stored_subagent_history(&forked.session).snapshot();
        let selected_call = if index == 1 { FIRST_CALL } else { NEXT_CALL };
        let selected_call = if batch {
            format!("{selected_call}:0")
        } else {
            selected_call.into()
        };
        assert_eq!(
            histories.records()[TASK].version_id(),
            Some(selected_call.as_str())
        );
        assert_eq!(
            histories.histories()[TASK][0].first_text_content(),
            Some(expected)
        );
    }
    let histories = crate::agent::stored_subagent_history(&app.state.session).snapshot();
    assert_eq!(
        histories.histories()[TASK][0].first_text_content(),
        Some(NEXT_TEXT)
    );
    app.reset_ui_chrome();
    app.restore_display();
    app.focus_task(TASK).unwrap();
    assert_eq!(
        app.chats[app.active_chat].message_at(0).unwrap().text,
        NEXT_TEXT
    );
    app.state.session_mut().meta.history_head = Some(items[2].id);
    app.state.session_mut().set_subagent_history(
        TASK.into(),
        crate::history_items(&[Message::user(NEXT_TEXT.into())]),
        Some(caudra_storage::sessions::StoredSubagentTaskSpec::default()),
    );
    let seeded = crate::agent::stored_subagent_history(&app.state.session);
    let selected = if batch {
        format!("{FIRST_CALL}:0")
    } else {
        FIRST_CALL.into()
    };
    assert_eq!(
        seeded.snapshot().records()[TASK].version_id(),
        Some(selected.as_str())
    );
    let continuation = seeded.continue_task_with(TASK, Default::default()).unwrap();
    assert_eq!(
        continuation.history().unwrap()[0].first_text_content(),
        Some(FIRST_TEXT)
    );
    let mut missing = AppSession::new(&app.state.session.model, &app.state.session.cwd);
    missing.replace_messages(crate::active_session_history(&app.state.session).unwrap());
    for (call, output) in app.state.session.tool_outputs() {
        missing.insert_tool_output(call.clone(), output.as_ref().clone());
    }
    missing.set_subagents(app.state.session.subagents().to_vec());
    missing.set_subagent_history(
        TASK.into(),
        crate::history_items(&[Message::user(NEXT_TEXT.into())]),
        Some(caudra_storage::sessions::StoredSubagentTaskSpec::default()),
    );
    let seed = crate::agent::stored_subagent_history(&missing);
    assert!(!seed.snapshot().records().contains_key(TASK));
    assert!(seed.reserve(TASK).is_err());
    app.state.session = Arc::new(missing);
    app.reset_ui_chrome();
    app.restore_display();
    assert!(app.focus_task(TASK).is_err());
    let forked = app
        .fork_at(DisplaySource::ToolCall {
            id: items[1].id,
            result_id: Some(items[2].id),
        })
        .unwrap();
    assert!(!forked.session.subagent_messages().contains_key(TASK));
    assert!(
        crate::agent::stored_subagent_history(&forked.session)
            .reserve(TASK)
            .is_err()
    );
}

#[test]
fn fork_copies_only_reachable_managed_outputs_including_nested_subagents() {
    let (_temp, storage, _, mut app) = tempdir_app();
    let source_session_id = app.state.session.id;
    let store = ToolOutputStore::new(storage);
    let main_ref = store.put(source_session_id, "main full output").unwrap();
    let subagent_ref = store
        .put(source_session_id, "subagent full output")
        .unwrap();
    let nested_ref = store.put(source_session_id, "nested full output").unwrap();
    let outside_ref = store.put(source_session_id, "outside full output").unwrap();
    let items = crate::history_items(&[
        Message::user("delegate".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "task-live",
                "task",
                serde_json::json!({}),
            )],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "task-live".into(),
                content: "main preview".into(),
                is_error: false,
                output_ref: Some(main_ref.clone()),
            }],
            ..Default::default()
        },
        Message::user("later".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "outside",
                "read",
                serde_json::json!({}),
            )],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "outside".into(),
                content: "outside preview".into(),
                is_error: false,
                output_ref: Some(outside_ref.clone()),
            }],
            ..Default::default()
        },
    ]);
    let subagent = crate::history_items(&[
        Message::user("inspect".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "task-nested",
                "task",
                serde_json::json!({}),
            )],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "task-nested".into(),
                content: "subagent preview".into(),
                is_error: false,
                output_ref: Some(subagent_ref.clone()),
            }],
            ..Default::default()
        },
    ]);
    let nested_subagent = crate::history_items(&[
        Message::user("inspect deeper".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "nested-read",
                "read",
                serde_json::json!({}),
            )],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "nested-read".into(),
                content: "nested preview".into(),
                is_error: false,
                output_ref: Some(nested_ref.clone()),
            }],
            ..Default::default()
        },
    ]);
    let session = app.state.session_mut();
    session.replace_messages(items.clone());
    session.set_subagent_messages("task-live".into(), subagent.clone());
    session.set_subagent_messages("task-nested".into(), nested_subagent.clone());

    let forked = app.fork_at(DisplaySource::ToolResult(items[2].id)).unwrap();

    assert_eq!(forked.session.messages(), &items[..3]);
    assert_eq!(
        forked.session.subagent_messages()["task-live"].as_ref(),
        &subagent
    );
    assert_eq!(
        forked.session.subagent_messages()["task-nested"].as_ref(),
        &nested_subagent
    );
    for (output_ref, expected) in [
        (&main_ref, "main full output"),
        (&subagent_ref, "subagent full output"),
        (&nested_ref, "nested full output"),
    ] {
        assert_eq!(
            store
                .read(forked.session.id, output_ref.id.clone(), 1, 10)
                .unwrap()
                .text,
            expected
        );
    }
    assert!(matches!(
        store.read(forked.session.id, outside_ref.id, 1, 10),
        Err(ToolOutputError::NotFound { .. })
    ));
}

#[test_case(false; "single_reference")]
#[test_case(true; "duplicate_references")]
fn fork_reloads_truncated_task_observation_without_output_retrieval(duplicate: bool) {
    const TASK: &str = "bright-calm-wren";
    const INVOCATION: &str = "private-background-invocation";
    const EVENT: &str = "private-background-event";
    const OUTCOME: &str =
        r#"{"output":"Complete background result beyond the preview","success":true}"#;
    const UNRELATED: &str = "Unrelated output mentioned only in prose";
    const ACK: &str = "The background task finished.";
    let (_temp, storage, _, mut app) = tempdir_app();
    let source = app.state.session.id;
    let store = ToolOutputStore::new(storage.clone());
    let reference = store.put(source, OUTCOME).unwrap();
    let unrelated = store.put(source, UNRELATED).unwrap();
    let origin = TaskEventOrigin {
        task_id: TASK.into(),
        invocation_id: INVOCATION.into(),
        event_id: EVENT.into(),
    };
    let delivery = format!(
        "Task {TASK}: success.\n\nResult preview truncated.\nRead full outcome: tool_output {}\nMentioned only: {}",
        reference.id, unrelated.id,
    );
    let mut observation = Message::task_observation(delivery.clone(), origin.clone());
    observation.retained_output_refs = vec![reference.clone()];
    if duplicate {
        observation.retained_output_refs.push(reference.clone());
    }
    let mut messages = vec![
        Message::user("Run a background task".into()),
        observation.clone(),
    ];
    if duplicate {
        let mut repeated = observation;
        repeated
            .task_event
            .as_mut()
            .unwrap()
            .event_id
            .push_str("-next");
        messages.push(repeated);
    }
    messages.push(Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text { text: ACK.into() }],
        ..Default::default()
    });
    let items = crate::history_items(&messages);
    let head = items.last().unwrap().id;
    app.state.session_mut().replace_messages(items);
    app.state.session_mut().save(&storage).unwrap();
    app.state.session = Arc::new(AppSession::load(source, &storage).unwrap());
    let mut forked = app.fork_at(DisplaySource::AssistantText(head)).unwrap();
    let target = forked.session.id;
    forked.session.save(&storage).unwrap();
    let reloaded = AppSession::load(target, &storage).unwrap();
    let projected = project_messages(reloaded.messages()).unwrap();
    let delivered = projected
        .iter()
        .find(|message| message.task_event.as_ref() == Some(&origin))
        .unwrap();
    assert_eq!(delivered.retained_output_refs, from_ref(&reference));
    assert_eq!(delivered.first_text_content(), Some(delivery.as_str()));
    let read = store.read(target, reference.id.clone(), 1, 10).unwrap();
    assert_eq!(read.text, OUTCOME);
    assert_eq!(read.total_bytes, OUTCOME.len());
    assert!(matches!(
        store.read(target, unrelated.id, 1, 10),
        Err(ToolOutputError::NotFound { .. })
    ));
    app.state.session = Arc::new(reloaded);
    app.reset_ui_chrome();
    app.restore_display();
    let chat = app.main_chat();
    let display = (0..chat.message_count()).filter_map(|index| chat.message_at(index)).find(|message| {
        matches!(&message.role, DisplayRole::TaskDelivery(saved) if **saved == origin)
    }).unwrap();
    assert_eq!(display.text, delivery);
}

/// Reloading a compacted session used to stop at the border, because the
/// transcript and the request read the same walk and compaction starts a chain
/// the request is meant to begin at. The turns were never deleted, only left
/// unreachable, so a reader could not scroll back to what they had written.
#[test]
fn a_restored_compacted_session_scrolls_past_the_border() {
    const EARLIER: &str = "the turn the summary replaced";
    const SUMMARY: &str = "## Objective";

    let mut app = test_app();
    let mut items = crate::history_items(&[Message::user(EARLIER.into())]);
    let superseded = items.last().unwrap().id;
    let mut compacted = crate::history_items(&[
        Message::synthetic(caudra_agent::COMPACTION_ANCHOR.into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: SUMMARY.into(),
            }],
            is_compaction_summary: true,
            ..Default::default()
        },
    ]);
    compacted[0].supersedes = Some(superseded);
    let head = compacted.last().unwrap().id;
    items.extend(compacted);
    let session = app.state.session_mut();
    session.replace_messages(items);
    session.meta.history_head = Some(head);

    app.restore_display();

    let chat = app.main_chat();
    assert_eq!(chat.message_at(0).map(|m| m.text.as_str()), Some(EARLIER));
    assert_eq!(
        chat.message_at(1).map(|m| &m.role),
        Some(&DisplayRole::Notice),
        "the border still separates the summary from what it replaced"
    );
    assert_eq!(chat.message_at(2).map(|m| m.text.as_str()), Some(SUMMARY));
}

/// A respawned agent starts from the list the transcript last committed, and
/// a compaction moves that update out of the active chain into the archive.
#[test]
fn a_restored_compacted_session_hands_the_agent_its_todo_list() {
    const PLAN_CALL: &str = "todo-1";
    const PLAN: &str = "verify the restored list";
    const SUMMARY: &str = "## Objective";

    let mut app = test_app();
    let todos = vec![TodoItem {
        content: PLAN.into(),
        status: TodoStatus::InProgress,
        priority: TodoPriority::High,
    }];
    let mut items = crate::history_items(&[
        Message::user(PLAN.into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                PLAN_CALL,
                TODOWRITE_TOOL_NAME,
                serde_json::json!({}),
            )],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: PLAN_CALL.into(),
                content: String::new(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        },
    ]);
    let superseded = items.last().unwrap().id;
    let mut compacted = crate::history_items(&[
        Message::synthetic(caudra_agent::COMPACTION_ANCHOR.into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: SUMMARY.into(),
            }],
            is_compaction_summary: true,
            ..Default::default()
        },
    ]);
    compacted[0].supersedes = Some(superseded);
    let head = compacted.last().unwrap().id;
    items.extend(compacted);
    let session = app.state.session_mut();
    session.replace_messages(items);
    session.meta.history_head = Some(head);
    session.insert_tool_output(PLAN_CALL.into(), ToolOutput::TodoList(todos.clone()));

    let session = &app.state.session;
    let active = crate::active_session_history(session).unwrap();
    let archived = crate::archived_session_history(session);
    assert_eq!(crate::session_todos(session, &[], &active), None);
    assert_eq!(
        crate::session_todos(session, &archived, &active),
        Some(todos)
    );
}

#[test]
fn compacted_fork_copies_subagent_artifacts_without_top_level_refs() {
    let (_temp, storage, _, mut app) = tempdir_app();
    let store = ToolOutputStore::new(storage);
    let output_ref = store
        .put(app.state.session.id, "nested full output")
        .unwrap();
    let items = crate::history_items(&[
        Message::user("What did we do so far?".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: format!("Nested output ID: {}", output_ref.id),
            }],
            retained_subagent_ids: vec!["old-task".into()],
            is_compaction_summary: true,
            ..Default::default()
        },
    ]);
    let subagent = crate::history_items(&[Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "nested-read".into(),
            content: "preview".into(),
            is_error: false,
            output_ref: Some(output_ref.clone()),
        }],
        ..Default::default()
    }]);
    let session = app.state.session_mut();
    session.replace_messages(items.clone());
    session.set_subagent_messages("old-task".into(), subagent.clone());

    let forked = app
        .fork_at(DisplaySource::AssistantText(items[1].id))
        .unwrap();

    assert_eq!(
        forked.session.subagent_messages()["old-task"].as_ref(),
        &subagent
    );
    assert_eq!(
        store
            .read(forked.session.id, output_ref.id, 1, 10)
            .unwrap()
            .text,
        "nested full output"
    );
}

#[test]
fn fork_fails_when_referenced_managed_output_is_missing() {
    let (_temp, storage, _, mut app) = tempdir_app();
    let source_session_id = app.state.session.id;
    let store = ToolOutputStore::new(storage);
    let missing_ref = store.put(source_session_id, "missing full output").unwrap();
    store.delete_session(source_session_id).unwrap();
    let items = crate::history_items(&[
        Message::user("inspect".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "read-1",
                "read",
                serde_json::json!({}),
            )],
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "read-1".into(),
                content: "preview".into(),
                is_error: false,
                output_ref: Some(missing_ref.clone()),
            }],
            ..Default::default()
        },
    ]);
    app.state.session_mut().replace_messages(items.clone());

    let Err(error) = app.fork_at(DisplaySource::ToolResult(items[2].id)) else {
        panic!("fork unexpectedly succeeded with a missing managed output");
    };

    assert!(error.contains("Failed to copy managed tool outputs for fork"));
    assert!(error.contains(&missing_ref.id.to_string()));
    assert!(error.contains(&source_session_id.to_string()));
}

#[test_case("/move-session", false, None; "move_picker")]
#[test_case("/migrate-sessions", true, None; "migrate_picker")]
#[test_case("/move-session destination with spaces", false, Some(RELOCATION_DESTINATION); "move_destination")]
#[test_case("/migrate-sessions destination with spaces", true, Some(RELOCATION_DESTINATION); "migrate_destination")]
#[test_case("  /MOVE-SESSION destination with spaces  ", false, Some(RELOCATION_DESTINATION); "normalized_command")]
fn relocation_commands_request_runtime_preflight(
    command: &str,
    bulk: bool,
    destination: Option<&str>,
) {
    let mut app = test_app();
    let actions = app.run_cmdline(command, 0).unwrap();
    assert!(matches!(
        &actions[..],
        [Action::OpenSessionRelocation { bulk: actual_bulk, destination: actual_destination }]
            if *actual_bulk == bulk && actual_destination.as_deref() == destination
    ));
    assert!(!app.session_relocation_picker.is_open());
}

#[test_case(kb::MOVE_SESSION, false; "move_current")]
#[test_case(kb::MIGRATE_SESSIONS, true; "migrate_directory")]
fn session_picker_relocation_keys_request_runtime_preflight(bind: Bind, bulk: bool) {
    let mut app = test_app();
    app.sessions_browse();
    let actions = app.update(Msg::Key(bind.to_key_event()));
    assert!(matches!(
        &actions[..],
        [Action::OpenSessionRelocation { bulk: actual_bulk, destination: None }]
            if *actual_bulk == bulk
    ));
    assert!(!app.session_picker.is_open());
    assert!(!app.session_relocation_picker.is_open());
}

fn open_relocation_picker(app: &mut App) {
    app.open_session_relocation(Vec::new(), false, None, 0);
}

#[test_case(false; "escape")]
#[test_case(true; "close_all")]
fn relocation_cancellation_preserves_the_draft(close_all: bool) {
    let mut app = test_app();
    app.update(Msg::Paste(RELOCATION_DRAFT.into()));
    open_relocation_picker(&mut app);
    assert!(app.has_modal_overlay());
    if close_all {
        app.close_all_overlays();
    } else {
        assert!(app.update(Msg::Key(key(KeyCode::Esc))).is_empty());
    }
    assert!(!app.any_overlay_open());
    assert_eq!(app.input_box.buffer.value(), RELOCATION_DRAFT);
}

#[test_case(false, false, false; "composer_keyboard")]
#[test_case(true, false, false; "workbench_keyboard")]
#[test_case(false, true, false; "composer_mouse")]
#[test_case(true, true, false; "workbench_mouse")]
#[test_case(false, false, true; "bulk_composer_keyboard")]
#[test_case(true, false, true; "bulk_workbench_keyboard")]
#[test_case(false, true, true; "bulk_composer_mouse")]
#[test_case(true, true, true; "bulk_workbench_mouse")]
fn relocation_custom_destination_paste_reaches_confirmation(
    workbench: bool,
    mouse: bool,
    bulk: bool,
) {
    let (_temp, storage, writer, mut app) = tempdir_app();
    let destination = TempDir::new().unwrap();
    let destination = destination.path().canonicalize().unwrap();
    let destination_text = destination.to_str().unwrap();
    let location = SessionLocation {
        id: app.state.session.id,
        title: String::new(),
        cwd: app.state.session.cwd.clone(),
        updated_at: 0,
        write_version: RELOCATION_WRITE_VERSION,
    };
    let generation = writer.generation();
    app.update(Msg::Paste(RELOCATION_DRAFT.into()));
    if workbench {
        app.run_builtin(BuiltinAction::Workbench);
    }
    app.open_session_relocation(
        vec![location.clone()],
        bulk,
        None,
        RELOCATION_OTHER_OPEN_COUNT,
    );
    if bulk {
        assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    }
    assert!(
        app.update(Msg::Key(kb::RELOCATION_CUSTOM.to_key_event()))
            .is_empty()
    );
    assert!(app.update(Msg::Paste(destination_text.into())).is_empty());
    assert_eq!(app.input_box.buffer.value(), RELOCATION_DRAFT);
    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    assert!(rendered(&mut app).contains(RELOCATION_CONFIRM_TITLE));
    assert!(app.update(Msg::Paste(RELOCATION_DRAFT.into())).is_empty());
    if bulk {
        if mouse {
            let (row, column) = screen_hit(&mut app, RELOCATION_PROJECT_USAGE);
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                assert!(app.update(mouse_event(kind, column, row)).is_empty());
            }
        } else {
            assert!(app.update(Msg::Key(key(KeyCode::Up))).is_empty());
            assert!(
                app.update(Msg::Key(kb::RELOCATION_USAGE.to_key_event()))
                    .is_empty()
            );
        }
        assert!(rendered(&mut app).contains(&format!("[ ] {RELOCATION_PROJECT_USAGE}")));
        if !mouse {
            assert!(app.update(Msg::Key(key(KeyCode::Down))).is_empty());
        }
    }
    let actions = if mouse {
        let (row, column) = screen_hit(&mut app, RELOCATION_CONFIRM);
        assert!(
            app.update(mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                column,
                row
            ))
            .is_empty()
        );
        assert!(app.selection_state.is_none());
        app.update(Msg::Scroll {
            column,
            row,
            delta: 1,
        });
        assert!(
            app.update(mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                column,
                row
            ))
            .is_empty()
        );
        assert!(app.session_relocation_picker.is_open());
        let (row, column) = screen_hit(&mut app, RELOCATION_CONFIRM);
        app.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            column,
            row,
        ));
        app.update(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            column,
            row,
        ))
    } else {
        app.update(Msg::Key(key(KeyCode::Enter)))
    };
    assert!(matches!(
        &actions[..],
        [Action::RelocateSessions { request, donor: None }]
            if request.source_cwd == bulk.then(|| location.cwd.clone())
                && request.sessions == [location]
                && !request.include_project_usage
                && request.destination == destination_text
    ));
    assert!(!app.session_relocation_picker.is_open());
    assert_eq!(app.input_box.buffer.value(), RELOCATION_DRAFT);
    assert_eq!(writer.generation(), generation);
    assert!(AppSession::load(app.state.session.id, &storage).is_err());
}

#[test_case(false; "composer_background")]
#[test_case(true; "workbench_background")]
fn relocation_outside_press_dismisses_without_reaching_the_background(workbench: bool) {
    let mut app = test_app();
    app.update(Msg::Paste(RELOCATION_DRAFT.into()));
    if workbench {
        app.run_builtin(BuiltinAction::Workbench);
    }
    open_relocation_picker(&mut app);
    rendered(&mut app);
    let (column, row) = OUTSIDE_MODAL;
    assert!(
        app.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            column,
            row
        ))
        .is_empty()
    );
    assert!(!app.session_relocation_picker.is_open());
    assert_eq!(app.workbench.is_open(), workbench);
    assert!(app.selection_state.is_none());
    assert_eq!(app.input_box.buffer.value(), RELOCATION_DRAFT);
}

/// The picker's live half never moves for a session this process does not
/// have open, so a delete that only the writer thread can finish left the row
/// on screen for as long as the picker stayed up.
#[test]
fn deleting_a_stored_session_drops_it_from_the_open_picker() {
    let (_temp, storage, writer, mut app) = tempdir_app();
    let mut stored = AppSession::new(&app.state.session.model, &app.state.session.cwd);
    let id = stored.id;
    stored.save(&storage).unwrap();
    app.sessions_browse();
    assert!(app.session_picker.ids().contains(&id), "{PICKER_MISSED_IT}");
    assert_eq!(app.refresh_session_picker(), Dirty::NO, "{QUIET}");

    writer.delete_sync(id).unwrap();

    assert_eq!(app.refresh_session_picker(), Dirty::YES, "{OWED}");
    assert!(!app.session_picker.ids().contains(&id), "{PICKER_KEPT_IT}");
    assert_eq!(app.refresh_session_picker(), Dirty::NO, "{QUIET}");
}

/// The transcript binds hand every key to the open overlay, so this walks the
/// whole chain, not just the list. Bare `Home` and `End` are the filter's.
#[test_case(KeyEventKind::Press; "press")]
#[test_case(KeyEventKind::Repeat; "repeat")]
fn the_list_end_keys_reach_an_open_picker_list(kind: KeyEventKind) {
    let (_temp, storage, _writer, mut app) = tempdir_app();
    for _ in 0..PICKER_ROWS {
        let mut stored = AppSession::new(&app.state.session.model, &app.state.session.cwd);
        stored.save(&storage).unwrap();
    }
    app.sessions_browse();
    let last = app.session_picker.ids().len() - 1;
    assert!(last > 0, "{PICKER_NEEDS_ROWS}");

    dispatch_reported_key(&mut app, workbench_keys::LIST_LAST.to_key_event(), kind);
    assert_eq!(app.session_picker.selected_index(), Some(last));

    dispatch_reported_key(&mut app, key(KeyCode::Home), kind);
    assert_eq!(app.session_picker.selected_index(), Some(last));

    dispatch_reported_key(&mut app, workbench_keys::LIST_FIRST.to_key_event(), kind);
    assert_eq!(app.session_picker.selected_index(), Some(0));
}

#[test]
fn retitling_a_stored_session_refreshes_the_open_picker() {
    let (_temp, storage, writer, mut app) = tempdir_app();
    let mut stored = AppSession::new(&app.state.session.model, &app.state.session.cwd);
    let id = stored.id;
    stored.save(&storage).unwrap();
    app.sessions_browse();
    assert_eq!(app.refresh_session_picker(), Dirty::NO, "{QUIET}");
    let mut renamed = AppSession::load(id, &storage).unwrap();
    renamed.set_title(GENERATED_TITLE.into());

    writer.save_sync(Arc::new(renamed)).unwrap();

    assert_eq!(app.refresh_session_picker(), Dirty::YES, "{OWED}");
}

#[test]
fn fork_title_uses_next_number_for_base_title() {
    let (_temp, storage, _, mut app) = tempdir_app();
    app.state.session_mut().set_title("Investigate".into());
    let mut existing = AppSession::new(&app.state.session.model, &app.state.session.cwd);
    existing.set_title("Investigate (fork #2)".into());
    existing.save(&storage).unwrap();
    let item = crate::history_items(&[Message::user("prompt".into())]);
    let source = DisplaySource::User(item[0].id);
    app.state.session_mut().replace_messages(item);

    let forked = app.fork_at(source).unwrap();

    assert_eq!(forked.session.title, "Investigate (fork #3)");
    assert_ne!(forked.session.id, app.state.session.id);
    assert!(forked.session.created_at >= app.state.session.created_at);
}

#[test]
fn fork_copies_execution_settings_but_resets_conversation_state() {
    let (_temp, _, _, mut app) = tempdir_app();
    let plan = PathBuf::from(&app.state.session.cwd).join(HANDOFF_PLAN_FILE);
    PlanFile::new(plan.clone())
        .unwrap()
        .write(HANDOFF_PLAN_CONTENT)
        .unwrap();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Ready(plan.clone());
    app.state.thinking = ThinkingConfig::Adaptive;
    app.state.fast = true;
    app.state.system_prompt_profile_name = "review".into();
    app.permissions
        .load_structured_conversation_rules(vec![conversation_permission_record()]);
    app.permissions.set_session_mode(Some(PermissionMode::Yolo));
    app.state.token_usage.input = 42;
    app.state.session_mut().meta.input_draft = Some("old draft".into());
    app.state.session_mut().meta.queued_messages = vec![stored_queued_prompt("queued")];
    app.state.session_mut().meta.active_goal = Some(Box::new(StoredActiveGoal {
        condition: GOAL_CONDITION.into(),
        evaluations: 0,
        elapsed_ms: 0,
        usage: StoredTokenUsage::default(),
        last_verdict: None,
        last_reason: None,
    }));
    let items = crate::history_items(&[Message::user("prompt".into())]);
    let source = DisplaySource::User(items[0].id);
    app.state.session_mut().replace_messages(items);

    let forked = app.fork_at(source).unwrap();
    let child = forked.session;

    assert_eq!(child.model, app.state.session.model);
    assert_eq!(child.cwd, app.state.session.cwd);
    assert_eq!(child.meta.mode, Some(StoredMode::Plan));
    let copy = PathBuf::from(child.meta.plan_path.as_deref().unwrap());
    assert_ne!(copy, plan);
    assert_eq!(
        PlanFile::new(copy).unwrap().read().unwrap(),
        HANDOFF_PLAN_CONTENT
    );
    assert!(child.meta.plan_written);
    assert_eq!(child.meta.thinking, Some(StoredThinking::Adaptive));
    assert!(child.meta.fast);
    assert_eq!(child.meta.system_prompt_profile.as_deref(), Some("review"));
    assert!(child.meta.structured_permission_rules.is_empty());
    assert_eq!(child.meta.permission_mode, None);
    assert_eq!(child.token_usage, TokenUsage::default());
    assert!(child.usage_by_model().is_empty());
    assert!(child.meta.input_draft.is_none());
    assert!(child.meta.input_draft_images.is_empty());
    assert!(child.meta.queued_messages.is_empty());
    assert!(child.meta.active_goal.is_none());
    assert!(child.meta.goal_result.is_none());
    assert!(child.meta.pending_revert.is_none());
}

/// The text of the plan `target` names, read as `owner`, the session whose
/// document it is.
fn plan_text(app: &App, owner: &str, target: &PlanTarget) -> String {
    match target {
        PlanTarget::Local(path) => PlanFile::new(Path::new(&app.state.session.cwd).join(path))
            .unwrap()
            .read()
            .unwrap(),
        PlanTarget::Remote(reference) => {
            let store = app.local_documents.as_ref().unwrap();
            let document = LocalDocumentRef::Plan(reference.clone());
            store
                .read(store.project_key(), Some(owner), &document)
                .unwrap()
                .content
        }
    }
}

fn write_plan_text(app: &App, owner: &str, target: &PlanTarget, text: &str) {
    match target {
        PlanTarget::Local(path) => {
            PlanFile::new(Path::new(&app.state.session.cwd).join(path))
                .unwrap()
                .write(text)
                .unwrap();
        }
        PlanTarget::Remote(reference) => {
            let store = app.local_documents.as_ref().unwrap();
            let document = LocalDocumentRef::Plan(reference.clone());
            store
                .write(store.project_key(), Some(owner), &document, text)
                .unwrap();
        }
    }
}

fn fork_after_first_prompt(app: &mut App) -> ForkedSession {
    let items = crate::history_items(&[Message::user("prompt".into())]);
    let source = DisplaySource::User(items[0].id);
    app.state.session_mut().replace_messages(items);
    app.fork_at(source).unwrap()
}

/// A fork revises a plan of its own, drafting or ready as its parent's is, so
/// neither session's writes reach the other's.
#[test_case(false, false ; "local_drafting")]
#[test_case(false, true ; "local_ready")]
#[test_case(true, false ; "remote_drafting")]
#[test_case(true, true ; "remote_ready")]
fn fork_copies_the_plan_into_a_document_of_its_own(remote: bool, ready: bool) {
    let mut app = if remote {
        remote_plan_app()
    } else {
        plan_app()
    };
    if !ready {
        app.state.plan.mark_drafting();
    }
    let parent = app.state.plan.target().unwrap();

    let forked = fork_after_first_prompt(&mut app);

    let child = &forked.session;
    assert!(forked.warning.is_none());
    assert_eq!(child.meta.plan_written, ready);
    assert_eq!(child.meta.plan_path.is_some(), !remote);
    let copy = PlanTarget::from(child.meta.plan_target.as_ref().unwrap());
    let owner = child.id.to_string();
    assert_eq!(plan_text(&app, &owner, &copy), HANDOFF_PLAN_CONTENT);
    write_plan_text(&app, &owner, &copy, HANDOFF_PLAN_CHANGED);
    assert_eq!(
        plan_text(&app, &app.state.session.id.to_string(), &parent),
        HANDOFF_PLAN_CONTENT
    );
}

/// A plan that cannot be copied does not stop the fork: it goes ahead without
/// one, and says so.
#[test_case(false ; "local_unreadable")]
#[test_case(true ; "remote_store_unavailable")]
fn a_fork_whose_plan_cannot_be_copied_has_none(remote: bool) {
    let mut app = if remote {
        remote_plan_app()
    } else {
        plan_app()
    };
    if remote {
        app.local_documents = None;
    } else {
        let path = Path::new(&app.state.session.cwd).join(HANDOFF_PLAN_FILE);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
    }

    let forked = fork_after_first_prompt(&mut app);

    let meta = &forked.session.meta;
    assert!(meta.plan_target.is_none());
    assert!(meta.plan_path.is_none());
    assert!(!meta.plan_written);
    assert!(
        forked
            .warning
            .as_deref()
            .is_some_and(|warning| warning.starts_with(PLAN_COPY_FAILED))
    );
}

#[test]
fn a_files_revert_without_change_records_says_why() {
    const EXPLAINED_MSG: &str = "a revert with no change records must name the reason";
    let mut app = build_rewind_app();
    let target = app.state.session.messages()[0].id;

    let actions = app.revert_to(target, RestoreMode::Files);

    assert!(actions.is_empty(), "{EXPLAINED_MSG}");
    assert_eq!(
        app.status_bar.flash_text(),
        Some(file_revert::NO_CHANGE_RECORDS),
        "{EXPLAINED_MSG}"
    );
}

/// The conversation half is still doable, so refusing the whole request would
/// cost the user something Caudra can actually deliver. Rewinding resets the
/// chrome, and the reason must outlive that.
#[test]
fn a_both_revert_without_change_records_still_rewinds_the_conversation() {
    const DOWNGRADED_MSG: &str = "a both revert must fall back to the conversation half";
    let mut app = build_rewind_app();
    let target = app.state.session.messages()[0].id;

    let actions = app.revert_to(target, RestoreMode::Both);

    assert!(
        matches!(actions.as_slice(), [Action::LoadSession(_)]),
        "{DOWNGRADED_MSG}"
    );
    assert_eq!(
        app.status_bar.flash_text(),
        Some(file_revert::NO_CHANGE_RECORDS),
        "{DOWNGRADED_MSG}"
    );
}

#[test]
fn revert_is_rejected_while_streaming() {
    let mut app = build_rewind_app();
    let source = DisplaySource::AssistantText(app.state.session.messages()[1].id);
    let head = crate::session_history_head(&app.state.session);
    app.status = Status::Streaming;

    let actions = app.revert_at(source, RestoreMode::Conversation);

    assert!(actions.is_empty());
    assert_eq!(crate::session_history_head(&app.state.session), head);
    assert_eq!(app.status_bar.flash_text(), Some(REVERT_BUSY_MSG));
}

#[test]
fn revert_and_unrevert_are_rejected_while_cancellation_is_pending() {
    let mut reverting = build_rewind_app();
    let second_user = reverting.state.session.messages()[3].id;
    let original_head = crate::session_history_head(&reverting.state.session);
    reverting.status = Status::Streaming;
    reverting.run_id = 1;
    let _reverting_receiver = attach_processing_queue(&mut reverting);
    reverting.handle_cancel();

    assert!(
        reverting
            .revert_to(second_user, RestoreMode::Conversation)
            .is_empty()
    );
    assert_eq!(
        crate::session_history_head(&reverting.state.session),
        original_head
    );

    let mut unreverting = build_rewind_app();
    unreverting.revert_to(second_user, RestoreMode::Conversation);
    let reverted_head = crate::session_history_head(&unreverting.state.session);
    unreverting.status = Status::Streaming;
    unreverting.run_id = 1;
    let _unreverting_receiver = attach_processing_queue(&mut unreverting);
    unreverting.handle_cancel();

    assert!(unreverting.unrevert().is_empty());
    assert_eq!(
        crate::session_history_head(&unreverting.state.session),
        reverted_head
    );
}

#[test_case(Duration::ZERO,          true  ; "keeps_fresh_error")]
#[test_case(Duration::from_secs(60), false ; "clears_stale_error")]
fn tick_error_expiry(age: Duration, expect_error: bool) {
    let mut app = test_app();
    app.status = Status::Error {
        message: "fail".into(),
        since: Instant::now() - age,
    };
    let _ = app.tick_error_expiry();
    assert_eq!(matches!(app.status, Status::Error { .. }), expect_error);
}

#[test]
fn retry_clears_in_progress_tools() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::ToolPending {
        id: "t1".into(),
        name: "bash".into(),
    }));
    assert_eq!(app.chats[0].in_progress_count(), 1);

    app.update(agent_msg(AgentEvent::Retry {
        attempt: 1,
        message: "overloaded".into(),
        delay_ms: 1000,
    }));
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert!(app.chats[0].retry().is_some());
}

/// The countdown is a control, not just a label: clicking it asks the agent to
/// stop waiting, and the chip goes away so the click visibly landed.
#[test]
fn clicking_the_retry_countdown_only_nudges_the_main_chat() {
    let mut app = app_with_retrying_subagent();
    let (cmd_tx, cmd_rx) = flume::unbounded();
    app.cmd_tx = Some(cmd_tx);
    app.update(agent_msg(retry_event()));
    let main_deadline = app.chats[0].retry().unwrap().deadline;
    let task_deadline = app.chats[1].retry().unwrap().deadline;
    let hit = status_hit(&mut app, StatusBarHitTarget::Retry);

    app.active_chat = 1;
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        assert!(
            app.update(mouse_event(kind, hit.area.x, hit.area.y))
                .is_empty()
        );
    }
    assert!(cmd_rx.try_recv().is_err());
    assert_eq!(app.chats[0].retry().unwrap().deadline, main_deadline);
    assert_eq!(app.chats[1].retry().unwrap().deadline, task_deadline);

    app.active_chat = 0;
    assert!(click_status(&mut app, StatusBarHitTarget::Retry).is_empty());

    assert!(app.chats[0].retry().is_none());
    assert_eq!(app.chats[1].retry().unwrap().deadline, task_deadline);
    assert!(matches!(
        cmd_rx.try_recv(),
        Ok(crate::agent::AgentCommand::RetryNow)
    ));
    assert!(cmd_rx.try_recv().is_err());
}

#[test]
fn hovering_the_retry_countdown_marks_it_hovered() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(retry_event()));
    let hit = status_hit(&mut app, StatusBarHitTarget::Retry);

    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));

    assert_eq!(app.status_hover, Some(StatusBarHitTarget::Retry));
}

/// Nothing is waiting, so there is nothing to cut short.
#[test]
fn an_idle_bar_has_no_retry_control() {
    let mut app = test_app();
    let _ = rendered(&mut app);
    assert!(
        app.status_hits
            .iter()
            .all(|hit| hit.target != StatusBarHitTarget::Retry)
    );
}

#[test]
fn retry_clears_subagent_in_progress_tools() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::ToolPending {
            id: "st1".into(),
            name: "bash".into(),
        },
        TASK_ID,
        Some("research"),
    ));
    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.chats[1].in_progress_count(), 1);

    app.update(subagent_msg(
        AgentEvent::Retry {
            attempt: 1,
            message: "overloaded".into(),
            delay_ms: 1000,
        },
        TASK_ID,
        Some("research"),
    ));
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(app.chats[1].retry().is_some(), "{SUBAGENT_RETRY_MISSING}");
    assert!(app.chats[0].retry().is_none(), "{SUBAGENT_RETRY_LEAKED}");
}

fn retry_event() -> AgentEvent {
    AgentEvent::Retry {
        attempt: RETRY_ATTEMPT,
        message: RETRY_MESSAGE.into(),
        delay_ms: RETRY_DELAY_MS,
    }
}

fn app_with_retrying_subagent() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(retry_event(), TASK_ID, Some(RESEARCH_NAME)));
    app
}

/// The parent's task header already says a retry happened; the transcript the
/// user opens to watch that task showed nothing at all.
#[test]
fn a_subagent_retry_is_recorded_on_the_chat_it_belongs_to() {
    let before = Instant::now();
    let app = app_with_retrying_subagent();
    let after = Instant::now();

    let retry = app.chats[1].retry().expect(SUBAGENT_RETRY_MISSING);
    assert_eq!(retry.message, RETRY_MESSAGE);
    assert_eq!(retry.attempt, RETRY_ATTEMPT);
    assert!(retry.deadline >= before + RETRY_DELAY);
    assert!(retry.deadline <= after + RETRY_DELAY);
    assert!(app.chats[0].retry().is_none(), "{SUBAGENT_RETRY_LEAKED}");
}

#[test_case(None ; "main_chat")]
#[test_case(Some(RETRY_SIBLING_TASK_ID) ; "sibling_task")]
fn another_chats_event_leaves_the_subagent_retry_standing(source: Option<&str>) {
    let mut app = app_with_retrying_subagent();
    let deadline = app.chats[1].retry().unwrap().deadline;
    let event = AgentEvent::ToolPending {
        id: SUB_TOOL_ID.into(),
        name: SHELL_TOOL_NAME.into(),
    };
    app.update(match source {
        Some(task) => subagent_msg(event, task, Some(RESEARCH_NAME)),
        None => agent_msg(event),
    });

    assert_eq!(app.chats[1].retry().unwrap().deadline, deadline);

    app.update(subagent_msg(
        AgentEvent::StreamReset,
        TASK_ID,
        Some(RESEARCH_NAME),
    ));
    assert!(app.chats[1].retry().is_none());
}

#[test_case(false ; "cancel_main_run")]
#[test_case(true ; "cancel_only_the_task")]
fn cancelling_a_long_retry_clears_the_affected_countdowns(cancel_task: bool) {
    let mut app = app_with_retrying_subagent();
    app.update(agent_msg(retry_event()));
    let main_deadline = app.chats[0].retry().unwrap().deadline;

    if cancel_task {
        assert!(matches!(
            &app.cancel_subagent(TASK_ID.into())[..],
            [Action::CancelSubagent { tool_use_id }] if tool_use_id == TASK_ID
        ));
        assert_eq!(app.chats[0].retry().unwrap().deadline, main_deadline);
    } else {
        let cancelled_run = app.run_id;
        assert!(matches!(
            &app.handle_cancel()[..],
            [Action::CancelAgent { run_id }] if *run_id == cancelled_run
        ));
        assert!(app.chats[0].retry().is_none());
    }
    assert!(app.chats[1].retry().is_none());
    assert_eq!(app.chats[1].task_outcome(), Some(TaskOutcome::Killed));
}

/// A running task publishes a digest on every activity change, so a single
/// clear that ignored which chat an event addressed wiped the main chat's
/// countdown almost as soon as it appeared.
#[test_case(progress_event(SubagentActivity::Responding, 1) ; "progress_digest")]
#[test_case(AgentEvent::StreamReset ; "stream_reset")]
#[test_case(AgentEvent::ToolPending { id: SUB_TOOL_ID.into(), name: "bash".into() } ; "tool_event")]
fn a_subagents_event_leaves_the_main_retry_standing(event: AgentEvent) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(retry_event()));
    let deadline = app.chats[0].retry().unwrap().deadline;

    app.update(subagent_msg(event, TASK_ID, Some(RESEARCH_NAME)));
    assert!(app.chats[0].retry().is_some(), "{MAIN_RETRY_WIPED}");
    assert_eq!(app.chats[0].retry().unwrap().deadline, deadline);

    app.update(agent_msg(AgentEvent::ToolPending {
        id: "t1".into(),
        name: "bash".into(),
    }));
    assert!(app.chats[0].retry().is_none(), "{MAIN_RETRY_STUCK}");
}

/// Nothing else is in flight while a task sleeps off its backoff, so the loop
/// has only the countdown to stay awake for.
#[test]
fn a_backgrounded_tasks_backoff_keeps_the_loop_awake() {
    let mut app = app_with_retrying_subagent();
    app.status = Status::Idle;
    assert_eq!(app.active_chat, 0);

    assert!(app.has_lifecycle_work(), "{BACKOFF_SLEPT}");
    assert!(app.chats[1].cadence().moves());
    assert_eq!(app.chats[1].task_outcome(), None);

    assert!(matches!(
        &app.cancel_subagent(TASK_ID.into())[..],
        [Action::CancelSubagent { tool_use_id }] if tool_use_id == TASK_ID
    ));
    assert!(app.chats[1].retry().is_none());
    assert_eq!(app.chats[1].task_outcome(), Some(TaskOutcome::Killed));
    assert!(!app.has_lifecycle_work());
}

/// The bar is drawn for whichever chat is on screen, so it must read that
/// chat's backoff and no other's. Only the main chat's countdown answers the
/// pointer: an immediate retry reaches the top-level agent alone, so clicking a
/// task's chip would cut the main conversation's backoff short and leave the
/// task on screen waiting out the one the user asked to skip.
#[test_case(0, 0, true, true   ; "the_main_chat_is_waiting_it_out")]
#[test_case(1, 0, false, false ; "the_main_chat_is_not_the_one_retrying")]
#[test_case(1, 1, true, false  ; "the_retrying_task_is_on_screen")]
fn the_focused_chats_countdown_is_drawn_but_only_the_main_chats_is_clickable(
    retrying: usize,
    focused: usize,
    expect_chip: bool,
    expect_control: bool,
) {
    let mut app = app_with_retrying_subagent();
    if retrying == 0 {
        app.chats[1].clear_retry();
        app.chats[0].set_retry(retry_info());
    }
    app.active_chat = focused;

    let bar = rendered_wide(&mut app, SIGMA_BAR_WIDTH);

    assert_eq!(
        bar.contains(RETRY_COUNTDOWN_PREFIX),
        expect_chip,
        "{COUNTDOWN_DRAWN_FOR_THE_WRONG_CHAT}"
    );
    assert_eq!(
        app.status_hits
            .iter()
            .any(|hit| hit.target == StatusBarHitTarget::Retry),
        expect_control,
        "{RETRY_CONTROL_MISPLACED}"
    );
}

#[test]
fn auth_stream_reset_clears_partial_tools_without_retry_status() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::ToolPending {
        id: "t1".into(),
        name: "bash".into(),
    }));

    app.update(agent_msg(AgentEvent::StreamReset));

    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert!(app.chats[0].retry().is_none());
}

fn auth_retry_enter(app: &mut App) -> Vec<Action> {
    app.update(Msg::Key(key(KeyCode::Enter)))
}

fn auth_retry_type_then_enter(app: &mut App) -> Vec<Action> {
    type_and_submit(app, "ignored")
}

#[test_case(auth_retry_enter          ; "bare_enter")]
#[test_case(auth_retry_type_then_enter ; "typed_text_then_enter")]
fn auth_retry_sends_empty_answer(submit: fn(&mut App) -> Vec<Action>) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let (tx, rx) = flume::unbounded();
    app.answer_tx = Some(tx);

    app.update(agent_msg(AgentEvent::AuthRequired));
    assert_eq!(
        app.pending_input,
        PendingInput::AuthRetry {
            waiters: HashSet::from([None])
        }
    );

    let actions = submit(&mut app);
    assert!(actions.is_empty());
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(rx.try_recv().unwrap(), "");
}

fn app_with_subagent_tx(id: &str) -> (App, flume::Receiver<String>, flume::Receiver<String>) {
    let (sub_tx, sub_rx) = flume::unbounded();
    let (main_tx, main_rx) = flume::unbounded();
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.answer_tx = Some(main_tx);
    app.update(Msg::Agent(Box::new(Envelope {
        task: None,
        event: AgentEvent::TextDelta { text: "x".into() },
        subagent: Some(subagent_info_with_tx(id, "research", Some(sub_tx))),
        run_id: 1,
        workflow: None,
    })));
    (app, sub_rx, main_rx)
}

#[test]
fn auth_required_in_subagent_shows_in_both_chats() {
    let mut app = app_with_subagent_id("sub1");
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));

    assert_eq!(app.chats[1].last_message_text(), AUTH_EXPIRED_MSG);
    assert_eq!(app.chats[0].last_message_text(), AUTH_EXPIRED_MSG);
    assert_eq!(
        app.pending_input,
        PendingInput::AuthRetry {
            waiters: HashSet::from([Some("sub1".into())])
        }
    );
}

#[test]
fn auth_retry_in_subagent_routes_to_subagent_channel() {
    let (mut app, sub_rx, main_rx) = app_with_subagent_tx("sub1");
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(actions.is_empty());
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(sub_rx.try_recv().unwrap(), "");
    assert!(main_rx.try_recv().is_err());
}

#[test]
fn auth_retry_wakes_every_waiting_agent() {
    let (mut app, sub_rx, main_rx) = app_with_subagent_tx("sub1");
    app.update(agent_msg(AgentEvent::AuthRequired));
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));

    app.update(Msg::Key(key(KeyCode::Enter)));

    assert_eq!(main_rx.try_recv().unwrap(), "");
    assert_eq!(sub_rx.try_recv().unwrap(), "");
    assert_eq!(app.pending_input, PendingInput::None);
}

#[test]
fn auth_restored_clears_only_the_matching_waiter() {
    let (mut app, _sub_rx, _main_rx) = app_with_subagent_tx("sub1");
    app.update(agent_msg(AgentEvent::AuthRequired));
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));

    app.update(subagent_msg(
        AgentEvent::AuthRestored,
        "sub1",
        Some("research"),
    ));

    assert_eq!(
        app.pending_input,
        PendingInput::AuthRetry {
            waiters: HashSet::from([None])
        }
    );
    app.update(agent_msg(AgentEvent::AuthRestored));
    assert_eq!(app.pending_input, PendingInput::None);
}

#[test]
fn cancel_clears_subagent_auth_retry() {
    let (mut app, sub_rx, _main_rx) = app_with_subagent_tx("sub1");
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));

    cancel_app(&mut app);

    assert_eq!(app.pending_input, PendingInput::None);
    assert!(sub_rx.try_recv().is_err());
}

#[test]
fn cancelling_one_auth_waiter_does_not_retry_it_as_the_main_agent() {
    let (mut app, sub_rx, main_rx) = app_with_subagent_tx("sub1");
    app.update(agent_msg(AgentEvent::AuthRequired));
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));
    app.focus_task("sub1").unwrap();

    let actions = app.handle_subagent_cancel();

    assert!(matches!(
        actions.as_slice(),
        [Action::CancelSubagent { tool_use_id }] if tool_use_id == "sub1"
    ));
    assert_eq!(
        app.pending_input,
        PendingInput::AuthRetry {
            waiters: HashSet::from([None])
        }
    );
    app.active_chat = 0;
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(main_rx.try_recv().unwrap(), "");
    assert!(sub_rx.try_recv().is_err());
}

#[test]
fn stale_auth_required_after_cancel_is_dropped() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 2;
    let count_before = app.chats[0].message_count();
    app.update(Msg::Agent(Box::new(Envelope {
        task: None,
        event: AgentEvent::AuthRequired,
        subagent: None,
        run_id: 1,
        workflow: None,
    })));
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(app.chats[0].message_count(), count_before);
}

#[test]
fn send_to_agent_unknown_subagent_falls_back_to_main() {
    let (main_tx, main_rx) = flume::unbounded();
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.answer_tx = Some(main_tx);

    app.pending_input = PendingInput::AuthRetry {
        waiters: HashSet::from([Some("nonexistent".into())]),
    };
    app.update(Msg::Key(key(KeyCode::Enter)));

    assert_eq!(main_rx.try_recv().unwrap(), "");
    assert_eq!(app.pending_input, PendingInput::None);
}

#[test_case(42, false ; "restores_scroll_position")]
#[test_case(0,  true  ; "restores_auto_scroll")]
fn search_escape_restores_scroll(scroll_top: u32, auto_scroll: bool) {
    let mut app = test_app();
    app.active_chat().restore_scroll(scroll_top, auto_scroll);

    app.update(Msg::Key(kb::SEARCH.to_key_event()));
    app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(!app.search_modal.is_open());
    assert_eq!(app.active_chat().scroll_top(), scroll_top);
    assert_eq!(app.active_chat().auto_scroll(), auto_scroll);
}

#[test]
fn mcp_command_opens_picker() {
    let mut app = test_app();
    app.execute_command(cmd("/mcp"), 0);
    assert!(app.mcp_picker.is_open());
}

#[test]
fn mcp_toggle_dispatches_action() {
    let mut app = test_app();
    app.mcp_picker = McpPicker::new(
        McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![McpServerInfo {
                name: "test-srv".into(),
                transport_kind: "stdio",
                tool_count: 2,
                prompt_count: 0,
                status: McpServerStatus::Running,
                config_path: PathBuf::from("/tmp/config.toml"),
                url: None,
                oauth: None,
                resolved_addresses: Vec::new(),
                review: McpReviewSummary {
                    command: Some(vec!["test-server".into()]),
                    url: None,
                    config_source: McpConfigSource::Runtime,
                    environment_names: vec![],
                    header_names: vec![],
                },
            }],
            prompts: vec![],
            pids: vec![],
            generation: 0,
        }),
        McpConfigErrors::new(PathBuf::new()),
    );
    app.execute_command(cmd("/mcp"), 0);

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(
        &actions[0],
        Action::ToggleMcp(name, false) if name == "test-srv"
    ));
}

#[test]
fn permissions_command_lists_current_conversation_rules() {
    let mut app = test_app();
    app.permissions
        .load_structured_conversation_rules(vec![conversation_permission_record()]);

    app.execute_command(cmd("/permissions"), 0);
    app.finish_permission_jobs();

    assert!(app.permissions_picker.is_open());
}

fn app_awaiting_permission_config_trust() -> App {
    let mut app = test_app();
    let project = PathBuf::from(&app.state.session.cwd);
    let rule = PermissionRule {
        tool: ToolKey::native("bash"),
        scope: Some(PROJECT_PERMISSION_TEST_SCOPE.into()),
        effect: Effect::Allow,
    };
    app.permissions = Arc::new(PermissionManager::new_persistent_in(
        PermissionsConfig {
            project_allow_rules: vec![rule.clone()],
            review_candidates: vec![PermissionReviewCandidate {
                source: PermissionSource::Project,
                kind: PermissionReviewKind::Rule,
                tool: Some(rule.tool),
                scope: rule.scope,
            }],
            ..Default::default()
        },
        project,
        Arc::default(),
        app.storage.clone(),
    ));
    assert!(app.permissions.needs_project_permission_config_trust());
    app
}

fn awaiting_mcp_picker() -> McpPicker {
    McpPicker::new(
        McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![McpServerInfo {
                name: "project-srv".into(),
                transport_kind: "stdio",
                tool_count: 0,
                prompt_count: 0,
                status: McpServerStatus::AwaitingTrust,
                config_path: PathBuf::from("/project/.caudra/config.toml"),
                url: None,
                oauth: None,
                resolved_addresses: Vec::new(),
                review: McpReviewSummary {
                    command: Some(vec!["project-server".into()]),
                    url: None,
                    config_source: McpConfigSource::Project,
                    environment_names: vec!["PROJECT_TOKEN".into()],
                    header_names: vec![],
                },
            }],
            prompts: vec![],
            pids: vec![],
            generation: 0,
        }),
        McpConfigErrors::new(PathBuf::new()),
    )
}

#[test]
fn mcp_trust_once_dispatches_action_without_toggle() {
    let mut app = test_app();
    app.mcp_picker = awaiting_mcp_picker();
    app.execute_command(cmd("/mcp"), 0);

    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    let actions = app.update(Msg::Key(key(KeyCode::Char('o'))));
    assert!(matches!(
        actions.as_slice(),
        [Action::TrustMcpOnce(server)] if server == "project-srv"
    ));
}

#[test_case(false, true ; "opens_without_login")]
#[test_case(true, false ; "does_not_disrupt_login")]
fn awaiting_mcp_trust_startup_behavior(needs_login: bool, opens: bool) {
    let mut app = test_app();
    app.mcp_picker = awaiting_mcp_picker();

    app.open_awaiting_mcp_trust(needs_login);

    assert_eq!(app.mcp_picker.is_open(), opens);
}

#[test_case(false, false, true  ; "opens_when_unblocked")]
#[test_case(true,  false, false ; "does_not_disrupt_login")]
#[test_case(false, true,  false ; "does_not_stack_over_mcp_trust")]
fn awaiting_permission_config_trust_startup_behavior(
    needs_login: bool,
    open_mcp_trust: bool,
    opens: bool,
) {
    let mut app = app_awaiting_permission_config_trust();
    if open_mcp_trust {
        app.mcp_picker = awaiting_mcp_picker();
        app.open_awaiting_mcp_trust(false);
    }

    app.open_awaiting_permission_config_trust(needs_login);
    app.finish_permission_jobs();

    assert_eq!(app.permissions_picker.is_open(), opens);
}

#[test]
fn deferred_permission_config_trust_opens_after_mcp_trust_settles() {
    let mut app = app_awaiting_permission_config_trust();
    app.mcp_picker = awaiting_mcp_picker();
    app.open_awaiting_mcp_trust(false);
    app.open_awaiting_permission_config_trust(false);
    assert!(!app.permissions_picker.is_open());

    app.mcp_picker = McpPicker::new(
        McpSnapshotReader::empty(),
        McpConfigErrors::new(PathBuf::new()),
    );
    let _ = app.tick();
    app.finish_permission_jobs();

    assert!(app.permissions_picker.is_open());
}

#[test]
fn deferred_permission_config_trust_does_not_close_a_manual_mcp_picker() {
    let mut app = app_awaiting_permission_config_trust();
    app.mcp_picker = awaiting_mcp_picker();
    app.open_awaiting_mcp_trust(false);
    app.open_awaiting_permission_config_trust(false);
    app.mcp_picker = McpPicker::new(
        McpSnapshotReader::empty(),
        McpConfigErrors::new(PathBuf::new()),
    );
    app.mcp_picker.open();

    let _ = app.tick();

    assert!(app.mcp_picker.is_open());
    assert!(!app.permissions_picker.is_open());
    assert!(app.permission_job_pending());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.mcp_picker.is_open());
    assert!(!app.permissions_picker.is_open());
    assert!(app.permission_config_trust_deferred);
    assert_eq!(app.tick_permission_config_trust(), Dirty::NO);
    app.finish_permission_jobs();
    assert!(app.permission_config_trust_deferred);
    assert_eq!(app.tick_permission_config_trust(), Dirty::YES);
    app.finish_permission_jobs();
    assert!(!app.permission_config_trust_deferred);
    assert!(app.permissions_picker.is_open());
}

#[test]
fn closing_login_advances_mcp_then_project_trust() {
    let mut app = app_awaiting_permission_config_trust();
    app.mcp_picker = awaiting_mcp_picker();
    app.login_picker.open(app.storage.clone());
    app.open_awaiting_mcp_trust(true);
    app.open_awaiting_permission_config_trust(true);

    let actions = app.handle_login_picker_action(LoginPickerAction::Close);

    assert!(actions.is_empty());
    assert!(!app.login_picker.is_open());
    assert!(app.mcp_picker.is_open());
    assert!(!app.permissions_picker.is_open());
    app.update(Msg::Key(key(KeyCode::Esc)));
    app.finish_permission_jobs();
    assert!(app.permissions_picker.is_open());
}

#[test]
fn permission_request_suspends_and_then_restores_project_trust() {
    let mut app = app_awaiting_permission_config_trust();
    app.open_awaiting_permission_config_trust(false);
    app.finish_permission_jobs();
    app.status = Status::Streaming;
    app.run_id = 1;
    assert!(app.permissions_picker.is_open());

    app.update(agent_msg(permission_event("request", "cargo check")));

    assert!(!app.permissions_picker.is_open());
    assert!(app.permission_prompt.is_open());
    app.update(Msg::Key(key(KeyCode::Esc)));
    let _ = app.tick();
    app.finish_permission_jobs();
    assert!(!app.permission_prompt.is_open());
    assert!(app.permissions_picker.is_open());
}

pub(super) fn pattern_suggestion_candidate(project: &Path) -> PatternCandidate {
    PatternCandidate {
        definition: PatternDefinition {
            version: PATTERN_SCHEMA_VERSION,
            name: PATTERN_TEST_COMMAND.into(),
            context: PatternContext {
                tool_identity: "workcell/shell".into(),
                executable_identity: PATTERN_TEST_COMMAND.into(),
                effective_workdir: project.to_string_lossy().into_owned(),
                path_binding: project.to_string_lossy().into_owned(),
                analysis_version: PATTERN_TEST_ANALYSIS.into(),
            },
            argv: vec![PatternToken::Exact {
                value: PATTERN_TEST_COMMAND.into(),
                role: ArgumentRole::Executable,
            }],
            slots: Vec::new(),
            combinations: SlotCombinations::Independent,
        },
        evidence: CandidateEvidence {
            support: SupportCount {
                observations: 2,
                independent_sessions: 2,
            },
            provenance: ObservationProvenance::Imported,
            sources: [PATTERN_TEST_SOURCE.into()].into(),
            outcomes: [(InvocationOutcome::Unknown, 2)].into(),
            first_seen_ms: 1,
            last_seen_ms: 1,
            distributions: Default::default(),
            tuples: Vec::new(),
        },
    }
}

fn pattern_suggestion_reply(project: &Path) -> Arc<PatternDiscoveryOutcome> {
    Arc::new(PatternDiscoveryOutcome::Ready(
        test_pattern_discovery_report(vec![pattern_suggestion_candidate(project)]),
    ))
}

#[test_case("discover", true; "discover_opens_proposals_directly")]
#[test_case("suggested", true; "suggested_alias_opens_proposals_directly")]
#[test_case("", false; "plain_permissions_opens_rules")]
fn permissions_command_selects_the_requested_mode_with_large_rule_inventory(
    argument: &str,
    discover: bool,
) {
    let mut app = test_app();
    let rules = (0..PERMISSIONS_MODE_RULE_COUNT)
        .map(|index| {
            let mut rule = conversation_permission_record().rule;
            rule.resources[0].selector = PermissionResourceSelector::CommandPattern {
                pattern: format!("opsctl inspect task-{index:03}"),
            };
            PermissionRuleRecord::conversation(rule).unwrap()
        })
        .collect::<Vec<_>>();
    app.permissions
        .load_structured_conversation_rules(rules.clone());
    app.set_pattern_suggestion_loader(Some(Arc::new(move |project, _| {
        let first = pattern_suggestion_candidate(&project);
        let mut second = first.clone();
        second.definition.name = PATTERN_TEST_OTHER_COMMAND.into();
        second.definition.context.executable_identity = PATTERN_TEST_OTHER_COMMAND.into();
        second.definition.argv[0] = PatternToken::Exact {
            value: PATTERN_TEST_OTHER_COMMAND.into(),
            role: ArgumentRole::Executable,
        };
        let (reply, receiver) = flume::bounded(1);
        reply
            .send(Arc::new(PatternDiscoveryOutcome::Ready(
                test_pattern_discovery_report(vec![first, second]),
            )))
            .unwrap();
        receiver
    })));
    assert_eq!(app.poll_pattern_suggestions(), Dirty::YES);
    app.run_cmdline(&format!("/permissions {argument}"), 0)
        .unwrap();
    app.finish_permission_jobs();
    assert!(app.permissions_picker.is_open());
    assert_eq!(app.permissions_picker.discovery_mode(), discover);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| {
            app.permissions_picker.view(frame, frame.area());
        })
        .unwrap();
    let screen = buffer_text(terminal.backend().buffer());
    for command in [PATTERN_TEST_COMMAND, PATTERN_TEST_OTHER_COMMAND] {
        assert_eq!(screen.contains(command), discover, "{screen}");
    }
    assert_eq!(screen.contains("Grants & policies"), !discover, "{screen}");
    assert_eq!(
        app.permissions.structured_rule_inventory().unwrap().len(),
        rules.len()
    );
}

#[test_case(false; "current_root_reply_applied_by_tick_not_render")]
#[test_case(true; "foreign_candidate_context_rejected")]
fn pattern_suggestions_are_polled_outside_rendering(foreign: bool) {
    let mut app = test_app();
    let project = app.permissions.project_cwd();
    let (request, requested) = flume::unbounded();
    let candidate = pattern_suggestion_candidate(if foreign {
        Path::new("/another/project")
    } else {
        &project
    });
    app.set_pattern_suggestion_loader(Some(Arc::new(move |project, _| {
        let (reply, receiver) = flume::bounded(1);
        reply
            .send(Arc::new(PatternDiscoveryOutcome::Ready(
                test_pattern_discovery_report(vec![candidate.clone()]),
            )))
            .unwrap();
        request.send(project).unwrap();
        receiver
    })));
    assert_eq!(requested.try_recv().unwrap(), project);
    let backend = ratatui::backend::TestBackend::new(TEST_AREA.width, TEST_AREA.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.view(frame)).unwrap();
    assert!(app.pending_pattern_suggestions.is_some());
    assert!(requested.is_empty());
    assert_eq!(app.poll_pattern_suggestions(), Dirty::YES);
    assert!(app.pending_pattern_suggestions.is_none());
    assert_eq!(
        app.permissions.pattern_proposal_inventory().2.len(),
        usize::from(!foreign)
    );
    if foreign {
        let DiscoveryState::Complete(outcome) = app.permissions_picker.discovery_state() else {
            panic!("missing discovery failure")
        };
        assert!(matches!(
            outcome.as_ref(),
            PatternDiscoveryOutcome::Unavailable(PATTERN_INVALID_CONTEXT)
        ));
    }
    assert_eq!(app.poll_pattern_suggestions(), Dirty::NO);
}

#[test_case(false; "refresh_cancelled_before_completion")]
#[test_case(true; "ready_reply_cancelled_before_poll")]
fn permissions_discovery_refresh_and_cancel_are_explicit_single_requests(
    ready_before_cancel: bool,
) {
    let mut app = test_app();
    let (request, requested) = flume::unbounded();
    app.set_pattern_suggestion_loader(Some(Arc::new(move |project, mode| {
        let (reply, receiver) = flume::bounded(1);
        request.send((project, mode, reply)).unwrap();
        receiver
    })));
    let (project, mode, first) = requested.try_recv().unwrap();
    assert_eq!(mode, PatternDiscoveryMode::Cached);
    first.send(pattern_suggestion_reply(&project)).unwrap();
    assert_eq!(app.poll_pattern_suggestions(), Dirty::YES);
    let before = app.permissions.structured_rule_inventory().unwrap();
    app.run_cmdline("/permissions discover", 0).unwrap();
    app.finish_permission_jobs();
    for _ in 0..3 {
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL,
        )));
    }
    let (project, mode, cancelled) = requested.try_recv().unwrap();
    assert_eq!(mode, PatternDiscoveryMode::Refresh);
    assert!(requested.is_empty());
    assert!(matches!(
        app.permissions_picker.discovery_state(),
        DiscoveryState::Loading
    ));
    if ready_before_cancel {
        cancelled.send(pattern_suggestion_reply(&project)).unwrap();
    }
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::CONTROL,
    )));
    assert!(cancelled.is_disconnected());
    assert!(app.permissions_picker.discovery_cancelled());
    assert_eq!(app.poll_pattern_suggestions(), Dirty::NO);
    app.request_pattern_suggestions();
    assert!(requested.is_empty());
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    )));
    let (project, mode, reply) = requested.try_recv().unwrap();
    assert_eq!(mode, PatternDiscoveryMode::Refresh);
    reply.send(pattern_suggestion_reply(&project)).unwrap();
    assert_eq!(app.poll_pattern_suggestions(), Dirty::YES);
    for code in ['g', 'r', 'x'] {
        app.update(Msg::Key(KeyEvent::new_with_kind(
            KeyCode::Char(code),
            KeyModifiers::CONTROL,
            KeyEventKind::Repeat,
        )));
    }
    assert!(requested.is_empty());
    assert!(app.pending_pattern_suggestions.is_none());
    assert_eq!(app.permissions.structured_rule_inventory().unwrap(), before);
}

#[test_case(false; "worker_reports_failure")]
#[test_case(true; "worker_channel_disconnects")]
fn permissions_discovery_failures_remain_visible_and_retryable(disconnected: bool) {
    let mut app = test_app();
    let (request, requested) = flume::unbounded();
    app.set_pattern_suggestion_loader(Some(Arc::new(move |_, _| {
        request.send(()).unwrap();
        let (reply, receiver) = flume::bounded(1);
        if !disconnected {
            reply
                .send(Arc::new(PatternDiscoveryOutcome::Unavailable(
                    PATTERN_WORKER_UNAVAILABLE,
                )))
                .unwrap();
        }
        receiver
    })));
    requested.try_recv().unwrap();
    assert_eq!(app.poll_pattern_suggestions(), Dirty::YES);
    let DiscoveryState::Complete(outcome) = app.permissions_picker.discovery_state() else {
        panic!("missing discovery failure")
    };
    let expected = if disconnected {
        PATTERN_WORKER_DISCONNECTED
    } else {
        PATTERN_WORKER_UNAVAILABLE
    };
    assert!(
        matches!(outcome.as_ref(), PatternDiscoveryOutcome::Unavailable(reason) if *reason == expected)
    );
    app.run_cmdline("/permissions discover", 0).unwrap();
    app.finish_permission_jobs();
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    )));
    requested.try_recv().unwrap();
    assert_eq!(app.poll_pattern_suggestions(), Dirty::YES);
    assert!(
        app.permissions
            .structured_rule_inventory()
            .unwrap()
            .is_empty()
    );
}

#[test_case(false; "project_revision_changed")]
#[test_case(true; "manager_replaced_at_same_project")]
fn completed_discovery_status_is_not_reused_across_permission_contexts(replace: bool) {
    let mut app = test_app();
    app.set_pattern_suggestion_loader(Some(Arc::new(move |project, _| {
        let (reply, receiver) = flume::bounded(1);
        reply.send(pattern_suggestion_reply(&project)).unwrap();
        receiver
    })));
    assert_eq!(app.poll_pattern_suggestions(), Dirty::YES);
    app.open_permissions_picker().unwrap();
    app.finish_permission_jobs();
    let project = app.permissions.project_cwd();
    if replace {
        app.permissions = Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            project,
            Arc::default(),
        ));
    } else {
        app.permissions.set_project(&project);
    }
    assert_eq!(app.poll_pattern_suggestions(), Dirty::YES);
    assert!(!app.permissions_picker.is_open());
    assert!(matches!(
        app.permissions_picker.discovery_state(),
        DiscoveryState::Idle
    ));
    assert!(app.pending_pattern_suggestions.is_none());
}

#[test_case("switch"; "directory_lifecycle_replaces_request_and_cancels_old_receiver")]
fn pattern_suggestion_context_switch_cancels_stale_work(_case: &str) {
    let mut app = test_app();
    let (request, requested) = flume::unbounded();
    app.set_pattern_suggestion_loader(Some(Arc::new(move |project, _| {
        let (reply, receiver) = flume::bounded(1);
        request.send((project, reply)).unwrap();
        receiver
    })));
    let (original, old_reply) = requested.try_recv().unwrap();
    app.request_pattern_suggestions();
    assert!(requested.is_empty());

    let temp = TempDir::new().unwrap();
    let project = std::fs::canonicalize(temp.path()).unwrap();
    app.install_working_directory(&project, PermissionsConfig::default());
    assert!(old_reply.is_disconnected());
    let (active, reply) = requested.try_recv().unwrap();
    assert_ne!(active, original);
    assert_eq!(active, project);
    reply.send(pattern_suggestion_reply(&active)).unwrap();
    assert_eq!(app.poll_pattern_suggestions(), Dirty::YES);
    assert!(app.pending_pattern_suggestions.is_none());
}

#[test_case(false; "manager_root_changed_outside_loader")]
#[test_case(true; "shutdown_cancels_pending_reply")]
fn pattern_suggestion_late_replies_cannot_cross_lifecycle(shutdown: bool) {
    let mut app = test_app();
    let (request, requested) = flume::unbounded();
    app.set_pattern_suggestion_loader(Some(Arc::new(move |project, _| {
        let (reply, receiver) = flume::bounded(1);
        request.send((project, reply)).unwrap();
        receiver
    })));
    let (project, reply) = requested.try_recv().unwrap();
    reply.send(pattern_suggestion_reply(&project)).unwrap();
    let temp = TempDir::new().unwrap();
    if shutdown {
        app.prepare_shutdown();
    } else {
        app.permissions.set_project(temp.path());
    }
    assert_eq!(app.poll_pattern_suggestions(), Dirty::NO);
    assert!(app.pending_pattern_suggestions.is_none());
    assert!(reply.is_disconnected());
}

#[test_case(false, false; "same_path_new_revision_rejects_ready_reply")]
#[test_case(true, false; "replacement_manager_same_path_and_revision_rejects_ready_reply")]
#[test_case(false, true; "same_path_new_revision_resubmits_and_cancels_old_work")]
#[test_case(true, true; "replacement_manager_resubmits_and_cancels_old_work")]
fn pattern_suggestion_reply_is_bound_to_the_captured_manager_context(
    replace: bool,
    resubmit: bool,
) {
    let mut app = test_app();
    let (request, requested) = flume::unbounded();
    app.set_pattern_suggestion_loader(Some(Arc::new(move |project, _| {
        let (reply, receiver) = flume::bounded(1);
        request.send((project, reply)).unwrap();
        receiver
    })));
    let (project, old_reply) = requested.try_recv().unwrap();
    let old_manager = Arc::clone(&app.permissions);
    let old_context = old_manager.pattern_candidate_context();
    assert_eq!(
        app.pending_pattern_suggestions.as_ref().unwrap().revision,
        old_context.1
    );
    if replace {
        app.permissions = Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            project.clone(),
            Arc::default(),
        ));
        assert_eq!(app.permissions.pattern_candidate_context(), old_context);
    } else {
        app.permissions.set_project(&project);
        assert_ne!(app.permissions.pattern_candidate_context().1, old_context.1);
    }
    old_reply.send(pattern_suggestion_reply(&project)).unwrap();
    if resubmit {
        app.request_pattern_suggestions();
        assert!(old_reply.is_disconnected());
        let (active, reply) = requested.try_recv().unwrap();
        let pending = app.pending_pattern_suggestions.as_ref().unwrap();
        assert!(pending.owner.ptr_eq(&Arc::downgrade(&app.permissions)));
        assert_eq!(
            pending.revision,
            app.permissions.pattern_candidate_context().1
        );
        reply.send(pattern_suggestion_reply(&active)).unwrap();
        assert_eq!(app.poll_pattern_suggestions(), Dirty::YES);
    } else {
        assert_eq!(app.poll_pattern_suggestions(), Dirty::NO);
        assert!(old_reply.is_disconnected());
    }
    assert!(app.pending_pattern_suggestions.is_none());
}

#[test_case(false; "remote_sessions_do_not_request_local_history")]
#[test_case(true; "ephemeral_sessions_do_not_request_persistent_history")]
fn pattern_suggestion_lifecycle_respects_privacy(ephemeral: bool) {
    let mut app = test_app();
    if ephemeral {
        app.storage = StateDir::split(
            app.storage.path().join("volatile"),
            app.storage.path().into(),
        );
    } else {
        app.workspace_session = Some(remote_workspace_session());
    }
    let (request, requested) = flume::unbounded();
    app.set_pattern_suggestion_loader(Some(Arc::new(move |project, _| {
        request.send(project).unwrap();
        flume::bounded(1).1
    })));
    app.request_pattern_suggestions();
    assert_eq!(app.poll_pattern_suggestions(), Dirty::NO);
    assert!(requested.is_empty());
    assert!(app.pending_pattern_suggestions.is_none());
}

#[test]
fn changing_projects_closes_stale_permission_config_actions() {
    let mut app = app_awaiting_permission_config_trust();
    app.open_awaiting_permission_config_trust(false);
    app.finish_permission_jobs();
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("destination");
    std::fs::create_dir(&project).unwrap();
    assert!(app.permissions_picker.is_open());

    app.install_working_directory(&project, PermissionsConfig::default());

    assert!(!app.permissions_picker.is_open());
    assert!(!app.permission_config_trust_deferred);
}

#[test]
fn closing_mcp_trust_opens_deferred_permission_config_trust() {
    let mut app = app_awaiting_permission_config_trust();
    app.mcp_picker = awaiting_mcp_picker();
    app.open_awaiting_mcp_trust(false);
    app.open_awaiting_permission_config_trust(false);

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    app.finish_permission_jobs();

    assert!(actions.is_empty());
    assert!(!app.mcp_picker.is_open());
    assert!(app.permissions_picker.is_open());
}

#[test]
fn trusting_project_permission_config_refreshes_picker() {
    let mut app = app_awaiting_permission_config_trust();
    app.execute_command(cmd("/permissions"), 0);
    app.finish_permission_jobs();
    assert_eq!(
        app.lifecycle_blocker().as_deref(),
        Some(PROJECT_PERMISSION_CONFIG_TRUST_BLOCKER)
    );

    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    app.finish_permission_jobs();

    assert!(!app.permissions.needs_project_permission_config_trust());
    assert!(app.permissions_picker.is_open());
    assert!(!app.permissions_picker.discovery_mode());
    assert!(app.permissions.active_policy().iter().any(|policy| {
        policy.origin == RuleOrigin::Config
            && policy.rule.tool == ToolKey::native("bash")
            && policy.rule.effect == Effect::Allow
            && policy.rule.scope.as_deref() == Some(PROJECT_PERMISSION_TEST_SCOPE)
    }));
    assert_eq!(app.lifecycle_blocker(), None);
    assert_eq!(
        app.status_bar.flash_text(),
        Some("Project permission config trusted")
    );
    let backend = ratatui::backend::TestBackend::new(120, 18);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            app.permissions_picker.view(frame, frame.area());
        })
        .unwrap();
    let screen = buffer_text(terminal.backend().buffer());
    assert!(!screen.contains("no authority has been granted"));
    assert!(screen.contains("Shell allow patterns are active"));
    assert!(screen.contains("Trust or revoke"));
    assert!(!screen.contains("needs review"));

    assert!(app.update(Msg::Key(key(KeyCode::Down))).is_empty());
    terminal
        .draw(|frame| {
            app.permissions_picker.view(frame, frame.area());
        })
        .unwrap();
    let screen = buffer_text(terminal.backend().buffer());
    for visible in ["Allow  bash:", "configuration", "read-only"] {
        assert!(screen.contains(visible), "{visible}: {screen}");
    }
    for word in PROJECT_PERMISSION_TEST_SCOPE.split_whitespace() {
        assert!(screen.contains(word), "{word}: {screen}");
    }
    assert!(app.permissions.project_permission_config_trusted());
    assert!(
        app.update(Msg::Key(workbench_keys::LIST_FIRST.to_key_event()))
            .is_empty()
    );

    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    assert!(app.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    app.finish_permission_jobs();
    assert!(app.permissions.needs_project_permission_config_trust());
    assert_eq!(
        app.status_bar.flash_text(),
        Some("Project permission config trust revoked")
    );
}

#[test]
fn project_permission_config_trust_blocks_only_while_picker_is_open() {
    let mut app = app_awaiting_permission_config_trust();
    assert_eq!(app.lifecycle_blocker(), None);

    app.execute_command(cmd("/permissions"), 0);
    app.finish_permission_jobs();
    assert_eq!(
        app.lifecycle_blocker().as_deref(),
        Some(PROJECT_PERMISSION_CONFIG_TRUST_BLOCKER)
    );
    app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(!app.permissions_picker.is_open());
    assert!(app.permissions.needs_project_permission_config_trust());
    assert_eq!(app.lifecycle_blocker(), None);
}

#[test_case(
    |app: &mut App| { app.state.mode = Mode::Plan; app.plan_form.on_plan_ready(); },
    ""
    ; "consumed_by_plan_form"
)]
#[test_case(
    |app: &mut App| {
        crate::push_history_message(app.state.session_mut(), Message::user("test".into()));
        app.open_rewind_picker();
    },
    ""
    ; "routed_to_open_picker"
)]
#[test_case(
    |app: &mut App| { app.update(Msg::Key(kb::SEARCH.to_key_event())); },
    ""
    ; "routed_to_search_modal"
)]
#[test_case(
    |_: &mut App| {},
    "pasted"
    ; "falls_through_to_input"
)]
fn paste_routing(setup: fn(&mut App), expected_input: &str) {
    let mut app = test_app();
    setup(&mut app);
    app.update(Msg::Paste("pasted".into()));
    assert_eq!(app.input_box.buffer.value(), expected_input);
}

#[test]
fn btw_empty_flashes_error() {
    let mut app = test_app();
    let actions = app.execute_command(
        ParsedCommand {
            name: "/btw".into(),
            args: String::new(),
        },
        0,
    );
    assert!(actions.is_empty());
    assert_eq!(
        app.status_bar.flash_text().unwrap(),
        "Usage: /btw <question>"
    );
}

#[test]
fn btw_with_question_returns_action() {
    let mut app = test_app();
    let actions = app.execute_command(
        ParsedCommand {
            name: "/btw".into(),
            args: "what is rust?".into(),
        },
        0,
    );
    assert!(matches!(&actions[..], [Action::Btw(q)] if q == "what is rust?"));
}

#[test]
fn btw_usage_settles_into_the_session_ledger() {
    const BTW_MODEL: &str = "btw-model";
    const BTW_COST: f64 = 0.25;
    let mut app = test_app();
    app.state.goal.set("ship it").unwrap();
    app.chats[0].context_size = 1000;

    let (tx, rx) = flume::bounded(1);
    let (trigger, _cancel) = caudra_agent::CancelToken::new();
    app.stream_modal.open(
        " /btw ",
        "why sqlite?".into(),
        StreamFooter::FollowUp,
        rx,
        trigger,
    );
    tx.send(StreamEvent::Done(StreamDone {
        usage: StreamUsage {
            usage: TokenUsage {
                input: 100,
                output: 40,
                cache_read: 900,
                ..Default::default()
            },
            cost: Some(BTW_COST),
            billing: Billing::Api,
            model: BTW_MODEL.into(),
            provider: TEST_PROVIDER.into(),
            purpose: LedgerPurpose::Btw,
        },
        answer: None,
    }))
    .unwrap();
    let _ = app.tick();

    assert_eq!(app.state.token_usage.input, 100);
    assert_eq!(app.state.token_usage.cache_read, 900);
    assert_eq!(app.state.cost, Some(BTW_COST));
    assert_eq!(
        app.chats[0].cost,
        Some(BTW_COST),
        "the status bar renders the focused chat's cost, not the session total"
    );
    let billed = &app.state.session.usage_by_model()[&format!("{TEST_PROVIDER}/{BTW_MODEL}")];
    assert_eq!(billed.input, 100);
    assert_eq!(billed.cost, Some(BTW_COST));
    assert_eq!(
        app.state.goal.snapshot().unwrap().usage.input,
        100,
        "btw spends inside the goal window, so the goal is charged"
    );
    assert_eq!(
        app.chats[0].context_size, 1000,
        "btw never enters history, so it must not move the context gauge"
    );
}

#[test]
fn btw_modal_key_routing_and_animation() {
    let mut app = test_app();
    let (tx, rx) = flume::bounded(1);
    let (trigger, _cancel) = caudra_agent::CancelToken::new();
    app.stream_modal
        .open(" /btw ", "test".into(), StreamFooter::FollowUp, rx, trigger);

    // The spinner carries the wait before the first token; once text flows the
    // typewriter revealing it wins.
    assert!(app.stream_modal.is_streaming());
    assert_eq!(app.stream_modal.cadence(), Cadence::SPINNER);
    tx.send(StreamEvent::TextDelta("hi".into())).unwrap();
    assert_eq!(app.stream_modal.poll(), Dirty::YES);
    assert_eq!(app.stream_modal.cadence(), Cadence::SMOOTH);

    let actions = app.update(Msg::Key(key(KeyCode::Char('x'))));
    assert!(actions.is_empty());
    assert!(app.stream_modal.is_open());
    assert_eq!(
        app.input_box.buffer.value(),
        "",
        "a key typed at the modal never reaches the composer"
    );
    assert_eq!(app.stream_modal.input_text(), "x");

    app.update(Msg::Key(kb::DELETE_WORD.to_key_event()));
    assert_eq!(app.stream_modal.input_text(), "", "{BTW_CHORD_SWALLOWED}");
    assert!(app.stream_modal.is_open(), "{BTW_CHORD_SWALLOWED}");

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());
    assert!(!app.stream_modal.is_open());
    assert_eq!(app.stream_modal.cadence(), Cadence::IDLE);
}

/// A held `Backspace` keeps deleting in the follow-up, as it does in the
/// composer, rather than stopping after the first character.
#[test_case(KeyEventKind::Press; "legacy_autorepeat")]
#[test_case(KeyEventKind::Repeat; "reported_autorepeat")]
fn a_held_backspace_keeps_deleting_the_btw_follow_up(kind: KeyEventKind) {
    let mut app = btw_awaiting_follow_up();
    for _ in 0..2 {
        dispatch_reported_key(&mut app, key(KeyCode::Backspace), kind);
    }
    assert_eq!(app.stream_modal.input_text(), BTW_FOLLOW_UP_TRIMMED);
}

fn btw_ready_app() -> App {
    let mut app = test_app();
    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(HistorySnapshot::new(
        crate::history_items(&[
            Message::user("pick a database".into()),
            assistant_message("sqlite"),
        ]),
    ))));
    app.btw_prompt = Some(Arc::new(ArcSwap::from_pointee(crate::agent::BtwPrompt {
        provider: Arc::new(PendingProvider),
        model: test_model(),
        system: "system".into(),
        tools: serde_json::json!([]),
        opts: caudra_providers::RequestOptions::default(),
    })));
    app
}

fn notice_texts(app: &mut App) -> Vec<String> {
    let chat = app.main_chat();
    (0..chat.message_count())
        .filter_map(|i| chat.message_at(i))
        .filter(|m| m.role == DisplayRole::Notice)
        .map(|m| m.text.clone())
        .collect()
}

/// The marker shows where the thread's view of the conversation ends, and
/// leaves with the thread however the modal was dismissed.
#[test]
fn btw_marks_the_cutoff_and_clears_it_when_the_modal_closes() {
    let mut app = btw_ready_app();
    let before = app.main_chat().message_count();

    app.start_btw("why sqlite?".into());

    assert!(app.stream_modal.is_open());
    assert!(app.btw_thread.is_some());
    assert_eq!(app.main_chat().message_count(), before + 1);
    assert_eq!(
        notice_texts(&mut app),
        [super::btw::BTW_CUTOFF_MARKER],
        "the marker is the newest row"
    );

    app.main_chat().push(DisplayMessage::new(
        DisplayRole::Assistant,
        "the agent kept talking".into(),
    ));
    app.update(Msg::Key(key(KeyCode::Esc)));
    let _ = app.tick();

    assert!(app.btw_thread.is_none());
    assert!(notice_texts(&mut app).is_empty(), "the marker is gone");
    assert_eq!(
        app.main_chat().message_count(),
        before + 1,
        "only the marker left; the row appended after it stays"
    );
}

const BTW_QUESTION: &str = "why sqlite?";
const BTW_HEADER: &str = "Q: why sqlite?";
const BTW_FOLLOW_UP: &str = "and postgres?";
const BTW_FOLLOW_UP_TRIMMED: &str = "and postgre";
const BTW_CHORD_SWALLOWED: &str = "an editing chord at /btw must edit the follow-up field";
const BTW_FOLLOW_UP_HEADER: &str = "Q: and postgres?";

fn btw_answer() -> StreamEvent {
    StreamEvent::Done(StreamDone {
        usage: StreamUsage {
            usage: TokenUsage::default(),
            cost: None,
            billing: Billing::Api,
            model: "m".into(),
            provider: TEST_PROVIDER.into(),
            purpose: LedgerPurpose::Btw,
        },
        answer: Some("it ships in the binary".into()),
    })
}

/// A live `/btw` whose answer is stood in for by a channel the test owns: the
/// stub provider never answers on its own.
fn btw_streaming(app: &mut App) -> flume::Sender<StreamEvent> {
    app.start_btw(BTW_QUESTION.into());
    let (tx, rx) = flume::bounded(1);
    let (trigger, _cancel) = caudra_agent::CancelToken::new();
    app.stream_modal.open(
        " /btw ",
        BTW_HEADER.into(),
        StreamFooter::FollowUp,
        rx,
        trigger,
    );
    tx
}

/// A btw whose first answer has landed, with the follow-up typed but not yet
/// sent.
fn btw_awaiting_follow_up() -> App {
    let mut app = btw_ready_app();
    btw_streaming(&mut app).send(btw_answer()).unwrap();
    let _ = app.tick();
    assert!(
        app.btw_thread.is_some(),
        "a settled answer keeps the modal's thread"
    );
    assert_eq!(app.btw_thread.as_ref().unwrap().exchange_count(), 1);
    for c in BTW_FOLLOW_UP.chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app
}

fn assert_btw_follow_up_in_flight(app: &App) {
    let thread = app.btw_thread.as_ref().unwrap();
    assert_eq!(thread.exchange_count(), 1);
    assert_eq!(thread.pending(), Some(BTW_FOLLOW_UP));
    assert!(
        app.stream_modal.is_streaming(),
        "the follow-up is in flight"
    );
    assert_eq!(
        app.stream_modal.headers(),
        [BTW_HEADER, BTW_FOLLOW_UP_HEADER]
    );
    assert_eq!(app.stream_modal.input_text(), "");
}

/// A follow-up typed into the modal extends the same thread: the answered
/// question is filed, the new one is pending, and a fresh exchange opens.
#[test]
fn a_btw_follow_up_extends_the_thread_over_the_same_snapshot() {
    let mut app = btw_awaiting_follow_up();

    app.update(Msg::Key(key(KeyCode::Enter)));

    assert_btw_follow_up_in_flight(&app);
}

/// Typing the next question while the answer is still coming used to be
/// silently ignored. It is held instead, and sent once the answer it follows
/// up has been filed, so the thread extends rather than restarting.
#[test]
fn a_btw_question_queued_mid_answer_is_sent_when_the_answer_lands() {
    let mut app = btw_ready_app();
    let tx = btw_streaming(&mut app);

    for c in BTW_FOLLOW_UP.chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.stream_modal.input_text(), "", "the question is held");
    assert_eq!(
        app.btw_thread.as_ref().unwrap().exchange_count(),
        0,
        "nothing is filed while the first answer is still coming"
    );

    tx.send(btw_answer()).unwrap();
    let _ = app.tick();

    assert_btw_follow_up_in_flight(&app);
}

/// A click on the footer's send control takes the same route as Enter.
#[test]
fn a_btw_follow_up_is_sent_by_clicking_the_footer() {
    const SEND: usize = 0;
    let mut app = btw_awaiting_follow_up();
    let _ = rendered(&mut app);
    let send = app.stream_modal.footer_hit(SEND);
    assert!(!send.is_empty());

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        send.x,
        send.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        send.x,
        send.y,
    ));

    assert_btw_follow_up_in_flight(&app);
}

#[test]
fn extract_command_returns_action() {
    let mut app = test_app();
    let actions = app.execute_command(
        ParsedCommand {
            name: "/extract".into(),
            args: String::new(),
        },
        0,
    );
    assert!(matches!(&actions[..], [Action::Extract]));
}

#[test]
fn extract_on_a_session_without_user_turns_flashes_instead_of_opening() {
    let mut app = test_app();
    let chat = crate::agent::ModelSlot {
        model: test_model(),
        provider: Arc::new(PendingProvider),
    };
    app.start_extract(caudra_providers::Timeouts::default(), &chat);
    assert!(!app.stream_modal.is_open());
    assert_eq!(
        app.status_bar.flash_text().unwrap(),
        super::extract::NOTHING_TO_EXTRACT
    );
}

/// `y` hands the list to the clipboard and leaves the modal open, so the user
/// can keep reading or copy again once more of it has streamed.
#[test]
fn extract_modal_copy_keeps_the_modal_open() {
    let mut app = test_app();
    let (tx, rx) = flume::bounded(1);
    let (trigger, _cancel) = caudra_agent::CancelToken::new();
    app.stream_modal.open(
        " /extract ",
        "Extracting…".into(),
        StreamFooter::Copy,
        rx,
        trigger,
    );
    tx.send(StreamEvent::TextDelta("- ship it".into())).unwrap();
    assert_eq!(app.stream_modal.poll(), Dirty::YES);

    let actions = app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert!(actions.is_empty());
    assert!(app.stream_modal.is_open());
    assert!(
        app.status_bar.flash_text().is_some(),
        "the copy reports its outcome in the status bar"
    );
}

#[test]
fn extract_usage_settles_under_the_extract_purpose() {
    const EXTRACT_MODEL: &str = "extract-model";
    const EXTRACT_COST: f64 = 0.05;
    let (_tmp, dir, writer, mut app) = tempdir_app();
    app.chats[0].context_size = 1000;

    let (tx, rx) = flume::bounded(1);
    let (trigger, _cancel) = caudra_agent::CancelToken::new();
    app.stream_modal.open(
        " /extract ",
        "Extracting…".into(),
        StreamFooter::Copy,
        rx,
        trigger,
    );
    tx.send(StreamEvent::Done(StreamDone {
        usage: StreamUsage {
            usage: TokenUsage {
                input: 300,
                output: 60,
                ..Default::default()
            },
            cost: Some(EXTRACT_COST),
            billing: Billing::Api,
            model: EXTRACT_MODEL.into(),
            provider: TEST_PROVIDER.into(),
            purpose: LedgerPurpose::Extract,
        },
        answer: None,
    }))
    .unwrap();
    let _ = app.tick();

    assert_eq!(app.state.token_usage.input, 300);
    assert_eq!(app.state.cost, Some(EXTRACT_COST));
    assert_eq!(
        app.chats[0].context_size, 1000,
        "an extraction never enters history, so it must not move the context gauge"
    );
    drain_writer(app, writer);

    let rows = UsageLedger::open(&dir).unwrap().buckets(None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].purpose, LedgerPurpose::Extract.storage_name());
    assert_eq!(rows[0].cost, EXTRACT_COST);
}

#[test]
fn overlay_zone_click_gating() {
    let mut app = test_app();
    let msg = Rect::new(0, 0, 80, 15);
    let overlay = Rect::new(10, 3, 60, 10);
    set_zone(&mut app, SelectionZone::Messages, msg);
    set_zone(&mut app, SelectionZone::Overlay, overlay);
    app.help_modal.toggle();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 1));
    assert!(app.selection_state.is_none());

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 20, 5));
    let state = app.selection_state.as_ref().unwrap();
    assert_eq!(state.sel().zone, SelectionZone::Overlay);
}

#[test]
fn top_modal_blocks_stale_plan_row_clicks() {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.plan_form.on_plan_ready();
    let _ = rendered(&mut app);
    let row = app.plan_form.row_area(2).unwrap();
    app.help_modal.toggle();
    let _ = rendered(&mut app);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        row.x,
        row.y,
    ));
    let actions = app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        row.x,
        row.y,
    ));

    assert!(actions.is_empty());
    assert!(app.plan_form.is_visible());
    assert_eq!(app.state.mode, Mode::Plan);
}

#[test]
fn picker_overlay_blocks_plan_row_clicks() {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.plan_form.on_plan_ready();
    let _ = rendered(&mut app);
    let row = app.plan_form.row_area(2).unwrap();
    app.model_picker.open(&app.state.model, &app.model_policy);
    let _ = rendered(&mut app);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        row.x,
        row.y,
    ));
    let actions = app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        row.x,
        row.y,
    ));

    assert!(actions.is_empty());
    assert!(app.plan_form.is_visible());
    assert_eq!(app.state.mode, Mode::Plan);
}

const OUTSIDE_MODAL: (u16, u16) = (0, 0);
const MODAL_CENTRE: (u16, u16) = (80 / 2, 24 / 2);
const LEFT_STANDING: &str = "a press outside the modal should have dismissed it";
const DISMISSED: &str = "a press inside the modal should have left it standing";
const PRESS_SWALLOWED: &str = "the press that dismisses must not also start a selection";
const ARGS_COMMAND: &str = "btw";

fn open_help_modal(app: &mut App) {
    app.help_modal.toggle();
}

fn open_usage_modal(app: &mut App) {
    app.usage_modal.toggle();
}

fn open_context_modal(app: &mut App) {
    app.context_modal.open(false);
}

fn open_logs_modal(app: &mut App) {
    app.logs_modal.open();
}

fn open_docs_modal(app: &mut App) {
    app.execute_command(cmd(DOCS_COMMAND), 0);
}

fn open_tools_modal(app: &mut App) {
    app.tools_modal.open();
}

fn open_decisions_modal(app: &mut App) {
    app.decisions_modal.open();
}

fn open_skills_modal(app: &mut App) {
    app.skills_modal.open();
}

fn open_storage_modal(app: &mut App) {
    app.storage_modal.open(false);
}

fn open_system_prompt_modal(app: &mut App) {
    app.execute_command(cmd("/system-prompt"), 0);
}

fn open_projection_modal(app: &mut App) {
    app.execute_command(cmd("/projection"), 0);
}

fn open_goal_modal(app: &mut App) {
    app.goal_modal.open();
}

fn open_peer_manager(app: &mut App) {
    app.open_peer_manager(PeerView::Sessions);
}

fn open_model_picker(app: &mut App) {
    app.model_picker.open(&app.state.model, &app.model_policy);
}

fn open_command_modal(app: &mut App) {
    app.run_builtin(BuiltinAction::CommandPalette);
}

/// Reaches the argument prompt, the command modal's second stage.
fn open_argument_prompt(app: &mut App) {
    app.run_builtin(BuiltinAction::CommandPalette);
    app.route_text_paste(ARGS_COMMAND);
    app.update(Msg::Key(key(KeyCode::Enter)));
}

#[test_case(open_help_modal    ; "help_modal")]
#[test_case(open_usage_modal   ; "usage_modal")]
#[test_case(open_context_modal ; "context_modal")]
#[test_case(open_logs_modal    ; "logs_modal")]
#[test_case(open_docs_modal    ; "docs_modal")]
#[test_case(open_tools_modal   ; "tools_modal")]
#[test_case(open_decisions_modal ; "decisions_modal")]
#[test_case(open_skills_modal  ; "skills_modal")]
#[test_case(open_storage_modal ; "storage_modal")]
#[test_case(open_system_prompt_modal ; "system_prompt_modal")]
#[test_case(open_projection_modal ; "projection_modal")]
#[test_case(open_goal_modal    ; "goal_modal")]
#[test_case(open_model_picker  ; "model_picker")]
#[test_case(open_command_modal ; "command_modal")]
#[test_case(open_relocation_picker ; "relocation_picker")]
#[test_case(open_peer_manager ; "peer_manager")]
fn a_press_outside_a_modal_dismisses_it(open: fn(&mut App)) {
    let mut app = test_app();
    open(&mut app);
    let _ = rendered(&mut app);

    let (column, row) = OUTSIDE_MODAL;
    let actions = app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));

    assert!(actions.is_empty());
    assert!(!app.any_overlay_open(), "{LEFT_STANDING}");
    assert!(app.selection_state.is_none(), "{PRESS_SWALLOWED}");
}

#[test_case(open_help_modal    ; "help_modal")]
#[test_case(open_usage_modal   ; "usage_modal")]
#[test_case(open_context_modal ; "context_modal")]
#[test_case(open_logs_modal    ; "logs_modal")]
#[test_case(open_docs_modal    ; "docs_modal")]
#[test_case(open_tools_modal   ; "tools_modal")]
#[test_case(open_decisions_modal ; "decisions_modal")]
#[test_case(open_skills_modal  ; "skills_modal")]
#[test_case(open_storage_modal ; "storage_modal")]
#[test_case(open_system_prompt_modal ; "system_prompt_modal")]
#[test_case(open_projection_modal ; "projection_modal")]
#[test_case(open_goal_modal    ; "goal_modal")]
#[test_case(open_model_picker  ; "model_picker")]
#[test_case(open_command_modal ; "command_modal")]
#[test_case(open_peer_manager ; "peer_manager")]
fn a_press_inside_a_modal_leaves_it_standing(open: fn(&mut App)) {
    let mut app = test_app();
    open(&mut app);
    let _ = rendered(&mut app);

    let (column, row) = MODAL_CENTRE;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));

    assert!(app.any_overlay_open(), "{DISMISSED}");
}

/// The argument prompt is a stage of its own, and a press outside dismisses the
/// whole modal rather than stepping back to the list `Esc` would return to.
#[test]
fn a_press_outside_the_argument_prompt_dismisses_the_whole_command_modal() {
    let mut app = test_app();
    open_argument_prompt(&mut app);
    let _ = rendered(&mut app);
    assert!(app.command_modal.is_open());

    let (column, row) = OUTSIDE_MODAL;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));

    assert!(!app.command_modal.is_open(), "{LEFT_STANDING}");
}

/// A press on the argument prompt itself is a press on the modal, even though
/// the list it replaced is the only stage that scrolls.
#[test]
fn a_press_on_the_argument_prompt_leaves_it_standing() {
    let mut app = test_app();
    open_argument_prompt(&mut app);
    let _ = rendered(&mut app);

    let (column, row) = MODAL_CENTRE;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));

    assert!(app.command_modal.is_open(), "{DISMISSED}");
}

#[test]
fn the_press_that_dismisses_a_modal_does_not_act_on_what_is_under_it() {
    let mut app = test_app();
    let _ = rendered(&mut app);
    let mode_control = app
        .status_hits
        .iter()
        .find(|hit| hit.target == StatusBarHitTarget::Mode)
        .expect("the mode control should be on the status bar")
        .area;
    let mode = app.state.mode;
    app.help_modal.toggle();
    let _ = rendered(&mut app);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        mode_control.x,
        mode_control.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        mode_control.x,
        mode_control.y,
    ));

    assert!(!app.help_modal.is_open(), "{LEFT_STANDING}");
    assert_eq!(app.state.mode, mode);
}

const SEARCH_SCROLL_TOP: u32 = 3;
const RESTORED_SCROLL: &str = "dismissing the search owes the transcript its scroll back";

#[test]
fn a_press_outside_the_search_modal_restores_the_scroll_it_saved() {
    let mut app = test_app();
    app.main_chat().push(DisplayMessage::new(
        DisplayRole::Assistant,
        "searchable item".into(),
    ));
    let _ = rendered(&mut app);
    app.search_modal.open(SEARCH_SCROLL_TOP, false);
    let _ = rendered(&mut app);
    let (column, row) = OUTSIDE_MODAL;
    assert!(!app.search_modal.contains(Position::new(column, row)));

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));

    assert!(!app.search_modal.is_open(), "{LEFT_STANDING}");
    assert_eq!(
        app.chats[0].scroll_top(),
        SEARCH_SCROLL_TOP,
        "{RESTORED_SCROLL}"
    );
    assert!(!app.chats[0].auto_scroll(), "{RESTORED_SCROLL}");
}

const ORIGIN_RESTORED: &str = "dismissing the picker owes the transcript it was opened from";

#[test]
fn a_press_outside_the_task_picker_returns_to_the_origin_transcript() {
    let mut app = app_with_subagent();
    app.focus_task(TASK_ID).unwrap();
    app.tasks_browse();
    let _ = rendered(&mut app);
    app.update(Msg::Key(key(KeyCode::Up)));
    assert_eq!(app.active_chat, 0, "the preview should have moved to Main");

    let (column, row) = OUTSIDE_MODAL;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));

    assert!(!app.task_picker.is_open(), "{LEFT_STANDING}");
    assert_eq!(app.active_chat, 1, "{ORIGIN_RESTORED}");
}

const FORM_STANDING: &str = "a docked form owns no outside and must survive any press";

#[test]
fn a_press_leaves_the_permission_prompt_standing() {
    let mut app = streaming_app();
    app.update(agent_msg(permission_event("request", "cargo check")));
    let _ = rendered(&mut app);
    assert!(app.permission_prompt.is_open());

    let (column, row) = OUTSIDE_MODAL;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));

    assert!(app.permission_prompt.is_open(), "{FORM_STANDING}");
}

#[test]
fn a_press_leaves_the_plan_form_standing() {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.plan_form.on_plan_ready();
    let _ = rendered(&mut app);

    let (column, row) = OUTSIDE_MODAL;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));

    assert!(app.plan_form.is_visible(), "{FORM_STANDING}");
}

/// It holds edits nothing else has a copy of, so a stray press must not be able
/// to throw them away.
#[test]
fn a_press_leaves_the_paste_editor_standing() {
    let mut app = test_app();
    app.update(Msg::Paste("a\nb\nc".into()));
    app.update(Msg::Key(key(KeyCode::Left)));
    app.update(Msg::Key(key(KeyCode::Left)));
    app.update(Msg::Key(key(KeyCode::Enter)));
    let _ = rendered(&mut app);
    assert!(app.paste_editor.is_open());

    let (column, row) = OUTSIDE_MODAL;
    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));

    assert!(app.paste_editor.is_open(), "{FORM_STANDING}");
}

#[test]
fn search_hover_outside_results_preserves_current_preview() {
    let mut app = test_app();
    for text in ["item zero", "item one", "item two"] {
        app.main_chat()
            .push(DisplayMessage::new(DisplayRole::Assistant, text.into()));
    }
    let _ = rendered(&mut app);
    app.run_builtin(BuiltinAction::Search);
    app.route_text_paste("item");
    let _ = rendered(&mut app);
    let row = app.search_modal.row_area(2).unwrap();

    app.update(mouse_event(MouseEventKind::Moved, row.x, row.y));
    let selected = app.search_modal.current_segment_index();
    app.update(mouse_event(
        MouseEventKind::Moved,
        row.x.saturating_sub(1),
        row.y,
    ));

    assert_eq!(app.search_modal.current_segment_index(), selected);
}

#[test]
fn dragging_picker_row_keeps_overlay_text_selection() {
    let mut app = test_app();
    app.main_chat().push(DisplayMessage::new(
        DisplayRole::Assistant,
        "searchable item".into(),
    ));
    let _ = rendered(&mut app);
    app.run_builtin(BuiltinAction::Search);
    app.route_text_paste("searchable");
    let _ = rendered(&mut app);
    let row = app.search_modal.row_area(0).unwrap();
    let end = (row.x + 4).min(row.right().saturating_sub(1));

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        row.x,
        row.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        end,
        row.y,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        end,
        row.y,
    ));

    assert!(app.search_modal.is_open());
    assert!(matches!(
        app.selection_state,
        Some(SelectionState::PendingCopy { .. })
    ));
}

fn streaming_app_with_history() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let history = vec![
        Message::user("hello".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "world".into(),
            }],
            ..Default::default()
        },
    ];
    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(
        caudra_agent::HistorySnapshot::new(crate::history_items(&history)),
    )));
    app
}

/// The stale event is dropped, yet the cancelled turn still reaches disk: the
/// next frame's checkpoint syncs the mirror whatever event arrived.
#[test_case(done() ; "stale_done")]
#[test_case(
    AgentEvent::Error { message: "timeout".into() } ; "stale_error"
)]
fn checkpoint_after_cancel_persists_the_cancelled_turn(event: AgentEvent) {
    let mut app = streaming_app_with_history();
    let old_run_id = app.run_id;
    cancel_app(&mut app);
    assert_ne!(app.run_id, old_run_id);
    assert!(app.state.session.messages().is_empty());

    app.update(agent_msg_with_run_id(event, old_run_id));
    app.checkpoint();
    assert_eq!(app.state.session.messages().len(), 2);
}

#[test]
fn parent_done_reconciles_unresolved_children_and_tools() {
    let mut app = streaming_app_with_history();
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "task1".into(),
        effect: ToolEffect::Unknown,
        tool: "task".into(),
        summary: "research".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));
    app.update(subagent_msg(
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: "child-tool".into(),
            effect: ToolEffect::Unknown,
            tool: "read".into(),
            summary: "reading".into(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        })),
        "task1",
        Some("research"),
    ));
    let buf = Arc::new(caudra_agent::SharedBuf::new());
    app.update(subagent_msg(
        AgentEvent::LiveToolBuf {
            id: "child-tool".into(),
            body: buf,
        },
        "task1",
        None,
    ));

    app.update(done_event());

    assert!(app.chats[1].is_finished());
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(
        app.chats[0]
            .last_message_text()
            .contains(MISSING_TOOL_COMPLETION)
    );
    app.checkpoint();
    assert!(app.state.session.subagents().is_empty());
    assert_eq!(app.state.session.messages().len(), 2);
    assert!(app.state.session.tool_outputs().is_empty());
    assert_eq!(app.cadence(), Cadence::IDLE);
}

#[test]
fn parent_error_refreshes_picker_and_persists_only_completed_children() {
    let mut app = streaming_app_with_history();
    app.update(subagent_msg_with_model(
        AgentEvent::TextDelta { text: "one".into() },
        "task1",
        "first",
        "model-a",
    ));
    finish_subagent(&mut app, "task1", false);
    app.update(subagent_msg_with_model(
        AgentEvent::TextDelta { text: "two".into() },
        "task2",
        "second",
        "model-b",
    ));
    app.update(subagent_msg_with_model(
        AgentEvent::TextDelta {
            text: "three".into(),
        },
        "task3",
        "third",
        "model-c",
    ));
    finish_subagent(&mut app, "task3", false);

    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));

    app.checkpoint();
    let saved: Vec<_> = app
        .state
        .session
        .subagents()
        .iter()
        .map(|subagent| {
            (
                subagent.tool_use_id.as_str(),
                subagent.name.as_str(),
                subagent.model.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        saved,
        vec![
            ("task1", "first", Some("model-a")),
            ("task3", "third", Some("model-c")),
        ]
    );
}

#[test]
fn reserved_shell_survives_parent_done_until_shell_done() {
    let mut app = streaming_app_with_history();
    let id = app.shell.reserve_id();

    app.update(done_event());
    assert!(app.shell.active_ids().contains(&id));

    app.handle_shell_event(shell::ShellEvent::Start {
        id: id.clone(),
        command: "true".into(),
    });
    assert_eq!(app.chats[0].in_progress_count(), 1);
    app.handle_shell_event(shell::ShellEvent::Done {
        id: id.clone(),
        command: "true".into(),
        output: String::new(),
        is_error: false,
        visible: false,
        max_output_lines: 10,
        max_output_bytes: 1_024,
    });
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert!(!app.shell.active_ids().contains(&id));
}

#[test]
fn active_shell_survives_agent_error_while_agent_and_child_tools_fail() {
    let mut app = streaming_app_with_history();
    let shell_id = app.shell.reserve_id();
    app.handle_shell_event(shell::ShellEvent::Start {
        id: shell_id.clone(),
        command: "true".into(),
    });
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "agent-tool".into(),
        effect: ToolEffect::Unknown,
        tool: "read".into(),
        summary: "reading".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));
    app.update(subagent_msg(
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: "child-tool".into(),
            effect: ToolEffect::Unknown,
            tool: "read".into(),
            summary: "reading".into(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        })),
        "task1",
        Some("research"),
    ));

    app.update(agent_msg(AgentEvent::Error {
        message: "provider overloaded".into(),
    }));

    assert_eq!(app.chats[0].in_progress_count(), 1);
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(app.chats[1].is_finished());

    app.handle_shell_event(shell::ShellEvent::Done {
        id: shell_id.clone(),
        command: "true".into(),
        output: String::new(),
        is_error: false,
        visible: false,
        max_output_lines: 10,
        max_output_bytes: 1_024,
    });
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert!(!app.shell.active_ids().contains(&shell_id));
}

#[test]
fn main_shell_exclusion_does_not_protect_same_id_in_child_chat() {
    let mut app = streaming_app_with_history();
    let id = app.shell.reserve_id();
    app.handle_shell_event(shell::ShellEvent::Start {
        id: id.clone(),
        command: "true".into(),
    });
    app.update(subagent_msg(
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: id.clone(),
            effect: ToolEffect::Unknown,
            tool: "read".into(),
            summary: "reading".into(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        })),
        "task1",
        Some("research"),
    ));

    app.update(done_event());

    assert_eq!(app.chats[0].in_progress_count(), 1);
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(app.chats[1].is_finished());
}

#[test]
fn error_event_matching_run_id_saves_session_and_queued_messages() {
    let mut app = streaming_app_with_history();
    app.queue_and_notify(queued_msg("next"));
    app.queue
        .set_delivery(caudra_agent::QueueDelivery::TogetherNextTurn);

    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));
    app.checkpoint();

    assert_eq!(app.state.session.messages().len(), 2);
    assert_eq!(
        app.state.session.meta.queued_messages,
        [stored_queued_prompt("next")]
    );
    assert!(app.state.session.meta.queued_messages_together);
    assert!(app.queue.is_empty());

    assert_eq!(
        app.state.session.meta.queued_messages,
        [stored_queued_prompt("next")]
    );

    type_and_submit(&mut app, "replacement");
    app.checkpoint();
    assert!(app.state.session.meta.queued_messages.is_empty());
    assert!(!app.state.session.meta.queued_messages_together);
}

#[test]
fn flush_restored_queue_drops_recovery_snapshot() {
    let mut app = streaming_app_with_history();
    app.queue_and_notify(queued_msg("next"));
    app.queue
        .set_delivery(caudra_agent::QueueDelivery::TogetherNextTurn);
    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));
    app.checkpoint();
    assert_eq!(
        app.state.session.meta.queued_messages,
        [stored_queued_prompt("next")]
    );
    assert!(app.state.session.meta.queued_messages_together);

    app.flush_restored_queue();
    app.checkpoint();
    assert_eq!(
        app.state.session.meta.queued_messages,
        [stored_queued_prompt("next")]
    );
    assert!(app.state.session.meta.queued_messages_together);
    assert!(app.recoverable_queue.is_empty());

    app.queue.clear();
    app.checkpoint();
    assert!(app.state.session.meta.queued_messages.is_empty());
}

// --- Plan form integration tests ---

fn implement_msg(app: &App, remote: bool, parallel: bool) -> String {
    let header = if remote {
        format!("{IMPLEMENT_VERB} {}.", plan::SESSION_PLAN_LABEL)
    } else {
        let path = Path::new(&app.state.session.cwd).join(HANDOFF_PLAN_FILE);
        format!("{IMPLEMENT_MSG_PREFIX} from `{}`.", path.display())
    };
    let hint = if parallel {
        format!(" {IMPLEMENT_PARALLEL_HINT}")
    } else {
        String::new()
    };
    format!("{header}{hint}\n\n{HANDOFF_PLAN_CONTENT}")
}

/// A workspace-bound app whose plan lives in the document store and is ready.
fn remote_plan_app() -> App {
    let mut app = test_app();
    let workspace = workbench_workspace();
    let store = Arc::new(LocalDocumentStore::remote(
        app.storage.clone(),
        workspace.binding(),
    ));
    let session = app.state.session.id.to_string();
    let reference = store.create_plan(store.project_key(), &session).unwrap();
    store
        .write(
            store.project_key(),
            Some(&session),
            &LocalDocumentRef::Plan(reference.clone()),
            HANDOFF_PLAN_CONTENT,
        )
        .unwrap();
    app.workspace_session = Some(workspace);
    app.local_documents = Some(store);
    app.state.mode = Mode::Plan;
    app.state.applied_mode = Mode::Plan;
    app.state.plan = PlanState::RemoteDrafting(reference);
    app.transition_plan(PlanTrigger::WriteDone);
    app
}

pub(crate) fn plan_app() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.state.mode = Mode::Plan;
    app.state.applied_mode = Mode::Plan;
    PlanFile::new(Path::new(&app.state.session.cwd).join(HANDOFF_PLAN_FILE))
        .unwrap()
        .write(HANDOFF_PLAN_CONTENT)
        .unwrap();
    app.state.plan = PlanState::Drafting(PathBuf::from(HANDOFF_PLAN_FILE));
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 42 bytes to test-plan.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("test-plan.md".into()),
        written_paths: Vec::new(),
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    }))));
    app
}

#[test_case(Mode::Plan,  true  ; "plan_mode_tooldone_opens_form")]
#[test_case(Mode::Build, false ; "build_mode_tooldone_no_form")]
fn tool_done_write_opens_plan_form(mode: Mode, expect_form: bool) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.state.mode = mode;
    app.state.applied_mode = mode;
    app.state.plan = PlanState::Drafting(PathBuf::from("/tmp/plans/test.md"));
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 42 bytes to /tmp/plans/test.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("/tmp/plans/test.md".into()),
        written_paths: Vec::new(),
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    }))));
    assert_eq!(app.plan_form.is_visible(), expect_form);
    if expect_form {
        assert!(app.state.plan.is_ready());
    }
}

#[test]
fn done_event_does_not_open_plan_form() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Ready(PathBuf::from("test-plan.md"));
    app.update(done_event());
    assert!(!app.plan_form.is_visible());
}

#[test]
fn re_edit_keeps_plan_form_visible() {
    let mut app = plan_app();
    assert!(app.state.plan.is_ready());
    assert!(app.plan_form.is_visible());

    // Agent edits the plan again (second write to same path) — idempotent, stays Ready
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t2".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 50 bytes to test-plan.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("test-plan.md".into()),
        written_paths: Vec::new(),
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    }))));
    assert!(matches!(app.state.plan, PlanState::Ready(_)));
    assert!(app.plan_form.is_visible());
}

#[test_case(1, Mode::Build, true,  true  ; "clear_and_implement")]
#[test_case(2, Mode::Build, false, true  ; "implement_keeps_context")]
fn plan_form_menu_options(
    downs: usize,
    expected_mode: Mode,
    has_new_session: bool,
    has_send_message: bool,
) {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    for _ in 0..downs {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    if has_new_session {
        assert!(app.plan_form.is_visible());
        assert_eq!(app.state.mode, Mode::Plan);
        assert!(app.state.plan.is_ready());
        assert!(
            matches!(&actions[..], [Action::ClearAndImplement(handoff)] if handoff.input.message == implement_msg(&app, false, false))
        );
        return;
    }
    assert!(!app.plan_form.is_visible());
    assert_eq!(app.state.mode, expected_mode);
    assert_eq!(
        app.state.plan,
        PlanState::Drafting(PathBuf::from(HANDOFF_PLAN_FILE))
    );
    assert_eq!(
        actions
            .iter()
            .any(|a| matches!(a, Action::RequestNewSession)),
        has_new_session
    );
    let expected_msg = implement_msg(&app, false, PlanForm::new().parallel());
    assert_eq!(
        actions
            .iter()
            .any(|a| matches!(a, Action::SendMessage(i) if i.message == expected_msg)),
        has_send_message
    );
}

#[test]
fn plan_form_implement_toggled_parallel() {
    let mut app = plan_app();
    app.update(Msg::Key(key(KeyCode::Char(' '))));
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Down)));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    let expected_msg = implement_msg(&app, false, !PlanForm::new().parallel());
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::SendMessage(i) if i.message == expected_msg))
    );
}

#[test_case(false, false ; "local")]
#[test_case(false, true ; "local_parallel")]
#[test_case(true, false ; "remote")]
#[test_case(true, true ; "remote_parallel")]
fn implement_message_names_the_plan_and_never_a_revision(remote: bool, parallel: bool) {
    let mut app = if remote {
        remote_plan_app()
    } else {
        plan_app()
    };
    if parallel {
        app.update(Msg::Key(key(KeyCode::Char(' '))));
    }
    let expected = implement_msg(&app, remote, parallel);
    let actions = app.implement_plan(false);
    let message = actions
        .iter()
        .find_map(|action| match action {
            Action::SendMessage(input) => Some(&input.message),
            _ => None,
        })
        .unwrap();
    assert_eq!(*message, expected);
    assert!(!message.contains(IMPLEMENT_REVISION));
}

#[test_case(false ; "implement")]
#[test_case(true ; "clear_and_implement")]
fn plan_handoff_captures_content_before_consumption(clear: bool) {
    let mut app = plan_app();
    let expected = implement_msg(&app, false, false);
    let actions = app.implement_plan(clear);
    let path = Path::new(&app.state.session.cwd).join(HANDOFF_PLAN_FILE);
    PlanFile::new(path.clone())
        .unwrap()
        .write(HANDOFF_PLAN_CHANGED)
        .unwrap();
    let actions = if clear {
        let Action::ClearAndImplement(handoff) = actions.into_iter().next().unwrap() else {
            panic!("expected captured plan handoff");
        };
        assert!(app.plan_form.is_visible());
        assert!(app.state.plan.is_ready());
        app.reset_session_for_plan().unwrap();
        app.adopt_plan(&handoff);
        app.finish_plan_handoff(*handoff)
    } else {
        actions
    };
    let input = actions
        .iter()
        .find_map(|action| match action {
            Action::SendMessage(input) => Some(input),
            _ => None,
        })
        .unwrap();
    assert_eq!(input.message, expected);
    assert_eq!(input.mode, AgentMode::Build);
    let plan = PlanState::Drafting(if clear {
        path
    } else {
        PathBuf::from(HANDOFF_PLAN_FILE)
    });
    assert_eq!(input.plan, plan.target());
    assert_eq!(app.state.plan, plan);
}

/// Implement leaves the session bound to its plan, drafting again, so a
/// return to Plan revises the same document rather than starting another.
#[test_case(false; "local")]
#[test_case(true; "remote")]
fn implement_keeps_the_session_plan_for_a_return_to_plan(remote: bool) {
    let mut app = if remote {
        remote_plan_app()
    } else {
        plan_app()
    };
    let planning = app.agent_mode();
    let bound = app.state.plan.target();

    let actions = app.implement_plan(false);

    assert_eq!(sent_input(&actions).plan, bound);
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan.target(), bound);
    assert!(!app.state.plan.is_ready());
    app.checkpoint();
    assert!(app.state.session.meta.plan_target.is_some());
    assert!(!app.state.session.meta.plan_written);

    app.toggle_mode();

    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(app.agent_mode(), planning);
    assert_eq!(app.state.plan.target(), bound);
}

#[test_case(true, false; "reservation")]
#[test_case(false, false; "retirement")]
#[test_case(false, true; "admission")]
fn failed_clear_plan_transaction_preserves_source(reservation: bool, admission: bool) {
    let mut app = plan_app();
    app.status = Status::Idle;
    let plan = app.state.plan.clone();
    let session = app.state.session.id;
    let run = app.run_id;
    let messages = app.main_chat().message_count();
    let actions = app.implement_plan(true);
    assert!(matches!(&actions[..], [Action::ClearAndImplement(_)]));
    assert_eq!(app.run_id, run);
    assert_eq!(app.status, Status::Idle);
    let tmp = private_tempdir();
    let blocked = tmp.path().join("not-a-directory");
    fs::write(&blocked, b"blocked").unwrap();
    if reservation {
        app.storage = StateDir::from_path(blocked);
    } else {
        app.storage_writer = Arc::new(test_writer(StateDir::from_path(blocked)));
    }
    if admission {
        app.conversation_permissions = permission_editor::ConversationPermissions::Pending;
    }
    let error = app.reset_session_for_plan().err().unwrap();
    if reservation {
        assert!(error.starts_with("Failed to reserve new session:"));
    } else if !admission {
        assert!(error.starts_with("Failed to retire current session:"));
    }
    assert_eq!(app.state.session.id, session);
    assert_eq!(app.state.plan, plan);
    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(app.run_id, run);
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.main_chat().message_count(), messages);
    assert!(app.plan_form.is_visible());
}

#[test_case(false, None ; "implement_missing")]
#[test_case(true, None ; "clear_missing")]
#[test_case(false, Some("") ; "implement_empty")]
#[test_case(true, Some(" \n") ; "clear_blank")]
fn failed_plan_capture_preserves_state(clear: bool, content: Option<&str>) {
    let mut app = plan_app();
    let path = Path::new(&app.state.session.cwd).join(HANDOFF_PLAN_FILE);
    match content {
        Some(content) => {
            PlanFile::new(path).unwrap().write(content).unwrap();
        }
        None => fs::remove_file(path).unwrap(),
    }
    let plan = app.state.plan.clone();
    let session = app.state.session.id;
    let messages = app.main_chat().message_count();
    let run = app.run_id;
    let actions = app.implement_plan(clear);
    assert!(actions.is_empty());
    assert_eq!(app.state.plan, plan);
    assert_eq!(app.state.session.id, session);
    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(app.run_id, run);
    assert_eq!(app.main_chat().message_count(), messages);
    assert!(app.plan_form.is_visible());
    assert!(
        app.status_bar
            .flash_text()
            .unwrap()
            .starts_with(IMPLEMENT_CAPTURE_FAILED)
    );
}

#[test_case(false ; "implement")]
#[test_case(true ; "clear_and_implement")]
fn blocked_plan_submission_preserves_state(clear: bool) {
    let mut app = plan_app();
    app.cancelling_run = Some(app.run_id);
    let plan = app.state.plan.clone();
    assert!(app.implement_plan(clear).is_empty());
    assert_eq!(app.state.plan, plan);
    assert_eq!(app.state.mode, Mode::Plan);
    assert!(app.plan_form.is_visible());
    assert_eq!(app.status_bar.flash_text(), Some(REVERT_BUSY_MSG));
}

#[test]
fn a_child_plan_write_cannot_complete_the_main_plan() {
    let mut app = plan_app();
    app.state.plan.mark_drafting();
    app.plan_form.on_plan_drafting();
    let messages = app.main_chat().message_count();
    let mut done = ToolDoneEvent::error("child-plan".into(), HANDOFF_PLAN_CONTENT);
    done.is_error = false;
    done.written_path = Some(HANDOFF_PLAN_FILE.into());
    app.update(subagent_msg(
        AgentEvent::ToolDone(Box::new(done)),
        "child",
        None,
    ));
    assert!(!app.state.plan.is_ready());
    assert!(!app.plan_form.is_visible());
    assert_eq!(app.main_chat().message_count(), messages);
    assert!(!app.chats[1].last_message_is_plan());
}

#[test_case(Mode::Build, Mode::Plan, false, false, false, true ; "selected_build_executing_plan")]
#[test_case(Mode::Plan, Mode::Build, false, false, false, false ; "selected_plan_executing_build")]
#[test_case(Mode::Plan, Mode::Plan, true, false, false, false ; "old_target")]
#[test_case(Mode::Plan, Mode::Plan, false, true, false, false ; "child")]
#[test_case(Mode::Plan, Mode::Plan, false, false, true, false ; "failed")]
fn native_plan_completion_uses_committed_authority(
    selected: Mode,
    execution: Mode,
    stale: bool,
    child: bool,
    error: bool,
    expected: bool,
) {
    for remote in [false, true] {
        let mut app = test_app();
        app.state.mode = selected;
        app.state.plan = if remote {
            PlanState::RemoteDrafting(PlanRef::new(COMMITTED_PLAN_REF).unwrap())
        } else {
            PlanState::Drafting(COMMITTED_PLAN_PATH.into())
        };
        app.execution_mode = Some(Arc::new(ArcSwap::from_pointee(
            app.agent_mode_for(execution),
        )));
        app.status = Status::Streaming;
        app.run_id = 1;
        let target = if remote {
            PlanTarget::Remote(
                PlanRef::new(if stale {
                    STALE_PLAN_REF
                } else {
                    COMMITTED_PLAN_REF
                })
                .unwrap(),
            )
        } else {
            PlanTarget::Local(
                if stale {
                    STALE_PLAN_PATH
                } else {
                    COMMITTED_PLAN_PATH
                }
                .into(),
            )
        };
        let written = PlanWriteResult::new(target, HANDOFF_PLAN_CONTENT.into());
        let mut done = ToolDoneEvent::error(COMMITTED_PLAN_CALL.into(), HANDOFF_PLAN_CONTENT);
        done.tool = plan::NAME.into();
        done.is_error = error;
        done.output = ToolOutput::Markdown(HANDOFF_PLAN_CONTENT.into());
        done.annotation = Some(written.annotation().unwrap());
        let event = AgentEvent::ToolDone(Box::new(done));
        app.update(if child {
            subagent_msg(event, "child", None)
        } else {
            agent_msg(event)
        });
        assert_eq!(app.state.plan.is_ready(), expected);
        assert_eq!(app.plan_form.is_visible(), expected);
        assert_eq!(app.main_chat().last_message_is_plan(), expected);
        if expected {
            assert_eq!(app.main_chat().last_message_text(), HANDOFF_PLAN_CONTENT);
            let stored = &app.state.session.tool_outputs()[COMMITTED_PLAN_CALL];
            let restored: ToolOutput =
                serde_json::from_str(&serde_json::to_string(stored).unwrap()).unwrap();
            assert_eq!(restored.plan_write_result(), Some(written.clone()));
            let messages = app.main_chat().message_count();
            let mut duplicate =
                ToolDoneEvent::error(COMMITTED_PLAN_CALL.into(), HANDOFF_PLAN_CHANGED);
            duplicate.tool = plan::NAME.into();
            app.update(agent_msg(AgentEvent::ToolDone(Box::new(duplicate))));
            assert_eq!(app.main_chat().message_count(), messages);
            assert_eq!(
                app.state.session.tool_outputs()[COMMITTED_PLAN_CALL].plan_write_result(),
                Some(written)
            );
        }
        if child {
            assert!(!app.chats[1].last_message_is_plan());
        }
    }
}

#[test_case(Mode::Build, Mode::Plan, true ; "pending_build")]
#[test_case(Mode::Plan, Mode::Build, false ; "pending_plan")]
fn plan_handoff_uses_execution_not_selection(selected: Mode, execution: Mode, expected: bool) {
    let mut app = plan_app();
    app.state.mode = selected;
    app.execution_mode = Some(Arc::new(ArcSwap::from_pointee(
        app.agent_mode_for(execution),
    )));
    let plan = app.state.plan.clone();
    let actions = app.implement_plan(false);
    assert_eq!(!actions.is_empty(), expected);
    if !expected {
        assert_eq!(app.state.plan, plan);
        assert!(app.plan_form.is_visible());
    }
}

#[test_case(false, false, false, false, true; "local")]
#[test_case(true, false, false, false, true; "remote")]
#[test_case(true, true, false, false, false; "subagent")]
#[test_case(false, false, true, false, false; "error")]
#[test_case(false, false, false, true, false; "stale_target")]
fn batch_plan_completion_checks_authority(
    remote: bool,
    child: bool,
    error: bool,
    stale: bool,
    expected: bool,
) {
    let mut app = streaming_app();
    app.state.mode = Mode::Plan;
    app.state.applied_mode = Mode::Plan;
    app.state.plan = if remote {
        PlanState::RemoteDrafting(PlanRef::new(COMMITTED_PLAN_REF).unwrap())
    } else {
        PlanState::Drafting(COMMITTED_PLAN_PATH.into())
    };
    let target = if remote {
        PlanTarget::Remote(PlanRef::new(COMMITTED_PLAN_REF).unwrap())
    } else {
        PlanTarget::Local(
            if stale {
                STALE_PLAN_PATH
            } else {
                COMMITTED_PLAN_PATH
            }
            .into(),
        )
    };
    let written = PlanWriteResult::new(target, HANDOFF_PLAN_CONTENT.into());
    let event = || {
        AgentEvent::BatchProgress(Box::new(BatchProgressEvent {
            id: COMMITTED_PLAN_CALL.into(),
            index: 0,
            entry: BatchToolEntry {
                tool: plan::NAME.into(),
                effect: ToolEffect::Mutating,
                summary: String::new(),
                status: if error {
                    BatchToolStatus::Error
                } else {
                    BatchToolStatus::Success
                },
                input: None,
                raw_input: None,
                output: Some(ToolOutput::Markdown(TextOutput {
                    state: Some(written.annotation().unwrap().into()),
                    ..HANDOFF_PLAN_CONTENT.into()
                })),
                annotation: None,
                model_suffix: None,
            },
        }))
    };
    let wrap = |event| {
        if child {
            subagent_msg(event, "child", None)
        } else {
            agent_msg(event)
        }
    };
    app.update(wrap(batch_roster(COMMITTED_PLAN_CALL, 1)));
    app.update(wrap(event()));
    assert_eq!(app.state.plan.is_ready(), expected);
    assert_eq!(app.plan_form.is_visible(), expected);
    assert!(!app.main_chat().last_message_is_plan());
    app.state.plan.mark_drafting();
    app.plan_form.on_plan_drafting();
    app.update(wrap(event()));
    assert!(!app.state.plan.is_ready());
    assert!(!app.plan_form.is_visible());
}

fn rewrite_plan(app: &mut App) {
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t2".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 99 bytes to test-plan.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("test-plan.md".into()),
        written_paths: Vec::new(),
        remote_written_paths: false,
        output_ref: None,
        output_limits: None,
        model_suffix: None,
        model_output: None,
        model_output_from_ref: false,
        accounting: ToolAccounting::default(),
    }))));
}

fn dismiss_plan_esc(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Esc)));
}

#[test]
fn rewrite_does_not_reopen_after_dismiss() {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    dismiss_plan_esc(&mut app);
    assert!(!app.plan_form.is_visible());
    assert!(app.state.plan.is_ready());

    rewrite_plan(&mut app);
    assert!(!app.plan_form.is_visible());
    assert!(app.state.plan.is_ready());
}

#[test]
fn the_plan_chord_toggles_the_plan_form_in_plan_mode() {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    press_chord(&mut app, chord::PLAN_TOGGLE);
    assert!(!app.plan_form.is_visible());

    press_chord(&mut app, chord::PLAN_TOGGLE);
    assert!(app.plan_form.is_visible());
}

#[test]
fn the_plan_chord_is_a_noop_when_no_plan_is_ready() {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("test-plan.md"));
    assert!(!app.plan_form.is_visible());

    press_chord(&mut app, chord::PLAN_TOGGLE);
    assert!(!app.plan_form.is_visible());
}

fn install_override(
    app: &mut App,
    key: KeyCode,
    modifiers: KeyModifiers,
) -> caudra_lua::test_support::RequestProbe {
    install_override_at(app, key, modifiers, false)
}

fn install_override_at(
    app: &mut App,
    key: KeyCode,
    modifiers: KeyModifiers,
    leader: bool,
) -> caudra_lua::test_support::RequestProbe {
    app.keymap_reader =
        caudra_lua::test_support::keymap_reader_with(vec![caudra_lua::KeymapEntry {
            key,
            modifiers,
            leader,
            desc: "plugin override".into(),
            plugin: Arc::from("test-plugin"),
            id: 1,
        }]);
    let (handle, probe) = caudra_lua::test_support::probed_event_handle();
    app.lua_event_handle = handle;
    probe
}

const OVERRIDE_DISPATCHED: &str = "override callback must be dispatched";
const OVERRIDE_NOT_DISPATCHED: &str = "override callback must not be dispatched";
const FORM_TRAPPED: &str = "Esc must close the plan form whatever Lua bound the chord to";
const PLAN_STOLE_THINKING: &str = "Ctrl+T cycles the reasoning ladder, it no longer moves panels";

#[test]
fn a_leader_override_answers_the_chord_and_shadows_the_builtin() {
    let mut app = test_app();
    let probe = install_override_at(
        &mut app,
        chord::MODEL_PICKER.code,
        chord::MODEL_PICKER.modifiers,
        true,
    );

    let actions = press_chord(&mut app, chord::MODEL_PICKER);

    assert!(actions.is_empty());
    assert!(probe.try_recv().is_some(), "{OVERRIDE_DISPATCHED}");
    assert!(!app.model_picker.is_open(), "{OVERRIDE_DISPATCHED}");
}

#[test]
fn a_leader_override_leaves_the_bare_key_alone() {
    let mut app = test_app();
    let probe = install_override_at(&mut app, KeyCode::Char('m'), KeyModifiers::NONE, true);

    app.update(Msg::Key(key(KeyCode::Char('m'))));

    assert!(probe.try_recv().is_none(), "{OVERRIDE_NOT_DISPATCHED}");
    assert_eq!(app.input_box.buffer.value(), "m");
}

#[test]
fn override_shadows_builtin_ctrl_when_no_overlay_open() {
    let mut app = test_app();
    let probe = install_override(&mut app, kb::HELP.code, kb::HELP.modifiers);

    let actions = app.update(Msg::Key(kb::HELP.to_key_event()));

    assert!(actions.is_empty());
    assert!(probe.try_recv().is_some(), "{OVERRIDE_DISPATCHED}");
    assert!(
        !app.help_modal.is_open(),
        "override must consume the key before the built-in HELP handler runs"
    );
}

#[test]
fn override_shadows_quit_builtin() {
    let mut app = test_app();
    app.status = Status::Idle;
    let probe = install_override(&mut app, kb::QUIT.code, kb::QUIT.modifiers);

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));

    assert!(actions.is_empty());
    assert!(probe.try_recv().is_some(), "{OVERRIDE_DISPATCHED}");
    assert_eq!(
        app.exit_request,
        ExitRequest::None,
        "override must consume Ctrl+C before the built-in quit handler runs"
    );
}

#[test]
fn override_shadows_double_ctrl_d_exit() {
    let mut app = test_app();
    let probe = install_override(&mut app, kb::EXIT.code, kb::EXIT.modifiers);

    let actions = app.update(Msg::Key(kb::EXIT.to_key_event()));

    assert!(actions.is_empty());
    assert!(probe.try_recv().is_some(), "{OVERRIDE_DISPATCHED}");
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.last_exit.is_none());
}

#[test]
fn override_shadows_tab_mode_toggle() {
    let mut app = test_app();
    let initial_mode = app.state.mode;
    let probe = install_override(&mut app, KeyCode::Tab, KeyModifiers::NONE);

    let actions = app.update(Msg::Key(key(KeyCode::Tab)));

    assert!(actions.is_empty());
    assert!(probe.try_recv().is_some(), "{OVERRIDE_DISPATCHED}");
    assert_eq!(
        app.state.mode, initial_mode,
        "override must consume Tab before the built-in mode toggle runs"
    );
}

#[test]
fn override_shadows_esc_builtin() {
    let mut app = test_app();
    let probe = install_override(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(actions.is_empty());
    assert!(probe.try_recv().is_some(), "{OVERRIDE_DISPATCHED}");
    assert!(
        app.last_esc.is_none(),
        "override must consume Esc before the built-in esc handler runs"
    );
}

#[test]
fn builtin_runs_when_no_override() {
    let mut app = test_app();

    app.update(Msg::Key(kb::HELP.to_key_event()));

    assert!(app.help_modal.is_open());
}

/// The plan toggle is a leader chord now, and leader chords are overridable
/// like every other one. It used to be the form's own close key, which put it
/// ahead of an override. Carving out a single unoverridable chord would be
/// worse than the rule it breaks, and it is safe to drop because `Esc` closes
/// the form whatever Lua binds.
#[test]
fn an_override_takes_the_plan_chord_even_while_the_form_is_open() {
    let mut app = plan_app();
    let probe = install_override_at(
        &mut app,
        chord::PLAN_TOGGLE.code,
        chord::PLAN_TOGGLE.modifiers,
        true,
    );
    assert!(app.plan_form.is_visible());

    press_chord(&mut app, chord::PLAN_TOGGLE);

    assert!(probe.try_recv().is_some(), "{OVERRIDE_DISPATCHED}");
    assert!(app.plan_form.is_visible(), "{OVERRIDE_DISPATCHED}");
}

/// Which is only safe because a rebind cannot trap the form open.
#[test]
fn esc_still_closes_the_plan_form_under_an_override() {
    let mut app = plan_app();
    install_override_at(
        &mut app,
        chord::PLAN_TOGGLE.code,
        chord::PLAN_TOGGLE.modifiers,
        true,
    );

    app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(!app.plan_form.is_visible(), "{FORM_TRAPPED}");
}

#[test]
fn streaming_cancel_wins_over_quit_override() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let _receiver = attach_processing_queue(&mut app);
    let probe = install_override(&mut app, kb::QUIT.code, kb::QUIT.modifiers);

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));

    assert!(
        matches!(&actions[0], Action::CancelAgent { .. }),
        "built-in cancel must win while streaming even when Ctrl+C is overridden"
    );
    assert_eq!(app.status, Status::Streaming);
    assert_eq!(app.cancelling_run, Some(1));
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(probe.try_recv().is_none(), "{OVERRIDE_NOT_DISPATCHED}");
}

#[test]
fn dead_host_override_falls_back_to_builtin() {
    let mut app = test_app();
    let _probe = install_override(&mut app, kb::HELP.code, kb::HELP.modifiers);
    app.lua_event_handle = caudra_lua::EventHandle::disconnected_for_test();

    app.update(Msg::Key(kb::HELP.to_key_event()));

    assert!(
        app.help_modal.is_open(),
        "dead lua host must fall back to the built-in HELP handler"
    );
}

#[test]
fn streaming_cancel_wins_over_esc_override() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let _receiver = attach_processing_queue(&mut app);
    let probe = install_override(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(
        matches!(&actions[0], Action::CancelAgent { .. }),
        "built-in cancel must win while streaming even when Esc is overridden"
    );
    assert_eq!(app.status, Status::Streaming);
    assert_eq!(app.cancelling_run, Some(1));
    assert!(probe.try_recv().is_none(), "{OVERRIDE_NOT_DISPATCHED}");
}

#[test]
fn reset_session_closes_plan_form() {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    app.reset_session();
    assert!(!app.plan_form.is_visible());
}

#[test]
fn ctrl_c_closes_overlay_instead_of_quitting() {
    let mut app = test_app();
    app.help_modal.toggle();
    assert!(app.help_modal.is_open());

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(!app.help_modal.is_open());
    assert!(actions.is_empty());
}

#[test]
fn bash_prefix_overrides_mode() {
    let mut app = test_app();

    app.input_box.set_input("! ls".into());
    assert_eq!(&*app.mode_label().full, "[BASH]");

    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(
        app.state.mode,
        Mode::Plan,
        "tab must not toggle while bash prefix present"
    );

    app.input_box.set_input("ls".into());
    assert_eq!(&*app.mode_label().full, "[PLAN]");
}

/// A toggle only reaches the agent on the next message, so claiming the new
/// mode straight away would promise a switch that has not happened.
#[test]
fn a_toggled_mode_reads_as_pending_until_a_message_carries_it() {
    let mut app = test_app();
    assert_eq!(&*app.mode_label().full, "[PLAN]");

    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(&*app.mode_label().full, "[PLAN\u{2192}BUILD]");
    assert_eq!(&*app.mode_label().short, "[P\u{2192}B]");

    app.build_agent_input(&QueuedMessage {
        text: "go".into(),
        images: Vec::new(),
        mentions: Vec::new(),
        commits: Vec::new(),
        paste_ranges: Vec::new(),
    });
    assert_eq!(&*app.mode_label().full, "[PLAN\u{2192}BUILD]");
    assert_eq!(&*app.mode_label().short, "[P\u{2192}B]");
}

/// Toggling back is not a pending transition, it is no transition at all.
#[test]
fn toggling_back_before_sending_clears_the_pending_label() {
    let mut app = test_app();
    app.update(Msg::Key(key(KeyCode::Tab)));
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(&*app.mode_label().full, "[PLAN]");
}

/// The model the runtime would have installed, since the app only asks for one.
fn other_model() -> caudra_providers::Model {
    caudra_providers::Model {
        id: OTHER_MODEL_ID.into(),
        ..test_model()
    }
}

fn tab(app: &mut App) -> Vec<Action> {
    app.update(Msg::Key(key(KeyCode::Tab)))
}

fn asked_for(actions: &[Action]) -> Option<&str> {
    match actions {
        [Action::ChangeModel(spec)] => Some(spec),
        _ => None,
    }
}

fn restore_plan_binding(previous: Option<Binding>, storage: &StateDir) {
    match previous {
        Some(binding) => {
            model_registry::set_binding_and_persist(ModelPurpose::Plan, binding, storage)
        }
        None => model_registry::clear_binding_and_persist(ModelPurpose::Plan, storage),
    }
    .unwrap();
}

/// Plan and build each keep the model they were left on, so a toggle asks for
/// the other one rather than planning on whatever last wrote code.
#[test]
fn toggling_modes_asks_for_the_model_that_mode_remembers() {
    let mut app = test_app();
    caudra_storage::model::persist_model(&app.storage, StoredMode::Build, OTHER_MODEL_SPEC);

    let actions = tab(&mut app);

    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(
        asked_for(&actions),
        Some(OTHER_MODEL_SPEC),
        "{MODEL_UNASKED}"
    );
}

/// A mode that was never used on its own inherits the other's, which is the
/// same model already selected, so nothing is asked for.
#[test]
fn a_mode_with_no_remembered_model_asks_for_nothing() {
    let mut app = test_app();

    let actions = tab(&mut app);

    assert_eq!(asked_for(&actions), None, "{MODEL_CHURNED}");
}

#[test]
fn toggling_back_before_sending_asks_for_the_first_model_again() {
    let mut app = test_app();
    let plan_model = app.state.model.clone();
    caudra_storage::model::persist_model(&app.storage, StoredMode::Plan, &plan_model.spec());
    caudra_storage::model::persist_model(&app.storage, StoredMode::Build, OTHER_MODEL_SPEC);

    tab(&mut app);
    app.select_model(&other_model());
    let actions = tab(&mut app);

    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(
        asked_for(&actions),
        Some(plan_model.spec().as_str()),
        "{MODEL_UNASKED}"
    );
}

/// A bound Plan job already decides what a plan run uses, whatever the
/// selection says, so moving the selection would only make the bar name a model
/// the run ignores.
#[test_case(Mode::Plan ; "entering_build")]
#[test_case(Mode::Build ; "entering_plan")]
fn a_bound_plan_job_leaves_the_selection_alone(from: Mode) {
    let mut app = test_app();
    app.state.mode = from;
    app.state.applied_mode = from;
    caudra_storage::model::persist_model(&app.storage, StoredMode::Build, OTHER_MODEL_SPEC);
    caudra_storage::model::persist_model(&app.storage, StoredMode::Plan, PLAN_CONTEXT_SPEC);
    let previous = model_registry::binding(ModelPurpose::Plan);
    model_registry::set_binding_and_persist(
        ModelPurpose::Plan,
        Binding::Exact(PLAN_CONTEXT_SPEC.into()),
        &app.storage,
    )
    .unwrap();

    let actions = tab(&mut app);

    restore_plan_binding(previous, &app.storage);
    assert_eq!(asked_for(&actions), None, "{BINDING_OVERRIDDEN}");
}

/// The remembered model outlives the policy that allowed it, so a toggle must
/// not flash a rejection on every press.
#[test]
fn a_disallowed_remembered_model_is_not_asked_for() {
    let mut app = test_app();
    caudra_storage::model::persist_model(&app.storage, StoredMode::Build, OTHER_MODEL_SPEC);
    let raw: caudra_config::RawConfig = serde_json::from_value(serde_json::json!({
        "provider": {"allowed_models": [app.state.model.spec()]}
    }))
    .unwrap();
    app.model_policy = Arc::new(raw.into_config(false).unwrap().provider.model_policy);

    let actions = tab(&mut app);

    assert_eq!(asked_for(&actions), None, "{POLICY_IGNORED}");
}

#[test]
fn preparing_a_message_keeps_mode_and_model_pending() {
    let mut app = test_app();
    let other = other_model();
    tab(&mut app);
    app.select_model(&other);
    assert_ne!(app.state.applied_model, other.spec(), "{BASELINE_EARLY}");

    app.build_agent_input(&QueuedMessage {
        text: "go".into(),
        images: Vec::new(),
        mentions: Vec::new(),
        commits: Vec::new(),
        paste_ranges: Vec::new(),
    });

    assert_eq!(app.state.applied_mode, Mode::Plan);
    assert_ne!(
        app.state.applied_model,
        other.spec(),
        "{BASELINE_UNSETTLED}"
    );
}

/// A pick made with nothing pending is the model the next turn runs on, so it
/// becomes what a later toggle is measured against. One made mid switch must
/// not move that baseline, or the transition loses the model it started from.
#[test_case(false, OTHER_MODEL_SPEC ; "nothing_pending_moves_the_baseline")]
#[test_case(true, TEST_MODEL_SPEC   ; "a_pending_switch_keeps_it")]
fn a_pick_moves_the_baseline_only_when_nothing_is_pending(pending: bool, expected: &str) {
    let mut app = test_app();
    if pending {
        tab(&mut app);
    }

    app.select_model(&other_model());

    assert_eq!(app.state.applied_model, expected, "{BASELINE_WRONG}");
}

/// While a switch is pending the mode on screen is the one the pick is for, so
/// that is the mode it is remembered against.
#[test]
fn a_pick_is_remembered_against_the_mode_on_screen() {
    let mut app = test_app();
    tab(&mut app);

    app.select_model(&other_model());

    assert_eq!(
        caudra_storage::model::read_model(&app.storage, StoredMode::Build).as_deref(),
        Some(OTHER_MODEL_SPEC),
        "{PICK_MISFILED}"
    );
}

/// A session reconciled to a model the shared slot moved to has not chosen it,
/// so nothing may be recorded: a background session in the other mode would
/// otherwise overwrite that mode's choice.
#[test]
fn reconciling_to_the_shared_slot_records_nothing() {
    let mut app = test_app();

    app.update_model(&other_model());

    assert_eq!(
        caudra_storage::model::read_model(&app.storage, StoredMode::Plan),
        None,
        "{PICK_MISFILED}"
    );
}

#[test]
fn thinking_toggle_cycles_off_adaptive() {
    let mut app = test_app();
    assert_eq!(app.state.thinking, ThinkingConfig::Off);

    app.execute_command(cmd("/thinking"), 0);
    assert_eq!(app.state.thinking, ThinkingConfig::Adaptive);

    app.execute_command(cmd("/thinking"), 0);
    assert_eq!(app.state.thinking, ThinkingConfig::Off);
}

#[test]
fn shift_tab_cycles_explicit_reasoning_efforts() {
    let mut app = test_app();
    let shift_tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);

    for effort in app.state.model.reasoning_options().effort_ladder() {
        assert!(app.update(Msg::Key(shift_tab)).is_empty());
        assert_eq!(app.state.thinking, ThinkingConfig::Effort(effort.into()));
        let expected = format!("Reasoning effort: {effort}");
        assert_eq!(app.status_bar.flash_text(), Some(expected.as_str()));
    }
    app.update(Msg::Key(shift_tab));
    assert_eq!(app.state.thinking, ThinkingConfig::Off);
}

/// The level has to reach disk on the way through, or the next session opens
/// on whatever the last one happened to leave in its own meta.
#[test]
fn a_chosen_reasoning_effort_reaches_the_next_session() {
    let (_tmp, dir, _writer, mut app) = tempdir_app();
    let spec = app.state.model.spec();
    assert_eq!(
        caudra_storage::thinking::read(&dir, &spec),
        None,
        "{LEVEL_UNSET}"
    );

    app.set_thinking("high").unwrap();
    assert_eq!(
        caudra_storage::thinking::read(&dir, &spec),
        Some(StoredThinking::Effort {
            level: "high".into()
        }),
        "{LEVEL_UNSAVED}"
    );

    app.update(Msg::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)));
    assert_eq!(
        caudra_storage::thinking::read(&dir, &spec),
        Some(app.state.thinking.clone().into()),
        "{LEVEL_UNSAVED}"
    );
}

/// A level is only meaningful against the ladder that declared it, so switching
/// models must restore what was chosen there rather than carrying one model's
/// depth onto another.
#[test]
fn switching_models_restores_the_level_chosen_for_that_model() {
    let (_tmp, dir, _writer, mut app) = tempdir_app();
    let first = app.state.model.clone();
    let second = Model {
        id: "other-model".into(),
        ..first.clone()
    };
    caudra_storage::thinking::persist(&dir, &second.spec(), &StoredThinking::Off);

    app.set_thinking("high").unwrap();
    app.update_model(&second);
    assert_eq!(app.state.thinking, ThinkingConfig::Off, "{LEVEL_LEAKED}");

    app.update_model(&first);
    assert_eq!(
        app.state.thinking,
        ThinkingConfig::Effort("high".into()),
        "{LEVEL_LOST_ON_RETURN}"
    );
}

#[test_case(None, None, SETUP_MEDIUM_EFFORT ; "first_setup_uses_medium")]
#[test_case(Some(SETUP_HIGH_EFFORT), None, SETUP_HIGH_EFFORT ; "remembered_effort_wins")]
#[test_case(Some(SETUP_OFF), None, SETUP_OFF ; "remembered_off_wins")]
#[test_case(None, Some(SETUP_OFF), SETUP_OFF ; "explicit_session_or_config_off_wins")]
#[test_case(Some(SETUP_HIGH_EFFORT), Some(SETUP_OFF), SETUP_OFF ; "explicit_override_beats_remembered")]
fn openai_setup_selects_model_and_restores_thinking(
    remembered: Option<&str>,
    explicit: Option<&str>,
    expected: &str,
) {
    let (_tmp, dir, _writer, mut app) = tempdir_app();
    let model = Model::from_spec(OPENAI_SETUP_MODEL).unwrap();
    if let Some(remembered) = remembered {
        caudra_storage::thinking::persist(
            &dir,
            OPENAI_SETUP_MODEL,
            &StoredThinking::parse_setting(remembered).unwrap(),
        );
    }
    if let Some(explicit) = explicit {
        app.state.session_mut().meta.thinking =
            Some(StoredThinking::parse_setting(explicit).unwrap());
        app.state = SessionState::from_session(
            app.state.session.as_ref().clone(),
            &app.state.model,
            &dir,
            &ModelPolicy::default(),
        );
    }
    app.checkpoint();

    let actions = app.handle_login_picker_action(LoginPickerAction::Authenticated {
        model_spec: OPENAI_SETUP_MODEL.into(),
    });
    assert!(matches!(
        actions.as_slice(),
        [Action::CompleteProviderSetup(spec), Action::RefreshModels] if spec == OPENAI_SETUP_MODEL
    ));
    app.select_setup_model(&model);

    let expected: ThinkingConfig = StoredThinking::parse_setting(expected).unwrap().into();
    assert_eq!(app.state.model.spec(), OPENAI_SETUP_MODEL);
    assert_eq!(app.state.thinking, expected);
    app.checkpoint();
    let restored = SessionState::from_session(
        app.state.session.as_ref().clone(),
        &model,
        &dir,
        &ModelPolicy::default(),
    );
    assert_eq!(restored.thinking, expected);
    assert_eq!(
        caudra_storage::thinking::read(&dir, OPENAI_SETUP_MODEL),
        Some(StoredThinking::parse_setting(remembered.unwrap_or(SETUP_MEDIUM_EFFORT)).unwrap()),
    );
}

#[test_case(SETUP_OFF ; "off")]
#[test_case(SETUP_HIGH_EFFORT ; "high")]
fn openai_setup_preserves_live_explicit_thinking(effort: &str) {
    let (_tmp, _dir, _writer, mut app) = tempdir_app();
    let model = Model::from_spec(OPENAI_SETUP_MODEL).unwrap();
    app.set_thinking(effort).unwrap();
    app.checkpoint();

    app.select_setup_model(&model);

    assert_eq!(
        app.state.thinking,
        StoredThinking::parse_setting(effort).unwrap().into(),
    );
}

#[test_case(false ; "cancelled_login")]
#[test_case(true ; "oauth_not_yet_successful")]
fn unfinished_openai_setup_does_not_seed_thinking(oauth: bool) {
    let (_tmp, dir, _writer, mut app) = tempdir_app();
    let action = if oauth {
        LoginPickerAction::AuthenticateProvider {
            provider: SubscriptionProvider::OpenAi,
            model_spec: OPENAI_SETUP_MODEL.into(),
        }
    } else {
        LoginPickerAction::Close
    };

    let actions = app.handle_login_picker_action(action);

    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::CompleteProviderSetup(_)))
    );
    assert_eq!(
        caudra_storage::thinking::read(&dir, OPENAI_SETUP_MODEL),
        None
    );
}

#[test]
fn backtab_representation_cycles_reasoning_effort() {
    let mut app = test_app();

    app.update(Msg::Key(KeyEvent::new(
        KeyCode::BackTab,
        KeyModifiers::SHIFT,
    )));

    assert_eq!(app.state.thinking, ThinkingConfig::Effort("minimal".into()));
}

/// `Ctrl+T` is the mnemonic spelling, and the key opencode spends on the same
/// operation. `Shift+Tab` stays because that is where the muscle memory is.
#[test_case(kb::THINKING.to_key_event() ; "ctrl_t")]
#[test_case(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT) ; "shift_tab")]
fn both_thinking_keys_cycle_the_reasoning_effort(pressed: KeyEvent) {
    let mut app = test_app();

    app.update(Msg::Key(pressed));

    assert_eq!(app.state.thinking, ThinkingConfig::Effort("minimal".into()));
}

/// The preview is the live feedback for the shortcut, so it has to draw without
/// ever standing between the next keystroke and the cycle.
#[test]
fn cycling_flashes_the_picker_without_capturing_keys() {
    let mut app = test_app();

    app.update(Msg::Key(kb::THINKING.to_key_event()));
    assert!(!app.thinking_picker.is_open(), "{FLASH_STOLE_KEYS}");
    assert_eq!(app.thinking_picker.selected_label(), Some(THINKING_MINIMAL));
    // The render is the point, and the one step no state assertion reaches:
    // drawing the preview off `is_open` would leave every other assertion here
    // passing against a blank screen.
    assert!(
        rendered(&mut app).contains(THINKING_TITLE),
        "{FLASH_NOT_DRAWN}"
    );

    app.update(Msg::Key(kb::THINKING.to_key_event()));

    assert_eq!(app.state.thinking, ThinkingConfig::Effort("low".into()));
    assert!(
        rendered(&mut app).contains(THINKING_TITLE),
        "{FLASH_NOT_DRAWN}"
    );
}

/// A preview that counted as a modal would blank the status-bar hover and
/// swallow every click behind it for a second.
#[test]
fn a_flash_does_not_count_as_a_modal_overlay() {
    let mut app = test_app();

    app.update(Msg::Key(kb::THINKING.to_key_event()));

    assert!(app.thinking_picker.is_visible(), "{FLASH_NOT_DRAWN}");
    assert!(!app.any_overlay_open(), "{FLASH_STOLE_KEYS}");
    assert!(!app.has_modal_overlay(), "{FLASH_STOLE_KEYS}");
}

#[test]
fn any_other_key_dismisses_the_flash() {
    let mut app = test_app();
    app.update(Msg::Key(kb::THINKING.to_key_event()));
    assert!(app.thinking_picker.is_visible(), "{FLASH_NOT_DRAWN}");

    app.update(Msg::Key(key(KeyCode::Char('x'))));

    assert!(!app.thinking_picker.is_visible(), "{FLASH_STOLE_KEYS}");
}

/// `Ctrl+T` used to toggle the plan panel, which answers to the chord now.
#[test]
fn the_thinking_key_no_longer_toggles_the_plan_form() {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    app.update(Msg::Key(kb::THINKING.to_key_event()));

    assert!(app.plan_form.is_visible(), "{PLAN_STOLE_THINKING}");
}

#[test]
fn required_reasoning_wraps_from_max_to_minimal() {
    let mut app = test_app();
    app.state.model.thinking_override = Some(caudra_providers::ThinkingSupport::Required);
    app.state.thinking = ThinkingConfig::Effort("max".into());

    app.update(Msg::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)));

    assert_eq!(app.state.thinking, ThinkingConfig::Effort("minimal".into()));
}

#[test]
fn shift_tab_rejects_model_without_reasoning() {
    let mut app = test_app();
    app.state.model.thinking_override = Some(caudra_providers::ThinkingSupport::No);

    app.update(Msg::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)));

    assert_eq!(app.state.thinking, ThinkingConfig::Off);
    assert_eq!(app.status_bar.flash_text(), Some(THINKING_UNSUPPORTED_MSG));
}

#[test]
fn thinking_explicit_args() {
    let mut app = test_app();

    app.execute_command(
        ParsedCommand {
            name: "/thinking".into(),
            args: "8192".into(),
        },
        0,
    );
    assert_eq!(app.state.thinking, ThinkingConfig::Budget(8192));

    app.execute_command(
        ParsedCommand {
            name: "/thinking".into(),
            args: "high".into(),
        },
        0,
    );
    assert_eq!(app.state.thinking, ThinkingConfig::Effort("high".into()));
}

#[test]
fn thinking_unsupported_model_flashes_error() {
    let mut app = test_app();
    app.state.model.thinking_override = Some(caudra_providers::ThinkingSupport::No);

    app.execute_command(cmd("/thinking"), 0);
    assert_eq!(app.state.thinking, ThinkingConfig::Off);
    assert_eq!(app.status_bar.flash_text(), Some(THINKING_UNSUPPORTED_MSG));
}

#[test]
fn thinking_restored_from_session_meta() {
    let tmp = TempDir::new().unwrap();
    let storage = StateDir::from_path(tmp.path().to_path_buf());
    let mut session = AppSession::new("test-model", "/tmp/test");
    session.meta.thinking = Some(StoredThinking::Budget { tokens: 4096 });

    let state = SessionState::from_session(
        session,
        &test_model(),
        &storage,
        &caudra_config::ModelPolicy::default(),
    );
    assert_eq!(state.thinking, ThinkingConfig::Budget(4096));
}

fn set_opus_model(app: &mut App) {
    app.state.model = caudra_providers::Model::from_spec(OPUS_SPEC).unwrap();
}

#[test]
fn fast_toggle_on_off_on_opus() {
    let mut app = test_app();
    set_opus_model(&mut app);
    assert!(!app.state.fast);

    app.execute_command(cmd("/fast"), 0);
    assert!(app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_ON_MSG));

    app.execute_command(cmd("/fast"), 0);
    assert!(!app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_OFF_MSG));
}

#[test]
fn clicking_the_fast_chip_turns_it_off() {
    let mut app = test_app();
    set_opus_model(&mut app);
    app.execute_command(cmd("/fast"), 0);
    let hit = status_hit(&mut app, StatusBarHitTarget::Fast);
    app.update(mouse_event(MouseEventKind::Moved, hit.area.x, hit.area.y));
    assert_eq!(app.status_hover, Some(StatusBarHitTarget::Fast));

    assert!(click_status(&mut app, StatusBarHitTarget::Fast).is_empty());

    assert!(!app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_OFF_MSG));
    assert_eq!(app.status_hover, None);
    let _ = rendered(&mut app);
    assert!(
        app.status_hits
            .iter()
            .all(|hit| hit.target != StatusBarHitTarget::Fast)
    );
}

/// Sessions spawned from Lua have synthetic ids that no ToolDone matches, so
/// SubagentHistory is what finishes their chat.
#[test]
fn subagent_history_finishes_lua_spawned_chat() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "sub".into() },
        "session-abc",
        Some("researcher"),
    ));
    assert_eq!(app.chats.len(), 2);
    assert!(!app.chats[1].is_finished());

    app.update(agent_msg_with_run_id(
        AgentEvent::SubagentHistory {
            task_id: "session-abc".into(),
            parent_tool_use_id: "lua-parent".into(),
            root_tool_use_id: "lua-parent".into(),
            name: "researcher".into(),
            model: SONNET_SPEC.into(),
            messages: vec![],
            spec: None,
        },
        1,
    ));
    assert!(app.chats[1].is_finished());
    assert_eq!(app.chats[1].last_message_text(), DONE_TEXT);
}

#[test]
fn subagent_history_without_prior_events_creates_persisted_chat() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg_with_run_id(
        AgentEvent::SubagentHistory {
            task_id: "failed-task".into(),
            parent_tool_use_id: "task-call".into(),
            root_tool_use_id: "task-call".into(),
            name: "failed researcher".into(),
            model: SONNET_SPEC.into(),
            messages: vec![],
            spec: Some(caudra_storage::sessions::StoredSubagentTaskSpec::default()),
        },
        1,
    ));

    assert_eq!(app.chats.len(), 2);
    assert!(app.chats[1].is_finished());
    assert!(app.state.session.subagent_messages()["failed-task"].is_empty());
    assert!(
        app.state
            .session
            .subagents()
            .iter()
            .any(|subagent| subagent.tool_use_id == "failed-task")
    );
}

#[test]
fn stale_cancelled_subagent_history_is_still_persisted() {
    let mut app = test_app();
    app.run_id = 2;
    for item in crate::history_items(&[Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "task-call".into(),
            name: "task".into(),
            input: serde_json::json!({}),
            thought_signature: None,
        }],
        ..Message::default()
    }]) {
        app.state.session_mut().push_message(item);
    }

    app.update(agent_msg_with_run_id(
        AgentEvent::SubagentHistory {
            task_id: "cancelled-task".into(),
            parent_tool_use_id: "task-call".into(),
            root_tool_use_id: "task-call".into(),
            name: "cancelled researcher".into(),
            model: SONNET_SPEC.into(),
            messages: vec![Message::user("partial transcript".into())],
            spec: Some(caudra_storage::sessions::StoredSubagentTaskSpec::default()),
        },
        1,
    ));

    assert_eq!(app.chats.len(), 1);
    assert!(!app.state.session.subagent_messages()["cancelled-task"].is_empty());
    assert!(
        app.state
            .session
            .subagents()
            .iter()
            .any(|subagent| subagent.tool_use_id == "cancelled-task")
    );
}

#[test]
fn stale_subagent_history_from_inactive_branch_is_ignored() {
    let mut app = test_app();
    app.run_id = 2;

    app.update(agent_msg_with_run_id(
        AgentEvent::SubagentHistory {
            task_id: "abandoned-task".into(),
            parent_tool_use_id: "abandoned-call".into(),
            root_tool_use_id: "abandoned-call".into(),
            name: "abandoned researcher".into(),
            model: SONNET_SPEC.into(),
            messages: vec![Message::user("future transcript".into())],
            spec: Some(caudra_storage::sessions::StoredSubagentTaskSpec::default()),
        },
        1,
    ));

    assert!(
        !app.state
            .session
            .subagent_messages()
            .contains_key("abandoned-task")
    );
}

#[test]
fn stale_batched_subagent_history_uses_active_root_call() {
    let mut app = test_app();
    app.run_id = 2;
    for item in crate::history_items(&[Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "batch-call".into(),
            name: "batch".into(),
            input: serde_json::json!({}),
            thought_signature: None,
        }],
        ..Message::default()
    }]) {
        app.state.session_mut().push_message(item);
    }

    app.update(agent_msg_with_run_id(
        AgentEvent::SubagentHistory {
            task_id: "batched-task".into(),
            parent_tool_use_id: "batch-child-call".into(),
            root_tool_use_id: "batch-call".into(),
            name: "batched researcher".into(),
            model: SONNET_SPEC.into(),
            messages: vec![Message::user("partial transcript".into())],
            spec: Some(caudra_storage::sessions::StoredSubagentTaskSpec::default()),
        },
        1,
    ));

    assert!(
        app.state
            .session
            .subagent_messages()
            .contains_key("batched-task")
    );
    assert!(
        app.state
            .session
            .subagent_messages()
            .contains_key("batch-child-call")
    );
}

#[test_case(SONNET_SPEC ; "anthropic_model_without_a_fast_tier")]
#[test_case("openai/gpt-5.5" ; "openai_model_without_a_fast_tier")]
fn fast_flashes_error_on_ineligible_model(spec: &str) {
    let mut app = test_app();
    app.state.model = caudra_providers::Model::from_spec(spec).unwrap();

    app.execute_command(cmd("/fast"), 0);
    assert!(!app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_UNSUPPORTED_MSG));
}

#[test]
fn fast_restored_from_session_meta() {
    let tmp = TempDir::new().unwrap();
    let storage = StateDir::from_path(tmp.path().to_path_buf());
    let mut session = AppSession::new("anthropic/claude-opus-4-8", "/tmp/test");
    session.meta.fast = true;

    let state = SessionState::from_session(
        session,
        &test_model(),
        &storage,
        &caudra_config::ModelPolicy::default(),
    );
    assert!(state.fast);
}

#[test]
fn fast_normalized_off_when_restored_onto_ineligible_model() {
    let tmp = TempDir::new().unwrap();
    let storage = StateDir::from_path(tmp.path().to_path_buf());
    // Saved as fast=true, but sonnet cannot do fast mode, so restoring must drop
    // it to false or the UI would show a phantom [fast] badge.
    let mut session = AppSession::new(SONNET_SPEC, "/tmp/test");
    session.meta.fast = true;

    let state = SessionState::from_session(
        session,
        &test_model(),
        &storage,
        &caudra_config::ModelPolicy::default(),
    );
    assert!(!state.fast);
}

#[test]
fn model_state_reports_the_model_and_what_it_supports() {
    let mut app = test_app();
    app.state.model = caudra_providers::Model::from_spec(PLAIN_MODEL_SPEC).unwrap();
    assert_eq!(
        app.model_state(),
        serde_json::json!({
            "spec": PLAIN_MODEL_SPEC,
            "id": "qwen3",
            "provider": "ollama",
            "thinking": "off",
            "fast": false,
            "supports_thinking": false,
            "supports_fast": false,
        })
    );

    set_opus_model(&mut app);
    app.set_thinking("high").unwrap();
    app.set_fast(true).unwrap();
    assert_eq!(
        app.model_state(),
        serde_json::json!({
            "spec": OPUS_SPEC,
            "id": "claude-opus-4-8",
            "provider": "anthropic",
            "thinking": "high",
            "fast": true,
            "supports_thinking": true,
            "supports_fast": true,
        })
    );
}

/// What `model_state` reports has to parse back into the same state, or a
/// `caudra.model.get` -> `caudra.model.set` hop would silently change it.
#[test_case(ThinkingConfig::Off, "off" ; "off")]
#[test_case(ThinkingConfig::Adaptive, "adaptive" ; "adaptive")]
#[test_case(ThinkingConfig::Effort("high".into()), "high" ; "effort")]
#[test_case(ThinkingConfig::Budget(8192), "8192" ; "budget")]
fn model_state_thinking_round_trips_into_set_thinking(thinking: ThinkingConfig, expected: &str) {
    let mut app = test_app();
    app.state.thinking = thinking.clone();

    let reported = app.model_state()["thinking"].as_str().unwrap().to_owned();
    assert_eq!(reported, expected);
    assert_eq!(app.set_thinking(&reported).unwrap(), thinking);
    assert_eq!(app.set_thinking(&reported).unwrap(), thinking);
}

#[test]
fn set_thinking_toggles_on_blank_input() {
    let mut app = test_app();
    assert_eq!(app.set_thinking("").unwrap(), ThinkingConfig::Adaptive);
    assert_eq!(app.set_thinking("").unwrap(), ThinkingConfig::Off);
}

#[test_case(true, "garbage", THINKING_USAGE ; "unknown_word")]
#[test_case(true, "0", THINKING_USAGE ; "zero_budget")]
#[test_case(false, "low", THINKING_UNSUPPORTED_MSG ; "model_without_thinking")]
fn set_thinking_keeps_state_on_rejected_input(supported: bool, input: &str, expected: &str) {
    let mut app = test_app();
    app.set_thinking("high").unwrap();
    if !supported {
        app.state.model.thinking_override = Some(caudra_providers::ThinkingSupport::No);
    }

    assert_eq!(app.set_thinking(input).unwrap_err(), expected);
    assert_eq!(app.state.thinking, ThinkingConfig::Effort("high".into()));
}

/// Fast must never get stuck on: after switching to a model without fast mode,
/// you still have to be able to turn it off.
#[test]
fn fast_turns_off_on_a_model_that_lost_fast_support() {
    let mut app = test_app();
    set_opus_model(&mut app);
    app.execute_command(cmd("/fast"), 0);
    assert!(app.state.fast);

    app.state.model = caudra_providers::Model::from_spec(SONNET_SPEC).unwrap();
    app.execute_command(cmd("/fast"), 0);
    assert!(!app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_OFF_MSG));
}

#[test]
fn update_model_to_ineligible_resets_fast() {
    let mut app = test_app();
    set_opus_model(&mut app);
    app.state.fast = true;

    let sonnet = caudra_providers::Model::from_spec(SONNET_SPEC).unwrap();
    app.state.update_model(&sonnet);
    assert!(!app.state.fast);
}

#[test]
fn agent_error_creates_synthetic_tool_done_with_message() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "t1".into(),
        effect: ToolEffect::Unknown,
        tool: "bash".into(),
        summary: "echo hello".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));
    assert_eq!(app.main_chat().in_progress_count(), 1);

    let error_msg = "Provider is overloaded";
    app.update(agent_msg(AgentEvent::Error {
        message: error_msg.into(),
    }));

    assert_eq!(app.main_chat().in_progress_count(), 0);
    let text = app.main_chat().last_message_text();
    assert!(
        text.contains(error_msg),
        "tool output should contain error: {text}"
    );
}

fn dispatch_reported_key(app: &mut App, mut key: KeyEvent, kind: KeyEventKind) -> Vec<Action> {
    key.kind = kind;
    app.update(Msg::Key(key))
}

fn open_permission_test_editor(app: &mut App) {
    let id = app.input_box.buffer.insert_paste(PERMISSION_EDITOR_DRAFT);
    app.open_paste_editor(id);
}

fn open_permission_test_leader(app: &mut App) {
    app.which_key = crate::components::which_key::WhichKey::new(Duration::ZERO);
    app.which_key.arm();
}

#[test_case(open_help_modal; "help")]
#[test_case(open_usage_modal; "usage")]
#[test_case(open_logs_modal; "logs")]
#[test_case(open_docs_modal; "docs")]
#[test_case(open_context_modal; "context")]
#[test_case(open_tools_modal; "tools")]
#[test_case(open_decisions_modal; "decisions")]
#[test_case(open_skills_modal; "skills")]
#[test_case(open_storage_modal; "storage")]
#[test_case(open_system_prompt_modal; "system_prompt")]
#[test_case(open_projection_modal; "projection")]
#[test_case(open_goal_modal; "goal")]
#[test_case(open_model_picker; "model_picker")]
#[test_case(open_command_modal; "command_modal")]
#[test_case(open_relocation_picker; "relocation_picker")]
#[test_case(open_permission_test_editor; "paste_editor")]
#[test_case(open_permission_test_leader; "leader")]
#[test_case(|app| app.mcp_picker.open(); "mcp_picker")]
#[test_case(|app| { app.execute_command(cmd("/permissions"), 0); app.finish_permission_jobs(); }; "permissions_picker")]
fn permission_ownership_survives_overlays_before_and_after_arrival(open: fn(&mut App)) {
    for workbench in [false, true] {
        for overlay_first in [false, true] {
            let mut app = if workbench {
                open_workbench()
            } else {
                test_app()
            };
            if overlay_first {
                open(&mut app);
                rendered(&mut app);
            }
            app.status = Status::Streaming;
            app.run_id = 1;
            app.update(agent_msg(permission_event(
                REPORTED_PERMISSION_FIRST,
                REPORTED_PERMISSION_COMMAND,
            )));
            if !overlay_first {
                open(&mut app);
            }
            assert!(app.update(Msg::Key(key(KeyCode::Char('y')))).is_empty());
            assert_eq!(
                app.permission_prompt.request_id(),
                Some(REPORTED_PERMISSION_FIRST)
            );
            let screen = rendered(&mut app);
            for visible in [
                PERMISSION_TITLE,
                REPORTED_PERMISSION_COMMAND,
                PERMISSION_ALLOW_HINT,
            ] {
                assert!(screen.contains(visible), "{visible}: {screen}");
            }
            app.update(Msg::Key(key(KeyCode::Tab)));
            rendered(&mut app);
            assert!(app.update(Msg::Key(key(KeyCode::Char('y')))).is_empty());
            assert!(!app.permission_prompt.is_open());
            assert_eq!(app.exit_request, ExitRequest::None);
        }
    }
}

#[test_case(open_help_modal; "help")]
#[test_case(open_logs_modal; "logs")]
#[test_case(open_docs_modal; "docs")]
#[test_case(open_permission_test_editor; "paste_editor")]
#[test_case(|app| { app.run_builtin(BuiltinAction::Workbench); }; "workbench")]
fn visible_permission_buttons_own_the_pointer_over_other_overlays(open: fn(&mut App)) {
    let mut app = test_app();
    open(&mut app);
    app.permission_prompt.open(
        REPORTED_PERMISSION_FIRST.into(),
        ToolKey::native("bash"),
        vec![REPORTED_PERMISSION_COMMAND.into()],
        None,
    );
    let (row, column) = screen_hit(&mut app, PERMISSION_ALLOW_HINT);
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        assert!(app.update(mouse_event(kind, column, row)).is_empty());
    }
    assert!(!app.permission_prompt.is_open());
}

#[test_case(false; "paste_editor")]
#[test_case(true; "workbench")]
fn permission_guidance_owns_paste_instead_of_the_hidden_editor(workbench: bool) {
    let mut app = test_app();
    open_permission_test_editor(&mut app);
    if workbench {
        app.run_builtin(BuiltinAction::Workbench);
    }
    app.permission_prompt.open(
        REPORTED_PERMISSION_FIRST.into(),
        ToolKey::native("bash"),
        vec![REPORTED_PERMISSION_COMMAND.into()],
        None,
    );
    app.update(Msg::Key(key(KeyCode::Char(PERMISSION_GUIDANCE_KEY))));
    app.update(Msg::Paste(REPORTED_PERMISSION_COMMAND.into()));
    assert!(rendered(&mut app).contains(&format!(
        "{PERMISSION_GUIDANCE_PROMPT}{REPORTED_PERMISSION_COMMAND}"
    )));
    assert_eq!(
        app.input_box.expanded_text(),
        format!("{PERMISSION_EDITOR_DRAFT} ")
    );
    assert!(app.paste_editor.is_open());
}

#[test_case(KeyCode::Char('y'); "queued_approval_requires_release_and_fresh_press")]
fn reported_permission_keys_preserve_physical_release(approval: KeyCode) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    for id in [REPORTED_PERMISSION_FIRST, REPORTED_PERMISSION_SECOND] {
        app.update(agent_msg(permission_event(id, REPORTED_PERMISSION_COMMAND)));
    }
    rendered(&mut app);
    assert!(dispatch_reported_key(&mut app, key(approval), KeyEventKind::Press).is_empty());
    assert_eq!(
        app.permission_prompt.request_id(),
        Some(REPORTED_PERMISSION_SECOND)
    );
    rendered(&mut app);
    assert!(dispatch_reported_key(&mut app, key(approval), KeyEventKind::Repeat).is_empty());
    assert_eq!(
        app.permission_prompt.request_id(),
        Some(REPORTED_PERMISSION_SECOND)
    );
    assert!(dispatch_reported_key(&mut app, key(approval), KeyEventKind::Release).is_empty());
    assert_eq!(
        app.permission_prompt.request_id(),
        Some(REPORTED_PERMISSION_SECOND)
    );
    assert!(dispatch_reported_key(&mut app, key(approval), KeyEventKind::Press).is_empty());
    assert!(!app.permission_prompt.is_open());
}

#[test_case('y', KeyEventKind::Press; "legacy_once")]
#[test_case('a', KeyEventKind::Press; "legacy_project")]
#[test_case('y', KeyEventKind::Repeat; "reported_once")]
#[test_case('a', KeyEventKind::Repeat; "reported_project")]
fn a_key_held_before_the_first_permission_needs_rearming(shortcut: char, kind: KeyEventKind) {
    for release in [false, true] {
        let mut app = streaming_app();
        app.run_id = 1;
        let held = key(KeyCode::Char(shortcut));
        assert!(dispatch_reported_key(&mut app, held, kind).is_empty());
        assert_eq!(app.input_box.buffer.value(), shortcut.to_string());
        let mut request = Box::new(PermissionRequest::from_legacy(
            REPORTED_PERMISSION_FIRST.into(),
            ToolKey::native("bash"),
            vec![REPORTED_PERMISSION_COMMAND.into()],
            serde_json::json!({"command": REPORTED_PERMISSION_COMMAND}),
            Path::new("/project"),
            false,
        ));
        request.presentation.project = Some("/project".into());
        app.update(agent_msg(AgentEvent::PermissionRequest(request)));
        assert!(rendered(&mut app).contains(PERMISSION_TITLE));
        assert!(dispatch_reported_key(&mut app, held, KeyEventKind::Press).is_empty());
        assert_eq!(
            app.permission_prompt.request_id(),
            Some(REPORTED_PERMISSION_FIRST)
        );
        if release {
            dispatch_reported_key(&mut app, held, KeyEventKind::Release);
        } else {
            app.update(Msg::Key(key(KeyCode::Tab)));
        }
        rendered(&mut app);
        assert!(dispatch_reported_key(&mut app, held, KeyEventKind::Press).is_empty());
        assert!(!app.permission_prompt.is_open());
        assert_eq!(app.input_box.buffer.value(), shortcut.to_string());
    }
}

#[test_case(KeyCode::Char('y'); "approval")]
#[test_case(KeyCode::Esc; "denial")]
fn physical_release_between_requests_rearms_the_next_prompt(code: KeyCode) {
    let mut app = test_app();
    app.permission_prompt.open(
        REPORTED_PERMISSION_FIRST.into(),
        ToolKey::native("bash"),
        vec![REPORTED_PERMISSION_COMMAND.into()],
        None,
    );
    rendered(&mut app);
    dispatch_reported_key(&mut app, key(code), KeyEventKind::Press);
    assert!(!app.permission_prompt.is_open());
    dispatch_reported_key(&mut app, key(code), KeyEventKind::Release);
    app.permission_prompt.open(
        REPORTED_PERMISSION_SECOND.into(),
        ToolKey::native("bash"),
        vec![REPORTED_PERMISSION_COMMAND.into()],
        None,
    );
    rendered(&mut app);
    dispatch_reported_key(&mut app, key(code), KeyEventKind::Press);
    assert!(!app.permission_prompt.is_open());
}

#[test_case(KeyEventKind::Repeat; "repeat_cannot_quit_or_run_a_leader_action")]
#[test_case(KeyEventKind::Release; "release_cannot_quit_or_run_a_leader_action")]
fn reported_permission_key_kinds_never_activate_global_shortcuts(kind: KeyEventKind) {
    let mut app = test_app();
    app.permission_prompt.open(
        REPORTED_PERMISSION_FIRST.into(),
        ToolKey::native("bash"),
        vec![REPORTED_PERMISSION_COMMAND.into()],
        None,
    );
    app.which_key.arm();
    for key in [kb::QUIT.to_key_event(), chord::HELP.to_key_event()] {
        assert!(dispatch_reported_key(&mut app, key, kind).is_empty());
        assert!(app.permission_prompt.is_open());
        assert!(app.which_key.is_armed());
        assert_eq!(app.exit_request, ExitRequest::None);
    }
}

#[test_case(KeyEventKind::Press; "legacy_editing_and_navigation")]
#[test_case(KeyEventKind::Repeat; "reported_autorepeat_editing_and_navigation")]
fn reported_key_routing_preserves_composer_editing(kind: KeyEventKind) {
    let mut app = test_app();
    app.update(Msg::Paste("ab".into()));
    dispatch_reported_key(&mut app, key(KeyCode::Left), kind);
    dispatch_reported_key(&mut app, key(KeyCode::Char('X')), kind);
    assert_eq!(app.input_box.buffer.value(), "aXb");
    dispatch_reported_key(&mut app, key(KeyCode::Backspace), kind);
    assert_eq!(app.input_box.buffer.value(), "ab");
    dispatch_reported_key(&mut app, key(KeyCode::Backspace), KeyEventKind::Release);
    assert_eq!(app.input_box.buffer.value(), "ab");
}

/// A held word delete has to keep deleting. A terminal reporting event types
/// sends every repeat after the first as `Repeat`, so a guard that only knows
/// `Press` leaves the chord good for exactly one word.
#[test_case(KeyEventKind::Press; "legacy_autorepeat")]
#[test_case(KeyEventKind::Repeat; "reported_autorepeat")]
fn a_held_word_delete_keeps_deleting_words(kind: KeyEventKind) {
    let mut app = test_app();
    app.update(Msg::Paste(REPEAT_WORDS.into()));
    dispatch_reported_key(&mut app, kb::DELETE_WORD.to_key_event(), kind);
    assert_eq!(app.input_box.buffer.value(), REPEAT_WORDS_ONE_LEFT);
    dispatch_reported_key(&mut app, kb::DELETE_WORD.to_key_event(), kind);
    assert!(app.input_box.buffer.value().is_empty());
}

#[test]
fn a_held_word_motion_keeps_crossing_words() {
    let mut app = test_app();
    app.update(Msg::Paste(REPEAT_WORDS.into()));
    let word_left = KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL);
    for _ in 0..2 {
        dispatch_reported_key(&mut app, word_left, KeyEventKind::Repeat);
    }
    dispatch_reported_key(&mut app, key(KeyCode::Char('X')), KeyEventKind::Repeat);
    assert_eq!(app.input_box.buffer.value(), REPEAT_WORDS_CARET_HOME);
}

/// A held selection chord keeps extending instead of stopping after the first
/// word: the field keymap reads Shift as extending, and a motion repeats.
#[test]
fn a_held_word_selection_keeps_extending() {
    let mut app = test_app();
    app.update(Msg::Paste(REPEAT_WORDS.into()));
    let extend_left = KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
    for _ in 0..2 {
        dispatch_reported_key(&mut app, extend_left, KeyEventKind::Repeat);
    }
    assert_eq!(app.input_box.selected_text().as_deref(), Some(REPEAT_WORDS));
}

#[test_case(KeyEventKind::Press; "press")]
#[test_case(KeyEventKind::Repeat; "repeat")]
fn reported_key_routing_preserves_paste_editor_editing(kind: KeyEventKind) {
    let mut app = test_app();
    open_permission_test_editor(&mut app);
    dispatch_reported_key(&mut app, key(KeyCode::End), kind);
    dispatch_reported_key(&mut app, key(KeyCode::Char('X')), kind);
    assert!(rendered(&mut app).contains(&format!("{PERMISSION_EDITOR_DRAFT}X")));
    dispatch_reported_key(&mut app, key(KeyCode::Backspace), kind);
    assert!(rendered(&mut app).contains(PERMISSION_EDITOR_DRAFT));
    assert!(app.paste_editor.is_open());
}

fn open_repeat_test_session_picker(app: &mut App) {
    app.session_picker.open(
        vec![SessionRow {
            id: app.state.session.id,
            title: REPEAT_FIELD_TEXT.into(),
            updated_at: 0,
            activity: None,
            focused: true,
            checkout: None,
        }],
        0,
    );
}

#[test_case(KeyEventKind::Repeat; "repeat")]
#[test_case(KeyEventKind::Release; "release")]
fn session_title_generation_requires_a_fresh_press(kind: KeyEventKind) {
    let mut app = test_app();
    open_repeat_test_session_picker(&mut app);
    assert!(dispatch_reported_key(&mut app, kb::GENERATE_TITLE.to_key_event(), kind).is_empty());
    assert!(app.session_picker.is_open());
    assert!(dispatch_reported_key(&mut app, kb::GENERATE_TITLE.to_key_event(), KeyEventKind::Press)
        .iter()
        .any(|action| matches!(action, Action::GenerateSessionTitle(id) if *id == app.state.session.id)));
}

#[test_case(KeyEventKind::Press; "press")]
#[test_case(KeyEventKind::Repeat; "repeat")]
fn search_field_preserves_text_and_backspace_repeats(kind: KeyEventKind) {
    let mut app = test_app();
    open_search(&mut app);
    app.update(Msg::Paste(REPEAT_FIELD_TEXT.into()));
    dispatch_reported_key(&mut app, key(KeyCode::Char('X')), kind);
    let extended = format!("{REPEAT_FIELD_TEXT}X");
    assert!(rendered(&mut app).contains(&extended));
    dispatch_reported_key(&mut app, key(KeyCode::Backspace), kind);
    let screen = rendered(&mut app);
    assert!(screen.contains(REPEAT_FIELD_TEXT));
    assert!(!screen.contains(&extended));
    assert!(dispatch_reported_key(&mut app, key(KeyCode::Enter), KeyEventKind::Repeat).is_empty());
    assert!(app.search_modal.is_open());
    assert!(app.input_box.is_empty());
}

#[test_case(KeyEventKind::Press; "press")]
#[test_case(KeyEventKind::Repeat; "repeat")]
fn logs_filter_preserves_edit_repeats_without_repeating_log_actions(kind: KeyEventKind) {
    let mut app = test_app();
    app.logs_modal.open();
    assert!(
        dispatch_reported_key(&mut app, key(KeyCode::Char('/')), KeyEventKind::Repeat).is_empty()
    );
    assert!(!app.logs_modal.text_input_active());
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.logs_modal.text_input_active());
    app.update(Msg::Paste(REPEAT_FIELD_TEXT.into()));
    dispatch_reported_key(&mut app, key(KeyCode::Char('q')), kind);
    let extended = format!("{REPEAT_FIELD_TEXT}q");
    assert!(rendered(&mut app).contains(&extended));
    dispatch_reported_key(&mut app, key(KeyCode::Backspace), kind);
    let screen = rendered(&mut app);
    assert!(screen.contains(REPEAT_FIELD_TEXT));
    assert!(!screen.contains(&extended));
    for event in [
        key(KeyCode::Enter),
        key(KeyCode::Esc),
        kb::QUIT.to_key_event(),
    ] {
        assert!(dispatch_reported_key(&mut app, event, KeyEventKind::Repeat).is_empty());
        assert!(app.logs_modal.text_input_active());
    }
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.logs_modal.text_input_active());
    for code in [KeyCode::Char('q'), KeyCode::Char('/')] {
        assert!(dispatch_reported_key(&mut app, key(code), KeyEventKind::Repeat).is_empty());
        assert!(app.logs_modal.is_open());
        assert!(!app.logs_modal.text_input_active());
    }
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert!(!app.logs_modal.is_open());
    assert!(app.input_box.is_empty());
}

#[test_case(KeyEventKind::Press; "press")]
#[test_case(KeyEventKind::Repeat; "repeat")]
fn docs_query_preserves_edit_repeats_without_repeating_docs_actions(kind: KeyEventKind) {
    let mut app = test_app();
    open_docs_modal(&mut app);
    assert!(
        dispatch_reported_key(&mut app, key(KeyCode::Char('/')), KeyEventKind::Repeat).is_empty()
    );
    assert!(!app.docs_modal.text_input_active());
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.docs_modal.text_input_active());
    app.update(Msg::Paste(REPEAT_FIELD_TEXT.into()));
    dispatch_reported_key(&mut app, key(KeyCode::Char('q')), kind);
    let extended = format!("{REPEAT_FIELD_TEXT}q");
    assert!(rendered(&mut app).contains(&extended));
    dispatch_reported_key(&mut app, key(KeyCode::Backspace), kind);
    let screen = rendered(&mut app);
    assert!(screen.contains(REPEAT_FIELD_TEXT));
    assert!(!screen.contains(&extended));
    for event in [
        key(KeyCode::Enter),
        key(KeyCode::Esc),
        kb::QUIT.to_key_event(),
    ] {
        assert!(dispatch_reported_key(&mut app, event, KeyEventKind::Repeat).is_empty());
        assert!(app.docs_modal.text_input_active());
    }
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.docs_modal.text_input_active());
    for code in [KeyCode::Char('q'), KeyCode::Char('/')] {
        assert!(dispatch_reported_key(&mut app, key(code), KeyEventKind::Repeat).is_empty());
        assert!(app.docs_modal.is_open());
        assert!(!app.docs_modal.text_input_active());
    }
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert!(!app.docs_modal.is_open());
    assert!(app.input_box.is_empty());
}

#[test_case(KeyEventKind::Press; "press")]
#[test_case(KeyEventKind::Repeat; "repeat")]
fn custom_question_answer_preserves_edit_repeats_without_repeating_decisions(kind: KeyEventKind) {
    let mut app = streaming_app();
    let (answer_tx, answers) = flume::unbounded();
    app.answer_tx = Some(answer_tx);
    open_question(&mut app);
    app.update(Msg::Key(key(KeyCode::Down)));
    assert!(dispatch_reported_key(&mut app, key(KeyCode::Enter), KeyEventKind::Repeat).is_empty());
    assert!(!app.question_form.text_input_active());
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.question_form.text_input_active());
    app.update(Msg::Paste(REPEAT_FIELD_TEXT.into()));
    dispatch_reported_key(&mut app, key(KeyCode::Char('X')), kind);
    assert!(rendered(&mut app).contains(&format!("{REPEAT_FIELD_TEXT}X")));
    for _ in 0..2 {
        dispatch_reported_key(&mut app, key(KeyCode::Backspace), kind);
    }
    for event in [
        key(KeyCode::Enter),
        key(KeyCode::Esc),
        kb::QUIT.to_key_event(),
    ] {
        assert!(dispatch_reported_key(&mut app, event, KeyEventKind::Repeat).is_empty());
        assert!(app.question_form.text_input_active());
        assert_eq!(answers.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(app.status, Status::Streaming);
    }
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.question_form.is_open());
    assert!(!app.question_form.text_input_active());
    let submitted: Vec<Vec<String>> = serde_json::from_str(&answers.try_recv().unwrap()).unwrap();
    assert_eq!(submitted, vec![vec![REPEAT_FIELD_EDITED.to_owned()]]);
    assert!(app.input_box.is_empty());
}

#[test_case(KeyEventKind::Press; "press")]
#[test_case(KeyEventKind::Repeat; "repeat")]
fn session_rename_preserves_text_and_backspace_repeats(kind: KeyEventKind) {
    let mut app = test_app();
    open_repeat_test_session_picker(&mut app);
    app.update(Msg::Key(kb::RENAME_SESSION.to_key_event()));
    dispatch_reported_key(&mut app, key(KeyCode::Char('X')), kind);
    assert!(rendered(&mut app).contains(&format!("{REPEAT_FIELD_TEXT}X")));
    for _ in 0..2 {
        dispatch_reported_key(&mut app, key(KeyCode::Backspace), kind);
    }
    assert!(dispatch_reported_key(&mut app, key(KeyCode::Enter), KeyEventKind::Repeat).is_empty());
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(actions.iter().any(|action| matches!(action,
        Action::SetSessionTitle { id, title } if *id == app.state.session.id && title == REPEAT_FIELD_EDITED)));
    assert!(app.input_box.is_empty());
}

fn workbench_repeat_editor() -> (TempDir, PathBuf, App) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(REPEAT_EDITOR_FILE);
    fs::write(&path, REPEAT_FIELD_TEXT).unwrap();
    let mut app = test_app();
    app.workbench.open_at(dir.path(), &path, None);
    rendered(&mut app);
    (dir, path, app)
}

#[test_case(KeyEventKind::Press; "press")]
#[test_case(KeyEventKind::Repeat; "repeat")]
fn workbench_text_and_backspace_repeats_edit_without_repeating_save(kind: KeyEventKind) {
    let (_dir, path, mut app) = workbench_repeat_editor();
    assert!(app.workbench.text_input_active());
    dispatch_reported_key(&mut app, key(KeyCode::End), kind);
    dispatch_reported_key(&mut app, key(KeyCode::Char('X')), kind);
    assert!(rendered(&mut app).contains(&format!("{REPEAT_FIELD_TEXT}X")));
    for _ in 0..2 {
        dispatch_reported_key(&mut app, key(KeyCode::Backspace), kind);
    }
    let save = KeyEvent::new(workbench_keys::SAVE.code, workbench_keys::SAVE.modifiers);
    assert!(dispatch_reported_key(&mut app, save, KeyEventKind::Repeat).is_empty());
    assert_eq!(fs::read_to_string(&path).unwrap(), REPEAT_FIELD_TEXT);
    assert!(dispatch_reported_key(&mut app, save, KeyEventKind::Press).is_empty());
    assert_eq!(fs::read_to_string(&path).unwrap(), REPEAT_FIELD_EDITED);
    assert!(app.input_box.is_empty());
}

#[test_case(KeyCode::Char('d'); "discard")]
#[test_case(KeyCode::Char('s'); "save")]
#[test_case(KeyCode::Char('c'); "cancel")]
#[test_case(KeyCode::Enter; "selected_answer")]
fn workbench_confirmation_never_inherits_text_repeat_ownership(code: KeyCode) {
    let (_dir, path, mut app) = workbench_repeat_editor();
    app.update(Msg::Key(key(KeyCode::End)));
    app.update(Msg::Key(key(KeyCode::Char('X'))));
    app.update(Msg::Key(kb::LEADER.to_key_event()));
    app.update(Msg::Key(KeyEvent::new(
        workbench_keys::CLOSE_TAB.code,
        workbench_keys::CLOSE_TAB.modifiers,
    )));
    assert!(!app.workbench.text_input_active());
    assert!(dispatch_reported_key(&mut app, key(code), KeyEventKind::Repeat).is_empty());
    assert!(!app.workbench.text_input_active());
    assert_eq!(app.workbench.layout().tabs, vec![path.clone()]);
    assert_eq!(fs::read_to_string(&path).unwrap(), REPEAT_FIELD_TEXT);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    assert!(app.workbench.text_input_active());
}

#[test_case(KeyEventKind::Press; "press")]
#[test_case(KeyEventKind::Repeat; "repeat")]
fn review_note_preserves_edit_repeats_without_repeating_note_actions(kind: KeyEventKind) {
    let mut app = test_app();
    app.review.open(
        DisplaySource::AssistantText(CaudraId::generate()),
        ReviewTarget {
            lines: vec![Line::raw(REPEAT_FIELD_TEXT)],
            provenance: None,
            label: ASSISTANT_LABEL,
        },
    );
    rendered(&mut app);
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.review.text_input_active());
    app.update(Msg::Paste(REPEAT_FIELD_TEXT.into()));
    dispatch_reported_key(&mut app, key(KeyCode::Char('X')), kind);
    assert!(rendered(&mut app).contains(&format!("{REPEAT_FIELD_TEXT}X")));
    for _ in 0..2 {
        dispatch_reported_key(&mut app, key(KeyCode::Backspace), kind);
    }
    let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(dispatch_reported_key(&mut app, save, KeyEventKind::Repeat).is_empty());
    assert!(app.review.text_input_active());
    app.update(Msg::Key(save));
    assert!(!app.review.text_input_active());
    assert_eq!(app.review.notes_pending(), 1);
    assert!(
        dispatch_reported_key(&mut app, key(KeyCode::Char('d')), KeyEventKind::Repeat).is_empty()
    );
    assert_eq!(app.review.notes_pending(), 1);
    assert!(dispatch_reported_key(&mut app, save, KeyEventKind::Repeat).is_empty());
    assert!(app.review.is_open());
    app.update(Msg::Key(save));
    assert!(!app.review.is_open());
    assert!(
        app.input_box
            .expanded_text()
            .contains(&format!("\n{REPEAT_FIELD_EDITED}\n</note>"))
    );
}

#[test_case(KeyEventKind::Release; "denial_release_does_not_become_a_global_quit")]
#[test_case(KeyEventKind::Repeat; "held_denial_does_not_become_a_global_quit")]
fn reported_ctrl_c_cannot_exit_after_denying_a_prompt(kind: KeyEventKind) {
    let mut app = test_app();
    app.permission_prompt.open(
        REPORTED_PERMISSION_FIRST.into(),
        ToolKey::native("bash"),
        vec![REPORTED_PERMISSION_COMMAND.into()],
        None,
    );
    assert!(
        dispatch_reported_key(&mut app, kb::QUIT.to_key_event(), KeyEventKind::Press).is_empty()
    );
    assert!(!app.permission_prompt.is_open());
    assert!(dispatch_reported_key(&mut app, kb::QUIT.to_key_event(), kind).is_empty());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(
        dispatch_reported_key(&mut app, kb::QUIT.to_key_event(), KeyEventKind::Press)
            .iter()
            .any(|action| matches!(action, Action::ManualExit))
    );
}

#[test_case(KeyEventKind::Repeat; "repeat")]
#[test_case(KeyEventKind::Release; "release")]
fn reported_ctrl_c_cannot_cancel_a_run_after_denying_its_last_prompt(kind: KeyEventKind) {
    let mut app = streaming_app();
    app.permission_prompt.open(
        REPORTED_PERMISSION_FIRST.into(),
        ToolKey::native("bash"),
        vec![REPORTED_PERMISSION_COMMAND.into()],
        None,
    );
    assert!(
        dispatch_reported_key(&mut app, kb::QUIT.to_key_event(), KeyEventKind::Press).is_empty()
    );
    assert!(!app.permission_prompt.is_open());
    assert!(dispatch_reported_key(&mut app, kb::QUIT.to_key_event(), kind).is_empty());
    assert_eq!(app.status, Status::Streaming);
    assert!(app.cancelling_run.is_none());
    assert_eq!(app.exit_request, ExitRequest::None);
}

#[test_case(kb::LEADER.to_key_event(); "leader")]
#[test_case(kb::EXIT.to_key_event(); "exit")]
#[test_case(kb::QUIT.to_key_event(); "quit")]
#[test_case(kb::SEARCH.to_key_event(); "search")]
#[test_case(kb::FILE_PICKER.to_key_event(); "file_picker")]
#[test_case(kb::OPEN_EDITOR.to_key_event(); "open_editor")]
#[test_case(key(KeyCode::Enter); "submit")]
#[test_case(key(KeyCode::Esc); "escape")]
fn reported_repeats_do_not_activate_global_actions(key: KeyEvent) {
    let mut app = test_app();
    app.update(Msg::Paste(REPORTED_PERMISSION_COMMAND.into()));
    for _ in 0..2 {
        assert!(dispatch_reported_key(&mut app, key, KeyEventKind::Repeat).is_empty());
    }
    assert_eq!(app.input_box.buffer.value(), REPORTED_PERMISSION_COMMAND);
    assert!(!app.which_key.is_armed());
    assert!(!app.search_modal.is_open());
    assert!(!app.file_picker.is_open());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.last_exit.is_none());
    assert!(app.last_esc.is_none());
}

#[test_case(KeyCode::Enter; "enter")]
#[test_case(KeyCode::Char('y'); "yes")]
fn reported_repeats_cannot_accept_project_trust_confirmation(code: KeyCode) {
    let mut app = app_awaiting_permission_config_trust();
    app.open_awaiting_permission_config_trust(false);
    app.finish_permission_jobs();
    app.update(Msg::Key(key(KeyCode::Enter)));
    rendered(&mut app);
    assert!(dispatch_reported_key(&mut app, key(code), KeyEventKind::Repeat).is_empty());
    assert!(app.permissions.needs_project_permission_config_trust());
    assert!(app.permissions_picker.is_open());
}

#[test]
fn ctrl_c_denies_permission_prompt() {
    let mut app = test_app();
    app.permission_prompt.open(
        "id".into(),
        caudra_config::ToolKey::native("bash"),
        vec!["execute".into()],
        None,
    );
    assert!(app.permission_prompt.is_open());

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(!app.permission_prompt.is_open());
    assert!(actions.is_empty());
}

#[test]
fn subagent_permission_requests_remain_in_fifo_order() {
    let mut app = app_with_subagent_id("sub1");
    app.update(subagent_msg(
        permission_event("subagent-request", "cargo test"),
        "sub1",
        Some("research"),
    ));
    app.update(agent_msg(permission_event("main-request", "cargo check")));

    assert_eq!(app.permission_prompt.pending_count(), 2);
    assert_eq!(app.permission_prompt.request_id(), Some("subagent-request"));
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.permission_prompt.request_id(), Some("main-request"));
}

#[test]
fn covered_permission_event_removes_the_matching_queued_prompt() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.permission_prompt.open(
        "first".into(),
        caudra_config::ToolKey::native("bash"),
        vec!["cargo test".into()],
        None,
    );
    app.permission_prompt.open(
        "covered".into(),
        caudra_config::ToolKey::native("bash"),
        vec!["cargo test".into()],
        None,
    );

    app.update(agent_msg(AgentEvent::PermissionRequestResolved {
        request_id: "covered".into(),
        source_request_id: "first".into(),
    }));

    assert_eq!(app.permission_prompt.pending_count(), 1);
    assert_eq!(app.permission_prompt.request_id(), Some("first"));
}

const STAGED_CALL_ID: &str = "staged_call";
const STAGED_COMMAND: &str = "cargo test";
/// A call from an MCP server, which starts before it asks.
const STAGED_MCP_TOOL: &str = "mcp_server_lookup";
const APPROVAL_STAGED_MSG: &str =
    "the card whose call raised a request names the wait, in the chat that raised it";
const APPROVAL_SETTLED_MSG: &str = "an answered request hands the title back to the call";

/// Where `chat`'s card for `tool_id` says its call is.
fn card_stage(app: &App, chat: usize, tool_id: &str) -> Option<CallStage> {
    let chat = &app.chats[chat];
    (0..chat.message_count())
        .filter_map(|index| chat.message_at(index))
        .find(|msg| matches!(&msg.role, DisplayRole::Tool(tool) if tool.id == tool_id))
        .and_then(|msg| msg.tool_stage)
}

#[test]
fn a_subagent_request_stages_the_card_in_its_own_chat() {
    let mut app = app_with_subagent();
    for event in [
        AgentEvent::ToolPending {
            id: STAGED_CALL_ID.into(),
            name: "bash".into(),
        },
        permission_event(STAGED_CALL_ID, STAGED_COMMAND),
    ] {
        app.update(subagent_msg(event, TASK_ID, Some(RESEARCH_NAME)));
    }

    assert_eq!(
        card_stage(&app, 1, STAGED_CALL_ID),
        Some(CallStage::AwaitingApproval),
        "{APPROVAL_STAGED_MSG}"
    );
}

fn answer_with_a_key(app: &mut App) {
    rendered(app);
    app.update(Msg::Key(key(KeyCode::Char('y'))));
    app.finish_permission_jobs();
}

fn resolve_by_rule(app: &mut App) {
    app.update(agent_msg(AgentEvent::PermissionRequestResolved {
        request_id: STAGED_CALL_ID.into(),
        source_request_id: STAGED_CALL_ID.into(),
    }));
}

/// No start follows the answer for a call that started before it asked, so
/// the answer itself, however it arrives, is what ends the wait.
#[test_case(answer_with_a_key ; "the_reader_answers")]
#[test_case(resolve_by_rule ; "a_rule_answers_for_them")]
fn a_resolved_request_unstages_the_card(resolve: fn(&mut App)) {
    let mut app = streaming_app();
    app.update(agent_msg(tool_start(STAGED_CALL_ID, STAGED_MCP_TOOL)));
    app.update(agent_msg(permission_event(STAGED_CALL_ID, STAGED_COMMAND)));
    assert_eq!(
        card_stage(&app, 0, STAGED_CALL_ID),
        Some(CallStage::AwaitingApproval),
        "{APPROVAL_STAGED_MSG}"
    );

    resolve(&mut app);

    assert!(!app.permission_prompt.is_open(), "{APPROVAL_SETTLED_MSG}");
    assert_eq!(
        card_stage(&app, 0, STAGED_CALL_ID),
        None,
        "{APPROVAL_SETTLED_MSG}"
    );
}

#[test]
fn permission_decision_answers_manager_request_id_directly() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let manager = Arc::clone(&app.permissions);
    let (event_tx, event_rx) = flume::unbounded();
    let event_sender = caudra_agent::EventSender::new(event_tx, 1);
    let (legacy_tx, legacy_rx) = flume::unbounded();
    let (_cancel_trigger, cancel) = caudra_agent::CancelToken::new();
    let task = smol::spawn(async move {
        let legacy_rx = async_lock::Mutex::new(legacy_rx);
        manager
            .enforce(
                &ToolKey::native("bash"),
                &caudra_agent::tools::PermissionScopes::single("cargo test".into()),
                &serde_json::json!({"command": "cargo test"}),
                &event_sender,
                Some(&legacy_rx),
                "request-by-id",
                &cancel,
                None,
            )
            .await
    });
    let envelope = event_rx.recv_timeout(PERMISSION_TEST_TIMEOUT).unwrap();
    assert_eq!(app.permissions.pending_count(), 1);
    app.update(Msg::Agent(Box::new(envelope)));
    rendered(&mut app);

    app.update(Msg::Key(key(KeyCode::Char('y'))));
    app.finish_permission_jobs();

    assert!(smol::block_on(futures_lite::future::or(
        async { task.await.is_ok() },
        async {
            smol::Timer::after(PERMISSION_TEST_TIMEOUT).await;
            false
        },
    )));
    assert_eq!(app.permissions.pending_count(), 0);
    assert!(app.permission_prompt.request_id().is_none());
    drop(legacy_tx);
}

#[test_case(false; "current_request")]
#[test_case(true; "queued_request")]
fn permission_updates_refresh_existing_requests_without_enqueuing(queued: bool) {
    const REQUEST_ID: &str = "updated-request";
    const ORIGINAL: &str = "cargo test";
    const UPDATED: &str = "cargo check";
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    if queued {
        app.update(agent_msg(permission_event("first", "cargo build")));
    }
    app.update(agent_msg(permission_event(REQUEST_ID, ORIGINAL)));
    rendered(&mut app);
    let AgentEvent::PermissionRequest(request) = permission_event(REQUEST_ID, UPDATED) else {
        unreachable!()
    };
    app.update(agent_msg(AgentEvent::PermissionRequestUpdated(request)));
    assert_eq!(
        app.permission_prompt.pending_count(),
        1 + usize::from(queued)
    );
    if queued {
        app.update(Msg::Key(key(KeyCode::Esc)));
    }
    assert_eq!(app.permission_prompt.request_id(), Some(REQUEST_ID));
    app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert_eq!(app.permission_prompt.request_id(), Some(REQUEST_ID));
    let text = rendered(&mut app);
    assert!(text.contains(UPDATED));
    assert!(!text.contains(ORIGINAL));
}

const TEST_AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 80,
    height: 40,
};
const SPLIT_EXTENT: u16 = 8;

fn open_split_window(app: &mut App, dir: caudra_lua::Split) {
    let buf = Arc::new(caudra_agent::SharedBuf::new());
    let config = caudra_lua::FloatConfig {
        width: caudra_lua::Dimension::Abs(SPLIT_EXTENT),
        height: caudra_lua::Dimension::Abs(SPLIT_EXTENT),
        border: caudra_lua::Border::None,
        split: dir,
        ..caudra_lua::FloatConfig::default()
    };
    let (event_tx, _event_rx) = flume::bounded::<caudra_lua::WinEvent>(8);
    let (_cmd_tx, cmd_rx) = flume::bounded::<caudra_lua::WinCommand>(8);
    app.float_mgr.open(buf, config, true, event_tx, cmd_rx);
}

#[test]
fn attention_float_marks_app_as_awaiting_input_until_close() {
    let mut app = test_app();
    let buf = Arc::new(caudra_agent::SharedBuf::new());
    let config = caudra_lua::FloatConfig {
        needs_input: true,
        ..caudra_lua::FloatConfig::default()
    };
    let (event_tx, _event_rx) = flume::bounded::<caudra_lua::WinEvent>(8);
    let (cmd_tx, cmd_rx) = flume::bounded::<caudra_lua::WinCommand>(8);

    app.float_mgr.open(buf, config, true, event_tx, cmd_rx);
    assert!(app.awaiting_input());
    assert_eq!(app.attention(), Some(Notification::QuestionRequested));

    cmd_tx
        .send(caudra_lua::WinCommand::SetVisible(false))
        .unwrap();
    let _ = app.float_mgr.tick();
    assert!(!app.awaiting_input());
    assert_eq!(app.attention(), None);

    cmd_tx
        .send(caudra_lua::WinCommand::SetVisible(true))
        .unwrap();
    let _ = app.float_mgr.tick();
    assert_eq!(app.attention(), Some(Notification::QuestionRequested));

    cmd_tx.send(caudra_lua::WinCommand::Close).unwrap();
    let _ = app.float_mgr.tick();
    assert!(!app.awaiting_input());
    assert_eq!(app.attention(), None);
}

#[test]
fn below_split_reserves_bottom_and_suppresses_input() {
    let mut app = test_app();
    let (msg_before, _b, _s, input_before, splits_before) = app.layout_geometry(TEST_AREA);
    assert!(
        splits_before.rect(caudra_lua::Split::Below).is_none(),
        "no split open yet"
    );
    assert!(input_before.height > 0, "input box visible before split");

    open_split_window(&mut app, caudra_lua::Split::Below);
    let (msg_after, _bottom, _s, input_after, splits_after) = app.layout_geometry(TEST_AREA);

    let band = splits_after
        .rect(caudra_lua::Split::Below)
        .expect("below split should reserve a bottom band");
    assert_eq!(
        band.height, SPLIT_EXTENT,
        "below band reserves the requested rows",
    );
    assert!(
        msg_after.height < msg_before.height,
        "chat must shrink to make room for the below split",
    );
    assert_eq!(
        input_after.height, 0,
        "input box is suppressed under a below split"
    );
}

/// `carve` already tests the per-direction geometry; this pins the app wiring:
/// a split shrinks the chat while the status stays aligned to the main gutter.
/// Below is tested separately since it also hides the input box.
#[test_case(caudra_lua::Split::Above ; "above")]
#[test_case(caudra_lua::Split::Left ; "left")]
#[test_case(caudra_lua::Split::Right ; "right")]
fn non_below_split_reserves_band_and_keeps_status_aligned(dir: caudra_lua::Split) {
    let mut app = test_app();
    let (msg_before, _b, _s, _i, _sp) = app.layout_geometry(TEST_AREA);

    open_split_window(&mut app, dir);
    let (msg_after, _bottom, status_after, _input, splits) = app.layout_geometry(TEST_AREA);

    assert!(splits.rect(dir).is_some(), "split must reserve a band");
    assert!(
        msg_after.area() < msg_before.area(),
        "chat must shrink to make room for the split",
    );
    let footer_rows = crate::components::status_bar::height(TEST_AREA.height);
    assert_eq!(
        status_after,
        super::view::main_content_area(Rect::new(
            TEST_AREA.x,
            TEST_AREA.bottom() - footer_rows,
            TEST_AREA.width,
            footer_rows,
        )),
        "status bar stays aligned to the main gutter regardless of the split",
    );
}

/// Too short to spare a transcript row for a second footer row.
const SHORT_TERMINAL_ROWS: u16 = 12;
const FOOTER_EDGE_MSG: &str = "the footer must sit on the terminal's bottom edge";

/// Only the terminal's height picks the footer's rows, so nothing the footer
/// shows can move the composer above it.
#[test_case(TEST_AREA.height    => 2 ; "a_tall_terminal_splits_it")]
#[test_case(SHORT_TERMINAL_ROWS => 1 ; "a_short_terminal_keeps_one_row")]
fn the_footer_takes_two_rows_unless_the_terminal_is_short(rows: u16) -> u16 {
    let area = Rect {
        height: rows,
        ..TEST_AREA
    };
    let (_msg, _bottom, status, _input, _splits) = test_app().layout_geometry(area);

    assert_eq!(status.bottom(), area.bottom(), "{FOOTER_EDGE_MSG}");
    status.height
}

#[test]
fn closing_split_restores_layout() {
    let mut app = test_app();
    let before = app.layout_geometry(TEST_AREA);

    open_split_window(&mut app, caudra_lua::Split::Below);
    app.float_mgr.close_all();

    let after = app.layout_geometry(TEST_AREA);
    assert_eq!(after, before, "closing the split restores the layout");
}

#[test]
fn permission_prompt_takes_bottom_precedence_over_below_split() {
    let mut app = test_app();
    open_split_window(&mut app, caudra_lua::Split::Below);
    open_split_window(&mut app, caudra_lua::Split::Left);
    open_split_window(&mut app, caudra_lua::Split::Above);
    app.permission_prompt.open(
        "perm-1".into(),
        caudra_config::ToolKey::native("bash"),
        vec!["ls".into()],
        None,
    );

    let (_msg, _bottom, _status, _input, splits) = app.layout_geometry(TEST_AREA);
    assert!(
        splits.rect(caudra_lua::Split::Below).is_none(),
        "below split must yield the bottom area to an open permission prompt",
    );
    assert!(
        splits.rect(caudra_lua::Split::Left).is_some(),
        "the prompt must leave a left split untouched",
    );
    assert!(
        splits.rect(caudra_lua::Split::Above).is_some(),
        "the prompt must leave an above split untouched",
    );
}

fn app_with_active_subagent() -> App {
    let mut app = app_with_subagent();
    app.run_builtin(BuiltinAction::NextChat);
    assert_eq!(app.active_chat, 1);
    app
}

#[test]
fn double_esc_in_subagent_cancels_subagent() {
    let mut app = app_with_active_subagent();
    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::CancelSubagent { tool_use_id } if tool_use_id == TASK_ID
    ));
    assert!(app.chats[1].is_finished());
    assert_eq!(app.chats[1].last_message_text(), CANCELLED_TEXT);
}

#[test]
fn single_or_stale_esc_in_subagent_flashes() {
    let mut app = app_with_active_subagent();
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());
    assert_eq!(app.status_bar.flash_text().unwrap(), FLASH_CANCEL);

    app.last_esc = Some(Instant::now().checked_sub(Duration::from_secs(10)).unwrap());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());
    assert!(!app.chats[1].is_finished());
}

#[test_case(false, Duration::ZERO ; "read_only_without_ordinary_flash")]
#[test_case(false, LONG_FLASH_DURATION ; "read_only_with_long_ordinary_flash")]
#[test_case(true, Duration::ZERO ; "composer_without_ordinary_flash")]
#[test_case(true, LONG_FLASH_DURATION ; "composer_with_long_ordinary_flash")]
fn task_esc_confirmation_uses_fixed_window(steerable: bool, flash_duration: Duration) {
    for (elapsed, confirmed) in CONFIRMATION_TIMINGS {
        let mut app = if steerable {
            focused_task_composer().0
        } else {
            app_with_active_subagent()
        };
        assert_eq!(app.active_subagent_can_steer(), steerable);
        app.status_bar.flash_duration = flash_duration;

        assert!(app.update(Msg::Key(key(KeyCode::Esc))).is_empty());
        assert_eq!(app.status_bar.clear_expired_hint(), Dirty::NO);
        assert_eq!(app.status_bar.flash_text(), Some(FLASH_CANCEL));

        let pressed_at = Instant::now().checked_sub(elapsed).unwrap();
        app.last_esc = Some(pressed_at);
        let actions = app.update(Msg::Key(key(KeyCode::Esc)));

        assert_eq!(app.chats[1].is_finished(), confirmed);
        if confirmed {
            assert!(matches!(
                actions.as_slice(),
                [Action::CancelSubagent { tool_use_id }] if tool_use_id == TASK_ID
            ));
            assert!(app.last_esc.is_none());
        } else {
            assert!(actions.is_empty());
            assert!(
                app.last_esc
                    .is_some_and(|rearmed_at| rearmed_at > pressed_at)
            );
            assert_eq!(app.status_bar.flash_text(), Some(FLASH_CANCEL));
        }
    }
}

#[test]
fn esc_in_main_chat_with_active_subagent_no_cancel() {
    let mut app = app_with_subagent();
    assert_eq!(app.active_chat, 0);
    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::CancelAgent { .. }));
    assert!(!matches!(&actions[0], Action::CancelSubagent { .. }));
}

#[test]
fn cancel_subagent_removes_answer_sender() {
    let (mut app, _sub_rx, _main_rx) = app_with_subagent_tx(TASK_ID);
    assert!(!app.subagent_answers.is_empty());
    app.run_builtin(BuiltinAction::NextChat);
    assert_eq!(app.active_chat, 1);
    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.subagent_answers.contains_key(TASK_ID));
}

#[test]
fn multiple_subagents_cancel_one_other_unaffected() {
    let mut app = app_with_subagent_id(TASK_ID);
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "y".into() },
        "task2",
        Some("build"),
    ));
    assert_eq!(app.chats.len(), 3);

    app.active_chat = *app.chat_index.get("task2").unwrap();
    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));

    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::CancelSubagent { tool_use_id } if tool_use_id == "task2"
    ));
    let task1_idx = *app.chat_index.get(TASK_ID).unwrap();
    assert!(!app.chats[task1_idx].is_finished());
    assert!(app.chats[app.active_chat].is_finished());
}

#[test]
fn double_esc_in_finished_subagent_noop() {
    let mut app = app_with_active_subagent();
    finish_subagent_task(&mut app, false);
    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());
}

#[test]
fn subagent_cancel_then_navigate_back_main_unaffected() {
    let mut app = app_with_active_subagent();
    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.chats[1].is_finished());

    app.run_builtin(BuiltinAction::PrevChat);
    assert_eq!(app.active_chat, 0);
    assert_eq!(app.status, Status::Streaming);
    assert!(!app.chats[0].is_finished());
}

// -- Every frame checkpoints: one way in for a history, one trigger to save --

/// Long enough that a waiting change is still waiting when the assert runs, on
/// any machine, so none of these tests depend on the wall clock.
const SOFT_DELAY_HELD: Duration = Duration::from_secs(3600);
const MID_BATCH_RESULT: &str = "file contents";
const TYPED_DRAFT: &str = "hi";
const UNSENT_DRAFT: &str = "half typed thought";
const LIVE_AGENT_TEXT: &str = "live agent turn";
const STORED_SESSION_TEXT: &str = "other session talk";
const SWITCHED_DRAFT: &str = "draft typed after switching";
const BUMP_TITLE: &str = "title bump ";
const TOOL_IDS: [&str; 2] = ["tool-a", "tool-b"];
const FINISHED_TASK_ID: &str = "task-finished";
const UNFINISHED_TASK_ID: &str = "task-unfinished";

#[test]
fn turn_response_normalizes_text_and_truncates_unicode() {
    let long = "界".repeat(201);
    let message = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "  first\n\tsecond ".into(),
            },
            ContentBlock::thinking("ignored".into(), None),
            ContentBlock::Text { text: long },
        ],
        ..Default::default()
    };
    let response = turn_response(&message).unwrap();
    assert_eq!(response.chars().count(), 200);
    assert!(response.starts_with("first second 界"));
    assert_eq!(turn_response(&Message::default()), None);
    assert_eq!(turn_response(&tool_use_msg("tool")), None);
}

#[test]
fn turn_response_stops_after_bounded_large_input() {
    let message = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text {
                text: format!("first {}", "x".repeat(1_000_000)),
            },
            ContentBlock::Text {
                text: "not reached".into(),
            },
        ],
        ..Default::default()
    };

    let response = turn_response(&message).unwrap();

    assert_eq!(response.chars().count(), 200);
    assert!(response.starts_with("first "));
    assert!(!response.contains("not reached"));
}

#[test_case(Notification::TurnComplete { response: Some("answer".into()) }, "answer", false ; "turn_response")]
#[test_case(Notification::TurnComplete { response: None }, "Agent turn complete", false ; "turn_fallback")]
#[test_case(Notification::PermissionRequested { tool: Some("bash".into()) }, "Permission requested: bash", true ; "permission_tool")]
#[test_case(Notification::PermissionRequested { tool: None }, "Permission requested", true ; "permission_fallback")]
#[test_case(Notification::AuthenticationRequired, "Authentication required", true ; "authentication")]
#[test_case(Notification::QuestionRequested, "Question requested", true ; "question")]
#[test_case(Notification::PlanReady, "Plan ready", true ; "plan")]
#[test_case(Notification::error_completion(), "Agent stopped with an error", false ; "error_completion")]
fn notification_message_and_urgency(
    notification: Notification,
    expected_message: &str,
    urgent: bool,
) {
    assert_eq!(notification.message(), expected_message);
    assert_eq!(notification.is_urgent(), urgent);
}

#[test]
fn attention_prioritizes_permission_and_normalizes_tool() {
    let mut app = test_app();
    app.pending_input = PendingInput::AuthRetry {
        waiters: HashSet::from([None]),
    };
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Ready(PathBuf::from("plan.md"));
    app.plan_form.on_plan_ready();
    app.permission_prompt.open(
        "id".into(),
        caudra_config::ToolKey::native("bash"),
        vec!["execute".into()],
        None,
    );
    assert_eq!(
        app.attention(),
        Some(Notification::PermissionRequested {
            tool: Some("bash".into())
        })
    );

    app.permission_prompt.close();
    app.permission_prompt
        .open("id".into(), caudra_config::ToolKey::Wildcard, vec![], None);
    assert_eq!(
        app.attention(),
        Some(Notification::PermissionRequested { tool: None })
    );
}

#[test]
fn lifecycle_blocker_reads_like_the_notification() {
    let mut app = test_app();
    app.permission_prompt.open(
        "id".into(),
        caudra_config::ToolKey::native("bash"),
        vec!["execute".into()],
        None,
    );

    assert_eq!(
        app.lifecycle_blocker().as_deref(),
        Some(
            Notification::PermissionRequested {
                tool: Some("bash".into())
            }
            .message()
            .as_str()
        )
    );
}

#[test]
fn attention_classifies_auth_and_ready_plan() {
    let mut app = test_app();
    app.pending_input = PendingInput::AuthRetry {
        waiters: HashSet::from([None]),
    };
    assert_eq!(app.attention(), Some(Notification::AuthenticationRequired));

    app.pending_input = PendingInput::None;
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Ready(PathBuf::from("plan.md"));
    app.plan_form.on_plan_ready();
    app.status = Status::Streaming;
    assert_eq!(app.attention(), None);
    app.status = Status::Idle;
    assert_eq!(app.attention(), Some(Notification::PlanReady));
    assert!(!app.awaiting_input());

    app.plan_form.hide();
    assert_eq!(app.attention(), None);
    app.plan_form.on_plan_ready();
    app.state.plan = PlanState::Drafting(PathBuf::from("plan.md"));
    assert_eq!(app.attention(), None);
    app.state.plan = PlanState::Ready(PathBuf::from("plan.md"));
    app.state.mode = Mode::Build;
    assert_eq!(app.attention(), None);
}

fn tool_use_msg(id: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::tool_use(id, "read", serde_json::json!({}))],
        ..Default::default()
    }
}

fn tool_result_msg(id: &str, text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: text.into(),
            is_error: false,
            output_ref: None,
        }],
        display_text: Some(String::new()),
        ..Default::default()
    }
}

fn tool_text(id: &str) -> String {
    format!("output of {id}")
}

fn attach_live_history(app: &mut App, messages: Vec<Message>) -> caudra_agent::History {
    let mirror: caudra_agent::SharedHistory = Arc::new(ArcSwap::from_pointee(
        caudra_agent::HistorySnapshot::default(),
    ));
    let history = caudra_agent::History::new(messages).with_mirror(Arc::clone(&mirror));
    app.shared_history = Some(mirror);
    history
}

/// Types [`TYPED_DRAFT`] one key per frame and hands back the stamp of the
/// write the first key caused. The soft delay never elapses, so every key after
/// the first is still waiting when the caller looks.
fn type_draft_leaving_last_key_waiting(app: &mut App) -> Sent {
    let mut keys = TYPED_DRAFT.chars();
    app.update(Msg::Key(key(KeyCode::Char(keys.next().unwrap()))));
    app.checkpoint();
    let first = app
        .last_sent
        .clone()
        .expect("the first keystroke puts the session on disk");

    for c in keys {
        app.update(Msg::Key(key(KeyCode::Char(c))));
        app.checkpoint_with(SOFT_DELAY_HELD);
    }
    first
}

const MERGE_FIRST_MSG: &str = "the first checkpoint has nothing to compare against and must merge";
const MERGE_SKIP_MSG: &str = "an unchanged history must not walk the graph again";
const MERGE_PUSH_MSG: &str = "a message the producer appended must bring the merge back";
const MERGE_REINSTALL_MSG: &str = "installing a different history must bring the merge back";

/// The event loop checkpoints every frame, so the merge behind it has to cost
/// nothing while both sides hold still. It used to allocate a set of every
/// message id, deep-clone the message vector and deep-compare it ten times a
/// second, which is what made an idle session burn a quarter of a core.
#[test]
fn an_unchanged_history_skips_the_merge() {
    let (_tmp, _dir, _writer, mut app) = tempdir_app();
    let mut history = attach_live_history(&mut app, vec![Message::user("go".into())]);
    let snapshot = app.shared_history.as_ref().unwrap().load_full();
    assert!(app.history_moved(&snapshot), "{MERGE_FIRST_MSG}");

    app.checkpoint();
    let snapshot = app.shared_history.as_ref().unwrap().load_full();
    assert!(!app.history_moved(&snapshot), "{MERGE_SKIP_MSG}");

    history.push(tool_use_msg("t1"));
    let snapshot = app.shared_history.as_ref().unwrap().load_full();
    assert!(app.history_moved(&snapshot), "{MERGE_PUSH_MSG}");
}

const MERGE_MESSAGES_ONLY_MSG: &str =
    "only the session's own messages moving may bring the merge back";
const MERGE_USAGE_MODEL: &str = "usage-model";

/// Every tool call lands an output and every response a usage record, and
/// neither changes what the merge reads. Keyed on the content revision, each
/// one used to walk the whole transcript again.
#[test_case(|session| session.insert_tool_output("t1".into(), ToolOutput::Plain(tool_text("t1").into())), false ; "a_tool_output")]
#[test_case(|session| session.add_model_usage(MERGE_USAGE_MODEL, StoredTokenUsage::default()), false ; "a_usage_record")]
#[test_case(|session| session.truncate_messages(1), true ; "a_truncation")]
fn only_the_sessions_messages_bring_the_merge_back(mutate: fn(&mut AppSession), moves: bool) {
    let (_tmp, _dir, _writer, mut app) = tempdir_app();
    let _history = attach_live_history(
        &mut app,
        vec![
            Message::user("go".into()),
            tool_use_msg("t1"),
            tool_result_msg("t1", &tool_text("t1")),
        ],
    );
    app.checkpoint();

    mutate(app.state.session_mut());

    let snapshot = app.shared_history.as_ref().unwrap().load_full();
    assert_eq!(
        app.history_moved(&snapshot),
        moves,
        "{MERGE_MESSAGES_ONLY_MSG}"
    );
}

const APPENDED_REPLY: &str = "the appended reply";
const APPEND_FAST_PATH_MSG: &str = "a producer that only appended must take the fast path";
const APPEND_ORACLE_MSG: &str = "the fast path must store exactly what the full merge would";
const APPEND_BOUND_MSG: &str = "a row the fast path appended must still learn its source";

/// The fast path appends without walking the graph, so the full merge is its
/// oracle, and the rows it adds must still be bound to their items.
#[test]
fn an_appended_history_stores_what_the_full_merge_would() {
    let (_tmp, _dir, _writer, mut app) = tempdir_app();
    let mut history = attach_live_history(&mut app, vec![Message::user("go".into())]);
    app.main_chat().push_user_message("go");
    app.checkpoint();
    let before = app.state.session.messages().to_vec();

    history.push(tool_use_msg("t1"));
    history.push(tool_result_msg("t1", &tool_text("t1")));
    history.push(assistant_message(APPENDED_REPLY));
    app.main_chat().push(DisplayMessage::new(
        DisplayRole::Assistant,
        APPENDED_REPLY.into(),
    ));
    let snapshot = app.shared_history.as_ref().unwrap().load_full();
    assert_eq!(
        app.appended_since_merge(&snapshot),
        Some(before.len()),
        "{APPEND_FAST_PATH_MSG}"
    );

    app.checkpoint();

    let mut expected = before;
    merge_history_items(&mut expected, &snapshot.messages).unwrap();
    assert_eq!(
        app.state.session.messages(),
        expected.as_slice(),
        "{APPEND_ORACLE_MSG}"
    );
    let reply = snapshot.messages.last().unwrap().id;
    assert_eq!(
        app.main_chat().message_at(1).unwrap().source,
        Some(DisplaySource::AssistantText(reply)),
        "{APPEND_BOUND_MSG}"
    );
}

/// Pointer identity cannot notice a whole history being swapped underneath the
/// app, so every install has to drop the memo by hand.
#[test]
fn reinstalling_a_history_brings_the_merge_back() {
    let (_tmp, _dir, _writer, mut app) = tempdir_app();
    let _history = attach_live_history(&mut app, vec![Message::user("go".into())]);
    app.checkpoint();

    let _replacement = attach_live_history(&mut app, vec![Message::user("again".into())]);
    app.forget_merged_history();
    let snapshot = app.shared_history.as_ref().unwrap().load_full();
    assert!(app.history_moved(&snapshot), "{MERGE_REINSTALL_MSG}");
}

/// Checkpointing mid-batch used to freeze the tools as failed forever. The
/// synthetic closing message made the snapshot as long as the real results that
/// followed, so the append cursor never saw them.
#[test]
fn mid_batch_checkpoint_does_not_shadow_the_real_tool_results() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let mut history = attach_live_history(
        &mut app,
        vec![Message::user("go".into()), tool_use_msg("t1")],
    );
    app.checkpoint();

    history.push(tool_result_msg("t1", MID_BATCH_RESULT));
    app.checkpoint();

    let id = app.state.session.id;
    drain_writer(app, writer);

    let loaded = AppSession::load(id, &dir).unwrap();
    assert_eq!(loaded.messages().len(), 4);
    let Some(HistoryItemKind::ToolResult {
        content, is_error, ..
    }) = loaded.messages().iter().find_map(|item| match &item.kind {
        result @ HistoryItemKind::ToolResult { .. } => Some(result),
        _ => None,
    })
    else {
        panic!("expected one real tool result: {:?}", loaded.messages());
    };
    assert_eq!((content.as_str(), *is_error), (MID_BATCH_RESULT, false));
}

#[test]
fn restored_unfinished_batch_checkpoint_keeps_pending_unrevert_until_real_work() {
    let (_tmp, _dir, _writer, mut app) = tempdir_app();
    let items = crate::history_items(&[
        Message::user("go".into()),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::tool_use("t1", "read", serde_json::json!({})),
                ContentBlock::tool_use("t2", "read", serde_json::json!({})),
            ],
            ..Default::default()
        },
    ]);
    let first_call = items[1].id;
    let unfinished_head = items.last().unwrap().id;
    app.state.session_mut().replace_messages(items);
    let forked = app
        .fork_at(DisplaySource::ToolCall {
            id: first_call,
            result_id: None,
        })
        .unwrap();
    assert_eq!(forked.session.messages().len(), 3);
    assert!(caudra_agent::History::restored(forked.session.messages().to_vec()).is_ok());

    app.revert_to(first_call, RestoreMode::Conversation);
    assert!(app.state.session.meta.pending_revert.is_some());
    assert_eq!(
        crate::session_history_head(&app.state.session),
        Some(unfinished_head)
    );

    let mirror: caudra_agent::SharedHistory =
        Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()));
    let mut history =
        caudra_agent::History::restored(crate::active_session_history(&app.state.session).unwrap())
            .unwrap()
            .with_mirror(Arc::clone(&mirror));
    app.shared_history = Some(mirror);

    app.checkpoint();

    assert!(app.state.session.meta.pending_revert.is_some());
    assert_ne!(
        crate::session_history_head(&app.state.session),
        Some(unfinished_head)
    );

    history.push(Message::user("actual work".into()));
    app.checkpoint();
    assert!(app.state.session.meta.pending_revert.is_none());
}

/// A staged head move is durable without deleting the inactive tail.
#[test]
fn checkpoint_after_rewind_persists_head_and_inactive_tail() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let _live = attach_live_history(
        &mut app,
        vec![
            Message::user("first prompt".into()),
            Message::user("second prompt".into()),
        ],
    );
    app.checkpoint();

    let entry = RewindEntry {
        turn_index: 1,
        prompt_preview: "2: second".into(),
    };
    app.rewind_to(entry);
    assert!(app.shared_history.is_none(), "mirror handle is dropped");
    app.checkpoint();

    let id = app.state.session.id;
    drain_writer(app, writer);
    let loaded = AppSession::load(id, &dir).unwrap();
    assert_eq!(loaded.messages().len(), 2);
    assert_eq!(crate::active_session_history(&loaded).unwrap().len(), 1);
    assert!(loaded.meta.pending_revert.is_some());
}

#[test]
fn branched_checkpoint_and_restart_retain_the_abandoned_tail() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let _live = attach_live_history(
        &mut app,
        vec![
            Message::user("first prompt".into()),
            Message::user("abandoned prompt".into()),
        ],
    );
    app.checkpoint();
    let abandoned_id = app.state.session.messages()[1].id;
    app.rewind_to(RewindEntry {
        turn_index: 1,
        prompt_preview: "2: abandoned".into(),
    });

    let mut branch = crate::active_session_history(&app.state.session).unwrap();
    let parent_id = branch.last().map(|item| item.id);
    branch.extend(expand_message(
        &Message::user("replacement prompt".into()),
        parent_id,
    ));
    let branch_head = branch.last().unwrap().id;
    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(
        caudra_agent::HistorySnapshot::new(branch),
    )));
    app.checkpoint();

    assert_eq!(app.state.session.meta.pending_revert, None);
    assert_eq!(app.state.session.meta.history_head, Some(branch_head));
    let id = app.state.session.id;
    drain_writer(app, writer);

    let loaded = AppSession::load(id, &dir).unwrap();
    assert_eq!(loaded.messages().len(), 3);
    assert!(loaded.messages().iter().any(|item| item.id == abandoned_id));
    assert_eq!(crate::session_history_head(&loaded), Some(branch_head));
    assert_eq!(crate::active_session_history(&loaded).unwrap().len(), 2);
    assert_eq!(loaded.meta.pending_revert, None);
}

#[test]
fn reset_session_never_writes_the_old_conversation_under_the_new_id() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let _live = attach_live_history(&mut app, vec![Message::user("old talk".into())]);
    app.checkpoint();
    let old_id = app.state.session.id;

    app.reset_session();
    app.checkpoint();
    let new_id = app.state.session.id;
    assert_ne!(new_id, old_id);

    drain_writer(app, writer);
    assert_eq!(AppSession::load(old_id, &dir).unwrap().messages().len(), 1);
    assert!(
        AppSession::load(new_id, &dir).is_err(),
        "an empty session has no content to persist",
    );
}

/// Two traps in one switch. `install_local_history` has to drop the mirror
/// handle, or the old agent's messages land under the freshly loaded id. And
/// `revision` is `#[serde(skip)]`, so the loaded session starts back at zero and
/// can collide with the revision already sent for the one it replaced, which
/// only keying `last_sent` by id survives.
#[test]
fn load_session_persists_the_new_session_and_leaks_no_history_into_it() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let mut stored = AppSession::new("test-model", &app.state.session.cwd);
    crate::push_history_message(&mut stored, Message::user(STORED_SESSION_TEXT.into()));
    stored.save(&dir).unwrap();

    let _live = attach_live_history(&mut app, vec![Message::user(LIVE_AGENT_TEXT.into())]);
    app.input_box.set_input(UNSENT_DRAFT.into());
    app.checkpoint();
    let (live_id, sent_revision) = (app.state.session.id, app.state.session.revision());

    app.load_session(stored.id);
    assert_eq!(app.state.session.id, stored.id);
    // Walk the loaded session up to the revision already sent for the live one,
    // so the checkpoint below lands on the exact collision.
    let session = app.state.session_mut();
    while session.revision() + 1 < sent_revision {
        session.set_title(format!("{BUMP_TITLE}{}", session.revision()));
    }
    app.input_box.set_input(SWITCHED_DRAFT.into());
    app.checkpoint();
    assert_eq!(
        app.state.session.revision(),
        sent_revision,
        "both sessions must sit at the same revision for this to test anything"
    );

    drain_writer(app, writer);
    let loaded = AppSession::load(stored.id, &dir).unwrap();
    assert_eq!(loaded.meta.input_draft.as_deref(), Some(SWITCHED_DRAFT));
    assert_eq!(loaded.messages().len(), 1);
    assert!(matches!(
        &loaded.messages()[0].kind,
        HistoryItemKind::User { text, .. } if text == STORED_SESSION_TEXT
    ));
    let previous = AppSession::load(live_id, &dir).unwrap();
    assert!(matches!(
        &previous.messages()[0].kind,
        HistoryItemKind::User { text, .. } if text == LIVE_AGENT_TEXT
    ));
}

#[test]
fn idle_checkpoint_changes_nothing() {
    let mut app = test_app();
    crate::push_history_message(app.state.session_mut(), Message::user("hello".into()));
    app.checkpoint();
    let (revision, updated_at) = (app.state.session.revision(), app.state.session.updated_at);

    app.checkpoint();
    app.checkpoint();

    assert_eq!(app.state.session.revision(), revision);
    assert_eq!(app.state.session.updated_at, updated_at);
}

/// Issue #675: a crash between a keystroke and submit threw the draft away,
/// because nothing was written until the turn ended. The first key lands within
/// a frame now, and the keys behind it ride along on a later write rather than
/// each costing an `fsync`.
#[test]
fn first_draft_keystroke_lands_and_the_rest_coalesce() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    let first = type_draft_leaving_last_key_waiting(&mut app);
    assert_eq!(
        app.last_sent.as_ref(),
        Some(&first),
        "a keystroke on its own waits instead of costing a write",
    );

    app.checkpoint_with(Duration::ZERO);
    assert_ne!(
        app.last_sent.as_ref(),
        Some(&first),
        "and lands once the delay is up"
    );

    let id = app.state.session.id;
    drain_writer(app, writer);
    let saved = AppSession::load(id, &dir).unwrap();
    assert_eq!(saved.meta.input_draft.as_deref(), Some(TYPED_DRAFT));
    assert!(saved.messages().is_empty());
}

#[test]
fn a_content_change_writes_the_waiting_draft_with_it() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    type_draft_leaving_last_key_waiting(&mut app);

    crate::push_history_message(
        app.state.session_mut(),
        Message::user(LIVE_AGENT_TEXT.into()),
    );
    app.checkpoint_with(SOFT_DELAY_HELD);

    let id = app.state.session.id;
    drain_writer(app, writer);
    let saved = AppSession::load(id, &dir).unwrap();
    assert_eq!(saved.messages().len(), 1, "content never waits");
    assert_eq!(saved.meta.input_draft.as_deref(), Some(TYPED_DRAFT));
}

#[test]
fn shutdown_writes_a_draft_that_is_still_waiting() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    type_draft_leaving_last_key_waiting(&mut app);

    app.checkpoint_now();

    let id = app.state.session.id;
    drain_writer(app, writer);
    let saved = AppSession::load(id, &dir).unwrap();
    assert_eq!(saved.meta.input_draft.as_deref(), Some(TYPED_DRAFT));
}

#[test]
fn shutdown_preparation_preserves_unclaimed_queue_for_the_final_checkpoint() {
    let mut app = app_with_queued_message();

    app.prepare_shutdown();
    app.disconnect_agent_queue();
    app.checkpoint_now();

    assert_eq!(
        app.state.session.meta.queued_messages,
        [stored_queued_prompt("queued")]
    );
}

/// Submitting empties the draft a frame before the agent mirrors the prompt
/// back. Delete the session in that gap and the user loses the one they were
/// just starting.
#[test]
fn submitting_the_draft_keeps_the_session_on_disk() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    type_draft_leaving_last_key_waiting(&mut app);
    let id = app.state.session.id;

    app.update(Msg::Key(key(KeyCode::Enter)));
    app.checkpoint();
    assert!(!app.has_content(), "the submit window is what this covers");

    drain_writer(app, writer);
    assert!(AppSession::load(id, &dir).is_ok());
}

/// The draft put the session on disk, and deleting it leaves nothing worth
/// keeping. Without the delete the file survives with the abandoned draft in
/// it, and the picker offers an empty session to resume.
#[test]
fn deleting_the_draft_takes_the_session_off_disk() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    type_draft_leaving_last_key_waiting(&mut app);
    let id = app.state.session.id;

    for _ in TYPED_DRAFT.chars() {
        app.update(Msg::Key(key(KeyCode::Backspace)));
    }
    app.checkpoint();
    assert!(app.last_sent.is_none(), "nothing is on disk to stamp");

    drain_writer(app, writer);
    assert!(AppSession::load(id, &dir).is_err());
}

/// The second result goes through the append cursor the first one opened, so a
/// stale cursor would quietly drop or duplicate it.
#[test]
fn two_tool_results_checkpointed_separately_both_reach_disk() {
    let (_tmp, dir, writer, mut app) = tempdir_app();
    crate::push_history_message(app.state.session_mut(), Message::user("prompt".into()));
    app.status = Status::Streaming;
    app.run_id = 1;

    for tool_id in TOOL_IDS {
        app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            id: tool_id.into(),
            tool: "bash".into(),
            output: ToolOutput::Plain(tool_text(tool_id).into()),
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
        }))));
        app.checkpoint();
    }

    let id = app.state.session.id;
    drain_writer(app, writer);
    let loaded = AppSession::load(id, &dir).unwrap();
    for tool_id in TOOL_IDS {
        match loaded.tool_outputs().get(tool_id).map(Arc::as_ref) {
            Some(ToolOutput::Plain(out)) => assert_eq!(out.text, tool_text(tool_id)),
            other => panic!("missing plain output for {tool_id}: {other:?}"),
        }
    }
}

/// The `Done` path clears `chat_index` right after pruning it, so nothing can
/// rebuild the tabs later. Only the `sync_subagents` call inside
/// `retain_resolved_subagents` carries the survivors over.
#[test]
fn turn_end_keeps_only_the_subagents_that_finished() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    for (task_id, name) in [
        (FINISHED_TASK_ID, "finished child"),
        (UNFINISHED_TASK_ID, "open child"),
    ] {
        app.update(subagent_msg(
            AgentEvent::TextDelta { text: "x".into() },
            task_id,
            Some(name),
        ));
    }
    finish_subagent(&mut app, FINISHED_TASK_ID, false);
    assert_eq!(app.state.session.subagents().len(), 2);

    app.update(done_event());
    assert!(app.chat_index.is_empty());
    app.checkpoint();

    let ids: Vec<_> = app
        .state
        .session
        .subagents()
        .iter()
        .map(|sa| sa.tool_use_id.as_str())
        .collect();
    assert_eq!(ids, [FINISHED_TASK_ID]);
}

fn change_directory(app: &mut App, cwd: &Path) {
    app.install_working_directory(cwd, PermissionsConfig::default());
}

const CD_SHELL_ID: &str = "cd-shell";
const CD_SHELL_COMMAND: &str = "pwd";
/// A running shell card's annotation closing on its deadline, which it only
/// does when there is no directory to name after it.
const NO_WORKDIR_ANNOTATION: &str = "(2m timeout)";
const CD_TRANSCRIPT_MSG: &str =
    "a command sent to the directory a cd moved into is not named as running elsewhere";

/// The compiler holds every new chat to being given the session's directory,
/// but nothing holds a `cd` to handing the transcript the new one.
#[test]
fn a_cd_moves_the_directory_the_transcript_resolves_against() {
    let dir = TempDir::new().expect("a temporary directory");
    let mut app = app_without_splash();
    change_directory(&mut app, dir.path());

    let AgentEvent::ToolStart(mut start) = tool_start(CD_SHELL_ID, SHELL_TOOL_NAME) else {
        unreachable!()
    };
    start.raw_input = Some(serde_json::json!({
        "command": CD_SHELL_COMMAND,
        "workdir": dir.path(),
    }));
    app.update(agent_msg(AgentEvent::ToolStart(start)));

    let screen = rendered(&mut app);
    assert!(
        screen.contains(NO_WORKDIR_ANNOTATION),
        "{CD_TRANSCRIPT_MSG}: {screen}"
    );
}

/// The popup is fed by a walker thread, so a test settles it before asserting
/// on what it matched.
fn mention_popup_at(app: &mut App, cwd: &std::path::Path, query: &str) {
    change_directory(app, cwd);
    for character in query.chars() {
        app.update(Msg::Key(key(KeyCode::Char(character))));
    }
    app.mention_popup.settle();
}

#[test]
fn typing_an_at_sigil_opens_the_mention_popup_and_completing_splices_in_place() {
    let dir = TempDir::new().expect("a temporary directory");
    std::fs::write(dir.path().join("target.rs"), "body\n").expect("a file");
    let mut app = test_app();

    mention_popup_at(&mut app, dir.path(), "see @targ");
    assert!(app.mention_popup.is_open(), "{MENTION_POPUP_CLOSED}");

    app.update(Msg::Key(key(KeyCode::Enter)));

    assert_eq!(app.input_box.buffer.display_text(), "see @target.rs");
    assert!(!app.mention_popup.is_open(), "{MENTION_POPUP_LINGERED}");
    let mentions = app.input_box.mentions();
    assert_eq!(mentions.len(), 1, "{MENTION_UNRESOLVED}");
    assert_eq!(
        mentions[0].1.local_path(),
        Some(std::path::Path::new("target.rs"))
    );
}

/// Where `needle` was drawn, so a test can press the row the reader sees.
fn screen_hit(app: &mut App, needle: &str) -> (u16, u16) {
    let backend = ratatui::backend::TestBackend::new(TEST_AREA.width, TEST_AREA.height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.view(frame)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (buffer.area.y..buffer.area.bottom())
        .find_map(|row| {
            let line: String = (buffer.area.x..buffer.area.right())
                .map(|column| buffer[(column, row)].symbol())
                .collect();
            line.find(needle).map(|column| (row, column as u16))
        })
        .expect(MENTION_ROW_MISSING)
}

#[test]
fn clicking_a_popup_row_completes_the_mention_it_shows() {
    let dir = TempDir::new().expect("a temporary directory");
    std::fs::write(dir.path().join("target.rs"), "body\n").expect("a file");
    let mut app = test_app();

    mention_popup_at(&mut app, dir.path(), "see @targ");
    let (row, column) = screen_hit(&mut app, "target.rs");

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        column,
        row,
    ));

    assert_eq!(app.input_box.buffer.display_text(), "see @target.rs");
    assert!(!app.mention_popup.is_open(), "{MENTION_POPUP_LINGERED}");
}

/// A transcript holding one user message that mentions a real file, with the
/// screen position of the mention.
fn transcript_mention(app: &mut App) -> (TempDir, u16, u16) {
    let dir = TempDir::new().expect("a temporary directory");
    std::fs::write(dir.path().join(MENTIONED_FILE), "body\n").expect("a file");
    app.install_working_directory(dir.path(), PermissionsConfig::default());
    app.main_chat()
        .push_user_message(format!("look at @{MENTIONED_FILE} please"));
    let (row, column) = screen_hit(app, MENTIONED_FILE);
    (dir, row, column)
}

#[test]
fn clicking_a_mention_in_the_transcript_opens_the_workbench() {
    let mut app = test_app();
    let (_dir, row, column) = transcript_mention(&mut app);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        column,
        row,
    ));

    assert!(app.workbench.is_open(), "{MENTION_NOT_OPENED}");
    assert!(
        rendered(&mut app).contains(MENTIONED_FILE),
        "{MENTION_NOT_OPENED}"
    );
}

/// Pressing a mention and releasing on prose is a slip rather than a choice.
#[test]
fn releasing_off_the_mention_opens_nothing() {
    let mut app = test_app();
    let (_dir, row, column) = transcript_mention(&mut app);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 0, row));

    assert!(!app.workbench.is_open(), "{MENTION_OPENED}");
}

/// Dragging across a mention is how a reader copies the text around it, so it
/// must stay a selection.
#[test]
fn dragging_over_a_mention_selects_instead_of_opening() {
    let mut app = test_app();
    let (_dir, row, column) = transcript_mention(&mut app);

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        column + 5,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        column + 5,
        row,
    ));

    assert!(!app.workbench.is_open(), "{MENTION_OPENED}");
    assert!(app.selection_state.is_some(), "{MENTION_ATE_SELECTION}");
}

/// Clicking away moves the caret out of the query, and the popup has no
/// business outliving it.
#[test]
fn clicking_the_composer_closes_the_mention_popup() {
    let dir = TempDir::new().expect("a temporary directory");
    std::fs::write(dir.path().join("target.rs"), "body\n").expect("a file");
    let mut app = test_app();

    mention_popup_at(&mut app, dir.path(), "see @targ");
    let (row, column) = screen_hit(&mut app, "see @targ");

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));

    assert!(!app.mention_popup.is_open(), "{MENTION_POPUP_LINGERED}");
}

/// A one-commit log window. No repository is created: the index is the whole
/// source of truth, which is the property the `#` scanner rests on.
fn commit_index() -> CommitIndex {
    CommitIndex::loaded(vec![CommitSummary {
        id: COMMIT_ID.to_owned(),
        subject: COMMIT_SUBJECT.to_owned(),
        author: COMMIT_AUTHOR.to_owned(),
        committed_unix_seconds: COMMIT_UNIX_SECONDS,
    }])
}

/// Types `query` and then supplies the window the `#` asked for, which is what
/// the reload would have delivered. The read itself is dropped: these tests are
/// about the popup, and `repo::log` answers for itself elsewhere.
fn commit_popup_at(app: &mut App, query: &str) {
    for character in query.chars() {
        app.update(Msg::Key(key(KeyCode::Char(character))));
    }
    app.commit_reload = CommitReload::Idle;
    app.set_commit_index(commit_index());
    app.resync_dropdowns();
    app.commit_popup.settle();
}

#[test_case("#a1b2", KeyCode::Enter; "hash_enter")]
#[test_case("#a1b2", KeyCode::Tab; "hash_tab")]
#[test_case("#login", KeyCode::Enter; "title_enter")]
#[test_case("#login", KeyCode::Tab; "title_tab")]
fn typing_a_hash_sigil_opens_the_commit_popup_and_completing_splices_in_place(
    query: &str,
    completion: KeyCode,
) {
    let mut app = test_app();
    app.update(Msg::Paste(COMMIT_COMPLETION_SUFFIX.to_owned()));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Home,
        KeyModifiers::CONTROL,
    )));

    commit_popup_at(&mut app, &format!("{COMMIT_COMPLETION_PREFIX}{query}"));
    assert_eq!(
        app.input_box.buffer.display_text(),
        format!("{COMMIT_COMPLETION_PREFIX}{query}{COMMIT_COMPLETION_SUFFIX}")
    );
    assert!(app.commit_popup.is_open(), "{COMMIT_POPUP_CLOSED}");

    app.update(Msg::Key(key(completion)));

    assert_eq!(
        app.input_box.buffer.display_text(),
        format!("{COMMIT_COMPLETION_PREFIX}#{COMMIT_SHORT_ID}{COMMIT_COMPLETION_SUFFIX}")
    );
    assert!(!app.commit_popup.is_open(), "{COMMIT_POPUP_LINGERED}");
    let commits = app.input_box.commits();
    assert_eq!(commits.len(), 1, "{COMMIT_UNRESOLVED}");
    assert_eq!(commits[0].1.id, COMMIT_SHORT_ID);
}

/// Clicking away moves the caret out of the query, and the popup has no
/// business outliving it.
#[test]
fn clicking_the_composer_closes_the_commit_popup() {
    let mut app = test_app();

    commit_popup_at(&mut app, "landed in #a1b2");
    let (row, column) = screen_hit(&mut app, "landed in #a1b2");

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));

    assert!(!app.commit_popup.is_open(), "{COMMIT_POPUP_LINGERED}");
}

/// Prose that merely contains a `#` is not a revision, so nothing opens and
/// nothing is sent as context.
#[test]
fn a_heading_in_the_composer_is_never_a_commit() {
    let mut app = test_app();

    commit_popup_at(&mut app, "# Notes");

    assert!(!app.commit_popup.is_open(), "{COMMIT_POPUP_LINGERED}");
    assert!(app.input_box.commits().is_empty(), "{COMMIT_UNRESOLVED}");
}

/// Ticks until the log the `#` asked for has landed, the way the event loop
/// would.
#[track_caller]
fn settle_commit_reload(app: &mut App) {
    let deadline = Instant::now() + COMMIT_RELOAD_TIMEOUT;
    while !matches!(app.commit_reload, CommitReload::Idle) {
        assert!(Instant::now() < deadline, "{COMMIT_RELOAD_STALLED}");
        let _ = app.tick();
        std::thread::yield_now();
    }
}

/// A project whose history is a round trip away must still answer the `#` on the
/// frame it was typed on, which is what the spinner is for.
#[test]
fn a_hash_typed_before_the_log_arrives_draws_a_spinner_and_then_the_list() {
    let mut app = test_app();

    app.update(Msg::Key(key(KeyCode::Char('#'))));

    assert!(app.commit_popup.is_loading(), "{COMMIT_POPUP_CLOSED}");
    assert!(!app.commit_popup.is_open(), "{COMMIT_LOADING_CAPTURES}");

    app.commit_reload = CommitReload::Idle;
    app.set_commit_index(commit_index());
    app.resync_dropdowns();
    app.commit_popup.settle();

    assert!(app.commit_popup.is_open(), "{COMMIT_POPUP_CLOSED}");
    assert!(!app.commit_popup.is_loading(), "{COMMIT_LOADING_LINGERED}");
}

/// The window is read when the reader asks for it, not once at startup, so a
/// commit made during the session is mentionable without leaving it.
#[test]
fn typing_a_hash_rereads_the_log_and_replaces_the_window_it_was_holding() {
    let directory = TempDir::new().expect("a temporary directory");
    let mut app = test_app();
    app.state.session_mut().cwd = directory.path().to_string_lossy().into_owned();
    app.set_commit_index(commit_index());

    app.update(Msg::Key(key(KeyCode::Char('#'))));
    settle_commit_reload(&mut app);

    // The directory is not a repository, so the fresh read is empty. What
    // matters is that it replaced the stale window rather than being ignored.
    assert!(
        app.input_box.commit_index().commits().is_empty(),
        "{COMMIT_WINDOW_STALE}"
    );
    assert!(!app.commit_popup.is_active(), "{COMMIT_POPUP_LINGERED}");
    assert!(app.input_box.commits().is_empty(), "{COMMIT_UNRESOLVED}");
}

/// A remote workspace has no log this process can walk, so the request goes to
/// the workbench, which owns the only client that can page one.
#[test]
fn a_remote_session_asks_the_workbench_for_its_log_rather_than_reading_locally() {
    let mut app = test_app();
    app.workspace_session = Some(remote_workspace_session());

    app.update(Msg::Key(key(KeyCode::Char('#'))));

    assert!(
        matches!(app.commit_reload, CommitReload::Workbench),
        "{COMMIT_READ_LOCALLY}"
    );
}

/// A second `#` while a read is in flight rides on it. Stacking reads would
/// hammer a remote workspace for every keystroke that reopens the popup.
#[test]
fn a_reload_already_in_flight_is_not_started_again() {
    let mut app = test_app();
    app.workspace_session = Some(remote_workspace_session());

    app.update(Msg::Key(key(KeyCode::Char('#'))));
    app.update(Msg::Key(key(KeyCode::Backspace)));
    app.update(Msg::Key(key(KeyCode::Char('#'))));

    assert!(
        matches!(app.commit_reload, CommitReload::Workbench),
        "{COMMIT_READ_LOCALLY}"
    );
}

/// A sent hash is a click target the way a sent path is, and it lands on
/// source control rather than the explorer.
#[test]
fn clicking_a_commit_in_the_transcript_opens_source_control() {
    let mut app = test_app();
    app.set_commit_index(commit_index());
    app.main_chat()
        .push_user_message("landed in #a1b2c3d".to_owned());
    let (row, column) = screen_hit(&mut app, "#a1b2c3d");

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row,
    ));
    app.update(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        column,
        row,
    ));

    assert!(app.workbench.is_open(), "{COMMIT_NOT_OPENED}");
}

#[test]
fn completing_a_mention_leaves_an_earlier_paste_token_intact() {
    let dir = TempDir::new().expect("a temporary directory");
    std::fs::write(dir.path().join("target.rs"), "body\n").expect("a file");
    let mut app = test_app();
    app.input_box.handle_paste("one\ntwo\nthree");

    mention_popup_at(&mut app, dir.path(), "@targ");
    app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(app.input_box.has_pastes(), "{MENTION_DROPPED_A_PASTE}");
    assert!(
        app.input_box
            .buffer
            .expanded_text()
            .contains("one\ntwo\nthree")
    );
    assert!(app.input_box.buffer.display_text().ends_with("@target.rs"));
}

#[test]
fn run_builtin_file_picker_opens_modal() {
    let mut app = test_app();
    assert!(app.run_builtin(BuiltinAction::FilePicker).is_empty());
    assert!(app.file_picker.is_open());
}

const NOTHING_TO_COPY: &str = "Nothing to copy";

#[test]
fn copy_message_reports_when_there_is_no_reply() {
    let mut app = test_app();
    app.run_builtin(BuiltinAction::CopyMessage);
    assert_eq!(app.status_bar.flash_text(), Some(NOTHING_TO_COPY));
}

/// The action reads the markdown source, not the rendered lines, so a reply
/// is copyable whether or not the transcript has been drawn.
#[test]
fn copy_message_picks_the_last_assistant_reply() {
    const REPLY: &str = "# Heading\n\n- **bold** item";
    let mut app = test_app();
    let chat = &mut app.chats[app.active_chat];
    chat.push(DisplayMessage::new(DisplayRole::Assistant, "older".into()));
    chat.push(DisplayMessage::new(DisplayRole::Assistant, REPLY.into()));

    assert_eq!(chat.last_reply_source().as_deref(), Some(REPLY));
    app.run_builtin(BuiltinAction::CopyMessage);
    assert_ne!(app.status_bar.flash_text(), Some(NOTHING_TO_COPY));
}

#[test]
fn run_builtin_model_picker_opens_and_refreshes() {
    let mut app = test_app();
    let actions = app.run_builtin(BuiltinAction::ModelPicker);
    assert!(app.model_picker.is_open());
    assert!(matches!(&actions[..], [Action::RefreshModels]));
}

const CHORD_LEAKED: &str = "the key after the leader must never reach the composer";
const LEADER_STAYS_ARMED: &str = "the panel must disarm as soon as the chord resolves";
const UNBOUND_CHORD_IS_ANNOUNCED: &str = "an unbound chord must say so rather than act on a typo";

#[test]
fn arming_the_leader_shows_the_panel_and_types_nothing() {
    let mut app = test_app();

    assert!(app.update(Msg::Key(kb::LEADER.to_key_event())).is_empty());

    assert!(app.which_key.is_armed());
    assert_eq!(app.input_box.buffer.value(), "", "{CHORD_LEAKED}");
}

#[test_case(KeyCode::Esc ; "esc_backs_out")]
#[test_case(kb::QUIT.code ; "quit_backs_out")]
fn cancelling_a_chord_is_silent(code: KeyCode) {
    let mut app = test_app();
    app.update(Msg::Key(kb::LEADER.to_key_event()));

    let modifiers = match code {
        KeyCode::Esc => KeyModifiers::NONE,
        _ => kb::QUIT.modifiers,
    };
    let actions = app.update(Msg::Key(KeyEvent::new(code, modifiers)));

    assert!(actions.is_empty());
    assert!(!app.which_key.is_armed(), "{LEADER_STAYS_ARMED}");
    assert_eq!(app.input_box.buffer.value(), "", "{CHORD_LEAKED}");
    assert_eq!(app.status_bar.flash_text(), None);
}

#[test]
fn an_unbound_chord_flashes_and_types_nothing() {
    let mut app = test_app();
    app.update(Msg::Key(kb::LEADER.to_key_event()));

    let actions = app.update(Msg::Key(key(KeyCode::Char('ß'))));

    assert!(actions.is_empty());
    assert!(!app.which_key.is_armed(), "{LEADER_STAYS_ARMED}");
    assert_eq!(app.input_box.buffer.value(), "", "{CHORD_LEAKED}");
    assert!(
        app.status_bar
            .flash_text()
            .is_some_and(|text| text.contains(FLASH_NO_CHORD)),
        "{UNBOUND_CHORD_IS_ANNOUNCED}"
    );
}

/// A layout that needs shift for the chord's letter still reports it, so the
/// chord has to match with shift stripped.
#[test]
fn a_shifted_chord_key_still_resolves() {
    let mut app = test_app();
    app.update(Msg::Key(kb::LEADER.to_key_event()));

    app.update(Msg::Key(KeyEvent::new(
        chord::MODEL_PICKER.code,
        KeyModifiers::SHIFT,
    )));

    assert!(app.model_picker.is_open());
}

/// AltGr arrives as Ctrl+Alt, so the leader must not answer it and steal a
/// character the user meant to type.
#[test]
fn alt_gr_is_text_not_the_leader() {
    let mut app = test_app();

    app.update(Msg::Key(KeyEvent::new(
        kb::LEADER.code,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    )));

    assert!(!app.which_key.is_armed());
}

const PAN_CHART: &str = "```mermaid\nflowchart LR\n  A[Ingest events] --> B[Normalise schema] --> C[Enrich metadata] --> D[Write store]\n```";
const CURSOR_PROBE: &str = "abc";

fn app_with_chart() -> App {
    let mut app = test_app();
    app.chats[0].push(DisplayMessage::new(
        DisplayRole::Assistant,
        PAN_CHART.into(),
    ));
    let _ = rendered(&mut app);
    app
}

#[test]
fn shift_arrows_pan_a_wide_diagram() {
    let mut app = app_with_chart();

    app.update(Msg::Key(kb::PAN_RIGHT.to_key_event()));
    let _ = rendered(&mut app);
    assert_eq!(
        app.chats[0].panned_diagram_count(),
        1,
        "shift+right must pan the visible diagram"
    );

    app.update(Msg::Key(kb::PAN_LEFT.to_key_event()));
    let _ = rendered(&mut app);
    assert_eq!(app.chats[0].panned_diagram_count(), 0, "shift+left returns");
}

#[test]
fn shift_arrows_still_reach_the_composer_when_no_diagram_is_visible() {
    let mut app = test_app();
    for character in CURSOR_PROBE.chars() {
        app.update(Msg::Key(key(KeyCode::Char(character))));
    }
    app.update(Msg::Key(kb::PAN_LEFT.to_key_event()));
    app.update(Msg::Key(key(KeyCode::Char('X'))));
    assert_eq!(
        app.input_box.buffer.value(),
        "abX",
        "shift+left selects the last character, which typing then replaces"
    );
}

#[test]
fn plain_arrows_never_pan_a_diagram() {
    let mut app = app_with_chart();
    app.update(Msg::Key(key(KeyCode::Right)));
    app.update(Msg::Key(key(KeyCode::Left)));
    assert_eq!(
        app.chats[0].panned_diagram_count(),
        0,
        "plain arrows belong to the input box"
    );
}

#[test]
fn a_sideways_wheel_pans_the_diagram_under_the_pointer() {
    let mut app = app_with_chart();
    let reachable: Vec<u16> = (0..24)
        .filter(|&row| {
            app.update(mouse_event(MouseEventKind::ScrollRight, 5, row));
            let panned = app.chats[0].panned_diagram_count() > 0;
            if panned {
                app.update(mouse_event(MouseEventKind::ScrollLeft, 5, row));
                assert_eq!(app.chats[0].panned_diagram_count(), 0, "row {row} resets");
            }
            panned
        })
        .collect();

    assert!(
        !reachable.is_empty(),
        "the wheel must reach the diagram rows"
    );
    assert!(
        reachable.len() < 24,
        "rows outside the diagram must not pan: {reachable:?}"
    );
}

#[test]
fn a_sideways_wheel_over_prose_pans_nothing() {
    let mut app = test_app();
    app.chats[0].push(DisplayMessage::new(
        DisplayRole::Assistant,
        "just some prose".into(),
    ));
    let _ = rendered(&mut app);
    for row in 0..24 {
        app.update(mouse_event(MouseEventKind::ScrollRight, 5, row));
    }
    assert_eq!(app.chats[0].panned_diagram_count(), 0);
}

/// 43 columns of model id, which puts `/usage`'s row well past an 80-column
/// terminal and leaves the cost column off the modal.
const LONG_SPEND_MODEL: &str = "a-model-with-a-deliberately-long-identifier";
const LONG_SPEND_TEXT: &str = "0.750";
const CLIPPED_COST: &str = "the cost column must start off the edge of a narrow modal";
const WHEEL_DROPPED: &str = "a sideways wheel over a clipped modal must pan it";

fn app_with_wide_spend() -> App {
    let mut app = test_app();
    app.state.session_mut().add_model_usage(
        LONG_SPEND_MODEL,
        StoredTokenUsage {
            input: 1_000_000,
            cost: Some(0.75),
            ..StoredTokenUsage::default()
        },
    );
    app.execute_command(cmd("/usage"), 0);
    app
}

/// The wheel used to be dropped outright while a modal was open, which left the
/// clipped half of a wide table unreachable by pointer.
#[test]
fn a_sideways_wheel_pans_the_modal_it_is_over() {
    let mut app = app_with_wide_spend();

    assert!(
        !rendered(&mut app).contains(LONG_SPEND_TEXT),
        "{CLIPPED_COST}"
    );

    for _ in 0..8 {
        app.update(mouse_event(MouseEventKind::ScrollRight, 5, 5));
    }

    assert!(
        rendered(&mut app).contains(LONG_SPEND_TEXT),
        "{WHEEL_DROPPED}"
    );
}

/// The chord reaches the modal the same way the wheel does, and the transcript
/// behind it never sees either while one is open.
#[test]
fn the_pan_chord_reaches_an_open_modal() {
    let mut app = app_with_wide_spend();
    let _ = rendered(&mut app);

    for _ in 0..8 {
        app.update(Msg::Key(kb::PAN_RIGHT.to_key_event()));
    }

    assert!(
        rendered(&mut app).contains(LONG_SPEND_TEXT),
        "{WHEEL_DROPPED}"
    );
}

/// `test_app` shares one state dir across the whole run, and the stash is a
/// single global file, so these tests need their own.
fn stash_app() -> (TempDir, App) {
    let (tmp, _dir, _writer, mut app) = tempdir_app();
    let (shared_queue, _rx) = shared_queue::queue();
    app.queue.set_shared(shared_queue);
    (tmp, app)
}

fn stash_entries(app: &App) -> Vec<StashEntry> {
    PromptStash::open(&app.storage).unwrap().entries().to_vec()
}

#[test]
fn stash_moves_the_draft_out_of_the_composer() {
    let (_tmp, mut app) = stash_app();
    app.update(Msg::Paste("draft text".into()));
    app.execute_command(cmd("/stash"), 0);

    assert!(app.input_box.is_empty());
    let entries = stash_entries(&app);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, "draft text");
    assert_eq!(entries[0].cwd, app.state.session.cwd);
}

#[test]
fn stashing_an_empty_composer_stores_nothing() {
    let (_tmp, mut app) = stash_app();
    app.execute_command(cmd("/stash"), 0);
    assert!(stash_entries(&app).is_empty());
}

#[test]
fn stash_pop_restores_paste_tokens_and_images() {
    let (_tmp, mut app) = stash_app();
    app.update(Msg::Paste("a\nb\nc".into()));
    with_image(&mut app);
    app.execute_command(cmd("/stash"), 0);
    app.execute_command(cmd("/stash-pop"), 0);

    assert_eq!(app.input_box.buffer.display_text(), "[Pasted 3 lines] ");
    assert_eq!(app.input_box.buffer.expanded_text(), "a\nb\nc ");
    assert_eq!(app.input_box.pending_images().len(), 1);
    assert!(stash_entries(&app).is_empty());
}

#[test]
fn stash_pop_takes_the_newest_entry() {
    let (_tmp, mut app) = stash_app();
    for text in ["older", "newer"] {
        app.update(Msg::Paste(text.into()));
        app.execute_command(cmd("/stash"), 0);
    }
    app.execute_command(cmd("/stash-pop"), 0);

    assert_eq!(app.input_box.buffer.value(), "newer");
    assert_eq!(stash_entries(&app)[0].text, "older");
}

#[test]
fn stash_pop_refuses_to_overwrite_a_draft() {
    let (_tmp, mut app) = stash_app();
    app.update(Msg::Paste("stashed".into()));
    app.execute_command(cmd("/stash"), 0);
    app.update(Msg::Paste("in progress".into()));
    app.execute_command(cmd("/stash-pop"), 0);

    assert_eq!(app.input_box.buffer.value(), "in progress");
    assert_eq!(stash_entries(&app).len(), 1);
}

#[test]
fn stash_pop_on_an_empty_stash_leaves_the_composer_alone() {
    let (_tmp, mut app) = stash_app();
    app.execute_command(cmd("/stash-pop"), 0);
    assert!(app.input_box.is_empty());
    assert!(!app.stash_picker.is_open());
}

#[test]
fn stash_list_restores_the_chosen_entry_and_drops_it() {
    let (_tmp, mut app) = stash_app();
    for text in ["older", "newer"] {
        app.update(Msg::Paste(text.into()));
        app.execute_command(cmd("/stash"), 0);
    }
    app.execute_command(cmd("/stash-list"), 0);
    assert!(app.stash_picker.is_open());

    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(!app.stash_picker.is_open());
    assert_eq!(app.input_box.buffer.value(), "older");
    assert_eq!(stash_entries(&app)[0].text, "newer");
}

#[test]
fn stash_list_deletes_an_entry_after_two_presses() {
    let (_tmp, mut app) = stash_app();
    for text in ["older", "newer"] {
        app.update(Msg::Paste(text.into()));
        app.execute_command(cmd("/stash"), 0);
    }
    app.execute_command(cmd("/stash-list"), 0);

    let delete = KeyEvent::new(kb::DELETE.code, kb::DELETE.modifiers);
    app.update(Msg::Key(delete));
    assert_eq!(
        stash_entries(&app).len(),
        2,
        "one press only arms the delete"
    );

    app.update(Msg::Key(delete));
    let entries = stash_entries(&app);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, "older");
    assert!(
        app.stash_picker.is_open(),
        "the picker stays up for more work"
    );
}

#[test]
fn deleting_the_last_stash_entry_closes_the_picker() {
    let (_tmp, mut app) = stash_app();
    app.update(Msg::Paste("only".into()));
    app.execute_command(cmd("/stash"), 0);
    app.execute_command(cmd("/stash-list"), 0);

    let delete = KeyEvent::new(kb::DELETE.code, kb::DELETE.modifiers);
    app.update(Msg::Key(delete));
    app.update(Msg::Key(delete));

    assert!(!app.stash_picker.is_open());
    assert!(stash_entries(&app).is_empty());
}

#[test]
fn stash_list_on_an_empty_stash_opens_nothing() {
    let (_tmp, mut app) = stash_app();
    app.execute_command(cmd("/stash-list"), 0);
    assert!(!app.stash_picker.is_open());
}

#[test_case(chord::STASH_PUSH, "" ; "push_clears_the_composer")]
#[test_case(chord::STASH_POP, "draft" ; "pop_is_inert_while_the_composer_is_busy")]
fn stash_keybindings_reach_the_builtin(bind: Bind, expected: &str) {
    let (_tmp, mut app) = stash_app();
    app.update(Msg::Paste("draft".into()));
    press_chord(&mut app, bind);
    assert_eq!(app.input_box.buffer.value(), expected);
}

#[test]
fn stashing_is_refused_while_a_queued_prompt_is_being_edited() {
    let (_tmp, mut app) = stash_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue_and_notify(queued_msg("queued"));
    app.queue.set_focus_at(0);
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.queue_editor_active());

    app.execute_command(cmd("/stash"), 0);

    assert_eq!(app.input_box.buffer.value(), "queued");
    assert!(stash_entries(&app).is_empty());
}

#[test]
fn restoring_is_refused_while_a_queued_prompt_is_being_edited() {
    let (_tmp, mut app) = stash_app();
    app.update(Msg::Paste("stashed".into()));
    app.execute_command(cmd("/stash"), 0);

    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue_and_notify(queued_msg("queued"));
    app.queue.set_focus_at(0);
    app.update(Msg::Key(key(KeyCode::Enter)));

    app.execute_command(cmd("/stash-pop"), 0);

    assert_eq!(app.input_box.buffer.value(), "queued");
    assert_eq!(stash_entries(&app).len(), 1);
}

const WORKBENCH_OPENS: &str = "the workbench chord must put the workbench on screen";
const WORKBENCH_CLOSES: &str = "the workbench chord must hand the screen back to the transcript";
const UNDO_MISSED: &str = "Ctrl+Z must undo the last edit wherever the draft is";
const UNDO_DRAFT: &str = "undo this";
const QUIT_REACHABLE: &str =
    "Ctrl+C without a selection must still reach quit, or the workbench traps the session";
const WHEEL_MISROUTED: &str =
    "the wheel must reach the open workbench, not the transcript behind it";
const LEADER_TRAPPED_MSG: &str =
    "the leader must arm over an open workbench, or its chords are unreachable";
const OVERLAY_HIDDEN: &str = "an overlay opened over the workbench must be drawn over it";
/// A chord description no transcript context offers, so seeing it proves the
/// panel is both drawn and scoped to the workbench. One of the first chords
/// listed, because a narrow screen fits one column of eight.
const WORKBENCH_CHORD_DESC: &str = "Narrow / widen the sidebar";
const PROMPT_UNANSWERABLE: &str =
    "the prompt must answer before the workbench, or the session hangs on it";

fn open_workbench() -> App {
    let mut app = test_app();
    press_chord(&mut app, chord::WORKBENCH);
    app
}

#[test]
fn the_workbench_chord_toggles_the_workbench() {
    let mut app = test_app();
    assert!(!app.workbench.is_open(), "{WORKBENCH_CLOSES}");

    press_chord(&mut app, chord::WORKBENCH);
    assert!(app.workbench.is_open(), "{WORKBENCH_OPENS}");

    press_chord(&mut app, chord::WORKBENCH);
    assert!(!app.workbench.is_open(), "{WORKBENCH_CLOSES}");
}

#[test]
fn the_workbench_command_opens_the_same_view() {
    let mut app = test_app();
    app.execute_command(cmd("/workbench"), 0);
    assert!(app.workbench.is_open(), "{WORKBENCH_OPENS}");
}

#[test]
fn esc_leaves_the_workbench() {
    let mut app = open_workbench();
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.workbench.is_open(), "{WORKBENCH_CLOSES}");
}

#[test]
fn ctrl_z_undoes_the_draft() {
    let undo = kb::UNDO.to_key_event();
    let mut app = test_app();
    app.update(Msg::Paste(UNDO_DRAFT.into()));
    app.update(Msg::Key(kb::DELETE_WORD.to_key_event()));
    assert!(app.update(Msg::Key(undo)).is_empty(), "{UNDO_MISSED}");
    assert_eq!(app.input_box.buffer.value(), UNDO_DRAFT, "{UNDO_MISSED}");

    let (_dir, _path, mut app) = workbench_repeat_editor();
    app.update(Msg::Key(key(KeyCode::End)));
    app.update(Msg::Key(key(KeyCode::Char('X'))));
    assert!(app.update(Msg::Key(undo)).is_empty(), "{UNDO_MISSED}");
    assert!(app.workbench.is_open(), "{UNDO_MISSED}");
    assert!(
        !rendered(&mut app).contains(&format!("{REPEAT_FIELD_TEXT}X")),
        "{UNDO_MISSED}"
    );
}

/// The wheel is aggregated into `Msg::Scroll` before `handle_mouse` runs, so
/// the workbench needs its own branch there. Without it the transcript scrolled
/// under an open workbench and none of its panes moved.
#[test]
fn the_wheel_reaches_an_open_workbench_rather_than_the_transcript() {
    let mut app = open_workbench();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().enable_auto_scroll();

    app.update(Msg::Scroll {
        column: 10,
        row: 10,
        delta: 3,
    });

    assert!(app.chats[0].auto_scroll(), "{WHEEL_MISROUTED}");
}

#[test]
fn ctrl_c_is_not_swallowed_by_an_open_workbench() {
    let mut app = open_workbench();
    let actions = app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert!(
        actions.iter().any(|a| matches!(a, Action::ManualExit)),
        "{QUIT_REACHABLE}"
    );
}

/// The workbench is a full-screen view, not a modal. It used to be rendered
/// with an early return that skipped every overlay, so anything opened over it
/// was invisible while it went on owning the keyboard.
#[test]
fn the_which_key_panel_draws_over_the_workbench() {
    let mut app = open_workbench();
    app.which_key = crate::components::which_key::WhichKey::new(Duration::ZERO);
    app.update(Msg::Key(kb::LEADER.to_key_event()));

    let frame = rendered(&mut app);

    assert!(frame.contains(WORKBENCH_CHORD_DESC), "{OVERLAY_HIDDEN}");
}

#[test_case(chord::HELP, "Keybindings" ; "help")]
#[test_case(chord::MODEL_PICKER, "Model" ; "model_picker")]
fn a_chord_that_opens_a_modal_draws_it_over_the_workbench(bind: Bind, needle: &str) {
    let mut app = open_workbench();
    press_chord(&mut app, bind);

    let frame = rendered(&mut app);

    assert!(frame.contains(needle), "{OVERLAY_HIDDEN}");
}

/// The severe one: an invisible prompt that still owns the keyboard cannot be
/// answered, so the agent waits on it forever.
#[test]
fn a_permission_prompt_is_visible_and_answerable_over_the_workbench() {
    let mut app = open_workbench();
    app.permission_prompt.open(
        "id".into(),
        caudra_config::ToolKey::native("bash"),
        vec!["execute".into()],
        None,
    );

    assert!(
        rendered(&mut app).contains(PERMISSION_TITLE),
        "{OVERLAY_HIDDEN}"
    );

    app.update(Msg::Key(kb::QUIT.to_key_event()));

    assert!(!app.permission_prompt.is_open(), "{PROMPT_UNANSWERABLE}");
    assert!(app.workbench.is_open(), "{PROMPT_UNANSWERABLE}");
}

/// End to end for the bug that sent this design back to the drawing board:
/// the workbench used to cut on `Ctrl+X` whenever a selection existed, so the
/// prefix never reached the host and every workbench chord went dead.
#[test]
fn the_leader_arms_over_an_open_workbench() {
    let mut app = open_workbench();

    let actions = app.update(Msg::Key(kb::LEADER.to_key_event()));

    assert!(actions.is_empty());
    assert!(app.which_key.is_armed(), "{LEADER_TRAPPED_MSG}");
}

#[test]
fn the_workbench_reports_its_own_keybind_contexts() {
    let app = open_workbench();
    let contexts = app.active_keybind_contexts();
    assert!(
        contexts.contains(&KeybindContext::Workbench),
        "the help modal must show the workbench keymap while it is open"
    );
    assert!(
        !contexts.contains(&KeybindContext::Editing),
        "the composer keymap must not be offered while the composer is hidden"
    );
}
