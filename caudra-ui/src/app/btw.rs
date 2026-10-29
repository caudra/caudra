use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use caudra_agent::agent::aside::run_aside;
use caudra_agent::agent::task_runner::WorkflowHostContext;
use caudra_agent::context::{ContextKey, RequestPrefix};
use caudra_agent::permissions::AutopilotNote;
use caudra_agent::{
    AgentEvent, CancelToken, CancelTrigger, DoneReason, Envelope, EventSender, History,
    HistorySnapshot,
};
use caudra_providers::{CacheKey, ContentBlock, Message, Role, TokenUsage, project_messages};
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::media::MediaStore;
use crossterm::event::{KeyEvent, MouseEvent};
use flume::{Receiver, Sender, TryRecvError};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use crate::chat::Chat;
use crate::components::side_transcript::{MOVED_ON_NOTE, SideAction, SideFooter, SideTranscript};
use crate::components::tooltip::Tip;
use crate::components::{DisplayMessage, DisplayRole, Overlay};
use crate::repaint::{Cadence, Dirty};

use super::{App, COPIED_SELECTION, HISTORY_UNREADABLE};

const TITLE: &str = " /btw ";
const ASK_TITLE: &str = " /btw · Side question ";
const CLARIFICATION_TITLE: &str = " /btw · Clarify questions ";
const CHAT_NAME: &str = "/btw";
/// What the permission prompt says asked, for a call a side question made.
const PERMISSION_ORIGIN: &str = "/btw";
const STOPPED_NOTE: &str = "Stopped.";
pub(super) const RUN_LOST: &str = "The side question stopped without an answer";
pub(super) const PROMPT_INITIALIZING: &str = "System prompt is still initializing";
pub(super) const TASK_HISTORY_UNAVAILABLE: &str =
    "This task's conversation is not loaded, so /btw cannot read it";
const CLARIFICATION_REMINDER: &str = "The user is clarifying a pending question form, not answering it. \
The asking agent is still waiting. Explain the question or choices; do not select, submit, or change \
answers. The following is the question currently being viewed, not a user decision:";
/// Drawn where the thread's snapshot of the conversation ends, so the reader
/// can tell what the side question could and could not see.
pub(crate) const BTW_CUTOFF_MARKER: &str = "/btw reads the conversation up to here";

pub(super) const CATCHUP_HEADER: &str = "Catch me up";
pub(super) const NOTHING_TO_CATCH_UP: &str = "Nothing to catch up on yet";
pub(super) const CATCHUP_FOCUS_PREFIX: &str = "\n\nFocus on: ";
pub(super) const CATCHUP_PROMPT: &str = "I've lost track of this session. I've been away, or busy \
with other sessions. Bring me up to speed from the conversation so far, so I can carry on without \
rereading it.

Open with one or two sentences: what we are trying to achieve, and where things stand right now.

Then use these short sections, in this order, and leave out any that would be empty:
- **Needs you**: anything waiting on me, such as an unanswered question, a decision, an approval, \
a plan to review, or a failure only I can unblock. Say exactly what I have to decide and what the \
options are.
- **Now**: what the agent was doing where the conversation ends, and what it was about to do next.
- **Done**: what is finished, stated concretely (files, commands, commits, test results). Mark \
anything that was claimed but not verified.
- **Decided**: choices already settled and the reason for each, so I don't reopen them. Quote my \
own words where I made the call.
- **Open**: failures, risks, and loose ends that are still unresolved.

Give more weight to what happened since my last message than to early turns, and drop anything \
that no longer matters. Don't retell the session turn by turn. Name things exactly (files, \
branches, commands, errors) rather than describing them. Keep it to about 200 words unless the \
session really needs more. If the conversation doesn't make something clear, say so instead of \
guessing.";

const ROUND_SEPARATOR: &str = "\n\n";

/// Leads the first question after the thread caught up with a conversation
/// that moved on, over the exchanges it had before. Quoting them in one
/// message keeps the conversation's cached prefix reachable, and saying they
/// are older keeps the model from defending an answer the conversation has
/// since overtaken.
const REBASE_REMINDER: &str = "<system-reminder>\n# Earlier side questions\n\nThe side \
questions quoted below were answered from an earlier point in the conversation, which has moved \
on since. They are quoted for continuity only. Where they disagree with the conversation as it \
now stands, the conversation is right; say so when a new answer corrects an old one.\n\
</system-reminder>";
/// How many earlier exchanges a caught-up thread quotes. Older ones are only
/// counted, so a long thread cannot grow its request without bound.
const MAX_REPLAYED_EXCHANGES: usize = 10;
const OMITTED_PREFIX: &str = "(";
const OMITTED_SUFFIX: &str = " earlier exchanges left out)";
const QUESTION_OPEN: &str = "<question>\n";
const QUESTION_CLOSE: &str = "\n</question>\n";
const ANSWER_OPEN: &str = "<answer>\n";
const ANSWER_CLOSE: &str = "\n</answer>";

/// The text of a message, without its reasoning or tool calls.
fn message_text(message: &Message) -> Option<String> {
    let text: Vec<&str> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let text = text.join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// What the model said last, which is what a later quote of the exchange
/// keeps once its tool turns are dropped.
fn final_answer(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .filter(|message| matches!(message.role, Role::Assistant))
        .find_map(message_text)
        .unwrap_or_default()
}

/// How far a conversation had got when a thread read it. Equal marks mean the
/// thread's base is still the whole conversation; a conversation only grows
/// or starts a new epoch, and a finished task's never changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HistoryMark {
    epoch: Option<u64>,
    len: usize,
}

/// Where a side question reads a conversation from, held as one load so the
/// messages and the mark that describes them cannot disagree.
enum BtwSource {
    Live(Arc<HistorySnapshot>),
    Kept(Arc<Vec<Message>>),
}

impl BtwSource {
    fn mark(&self) -> HistoryMark {
        match self {
            Self::Live(snapshot) => HistoryMark {
                epoch: Some(snapshot.epoch),
                len: snapshot.messages.len(),
            },
            Self::Kept(messages) => HistoryMark {
                epoch: None,
                len: messages.len(),
            },
        }
    }

