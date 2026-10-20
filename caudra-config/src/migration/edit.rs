//! The edits a [`Step`](super::Step) makes. Each addresses a table by its
//! path of keys, reached through standard tables, dotted keys, inline tables
//! and every table of an array of tables. An edit whose source is missing does
//! nothing, and one that would overwrite a key the file already sets fails.

use toml_edit::{Array, Decor, InlineTable, Item, Key, RawString, Table, TableLike, Value};

use super::{MigrationError, Upgrade, note};
use crate::config_version::CONFIG_VERSION_KEY;
use crate::toml_document::keeping_closing_space;

const VERSION_ADDED: &str = "the format version of this file, which Caudra now requires";
/// What toml_edit renders before a table header that has no decor.
const TABLE_PREFIX: &str = "\n";
const KEY_SUFFIX: &str = " ";
const NEWLINE: char = '\n';
const PATH_SEPARATOR: &str = ".";
const KEY_VALUE_SEPARATOR: &str = "=";

/// What [`Upgrade::rename_in_array`] does with a string in an array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayRename {
    To(&'static str),
    Retire,
}

/// One table a path reaches.
#[derive(Debug, Clone, Default)]
struct Location {
    segments: Vec<Segment>,
    /// The first segment that enters an inline table: the edits below it
    /// share that segment's line.
    inline_from: Option<usize>,
    /// How many segments lead to the table whose header the lines below
    /// belong to, past dotted keys.
    header: usize,
}

#[derive(Debug, Clone)]
struct Segment {
    key: String,
    /// The table's place in an array of tables.
    index: Option<usize>,
}

