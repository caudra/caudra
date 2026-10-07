//! The authoring guide against the engine it describes: every complete script in it validates and
//! smoke-runs, every complete example is the tested copy in `tests/examples/`, and the frontmatter
//! that lists the skill stays out of the guide's body.

use std::fs;
use std::path::Path;

use caudra_automation::AUTOMATION_SKILL;
use caudra_automation::meta::parse_meta;
use caudra_automation::validate::validate;

const RHAI_FENCE: &str = "```rhai\n";
const FENCE: &str = "```";
const HEADER_START: &str = "let meta = #{";
const FRONTMATTER_OPEN: &str = "---\n";
const FRONTMATTER_CLOSE: &str = "\n---\n";
const NAME_LINE: &str = "name: caudra-automation-dev";
const DESCRIPTION_KEY: &str = "description:";
const HEADING: &str = "# Writing Caudra automations";
const EXAMPLES_DIR: &str = "tests/examples";
const EXAMPLE_EXTENSION: &str = "rhai";
/// The tested examples the guide embeds, in the order it shows them.
const EMBEDDED_EXAMPLES: [&str; 13] = [
    "keep-going",
    "goal-chain",
    "timebox",
    "spend-guard",
    "retry-overload",
    "goal-webhook",
    "nightly-review",
    "ci-watch",
    "status-desk",
    "status-beacon",
    "task-tracker",
    "work-nudge",
    "research-desk",
];
/// 2026-10-05 08:30 UTC, a Monday.
const NOW_MS: i64 = 1_791_189_000_000;
const TOO_FEW_SCRIPTS: &str = "the guide carries its complete examples";
const UNNAMED: &str = "a complete script has a valid header";
const NO_FRONTMATTER: &str = "the guide opens with frontmatter between --- lines";
const UNNAMED_SKILL: &str = "the frontmatter names the skill";
const UNDESCRIBED: &str = "the frontmatter describes the skill for the model to pick it by";
const FRONTMATTER_LEAKED: &str = "the body starts at the heading, after the frontmatter";

/// The fenced `rhai` blocks that hold a whole script, exactly as written. Fragments without a
/// header illustrate a point and are not run.
fn complete_scripts() -> Vec<&'static str> {
    AUTOMATION_SKILL
        .split(RHAI_FENCE)
        .skip(1)
        .filter_map(|rest| rest.split_once(FENCE).map(|(block, _)| block))
        .filter(|block| block.starts_with(HEADER_START))
        .collect()
}

fn tested_copy(name: &str) -> Option<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(EXAMPLES_DIR)
        .join(name)
        .with_extension(EXAMPLE_EXTENSION);
    fs::read_to_string(path).ok()
}

#[test]
fn every_complete_script_validates_and_smoke_runs() {
    let scripts = complete_scripts();
    assert!(
        scripts.len() >= EMBEDDED_EXAMPLES.len(),
        "{TOO_FEW_SCRIPTS}"
    );
    for script in scripts {
        if let Err(error) = validate(script, NOW_MS) {
            let name = parse_meta(script).map(|meta| meta.name);
            panic!("{name:?}: {error}");
        }
    }
}

#[test]
fn complete_examples_are_their_tested_copies() {
    let mut embedded = Vec::new();
    for script in complete_scripts() {
        let name = parse_meta(script).expect(UNNAMED).name;
        if let Some(tested) = tested_copy(&name) {
            assert_eq!(script, tested, "{name}");
            embedded.push(name);
        }
    }
    assert_eq!(embedded, EMBEDDED_EXAMPLES);
}

#[test]
fn the_frontmatter_names_and_describes_the_skill_outside_the_body() {
    let (frontmatter, body) = AUTOMATION_SKILL
        .strip_prefix(FRONTMATTER_OPEN)
        .and_then(|rest| rest.split_once(FRONTMATTER_CLOSE))
        .expect(NO_FRONTMATTER);
    let fields: Vec<&str> = frontmatter.lines().collect();
    assert!(fields.contains(&NAME_LINE), "{UNNAMED_SKILL}");
    assert!(
        fields.iter().any(|field| field
            .strip_prefix(DESCRIPTION_KEY)
            .is_some_and(|description| !description.trim().is_empty())),
        "{UNDESCRIBED}"
    );
    assert!(
        body.trim_start().starts_with(HEADING),
        "{FRONTMATTER_LEAKED}"
    );
    assert!(!body.contains(NAME_LINE), "{FRONTMATTER_LEAKED}");
}
