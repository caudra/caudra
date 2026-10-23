//! The second model of upstream's `tlc-to-chart.mjs`: the open fix (`--fixed-dot`) or the spec
//! the code must follow (`--against-dot`). Each bug replays on it by the screens and step words
//! the reader sees, and stops at the first step that it shows the reader differently.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde_json::{Value as Json, json};

use super::chart::{ChartError, Charter, Keys, SecondSide, View};
use super::model::{Model, Trace, Vars};
use super::ordered::OrderedMap;
use super::text::join_and;

const FIX: &str = "fix";
const AGAINST: &str = "against";
const DEFAULT_FIX_LABEL: &str = "the fix";
/// What upstream calls the second model when no `--label` names it, also with no second model.
pub(super) const DEFAULT_AGAINST_LABEL: &str = "the spec";
const WITH: &str = "with";
const IN: &str = "in";
const SPEC_EXTENSION: &str = ".tla";
const CFG_EXTENSION: &str = ".cfg";
const TRACE_FILE: &str = "_TTrace_";
/// Why a bug cannot replay on the second model.
pub(super) const STARTS_ELSEWHERE: &str = "starts on another screen";

static MODULE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\\\* module:[ \t]*([0-9A-Za-z_]+)[ \t]*\r?$").expect("valid regex")
});
static EXPECT_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\\\* expect:[ \t]*([0-9A-Za-z_]+)[ \t]*\r?$").expect("valid regex")
});
/// An `expect:` outcome that is not a rule TLC breaks.
static NOT_A_RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(pass|deadlock)$|^error").expect("valid regex"));

/// What the second model is, and what the chart and stories call it.
#[derive(Debug, Clone)]
pub enum Role {
    /// The open fix. A state is fixed when the fix no longer breaks it.
    Fix { label: Option<String> },
    /// The spec the code must follow. Its bugs are drawn wrong too, and `listed` says what its
    /// configs expect TLC to break.
    Against {
        label: Option<String>,
        listed: Option<Listed>,
    },
}

impl Role {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Fix { .. } => FIX,
            Self::Against { .. } => AGAINST,
        }
    }

    pub(super) fn label(&self) -> &str {
        match self {
            Self::Fix { label } => label.as_deref().unwrap_or(DEFAULT_FIX_LABEL),
            Self::Against { label, .. } => label.as_deref().unwrap_or(DEFAULT_AGAINST_LABEL),
        }
    }

    /// "with the fix" or "in the spec".
    pub(super) fn within(&self) -> &'static str {
        match self {
            Self::Fix { .. } => WITH,
            Self::Against { .. } => IN,
        }
    }

    pub(super) fn is_fix(&self) -> bool {
        matches!(self, Self::Fix { .. })
    }

    pub(super) fn listed(&self) -> Option<&Listed> {
        match self {
            Self::Against { listed, .. } => listed.as_ref(),
            Self::Fix { .. } => None,
        }
    }
}

/// The model each bug replays on. Its `rules` are the rules its config checks: a bug of any other
/// rule reads "not checked".
#[derive(Debug, Clone)]
pub struct SecondModel<'a> {
    pub model: &'a Model,
    pub role: Role,
}

/// What the configs beside a spec expect: each config whose `\* expect:` line names a rule TLC
/// breaks, by its file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub spec: String,
    pub breaks: BTreeMap<String, String>,
}

impl Listed {
    /// The configs that expect TLC to break `rule`.
    pub(super) fn configs(&self, rule: &str) -> Vec<String> {
        self.breaks
            .iter()
            .filter(|(_, broken)| *broken == rule)
            .map(|(cfg, _)| cfg.clone())
            .collect()
    }

    pub(super) fn json(&self) -> Json {
        json!({"spec": self.spec, "breaks": self.breaks})
    }

    /// The story line that says whether a config already expects TLC to break `rule`.
    pub(super) fn line(&self, rule: &str) -> String {
        let configs = self.configs(rule);
        if !configs.is_empty() {
            let (configs_word, expect) = if configs.len() == 1 {
                ("a config", "expects")
            } else {
                ("configs", "expect")
            };
            return format!(
                "Listed: {}, {configs_word} of {}, {expect} TLC to break this rule.",
                join_and(&configs),
                self.spec
            );
        }
        let others: Vec<String> = self
            .breaks
            .values()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|rule| format!("`{rule}`"))
            .collect();
        let expected = if others.is_empty() {
            "None of its configs expects TLC to break a rule.".to_owned()
        } else {
            format!("Its configs expect TLC to break {}.", join_and(&others))
        };
        format!(
            "Not listed: no config of {} expects TLC to break this rule. {expected}",
            self.spec
        )
    }
}

