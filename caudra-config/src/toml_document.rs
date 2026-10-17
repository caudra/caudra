//! Edits to TOML files a user also writes by hand. Each change lands in the
//! parsed document, so comments, quoting and order outside it survive.

use serde::Serialize;
use serde::ser::Error as _;
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, TomlError, Value};

/// `value` rendered by serde as a table that belongs to no document yet.
#[expect(
    clippy::disallowed_methods,
    reason = "renders new values only; they are merged into the edited document"
)]
pub(crate) fn to_table(value: &impl Serialize) -> Result<Table, toml::ser::Error> {
    let document: DocumentMut = toml::to_string(value)?
        .parse()
        .map_err(|error: TomlError| toml::ser::Error::custom(error.to_string()))?;
    let mut table = document.into_table();
    table.set_implicit(false);
    Ok(table)
}

/// Brings `current` from `previous` to `desired`, touching only what changed.
/// A key whose rendering did not change keeps its formatting, a replaced value
/// keeps its comments, and an inline table stays inline.
pub(crate) fn merge_table(
    current: &mut dyn TableLike,
    previous: &dyn TableLike,
    desired: &dyn TableLike,
) {
    let dropped: Vec<String> = current
        .iter()
        .map(|(key, _)| key)
        .filter(|key| !desired.contains_key(key))
        .map(str::to_owned)
        .collect();
    for key in dropped {
        current.remove(&key);
    }
    for (key, wanted) in desired.iter() {
        merge_item(current, key, previous.get(key), wanted);
    }
}

/// Brings `parent[key]` from `previous` to `desired` the way [`merge_table`]
/// does, inserting it when the file lacks it.
pub(crate) fn merge_item(
    parent: &mut dyn TableLike,
    key: &str,
    previous: Option<&Item>,
    desired: &Item,
) {
    if previous.is_some_and(|previous| same_item(previous, desired)) {
        return;
    }
    match (parent.get_mut(key), desired) {
        (Some(Item::Value(existing)), Item::Value(desired)) if !existing.is_inline_table() => {
            replace_value(existing, desired.clone());
        }
        (Some(existing), desired) if existing.is_table_like() && desired.is_table_like() => {
            let empty = Table::new();
            let previous = previous.and_then(Item::as_table_like).unwrap_or(&empty);
            if let Some(desired) = desired.as_table_like() {
                match existing.as_inline_table_mut() {
                    Some(inline) => merge_inline_table(inline, previous, desired),
                    None => {
                        if let Some(existing) = existing.as_table_like_mut() {
                            merge_table(existing, previous, desired);
                        }
                    }
                }
            }
        }
        _ => {
            let mut item = desired.clone();
            detach(&mut item);
            parent.insert(key, item);
        }
    }
}

/// The space before an inline table's closing brace belongs to its last value,
/// so it moves to whichever value is last after the merge.
fn merge_inline_table(table: &mut InlineTable, previous: &dyn TableLike, desired: &dyn TableLike) {
    let closing = last_value(table).and_then(|value| {
        let closing = value.decor().suffix().cloned();
        value.decor_mut().set_suffix("");
        closing
    });
    merge_table(table, previous, desired);
    if let (Some(closing), Some(value)) = (closing, last_value(table)) {
        value.decor_mut().set_suffix(closing);
    }
}

fn last_value(table: &mut InlineTable) -> Option<&mut Value> {
    table.iter_mut().last().map(|(_, value)| value)
}

/// Sets `key` to `value`, keeping the comments around a value it replaces.
pub fn set_value(table: &mut dyn TableLike, key: &str, value: Value) {
    match table.get_mut(key) {
        Some(Item::Value(existing)) => replace_value(existing, value),
        _ => {
            table.insert(key, Item::Value(value));
        }
    }
}

fn replace_value(existing: &mut Value, value: Value) {
    let decor = existing.decor().clone();
    *existing = value;
    *existing.decor_mut() = decor;
}

fn same_item(left: &Item, right: &Item) -> bool {
    match (left.as_table_like(), right.as_table_like()) {
        (Some(left), Some(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, value)| right.get(key).is_some_and(|other| same_item(value, other)))
        }
        (None, None) => match (left.as_value(), right.as_value()) {
            (Some(left), Some(right)) => left.to_string() == right.to_string(),
            _ => false,
        },
        _ => false,
    }
}

