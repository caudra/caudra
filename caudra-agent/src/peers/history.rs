//! The process's writer for the shared message history. One thread owns the
//! connection, so peer state never waits on the database and every report
//! reaches it in the order it was made.

#[cfg(any(test, feature = "test-support"))]
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use caudra_config::MessagingConfig;
use caudra_storage::StateDir;
use caudra_storage::messages::{
    HistoryChannel, MessageLog, MessageLogError, MessageRecipient, NewMessage, Retention,
    StoredMessage, TopicSummary,
};
use flume::{Receiver, Sender};

use super::topics::pattern_matches;
use super::wall_ms;

const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const WRITER_THREAD: &str = "caudra-message-history";
const UNAVAILABLE: &str = "Message history is unavailable";
const STOPPED: &str = "The message history writer stopped";

type Job = Box<dyn FnOnce(&mut MessageLog) + Send>;

/// Queues work for the writer thread. Cheap to clone; every clone feeds the
/// same thread.
#[derive(Clone)]
pub(super) struct MessageHistory {
    jobs: Sender<Job>,
}

/// Waits for queued reports when dropped, so they outlive the process's last
/// session. Must be dropped after every [`MessageHistory`] clone.
pub(super) struct HistoryWriter(Option<JoinHandle<()>>);

impl Drop for HistoryWriter {
    fn drop(&mut self) {
        if let Some(thread) = self.0.take()
            && thread.join().is_err()
        {
            tracing::warn!("message history writer panicked");
        }
    }
}

impl MessageHistory {
    /// The history every session of this user shares.
    pub(super) fn open_shared(
        messaging: &MessagingConfig,
    ) -> Result<(Self, HistoryWriter), String> {
        let state_dir = StateDir::resolve().map_err(|error| format!("{UNAVAILABLE}: {error}"))?;
        Self::open(&state_dir, messaging)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn open_in(
        directory: &Path,
        messaging: &MessagingConfig,
    ) -> Result<(Self, HistoryWriter), String> {
        Self::open(&StateDir::from_path(directory.to_owned()), messaging)
    }

    /// A history whose writer is gone, so every query fails.
    #[cfg(test)]
    pub(super) fn stopped() -> (Self, HistoryWriter) {
        let (jobs, _) = flume::unbounded();
        (Self { jobs }, HistoryWriter(None))
    }

    fn open(
        state_dir: &StateDir,
        messaging: &MessagingConfig,
    ) -> Result<(Self, HistoryWriter), String> {
        let retention = Retention {
            days: messaging.history_days,
            max_messages: messaging.history_max_messages,
        };
        let log = MessageLog::open(state_dir, &retention, wall_ms())
            .map_err(|error| format!("{UNAVAILABLE}: {error}"))?;
        let (jobs, queue) = flume::unbounded();
        let thread = thread::Builder::new()
            .name(WRITER_THREAD.into())
            .spawn(move || write(log, &queue, &retention))
            .map_err(|error| format!("{UNAVAILABLE}: {error}"))?;
        Ok((Self { jobs }, HistoryWriter(Some(thread))))
    }

    /// Runs `job` after everything queued before it and returns its result.
    async fn query<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut MessageLog) -> Result<T, MessageLogError> + Send + 'static,
    ) -> Result<T, String> {
        let (reply, result) = flume::bounded(1);
        self.jobs
            .send(Box::new(move |log| {
                let _ = reply.send(job(log).map_err(|error| format!("{UNAVAILABLE}: {error}")));
            }))
            .map_err(|_| STOPPED.to_owned())?;
        result.recv_async().await.map_err(|_| STOPPED.to_owned())?
    }

    /// Queues `job` without waiting for it. Reports describe what already
    /// happened, so a failure is logged rather than undoing anything.
    fn report(
        &self,
        job: impl FnOnce(&mut MessageLog) -> Result<(), MessageLogError> + Send + 'static,
    ) {
        let job: Job = Box::new(move |log| {
            if let Err(error) = job(log) {
                tracing::warn!(%error, "message history update failed");
            }
        });
        if self.jobs.send(job).is_err() {
            tracing::warn!(STOPPED);
        }
    }

    pub(super) async fn record(
        &self,
        message: NewMessage,
        recipients: Vec<MessageRecipient>,
    ) -> Result<i64, String> {
        self.query(move |log| log.record(&message, &recipients))
            .await
    }

    pub(super) fn receipt(
        &self,
        sender_route: String,
        message_id: String,
        recipient_session: String,
        status: String,
        reason: Option<String>,
    ) {
        self.report(move |log| {
            log.record_receipt(
                &sender_route,
                &message_id,
                &recipient_session,
                &status,
                reason.as_deref(),
                wall_ms(),
            )
        });
    }

    pub(super) fn transition(
        &self,
        sender_route: String,
        message_id: String,
        recipient: MessageRecipient,
        status: &'static str,
        reason: Option<String>,
    ) {
        self.report(move |log| {
            log.transition(
                &sender_route,
                &message_id,
                &recipient,
                status,
                reason.as_deref(),
                wall_ms(),
            )
        });
    }

    pub(super) fn seen(&self, session: String, sender_route: String, message_id: String) {
        self.report(move |log| log.mark_seen(&session, &sender_route, &message_id));
    }

    pub(super) async fn unseen(
        &self,
        session: String,
        patterns: Vec<String>,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, String> {
        self.query(move |log| {
            log.unseen(
                &session,
                |topic| {
                    patterns
                        .iter()
                        .any(|pattern| pattern_matches(pattern, topic))
                },
                limit,
            )
        })
        .await
    }

    pub(super) async fn directory(&self) -> Result<Vec<TopicSummary>, String> {
        self.query(|log| log.directory()).await
    }

    /// Messages on every stored topic `pattern` matches, or on broadcasts
    /// without one, newest first.
    pub(super) async fn history(
        &self,
        pattern: Option<String>,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, String> {
        self.query(move |log| {
            let channel = match pattern {
                Some(pattern) => HistoryChannel::Topics(
                    log.directory()?
                        .into_iter()
                        .map(|summary| summary.topic)
                        .filter(|topic| pattern_matches(&pattern, topic))
                        .collect(),
                ),
                None => HistoryChannel::Broadcast,
            };
            log.history(&channel, before, limit)
        })
        .await
    }
}

fn write(mut log: MessageLog, queue: &Receiver<Job>, retention: &Retention) {
    let mut pruned = Instant::now();
    for job in queue.iter() {
        job(&mut log);
        if pruned.elapsed() >= PRUNE_INTERVAL {
            pruned = Instant::now();
            if let Err(error) = log.prune(retention, wall_ms()) {
                tracing::warn!(%error, "message history pruning failed");
            }
        }
    }
}
