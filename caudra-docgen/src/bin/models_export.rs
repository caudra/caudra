use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use caudra_models::{self as wire, repo};
use caudra_providers::manifest::{ManifestRegistry, ProviderManifest};
use caudra_providers::pricing::Period;
use caudra_providers::{
    FastPricing, ModelFamily, ModelPricing, StaticReasoningOption, decision_pricing,
};
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use serde::Serialize;
use serde_json::Value;

const USAGE: &str = "usage: caudra-models-export [--check] <seed-dir>";
const COPILOT: &str = "copilot";
const ENTERPRISE: &str = "copilot-enterprise";
const SOURCES: &[(&str, &[&str])] = &[
    (
        "anthropic",
        &[
            "providers/anthropic/shared.rs",
            "providers/anthropic/mod.rs",
        ],
    ),
    ("openai", &["providers/openai/mod.rs"]),
    ("google", &["providers/google/mod.rs"]),
    (COPILOT, &["providers/copilot/mod.rs"]),
    (ENTERPRISE, &["providers/copilot/mod.rs"]),
    ("mistral", &["providers/mistral.rs"]),
    ("zai", &["providers/zai/mod.rs"]),
    ("deepseek", &["providers/deepseek.rs"]),
    ("synthetic", &["providers/synthetic.rs"]),
    ("xai", &["providers/xai/mod.rs"]),
];

#[derive(Default)]
struct Notes {
    models: BTreeMap<String, String>,
    periods: BTreeMap<String, String>,
    availability: BTreeMap<String, String>,
    provider: Vec<String>,
    unattached: Vec<String>,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let args: Vec<_> = env::args_os().skip(1).collect();
    let (check, dir) = match args.as_slice() {
        [dir] => (false, PathBuf::from(dir)),
        [flag, dir] if flag == "--check" => (true, PathBuf::from(dir)),
        _ => bail!(USAGE),
    };
    let mut payload = export_tables();
    attach_notes(&mut payload)?;
    if check {
        return check_snapshot(&dir, &payload);
    }
    let copilot = payload
        .providers
        .iter()
        .find(|provider| provider.slug == COPILOT)
        .cloned()
        .ok_or_else(|| eyre!("missing Copilot manifest"))?;
    let enterprise = payload
        .providers
        .iter_mut()
        .find(|provider| provider.slug == ENTERPRISE)
        .ok_or_else(|| eyre!("missing Enterprise manifest"))?;
    if enterprise.models == copilot.models
        && enterprise.generations == copilot.generations
        && enterprise.price_periods == copilot.price_periods
        && enterprise.availability == copilot.availability
    {
        enterprise.models_from = Some(COPILOT.to_owned());
        enterprise.models.clear();
        enterprise.generations.clear();
        enterprise.price_periods.clear();
        enterprise.availability.clear();
        eprintln!("Copilot Enterprise tables match Copilot; models_from = {COPILOT:?}");
    } else {
        eprintln!("Copilot Enterprise tables differ; exporting its complete tables");
    }
    write_json(
        &dir.join(repo::INDEX),
        &payload
            .providers
            .iter()
            .map(|provider| &provider.slug)
            .collect::<Vec<_>>(),
    )?;
    for provider in &payload.providers {
        #[derive(Serialize)]
        struct AuthoringProvider<'a> {
            #[serde(rename = "$schema")]
            schema: &'static str,
            #[serde(flatten)]
            provider: &'a wire::Provider,
        }
        write_json(
            &dir.join(format!("providers/{}.json", provider.slug)),
            &AuthoringProvider {
                schema: "../schema/provider.v1.json",
                provider,
            },
        )?;
    }
    write_json(
        &dir.join(repo::DECISIONS),
        &repo::DecisionFile {
            schema: Some("schema/decision-models.v1.json".to_owned()),
            decision_models: payload.decision_models,
        },
    )?;
    let import = dir.join(repo::IMPORT);
    if !import.exists() {
        write_json(&import, &payload.catalog)?;
    }
    Ok(())
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn family(value: ModelFamily) -> wire::Family {
    match value {
        ModelFamily::Claude => wire::Family::Claude,
        ModelFamily::Generic => wire::Family::Generic,
        ModelFamily::Gemini => wire::Family::Gemini,
        ModelFamily::Glm => wire::Family::Glm,
        ModelFamily::Gpt => wire::Family::Gpt,
        ModelFamily::Synthetic => wire::Family::Synthetic,
    }
}

