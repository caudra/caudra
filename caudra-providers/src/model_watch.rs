//! Compares the built-in model tables with the models.dev catalog, so a
//! `make model-watch` run sees what moved upstream before anyone reads the
//! provider's own pages. The catalog is a lead and never a source: it is
//! sometimes wrong, and the tables cap some limits on purpose. [`ACCEPTED`]
//! holds the divergences that were checked against a model card, each only for
//! as long as models.dev prints the value that was checked.

use std::collections::{HashMap, HashSet};
use std::fmt;

use jiff::civil::Date;
use serde::Deserialize;

use crate::AgentError;
use crate::decision_pricing;
use crate::manifest::{ManifestRegistry, ProviderManifest};
use crate::model::{ModelEntry, ModelPricing, PricingTier, lookup_entry};
use crate::providers::anthropic::shared::{WIDE_CONTEXT_WINDOW, declares_input_budget};
use crate::providers::catalog::{self, CatalogMeta, schema::CatalogModel};
use crate::providers::openai;
use crate::providers::xai::GROK_MAX_OUTPUT_TOKENS;

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
const DECISION_MODELS_HEADING: &str = "## Decision models\n\nmodels.dev does not list decision models, so this audit cannot compare them. Check every row on its source page each run, and change `caudra-providers/src/decision_pricing.rs` when a price, a release, or an alias moves. A new release gets its own prefix, never a broader one.\n";
const OLDER_MODELS_HEADING: &str = "## Older models no table names\n\nReleased before the survey window, yet still served. Add each one, or accept its omission in `ACCEPTED` as `Field::Unlisted` with the reason.\n";
const COPILOT_FALLBACK_WINDOW: u32 = 200_000;
const COPILOT_FALLBACK_OUTPUT: u32 = 100_000;
const XAI_LONG_PROMPT: &str = "xAI bills a prompt of 200,000 tokens or more at the long-context rate (docs.x.ai/developers/models), and a tier's `above` is exclusive, so the table's sits at 199,999.";
const LEAD_NOT_SOURCE: &str = "models.dev is a lead, not a source: confirm every row on the provider's own model card, pricing, or deprecation page before changing a table. A divergence that is deliberate goes into `ACCEPTED` in `caudra-providers/src/model_watch.rs` with the value shown here and the reason.";

/// What a finding is about. Each one is a column of a table entry, except
/// [`Field::Unlisted`] (models.dev has a model no table names) and
/// [`Field::Missing`] (a table names a model models.dev does not list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Field {
    Unlisted,
    Status,
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
#[derive(Debug)]
pub struct Accepted {
    /// The manifest slug, such as `copilot`.
    pub provider: &'static str,
    /// The table prefix, or the catalog id for [`Field::Unlisted`].
    pub model: &'static str,
    pub field: Field,
    /// The models.dev column of the report, as printed.
    pub catalog: &'static str,
    pub reason: &'static str,
}

pub const ACCEPTED: &[Accepted] = &[
    Accepted {
        provider: "google",
        model: "gemini-4-argon",
        field: Field::MaxOutput,
        catalog: "32768",
        reason: "Google's model page (ai.google.dev/gemini-api/docs/models) gives Gemini 4 Argon 65,536 output tokens.",
    },
    Accepted {
        provider: "xai",
        model: "grok-4.7",
        field: Field::Tiers,
        catalog: "above 200000: 4 / 12 / 1 / 0",
        reason: XAI_LONG_PROMPT,
    },
    Accepted {
        provider: "xai",
        model: "grok-4.6",
        field: Field::Tiers,
        catalog: "above 200000: 4 / 12 / 1 / 0",
        reason: XAI_LONG_PROMPT,
    },
    Accepted {
        provider: "xai",
        model: "grok-4.5",
        field: Field::Tiers,
        catalog: "above 200000: 4 / 12 / 0.6 / 0",
        reason: XAI_LONG_PROMPT,
    },
    Accepted {
        provider: "xai",
        model: "grok-4.3",
        field: Field::Tiers,
        catalog: "above 200000: 2.5 / 5 / 0.4 / 0",
        reason: XAI_LONG_PROMPT,
    },
];

/// A limit a table holds below what the model accepts, on purpose: a working
/// window caudra compacts inside, or a cold-start fallback the provider's own
/// listing replaces. It silences a finding of `field` for `provider` whose
/// table value is `ours` while models.dev prints more. A model that accepts
/// less still reports, because then the table overstates it.
#[derive(Debug)]
pub struct Cap {
    pub provider: &'static str,
    pub field: Field,
    pub ours: u32,
    pub reason: &'static str,
}