    fn messages(&self) -> Result<Vec<Message>, String> {
        match self {
            Self::Live(snapshot) => project_messages(&snapshot.messages)
                .map_err(|error| format!("{HISTORY_UNREADABLE}{error}")),
            Self::Kept(messages) => Ok(Vec::clone(messages)),
        }
    }
}

/// A side conversation forked from the main one: the prefix it continues,
/// pinned, and a history that starts as the conversation it forked and grows
/// with every exchange, tool turns included. The agent projects it exactly as
/// the live request does, so the provider reads the cached prefix. Nothing
/// here is persisted. Closing the window keeps it until it is cleared or the
/// session changes.
pub(crate) struct BtwThread {
    prefix: Arc<RequestPrefix>,
    messages: Vec<Message>,
    mark: HistoryMark,
    /// Exchanges asked over an earlier base, quoted rather than replayed.
    earlier: Vec<(String, String)>,
    /// Earlier exchanges too old to quote.
    omitted: usize,
    /// The questions answered over the current base, with the answer each
    /// ended on: what a rebase quotes once their tool turns are dropped.
    answered: Vec<(String, String)>,
    pending: Option<String>,
    clarification_context: Option<String>,
    /// The transcript already says the conversation moved on past it.
    moved_on: bool,
}

impl BtwThread {
    fn new(prefix: Arc<RequestPrefix>, messages: Vec<Message>, mark: HistoryMark) -> Self {
        Self {
            prefix,
            messages,
            mark,
            earlier: Vec::new(),
            omitted: 0,
            answered: Vec::new(),
            pending: None,
            clarification_context: None,
            moved_on: false,
        }
    }

    /// Moves the thread onto the conversation as it stands now. The answers
    /// so far were given over the old base, so they become quoted context
    /// for the next question, and their tool turns are left behind.
    fn rebase(&mut self, prefix: Arc<RequestPrefix>, base: Vec<Message>, mark: HistoryMark) {
        self.earlier.append(&mut self.answered);
        let excess = self.earlier.len().saturating_sub(MAX_REPLAYED_EXCHANGES);
        self.earlier.drain(..excess);
        self.omitted += excess;
        self.prefix = prefix;
        self.messages = base;
        self.mark = mark;
        self.moved_on = false;
    }

    /// The earlier exchanges as the quote that leads the first question.
    fn digest(&self) -> Option<String> {
        if self.earlier.is_empty() {
            return None;
        }
        let mut digest = REBASE_REMINDER.to_owned();
        if self.omitted > 0 {
            digest.push_str(&format!(
                "{ROUND_SEPARATOR}{OMITTED_PREFIX}{}{OMITTED_SUFFIX}",
                self.omitted
            ));
        }
        for (question, answer) in &self.earlier {
            digest.push_str(&format!(
                "{ROUND_SEPARATOR}{QUESTION_OPEN}{question}{QUESTION_CLOSE}{ANSWER_OPEN}{answer}{ANSWER_CLOSE}"
            ));
        }
        Some(digest)
    }

    /// A question that never settled is replaced rather than kept: the run
    /// that carried it failed, so the model never answered it.
    fn ask(&mut self, question: String) {
        self.pending = Some(question);
    }

    fn ask_clarification(&mut self, question: String, context: String) {
        let changed = self.clarification_context.as_ref() != Some(&context)
            || self.pending.is_some()
            || self.answered.is_empty();
        let question = if changed {
            format!("{CLARIFICATION_REMINDER}\n\n{context}\n\n{question}")
        } else {
            question
        };
        self.clarification_context = Some(context);
        self.ask(question);
    }

    /// The pending question as the model reads it. After a rebase the quoted
    /// earlier exchanges ride the first question only: a later copy would
    /// say nothing new and move the cache boundary for every follow-up.
    fn question(&self) -> String {
        let question = self.pending.clone().unwrap_or_default();
        match self.digest().filter(|_| self.answered.is_empty()) {
            Some(digest) => format!("{digest}{ROUND_SEPARATOR}{question}"),
            None => question,
        }
    }

    /// Files the pending question with the history its run left, which ends
    /// on the answer. No answer, which is what a failed or stopped run
    /// leaves, drops the question instead, along with whatever the run did.
    fn settle(&mut self, answered: Option<Vec<Message>>) {
        let Some(question) = self.pending.take() else {
            return;
        };
        match answered {
            Some(messages) => {
                self.answered.push((question, final_answer(&messages)));
                self.messages = messages;
            }
            None => self.clarification_context = None,
        }
    }

    #[cfg(test)]
    pub(super) fn exchange_count(&self) -> usize {
        self.answered.len()
    }

    #[cfg(test)]
    pub(super) fn quoted_count(&self) -> usize {
        self.earlier.len()
    }

    #[cfg(test)]
    pub(super) fn pending(&self) -> Option<&str> {
        self.pending.as_deref()
    }

    #[cfg(test)]
    pub(super) fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// The text of every message the next question extends.
    #[cfg(test)]
    pub(super) fn base_texts(&self) -> Vec<String> {
        self.messages.iter().filter_map(message_text).collect()
    }

    /// The model the thread asks and the cache it reads.
    #[cfg(test)]
    pub(super) fn route(&self) -> (String, Option<&CacheKey>) {
        (self.prefix.model.spec(), self.prefix.cache_key.as_ref())
    }
}

/// A side question's window, the conversation it asks about, and the thread
/// it keeps once the first question is sent.
pub(crate) struct SideQuestion {
    /// The task whose conversation it reads, `None` for the main one.
    task: Option<String>,
    window: SideTranscript,
    thread: Option<BtwThread>,
    /// The question form a clarification is about, as the window last showed
    /// it. Only a clarification has one.
    context: Option<String>,
}

impl SideQuestion {
    fn new(task: Option<String>, mut window: SideTranscript) -> Self {
        window.open();
        Self {
            task,
            window,
            thread: None,
            context: None,
        }
    }

    /// Says once under the last answer that the conversation moved on since.
    fn note_moved_on(&mut self) {
        if let Some(thread) = self.thread.as_mut()
            && !thread.moved_on
        {
            thread.moved_on = true;
            self.window.push_notice(MOVED_ON_NOTE);
        }
    }
}

