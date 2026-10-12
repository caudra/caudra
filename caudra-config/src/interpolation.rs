//! References in a config file's string values: `{env:NAME}` becomes the
//! value of an environment variable, and `{file:PATH}` the contents of a file.

use std::borrow::Cow;
use std::collections::HashMap;
use std::env::VarError;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf, is_separator};

use caudra_storage::paths;
use serde::de::DeserializeOwned;
use thiserror::Error;
use toml::Spanned;
use toml::de::{DeTable, DeValue, Deserializer, Error as TomlError};

use crate::{ENV_FILE, collect_env_vars, env_file_var_is_allowed, inherited_env_value};

pub const ENV_OPEN: &str = "{env:";
pub const FILE_OPEN: &str = "{file:";
const REFERENCE_START: char = '{';
const REFERENCE_CLOSE: char = '}';
/// `${env:NAME}` is PowerShell's syntax, so a reference after `$` stays as
/// written.
const LITERAL_MARKER: char = '$';
const NUL: char = '\0';
const FORBIDDEN_NAME_CHARS: &[char] = &[REFERENCE_START, '=', NUL];
const HOME: &str = "~";
const MAX_REFERENCED_FILE_BYTES: u64 = 1 << 20;

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
    MalformedEnv { line: usize },
    #[error(
        "line {line}: a `{{file:PATH}}` reference needs a path that neither starts nor ends \
         with a space, holds no `{{` outside an `{{env:NAME}}`, and has a closing `}}`"
    )]
    MalformedFile { line: usize },
    #[error(
        "line {line}: `{reference}` works only in global config files, so a repository cannot \
         read your environment or your files; move the setting to the global file"
    )]
    Refused { line: usize, reference: String },
    #[error("line {line}: `~` stands for your home directory, which Caudra cannot find")]
    NoHome { line: usize },
    #[error("line {line}: {} {problem}", path.display())]
    File {
        line: usize,
        path: PathBuf,
        problem: FileProblem,
    },
}

/// Why the file a `{file:PATH}` reference names gave no value.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FileProblem {
    #[error("does not exist")]
    Missing,
    #[error("is not a regular file")]
    NotAFile,
    #[error("is larger than {MAX_REFERENCED_FILE_BYTES} bytes")]
    TooLarge,
    #[error("is not valid UTF-8")]
    NotUnicode,
    #[error("cannot be read: {0}")]
    Unreadable(ErrorKind),
}

#[derive(Debug, Error)]
pub enum ConfigTextError {
    #[error(transparent)]
    Toml(#[from] TomlError),
    #[error(transparent)]
    Reference(#[from] InterpolationError),
}

/// Where a file's references read their values from.
#[derive(Debug, Clone, Copy)]
pub enum Lookup<'a> {
    /// The global config file at this path: variables come from the
    /// environment the process inherited, then the `.env` beside the file as
    /// it is now, and a relative file path starts at the file's directory.
    Global(&'a Path),
    /// A file a repository wrote, which may read neither your environment
    /// nor your files.
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Env,
    File,
}

struct Reference<'a> {
    kind: Kind,
    /// The name or path between the opener and the closing brace.
    inner: &'a str,
    /// The whole reference as written.
    written: &'a str,
}

enum Fault {
    Malformed(Kind),
    Unset(String),
    NotUnicode(String),
    Refused(String),
    NoHome,
    File(PathBuf, FileProblem),
}

type Resolve<'r> = dyn FnMut(Reference<'_>) -> Result<String, Fault> + 'r;
type ResolveEnv<'r> = dyn FnMut(&str) -> Result<String, Fault> + 'r;

/// Whether `value` holds a reference, and so names where its value comes from.
pub fn holds_reference(value: &str) -> bool {
    Kind::ALL.iter().any(|kind| value.contains(kind.open()))
}

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
    match lookup {
        Lookup::Global(file) => {
            let mut global = Global { file, dotenv: None };
            expand_table(root, text, &mut |reference| global.resolve(reference))
        }
        Lookup::Refused => expand_table(root, text, &mut |reference| {
            Err(Fault::Refused(reference.written.to_owned()))
        }),
    }
}

