use std::collections::BTreeMap;
use std::fs;
use std::mem;
use std::path::Path;

use caudra_grab::grab_scope;
use caudra_storage::id::CaudraId;
use caudra_storage::paths;
use caudra_storage::sessions::{SessionLocation, SessionRelocation};
use caudra_workbench::keys::{LIST_FIRST, LIST_LAST};
use caudra_workbench::text_field::{FieldKind, TextField, TextKey};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use super::{Hint, Overlay};
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::Cadence;

const MAX_VISIBLE: u16 = 12;
const WIDTH_PERCENT: u16 = 95;
const SOURCE_TITLE: &str = " Migrate directory sessions: source ";
const DESTINATION_TITLE: &str = " Session relocation: destination ";
const CUSTOM_TITLE: &str = " Session relocation: custom directory ";
const CONFIRM_TITLE: &str = " Confirm session relocation ";
const CUSTOM_DIRECTORY: &str = "Custom directory";
const CONFIRM: &str = "Confirm relocation";
const UNTITLED: &str = "(untitled)";
const EMPTY_SELECTION: &str = "No sessions match this source. Refresh the session inventory.";
const SAME_DIRECTORY: &str = "Already in that directory. Choose a different destination.";
const EMPTY_DESTINATION: &str = "Enter an existing destination directory.";
const MISSING_HOME: &str = "Cannot resolve ~: home directory is unavailable.";
const UNSUPPORTED_TILDE: &str = "Use ~ or ~/path; named-user expansion is not supported.";
const NON_UTF8_DESTINATION: &str = "Destination directory is not valid UTF-8.";
const NOT_DIRECTORY: &str = "Not a directory";
const CANNOT_OPEN: &str = "Cannot open";
const CUSTOM_HINT: &str = "Enter an existing directory. Relative paths use the invoking session's cwd; ~ uses your home. No shell expansion.";
const SOURCE_HINT: &str =
    "Choose one exact stored cwd, including missing directories. Descendants are not included.";
const DESTINATION_HINT: &str =
    "A destination session supplies its directory only; its conversation is not merged or changed.";
const FILES_UNCHANGED: &str =
    "No files or old workspace snapshots are moved. Destination sessions stay intact.";
const CLOSING_LABEL: &str = "Other open tabs to save and close";
const AFFECTED_LABEL: &str = "Affected sessions";
const DESTINATIONS_LABEL: &str = "destinations";
const PROJECT_USAGE: &str = "Include historical project usage";
const PROJECT_USAGE_SCOPE: &str = "ALL recorded lifetime usage under the exact source cwd, including forgotten and ephemeral sessions.";
const PROJECT_USAGE_AGGREGATED: &str =
    "The ledger is historically aggregated; individual session contributions cannot be selected.";
const PROJECT_USAGE_PRECONDITION: &str = "Before confirming with usage included, close OTHER processes using the source, including ephemeral processes.";
const PROJECT_USAGE_UNCHANGED: &str = "Lifetime project usage attribution will stay unchanged.";
const SESSION_USAGE_UNCHANGED: &str =
    "This session retains its own counters. Lifetime project usage attribution stays unchanged.";

#[derive(Debug)]
pub enum SessionRelocationAction {
    Consumed,
    Closed,
    Confirm(SessionRelocation, Option<(CaudraId, String)>),
    Copy(String),
}

enum EntryKind {
    Source(String),
    Destination(CaudraId, String),
    Custom,
    ProjectUsage,
    Confirm,
}

struct Entry {
    label: String,
    kind: EntryKind,
}

impl PickerItem for Entry {
    fn label(&self) -> &str {
        &self.label
    }
}

struct Target {
    directory: String,
    donor: Option<(CaudraId, String)>,
}

enum Stage {
    Source(Option<Target>),
    Destination,
    Custom(TextField),
    Confirm(SessionRelocation, Option<(CaudraId, String)>),
}

struct Flow {
    current_id: CaudraId,
    current_cwd: String,
    locations: Vec<SessionLocation>,
    source_cwd: Option<String>,
    other_open_count: usize,
    include_project_usage: bool,
    stage: Stage,
}

pub struct SessionRelocationPicker {
    picker: ListPicker<Entry>,
    flow: Option<Flow>,
}

