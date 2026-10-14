//! The `/autopilot` panel, with its Status and Log tabs, and the wording
//! every autopilot surface shares.

mod log;
mod panel;
mod status;
mod text;

pub(crate) use log::AutopilotLog;
#[cfg(test)]
pub(crate) use log::ENGAGEMENT_HEADER;
pub use panel::AutopilotPanel;
pub(crate) use panel::{
    CLOSE_LABEL, COPIED_ENTRY, COPY_LABEL, DOWN_LABEL, FILTER_LABEL, MODEL_LABEL_KEY, NO_ENTRIES,
    PanelAction, PanelTab, STOP_LABEL, UP_LABEL,
};
#[cfg(test)]
pub(crate) use panel::{MODEL_KEY, STOP_KEY};
pub(crate) use status::PanelContent;
#[cfg(test)]
pub(crate) use text::{
    ANSWERED_TAG, CANCELLED_TAG, DENIED_UNATTENDED_TAG, ENGAGED_STATE, ERROR_END, HUMAN_FIRST_TAG,
    STOPPED, SUPERSEDED_TAG, TAKEN_OVER_TAG, WAITING_STATE,
};
pub(crate) use text::{
    REASON_SEPARATOR, budget_left, card_note, decision_details, decision_title, end_notice,
    end_reason, goal_line, spend_summary, state_label,
};
