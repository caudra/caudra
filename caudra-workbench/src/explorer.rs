//! The explorer's stacked sections. The project comes first and is always
//! there. Caudra's own directories stand under it once the host names them,
//! as do the folders added by hand, each in a tree of its own that is read
//! from this machine whatever the session is bound to.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;

use crate::editor::TabLabel;
use crate::fs::backend::WorkbenchPath;
use crate::fs::ops::{self, OpsError};
use crate::fs::tree::{HostMount, Row, Tree};
use crate::fs::watch::HostWatch;
use crate::menu::{Action as MenuAction, Menu, RowOffer};
use crate::scm::MIN_SECTION_ROWS;
use crate::{
    Drag, Focus, InputKind, OpenPurpose, SECTION_STEP, SectionLayout, Stack, Workbench,
    WorkbenchAction, keys, view,
};

const PROJECT_TITLE: &str = "PROJECT";
const CAUDRA_TITLE: &str = "CAUDRA";
const FOLDERS_TITLE: &str = "FOLDERS";
/// The project's height while a section under it is open as well. Whichever
/// open section is lowest takes the rest, so this only bites once one is.
const DEFAULT_PROJECT_ROWS: u16 = 12;
/// How tall a section of this machine's folders opens the first time. It
/// starts folded, so the project keeps the pane until asked otherwise.
const DEFAULT_HOST_ROWS: u16 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExplorerSection {
    Project,
    Caudra,
    Folders,
}

impl ExplorerSection {
    pub(crate) const ALL: [Self; 3] = [Self::Project, Self::Caudra, Self::Folders];
    pub(crate) const COUNT: usize = Self::ALL.len();

    pub(crate) const fn index(self) -> usize {
        self as usize
    }
}

/// How the sections are arranged when nothing was stored, in stacking order.
pub(crate) fn default_frames() -> [SectionLayout; ExplorerSection::COUNT] {
    let host = SectionLayout {
        height: DEFAULT_HOST_ROWS,
        collapsed: true,
    };
    [
        SectionLayout {
            height: DEFAULT_PROJECT_ROWS,
            collapsed: false,
        },
        host.clone(),
        host,
    ]
}

