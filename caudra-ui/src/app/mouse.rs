use std::time::{Duration, Instant};

use crate::agent::AgentCommand;
use crate::components::command::ChatScope;
use crate::components::help_modal::HelpMouse;
use crate::components::input::{ChordHint, InputHit};
use crate::components::paste_editor::PasteEditorAction;
use crate::components::permission_prompt::PromptMouse;
use crate::components::queue_panel::{QueueAction, QueueHit, QueueHitTarget};
use crate::components::status_bar::{StatusBarHit, StatusBarHitTarget};
use crate::components::stream_modal::StreamAction;
use crate::components::workflow_card::CardHit;
use crate::components::{Action, Overlay};
use crate::selection::{self, ContentRegion, EdgeScroll, Selection, SelectionState, SelectionZone};
use caudra_agent::{CommitRef, Mention};
use caudra_storage::sessions::PermissionMode;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use std::path::PathBuf;

use crate::repaint::Dirty;

use super::tasks::MAIN_TASK_ID;
use super::{AUTO_OFF_MSG, App, FAST_OFF_MSG, KeyFocus, YOLO_OFF_MSG};

pub(super) const EDGE_SCROLL_LINES: i32 = 1;
pub(super) const EDGE_SCROLL_INTERVAL: Duration = Duration::from_millis(25);
const MESSAGE_ACTIONS_UNAVAILABLE: &str = "Message actions unavailable here";
const SELECTION_COPIED: &str = "Copied selection";
/// Rows either side of the origin that scroll nothing, so parking the pointer
/// where it started holds the view still.
const AUTOSCROLL_DEAD_ZONE: i32 = 2;
/// Divides the squared distance. Small enough that the first rows past the
/// dead zone creep, large enough that the edge of a tall pane still flies.
const AUTOSCROLL_RAMP: i32 = 6;
/// Rows per tick at full tilt. The tick runs on [`Cadence::SMOOTH`], so this is
/// a rate per frame rather than per second.
const AUTOSCROLL_MAX_ROWS: i32 = 8;

/// Velocity scrolling anchored on a middle press: the view moves on its own,
/// and how fast is the pointer's distance from where the press landed.
pub(crate) struct Autoscroll {
    origin: Position,
    pointer: Position,
}

impl Autoscroll {
    pub(crate) fn origin(&self) -> Position {
        self.origin
    }
}

/// Quadratic rather than linear so one gesture covers both a nudge and a leap
/// through a long document. Negative is upwards, matching the wheel.
fn autoscroll_rows(distance: i32) -> i32 {
    let past = distance.abs() - AUTOSCROLL_DEAD_ZONE;
    if past <= 0 {
        return 0;
    }
    let speed = (past * past / AUTOSCROLL_RAMP).clamp(1, AUTOSCROLL_MAX_ROWS);
    match distance > 0 {
        true => -speed,
        false => speed,
    }
}

