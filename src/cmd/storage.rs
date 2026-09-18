use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::sync::Arc;

use caudra_agent::snapshots::{SESSION_SNAPSHOTS_DIR, SnapshotStore, StoreEntry};
use caudra_agent::tools::ToolRegistry;
use caudra_config::{RetentionConfig, load_env_files};
use caudra_lua::PluginHost;
use caudra_providers::{format_hit_rate, format_tokens_u64};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::retention::{
    Decision, Duration as RetentionDuration, GroupBy, KeepPolicy, SessionFacts,
};
use caudra_storage::sessions::sweep::{
    self, Action, ExecuteReport, OutcomeKind, Plan, PruneReport,
};
use caudra_storage::sessions::{SESSIONS_DB_FILE, SessionDatabase, UsageBucket, cache_hit_rate};
use caudra_storage::tool_ledger::ToolLedger;
use caudra_storage::usage_ledger::UsageLedger;
use color_eyre::Result;
use color_eyre::eyre::{Context, bail, eyre};
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};
use serde::Serialize;

use crate::cli::{KeepPolicyArgs, PolicyScopeArgs, StorageAction, UsageGrouping};

const ID_WIDTH: usize = 22;
const ACTIVITY_WIDTH: usize = 16;
const SIZE_WIDTH: usize = 10;
const OBJECTS_WIDTH: usize = 9;
const MANIFESTS_WIDTH: usize = 6;
const NO_SNAPSHOTS: &str = "no workspace snapshots";
const ORPHANED_STORE: &str = "(workspace root missing)";
const ID_POLICY_CONFLICT: &str = "session IDs and --keep-* rules cannot be combined";
const REASONS_WIDTH: usize = 24;
const TITLE_WIDTH: usize = 40;
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M";
const UNGROUPED: &str = "all sessions";
const EMPTY_POLICY: &str = "refusing to act on an empty policy; pass --keep-* rules, configure \
    storage.retention, or combine --unsafe-allow-remove-all with --directory";
const BYTE_UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
const GROUP_WIDTH: usize = 34;
const TOKEN_WIDTH: usize = 14;
const RATE_WIDTH: usize = 5;
const COST_WIDTH: usize = 12;
const DAY_FORMAT: &str = "%Y-%m-%d";
const MONTH_FORMAT: &str = "%Y-%m";
const USAGE_TOTAL: &str = "all recorded spend";
const UNREPRESENTABLE_DURATION: &str = "duration does not land in the representable range";

/// One aggregated line of `storage usage`.
#[derive(Serialize, Default)]
struct UsageRow {
    group: String,
    input: u64,
    output: u64,
    cache_creation: u64,
    cache_read: u64,
    cost: f64,
    /// What a subscription covered, at API list rates. Kept out of `cost`,
    /// which is money someone was invoiced for.
    subscription_cost: f64,
    /// Spend from `--ephemeral` runs, already counted in `cost` or
    /// `subscription_cost` depending on who paid for it.
    ephemeral_cost: f64,
    priced_turns: u64,
    /// Turns that spent tokens on a model with no price, so `cost` understates
    /// by an unknown amount rather than by zero.
    unpriced_turns: u64,
    /// Share of prompt tokens served from cache, or `None` when the group had
    /// none to score. Derived, and filled once the fold is complete.
    cache_hit_rate: Option<f64>,
}

#[derive(Serialize)]
struct SessionRow<'a> {
    #[serde(flatten)]
    facts: &'a SessionFacts,
    artifact_bytes: u64,
    trimmed: bool,
}

#[derive(Serialize)]
struct PlanDocument<'a> {
    dry_run: bool,
    plan: &'a Plan,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcomes: Option<&'a ExecuteReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prune: Option<&'a PruneReport>,
}

