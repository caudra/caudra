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
    #[cfg(feature = "index")]
    let expected = fixture["expected"]["tools"].clone();
    #[cfg(not(feature = "index"))]
    let expected = {
        let mut expected = fixture["expected"]["tools"].clone();
        expected.as_array_mut().expect("fixture tools").pop();
        expected
    };
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn dispatch_routes_known_names_and_separates_rendering_from_record() {
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
    // The content block is the model-facing rendering and the structured
    // content is the canonical record, so neither restates the other.
    assert_eq!(content.text, "1: visible");
    assert_eq!(structured["text"], json!("visible"));
    assert_eq!(structured["lineStart"], json!(1));
    assert_eq!(structured.get("numberedText"), None);

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

#[cfg(feature = "index")]
#[tokio::test]
async fn index_dispatch_returns_bare_model_text_and_complete_structured_content() {
    let root = tempdir().expect("root");
    std::fs::write(root.path().join("source.rs"), "pub fn run() {}\n").expect("file");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");
    let result = group
        .dispatch(
            "index",
            json!({"path": "source.rs"}),
            CancellationToken::new(),
        )
        .await
        .expect("known")
        .expect("result");
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("expected text")
    };
    assert_eq!(content.text, "fns:\n  pub run() [1]");
    // The outline is the content block. The record carries `lines`, which the
    // outline is joined from, so it is not repeated in structured output.
    let structured = result.structured_content.as_ref().unwrap();
    assert_eq!(structured.get("skeleton"), None);
    assert_eq!(
        structured["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line["text"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n"),
        content.text
    );

    let directory = group
        .dispatch("index", json!({"path": "."}), CancellationToken::new())
        .await
        .expect("known")
        .expect("result");
    let ContentBlock::Text(directory_text) = &directory.content[0] else {
        panic!("expected text")
    };
    assert_eq!(directory_text.text, "source.rs");
    assert_eq!(
        directory.structured_content.as_ref().unwrap()["kind"],
        "directory"
    );

    for arguments in [
        json!({"path": ""}),
        json!({"path": "source.rs", "extra": true}),
    ] {
        let invalid = group
            .dispatch("index", arguments, CancellationToken::new())
            .await
            .expect("known")
            .expect("tool error");
        assert_eq!(invalid.is_error, Some(true));
    }
}

#[cfg(feature = "index")]
#[tokio::test]
async fn index_dispatch_fits_escape_heavy_source_into_a_successful_result() {
    let root = tempdir().expect("root");
    let escaped = "\\".repeat(900);
    let source = (0..60)
        .map(|index| format!("<div id=\"item-{index}-{escaped}\"></div>\n"))
        .collect::<String>();
    std::fs::write(root.path().join("escaped.html"), source).expect("file");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");

    let result = group
        .dispatch(
            "index",
            json!({"path": "escaped.html"}),
            CancellationToken::new(),
        )
        .await
        .expect("known")
        .expect("successful bounded result");
    assert_ne!(result.is_error, Some(true));
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("expected text")
    };
    let structured = result.structured_content.as_ref().unwrap();
    assert_eq!(structured["truncated"], true);
    assert_eq!(structured.get("skeleton"), None);
    assert_eq!(
        structured["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line["text"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n"),
        content.text
    );
    assert_eq!(
        structured["lines"].as_array().unwrap().last().unwrap()["text"],
        "[truncated]"
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
