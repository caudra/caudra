//! The context menu, and what each thing it opens over offers.
//!
//! One list per target, stated once, so the painter and the pointer read the
//! same items in the same order.

use unicode_width::UnicodeWidthStr;

use crate::editor::Tab;
use crate::fs::backend::WorkbenchPath;
use crate::fs::tree::Row;

/// Room for the widest list either target builds, so opening a menu is one
/// allocation.
const ITEMS: usize = 13;

/// One thing a menu can do. What it does is read from the target the menu was
/// opened on, so the same action covers a row and a tab where they agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Open,
    NewFile,
    NewFolder,
    CopyPath,
    CopyRelative,
    SendToComposer,
    Rename,
    Delete,
    AddFolder,
    RemoveFolder,
    Close,
    CloseOthers,
    CloseRight,
    CloseSaved,
    CloseAll,
    KeepOpen,
    Save,
    ShowRendered,
    ShowSource,
    RevealInExplorer,
}

impl Action {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::NewFile => "New File",
            Self::NewFolder => "New Folder",
            Self::CopyPath => "Copy Path",
            Self::CopyRelative => "Copy Relative Path",
            Self::SendToComposer => "Send to Composer",
            Self::Rename => "Rename",
            Self::Delete => "Delete",
            Self::AddFolder => "Add Folder",
            Self::RemoveFolder => "Remove Folder",
            Self::Close => "Close",
            Self::CloseOthers => "Close Others",
            Self::CloseRight => "Close to the Right",
            Self::CloseSaved => "Close Saved",
            Self::CloseAll => "Close All",
            Self::KeepOpen => "Keep Open",
            Self::Save => "Save",
            Self::ShowRendered => "Show Rendered",
            Self::ShowSource => "Show Source",
            Self::RevealInExplorer => "Reveal in Explorer",
        }
    }
}

/// A painted row of the menu. A separator is drawn and skipped over rather
/// than left out, because the grouping is what makes a long list readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Item {
    Action(Action),
    Separator,
}

/// What a row's menu offers beyond reading it. A project row offers all of it;
/// a row of this machine's own folders holds some of it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowOffer {
    /// Making a file or a folder beside or inside the row.
    pub(crate) create: bool,
    /// Renaming or deleting the row itself.
    pub(crate) mutate: bool,
    /// Naming the row in the composer.
    pub(crate) mention: bool,
    /// Taking a folder added by hand back out of the explorer.
    pub(crate) remove: bool,
}

#[cfg(test)]
impl RowOffer {
    pub(crate) const ALL: Self = Self {
        create: true,
        mutate: true,
        mention: true,
        remove: true,
    };
}

/// What the menu was opened on. Captured when it opens: any other input closes
/// the menu, so neither the row nor the strip can move underneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Row(WorkbenchPath),
    Tab(usize),
    /// An explorer section's header, which stands for the explorer itself.
    Header,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Menu {
    items: Vec<Item>,
    selected: usize,
    /// The cell the menu was asked for, which is the corner it is drawn from.
    at: (u16, u16),
    target: Target,
}

impl Menu {
    /// The menu for a tree row. A folder has nothing to open, since pressing
    /// one expands it.
    pub(crate) fn for_row(row: &Row, at: (u16, u16), offer: &RowOffer) -> Self {
        let groups: [&[(bool, Action)]; 3] = [
            &[
                (!row.is_dir(), Action::Open),
                (offer.create, Action::NewFile),
                (offer.create, Action::NewFolder),
            ],
            &[
                (true, Action::CopyPath),
                (true, Action::CopyRelative),
                (offer.mention, Action::SendToComposer),
            ],
            &[
                (offer.mutate, Action::Rename),
                (offer.mutate, Action::Delete),
                (offer.remove, Action::RemoveFolder),
            ],
        ];
        let mut items = Vec::with_capacity(ITEMS);
        for group in groups {
            let mut offered = group
                .iter()
                .filter_map(|(shown, action)| shown.then_some(Item::Action(*action)))
                .peekable();
            if offered.peek().is_some() && !items.is_empty() {
                items.push(Item::Separator);
            }
            items.extend(offered);
        }
        Self::new(items, at, Target::Row(row.path.clone()))
    }

