//! Questions the main agent asked without waiting, and how they share the one
//! docked form with the questions an agent waits on.
//!
//! One form is docked at a time. A deferred form waits in a queue with its
//! draft until it docks; an ask an agent waits on goes ahead of the queue but
//! never takes the place of a deferred form the user has started answering.

use std::collections::{HashMap, VecDeque};
use std::mem;

use caudra_agent::autopilot::DecisionKind;
use caudra_agent::permissions::AutopilotNote;
use caudra_agent::types::{QUESTION_DEFERRED_REPLY, QuestionDelivery, QuestionEvent};
use caudra_storage::autopilot::AutopilotStopper;

use super::{App, Status};
use crate::components::Action;
use crate::components::document_view::COPIED_SELECTION;
use crate::components::question_form::{QuestionForm, QuestionFormAction};

pub(crate) const QUESTION_CLOSED_MSG: &str = "That question is no longer open";
pub(crate) const NO_WAITING_QUESTION_MSG: &str = "No unanswered questions";
pub(crate) const ANSWER_OPEN_QUESTION_MSG: &str = "Answer or dismiss the open question first";
const NO_BACKGROUND_RUNTIME: &str = "this session runs no background jobs";
pub(crate) const NOTE_DRAFT_SEPARATOR: &str = "\n\n";

#[derive(Default)]
pub(crate) struct QuestionQueue {
    /// Deferred forms off the dock, oldest first.
    waiting: VecDeque<Waiting>,
    /// An ask an agent waits on, held while the user answers a deferred form.
    blocking: Option<QuestionForm>,
    /// A form the user put off, until its question comes back deferred.
    deferring: Option<QuestionForm>,
    /// The arrival of the docked deferred form, which places it among the rest.
    docked_arrival: u64,
    arrivals: u64,
    /// The deferred question each autopilot decision in flight is about, by
    /// seq. A decision missing here is about the form an agent waits on.
    deciding: HashMap<u32, String>,
}

struct Waiting {
    form: QuestionForm,
    arrival: u64,
    /// Put aside by the user: it docks again only when asked for, or when the
    /// agent goes idle.
    aside: bool,
}

impl QuestionQueue {
    fn arrive(&mut self) -> u64 {
        self.arrivals += 1;
        self.arrivals
    }

    fn position(&self, task_id: &str) -> Option<usize> {
        self.waiting
            .iter()
            .position(|waiting| waiting.form.deferred_task() == Some(task_id))
    }
}

impl App {
    /// Every deferred question still open, docked or not.
    pub(crate) fn open_question_count(&self) -> usize {
        self.questions.waiting.len() + usize::from(self.question_form.deferred_task().is_some())
    }

    /// The deferred questions off the dock, which the status bar counts. A
    /// docked deferred form counts them in its own title.
    pub(crate) fn waiting_question_count(&self) -> usize {
        match self.question_form.deferred_task() {
            Some(_) => 0,
            None => self.questions.waiting.len(),
        }
    }

    /// Whether `task_id` is a deferred question still waiting for the user.
    pub(crate) fn is_open_question(&self, task_id: &str) -> bool {
        self.question_form.deferred_task() == Some(task_id)
            || self.questions.position(task_id).is_some()
    }

    /// An agent parks on this ask until the user answers it.
    pub(super) fn receive_question(&mut self, event: QuestionEvent, subagent: Option<String>) {
        let form = self.question_for(event);
        self.question_subagent = subagent;
        if self.question_form.deferred_task().is_some() {
            if self.question_form.touched() {
                self.questions.blocking = Some(form);
                return;
            }
            self.shelve_docked(false, true);
        }
        self.discard_question_btw();
        self.question_form = form;
        self.place_questions();
    }

    /// The main agent asked without waiting, or the user put its ask off.
    pub(super) fn receive_deferred_question(&mut self, event: QuestionEvent) {
        let QuestionDelivery::Deferred { docked, .. } = event.delivery else {
            return;
        };
        let form = self.question_for(event);
        let arrival = self.questions.arrive();
        if docked && !self.question_form.is_open() {
            self.discard_question_btw();
            self.dock_deferred(form, arrival);
        } else {
            self.questions.waiting.push_back(Waiting {
                form,
                arrival,
                aside: !docked,
            });
        }
        self.place_questions();
    }

