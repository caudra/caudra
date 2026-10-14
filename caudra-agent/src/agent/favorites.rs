//! The models the user starred, as the main agent reads them and as a task's
//! model name resolves against them. One list serves both, so the favorites
//! the agent is told about are exactly the ones a name prefers.

use caudra_config::ModelPolicy;
use caudra_providers::Message;
use caudra_providers::model::{Model, ModelPurpose};
use caudra_providers::model_registry;
use caudra_storage::thinking::effort_rank;

use super::{last_announced, standing_notice};
use crate::prompt::{
    FAVORITES_SLOT, MODEL_FAVORITES_MARKER, MODEL_FAVORITES_PROMPT,
    MODEL_FAVORITES_WITHDRAWN_PROMPT,
};

const LINE_SEPARATOR: &str = "\n";
const PART_SEPARATOR: &str = "; ";
const LIST_SEPARATOR: &str = ", ";
const LEVEL_RANGE: &str = "–";
const LANE_LABEL: &str = "lane ";
const JOBS_LABEL: &str = "jobs ";
const THINKING_LABEL: &str = "thinking ";
const NO_THINKING: &str = "no thinking";
const OFF_LEVEL: &str = "off";
const NEVER_OFF: &str = "never off";

/// The favorites `policy` allows, newest first. The same rule the model
/// picker applies, so the agent never hears of a favorite the user cannot see.
pub(crate) fn allowed(policy: &ModelPolicy) -> Vec<String> {
    model_registry::favorites()
        .into_iter()
        .filter(|spec| policy.allows(spec))
        .collect()
}

/// The reminder listing every allowed favorite Caudra can build, or `None`
/// when none remains. Blocking: building a model can consult the catalog.
pub(super) fn list(policy: &ModelPolicy) -> Option<String> {
    let lines: Vec<String> = allowed(policy)
        .iter()
        .filter_map(|spec| {
            let model = Model::from_spec(spec).ok()?;
            Some(line(spec, &model, &model_registry::purposes_bound_to(spec)))
        })
        .collect();
    (!lines.is_empty())
        .then(|| MODEL_FAVORITES_PROMPT.replace(FAVORITES_SLOT, &lines.join(LINE_SEPARATOR)))
}

/// `listed` when the transcript does not already state it, else nothing. With
/// no list, withdraws one the transcript announced, so a favorite the user
/// dropped stops steering names the resolver no longer reads against it.
pub(super) fn notice(history: &[Message], listed: Option<String>) -> Option<Message> {
    let withdrawn = || {
        last_announced(history, &[MODEL_FAVORITES_MARKER]).map(|_| MODEL_FAVORITES_WITHDRAWN_PROMPT)
    };
    standing_notice(
        history,
        MODEL_FAVORITES_MARKER,
        listed.as_deref().or_else(withdrawn),
    )
}

fn line(spec: &str, model: &Model, jobs: &[ModelPurpose]) -> String {
    let mut parts = Vec::new();
    if let Some(marker) = Model::marker_of(&model.provider, &model.id) {
        parts.push(format!("{LANE_LABEL}{}", marker.label()));
    }
    if !jobs.is_empty() {
        let jobs: Vec<&str> = jobs.iter().map(|job| job.label()).collect();
        parts.push(format!("{JOBS_LABEL}{}", jobs.join(LIST_SEPARATOR)));
    }
    parts.push(thinking(model));
    format!("- `{spec}`: {}", parts.join(PART_SEPARATOR))
}

/// The levels the `/thinking` picker offers for `model`, so the agent and the
/// user see one ladder.
fn thinking(model: &Model) -> String {
    if !model.supports_thinking() {
        return NO_THINKING.to_owned();
    }
    let options = model.reasoning_options();
    let ladder = options.effort_ladder();
    let requires = model.requires_thinking();
    let parts = (!requires)
        .then_some(OFF_LEVEL.to_owned())
        .into_iter()
        .chain((!ladder.is_empty()).then(|| levels(&ladder)))
        .chain(requires.then_some(NEVER_OFF.to_owned()))
        .collect::<Vec<_>>();
    format!("{THINKING_LABEL}{}", parts.join(LIST_SEPARATOR))
}

