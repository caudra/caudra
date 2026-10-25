use std::borrow::Cow;
use std::sync::atomic::Ordering;

use crate::components::docs_link::{self, DocsTopic};
use crate::components::goal_panel::StatusContent;
use crate::components::input::{self, Placeholder};
use crate::components::keybindings;
#[cfg(test)]
use crate::components::keybindings::KeybindContext;
use crate::components::queue_panel;
use crate::components::split_layout::{MIN_CHAT_ROWS, SplitLayout, carve};
use crate::components::status_bar::{
    self, DecisionsChip, StatusBarContext, UsageStats, workflow_chip,
};
use crate::components::tools_modal::ToolsModalContext;
use crate::components::usage_modal::UsageModalContext;
use crate::components::{Overlay, hover_style};
use crate::selection::{self, SelectableZone, SelectionZone, ZoneRegistry};
use crate::theme;
use crate::update;
use caudra_grab::grab_scope;
use caudra_lua::Split;
use caudra_providers::RequestOptions;
#[cfg(test)]
use caudra_workbench::{Focus, SidebarView};
use ratatui::Frame;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

use super::accounts::account_chip;
use super::permission_editor::LOCAL_SOURCE_NOTICE;
use super::sandbox::attached_sandbox_instance;
use super::tooltip::ChordTopic;
use super::{App, Mode};

const MAIN_GUTTER_WIDE: u16 = 2;
const MAIN_GUTTER_NARROW: u16 = 1;
const MESSAGE_VERTICAL_PADDING: u16 = 1;
/// Marks where a middle press anchored autoscroll. Without it the view moves
/// with nothing on screen to say why, or how to aim it.
pub(crate) const AUTOSCROLL_ORIGIN: &str = "\u{2295}";

struct ViewLayout {
    msg_area: Rect,
    banner_area: Rect,
    bottom_area: Rect,
    status_area: Rect,
    queue_area: Rect,
    todo_area: Rect,
    panel_windows: Vec<(usize, Rect)>,
    input_area: Rect,
    splits: SplitLayout,
    bottom_takeover: bool,
}

/// The overlay drawn last, so on top: where it is, and the docs it links to.
#[derive(Default)]
struct Topmost {
    rect: Rect,
    topic: Option<DocsTopic>,
}

impl App {
    /// Recording brackets the whole draw, so anything painted after
    /// `caudra_grab::end_frame` is invisible to the grab hit test. The grab
    /// interface paints there, which is how it stays out of its own results.
    pub fn view(&mut self, frame: &mut Frame) {
        caudra_grab::begin_frame();
        self.view_body(frame);
        caudra_grab::end_frame();
        #[cfg(debug_assertions)]
        self.render_grab(frame);
    }

    fn view_body(&mut self, frame: &mut Frame) {
        grab_scope!("app", frame.area());
        self.sync_subagent_input_target();
        self.queue_hits.clear();
        self.queue_cut_rows.clear();
        self.admission_hits.clear();
        self.chord_hint_hit = None;
        self.update_close = None;
        self.update_text = None;
        if self.workbench.is_open() {
            self.render_workbench(frame);
            return;
        }
        let layout = self.compute_layout(frame.area());
        let render_chat = self.active_chat;

        self.render_background(frame);
        self.render_messages(frame, &layout, render_chat);
        self.render_update_banner(frame, layout.banner_area);
        self.render_bottom_panel(frame, &layout);
        self.render_splits(frame, &layout);
        let above_footer = Rect {
            height: layout.status_area.y.saturating_sub(frame.area().y),
            ..frame.area()
        };
        let below = self.render_picker_overlays(frame, layout.msg_area, above_footer);
        self.render_status_bar(frame, layout.status_area, render_chat);
        let topmost = self.render_top_modals(frame, below);
        self.register_zones(&layout, topmost.rect);
        self.render_docs_badge(frame, &topmost);
        self.apply_selection(frame, render_chat);
        self.render_autoscroll_origin(frame);
        self.render_tooltip(frame);
    }

