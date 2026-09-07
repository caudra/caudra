//! Reasoning effort vocabulary and the persisted thinking setting.
//!
//! Effort levels are raw provider strings, never a caudra enum: the set a model
//! accepts is declared per model (the models.dev `reasoning_options` shape this
//! module mirrors), so any ladder caudra invented here would be a guess that
//! silently changes what the user asked for. The one ordering that survives,
//! [`EFFORT_LEVELS`], exists solely to clamp a setting across a model switch.
//!
//! Lives in the storage crate, the leaf both the config and provider layers
//! depend on, so neither needs the other to parse or validate a setting.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::state::{self, SCOPE_GLOBAL, StateKey};
use crate::{StateClass, StateDir};

/// Floor for every token budget sent to a provider; some APIs reject smaller values.
pub const MIN_THINKING_BUDGET: u32 = 1024;

/// The level last chosen interactively for each model, so a restart reopens on
/// it. Keyed by model spec because a level is only meaningful against the
/// ladder that declared it: carrying one model's `max` onto a model that never
/// declared it means sending a level the endpoint rejects, or silently snapping
/// to something the user did not choose. Global rather than per project,
/// matching the selected model.
const SELECTED_BY_MODEL: StateKey = StateKey {
    name: "thinking.selected_by_model",
    class: StateClass::Persistent,
};

/// The single level chosen before the setting became per model. Read as the
/// fallback for a model with no choice of its own, so upgrading keeps the level
/// already in use instead of silently resetting it.
const SELECTED: StateKey = StateKey {
    name: "thinking.selected",
    class: StateClass::Persistent,
};

/// Every effort level the models.dev catalog declares, ascending. Of the 3004
/// models carrying an effort option, 3001 use only these and 3003 declare them
/// in this order, so its reasoning depths double as the fallback ladder for a
/// model that declares nothing.
pub const EFFORT_LEVELS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Wire spelling that disables reasoning on models whose effort parameter takes
/// an explicit opt-out instead of an omitted field.
pub const EFFORT_NONE: &str = "none";

/// Levels offered for a model that takes a token budget rather than named
/// levels. Two steps, because the declared bounds only justify two: the
/// ceiling, and half of it.
pub const BUDGET_LADDER: [&str; 2] = ["high", "max"];

/// Ceiling for a budget model that declared no maximum and whose output window
/// is unknown. Only reached when two independent sources are missing at once.
const FALLBACK_BUDGET_CEILING: u32 = 32_768;

/// Position of `level` in [`EFFORT_LEVELS`], or `None` for a spelling no
/// catalog provider has ever declared, which therefore cannot be ordered
/// against anything.
pub fn effort_rank(level: &str) -> Option<usize> {
    EFFORT_LEVELS.iter().position(|known| *known == level)
}

/// How a model spells reasoning control, mirroring the models.dev
/// `reasoning_options` union. A model may declare more than one: Claude Sonnet 5
/// takes both a toggle and an effort level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    /// Reasoning switches on and off with no level; the model picks its depth.
    Toggle,
    /// The exact strings the model's effort parameter accepts, in declaration
    /// order.
    Effort { values: Vec<String> },
    /// A token budget, bounded by whatever the API documents.
    BudgetTokens {
        #[serde(
            default,
            deserialize_with = "deserialize_bound",
            skip_serializing_if = "Option::is_none"
        )]
        min: Option<u32>,
        #[serde(
            default,
            deserialize_with = "deserialize_bound",
            skip_serializing_if = "Option::is_none"
        )]
        max: Option<u32>,
    },
}

/// A catalog bound, where a negative number is the sentinel for "the model
/// decides" rather than a real token count. Treated as no bound at all, since
/// that is what it means and a signed budget is not something to clamp against.
fn deserialize_bound<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u32>, D::Error> {
    let raw = Option::<i64>::deserialize(deserializer)?;
    Ok(raw.and_then(|value| u32::try_from(value).ok()))
}

/// The reasoning controls a model advertises. Empty means the model either
/// cannot reason or never told us how, which callers treat as "send nothing and
/// let the API default apply".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReasoningOptions(Vec<ReasoningOption>);

