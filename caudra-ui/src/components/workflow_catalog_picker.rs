//! The `/workflows` picker: every workflow the session can launch, and the
//! trust step a project workflow needs before it may run.
//!
//! The catalog arrives from the runtime after the picker opens, so it starts
//! empty with a scanning notice and fills in on the reply. Trusting is a
//! two-key confirmation that quotes the digest the runtime pins, modelled on
//! the MCP picker's project trust.

use caudra_workflow::WorkflowCatalog;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;

use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::{Overlay, escape_terminal_controls, hint_line};
use crate::repaint::Cadence;

const TITLE: &str = " Workflows ";
const EMPTY_TEXT: &str = "No workflows found";
const SCANNING_TEXT: &str = "Scanning workflows...";
const UNTRUSTED_SUFFIX: &str = "untrusted";
const INVALID_PREFIX: &str = "Invalid workflow";
const TRUST_PROMPT_PREFIX: &str = "Trust ";
const TRUST_PROMPT_SUFFIX: &str = "? Press Enter/y to confirm or Esc to cancel.";
const WHEN_TO_USE_PREFIX: &str = "Use when: ";

#[must_use]
pub enum WorkflowCatalogAction {
    Consumed,
    /// A trusted workflow was chosen; the app pre-fills the composer with it.
    Launch(String),
    /// The user confirmed the exact digest shown.
    Trust {
        name: String,
        digest: String,
    },
    Close,
}

pub struct CatalogRow {
    name: String,
    detail_text: String,
    suffix: &'static str,
    when_to_use: Option<String>,
    digest: String,
    trusted: bool,
}

impl PickerItem for CatalogRow {
    fn label(&self) -> &str {
        &self.name
    }

    fn suffix(&self) -> Option<&str> {
        Some(self.suffix)
    }

    fn detail(&self) -> Option<&str> {
        (!self.detail_text.is_empty()).then_some(&self.detail_text)
    }

    fn is_highlighted(&self) -> bool {
        !self.trusted
    }
}

pub struct WorkflowCatalogPicker {
    picker: ListPicker<CatalogRow>,
    /// Kept so a click, which hands the row over and empties the list, can
    /// put the list back for the trust confirmation it may lead to.
    catalog: WorkflowCatalog,
    pending_trust: Option<(String, String)>,
}

impl WorkflowCatalogPicker {
    pub fn new() -> Self {
        let mut picker = ListPicker::new().with_footer_builder(footer);
        picker.set_empty_text(EMPTY_TEXT);
        Self {
            picker,
            catalog: WorkflowCatalog::default(),
            pending_trust: None,
        }
    }

    /// Opens empty and says so; [`Self::fill`] brings the rows.
    pub fn open(&mut self) {
        self.pending_trust = None;
        self.catalog = WorkflowCatalog::default();
        self.picker.set_error_text(None);
        self.picker.open(Vec::new(), TITLE);
        self.picker.set_info_text(Some(SCANNING_TEXT.into()));
    }

    pub fn fill(&mut self, catalog: WorkflowCatalog) {
        if !self.picker.is_open() {
            return;
        }
        let selected = self.selected_name();
        self.picker.replace_items(build_rows(&catalog));
        self.picker.set_error_text(invalid_text(&catalog));
        self.catalog = catalog;
        if let Some(name) = selected {
            self.picker.select_item_by(|row| row.name == name);
        }
        self.sync_info();
    }

    pub fn fail(&mut self, error: String) {
        if !self.picker.is_open() {
            return;
        }
        self.picker.set_info_text(None);
        self.picker.set_error_text(Some(error));
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
        self.pending_trust = None;
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> WorkflowCatalogAction {
        if let Some((name, digest)) = self.pending_trust.take() {
            return match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    WorkflowCatalogAction::Trust { name, digest }
                }
                KeyCode::Esc => {
                    self.sync_info();
                    WorkflowCatalogAction::Consumed
                }
                _ => {
                    self.pending_trust = Some((name, digest));
                    WorkflowCatalogAction::Consumed
                }
            };
        }
        if key.code == KeyCode::Enter {
            return self.choose();
        }
        let action = self.picker.handle_key(key);
        self.sync_info();
        self.map_action(action)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> WorkflowCatalogAction {
        if self.pending_trust.is_some() {
            return WorkflowCatalogAction::Consumed;
        }
        let action = self.picker.handle_mouse(event);
        self.sync_info();
        self.map_action(action)
    }

    /// A confirmation owns the whole picker until it is answered.
    pub fn scroll(&mut self, delta: i32) {
        if self.pending_trust.is_some() {
            return;
        }
        self.picker.scroll(delta);
        self.sync_info();
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }

    fn selected_name(&self) -> Option<String> {
        self.picker.selected_item().map(|row| row.name.clone())
    }

    /// Enter on a trusted row launches; on an untrusted one it asks for the
    /// digest to be confirmed first.
    fn choose(&mut self) -> WorkflowCatalogAction {
        let Some(row) = self.picker.selected_item() else {
            return WorkflowCatalogAction::Consumed;
        };
        if row.trusted {
            let name = row.name.clone();
            self.close();
            return WorkflowCatalogAction::Launch(name);
        }
        let name = row.name.clone();
        let digest = row.digest.clone();
        self.picker.set_info_text(Some(format!(
            "{TRUST_PROMPT_PREFIX}{name} (digest {digest}){TRUST_PROMPT_SUFFIX}"
        )));
        self.pending_trust = Some((name, digest));
        WorkflowCatalogAction::Consumed
    }

