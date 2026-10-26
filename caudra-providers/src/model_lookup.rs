//! Resolves a model someone named in passing, such as `sonnet`, `opus 4.6` or
//! `anthropic/terra`, to one exact `provider/model` spec among the models
//! Caudra can use.
//!
//! Names and numbers are matched separately: every word of the query must
//! appear in the model id, and the query's version must be a prefix of the
//! model's, so `opus 4` covers `claude-opus-4-6` but not `claude-opus-5`. A
//! numbered word such as `qwen3` or `k2` is also its family name and the start
//! of the version, so `qwen` finds `qwen3.8-27b`. Among the matches, the
//! newest version wins, then the fewest extra words, so `terra` picks
//! `gpt-5.6-terra` over a `gpt-5.6-terra-preview` and `qwen` picks
//! `qwen3.8-27b` over its `-caveman7` variant. Numbers after the version
//! are words, so `caveman7` and `distill 1.5` still name their variants.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

const MAX_MODEL_CANDIDATES: usize = 12;
/// A numeric run this long is a release date, as in `claude-sonnet-4-20250514`
/// or `gpt-4o-2024-08-06`, never a version.
const MIN_DATE_DIGITS: usize = 4;
const SPEC_SEPARATOR: char = '/';
const LIST_SEPARATOR: &str = ", ";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ModelLookupError {
    #[error("model name is empty")]
    Empty,
    #[error("model {query:?} matches more than one model; pass one of: {}", .candidates.join(LIST_SEPARATOR))]
    Ambiguous {
        query: String,
        candidates: Vec<String>,
    },
    #[error("no available model matches {query:?}{}", suggestion_suffix(.suggestions))]
    NotFound {
        query: String,
        suggestions: Vec<String>,
    },
}