impl ReasoningOptions {
    pub fn new(options: Vec<ReasoningOption>) -> Self {
        Self(options)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Levels the model accepts, in declaration order. Empty when the model
    /// exposes only a toggle or a budget.
    pub fn efforts(&self) -> &[String] {
        self.0
            .iter()
            .find_map(|option| match option {
                ReasoningOption::Effort { values } => Some(values.as_slice()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// True when the model can be switched on without naming a level, which is
    /// what `adaptive` means for it.
    pub fn has_toggle(&self) -> bool {
        self.0
            .iter()
            .any(|option| matches!(option, ReasoningOption::Toggle))
    }

    /// The declared token-budget bounds, if the model takes a budget at all.
    pub fn budget_bounds(&self) -> Option<(Option<u32>, Option<u32>)> {
        self.0.iter().find_map(|option| match option {
            ReasoningOption::BudgetTokens { min, max } => Some((*min, *max)),
            _ => None,
        })
    }

    /// Whether the model can be asked not to reason at all. A declared `none`
    /// level is the opt-out for effort models; a toggle is the opt-out for the
    /// rest. Anything else reasons unconditionally.
    pub fn can_disable(&self) -> bool {
        self.is_empty() || self.has_toggle() || self.efforts().iter().any(|v| v == EFFORT_NONE)
    }

    /// The declared level nearest `level`: the exact match, else the highest
    /// declared level below it, else the lowest declared level. `None` when the
    /// model declares no levels, or when `level` is a spelling this model does
    /// not declare and the catalog has never seen, leaving nothing to order it
    /// against.
    pub fn snap(&self, level: &str) -> Option<&str> {
        let values = self.efforts();
        if let Some(exact) = values.iter().find(|declared| *declared == level) {
            return Some(exact);
        }
        let rank = effort_rank(level)?;
        values
            .iter()
            .filter_map(|declared| Some((effort_rank(declared)?, declared)))
            .filter(|(declared_rank, _)| *declared_rank < rank)
            .max_by_key(|(declared_rank, _)| *declared_rank)
            .map(|(_, declared)| declared.as_str())
            .or_else(|| values.first().map(String::as_str))
    }

    /// Depths to offer in the UI. A budget model gets the two steps its bounds
    /// justify; a model that declared nothing gets the canonical ladder so it
    /// still cycles through something useful. `none` is left out because it is
    /// how a model spells off, not a depth, and off is already its own setting.
    pub fn effort_ladder(&self) -> Vec<&str> {
        match self.efforts() {
            [] if self.budget_bounds().is_some() => BUDGET_LADDER.to_vec(),
            [] => EFFORT_LEVELS[1..].to_vec(),
            declared => declared
                .iter()
                .map(String::as_str)
                .filter(|level| *level != EFFORT_NONE)
                .collect(),
        }
    }

    /// Ceiling for a token budget: the declared maximum, else half the output
    /// window, since thinking and the answer are drawn from the same pool and a
    /// budget that claims all of it leaves nothing to reply with. `None`
    /// when neither is known, and an explicit budget then passes through as
    /// asked rather than meeting a limit caudra invented.
    pub fn budget_ceiling(&self, max_output: Option<u32>) -> Option<u32> {
        let declared = self.budget_bounds().and_then(|(_, max)| max);
        let from_window = max_output.map(|window| window / 2);
        match (declared, from_window) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (only, None) | (None, only) => only,
        }
    }

    /// An explicit budget held to the model's declared bounds and the protocol
    /// floor. The floor wins over a derived ceiling: a ceiling below it means
    /// the caller sized the output window too small for a model that reasons,
    /// not that a smaller budget became legal, and providers reject one.
    pub fn clamp_budget(&self, tokens: u32, max_output: Option<u32>) -> u32 {
        let floor = self
            .budget_bounds()
            .and_then(|(min, _)| min)
            .unwrap_or(MIN_THINKING_BUDGET)
            .max(MIN_THINKING_BUDGET);
        match self.budget_ceiling(max_output) {
            Some(ceiling) => tokens.clamp(floor, ceiling.max(floor)),
            None => tokens.max(floor),
        }
    }

    /// What an effort level means on a model that takes budgets instead of
    /// levels: the ceiling at the top of the ladder, half of it below. Two
    /// steps, because two is all the declared bounds justify -- a finer ladder
    /// would be a percentage table caudra made up.
    pub fn budget_for_effort(&self, level: &str, max_output: Option<u32>) -> u32 {
        let ceiling = self
            .budget_ceiling(max_output)
            .unwrap_or(FALLBACK_BUDGET_CEILING);
        let tokens = if level == BUDGET_LADDER[BUDGET_LADDER.len() - 1] {
            ceiling
        } else {
            ceiling / 2
        };
        self.clamp_budget(tokens, max_output)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThinkingParseError {
    #[error("thinking budget must be greater than zero")]
    BudgetZero,
    #[error("thinking budget {0} is out of range")]
    BudgetRange(String),
    #[error("unknown thinking level {level}, expected one of: {}", EFFORT_LEVELS.join(", "))]
    UnknownLevel { level: String },
}

/// The thinking setting as persisted and as the user typed it. Resolving it
/// against a model's [`ReasoningOptions`] happens at request time, so a level
/// stays exactly as asked while the user moves between models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum StoredThinking {
    Off,
    Adaptive,
    Effort { level: String },
    Budget { tokens: u32 },
}

impl StoredThinking {
    /// The one string-to-thinking parser: `/thinking`, `always_thinking`
    /// config, and the Lua agent API all delegate here. Typed levels are held to
    /// [`EFFORT_LEVELS`] because a level caudra cannot rank is a level it cannot
    /// snap, and an unrankable one would reach the API verbatim and be rejected.
    /// A model is still free to declare its own spellings; those are reached by
    /// picking from its ladder, not by typing.
    pub fn parse_setting(input: &str) -> Result<Self, ThinkingParseError> {
        let input = input.trim();
        match input {
            "off" => Ok(Self::Off),
            "adaptive" => Ok(Self::Adaptive),
            _ if input.chars().all(|c| c.is_ascii_digit()) => match input.parse::<u32>() {
                Ok(0) => Err(ThinkingParseError::BudgetZero),
                Ok(tokens) => Ok(Self::Budget { tokens }),
                Err(_) => Err(ThinkingParseError::BudgetRange(input.to_string())),
            },
            level => {
                let level = level.to_ascii_lowercase();
                if effort_rank(&level).is_none() {
                    return Err(ThinkingParseError::UnknownLevel { level });
                }
                Ok(Self::Effort { level })
            }
        }
    }
}

impl fmt::Display for StoredThinking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::Adaptive => f.write_str("adaptive"),
            Self::Effort { level } => f.write_str(level),
            Self::Budget { tokens } => write!(f, "{tokens}"),
        }
    }
}

impl FromStr for StoredThinking {
    type Err = ThinkingParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_setting(s)
    }
}

pub fn persist(dir: &StateDir, model_spec: &str, thinking: &StoredThinking) {
    let stored = state::update(
        dir,
        SCOPE_GLOBAL,
        SELECTED_BY_MODEL,
        |by_model: &mut HashMap<String, StoredThinking>| {
            by_model.insert(model_spec.to_owned(), thinking.clone());
        },
    );
    if let Err(error) = stored {
        warn!(%error, model = %model_spec, "failed to persist thinking level");
    }
}

/// The level last chosen for `model_spec`, falling back to the pre-per-model
/// setting. `None` when neither was stored or the row no longer deserializes,
/// which leaves the caller's own default standing.
pub fn read(dir: &StateDir, model_spec: &str) -> Option<StoredThinking> {
    let by_model: Option<HashMap<String, StoredThinking>> =
        state::get(dir, SCOPE_GLOBAL, SELECTED_BY_MODEL).unwrap_or_else(|error| {
            warn!(%error, "failed to read thinking levels");
            None
        });
    by_model
        .and_then(|mut by_model| by_model.remove(model_spec))
        .or_else(|| {
            state::get(dir, SCOPE_GLOBAL, SELECTED).unwrap_or_else(|error| {
                warn!(%error, "failed to read thinking level");
                None
            })
        })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        BUDGET_LADDER, EFFORT_LEVELS, MIN_THINKING_BUDGET, ReasoningOption, ReasoningOptions,
        SCOPE_GLOBAL, StateDir, StoredThinking, ThinkingParseError, effort_rank, state,
    };

