use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::Path;

use jiff::civil::Date;
use serde::Deserialize;

use crate::repo::{self, RepoError};
use crate::routing::{self, RoutingAccepted, RoutingChange};
use crate::{
    Availability, CatalogCost, CatalogModel, CatalogProvider, DecisionModel, ModelEntry, Payload,
    Pricing, Provider, Tier,
};

pub(crate) const RULES: &str = "audit/accepted.json";
const UNKNOWN_PROVIDER: &str = "must name a provider with curated models and a catalog slug";
const PREFIX_RULE: &str =
    "unlisted must name an uncovered model; other fields must name a curated prefix or alias";
const EMPTY_VALUE: &str = "must not be empty";
const DUPLICATE_RULE: &str = "duplicate audit rule";
const UNUSED_CAP: &str = "cap must name a context or output limit held by a provider entry";
/// Catalog slugs whose new models belong in a table. Aggregators relist every
/// vendor's models, so their additions are someone else's release.
const NEW_MODELS_FROM: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "xai",
    "github-copilot",
    "mistral",
    "zai",
    "deepseek",
];
const DEPRECATED: &str = "deprecated";
const TEXT_OUTPUT: &str = "text";
const LATEST_ALIAS: &str = "-latest";
const RATE_TOLERANCE: f64 = 1e-9;
const NOT_LISTED: &str = "-";
const NO_TIERS: &str = "none";
const TIER_SEPARATOR: &str = "; ";
const NO_DIFFERENCES: &str = "No differences.";
const NEW_MODELS_HEADING: &str = "## New models\n";
const DECISION_MODELS_HEADING: &str = "## Decision models\n\nmodels.dev does not list decision models, so this audit cannot compare them. Check every row on its source page each run, and change `decision-models.json` in the model data repository when a price, a release, or an alias moves. A new release gets its own prefix, never a broader one.\n";
const RETIRED_HEADING: &str = "## Retired, remove from the table\n\nPast the shutdown day of their availability row, so every list already leaves them out. Delete each entry and its row.\n";
const OLDER_MODELS_HEADING: &str = "## Older models no table names\n\nReleased before the survey window, yet still served. Add each one, or accept its omission in `audit/accepted.json` with field `unlisted` and a reason.\n";
const LEAD_NOT_SOURCE: &str = "models.dev is a lead, not a source. Confirm every row on the provider's own model card, pricing, or deprecation page before changing a table. A deliberate divergence goes into `audit/accepted.json` with the value shown here and the reason.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Unlisted,
    Status,
    Retired,
    Input,
    Output,
    CacheRead,
    CacheWrite,
    Tiers,
    Context,
    MaxOutput,
    Missing,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Self::Unlisted => "unlisted",
            Self::Status => "status",
            Self::Retired => "retired",
            Self::Input => "input $/MTok",
            Self::Output => "output $/MTok",
            Self::CacheRead => "cache read $/MTok",
            Self::CacheWrite => "cache write $/MTok",
            Self::Tiers => "long-prompt tiers",
            Self::Context => "context window",
            Self::MaxOutput => "max output",
            Self::Missing => "missing",
        }
    }
}

/// A divergence checked against the provider's own page and kept on purpose.
/// It silences the finding with the same provider, model, and field only while
/// models.dev prints exactly `catalog`, so a later change reports again.
#[derive(Debug, Clone, Deserialize)]
pub struct Accepted {
    /// The manifest slug, such as `copilot`.
    pub provider: String,
    /// The model id printed in the report, including catalog aliases.
    pub model: String,
    pub field: Field,
    /// The models.dev column of the report, as printed.
    pub catalog: String,
    pub reason: String,
}

/// A limit a table holds below what the model accepts, on purpose: a working
/// window caudra compacts inside, or a cold-start fallback the provider's own
/// listing replaces. It silences a finding of `field` for `provider` whose
/// table value is `ours` while models.dev prints more. A model that accepts
/// less still reports, because then the table overstates it.
#[derive(Debug, Clone, Deserialize)]
pub struct Cap {
    pub provider: String,
    pub field: Field,
    pub ours: u32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub provider: String,
    pub model: String,
    pub field: Field,
    /// What the table says, or for an unlisted model the entry it resolves to
    /// today, if any.
    pub ours: String,
    /// What models.dev says, or for an unlisted model its release date.
    pub catalog: String,
}

impl Finding {
    fn matches(&self, accepted: &Accepted) -> bool {
        self.provider == accepted.provider
            && self.model == accepted.model
            && self.field == accepted.field
            && self.catalog == accepted.catalog
    }
}