pub fn run(action: StorageAction, no_plugins: bool, no_jit: bool) -> Result<()> {
    let state_dir = StateDir::resolve().context("resolve state directory")?;
    match action {
        StorageAction::Path => {
            println!("{}", state_dir.path().join(SESSIONS_DB_FILE).display());
        }
        StorageAction::Stats { json } => {
            let database = SessionDatabase::open_read_only(&state_dir)
                .context("open session database read-only")?;
            let stats = database
                .stats()
                .context("read session storage statistics")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!("database_bytes: {}", bytes(stats.database_bytes));
                println!("wal_bytes: {}", bytes(stats.wal_bytes));
                println!("shm_bytes: {}", bytes(stats.shm_bytes));
                println!("page_size: {}", stats.page_size);
                println!("page_count: {}", stats.page_count);
                println!("freelist_count: {}", stats.freelist_count);
                println!("auto_vacuum: {}", stats.auto_vacuum);
                println!("schema_version: {}", stats.schema_version);
                println!("sessions: {}", stats.session_count);
                println!("pinned_sessions: {}", stats.pinned_count);
                println!("trimmed_sessions: {}", stats.trimmed_count);
                println!("history_items: {}", stats.history_item_count);
                println!("tool_outputs: {}", stats.tool_output_count);
                println!("subagent_items: {}", stats.subagent_item_count);
                println!("logical_bytes: {}", bytes(stats.logical_bytes));
                println!(
                    "tool_output_file_bytes: {}",
                    bytes(stats.tool_output_file_bytes)
                );
                println!("snapshot_bytes: {}", bytes(stats.snapshot_bytes));
                println!("archive_bytes: {}", bytes(stats.archive_bytes));
                println!("pending_cleanup_jobs: {}", stats.pending_cleanup_jobs);
            }
        }
        StorageAction::Snapshots { json, manifests } => {
            let entries =
                SnapshotStore::store_entries(&state_dir.path().join(SESSION_SNAPSHOTS_DIR));
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                print!("{}", render_snapshots(&entries, manifests));
            }
        }
        StorageAction::Check => {
            let database = SessionDatabase::open_read_only(&state_dir)
                .context("open session database read-only")?;
            database.quick_check().context("check session database")?;
            println!("ok");
        }
        StorageAction::Checkpoint { truncate } => {
            let database = SessionDatabase::open(&state_dir).context("open session database")?;
            let result = database
                .checkpoint(truncate)
                .context("checkpoint session database")?;
            println!("busy: {}", result.busy);
            println!("log_frames: {}", result.log_frames);
            println!("checkpointed_frames: {}", result.checkpointed_frames);
        }
        StorageAction::Vacuum { pages } => {
            let database = SessionDatabase::open(&state_dir).context("open session database")?;
            let freed = database
                .incremental_vacuum(pages)
                .context("vacuum session database")?;
            println!("freed_pages: {freed}");
        }
        StorageAction::Sessions { directory, json } => {
            let database = SessionDatabase::open(&state_dir).context("open session database")?;
            let facts = database
                .session_facts(directory.as_deref())
                .context("list sessions")?;
            let rows: Vec<SessionRow<'_>> = facts
                .iter()
                .map(|facts| SessionRow {
                    facts,
                    artifact_bytes: database.artifact_bytes(facts.id),
                    trimmed: facts.is_trimmed(),
                })
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print!("{}", render_sessions(&rows));
            }
        }
        StorageAction::Trim {
            ids,
            policy,
            scope,
            dry_run,
            json,
        } => {
            let mut database =
                SessionDatabase::open(&state_dir).context("open session database")?;
            let options = SweepOptions {
                dry_run,
                json,
                prune: false,
            };
            if ids.is_empty() {
                let retention = load_retention(no_plugins, no_jit)?;
                let policy = effective_policy(&policy, &scope, retention.trim)?;
                let group_by = scope.group_by.unwrap_or(retention.group_by);
                let plan = sweep::plan(
                    &database,
                    Action::Trim,
                    policy,
                    group_by,
                    scope.directory.as_deref(),
                    &Zoned::now(),
                )
                .context("plan trim")?;
                let outcomes = (!dry_run)
                    .then(|| sweep::execute(&mut database, &state_dir, &plan))
                    .transpose()
                    .context("trim sessions")?;
                report_plan(&plan, dry_run, outcomes.as_ref(), None, json)?;
            } else {
                if policy.policy().is_some() {
                    bail!("{ID_POLICY_CONFLICT}");
                }
                apply_by_ids(
                    &mut database,
                    &state_dir,
                    Action::Trim,
                    &parse_ids(&ids)?,
                    options,
                )?;
            }
        }
        StorageAction::Forget {
            ids,
            policy,
            scope,
            dry_run,
            json,
            prune,
        } => {
            let mut database =
                SessionDatabase::open(&state_dir).context("open session database")?;
            let options = SweepOptions {
                dry_run,
                json,
                prune,
            };
            if ids.is_empty() {
                let retention = load_retention(no_plugins, no_jit)?;
                let policy = effective_policy(&policy, &scope, retention.forget)?;
                let group_by = scope.group_by.unwrap_or(retention.group_by);
                forget_by_policy(&mut database, &state_dir, policy, group_by, &scope, options)?;
            } else {
                if policy.policy().is_some() {
                    bail!("{ID_POLICY_CONFLICT}");
                }
                apply_by_ids(
                    &mut database,
                    &state_dir,
                    Action::Forget,
                    &parse_ids(&ids)?,
                    options,
                )?;
            }
        }
        StorageAction::Prune { dry_run, json } => {
            let mut database =
                SessionDatabase::open(&state_dir).context("open session database")?;
            let report = sweep::prune(&mut database, &state_dir, dry_run).context("prune")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", render_prune(&report));
            }
        }
        StorageAction::Usage {
            since,
            group_by,
            json,
            prune_older_than,
        } => {
            let ledger = UsageLedger::open(&state_dir).context("open usage ledger")?;
            if let Some(duration) = prune_older_than {
                // Tool activity is pruned with spend: a user who asks to forget
                // one does not expect the other to survive.
                let cutoff = epoch_cutoff(duration)?;
                let removed = ledger
                    .prune_before(cutoff)
                    .context("prune recorded spend")?;
                let tools = ToolLedger::open(&state_dir)
                    .and_then(|tools| tools.prune_before(cutoff))
                    .context("prune recorded tool activity")?;
                println!("pruned_buckets: {removed}");
                println!("pruned_tool_buckets: {tools}");
                return Ok(());
            }
            let since = since.map(epoch_cutoff).transpose()?;
            let buckets = ledger.buckets(since).context("read recorded spend")?;
            let rows = group_usage(&buckets, group_by);
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print!("{}", render_usage(&rows, group_by));
            }
        }
        StorageAction::Pin { ids } => set_pinned(&state_dir, &ids, true)?,
        StorageAction::Unpin { ids } => set_pinned(&state_dir, &ids, false)?,
    }
    Ok(())
}