    const ROUND_TRIP: &str = "a stored level must read back as written";
    const UNSET: &str = "an unwritten level must leave the caller's default alone";
    const PER_MODEL: &str = "a level belongs to the model it was chosen for";
    const LEGACY_CARRIES: &str =
        "a model with no choice of its own must inherit the pre-upgrade level";
    const MODEL_A: &str = "anthropic/claude-opus-5";
    const MODEL_B: &str = "ninfer-4090/qwen3.8-27b-lora";
    const SENTINEL_IS_NOT_A_BOUND: &str =
        "a negative bound means the model decides, not a token count to clamp against";
    /// The window the session title request used to ask for. Half of it is
    /// below the protocol floor, which is exactly how every title request
    /// reached Anthropic with `budget_tokens: 256` and came back a 400.
    const TIGHT_OUTPUT_WINDOW: u32 = 512;
    const FLOOR_IS_NOT_NEGOTIABLE: &str = "a window too small for the floor is a caller sizing bug, not a licence to send a budget no provider accepts";

    /// models.dev publishes `min: -1` on a handful of models. Rejecting it once
    /// cost the entire catalog, because one unreadable model failed the whole
    /// parse.
    #[test_case(r#"{"type":"budget_tokens","min":-1,"max":32768}"#, None, Some(32_768) ; "negative_min_is_no_min")]
    #[test_case(r#"{"type":"budget_tokens","min":1024,"max":-1}"#, Some(1_024), None ; "negative_max_is_no_max")]
    #[test_case(r#"{"type":"budget_tokens","min":1024,"max":32768}"#, Some(1_024), Some(32_768) ; "real_bounds_survive")]
    #[test_case(r#"{"type":"budget_tokens"}"#, None, None ; "absent_bounds")]
    fn budget_bounds_read_negative_sentinels_as_unbounded(
        json: &str,
        min: Option<u32>,
        max: Option<u32>,
    ) {
        let option: ReasoningOption = serde_json::from_str(json).expect(SENTINEL_IS_NOT_A_BOUND);
        assert_eq!(
            option,
            ReasoningOption::BudgetTokens { min, max },
            "{SENTINEL_IS_NOT_A_BOUND}"
        );
    }

