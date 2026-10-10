//! `{env:NAME}` in a config file's string values: each reference becomes the
//! value of the environment variable it names.

use std::borrow::Cow;
use std::collections::HashMap;
use std::env::VarError;
use std::path::Path;

use serde::de::DeserializeOwned;
use thiserror::Error;
use toml::Spanned;
use toml::de::{DeTable, DeValue, Deserializer, Error as TomlError};

use crate::{ENV_FILE, collect_env_vars, env_file_var_is_allowed, global_env_value};

const REFERENCE_OPEN: &str = "{env:";
const REFERENCE_CLOSE: char = '}';
/// `${env:NAME}` is PowerShell's syntax, so it stays as written.
const LITERAL_MARKER: char = '$';
const FORBIDDEN_NAME_CHARS: &[char] = &['{', '=', '\0'];

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InterpolationError {
    #[error("line {line}: `{name}` is not set, in the environment or in the global .env")]
    Unset { line: usize, name: String },
    #[error("line {line}: the value of `{name}` is not valid Unicode")]
    NotUnicode { line: usize, name: String },
    #[error(
        "line {line}: an `{{env:NAME}}` reference needs a name without spaces, `{{` or `=`, \
         and a closing `}}`"
    )]
    Malformed { line: usize },
    #[error(
        "line {line}: `{{env:{name}}}` works only in global config files, so a repository cannot \
         read your environment; move the setting to the global file"
    )]
    Refused { line: usize, name: String },
}

