//! A learner run's conversation, opened from `/learn`: fed live while the run
//! goes, read back from the store once the run kept it. It never joins the
//! session's chats, so nothing here is saved with the session or searched.

use std::collections::VecDeque;
use std::path::Path;

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use crate::chat::Chat;
use crate::components::Overlay;
use crate::components::docs_link::DocsTopic;
use crate::components::side_transcript::{SideAction, SideFooter, SideTranscript};
use crate::components::tooltip::Tip;
use crate::repaint::{Cadence, Dirty};

const TITLE: &str = " Learner transcript ";
/// Runs watched live this session: the one running and the one before it.
pub(crate) const LIVE_RUNS: usize = 2;

/// Every run is its own read-only window, so closing one never stops the run
/// it watches.
#[derive(Default)]
pub(crate) struct LearnTranscripts {
    live: VecDeque<(i64, SideTranscript)>,
    stored: Option<(i64, SideTranscript)>,
    shown: Option<i64>,
}

impl LearnTranscripts {
    /// Watches a run that just started in `chat`. The oldest live run gives way.
    pub fn begin(&mut self, run_id: i64, chat: Chat) {
        self.live.retain(|(id, _)| *id != run_id);
        if self.live.len() == LIVE_RUNS
            && let Some((dropped, _)) = self.live.pop_front()
            && self.shown == Some(dropped)
        {
            self.shown = None;
        }
        self.live.push_back((run_id, window(chat)));
    }

    pub fn live_mut(&mut self, run_id: i64) -> Option<&mut SideTranscript> {
        self.live
            .iter_mut()
            .find(|(id, _)| *id == run_id)
            .map(|(_, side)| side)
    }

    pub fn has(&self, run_id: i64) -> bool {
        self.side(run_id).is_some()
    }

    /// Keeps a run read back from the store, in place of the last one read.
    pub fn keep(&mut self, run_id: i64, chat: Chat) {
        if let Some((replaced, _)) = self.stored.replace((run_id, window(chat)))
            && self.shown == Some(replaced)
        {
            self.shown = None;
        }
    }

    pub fn show(&mut self, run_id: i64) {
        if !self.has(run_id) {
            return;
        }
        self.close();
        self.shown = Some(run_id);
        if let Some(side) = self.shown_side() {
            side.open();
        }
    }

    #[cfg(test)]
    pub fn shown(&self) -> Option<i64> {
        self.shown
    }

    #[cfg(test)]
    pub fn chat(&self, run_id: i64) -> Option<&Chat> {
        self.side(run_id).map(SideTranscript::chat)
    }

    fn side(&self, run_id: i64) -> Option<&SideTranscript> {
        self.live
            .iter()
            .chain(&self.stored)
            .find(|(id, _)| *id == run_id)
            .map(|(_, side)| side)
    }

    fn shown_side(&mut self) -> Option<&mut SideTranscript> {
        let run_id = self.shown?;
        self.live
            .iter_mut()
            .chain(&mut self.stored)
            .find(|(id, _)| *id == run_id)
            .map(|(_, side)| side)
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.shown
            .and_then(|run_id| self.side(run_id))
            .is_some_and(|side| side.contains(pos))
    }

    pub fn tick(&mut self) -> Dirty {
        self.shown_side().map_or(Dirty::NO, SideTranscript::tick)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SideAction {
        let action = self
            .shown_side()
            .map_or(SideAction::Ignored, |side| side.handle_key(key));
        self.settle(action)
    }

    pub fn scroll_at(&mut self, at: Position, delta: i32) {
        if let Some(side) = self.shown_side() {
            side.scroll_at(at, delta);
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent, cwd: &Path) -> SideAction {
        let action = self
            .shown_side()
            .map_or(SideAction::Ignored, |side| side.handle_mouse(event, cwd));
        self.settle(action)
    }

    /// A window that closed itself leaves nothing shown.
    fn settle(&mut self, action: SideAction) -> SideAction {
        if action == SideAction::Close {
            self.shown = None;
        }
        action
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.shown_side()
            .map_or_else(Rect::default, |side| side.view(frame, area))
    }
}

fn window(chat: Chat) -> SideTranscript {
    SideTranscript::new(TITLE, SideFooter::Observe, chat)
}

impl Overlay for LearnTranscripts {
    fn is_open(&self) -> bool {
        self.shown.is_some()
    }

    fn close(&mut self) {
        if let Some(side) = self.shown_side() {
            side.close();
        }
        self.shown = None;
    }

    fn tooltip(&self) -> Option<Tip> {
        self.shown
            .and_then(|run_id| self.side(run_id))
            .and_then(SideTranscript::tooltip)
    }

    fn cadence(&self) -> Cadence {
        self.shown
            .and_then(|run_id| self.side(run_id))
            .map_or(Cadence::IDLE, SideTranscript::cadence)
    }

    fn docs_topic(&self) -> Option<DocsTopic> {
        Some(DocsTopic::Learning)
    }
}