impl SessionRelocationPicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new()
                .with_max_visible(MAX_VISIBLE)
                .with_width_percent(WIDTH_PERCENT),
            flow: None,
        }
    }

    pub fn open(
        &mut self,
        current_id: CaudraId,
        current_cwd: String,
        locations: Vec<SessionLocation>,
        bulk: bool,
        initial_destination: Option<String>,
        other_open_count: usize,
    ) {
        self.close();
        self.flow = Some(Flow {
            current_id,
            source_cwd: bulk.then(|| current_cwd.clone()),
            current_cwd,
            locations,
            other_open_count,
            include_project_usage: bulk,
            stage: Stage::Destination,
        });
        if bulk {
            self.show_sources(initial_destination.map(|directory| Target {
                directory,
                donor: None,
            }));
        } else if let Some(destination) = initial_destination {
            self.preview(destination, None);
        } else {
            self.show_destinations();
        }
    }

    pub fn is_open(&self) -> bool {
        self.flow.is_some()
    }

    pub fn close(&mut self) {
        self.flow = None;
        self.picker.close();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
        self.sync_selection();
    }

    pub fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }

    pub fn handle_key(&mut self, event: KeyEvent) -> SessionRelocationAction {
        let Some(flow) = &mut self.flow else {
            return SessionRelocationAction::Consumed;
        };
        if event.code == KeyCode::Esc {
            self.close();
            return SessionRelocationAction::Closed;
        }
        if event.code == KeyCode::Tab && matches!(flow.stage, Stage::Custom(_) | Stage::Confirm(..))
        {
            self.show_destinations();
            return SessionRelocationAction::Consumed;
        }
        match &mut flow.stage {
            Stage::Custom(field) => {
                if key::RENAME_SESSION.matches(event) {
                    let directory = field.text();
                    if flow.source_cwd.is_some() {
                        self.show_sources(Some(Target {
                            directory,
                            donor: None,
                        }));
                    } else {
                        self.show_destinations();
                    }
                } else if event.code == KeyCode::Enter {
                    let destination = field.text();
                    self.preview(destination, None);
                } else {
                    let edit = field.handle_key(event);
                    self.sync_custom();
                    match edit {
                        TextKey::Copy(text) | TextKey::Cut(text) => {
                            return SessionRelocationAction::Copy(text);
                        }
                        TextKey::Ignored if key::QUIT.matches(event) => {
                            self.close();
                            return SessionRelocationAction::Closed;
                        }
                        _ => {}
                    }
                }
                return SessionRelocationAction::Consumed;
            }
            Stage::Confirm(request, donor) => {
                if key::RENAME_SESSION.matches(event) {
                    let target = Target {
                        directory: request.destination.clone(),
                        donor: donor.clone(),
                    };
                    if flow.source_cwd.is_some() {
                        self.show_sources(Some(target));
                    } else {
                        self.show_destinations();
                    }
                    return SessionRelocationAction::Consumed;
                }
                if key::RELOCATION_CUSTOM.matches(event) {
                    let destination = request.destination.clone();
                    self.show_custom(destination);
                    return SessionRelocationAction::Consumed;
                }
                if key::RELOCATION_USAGE.matches(event) {
                    if self
                        .picker
                        .selected_item()
                        .is_some_and(|entry| matches!(entry.kind, EntryKind::ProjectUsage))
                    {
                        self.toggle_project_usage();
                    }
                    return SessionRelocationAction::Consumed;
                }
                // The list has nothing to search here, so it hears only the
                // keys that move or answer it; `Ctrl+C` closes through it.
                let for_list = matches!(
                    event.code,
                    KeyCode::Enter
                        | KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::PageUp
                        | KeyCode::PageDown
                ) || LIST_FIRST.matches(event)
                    || LIST_LAST.matches(event)
                    || key::QUIT.matches(event);
                if !for_list {
                    return SessionRelocationAction::Consumed;
                }
            }
            Stage::Destination if key::RELOCATION_CUSTOM.matches(event) => {
                self.show_custom(String::new());
                return SessionRelocationAction::Consumed;
            }
            _ => {}
        }
        let action = self.picker.handle_key(event);
        let result = self.map_action(action);
        self.sync_selection();
        result
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        match self.flow.as_mut().map(|flow| &mut flow.stage) {
            Some(Stage::Custom(field)) => {
                field.paste(text);
                self.sync_custom();
                true
            }
            Some(Stage::Confirm(..)) => true,
            Some(_) => {
                let handled = self.picker.handle_paste(text);
                self.sync_selection();
                handled
            }
            None => false,
        }
    }

    /// The custom stage is a text field, so the only thing its picker can
    /// answer the pointer with is a footer click.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> SessionRelocationAction {
        if matches!(
            self.flow.as_ref().map(|flow| &flow.stage),
            Some(Stage::Custom(_))
        ) {
            return match self.picker.handle_footer_mouse(event) {
                Some(key) => self.handle_key(key),
                None => SessionRelocationAction::Consumed,
            };
        }
        let action = self.picker.handle_mouse(event);
        let result = self.map_action(action);
        self.sync_selection();
        result
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("session_relocation", area);
        self.picker.view(frame, area)
    }

    fn show_sources(&mut self, destination: Option<Target>) {
        let Some(flow) = &mut self.flow else {
            return;
        };
        let mut groups = BTreeMap::new();
        groups.insert(flow.current_cwd.clone(), 0usize);
        for location in &flow.locations {
            *groups.entry(location.cwd.clone()).or_default() += 1;
        }
        let items = groups
            .into_iter()
            .map(|(cwd, count)| {
                let missing = if Path::new(&cwd).is_dir() {
                    ""
                } else {
                    " · missing"
                };
                Entry {
                    label: format!("{cwd} · {count} sessions{missing}"),
                    kind: EntryKind::Source(cwd),
                }
            })
            .collect();
        flow.stage = Stage::Source(destination);
        self.picker.set_error_text(None);
        self.picker.set_info_text(Some(SOURCE_HINT.into()));
        self.picker.set_footer_builder(source_footer);
        self.picker.open(items, SOURCE_TITLE);
        self.picker.select_item_by(|entry| {
            matches!(&entry.kind, EntryKind::Source(cwd) if Some(cwd) == flow.source_cwd.as_ref())
        });
        self.sync_selection();
    }

    fn show_destinations(&mut self) {
        let Some(flow) = &mut self.flow else {
            return;
        };
        flow.stage = Stage::Destination;
        let mut locations: Vec<_> = flow.locations.iter().collect();
        locations.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.as_bytes().cmp(b.id.as_bytes()))
        });
        let mut items = vec![Entry {
            label: CUSTOM_DIRECTORY.into(),
            kind: EntryKind::Custom,
        }];
        items.extend(locations.into_iter().map(|location| {
            let title = if location.title.is_empty() {
                UNTITLED
            } else {
                &location.title
            };
            Entry {
                label: format!("{title} · {} · {}", location.id, location.cwd),
                kind: EntryKind::Destination(location.id, location.cwd.clone()),
            }
        }));
        self.picker.set_error_text(None);
        self.picker.set_info_text(Some(DESTINATION_HINT.into()));
        self.picker.set_footer_builder(destination_footer);
        self.picker.open(items, DESTINATION_TITLE);
        self.sync_selection();
    }

    fn sync_selection(&mut self) {
        let hint = match self.flow.as_ref().map(|flow| &flow.stage) {
            Some(Stage::Source(_)) => SOURCE_HINT,
            Some(Stage::Destination) => DESTINATION_HINT,
            _ => return,
        };
        self.picker
            .set_info_text(Some(self.picker.selected_item().map_or_else(
                || hint.to_owned(),
                |entry| format!("{hint}\n{}", entry.label),
            )));
    }

    fn show_custom(&mut self, destination: String) {
        let Some(flow) = &mut self.flow else {
            return;
        };
        flow.stage = Stage::Custom(TextField::with_text(FieldKind::Line, &destination));
        self.picker.set_error_text(None);
        self.picker.set_info_text(Some(CUSTOM_HINT.into()));
        self.picker.set_empty_text(EMPTY_DESTINATION);
        self.picker.set_footer_builder(custom_footer);
        self.picker.open(Vec::new(), CUSTOM_TITLE);
        self.sync_custom();
    }

    fn sync_custom(&mut self) {
        if let Some(Flow {
            stage: Stage::Custom(field),
            ..
        }) = &self.flow
        {
            self.picker.mirror_search(field);
        }
    }

    /// Whether keys are typing into the custom directory.
    pub fn text_input_active(&self) -> bool {
        matches!(
            self.flow.as_ref().map(|flow| &flow.stage),
            Some(Stage::Custom(_))
        )
    }

    fn preview(&mut self, input: String, donor: Option<(CaudraId, String)>) {
        let Some(flow) = &self.flow else {
            return;
        };
        let result = resolve_destination(
            &input,
            Path::new(&flow.current_cwd),
            paths::home().as_deref(),
        )
        .and_then(|destination| {
            let sessions: Vec<_> = flow
                .locations
                .iter()
                .filter(|location| {
                    flow.source_cwd
                        .as_ref()
                        .map_or(location.id == flow.current_id, |source| {
                            location.cwd == *source
                        })
                })
                .cloned()
                .collect();
            if sessions.is_empty() {
                return Err(EMPTY_SELECTION.into());
            }
            if sessions
                .iter()
                .any(|session| same_directory(&session.cwd, &destination))
            {
                return Err(SAME_DIRECTORY.into());
            }
            Ok(SessionRelocation {
                sessions,
                source_cwd: flow.source_cwd.clone(),
                destination,
                include_project_usage: flow.include_project_usage,
                keep_plan: false,
            })
        });
        let request = match result {
            Ok(request) => request,
            Err(error) => {
                self.show_custom(input);
                self.picker.set_error_text(Some(error));
                return;
            }
        };
        if let Some(flow) = &mut self.flow {
            flow.stage = Stage::Confirm(request, donor);
        }
        self.show_confirmation();
    }

    fn show_confirmation(&mut self) {
        let Some(flow) = &self.flow else {
            return;
        };
        let Stage::Confirm(request, _) = &flow.stage else {
            return;
        };
        let source = request
            .source_cwd
            .as_deref()
            .unwrap_or(&request.sessions[0].cwd);
        let closing = if request.source_cwd.is_some() {
            0
        } else {
            flow.other_open_count
        };
        let mut info = format!(
            "Source: {source}\nDestination: {}\n{AFFECTED_LABEL}: {}\n{CLOSING_LABEL}: {closing}\n{FILES_UNCHANGED}",
            request.destination,
            request.sessions.len(),
        );
        let mut items = Vec::new();
        if request.source_cwd.is_some() {
            info.push_str(&format!(
                "\n{PROJECT_USAGE_SCOPE}\n{PROJECT_USAGE_AGGREGATED}\n{}",
                if request.include_project_usage {
                    PROJECT_USAGE_PRECONDITION
                } else {
                    PROJECT_USAGE_UNCHANGED
                }
            ));
            items.push(Entry {
                label: format!(
                    "[{}] {PROJECT_USAGE} ({} toggles this row)",
                    if request.include_project_usage {
                        "x"
                    } else {
                        " "
                    },
                    key::RELOCATION_USAGE.label,
                ),
                kind: EntryKind::ProjectUsage,
            });
        } else {
            info.push_str(&format!("\n{SESSION_USAGE_UNCHANGED}"));
        }
        items.push(Entry {
            label: CONFIRM.into(),
            kind: EntryKind::Confirm,
        });
        self.picker.set_error_text(None);
        self.picker.set_info_text(Some(info));
        self.picker.set_footer_builder(confirm_footer);
        self.picker.open(items, CONFIRM_TITLE);
        self.picker
            .select_item_by(|entry| matches!(entry.kind, EntryKind::Confirm));
    }

    fn toggle_project_usage(&mut self) {
        let Some(Flow {
            stage: Stage::Confirm(request, _),
            include_project_usage,
            ..
        }) = &mut self.flow
        else {
            return;
        };
        if request.source_cwd.is_none() {
            return;
        }
        *include_project_usage = !*include_project_usage;
        request.include_project_usage = *include_project_usage;
        self.show_confirmation();
        self.picker
            .select_item_by(|entry| matches!(entry.kind, EntryKind::ProjectUsage));
    }

    fn confirm(&mut self) -> SessionRelocationAction {
        if !matches!(
            self.flow.as_ref().map(|flow| &flow.stage),
            Some(Stage::Confirm(..))
        ) {
            return SessionRelocationAction::Consumed;
        }
        self.picker.close();
        match self.flow.take().map(|flow| flow.stage) {
            Some(Stage::Confirm(request, donor)) => {
                SessionRelocationAction::Confirm(request, donor)
            }
            _ => SessionRelocationAction::Consumed,
        }
    }

    fn map_action(&mut self, action: PickerAction<Entry>) -> SessionRelocationAction {
        match action {
            PickerAction::Close => {
                self.close();
                return SessionRelocationAction::Closed;
            }
            PickerAction::Select(entry) => match entry.kind {
                EntryKind::Source(source) => {
                    let Some(flow) = &mut self.flow else {
                        return SessionRelocationAction::Consumed;
                    };
                    flow.source_cwd = Some(source);
                    let previous = mem::replace(&mut flow.stage, Stage::Destination);
                    if let Stage::Source(Some(target)) = previous {
                        self.preview(target.directory, target.donor);
                    } else {
                        self.show_destinations();
                    }
                }
                EntryKind::Destination(id, cwd) => self.preview(cwd.clone(), Some((id, cwd))),
                EntryKind::Custom => self.show_custom(String::new()),
                EntryKind::ProjectUsage => self.toggle_project_usage(),
                EntryKind::Confirm => return self.confirm(),
            },
            PickerAction::Key(key) => return self.handle_key(key),
            PickerAction::Copy(text) => return SessionRelocationAction::Copy(text),
            PickerAction::Consumed | PickerAction::Toggle(..) => {}
        }
        SessionRelocationAction::Consumed
    }
}

