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
use serde_json::json;

use crate::cli::{Cli, DecisionAction};

pub(super) fn run(action: DecisionAction, cli: &Cli) -> Result<()> {
    cli.startup.features.require(Feature::DecisionEngine)?;
    if cli.workcell.is_set() || cli.ephemeral {
        bail!("decision log administration requires local persistent storage");
    }
    let storage = StateDir::resolve_without_create().context("resolve decision log directory")?;
    if matches!(action, DecisionAction::Status) {
        let decisions = configuration(cli)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "configured": decisions.base_url.is_some(),
                "endpoint_origin": decisions.base_url.as_ref().map(|url| url.origin().ascii_serialization()),
                "model": decisions.model,
                "timeout_ms": decisions.timeout_ms,
                "logging": decisions.log,
                "retention_days": decisions.log_retention_days,
                "features": decisions.features,
                "reachability": "not_probed",
            }))?
        );
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
