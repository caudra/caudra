//! Delivers the pane status to whoever receives it: a blocking sink on a
//! thread of its own, or escape sequences written on the caller's thread.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::warn;

use crate::alert::write_to_terminal;
use crate::host::{BlockingStatusSink, InBandStatusEncoder, StatusChannel, TerminalHost};
use crate::mux::TerminalMux;
use crate::status::{PaneMetadata, PaneStatus};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const THREAD_NAME: &str = "pane-status-reporter";

/// Writes an escape sequence to the terminal.
pub type Writer = Arc<dyn Fn(&str) -> io::Result<()> + Send + Sync>;

pub fn terminal_writer() -> Writer {
    Arc::new(write_to_terminal)
}

/// Only the newest value is ever sent, and an unchanged one is never sent twice.
struct Latest<T> {
    value: Option<T>,
    unsent: bool,
}

impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self {
            value: None,
            unsent: false,
        }
    }
}

impl<T: Clone + PartialEq> Latest<T> {
    fn set(&mut self, value: T) -> bool {
        let changed = self.value.as_ref() != Some(&value);
        if changed {
            self.value = Some(value);
            self.unsent = true;
        }
        changed
    }

    fn take(&mut self) -> Option<T> {
        std::mem::take(&mut self.unsent)
            .then(|| self.value.clone())
            .flatten()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Farewell {
    /// The user exited: the host forgets the agent and how to resume it.
    Release,
    /// The process is going away for any other reason. The host keeps the
    /// resume command, and notices on its own when the shell gets the pane
    /// back.
    Detach,
}

#[derive(Default)]
struct Pending {
    status: Latest<PaneStatus>,
    metadata: Latest<PaneMetadata<'static>>,
    farewell: Option<Farewell>,
}

enum WorkerAction {
    Report(PaneStatus),
    Describe(PaneMetadata<'static>),
    Release,
    Stop,
}

struct Shared {
    pending: Mutex<Pending>,
    wake: flume::Sender<()>,
}

impl Shared {
    fn update(&self, change: impl FnOnce(&mut Pending) -> bool) {
        let changed = {
            let mut pending = lock(&self.pending);
            pending.farewell.is_none() && change(&mut pending)
        };
        if changed {
            let _ = self.wake.try_send(());
        }
    }

    fn observe(&self, status: PaneStatus) {
        self.update(|pending| pending.status.set(status));
    }

    fn describe(&self, metadata: PaneMetadata<'_>) {
        self.update(|pending| {
            pending.metadata.value.as_ref() != Some(&metadata)
                && pending.metadata.set(metadata.into_owned())
        });
    }

    fn close(&self, farewell: Farewell) {
        self.update(|pending| {
            pending.farewell = Some(farewell);
            true
        });
    }

    fn next_action(&self) -> Option<WorkerAction> {
        let mut pending = lock(&self.pending);
        match pending.farewell {
            Some(Farewell::Release) => Some(WorkerAction::Release),
            Some(Farewell::Detach) => Some(WorkerAction::Stop),
            None => pending
                .status
                .take()
                .map(WorkerAction::Report)
                .or_else(|| pending.metadata.take().map(WorkerAction::Describe)),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

struct InBand {
    encoder: Box<dyn InBandStatusEncoder>,
    writer: Writer,
    mux: TerminalMux,
    status: Option<PaneStatus>,
    metadata: Option<PaneMetadata<'static>>,
    written: Option<String>,
    /// Closed on a farewell, or after a write failed.
    closed: bool,
}

impl InBand {
    fn flush(&mut self) {
        let Some(status) = &self.status else {
            return;
        };
        let sequence = self.encoder.encode(status, self.metadata.as_ref());
        if self.written.as_ref() != Some(&sequence) && self.write(&sequence) {
            self.written = Some(sequence);
        }
    }

    fn write(&mut self, sequence: &str) -> bool {
        let wrapped = if self.encoder.wraps_for_mux() {
            self.mux.wrap(sequence.to_owned())
        } else {
            sequence.to_owned()
        };
        match (self.writer)(&wrapped) {
            Ok(()) => true,
            Err(error) => {
                warn!(%error, "pane status reporting disabled after a write failure");
                self.closed = true;
                false
            }
        }
    }

    fn close(&mut self, farewell: Farewell) {
        if std::mem::replace(&mut self.closed, true) || farewell == Farewell::Detach {
            return;
        }
        if let Some(sequence) = self.encoder.release() {
            self.write(&sequence);
        }
    }
}

enum Delivery {
    Blocking(Shared),
    InBand(Mutex<InBand>),
}

/// Reports the pane status. Cloned wherever the status is known.
#[derive(Clone)]
pub struct ReporterHandle {
    delivery: Arc<Delivery>,
    raises_notifications: bool,
}

impl ReporterHandle {
    pub fn observe(&self, status: PaneStatus) {
        match &*self.delivery {
            Delivery::Blocking(shared) => shared.observe(status),
            Delivery::InBand(in_band) => {
                let mut in_band = lock(in_band);
                if !in_band.closed {
                    in_band.status = Some(status);
                    in_band.flush();
                }
            }
        }
    }

    pub fn describe(&self, metadata: PaneMetadata<'_>) {
        match &*self.delivery {
            Delivery::Blocking(shared) => shared.describe(metadata),
            Delivery::InBand(in_band) => {
                let mut in_band = lock(in_band);
                if !in_band.closed && in_band.metadata.as_ref() != Some(&metadata) {
                    in_band.metadata = Some(metadata.into_owned());
                    in_band.flush();
                }
            }
        }
    }

    /// The terminal was handed to another program, which may have changed
    /// what it shows, so the next status is written again.
    pub fn invalidate(&self) {
        if let Delivery::InBand(in_band) = &*self.delivery {
            lock(in_band).written = None;
        }
    }

    pub fn raises_notifications(&self) -> bool {
        self.raises_notifications
    }

    fn close(&self, farewell: Farewell) {
        match &*self.delivery {
            Delivery::Blocking(shared) => shared.close(farewell),
            Delivery::InBand(in_band) => lock(in_band).close(farewell),
        }
    }
}

pub struct Reporter {
    handle: ReporterHandle,
    worker: Option<(JoinHandle<()>, flume::Receiver<()>)>,
}

impl Reporter {
    /// Starts reporting to `host`, writing in-band sequences to the terminal.
    pub fn for_host(host: &dyn TerminalHost) -> Option<Self> {
        let channel = host.status_channel()?;
        Self::start(channel, host.raises_notifications(), terminal_writer()).map_or_else(
            |error| {
                warn!(%error, host = host.name(), "failed to start the pane status reporter");
                None
            },
            Some,
        )
    }

    pub fn start(
        channel: StatusChannel,
        raises_notifications: bool,
        writer: Writer,
    ) -> io::Result<Self> {
        let (delivery, worker) = match channel {
            StatusChannel::Blocking(sink) => {
                let (wake_tx, wake_rx) = flume::bounded(1);
                let (done_tx, done_rx) = flume::bounded(1);
                let delivery = Arc::new(Delivery::Blocking(Shared {
                    pending: Mutex::new(Pending::default()),
                    wake: wake_tx,
                }));
                let worker_delivery = Arc::clone(&delivery);
                let worker =
                    std::thread::Builder::new()
                        .name(THREAD_NAME.into())
                        .spawn(move || {
                            if let Delivery::Blocking(shared) = &*worker_delivery {
                                worker_loop(sink, shared, &wake_rx, &done_tx);
                            }
                        })?;
                (delivery, Some((worker, done_rx)))
            }
            StatusChannel::InBand(encoder) => (
                Arc::new(Delivery::InBand(Mutex::new(InBand {
                    encoder,
                    writer,
                    mux: TerminalMux::detect(),
                    status: None,
                    metadata: None,
                    written: None,
                    closed: false,
                }))),
                None,
            ),
        };
        Ok(Self {
            handle: ReporterHandle {
                delivery,
                raises_notifications,
            },
            worker,
        })
    }

    pub fn handle(&self) -> ReporterHandle {
        self.handle.clone()
    }

    /// Tells the host the agent left the pane. Only a deliberate exit may: a
    /// host may forget how to resume the session, which is what brings it
    /// back after a hangup or a host restart. Dropping the reporter without
    /// this keeps it.
    pub fn release(mut self) {
        self.handle.close(Farewell::Release);
        let Some((worker, done_rx)) = self.worker.take() else {
            return;
        };
        if done_rx.recv_timeout(SHUTDOWN_TIMEOUT).is_err() {
            warn!("pane status reporter did not stop within {SHUTDOWN_TIMEOUT:?}");
            return;
        }
        if worker.join().is_err() {
            warn!("pane status reporter thread panicked");
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        self.handle.close(Farewell::Detach);
    }
}

fn worker_loop(
    mut sink: Box<dyn BlockingStatusSink>,
    shared: &Shared,
    wake_rx: &flume::Receiver<()>,
    done_tx: &flume::Sender<()>,
) {
    while wake_rx.recv().is_ok() {
        while let Some(action) = shared.next_action() {
            match action {
                WorkerAction::Report(status) => sink.report(&status),
                WorkerAction::Describe(metadata) => sink.describe(&metadata),
                WorkerAction::Release => {
                    sink.release();
                    let _ = done_tx.send(());
                    return;
                }
                WorkerAction::Stop => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{BlockKind, PaneState};
    use test_case::test_case;

    const BLOCKER: &str = "Permission requested: shell";
    const TITLE: &str = "Refactor auth";
    const MODEL: &str = "claude-opus-4-5";
    const RELEASED: &str = "released";
    const SESSION: &str = "4Tx8ZrCGn1j9aE6oUvWm2k";

    fn shared() -> Shared {
        let (wake, _wake_rx) = flume::bounded(1);
        Shared {
            pending: Mutex::new(Pending::default()),
            wake,
        }
    }

    fn status(state: PaneState) -> PaneStatus {
        PaneStatus::new(state)
    }

    fn blocked() -> PaneStatus {
        PaneStatus {
            label: Some(BLOCKER.into()),
            ..status(PaneState::Blocked(Some(BlockKind::Permission)))
        }
    }

    fn metadata(context_percent: Option<u32>) -> PaneMetadata<'static> {
        PaneMetadata {
            title: TITLE.into(),
            model: MODEL.into(),
            context_percent,
        }
    }

    #[test]
    fn status_coalesces_to_the_latest_before_metadata() {
        let shared = shared();
        shared.describe(metadata(Some(1)));
        shared.observe(status(PaneState::Idle));
        shared.observe(status(PaneState::Working));
        shared.observe(blocked());

        assert!(matches!(
            shared.next_action(),
            Some(WorkerAction::Report(reported)) if reported == blocked()
        ));
        assert!(matches!(
            shared.next_action(),
            Some(WorkerAction::Describe(described)) if described == metadata(Some(1))
        ));
        assert!(shared.next_action().is_none());
    }

    #[test]
    fn unchanged_values_are_not_sent_twice() {
        let shared = shared();
        shared.observe(status(PaneState::Idle));
        shared.describe(metadata(None));
        while shared.next_action().is_some() {}

        shared.observe(status(PaneState::Idle));
        shared.describe(metadata(None));

        assert!(shared.next_action().is_none());
    }

    #[test]
    fn a_new_resume_target_is_a_change() {
        let shared = shared();
        shared.observe(status(PaneState::Idle));
        while shared.next_action().is_some() {}

        shared.observe(PaneStatus {
            resume: Some(vec![SESSION.into()]),
            ..status(PaneState::Idle)
        });

        assert!(matches!(
            shared.next_action(),
            Some(WorkerAction::Report(_))
        ));
    }

    #[test_case(Farewell::Release ; "release")]
    #[test_case(Farewell::Detach ; "detach")]
    fn the_first_farewell_wins_and_drops_pending_reports(first: Farewell) {
        let shared = shared();
        shared.observe(status(PaneState::Working));
        shared.close(first);
        shared.close(Farewell::Release);
        shared.close(Farewell::Detach);
        shared.observe(status(PaneState::Idle));

        let action = shared.next_action();

        match first {
            Farewell::Release => assert!(matches!(action, Some(WorkerAction::Release))),
            Farewell::Detach => assert!(matches!(action, Some(WorkerAction::Stop))),
        }
    }

    /// Encodes the state and the model, so a test sees what was written.
    struct Encoder;

    impl InBandStatusEncoder for Encoder {
        fn encode(&self, status: &PaneStatus, metadata: Option<&PaneMetadata<'_>>) -> String {
            format!(
                "{:?}/{}",
                status.state,
                metadata.map_or("", |metadata| &metadata.model)
            )
        }

        fn release(&self) -> Option<String> {
            Some(RELEASED.into())
        }

        fn wraps_for_mux(&self) -> bool {
            false
        }
    }

    fn in_band(fail: bool) -> (Reporter, Arc<Mutex<Vec<String>>>) {
        let written = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&written);
        let writer: Writer = Arc::new(move |sequence: &str| {
            if fail {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            lock(&sink).push(sequence.to_owned());
            Ok(())
        });
        let reporter =
            Reporter::start(StatusChannel::InBand(Box::new(Encoder)), true, writer).unwrap();
        (reporter, written)
    }

    fn written(log: &Mutex<Vec<String>>) -> Vec<String> {
        lock(log).clone()
    }

    #[test]
    fn in_band_writes_once_per_change_and_again_after_invalidation() {
        let (reporter, log) = in_band(false);
        let handle = reporter.handle();

        handle.describe(metadata(None));
        handle.observe(status(PaneState::Working));
        handle.observe(status(PaneState::Working));
        handle.describe(metadata(None));
        handle.invalidate();
        handle.observe(status(PaneState::Working));

        let working = format!("{:?}/{MODEL}", PaneState::Working);
        assert_eq!(written(&log), [working.clone(), working]);
    }

    #[test_case(true, &[RELEASED] ; "release_says_goodbye")]
    #[test_case(false, &[] ; "detach_writes_nothing")]
    fn in_band_farewells(release: bool, expected: &[&str]) {
        let (reporter, log) = in_band(false);
        let handle = reporter.handle();

        if release {
            reporter.release();
        } else {
            drop(reporter);
        }
        handle.observe(status(PaneState::Working));

        assert_eq!(written(&log), expected);
    }

    #[test]
    fn a_failed_write_stops_in_band_reporting() {
        let (reporter, log) = in_band(true);
        let handle = reporter.handle();

        handle.observe(status(PaneState::Working));
        reporter.release();

        assert!(written(&log).is_empty());
        assert!(matches!(&*handle.delivery, Delivery::InBand(in_band) if lock(in_band).closed));
    }
}