#[derive(Debug)]
pub struct Audit {
    pub today: Date,
    pub since: Date,
    pub findings: Vec<Finding>,
    pub stale: Vec<Accepted>,
    /// Unaccepted changes against the published file, `None` without one.
    pub routing: Option<Vec<RoutingChange>>,
    pub stale_routing: Vec<RoutingAccepted>,
    pub decisions: Vec<DecisionModel>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Rules {
    pub accepted: Vec<Accepted>,
    pub caps: Vec<Cap>,
    #[serde(default)]
    pub routing: Vec<RoutingAccepted>,
}

impl Rules {
    /// The changes no `routing` acceptance covers, and the acceptances no
    /// change needs any more.
    pub fn unaccepted_routing(
        &self,
        changes: Vec<RoutingChange>,
    ) -> (Vec<RoutingChange>, Vec<RoutingAccepted>) {
        let stale = self
            .routing
            .iter()
            .filter(|row| !changes.iter().any(|change| change.matches(row)))
            .cloned()
            .collect();
        let unaccepted = changes
            .into_iter()
            .filter(|change| !self.routing.iter().any(|row| change.matches(row)))
            .collect();
        (unaccepted, stale)
    }
}

pub fn load_rules(dir: &Path, providers: &[Provider]) -> Result<Rules, RepoError> {
    let path = dir.join(RULES);
    let rules = if path
        .try_exists()
        .map_err(|error| RepoError::new(&path, "", error))?
    {
        repo::read_json(&path)?
    } else {
        Rules::default()
    };
    validate_rules(&rules, providers)
        .map_err(|(pointer, message)| RepoError::new(&path, &pointer, message))?;
    Ok(rules)
}

fn validate_rules(rules: &Rules, providers: &[Provider]) -> Result<(), (String, &'static str)> {
    let provider = |slug: &str| {
        providers.iter().find(|provider| {
            provider.slug == slug && !provider.models.is_empty() && provider.catalog_slug.is_some()
        })
    };
    let mut accepted = HashSet::new();
    for (index, row) in rules.accepted.iter().enumerate() {
        let path = format!("/accepted/{index}");
        for (key, value) in [("model", &row.model), ("reason", &row.reason)] {
            if value.trim().is_empty() {
                return Err((format!("{path}/{key}"), EMPTY_VALUE));
            }
        }
        if row.field != Field::Unlisted && row.catalog.trim().is_empty() {
            return Err((format!("{path}/catalog"), EMPTY_VALUE));
        }
        let Some(provider) = provider(&row.provider) else {
            return Err((format!("{path}/provider"), UNKNOWN_PROVIDER));
        };
        let named = covered(&provider.models, &row.model);
        if named == (row.field == Field::Unlisted) {
            return Err((format!("{path}/model"), PREFIX_RULE));
        }
        if !accepted.insert((&row.provider, &row.model, row.field, &row.catalog)) {
            return Err((path, DUPLICATE_RULE));
        }
    }
    let mut caps = HashSet::new();
    for (index, row) in rules.caps.iter().enumerate() {
        let path = format!("/caps/{index}");
        if row.reason.trim().is_empty() {
            return Err((format!("{path}/reason"), EMPTY_VALUE));
        }
        let Some(provider) = provider(&row.provider) else {
            return Err((format!("{path}/provider"), UNKNOWN_PROVIDER));
        };
        if !provider.models.iter().any(|entry| match row.field {
            Field::Context => entry.context_window == row.ours,
            Field::MaxOutput => entry.max_output_tokens == Some(row.ours),
            _ => false,
        }) {
            return Err((path, UNUSED_CAP));
        }
        if !caps.insert((&row.provider, row.field, row.ours)) {
            return Err((path, DUPLICATE_RULE));
        }
    }
    let mut routing = HashSet::new();
    for (index, row) in rules.routing.iter().enumerate() {
        let path = format!("/routing/{index}");
        for (key, value) in [
            ("provider", &row.provider),
            ("catalog", &row.catalog),
            ("reason", &row.reason),
        ] {
            if value.trim().is_empty() {
                return Err((format!("{path}/{key}"), EMPTY_VALUE));
            }
        }
        if !routing.insert((&row.provider, row.field, &row.catalog)) {
            return Err((path, DUPLICATE_RULE));
        }
    }
    Ok(())
}

pub fn audit(
    payload: &Payload,
    catalog: &BTreeMap<String, CatalogProvider>,
    published: Option<&Payload>,
    today: Date,
    since: Date,
    rules: &Rules,
) -> Audit {
    let accepted = &rules.accepted;
    let mut seen = HashSet::new();
    let raw: Vec<Finding> = payload
        .providers
        .iter()
        .filter(|manifest| !manifest.models.is_empty())
        .filter_map(|manifest| Some((manifest, manifest.catalog_slug.as_deref()?)))
        .filter(|(_, slug)| seen.insert(*slug))
        .flat_map(|(manifest, slug)| {
            let listed = catalog.get(slug).map(|provider| &provider.models);
            let mut findings = table_findings(manifest, listed, today, &rules.caps);
            if NEW_MODELS_FROM.contains(&slug) {
                findings.extend(unlisted_models(manifest, listed));
            }
            findings
        })
        .collect();
    let stale = accepted
        .iter()
        .filter(|row| !raw.iter().any(|finding| finding.matches(row)))
        .cloned()
        .collect();
    let findings = raw
        .into_iter()
        .filter(|finding| !accepted.iter().any(|row| finding.matches(row)))
        .collect();
    let (routing, stale_routing) = published.map_or((None, Vec::new()), |published| {
        let (unaccepted, stale) = rules.unaccepted_routing(routing_changes(published, payload));
        (Some(unaccepted), stale)
    });
    Audit {
        today,
        since,
        findings,
        stale,
        routing,
        stale_routing,
        decisions: payload.decision_models.clone(),
    }
}

/// Routing changes from the published file to `next`, protecting the rows
/// of every built-in provider either of them names.
pub fn routing_changes(published: &Payload, next: &Payload) -> Vec<RoutingChange> {
    routing::changes(
        &routing::builtin_slugs(published.providers.iter().chain(&next.providers)),
        &published.catalog,
        &next.catalog,
    )
}

fn availability<'a>(provider: &'a Provider, entry: &ModelEntry) -> Option<&'a Availability> {
    provider
        .availability
        .iter()
        .find(|row| entry.prefixes.contains(&row.prefix))
}

fn lookup_entry<'a>(entries: &'a [ModelEntry], id: &str) -> Option<&'a ModelEntry> {
    entries
        .iter()
        .flat_map(|entry| entry.prefixes.iter().map(move |prefix| (prefix, entry)))
        .filter(|(prefix, _)| id.starts_with(prefix.as_str()))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, entry)| entry)
}

fn catalog_tiers(cost: &CatalogCost) -> Vec<Tier> {
    let mut tiers: Vec<Tier> = cost
        .tiers
        .iter()
        .map(|tier| Tier {
            above: tier.tier.size,
            input: tier.input.or(cost.input).unwrap_or_default(),
            output: tier.output.or(cost.output).unwrap_or_default(),
            cache_read: tier.cache_read.or(cost.cache_read).unwrap_or_default(),
            cache_write: tier.cache_write.or(cost.cache_write).unwrap_or_default(),
            fast: None,
        })
        .collect();
    tiers.sort_by_key(|tier| tier.above);
    tiers
}

fn table_findings(
    manifest: &Provider,
    listed: Option<&BTreeMap<String, CatalogModel>>,
    today: Date,
    caps: &[Cap],
) -> Vec<Finding> {
    manifest
        .models
        .iter()
        .flat_map(|entry| {
            let primary = entry
                .prefixes
                .first()
                .map(String::as_str)
                .unwrap_or_default();
            if let Some(shutdown) = shut_down_by(manifest, entry, today) {
                return vec![Finding {
                    provider: manifest.slug.clone(),
                    model: primary.to_owned(),
                    field: Field::Retired,
                    ours: shutdown.to_string(),
                    catalog: NOT_LISTED.to_owned(),
                }];
            }
            match listed.and_then(|models| find_listing(entry, models)) {
                Some((id, listing)) => entry_findings(manifest, entry, id, listing, today, caps),
                None => vec![Finding {
                    provider: manifest.slug.clone(),
                    model: primary.to_owned(),
                    field: Field::Missing,
                    ours: entry.prefixes.join(", "),
                    catalog: NOT_LISTED.to_owned(),
                }],
            }
        })
        .collect()
}

/// The day `entry` left the lists, once `today` has reached it.
fn shut_down_by(manifest: &Provider, entry: &ModelEntry, today: Date) -> Option<Date> {
    availability(manifest, entry)
        .and_then(|row| row.during.until)
        .filter(|until| *until <= today)
}