struct SweepOptions {
    dry_run: bool,
    json: bool,
    prune: bool,
}

#[derive(Serialize)]
struct OutcomeDocument<'a> {
    outcomes: &'a ExecuteReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    prune: Option<&'a PruneReport>,
}

fn forget_by_policy(
    database: &mut SessionDatabase,
    state_dir: &StateDir,
    policy: KeepPolicy,
    group_by: GroupBy,
    scope: &PolicyScopeArgs,
    options: SweepOptions,
) -> Result<()> {
    let plan = sweep::plan(
        database,
        Action::Forget,
        policy,
        group_by,
        scope.directory.as_deref(),
        &Zoned::now(),
    )
    .context("plan forget")?;
    let outcomes = (!options.dry_run)
        .then(|| sweep::execute(database, state_dir, &plan))
        .transpose()
        .context("forget sessions")?;
    let pruned = match &outcomes {
        Some(report) if options.prune && report.acted() > 0 => {
            Some(sweep::prune(database, state_dir, false).context("prune")?)
        }
        _ => None,
    };
    report_plan(
        &plan,
        options.dry_run,
        outcomes.as_ref(),
        pruned.as_ref(),
        options.json,
    )
}

fn apply_by_ids(
    database: &mut SessionDatabase,
    state_dir: &StateDir,
    action: Action,
    ids: &[CaudraId],
    options: SweepOptions,
) -> Result<()> {
    if options.dry_run {
        let facts = database.session_facts(None)?;
        let rows = ids
            .iter()
            .map(|id| {
                facts
                    .iter()
                    .find(|facts| facts.id == *id)
                    .map(|facts| SessionRow {
                        facts,
                        artifact_bytes: database.artifact_bytes(facts.id),
                        trimmed: facts.is_trimmed(),
                    })
                    .ok_or_else(|| color_eyre::eyre::eyre!("unknown session {id}"))
            })
            .collect::<Result<Vec<_>>>()?;
        if options.json {
            println!("{}", serde_json::to_string_pretty(&rows)?);
        } else {
            println!("Dry run: would {} {} sessions", action.verb(), rows.len());
            print!("{}", render_sessions(&rows));
        }
        return Ok(());
    }
    let outcomes = sweep::apply_ids(database, state_dir, action, ids)
        .with_context(|| format!("{} sessions", action.verb()))?;
    let pruned = if options.prune && outcomes.acted() > 0 {
        Some(sweep::prune(database, state_dir, false).context("prune")?)
    } else {
        None
    };
    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&OutcomeDocument {
                outcomes: &outcomes,
                prune: pruned.as_ref(),
            })?
        );
    } else {
        print!("{}", render_outcomes(action, &outcomes));
        if let Some(report) = &pruned {
            print!("{}", render_prune(report));
        }
    }
    if outcomes.failed() > 0 {
        bail!(
            "{} sessions could not be {}",
            outcomes.failed(),
            action.past()
        );
    }
    Ok(())
}

fn load_retention(no_plugins: bool, no_jit: bool) -> Result<RetentionConfig> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);
    let host = PluginHost::with_jit(Arc::clone(ToolRegistry::global_arc()), !no_jit)
        .context("initialize lua plugin host")?;
    let config = host
        .load_init_files_or_skip(no_plugins, &cwd)
        .context("load init.lua files")?
        .unwrap_or_default()
        .into_config(false)
        .context("invalid config")?;
    Ok(config.storage.retention)
}

/// Flags win over configuration. An empty policy keeps nothing, so it is
/// refused unless the caller opted in for one directory, as restic does.
fn effective_policy(
    args: &KeepPolicyArgs,
    scope: &PolicyScopeArgs,
    configured: KeepPolicy,
) -> Result<KeepPolicy> {
    let policy = args.policy().unwrap_or(configured);
    if policy.is_empty() && !(scope.unsafe_allow_remove_all && scope.directory.is_some()) {
        bail!("{EMPTY_POLICY}");
    }
    Ok(policy)
}

fn parse_ids(raw: &[String]) -> Result<Vec<CaudraId>> {
    raw.iter()
        .map(|raw| {
            raw.parse::<CaudraId>()
                .map_err(|error| color_eyre::eyre::eyre!("invalid session id {raw:?}: {error}"))
        })
        .collect()
}

fn set_pinned(state_dir: &StateDir, raw: &[String], pinned: bool) -> Result<()> {
    let database = SessionDatabase::open(state_dir).context("open session database")?;
    for id in parse_ids(raw)? {
        database
            .set_pinned(id, pinned)
            .with_context(|| format!("update session {id}"))?;
        println!("{} {id}", if pinned { "pinned" } else { "unpinned" });
    }
    Ok(())
}

fn report_plan(
    plan: &Plan,
    dry_run: bool,
    outcomes: Option<&ExecuteReport>,
    prune: Option<&PruneReport>,
    json: bool,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&PlanDocument {
                dry_run,
                plan,
                outcomes,
                prune,
            })?
        );
    } else {
        print!("{}", render_plan(plan, dry_run));
        if let Some(report) = outcomes {
            print!("{}", render_outcomes(plan.action, report));
        }
        if let Some(report) = prune {
            print!("{}", render_prune(report));
        }
    }
    if let Some(report) = outcomes
        && report.failed() > 0
    {
        bail!(
            "{} sessions could not be {}",
            report.failed(),
            plan.action.past()
        );
    }
    Ok(())
}

