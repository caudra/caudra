//! Every built-in slash command with the grammar of what follows its name.

use caudra_agent::agent::COMPACT_DESCRIPTION;
use caudra_config::{Feature, FeatureFlags};

use super::ChatScope;
use super::grammar::{Arg, Choice, Node, Source, Subcommand};
use super::words;

pub(crate) const SEARCH_COMMAND: &str = "/search";
pub(crate) const ASIDE_NEW: &str = "/new";
pub(crate) const ASIDE_CATCHUP: &str = "/catchup";
const LEAF: Node = Node::args(&[]);

pub struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub args: Node<'static>,
    pub scope: ChatScope,
    /// Experiments any one of which makes the command available; empty for a
    /// command that always is.
    pub features: &'static [Feature],
}

impl BuiltinCommand {
    const fn new(name: &'static str, description: &'static str, args: Node<'static>) -> Self {
        Self {
            name,
            description,
            args,
            scope: ChatScope::Any,
            features: &[],
        }
    }

    const fn main_only(mut self) -> Self {
        self.scope = ChatScope::MainOnly;
        self
    }

    const fn requires(mut self, features: &'static [Feature]) -> Self {
        self.features = features;
        self
    }

    pub fn available(&self, features: FeatureFlags) -> bool {
        self.features.is_empty()
            || self
                .features
                .iter()
                .any(|&feature| features.enabled(feature))
    }

    pub fn max_args(&self) -> usize {
        self.args.max_args()
    }

    pub fn signature(&self) -> String {
        self.args.signature()
    }
}

/// The experiment that keeps the built-in command `name` out of this
/// process, so an explicit request can say how to turn it on.
pub fn disabled_feature(name: &str, features: FeatureFlags) -> Option<Feature> {
    BUILTIN_COMMANDS
        .iter()
        .find(|command| command.name.eq_ignore_ascii_case(name))
        .filter(|command| !command.available(features))
        .and_then(|command| command.features.first().copied())
}

const CONTEXT_ALL: &[Choice] = &[Choice::new(
    words::ALL_ARG,
    "Add the tools, profiles, memory and skills item by item",
)];
const STORAGE_ALL: &[Choice] = &[Choice::new(
    words::ALL_ARG,
    "Break every category down to its entries",
)];
const CHANGELOG_ALL: &[Choice] = &[Choice::new(
    words::ALL_ARG,
    "Every release, not only the new ones",
)];

const SEARCH_WORDS: &[Arg] = &[Arg::text(
    "words",
    "Words to search for, with kind:, tool:, since: and until: filters",
    None,
)];
const SEARCH_TABS: &[Subcommand] = &[
    Subcommand::new(
        words::CHAT_ARG,
        "Search the open chat",
        Node::args(SEARCH_WORDS),
    ),
    Subcommand::new(
        words::PROJECT_ARG,
        "Search this project's past sessions",
        Node::args(SEARCH_WORDS),
    ),
    Subcommand::new(
        words::ALL_ARG,
        "Search the past sessions of every project",
        Node::args(SEARCH_WORDS),
    ),
];

const STASH_SUBS: &[Subcommand] = &[
    Subcommand::new(
        words::POP_ARG,
        "Restore the most recently stashed prompt",
        LEAF,
    ),
    Subcommand::new(words::LIST_ARG, "Browse stashed prompts", LEAF),
];
const UPDATE_SUBS: &[Subcommand] = &[Subcommand::new(
    words::DISMISS_ARG,
    "Hide the update banner until Caudra restarts",
    LEAF,
)];

const TASK_ID: &[Arg] = &[Arg::value("id", "A task id", Some(Source::Tasks)).required()];
const TASK_OR_SHELL_ID: &[Arg] =
    &[Arg::value("id", "A task or shell id", Some(Source::TasksOrShells)).required()];
const TASK_SUBS: &[Subcommand] = &[
    Subcommand::new(words::LIST_ARG, "Open the task picker", LEAF),
    Subcommand::new(
        words::STATUS_ARG,
        "Open a task, or a shell command, with its details",
        Node::args(TASK_OR_SHELL_ID),
    ),
    Subcommand::new(
        words::BACKGROUND_ARG,
        "Let a foreground task go on in the background",
        Node::args(TASK_ID),
    ),
    Subcommand::new(words::CANCEL_ARG, "Cancel a task", Node::args(TASK_ID)),
];