    fn efforts(values: &[&str]) -> ReasoningOptions {
        ReasoningOptions::new(vec![ReasoningOption::Effort {
            values: values.iter().map(|v| (*v).to_string()).collect(),
        }])
    }

    #[test_case("off", StoredThinking::Off ; "off")]
    #[test_case("adaptive", StoredThinking::Adaptive ; "adaptive")]
    #[test_case("  high  ", StoredThinking::Effort { level: "high".into() } ; "trims")]
    #[test_case("XHigh", StoredThinking::Effort { level: "xhigh".into() } ; "lowercases")]
    #[test_case("2048", StoredThinking::Budget { tokens: 2048 } ; "budget")]
    fn parse_setting_reads_every_spelling(input: &str, expected: StoredThinking) {
        assert_eq!(StoredThinking::parse_setting(input).unwrap(), expected);
    }

    #[test]
    fn parse_setting_rejects_an_unknown_level() {
        assert_eq!(
            StoredThinking::parse_setting("turbo").unwrap_err(),
            ThinkingParseError::UnknownLevel {
                level: "turbo".into()
            }
        );
    }

    #[test]
    fn parse_setting_rejects_a_zero_budget() {
        assert_eq!(
            StoredThinking::parse_setting("0").unwrap_err(),
            ThinkingParseError::BudgetZero
        );
    }

    #[test]
    fn parse_setting_round_trips_through_display() {
        for input in ["off", "adaptive", "high", "4096"] {
            let parsed = StoredThinking::parse_setting(input).unwrap();
            assert_eq!(parsed.to_string(), input);
            assert_eq!(
                StoredThinking::parse_setting(&parsed.to_string()).unwrap(),
                parsed
            );
        }
    }

