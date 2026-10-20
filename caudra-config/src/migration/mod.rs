//! Upgrades a config file's text from the format version it declares to the
//! newest one. Every step edits the parsed document, so the user's layout and
//! comments survive, and every changed line gains a note naming the build and
//! the day that changed it.

mod edit;
mod note;

use std::borrow::Cow;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use thiserror::Error;
use toml_edit::{DocumentMut, Item, TomlError};

pub use edit::ArrayRename;

use crate::config_version::{
    CONFIG_VERSION_KEY, ConfigVersionError, FIRST_VERSION, checked_version,
};

const PRODUCT: &str = "Caudra";
const BOM: char = '\u{feff}';
const CRLF: &str = "\r\n";
const LF: &str = "\n";

/// A config file's format history: one [`Step`] per version, from the first
/// up to the one before `latest`.
#[derive(Debug)]
pub struct ConfigFormat {
    pub file: &'static str,
    pub latest: u32,
    pub steps: &'static [Step],
    /// Set for a file whose first version already required `version`, so one
    /// without it is refused rather than read as that version.
    pub requires_version: bool,
}

/// Brings a file from version `from` to `from + 1`.
#[derive(Debug)]
pub struct Step {
    pub from: u32,
    /// Appended to every note this step leaves, after "because".
    pub why: Option<&'static str>,
    pub apply: fn(&mut Upgrade<'_>) -> Result<(), MigrationError>,
}

/// Who upgraded a file, and when, as the notes name them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    pub by: String,
    pub on: Date,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub what: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migrated {
    pub from: u32,
    /// `None` when the file already declares the latest version.
    pub text: Option<String>,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum MigrationError {
    #[error("{file} is not valid TOML: {source}")]
    Parse {
        file: &'static str,
        source: TomlError,
    },
    #[error("{file}: {source}")]
    Version {
        file: &'static str,
        source: ConfigVersionError,
    },
    #[error(
        "{file} sets both `{old}` and `{new}`, which now mean the same thing: keep `{new}`, remove `{old}`, and start Caudra again"
    )]
    Conflict {
        file: &'static str,
        old: String,
        new: String,
    },
    #[error("{file} has no upgrade from version {from}: this Caudra build is broken")]
    MissingStep { file: &'static str, from: u32 },
    #[error(
        "{file} lists an upgrade from version {from} out of order: this Caudra build is broken"
    )]
    StrayStep { file: &'static str, from: u32 },
}

/// A config file's text in its latest format, ready for its parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The upgraded text, or the file's own when it was already current.
    pub text: String,
    /// The version the file declared, or the first one when it declared none.
    pub from: u32,
    /// Whether `text` differs from what the file holds.
    pub migrated: bool,
}

#[derive(Debug, Error)]
pub enum ReadError {
    #[error("read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid {}: {source}", path.display())]
    Invalid {
        path: PathBuf,
        #[source]
        source: Box<MigrationError>,
    },
}

/// The document a [`Step`] edits, and the changes it has made so far.
pub struct Upgrade<'a> {
    document: DocumentMut,
    file: &'static str,
    stamp: &'a Stamp,
    why: Option<&'static str>,
    changes: Vec<Change>,
}

impl Stamp {
    pub fn now() -> Self {
        Self {
            by: format!("{PRODUCT} {}", env!("CARGO_PKG_VERSION")),
            on: Timestamp::now().to_zoned(TimeZone::UTC).date(),
        }
    }
}

impl ConfigFormat {
    /// Fails unless the steps chain one by one from the first version to
    /// `latest`.
    pub fn check(&self) -> Result<(), MigrationError> {
        let mut steps = self.steps.iter();
        for from in FIRST_VERSION..self.latest {
            if steps.next().is_none_or(|step| step.from != from) {
                return Err(MigrationError::MissingStep {
                    file: self.file,
                    from,
                });
            }
        }
        match steps.next() {
            Some(step) => Err(MigrationError::StrayStep {
                file: self.file,
                from: step.from,
            }),
            None => Ok(()),
        }
    }

    fn step(&self, from: u32) -> Result<&Step, MigrationError> {
        self.steps
            .iter()
            .find(|step| step.from == from)
            .ok_or(MigrationError::MissingStep {
                file: self.file,
                from,
            })
    }
}

impl Upgrade<'_> {
    pub fn document(&self) -> &DocumentMut {
        &self.document
    }

    /// Records `what` and returns the note that goes above the changed line.
    fn record(&mut self, what: String) -> String {
        let note = note::upgraded(self.stamp, &what, self.why);
        self.changes.push(Change { what });
        note
    }
}