/// The catalog's listing of `entry`: one of its prefixes exactly, else a dated
/// snapshot or alias of one.
fn find_listing<'a>(
    entry: &ModelEntry,
    models: &'a BTreeMap<String, CatalogModel>,
) -> Option<(&'a str, &'a CatalogModel)> {
    let exact = entry
        .prefixes
        .iter()
        .find_map(|prefix| models.get_key_value(prefix));
    exact
        .or_else(|| {
            models
                .iter()
                .filter(|(id, _)| entry.prefixes.iter().any(|prefix| is_alias(id, prefix)))
                .max_by_key(|(id, _)| *id)
        })
        .map(|(id, listing)| (id.as_str(), listing))
}

fn entry_findings(
    manifest: &Provider,
    entry: &ModelEntry,
    id: &str,
    listed: &CatalogModel,
    today: Date,
    caps: &[Cap],
) -> Vec<Finding> {
    let finding = |field, ours: String, catalog: String| Finding {
        provider: manifest.slug.clone(),
        model: id.to_owned(),
        field,
        ours,
        catalog,
    };
    let mut findings = Vec::new();
    // A shutdown day already on the table is handled: the entry leaves the
    // lists by itself, and the report says when to delete it.
    let scheduled = availability(manifest, entry).is_some_and(|row| row.during.until.is_some());
    if listed.status.as_deref() == Some(DEPRECATED) && !scheduled {
        findings.push(finding(
            Field::Status,
            NOT_LISTED.to_owned(),
            DEPRECATED.to_owned(),
        ));
    }
    let pricing = match manifest
        .price_periods
        .iter()
        .find(|row| entry.prefixes.contains(&row.prefix))
    {
        Some(period)
            if period.during.from.is_none_or(|from| from <= today)
                && period.during.until.is_none_or(|until| today < until) =>
        {
            &period.pricing
        }
        _ => &entry.pricing,
    };
    if let Some(cost) = &listed.cost {
        for (field, ours, theirs) in [
            (Field::Input, pricing.input, cost.input),
            (Field::Output, pricing.output, cost.output),
            (Field::CacheRead, pricing.cache_read, cost.cache_read),
            (Field::CacheWrite, pricing.cache_write, cost.cache_write),
        ] {
            if let Some(theirs) = theirs
                && !same_rate(ours, theirs)
            {
                findings.push(finding(field, ours.to_string(), theirs.to_string()));
            }
        }
        let theirs = catalog_tiers(cost);
        if !same_tiers(pricing, &theirs, entry.context_window) {
            findings.push(finding(
                Field::Tiers,
                tiers_text(&pricing.tiers),
                tiers_text(&theirs),
            ));
        }
    }
    let limit = listed.limit.as_ref();
    let input_budget = entry.window_excludes_output;
    let limits = [
        (
            Field::Context,
            Some(entry.context_window),
            limit.and_then(|limit| {
                if input_budget {
                    limit.input.or(limit.context)
                } else {
                    limit.context
                }
            }),
        ),
        (
            Field::MaxOutput,
            entry.max_output_tokens,
            limit.and_then(|limit| limit.output),
        ),
    ];
    for (field, ours, theirs) in limits {
        if let (Some(ours), Some(theirs)) = (ours, theirs)
            && ours != theirs
            && !(ours < theirs && capped(caps, &manifest.slug, field, ours))
        {
            findings.push(finding(field, ours.to_string(), theirs.to_string()));
        }
    }
    findings
}

fn unlisted_models(
    manifest: &Provider,
    listed: Option<&BTreeMap<String, CatalogModel>>,
) -> Vec<Finding> {
    let Some(models) = listed else {
        return Vec::new();
    };
    let mut findings: Vec<Finding> = models
        .iter()
        .filter(|(_, entry)| is_live_tool_model(entry))
        .filter(|(id, _)| !covered(&manifest.models, id))
        .map(|(id, entry)| Finding {
            provider: manifest.slug.clone(),
            model: id.clone(),
            field: Field::Unlisted,
            ours: lookup_entry(&manifest.models, id)
                .and_then(|entry| entry.prefixes.first().map(String::as_str))
                .unwrap_or(NOT_LISTED)
                .to_owned(),
            catalog: entry.release_date.clone().unwrap_or_default(),
        })
        .collect();
    findings.sort_by(|left, right| left.model.cmp(&right.model));
    findings
}

fn is_live_tool_model(listing: &CatalogModel) -> bool {
    listing.tool_call == Some(true)
        && listing.status.as_deref() != Some(DEPRECATED)
        && listing
            .modalities
            .as_ref()
            .is_some_and(|modalities| modalities.output.iter().any(|kind| kind == TEXT_OUTPUT))
}

/// Whether a table already serves `id`, as one of its prefixes or a dated
/// snapshot or alias of one. Matching a prefix by its start alone is the
/// trap this audit exists for: `claude-sonnet-5` would claim
/// `claude-sonnet-5-5`.
fn covered(entries: &[ModelEntry], id: &str) -> bool {
    entries
        .iter()
        .flat_map(|entry| &entry.prefixes)
        .any(|prefix| id == prefix || is_alias(id, prefix))
}

/// `id` is `prefix` plus a date stamp (`-20250805`, `-2024-08-06`, `-2411`)
/// or `-latest`.
fn is_alias(id: &str, prefix: &str) -> bool {
    let Some(suffix) = id.strip_prefix(prefix) else {
        return false;
    };
    if suffix == LATEST_ALIAS {
        return true;
    }
    let Some(stamp) = suffix.strip_prefix('-') else {
        return false;
    };
    let digits =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    match stamp.split('-').collect::<Vec<_>>().as_slice() {
        [compact] => digits(compact, 4) || digits(compact, 8),
        [year, month, day] => digits(year, 4) && digits(month, 2) && digits(day, 2),
        _ => false,
    }
}

fn same_rate(ours: f64, theirs: f64) -> bool {
    (ours - theirs).abs() < RATE_TOLERANCE
}

fn capped(caps: &[Cap], provider: &str, field: Field, ours: u32) -> bool {
    caps.iter()
        .any(|cap| cap.provider == provider && cap.field == field && cap.ours == ours)
}

/// Compares only the tiers a prompt inside `window` can reach: a table states
/// no tier past its own window, since no prompt it sends bills there.
fn same_tiers(ours: &Pricing, theirs: &[Tier], window: u32) -> bool {
    let (ours, theirs) = (reachable(&ours.tiers, window), reachable(theirs, window));
    ours.len() == theirs.len()
        && ours.iter().zip(&theirs).all(|(ours, theirs)| {
            ours.above == theirs.above
                && same_rate(ours.input, theirs.input)
                && same_rate(ours.output, theirs.output)
                && same_rate(ours.cache_read, theirs.cache_read)
                && same_rate(ours.cache_write, theirs.cache_write)
        })
}

fn reachable(tiers: &[Tier], window: u32) -> Vec<&Tier> {
    tiers.iter().filter(|tier| tier.above < window).collect()
}