fn suggestion_suffix(suggestions: &[String]) -> String {
    if suggestions.is_empty() {
        String::new()
    } else {
        format!(
            "; available models include: {}",
            suggestions.join(LIST_SEPARATOR)
        )
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Tokens {
    names: BTreeSet<String>,
    /// The family names inside numbered words, matched but never counted as
    /// extra words.
    stems: BTreeSet<String>,
    version: Vec<u64>,
    date: Vec<u64>,
}

impl Tokens {
    /// The version is the first run of numbers. A word after it ends the run,
    /// so the `7` of `qwen3.8-27b-caveman7` and the `1.5` of a
    /// `-distill-1.5` variant are words, never a newer version.
    fn parse(text: &str) -> Self {
        Self::read(text, true)
    }

    /// Every number but a date as a word, the way a variant's own numbers
    /// read, so `caveman7` and `distill 1.5` name their variants.
    fn parse_words(text: &str) -> Self {
        Self::read(text, false)
    }

    fn read(text: &str, mut version_open: bool) -> Self {
        let mut tokens = Self::default();
        for token in text
            .split(|character: char| !character.is_alphanumeric())
            .filter(|token| !token.is_empty())
        {
            if token.bytes().all(|byte| byte.is_ascii_digit())
                && tokens.push_number(token, version_open)
            {
                continue;
            }
            let word = token.to_lowercase();
            let version_before = tokens.version.len();
            if let Some((stem, number)) = split_numbered(&word) {
                tokens.stems.insert(stem.to_owned());
                tokens.push_number(number, version_open);
            }
            version_open &= tokens.version.is_empty() || tokens.version.len() > version_before;
            tokens.names.insert(word);
        }
        tokens
    }

    /// `false` when `digits` is neither a date nor part of the version, so
    /// the caller keeps it as a word.
    fn push_number(&mut self, digits: &str, version_open: bool) -> bool {
        let Ok(number) = digits.parse::<u64>() else {
            return false;
        };
        if digits.len() >= MIN_DATE_DIGITS || !self.date.is_empty() {
            self.date.push(number);
        } else if version_open {
            self.version.push(number);
        } else {
            return false;
        }
        true
    }

    fn knows(&self, word: &str) -> bool {
        self.names.contains(word) || self.stems.contains(word)
    }

    fn admits(&self, candidate: &Self) -> bool {
        self.names.iter().all(|word| candidate.knows(word))
            && candidate.version.starts_with(&self.version)
            && candidate.date.starts_with(&self.date)
    }

    /// Newer versions first; at one version the undated alias beats a dated
    /// snapshot, and a later snapshot beats an earlier one.
    fn newness(&self) -> (&[u64], bool, &[u64]) {
        (&self.version, self.date.is_empty(), &self.date)
    }
}

/// `qwen3` as `qwen` and `3`. Only letters then digits: `4o` and `27b` stay
/// whole words.
fn split_numbered(word: &str) -> Option<(&str, &str)> {
    let (stem, number) = word.split_at(word.find(|character: char| character.is_ascii_digit())?);
    (!stem.is_empty()
        && stem.chars().all(char::is_alphabetic)
        && number.bytes().all(|byte| byte.is_ascii_digit()))
    .then_some((stem, number))
}

struct Candidate<'a> {
    spec: &'a str,
    provider: &'a str,
    id: &'a str,
    tokens: Tokens,
}

/// The one spec in `candidates` that `query` names. `preferred_providers`
/// breaks a tie between providers serving the same model, first entry first.
pub fn find_model(
    query: &str,
    candidates: &[String],
    preferred_providers: &[&str],
) -> Result<String, ModelLookupError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(ModelLookupError::Empty);
    }
    if let Some(spec) = candidates
        .iter()
        .find(|spec| spec.eq_ignore_ascii_case(query))
    {
        return Ok(spec.clone());
    }
    let parsed: Vec<Candidate> = candidates
        .iter()
        .filter_map(|spec| {
            let (provider, id) = spec.split_once(SPEC_SEPARATOR)?;
            Some(Candidate {
                spec,
                provider,
                id,
                tokens: Tokens::parse(id),
            })
        })
        .collect();
    let (scope, name) = match query.split_once(SPEC_SEPARATOR) {
        Some((provider, rest))
            if parsed
                .iter()
                .any(|candidate| candidate.provider.eq_ignore_ascii_case(provider)) =>
        {
            (Some(provider), rest)
        }
        _ => (None, query),
    };
    let in_scope: Vec<&Candidate> = parsed
        .iter()
        .filter(|candidate| {
            scope.is_none_or(|scope| candidate.provider.eq_ignore_ascii_case(scope))
        })
        .collect();
    let wanted = Tokens::parse(name);
    let wanted_words = Tokens::parse_words(name);
    let exact: Vec<&Candidate> = in_scope
        .iter()
        .copied()
        .filter(|candidate| candidate.id.eq_ignore_ascii_case(name))
        .collect();
    let matches = if exact.is_empty() {
        in_scope
            .iter()
            .copied()
            .filter(|candidate| {
                wanted.admits(&candidate.tokens) || wanted_words.admits(&candidate.tokens)
            })
            .collect()
    } else {
        exact
    };
    if matches.is_empty() {
        return Err(ModelLookupError::NotFound {
            query: query.to_owned(),
            suggestions: suggestions(&wanted, &in_scope, preferred_providers),
        });
    }

    let providers: BTreeSet<&str> = matches.iter().map(|candidate| candidate.provider).collect();
    let chosen_provider = preferred_providers
        .iter()
        .copied()
        .find(|provider| providers.contains(provider))
        .or_else(|| (providers.len() == 1).then(|| matches[0].provider));
    let Some(provider) = chosen_provider else {
        let candidates = providers
            .into_iter()
            .flat_map(
                |provider| match closest(query, on_provider(&matches, provider)) {
                    Ok(candidate) => vec![candidate.spec.to_owned()],
                    Err(ModelLookupError::Ambiguous { candidates, .. }) => candidates,
                    Err(_) => Vec::new(),
                },
            )
            .take(MAX_MODEL_CANDIDATES)
            .collect();
        return Err(ModelLookupError::Ambiguous {
            query: query.to_owned(),
            candidates,
        });
    };
    closest(query, on_provider(&matches, provider)).map(|candidate| candidate.spec.to_owned())
}

fn on_provider<'a, 'b>(matches: &[&'b Candidate<'a>], provider: &str) -> Vec<&'b Candidate<'a>> {
    matches
        .iter()
        .copied()
        .filter(|candidate| candidate.provider == provider)
        .collect()
}

