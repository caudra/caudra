//! What the workbench asks the host to do.
//!
//! The crate owns no clipboard, no composer and no session, so anything that
//! reaches outside its own state leaves as one of these.

use std::ops::RangeInclusive;

use crate::fs::backend::WorkbenchPath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkbenchAction {
    /// The workbench handled the event.
    Consumed,
    /// The workbench wants nothing to do with the event. The host resumes its
    /// own dispatch, which is what keeps quit and suspend reachable.
    Passthrough,
    /// Hand the workbench back to the transcript.
    Close,
    /// Mention a file in the composer, then return to the transcript. The path
    /// is relative to the project root, and the host owns the spelling so the
    /// workbench never has to keep a copy of the composer's mention syntax.
    SendToComposer {
        path: WorkbenchPath,
        lines: Option<RangeInclusive<usize>>,
    },
    /// Put text on the system clipboard.
    Copy(String),
    /// Say something in the status bar.
    Flash(String),
}