fn tiers_text(tiers: &[Tier]) -> String {
    if tiers.is_empty() {
        return NO_TIERS.to_owned();
    }
    tiers
        .iter()
        .map(|tier| {
            format!(
                "above {}: {} / {} / {} / {}",
                tier.above, tier.input, tier.output, tier.cache_read, tier.cache_write
            )
        })
        .collect::<Vec<_>>()
        .join(TIER_SEPARATOR)
}

impl Audit {
    fn section(&self, fields: &[Field]) -> Vec<&Finding> {
        self.findings
            .iter()
            .filter(|finding| fields.contains(&finding.field))
            .collect()
    }
}

impl fmt::Display for Audit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "# models.dev audit, {} (new models since {})\n",
            self.today, self.since
        )?;
        writeln!(f, "{LEAD_NOT_SOURCE}\n")?;
        if self.findings.is_empty()
            && self.stale.is_empty()
            && self.stale_routing.is_empty()
            && self.routing.as_ref().is_none_or(Vec::is_empty)
        {
            writeln!(f, "{NO_DIFFERENCES}\n")?;
        } else {
            self.write_findings(f)?;
        }
        writeln!(f, "{}", routing::section(self.routing.as_deref()))?;
        write_decision_models(f, &self.decisions)
    }
}

/// models.dev has no decision models, so the report restates the table for a
/// run to check against each entry's source page by hand.
fn write_decision_models(f: &mut fmt::Formatter<'_>, decisions: &[DecisionModel]) -> fmt::Result {
    writeln!(f, "{DECISION_MODELS_HEADING}")?;
    writeln!(
        f,
        "| Protocol | Models | Input / output / cache read / cache write | Tiers | Source |"
    )?;
    writeln!(f, "|---|---|---|---|---|")?;
    for entry in decisions {
        let pricing = &entry.pricing;
        writeln!(
            f,
            "| {} | {} | {} / {} / {} / {} | {} | {} |",
            entry.protocol.as_str(),
            entry
                .prefixes
                .iter()
                .map(|prefix| format!("`{prefix}`"))
                .collect::<Vec<_>>()
                .join(", "),
            pricing.input,
            pricing.output,
            pricing.cache_read,
            pricing.cache_write,
            tiers_text(&pricing.tiers),
            entry.source
        )?;
    }
    Ok(())
}