const REVIEWED_ID: &[Arg] = &[Arg::value(
    "id",
    "A message opened for review",
    Some(Source::PeerReviewed),
)
.required()];
const INBOUND_POLICIES: &[Choice] = &[
    Choice::new(
        words::AUTO_ARG,
        "Deliver from this workspace and mode, hold the rest",
    ),
    Choice::new(words::ACCEPT_ARG, "Deliver every message"),
    Choice::new(words::HOLD_ARG, "Hold every message for review"),
    Choice::new(words::REFUSE_ARG, "Refuse every message"),
];
const INBOUND_POLICY: &[Arg] = &[Arg::words(
    "policy",
    "How this session takes peer messages",
    INBOUND_POLICIES,
)
.required()];
const MESSAGES_SUBS: &[Subcommand] = &[
    Subcommand::new(words::HELP_ARG, "Explain review and inbound policy", LEAF),
    Subcommand::new(
        words::APPROVE_ARG,
        "Accept a reviewed message",
        Node::args(REVIEWED_ID),
    ),
    Subcommand::new(
        words::REJECT_ARG,
        "Reject a reviewed message",
        Node::args(REVIEWED_ID),
    ),
    Subcommand::new(
        words::INBOUND_ARG,
        "Set this session's inbound policy",
        Node::args(INBOUND_POLICY),
    ),
];

const TOPIC_PATTERNS: &[Arg] = &[Arg::text(
    "patterns",
    "Topics such as ci.failures; * matches a segment, a final ** the rest",
    Some(Source::PeerTopics),
)
.required()];
const ON_OFF: &[Choice] = &[
    Choice::new(words::ON_ARG, "Receive broadcasts"),
    Choice::new(words::OFF_ARG, "Ignore broadcasts"),
];
const BROADCAST_STATE: &[Arg] = &[Arg::words("state", "", ON_OFF).required()];
const TOPICS_SUBS: &[Subcommand] = &[
    Subcommand::new(words::HELP_ARG, "Explain topics and subscriptions", LEAF),
    Subcommand::new(
        words::SUBSCRIBE_ARG,
        "Subscribe to topics",
        Node::args(TOPIC_PATTERNS),
    ),
    Subcommand::new(
        words::UNSUBSCRIBE_ARG,
        "Drop topic subscriptions",
        Node::args(TOPIC_PATTERNS),
    ),
    Subcommand::new(
        words::BROADCAST_ARG,
        "Turn broadcasts on or off",
        Node::args(BROADCAST_STATE),
    ),
];

const ANY_GROUP: &[Arg] = &[Arg::value("group", "A consumer group", None).required()];
const JOINED_GROUP: &[Arg] = &[Arg::value(
    "group",
    "A group this session takes work from",
    Some(Source::PeerJoinedGroups),
)
.required()];
const WORK_ID: &[Arg] = &[Arg::value("work", "A work item", None).required()];
const GROUPS_SUBS: &[Subcommand] = &[
    Subcommand::new(words::HELP_ARG, "Explain consumer groups", LEAF),
    Subcommand::new(
        words::JOIN_ARG,
        "Take work from a group",
        Node::args(ANY_GROUP),
    ),
    Subcommand::new(
        words::LEAVE_ARG,
        "Stop taking work from a group",
        Node::args(JOINED_GROUP),
    ),
    Subcommand::new(
        words::RETRY_ARG,
        "Queue a work item again",
        Node::args(WORK_ID),
    ),
    Subcommand::new(
        words::PAUSE_ARG,
        "Hold a queued work item",
        Node::args(WORK_ID),
    ),
    Subcommand::new(
        words::CANCEL_ARG,
        "Give up on a work item no member holds",
        Node::args(WORK_ID),
    ),
];

