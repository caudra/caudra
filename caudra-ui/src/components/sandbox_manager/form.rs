use super::super::text_editor::TextEditor;
use caudra_config::sandbox::{
    CidrRule, DomainRule, MAX_NETWORK_RULES, RecordKind, Revision, SandboxDraft, SandboxError,
    SandboxName, SandboxOrigin, SandboxRecord, TransferPolicy,
};
use caudra_storage::sandbox_auth::SandboxCredentialRef;
use caudra_workspace::WorkspacePath;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};
use std::num::NonZeroU32;

const POSITIVE: &str = "Use a positive whole number (no units or rounding).";
const BOOLEAN: &str = "Use true or false; this changes saved defaults only.";
pub(super) const DOMAIN_HELP: &str = "One hostname per line, e.g. api.example.com or *.example.com (subdomains only). No URLs, paths, ports or methods. Enter adds a line; Alt+Insert appends; Alt+Delete clears the current line. Blank lines are ignored. Empty domains AND CIDRs deny all under required enforcement. Operator blocks still win.";
pub(super) const CIDR_HELP: &str = "One IPv4/IPv6 network per line; normalized at review. Enter adds a line; Alt+Insert appends; Alt+Delete clears the current line. Blank lines are ignored. Operator metadata/private-destination blocks still win.";
const PROFILE_FIELDS: &[(&str, &str, Input, &str)] = &[
    (
        "provider",
        "Provider",
        Input::Name,
        "Reference a saved provider; credentials stay client-local.",
    ),
    (
        "template",
        "Template ID",
        Input::Name,
        "Daemon catalog ID, never a host image path.",
    ),
    (
        "template_revision",
        "Immutable revision",
        Input::Revision,
        "sha256: followed by 64 hex digits; never a mutable alias.",
    ),
    ("cpus", "CPU count", Input::Positive, POSITIVE),
    ("memory_mib", "Memory (MiB)", Input::Positive, POSITIVE),
    (
        "disk_gib",
        "Disk (GiB)",
        Input::Positive,
        "No silent growth, shrinking, or rounding; provider/image limits apply.",
    ),
    (
        "cwd",
        "Guest working directory",
        Input::Cwd,
        "Workspace-relative path; '.' is the guest workspace root.",
    ),
    ("persistent", "Persistent disk", Input::Boolean, BOOLEAN),
    (
        "running_ttl_seconds",
        "Running TTL (seconds)",
        Input::Positive,
        POSITIVE,
    ),
    (
        "on_exit",
        "On exit",
        Input::Choice(&["detach"]),
        "Locked: only detach is supported; closing never destroys a disk.",
    ),
    (
        "network",
        "Network policy",
        Input::Name,
        "Shared policy reference. Use Network below to inspect/edit reusable policies.",
    ),
    (
        "transfer",
        "Transfer policy",
        Input::Name,
        "Shared policy reference. No files move when defaults are saved.",
    ),
];
const PROVIDER_FIELDS: &[(&str, &str, Input, &str)] = &[
    (
        "kind",
        "Provider kind",
        Input::Choice(&["e2b-libvirt"]),
        "Locked: e2b-libvirt is the only supported provider.",
    ),
    (
        "api_endpoint",
        "Lifecycle API origin",
        Input::Origin,
        "HTTPS or numeric loopback HTTP; no credentials, path, query or fragment.",
    ),
    (
        "proxy_endpoint",
        "Workcell data proxy origin",
        Input::Origin,
        "Not the guest egress firewall or the model-provider proxy.",
    ),
    (
        "credential_ref",
        "Credential reference",
        Input::Credential,
        "sandbox-api:NAME only. Never paste a token; this editor cannot store secrets.",
    ),
];
const NETWORK_FIELDS: &[(&str, &str, Input, &str)] = &[
    (
        "enforcement",
        "Enforcement",
        Input::Choice(&["required", "off"]),
        "Required denies by default. Off is explicitly UNRESTRICTED and cannot carry rules.",
    ),
    (
        "tls_mode",
        "TLS mode",
        Input::Choice(&["sni-only", "mitm"]),
        "SNI-only preserves end-to-end encryption. MITM needs a guest CA and can break certificate pinning.",
    ),
    (
        "domains",
        "Allowed domains (one per line)",
        Input::Domains,
        DOMAIN_HELP,
    ),
    (
        "cidrs",
        "Allowed CIDRs (one per line)",
        Input::Cidrs,
        CIDR_HELP,
    ),
];
const TRANSFER_FIELDS: &[(&str, &str, Input, &str)] = &[
    (
        "respect_gitignore",
        "Respect gitignore",
        Input::Boolean,
        "Additional filtering, not a security boundary.",
    ),
    (
        "initial_seed",
        "Initial seed",
        Input::Choice(&["ask", "none"]),
        "Ask or none; no automatic file transfers.",
    ),
    (
        "delete_extraneous",
        "Delete extraneous files",
        Input::Boolean,
        "Locked: automatic deletion is unsupported.",
    ),
    (
        "exclude",
        "Exclusion globs (one per line)",
        Input::Excludes,
        "Relative globs only; no traversal, negation or absolute client paths. Server confinement still applies.",
    ),
];