fn render_plan(plan: &Plan, dry_run: bool) -> String {
    let mut out = String::new();
    let grouping = match plan.group_by {
        GroupBy::Directory => "grouped by directory",
        GroupBy::None => "across all sessions",
    };
    let _ = writeln!(out, "Applying policy: {} ({grouping})", plan.policy);
    if let Some(directory) = &plan.directory {
        let _ = writeln!(out, "Only sessions in {directory}");
    }
    if dry_run {
        let _ = writeln!(out, "Dry run: nothing will be changed");
    }
    for group in &plan.groups {
        let key = if group.key.is_empty() {
            UNGROUPED
        } else {
            &group.key
        };
        let _ = writeln!(out, "\n{key}:");
        if !group.keep.is_empty() {
            let _ = writeln!(out, "keep {} sessions:", group.keep.len());
            let _ = writeln!(
                out,
                "{:ID_WIDTH$} {:ACTIVITY_WIDTH$} {:>SIZE_WIDTH$} {:REASONS_WIDTH$} Title",
                "ID", "Last activity", "Size", "Reasons"
            );
            for decision in &group.keep {
                let _ = writeln!(out, "{}", keep_line(decision));
            }
        }
        if !group.skip.is_empty() {
            let _ = writeln!(out, "skip {} sessions:", group.skip.len());
            for skipped in &group.skip {
                let _ = writeln!(
                    out,
                    "{:ID_WIDTH$} {:ACTIVITY_WIDTH$} {:>SIZE_WIDTH$} {:REASONS_WIDTH$} {}",
                    skipped.session.id,
                    activity(&skipped.session),
                    bytes(skipped.session.logical_bytes),
                    skipped.reason.label(),
                    truncate(&skipped.session.title, TITLE_WIDTH)
                );
            }
        }
        let _ = writeln!(out, "{} {} sessions:", plan.action.verb(), group.act.len());
        if !group.act.is_empty() {
            let _ = writeln!(
                out,
                "{:ID_WIDTH$} {:ACTIVITY_WIDTH$} {:>SIZE_WIDTH$} {:>SIZE_WIDTH$} Title",
                "ID", "Last activity", "Rows", "Artifacts"
            );
            for candidate in &group.act {
                let _ = writeln!(
                    out,
                    "{:ID_WIDTH$} {:ACTIVITY_WIDTH$} {:>SIZE_WIDTH$} {:>SIZE_WIDTH$} {}",
                    candidate.session.id,
                    activity(&candidate.session),
                    bytes(candidate.session.logical_bytes),
                    bytes(candidate.artifact_bytes),
                    truncate(&candidate.session.title, TITLE_WIDTH)
                );
            }
        }
    }
    let artifacts: u64 = plan.candidates().map(|c| c.artifact_bytes).sum();
    let _ = writeln!(
        out,
        "\n{} sessions to {}, {} of artifacts",
        plan.candidate_count(),
        plan.action.verb(),
        bytes(artifacts)
    );
    out
}

fn keep_line(decision: &Decision) -> String {
    let reasons = decision
        .reasons
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{:ID_WIDTH$} {:ACTIVITY_WIDTH$} {:>SIZE_WIDTH$} {:REASONS_WIDTH$} {}",
        decision.session.id,
        activity(&decision.session),
        bytes(decision.session.logical_bytes),
        truncate(&reasons, REASONS_WIDTH),
        truncate(&decision.session.title, TITLE_WIDTH)
    )
}

fn render_outcomes(action: Action, report: &ExecuteReport) -> String {
    let mut out = String::new();
    for outcome in &report.outcomes {
        let note = match &outcome.kind {
            OutcomeKind::Trimmed(trim) => format!(
                "trimmed, {} rows and {} of artifacts released",
                trim.tool_output_rows,
                bytes(trim.artifact_bytes + trim.tool_output_row_bytes)
            ),
            OutcomeKind::Forgotten { artifact_bytes } => {
                format!(
                    "forgotten, {} of artifacts released",
                    bytes(*artifact_bytes)
                )
            }
            OutcomeKind::Skipped { reason } => format!("skipped: {reason}"),
            OutcomeKind::Failed { error } => format!("failed: {error}"),
        };
        let _ = writeln!(out, "{:ID_WIDTH$} {note}", outcome.session.id);
    }
    let _ = writeln!(
        out,
        "\n{} sessions {}, {} released, {} skipped, {} failed",
        report.acted(),
        action.past(),
        bytes(report.released_bytes()),
        report
            .outcomes
            .iter()
            .filter(|o| matches!(o.kind, OutcomeKind::Skipped { .. }))
            .count(),
        report.failed()
    );
    out
}

