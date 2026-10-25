//! A session reopened after Caudra stopped in the middle of work: the notice
//! that says what was cut off, and the resume that picks the main turn and the
//! tasks it started back up, at once or on `/continue`.

use std::sync::Arc;

use caudra_agent::CancelToken;
use caudra_agent::automation::clock::{Clock, SystemClock};
use caudra_agent::restart::{DanglingCall, InterruptedJob, Interruption, MainState, RestoredWork};
use caudra_automation::event::StartedBy;
use caudra_config::ResumeInterrupted;
use caudra_providers::{Message, RequestOptions};
use caudra_storage::id::CaudraId;
use caudra_storage::now_epoch;
use caudra_storage::sessions::{StoredOpenRun, StoredSubagent, StoredSubagentOutcome};
use tracing::{info, warn};

use super::App;
use super::session::render_subagent_displays;
use super::tasks::TaskOutcome;
use crate::chat::INTERRUPTED_TEXT;
use crate::components::{Action, DisplayMessage, DisplayRole};

pub(crate) const RESUMED_NOTICE: &str = "Resumed after interruption: ";
pub(crate) const INTERRUPTED_NOTICE: &str = "Interrupted when Caudra stopped: ";
pub(crate) const ASK_HINT: &str = "/continue resumes it";

impl App {
    /// Takes what the reopened session's runtime found cut off. `configured`
    /// and `max_age_hours` are `[agent] resume_interrupted` and
    /// `resume_max_age_hours`.
    pub(crate) fn restore_interruption(
        &mut self,
        work: RestoredWork,
        dangling: Vec<DanglingCall>,
        configured: ResumeInterrupted,
        max_age_hours: u64,
    ) {
        self.interrupted = None;
        self.resume_due = false;
        self.resume_preamble = None;
        self.restore_interrupted_chats(&work);
        let open_run = self.state.session.meta.open_run.as_ref();
        let Some(interruption) =
            Interruption::new(work, dangling, open_run, self.state.session.updated_at)
        else {
            self.state.session_mut().meta.open_run = None;
            return;
        };
        let choice = interruption.choice(
            configured,
            max_age_hours,
            now_epoch(),
            self.automatic_wakes_suppressed,
        );
        info!(
            session_id = %self.state.session.id,
            main = ?interruption.main,
            cause = ?interruption.cause,
            tasks = interruption.work.resumable().count(),
            shells = interruption.work.shells.len(),
            attempts = interruption.attempts,
            ?configured,
            ?choice,
            "session reopened with interrupted work"
        );
        let wakes_main = interruption.main == MainState::AwaitingBackground;
        if choice == ResumeInterrupted::Off || !(interruption.resumable() || wakes_main) {
            self.state.session_mut().meta.open_run = None;
            if let Some(nudge) = interruption.main_nudge(false) {
                self.resume_preamble = Some(Message::synthetic(nudge));
            }
            self.notice(format!("{INTERRUPTED_NOTICE}{}", interruption.summary()));
            return;
        }
        if choice == ResumeInterrupted::Ask {
            self.notice(format!(
                "{INTERRUPTED_NOTICE}{} ({ASK_HINT})",
                interruption.summary()
            ));
        }
        self.resume_due = choice == ResumeInterrupted::Auto;
        self.interrupted = Some(interruption);
    }

    pub(crate) fn interrupted_resume_due(&self) -> bool {
        self.resume_due && self.interrupted.is_some()
    }

