use super::{
    Confirmation, Control, DocumentMode, FUTURE_ONLY, Focus, Manager, RecordKind, SandboxManager,
    SandboxName, SandboxView, SnapshotState, UNAVAILABLE, live::Kind,
};
use crate::components::{Hint, HintBar, Overlay, escape_terminal_controls, keybindings::key};
use crate::theme;
use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

const WIDE_WIDTH: u16 = 100;
const LIST_WIDTH: u16 = 30;
const STATUS_ROWS: u16 = 3;
const FIELD_EDITOR_ROWS: u16 = 7;
const FORM_HELP_ROWS: u16 = 4;
const FORM_SUMMARY_ROWS: u16 = 3;

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
            self.pressed = None;
            if self.focus == Focus::Detail
                && let Some(form) = self.form.as_mut()
            {
                form.reveal_focus = true;
            }
            if self.focus == Focus::List {
                self.reveal_selected = true;
            }
            for bar in &mut self.hints {
                bar.reset();
            }
        }
        self.area = area;
        self.hits.clear();
        self.editor_area = Rect::ZERO;
        self.list_area = Rect::ZERO;
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
                Paragraph::new(format!("{} {}", index + 1, view.label())).style(style),
                tab_areas[index],
            );
            self.hits.push((tab_areas[index], Control::View(view)));
        }
        if let Some(panel) = self.transfer.as_mut() {
            panel.view(frame, body);
        } else if self.confirmation.is_some() {
            self.view_confirmation(frame, body);
        } else if self.live_form.is_some() {
            self.view_live(frame, body);
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
                }
            };
            let block = Block::default().borders(Borders::ALL).title(title);
            self.editor_area = block.inner(body);
            frame.render_widget(block, body);
            document
                .destination
                .as_mut()
                .unwrap_or(&mut document.editor)
                .view(frame, self.editor_area);
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
        frame.render_widget(
            Paragraph::new(safe(&self.status))
                .style(theme.tool_dim)
                .wrap(Wrap { trim: false }),
            status,
        );
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
        if self.transfer.is_some() {
            return vec![
                vec![
                    Hint::char("s", "Seed"),
                    Hint::char("p", "Push"),
                    Hint::char("l", "Pull"),
                    Hint::char("r", "Review"),
                    Hint::char("x", "Execute"),
                ],
                vec![
                    Hint::char("c", "Compare"),
                    Hint::char("q", "Reconcile"),
                    Hint::key("Esc", KeyCode::Esc, "Cancel / close"),
                ],
            ];
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
                Confirmation::Live { .. } => vec![
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
                    Hint::key("F6", KeyCode::F(6), "Discard action draft"),
                ],
                vec![Hint::key("Esc", KeyCode::Esc, "Close / retain draft")],
            ];
        }
        if let Some(document) = &self.document {
            let action = if document.destination.is_some() {
                Hint::key("Enter", KeyCode::Enter, "Publish new file")
            } else {
                match document.mode {
                    DocumentMode::Import => Hint::bind(key::SANDBOX_APPLY, "Apply import to draft"),
                    DocumentMode::Export => Hint::bind(key::SAVE, "Save as"),
                    DocumentMode::Compare => Hint::inert("Ctrl+A/C", "Select / copy preview"),
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
                        Hint::char("a", "Attach"),
                        Hint::char("t", "Transfer"),
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
        } else {
            frame.render_widget(
                Paragraph::new(format!("/ Search: {}", safe(&self.search.text())))
                    .style(theme.tool_dim),
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
            frame.render_widget(
                Paragraph::new(safe(&empty)).wrap(Wrap { trim: false }),
                rows,
            );
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
            frame.render_widget(Paragraph::new(safe(entry)).style(style), area);
            self.hits.push((area, Control::Row(index)));
        }
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
            .title("Filter references · Enter applies to draft");
        let inner = block.inner(search);
        self.editor_area = inner;
        frame.render_widget(block, search);
        picker.search.view(frame, inner);
        let filtered = picker.filtered();
        if filtered.is_empty() {
            frame.render_widget(Paragraph::new("No matching references. Create a provider/policy first, or wait for authenticated catalog metadata.").wrap(Wrap { trim: false }), rows);
        }
        let start = picker
            .selected
            .saturating_sub(rows.height.saturating_sub(1) as usize);
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
                Paragraph::new(safe(&picker.choices[*index].label)).style(style),
                area,
            );
            self.hits.push((area, Control::Reference(*index)));
        }
    }

    fn view_detail(&mut self, frame: &mut Frame, area: Rect) {
        if self.form.is_none() {
            let text = self.read_only_detail();
            frame.render_widget(
                Paragraph::new(safe(&text))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Inspect · no lifecycle actions"),
                    )
                    .wrap(Wrap { trim: false })
                    .scroll((self.detail_scroll, 0)),
                area,
            );
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
            Constraint::Length(FORM_SUMMARY_ROWS),
            Constraint::Min(1),
            Constraint::Length(FORM_HELP_ROWS),
            Constraint::Length(if form.editing { FIELD_EDITOR_ROWS } else { 0 }),
        ])
        .areas(inner);
        frame.render_widget(
            Paragraph::new(safe(&summary))
                .style(theme.tool_dim)
                .wrap(Wrap { trim: false }),
            summary_area,
        );
        if form.reveal_focus {
            form.scroll = form.scroll.min(form.focus);
            if form.focus >= form.scroll + rows.height as usize {
                form.scroll = form
                    .focus
                    .saturating_sub(rows.height.saturating_sub(1) as usize);
            }
            form.reveal_focus = false;
        }
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
            frame.render_widget(
                Paragraph::new(format!("{}: {} {marker}", field.label, safe(&value))).style(style),
                area,
            );
            self.hits.push((area, Control::Field(index)));
        }
        let field = &mut form.fields[form.focus];
        let guidance = field
            .error
            .as_deref()
            .or(field.locked.as_deref())
            .unwrap_or(field.help);
        frame.render_widget(
            Paragraph::new(safe(guidance))
                .wrap(Wrap { trim: false })
                .style(theme.tool_dim),
            help,
        );
        if form.editing {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!("{} · Apply field ≠ Save", field.label));
            self.editor_area = block.inner(editor);
            frame.render_widget(block, editor);
            field.editor.view(frame, self.editor_area);
        }
    }

    fn form_summary(&self) -> String {
        let Some(form) = &self.form else {
            return String::new();
        };
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
                    "Instance {}\nProvider {}\nVM: {:?} ({})\nWorkcell: {:?} (Ready requires this runtime's authenticated connection, not just Running)\nLease: {}\nDisk retention: {}\nBlockers / in use: {}\nPending / ownership / recovery: {}\nLive effective:\n{}\n{effective}",
                    instance.id,
                    instance.provider,
                    instance.state,
                    if instance.live.is_some() {
                        "live"
                    } else {
                        "last known; unavailable"
                    },
                    instance.workcell,
                    instance.lease_deadline.as_deref().unwrap_or("unavailable"),
                    instance
                        .retention_deadline
                        .as_deref()
                        .unwrap_or("unavailable"),
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
            None => return,
        };
        let [message_area, buttons] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(choices.len() as u16)])
                .areas(area);
        frame.render_widget(
            Paragraph::new(message)
                .wrap(Wrap { trim: false })
                .scroll((self.detail_scroll, 0)),
            message_area,
        );
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
            frame.render_widget(Paragraph::new(*label).style(style), area);
            self.hits.push((area, Control::Confirm(index)));
        }
    }
}

fn safe(text: &str) -> String {
    text.split('\n')
        .map(escape_terminal_controls)
        .collect::<Vec<_>>()
        .join("\n")
}
