use std::{fmt, path::Path};

#[cfg(unix)]
use std::{fs::File, io::Read};

use serde::Deserialize;
use tree_sitter::{Node, Parser};

use crate::types::{ShellCommandAnalysis, ShellCommandScope};

const POLICY_VERSION: u8 = 1;
pub(crate) const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_POLICY_BYTES: usize = 64 * 1024;
const MAX_PATTERNS: usize = 256;
const MAX_PATTERN_BYTES: usize = 512;
const MAX_AST_NODES: usize = 4_096;
const MAX_AST_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum DefaultDecision {
    Allow,
    #[default]
    Deny,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyDocument {
    version: u8,
    #[serde(default)]
    default: DefaultDecision,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

#[derive(Clone)]
pub struct ShellPermissionPolicy {
    default: DefaultDecision,
    allow: Vec<String>,
    deny: Vec<String>,
    yolo: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShellPermissionPolicySummary {
    pub default_decision: &'static str,
    pub allow_rule_count: usize,
    pub deny_rule_count: usize,
    pub yolo: bool,
}

impl fmt::Debug for ShellPermissionPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShellPermissionPolicy")
            .field("default", &self.default)
            .field("allow_count", &self.allow.len())
            .field("deny_count", &self.deny.len())
            .field("yolo", &self.yolo)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellPermissionPolicyError {
    FileUnavailable,
    TooLarge,
    InvalidDocument,
    UnsupportedVersion,
    UnsupportedPlatform,
    TooManyPatterns,
    InvalidPattern,
}

impl fmt::Display for ShellPermissionPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::FileUnavailable => {
                "shell permission policy file could not be opened; verify --shell-policy points to a readable regular file"
            }
            Self::TooLarge => "shell permission policy exceeds the maximum size",
            Self::InvalidDocument => "shell permission policy is invalid",
            Self::UnsupportedVersion => "shell permission policy version is unsupported",
            Self::UnsupportedPlatform => {
                "shell permission policy files require a Unix platform with Bash"
            }
            Self::TooManyPatterns => "shell permission policy contains too many patterns",
            Self::InvalidPattern => "shell permission policy contains an invalid pattern",
        })
    }
}

impl std::error::Error for ShellPermissionPolicyError {}

#[derive(Debug)]
enum AuthorizationError {
    #[cfg(feature = "mcp")]
    Denied(String),
    #[cfg(feature = "mcp")]
    Required(String),
    Opaque,
}

impl fmt::Display for AuthorizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(feature = "mcp")]
            Self::Denied(scope) => write!(
                formatter,
                "Shell execution denied by an immutable Workcell policy rule for scope `{scope}`. Ask the Workcell operator to remove or narrow the matching deny rule if this command is required; tool arguments cannot override it"
            ),
            #[cfg(feature = "mcp")]
            Self::Required(scope) => write!(
                formatter,
                "Shell execution requires an allow rule for scope `{scope}`. Ask the Workcell operator to update the immutable shell policy; tool arguments cannot approve execution"
            ),
            Self::Opaque => formatter.write_str(
                "Shell execution denied because Workcell could not safely classify every command scope. Use static Bash syntax or ask the Workcell operator to review the immutable shell policy; tool arguments cannot approve execution",
            ),
        }
    }
}

impl ShellPermissionPolicy {
    #[must_use]
    pub fn summary(&self) -> ShellPermissionPolicySummary {
        ShellPermissionPolicySummary {
            default_decision: match self.default {
                DefaultDecision::Allow => "allow",
                DefaultDecision::Deny => "deny",
            },
            allow_rule_count: self.allow.len(),
            deny_rule_count: self.deny.len(),
            yolo: self.yolo,
        }
    }

    #[must_use]
    pub const fn restricted() -> Self {
        Self {
            default: DefaultDecision::Deny,
            allow: Vec::new(),
            deny: Vec::new(),
            yolo: false,
        }
    }

    #[must_use]
    pub fn yolo() -> Self {
        Self {
            yolo: true,
            ..Self::restricted()
        }
    }