/// Upgrades `text`, a file in `format`, to its latest version. A file without
/// a version is the first version, and gains the key.
pub fn migrate(
    text: &str,
    format: &ConfigFormat,
    stamp: &Stamp,
) -> Result<Migrated, MigrationError> {
    let file = format.file;
    let (bom, body) = text
        .strip_prefix(BOM)
        .map_or((false, text), |body| (true, body));
    let crlf = uses_crlf(body);
    let body = if crlf {
        Cow::Owned(body.replace(CRLF, LF))
    } else {
        Cow::Borrowed(body)
    };
    let document: DocumentMut = body
        .parse()
        .map_err(|source| MigrationError::Parse { file, source })?;
    let declared = declared_version(&document, format)
        .map_err(|source| MigrationError::Version { file, source })?;
    if declared == Some(format.latest) {
        return Ok(Migrated {
            from: format.latest,
            text: None,
            changes: Vec::new(),
        });
    }
    let from = declared.unwrap_or(FIRST_VERSION);
    let mut upgrade = Upgrade {
        document,
        file,
        stamp,
        why: None,
        changes: Vec::new(),
    };
    upgrade.stamp_version(declared, format.latest);
    for version in from..format.latest {
        let step = format.step(version)?;
        upgrade.why = step.why;
        (step.apply)(&mut upgrade)?;
    }
    let mut upgraded = upgrade.document.to_string();
    if crlf {
        upgraded = upgraded.replace(LF, CRLF);
    }
    if bom {
        upgraded.insert(0, BOM);
    }
    Ok(Migrated {
        from,
        text: Some(upgraded),
        changes: upgrade.changes,
    })
}