pub const CAPS: &[Cap] = &[
    Cap {
        provider: "anthropic",
        field: Field::Context,
        ours: WIDE_CONTEXT_WINDOW,
        reason: "The working window of the 1M-token models, capped to bound cost and latency. A `-1m` id opens the full window.",
    },
    Cap {
        provider: "openai",
        field: Field::Context,
        ours: openai::WIDE_CONTEXT_WINDOW,
        reason: "The working window of gpt-5.6 and gpt-6, below what the API accepts, because caudra fills it before compacting.",
    },
    Cap {
        provider: "copilot",
        field: Field::Context,
        ours: COPILOT_FALLBACK_WINDOW,
        reason: "A cold-start fallback: Copilot's `/models` reports each model's prompt limit, which replaces it.",
    },
    Cap {
        provider: "copilot",
        field: Field::MaxOutput,
        ours: COPILOT_FALLBACK_OUTPUT,
        reason: "A cold-start fallback: Copilot's `/models` reports each model's output limit, which replaces it.",
    },
    Cap {
        provider: "xai",
        field: Field::MaxOutput,
        ours: GROK_MAX_OUTPUT_TOKENS,
        reason: "xAI documents no text output limit for Grok 4.5 and later (docs.x.ai/developers/grok-4-7), so the table asks for at most this many.",
    },
];

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub provider: &'static str,
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
    pub stale: Vec<&'static Accepted>,
}

/// The facts the runtime schema has no use for.
#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    release_date: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    modalities: Option<Modalities>,
}

#[derive(Deserialize)]
struct Modalities {
    #[serde(default)]
    output: Vec<String>,
}

#[derive(Deserialize)]
struct RawProvider {
    #[serde(default)]
    models: HashMap<String, serde_json::Value>,
}

struct CatalogEntry {
    model: CatalogModel,
    listing: Listing,
}

type Catalog = HashMap<String, HashMap<String, CatalogEntry>>;

/// models.dev's catalog as it is served now, never from the cache.
pub fn fetch_catalog() -> Result<String, AgentError> {
    catalog::fetch_catalog_text()
}

/// Every divergence between the built-in tables and `catalog_json`, minus
/// [`ACCEPTED`]. Prices are compared as they stand on `today`. Every model no
/// table names reports, whenever it was released: a run's window only moves
/// forward, so one released before the first run would otherwise never show.
/// The report lists those released before `since` apart from the new ones.
pub fn audit(catalog_json: &str, today: Date, since: Date) -> Result<Audit, serde_json::Error> {
    audit_with(catalog_json, today, since, ACCEPTED)
}

fn audit_with(
    catalog_json: &str,
    today: Date,
    since: Date,
    accepted: &'static [Accepted],
) -> Result<Audit, serde_json::Error> {
    let catalog = parse(catalog_json)?;
    let mut seen = HashSet::new();
    let raw: Vec<Finding> = ManifestRegistry::builtins()
        .iter()
        .filter(|manifest| !manifest.models.is_empty())
        .filter_map(|manifest| Some((manifest, manifest.catalog_slug?)))
        .filter(|(_, slug)| seen.insert(*slug))
        .flat_map(|(manifest, slug)| {
            let listed = catalog.get(slug);
            let mut findings = table_findings(manifest, listed, today);
            if NEW_MODELS_FROM.contains(&slug) {
                findings.extend(unlisted_models(manifest, listed));
            }
            findings
        })
        .collect();
    let stale = accepted
        .iter()
        .filter(|row| !raw.iter().any(|finding| finding.matches(row)))
        .collect();
    let findings = raw
        .into_iter()
        .filter(|finding| !accepted.iter().any(|row| finding.matches(row)))
        .collect();
    Ok(Audit {
        today,
        since,
        findings,
        stale,
    })
}

/// Reads each model on its own, so one shape this schema has not met costs
/// that model rather than the audit.
fn parse(catalog_json: &str) -> Result<Catalog, serde_json::Error> {
    let raw: HashMap<String, RawProvider> = serde_json::from_str(catalog_json)?;
    Ok(raw
        .into_iter()
        .map(|(slug, provider)| {
            let models = provider
                .models
                .into_iter()
                .filter_map(|(id, value)| {
                    let model = CatalogModel::deserialize(&value).ok()?;
                    let listing = Listing::deserialize(&value).ok()?;
                    Some((id, CatalogEntry { model, listing }))
                })
                .collect();
            (slug, models)
        })
        .collect())
}

