//! A session reopened after Caudra stopped in the middle of work: what the previous process left
//! unfinished, what each call that never settled says now, and the one-shot reminder a resumed
//! agent reads before it carries on.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use caudra_config::ResumeInterrupted;
use caudra_providers::{ContentBlock, HistoryItem, HistoryItemKind, Message, Role, expand_message};
use caudra_storage::background::{JobKind, JobOwner};
use caudra_storage::sessions::StoredOpenRun;
use caudra_storage::shell_history::ShellExecutionRecord;
use serde::Serialize;

const NUDGE_OPEN: &str = "<system-reminder>\n# Resumed after interruption\n";
const NUDGE_CLOSE: &str = "</system-reminder>";
const RECONCILE: &str = "Before you continue, reconcile: check what the interrupted work changed (files, git status, processes that may still run detached), and rerun only what is safe to repeat. Then carry on with the task.";
const CRASHED: &str = "Caudra stopped unexpectedly (a crash or a kill)";
const SHUT_DOWN: &str = "Caudra was shut down";
const WHILE_WORKING: &str = "while you were working";
const WHILE_WAITING: &str = "while you were waiting on background work";
const REOPENED: &str = "The session has been reopened.";
const INTERRUPTED_HEADING: &str = "Interrupted:";
pub const CALL_INTERRUPTED: &str = "Interrupted: Caudra stopped while this call ran. Its effects are unknown and it was not run again.";
const TASK_TOOL: &str = "task";
const MAIN_TURN: &str = "main turn";
const MAIN_CHAT: &str = "main chat";
const BACKGROUND_WORK: &str = "background work";
const ONE_TASK: &str = "1 task";
const SECS_PER_HOUR: u64 = 3600;

/// Why the previous process left work unfinished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCause {
    Crash,
    Shutdown,
}

/// What the main chat was doing when Caudra stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainState {
    /// A turn was running.
    Working,
    /// The turn had ended, but work it started was still going.
    AwaitingBackground,
    Idle,
}

/// One job the previous process left unfinished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedJob {
    pub task_id: String,
    pub invocation_id: String,
    /// The call that launched it, which for a foreground task is the call its caller still waits on.
    pub call_id: String,
    pub kind: JobKind,
    pub owner: JobOwner,
    pub background: bool,
    pub label: String,
    pub cause: StopCause,
    /// Restarts its run was already resumed across.
    pub restarts: u32,
    /// Unix seconds of its last saved progress.
    pub stopped_at: u64,
}

/// Everything a reopened session's background runtime found cut off.
#[derive(Debug, Clone, Default)]
pub struct RestoredWork {
    pub jobs: Vec<InterruptedJob>,
    /// Commands that were running in a shell call when the process went away.
    pub shells: Vec<ShellExecutionRecord>,
    /// The saved transcript of each interrupted agent task, by task_id, with
    /// its open calls answered.
    pub transcripts: HashMap<String, Arc<Vec<Message>>>,
    /// The calls each of those transcripts left open, by task_id.
    pub dangling: HashMap<String, Vec<DanglingCall>>,
}

impl RestoredWork {
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty() && self.shells.is_empty()
    }

    /// The agent tasks the main chat started, which a resume restarts.
    pub fn resumable(&self) -> impl Iterator<Item = &InterruptedJob> {
        self.owned_by(&JobOwner::Main)
            .filter(|job| job.kind == JobKind::Agent)
    }

    pub fn owned_by<'a>(&'a self, owner: &'a JobOwner) -> impl Iterator<Item = &'a InterruptedJob> {
        self.jobs.iter().filter(move |job| &job.owner == owner)
    }

    /// Shutdown only when every job says so: one a crash cut off means the
    /// process never finished stopping.
    pub fn cause(&self) -> StopCause {
        match self.jobs.iter().all(|job| job.cause == StopCause::Shutdown) && !self.jobs.is_empty()
        {
            true => StopCause::Shutdown,
            false => StopCause::Crash,
        }
    }

    fn shell(&self, call_id: &str) -> Option<&ShellExecutionRecord> {
        self.shells.iter().find(|shell| shell.call_id == call_id)
    }

    fn agent_task(&self, call_id: &str) -> Option<&InterruptedJob> {
        self.jobs
            .iter()
            .find(|job| job.kind == JobKind::Agent && job.call_id == call_id)
    }
}