enum Parent<'a> {
    Table(&'a mut Table),
    Inline(&'a mut InlineTable),
}

impl Upgrade<'_> {
    /// Renames `old` to `new` in place, whether it holds a value or a table.
    pub fn rename_key(
        &mut self,
        path: &[&str],
        old: &str,
        new: &str,
    ) -> Result<(), MigrationError> {
        for at in self.locate(path) {
            let Some(mut parent) = self.parent_mut(&at) else {
                continue;
            };
            if !parent.contains(old) {
                continue;
            }
            if parent.contains(new) {
                return Err(self.conflict(path, old, path, new));
            }
            parent.rename(old, Key::new(new));
            let note = self.record(format!(
                "renamed `{}` to `{}`",
                dotted(path, old),
                dotted(&[], new)
            ));
            self.annotate(&at, new, |prefix| note::annotate(prefix, &note));
        }
        Ok(())
    }

    /// Moves `key` from the table at `from` to the one at `to`, creating
    /// standard tables for whatever part of `to` is missing.
    pub fn move_key(
        &mut self,
        from: &[&str],
        key: &str,
        to: &[&str],
    ) -> Result<(), MigrationError> {
        for at in self.locate(from) {
            if !self
                .parent_mut(&at)
                .is_some_and(|parent| parent.contains(key))
            {
                continue;
            }
            let conflict = self.conflict(from, key, to, key);
            let destination = self.ensure_table(to).ok_or_else(|| conflict.clone())?;
            if self
                .parent_mut(&destination)
                .is_none_or(|parent| parent.contains(key))
            {
                return Err(conflict);
            }
            let crosses_inline = at.inline_from.is_some() || destination.inline_from.is_some();
            let Some((mut moved_key, mut item)) =
                self.parent_mut(&at).and_then(|mut parent| parent.take(key))
            else {
                continue;
            };
            if crosses_inline {
                moved_key = Key::new(key);
                if let Some(value) = item.as_value_mut() {
                    value.decor_mut().clear();
                }
            }
            if let Some(mut parent) = self.parent_mut(&destination) {
                parent.put(moved_key, item);
            }
            let note = self.record(format!(
                "moved `{}` to `{}`",
                dotted(from, key),
                dotted(to, key)
            ));
            self.annotate(&destination, key, |prefix| note::annotate(prefix, &note));
        }
        Ok(())
    }

    /// Renames the strings `renames` names in the array at `key`. A rename to
    /// a string the array already holds drops the old one.
    pub fn rename_in_array(&mut self, path: &[&str], key: &str, renames: &[(&str, ArrayRename)]) {
        for at in self.locate(path) {
            let Some(array) = self
                .parent_mut(&at)
                .and_then(|parent| parent.value_mut(key))
                .and_then(Value::as_array_mut)
            else {
                continue;
            };
            let mut edits = Vec::new();
            for (old, rename) in renames {
                loop {
                    let found = array.iter().position(|value| value.as_str() == Some(old));
                    let Some(index) = found else {
                        break;
                    };
                    match rename {
                        ArrayRename::To(new)
                            if !array.iter().any(|value| value.as_str() == Some(new)) =>
                        {
                            let mut renamed = Value::from(*new);
                            *renamed.decor_mut() = array
                                .get(index)
                                .map(Value::decor)
                                .cloned()
                                .unwrap_or_default();
                            array.replace_formatted(index, renamed);
                            edits.push(format!("renamed `{old}` to `{new}`"));
                        }
                        ArrayRename::To(new) => {
                            remove_element(array, index);
                            edits.push(format!("removed `{old}`, as `{new}` is listed"));
                        }
                        ArrayRename::Retire => {
                            remove_element(array, index);
                            edits.push(format!("removed `{old}`"));
                        }
                    }
                }
            }
            if edits.is_empty() {
                continue;
            }
            let note = self.record(format!(
                "{} in `{}`",
                edits.join(" and "),
                dotted(path, key)
            ));
            self.annotate(&at, key, |prefix| note::annotate(prefix, &note));
        }
    }

    /// Replaces the value at `key` with what `map` makes of it, unless it
    /// returns `None`. The value keeps its comments.
    pub fn map_value(&mut self, path: &[&str], key: &str, map: fn(&Value) -> Option<Value>) {
        for at in self.locate(path) {
            let Some(value) = self
                .parent_mut(&at)
                .and_then(|parent| parent.value_mut(key))
            else {
                continue;
            };
            let Some(mut mapped) = map(value) else {
                continue;
            };
            let (old, new) = (bare(value), bare(&mapped));
            if old == new {
                continue;
            }
            *mapped.decor_mut() = value.decor().clone();
            *value = mapped;
            let note = self.record(format!(
                "changed `{}` from {old} to {new}",
                dotted(path, key)
            ));
            self.annotate(&at, key, |prefix| note::annotate(prefix, &note));
        }
    }

    /// Comments out the value at `key` below a note. A table under `key` is
    /// left as it is.
    pub fn retire_key(&mut self, path: &[&str], key: &str) {
        for at in self.locate(path) {
            let what = format!("retired `{}`", dotted(path, key));
            if at.inline_from.is_some() {
                self.retire_inline(&at, key, what);
            } else {
                self.retire_line(&at, key, what);
            }
        }
    }

    /// Sets `version` to `latest`. A file without one gains it as its first
    /// key, below the comment block that leads the file.
    pub(super) fn stamp_version(&mut self, declared: Option<u32>, latest: u32) {
        let version = Value::from(i64::from(latest));
        let root = self.document.as_table_mut();
        if let Some(from) = declared {
            if let Some(Item::Value(existing)) = root.get_mut(CONFIG_VERSION_KEY) {
                let decor = existing.decor().clone();
                *existing = version;
                *existing.decor_mut() = decor;
            }
            let note = note::upgraded(self.stamp, &format!("version {from} → {latest}"), None);
            edit_line_prefix(root, CONFIG_VERSION_KEY, |prefix| {
                note::annotate(prefix, &note)
            });
            return;
        }
        let head = self.take_leading_block();
        let note = note::added(self.stamp, VERSION_ADDED);
        let mut key = Key::new(CONFIG_VERSION_KEY);
        key.leaf_decor_mut().set_prefix(note::below(&head, &note));
        let mut root = Parent::Table(self.document.as_table_mut());
        let rest = root.take_from(0);
        root.put(key, Item::Value(version));
        rest.into_iter().for_each(|(key, item)| root.put(key, item));
    }

    /// Cuts the file's leading comment block off the decor of the item that
    /// comes first, which is then set apart from the line above it.
    fn take_leading_block(&mut self) -> String {
        let mut head = String::new();
        let mut cut = |prefix: &str| {
            let (leading, rest) = note::split_leading(prefix);
            head = leading.to_owned();
            rest.to_owned()
        };
        let root = self.document.as_table_mut();
        if let Some(first) = body_keys(root).into_iter().next() {
            edit_body_prefix(root, &first, |prefix| note::separated(&cut(prefix)));
        } else if let Some(first) = first_table(root) {
            if let Some(Parent::Table(table)) = parent_mut(root, &first) {
                edit_prefix(table.decor_mut(), "", |prefix| {
                    note::separated(&cut(prefix))
                });
            }
        } else {
            let trailing = self
                .document
                .trailing()
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let rest = cut(&trailing);
            let rest = if rest.is_empty() {
                rest
            } else {
                note::separated(&rest)
            };
            self.document.set_trailing(rest);
        }
        head
    }

    fn retire_inline(&mut self, at: &Location, key: &str, what: String) {
        let Some(Parent::Inline(table)) = self.parent_mut(at) else {
            return;
        };
        let mut removed = None;
        keeping_closing_space(table, |table| removed = table.remove(key));
        let Some(removed) = removed else {
            return;
        };
        let entry = format!(
            "{} {KEY_VALUE_SEPARATOR} {}",
            dotted(&[], key),
            bare(&removed)
        );
        let note = self.record(what);
        self.annotate(at, key, |prefix| {
            note::annotate_retired(prefix, &note, &entry)
        });
    }

    /// Removes the line, and puts the note and the line commented out where
    /// it was: in front of the next line of its table, or else after the
    /// line or the header before it.
    fn retire_line(&mut self, at: &Location, key: &str, what: String) {
        let holds_value = self
            .parent_mut(at)
            .is_some_and(|parent| parent.as_table_like().get(key).is_some_and(Item::is_value));
        if !holds_value {
            return;
        }
        let note = self.record(what);
        let Some(Parent::Table(header)) =
            parent_mut(self.document.as_table_mut(), &at.segments[..at.header])
        else {
            return;
        };
        let target: Vec<String> = at.segments[at.header..]
            .iter()
            .map(|segment| segment.key.clone())
            .chain([key.to_owned()])
            .collect();
        let keys = body_keys(header);
        let (Some(index), Some((prefix, line))) = (
            keys.iter().position(|keys| *keys == target),
            body_line(header, &target),
        ) else {
            return;
        };
        if let Some((parent, leaf)) = dotted_parent(header, &target) {
            parent.remove(leaf);
        }
        let retired = note::retired(&prefix, &note, &line);
        let after = |suffix: &str| {
            format!(
                "{suffix}{NEWLINE}{}",
                retired.strip_suffix(NEWLINE).unwrap_or(&retired)
            )
        };
        if let Some(next) = keys.get(index + 1) {
            edit_body_prefix(header, next, |prefix| format!("{retired}{prefix}"));
        } else if let Some(previous) = index.checked_sub(1).and_then(|previous| keys.get(previous))
        {
            if let Some(value) = dotted_parent(header, previous)
                .and_then(|(parent, leaf)| parent.get_mut(leaf))
                .and_then(Item::as_value_mut)
            {
                edit_suffix(value.decor_mut(), after);
            }
        } else if at.header > 0 {
            edit_suffix(header.decor_mut(), after);
        } else {
            edit_prefix(header.decor_mut(), "", |prefix| {
                format!("{prefix}{retired}")
            });
        }
    }

    fn locate(&self, path: &[&str]) -> Vec<Location> {
        let mut found = Vec::new();
        walk(
            self.document.as_table(),
            path,
            Location::default(),
            &mut found,
        );
        found
    }

    fn parent_mut(&mut self, at: &Location) -> Option<Parent<'_>> {
        parent_mut(self.document.as_table_mut(), &at.segments)
    }

    /// The location of the table at `path`, created where missing, or `None`
    /// when a value or an array of tables stands in the way.
    fn ensure_table(&mut self, path: &[&str]) -> Option<Location> {
        let position = max_position(self.document.as_table()) + 1;
        let mut parent = Parent::Table(self.document.as_table_mut());
        let mut at = Location::default();
        for key in path {
            if !parent.contains(key) {
                let created = match &parent {
                    Parent::Table(_) => {
                        let mut table = Table::new();
                        table.set_implicit(true);
                        table.set_position(Some(position));
                        Item::Table(table)
                    }
                    Parent::Inline(_) => Item::Value(Value::InlineTable(InlineTable::new())),
                };
                parent.put(Key::new(*key), created);
            }
            parent = match parent {
                Parent::Table(table) => match table.get_mut(key)? {
                    Item::Table(child) => {
                        at = at.enter(key, None, child.is_dotted());
                        Parent::Table(child)
                    }
                    Item::Value(Value::InlineTable(child)) => {
                        at = at.enter_inline(key);
                        Parent::Inline(child)
                    }
                    _ => return None,
                },
                Parent::Inline(table) => match table.get_mut(key)? {
                    Value::InlineTable(child) => {
                        at = at.enter_inline(key);
                        Parent::Inline(child)
                    }
                    _ => return None,
                },
            };
        }
        Some(at)
    }

    /// Edits the prefix of the line that holds `key` at `at`: the line of
    /// the inline table it sits in, if any.
    fn annotate(&mut self, at: &Location, key: &str, edit: impl FnOnce(&str) -> String) {
        let (segments, key) = match at.inline_from {
            Some(inline) => (&at.segments[..inline], at.segments[inline].key.as_str()),
            None => (at.segments.as_slice(), key),
        };
        if let Some(Parent::Table(table)) = parent_mut(self.document.as_table_mut(), segments) {
            edit_line_prefix(table, key, edit);
        }
    }

    fn conflict(
        &self,
        old_path: &[&str],
        old: &str,
        new_path: &[&str],
        new: &str,
    ) -> MigrationError {
        MigrationError::Conflict {
            file: self.file,
            old: dotted(old_path, old),
            new: dotted(new_path, new),
        }
    }
}