    pub fn from_toml(contents: &str, yolo: bool) -> Result<Self, ShellPermissionPolicyError> {
        if contents.len() > MAX_POLICY_BYTES {
            return Err(ShellPermissionPolicyError::TooLarge);
        }
        let document: PolicyDocument =
            toml::from_str(contents).map_err(|_| ShellPermissionPolicyError::InvalidDocument)?;
        if document.version != POLICY_VERSION {
            return Err(ShellPermissionPolicyError::UnsupportedVersion);
        }
        if document.allow.len().saturating_add(document.deny.len()) > MAX_PATTERNS {
            return Err(ShellPermissionPolicyError::TooManyPatterns);
        }
        if document
            .allow
            .iter()
            .chain(&document.deny)
            .any(|pattern| !valid_pattern(pattern))
        {
            return Err(ShellPermissionPolicyError::InvalidPattern);
        }
        Ok(Self {
            default: document.default,
            allow: document.allow,
            deny: document.deny,
            yolo,
        })
    }

    pub fn from_file(path: &Path, yolo: bool) -> Result<Self, ShellPermissionPolicyError> {
        #[cfg(not(unix))]
        {
            let _ = (path, yolo);
            Err(ShellPermissionPolicyError::UnsupportedPlatform)
        }
        #[cfg(unix)]
        {
            let file = open_policy_file(path)?;
            let metadata = file
                .metadata()
                .map_err(|_| ShellPermissionPolicyError::InvalidDocument)?;
            if !metadata.file_type().is_file() {
                return Err(ShellPermissionPolicyError::InvalidDocument);
            }
            if metadata.len() > MAX_POLICY_BYTES as u64 {
                return Err(ShellPermissionPolicyError::TooLarge);
            }
            let mut contents = String::new();
            file.take(MAX_POLICY_BYTES as u64 + 1)
                .read_to_string(&mut contents)
                .map_err(|_| ShellPermissionPolicyError::InvalidDocument)?;
            if contents.len() > MAX_POLICY_BYTES {
                return Err(ShellPermissionPolicyError::TooLarge);
            }
            Self::from_toml(&contents, yolo)
        }
    }

    #[cfg(feature = "mcp")]
    pub(crate) fn authorize(&self, command: &str) -> Result<(), String> {
        self.authorize_inner(command)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "mcp")]
    fn authorize_inner(&self, command: &str) -> Result<(), AuthorizationError> {
        let analysis = match analyze(command) {
            Ok(analysis) => analysis,
            Err(_) if self.yolo && self.deny.is_empty() => return Ok(()),
            Err(error) => return Err(error),
        };
        for scope in &analysis.scopes {
            if self
                .deny
                .iter()
                .any(|pattern| scope_matches(pattern, scope))
            {
                return Err(AuthorizationError::Denied(scope.permission.clone()));
            }
        }
        if analysis.opaque && !self.deny.is_empty() {
            return Err(AuthorizationError::Opaque);
        }
        if self.yolo {
            return Ok(());
        }
        if analysis.opaque {
            return Err(AuthorizationError::Opaque);
        }
        if self.default == DefaultDecision::Allow {
            return Ok(());
        }
        for scope in &analysis.scopes {
            if !self
                .allow
                .iter()
                .any(|pattern| scope_matches(pattern, scope))
            {
                return Err(AuthorizationError::Required(scope.permission.clone()));
            }
        }
        Ok(())
    }
}

pub(crate) fn inspect(command: &str) -> ShellCommandAnalysis {
    analyze(command).unwrap_or_else(|_| ShellCommandAnalysis {
        scopes: Vec::new(),
        opaque: true,
    })
}

fn analyze(command: &str) -> Result<ShellCommandAnalysis, AuthorizationError> {
    if command.len() > MAX_COMMAND_BYTES {
        return Err(AuthorizationError::Opaque);
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        return Err(AuthorizationError::Opaque);
    }
    #[cfg(unix)]
    {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .map_err(|_| AuthorizationError::Opaque)?;
        let tree = parser
            .parse(command.as_bytes(), None)
            .ok_or(AuthorizationError::Opaque)?;
        let root = tree.root_node();
        let mut analysis = ShellCommandAnalysis {
            scopes: Vec::new(),
            opaque: root.has_error(),
        };
        let mut stack = vec![(root, 0_usize)];
        let mut visited = 0_usize;
        while let Some((node, depth)) = stack.pop() {
            visited = visited.saturating_add(1);
            if visited > MAX_AST_NODES || depth > MAX_AST_DEPTH {
                return Err(AuthorizationError::Opaque);
            }
            if is_opaque_construct(node.kind()) {
                analysis.opaque = true;
            }
            if is_scope_node(node.kind()) {
                match command_scope(node, command.as_bytes()) {
                    Some((scope, opaque)) => {
                        analysis.scopes.push(scope);
                        analysis.opaque |= opaque;
                    }
                    None => analysis.opaque = true,
                }
            }
            push_named_children(node, depth, &mut stack);
        }
        analysis.scopes.sort_by_key(|scope| scope.start_byte);
        analysis
            .scopes
            .dedup_by(|left, right| left.start_byte == right.start_byte);
        if analysis.scopes.is_empty() {
            analysis.opaque = true;
        }
        Ok(analysis)
    }
}