/// What a reopened session found cut off, and what its main chat was doing
/// when Caudra stopped.
#[derive(Debug, Clone)]
pub struct Interruption {
    pub work: RestoredWork,
    /// The calls the main transcript left open, answered on restore.
    pub dangling: Vec<DanglingCall>,
    pub main: MainState,
    pub cause: StopCause,
    /// Unix seconds of the last progress the interrupted work saved.
    pub stopped_at: u64,
    /// Automatic resumes this work already died in.
    pub attempts: u32,
}

impl Interruption {
    /// `None` when nothing was cut off. `last_saved` is when the session was
    /// last written, in Unix seconds.
    pub fn new(
        work: RestoredWork,
        dangling: Vec<DanglingCall>,
        open_run: Option<&StoredOpenRun>,
        last_saved: u64,
    ) -> Option<Self> {
        let main = main_state(open_run.is_some() || !dangling.is_empty(), &work);
        if main == MainState::Idle && work.is_empty() {
            return None;
        }
        let cause = match open_run {
            Some(run) if run.shut_down_at.is_some() => StopCause::Shutdown,
            Some(_) => StopCause::Crash,
            None => work.cause(),
        };
        let stopped_at = work
            .jobs
            .iter()
            .map(|job| job.stopped_at)
            .chain((main == MainState::Working).then_some(last_saved))
            .max()
            .unwrap_or(last_saved);
        let attempts = work
            .resumable()
            .map(|job| job.restarts)
            .chain(open_run.map(|run| run.resume_attempts))
            .max()
            .unwrap_or_default();
        Some(Self {
            work,
            dangling,
            main,
            cause,
            stopped_at,
            attempts,
        })
    }

    /// What `configured` comes to for this work: `auto` asks instead once a
    /// resume already crashed in it, once it is older than `max_age_hours`, or
    /// while automatic wakes are held.
    pub fn choice(
        &self,
        configured: ResumeInterrupted,
        max_age_hours: u64,
        now: u64,
        wakes_suppressed: bool,
    ) -> ResumeInterrupted {
        let stale =
            now.saturating_sub(self.stopped_at) > max_age_hours.saturating_mul(SECS_PER_HOUR);
        let crash_loop = self.attempts > 0 && self.cause == StopCause::Crash;
        match configured {
            ResumeInterrupted::Auto if crash_loop || stale || wakes_suppressed => {
                ResumeInterrupted::Ask
            }
            configured => configured,
        }
    }

    /// Whether a resume has anything to restart.
    pub fn resumable(&self) -> bool {
        self.main == MainState::Working || self.work.resumable().next().is_some()
    }

    /// The main chat's reminder. `resuming` is whether the main-owned tasks
    /// are restarted with it.
    pub fn main_nudge(&self, resuming: bool) -> Option<String> {
        if self.main == MainState::Idle {
            return None;
        }
        let lines = agent_lines(&self.dangling, &self.work, &JobOwner::Main, resuming);
        nudge(self.cause, self.main == MainState::Working, &lines)
    }

    /// The prompt a restarted task continues with: the calls its transcript
    /// left open and its own cut-off jobs, which it picks back up itself.
    pub fn task_nudge(&self, job: &InterruptedJob) -> String {
        let owner = JobOwner::Child {
            invocation_id: job.invocation_id.clone(),
        };
        let dangling = self
            .work
            .dangling
            .get(&job.task_id)
            .map_or(&[][..], Vec::as_slice);
        let lines = agent_lines(dangling, &self.work, &owner, false);
        nudge(job.cause, true, &lines).unwrap_or_default()
    }

    /// What a resume picks back up, in a few words: "main turn, 2 tasks".
    pub fn summary(&self) -> String {
        let tasks = self.work.resumable().count();
        let main = match self.main {
            MainState::Working => Some(MAIN_TURN),
            MainState::AwaitingBackground if tasks == 0 => Some(MAIN_CHAT),
            MainState::AwaitingBackground => None,
            MainState::Idle if tasks == 0 => Some(BACKGROUND_WORK),
            MainState::Idle => None,
        }
        .map(str::to_owned);
        let tasks = match tasks {
            0 => None,
            1 => Some(ONE_TASK.to_owned()),
            count => Some(format!("{count} tasks")),
        };
        [main, tasks]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// What a reopened session says it found cut off, for a host to offer a
/// resume of what it did not pick back up itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InterruptedNotice {
    /// Whether a main turn was cut off.
    pub main: bool,
    /// The task_ids of the agent tasks the main chat started.
    pub tasks: Vec<String>,
    /// Whether those tasks were restarted, and the next run resumes the turn.
    pub resumed: bool,
}

impl InterruptedNotice {
    pub fn new(interruption: &Interruption, resumed: bool) -> Self {
        Self {
            main: interruption.main == MainState::Working,
            tasks: interruption
                .work
                .resumable()
                .map(|job| job.task_id.clone())
                .collect(),
            resumed,
        }
    }
}

pub fn main_state(open_run: bool, work: &RestoredWork) -> MainState {
    if open_run {
        MainState::Working
    } else if work.owned_by(&JobOwner::Main).next().is_some() {
        MainState::AwaitingBackground
    } else {
        MainState::Idle
    }
}

/// A call of the transcript that never got its result, by id and tool name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanglingCall {
    pub call_id: String,
    pub tool: String,
}