impl Location {
    fn enter(mut self, key: &str, index: Option<usize>, dotted: bool) -> Self {
        self.segments.push(Segment {
            key: key.to_owned(),
            index,
        });
        if self.inline_from.is_none() && !dotted {
            self.header = self.segments.len();
        }
        self
    }

    fn enter_inline(mut self, key: &str) -> Self {
        self.segments.push(Segment {
            key: key.to_owned(),
            index: None,
        });
        self.inline_from.get_or_insert(self.segments.len() - 1);
        self
    }
}

impl<'a> Parent<'a> {
    fn as_table_like(&self) -> &dyn TableLike {
        match self {
            Self::Table(table) => &**table,
            Self::Inline(table) => &**table,
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.as_table_like().contains_key(key)
    }

    fn value_mut(self, key: &str) -> Option<&'a mut Value> {
        match self {
            Self::Table(table) => table.get_mut(key)?.as_value_mut(),
            Self::Inline(table) => table.get_mut(key),
        }
    }

    /// Removes `key`, keeping the space before an inline table's closing
    /// brace.
    fn take(&mut self, key: &str) -> Option<(Key, Item)> {
        match self {
            Self::Table(table) => table.remove_entry(key),
            Self::Inline(table) => {
                let mut removed = None;
                keeping_closing_space(table, |table| removed = table.remove_entry(key));
                removed.map(|(key, value)| (key, Item::Value(value)))
            }
        }
    }

