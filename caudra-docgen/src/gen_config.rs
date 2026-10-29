use std::fmt::Write;

use caudra_agent::agent::{Surface, SurfaceBaseline};
use caudra_agent::mcp::code::catalog;
use caudra_agent::mcp::code::naming::Names;
use caudra_agent::mcp::code::view::{ServerState, ServerView, ToolView};
use caudra_agent::prompt::{
    CATALOG_SLOT, CHANGES_SLOT, CONFIGURATION_CHANGED_PROMPT, MCP_SERVERS_PROMPT,
};
use caudra_config::config_file::{CONFIG_FORMAT, CONFIG_VERSION};
use caudra_config::decisions::{DecisionFeatures, DecisionThresholds, FeatureMode};
use caudra_config::example::Entry;
use caudra_config::files::{self, CONFIG_FILES, Scope};
use caudra_config::mcp::MCP_FORMAT;
use caudra_config::migration::{ConfigFormat, Stamp, migrate};
use caudra_config::providers::PROVIDERS_FORMAT;
use caudra_config::sandbox::SANDBOX_FORMAT;
use caudra_config::shell_env::ShellEnvConfig;
use caudra_config::steering::{SteeringRule, SteeringRulesConfig};
use caudra_config::websearch::{WebsearchBackendName, WebsearchConfig};
use caudra_config::workcell::WORKCELL_FORMAT;
use caudra_config::{
    AgentConfig, AutomationsConfig, AutopilotConfig, ConfigField, ConfigValue,
    DEFAULT_MAX_LOG_FILES, DEFAULT_MAX_OUTPUT_LINES, DEFAULT_MOUSE_SCROLL_LINES, DecisionsConfig,
    Feature, FeatureFlags, MIN_TOOL_OUTPUT_LINES, MessagingConfig, NATIVE_PLUGIN_OPTIONS,
    PERMISSIONS_FORMAT, ProviderConfig, RetentionConfig, SnapshotsConfig, SteeringConfig,
    StorageConfig, TOP_LEVEL_FIELDS, TelemetryConfig, ToolOutputLines, UiConfig, WebConfig,
    WorktreesConfig,
};
use caudra_lua::MANIFEST_FORMAT;
use jiff::civil::{Date, date};
use serde_json::{Value, json};

use crate::gen_providers::join_and;
use crate::gen_reference_configs;
use crate::page_header;

const MCP_REMINDER_SLOT: &str = "{mcp_reminder}";
const PROFILE_CHANGES_SLOT: &str = "{profile_changes}";
const EXAMPLE_MCP_SERVER: &str = "analytics";
const EXAMPLE_MCP_TOOL: &str = "run_query";
const EXAMPLE_MCP_TOOL_DESCRIPTION: &str =
    "Run a read-only SQL query against the warehouse. Results stop at 1000 rows.";
const EXAMPLE_RUNNING_SERVER: &str = "github";
const EXAMPLE_RUNNING_TOOL: &str = "list_issues";
const EXAMPLE_RUNNING_TOOL_DESCRIPTION: &str = "List the open issues of a repository.";
/// The `review` profile of the system prompt docs, before and after the edit
/// the hot reload example makes.
const EXAMPLE_PROFILE: &str = "Prioritize correctness bugs, regressions, and missing tests.\nDo not edit files unless the user explicitly asks.\n";
const EXAMPLE_PROFILE_EDITED: &str = "Prioritize correctness bugs, regressions, and missing tests.\nDo not edit files. Report each finding with its file and line.\n";
/// Each versioned TOML file, with what happens when its version is newer
/// than this build reads.
const VERSIONED_FILES: &[(&ConfigFormat, &str)] = &[
    (&CONFIG_FORMAT, "Caudra stops with an error"),
    (
        &PERMISSIONS_FORMAT,
        "Fails closed. Tool calls are denied until the file is fixed",
    ),
    (
        &MCP_FORMAT,
        "Servers from that file do not start, and Caudra shows the error",
    ),
    (&PROVIDERS_FORMAT, "Caudra stops with an error"),
    (
        &MANIFEST_FORMAT,
        "Every permission of that plugin is denied",
    ),
    (&WORKCELL_FORMAT, "Rejected with an error"),
    (&SANDBOX_FORMAT, "Rejected with an error"),
];
const VERSION_OPTIONAL: &str = "Read as version 1";
const VERSION_REQUIRED: &str = "Rejected";
/// A version 1 `caudra.toml` and the stamp its upgrade notes carry.
const UPGRADE_EXAMPLE: &str = "\
# My settings

[ui.tool_output_lines]
# keep code output short
python_execution = 40

[agent]
defer_builtin_tools = true
";
const UPGRADE_EXAMPLE_BY: &str = "Caudra 0.2.0-preview.12";
const UPGRADE_EXAMPLE_ON: Date = date(2026, 10, 20);

/// Shown bare, since the text is prose rather than a value to copy.
fn default_cell(value: &ConfigValue) -> String {
    match value {
        ConfigValue::Unset | ConfigValue::Varies(_) | ConfigValue::Required(_) => {
            value.format_default()
        }
        _ => format!("`{}`", value.format_default()),
    }
}

fn range_cell(field: &ConfigField) -> String {
    match (field.min, field.max) {
        (Some(min), Some(max)) => format!("{min}–{max}"),
        _ => "-".into(),
    }
}