impl App {
    pub(super) fn handle_mouse(&mut self, event: MouseEvent) -> Vec<Action> {
        if self.mode_submission.is_open()
            && !self.permission_prompt.is_open()
            && !self.question_form.is_open()
        {
            self.clear_control_hovers();
            self.autoscroll = None;
            self.selection_state = None;
            let action = self.mode_submission.handle_mouse(event);
            return self.handle_mode_submission(action);
        }
        if self.sandbox_manager.is_open() {
            self.clear_control_hovers();
            self.autoscroll = None;
            self.selection_state = None;
            let action = self.sandbox_manager.handle_mouse(event);
            self.handle_sandbox_action(action);
            return Vec::new();
        }
        let at = Position::new(event.column, event.row);
        #[cfg(debug_assertions)]
        if let Some(actions) = self.handle_grab_mouse(event) {
            return actions;
        }
        if self.permissions_picker.is_open() && self.permissions_picker.editor_mut().is_some() {
            self.clear_control_hovers();
            if self.permission_mutation_pending() {
                return Vec::new();
            }
            let action = self.permissions_picker.handle_mouse(event);
            return self.handle_permissions_picker_action(action);
        }
        if self.peer_manager.is_open() && !self.paste_editor.is_open() {
            self.clear_control_hovers();
            self.autoscroll = None;
            self.selection_state = None;
            let action = self.peer_manager.handle_mouse(event);
            return self.handle_peer_manager_action(action);
        }
        if self.session_relocation_picker.is_open() {
            self.clear_control_hovers();
            self.autoscroll = None;
            self.selection_state = None;
            if event.kind == MouseEventKind::Down(MouseButton::Left)
                && !self.session_relocation_picker.contains(at)
            {
                self.session_relocation_picker.close();
                return Vec::new();
            }
            let action = self.session_relocation_picker.handle_mouse(event);
            return self.handle_session_relocation_action(action);
        }
        if self.worktree_picker.is_open() {
            self.clear_control_hovers();
            self.autoscroll = None;
            self.selection_state = None;
            if event.kind == MouseEventKind::Down(MouseButton::Left)
                && !self.worktree_picker.contains(at)
            {
                self.worktree_picker.close();
                return Vec::new();
            }
            let action = self.worktree_picker.handle_mouse(event);
            return self.handle_worktree_action(action);
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Middle) => {
                self.toggle_autoscroll(at);
                return Vec::new();
            }
            // Pointer distance from the origin is the throttle, so every report
            // of where it is moves it, held button or not.
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                if let Some(auto) = &mut self.autoscroll {
                    auto.pointer = at;
                    return Vec::new();
                }
            }
            MouseEventKind::Down(_) => self.autoscroll = None,
            _ => {}
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(actions) = self.dismiss_at(Position::new(event.column, event.row))
        {
            return actions;
        }
        if self.stream_modal.is_open() && !self.permission_prompt.is_open() {
            self.clear_control_hovers();
            match self.stream_modal.handle_mouse(&event) {
                StreamAction::Ignored => {}
                StreamAction::Consumed => return Vec::new(),
                StreamAction::Copy(text) => {
                    self.copy_to_clipboard(&text);
                    return Vec::new();
                }
                StreamAction::Submit(question) => {
                    self.continue_btw(question);
                    return Vec::new();
                }
            }
        }
        if self.workbench.is_open()
            && !self.stream_modal.is_open()
            && !self.permission_prompt.is_open()
        {
            self.clear_control_hovers();
            if self.question_form.is_open()
                && let Some(action) = self.question_form.handle_mouse(event)
            {
                return self.handle_question_form_action(action);
            }
            if self.question_form.contains(at) {
                return Vec::new();
            }
            let action = self.workbench.handle_mouse(event);
            return self.handle_workbench_action(action);
        }
        if self.paste_editor.is_open() {
            self.clear_control_hovers();
            if let PasteEditorAction::Copy(text) = self.paste_editor.handle_mouse(&event) {
                self.copy_to_clipboard(&text);
            }
            return Vec::new();
        }
        if self.logs_modal.is_open() && !self.permission_prompt.is_open() {
            self.clear_control_hovers();
            let action = self.logs_modal.handle_mouse(event);
            self.handle_logs_action(action);
            return Vec::new();
        }
        if self.docs_modal.is_open() && !self.permission_prompt.is_open() {
            self.clear_control_hovers();
            let action = self.docs_modal.handle_mouse(event);
            return self.handle_docs_action(action);
        }
        if self.context_modal.is_open() && !self.permission_prompt.is_open() {
            self.clear_control_hovers();
            self.context_modal.handle_mouse(event);
            return Vec::new();
        }
        if self.tools_modal.is_open() && !self.permission_prompt.is_open() {
            self.clear_control_hovers();
            self.tools_modal.handle_mouse(event);
            return Vec::new();
        }
        if self.decisions_modal.is_open() && !self.permission_prompt.is_open() {
            self.clear_control_hovers();
            let action = self.decisions_modal.handle_mouse(event);
            self.handle_decisions_action(action);
            return Vec::new();
        }
        if self.skills_modal.is_open() && !self.permission_prompt.is_open() {
            self.clear_control_hovers();
            self.skills_modal.handle_mouse(event);
            return Vec::new();
        }
        if self.storage_modal.is_open() && !self.permission_prompt.is_open() {
            self.clear_control_hovers();
            self.storage_modal.handle_mouse(event);
            return Vec::new();
        }
        if self.system_prompt_modal.is_open() && !self.permission_prompt.is_open() {
            self.clear_control_hovers();
            let action = self.system_prompt_modal.handle_mouse(event);
            self.handle_system_prompt_action(action);
            return Vec::new();
        }
        if self.projection_modal.is_open() && !self.permission_prompt.is_open() {
            self.clear_control_hovers();
            let action = self.projection_modal.handle_mouse(event);
            self.handle_projection_action(action);
            return Vec::new();
        }
        // The goal footer names session commands rather than modal state, and
        // `/goal-model` opens a picker this modal is drawn over, so the click
        // closes the modal before the command runs.
        if self.goal_modal.is_open() && !self.permission_prompt.is_open() {
            self.clear_control_hovers();
            let Some(cmdline) = self.goal_modal.handle_mouse(event) else {
                return Vec::new();
            };
            self.goal_modal.close();
            return self.run_footer_command(cmdline);
        }
        let passive_modal_open =
            self.help_modal.is_open() || self.usage_modal.is_open() || self.float_mgr.is_open();
        if passive_modal_open {
            self.clear_control_hovers();
            // Help keeps every key while it is up, so it closes before its
            // footer's command runs or the docs it opens could not be read.
            match self.help_modal.handle_mouse(&event) {
                HelpMouse::Ignored => {}
                HelpMouse::Consumed => return Vec::new(),
                HelpMouse::Command(cmdline) => {
                    self.help_modal.close();
                    return self.run_footer_command(cmdline);
                }
            }
            // Neither of these reads the pointer for anything but its bar, and
            // at most one is up, so the first taker wins and the other no-ops.
            if self.usage_modal.handle_mouse(&event) || self.float_mgr.handle_mouse(&event) {
                return Vec::new();
            }
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
        } else if self.shell_modal.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.shell_modal.handle_mouse(event),
                |app, action| app.handle_shell_modal_action(action),
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
        } else if self.workflow_inspector.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.workflow_inspector.handle_mouse(event),
                |app, action| app.handle_workflow_inspector_action(action),
            ) {
                return actions;
            }
        } else if self.workflow_catalog_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.workflow_catalog_picker.handle_mouse(event),
                |app, action| app.handle_workflow_catalog_action(action),
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
        } else if self.thinking_picker.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.thinking_picker.handle_mouse(event),
                |app, action| app.handle_thinking_picker_action(action),
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
        } else if self.queue_actions.is_open() {
            if let Some(actions) = self.route_overlay_mouse(
                event,
                |app, event| app.queue_actions.handle_mouse(event),
                |app, action| app.handle_queue_actions_action(action),
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
            let input = self.active_input_box().buffer.value();
            let action = self.command_palette.handle_mouse(event, &input);
            if let Some(actions) = self.handle_command_action(action) {
                self.clear_control_hovers();
                return actions;
            }
        } else if self.mention_popup.is_open() {
            let action = self.mention_popup.handle_mouse(event);
            if let Some(actions) = self.handle_mention_action(action) {
                self.clear_control_hovers();
                return actions;
            }
        } else if self.commit_popup.is_open() {
            let action = self.commit_popup.handle_mouse(event);
            if let Some(actions) = self.handle_commit_action(action) {
                self.clear_control_hovers();
                return actions;
            }
        }
        // Docked and not modal, so it is asked last and only acts on what it
        // drew: a drag that selected text releases as a selection rather than
        // pressing whatever it ended over.
        if self.question_form.is_open()
            && !self.stream_modal.is_open()
            && !self.permission_prompt.is_open()
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
        // Ahead of the selection below, which would otherwise read a press on
        // a bar as the start of a sweep down the surface behind it. Both
        // columns sit outside the areas selection measures against, so nothing
        // else wants them.
        // A card's bar is ahead of the transcript's own: it sits inside the
        // body rather than beside it, so the transcript's column never claims
        // it, but the order says which one owns a press either could take.
        if !self.has_modal_overlay()
            && (self.chats[self.active_chat].handle_card_scrollbar(&event)
                || self.chats[self.active_chat].handle_scrollbar(&event)
                || self.active_input_box_mut().handle_scrollbar(&event))
        {
            self.clear_selection_unless_pending_copy();
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
                self.open_message_actions(source);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.admission_mouse_down = None;
                self.chord_hint_down = None;
                self.queue_mouse_down = None;
                self.status_mouse_down = None;
                self.message_action_mouse_down = None;
                self.link_mouse_down = None;
                self.mention_mouse_down = None;
                if !self.has_modal_overlay() {
                    self.admission_mouse_down = self.admission_hit_at(event.row, event.column);
                    self.chord_hint_down = self.chord_hint_at(event.row, event.column);
                    self.status_mouse_down = self
                        .status_hit_at(event.row, event.column)
                        .filter(|hit| hit.target.accepts_click());
                    self.queue_mouse_down = self.queue_hit_at(event.row, event.column);
                    if self.queue_mouse_down.is_none() {
                        self.unfocus_active_queue();
                    }
                }
                if let Some(zone) = self.zone_at(event.row, event.column) {
                    if self.has_modal_overlay() && zone.zone != SelectionZone::Overlay {
                        return Vec::new();
                    }
                    // The second way the transcript takes the navigation keys.
                    // The wheel deliberately stays out of it: scrolling past
                    // something must not disarm Home in a half-typed draft.
                    match zone.zone {
                        SelectionZone::Input => self.key_focus = KeyFocus::Composer,
                        SelectionZone::Messages => self.key_focus = KeyFocus::Transcript,
                        SelectionZone::Overlay => {}
                    }
                    if zone.zone == SelectionZone::Messages
                        && self.is_main_chat()
                        && !self.has_modal_overlay()
                        && let Some(target) =
                            self.chats[0].message_action_at(event.row, event.column)
                    {
                        self.message_action_mouse_down = Some(target);
                        return Vec::new();
                    }
                    // Move the cursor to the click position in the input area.
                    if zone.zone == SelectionZone::Input {
                        let focused = self.composer_holds_keys();
                        let hit = self.active_input_box_mut().handle_click(
                            zone.area,
                            event.row,
                            event.column,
                            focused,
                        );
                        match hit {
                            Some(InputHit::Paste(id)) => {
                                self.selection_state = None;
                                self.open_paste_editor(id);
                                return Vec::new();
                            }
                            Some(InputHit::Mention { mention, .. }) => {
                                self.selection_state = None;
                                self.open_workbench_at(&mention);
                                return Vec::new();
                            }
                            // The caret moved, so the `@` popup has to decide
                            // again whether it is still inside its query.
                            None => self.resync_dropdowns(),
                        }
                    }
                    if zone.zone == SelectionZone::Messages && !self.has_modal_overlay() {
                        // On the press, not the release: touch reports no held
                        // drag, so the release path below never runs for a tap.
                        // It does not consume the press either, or a sweep
                        // could not start inside a card body.
                        self.chats[self.active_chat].arm_card_at(event.column, event.row);
                        // Not gated on an opener the way a link is: the
                        // workbench is ours to open.
                        self.mention_mouse_down =
                            self.transcript_mention_at(event.row, event.column);
                        self.commit_mouse_down = self.transcript_commit_at(event.row, event.column);
                        if crate::terminal::local_url_opener_available() {
                            self.link_mouse_down = self.chats[self.active_chat].link_at(
                                event.row,
                                event.column,
                                self.msg_area(),
                            );
                        }
                    }
                    // A sweep needs a held drag, which touch never reports: the
                    // press above is half of a tap that has already released.
                    // Starting a selection here would leave one pinned open on
                    // every tap, and the terminal's own long-press selection is
                    // the one that works anyway.
                    if !caudra_workbench::scroll::touch() {
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
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.clear_control_hovers();
                self.admission_mouse_down = None;
                self.chord_hint_down = None;
                self.queue_mouse_down = None;
                self.status_mouse_down = None;
                self.message_action_mouse_down = None;
                self.link_mouse_down = None;
                self.mention_mouse_down = None;
                self.handle_drag(event.row, event.column);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if !self.has_modal_overlay()
                    && let Some(pressed) = self.admission_mouse_down.take()
                    && self.admission_hit_at(event.row, event.column) == Some(pressed)
                {
                    self.queue_mouse_down = None;
                    self.status_mouse_down = None;
                    self.message_action_mouse_down = None;
                    self.link_mouse_down = None;
                    self.mention_mouse_down = None;
                    self.chord_hint_down = None;
                    return self.handle_streaming_admission(pressed.admission);
                }
                if !self.has_modal_overlay()
                    && let Some(pressed) = self.chord_hint_down.take()
                    && self.chord_hint_at(event.row, event.column) == Some(pressed)
                {
                    self.queue_mouse_down = None;
                    self.status_mouse_down = None;
                    self.message_action_mouse_down = None;
                    self.link_mouse_down = None;
                    self.mention_mouse_down = None;
                    return self.press_chord_hint(pressed);
                }
                if !self.has_modal_overlay()
                    && self
                        .zone_at(event.row, event.column)
                        .is_some_and(|zone| zone.zone == SelectionZone::Messages)
                    && let Some(pressed) = self.message_action_mouse_down.take()
                    && self.chats[0].message_action_at(event.row, event.column) == Some(pressed)
                {
                    self.queue_mouse_down = None;
                    self.status_mouse_down = None;
                    self.link_mouse_down = None;
                    self.mention_mouse_down = None;
                    self.open_message_actions(pressed.source());
                    return Vec::new();
                }
                if let Some(SelectionState::Dragging { sel, .. }) = self.selection_state {
                    if !sel.is_empty() {
                        self.queue_mouse_down = None;
                        self.link_mouse_down = None;
                        self.mention_mouse_down = None;
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
                            self.message_action_mouse_down = None;
                            self.queue_mouse_down = None;
                            self.status_mouse_down = None;
                            return vec![Action::OpenUrl(target.to_string())];
                        }
                        if zone == SelectionZone::Messages
                            && !self.has_modal_overlay()
                            && let Some(pressed) = self.mention_mouse_down.take()
                            && self.transcript_mention_at(event.row, event.column)
                                == Some(pressed.clone())
                        {
                            self.message_action_mouse_down = None;
                            self.queue_mouse_down = None;
                            self.status_mouse_down = None;
                            self.open_workbench_at(&pressed);
                            return Vec::new();
                        }
                        if zone == SelectionZone::Messages
                            && !self.has_modal_overlay()
                            && let Some(pressed) = self.commit_mouse_down.take()
                            && self.transcript_commit_at(event.row, event.column)
                                == Some(pressed.clone())
                        {
                            self.message_action_mouse_down = None;
                            self.queue_mouse_down = None;
                            self.status_mouse_down = None;
                            self.open_workbench_commit(&pressed);
                            return Vec::new();
                        }
                        self.message_action_mouse_down = None;
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
                            return self.handle_queue_click(pressed);
                        }
                        if zone == SelectionZone::Messages {
                            let area = self.msg_area();
                            if self.active_chat == 0
                                && let Some(hit) = self.chats[0].workflow_hit_at(event.row, area)
                            {
                                match hit {
                                    CardHit::Run(run_id) => {
                                        self.open_workflow_inspector(Some(&run_id));
                                    }
                                    CardHit::ScratchFile(path) => {
                                        self.open_workbench_file(&path, None);
                                    }
                                }
                                return Vec::new();
                            }
                            if self.active_chat == 0
                                && let Some(path) = self.chats[0].memory_hit_at(event.row, area)
                            {
                                self.open_memory_note(&path);
                                return Vec::new();
                            }
                            if self.active_chat == 0
                                && let Some(task_id) = self.task_id_at(event.row, area)
                                && self.focus_task(&task_id).is_ok()
                            {
                                return Vec::new();
                            }
                            // The press already armed this window, and that is
                            // the whole of what it does: the card's own control
                            // is its header, above the window.
                            if self.chats[self.active_chat].armed_card_at(event.column, event.row) {
                                return Vec::new();
                            }
                            self.chats[self.active_chat].handle_click(event.row, area);
                        }
                    }
                }
                self.admission_mouse_down = None;
                self.chord_hint_down = None;
                self.queue_mouse_down = None;
                self.status_mouse_down = None;
                self.message_action_mouse_down = None;
                self.link_mouse_down = None;
                self.mention_mouse_down = None;
            }
            MouseEventKind::Moved => {
                if self.has_modal_overlay() {
                    self.clear_control_hovers();
                    return Vec::new();
                }
                self.admission_hover = self
                    .admission_hit_at(event.row, event.column)
                    .map(|hit| hit.admission);
                self.chord_hint_hover = self.chord_hint_at(event.row, event.column);
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
                let delta = match event.kind {
                    MouseEventKind::ScrollLeft => -super::PAN_STEP,
                    _ => super::PAN_STEP,
                };
                if self.has_modal_overlay() {
                    self.pan_modal(delta);
                    return Vec::new();
                }
                // Hover decides the target, so a sideways wheel only reaches
                // the diagram the pointer is actually over.
                self.update_transcript_hover(event.row, event.column);
                self.chats[self.active_chat].pan_hovered_diagram(delta);
            }
            _ => {}
        }
        Vec::new()
    }

    /// A footer that names a session command runs it as typing it would, and
    /// says why when the command is refused.
    fn run_footer_command(&mut self, cmdline: &str) -> Vec<Action> {
        match self.run_cmdline(cmdline, 0) {
            Ok(actions) => actions,
            Err(error) => {
                self.flash(error);
                Vec::new()
            }
        }
    }

    /// Offers a sideways wheel to the open modal. Only the ones that draw
    /// unwrapped lines can use it; the rest reflow, so there is nothing off
    /// screen for a pan to reach and the event is dropped as it was before.
    /// `/logs` and `/docs` are absent because they answer the wheel in their own
    /// `handle_mouse`, which runs long before this.
    fn pan_modal(&mut self, delta: i32) {
        if self.peer_manager.is_open() {
            self.peer_manager.pan(delta);
        } else if self.usage_modal.is_open() {
            self.usage_modal.pan(delta);
        } else if self.tools_modal.is_open() {
            self.tools_modal.pan(delta);
        } else if self.help_modal.is_open() {
            self.help_modal.pan(delta);
        } else if self.system_prompt_modal.is_open() {
            self.system_prompt_modal.pan(delta);
        } else if self.projection_modal.is_open() {
            self.projection_modal.pan(delta);
        }
    }

    pub(super) fn handle_scroll(&mut self, column: u16, row: u16, delta: i32) {
        if self.sandbox_manager.is_open() {
            self.sandbox_manager
                .scroll_at(Position::new(column, row), delta);
            return;
        }
        if self.permissions_picker.is_open() {
            self.permissions_picker
                .scroll_at(Position::new(column, row), delta);
            return;
        }
        if self.peer_manager.is_open() && !self.paste_editor.is_open() {
            self.scroll_at(column, row, delta);
            self.clear_selection_unless_pending_copy();
            return;
        }
        if self.session_relocation_picker.is_open() {
            self.session_relocation_picker.scroll(delta);
            return;
        }
        if self.worktree_picker.is_open() {
            self.worktree_picker.scroll(delta);
            return;
        }
        // The wheel is aggregated into `Msg::Scroll` before `handle_mouse` ever
        // runs, so the workbench has to be offered it here as well or its panes
        // never see a wheel at all. Its rows count downwards.
        if self.stream_modal.is_open() || self.permission_prompt.is_open() {
            self.scroll_at(column, row, delta);
            self.clear_selection_unless_pending_copy();
            return;
        }
        if self.workbench.is_open() {
            if self.question_form.contains(Position::new(column, row)) {
                self.question_form.scroll(delta);
                return;
            }
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
        if zone == SelectionZone::Input {
            let focused = self.composer_holds_keys();
            self.active_input_box_mut()
                .handle_drag(area, row, col, focused);
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

    /// Drives the view while a middle press holds it. Rows per tick rather than
    /// per second, so the rate is the cadence's and the test is a loop.
    pub fn tick_autoscroll(&mut self) -> Dirty {
        let Some(auto) = &self.autoscroll else {
            return Dirty::NO;
        };
        let distance = i32::from(auto.pointer.y) - i32::from(auto.origin.y);
        let delta = autoscroll_rows(distance);
        if delta == 0 {
            return Dirty::NO;
        }
        // Through the wheel's own entry point, so autoscroll reaches exactly
        // what a wheel over the origin would have reached.
        let (column, row) = (auto.origin.x, auto.origin.y);
        self.handle_scroll(column, row, delta);
        Dirty::YES
    }

    /// A second middle press puts it away, which is the only way out that does
    /// not also do something else.
    fn toggle_autoscroll(&mut self, at: Position) {
        self.autoscroll = match self.autoscroll {
            Some(_) => None,
            None => Some(Autoscroll {
                origin: at,
                pointer: at,
            }),
        };
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
            // The sweep tracked screen cells only to drive edge scrolling; what
            // it selected is the document's own range.
            SelectionZone::Input => self.active_input_box().selected_text().unwrap_or_default(),
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

        self.copy_labelled(&text, SELECTION_COPIED);
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

    fn chord_hint_at(&self, row: u16, col: u16) -> Option<ChordHint> {
        let hit = self.chord_hint_hit?;
        hit.area
            .contains(Position::new(col, row))
            .then_some(hit.target)
    }

    /// A click stands in for the chord the hint names, so the two paths can
    /// never advertise one thing and do another.
    fn press_chord_hint(&mut self, target: ChordHint) -> Vec<Action> {
        match target {
            ChordHint::Tasks => self.tasks_browse(),
            ChordHint::PlanOrTodo => {
                self.toggle_plan_or_todo();
                Vec::new()
            }
        }
    }

    fn status_hit_at(&self, row: u16, col: u16) -> Option<StatusBarHit> {
        let position = Position::new(col, row);
        self.status_hits
            .iter()
            .find(|hit| hit.area.contains(position))
            .copied()
    }

    /// The scope table is the bar's, so a control the bar drew inert cannot be
    /// activated by a click landing on a hit rect from an earlier frame.
    fn handle_status_click(&mut self, hit: StatusBarHit) -> Vec<Action> {
        if hit.target.scope() == ChatScope::MainOnly && !self.is_main_chat() {
            return Vec::new();
        }
        match hit.target {
            StatusBarHitTarget::BackToMain if !self.is_main_chat() => {
                let _ = self.focus_task(MAIN_TASK_ID);
                Vec::new()
            }
            StatusBarHitTarget::Mode if !self.is_bash_input() => self.toggle_mode(),
            StatusBarHitTarget::Model => {
                self.clear_control_hovers();
                self.run_builtin(caudra_lua::BuiltinAction::ModelPicker)
            }
            StatusBarHitTarget::Thinking if self.state.model.supports_thinking() => {
                self.clear_control_hovers();
                self.thinking_picker
                    .open(&self.state.model, &self.state.thinking);
                Vec::new()
            }
            StatusBarHitTarget::Goal => {
                self.clear_control_hovers();
                self.goal_modal.open();
                Vec::new()
            }
            StatusBarHitTarget::Tasks => {
                self.clear_control_hovers();
                self.tasks_browse()
            }
            StatusBarHitTarget::Shells => {
                self.clear_control_hovers();
                self.shells_browse()
            }
            StatusBarHitTarget::Context => {
                self.clear_control_hovers();
                self.execute_context("");
                Vec::new()
            }
            StatusBarHitTarget::Usage => {
                self.clear_control_hovers();
                self.toggle_usage_modal()
            }
            StatusBarHitTarget::Workflows => {
                self.clear_control_hovers();
                self.execute_workflow("")
            }
            StatusBarHitTarget::Sandbox => {
                self.clear_control_hovers();
                self.open_sandbox("");
                Vec::new()
            }
            StatusBarHitTarget::Decisions => {
                self.clear_control_hovers();
                self.execute_decisions("");
                Vec::new()
            }
            // The label goes as soon as the transcript follows again, so the
            // hover it was drawn under has nothing left to sit on.
            StatusBarHitTarget::ResumeAutoScroll => {
                self.clear_control_hovers();
                self.active_chat().enable_auto_scroll();
                Vec::new()
            }
            // A click means off, so a hit rect left over from an earlier frame
            // can never switch it back on. The chip goes with the state it
            // warned about, taking the hover it was drawn under with it.
            StatusBarHitTarget::Yolo => {
                self.clear_control_hovers();
                self.permissions.set_session_mode(Some(PermissionMode::Ask));
                self.flash(YOLO_OFF_MSG.into());
                Vec::new()
            }
            StatusBarHitTarget::Auto => {
                self.clear_control_hovers();
                self.permissions.set_session_mode(Some(PermissionMode::Ask));
                self.flash(AUTO_OFF_MSG.into());
                Vec::new()
            }
            // Off only, on the same terms as yolo: the chip is drawn from the
            // state it reports, so a click can only ever retire it. Turning
            // fast back on goes through `/fast`, which is where the model's
            // eligibility is answered.
            StatusBarHitTarget::Fast => {
                self.clear_control_hovers();
                self.state.fast = false;
                self.flash(FAST_OFF_MSG.into());
                Vec::new()
            }
            // The countdown is cleared here rather than left to the next event:
            // the agent may spend a moment on the request, and a chip stuck at
            // "retrying in 0s" reads like the click missed.
            StatusBarHitTarget::Retry => {
                self.clear_control_hovers();
                self.active_chat().clear_retry();
                if let Some(cmd_tx) = &self.cmd_tx {
                    let _ = cmd_tx.try_send(AgentCommand::RetryNow);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_queue_click(&mut self, hit: QueueHit) -> Vec<Action> {
        self.queue_hover = None;
        match hit.target {
            QueueHitTarget::ToggleTogether => self.toggle_active_queue_delivery(),
            QueueHitTarget::Item { .. } if self.queue_editor_active() => {}
            QueueHitTarget::Item { id, action } => match action {
                QueueAction::Select => self.select_active_queue_item(id),
                QueueAction::Menu => self.open_queue_actions(id),
            },
        }
        Vec::new()
    }

    /// A left press outside the overlay that owns the screen dismisses it, and
    /// the press is swallowed so it never also acts on what is under it.
    /// Ordered like [`App::scroll_at`]: the topmost overlay answers first, and a
    /// press inside it is no dismissal at all.
    ///
    /// The docked forms are absent by design. The agent is parked on them until
    /// they are answered, and the transcript behind them stays live, so they
    /// own no outside to press.
    fn dismiss_at(&mut self, pos: Position) -> Option<Vec<Action>> {
        // Both take the mouse ahead of everything else, and the paste editor
        // holds edits that no stray press should throw away.
        if self.paste_editor.is_open() || self.permission_prompt.is_open() {
            return None;
        }

        macro_rules! dismiss {
            // Closing outright is the whole dismissal.
            ($overlay:expr) => {
                dismiss!($overlay, {
                    $overlay.close();
                    Vec::new()
                })
            };
            // Closing owes the app something as well: a restore, a warning, or
            // the next prompt in a chain.
            ($overlay:expr, $dismissal:expr) => {
                if $overlay.is_open() {
                    if $overlay.contains(pos) {
                        return None;
                    }
                    return Some($dismissal);
                }
            };
        }

        // A centred float owns the screen the way a modal does. Splits and
        // panels are docked chrome, so they neither answer a press nor stand in
        // the way of one reaching the overlay below.
        if self.float_mgr.contains(pos) {
            return None;
        }
        if self.float_mgr.dismiss_outside(pos) {
            return Some(Vec::new());
        }

        dismiss!(self.stream_modal, {
            self.stream_modal.dismiss();
            Vec::new()
        });
        dismiss!(self.help_modal);
        dismiss!(self.usage_modal);
        dismiss!(self.logs_modal);
        dismiss!(self.docs_modal);
        dismiss!(self.context_modal);
        dismiss!(self.tools_modal);
        dismiss!(self.decisions_modal);
        dismiss!(self.skills_modal);
        dismiss!(self.storage_modal);
        dismiss!(self.system_prompt_modal);
        dismiss!(self.projection_modal);
        dismiss!(self.goal_modal);

        dismiss!(self.command_modal);
        dismiss!(self.search_modal, {
            let action = self.search_modal.cancel();
            self.handle_search_action(action)
        });
        dismiss!(self.theme_picker, {
            let action = self.theme_picker.cancel();
            self.handle_theme_picker_action(action)
        });
        dismiss!(
            self.mcp_picker,
            self.handle_mcp_picker_action(crate::components::mcp_picker::McpPickerAction::Close)
        );
        dismiss!(
            self.login_picker,
            self.handle_login_picker_action(
                crate::components::login_picker::LoginPickerAction::Close
            )
        );
        dismiss!(self.rewind_picker);
        dismiss!(self.message_actions);
        dismiss!(self.queue_actions);
        dismiss!(
            self.review,
            self.handle_review_action(crate::components::review::ReviewAction::Close)
        );
        dismiss!(self.model_picker);
        dismiss!(self.prompt_profile_picker);
        dismiss!(self.thinking_picker);
        // Debounced onto the screen, so it can be open with nothing drawn for
        // it. A press cannot land outside an overlay that was never there.
        if self.file_picker.is_open() {
            if !self.file_picker.is_drawn() || self.file_picker.contains(pos) {
                return None;
            }
            self.file_picker.close();
            return Some(Vec::new());
        }
        dismiss!(self.permissions_picker);
        dismiss!(self.stash_picker);
        dismiss!(self.memory_picker);
        dismiss!(self.workflow_inspector);
        dismiss!(self.workflow_catalog_picker);
        dismiss!(self.task_picker, {
            let action = self.task_picker.cancel();
            self.handle_task_picker_action(action)
        });
        dismiss!(self.shell_modal);
        dismiss!(self.session_picker);

        None
    }

    fn route_overlay_mouse<T>(
        &mut self,
        event: MouseEvent,
        dispatch: impl FnOnce(&mut Self, MouseEvent) -> T,
        map: impl FnOnce(&mut Self, T) -> Vec<Action>,
    ) -> Option<Vec<Action>> {
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
        self.chord_hint_hover = None;
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
            let focused = self.composer_holds_keys();
            self.active_input_box_mut()
                .update_hover(area, row, col, focused);
        }
    }

    /// The transcript a click at `row` would open. A batch child is asked
    /// about first: its row sits inside the batch's card, so the card's own id
    /// answers for it and would send every roster row to the same place.
    ///
    /// Either id names the call the row would publish under, which is what a
    /// delegation still being written is filed under too. So a dispatched call
    /// resolves through `parent_task_ids` and a predicted one answers for
    /// itself; a row that delegates nothing names no chat either way.
    fn task_id_at(&self, row: u16, area: Rect) -> Option<String> {
        if let Some(id) = self.chats[0].task_hit_at(row, area) {
            return Some(id);
        }
        let id = self.chats[0]
            .dispatched_id_at(row, area)
            .or_else(|| self.chats[0].tool_id_at(row, area).map(str::to_owned))?;
        self.parent_task_ids
            .get(&id)
            .cloned()
            .or_else(|| self.pending_delegations.contains(&id).then_some(id.clone()))
            .or_else(|| {
                self.state
                    .session
                    .subagents()
                    .iter()
                    .find(|task| {
                        task.parent_tool_use_id
                            .as_deref()
                            .unwrap_or(&task.tool_use_id)
                            == id
                    })
                    .map(|task| task.tool_use_id.clone())
            })
    }

    fn update_transcript_hover(&mut self, row: u16, col: u16) {
        if !self
            .zone_at(row, col)
            .is_some_and(|zone| zone.zone == SelectionZone::Messages)
        {
            self.chats[self.active_chat].clear_hover();
            // The pointer left the transcript, so it is over no window it
            // armed. Said here rather than in `clear_hover`, which many paths
            // call for reasons that have nothing to do with the pointer.
            self.chats[self.active_chat].disarm_card();
            return;
        }
        let area = self.msg_area();
        let known_task_target = self.active_chat == 0
            && self.task_id_at(row, area).is_some_and(|task_id| {
                self.chats
                    .iter()
                    .any(|chat| chat.task_id().is_some_and(|id| **id == task_id))
            });
        if self.workspace_session.is_some() {
            self.chats[self.active_chat].update_hover_remote(row, col, area, known_task_target);
        } else {
            let cwd = PathBuf::from(&self.state.session.cwd);
            self.chats[self.active_chat].update_hover(row, col, area, known_task_target, &cwd);
        }
    }

    /// The commit under the pointer in the transcript. A reference is
    /// client-local by construction: the hash names a revision, not a path, so
    /// there is no remote spelling to choose between.
    fn transcript_commit_at(&self, row: u16, col: u16) -> Option<CommitRef> {
        self.chats[self.active_chat].commit_at(row, col, self.msg_area())
    }

    /// The mention under the pointer in the transcript.
    fn transcript_mention_at(&self, row: u16, col: u16) -> Option<Mention> {
        if self.workspace_session.is_some() {
            return self.chats[self.active_chat].mention_at_remote(row, col, self.msg_area());
        }
        let cwd = PathBuf::from(&self.state.session.cwd);
        self.chats[self.active_chat].mention_at(row, col, self.msg_area(), &cwd)
    }

    pub(super) fn scroll_offset(&self, zone: SelectionZone) -> u32 {
        match zone {
            SelectionZone::Messages => self.chats[self.active_chat].scroll_top(),
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
