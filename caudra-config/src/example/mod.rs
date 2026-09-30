//! Commented references for the TOML config files, built from the metadata
//! the docs use. A reference parses as its file and changes nothing.

use std::fmt::Write;

use crate::config_version::CONFIG_VERSION_KEY;
use crate::experimental::Feature;
use crate::files::{ConfigFile, Scope};
use crate::{ConfigField, ConfigValue};

pub mod caudra;
pub mod mcp;
pub mod permissions;
pub mod providers;
pub mod sandboxes;
pub mod workcell;

const COMMENT: &str = "# ";
const PARAGRAPH_BREAK: &str = "#\n";
const CODE_SPAN: char = '`';
const WIDTH: usize = 79;
const GLOBAL_ONLY: &str = " There is no project file.";
const RECORDS_USAGE: &str = "Each commented table header starts an example. To use one, copy \
     the header and the lines you need, remove the \"#\", and put your own names and values in \
     place of the examples. Leave the lines without a \"#\" as they are. A required key shows a \
     sample value, and any other key shows its default. A value in angle brackets, such as \
     <string>, marks a key with no default to show.";

/// A file's reference: the comment it opens with, then its tables in the order
/// the file lists them.
pub struct Document {
    pub preamble: Vec<String>,
    pub version: u32,
    pub tables: Vec<Table>,
}

/// How a table's header appears in a reference.
pub enum Header {
    /// Keys at the top of the file, before any header.
    Root,
    /// A table the file always has, such as `[ui]`, so its header stays live.
    Fixed(String),
    /// One named record, such as `[mcp.filesystem]`, which exists only once
    /// written.
    Record(String),
    /// One element of an array of tables, such as `[[my-provider.models]]`.
    RecordArray(String),
}

pub struct Table {
    pub header: Header,
    pub about: Option<String>,
    pub entries: Vec<Entry>,
}

/// One key: a [`ConfigField`] whose description may be built at runtime.
pub struct Entry {
    pub name: &'static str,
    pub ty: &'static str,
    pub default: ConfigValue,
    pub min: Option<u64>,
    pub max: Option<u64>,
    pub env: Option<&'static str>,
    pub description: String,
}

/// Which lines of a reference are live TOML.
#[derive(Clone, Copy)]
pub enum Render {
    /// Only `version` and fixed headers, so the text changes nothing.
    Reference,
    /// Record headers and required samples too, and every stated default
    /// when `defaults` is set. Tests parse it to check the metadata.
    Live { defaults: bool },
}

impl Document {
    pub fn render(&self, render: Render) -> String {
        let mut out = String::new();
        for (index, paragraph) in self.preamble.iter().enumerate() {
            if index > 0 {
                out.push_str(PARAGRAPH_BREAK);
            }
            comment(&mut out, paragraph);
        }
        let _ = writeln!(out, "\n{CONFIG_VERSION_KEY} = {}", self.version);
        for table in &self.tables {
            if let Some(header) = table.header.line(render) {
                let _ = writeln!(out, "\n{header}");
            }
            if let Some(about) = &table.about {
                comment(&mut out, about);
            }
            for entry in &table.entries {
                if self.has_own_table(table.header.path(), entry.name) {
                    continue;
                }
                out.push('\n');
                comment(&mut out, &full_stop(&entry.description));
                comment(&mut out, &facts(entry));
                let _ = writeln!(out, "{}", entry.line(render));
            }
        }
        out
    }

    pub fn table(&self, path: &str) -> Option<&Table> {
        self.tables.iter().find(|table| table.header.path() == path)
    }

    /// A table-valued key written as tables of its own, like `rules` under
    /// `[agent.steering]`, would clash with them as an inline `{}`.
    fn has_own_table(&self, parent: &str, name: &str) -> bool {
        let path = if parent.is_empty() {
            name.to_owned()
        } else {
            format!("{parent}.{name}")
        };
        self.tables.iter().any(|table| {
            table
                .header
                .path()
                .strip_prefix(&path)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        })
    }
}

impl Render {
    fn records(self) -> bool {
        matches!(self, Self::Live { .. })
    }

    fn defaults(self) -> bool {
        matches!(self, Self::Live { defaults: true })
    }
}

impl Header {
    pub fn path(&self) -> &str {
        match self {
            Self::Root => "",
            Self::Fixed(path) | Self::Record(path) | Self::RecordArray(path) => path,
        }
    }

    fn line(&self, render: Render) -> Option<String> {
        let prefix = if render.records() { "" } else { COMMENT };
        match self {
            Self::Root => None,
            Self::Fixed(path) => Some(format!("[{path}]")),
            Self::Record(path) => Some(format!("{prefix}[{path}]")),
            Self::RecordArray(path) => Some(format!("{prefix}[[{path}]]")),
        }
    }
}

impl Table {
    fn new(header: Header, entries: Vec<Entry>) -> Self {
        Self {
            header,
            about: None,
            entries,
        }
    }

    fn of<'a>(header: Header, fields: impl IntoIterator<Item = &'a ConfigField>) -> Self {
        Self::new(header, fields.into_iter().map(Entry::from).collect())
    }

    fn about(mut self, about: impl Into<String>) -> Self {
        self.about = Some(about.into());
        self
    }
}

impl Entry {
    fn new(
        name: &'static str,
        ty: &'static str,
        default: ConfigValue,
        description: String,
    ) -> Self {
        Self {
            name,
            ty,
            default,
            min: None,
            max: None,
            env: None,
            description,
        }
    }