impl Overlay for SessionRelocationPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        self.cadence()
    }
}

fn resolve_destination(
    input: &str,
    current_cwd: &Path,
    home: Option<&Path>,
) -> Result<String, String> {
    if input.is_empty() {
        return Err(EMPTY_DESTINATION.into());
    }
    let path = if input == "~" {
        home.ok_or(MISSING_HOME)?.to_path_buf()
    } else if let Some(rest) = input.strip_prefix("~/") {
        home.ok_or(MISSING_HOME)?.join(rest.trim_start_matches('/'))
    } else if input.starts_with('~') {
        return Err(UNSUPPORTED_TILDE.into());
    } else {
        current_cwd.join(input)
    };
    let canonical = fs::canonicalize(&path)
        .map_err(|error| format!("{CANNOT_OPEN} {}: {error}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!("{NOT_DIRECTORY}: {}", canonical.display()));
    }
    fs::read_dir(&canonical)
        .map_err(|error| format!("Cannot access {}: {error}", canonical.display()))?;
    canonical
        .into_os_string()
        .into_string()
        .map_err(|_| NON_UTF8_DESTINATION.into())
}

fn same_directory(source: &str, destination: &str) -> bool {
    source == destination
        || fs::canonicalize(source).ok().as_deref() == Some(Path::new(destination))
}

fn source_footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "choose source"),
        Hint::bind(key::ESC, "cancel"),
    ]
}

