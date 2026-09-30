use super::{
    Confirmation, Control, DocumentMode, FUTURE_ONLY, Focus, Manager, ReadSurface, RecordKind,
    SandboxManager, SandboxName, SandboxView, SnapshotState, UNAVAILABLE,
    live::{INSTANCE_ACTIONS, Kind},
};
use crate::components::{
    Hint, HintBar, Overlay, escape_terminal_controls, json_text, keybindings::key,
};
use crate::sandbox::{NETWORK_SAVE_NOTICE, SandboxInstanceState, affected_network};
use crate::theme;
use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::collections::BTreeSet;

const WIDE_WIDTH: u16 = 100;
const LIST_WIDTH: u16 = 30;
const STATUS_ROWS: u16 = 3;
const FIELD_EDITOR_ROWS: u16 = 7;
const FORM_HELP_ROWS: u16 = 4;
const FORM_SUMMARY_ROWS: u16 = 3;
const PAUSED_DISK: &str = "Paused / stopped execution: persistent disk retained, subject to disk retention. Memory/process state is not saved; Resume is a cold boot.\n";
const DELETED_DISK: &str = "Deleted: no paused disk to Resume.\n";
const NO_EXPIRY_DEADLINE: &str = "none (runs until paused or deleted)";
const UNAVAILABLE_DEADLINE: &str = "unavailable";

impl SandboxManager {
    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.is_open() {
            return Rect::ZERO;
        }
        if let Some(state) = self.state.as_mut() {
            state.view(frame, area);
        }
        area
    }
}

impl Manager {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = theme::current();
        if self.area != area {
            self.cancel_selections();
            self.reset_mouse();
            if self.instance_action.is_some() {
                self.reveal_reference = true;
            }
            if self.focus == Focus::Detail
                && let Some(form) = self.form.as_mut()
            {
                form.reveal_focus = true;
            }
            if self.focus == Focus::List {
                self.reveal_selected = true;
            }
        }
        self.area = area;
        self.hits.clear();
        self.editor_area = Rect::ZERO;
        self.list_area = Rect::ZERO;
        self.fields_area = Rect::ZERO;
        self.references_area = Rect::ZERO;
        for reader in &mut self.readers {
            reader.visible = false;
        }
        frame.render_widget(Clear, area);
        let state = if self.live_pending.is_some() {
            "Live operation pending · Esc closes, NOT cancels"
        } else if self.pending.is_some() {
            "Persistence pending"
        } else if self.dirty() {
            "Draft · Unsaved"
        } else {
            "Saved defaults"
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Sandboxes · {state} "))
            .border_style(theme.tool_dim);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let hint_rows = self.footer();
        let status_rows = if self.confirmation.is_some() {
            STATUS_ROWS.min(inner.height.saturating_sub(hint_rows.len() as u16 + 4))
        } else {
            STATUS_ROWS
        };
        let [tabs, body, status, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(status_rows),
            Constraint::Length(hint_rows.len() as u16),
        ])
        .areas(inner);
        let tab_areas = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(tabs);
        for (index, view) in [
            SandboxView::Instances,
            SandboxView::Profiles,
            SandboxView::Images,
            SandboxView::Providers,
        ]
        .into_iter()
        .enumerate()
        {
            let style = if self.view == view {
                theme.keybind_key.add_modifier(Modifier::REVERSED)
            } else {
                theme.tool_dim
            };
            frame.render_widget(
                Paragraph::new(format!("{} {}", index + 1, view.label())).style(hover_style(
                    style,
                    self.hovered == Some(Control::View(view.clone())),
                )),
                tab_areas[index],
            );
            self.hits.push((tab_areas[index], Control::View(view)));
        }
        if self.confirmation.is_some() {
            self.view_confirmation(frame, body);
        } else if self.live_pending.is_some() {
            self.readers[ReadSurface::Body as usize].view(frame, body, safe(&self.status));
        } else if self.live_form.is_some() {
            self.view_live(frame, body);
        } else if self.instance_action.is_some() {
            self.view_instance_actions(frame, body);
        } else if self.references.is_some() {
            self.view_references(frame, body);
        } else if let Some(document) = self.document.as_mut() {
            let title = if document.destination.is_some() {
                "New client file (absolute path)"
            } else {
                match document.mode {
                    DocumentMode::Import => "Import configuration draft",
                    DocumentMode::Export => "Export preview · references only",
                    DocumentMode::Compare => "Baseline / Draft / External disk · read-only",
                    DocumentMode::LiveReport => "Live action · status / result",
                }
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(hover_style(
                    Style::default(),
                    self.hovered == Some(Control::Editor),
                ));
            self.editor_area = block.inner(body);
            frame.render_widget(block, body);
            if let Some(destination) = document.destination.as_mut() {
                destination.view(frame, self.editor_area);
            } else {
                document.editor.view_json(frame, self.editor_area);
            }
        } else if area.width >= WIDE_WIDTH {
            let [list, detail] =
                Layout::horizontal([Constraint::Length(LIST_WIDTH), Constraint::Min(1)])
                    .areas(body);
            self.view_list(frame, list);
            self.view_detail(frame, detail);
        } else if self.detail {
            self.view_detail(frame, body);
        } else {
            self.view_list(frame, body);
        }
        self.readers[ReadSurface::Status as usize].view(frame, status, safe(&self.status));
        if self.list_area.is_empty() {
            self.list_bar = Default::default();
        }
        if self.fields_area.is_empty() {
            self.fields_bar = Default::default();
        }
        if self.references_area.is_empty() {
            self.references_bar = Default::default();
        }
        for surface in ReadSurface::ALL {
            if !self.readers[surface as usize].visible
                || self.readers[surface as usize].area.is_empty()
            {
                self.readers[surface as usize].area = Rect::ZERO;
                self.readers[surface as usize].editor.cancel_selection();
                self.readers[surface as usize].fingerprint = None;
                if self.reader_focus == Some(surface) {
                    self.reader_focus = None;
                }
                if self.reader_capture == Some(surface) {
                    self.reader_capture = None;
                }
            }
        }
        self.hints.resize_with(hint_rows.len(), HintBar::default);
        for (row, hints) in hint_rows.into_iter().enumerate() {
            let area = Rect {
                y: footer.y.saturating_add(row as u16),
                height: u16::from(row < footer.height as usize),
                ..footer
            };
            self.hints[row].draw(frame, area, hints);
        }
    }