    #[test]
    fn canonical_ladder_ranks_ascending() {
        let ranks: Vec<usize> = EFFORT_LEVELS
            .iter()
            .map(|level| effort_rank(level).unwrap())
            .collect();
        assert!(ranks.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test_case("high", Some("high") ; "exact_match_is_kept")]
    #[test_case("xhigh", Some("high") ; "above_the_top_falls_to_the_top")]
    #[test_case("max", Some("high") ; "far_above_falls_to_the_top")]
    #[test_case("none", Some("low") ; "below_the_floor_rises_to_the_floor")]
    #[test_case("turbo", None ; "unrankable_level_has_no_neighbour")]
    fn snap_clamps_to_a_declared_level(level: &str, expected: Option<&str>) {
        assert_eq!(efforts(&["low", "medium", "high"]).snap(level), expected);
    }

    #[test]
    fn snap_returns_nothing_when_no_levels_are_declared() {
        assert_eq!(ReasoningOptions::default().snap("high"), None);
        assert_eq!(
            ReasoningOptions::new(vec![ReasoningOption::Toggle]).snap("high"),
            None
        );
    }

    #[test]
    fn snap_keeps_a_declared_level_the_catalog_has_never_seen() {
        assert_eq!(efforts(&["turbo", "high"]).snap("turbo"), Some("turbo"));
    }

    #[test]
    fn can_disable_reads_the_declared_opt_out() {
        assert!(efforts(&["none", "high"]).can_disable());
        assert!(!efforts(&["high", "max"]).can_disable());
        assert!(ReasoningOptions::new(vec![ReasoningOption::Toggle]).can_disable());
        assert!(ReasoningOptions::default().can_disable());
    }

    #[test]
    fn effort_ladder_falls_back_to_the_canonical_levels() {
        assert_eq!(
            ReasoningOptions::default().effort_ladder(),
            EFFORT_LEVELS[1..]
        );
        assert_eq!(efforts(&["high", "max"]).effort_ladder(), ["high", "max"]);
        assert_eq!(budget(Some(1024), None).effort_ladder(), BUDGET_LADDER);
    }

    fn budget(min: Option<u32>, max: Option<u32>) -> ReasoningOptions {
        ReasoningOptions::new(vec![ReasoningOption::BudgetTokens { min, max }])
    }

    #[test_case(budget(Some(128), Some(32_768)), Some(131_072), Some(32_768) ; "declared_max_wins_over_window")]
    #[test_case(budget(Some(1024), None), Some(64_000), Some(32_000) ; "half_window_when_undeclared")]
    #[test_case(budget(Some(1024), None), None, None ; "unknown_on_both_sides")]
    #[test_case(budget(None, Some(24_576)), None, Some(24_576) ; "declared_max_without_a_window")]
    fn budget_ceiling_takes_the_tighter_bound(
        options: ReasoningOptions,
        max_output: Option<u32>,
        expected: Option<u32>,
    ) {
        assert_eq!(options.budget_ceiling(max_output), expected);
    }

    #[test_case(50_000, Some(32_768) ; "above_the_ceiling_is_capped")]
    #[test_case(64, Some(1_024) ; "below_the_floor_is_raised")]
    #[test_case(8_192, Some(8_192) ; "inside_the_bounds_passes_through")]
    fn clamp_budget_holds_the_declared_bounds(tokens: u32, expected: Option<u32>) {
        let options = budget(Some(128), Some(32_768));
        assert_eq!(options.clamp_budget(tokens, None), expected.unwrap());
    }

    #[test]
    fn clamp_budget_never_caps_when_no_ceiling_is_known() {
        assert_eq!(budget(None, None).clamp_budget(1_000_000, None), 1_000_000);
    }

    #[test_case("max", 32_000 ; "top_of_the_ladder_takes_the_ceiling")]
    #[test_case("high", 16_000 ; "below_the_top_takes_half")]
    fn budget_for_effort_derives_two_steps(level: &str, expected: u32) {
        // Claude Sonnet 4.5: declares only a floor, so the window sets the ceiling.
        let options = budget(Some(1024), None);
        assert_eq!(options.budget_for_effort(level, Some(64_000)), expected);
    }

    #[test_case(64 ; "below_the_floor")]
    #[test_case(8_192 ; "above_the_ceiling")]
    fn clamp_budget_keeps_the_floor_over_a_tighter_ceiling(tokens: u32) {
        let clamped = budget(Some(1024), None).clamp_budget(tokens, Some(TIGHT_OUTPUT_WINDOW));

        assert!(clamped >= MIN_THINKING_BUDGET, "{FLOOR_IS_NOT_NEGOTIABLE}");
    }

    #[test_case("max" ; "top_of_the_ladder")]
    #[test_case("high" ; "below_the_top")]
    fn budget_for_effort_never_falls_below_the_floor(level: &str) {
        let derived = budget(Some(1024), None).budget_for_effort(level, Some(TIGHT_OUTPUT_WINDOW));

        assert!(derived >= MIN_THINKING_BUDGET, "{FLOOR_IS_NOT_NEGOTIABLE}");
    }

    #[test]
    fn reasoning_options_round_trip_through_json() {
        let options = ReasoningOptions::new(vec![
            ReasoningOption::Toggle,
            ReasoningOption::Effort {
                values: vec!["low".into(), "high".into()],
            },
            ReasoningOption::BudgetTokens {
                min: Some(1024),
                max: None,
            },
        ]);
        let json = serde_json::to_string(&options).unwrap();
        assert_eq!(
            json,
            r#"[{"type":"toggle"},{"type":"effort","values":["low","high"]},{"type":"budget_tokens","min":1024}]"#
        );
        assert_eq!(
            serde_json::from_str::<ReasoningOptions>(&json).unwrap(),
            options
        );
    }

    #[test]
    fn stored_thinking_deserializes_legacy_effort_sessions() {
        let legacy = r#"{"kind":"effort","level":"medium"}"#;
        assert_eq!(
            serde_json::from_str::<StoredThinking>(legacy).unwrap(),
            StoredThinking::Effort {
                level: "medium".into()
            }
        );
    }

    #[test_case(StoredThinking::Off ; "off")]
    #[test_case(StoredThinking::Adaptive ; "adaptive")]
    #[test_case(StoredThinking::Effort { level: "high".into() } ; "effort")]
    #[test_case(StoredThinking::Budget { tokens: 8192 } ; "budget")]
    fn a_chosen_level_round_trips(thinking: StoredThinking) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        super::persist(&dir, MODEL_A, &thinking);
        assert_eq!(super::read(&dir, MODEL_A), Some(thinking), "{ROUND_TRIP}");
    }