fn destination_footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "choose"),
        Hint::bind(key::RELOCATION_CUSTOM, "Custom directory"),
        Hint::bind(key::ESC, "cancel"),
    ]
}

fn custom_footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "preview"),
        Hint::bind(key::RENAME_SESSION, "change selection"),
        Hint::bind(key::TAB, DESTINATIONS_LABEL),
        Hint::bind(key::ESC, "cancel"),
    ]
}

fn confirm_footer() -> Vec<Hint> {
    vec![
        Hint::inert("↑↓", "select"),
        Hint::bind(key::ENTER, "activate"),
        Hint::bind(key::RENAME_SESSION, "selection"),
        Hint::bind(key::TAB, DESTINATIONS_LABEL),
        Hint::bind(key::RELOCATION_CUSTOM, "directory"),
        Hint::bind(key::ESC, "cancel"),
    ]
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use caudra_storage::id::CaudraId;
    use caudra_storage::sessions::SessionLocation;
    use caudra_workbench::keys::LIST_LAST;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect};
    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    use super::{
        AFFECTED_LABEL, CANNOT_OPEN, CLOSING_LABEL, CONFIRM, CUSTOM_DIRECTORY, DESTINATIONS_LABEL,
        EMPTY_DESTINATION, EMPTY_SELECTION, EntryKind, FILES_UNCHANGED, MISSING_HOME,
        NOT_DIRECTORY, PROJECT_USAGE, PROJECT_USAGE_AGGREGATED, PROJECT_USAGE_PRECONDITION,
        PROJECT_USAGE_SCOPE, PROJECT_USAGE_UNCHANGED, SAME_DIRECTORY, SESSION_USAGE_UNCHANGED,
        SessionRelocationAction, SessionRelocationPicker, Stage, UNSUPPORTED_TILDE,
        resolve_destination,
    };
    use crate::components::keybindings::key;

    const SOURCE: &str = "source";
    const TARGET: &str = "target with spaces";
    const HOME: &str = "home";
    const OLD: &str = "deleted source";
    const TITLE: &str = "same title";
    const COPY_KEEPS_FIELD: &str = "copying the custom directory left the field";
    const WRITE_VERSION: i64 = 7;
    const UPDATED_AT: u64 = 42;
    const CURRENT: u8 = 1;
    const SIBLING: u8 = 2;
    const DONOR: u8 = 3;
    const CHILD: u8 = 4;
    const OTHER_OPEN_COUNT: usize = 2;
    const SCREEN_WIDTH: u16 = 120;
    const SCREEN_HEIGHT: u16 = 40;

    fn workspace() -> TempDir {
        let root = tempdir().unwrap();
        for directory in [SOURCE, TARGET, HOME] {
            fs::create_dir(root.path().join(directory)).unwrap();
        }
        fs::create_dir(root.path().join(HOME).join(TARGET)).unwrap();
        root
    }

    fn path_string(path: &Path) -> String {
        path.to_str().unwrap().to_owned()
    }

    fn location(tag: u8, cwd: &Path) -> SessionLocation {
        SessionLocation {
            id: CaudraId::from_bytes([tag; 16]),
            title: TITLE.into(),
            cwd: path_string(cwd),
            updated_at: UPDATED_AT,
            write_version: WRITE_VERSION,
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn opened(root: &Path, bulk: bool, destination: Option<String>) -> SessionRelocationPicker {
        let source = root.join(SOURCE);
        let current = location(CURRENT, &source);
        let mut picker = SessionRelocationPicker::new();
        picker.open(
            current.id,
            current.cwd.clone(),
            vec![
                current,
                location(SIBLING, &source),
                location(DONOR, &root.join(TARGET)),
            ],
            bulk,
            destination,
            OTHER_OPEN_COUNT,
        );
        picker
    }

    fn render(picker: &mut SessionRelocationPicker) -> (String, Position) {
        render_hit(picker, CONFIRM)
    }

    fn render_hit(picker: &mut SessionRelocationPicker, label: &str) -> (String, Position) {
        let mut terminal = Terminal::new(TestBackend::new(SCREEN_WIDTH, SCREEN_HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, Rect::new(0, 0, SCREEN_WIDTH, SCREEN_HEIGHT));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        let mut confirm = Position::default();
        for y in 0..SCREEN_HEIGHT {
            let line: String = (0..SCREEN_WIDTH)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect();
            if let Some(x) = line.find(label) {
                confirm = Position::new(line[..x].chars().count() as u16, y);
            }
            text.push_str(line.trim_end());
            text.push('\n');
        }
        (text, confirm)
    }

    fn click(picker: &mut SessionRelocationPicker, position: Position) -> SessionRelocationAction {
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: position.x,
            row: position.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            picker.handle_mouse(event),
            SessionRelocationAction::Consumed
        ));
        picker.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..event
        })
    }

    #[test_case("target with spaces", "target with spaces"; "relative_spaces")]
    #[test_case("./target with spaces/..", ""; "canonical_parent")]
    #[test_case("~", "home"; "home_directory")]
    #[test_case("~/target with spaces", "home/target with spaces"; "home_child")]
    fn destination_resolution(input: &str, expected: &str) {
        let root = workspace();
        let resolved =
            resolve_destination(input, root.path(), Some(&root.path().join(HOME))).unwrap();
        assert_eq!(
            resolved,
            path_string(&fs::canonicalize(root.path().join(expected)).unwrap())
        );
    }

    #[test_case("", EMPTY_DESTINATION; "empty")]
    #[test_case("~", MISSING_HOME; "missing_home")]
    #[test_case("~another", UNSUPPORTED_TILDE; "named_user")]
    fn destination_resolution_errors(input: &str, expected: &str) {
        let root = workspace();
        assert_eq!(
            resolve_destination(input, root.path(), None).unwrap_err(),
            expected
        );
    }

    #[test_case(false; "missing")]
    #[test_case(true; "file")]
    fn invalid_destination_stays_editable(file_exists: bool) {
        let root = workspace();
        let invalid = root.path().join(OLD);
        if file_exists {
            fs::write(&invalid, TITLE).unwrap();
        }
        let picker = opened(root.path(), false, Some(path_string(&invalid)));
        assert!(matches!(
            picker.flow.as_ref().unwrap().stage,
            Stage::Custom(_)
        ));
        let prefix = if file_exists {
            NOT_DIRECTORY
        } else {
            CANNOT_OPEN
        };
        assert!(picker.picker.error_text().unwrap().starts_with(prefix));
    }

    #[test_case(false; "exact")]
    #[test_case(true; "canonical_alias")]
    fn same_directory_is_an_explicit_noop(alias: bool) {
        let root = workspace();
        let source = root.path().join(SOURCE);
        let destination = if alias { source.join(".") } else { source };
        let mut picker = opened(root.path(), false, Some(path_string(&destination)));
        assert_eq!(picker.picker.error_text(), Some(SAME_DIRECTORY));
        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            SessionRelocationAction::Consumed
        ));
        assert!(picker.is_open());
    }

    #[test_case(false; "current_not_checkpointed")]
    #[test_case(true; "empty_source")]
    fn empty_inventory_never_confirms(bulk: bool) {
        let root = workspace();
        let current = location(CURRENT, &root.path().join(SOURCE));
        let mut picker = SessionRelocationPicker::new();
        picker.open(
            current.id,
            current.cwd,
            Vec::new(),
            bulk,
            Some(path_string(&root.path().join(TARGET))),
            0,
        );
        if bulk {
            picker.handle_key(press(KeyCode::Enter));
        }
        assert_eq!(picker.picker.error_text(), Some(EMPTY_SELECTION));
        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            SessionRelocationAction::Consumed
        ));
    }

    #[test_case(false; "keyboard")]
    #[test_case(true; "mouse")]
    fn current_move_preview_and_confirmation(mouse: bool) {
        let root = workspace();
        let destination = path_string(&root.path().join(TARGET));
        let mut picker = opened(root.path(), false, Some(destination.clone()));
        let (text, confirm) = render(&mut picker);
        assert!(text.contains(FILES_UNCHANGED));
        assert!(text.contains(SESSION_USAGE_UNCHANGED));
        assert!(!text.contains(PROJECT_USAGE));
        assert!(matches!(
            picker.handle_key(key::RELOCATION_USAGE.to_key_event()),
            SessionRelocationAction::Consumed
        ));
        assert!(text.contains(&format!("{CLOSING_LABEL}: {OTHER_OPEN_COUNT}")));
        assert!(text.contains(&format!("{AFFECTED_LABEL}: 1")));
        assert!(picker.contains(confirm));
        let action = if mouse {
            let event = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: confirm.x,
                row: confirm.y,
                modifiers: KeyModifiers::NONE,
            };
            assert!(matches!(
                picker.handle_mouse(event),
                SessionRelocationAction::Consumed
            ));
            picker.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..event
            })
        } else {
            picker.handle_key(press(KeyCode::Enter))
        };
        let SessionRelocationAction::Confirm(request, donor) = action else {
            panic!("{action:?}")
        };
        assert_eq!(
            request.sessions,
            vec![location(CURRENT, &root.path().join(SOURCE))]
        );
        assert_eq!(request.source_cwd, None);
        assert!(!request.include_project_usage);
        assert_eq!(request.destination, destination);
        assert_eq!(donor, None);
        assert!(!picker.is_open());
        assert!(root.path().join(SOURCE).is_dir());
    }

    #[test_case(1; "up")]
    #[test_case(-1; "down")]
    fn scrolling_cancels_a_pressed_confirmation(delta: i32) {
        let root = workspace();
        let mut picker = opened(
            root.path(),
            false,
            Some(path_string(&root.path().join(TARGET))),
        );
        let (_, confirm) = render(&mut picker);
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: confirm.x,
            row: confirm.y,
            modifiers: KeyModifiers::NONE,
        };
        picker.handle_mouse(event);
        picker.scroll(delta);
        assert!(matches!(
            picker.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..event
            }),
            SessionRelocationAction::Consumed
        ));
        assert!(picker.is_open());
        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            SessionRelocationAction::Confirm(..)
        ));
    }

    #[test_case(false; "without_prefill")]
    #[test_case(true; "with_prefill")]
    fn bulk_source_is_exact_and_editable(prefill: bool) {
        let root = workspace();
        let source = root.path().join(SOURCE);
        let old = root.path().join(OLD);
        let destination = path_string(&root.path().join(TARGET));
        let current = location(CURRENT, &source);
        let expected = location(SIBLING, &old);
        let mut picker = SessionRelocationPicker::new();
        picker.open(
            current.id,
            current.cwd.clone(),
            vec![
                current,
                expected.clone(),
                location(CHILD, &old.join(SOURCE)),
            ],
            true,
            prefill.then(|| destination.clone()),
            OTHER_OPEN_COUNT,
        );
        assert!(
            matches!(&picker.picker.selected_item().unwrap().kind, EntryKind::Source(cwd) if cwd == &path_string(&source))
        );
        picker.picker.select_item_by(
            |entry| matches!(&entry.kind, EntryKind::Source(cwd) if cwd == &path_string(&old)),
        );
        picker.handle_key(press(KeyCode::Enter));
        if !prefill {
            picker.handle_key(key::RELOCATION_CUSTOM.to_key_event());
            picker.handle_paste(&destination);
            picker.handle_key(press(KeyCode::Enter));
        }
        let SessionRelocationAction::Confirm(request, _) = picker.handle_key(press(KeyCode::Enter))
        else {
            panic!()
        };
        assert_eq!(request.sessions, vec![expected]);
        assert_eq!(request.source_cwd.as_deref(), old.to_str());
        assert!(request.include_project_usage);
        assert!(!old.exists());
    }

    #[test_case(KeyCode::Char(' '), false; "space")]
    #[test_case(KeyCode::Enter, false; "enter")]
    #[test_case(KeyCode::Enter, true; "mouse")]
    fn bulk_usage_can_be_excluded_before_confirmation(code: KeyCode, mouse: bool) {
        let root = workspace();
        let mut picker = opened(
            root.path(),
            true,
            Some(path_string(&root.path().join(TARGET))),
        );
        picker.handle_key(press(KeyCode::Enter));
        let (text, usage) = render_hit(&mut picker, PROJECT_USAGE);
        assert!(text.contains(&format!("[x] {PROJECT_USAGE}")));
        assert!(text.contains(PROJECT_USAGE_SCOPE));
        assert!(text.contains(PROJECT_USAGE_AGGREGATED));
        assert!(text.contains(PROJECT_USAGE_PRECONDITION));
        assert!(text.contains(key::RELOCATION_USAGE.label));
        assert!(!text.contains(SESSION_USAGE_UNCHANGED));
        let action = if mouse {
            click(&mut picker, usage)
        } else {
            picker.handle_key(press(KeyCode::Up));
            picker.handle_key(press(code))
        };
        assert!(matches!(action, SessionRelocationAction::Consumed));
        let (text, confirm) = render(&mut picker);
        assert!(text.contains(&format!("[ ] {PROJECT_USAGE}")));
        assert!(text.contains(PROJECT_USAGE_UNCHANGED));
        assert!(!text.contains(PROJECT_USAGE_PRECONDITION));
        let action = if mouse {
            click(&mut picker, confirm)
        } else {
            picker.handle_key(press(KeyCode::Down));
            picker.handle_key(press(KeyCode::Enter))
        };
        let SessionRelocationAction::Confirm(request, None) = action else {
            panic!("{action:?}")
        };
        assert!(!request.include_project_usage);
        assert_eq!(request.sessions.len(), 2);
        assert_eq!(
            request.source_cwd,
            Some(path_string(&root.path().join(SOURCE)))
        );
        assert!(!picker.is_open());
    }

    #[test_case(1; "up")]
    #[test_case(-1; "down")]
    fn scrolling_cancels_a_pressed_usage_toggle(delta: i32) {
        let root = workspace();
        let mut picker = opened(
            root.path(),
            true,
            Some(path_string(&root.path().join(TARGET))),
        );
        picker.handle_key(press(KeyCode::Enter));
        let (_, usage) = render_hit(&mut picker, PROJECT_USAGE);
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: usage.x,
            row: usage.y,
            modifiers: KeyModifiers::NONE,
        };
        picker.handle_mouse(event);
        picker.scroll(delta);
        assert!(matches!(
            picker.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..event
            }),
            SessionRelocationAction::Consumed
        ));
        assert!(
            render(&mut picker)
                .0
                .contains(&format!("[x] {PROJECT_USAGE}"))
        );
        picker.handle_key(LIST_LAST.to_key_event());
        let SessionRelocationAction::Confirm(request, _) = picker.handle_key(press(KeyCode::Enter))
        else {
            panic!()
        };
        assert!(request.include_project_usage);
    }

    #[test_case(false; "opt_out")]
    #[test_case(true; "opt_back_in")]
    fn bulk_usage_choice_survives_source_destination_and_custom_navigation(include: bool) {
        let root = workspace();
        let destination = path_string(&root.path().join(TARGET));
        let mut picker = opened(root.path(), true, Some(destination));
        let replacement = location(CHILD, &root.path().join(HOME));
        picker
            .flow
            .as_mut()
            .unwrap()
            .locations
            .push(replacement.clone());
        picker.handle_key(press(KeyCode::Enter));
        picker.handle_key(press(KeyCode::Up));
        picker.handle_key(key::RELOCATION_USAGE.to_key_event());
        if include {
            picker.handle_key(key::RELOCATION_USAGE.to_key_event());
        }
        picker.handle_key(key::RENAME_SESSION.to_key_event());
        picker.picker.select_item_by(
            |entry| matches!(&entry.kind, EntryKind::Source(cwd) if *cwd == replacement.cwd),
        );
        picker.handle_key(press(KeyCode::Enter));
        picker.handle_key(key::RELOCATION_CUSTOM.to_key_event());
        picker.handle_key(key::RENAME_SESSION.to_key_event());
        picker.handle_key(press(KeyCode::Enter));
        picker.handle_key(key::RELOCATION_CUSTOM.to_key_event());
        picker.handle_key(press(KeyCode::Tab));
        picker.handle_key(key::RELOCATION_CUSTOM.to_key_event());
        picker.handle_paste(&path_string(&root.path().join(OLD)));
        picker.handle_key(press(KeyCode::Enter));
        assert!(picker.picker.error_text().unwrap().starts_with(CANNOT_OPEN));
        picker.handle_key(press(KeyCode::Tab));
        let donor = location(DONOR, &root.path().join(TARGET));
        picker.picker.select_item_by(
            |entry| matches!(&entry.kind, EntryKind::Destination(id, _) if *id == donor.id),
        );
        picker.handle_key(press(KeyCode::Enter));
        let text = render(&mut picker).0;
        assert_eq!(text.contains(PROJECT_USAGE_PRECONDITION), include);
        let SessionRelocationAction::Confirm(request, selected) =
            picker.handle_key(press(KeyCode::Enter))
        else {
            panic!()
        };
        assert_eq!(request.include_project_usage, include);
        assert_eq!(request.source_cwd.as_ref(), Some(&replacement.cwd));
        assert_eq!(request.sessions, vec![replacement]);
        assert_eq!(selected, Some((donor.id, donor.cwd)));
    }

    #[test_case(false, false; "confirmed_then_single")]
    #[test_case(false, true; "confirmed_then_bulk")]
    #[test_case(true, false; "cancelled_then_single")]
    #[test_case(true, true; "cancelled_then_bulk")]
    fn new_flow_resets_usage_choice(cancel: bool, bulk: bool) {
        let root = workspace();
        let destination = path_string(&root.path().join(TARGET));
        let mut picker = opened(root.path(), true, Some(destination.clone()));
        picker.handle_key(press(KeyCode::Enter));
        picker.handle_key(press(KeyCode::Up));
        picker.handle_key(key::RELOCATION_USAGE.to_key_event());
        picker.handle_key(press(KeyCode::Down));
        let action = picker.handle_key(press(if cancel { KeyCode::Esc } else { KeyCode::Enter }));
        if cancel {
            assert!(matches!(action, SessionRelocationAction::Closed));
        } else {
            assert!(
                matches!(action, SessionRelocationAction::Confirm(request, _) if !request.include_project_usage)
            );
        }
        assert!(!picker.is_open());
        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            SessionRelocationAction::Consumed
        ));
        let current = location(CURRENT, &root.path().join(SOURCE));
        picker.open(
            current.id,
            current.cwd.clone(),
            vec![current],
            bulk,
            Some(destination),
            OTHER_OPEN_COUNT,
        );
        if bulk {
            picker.handle_key(press(KeyCode::Enter));
        }
        let text = render(&mut picker).0;
        assert_eq!(text.contains(&format!("[x] {PROJECT_USAGE}")), bulk);
        let SessionRelocationAction::Confirm(request, None) =
            picker.handle_key(press(KeyCode::Enter))
        else {
            panic!()
        };
        assert_eq!(request.include_project_usage, bulk);
    }

    #[test_case(false; "current")]
    #[test_case(true; "bulk_change_source")]
    fn destination_identity_survives_confirmation(bulk: bool) {
        let root = workspace();
        let mut picker = opened(root.path(), bulk, None);
        if bulk {
            picker.handle_key(press(KeyCode::Enter));
        }
        let donor = location(DONOR, &root.path().join(TARGET));
        let labels: Vec<_> = (0..)
            .map_while(|index| picker.picker.item(index))
            .map(|entry| entry.label.clone())
            .collect();
        assert_eq!(labels[0], CUSTOM_DIRECTORY);
        assert!(
            labels
                .iter()
                .any(|label| label.contains(&donor.id.to_string()) && label.contains(&donor.cwd))
        );
        picker.picker.select_item_by(
            |entry| matches!(&entry.kind, EntryKind::Destination(id, _) if *id == donor.id),
        );
        picker.handle_key(press(KeyCode::Enter));
        if bulk {
            picker.handle_key(key::RENAME_SESSION.to_key_event());
            picker.handle_key(press(KeyCode::Enter));
        }
        let SessionRelocationAction::Confirm(request, selected) =
            picker.handle_key(press(KeyCode::Enter))
        else {
            panic!()
        };
        assert_eq!(selected, Some((donor.id, donor.cwd)));
        assert_eq!(request.sessions.len(), if bulk { 2 } else { 1 });
    }

    #[test_case(false, false; "custom")]
    #[test_case(true, false; "prefilled_confirmation")]
    #[test_case(true, true; "session_confirmation")]
    fn bulk_can_return_to_destination_sessions(confirm: bool, from_session: bool) {
        let root = workspace();
        let destination = path_string(&root.path().join(TARGET));
        let mut picker = opened(root.path(), true, (!from_session).then_some(destination));
        let previous = location(DONOR, &root.path().join(TARGET));
        let replacement = location(CHILD, &root.path().join(HOME));
        picker
            .flow
            .as_mut()
            .unwrap()
            .locations
            .push(replacement.clone());
        picker.handle_key(press(KeyCode::Enter));
        if from_session {
            picker.picker.select_item_by(
                |entry| matches!(&entry.kind, EntryKind::Destination(id, _) if *id == previous.id),
            );
            picker.handle_key(press(KeyCode::Enter));
        }
        if !confirm {
            picker.handle_key(key::RELOCATION_CUSTOM.to_key_event());
        }
        assert!(render(&mut picker).0.contains(DESTINATIONS_LABEL));
        picker.handle_key(press(KeyCode::Tab));
        assert!(matches!(
            picker.flow.as_ref().unwrap().stage,
            Stage::Destination
        ));
        assert_eq!(picker.picker.error_text(), None);
        picker.picker.select_item_by(
            |entry| matches!(&entry.kind, EntryKind::Destination(id, _) if *id == replacement.id),
        );
        picker.handle_key(press(KeyCode::Enter));
        let SessionRelocationAction::Confirm(request, donor) =
            picker.handle_key(press(KeyCode::Enter))
        else {
            panic!()
        };
        assert_eq!(
            request.source_cwd,
            Some(path_string(&root.path().join(SOURCE)))
        );
        assert_eq!(request.sessions.len(), 2);
        assert_eq!(request.destination, replacement.cwd);
        assert_eq!(donor, Some((replacement.id, replacement.cwd)));
    }

    #[test_case(false; "valid_prefill")]
    #[test_case(true; "invalid_prefill")]
    fn bulk_custom_can_change_source_with_prefill(invalid: bool) {
        let root = workspace();
        let destination = path_string(&root.path().join(if invalid { OLD } else { TARGET }));
        let mut picker = opened(root.path(), true, Some(destination.clone()));
        let replacement = location(CHILD, &root.path().join(HOME));
        picker
            .flow
            .as_mut()
            .unwrap()
            .locations
            .push(replacement.clone());
        picker.handle_key(press(KeyCode::Enter));
        if !invalid {
            picker.handle_key(key::RELOCATION_CUSTOM.to_key_event());
        }
        picker.handle_key(key::RENAME_SESSION.to_key_event());
        assert!(matches!(
            &picker.flow.as_ref().unwrap().stage,
            Stage::Source(Some(target)) if target.directory == destination && target.donor.is_none()
        ));
        picker.picker.select_item_by(
            |entry| matches!(&entry.kind, EntryKind::Source(cwd) if *cwd == replacement.cwd),
        );
        picker.handle_key(press(KeyCode::Enter));
        assert_eq!(
            picker.flow.as_ref().unwrap().source_cwd.as_ref(),
            Some(&replacement.cwd)
        );
        if invalid {
            assert_eq!(picker.picker.search_text(), destination);
            assert!(picker.picker.error_text().unwrap().starts_with(CANNOT_OPEN));
        } else {
            let SessionRelocationAction::Confirm(request, None) =
                picker.handle_key(press(KeyCode::Enter))
            else {
                panic!()
            };
            assert_eq!(request.sessions, vec![replacement]);
            assert_eq!(request.destination, destination);
        }
    }

    #[test_case(0; "source")]
    #[test_case(1; "destination")]
    #[test_case(2; "custom")]
    #[test_case(3; "confirmation")]
    fn escape_cancels_every_stage(stage: usize) {
        let root = workspace();
        let mut picker = opened(
            root.path(),
            stage == 0,
            (stage == 3).then(|| path_string(&root.path().join(TARGET))),
        );
        if stage == 2 {
            picker.handle_key(key::RELOCATION_CUSTOM.to_key_event());
        }
        assert!(matches!(
            picker.handle_key(press(KeyCode::Esc)),
            SessionRelocationAction::Closed
        ));
        assert!(!picker.is_open());
        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            SessionRelocationAction::Consumed
        ));
        assert!(!picker.handle_paste(TITLE));
    }

    #[test_case(0; "source")]
    #[test_case(1; "destination")]
    #[test_case(2; "custom")]
    #[test_case(3; "confirmation")]
    fn ctrl_c_with_nothing_selected_cancels_every_stage(stage: usize) {
        let root = workspace();
        let mut picker = opened(
            root.path(),
            stage == 0,
            (stage == 3).then(|| path_string(&root.path().join(TARGET))),
        );
        if stage == 2 {
            picker.handle_key(key::RELOCATION_CUSTOM.to_key_event());
        }
        assert!(matches!(
            picker.handle_key(key::QUIT.to_key_event()),
            SessionRelocationAction::Closed
        ));
        assert!(!picker.is_open());
    }

    #[test]
    fn ctrl_c_copies_the_selected_custom_directory() {
        let root = workspace();
        let mut picker = opened(root.path(), false, None);
        picker.handle_key(key::RELOCATION_CUSTOM.to_key_event());
        picker.handle_paste(TITLE);
        picker.handle_key(key::SELECT_ALL.to_key_event());

        let action = picker.handle_key(key::QUIT.to_key_event());

        assert!(matches!(action, SessionRelocationAction::Copy(text) if text == TITLE));
        assert!(picker.text_input_active(), "{COPY_KEEPS_FIELD}");
    }

    #[test_case(false; "custom_entry")]
    #[test_case(true; "custom_shortcut_after_filter")]
    fn custom_input_echoes_paste_and_resolves_against_invoking_cwd(shortcut: bool) {
        let root = workspace();
        let mut picker = opened(root.path(), false, None);
        if shortcut {
            picker.handle_paste(TITLE);
            picker.handle_key(key::RELOCATION_CUSTOM.to_key_event());
        } else {
            picker.handle_key(press(KeyCode::Enter));
        }
        let input = format!("../{TARGET}");
        assert!(picker.handle_paste(&input));
        assert_eq!(picker.picker.search_text(), input);
        picker.handle_key(press(KeyCode::Enter));
        let SessionRelocationAction::Confirm(request, None) =
            picker.handle_key(press(KeyCode::Enter))
        else {
            panic!()
        };
        assert_eq!(request.destination, path_string(&root.path().join(TARGET)));
    }
}