/// A ladder that walks the effort levels without a gap reads as its two ends.
fn levels(ladder: &[&str]) -> String {
    let ranks: Option<Vec<usize>> = ladder.iter().map(|level| effort_rank(level)).collect();
    let unbroken = ranks.is_some_and(|ranks| ranks.windows(2).all(|pair| pair[1] == pair[0] + 1));
    match ladder {
        [first, .., last] if unbroken => format!("{first}{LEVEL_RANGE}{last}"),
        _ => ladder.join(LIST_SEPARATOR),
    }
}

#[cfg(test)]
mod tests {
    use caudra_storage::StateDir;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const LISTED: &str = "a list the transcript lacks must be announced";
    const REPEATED: &str = "a list the transcript already states must not repeat";
    const WITHDRAWN: &str = "an announced list must be withdrawn once none applies";
    const OPUS: &str = "anthropic/claude-opus-5-5";
    const HAIKU: &str = "anthropic/claude-haiku-5-5";
    const OLD_OPUS: &str = "anthropic/claude-opus-4-5";

    fn listing(spec: &str) -> String {
        MODEL_FAVORITES_PROMPT.replace(FAVORITES_SLOT, spec)
    }

    #[test_case(&["low", "medium", "high", "xhigh", "max"], "low–max" ; "unbroken_run_reads_as_its_ends")]
    #[test_case(&["low", "high"], "low, high" ; "a_gap_lists_every_level")]
    #[test_case(&["high", "max"], "high, max" ; "budget_ladder_skips_xhigh")]
    #[test_case(&["high"], "high" ; "a_single_level")]
    #[test_case(&["low", "turbo"], "low, turbo" ; "an_unknown_level_lists_every_level")]
    fn levels_read_as_the_picker_offers_them(ladder: &[&str], expected: &str) {
        assert_eq!(levels(ladder), expected);
    }

    #[test_case(OPUS, &[ModelPurpose::Chat, ModelPurpose::Subagent], "- `anthropic/claude-opus-5-5`: lane Best; jobs Chat, Subagent; thinking low–max, never off" ; "lane_jobs_and_a_ladder_that_never_turns_off")]
    #[test_case(HAIKU, &[], "- `anthropic/claude-haiku-5-5`: lane Fast; thinking off, low–max" ; "no_jobs_and_a_ladder_that_turns_off")]
    #[test_case(OLD_OPUS, &[], "- `anthropic/claude-opus-4-5`: thinking low–high, never off" ; "no_marker_leaves_the_lane_out")]
    fn a_favorite_line_names_lane_jobs_and_thinking(
        spec: &str,
        jobs: &[ModelPurpose],
        expected: &str,
    ) {
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(line(spec, &model, jobs), expected);
    }

    #[test]
    fn nothing_is_said_before_favorites_exist() {
        assert!(notice(&[], None).is_none());
    }

    #[test]
    fn a_list_is_stated_once_and_restated_when_it_changes() {
        let first = notice(&[], Some(listing(OPUS))).expect(LISTED);
        let history = [first];
        assert!(
            notice(&history, Some(listing(OPUS))).is_none(),
            "{REPEATED}"
        );

        let changed = notice(&history, Some(listing(HAIKU))).expect(LISTED);
        assert_eq!(changed.user_text(), Some(listing(HAIKU).as_str()));
    }

    #[test]
    fn an_announced_list_is_withdrawn_once() {
        let history = [notice(&[], Some(listing(OPUS))).expect(LISTED)];
        let withdrawal = notice(&history, None).expect(WITHDRAWN);
        assert_eq!(
            withdrawal.user_text(),
            Some(MODEL_FAVORITES_WITHDRAWN_PROMPT)
        );

        let history = [history[0].clone(), withdrawal];
        assert!(notice(&history, None).is_none(), "{REPEATED}");
    }

    #[test]
    fn the_policy_drops_an_excluded_favorite() {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        for spec in [OPUS, HAIKU] {
            model_registry::toggle_favorite_and_persist(spec, &dir).unwrap();
        }
        let policy = ModelPolicy::new(&[], &[OPUS.to_owned()]).unwrap();

        assert_eq!(allowed(&policy), [HAIKU]);
    }
}