/// What a run hands back: the history it left and how it ended.
type AsideReply = (Vec<Message>, Result<DoneReason, String>);

/// The side question being answered. Dropping it stops the run.
struct AsideRun {
    id: u64,
    usage: TokenUsage,
    reply: Receiver<AsideReply>,
    _stop: CancelTrigger,
}

/// Every side question: the one shown, or the clarification kept behind its
/// form, and the ones closed and kept per conversation. Runs report on one
/// channel, so a run's spend is billed even after its window has gone.
pub(crate) struct Btw {
    side: Option<SideQuestion>,
    parked: HashMap<Option<String>, SideQuestion>,
    run: Option<AsideRun>,
    sender: Sender<Envelope>,
    events: Receiver<Envelope>,
    runs: u64,
}

impl Default for Btw {
    fn default() -> Self {
        let (sender, events) = flume::unbounded();
        Self {
            side: None,
            parked: HashMap::new(),
            run: None,
            sender,
            events,
            runs: 0,
        }
    }
}

impl Btw {
    pub(crate) fn is_running(&self) -> bool {
        self.run.is_some()
    }

    pub(crate) fn is_clarifying(&self) -> bool {
        self.side
            .as_ref()
            .is_some_and(|side| side.context.is_some())
    }

    pub(crate) fn text_input_active(&self) -> bool {
        self.side
            .as_ref()
            .is_some_and(|side| side.window.text_input_active())
    }

    pub(crate) fn handle_text(&mut self, text: &str, paste: bool) -> bool {
        self.is_open()
            && self
                .side
                .as_mut()
                .is_some_and(|side| side.window.handle_text(text, paste))
    }

    pub(crate) fn contains(&self, pos: Position) -> bool {
        self.is_open()
            && self
                .side
                .as_ref()
                .is_some_and(|side| side.window.contains(pos))
    }