const RUN: &[Arg] = &[Arg::value(
    "run",
    "A run's display name or id",
    Some(Source::WorkflowRuns),
)
.required()];
const RESUMED_RUN: &[Arg] = &[
    Arg::value(
        "run",
        "A run's display name or id",
        Some(Source::WorkflowRuns),
    )
    .required(),
    Arg::value("budget", "A new agent budget", None),
];
const WORKFLOW_SUBS: &[Subcommand] = &[
    Subcommand::new(words::RUNS_ARG, "Open the runs picker", LEAF),
    Subcommand::new(words::PAUSE_ARG, "Pause a run", Node::args(RUN)),
    Subcommand::new(
        words::RESUME_ARG,
        "Resume a paused run",
        Node::args(RESUMED_RUN),
    ),
    Subcommand::new(words::STOP_ARG, "Stop a run", Node::args(RUN)),
];
const WORKFLOW_LAUNCH: &[Arg] = &[
    Arg::value("name", "A workflow to start", Some(Source::WorkflowNames)),
    Arg::text(
        "args",
        "--agent-budget N, then a JSON object or the query",
        None,
    ),
];

const AUTOMATION_NAME: &[Arg] =
    &[Arg::value("name", "An automation", Some(Source::Automations)).required()];
const ARMED_AUTOMATION: &[Arg] = &[
    Arg::value("name", "An automation", Some(Source::Automations)).required(),
    Arg::text("args", "Its args as a JSON object", None),
];
const AUTOMATIONS_SUBS: &[Subcommand] = &[
    Subcommand::new(
        words::ARM_ARG,
        "Arm an automation",
        Node::args(ARMED_AUTOMATION),
    ),
    Subcommand::new(
        words::DISARM_ARG,
        "Disarm an automation",
        Node::args(AUTOMATION_NAME),
    ),
];

const DESTINATION: &[Arg] = &[Arg::text(
    "directory",
    "The destination; leave it out to pick one",
    Some(Source::Directories),
)];
const SESSIONS_SUBS: &[Subcommand] = &[
    Subcommand::new(
        words::MOVE_ARG,
        "Move the current session; retain its counters and lifetime project attribution",
        Node::args(DESTINATION),
    )
    .main_only(),
    Subcommand::new(
        words::MIGRATE_ARG,
        "Migrate sessions with one exact stored cwd; historical project usage included by default",
        Node::args(DESTINATION),
    )
    .main_only(),
];

const MODEL_SUBS: &[Subcommand] = &[
    Subcommand::new(words::NEXT_ARG, "Switch to the next favorite model", LEAF).main_only(),
    Subcommand::new(
        words::REFRESH_ARG,
        "Check for a newer signed model list now",
        LEAF,
    ),
];
const DECISIONS_SUBS: &[Subcommand] = &[
    Subcommand::new(
        words::MODEL_ARG,
        "Pick the decision endpoint that screens tool calls",
        LEAF,
    )
    .main_only(),
    Subcommand::new(
        words::NEXT_ARG,
        "Switch to the next decision endpoint",
        LEAF,
    )
    .main_only(),
];
const PERMISSIONS_SUBS: &[Subcommand] = &[Subcommand::new(
    words::DISCOVER_ARG,
    "Open on the rules suggested from recent prompts",
    LEAF,
)
.aliases(&[words::SUGGESTED_ARG])];

const DIRECTORY: &[Arg] = &[Arg::value(
    "directory",
    "The new working directory; leave it out for home",
    Some(Source::Directories),
)];
const BRANCH: &[Arg] = &[Arg::value(
    "branch",
    "A new or existing branch; leave it out to have one named",
    None,
)];
const WORKTREE_SUBS: &[Subcommand] = &[
    Subcommand::new(words::NEW_ARG, "Create a worktree", Node::args(BRANCH)),
    Subcommand::new(
        words::CLOSE_ARG,
        "Close this worktree, keeping its branch",
        LEAF,
    ),
    Subcommand::new(words::REMOVE_ARG, "Remove this worktree", LEAF),
];

const GOAL_SUBS: &[Subcommand] = &[
    Subcommand::new(words::CLEAR_ARG, "Stop the active completion goal", LEAF)
        .aliases(words::GOAL_CLEAR_ALIASES),
    Subcommand::new(words::MODEL_ARG, "Assign the completion goal model", LEAF),
    Subcommand::new(words::LOG_ARG, "Every goal and check of this session", LEAF),
];
const GOAL_CONDITION: &[Arg] = &[Arg::text("condition", "Work until this is true", None)];
const AUTOPILOT_SUBS: &[Subcommand] = &[
    Subcommand::new(words::OFF_ARG, "Stop autopilot", LEAF).aliases(&[words::STOP_ARG]),
    Subcommand::new(words::MODEL_ARG, "Assign the autopilot model", LEAF),
    Subcommand::new(words::LOG_ARG, "What autopilot decided", LEAF),
];
const AUTOPILOT_GOAL: &[Arg] = &[Arg::text(
    "goal",
    "Work until this is true, answering for you",
    None,
)];

