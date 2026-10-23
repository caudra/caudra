//! pstack's model roles: the kinds of delegated work its pages name, the choices the user stores
//! for them in `/settings`, and the table the index and every page that points at it carry.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const STORED_ROLES_VERSION: u32 = 1;
/// How a page points at the table. A served page that holds it ends with the table.
pub const MODEL_ROLES_REF: &str = "the **Model roles** table";
/// The effort auto seats aim for when the user stored none.
pub const DEFAULT_EFFORT: &str = "xhigh";
const EFFORT_LADDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];
const PANEL_SEATS: usize = 2;
const HEADING: &str = "## Model roles\n\n";
const TABLE_HEAD: &str = "| Kind | task arguments | Source |\n| --- | --- | --- |\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Code,
    Hardest,
    Judgment,
    Verify,
    Panel,
    CrossJudge,
}

impl Kind {
    /// Panel before Cross-judge pool, which resolves from the Panel's seats.
    pub const ALL: [Self; 6] = [
        Self::Code,
        Self::Hardest,
        Self::Judgment,
        Self::Verify,
        Self::Panel,
        Self::CrossJudge,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Code => "Code",
            Self::Hardest => "Hardest",
            Self::Judgment => "Judgment",
            Self::Verify => "Verify",
            Self::Panel => "Panel",
            Self::CrossJudge => "Cross-judge pool",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Code => "Code-writing delegates, explorers, and investigators.",
            Self::Hardest => "The hardest changes, where the strongest model pays for itself.",
            Self::Judgment => "Prose, judgment calls, explainers, and synthesizers.",
            Self::Verify => "Verification lanes, such as swarm workers.",
            Self::Panel => {
                "The diverse panel for arena runners, architect runners, and interrogate \
                 reviewers, one task per seat."
            }
            Self::CrossJudge => "The pool arena draws its cross-judges from, one task per seat.",
        }
    }

    /// Whether the kind holds several seats, each its own task.
    pub const fn is_list(self) -> bool {
        matches!(self, Self::Panel | Self::CrossJudge)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "seat", rename_all = "snake_case")]
pub enum Seat {
    Model {
        model: String,
        #[serde(default)]
        thinking: Option<String>,
    },
    Inherit,
}

/// What the user chose in `/settings`. A kind absent from `kinds` is resolved automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StoredRoles {
    pub version: u32,
    pub effort: Option<String>,
    pub kinds: BTreeMap<Kind, Vec<Seat>>,
}

impl Default for StoredRoles {
    fn default() -> Self {
        Self {
            version: STORED_ROLES_VERSION,
            effort: None,
            kinds: BTreeMap::new(),
        }
    }
}

/// A model `task` can run, as auto seats need to know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub spec: String,
    pub provider: String,
    pub best_lane: bool,
    /// The thinking effort levels the model takes, low to high; empty when it has no control.
    pub levels: Vec<String>,
}

pub struct ModelFacts<'a> {
    /// The allowed favorites, newest first.
    pub favorites: Vec<Candidate>,
    /// The allowed curated flagship of each signed-in provider, in Caudra's provider order: what
    /// auto seats take after the favorites run out.
    pub flagships: Vec<Candidate>,
    pub task_model_choice: bool,
    /// The usable model a stored name means, or `None` when `task` could not run it.
    pub known: &'a dyn Fn(&str) -> Option<Candidate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleTable {
    pub rows: Vec<RoleRow>,
    pub task_model_choice: bool,
    pub effort: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRow {
    pub kind: Kind,
    pub seats: Vec<ResolvedSeat>,
    pub source: Source,
}