    pub(crate) fn scroll_at(&mut self, at: Position, delta: i32) {
        if let Some(side) = self.side.as_mut() {
            side.window.scroll_at(at, delta);
        }
    }

    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.side
            .as_mut()
            .map_or_else(Rect::default, |side| side.window.view(frame, area))
    }

    fn handle_key(&mut self, key: KeyEvent) -> SideAction {
        self.side
            .as_mut()
            .map_or(SideAction::Ignored, |side| side.window.handle_key(key))
    }

    fn handle_mouse(&mut self, event: MouseEvent, cwd: &Path) -> SideAction {
        self.side.as_mut().map_or(SideAction::Ignored, |side| {
            side.window.handle_mouse(event, cwd)
        })
    }

    fn thread_mut(&mut self) -> Option<&mut BtwThread> {
        self.side.as_mut()?.thread.as_mut()
    }

    #[cfg(test)]
    pub(crate) fn thread(&self) -> Option<&BtwThread> {
        self.side.as_ref()?.thread.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn window(&self) -> Option<&SideTranscript> {
        self.side.as_ref().map(|side| &side.window)
    }

    #[cfg(test)]
    pub(crate) fn has_parked(&self) -> bool {
        !self.parked.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn sender(&self) -> &Sender<Envelope> {
        &self.sender
    }
}

impl Overlay for Btw {
    fn is_open(&self) -> bool {
        self.side.as_ref().is_some_and(|side| side.window.is_open())
    }

    /// Hides the window. What closing means for the thread it shows, the app
    /// settles on its next tick.
    fn close(&mut self) {
        if let Some(side) = self.side.as_mut() {
            side.window.close();
        }
    }

    fn cadence(&self) -> Cadence {
        self.side
            .as_ref()
            .map_or(Cadence::IDLE, |side| side.window.cadence())
    }

    fn tooltip(&self) -> Option<Tip> {
        self.side.as_ref()?.window.tooltip()
    }
}

/// Answers `question` over `messages` on the agent loop, in a context built
/// for this run alone. What the stored media of a restored session only
/// references is loaded first, off the UI thread, so it goes out attached.
async fn ask_aside(
    host: Arc<WorkflowHostContext>,
    prefix: Arc<RequestPrefix>,
    mut messages: Vec<Message>,
    store: MediaStore,
    events: EventSender,
    cancel: CancelToken,
    question: String,
) -> AsideReply {
    caudra_agent::load_stored_media(&mut messages, &store).await;
    let mut history = History::new(messages);
    let call_id = CaudraId::generate().to_string();
    let done = match crate::agent::task_context(&host, None, cancel, events, &call_id).await {
        Ok(ctx) => run_aside(&ctx, &prefix, &mut history, question)
            .await
            .map_err(|error| error.to_string()),
        Err(error) => Err(error),
    };
    (history.as_slice().to_vec(), done)
}

impl App {
    fn btw_window(&self, title: &'static str, footer: SideFooter) -> SideTranscript {
        let chat = Chat::new(
            CHAT_NAME.to_owned(),
            Path::new(&self.state.session.cwd),
            self.ui_config.clone(),
            self.lua_event_handle.clone(),
        );
        SideTranscript::new(title, footer, chat)
    }

    pub(super) fn open_question_btw(&mut self) {
        if !self.question_form.is_open() {
            return;
        }
        let asker = self.question_asker();
        if self.btw_prompt_for(asker.as_deref()).is_none() {
            self.flash(PROMPT_INITIALIZING.into());
            return;
        }
        self.close_stream_modal();
        if self.btw.is_clarifying() {
            let _ = self.poll_btw_run();
        } else {
            self.close_btw();
        }
        self.autoscroll = None;
        self.selection_state = None;
        let context = self.question_form.clarification_context();
        if self.btw.side.is_none() {
            let window = self.btw_window(CLARIFICATION_TITLE, SideFooter::Clarify);
            self.btw.side = Some(SideQuestion::new(asker, window));
        }
        let Some(side) = self.btw.side.as_mut() else {
            return;
        };
        if side.context.as_ref() != Some(&context) {
            side.window.push_notice(context.clone());
            side.context = Some(context);
        }
        side.window.open();
    }

    /// `F2`: the thread this conversation kept, or an empty `/btw` that reads
    /// the conversation only once the first question is sent, so the answer
    /// sees everything up to then.
    pub(super) fn open_btw_prompt(&mut self) {
        let task = self.active_subagent_id().map(str::to_owned);
        if self.btw_prompt_for(task.as_deref()).is_none() {
            self.flash(PROMPT_INITIALIZING.into());
            return;
        }
        self.close_stream_modal();
        self.close_btw();
        self.autoscroll = None;
        self.selection_state = None;
        if !self.resume_btw(task.clone()) {
            let window = self.btw_window(ASK_TITLE, SideFooter::Ask);
            self.btw.side = Some(SideQuestion::new(task, window));
        }
    }

    pub(super) fn discard_question_btw(&mut self) {
        if self.btw.is_clarifying() {
            self.close_btw();
        }
    }

    /// Takes the side question off the screen and stops its run. A side
    /// thread is kept for its conversation; a clarification is not.
    pub(super) fn close_btw(&mut self) {
        if self.btw.side.is_none() {
            return;
        }
        self.stop_btw_run();
        let Some(mut side) = self.btw.side.take() else {
            return;
        };
        side.window.close();
        self.unmark_btw_cutoff(side.task.as_deref());
        if side.context.is_none() {
            self.btw.parked.insert(side.task.clone(), side);
        }
    }

    /// The window closed itself. A clarification stays behind its form,
    /// stopped; anything else is closed.
    fn btw_closed(&mut self) {
        match self.btw.is_clarifying() {
            true => self.stop_btw_run(),
            false => self.close_btw(),
        }
    }

    pub(super) fn handle_btw_key(&mut self, key: KeyEvent) {
        let action = self.btw.handle_key(key);
        self.handle_btw_action(action);
    }

    /// Whether the pointer was the window's.
    pub(super) fn handle_btw_mouse(&mut self, event: MouseEvent) -> bool {
        let cwd = self.state.session.cwd.clone();
        let action = self.btw.handle_mouse(event, Path::new(&cwd));
        let ignored = action == SideAction::Ignored;
        self.handle_btw_action(action);
        !ignored
    }

    /// An outside click hides the window as Esc would.
    pub(super) fn dismiss_btw(&mut self) {
        Overlay::close(&mut self.btw);
        self.btw_closed();
    }

    fn handle_btw_action(&mut self, action: SideAction) {
        match action {
            SideAction::Ignored | SideAction::Consumed => {}
            SideAction::Close => self.btw_closed(),
            SideAction::Copy(text) => self.copy_to_clipboard(&text),
            SideAction::CopySelection(text) => self.copy_labelled(&text, COPIED_SELECTION),
            SideAction::Submit(question) => self.continue_btw(question),
            SideAction::Clear => self.clear_btw_thread(),
            SideAction::Interrupt => self.stop_btw_run(),
        }
    }

    /// `Ctrl+L`: forgets the thread the window shows and leaves an empty
    /// `/btw` in its place.
    pub(super) fn clear_btw_thread(&mut self) {
        self.stop_btw_run();
        let task = match self.btw.side.take() {
            Some(side) => {
                self.unmark_btw_cutoff(side.task.as_deref());
                side.task
            }
            None => self.active_subagent_id().map(str::to_owned),
        };
        self.btw.parked.remove(&task);
        let window = self.btw_window(ASK_TITLE, SideFooter::Ask);
        self.btw.side = Some(SideQuestion::new(task, window));
    }

    /// Forgets every side question, for a session that is going away. What
    /// their runs spent is billed to it first, and anything a stopped run
    /// reports later never reaches the next one.
    pub(super) fn discard_btw_threads(&mut self) {
        self.stop_btw_run();
        if let Some(side) = self.btw.side.take() {
            self.unmark_btw_cutoff(side.task.as_deref());
        }
        self.btw = Btw::default();
    }

    /// Reopens what `task`'s conversation kept, if it kept anything. A
    /// thread whose conversation has moved on says so and gets no transcript
    /// marker until its next question catches it up.
    fn resume_btw(&mut self, task: Option<String>) -> bool {
        let Some(mut side) = self.btw.parked.remove(&task) else {
            return false;
        };
        let current = match &side.thread {
            None => true,
            Some(thread) => match self.btw_source(task.as_deref()) {
                Ok(source) => source.mark() == thread.mark,
                Err(_) => return false,
            },
        };
        side.window.open();
        if side.thread.is_some() && !current {
            side.note_moved_on();
        }
        let marked = side.thread.is_some() && current;
        self.btw.side = Some(side);
        if marked {
            self.mark_btw_cutoff(task.as_deref());
        }
        true
    }

    /// Moves the open thread onto its conversation as it stands now, if that
    /// moved on since the thread read it. False, with the reason flashed,
    /// when the conversation can no longer be read.
    fn catch_up_btw_thread(&mut self) -> bool {
        let Some(side) = &self.btw.side else {
            return true;
        };
        let Some(thread) = &side.thread else {
            return true;
        };
        let task = side.task.clone();
        let caught_up = self.btw_source(task.as_deref()).and_then(|source| {
            let mark = source.mark();
            if thread.mark == mark {
                return Ok(None);
            }
            let prefix = self
                .btw_prompt_for(task.as_deref())
                .ok_or_else(|| PROMPT_INITIALIZING.to_owned())?;
            Ok(Some((prefix, source.messages()?, mark)))
        });
        let (prefix, base, mark) = match caught_up {
            Ok(None) => return true,
            Ok(Some(caught_up)) => caught_up,
            Err(error) => {
                self.status_bar.flash(error);
                return false;
            }
        };
        if let Some(side) = self.btw.side.as_mut() {
            side.note_moved_on();
            if let Some(thread) = side.thread.as_mut() {
                thread.rebase(prefix, base, mark);
            }
        }
        self.mark_btw_cutoff(task.as_deref());
        true
    }

    /// The task whose question the form shows, `None` for the main agent's.
    fn question_asker(&self) -> Option<String> {
        if self.question_form.deferred_task().is_some() {
            return None;
        }
        self.question_subagent.clone().or_else(|| {
            self.task_interactions
                .question
                .as_ref()
                .map(|origin| origin.task_id.clone())
        })
    }

    /// The prefix a side question over `task` sends, and the cache it reads,
    /// once the session can run one. A task this process ran continues its
    /// own prefix. One restored from an earlier process has none, so it
    /// borrows the main session's.
    fn btw_prompt_for(&self, task: Option<&str>) -> Option<Arc<RequestPrefix>> {
        self.task_host.as_ref()?;
        if let Some(prefix) = task.and_then(|task| {
            self.context_store
                .as_ref()?
                .request_prefix(&ContextKey::task(task))
        }) {
            return Some(prefix);
        }
        let prompt = self.btw_prompt.as_ref()?.load_full();
        (!prompt.system.is_empty()).then(|| {
            Arc::new(prompt.prefix(CacheKey::session(&SessionRef::from(self.state.session.id))))
        })
    }

    /// The conversation a side question over `task` reads: an open task's as
    /// it commits, then a finished one's as it was kept.
    fn btw_source(&self, task: Option<&str>) -> Result<BtwSource, String> {
        let Some(task) = task else {
            return Ok(match &self.shared_history {
                Some(history) => BtwSource::Live(history.load_full()),
                None => BtwSource::Kept(Arc::default()),
            });
        };
        if let Some(mirror) = self
            .context_store
            .as_ref()
            .and_then(|store| store.live_history(&ContextKey::task(task)))
        {
            return Ok(BtwSource::Live(mirror.load_full()));
        }
        self.subagent_history
            .as_ref()
            .and_then(|store| {
                let snapshot = store.snapshot();
                snapshot
                    .records()
                    .get(task)
                    .map(|record| BtwSource::Kept(Arc::clone(record.messages())))
            })
            .ok_or_else(|| TASK_HISTORY_UNAVAILABLE.to_owned())
    }

    fn capture_btw_thread(&mut self, task: Option<&str>) -> Option<BtwThread> {
        let read = self
            .btw_source(task)
            .and_then(|source| Ok((source.messages()?, source.mark())));
        let (messages, mark) = match read {
            Ok(read) => read,
            Err(error) => {
                self.status_bar.flash(error);
                return None;
            }
        };
        let Some(prefix) = self.btw_prompt_for(task) else {
            self.status_bar.flash(PROMPT_INITIALIZING.into());
            return None;
        };
        Some(BtwThread::new(prefix, messages, mark))
    }

    /// The transcript of the conversation `task` names, while it is open.
    fn btw_chat(&mut self, task: Option<&str>) -> Option<&mut Chat> {
        let index = match task {
            None => 0,
            Some(task) => self.chat_for_task(task)?,
        };
        self.chats.get_mut(index)
    }

    fn install_btw_thread(&mut self, thread: BtwThread) {
        let Some(side) = self.btw.side.as_mut() else {
            return;
        };
        side.thread = Some(thread);
        let task = side.task.clone();
        self.mark_btw_cutoff(task.as_deref());
    }

    /// Streaming text is not in history yet and draws below every message,
    /// so a marker appended now lands exactly where the snapshot cuts.
    fn mark_btw_cutoff(&mut self, task: Option<&str>) {
        if let Some(chat) = self.btw_chat(task) {
            chat.remove_notice(BTW_CUTOFF_MARKER);
            chat.push(DisplayMessage::new(
                DisplayRole::Notice,
                BTW_CUTOFF_MARKER.into(),
            ));
        }
    }

    fn unmark_btw_cutoff(&mut self, task: Option<&str>) {
        if let Some(chat) = self.btw_chat(task) {
            chat.remove_notice(BTW_CUTOFF_MARKER);
        }
    }

    pub(crate) fn start_btw(&mut self, question: String) {
        self.open_btw(question.clone(), question, |_| true);
    }

    /// A `/btw` with a canned question, headed by a short label rather than
    /// the prompt, so the transcript does not repeat the whole prompt.
    pub(crate) fn start_catchup(&mut self, focus: &str) {
        let (header, question) = if focus.is_empty() {
            (CATCHUP_HEADER.to_owned(), CATCHUP_PROMPT.to_owned())
        } else {
            (
                format!("{CATCHUP_HEADER}: {focus}"),
                format!("{CATCHUP_PROMPT}{CATCHUP_FOCUS_PREFIX}{focus}"),
            )
        };
        self.open_btw(header, question, |thread| {
            thread
                .messages
                .iter()
                .any(|message| matches!(message.role, Role::User))
        });
    }

    /// Asks `question` in the thread the focused conversation kept, caught
    /// up with it, or in a new one. `answerable` says whether the thread's
    /// view of the conversation gives the question anything to answer from.
    fn open_btw(&mut self, header: String, question: String, answerable: fn(&BtwThread) -> bool) {
        let task = self.active_subagent_id().map(str::to_owned);
        self.close_stream_modal();
        self.close_btw();
        let resumed = self.resume_btw(task.clone());
        if resumed && self.btw.thread_mut().is_some() {
            if !self.catch_up_btw_thread() {
                return;
            }
        } else {
            let Some(thread) = self.capture_btw_thread(task.as_deref()) else {
                return;
            };
            if !resumed {
                let window = self.btw_window(TITLE, SideFooter::FollowUp);
                self.btw.side = Some(SideQuestion::new(task.clone(), window));
            }
            self.install_btw_thread(thread);
        }
        let Some(thread) = self.btw.thread_mut() else {
            return;
        };
        if !answerable(thread) {
            self.status_bar.flash(NOTHING_TO_CATCH_UP.into());
            if !resumed {
                self.close_btw();
                self.btw.parked.remove(&task);
            }
            return;
        }
        thread.ask(question);
        self.stream_btw_answer(header);
    }

    /// Asks the next question over the same thread, so it stays one cached
    /// lineage, unless the conversation moved on since, when the thread
    /// catches up with it first.
    pub(crate) fn continue_btw(&mut self, question: String) {
        let _ = self.poll_btw_run();
        let Some(side) = &self.btw.side else {
            return;
        };
        let clarifying = side.context.is_some();
        if side.thread.is_none() {
            let task = side.task.clone();
            let Some(thread) = self.capture_btw_thread(task.as_deref()) else {
                self.restore_btw_draft(&question);
                return;
            };
            self.install_btw_thread(thread);
        } else if !clarifying && !self.catch_up_btw_thread() {
            self.restore_btw_draft(&question);
            return;
        }
        let context = clarifying.then(|| self.question_form.clarification_context());
        let Some(thread) = self.btw.thread_mut() else {
            return;
        };
        match context {
            Some(context) => thread.ask_clarification(question.clone(), context),
            None => thread.ask(question.clone()),
        }
        self.stream_btw_answer(question);
    }

    fn restore_btw_draft(&mut self, question: &str) {
        if let Some(side) = self.btw.side.as_mut() {
            side.window.set_draft(question);
        }
    }

    /// Shows `header` as the question and starts the run that answers the
    /// thread's pending one.
    fn stream_btw_answer(&mut self, header: String) {
        let Some(host) = self.task_host.clone() else {
            self.flash(PROMPT_INITIALIZING.into());
            if let Some(thread) = self.btw.thread_mut() {
                thread.settle(None);
            }
            return;
        };
        self.renew_question_wait();
        let store = MediaStore::new(&self.storage);
        let btw = &mut self.btw;
        let Some(side) = btw.side.as_mut() else {
            return;
        };
        let Some(thread) = &side.thread else {
            return;
        };
        btw.runs += 1;
        let (stop, cancel) = CancelToken::new();
        let (reply_tx, reply) = flume::bounded(1);
        let asked = ask_aside(
            host,
            Arc::clone(&thread.prefix),
            thread.messages.clone(),
            store,
            EventSender::new(btw.sender.clone(), btw.runs),
            cancel,
            thread.question(),
        );
        smol::spawn(async move {
            let _ = reply_tx.send(asked.await);
        })
        .detach();
        btw.run = Some(AsideRun {
            id: btw.runs,
            usage: TokenUsage::default(),
            reply,
            _stop: stop,
        });
        if side.context.is_none() {
            side.window.set_footer(SideFooter::FollowUp);
        }
        side.window.chat_mut().show_user_message(header);
        side.window.begin_run();
    }

    /// The side question on the clock: a clarification whose form is gone
    /// goes with it, a finished run settles, and a window something else
    /// closed is closed properly.
    pub(super) fn tick_btw(&mut self) -> Dirty {
        if self.btw.is_clarifying() && !self.question_form.is_open() {
            self.discard_question_btw();
            return Dirty::YES;
        }
        let dirty = self.poll_btw_run();
        if self
            .btw
            .side
            .as_ref()
            .is_some_and(|side| !side.window.is_open() && side.context.is_none())
        {
            self.close_btw();
            return Dirty::YES;
        }
        dirty
            | self
                .btw
                .side
                .as_mut()
                .map_or(Dirty::NO, |side| side.window.tick())
    }

    /// Reads what the runs reported and settles the live one if it ended.
    /// Its reply is taken before its events are read: every event a run
    /// sends goes out before its reply, so none is left behind.
    fn poll_btw_run(&mut self) -> Dirty {
        let reply = self
            .btw
            .run
            .as_ref()
            .and_then(|run| match run.reply.try_recv() {
                Ok(reply) => Some(reply),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some((Vec::new(), Err(RUN_LOST.to_owned()))),
            });
        let dirty = self.drain_btw_events();
        match reply {
            Some(reply) => {
                self.settle_btw_run(reply);
                Dirty::YES
            }
            None => dirty,
        }
    }

    /// Spend is billed whichever run it came from; the rest is drawn only
    /// for the run the window shows. Approvals go to the permission prompt
    /// as a task's do.
    fn drain_btw_events(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Ok(envelope) = self.btw.events.try_recv() {
            dirty = Dirty::YES;
            let live = self
                .btw
                .run
                .as_mut()
                .filter(|run| run.id == envelope.run_id);
            match envelope.event {
                AgentEvent::ModelUsage {
                    usage,
                    cost,
                    billing,
                    provider,
                    model,
                    purpose,
                    account,
                } => {
                    if let Some(run) = live {
                        run.usage += usage;
                    }
                    self.settle_side_spend(
                        &provider,
                        account.as_deref(),
                        &model,
                        purpose,
                        usage,
                        cost,
                        billing,
                    );
                }
                AgentEvent::PermissionRequest(request) => {
                    if request.presentation.autopilot != Some(AutopilotNote::Deciding) {
                        self.make_way_for_permission_prompt();
                    }
                    let call_id = request.id.clone();
                    if self
                        .permission_prompt
                        .enqueue(request, Some(PERMISSION_ORIGIN.to_owned()))
                        && let Some(side) = self.btw.side.as_mut()
                    {
                        side.window.chat_mut().await_approval(&call_id);
                    }
                }
                AgentEvent::PermissionRequestUpdated(request) => {
                    let escalated = matches!(
                        request.presentation.autopilot,
                        Some(AutopilotNote::Escalated { .. })
                    );
                    if self.permission_prompt.update(request) && escalated {
                        self.make_way_for_permission_prompt();
                    }
                }
                AgentEvent::PermissionRequestResolved { request_id, .. } => {
                    self.permission_prompt.resolve_pending(&request_id);
                    if let Some(side) = self.btw.side.as_mut() {
                        side.window.chat_mut().approval_settled(&request_id);
                    }
                }
                event => {
                    if live.is_some()
                        && let Some(side) = self.btw.side.as_mut()
                    {
                        side.window.handle_event(event);
                    }
                }
            }
        }
        dirty
    }

    fn settle_btw_run(&mut self, (messages, done): AsideReply) {
        let Some(run) = self.btw.run.take() else {
            return;
        };
        let Some(side) = self.btw.side.as_mut() else {
            return;
        };
        let answered = match done {
            Ok(DoneReason::Cancelled) => None,
            Ok(_) => Some(messages),
            Err(error) => {
                side.window
                    .chat_mut()
                    .push(DisplayMessage::new(DisplayRole::Error, error));
                None
            }
        };
        if let Some(thread) = side.thread.as_mut() {
            thread.settle(answered);
        }
        side.window.finish_run(Some(run.usage));
        if let Some(question) = side.window.take_queued() {
            self.continue_btw(question);
        }
    }

    /// Stops the live run and says so where it was cut, unless it already
    /// finished, which settles as it would have on the next tick.
    pub(super) fn stop_btw_run(&mut self) {
        let _ = self.poll_btw_run();
        let Some(run) = self.btw.run.take() else {
            return;
        };
        let Some(side) = self.btw.side.as_mut() else {
            return;
        };
        if let Some(thread) = side.thread.as_mut() {
            thread.settle(None);
        }
        let chat = side.window.chat_mut();
        chat.flush();
        chat.cancel_in_progress();
        side.window.push_notice(STOPPED_NOTE);
        side.window.finish_run(Some(run.usage));
    }
}

#[cfg(test)]
const RUN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(test)]
const NO_RUN: &str = "a side question must be running";
#[cfg(test)]
const RUN_HUNG: &str = "the side question never finished";