/// Upstream's `listedBreaks`: what the configs beside `spec` expect of it, or `None` when none
/// has an `\* expect:` line for it. A config names its module with `\* module:`, or is for the
/// only spec in its folder.
pub fn listed_breaks(spec: &Path) -> io::Result<Option<Listed>> {
    let folder = spec
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut files: Vec<String> = fs::read_dir(folder)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    files.sort();
    let modules: Vec<&String> = files
        .iter()
        .filter(|file| file.ends_with(SPEC_EXTENSION) && !file.contains(TRACE_FILE))
        .collect();
    let only = match modules.as_slice() {
        [module] => module.strip_suffix(SPEC_EXTENSION),
        _ => None,
    };
    let name = spec
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    let mut breaks = BTreeMap::new();
    let mut headed = false;
    for file in files.iter().filter(|file| file.ends_with(CFG_EXTENSION)) {
        let cfg = fs::read_to_string(folder.join(file))?;
        let capture = |line: &Regex| {
            line.captures(&cfg)
                .and_then(|found| found.get(1))
                .map(|found| found.as_str().to_owned())
        };
        let module = capture(&MODULE_LINE).or_else(|| only.map(str::to_owned));
        let Some(expect) = capture(&EXPECT_LINE) else {
            continue;
        };
        if module.as_deref() != Some(name) {
            continue;
        }
        headed = true;
        if !NOT_A_RULE.is_match(&expect) {
            breaks.insert(file.clone(), expect);
        }
    }
    Ok(headed.then(|| Listed {
        spec: spec
            .file_name()
            .map(|file| file.to_string_lossy().into_owned())
            .unwrap_or_default(),
        breaks,
    }))
}

