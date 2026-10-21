//! Key-by-key edits to the global `caudra.toml`, for settings changed from inside Caudra. Each
//! edit touches one key, keeps the rest of the file as written, and commits only while the file
//! still holds what was read and still loads.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use crate::global_file::{GlobalDocument, GlobalFileError};
use crate::toml_document::set_value;

const UNEXPECTED_LAYOUT: &str = "a table on the way to the key is not a table";

#[derive(Debug, Clone)]
pub struct GlobalSettingsFile {
    file: GlobalDocument,
}

impl GlobalSettingsFile {
    /// A missing file is an empty one.
    pub fn load(path: &Path) -> Result<Self, GlobalFileError> {
        GlobalDocument::load(path).map(|file| Self { file })
    }

    pub fn path(&self) -> &Path {
        &self.file.path
    }

    pub fn document(&self) -> &DocumentMut {
        &self.file.document
    }

    /// `key` of the table at `table` as written, `None` while unset. An empty `table` is the root.
    pub fn get(&self, table: &[&str], key: &str) -> Option<&Item> {
        let mut current: &dyn TableLike = self.file.document.as_table();
        for name in table {
            current = current.get(name)?.as_table_like()?;
        }
        current.get(key).filter(|item| !item.is_none())
    }

    pub fn set(&self, table: &[&str], key: &str, value: Value) -> Result<Self, GlobalFileError> {
        self.edit(table, key, Some(value))
    }

    /// Removes `key`, and every table the removal left empty that has no header of its own.
    pub fn remove(&self, table: &[&str], key: &str) -> Result<Self, GlobalFileError> {
        self.edit(table, key, None)
    }

    pub(crate) fn edit(
        &self,
        table: &[&str],
        key: &str,
        value: Option<Value>,
    ) -> Result<Self, GlobalFileError> {
        let mut document = self.file.document.clone();
        let edited = match value {
            Some(value) => created_table(&mut document, table).map(|parent| {
                set_value(parent, key, value);
            }),
            None => {
                removed(&mut document, table, key);
                Ok(())
            }
        };
        edited.map_err(|()| GlobalFileError::Invalid {
            path: self.file.path.clone(),
            message: UNEXPECTED_LAYOUT.into(),
        })?;
        self.file.commit(&document).map(|file| Self { file })
    }
}

/// The table at `path`, creating the missing ones: the last with a header, the rest implicit.
fn created_table<'a>(
    document: &'a mut DocumentMut,
    path: &[&str],
) -> Result<&'a mut dyn TableLike, ()> {
    let mut current: &mut dyn TableLike = document.as_table_mut();
    for (index, name) in path.iter().enumerate() {
        let mut table = Table::new();
        table.set_implicit(index + 1 < path.len());
        current = current
            .entry(name)
            .or_insert(Item::Table(table))
            .as_table_like_mut()
            .ok_or(())?;
    }
    Ok(current)
}

fn removed(document: &mut DocumentMut, path: &[&str], key: &str) {
    fn remove_in(table: &mut dyn TableLike, path: &[&str], key: &str) {
        match path.split_first() {
            None => {
                table.remove(key);
            }
            Some((name, rest)) => {
                let Some(child) = table.get_mut(name) else {
                    return;
                };
                if let Some(child) = child.as_table_like_mut() {
                    remove_in(child, rest, key);
                }
                if table.get(name).is_some_and(is_empty_without_header) {
                    table.remove(name);
                }
            }
        }
    }
    remove_in(document.as_table_mut(), path, key);
}

fn is_empty_without_header(item: &Item) -> bool {
    let has_header = item
        .as_table()
        .is_some_and(|table| !table.is_implicit() && !table.is_dotted());
    !has_header && item.as_table_like().is_some_and(TableLike::is_empty)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::config_file::CONFIG_FILE;

    const ORIGINAL: &str =
        "# my settings\nversion = 3\n\n[ui]\n# keep this\ntheme = \"dark\" # trailing\n";
    const UI: &str = "ui";
    const SCROLLBAR: &str = "scrollbar";
    const THEME: &str = "theme";
    const ALWAYS_FAST: &str = "always_fast";

    fn written(contents: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn a_set_keeps_comments_and_a_remove_restores_the_file() {
        let (_dir, path) = written(ORIGINAL);
        let file = GlobalSettingsFile::load(&path).unwrap();
        let edited = file
            .set(&[UI], SCROLLBAR, Value::from(false))
            .unwrap()
            .set(&[], ALWAYS_FAST, Value::from(true))
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# keep this\ntheme = \"dark\" # trailing"),
            "{text}"
        );
        assert_eq!(
            edited.get(&[UI], SCROLLBAR).and_then(Item::as_bool),
            Some(false)
        );
        assert_eq!(
            edited.get(&[], ALWAYS_FAST).and_then(Item::as_bool),
            Some(true)
        );
        edited
            .remove(&[UI], SCROLLBAR)
            .unwrap()
            .remove(&[], ALWAYS_FAST)
            .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), ORIGINAL);
    }

    #[test]
    fn a_value_the_config_refuses_is_not_written() {
        let (_dir, path) = written(ORIGINAL);
        let file = GlobalSettingsFile::load(&path).unwrap();
        let error = file.set(&[UI], THEME, Value::from(1)).unwrap_err();
        assert!(matches!(error, GlobalFileError::Invalid { .. }), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), ORIGINAL);
    }
}