/// Among the newest version, the model adding the fewest words to the query.
/// Two different word sets left at that point, such as `gpt-5.6-terra` and
/// `gpt-5.6-luna` for `gpt-5.6`, are different models and the query is
/// ambiguous.
fn closest<'a, 'b>(
    query: &str,
    matches: Vec<&'b Candidate<'a>>,
) -> Result<&'b Candidate<'a>, ModelLookupError> {
    let newest_version = matches
        .iter()
        .map(|&candidate| &candidate.tokens.version)
        .max();
    let latest: Vec<&Candidate> = matches
        .iter()
        .copied()
        .filter(|candidate| Some(&candidate.tokens.version) == newest_version)
        .collect();
    let fewest = latest
        .iter()
        .map(|candidate| candidate.tokens.names.len())
        .min()
        .unwrap_or_default();
    let mut newest: BTreeMap<&BTreeSet<String>, &Candidate> = BTreeMap::new();
    for candidate in latest
        .into_iter()
        .filter(|candidate| candidate.tokens.names.len() == fewest)
    {
        newest
            .entry(&candidate.tokens.names)
            .and_modify(|best| {
                if is_newer(candidate, best) {
                    *best = candidate;
                }
            })
            .or_insert(candidate);
    }
    let mut groups: Vec<&Candidate> = newest.into_values().collect();
    match groups.len() {
        1 => Ok(groups.remove(0)),
        _ => {
            groups.sort_by(|left, right| right.tokens.newness().cmp(&left.tokens.newness()));
            Err(ModelLookupError::Ambiguous {
                query: query.to_owned(),
                candidates: groups
                    .into_iter()
                    .take(MAX_MODEL_CANDIDATES)
                    .map(|candidate| candidate.spec.to_owned())
                    .collect(),
            })
        }
    }
}

fn is_newer(candidate: &Candidate, best: &Candidate) -> bool {
    candidate
        .tokens
        .newness()
        .cmp(&best.tokens.newness())
        .then_with(|| best.id.cmp(candidate.id))
        .is_gt()
}

