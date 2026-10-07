//! Automation support around the `caudra-automation` crate: discovering scripts and recording
//! trust in them, persisting one session's bindings, firings and actions, the authoring skill,
//! and the session runtime. [`manager::AutomationRuntime`] arms scripts, routes session signals
//! to them, runs each firing on its own thread through the [`host`] bridge, performs `http()`
//! off the actor through the [`http::HttpClient`] it is given, takes part in cross-session
//! messaging through the session's [`messaging::Messaging`], starts workflow runs and fires
//! `workflow_finished` through the session's [`workflows::Workflows`], and journals everything it
//! does;
//! [`handle::AutomationHandle`] is its cloneable face, found by session id
//! through [`handle::AutomationHandle::lookup`], and claims [`outbox`] deliveries without waiting
//! on the runtime. [`busy`] derives a busy period's idle report for the frontend, and
//! [`frontend`] holds what every frontend tells its runtime. The runtime refuses to start while
//! `experimental.automations` is off.

pub mod busy;
pub mod catalog;
pub mod clock;
mod dry_run;
pub mod frontend;
pub mod handle;
pub mod host;
pub mod http;
pub mod manager;
pub mod messaging;
pub mod outbox;
mod registry;
pub mod restore;
pub mod skill;
pub mod store;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
mod work_finished;
pub mod workflows;
