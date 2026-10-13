use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use caudra_agent::mcp::config::McpConfigSource;
use caudra_agent::mcp::oauth::REFUSED_ACCESS;
use caudra_agent::{
    McpConfigErrors, McpServerInfo, McpServerKind, McpServerStatus, McpSnapshot, McpSnapshotReader,
};
use caudra_grab::grab_scope;

use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::tooltip::Tip;
use crate::components::{Hint, Overlay, escape_terminal_controls};
use crate::repaint::{Cadence, Dirty, Watch};

const TITLE: &str = " MCP Servers ";
const DETAIL_SEPARATOR: &str = " \u{00b7} ";
const PROTOCOL_PREFIX: &str = "MCP ";
const USES_PREFIX: &str = "Uses: ";
const USES_SEPARATOR: &str = ", ";
const USES_RESOURCES: &str = "resources";
const USES_COMPLETIONS: &str = "completions";
const USES_LIST_CHANGES: &str = "list changes";
const INSTRUCTIONS_PREFIX: &str = "Instructions: ";
/// Lines of a server's instructions the picker shows under it.
const INSTRUCTIONS_PREVIEW_LINES: usize = 3;
const NATIVE_ELICITATION_QUERY: &str = "elicitation_mode=native";
const REFUSED_DETAIL: &str = "refused access \u{00b7} details below";
const REFUSED_HEADING: &str = "Refused: ";
const SWITCH_ACCOUNT_PREFIX: &str = "Run 'caudra mcp auth ";
const SWITCH_ACCOUNT_SUFFIX: &str = "' to sign in with another account.";
const EXECUTOR_NATIVE_HINT: &str = "Executor asks the model to approve its runs. Add ?elicitation_mode=native to the URL so Caudra asks you.";

fn build_entries(infos: &[McpServerInfo]) -> (Vec<McpEntry>, Vec<bool>, Vec<bool>) {
    let entries = infos
        .iter()
        .map(|info| {
            let info_text = match info.status {
                McpServerStatus::AwaitingTrust => Some(review_text(info)),
                McpServerStatus::Running => running_text(info),
                McpServerStatus::Refused => Some(format!(
                    "{REFUSED_HEADING}{REFUSED_ACCESS}.\n{SWITCH_ACCOUNT_PREFIX}{}{SWITCH_ACCOUNT_SUFFIX}",
                    info.name
                )),
                _ => None,
            };
            McpEntry {
                name: info.name.clone(),
                status: info.status.clone(),
                info_text,
                detail_text: match &info.status {
                    McpServerStatus::Connecting => {
                        format!("{} \u{00b7} connecting\u{2026}", info.transport_kind)
                    }
                    McpServerStatus::Running => {
                        let mut parts: Vec<String> = info
                            .kind
                            .map(|kind| kind.label().to_owned())
                            .into_iter()
                            .collect();
                        parts.push(info.transport_kind.to_string());
                        if info.tool_count > 0 {
                            parts.push(format!("{} tools", info.tool_count));
                        }
                        if info.prompt_count > 0 {
                            parts.push(format!("{} prompts", info.prompt_count));
                        }
                        if info.tool_count == 0 && info.prompt_count == 0 {
                            parts.push("no capabilities".into());
                        }
                        parts.join(" \u{00b7} ")
                    }
                    McpServerStatus::AwaitingTrust => {
                        format!(
                            "{} \u{00b7} awaiting trust \u{00b7} review below",
                            info.transport_kind
                        )
                    }
                    McpServerStatus::Disabled => {
                        format!("{} \u{00b7} disabled", info.transport_kind)
                    }
                    McpServerStatus::Failed(e) => {
                        format!("{} \u{00b7} error: {}", info.transport_kind, e)
                    }
                    McpServerStatus::NeedsAuth { .. } => {
                        format!(
                            "{} \u{00b7} needs auth \u{00b7} run 'caudra mcp auth {}'",
                            info.transport_kind, info.name
                        )
                    }
                    McpServerStatus::Refused => {
                        format!("{}{DETAIL_SEPARATOR}{REFUSED_DETAIL}", info.transport_kind)
                    }
                },
            }
        })
        .collect();
    let enabled = infos.iter().map(|info| info.status.is_active()).collect();
    let toggleable = infos
        .iter()
        .map(|info| info.status != McpServerStatus::AwaitingTrust)
        .collect();
    (entries, enabled, toggleable)
}

