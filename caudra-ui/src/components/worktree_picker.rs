use std::mem;
use std::path::{Path, PathBuf};

use caudra_agent::worktree::{Changes, CreateRequest, RemoveRequest, Request, label};
use caudra_grab::grab_scope;
use caudra_storage::id::CaudraId;
use caudra_storage::worktrees::CheckoutSessions;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use super::status_bar::collapse_home;
use super::{Hint, Overlay};
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::Cadence;
use crate::text_buffer::TextBuffer;

const MAX_VISIBLE: u16 = 12;
const WIDTH_PERCENT: u16 = 80;
const LIST_TITLE: &str = " Worktrees ";
const CREATE_TITLE: &str = " New worktree ";
const BRANCH_TITLE: &str = " New worktree: branch ";
const BASE_TITLE: &str = " New worktree: start at ";
const REMOVE_TITLE: &str = " Remove worktree ";
const HEAD: &str = "HEAD";
const MAIN_CHECKOUT: &str = "main checkout";
const CURRENT: &str = "current";
const GENERATED_BRANCH: &str = "named for you";
const BRANCH_LABEL: &str = "Branch";
const BASE_LABEL: &str = "Start at";
const CARRY_LABEL: &str = "Carry uncommitted changes";
const CREATE: &str = "Create worktree";
const REMOVE: &str = "Remove worktree";
const STASH_AND_REMOVE: &str = "Stash uncommitted changes, then remove";
const DISCARD_AND_REMOVE: &str = "Discard uncommitted changes and remove";
const MAIN_NOT_REMOVABLE: &str = "The main checkout cannot be removed.";
const ALREADY_HERE: &str = "This session already works in that checkout.";
const LIST_HINT_GIT: &str = "Enter moves every open tab into the checkout, as /cd does.";
const LIST_HINT_HERDR: &str = "Enter opens the checkout in its Herdr workspace.";
const CREATE_HINT: &str = "The session moves into the new worktree and back once it is removed.";
const CARRY_HINT: &str = "Carried changes leave this checkout.";
const BRANCH_HINT: &str =
    "Leave it empty to have one named for you. An existing branch is checked out as it is.";
const BASE_HINT: &str = "A branch, tag or commit of this checkout.";
const BRANCH_KEPT: &str = "Its branch is kept.";
pub(crate) const USAGE: &str = "Usage: /worktree [new [branch] | remove]";
const NEW_ARGUMENT: &str = "new";
const REMOVE_ARGUMENT: &str = "remove";

/// Where `/worktree` opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeView {
    List,
    Create(Option<String>),
    /// The removal of the checkout the session works in.
    Remove,
}

impl WorktreeView {
    pub fn parse(args: &str) -> Result<Self, &'static str> {
        let mut words = args.split_whitespace();
        let view = match words.next() {
            None => Self::List,
            Some(NEW_ARGUMENT) => Self::Create(words.next().map(str::to_owned)),
            Some(REMOVE_ARGUMENT) => Self::Remove,
            Some(_) => return Err(USAGE),
        };
        match words.next() {
            Some(_) => Err(USAGE),
            None => Ok(view),
        }
    }
}

#[derive(Debug)]
pub enum WorktreeAction {
    Consumed,
    Closed,
    Refresh,
    Open(PathBuf),
    /// The removal of this checkout needs to know whether it has changes.
    InspectRemoval(PathBuf),
    Run(Request),
}

/// The repository as `/worktree` shows it.
#[derive(Clone, Debug)]
pub struct WorktreeOverview {
    pub session: CaudraId,
    pub cwd: PathBuf,
    /// The checkout the session works in.
    pub current: PathBuf,
    pub main_root: PathBuf,
    pub checkouts: Vec<CheckoutSessions>,
    /// Whether the current checkout has uncommitted changes.
    pub dirty: bool,
    pub herdr: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Branch,
    Base,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Draft {
    branch: String,
    base: String,
    carry: bool,
}

enum Stage {
    List,
    Create(Draft),
    Edit(Draft, Field, TextBuffer),
    Remove {
        root: PathBuf,
        branch: Option<String>,
    },
}

enum EntryKind {
    Checkout(PathBuf),
    Field(Field),
    Carry,
    Create,
    Remove(Changes),
}

struct Entry {
    label: String,
    detail: Option<String>,
    kind: EntryKind,
}

impl PickerItem for Entry {
    fn label(&self) -> &str {
        &self.label
    }

    fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

struct Flow {
    overview: WorktreeOverview,
    stage: Stage,
}

pub struct WorktreePicker {
    picker: ListPicker<Entry>,
    flow: Option<Flow>,
}

impl WorktreePicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new()
                .with_max_visible(MAX_VISIBLE)
                .with_width_percent(WIDTH_PERCENT),
            flow: None,
        }
    }

    pub fn open(&mut self, overview: WorktreeOverview, view: WorktreeView) {
        self.close();
        let (current, dirty) = (overview.current.clone(), overview.dirty);
        self.flow = Some(Flow {
            overview,
            stage: Stage::List,
        });
        self.show_list();
        match view {
            WorktreeView::List => {}
            WorktreeView::Create(branch) => self.show_create(branch),
            WorktreeView::Remove => self.show_removal(current, dirty),
        }
    }

    fn show_create(&mut self, branch: Option<String>) {
        let Some(flow) = &mut self.flow else {
            return;
        };
        flow.stage = Stage::Create(Draft {
            branch: branch.unwrap_or_default(),
            base: HEAD.into(),
            carry: false,
        });
        self.render_create(None);
    }

    pub fn show_removal(&mut self, root: PathBuf, dirty: bool) {
        let Some(flow) = &mut self.flow else {
            return;
        };
        let Some(checkout) = flow
            .overview
            .checkouts
            .iter()
            .find(|checkout| checkout.root == root)
        else {
            return;
        };
        if !checkout.linked {
            self.picker.set_error_text(Some(MAIN_NOT_REMOVABLE.into()));
            return;
        }
        let branch = checkout.branch.clone();
        let info = format!(
            "{} at {}\n{} {BRANCH_KEPT}",
            label(branch.as_deref(), &root),
            collapse_home(&root.to_string_lossy()),
            moving_back(checkout.sessions.len(), &flow.overview.main_root),
        );
        let entries = if dirty {
            vec![
                action(STASH_AND_REMOVE, EntryKind::Remove(Changes::Stash)),
                action(DISCARD_AND_REMOVE, EntryKind::Remove(Changes::Discard)),
            ]
        } else {
            vec![action(REMOVE, EntryKind::Remove(Changes::None))]
        };
        flow.stage = Stage::Remove { root, branch };
        self.picker.set_error_text(None);
        self.picker.set_info_text(Some(info));
        self.picker.set_footer_builder(confirm_footer);
        self.picker.open(entries, REMOVE_TITLE);
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
    }

    pub fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("worktree_picker", area);
        self.picker.view(frame, area)
    }

    pub fn handle_key(&mut self, event: KeyEvent) -> WorktreeAction {
        let Some(flow) = &mut self.flow else {
            return WorktreeAction::Consumed;
        };
        if event.code == KeyCode::Esc || key::QUIT.matches(event) {
            self.close();
            return WorktreeAction::Closed;
        }
        match &mut flow.stage {
            Stage::Edit(_, _, buffer) => {
                if event.code == KeyCode::Enter {
                    self.finish_edit();
                } else {
                    buffer.handle_key(event);
                    self.sync_edit();
                }
                return WorktreeAction::Consumed;
            }
            Stage::List => {
                if key::NEW_SESSION.matches(event) {
                    self.show_create(None);
                    return WorktreeAction::Consumed;
                }
                if key::RENAME_SESSION.matches(event) {
                    return WorktreeAction::Refresh;
                }
                if key::DELETE.matches(event) {
                    return match self.picker.selected_item().map(|entry| &entry.kind) {
                        Some(EntryKind::Checkout(root)) => {
                            WorktreeAction::InspectRemoval(root.clone())
                        }
                        _ => WorktreeAction::Consumed,
                    };
                }
            }
            Stage::Create(_) | Stage::Remove { .. } => {
                if event.code == KeyCode::Tab {
                    self.show_list();
                    return WorktreeAction::Consumed;
                }
                if matches!(flow.stage, Stage::Create(_))
                    && key::RELOCATION_USAGE.matches(event)
                    && self
                        .picker
                        .selected_item()
                        .is_some_and(|entry| matches!(entry.kind, EntryKind::Carry))
                {
                    self.toggle_carry();
                    return WorktreeAction::Consumed;
                }
                if !matches!(
                    event.code,
                    KeyCode::Enter
                        | KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::PageUp
                        | KeyCode::PageDown
                        | KeyCode::Home
                        | KeyCode::End
                ) {
                    return WorktreeAction::Consumed;
                }
            }
        }
        let action = self.picker.handle_key(event);
        self.map_action(action)
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        match self.flow.as_mut().map(|flow| &mut flow.stage) {
            Some(Stage::Edit(_, _, buffer)) => {
                buffer.insert_text(text);
                self.sync_edit();
                true
            }
            Some(Stage::List) => self.picker.handle_paste(text),
            Some(_) => true,
            None => false,
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> WorktreeAction {
        if matches!(
            self.flow.as_ref().map(|flow| &flow.stage),
            Some(Stage::Edit(..))
        ) {
            return match self.picker.handle_footer_mouse(event) {
                Some(key) => self.handle_key(key),
                None => WorktreeAction::Consumed,
            };
        }
        let action = self.picker.handle_mouse(event);
        self.map_action(action)
    }

    fn show_list(&mut self) {
        let Some(flow) = &mut self.flow else {
            return;
        };
        flow.stage = Stage::List;
        let overview = &flow.overview;
        let entries = overview
            .checkouts
            .iter()
            .map(|checkout| {
                let mut detail = vec![
                    collapse_home(&checkout.root.to_string_lossy()),
                    session_count(checkout.sessions.len()),
                ];
                if !checkout.linked {
                    detail.push(MAIN_CHECKOUT.into());
                }
                if checkout.root == overview.current {
                    detail.push(CURRENT.into());
                }
                Entry {
                    label: label(checkout.branch.as_deref(), &checkout.root),
                    detail: Some(detail.join(" · ")),
                    kind: EntryKind::Checkout(checkout.root.clone()),
                }
            })
            .collect();
        let hint = if overview.herdr {
            LIST_HINT_HERDR
        } else {
            LIST_HINT_GIT
        };
        let current = overview.current.clone();
        self.picker.set_error_text(None);
        self.picker.set_info_text(Some(hint.into()));
        self.picker.set_footer_builder(list_footer);
        self.picker.open(entries, LIST_TITLE);
        self.picker.select_item_by(
            |entry| matches!(&entry.kind, EntryKind::Checkout(root) if *root == current),
        );
    }

    fn render_create(&mut self, selected: Option<fn(&EntryKind) -> bool>) {
        let Some(Flow {
            overview,
            stage: Stage::Create(draft),
        }) = &self.flow
        else {
            return;
        };
        let branch = if draft.branch.is_empty() {
            GENERATED_BRANCH
        } else {
            &draft.branch
        };
        let mut entries = vec![
            field(BRANCH_LABEL, branch, Field::Branch),
            field(BASE_LABEL, &draft.base, Field::Base),
        ];
        let mut info = CREATE_HINT.to_owned();
        if overview.dirty {
            entries.push(Entry {
                label: format!(
                    "[{}] {CARRY_LABEL} ({} toggles)",
                    if draft.carry { "x" } else { " " },
                    key::RELOCATION_USAGE.label
                ),
                detail: None,
                kind: EntryKind::Carry,
            });
            info.push('\n');
            info.push_str(CARRY_HINT);
        }
        entries.push(action(CREATE, EntryKind::Create));
        self.picker.set_error_text(None);
        self.picker.set_info_text(Some(info));
        self.picker.set_footer_builder(create_footer);
        self.picker.open(entries, CREATE_TITLE);
        let selected = selected.unwrap_or(|kind| matches!(kind, EntryKind::Create));
        self.picker.select_item_by(|entry| selected(&entry.kind));
    }

    fn edit(&mut self, field: Field) {
        let Some(flow) = &mut self.flow else {
            return;
        };
        let Stage::Create(draft) = mem::replace(&mut flow.stage, Stage::List) else {
            return;
        };
        let (value, title, hint) = match field {
            Field::Branch => (draft.branch.clone(), BRANCH_TITLE, BRANCH_HINT),
            Field::Base => (draft.base.clone(), BASE_TITLE, BASE_HINT),
        };
        let mut buffer = TextBuffer::new(value);
        buffer.move_to_end();
        flow.stage = Stage::Edit(draft, field, buffer);
        self.picker.set_error_text(None);
        self.picker.set_info_text(Some(hint.into()));
        self.picker.set_footer_builder(edit_footer);
        self.picker.open(Vec::new(), title);
        self.sync_edit();
    }

    fn sync_edit(&mut self) {
        if let Some(Flow {
            stage: Stage::Edit(_, _, buffer),
            ..
        }) = &self.flow
        {
            self.picker.set_search_text(&buffer.value());
            self.picker.set_search_cursor(buffer.cursor_offset());
        }
    }

    fn finish_edit(&mut self) {
        let Some(flow) = &mut self.flow else {
            return;
        };
        let Stage::Edit(mut draft, field, buffer) = mem::replace(&mut flow.stage, Stage::List)
        else {
            return;
        };
        let value = buffer.value().trim().to_owned();
        match field {
            Field::Branch => draft.branch = value,
            Field::Base if value.is_empty() => draft.base = HEAD.into(),
            Field::Base => draft.base = value,
        }
        flow.stage = Stage::Create(draft);
        self.render_create(Some(match field {
            Field::Branch => |kind| matches!(kind, EntryKind::Field(Field::Branch)),
            Field::Base => |kind| matches!(kind, EntryKind::Field(Field::Base)),
        }));
    }

    fn toggle_carry(&mut self) {
        if let Some(Flow {
            stage: Stage::Create(draft),
            ..
        }) = &mut self.flow
        {
            draft.carry = !draft.carry;
            self.render_create(Some(|kind| matches!(kind, EntryKind::Carry)));
        }
    }

    fn map_action(&mut self, action: PickerAction<Entry>) -> WorktreeAction {
        match action {
            PickerAction::Close => {
                self.close();
                WorktreeAction::Closed
            }
            PickerAction::Select(entry) => match entry.kind {
                EntryKind::Checkout(root) => self.open_checkout(root),
                EntryKind::Field(field) => {
                    self.edit(field);
                    WorktreeAction::Consumed
                }
                EntryKind::Carry => {
                    self.toggle_carry();
                    WorktreeAction::Consumed
                }
                EntryKind::Create => self.confirm_create(),
                EntryKind::Remove(changes) => self.confirm_removal(changes),
            },
            PickerAction::Key(key) => self.handle_key(key),
            PickerAction::Consumed | PickerAction::Toggle(..) => WorktreeAction::Consumed,
        }
    }

    fn open_checkout(&mut self, root: PathBuf) -> WorktreeAction {
        if self
            .flow
            .as_ref()
            .is_some_and(|flow| flow.overview.current == root)
        {
            self.show_list();
            self.picker.set_error_text(Some(ALREADY_HERE.into()));
            return WorktreeAction::Consumed;
        }
        self.close();
        WorktreeAction::Open(root)
    }

    fn confirm_create(&mut self) -> WorktreeAction {
        let Some(Flow {
            overview,
            stage: Stage::Create(draft),
        }) = self.flow.take()
        else {
            return WorktreeAction::Consumed;
        };
        self.picker.close();
        WorktreeAction::Run(Request::Create(CreateRequest {
            session: overview.session,
            cwd: overview.cwd,
            source: overview.current,
            main_root: overview.main_root,
            branch: (!draft.branch.is_empty()).then_some(draft.branch),
            base: draft.base,
            carry: draft.carry && overview.dirty,
        }))
    }

    fn confirm_removal(&mut self, changes: Changes) -> WorktreeAction {
        let Some(Flow {
            overview,
            stage: Stage::Remove { root, branch },
        }) = self.flow.take()
        else {
            return WorktreeAction::Consumed;
        };
        self.picker.close();
        WorktreeAction::Run(Request::Remove(RemoveRequest {
            root,
            main_root: overview.main_root,
            branch,
            changes,
        }))
    }
}

impl Overlay for WorktreePicker {
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

fn field(name: &str, value: &str, field: Field) -> Entry {
    Entry {
        label: format!("{name}: {value}"),
        detail: None,
        kind: EntryKind::Field(field),
    }
}

fn action(label: &str, kind: EntryKind) -> Entry {
    Entry {
        label: label.into(),
        detail: None,
        kind,
    }
}

fn session_count(count: usize) -> String {
    match count {
        1 => "1 session".into(),
        count => format!("{count} sessions"),
    }
}

fn moving_back(count: usize, main_root: &Path) -> String {
    match count {
        0 => "No session works in it.".into(),
        count => format!(
            "{} move back to {}.",
            session_count(count),
            collapse_home(&main_root.to_string_lossy())
        ),
    }
}

fn list_footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "open"),
        Hint::bind(key::NEW_SESSION, "new"),
        Hint::bind(key::DELETE, "remove"),
        Hint::bind(key::RENAME_SESSION, "refresh"),
        Hint::bind(key::ESC, "close"),
    ]
}