#[derive(Clone)]
pub(super) enum Input {
    Name,
    Revision,
    Origin,
    Credential,
    Cwd,
    Positive,
    Boolean,
    Choice(&'static [&'static str]),
    Domains,
    Cidrs,
    Excludes,
}

pub(super) struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub input: Input,
    pub help: &'static str,
    pub editor: TextEditor,
    pub error: Option<String>,
    pub locked: Option<String>,
    initial: String,
}

impl Field {
    pub fn text(&self) -> String {
        self.editor.text()
    }

    pub fn multiline(&self) -> bool {
        matches!(self.input, Input::Domains | Input::Cidrs | Input::Excludes)
    }

    fn value(&self) -> Result<Value, String> {
        let text = self.text();
        let sandbox_error = |error: SandboxError| error.to_string();
        let string = match &self.input {
            Input::Name => SandboxName::parse(&text)
                .map(|value| value.to_string())
                .map_err(sandbox_error)?,
            Input::Revision => Revision::parse(&text)
                .map(|value| value.as_str().to_owned())
                .map_err(sandbox_error)?,
            Input::Origin => SandboxOrigin::parse(&text)
                .map(|value| value.as_str().to_owned())
                .map_err(sandbox_error)?,
            Input::Credential => text
                .parse::<SandboxCredentialRef>()
                .map(|value| value.to_string())
                .map_err(|error| error.to_string())?,
            Input::Cwd => WorkspacePath::new(text)
                .map(|value| value.to_string())
                .map_err(|error| error.to_string())?,
            Input::Positive => {
                return text
                    .parse::<NonZeroU32>()
                    .map(|value| Value::from(value.get()))
                    .map_err(|_| POSITIVE.into());
            }
            Input::Boolean => {
                return text
                    .parse::<bool>()
                    .map(Value::Bool)
                    .map_err(|_| BOOLEAN.into());
            }
            Input::Choice(choices) => {
                if !choices.contains(&text.as_str()) {
                    return Err(format!("Choose {}.", choices.join(" or ")));
                }
                text
            }
            Input::Domains | Input::Cidrs | Input::Excludes => {
                let values = match self.input {
                    Input::Domains => parse_domains(&text)?,
                    Input::Cidrs => parse_cidrs(&text)?,
                    _ => text.lines().map(str::to_owned).collect(),
                };
                return Ok(Value::Array(
                    values.into_iter().map(Value::String).collect(),
                ));
            }
        };
        Ok(Value::String(string))
    }
}

pub(super) struct Form {
    pub kind: RecordKind,
    pub original: Option<SandboxName>,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub editing: bool,
    pub scroll: usize,
    pub reveal_focus: bool,
}

impl Form {
    pub fn new(
        kind: RecordKind,
        name: Option<SandboxName>,
        record: Option<SandboxRecord>,
    ) -> Result<Self, String> {
        let value = match record {
            Some(SandboxRecord::Profile(value)) => serde_json::to_value(value),
            Some(SandboxRecord::Provider(value)) => serde_json::to_value(value),
            Some(SandboxRecord::Network(value)) => serde_json::to_value(value),
            Some(SandboxRecord::Transfer(value)) => serde_json::to_value(value),
            None => Ok(Value::Null),
        }
        .map_err(|_| "Could not prepare sandbox form".to_owned())?;
        let specs = match kind {
            RecordKind::Profile => PROFILE_FIELDS,
            RecordKind::Provider => PROVIDER_FIELDS,
            RecordKind::Network => NETWORK_FIELDS,
            RecordKind::Transfer => TRANSFER_FIELDS,
        };
        let mut fields = Vec::with_capacity(specs.len() + 1);
        let name_spec = (
            "name",
            "Name",
            Input::Name,
            "A unique configuration name, not an instance ID. Duplicate to rename existing records.",
        );
        for (key, label, input, help) in [name_spec].iter().chain(specs) {
            let initial = if *key == "name" {
                name.as_ref().map(ToString::to_string).unwrap_or_default()
            } else if let Some(value) = value.get(key) {
                value_text(value)
            } else {
                default_text(key)
            };
            let locked = match *key {
                "name" if name.is_some() => {
                    Some("Duplicate to give an existing record a new name.".into())
                }
                "kind" | "on_exit" | "delete_extraneous" => Some((*help).into()),
                _ => None,
            };
            let mut editor = TextEditor::new();
            editor.set_text(initial.clone());
            fields.push(Field {
                key,
                label,
                input: input.clone(),
                help,
                editor,
                error: None,
                locked,
                initial,
            });
        }
        Ok(Self {
            kind,
            original: name,
            fields,
            focus: 0,
            editing: false,
            scroll: 0,
            reveal_focus: true,
        })
    }

