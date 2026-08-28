//! Public catalog contract.
//!
//! The committed fixture is the compatibility contract for the tool name, title, description,
//! schema, annotations, and presentation metadata. Regenerate it deliberately when the contract
//! changes; do not relax this assertion to make a diff pass.

use serde_json::Value;
use workcell_mcp_code::catalog;

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
        "No variables, definitions, or imports persist between calls",
    ] {
        assert!(
            description.contains(expected),
            "description must mention {expected:?}"
        );
    }
}