impl Audit {
    fn write_findings(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let since = self.since.to_string();
        let (new, older): (Vec<&Finding>, Vec<&Finding>) = self
            .section(&[Field::Unlisted])
            .into_iter()
            .partition(|finding| finding.catalog >= since);
        for (heading, rows) in [(NEW_MODELS_HEADING, new), (OLDER_MODELS_HEADING, older)] {
            if rows.is_empty() {
                continue;
            }
            writeln!(f, "{heading}")?;
            writeln!(f, "| Provider | Model | Released | Resolves to today |")?;
            writeln!(f, "|---|---|---|---|")?;
            for finding in rows {
                writeln!(
                    f,
                    "| {} | `{}` | {} | {} |",
                    finding.provider, finding.model, finding.catalog, finding.ours
                )?;
            }
            writeln!(f)?;
        }
        let retired = self.section(&[Field::Retired]);
        if !retired.is_empty() {
            writeln!(f, "{RETIRED_HEADING}")?;
            writeln!(f, "| Provider | Model | Shut down |")?;
            writeln!(f, "|---|---|---|")?;
            for finding in retired {
                writeln!(
                    f,
                    "| {} | `{}` | {} |",
                    finding.provider, finding.model, finding.ours
                )?;
            }
            writeln!(f)?;
        }
        let deprecated = self.section(&[Field::Status]);
        if !deprecated.is_empty() {
            writeln!(f, "## Deprecated\n")?;
            writeln!(f, "| Provider | Model |")?;
            writeln!(f, "|---|---|")?;
            for finding in deprecated {
                writeln!(f, "| {} | `{}` |", finding.provider, finding.model)?;
            }
            writeln!(f)?;
        }
        let changed = self.section(&[
            Field::Input,
            Field::Output,
            Field::CacheRead,
            Field::CacheWrite,
            Field::Tiers,
            Field::Context,
            Field::MaxOutput,
        ]);
        if !changed.is_empty() {
            writeln!(f, "## Changed\n")?;
            writeln!(f, "| Provider | Model | Field | Ours | models.dev |")?;
            writeln!(f, "|---|---|---|---|---|")?;
            for finding in changed {
                writeln!(
                    f,
                    "| {} | `{}` | {} | {} | {} |",
                    finding.provider,
                    finding.model,
                    finding.field.label(),
                    finding.ours,
                    finding.catalog
                )?;
            }
            writeln!(f)?;
        }
        let missing = self.section(&[Field::Missing]);
        if !missing.is_empty() {
            writeln!(f, "## Not on models.dev\n")?;
            writeln!(f, "| Provider | Model | Prefixes |")?;
            writeln!(f, "|---|---|---|")?;
            for finding in missing {
                writeln!(
                    f,
                    "| {} | `{}` | {} |",
                    finding.provider, finding.model, finding.ours
                )?;
            }
            writeln!(f)?;
        }
        if !self.stale.is_empty() || !self.stale_routing.is_empty() {
            writeln!(f, "## Stale acceptances\n")?;
            writeln!(
                f,
                "models.dev no longer prints these values. Delete each row from `audit/accepted.json`, and check any new finding for the same model.\n"
            )?;
            writeln!(f, "| Provider | Model | Field | Accepted value | Reason |")?;
            writeln!(f, "|---|---|---|---|---|")?;
            for row in &self.stale {
                writeln!(
                    f,
                    "| {} | `{}` | {} | {} | {} |",
                    row.provider,
                    row.model,
                    row.field.label(),
                    row.catalog,
                    row.reason
                )?;
            }
            for row in &self.stale_routing {
                writeln!(
                    f,
                    "| {} | {NOT_LISTED} | routing {} | {} | {} |",
                    row.provider,
                    row.field.label(),
                    row.catalog,
                    row.reason
                )?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;
    use test_case::test_case;

    use super::{
        Accepted, Audit, Cap, DECISION_MODELS_HEADING, DEPRECATED, DUPLICATE_RULE, EMPTY_VALUE,
        Field, Finding, NEW_MODELS_HEADING, NO_DIFFERENCES, OLDER_MODELS_HEADING, PREFIX_RULE,
        RETIRED_HEADING, Rules, TEXT_OUTPUT, UNKNOWN_PROVIDER, UNUSED_CAP, audit, is_alias,
        validate_rules,
    };
    use crate::routing::{self, RoutingAccepted, RoutingField};
    use crate::{ModelEntry, Payload, Provider};
    use jiff::civil::Date;

    const TODAY: Date = date(2026, 10, 14);
    const SINCE: Date = date(2026, 7, 1);
    const PROVIDER: &str = "anthropic";
    const GOOGLE: &str = "google";
    const OPUS: &str = "claude-opus-4-5";
    const SNAPSHOT_STAMP: &str = "-20251101";
    const UNLISTED_SUFFIX: &str = "-9";
    const NOVEL: &str = "claude-novel-1";
    const RELEASED: &str = "2026-09-01";
    const RELEASED_BEFORE_SINCE: &str = "2026-06-30";
    const SECTION_START: &str = "\n## ";
    const LISTED_RELEASE: &str = "2025-11-24";
    const IMAGE_OUTPUT: &str = "image";
    const CHANGED_RATE: f64 = 99.0;
    const OTHER_CHANGED_RATE: f64 = 98.0;
    const CHANGED_PRICE: &str = "99";
    const TINY_LIMIT: u32 = 1;
    const TIER_ABOVE: u32 = 1;
    const TIER_INPUT: f64 = 9.0;
    const ACCEPTANCE_REASON: &str = "checked on the model card";
    const REPORTED: &str = "the divergence must be reported";
    const SILENCED: &str = "an accepted divergence must be silenced";
    const STALE: &str = "an acceptance the catalog no longer matches must be reported as stale";
    const SKIPPED: &str = "the catalog id must not be reported as new";
    const ALIAS: &str = "a dated snapshot of a table prefix is covered";
    const REPORT_EMPTY: &str = "an audit without findings must say so";
    const DECISION_MODELS_LISTED: &str =
        "every report must restate the decision models and where to check them";
    const NO_REASON: &str = "an acceptance needs a reason";
    const COPILOT: &str = "copilot";
    const COPILOT_CATALOG: &str = "github-copilot";
    const RETIRING: &str = "claude-opus-5";
    const LAST_DAY_BEFORE_SHUTDOWN: Date = date(2026, 11, 15);
    const SHUTDOWN_DAY: Date = date(2026, 11, 16);
    const SCHEDULED: &str =
        "a shutdown the table schedules reports once its day has come, and only as retired";
    const OPENAI: &str = "openai";
    const COPILOT_UNTIERED: &str = "gpt-6-luna";
    const TOTAL_WINDOW: &str = "gpt-5.2";
    const INPUT_BUDGET: &str = "claude-opus-5-5";
    const CAP_SILENCES: &str = "a catalog limit above a deliberate cap must be silenced";
    const CAP_OVERSTATES: &str = "a catalog limit below a cap must be reported";
    const UNREACHABLE_TIER: &str = "a tier past the table's window never bills";
    const WINDOW_KIND: &str =
        "a total window compares with the catalog's context, an input budget with its input limit";
    const COPILOT_FALLBACK_WINDOW: u32 = 200_000;
    const GATEWAY: &str = "gateway";
    const GATEWAY_API: &str = "https://gateway.example.com/v1";
    const MOVED_API: &str = "https://moved.example.com/v1";
    const ROUTING_REPORTED: &str = "an unaccepted routing change must be reported";
    const ROUTING_STALE: &str = "a routing acceptance no change needs must be reported as stale";

    fn fixture() -> Payload {
        serde_json::from_str(include_str!("audit_fixture.json")).unwrap()
    }

    fn provider(slug: &str) -> Option<Provider> {
        fixture()
            .providers
            .into_iter()
            .find(|provider| provider.slug == slug)
    }

    fn opus_input_accepted() -> Vec<Accepted> {
        vec![Accepted {
            provider: PROVIDER.into(),
            model: OPUS.into(),
            field: Field::Input,
            catalog: CHANGED_PRICE.into(),
            reason: ACCEPTANCE_REASON.into(),
        }]
    }

    fn caps() -> Vec<Cap> {
        [
            (PROVIDER, Field::Context, 372_000),
            (OPENAI, Field::Context, 372_000),
            (COPILOT, Field::Context, COPILOT_FALLBACK_WINDOW),
            (COPILOT, Field::MaxOutput, 100_000),
            ("xai", Field::MaxOutput, 131_072),
        ]
        .into_iter()
        .map(|(provider, field, ours)| Cap {
            provider: provider.into(),
            field,
            ours,
            reason: ACCEPTANCE_REASON.into(),
        })
        .collect()
    }

    fn audit_with(
        json: &str,
        today: Date,
        since: Date,
        accepted: &[Accepted],
    ) -> Result<Audit, serde_json::Error> {
        Ok(audit(
            &fixture(),
            &serde_json::from_str(json)?,
            None,
            today,
            since,
            &Rules {
                accepted: accepted.to_vec(),
                caps: caps(),
                routing: Vec::new(),
            },
        ))
    }

    fn entry(slug: &str, prefix: &str) -> ModelEntry {
        provider(slug)
            .unwrap()
            .models
            .into_iter()
            .find(|entry| entry.prefixes.iter().any(|p| p == prefix))
            .unwrap()
    }

    /// The catalog listing `entry` exactly as its table does.
    fn faithful(entry: &ModelEntry, id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "tool_call": true,
            "release_date": LISTED_RELEASE,
            "modalities": { "input": [TEXT_OUTPUT], "output": [TEXT_OUTPUT] },
            "limit": { "context": entry.context_window, "output": entry.max_output_tokens },
            "cost": {
                "input": entry.pricing.input,
                "output": entry.pricing.output,
                "cache_read": entry.pricing.cache_read,
                "cache_write": entry.pricing.cache_write,
                "tiers": entry.pricing.tiers.iter().map(|tier| serde_json::json!({
                    "input": tier.input,
                    "output": tier.output,
                    "cache_read": tier.cache_read,
                    "cache_write": tier.cache_write,
                    "tier": { "size": tier.above },
                })).collect::<Vec<_>>(),
            },
        })
    }

    fn catalog(provider: &str, models: serde_json::Value) -> String {
        serde_json::json!({ provider: { "name": provider, "npm": "@ai-sdk/openai-compatible", "models": models } }).to_string()
    }

    fn opus_with(change: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut model = faithful(&entry(PROVIDER, OPUS), OPUS);
        change(&mut model);
        catalog(PROVIDER, serde_json::json!({ OPUS: model }))
    }

    fn fields_of(audit: &Audit, provider: &str, model: &str) -> Vec<Field> {
        audit
            .findings
            .iter()
            .filter(|finding| finding.provider == provider && finding.model == model)
            .map(|finding| finding.field)
            .collect()
    }

    fn opus_fields(json: &str, accepted: &[Accepted]) -> (Vec<Field>, Audit) {
        let audit = audit_with(json, TODAY, SINCE, accepted).unwrap();
        (fields_of(&audit, PROVIDER, OPUS), audit)
    }

    fn new_model(tool_call: bool, output: &str, released: &str) -> serde_json::Value {
        serde_json::json!({
            "tool_call": tool_call,
            "release_date": released,
            "modalities": { "input": [TEXT_OUTPUT], "output": [output] },
        })
    }

    #[test]
    fn a_faithful_listing_reports_nothing() {
        assert_eq!(opus_fields(&opus_with(|_| {}), &[]).0, Vec::<Field>::new());
    }

    #[test_case(|m| m["cost"]["input"] = serde_json::json!(CHANGED_RATE), Field::Input ; "input_price")]
    #[test_case(|m| m["cost"]["cache_read"] = serde_json::json!(CHANGED_RATE), Field::CacheRead ; "cache_read_price")]
    #[test_case(|m| m["limit"]["context"] = serde_json::json!(TINY_LIMIT), Field::Context ; "context")]
    #[test_case(|m| m["limit"]["output"] = serde_json::json!(TINY_LIMIT), Field::MaxOutput ; "max_output")]
    #[test_case(|m| m["cost"]["tiers"] = serde_json::json!([{ "input": TIER_INPUT, "tier": { "size": TIER_ABOVE } }]), Field::Tiers ; "tiers")]
    #[test_case(|m| m["status"] = serde_json::json!(DEPRECATED), Field::Status ; "deprecated")]
    fn a_divergence_is_reported(change: fn(&mut serde_json::Value), field: Field) {
        assert_eq!(
            opus_fields(&opus_with(change), &[]).0,
            vec![field],
            "{REPORTED}"
        );
    }

    #[test]
    fn a_rate_models_dev_leaves_out_is_not_a_divergence() {
        let json = opus_with(|model| {
            model["cost"].as_object_mut().unwrap().remove("cache_write");
        });
        assert_eq!(opus_fields(&json, &[]).0, Vec::<Field>::new());
    }

    #[test]
    fn an_accepted_divergence_is_silenced_while_the_catalog_prints_its_value() {
        let json = opus_with(|model| model["cost"]["input"] = serde_json::json!(CHANGED_RATE));
        let (fields, audit) = opus_fields(&json, &opus_input_accepted());
        assert_eq!(fields, Vec::<Field>::new(), "{SILENCED}");
        assert!(audit.stale.is_empty(), "{SILENCED}");
    }

    #[test]
    fn a_changed_catalog_value_reports_again_and_the_acceptance_goes_stale() {
        let json =
            opus_with(|model| model["cost"]["input"] = serde_json::json!(OTHER_CHANGED_RATE));
        let (fields, audit) = opus_fields(&json, &opus_input_accepted());
        assert_eq!(fields, vec![Field::Input], "{REPORTED}");
        assert_eq!(audit.stale.len(), opus_input_accepted().len(), "{STALE}");
    }

    #[test]
    fn an_entry_models_dev_does_not_list_is_missing() {
        let json = catalog(PROVIDER, serde_json::json!({}));
        assert_eq!(
            opus_fields(&json, &[]).0,
            vec![Field::Missing],
            "{REPORTED}"
        );
    }

    #[test]
    fn a_dated_snapshot_answers_for_its_prefix() {
        let dated = format!("{OPUS}{SNAPSHOT_STAMP}");
        let json = catalog(
            PROVIDER,
            serde_json::json!({ dated.clone(): faithful(&entry(PROVIDER, OPUS), &dated) }),
        );
        let (fields, audit) = opus_fields(&json, &[]);
        assert_eq!(fields, Vec::<Field>::new(), "{ALIAS}");
        assert_eq!(
            fields_of(&audit, PROVIDER, &dated),
            Vec::<Field>::new(),
            "{ALIAS}"
        );
    }

    #[test]
    fn a_new_model_names_the_entry_that_would_swallow_it() {
        let id = format!("{OPUS}{UNLISTED_SUFFIX}");
        let json = catalog(
            PROVIDER,
            serde_json::json!({ id.clone(): new_model(true, TEXT_OUTPUT, RELEASED) }),
        );
        let audit = audit_with(&json, TODAY, SINCE, &[]).unwrap();
        let finding = audit
            .findings
            .iter()
            .find(|finding| finding.model == id)
            .expect(REPORTED);
        assert_eq!(
            (
                finding.field,
                finding.ours.as_str(),
                finding.catalog.as_str()
            ),
            (Field::Unlisted, OPUS, RELEASED)
        );
    }

    #[test_case(false, TEXT_OUTPUT ; "without_tools")]
    #[test_case(true, IMAGE_OUTPUT ; "without_text_output")]
    fn a_catalog_id_outside_the_scope_is_not_new(tool_call: bool, output: &str) {
        let json = catalog(
            PROVIDER,
            serde_json::json!({ NOVEL: new_model(tool_call, output, RELEASED) }),
        );
        let audit = audit_with(&json, TODAY, SINCE, &[]).unwrap();
        assert_eq!(
            fields_of(&audit, PROVIDER, NOVEL),
            Vec::<Field>::new(),
            "{SKIPPED}"
        );
    }

    /// A run's window only moves forward, so a model released before the
    /// first one has to report too, or no run ever sees it.
    #[test_case(RELEASED, NEW_MODELS_HEADING ; "released_in_the_window")]
    #[test_case(RELEASED_BEFORE_SINCE, OLDER_MODELS_HEADING ; "released_before_the_window")]
    fn an_unlisted_model_reports_under_its_release(released: &str, heading: &str) {
        let json = catalog(
            PROVIDER,
            serde_json::json!({ NOVEL: new_model(true, TEXT_OUTPUT, released) }),
        );
        let audit = audit_with(&json, TODAY, SINCE, &[]).unwrap();
        assert_eq!(
            fields_of(&audit, PROVIDER, NOVEL),
            vec![Field::Unlisted],
            "{REPORTED}"
        );
        let report = audit.to_string();
        let section = report
            .split(heading)
            .nth(1)
            .and_then(|rest| rest.split(SECTION_START).next())
            .expect(REPORTED);
        assert!(section.contains(NOVEL), "{REPORTED}");
    }

    #[test_case("claude-opus-4-1-20250805", "claude-opus-4-1" => true ; "compact_date")]
    #[test_case("gpt-4o-2024-08-06", "gpt-4o" => true ; "dashed_date")]
    #[test_case("mistral-large-2411", "mistral-large" => true ; "year_month")]
    #[test_case("mistral-large-latest", "mistral-large" => true ; "latest")]
    #[test_case("claude-sonnet-5-5", "claude-sonnet-5" => false ; "a_newer_version")]
    #[test_case("gpt-6-mini", "gpt-6" => false ; "a_sibling")]
    fn a_dated_snapshot_or_alias_is_covered(id: &str, prefix: &str) -> bool {
        is_alias(id, prefix)
    }

    /// models.dev already prints the announced price. Before the period's
    /// first day the table's current price is what bills, so they differ.
    #[test_case(true ; "a_period_in_force_is_compared")]
    #[test_case(false ; "a_pending_period_is_not")]
    fn a_price_period_counts_from_its_date(in_force: bool) {
        let period = provider(GOOGLE)
            .unwrap()
            .price_periods
            .first()
            .cloned()
            .unwrap();
        let from = period.during.from.unwrap();
        let mut model = faithful(&entry(GOOGLE, &period.prefix), &period.prefix);
        model["cost"]["input"] = serde_json::json!(period.pricing.input);
        let json = catalog(GOOGLE, serde_json::json!({ &period.prefix: model }));
        let today = if in_force {
            from
        } else {
            from.yesterday().unwrap()
        };
        let audit = audit_with(&json, today, SINCE, &[]).unwrap();
        assert_eq!(
            fields_of(&audit, GOOGLE, &period.prefix).contains(&Field::Input),
            !in_force
        );
    }

    fn deprecated_opus_5() -> String {
        let mut model = faithful(&entry(COPILOT, RETIRING), RETIRING);
        model["status"] = serde_json::json!(DEPRECATED);
        catalog(COPILOT_CATALOG, serde_json::json!({ RETIRING: model }))
    }

    /// An entry with a shutdown day is handled until that day, and from it on
    /// the report asks for nothing but deleting the entry.
    #[test_case(LAST_DAY_BEFORE_SHUTDOWN, &[] ; "a_scheduled_shutdown_is_not_a_finding")]
    #[test_case(SHUTDOWN_DAY, &[Field::Retired] ; "past_the_shutdown_only_the_retirement_reports")]
    fn a_dated_shutdown_settles_the_deprecation(today: Date, expected: &[Field]) {
        let audit = audit_with(&deprecated_opus_5(), today, SINCE, &[]).unwrap();
        let fields = fields_of(&audit, COPILOT, RETIRING);
        assert!(!fields.contains(&Field::Status), "{SCHEDULED}");
        assert_eq!(
            fields
                .iter()
                .filter(|field| **field == Field::Retired)
                .copied()
                .collect::<Vec<_>>(),
            expected,
            "{SCHEDULED}"
        );
        let report = audit.to_string();
        let section = report
            .split(RETIRED_HEADING)
            .nth(1)
            .and_then(|rest| rest.split(SECTION_START).next());
        assert_eq!(
            section.is_some_and(|rows| rows.contains(RETIRING)),
            !expected.is_empty(),
            "{SCHEDULED}"
        );
    }

    #[test]
    fn an_empty_audit_says_there_are_no_differences() {
        let audit = Audit {
            today: TODAY,
            since: SINCE,
            findings: Vec::new(),
            stale: Vec::new(),
            routing: None,
            stale_routing: Vec::new(),
            decisions: fixture().decision_models,
        };
        assert!(audit.to_string().contains(NO_DIFFERENCES), "{REPORT_EMPTY}");
    }

    #[test_case(false ; "without_findings")]
    #[test_case(true ; "with_findings")]
    fn every_report_lists_the_decision_models_with_their_sources(with_findings: bool) {
        let findings = with_findings
            .then(|| Finding {
                provider: PROVIDER.into(),
                model: OPUS.into(),
                field: Field::Input,
                ours: CHANGED_PRICE.into(),
                catalog: CHANGED_PRICE.into(),
            })
            .into_iter()
            .collect();
        let report = Audit {
            today: TODAY,
            since: SINCE,
            findings,
            stale: Vec::new(),
            routing: None,
            stale_routing: Vec::new(),
            decisions: fixture().decision_models,
        }
        .to_string();
        let section = report
            .split_once(DECISION_MODELS_HEADING)
            .map(|(_, rest)| rest)
            .expect(DECISION_MODELS_LISTED);
        for entry in fixture().decision_models {
            assert!(section.contains(&entry.source), "{DECISION_MODELS_LISTED}");
            for prefix in entry.prefixes {
                assert!(section.contains(&prefix), "{DECISION_MODELS_LISTED}");
            }
        }
    }

    fn cap_limit(entry: &ModelEntry, field: Field) -> Option<u32> {
        match field {
            Field::Context => Some(entry.context_window),
            Field::MaxOutput => entry.max_output_tokens,
            _ => None,
        }
    }

    fn limit_key(field: Field) -> &'static str {
        match field {
            Field::MaxOutput => "output",
            _ => "context",
        }
    }