fn push_named_children<'tree>(
    node: Node<'tree>,
    depth: usize,
    stack: &mut Vec<(Node<'tree>, usize)>,
) {
    for index in (0..node.named_child_count()).rev() {
        let Ok(index) = u32::try_from(index) else {
            continue;
        };
        if let Some(child) = node.named_child(index) {
            stack.push((child, depth.saturating_add(1)));
        }
    }
}

fn is_opaque_construct(kind: &str) -> bool {
    matches!(
        kind,
        "command_substitution"
            | "process_substitution"
            | "subshell"
            | "function_definition"
            | "arithmetic_expansion"
            | "expansion"
            | "variable_assignment"
    )
}

fn is_scope_node(kind: &str) -> bool {
    matches!(
        kind,
        "command" | "declaration_command" | "unset_command" | "test_command"
    )
}

fn command_scope(node: Node<'_>, command: &[u8]) -> Option<(ShellCommandScope, bool)> {
    let (start_byte, raw_name) = if node.kind() == "command" {
        let name = node.child_by_field_name("name")?;
        (name.start_byte(), name.utf8_text(command).ok()?)
    } else {
        let node_source = node.utf8_text(command).ok()?;
        let leading_bytes = node_source
            .len()
            .checked_sub(node_source.trim_start().len())?;
        let source = node_source.trim_start();
        let name_bytes = source.find(char::is_whitespace).unwrap_or(source.len());
        (
            node.start_byte().checked_add(leading_bytes)?,
            source.get(..name_bytes)?,
        )
    };
    let executable = decode_static_word(raw_name)?;
    let raw_scope = std::str::from_utf8(command.get(start_byte..node.end_byte())?).ok()?;
    let source = normalize_shell_whitespace(raw_scope);
    let canonical_name_len = normalize_shell_whitespace(raw_name).len();
    let tail = source.get(canonical_name_len..)?;
    let basename = Path::new(&executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&executable);
    let normalized = if executable == basename && raw_name == basename {
        source.clone()
    } else {
        format!("{basename}{tail}")
    };
    Some((
        ShellCommandScope {
            start_byte,
            source,
            normalized,
            permission: format!("{basename} *"),
        },
        is_opaque_wrapper(basename),
    ))
}

fn decode_static_word(word: &str) -> Option<String> {
    let mut decoded = String::new();
    let mut characters = word.chars();
    let mut quote = None;
    while let Some(character) = characters.next() {
        match (quote, character) {
            (None, '\'') => quote = Some('\''),
            (None, '"') => quote = Some('"'),
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), value) => decoded.push(value),
            (Some('"'), '$' | '`') => return None,
            (Some('"'), '\\') => {
                let escaped = characters.next()?;
                if matches!(escaped, '$' | '`' | '"' | '\\') {
                    decoded.push(escaped);
                } else if escaped != '\n' {
                    decoded.push('\\');
                    decoded.push(escaped);
                }
            }
            (Some('"'), value) => decoded.push(value),
            (None, '\\') => {
                let escaped = characters.next()?;
                if escaped != '\n' {
                    decoded.push(escaped);
                }
            }
            (None, '$' | '`' | '*' | '?' | '[' | '{') => return None,
            (None, value) if value.is_whitespace() || value.is_control() => return None,
            (None, value) => decoded.push(value),
            _ => return None,
        }
    }
    (quote.is_none() && !decoded.is_empty()).then_some(decoded)
}