    fn line(&self, render: Render) -> String {
        let (value, live) = match self.default {
            ConfigValue::Required(sample) => (sample.to_owned(), render.records()),
            default => match default.toml() {
                Some(value) => (value, render.defaults()),
                None => (format!("<{}>", self.ty), false),
            },
        };
        let prefix = if live { "" } else { COMMENT };
        format!("{prefix}{} = {value}", self.name)
    }
}

impl From<&ConfigField> for Entry {
    fn from(field: &ConfigField) -> Self {
        Self {
            min: field.min,
            max: field.max,
            env: field.env,
            ..Self::new(
                field.name,
                field.ty,
                field.default,
                field.description.to_owned(),
            )
        }
    }
}

/// What the file is, then `body`, then the switch it needs and its docs.
fn preamble(file: &ConfigFile, body: impl IntoIterator<Item = String>) -> Vec<String> {
    let intro = format!(
        "Every {} setting. Each one is commented out, so this file changes nothing until you \
         edit it. `{}` prints it.",
        file.name,
        file.example_command()
    );
    let mut paragraphs = vec![intro];
    paragraphs.extend(body);
    paragraphs.extend(file.feature.map(needs));
    paragraphs.push(format!("Full reference: {}", file.docs_url()));
    paragraphs
}

fn global_location(file: &ConfigFile) -> String {
    let mut location = format!(
        "The global file is ~/.config/caudra/{name}, or %APPDATA%\\caudra\\{name} on Windows.",
        name = file.name
    );
    if !file.scopes.contains(&Scope::Project) {
        location.push_str(GLOBAL_ONLY);
    }
    location
}

fn needs(feature: Feature) -> String {
    format!(
        "Caudra reads this file only while `{}` is true under [experimental] in the global \
         caudra.toml.",
        feature.key()
    )
}

fn facts(entry: &Entry) -> String {
    let mut facts = format!("Type: {}", entry.ty);
    let _ = match (entry.min, entry.max) {
        (Some(min), Some(max)) => write!(facts, ", {min} to {max}"),
        (Some(min), None) => write!(facts, ", at least {min}"),
        (None, Some(max)) => write!(facts, ", at most {max}"),
        (None, None) => Ok(()),
    };
    match entry.default {
        ConfigValue::Unset | ConfigValue::Varies(_) => {
            let _ = write!(facts, ". Default: {}", entry.default.format_default());
        }
        ConfigValue::Required(_) => facts.push_str(". Required"),
        _ => {}
    }
    if let Some(env) = entry.env {
        let _ = write!(facts, ". Env: {env}");
    }
    facts.push('.');
    facts
}

fn code_list(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    quoted.join(", ")
}

fn full_stop(text: &str) -> String {
    if text.is_empty() || text.ends_with(['.', '!', '?']) {
        text.to_owned()
    } else {
        format!("{text}.")
    }
}

fn comment(out: &mut String, text: &str) {
    let mut line = String::new();
    for word in unbroken_words(text) {
        if !line.is_empty() && COMMENT.len() + line.len() + 1 + word.len() > WIDTH {
            let _ = writeln!(out, "{COMMENT}{line}");
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&word);
    }
    let _ = writeln!(out, "{COMMENT}{line}");
}

/// Words split on whitespace, except that a code span stays whole, so a
/// wrapped comment never breaks `caudra storage` across two lines.
fn unbroken_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut open_span: Option<String> = None;
    for word in text.split_whitespace() {
        let word = match open_span.take() {
            Some(span) => format!("{span} {word}"),
            None => word.to_owned(),
        };
        if word.matches(CODE_SPAN).count() % 2 == 1 {
            open_span = Some(word);
        } else {
            words.push(word);
        }
    }
    words.extend(open_span);
    words
}

#[cfg(test)]
mod tests {
    use test_case::test_case;
    use toml::{Table as TomlTable, Value as TomlValue};

    use super::{COMMENT, Header, Render, unbroken_words};
    use crate::files::CONFIG_FILES;

    /// The table a header writes to. An array of tables writes to its last
    /// element.
    fn values<'a>(root: &'a TomlTable, header: &Header) -> &'a TomlTable {
        let mut table = root;
        for key in header.path().split('.').filter(|key| !key.is_empty()) {
            table = match &table[key] {
                TomlValue::Array(elements) => elements.last().and_then(TomlValue::as_table),
                value => value.as_table(),
            }
            .unwrap();
        }
        table
    }

    #[test]
    fn every_entry_is_described_and_sets_its_own_key() {
        let live = Render::Live { defaults: true };
        for document in CONFIG_FILES.iter().filter_map(|file| file.example) {
            let document = document();
            let root: TomlTable = document.render(live).parse().unwrap();
            for table in &document.tables {
                let values = values(&root, &table.header);
                for entry in &table.entries {
                    let key = format!("{}.{}", table.header.path(), entry.name);
                    assert!(!entry.description.is_empty(), "{key}");
                    if !entry.line(live).starts_with(COMMENT) {
                        assert!(values.contains_key(entry.name), "{key}");
                    }
                }
            }
        }
    }

    #[test_case("run `caudra storage` now", &["run", "`caudra storage`", "now"] ; "span_with_spaces")]
    #[test_case("`0` disables it", &["`0`", "disables", "it"] ; "one_word_span")]
    #[test_case("an `unclosed span", &["an", "`unclosed span"] ; "unclosed_span")]
    fn a_code_span_wraps_as_one_word(text: &str, expected: &[&str]) {
        assert_eq!(unbroken_words(text), expected);
    }
}