/// Positions and spacing come from the scratch document a value was rendered
/// in, so they would interleave its tables with the edited file's. Without
/// them each table follows the one before it, spaced like a new table.
fn detach(item: &mut Item) {
    match item {
        Item::Table(table) => detach_table(table),
        Item::ArrayOfTables(array) => array.iter_mut().for_each(detach_table),
        Item::None | Item::Value(_) => {}
    }
}

fn detach_table(table: &mut Table) {
    table.set_position(None);
    table.decor_mut().clear();
    table.iter_mut().for_each(|(_, child)| detach(child));
}

#[cfg(test)]
mod tests {
    use toml_edit::{DocumentMut, Value};

    use super::{merge_table, set_value};

    const KEPT_COMMENT: &str = "# keep this note";
    const COMMENT_LOST: &str = "a comment in the edited file was dropped";
    const LAYOUT_CHANGED: &str = "the edit changed the file's layout";

    fn document(source: &str) -> DocumentMut {
        source.parse().unwrap()
    }

    fn merged(current: &str, previous: &str, desired: &str) -> String {
        let mut current = document(current);
        merge_table(
            current.as_table_mut(),
            document(previous).as_table(),
            document(desired).as_table(),
        );
        current.to_string()
    }

    #[test]
    fn a_changed_value_keeps_its_comment_and_its_neighbours_formatting() {
        let current = format!("[a]\nkept = 'literal' {KEPT_COMMENT}\nchanged = 1 {KEPT_COMMENT}\n");
        let previous = "[a]\nkept = \"literal\"\nchanged = 1\n";
        let desired = "[a]\nkept = \"literal\"\nchanged = 2\n";

        let rendered = merged(&current, previous, desired);

        assert_eq!(
            rendered,
            format!("[a]\nkept = 'literal' {KEPT_COMMENT}\nchanged = 2 {KEPT_COMMENT}\n"),
            "{COMMENT_LOST}"
        );
    }

    #[test]
    fn an_inline_table_stays_inline() {
        let current = "a = { x = 1, y = 2 }\n";
        let previous = "[a]\nx = 1\ny = 2\n";
        let desired = "[a]\nx = 3\ny = 2\n";

        assert_eq!(merged(current, previous, desired), "a = { x = 3, y = 2 }\n");
    }

    #[test]
    fn a_new_table_follows_the_existing_ones() {
        let current = format!("{KEPT_COMMENT}\n[a]\nx = 1\n\n[b]\nx = 2\n");
        let previous = "[a]\nx = 1\n\n[b]\nx = 2\n";
        let desired = "[new]\nx = 3\n\n[a]\nx = 1\n\n[b]\nx = 2\n";

        assert_eq!(
            merged(&current, previous, desired),
            format!("{KEPT_COMMENT}\n[a]\nx = 1\n\n[b]\nx = 2\n\n[new]\nx = 3\n"),
            "{LAYOUT_CHANGED}"
        );
    }

    #[test]
    fn a_removed_key_goes_and_the_rest_stays() {
        let current = format!("[a]\nx = 1\ny = 2 {KEPT_COMMENT}\n");
        let previous = "[a]\nx = 1\ny = 2\n";
        let desired = "[a]\ny = 2\n";

        assert_eq!(
            merged(&current, previous, desired),
            format!("[a]\ny = 2 {KEPT_COMMENT}\n"),
            "{COMMENT_LOST}"
        );
    }

    #[test]
    fn set_value_keeps_the_trailing_comment() {
        let mut edited = document(&format!("[a]\nenabled = true {KEPT_COMMENT}\n"));
        let table = edited["a"].as_table_like_mut().unwrap();

        set_value(table, "enabled", Value::from(false));
        set_value(table, "added", Value::from(1));

        assert_eq!(
            edited.to_string(),
            format!("[a]\nenabled = false {KEPT_COMMENT}\nadded = 1\n"),
            "{COMMENT_LOST}"
        );
    }
}