fn normalize_shell_whitespace(source: &str) -> String {
    let mut normalized = String::with_capacity(source.len());
    let mut quote = None;
    let mut escaped = false;
    let mut pending_space = false;
    for character in source.trim().chars() {
        if escaped {
            normalized.push(character);
            escaped = false;
            continue;
        }
        match (quote, character) {
            (_, '\\') => {
                if pending_space {
                    normalized.push(' ');
                    pending_space = false;
                }
                normalized.push(character);
                escaped = true;
            }
            (None, '\'' | '"') => {
                if pending_space {
                    normalized.push(' ');
                    pending_space = false;
                }
                quote = Some(character);
                normalized.push(character);
            }
            (Some(active), value) if active == value => {
                quote = None;
                normalized.push(value);
            }
            (None, value) if value.is_whitespace() => pending_space = !normalized.is_empty(),
            (_, value) => {
                if pending_space {
                    normalized.push(' ');
                    pending_space = false;
                }
                normalized.push(value);
            }
        }
    }
    normalized
}

fn is_opaque_wrapper(executable: &str) -> bool {
    matches!(
        Path::new(executable)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(executable),
        "command"
            | "env"
            | "sudo"
            | "doas"
            | "eval"
            | "source"
            | "."
            | "bash"
            | "sh"
            | "time"
            | "coproc"
    )
}

fn valid_pattern(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern.len() <= MAX_PATTERN_BYTES
        && pattern.trim() == pattern
        && !pattern.chars().any(char::is_control)
        && pattern.matches('*').count() <= 1
        && (!pattern.contains('*') || pattern.ends_with('*'))
}

#[cfg(feature = "mcp")]
fn scope_matches(pattern: &str, scope: &ShellCommandScope) -> bool {
    matches_text(pattern, &scope.source) || matches_text(pattern, &scope.normalized)
}

#[cfg(feature = "mcp")]
fn matches_text(pattern: &str, scope: &str) -> bool {
    if matches!(pattern, "*" | "**") {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(" *") {
        return scope == prefix || scope.starts_with(&format!("{prefix} "));
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return scope.starts_with(prefix);
    }
    scope == pattern
}

#[cfg(unix)]
fn open_policy_file(path: &Path) -> Result<File, ShellPermissionPolicyError> {
    use rustix::fs::{Mode, OFlags, open};

    let descriptor = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| ShellPermissionPolicyError::FileUnavailable)?;
    Ok(File::from(descriptor))
}

#[cfg(all(test, feature = "mcp"))]
mod tests {
    use super::*;

    fn policy(contents: &str, yolo: bool) -> ShellPermissionPolicy {
        ShellPermissionPolicy::from_toml(contents, yolo).unwrap()
    }

    #[test]
    fn splits_chained_commands_into_independent_scopes() {
        let analysis = analyze("git diff && rm -rf /").unwrap();
        assert!(!analysis.opaque);
        assert_eq!(
            analysis
                .scopes
                .iter()
                .map(|scope| scope.permission.as_str())
                .collect::<Vec<_>>(),
            ["git *", "rm *"]
        );
    }

    #[test]
    fn one_denied_scope_rejects_the_entire_chain() {
        let policy = policy(
            "version = 1\ndefault = 'deny'\nallow = ['git *']\ndeny = ['rm *']\n",
            false,
        );
        assert!(matches!(
            policy.authorize_inner("git diff && rm -rf /"),
            Err(AuthorizationError::Denied(scope)) if scope == "rm *"
        ));
    }

    #[test]
    fn yolo_allows_unmatched_but_not_explicitly_denied_commands() {
        let policy = policy("version = 1\ndeny = ['rm *']\n", true);
        assert!(policy.authorize_inner("git diff").is_ok());
        assert!(matches!(
            policy.authorize_inner("rm -rf /"),
            Err(AuthorizationError::Denied(_))
        ));
    }

    #[test]
    fn dynamic_execution_is_opaque_without_yolo() {
        let restricted = ShellPermissionPolicy::restricted();
        assert!(matches!(
            restricted.authorize_inner("echo $(whoami)"),
            Err(AuthorizationError::Opaque)
        ));
        assert!(
            ShellPermissionPolicy::yolo()
                .authorize_inner("echo $(whoami)")
                .is_ok()
        );
    }

