//! A learner run's conversation, opened from `/learn`: fed live while the run
//! goes, read back from the store once the run kept it. It never joins the
//! session's chats, so nothing here is saved with the session or searched.

use std::collections::VecDeque;
use std::mem;
use std::path::Path;

use caudra_grab::grab_scope;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use crate::chat::Chat;
use crate::components::docs_link::DocsTopic;
use crate::components::modal::Modal;
use crate::components::tooltip::Tip;
use crate::components::{Hint, HintBar, Overlay};
use crate::repaint::{Cadence, Dirty};

const TITLE: &str = " Learner transcript ";
const PERCENT: u16 = 90;
const FOOTER_ROWS: u16 = 1;
const LINE_STEP: i32 = 1;
/// Runs watched live this session: the one running and the one before it.
pub(crate) const LIVE_RUNS: usize = 2;
const HINTS: &[Hint] = &[
    Hint::inert("\u{2191}\u{2193}", "scroll"),
    Hint::inert("Home/End", "top/bottom"),
    Hint::key("Esc", KeyCode::Esc, "back"),
];

#[derive(Default)]
pub(crate) struct LearnTranscripts {
    live: VecDeque<(i64, Chat)>,
    stored: Option<(i64, Chat)>,
    shown: Option<i64>,
    footer: HintBar,
    /// Where the chat was drawn, and whether a press landed in it, so only a
    /// press and release both on the chat click it.
    body: Rect,
    pressed: bool,
    popup: Rect,
}

impl LearnTranscripts {
    /// Watches a run that just started in `chat`. The oldest live run gives way.
    pub fn begin(&mut self, run_id: i64, chat: Chat) {
        self.live.retain(|(id, _)| *id != run_id);
        if self.live.len() == LIVE_RUNS
            && let Some((dropped, _)) = self.live.pop_front()
            && self.shown == Some(dropped)
        {
            self.close();
        }
        self.live.push_back((run_id, chat));
    }

    pub fn live_mut(&mut self, run_id: i64) -> Option<&mut Chat> {
        self.live
            .iter_mut()
            .find(|(id, _)| *id == run_id)
            .map(|(_, chat)| chat)
    }

    pub fn has(&self, run_id: i64) -> bool {
        self.chat(run_id).is_some()
    }

    /// Keeps a run read back from the store, in place of the last one read.
    pub fn keep(&mut self, run_id: i64, chat: Chat) {
        self.stored = Some((run_id, chat));
    }

    pub fn show(&mut self, run_id: i64) {
        if self.has(run_id) {
            self.shown = Some(run_id);
        }
    }

    #[cfg(test)]
    pub fn shown(&self) -> Option<i64> {
        self.shown
    }

    pub fn chat(&self, run_id: i64) -> Option<&Chat> {
        self.live
            .iter()
            .chain(&self.stored)
            .find(|(id, _)| *id == run_id)
            .map(|(_, chat)| chat)
    }

    fn shown_chat(&mut self) -> Option<&mut Chat> {
        let run_id = self.shown?;
        self.live
            .iter_mut()
            .chain(&mut self.stored)
            .find(|(id, _)| *id == run_id)
            .map(|(_, chat)| chat)
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.popup.contains(pos)
    }

    pub fn tick(&mut self) -> Dirty {
        self.shown_chat().map_or(Dirty::NO, Chat::tick)
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.close();
            return;
        }
        let Some(chat) = self.shown_chat() else {
            return;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => chat.scroll(-LINE_STEP),
            KeyCode::Down | KeyCode::Char('j') => chat.scroll(LINE_STEP),
            KeyCode::PageUp => chat.scroll(-chat.half_page()),
            KeyCode::PageDown => chat.scroll(chat.half_page()),
            KeyCode::Home | KeyCode::Char('g') => chat.scroll_to_top(),
            KeyCode::End | KeyCode::Char('G') => chat.enable_auto_scroll(),
            _ => {}
        }
    }

    /// An armed scroll card under the pointer takes the wheel first, as in
    /// the main transcript, and hands back what it cannot use.
    pub fn scroll_at(&mut self, at: Position, delta: i32) {
        let Some(chat) = self.shown_chat() else {
            return;
        };
        let delta = chat.scroll_card_at(at.x, at.y, delta);
        if delta != 0 {
            chat.scroll(delta);
        }
    }

    /// The chat answers the pointer as the main transcript does: hovering
    /// marks what a click acts on, a press arms a scroll card, and a click
    /// folds or opens what it lands on. A footer hint presses its key.
    pub fn handle_mouse(&mut self, event: MouseEvent, cwd: &Path) {
        if let Some(key) = self.footer.handle_mouse(event) {
            self.handle_key(key);
            return;
        }
        let body = self.body;
        let on_body = body.contains(Position::new(event.column, event.row));
        let pressed = mem::replace(
            &mut self.pressed,
            on_body && event.kind == MouseEventKind::Down(MouseButton::Left),
        );
        let Some(chat) = self.shown_chat() else {
            return;
        };
        if chat.handle_card_scrollbar(&event) || chat.handle_scrollbar(&event) {
            return;
        }
        match event.kind {
            MouseEventKind::Moved if on_body => {
                chat.update_hover(event.row, event.column, body, false, cwd);
            }
            MouseEventKind::Moved => {
                chat.clear_hover();
                chat.disarm_card();
            }
            MouseEventKind::Down(MouseButton::Left) if on_body => {
                chat.arm_card_at(event.column, event.row);
            }
            MouseEventKind::Up(MouseButton::Left)
                if pressed && on_body && !chat.armed_card_at(event.column, event.row) =>
            {
                chat.handle_click(event.row, body);
            }
            MouseEventKind::ScrollUp => chat.scroll(-LINE_STEP),
            MouseEventKind::ScrollDown => chat.scroll(LINE_STEP),
            _ => {}
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let Some(chat) = self.shown_chat() else {
            return Rect::default();
        };
        grab_scope!("learn_transcript", area);
        let modal = Modal {
            title: TITLE,
            width_percent: PERCENT,
            max_height_percent: PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, area.height);
        let body = Rect {
            height: inner.height.saturating_sub(FOOTER_ROWS),
            ..inner
        };
        chat.view(frame, body, false, false);
        let footer = Rect {
            y: inner.bottom().saturating_sub(FOOTER_ROWS),
            height: FOOTER_ROWS.min(inner.height),
            ..inner
        };
        self.footer.draw(frame, footer, HINTS.to_vec());
        self.body = body;
        self.popup = popup;
        popup
    }
}

impl Overlay for LearnTranscripts {
    fn is_open(&self) -> bool {
        self.shown.is_some()
    }

    fn close(&mut self) {
        if let Some(chat) = self.shown_chat() {
            chat.clear_hover();
            chat.disarm_card();
        }
        self.shown = None;
        self.footer.reset();
        self.body = Rect::default();
        self.pressed = false;
        self.popup = Rect::default();
    }

    fn tooltip(&self) -> Option<Tip> {
        self.shown
            .and_then(|run_id| self.chat(run_id))
            .and_then(Chat::hover_tip)
    }

    fn cadence(&self) -> Cadence {
        self.shown
            .and_then(|run_id| self.chat(run_id))
            .map_or(Cadence::IDLE, Chat::cadence)
    }

    fn docs_topic(&self) -> Option<DocsTopic> {
        Some(DocsTopic::Learning)
    }
}
