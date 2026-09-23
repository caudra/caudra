use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use toml::de::Error as TomlError;
use toml::{Table, Value};

pub const CONFIG_VERSION_KEY: &str = "version";
/// Versions start here, and a file without the key predates versioning.
const FIRST_VERSION: u32 = 1;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ConfigVersionError {
    #[error("`{key}` must be a positive integer", key = CONFIG_VERSION_KEY)]
    Invalid,
    #[error("version {found} needs a newer Caudra: this build reads up to version {latest}")]
    Newer { found: i64, latest: u32 },
}

/// Format version of a config file whose newest shape is `LATEST`.
///
/// A missing key means [`FIRST_VERSION`] rather than `LATEST`, so raising
/// `LATEST` never changes what an unversioned file means. Raise a file's
/// constant only when an older build would misread the new shape: new meaning
/// for existing syntax, or a restrictive construct a lenient parser would
/// skip. Optional fields an older build can ignore need no bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigVersion<const LATEST: u32>(u32);

impl<const LATEST: u32> ConfigVersion<LATEST> {
    /// For parsers that read every other top-level key as a name.
    pub fn take(table: &mut Table) -> Result<Self, ConfigVersionError> {
        table
            .remove(CONFIG_VERSION_KEY)
            .as_ref()
            .map_or(Ok(Self::default()), Self::from_value)
    }

    /// For lenient parsers: checks the key alone, and errors keep the TOML
    /// line and column.
    pub fn check_document(document: &str) -> Result<Self, TomlError> {
        toml::from_str::<VersionProbe<LATEST>>(document).map(|probe| probe.version)
    }

    fn from_value(value: &Value) -> Result<Self, ConfigVersionError> {
        let found = value.as_integer().ok_or(ConfigVersionError::Invalid)?;
        if found < i64::from(FIRST_VERSION) {
            return Err(ConfigVersionError::Invalid);
        }
        u32::try_from(found)
            .ok()
            .filter(|version| *version <= LATEST)
            .map(Self)
            .ok_or(ConfigVersionError::Newer {
                found,
                latest: LATEST,
            })
    }
}

impl<const LATEST: u32> Default for ConfigVersion<LATEST> {
    fn default() -> Self {
        Self(FIRST_VERSION)
    }
}

impl<'de, const LATEST: u32> Deserialize<'de> for ConfigVersion<LATEST> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_value(&Value::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Writers emit the current shape, so this stamps `LATEST` whichever version
/// was read.
impl<const LATEST: u32> Serialize for ConfigVersion<LATEST> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(LATEST)
    }
}

#[derive(Deserialize)]
struct VersionProbe<const LATEST: u32> {
    #[serde(default)]
    version: ConfigVersion<LATEST>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const LATEST: u32 = 2;
    const OTHER_KEY: &str = "other";

    type Version = ConfigVersion<LATEST>;

    fn table(document: &str) -> Table {
        toml::from_str(document).unwrap()
    }

    fn assert_rejected(value: &str, expected: ConfigVersionError) {
        let document = format!("{CONFIG_VERSION_KEY} = {value}\n");
        assert_eq!(Version::take(&mut table(&document)), Err(expected.clone()));
        let error = Version::check_document(&document).unwrap_err();
        assert!(error.to_string().contains(&expected.to_string()), "{error}");
    }

    #[test_case("", FIRST_VERSION ; "missing_key_is_the_first_version_not_the_latest")]
    #[test_case("version = 1", FIRST_VERSION ; "first_version")]
    #[test_case("version = 2", LATEST ; "latest_version")]
    fn take_accepts_known_versions_and_removes_the_key(version: &str, expected: u32) {
        let mut table = table(&format!("{version}\n{OTHER_KEY} = true\n"));

        assert_eq!(Version::take(&mut table), Ok(ConfigVersion(expected)));
        assert_eq!(table.keys().collect::<Vec<_>>(), [OTHER_KEY]);
    }

    #[test_case(i64::from(LATEST) + 1 ; "next_version")]
    #[test_case(i64::from(u32::MAX) + 1 ; "beyond_u32")]
    fn newer_versions_ask_for_an_upgrade(found: i64) {
        assert_rejected(
            &found.to_string(),
            ConfigVersionError::Newer {
                found,
                latest: LATEST,
            },
        );
    }

    #[test_case("0" ; "zero")]
    #[test_case("-1" ; "negative")]
    #[test_case("\"1\"" ; "string")]
    #[test_case("1.0" ; "float")]
    #[test_case("{ major = 1 }" ; "table")]
    fn anything_but_a_positive_integer_is_invalid(value: &str) {
        assert_rejected(value, ConfigVersionError::Invalid);
    }

    #[test]
    fn check_document_ignores_every_other_key() {
        let document = "defer_tools = 3\n[mcp.server]\ncommand = ['x']\n";

        assert_eq!(
            Version::check_document(document).unwrap(),
            ConfigVersion(FIRST_VERSION)
        );
    }

    #[test]
    fn serializing_stamps_the_latest_version() {
        let written = Value::try_from(Version::default()).unwrap();

        assert_eq!(written.as_integer(), Some(i64::from(LATEST)));
    }
}