/// What the references of the global config file `file` read.
struct Global<'a> {
    file: &'a Path,
    dotenv: Option<HashMap<String, String>>,
}

impl Global<'_> {
    fn resolve(&mut self, reference: Reference<'_>) -> Result<String, Fault> {
        match reference.kind {
            Kind::Env => self.env(reference.inner),
            Kind::File => {
                let file = self.file;
                let home = paths::home();
                let path = file_path(
                    reference.inner,
                    file.parent().unwrap_or(file),
                    home.as_deref(),
                    &mut |name| self.env(name),
                )?;
                read_referenced_file(&path).map_err(|problem| Fault::File(path, problem))
            }
        }
    }

    /// The `.env` is read here rather than trusted to be loaded already,
    /// because some commands read config before they load it, and a reparse
    /// must see the file's edits.
    fn env(&mut self, name: &str) -> Result<String, Fault> {
        match inherited_env_value(name) {
            Ok(Some(value)) => return Ok(value),
            Ok(None) | Err(VarError::NotPresent) => {}
            Err(VarError::NotUnicode(_)) => return Err(Fault::NotUnicode(name.to_owned())),
        }
        let file = self.file;
        self.dotenv
            .get_or_insert_with(|| {
                let mut vars = HashMap::new();
                collect_env_vars(&file.with_file_name(ENV_FILE), &mut vars);
                vars
            })
            .get(name)
            .filter(|_| env_file_var_is_allowed(name))
            .cloned()
            .ok_or_else(|| Fault::Unset(name.to_owned()))
    }
}

/// Where `{file:PATH}` points. `~` stands for the home directory only where
/// it is written, a relative path starts at `base`, and `.` and `..` come off
/// the text without following symlinks, as on Windows.
fn file_path(
    written: &str,
    base: &Path,
    home: Option<&Path>,
    env: &mut ResolveEnv<'_>,
) -> Result<PathBuf, Fault> {
    let (base, relative) = match home_relative(written) {
        Some(relative) => (home.ok_or(Fault::NoHome)?, relative),
        None => (base, written),
    };
    let expanded = substitute(
        relative,
        &[Kind::Env],
        &mut |reference| env(reference.inner),
    )?;
    Ok(paths::normalize_path(
        &base.join(expanded.as_deref().unwrap_or(relative)),
    ))
}

fn home_relative(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(HOME)?;
    (rest.is_empty() || rest.starts_with(is_separator))
        .then(|| rest.trim_start_matches(is_separator))
}