    pub fn duplicate(&mut self) {
        self.original = None;
        self.fields[0].editor.set_text(String::new());
        self.fields[0].locked = None;
        self.focus = 0;
        self.editing = true;
    }

    pub fn dirty(&self) -> bool {
        self.original.is_none()
            || self
                .fields
                .iter()
                .any(|field| field.text() != field.initial)
    }

    pub fn rebase(&mut self, draft: &SandboxDraft) {
        let Ok(name) = SandboxName::parse(&self.text("name")) else {
            return;
        };
        let Ok(record) = draft.get(self.kind.clone(), &name) else {
            return;
        };
        let Ok(saved) = Self::new(self.kind.clone(), Some(name.clone()), Some(record)) else {
            return;
        };
        self.original = Some(name);
        for (field, saved) in self.fields.iter_mut().zip(saved.fields) {
            field.initial = saved.initial;
        }
        self.fields[0].locked = Some("Duplicate to give an existing record a new name.".into());
    }

    pub fn text(&self, key: &str) -> String {
        self.fields
            .iter()
            .find(|field| field.key == key)
            .map(Field::text)
            .unwrap_or_default()
    }

    pub fn validate(&mut self, draft: &SandboxDraft) -> Result<SandboxDraft, String> {
        let mut object = Map::new();
        let mut first_error = None;
        for (index, field) in self.fields.iter_mut().enumerate() {
            field.error = None;
            match field.value() {
                Ok(value) => {
                    object.insert(field.key.into(), value);
                }
                Err(error) => {
                    field.error = Some(error.clone());
                    first_error.get_or_insert((index, error));
                }
            }
        }
        if let Some((_, error)) = first_error {
            return Err(error);
        }
        object.remove("name");
        let name = SandboxName::parse(&self.text("name")).map_err(|error| error.to_string())?;
        let object = Value::Object(object);
        let record = match self.kind {
            RecordKind::Profile => serde_json::from_value(object).map(SandboxRecord::Profile),
            RecordKind::Provider => serde_json::from_value(object).map(SandboxRecord::Provider),
            RecordKind::Network => serde_json::from_value(object).map(SandboxRecord::Network),
            RecordKind::Transfer => serde_json::from_value(object).map(SandboxRecord::Transfer),
        }
        .map_err(|_| "Invalid sandbox form; check field types".to_owned())?;
        let mut candidate = draft.clone();
        let result = if self.original.is_some() {
            candidate.update(name, record)
        } else {
            candidate.create(name, record)
        };
        result
            .and_then(|()| candidate.validate())
            .map_err(|error| {
                let field_name = match &error {
                    SandboxError::Field { field, .. } => *field,
                    SandboxError::Dangling { kind, .. } => match kind {
                        RecordKind::Provider => "provider",
                        RecordKind::Network => "network",
                        RecordKind::Transfer => "transfer",
                        RecordKind::Profile => "name",
                    },
                    _ => "name",
                };
                let message = error.to_string();
                self.set_error(field_name, &message);
                message
            })?;
        Ok(candidate)
    }

    pub fn set_error(&mut self, key: &str, message: &str) {
        let key = if self.kind == RecordKind::Profile && matches!(key, "enforcement" | "tls_mode") {
            "network"
        } else {
            key
        };
        if let Some(field) = self.fields.iter_mut().find(|field| field.key == key) {
            field.error = Some(message.into());
        }
    }

    pub fn focus_error(&mut self) {
        if let Some(index) = self.fields.iter().position(|field| field.error.is_some()) {
            self.focus = index;
            self.reveal_focus = true;
        }
    }
}

pub(super) fn parse_domains(text: &str) -> Result<Vec<String>, String> {
    parse_rules(text, "Domain", |line| {
        DomainRule::parse(line).map(|rule| rule.as_str().to_owned())
    })
}

pub(super) fn parse_cidrs(text: &str) -> Result<Vec<String>, String> {
    parse_rules(text, "CIDR", |line| {
        CidrRule::parse(line).map(|rule| rule.as_str().to_owned())
    })
}