/// Models sharing a word with the query, the most shared first; failing
/// that, the newest models of the first preferred provider in scope.
fn suggestions(wanted: &Tokens, in_scope: &[&Candidate], preferred: &[&str]) -> Vec<String> {
    let mut related: Vec<(usize, &Candidate)> = in_scope
        .iter()
        .map(|candidate| {
            (
                wanted
                    .names
                    .iter()
                    .filter(|word| candidate.tokens.knows(word))
                    .count(),
                *candidate,
            )
        })
        .filter(|(shared, _)| *shared > 0)
        .collect();
    if related.is_empty() {
        let provider = preferred.iter().find(|provider| {
            in_scope
                .iter()
                .any(|candidate| candidate.provider == **provider)
        });
        related = in_scope
            .iter()
            .filter(|candidate| provider.is_none_or(|provider| candidate.provider == *provider))
            .map(|candidate| (0, *candidate))
            .collect();
    }
    related.sort_by(|(left_shared, left), (right_shared, right)| {
        right_shared
            .cmp(left_shared)
            .then_with(|| right.tokens.newness().cmp(&left.tokens.newness()))
            .then_with(|| left.spec.cmp(right.spec))
    });
    related
        .into_iter()
        .take(MAX_MODEL_CANDIDATES)
        .map(|(_, candidate)| candidate.spec.to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ManifestRegistry;
    use test_case::test_case;

    const ANTHROPIC: &str = "anthropic";
    const OPENAI: &str = "openai";
    const COPILOT: &str = "copilot";
    const CATALOG: &[&str] = &[
        "anthropic/claude-haiku-4-5",
        "anthropic/claude-sonnet-4",
        "anthropic/claude-sonnet-4-20250514",
        "anthropic/claude-sonnet-4-5",
        "anthropic/claude-sonnet-4-6",
        "anthropic/claude-sonnet-5",
        "anthropic/claude-sonnet-5-5",
        "anthropic/claude-opus-4-6",
        "anthropic/claude-opus-4-8",
        "anthropic/claude-opus-5-5",
        "openai/gpt-5",
        "openai/gpt-5.6-terra",
        "openai/gpt-5.6-terra-preview",
        "openai/gpt-5.6-luna",
        "openai/gpt-5.6-sol",
        "openai/gpt-4o",
        "openai/gpt-4o-2024-08-06",
        "copilot/gpt-5.6-terra",
        "copilot/claude-sonnet-4.5",
    ];

    fn catalog() -> Vec<String> {
        CATALOG.iter().map(|spec| (*spec).to_owned()).collect()
    }

    #[test_case("anthropic/claude-opus-4-6", &[], "anthropic/claude-opus-4-6" ; "exact_spec")]
    #[test_case("Anthropic/Claude-Opus-4-6", &[], "anthropic/claude-opus-4-6" ; "exact_spec_any_case")]
    #[test_case("anthropic/sonnet", &[], "anthropic/claude-sonnet-5-5" ; "provider_scoped_name")]
    #[test_case("sonnet", &[ANTHROPIC], "anthropic/claude-sonnet-5-5" ; "newest_version_wins")]
    #[test_case("opus 4", &[], "anthropic/claude-opus-4-8" ; "version_prefix")]
    #[test_case("opus 4.6", &[], "anthropic/claude-opus-4-6" ; "dotted_version")]
    #[test_case("Claude Opus", &[], "anthropic/claude-opus-5-5" ; "spaced_words_any_case")]
    #[test_case("sonnet 4", &[ANTHROPIC], "anthropic/claude-sonnet-4-6" ; "newest_within_major")]
    #[test_case("gpt 4o", &[], "openai/gpt-4o" ; "alias_beats_dated_snapshot")]
    #[test_case("gpt-5.6-terra-preview", &[], "openai/gpt-5.6-terra-preview" ; "exact_id")]
    #[test_case("terra", &[OPENAI], "openai/gpt-5.6-terra" ; "preview_adds_a_word")]
    #[test_case("terra", &[COPILOT], "copilot/gpt-5.6-terra" ; "preferred_provider_breaks_tie")]
    #[test_case("haiku", &[OPENAI], "anthropic/claude-haiku-4-5" ; "only_provider_with_a_match")]
    fn a_name_resolves_to_one_model(query: &str, preferred: &[&str], expected: &str) {
        assert_eq!(find_model(query, &catalog(), preferred).unwrap(), expected);
    }

    #[test_case("terra", &[], &["copilot/gpt-5.6-terra", "openai/gpt-5.6-terra"] ; "two_providers_serve_it")]
    #[test_case("openai/gpt-5.6", &[], &["openai/gpt-5.6-luna", "openai/gpt-5.6-sol", "openai/gpt-5.6-terra"] ; "different_models_share_a_word")]
    #[test_case("gpt 5", &[OPENAI], &["openai/gpt-5.6-luna", "openai/gpt-5.6-sol", "openai/gpt-5.6-terra"] ; "newer_version_outranks_fewer_words")]
    #[test_case("sonnet 4.5", &[OPENAI], &["anthropic/claude-sonnet-4-5", "copilot/claude-sonnet-4.5"] ; "no_preferred_provider_matches")]
    fn an_ambiguous_name_lists_its_candidates(query: &str, preferred: &[&str], expected: &[&str]) {
        assert_eq!(
            find_model(query, &catalog(), preferred),
            Err(ModelLookupError::Ambiguous {
                query: query.to_owned(),
                candidates: expected.iter().map(|spec| (*spec).to_owned()).collect(),
            })
        );
    }

    #[test]
    fn an_unknown_name_suggests_models_sharing_a_word() {
        let Err(ModelLookupError::NotFound { suggestions, .. }) =
            find_model("sonnet 9", &catalog(), &[ANTHROPIC])
        else {
            panic!("an unknown version must not resolve");
        };
        assert_eq!(
            suggestions.first().map(String::as_str),
            Some("anthropic/claude-sonnet-5-5")
        );
        assert!(suggestions.iter().all(|spec| spec.contains("sonnet")));
    }

    #[test]
    fn an_unrelated_name_suggests_the_preferred_providers_models() {
        let Err(ModelLookupError::NotFound { suggestions, .. }) =
            find_model("llama", &catalog(), &[OPENAI])
        else {
            panic!("an unrelated name must not resolve");
        };
        assert!(!suggestions.is_empty());
        assert!(suggestions.iter().all(|spec| spec.starts_with(OPENAI)));
    }

    #[test]
    fn suggestions_are_bounded() {
        let many: Vec<String> = (0..MAX_MODEL_CANDIDATES * 2)
            .map(|index| format!("{OPENAI}/model-{index}"))
            .collect();
        let Err(ModelLookupError::NotFound { suggestions, .. }) =
            find_model("unknown", &many, &[OPENAI])
        else {
            panic!("an unrelated name must not resolve");
        };
        assert_eq!(suggestions.len(), MAX_MODEL_CANDIDATES);
    }

    #[test_case("" ; "empty")]
    #[test_case("   " ; "blank")]
    fn an_empty_name_is_rejected(query: &str) {
        assert_eq!(
            find_model(query, &catalog(), &[]),
            Err(ModelLookupError::Empty)
        );
    }

    const LOCAL: &str = "ninfer-4090";
    /// Modelled on what `caudra models` listed on 2026-10-13 with OpenAI,
    /// Anthropic and a local server signed in, plus the built-in GitHub
    /// Copilot catalog.
    const LISTED: &[&str] = &[
        "openai/gpt-5.2",
        "openai/gpt-5.3-codex",
        "openai/gpt-5.4",
        "openai/gpt-5.4-mini",
        "openai/gpt-5.5",
        "openai/gpt-5.6-luna",
        "openai/gpt-5.6-sol",
        "openai/gpt-5.6",
        "openai/gpt-5.6-terra",
        "openai/gpt-6-astra",
        "openai/gpt-6-luna",
        "openai/gpt-6-sol",
        "openai/gpt-6.1-sol",
        "openai/gpt-5.4-nano",
        "openai/gpt-4.1-mini",
        "openai/gpt-4.1",
        "openai/o3",
        "ninfer-4090/qwen3.8-27b",
        "ninfer-4090/qwen3.8-27b-caveman7",
        "ninfer-4090/qwen3.8-27b-distill-1",
        "ninfer-4090/qwen3.8-27b-distill-1.5-6500",
        "ninfer-4090/qwen3.8-27b-distill-1.5-8500",
        "ninfer-4090/qwen3.8-27b-grpo_sudoku",
        "ninfer-4090/qwen3.8-27b-random",
        "ninfer-4090/qwen3.8-27b-zero",
        "ninfer-4090/decision-1",
        "anthropic/claude-haiku-5-5",
        "anthropic/claude-haiku-4-5",
        "anthropic/claude-sonnet-4-5",
        "anthropic/claude-sonnet-4-6",
        "anthropic/claude-sonnet-5-5",
        "anthropic/claude-sonnet-5",
        "anthropic/claude-opus-4-5",
        "anthropic/claude-opus-4-6",
        "anthropic/claude-opus-4-7",
        "anthropic/claude-opus-4-8",
        "anthropic/claude-opus-5-5",
        "anthropic/claude-opus-5",
        "anthropic/claude-fable-5-1",
        "anthropic/claude-fable-5",
        "copilot/gpt-5.4-nano",
        "copilot/claude-haiku-4.5",
        "copilot/claude-haiku-5.5",
        "copilot/gemini-3.8-flash",
        "copilot/claude-sonnet-4.6",
        "copilot/claude-sonnet-5.5",
        "copilot/claude-sonnet-5",
        "copilot/kimi-k3",
        "copilot/gemini-4-argon",
        "copilot/gpt-6-luna",
        "copilot/gpt-6.1-sol",
        "copilot/gpt-6-sol",
        "copilot/gpt-5.6-luna",
        "copilot/gpt-5.6-sol",
        "copilot/gpt-5.6-terra",
        "copilot/gpt-5.3-codex",
        "copilot/claude-opus-5.5",
        "copilot/claude-opus-4.8-fast",
        "copilot/claude-fable-5",
        "copilot/grok-4.5",
        "copilot/grok-4.6",
        "copilot/grok-4.7",
    ];

    fn listed() -> Vec<String> {
        LISTED.iter().map(|spec| (*spec).to_owned()).collect()
    }

    #[test_case("sonnet", &[ANTHROPIC], "anthropic/claude-sonnet-5-5" ; "sonnet")]
    #[test_case("opus", &[ANTHROPIC], "anthropic/claude-opus-5-5" ; "opus")]
    #[test_case("opus 4.6", &[], "anthropic/claude-opus-4-6" ; "opus_4_6_only_anthropic_serves")]
    #[test_case("opus 4", &[ANTHROPIC], "anthropic/claude-opus-4-8" ; "opus_4_newest_point_release")]
    #[test_case("opus 5", &[ANTHROPIC], "anthropic/claude-opus-5-5" ; "opus_5_covers_5_5")]
    #[test_case("haiku", &[ANTHROPIC], "anthropic/claude-haiku-5-5" ; "haiku")]
    #[test_case("haiku 4", &[ANTHROPIC], "anthropic/claude-haiku-4-5" ; "haiku_4")]
    #[test_case("haiku", &[COPILOT], "copilot/claude-haiku-5.5" ; "copilot_haiku")]
    #[test_case("fable", &[ANTHROPIC], "anthropic/claude-fable-5-1" ; "fable")]
    #[test_case("anthropic/opus", &[OPENAI], "anthropic/claude-opus-5-5" ; "provider_beats_preference")]
    #[test_case("gpt", &[OPENAI], "openai/gpt-6.1-sol" ; "gpt_newest_release")]
    #[test_case("gpt 5.4", &[OPENAI], "openai/gpt-5.4" ; "gpt_5_4_without_mini_or_nano")]
    #[test_case("terra", &[OPENAI], "openai/gpt-5.6-terra" ; "terra")]
    #[test_case("luna", &[OPENAI], "openai/gpt-6-luna" ; "luna_newest_line")]
    #[test_case("sol", &[OPENAI], "openai/gpt-6.1-sol" ; "sol_point_release")]
    #[test_case("astra", &[ANTHROPIC], "openai/gpt-6-astra" ; "astra_only_openai_serves")]
    #[test_case("codex", &[OPENAI], "openai/gpt-5.3-codex" ; "codex")]
    #[test_case("mini", &[OPENAI], "openai/gpt-5.4-mini" ; "mini_newest")]
    #[test_case("o3", &[], "openai/o3" ; "o3")]
    #[test_case("qwen", &[ANTHROPIC], "ninfer-4090/qwen3.8-27b" ; "qwen_base_beats_numbered_variants")]
    #[test_case("caveman", &[], "ninfer-4090/qwen3.8-27b-caveman7" ; "stem_of_a_trailing_numbered_word")]
    #[test_case("distill", &[], "ninfer-4090/qwen3.8-27b-distill-1" ; "variant_numbers_add_words")]
    #[test_case("caveman7", &[], "ninfer-4090/qwen3.8-27b-caveman7" ; "a_variants_own_numbered_word")]
    #[test_case("distill 1.5", &[], "ninfer-4090/qwen3.8-27b-distill-1.5-8500" ; "variant_number_picks_its_latest_checkpoint")]
    #[test_case("distill 1.5 6500", &[], "ninfer-4090/qwen3.8-27b-distill-1.5-6500" ; "variant_checkpoint")]
    #[test_case("decision 1", &[], "ninfer-4090/decision-1" ; "trailing_number_as_version")]
    #[test_case("sudoku", &[], "ninfer-4090/qwen3.8-27b-grpo_sudoku" ; "underscore_separates_words")]
    #[test_case("qwen3.8", &[], "ninfer-4090/qwen3.8-27b" ; "qwen_numbered_word")]
    #[test_case("qwen zero", &[], "ninfer-4090/qwen3.8-27b-zero" ; "qwen_variant")]
    #[test_case("decision", &[], "ninfer-4090/decision-1" ; "local_model")]
    #[test_case("sonnet", &[COPILOT], "copilot/claude-sonnet-5.5" ; "copilot_dotted_sonnet")]
    #[test_case("opus", &[COPILOT], "copilot/claude-opus-5.5" ; "copilot_opus")]
    #[test_case("opus fast", &[], "copilot/claude-opus-4.8-fast" ; "copilot_fast_variant")]
    #[test_case("kimi", &[], "copilot/kimi-k3" ; "kimi_numbered_versions")]
    #[test_case("gemini", &[ANTHROPIC], "copilot/gemini-4-argon" ; "gemini_newest")]
    #[test_case("gemini flash", &[], "copilot/gemini-3.8-flash" ; "gemini_flash_newest")]
    #[test_case("argon", &[], "copilot/gemini-4-argon" ; "argon")]
    #[test_case("grok", &[], "copilot/grok-4.7" ; "grok")]
    #[test_case("gpt-5.6", &[OPENAI], "openai/gpt-5.6" ; "exact_alias_beats_siblings")]
    fn a_listed_model_resolves(query: &str, preferred: &[&str], expected: &str) {
        assert_eq!(find_model(query, &listed(), preferred).unwrap(), expected);
    }

    #[test_case("claude", &[ANTHROPIC], &["anthropic/claude-haiku-5-5", "anthropic/claude-opus-5-5", "anthropic/claude-sonnet-5-5"] ; "claude_names_a_vendor")]
    #[test_case("sonnet", &[OPENAI, LOCAL], &["anthropic/claude-sonnet-5-5", "copilot/claude-sonnet-5.5"] ; "two_providers_serve_sonnet")]
    #[test_case("sonnet 4.6", &[], &["anthropic/claude-sonnet-4-6", "copilot/claude-sonnet-4.6"] ; "spellings_differ_by_provider")]
    fn an_ambiguous_listed_name_lists_its_candidates(
        query: &str,
        preferred: &[&str],
        expected: &[&str],
    ) {
        assert_eq!(
            find_model(query, &listed(), preferred),
            Err(ModelLookupError::Ambiguous {
                query: query.to_owned(),
                candidates: expected.iter().map(|spec| (*spec).to_owned()).collect(),
            })
        );
    }

    #[test_case("opus 4.9" ; "unreleased_version")]
    #[test_case("qwen 4" ; "unserved_version")]
    #[test_case("llama" ; "unserved_family")]
    fn an_unlisted_model_is_not_found(query: &str) {
        let Err(ModelLookupError::NotFound { suggestions, .. }) =
            find_model(query, &listed(), &[ANTHROPIC])
        else {
            panic!("{query} must not resolve");
        };
        assert!(!suggestions.is_empty());
    }

    fn builtin_specs() -> Vec<String> {
        let mut specs: Vec<String> = ManifestRegistry::builtins()
            .iter()
            .flat_map(|manifest| {
                manifest
                    .models
                    .iter()
                    .flat_map(|entry| entry.prefixes)
                    .map(move |id| format!("{}{SPEC_SEPARATOR}{id}", manifest.slug))
            })
            .collect();
        specs.sort();
        specs.dedup();
        specs
    }

    #[test]
    fn every_builtin_model_resolves_by_its_spec_and_its_id() {
        let specs = builtin_specs();
        assert!(!specs.is_empty());
        for spec in &specs {
            let (provider, id) = spec.split_once(SPEC_SEPARATOR).unwrap();
            assert_eq!(find_model(spec, &specs, &[]).as_deref(), Ok(spec.as_str()));
            assert_eq!(
                find_model(id, &specs, &[provider]).as_deref(),
                Ok(spec.as_str()),
                "{id} on {provider}"
            );
        }
    }

    /// Checked against whatever the built-in catalogs hold now, so a catalog
    /// update that leaves a family unreachable fails here.
    #[test_case("anthropic", "opus")]
    #[test_case("anthropic", "sonnet")]
    #[test_case("anthropic", "haiku")]
    #[test_case("anthropic", "fable")]
    #[test_case("openai", "gpt")]
    #[test_case("openai", "terra")]
    #[test_case("openai", "luna")]
    #[test_case("openai", "sol")]
    #[test_case("openai", "codex")]
    #[test_case("copilot", "opus")]
    #[test_case("copilot", "kimi")]
    #[test_case("copilot", "gemini")]
    #[test_case("google", "gemini flash")]
    #[test_case("google", "argon")]
    #[test_case("xai", "grok")]
    #[test_case("deepseek", "deepseek pro")]
    fn a_builtin_family_resolves_on_the_preferred_provider(provider: &str, family: &str) {
        let resolved = find_model(family, &builtin_specs(), &[provider]).unwrap();
        let (on, id) = resolved.split_once(SPEC_SEPARATOR).unwrap();
        assert_eq!(on, provider);
        assert!(
            family.split_whitespace().all(|word| id.contains(word)),
            "{family} resolved to {resolved}"
        );
    }
}