#[cfg(test)]
impl App {
    /// Waits for the live run to end, then settles it as a tick would.
    pub(super) fn await_btw_run(&mut self) {
        self.await_btw_reply();
        let _ = self.poll_btw_run();
    }

    /// Waits for the live run to end and leaves it for the next poll, as a
    /// run that finished between two ticks is.
    pub(super) fn await_btw_reply(&mut self) {
        let run = self.btw.run.as_mut().expect(NO_RUN);
        let reply = run.reply.recv_timeout(RUN_DEADLINE).expect(RUN_HUNG);
        let (finished, held) = flume::bounded(1);
        let _ = finished.send(reply);
        run.reply = held;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use caudra_providers::{DOCUMENT_OMITTED_PREFIX, DocumentSource, RequestOptions};
    use caudra_storage::StateDir;
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::app::tests::{BtwScript, PendingProvider, btw_host, btw_reply, gated_provider};
    use crate::components::test_model;

    const Q: &str = "why sqlite?";
    const FOLLOW_UP: &str = "and why not postgres?";
    const ANSWER: &str = "it ships in the binary";
    const LOOKING: &str = "the config decides";
    const BASE: &str = "let's pick a database";
    const NEWER: &str = "we switched to postgres";
    const SYSTEM: &str = "system";
    const TOOL: &str = "file_read";
    const CALL: &str = "call-1";
    const CLARIFY_STORAGE: &str = "About: Q1/2 · Storage\nWhich database?\nSQLite\nPostgreSQL";
    const CLARIFY_DEPLOYMENT: &str = "About: Q2/2 · Deployment\nWhere should it run?\nLocal\nCloud";
    const PDF_CALL: &str = "fetch-pdf";
    const PDF_URL: &str = "https://example.com/paper.pdf";
    const PDF_DATA: &str = "JVBERi0xLjc=";
    const REBASE_MSG: &str =
        "a caught-up thread quotes its earlier exchanges in its first question";

    fn prefix() -> Arc<RequestPrefix> {
        Arc::new(RequestPrefix {
            provider: Arc::new(PendingProvider),
            model: test_model(),
            system: SYSTEM.into(),
            tools: json!([]),
            opts: RequestOptions::default(),
            cache_key: None,
        })
    }

    fn base() -> Vec<Message> {
        vec![Message::user(BASE.into())]
    }

    fn thread() -> BtwThread {
        BtwThread::new(prefix(), base(), HistoryMark::default())
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        }
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }

