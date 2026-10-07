//! The bridge between a firing's thread and the runtime. The engine calls the host one request
//! at a time on the firing's thread; `admit` and `act` cross to the runtime's actor and block
//! until it answers, while `interrupted` and `now_ms` answer on the spot. The actor never waits
//! on a firing, so a firing whose actor is gone ends as shut down.

use std::any::Any;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, OnceLock};
use std::thread;

use caudra_automation::catalog::CatalogEntry;
use caudra_automation::engine::{
    ErrorKind, Firing, FiringEnd, FiringError, FiringLimits, FiringOutcome, StopKind, run_firing,
};
use caudra_automation::event::Event;
use caudra_automation::host::{
    ActionKind, ActionReply, ActionRequest, AutomationHost, CallSite, HostError, HostResult,
    Interruption,
};
use caudra_automation::limits::LimitRefusal;
use event_listener::Event as Notify;
use serde_json::{Map, Value};
use tracing::warn;

use super::clock::Clock;
use super::handle::Command;

pub(super) const THREAD_NAME: &str = "automation-firing";
/// Why `http()` refuses in a runtime without an HTTP client, and a request that is not a message
/// refuses on its way to the session's peers.
pub const NOT_IN_THIS_BUILD: &str = "is not available in this build";
/// Opens the error of a firing whose host side panicked.
pub const HOST_PANICKED: &str = "the automation host panicked";
const UNKNOWN_PANIC: &str = "unknown panic";

/// Why a running firing must stop. The first reason set wins. The firing reads it between
/// operations and before each request, and a request in flight off the actor awaits it.
#[derive(Default)]
pub(super) struct Stop {
    reason: OnceLock<Interruption>,
    raised: Notify,
}

/// What a firing thread runs: the armed script, its event, and the state and args it loaded.
pub(super) struct FiringJob {
    pub(super) source: Arc<str>,
    pub(super) entry: Arc<CatalogEntry>,
    pub(super) event: Event,
    pub(super) state: Value,
    pub(super) args: Arc<Map<String, Value>>,
}

pub(super) struct FiringHost {
    pub(super) fire_id: String,
    pub(super) commands: flume::Sender<Command>,
    pub(super) stop: Arc<Stop>,
    pub(super) clock: Arc<dyn Clock>,
    #[cfg(test)]
    pub(super) hold: Option<Hold>,
}

/// Holds every `log` request until the test releases it, so a firing stays running on cue.
#[cfg(test)]
#[derive(Clone)]
pub(super) struct Hold {
    pub(super) entered: flume::Sender<String>,
    pub(super) release: flume::Receiver<()>,
}

/// Runs the firing on its own thread and reports its outcome to the actor, even when the host
/// side panics, so the firing never holds its slot past its thread.
pub(super) fn spawn_firing(job: FiringJob, host: FiringHost) -> io::Result<()> {
    thread::Builder::new()
        .name(THREAD_NAME.to_owned())
        .spawn(move || {
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                run_firing(Firing {
                    source: &job.source,
                    meta: &job.entry.meta,
                    event: &job.event,
                    state: &job.state,
                    args: &job.args,
                    limits: &FiringLimits::default(),
                    host: &host,
                })
            }))
            .unwrap_or_else(|payload| {
                let detail = panic_detail(payload.as_ref());
                warn!(
                    fire_id = host.fire_id,
                    automation = job.entry.meta.name,
                    detail,
                    "automation firing panicked on the host side"
                );
                stopped_internally(format!("{HOST_PANICKED}: {detail}"))
            });
            let _ = host.commands.send(Command::Finished {
                fire_id: host.fire_id.clone(),
                outcome,
            });
        })
        .map(drop)
}

fn panic_detail(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or(UNKNOWN_PANIC)
}

/// A firing cut off where the engine could not report its own end: nothing commits, and
/// whether it was charged is unknown, so it reads as uncharged.
fn stopped_internally(message: String) -> FiringOutcome {
    FiringOutcome {
        end: FiringEnd::Stopped(FiringError {
            kind: ErrorKind::Stop(StopKind::Internal),
            message,
            line: None,
            column: None,
        }),
        state: None,
        operations: 0,
        charged: false,
        actions: 0,
        logs: 0,
    }
}

/// The refusal of a host function this runtime cannot perform, which stops the firing.
pub(super) fn not_in_this_build(kind: ActionKind) -> HostError {
    HostError::Refused(format!("{}() {NOT_IN_THIS_BUILD}", kind.as_str()))
}

impl Stop {
    pub(super) fn set(&self, reason: Interruption) {
        if self.reason.set(reason).is_ok() {
            self.raised.notify(usize::MAX);
        }
    }

    pub(super) fn get(&self) -> Option<Interruption> {
        self.reason.get().copied()
    }

    /// Completes with the reason once one is set. The second read closes the gap between the
    /// first and the listener.
    pub(super) async fn raised(&self) -> Interruption {
        loop {
            if let Some(reason) = self.get() {
                return reason;
            }
            let listener = self.raised.listen();
            if let Some(reason) = self.get() {
                return reason;
            }
            listener.await;
        }
    }
}

impl AutomationHost for FiringHost {
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    fn interrupted(&self) -> Option<Interruption> {
        self.stop.get()
    }

    /// Without an actor the firing is charged; its first request then ends it as shut down.
    fn admit(&self) -> Result<(), LimitRefusal> {
        let (reply, answer) = flume::bounded(1);
        let admit = Command::Admit {
            fire_id: self.fire_id.clone(),
            reply,
        };
        if self.commands.send(admit).is_err() {
            return Ok(());
        }
        answer.recv().unwrap_or(Ok(()))
    }

    fn act(&self, site: CallSite, request: ActionRequest) -> HostResult<ActionReply> {
        #[cfg(test)]
        self.hold(&request);
        let (reply, answer) = flume::bounded(1);
        self.commands
            .send(Command::Act {
                fire_id: self.fire_id.clone(),
                site,
                request,
                reply,
            })
            .map_err(|_| shut_down())?;
        answer.recv().unwrap_or_else(|_| Err(shut_down()))
    }
}

#[cfg(test)]
impl FiringHost {
    fn hold(&self, request: &ActionRequest) {
        if let (Some(hold), ActionRequest::Log { .. }) = (&self.hold, request) {
            let _ = hold.entered.send(self.fire_id.clone());
            let _ = hold.release.recv();
        }
    }
}

pub(super) fn shut_down() -> HostError {
    HostError::Interrupted(Interruption::Shutdown)
}