fn table_findings(
    manifest: &'static ProviderManifest,
    listed: Option<&HashMap<String, CatalogEntry>>,
    today: Date,
) -> Vec<Finding> {
    manifest
        .models
        .iter()
        .flat_map(|entry| {
            let primary = entry.prefixes.first().copied().unwrap_or_default();
            match listed.and_then(|models| find_listing(entry, models)) {
                Some((id, listing)) => entry_findings(manifest, entry, id, listing, today),
                None => vec![Finding {
                    provider: manifest.slug,
                    model: primary.to_owned(),
                    field: Field::Missing,
                    ours: entry.prefixes.join(", "),
                    catalog: NOT_LISTED.to_owned(),
                }],
            }
        })
        .collect()
}

/// The catalog's listing of `entry`: one of its prefixes exactly, else a dated
/// snapshot or alias of one.
fn find_listing<'a>(
    entry: &ModelEntry,
    models: &'a HashMap<String, CatalogEntry>,
) -> Option<(&'a str, &'a CatalogEntry)> {
    let exact = entry
        .prefixes
        .iter()
        .find_map(|prefix| models.get_key_value(*prefix));
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
    manifest: &'static ProviderManifest,
    entry: &ModelEntry,
    id: &str,
    listed: &CatalogEntry,
    today: Date,
) -> Vec<Finding> {
    let finding = |field, ours: String, catalog: String| Finding {
        provider: manifest.slug,
        model: id.to_owned(),
        field,
        ours,
        catalog,
    };
    let mut findings = Vec::new();
    if listed.listing.status.as_deref() == Some(DEPRECATED) {
        findings.push(finding(
            Field::Status,
            NOT_LISTED.to_owned(),
            DEPRECATED.to_owned(),
        ));
    }
    let pricing = match manifest.price_change(entry) {
        Some(change) if change.from <= today => &change.pricing,
        _ => &entry.pricing,
    };
    if let Some(cost) = &listed.model.cost {
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
        let theirs = CatalogMeta::from_model(&listed.model).pricing_tiers;
        if !same_tiers(pricing, &theirs, entry.context_window) {
            findings.push(finding(
                Field::Tiers,
                tiers_text(&pricing.tiers),
                tiers_text(&theirs),
            ));
        }
    }
    let limit = listed.model.limit.as_ref();
    let input_budget = declares_input_budget(manifest.slug, entry);
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
            && !(ours < theirs && capped(manifest.slug, field, ours))
        {
            findings.push(finding(field, ours.to_string(), theirs.to_string()));
        }
    }
    findings
}

fn unlisted_models(
    manifest: &'static ProviderManifest,
    listed: Option<&HashMap<String, CatalogEntry>>,
) -> Vec<Finding> {
    let Some(models) = listed else {
        return Vec::new();
    };
    let mut findings: Vec<Finding> = models
        .iter()
        .filter(|(_, entry)| is_live_tool_model(&entry.listing))
        .filter(|(id, _)| !covered(manifest.models, id))
        .map(|(id, entry)| Finding {
            provider: manifest.slug,
            model: id.clone(),
            field: Field::Unlisted,
            ours: lookup_entry(manifest.models, id)
                .ok()
                .and_then(|entry| entry.prefixes.first().copied())
                .unwrap_or(NOT_LISTED)
                .to_owned(),
            catalog: entry.listing.release_date.clone(),
        })
        .collect();
    findings.sort_by(|left, right| left.model.cmp(&right.model));
    findings
}

