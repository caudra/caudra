use serde_json::{Value, json};
use tempfile::tempdir;

use super::*;

fn expected(fixture: &str) -> Value {
    let fixture: Value = serde_json::from_str(fixture).expect("transfer catalog fixture");
    fixture["expected"]["tools"].clone()
}

#[test]
fn catalog_matches_the_conformance_fixture() {
    let actual = serde_json::to_value(catalog::catalog(true)).expect("serialize transfer catalog");
    assert_eq!(
        actual,
        expected(include_str!(
            "../../fixtures/mcp-conformance/catalog/v1/transfer-tools.json"
        ))
    );
}

/// Enumeration must agree with authorization: a deployment that cannot write must not advertise an
/// upload the endpoint would refuse.
#[test]
fn read_only_catalog_matches_the_conformance_fixture() {
    let actual =
        serde_json::to_value(catalog::catalog(false)).expect("serialize read-only catalog");
    assert_eq!(
        actual,
        expected(include_str!(
            "../../fixtures/mcp-conformance/catalog/v1/transfer-tools-read-only.json"
        ))
    );
}

async fn group(allow_write: bool) -> (tempfile::TempDir, TransferToolGroup) {
    let root = tempdir().expect("root");
    let group = TransferToolGroup::new(root.path(), allow_write, 1024)
        .await
        .expect("group");
    (root, group)
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

async fn call(group: &TransferToolGroup, name: &str, path: &str) -> CallToolResult {
    group
        .dispatch(name, json!({"path": path}))
        .await
        .expect("owned tool name")
        .expect("tool result")
}

#[tokio::test]
async fn read_only_dispatch_does_not_own_the_upload_name() {
    let (_root, group) = group(false).await;
    assert!(
        group
            .dispatch("file_upload", json!({"path": "new.bin"}))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn download_mints_a_relative_url_and_moves_no_bytes() {
    let (root, group) = group(false).await;
    std::fs::write(root.path().join("report data.bin"), b"0123456789").expect("file");
    let result = call(&group, "file_download", "report data.bin").await;
    assert_ne!(result.is_error, Some(true));
    let structured = result.structured_content.clone().expect("structured");
    assert_eq!(structured["method"], "GET");
    assert_eq!(structured["bytes"], 10);
    assert_eq!(structured["name"], "report data.bin");
    // Relative on purpose: only the harness knows the externally reachable origin.
    assert_eq!(structured["url"], "/files?path=report+data.bin");
    assert!(text(&result).contains("No bytes were transferred by this call."));
}

#[tokio::test]
async fn download_rejects_a_directory_and_an_oversized_file() {
    let (root, group) = group(false).await;
    std::fs::create_dir(root.path().join("nested")).expect("dir");
    std::fs::write(root.path().join("big.bin"), vec![0_u8; 2048]).expect("file");
    for (path, fragment) in [("nested", "is a directory"), ("big.bin", "transfer limit")] {
        let result = call(&group, "file_download", path).await;
        assert_eq!(result.is_error, Some(true), "{path}");
        assert!(text(&result).contains(fragment), "{path}");
    }
}

#[tokio::test]
async fn transfer_tools_refuse_paths_outside_the_root() {
    let (_root, group) = group(true).await;
    for name in ["file_download", "file_upload"] {
        let result = call(&group, name, "../escaped.bin").await;
        assert_eq!(result.is_error, Some(true), "{name}");
    }
}

#[test]
fn minted_urls_percent_encode_the_path() {
    assert_eq!(
        transfer_url("dir/a b&c?d=e.bin"),
        "/files?path=dir%2Fa+b%26c%3Fd%3De.bin"
    );
}

mod endpoint {
    use axum::{
        body::{Body, to_bytes},
        extract::State,
        http::{Request, StatusCode, Uri, header::CONTENT_TYPE},
        response::Response,
    };

    use super::{group, json};
    use crate::transfer::endpoints;

    async fn body(response: Response) -> Vec<u8> {
        to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body")
            .to_vec()
    }

    fn uri(path: &str) -> Uri {
        crate::transfer::transfer_url(path).parse().expect("uri")
    }

    fn post(path: &str, content_type: &str, bytes: Vec<u8>) -> Request<Body> {
        Request::post(uri(path))
            .header(CONTENT_TYPE, content_type)
            .body(Body::from(bytes))
            .expect("request")
    }

    #[tokio::test]
    async fn download_streams_the_file_with_a_sanitized_disposition() {
        let (root, group) = group(false).await;
        std::fs::write(root.path().join("a\"b;c.bin"), b"payload").expect("file");
        let response = endpoints::download(State(group), uri("a\"b;c.bin")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let disposition = response
            .headers()
            .get(axum::http::header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .expect("disposition")
            .to_owned();
        assert_eq!(disposition, "attachment; filename=\"abc.bin\"");
        assert_eq!(body(response).await, b"payload");
    }

    #[tokio::test]
    async fn download_requires_a_path_and_refuses_traversal() {
        let (_root, group) = group(false).await;
        let missing =
            endpoints::download(State(group.clone()), "/files".parse().expect("uri")).await;
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
        let escaped = endpoints::download(State(group), uri("../escaped.bin")).await;
        assert_eq!(escaped.status(), StatusCode::BAD_REQUEST);
    }

    /// A published file must be whole or absent. The staging file is the only artifact a failed or
    /// partial transfer may leave behind, and it must not survive either.
    #[tokio::test]
    async fn upload_publishes_atomically_and_leaves_no_staging_file() {
        let (root, group) = group(true).await;
        let response = endpoints::upload(
            State(group),
            post(
                "nested/new.bin",
                "application/octet-stream",
                b"payload".to_vec(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let reported: serde_json::Value =
            serde_json::from_slice(&body(response).await).expect("json");
        assert_eq!(
            reported,
            json!({"path": "nested/new.bin", "name": "new.bin", "bytes": 7})
        );
        assert_eq!(
            std::fs::read(root.path().join("nested/new.bin")).expect("published"),
            b"payload"
        );
        assert!(staging_files(&root.path().join("nested")).is_empty());
    }

    fn staging_files(directory: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(directory)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".workcell-upload-"))
            .collect()
    }

    #[tokio::test]
    async fn upload_is_absent_without_write_authority() {
        let (_root, group) = group(false).await;
        let response = endpoints::upload(
            State(group),
            post("new.bin", "application/octet-stream", b"payload".to_vec()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn upload_rejects_a_foreign_media_type_and_an_oversized_body() {
        let (root, group) = group(true).await;
        let wrong_type = endpoints::upload(
            State(group.clone()),
            post("new.bin", "application/json", b"{}".to_vec()),
        )
        .await;
        assert_eq!(wrong_type.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // Content-Length is present here, so this is refused before a byte is read.
        let oversized = endpoints::upload(
            State(group.clone()),
            post("new.bin", "application/octet-stream", vec![0_u8; 2048]),
        )
        .await;
        assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!root.path().join("new.bin").exists());
        assert!(staging_files(root.path()).is_empty());
    }

    /// Without a declared length the bound has to hold while streaming, and the destination must
    /// still be untouched afterwards.
    #[tokio::test]
    async fn upload_bounds_a_chunked_body_while_streaming() {
        let (root, group) = group(true).await;
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
            (0..4).map(|_| Ok(vec![0_u8; 512])).collect();
        let request = Request::post(uri("new.bin"))
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(Body::from_stream(futures_util::stream::iter(chunks)))
            .expect("request");
        let response = endpoints::upload(State(group), request).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!root.path().join("new.bin").exists());
        assert!(staging_files(root.path()).is_empty());
    }

    #[tokio::test]
    async fn upload_refuses_a_destination_outside_the_root() {
        let (_root, group) = group(true).await;
        let response = endpoints::upload(
            State(group),
            post("../escaped.bin", "application/octet-stream", b"x".to_vec()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