    #[test]
    fn absolute_executables_match_basename_denies() {
        let policy = policy("version = 1\ndefault = 'allow'\ndeny = ['rm *']\n", false);
        assert!(matches!(
            policy.authorize_inner("/bin/rm -rf /tmp/example"),
            Err(AuthorizationError::Denied(_))
        ));
    }

    #[test]
    fn denies_are_not_bypassed_by_shell_lexical_forms() {
        let policy = policy("version = 1\ndefault = 'allow'\ndeny = ['rm *']\n", true);
        for command in [
            "rm\t-rf /tmp/example",
            "MODE=test rm -rf /tmp/example",
            ">/tmp/output rm -rf /tmp/example",
            "'r''m' -rf /tmp/example",
            "r\\m -rf /tmp/example",
        ] {
            assert!(
                matches!(
                    policy.authorize_inner(command),
                    Err(AuthorizationError::Denied(_))
                ),
                "deny bypassed by {command:?}"
            );
        }
    }

    #[test]
    fn wrappers_remain_visible_deny_scopes() {
        let policy = policy("version = 1\ndeny = ['bash *']\n", true);
        assert!(matches!(
            policy.authorize_inner("bash -c 'printf hidden'"),
            Err(AuthorizationError::Denied(_))
        ));
    }

    #[test]
    fn universal_deny_rejects_opaque_commands_under_yolo() {
        let policy = policy("version = 1\ndeny = ['*']\n", true);
        assert!(matches!(
            policy.authorize_inner("$COMMAND argument"),
            Err(AuthorizationError::Opaque)
        ));
    }

    #[test]
    fn opaque_direct_execution_cannot_bypass_configured_denies() {
        let policy = policy("version = 1\ndeny = ['rm *']\n", true);
        for command in [
            "time rm -rf /tmp/example",
            "coproc rm -rf /tmp/example",
            "$'rm' -rf /tmp/example",
            "$\"rm\" -rf /tmp/example",
        ] {
            assert!(
                policy.authorize_inner(command).is_err(),
                "deny bypassed by {command:?}"
            );
        }
    }

    #[test]
    fn declaration_commands_are_classified() {
        let policy = policy(
            "version = 1\ndefault = 'allow'\ndeny = ['export *']\n",
            true,
        );
        assert!(matches!(
            policy.authorize_inner("export NAME=value"),
            Err(AuthorizationError::Denied(_))
        ));
    }

    #[test]
    fn policy_documents_are_strict_and_bounded() {
        assert_eq!(
            ShellPermissionPolicy::from_toml("version = 2", false).unwrap_err(),
            ShellPermissionPolicyError::UnsupportedVersion
        );
        assert_eq!(
            ShellPermissionPolicy::from_toml("version = 1\nallow = ['g*t']", false).unwrap_err(),
            ShellPermissionPolicyError::InvalidPattern
        );
    }

    #[test]
    fn parsed_summary_discloses_policy_shape_without_rule_values() {
        let policy = policy(
            "version = 1\ndefault = 'deny'\nallow = ['private-command *']\ndeny = ['secret-path *']\n",
            false,
        );
        assert_eq!(
            policy.summary(),
            ShellPermissionPolicySummary {
                default_decision: "deny",
                allow_rule_count: 1,
                deny_rule_count: 1,
                yolo: false,
            }
        );
        let debug = format!("{policy:?}");
        assert!(!debug.contains("private-command"));
        assert!(!debug.contains("secret-path"));
    }

    #[test]
    fn policy_file_reads_are_bounded_and_reject_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let oversized = directory.path().join("oversized.toml");
        std::fs::write(&oversized, "x".repeat(MAX_POLICY_BYTES + 1)).unwrap();
        assert_eq!(
            ShellPermissionPolicy::from_file(&oversized, false).unwrap_err(),
            ShellPermissionPolicyError::TooLarge
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let target = directory.path().join("policy.toml");
            let link = directory.path().join("policy-link.toml");
            std::fs::write(&target, "version = 1\n").unwrap();
            symlink(&target, &link).unwrap();
            assert_eq!(
                ShellPermissionPolicy::from_file(&link, false).unwrap_err(),
                ShellPermissionPolicyError::FileUnavailable
            );
        }
    }
}