    /// Appends `key`, turning a table into an inline one when this table is
    /// inline.
    fn put(&mut self, key: Key, item: Item) {
        match self {
            Self::Table(table) => {
                table.insert_formatted(&key, item);
            }
            Self::Inline(table) => {
                if let Ok(value) = item.into_value() {
                    keeping_closing_space(table, |table| {
                        table.insert_formatted(&key, value);
                    });
                }
            }
        }
    }

    /// Removes every entry from the `start`th on, in order.
    fn take_from(&mut self, start: usize) -> Vec<(Key, Item)> {
        let keys: Vec<String> = self
            .as_table_like()
            .iter()
            .skip(start)
            .map(|(key, _)| key.to_owned())
            .collect();
        keys.iter()
            .filter_map(|key| match self {
                Self::Table(table) => table.remove_entry(key),
                Self::Inline(table) => table
                    .remove_entry(key)
                    .map(|(key, value)| (key, Item::Value(value))),
            })
            .collect()
    }

    /// Renames `old` to `new` where it stands, keeping its decor.
    fn rename(&mut self, old: &str, new: Key) {
        let Some(start) = self.as_table_like().iter().position(|(key, _)| key == old) else {
            return;
        };
        let mut rest = self.take_from(start).into_iter();
        if let Some((old, item)) = rest.next() {
            let renamed = new
                .with_leaf_decor(old.leaf_decor().clone())
                .with_dotted_decor(old.dotted_decor().clone());
            self.put_raw(renamed, item);
        }
        rest.for_each(|(key, item)| self.put_raw(key, item));
    }

    /// [`Self::put`] without moving the space before a closing brace, for
    /// entries put back in their own order.
    fn put_raw(&mut self, key: Key, item: Item) {
        match self {
            Self::Table(table) => {
                table.insert_formatted(&key, item);
            }
            Self::Inline(table) => {
                if let Ok(value) = item.into_value() {
                    table.insert_formatted(&key, value);
                }
            }
        }
    }
}

fn walk(table: &dyn TableLike, path: &[&str], at: Location, found: &mut Vec<Location>) {
    let Some((key, rest)) = path.split_first() else {
        found.push(at);
        return;
    };
    match table.get(key) {
        Some(Item::Table(child)) => {
            walk(child, rest, at.enter(key, None, child.is_dotted()), found)
        }
        Some(Item::ArrayOfTables(array)) => {
            for (index, child) in array.iter().enumerate() {
                walk(
                    child,
                    rest,
                    at.clone().enter(key, Some(index), false),
                    found,
                );
            }
        }
        Some(Item::Value(Value::InlineTable(child))) => {
            walk(child, rest, at.enter_inline(key), found)
        }
        _ => {}
    }
}

