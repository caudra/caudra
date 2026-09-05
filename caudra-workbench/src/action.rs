//! What the workbench asks the host to do.
//!
//! The crate owns no clipboard, no composer and no session, so anything that
//! reaches outside its own state leaves as one of these.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkbenchAction {
    /// The workbench handled the event.
    Consumed,
    /// The workbench wants nothing to do with the event. The host resumes its
    /// own dispatch, which is what keeps quit and suspend reachable.
    Passthrough,
    /// Hand the workbench back to the transcript.
    Close,
    /// Append text to the composer, then return to the transcript.
    SendToComposer(String),
    /// Put text on the system clipboard.
    Copy(String),
    /// Say something in the status bar.
    Flash(String),
}
