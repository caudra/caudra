use std::sync::atomic::Ordering;

use crate::components::Overlay;
use crate::components::input::{self, Placeholder};
use crate::components::keybindings;
#[cfg(test)]
use crate::components::keybindings::KeybindContext;
use crate::components::queue_panel;
use crate::components::split_layout::{MIN_CHAT_ROWS, SplitLayout, carve};
use crate::components::status_bar::{StatusBarContext, UsageStats};
use crate::components::usage_modal::UsageModalContext;
use crate::selection::{self, SelectableZone, SelectionZone, ZoneRegistry};
use crate::theme;
use caudra_lua::Split;
use caudra_providers::RequestOptions;
#[cfg(test)]
use caudra_workbench::{Focus, SidebarView};
use ratatui::Frame;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};

use super::{App, Mode, Status};

const MAIN_GUTTER_WIDE: u16 = 2;
const MAIN_GUTTER_NARROW: u16 = 1;
const MESSAGE_VERTICAL_PADDING: u16 = 1;

struct ViewLayout {
    msg_area: Rect,
    bottom_area: Rect,
    status_area: Rect,
    queue_area: Rect,
    todo_area: Rect,
    panel_windows: Vec<(usize, Rect)>,
    input_area: Rect,
    splits: SplitLayout,
    bottom_takeover: bool,
}

impl App {
    pub fn view(&mut self, frame: &mut Frame) {
        self.sync_subagent_input_target();
        self.queue_hits.clear();
        self.admission_hits.clear();
        self.task_hint_hit = Rect::ZERO;
        if self.workbench.is_open() {
            self.render_workbench(frame);
            return;
        }
        let layout = self.compute_layout(frame.area());
        let render_chat = self.active_chat;

        self.render_background(frame);
        self.render_messages(frame, &layout, render_chat);
        self.render_bottom_panel(frame, &layout);
        self.render_splits(frame, &layout);
        let mut overlay_rect = self.render_picker_overlays(frame, layout.msg_area);
        self.render_status_bar(frame, layout.status_area, render_chat);
        overlay_rect = self.render_top_modals(frame, overlay_rect);
        self.register_zones(&layout, overlay_rect);
        self.apply_selection(frame, render_chat);
    }

