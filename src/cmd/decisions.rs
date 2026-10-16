use std::{env, fmt::Write as _, io, sync::Arc};

use caudra_agent::decisions::{resolve_active_decisions, stats_thresholds, use_decision_endpoint};
use caudra_agent::tools::ToolRegistry;
use caudra_config::decisions::DecisionsConfig;
use caudra_config::global_env_value;
use caudra_config::providers::ProvidersConfig;
use caudra_storage::{
    StateDir,
    auth::load_decision_credentials,
    decision_log::{DecisionFilter, DecisionLog},
    decisions::read_active_decision_endpoint,
};
use color_eyre::{
    Result,
    eyre::{Context, bail},
};
use serde_json::{Value, json};

use crate::cli::{Cli, DecisionAction};

pub(super) const DECISIONS_OFF: &str = "off";
pub(super) const DECISION_LOGIN_HINT: &str = "run: caudra auth login --decision";
const INVALID_HOST: &str = "(invalid)";
const KEY_SAVED: &str = "saved";
const KEY_ENV_PREFIX: &str = "env:";
const KEY_NONE: &str = "none";
const ACTIVE_COLUMN: &str = "active";
const LIST_HEADINGS: [&str; 6] = ["NAME", "PROTOCOL", "MODEL", "HOST", "KEY", "ACTIVE"];
const LIST_COLUMN_GAP: &str = "  ";

/// Where an endpoint's key comes from, as `auth status` and `decisions list` show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DecisionKey {
    /// The masked stored key.
    Saved(String),
    /// The environment variable that holds it.
    Env(String),
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DecisionRow {
    pub name: String,
    pub protocol: &'static str,
    pub model: String,
    pub host: String,
    pub key: DecisionKey,
    pub active: bool,
}

