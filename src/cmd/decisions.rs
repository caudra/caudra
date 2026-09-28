use std::{env, io, sync::Arc};

use caudra_agent::tools::ToolRegistry;
use caudra_config::decisions::{DecisionThresholds, DecisionsConfig};
use caudra_lua::PluginHost;
use caudra_storage::{
    StateDir,
    decision_log::{DecisionLog, StatsThresholds},
};
use color_eyre::{
    Result,
    eyre::{Context, bail},
};
use serde_json::json;

use crate::cli::{Cli, DecisionAction};

pub(super) fn run(action: DecisionAction, cli: &Cli) -> Result<()> {
    if cli.workcell.is_set() || cli.ephemeral {
        bail!("decision log administration requires local persistent storage");
    }
    let storage = StateDir::resolve_without_create().context("resolve decision log directory")?;
    if matches!(action, DecisionAction::Status) {
        let decisions = configuration(cli)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "configured": decisions.endpoint.is_some(),
                "endpoint_origin": decisions.endpoint.as_ref().map(|url| url.origin().ascii_serialization()),
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
    let log = DecisionLog::open_existing(&storage).context("open existing decision log")?;
    match action {
        DecisionAction::Stats { feature } => {
            let config = configuration(cli)?;
            let stats = log
                .as_ref()
                .map(|log| {
                    let rows = log.stats(feature.as_deref(), &StatsThresholds::default())?;
                    rows.into_iter()
                        .map(|row| {
                            log.stats(
                                Some(&row.feature),
                                &stats_thresholds(&config.thresholds, &row.feature),
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()
                        .map(|rows| rows.into_iter().flatten().collect::<Vec<_>>())
                })
                .transpose()?
                .unwrap_or_default();
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
        DecisionAction::Export { feature } => {
            if let Some(log) = log {
                log.export_jsonl(io::stdout().lock(), feature.as_deref())?;
            }
        }
        DecisionAction::Purge { yes } => {
            if !yes {
                bail!("deleting the decision log requires --yes");
            }
            if let Some(log) = log {
                log.purge()?;
            }
            println!("Decision log purged.");
        }
        DecisionAction::Status => {}
    }
    Ok(())
}

fn configuration(cli: &Cli) -> Result<DecisionsConfig> {
    let host = PluginHost::with_jit(Arc::clone(ToolRegistry::global_arc()), !cli.no_jit)
        .context("initialize config host")?;
    let cwd = env::current_dir().context("resolve working directory")?;
    Ok(super::load_config(&host, cli, &cwd, false)?.decisions)
}

fn stats_thresholds(config: &DecisionThresholds, feature: &str) -> StatsThresholds {
    let mut thresholds = StatsThresholds::default();
    let overrides = &mut thresholds.noul_by_question;
    match feature {
        "permission" | "auto" => {
            let threshold = if feature == "auto" {
                config.auto_flag
            } else {
                config.permission_flag
            };
            for flag in [
                "deletes",
                "uploads",
                "credentials",
                "permissions",
                "remote_rewrite",
                "off_task",
            ] {
                overrides.insert(flag.into(), threshold);
            }
        }
        "content" => {
            overrides.insert("injection".into(), config.content_injection);
            overrides.insert(
                "addressed_to_agent".into(),
                config.content_addressed_to_agent,
            );
        }
        "shell_duration" => {
            overrides.insert("endless".into(), config.shell_endless);
            overrides.insert("heavy".into(), config.shell_heavy);
        }
        "shell_effect" => {
            if let Some(threshold) = config.shell_writes {
                overrides.insert("writes_project_files".into(), threshold);
            }
        }
        _ => {}
    }
    thresholds
}

#[cfg(test)]
mod tests {
    use super::stats_thresholds;
    use caudra_config::decisions::DecisionThresholds;
    use caudra_storage::decision_log::StatsThresholds;
    use test_case::test_case;

    #[test_case("permission", 0.8; "permission")]
    #[test_case("auto", 0.9; "auto")]
    fn configured_thresholds_do_not_reinterpret_approval_labels(feature: &str, expected: f64) {
        let config = DecisionThresholds {
            permission_flag: 0.8,
            auto_flag: 0.9,
            ..Default::default()
        };
        let thresholds = stats_thresholds(&config, feature);
        assert_eq!(thresholds.noul_by_question["uploads"], expected);
        assert!(!thresholds.noul_by_question.contains_key("user_approves"));
        assert_eq!(
            thresholds.default_noul,
            StatsThresholds::default().default_noul
        );
    }
}
