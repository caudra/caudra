#![cfg(feature = "mcp")]

//! Public catalog contract.
//!
//! The committed fixture is the compatibility contract for the tool name, title, description,
//! schema, annotations, and presentation metadata. Regenerate it deliberately when the contract
//! changes; do not relax this assertion to make a diff pass.

use serde_json::Value;
use workcell_mcp_shell::catalog;

#[test]
fn matches_the_committed_catalog_fixture() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../fixtures/mcp-conformance/catalog/v1/shell-tools.json"
    ))
    .expect("shell catalog fixture");
    let actual = serde_json::to_value(catalog()).expect("serialize shell catalog");
    assert_eq!(actual, fixture["expected"]["tools"]);
}