    /// The form the user put off comes back with its draft, whatever
    /// delivery it now has.
    fn question_for(&mut self, event: QuestionEvent) -> QuestionForm {
        let call_id = event.call_id.as_deref();
        if let Some(mut form) = self
            .questions
            .deferring
            .take_if(|form| call_id.is_some() && form.call_id() == call_id)
        {
            form.redeliver(event.delivery);
            return form;
        }
        let mut form = QuestionForm::new();
        form.ask(event);
        form
    }

    fn dock_deferred(&mut self, form: QuestionForm, arrival: u64) {
        self.question_form = form;
        self.questions.docked_arrival = arrival;
    }

    /// Takes the docked deferred form off the dock, draft and all.
    fn shelve_docked(&mut self, aside: bool, front: bool) {
        if self.question_form.deferred_task().is_none() {
            return;
        }
        self.discard_question_btw();
        let waiting = Waiting {
            form: mem::replace(&mut self.question_form, QuestionForm::new()),
            arrival: self.questions.docked_arrival,
            aside,
        };
        match front {
            true => self.questions.waiting.push_front(waiting),
            false => self.questions.waiting.push_back(waiting),
        }
    }

    /// Fills an empty dock: the held ask an agent waits on first, then the
    /// oldest deferred form the user did not put aside.
    pub(super) fn dock_next_question(&mut self) {
        if !self.question_form.is_open() {
            if let Some(form) = self.questions.blocking.take() {
                self.question_form = form;
            } else if let Some(waiting) = self
                .questions
                .waiting
                .iter()
                .position(|waiting| !waiting.aside)
                .and_then(|index| self.questions.waiting.remove(index))
            {
                self.dock_deferred(waiting.form, waiting.arrival);
            }
        }
        self.place_questions();
    }

    /// Docks deferred question `task_id`, or the oldest one, whether or not
    /// the user put it aside. A form an agent waits on keeps the dock.
    pub(crate) fn reopen_question(&mut self, task_id: Option<&str>) -> bool {
        if self.question_form.is_open() && self.question_form.deferred_task().is_none() {
            self.flash(ANSWER_OPEN_QUESTION_MSG.into());
            return false;
        }
        if task_id.is_none_or(|id| self.question_form.deferred_task() == Some(id))
            && self.question_form.is_open()
        {
            return true;
        }
        let index = match task_id {
            Some(id) => self.questions.position(id),
            None => (!self.questions.waiting.is_empty()).then_some(0),
        };
        let Some(waiting) = index.and_then(|index| self.questions.waiting.remove(index)) else {
            self.flash(NO_WAITING_QUESTION_MSG.into());
            return false;
        };
        self.shelve_docked(false, true);
        self.dock_deferred(waiting.form, waiting.arrival);
        self.place_questions();
        true
    }

    /// The main agent went idle with questions open: the oldest docks, since
    /// the agent may now be waiting on its answer.
    pub(super) fn escalate_questions(&mut self) {
        if !self.question_form.is_open() && !self.questions.waiting.is_empty() {
            self.reopen_question(None);
        }
    }

    fn place_questions(&mut self) {
        let place = self.question_form.deferred_task().is_some().then(|| {
            let arrival = self.questions.docked_arrival;
            let ahead = self
                .questions
                .waiting
                .iter()
                .filter(|waiting| waiting.arrival < arrival)
                .count();
            (ahead + 1, self.questions.waiting.len() + 1)
        });
        self.question_form.set_place(place);
    }