    /// The workbench replaces the transcript outright. Caudra's own status bar
    /// stays, so the model and the token budget never leave the screen, and
    /// there are no message zones behind it to register or select.
    ///
    /// It is a view, not a modal, so every overlay still draws on top of it.
    /// Skipping them left a permission prompt invisible while it went on
    /// owning the keyboard, which hangs the session outright.
    fn render_workbench(&mut self, frame: &mut Frame) {
        let render_chat = self.active_chat;
        let [body, status] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)])
            .areas(main_content_area(frame.area()));
        self.zones = ZoneRegistry::new();
        self.zones.push_overlay(frame.area());
        self.render_background(frame);
        self.workbench.view(frame, body);
        self.render_status_bar(frame, status, render_chat);

        let prompt = self.render_workbench_prompt(frame, body);
        let mut overlay_rect = self.render_picker_overlays(frame, body);
        overlay_rect = self.render_top_modals(frame, overlay_rect);
        for rect in [prompt, overlay_rect] {
            if rect.width > 0 {
                self.zones.push_overlay(rect);
            }
        }
    }

    /// The docked prompts take the foot of the workbench the way they take the
    /// foot of the transcript, so an answer is asked for in the same place
    /// wherever the user happens to be.
    fn render_workbench_prompt(&mut self, frame: &mut Frame, body: Rect) -> Rect {
        let height = if self.permission_prompt.is_open() {
            self.permission_prompt.height(body.width)
        } else if self.question_form.is_open() {
            self.question_form.height(body.width, body.height)
        } else {
            return Rect::default();
        };

        let height = height.min(body.height);
        let area = Rect {
            y: body.y + body.height - height,
            height,
            ..body
        };
        frame.render_widget(Clear, area);
        if self.permission_prompt.is_open() {
            self.permission_prompt.view(frame, area);
        } else {
            self.question_form.view(frame, area);
        }
        area
    }

    pub(crate) fn apply_terminal_links(&self, buffer: &mut Buffer) {
        let Some(chat) = self.chats.get(self.active_chat) else {
            return;
        };
        for link in chat.terminal_links() {
            if !(0..link.width).all(|offset| {
                self.zones
                    .zone_at(link.position.y, link.position.x.saturating_add(offset))
                    .is_some_and(|zone| zone.zone == SelectionZone::Messages)
            }) {
                continue;
            }
            let encoded = buffer.cell_mut(link.position).is_some_and(|cell| {
                cell.modifier.contains(Modifier::UNDERLINED)
                    && crate::terminal::encode_hyperlink_cell(
                        cell,
                        &link.symbol,
                        link.width,
                        &link.target,
                    )
            });
            if encoded
                && link.width == 1
                && let Some(cell) = buffer.cell_mut(Position::new(
                    link.position.x.saturating_add(1),
                    link.position.y,
                ))
                && cell.diff_option == CellDiffOption::None
            {
                cell.set_diff_option(CellDiffOption::AlwaysUpdate);
            }
        }
    }

    fn compute_layout(&self, area: Rect) -> ViewLayout {
        let permission_open = self.permission_prompt.is_open();
        let question_open = self.question_form.is_open();
        // Both park the agent on the user, so both own the bottom outright.
        let blocking_form = permission_open || question_open;
        let form_visible = blocking_form || self.plan_form_active();

        // Carve the full-width status bar first so the split carving below only
        // ever deals with the content region above it.
        let [content, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

        // A blocking form owns the bottom area, so drop any `below` split here
        // at the source. That keeps "the form wins bottom" in one filter
        // instead of needing a fix-up further down.
        let reqs: Vec<_> = self
            .float_mgr
            .split_reqs(content)
            .into_iter()
            .filter(|r| !(blocking_form && r.split == Split::Below))
            .collect();
        let splits = carve(content, &reqs);
        let inner = main_content_area(splits.inner);

        let below_active = splits.rect(Split::Below).is_some();
        let bottom_takeover = form_visible || below_active;
        let max_bottom = inner.height.saturating_sub(MIN_CHAT_ROWS);
        let bottom_height = if permission_open {
            self.permission_prompt.height(inner.width).min(max_bottom)
        } else if question_open {
            self.question_form
                .height(inner.width, inner.height)
                .min(max_bottom)
        } else if below_active {
            0
        } else if form_visible {
            self.plan_form.height().min(max_bottom)
        } else if self.is_main_chat() {
            let panel_h: u16 = self.float_mgr.panel_reqs().iter().map(|(_, h)| *h).sum();
            queue_panel::height(&self.queue.panel_entries())
                + self.todo_panel.height()
                + panel_h
                + self.input_box.height(inner.width).min(max_bottom)
        } else {
            let panel_h: u16 = self.float_mgr.panel_reqs().iter().map(|(_, h)| *h).sum();
            queue_panel::height(&self.active_queue_entries())
                + self.todo_panel.height()
                + panel_h
                + if self.active_subagent_can_steer() || self.queue_editor_active() {
                    self.subagent_input_box.height(inner.width).min(max_bottom)
                } else {
                    1
                }
        };

        // The `below` split lives outside `inner` (drawn by render_splits), so
        // the bottom panel only ever splits the chat region.
        let [msg_region, bottom_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(bottom_height)]).areas(inner);
        let msg_area = message_content_area(msg_region);

        let panel_reqs = if bottom_takeover {
            Vec::new()
        } else {
            self.float_mgr.panel_reqs()
        };

        let queue_height = if bottom_takeover {
            0
        } else if !self.is_main_chat() {
            queue_panel::height(&self.active_queue_entries())
        } else {
            queue_panel::height(&self.queue.panel_entries())
        };

        let todo_height = if bottom_takeover {
            0
        } else {
            self.todo_panel.height()
        };

        let mut constraints = vec![
            Constraint::Length(queue_height),
            Constraint::Length(todo_height),
        ];
        for &(_, h) in &panel_reqs {
            constraints.push(Constraint::Length(h));
        }
        constraints.push(Constraint::Min(1));

        let areas = Layout::vertical(constraints).split(bottom_area);
        let queue_area = areas[0];
        let todo_area = areas[1];
        let panel_windows: Vec<(usize, Rect)> = panel_reqs
            .iter()
            .enumerate()
            .map(|(i, &(idx, _))| (idx, areas[2 + i]))
            .collect();
        let input_area = areas[areas.len() - 1];

        ViewLayout {
            msg_area,
            bottom_area,
            status_area: main_content_area(status_area),
            queue_area,
            todo_area,
            panel_windows,
            input_area,
            splits,
            bottom_takeover,
        }
    }

    fn render_background(&self, frame: &mut Frame) {
        let bg = Block::default().style(theme::current().surface_style());
        bg.render(frame.area(), frame.buffer_mut());
    }

    fn render_messages(&mut self, frame: &mut Frame, layout: &ViewLayout, render_chat: usize) {
        let accent = self.effective_mode_color();
        // Pushed per frame rather than at construction so subagent chats,
        // which are created mid-session, inherit the current mode.
        self.chats[render_chat].set_view(self.view);
        self.chats[render_chat].set_accent(accent);
        self.chats[render_chat].view(
            frame,
            layout.msg_area,
            self.selection_state.is_some(),
            render_chat == 0,
        );
    }

    fn render_bottom_panel(&mut self, frame: &mut Frame, layout: &ViewLayout) {
        if self.permission_prompt.is_open() {
            self.permission_prompt.view(frame, layout.bottom_area);
        } else if self.question_form.is_open() {
            self.question_form.view(frame, layout.bottom_area);
        } else if !self.is_main_chat() {
            let queue_entries = self.active_queue_entries();
            let queue_title = self.active_queue_title();
            let together = self
                .active_queue_delivery()
                .map(|delivery| delivery == caudra_agent::QueueDelivery::TogetherNextTurn);
            self.queue_hits = queue_panel::view(
                frame,
                layout.queue_area,
                &queue_title,
                &queue_entries,
                queue_panel::QueuePanelState {
                    focus: self.active_queue_focus(),
                    viewport: self.active_queue_viewport(),
                    together,
                    hovered: self.queue_hover,
                },
            );
            self.todo_panel.view(frame, layout.todo_area);
            for &(idx, rect) in &layout.panel_windows {
                self.float_mgr.view_panel(frame, idx, rect);
            }
            if self.active_subagent_can_steer() || self.queue_editor_active() {
                self.subagent_input_box.view(
                    frame,
                    layout.input_area,
                    if self.queue_editor_active() {
                        Placeholder::QueueEdit
                    } else {
                        Placeholder::Steer
                    },
                    self.separator_style(),
                    !self.any_overlay_open(),
                    None,
                );
                self.command_palette.view(frame, layout.input_area);
                self.mention_popup.view(frame, layout.input_area);
            } else {
                let sep = Block::default()
                    .borders(Borders::TOP)
                    .border_style(self.separator_style());
                frame.render_widget(sep, layout.input_area);
            }
        } else if self.plan_form_active() {
            self.plan_form.view(frame, layout.bottom_area);
        } else if layout.bottom_area.height > 0 {
            let queue_entries = self.queue.panel_entries();
            let queue_title = self.active_queue_title();
            let together = self
                .active_queue_delivery()
                .map(|delivery| delivery == caudra_agent::QueueDelivery::TogetherNextTurn);
            self.queue_hits = queue_panel::view(
                frame,
                layout.queue_area,
                &queue_title,
                &queue_entries,
                queue_panel::QueuePanelState {
                    focus: self.queue.focus(),
                    viewport: self.queue.viewport(),
                    together,
                    hovered: self.queue_hover,
                },
            );
            self.todo_panel.view(frame, layout.todo_area);
            for &(idx, rect) in &layout.panel_windows {
                self.float_mgr.view_panel(frame, idx, rect);
            }
            let placeholder = if self.queue_editor_active() {
                Placeholder::QueueEdit
            } else if self.status == Status::Streaming {
                Placeholder::Queue
            } else if crate::session_history_head(&self.state.session).is_none() {
                Placeholder::Suggestion
            } else {
                Placeholder::Blank
            };
            let panel_hint = if self.status == Status::Streaming && !self.queue_editor_active() {
                let (hint, hits) = input::admission_hint(layout.input_area, self.admission_hover);
                self.admission_hits = hits;
                Some(hint)
            } else {
                let panel = (self.state.mode == Mode::Plan)
                    .then(|| self.plan_form.hint_line())
                    .flatten()
                    .or_else(|| self.todo_panel.hint_line());
                match panel {
                    Some(hint) => Some(hint),
                    None => match self.task_hint(layout.input_area) {
                        Some((hint, hit)) => {
                            self.task_hint_hit = hit;
                            Some(hint)
                        }
                        None => self.lua_hint_line(),
                    },
                }
            };
            self.input_box.view(
                frame,
                layout.input_area,
                placeholder,
                self.separator_style(),
                !self.any_overlay_open(),
                panel_hint,
            );
            self.command_palette.view(frame, layout.input_area);
            self.mention_popup.view(frame, layout.input_area);
        }
    }

    fn render_splits(&mut self, frame: &mut Frame, layout: &ViewLayout) {
        for dir in Split::ALL {
            if let Some(rect) = layout.splits.rect(dir) {
                self.float_mgr.view_split(frame, dir, rect);
            }
        }
    }

    fn render_picker_overlays(&mut self, frame: &mut Frame, msg_area: Rect) -> Rect {
        let mut overlay_rect = Rect::default();
        let full = frame.area();

        if self.search_modal.is_open() {
            overlay_rect = self.search_modal.view(frame, msg_area);
        }

        if self.file_picker.is_open() {
            overlay_rect = self.file_picker.view(frame, full);
        }

        macro_rules! render_if_open {
            ($overlay:expr) => {
                if $overlay.is_open() {
                    overlay_rect = $overlay.view(frame, full);
                }
            };
        }

        render_if_open!(self.command_modal);
        render_if_open!(self.rewind_picker);
        render_if_open!(self.message_actions);
        render_if_open!(self.queue_actions);
        render_if_open!(self.review);
        render_if_open!(self.theme_picker);
        render_if_open!(self.prompt_profile_picker);
        render_if_open!(self.model_picker);
        render_if_open!(self.login_picker);
        render_if_open!(self.mcp_picker);
        render_if_open!(self.permissions_picker);
        render_if_open!(self.stash_picker);
        render_if_open!(self.memory_picker);
        render_if_open!(self.task_picker);
        render_if_open!(self.session_picker);

        overlay_rect
    }

    fn render_top_modals(&mut self, frame: &mut Frame, mut overlay_rect: Rect) -> Rect {
        let full = frame.area();
        let r = self.btw_modal.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        let r = self.help_modal.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        if self.usage_modal.is_open() {
            let ctx = UsageModalContext {
                total: &self.state.token_usage,
                total_cost: self.state.cost,
                subscription_cost: self.state.subscription_cost,
                by_model: self.state.session.usage_by_model(),
                model: &self.state.model,
                fast: self.state.fast,
                clock_format: self.ui_config.clock_format,
                lifetime: self.lifetime_usage.as_ref(),
            };
            let r = self.usage_modal.view(frame, full, &ctx);
            if r.width > 0 {
                overlay_rect = r;
            }
        }
        if self.logs_modal.is_open() {
            let theme = crate::theme::current();
            let r = self.logs_modal.view(frame, full, &theme);
            if r.width > 0 {
                overlay_rect = r;
            }
        }
        if self.context_modal.is_open() {
            let snapshot = self.context_snapshot.get();
            let r = self.context_modal.view(frame, full, snapshot);
            if r.width > 0 {
                overlay_rect = r;
            }
        }
        if self.tools_modal.is_open() {
            let snapshot = self.context_snapshot.get();
            let r = self.tools_modal.view(frame, full, snapshot);
            if r.width > 0 {
                overlay_rect = r;
            }
        }
        if self.skills_modal.is_open() {
            let snapshot = self.context_snapshot.get();
            let r = self.skills_modal.view(frame, full, snapshot);
            if r.width > 0 {
                overlay_rect = r;
            }
        }
        if self.storage_modal.is_open() {
            let r = self.storage_modal.view(frame, full);
            if r.width > 0 {
                overlay_rect = r;
            }
        }
        if self.goal_modal.is_open() {
            let status = self.state.goal.status();
            let evaluator =
                caudra_providers::model_registry::binding(caudra_providers::ModelPurpose::Goal);
            let r = self.goal_modal.view(
                frame,
                full,
                status.as_ref(),
                evaluator.as_ref(),
                self.state.goal.continuation_limit(),
            );
            if r.width > 0 {
                overlay_rect = r;
            }
        }
        let r = self.float_mgr.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        let r = self.paste_editor.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        // Last, so a pending chord's list sits over whatever it was armed on.
        let chords = keybindings::leader_chords(&self.leader_contexts());
        let r = self.which_key.view(frame, full, &chords);
        if r.width > 0 {
            overlay_rect = r;
        }
        overlay_rect
    }

    fn render_status_bar(&mut self, frame: &mut Frame, status_area: Rect, render_chat: usize) {
        let chat = &self.chats[render_chat];
        let goal = self.state.goal.snapshot();
        let chat_name = (self.chats.len() > 1).then_some(chat.name.as_str());
        let mode = self.mode_label();
        let main_chat = render_chat == 0;
        // What the request will actually carry, not what was asked for: a
        // model can refuse to stop reasoning, and the badge has to say so.
        let thinking = (main_chat && self.state.model.supports_thinking()).then(|| {
            RequestOptions {
                thinking: self.state.thinking.clone(),
                fast: self.state.fast,
            }
            .clamped(&self.state.model)
            .thinking
            .resolve(&self.state.model)
            .to_string()
            .into()
        });
        let ctx = StatusBarContext {
            status: &self.status,
            mode,
            model_id: chat
                .model_id
                .as_deref()
                .unwrap_or(&self.state.session.model),
            stats: UsageStats {
                global_cost: self.state.cost,
                global_subscription_cost: self.state.subscription_cost,
                context_size: chat.context_size,
                cost: chat.cost,
                subscription_cost: chat.subscription_cost,
                context_window: if chat.context_window > 0 {
                    chat.context_window
                } else {
                    self.state.model.context_window
                },
                show_global: self.chats.len() > 1,
            },
            auto_scroll: chat.auto_scroll(),
            chat_name,
            back_to_main: render_chat != 0,
            retry_info: self.retry_info.as_ref(),
            thinking,
            fast: self.state.fast,
            workflow: self.state.workflow,
            yolo: self.permissions.is_yolo(),
            restoring: self.restoring.load(Ordering::Relaxed),
            snapshotting: self.is_snapshotting(),
            goal: goal.as_ref(),
            mode_clickable: main_chat && !self.is_bash_input(),
            settings_clickable: main_chat,
            hovered: (!self.has_modal_overlay())
                .then_some(self.status_hover)
                .flatten(),
            hover_hint: (!self.has_modal_overlay())
                .then(|| chat.hovered_hint())
                .flatten(),
        };
        self.status_hits = self.status_bar.view(frame, status_area, &ctx);
    }

    fn register_zones(&mut self, layout: &ViewLayout, overlay_rect: Rect) {
        // Push order = z-order. zone_at() walks in reverse, so later entries win.
        self.zones = ZoneRegistry::new();

        self.zones.push(SelectableZone {
            area: layout.msg_area,
            zone: SelectionZone::Messages,
        });

        if layout.input_area.height > 0
            && !layout.bottom_takeover
            && (self.is_main_chat()
                || self.active_subagent_can_steer()
                || self.queue_editor_active())
        {
            let input_inner = input::content_area(layout.input_area);
            self.zones.push(SelectableZone {
                area: input_inner,
                zone: SelectionZone::Input,
            });
        }

        self.zones.push_overlay(layout.status_area);

        if self.permission_prompt.is_open()
            || self.question_form.is_open()
            || self.plan_form_active()
        {
            self.zones.push_overlay(layout.bottom_area);
        }

        for &(_, rect) in &layout.panel_windows {
            self.zones.push_overlay(selection::inset_border(rect));
        }

        if !self.is_main_chat()
            && !self.active_subagent_can_steer()
            && layout.bottom_area.height > 0
        {
            self.zones.push_overlay(layout.bottom_area);
        }

        if layout.queue_area.height > 0 && !layout.bottom_takeover {
            self.zones.push_overlay(layout.queue_area);
        }

        if layout.todo_area.height > 0 && !layout.bottom_takeover {
            self.zones.push_overlay(layout.todo_area);
        }

        for dir in Split::ALL {
            if let Some(rect) = layout.splits.rect(dir) {
                self.zones.push_overlay(selection::inset_border(rect));
            }
        }

        if overlay_rect.width > 0 {
            self.zones
                .push_overlay(selection::inset_border(overlay_rect));
        }

        // Overlay zone was removed (e.g. dialog closed), drop the dangling selection
        if let Some(ref state) = self.selection_state
            && state.sel().zone == SelectionZone::Overlay
            && self.zones.find_area(state.sel().area).is_none()
        {
            self.selection_state = None;
        }
    }

    fn apply_selection(&mut self, frame: &mut Frame, render_chat: usize) {
        let Some(ref state) = self.selection_state else {
            return;
        };

        let sel = state.sel();
        let scroll = self.scroll_offset(sel.zone);
        if let Some(screen_sel) = sel.to_screen(scroll) {
            selection::apply_highlight(frame.buffer_mut(), sel.highlight_area(), &screen_sel);
        }
        if state.is_pending_copy() {
            let sel = *sel;
            self.copy_selection(frame.buffer_mut(), &sel, render_chat);
        }
    }

    /// Layout geometry for tests: `(msg_area, bottom_area, status_area,
    /// input_area, splits)`.
    #[cfg(test)]
    pub(super) fn layout_geometry(&self, area: Rect) -> (Rect, Rect, Rect, Rect, SplitLayout) {
        let layout = self.compute_layout(area);
        (
            layout.msg_area,
            layout.bottom_area,
            layout.status_area,
            layout.input_area,
            layout.splits,
        )
    }

    fn lua_hint_line(&self) -> Option<Line<'static>> {
        let snap = self.hints.get()?;
        if snap.entries.is_empty() {
            return None;
        }
        let mut spans = Vec::new();
        for (_, pairs) in &snap.entries {
            for (text, style_name) in pairs {
                let style = theme::style_by_name(style_name);
                spans.push(Span::styled(text.clone(), style));
            }
        }
        Some(Line::from(spans))
    }

    #[cfg(test)]
    pub(super) fn active_keybind_contexts(&self) -> Vec<KeybindContext> {
        let mut contexts = vec![KeybindContext::General];
        if self.workbench.is_open() {
            contexts.push(KeybindContext::Workbench);
            contexts.push(match self.workbench.sidebar_view() {
                SidebarView::Explorer => KeybindContext::WorkbenchExplorer,
                SidebarView::SourceControl => KeybindContext::WorkbenchSourceControl,
                SidebarView::Search => KeybindContext::WorkbenchSearch,
            });
            if self.workbench.focus() == Focus::Editor {
                contexts.push(KeybindContext::WorkbenchEditor);
            }
        } else if self.paste_editor.is_open() {
            contexts.push(KeybindContext::PasteEditor);
        } else if self.review.is_open() {
            contexts.push(KeybindContext::Review);
        } else if self.plan_form_active() {
            contexts.push(KeybindContext::FormInput);
        } else if self.queue_editor_active() {
            contexts.push(KeybindContext::Editing);
        } else if self.active_queue_is_focused() {
            contexts.push(KeybindContext::QueueFocus);
        } else if self.rewind_picker.is_open() {
            contexts.push(KeybindContext::RewindPicker);
        } else if self.theme_picker.is_open() || self.prompt_profile_picker.is_open() {
            contexts.push(KeybindContext::ThemePicker);
        } else if self.model_picker.is_open() {
            contexts.push(KeybindContext::ModelPicker);
        } else if self.command_palette.is_active() {
            contexts.push(KeybindContext::CommandPalette);
        } else if self.search_modal.is_open() {
            contexts.push(KeybindContext::Search);
        } else if self.file_picker.is_open() {
            contexts.push(KeybindContext::FilePicker);
        } else {
            if self.status == Status::Streaming {
                contexts.push(KeybindContext::Streaming);
            }
            contexts.push(KeybindContext::Editing);
        }
        contexts
    }
}