pub(super) fn run(action: DecisionAction, cli: &Cli) -> Result<()> {
    if cli.workcell.is_set() || cli.ephemeral {
        bail!("decision commands require local persistent storage");
    }
    match action {
        DecisionAction::Status => {
            let storage = read_only_storage()?;
            let decisions =
                resolve_active_decisions(&configuration(cli)?, &storage, ProvidersConfig::load)
                    .context("resolve the active decision endpoint")?;
            println!("{}", serde_json::to_string_pretty(&status(&decisions))?);
        }
        DecisionAction::List => {
            let rows = decision_rows(&read_only_storage()?, &ProvidersConfig::load())?;
            print!("{}", render_list(&rows));
        }
        DecisionAction::Use { name } => {
            let storage = StateDir::resolve().context("resolve state directory")?;
            let name = name.trim().to_ascii_lowercase();
            let name = (name != DECISIONS_OFF).then_some(name);
            match use_decision_endpoint(&storage, name.as_deref(), ProvidersConfig::load)? {
                Some(endpoint) => println!(
                    "Active decision endpoint: {} ({} · {}). New sessions screen with it.",
                    endpoint.name,
                    endpoint.protocol.as_str(),
                    endpoint.model
                ),
                None => println!("Decisions off."),
            }
        }
        DecisionAction::Stats { feature } => {
            let config = configuration(cli)?;
            let filter = DecisionFilter {
                feature: feature.as_deref(),
                ..Default::default()
            };
            let stats = DecisionLog::open_read_only(&read_only_storage()?)
                .context("open existing decision log")?
                .map(|log| {
                    log.stats(&filter, |feature| {
                        stats_thresholds(&config.thresholds, feature)
                    })
                })
                .transpose()?
                .unwrap_or_default();
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
        DecisionAction::Export { feature } => {
            if let Some(log) = DecisionLog::open_read_only(&read_only_storage()?)
                .context("open existing decision log")?
            {
                log.export_jsonl(io::stdout().lock(), feature.as_deref())?;
            }
        }
        DecisionAction::Purge { yes } => {
            if !yes {
                bail!("deleting the decision log requires --yes");
            }
            if let Some(log) = DecisionLog::open_existing(&read_only_storage()?)
                .context("open existing decision log")?
            {
                log.purge()?;
            }
            println!("Decision log purged.");
        }
    }
    Ok(())
}

fn read_only_storage() -> Result<StateDir> {
    StateDir::resolve_without_create().context("resolve decision log directory")
}

fn configuration(cli: &Cli) -> Result<DecisionsConfig> {
    let host = super::cli_plugin_host(cli, Arc::clone(ToolRegistry::global_arc()))?;
    let cwd = env::current_dir().context("resolve working directory")?;
    Ok(super::load_config(&host, cli, &cwd, false)?.decisions)
}

/// The stored key wins over the endpoint's `api_key_env`, as at runtime.
pub(super) fn decision_key(
    storage: &StateDir,
    name: &str,
    api_key_env: Option<&str>,
) -> Result<DecisionKey> {
    if let Some(credentials) = load_decision_credentials(storage, name)
        .with_context(|| format!("load the key for decision endpoint '{name}'"))?
    {
        return Ok(DecisionKey::Saved(credentials.masked_api_key()));
    }
    Ok(match api_key_env {
        Some(variable) if global_env_value(variable).ok().flatten().is_some() => {
            DecisionKey::Env(variable.into())
        }
        _ => DecisionKey::Missing,
    })
}

/// Every configured endpoint. One that does not resolve still lists, with
/// its host shown as invalid, so `decisions use` can name the problem.
pub(super) fn decision_rows(
    storage: &StateDir,
    providers: &ProvidersConfig,
) -> Result<Vec<DecisionRow>> {
    let active = read_active_decision_endpoint(storage);
    providers
        .decisions
        .iter()
        .map(|(name, def)| {
            let host = def.resolve(name).ok().map_or_else(
                || INVALID_HOST.to_owned(),
                |endpoint| {
                    let host = endpoint.base_url.host_str().unwrap_or(INVALID_HOST);
                    match endpoint.base_url.port() {
                        Some(port) => format!("{host}:{port}"),
                        None => host.to_owned(),
                    }
                },
            );
            Ok(DecisionRow {
                name: name.clone(),
                protocol: def.protocol.as_str(),
                model: def
                    .model
                    .clone()
                    .unwrap_or_else(|| def.protocol.default_model().into()),
                host,
                key: decision_key(storage, name, def.api_key_env.as_deref())?,
                active: active.as_deref() == Some(name),
            })
        })
        .collect()
}

fn render_list(rows: &[DecisionRow]) -> String {
    if rows.is_empty() {
        return format!("No decision endpoints configured; {DECISION_LOGIN_HINT}\n");
    }
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|row| {
            [
                row.name.clone(),
                row.protocol.to_owned(),
                row.model.clone(),
                row.host.clone(),
                match &row.key {
                    DecisionKey::Saved(_) => KEY_SAVED.to_owned(),
                    DecisionKey::Env(variable) => format!("{KEY_ENV_PREFIX}{variable}"),
                    DecisionKey::Missing => KEY_NONE.to_owned(),
                },
                if row.active { ACTIVE_COLUMN } else { "" }.to_owned(),
            ]
        })
        .collect();
    let widths: [usize; 6] = std::array::from_fn(|column| {
        cells
            .iter()
            .map(|row| row[column].len())
            .chain([LIST_HEADINGS[column].len()])
            .max()
            .unwrap_or_default()
    });
    let mut output = String::new();
    let headings = LIST_HEADINGS.map(str::to_owned);
    for row in [&headings].into_iter().chain(&cells) {
        let line = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect::<Vec<_>>()
            .join(LIST_COLUMN_GAP);
        let _ = writeln!(output, "{}", line.trim_end());
    }
    output
}

fn status(decisions: &DecisionsConfig) -> Value {
    let endpoint = decisions.endpoint.as_ref();
    json!({
        "configured": endpoint.is_some(),
        "endpoint": endpoint.map(|endpoint| &endpoint.name),
        "endpoint_origin": endpoint.map(|endpoint| endpoint.base_url.origin().ascii_serialization()),
        "protocol": endpoint.map(|endpoint| endpoint.protocol.as_str()),
        "model": endpoint.map(|endpoint| &endpoint.model),
        "timeout_ms": decisions.timeout_ms,
        "rerank_timeout_ms": decisions.rerank_timeout_ms,
        "logging": decisions.log,
        "retention_days": decisions.log_retention_days,
        "features": decisions.features,
        "reachability": "not_probed",
    })
}

