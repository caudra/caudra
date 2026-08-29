use std::time::{Duration, Instant};

use crate::clipboard::CopyResult;
use crate::components::Overlay;
use crate::components::queue_panel::{QueueAction, QueueHit, QueueHitTarget};
use crate::components::status_bar::{StatusBarHit, StatusBarHitTarget};
use crate::selection::{self, ContentRegion, EdgeScroll, Selection, SelectionState, SelectionZone};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::repaint::Dirty;

use super::tasks::MAIN_TASK_ID;
use super::{App, MessageMouseDown};

pub(super) const EDGE_SCROLL_LINES: i32 = 1;
pub(super) const EDGE_SCROLL_INTERVAL: Duration = Duration::from_millis(25);
pub(super) const MESSAGE_LONG_CLICK: Duration = Duration::from_millis(500);
const MESSAGE_ACTIONS_UNAVAILABLE: &str = "Message actions unavailable here";

impl App {
    pub(super) fn handle_mouse(&mut self, event: MouseEvent) -> Vec<crate::components::Action> {
        if self.paste_editor.is_open() {
            self.clear_control_hovers();
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                self.paste_editor.handle_click(event.row, event.column);
            }
            return Vec::new();
        }
        let passive_modal_open = self.help_modal.is_open()
            || self.usage_modal.is_open()
            || self.goal_modal.is_open()
            || self.btw_modal.is_open()
            || self.float_mgr.is_open();
        if passive_modal_open || self.permission_prompt.is_open() {
            self.clear_control_hovers();
        } else if self.permissions_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.permissions_picker.handle_mouse(event),
                |app, action| app.handle_permissions_picker_action(action),
            ) {
                return actions;
            }
        } else if self.mcp_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.mcp_picker.handle_mouse(event),
                |app, action| app.handle_mcp_picker_action(action),
            ) {
                return actions;
            }
        } else if self.login_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.login_picker.handle_mouse(event),
                |app, action| app.handle_login_picker_action(action),
            ) {
                return actions;
            }
        } else if self.model_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.model_picker.handle_mouse(event),
                |app, action| app.handle_model_picker_action(action),
            ) {
                return actions;
            }
        } else if self.theme_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.theme_picker.handle_mouse(event),
                |app, action| app.handle_theme_picker_action(action),
            ) {
                return actions;
            }
        } else if self.message_actions.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.message_actions.handle_mouse(event),
                |app, action| app.handle_message_actions_action(action),
            ) {
                return actions;
            }
        } else if self.rewind_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.rewind_picker.handle_mouse(event),
                |app, action| app.handle_rewind_picker_action(action),
            ) {
                return actions;
            }
        } else if self.file_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.file_picker.handle_mouse(event),
                |app, action| app.handle_file_picker_action(action),
            ) {
                return actions;
            }
        } else if self.search_modal.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.search_modal.handle_mouse(event),
                |app, action| app.handle_search_action(action),
            ) {
                return actions;
            }
        } else if self.plan_form_active() {
            let action = self.plan_form.handle_mouse(event);
            if action != crate::components::plan_form::PlanFormAction::Passthrough {
                self.clear_control_hovers();
                return self.handle_plan_form_action(action);
            }
        } else if self.command_palette.is_active() {
            let input = self.input_box.buffer.value();
            let action = self.command_palette.handle_mouse(event, &input);
            if let Some(actions) = self.handle_command_action(action) {
                self.clear_control_hovers();
                return actions;
            }
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Right) => {
                if self.any_overlay_open() {
                    return Vec::new();
                }
                let Some(zone) = self.zone_at(event.row, event.column) else {
                    return Vec::new();
                };
                if zone.zone != SelectionZone::Messages || !self.is_main_chat() {
                    self.flash(MESSAGE_ACTIONS_UNAVAILABLE.into());
                    return Vec::new();
                }
                let area = self.msg_area();
                if !area.contains(Position::new(event.column, event.row)) {
                    self.flash(MESSAGE_ACTIONS_UNAVAILABLE.into());
                    return Vec::new();
                }
                let Some(source) = self.chats[0].source_at(event.row, area) else {
                    self.flash(MESSAGE_ACTIONS_UNAVAILABLE.into());
                    return Vec::new();
                };
                self.message_actions
                    .open(source, self.state.session.meta.pending_revert.is_some());
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.admission_mouse_down = None;
                self.queue_mouse_down = None;
                self.status_mouse_down = None;
                self.message_mouse_down = None;
                if !self.has_modal_overlay() {
                    self.admission_mouse_down = self.admission_hit_at(event.row, event.column);
                    self.status_mouse_down = self.status_hit_at(event.row, event.column);
                    self.queue_mouse_down = self.queue_hit_at(event.row, event.column);
                    if self.queue_mouse_down.is_none() {
                        self.unfocus_active_queue();
                    }
                }
                if let Some(zone) = self.zone_at(event.row, event.column) {
                    if self.has_modal_overlay() && zone.zone != SelectionZone::Overlay {
                        return Vec::new();
                    }
                    // Move the cursor to the click position in the input area.
                    if zone.zone == SelectionZone::Input {
                        let focused = !self.any_overlay_open();
                        let paste = self.active_input_box_mut().handle_click(
                            zone.area,
                            event.row,
                            event.column,
                            focused,
                        );
                        if let Some(id) = paste {
                            self.selection_state = None;
                            self.open_paste_editor(id);
                            return Vec::new();
                        }
                    }
                    if zone.zone == SelectionZone::Messages
                        && self.is_main_chat()
                        && !self.has_modal_overlay()
                        && let Some(source) = self.chats[0].source_at(event.row, self.msg_area())
                    {
                        self.message_mouse_down = Some(MessageMouseDown {
                            source,
                            since: Instant::now(),
                        });
                    }
                    let scroll = self.scroll_offset(zone.zone);
                    self.selection_state = Some(SelectionState::Dragging {
                        sel: Selection::start(
                            event.row,
                            event.column,
                            zone.area,
                            zone.zone,
                            scroll,
                        ),
                        edge_scroll: None,
                        last_drag_col: event.column,
                    });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.clear_control_hovers();
                self.admission_mouse_down = None;
                self.queue_mouse_down = None;
                self.status_mouse_down = None;
                self.message_mouse_down = None;
                self.handle_drag(event.row, event.column);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if !self.has_modal_overlay()
                    && let Some(pressed) = self.admission_mouse_down.take()
                    && self.admission_hit_at(event.row, event.column) == Some(pressed)
                {
                    self.queue_mouse_down = None;
                    self.status_mouse_down = None;
                    self.message_mouse_down = None;
                    return self.handle_streaming_admission(pressed.admission);
                }
                if let Some(SelectionState::Dragging { sel, .. }) = self.selection_state {
                    if !sel.is_empty() {
                        self.queue_mouse_down = None;
                        self.selection_state = Some(SelectionState::PendingCopy { sel });
                    } else {
                        let zone = sel.zone;
                        self.selection_state = None;
                        if zone == SelectionZone::Messages
                            && self.is_main_chat()
                            && let Some(pressed) = self.message_mouse_down.take()
                            && pressed.since.elapsed() >= MESSAGE_LONG_CLICK
                            && self.chats[0].source_at(event.row, self.msg_area())
                                == Some(pressed.source)
                        {
                            self.queue_mouse_down = None;
                            self.message_actions.open(
                                pressed.source,
                                self.state.session.meta.pending_revert.is_some(),
                            );
                            return Vec::new();
                        }
                        self.message_mouse_down = None;
                        if !self.has_modal_overlay()
                            && let Some(pressed) = self.status_mouse_down.take()
                            && self.status_hit_at(event.row, event.column) == Some(pressed)
                        {
                            self.queue_mouse_down = None;
                            return self.handle_status_click(pressed);
                        }
                        if !self.has_modal_overlay()
                            && let Some(pressed) = self.queue_mouse_down.take()
                            && self.queue_hit_at(event.row, event.column) == Some(pressed)
                        {
                            self.handle_queue_click(pressed);
                            return Vec::new();
                        }
                        if zone == SelectionZone::Messages {
                            let area = self.msg_area();
                            if self.active_chat == 0
                                && let Some(tool_id) = self.chats[0].tool_id_at(event.row, area)
                            {
                                let task_id = self
                                    .parent_task_ids
                                    .get(tool_id)
                                    .cloned()
                                    .unwrap_or_else(|| tool_id.to_owned());
                                if self.focus_task(&task_id).is_ok() {
                                    return Vec::new();
                                }
                            }
                            self.chats[self.active_chat].handle_click(event.row, area);
                        }
                    }
                }
                self.admission_mouse_down = None;
                self.queue_mouse_down = None;
                self.status_mouse_down = None;
                self.message_mouse_down = None;
            }
            MouseEventKind::Moved => {
                if self.has_modal_overlay() {
                    self.clear_control_hovers();
                    return Vec::new();
                }
                self.admission_hover = self
                    .admission_hit_at(event.row, event.column)
                    .map(|hit| hit.admission);
                self.queue_hover = self
                    .queue_hit_at(event.row, event.column)
                    .map(|hit| hit.target);
                self.status_hover = self
                    .status_hit_at(event.row, event.column)
                    .map(|hit| hit.target);
                self.update_input_hover(event.row, event.column);
                self.update_transcript_hover(event.row, event.column);
            }
            _ => {}
        }
        Vec::new()
    }

    pub(super) fn handle_scroll(&mut self, column: u16, row: u16, delta: i32) {
        if self.paste_editor.is_open() {
            self.paste_editor.scroll(delta);
            return;
        }
        if !self.has_modal_overlay() && self.queue_hit_at(row, column).is_some() {
            self.scroll_active_queue(delta);
            self.clear_selection_unless_pending_copy();
            return;
        }
        let drag_zone = match self.selection_state {
            Some(SelectionState::Dragging { ref sel, .. }) => Some(sel.zone),
            _ => None,
        };
        match self.scroll_at(column, row, delta) {
            Some(zone) if drag_zone == Some(zone) => {
                let scroll = self.scroll_offset(zone);
                if let Some(SelectionState::Dragging { sel, .. }) = &mut self.selection_state {
                    sel.update(row, column, scroll);
                }
            }
            _ => self.clear_selection_unless_pending_copy(),
        }
    }

    fn handle_drag(&mut self, row: u16, col: u16) {
        let (zone, area) = match self.selection_state {
            Some(SelectionState::Dragging {
                ref sel,
                ref mut last_drag_col,
                ..
            }) => {
                *last_drag_col = col;
                (sel.zone, sel.area)
            }
            _ => return,
        };

        let at_top = row <= area.y;
        let at_bottom = row + 1 >= area.bottom();

        if at_top || at_bottom {
            let dir = if at_top {
                EDGE_SCROLL_LINES
            } else {
                -EDGE_SCROLL_LINES
            };
            let first_edge_hit = if let Some(SelectionState::Dragging { edge_scroll, .. }) =
                &mut self.selection_state
            {
                let first = edge_scroll.is_none();
                match edge_scroll {
                    Some(es) => es.dir = dir,
                    None => {
                        *edge_scroll = Some(EdgeScroll {
                            dir,
                            last_tick: Instant::now(),
                        });
                    }
                }
                first
            } else {
                false
            };
            if first_edge_hit {
                self.scroll_zone(zone, dir);
            }
            self.update_selection_to_edge(zone, col);
        } else {
            if let Some(SelectionState::Dragging { edge_scroll, .. }) = &mut self.selection_state {
                *edge_scroll = None;
            }
            let scroll = self.scroll_offset(zone);
            if let Some(SelectionState::Dragging { sel, .. }) = &mut self.selection_state {
                sel.update(row, col, scroll);
            }
        }
    }

    fn update_selection_to_edge(&mut self, zone: SelectionZone, col: u16) {
        let scroll = self.scroll_offset(zone);
        let Some(SelectionState::Dragging {
            ref mut sel,
            ref edge_scroll,
            ..
        }) = self.selection_state
        else {
            return;
        };
        let edge_row = if edge_scroll.as_ref().is_some_and(|es| es.dir > 0) {
            sel.area.y
        } else {
            sel.area.bottom().saturating_sub(1)
        };
        sel.update(edge_row, col, scroll);
    }

    pub fn tick_edge_scroll(&mut self) -> Dirty {
        let (dir, zone, col) = match self.selection_state {
            Some(SelectionState::Dragging {
                ref sel,
                ref mut edge_scroll,
                last_drag_col,
            }) => {
                let Some(es) = edge_scroll else {
                    return Dirty::NO;
                };
                if es.last_tick.elapsed() < EDGE_SCROLL_INTERVAL {
                    return Dirty::NO;
                }
                let dir = es.dir;
                es.last_tick = Instant::now();
                (dir, sel.zone, last_drag_col)
            }
            _ => return Dirty::NO,
        };

        self.scroll_zone(zone, dir);
        self.update_selection_to_edge(zone, col);
        Dirty::YES
    }

    pub(super) fn copy_selection(
        &mut self,
        buf: &mut ratatui::buffer::Buffer,
        sel: &Selection,
        render_chat: usize,
    ) {
        let text = match sel.zone {
            SelectionZone::Messages => {
                let msg_area = self.msg_area();
                self.chats[render_chat].extract_selection_text(sel, msg_area)
            }
            SelectionZone::Input => {
                let scroll = self.scroll_offset(sel.zone);
                let Some(screen_sel) = sel.to_screen(scroll) else {
                    self.selection_state = None;
                    return;
                };
                let input_box = self.active_input_box();
                let copy_text = input_box.copy_text();
                let input_area = sel.area;
                let line_breaks = input_box.line_breaks(input_area.width);
                let regions = [ContentRegion {
                    area: input_area,
                    raw_text: &copy_text,
                    line_breaks,
                }];
                selection::extract_selected_text(buf, &screen_sel, &regions)
            }
            SelectionZone::Overlay => {
                let scroll = self.scroll_offset(sel.zone);
                let Some(screen_sel) = sel.to_screen(scroll) else {
                    self.selection_state = None;
                    return;
                };
                let regions = [ContentRegion {
                    area: sel.area,
                    ..Default::default()
                }];
                selection::extract_selected_text(buf, &screen_sel, &regions)
            }
        };

        match self.clipboard.copy_text(&text) {
            Ok(CopyResult::Noop) => {}
            Ok(CopyResult::Copied) => self.status_bar.flash("Copied selection".into()),
            Err(e) => self.status_bar.flash(format!("Copy failed: {e}")),
        }
        self.selection_state = None;
    }

    pub(super) fn zone_at(&self, row: u16, col: u16) -> Option<selection::SelectableZone> {
        self.zones.zone_at(row, col)
    }

    fn queue_hit_at(&self, row: u16, col: u16) -> Option<QueueHit> {
        let position = Position::new(col, row);
        self.queue_hits
            .iter()
            .rev()
            .find(|hit| hit.area.contains(position))
            .copied()
    }

    fn admission_hit_at(
        &self,
        row: u16,
        col: u16,
    ) -> Option<crate::components::input::AdmissionHit> {
        let position = Position::new(col, row);
        self.admission_hits
            .iter()
            .find(|hit| hit.area.contains(position))
            .copied()
    }

    fn status_hit_at(&self, row: u16, col: u16) -> Option<StatusBarHit> {
        let position = Position::new(col, row);
        self.status_hits
            .iter()
            .find(|hit| hit.area.contains(position))
            .copied()
    }

    fn handle_status_click(&mut self, hit: StatusBarHit) -> Vec<crate::components::Action> {
        match hit.target {
            StatusBarHitTarget::BackToMain if !self.is_main_chat() => {
                let _ = self.focus_task(MAIN_TASK_ID);
                Vec::new()
            }
            StatusBarHitTarget::Mode if self.is_main_chat() && !self.is_bash_input() => {
                self.toggle_mode()
            }
            StatusBarHitTarget::Model if self.is_main_chat() => {
                self.clear_control_hovers();
                self.run_builtin(maki_lua::BuiltinAction::ModelPicker)
            }
            StatusBarHitTarget::Thinking
                if self.is_main_chat() && self.state.model.supports_thinking() =>
            {
                self.cycle_reasoning_effort();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_queue_click(&mut self, hit: QueueHit) {
        self.queue_hover = None;
        match hit.target {
            QueueHitTarget::ToggleTogether => self.toggle_active_queue_delivery(),
            QueueHitTarget::Item { .. } if self.queue_editor_active() => {}
            QueueHitTarget::Item { id, action } => match action {
                QueueAction::Select => self.select_active_queue_item(id),
                QueueAction::Edit => {
                    self.select_active_queue_item(id);
                    self.begin_queue_edit(id);
                }
                QueueAction::Delete => {
                    self.delete_active_queue_item(id);
                }
                QueueAction::MoveMain => self.move_unsent_to_main(id),
            },
        }
    }

    fn route_overlay_mouse<T>(
        &mut self,
        event: MouseEvent,
        dispatch: impl FnOnce(&mut Self, MouseEvent) -> T,
        map: impl FnOnce(&mut Self, T) -> Vec<crate::components::Action>,
    ) -> Option<Vec<crate::components::Action>> {
        self.clear_control_hovers();
        if event.kind == MouseEventKind::Up(MouseButton::Left)
            && matches!(
                &self.selection_state,
                Some(SelectionState::Dragging { sel, .. }) if !sel.is_empty()
            )
        {
            return None;
        }

        let action = dispatch(self, event);
        let actions = map(self, action);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {
                (!actions.is_empty()).then_some(actions)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.selection_state = None;
                Some(actions)
            }
            _ => Some(actions),
        }
    }

    fn clear_control_hovers(&mut self) {
        self.admission_hover = None;
        self.queue_hover = None;
        self.status_hover = None;
        self.input_box.clear_hover();
        self.subagent_input_box.clear_hover();
        for chat in &mut self.chats {
            chat.clear_hover();
        }
    }

    fn update_input_hover(&mut self, row: u16, col: u16) {
        let input_area = self
            .zone_at(row, col)
            .filter(|zone| zone.zone == SelectionZone::Input)
            .map(|zone| zone.area);
        self.input_box.clear_hover();
        self.subagent_input_box.clear_hover();
        if let Some(area) = input_area {
            let focused = !self.any_overlay_open();
            self.active_input_box_mut()
                .update_paste_hover(area, row, col, focused);
        }
    }

    fn update_transcript_hover(&mut self, row: u16, col: u16) {
        let area = self.msg_area();
        let known_task_target = if self.active_chat == 0 {
            self.chats[0]
                .tool_id_at(row, area)
                .map(|tool_id| {
                    self.parent_task_ids
                        .get(tool_id)
                        .map_or(tool_id, String::as_str)
                })
                .is_some_and(|task_id| {
                    self.chats
                        .iter()
                        .any(|chat| chat.task_id().is_some_and(|id| &**id == task_id))
                })
        } else {
            false
        };
        self.chats[self.active_chat].update_hover(row, col, area, known_task_target);
    }

    pub(super) fn scroll_offset(&self, zone: SelectionZone) -> u32 {
        match zone {
            SelectionZone::Messages => self.chats[self.active_chat].scroll_top() as u32,
            SelectionZone::Input => self.active_input_box().scroll_y() as u32,
            SelectionZone::Overlay => 0,
        }
    }

    pub(super) fn scroll_zone(&mut self, zone: SelectionZone, delta: i32) {
        match zone {
            SelectionZone::Messages => self.chats[self.active_chat].scroll(delta),
            SelectionZone::Input => self.active_input_box_mut().scroll(delta),
            SelectionZone::Overlay => {}
        }
    }

    pub(super) fn msg_area(&self) -> Rect {
        self.zones
            .find(SelectionZone::Messages)
            .map(|z| {
                let a = z.area;
                Rect::new(a.x, a.y, a.width.saturating_sub(1), a.height)
            })
            .unwrap_or_default()
    }
}
