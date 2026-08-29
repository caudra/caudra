#![cfg(feature = "mcp")]

use rmcp::model::ContentBlock;
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use workcell_mcp_files::{FileToolGroup, catalog};

#[test]
fn exposes_exact_order_titles_schemas_annotations_and_presentations() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../fixtures/mcp-conformance/catalog/v1/filesystem-tools.json"
    ))
    .expect("filesystem catalog fixture");
    let actual = serde_json::to_value(catalog()).expect("serialize filesystem catalog");
    assert_eq!(actual, fixture["expected"]["tools"]);
}

#[tokio::test]
async fn dispatch_routes_known_names_and_returns_pretty_and_structured_json() {
    let root = tempdir().expect("root");
    std::fs::write(root.path().join("visible.txt"), "visible\n").expect("file");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");
    let result = group
        .dispatch(
            "file_read",
            json!({"filePath": "visible.txt", "limit": 1}),
            CancellationToken::new(),
        )
        .await
        .expect("known tool")
        .expect("protocol success");
    assert_eq!(result.is_error, None);
    let structured = result.structured_content.expect("structured content");
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("expected text content");
    };
    assert_eq!(
        content.text,
        serde_json::to_string_pretty(&structured).unwrap()
    );
    assert_eq!(structured["numberedText"], json!("1: visible"));

    let invalid = group
        .dispatch(
            "file_read",
            json!({"filePath": "visible.txt", "limit": -1}),
            CancellationToken::new(),
        )
        .await
        .expect("known tool")
        .expect("tool error, not protocol error");
    assert_eq!(invalid.is_error, Some(true));

    assert!(
        group
            .dispatch("not_a_file_tool", json!({}), CancellationToken::new())
            .await
            .is_none()
    );
}

#[tokio::test]
async fn dispatch_surfaces_cancellation_as_a_tool_error() {
    let root = tempdir().expect("root");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");
    let token = CancellationToken::new();
    token.cancel();
    let result = group
        .dispatch("file_glob", json!({"pattern": "*"}), token)
        .await
        .expect("known")
        .expect("tool result");
    assert_eq!(result.is_error, Some(true));
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("expected text");
    };
    assert_eq!(content.text, "Operation aborted");
}