/// The calls of a stored transcript that no result answers, oldest first.
pub fn dangling_items(items: &[HistoryItem]) -> Vec<DanglingCall> {
    let answered: HashSet<&str> = items
        .iter()
        .filter_map(|item| match &item.kind {
            HistoryItemKind::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    items
        .iter()
        .filter_map(|item| match &item.kind {
            HistoryItemKind::ToolCall { call_id, name, .. }
                if !answered.contains(call_id.as_str()) =>
            {
                Some(DanglingCall {
                    call_id: call_id.clone(),
                    tool: name.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

/// What a call that never settled says once its session reopens.
pub fn interrupted_call_result(call_id: &str, work: &RestoredWork) -> String {
    if let Some(job) = work.agent_task(call_id) {
        return format!(
            "Interrupted: Caudra stopped while task `{}` ran. Its transcript was kept, and it continues in the background under the same task_id; its result arrives as a report.",
            job.task_id
        );
    }
    if let Some(shell) = work.shell(call_id) {
        return format!(
            "Interrupted: Caudra stopped while `{}` ran in {}. Its outcome is unknown and it was not run again; processes it detached may still be running.",
            shell.command, shell.workdir
        );
    }
    CALL_INTERRUPTED.to_owned()
}

/// The result message answering `calls`, as a restore writes it.
fn interrupted_results(calls: &[DanglingCall], work: &RestoredWork) -> Message {
    Message {
        role: Role::User,
        content: calls
            .iter()
            .map(|call| ContentBlock::ToolResult {
                tool_use_id: call.call_id.clone(),
                content: interrupted_call_result(&call.call_id, work),
                is_error: true,
                output_ref: None,
            })
            .collect(),
        display_text: Some(String::new()),
        ..Default::default()
    }
}

/// Ends a transcript that stops on an assistant turn with open calls, answering
/// each with what [`interrupted_call_result`] says about it. Returns the calls it
/// closed.
pub fn close_interrupted_calls(
    messages: &mut Vec<Message>,
    work: &RestoredWork,
) -> Vec<DanglingCall> {
    let Some(last) = messages
        .last()
        .filter(|last| matches!(last.role, Role::Assistant))
    else {
        return Vec::new();
    };
    let calls: Vec<DanglingCall> = last
        .tool_uses()
        .map(|(id, name, _)| DanglingCall {
            call_id: id.to_owned(),
            tool: name.to_owned(),
        })
        .collect();
    if !calls.is_empty() {
        messages.push(interrupted_results(&calls, work));
    }
    calls
}

/// [`close_interrupted_calls`] for a stored transcript, before it is restored:
/// only the calls of its final group are answered, since a restore repairs any
/// earlier gap as it always has.
pub fn close_interrupted_items(
    items: &mut Vec<HistoryItem>,
    work: &RestoredWork,
) -> Vec<DanglingCall> {
    let Some(group_id) = items.last().map(|item| item.group_id) else {
        return Vec::new();
    };
    let group_start = items
        .iter()
        .rposition(|item| item.group_id != group_id)
        .map_or(0, |index| index + 1);
    let calls: Vec<DanglingCall> = dangling_items(&items[group_start..]);
    if calls.is_empty() {
        return calls;
    }
    let parent = items.last().map(|item| item.id);
    items.extend(expand_message(&interrupted_results(&calls, work), parent));
    calls
}

/// One line of the reminder for a call that never settled.
pub fn call_line(call: &DanglingCall, work: &RestoredWork) -> String {
    if let Some(shell) = work.shell(&call.call_id) {
        return format!(
            "`{}` call running `{}` in {}: outcome unknown, not run again",
            call.tool, shell.command, shell.workdir
        );
    }
    format!("`{}` call: effects unknown, not run again", call.tool)
}

/// One line of the reminder for a job the agent started. `resumed` is whether
/// Caudra restarts it on its own.
pub fn job_line(job: &InterruptedJob, resumed: bool) -> String {
    match job.kind {
        JobKind::Agent if resumed => format!(
            "task `{}` ({}): resumed in the background from its saved transcript; its result arrives as a report",
            job.task_id,
            match job.background {
                true => "background",
                false => "was foreground",
            }
        ),
        JobKind::Agent => format!(
            "task `{}`: interrupted; continue it with `{TASK_TOOL}` and its task_id if it is still needed",
            job.task_id
        ),
        JobKind::Shell => format!(
            "background shell `{}`: interrupted, outcome unknown, not run again",
            job.label
        ),
        JobKind::Question => format!(
            "question `{}`: closed unanswered; ask again if you still need the answer",
            job.label
        ),
    }
}

/// The reminder a resumed agent reads first. `None` when nothing it owned was
/// cut off and it was not working either.
pub fn nudge(cause: StopCause, working: bool, lines: &[String]) -> Option<String> {
    if !working && lines.is_empty() {
        return None;
    }
    let mut text = String::from(NUDGE_OPEN);
    let stopped = match cause {
        StopCause::Crash => CRASHED,
        StopCause::Shutdown => SHUT_DOWN,
    };
    let doing = match working {
        true => WHILE_WORKING,
        false => WHILE_WAITING,
    };
    text.push_str(&format!("{stopped} {doing}. {REOPENED}\n"));
    if !lines.is_empty() {
        text.push_str(INTERRUPTED_HEADING);
        text.push('\n');
        for line in lines {
            text.push_str("- ");
            text.push_str(line);
            text.push('\n');
        }
    }
    text.push_str(RECONCILE);
    text.push('\n');
    text.push_str(NUDGE_CLOSE);
    Some(text)
}

/// What an agent's reminder lists: the calls its transcript left open and the
/// jobs `owner` started, as `resuming` restarts its agent tasks. A call that
/// started one of those jobs is listed once, as the job.
pub fn agent_lines(
    dangling: &[DanglingCall],
    work: &RestoredWork,
    owner: &JobOwner,
    resuming: bool,
) -> Vec<String> {
    let jobs: Vec<&InterruptedJob> = work.owned_by(owner).collect();
    let started: HashSet<&str> = jobs.iter().map(|job| job.call_id.as_str()).collect();
    dangling
        .iter()
        .filter(|call| !started.contains(call.call_id.as_str()))
        .map(|call| call_line(call, work))
        .chain(
            jobs.into_iter()
                .map(|job| job_line(job, resuming && job.kind == JobKind::Agent)),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use caudra_providers::CaudraId;
    use caudra_storage::shell_history::{ShellExecutionOwner, ShellExecutionState};
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const SHELL_CALL: &str = "call-shell";
    const TASK_CALL: &str = "call-task";
    const CODE_CALL: &str = "call-code";
    const TASK_ID: &str = "brave-otter";
    const COMMAND: &str = "cargo test";
    const WORKDIR: &str = "/repo";

    fn job(kind: JobKind, owner: JobOwner, background: bool, cause: StopCause) -> InterruptedJob {
        InterruptedJob {
            task_id: TASK_ID.into(),
            invocation_id: CaudraId::generate().to_string(),
            call_id: TASK_CALL.into(),
            kind,
            owner,
            background,
            label: TASK_ID.into(),
            cause,
            restarts: 0,
            stopped_at: 0,
        }
    }

    fn shell() -> ShellExecutionRecord {
        ShellExecutionRecord {
            execution_id: CaudraId::generate().to_string(),
            owner: ShellExecutionOwner::default(),
            call_id: SHELL_CALL.into(),
            command: COMMAND.into(),
            command_truncated: false,
            workdir: WORKDIR.into(),
            remote: None,
            timeout_ms: None,
            created_at_ms: 0,
            finished_at_ms: None,
            state: ShellExecutionState::Interrupted,
            started: true,
            reason: None,
        }
    }

    fn work() -> RestoredWork {
        RestoredWork {
            jobs: vec![job(JobKind::Agent, JobOwner::Main, false, StopCause::Crash)],
            shells: vec![shell()],
            ..RestoredWork::default()
        }
    }

    fn calls(ids: &[(&str, &str)]) -> Message {
        Message {
            role: Role::Assistant,
            content: ids
                .iter()
                .map(|(id, name)| ContentBlock::ToolUse {
                    id: (*id).into(),
                    name: (*name).into(),
                    input: json!({}),
                    thought_signature: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test_case(SHELL_CALL, COMMAND; "shell_names_its_command")]
    #[test_case(TASK_CALL, TASK_ID; "foreground_task_names_its_id")]
    #[test_case(CODE_CALL, CALL_INTERRUPTED; "unknown_call_is_generic")]
    fn interrupted_calls_say_what_became_of_them(call_id: &str, expected: &str) {
        assert!(interrupted_call_result(call_id, &work()).contains(expected));
    }

    #[test]
    fn a_stored_transcript_gets_its_final_calls_answered() {
        let mut items = expand_message(&Message::user("go".into()), None);
        items.extend(expand_message(
            &calls(&[(SHELL_CALL, "shell"), (CODE_CALL, "code")]),
            items.last().map(|item| item.id),
        ));
        let closed = close_interrupted_items(&mut items, &work());
        assert_eq!(closed.len(), 2);
        assert!(dangling_items(&items).is_empty());
        assert!(close_interrupted_items(&mut items, &work()).is_empty());
    }

    #[test]
    fn only_unanswered_calls_dangle() {
        let mut items = expand_message(&calls(&[(SHELL_CALL, "shell"), (CODE_CALL, "code")]), None);
        let result = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: CODE_CALL.into(),
                content: String::new(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        };
        items.extend(expand_message(&result, items.last().map(|item| item.id)));
        assert_eq!(
            dangling_items(&items),
            [DanglingCall {
                call_id: SHELL_CALL.into(),
                tool: "shell".into(),
            }]
        );
    }

    #[test]
    fn closing_an_open_turn_answers_every_call() {
        let mut messages = vec![
            Message::user("go".into()),
            calls(&[(SHELL_CALL, "shell"), (CODE_CALL, "code")]),
        ];
        let closed = close_interrupted_calls(&mut messages, &work());
        assert_eq!(closed.len(), 2);
        let results: Vec<&str> = messages[2]
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert!(results[0].contains(COMMAND));
        assert_eq!(results[1], CALL_INTERRUPTED);
        assert!(close_interrupted_calls(&mut messages, &work()).is_empty());
    }

    #[test_case(true, &[] => true; "working_alone_nudges")]
    #[test_case(false, &[] => false; "idle_with_nothing_is_silent")]
    #[test_case(false, &["x"] => true; "waiting_with_items_nudges")]
    fn nudge_appears_only_with_something_to_say(working: bool, lines: &[&str]) -> bool {
        let lines: Vec<String> = lines.iter().map(|line| (*line).into()).collect();
        nudge(StopCause::Crash, working, &lines).is_some()
    }

    #[test_case(true, JobOwner::Main => MainState::Working; "open_run")]
    #[test_case(false, JobOwner::Main => MainState::AwaitingBackground; "main_owned_job")]
    #[test_case(false, JobOwner::Child { invocation_id: TASK_ID.into() } => MainState::Idle; "child_owned_job")]
    fn main_state_follows_the_open_run_and_its_jobs(open: bool, owner: JobOwner) -> MainState {
        let work = RestoredWork {
            jobs: vec![job(JobKind::Agent, owner, true, StopCause::Crash)],
            ..RestoredWork::default()
        };
        main_state(open, &work)
    }

    #[test_case(&[StopCause::Shutdown, StopCause::Shutdown] => StopCause::Shutdown; "all_shut_down")]
    #[test_case(&[StopCause::Shutdown, StopCause::Crash] => StopCause::Crash; "one_crashed")]
    #[test_case(&[] => StopCause::Crash; "nothing_recorded")]
    fn the_cause_is_shutdown_only_when_every_job_says_so(causes: &[StopCause]) -> StopCause {
        RestoredWork {
            jobs: causes
                .iter()
                .map(|cause| job(JobKind::Agent, JobOwner::Main, true, *cause))
                .collect(),
            ..RestoredWork::default()
        }
        .cause()
    }

    const MAX_AGE_HOURS: u64 = 24;
    const MAIN_WORKING: bool = true;
    const MAIN_WAITING: bool = false;

    fn interrupted_run(attempts: u32, shut_down: bool) -> Interruption {
        let run = StoredOpenRun {
            started_at: 0,
            resume_attempts: attempts,
            shut_down_at: shut_down.then_some(0),
        };
        Interruption::new(RestoredWork::default(), Vec::new(), Some(&run), 0).unwrap()
    }

    #[test_case(ResumeInterrupted::Auto, 0, false, 1, false => ResumeInterrupted::Auto; "fresh_work_resumes")]
    #[test_case(ResumeInterrupted::Auto, 1, false, 1, false => ResumeInterrupted::Ask; "a_resume_already_crashed")]
    #[test_case(ResumeInterrupted::Auto, 1, true, 1, false => ResumeInterrupted::Auto; "a_resume_quit_cleanly")]
    #[test_case(ResumeInterrupted::Auto, 0, false, 25, false => ResumeInterrupted::Ask; "older_than_the_max_age")]
    #[test_case(ResumeInterrupted::Auto, 0, false, 1, true => ResumeInterrupted::Ask; "wakes_suppressed")]
    #[test_case(ResumeInterrupted::Off, 1, false, 25, true => ResumeInterrupted::Off; "off_stays_off")]
    fn auto_asks_behind_its_guards(
        configured: ResumeInterrupted,
        attempts: u32,
        shut_down: bool,
        hours_ago: u64,
        wakes_suppressed: bool,
    ) -> ResumeInterrupted {
        interrupted_run(attempts, shut_down).choice(
            configured,
            MAX_AGE_HOURS,
            hours_ago * SECS_PER_HOUR,
            wakes_suppressed,
        )
    }

    #[test_case(true, true => (MAIN_WORKING, vec![TASK_ID.to_owned()], true); "resumed_turn_and_task")]
    #[test_case(false, false => (MAIN_WAITING, vec![TASK_ID.to_owned()], false); "waiting_and_left")]
    fn the_notice_names_what_a_host_could_resume(
        open: bool,
        resumed: bool,
    ) -> (bool, Vec<String>, bool) {
        let run = StoredOpenRun {
            started_at: 0,
            resume_attempts: 0,
            shut_down_at: None,
        };
        let interruption = Interruption::new(work(), Vec::new(), open.then_some(&run), 0).unwrap();
        let notice = InterruptedNotice::new(&interruption, resumed);
        (notice.main, notice.tasks, notice.resumed)
    }

    #[test]
    fn nothing_cut_off_is_no_interruption() {
        assert!(Interruption::new(RestoredWork::default(), Vec::new(), None, 0).is_none());
    }

    #[test]
    fn the_summary_counts_what_resumes() {
        let mut work = work();
        work.jobs[0].background = true;
        work.jobs
            .push(job(JobKind::Agent, JobOwner::Main, true, StopCause::Crash));
        let run = StoredOpenRun {
            started_at: 0,
            resume_attempts: 0,
            shut_down_at: Some(0),
        };
        let interruption = Interruption::new(work, Vec::new(), Some(&run), 0).unwrap();
        assert_eq!(interruption.summary(), "main turn, 2 tasks");
        assert_eq!(interruption.cause, StopCause::Shutdown);
    }

    #[test]
    fn a_task_nudge_lists_only_its_own_children() {
        let mut work = RestoredWork::default();
        let parent = job(JobKind::Agent, JobOwner::Main, true, StopCause::Crash);
        let owner = JobOwner::Child {
            invocation_id: parent.invocation_id.clone(),
        };
        work.jobs
            .push(job(JobKind::Shell, owner, true, StopCause::Crash));
        work.jobs.push(parent.clone());
        let interruption = Interruption::new(work, Vec::new(), None, 0).unwrap();
        let nudge = interruption.task_nudge(&parent);
        assert_eq!(nudge.matches("\n- ").count(), 1);
        assert!(nudge.contains("background shell"));
    }

    #[test_case(false; "foreground")]
    #[test_case(true; "background_launched_beside_an_open_call")]
    fn a_task_is_listed_once_as_resumed(background: bool) {
        let mut work = work();
        work.jobs[0].background = background;
        let dangling = [DanglingCall {
            call_id: TASK_CALL.into(),
            tool: TASK_TOOL.into(),
        }];
        let lines = agent_lines(&dangling, &work, &JobOwner::Main, true);
        assert_eq!(lines, [job_line(&work.jobs[0], true)]);
    }

    #[test]
    fn a_task_nudge_names_the_calls_its_transcript_left_open() {
        let mut work = work();
        work.dangling.insert(
            TASK_ID.into(),
            vec![DanglingCall {
                call_id: SHELL_CALL.into(),
                tool: "shell".into(),
            }],
        );
        let interruption = Interruption::new(work, Vec::new(), None, 0).unwrap();
        let nudge = interruption.task_nudge(&interruption.work.jobs[0]);
        assert!(nudge.contains(COMMAND));
    }
}