    /// What the user did on a docked deferred form. Nothing waits on it, so
    /// only an answer goes back, and to the question's own job.
    pub(super) fn handle_deferred_question_action(
        &mut self,
        action: QuestionFormAction,
    ) -> Vec<Action> {
        match action {
            QuestionFormAction::Consumed => {}
            QuestionFormAction::AskBtw => self.open_question_btw(),
            QuestionFormAction::Copy(text) => self.copy_to_clipboard(&text),
            QuestionFormAction::CopySelection(text) => {
                self.copy_labelled(&text, COPIED_SELECTION);
            }
            QuestionFormAction::Submit(answers) => {
                let reply = self.question_form.reply(Some(answers));
                self.answer_deferred_question(reply);
            }
            QuestionFormAction::Later => self.rotate_questions(),
            QuestionFormAction::Accept
            | QuestionFormAction::Decline
            | QuestionFormAction::Secret(_)
            | QuestionFormAction::Dismiss
            | QuestionFormAction::PutAside => self.put_questions_aside(),
            // Stopping the run withdraws every deferred question with it.
            QuestionFormAction::Cancel => {
                self.user_stopped(AutopilotStopper::QuestionCancel);
                let note = self.question_form.note_text();
                let actions = self.handle_cancel();
                self.keep_question_note(note);
                return actions;
            }
        }
        Vec::new()
    }

    /// `Ctrl+C` stops the agent that asked, and the note the user wrote for it
    /// waits in the composer for their next message, after any draft there.
    /// It goes in after the stop, which may hand a queued draft back first.
    pub(super) fn keep_question_note(&mut self, note: String) {
        if note.is_empty() {
            return;
        }
        let input = self.active_input_box_mut();
        input.move_to_end();
        if !input.is_empty() {
            input.buffer.insert_text(NOTE_DRAFT_SEPARATOR);
        }
        input.buffer.insert_text(&note);
    }

    fn answer_deferred_question(&mut self, reply: String) {
        let Some(task_id) = self.question_form.deferred_task().map(str::to_owned) else {
            return;
        };
        self.discard_question_btw();
        self.question_form.close();
        let answered = self
            .background
            .as_ref()
            .ok_or_else(|| NO_BACKGROUND_RUNTIME.to_owned())
            .and_then(|tasks| tasks.answer_question(&task_id, reply));
        if let Err(error) = answered {
            tracing::debug!(%task_id, %error, "a deferred answer found no open question");
            self.flash(QUESTION_CLOSED_MSG.into());
        }
        self.dock_next_question();
    }

    /// `Esc`: every deferred form leaves the dock until asked for, and only
    /// an ask an agent waits on takes its place.
    fn put_questions_aside(&mut self) {
        self.shelve_docked(true, true);
        for waiting in &mut self.questions.waiting {
            waiting.aside = true;
        }
        self.dock_next_question();
    }

    /// `F4` on a deferred form: it goes to the back, and the next one docks.
    fn rotate_questions(&mut self) {
        self.shelve_docked(false, false);
        match self.questions.blocking.take() {
            Some(form) => self.question_form = form,
            None => {
                if let Some(waiting) = self.questions.waiting.pop_front() {
                    self.dock_deferred(waiting.form, waiting.arrival);
                }
            }
        }
        self.place_questions();
    }

    /// `F4` on a form the main agent waits on: it gets a receipt instead of
    /// an answer, and the form comes back deferred with its draft.
    pub(super) fn defer_question(&mut self) {
        self.discard_question_btw();
        self.task_interactions.question = None;
        let subagent = self.question_subagent.take();
        self.questions.deferring = Some(mem::replace(&mut self.question_form, QuestionForm::new()));
        self.send_to_agent(subagent.as_deref(), QUESTION_DEFERRED_REPLY.to_owned());
        self.dock_next_question();
    }

    /// A deferred question that closed without the user: withdrawn, or
    /// answered by autopilot.
    pub(super) fn close_deferred_question(&mut self, task_id: &str) -> bool {
        if self.question_form.deferred_task() == Some(task_id) {
            self.discard_question_btw();
            self.question_form.close();
            self.dock_next_question();
            return true;
        }
        let Some(index) = self.questions.position(task_id) else {
            return false;
        };
        self.questions.waiting.remove(index);
        self.place_questions();
        true
    }

    /// The ask an agent waits on is over, docked or held.
    pub(super) fn close_blocking_question(&mut self) {
        self.question_subagent = None;
        self.task_interactions.question = None;
        if self.questions.blocking.take().is_some() || self.question_form.deferred_task().is_some()
        {
            return;
        }
        self.discard_question_btw();
        self.question_form.close();
        self.dock_next_question();
    }