    fn footer(&self) -> Vec<Vec<Hint>> {
        if self.instance_action.is_some() {
            return vec![vec![
                Hint::key("↑/↓", KeyCode::Down, "Select"),
                Hint::key("Home", KeyCode::Home, "First"),
                Hint::key("End", KeyCode::End, "Last"),
                Hint::key("Enter", KeyCode::Enter, "Draft / inspect"),
                Hint::key("Esc", KeyCode::Esc, "Back"),
            ]];
        }
        if self.references.is_some() {
            return vec![vec![
                Hint::key("Enter", KeyCode::Enter, "Use reference"),
                Hint::key("Esc", KeyCode::Esc, "Back"),
            ]];
        }
        if let Some(confirmation) = &self.confirmation {
            return match confirmation {
                Confirmation::Dirty { .. } => vec![
                    vec![Hint::char("k", "Keep editing"), Hint::char("s", "Save")],
                    vec![
                        Hint::char("d", "Discard draft"),
                        Hint::key("Esc", KeyCode::Esc, "Keep editing"),
                    ],
                ],
                Confirmation::DeleteProfile { .. } => vec![vec![
                    Hint::char("k", "Keep profile"),
                    Hint::char("s", "Confirm deletion"),
                ]],
                Confirmation::Live { .. } | Confirmation::NetworkSave { .. } => vec![
                    vec![
                        Hint::char("k", "Keep draft"),
                        Hint::char("s", "Accept action"),
                    ],
                    vec![
                        Hint::inert("PgUp/Dn", "Review full payload"),
                        Hint::key("Esc", KeyCode::Esc, "Back (no action)"),
                    ],
                ],
            };
        }
        if self.live_pending.is_some() {
            return vec![vec![Hint::key("Esc", KeyCode::Esc, "Close (NOT cancel)")]];
        }
        if let Some(form) = &self.live_form {
            if form.picker.is_some() {
                return vec![vec![
                    Hint::key("Enter", KeyCode::Enter, "Open / select host file"),
                    Hint::key("Backspace", KeyCode::Backspace, "Parent"),
                    Hint::key("Esc", KeyCode::Esc, "Cancel picker"),
                ]];
            }
            let image = matches!(
                form.kind,
                Kind::ImportImage | Kind::Build | Kind::Gc | Kind::InspectImage
            );
            return vec![
                vec![
                    Hint::bind(key::SANDBOX_APPLY, "Review action"),
                    Hint::key("Tab", KeyCode::Tab, "Field"),
                ],
                vec![
                    Hint::key(
                        "F2",
                        KeyCode::F(2),
                        if image { "Host qcow2 picker" } else { "" },
                    ),
                    Hint::key("F3", KeyCode::F(3), "Cycle choice"),
                    Hint::key(
                        "F4",
                        KeyCode::F(4),
                        if image {
                            "Review image probe"
                        } else {
                            "Test rules"
                        },
                    ),
                ],
                vec![
                    Hint::key("F6", KeyCode::F(6), "Discard draft"),
                    Hint::key("Esc", KeyCode::Esc, "Back · keeps draft"),
                ],
            ];
        }
        if let Some(document) = &self.document {
            let action = if document.destination.is_some() {
                Hint::key("Enter", KeyCode::Enter, "Publish new file")
            } else {
                match document.mode {
                    DocumentMode::Import => Hint::bind(key::SANDBOX_APPLY, "Apply import to draft"),
                    DocumentMode::Export => Hint::bind(key::SAVE, "Save as"),
                    DocumentMode::Compare | DocumentMode::LiveReport => {
                        Hint::inert("Ctrl+A/C", "Select / copy preview")
                    }
                }
            };
            return vec![vec![action], vec![Hint::key("Esc", KeyCode::Esc, "Back")]];
        }
        let mut rows = vec![vec![
            Hint::bind(key::SAVE, "Save"),
            Hint::key("Tab", KeyCode::Tab, "Focus"),
            Hint::key("Esc", KeyCode::Esc, "Back"),
        ]];
        if self.form.as_ref().is_some_and(|form| form.editing) {
            rows.push(vec![
                Hint::bind(key::SANDBOX_APPLY, "Apply"),
                Hint::inert("Ctrl+Z/Y", "Undo/redo"),
            ]);
        } else {
            if self.view == SandboxView::Instances {
                return vec![
                    vec![
                        Hint::key("F3", KeyCode::F(3), "Instance actions / recovery"),
                        Hint::char("a", "Attach"),
                        Hint::char("u", "Resume"),
                        Hint::char("p", "Pause"),
                    ],
                    vec![
                        Hint::char("e", "Extend"),
                        Hint::key("Del", KeyCode::Delete, "Delete / detach"),
                        Hint::char("r", "Reconcile"),
                    ],
                    vec![
                        Hint::char("g", "Network"),
                        Hint::char("z", "Cancel create"),
                        Hint::char("h", "Doctor"),
                    ],
                    vec![
                        Hint::char("d", "Detach record (keep VM)"),
                        Hint::char("f", "Acknowledge failure"),
                    ],
                    vec![
                        Hint::key("Enter", KeyCode::Enter, "Inspect"),
                        Hint::char("/", "Search"),
                        Hint::key("Esc", KeyCode::Esc, "Back"),
                    ],
                ];
            }
            if self.view == SandboxView::Images {
                return vec![
                    vec![
                        Hint::char("i", "Import (daemon stopped)"),
                        Hint::char("b", "Build (daemon stopped)"),
                        Hint::char("g", "GC"),
                    ],
                    vec![Hint::char("l", "Local inspect"), Hint::char("h", "Doctor")],
                    vec![
                        Hint::key("Enter", KeyCode::Enter, "Inspect"),
                        Hint::char("/", "Search"),
                        Hint::key("Esc", KeyCode::Esc, "Back"),
                    ],
                ];
            }
            rows.push(vec![
                Hint::key(
                    "Enter",
                    KeyCode::Enter,
                    if self.focus == Focus::Detail {
                        "Edit"
                    } else {
                        "Inspect"
                    },
                ),
                Hint::char("/", "Search"),
                Hint::key("F2", KeyCode::F(2), "Choose"),
            ]);
            if self.kind().is_some() {
                let mut actions = vec![Hint::char("n", "New"), Hint::char("d", "Duplicate")];
                if self.kind() == Some(RecordKind::Profile) {
                    actions.push(Hint::key("Del", KeyCode::Delete, "Delete"));
                }
                rows.push(actions);
            }
            if self.view == SandboxView::Profiles {
                rows.push(vec![
                    Hint::char("g", "Networks"),
                    Hint::char("t", "Transfers"),
                ]);
                if self.kind() == Some(RecordKind::Profile) {
                    rows.push(vec![
                        Hint::char("v", "Create VM"),
                        Hint::char("h", "Doctor"),
                    ]);
                }
            }
            if self.view == SandboxView::Providers {
                rows.push(vec![
                    Hint::char("k", "Edit credential"),
                    Hint::char("h", "Doctor"),
                ]);
            }
            rows.push(vec![
                Hint::char("i", "Import"),
                Hint::char("x", "Export"),
                Hint::char("r", "Reload"),
            ]);
            rows.push(vec![Hint::char("c", "Compare"), Hint::char("a", "Save as")]);
        }
        rows
    }