fn fast(value: &FastPricing) -> wire::FastPricing {
    wire::FastPricing {
        input: value.input,
        output: value.output,
        cache_write: value.cache_write,
        cache_read: value.cache_read,
    }
}

fn pricing(value: &ModelPricing) -> wire::Pricing {
    wire::Pricing {
        input: value.input,
        output: value.output,
        cache_write: value.cache_write,
        cache_read: value.cache_read,
        fast: value.fast.as_ref().map(fast),
        tiers: value
            .tiers
            .iter()
            .map(|tier| wire::Tier {
                above: tier.above,
                input: tier.input,
                output: tier.output,
                cache_write: tier.cache_write,
                cache_read: tier.cache_read,
                fast: tier.fast.as_ref().map(fast),
            })
            .collect(),
    }
}

fn period(value: &Period) -> wire::Period {
    wire::Period {
        from: value.from,
        until: value.until,
    }
}

fn reasoning(value: &StaticReasoningOption) -> wire::ReasoningOption {
    match value {
        StaticReasoningOption::Toggle => wire::ReasoningOption::Toggle,
        StaticReasoningOption::Effort(values) => wire::ReasoningOption::Effort {
            values: strings(values),
        },
        StaticReasoningOption::BudgetTokens { min, max } => wire::ReasoningOption::BudgetTokens {
            min: *min,
            max: *max,
        },
    }
}

fn provider(value: &ProviderManifest) -> wire::Provider {
    wire::Provider {
        slug: value.slug.to_owned(),
        display_name: value.display_name.to_owned(),
        family: family(value.family),
        supports_thinking: value.supports_thinking,
        accepts_arbitrary_models: value.accepts_arbitrary_models,
        fallback_max_output: value.fallback_max_output,
        fallback_context_window: value.fallback_context_window,
        serves_fast_mode: value.serves_fast_mode,
        catalog_slug: value.catalog_slug.map(str::to_owned),
        pricing_schedule: value
            .pricing_schedule
            .map(|schedule| wire::PricingSchedule {
                windows: schedule
                    .windows()
                    .iter()
                    .map(|window| wire::PricingWindow {
                        start: window.start(),
                        end: window.end(),
                    })
                    .collect(),
                multiplier: schedule.multiplier(),
            }),
        models_from: None,
        generations: value
            .generations
            .iter()
            .map(|generation| wire::Generation {
                label: generation.label.to_owned(),
                members: strings(generation.members),
            })
            .collect(),
        models: value
            .models
            .iter()
            .map(|entry| wire::ModelEntry {
                prefixes: strings(entry.prefixes),
                small: entry.small,
                default: entry.default,
                vision: entry.vision,
                family: family(entry.family),
                pricing: pricing(&entry.pricing),
                max_output_tokens: entry.max_output_tokens,
                context_window: entry.context_window,
                window_excludes_output: entry.window_excludes_output,
                reasoning_options: entry
                    .reasoning_options
                    .map(|options| options.iter().map(reasoning).collect()),
                requires: Vec::new(),
                note: None,
                source: None,
            })
            .collect(),
        price_periods: value
            .price_periods
            .iter()
            .map(|row| wire::PricePeriod {
                prefix: row.prefix.to_owned(),
                during: period(&row.during),
                pricing: pricing(&row.pricing),
                note: None,
                source: None,
            })
            .collect(),
        availability: value
            .availability
            .iter()
            .map(|row| wire::Availability {
                prefix: row.prefix.to_owned(),
                during: period(&row.during),
                note: None,
                source: None,
            })
            .collect(),
        requires: Vec::new(),
        note: None,
        source: None,
    }
}

