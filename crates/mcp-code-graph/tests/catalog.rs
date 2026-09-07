#![cfg(feature = "mcp")]

//! Public catalog contract.
//!
//! The committed fixture is the compatibility contract for every tool name, title, description,
//! schema, annotation, and presentation profile in this group. Regenerate it deliberately when the
//! contract changes; do not relax this assertion to make a diff pass.

use serde_json::Value;
use workcell_mcp_code_graph::{catalog, specs};

#[test]
fn matches_the_committed_catalog_fixture() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../fixtures/mcp-conformance/catalog/v1/code-graph-tools.json"
    ))
    .expect("code graph catalog fixture");
    let actual = serde_json::to_value(catalog()).expect("serialize code graph catalog");
    assert_eq!(actual, fixture["expected"]["tools"]);
}

/// MCP is a projection of `ToolSpec`, so the two tables must not be able to drift into disagreeing
/// about which tools exist or what order they are offered in.
#[test]
fn the_mcp_projection_covers_every_neutral_spec_in_order() {
    let names = catalog()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<_>>();
    let neutral = specs()
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect::<Vec<_>>();
    assert_eq!(names, neutral);
    assert_eq!(
        names,
        [
            "code_map",
            "code_context",
            "code_refs",
            "code_impact",
            "code_expand"
        ]
    );
}

/// Every description has to state the floor property, because a caller that reads a count as exact
/// draws a conclusion the graph cannot support.
#[test]
fn every_description_discloses_that_counts_are_floors() {
    for spec in specs() {
        let description: &str = &spec.description;
        assert!(
            description.contains("FLOOR") || description.contains("floor"),
            "{} omits the floor disclosure",
            spec.name
        );
    }
}