    fn capped_entry(cap: &Cap) -> ModelEntry {
        provider(&cap.provider)
            .unwrap()
            .models
            .iter()
            .find(|entry| cap_limit(entry, cap.field) == Some(cap.ours))
            .cloned()
            .expect(UNUSED_CAP)
    }

    #[test_case(1, false ; "above_the_cap_is_silenced")]
    #[test_case(-1, true ; "below_the_cap_is_reported")]
    fn a_cap_silences_only_a_catalog_that_accepts_more(offset: i64, reported: bool) {
        for cap in &caps() {
            let entry = capped_entry(cap);
            let id = entry.prefixes[0].as_str();
            let mut model = faithful(&entry, id);
            model["limit"][limit_key(cap.field)] = serde_json::json!(i64::from(cap.ours) + offset);
            let slug = provider(&cap.provider).unwrap().catalog_slug.unwrap();
            let json = catalog(&slug, serde_json::json!({ id: model }));
            let audit = audit_with(&json, TODAY, SINCE, &[]).unwrap();
            let message = if reported {
                CAP_OVERSTATES
            } else {
                CAP_SILENCES
            };
            assert_eq!(
                fields_of(&audit, &cap.provider, id).contains(&cap.field),
                reported,
                "{message}: {cap:?}"
            );
        }
    }