fn parent_mut<'a>(root: &'a mut Table, segments: &[Segment]) -> Option<Parent<'a>> {
    segments
        .iter()
        .try_fold(Parent::Table(root), |parent, segment| {
            let key = segment.key.as_str();
            match parent {
                Parent::Table(table) => match (table.get_mut(key)?, segment.index) {
                    (Item::Table(child), None) => Some(Parent::Table(child)),
                    (Item::ArrayOfTables(array), Some(index)) => {
                        array.get_mut(index).map(Parent::Table)
                    }
                    (Item::Value(Value::InlineTable(child)), None) => Some(Parent::Inline(child)),
                    _ => None,
                },
                Parent::Inline(table) => match (table.get_mut(key)?, segment.index) {
                    (Value::InlineTable(child), None) => Some(Parent::Inline(child)),
                    _ => None,
                },
            }
        })
}

/// Edits the prefix of the line that renders `key` of `table`: its header
/// when it is a table, or the first line it renders when that table has no
/// header of its own.
fn edit_line_prefix(table: &mut Table, key: &str, edit: impl FnOnce(&str) -> String) {
    match table.get_mut(key) {
        Some(Item::Table(child)) if child.is_dotted() || child.is_implicit() => {
            let first = child.iter().next().map(|(key, _)| key.to_owned());
            if let Some(first) = first {
                edit_line_prefix(child, &first, edit);
            }
        }
        Some(Item::Table(child)) => edit_prefix(child.decor_mut(), TABLE_PREFIX, edit),
        Some(Item::ArrayOfTables(array)) => {
            if let Some(first) = array.get_mut(0) {
                edit_prefix(first.decor_mut(), TABLE_PREFIX, edit);
            }
        }
        Some(Item::Value(_)) => {
            if let Some(mut key) = table.key_mut(key) {
                edit_prefix(key.leaf_decor_mut(), "", edit);
            }
        }
        Some(Item::None) | None => {}
    }
}

fn edit_prefix(decor: &mut Decor, default: &str, edit: impl FnOnce(&str) -> String) {
    let edited = edit(
        decor
            .prefix()
            .and_then(RawString::as_str)
            .unwrap_or(default),
    );
    decor.set_prefix(edited);
}

fn edit_suffix(decor: &mut Decor, edit: impl FnOnce(&str) -> String) {
    let edited = edit(
        decor
            .suffix()
            .and_then(RawString::as_str)
            .unwrap_or_default(),
    );
    decor.set_suffix(edited);
}

/// The key paths of the lines under `table`'s header, in the order they
/// render.
fn body_keys(table: &Table) -> Vec<Vec<String>> {
    table
        .get_values()
        .into_iter()
        .map(|(keys, _)| keys.iter().map(|key| key.get().to_owned()).collect())
        .collect()
}

/// The prefix of the line at `keys` under `table`'s header, and the line
/// itself.
fn body_line(table: &Table, keys: &[String]) -> Option<(String, String)> {
    let (path, value) = table.get_values().into_iter().find(|(path, _)| {
        path.iter()
            .map(|key| key.get())
            .eq(keys.iter().map(String::as_str))
    })?;
    let leaf = path.last()?.leaf_decor();
    let prefix = leaf
        .prefix()
        .and_then(RawString::as_str)
        .unwrap_or_default()
        .to_owned();
    let suffix = leaf
        .suffix()
        .and_then(RawString::as_str)
        .unwrap_or(KEY_SUFFIX);
    let key = path
        .iter()
        .map(|key| key.display_repr())
        .collect::<Vec<_>>()
        .join(PATH_SEPARATOR);
    Some((prefix, format!("{key}{suffix}{KEY_VALUE_SEPARATOR}{value}")))
}

fn edit_body_prefix(table: &mut Table, keys: &[String], edit: impl FnOnce(&str) -> String) {
    if let Some((parent, leaf)) = dotted_parent(table, keys)
        && let Some(mut key) = parent.key_mut(leaf)
    {
        edit_prefix(key.leaf_decor_mut(), "", edit);
    }
}

/// The dotted table that holds the line at `keys`, and the line's own key.
fn dotted_parent<'a, 'k>(
    table: &'a mut Table,
    keys: &'k [String],
) -> Option<(&'a mut Table, &'k str)> {
    let (leaf, path) = keys.split_last()?;
    let parent = path
        .iter()
        .try_fold(table, |table, key| table.get_mut(key)?.as_table_mut())?;
    Some((parent, leaf))
}

/// The table rendered first, as a path from the root.
fn first_table(root: &Table) -> Option<Vec<Segment>> {
    let mut first: Option<(isize, Vec<Segment>)> = None;
    visit_tables(root, &mut Vec::new(), &mut |table, path| {
        let visible = !table.is_dotted() && !(table.is_implicit() && table.get_values().is_empty());
        let position = table.position().unwrap_or(isize::MAX);
        if visible && first.as_ref().is_none_or(|(first, _)| position < *first) {
            first = Some((position, path.to_vec()));
        }
    });
    first.map(|(_, path)| path)
}