fn parse_rules(
    text: &str,
    label: &str,
    parse: impl Fn(&str) -> Result<String, SandboxError>,
) -> Result<Vec<String>, String> {
    let mut rules = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if rules.len() == MAX_NETWORK_RULES {
            return Err(format!("{label} list exceeds {MAX_NETWORK_RULES} rules."));
        }
        rules.push(parse(line).map_err(|error| format!("{label} line {}: {error}", index + 1))?);
    }
    Ok(rules)
}

pub(super) fn network_list_key(editor: &mut TextEditor, event: KeyEvent, limit: usize) -> bool {
    if event.modifiers != KeyModifiers::ALT {
        return false;
    }
    match event.code {
        KeyCode::Insert => {
            editor.move_to_end();
            if !editor.text().is_empty() && !editor.text().ends_with('\n') {
                editor.handle_paste_bounded("\n", limit);
            }
        }
        KeyCode::Delete => {
            editor.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
            editor.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT));
            editor.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        _ => return false,
    }
    true
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(values) => values.iter().map(value_text).collect::<Vec<_>>().join("\n"),
        _ => value.to_string(),
    }
}

fn default_text(key: &str) -> String {
    match key {
        "kind" => "e2b-libvirt",
        "on_exit" => "detach",
        "enforcement" => "required",
        "tls_mode" => "sni-only",
        "initial_seed" => "ask",
        "cpus" => "1",
        "memory_mib" => "1024",
        "disk_gib" => "10",
        "running_ttl_seconds" => "3600",
        "cwd" => ".",
        "persistent" | "respect_gitignore" => "true",
        "delete_extraneous" => "false",
        "exclude" => return TransferPolicy::default().exclude.join("\n"),
        _ => "",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::{MAX_NETWORK_RULES, TextEditor, network_list_key, parse_cidrs, parse_domains};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    const DOMAIN: &str = "api.example.com";
    const LINE_ERROR: &str = "Domain line 3:";

    #[test_case(0, false; "at_bound")]
    #[test_case(1, true; "room_for_newline")]
    fn network_append_respects_bound(extra: usize, appended: bool) {
        let mut editor = TextEditor::new();
        editor.set_text(DOMAIN.into());
        assert!(network_list_key(
            &mut editor,
            KeyEvent::new(KeyCode::Insert, KeyModifiers::ALT),
            DOMAIN.len() + extra
        ));
        assert_eq!(
            editor.text(),
            if appended {
                format!("{DOMAIN}\n")
            } else {
                DOMAIN.into()
            }
        );
    }

    #[test_case(" API.Example.com \n\n *.Example.com \n", vec!["api.example.com", "*.example.com"]; "normalize_and_skip_blank_lines")]
    #[test_case(" \n\t\n", vec![]; "empty_is_deny_all")]
    fn domain_list_parsing(text: &str, expected: Vec<&str>) {
        assert_eq!(parse_domains(text).unwrap(), expected);
    }

    #[test_case("https://api.example.com"; "url")]
    #[test_case("api.example.com:443"; "port")]
    #[test_case("api.example.com/path"; "path")]
    #[test_case("127.0.0.1"; "ip")]
    #[test_case("*"; "all_hosts")]
    fn invalid_domain_reports_source_line(invalid: &str) {
        let error = parse_domains(&format!("{DOMAIN}\n\n{invalid}")).unwrap_err();
        assert!(error.starts_with(LINE_ERROR), "{error}");
    }

    #[test_case(MAX_NETWORK_RULES, true; "at_bound")]
    #[test_case(MAX_NETWORK_RULES + 1, false; "over_bound")]
    fn domain_list_bound(count: usize, valid: bool) {
        assert_eq!(
            parse_domains(&vec![DOMAIN; count].join("\n")).is_ok(),
            valid
        );
    }

    #[test_case(" 10.20.30.40/24 \n\n", vec!["10.20.30.0/24"]; "normalize_cidr")]
    #[test_case("\n", vec![]; "empty_cidrs")]
    fn cidr_list_parsing(text: &str, expected: Vec<&str>) {
        assert_eq!(parse_cidrs(text).unwrap(), expected);
    }

    #[test_case(KeyCode::Insert, "api.example.com\n"; "append")]
    #[test_case(KeyCode::Delete, ""; "clear")]
    fn rule_list_shortcut_is_undoable(code: KeyCode, expected: &str) {
        let mut editor = TextEditor::new();
        editor.set_text(DOMAIN.into());
        assert!(network_list_key(
            &mut editor,
            KeyEvent::new(code, KeyModifiers::ALT),
            usize::MAX,
        ));
        assert_eq!(editor.text(), expected);
        editor.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), DOMAIN);
    }
}