const THINKING_LEVEL: &[Arg] = &[Arg::value(
    "level",
    "off, adaptive, an effort level, or a token budget",
    Some(Source::ThinkingLevels),
)];

const OPERATION_ID: &[Arg] = &[
    Arg::value("operation-id", "An operation from /remote pending", None).required(),
    Arg::words(
        "confirm",
        "The operation may already have had effects",
        ACCEPT_EFFECTS,
    )
    .required(),
];
const ACCEPT_EFFECTS: &[Choice] = &[Choice::new(
    "--accept-possible-effects",
    "Clear it without cancelling, undoing, or resending it",
)];
const REMOTE_SUBS: &[Subcommand] = &[
    Subcommand::new("status", "Show the remote workspace", LEAF),
    Subcommand::new("pending", "List operations waiting on the remote", LEAF),
    Subcommand::new("reconnect", "Connect to the remote again", LEAF),
    Subcommand::new(
        "reconcile",
        "Settle pending operations with the remote",
        LEAF,
    ),
    Subcommand::new(
        "acknowledge",
        "Clear an operation from the pending report",
        Node::args(OPERATION_ID),
    ),
];

const SANDBOX_SUBS: &[Subcommand] = &[
    Subcommand::new(words::INSTANCES_ARG, "Live sandboxes", LEAF).aliases(&[words::STATUS_ARG]),
    Subcommand::new(words::PROFILES_ARG, "Sandbox profiles", LEAF),
    Subcommand::new(words::IMAGES_ARG, "Sandbox images", LEAF),
    Subcommand::new(words::PROVIDERS_ARG, "Sandbox providers and Doctor", LEAF)
        .aliases(&[words::DOCTOR_ARG]),
    Subcommand::new(
        words::RECONCILE_NETWORK_ARG,
        "Inspect durable outcomes, then reconcile saved networks",
        LEAF,
    ),
];

/// What the `/btw` composer runs. Custom commands join these, since their
/// text can be sent as a side question.
pub(crate) const ASIDE_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand::new(
        ASIDE_NEW,
        "Forget this side conversation and start a new one",
        LEAF,
    ),
    BuiltinCommand::new(
        ASIDE_CATCHUP,
        "Ask for a summary of the conversation /btw reads",
        Node::args(&[Arg::text("focus", "What to narrow the summary to", None)]),
    ),
];