#[derive(Debug, Error)]
pub enum ConfigTextError {
    #[error(transparent)]
    Toml(#[from] TomlError),
    #[error(transparent)]
    Environment(#[from] InterpolationError),
}

/// Where a file's references read their values from.
#[derive(Debug, Clone, Copy)]
pub enum Lookup<'a> {
    /// The global config file at this path: the process environment, then
    /// the `.env` beside the file.
    Global(&'a Path),
    /// A file a repository wrote, which may not read the environment at all.
    Refused,
}

enum Fault {
    Unset,
    NotUnicode,
    Refused,
}

type Resolve<'r> = dyn FnMut(&str) -> Result<String, Fault> + 'r;

/// Parses `text`, expands its references, then deserializes, so a value that
/// holds quotes or newlines stays one string and errors keep their lines.
pub fn from_str<T: DeserializeOwned>(text: &str, lookup: Lookup<'_>) -> Result<T, ConfigTextError> {
    let mut root = DeTable::parse(text)?;
    expand(root.get_mut(), text, lookup)?;
    T::deserialize(Deserializer::from(root)).map_err(|mut error| {
        error.set_input(Some(text));
        error.into()
    })
}

/// `text` is the source `root` was parsed from, for the line numbers.
pub fn expand(
    root: &mut DeTable<'_>,
    text: &str,
    lookup: Lookup<'_>,
) -> Result<(), InterpolationError> {
    let mut dotenv = None;
    expand_table(root, text, &mut |name| match lookup {
        Lookup::Global(file) => global_value(name, file, &mut dotenv),
        Lookup::Refused => Err(Fault::Refused),
    })
}

/// The global `.env` is read here rather than trusted to be loaded already,
/// because some commands read config before they load it.
fn global_value(
    name: &str,
    file: &Path,
    dotenv: &mut Option<HashMap<String, String>>,
) -> Result<String, Fault> {
    match global_env_value(name) {
        Ok(Some(value)) => return Ok(value),
        Ok(None) | Err(VarError::NotPresent) => {}
        Err(VarError::NotUnicode(_)) => return Err(Fault::NotUnicode),
    }
    dotenv
        .get_or_insert_with(|| {
            let mut vars = HashMap::new();
            collect_env_vars(&file.with_file_name(ENV_FILE), &mut vars);
            vars
        })
        .get(name)
        .filter(|_| env_file_var_is_allowed(name))
        .cloned()
        .ok_or(Fault::Unset)
}

fn expand_table(
    table: &mut DeTable<'_>,
    text: &str,
    resolve: &mut Resolve<'_>,
) -> Result<(), InterpolationError> {
    table
        .iter_mut()
        .try_for_each(|(_, value)| expand_value(value, text, resolve))
}

fn expand_value(
    value: &mut Spanned<DeValue<'_>>,
    text: &str,
    resolve: &mut Resolve<'_>,
) -> Result<(), InterpolationError> {
    let start = value.span().start;
    match value.get_mut() {
        DeValue::String(string) if string.contains(REFERENCE_OPEN) => {
            if let Some(expanded) = expand_string(string, line_of(text, start), resolve)? {
                *string = Cow::Owned(expanded);
            }
            Ok(())
        }
        DeValue::Array(items) => items
            .iter_mut()
            .try_for_each(|item| expand_value(item, text, resolve)),
        DeValue::Table(table) => expand_table(table, text, resolve),
        _ => Ok(()),
    }
}

/// `None` when nothing but `${env:…}` literals matched.
fn expand_string(
    value: &str,
    line: usize,
    resolve: &mut Resolve<'_>,
) -> Result<Option<String>, InterpolationError> {
    let mut expanded = String::new();
    let mut rest = value;
    let mut changed = false;
    while let Some(at) = rest.find(REFERENCE_OPEN) {
        let (before, reference) = rest.split_at(at);
        let body = &reference[REFERENCE_OPEN.len()..];
        if before.ends_with(LITERAL_MARKER) {
            expanded.push_str(&rest[..at + REFERENCE_OPEN.len()]);
            rest = body;
            continue;
        }
        let name = body
            .split_once(REFERENCE_CLOSE)
            .map(|(name, _)| name)
            .filter(|name| is_valid_name(name))
            .ok_or(InterpolationError::Malformed { line })?;
        let value = resolve(name).map_err(|fault| fault.at(line, name))?;
        expanded.push_str(before);
        expanded.push_str(&value);
        rest = &body[name.len() + REFERENCE_CLOSE.len_utf8()..];
        changed = true;
    }
    if !changed {
        return Ok(None);
    }
    expanded.push_str(rest);
    Ok(Some(expanded))
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(|c: char| c.is_whitespace() || FORBIDDEN_NAME_CHARS.contains(&c))
}

fn line_of(text: &str, offset: usize) -> usize {
    text.bytes()
        .take(offset)
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

impl Fault {
    fn at(self, line: usize, name: &str) -> InterpolationError {
        let name = name.to_owned();
        match self {
            Self::Unset => InterpolationError::Unset { line, name },
            Self::NotUnicode => InterpolationError::NotUnicode { line, name },
            Self::Refused => InterpolationError::Refused { line, name },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde::Deserialize;
    use tempfile::TempDir;
    use test_case::test_case;
    use toml::Table;
    use toml::de::{DeTable, Deserializer};

    use super::{
        ConfigTextError, ENV_FILE, Fault, InterpolationError, Lookup, REFERENCE_OPEN, expand_table,
        from_str,
    };

    const CONFIG: &str = "caudra.toml";
    const TOKEN: &str = "TOKEN";
    const SECRET: &str = "secret";
    const UNSET: &str = "MISSING";
    const BREAKS_TOML: &str = "a\"b\n]\nc = 'd'";
    const VARIABLES: &[(&str, &str)] = &[
        (TOKEN, SECRET),
        ("HOST", "example.test"),
        ("PORT", "8080"),
        ("QUOTED", BREAKS_TOML),
    ];
    /// Unique to these tests, so neither the process nor a developer sets it.
    const DOTENV_ONLY: &str = "CAUDRA_TEST_INTERPOLATION_DOTENV_ONLY";
    const PROCESS_WINS: &str = "CAUDRA_TEST_INTERPOLATION_PROCESS_WINS";
    const FROM_DOTENV: &str = "from-dotenv";
    const FROM_PROCESS: &str = "from-process";

    fn expanded(text: &str) -> Result<Table, InterpolationError> {
        let mut root = DeTable::parse(text).unwrap();
        expand_table(root.get_mut(), text, &mut |name| {
            VARIABLES
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
                .ok_or(Fault::Unset)
        })?;
        Ok(Table::deserialize(Deserializer::from(root)).unwrap())
    }

    fn global_dir(dotenv: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(ENV_FILE), dotenv).unwrap();
        dir
    }

    #[test_case("key = '{env:TOKEN}'", SECRET; "whole_value")]
    #[test_case("key = 'Bearer {env:TOKEN}'", "Bearer secret"; "part_of_a_value")]
    #[test_case("key = 'https://{env:HOST}:{env:PORT}/'", "https://example.test:8080/"; "several_references")]
    #[test_case("key = '{env:QUOTED}'", BREAKS_TOML; "value_cannot_inject_toml")]
    #[test_case("key = '${env:TOKEN}'", "${env:TOKEN}"; "powershell_syntax_stays")]
    #[test_case("key = '${env:TOKEN} {env:TOKEN}'", "${env:TOKEN} secret"; "literal_beside_reference")]
    #[test_case("key = 'env:TOKEN {TOKEN}'", "env:TOKEN {TOKEN}"; "no_reference")]
    fn string_values_expand(source: &str, expected: &str) {
        assert_eq!(expanded(source).unwrap()["key"].as_str(), Some(expected));
    }

    #[test]
    fn references_expand_at_every_depth() {
        let table = expanded(
            "top = '{env:TOKEN}'\n[a.b]\nlist = ['{env:TOKEN}', { inner = '{env:TOKEN}' }, [ '{env:TOKEN}' ]]\n[[c]]\nd = '{env:TOKEN}'\n",
        )
        .unwrap();
        let rendered = toml::to_string(&table).unwrap();
        assert!(!rendered.contains(REFERENCE_OPEN), "{rendered}");
        assert_eq!(rendered.matches(SECRET).count(), 5, "{rendered}");
    }

    #[test]
    fn keys_stay_as_written() {
        let table = expanded("'{env:TOKEN}' = 1\n").unwrap();
        assert!(table.contains_key("{env:TOKEN}"));
    }

    #[test_case("a = 1\nb = '{env:MISSING}'", InterpolationError::Unset { line: 2, name: UNSET.into() }; "unset")]
    #[test_case("\n\nkey = '''\n{env:MISSING}'''", InterpolationError::Unset { line: 3, name: UNSET.into() }; "multiline_string_reports_its_start")]
    #[test_case("a = 1\nkey = '{env:}'", InterpolationError::Malformed { line: 2 }; "empty_name")]
    #[test_case("a = 1\nkey = '{env:TOKEN'", InterpolationError::Malformed { line: 2 }; "unclosed")]
    #[test_case("a = 1\nkey = '{env:A=B}'", InterpolationError::Malformed { line: 2 }; "equals_sign")]
    #[test_case("a = 1\nkey = '{env: TOKEN}'", InterpolationError::Malformed { line: 2 }; "whitespace")]
    #[test_case("a = 1\nkey = '{env:{env:TOKEN}}'", InterpolationError::Malformed { line: 2 }; "nested")]
    #[test_case("a = 1\nkey = ['{env:TOKEN}', '{env:MISSING}']", InterpolationError::Unset { line: 2, name: UNSET.into() }; "inside_an_array")]
    fn bad_references_name_their_line(source: &str, expected: InterpolationError) {
        assert_eq!(expanded(source).unwrap_err(), expected);
    }

    #[test]
    fn refused_files_name_the_first_reference() {
        let error = from_str::<Table>("a = 1\nb = 'x {env:TOKEN}'\n", Lookup::Refused).unwrap_err();
        assert!(matches!(
            error,
            ConfigTextError::Environment(InterpolationError::Refused { line: 2, ref name }) if name == TOKEN
        ));
    }

    #[test]
    fn refused_files_without_references_parse() {
        let table = from_str::<Table>("a = '${env:TOKEN}'\n", Lookup::Refused).unwrap();
        assert_eq!(table["a"].as_str(), Some("${env:TOKEN}"));
    }

    #[test]
    fn global_files_read_the_dotenv_beside_them() {
        let dir = global_dir(&format!("{DOTENV_ONLY}={FROM_DOTENV}\n"));
        let source = format!("a = 'x-{{env:{DOTENV_ONLY}}}'\n");
        let table: Table = from_str(&source, Lookup::Global(&dir.path().join(CONFIG))).unwrap();
        assert_eq!(
            table["a"].as_str(),
            Some(format!("x-{FROM_DOTENV}").as_str())
        );
    }

    #[test]
    fn the_process_environment_beats_the_dotenv() {
        let dir = global_dir(&format!("{PROCESS_WINS}={FROM_DOTENV}\n"));
        unsafe { std::env::set_var(PROCESS_WINS, FROM_PROCESS) };
        let source = format!("a = '{{env:{PROCESS_WINS}}}'\n");
        let table: Result<Table, _> = from_str(&source, Lookup::Global(&dir.path().join(CONFIG)));
        unsafe { std::env::remove_var(PROCESS_WINS) };
        assert_eq!(table.unwrap()["a"].as_str(), Some(FROM_PROCESS));
    }

    #[test]
    fn deserialize_errors_keep_their_line_after_expansion() {
        #[derive(Deserialize)]
        struct Strict {
            #[serde(rename = "a")]
            _a: String,
            #[serde(rename = "b")]
            _b: u32,
        }
        let dir = global_dir(&format!("{DOTENV_ONLY}={FROM_DOTENV}\n"));
        let source = format!("a = '''\n{{env:{DOTENV_ONLY}}}'''\nb = 'not a number'\n");
        let Err(error) = from_str::<Strict>(&source, Lookup::Global(&dir.path().join(CONFIG)))
        else {
            panic!("a string is not a number");
        };
        assert!(matches!(error, ConfigTextError::Toml(_)));
        assert!(error.to_string().contains("line 3"), "{error}");
    }
}
