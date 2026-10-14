//! The `/goal` panel, with its Status and Log tabs, and the wording every
//! goal surface shares.

mod log;
mod panel;
mod status;
mod text;

pub(crate) use log::{GoalLog, NO_GOAL_LOG};
#[cfg(test)]
pub(crate) use panel::{FooterCommand, GOAL_CLEAR, GOAL_MODEL};
pub(crate) use panel::{GoalPanel, LOWER_LABEL, PanelAction, PanelCommand, RAISE_LABEL};
pub(crate) use status::StatusContent;
pub(crate) use text::{
    cleared_after_error_row, evaluation_failed_row, evaluation_row, finished_row, not_met_row,
};