pub enum McpPickerAction {
    Consumed,
    Toggle { server_name: String, enabled: bool },
    TrustOnce { server_name: String },
    TrustProject { server_name: String },
    Reject { server_name: String },
    Close,
    Copy(String),
}

struct McpEntry {
    name: String,
    status: McpServerStatus,
    detail_text: String,
    info_text: Option<String>,
}

enum PendingConfirmation {
    TrustProject(String),
    Reject(String),
}

impl PickerItem for McpEntry {
    fn label(&self) -> &str {
        &self.name
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.detail_text)
    }

    fn info(&self) -> Option<&str> {
        self.info_text.as_deref()
    }
}

pub struct McpPicker {
    picker: ListPicker<McpEntry>,
    reader: McpSnapshotReader,
    servers: Watch<McpSnapshot>,
    config_errors: McpConfigErrors,
    pending_confirmation: Option<PendingConfirmation>,
}

impl McpPicker {
    pub fn new(reader: McpSnapshotReader, config_errors: McpConfigErrors) -> Self {
        let picker = ListPicker::new().with_footer_builder(footer);
        Self {
            picker,
            reader,
            servers: Watch::default(),
            config_errors,
            pending_confirmation: None,
        }
    }

    pub fn open(&mut self) {
        self.pending_confirmation = None;
        let _ = self.servers.poll(self.reader.load_full());
        let (entries, enabled, toggleable) = self.entries();
        self.show_config_errors();
        self.picker
            .open_selectively_toggleable(entries, enabled, toggleable, TITLE);
        self.sync_review();
    }

    /// Adopts a restarted manager's reader. An open picker lists its servers
    /// at the next [`Self::refresh`], which sees a snapshot it has not drawn.
    pub(crate) fn set_reader(&mut self, reader: McpSnapshotReader) {
        self.reader = reader;
    }

    /// Replaces what the banner reports, at once if the picker is open.
    pub(crate) fn set_config_errors(&mut self, errors: McpConfigErrors) {
        self.config_errors = errors;
        self.show_config_errors();
    }

    fn show_config_errors(&mut self) {
        let errors = (!self.config_errors.is_empty()).then(|| self.config_errors.to_string());
        self.picker.set_error_text(errors);
    }

    pub fn refresh(&mut self) -> Dirty {
        if !self.picker.is_open() {
            return Dirty::NO;
        }
        if self.servers.poll(self.reader.load_full()) == Dirty::NO {
            return Dirty::NO;
        }
        let (entries, enabled, toggleable) = self.entries();
        self.picker
            .replace_selectively_toggleable(entries, enabled, toggleable);
        self.sync_review();
        Dirty::YES
    }

