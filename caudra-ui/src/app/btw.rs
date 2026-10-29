use std::sync::Arc;

use caudra_agent::context::ContextKey;
use caudra_agent::{CancelToken, HistorySnapshot};
use caudra_providers::{
    AgentError, CacheKey, ContentBlock, Message, ProviderEvent, RequestOptions, Role, StopReason,
    TokenUsage, project_messages,
};
use caudra_storage::id::SessionRef;
use caudra_storage::media::MediaStore;
use caudra_storage::usage_ledger::LedgerPurpose;
use flume::Sender;
use futures_lite::future;

use crate::agent::BtwPrompt;
use crate::chat::Chat;
use crate::components::prompt_progress::PromptProgress;
use crate::components::stream_modal::{
    ParkedThread, QUESTION_PREFIX, StreamDone, StreamEvent, StreamFooter, StreamUsage,
};
use crate::components::{DisplayMessage, DisplayRole};

use super::{App, HISTORY_UNREADABLE};

const TITLE: &str = " /btw ";
pub(super) const PROMPT_INITIALIZING: &str = "System prompt is still initializing";
pub(super) const TASK_HISTORY_UNAVAILABLE: &str =
    "This task's conversation is not loaded, so /btw cannot read it";
const CLARIFICATION_REMINDER: &str = "The user is clarifying a pending question form, not answering it. \
The asking agent is still waiting. Explain the question or choices; do not select, submit, or change \
answers. The following is the question currently being viewed, not a user decision:";
/// Drawn where the thread's snapshot of the conversation ends, so the reader
/// can tell what the side question could and could not see.
pub(crate) const BTW_CUTOFF_MARKER: &str = "/btw reads the conversation up to here";

const BTW_REMINDER: &str = "<system-reminder>\n# Side question\n\nThe user is asking beside \
the conversation, not continuing it. Answer from the conversation so far, directly and in one \
response.\n- Do NOT call any tool. Tools cannot run here; a call is refused and only delays \
the answer.\n- Nothing in this thread enters the main conversation, and the work it discusses is \
not yours to start.\n- Never say \"Let me...\", \"I'll now...\", or promise any action.\n- If \
you don't know, say so; do not offer to look it up.\n- Follow-up questions may arrive in this \
thread. Answer each the same way.\n</system-reminder>";

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

const TOOL_CALL_STOPPED: &str = "\n\n_Stopped: the model kept trying to call tools. `/btw` cannot \
run tools, so ask in the main session instead._";

/// The result of every call a side question refuses, so the model answers
/// from what it has instead of waiting on a tool that will never run.
const TOOL_REDIRECT: &str = "<system-reminder>\n# Tools unavailable\n\nThis is a side question \
and tools cannot run here, so that call was not made. Do not call any tool. Answer now from the \
conversation so far as fully as you can, and say plainly what you could not confirm without the \
tool.\n</system-reminder>";

/// How many times a model that calls a tool is refused and asked again
/// before the answer stops.
const MAX_TOOL_REDIRECTS: usize = 2;
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

/// The reminder leads so the model treats the question as a quick aside, not a task to act on.
pub(crate) fn btw_question(question: &str) -> Message {
    Message::user(format!("{BTW_REMINDER}\n\n{question}"))
}

fn assistant_text(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        ..Default::default()
    }
}