/// Its contents, trimmed. Only a regular file is opened, so a FIFO cannot
/// hold up loading config.
fn read_referenced_file(path: &Path) -> Result<String, FileProblem> {
    if !fs::metadata(path)?.is_file() {
        return Err(FileProblem::NotAFile);
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_REFERENCED_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REFERENCED_FILE_BYTES {
        return Err(FileProblem::TooLarge);
    }
    let text = String::from_utf8(bytes).map_err(|_| FileProblem::NotUnicode)?;
    Ok(text.trim().to_owned())
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
        DeValue::String(string) if holds_reference(string) => {
            let expanded = substitute(string, Kind::ALL, resolve)
                .map_err(|fault| fault.at(line_of(text, start)))?;
            if let Some(expanded) = expanded {
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

/// `value` with each reference of `kinds` replaced, or `None` when it has
/// none to replace. A replacement is never scanned again.
fn substitute(
    value: &str,
    kinds: &[Kind],
    resolve: &mut Resolve<'_>,
) -> Result<Option<String>, Fault> {
    let mut expanded = String::new();
    let mut rest = value;
    let mut changed = false;
    while let Some((at, kind)) = next_reference(rest, kinds) {
        let (before, reference) = rest.split_at(at);
        let open = kind.open().len();
        let inner = kind.inner_len(&reference[open..]);
        let written_len = |inner: usize| open + inner + REFERENCE_CLOSE.len_utf8();
        if before.ends_with(LITERAL_MARKER) {
            let literal = at + inner.map_or(open, written_len);
            expanded.push_str(&rest[..literal]);
            rest = &rest[literal..];
            continue;
        }
        let inner = inner.ok_or(Fault::Malformed(kind))?;
        let (written, after) = reference.split_at(written_len(inner));
        expanded.push_str(before);
        expanded.push_str(&resolve(Reference {
            kind,
            inner: &written[open..open + inner],
            written,
        })?);
        rest = after;
        changed = true;
    }
    if !changed {
        return Ok(None);
    }
    expanded.push_str(rest);
    Ok(Some(expanded))
}

fn next_reference(text: &str, kinds: &[Kind]) -> Option<(usize, Kind)> {
    text.match_indices(REFERENCE_START).find_map(|(at, _)| {
        kinds
            .iter()
            .copied()
            .find(|kind| text[at..].starts_with(kind.open()))
            .map(|kind| (at, kind))
    })
}

fn env_name_len(body: &str) -> Option<usize> {
    let (name, _) = body.split_once(REFERENCE_CLOSE)?;
    let valid = !name.is_empty()
        && !name.contains(|c: char| c.is_whitespace() || FORBIDDEN_NAME_CHARS.contains(&c));
    valid.then_some(name.len())
}

/// A path ends at the first `}` outside the `{env:NAME}` references it holds.
fn file_path_len(body: &str) -> Option<usize> {
    let mut end = 0;
    loop {
        let at = end + body[end..].find([REFERENCE_START, REFERENCE_CLOSE])?;
        let rest = &body[at..];
        if rest.starts_with(REFERENCE_CLOSE) {
            end = at;
            break;
        }
        let name = env_name_len(rest.strip_prefix(ENV_OPEN)?)?;
        end = at + ENV_OPEN.len() + name + REFERENCE_CLOSE.len_utf8();
    }
    let path = &body[..end];
    (!path.is_empty() && path.trim() == path && !path.contains(NUL)).then_some(end)
}

fn line_of(text: &str, offset: usize) -> usize {
    text.bytes()
        .take(offset)
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

impl Kind {
    const ALL: &[Self] = &[Self::Env, Self::File];

    fn open(self) -> &'static str {
        match self {
            Self::Env => ENV_OPEN,
            Self::File => FILE_OPEN,
        }
    }

    /// The length of the name or path `body` starts with, the text after the
    /// opener; `None` when the reference is malformed.
    fn inner_len(self, body: &str) -> Option<usize> {
        match self {
            Self::Env => env_name_len(body),
            Self::File => file_path_len(body),
        }
    }
}

impl Fault {
    fn at(self, line: usize) -> InterpolationError {
        match self {
            Self::Malformed(Kind::Env) => InterpolationError::MalformedEnv { line },
            Self::Malformed(Kind::File) => InterpolationError::MalformedFile { line },
            Self::Unset(name) => InterpolationError::Unset { line, name },
            Self::NotUnicode(name) => InterpolationError::NotUnicode { line, name },
            Self::Refused(reference) => InterpolationError::Refused { line, reference },
            Self::NoHome => InterpolationError::NoHome { line },
            Self::File(path, problem) => InterpolationError::File {
                line,
                path,
                problem,
            },
        }
    }
}

impl From<io::Error> for FileProblem {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            ErrorKind::NotFound => Self::Missing,
            kind => Self::Unreadable(kind),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    use caudra_storage::paths::normalize_path;
    #[cfg(all(unix, not(target_vendor = "apple")))]
    use rustix::fs::{CWD, Mode, mkfifoat};
    use serde::Deserialize;
    use tempfile::TempDir;
    use test_case::test_case;
    use toml::Table;
    use toml::de::{DeTable, Deserializer};

    use super::{
        ConfigTextError, ENV_FILE, ENV_OPEN, FILE_OPEN, Fault, FileProblem, InterpolationError,
        Kind, Lookup, MAX_REFERENCED_FILE_BYTES, Reference, expand_table, file_path, from_str,
    };
    use crate::{PROJECT_DIR, load_env_files_scoped};

    const CONFIG: &str = "caudra.toml";
    const TOKEN: &str = "TOKEN";
    const SECRET: &str = "secret";
    const FILE_SECRET: &str = "file-secret";
    const UNSET: &str = "MISSING";
    const MISSING_FILE: &str = "missing";
    const BREAKS_TOML: &str = "a\"b\n]\nc = 'd'";
    const VARIABLES: &[(&str, &str)] = &[
        (TOKEN, SECRET),
        ("HOST", "example.test"),
        ("PORT", "8080"),
        ("QUOTED", BREAKS_TOML),
    ];
    const FILES: &[(&str, &str)] = &[
        ("key", FILE_SECRET),
        ("toml", BREAKS_TOML),
        ("reference", "{env:TOKEN}"),
        ("Application Support/key", FILE_SECRET),
        ("{env:HOST}/key", FILE_SECRET),
    ];
    /// Unique to these tests, so neither the process nor a developer sets it.
    const DOTENV_ONLY: &str = "CAUDRA_TEST_INTERPOLATION_DOTENV_ONLY";
    const PROCESS_WINS: &str = "CAUDRA_TEST_INTERPOLATION_PROCESS_WINS";
    const SECRET_DIR: &str = "CAUDRA_TEST_INTERPOLATION_SECRET_DIR";
    const FROM_DOTENV: &str = "from-dotenv";
    const FROM_PROCESS: &str = "from-process";
    const FROM_EDIT: &str = "from-edit";
    const FROM_PROJECT: &str = "from-project";
    const EDITED: &str = "CAUDRA_TEST_INTERPOLATION_EDITED";
    const REMOVED: &str = "CAUDRA_TEST_INTERPOLATION_REMOVED";
    const INHERITED: &str = "CAUDRA_TEST_INTERPOLATION_INHERITED";
    const PROJECT_ONLY: &str = "CAUDRA_TEST_INTERPOLATION_PROJECT_ONLY";
    const PROJECT_SHADOWS: &str = "CAUDRA_TEST_INTERPOLATION_PROJECT_SHADOWS";
    const KEY: &str = "a";
    const KEY_FILE: &str = "key";
    const SECRETS: &str = "secrets";
    const LINE: usize = 1;
    const PATH_VARIABLES: &[(&str, &str)] =
        &[("DIR", CREDENTIALS), ("NAME", KEY_FILE), ("TILDE", "~")];
    #[cfg(unix)]
    const BASE: &str = "/etc/caudra";
    #[cfg(unix)]
    const HOME_DIR: &str = "/home/me";
    #[cfg(unix)]
    const CREDENTIALS: &str = "/run/credentials";
    #[cfg(windows)]
    const BASE: &str = r"C:\caudra";
    #[cfg(windows)]
    const HOME_DIR: &str = r"C:\Users\me";
    #[cfg(windows)]
    const CREDENTIALS: &str = r"D:\credentials";

    fn lookup(table: &[(&str, &str)], name: &str) -> Option<String> {
        table
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    }

    fn expanded(text: &str) -> Result<Table, InterpolationError> {
        let mut root = DeTable::parse(text).unwrap();
        expand_table(
            root.get_mut(),
            text,
            &mut |reference: Reference<'_>| match reference.kind {
                Kind::Env => lookup(VARIABLES, reference.inner)
                    .ok_or_else(|| Fault::Unset(reference.inner.to_owned())),
                Kind::File => lookup(FILES, reference.inner).ok_or_else(|| {
                    Fault::File(PathBuf::from(reference.inner), FileProblem::Missing)
                }),
            },
        )?;
        Ok(Table::deserialize(Deserializer::from(root)).unwrap())
    }

    fn resolved_path(written: &str, home: Option<&Path>) -> Result<PathBuf, InterpolationError> {
        file_path(written, Path::new(BASE), home, &mut |name| {
            lookup(PATH_VARIABLES, name).ok_or_else(|| Fault::Unset(name.to_owned()))
        })
        .map_err(|fault| fault.at(LINE))
    }

    fn global_dir(dotenv: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(ENV_FILE), dotenv).unwrap();
        dir
    }

    fn global(dir: &TempDir, source: &str) -> Result<Table, ConfigTextError> {
        from_str(source, Lookup::Global(&dir.path().join(CONFIG)))
    }

    fn dotenv(name: &str, value: Option<&str>) -> String {
        value.map_or_else(String::new, |value| format!("{name}={value}\n"))
    }

    /// At startup `name` is `process` in the environment, `global` in the
    /// global `.env` and `project` in the project's; then the global `.env`
    /// changes to `edited` and the global file is parsed again.
    #[test_case(EDITED, None, Some(FROM_DOTENV), None, Some(FROM_EDIT), Some(FROM_EDIT); "edited_global_dotenv_is_reread")]
    #[test_case(REMOVED, None, Some(FROM_DOTENV), None, None, None; "removed_global_key_is_unset")]
    #[test_case(INHERITED, Some(FROM_PROCESS), Some(FROM_DOTENV), None, Some(FROM_EDIT), Some(FROM_PROCESS); "inherited_value_beats_the_dotenv")]
    #[test_case(PROJECT_ONLY, None, None, Some(FROM_PROJECT), None, None; "project_dotenv_never_feeds_a_global_file")]
    #[test_case(PROJECT_SHADOWS, None, Some(FROM_DOTENV), Some(FROM_PROJECT), Some(FROM_EDIT), Some(FROM_EDIT); "shadowed_global_key_is_reread")]
    fn reparse_reads_the_current_global_dotenv(
        name: &str,
        process: Option<&str>,
        global: Option<&str>,
        project: Option<&str>,
        edited: Option<&str>,
        expected: Option<&str>,
    ) {
        let dir = global_dir(&dotenv(name, global));
        let project_dir = dir.path().join(PROJECT_DIR);
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join(ENV_FILE), dotenv(name, project)).unwrap();
        if let Some(value) = process {
            unsafe { std::env::set_var(name, value) };
        }
        load_env_files_scoped(dir.path(), Some(dir.path()), true);
        fs::write(dir.path().join(ENV_FILE), dotenv(name, edited)).unwrap();

        let source = format!("{KEY} = '{{env:{name}}}'\n");
        let parsed = from_str::<Table>(&source, Lookup::Global(&dir.path().join(CONFIG)));
        unsafe { std::env::remove_var(name) };
        let resolved =
            match parsed {
                Ok(table) => table[KEY].as_str().map(str::to_owned),
                Err(ConfigTextError::Reference(InterpolationError::Unset {
                    name: unset, ..
                })) if unset == name => None,
                Err(error) => panic!("{error}"),
            };
        assert_eq!(resolved.as_deref(), expected);
    }

    #[test_case("key = '{env:TOKEN}'", SECRET; "whole_value")]
    #[test_case("key = 'Bearer {env:TOKEN}'", "Bearer secret"; "part_of_a_value")]
    #[test_case("key = 'https://{env:HOST}:{env:PORT}/'", "https://example.test:8080/"; "several_references")]
    #[test_case("key = '{env:QUOTED}'", BREAKS_TOML; "value_cannot_inject_toml")]
    #[test_case("key = '${env:TOKEN}'", "${env:TOKEN}"; "powershell_syntax_stays")]
    #[test_case("key = '${env:TOKEN} {env:TOKEN}'", "${env:TOKEN} secret"; "literal_beside_reference")]
    #[test_case("key = 'env:TOKEN {TOKEN} file:key {key}'", "env:TOKEN {TOKEN} file:key {key}"; "no_reference")]
    #[test_case("key = '{file:key}'", FILE_SECRET; "whole_file")]
    #[test_case("key = 'Bearer {file:key} {env:TOKEN}'", "Bearer file-secret secret"; "file_beside_variable")]
    #[test_case("key = '{file:toml}'", BREAKS_TOML; "file_cannot_inject_toml")]
    #[test_case("key = '{file:reference}'", "{env:TOKEN}"; "file_contents_are_not_scanned_again")]
    #[test_case("key = '{file:Application Support/key}'", FILE_SECRET; "spaces_inside_a_path")]
    #[test_case("key = '{file:{env:HOST}/key} tail'", "file-secret tail"; "variable_inside_a_path")]
    #[test_case("key = '${file:key}'", "${file:key}"; "dollar_keeps_a_file_reference")]
    #[test_case("key = '${file:{env:TOKEN}} {file:key}'", "${file:{env:TOKEN}} file-secret"; "dollar_keeps_the_whole_file_reference")]
    fn string_values_expand(source: &str, expected: &str) {
        assert_eq!(expanded(source).unwrap()["key"].as_str(), Some(expected));
    }

    #[test]
    fn references_expand_at_every_depth() {
        let table = expanded(
            "top = '{env:TOKEN}'\n[a.b]\nlist = ['{env:TOKEN}', { inner = '{file:key}' }, [ '{env:TOKEN}' ]]\n[[c]]\nd = '{file:key}'\n",
        )
        .unwrap();
        let rendered = toml::to_string(&table).unwrap();
        assert!(!rendered.contains(ENV_OPEN), "{rendered}");
        assert!(!rendered.contains(FILE_OPEN), "{rendered}");
        assert_eq!(rendered.matches(SECRET).count(), 5, "{rendered}");
    }

    #[test]
    fn keys_stay_as_written() {
        let table = expanded("'{env:TOKEN}' = 1\n'{file:key}' = 2\n").unwrap();
        assert!(table.contains_key("{env:TOKEN}"));
        assert!(table.contains_key("{file:key}"));
    }

    #[test_case("a = 1\nb = '{env:MISSING}'", InterpolationError::Unset { line: 2, name: UNSET.into() }; "unset")]
    #[test_case("\n\nkey = '''\n{env:MISSING}'''", InterpolationError::Unset { line: 3, name: UNSET.into() }; "multiline_string_reports_its_start")]
    #[test_case("a = 1\nkey = '{env:}'", InterpolationError::MalformedEnv { line: 2 }; "empty_name")]
    #[test_case("a = 1\nkey = '{env:TOKEN'", InterpolationError::MalformedEnv { line: 2 }; "unclosed")]
    #[test_case("a = 1\nkey = '{env:A=B}'", InterpolationError::MalformedEnv { line: 2 }; "equals_sign")]
    #[test_case("a = 1\nkey = '{env: TOKEN}'", InterpolationError::MalformedEnv { line: 2 }; "whitespace")]
    #[test_case("a = 1\nkey = '{env:{env:TOKEN}}'", InterpolationError::MalformedEnv { line: 2 }; "nested")]
    #[test_case("a = 1\nkey = ['{env:TOKEN}', '{env:MISSING}']", InterpolationError::Unset { line: 2, name: UNSET.into() }; "inside_an_array")]
    #[test_case("a = 1\nkey = '{file:}'", InterpolationError::MalformedFile { line: 2 }; "empty_path")]
    #[test_case("a = 1\nkey = '{file:key'", InterpolationError::MalformedFile { line: 2 }; "unclosed_path")]
    #[test_case("a = 1\nkey = '{file: key}'", InterpolationError::MalformedFile { line: 2 }; "leading_space")]
    #[test_case("a = 1\nkey = '{file:key }'", InterpolationError::MalformedFile { line: 2 }; "trailing_space")]
    #[test_case("a = 1\nkey = '{file:a{b}'", InterpolationError::MalformedFile { line: 2 }; "stray_brace")]
    #[test_case("a = 1\nkey = '{file:{file:key}}'", InterpolationError::MalformedFile { line: 2 }; "nested_file")]
    #[test_case("a = 1\nkey = '{file:{env:}/key}'", InterpolationError::MalformedFile { line: 2 }; "malformed_variable_in_a_path")]
    #[test_case("a = 1\nkey = \"{file:a\\u0000b}\"", InterpolationError::MalformedFile { line: 2 }; "nul_in_a_path")]
    #[test_case("a = 1\nkey = '{file:missing}'", InterpolationError::File { line: 2, path: MISSING_FILE.into(), problem: FileProblem::Missing }; "missing_file")]
    fn bad_references_name_their_line(source: &str, expected: InterpolationError) {
        assert_eq!(expanded(source).unwrap_err(), expected);
    }

    #[test_case("a = 1\nb = 'x {env:TOKEN}'\n", "{env:TOKEN}"; "variable")]
    #[test_case("a = 1\nb = 'x {file:~/.ssh/id_ed25519}'\n", "{file:~/.ssh/id_ed25519}"; "file")]
    #[test_case("a = 1\nb = '{file:{env:HOME}/.netrc}'\n", "{file:{env:HOME}/.netrc}"; "variable_inside_a_file")]
    fn refused_files_name_the_first_reference(source: &str, reference: &str) {
        let error = from_str::<Table>(source, Lookup::Refused).unwrap_err();
        assert!(
            matches!(
                &error,
                ConfigTextError::Reference(InterpolationError::Refused { line: 2, reference: refused })
                    if refused == reference
            ),
            "{error}"
        );
    }

    #[test]
    fn refused_files_without_references_parse() {
        let table = from_str::<Table>("a = '${env:TOKEN} ${file:key}'\n", Lookup::Refused).unwrap();
        assert_eq!(table["a"].as_str(), Some("${env:TOKEN} ${file:key}"));
    }

    #[test]
    fn global_files_read_the_dotenv_beside_them() {
        let dir = global_dir(&format!("{DOTENV_ONLY}={FROM_DOTENV}\n"));
        let table = global(&dir, &format!("a = 'x-{{env:{DOTENV_ONLY}}}'\n")).unwrap();
        assert_eq!(
            table["a"].as_str(),
            Some(format!("x-{FROM_DOTENV}").as_str())
        );
    }

    #[test]
    fn the_process_environment_beats_the_dotenv() {
        let dir = global_dir(&format!("{PROCESS_WINS}={FROM_DOTENV}\n"));
        unsafe { std::env::set_var(PROCESS_WINS, FROM_PROCESS) };
        let table = global(&dir, &format!("a = '{{env:{PROCESS_WINS}}}'\n"));
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

    #[cfg(unix)]
    #[test_case("~/key", "/home/me/key"; "home")]
    #[test_case("~//key", "/home/me/key"; "home_with_repeated_separators")]
    #[test_case("~", "/home/me"; "home_alone")]
    #[test_case("~me/key", "/etc/caudra/~me/key"; "tilde_user_is_a_name")]
    #[test_case("key", "/etc/caudra/key"; "relative_to_the_config")]
    #[test_case("./secrets/./key", "/etc/caudra/secrets/key"; "current_directory")]
    #[test_case("secrets/../key", "/etc/caudra/key"; "parent_directory")]
    #[test_case("../../../../key", "/key"; "never_above_the_root")]
    #[test_case("~/../shared/key", "/home/shared/key"; "parent_of_home")]
    #[test_case("/run/key", "/run/key"; "absolute")]
    #[test_case("{env:DIR}/key", "/run/credentials/key"; "variable_in_the_path")]
    #[test_case("~/{env:NAME}", "/home/me/key"; "variable_after_home")]
    #[test_case("{env:TILDE}/key", "/etc/caudra/~/key"; "tilde_from_a_variable_is_a_name")]
    #[test_case(r"a\b", r"/etc/caudra/a\b"; "backslash_is_part_of_a_name")]
    fn unix_file_paths_resolve(written: &str, expected: &str) {
        assert_eq!(
            resolved_path(written, Some(Path::new(HOME_DIR))).unwrap(),
            PathBuf::from(expected)
        );
    }

    #[cfg(windows)]
    #[test_case(r"~\key", r"C:\Users\me\key"; "home_with_a_backslash")]
    #[test_case("~/key", r"C:\Users\me\key"; "home_with_a_slash")]
    #[test_case(r"secrets\..\key", r"C:\caudra\key"; "parent_directory")]
    #[test_case(r"..\..\..\key", r"C:\key"; "never_above_the_drive")]
    #[test_case(r"D:\keys\key", r"D:\keys\key"; "drive")]
    #[test_case("D:/keys/../key", r"D:\key"; "drive_with_slashes")]
    #[test_case(r"\keys\key", r"C:\keys\key"; "root_keeps_the_config_drive")]
    #[test_case(r"\\server\share\key", r"\\server\share\key"; "unc")]
    #[test_case(r"{env:DIR}\key", r"D:\credentials\key"; "variable_in_the_path")]
    fn windows_file_paths_resolve(written: &str, expected: &str) {
        assert_eq!(
            resolved_path(written, Some(Path::new(HOME_DIR))).unwrap(),
            PathBuf::from(expected)
        );
    }

    #[test_case("~/key", None, InterpolationError::NoHome { line: LINE }; "no_home")]
    #[test_case("{env:MISSING}/key", Some(HOME_DIR), InterpolationError::Unset { line: LINE, name: UNSET.into() }; "unset_variable")]
    fn unresolvable_paths_name_the_cause(
        written: &str,
        home: Option<&str>,
        expected: InterpolationError,
    ) {
        assert_eq!(
            resolved_path(written, home.map(Path::new)).unwrap_err(),
            expected
        );
    }

    #[test]
    fn files_are_read_beside_the_config_and_trimmed() {
        let dir = global_dir(&format!("{SECRET_DIR}={SECRETS}\n"));
        fs::create_dir(dir.path().join(SECRETS)).unwrap();
        fs::write(
            dir.path().join(SECRETS).join(KEY_FILE),
            format!("  {FILE_SECRET} \n\n"),
        )
        .unwrap();
        let table = global(
            &dir,
            &format!("a = 'Bearer {{file:{SECRETS}/{KEY_FILE}}}'\nb = '{{file:{{env:{SECRET_DIR}}}/{KEY_FILE}}}'\n"),
        )
        .unwrap();
        assert_eq!(
            table["a"].as_str(),
            Some(format!("Bearer {FILE_SECRET}").as_str())
        );
        assert_eq!(table["b"].as_str(), Some(FILE_SECRET));
    }

    #[test_case(|_| {}, FileProblem::Missing; "missing")]
    #[test_case(|path| fs::create_dir(path).unwrap(), FileProblem::NotAFile; "directory")]
    #[test_case(|path| fs::write(path, vec![b'a'; MAX_REFERENCED_FILE_BYTES as usize + 1]).unwrap(), FileProblem::TooLarge; "too_large")]
    #[test_case(|path| fs::write(path, [0xff]).unwrap(), FileProblem::NotUnicode; "not_utf8")]
    fn unreadable_files_name_their_path(create: fn(&Path), problem: FileProblem) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(KEY_FILE);
        create(&path);
        let error = global(&dir, &format!("a = '{{file:{KEY_FILE}}}'\n")).unwrap_err();
        assert!(
            matches!(
                &error,
                ConfigTextError::Reference(InterpolationError::File { line: 1, path: named, problem: found })
                    if *named == normalize_path(&path) && *found == problem
            ),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_followed_but_parent_directories_come_off_the_text() {
        const LINK: &str = "link";
        const LEXICAL: &str = "lexical";
        const PHYSICAL: &str = "physical";
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("real").join("sub");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, dir.path().join(LINK)).unwrap();
        fs::write(dir.path().join(KEY_FILE), LEXICAL).unwrap();
        fs::write(dir.path().join("real").join(KEY_FILE), PHYSICAL).unwrap();
        fs::write(target.join(KEY_FILE), FILE_SECRET).unwrap();

        let table = global(
            &dir,
            &format!("a = '{{file:{LINK}/{KEY_FILE}}}'\nb = '{{file:{LINK}/../{KEY_FILE}}}'\n"),
        )
        .unwrap();

        assert_eq!(table["a"].as_str(), Some(FILE_SECRET));
        assert_eq!(table["b"].as_str(), Some(LEXICAL));
    }

    #[cfg(all(unix, not(target_vendor = "apple")))]
    #[test]
    fn a_fifo_is_refused_without_opening_it() {
        let dir = TempDir::new().unwrap();
        mkfifoat(CWD, dir.path().join(KEY_FILE), Mode::RUSR | Mode::WUSR).unwrap();
        let error = global(&dir, &format!("a = '{{file:{KEY_FILE}}}'\n")).unwrap_err();
        assert!(
            matches!(
                &error,
                ConfigTextError::Reference(InterpolationError::File {
                    problem: FileProblem::NotAFile,
                    ..
                })
            ),
            "{error}"
        );
    }
}