    #[test]
    fn an_unchosen_level_leaves_the_caller_its_default() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        assert_eq!(super::read(&dir, MODEL_A), None, "{UNSET}");
    }

    #[test]
    fn the_latest_choice_replaces_the_last() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        super::persist(&dir, MODEL_A, &StoredThinking::Adaptive);
        super::persist(&dir, MODEL_A, &StoredThinking::Off);
        assert_eq!(
            super::read(&dir, MODEL_A),
            Some(StoredThinking::Off),
            "{ROUND_TRIP}"
        );
    }

    /// The whole point: `max` chosen on a model that declares it must not
    /// follow the user onto one that never did.
    #[test]
    fn a_level_chosen_for_one_model_does_not_reach_another() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        super::persist(
            &dir,
            MODEL_A,
            &StoredThinking::Effort {
                level: "max".into(),
            },
        );
        super::persist(&dir, MODEL_B, &StoredThinking::Off);

        assert_eq!(
            super::read(&dir, MODEL_A),
            Some(StoredThinking::Effort {
                level: "max".into()
            }),
            "{PER_MODEL}"
        );
        assert_eq!(
            super::read(&dir, MODEL_B),
            Some(StoredThinking::Off),
            "{PER_MODEL}"
        );
    }

    /// Upgrading must not silently reset a level the user already set.
    #[test]
    fn a_model_with_no_choice_inherits_the_pre_upgrade_level() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        state::set(
            &dir,
            SCOPE_GLOBAL,
            super::SELECTED,
            &StoredThinking::Adaptive,
        )
        .unwrap();

        assert_eq!(
            super::read(&dir, MODEL_A),
            Some(StoredThinking::Adaptive),
            "{LEGACY_CARRIES}"
        );

        super::persist(&dir, MODEL_A, &StoredThinking::Off);
        assert_eq!(
            super::read(&dir, MODEL_A),
            Some(StoredThinking::Off),
            "{PER_MODEL}"
        );
        assert_eq!(
            super::read(&dir, MODEL_B),
            Some(StoredThinking::Adaptive),
            "{LEGACY_CARRIES}"
        );
    }
}