#[cfg(test)]
mod tests {
    use caudra_config::decisions::{
        DECISION_PRESETS, DecisionEndpoint, DecisionEndpointDef, DecisionProtocol, DecisionsConfig,
    };
    use caudra_storage::auth::save_decision_credentials;
    use caudra_storage::decisions::persist_active_decision_endpoint;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const NAME: &str = "tunnel";
    const BASE_URL: &str = "https://private-user:private-password@example.com/private-path?private-query#private-fragment";
    const ORIGIN: &str = "https://example.com";
    const NOT_PROBED: &str = "not_probed";
    const TUNNEL_BASE_URL: &str = "http://100.64.0.2:8080/v1";
    const TUNNEL_HOST: &str = "100.64.0.2:8080";
    const TUNNEL_MODEL: &str = "systemone-decision-v7";
    const STORED_KEY: &str = "stored-decision-key";
    const UNSET_VARIABLE: &str = "TEST_CAUDRA_DECISION_LIST_UNSET_KEY";
    const EXPECTED_LIST: &str = "\
NAME      PROTOCOL  MODEL                  HOST             KEY    ACTIVE
openai    openai    gpt-6-luna             api.openai.com   none
tunnel    openai    systemone-decision-v7  100.64.0.2:8080  saved  active
typesafe  typesafe  jev-latest             (invalid)        none
";

    fn providers() -> ProvidersConfig {
        let mut providers = ProvidersConfig::default();
        for preset in &DECISION_PRESETS {
            let mut def = preset.def();
            def.api_key_env = Some(UNSET_VARIABLE.into());
            providers.decisions.insert(preset.name.into(), def);
        }
        providers
            .decisions
            .get_mut(DECISION_PRESETS[0].name)
            .unwrap()
            .base_url = TUNNEL_BASE_URL.into();
        providers.decisions.insert(
            NAME.into(),
            DecisionEndpointDef {
                protocol: DecisionProtocol::OpenAI,
                base_url: TUNNEL_BASE_URL.into(),
                model: Some(TUNNEL_MODEL.into()),
                api_key_env: None,
                allow_http: Some(true),
            },
        );
        providers
    }

    #[test]
    fn list_renders_every_endpoint_with_its_key_and_active_marker() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        save_decision_credentials(&dir, NAME, STORED_KEY).unwrap();
        persist_active_decision_endpoint(&dir, Some(NAME)).unwrap();

        let rows = decision_rows(&dir, &providers()).unwrap();

        assert_eq!(
            rows.iter().find(|row| row.name == NAME).unwrap().host,
            TUNNEL_HOST
        );
        assert_eq!(render_list(&rows), EXPECTED_LIST);
    }

    #[test]
    fn list_without_endpoints_points_at_login() {
        assert!(render_list(&[]).contains(DECISION_LOGIN_HINT));
    }

    #[test]
    fn status_without_an_endpoint_is_off() {
        let config = DecisionsConfig::default();

        let result = status(&config);

        assert_eq!(result["timeout_ms"], config.timeout_ms);
        assert_eq!(result["rerank_timeout_ms"], config.rerank_timeout_ms);
        assert_eq!(result["configured"], false);
        assert!(result["endpoint"].is_null());
        assert!(result["endpoint_origin"].is_null());
        assert!(result["protocol"].is_null());
        assert_eq!(result["reachability"], NOT_PROBED);
    }

    #[test_case(DecisionProtocol::TypeSafe, "typesafe"; "typesafe")]
    #[test_case(DecisionProtocol::OpenAI, "openai"; "openai")]
    fn status_exposes_only_the_endpoint_origin(protocol: DecisionProtocol, expected: &str) {
        let config = DecisionsConfig::default().with_endpoint(Some(DecisionEndpoint::new(
            NAME,
            protocol,
            BASE_URL.parse().unwrap(),
        )));

        let result = status(&config);

        assert_eq!(result["configured"], true);
        assert_eq!(result["endpoint"], NAME);
        assert_eq!(result["protocol"], expected);
        assert_eq!(result["endpoint_origin"], ORIGIN);
        assert!(!result.to_string().contains("private-"));
        assert_eq!(result["reachability"], NOT_PROBED);
    }
}