fn export_tables() -> wire::Payload {
    wire::Payload {
        providers: ManifestRegistry::builtins().iter().map(provider).collect(),
        decision_models: decision_pricing::entries()
            .iter()
            .map(|entry| wire::DecisionModel {
                protocol: entry.protocol.as_str().to_owned(),
                prefixes: strings(entry.prefixes),
                pricing: pricing(&entry.pricing),
                source: entry.source.to_owned(),
            })
            .collect(),
        catalog: BTreeMap::new(),
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    fs::write(path, bytes).wrap_err_with(|| format!("writing {}", path.display()))
}

fn check_snapshot(dir: &Path, expected: &wire::Payload) -> Result<()> {
    let path = dir.join(repo::DIST);
    let snapshot = wire::parse_bounded(&fs::read(&path)?)?;
    wire::validate(&snapshot).map_err(|issues| eyre!("{issues:?}"))?;
    if !snapshot.hash_matches() {
        bail!("{}: /content_sha256 mismatch", path.display());
    }
    let mut actual = snapshot.payload;
    actual.catalog.clear();
    let actual = serde_json::to_value(actual)?;
    let expected = serde_json::to_value(expected)?;
    compare("", &expected, &actual)?;
    eprintln!(
        "Round-trip verified every field of {} providers, {} model entries, and {} decision models",
        ManifestRegistry::builtins().len(),
        ManifestRegistry::builtins()
            .iter()
            .map(|manifest| manifest.models.len())
            .sum::<usize>(),
        decision_pricing::entries().len()
    );
    Ok(())
}

fn compare(pointer: &str, expected: &Value, actual: &Value) -> Result<()> {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            for (key, value) in expected {
                let path = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                compare(
                    &path,
                    value,
                    actual
                        .get(key)
                        .ok_or_else(|| eyre!("{path}: missing field"))?,
                )?;
            }
            if expected.len() != actual.len() {
                bail!("{pointer}: unexpected fields in snapshot");
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            if expected.len() != actual.len() {
                bail!(
                    "{pointer}: expected {} rows, found {}",
                    expected.len(),
                    actual.len()
                );
            }
            for (index, (expected, actual)) in expected.iter().zip(actual).enumerate() {
                compare(&format!("{pointer}/{index}"), expected, actual)?;
            }
        }
        _ if expected != actual => bail!("{pointer}: live table {expected}, snapshot {actual}"),
        _ => {}
    }
    Ok(())
}

fn preceding_comment(lines: &[&str], index: usize) -> String {
    let start = lines[..index]
        .iter()
        .rposition(|line| !line.trim().starts_with("//"))
        .map_or(0, |index| index + 1);
    lines[start..index]
        .iter()
        .map(|line| line.trim().trim_start_matches('/').trim())
        .collect::<Vec<_>>()
        .join("\n")
}

fn first_string(text: &str) -> Option<String> {
    let start = text.find('"')?;
    let end = text[start + 1..].find('"')? + start + 2;
    serde_json::from_str(&text[start..end]).ok()
}

fn scan_notes(text: &str, file: &str) -> Notes {
    let lines: Vec<_> = text
        .split("#[cfg(test)]")
        .next()
        .unwrap_or(text)
        .lines()
        .collect();
    let mut constants = BTreeMap::new();
    let mut constant_notes = BTreeMap::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some((_, declaration)) = line.split_once("const ")
            && let Some((name, rest)) = declaration.split_once(':')
        {
            if let Some((_, value)) = rest.split_once('=')
                && let Some(value) = first_string(value)
            {
                constants.insert(name.trim(), value);
            }
            let comment = preceding_comment(&lines, index);
            if !comment.is_empty() {
                constant_notes.insert(name.trim(), comment);
            }
        }
    }
    let mut notes = Notes::default();
    for (index, line) in lines.iter().enumerate() {
        let kind = if line.trim() == "ModelEntry {" {
            "models"
        } else if line.contains("PricePeriod {") {
            "periods"
        } else if line.contains("Availability {") {
            "availability"
        } else {
            continue;
        };
        let mut end = index;
        let mut depth = 0_i32;
        for (offset, line) in lines[index..].iter().enumerate() {
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
            end = index + offset;
            if depth == 0 {
                break;
            }
        }
        let body = lines[index..=end].join("\n");
        let mut comments = Vec::new();
        let direct = preceding_comment(&lines, index);
        if !direct.is_empty() {
            comments.push(direct);
        }
        let group = match kind {
            "periods" => "PRICE_PERIODS",
            "availability" => "AVAILABILITY",
            _ => "MODELS",
        };
        if let Some(comment) = constant_notes.get(group)
            && !comments.contains(comment)
        {
            comments.push(comment.clone());
        }
        for token in
            body.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        {
            if let Some(comment) = constant_notes.get(token)
                && !comments.contains(comment)
            {
                comments.push(comment.clone());
            }
        }
        for line in &lines[index + 1..=end] {
            if let Some(comment) = line.trim().strip_prefix("//") {
                comments.push(comment.trim_start_matches('/').trim().to_owned());
            }
        }
        if comments.is_empty() {
            continue;
        }
        let prefix = if kind == "models" {
            body.split_once("prefixes:")
                .map(|(_, rest)| rest.trim_start().trim_start_matches("&[").trim_start())
        } else {
            body.split_once("prefix:")
                .map(|(_, rest)| rest.trim_start())
        };
        let prefix = prefix.and_then(|prefix| {
            if prefix.starts_with('"') {
                first_string(prefix)
            } else {
                constants
                    .get(prefix.split([',', ']']).next()?.trim())
                    .cloned()
            }
        });
        let comment = comments.join("\n\n");
        if let Some(prefix) = prefix {
            let map = match kind {
                "models" => &mut notes.models,
                "periods" => &mut notes.periods,
                _ => &mut notes.availability,
            };
            map.insert(prefix, comment);
        } else {
            notes
                .unattached
                .push(format!("{file}:{}: {comment}", index + 1));
        }
    }
    if let Some(comment) = constant_notes.get("PEAK_HOURS") {
        notes.provider.push(comment.clone());
    }
    notes
}