    fn entries(&self) -> (Vec<McpEntry>, Vec<bool>, Vec<bool>) {
        self.servers
            .get()
            .map(|s| build_entries(&s.infos))
            .unwrap_or_default()
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub(crate) fn has_awaiting_trust(&self) -> bool {
        self.reader
            .load()
            .infos
            .iter()
            .any(|info| info.status == McpServerStatus::AwaitingTrust)
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.handle_text(text, true)
    }

    pub(crate) fn text_key(&self, key: Option<KeyEvent>) -> bool {
        self.pending_confirmation.is_none()
            && !(key.is_some_and(|key| {
                key.modifiers.is_empty() && matches!(key.code, KeyCode::Char('o' | 'p' | 'r'))
            }) && self
                .picker
                .selected_item()
                .is_some_and(|entry| entry.status == McpServerStatus::AwaitingTrust))
    }

    pub fn handle_text(&mut self, text: &str, paste: bool) -> bool {
        self.picker.handle_text(text, paste)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> McpPickerAction {
        if let Some(pending) = self.pending_confirmation.take() {
            return match key.code {
                KeyCode::Enter | KeyCode::Char('y') => match pending {
                    PendingConfirmation::TrustProject(server_name) => {
                        McpPickerAction::TrustProject { server_name }
                    }
                    PendingConfirmation::Reject(server_name) => {
                        McpPickerAction::Reject { server_name }
                    }
                },
                KeyCode::Esc => {
                    self.sync_review();
                    McpPickerAction::Consumed
                }
                _ => {
                    self.pending_confirmation = Some(pending);
                    McpPickerAction::Consumed
                }
            };
        }
        if key.modifiers.is_empty()
            && let Some(entry) = self.picker.selected_item()
            && entry.status == McpServerStatus::AwaitingTrust
        {
            let server_name = entry.name.clone();
            return match key.code {
                KeyCode::Char('o') => McpPickerAction::TrustOnce { server_name },
                KeyCode::Char('p') => {
                    self.pending_confirmation =
                        Some(PendingConfirmation::TrustProject(server_name));
                    self.picker.set_info_text(Some(
                        "Trust this exact server configuration for the project? Press Enter/y to confirm or Esc to cancel.".into(),
                    ));
                    McpPickerAction::Consumed
                }
                KeyCode::Char('r') => {
                    self.pending_confirmation = Some(PendingConfirmation::Reject(server_name));
                    self.picker.set_info_text(Some(
                        "Reject and disable this project server? Press Enter/y to confirm or Esc to cancel.".into(),
                    ));
                    McpPickerAction::Consumed
                }
                KeyCode::Enter => McpPickerAction::Consumed,
                _ => {
                    let action = self.picker.handle_key(key);
                    self.sync_review();
                    self.map_picker_action(action)
                }
            };
        }
        let action = self.picker.handle_key(key);
        self.sync_review();
        self.map_picker_action(action)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> McpPickerAction {
        if self.pending_confirmation.is_some() {
            return McpPickerAction::Consumed;
        }
        let action = self.picker.handle_mouse(event);
        if let PickerAction::Key(key) = action {
            return self.handle_key(key);
        }
        self.sync_review();
        self.map_picker_action(action)
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    /// A confirmation owns the whole picker until it is answered, list
    /// included, so the wheel must not move what it is asking about.
    pub fn scroll(&mut self, delta: i32) {
        if self.pending_confirmation.is_some() {
            return;
        }
        self.picker.scroll(delta);
    }

    fn map_picker_action(&self, action: PickerAction<McpEntry>) -> McpPickerAction {
        match action {
            PickerAction::Consumed => McpPickerAction::Consumed,
            PickerAction::Toggle(idx, enabled) => {
                let server_name = self
                    .picker
                    .item(idx)
                    .expect("toggle idx valid")
                    .name
                    .clone();
                McpPickerAction::Toggle {
                    server_name,
                    enabled,
                }
            }
            PickerAction::Select(..) | PickerAction::Close => McpPickerAction::Close,
            PickerAction::Key(_) => McpPickerAction::Consumed,
            PickerAction::Copy(text) => McpPickerAction::Copy(text),
        }
    }

    fn sync_review(&mut self) {
        if let Some(message) = self.confirmation_text() {
            self.picker.set_info_text(Some(message));
            return;
        }
        let review = self
            .picker
            .selected_item()
            .and_then(|entry| entry.info_text.clone());
        self.picker.set_info_text(review);
    }

    fn confirmation_text(&self) -> Option<String> {
        self.pending_confirmation.as_ref().map(|pending| match pending {
            PendingConfirmation::TrustProject(_) =>
                "Trust this exact server configuration for the project? Press Enter/y to confirm or Esc to cancel.".into(),
            PendingConfirmation::Reject(_) =>
                "Reject and disable this project server? Press Enter/y to confirm or Esc to cancel.".into(),
        })
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("mcp_picker", area);
        self.picker.view(frame, area)
    }
}

fn footer() -> Vec<Hint> {
    vec![
        Hint::char("o", "Trust for process"),
        Hint::char("p", "Trust exact config for project"),
        Hint::char("r", "Reject and disable"),
        Hint::bind(key::ENTER, "Toggle other rows"),
        Hint::bind(key::ESC, "Close"),
    ]
}

fn review_text(info: &McpServerInfo) -> String {
    let review = &info.review;
    let target = review
        .command
        .as_ref()
        .map(|command| format!("command: {}", safe_command(command)))
        .or_else(|| {
            review
                .url
                .as_deref()
                .map(|url| format!("URL: {}", safe_url(url)))
        })
        .unwrap_or_else(|| "target: unavailable".into());
    let source = source_name(review.config_source);
    let config_path = escape_terminal_controls(&info.config_path.to_string_lossy());
    let environment = name_list("env", &review.environment_names);
    let headers = name_list("headers", &review.header_names);
    format!(
        "Awaiting trust: no connection has been attempted.\nTarget {target}\nConfig: {source} · {config_path}\n{environment}\n{headers}"
    )
}

/// What a running server said about itself: who it is, the protocol, the
/// features it offers, the start of its instructions, and for Executor in
/// model mode, how to have it ask the user instead.
fn running_text(info: &McpServerInfo) -> Option<String> {
    let discovery = info.discovery.as_ref()?;
    let server = &discovery.server_info;
    let name = server.title.as_deref().unwrap_or(&server.name);
    let mut identity: Vec<&str> = [name, server.version.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect();
    let protocol = format!("{PROTOCOL_PREFIX}{}", discovery.protocol_version);
    if !discovery.protocol_version.is_empty() {
        identity.push(&protocol);
    }
    let mut lines = vec![escape_terminal_controls(&identity.join(DETAIL_SEPARATOR))];

    let capabilities = &discovery.capabilities;
    let lists_change = [
        &capabilities.tools,
        &capabilities.prompts,
        &capabilities.resources,
    ]
    .into_iter()
    .any(|list| list.as_ref().is_some_and(|list| list.list_changed));
    let uses: Vec<&str> = [
        (capabilities.resources.is_some(), USES_RESOURCES),
        (capabilities.completions, USES_COMPLETIONS),
        (lists_change, USES_LIST_CHANGES),
    ]
    .into_iter()
    .filter_map(|(offered, feature)| offered.then_some(feature))
    .collect();
    if !uses.is_empty() {
        lines.push(format!("{USES_PREFIX}{}", uses.join(USES_SEPARATOR)));
    }

    if let Some(instructions) = discovery.instructions.as_deref() {
        let preview: Vec<String> = instructions
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(INSTRUCTIONS_PREVIEW_LINES)
            .map(escape_terminal_controls)
            .collect();
        if !preview.is_empty() {
            lines.push(format!("{INSTRUCTIONS_PREFIX}{}", preview.join("\n")));
        }
    }

    let model_mode = info
        .url
        .as_deref()
        .is_some_and(|url| !url.contains(NATIVE_ELICITATION_QUERY));
    if info.kind == Some(McpServerKind::Executor) && model_mode {
        lines.push(EXECUTOR_NATIVE_HINT.into());
    }
    Some(lines.join("\n"))
}

fn source_name(source: McpConfigSource) -> &'static str {
    match source {
        McpConfigSource::Global => "global",
        McpConfigSource::Project => "project",
        McpConfigSource::Runtime => "runtime",
    }
}

fn name_list(label: &str, names: &[String]) -> String {
    if names.is_empty() {
        return format!("{label}: none");
    }
    format!(
        "{label}: {}",
        names
            .iter()
            .map(|name| escape_terminal_controls(name))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn safe_command(command: &[String]) -> String {
    let mut redact_next = false;
    let safe: Vec<String> = command
        .iter()
        .map(|argument| {
            if redact_next {
                redact_next = false;
                return "<redacted:string>".into();
            }
            if let Some((flag, _)) = argument.split_once('=')
                && likely_secret_flag(flag)
            {
                return format!("{flag}=<redacted:string>");
            }
            redact_next = likely_secret_flag(argument);
            escape_terminal_controls(argument)
        })
        .collect();
    serde_json::to_string(&safe).unwrap_or_else(|_| "[]".into())
}

fn likely_secret_flag(argument: &str) -> bool {
    let normalized: String = argument
        .trim_start_matches('-')
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    ["password", "secret", "token", "apikey", "authorization"]
        .iter()
        .any(|marker| normalized.contains(marker))
}

fn safe_url(url: &str) -> String {
    let escaped = escape_terminal_controls(url);
    let without_fragment = escaped
        .split_once('#')
        .map_or(escaped.as_str(), |(base, _)| base);
    let (base, query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, None), |(base, query)| {
            (base, Some(query))
        });
    let base = if let Some(scheme_end) = base.find("://") {
        let authority_start = scheme_end + 3;
        if let Some(at) = base[authority_start..].find('@') {
            format!(
                "{}<redacted>@{}",
                &base[..authority_start],
                &base[authority_start + at + 1..]
            )
        } else {
            base.to_string()
        }
    } else {
        base.to_string()
    };
    let Some(query) = query else {
        return base;
    };
    let names = query
        .split('&')
        .map(|part| part.split_once('=').map_or(part, |(name, _)| name))
        .map(|name| format!("{name}=<redacted>"))
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{names}")
}

impl Overlay for McpPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.pending_confirmation = None;
        self.picker.close()
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }

    fn tooltip(&self) -> Option<Tip> {
        self.picker.tooltip()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::buffer_text;
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use caudra_agent::mcp::config::{McpReviewSummary, load_config};
    use caudra_agent::mcp::protocol::{
        ListCapability, ServerCapabilities, ServerDiscovery, ServerInfo,
    };
    use caudra_agent::{McpServerInfo, McpSnapshot};
    use caudra_config::PROJECT_DIR;
    use caudra_config::mcp::MCP_FILE;
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::fs;
    use std::path::PathBuf;
    use test_case::test_case;

    fn test_snapshot() -> McpSnapshotReader {
        McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![
                McpServerInfo {
                    name: "fs".into(),
                    transport_kind: "stdio",
                    tool_count: 5,
                    prompt_count: 0,
                    status: McpServerStatus::Running,
                    config_path: PathBuf::from("/home/.config/caudra/config.toml"),
                    url: None,
                    oauth: None,
                    resolved_addresses: Vec::new(),
                    review: McpReviewSummary {
                        command: Some(vec!["filesystem".into()]),
                        url: None,
                        config_source: McpConfigSource::Global,
                        environment_names: vec![],
                        header_names: vec![],
                    },
                    kind: None,
                    discovery: None,
                },
                McpServerInfo {
                    name: "github".into(),
                    transport_kind: "stdio",
                    tool_count: 3,
                    prompt_count: 0,
                    status: McpServerStatus::Disabled,
                    config_path: PathBuf::from("/project/.caudra/config.toml"),
                    url: None,
                    oauth: None,
                    resolved_addresses: Vec::new(),
                    review: McpReviewSummary {
                        command: Some(vec!["github".into()]),
                        url: None,
                        config_source: McpConfigSource::Project,
                        environment_names: vec![],
                        header_names: vec![],
                    },
                    kind: None,
                    discovery: None,
                },
            ],
            prompts: vec![],
            resources: Vec::new(),
            pids: vec![],
            generation: 0,
        })
    }

    #[test]
    fn toggle_returns_server_name_and_new_state() {
        let mut p = McpPicker::new(test_snapshot(), McpConfigErrors::new(PathBuf::new()));
        p.open();
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            action,
            McpPickerAction::Toggle { ref server_name, enabled: false } if server_name == "fs"
        ));
    }