/// The file at `path`, upgraded in memory. `None` when there is no file.
pub fn read(path: &Path, format: &ConfigFormat) -> Result<Option<Loaded>, ReadError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ReadError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    upgrade_text(text, format)
        .map(Some)
        .map_err(|source| ReadError::Invalid {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
}

/// `text`, a file in `format` already read, upgraded in memory.
pub fn upgrade_text(text: String, format: &ConfigFormat) -> Result<Loaded, MigrationError> {
    let Migrated {
        from,
        text: upgraded,
        ..
    } = migrate(&text, format, &Stamp::now())?;
    Ok(match upgraded {
        Some(upgraded) => Loaded {
            text: upgraded,
            from,
            migrated: true,
        },
        None => Loaded {
            text,
            from,
            migrated: false,
        },
    })
}

/// A file's line ending is the one its first line ends with.
fn uses_crlf(text: &str) -> bool {
    text.find(LF).is_some_and(|end| text[..end].ends_with('\r'))
}

fn declared_version(
    document: &DocumentMut,
    format: &ConfigFormat,
) -> Result<Option<u32>, ConfigVersionError> {
    match document.get(CONFIG_VERSION_KEY) {
        Some(item) => Item::as_integer(item)
            .ok_or(ConfigVersionError::Invalid)
            .and_then(|found| checked_version(found, format.latest))
            .map(Some),
        None if format.requires_version => Err(ConfigVersionError::Missing {
            latest: format.latest,
        }),
        None => Ok(None),
    }
}

#[cfg(test)]
const TEST_STAMP_BY: &str = "Caudra 9.9.9";

#[cfg(test)]
fn test_stamp() -> Stamp {
    Stamp {
        by: TEST_STAMP_BY.to_owned(),
        on: jiff::civil::date(2026, 10, 20),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        Change, ConfigFormat, ConfigVersionError, Loaded, Migrated, MigrationError, ReadError,
        Step, Upgrade, migrate, read, test_stamp, upgrade_text,
    };

    macro_rules! added {
        () => {
            "# Added by Caudra 9.9.9 on 2026-10-20: the format version of this file, which Caudra now requires.\nversion = 3\n"
        };
    }

    const FILE: &str = "test.toml";
    const LINE_ONE: &str = "line 1";
    const RENAME_WHY: &str = "the `code` tool now runs plain computation";
    const RENAMED: &str = "renamed `ui.python_execution` to `code`";
    const RETIRED: &str = "retired `agent.defer_builtin_tools`";
    const STEPS: &[Step] = &[
        Step {
            from: 1,
            why: Some(RENAME_WHY),
            apply: rename_code,
        },
        Step {
            from: 2,
            why: None,
            apply: retire_defer,
        },
    ];
    const FORMAT: ConfigFormat = ConfigFormat {
        file: FILE,
        latest: 3,
        steps: STEPS,
        requires_version: false,
    };
    const REQUIRED: ConfigFormat = ConfigFormat {
        requires_version: true,
        ..FORMAT
    };
    const GAP: ConfigFormat = ConfigFormat {
        file: FILE,
        latest: 3,
        steps: &[Step {
            from: 2,
            why: None,
            apply: retire_defer,
        }],
        requires_version: false,
    };
    const STRAY: ConfigFormat = ConfigFormat {
        latest: 2,
        ..FORMAT
    };

    const LEGACY: &str = "\
# My Caudra config

# the model I like
model = \"x\"

[ui]
# keep this short
python_execution = 40

[agent]
defer_builtin_tools = true
";
    const LEGACY_UPGRADED: &str = concat!(
        "# My Caudra config\n\n",
        added!(),
        "
# the model I like
model = \"x\"

[ui]
# Upgraded by Caudra 9.9.9 on 2026-10-20: renamed `ui.python_execution` to `code`, because the `code` tool now runs plain computation.
#
# keep this short
code = 40

[agent]
# Upgraded by Caudra 9.9.9 on 2026-10-20: retired `agent.defer_builtin_tools`.
# defer_builtin_tools = true
"
    );
    const SECOND: &str = "\
version = 2 # pinned
[ui]
python_execution = 1
[agent]
model = 'a'
defer_builtin_tools = true
";
    const SECOND_UPGRADED: &str = "\
# Upgraded by Caudra 9.9.9 on 2026-10-20: version 2 → 3.
version = 3 # pinned
[ui]
python_execution = 1
[agent]
model = 'a'
# Upgraded by Caudra 9.9.9 on 2026-10-20: retired `agent.defer_builtin_tools`.
# defer_builtin_tools = true
";
    const CRLF_BOM: &str = "\u{feff}model = 1\r\n[ui]\r\npython_execution = 2\r\n";
    const CRLF_BOM_UPGRADED: &str = "\u{feff}# Added by Caudra 9.9.9 on 2026-10-20: the format version of this file, which Caudra now requires.\r\nversion = 3\r\n\r\nmodel = 1\r\n[ui]\r\n# Upgraded by Caudra 9.9.9 on 2026-10-20: renamed `ui.python_execution` to `code`, because the `code` tool now runs plain computation.\r\ncode = 2\r\n";

    fn rename_code(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.rename_key(&["ui"], "python_execution", "code")
    }

    fn retire_defer(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.retire_key(&["agent"], "defer_builtin_tools");
        Ok(())
    }

    fn upgraded(text: &str) -> String {
        migrate(text, &FORMAT, &test_stamp()).unwrap().text.unwrap()
    }

    fn change(what: &str) -> Change {
        Change {
            what: what.to_owned(),
        }
    }

    #[test]
    fn a_legacy_file_runs_every_step_and_gains_the_version_below_its_leading_comments() {
        assert_eq!(
            migrate(LEGACY, &FORMAT, &test_stamp()),
            Ok(Migrated {
                from: 1,
                text: Some(LEGACY_UPGRADED.to_owned()),
                changes: vec![change(RENAMED), change(RETIRED)],
            })
        );
    }

    #[test]
    fn a_versioned_file_runs_only_the_steps_after_its_version() {
        assert_eq!(upgraded(SECOND), SECOND_UPGRADED);
    }

    #[test_case(LEGACY_UPGRADED ; "legacy")]
    #[test_case(SECOND_UPGRADED ; "versioned")]
    #[test_case(CRLF_BOM_UPGRADED ; "crlf_and_bom")]
    fn upgrading_again_changes_nothing(upgraded: &str) {
        assert_eq!(
            migrate(upgraded, &FORMAT, &test_stamp()),
            Ok(Migrated {
                from: 3,
                text: None,
                changes: Vec::new(),
            })
        );
    }

    #[test_case("", added!() ; "empty")]
    #[test_case("# just notes\n", concat!(added!(), "\n# just notes\n") ; "comments_without_a_block")]
    #[test_case("# head\n\n# tail", concat!("# head\n\n", added!(), "\n# tail") ; "comments_with_a_block")]
    #[test_case("# head\n\n[ui]\nx = 1\n", concat!("# head\n\n", added!(), "\n[ui]\nx = 1\n") ; "tables_only")]
    #[test_case("model = 1\n", concat!(added!(), "\nmodel = 1\n") ; "no_comments")]
    #[test_case("# head\n\nui.x = 1\n", concat!("# head\n\n", added!(), "\nui.x = 1\n") ; "dotted_first_key")]
    fn a_file_without_a_version_gains_it_first(text: &str, expected: &str) {
        assert_eq!(upgraded(text), expected);
    }

    #[test]
    fn crlf_line_endings_and_a_bom_survive() {
        assert_eq!(upgraded(CRLF_BOM), CRLF_BOM_UPGRADED);
    }

    #[test_case("version = 4\n", ConfigVersionError::Newer { found: 4, latest: 3 } ; "newer")]
    #[test_case("version = 0\n", ConfigVersionError::Invalid ; "zero")]
    #[test_case("version = \"1\"\n", ConfigVersionError::Invalid ; "string")]
    #[test_case("[version]\n", ConfigVersionError::Invalid ; "table")]
    fn an_unreadable_version_is_refused(text: &str, source: ConfigVersionError) {
        assert_eq!(
            migrate(text, &FORMAT, &test_stamp()),
            Err(MigrationError::Version { file: FILE, source })
        );
    }

    #[test]
    fn a_format_that_requires_its_version_refuses_a_file_without_one() {
        assert_eq!(
            migrate(LEGACY, &REQUIRED, &test_stamp()),
            Err(MigrationError::Version {
                file: FILE,
                source: ConfigVersionError::Missing { latest: 3 },
            })
        );
        assert_eq!(
            migrate(SECOND, &REQUIRED, &test_stamp()).unwrap().text,
            Some(SECOND_UPGRADED.to_owned())
        );
    }

    #[test]
    fn upgrade_text_hands_over_a_current_file_as_it_is() {
        assert_eq!(
            upgrade_text(SECOND_UPGRADED.to_owned(), &FORMAT),
            Ok(Loaded {
                text: SECOND_UPGRADED.to_owned(),
                from: 3,
                migrated: false,
            })
        );
    }

    #[test]
    fn upgrade_text_upgrades_a_legacy_file() {
        let loaded = upgrade_text(LEGACY.to_owned(), &FORMAT).unwrap();

        assert_eq!((loaded.from, loaded.migrated), (1, true));
        assert_eq!(
            migrate(&loaded.text, &FORMAT, &test_stamp()).unwrap().text,
            None
        );
    }

    #[test]
    fn read_upgrades_a_file_and_skips_a_missing_one() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(FILE);

        assert_eq!(read(&path, &FORMAT).unwrap(), None);
        fs::write(&path, SECOND).unwrap();
        let loaded = read(&path, &FORMAT).unwrap().unwrap();
        assert_eq!((loaded.from, loaded.migrated), (2, true));
    }

    #[test]
    fn read_names_the_path_and_the_line_of_invalid_toml() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(FILE);
        fs::write(&path, "model = \n").unwrap();

        let error = read(&path, &FORMAT).unwrap_err();

        assert!(matches!(error, ReadError::Invalid { .. }), "{error}");
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains(LINE_ONE), "{message}");
    }

    #[test]
    fn invalid_toml_is_refused() {
        let error = migrate("model = \n", &FORMAT, &test_stamp()).unwrap_err();

        assert!(
            matches!(error, MigrationError::Parse { file: FILE, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_conflict_fails_the_whole_upgrade() {
        let text = "[ui]\npython_execution = 1\ncode = 2\n";

        assert_eq!(
            migrate(text, &FORMAT, &test_stamp()),
            Err(MigrationError::Conflict {
                file: FILE,
                old: "ui.python_execution".to_owned(),
                new: "ui.code".to_owned(),
            })
        );
    }

    #[test]
    fn a_missing_step_fails_the_upgrade() {
        assert_eq!(
            migrate("", &GAP, &test_stamp()),
            Err(MigrationError::MissingStep {
                file: FILE,
                from: 1
            })
        );
    }

    #[test_case(&FORMAT, Ok(()) ; "contiguous")]
    #[test_case(&GAP, Err(MigrationError::MissingStep { file: FILE, from: 1 }) ; "gap")]
    #[test_case(&STRAY, Err(MigrationError::StrayStep { file: FILE, from: 2 }) ; "stray")]
    fn check_requires_one_step_per_version(
        format: &ConfigFormat,
        expected: Result<(), MigrationError>,
    ) {
        assert_eq!(format.check(), expected);
    }
}