fn max_position(root: &Table) -> isize {
    let mut max = 0;
    visit_tables(root, &mut Vec::new(), &mut |table, _| {
        max = max.max(table.position().unwrap_or_default());
    });
    max
}

fn visit_tables(
    table: &Table,
    path: &mut Vec<Segment>,
    visit: &mut impl FnMut(&Table, &[Segment]),
) {
    for (key, item) in table.iter() {
        let tables: Vec<(Option<usize>, &Table)> = match item {
            Item::Table(child) => vec![(None, child)],
            Item::ArrayOfTables(array) => array
                .iter()
                .enumerate()
                .map(|(index, child)| (Some(index), child))
                .collect(),
            Item::Value(_) | Item::None => Vec::new(),
        };
        for (index, child) in tables {
            path.push(Segment {
                key: key.to_owned(),
                index,
            });
            visit(child, path);
            visit_tables(child, path, visit);
            path.pop();
        }
    }
}

/// Removes `index`, giving its spacing to the element that takes its place
/// at the front.
fn remove_element(array: &mut Array, index: usize) {
    let removed = array.remove(index);
    if index == 0
        && let (Some(next), Some(prefix)) = (array.get_mut(0), removed.decor().prefix())
    {
        next.decor_mut().set_prefix(prefix.clone());
    }
}

/// `value` as it reads without the spacing and comments around it.
fn bare(value: &Value) -> String {
    let mut value = value.clone();
    value.decor_mut().clear();
    value.to_string()
}