/// The `task` call's `model` and `thinking`; no model means `inherit`, which omits both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedSeat {
    pub model: Option<String>,
    pub thinking: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Set,
    Auto(AutoReason),
    /// The stored choice names a model `task` cannot run, so the kind fell back to auto.
    Replaced {
        stored: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoReason {
    SubagentJob,
    BestFavorite,
    Flagship,
    PanelFavorites,
    PanelFlagships,
    PanelFlagshipAdded,
    NotDiverse,
    FromPanel,
    NoModel,
}

impl AutoReason {
    pub const fn describe(self) -> &'static str {
        match self {
            Self::SubagentJob => "auto: runs as a Subagent job",
            Self::BestFavorite => "auto: newest Best-lane favorite",
            Self::Flagship => "auto: no Best-lane favorite, so a signed-in provider's flagship",
            Self::PanelFavorites => {
                "auto: newest favorites on different providers, Best lane first"
            }
            Self::PanelFlagships => "auto: signed-in providers' flagships, one per provider",
            Self::PanelFlagshipAdded => {
                "auto: favorites, plus a signed-in provider's flagship for a second provider"
            }
            Self::NotDiverse => {
                "auto: every model to hand shares one provider, so the seat repeats"
            }
            Self::FromPanel => "auto: the Panel's seats",
            Self::NoModel => "auto: no favorite, and no signed-in provider with a flagship",
        }
    }
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Self::Set => "set in /settings".to_owned(),
            Self::Auto(reason) => reason.describe().to_owned(),
            Self::Replaced { stored } => format!("auto: your stored {stored} isn't usable"),
        }
    }
}

impl ResolvedSeat {
    fn auto(favorite: &Candidate, effort: &str) -> Self {
        Self {
            model: Some(favorite.spec.clone()),
            thinking: snap(effort, &favorite.levels),
        }
    }

    pub fn arguments(&self) -> String {
        match (&self.model, &self.thinking) {
            (None, _) => "`inherit`".to_owned(),
            (Some(model), None) => format!("`model: \"{model}\"`"),
            (Some(model), Some(thinking)) => {
                format!("`model: \"{model}\", thinking: \"{thinking}\"`")
            }
        }
    }
}

impl RoleTable {
    pub fn resolve(stored: &StoredRoles, facts: &ModelFacts) -> Self {
        let effort = stored
            .effort
            .clone()
            .unwrap_or_else(|| DEFAULT_EFFORT.to_owned());
        let mut rows: Vec<RoleRow> = Vec::with_capacity(Kind::ALL.len());
        for kind in Kind::ALL {
            let panel = rows
                .iter()
                .find(|row| row.kind == Kind::Panel)
                .map_or(&[][..], |row| row.seats.as_slice());
            let automatic = || auto(kind, facts, &effort, panel);
            let (seats, source) = match stored.kinds.get(&kind).filter(|seats| !seats.is_empty()) {
                None => {
                    let (seats, reason) = automatic();
                    (seats, Source::Auto(reason))
                }
                Some(seats) => match chosen(kind, seats, facts, &effort) {
                    Ok(seats) => (seats, Source::Set),
                    Err(stored) => (automatic().0, Source::Replaced { stored }),
                },
            };
            let seats = match facts.task_model_choice {
                true => seats,
                false => vec![ResolvedSeat::default(); seats.len()],
            };
            rows.push(RoleRow {
                kind,
                seats,
                source,
            });
        }
        Self {
            rows,
            task_model_choice: facts.task_model_choice,
            effort,
        }
    }

    pub fn row(&self, kind: Kind) -> Option<&RoleRow> {
        self.rows.iter().find(|row| row.kind == kind)
    }

    /// The `## Model roles` section the index and the pages that point at it carry.
    pub fn render(&self) -> String {
        if !self.task_model_choice {
            return format!(
                "{HEADING}`task` has no `model` or `thinking` here (`agent.task_model_choice` is \
                 off), so every kind runs as `inherit`: omit both.\n"
            );
        }
        let mut text = format!(
            "{HEADING}Each kind of delegated work runs on its rows below. Pass a row's task \
             arguments as the `task` call's `model` and `thinking`. `inherit` means omit both, so \
             the task runs on the profile's `subagent_model`, else the Subagent model. To adjust \
             effort, change only `thinking`. Panel and Cross-judge pool rows are one task per \
             seat. If `task` rejects a value, run that seat as `inherit` and say so. Auto rows aim \
             for effort `{}`, snapped down to a level the model takes. The user changes these in \
             `/settings` → pstack.\n\n{TABLE_HEAD}",
            self.effort
        );
        for row in &self.rows {
            let source = row.source.describe();
            for seat in &row.seats {
                text.push_str(&format!(
                    "| {} | {} | {source} |\n",
                    row.kind.label(),
                    seat.arguments()
                ));
            }
        }
        text
    }
}

