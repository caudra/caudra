use std::{env, io, sync::Arc};

use caudra_agent::decisions::stats_thresholds;
use caudra_agent::tools::ToolRegistry;
use caudra_config::Feature;
use caudra_config::decisions::DecisionsConfig;
use caudra_storage::{
    StateDir,
    decision_log::{DecisionFilter, DecisionLog},
};
use color_eyre::{
    Result,
    eyre::{Context, bail},
};
use serde_json::{Value, json};

use crate::cli::{Cli, DecisionAction};

pub(super) fn run(action: DecisionAction, cli: &Cli) -> Result<()> {
    cli.startup.features.require(Feature::DecisionEngine)?;
    if cli.workcell.is_set() || cli.ephemeral {
        bail!("decision log administration requires local persistent storage");
    }
    let storage = StateDir::resolve_without_create().context("resolve decision log directory")?;
    if matches!(action, DecisionAction::Status) {
        let decisions = configuration(cli)?;
        println!("{}", serde_json::to_string_pretty(&status(&decisions))?);
        return Ok(());
    }
    match action {
        DecisionAction::Stats { feature } => {
            let config = configuration(cli)?;
            let filter = DecisionFilter {
                feature: feature.as_deref(),
                ..Default::default()
            };
            let stats = DecisionLog::open_read_only(&storage)
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
            if let Some(log) =
                DecisionLog::open_read_only(&storage).context("open existing decision log")?
            {
                log.export_jsonl(io::stdout().lock(), feature.as_deref())?;
            }
        }
        DecisionAction::Purge { yes } => {
            if !yes {
                bail!("deleting the decision log requires --yes");
            }
            if let Some(log) =
                DecisionLog::open_existing(&storage).context("open existing decision log")?
            {
                log.purge()?;
            }
            println!("Decision log purged.");
        }
        DecisionAction::Status => {}
    }
    Ok(())
}

fn configuration(cli: &Cli) -> Result<DecisionsConfig> {
    let host = super::cli_plugin_host(cli, Arc::clone(ToolRegistry::global_arc()))?;
    let cwd = env::current_dir().context("resolve working directory")?;
    Ok(super::load_config(&host, cli, &cwd, false)?.decisions)
}

fn status(decisions: &DecisionsConfig) -> Value {
    json!({
        "configured": decisions.base_url.is_some(),
        "endpoint_origin": decisions.base_url.as_ref().map(|url| url.origin().ascii_serialization()),
        "protocol": decisions.protocol.as_str(),
        "model": decisions.model,
        "timeout_ms": decisions.timeout_ms,
        "logging": decisions.log,
        "retention_days": decisions.log_retention_days,
        "features": decisions.features,
        "reachability": "not_probed",
    })
}

#[cfg(test)]
mod tests {
    use caudra_config::decisions::{DecisionProtocol, DecisionsConfig};
    use test_case::test_case;

    use super::status;

    const BASE_URL: &str = "https://private-user:private-password@example.com/private-path?private-query#private-fragment";
    const ORIGIN: &str = "https://example.com";
    const NOT_PROBED: &str = "not_probed";

    #[test_case(DecisionProtocol::TypeSafe, "typesafe"; "typesafe")]
    #[test_case(DecisionProtocol::OpenAI, "openai"; "openai")]
    fn status_shows_protocol_without_an_endpoint(protocol: DecisionProtocol, expected: &str) {
        let config = DecisionsConfig {
            protocol,
            ..Default::default()
        };

        let result = status(&config);

        assert_eq!(result["protocol"], expected);
        assert_eq!(result["configured"], false);
        assert!(result["endpoint_origin"].is_null());
        assert_eq!(result["reachability"], NOT_PROBED);
    }

    #[test_case(DecisionProtocol::TypeSafe; "typesafe")]
    #[test_case(DecisionProtocol::OpenAI; "openai")]
    fn status_exposes_only_the_endpoint_origin(protocol: DecisionProtocol) {
        let config = DecisionsConfig {
            protocol,
            base_url: Some(BASE_URL.parse().unwrap()),
            ..Default::default()
        };

        let result = status(&config);

        assert_eq!(result["configured"], true);
        assert_eq!(result["endpoint_origin"], ORIGIN);
        assert!(!result.to_string().contains("private-"));
        assert_eq!(result["reachability"], NOT_PROBED);
    }
}