    #[test_case(COPILOT_FALLBACK_WINDOW, false ; "a_tier_past_the_window_is_not_compared")]
    #[test_case(TIER_ABOVE, true ; "a_tier_inside_the_window_is")]
    fn a_tier_counts_only_inside_the_window(above: u32, reported: bool) {
        let mut model = faithful(&entry(COPILOT, COPILOT_UNTIERED), COPILOT_UNTIERED);
        model["cost"]["tiers"] =
            serde_json::json!([{ "input": TIER_INPUT, "tier": { "size": above } }]);
        let slug = provider(COPILOT).unwrap().catalog_slug.unwrap();
        let json = catalog(&slug, serde_json::json!({ COPILOT_UNTIERED: model }));
        let audit = audit_with(&json, TODAY, SINCE, &[]).unwrap();
        assert_eq!(
            fields_of(&audit, COPILOT, COPILOT_UNTIERED).contains(&Field::Tiers),
            reported,
            "{UNREACHABLE_TIER}"
        );
    }

    #[test_case(OPENAI, TOTAL_WINDOW, false ; "a_total_window_ignores_the_input_limit")]
    #[test_case(PROVIDER, INPUT_BUDGET, true ; "an_input_budget_compares_with_it")]
    fn a_window_compares_with_the_limit_of_its_kind(provider: &str, id: &str, reported: bool) {
        let mut model = faithful(&entry(provider, id), id);
        model["limit"]["input"] = serde_json::json!(TINY_LIMIT);
        let json = catalog(provider, serde_json::json!({ id: model }));
        let audit = audit_with(&json, TODAY, SINCE, &[]).unwrap();
        assert_eq!(
            fields_of(&audit, provider, id).contains(&Field::Context),
            reported,
            "{WINDOW_KIND}"
        );
    }

