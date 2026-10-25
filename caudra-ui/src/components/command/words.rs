//! The keywords built-in commands take, shared by their grammar and the
//! handlers that parse them, so the palette never offers a word a handler
//! refuses.

pub(crate) const ALL_ARG: &str = "all";
pub(crate) const CLEAR_ARG: &str = "clear";
/// Other words `/goal` reads as `clear`.
pub(crate) const GOAL_CLEAR_ALIASES: &[&str] = &["stop", "off", "reset", "none", "cancel"];
pub(crate) const MODEL_ARG: &str = "model";
pub(crate) const LOG_ARG: &str = "log";
pub(crate) const OFF_ARG: &str = "off";
pub(crate) const STOP_ARG: &str = "stop";
pub(crate) const ADAPTIVE_ARG: &str = "adaptive";
pub(crate) const POP_ARG: &str = "pop";
pub(crate) const LIST_ARG: &str = "list";
pub(crate) const DISMISS_ARG: &str = "dismiss";
pub(crate) const NEXT_ARG: &str = "next";
pub(crate) const MOVE_ARG: &str = "move";
pub(crate) const MIGRATE_ARG: &str = "migrate";
pub(crate) const CHAT_ARG: &str = "chat";
pub(crate) const PROJECT_ARG: &str = "project";
pub(crate) const DISCOVER_ARG: &str = "discover";
pub(crate) const SUGGESTED_ARG: &str = "suggested";
pub(crate) const STATUS_ARG: &str = "status";
pub(crate) const CANCEL_ARG: &str = "cancel";
pub(crate) const BACKGROUND_ARG: &str = "background";
pub(crate) const RUNS_ARG: &str = "runs";
pub(crate) const PAUSE_ARG: &str = "pause";
pub(crate) const RESUME_ARG: &str = "resume";
pub(crate) const ARM_ARG: &str = "arm";
pub(crate) const DISARM_ARG: &str = "disarm";
pub(crate) const NEW_ARG: &str = "new";
pub(crate) const CLOSE_ARG: &str = "close";
pub(crate) const REMOVE_ARG: &str = "remove";
pub(crate) const INSTANCES_ARG: &str = "instances";
pub(crate) const PROFILES_ARG: &str = "profiles";
pub(crate) const IMAGES_ARG: &str = "images";
pub(crate) const PROVIDERS_ARG: &str = "providers";
pub(crate) const DOCTOR_ARG: &str = "doctor";
pub(crate) const RECONCILE_NETWORK_ARG: &str = "reconcile-network";
pub(crate) const HELP_ARG: &str = "help";
pub(crate) const APPROVE_ARG: &str = "approve";
pub(crate) const REJECT_ARG: &str = "reject";
pub(crate) const INBOUND_ARG: &str = "inbound";
pub(crate) const SUBSCRIBE_ARG: &str = "subscribe";
pub(crate) const UNSUBSCRIBE_ARG: &str = "unsubscribe";
pub(crate) const BROADCAST_ARG: &str = "broadcast";
pub(crate) const ON_ARG: &str = "on";
pub(crate) const JOIN_ARG: &str = "join";
pub(crate) const LEAVE_ARG: &str = "leave";
pub(crate) const RETRY_ARG: &str = "retry";
pub(crate) const AUTO_ARG: &str = "auto";
pub(crate) const ACCEPT_ARG: &str = "accept";
pub(crate) const HOLD_ARG: &str = "hold";
pub(crate) const REFUSE_ARG: &str = "refuse";
