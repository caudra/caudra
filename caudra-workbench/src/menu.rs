//! The context menu, and what each thing it opens over offers.
//!
//! One list per target, stated once, so the painter and the pointer read the
//! same items in the same order.

use std::path::PathBuf;

use unicode_width::UnicodeWidthStr;

use crate::editor::Tab;
use crate::fs::tree::Row;

/// Room for the widest list either target builds, so opening a menu is one
/// allocation.
const ITEMS: usize = 12;

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
    Close,
    CloseOthers,
    CloseRight,
    CloseSaved,
    CloseAll,
    KeepOpen,
    Save,
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
            Self::Close => "Close",
            Self::CloseOthers => "Close Others",
            Self::CloseRight => "Close to the Right",
            Self::CloseSaved => "Close Saved",
            Self::CloseAll => "Close All",
            Self::KeepOpen => "Keep Open",
            Self::Save => "Save",
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

/// What the menu was opened on. Captured when it opens: any other input closes
/// the menu, so neither the row nor the strip can move underneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Row(PathBuf),
    Tab(usize),
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
    pub(crate) fn for_row(row: &Row, at: (u16, u16)) -> Self {
        let mut items = Vec::with_capacity(ITEMS);
        if !row.is_dir() {
            items.push(Item::Action(Action::Open));
        }
        items.extend([
            Item::Action(Action::NewFile),
            Item::Action(Action::NewFolder),
            Item::Separator,
            Item::Action(Action::CopyPath),
            Item::Action(Action::CopyRelative),
            Item::Action(Action::SendToComposer),
            Item::Separator,
            Item::Action(Action::Rename),
            Item::Action(Action::Delete),
        ]);
        Self::new(items, at, Target::Row(row.path.clone()))
    }

    /// The menu for a tab. A diff has no file behind it, so it offers nothing
    /// that names one.
    pub(crate) fn for_tab(tab: &Tab, index: usize, at: (u16, u16)) -> Self {
        let on_disk = tab.diff_kinds().is_none();
        let mut items = Vec::with_capacity(ITEMS);
        items.extend([
            Item::Action(Action::Close),
            Item::Action(Action::CloseOthers),
            Item::Action(Action::CloseRight),
            Item::Action(Action::CloseSaved),
            Item::Action(Action::CloseAll),
        ]);
        let about_this_tab = [
            (tab.preview, Action::KeepOpen),
            (tab.is_dirty(), Action::Save),
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
    use super::{Action, Item, Menu, Target};
    use crate::editor::Tab;
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

    fn row(kind: EntryKind) -> Row {
        Row {
            path: PathBuf::from(FILE_NAME),
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
        let menu = Menu::for_row(&row(kind), ANYWHERE);

        assert_eq!(
            actions(&menu).contains(&Action::Open),
            expected,
            "{WRONG_ITEMS}"
        );
        assert_eq!(menu.target(), &Target::Row(PathBuf::from(FILE_NAME)));
    }

    #[test]
    fn every_row_can_be_renamed_copied_and_thrown_away() {
        let menu = Menu::for_row(&row(EntryKind::Directory), ANYWHERE);
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

    #[test]
    fn a_saved_tab_offers_nothing_to_save_and_nothing_to_keep() {
        let dir = TempDir::new().expect("a temporary directory");
        let offered = actions(&Menu::for_tab(&tab(&dir), 0, ANYWHERE));

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

        let offered = actions(&Menu::for_tab(&tab, 0, ANYWHERE));

        assert!(offered.contains(&Action::KeepOpen), "{WRONG_ITEMS}");
        assert!(offered.contains(&Action::Save), "{WRONG_ITEMS}");
    }

    /// A diff is built from the repository rather than read from a path, so
    /// everything that names a file is left off.
    #[test]
    fn a_diff_tab_offers_only_the_closes() {
        let tab = Tab::synthetic(
            Path::new(FILE_NAME),
            FILE_NAME.to_owned(),
            Vec::new(),
            Vec::new(),
            0,
        );

        let offered = actions(&Menu::for_tab(&tab, 1, ANYWHERE));

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
        let mut menu = Menu::for_row(&row(EntryKind::File), ANYWHERE);
        let mut landed = vec![menu.selected().expect(RULE_PICKED)];

        for _ in 1..actions(&menu).len() {
            menu.step(1);
            landed.push(menu.selected().expect(RULE_PICKED));
        }

        assert_eq!(landed, actions(&menu), "{RULE_PICKED}");
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut menu = Menu::for_row(&row(EntryKind::File), ANYWHERE);

        menu.step(-1);
        assert_eq!(menu.selected(), Some(Action::Open), "{RULE_PICKED}");

        menu.select_last();
        menu.step(1);
        assert_eq!(menu.selected(), Some(Action::Delete), "{RULE_PICKED}");
    }

    #[test]
    fn a_rule_is_never_where_the_cursor_lands() {
        let mut menu = Menu::for_row(&row(EntryKind::File), ANYWHERE);
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
        let menu = Menu::for_row(&row(EntryKind::File), ANYWHERE);

        assert_eq!(
            menu.width(),
            Action::CopyRelative.label().len(),
            "{WRONG_WIDTH}"
        );
    }
}