type ExtraColumn = (&'static str, fn(&Entry) -> String);

fn write_table(out: &mut String, fields: &[ConfigField]) {
    let entries: Vec<Entry> = fields.iter().map(Entry::from).collect();
    write_entries(out, &entries);
}

pub fn write_entries<'a>(out: &mut String, entries: impl IntoIterator<Item = &'a Entry>) {
    let entries: Vec<&Entry> = entries.into_iter().collect();
    let mut extras: Vec<ExtraColumn> = Vec::new();
    if entries.iter().any(|f| f.env.is_some()) {
        extras.push(("Env", |f: &Entry| {
            f.env.map_or("-".to_string(), |e| {
                e.split(", ")
                    .map(|v| format!("`{v}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        }));
    }
    if entries.iter().any(|f| f.min.is_some()) {
        extras.push(("Min", |f: &Entry| {
            f.min.map_or("-".to_string(), |v| v.to_string())
        }));
    }
    if entries.iter().any(|f| f.max.is_some()) {
        extras.push(("Max", |f: &Entry| {
            f.max.map_or("-".to_string(), |v| v.to_string())
        }));
    }

    let header: String = extras
        .iter()
        .map(|(name, _)| format!(" {name} |"))
        .collect();
    let rule: String = extras.iter().map(|_| "-----|").collect();
    writeln!(out, "| Field | Type | Default |{header} Description |").unwrap();
    writeln!(out, "|-------|------|---------|{rule}-------------|").unwrap();
    for f in entries {
        let cells: String = extras
            .iter()
            .map(|(_, cell)| format!(" {} |", cell(f)))
            .collect();
        writeln!(
            out,
            "| `{name}` | {ty} | {default} |{cells} {desc} |",
            name = f.name,
            ty = escape_pipes(f.ty),
            default = default_cell(&f.default),
            desc = f.description,
        )
        .unwrap();
    }
}

fn escape_pipes(ty: &str) -> String {
    ty.replace('|', "\\|")
}

fn write_section(out: &mut String, table: &str, fields: &[ConfigField]) {
    writeln!(out, "### `{table}`\n").unwrap();
    write_table(out, fields);
    writeln!(out).unwrap();
}

fn experiment_scope(feature: Feature) -> &'static str {
    match feature {
        Feature::Workflows => {
            "[Workflows](/docs/workflows/): the `workflow` tool, the workflow commands and \
             inspector, and the `caudra-workflow-dev` skill."
        }
        Feature::Sandboxes => {
            "[Managed sandboxes](/docs/sandboxes/): `caudra sandbox`, `caudra auth sandbox`, \
             `--sandbox`, `/sandbox`, and the workbench Transfer view. Sandboxes bring their own \
             connection to Workcell and do not need `remote_workcell`."
        }
        Feature::RemoteWorkcell => {
            "Direct [remote Workcell](/docs/remote-workspaces/) connections: the `--workcell-*` \
             flags and `caudra auth workcell`."
        }
        Feature::LuaPlugins => {
            "Every use of Lua: [plugins](/docs/plugins/), the [Lua API](/docs/lua-api/), global \
             and project `init.lua`, and the `caudra-plugin-dev` skill. `--no-plugins` still \
             turns Lua off for one run."
        }
        Feature::CrossSessionMessaging => {
            "Local [cross-session messaging](/docs/messaging/): the `list_sessions`, `send_message`, `publish_message`, `read_topic`, and `work_assignment` tools, `/peers`, `/messages`, `/topics`, `/groups`, `caudra message`, live session inboxes, consumer groups, and the message history. Both processes must opt in."
        }
        Feature::Automations => {
            "[Automations](/docs/automations/): scripts that act on session events when their \
             conditions hold, within the limits in [`[automations]`](#automations), and the \
             `caudra-automation-dev` skill. Messaging triggers and actions also need \
             `cross_session_messaging`, and workflow triggers and actions also need `workflows`."
        }
    }
}

fn write_experimental_section(out: &mut String) {
    out.push_str(
        "## Experimental features\n\n\
         Some features are experimental and stay off until you turn them on. Each one has its \
         own switch in the `[experimental]` table of the global `caudra.toml`:\n\n\
         ```toml\n\
         [experimental]\n\
         workflows = true\n\
         automations = true\n\
         ```\n\n\
         | Key | Default | Turns on |\n\
         |-----|---------|----------|\n",
    );
    for feature in Feature::ALL {
        writeln!(
            out,
            "| `{key}` | `{default}` | {scope} |",
            key = feature.key(),
            default = FeatureFlags::default().enabled(feature),
            scope = experiment_scope(feature),
        )
        .unwrap();
    }
    out.push_str(
        "\nEach switch is independent, so turning one on never turns on another. A missing file, \
         table, or key leaves a switch off, and an unknown key is an error. `caudra remote` and \
         `/remote` work when either `sandboxes` or `remote_workcell` is on, and each session \
         checks the switch for its own source.\n\n\
         Only the global file may hold `[experimental]`. Caudra rejects a project \
         `.caudra/caudra.toml` that contains the table, even an empty one, so a repository cannot \
         opt you in. Lua, tool allowlists, and saved sessions cannot turn a feature on either.\n\n\
         Caudra reads the switches once at startup and keeps them until it exits. `/reload`, \
         session switches, and ACP sessions all use the startup values. When the table changes on \
         disk, Caudra shows a notice asking for a restart.\n\n\
         A feature that is off is hidden and does no work. Its tools, commands, shortcuts, help \
         entries, and status chips are gone, and startup skips it. Asking for it directly, such \
         as typing its command or passing its flag, fails with a message that names the switch. \
         A saved session attached to a sandbox or a remote workspace does not resume while its \
         switch is off, and it never falls back to local execution. Turning a feature off keeps \
         its data and leaves external resources alone, so a running sandbox keeps running until \
         you stop it.\n\n",
    );
}

fn write_migration_section(out: &mut String) {
    out.push_str(
        r#"## Migrating from Lua settings

Earlier releases read settings from `init.lua` through `caudra.setup()`. Caudra now runs `init.lua` only when `lua_plugins` is on. With Lua off, Caudra shows one notice that names each `init.lua` it skipped. It does not read, run, or change those files.

To migrate, move the table you passed to `caudra.setup()` into the `caudra.toml` of the same scope. Keys and values stay the same. Top-level values come first, and each nested table becomes a TOML table:

```lua
caudra.setup({
    always_fast = true,
    ui = { theme = "tokyonight" },
    agent = { disabled_tools = { "websearch" } },
})
```

```toml
always_fast = true

[ui]
theme = "tokyonight"

[agent]
disabled_tools = ["websearch"]
```

Lists become arrays, and deeper tables become dotted headers such as `[agent.steering.rules.repetition]`. Quote a key that holds other characters, as in `[agent.steering.models."openai/gpt-5"]`. Leave out any key you set to `nil`.

To keep using `init.lua`, set `lua_plugins = true` under `[experimental]`. Its `caudra.setup()` values then apply on top of the `caudra.toml` of the same scope, in the order shown [above](#configuration).

"#,
    );
}

/// These tables outlived the Lua plugins they were written for: the tool is
/// native now and Rust validates the same keys.
fn native_tool_note(plugin: &str) -> Option<String> {
    match plugin {
        "index" => Some(format!(
            "`file_index` executes as a native Workcell tool, and this table keeps the `plugins.index` key it was configured under. The file-size limit accepts {} through {} MiB to bound parser memory and work.",
            caudra_config::MIN_INDEX_MAX_FILE_SIZE_MB,
            caudra_config::MAX_INDEX_MAX_FILE_SIZE_MB,
        )),
        "skill" | "task" => Some(format!(
            "`{plugin}` executes as a native Caudra tool. This table keeps its existing configuration key."
        )),
        _ => None,
    }
}

fn write_plugin_options(out: &mut String) {
    for (plugin, fields) in NATIVE_PLUGIN_OPTIONS {
        writeln!(out, "### `plugins.{plugin}`\n").unwrap();
        if let Some(note) = native_tool_note(plugin) {
            writeln!(out, "{note}\n").unwrap();
        }
        write_table(out, fields);
        writeln!(out).unwrap();
    }
}

fn write_theme_section(out: &mut String) {
    writeln!(out, "### `ui.theme`\n").unwrap();
    writeln!(
        out,
        "Name of the color theme to load at startup, overriding the theme you \
         last picked interactively. If unset, Caudra keeps your last selection, \
         which starts out as `{}`. An unknown name is ignored with a warning.\n",
        caudra_ui::DEFAULT_THEME
    )
    .unwrap();
    let names = caudra_ui::BUNDLED_THEMES
        .iter()
        .map(|t| format!("`{}`", t.name))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "Available themes: {names}.\n").unwrap();
    writeln!(
        out,
        "You can add your own themes too. Drop a `<name>.toml` file into \
         `themes/` inside your Caudra config directory, for example \
         `~/.config/caudra/themes/`. If it reuses a built-in name, yours wins.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Themes use 24-bit colors, but not every terminal can show them. Caudra \
         checks the environment, terminfo, and the terminal itself, and when \
         truecolor is missing it quietly falls back to the closest of the 256 \
         classic terminal colors. If detection gets it wrong, set \
         `CAUDRA_TRUECOLOR=1` to force truecolor or `CAUDRA_TRUECOLOR=0` to force \
         the fallback.\n"
    )
    .unwrap();

    let pairs = caudra_ui::THEME_PAIRS
        .iter()
        .map(|pair| format!("`{}` and `{}`", pair.dark, pair.light))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(
        out,
        "Some themes ship as a light and dark pair: {pairs}. Choosing either \
         half makes Caudra ask the terminal whether it is in light or dark mode \
         and show the half that matches. Caudra changes halves as soon as the \
         terminal reports a new mode, for example when your desktop switches to \
         dark mode. A terminal that cannot report its mode is asked for its \
         background color instead. Themes outside these pairs stay as you left \
         them.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Caudra asks again every ten minutes, and also when the terminal \
         regains focus or changes size, so reattaching a multiplexer to \
         another terminal updates the theme. Following the terminal only \
         changes the running session, and the theme you saved from `/theme` \
         stays saved. A terminal that answers neither question is asked for its \
         background color only a few times. After that, Caudra asks only \
         whether it is in light or dark mode.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Mosh drops the request for mode reports before it reaches your \
         terminal. Turn the reports on yourself when you connect, and off again \
         when you leave:\n"
    )
    .unwrap();
    writeln!(
        out,
        "```sh\nprintf '\\033[?2031h'; mosh user@host; printf '\\033[?2031l'\n```\n"
    )
    .unwrap();

    writeln!(out, "### `ui.theme_light`\n").unwrap();
    writeln!(
        out,
        "Light half to use in place of the one from the pairing table, or to \
         give a theme that has no pair. `ui.theme` becomes the dark half:\n"
    )
    .unwrap();
    writeln!(
        out,
        "```toml\n[ui]\ntheme = \"tokyonight\"\ntheme_light = \"catppuccin_latte\"\n```\n"
    )
    .unwrap();
    writeln!(
        out,
        "Leave `ui.theme` unset to pair the light theme with whatever you last \
         picked from `/theme`.\n"
    )
    .unwrap();
}

fn write_update_check_section(out: &mut String) {
    writeln!(out, "### `ui.update_check`\n").unwrap();
    writeln!(
        out,
        "Enabled by default in the interactive UI. Caudra checks GitHub releases \
         in the background at startup. When a newer version is available, a \
         one-line banner above the prompt names it and the command that \
         installs it, in every session of that process. A shared on-disk cache \
         reuses successful checks for 24 hours across processes, with backoff \
         after failures. Headless and ACP runs do not start an update check. \
         Nothing is installed without asking.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Click the banner's text or run `/update` to install the release. \
         Caudra asks first, then stops every session, runs the installer in \
         the terminal, and restarts on the same tabs with the flags it was \
         started with. Work that was running is interrupted, and \
         `agent.resume_interrupted` decides whether it resumes. If nothing is \
         installed, the sessions reopen with a warning. An `--ephemeral` run, \
         and Windows, where the install finishes after Caudra exits, exit \
         instead of restarting. A Homebrew, Nix, or Cargo installation is \
         updated through its package manager instead.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Click the banner's `×` or run `/update dismiss` to hide it in every \
         session until Caudra exits. The dismissal is not saved, so the next \
         start shows the banner again while the release is still newer.\n"
    )
    .unwrap();
    writeln!(
        out,
        "GitHub receives your source IP address and normal request metadata, \
         including a `caudra` user agent. The check sends no session data, \
         machine identifier, or current version.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Set `ui.update_check = false` to disable startup checks. \
         `CAUDRA_ENABLE_UPDATE_CHECK=0` disables them for one run. \
         `CAUDRA_ENABLE_UPDATE_CHECK=1` checks even when the config says false, \
         and it asks GitHub instead of reading the cache, so a release published \
         since the last check shows at once. Its answer refreshes the cache for \
         later starts. An empty value leaves the config in effect. A normal run \
         still contacts your model provider, fetches the public \
         [models.dev](https://models.dev/) model catalog when the cached copy \
         is more than a day old, and sends web searches to Exa when the agent \
         uses `websearch`.\n"
    )
    .unwrap();
    writeln!(out, "### `ui.update_channel`\n").unwrap();
    writeln!(
        out,
        "The default, `\"auto\"`, follows stable releases from a stable build \
         and preview releases from a prerelease build. `\"stable\"` excludes \
         prereleases. `\"preview\"` includes both prereleases and stable releases, \
         so a preview can graduate to its stable release. After graduation, \
         `\"auto\"` follows stable releases.\n\n\
         Run `caudra update --channel auto`, `caudra update --channel stable`, \
         or `caudra update --channel preview` to update manually. An explicit \
         update bypasses the startup cache and opt-out. The banner's command \
         names the channel the check used, with `auto` resolved to `stable` or \
         `preview`.\n"
    )
    .unwrap();
    writeln!(out, "### `ui.changelog_on_update`\n").unwrap();
    writeln!(
        out,
        "Enabled by default. The first interactive start after an update opens \
         the release notes for every release since the one that ran before. It \
         opens once per update, even with several Caudra processes running. A \
         start that runs a prompt from the command line, exits when it is done, \
         or opens on a login or trust dialog leaves the notes to the next start. \
         A new install shows no notes. Caudra reads this setting at startup.\n\n\
         `/changelog` opens the same notes at any time, and `/changelog all` \
         opens every release. Press `g` in the modal to switch between the two, \
         and `o` to open the release you are reading on GitHub. The same notes \
         are on the [Release notes](/docs/release-notes/) page.\n"
    )
    .unwrap();
}

fn write_retention_section(out: &mut String) {
    write_section(out, "[storage.retention]", RetentionConfig::FIELDS);
    writeln!(
        out,
        "`trim` and `forget` are keep policies in `restic forget` terms: `keep_last`, \
         `keep_hourly`, `keep_daily`, `keep_weekly`, `keep_monthly`, `keep_yearly` take a \
         count, and `keep_within` plus `keep_within_hourly` through `keep_within_yearly` \
         take a duration such as `\"90d\"` or `\"2y5m7d3h\"`. A session is kept when any \
         rule matches. An empty `forget` policy disables automatic deletion. See \
         [Sessions](/docs/sessions/#retention) for what each tier keeps and how the sweep \
         runs.\n"
    )
    .unwrap();
}

fn write_snapshots_section(out: &mut String) {
    write_section(out, "[storage.snapshots]", SnapshotsConfig::FIELDS);
    writeln!(
        out,
        "A change record that would cover more than `max_files` files or `max_bytes_mb` of \
         file data is refused, and its call runs without a record. A file over \
         `max_file_bytes_mb` is left unrecorded. Values above the store's limits are lowered \
         to them, and zero is rejected. See [Sessions](/docs/sessions/#limits) for what a \
         record covers.\n"
    )
    .unwrap();
}

fn write_telemetry_section(out: &mut String) {
    write_section(out, "[telemetry]", TelemetryConfig::FIELDS);
    writeln!(
        out,
        "Every field also has an environment variable, shown in the Env \
         column, and the variable wins. See [Telemetry](/docs/telemetry/) \
         for the full picture.\n"
    )
    .unwrap();
}

fn write_worktrees_section(out: &mut String) {
    write_section(out, "[worktrees]", WorktreesConfig::FIELDS);
    writeln!(
        out,
        "Inside a Herdr pane, `auto` asks Herdr to create and remove worktrees, so each one \
         opens as a grouped Herdr workspace. Inside an Orca terminal, `auto` asks Orca, so \
         each one appears in Orca's worktree list. `directory` applies only to worktrees git \
         creates. See [Worktrees](/docs/worktrees/) for what `/worktree` does with each \
         backend.\n"
    )
    .unwrap();
}

fn write_websearch_section(out: &mut String) {
    write_section(out, "[websearch]", WebsearchConfig::FIELDS);
    writeln!(
        out,
        "Global-only: a project `caudra.toml` that sets `[websearch]` fails to load, because the \
         table holds credentials and decides where search queries go. Each backend reads its \
         settings from its own table below. A key left unset reads the variable in its Env \
         column, from Caudra's environment or the global `.env`, never a project `.env`. \
         Prefer `{{env:NAME}}` or `{{file:PATH}}` references over a credential written in the \
         file. Edits apply at once while the terminal UI runs, and `/settings` edits them for \
         you. See [Web search](/docs/web-search/).\n"
    )
    .unwrap();
    for backend in WebsearchBackendName::ALL {
        if !backend.fields().is_empty() {
            write_section(out, &format!("[websearch.{backend}]"), backend.fields());
        }
    }
}

fn write_automations_section(out: &mut String) {
    write_section(out, "[automations]", AutomationsConfig::FIELDS);
    writeln!(
        out,
        "[Automations](/docs/automations/) are experimental and need `automations = true` under \
         [`[experimental]`](#experimental-features). Only the global `caudra.toml` may hold this \
         table. Caudra rejects a project `.caudra/caudra.toml` that contains it, even an empty \
         one, so a repository cannot raise these limits or let automations reach a private \
         network.\n"
    )
    .unwrap();
}

fn write_web_section(out: &mut String) {
    write_section(out, "[web]", WebConfig::FIELDS);
    out.push_str(
        "`webfetch` reaches only the public internet unless this table names private targets \
         as well. It applies to local sessions. A [remote workspace](/docs/remote-workspaces/) \
         or a [sandbox](/docs/sandboxes/) keeps the public-internet rule. Only the global \
         `caudra.toml` may hold this table. Caudra rejects a project `.caudra/caudra.toml` that \
         contains it, even an empty one, so a repository cannot point the model at your \
         network.\n\n\
         ```toml\n\
         [web]\n\
         allow_private_hosts = [\"100.64.0.2:8787\", \"localhost:3000\", \"[fd7a::1]:80\"]\n\
         allow_private_networks = [\"tailscale\", \"10.20.0.0/16\"]\n\
         ```\n\n\
         An `allow_private_hosts` entry admits one host name or address on one port, whatever \
         the name resolves to. An `allow_private_networks` entry admits any port, and a name \
         only when every address it resolves to is public or inside a listed network. It takes \
         a CIDR or one of these names:\n\n\
         | Name | Addresses |\n\
         |------|-----------|\n\
         | `loopback` | `127.0.0.0/8` and `::1` |\n\
         | `private` | `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, and `fc00::/7` |\n\
         | `cgnat` | `100.64.0.0/10` |\n\
         | `tailscale` | `100.64.0.0/10` and `fd7a:115c:a1e0::/48` |\n\n\
         No name covers link-local addresses, which hold the cloud metadata service at \
         `169.254.169.254`. A CIDR can still name them. No entry admits an unspecified or a \
         multicast address.\n\n\
         An `http` URL to a listed target stays `http`, because a local server often has no \
         certificate. Other `http` URLs are still upgraded to `https`. A redirect may lead to a \
         listed target only from a URL the list admitted too, so a public page cannot send \
         `webfetch` into your network. When a local `webfetch` refuses a private target, its \
         error names the `HOST:PORT` to add to `allow_private_hosts` and links to this section, \
         so the model can tell you what to change. The hint is the same whether or not this \
         table exists.\n\n\
         With a proxy set in `HTTP_PROXY`, `HTTPS_PROXY`, or `ALL_PROXY`, a request to a listed \
         target goes through the proxy unless `NO_PROXY` names its host. The proxy then \
         resolves the name, so a special-use name such as `localhost` passes only through an \
         `allow_private_hosts` entry. Each fetch reads the list once, when it starts, and an \
         edit applies to the next fetch in every open session. See [Hot reload](#hot-reload).\n\n",
    );
}

fn write_shell_env_section(out: &mut String) {
    write_section(out, "agent.shell_env", ShellEnvConfig::FIELDS);
    writeln!(
        out,
        "These choose which host variables reach local [shell commands](/docs/cli/#forwarding-more-variables), \
         and `/env` edits them. Only the global `caudra.toml` may hold this table. Caudra rejects a \
         project `.caudra/caudra.toml` that contains it, even an empty one, so a repository cannot \
         forward your credentials to the commands it asks for. A [sandbox](/docs/sandboxes/#shell-environment) \
         forwards its guest's variables instead, set per profile.\n"
    )
    .unwrap();
}

fn write_autopilot_section(out: &mut String) {
    write_section(out, "[autopilot]", AutopilotConfig::FIELDS);
    writeln!(
        out,
        "Limits for [autopilot](/docs/autopilot/). Only the global `caudra.toml` may hold this \
         table. Caudra rejects a project `.caudra/caudra.toml` that contains it, even an empty \
         one.\n"
    )
    .unwrap();
}

fn write_steering_section(out: &mut String) {
    out.push_str(
        "### `agent.steering`\n\n\
         Automatic steering repairs unusable model output and can add bounded guidance about \
         repeated behavior or needlessly long tool paths. Every rule is enabled by default. \
         Configure overrides in the `[agent.steering]` table. All fields are optional.\n\n",
    );
    write_table(out, SteeringConfig::FIELDS);
    out.push_str(
        "\n#### Rules\n\n\
         | Rule in `rules` | Behavior |\n\
         |-----------------|----------|\n",
    );
    for rule in SteeringRulesConfig::RULES {
        writeln!(out, "| `{}` | {} |", rule.name, rule.description).unwrap();
    }
    out.push_str(
        r###"
Recovery and advisory budgets are separate. Advisory rules allow at most 4 total injections per invocation. Repetition and tool-planning advisories each wait a default cooldown of 3 completed model responses.

Tool-planning evidence starts after the last response containing any successful tool result, including results outside the retained batch window. A background admission ends the failure episode without proving that the background work succeeded. Later terminal outcomes do not retroactively change that admission into a failed attempt.

Relative-path evidence is the latest response only, and the hint yields to repetition and tool-planning advisories. While an earlier hint remains in context, even across user turns, no new hint is added. Once compaction removes it, another hint appears only if a later response uses absolute paths again. A suggestion climbs at most one directory with `../`. Local sessions get suggestions only when the working directory the model sees is the canonical project path. Remote workspaces, sandboxes, and source-code scopes get suggestions inside the working directory only. Paths with backticks, angle brackets, control characters, or invisible Unicode formatting characters are never quoted. Caudra never rewrites the paths a model sends.

An empty-response episode lives in the transcript tail, so it survives a restore and a new invocation. A message typed into a stall is answered, but it does not refill the budget: only a response carrying a tool call or visible text ends the episode. `max_stalled_turns` bounds the turns that interleaved rules spend between them, independently of any single rule's allowance.

Advisories only accompany an independently scheduled next request. They never reopen a valid final answer. Tool-looking prose, JSON, XML, code fences, and quoted examples do not independently trigger protocol correction. Caudra does not scrape tool names or arguments from text and execute them. Only actual tool calls pass through normal validation and authorization. Ordinary assistant answers do not have to be JSON.

#### Rule fields

Each table at `agent.steering.rules.<rule>` accepts these common fields:

"###,
    );
    write_table(out, SteeringRule::COMMON_FIELDS);
    out.push_str(
        r###"
Custom prompts replace guidance only. They are literal user-configured text, without template expansion or executable expressions. They do not change triggers, budgets, enforcement, or factual tool-failure information. A custom prompt cannot authorize a tool or turn a rejected call into an executed one.

The remaining fields are integers. All ranges are inclusive. Set `enabled = false` to disable a rule rather than setting a positive threshold to zero.

| Field under `rules` | Default | Range | Unit and meaning |
|---------------------|---------|-------|------------------|
"###,
    );
    for rule in SteeringRulesConfig::RULES {
        for field in rule.fields {
            writeln!(
                out,
                "| `{}.{}` | {} | {} | {} |",
                rule.name,
                field.name,
                default_cell(&field.default),
                range_cell(field),
                field.description,
            )
            .unwrap();
        }
    }
    out.push_str(
        r###"
Validation also requires:

- `repetition.window >= repetition.max_cycle * repetition.cycle_repeats`.
- `repetition.text_window >= repetition.text_repeats`.
- `tool_planning.after_calls >= tool_planning.after_responses`.

Unknown fields, invalid types, out-of-range values, and impossible threshold/window combinations are rejected, even for disabled rules. Cooldowns count completed model responses, not seconds, stream chunks, tool children, or injected messages. Advisory eligibility excludes synthetic messages, empty markers, reasoning-only padding, and private title, compaction, or evaluator requests. Advisory evidence is the responses to the request in flight: a new user message ends it, as a compaction or a model change does, so an ordinary conversation is never advised. Recent-pattern windows reset with it, without refilling an active invocation's budgets.

Tool-planning guidance asks the model to reconsider its tool choices and identify the next useful action. It does not switch Plan Mode or require a todo list.

#### Global and exact-model overrides

This example disables repetition guidance globally, then enables it with a higher threshold for one exact model and adjusts that model's tool-planning guidance:

```toml
[agent.steering.rules.repetition]
enabled = false

[agent.steering.models."openai/gpt-5".rules.repetition]
enabled = true
text_repeats = 4

[agent.steering.models."openai/gpt-5".rules.tool_planning]
after_calls = 8
prompt = "Reassess your recent tool choices. Choose a different useful action if these calls are not helping."
```

Model entries accept `enabled`, `max_recoveries`, `max_advisories`, `max_stalled_turns`, and `rules` with the same types and limits as the global fields. They cannot contain another `models` table. Omitted fields inherit through the resolution order below.

Keys are case-sensitive exact IDs, at most 512 UTF-8 bytes each. Use a nonempty provider and model suffix separated by `/`. Additional slashes inside the suffix are allowed, but every segment must be nonempty. Whitespace, control characters, `*`, `?`, `[`, `]`, `{`, `}`, and backslashes are rejected. Matching requires no authentication or model discovery. There are no glob overrides, provider-wide layers, capability guesses from model names, or Lua detector callbacks.

Global and project settings merge field by field, with project values taking precedence. Model maps merge by exact key and rules merge by rule name and field. Omission inherits. Explicit `false` and `0` survive merging. An empty table does not clear inherited entries. Disable an inherited model policy or rule with `enabled = false`.

After merging, resolve against the effective routed model for Chat, Plan, or a delegated task:

1. Start with built-in policy and rule defaults.
2. Apply explicit global fields and rule fields.
3. Apply explicit matching model fields and rule fields.

A child resolves its own effective model using the inherited unresolved configuration, rather than inheriting the parent's resolved policy or runtime counters.

#### Budgets and safety boundaries

A new externally initiated main-agent or task invocation gets its own allowance. An explicit user/caller resume starts a fresh bounded invocation. Automatic continuations, internal retries, task report-correction prompts, compaction, and mid-run queued instructions do not refill the active allowance, including when report correction constructs a fresh agent. Separately delegated children have independent allowances. Counters are not durable across process restarts or explicit task resume.

Charge one recovery for a completed-response-to-next-request transition caused by empty or truncated output, all-invalid tool calls, a response consisting entirely of repeat-policy refusals, an explicit protocol mismatch, or a missing task report. Corrective tool-error feedback can supply the guidance without a supplemental prompt and still consumes the transition. Malformed-argument and schema repair use this allowance without a separate rule table. Per-rule limits apply underneath the combined recovery cap.

A mixed batch with useful successful siblings proceeds normally. It is not replayed or charged once per child. Transport and authentication retries, ordinary tool execution failures, permission denials, normal successful tool progress, explicit goal evaluation, and manual steering are separate from model-format recovery. The recovery budget does not bound every possible agent loop. Outer turn limits and cancellation still apply.

At most one supplemental steering message is added per request. Recovery takes priority, then repetition, tool planning, and relative paths. Advisory exhaustion only suppresses hints. Recovery exhaustion with an unmet output contract reports a failure and retains partial output, except for `abandoned_turn`, which stops intervening and lets the turn end. A valid captured structured task report remains usable after an empty tail, but cancellation, transport/permission failures, and hard outer-limit failures do not become success.

`abandoned_turn` reads the tail of a response that called no tool and would otherwise end the turn. It fires on a text stopping at a bare colon, or on a last sentence that opens on an intent to act. It does not fire on a question, an offer, a completion, or a promise deferred behind another event, and code spans and quoted prose are removed before any of that is matched. It also stays quiet while a task, background job, or workflow is still running, because that work's report wakes the agent to continue. Tool-looking prose is not executed here either; the rule only decides whether to ask for one more response.

The resolved `enabled = false` disables automatic recovery, including truncation, advisories, and repeat-policy blocking. It leaves malformed-input rejection, schema validation, permissions, mode restrictions, cancellation, explicit goals, manual steering, and compaction policy intact. A rule-level switch disables only that rule. Zero budgets prevent the corresponding continuations or hints without bypassing input validation or repeat-policy enforcement.

Truncation recovery counts corrective requests, not ordinary responses or tool rounds. Each request consumes one attempt from `rules.truncation.max_attempts` and one recovery from `max_recoveries`. Automatic report-correction prompts and compaction do not refill either allowance. The last allowed correction may complete the answer. If another correction is needed, exhaustion reports an error with partial output and usage retained, including when `max_recoveries = 0`.

Disabling the master switch or setting `rules.truncation.enabled = false` stops with the truncated outcome instead of requesting a continuation. Disabled or exhausted truncation does not fall through to empty-response repair, even when the truncated response is empty. An unresolved truncated task result remains cut short rather than reopening through report repair. A valid structured report can still satisfy the task's report contract. Cancellation, queued user instructions, and outer turn limits take priority over automatic steering.

"###,
    );
}

fn write_decisions_section(out: &mut String) {
    out.push_str(
        "### `decisions`\n\n\
         The `[decisions]` table holds the settings every decision endpoint shares: deadlines, \
         logging, features, and thresholds. The endpoints themselves are `[decisions.<name>]` \
         tables in the global `providers.toml`, and the engine runs only while one of them is \
         active. `caudra auth login --decision` adds one and makes it active. See \
         [Decision endpoints](/docs/providers/#decision-endpoints). With no active endpoint, \
         Caudra still validates this table. It starts no engine, sends no decision requests, \
         reads no engine credentials, and leaves decision logs untouched. No passive feature or decision logging is enabled by default. Explicit \
         workflow [`decide()` calls](/docs/workflows/#typed-decisions) and the MCP \
         [`decide()` and `rerank()` helpers](/docs/mcp/#decisions-and-reranking) need an active \
         endpoint but no passive feature.\n\n\
         These settings are global-only. Projects may set individual features \
         to `\"off\"`, set `log = false`, or keep or shorten inherited log retention. \
         Other project overrides are errors, even when they repeat a global value. \
         Disabling globally required Auto screening or its active content screening restores \
         prompting for eligible Auto calls. `base_url`, `protocol`, `model`, `api_key_env`, \
         and `allow_http` are not accepted here. The error for each one shows the \
         `providers.toml` table that replaces it. The [upgrade](#config-file-versions) \
         comments out `allow_remote`, because configuring an endpoint is now the consent to \
         send decision context to it.\n\n",
    );
    write_table(out, DecisionsConfig::FIELDS);
    write!(
        out,
        "\nBoth protocols send text only. OpenAI maps native boolean questions to predicates, \
         retaining the true and false criteria in the instruction text. Answers normalize to the \
         same typed results, so workflows and MCP snippets read the same answers under either \
         protocol. A refusal fails the whole \
         evaluation without applying partial answers.\n\n\
         Caudra retries HTTP 408, 429, and 5xx responses at most twice. Each retry waits for the \
         delay the server requests in `retry-after-ms` or `Retry-After`, or else for an exponential \
         backoff that starts near half a second. No retry waits past the request deadline, so under \
         the default {timeout_ms} ms `timeout_ms` most retries need a short server-requested delay. \
         Connection failures, 401, 422, and other client errors fail at once.\n\n\
         Tool search and the [reranking features](#reranking) judge up to 64 candidates in one \
         request and use `rerank_timeout_ms` (default {rerank_timeout_ms} ms) for every 16 of them \
         instead. Memory recall \
         keeps `timeout_ms` because it delays the start of a turn. A `decide()` or `rerank()` call \
         from a workflow or an MCP snippet may ask for up to the larger of the two.\n\n\
         Redaction is best effort. Decision context can include commands, task text, tool output, \
         and candidate descriptions. Review what you send and any exports before sharing them. \
         See [decision advice and logging](/docs/permissions/#decision-engine-advice).\n\n\
         #### `decisions.features`\n\n\
         `off` disables the feature. `shadow` collects predictions without applying them. \
         `advise` adds caution or suggestions. `enforce` applies only the feature-specific behavior \
         listed below, never permission grants or relaxed executor restrictions. \
         Unsupported modes are configuration errors. YOLO bypasses permission advice, Auto screening, \
         and permission-related shell-effect checks. Content screening, tool search, skill \
         selection, goal prescreening, subagent routing, the question-tool nudge, and the \
         reranking features still follow their feature settings in YOLO. Deterministic denies and \
         hard restrictions still apply.\n\n\
         | Feature | Default | Supported modes | Behavior beyond shadow |\n\
         |---------|---------|-----------------|------------------------|\n",
        timeout_ms = DecisionsConfig::default().timeout_ms,
        rerank_timeout_ms = DecisionsConfig::default().rerank_timeout_ms,
    )
    .unwrap();
    for feature in DecisionFeatures::ALL {
        let modes: Vec<String> = feature
            .modes
            .iter()
            .map(|mode| format!("`{}`", mode.as_str()))
            .collect();
        writeln!(
            out,
            "| `{}` | `{}` | {} | {} |",
            feature.name,
            FeatureMode::default().as_str(),
            modes.join(", "),
            feature.description,
        )
        .unwrap();
    }
    out.push_str(
        "\n#### `decisions.thresholds`\n\n\
         Flag thresholds trigger at or above the configured value. Goal prescreening uses an \
         at-or-below comparison. Content screening requires both signals in the same chunk. \
         After content is flagged, Auto uses 75% of `auto_flag` for upload and credential flags.\n\n",
    );
    write_table(out, DecisionThresholds::FIELDS);
    out.push_str(
        "\n#### Question-tool nudge\n\n\
         `question_tool_nudge` defaults to `off` and supports `off`, `shadow`, and `advise`. \
         It checks an attempted final prose reply for a live request for the user to clarify, choose, \
         supply information, or approve a next step. It needs a configured engine and a usable \
         `question` tool with an answer channel in the main session. Subagents and frontends without \
         that channel are excluded. YOLO does not disable it. Once the plan is saved in plan mode, \
         the nudge stays off until the next user message. A `question` answer cannot leave plan \
         mode, so the plan is approved through the plan menu, autopilot, or the client's mode \
         switch.\n\n\
         In `shadow`, the prediction leaves the reply unchanged. In `advise`, a score at or above \
         `decisions.thresholds.question_tool_nudge` (default `0.85`) adds a normal system reminder \
         asking the assistant to use `question` for the outstanding answer. The original reply stays \
         in history. The reminder follows the `show_reminders` display preference and can occur at \
         most once per genuine user-input episode. Synthetic reminders, retries, and resume do not \
         renew that allowance. It never fabricates a tool call, an answer, or approval. While \
         [autopilot](/docs/autopilot/#questions) is engaged, its delegate answers the form that \
         the reminder leads to.\n\n\
         The request contains redacted excerpts of the current visible reply and relevant user \
         request within the 1,500-byte serialized state limit. Long replies retain an opening and \
         a larger tail, so questions in the omitted middle can be missed. Errors, timeouts, invalid \
         answers, and scores below the threshold leave the reply unchanged.\n\n\
         #### Reranking\n\n\
         Tool search, `memory_search`, `history_search`, `memory_recall`, `memory_duplicates`, \
         `compaction_retention`, `output_pointers`, and `source_code_context` use the engine as a \
         reranker. A cheap first pass, such as keyword matching, picks the candidates. The engine then answers one true or false \
         relevance question per candidate, and Caudra orders, prunes, or names candidates by the \
         probability it returns. The feature table above lists what each one sends.\n\n\
         One request holds up to 64 candidates, each with an opaque ID such as `c00`, so the \
         engine reads the shared question and query once. `memory_search`, `history_search`, and \
         `source_code_context` judge up to 64 candidates; the others judge up to 16. A request \
         waits one `rerank_timeout_ms` for every 16 candidates it holds. Candidate text is redacted and cut to a \
         fixed size before it is sent, and every question tells the engine to treat the candidate as \
         data rather than instructions. The request state, such as the query, task, or goal, stays \
         within the 1,500-byte state limit.\n\n\
         Enforcing features compare each probability with `thresholds.relevance_floor` (default \
         `0.1`). Tool search only reorders, putting candidates at or above the floor first. \
         `source_code_context` keeps the rows at or above the floor, best first, and leaves out the \
         rest and the rows past the first 64, unless none clears the floor. \
         `memory_search` omits matches below the floor and names them. \
         `history_search` drops matches below the floor and returns the rest best first, as \
         [Ranking](/docs/code/#ranking) describes. \
         `compaction_retention` keeps the same number of tool results as without the engine, \
         choosing them by relevance, and never drops a result it could not otherwise drop. Advisory \
         features name a candidate only at or above `thresholds.routing_confidence` (default \
         `0.9`).\n\n\
         When any request fails, is refused, times out, or is cancelled, or the decision service \
         changes while it runs, the feature behaves as if it were `off`. In `shadow`, requests are \
         sent and logged without effect. With `log = true`, an enforcing feature records \
         `rerouted` when the order it used differs from the first pass, and an advisory feature \
         records `advised` when it named a candidate. Workflows and MCP snippets can rerank their \
         own candidates with `rerank()`: see [workflows](/docs/workflows/#reranking-candidates) \
         and [MCP](/docs/mcp/#decisions-and-reranking). See also \
         [memory relevance](/docs/memory/#relevance-from-the-decision-engine).\n\n\
         #### Question overrides\n\n\
         Only the permission question set currently supports a user-global file override: \
         `~/.config/caudra/decisions/permission.json`. It must be a regular JSON file no larger \
         than 64 KiB, retain all required question IDs as `noul`, and pass question validation. \
         It is read when a base URL is set and permission advice or Auto screening is enabled. \
         Projects cannot supply this override. Other feature question sets have no file override. \
         While the terminal UI runs, an edit to the file applies at once, and a file that fails \
         to load keeps the questions in use, see [Hot reload](#hot-reload).\n\n",
    );
}

fn write_tool_output_section(out: &mut String) {
    writeln!(out, "### `ui.tool_output_lines`\n").unwrap();
    writeln!(
        out,
        "How many terminal rows of output an open card shows per tool before it says how \
         many it is holding back. A line that wraps spends a row for each row it wraps to, \
         so an abridged card is the same height whatever its lines are. Clicking the card \
         shows all of it regardless. \
         All values are `usize` with a minimum of {MIN_TOOL_OUTPUT_LINES}.\n\n\
         The `bash`, `code`, and `task` entries apply only when \
         `ui.scroll_card_lines` is `0`. Above that, those tools draw a fixed window of \
         that many rows instead, and the budget here goes unused. `write` does not reach \
         a `file_write` that created a file, whose body is that file and is always drawn \
         whole. It is also a floor rather than a bound for anything drawn as a diff, since \
         a diff is already only the part that changed: an edit, a patch, and an overwrite \
         are drawn whole until they run long, and raising `write` past that point is what \
         makes this number matter to them. No entry reaches an MCP call's card, which \
         opens with its arguments and its whole answer.\n"
    )
    .unwrap();
    writeln!(out, "| Field | Default | Tools |").unwrap();
    writeln!(out, "|-------|---------|-------|").unwrap();
    for (name, default) in ToolOutputLines::FIELD_DEFAULTS {
        let tools = ToolOutputLines::FIELD_TOOLS
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, tools)| {
                tools
                    .iter()
                    .map(|tool| format!("`{tool}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        writeln!(out, "| `{name}` | {default} | {tools} |").unwrap();
    }
    writeln!(out).unwrap();
}

const CONFIG_FILES_INTRO: &str = "## Config files\n\n\
    `caudra.toml` holds the settings that only you write. A file stays separate from it when \
    Caudra also writes the file, when the file decides where credentials or processes go, or when \
    it has its own rules for trust, errors, or privacy.\n\n\
    | File | Scope | Holds | Kept separate because | Reference |\n\
    |------|-------|-------|-----------------------|-----------|\n";
const CONFIG_FILES_COMMANDS: &str = "\n`caudra config files` lists where each file lives on your \
    machine and whether it is there. `caudra config example FILE` prints the reference of a TOML \
    file, such as `caudra config example mcp`. See [`caudra config`](/docs/cli/#caudra-config).\n";

const ENVIRONMENT_SECTION: &str = r#"## Environment variables

A string value in a global config file can read an environment variable. Write `{env:NAME}` where the value goes, and Caudra puts the value of `NAME` there when it reads the file. This keeps a secret or a machine-specific value out of the file:

```toml
[telemetry]
endpoint = "https://{env:OTEL_HOST}"
headers = { authorization = "Bearer {env:OTEL_TOKEN}" }
```

- It works in the global `caudra.toml`, `permissions.toml`, `mcp.toml`, `providers.toml`, and `workcell.toml`. `sandboxes.toml` keeps every value as written.
- A reference can be the whole value or part of it, and one value can hold several. Only string values expand, so keys, numbers, and booleans cannot come from the environment.
- Caudra looks in the process environment first, then in the global `.env`. A project `.env` never supplies a value.
- An unset variable is an error that names the file, the line, and the variable. At startup, a bad `caudra.toml` stops startup and a bad `providers.toml` exits. A bad `permissions.toml` fails closed and denies every tool call. A bad `mcp.toml` loads none of its servers, and `/mcp` shows the error. An edit that breaks a file while Caudra runs follows the [hot reload](#hot-reload) rules instead.
- A project file under `.caudra/` cannot hold a reference, and Caudra refuses to load it. A project MCP server on a public HTTPS URL connects without review, so a reference there could send your secret to a server the repository chose.
- `${env:NAME}`, the PowerShell form, stays as written, so shell commands and permission patterns can use it.
- When Caudra writes a file, such as `/mcp` turning a server off or `caudra auth login` adding a provider, the reference stays in the file rather than its value.

"#;

const FILES_SECTION: &str = r#"## Values from files

A string value in a global config file can also hold the contents of a file. Write `{file:PATH}` where the value goes, and Caudra puts the file's text there, with spaces and line breaks trimmed from both ends. This suits a secret that a password manager or a deployment tool keeps in a file:

```toml
[telemetry]
headers = { authorization = "Bearer {file:~/.secrets/otel-token}" }
```

- The same files accept it, and the rules for [environment variables](#environment-variables) apply to it too. Only string values expand, a project file under `.caudra/` cannot hold one, `${file:PATH}` stays as written, and Caudra writes the reference back rather than the contents.
- A relative path starts at the directory of the config file that holds it. `~` followed by `/` stands for your home directory, and so does `~\` on Windows.
- Caudra removes `.` and `..` from the path as written before it reads the file. After a symlinked directory, `..` goes back to the directory that holds the link, as on Windows, rather than to the parent of its target. A symlinked file is followed.
- A path can hold `{env:NAME}`, such as `{file:{env:CREDENTIALS_DIRECTORY}/otel-token}`. Caudra uses the variable's value as written, so a `~` in it is a plain name.
- On Windows, write a path with backslashes in a single-quoted TOML string, such as `'{file:C:\Users\me\otel-token}'`, or use forward slashes. A double-quoted string needs every backslash doubled. A path such as `\secrets\otel-token` starts at the root of the config file's drive, and UNC paths such as `\\server\share\otel-token` work.
- The file must be a regular file in UTF-8, at most 1 MiB. A path cannot contain `}`, or start or end with a space.
- A missing or unreadable file is an error that names the config file, the line, and the path Caudra tried, never the contents. The config file then fails to load, with the same effect as an unset variable.
- Caudra does not watch the file. It reads it again when it reloads the config file that names it, such as after you edit that file or run `/reload`.

"#;

const HOT_RELOAD_SECTION: &str = r#"## Hot reload

While the terminal UI runs, Caudra watches its config files, skill and command directories, and [system prompt profiles](/docs/system-prompts/). It applies an edit shortly after you save it. One line in the footer then says what reloaded, what still needs `/reload` or a restart, and which file failed to load. For `caudra.toml` it names the tables whose values changed. It names files and keys and never shows a value from them. A save that changed nothing says nothing, and so do Caudra's own writes, such as turning a server off in `/mcp`. Headless runs and ACP read configuration once, at startup.

| What changed | When it applies |
|--------------|-----------------|
| `caudra.toml` | [`[decisions]`](#decisions), [`[web]`](#web), [`[websearch]`](/docs/web-search/), `ui.show_decisions`, [`plugins.skill.pstack`](/docs/skills/#pstack), and [`plugins.skill.mattpocock`](/docs/skills/#mattpocock) at once, in every open session. `[agent]` settings and [`[autopilot]`](/docs/autopilot/#budgets) limits from the next turn. Other tables after `/reload`. `[experimental]` after a restart. |
| `permissions.toml` | Before the next tool call. |
| `mcp.toml` | At once. New servers start, even when Caudra started with none. Removed and disabled servers stop, and changed servers restart. The other servers keep running. |
| `providers.toml` | The next time Caudra reads it, such as when a model list opens. An edit to the active [decision endpoint](/docs/providers/#decision-endpoints) applies at once, and removing its table turns decisions off. |
| `decisions/permission.json` | At once. See [Question overrides](#question-overrides). |
| Skills | At once. |
| System prompt profiles | From the next turn. |
| Custom commands | At once. |
| Workflows and automations | Caudra reads them again each time it lists, launches, or arms one. |
| `workcell.toml` and `sandboxes.toml` | After `/reload`. |
| `init.lua` | After `/reload`, which runs it again. A changed script never runs by itself. |
| Global `.env` | `{env:NAME}` references follow it at once. `[websearch]` takes up both its references and the backend variables it leaves to the environment, such as `SEARXNG_URL`. Shell commands and other keys read from the environment keep their startup values until a restart. |
| Files that `{file:PATH}` reads | When the config file that names them reloads. Caudra does not watch them. |
| Project `.env` | After a restart. |

A [remote workspace](/docs/remote-workspaces/) session keeps the permissions and commands of its workspace. Its project files apply after `/reload`.

### The prompt cache

Every request starts with the tool definitions, then the system prompt, then the conversation. Providers cache that prefix, so a turn pays full price only for what is new since the previous request. A change to the tool definitions or the system prompt breaks the match at that point, and the provider reads the whole conversation again at full price. With Claude, a cached token costs a tenth of a fresh one and writing the cache costs a quarter more than a fresh one, so the prompt of that request costs about twelve times what a hit would. `/usage` shows it as a drop in the [cache hit rate](/docs/token-economy/#cache-hit-rate).

So Caudra fixes the tool definitions and the system prompt when a conversation sends its first request, and keeps both byte for byte until it rebuilds them. What Caudra enforces follows your edits at once, whatever the model was told. Permissions, MCP servers, and which tools can run follow the files. The model learns of a change from a [reminder](#what-the-model-is-told) appended to the conversation, which costs only its own tokens.

### When Caudra rebuilds

A rebuild takes the configuration as it is at that moment. With nothing pending, that gives the same bytes, and the cache holds. With something pending, that one request reads the whole conversation at full price, however many edits are pending. Edits to [instruction files](/docs/context/#instruction-files) and notes that other sessions add to [memory](/docs/memory/#when-the-view-changes) count as pending too.

Caudra rebuilds at these moments, where the request changes or the cache is gone anyway:

| Moment | Why then |
|--------|----------|
| Compaction | The conversation is new. |
| Switching model or provider | Each model keeps its own cache. |
| Switching the system prompt profile | The system prompt changes with it. |
| Changing the thinking level | Claude's cache of the conversation does not survive a change to the thinking settings. |
| Switching mode | The first switch to plan adds the `plan` tool. Later switches rebuild the same definitions, so the cache holds unless an edit is pending. |
| The cache expires | Claude keeps a cache for five minutes after the last request that used it. Caudra counts from the end of the turn, so it rebuilds a little late rather than too early. Other APIs give no fixed lifetime, so Caudra never rebuilds on a timer for them. |

It also rebuilds at moments you choose, where the cached conversation can still be warm:

- a [conversation revert](/docs/sessions/#conversation-revert), which moves the conversation back to an earlier message
- `/cd`, which loads the instruction files and memory of the new directory
- `/reload`, which applies everything at once

MCP servers never change the definitions. The `code` tool is always there, and the servers and their tools reach the model in a [`# MCP servers` reminder](/docs/mcp/#code-mode) instead. A server that connects, fails, or changes its tools changes only that reminder.

To keep the cache through a long task, save your edits and let the next rebuild take them in. When the model needs a change right away, such as a tool you turned back on, run `/reload` and pay for one full read.

### What the model is told

Until the next rebuild, a `# Configuration changed` [reminder](/docs/context/#what-caudra-writes-on-your-behalf) tells the model how the configuration differs from what it was given. It arrives with your next message and lists:

- tools that are gone. A call to one fails.
- tools that are new, with the first sentence of their description. They become callable at the next rebuild.
- tools whose definition changed, such as the `skill` tool when a skill appears. A call follows the current definition.
- the system prompt, as a diff that the model applies to what it reads.

Each reminder states the whole difference, so the newest one replaces the others. When the configuration matches what the model was given again, because you put a file back or a rebuild took the change in, a short reminder tells the model to disregard the earlier notice. Instruction files and memory have reminders of their own, `# Instructions changed` and `# Memory updated`, which follow the same rules.

### When a file breaks

An edit that fails to load keeps the last valid version in force, and the footer names the file and the error. A red chip such as `[caudra.toml invalid]` stays in the status bar until the file loads again. Click it to see the error and what Caudra does meanwhile. A system prompt profile that fails to load shows as `[system prompts invalid]`, and a session that uses it keeps the version it loaded. `permissions.toml` is the exception. Caudra denies every tool call until the file loads again, so a broken edit never leaves an older, looser policy in force.

### Project files that grant access

The agent and the repository can both write `.caudra/`, so a project change that grants access waits for you. An edit to the project rules in `permissions.toml` invalidates [project trust](/docs/permissions/#toml-policy), and its allows stay off until you review them in `/permissions`. A [project MCP server that needs review](/docs/mcp/#project-server-trust) waits in `/mcp` until you approve the changed definition. No dialog opens by itself while you work. A `[review permissions]` or `[review MCP]` chip in the status bar opens the review when you are ready. A change that only removes access applies at once.

### Examples

Each one starts in the middle of a session, with the cache warm.

#### Add an MCP server

You add the `analytics` server of the [HTTP example](/docs/mcp/#http) to `~/.config/caudra/mcp.toml` while a `github` server runs. The footer shows `Reloaded mcp.toml`, and the server starts at once. The tool definitions stay as they were, so the next request reads the conversation from the cache. With your next message the model gets the server list again, now with `analytics` in it:

```text
{mcp_reminder}
```

The model can call the new tool through `code` at once.

#### Turn off an MCP server

You turn a server off in `/mcp`. Caudra writes `mcp.toml` itself, so the footer stays quiet. The server stops at once, and the next server list leaves it out. The definitions stay as they were, so the cache holds. When it was the last enabled server, a short reminder tells the model to disregard the list.

#### Switch to plan mode

You press `Tab` to plan while MCP servers run. Plan mode keeps the `code` tool and refuses each server call it does not allow, so MCP servers do not change the definitions. Only the first switch to plan changes them, by adding the `plan` tool, and that request reads the whole conversation at full price.

#### Disable a tool

You add to `caudra.toml`:

```toml
[agent]
disabled_tools = ["websearch", "analytics.*"]
```

The footer shows `Reloaded caudra.toml · [agent] from the next turn`. From the next turn Caudra refuses calls to `websearch` and to every tool of the `analytics` server. The definitions keep them until the next rebuild, and the reminder lists them as gone. The cache holds.

#### Edit a system prompt profile

You change a line in `~/.config/caudra/system-prompts/review.md` while a session uses the `review` profile. The footer shows `Reloaded system prompts`. The system prompt the model was given stays as it was, and the reminder carries the change as a diff:

```text
{profile_changes}
```

Each later edit sends a new reminder with the whole difference so far. After a large rewrite, every small edit that follows repeats the long diff, so run `/reload` once to fold the rewrite into the system prompt.

`/system-prompt` shows the prompt as the provider received it, so it keeps the old text until the next rebuild. A notice above it says that the profile changed since.

#### Add a skill

You create `~/.config/caudra/skills/git-release/SKILL.md`. The footer shows `Reloaded skills`. The list of skills is part of the `skill` tool's description, so the reminder shows that description as a diff with the new skill in it. The `skill` tool reads the skill directories each time it runs, so the model can load the new skill right away. The cache holds.

#### Edit an instruction file

You add a rule to `AGENTS.md`. The model gets an `# Instructions changed` reminder with the change as a diff, and the system prompt keeps the text it was built with until the next rebuild. The cache holds. See [Instruction files](/docs/context/#instruction-files).

#### Break the permissions file

A typo in `permissions.toml` leaves the definitions and the system prompt as they were, since neither carries your permission rules. Every tool call is denied until the file loads again, the footer names the error, and a red `[permissions.toml invalid]` chip stays in the status bar. The cache holds.

#### Put a file back

You save a change and put the file back before your next message. The model hears of neither. When a turn ran in between, the model got the reminder, and the next turn adds a short one telling it to disregard the earlier notice.

"#;

/// [`HOT_RELOAD_SECTION`] with its reminders rendered by the code that sends
/// them, so the page cannot drift from what the model reads.
fn hot_reload_section() -> String {
    let profile_notice = configuration_notice(
        surface(EXAMPLE_PROFILE, json!([])),
        surface(EXAMPLE_PROFILE_EDITED, json!([])),
    );
    let (head, tail) = CONFIGURATION_CHANGED_PROMPT
        .split_once(CHANGES_SLOT)
        .expect("the notice has a slot for the changes");
    let profile_changes = profile_notice
        .strip_prefix(head)
        .and_then(|rest| rest.strip_suffix(tail))
        .expect("the notice fills its template");
    HOT_RELOAD_SECTION
        .replace(MCP_REMINDER_SLOT, mcp_servers_reminder().trim_end())
        .replace(PROFILE_CHANGES_SLOT, profile_changes)
}

/// The `# MCP servers` reminder once the example's `analytics` server joins a
/// running `github` server.
fn mcp_servers_reminder() -> String {
    let servers = [
        example_server(
            EXAMPLE_MCP_SERVER,
            EXAMPLE_MCP_TOOL,
            EXAMPLE_MCP_TOOL_DESCRIPTION,
            json!({
                "type": "object",
                "properties": { "sql": { "type": "string" } },
                "required": ["sql"],
            }),
        ),
        example_server(
            EXAMPLE_RUNNING_SERVER,
            EXAMPLE_RUNNING_TOOL,
            EXAMPLE_RUNNING_TOOL_DESCRIPTION,
            json!({
                "type": "object",
                "properties": {
                    "owner": { "type": "string" },
                    "repo": { "type": "string" },
                },
                "required": ["owner", "repo"],
            }),
        ),
    ];
    let listed = catalog::render(&servers, &Names::new(&servers));
    MCP_SERVERS_PROMPT.replace(CATALOG_SLOT, &listed)
}

fn example_server(name: &str, tool: &str, description: &str, input_schema: Value) -> ServerView {
    ServerView {
        name: name.to_owned(),
        title: None,
        state: ServerState::Connected,
        instructions: None,
        tools: vec![ToolView {
            name: tool.to_owned(),
            title: None,
            description: description.to_owned(),
            input_schema,
            output_schema: None,
        }],
        resources: false,
    }
}

/// The `# Configuration changed` notice a session carries while `live`
/// differs from the `fixed` prefix.
fn configuration_notice(fixed: Surface, live: Surface) -> String {
    let mut baseline = SurfaceBaseline::default();
    baseline.settle(fixed, true, (), 0, &[]);
    baseline
        .settle(live, false, (), 0, &[])
        .notice
        .expect("the surfaces differ")
}

fn surface(system: &str, tools: Value) -> Surface {
    Surface::new(system.to_owned(), tools)
}

fn write_config_files_section(out: &mut String) {
    out.push_str(CONFIG_FILES_INTRO);
    for file in CONFIG_FILES {
        let scopes: Vec<&str> = file.scopes.iter().map(|scope| scope.label()).collect();
        let needs = file
            .feature
            .map(|feature| format!(" (needs `experimental.{}`)", feature.key()))
            .unwrap_or_default();
        let reference = if file.example.is_some() {
            format!(
                "[`{}`]({})",
                file.example_command(),
                gen_reference_configs::link(file)
            )
        } else {
            "-".to_owned()
        };
        writeln!(
            out,
            "| [`{name}`]({docs}) | {scopes} | {holds}{needs} | {why} | {reference} |",
            name = file.name,
            docs = file.docs,
            scopes = scopes.join(", "),
            holds = file.holds,
            why = file.separate_because,
        )
        .unwrap();
    }
    out.push_str(CONFIG_FILES_COMMANDS);
}

/// Every file the global config directory can hold, in catalog order.
fn config_dir_files() -> String {
    let names: Vec<String> = CONFIG_FILES
        .iter()
        .filter(|file| file.scopes.contains(&Scope::Global))
        .map(|file| format!("`{}`", file.name))
        .collect();
    join_and(&names)
}

fn version_rows() -> String {
    VERSIONED_FILES
        .iter()
        .map(|(format, when_newer)| {
            let without = if format.requires_version {
                VERSION_REQUIRED
            } else {
                VERSION_OPTIONAL
            };
            format!(
                "| `{}` | {} | {without} | {when_newer} |\n",
                format.file, format.latest
            )
        })
        .collect()
}

fn upgraded_example() -> String {
    let stamp = Stamp {
        by: UPGRADE_EXAMPLE_BY.to_owned(),
        on: UPGRADE_EXAMPLE_ON,
    };
    migrate(UPGRADE_EXAMPLE, &CONFIG_FORMAT, &stamp)
        .expect("the upgrade example migrates")
        .text
        .expect("the upgrade example is behind the latest format")
}

pub fn generate() -> String {
    let mut out = page_header(
        "Configuration",
        "caudra.toml settings, experimental features, separate profile files, and directory layout.",
    );

    writeln!(
        out,
        "\
Settings go in `caudra.toml`. It has two places, and both are optional:

- **Global**: `~/.config/caudra/caudra.toml`
- **Project**: `.caudra/caudra.toml` (relative to your working directory)

When both exist, project settings override global ones field by field. A few settings are global-only, and their descriptions say so. Caudra notices when either file changes while it runs, see [Hot reload](#hot-reload). `/reload` reads both files again, except for the [experimental switches](#experimental-features), which apply from startup.

In the terminal UI, `/settings` edits most tables of the global `caudra.toml` for you and keeps your comments. Each key shows its description, its allowed values and default, and when an edit applies. Its `[?]` opens the key's section of this page, and `/` filters every tab by key or table name.

Settings apply in this order, and each layer overrides the ones before it:

1. Built-in defaults
2. Global `caudra.toml`
3. Global `init.lua`, only with Lua plugins turned on
4. Project `.caudra/caudra.toml`
5. Project `.caudra/init.lua`, only with Lua plugins turned on
6. Command-line flags

Remote sessions load only the client's global configuration. They skip both project layers, project environment files, and project MCP configuration. Remote project context uses a bounded declarative asset manifest instead. See [Remote Workspaces](/docs/remote-workspaces/#project-context-and-trust).
"
    )
    .unwrap();
    write_config_files_section(&mut out);
    writeln!(
        out,
        "
## Example

```toml
[ui]
splash_animation = true
mouse_scroll_lines = {mouse_scroll}
theme = \"tokyonight\"

[ui.tool_output_lines]
bash = {tol_bash}
read = {tol_read}

[agent]
max_output_lines = {max_output_lines}

[provider]
default_model = \"anthropic/claude-sonnet-4-6\"
allowed_models = [\"anthropic/*\", \"openai/gpt-5\"]
excluded_models = [\"*/*-preview\"]

[storage]
max_log_files = {max_log_files}

[plugins.bash]
timeout_secs = 180

[plugins.index]
max_file_size_mb = 4
```

All fields are optional. A file starts with `version = {version}`, and Caudra upgrades one without it, see [Config file versions](#config-file-versions). Typos in field names and values of the wrong type cause an error right away, with the file and line.

For every setting in one file, with its type, default, and description, see [Reference configs]({reference}) or run [`caudra config example`](/docs/cli/#caudra-config).

`provider.allowed_models` is a list of glob patterns for qualified `provider/model-id` specs. `*` also matches `/`, so `opencode/*` includes nested model IDs. When the list is empty or omitted, every model is allowed. `provider.excluded_models` removes matching models after that, so exclusions always win. A project list replaces the matching global list. Omit it to inherit, or use `[]` to clear it. The policy applies to selectors, CLI and API model changes, delegation, and `caudra models`.
",
        mouse_scroll = DEFAULT_MOUSE_SCROLL_LINES + 2,
        tol_bash = ToolOutputLines::DEFAULT.bash + 3,
        tol_read = ToolOutputLines::DEFAULT.read + 2,
        max_output_lines = DEFAULT_MAX_OUTPUT_LINES + 1000,
        max_log_files = DEFAULT_MAX_LOG_FILES / 2,
        version = CONFIG_VERSION,
        reference = gen_reference_configs::link(&files::CAUDRA),
    )
    .unwrap();
    out.push_str(ENVIRONMENT_SECTION);
    out.push_str(FILES_SECTION);
    out.push_str(&hot_reload_section());
    write_experimental_section(&mut out);
    write_migration_section(&mut out);

    writeln!(out, "## Full Reference\n").unwrap();
    writeln!(out, "### Top-level\n").unwrap();
    write_table(&mut out, TOP_LEVEL_FIELDS);
    writeln!(out).unwrap();

    write_section(&mut out, "ui", UiConfig::FIELDS);
    write_theme_section(&mut out);
    write_update_check_section(&mut out);
    write_tool_output_section(&mut out);
    write_section(&mut out, "agent", AgentConfig::FIELDS);
    write_section(&mut out, "agent.messaging", MessagingConfig::FIELDS);
    write_shell_env_section(&mut out);
    write_steering_section(&mut out);
    write_section(&mut out, "provider", ProviderConfig::FIELDS);
    write_section(&mut out, "storage", StorageConfig::FIELDS);
    write_retention_section(&mut out);
    write_snapshots_section(&mut out);
    write_telemetry_section(&mut out);
    write_worktrees_section(&mut out);
    write_websearch_section(&mut out);
    write_decisions_section(&mut out);
    write_automations_section(&mut out);
    write_autopilot_section(&mut out);
    write_web_section(&mut out);

    writeln!(out, "## Plugins\n").unwrap();
    writeln!(
        out,
        "The `plugins` table turns bundled features and plugins on or off and passes options to \
         them. All bundled features are on by default. Set \
         `enabled = false` to turn one off.\n\n\
         Each feature checks its own options at startup. A typo, a wrong \
         type, or an unknown plugin name gives you a clear error right \
         away.\n\n\
         `enabled = false` turns off the tools that key produced, under the \
         names they are registered with today, so `plugins.bash` turns off \
         `shell` and `plugins.edit` turns off `file_edit` and \
         `file_apply_patch`. To name a tool directly, use \
         `agent.disabled_tools`, described in \
         [Disabling tools](/docs/tools/#disabling-tools).\n\n\
         The edit plugin's extra tools are options too: \
         `plugins.edit = {{ multiedit = false, insert_lines = true }}`.\n\n\
         This table is for bundled plugins only, and it works without Lua. Your own plugins go \
         in `~/.config/caudra/lua/` and need `lua_plugins` turned on. See \
         [Plugins](/docs/plugins/).\n"
    )
    .unwrap();
    writeln!(
        out,
        "\
```toml
[plugins.bash]
timeout_secs = 180

[plugins.websearch]
enabled = false
```\n"
    )
    .unwrap();

    write_plugin_options(&mut out);

    writeln!(out, "## Validation\n").unwrap();
    writeln!(
        out,
        "If a value is below its minimum or above its maximum, Caudra shows a `ConfigError` with \
         the field name, the value, and the bound it crossed."
    )
    .unwrap();

    writeln!(
        out,
        "
## Directory layout

Caudra follows platform directory conventions. On Linux and macOS that is XDG. On Windows, config, data, state, and logs all live under Roaming AppData (Windows has no separate state dir in this layout).

| Purpose | Linux / macOS | Windows |
|---------|---------------|---------|
| Config | `~/.config/caudra/` | `%APPDATA%\\caudra\\` |
| Data | `~/.local/share/caudra/` | `%APPDATA%\\caudra\\` |
| State | `~/.local/state/caudra/` | `%APPDATA%\\caudra\\` |
| Logs | `~/.local/logs/caudra/` | `%APPDATA%\\caudra\\` |
| Cache | `~/.cache/caudra/` | `%LOCALAPPDATA%\\caudra\\` |
| Scratch | `$TMPDIR/caudra/` | `%TEMP%\\caudra\\` |

Config holds {config_files}. State holds sessions, the images and PDFs they refer to in `media/`, auth tokens, memories, plans, model-job bindings, sandbox lifecycle records, transfer recovery journals, and the [backups](#backups) Caudra saves before it upgrades a file. The install script puts the binary under `%LOCALAPPDATA%\\caudra` on Windows; that is separate from these runtime dirs.

Scratch holds work that belongs outside your project, such as a file the model writes while thinking or a temporary a command leaves behind. Each project gets its own subdirectory, named by the project directory plus a three-word phrase derived from its path, as in `caudra-heroic-easy-grouse`. Two checkouts sharing a name get different phrases, so they cannot overwrite each other. The phrase is derived rather than drawn at random, so a project returns to the same directory on every run. Caudra creates it at startup and points `TMPDIR`, `TMP`, and `TEMP` at it, so every command Caudra runs puts its own temporary files there instead of the shared temp root. The model is told the path, and writing anywhere under the scratch root needs no approval. Paths beside the root still ask.

The choice of subdirectory is fixed when Caudra starts. `/cd` moves the project without moving it, which is why approval covers the whole root rather than one project's share of it. Directories are owner-only, and Caudra refuses to use one that turns out to be a symlink. Nothing sweeps them, so they live as long as your system keeps its temp root.

State that belongs to one project sits under `…/state/caudra/projects/<project-id>/`, where the id is the project directory name plus a hash of its path. Memory notes and plan-mode documents both live there, so removing that directory clears everything Caudra kept for the project.

Development builds compiled with debug assertions use `caudra-debug` for every platform directory. This keeps global config, sessions, auth, logs, and caches separate from release builds. Per-project `.caudra/` directories remain shared.

Set `CAUDRA_NAMESPACE` to choose the directory name yourself instead of letting the build profile pick it. The value is a single directory name, so `CAUDRA_NAMESPACE=caudra-review` reads and writes `~/.config/caudra-review/`, `~/.local/state/caudra-review/`, and the rest. Use it to give a run its own config and session store, or to point a debug build at your release directories. A value that cannot be a directory name, such as one holding a path separator or `..`, stops Caudra with an error rather than falling back. An empty value counts as unset. Per-project `.caudra/` directories are unaffected.

## Config file versions

Each TOML config file starts with a top-level `version`, the version of its format. A file without the key predates versioning, so Caudra reads it as version 1 and adds the key when it upgrades the file. `workcell.toml` and `sandboxes.toml` always required the key and reject a file without it. A build that finds a newer version than it reads refuses the file instead of guessing what it means, with an error that says the file needs a newer Caudra and where its backup is.

| File | Version | Without `version` | Newer than this build reads |
|------|---------|-------------------|-----------------------------|
{version_rows}
When a release changes a format, it raises the version and Caudra upgrades older files as it reads them. A renamed key or tool name gets its new name, and a removed key is commented out rather than deleted. If an old key and its new name are both set, or the upgraded file would not load, Caudra leaves the file unchanged and shows the error.

`init.lua`, which runs only with Lua plugins turned on, has no version because it is a script. To share one `init.lua` across releases, branch on [`caudra.version()`](/docs/lua-api/#caudra-version).

### Global and project files

At startup, Caudra rewrites each global file that is behind its format, after it saves a [backup](#backups). It does this only while no other Caudra process is running, because an older Caudra that is still open would refuse the upgraded file the next time it reloads it. While another session is open, Caudra reads the file upgraded in memory and leaves it unchanged on disk. A notice names the open sessions, and the next start tries again. Until then, saving to that file from inside Caudra fails with an error that says how to finish the upgrade. That covers `/env`, switching a feature in `/decisions`, turning a server on or off in `/mcp`, saving a key in `/login`, and the `/sandbox` manager.

Caudra never rewrites the files in a project's `.caudra/` directory on its own, because they are usually committed and your teammates may run an older Caudra. It reads them upgraded in memory and shows a notice that names the command to update them.

`caudra config migrate` upgrades the global files on demand, and `--project` adds the project files of the current directory. It prints what changed in each file and where the backup is. While another Caudra process is running it writes nothing, lists the open sessions, and exits with an error. `--dry-run` prints each change with a diff and writes nothing, so it works while sessions are open. After `--project`, teammates need the same Caudra release or a newer one to read the files.

### Upgrade notes

Every line the upgrade changes gets a comment above it with the Caudra release, the date, what changed, and why. The comments you wrote on that line stay below a lone `#`. A removed key stays as a commented-out line. For example, this file:

```toml
{upgrade_before}```

reads like this after the upgrade:

```toml
{upgrade_after}```

### Backups

Before Caudra rewrites a file, it copies the original byte for byte into `backups/` in the [state directory](#directory-layout). Backups are owner-only, and Caudra never deletes them. The time in a name is UTC, so a later upgrade never overwrites an earlier backup.

| Backup | Path under `backups/` |
|--------|-----------------------|
| A global config file | `config/global/<file>.v<from>.<YYYYMMDDTHHMMSSZ>.bak` |
| A project config file | `config/projects/<project-id>/<file>.v<from>.<YYYYMMDDTHHMMSSZ>.bak` |
| `caudra.db` before a schema upgrade | `caudra.db.v<from>.bak` |
| `caudra.db` before `caudra permissions repair-review --apply` | `caudra.db.permission-review-<ID>.bak` |

`<from>` is the version the file had, and `<project-id>` is the same id as in the project's state directory. Database backups that earlier releases made stay beside `caudra.db`.

A database backup from schema 44 on refers to its images and PDFs in `media/` rather than holding them. Prune never removes a media file that a backup refers to, so backups keep their media, and `caudra storage prune` reports the space that only backups keep. To restore such a backup, copy it over `caudra.db` in a state directory that still has the `media/` directory the backup was made with. Prune caches the hashes each backup refers to in `<backup>.media`, and deletes that file once you delete its backup.

### Rolling back

An older Caudra cannot read a file at a newer version. To go back to an older release, copy the backup over the file, for example:

```bash
cp ~/.local/state/caudra/backups/config/global/caudra.toml.v1.20261020T101500Z.bak ~/.config/caudra/caudra.toml
```

`caudra rollback` restores only the previous binary. The restored file loses any change made after the upgrade. The next time a newer Caudra starts with no other session open, it upgrades the file again.

## Personal Instructions

On top of the project instruction files Caudra loads from the git root down to the cwd (`AGENTS.md`, `CLAUDE.md`, and friends; see [Context](/docs/context/#instruction-files)), you can add:

- `AGENTS.local.md` in any of those project directories for per-directory preferences (gitignored)
- `~/.config/caudra/AGENTS.md` for preferences that apply to all projects

All of these are added to the system prompt at the start of every session.

## Memory

The `memory` tool keeps project notes as Markdown files under the state directory, scoped per project:

`…/state/caudra/projects/<project-id>/memories/`

That is `~/.local/state/caudra/…` on Linux and macOS and `%APPDATA%\\caudra\\…` on Windows. The Memory model summarizes the notes in the background for the view in the system prompt. Set [`agent.summarize_memory`](#agent) to `false` to stop that spend. [Memory](/docs/memory/) covers the journal, the view, and the `/memory` inspector.

Related pages: [Skills](/docs/skills/), [CLI](/docs/cli/), [Providers](/docs/providers/#providers-toml).",
        config_files = config_dir_files(),
        version_rows = version_rows(),
        upgrade_before = UPGRADE_EXAMPLE,
        upgrade_after = upgraded_example(),
    )
    .unwrap();

    out
}

#[cfg(test)]
mod tests {
    use caudra_config::config_file::{CONFIG_FORMAT, GlobalConfigFile};
    use caudra_config::interpolation::{ENV_OPEN, FILE_OPEN, Lookup};
    use caudra_config::migration;
    use caudra_config::{
        AgentConfig, ConfigValue, DecisionsConfig, Feature, SteeringConfig,
        decisions::RawDecisionsConfig,
    };
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{
        generate, write_decisions_section, write_experimental_section, write_steering_section,
    };

    const MODEL: &str = "provider/model";
    const HEADING: &str = "### `agent.steering`";
    const PROMPT_ROW: &str = "| `prompt` | string | unset |";
    const RULE_COUNT: usize = 9;
    const TOML_FENCE: &str = "```toml\n";
    const FENCE_END: &str = "```";
    /// A reference needs a real environment or file, and caudra-config tests
    /// those. The examples only have to be valid settings once it is gone.
    const REFERENCE_PLACEHOLDER: &str = "{";

    #[test]
    fn experimental_reference_lists_every_switch_off() {
        let mut reference = String::new();
        write_experimental_section(&mut reference);
        for feature in Feature::ALL {
            let row = format!("| `{}` | `false` |", feature.key());
            assert!(
                reference.lines().any(|line| line.starts_with(&row)),
                "{row}"
            );
        }
    }

    #[test]
    fn every_toml_example_is_a_valid_global_config() {
        let page = generate();
        let examples: Vec<&str> = page
            .split(TOML_FENCE)
            .skip(1)
            .filter_map(|rest| rest.split_once(FENCE_END).map(|(body, _)| body))
            .collect();
        assert!(!examples.is_empty());
        for example in examples {
            let shape = example
                .replace(ENV_OPEN, REFERENCE_PLACEHOLDER)
                .replace(FILE_OPEN, REFERENCE_PLACEHOLDER);
            let loaded = migration::upgrade_text(shape, &CONFIG_FORMAT)
                .unwrap_or_else(|error| panic!("{error}\n{example}"));
            if let Err(error) = GlobalConfigFile::parse(&loaded.text, Lookup::Refused) {
                panic!("{error}\n{example}");
            }
        }
    }

    #[test_case("base_url")]
    #[test_case("protocol")]
    #[test_case("model")]
    #[test_case("api_key_env")]
    #[test_case("allow_remote")]
    #[test_case("allow_http")]
    fn decision_reference_lists_no_connection_keys(field: &str) {
        let mut reference = String::new();
        write_decisions_section(&mut reference);
        let prefix = format!("| `{field}` |");
        assert!(!reference.lines().any(|line| line.starts_with(&prefix)));
    }

    #[test_case("off")]
    #[test_case("shadow")]
    #[test_case("advise")]
    #[test_case("enforce")]
    fn decision_reference_modes_match_validation(mode: &str) {
        let defaults = serde_json::to_value(DecisionsConfig::default().features).unwrap();
        let mut reference = String::new();
        write_decisions_section(&mut reference);
        for (feature, default) in defaults.as_object().unwrap() {
            let row = reference
                .lines()
                .find(|line| line.starts_with(&format!("| `{feature}` |")))
                .unwrap();
            assert_eq!(
                row.split('|').nth(2).unwrap().trim(),
                format!("`{}`", default.as_str().unwrap())
            );
            let documented = row
                .split('|')
                .nth(3)
                .unwrap()
                .contains(&format!("`{mode}`"));
            let raw: RawDecisionsConfig =
                serde_json::from_value(json!({"features": {feature: mode}})).unwrap();
            assert_eq!(documented, raw.resolve().is_ok(), "{feature}: {mode}");
        }
    }

    #[test]
    fn decision_reference_covers_threshold_defaults() {
        let defaults = serde_json::to_value(DecisionsConfig::default().thresholds).unwrap();
        let mut reference = String::new();
        write_decisions_section(&mut reference);
        for (field, value) in defaults.as_object().unwrap() {
            let default = if value.is_null() {
                ConfigValue::Unset.format_default()
            } else {
                format!("`{value}`")
            };
            let prefix = format!("| `{field}` | float | {default} |");
            assert!(
                reference.lines().any(|line| line.starts_with(&prefix)),
                "{field}"
            );
        }
    }

    #[test]
    fn steering_reference_matches_resolved_defaults() {
        let config = SteeringConfig::default();
        config.validate().unwrap();
        let policy = serde_json::to_value(config.resolve(MODEL)).unwrap();
        let mut reference = String::new();
        write_steering_section(&mut reference);
        assert_eq!(reference.matches(HEADING).count(), 1);
        assert!(
            AgentConfig::FIELDS
                .iter()
                .all(|field| field.name != "steering")
        );
        assert!(reference.lines().any(|line| line.starts_with(PROMPT_ROW)));

        for field in [
            "enabled",
            "max_recoveries",
            "max_advisories",
            "max_stalled_turns",
        ] {
            let row = reference
                .lines()
                .find(|line| line.starts_with(&format!("| `{field}` |")))
                .unwrap();
            assert_eq!(
                row.split('|').nth(3).unwrap().trim(),
                format!("`{}`", policy[field])
            );
        }

        let mut numeric_fields = 0;
        let rules = policy["rules"].as_object().unwrap();
        assert_eq!(rules.len(), RULE_COUNT);
        for (rule, fields) in rules {
            assert!(
                reference
                    .lines()
                    .any(|line| line.starts_with(&format!("| `{rule}` |")))
            );
            for (field, value) in fields.as_object().unwrap() {
                match field.as_str() {
                    "enabled" => assert_eq!(value, &Value::Bool(true)),
                    "prompt" => assert_eq!(value, &Value::Null),
                    _ => {
                        assert!(value.is_number());
                        let prefix = format!("| `{rule}.{field}` | `{value}` |");
                        assert!(reference.lines().any(|line| line.starts_with(&prefix)));
                        numeric_fields += 1;
                    }
                }
            }
        }
        assert_eq!(
            reference
                .lines()
                .filter(
                    |line| line.starts_with("| `") && line.split('|').nth(1).unwrap().contains('.')
                )
                .count(),
            numeric_fields
        );
    }

    #[test_case("/max_recoveries", 32; "combined_recoveries")]
    #[test_case("/max_advisories", 4; "combined_advisories")]
    #[test_case("/rules/truncation/max_attempts", 3; "truncation_attempts")]
    #[test_case("/rules/abandoned_turn/max_attempts", 2; "abandoned_turn_attempts")]
    fn steering_numeric_defaults(path: &str, expected: u64) {
        let policy = serde_json::to_value(SteeringConfig::default().resolve(MODEL)).unwrap();
        assert_eq!(policy.pointer(path).and_then(Value::as_u64), Some(expected));
    }
}