fn auto(
    kind: Kind,
    facts: &ModelFacts,
    effort: &str,
    panel: &[ResolvedSeat],
) -> (Vec<ResolvedSeat>, AutoReason) {
    let best_favorite = || facts.favorites.iter().find(|favorite| favorite.best_lane);
    match kind {
        Kind::Code | Kind::Verify => (vec![ResolvedSeat::default()], AutoReason::SubagentJob),
        Kind::Hardest | Kind::Judgment => match (best_favorite(), facts.flagships.first()) {
            (Some(best), _) => (
                vec![ResolvedSeat::auto(best, effort)],
                AutoReason::BestFavorite,
            ),
            (None, Some(flagship)) => (
                vec![ResolvedSeat::auto(flagship, effort)],
                AutoReason::Flagship,
            ),
            (None, None) => (vec![ResolvedSeat::default()], AutoReason::NoModel),
        },
        Kind::Panel => auto_panel(facts, effort),
        Kind::CrossJudge => (panel.to_vec(), AutoReason::FromPanel),
    }
}

/// Two seats on distinct providers, drawn from the favorites, Best lane first, then the
/// flagships: a panel exists to compare models, so any of them beats running one model twice.
fn auto_panel(facts: &ModelFacts, effort: &str) -> (Vec<ResolvedSeat>, AutoReason) {
    let (best, rest): (Vec<&Candidate>, Vec<&Candidate>) = facts
        .favorites
        .iter()
        .partition(|favorite| favorite.best_lane);
    let pool: Vec<&Candidate> = best
        .into_iter()
        .chain(rest)
        .chain(&facts.flagships)
        .collect();
    let Some(&first) = pool.first() else {
        return (
            vec![ResolvedSeat::default(); PANEL_SEATS],
            AutoReason::NoModel,
        );
    };
    let second = pool
        .iter()
        .position(|candidate| candidate.provider != first.provider);
    match second {
        Some(at) => (
            vec![
                ResolvedSeat::auto(first, effort),
                ResolvedSeat::auto(pool[at], effort),
            ],
            match facts.favorites.len() {
                0 => AutoReason::PanelFlagships,
                favorites if at < favorites => AutoReason::PanelFavorites,
                _ => AutoReason::PanelFlagshipAdded,
            },
        ),
        None => (
            vec![ResolvedSeat::auto(first, effort); PANEL_SEATS],
            AutoReason::NotDiverse,
        ),
    }
}

/// The stored seats as `task` arguments, or the first unusable one as the user wrote it.
fn chosen(
    kind: Kind,
    seats: &[Seat],
    facts: &ModelFacts,
    effort: &str,
) -> Result<Vec<ResolvedSeat>, String> {
    let kept = match kind.is_list() {
        true => seats.len(),
        false => 1,
    };
    seats
        .iter()
        .take(kept)
        .map(|seat| match seat {
            Seat::Inherit => Ok(ResolvedSeat::default()),
            Seat::Model { model, thinking } => {
                let known = (facts.known)(model).ok_or_else(|| match thinking {
                    Some(thinking) => format!("{model} {thinking}"),
                    None => model.clone(),
                })?;
                Ok(ResolvedSeat {
                    thinking: thinking.clone().or_else(|| snap(effort, &known.levels)),
                    model: Some(known.spec),
                })
            }
        })
        .collect()
}