    /// The menu for a tab. A diff or a host's document has no file behind it,
    /// so it offers nothing that names one. `renderable` says whether the
    /// workbench can paint the tab's rendered view, in which case it offers
    /// whichever view is hidden.
    pub(crate) fn for_tab(tab: &Tab, index: usize, at: (u16, u16), renderable: bool) -> Self {
        let on_disk = tab.is_file();
        let mut items = Vec::with_capacity(ITEMS);
        items.extend([
            Item::Action(Action::Close),
            Item::Action(Action::CloseOthers),
            Item::Action(Action::CloseRight),
            Item::Action(Action::CloseSaved),
            Item::Action(Action::CloseAll),
        ]);
        let other_view = match tab.is_rendered() {
            true => Action::ShowSource,
            false => Action::ShowRendered,
        };
        let about_this_tab = [
            (tab.preview, Action::KeepOpen),
            (tab.is_dirty(), Action::Save),
            (renderable, other_view),
            (on_disk, Action::RevealInExplorer),
        ];
        let mut offered = about_this_tab
            .into_iter()
            .filter_map(|(shown, action)| shown.then_some(Item::Action(action)))
            .peekable();
        if offered.peek().is_some() {
            items.push(Item::Separator);
            items.extend(offered);
        }
        if on_disk {
            items.extend([
                Item::Separator,
                Item::Action(Action::CopyPath),
                Item::Action(Action::CopyRelative),
            ]);
        }
        Self::new(items, at, Target::Tab(index))
    }

    /// The menu for an explorer section's header, which is where a folder of
    /// this machine joins the explorer.
    pub(crate) fn for_header(at: (u16, u16)) -> Self {
        Self::new(vec![Item::Action(Action::AddFolder)], at, Target::Header)
    }

    fn new(items: Vec<Item>, at: (u16, u16), target: Target) -> Self {
        let mut menu = Self {
            items,
            selected: 0,
            at,
            target,
        };
        menu.selected = menu.stops().first().copied().unwrap_or_default();
        menu
    }

    pub(crate) fn items(&self) -> &[Item] {
        &self.items
    }

    pub(crate) fn at(&self) -> (u16, u16) {
        self.at
    }

    pub(crate) fn target(&self) -> &Target {
        &self.target
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    pub(crate) fn selected(&self) -> Option<Action> {
        self.action_at(self.selected)
    }

    /// What the row `offset` rows down the panel does, if it does anything.
    pub(crate) fn action_at(&self, offset: usize) -> Option<Action> {
        match self.items.get(offset) {
            Some(Item::Action(action)) => Some(*action),
            _ => None,
        }
    }

    /// The neighbouring action, stepping over the rules between groups and
    /// stopping at both ends rather than wrapping.
    pub(crate) fn step(&mut self, delta: isize) {
        let stops = self.stops();
        let Some(at) = stops.iter().position(|stop| *stop == self.selected) else {
            return;
        };
        let reached = at.saturating_add_signed(delta).min(stops.len() - 1);
        self.selected = stops[reached];
    }

    pub(crate) fn select_first(&mut self) {
        self.selected = self.stops().first().copied().unwrap_or_default();
    }

    pub(crate) fn select_last(&mut self) {
        self.selected = self.stops().last().copied().unwrap_or_default();
    }

    /// The widest label, which is what the panel is drawn and measured from.
    pub(crate) fn width(&self) -> usize {
        self.items
            .iter()
            .filter_map(|item| match item {
                Item::Action(action) => Some(action.label().width()),
                Item::Separator => None,
            })
            .max()
            .unwrap_or_default()
    }

    /// Every row the cursor is allowed to rest on.
    fn stops(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| matches!(item, Item::Action(_)))
            .map(|(offset, _)| offset)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, Item, Menu, RowOffer, Target};
    use crate::editor::Tab;
    use crate::fs::backend::WorkbenchPath;
    use crate::fs::tree::{EntryKind, Row};
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;
    use test_case::test_case;

