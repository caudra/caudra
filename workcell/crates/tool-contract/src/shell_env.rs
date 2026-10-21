//! Which environment variables a shell command may receive beyond the fixed allowlist.
//!
//! Hosts and servers share these rules so a name refused by configuration is the same name the
//! shell drops at launch.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;

use serde::{Deserialize, Serialize};

pub const BASH_EXECUTABLE_ENV: &str = "WORKCELL_BASH_EXECUTABLE";
pub const MAX_VARIABLE_NAME_BYTES: usize = 256;
pub const MAX_SELECTION_PATTERNS: usize = 256;
pub const MAX_SET_ENTRIES: usize = 64;
pub const MAX_SET_VALUE_BYTES: usize = 4096;
/// Bash gives variables under this prefix special meaning, `BASH_ENV` and exported functions among
/// them.
const BASH_VARIABLE_PREFIX: &str = "BASH";
/// Names that decide which Bash runs or how it starts. The command-context analysis a host
/// authorizes against assumes Bash started without them.
const STARTUP_VARIABLES: &[&str] = &[
    BASH_EXECUTABLE_ENV,
    "ENV",
    "SHELLOPTS",
    "POSIXLY_CORRECT",
    "CDPATH",
    "PWD",
    "OLDPWD",
    "PROMPT_COMMAND",
    "PS4",
    "IFS",
    "GLOBIGNORE",
    "EXECIGNORE",
];
const PROXY_VARIABLES: &[&str] = &["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"];
/// What every shell command receives from its host, when set, before any configured forwarding.
///
/// Proxy selection is forwarded verbatim, credentials included. Under a sandbox whose only egress
/// is an enforcing proxy, withholding it makes every network-using command fail closed; the
/// accepted cost is that a credentialed proxy URL becomes readable by any admitted command. Both
/// cases are listed because curl reads the lowercase names while most other clients read the
/// uppercase ones.
pub const BASE_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LC_ALL",
    "TERM",
    "ComSpec",
    "SystemRoot",
    "WINDIR",
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];
/// Set on every command after everything forwarded, so tools that honour the conventions print no
/// colour codes.
pub const FORCED_VARIABLES: &[(&str, &str)] = &[("NO_COLOR", "1"), ("CLICOLOR", "0")];
/// Prefixes of names that configure a Workcell server, its bearer token among them.
const SERVER_PREFIXES: &[&str] = &["WORKCELL_", "SEARXNG_"];
/// Matched anywhere inside one `_`-separated word of a name.
const SECRET_MARKERS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "COOKIE",
    "AUTHORIZATION",
];
/// Matched at the end of one word, so `APIKEY` counts and `KEYBOARD` does not.
const SECRET_WORD_SUFFIXES: &[&str] = &["KEY"];
/// Matched as a whole word, so `AUTH` counts and `AUTHOR` does not.
const SECRET_WORDS: &[&str] = &["AUTH", "PASS", "PAT"];
const WILDCARD: char = '*';
const ENV_REFERENCE_PREFIX: &str = "{env:";

#[must_use]
pub fn is_shell_startup_variable(name: &str) -> bool {
    name.starts_with(BASH_VARIABLE_PREFIX) || STARTUP_VARIABLES.contains(&name)
}

/// The whole environment a command starts with, for a caller that clears its own first: the
/// allowlisted names `read` answers, then `overlay` in order, a later entry replacing an earlier
/// one, minus the names that decide how Bash starts, then [`FORCED_VARIABLES`].
///
/// The forced names come last, so no forwarded name can replace them. A cleared environment has
/// no inherited `NO_COLOR`, `FORCE_COLOR`, or `CLICOLOR_FORCE`, so they set a default rather than
/// override an operator's choice, and an explicit `--color=always` still wins. They describe the
/// environment a command runs in, not how its output is rendered, so no output setting turns them
/// off.
#[must_use]
pub fn command_environment(
    overlay: &[(String, OsString)],
    read: impl Fn(&str) -> Option<OsString>,
) -> Vec<(String, OsString)> {
    let mut environment: BTreeMap<String, OsString> = BASE_ALLOWLIST
        .iter()
        .filter_map(|&name| read(name).map(|value| (name.to_owned(), value)))
        .collect();
    environment.extend(
        overlay
            .iter()
            .filter(|(name, _)| !is_shell_startup_variable(name))
            .cloned(),
    );
    environment.extend(
        FORCED_VARIABLES
            .iter()
            .map(|&(name, value)| (name.to_owned(), OsString::from(value))),
    );
    environment.into_iter().collect()
}