/// The highest of `levels` at or below `effort` on the ladder, else the lowest.
fn snap(effort: &str, levels: &[String]) -> Option<String> {
    let rank = |level: &str| EFFORT_LADDER.iter().position(|known| *known == level);
    let ceiling = rank(effort);
    levels
        .iter()
        .rev()
        .find(|level| matches!((rank(level), ceiling), (Some(at), Some(top)) if at <= top))
        .or_else(|| levels.first())
        .cloned()
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const OPUS: &str = "anthropic/claude-opus-5-5";
    const OPUS_SHORT: &str = "claude-opus-5-5";
    const SONNET: &str = "anthropic/claude-sonnet-5-5";
    const GROK: &str = "xai/grok-4.7";
    const GROK_SHORT: &str = "grok-4.7";
    const GROK_FAST: &str = "xai/grok-4.7-fast";
    const ASTRA: &str = "openai/gpt-6-astra";
    const XHIGH: &str = "xhigh";
    const HIGH: &str = "high";
    const LOW: &str = "low";
    const MAX: &str = "max";
    const FULL: &[&str] = &["low", "medium", "high", "xhigh", "max"];
    const TO_XHIGH: &[&str] = &["low", "medium", "high", "xhigh"];
    const TO_HIGH: &[&str] = &["low", "medium", "high"];
    const NO_LEVELS: &[&str] = &[];
    const UNUSABLE: &str = "auto: your stored grok-4.7 xhigh isn't usable";
    const OFF_LINE: &str = "so every kind runs as `inherit`";

    fn favorite(spec: &str, best_lane: bool, levels: &[&str]) -> Candidate {
        Candidate {
            spec: spec.to_owned(),
            provider: spec.split_once('/').unwrap().0.to_owned(),
            best_lane,
            levels: levels.iter().map(|level| (*level).to_owned()).collect(),
        }
    }

    fn table(
        favorites: Vec<Candidate>,
        stored: &StoredRoles,
        task_model_choice: bool,
    ) -> RoleTable {
        table_with(favorites, Vec::new(), stored, task_model_choice)
    }

    fn table_with(
        favorites: Vec<Candidate>,
        flagships: Vec<Candidate>,
        stored: &StoredRoles,
        task_model_choice: bool,
    ) -> RoleTable {
        let usable = favorites.clone();
        let known = move |name: &str| {
            usable
                .iter()
                .find(|favorite| {
                    favorite.spec == name || favorite.spec.split_once('/').unwrap().1 == name
                })
                .cloned()
        };
        RoleTable::resolve(
            stored,
            &ModelFacts {
                favorites,
                flagships,
                task_model_choice,
                known: &known,
            },
        )
    }

    fn flagships() -> Vec<Candidate> {
        vec![favorite(OPUS, true, FULL), favorite(ASTRA, true, FULL)]
    }

    fn seat(model: &str, thinking: Option<&str>) -> ResolvedSeat {
        ResolvedSeat {
            model: Some(model.to_owned()),
            thinking: thinking.map(str::to_owned),
        }
    }

    fn stored(kind: Kind, model: &str, thinking: Option<&str>) -> StoredRoles {
        StoredRoles {
            kinds: BTreeMap::from([(
                kind,
                vec![Seat::Model {
                    model: model.to_owned(),
                    thinking: thinking.map(str::to_owned),
                }],
            )]),
            ..StoredRoles::default()
        }
    }

    fn row(table: &RoleTable, kind: Kind) -> (Vec<ResolvedSeat>, Source) {
        let row = table.row(kind).unwrap();
        (row.seats.clone(), row.source.clone())
    }

    #[test]
    fn without_favorites_or_flagships_every_kind_inherits() {
        let table = table(Vec::new(), &StoredRoles::default(), true);
        let inherit = ResolvedSeat::default();
        assert_eq!(
            row(&table, Kind::Code),
            (vec![inherit.clone()], Source::Auto(AutoReason::SubagentJob))
        );
        assert_eq!(
            row(&table, Kind::Hardest),
            (vec![inherit.clone()], Source::Auto(AutoReason::NoModel))
        );
        assert_eq!(
            row(&table, Kind::Panel),
            (
                vec![inherit.clone(); PANEL_SEATS],
                Source::Auto(AutoReason::NoModel)
            )
        );
        assert_eq!(
            row(&table, Kind::CrossJudge),
            (
                vec![inherit; PANEL_SEATS],
                Source::Auto(AutoReason::FromPanel)
            )
        );
        assert_eq!(table.effort, DEFAULT_EFFORT);
    }

    #[test]
    fn one_best_favorite_fills_every_seat_and_the_panel_repeats() {
        let table = table(
            vec![favorite(OPUS, true, FULL)],
            &StoredRoles::default(),
            true,
        );
        let opus = seat(OPUS, Some(XHIGH));
        for kind in [Kind::Hardest, Kind::Judgment] {
            assert_eq!(
                row(&table, kind),
                (vec![opus.clone()], Source::Auto(AutoReason::BestFavorite))
            );
        }
        assert_eq!(
            row(&table, Kind::Panel),
            (
                vec![opus.clone(); PANEL_SEATS],
                Source::Auto(AutoReason::NotDiverse)
            )
        );
        assert_eq!(row(&table, Kind::Verify).0, vec![ResolvedSeat::default()]);
    }

    #[test]
    fn the_panel_spans_providers_newest_first() {
        let table = table(
            vec![
                favorite(SONNET, true, FULL),
                favorite(OPUS, true, FULL),
                favorite(GROK, true, TO_XHIGH),
            ],
            &StoredRoles::default(),
            true,
        );
        let panel = vec![seat(SONNET, Some(XHIGH)), seat(GROK, Some(XHIGH))];
        assert_eq!(row(&table, Kind::Hardest).0, [seat(SONNET, Some(XHIGH))]);
        assert_eq!(
            row(&table, Kind::Panel),
            (panel.clone(), Source::Auto(AutoReason::PanelFavorites))
        );
        assert_eq!(
            row(&table, Kind::CrossJudge),
            (panel, Source::Auto(AutoReason::FromPanel))
        );
    }

    #[test]
    fn the_panel_takes_another_provider_from_any_lane() {
        let table = table(
            vec![
                favorite(OPUS, true, FULL),
                favorite(GROK_FAST, false, NO_LEVELS),
            ],
            &StoredRoles::default(),
            true,
        );
        assert_eq!(
            row(&table, Kind::Panel),
            (
                vec![seat(OPUS, Some(XHIGH)), seat(GROK_FAST, None)],
                Source::Auto(AutoReason::PanelFavorites)
            )
        );
    }

    #[test]
    fn without_a_best_lane_favorite_the_panel_still_spans_providers() {
        let table = table(
            vec![
                favorite(GROK_FAST, false, NO_LEVELS),
                favorite(SONNET, false, FULL),
            ],
            &StoredRoles::default(),
            true,
        );
        assert_eq!(
            row(&table, Kind::Panel),
            (
                vec![seat(GROK_FAST, None), seat(SONNET, Some(XHIGH))],
                Source::Auto(AutoReason::PanelFavorites)
            )
        );
        assert_eq!(
            row(&table, Kind::Judgment),
            (
                vec![ResolvedSeat::default()],
                Source::Auto(AutoReason::NoModel)
            )
        );
    }

    #[test]
    fn without_favorites_the_flagships_fill_the_seats() {
        let table = table_with(Vec::new(), flagships(), &StoredRoles::default(), true);
        let opus = seat(OPUS, Some(XHIGH));
        for kind in [Kind::Hardest, Kind::Judgment] {
            assert_eq!(
                row(&table, kind),
                (vec![opus.clone()], Source::Auto(AutoReason::Flagship))
            );
        }
        assert_eq!(
            row(&table, Kind::Panel),
            (
                vec![opus, seat(ASTRA, Some(XHIGH))],
                Source::Auto(AutoReason::PanelFlagships)
            )
        );
        assert_eq!(
            row(&table, Kind::Code),
            (
                vec![ResolvedSeat::default()],
                Source::Auto(AutoReason::SubagentJob)
            )
        );
    }

    #[test]
    fn a_flagship_completes_a_panel_the_favorites_cannot() {
        let table = table_with(
            vec![favorite(SONNET, true, TO_HIGH)],
            flagships(),
            &StoredRoles::default(),
            true,
        );
        assert_eq!(
            row(&table, Kind::Hardest),
            (
                vec![seat(SONNET, Some(HIGH))],
                Source::Auto(AutoReason::BestFavorite)
            )
        );
        assert_eq!(
            row(&table, Kind::Panel),
            (
                vec![seat(SONNET, Some(HIGH)), seat(ASTRA, Some(XHIGH))],
                Source::Auto(AutoReason::PanelFlagshipAdded)
            )
        );
    }

    #[test]
    fn a_favorite_outranks_a_flagship_for_the_second_seat() {
        let table = table_with(
            vec![
                favorite(OPUS, true, FULL),
                favorite(GROK_FAST, false, NO_LEVELS),
            ],
            flagships(),
            &StoredRoles::default(),
            true,
        );
        assert_eq!(
            row(&table, Kind::Panel),
            (
                vec![seat(OPUS, Some(XHIGH)), seat(GROK_FAST, None)],
                Source::Auto(AutoReason::PanelFavorites)
            )
        );
    }

    #[test]
    fn an_unusable_stored_model_falls_back_to_auto() {
        let table = table(
            vec![favorite(OPUS, true, FULL)],
            &stored(Kind::Hardest, GROK_SHORT, Some(XHIGH)),
            true,
        );
        let (seats, source) = row(&table, Kind::Hardest);
        assert_eq!(seats, [seat(OPUS, Some(XHIGH))]);
        assert_eq!(source.describe(), UNUSABLE);
        assert!(table.render().contains(UNUSABLE));
    }

    #[test]
    fn a_stored_short_name_resolves_to_its_spec() {
        let table = table(
            vec![favorite(OPUS, true, TO_HIGH)],
            &stored(Kind::Code, OPUS_SHORT, None),
            true,
        );
        assert_eq!(
            row(&table, Kind::Code),
            (vec![seat(OPUS, Some(HIGH))], Source::Set)
        );
    }

    #[test]
    fn a_stored_seat_keeps_its_own_thinking() {
        let mut roles = stored(Kind::Judgment, OPUS, Some(LOW));
        roles.effort = Some(MAX.to_owned());
        let table = table(vec![favorite(OPUS, true, FULL)], &roles, true);
        assert_eq!(
            row(&table, Kind::Judgment),
            (vec![seat(OPUS, Some(LOW))], Source::Set)
        );
        assert_eq!(row(&table, Kind::Hardest).0, [seat(OPUS, Some(MAX))]);
    }

    #[test_case(MAX, TO_XHIGH, Some(XHIGH) ; "max_snaps_down_to_xhigh")]
    #[test_case(XHIGH, TO_HIGH, Some(HIGH) ; "xhigh_snaps_down_to_a_model_topping_at_high")]
    #[test_case(XHIGH, FULL, Some(XHIGH) ; "an_offered_level_is_kept")]
    #[test_case("minimal", TO_HIGH, Some(LOW) ; "below_every_level_takes_the_lowest")]
    #[test_case(XHIGH, NO_LEVELS, None ; "no_levels_means_no_thinking")]
    fn effort_snaps_down_to_an_offered_level(
        effort: &str,
        levels: &[&str],
        expected: Option<&str>,
    ) {
        let levels: Vec<String> = levels.iter().map(|level| (*level).to_owned()).collect();
        assert_eq!(snap(effort, &levels).as_deref(), expected);
    }

    #[test]
    fn without_task_model_choice_every_seat_inherits() {
        let table = table(
            vec![favorite(OPUS, true, FULL), favorite(GROK, true, FULL)],
            &stored(Kind::Code, OPUS, None),
            false,
        );
        assert!(
            table
                .rows
                .iter()
                .flat_map(|row| &row.seats)
                .all(|seat| seat.model.is_none() && seat.thinking.is_none())
        );
        let rendered = table.render();
        assert!(rendered.contains(OFF_LINE), "{rendered}");
        assert!(!rendered.contains(TABLE_HEAD), "{rendered}");
    }

    #[test]
    fn the_render_gives_each_seat_a_row() {
        let rendered = table(
            vec![favorite(OPUS, true, FULL), favorite(GROK, true, FULL)],
            &StoredRoles::default(),
            true,
        )
        .render();
        assert!(rendered.starts_with(HEADING), "{rendered}");
        assert!(rendered.contains(TABLE_HEAD), "{rendered}");
        let panel_rows = rendered
            .lines()
            .filter(|line| line.starts_with("| Panel |"))
            .count();
        assert_eq!(panel_rows, PANEL_SEATS, "{rendered}");
        assert!(
            rendered.contains(&format!(
                "| Hardest | `model: \"{OPUS}\", thinking: \"{XHIGH}\"` | {} |",
                AutoReason::BestFavorite.describe()
            )),
            "{rendered}"
        );
        assert!(rendered.contains("| Code | `inherit` |"), "{rendered}");
    }

    #[test]
    fn stored_roles_survive_a_serde_round_trip() {
        let mut roles = stored(Kind::Panel, OPUS, Some(HIGH));
        roles.effort = Some(MAX.to_owned());
        roles
            .kinds
            .get_mut(&Kind::Panel)
            .unwrap()
            .push(Seat::Inherit);
        roles.kinds.insert(Kind::CrossJudge, vec![Seat::Inherit]);
        let text = serde_json::to_string(&roles).unwrap();
        assert_eq!(serde_json::from_str::<StoredRoles>(&text).unwrap(), roles);
        assert_eq!(
            serde_json::from_str::<StoredRoles>("{}").unwrap(),
            StoredRoles::default()
        );
    }
}