    /// Restarts the tasks the main chat started, then the main turn itself
    /// when one was running. A main chat that was only waiting gets its
    /// reminder with the first report a restarted task sends.
    pub(crate) fn resume_interrupted(&mut self) -> Vec<Action> {
        self.resume_due = false;
        let Some(interruption) = self.interrupted.take() else {
            return Vec::new();
        };
        let jobs: Vec<InterruptedJob> = interruption.work.resumable().cloned().collect();
        for job in &jobs {
            self.restart_task(job, interruption.task_nudge(job));
        }
        self.notice(format!("{RESUMED_NOTICE}{}", interruption.summary()));
        let nudge = interruption.main_nudge(true).map(Message::synthetic);
        match interruption.main {
            MainState::Working => {
                let mut input = self.continuation_input();
                input.resume = true;
                input.preamble.extend(nudge);
                self.start_run(input, String::new(), StartedBy::Resume)
            }
            MainState::AwaitingBackground if !jobs.is_empty() => {
                self.state.session_mut().meta.open_run = None;
                self.resume_preamble = nudge;
                Vec::new()
            }
            MainState::AwaitingBackground | MainState::Idle => {
                self.state.session_mut().meta.open_run = None;
                match nudge {
                    Some(nudge) => self.start_mailbox_run(vec![nudge], StartedBy::Resume),
                    None => Vec::new(),
                }
            }
        }
    }

    /// Records a main run as open, so a session that reopens with it still
    /// set knows the run was cut off.
    pub(super) fn open_run(&mut self, resumed: bool) {
        let run = StoredOpenRun::start(
            self.state.session.meta.open_run.as_ref(),
            resumed,
            SystemClock::new().now_ms(),
        );
        self.state.session_mut().meta.open_run = Some(run);
        if !resumed {
            self.interrupted = None;
            self.resume_due = false;
        }
    }

    /// A queued prompt the agent loop picked up starts its run without
    /// `start_run`, so it opens the run here. One it guided into a run already
    /// open changes nothing.
    pub(super) fn open_queued_run(&mut self) {
        if self.state.session.meta.open_run.is_none() {
            self.open_run(false);
        }
    }

    /// A run that ended on its own leaves nothing to resume. One a shutdown
    /// cut off stays open for the next start.
    pub(super) fn settle_open_run(&mut self) {
        if self.state.session.meta.open_run.is_some() {
            let meta = &mut self.state.session_mut().meta;
            meta.open_run = StoredOpenRun::settle(meta.open_run.take());
        }
    }

    /// Whether a shutdown is cutting the open run off, so its cancel is no stop
    /// the user asked for.
    pub(super) fn shutting_down(&self) -> bool {
        self.state
            .session
            .meta
            .open_run
            .as_ref()
            .is_some_and(|run| run.shut_down_at.is_some())
    }

    pub(super) fn mark_open_run_shut_down(&mut self) {
        if let Some(run) = &mut self.state.session_mut().meta.open_run {
            run.shut_down_at = Some(SystemClock::new().now_ms());
        }
    }

    /// Shows each interrupted agent task's saved transcript as a chat of its
    /// own, which a resume then carries on in place.
    fn restore_interrupted_chats(&mut self, work: &RestoredWork) {
        let restorable: Vec<_> = work
            .jobs
            .iter()
            .filter(|job| !self.chat_index.contains_key(&job.task_id))
            .filter_map(|job| {
                let transcript = work.transcripts.get(&job.task_id)?;
                let subagent = StoredSubagent {
                    tool_use_id: job.task_id.clone(),
                    parent_tool_use_id: Some(job.call_id.clone()),
                    root_tool_use_id: Some(job.call_id.clone()),
                    name: job.label.clone(),
                    model: None,
                    thinking: None,
                    fast: false,
                    outcome: StoredSubagentOutcome::Unknown,
                };
                Some((subagent, Arc::new(crate::history_items(transcript))))
            })
            .collect();
        if restorable.is_empty() {
            return;
        }
        let rendered = render_subagent_displays(
            &restorable,
            &[],
            self.state.session.tool_outputs(),
            &self.ui_config.tool_output_lines,
            self.ui_config.show_reminders,
        );
        for ((subagent, _), (display, items)) in restorable.into_iter().zip(rendered) {
            self.push_restored_subagent(
                subagent,
                display,
                items,
                (TaskOutcome::Unknown, INTERRUPTED_TEXT),
            );
        }
        self.sync_subagents();
    }

    fn notice(&mut self, text: String) {
        self.main_chat()
            .push(DisplayMessage::new(DisplayRole::Notice, text));
    }