#[must_use]
pub fn is_proxy_variable(name: &str) -> bool {
    PROXY_VARIABLES
        .iter()
        .any(|proxy| proxy.eq_ignore_ascii_case(name))
}

#[must_use]
pub fn is_server_variable(name: &str) -> bool {
    SERVER_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// A heuristic over `_`-separated words, not detection: a credential under an ordinary name passes.
#[must_use]
pub fn looks_secret(name: &str) -> bool {
    name.to_ascii_uppercase().split('_').any(|word| {
        SECRET_MARKERS.iter().any(|marker| word.contains(marker))
            || SECRET_WORD_SUFFIXES
                .iter()
                .any(|suffix| word.ends_with(suffix))
            || SECRET_WORDS.contains(&word)
    })
}

#[must_use]
pub fn is_valid_variable_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= MAX_VARIABLE_NAME_BYTES
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvPatternError {
    #[error("an environment pattern cannot be empty")]
    Empty,
    #[error(
        "{0:?} is not a variable name or NAME_* prefix: use letters, digits, and _, not starting with a digit"
    )]
    Invalid(String),
    #[error("{0} decides how Bash starts, so no shell command can receive it")]
    Reserved(String),
    #[error("{0} is the proxy the execution host enforces, so it cannot be replaced")]
    Proxy(String),
    #[error("{0} configures Workcell itself, so no shell command can receive it")]
    Server(String),
    #[error("{0} looks like a credential; put it in the image or host environment instead")]
    Secret(String),
    #[error("the value of {0} reads the environment; values here are kept as written")]
    Reference(String),
    #[error("the value of {name} is {bytes} bytes, more than {MAX_SET_VALUE_BYTES}")]
    ValueTooLarge { name: String, bytes: usize },
    #[error("at most {MAX_SET_ENTRIES} values may be set")]
    TooManyValues,
    #[error("at most {MAX_SELECTION_PATTERNS} patterns may be forwarded")]
    TooManyPatterns,
}

/// One forwarding entry: every name, names under a prefix, or one exact name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum EnvPattern {
    All,
    Prefix(String),
    Exact(String),
}

impl EnvPattern {
    pub fn parse(text: &str) -> Result<Self, EnvPatternError> {
        if text.is_empty() {
            return Err(EnvPatternError::Empty);
        }
        if text == "*" {
            return Ok(Self::All);
        }
        let (stem, prefix) = match text.strip_suffix(WILDCARD) {
            Some(stem) => (stem, true),
            None => (text, false),
        };
        if !is_valid_variable_name(stem) {
            return Err(EnvPatternError::Invalid(text.to_owned()));
        }
        Ok(if prefix {
            Self::Prefix(stem.to_owned())
        } else {
            Self::Exact(stem.to_owned())
        })
    }

    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Prefix(prefix) => name.starts_with(prefix.as_str()),
            Self::Exact(exact) => exact == name,
        }
    }

    #[must_use]
    pub fn exact(&self) -> Option<&str> {
        match self {
            Self::Exact(name) => Some(name),
            Self::All | Self::Prefix(_) => None,
        }
    }

    /// The first Bash startup name this entry would capture, for an entry other than `*`.
    #[must_use]
    pub fn reserved_capture(&self) -> Option<String> {
        match self {
            Self::All => None,
            Self::Exact(name) => is_shell_startup_variable(name).then(|| name.clone()),
            Self::Prefix(prefix) => {
                if BASH_VARIABLE_PREFIX.starts_with(prefix.as_str())
                    || prefix.starts_with(BASH_VARIABLE_PREFIX)
                {
                    return Some(format!("{BASH_VARIABLE_PREFIX}{WILDCARD}"));
                }
                STARTUP_VARIABLES
                    .iter()
                    .find(|name| name.starts_with(prefix.as_str()))
                    .map(|name| (*name).to_owned())
            }
        }
    }
}

impl fmt::Display for EnvPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("*"),
            Self::Prefix(prefix) => write!(f, "{prefix}{WILDCARD}"),
            Self::Exact(name) => f.write_str(name),
        }
    }
}

impl TryFrom<String> for EnvPattern {
    type Error = EnvPatternError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<EnvPattern> for String {
    fn from(pattern: EnvPattern) -> Self {
        pattern.to_string()
    }
}

/// What a host asks a remote execution server to forward from the server's own environment, plus
/// literal values. It carries names and non-secret literals only, never a host value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellEnvironmentSelection {
    #[serde(default)]
    pub pass: Vec<EnvPattern>,
    #[serde(default)]
    pub set: BTreeMap<String, String>,
}

