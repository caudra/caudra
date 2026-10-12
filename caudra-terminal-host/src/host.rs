use std::fmt::Debug;
use std::sync::Arc;

use crate::skill::SkillSource;
use crate::status::{PaneMetadata, PaneStatus};
use crate::workspaces::Workspaces;

/// A terminal application Caudra runs inside and cooperates with, such as
/// Herdr or Orca.
pub trait TerminalHost: Debug + Send + Sync {
    fn name(&self) -> &'static str;
    /// How the pane status reaches the host, if it can at all.
    fn status_channel(&self) -> Option<StatusChannel>;
    fn workspaces(&self) -> Option<Arc<dyn Workspaces>>;
    fn skill(&self) -> Option<SkillSource>;
    /// The host raises its own notices for finished turns and waiting
    /// prompts, so Caudra does not alert for them as well.
    fn raises_notifications(&self) -> bool;
    /// The host names the pane's tab after the session from the metadata its
    /// status channel receives, so Caudra leaves the window title alone.
    fn names_tabs(&self) -> bool;
}

pub enum StatusChannel {
    /// Blocking calls, made on a thread of their own.
    Blocking(Box<dyn BlockingStatusSink>),
    /// Escape sequences in the pane's own output, written where the UI draws
    /// so they never land inside one of its sequences.
    InBand(Box<dyn InBandStatusEncoder>),
}

pub trait BlockingStatusSink: Send {
    fn report(&mut self, status: &PaneStatus);
    fn describe(&mut self, metadata: &PaneMetadata<'_>);
    /// The user exited, so the host forgets the agent.
    fn release(&mut self);
}

pub trait InBandStatusEncoder: Send {
    fn encode(&self, status: &PaneStatus, metadata: Option<&PaneMetadata<'_>>) -> String;
    /// What tells the host the user exited, if anything does.
    fn release(&self) -> Option<String>;
    /// Whether a multiplexer has to pass the sequence through, rather than
    /// understand it itself.
    fn wraps_for_mux(&self) -> bool;
}