pub const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand::new("/compact", COMPACT_DESCRIPTION, LEAF).main_only(),
    BuiltinCommand::new(
        "/continue",
        "Resume an interrupted turn without adding a message",
        LEAF,
    )
    .main_only(),
    BuiltinCommand::new("/new", "Start a new session", LEAF),
    BuiltinCommand::new("/help", "Show keybindings", LEAF),
    BuiltinCommand::new("/usage", "Show token usage breakdown", LEAF),
    BuiltinCommand::new(
        "/context",
        "Inspect active context window usage",
        Node::args(&[Arg::words("all", "", CONTEXT_ALL)]),
    ),
    BuiltinCommand::new(
        "/storage",
        "Inspect what the state directory holds",
        Node::args(&[Arg::words("all", "", STORAGE_ALL)]),
    ),
    BuiltinCommand::new("/logs", "Browse the structured log", LEAF),
    BuiltinCommand::new(
        "/docs",
        "Read Caudra's documentation, or search it",
        Node::args(&[Arg::text(
            "page",
            "A page or section, or words to search for",
            Some(Source::DocsAddresses),
        )]),
    ),
    BuiltinCommand::new(
        SEARCH_COMMAND,
        "Search this chat, this project's past sessions, or every project's",
        Node::with(SEARCH_TABS, SEARCH_WORDS),
    ),
    BuiltinCommand::new(
        "/changelog",
        "Show what's new in Caudra, or every release with `all`",
        Node::args(&[Arg::words("all", "", CHANGELOG_ALL)]),
    ),
    BuiltinCommand::new("/tools", "Show which tools the model can reach", LEAF),
    BuiltinCommand::new(
        "/skills",
        "Show the skills the model can load and where they come from",
        LEAF,
    ),
    BuiltinCommand::new("/queue", "Inspect and edit queued prompts", LEAF),
    BuiltinCommand::new(
        "/stash",
        "Park the current prompt draft for later, or restore or browse stashed ones",
        Node::with(STASH_SUBS, &[]),
    ),
    BuiltinCommand::new(
        "/update",
        "Install the release the update banner names, then restart",
        Node::with(UPDATE_SUBS, &[]),
    ),
    BuiltinCommand::new(
        "/memory",
        "Inspect project memory: the view, its summary tree, and the notes",
        LEAF,
    ),
    BuiltinCommand::new(
        "/learn",
        "Review what continual learning changed, revert it, or pause it",
        LEAF,
    ),
    BuiltinCommand::new(
        "/tasks",
        "Browse tasks and steer running subagents",
        Node::with(TASK_SUBS, &[]),
    ),
    BuiltinCommand::new("/shells", "Browse shell commands and stop running ones", LEAF),
    BuiltinCommand::new("/peers", "Browse live local messaging peers (experimental)", LEAF)
        .main_only()
        .requires(&[Feature::CrossSessionMessaging]),
    BuiltinCommand::new(
        "/messages",
        "Review held peer messages and manage this session's inbound policy",
        Node::with(MESSAGES_SUBS, &[]),
    )
    .main_only()
    .requires(&[Feature::CrossSessionMessaging]),
    BuiltinCommand::new(
        "/topics",
        "Browse peer topic messages, or change this session's topic subscriptions",
        Node::with(TOPICS_SUBS, &[]),
    )
    .main_only()
    .requires(&[Feature::CrossSessionMessaging]),
    BuiltinCommand::new(
        "/groups",
        "List consumer groups and this session's work, join or leave a group, or retry, pause, or cancel a work item",
        Node::with(GROUPS_SUBS, &[]),
    )
    .main_only()
    .requires(&[Feature::CrossSessionMessaging]),
    BuiltinCommand::new("/workflows", "Browse, trust, and launch workflows", LEAF)
        .main_only()
        .requires(&[Feature::Workflows]),
    BuiltinCommand::new(
        "/workflow",
        "Start a workflow, or list, pause, resume, and stop runs",
        Node::with(WORKFLOW_SUBS, WORKFLOW_LAUNCH),
    )
    .main_only()
    .requires(&[Feature::Workflows]),
    BuiltinCommand::new(
        "/deep-research",
        "Research a question with the deep-research workflow",
        Node::args(&[Arg::text("query", "The question to research", None).required()]),
    )
    .main_only()
    .requires(&[Feature::Workflows]),
    BuiltinCommand::new(
        "/review-changes",
        "Review a change from independent angles with the review-changes workflow",
        Node::args(&[Arg::text("scope", "What to review, such as a branch or the staged changes", None).required()]),
    )
    .main_only()
    .requires(&[Feature::Workflows]),
    BuiltinCommand::new(
        "/root-cause",
        "Diagnose a failure with the root-cause workflow",
        Node::args(&[Arg::text("failure", "The failure to diagnose", None).required()]),
    )
    .main_only()
    .requires(&[Feature::Workflows]),
    BuiltinCommand::new(
        "/automations",
        "Inspect, trust, and pause automations, or arm and disarm one",
        Node::with(AUTOMATIONS_SUBS, &[]),
    )
    .main_only()
    .requires(&[Feature::Automations]),
    BuiltinCommand::new(
        "/sessions",
        "Browse and switch sessions, or move them to another directory",
        Node::with(SESSIONS_SUBS, &[]),
    ),
    BuiltinCommand::new(
        "/rename",
        "Rename the current session",
        Node::args(&[Arg::text("title", "The new title", None).required()]),
    ),
    BuiltinCommand::new(
        "/fork",
        "Fork the session into a new one at its latest message",
        LEAF,
    )
    .main_only(),
    BuiltinCommand::new(
        "/model",
        "Switch the chat's model (a task's own in a task), assign job models, or check for a newer model list",
        Node::with(MODEL_SUBS, &[]),
    ),
    BuiltinCommand::new(
        "/accounts",
        "Choose which account of the model's provider requests go to",
        LEAF,
    )
    .main_only(),
    BuiltinCommand::new(
        "/system-prompt",
        "Inspect the system prompt and switch profile",
        LEAF,
    )
    .main_only(),
    BuiltinCommand::new(
        "/projection",
        "Inspect the conversation as the provider receives it",
        LEAF,
    )
    .main_only(),
    BuiltinCommand::new("/review", "Review the last reply passage by passage", LEAF),
    BuiltinCommand::new("/theme", "Switch color theme", LEAF),
    BuiltinCommand::new("/view", "Cycle transcript: auto / compact / expanded", LEAF),
    BuiltinCommand::new("/mcp", "Configure MCP servers", LEAF),
    BuiltinCommand::new(
        "/env",
        "View environment variables and choose which reach shell commands",
        LEAF,
    ),
    BuiltinCommand::new(
        "/secrets",
        "Save, replace, move and delete the secrets shell commands receive",
        LEAF,
    ),
    BuiltinCommand::new(
        "/settings",
        "Choose web search backends and pstack's model roles, and change plain settings in the global caudra.toml",
        LEAF,
    ),
    BuiltinCommand::new(
        "/permissions",
        "Inspect active conversation permission rules",
        Node::with(PERMISSIONS_SUBS, &[]),
    ),
    BuiltinCommand::new("/login", "Authenticate with an LLM provider", LEAF),
    BuiltinCommand::new("/cd", "Change working directory", Node::args(DIRECTORY)),
    BuiltinCommand::new(
        "/worktree",
        "Open, create, close or remove git worktrees of this repository",
        Node::with(WORKTREE_SUBS, &[]),
    )
    .main_only(),
    BuiltinCommand::new(
        "/btw",
        "Ask a side question, with follow-ups (read-only tools, nothing enters history)",
        Node::args(&[Arg::text("question", "Your side question", None).required()]),
    ),
    BuiltinCommand::new(
        "/catchup",
        "Bring yourself back up to speed on this session or the focused task (a /btw, nothing enters history)",
        Node::args(&[Arg::text("focus", "What to narrow the summary to", None)]),
    ),
    BuiltinCommand::new(
        "/extract",
        "List the requirements the session has gathered so far (Extract model, copyable, no history pollution)",
        LEAF,
    )
    .main_only(),
    BuiltinCommand::new(
        "/goal",
        "Work until a completion condition is met",
        Node::with(GOAL_SUBS, GOAL_CONDITION),
    )
    .main_only(),
    BuiltinCommand::new(
        "/autopilot",
        "Keep the session moving: answer prompts, questions, and plans for you",
        Node::with(AUTOPILOT_SUBS, AUTOPILOT_GOAL),
    )
    .main_only(),
    BuiltinCommand::new(
        "/auto",
        "Toggle Auto permissions (skip unmatched prompts, keep safeguards)",
        LEAF,
    ),
    BuiltinCommand::new(
        "/decisions",
        "Inspect decision engine status and recent decisions, or switch endpoints",
        Node::with(DECISIONS_SUBS, &[]),
    ),
    BuiltinCommand::new("/yolo", "Toggle YOLO mode (skip all permission prompts)", LEAF),
    BuiltinCommand::new(
        "/thinking",
        "Set reasoning (off, adaptive/provider default, effort, or token budget)",
        Node::args(THINKING_LEVEL),
    ),
    BuiltinCommand::new("/fast", "Toggle fast mode (models that sell a fast tier)", LEAF),
    BuiltinCommand::new("/exit", "Exit the application", LEAF),
    BuiltinCommand::new("/reload", "Reload plugins and config", LEAF),
    BuiltinCommand::new(
        "/workbench",
        "Open the file explorer, editor and source control view",
        LEAF,
    ),
    BuiltinCommand::new(
        "/remote",
        "Remote status, pending operations, reconnect, reconcile or explicit acknowledgement",
        Node::with(REMOTE_SUBS, &[]),
    )
    .main_only()
    .requires(&[Feature::RemoteWorkcell, Feature::Sandboxes]),
    BuiltinCommand::new(
        "/sandbox",
        "Manage live sandboxes, profiles, images and providers; explicit reviewed actions, Doctor and recovery",
        Node::with(SANDBOX_SUBS, &[]),
    )
    .requires(&[Feature::Sandboxes]),
];