pub(super) fn main_content_area(area: Rect) -> Rect {
    let gutter = if area.width >= 60 {
        MAIN_GUTTER_WIDE
    } else if area.width >= 36 {
        MAIN_GUTTER_NARROW
    } else {
        0
    };
    Rect::new(
        area.x.saturating_add(gutter),
        area.y,
        area.width.saturating_sub(gutter.saturating_mul(2)),
        area.height,
    )
}

fn message_content_area(area: Rect) -> Rect {
    let padding = if area.height
        >= MIN_CHAT_ROWS.saturating_add(MESSAGE_VERTICAL_PADDING.saturating_mul(2))
    {
        MESSAGE_VERTICAL_PADDING
    } else {
        0
    };
    Rect::new(
        area.x,
        area.y.saturating_add(padding),
        area.width,
        area.height.saturating_sub(padding.saturating_mul(2)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(35, 5, 35 ; "compact_has_no_gutter")]
    #[test_case(36, 6, 34 ; "narrow_threshold")]
    #[test_case(59, 6, 57 ; "narrow_upper_bound")]
    #[test_case(60, 7, 56 ; "wide_threshold")]
    fn main_content_gutter_is_responsive(width: u16, expected_x: u16, expected_width: u16) {
        let area = Rect::new(5, 3, width, 10);

        assert_eq!(
            main_content_area(area),
            Rect::new(expected_x, area.y, expected_width, area.height)
        );
    }

    #[test_case(2, 0, 2 ; "minimum_height_keeps_content")]
    #[test_case(3, 0, 3 ; "short_height_keeps_content")]
    #[test_case(4, 1, 2 ; "padding_starts_with_two_content_rows")]
    #[test_case(10, 1, 8 ; "regular_height_has_vertical_padding")]
    fn message_area_preserves_vertical_breathing_room(
        height: u16,
        expected_y_offset: u16,
        expected_height: u16,
    ) {
        let area = Rect::new(5, 3, 40, height);

        assert_eq!(
            message_content_area(area),
            Rect::new(
                area.x,
                area.y + expected_y_offset,
                area.width,
                expected_height,
            )
        );
    }
}