    fn view_list(&mut self, frame: &mut Frame, area: Rect) {
        let theme = theme::current();
        let label = match self.policies {
            Some(RecordKind::Network) => "Shared network policies",
            Some(RecordKind::Transfer) => "Shared transfer policies",
            _ => self.view.label(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(label)
            .border_style(if self.focus == Focus::List {
                theme.keybind_key
            } else {
                theme.tool_dim
            });
        let inner = block.inner(area);
        self.list_area = inner;
        frame.render_widget(block, area);
        let [search, rows] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        self.hits.push((search, Control::Search));
        if self.focus == Focus::Search {
            self.search.view(frame, search);
            frame.buffer_mut().set_style(
                search,
                hover_style(Style::default(), self.hovered == Some(Control::Search)),
            );
        } else {
            frame.render_widget(
                Paragraph::new(format!("/ Search: {}", safe(&self.search.text()))).style(
                    hover_style(theme.tool_dim, self.hovered == Some(Control::Search)),
                ),
                search,
            );
        }
        let entries = self.entries();
        if entries.is_empty() {
            let empty = if self.baseline.is_none() {
                self.status.clone()
            } else if self.kind().is_some() {
                "No matching saved records. n New; i Import.".into()
            } else {
                self.snapshot_status()
            };
            self.readers[ReadSurface::Empty as usize].view(frame, rows, safe(&empty));
            self.list_bar = Default::default();
            self.list_bar.draw(frame, rows, 0_u32, 0_u32);
            return;
        }
        if self.reveal_selected {
            self.list_scroll = self.list_scroll.min(self.selected);
            if self.selected >= self.list_scroll + rows.height as usize {
                self.list_scroll = self
                    .selected
                    .saturating_sub(rows.height.saturating_sub(1) as usize);
            }
            self.reveal_selected = false;
        }
        self.list_scroll = self
            .list_scroll
            .min(entries.len().saturating_sub(rows.height as usize));
        for (row, (index, entry)) in entries
            .iter()
            .enumerate()
            .skip(self.list_scroll)
            .take(rows.height as usize)
            .enumerate()
        {
            let area = Rect {
                y: rows.y + row as u16,
                height: 1,
                ..rows
            };
            let style = if index == self.selected {
                theme.keybind_key.add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            frame.render_widget(
                Paragraph::new(safe(entry)).style(hover_style(
                    style,
                    self.hovered == Some(Control::Row(index)),
                )),
                area,
            );
            self.hits.push((area, Control::Row(index)));
        }
        self.list_bar
            .draw(frame, rows, entries.len() as u32, self.list_scroll as u32);
    }

    fn view_references(&mut self, frame: &mut Frame, area: Rect) {
        self.hits.clear();
        let Some(picker) = self.references.as_mut() else {
            return;
        };
        let [search, rows] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Filter references · Enter applies to draft")
            .border_style(hover_style(
                Style::default(),
                self.hovered == Some(Control::Editor),
            ));
        let inner = block.inner(search);
        self.editor_area = inner;
        frame.render_widget(block, search);
        picker.search.view(frame, inner);
        let filtered = picker.filtered();
        self.references_area = rows;
        if filtered.is_empty() {
            self.references_bar = Default::default();
            self.readers[ReadSurface::Empty as usize].view(frame, rows, "No matching references. Create a provider/policy first, or wait for authenticated catalog metadata.".into());
        }
        if self.reveal_reference {
            self.reference_scroll = self.reference_scroll.min(picker.selected);
            if picker.selected >= self.reference_scroll + rows.height as usize {
                self.reference_scroll = picker
                    .selected
                    .saturating_sub(rows.height.saturating_sub(1) as usize);
            }
            self.reveal_reference = false;
        }
        self.reference_scroll = self
            .reference_scroll
            .min(filtered.len().saturating_sub(rows.height as usize));
        let start = self.reference_scroll;
        for (row, index) in filtered
            .iter()
            .skip(start)
            .take(rows.height as usize)
            .enumerate()
        {
            let area = Rect {
                y: rows.y + row as u16,
                height: 1,
                ..rows
            };
            let style = if start + row == picker.selected {
                theme::current()
                    .keybind_key
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            frame.render_widget(
                Paragraph::new(safe(&picker.choices[*index].label)).style(hover_style(
                    style,
                    self.hovered == Some(Control::Reference(*index)),
                )),
                area,
            );
            self.hits.push((area, Control::Reference(*index)));
        }
        self.references_bar
            .draw(frame, rows, filtered.len() as u32, start as u32);
    }

    fn view_instance_actions(&mut self, frame: &mut Frame, area: Rect) {
        let selected = self.instance_action.unwrap_or_default();
        let context = self.selected_instance().map_or_else(
            || "No selected instance · refresh or select a row".into(),
            |row| {
                format!(
                    "{} · {:?} · {}",
                    row.id,
                    row.state,
                    if row
                        .record
                        .as_ref()
                        .and_then(|record| record.lifecycle.as_ref())
                        .is_some_and(|intent| intent.is_pending())
                    {
                        "PENDING: Inspect / Reconcile first; acknowledge failure separately"
                    } else {
                        "Select an action; nothing executes until reviewed and confirmed"
                    }
                )
            },
        );
        let [summary, rows, help] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(4),
        ])
        .areas(area);
        self.readers[ReadSurface::Summary as usize].view(frame, summary, safe(&context));
        self.references_area = rows;
        if self.reveal_reference {
            self.reference_scroll = self.reference_scroll.min(selected);
            if selected >= self.reference_scroll + rows.height as usize {
                self.reference_scroll =
                    selected.saturating_sub(rows.height.saturating_sub(1) as usize);
            }
            self.reveal_reference = false;
        }
        self.reference_scroll = self
            .reference_scroll
            .min(INSTANCE_ACTIONS.len().saturating_sub(rows.height as usize));
        for (index, (label, kind)) in INSTANCE_ACTIONS
            .iter()
            .enumerate()
            .skip(self.reference_scroll)
            .take(rows.height as usize)
        {
            let disabled = kind
                .as_ref()
                .and_then(|kind| self.instance_action_error(kind));
            let mut style = if disabled.is_some() {
                theme::current().tool_dim
            } else {
                theme::current().keybind_key
            };
            if index == selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let row = Rect {
                y: rows.y + (index - self.reference_scroll) as u16,
                height: 1,
                width: rows.width.saturating_sub(1),
                ..rows
            };
            frame.render_widget(
                Paragraph::new(format!(
                    "{} {label}{}",
                    if index == selected { ">" } else { " " },
                    if disabled.is_some() {
                        " · unavailable"
                    } else {
                        ""
                    }
                ))
                .style(hover_style(
                    style,
                    self.hovered == Some(Control::InstanceAction(index)),
                )),
                row,
            );
            self.hits.push((row, Control::InstanceAction(index)));
        }
        self.references_bar.draw(
            frame,
            rows,
            INSTANCE_ACTIONS.len() as u32,
            self.reference_scroll as u32,
        );
        let explained = match self.hovered {
            Some(Control::InstanceAction(index)) => index,
            _ => selected,
        };
        let (label, kind) = &INSTANCE_ACTIONS[explained];
        let reason = kind.as_ref().and_then(|kind| self.instance_action_error(kind)).unwrap_or_else(|| "Available: Enter opens a draft for explicit review. Inspect only reads; acknowledgement never retries or resumes.".into());
        self.readers[ReadSurface::Help as usize].view(
            frame,
            help,
            safe(&format!("{label}\n{reason}")),
        );
    }

    fn view_detail(&mut self, frame: &mut Frame, area: Rect) {
        let area = if self.view == SandboxView::Instances {
            let [actions, detail] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
            frame.render_widget(
                Paragraph::new(" [F3] Instance actions / recovery ▸ ").style(hover_style(
                    theme::current().keybind_key,
                    self.hovered == Some(Control::InstanceActions),
                )),
                actions,
            );
            self.hits.push((actions, Control::InstanceActions));
            detail
        } else {
            area
        };
        if self.form.is_none() {
            let mut text = self.read_only_detail();
            if self.view == SandboxView::Instances
                && let Some(report) = &self.network_report
            {
                text = format!("Saved-network reconciliation\n{report}\n\n{text}");
            }
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Inspect · no lifecycle actions");
            let inner = block.inner(area);
            frame.render_widget(block, area);
            self.readers[ReadSurface::Body as usize].view(frame, inner, safe(&text));
            if self.detail_scroll != 0 {
                self.readers[ReadSurface::Body as usize]
                    .editor
                    .scroll(-i32::from(self.detail_scroll));
                self.readers[ReadSurface::Body as usize]
                    .editor
                    .view_json(frame, inner);
                self.detail_scroll = 0;
            }
            return;
        }
        let theme = theme::current();
        let summary = self.form_summary();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let title = format!(
            "{:?} · {}",
            form.kind,
            if form.dirty() {
                "Draft"
            } else {
                "Saved defaults"
            }
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(if self.focus == Focus::Detail {
                theme.keybind_key
            } else {
                theme.tool_dim
            });
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [summary_area, rows, help, editor] = Layout::vertical([
            Constraint::Length(if form.kind == RecordKind::Network {
                inner.height / 2
            } else {
                FORM_SUMMARY_ROWS
            }),
            Constraint::Min(1),
            Constraint::Length(FORM_HELP_ROWS),
            Constraint::Length(if form.editing { FIELD_EDITOR_ROWS } else { 0 }),
        ])
        .areas(inner);
        self.readers[ReadSurface::Summary as usize].view(frame, summary_area, safe(&summary));
        self.fields_area = rows;
        if form.reveal_focus {
            form.scroll = form.scroll.min(form.focus);
            if form.focus >= form.scroll + rows.height as usize {
                form.scroll = form
                    .focus
                    .saturating_sub(rows.height.saturating_sub(1) as usize);
            }
            form.reveal_focus = false;
        }
        form.scroll = form
            .scroll
            .min(form.fields.len().saturating_sub(rows.height as usize));
        for (row, (index, field)) in form
            .fields
            .iter()
            .enumerate()
            .skip(form.scroll)
            .take(rows.height as usize)
            .enumerate()
        {
            let area = Rect {
                y: rows.y + row as u16,
                height: 1,
                ..rows
            };
            let marker = if field.error.is_some() {
                "!"
            } else if field.locked.is_some() {
                "[locked]"
            } else {
                ""
            };
            let value = field.text().replace('\n', ", ");
            let style = if index == form.focus && self.focus == Focus::Detail {
                theme.keybind_key.add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let mut spans = vec![Span::raw(format!("{}: ", field.label))];
            spans.extend(json_text::line(&safe(&value)).spans);
            spans.push(Span::raw(format!(" {marker}")));
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(hover_style(
                    style,
                    self.hovered == Some(Control::Field(index)),
                )),
                area,
            );
            self.hits.push((area, Control::Field(index)));
        }
        self.fields_bar
            .draw(frame, rows, form.fields.len() as u32, form.scroll as u32);
        let field = &mut form.fields[form.focus];
        let guidance = field
            .error
            .as_deref()
            .or(field.locked.as_deref())
            .unwrap_or(field.help);
        self.readers[ReadSurface::Help as usize].view(frame, help, safe(guidance));
        if form.editing {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!("{} · Apply field ≠ Save", field.label))
                .border_style(hover_style(
                    Style::default(),
                    self.hovered == Some(Control::Editor),
                ));
            self.editor_area = block.inner(editor);
            frame.render_widget(block, editor);
            field.editor.view_json(frame, self.editor_area);
        }
    }

    fn form_summary(&self) -> String {
        let Some(form) = &self.form else {
            return String::new();
        };
        if form.kind == RecordKind::Network {
            let networks: BTreeSet<_> = SandboxName::parse(&form.text("name"))
                .into_iter()
                .chain(form.original.clone())
                .collect();
            let instances = self
                .snapshot
                .as_ref()
                .and_then(|snapshot| match &snapshot.instances {
                    SnapshotState::Ready(rows) => Some(
                        rows.iter()
                            .filter_map(|row| {
                                let record = row.record.as_ref()?;
                                if !affected_network(record, &networks) {
                                    return None;
                                }
                                let launch = record.launch.as_ref()?.configuration();
                                let current = self
                                    .draft
                                    .profiles
                                    .get(&launch.profile_name)
                                    .map(|profile| profile.network.as_str())
                                    .unwrap_or("missing");
                                Some(format!(
                                    "{} (launch network {}; current profile network {current})",
                                    record.name,
                                    launch.profile.value().network
                                ))
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    "inventory unavailable; worker discovers affected owned instances after commit"
                        .into()
                });
            return format!(
                "{NETWORK_SAVE_NOTICE}\nAffected existing instances (last snapshot): {}",
                if instances.is_empty() {
                    "none"
                } else {
                    &instances
                }
            );
        }
        match form.kind {
            RecordKind::Profile => {
                let provider = SandboxName::parse(&form.text("provider"))
                    .ok()
                    .and_then(|name| self.provider(&name, &self.draft));
                let capability = if provider.is_some() {
                    "Provider limits checked. F2 chooses references."
                } else {
                    "Offline defaults: capability compatibility unverified."
                };
                let choices = match form.fields[form.focus].key {
                    "provider" => self
                        .draft
                        .providers
                        .keys()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                    "network" => self
                        .draft
                        .networks
                        .keys()
                        .map(ToString::to_string)
                        .collect(),
                    "transfer" => self
                        .draft
                        .transfers
                        .keys()
                        .map(ToString::to_string)
                        .collect(),
                    _ => Vec::new(),
                };
                if choices.is_empty() {
                    format!("Future defaults; running instances unchanged.\n{capability}")
                } else {
                    format!(
                        "Future defaults; running instances unchanged.\nF2 chooses: {}",
                        choices.join(", ")
                    )
                }
            }
            RecordKind::Network | RecordKind::Transfer | RecordKind::Provider => {
                let affected = form
                    .original
                    .as_ref()
                    .map(|name| {
                        self.draft
                            .affected_profiles(form.kind.clone(), name)
                            .into_iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let network = if form.kind == RecordKind::Network {
                    if form.text("enforcement") == "off" {
                        "UNRESTRICTED (saved default only)."
                    } else {
                        "Deny by default. Operator blocks remain locked."
                    }
                } else {
                    "No live effects."
                };
                format!(
                    "Future defaults only. Affected profiles: {}\n{network}",
                    if affected.is_empty() {
                        "none"
                    } else {
                        &affected
                    }
                )
            }
        }
    }

    pub(super) fn snapshot_status(&self) -> String {
        let Some(snapshot) = &self.snapshot else {
            return UNAVAILABLE.into();
        };
        if !snapshot.failures.is_empty() {
            return snapshot
                .failures
                .iter()
                .map(|(provider, error)| format!("{provider}: {error}"))
                .collect::<Vec<_>>()
                .join("\n");
        }
        match self.view {
            SandboxView::Instances => match &snapshot.instances {
                SnapshotState::Loading => "Loading instance snapshot…".into(),
                SnapshotState::Unavailable(reason) => format!("Instances unavailable: {reason}"),
                SnapshotState::Ready(_) => "No matching instances.".into(),
            },
            SandboxView::Images => {
                if snapshot.providers.is_empty() {
                    return UNAVAILABLE.into();
                }
                snapshot
                    .providers
                    .iter()
                    .map(|(name, provider)| {
                        format!(
                            "{name}: {}",
                            match &provider.catalog {
                                SnapshotState::Loading => "Loading catalog…".into(),
                                SnapshotState::Unavailable(reason) =>
                                    format!("Unavailable: {reason}"),
                                SnapshotState::Ready(_) => "No matching images.".into(),
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            _ => "Select or create a configuration record.".into(),
        }
    }

    fn read_only_detail(&self) -> String {
        let entries = self.entries();
        let selected = entries.get(self.selected);
        if let Some(snapshot) = &self.snapshot {
            if self.view == SandboxView::Instances
                && let SnapshotState::Ready(rows) = &snapshot.instances
                && let Some(instance) = rows.iter().find(|row| Some(&row.id) == selected)
            {
                let effective = instance.effective.as_ref().map(|launch| {
                    let config = launch.configuration();
                    let profile = config.profile.value();
                    let drift = [(RecordKind::Profile, &config.profile_name, config.profile.revision()), (RecordKind::Provider, &profile.provider, config.provider.revision()), (RecordKind::Network, &profile.network, config.network.revision()), (RecordKind::Transfer, &profile.transfer, config.transfer.revision())].into_iter().any(|(kind, name, revision)| self.baseline.as_ref().and_then(|baseline| baseline.saved().record(kind, name).ok()).is_none_or(|saved| saved.revision() != revision));
                    format!("Immutable launch snapshot (current VM/network are in the live descriptor):\nProfile {} @ {}\nSaved defaults drift: {}\n{}\n", config.profile_name, config.profile.revision().as_str(), drift, serde_json::to_string_pretty(config).unwrap_or_default())
                }).unwrap_or_else(|| "Effective configuration unavailable.".into());
                return format!(
                    "Instance {}\nProvider {}\nVM: {:?} ({})\n{}Workcell: {:?} (Ready requires this runtime's authenticated connection, not just Running)\nLease: {}\nDisk retention: {}\nBlockers / in use: {}\nPending / ownership / recovery: {}\nLive effective:\n{}\n{effective}",
                    instance.id,
                    instance.provider,
                    instance.state,
                    if instance.live.is_some() {
                        "live"
                    } else {
                        "last known; unavailable"
                    },
                    match instance.state {
                        SandboxInstanceState::Paused => PAUSED_DISK,
                        SandboxInstanceState::Deleted => DELETED_DISK,
                        _ => "",
                    },
                    instance.workcell,
                    match (&instance.lease_deadline, &instance.state) {
                        (Some(deadline), _) => deadline.as_str(),
                        (None, SandboxInstanceState::Running) => NO_EXPIRY_DEADLINE,
                        (None, _) => UNAVAILABLE_DEADLINE,
                    },
                    instance
                        .retention_deadline
                        .as_deref()
                        .unwrap_or(UNAVAILABLE_DEADLINE),
                    instance.blockers.join(", "),
                    instance.record.as_ref().map(|record| serde_json::to_string_pretty(&serde_json::json!({"ownership":record.ownership,"detached":record.detached,"create":record.create,"lifecycle":record.lifecycle})).unwrap_or_default()).unwrap_or_else(|| "untracked; Attach borrows by default".into()),
                    serde_json::to_string_pretty(&instance.live).unwrap_or_default(),
                );
            }
            if self.view == SandboxView::Images {
                for (name, provider) in &snapshot.providers {
                    if let SnapshotState::Ready(catalog) = &provider.catalog {
                        for entry in catalog.entries() {
                            let label =
                                format!("{name}: {} @ {}", entry.id, entry.revision.as_str());
                            if Some(&label) == selected {
                                return format!(
                                    "Image (daemon-owned, immutable)\n{}\n{}\n{}\nIn use: {}\nImport / Build / GC / Local inspect require an explicitly approved OFFLINE local helper. No host paths are sent over HTTP; Caudra never stops the daemon.",
                                    label,
                                    serde_json::to_string_pretty(entry).unwrap_or_default(),
                                    provider
                                        .doctor
                                        .as_ref()
                                        .and_then(|doctor| doctor
                                            .templates
                                            .iter()
                                            .find(|template| template.manifest.id == entry.id
                                                && template.revision == entry.revision))
                                        .and_then(
                                            |template| serde_json::to_string_pretty(template).ok()
                                        )
                                        .unwrap_or_default(),
                                    match &snapshot.instances {
                                        SnapshotState::Ready(rows) => rows
                                            .iter()
                                            .filter(|row| &row.provider == name
                                                && row.live.as_ref().is_some_and(
                                                    |instance| instance.template.revision
                                                        == entry.revision.as_str()
                                                ))
                                            .map(|row| row.id.as_str())
                                            .collect::<Vec<_>>()
                                            .join(", "),
                                        _ => "unknown (GC helper enforces live references)".into(),
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
        format!("{}\n\n{FUTURE_ONLY}", self.snapshot_status())
    }

    fn view_confirmation(&mut self, frame: &mut Frame, area: Rect) {
        let theme = theme::current();
        self.hits.clear();
        let (message, choices, selected): (String, &[&str], usize) = match &self.confirmation {
            Some(Confirmation::Dirty { choice, .. }) => ("Unsaved configuration. Save validates and publishes; Discard restores the baseline; Keep editing preserves all text.".into(), &["Keep editing", "Save", "Discard"], *choice),
            Some(Confirmation::DeleteProfile { name, choice }) => (format!("Delete saved profile {name}? This stages only configuration deletion, never instance or disk deletion. Ctrl+S publishes it."), &["Keep profile", "Confirm deletion"], *choice),
            Some(Confirmation::Live { preview, choice, .. }) => (safe(preview), &["Keep draft (no action)", "Accept reviewed action"], *choice),
            Some(Confirmation::NetworkSave { preview, choice, .. }) => (safe(preview), &["Keep editing (no save)", "Save and reconcile owned instances"], *choice),
            None => return,
        };
        let [message_area, buttons] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(choices.len() as u16)])
                .areas(area);
        self.readers[ReadSurface::Body as usize].view(frame, message_area, safe(&message));
        if self.detail_scroll != 0 {
            self.readers[ReadSurface::Body as usize]
                .editor
                .scroll(-i32::from(self.detail_scroll));
            self.readers[ReadSurface::Body as usize]
                .editor
                .view_json(frame, message_area);
            self.detail_scroll = 0;
        }
        for (index, label) in choices.iter().take(buttons.height as usize).enumerate() {
            let area = Rect {
                y: buttons.y + index as u16,
                height: 1,
                ..buttons
            };
            let style = if index == selected {
                theme.keybind_key.add_modifier(Modifier::REVERSED)
            } else {
                theme.tool_dim
            };
            frame.render_widget(
                Paragraph::new(*label).style(hover_style(
                    style,
                    self.hovered == Some(Control::Confirm(index)),
                )),
                area,
            );
            self.hits.push((area, Control::Confirm(index)));
        }
    }
}

pub(super) fn hover_style(style: Style, hovered: bool) -> Style {
    let style = crate::components::hover_style(style, hovered);
    if hovered {
        style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        style
    }
}

fn safe(text: &str) -> String {
    text.split('\n')
        .map(escape_terminal_controls)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{
        Confirmation, Control, DELETED_DISK, Focus, NO_EXPIRY_DEADLINE, PAUSED_DISK, ReadSurface,
        SandboxView, UNAVAILABLE_DEADLINE,
    };
    use crate::components::{
        Overlay, buffer_text,
        sandbox_manager::{
            Navigation, ReferenceChoice, ReferencePicker, SandboxAction, StoreTicket,
            image::HostPicker,
            live::Kind,
            tests::{fixture, live_instance, press},
        },
        text_editor::{EditorKey, TextEditor},
    };
    use crate::sandbox::{SandboxInstanceState, SnapshotState};
    use caudra_sandbox::dto::InstanceState;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    use test_case::test_case;

    const WIDTH: u16 = 120;
    const HEIGHT: u16 = 36;
    const DETAIL_TAIL: &str = "unchanged.";
    const CONFIRMATION_TAIL: &str = "text.";
    const RETENTION_DEADLINE: &str = "2026-09-22T12:00:00Z";
    const LEASE_DEADLINE: &str = "2026-09-20T13:00:00Z";

    #[test_case(SandboxInstanceState::Running, None, NO_EXPIRY_DEADLINE; "running_without_deadline_never_expires")]
    #[test_case(SandboxInstanceState::Running, Some(LEASE_DEADLINE), LEASE_DEADLINE; "running_with_deadline")]
    #[test_case(SandboxInstanceState::Paused, None, UNAVAILABLE_DEADLINE; "paused_has_no_lease")]
    fn instance_details_show_the_running_lease(
        instance_state: SandboxInstanceState,
        deadline: Option<&str>,
        shown: &str,
    ) {
        let (_directory, _store, mut manager) = fixture();
        live_instance(&mut manager, false);
        let state = manager.state.as_mut().unwrap();
        let SnapshotState::Ready(rows) = &mut state.snapshot.as_mut().unwrap().instances else {
            panic!("missing instance")
        };
        rows[0].state = instance_state;
        rows[0].lease_deadline = deadline.map(Into::into);
        assert!(
            state
                .read_only_detail()
                .contains(&format!("\nLease: {shown}\n"))
        );
    }

    #[test_case(InstanceState::Paused, SandboxInstanceState::Paused, PAUSED_DISK, DELETED_DISK; "paused_disk_retained")]
    #[test_case(InstanceState::Deleted, SandboxInstanceState::Deleted, DELETED_DISK, PAUSED_DISK; "deleted_is_not_stopped")]
    fn instance_details_distinguish_retained_paused_disk_from_deleted(
        remote_state: InstanceState,
        expected: SandboxInstanceState,
        summary: &str,
        absent: &str,
    ) {
        let (_directory, _store, mut manager) = fixture();
        live_instance(&mut manager, false);
        let state = manager.state.as_mut().unwrap();
        let SnapshotState::Ready(rows) = &mut state.snapshot.as_mut().unwrap().instances else {
            panic!("missing instance")
        };
        let target = &mut rows[0];
        target.state = SandboxInstanceState::from(&remote_state);
        assert_eq!(target.state, expected);
        let live = target.live.as_mut().unwrap();
        live.state = remote_state;
        live.retention.deadline = Some(RETENTION_DEADLINE.into());
        target.retention_deadline = live.retention.deadline.clone();
        target.record.as_mut().unwrap().instance = target.live.clone();
        let detail = state.read_only_detail();
        assert!(detail.contains(&format!("VM: {expected:?} (live)")));
        assert!(detail.contains(summary));
        assert!(!detail.contains(absent));
        assert!(detail.contains(&format!("Disk retention: {RETENTION_DEADLINE}")));
    }

    #[test_case(120, 30; "wide")]
    #[test_case(44, 18; "narrow_scrollable")]
    fn instance_picker_hover_disabled_click_and_boundaries(width: u16, height: u16) {
        use crate::components::sandbox_manager::{live::INSTANCE_ACTIONS, tests::live_instance};
        let (_directory, _store, mut manager) = fixture();
        live_instance(&mut manager, false);
        press(&mut manager, KeyCode::F(3));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let resume = INSTANCE_ACTIONS
            .iter()
            .position(|(_, kind)| *kind == Some(Kind::Resume))
            .unwrap();
        let area = manager
            .state
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find(|(_, control)| *control == Control::InstanceAction(resume))
            .unwrap()
            .0;
        let event = |kind| MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            manager.handle_mouse(event(MouseEventKind::Moved)),
            SandboxAction::None
        ));
        assert!(manager.state.as_ref().unwrap().hovered == Some(Control::InstanceAction(resume)));
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        assert!(
            manager.state.as_ref().unwrap().readers[ReadSurface::Help as usize]
                .editor
                .text()
                .contains("Resume requires a paused instance")
        );
        manager.handle_mouse(event(MouseEventKind::Down(MouseButton::Left)));
        assert!(matches!(
            manager.handle_mouse(event(MouseEventKind::Up(MouseButton::Left))),
            SandboxAction::None
        ));
        assert!(manager.state.as_ref().unwrap().live_form.is_none());
        assert!(!manager.pending());
        let help = &manager.state.as_ref().unwrap().readers[ReadSurface::Help as usize];
        let expected = help.editor.text();
        let help_area = help.area;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            manager.handle_mouse(MouseEvent {
                kind,
                column: help_area.x,
                row: help_area.y,
                modifiers: KeyModifiers::NONE,
            });
        }
        manager.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        let SandboxAction::Copy(copied) =
            manager.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        else {
            panic!("disabled reason selection not copied")
        };
        assert_eq!(copied, expected);
        press(&mut manager, KeyCode::Tab);
        let state = manager.state.as_ref().unwrap();
        let selected = state.instance_action;
        let rows = state.references_area;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            manager.handle_mouse(MouseEvent {
                kind,
                column: rows.right() - 1,
                row: rows.bottom() - 1,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert_eq!(manager.state.as_ref().unwrap().instance_action, selected);
        assert!(manager.state.as_ref().unwrap().live_form.is_none());
        assert!(!manager.pending());
        press(&mut manager, KeyCode::End);
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        assert_eq!(
            manager.state.as_ref().unwrap().instance_action,
            Some(INSTANCE_ACTIONS.len() - 1)
        );
        assert!(manager.state.as_ref().unwrap().hits.iter().any(|(_, control)| *control == Control::InstanceAction(INSTANCE_ACTIONS.len() - 1)));
        press(&mut manager, KeyCode::Home);
        press(&mut manager, KeyCode::Enter);
        assert!(manager.state.as_ref().unwrap().instance_action.is_none());
        assert!(manager.state.as_ref().unwrap().confirmation.is_none());
        assert!(!manager.pending());
    }

    #[test_case(false; "select_all")]
    #[test_case(true; "shift_arrow")]
    fn search_keyboard_selection_is_not_invalidated_by_inspection(shift: bool) {
        const QUERY: &str = "dev";
        let (_directory, _store, mut manager) = fixture();
        let state = manager.state.as_mut().unwrap();
        state.focus = Focus::Search;
        state.search.set_text(QUERY.into());
        let select = if shift {
            KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT)
        } else {
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)
        };
        manager.handle_key(select);
        let SandboxAction::Copy(text) =
            manager.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        else {
            panic!("search selection lost");
        };
        assert_eq!(text, if shift { "d" } else { QUERY });
        assert_eq!(manager.state.as_ref().unwrap().search.text(), QUERY);
    }

    #[test]
    fn search_drag_release_outside_list_copies_without_navigation() {
        const QUERY: &str = "dev";
        let (_directory, _store, mut manager) = fixture();
        let state = manager.state.as_mut().unwrap();
        state.focus = Focus::Search;
        state.search.set_text(QUERY.into());
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let state = manager.state.as_ref().unwrap();
        let search = state
            .hits
            .iter()
            .find(|(_, control)| *control == Control::Search)
            .unwrap()
            .0;
        let target = state
            .hits
            .iter()
            .find(|(_, control)| *control == Control::View(SandboxView::Images))
            .unwrap()
            .0;
        manager.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: search.x,
            row: search.y,
            modifiers: KeyModifiers::NONE,
        });
        manager.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: target.x,
            row: target.y,
            modifiers: KeyModifiers::NONE,
        });
        let SandboxAction::Copy(text) = manager.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: target.x,
            row: target.y,
            modifiers: KeyModifiers::NONE,
        }) else {
            panic!("search release lost outside list");
        };
        assert_eq!(text, QUERY);
        assert_eq!(manager.state.as_ref().unwrap().view, SandboxView::Profiles);
        assert!(!manager.state.as_ref().unwrap().search_capture);
    }

    #[test_case(0, false; "list_cancel")]
    #[test_case(1, false; "fields_cancel")]
    #[test_case(2, false; "references_cancel")]
    #[test_case(0, true; "list_hidden")]
    #[test_case(1, true; "fields_hidden")]
    #[test_case(2, true; "references_hidden")]
    fn scrollbar_grab_cannot_survive_hidden_release_and_return(index: usize, hidden: bool) {
        use crate::components::scrollbar::ScrollbarMouse;
        const AREA: Rect = Rect::new(2, 3, 20, 5);
        let (_directory, _store, mut manager) = fixture();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let state = manager.state.as_mut().unwrap();
        let bar = match index {
            0 => &mut state.list_bar,
            1 => &mut state.fields_bar,
            _ => &mut state.references_bar,
        };
        terminal
            .draw(|frame| bar.draw(frame, AREA, 100_u32, 0_u32))
            .unwrap();
        assert!(!matches!(
            bar.handle(&MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: AREA.right() - 1,
                row: AREA.y,
                modifiers: KeyModifiers::NONE
            }),
            ScrollbarMouse::Ignored
        ));
        if hidden {
            state.export_preview();
            terminal
                .draw(|frame| {
                    manager.view(frame, frame.area());
                })
                .unwrap();
        } else {
            state.go(Navigation::View(SandboxView::Images));
            state.reset_mouse();
        }
        manager.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: AREA.x,
            row: AREA.y,
            modifiers: KeyModifiers::NONE,
        });
        let state = manager.state.as_mut().unwrap();
        let bar = match index {
            0 => &mut state.list_bar,
            1 => &mut state.fields_bar,
            _ => &mut state.references_bar,
        };
        terminal
            .draw(|frame| bar.draw(frame, AREA, 100_u32, 0_u32))
            .unwrap();
        assert!(matches!(
            bar.handle(&MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: AREA.right() - 1,
                row: AREA.bottom() - 1,
                modifiers: KeyModifiers::NONE
            }),
            ScrollbarMouse::Ignored
        ));
    }

    #[test_case("status"; "status")]
    #[test_case("help"; "saved_field_help")]
    #[test_case("pending"; "pending_operation")]
    #[test_case("fieldless"; "fieldless_live_instructions")]
    fn clipped_read_only_panes_scroll_to_end_and_copy_complete_source(surface: &str) {
        const SOURCE: &str = "界e\u{301}🙂 source line with wrapped words\n";
        const TAIL: &str = "END OF SOURCE";
        let (_directory, _store, mut manager) = fixture();
        let text = format!("{}{TAIL}", SOURCE.repeat(20));
        let scope = manager
            .snapshot_request(manager.state.as_ref().unwrap().conversation)
            .unwrap();
        let state = manager.state.as_mut().unwrap();
        let reader = match surface {
            "status" => {
                state.status = text.clone();
                ReadSurface::Status
            }
            "help" => {
                state.detail = true;
                state.form.as_mut().unwrap().fields[0].error = Some(text.clone());
                ReadSurface::Help
            }
            "pending" => {
                state.status = text.clone();
                state.live_pending = Some((
                    StoreTicket {
                        session: state.session,
                        operation: state.operation,
                        draft_revision: state.revision,
                    },
                    scope,
                ));
                ReadSurface::Body
            }
            _ => {
                state.open_live(Kind::Doctor);
                state.live_form.as_mut().unwrap().fields.clear();
                ReadSurface::Body
            }
        };
        let mut terminal = Terminal::new(TestBackend::new(24, 18)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let area = manager.state.as_ref().unwrap().readers[reader as usize].area;
        assert!(!area.is_empty());
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            manager.handle_mouse(MouseEvent {
                kind,
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            });
        }
        press(&mut manager, KeyCode::End);
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        assert!(
            buffer_text(terminal.backend().buffer()).contains(if surface == "fieldless" {
                "draft."
            } else {
                TAIL
            })
        );
        manager.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        let SandboxAction::Copy(copied) =
            manager.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        else {
            panic!("read-only source not copied");
        };
        if surface == "fieldless" {
            assert!(copied.starts_with("Ctrl+Enter reviews this action."));
            assert!(copied.ends_with("keeps the action draft; F6 discards it."));
        } else {
            assert_eq!(copied, text);
        }
    }

    #[test]
    fn host_picker_path_copy_is_forwarded_without_closing_or_selecting() {
        let (directory, _store, mut manager) = fixture();
        let path = directory.path().to_string_lossy();
        let state = manager.state.as_mut().unwrap();
        state.open_live(Kind::ImportImage);
        state.live_form.as_mut().unwrap().picker = Some(HostPicker::open(&path).unwrap());
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let (row, line) = terminal
            .backend()
            .buffer()
            .content()
            .chunks(WIDTH as usize)
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
            .enumerate()
            .rfind(|(_, line)| line.contains(path.as_ref()))
            .unwrap();
        let column = line[..line.find(path.as_ref()).unwrap()].chars().count() as u16;
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), column),
            (MouseEventKind::Drag(MouseButton::Left), column + 4),
        ] {
            manager.handle_mouse(MouseEvent {
                kind,
                column,
                row: row as u16,
                modifiers: KeyModifiers::NONE,
            });
        }
        let SandboxAction::Copy(text) = manager.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: column + 4,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        }) else {
            panic!("picker copy was not forwarded");
        };
        assert_eq!(text, &path[..4]);
        let picker = manager
            .state
            .as_ref()
            .unwrap()
            .live_form
            .as_ref()
            .unwrap()
            .picker
            .as_ref()
            .unwrap();
        assert!(picker.copy.is_none());
        assert!(!manager.pending());
    }