    /// The history a run leaves when it reads a file before it answers.
    fn answered(thread: &BtwThread, question: &str) -> Vec<Message> {
        let mut messages = thread.messages.clone();
        messages.extend([
            Message::user(question.into()),
            assistant(vec![
                text(LOOKING),
                ContentBlock::tool_use(CALL, TOOL, json!({})),
            ]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: CALL.into(),
                    content: LOOKING.into(),
                    is_error: false,
                    output_ref: None,
                }],
                ..Default::default()
            },
            assistant(vec![text(ANSWER)]),
        ]);
        messages
    }

    fn settle_answered(thread: &mut BtwThread) {
        let question = thread.question();
        let messages = answered(thread, &question);
        thread.settle(Some(messages));
    }

    #[test_case(false ; "same_question")]
    #[test_case(true ; "different_question")]
    fn clarification_context_follows_focus_without_repeating_unchanged_questions(changed: bool) {
        let mut thread = thread();
        thread.ask_clarification(Q.into(), CLARIFY_STORAGE.into());
        let initial = thread.question();
        assert!(initial.contains(CLARIFICATION_REMINDER));
        assert!(initial.contains(CLARIFY_STORAGE));
        assert!(initial.ends_with(Q));
        settle_answered(&mut thread);

        let context = if changed {
            CLARIFY_DEPLOYMENT
        } else {
            CLARIFY_STORAGE
        };
        thread.ask_clarification(FOLLOW_UP.into(), context.into());
        let follow_up = thread.question();
        if changed {
            assert!(follow_up.contains(CLARIFY_DEPLOYMENT));
        } else {
            assert_eq!(follow_up, FOLLOW_UP);
        }
    }

    #[test_case(false ; "interrupted")]
    #[test_case(true ; "failed")]
    fn clarification_retry_keeps_context_that_never_settled(failed: bool) {
        let mut thread = thread();
        thread.ask_clarification(Q.into(), CLARIFY_STORAGE.into());
        settle_answered(&mut thread);
        thread.ask_clarification(FOLLOW_UP.into(), CLARIFY_DEPLOYMENT.into());
        if failed {
            thread.settle(None);
        }
        thread.ask_clarification(FOLLOW_UP.into(), CLARIFY_DEPLOYMENT.into());
        assert!(thread.question().contains(CLARIFY_DEPLOYMENT));
        assert_eq!(thread.exchange_count(), 1);
    }

    /// The thread keeps what the run did, tool turns included, so the next
    /// question extends the same lineage and the provider reads it cached.
    #[test]
    fn an_answer_files_the_whole_run_and_the_follow_up_extends_it() {
        let mut thread = thread();
        thread.ask(Q.into());
        let run = answered(&thread, Q);
        thread.settle(Some(run.clone()));
        thread.ask(FOLLOW_UP.into());

        assert_eq!(
            serde_json::to_value(thread.messages()).unwrap(),
            serde_json::to_value(&run).unwrap()
        );
        assert_eq!(thread.exchange_count(), 1);
        assert_eq!(thread.question(), FOLLOW_UP, "a follow-up is sent as typed");
    }

    #[test]
    fn an_unanswered_question_leaves_the_thread() {
        let mut thread = thread();
        thread.ask(Q.into());
        thread.settle(None);
        assert_eq!(thread.exchange_count(), 0);
        assert!(thread.pending().is_none());
        assert_eq!(thread.base_texts(), [BASE], "only the base remains");
    }

    #[test]
    fn a_question_asked_over_a_failed_one_replaces_it() {
        let mut thread = thread();
        thread.ask(Q.into());
        thread.ask(FOLLOW_UP.into());
        assert_eq!(thread.pending(), Some(FOLLOW_UP));
        assert_eq!(thread.question(), FOLLOW_UP);
    }

    fn rebased(exchanges: usize) -> BtwThread {
        let mut thread = thread();
        for _ in 0..exchanges {
            thread.ask(Q.into());
            settle_answered(&mut thread);
        }
        thread.rebase(
            prefix(),
            vec![Message::user(BASE.into()), Message::user(NEWER.into())],
            HistoryMark {
                epoch: None,
                len: 2,
            },
        );
        thread.ask(FOLLOW_UP.into());
        thread
    }

    #[test]
    fn a_rebased_thread_quotes_its_earlier_exchanges_over_the_new_base() {
        let thread = rebased(1);
        assert_eq!(thread.base_texts(), [BASE, NEWER], "{REBASE_MSG}");
        let question = thread.question();
        assert!(question.starts_with(REBASE_REMINDER), "{REBASE_MSG}");
        assert!(question.contains(&format!("{QUESTION_OPEN}{Q}{QUESTION_CLOSE}")));
        assert!(question.contains(&format!("{ANSWER_OPEN}{ANSWER}{ANSWER_CLOSE}")));
        assert!(
            !question.contains(LOOKING),
            "a quote keeps the final answer alone"
        );
        assert!(question.ends_with(FOLLOW_UP));
        assert!(!question.contains(OMITTED_SUFFIX));
    }

    #[test]
    fn the_quote_rides_only_the_first_question_after_a_rebase() {
        let mut thread = rebased(1);
        settle_answered(&mut thread);
        thread.ask(Q.into());
        assert_eq!(thread.question(), Q);
        assert_eq!(thread.quoted_count(), 1);
    }

    #[test]
    fn a_long_thread_quotes_only_its_latest_exchanges() {
        let extra = 2;
        let thread = rebased(MAX_REPLAYED_EXCHANGES + extra);
        let question = thread.question();
        assert_eq!(thread.quoted_count(), MAX_REPLAYED_EXCHANGES);
        assert!(question.contains(&format!("{OMITTED_PREFIX}{extra}{OMITTED_SUFFIX}")));
        assert_eq!(
            question.matches(QUESTION_OPEN).count(),
            MAX_REPLAYED_EXCHANGES
        );
    }

    #[test]
    fn the_final_answer_is_the_last_assistant_text() {
        let thread = thread();
        let mut messages = answered(&thread, Q);
        assert_eq!(final_answer(&messages), ANSWER);
        messages.push(assistant(vec![ContentBlock::tool_use(
            CALL,
            TOOL,
            json!({}),
        )]));
        assert_eq!(
            final_answer(&messages),
            ANSWER,
            "a turn without text says nothing"
        );
        assert_eq!(final_answer(&[]), "");
    }

    /// A resumed session holds only a reference to a PDF. The run loads it
    /// before it sends, so the PDF goes out attached while its file exists,
    /// and as a note to fetch it again only once the file is gone.
    #[test_case(true ; "stored_file_attaches")]
    #[test_case(false ; "missing_file_becomes_a_note")]
    fn a_restored_pdf_is_loaded_before_the_side_question_sends(file_exists: bool) {
        let temp = TempDir::new().unwrap();
        let store = MediaStore::new(&StateDir::from_path(temp.path().to_path_buf()));
        let mut document = DocumentSource {
            url: PDF_URL.into(),
            filename: None,
            page_count: 1,
            stored: None,
            data: Some(Arc::from(PDF_DATA)),
        };
        document.store_in(&store).unwrap();
        document.data = None;
        if !file_exists {
            fs::remove_file(store.path(&document.stored.as_ref().unwrap().sha256)).unwrap();
        }
        let mut result = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: PDF_CALL.into(),
                content: ANSWER.into(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        };
        result
            .tool_result_documents
            .insert(PDF_CALL.into(), vec![document]);
        let messages = vec![
            Message::user(BASE.into()),
            assistant(vec![ContentBlock::tool_use(PDF_CALL, TOOL, json!({}))]),
            result,
        ];
        let (
            provider,
            BtwScript {
                responses,
                requests,
            },
        ) = gated_provider();
        let mut model = test_model();
        model.supports_pdf = true;
        let prefix = Arc::new(RequestPrefix {
            provider,
            model,
            system: SYSTEM.into(),
            tools: json!([]),
            opts: RequestOptions::default(),
            cache_key: None,
        });
        responses.send(btw_reply(ANSWER)).unwrap();
        let (events, _received) = flume::unbounded();
        let (_stop, cancel) = CancelToken::new();

        let (_, done) = smol::block_on(ask_aside(
            btw_host(),
            prefix,
            messages,
            store,
            EventSender::new(events, 0),
            cancel,
            Q.into(),
        ));

        assert_eq!(done, Ok(DoneReason::EndTurn));
        let sent = requests.try_recv().unwrap().messages;
        let attached: Vec<_> = sent
            .iter()
            .flat_map(Message::result_documents)
            .map(|document| document.data.as_deref())
            .collect();
        let noted = sent.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(block, ContentBlock::ToolResult { content, .. }
                    if content.contains(DOCUMENT_OMITTED_PREFIX))
            })
        });
        if file_exists {
            assert_eq!(attached, [Some(PDF_DATA)]);
        } else {
            assert!(attached.is_empty());
        }
        assert_eq!(noted, !file_exists);
    }
}
