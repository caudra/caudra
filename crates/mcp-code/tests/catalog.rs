//! Public catalog contract.
//!
//! The committed fixture is the compatibility contract for the tool name, title, description,
//! schema, annotations, and presentation metadata. Regenerate it deliberately when the contract
//! changes; do not relax this assertion to make a diff pass.

use serde_json::Value;
use workcell_mcp_code::{SUBSET_MODULES, UNTYPED_BUILTINS, WITHHELD_BUILTINS, catalog};

#[test]
fn matches_the_committed_catalog_fixture() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../fixtures/mcp-conformance/catalog/v1/code-tools.json"
    ))
    .expect("code catalog fixture");
    let actual = serde_json::to_value(catalog()).expect("serialize code catalog");
    assert_eq!(actual, fixture["expected"]["tools"]);
}

#[test]
fn description_enumerates_the_subset_an_agent_will_trip_over() {
    let tools = catalog();
    let description = tools[0].description.as_deref().expect("tool description");
    // Each of these is a divergence that costs a wasted turn when the caller does not know it.
    for expected in [
        "str.format()",
        "match statements",
        "yield",
        "Class inheritance",
        "unicodedata",
        "third-party packages",
        "eager",
        "fancy-regex",
        "type stubs",
        "do not dispatch to user-defined dunders",
        "no os.path",
        "No variables, definitions, or imports persist between calls",
    ] {
        assert!(
            description.contains(expected),
            "description must mention {expected:?}"
        );
    }
}

/// The description is spliced from `subset`, so this is really asserting that the splice happened
/// rather than that someone retyped the lists — which is what produced a description advertising
/// `base64`, `binascii`, and `functools`.
#[test]
fn description_states_exactly_the_subset_the_crate_defines() {
    let tools = catalog();
    let description = tools[0].description.as_deref().expect("tool description");
    for name in SUBSET_MODULES
        .iter()
        .chain(WITHHELD_BUILTINS.iter())
        .chain(UNTYPED_BUILTINS.iter())
    {
        assert!(description.contains(name), "description omits {name}");
    }
}

/// The absent-module sentence has to keep naming the three that were wrongly advertised, because a
/// caller that saw the old description will otherwise assume they were merely omitted by accident.
#[test]
fn description_names_the_modules_that_were_wrongly_advertised() {
    let tools = catalog();
    let description = tools[0].description.as_deref().expect("tool description");
    let advertised = description
        .split_once("CPython surface: ")
        .expect("the description enumerates the modules")
        .1
        .split_once('.')
        .expect("the enumeration is a sentence")
        .0;
    for absent in ["base64", "binascii", "functools"] {
        assert!(
            !advertised.contains(absent),
            "{absent} is advertised as available: {advertised}"
        );
        assert!(
            description.contains(&format!("no {absent}"))
                || description.contains(&format!(" {absent},")),
            "{absent} should be listed among the absent modules"
        );
    }
}