    fn restart_task(&mut self, job: &InterruptedJob, prompt: String) {
        let (Some(runtime), Some(host), Some(events)) = (
            self.background.clone(),
            self.task_host.clone(),
            self.restore_event_tx.clone(),
        ) else {
            warn!(task_id = %job.task_id, "interrupted task not resumed: background tasks are unavailable");
            return;
        };
        let plan = self.state.plan.target();
        let workflow = self.workflow.runtime_handle();
        let opts = RequestOptions {
            thinking: self.state.thinking.clone(),
            fast: self.state.fast,
            ..RequestOptions::default()
        };
        let job = job.clone();
        smol::spawn(async move {
            let call_id = CaudraId::generate().to_string();
            let (_trigger, cancel) = CancelToken::new();
            let resumed = async {
                let mut ctx =
                    crate::agent::task_context(&host, workflow.as_ref(), cancel, events, &call_id)
                        .await?;
                ctx.plan = plan;
                ctx.opts = opts;
                ctx.jobs = Some(runtime.main_scope());
                runtime
                    .resume_interrupted(&ctx, &job, call_id.clone(), prompt)
                    .await
            };
            match resumed.await {
                Ok(card) => info!(task_id = %card.task_id, invocation_id = %card.invocation_id, "interrupted task resumed"),
                Err(error) => warn!(task_id = %job.task_id, %error, "interrupted task did not resume"),
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use caudra_agent::restart::{InterruptedJob, RestoredWork, StopCause};
    use caudra_agent::{AgentEvent, DoneReason, QueueConsumedItem, QueueItemId};
    use caudra_config::ResumeInterrupted;
    use caudra_providers::TokenUsage;
    use caudra_storage::background::{JobKind, JobOwner};
    use caudra_storage::now_epoch;
    use caudra_storage::sessions::{StoredOpenRun, StoredQueuedPrompt};
    use test_case::test_case;

    use super::super::App;
    use super::super::tests::{agent_msg, streaming_app, test_app};
    use crate::components::Action;

    const MAX_AGE_HOURS: u64 = 24;
    const RESUME_HEADING: &str = "Resumed after interruption";
    const SHELL_TASK: &str = "shell-1";
    const QUEUED_PROMPT: &str = "queued before the restart";
    const QUEUED_RUN_OPENS: &str = "a run the queue starts must be open, so a crash resumes it";
    const HELD_LEAVES_RESUME: &str = "held prompts must not hold back the resume";

    fn reopened(open_run: Option<u32>, work: RestoredWork, configured: ResumeInterrupted) -> App {
        let mut app = test_app();
        let session = app.state.session_mut();
        session.updated_at = now_epoch();
        session.meta.open_run = open_run.map(|resume_attempts| StoredOpenRun {
            started_at: 0,
            resume_attempts,
            shut_down_at: None,
        });
        app.restore_interruption(work, Vec::new(), configured, MAX_AGE_HOURS);
        app
    }

    fn shell_job() -> InterruptedJob {
        InterruptedJob {
            task_id: SHELL_TASK.into(),
            invocation_id: SHELL_TASK.into(),
            call_id: SHELL_TASK.into(),
            kind: JobKind::Shell,
            owner: JobOwner::Main,
            background: true,
            label: SHELL_TASK.into(),
            cause: StopCause::Crash,
            restarts: 0,
            stopped_at: now_epoch(),
        }
    }

    fn reminded_run(actions: &[Action]) -> bool {
        actions.iter().any(|action| match action {
            Action::SendMessage(input) => input.preamble.iter().any(|message| {
                message
                    .first_text_content()
                    .is_some_and(|text| text.contains(RESUME_HEADING))
            }),
            _ => false,
        })
    }

    #[test_case(ResumeInterrupted::Auto, 0 => true; "auto_resumes")]
    #[test_case(ResumeInterrupted::Auto, 1 => false; "a_crashed_resume_asks")]
    #[test_case(ResumeInterrupted::Ask, 0 => false; "ask_waits")]
    #[test_case(ResumeInterrupted::Off, 0 => false; "off_does_nothing")]
    fn a_cut_off_turn_resumes_on_its_own_only_under_auto(
        configured: ResumeInterrupted,
        attempts: u32,
    ) -> bool {
        reopened(Some(attempts), RestoredWork::default(), configured).interrupted_resume_due()
    }

    #[test]
    fn the_resumed_turn_reads_the_reminder_and_counts_the_attempt() {
        let mut app = reopened(Some(0), RestoredWork::default(), ResumeInterrupted::Auto);
        let actions = app.resume_interrupted();
        assert!(reminded_run(&actions));
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, Action::SendMessage(input) if input.resume))
        );
        let attempts = app
            .state
            .session
            .meta
            .open_run
            .as_ref()
            .map(|run| run.resume_attempts);
        assert_eq!(attempts, Some(1));
        assert!(!app.interrupted_resume_due());
    }

    #[test]
    fn off_holds_the_reminder_for_the_next_turn() {
        let app = reopened(Some(0), RestoredWork::default(), ResumeInterrupted::Off);
        assert!(app.resume_preamble.is_some());
        assert!(app.state.session.meta.open_run.is_none());
    }

    #[test]
    fn waiting_on_nothing_resumable_wakes_the_main_chat() {
        let work = RestoredWork {
            jobs: vec![shell_job()],
            ..RestoredWork::default()
        };
        let mut app = reopened(None, work, ResumeInterrupted::Auto);
        assert!(app.interrupted_resume_due());
        assert!(reminded_run(&app.resume_interrupted()));
    }

    #[test_case(true => false; "a_shutdown_cancel")]
    #[test_case(false => true; "a_user_cancel")]
    fn only_a_cancel_the_user_asked_for_holds_wakes(shutting_down: bool) -> bool {
        let mut app = streaming_app();
        app.open_run(false);
        if shutting_down {
            app.prepare_shutdown();
        }
        app.update(agent_msg(AgentEvent::Done {
            usage: TokenUsage::default(),
            num_turns: 1,
            reason: DoneReason::Cancelled,
        }));
        app.automatic_wakes_suppressed
    }

    #[test_case(false; "one_item")]
    #[test_case(true; "a_batch")]
    fn a_run_the_queue_starts_is_open(batch: bool) {
        let mut app = test_app();
        app.run_id = 1;
        let id = QueueItemId::new();
        let event = if batch {
            AgentEvent::QueueBatchConsumed {
                items: vec![QueueConsumedItem {
                    id,
                    text: QUEUED_PROMPT.into(),
                    images: Vec::new(),
                }],
            }
        } else {
            AgentEvent::QueueItemConsumed {
                id,
                text: QUEUED_PROMPT.into(),
                images: Vec::new(),
            }
        };

        app.update(agent_msg(event));

        assert!(
            app.state.session.meta.open_run.is_some(),
            "{QUEUED_RUN_OPENS}"
        );
    }

    #[test]
    fn held_prompts_leave_the_resume_due() {
        let mut app = test_app();
        app.state.session_mut().meta.queued_messages = vec![StoredQueuedPrompt {
            text: QUEUED_PROMPT.into(),
            mode: None,
            images: Vec::new(),
            paste_ranges: Vec::new(),
        }];
        app.restore_resumed_session();
        let session = app.state.session_mut();
        session.updated_at = now_epoch();
        session.meta.open_run = Some(StoredOpenRun {
            started_at: 0,
            resume_attempts: 0,
            shut_down_at: None,
        });
        app.restore_interruption(
            RestoredWork::default(),
            Vec::new(),
            ResumeInterrupted::Auto,
            MAX_AGE_HOURS,
        );

        assert_eq!(app.queue.held_prompts().count(), 1);
        assert!(app.queue.is_empty());
        assert!(app.interrupted_resume_due(), "{HELD_LEAVES_RESUME}");
    }
}