impl ShellEnvironmentSelection {
    /// The default for a guest the host provisioned: everything the server permits.
    #[must_use]
    pub fn everything() -> Self {
        Self {
            pass: vec![EnvPattern::All],
            set: BTreeMap::new(),
        }
    }

    /// Rules for a selection a server resolves against its own environment.
    pub fn validate(&self) -> Result<(), EnvPatternError> {
        if self.pass.len() > MAX_SELECTION_PATTERNS {
            return Err(EnvPatternError::TooManyPatterns);
        }
        if self.set.len() > MAX_SET_ENTRIES {
            return Err(EnvPatternError::TooManyValues);
        }
        if let Some(name) = self.pass.iter().find_map(EnvPattern::reserved_capture) {
            return Err(EnvPatternError::Reserved(name));
        }
        self.set
            .iter()
            .try_for_each(|(name, value)| validate_literal(name, value))
    }
}

/// Rules for a literal a remote server receives: never a reference, a credential-looking name,
/// a proxy, a startup name, or a server setting.
pub fn validate_literal(name: &str, value: &str) -> Result<(), EnvPatternError> {
    if !is_valid_variable_name(name) {
        return Err(EnvPatternError::Invalid(name.to_owned()));
    }
    if is_shell_startup_variable(name) {
        return Err(EnvPatternError::Reserved(name.to_owned()));
    }
    if is_proxy_variable(name) {
        return Err(EnvPatternError::Proxy(name.to_owned()));
    }
    if is_server_variable(name) {
        return Err(EnvPatternError::Server(name.to_owned()));
    }
    if looks_secret(name) {
        return Err(EnvPatternError::Secret(name.to_owned()));
    }
    if value.contains(ENV_REFERENCE_PREFIX) {
        return Err(EnvPatternError::Reference(name.to_owned()));
    }
    if value.len() > MAX_SET_VALUE_BYTES {
        return Err(EnvPatternError::ValueTooLarge {
            name: name.to_owned(),
            bytes: value.len(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("*", Ok(EnvPattern::All) ; "everything")]
    #[test_case("AWS_*", Ok(EnvPattern::Prefix("AWS_".into())) ; "prefix")]
    #[test_case("DOCKER_HOST", Ok(EnvPattern::Exact("DOCKER_HOST".into())) ; "exact")]
    #[test_case("", Err(EnvPatternError::Empty) ; "empty")]
    #[test_case("A*B", Err(EnvPatternError::Invalid("A*B".into())) ; "inner_wildcard")]
    #[test_case("**", Err(EnvPatternError::Invalid("**".into())) ; "double_wildcard")]
    #[test_case("1PATH", Err(EnvPatternError::Invalid("1PATH".into())) ; "leading_digit")]
    #[test_case("A-B", Err(EnvPatternError::Invalid("A-B".into())) ; "dash")]
    fn parse(text: &str, expected: Result<EnvPattern, EnvPatternError>) {
        assert_eq!(EnvPattern::parse(text), expected);
    }

    #[test_case("BASH_ENV", Some("BASH_ENV") ; "bash_exact")]
    #[test_case("BA*", Some("BASH*") ; "prefix_of_bash")]
    #[test_case("BASH_FUNC_*", Some("BASH*") ; "under_bash")]
    #[test_case("P*", Some("POSIXLY_CORRECT") ; "prefix_of_startup")]
    #[test_case("AWS_*", None ; "unrelated_prefix")]
    #[test_case("DOCKER_HOST", None ; "unrelated_exact")]
    #[test_case("*", None ; "everything_is_filtered_at_launch")]
    fn reserved_capture(text: &str, expected: Option<&str>) {
        assert_eq!(
            EnvPattern::parse(text)
                .unwrap()
                .reserved_capture()
                .as_deref(),
            expected
        );
    }

    #[test_case("RUST_LOG", "info", None ; "plain")]
    #[test_case("https_proxy", "http://x", Some(EnvPatternError::Proxy("https_proxy".into())) ; "proxy_lowercase")]
    #[test_case("WORKCELL_MCP_HTTP_TOKEN", "x", Some(EnvPatternError::Server("WORKCELL_MCP_HTTP_TOKEN".into())) ; "server_token")]
    #[test_case("GITHUB_TOKEN", "x", Some(EnvPatternError::Secret("GITHUB_TOKEN".into())) ; "secret_name")]
    #[test_case("IFS", "/", Some(EnvPatternError::Reserved("IFS".into())) ; "startup")]
    #[test_case("HOST", "{env:HOST}", Some(EnvPatternError::Reference("HOST".into())) ; "reference")]
    fn literal(name: &str, value: &str, expected: Option<EnvPatternError>) {
        assert_eq!(validate_literal(name, value).err(), expected);
    }

    #[test_case("ANTHROPIC_API_KEY", true ; "api_key")]
    #[test_case("OPENAI_APIKEY", true ; "joined_key")]
    #[test_case("GH_TOKEN", true ; "token")]
    #[test_case("DB_PASS", true ; "pass_word")]
    #[test_case("NPM_AUTH", true ; "auth_word")]
    #[test_case("GIT_AUTHOR_NAME", false ; "author")]
    #[test_case("KEYBOARD_LAYOUT", false ; "keyboard")]
    #[test_case("DOCKER_HOST", false ; "docker_host")]
    #[test_case("PATH", false ; "path")]
    fn secret_heuristic(name: &str, expected: bool) {
        assert_eq!(looks_secret(name), expected);
    }

    #[test]
    fn literal_values_are_bounded() {
        let value = "x".repeat(MAX_SET_VALUE_BYTES + 1);
        assert_eq!(
            validate_literal("BIG", &value),
            Err(EnvPatternError::ValueTooLarge {
                name: "BIG".into(),
                bytes: MAX_SET_VALUE_BYTES + 1
            })
        );
    }

    const HOST: &[(&str, &str)] = &[
        ("PATH", "/usr/bin"),
        ("HOME", "/home/user"),
        ("GH_TOKEN", "token-canary"),
        ("NO_COLOR", "0"),
    ];

    fn environment(overlay: &[(&str, &str)]) -> BTreeMap<String, String> {
        let overlay: Vec<(String, OsString)> = overlay
            .iter()
            .map(|&(name, value)| (name.to_owned(), OsString::from(value)))
            .collect();
        let read = |name: &str| {
            HOST.iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, value)| OsString::from(*value))
        };
        command_environment(&overlay, read)
            .into_iter()
            .map(|(name, value)| (name, value.to_string_lossy().into_owned()))
            .collect()
    }

    #[test_case(&[], "GH_TOKEN", None ; "unlisted_dropped")]
    #[test_case(&[], "PATH", Some("/usr/bin") ; "allowlisted_kept")]
    #[test_case(&[("GH_TOKEN", "token-canary")], "GH_TOKEN", Some("token-canary") ; "passed_forwarded")]
    #[test_case(&[("RUST_LOG", "debug"), ("RUST_LOG", "info")], "RUST_LOG", Some("info") ; "set_beats_pass")]
    #[test_case(&[("HOME", "/elsewhere")], "HOME", Some("/elsewhere") ; "overlay_beats_allowlist")]
    #[test_case(&[("BASH_ENV", "/tmp/rc")], "BASH_ENV", None ; "startup_variable_refused")]
    #[test_case(&[("IFS", "/")], "IFS", None ; "startup_name_refused")]
    #[test_case(&[], "NO_COLOR", Some("1") ; "no_color_forced")]
    #[test_case(&[("NO_COLOR", "0")], "NO_COLOR", Some("1") ; "forwarded_no_color_replaced")]
    #[test_case(&[], "CLICOLOR", Some("0") ; "clicolor_forced")]
    fn command_environment_matrix(overlay: &[(&str, &str)], name: &str, expected: Option<&str>) {
        assert_eq!(environment(overlay).get(name).map(String::as_str), expected);
    }

    #[test]
    fn selection_round_trips_patterns_as_strings() {
        let selection: ShellEnvironmentSelection =
            serde_json::from_str(r#"{"pass":["*","AWS_*","HOME"],"set":{"RUST_LOG":"info"}}"#)
                .unwrap();
        assert_eq!(
            serde_json::to_string(&selection).unwrap(),
            r#"{"pass":["*","AWS_*","HOME"],"set":{"RUST_LOG":"info"}}"#
        );
        assert_eq!(selection.validate(), Ok(()));
    }

    #[test]
    fn selection_refuses_a_reserved_prefix() {
        let selection = ShellEnvironmentSelection {
            pass: vec![EnvPattern::parse("BASH_*").unwrap()],
            set: BTreeMap::new(),
        };
        assert_eq!(
            selection.validate(),
            Err(EnvPatternError::Reserved("BASH*".into()))
        );
    }
}