fn render_prune(report: &PruneReport) -> String {
    let mut out = String::new();
    let prefix = if report.dry_run { "would " } else { "" };
    let _ = writeln!(out, "\nprune:");
    let _ = writeln!(
        out,
        "  {prefix}run {} due cleanup jobs{}",
        report.cleanup_jobs_due,
        if report.dry_run {
            String::new()
        } else {
            format!(", {} completed", report.cleanup_jobs_completed)
        }
    );
    let _ = writeln!(
        out,
        "  {prefix}remove {} orphaned artifact directories ({}) and {} orphaned tool output entries",
        report.orphan_directories,
        bytes(report.orphan_bytes),
        report.orphan_tool_output_entries
    );
    if let Some(checkpoint) = report.checkpoint {
        let _ = writeln!(
            out,
            "  checkpointed {} of {} WAL frames{}",
            checkpoint.checkpointed_frames,
            checkpoint.log_frames,
            if checkpoint.busy > 0 {
                " (readers kept the WAL busy)"
            } else {
                ""
            }
        );
    }
    let _ = writeln!(
        out,
        "  freelist pages: {} before, {} after",
        report.freelist_pages_before, report.freelist_pages_after
    );
    out
}

/// Folds hourly rows into whatever the caller asked to see. Ephemeral spend
/// counts toward the total and is reported again on its own, because it is the
/// same money either way.
fn group_usage(buckets: &[UsageBucket], group_by: UsageGrouping) -> Vec<UsageRow> {
    let mut grouped: BTreeMap<String, UsageRow> = BTreeMap::new();
    for bucket in buckets {
        let key = usage_key(bucket, group_by);
        let row = grouped.entry(key.clone()).or_insert_with(|| UsageRow {
            group: key,
            ..UsageRow::default()
        });
        row.input += bucket.input;
        row.output += bucket.output;
        row.cache_creation += bucket.cache_creation;
        row.cache_read += bucket.cache_read;
        if bucket.subscription {
            row.subscription_cost += bucket.cost;
        } else {
            row.cost += bucket.cost;
        }
        row.priced_turns += bucket.priced_turns;
        row.unpriced_turns += bucket.unpriced_turns;
        if bucket.ephemeral {
            row.ephemeral_cost += bucket.cost;
        }
    }
    let mut rows: Vec<UsageRow> = grouped.into_values().collect();
    for row in &mut rows {
        row.cache_hit_rate = cache_hit_rate(
            row.cache_read,
            row.input + row.cache_creation + row.cache_read,
        );
    }
    // Both payers rank together, or a subscription-only ledger comes back in
    // alphabetical order with every row tied at zero.
    rows.sort_by(|a, b| {
        (b.cost + b.subscription_cost)
            .partial_cmp(&(a.cost + a.subscription_cost))
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.group.cmp(&b.group))
    });
    rows
}

fn usage_key(bucket: &UsageBucket, group_by: UsageGrouping) -> String {
    match group_by {
        UsageGrouping::Model => format!("{}/{}", bucket.provider, bucket.model),
        UsageGrouping::Provider => bucket.provider.clone(),
        UsageGrouping::Project => bucket.cwd.clone(),
        UsageGrouping::Purpose => bucket.purpose.clone(),
        UsageGrouping::Day => bucket_time(bucket, DAY_FORMAT),
        UsageGrouping::Month => bucket_time(bucket, MONTH_FORMAT),
        UsageGrouping::Total => USAGE_TOTAL.to_owned(),
    }
}

fn bucket_time(bucket: &UsageBucket, format: &str) -> String {
    Timestamp::from_second(bucket.bucket_start).map_or_else(
        |_| bucket.bucket_start.to_string(),
        |timestamp| {
            timestamp
                .to_zoned(Zoned::now().time_zone().clone())
                .strftime(format)
                .to_string()
        },
    )
}

fn epoch_cutoff(duration: RetentionDuration) -> Result<i64> {
    duration
        .epoch_cutoff(caudra_storage::now_epoch(), &TimeZone::system())
        .ok_or_else(|| eyre!("{UNREPRESENTABLE_DURATION}: {duration}"))
}

fn render_usage(rows: &[UsageRow], group_by: UsageGrouping) -> String {
    let mut out = String::new();
    let heading = match group_by {
        UsageGrouping::Model => "Provider / Model",
        UsageGrouping::Provider => "Provider",
        UsageGrouping::Project => "Project",
        UsageGrouping::Purpose => "Purpose",
        UsageGrouping::Day => "Day",
        UsageGrouping::Month => "Month",
        UsageGrouping::Total => "Total",
    };
    let _ = writeln!(
        out,
        "{heading:GROUP_WIDTH$} {:>TOKEN_WIDTH$} {:>TOKEN_WIDTH$} {:>TOKEN_WIDTH$} {:>RATE_WIDTH$} {:>COST_WIDTH$} {:>8}",
        "Input", "Output", "Cached", "Hit", "Cost", "Turns"
    );
    let mut total = UsageRow::default();
    for row in rows {
        let _ = writeln!(
            out,
            "{:GROUP_WIDTH$} {:>TOKEN_WIDTH$} {:>TOKEN_WIDTH$} {:>TOKEN_WIDTH$} {:>RATE_WIDTH$} {:>COST_WIDTH$} {:>8}",
            truncate(&row.group, GROUP_WIDTH),
            format_tokens_u64(row.input),
            format_tokens_u64(row.output),
            format_tokens_u64(row.cache_read + row.cache_creation),
            format_hit_rate(row.cache_hit_rate),
            // One column, so a row a plan covered says so with a tilde rather
            // than reporting zero next to real tokens.
            match (row.cost, row.subscription_cost) {
                (0.0, subscription) if subscription > 0.0 => format!("~${subscription:.4}"),
                _ => format!("${:.4}", row.cost + row.subscription_cost),
            },
            row.priced_turns + row.unpriced_turns,
        );
        total.input += row.input;
        total.output += row.output;
        total.cost += row.cost;
        total.subscription_cost += row.subscription_cost;
        total.unpriced_turns += row.unpriced_turns;
        total.ephemeral_cost += row.ephemeral_cost;
    }
    let _ = writeln!(out, "\ntotal_cost: ${:.4}", total.cost);
    if total.subscription_cost > 0.0 {
        let _ = writeln!(
            out,
            "subscription_cost: ${:.4} (covered by a plan, not billed)",
            total.subscription_cost
        );
    }
    if total.ephemeral_cost > 0.0 {
        let _ = writeln!(out, "ephemeral_cost: ${:.4}", total.ephemeral_cost);
    }
    if total.unpriced_turns > 0 {
        let _ = writeln!(
            out,
            "unpriced_turns: {} (tokens the total cannot price)",
            total.unpriced_turns
        );
    }
    out
}

