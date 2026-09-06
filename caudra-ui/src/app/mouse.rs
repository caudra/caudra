use std::time::{Duration, Instant};

use crate::clipboard::CopyResult;
use crate::components::Overlay;
use crate::components::permission_prompt::PromptMouse;
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
        if self.workbench.is_open() {
            self.clear_control_hovers();
            let action = self.workbench.handle_mouse(event);
            return self.handle_workbench_action(action);
        }
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
        if passive_modal_open {
            self.clear_control_hovers();
        } else if self.permission_prompt.is_open() {
            self.clear_control_hovers();
            match self.permission_prompt.handle_mouse(event) {
                PromptMouse::Passthrough => {}
                PromptMouse::Consumed => return Vec::new(),
                PromptMouse::Decided(decision) => {
                    self.apply_permission_decision(decision);
                    return Vec::new();
                }
            }
        } else if self.permissions_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.permissions_picker.handle_mouse(event),
                |app, action| app.handle_permissions_picker_action(action),
            ) {
                return actions;
            }
        } else if self.stash_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.stash_picker.handle_mouse(event),
                |app, action| app.handle_stash_picker_action(action),
            ) {
                return actions;
            }
        } else if self.session_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.session_picker.handle_mouse(event),
                |app, action| app.handle_session_picker_action(action),
            ) {
                return actions;
            }
        } else if self.task_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.task_picker.handle_mouse(event),
                |app, action| app.handle_task_picker_action(action),
            ) {
                return actions;
            }
        } else if self.memory_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.memory_picker.handle_mouse(event),
                |app, action| app.handle_memory_picker_action(action),
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
        } else if self.command_modal.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.command_modal.handle_mouse(event),
                |app, action| app.handle_command_modal_action(action),
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
        } else if self.prompt_profile_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.prompt_profile_picker.handle_mouse(event),
                |app, action| app.handle_prompt_profile_picker_action(action),
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
        } else if self.review.is_open() {
            let action = self.review.handle_mouse(event);
            if !matches!(action, crate::components::review::ReviewAction::Passthrough) {
                self.clear_control_hovers();
                return self.handle_review_action(action);
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
        // Docked and not modal, so it is asked last and only acts on what it
        // drew: a drag that selected text releases as a selection rather than
        // pressing whatever it ended over.
        if self.question_form.is_open()
            && !(event.kind == MouseEventKind::Up(MouseButton::Left) && self.dragging_selection())
            && let Some(action) = self.question_form.handle_mouse(event)
        {
            self.clear_control_hovers();
            if event.kind == MouseEventKind::Up(MouseButton::Left) {
                self.selection_state = None;
            }
            return self.handle_question_form_action(action);
        }
        // Bottom-stack chrome rather than an overlay, so it is checked after
        // the overlay chain and only when nothing modal is drawn over it.
        if !self.has_modal_overlay() && self.todo_panel.handle_mouse(event) {
            self.clear_control_hovers();
            return Vec::new();
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
                self.task_hint_mouse_down = false;
                self.queue_mouse_down = None;
                self.status_mouse_down = None;
                self.message_mouse_down = None;
                self.link_mouse_down = None;
                if !self.has_modal_overlay() {
                    self.admission_mouse_down = self.admission_hit_at(event.row, event.column);
                    self.task_hint_mouse_down = self.task_hint_hit_at(event.row, event.column);
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
                        && !self.has_modal_overlay()
                        && crate::terminal::local_url_opener_available()
                    {
                        self.link_mouse_down = self.chats[self.active_chat].link_at(
                            event.row,
                            event.column,
                            self.msg_area(),
                        );
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
                self.task_hint_mouse_down = false;
                self.queue_mouse_down = None;
                self.status_mouse_down = None;
                self.message_mouse_down = None;
                self.link_mouse_down = None;
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
                    self.link_mouse_down = None;
                    self.task_hint_mouse_down = false;
                    return self.handle_streaming_admission(pressed.admission);
                }
                if !self.has_modal_overlay()
                    && std::mem::take(&mut self.task_hint_mouse_down)
                    && self.task_hint_hit_at(event.row, event.column)
                {
                    self.queue_mouse_down = None;
                    self.status_mouse_down = None;
                    self.message_mouse_down = None;
                    self.link_mouse_down = None;
                    return self.tasks_browse();
                }
                if let Some(SelectionState::Dragging { sel, .. }) = self.selection_state {
                    if !sel.is_empty() {
                        self.queue_mouse_down = None;
                        self.link_mouse_down = None;
                        self.selection_state = Some(SelectionState::PendingCopy { sel });
                    } else {
                        let zone = sel.zone;
                        self.selection_state = None;
                        if zone == SelectionZone::Messages
                            && !self.has_modal_overlay()
                            && self
                                .zone_at(event.row, event.column)
                                .is_some_and(|zone| zone.zone == SelectionZone::Messages)
                            && let Some(target) = self.link_mouse_down.take()
                            && self.chats[self.active_chat]
                                .link_at(event.row, event.column, self.msg_area())
                                .as_deref()
                                == Some(target.as_ref())
                        {
                            self.message_mouse_down = None;
                            self.queue_mouse_down = None;
                            self.status_mouse_down = None;
                            return vec![crate::components::Action::OpenUrl(target.to_string())];
                        }
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
                                && let Some(task_id) = self.task_id_at(event.row, area)
                                && self.focus_task(&task_id).is_ok()
                            {
                                return Vec::new();
                            }
                            self.chats[self.active_chat].handle_click(event.row, area);
                        }
                    }
                }
                self.admission_mouse_down = None;
                self.task_hint_mouse_down = false;
                self.queue_mouse_down = None;
                self.status_mouse_down = None;
                self.message_mouse_down = None;
                self.link_mouse_down = None;
            }
            MouseEventKind::Moved => {
                if self.has_modal_overlay() {
                    self.clear_control_hovers();
                    return Vec::new();
                }
                self.admission_hover = self
                    .admission_hit_at(event.row, event.column)
                    .map(|hit| hit.admission);
                self.task_hint_hover = self.task_hint_hit_at(event.row, event.column);
                self.queue_hover = self
                    .queue_hit_at(event.row, event.column)
                    .map(|hit| hit.target);
                self.status_hover = self
                    .status_hit_at(event.row, event.column)
                    .map(|hit| hit.target);
                self.update_input_hover(event.row, event.column);
                self.update_transcript_hover(event.row, event.column);
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                if self.has_modal_overlay() {
                    return Vec::new();
                }
                // Hover decides the target, so a sideways wheel only reaches
                // the diagram the pointer is actually over.
                self.update_transcript_hover(event.row, event.column);
                let delta = match event.kind {
                    MouseEventKind::ScrollLeft => -super::PAN_STEP,
                    _ => super::PAN_STEP,
                };
                self.chats[self.active_chat].pan_hovered_diagram(delta);
            }
            _ => {}
        }
        Vec::new()
    }

    pub(super) fn handle_scroll(&mut self, column: u16, row: u16, delta: i32) {
        // The wheel is aggregated into `Msg::Scroll` before `handle_mouse` ever
        // runs, so the workbench has to be offered it here as well or its panes
        // never see a wheel at all. Its rows count downwards.
        if self.workbench.is_open() {
            self.workbench.scroll(column, row, -delta as isize);
            return;
        }
        if self.paste_editor.is_open() {
            self.paste_editor.scroll(delta);
            return;
        }
        if !self.has_modal_overlay() && self.queue_hit_at(row, column).is_some() {
            self.scroll_active_queue(delta);
            self.clear_selection_unless_pending_copy();
            return;
        }
        // The panel caps at `MAX_VISIBLE_ROWS`, so without this the rows past
        // it cannot be reached by any input at all.
        if !self.has_modal_overlay() && self.todo_panel.contains(Position::new(column, row)) {
            self.todo_panel.scroll(delta);
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

    fn task_hint_hit_at(&self, row: u16, col: u16) -> bool {
        self.task_hint_hit.contains(Position::new(col, row))
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
                self.run_builtin(caudra_lua::BuiltinAction::ModelPicker)
            }
            StatusBarHitTarget::Thinking
                if self.is_main_chat() && self.state.model.supports_thinking() =>
            {
                self.cycle_reasoning_effort();
                Vec::new()
            }
            StatusBarHitTarget::Goal if self.is_main_chat() => {
                self.clear_control_hovers();
                self.goal_modal.open();
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
        if event.kind == MouseEventKind::Up(MouseButton::Left) && self.dragging_selection() {
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

    /// A drag that has covered ground, so the release belongs to the selection
    /// rather than to whatever control it happened to end over.
    fn dragging_selection(&self) -> bool {
        matches!(
            &self.selection_state,
            Some(SelectionState::Dragging { sel, .. }) if !sel.is_empty()
        )
    }

    fn clear_control_hovers(&mut self) {
        self.admission_hover = None;
        self.task_hint_hover = false;
        self.queue_hover = None;
        self.status_hover = None;
        self.input_box.clear_hover();
        self.subagent_input_box.clear_hover();
        self.todo_panel.clear_hover();
        self.permission_prompt.clear_hover();
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

    /// The transcript a click at `row` would open. A batch child is asked
    /// about first: its row sits inside the batch's card, so the card's own id
    /// answers for it and would send every roster row to the same place.
    fn task_id_at(&self, row: u16, area: Rect) -> Option<String> {
        let dispatched = self.chats[0]
            .dispatched_id_at(row, area)
            .and_then(|id| self.parent_task_ids.get(&id).cloned());
        dispatched.or_else(|| {
            let tool_id = self.chats[0].tool_id_at(row, area)?;
            Some(
                self.parent_task_ids
                    .get(tool_id)
                    .cloned()
                    .unwrap_or_else(|| tool_id.to_owned()),
            )
        })
    }

    fn update_transcript_hover(&mut self, row: u16, col: u16) {
        let area = self.msg_area();
        let known_task_target = self.active_chat == 0
            && self.task_id_at(row, area).is_some_and(|task_id| {
                self.chats
                    .iter()
                    .any(|chat| chat.task_id().is_some_and(|id| **id == task_id))
            });
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