fn source(note: &str) -> Option<String> {
    let start = note.find("https://")?;
    Some(
        note[start..]
            .split(|character: char| {
                character.is_whitespace() || matches!(character, '>' | ')' | ']' | '"' | '`')
            })
            .next()?
            .trim_end_matches(['.', ','])
            .to_owned(),
    )
}

fn attach_notes(payload: &mut wire::Payload) -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../caudra-providers/src");
    let manifests = fs::read_to_string(root.join("manifest.rs"))?;
    let manifest_lines: Vec<_> = manifests.lines().collect();
    let mut count = 0;
    let mut unattached = Vec::new();
    for provider in &mut payload.providers {
        let mut notes = Notes::default();
        if let Some((_, files)) = SOURCES.iter().find(|(slug, _)| *slug == provider.slug) {
            for file in *files {
                let found = scan_notes(&fs::read_to_string(root.join(file))?, file);
                notes.models.extend(found.models);
                notes.periods.extend(found.periods);
                notes.availability.extend(found.availability);
                notes.provider.extend(found.provider);
                notes.unattached.extend(found.unattached);
            }
        }
        let name = provider.slug.replace('-', "_").to_uppercase();
        for (index, line) in manifest_lines.iter().enumerate() {
            if line.starts_with(&format!("const {name}: ProviderManifest")) {
                let comment = preceding_comment(&manifest_lines, index);
                if !comment.is_empty() {
                    notes.provider.push(comment);
                }
            }
        }
        for entry in &mut provider.models {
            if let Some(note) = entry
                .prefixes
                .first()
                .and_then(|prefix| notes.models.remove(prefix))
            {
                entry.source = source(&note);
                entry.note = Some(note);
                count += 1;
            }
        }
        for row in &mut provider.price_periods {
            if let Some(note) = notes.periods.remove(&row.prefix) {
                row.source = source(&note);
                row.note = Some(note);
                count += 1;
            }
        }
        for row in &mut provider.availability {
            if let Some(note) = notes.availability.remove(&row.prefix) {
                row.source = source(&note);
                row.note = Some(note);
                count += 1;
            }
        }
        if !notes.provider.is_empty() {
            let note = notes.provider.join("\n\n");
            provider.source = source(&note);
            provider.note = Some(note);
            count += 1;
        }
        for (prefix, note) in notes
            .models
            .into_iter()
            .chain(notes.periods)
            .chain(notes.availability)
        {
            unattached.push(format!("{} / {prefix}: {note}", provider.slug));
        }
        unattached.extend(notes.unattached);
    }
    eprintln!(
        "Attached notes to {count} rows; {} unattached comments",
        unattached.len()
    );
    for note in unattached {
        eprintln!("Unattached: {note}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{compare, scan_notes, source};
    use serde_json::json;

    const URL: &str = "https://example.com/pricing";
    const FIXTURE: &str = "const PREFIX: &str = \"model\";\n// A price change.\n// <https://example.com/pricing>.\nconst PRICE_PERIODS: &[PricePeriod] = &[\n    PricePeriod {\n        prefix: PREFIX,\n        during: Period::from(date),\n    },\n];\n// A model note.\nModelEntry {\n    prefixes: &[\n        \"model\",\n    ],\n}\n";

    #[test]
    fn comment_scanner_resolves_constants_and_multiline_prefixes() {
        let notes = scan_notes(FIXTURE, "fixture.rs");
        assert!(notes.unattached.is_empty());
        assert_eq!(notes.models["model"], "A model note.");
        assert_eq!(source(&notes.periods["model"]).as_deref(), Some(URL));
    }

    #[test]
    fn comparison_reports_the_changed_field() {
        let error = compare(
            "",
            &json!({"models": [{"context_window": 100}]}),
            &json!({"models": [{"context_window": 101}]}),
        )
        .unwrap_err();
        assert!(error.to_string().starts_with("/models/0/context_window:"));
    }
}