    #[test_case(key(KeyCode::Esc)       ; "esc_closes")]
    #[test_case(kb::QUIT.to_key_event() ; "ctrl_c_closes")]
    fn close_keys(cancel_key: KeyEvent) {
        let mut p = McpPicker::new(test_snapshot(), McpConfigErrors::new(PathBuf::new()));
        p.open();
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, McpPickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn open_with_empty_infos() {
        let mut p = McpPicker::new(
            McpSnapshotReader::empty(),
            McpConfigErrors::new(PathBuf::new()),
        );
        p.open();
        assert!(p.is_open());
    }

    fn awaiting_trust_snapshot() -> McpSnapshotReader {
        McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![McpServerInfo {
                name: "project-server".into(),
                transport_kind: "stdio",
                tool_count: 0,
                prompt_count: 0,
                status: McpServerStatus::AwaitingTrust,
                config_path: PathBuf::from("/project/.caudra/config.toml"),
                url: None,
                oauth: None,
                resolved_addresses: Vec::new(),
                review: McpReviewSummary {
                    command: Some(vec![
                        "node".into(),
                        "server.js\u{1b}[2J".into(),
                        "--token".into(),
                        "secret-value".into(),
                    ]),
                    url: None,
                    config_source: McpConfigSource::Project,
                    environment_names: vec!["GITHUB_TOKEN".into()],
                    header_names: vec!["Authorization".into()],
                },
                kind: None,
                discovery: None,
            }],
            prompts: vec![],
            resources: Vec::new(),
            pids: vec![],
            generation: 0,
        })
    }

    #[test]
    fn awaiting_trust_enter_does_not_toggle() {
        let mut picker = McpPicker::new(
            awaiting_trust_snapshot(),
            McpConfigErrors::new(PathBuf::new()),
        );
        assert!(picker.has_awaiting_trust());
        picker.open();
        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            McpPickerAction::Consumed
        ));
    }

    #[test]
    fn text_without_a_command_filters_but_does_not_edit_a_trust_confirmation() {
        let mut picker = McpPicker::new(
            awaiting_trust_snapshot(),
            McpConfigErrors::new(PathBuf::new()),
        );
        picker.open();
        assert!(picker.text_key(None));
        assert!(!picker.text_key(Some(key(KeyCode::Char('o')))));
        picker.handle_text("project", false);
        assert_eq!(picker.picker.search_text(), "project");
        picker.handle_key(key(KeyCode::Char('p')));
        assert!(!picker.text_key(None));
        assert!(picker.pending_confirmation.is_some());
    }

    #[test_case('p', "project" ; "trust_project")]
    #[test_case('r', "reject" ; "reject")]
    fn persistent_trust_actions_require_confirmation(shortcut: char, expected: &str) {
        let mut picker = McpPicker::new(
            awaiting_trust_snapshot(),
            McpConfigErrors::new(PathBuf::new()),
        );
        picker.open();
        assert!(matches!(
            picker.handle_key(key(KeyCode::Char(shortcut))),
            McpPickerAction::Consumed
        ));
        let action = picker.handle_key(key(KeyCode::Enter));
        let (server_name, kind) = match action {
            McpPickerAction::TrustOnce { server_name } => (server_name, "once"),
            McpPickerAction::TrustProject { server_name } => (server_name, "project"),
            McpPickerAction::Reject { server_name } => (server_name, "reject"),
            _ => panic!("unexpected action"),
        };
        assert_eq!(server_name, "project-server");
        assert_eq!(kind, expected);
    }

    #[test]
    fn trust_once_is_scoped_to_the_process_and_immediate() {
        let mut picker = McpPicker::new(
            awaiting_trust_snapshot(),
            McpConfigErrors::new(PathBuf::new()),
        );
        picker.open();
        assert!(matches!(
            picker.handle_key(key(KeyCode::Char('o'))),
            McpPickerAction::TrustOnce { server_name } if server_name == "project-server"
        ));
    }

    #[test]
    fn awaiting_trust_review_is_safe_and_complete() {
        let snapshot = awaiting_trust_snapshot();
        let (entries, _, _) = build_entries(&snapshot.load().infos);
        let detail = entries[0].info_text.as_deref().unwrap();
        assert!(detail.contains("command:"));
        assert!(detail.contains("Config: project · /project/.caudra/config.toml"));
        assert!(detail.contains("env: GITHUB_TOKEN"));
        assert!(detail.contains("headers: Authorization"));
        assert!(detail.contains("<redacted:string>"));
        assert!(!detail.contains("secret-value"));
        assert!(!detail.contains('\u{1b}'));
        assert!(detail.contains("\\u{1b}"));
    }

    #[test]
    fn awaiting_trust_review_metadata_is_rendered() {
        let mut picker = McpPicker::new(
            awaiting_trust_snapshot(),
            McpConfigErrors::new(PathBuf::new()),
        );
        picker.open();
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let screen = buffer_text(terminal.backend().buffer());
        assert!(screen.contains("no connection has been attempted"));
        assert!(screen.contains("Config: project"));
        assert!(screen.contains("GITHUB_TOKEN"));
        assert!(screen.contains("Authorization"));
        assert!(screen.contains("Trust for process"));
    }

    const BROKEN_MCP_TOML: &str = "[mcp.broken\n";

    fn no_errors() -> McpConfigErrors {
        McpConfigErrors::new(PathBuf::new())
    }

    fn broken_config_errors() -> McpConfigErrors {
        let project = tempfile::tempdir().unwrap();
        let config_dir = project.path().join(PROJECT_DIR);
        fs::create_dir(&config_dir).unwrap();
        fs::write(config_dir.join(MCP_FILE), BROKEN_MCP_TOML).unwrap();
        let (_, errors) = load_config(project.path());
        assert!(!errors.is_empty());
        errors
    }

    #[test]
    fn replacing_config_errors_updates_an_open_banner_at_once() {
        let errors = broken_config_errors();
        let expected = errors.to_string();
        let mut picker = McpPicker::new(McpSnapshotReader::empty(), no_errors());
        picker.open();

        picker.set_config_errors(errors);
        assert_eq!(picker.picker.error_text(), Some(expected.as_str()));

        picker.set_config_errors(no_errors());
        assert_eq!(picker.picker.error_text(), None);
    }

    #[test]
    fn an_adopted_reader_reaches_an_open_picker_at_the_next_refresh() {
        let mut picker = McpPicker::new(McpSnapshotReader::empty(), no_errors());
        picker.open();

        picker.set_reader(awaiting_trust_snapshot());

        assert_eq!(picker.refresh(), Dirty::YES);
        assert!(picker.has_awaiting_trust());
    }

    const SERVER_NAME: &str = "exec";
    const EXECUTOR_URL: &str = "https://mcp.executor.sh/mcp";
    const EXECUTOR_NATIVE_URL: &str = "https://mcp.executor.sh/mcp?elicitation_mode=native";
    const SERVER_TITLE: &str = "Executor";
    const SERVER_VERSION: &str = "1.4.2";
    const PROTOCOL_VERSION: &str = "2025-11-25";
    const INSTRUCTIONS: &str = "Search first.\n\nThen execute.\nThen resume.\nNever more.";

    fn running(kind: Option<McpServerKind>, url: &str) -> McpServerInfo {
        McpServerInfo {
            name: SERVER_NAME.into(),
            transport_kind: "http",
            tool_count: 4,
            prompt_count: 0,
            status: McpServerStatus::Running,
            config_path: PathBuf::from("/home/.config/caudra/mcp.toml"),
            url: Some(url.into()),
            oauth: None,
            resolved_addresses: Vec::new(),
            review: McpReviewSummary {
                command: None,
                url: Some(url.into()),
                config_source: McpConfigSource::Global,
                environment_names: vec![],
                header_names: vec![],
            },
            kind,
            discovery: Some(ServerDiscovery {
                protocol_version: PROTOCOL_VERSION.into(),
                server_info: ServerInfo {
                    name: "executor".into(),
                    title: Some(SERVER_TITLE.into()),
                    version: SERVER_VERSION.into(),
                },
                capabilities: ServerCapabilities {
                    tools: Some(ListCapability { list_changed: true }),
                    completions: true,
                    ..ServerCapabilities::default()
                },
                instructions: Some(INSTRUCTIONS.into()),
                ..ServerDiscovery::default()
            }),
        }
    }

    #[test]
    fn a_running_server_shows_what_it_said_about_itself() {
        let (entries, _, _) =
            build_entries(&[running(Some(McpServerKind::Executor), EXECUTOR_URL)]);

        assert!(
            entries[0]
                .detail_text
                .starts_with(McpServerKind::Executor.label())
        );
        let info = entries[0].info_text.as_deref().unwrap();
        let identity = format!(
            "{SERVER_TITLE}{DETAIL_SEPARATOR}{SERVER_VERSION}{DETAIL_SEPARATOR}{PROTOCOL_PREFIX}{PROTOCOL_VERSION}"
        );
        assert!(info.starts_with(&identity), "{info}");
        assert!(info.contains(&format!(
            "{USES_PREFIX}{USES_COMPLETIONS}{USES_SEPARATOR}{USES_LIST_CHANGES}"
        )));
        assert!(
            info.contains("Search first.\nThen execute.\nThen resume."),
            "{info}"
        );
        assert!(!info.contains("Never more."));
    }

    #[test_case(Some(McpServerKind::Executor), EXECUTOR_URL, true ; "executor_in_model_mode")]
    #[test_case(Some(McpServerKind::Executor), EXECUTOR_NATIVE_URL, false ; "executor_in_native_mode")]
    #[test_case(None, EXECUTOR_URL, false ; "unrecognised_server")]
    fn executor_in_model_mode_is_told_how_to_ask_the_user(
        kind: Option<McpServerKind>,
        url: &str,
        hinted: bool,
    ) {
        let (entries, _, _) = build_entries(&[running(kind, url)]);

        let info = entries[0].info_text.as_deref().unwrap();
        assert_eq!(info.contains(EXECUTOR_NATIVE_HINT), hinted);
    }

    const STEADY_PICKER_MSG: &str =
        "moving the selection must keep the popup and its rows where they were";
    const QUIET_SERVER: &str = "quiet";

    fn drawn(
        picker: &mut McpPicker,
        terminal: &mut Terminal<TestBackend>,
    ) -> (Rect, Option<usize>) {
        let mut popup = Rect::default();
        terminal
            .draw(|frame| popup = picker.view(frame, frame.area()))
            .unwrap();
        let row = buffer_text(terminal.backend().buffer())
            .lines()
            .position(|line| line.contains(QUIET_SERVER));
        (popup, row)
    }

    #[test]
    fn moving_between_servers_with_unequal_info_keeps_the_picker_still() {
        let quiet = McpServerInfo {
            name: QUIET_SERVER.into(),
            status: McpServerStatus::Disabled,
            discovery: None,
            ..running(None, EXECUTOR_URL)
        };
        let reader = McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![running(Some(McpServerKind::Executor), EXECUTOR_URL), quiet],
            ..McpSnapshot::default()
        });
        let mut picker = McpPicker::new(reader, no_errors());
        picker.open();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let first = drawn(&mut picker, &mut terminal);

        picker.handle_key(key(KeyCode::Down));
        let second = drawn(&mut picker, &mut terminal);

        assert!(first.1.is_some(), "{STEADY_PICKER_MSG}");
        assert_eq!(first, second, "{STEADY_PICKER_MSG}");
        assert_eq!(picker.picker.selected_item().unwrap().name, QUIET_SERVER);
    }

    #[test]
    fn a_refused_server_says_why_and_how_to_switch_accounts() {
        let refused = McpServerInfo {
            status: McpServerStatus::Refused,
            discovery: None,
            ..running(Some(McpServerKind::HomeAssistant), EXECUTOR_URL)
        };

        let (entries, enabled, toggleable) = build_entries(&[refused]);

        assert!(entries[0].detail_text.ends_with(REFUSED_DETAIL));
        let info = entries[0].info_text.as_deref().unwrap();
        assert!(info.starts_with(&format!("{REFUSED_HEADING}{REFUSED_ACCESS}")));
        assert!(info.contains(&format!(
            "{SWITCH_ACCOUNT_PREFIX}{SERVER_NAME}{SWITCH_ACCOUNT_SUFFIX}"
        )));
        assert_eq!((enabled[0], toggleable[0]), (false, true));
    }
}