/// The paths of the variables and record fields that `a` has and `b` lacks.
pub(super) fn lacking(a: &Vars, b: &Vars) -> Vec<Vec<String>> {
    a.iter()
        .flat_map(|(name, value)| match (value, b.get(name)) {
            (_, None) => vec![vec![name.clone()]],
            (Json::Object(inner), Some(Json::Object(other))) => lacking(inner, other)
                .into_iter()
                .map(|path| [vec![name.clone()], path].concat())
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// `vars` without the `paths`, to compare the states of two models on what both have.
pub(super) fn omit(vars: &Vars, paths: &[Vec<String>]) -> Vars {
    let mut out = vars.clone();
    for path in paths {
        let Some((last, holders)) = path.split_last() else {
            continue;
        };
        let mut holder = Some(&mut out);
        for name in holders {
            holder = holder.and_then(|map| match map.get_mut(name) {
                Some(Json::Object(inner)) => Some(inner),
                _ => None,
            });
        }
        if let Some(map) = holder {
            map.remove(last);
        }
    }
    out
}

/// How a bug fares on the second model, whatever the view.
#[derive(Debug)]
pub(super) enum Replay {
    /// No start state of the second model shows the bug's first screen.
    StartsElsewhere,
    /// The second model shows the same screens and steps to the end.
    Same { checked: bool, rule_holds: bool },
    /// The first step the second model shows the reader differently.
    Differs(Turn),
}

#[derive(Debug)]
pub(super) struct Turn {
    /// The reader step before which, or at which, the models part.
    pub(super) step: usize,
    pub(super) said: String,
    /// The step is one the reader does not see on the first model.
    pub(super) hidden: bool,
    /// The bug's step on the first model, as its states and action.
    pub(super) before: usize,
    pub(super) after: usize,
    pub(super) action: Arc<str>,
    /// The step the second model takes instead, or `None` when it cannot take one.
    pub(super) taken: Option<usize>,
}

impl Charter<'_> {
    /// Upstream's `replaySecond`: the bug's steps on the second model, by the screens and step
    /// words the reader sees, with `keys` the customer view's keys of both models.
    pub(super) fn replay(
        &self,
        second: &SecondSide,
        trace: &Trace,
        keys: &Keys,
    ) -> Result<Replay, ChartError> {
        let side = &second.side;
        let screen = |state: usize| keys.second[state];
        let words = |index: usize| {
            let action = side.model.steps[index].action.clone().unwrap_or_default();
            let label = side.words[index].label(View::Customer, &action);
            if label.is_empty() {
                action.to_string()
            } else {
                label
            }
        };
        let settle = |states: Vec<usize>| -> Vec<usize> {
            let mut all = Vec::new();
            let mut seen = HashSet::new();
            for state in states {
                if seen.insert(state) {
                    all.push(state);
                }
            }
            let mut at = 0;
            while at < all.len() {
                let from = all[at];
                for &index in &side.out[from] {
                    let step = &side.model.steps[index];
                    let action = step.action.as_deref().unwrap_or_default();
                    if !self.mapping.reader_actions.contains(action)
                        && screen(step.to) == screen(from)
                        && seen.insert(step.to)
                    {
                        all.push(step.to);
                    }
                }
                at += 1;
            }
            all
        };
        let kept = self.reader_steps(trace, &keys.of);
        let seen = self.reader_path(trace, &keys.of, None, View::Customer)?;
        let shown_at: HashMap<usize, usize> = kept
            .iter()
            .enumerate()
            .map(|(index, &at)| (at, index))
            .collect();
        let mut here = settle(
            side.model
                .initial
                .iter()
                .copied()
                .filter(|&state| screen(state) == seen[0].key)
                .collect(),
        );
        if here.is_empty() {
            return Ok(Replay::StartsElsewhere);
        }
        let mut passed: HashSet<usize> = here.iter().copied().collect();
        let shared = |state: usize| omit(&self.model.states[state], &second.lacks);
        let mut reader_step = 0;
        for at in 1..trace.states.len() {
            let action = &trace.actions[at - 1];
            let hidden = !shown_at.contains_key(&at);
            if let Some(&index) = shown_at.get(&at) {
                reader_step = index;
            }
            let want = &seen[reader_step];
            let (before, after) = (trace.states[at - 1], trace.states[at]);
            let known = second.actions.contains(action);
            let after_shared = (!known).then(|| shared(after));
            if !known && (hidden || after_shared.as_ref() == Some(&shared(before))) {
                continue;
            }
            let tries: Vec<usize> = here
                .iter()
                .flat_map(|&state| side.out[state].iter().copied())
                .filter(|&index| {
                    let step = &side.model.steps[index];
                    match &after_shared {
                        None => step.action == *action,
                        Some(after) => second.shared[step.to] == *after,
                    }
                })
                .collect();
            if hidden && tries.is_empty() {
                continue;
            }
            let same: Vec<usize> = if known {
                tries
                    .iter()
                    .copied()
                    .filter(|&index| {
                        screen(side.model.steps[index].to) == want.key
                            && (hidden || words(index) == want.label)
                    })
                    .collect()
            } else {
                tries.clone()
            };
            if !same.is_empty() {
                here = settle(
                    same.iter()
                        .map(|&index| side.model.steps[index].to)
                        .collect(),
                );
                passed.extend(here.iter().copied());
                continue;
            }
            let mut outcomes: OrderedMap<(usize, String), Vec<usize>> = OrderedMap::default();
            for &index in &tries {
                outcomes
                    .entry((screen(side.model.steps[index].to), words(index)), Vec::new)
                    .push(index);
            }
            let taken = outcomes
                .values()
                .fold(None::<&Vec<usize>>, |best, group| match best {
                    Some(best) if best.len() >= group.len() => Some(best),
                    _ => Some(group),
                })
                .map(|group| group[0]);
            let action = action.clone().unwrap_or_default();
            let said = if hidden {
                let label = self
                    .main
                    .step_words(self.mapping, before, after, &action)?
                    .label(View::Customer, &action);
                if label.is_empty() {
                    action.to_string()
                } else {
                    label
                }
            } else {
                want.label.clone()
            };
            return Ok(Replay::Differs(Turn {
                step: reader_step,
                said,
                hidden,
                before,
                after,
                action,
                taken,
            }));
        }
        let bad: HashSet<usize> = side
            .model
            .traces
            .iter()
            .filter(|other| other.rule == trace.rule)
            .filter_map(|other| other.states.last().copied())
            .collect();
        let checked = side.model.rules.contains(&trace.rule);
        Ok(Replay::Same {
            checked,
            rule_holds: checked && !passed.iter().any(|state| bad.contains(state)),
        })
    }
}

#[cfg(test)]
mod tests {
    use regex::Regex;
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::scripts::statechart::chart::{ChartOptions, draw_with};
    use crate::scripts::statechart::model::explore;
    use crate::scripts::statechart::testing::{
        CHARGE_AGAIN, PAY_CFG, PAY_MAP, Spec, arrows, explore_text, fix, fixture, pay, rule_cfg,
        status_of,
    };

    const AGREED: &str = "SaveAgreed.tla";
    const AGREED_LABEL: &str = "the agreed chart";
    const BADGE_FIX: &str = "the badge fix";
    const WITH_THE_FIX: &str = "with the fix";
    const PAY_STATE: &str = r#"v.screen + ["", ", charged", ", charged twice"][v.charges]"#;
    const SCREEN_ONLY: &str = "v.screen";
    const CHARGE_ONCE_PAY: &str = r#"screen = "Paying" /\ charges < 2 /\ IF charges = 0 THEN charges' = 1 /\ UNCHANGED screen ELSE screen' = "Paid" /\ UNCHANGED charges"#;
    const ORDER_CHARGE_AGAIN: &str = r"charges' = charges + 1 /\ UNCHANGED <<screen, note>>";
    const ORDER_CHARGE_ONCE: &str = r#"IF charges = 0 THEN charges' = 1 /\ UNCHANGED <<screen, note>> ELSE screen' = "Paid" /\ UNCHANGED <<charges, note>>"#;
    const ORDER_CHARGE_FIRST_ONLY: &str =
        r"charges = 0 /\ charges' = 1 /\ UNCHANGED <<screen, note>>";
    const NO_DOUBLE_CHARGE: &str = "NoDoubleCharge";
    const NO_LOST_EDIT: &str = "NoLostEdit";
    const EDIT_MAP: &str = r#"
        fn reader_actions() { ["Tap"] }
        fn state(v) { v.s }
        fn step(a) { #{ Tap: "you tap Save", Lose: "the save fails", Flash: "the screen flashes", Back: "you go back" }[a] ?? "" }
    "#;
    const NOTIFY_CFG: &str =
        "SPECIFICATION Spec\nINVARIANT NeverMissed\nINVARIANT NoBannerOnThread\n";
    const NOTIFY_MAP: &str = r#"
        fn reader_actions() { ["OpenThread", "OpenSettings", "TapBack", "Mute", "Refresh"] }
        fn state(v) {
            let shown = v.screen;
            if v.muted { shown += ", muted"; }
            if v.banner { shown += ", banner"; }
            if v.badge { shown += ", badge"; }
            shown
        }
        fn step(a) { #{ OpenThread: "you open the thread", OpenSettings: "you open settings", TapBack: "you tap Back", Mute: "you mute alerts", Refresh: "you pull to refresh", Arrive: "a message arrives" }[a] }
        fn cause() { () }
    "#;
    const NOTIFY_MISSED: &str = r"missed' = TRUE /\ UNCHANGED <<banner, badge>>";
    const NOTIFY_BADGE: &str = r"badge' = TRUE /\ UNCHANGED <<banner, missed>>";
    const SAVE_STATE: &str = "${v.note.text}`";
    const SAVE_STATE_WITH_REV: &str = "${v.note.text} r${v.note.rev}`";
    const SAVE_STATE_TESTING_REV: &str =
        r#"${v.note.text}${if "rev" in v.note { ` r${v.note.rev}` } else { "" }}`"#;
    const READS_THE_LACKED_REV: &str = "Test.map.rhai: the agreed chart has no note.rev, and the mapping reads v.note.rev on its states. Label states and steps only from what both models have, or test \"rev\" in v.note first.";
    const GROUP_YOUR_NOTES: &str = "\nfn group(v) { \"Your notes\" }\n";

    fn vars(value: Json) -> Vars {
        value.as_object().expect("an object").clone()
    }

    #[test]
    fn a_path_lacks_a_variable_or_a_record_field() {
        let code =
            vars(json!({"screen": "Editing", "note": {"text": "draft", "rev": 0}, "retries": 0}));
        let agreed = vars(json!({"screen": "Editing", "note": {"text": "draft"}}));

        let mut lacks: Vec<String> = lacking(&code, &agreed)
            .into_iter()
            .map(|path| path.join("."))
            .collect();
        lacks.sort();

        assert_eq!(lacks, ["note.rev", "retries"]);
        assert!(lacking(&agreed, &code).is_empty());
        assert_eq!(
            omit(&code, &lacking(&code, &agreed)),
            vars(json!({"note": {"text": "draft"}, "screen": "Editing"}))
        );
    }

    #[test_case(&[("A.cfg", "NoFalseSaved")], "NoFalseSaved" => "Listed: A.cfg, a config of Spec.tla, expects TLC to break this rule."; "one config")]
    #[test_case(&[("A.cfg", "Rule"), ("B.cfg", "Rule")], "Rule" => "Listed: A.cfg and B.cfg, configs of Spec.tla, expect TLC to break this rule."; "two configs")]
    #[test_case(&[("A.cfg", "TypeOK")], "NoFalseSaved" => "Not listed: no config of Spec.tla expects TLC to break this rule. Its configs expect TLC to break `TypeOK`."; "another rule")]
    #[test_case(&[], "NoFalseSaved" => "Not listed: no config of Spec.tla expects TLC to break this rule. None of its configs expects TLC to break a rule."; "no rule")]
    fn a_story_says_whether_a_config_expects_the_bug(
        breaks: &[(&str, &str)],
        rule: &str,
    ) -> String {
        Listed {
            spec: "Spec.tla".to_owned(),
            breaks: breaks
                .iter()
                .map(|(cfg, rule)| ((*cfg).to_owned(), (*rule).to_owned()))
                .collect(),
        }
        .line(rule)
    }

    #[test_case(&[("SaveAgreedBug.cfg", "\\* module: SaveAgreed\n\\* expect: NoFalseSaved\nSPECIFICATION Spec\n")] => Some("SaveAgreedBug.cfg: NoFalseSaved".to_owned()); "a config names the module")]
    #[test_case(&[("A.cfg", "\\* expect: pass\n"), ("B.cfg", "\\* expect: Rule\r\n")] => Some("B.cfg: Rule".to_owned()); "the only module passes one config")]
    #[test_case(&[("A.cfg", "\\* expect: Rule\n"), ("Other.tla", "")] => None; "a config of no named module")]
    #[test_case(&[("A.cfg", "SPECIFICATION Spec\n")] => None; "no expect line")]
    fn the_configs_beside_a_spec_say_what_it_breaks(files: &[(&str, &str)]) -> Option<String> {
        let folder = tempfile::tempdir().expect("a temporary folder");
        fs::write(folder.path().join(AGREED), "").expect("the spec writes");
        for (name, text) in files {
            fs::write(folder.path().join(name), text).expect("the file writes");
        }

        let listed = listed_breaks(&folder.path().join(AGREED)).expect("the folder reads")?;

        assert_eq!(listed.spec, AGREED);
        Some(
            listed
                .breaks
                .iter()
                .map(|(cfg, rule)| format!("{cfg}: {rule}"))
                .collect::<Vec<_>>()
                .join(", "),
        )
    }

    fn order(module: &str, note: u8, charge: &str) -> String {
        format!(
            r#"---- MODULE {module} ----
EXTENDS Naturals
VARIABLES screen, charges, note
vars == <<screen, charges, note>>
Init == screen = "Cart" /\ charges = 0 /\ note = 0
Pay == screen = "Cart" /\ screen' = "Paying" /\ note' = {note} /\ UNCHANGED charges
Charge == screen = "Paying" /\ charges < 2 /\ {charge}
Next == Pay \/ Charge
Spec == Init /\ [][Next]_vars
NoDoubleCharge == charges <= 1
===="#
        )
    }

    fn edit(module: &str, fixed: bool) -> String {
        let (flashed, lose) = if fixed {
            ("TRUE", "")
        } else {
            ("FALSE", r" \/ Lose")
        };
        format!(
            r#"---- MODULE {module} ----
VARIABLES s, flashed
vars == <<s, flashed>>
Init == s = "Idle" /\ flashed = FALSE
Tap == s = "Idle" /\ s' = "Saving" /\ UNCHANGED flashed
Lose == s = "Saving" /\ s' = "Lost edit" /\ UNCHANGED flashed
Flash == s = "Saving" /\ s' = "Flash" /\ flashed' = {flashed}
Back == s \in {{"Lost edit", "Flash"}} /\ s' = "Idle" /\ flashed' = FALSE
Next == Tap \/ Flash \/ Back{lose}
Spec == Init /\ [][Next]_vars
NoLostEdit == s # "Lost edit" /\ ~(s = "Flash" /\ flashed)
===="#
        )
    }

    fn notify(module: &str, muted_arrive: &str) -> String {
        format!(
            r#"---- MODULE {module} ----
EXTENDS Naturals
VARIABLES screen, muted, banner, badge, missed, msgs, syncs
vars == <<screen, muted, banner, badge, missed, msgs, syncs>>
Init == screen = "Inbox" /\ muted = FALSE /\ banner = FALSE /\ badge = FALSE
        /\ missed = FALSE /\ msgs = 0 /\ syncs = 0
OpenThread == screen = "Inbox" /\ screen' = "Thread" /\ UNCHANGED <<muted, banner, badge, missed, msgs, syncs>>
OpenSettings == screen \in {{"Inbox", "Thread"}} /\ screen' = "Settings" /\ UNCHANGED <<muted, banner, badge, missed, msgs, syncs>>
TapBack == screen \in {{"Thread", "Settings"}} /\ screen' = "Inbox" /\ UNCHANGED <<muted, banner, badge, missed, msgs, syncs>>
Mute == screen = "Settings" /\ ~muted /\ muted' = TRUE /\ UNCHANGED <<screen, banner, badge, missed, msgs, syncs>>
Refresh == syncs < 2 /\ syncs' = syncs + 1 /\ UNCHANGED <<screen, muted, banner, badge, missed, msgs>>
Arrive == msgs < syncs /\ msgs' = msgs + 1
          /\ (IF muted THEN {muted_arrive} ELSE banner' = TRUE /\ UNCHANGED <<badge, missed>>)
          /\ UNCHANGED <<screen, muted, syncs>>
Next == OpenThread \/ OpenSettings \/ TapBack \/ Mute \/ Refresh \/ Arrive
Spec == Init /\ [][Next]_vars
NeverMissed == ~missed
NoBannerOnThread == ~(banner /\ screen = "Thread")
===="#
        )
    }

    /// The chart's arrows whose label says what `within` does instead.
    fn second_arrows(mermaid: &str, within: &str) -> Vec<String> {
        arrows(mermaid)
            .into_iter()
            .filter(|arrow| arrow.contains(within))
            .collect()
    }

    fn legend(mermaid: &str) -> Vec<String> {
        Regex::new(r#"(?m)^ {4}state "([^"]+)" as legend\w+$"#)
            .expect("a pattern")
            .captures_iter(mermaid)
            .map(|found| found[1].to_owned())
            .collect()
    }

    fn classes(summary: &Json, fields: &[&str]) -> Json {
        summary["classes"]
            .as_array()
            .expect("classes")
            .iter()
            .map(|class| {
                fields
                    .iter()
                    .map(|field| class[*field].clone())
                    .collect::<Json>()
            })
            .collect()
    }

    fn save_spec(map: &str) -> Spec {
        let read = |name: &str| fs::read_to_string(fixture(name)).expect("the fixture reads");
        Spec::new(
            "SaveCode",
            &read("SaveCode.tla"),
            &read("SaveCode.cfg"),
            map,
        )
    }

    fn save_map() -> String {
        fs::read_to_string(fixture("SaveCode.map.rhai")).expect("the mapping reads")
    }

    fn agreed(listed: Option<Listed>) -> (Model, Role) {
        let model =
            explore(&fixture(AGREED), &fixture("SaveCode.cfg")).expect("the model explores");
        let role = Role::Against {
            label: Some(AGREED_LABEL.to_owned()),
            listed,
        };
        (model, role)
    }

    #[test]
    fn the_fix_edge_leaves_the_state_before_the_changed_step() {
        let spec = Spec::new("Pay", &pay("Pay", CHARGE_AGAIN, false), PAY_CFG, PAY_MAP);
        let fixed = explore_text(
            "PayFixed",
            &pay("PayFixed", CHARGE_ONCE_PAY, false),
            PAY_CFG,
        );

        let drawing = spec.chart_with(&fix(&fixed), &ChartOptions::default());

        let expected = [
            ("Cart", "works"),
            ("Paid, charged", "fixed"),
            ("Paying", "works"),
            ("Paying, charged", "works"),
            ("Paying, charged twice", "fixed"),
            ("Refunded, charged", "fixed"),
        ]
        .map(|(state, class)| (state.to_owned(), class.to_owned()));
        assert_eq!(status_of(&drawing.mermaid), BTreeMap::from(expected));
        assert_eq!(
            legend(&drawing.mermaid),
            ["Works today", "Fixed or new with the fix"]
        );
        assert!(
            drawing
                .mermaid
                .contains("stateDiagram-v2\n  %% second model (fix): the fix\n")
        );
        assert_eq!(
            second_arrows(&drawing.mermaid, "with the fix, "),
            ["Paying, charged -> Paid, charged : with the fix, the bank charges you"]
        );
    }

    #[test_case("SPECIFICATION Spec\n" => Some("In the spec you see the same screens and steps, but the spec does not check this rule.".to_owned()); "a rule it does not check")]
    #[test_case(PAY_CFG => Some("In the spec you see the same screens and steps, so the spec does not change this path.".to_owned()); "a rule it checks")]
    fn a_rule_the_second_model_does_not_check_reads_not_checked(cfg: &str) -> Option<String> {
        let spec = Spec::new("Pay", &pay("Pay", CHARGE_AGAIN, false), PAY_CFG, PAY_MAP);
        let against = explore_text("Pay", &pay("Pay", CHARGE_AGAIN, false), cfg);
        let second = SecondModel {
            model: &against,
            role: Role::Against {
                label: None,
                listed: None,
            },
        };

        let drawing = spec.chart_with(&second, &ChartOptions::default());

        drawing
            .stories
            .lines()
            .find(|line| line.starts_with("In the spec"))
            .map(str::to_owned)
    }

    #[test]
    fn a_bug_only_the_fix_has_is_numbered_after_the_current_bugs_and_drawn_wrong() {
        let cfg = rule_cfg(NO_LOST_EDIT);
        let spec = Spec::new("Edit", &edit("Edit", false), &cfg, EDIT_MAP);
        let fixed = explore_text("EditFixed", &edit("EditFixed", true), &cfg);
        let second = fix(&fixed);

        let drawing = spec.chart_with(
            &second,
            &ChartOptions {
                cover: true,
                ..ChartOptions::default()
            },
        );
        let two = spec.chart_with(
            &second,
            &ChartOptions {
                highlight: 2,
                path_only: true,
                ..ChartOptions::default()
            },
        );

        assert_eq!(
            classes(&drawing.summary, &["n", "end", "fix"]),
            json!([[1, "Lost edit", "Saving"], [2, "Flash", "new with the fix"]])
        );
        assert_eq!(
            drawing.summary["chartStatus"],
            json!({"works": 2, "wrong": 1, "fixed": 1})
        );
        assert!(
            drawing
                .stories
                .lines()
                .any(|line| line == "## 2. New with the fix: Flash")
        );
        assert!(
            two.mermaid.contains(": 2. the screen flashes\n"),
            "{}",
            two.mermaid
        );
    }

    #[test_case(SAVE_STATE_WITH_REV => Some(READS_THE_LACKED_REV.to_owned()); "a field the spec lacks")]
    #[test_case(SAVE_STATE_TESTING_REV => None; "a field it tests first")]
    fn a_mapping_reads_only_what_both_models_have(state: &str) -> Option<String> {
        let map = save_map().replace(SAVE_STATE, state);
        assert_ne!(map, save_map());
        let spec = save_spec(&map);
        let (model, role) = agreed(None);
        let second = SecondModel {
            model: &model,
            role,
        };

        draw_with(&spec.model, Some(&second), &spec.mapping, &spec.options)
            .err()
            .map(|error| error.to_string())
    }

    #[test]
    fn a_spec_with_no_expect_line_lists_nothing() {
        let spec = save_spec(&save_map());
        let (model, role) = agreed(None);

        let charts = spec.draw_with(&SecondModel {
            model: &model,
            role,
        });

        assert_eq!(charts.summary["second"].get("listed"), None);
        assert_eq!(charts.summary["classes"][0].get("listed"), None);
        assert!(!charts.stories.contains("listed"));
        assert!(!charts.stories.contains("Listed"));
    }

    #[test]
    fn a_story_says_no_config_expects_a_new_bug() {
        let spec = save_spec(&save_map());
        let (model, role) = agreed(Some(Listed {
            spec: AGREED.to_owned(),
            breaks: BTreeMap::from([("SaveAgreedBug.cfg".to_owned(), "TypeOK".to_owned())]),
        }));

        let charts = spec.draw_with(&SecondModel {
            model: &model,
            role,
        });

        assert!(charts.stories.lines().any(|line| line == "Not listed: no config of SaveAgreed.tla expects TLC to break this rule. Its configs expect TLC to break `TypeOK`."), "{}", charts.stories);
        assert_eq!(charts.summary["classes"][0]["listed"], json!([]));
    }

    #[test_case(ORDER_CHARGE_AGAIN, PAY_STATE, &[], "With the fix you see the same screens and steps, so the fix does not change this path."; "a step the reader does not see")]
    #[test_case(ORDER_CHARGE_ONCE, PAY_STATE, &["Paying, charged -> Paid, charged : with the fix, the bank charges you"], "With the fix, step 3 (the bank charges you) takes you to: Paid, charged."; "the step the reader sees")]
    #[test_case(ORDER_CHARGE_ONCE, SCREEN_ONLY, &["Paying -> Paid : with the fix, the bank charges you"], "With the fix, after step 1, \"the bank charges you\" happens and takes you to: Paid."; "a hidden step that changes the screen")]
    #[test_case(ORDER_CHARGE_FIRST_ONLY, SCREEN_ONLY, &[], "yet the rule holds, so the state labels do not show what the fix changes"; "a fix the labels do not show")]
    fn the_fix_changes_a_bug_only_at_the_first_step_the_reader_sees_differently(
        charge: &str,
        state: &str,
        fix_arrows: &[&str],
        story: &str,
    ) {
        let map = PAY_MAP.replace(PAY_STATE, state);
        let cfg = rule_cfg(NO_DOUBLE_CHARGE);
        let spec = Spec::new("Order", &order("Order", 0, ORDER_CHARGE_AGAIN), &cfg, &map);
        let fixed = explore_text("OrderFixed", &order("OrderFixed", 1, charge), &cfg);

        let charts = spec.draw_with(&fix(&fixed));

        assert_eq!(second_arrows(&charts.chart, WITH_THE_FIX), fix_arrows);
        assert!(charts.stories.contains(story), "{}", charts.stories);
    }

    #[test]
    fn a_step_the_reader_does_not_see_breaks_the_rule_and_the_fix_shows_it_on_another_screen() {
        let spec = Spec::new(
            "Notify",
            &notify("Notify", NOTIFY_MISSED),
            NOTIFY_CFG,
            NOTIFY_MAP,
        );
        let badge = explore_text(
            "NotifyBadge",
            &notify("NotifyBadge", NOTIFY_BADGE),
            NOTIFY_CFG,
        );
        let second = SecondModel {
            model: &badge,
            role: Role::Fix {
                label: Some(BADGE_FIX.to_owned()),
            },
        };
        let within = format!("{WITH} {BADGE_FIX}");

        let charts = spec.draw_with(&second);

        assert_eq!(
            classes(&charts.summary, &["invariant", "end", "fix"]),
            json!([
                ["NoBannerOnThread", "Thread, banner", "unchanged"],
                ["NeverMissed", "Settings, muted", "Settings, muted, badge"],
                ["NeverMissed", "Inbox, muted", "Inbox, muted, badge"],
                [
                    "NeverMissed",
                    "Settings, muted, banner",
                    "Settings, muted, banner, badge"
                ],
                ["NeverMissed", "Thread, muted", "Thread, muted, badge"],
                ["NoBannerOnThread", "Thread, muted, banner", "unchanged"],
                [
                    "NeverMissed",
                    "Inbox, muted, banner",
                    "Inbox, muted, banner, badge"
                ],
                [
                    "NeverMissed",
                    "Thread, muted, banner",
                    "Thread, muted, banner, badge"
                ],
                [
                    "NoBannerOnThread",
                    "Thread, muted, banner, badge",
                    "new with the badge fix"
                ],
            ])
        );
        assert!(!charts.stories.contains("do not show what"));
        assert!(charts.stories.lines().any(|line| line == "With the badge fix, after step 3, \"a message arrives\" happens and takes you to: Settings, muted, badge."), "{}", charts.stories);
        let classes = charts.summary["classes"].as_array().expect("classes");
        let settings = classes
            .iter()
            .find(|class| class["end"] == "Settings, muted")
            .expect("a bug ends on muted settings");
        let file = format!("bug-{}.mmd", settings["n"]);
        let bug = charts
            .bugs
            .iter()
            .find(|bug| bug.file == file)
            .expect("the bug is drawn");
        assert_eq!(
            second_arrows(&bug.mermaid, &within),
            ["Settings, muted -> Settings, muted, badge : with the badge fix, a message arrives"]
        );
        let drawn = second_arrows(&charts.chart, &within);
        assert_eq!(drawn.iter().collect::<HashSet<_>>().len(), drawn.len());
        for class in classes
            .iter()
            .filter(|class| class["invariant"] == "NeverMissed")
        {
            let end = class["end"].as_str().expect("an end");
            let arrow = format!("{end} -> {end}, badge : with the badge fix, a message arrives");
            assert!(drawn.contains(&arrow), "bug {}", class["n"]);
        }
    }

    #[test]
    fn a_grouped_chart_marks_fixed_states_in_their_group_color() {
        let read = |name: &str| fs::read_to_string(fixture(name)).expect("the fixture reads");
        let map = format!("{}{GROUP_YOUR_NOTES}", read("Sync.map.rhai"));
        let spec = Spec::new("Sync", &read("Sync.tla"), &read("Sync.cfg"), &map);
        let fixed =
            explore(&fixture("SyncFixed.tla"), &fixture("Sync.cfg")).expect("the model explores");

        let charts = spec.draw_with(&fix(&fixed));

        let styled = Regex::new(r"^ {2}(classDef|class) |legend").expect("a pattern");
        let lines: Vec<&str> = charts
            .chart
            .lines()
            .filter(|line| styled.is_match(line))
            .collect();
        assert_eq!(
            lines,
            [
                "  classDef plainFixed fill:#f4f6f7,stroke:#1e8449,stroke-width:4px,color:#145a32",
                "  classDef group0 fill:#d6eaf8,stroke:#6b757c,color:#1b2631",
                "  classDef group0Fixed fill:#d6eaf8,stroke:#1e8449,stroke-width:4px,color:#145a32",
                "  state \"Legend\" as legend {",
                "    state \"Your notes\" as legendGroup0",
                "    state \"Fixed or new with the fix\" as legendFixed",
                "  class legendGroup0 group0",
                "  class legendFixed plainFixed",
                "  class s0 group0Fixed",
                "  class s1 group0",
                "  class s2 group0",
                "  class s3 group0",
            ]
        );
    }
}