fn dotted(path: &[&str], key: &str) -> String {
    path.iter()
        .chain([&key])
        .map(|segment| Key::new(*segment).display_repr().into_owned())
        .collect::<Vec<_>>()
        .join(PATH_SEPARATOR)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;
    use toml_edit::{DocumentMut, Value};

    use super::ArrayRename;
    use crate::migration::{ConfigFormat, MigrationError, Step, Upgrade, migrate, test_stamp};

    type Apply = fn(&mut Upgrade<'_>) -> Result<(), MigrationError>;

    macro_rules! upgraded {
        ($what:literal) => {
            concat!("# Upgraded by Caudra 9.9.9 on 2026-10-20: ", $what, ".\n")
        };
    }

    const FILE: &str = "test.toml";
    const VERSION_1: &str = "version = 1\n";
    const VERSION_2: &str = concat!(upgraded!("version 1 → 2"), "version = 2\n");
    const UNPARSEABLE: &str = "the upgraded text is not valid TOML";

    fn upgrade(body: &str, apply: Apply) -> Result<String, MigrationError> {
        let steps: &'static [Step] = Box::leak(Box::new([Step {
            from: 1,
            why: None,
            apply,
        }]));
        let format = ConfigFormat {
            file: FILE,
            latest: 2,
            steps,
        };
        let text = migrate(&format!("{VERSION_1}{body}"), &format, &test_stamp())?
            .text
            .unwrap_or_default();
        text.parse::<DocumentMut>().expect(UNPARSEABLE);
        Ok(text.strip_prefix(VERSION_2).unwrap_or(&text).to_owned())
    }

    fn rename_code_lines(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.rename_key(&["ui", "tool_output_lines"], "python_execution", "code")
    }

    fn rename_shell_under_star(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.rename_key(&["*"], "bash", "shell")
    }

    fn rename_shell(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.rename_key(&[], "bash", "shell")
    }

    fn move_timeout(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.move_key(&["agent"], "bash_timeout_secs", &["plugins", "bash"])
    }

    fn move_tools_table(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.move_key(&["tools"], "bash", &["plugins"])
    }

    fn rename_tools(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.rename_in_array(
            &["agent"],
            "disabled_tools",
            &[
                ("code_context", ArrayRename::To("source_code_context")),
                ("python_execution", ArrayRename::Retire),
            ],
        );
        Ok(())
    }

    fn eager_mode(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.map_value(&["agent"], "mode", |value| {
            (value.as_str() == Some("lazy")).then(|| Value::from("eager"))
        });
        Ok(())
    }

    fn retire_defer(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.retire_key(&["agent"], "defer_builtin_tools");
        Ok(())
    }

    fn retire_always_load(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.retire_key(&["servers"], "always_load");
        Ok(())
    }

    #[test_case(
        "\n[ui.tool_output_lines]\n\n# keep this short\npython_execution = 40\n",
        concat!("\n[ui.tool_output_lines]\n\n", upgraded!("renamed `ui.tool_output_lines.python_execution` to `code`"), "#\n# keep this short\ncode = 40\n"), rename_code_lines
        ; "standard_table_keeps_blank_lines_first_and_comments_below"
    )]
    #[test_case(
        "ui.tool_output_lines.python_execution = 3\n",
        concat!(upgraded!("renamed `ui.tool_output_lines.python_execution` to `code`"), "ui.tool_output_lines.code = 3\n"), rename_code_lines
        ; "dotted_at_the_root"
    )]
    #[test_case(
        "[ui]\ntool_output_lines.python_execution = 3 # three\nother = 1\n",
        concat!("[ui]\n", upgraded!("renamed `ui.tool_output_lines.python_execution` to `code`"), "tool_output_lines.code = 3 # three\nother = 1\n"), rename_code_lines
        ; "dotted_in_a_table"
    )]
    #[test_case(
        "ui = { tool_output_lines = { python_execution = 3, other = 1 } }\n",
        concat!(upgraded!("renamed `ui.tool_output_lines.python_execution` to `code`"), "ui = { tool_output_lines = { code = 3, other = 1 } }\n"), rename_code_lines
        ; "inline_tables_note_their_line"
    )]
    #[test_case(
        "[ui.tool_output_lines]\nfirst = 1\npython_execution = 2\nlast = 3\n",
        concat!("[ui.tool_output_lines]\nfirst = 1\n", upgraded!("renamed `ui.tool_output_lines.python_execution` to `code`"), "code = 2\nlast = 3\n"), rename_code_lines
        ; "keeps_its_place"
    )]
    #[test_case("[ui]\nx = 1\n", "[ui]\nx = 1\n", rename_code_lines ; "absent_is_a_no_op")]
    #[test_case(
        "[\"*\".bash]\nallow = [\"ls\"]\n",
        concat!(upgraded!("renamed `\"*\".bash` to `shell`"), "[\"*\".shell]\nallow = [\"ls\"]\n"), rename_shell_under_star
        ; "table_under_a_quoted_key"
    )]
    #[test_case(
        "\n# shell rules\n[bash.rules]\nx = 1\n",
        concat!("\n", upgraded!("renamed `bash` to `shell`"), "#\n# shell rules\n[shell.rules]\nx = 1\n"), rename_shell
        ; "implicit_table_notes_its_first_header"
    )]
    #[test_case(
        "[[bash]]\nx = 1\n[[bash]]\nx = 2\n",
        concat!(upgraded!("renamed `bash` to `shell`"), "[[shell]]\nx = 1\n[[shell]]\nx = 2\n"), rename_shell
        ; "array_of_tables"
    )]
    #[test_case(
        "[agent]\nbash_timeout_secs = 30\nmodel = \"a\"\n",
        concat!("[agent]\nmodel = \"a\"\n\n[plugins.bash]\n", upgraded!("moved `agent.bash_timeout_secs` to `plugins.bash.bash_timeout_secs`"), "bash_timeout_secs = 30\n"), move_timeout
        ; "move_creates_a_standard_table"
    )]
    #[test_case(
        "[agent]\n# seconds\nbash_timeout_secs = 30\n\n[plugins.bash]\nenabled = true\n",
        concat!("[agent]\n\n[plugins.bash]\nenabled = true\n", upgraded!("moved `agent.bash_timeout_secs` to `plugins.bash.bash_timeout_secs`"), "#\n# seconds\nbash_timeout_secs = 30\n"), move_timeout
        ; "move_into_an_existing_table_keeps_comments"
    )]
    #[test_case(
        "agent.bash_timeout_secs = 30\nplugins = { bash = { enabled = true } }\n",
        concat!(upgraded!("moved `agent.bash_timeout_secs` to `plugins.bash.bash_timeout_secs`"), "plugins = { bash = { enabled = true, bash_timeout_secs = 30 } }\n"), move_timeout
        ; "move_from_dotted_into_inline"
    )]
    #[test_case(
        "[tools.bash]\ntimeout = 3\n",
        concat!(upgraded!("moved `tools.bash` to `plugins.bash`"), "[plugins.bash]\ntimeout = 3\n"), move_tools_table
        ; "move_a_table_in_place"
    )]
    #[test_case(
        "[agent]\ndisabled_tools = [\"code_context\", \"python_execution\", \"shell\"]\n",
        concat!("[agent]\n", upgraded!("renamed `code_context` to `source_code_context` and removed `python_execution` in `agent.disabled_tools`"), "disabled_tools = [\"source_code_context\", \"shell\"]\n"), rename_tools
        ; "array_rename_and_retire"
    )]
    #[test_case(
        "[agent]\ndisabled_tools = [\"python_execution\", \"shell\"]\n",
        concat!("[agent]\n", upgraded!("removed `python_execution` in `agent.disabled_tools`"), "disabled_tools = [\"shell\"]\n"), rename_tools
        ; "array_first_element_retired"
    )]
    #[test_case(
        "[agent]\ndisabled_tools = [\n  \"python_execution\",\n  \"shell\",\n]\n",
        concat!("[agent]\n", upgraded!("removed `python_execution` in `agent.disabled_tools`"), "disabled_tools = [\n  \"shell\",\n]\n"), rename_tools
        ; "array_on_several_lines"
    )]
    #[test_case(
        "[agent]\ndisabled_tools = [\"code_context\", \"source_code_context\"]\n",
        concat!("[agent]\n", upgraded!("removed `code_context`, as `source_code_context` is listed in `agent.disabled_tools`"), "disabled_tools = [\"source_code_context\"]\n"), rename_tools
        ; "array_rename_to_a_listed_string_drops_it"
    )]
    #[test_case(
        "agent = { disabled_tools = [\"python_execution\"] }\n",
        concat!(upgraded!("removed `python_execution` in `agent.disabled_tools`"), "agent = { disabled_tools = [] }\n"), rename_tools
        ; "array_in_an_inline_table"
    )]
    #[test_case(
        "[agent]\nmode = \"lazy\" # fast\n",
        concat!("[agent]\n", upgraded!("changed `agent.mode` from \"lazy\" to \"eager\""), "mode = \"eager\" # fast\n"), eager_mode
        ; "map_value_keeps_the_comment"
    )]
    #[test_case("[agent]\nmode = \"eager\"\n", "[agent]\nmode = \"eager\"\n", eager_mode ; "map_value_unmapped_is_a_no_op")]
    #[test_case(
        "[agent]\nmodel = \"a\"\n\n# old\ndefer_builtin_tools = true\nother = 1\n",
        concat!("[agent]\nmodel = \"a\"\n\n", upgraded!("retired `agent.defer_builtin_tools`"), "#\n# old\n# defer_builtin_tools = true\nother = 1\n"), retire_defer
        ; "retire_between_lines"
    )]
    #[test_case(
        "[agent]\nmodel = \"a\"\ndefer_builtin_tools = true # why\n\n[ui]\nx = 1\n",
        concat!("[agent]\nmodel = \"a\"\n", upgraded!("retired `agent.defer_builtin_tools`"), "# defer_builtin_tools = true # why\n\n[ui]\nx = 1\n"), retire_defer
        ; "retire_last_in_table"
    )]
    #[test_case(
        "[agent]\ndefer_builtin_tools = true\n",
        concat!("[agent]\n", upgraded!("retired `agent.defer_builtin_tools`"), "# defer_builtin_tools = true\n"), retire_defer
        ; "retire_only_key_last_in_file"
    )]
    #[test_case(
        "agent.defer_builtin_tools = true\nmodel = 1\n",
        concat!(upgraded!("retired `agent.defer_builtin_tools`"), "# agent.defer_builtin_tools = true\nmodel = 1\n"), retire_defer
        ; "retire_dotted_at_the_root"
    )]
    #[test_case(
        "model = 1\nagent.defer_builtin_tools = true\n",
        concat!("model = 1\n", upgraded!("retired `agent.defer_builtin_tools`"), "# agent.defer_builtin_tools = true\n"), retire_defer
        ; "retire_dotted_last_in_file"
    )]
    #[test_case(
        "agent = { model = \"a\", defer_builtin_tools = true }\n",
        concat!(upgraded!("retired `agent.defer_builtin_tools`"), "# defer_builtin_tools = true\nagent = { model = \"a\" }\n"), retire_defer
        ; "retire_from_an_inline_table"
    )]
    #[test_case(
        "[[servers]]\nname = \"a\"\nalways_load = true\n\n[[servers]]\nname = \"b\"\nalways_load = false\n",
        concat!("[[servers]]\nname = \"a\"\n", upgraded!("retired `servers.always_load`"), "# always_load = true\n\n[[servers]]\nname = \"b\"\n", upgraded!("retired `servers.always_load`"), "# always_load = false\n"),
        retire_always_load
        ; "retire_in_every_table_of_an_array"
    )]
    #[test_case("[agent.defer_builtin_tools]\nx = 1\n", "[agent.defer_builtin_tools]\nx = 1\n", retire_defer ; "retire_leaves_tables")]
    fn primitives_edit_in_place_and_note_the_line(body: &str, expected: &str, apply: Apply) {
        assert_eq!(upgrade(body, apply), Ok(expected.to_owned()));
    }
}