    /// The `?` on the topmost modal's border. Where it landed is kept for the
    /// click, the tooltip and `F1`, so all three answer for the modal on top.
    fn render_docs_badge(&mut self, frame: &mut Frame, topmost: &Topmost) {
        self.docs_badge = topmost.topic.zip(docs_link::badge_area(topmost.rect));
        let Some((_, area)) = self.docs_badge else {
            return;
        };
        let hovered = self.tooltip.pointer().is_some_and(|at| area.contains(at));
        let style = hover_style(theme::current().panel_title, hovered);
        frame.render_widget(Span::styled(docs_link::BADGE, style), area);
    }

    fn render_autoscroll_origin(&self, frame: &mut Frame) {
        let Some(at) = self.autoscroll.as_ref().map(|auto| auto.origin()) else {
            return;
        };
        let buf = frame.buffer_mut();
        if let Some(cell) = buf.cell_mut(at) {
            cell.set_symbol(AUTOSCROLL_ORIGIN);
            cell.set_style(theme::current().accent);
        }
    }

    /// The workbench replaces the transcript outright. Caudra's own status bar
    /// stays, so the model and the token budget never leave the screen, and
    /// there are no message zones behind it to register or select.
    ///
    /// It is a view, not a modal, so every overlay still draws on top of it.
    /// Skipping them left a permission prompt invisible while it went on
    /// owning the keyboard, which hangs the session outright.
    fn render_workbench(&mut self, frame: &mut Frame) {
        grab_scope!("workbench", frame.area());
        let render_chat = self.active_chat;
        let [mut body, status] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(status_bar::height(frame.area().height)),
        ])
        .areas(main_content_area(frame.area()));
        self.zones = ZoneRegistry::new();
        self.zones.push_overlay(frame.area());
        self.render_background(frame);
        if self.parked_workbench.is_some() && body.height > 1 {
            let [notice, remaining] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(body);
            frame.render_widget(
                Line::from(LOCAL_SOURCE_NOTICE).style(theme::current().accent),
                notice,
            );
            body = remaining;
        }
        self.workbench.view(frame, body);
        self.render_status_bar(frame, status, render_chat);

        let prompt = self.render_workbench_prompt(frame, body);
        let below = self.render_picker_overlays(frame, body, frame.area());
        let topmost = self.render_top_modals(frame, below);
        for rect in [prompt, topmost.rect] {
            if rect.width > 0 {
                self.zones.push_overlay(rect);
            }
        }
        self.render_docs_badge(frame, &topmost);
        self.render_tooltip(frame);
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
        frame.render_widget(
            Block::default().style(theme::current().surface_style()),
            area,
        );
        if self.permission_prompt.is_open() {
            self.permission_prompt.view(frame, area);
        } else {
            self.question_form
                .view_with_focus(frame, area, !self.stream_modal.is_open());
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
        let form_visible = blocking_form || self.plan_form_in_view();

        // Carve the full-width status bar first so the split carving below only
        // ever deals with the content region above it.
        let [content, status_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(status_bar::height(area.height)),
        ])
        .areas(area);

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
            self.plan_form
                .height(inner.width, inner.height)
                .min(max_bottom)
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
                + if self.active_subagent_has_composer() || self.queue_editor_active() {
                    self.subagent_input_box.height(inner.width).min(max_bottom)
                } else {
                    1
                }
        };

        // A short terminal keeps the transcript's minimum before the banner.
        let banner_height = u16::from(
            self.update_banner.get().is_some()
                && inner.height.saturating_sub(bottom_height) > MIN_CHAT_ROWS,
        );
        // The `below` split lives outside `inner` (drawn by render_splits), so
        // the bottom panel only ever splits the chat region.
        let [msg_region, banner_area, bottom_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(banner_height),
            Constraint::Length(bottom_height),
        ])
        .areas(inner);
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
            banner_area,
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
        grab_scope!("messages", layout.msg_area);
        let accent = self.effective_mode_color();
        // Pushed per frame rather than at construction so subagent chats,
        // which are created mid-session, inherit the current mode.
        self.chats[render_chat].set_view(self.view);
        self.chats[render_chat].set_accent(accent);
        self.chats[render_chat].view(frame, layout.msg_area, self.selection_state.is_some(), true);
    }

    /// Drawn before the bottom panel, so the composer's popups, which open
    /// upwards over this row, stay on top of it.
    fn render_update_banner(&mut self, frame: &mut Frame, area: Rect) {
        let Some(available) = self.update_banner.get() else {
            return;
        };
        if area.height == 0 {
            return;
        }
        grab_scope!("update_banner", area);
        let close_width = Span::raw(update::CLOSE).width() as u16;
        let [text, close] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(close_width)]).areas(area);
        frame.render_widget(update::banner_line(available, text.width), text);
        frame.render_widget(
            Span::styled(update::CLOSE, theme::current().status_dim),
            close,
        );
        self.update_close = Some(close);
        self.update_text = Some(text);
    }

    fn render_bottom_panel(&mut self, frame: &mut Frame, layout: &ViewLayout) {
        grab_scope!("bottom_panel", layout.bottom_area);
        if self.permission_prompt.is_open() {
            self.permission_prompt.view(frame, layout.bottom_area);
        } else if self.question_form.is_open() {
            self.question_form.view_with_focus(
                frame,
                layout.bottom_area,
                !self.stream_modal.is_open(),
            );
        } else if !self.is_main_chat() {
            let queue_entries = self.active_queue_entries();
            let queue_title = self.active_queue_title();
            let together = self
                .active_queue_delivery()
                .map(|delivery| delivery == caudra_agent::QueueDelivery::TogetherNextTurn);
            let drawn = queue_panel::view(
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
            self.queue_hits = drawn.hits;
            self.queue_cut_rows = drawn.cut_rows;
            self.todo_panel.view(frame, layout.todo_area);
            for &(idx, rect) in &layout.panel_windows {
                self.float_mgr.view_panel(frame, idx, rect);
            }
            let panel_hint = if self.active_task_is_steerable() && !self.queue_editor_active() {
                let (hint, hits) = input::admission_hint(
                    layout.input_area,
                    self.admission_hover,
                    &input::TASK_ADMISSIONS,
                );
                self.admission_hits = hits;
                Some(hint)
            } else {
                self.chord_hint(layout.input_area)
            };
            if self.active_subagent_has_composer() || self.queue_editor_active() {
                let ghost = self.composer_palette_ghost();
                self.subagent_input_box.view(
                    frame,
                    layout.input_area,
                    if self.queue_editor_active() {
                        Placeholder::QueueEdit
                    } else if self
                        .active_subagent_id()
                        .is_some_and(|id| self.subagent_steers.contains_key(id))
                    {
                        Placeholder::Steer
                    } else {
                        Placeholder::ResumeTask
                    },
                    self.separator_style(),
                    self.composer_holds_keys(),
                    panel_hint,
                    ghost.as_deref(),
                );
                self.view_composer_palette(frame, layout.input_area);
                self.mention_popup.view(frame, layout.input_area);
                self.commit_popup.view(frame, layout.input_area);
            } else {
                let sep = Block::default()
                    .borders(Borders::TOP)
                    .border_style(self.separator_style());
                frame.render_widget(sep, layout.input_area);
                if let Some(hint) = panel_hint {
                    frame.render_widget(
                        Paragraph::new(hint.right_aligned()),
                        input::top_right_hint_area(layout.input_area),
                    );
                }
            }
        } else if self.plan_form_in_view() {
            self.plan_form.view(frame, layout.bottom_area);
        } else if layout.bottom_area.height > 0 {
            let queue_entries = self.queue.panel_entries();
            let queue_title = self.active_queue_title();
            let together = self
                .active_queue_delivery()
                .map(|delivery| delivery == caudra_agent::QueueDelivery::TogetherNextTurn);
            let drawn = queue_panel::view(
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
            self.queue_hits = drawn.hits;
            self.queue_cut_rows = drawn.cut_rows;
            self.todo_panel.view(frame, layout.todo_area);
            for &(idx, rect) in &layout.panel_windows {
                self.float_mgr.view_panel(frame, idx, rect);
            }
            let defers = self.prompt_defers();
            let placeholder = if self.queue_editor_active() {
                Placeholder::QueueEdit
            } else if defers {
                Placeholder::Queue
            } else if crate::session_history_head(&self.state.session).is_none() {
                Placeholder::Suggestion
            } else {
                Placeholder::Blank
            };
            let panel_hint = if defers && !self.queue_editor_active() {
                let (hint, hits) = input::admission_hint(
                    layout.input_area,
                    self.admission_hover,
                    &input::MAIN_ADMISSIONS,
                );
                self.admission_hits = hits;
                Some(hint)
            } else {
                self.chord_hint(layout.input_area)
                    .or_else(|| self.lua_hint_line())
            };
            let ghost = self.composer_palette_ghost();
            self.input_box.view(
                frame,
                layout.input_area,
                placeholder,
                self.separator_style(),
                self.composer_holds_keys(),
                panel_hint,
                ghost.as_deref(),
            );
            self.view_composer_palette(frame, layout.input_area);
            self.mention_popup.view(frame, layout.input_area);
            self.commit_popup.view(frame, layout.input_area);
        }
    }

    /// The command modal draws the palette itself while it is open, since
    /// its argument prompt is what the palette follows then.
    fn composer_palette_ghost(&self) -> Option<String> {
        match self.command_modal.is_open() {
            true => None,
            false => self.command_palette.ghost(),
        }
    }

    fn view_composer_palette(&mut self, frame: &mut Frame, input_area: Rect) {
        if !self.command_modal.is_open() {
            self.command_palette.view(frame, input_area);
        }
    }

    fn render_splits(&mut self, frame: &mut Frame, layout: &ViewLayout) {
        for dir in Split::ALL {
            if let Some(rect) = layout.splits.rect(dir) {
                self.float_mgr.view_split(frame, dir, rect);
            }
        }
    }

    /// `full` is what a picker may cover. The transcript view keeps its footer
    /// out of it, since the footer draws last and would hide whatever a picker
    /// put in its rows.
    fn render_picker_overlays(&mut self, frame: &mut Frame, msg_area: Rect, full: Rect) -> Topmost {
        if self.permission_prompt.is_open() {
            return Topmost::default();
        }
        let mut overlay_rect = Rect::default();
        let mut topic = None;

        if self.search_modal.is_open() {
            let area = match self.search_modal.tab().searches_history() {
                true => full,
                false => msg_area,
            };
            overlay_rect = self.search_modal.view(frame, area);
            topic = self.search_modal.docs_topic();
        }

        if self.file_picker.is_open() {
            overlay_rect = self.file_picker.view(frame, full);
            topic = self.file_picker.docs_topic();
        }

        macro_rules! render_if_open {
            ($overlay:expr) => {
                if $overlay.is_open() {
                    overlay_rect = $overlay.view(frame, full);
                    topic = $overlay.docs_topic();
                }
            };
        }

        if self.command_modal.is_open() {
            overlay_rect = self
                .command_modal
                .view(frame, full, &mut self.command_palette);
            topic = self.command_modal.docs_topic();
        }
        render_if_open!(self.rewind_picker);
        render_if_open!(self.message_actions);
        render_if_open!(self.queue_actions);
        render_if_open!(self.review);
        render_if_open!(self.theme_picker);
        render_if_open!(self.prompt_profile_picker);
        render_if_open!(self.model_picker);
        // Not `render_if_open!`: a flashed preview draws without being open, and
        // it claims no `overlay_rect` because it answers to nothing and must not
        // suppress a selection under it.
        if self.thinking_picker.is_visible() {
            self.thinking_picker.view(frame, full);
        }
        render_if_open!(self.login_picker);
        render_if_open!(self.mcp_picker);
        render_if_open!(self.env_modal);
        render_if_open!(self.settings_modal);
        render_if_open!(self.permissions_picker);
        render_if_open!(self.stash_picker);
        render_if_open!(self.memory_inspector);
        render_if_open!(self.task_picker);
        render_if_open!(self.shell_modal);
        render_if_open!(self.workflow_inspector);
        render_if_open!(self.workflow_catalog_picker);
        if self.automation_inspector.is_open() {
            overlay_rect = self
                .automation_inspector
                .view(frame, full, self.workflow.runs());
            topic = self.automation_inspector.docs_topic();
        }
        render_if_open!(self.session_picker);
        render_if_open!(self.session_relocation_picker);
        render_if_open!(self.worktree_picker);
        render_if_open!(self.accounts_modal);
        render_if_open!(self.sandbox_manager);
        if !self.question_form.is_open() {
            render_if_open!(self.mode_submission);
            render_if_open!(self.update_prompt);
        }

        Topmost {
            rect: overlay_rect,
            topic,
        }
    }

    /// The docs draw above every modal but the chord list, since a `?` on any
    /// of them opens the docs over it and closing them returns there.
    fn render_top_modals(&mut self, frame: &mut Frame, below: Topmost) -> Topmost {
        if self.permission_prompt.is_open() {
            return below;
        }
        let Topmost {
            rect: mut overlay_rect,
            mut topic,
        } = below;
        let full = frame.area();
        macro_rules! claim {
            ($rect:expr, topic: $topic:expr) => {{
                let r = $rect;
                if r.width > 0 {
                    overlay_rect = r;
                    topic = $topic;
                }
            }};
            ($rect:expr, $overlay:expr) => {
                claim!($rect, topic: $overlay.docs_topic())
            };
        }
        claim!(self.stream_modal.view(frame, full), self.stream_modal);
        claim!(
            self.help_modal.view(frame, full, self.features),
            self.help_modal
        );
        if self.usage_modal.is_open() {
            let scope = self.usage_modal.scope();
            let ctx = UsageModalContext {
                total: &self.state.token_usage,
                total_cost: self.state.cost,
                subscription_cost: self.state.subscription_cost,
                by_model: self.state.session.usage_by_model(),
                by_purpose: self.state.session.usage_by_purpose(),
                model: &self.state.model,
                fast: self.state.fast,
                clock_format: self.ui_config.clock_format,
                lifetime: self.lifetime_usage.as_ref(),
                accounts: &self.usage_accounts,
                warming: self
                    .warming_summary
                    .as_ref()
                    .filter(|(loaded, _)| *loaded == scope)
                    .map(|(_, summary)| summary),
            };
            claim!(self.usage_modal.view(frame, full, &ctx), self.usage_modal);
        }
        if self.logs_modal.is_open() {
            let theme = crate::theme::current();
            claim!(self.logs_modal.view(frame, full, &theme), self.logs_modal);
        }
        if self.changelog_modal.is_open() {
            claim!(self.changelog_modal.view(frame, full), self.changelog_modal);
        }
        if self.context_modal.is_open() {
            let snapshot = self.context_snapshot.get();
            claim!(
                self.context_modal.view(frame, full, snapshot),
                self.context_modal
            );
        }
        if self.tools_modal.is_open() {
            let ctx = ToolsModalContext {
                snapshot: self.tools_snapshot.get(),
                basis: &self.tools_basis,
                session: self.state.session.tool_usage(),
                recorded: self.tool_stats.as_ref(),
            };
            claim!(self.tools_modal.view(frame, full, &ctx), self.tools_modal);
        }
        if self.decisions_modal.is_open() {
            claim!(self.view_decisions_modal(frame, full), self.decisions_modal);
        }
        if self.skills_modal.is_open() {
            let snapshot = self.context_snapshot.get();
            claim!(
                self.skills_modal.view(frame, full, snapshot),
                self.skills_modal
            );
        }
        if self.storage_modal.is_open() {
            claim!(self.storage_modal.view(frame, full), self.storage_modal);
        }
        if self.learn_inspector.is_open() {
            claim!(self.learn_inspector.view(frame, full), self.learn_inspector);
        }
        if self.learn_transcripts.is_open() {
            claim!(
                self.learn_transcripts.view(frame, full),
                self.learn_transcripts
            );
        }
        if self.system_prompt_modal.is_open() {
            claim!(
                self.system_prompt_modal.view(frame, full),
                self.system_prompt_modal
            );
        }
        if self.projection_modal.is_open() {
            claim!(
                self.projection_modal.view(frame, full),
                self.projection_modal
            );
        }
        if self.goal_panel.is_open() {
            let status = self.state.goal.status();
            let evaluator =
                caudra_providers::model_registry::binding(caudra_providers::ModelPurpose::Goal);
            let content = StatusContent {
                status: status.as_ref(),
                evaluator: evaluator.as_ref(),
                continuation_limit: self.state.goal.continuation_limit(),
            };
            claim!(self.goal_panel.view(frame, full, &content), self.goal_panel);
        }
        if self.autopilot_panel.is_open() {
            claim!(self.draw_autopilot_panel(frame, full), self.autopilot_panel);
        }
        claim!(self.float_mgr.view(frame, full), self.float_mgr);
        if self.peer_manager.is_open() {
            overlay_rect = self.peer_manager.view(frame, full);
            topic = self.peer_manager.docs_topic();
        }
        claim!(self.paste_editor.view(frame, full), self.paste_editor);
        claim!(self.image_viewer.view(frame, full), self.image_viewer);
        if self.docs_modal.is_open() {
            claim!(self.docs_modal.view(frame, full), self.docs_modal);
        }
        // Last, so a pending chord's list sits over whatever it was armed on.
        let chords = keybindings::leader_chords(&self.leader_contexts(), self.features);
        claim!(self.which_key.view(frame, full, &chords), topic: None);
        Topmost {
            rect: overlay_rect,
            topic,
        }
    }

    fn render_status_bar(&mut self, frame: &mut Frame, status_area: Rect, render_chat: usize) {
        grab_scope!("status_bar", status_area);
        let pending_trust = self.pending_trust();
        let config_problems = self.config_problems.load();
        let chat = &self.chats[render_chat];
        let goal = self.state.goal.snapshot();
        let chat_name = (self.chats.len() > 1).then_some(chat.name.as_str());
        let sandbox = attached_sandbox_instance(&self.state.session, &self.sandbox_live);
        let mode = self.mode_label();
        let main_chat = render_chat == 0;
        let effective_model = main_chat.then(|| self.status_main_model()).flatten();
        let model = effective_model
            .as_ref()
            .map_or(&self.state.model, |slot| &slot.model);
        let effective_model_spec = effective_model.as_ref().map(|slot| slot.model.spec());
        // Read whether or not a turn is running: the snapshot now carries the
        // provider's own count, so it is the same number the chat records at
        // turn end and the bar no longer jumps between two ways of measuring.
        let effective_context = main_chat.then(|| self.main_context_snapshot()).flatten();
        // What the request will actually carry, not what was asked for: a
        // model can refuse to stop reasoning, and the badge has to say so. A
        // task resolved its own level against its own model, which a profile
        // may have overridden, so its footer draws what it carries rather than
        // what the session asked for.
        let thinking = match main_chat {
            true => model.supports_thinking().then(|| {
                RequestOptions {
                    thinking: self.state.thinking.clone(),
                    fast: self.state.fast,
                    ..RequestOptions::default()
                }
                .clamped(model)
                .thinking
                .resolve(model)
                .to_string()
                .into()
            }),
            false => chat.thinking.clone().map(Cow::Owned),
        };
        // A toggle swaps the selection to the model its mode was left on. While
        // work runs, neither reaches the agent until the session settles or the
        // next message carries them. Until then, the slot names the model still
        // in force and puts the one arriving behind the arrow.
        let leaving = (main_chat
            && self.state.applied_mode != self.state.mode
            && self.state.applied_model != self.state.session.model)
            .then(|| {
                effective_model_spec
                    .as_deref()
                    .unwrap_or(&self.state.applied_model)
            });
        let activity = self.task_activity();
        let autopilot = self.autopilot_chip();
        let decisions = self.permissions.decisions();
        let hover_hint = (!self.has_modal_overlay())
            .then(|| chat.hovered_hint())
            .flatten();
        let (context_size, context_window) = self.context_usage(
            chat,
            effective_model.as_deref(),
            effective_context.as_deref(),
        );
        let ctx = StatusBarContext {
            status: &self.status,
            mode,
            model_id: match leaving {
                Some(_) => &self.state.session.model,
                None => effective_model_spec.as_deref().unwrap_or_else(|| {
                    chat.model_id
                        .as_deref()
                        .unwrap_or(&self.state.session.model)
                }),
            },
            pending_model: leaving.map(Cow::Borrowed),
            stats: UsageStats {
                global_cost: self.state.cost,
                global_subscription_cost: self.state.subscription_cost,
                context_size,
                cost: chat.cost,
                subscription_cost: chat.subscription_cost,
                context_window,
                compaction_border: effective_context
                    .as_ref()
                    .and_then(|snapshot| snapshot.window.compaction_border()),
                show_global: self.chats.len() > 1,
            },
            auto_scroll: chat.auto_scroll(),
            chat_name,
            sandbox,
            main_chat,
            retry_info: chat.retry(),
            thinking,
            fast: if main_chat {
                self.state.fast
            } else {
                chat.fast
            },
            workflows: workflow_chip(self.workflow.runs()),
            permission_mode: self.permissions.mode(),
            decisions: decisions.as_ref().and_then(|decisions| {
                decisions.endpoint().map(|endpoint| DecisionsChip {
                    endpoint: &endpoint.name,
                    offline: decisions.status().reachable == Some(false),
                })
            }),
            account: account_chip(self.routed_account.as_ref(), &self.state.model.provider),
            waiting_questions: self.waiting_question_count(),
            config_problems: &config_problems,
            pending_trust,
            restoring: self.restoring.load(Ordering::Relaxed),
            background_waiting: self.waiting_for_background(),
            goal: goal.as_ref(),
            autopilot: autopilot.as_ref(),
            active_tasks: activity.agents,
            active_shells: activity.shells,
            bash_input: self.is_bash_input(),
            hovered: (!self.has_modal_overlay())
                .then_some(self.status_hover)
                .flatten(),
            hover_hint: hover_hint.as_deref(),
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

        if layout.input_area.height > 0 && !layout.bottom_takeover && self.composer_is_interactive()
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
            || self.plan_form_in_view()
        {
            self.zones.push_overlay(layout.bottom_area);
        }

        for &(_, rect) in &layout.panel_windows {
            self.zones.push_overlay(selection::inset_border(rect));
        }

        if !self.is_main_chat()
            && !self.active_subagent_has_composer()
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
        if self.search_modal.is_open() && overlay_rect == self.search_modal.area() {
            for pane in self.search_modal.text_panes() {
                if !pane.is_empty() {
                    self.zones.push_overlay(pane);
                }
            }
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
        // The composer selects logical text and paints it itself, so a second
        // screen-cell highlight over the same cells would only disagree with it.
        if sel.zone != SelectionZone::Input {
            let scroll = self.scroll_offset(sel.zone);
            if let Some(screen_sel) = sel.to_screen(scroll) {
                selection::apply_highlight(frame.buffer_mut(), sel.highlight_area(), &screen_sel);
            }
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

    /// The chord the composer's top row offers, and the cells it takes. Only
    /// one is ever drawn: a ready plan speaks before the todo count, and both
    /// before the task picker, so the hit recorded here is the only control the
    /// row holds this frame.
    fn chord_hint(&mut self, area: Rect) -> Option<Line<'static>> {
        let (topic, text) = self.chord_subject()?;
        let target = topic.target();
        let hovered = self.chord_hint_hover == Some(target);
        let (line, hit) = input::chord_hint(area, &text, target, hovered);
        self.chord_hint_hit = Some(hit);
        Some(line)
    }

    /// What the chord hint is about and the label it reads, for the hint and
    /// its tooltip alike, so the two cannot disagree.
    pub(super) fn chord_subject(&self) -> Option<(ChordTopic, String)> {
        if self.is_main_chat()
            && self.state.mode == Mode::Plan
            && let Some(label) = self.plan_form.hint_label()
        {
            Some((ChordTopic::Plan, label.to_string()))
        } else if let Some(progress) = self.todo_panel.hint_label() {
            Some((ChordTopic::Todo, progress))
        } else {
            Some((ChordTopic::Tasks, self.task_hint_text()?))
        }
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
                SidebarView::Transfer => KeybindContext::WorkbenchTransfer,
            });
            if self.workbench.focus() == Focus::Editor && !self.workbench.transfer_input_active() {
                contexts.push(KeybindContext::WorkbenchEditor);
            }
        } else if self.image_viewer.is_open() {
            contexts.push(KeybindContext::ImageViewer);
        } else if self.paste_editor.is_open() {
            contexts.push(KeybindContext::PasteEditor);
        } else if self.review.is_open() {
            contexts.push(KeybindContext::Review);
        } else if self.plan_form_in_view() {
            contexts.push(KeybindContext::FormInput);
        } else if self.queue_editor_active() {
            contexts.push(KeybindContext::Editing);
        } else if self.active_queue_is_focused() {
            contexts.push(KeybindContext::QueueFocus);
        } else if self.rewind_picker.is_open() {
            contexts.push(KeybindContext::RewindPicker);
        } else if self.theme_picker.is_open()
            || self.prompt_profile_picker.is_open()
            || self.thinking_picker.is_open()
        {
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
            if self.prompt_defers() {
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