fn render_sessions(rows: &[SessionRow<'_>]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:ID_WIDTH$} {:ACTIVITY_WIDTH$} {:>SIZE_WIDTH$} {:>SIZE_WIDTH$} {:8} Directory / Title",
        "ID", "Last activity", "Rows", "Artifacts", "State"
    );
    for row in rows {
        let state = match (row.facts.pinned, row.trimmed) {
            (true, _) => "pinned",
            (false, true) => "trimmed",
            (false, false) => "full",
        };
        let _ = writeln!(
            out,
            "{:ID_WIDTH$} {:ACTIVITY_WIDTH$} {:>SIZE_WIDTH$} {:>SIZE_WIDTH$} {:8} {}",
            row.facts.id,
            activity(row.facts),
            bytes(row.facts.logical_bytes),
            bytes(row.artifact_bytes),
            state,
            row.facts.cwd
        );
        let _ = writeln!(
            out,
            "{:ID_WIDTH$} {:ACTIVITY_WIDTH$} {:>SIZE_WIDTH$} {:>SIZE_WIDTH$} {:8} {}",
            "",
            "",
            "",
            "",
            "",
            truncate(&row.facts.title, TITLE_WIDTH)
        );
    }
    let _ = writeln!(out, "{} sessions", rows.len());
    out
}

fn render_snapshots(entries: &[StoreEntry], manifests: bool) -> String {
    let mut out = String::new();
    if entries.is_empty() {
        let _ = writeln!(out, "{NO_SNAPSHOTS}");
        return out;
    }
    let _ = writeln!(
        out,
        "{:ID_WIDTH$} {:>SIZE_WIDTH$} {:>OBJECTS_WIDTH$} {:>MANIFESTS_WIDTH$} Workspace",
        "Session", "Size", "Objects", "Snaps"
    );
    let mut total = 0;
    let mut objects = 0;
    for entry in entries {
        total += entry.bytes;
        objects += entry.objects;
        let _ = writeln!(
            out,
            "{:ID_WIDTH$} {:>SIZE_WIDTH$} {:>OBJECTS_WIDTH$} {:>MANIFESTS_WIDTH$} {}",
            entry.session_id,
            bytes(entry.bytes),
            entry.objects,
            entry.manifests.len(),
            entry.root.as_ref().map_or_else(
                || ORPHANED_STORE.to_owned(),
                |root| root.display().to_string()
            ),
        );
        if manifests {
            for name in &entry.manifests {
                let _ = writeln!(out, "{:ID_WIDTH$} {name}", "");
            }
        }
    }
    let _ = writeln!(
        out,
        "{} stores, {} objects, {}",
        entries.len(),
        objects,
        bytes(total)
    );
    out
}

fn activity(facts: &SessionFacts) -> String {
    i64::try_from(facts.active_at())
        .ok()
        .and_then(|seconds| Timestamp::from_second(seconds).ok())
        .map(|timestamp| timestamp.to_zoned(jiff::tz::TimeZone::system()))
        .map_or_else(
            || "-".to_owned(),
            |zoned| zoned.strftime(TIME_FORMAT).to_string(),
        )
}

fn bytes(value: u64) -> String {
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < BYTE_UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} {}", BYTE_UNITS[0])
    } else {
        format!("{size:.1} {}", BYTE_UNITS[unit])
    }
}