    fn map_action(&mut self, action: PickerAction<CatalogRow>) -> WorkflowCatalogAction {
        match action {
            PickerAction::Consumed | PickerAction::Toggle(..) => WorkflowCatalogAction::Consumed,
            PickerAction::Select(row) => {
                self.picker.open(build_rows(&self.catalog), TITLE);
                self.picker
                    .select_item_by(|candidate| candidate.name == row.name);
                self.choose()
            }
            PickerAction::Close => {
                self.close();
                WorkflowCatalogAction::Close
            }
        }
    }

    fn sync_info(&mut self) {
        let info = self
            .picker
            .selected_item()
            .and_then(|row| row.when_to_use.clone())
            .map(|text| format!("{WHEN_TO_USE_PREFIX}{text}"));
        self.picker.set_info_text(info);
    }
}

impl Overlay for WorkflowCatalogPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }
}

fn footer() -> Line<'static> {
    hint_line(&[("Enter", "Launch or trust"), ("Esc", "Close")])
}

fn build_rows(catalog: &WorkflowCatalog) -> Vec<CatalogRow> {
    catalog
        .entries
        .iter()
        .map(|entry| CatalogRow {
            name: entry.name.clone(),
            detail_text: escape_terminal_controls(&entry.description),
            suffix: if entry.trusted {
                entry.source_kind.as_str()
            } else {
                UNTRUSTED_SUFFIX
            },
            when_to_use: entry
                .when_to_use
                .as_deref()
                .map(escape_terminal_controls)
                .filter(|text| !text.is_empty()),
            digest: entry.digest.clone(),
            trusted: entry.trusted,
        })
        .collect()
}

fn invalid_text(catalog: &WorkflowCatalog) -> Option<String> {
    let first = catalog.invalid.first()?;
    let more = catalog.invalid.len() - 1;
    let path = first.path.display();
    let text = if more == 0 {
        format!("{INVALID_PREFIX} {path}: {}", first.error)
    } else {
        format!("{INVALID_PREFIX} {path}: {} (+{more} more)", first.error)
    };
    Some(escape_terminal_controls(&text))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use caudra_workflow::{CatalogEntry, InvalidEntry, SourceKind};
    use test_case::test_case;

    use super::*;
    use crate::components::key as key_event;

    const NAME: &str = "deep-research";
    const DIGEST: &str = "sha256:abc";
    const DESCRIPTION: &str = "Research a query";
    const NOT_CONFIRMED: &str = "a trust must not fire before its digest is confirmed";
    const LAUNCHED: &str = "a trusted row launches on Enter";
    const CONFIRMED: &str = "confirming must carry the exact digest the row showed";

    fn entry(trusted: bool, source_kind: SourceKind) -> CatalogEntry {
        CatalogEntry {
            name: NAME.into(),
            description: DESCRIPTION.into(),
            when_to_use: None,
            phases: Vec::new(),
            source_kind,
            path: Some(PathBuf::from("workflows/deep-research.rhai")),
            digest: DIGEST.into(),
            trusted,
            shadowed: Vec::new(),
        }
    }

    fn open_with(entries: Vec<CatalogEntry>) -> WorkflowCatalogPicker {
        let mut picker = WorkflowCatalogPicker::new();
        picker.open();
        picker.fill(WorkflowCatalog {
            entries,
            invalid: Vec::new(),
            ..WorkflowCatalog::default()
        });
        picker
    }

    #[test]
    fn enter_on_a_trusted_workflow_launches_it() {
        let mut picker = open_with(vec![entry(true, SourceKind::Project)]);
        match picker.handle_key(key_event(KeyCode::Enter)) {
            WorkflowCatalogAction::Launch(name) => assert_eq!(name, NAME),
            _ => panic!("{LAUNCHED}"),
        }
        assert!(!picker.is_open());
    }

    #[test_case(KeyCode::Enter ; "enter")]
    #[test_case(KeyCode::Char('y') ; "y")]
    fn an_untrusted_workflow_asks_before_trusting(confirm: KeyCode) {
        let mut picker = open_with(vec![entry(false, SourceKind::Project)]);
        assert!(
            matches!(
                picker.handle_key(key_event(KeyCode::Enter)),
                WorkflowCatalogAction::Consumed
            ),
            "{NOT_CONFIRMED}"
        );
        match picker.handle_key(key_event(confirm)) {
            WorkflowCatalogAction::Trust { name, digest } => {
                assert_eq!(
                    (name.as_str(), digest.as_str()),
                    (NAME, DIGEST),
                    "{CONFIRMED}"
                );
            }
            _ => panic!("{CONFIRMED}"),
        }
    }

    #[test]
    fn escape_withdraws_a_pending_trust() {
        let mut picker = open_with(vec![entry(false, SourceKind::Project)]);
        let _ = picker.handle_key(key_event(KeyCode::Enter));
        assert!(matches!(
            picker.handle_key(key_event(KeyCode::Esc)),
            WorkflowCatalogAction::Consumed
        ));
        assert!(picker.is_open());
        assert!(picker.pending_trust.is_none());
    }

    #[test]
    fn invalid_entries_are_reported_not_listed() {
        let mut picker = WorkflowCatalogPicker::new();
        picker.open();
        picker.fill(WorkflowCatalog {
            entries: Vec::new(),
            invalid: vec![InvalidEntry {
                path: PathBuf::from("bad.rhai"),
                source_kind: SourceKind::Project,
                error: "parse error".into(),
            }],
            ..WorkflowCatalog::default()
        });
        assert!(
            picker
                .picker
                .error_text()
                .is_some_and(|text| text.starts_with(INVALID_PREFIX))
        );
        assert!(picker.picker.selected_item().is_none());
    }
}