fn is_live_tool_model(listing: &Listing) -> bool {
    listing.tool_call
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
        .flat_map(|entry| entry.prefixes)
        .any(|prefix| id == *prefix || is_alias(id, prefix))
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

fn capped(provider: &str, field: Field, ours: u32) -> bool {
    CAPS.iter()
        .any(|cap| cap.provider == provider && cap.field == field && cap.ours == ours)
}

/// Compares only the tiers a prompt inside `window` can reach: a table states
/// no tier past its own window, since no prompt it sends bills there.
fn same_tiers(ours: &ModelPricing, theirs: &[PricingTier], window: u32) -> bool {
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

fn reachable(tiers: &[PricingTier], window: u32) -> Vec<&PricingTier> {
    tiers.iter().filter(|tier| tier.above < window).collect()
}

fn tiers_text(tiers: &[PricingTier]) -> String {
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
        if self.findings.is_empty() && self.stale.is_empty() {
            writeln!(f, "{NO_DIFFERENCES}\n")?;
        } else {
            self.write_findings(f)?;
        }
        write_decision_models(f)
    }
}

/// models.dev has no decision models, so the report restates the table for a
/// run to check against each entry's source page by hand.
fn write_decision_models(f: &mut fmt::Formatter<'_>) -> fmt::Result {
    writeln!(f, "{DECISION_MODELS_HEADING}")?;
    writeln!(
        f,
        "| Protocol | Models | Input / output / cache read / cache write | Tiers | Source |"
    )?;
    writeln!(f, "|---|---|---|---|---|")?;
    for entry in decision_pricing::entries() {
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
        if !self.stale.is_empty() {
            writeln!(f, "## Stale acceptances\n")?;
            writeln!(
                f,
                "models.dev no longer prints these values. Delete each row from `ACCEPTED`, and check any new finding for the same model.\n"
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
            writeln!(f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;
    use test_case::test_case;

    use super::*;

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
    const UNKNOWN_SLUG: &str = "an acceptance must name a builtin slug";
    const PREFIX_RULE: &str =
        "an unlisted model must name no table prefix, and every other field must name one";
    const NO_REASON: &str = "an acceptance needs a reason";
    const COPILOT: &str = "copilot";
    const OPENAI: &str = "openai";
    const COPILOT_UNTIERED: &str = "gpt-6-luna";
    const TOTAL_WINDOW: &str = "gpt-5.2";
    const INPUT_BUDGET: &str = "claude-opus-5-5";
    const CAP_SILENCES: &str = "a catalog limit above a deliberate cap must be silenced";
    const CAP_OVERSTATES: &str = "a catalog limit below a cap must be reported";
    const UNREACHABLE_TIER: &str = "a tier past the table's window never bills";
    const WINDOW_KIND: &str =
        "a total window compares with the catalog's context, an input budget with its input limit";
    const UNUSED_CAP: &str = "a cap must name a builtin slug and a value its table holds";

    static OPUS_INPUT_ACCEPTED: &[Accepted] = &[Accepted {
        provider: PROVIDER,
        model: OPUS,
        field: Field::Input,
        catalog: CHANGED_PRICE,
        reason: ACCEPTANCE_REASON,
    }];

    fn entry(provider: &str, prefix: &str) -> &'static ModelEntry {
        ManifestRegistry::get(provider)
            .unwrap()
            .models
            .iter()
            .find(|entry| entry.prefixes.contains(&prefix))
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
        serde_json::json!({ provider: { "models": models } }).to_string()
    }

    fn opus_with(change: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut model = faithful(entry(PROVIDER, OPUS), OPUS);
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

    fn opus_fields(json: &str, accepted: &'static [Accepted]) -> (Vec<Field>, Audit) {
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
        let (fields, audit) = opus_fields(&json, OPUS_INPUT_ACCEPTED);
        assert_eq!(fields, Vec::<Field>::new(), "{SILENCED}");
        assert!(audit.stale.is_empty(), "{SILENCED}");
    }

    #[test]
    fn a_changed_catalog_value_reports_again_and_the_acceptance_goes_stale() {
        let json =
            opus_with(|model| model["cost"]["input"] = serde_json::json!(OTHER_CHANGED_RATE));
        let (fields, audit) = opus_fields(&json, OPUS_INPUT_ACCEPTED);
        assert_eq!(fields, vec![Field::Input], "{REPORTED}");
        assert_eq!(audit.stale.len(), OPUS_INPUT_ACCEPTED.len(), "{STALE}");
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
            serde_json::json!({ dated.clone(): faithful(entry(PROVIDER, OPUS), &dated) }),
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

    /// models.dev already prints the announced price. Before the change's
    /// date the table's current price is what bills, so they differ.
    #[test_case(true ; "a_change_in_force_is_compared")]
    #[test_case(false ; "a_pending_change_is_not")]
    fn a_price_change_counts_from_its_date(in_force: bool) {
        let change = ManifestRegistry::get(GOOGLE)
            .unwrap()
            .price_changes
            .first()
            .unwrap();
        let mut model = faithful(entry(GOOGLE, change.prefix), change.prefix);
        model["cost"]["input"] = serde_json::json!(change.pricing.input);
        let json = catalog(GOOGLE, serde_json::json!({ change.prefix: model }));
        let today = if in_force {
            change.from
        } else {
            change.from.yesterday().unwrap()
        };
        let audit = audit_with(&json, today, SINCE, &[]).unwrap();
        assert_eq!(
            fields_of(&audit, GOOGLE, change.prefix).contains(&Field::Input),
            !in_force
        );
    }

    #[test]
    fn an_empty_audit_says_there_are_no_differences() {
        let audit = Audit {
            today: TODAY,
            since: SINCE,
            findings: Vec::new(),
            stale: Vec::new(),
        };
        assert!(audit.to_string().contains(NO_DIFFERENCES), "{REPORT_EMPTY}");
    }

    #[test_case(false ; "without_findings")]
    #[test_case(true ; "with_findings")]
    fn every_report_lists_the_decision_models_with_their_sources(with_findings: bool) {
        let findings = with_findings
            .then(|| Finding {
                provider: PROVIDER,
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
        }
        .to_string();
        let section = report
            .split_once(DECISION_MODELS_HEADING)
            .map(|(_, rest)| rest)
            .expect(DECISION_MODELS_LISTED);
        for entry in decision_pricing::entries() {
            assert!(section.contains(entry.source), "{DECISION_MODELS_LISTED}");
            for prefix in entry.prefixes {
                assert!(section.contains(prefix), "{DECISION_MODELS_LISTED}");
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

    fn capped_entry(cap: &Cap) -> &'static ModelEntry {
        ManifestRegistry::get(cap.provider)
            .unwrap()
            .models
            .iter()
            .find(|entry| cap_limit(entry, cap.field) == Some(cap.ours))
            .expect(UNUSED_CAP)
    }

    #[test_case(1, false ; "above_the_cap_is_silenced")]
    #[test_case(-1, true ; "below_the_cap_is_reported")]
    fn a_cap_silences_only_a_catalog_that_accepts_more(offset: i64, reported: bool) {
        for cap in CAPS {
            let entry = capped_entry(cap);
            let id = entry.prefixes[0];
            let mut model = faithful(entry, id);
            model["limit"][limit_key(cap.field)] = serde_json::json!(i64::from(cap.ours) + offset);
            let slug = ManifestRegistry::get(cap.provider)
                .unwrap()
                .catalog_slug
                .unwrap();
            let json = catalog(slug, serde_json::json!({ id: model }));
            let audit = audit_with(&json, TODAY, SINCE, &[]).unwrap();
            let message = if reported {
                CAP_OVERSTATES
            } else {
                CAP_SILENCES
            };
            assert_eq!(
                fields_of(&audit, cap.provider, id).contains(&cap.field),
                reported,
                "{message}: {cap:?}"
            );
        }
    }

    #[test_case(COPILOT_FALLBACK_WINDOW, false ; "a_tier_past_the_window_is_not_compared")]
    #[test_case(TIER_ABOVE, true ; "a_tier_inside_the_window_is")]
    fn a_tier_counts_only_inside_the_window(above: u32, reported: bool) {
        let mut model = faithful(entry(COPILOT, COPILOT_UNTIERED), COPILOT_UNTIERED);
        model["cost"]["tiers"] =
            serde_json::json!([{ "input": TIER_INPUT, "tier": { "size": above } }]);
        let slug = ManifestRegistry::get(COPILOT)
            .unwrap()
            .catalog_slug
            .unwrap();
        let json = catalog(slug, serde_json::json!({ COPILOT_UNTIERED: model }));
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
        let mut model = faithful(entry(provider, id), id);
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
        for cap in CAPS {
            capped_entry(cap);
            assert!(!cap.reason.is_empty(), "{NO_REASON}: {cap:?}");
        }
    }

    #[test]
    fn every_acceptance_names_a_table_and_its_model() {
        for row in ACCEPTED {
            let manifest = ManifestRegistry::get(row.provider);
            assert!(manifest.is_some(), "{UNKNOWN_SLUG}: {row:?}");
            let named = manifest
                .into_iter()
                .flat_map(|manifest| manifest.models)
                .any(|entry| entry.prefixes.contains(&row.model));
            assert_eq!(
                named,
                row.field != Field::Unlisted,
                "{PREFIX_RULE}: {row:?}"
            );
            assert!(!row.reason.is_empty(), "{NO_REASON}: {row:?}");
        }
    }
}