fn truncate(text: &str, width: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(width).collect();
    if chars.next().is_some() {
        format!("{}...", head.trim_end())
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use caudra_storage::usage_ledger::LedgerPurpose;

    use super::*;

    const HOUR: i64 = 3600;
    const UNPRICED_VISIBLE: &str = "a total that cannot price some of its tokens must say so";
    const EPHEMERAL_COUNTED: &str = "ephemeral spend is real money and belongs in the total";
    const PURPOSE_IS_ANSWERABLE: &str = "a bill must be able to name what goals cost";
    /// [`bucket`] reads 2 of its 13 prompt tokens from cache.
    const BUCKET_HIT_TEXT: &str = "15%";
    const NO_RATE_TEXT: &str = "—";
    const UNKNOWN_IS_NOT_ZERO: &str =
        "a group with no prompt tokens has no hit rate, which is not a hit rate of zero";
    const WORKSPACE_KEY: &str = "9a3913d670e736996dbc23f4de4fa88f02d249c5081e7cbeaec20d42bc85d002";
    const SESSION_START_MANIFEST: &str = "session-start.json";

    fn bucket(bucket_start: i64, provider: &str, model: &str, cwd: &str, cost: f64) -> UsageBucket {
        purposed_bucket(
            bucket_start,
            provider,
            model,
            cwd,
            cost,
            LedgerPurpose::Chat,
        )
    }

    fn purposed_bucket(
        bucket_start: i64,
        provider: &str,
        model: &str,
        cwd: &str,
        cost: f64,
        purpose: LedgerPurpose,
    ) -> UsageBucket {
        UsageBucket {
            bucket_start,
            provider: provider.into(),
            model: model.into(),
            cwd: cwd.into(),
            purpose: purpose.storage_name().into(),
            ephemeral: false,
            subscription: false,
            input: 10,
            output: 5,
            cache_creation: 1,
            cache_read: 2,
            cost,
            priced_turns: 1,
            unpriced_turns: 0,
        }
    }

    #[test_case(UsageGrouping::Model, 2 ; "model splits the two models")]
    #[test_case(UsageGrouping::Provider, 1 ; "provider folds them together")]
    #[test_case(UsageGrouping::Project, 2 ; "project splits the two directories")]
    #[test_case(UsageGrouping::Total, 1 ; "total is one row")]
    fn grouping_controls_how_many_rows_come_back(group_by: UsageGrouping, expected: usize) {
        let buckets = [
            bucket(0, "anthropic", "opus", "/a", 1.0),
            bucket(HOUR, "anthropic", "haiku", "/b", 2.0),
        ];

        let rows = group_usage(&buckets, group_by);

        assert_eq!(rows.len(), expected);
        assert_eq!(rows.iter().map(|row| row.cost).sum::<f64>(), 3.0);
    }

    #[test]
    fn grouping_by_purpose_separates_goal_spend_from_the_conversation() {
        let buckets = [
            bucket(0, "anthropic", "opus", "/a", 1.0),
            purposed_bucket(0, "anthropic", "opus", "/a", 4.0, LedgerPurpose::Goal),
        ];

        let rows = group_usage(&buckets, UsageGrouping::Purpose);

        assert_eq!(
            rows.iter()
                .map(|row| row.group.as_str())
                .collect::<Vec<_>>(),
            [
                LedgerPurpose::Goal.storage_name(),
                LedgerPurpose::Chat.storage_name()
            ],
            "{PURPOSE_IS_ANSWERABLE}"
        );
        assert_eq!(rows[0].cost, 4.0, "{PURPOSE_IS_ANSWERABLE}");
    }

    #[test]
    fn rows_are_ordered_by_what_they_cost() {
        let buckets = [
            bucket(0, "anthropic", "cheap", "/a", 1.0),
            bucket(0, "anthropic", "dear", "/a", 9.0),
        ];

        let rows = group_usage(&buckets, UsageGrouping::Model);

        assert_eq!(rows[0].group, "anthropic/dear");
    }

    /// The rate is folded from the group's own counters, so `--group-by
    /// provider` scores a provider over every model it served.
    #[test]
    fn rows_score_the_cache_over_the_prompt_tokens_they_folded() {
        let output_only = UsageBucket {
            input: 0,
            cache_creation: 0,
            cache_read: 0,
            ..bucket(0, "local", "llama", "/a", 0.0)
        };
        let rows = group_usage(
            &[
                bucket(0, "anthropic", "opus", "/a", 2.0),
                bucket(HOUR, "anthropic", "haiku", "/a", 1.0),
                output_only,
            ],
            UsageGrouping::Provider,
        );

        assert_eq!(rows[0].group, "anthropic");
        assert_eq!(
            rows[0].cache_hit_rate,
            cache_hit_rate(4, 26),
            "two buckets of 2 cached reads in 13 prompt tokens"
        );
        assert_eq!(rows[1].cache_hit_rate, None, "{UNKNOWN_IS_NOT_ZERO}");

        let rendered = render_usage(&rows, UsageGrouping::Provider);
        assert!(rendered.contains(BUCKET_HIT_TEXT), "{rendered}");
        assert!(rendered.contains(NO_RATE_TEXT), "{UNKNOWN_IS_NOT_ZERO}");
    }

    #[test]
    fn unpriced_turns_are_reported_rather_than_folded_into_the_cost() {
        let unpriced = UsageBucket {
            cost: 0.0,
            priced_turns: 0,
            unpriced_turns: 4,
            ..bucket(0, "local", "llama", "/a", 0.0)
        };
        let rows = group_usage(
            &[bucket(0, "anthropic", "opus", "/a", 2.0), unpriced],
            UsageGrouping::Total,
        );

        let rendered = render_usage(&rows, UsageGrouping::Total);

        assert!(
            rendered.contains("total_cost: $2.0000"),
            "{UNPRICED_VISIBLE}"
        );
        assert!(rendered.contains("unpriced_turns: 4"), "{UNPRICED_VISIBLE}");
    }

    #[test]
    fn ephemeral_spend_counts_toward_the_total_and_is_named_separately() {
        let ephemeral = UsageBucket {
            ephemeral: true,
            ..bucket(0, "anthropic", "opus", "/a", 3.0)
        };
        let rows = group_usage(
            &[bucket(0, "anthropic", "opus", "/a", 1.0), ephemeral],
            UsageGrouping::Total,
        );

        let rendered = render_usage(&rows, UsageGrouping::Total);

        assert!(
            rendered.contains("total_cost: $4.0000"),
            "{EPHEMERAL_COUNTED}"
        );
        assert!(
            rendered.contains("ephemeral_cost: $3.0000"),
            "{EPHEMERAL_COUNTED}"
        );
    }

    #[test]
    fn a_total_without_ephemeral_or_unpriced_spend_stays_quiet() {
        let rows = group_usage(
            &[bucket(0, "anthropic", "opus", "/a", 1.0)],
            UsageGrouping::Total,
        );

        let rendered = render_usage(&rows, UsageGrouping::Total);

        assert!(!rendered.contains("ephemeral_cost"));
        assert!(!rendered.contains("unpriced_turns"));
    }

    #[test_case(0, "0 B")]
    #[test_case(1023, "1023 B")]
    #[test_case(1024, "1.0 KiB")]
    #[test_case(48 * 1024 * 1024, "48.0 MiB")]
    #[test_case(5 * 1024 * 1024 * 1024, "5.0 GiB")]
    fn bytes_are_human_readable(value: u64, expected: &str) {
        assert_eq!(bytes(value), expected);
    }

    #[test]
    fn empty_policy_requires_opt_in_with_a_directory() {
        let args = KeepPolicyArgs::default();
        let scope = PolicyScopeArgs::default();
        assert!(effective_policy(&args, &scope, KeepPolicy::default()).is_err());

        let opted_in = PolicyScopeArgs {
            directory: Some("/project".into()),
            unsafe_allow_remove_all: true,
            ..PolicyScopeArgs::default()
        };
        assert!(effective_policy(&args, &opted_in, KeepPolicy::default()).is_ok());

        let configured = KeepPolicy {
            keep_last: Some(3),
            ..KeepPolicy::default()
        };
        assert_eq!(
            effective_policy(&args, &scope, configured).unwrap(),
            configured
        );
        let flagged = KeepPolicyArgs {
            keep_daily: Some(7),
            ..KeepPolicyArgs::default()
        };
        assert_eq!(
            effective_policy(&flagged, &scope, configured)
                .unwrap()
                .keep_daily,
            Some(7)
        );
    }

    fn store_entry(session_id: &str, root: Option<&str>, bytes: u64) -> StoreEntry {
        StoreEntry {
            session_id: session_id.to_owned(),
            workspace_key: WORKSPACE_KEY.to_owned(),
            root: root.map(Into::into),
            bytes,
            objects: 2,
            manifests: vec![SESSION_START_MANIFEST.to_owned()],
        }
    }

    /// The point of the listing is finding the store that got out of hand, so
    /// the biggest has to come first and the total has to be stated.
    #[test]
    fn snapshot_stores_are_reported_largest_first_with_a_total() {
        const ORDER_MSG: &str = "the largest store must be listed first";
        const TOTAL_MSG: &str = "the listing must total what the stores cost";
        let rendered = render_snapshots(
            &[
                store_entry("CeBig", Some("/repo/big"), 3 * 1024 * 1024),
                store_entry("CeSmall", Some("/repo/small"), 1024),
            ],
            false,
        );

        let big = rendered.find("CeBig").expect(ORDER_MSG);
        let small = rendered.find("CeSmall").expect(ORDER_MSG);
        assert!(big < small, "{ORDER_MSG}");
        assert!(rendered.contains("2 stores"), "{TOTAL_MSG}");
        assert!(
            rendered.contains(&bytes(3 * 1024 * 1024 + 1024)),
            "{TOTAL_MSG}"
        );
    }

    /// A store whose workspace marker is gone is exactly what an operator is
    /// hunting for, so it is named rather than dropped from the listing.
    #[test]
    fn an_orphaned_store_is_named_not_hidden() {
        let rendered = render_snapshots(&[store_entry("CeLost", None, 512)], false);
        assert!(
            rendered.contains(ORPHANED_STORE),
            "an orphaned store must say so"
        );
    }

    #[test]
    fn an_empty_store_says_so_instead_of_printing_a_header() {
        let rendered = render_snapshots(&[], false);
        assert_eq!(rendered.trim(), NO_SNAPSHOTS);
    }

    #[test]
    fn manifests_are_listed_only_when_asked() {
        const HIDDEN_MSG: &str = "manifests stay out of the default listing";
        const SHOWN_MSG: &str = "--manifests must list each manifest";
        let entry = [store_entry("CeM", Some("/repo"), 64)];
        assert!(
            !render_snapshots(&entry, false).contains(SESSION_START_MANIFEST),
            "{HIDDEN_MSG}"
        );
        assert!(
            render_snapshots(&entry, true).contains(SESSION_START_MANIFEST),
            "{SHOWN_MSG}"
        );
    }

    #[test]
    fn truncate_marks_cut_text() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a rather long title", 8), "a rather...");
    }
}