/// A folder added by hand reads as its own name, the way the project does.
fn folder_mount(root: PathBuf) -> HostMount {
    let label = root.file_name().map_or_else(
        || root.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    HostMount {
        label,
        root,
        managed: false,
    }
}

/// The sections beside the project and where the cursor rests among them.
/// The project's own tree stays the workbench's, since everything bound to
/// the session reads it.
pub(crate) struct Explorer {
    caudra: Tree,
    folders: Tree,
    active: ExplorerSection,
    /// Whether the cursor is on the active section's header rather than on a
    /// row of its tree.
    on_header: bool,
    frames: [SectionLayout; ExplorerSection::COUNT],
}

impl Default for Explorer {
    fn default() -> Self {
        Self {
            caudra: Tree::mounts(Vec::new(), false),
            folders: Tree::mounts(Vec::new(), false),
            active: ExplorerSection::Project,
            on_header: false,
            frames: default_frames(),
        }
    }
}

impl Explorer {
    /// The tree `section` lists, handed the project's so the caller keeps its
    /// other fields free.
    pub(crate) fn tree<'a>(&'a self, project: &'a Tree, section: ExplorerSection) -> &'a Tree {
        match section {
            ExplorerSection::Project => project,
            ExplorerSection::Caudra => &self.caudra,
            ExplorerSection::Folders => &self.folders,
        }
    }

    fn tree_mut<'a>(&'a mut self, project: &'a mut Tree, section: ExplorerSection) -> &'a mut Tree {
        match section {
            ExplorerSection::Project => project,
            ExplorerSection::Caudra => &mut self.caudra,
            ExplorerSection::Folders => &mut self.folders,
        }
    }

    /// A section with nothing named in it is not drawn at all.
    pub(crate) fn is_shown(&self, section: ExplorerSection) -> bool {
        match section {
            ExplorerSection::Project => true,
            ExplorerSection::Caudra => !self.caudra.mount_list().is_empty(),
            ExplorerSection::Folders => !self.folders.mount_list().is_empty(),
        }
    }

    /// Whether anything stands beside the project. A project on its own is
    /// drawn the way it always was, with no header to fold it by.
    pub(crate) fn is_stacked(&self) -> bool {
        self.is_shown(ExplorerSection::Caudra) || self.is_shown(ExplorerSection::Folders)
    }

    pub(crate) fn active(&self) -> ExplorerSection {
        self.active
    }

    pub(crate) fn on_header(&self) -> bool {
        self.on_header
    }

    /// Only a stacked explorer folds: a lone project has no header to unfold
    /// it again.
    pub(crate) fn is_collapsed(&self, section: ExplorerSection) -> bool {
        self.is_stacked() && self.frames[section.index()].collapsed
    }

    /// What [`crate::layout_sections`] is asked for on `section`'s behalf.
    pub(crate) fn wanted(&self, section: ExplorerSection) -> Option<(u16, bool)> {
        let frame = &self.frames[section.index()];
        self.is_shown(section)
            .then_some((frame.height, frame.collapsed))
    }

    pub(crate) fn title(section: ExplorerSection, project: &str) -> String {
        match section {
            ExplorerSection::Project if project.is_empty() => PROJECT_TITLE.to_owned(),
            ExplorerSection::Project => project.to_uppercase(),
            ExplorerSection::Caudra => CAUDRA_TITLE.to_owned(),
            ExplorerSection::Folders => FOLDERS_TITLE.to_owned(),
        }
    }

    fn select_row(&mut self, section: ExplorerSection) {
        self.active = section;
        self.on_header = false;
    }

    fn select_header(&mut self, section: ExplorerSection) {
        self.active = section;
        self.on_header = self.is_stacked();
    }

    /// Folding the section the cursor is in leaves the cursor on its header,
    /// since the row it was on is no longer drawn.
    pub(crate) fn toggle_collapsed(&mut self, section: ExplorerSection) {
        let frame = &mut self.frames[section.index()];
        frame.collapsed = !frame.collapsed;
        if frame.collapsed && self.active == section {
            self.on_header = true;
        }
    }

    pub(crate) fn set_height(&mut self, section: ExplorerSection, height: u16) {
        self.frames[section.index()].height = height.max(MIN_SECTION_ROWS);
    }

    /// The nearest shown section `step` away, up when negative.
    fn neighbour(&self, section: ExplorerSection, step: isize) -> Option<ExplorerSection> {
        let mut index = section.index();
        loop {
            index = index.checked_add_signed(step)?;
            let candidate = *ExplorerSection::ALL.get(index)?;
            if self.is_shown(candidate) {
                return Some(candidate);
            }
        }
    }

    /// Puts the cursor back on the project when the section it was in has
    /// gone, and off any header once there is none to be on.
    fn settle(&mut self) {
        if !self.is_shown(self.active) {
            self.select_row(ExplorerSection::Project);
        }
        self.on_header &= self.is_stacked();
    }

    pub(crate) fn saved(&self) -> Vec<SectionLayout> {
        self.frames.to_vec()
    }

    /// Zipped rather than indexed, like the source control sections, so a
    /// list written by a build that knew a different number keeps the
    /// defaults for whatever it does not name.
    pub(crate) fn restore(&mut self, frames: &[SectionLayout]) {
        for (frame, stored) in self.frames.iter_mut().zip(frames) {
            frame.height = stored.height.max(MIN_SECTION_ROWS);
            frame.collapsed = stored.collapsed;
        }
    }

    pub(crate) fn set_show_hidden(&mut self, show_hidden: bool) {
        self.caudra.set_show_hidden(show_hidden);
        self.folders.set_show_hidden(show_hidden);
    }

    pub(crate) fn reload(&mut self) {
        self.caudra.reload();
        self.folders.reload();
    }

    /// The folders added by hand, in the order they were added.
    pub(crate) fn folder_roots(&self) -> Vec<PathBuf> {
        self.folders
            .mount_list()
            .iter()
            .map(|mount| mount.root.clone())
            .collect()
    }

    /// A folder that has gone stays named, so it is listed again once it is
    /// back. The tree leaves out a mount with nothing on disk.
    pub(crate) fn set_folders(&mut self, roots: Vec<PathBuf>) {
        self.folders
            .set_mounts(roots.into_iter().map(folder_mount).collect());
        self.settle();
    }

    /// The mount a file of this machine is listed under, in any section.
    pub(crate) fn mount_of(&self, path: &Path) -> Option<&HostMount> {
        self.caudra
            .mount_of(path)
            .or_else(|| self.folders.mount_of(path))
    }

    /// The directories a watch has to cover for these trees to stay true:
    /// every mount, and every folder open under one.
    fn watched_dirs(&self) -> impl Iterator<Item = &Path> {
        [&self.caudra, &self.folders].into_iter().flat_map(|tree| {
            tree.mount_list()
                .iter()
                .map(|mount| mount.root.as_path())
                .chain(tree.open_dirs())
        })
    }
}

impl Workbench {
    /// Names the directories of this machine the Caudra section lists. The
    /// same set again reads nothing, so the host can say it whenever the
    /// session might have moved.
    pub fn set_caudra_mounts(&mut self, mounts: Vec<HostMount>) {
        self.explorer.caudra.set_show_hidden(self.show_hidden);
        self.explorer.caudra.set_mounts(mounts);
        self.explorer.settle();
    }

    /// Opens a file of this machine. With a label the file is one the host
    /// keeps for itself, such as the plan, and opens under that name. Either
    /// way the cursor goes no further than the tab and the sidebar stays on
    /// whatever it was showing, because the host is the one that asked.
    pub fn open_host_file(&mut self, path: &Path, label: Option<TabLabel>, preview: bool) {
        self.open_local(path, preview);
        if let Some(label) = label {
            self.editor
                .label(&WorkbenchPath::Local(path.to_path_buf()), label);
        }
    }

    /// Asks for a folder of this machine to list under the project.
    pub(crate) fn ask_for_folder(&mut self) {
        self.ask_for_name(InputKind::AddFolder, WorkbenchPath::Local(PathBuf::new()));
    }

    /// Lists the folder `typed` names in a section of its own, opened to show
    /// what it holds, or points at it where it is listed already: in the
    /// project, under one of Caudra's folders, or as a folder added before.
    pub(crate) fn add_folder(&mut self, typed: &str) -> Result<(), OpsError> {
        let folder = ops::folder(typed)?;
        let path = WorkbenchPath::Local(folder.clone());
        let added = self.listing_section(&path).is_none();
        if added {
            let mut roots = self.explorer.folder_roots();
            roots.push(folder);
            self.explorer.set_folders(roots);
        }
        self.reveal_in_explorer(&path, true);
        if added {
            self.explorer.folders.toggle_selected();
        }
        Ok(())
    }

    /// Takes a folder added by hand back out. What is in it stays on disk,
    /// and so do the tabs open on it.
    pub(crate) fn remove_folder(&mut self, root: &Path) {
        let mut roots = self.explorer.folder_roots();
        roots.retain(|kept| kept != root);
        self.explorer.set_folders(roots);
    }

    /// The first shown section listing `path`, the project first.
    fn listing_section(&self, path: &WorkbenchPath) -> Option<ExplorerSection> {
        ExplorerSection::ALL.into_iter().find(|section| {
            self.explorer.is_shown(*section) && self.section_tree(*section).contains(path)
        })
    }

    pub(crate) fn section_tree(&self, section: ExplorerSection) -> &Tree {
        self.explorer.tree(&self.tree, section)
    }

    pub(crate) fn section_tree_mut(&mut self, section: ExplorerSection) -> &mut Tree {
        self.explorer.tree_mut(&mut self.tree, section)
    }

    /// The path under the explorer's cursor, which is nothing while it rests
    /// on a header.
    pub(crate) fn explorer_selection(&self) -> Option<WorkbenchPath> {
        if self.explorer.on_header() {
            return None;
        }
        let tree = self.section_tree(self.explorer.active());
        tree.selected().map(|row| row.path.clone())
    }

    /// The rows a page key moves by, which is the body of the section the
    /// cursor is in rather than the whole sidebar.
    pub(crate) fn explorer_rows(&self) -> usize {
        self.panes.explorer[self.explorer.active().index()]
            .body
            .height as usize
    }

    /// Points the explorer at `path` in whichever section lists it, the
    /// project first. Asked for outright, a folded section unfolds to show
    /// it; following the editor around, a folded section stays folded.
    pub(crate) fn reveal_in_explorer(&mut self, path: &WorkbenchPath, unfold: bool) {
        let Some(section) = self.listing_section(path) else {
            return;
        };
        if self.explorer.is_collapsed(section) {
            if !unfold {
                return;
            }
            self.explorer.toggle_collapsed(section);
        }
        self.section_tree_mut(section).reveal_workbench_path(path);
        self.explorer.select_row(section);
    }

    pub(crate) fn explorer_key(&mut self, key: KeyEvent) -> WorkbenchAction {
        let section = self.explorer.active();
        if keys::COLLAPSE_ALL.matches(key) {
            self.section_tree_mut(section).collapse_all();
            return WorkbenchAction::Consumed;
        }
        if self.explorer.on_header() {
            match key.code {
                KeyCode::Up => self.step_explorer(-1),
                KeyCode::Down => self.step_explorer(1),
                KeyCode::Left if !self.explorer.is_collapsed(section) => {
                    self.explorer.toggle_collapsed(section);
                }
                KeyCode::Right if self.explorer.is_collapsed(section) => {
                    self.explorer.toggle_collapsed(section);
                }
                KeyCode::Enter => self.explorer.toggle_collapsed(section),
                _ => {}
            }
            return WorkbenchAction::Consumed;
        }
        let page = self.explorer_rows().max(1) as isize;
        let tree = self.section_tree_mut(section);
        match key.code {
            KeyCode::Up => self.step_explorer(-1),
            KeyCode::Down => self.step_explorer(1),
            KeyCode::PageUp => tree.move_selection(-page),
            KeyCode::PageDown => tree.move_selection(page),
            KeyCode::Home => tree.select_first(),
            KeyCode::End => tree.select_last(),
            KeyCode::Left => tree.collapse_or_parent(),
            KeyCode::Right | KeyCode::Enter => return self.enter_selected(),
            _ => {}
        }
        WorkbenchAction::Consumed
    }

    /// One row up or down, walking off the end of a section onto the next
    /// header and from a header into the rows under it. A lone project has
    /// no headers, so its rows are all there is.
    fn step_explorer(&mut self, step: isize) {
        let section = self.explorer.active();
        if !self.explorer.is_stacked() {
            self.tree.move_selection(step);
            return;
        }
        let tree = self.section_tree(section);
        let listed = !self.explorer.is_collapsed(section) && !tree.rows().is_empty();
        let selected = tree.selected_index();
        let last = tree.rows().len().saturating_sub(1);
        match (step > 0, self.explorer.on_header()) {
            (true, true) if listed => {
                self.section_tree_mut(section).select_first();
                self.explorer.select_row(section);
            }
            (true, false) if selected < last => self.section_tree_mut(section).move_selection(1),
            (true, _) => {
                if let Some(below) = self.explorer.neighbour(section, 1) {
                    self.explorer.select_header(below);
                }
            }
            (false, false) if selected > 0 => self.section_tree_mut(section).move_selection(-1),
            (false, false) => self.explorer.select_header(section),
            (false, true) => {
                let Some(above) = self.explorer.neighbour(section, -1) else {
                    return;
                };
                if self.explorer.is_collapsed(above) || self.section_tree(above).rows().is_empty() {
                    self.explorer.select_header(above);
                } else {
                    self.section_tree_mut(above).select_last();
                    self.explorer.select_row(above);
                }
            }
        }
    }

    /// Right and Enter mean the same thing on a row: step into the directory,
    /// or open the file.
    fn enter_selected(&mut self) -> WorkbenchAction {
        match self
            .section_tree_mut(self.explorer.active())
            .toggle_selected()
        {
            true => WorkbenchAction::Consumed,
            false => self.open_selected(),
        }
    }

    pub(crate) fn open_selected(&mut self) -> WorkbenchAction {
        match self.explorer_selection() {
            Some(path) => self.open_row(&path, false),
            None => WorkbenchAction::Consumed,
        }
    }

    /// Puts the selected file up without leaving the tree, which is what one
    /// click does. The cursor stays in the sidebar so the next arrow key walks
    /// on from where it was.
    pub(crate) fn preview_selected(&mut self) -> WorkbenchAction {
        match self.explorer_selection() {
            Some(path) => self.open_row(&path, true),
            None => WorkbenchAction::Consumed,
        }
    }

    /// Opens a row's file. One of Caudra's own goes to the host instead,
    /// which knows a plan from a policy file and opens each as what it is.
    pub(crate) fn open_row(&mut self, path: &WorkbenchPath, preview: bool) -> WorkbenchAction {
        if let Some(local) = path.local()
            && self.explorer.caudra.contains(path)
        {
            return WorkbenchAction::OpenHostFile {
                path: local.to_path_buf(),
                preview,
            };
        }
        match (path, preview) {
            (WorkbenchPath::Local(local), _) => self.open_local(local, preview),
            (_, true) => self.open_workbench_path(path, OpenPurpose::Preview),
            (_, false) => self.open_workbench_path(path, OpenPurpose::Open),
        }
        WorkbenchAction::Consumed
    }

    fn open_local(&mut self, path: &Path, preview: bool) {
        if !preview {
            self.open_path(path);
            return;
        }
        match self.editor.preview(path, self.theme_generation) {
            Ok(()) => {
                // The tree is already on this row, but source control is not.
                self.reveal_active();
                self.follow_cursor();
            }
            Err(error) => self.flash = Some(error.to_string()),
        }
    }

    /// A press on the explorer, or `None` when it landed on none of its
    /// sections. A header press is armed rather than acted on: the same press
    /// starts a resize, and only the release can tell the two apart.
    pub(crate) fn press_explorer(&mut self, at: (u16, u16), clicks: u8) -> Option<WorkbenchAction> {
        if let Some(section) = self.explorer_header_under(at) {
            self.focus = Focus::Sidebar;
            self.explorer.select_header(section);
            self.drag = Drag::Section(Stack::Explorer, section.index());
            return Some(WorkbenchAction::Consumed);
        }
        let (section, body) = self.explorer_body_under(at)?;
        if view::on_menu_mark(at.0, body.x) {
            self.open_menu_at(at);
            return Some(WorkbenchAction::Consumed);
        }
        Some(self.press_explorer_row(section, (at.1 - body.y) as usize, clicks))
    }

    /// One click on a file only previews it: the tab stays until the next
    /// single click takes it over, so walking a tree leaves no trail of tabs
    /// behind. Two clicks keep it.
    fn press_explorer_row(
        &mut self,
        section: ExplorerSection,
        offset: usize,
        clicks: u8,
    ) -> WorkbenchAction {
        self.focus = Focus::Sidebar;
        let tree = self.section_tree_mut(section);
        let row = tree.scroll() + offset;
        tree.select_index(row);
        // Selecting refuses a row past the end, so the empty space under a
        // short list opens nothing rather than whatever the cursor happened
        // to be left on.
        if tree.selected_index() != row {
            return WorkbenchAction::Consumed;
        }
        let folder = tree.selected().is_some_and(Row::is_dir);
        self.explorer.select_row(section);
        match folder {
            true => {
                self.section_tree_mut(section).toggle_selected();
                WorkbenchAction::Consumed
            }
            false if clicks == 1 => self.preview_selected(),
            false => self.open_selected(),
        }
    }

    fn explorer_header_under(&self, at: (u16, u16)) -> Option<ExplorerSection> {
        self.panes
            .explorer
            .iter()
            .position(|rects| rects.header.contains(at.into()))
            .map(|index| ExplorerSection::ALL[index])
    }

    /// The section body under `at` and the rows it was drawn in.
    fn explorer_body_under(&self, at: (u16, u16)) -> Option<(ExplorerSection, Rect)> {
        self.panes
            .explorer
            .iter()
            .position(|rects| rects.body.contains(at.into()))
            .map(|index| (ExplorerSection::ALL[index], self.panes.explorer[index].body))
    }

    /// The section whose body spans `row`, scrollbar column included, which
    /// is what a wheel turn answers to.
    pub(crate) fn explorer_body_at_row(&self, row: u16) -> Option<(ExplorerSection, Rect)> {
        self.panes
            .explorer
            .iter()
            .position(|rects| (rects.body.y..rects.body.bottom()).contains(&row))
            .map(|index| (ExplorerSection::ALL[index], self.panes.explorer[index].body))
    }

    /// The menu for the row or header under `at`, selecting it first.
    pub(crate) fn explorer_menu_at(&mut self, at: (u16, u16)) {
        if let Some(section) = self.explorer_header_under(at) {
            self.focus = Focus::Sidebar;
            self.explorer.select_header(section);
            self.menu = Some(Menu::for_header(at));
            return;
        }
        let Some((section, body)) = self.explorer_body_under(at) else {
            return;
        };
        let tree = self.section_tree_mut(section);
        let row = tree.scroll() + (at.1 - body.y) as usize;
        tree.select_index(row);
        // Selecting refuses a row past the end, so the air under a short tree
        // opens nothing rather than a menu for whatever was last selected.
        if tree.selected_index() != row {
            return;
        }
        self.focus = Focus::Sidebar;
        self.explorer.select_row(section);
        self.menu = self.row_menu(section, at);
    }

    /// The menu for wherever the cursor already is, which is how the keyboard
    /// reaches it.
    pub(crate) fn explorer_menu(&mut self) {
        let section = self.explorer.active();
        if self.explorer.on_header() {
            let header = self.panes.explorer[section.index()].header;
            self.menu = Some(Menu::for_header((header.x, header.y)));
            return;
        }
        let rows = self.panes.explorer[section.index()].body;
        let tree = self.section_tree(section);
        let offset = tree.selected_index().saturating_sub(tree.scroll());
        self.menu = self.row_menu(section, (rows.x, rows.y + offset as u16));
    }

    /// A row a store owns offers nothing that would rename its files behind
    /// its back, and a mount's own row cannot be renamed or deleted, since
    /// what it names is the host's choice, or the reader's for a folder they
    /// added, which they take out again instead. A file of this machine is
    /// never mentioned in a workspace session, whose agent could not read it.
    fn row_menu(&self, section: ExplorerSection, at: (u16, u16)) -> Option<Menu> {
        let tree = self.section_tree(section);
        let row = tree.selected()?;
        let managed = tree.is_managed(&row.path);
        let mount = tree.is_mount_root(&row.path);
        let offer = RowOffer {
            create: !managed,
            mutate: !managed && !mount,
            mention: !self.is_host_file(&row.path),
            remove: mount && section == ExplorerSection::Folders,
        };
        Some(Menu::for_row(row, at, &offer))
    }

    pub(crate) fn run_on_header(&mut self, action: MenuAction) -> WorkbenchAction {
        if action == MenuAction::AddFolder {
            self.ask_for_folder();
        }
        WorkbenchAction::Consumed
    }

    /// `Leader d` asks for a folder to add, and `Leader ↑`/`↓` move the
    /// border under the section the cursor is in.
    pub(crate) fn explorer_leader(&mut self, key: KeyEvent) -> bool {
        if keys::ADD_FOLDER.matches(key) {
            self.ask_for_folder();
            return true;
        }
        let step = if keys::GROW_SECTION.matches(key) {
            SECTION_STEP
        } else if keys::SHRINK_SECTION.matches(key) {
            -SECTION_STEP
        } else {
            return false;
        };
        if !self.explorer.is_stacked() {
            return false;
        }
        let section = self.explorer.active();
        let height = self.explorer.frames[section.index()]
            .height
            .saturating_add_signed(step);
        self.explorer.set_height(section, height);
        true
    }

    /// Starts the watch over this machine's own folders, which the workbench
    /// keeps for as long as it is up. A failure leaves them to a refresh, as
    /// the project's own watch does.
    pub(crate) fn watch_host(&mut self) {
        if self.host_watch.is_none() {
            self.host_watch = HostWatch::start();
        }
    }

    /// Folds what moved in this machine's own folders into the panes: tabs
    /// catch up or raise a conflict, and the sections listing them reread.
    /// None of it is the project's, so source control is left alone.
    pub(crate) fn absorb_host_changes(&mut self) -> bool {
        if self.host_watch.is_none() {
            return false;
        }
        let dirs = self.host_dirs();
        let Some(watch) = &mut self.host_watch else {
            return false;
        };
        watch.sync(&dirs);
        let changes = watch.drain();
        if changes.is_empty() {
            return false;
        }
        for path in &changes.files {
            self.reload_tab(path);
        }
        if changes.structural {
            self.explorer.reload();
        }
        true
    }

    /// Every directory of this machine whose entries something on screen
    /// stands for: the sections' own, and the folders holding open files
    /// that the project's watch does not cover.
    fn host_dirs(&self) -> HashSet<PathBuf> {
        let tabs = self
            .editor
            .tabs()
            .iter()
            .filter(|tab| tab.is_file())
            .filter_map(|tab| tab.path.local())
            .filter(|path| {
                path.is_absolute()
                    && (self.remote_backend.is_some()
                        || self.root.as_os_str().is_empty()
                        || !path.starts_with(&self.root))
            })
            .filter_map(Path::parent);
        self.explorer
            .watched_dirs()
            .chain(tabs)
            .map(Path::to_path_buf)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use crossterm::event::KeyCode;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{ExplorerSection, default_frames};
    use crate::fs::backend::WorkbenchPath;
    use crate::fs::ops::OpsError;
    use crate::fs::tree::HostMount;
    use crate::menu::{Action as MenuAction, Item as MenuItem};
    use crate::tests::{
        assert_still_bound, bound_beside_host, click, drag, key, paint, press, project, release,
        rewrite, row_body, tick_until, wheel,
    };
    use crate::{
        Focus, InputKind, Layout, SCROLL_LINES, SectionLayout, SectionRect, Workbench,
        WorkbenchAction, WorkbenchStyles, keys,
    };

    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 24;
    const CONFIG: &str = "Config";
    const PLANS: &str = "Plans";
    const CONFIG_DIR: &str = "config";
    const PLANS_DIR: &str = "plans";
    const CONFIG_FILE: &str = "config/caudra.toml";
    const PROMPTS_DIR: &str = "config/prompts";
    const PROMPT_FILE: &str = "config/prompts/main.md";
    const PLAN_FILE: &str = "plans/plan.md";
    const LAST_PROJECT_FILE: &str = "a.txt";
    const NEW_FILE: &str = "late.toml";
    const HOST_TEXT: &str = "first\n";
    const HOST_REWRITE: &str = "second\n";
    /// Enough rows that no section in a [`HEIGHT`] terminal shows them all.
    const CROWD: usize = 30;

    const NOT_STACKED: &str = "the Caudra section should stand under the project";
    const STILL_STACKED: &str = "with nothing named, the project should have the pane alone";
    const WRONG_FOLD: &str = "the section is folded the wrong way";
    const WRONG_HEIGHT: &str = "the border moved the wrong section by the wrong amount";
    const WRONG_SCROLL: &str = "the wheel scrolled a section it was not over";
    const WRONG_CURSOR: &str = "the explorer's cursor is not where the keys put it";
    const NOT_HANDED_OVER: &str = "a Caudra file should go to the host to open";
    const WRONG_OFFER: &str = "the row's menu offers the wrong things";
    const LAYOUT_LOST: &str = "the explorer's layout did not survive a round trip";
    const WRONG_REVEAL: &str = "reveal unfolded a section it should have left alone";
    const SECTION_STALE: &str = "a file made under a mount never reached the Caudra section";
    const TAB_STALE: &str = "a write to an open file of this machine never reached its tab";
    const ADDED_DIR: &str = "added";
    const SECOND_DIR: &str = "second";
    const NOTE: &str = "notes.md";
    const PROJECT_DIR: &str = "sub";
    const RELATIVE: &str = "relative/folder";
    const NOT_ADDED: &str = "the folder should stand in a section of its own";
    const ADDED_TWICE: &str = "a folder listed already should be shown where it is instead";
    const PROMPT_LOST: &str = "a refused folder should leave the prompt up with the reason";
    const NOT_REMOVED: &str = "the folder should leave the explorer and nothing else";
    const FOLDERS_LOST: &str = "the added folders did not survive a round trip";
    const NO_HEADER_MENU: &str = "a header's menu should offer to add a folder";

    /// The project from [`project`] beside a folder of this machine holding a
    /// config directory and a store's plans, named as the Caudra section.
    fn caudra_project() -> (TempDir, TempDir, Workbench) {
        let (dir, mut workbench) = project();
        let host = TempDir::new().expect("a temporary directory");
        fs::create_dir_all(host.path().join(PROMPTS_DIR)).expect("a config directory");
        fs::write(host.path().join(CONFIG_FILE), HOST_TEXT).expect("a config file");
        fs::write(host.path().join(PROMPT_FILE), HOST_TEXT).expect("a prompt");
        fs::create_dir(host.path().join(PLANS_DIR)).expect("a plans directory");
        fs::write(host.path().join(PLAN_FILE), HOST_TEXT).expect("a plan");
        workbench.set_caudra_mounts(mounts(host.path()));
        paint(&mut workbench, WIDTH, HEIGHT);
        (dir, host, workbench)
    }

    fn mounts(host: &Path) -> Vec<HostMount> {
        vec![
            HostMount {
                label: CONFIG.to_owned(),
                root: host.join(CONFIG_DIR),
                managed: false,
            },
            HostMount {
                label: PLANS.to_owned(),
                root: host.join(PLANS_DIR),
                managed: true,
            },
        ]
    }

    fn host_path(host: &TempDir, relative: &str) -> WorkbenchPath {
        WorkbenchPath::Local(host.path().join(relative))
    }

    fn rects(workbench: &Workbench, section: ExplorerSection) -> SectionRect {
        workbench.panes.explorer[section.index()]
    }

    fn names(workbench: &Workbench, section: ExplorerSection) -> Vec<String> {
        workbench
            .section_tree(section)
            .rows()
            .iter()
            .map(|row| row.name.clone())
            .collect()
    }

    fn cursor(workbench: &Workbench) -> (ExplorerSection, bool) {
        (workbench.explorer.active(), workbench.explorer.on_header())
    }

    fn unfold_caudra(workbench: &mut Workbench) {
        workbench.explorer.toggle_collapsed(ExplorerSection::Caudra);
        paint(workbench, WIDTH, HEIGHT);
    }

    /// Clicks the row the explorer's cursor is on, `clicks` times over.
    fn click_selected(workbench: &mut Workbench, clicks: u8) -> WorkbenchAction {
        let section = workbench.explorer.active();
        let body = rects(workbench, section).body;
        let tree = workbench.section_tree(section);
        let row = body.y + (tree.selected_index() - tree.scroll()) as u16;
        let column = row_body(body);
        let mut action = WorkbenchAction::Consumed;
        for _ in 0..clicks {
            action = workbench.handle_mouse(click(column, row));
            workbench.handle_mouse(release(column, row));
        }
        action
    }

    /// The one path every spelling of `path` shares, which is what an added
    /// folder is listed under.
    fn canonical(path: &Path) -> PathBuf {
        path.canonicalize().expect("a real path")
    }

    /// Types `typed` into the prompt the chord puts up, and takes it.
    fn add_folder(workbench: &mut Workbench, typed: &Path) {
        workbench.focus = Focus::Sidebar;
        workbench.handle_leader(press(keys::ADD_FOLDER));
        workbench
            .input
            .as_mut()
            .expect(PROMPT_LOST)
            .value
            .set_text(&typed.to_string_lossy());
        workbench.handle_key(key(KeyCode::Enter));
    }

    /// The project from [`project`] with a folder of this machine added
    /// beside it, holding one note, and the folder's own path.
    fn added_project() -> (TempDir, TempDir, PathBuf, Workbench) {
        let (dir, mut workbench) = project();
        let host = TempDir::new().expect("a temporary directory");
        fs::create_dir(host.path().join(ADDED_DIR)).expect("a folder");
        fs::write(host.path().join(ADDED_DIR).join(NOTE), HOST_TEXT).expect("a note");
        add_folder(&mut workbench, &host.path().join(ADDED_DIR));
        paint(&mut workbench, WIDTH, HEIGHT);
        let root = canonical(&host.path().join(ADDED_DIR));
        (dir, host, root, workbench)
    }

    #[test]
    fn mounts_stack_a_folded_caudra_section_under_the_project() {
        let (_dir, _host, workbench) = caudra_project();

        assert!(
            !rects(&workbench, ExplorerSection::Project)
                .header
                .is_empty()
                && !rects(&workbench, ExplorerSection::Caudra).header.is_empty(),
            "{NOT_STACKED}"
        );
        assert!(
            workbench.explorer.is_collapsed(ExplorerSection::Caudra),
            "{WRONG_FOLD}"
        );
        assert!(
            rects(&workbench, ExplorerSection::Folders)
                .header
                .is_empty(),
            "{NOT_STACKED}"
        );
    }

    #[test]
    fn dropping_every_mount_hands_the_pane_and_the_cursor_back_to_the_project() {
        let (_dir, _host, mut workbench) = caudra_project();
        workbench.explorer.select_header(ExplorerSection::Caudra);

        workbench.set_caudra_mounts(Vec::new());
        paint(&mut workbench, WIDTH, HEIGHT);

        assert!(
            ExplorerSection::ALL
                .iter()
                .all(|section| rects(&workbench, *section).header.is_empty()),
            "{STILL_STACKED}"
        );
        assert_eq!(
            cursor(&workbench),
            (ExplorerSection::Project, false),
            "{WRONG_CURSOR}"
        );
    }

    #[test]
    fn a_press_and_release_on_the_caudra_header_unfolds_it() {
        let (_dir, _host, mut workbench) = caudra_project();
        let header = rects(&workbench, ExplorerSection::Caudra).header;

        workbench.handle_mouse(click(header.x + 1, header.y));
        workbench.handle_mouse(release(header.x + 1, header.y));
        paint(&mut workbench, WIDTH, HEIGHT);

        assert!(
            !workbench.explorer.is_collapsed(ExplorerSection::Caudra),
            "{WRONG_FOLD}"
        );
        assert_eq!(
            names(&workbench, ExplorerSection::Caudra),
            [CONFIG, PLANS],
            "{NOT_STACKED}"
        );
        assert!(
            rects(&workbench, ExplorerSection::Caudra).body.height > 0,
            "{WRONG_FOLD}"
        );
    }

    #[test]
    fn dragging_the_caudra_header_resizes_the_project_above_it() {
        let (_dir, _host, mut workbench) = caudra_project();
        unfold_caudra(&mut workbench);
        let header = rects(&workbench, ExplorerSection::Caudra).header;
        let before = rects(&workbench, ExplorerSection::Project).body.height;
        let moved = header.y - 3;

        workbench.handle_mouse(click(header.x + 1, header.y));
        workbench.handle_mouse(drag(header.x + 1, moved));
        workbench.handle_mouse(release(header.x + 1, moved));

        assert_eq!(
            workbench.explorer.frames[ExplorerSection::Project.index()].height,
            before - 3,
            "{WRONG_HEIGHT}"
        );
        assert!(
            !workbench.explorer.is_collapsed(ExplorerSection::Caudra),
            "{WRONG_FOLD}"
        );
    }

    #[test]
    fn the_wheel_scrolls_only_the_section_under_the_pointer() {
        let (dir, host, mut workbench) = caudra_project();
        for index in 0..CROWD {
            fs::write(dir.path().join(format!("p{index}.txt")), "").expect("a file");
            fs::write(
                host.path().join(CONFIG_DIR).join(format!("c{index}.toml")),
                "",
            )
            .expect("a file");
        }
        workbench.reread();
        workbench.reveal_in_explorer(&host_path(&host, PROMPT_FILE), true);
        workbench.explorer.select_row(ExplorerSection::Project);
        paint(&mut workbench, WIDTH, HEIGHT);
        let scrolls = |workbench: &Workbench| {
            [ExplorerSection::Project, ExplorerSection::Caudra]
                .map(|section| workbench.section_tree(section).scroll())
        };
        let start = scrolls(&workbench);
        let step = SCROLL_LINES as usize;

        let caudra = rects(&workbench, ExplorerSection::Caudra).body;
        workbench.handle_mouse(wheel(caudra.x + 1, caudra.y + 1));
        assert_eq!(
            scrolls(&workbench),
            [start[0], start[1] + step],
            "{WRONG_SCROLL}"
        );

        let project = rects(&workbench, ExplorerSection::Project).body;
        workbench.handle_mouse(wheel(project.x + 1, project.y + 1));
        assert_eq!(
            scrolls(&workbench),
            [start[0] + step, start[1] + step],
            "{WRONG_SCROLL}"
        );
    }

    #[test]
    fn the_arrows_walk_over_a_header_into_the_section_under_it_and_back() {
        let (dir, host, mut workbench) = caudra_project();
        workbench.focus = Focus::Sidebar;
        workbench.tree.select_last();
        let config = host_path(&host, CONFIG_DIR);
        let last = WorkbenchPath::Local(dir.path().join(LAST_PROJECT_FILE));
        let steps: [(KeyCode, (ExplorerSection, bool), Option<&WorkbenchPath>); 5] = [
            (KeyCode::Down, (ExplorerSection::Caudra, true), None),
            (KeyCode::Down, (ExplorerSection::Caudra, true), None),
            (KeyCode::Right, (ExplorerSection::Caudra, true), None),
            (
                KeyCode::Down,
                (ExplorerSection::Caudra, false),
                Some(&config),
            ),
            (KeyCode::Up, (ExplorerSection::Caudra, true), None),
        ];

        for (code, expected, selected) in steps {
            workbench.handle_key(key(code));
            assert_eq!(
                cursor(&workbench),
                expected,
                "{WRONG_CURSOR} after {code:?}"
            );
            assert_eq!(
                workbench.explorer_selection().as_ref(),
                selected,
                "{WRONG_CURSOR} after {code:?}"
            );
        }
        workbench.handle_key(key(KeyCode::Up));

        assert_eq!(
            cursor(&workbench),
            (ExplorerSection::Project, false),
            "{WRONG_CURSOR}"
        );
        assert_eq!(workbench.explorer_selection(), Some(last), "{WRONG_CURSOR}");
    }

    #[test_case(1, true ; "one_click_asks_for_a_preview")]
    #[test_case(2, false ; "two_clicks_ask_for_a_tab")]
    fn a_click_on_a_caudra_file_hands_it_to_the_host(clicks: u8, preview: bool) {
        let (_dir, host, mut workbench) = caudra_project();
        let file = host_path(&host, CONFIG_FILE);
        workbench.reveal_in_explorer(&file, true);
        paint(&mut workbench, WIDTH, HEIGHT);

        let action = click_selected(&mut workbench, clicks);

        assert_eq!(
            action,
            WorkbenchAction::OpenHostFile {
                path: file.local().expect("a local path").to_path_buf(),
                preview,
            },
            "{NOT_HANDED_OVER}"
        );
        assert!(workbench.editor.tabs().is_empty(), "{NOT_HANDED_OVER}");
    }

    #[test]
    fn enter_on_a_caudra_file_hands_it_to_the_host_to_keep() {
        let (_dir, host, mut workbench) = caudra_project();
        let file = host_path(&host, CONFIG_FILE);
        workbench.reveal_in_explorer(&file, true);
        workbench.focus = Focus::Sidebar;

        let action = workbench.handle_key(key(KeyCode::Enter));

        assert_eq!(
            action,
            WorkbenchAction::OpenHostFile {
                path: file.local().expect("a local path").to_path_buf(),
                preview: false,
            },
            "{NOT_HANDED_OVER}"
        );
    }

    #[test_case(CONFIG_FILE, &[MenuAction::NewFile, MenuAction::SendToComposer, MenuAction::Rename, MenuAction::Delete], &[] ; "a_file_of_an_unmanaged_mount_offers_everything")]
    #[test_case(CONFIG_DIR, &[MenuAction::NewFile, MenuAction::NewFolder], &[MenuAction::Rename, MenuAction::Delete, MenuAction::RemoveFolder] ; "a_mount_root_cannot_be_renamed_deleted_or_removed")]
    #[test_case(PLAN_FILE, &[MenuAction::Open, MenuAction::CopyPath], &[MenuAction::NewFile, MenuAction::NewFolder, MenuAction::Rename, MenuAction::Delete] ; "a_managed_row_offers_nothing_that_renames_behind_the_store")]
    fn a_caudra_row_offers_what_its_mount_allows(
        relative: &str,
        offered: &[MenuAction],
        withheld: &[MenuAction],
    ) {
        let (_dir, host, mut workbench) = caudra_project();
        workbench.reveal_in_explorer(&host_path(&host, relative), true);
        paint(&mut workbench, WIDTH, HEIGHT);

        workbench.explorer_menu();

        let items = workbench.menu.as_ref().expect(WRONG_OFFER).items();
        for action in offered {
            assert!(
                items.contains(&MenuItem::Action(*action)),
                "{WRONG_OFFER}: {action:?}"
            );
        }
        for action in withheld {
            assert!(
                !items.contains(&MenuItem::Action(*action)),
                "{WRONG_OFFER}: {action:?}"
            );
        }
    }

    #[test_case(true ; "asked_for_outright_unfolds")]
    #[test_case(false ; "following_the_editor_leaves_it_folded")]
    fn revealing_a_caudra_file_unfolds_its_section_only_when_asked(unfold: bool) {
        let (_dir, host, mut workbench) = caudra_project();
        let prompt = host_path(&host, PROMPT_FILE);
        let before = workbench.explorer_selection();

        workbench.reveal_in_explorer(&prompt, unfold);

        assert_eq!(
            workbench.explorer.is_collapsed(ExplorerSection::Caudra),
            !unfold,
            "{WRONG_REVEAL}"
        );
        let expected = match unfold {
            true => (ExplorerSection::Caudra, Some(prompt)),
            false => (ExplorerSection::Project, before),
        };
        assert_eq!(
            (workbench.explorer.active(), workbench.explorer_selection()),
            expected,
            "{WRONG_REVEAL}"
        );
    }

    #[test]
    fn a_stored_explorer_layout_survives_a_round_trip() {
        let (_dir, _host, mut workbench) = caudra_project();
        workbench.explorer.toggle_collapsed(ExplorerSection::Caudra);
        workbench.explorer.set_height(ExplorerSection::Project, 5);

        let stored = workbench.layout();
        let mut restored = Workbench::new(WorkbenchStyles::default());
        restored.restore(stored.clone());

        assert_eq!(restored.layout().explorer, stored.explorer, "{LAYOUT_LOST}");
        assert_ne!(stored.explorer, default_frames(), "{LAYOUT_LOST}");
    }

    #[test]
    fn a_stored_layout_naming_fewer_sections_keeps_the_defaults_for_the_rest() {
        let stored = Layout {
            explorer: vec![SectionLayout {
                height: 5,
                collapsed: true,
            }],
            ..Layout::default()
        };
        let mut workbench = Workbench::new(WorkbenchStyles::default());

        workbench.restore(stored.clone());

        let saved = workbench.explorer.saved();
        assert_eq!(saved[..1], stored.explorer[..], "{LAYOUT_LOST}");
        assert_eq!(saved[1..], default_frames()[1..], "{LAYOUT_LOST}");
    }

    #[test]
    fn a_file_made_under_a_mount_reaches_the_caudra_section() {
        let (_dir, host, mut workbench) = caudra_project();
        workbench.reveal_in_explorer(&host_path(&host, CONFIG_FILE), true);
        workbench.tick();

        fs::write(host.path().join(CONFIG_DIR).join(NEW_FILE), HOST_TEXT).expect("a file");

        tick_until(
            &mut workbench,
            |workbench, _| {
                names(workbench, ExplorerSection::Caudra)
                    .iter()
                    .any(|name| name == NEW_FILE)
            },
            SECTION_STALE,
        );
    }

    #[test]
    fn a_write_to_an_open_file_of_this_machine_reaches_its_tab() {
        let (_dir, host, mut workbench) = caudra_project();
        let plan = host.path().join(PLAN_FILE);
        workbench.open_host_file(&plan, None, false);
        workbench.tick();

        rewrite(&plan, HOST_REWRITE);

        tick_until(
            &mut workbench,
            |workbench, _| {
                workbench
                    .editor
                    .active()
                    .is_some_and(|tab| tab.contents() == HOST_REWRITE)
            },
            TAB_STALE,
        );
    }

    #[test]
    fn a_caudra_row_in_a_workspace_session_is_read_here_and_never_mentioned() {
        let (mut workbench, _control, host, file) = bound_beside_host();
        workbench.set_caudra_mounts(vec![HostMount {
            label: CONFIG.to_owned(),
            root: host.path().to_path_buf(),
            managed: false,
        }]);
        let path = WorkbenchPath::Local(file.clone());
        workbench.reveal_in_explorer(&path, true);
        paint(&mut workbench, WIDTH, HEIGHT);

        workbench.explorer_menu();
        let items = workbench.menu.take().expect(WRONG_OFFER).items().to_vec();

        assert!(
            !items.contains(&MenuItem::Action(MenuAction::SendToComposer)),
            "{WRONG_OFFER}"
        );
        assert!(
            items.contains(&MenuItem::Action(MenuAction::Rename)),
            "{WRONG_OFFER}"
        );
        assert_eq!(
            workbench.open_selected(),
            WorkbenchAction::OpenHostFile {
                path: file,
                preview: false,
            },
            "{NOT_HANDED_OVER}"
        );
        assert_still_bound(&workbench);
    }

    #[test]
    fn a_typed_folder_stands_in_a_section_of_its_own() {
        let (_dir, _host, root, workbench) = added_project();

        assert!(workbench.input.is_none(), "{NOT_ADDED}");
        assert_eq!(
            workbench.explorer.folder_roots(),
            [root.as_path()],
            "{NOT_ADDED}"
        );
        assert_eq!(
            names(&workbench, ExplorerSection::Folders),
            [ADDED_DIR, NOTE],
            "{NOT_ADDED}"
        );
        assert!(
            rects(&workbench, ExplorerSection::Folders).body.height > 0,
            "{NOT_ADDED}"
        );
        assert_eq!(
            (cursor(&workbench), workbench.explorer_selection()),
            (
                (ExplorerSection::Folders, false),
                Some(WorkbenchPath::Local(root))
            ),
            "{NOT_ADDED}"
        );
    }

    #[test_case(ExplorerSection::Project ; "a_folder_of_the_project")]
    #[test_case(ExplorerSection::Caudra ; "a_folder_under_one_of_caudras")]
    #[test_case(ExplorerSection::Folders ; "a_folder_added_before")]
    fn a_folder_listed_already_is_shown_where_it_is(listed: ExplorerSection) {
        let (dir, host, _) = caudra_project();
        let mut workbench = Workbench::new(WorkbenchStyles::default());
        workbench.open(&canonical(dir.path()));
        workbench.set_caudra_mounts(mounts(&canonical(host.path())));
        fs::create_dir(host.path().join(ADDED_DIR)).expect("a folder");
        add_folder(&mut workbench, &host.path().join(ADDED_DIR));
        let typed = match listed {
            ExplorerSection::Project => canonical(dir.path()).join(PROJECT_DIR),
            ExplorerSection::Caudra => canonical(host.path()).join(PROMPTS_DIR),
            ExplorerSection::Folders => canonical(host.path()).join(ADDED_DIR),
        };
        let before = workbench.explorer.folder_roots();

        add_folder(&mut workbench, &typed);

        assert_eq!(workbench.explorer.folder_roots(), before, "{ADDED_TWICE}");
        assert_eq!(
            (cursor(&workbench), workbench.explorer_selection()),
            ((listed, false), Some(WorkbenchPath::Local(typed))),
            "{ADDED_TWICE}"
        );
    }

    #[test]
    fn a_folder_that_cannot_be_listed_leaves_the_prompt_up_with_the_reason() {
        let (_dir, mut workbench) = project();

        add_folder(&mut workbench, Path::new(RELATIVE));

        assert_eq!(
            workbench.flash,
            Some(OpsError::NotAbsolute.to_string()),
            "{PROMPT_LOST}"
        );
        assert!(workbench.input.is_some(), "{PROMPT_LOST}");
        assert!(
            workbench.explorer.folder_roots().is_empty(),
            "{PROMPT_LOST}"
        );
    }

    #[test_case(None, &[MenuAction::RemoveFolder, MenuAction::NewFile], &[MenuAction::Rename, MenuAction::Delete] ; "an_added_folder_is_taken_out_rather_than_deleted")]
    #[test_case(Some(NOTE), &[MenuAction::Rename, MenuAction::Delete], &[MenuAction::RemoveFolder] ; "a_row_inside_it_is_an_ordinary_row")]
    fn an_added_folder_row_offers_what_it_allows(
        inside: Option<&str>,
        offered: &[MenuAction],
        withheld: &[MenuAction],
    ) {
        let (_dir, _host, root, mut workbench) = added_project();
        let row = inside.map_or_else(|| root.clone(), |name| root.join(name));
        workbench.reveal_in_explorer(&WorkbenchPath::Local(row), true);

        workbench.explorer_menu();

        let items = workbench.menu.as_ref().expect(WRONG_OFFER).items();
        for action in offered {
            assert!(
                items.contains(&MenuItem::Action(*action)),
                "{WRONG_OFFER}: {action:?}"
            );
        }
        for action in withheld {
            assert!(
                !items.contains(&MenuItem::Action(*action)),
                "{WRONG_OFFER}: {action:?}"
            );
        }
    }

    #[test]
    fn removing_a_folder_takes_its_section_away_and_leaves_its_files_and_tabs() {
        let (_dir, _host, root, mut workbench) = added_project();
        let note = root.join(NOTE);
        workbench.open_host_file(&note, None, false);

        workbench.run_on_row(MenuAction::RemoveFolder, WorkbenchPath::Local(root));
        paint(&mut workbench, WIDTH, HEIGHT);

        assert!(
            workbench.explorer.folder_roots().is_empty(),
            "{NOT_REMOVED}"
        );
        assert!(
            rects(&workbench, ExplorerSection::Folders)
                .header
                .is_empty(),
            "{NOT_REMOVED}"
        );
        assert_eq!(
            cursor(&workbench),
            (ExplorerSection::Project, false),
            "{NOT_REMOVED}"
        );
        assert!(note.is_file(), "{NOT_REMOVED}");
        assert_eq!(
            workbench.editor.active().map(|tab| tab.path.clone()),
            Some(WorkbenchPath::Local(note)),
            "{NOT_REMOVED}"
        );
    }

    #[test_case(true ; "pointer")]
    #[test_case(false ; "keyboard")]
    fn the_menu_on_a_header_asks_for_a_folder_to_add(pointer: bool) {
        let (_dir, _host, mut workbench) = caudra_project();
        let header = rects(&workbench, ExplorerSection::Caudra).header;
        match pointer {
            true => workbench.explorer_menu_at((header.x + 1, header.y)),
            false => {
                workbench.explorer.select_header(ExplorerSection::Caudra);
                workbench.explorer_menu();
            }
        }

        let menu = workbench.menu.take().expect(NO_HEADER_MENU);
        assert_eq!(
            menu.items(),
            [MenuItem::Action(MenuAction::AddFolder)],
            "{NO_HEADER_MENU}"
        );
        assert_eq!(
            cursor(&workbench),
            (ExplorerSection::Caudra, true),
            "{NO_HEADER_MENU}"
        );
        workbench.run_menu(MenuAction::AddFolder, menu.target());
        assert_eq!(
            workbench.input.as_ref().map(|input| input.kind),
            Some(InputKind::AddFolder),
            "{NO_HEADER_MENU}"
        );
    }

    #[test]
    fn added_folders_survive_a_round_trip_while_one_is_gone() {
        let (_dir, host, _, mut workbench) = added_project();
        fs::create_dir(host.path().join(SECOND_DIR)).expect("a folder");
        add_folder(&mut workbench, &host.path().join(SECOND_DIR));
        fs::remove_dir(host.path().join(SECOND_DIR)).expect("a removal");

        let stored = workbench.layout();
        let mut restored = Workbench::new(WorkbenchStyles::default());
        restored.restore(stored.clone());

        assert_eq!(stored.folders.len(), 2, "{FOLDERS_LOST}");
        assert_eq!(restored.layout().folders, stored.folders, "{FOLDERS_LOST}");
        assert_eq!(
            names(&restored, ExplorerSection::Folders),
            [ADDED_DIR],
            "{FOLDERS_LOST}"
        );
    }

    #[test]
    fn a_folder_added_in_a_workspace_session_is_read_here_and_never_mentioned() {
        let (mut workbench, _control, host, file) = bound_beside_host();
        add_folder(&mut workbench, host.path());
        let path = WorkbenchPath::Local(canonical(&file));
        workbench.reveal_in_explorer(&path, true);
        paint(&mut workbench, WIDTH, HEIGHT);

        workbench.explorer_menu();
        let items = workbench.menu.take().expect(WRONG_OFFER).items().to_vec();

        assert!(
            !items.contains(&MenuItem::Action(MenuAction::SendToComposer)),
            "{WRONG_OFFER}"
        );
        assert_eq!(
            workbench.open_selected(),
            WorkbenchAction::Consumed,
            "{NOT_ADDED}"
        );
        assert_eq!(
            workbench.editor.active().map(|tab| tab.path.clone()),
            Some(path),
            "{NOT_ADDED}"
        );
        assert_still_bound(&workbench);
    }
}