    const WRONG_ITEMS: &str = "the menu is not offering what its target can do";
    const RULE_PICKED: &str = "the cursor must never rest on a rule between groups";
    const WRONG_WIDTH: &str = "the panel is not as wide as the longest thing in it";
    const ANYWHERE: (u16, u16) = (0, 0);
    const FILE_NAME: &str = "a.rs";
    const MARKDOWN_NAME: &str = "a.md";
    const MARKDOWN_TEXT: &str = "# one\n";

    fn row(kind: EntryKind) -> Row {
        Row {
            path: WorkbenchPath::Local(FILE_NAME.into()),
            resource: None,
            name: FILE_NAME.to_owned(),
            depth: 0,
            kind,
            expanded: false,
            git: None,
            agent_touched: false,
            ignored: false,
        }
    }

    /// A tab over a real file, which is the only way to get one that is not a
    /// diff.
    fn tab(dir: &TempDir) -> Tab {
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, "one\n").expect("a file");
        Tab::open(&path, 0).expect("a tab")
    }

    fn actions(menu: &Menu) -> Vec<Action> {
        menu.items()
            .iter()
            .filter_map(|item| match item {
                Item::Action(action) => Some(*action),
                Item::Separator => None,
            })
            .collect()
    }

    #[test_case(EntryKind::File, true ; "a file can be opened")]
    #[test_case(EntryKind::Directory, false ; "a folder expands instead")]
    fn a_row_offers_open_only_when_there_is_something_to_open(kind: EntryKind, expected: bool) {
        let menu = Menu::for_row(&row(kind), ANYWHERE, &RowOffer::ALL);

        assert_eq!(
            actions(&menu).contains(&Action::Open),
            expected,
            "{WRONG_ITEMS}"
        );
        assert_eq!(menu.target(), &Target::Row(PathBuf::from(FILE_NAME).into()));
    }

    #[test]
    fn every_row_can_be_renamed_copied_and_thrown_away() {
        let menu = Menu::for_row(&row(EntryKind::Directory), ANYWHERE, &RowOffer::ALL);
        let offered = actions(&menu);

        for action in [
            Action::NewFile,
            Action::NewFolder,
            Action::CopyPath,
            Action::CopyRelative,
            Action::SendToComposer,
            Action::Rename,
            Action::Delete,
        ] {
            assert!(offered.contains(&action), "{WRONG_ITEMS}: {action:?}");
        }
    }

    #[test_case(RowOffer { create: false, ..RowOffer::ALL }, &[Action::NewFile, Action::NewFolder] ; "no creating")]
    #[test_case(RowOffer { mutate: false, ..RowOffer::ALL }, &[Action::Rename, Action::Delete] ; "no renaming or deleting")]
    #[test_case(RowOffer { mention: false, ..RowOffer::ALL }, &[Action::SendToComposer] ; "no mentioning")]
    #[test_case(RowOffer { remove: false, ..RowOffer::ALL }, &[Action::RemoveFolder] ; "no removing")]
    fn a_row_holds_back_what_it_may_not_offer(offer: RowOffer, withheld: &[Action]) {
        let menu = Menu::for_row(&row(EntryKind::File), ANYWHERE, &offer);
        let offered = actions(&menu);

        for action in withheld {
            assert!(!offered.contains(action), "{WRONG_ITEMS}: {action:?}");
        }
        assert!(offered.contains(&Action::CopyPath), "{WRONG_ITEMS}");
    }

    #[test]
    fn a_row_offering_only_reads_draws_no_stray_rule() {
        let offer = RowOffer {
            create: false,
            mutate: false,
            mention: false,
            remove: false,
        };
        let menu = Menu::for_row(&row(EntryKind::Directory), ANYWHERE, &offer);

        assert_eq!(
            menu.items(),
            [
                Item::Action(Action::CopyPath),
                Item::Action(Action::CopyRelative)
            ],
            "{WRONG_ITEMS}"
        );
    }

    #[test]
    fn a_saved_tab_offers_nothing_to_save_and_nothing_to_keep() {
        let dir = TempDir::new().expect("a temporary directory");
        let offered = actions(&Menu::for_tab(&tab(&dir), 0, ANYWHERE, false));

        assert!(!offered.contains(&Action::Save), "{WRONG_ITEMS}");
        assert!(!offered.contains(&Action::KeepOpen), "{WRONG_ITEMS}");
        assert!(offered.contains(&Action::RevealInExplorer), "{WRONG_ITEMS}");
        assert!(offered.contains(&Action::CopyPath), "{WRONG_ITEMS}");
    }

    #[test]
    fn a_previewed_tab_with_edits_offers_both_ways_to_keep_it() {
        let dir = TempDir::new().expect("a temporary directory");
        let mut tab = tab(&dir);
        tab.preview = true;
        let edit = tab.buffer.insert("x");
        tab.record(edit);

        let offered = actions(&Menu::for_tab(&tab, 0, ANYWHERE, false));

        assert!(offered.contains(&Action::KeepOpen), "{WRONG_ITEMS}");
        assert!(offered.contains(&Action::Save), "{WRONG_ITEMS}");
    }

    #[test_case(false, false, &[] ; "nothing to render with")]
    #[test_case(true, false, &[Action::ShowRendered] ; "showing its source")]
    #[test_case(true, true, &[Action::ShowSource] ; "showing its rendered view")]
    fn a_markdown_tab_offers_the_view_it_is_not_showing(
        renderable: bool,
        rendered: bool,
        expected: &[Action],
    ) {
        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join(MARKDOWN_NAME);
        fs::write(&path, MARKDOWN_TEXT).expect("a file");
        let mut tab = Tab::open(&path, 0).expect("a tab");
        if rendered {
            tab.toggle_rendered();
        }

        let offered: Vec<Action> = actions(&Menu::for_tab(&tab, 0, ANYWHERE, renderable))
            .into_iter()
            .filter(|action| matches!(action, Action::ShowRendered | Action::ShowSource))
            .collect();

        assert_eq!(offered, expected, "{WRONG_ITEMS}");
    }

    /// A diff is built from the repository rather than read from a path, so
    /// everything that names a file is left off.
    #[test]
    fn a_diff_tab_offers_only_the_closes() {
        let tab = Tab::synthetic(Path::new(FILE_NAME), FILE_NAME.to_owned(), Vec::new(), 0);

        let offered = actions(&Menu::for_tab(&tab, 1, ANYWHERE, false));

        assert_eq!(
            offered,
            vec![
                Action::Close,
                Action::CloseOthers,
                Action::CloseRight,
                Action::CloseSaved,
                Action::CloseAll,
            ],
            "{WRONG_ITEMS}"
        );
    }

    #[test]
    fn the_cursor_steps_over_the_rules_between_groups() {
        let mut menu = Menu::for_row(&row(EntryKind::File), ANYWHERE, &RowOffer::ALL);
        let mut landed = vec![menu.selected().expect(RULE_PICKED)];

        for _ in 1..actions(&menu).len() {
            menu.step(1);
            landed.push(menu.selected().expect(RULE_PICKED));
        }

        assert_eq!(landed, actions(&menu), "{RULE_PICKED}");
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut menu = Menu::for_row(&row(EntryKind::File), ANYWHERE, &RowOffer::ALL);

        menu.step(-1);
        assert_eq!(menu.selected(), Some(Action::Open), "{RULE_PICKED}");

        menu.select_last();
        menu.step(1);
        assert_eq!(menu.selected(), Some(Action::RemoveFolder), "{RULE_PICKED}");
    }

    #[test]
    fn a_rule_is_never_where_the_cursor_lands() {
        let mut menu = Menu::for_row(&row(EntryKind::File), ANYWHERE, &RowOffer::ALL);
        let rule = menu
            .items()
            .iter()
            .position(|item| *item == Item::Separator)
            .expect("a rule between two groups");

        assert_eq!(menu.action_at(rule), None, "{RULE_PICKED}");
        for _ in 0..menu.items().len() {
            assert!(menu.selected().is_some(), "{RULE_PICKED}");
            menu.step(1);
        }
    }

    #[test]
    fn the_panel_is_as_wide_as_its_longest_label() {
        let menu = Menu::for_row(&row(EntryKind::File), ANYWHERE, &RowOffer::ALL);

        assert_eq!(
            menu.width(),
            Action::CopyRelative.label().len(),
            "{WRONG_WIDTH}"
        );
    }
}