    /// A stop, or a session change, ends every deferred question.
    pub(super) fn drop_questions(&mut self) {
        if self.question_form.deferred_task().is_some() {
            self.discard_question_btw();
            self.question_form.close();
        }
        self.questions = QuestionQueue::default();
    }

    /// Closing every overlay keeps a docked deferred form, put aside.
    pub(super) fn shelve_deferred_question(&mut self) {
        self.shelve_docked(true, true);
    }

    /// The form an agent is parked on, docked or held.
    fn blocking_form(&self) -> Option<&QuestionForm> {
        self.questions
            .blocking
            .as_ref()
            .or(
                (self.question_form.is_open() && self.question_form.deferred_task().is_none())
                    .then_some(&self.question_form),
            )
    }

    pub(super) fn has_blocking_question(&self) -> bool {
        self.blocking_form().is_some()
    }

    /// Whether an agent is parked on a question form the user must answer,
    /// rather than one autopilot is deciding.
    pub(crate) fn blocking_question_waits(&self) -> bool {
        self.blocking_form()
            .is_some_and(|form| form.autopilot_note() != Some(&AutopilotNote::Deciding))
    }

    /// The form waits on the user: one an agent is parked on, or a deferred
    /// one once the main agent has nothing left to do without it.
    pub(super) fn question_form_waits(&self) -> bool {
        self.blocking_question_waits()
            || (self.status != Status::Streaming && self.open_question_count() > 0)
    }

    /// What the session reports as needing input: an agent parked on the
    /// user, or a deferred question the idle main agent may wait on.
    pub(crate) fn needs_input(&self) -> bool {
        self.awaiting_input() || self.question_form_waits()
    }

    /// Notes which deferred question an autopilot decision is about.
    pub(super) fn note_question_decision(&mut self, seq: u32, task_id: &str) {
        if self.is_open_question(task_id) {
            self.questions.deciding.insert(seq, task_id.to_owned());
        }
    }

    /// Whether decision `seq` is about a deferred question, which outlives
    /// the run that asked it.
    pub(super) fn decides_deferred_question(&self, seq: u32) -> bool {
        self.questions.deciding.contains_key(&seq)
    }

    /// The form decision `seq` is about: the deferred one it names, or else
    /// the one an agent waits on.
    pub(super) fn decided_question(&mut self, seq: u32) -> Option<&mut QuestionForm> {
        match self.questions.deciding.get(&seq).cloned() {
            Some(task_id) if self.question_form.deferred_task() == Some(task_id.as_str()) => {
                Some(&mut self.question_form)
            }
            Some(task_id) => {
                let index = self.questions.position(&task_id)?;
                self.questions
                    .waiting
                    .get_mut(index)
                    .map(|waiting| &mut waiting.form)
            }
            None => match self.questions.blocking.as_mut() {
                Some(form) => Some(form),
                None => (self.question_form.deferred_task().is_none())
                    .then_some(&mut self.question_form),
            },
        }
    }

    /// Autopilot answered the form decision `seq` is about.
    pub(super) fn close_decided_question(&mut self, seq: u32) {
        match self.questions.deciding.remove(&seq) {
            Some(task_id) => {
                self.close_deferred_question(&task_id);
            }
            None => self.close_blocking_question(),
        }
    }

    /// Decision `seq` is over, whichever way it went.
    pub(super) fn forget_question_decision(&mut self, seq: u32) {
        self.questions.deciding.remove(&seq);
    }

    /// The user took the docked form from autopilot, so the decisions about
    /// it, and only those, are over.
    pub(super) fn claimed_question_decisions(&mut self) -> Vec<u32> {
        let task_id = self.question_form.deferred_task().map(str::to_owned);
        let claimed: Vec<u32> = self
            .autopilot_deciding
            .iter()
            .filter(|(seq, kind)| {
                *kind == DecisionKind::Question
                    && self.questions.deciding.get(seq).map(String::as_str) == task_id.as_deref()
            })
            .map(|(seq, _)| *seq)
            .collect();
        for seq in &claimed {
            self.questions.deciding.remove(seq);
        }
        claimed
    }
}