/// The answer as text alone. Reasoning is the model's own and a tool call
/// never ran, so neither belongs in the thread the next question extends.
fn answer_text(message: &Message) -> Option<String> {
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

/// Answers every call `message` made with a refusal that tells the model to
/// answer without it, which keeps each call paired with a result.
fn refuse_tool_calls(message: &Message) -> Message {
    Message {
        role: Role::User,
        content: message
            .tool_uses()
            .map(|(id, _, _)| ContentBlock::ToolResult {
                tool_use_id: id.to_owned(),
                content: TOOL_REDIRECT.into(),
                is_error: true,
                output_ref: None,
            })
            .collect(),
        ..Default::default()
    }
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

/// A side conversation forked from the main one: the prefix and history it
/// opened over, pinned, and the text-only exchanges asked on top of them.
/// Nothing here is persisted. Closing the modal keeps it until it is cleared
/// or the session changes.
pub(crate) struct BtwThread {
    prompt: Arc<BtwPrompt>,
    cache_key: Option<CacheKey>,
    /// The task whose conversation this forks, `None` for the main one.
    task: Option<String>,
    base: Vec<Message>,
    mark: HistoryMark,
    /// Exchanges asked over an earlier base, quoted rather than replayed.
    earlier: Vec<(String, String)>,
    /// Earlier exchanges too old to quote.
    omitted: usize,
    exchanges: Vec<(String, String)>,
    pending: Option<String>,
    clarification_context: Option<String>,
}

/// A thread whose modal was closed, with what the modal showed of it.
pub(crate) struct ParkedBtw {
    thread: BtwThread,
    display: ParkedThread,
}

/// Exactly what the live request sends for `base`, so the provider can reuse
/// the cached prefix instead of re-reading the whole history as fresh input
/// tokens. That includes answering a call the mirror caught mid-turn, which a
/// provider would otherwise reject. The base is projected per request, after
/// its stored media is loaded, so a PDF a restored session holds only a
/// reference to goes out attached instead of as a note to fetch it again.
fn request(prompt: &BtwPrompt, base: &[Message], questions: Vec<Message>) -> Vec<Message> {
    let mut messages = caudra_agent::project_request(
        base,
        &prompt.tools,
        &prompt.model,
        prompt.provider.reasoning_transport(&prompt.model),
    )
    .into_owned();
    messages.extend(questions);
    messages
}

impl BtwThread {
    fn new(prompt: Arc<BtwPrompt>, history: Vec<Message>) -> Self {
        Self {
            prompt,
            cache_key: None,
            task: None,
            base: history,
            mark: HistoryMark::default(),
            earlier: Vec::new(),
            omitted: 0,
            exchanges: Vec::new(),
            pending: None,
            clarification_context: None,
        }
    }

    /// Moves the thread onto the conversation as it stands now. The answers
    /// so far were given over the old base, so they become quoted context
    /// for the next question instead of turns the model would extend.
    fn rebase(
        &mut self,
        (prompt, cache_key): (Arc<BtwPrompt>, Option<CacheKey>),
        base: Vec<Message>,
        mark: HistoryMark,
    ) {
        self.earlier.append(&mut self.exchanges);
        let excess = self.earlier.len().saturating_sub(MAX_REPLAYED_EXCHANGES);
        self.earlier.drain(..excess);
        self.omitted += excess;
        self.prompt = prompt;
        self.cache_key = cache_key;
        self.base = base;
        self.mark = mark;
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

    /// A question that never settled is replaced rather than kept: the request
    /// that carried it failed, so the model never answered it.
    fn ask(&mut self, question: String) {
        self.pending = Some(question);
    }

    fn ask_clarification(&mut self, question: String, context: String) {
        let changed = self.clarification_context.as_ref() != Some(&context)
            || self.pending.is_some()
            || self.exchanges.is_empty();
        let question = if changed {
            format!("{CLARIFICATION_REMINDER}\n\n{context}\n\n{question}")
        } else {
            question
        };
        self.clarification_context = Some(context);
        self.ask(question);
    }

    #[cfg(test)]
    pub(super) fn exchange_count(&self) -> usize {
        self.exchanges.len()
    }

    #[cfg(test)]
    pub(super) fn quoted_count(&self) -> usize {
        self.earlier.len()
    }

    #[cfg(test)]
    pub(super) fn pending(&self) -> Option<&str> {
        self.pending.as_deref()
    }

    /// The text of every message the thread opened over.
    #[cfg(test)]
    pub(super) fn base_texts(&self) -> Vec<String> {
        request(&self.prompt, &self.base, Vec::new())
            .iter()
            .filter_map(answer_text)
            .collect()
    }

    /// The model the thread asks and the cache it reads.
    #[cfg(test)]
    pub(super) fn route(&self) -> (String, Option<&CacheKey>) {
        (self.prompt.model.spec(), self.cache_key.as_ref())
    }

    /// Files the pending question with its answer. No answer, which is what a
    /// refused or tool-calling reply leaves, drops the question instead, so the
    /// thread stays a strict alternation the provider accepts.
    fn settle(&mut self, answer: Option<String>) {
        let Some(question) = self.pending.take() else {
            return;
        };
        if let Some(answer) = answer {
            self.exchanges.push((question, answer));
        } else {
            self.clarification_context = None;
        }
    }

    #[cfg(test)]
    fn request_messages(&self) -> Vec<Message> {
        request(&self.prompt, &self.base, self.questions())
    }

    /// The reminder rides the first question only: a later block would say
    /// nothing new and would move the cache boundary for every follow-up.
    /// After a rebase the quoted earlier exchanges ride it too.
    fn questions(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        let digest = self.digest();
        let questions = self
            .exchanges
            .iter()
            .map(|(question, answer)| (question.as_str(), Some(answer.as_str())))
            .chain(self.pending.as_deref().map(|question| (question, None)));
        for (index, (question, answer)) in questions.enumerate() {
            messages.push(if index == 0 {
                match &digest {
                    Some(digest) => btw_question(&format!("{digest}{ROUND_SEPARATOR}{question}")),
                    None => btw_question(question),
                }
            } else {
                Message::user(question.to_owned())
            });
            if let Some(answer) = answer {
                messages.push(assistant_text(answer));
            }
        }
        messages
    }
}

impl App {
    pub(super) fn open_question_btw(&mut self) {
        if !self.question_form.is_open() {
            return;
        }
        if self
            .btw_prompt_for(self.question_asker().as_deref())
            .is_none()
        {
            self.flash(PROMPT_INITIALIZING.into());
            return;
        }
        if !self.stream_modal.is_clarification() {
            self.close_stream_modal();
        } else {
            self.settle_stream_modal();
        }
        self.autoscroll = None;
        self.selection_state = None;
        self.stream_modal
            .open_clarification(self.question_form.clarification_context());
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
        self.autoscroll = None;
        self.selection_state = None;
        if !self.resume_btw(task) {
            self.stream_modal.open_ask();
        }
    }

    pub(super) fn discard_question_btw(&mut self) {
        if self.stream_modal.is_clarification() {
            self.close_stream_modal();
        }
    }

    /// Closes whatever the modal shows. A side thread is kept for its
    /// conversation; a clarification and `/extract` are not.
    pub(super) fn close_stream_modal(&mut self) {
        let keep = self.btw_thread.is_some() && !self.stream_modal.is_clarification();
        if keep && self.stream_modal.is_open() {
            self.stream_modal.park();
        } else {
            let _ = self.stream_modal.poll();
        }
        self.settle_stream_modal();
        self.stream_modal.close();
        self.end_btw_thread(keep);
    }

    /// `Ctrl+L`: forgets the thread the modal shows and leaves an empty
    /// `/btw` in its place.
    pub(super) fn clear_btw_thread(&mut self) {
        let _ = self.stream_modal.poll();
        self.settle_stream_modal();
        let task = match &self.btw_thread {
            Some(thread) => thread.task.clone(),
            None => self.active_subagent_id().map(str::to_owned),
        };
        self.stream_modal.close();
        self.end_btw_thread(false);
        self.parked_btw.remove(&task);
        self.stream_modal.open_ask();
    }

    /// Forgets every kept thread, for a session that is going away.
    pub(super) fn discard_btw_threads(&mut self) {
        self.end_btw_thread(false);
        self.parked_btw.clear();
        self.stream_modal.forget_parked_draft();
    }

    /// Reopens the thread `task`'s conversation kept, if it kept one. One
    /// whose conversation has moved on says so and gets no transcript marker
    /// until its next question catches it up.
    fn resume_btw(&mut self, task: Option<String>) -> bool {
        let Some(parked) = self.parked_btw.remove(&task) else {
            return false;
        };
        let Ok(source) = self.btw_source(task.as_deref()) else {
            return false;
        };
        let current = source.mark() == parked.thread.mark;
        self.stream_modal.resume(parked.display);
        if current {
            self.install_btw_thread(parked.thread);
        } else {
            self.stream_modal.mark_moved_on();
            self.btw_thread = Some(parked.thread);
        }
        true
    }

    /// Moves the open thread onto its conversation as it stands now, if that
    /// moved on since the thread read it. False, with the reason flashed,
    /// when the conversation can no longer be read.
    fn catch_up_btw_thread(&mut self) -> bool {
        let Some(task) = self.btw_thread.as_ref().map(|thread| thread.task.clone()) else {
            return true;
        };
        let caught_up = self.btw_source(task.as_deref()).and_then(|source| {
            let mark = source.mark();
            if self
                .btw_thread
                .as_ref()
                .is_some_and(|thread| thread.mark == mark)
            {
                return Ok(None);
            }
            let route = self
                .btw_prompt_for(task.as_deref())
                .ok_or_else(|| PROMPT_INITIALIZING.to_owned())?;
            Ok(Some((route, source.messages()?, mark)))
        });
        let (route, base, mark) = match caught_up {
            Ok(None) => return true,
            Ok(Some(caught_up)) => caught_up,
            Err(error) => {
                self.status_bar.flash(error);
                return false;
            }
        };
        if let Some(thread) = self.btw_thread.as_mut() {
            thread.rebase(route, base, mark);
        }
        self.stream_modal.mark_moved_on();
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

    /// The prefix a side question over `task` sends, and the cache it reads.
    /// A task this process ran continues its own prefix. One restored from an
    /// earlier process has none, so it borrows the main session's.
    fn btw_prompt_for(&self, task: Option<&str>) -> Option<(Arc<BtwPrompt>, Option<CacheKey>)> {
        if let Some(prefix) = task.and_then(|task| {
            self.context_store
                .as_ref()?
                .request_prefix(&ContextKey::task(task))
        }) {
            return Some((
                Arc::new(BtwPrompt::from(prefix.as_ref())),
                prefix.cache_key.clone(),
            ));
        }
        let prompt = self.btw_prompt.as_ref()?.load_full();
        (!prompt.system.is_empty()).then(|| {
            let key = CacheKey::session(&SessionRef::from(self.state.session.id));
            (prompt, Some(key))
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

    fn capture_btw_thread(&mut self, task: Option<String>) -> Option<BtwThread> {
        let read = self
            .btw_source(task.as_deref())
            .and_then(|source| Ok((source.messages()?, source.mark())));
        let (messages, mark) = match read {
            Ok(read) => read,
            Err(error) => {
                self.status_bar.flash(error);
                return None;
            }
        };
        let Some((prompt, cache_key)) = self.btw_prompt_for(task.as_deref()) else {
            self.status_bar.flash(PROMPT_INITIALIZING.into());
            return None;
        };
        let mut thread = BtwThread::new(prompt, messages);
        thread.cache_key = cache_key;
        thread.task = task;
        thread.mark = mark;
        Some(thread)
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
        let task = thread.task.clone();
        self.btw_thread = Some(thread);
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

    pub(crate) fn start_btw(&mut self, question: String) {
        self.open_btw(format!("{QUESTION_PREFIX}{question}"), question, |_| true);
    }

    /// A `/btw` with a canned question, headed by a short label rather than
    /// the prompt, so copying the thread does not repeat the whole prompt.
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
                .base
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
        if self.resume_btw(task.clone()) {
            if !self.catch_up_btw_thread() {
                return;
            }
            if !self.btw_thread.as_ref().is_some_and(answerable) {
                self.status_bar.flash(NOTHING_TO_CATCH_UP.into());
                return;
            }
            if let Some(thread) = self.btw_thread.as_mut() {
                thread.ask(question);
            }
            self.stream_btw_answer(header);
            return;
        }
        let Some(mut thread) = self.capture_btw_thread(task) else {
            return;
        };
        if !answerable(&thread) {
            self.status_bar.flash(NOTHING_TO_CATCH_UP.into());
            return;
        }
        self.renew_question_wait();
        thread.ask(question);
        self.install_btw_thread(thread);

        let (tx, rx) = flume::bounded(64);
        let (trigger, cancel) = CancelToken::new();
        self.stream_modal
            .open(TITLE, header, StreamFooter::FollowUp, rx, trigger);
        self.spawn_btw(tx, cancel);
    }

    /// Asks the next question over the same snapshot and prefix, so the whole
    /// thread stays one cached lineage, unless the conversation moved on
    /// since, when the thread catches up with it first.
    pub(crate) fn continue_btw(&mut self, question: String) {
        self.settle_stream_modal();
        let context = self
            .stream_modal
            .is_clarification()
            .then(|| self.question_form.clarification_context());
        if self.btw_thread.is_none() {
            let asker = if context.is_some() {
                self.question_asker()
            } else {
                self.active_subagent_id().map(str::to_owned)
            };
            let Some(thread) = self.capture_btw_thread(asker) else {
                self.stream_modal.handle_paste(&question);
                return;
            };
            self.install_btw_thread(thread);
        } else if context.is_none() && !self.catch_up_btw_thread() {
            self.stream_modal.handle_paste(&question);
            return;
        }
        let header = format!("{QUESTION_PREFIX}{question}");
        let Some(thread) = self.btw_thread.as_mut() else {
            return;
        };
        match context {
            Some(context) => thread.ask_clarification(question, context),
            None => thread.ask(question),
        }
        self.stream_btw_answer(header);
    }

    fn stream_btw_answer(&mut self, header: String) {
        self.renew_question_wait();
        let (tx, rx) = flume::bounded(64);
        let (trigger, cancel) = CancelToken::new();
        self.stream_modal.begin_exchange(header, rx, trigger);
        self.spawn_btw(tx, cancel);
    }

    fn spawn_btw(&self, tx: Sender<StreamEvent>, cancel: CancelToken) {
        let Some(thread) = &self.btw_thread else {
            return;
        };
        // A btw forks a conversation, so it shares that conversation's cache key on purpose.
        smol::spawn(run_btw_over(
            Arc::clone(&thread.prompt),
            thread.base.clone(),
            thread.questions(),
            MediaStore::new(&self.storage),
            tx,
            thread.cache_key.clone(),
            cancel,
        ))
        .detach();
    }

    pub(crate) fn settle_btw(&mut self, answer: Option<String>) {
        if let Some(thread) = self.btw_thread.as_mut() {
            thread.settle(answer);
        }
    }

    /// Takes the open thread and its marker out of the transcript, keeping
    /// the thread for its conversation when `keep` says so and the modal set
    /// its exchanges aside. Every way the modal closes reaches here through
    /// the tick, so no close path has to know about the marker.
    pub(crate) fn end_btw_thread(&mut self, keep: bool) {
        let display = self.stream_modal.take_parked();
        let Some(mut thread) = self.btw_thread.take() else {
            return;
        };
        if let Some(chat) = self.btw_chat(thread.task.as_deref()) {
            chat.remove_notice(BTW_CUTOFF_MARKER);
        }
        if keep && let Some(display) = display {
            thread.settle(None);
            self.parked_btw
                .insert(thread.task.clone(), ParkedBtw { thread, display });
        }
    }
}

/// Loads what the snapshot holds only references to off the UI thread, then
/// asks over it.
async fn run_btw_over(
    prompt: Arc<BtwPrompt>,
    mut base: Vec<Message>,
    questions: Vec<Message>,
    store: MediaStore,
    btw_tx: Sender<StreamEvent>,
    cache_key: Option<CacheKey>,
    cancel: CancelToken,
) {
    caudra_agent::load_stored_media(&mut base, &store).await;
    let messages = request(&prompt, &base, questions);
    run_btw(prompt, messages, btw_tx, cache_key, cancel).await;
}

async fn run_btw(
    prompt: Arc<BtwPrompt>,
    messages: Vec<Message>,
    btw_tx: Sender<StreamEvent>,
    cache_key: Option<CacheKey>,
    cancel: CancelToken,
) {
    // btw bypasses `stream_with_retry`, the only other place options meet a model, so it has to
    // clamp for itself or it sends options the live request would have gated away.
    let opts = prompt.opts.clamped(&prompt.model);
    let (event_tx, event_rx) = flume::unbounded();

    let forwarder = smol::spawn({
        let btw_tx = btw_tx.clone();
        async move {
            while let Ok(event) = event_rx.recv_async().await {
                let forwarded = match event {
                    ProviderEvent::TextDelta { text } => StreamEvent::TextDelta(text),
                    ProviderEvent::ThinkingDelta { text } => StreamEvent::ThinkingDelta(text),
                    ProviderEvent::PromptProgress {
                        processed,
                        total,
                        cache,
                    } => StreamEvent::Progress(PromptProgress {
                        processed,
                        total,
                        cache,
                    }),
                    _ => continue,
                };
                if btw_tx.send(forwarded).is_err() {
                    return;
                }
            }
        }
    });

    let result = future::race(
        answer_without_tools(&prompt, messages, &event_tx, &opts, cache_key.as_ref()),
        async {
            cancel.cancelled().await;
            Err(AgentError::Cancelled)
        },
    )
    .await;
    drop(event_tx);
    forwarder.await;

    match result {
        Ok(answer) => {
            if answer.stopped {
                let _ = btw_tx.send(StreamEvent::TextDelta(TOOL_CALL_STOPPED.into()));
            }
            let model = &prompt.model;
            let _ = btw_tx.send(StreamEvent::Done(StreamDone {
                usage: StreamUsage {
                    cost: answer.cost,
                    billing: model.billing,
                    usage: answer.usage,
                    model: model.id.clone(),
                    provider: model.provider.to_string(),
                    purpose: LedgerPurpose::Btw,
                },
                answer: (!answer.texts.is_empty()).then(|| answer.texts.join(ROUND_SEPARATOR)),
            }));
        }
        // The receiver is already gone, which is what cancelled the stream.
        Err(AgentError::Cancelled) => {}
        Err(error) => {
            let _ = btw_tx.send(StreamEvent::Error(error.to_string()));
        }
    }
}

/// Every round's text and spend, and whether the model was still calling a
/// tool when its redirects ran out.
struct BtwAnswer {
    texts: Vec<String>,
    usage: TokenUsage,
    cost: Option<f64>,
    stopped: bool,
}

/// Streams the answer, refusing each tool call and asking again, so a model
/// that reaches for a tool still answers from what it already knows. Each
/// round is priced on its own: a tiered price depends on one request's size.
async fn answer_without_tools(
    prompt: &BtwPrompt,
    mut messages: Vec<Message>,
    event_tx: &Sender<ProviderEvent>,
    opts: &RequestOptions,
    cache_key: Option<&CacheKey>,
) -> Result<BtwAnswer, AgentError> {
    let mut answer = BtwAnswer {
        texts: Vec::new(),
        usage: TokenUsage::default(),
        cost: Some(0.0),
        stopped: false,
    };
    for round in 0..=MAX_TOOL_REDIRECTS {
        let response = prompt
            .provider
            .stream_message(
                &prompt.model,
                &messages,
                &prompt.system,
                &prompt.tools,
                event_tx,
                opts.clone(),
                cache_key,
            )
            .await?;
        answer.usage += response.usage;
        answer.cost = answer
            .cost
            .zip(prompt.model.billed_cost(&response.usage, opts.fast))
            .map(|(total, cost)| total + cost);
        let text = answer_text(&response.message);
        let streamed_text = text.is_some();
        answer.texts.extend(text);
        if response.stop_reason != Some(StopReason::ToolUse) {
            return Ok(answer);
        }
        let refusal = refuse_tool_calls(&response.message);
        if round == MAX_TOOL_REDIRECTS || refusal.content.is_empty() {
            break;
        }
        messages.push(response.message);
        messages.push(refusal);
        if streamed_text {
            event_tx.send(ProviderEvent::TextDelta {
                text: ROUND_SEPARATOR.into(),
            })?;
        }
    }
    answer.stopped = true;
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::Mutex;

    use arc_swap::ArcSwap;
    use caudra_agent::UNAVAILABLE_RESULT;
    use caudra_providers::provider::{BoxFuture, Provider};
    use caudra_providers::{
        DOCUMENT_OMITTED_PREFIX, DocumentSource, Model, ModelInfo, StreamResponse,
    };
    use caudra_storage::StateDir;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const Q: &str = "why sqlite?";
    const FOLLOW_UP: &str = "and why not postgres?";
    const ANSWER: &str = "it ships in the binary";
    const BASE: &str = "let's pick a database";
    const FIRST_MODEL: &str = "anthropic/claude-sonnet-4-20250514";
    const SECOND_MODEL: &str = "openai/gpt-5.4";
    const SYSTEM: &str = "system";
    const PREFILL_PROCESSED: u32 = 1_200;
    const PREFILL_TOTAL: u32 = 4_000;
    const PREFILL_CACHE: u32 = 900;
    const TOOL: &str = "file_read";
    const ORPHANED_CALL: &str = "orphaned";
    const UNANSWERED_CALL: &str = "unanswered";
    const OPEN_CALLS: [&str; 2] = ["open-first", "open-second"];
    const THINKING_DELTAS: [&str; 2] = ["considering ", "deployment"];
    const TEXT_DELTAS: [&str; 2] = ["it ships ", "in the binary"];
    const EXPECTED_DONE: &str = "expected stream completion after all deltas";
    const CLARIFY_STORAGE: &str = "About: Q1/2 · Storage\nWhich database?\nSQLite\nPostgreSQL";
    const CLARIFY_DEPLOYMENT: &str = "About: Q2/2 · Deployment\nWhere should it run?\nLocal\nCloud";
    const LOOKING: &str = "the config decides";
    const PDF_CALL: &str = "fetch-pdf";
    const PDF_URL: &str = "https://example.com/paper.pdf";
    const PDF_DATA: &str = "JVBERi0xLjc=";
    const ROUND_USAGE: TokenUsage = TokenUsage {
        input: 1_000,
        output: 50,
        cache_creation: 0,
        cache_read: 200,
    };

    #[test_case(false ; "same_question")]
    #[test_case(true ; "different_question")]
    fn clarification_context_follows_focus_without_repeating_unchanged_questions(changed: bool) {
        let (called, _requests) = flume::unbounded();
        let mut thread = BtwThread::new(
            Arc::new(prompt(FIRST_MODEL, called)),
            vec![Message::user(BASE.into())],
        );
        thread.ask_clarification(Q.into(), CLARIFY_STORAGE.into());
        let initial = thread.request_messages();
        let initial_question = user_text(initial.last().unwrap());
        assert!(initial_question.contains(CLARIFICATION_REMINDER));
        assert!(initial_question.contains(CLARIFY_STORAGE));
        assert!(initial_question.ends_with(Q));
        thread.settle(Some(ANSWER.into()));

        let context = if changed {
            CLARIFY_DEPLOYMENT
        } else {
            CLARIFY_STORAGE
        };
        thread.ask_clarification(FOLLOW_UP.into(), context.into());
        let next = thread.request_messages();
        assert_eq!(user_text(&next[0]), BASE);
        assert_eq!(user_text(&next[1]), initial_question);
        let follow_up = user_text(next.last().unwrap());
        if changed {
            assert!(follow_up.contains(CLARIFY_DEPLOYMENT));
        } else {
            assert_eq!(follow_up, FOLLOW_UP);
        }
    }

    #[test_case(false ; "interrupted")]
    #[test_case(true ; "failed")]
    fn clarification_retry_keeps_context_that_never_settled(failed: bool) {
        let (called, _requests) = flume::unbounded();
        let mut thread = BtwThread::new(Arc::new(prompt(FIRST_MODEL, called)), Vec::new());
        thread.ask_clarification(Q.into(), CLARIFY_STORAGE.into());
        thread.settle(Some(ANSWER.into()));
        thread.ask_clarification(FOLLOW_UP.into(), CLARIFY_DEPLOYMENT.into());
        if failed {
            thread.settle(None);
        }
        thread.ask_clarification(FOLLOW_UP.into(), CLARIFY_DEPLOYMENT.into());
        let messages = thread.request_messages();
        assert!(user_text(messages.last().unwrap()).contains(CLARIFY_DEPLOYMENT));
        assert_eq!(thread.exchange_count(), 1);
    }

    /// Hands over the route and messages of every request it answers.
    struct RecordingProvider(flume::Sender<(String, Vec<Message>)>);

    impl Provider for RecordingProvider {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a serde_json::Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.0.send((model.spec(), messages.to_vec())).unwrap();
                Ok(StreamResponse {
                    message: assistant_text(ANSWER),
                    stop_reason: Some(StopReason::EndTurn),
                    ..Default::default()
                })
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    /// Reports prefill before it answers, as OpenAI's Responses provider does.
    struct PrefillingProvider;

    impl Provider for PrefillingProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a serde_json::Value,
            event_tx: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                event_tx.send(ProviderEvent::PromptProgress {
                    processed: PREFILL_PROCESSED,
                    total: PREFILL_TOTAL,
                    cache: PREFILL_CACHE,
                })?;
                Ok(StreamResponse {
                    message: assistant_text(ANSWER),
                    stop_reason: Some(StopReason::EndTurn),
                    ..Default::default()
                })
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    struct ReasoningProvider {
        interleaved: bool,
    }

    impl Provider for ReasoningProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            event_tx: &'a Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let mut events = [
                    ProviderEvent::ThinkingDelta {
                        text: THINKING_DELTAS[0].into(),
                    },
                    ProviderEvent::ThinkingDelta {
                        text: THINKING_DELTAS[1].into(),
                    },
                    ProviderEvent::TextDelta {
                        text: TEXT_DELTAS[0].into(),
                    },
                    ProviderEvent::TextDelta {
                        text: TEXT_DELTAS[1].into(),
                    },
                ];
                if self.interleaved {
                    events.swap(1, 2);
                }
                for event in events {
                    event_tx.send(event)?;
                }
                let mut message = assistant_text(ANSWER);
                message.content.insert(
                    0,
                    ContentBlock::Thinking {
                        thinking: THINKING_DELTAS.concat(),
                        signature: None,
                        duration_ms: None,
                        interrupted: false,
                        responses: None,
                    },
                );
                Ok(StreamResponse {
                    message,
                    stop_reason: Some(StopReason::EndTurn),
                    ..Default::default()
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    /// Answers each request with the next scripted response, streaming its
    /// text as deltas, and hands over the messages every request carried.
    struct ScriptedProvider {
        responses: Mutex<VecDeque<StreamResponse>>,
        requests: flume::Sender<Vec<Message>>,
    }

    impl Provider for ScriptedProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a Value,
            event_tx: &'a Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.requests.send(messages.to_vec()).unwrap();
                let response = self.responses.lock().unwrap().pop_front().unwrap();
                if let Some(text) = answer_text(&response.message) {
                    event_tx.send(ProviderEvent::TextDelta { text })?;
                }
                Ok(response)
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    fn tool_round(text: Option<&str>, calls: &[&str]) -> StreamResponse {
        let mut content: Vec<ContentBlock> = text
            .map(|text| ContentBlock::Text { text: text.into() })
            .into_iter()
            .collect();
        content.extend(
            calls
                .iter()
                .map(|id| ContentBlock::tool_use(*id, TOOL, json!({}))),
        );
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content,
                ..Default::default()
            },
            usage: ROUND_USAGE,
            stop_reason: Some(StopReason::ToolUse),
            ..Default::default()
        }
    }

    /// Runs one scripted question and returns every request it sent, every
    /// text delta the modal saw, and how it finished.
    fn run_scripted(
        responses: Vec<StreamResponse>,
    ) -> (Vec<Vec<Message>>, Vec<String>, StreamDone) {
        let (requests_tx, requests) = flume::unbounded();
        let provider = ScriptedProvider {
            responses: Mutex::new(responses.into()),
            requests: requests_tx,
        };
        let (event_tx, event_rx) = flume::unbounded();
        smol::block_on(run_btw(
            Arc::new(prompt_with(Arc::new(provider), FIRST_MODEL)),
            vec![btw_question(Q)],
            event_tx,
            None,
            CancelToken::none(),
        ));
        let mut deltas = Vec::new();
        let mut done = None;
        for event in event_rx.drain() {
            match event {
                StreamEvent::TextDelta(text) => deltas.push(text),
                StreamEvent::Done(finished) => done = Some(finished),
                _ => {}
            }
        }
        (
            requests.drain().collect(),
            deltas,
            done.expect(EXPECTED_DONE),
        )
    }

    fn round_cost(rounds: usize) -> Option<f64> {
        let model = Model::from_spec(FIRST_MODEL).unwrap();
        (0..rounds).try_fold(0.0, |total, _| {
            model
                .billed_cost(&ROUND_USAGE, false)
                .map(|cost| total + cost)
        })
    }

    #[test]
    fn a_tool_call_is_refused_and_the_model_answers() {
        let (requests, deltas, done) = run_scripted(vec![
            tool_round(Some(LOOKING), &OPEN_CALLS),
            StreamResponse {
                message: assistant_text(ANSWER),
                usage: ROUND_USAGE,
                stop_reason: Some(StopReason::EndTurn),
                ..Default::default()
            },
        ]);

        assert_eq!(requests.len(), 2);
        let (refusal, rest) = requests[1].split_last().unwrap();
        let (called, sent_first) = rest.split_last().unwrap();
        assert_eq!(
            serde_json::to_value(sent_first).unwrap(),
            serde_json::to_value(&requests[0]).unwrap(),
            "the redirect extends the first request"
        );
        assert_eq!(
            serde_json::to_value(called).unwrap(),
            serde_json::to_value(&tool_round(Some(LOOKING), &OPEN_CALLS).message).unwrap(),
            "the call goes back exactly as the model made it"
        );
        assert!(matches!(refusal.role, Role::User));
        let results: Vec<(&str, &str, bool)> = refusal
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => Some((tool_use_id.as_str(), content.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            OPEN_CALLS
                .iter()
                .map(|id| (*id, TOOL_REDIRECT, true))
                .collect::<Vec<_>>()
        );

        assert_eq!(deltas, [LOOKING, ROUND_SEPARATOR, ANSWER]);
        assert_eq!(
            done.answer,
            Some([LOOKING, ANSWER].join(ROUND_SEPARATOR)),
            "the thread keeps what the modal showed"
        );
        let mut usage = ROUND_USAGE;
        usage += ROUND_USAGE;
        assert_eq!(done.usage.usage, usage);
        assert_eq!(done.usage.cost, round_cost(2));
    }

    #[test_case(Some(LOOKING) ; "text_only_in_first")]
    #[test_case(None ; "no_text")]
    fn a_model_that_keeps_calling_tools_stops_after_the_redirects(first_text: Option<&str>) {
        let rounds = MAX_TOOL_REDIRECTS + 1;
        let responses = (0..rounds)
            .map(|round| tool_round(first_text.filter(|_| round == 0), &OPEN_CALLS[..1]))
            .collect();
        let (requests, deltas, done) = run_scripted(responses);

        assert_eq!(requests.len(), rounds);
        assert_eq!(deltas.last().map(String::as_str), Some(TOOL_CALL_STOPPED));
        assert_eq!(done.answer.as_deref(), first_text);
        assert_eq!(done.usage.cost, round_cost(rounds));
    }

    fn prompt_with(provider: Arc<dyn Provider>, spec: &str) -> BtwPrompt {
        BtwPrompt {
            provider,
            model: Model::from_spec(spec).unwrap(),
            system: SYSTEM.into(),
            tools: json!([]),
            opts: RequestOptions::default(),
            profile: None,
        }
    }

    fn prompt(spec: &str, called: flume::Sender<(String, Vec<Message>)>) -> BtwPrompt {
        prompt_with(Arc::new(RecordingProvider(called)), spec)
    }

    fn thread() -> BtwThread {
        let (tx, _rx) = flume::unbounded();
        BtwThread::new(
            Arc::new(prompt(FIRST_MODEL, tx)),
            vec![Message::user(BASE.into())],
        )
    }

    fn user_text(msg: &Message) -> String {
        msg.content
            .iter()
            .filter_map(|b| match b {
                caudra_providers::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn injects_reminder_before_question() {
        let text = user_text(&btw_question(Q));
        assert!(text.starts_with(BTW_REMINDER), "reminder leads the message");
        assert!(text.ends_with(Q), "question trails the message");
    }

    #[test]
    fn reminder_forbids_tools_without_claiming_they_are_absent() {
        assert!(
            BTW_REMINDER.contains("Do NOT call any tool"),
            "tool definitions are now sent, so the reminder must forbid rather than deny them"
        );
        assert!(
            !BTW_REMINDER.contains("NO tools"),
            "claiming there are no tools contradicts the definitions on the wire"
        );
    }

    #[test]
    fn a_thread_extends_its_base_with_exchanges_and_the_pending_question() {
        let mut thread = thread();
        thread.ask(Q.into());
        thread.settle(Some(ANSWER.into()));
        thread.ask(FOLLOW_UP.into());

        let messages = thread.request_messages();
        let texts: Vec<(bool, String)> = messages
            .iter()
            .map(|m| (matches!(m.role, Role::Assistant), user_text(m)))
            .collect();
        assert_eq!(texts.len(), 4);
        assert_eq!(texts[0], (false, BASE.to_owned()));
        assert!(
            !texts[1].0 && texts[1].1.starts_with(BTW_REMINDER) && texts[1].1.ends_with(Q),
            "the reminder rides the first question"
        );
        assert_eq!(texts[2], (true, ANSWER.to_owned()));
        assert_eq!(
            texts[3],
            (false, FOLLOW_UP.to_owned()),
            "a follow-up carries no second reminder"
        );
    }

    #[test_case(None ; "no_answer")]
    #[test_case(Some("  \n") ; "blank_answer_is_no_answer")]
    fn an_unanswered_question_leaves_the_thread(answer: Option<&str>) {
        let mut thread = thread();
        thread.ask(Q.into());
        thread.settle(
            answer
                .map(str::to_owned)
                .and_then(|a| answer_text(&assistant_text(&a))),
        );
        assert!(thread.exchanges.is_empty());
        assert!(thread.pending.is_none());
        assert_eq!(thread.request_messages().len(), 1, "only the base remains");
    }

    const NEWER: &str = "we switched to postgres";
    const REBASE_MSG: &str =
        "a caught-up thread quotes its earlier exchanges in its first question";

    fn rebased(exchanges: usize) -> BtwThread {
        let mut thread = thread();
        for _ in 0..exchanges {
            thread.ask(Q.into());
            thread.settle(Some(ANSWER.into()));
        }
        let route = (Arc::clone(&thread.prompt), None);
        thread.rebase(
            route,
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
        let messages = rebased(1).request_messages();
        let texts: Vec<String> = messages.iter().map(user_text).collect();
        assert_eq!(texts.len(), 3, "{REBASE_MSG}");
        assert_eq!(texts[..2], [BASE, NEWER]);
        let question = &texts[2];
        assert!(question.starts_with(BTW_REMINDER), "{REBASE_MSG}");
        assert!(question.contains(REBASE_REMINDER), "{REBASE_MSG}");
        assert!(question.contains(&format!("{QUESTION_OPEN}{Q}{QUESTION_CLOSE}")));
        assert!(question.contains(&format!("{ANSWER_OPEN}{ANSWER}{ANSWER_CLOSE}")));
        assert!(question.ends_with(FOLLOW_UP));
        assert!(!question.contains(OMITTED_SUFFIX));
    }

    #[test]
    fn a_rebased_thread_settles_and_extends_its_new_lineage() {
        let mut thread = rebased(1);
        thread.settle(Some(ANSWER.into()));
        let first = thread.request_messages();
        thread.ask(Q.into());
        let next = thread.request_messages();
        assert_eq!(
            serde_json::to_value(&next[..first.len()]).unwrap(),
            serde_json::to_value(&first).unwrap(),
            "a follow-up after a rebase extends the request before it"
        );
        assert_eq!(user_text(next.last().unwrap()), Q);
    }

    #[test]
    fn a_long_thread_quotes_only_its_latest_exchanges() {
        let extra = 2;
        let thread = rebased(MAX_REPLAYED_EXCHANGES + extra);
        let question = user_text(thread.request_messages().last().unwrap());
        assert_eq!(thread.earlier.len(), MAX_REPLAYED_EXCHANGES);
        assert!(question.contains(&format!("{OMITTED_PREFIX}{extra}{OMITTED_SUFFIX}")));
        assert_eq!(
            question.matches(QUESTION_OPEN).count(),
            MAX_REPLAYED_EXCHANGES
        );
    }

    #[test]
    fn a_question_asked_over_a_failed_one_replaces_it() {
        let mut thread = thread();
        thread.ask(Q.into());
        thread.ask(FOLLOW_UP.into());
        let messages = thread.request_messages();
        assert_eq!(messages.len(), 2);
        assert!(user_text(&messages[1]).ends_with(FOLLOW_UP));
    }

    #[test]
    fn answer_text_keeps_text_blocks_only() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    signature: None,
                    duration_ms: None,
                    interrupted: false,
                    responses: None,
                },
                ContentBlock::Text {
                    text: ANSWER.into(),
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "file_read".into(),
                    input: json!({}),
                    thought_signature: None,
                },
            ],
            ..Default::default()
        };
        assert_eq!(answer_text(&message).as_deref(), Some(ANSWER));
        assert_eq!(answer_text(&Message::default()), None);
    }

    /// The forwarder used to keep text alone, which left the modal with
    /// nothing to draw for the whole of a long prefill.
    #[test]
    fn prefill_progress_reaches_the_modal() {
        smol::block_on(async {
            let (event_tx, event_rx) = flume::unbounded();
            run_btw(
                Arc::new(prompt_with(Arc::new(PrefillingProvider), FIRST_MODEL)),
                vec![Message::user(Q.into())],
                event_tx,
                None,
                CancelToken::none(),
            )
            .await;

            assert!(matches!(
                event_rx.try_recv(),
                Ok(StreamEvent::Progress(progress))
                    if progress.processed == PREFILL_PROCESSED
                        && progress.total == PREFILL_TOTAL
                        && progress.cache == PREFILL_CACHE
            ));
        });
    }

    #[test_case(false; "split_reasoning_before_text")]
    #[test_case(true; "interleaved_reasoning_and_text")]
    fn reasoning_streams_separately_and_stays_out_of_follow_up_history(interleaved: bool) {
        smol::block_on(async {
            let prompt = Arc::new(prompt_with(
                Arc::new(ReasoningProvider { interleaved }),
                FIRST_MODEL,
            ));
            let mut thread = BtwThread::new(prompt, vec![Message::user(BASE.into())]);
            thread.ask(Q.into());
            let (event_tx, event_rx) = flume::unbounded();
            run_btw(
                Arc::clone(&thread.prompt),
                thread.request_messages(),
                event_tx,
                None,
                CancelToken::none(),
            )
            .await;

            let expected = if interleaved {
                [
                    (true, THINKING_DELTAS[0]),
                    (false, TEXT_DELTAS[0]),
                    (true, THINKING_DELTAS[1]),
                    (false, TEXT_DELTAS[1]),
                ]
            } else {
                [
                    (true, THINKING_DELTAS[0]),
                    (true, THINKING_DELTAS[1]),
                    (false, TEXT_DELTAS[0]),
                    (false, TEXT_DELTAS[1]),
                ]
            };
            for (thinking, text) in expected {
                assert!(matches!(
                    (event_rx.try_recv().unwrap(), thinking),
                    (StreamEvent::ThinkingDelta(delta), true)
                        | (StreamEvent::TextDelta(delta), false) if delta == text
                ));
            }
            let StreamEvent::Done(done) = event_rx.try_recv().unwrap() else {
                panic!("{EXPECTED_DONE}");
            };
            assert_eq!(done.answer.as_deref(), Some(ANSWER));
            assert!(event_rx.try_recv().is_err());

            thread.settle(done.answer);
            assert!(thread.pending().is_none());
            assert_eq!(thread.exchanges, vec![(Q.to_owned(), ANSWER.to_owned())]);
            thread.ask(FOLLOW_UP.into());
            let expected = vec![
                Message::user(BASE.into()),
                btw_question(Q),
                assistant_text(ANSWER),
                Message::user(FOLLOW_UP.into()),
            ];
            assert_eq!(
                serde_json::to_value(thread.request_messages()).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        });
    }

    #[test]
    fn a_captured_prompt_keeps_its_model_provider_pair_across_a_switch() {
        smol::block_on(async {
            let (first_tx, first_rx) = flume::unbounded();
            let (second_tx, second_rx) = flume::unbounded();
            let shared = ArcSwap::from_pointee(prompt(FIRST_MODEL, first_tx));
            let captured = shared.load_full();
            shared.store(Arc::new(prompt(SECOND_MODEL, second_tx)));
            let (event_tx, event_rx) = flume::unbounded();

            run_btw(
                Arc::clone(&captured),
                vec![Message::user(Q.into())],
                event_tx,
                None,
                CancelToken::none(),
            )
            .await;
            assert_eq!(first_rx.try_recv().unwrap().0, FIRST_MODEL);
            assert!(matches!(
                event_rx.try_recv(),
                Ok(StreamEvent::Done(StreamDone { answer: Some(answer), .. })) if answer == ANSWER
            ));

            let (event_tx, _event_rx) = flume::unbounded();
            run_btw(
                captured,
                vec![Message::user(FOLLOW_UP.into())],
                event_tx,
                None,
                CancelToken::none(),
            )
            .await;
            assert_eq!(
                first_rx.try_recv().unwrap().0,
                FIRST_MODEL,
                "a follow-up rides the same captured route"
            );
            assert!(second_rx.try_recv().is_err());
        });
    }

    /// A thread goes out exactly as the live request would, repairs included,
    /// rather than through a hand-rolled copy of the projection that drifts.
    #[test]
    fn a_thread_sends_the_request_projection_and_the_question() {
        smol::block_on(async {
            let history = vec![
                Message::user(BASE.into()),
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: ORPHANED_CALL.into(),
                        content: ANSWER.into(),
                        is_error: false,
                        output_ref: None,
                    }],
                    ..Default::default()
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::tool_use(UNANSWERED_CALL, TOOL, json!({}))],
                    ..Default::default()
                },
                Message::user(FOLLOW_UP.into()),
                assistant_text(ANSWER),
            ];
            let (called, requests) = flume::unbounded();
            let mut thread = BtwThread::new(Arc::new(prompt(FIRST_MODEL, called)), history.clone());
            thread.ask(Q.into());
            let (event_tx, _event_rx) = flume::unbounded();

            run_btw(
                Arc::clone(&thread.prompt),
                thread.request_messages(),
                event_tx,
                None,
                CancelToken::none(),
            )
            .await;

            let pinned = &thread.prompt;
            let mut expected = caudra_agent::project_request(
                &history,
                &pinned.tools,
                &pinned.model,
                pinned.provider.reasoning_transport(&pinned.model),
            )
            .into_owned();
            expected.push(btw_question(Q));
            let (_, sent) = requests.try_recv().unwrap();
            assert_eq!(
                serde_json::to_value(sent).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        });
    }

    /// A resumed session holds only a reference to a PDF. The thread loads it
    /// before projecting, so the PDF goes out attached while its file exists,
    /// and as a note to fetch it again only once the file is gone.
    #[test_case(true ; "stored_file_attaches")]
    #[test_case(false ; "missing_file_becomes_a_note")]
    fn a_restored_pdf_is_loaded_before_the_thread_sends(file_exists: bool) {
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
        let base = vec![
            Message::user(BASE.into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(PDF_CALL, TOOL, json!({}))],
                ..Default::default()
            },
            result,
        ];
        let (called, requests) = flume::unbounded();
        let mut prompt = prompt(FIRST_MODEL, called);
        prompt.model.supports_pdf = true;
        let (event_tx, _event_rx) = flume::unbounded();

        smol::block_on(run_btw_over(
            Arc::new(prompt),
            base,
            vec![btw_question(Q)],
            store,
            event_tx,
            None,
            CancelToken::none(),
        ));

        let (_, sent) = requests.try_recv().unwrap();
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

    /// Reading the history rejects every broken tool pairing but one: a turn
    /// caught between its calls and their results. A provider rejects a call
    /// left unanswered, so the thread closes each one, in the order the turn
    /// made them.
    #[test_case(1 ; "one_open_call")]
    #[test_case(2 ; "two_open_calls")]
    fn a_thread_closes_the_calls_a_turn_left_open(open: usize) {
        let calls = &OPEN_CALLS[..open];
        let (called, _requests) = flume::unbounded();
        let mut thread = BtwThread::new(
            Arc::new(prompt(FIRST_MODEL, called)),
            vec![
                Message::user(BASE.into()),
                Message {
                    role: Role::Assistant,
                    content: calls
                        .iter()
                        .map(|id| ContentBlock::tool_use(*id, TOOL, json!({})))
                        .collect(),
                    ..Default::default()
                },
            ],
        );
        thread.ask(Q.into());

        let messages = thread.request_messages();
        let (_question, base) = messages.split_last().unwrap();
        let closing = base.last().unwrap();
        assert!(matches!(closing.role, Role::User));
        assert_eq!(closing.display_text.as_deref(), Some(""));
        let results: Vec<(&str, &str, bool)> = closing
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => Some((tool_use_id.as_str(), content.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            calls
                .iter()
                .map(|id| (*id, UNAVAILABLE_RESULT, true))
                .collect::<Vec<_>>()
        );
    }
}