    #[test]
    fn every_cap_holds_a_limit_of_its_table() {
        for cap in &caps() {
            capped_entry(cap);
            assert!(!cap.reason.is_empty(), "{NO_REASON}: {cap:?}");
        }
    }

    #[test_case("reason", "", EMPTY_VALUE; "empty_reason")]
    #[test_case("catalog", "", EMPTY_VALUE; "empty_price")]
    #[test_case("provider", "unknown", UNKNOWN_PROVIDER; "unknown_provider")]
    #[test_case("model", "unknown", PREFIX_RULE; "unknown_model")]
    fn invalid_acceptance_names_its_field(field: &str, value: &str, message: &str) {
        let mut row = serde_json::json!({"provider": PROVIDER, "model": OPUS, "field": "input", "catalog": CHANGED_PRICE, "reason": ACCEPTANCE_REASON});
        row[field] = value.into();
        let rules = Rules {
            accepted: vec![serde_json::from_value(row).unwrap()],
            caps: vec![],
            routing: Vec::new(),
        };
        assert_eq!(
            validate_rules(&rules, &fixture().providers),
            Err((format!("/accepted/0/{field}"), message))
        );
    }

    #[test]
    fn duplicate_acceptances_are_rejected() {
        let mut accepted = opus_input_accepted();
        accepted.extend(opus_input_accepted());
        assert_eq!(
            validate_rules(
                &Rules {
                    accepted,
                    caps: vec![],
                    routing: Vec::new(),
                },
                &fixture().providers
            )
            .unwrap_err()
            .1,
            DUPLICATE_RULE
        );
    }

    #[test_case(SNAPSHOT_STAMP; "dated_snapshot")]
    #[test_case("-latest"; "latest_alias")]
    fn a_catalog_alias_divergence_can_be_accepted(suffix: &str) {
        let alias = format!("{OPUS}{suffix}");
        let mut model = faithful(&entry(PROVIDER, OPUS), &alias);
        model["cost"]["input"] = serde_json::json!(CHANGED_RATE);
        let catalog = catalog(PROVIDER, serde_json::json!({ &alias: model }));
        let mut accepted = opus_input_accepted();
        accepted[0].model = alias.clone();
        let rules = Rules {
            accepted,
            caps: vec![],
            routing: Vec::new(),
        };
        validate_rules(&rules, &fixture().providers).unwrap();
        let audit = audit_with(&catalog, TODAY, SINCE, &rules.accepted).unwrap();
        assert!(fields_of(&audit, PROVIDER, &alias).is_empty(), "{SILENCED}");
        assert!(audit.stale.is_empty(), "{SILENCED}");
    }

    #[test]
    fn an_unlisted_model_without_a_release_date_can_be_accepted() {
        let mut model = new_model(true, TEXT_OUTPUT, RELEASED);
        model.as_object_mut().unwrap().remove("release_date");
        let catalog = catalog(PROVIDER, serde_json::json!({ NOVEL: model }));
        let rules = Rules {
            accepted: vec![Accepted {
                provider: PROVIDER.into(),
                model: NOVEL.into(),
                field: Field::Unlisted,
                catalog: String::new(),
                reason: ACCEPTANCE_REASON.into(),
            }],
            caps: vec![],
            routing: Vec::new(),
        };
        validate_rules(&rules, &fixture().providers).unwrap();
        let audit = audit_with(&catalog, TODAY, SINCE, &rules.accepted).unwrap();
        assert!(fields_of(&audit, PROVIDER, NOVEL).is_empty(), "{SILENCED}");
        assert!(audit.stale.is_empty(), "{SILENCED}");
    }

    #[test_case(Field::Input; "not_a_limit")]
    #[test_case(Field::MaxOutput; "wrong_value")]
    fn a_cap_must_name_an_existing_limit(field: Field) {
        let cap = Cap {
            provider: PROVIDER.into(),
            field,
            ours: TINY_LIMIT,
            reason: ACCEPTANCE_REASON.into(),
        };
        assert_eq!(
            validate_rules(
                &Rules {
                    accepted: vec![],
                    caps: vec![cap],
                    routing: Vec::new(),
                },
                &fixture().providers
            )
            .unwrap_err()
            .1,
            UNUSED_CAP
        );
    }

    fn routed(api: &str) -> Payload {
        let mut payload = fixture();
        payload.catalog.insert(
            GATEWAY.into(),
            serde_json::from_value(serde_json::json!({
                "name": GATEWAY, "npm": "@ai-sdk/openai-compatible", "api": api
            }))
            .unwrap(),
        );
        payload
    }

    fn routing_acceptance(catalog: &str, reason: &str) -> RoutingAccepted {
        RoutingAccepted {
            provider: GATEWAY.into(),
            field: RoutingField::Api,
            catalog: catalog.into(),
            reason: reason.into(),
        }
    }

    #[test_case(None, true, false; "unaccepted")]
    #[test_case(Some(MOVED_API), false, false; "accepted")]
    #[test_case(Some(GATEWAY_API), true, true; "accepted_another_value")]
    fn a_routing_change_reports_until_accepted(
        accepted: Option<&str>,
        reported: bool,
        stale: bool,
    ) {
        let rules = Rules {
            routing: accepted
                .map(|catalog| routing_acceptance(catalog, ACCEPTANCE_REASON))
                .into_iter()
                .collect(),
            ..Rules::default()
        };
        let next = routed(MOVED_API);
        let audit = audit(
            &next,
            &next.catalog,
            Some(&routed(GATEWAY_API)),
            TODAY,
            SINCE,
            &rules,
        );
        let report = audit.to_string();
        let section = report
            .split_once(routing::HEADING)
            .and_then(|(_, rest)| rest.split(SECTION_START).next())
            .expect(ROUTING_REPORTED);
        assert_eq!(section.contains(MOVED_API), reported, "{ROUTING_REPORTED}");
        assert_eq!(
            audit.stale_routing.len(),
            usize::from(stale),
            "{ROUTING_STALE}"
        );
    }

    #[test_case("", EMPTY_VALUE; "empty_reason")]
    #[test_case(ACCEPTANCE_REASON, DUPLICATE_RULE; "duplicate")]
    fn invalid_routing_acceptance_is_rejected(reason: &str, message: &str) {
        let row = routing_acceptance(MOVED_API, reason);
        let rules = Rules {
            routing: vec![row.clone(), row],
            ..Rules::default()
        };
        assert_eq!(
            validate_rules(&rules, &fixture().providers).unwrap_err().1,
            message
        );
    }
}