    #[test_case("resize"; "resize")]
    #[test_case("navigate"; "navigate")]
    #[test_case("close"; "close")]
    #[test_case("source"; "source_swap")]
    fn read_only_selection_survives_scroll_but_not_lifecycle(change: &str) {
        const SOURCE: &str = "界e\u{301}🙂 offscreen source line\n";
        let (_directory, _store, mut manager) = fixture();
        manager.state.as_mut().unwrap().status = SOURCE.repeat(30);
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let area = manager.state.as_ref().unwrap().readers[ReadSurface::Status as usize].area;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            manager.handle_mouse(MouseEvent {
                kind,
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            });
        }
        manager.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        manager.scroll_at(area.as_position(), -10);
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let SandboxAction::Copy(text) =
            manager.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        else {
            panic!("scroll lost selection");
        };
        assert_eq!(text, SOURCE.repeat(30));
        match change {
            "resize" => {
                terminal
                    .draw(|frame| {
                        manager.view(frame, Rect::new(0, 0, WIDTH - 1, HEIGHT));
                    })
                    .unwrap();
            }
            "navigate" => {
                manager
                    .state
                    .as_mut()
                    .unwrap()
                    .go(Navigation::View(SandboxView::Images));
            }
            "close" => manager.close(),
            _ => manager.receive_network_report("new source".into()),
        }
        let state = manager.state.as_mut().unwrap();
        assert!(state.reader_capture.is_none());
        assert!(!state.editor_capture);
        assert!(matches!(
            state.readers[ReadSurface::Status as usize]
                .editor
                .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            EditorKey::Passthrough
        ));
    }

    #[test]
    fn masked_scrollbar_capture_crosses_bounds_without_selection_or_action() {
        const SECRET: &str = "masked-scroll-test";
        let (_directory, _store, mut manager) = fixture();
        let state = manager.state.as_mut().unwrap();
        state.open_live(Kind::Credential);
        let form = state.live_form.as_mut().unwrap();
        form.focus = form.fields.iter().position(|field| field.secret()).unwrap();
        let source = SECRET.repeat(100);
        form.fields[form.focus].editor.set_text(source.clone());
        let mut terminal = Terminal::new(TestBackend::new(48, 18)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let before = terminal.backend().buffer().clone();
        let area = manager.state.as_ref().unwrap().editor_area;
        for (kind, column, row) in [
            (
                MouseEventKind::Down(MouseButton::Left),
                area.right() - 1,
                area.y,
            ),
            (
                MouseEventKind::Drag(MouseButton::Left),
                area.right() + 2,
                area.bottom() + 2,
            ),
            (
                MouseEventKind::Up(MouseButton::Left),
                area.right() + 2,
                area.bottom() + 2,
            ),
        ] {
            assert!(matches!(
                manager.handle_mouse(MouseEvent {
                    kind,
                    column,
                    row,
                    modifiers: KeyModifiers::NONE
                }),
                SandboxAction::None
            ));
        }
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        assert_ne!(&before, terminal.backend().buffer());
        assert!(!manager.pending());
        let form = manager.state.as_mut().unwrap().live_form.as_mut().unwrap();
        let editor = &mut form.fields[form.focus].editor;
        assert_eq!(editor.text(), source);
        assert!(matches!(
            editor.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            EditorKey::Passthrough
        ));
    }

    #[test]
    fn unicode_drag_uses_display_columns_and_read_only_focus_rejects_edits() {
        const PREFIX: &str = "α界e\u{301}🙂";
        const TEXT: &str = "α界e\u{301}🙂 source";
        let (_directory, _store, mut manager) = fixture();
        manager.state.as_mut().unwrap().status = TEXT.into();
        let mut terminal = Terminal::new(TestBackend::new(48, 18)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let area = manager.state.as_ref().unwrap().readers[ReadSurface::Status as usize].area;
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), area.x),
            (MouseEventKind::Drag(MouseButton::Left), area.x + 6),
        ] {
            manager.handle_mouse(MouseEvent {
                kind,
                column,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            });
        }
        let SandboxAction::Copy(text) = manager.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 6,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }) else {
            panic!("drag was not copied");
        };
        assert_eq!(text, PREFIX);
        let before = manager
            .state
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .fields[0]
            .text();
        manager.handle_paste("must not be inserted");
        press(&mut manager, KeyCode::Char('x'));
        assert_eq!(
            manager
                .state
                .as_ref()
                .unwrap()
                .form
                .as_ref()
                .unwrap()
                .fields[0]
                .text(),
            before
        );
        assert_eq!(manager.state.as_ref().unwrap().status, TEXT);
    }

    #[test_case(false; "status")]
    #[test_case(true; "recovery_report")]
    fn wrapped_unicode_selection_copies_source_and_refresh_invalidates_it(report: bool) {
        const TEXT: &str = "α界e\u{301}🙂 wrapped source\nsecond line";
        const UPDATED: &str = "replacement status";
        let (_directory, _store, mut manager) = fixture();
        let state = manager.state.as_mut().unwrap();
        let surface = if report {
            state.form = None;
            state.view = SandboxView::Instances;
            state.detail = true;
            state.network_report = Some(TEXT.repeat(12));
            ReadSurface::Body
        } else {
            state.status = TEXT.repeat(12);
            ReadSurface::Status
        };
        let mut terminal = Terminal::new(TestBackend::new(48, 18)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let area = manager.state.as_ref().unwrap().readers[surface as usize].area;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            manager.handle_mouse(MouseEvent {
                kind,
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            });
        }
        manager.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        let SandboxAction::Copy(text) =
            manager.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        else {
            panic!("selection was not copied");
        };
        assert!(text.contains(&TEXT.repeat(12)));
        assert!(!manager.pending());
        let state = manager.state.as_mut().unwrap();
        if report {
            state.network_report = Some(UPDATED.into());
        } else {
            state.status = UPDATED.into();
        }
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        assert!(matches!(
            manager.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            SandboxAction::None
        ));
    }

    #[test]
    fn confirmation_drag_across_action_copies_without_accepting() {
        let (_directory, _store, mut manager) = fixture();
        manager.state.as_mut().unwrap().confirmation = Some(Confirmation::Dirty {
            after: Navigation::Back,
            choice: 0,
        });
        let mut terminal = Terminal::new(TestBackend::new(48, 18)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let state = manager.state.as_ref().unwrap();
        let body = state.readers[ReadSurface::Body as usize].area;
        let button = state
            .hits
            .iter()
            .find(|(_, control)| *control == Control::Confirm(1))
            .unwrap()
            .0;
        for (kind, area) in [
            (MouseEventKind::Down(MouseButton::Left), body),
            (MouseEventKind::Drag(MouseButton::Left), button),
        ] {
            assert!(matches!(
                manager.handle_mouse(MouseEvent {
                    kind,
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE
                }),
                SandboxAction::None
            ));
        }
        assert!(matches!(
            manager.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: button.x,
                row: button.y,
                modifiers: KeyModifiers::NONE
            }),
            SandboxAction::Copy(_)
        ));
        assert!(manager.state.as_ref().unwrap().confirmation.is_some());
        assert!(!manager.pending());
    }

    #[test]
    fn reference_scrollbar_scrolls_without_applying_or_selecting_a_record() {
        let (_directory, _store, mut manager) = fixture();
        manager.state.as_mut().unwrap().references = Some(ReferencePicker {
            choices: (0..100)
                .map(|index| ReferenceChoice {
                    label: format!("record {index}"),
                    fields: Vec::new(),
                })
                .collect(),
            search: TextEditor::new(),
            selected: 0,
        });
        let mut terminal = Terminal::new(TestBackend::new(48, 18)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let area = manager.state.as_ref().unwrap().references_area;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            assert!(matches!(
                manager.handle_mouse(MouseEvent {
                    kind,
                    column: area.right() - 1,
                    row: area.bottom() - 1,
                    modifiers: KeyModifiers::NONE
                }),
                SandboxAction::None
            ));
        }
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let state = manager.state.as_ref().unwrap();
        assert!(state.reference_scroll > 0);
        assert_eq!(state.references.as_ref().unwrap().selected, 0);
        assert!(!manager.pending());
    }

    #[test]
    fn saved_field_json_uses_the_shared_key_color() {
        const VALUE: &str = "{\"key\": true}";
        let (_directory, _store, mut manager) = fixture();
        let field = &mut manager
            .state
            .as_mut()
            .unwrap()
            .form
            .as_mut()
            .unwrap()
            .fields[1];
        field.editor.set_text(VALUE.into());
        let key_column = format!("{}: {{\"", field.label).chars().count() as u16;
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let area = manager
            .state
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find(|(_, control)| *control == Control::Field(1))
            .unwrap()
            .0;
        let cell = &terminal.backend().buffer()[(area.x + key_column, area.y)];
        assert_eq!(cell.symbol(), "k");
        assert_eq!(Some(cell.fg), crate::theme::current().accent.fg);
    }

    #[test_case(Control::View(SandboxView::Images), false; "tab")]
    #[test_case(Control::Search, false; "search")]
    #[test_case(Control::Row(0), false; "navigation_row")]
    #[test_case(Control::Field(1), false; "field")]
    #[test_case(Control::Reference(1), false; "reference")]
    #[test_case(Control::Confirm(1), false; "confirmation_action")]
    #[test_case(Control::View(SandboxView::Profiles), true; "selected_tab")]
    #[test_case(Control::Row(0), true; "selected_navigation_row")]
    #[test_case(Control::Field(1), true; "selected_field")]
    #[test_case(Control::Reference(1), true; "selected_reference")]
    #[test_case(Control::Confirm(1), true; "selected_confirmation_action")]
    fn hover_changes_rendered_hitbox(control: Control, selected: bool) {
        let (_directory, _store, mut manager) = fixture();
        let state = manager.state.as_mut().unwrap();
        if control == Control::Row(0) {
            state.selected = usize::from(!selected);
            state.reveal_selected = false;
        }
        if control == Control::Reference(1) {
            state.references = Some(ReferencePicker {
                choices: ["first", "second"]
                    .into_iter()
                    .map(|label| ReferenceChoice {
                        label: label.into(),
                        fields: Vec::new(),
                    })
                    .collect(),
                search: TextEditor::new(),
                selected: usize::from(selected),
            });
        }
        if control == Control::Confirm(1) {
            state.confirmation = Some(Confirmation::Dirty {
                after: Navigation::Back,
                choice: usize::from(selected),
            });
        }
        if control == Control::Field(1) && selected {
            state.focus = Focus::Detail;
            state.form.as_mut().unwrap().focus = 1;
        }
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let area = manager
            .state
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find(|(_, hit)| *hit == control)
            .unwrap()
            .0;
        let before = terminal.backend().buffer()[(area.x, area.y)].clone();
        manager.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        });
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let after = &terminal.backend().buffer()[(area.x, area.y)];
        assert_eq!(before.symbol(), after.symbol());
        assert_ne!(before.style(), after.style());
    }

    #[test_case("document"; "document_editor")]
    #[test_case("destination"; "export_path_editor")]
    #[test_case("reference"; "reference_filter")]
    #[test_case("field"; "saved_field_editor")]
    #[test_case("live"; "live_editor")]
    #[test_case("secret"; "masked_live_editor")]
    fn editor_body_hover_changes_border_without_editing_or_leaking(surface: &str) {
        const TEXT: &str = "private-editor-value";
        let (_directory, _store, mut manager) = fixture();
        let state = manager.state.as_mut().unwrap();
        match surface {
            "document" | "destination" => {
                state.export_preview();
                let document = state.document.as_mut().unwrap();
                document.editor.set_text(TEXT.into());
                if surface == "destination" {
                    let mut editor = TextEditor::new();
                    editor.set_text(TEXT.into());
                    document.destination = Some(editor);
                }
            }
            "reference" => {
                let mut search = TextEditor::new();
                search.set_text(TEXT.into());
                state.references = Some(ReferencePicker {
                    choices: Vec::new(),
                    search,
                    selected: 0,
                });
            }
            "field" => {
                state.focus = Focus::Detail;
                let form = state.form.as_mut().unwrap();
                form.focus = 1;
                form.editing = true;
                form.fields[1].editor.set_text(TEXT.into());
            }
            _ => {
                state.open_live(Kind::Credential);
                let form = state.live_form.as_mut().unwrap();
                form.focus = form
                    .fields
                    .iter()
                    .position(|field| field.secret() == (surface == "secret"))
                    .unwrap();
                form.fields[form.focus].editor.set_text(TEXT.into());
            }
        }
        let revision = state.revision;
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let area = manager.state.as_ref().unwrap().editor_area;
        assert!(!area.is_empty());
        let border = (area.x - 1, area.y);
        let before = terminal.backend().buffer()[border].clone();
        let content = buffer_text(terminal.backend().buffer());
        let mouse = |kind, column| MouseEvent {
            kind,
            column,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            manager.handle_mouse(mouse(MouseEventKind::Moved, area.x)),
            SandboxAction::None
        ));
        assert!(manager.state.as_ref().unwrap().hovered == Some(Control::Editor));
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        assert_eq!(content, buffer_text(terminal.backend().buffer()));
        assert_ne!(before.style(), terminal.backend().buffer()[border].style());
        assert_eq!(manager.state.as_ref().unwrap().revision, revision);
        if surface == "secret" {
            assert!(!content.contains(TEXT));
            assert!(content.contains(&"*".repeat(TEXT.len())));
        }
        manager.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), area.x));
        manager.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + TEXT.len() as u16,
        ));
        let copied = manager.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            area.x + TEXT.len() as u16,
        ));
        if surface == "secret" {
            assert!(matches!(copied, SandboxAction::None));
        } else {
            assert!(matches!(copied, SandboxAction::Copy(value) if value == TEXT));
        }
    }

    #[test_case(false; "inspection")]
    #[test_case(true; "confirmation")]
    fn end_renders_bottom_of_wrapped_content(confirmation: bool) {
        let (_directory, _store, mut manager) = fixture();
        let state = manager.state.as_mut().unwrap();
        state.form = None;
        state.focus = Focus::Detail;
        if confirmation {
            state.confirmation = Some(Confirmation::Dirty {
                after: Navigation::Back,
                choice: 0,
            });
        }
        press(&mut manager, KeyCode::End);
        let state = manager.state.as_mut().unwrap();
        assert_eq!(state.detail_scroll, u16::MAX);
        let mut terminal = Terminal::new(TestBackend::new(32, 5)).unwrap();
        terminal
            .draw(|frame| {
                if confirmation {
                    state.view_confirmation(frame, frame.area());
                } else {
                    state.view_detail(frame, frame.area());
                }
            })
            .unwrap();
        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains(if confirmation {
            CONFIRMATION_TAIL
        } else {
            DETAIL_TAIL
        }));
        assert_eq!(state.detail_scroll, 0);
    }

    #[test]
    fn resized_render_clears_hover_and_press() {
        let (_directory, _store, mut manager) = fixture();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                manager.view(frame, frame.area());
            })
            .unwrap();
        let state = manager.state.as_mut().unwrap();
        state.hovered = Some(Control::Search);
        state.pressed = Some(Control::Search);
        terminal
            .draw(|frame| {
                manager.view(frame, Rect::new(0, 0, WIDTH - 1, HEIGHT));
            })
            .unwrap();
        let state = manager.state.as_ref().unwrap();
        assert!(state.hovered.is_none());
        assert!(state.pressed.is_none());
    }
}