fn create_footer() -> Vec<Hint> {
    vec![
        Hint::inert("↑↓", "select"),
        Hint::bind(key::ENTER, "activate"),
        Hint::bind(key::TAB, "worktrees"),
        Hint::bind(key::ESC, "cancel"),
    ]
}

fn edit_footer() -> Vec<Hint> {
    vec![
        Hint::bind(key::ENTER, "done"),
        Hint::bind(key::ESC, "cancel"),
    ]
}

fn confirm_footer() -> Vec<Hint> {
    vec![
        Hint::inert("↑↓", "select"),
        Hint::bind(key::ENTER, "confirm"),
        Hint::bind(key::TAB, "worktrees"),
        Hint::bind(key::ESC, "cancel"),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use caudra_agent::worktree::{Changes, CreateRequest, RemoveRequest, Request};
    use caudra_storage::id::CaudraId;
    use caudra_storage::worktrees::CheckoutSessions;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    use super::{
        ALREADY_HERE, HEAD, MAIN_NOT_REMOVABLE, USAGE, WorktreeAction, WorktreeOverview,
        WorktreePicker, WorktreeView,
    };
    use crate::components::keybindings::key;

    const MAIN: &str = "/work/app";
    const LINKED: &str = "/work/worktrees/app/login";
    const CWD: &str = "/work/app/src";
    const BRANCH: &str = "feature/login";
    const BASE: &str = "v1.2";

    fn checkout(root: &str, branch: &str, linked: bool) -> CheckoutSessions {
        CheckoutSessions {
            root: root.into(),
            branch: Some(branch.into()),
            linked,
            sessions: Vec::new(),
        }
    }

    fn overview(dirty: bool) -> WorktreeOverview {
        WorktreeOverview {
            session: CaudraId::generate(),
            cwd: CWD.into(),
            current: MAIN.into(),
            main_root: MAIN.into(),
            checkouts: vec![
                checkout(MAIN, "main", false),
                checkout(LINKED, BRANCH, true),
            ],
            dirty,
            herdr: false,
        }
    }

    fn opened(dirty: bool) -> (WorktreePicker, WorktreeOverview) {
        let overview = overview(dirty);
        let mut picker = WorktreePicker::new();
        picker.open(overview.clone(), WorktreeView::List);
        (picker, overview)
    }

    #[test_case("", Ok(WorktreeView::List) ; "list")]
    #[test_case("new", Ok(WorktreeView::Create(None)) ; "new_unnamed")]
    #[test_case(" new  feature/login ", Ok(WorktreeView::Create(Some(BRANCH.into()))) ; "new_named")]
    #[test_case("remove", Ok(WorktreeView::Remove) ; "remove")]
    #[test_case("delete", Err(USAGE) ; "unknown_subcommand")]
    #[test_case("remove now", Err(USAGE) ; "extra_argument")]
    fn worktree_arguments_choose_the_view(args: &str, expected: Result<WorktreeView, &str>) {
        assert_eq!(WorktreeView::parse(args), expected);
    }

    #[test]
    fn remove_view_confirms_the_current_linked_checkout() {
        let mut current = overview(false);
        current.current = LINKED.into();
        let mut picker = WorktreePicker::new();

        picker.open(current, WorktreeView::Remove);
        let action = picker.handle_key(press(KeyCode::Enter));

        let WorktreeAction::Run(Request::Remove(request)) = action else {
            panic!("expected a remove request, got {action:?}");
        };
        assert_eq!(request.root, PathBuf::from(LINKED));
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(picker: &mut WorktreePicker, text: &str) {
        for character in text.chars() {
            picker.handle_key(press(KeyCode::Char(character)));
        }
    }

    #[test]
    fn enter_opens_the_selected_checkout() {
        let (mut picker, _) = opened(false);

        picker.handle_key(press(KeyCode::Down));

        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            WorktreeAction::Open(root) if root == Path::new(LINKED)
        ));
        assert!(!picker.is_open());
    }

    #[test]
    fn the_current_checkout_is_already_open() {
        let (mut picker, _) = opened(false);

        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            WorktreeAction::Consumed
        ));
        assert!(picker.is_open());
        assert_eq!(picker.picker.error_text(), Some(ALREADY_HERE));
    }

    #[test]
    fn a_new_worktree_is_named_and_based_as_typed() {
        let (mut picker, overview) = opened(false);
        picker.handle_key(key::NEW_SESSION.to_key_event());

        picker.handle_key(press(KeyCode::Up));
        picker.handle_key(press(KeyCode::Up));
        picker.handle_key(press(KeyCode::Enter));
        type_text(&mut picker, BRANCH);
        picker.handle_key(press(KeyCode::Enter));
        picker.handle_key(press(KeyCode::Down));
        picker.handle_key(press(KeyCode::Enter));
        for _ in HEAD.chars() {
            picker.handle_key(press(KeyCode::Backspace));
        }
        type_text(&mut picker, BASE);
        picker.handle_key(press(KeyCode::Enter));
        picker.handle_key(press(KeyCode::Down));
        let action = picker.handle_key(press(KeyCode::Enter));

        let WorktreeAction::Run(Request::Create(request)) = action else {
            panic!("expected a create request, got {action:?}");
        };
        assert_eq!(
            request,
            CreateRequest {
                session: overview.session,
                cwd: CWD.into(),
                source: MAIN.into(),
                main_root: MAIN.into(),
                branch: Some(BRANCH.into()),
                base: BASE.into(),
                carry: false,
            }
        );
    }

    #[test_case(false, false ; "clean_checkout_offers_nothing_to_carry")]
    #[test_case(true, true ; "dirty_checkout_carries_when_toggled")]
    fn uncommitted_changes_carry_only_when_there_are_some(dirty: bool, carried: bool) {
        let (mut picker, _) = opened(dirty);
        picker.show_create(None);
        if dirty {
            picker.handle_key(press(KeyCode::Up));
            picker.handle_key(key::RELOCATION_USAGE.to_key_event());
            picker.handle_key(press(KeyCode::Down));
        }

        let action = picker.handle_key(press(KeyCode::Enter));

        let WorktreeAction::Run(Request::Create(request)) = action else {
            panic!("expected a create request, got {action:?}");
        };
        assert_eq!(request.carry, carried);
        assert_eq!(request.branch, None);
        assert_eq!(request.base, HEAD);
    }

    #[test_case(false, Changes::None ; "clean")]
    #[test_case(true, Changes::Stash ; "dirty_stashes_first")]
    fn removing_a_worktree_says_what_happens_to_its_changes(dirty: bool, changes: Changes) {
        let (mut picker, _) = opened(false);
        picker.handle_key(press(KeyCode::Down));

        let inspect = picker.handle_key(key::DELETE.to_key_event());
        let WorktreeAction::InspectRemoval(root) = inspect else {
            panic!("expected an inspection, got {inspect:?}");
        };
        picker.show_removal(root, dirty);
        let action = picker.handle_key(press(KeyCode::Enter));

        let WorktreeAction::Run(Request::Remove(request)) = action else {
            panic!("expected a remove request, got {action:?}");
        };
        assert_eq!(
            request,
            RemoveRequest {
                root: LINKED.into(),
                main_root: MAIN.into(),
                branch: Some(BRANCH.into()),
                changes,
            }
        );
    }

    #[test]
    fn the_main_checkout_is_never_removed() {
        let (mut picker, _) = opened(false);

        picker.show_removal(MAIN.into(), false);

        assert_eq!(picker.picker.error_text(), Some(MAIN_NOT_REMOVABLE));
        assert!(matches!(
            picker.handle_key(press(KeyCode::Enter)),
            WorktreeAction::Consumed
        ));
    }

    #[test]
    fn refresh_and_escape_leave_the_decision_to_the_owner() {
        let (mut picker, _) = opened(false);

        assert!(matches!(
            picker.handle_key(key::RENAME_SESSION.to_key_event()),
            WorktreeAction::Refresh
        ));
        assert!(matches!(
            picker.handle_key(press(KeyCode::Esc)),
            WorktreeAction::Closed
        ));
        assert!(!picker.is_open());
    }
}
