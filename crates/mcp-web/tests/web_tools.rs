mod support;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::StatusCode;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use workcell_mcp_web::{
    NativePdfExtractor, PdfExtraction, PdfExtractionError, PdfExtractor, SerpApiEngine,
    WebHttpError, WebHttpRequestKind, WebToolGroup, WebsearchBackend, WebsearchConfigurationIssue,
    WebsearchExecutionConfiguration, catalog,
};

use support::*;

#[test]
fn catalog_is_backend_specific_and_ordered() {
    let searxng = serde_json::to_value(catalog(
        2026,
        &WebsearchExecutionConfiguration::searxng("https://search.example.test"),
    ))
    .expect("serialize SearXNG catalog");
    let exa = serde_json::to_value(catalog(
        2026,
        &WebsearchExecutionConfiguration::exa("secret"),
    ))
    .expect("serialize Exa catalog");
    let exa_mcp = serde_json::to_value(catalog(2026, &WebsearchExecutionConfiguration::exa_mcp()))
        .expect("serialize Exa MCP catalog");
    let brave = serde_json::to_value(catalog(
        2026,
        &WebsearchExecutionConfiguration::brave("secret"),
    ))
    .expect("serialize Brave catalog");
    let unavailable = serde_json::to_value(catalog(
        2026,
        &WebsearchExecutionConfiguration::unconfigured(),
    ))
    .expect("serialize unavailable catalog");

    for tools in [&searxng, &exa, &exa_mcp, &brave, &unavailable] {
        assert_eq!(tools[0]["name"], json!("websearch"));
        assert_eq!(tools[1]["name"], json!("webfetch"));
    }
    let searxng_properties = searxng[0]["inputSchema"]["properties"]
        .as_object()
        .expect("SearXNG properties");
    assert!(searxng_properties.contains_key("categories"));
    assert!(searxng_properties.contains_key("language"));
    assert!(searxng_properties.contains_key("pageno"));
    assert!(
        searxng[0]["description"]
            .as_str()
            .unwrap()
            .contains("SearXNG")
    );
    assert!(!searxng[0]["description"].as_str().unwrap().contains("Exa"));

    let exa_properties = exa[0]["inputSchema"]["properties"]
        .as_object()
        .expect("Exa properties");
    assert_eq!(exa_properties.len(), 5);
    assert!(!exa_properties.contains_key("categories"));
    assert!(!exa_properties.contains_key("language"));
    assert!(!exa_properties.contains_key("pageno"));
    assert!(exa[0]["description"].as_str().unwrap().contains("Exa"));
    assert!(!exa[0]["description"].as_str().unwrap().contains("SearXNG"));

    let exa_mcp_properties = exa_mcp[0]["inputSchema"]["properties"]
        .as_object()
        .expect("Exa MCP properties");
    assert_eq!(exa_mcp_properties.len(), 3);
    for property in ["limit", "query", "timeoutSec"] {
        assert!(exa_mcp_properties.contains_key(property));
    }
    assert_eq!(exa_mcp_properties["query"]["maxLength"], json!(512));
    assert!(
        exa_mcp[0]["description"]
            .as_str()
            .unwrap()
            .contains("credential-free hosted MCP")
    );
    assert_eq!(
        serde_json::to_value(WebsearchBackend::ExaMcp).unwrap(),
        json!("exa-mcp")
    );

    let brave_properties = brave[0]["inputSchema"]["properties"]
        .as_object()
        .expect("Brave properties");
    assert_eq!(brave_properties.len(), 8);
    assert!(brave_properties.contains_key("country"));
    assert!(brave_properties.contains_key("language"));
    assert!(brave_properties.contains_key("pageno"));
    assert!(!brave_properties.contains_key("categories"));
    assert!(brave[0]["description"].as_str().unwrap().contains("Brave"));
    assert!(!brave[0]["description"].as_str().unwrap().contains("Exa"));
    assert!(
        !brave[0]["description"]
            .as_str()
            .unwrap()
            .contains("SearXNG")
    );

    let unavailable_properties = unavailable[0]["inputSchema"]["properties"]
        .as_object()
        .expect("unavailable properties");
    assert_eq!(unavailable_properties.keys().collect::<Vec<_>>(), ["query"]);
    assert!(
        unavailable[0]["description"]
            .as_str()
            .unwrap()
            .contains("configuration guidance")
    );
}

#[test]
fn kagi_and_serpapi_catalogs_are_provider_accurate_and_unique() {
    let kagi =
        serde_json::to_value(catalog(2026, &WebsearchExecutionConfiguration::kagi("key"))).unwrap();
    let google = serde_json::to_value(catalog(
        2026,
        &WebsearchExecutionConfiguration::serpapi("key", SerpApiEngine::Google),
    ))
    .unwrap();
    let bing = serde_json::to_value(catalog(
        2026,
        &WebsearchExecutionConfiguration::serpapi("key", SerpApiEngine::Bing),
    ))
    .unwrap();
    let kagi = kagi[0]["inputSchema"]["properties"].as_object().unwrap();
    let google = google[0]["inputSchema"]["properties"].as_object().unwrap();
    let bing = bing[0]["inputSchema"]["properties"].as_object().unwrap();
    assert!(!kagi.contains_key("categories") && !kagi.contains_key("language"));
    assert_eq!(kagi["pageno"]["maximum"], 10);
    assert!(google.contains_key("language") && google.contains_key("time_range"));
    assert!(!bing.contains_key("language") && !bing.contains_key("time_range"));
    assert_ne!(google, bing);
}

#[test]
fn http_response_debug_redacts_payload_headers_and_url_secrets() {
    let value = response(
        "https://example.test/path?api_key=canary#secret",
        StatusCode::OK,
        Some("secret/canary"),
        Bytes::from_static(b"body-canary"),
    );
    let debug = format!("{value:?}");
    assert!(debug.contains("example.test") && debug.contains("path"));
    assert!(
        !debug.contains("canary") && !debug.contains("secret") && !debug.contains("body-canary")
    );
}

#[tokio::test]
async fn kagi_lowering_and_data_search_parsing_are_strict() {
    let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "https://kagi.com/api/v1/search", StatusCode::OK, Some("application/json"),
        Bytes::from(serde_json::to_vec(&json!({"data":{"search":[{"title":"Kagi result","url":"https://kagi.example/","snippet":"Direct snippet"}],"other":[{"title":"ignored","url":"https://ignored.example/"}]}})).unwrap()),
    ))]));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::kagi("kagi-canary"),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(&group, "websearch", json!({"query":"rust search","country":"us","pageno":2,"time_range":"month","safesearch":2,"limit":4})).await;
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["backend"], "kagi");
    assert_eq!(structured["results"][0]["engine"], "kagi");
    let request = http.requests().remove(0);
    assert_eq!(request.method, http::Method::POST);
    assert_eq!(
        request.headers[http::header::AUTHORIZATION],
        "Bearer kagi-canary"
    );
    let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
    assert_eq!(
        body,
        json!({"query":"rust search","workflow":"search","format":"json","limit":4,"timeout":9,"page":2,"filters":{"region":"US","after":"2026-06-02"},"safe_search":true})
    );

    let invalid = call(&group, "websearch", json!({"query":"x","language":"en"})).await;
    assert!(text(&invalid).contains("language are not supported by Kagi"));
    let page = call(&group, "websearch", json!({"query":"x","pageno":11})).await;
    assert!(text(&page).contains("must not exceed 10"));
}

#[tokio::test]
async fn serpapi_engines_lower_and_parse_only_organic_results() {
    for (engine, expected_engine, expected_parameter) in [
        (SerpApiEngine::Google, "serpapi-google", "start=6"),
        (SerpApiEngine::Bing, "serpapi-bing", "first=11"),
    ] {
        let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
            "https://serpapi.com/search.json",
            StatusCode::OK,
            Some("application/json"),
            Bytes::from(serde_json::to_vec(&json!({"organic_results":[{"title":"Result","link":"https://result.example/","snippet":"Snippet"}],"news_results":[{"title":"ignored","link":"https://ignored.example/"}]})).unwrap()),
        ))]));
        let group = WebToolGroup::with_dependencies(
            WebsearchExecutionConfiguration::serpapi("serp-canary", engine),
            dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
        );
        let result = call(
            &group,
            "websearch",
            json!({"query":"query","pageno":2,"limit":6,"safesearch":2}),
        )
        .await;
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["backend"], "serpapi");
        assert_eq!(structured["results"][0]["engine"], expected_engine);
        let url = http.requests().remove(0).url;
        assert!(
            url.as_str().contains("api_key=serp-canary")
                && url.as_str().contains(expected_parameter)
        );
        if engine == SerpApiEngine::Bing {
            assert!(!url.query_pairs().any(|(key, _)| key == "count"));
        }
    }
}

#[tokio::test]
async fn kagi_requires_search_data_and_redacts_top_level_errors() {
    for body in [json!({"data": {}}), json!({"error": ["provider secret"]})] {
        let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
            "https://kagi.com/api/v1/search",
            StatusCode::OK,
            Some("application/json"),
            Bytes::from(serde_json::to_vec(&body).unwrap()),
        ))]));
        let group = WebToolGroup::with_dependencies(
            WebsearchExecutionConfiguration::kagi("key"),
            dependencies(http, Arc::new(FakeIcons::default()), default_pdf()),
        );
        let result = call(&group, "websearch", json!({"query":"x"})).await;
        let error = text(&result);
        assert!(
            error.contains("Kagi response did not contain")
                || error.contains("reported a search error")
        );
        assert!(!error.contains("provider secret"));
    }
}

#[tokio::test]
async fn serpapi_top_level_errors_are_safe_and_bing_rejects_google_fields() {
    let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "https://serpapi.com/search.json",
        StatusCode::OK,
        Some("application/json"),
        Bytes::from_static(br#"{"error":"request rejected"}"#),
    ))]));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::serpapi("key", SerpApiEngine::Bing),
        dependencies(http, Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(&group, "websearch", json!({"query":"x"})).await;
    let error = text(&result);
    assert!(error.contains("SerpApi reported a search error"));
    assert!(!error.contains("request rejected"));
    assert!(
        text(&call(&group, "websearch", json!({"query":"x","language":"en"})).await)
            .contains("not supported by SerpApi Bing")
    );
}

#[tokio::test]
async fn dispatch_matches_shared_network_conformance_fixtures_offline() {
    for case in [
        "websearch-searxng.json",
        "websearch-exa.json",
        "websearch-exa-mcp.json",
        "websearch-brave.json",
        "websearch-kagi.json",
        "websearch-serpapi-google.json",
        "websearch-serpapi-bing.json",
        "websearch-error.json",
        "webfetch-text.json",
        "webfetch-html.json",
        "webfetch-pdf.json",
    ] {
        run_network_fixture(case).await;
    }
}

#[tokio::test]
async fn unconfigured_and_disabled_search_are_successful_error_outputs() {
    let http = Arc::new(FakeHttp::default());
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::unconfigured(),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(&group, "websearch", json!({"query": "opencode plugins"})).await;
    assert_eq!(result.is_error, None);
    assert_eq!(
        text(&result),
        "Error: WORKCELL_WEBSEARCH_BACKEND is not configured. Use exa-mcp for credential-free hosted search, or configure searxng, exa, brave, kagi, or serpapi."
    );
    assert_eq!(
        result.structured_content.as_ref().expect("structured")["error"],
        json!(true)
    );
    assert_eq!(
        result.structured_content.as_ref().expect("structured")["backend"],
        Value::Null
    );
    assert!(http.requests().is_empty());

    let missing_exa_key = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::exa_without_api_key(),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &missing_exa_key,
        "websearch",
        json!({"query": "missing key"}),
    )
    .await;
    assert_eq!(result.is_error, None);
    assert!(text(&result).contains("EXA_API_KEY is not configured"));
    assert_eq!(
        result.structured_content.expect("structured")["backend"],
        json!("exa")
    );
    assert!(http.requests().is_empty());

    let missing_brave_key = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::brave_without_api_key(),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &missing_brave_key,
        "websearch",
        json!({"query": "missing key"}),
    )
    .await;
    assert_eq!(result.is_error, None);
    assert!(text(&result).contains("BRAVE_API_KEY is not configured"));
    assert_eq!(
        result.structured_content.expect("structured")["backend"],
        json!("brave")
    );
    assert!(http.requests().is_empty());

    let disabled = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::disabled(WebsearchBackend::ExaMcp),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(&disabled, "websearch", json!({"query": "disabled"})).await;
    assert!(text(&result).contains("disabled by the server configuration"));
    assert_eq!(
        result.structured_content.expect("structured")["backend"],
        json!("exa-mcp")
    );
    assert!(http.requests().is_empty());
}

#[tokio::test]
async fn searxng_lowering_normalization_deduplication_and_basic_auth_match_fixture() {
    let body = json!({
        "query": "workcell",
        "number_of_results": 42,
        "results": [
            {
                "title": "  Workcell  ",
                "url": "https://workcell.example/",
                "content": "Canvas-first   research workspace.",
                "engine": ["demo"],
                "iconDataUrl": PNG_DATA_URL
            },
            {"title": "duplicate", "url": "https://workcell.example/", "content": "ignored"},
            {"title": "bad", "url": "file:///tmp/nope", "content": "ignored"}
        ]
    });
    let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "https://search.example.test/search",
        StatusCode::OK,
        Some("application/json"),
        Bytes::from(serde_json::to_vec(&body).expect("json")),
    ))]));
    let configuration = WebsearchExecutionConfiguration::searxng_basic(
        "https://search.example.test/search",
        "user",
        "secret",
    );
    assert!(!format!("{configuration:?}").contains("secret"));
    let group = WebToolGroup::with_dependencies(
        configuration,
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &group,
        "websearch",
        json!({
            "query": "workcell",
            "categories": "general,news",
            "language": "en",
            "time_range": "month",
            "safesearch": 1,
            "limit": 5,
            "timeoutSec": 60
        }),
    )
    .await;
    assert_eq!(
        text(&result),
        "1. Workcell [demo]\n   URL: https://workcell.example/\n   Canvas-first research workspace."
    );
    let structured = result.structured_content.expect("structured");
    assert_eq!(structured["resultsFound"], json!(42));
    assert_eq!(structured["results"].as_array().expect("results").len(), 1);
    assert!(structured["results"][0].get("iconSource").is_none());
    assert!(structured["results"][0].get("iconCache").is_none());
    assert!(
        !serde_json::to_string(&structured)
            .expect("serialize")
            .contains("secret")
    );

    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.kind, WebHttpRequestKind::OperatorGet);
    assert_eq!(request.timeout, Duration::from_secs(60));
    assert_eq!(
        request
            .url
            .query_pairs()
            .find(|(name, _)| name == "q")
            .expect("q")
            .1,
        "workcell"
    );
    assert_eq!(
        request
            .url
            .query_pairs()
            .find(|(name, _)| name == "categories")
            .expect("categories")
            .1,
        "general,news"
    );
    assert_eq!(
        request
            .headers
            .get(http::header::AUTHORIZATION)
            .expect("auth"),
        "Basic dXNlcjpzZWNyZXQ="
    );
    assert!(!format!("{request:?}").contains("secret"));
}

#[tokio::test]
async fn searxng_api_key_and_bearer_auth_are_lowered_only_to_headers() {
    let cases = [
        (
            WebsearchExecutionConfiguration::searxng_api_key(
                "https://search.example.test/search",
                "api-secret",
            ),
            None,
            Some("api-secret"),
            "api-secret",
        ),
        (
            WebsearchExecutionConfiguration::searxng_bearer(
                "https://search.example.test/search",
                "bearer-secret",
            ),
            Some("Bearer bearer-secret"),
            None,
            "bearer-secret",
        ),
    ];
    for (configuration, expected_authorization, expected_api_key, secret) in cases {
        let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
            "https://search.example.test/search",
            StatusCode::OK,
            Some("application/json"),
            Bytes::from_static(b"{\"results\":[]}"),
        ))]));
        let group = WebToolGroup::with_dependencies(
            configuration,
            dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
        );
        let result = call(&group, "websearch", json!({"query": "authenticated"})).await;
        assert!(
            !serde_json::to_string(&result)
                .expect("result")
                .contains(secret)
        );
        let requests = http.requests();
        let request = &requests[0];
        assert_eq!(
            request
                .headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            expected_authorization
        );
        assert_eq!(
            request
                .headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok()),
            expected_api_key
        );
        assert!(!request.url.as_str().contains(secret));
    }
}

#[tokio::test]
async fn searxng_credentials_require_https_but_credential_free_http_remains_explicit() {
    let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "http://search.internal/search",
        StatusCode::OK,
        Some("application/json"),
        Bytes::from_static(b"{\"results\":[]}"),
    ))]));
    let credentialed = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::searxng_bearer("http://search.internal/search", "secret"),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &credentialed,
        "websearch",
        json!({"query": "insecure backend"}),
    )
    .await;
    assert!(text(&result).contains("must use HTTPS when credentials are configured"));
    assert!(http.requests().is_empty());

    let credential_free = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::searxng("http://search.internal/search"),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &credential_free,
        "websearch",
        json!({"query": "local backend"}),
    )
    .await;
    assert_eq!(result.is_error, None);
    assert_eq!(http.requests().len(), 1);
    assert_eq!(http.requests()[0].url.scheme(), "http");
}

#[tokio::test]
async fn exa_uses_fixed_post_time_lowering_and_rejects_redirects_without_secret_leaks() {
    let body = json!({"results": [{
        "title": "Exa result",
        "url": "https://result.example.test/article",
        "highlights": ["Relevant Exa", "search context."]
    }]});
    let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "https://api.exa.ai/search",
        StatusCode::OK,
        Some("application/json"),
        Bytes::from(serde_json::to_vec(&body).expect("json")),
    ))]));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::exa("exa-secret"),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &group,
        "websearch",
        json!({"query": "exa query", "limit": 5, "safesearch": 1, "time_range": "month"}),
    )
    .await;
    assert_eq!(
        text(&result),
        "1. Exa result [exa]\n   URL: https://result.example.test/article\n   Relevant Exa search context."
    );
    assert!(
        !serde_json::to_string(&result)
            .expect("result")
            .contains("exa-secret")
    );
    let requests = http.requests();
    let request = &requests[0];
    assert_eq!(request.kind, WebHttpRequestKind::PinnedProvider);
    assert_eq!(request.url.as_str(), "https://api.exa.ai/search");
    assert_eq!(request.max_redirects, 0);
    assert_eq!(request.headers["x-api-key"], "exa-secret");
    let lowered: Value =
        serde_json::from_slice(request.body.as_deref().expect("body")).expect("body JSON");
    assert_eq!(lowered["numResults"], json!(5));
    assert_eq!(lowered["moderation"], json!(true));
    assert_eq!(
        lowered["contents"]["highlights"]["maxCharacters"],
        json!(320)
    );
    assert_eq!(
        lowered["startPublishedDate"],
        json!("2026-06-02T00:00:00.000Z")
    );

    let redirects = Arc::new(FakeHttp::with_responses(vec![Err(
        WebHttpError::RedirectRejected,
    )]));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::exa("redirect-secret"),
        dependencies(redirects, Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(&group, "websearch", json!({"query": "redirect"})).await;
    assert_eq!(result.is_error, None);
    assert!(text(&result).contains("Search backend request failed"));
    assert!(
        !serde_json::to_string(&result)
            .expect("result")
            .contains("redirect-secret")
    );
}

#[tokio::test]
async fn exa_mcp_uses_anonymous_fixed_json_rpc_and_sanitizes_remote_errors() {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"content": [{
            "type": "text",
            "text": "Title: Hosted result\nURL: https://result.example.test/hosted\nPublished: N/A\nAuthor: N/A\nHighlights:\nCredential-free search context.",
            "_meta": {"private": "remote-metadata-canary"}
        }]}
    });
    let body = format!("event: message\r\ndata: {payload}\r\n\r\n");
    let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "https://mcp.exa.ai/mcp",
        StatusCode::OK,
        Some("text/event-stream"),
        Bytes::from(body),
    ))]));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::exa_mcp(),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &group,
        "websearch",
        json!({"query": "hosted query", "limit": 2}),
    )
    .await;

    assert_eq!(
        text(&result),
        "1. Hosted result [exa]\n   URL: https://result.example.test/hosted\n   Credential-free search context."
    );
    assert_eq!(
        result.structured_content.as_ref().unwrap()["backend"],
        json!("exa-mcp")
    );
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains("remote-metadata-canary")
    );
    let request = &http.requests()[0];
    assert_eq!(request.kind, WebHttpRequestKind::PinnedProvider);
    assert_eq!(request.url.as_str(), "https://mcp.exa.ai/mcp");
    assert_eq!(request.max_redirects, 0);
    assert!(request.headers.get("x-api-key").is_none());
    assert!(request.headers.get(http::header::AUTHORIZATION).is_none());
    assert_eq!(
        request.headers[http::header::ACCEPT],
        "application/json, text/event-stream"
    );
    let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
    assert_eq!(body["method"], "tools/call");
    assert_eq!(body["params"]["name"], "web_search_exa");
    assert_eq!(body["params"]["arguments"]["query"], "hosted query");
    assert_eq!(body["params"]["arguments"]["numResults"], 2);

    let remote_error = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "https://mcp.exa.ai/mcp",
        StatusCode::OK,
        Some("application/json"),
        Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"remote-private-canary"}]}}"#),
    ))]));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::exa_mcp(),
        dependencies(remote_error, Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(&group, "websearch", json!({"query": "failure"})).await;
    assert!(text(&result).contains("Exa MCP reported a search error"));
    assert!(!text(&result).contains("remote-private-canary"));

    let no_io = Arc::new(FakeHttp::default());
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::exa_mcp(),
        dependencies(no_io.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(&group, "websearch", json!({"query": "x".repeat(513)})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("must not exceed 512 characters"));
    assert!(no_io.requests().is_empty());
}

#[tokio::test]
async fn brave_uses_provider_owned_endpoint_and_native_query_lowering() {
    let body = json!({
        "query": {"original": "brave query", "altered": "brave queries"},
        "web": {"results": [
            {
                "title": "Brave result",
                "url": "https://result.example.test/brave",
                "description": "Relevant Brave search context."
            },
            {
                "title": "Duplicate",
                "url": "https://result.example.test/brave",
                "description": "Ignored"
            }
        ]}
    });
    let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "https://api.search.brave.com/res/v1/web/search",
        StatusCode::OK,
        Some("application/json"),
        Bytes::from(serde_json::to_vec(&body).expect("json")),
    ))]));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::brave("brave-secret"),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &group,
        "websearch",
        json!({
            "query": "brave query",
            "country": "de",
            "language": "de",
            "pageno": 3,
            "time_range": "month",
            "safesearch": 2,
            "limit": 20
        }),
    )
    .await;
    assert_eq!(result.is_error, None);
    assert_eq!(
        text(&result),
        "1. Brave result [brave]\n   URL: https://result.example.test/brave\n   Relevant Brave search context."
    );
    let structured = result.structured_content.expect("structured");
    assert_eq!(structured["backend"], json!("brave"));
    assert_eq!(structured["query"], json!("brave queries"));
    assert_eq!(structured["resultsFound"], json!(1));

    let requests = http.requests();
    let request = &requests[0];
    assert_eq!(request.kind, WebHttpRequestKind::PinnedProvider);
    assert_eq!(
        request.url.as_str().split('?').next(),
        Some("https://api.search.brave.com/res/v1/web/search")
    );
    assert_eq!(request.max_redirects, 0);
    assert_eq!(request.headers["x-subscription-token"], "brave-secret");
    let query = request
        .url
        .query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(query["q"], "brave query");
    assert_eq!(query["country"], "DE");
    assert_eq!(query["search_lang"], "de");
    assert_eq!(query["offset"], "2");
    assert_eq!(query["freshness"], "pm");
    assert_eq!(query["safesearch"], "strict");
    assert_eq!(query["count"], "20");
    assert_eq!(query["result_filter"], "web");
    assert_eq!(query["text_decorations"], "false");
    assert!(
        !serde_json::to_string(&structured)
            .unwrap()
            .contains("brave-secret")
    );
}

#[tokio::test]
async fn search_icon_enrichment_is_unique_batched_and_best_effort() {
    let results = (0..7)
        .map(|index| {
            json!({
                "title": format!("Result {index}"),
                "url": format!("https://origin-{index}.example.test/article"),
                "content": "Snippet"
            })
        })
        .chain(std::iter::once(json!({
            "title": "Same origin",
            "url": "https://origin-0.example.test/other",
            "content": "Snippet"
        })))
        .collect::<Vec<_>>();
    let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "https://search.example.test/search",
        StatusCode::OK,
        Some("application/json"),
        Bytes::from(serde_json::to_vec(&json!({"results": results})).expect("json")),
    ))]));
    let icons = Arc::new(FakeIcons {
        delay: true,
        ..FakeIcons::default()
    });
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::searxng("https://search.example.test/search"),
        dependencies(http, icons.clone(), default_pdf()),
    );
    let result = call(&group, "websearch", json!({"query": "icons", "limit": 10})).await;
    assert_eq!(
        result.structured_content.expect("structured")["results"]
            .as_array()
            .expect("results")
            .len(),
        8
    );
    assert_eq!(icons.requests.lock().expect("icon requests").len(), 7);
    assert!(icons.maximum_active.load(Ordering::SeqCst) <= 3);

    let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "https://search.example.test/search",
        StatusCode::OK,
        Some("application/json"),
        Bytes::from_static(
            b"{\"results\":[{\"title\":\"Result\",\"url\":\"https://result.example.test/\"}]}",
        ),
    ))]));
    let failing_icons = Arc::new(FakeIcons {
        fail: true,
        ..FakeIcons::default()
    });
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::searxng("https://search.example.test/search"),
        dependencies(http, failing_icons, default_pdf()),
    );
    let result = call(&group, "websearch", json!({"query": "icon failure"})).await;
    assert_eq!(result.is_error, None);
    assert!(
        result.structured_content.expect("structured")["results"][0]
            .get("iconDataUrl")
            .is_none()
    );
}

#[tokio::test]
async fn disabled_source_icons_skip_resolution_and_omit_all_icon_fields() {
    let search_body = json!({"results": [{
        "title": "Result",
        "url": "https://result.example.test/article",
        "content": "Snippet",
        "iconDataUrl": PNG_DATA_URL
    }]});
    let http = Arc::new(FakeHttp::with_responses(vec![
        Ok(response(
            "https://search.example.test/search",
            StatusCode::OK,
            Some("application/json"),
            Bytes::from(serde_json::to_vec(&search_body).unwrap()),
        )),
        Ok(response(
            "https://page.example.test/article",
            StatusCode::OK,
            Some("text/html"),
            Bytes::from_static(b"<html><head><title>Page</title></head><body>Body</body></html>"),
        )),
    ]));
    let icons = Arc::new(FakeIcons::default());
    let disabled =
        dependencies(http, icons.clone(), default_pdf()).with_source_icons_enabled(false);
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::searxng("https://search.example.test/search"),
        disabled,
    );

    let search = call(&group, "websearch", json!({"query": "no icons"})).await;
    let search_result = &search.structured_content.as_ref().unwrap()["results"][0];
    assert!(search_result.get("iconUrl").is_none());
    assert!(search_result.get("iconDataUrl").is_none());

    let fetch = call(
        &group,
        "webfetch",
        json!({"url": "https://page.example.test/article"}),
    )
    .await;
    let fetch = fetch.structured_content.as_ref().unwrap();
    assert!(fetch.get("iconUrl").is_none());
    assert!(fetch.get("iconDataUrl").is_none());
    assert!(icons.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn search_normalization_caps_rows_fields_and_total_model_output() {
    let long_title = "title".repeat(300);
    let long_engine = "engine".repeat(300);
    let long_snippet = "snippet ".repeat(300);
    let long_path = "a".repeat(3_800);
    let large_results = (0..100)
        .map(|index| {
            json!({
                "title": long_title,
                "url": format!("https://result-{index}.example.test/{long_path}"),
                "content": long_snippet,
                "engine": long_engine,
            })
        })
        .collect::<Vec<_>>();
    let duplicate_rows = (0..100)
        .map(|_| json!({"title": "duplicate", "url": "https://duplicate.example.test/"}))
        .chain(std::iter::once(json!({
            "title": "outside budget",
            "url": "https://outside-budget.example.test/"
        })))
        .collect::<Vec<_>>();
    let http = Arc::new(FakeHttp::with_responses(vec![
        Ok(response(
            "https://search.example.test/search",
            StatusCode::OK,
            Some("application/json"),
            Bytes::from(serde_json::to_vec(&json!({"results": large_results})).unwrap()),
        )),
        Ok(response(
            "https://search.example.test/search",
            StatusCode::OK,
            Some("application/json"),
            Bytes::from(serde_json::to_vec(&json!({"results": duplicate_rows})).unwrap()),
        )),
    ]));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::searxng("https://search.example.test/search"),
        dependencies(
            http,
            Arc::new(FakeIcons {
                fail: true,
                ..FakeIcons::default()
            }),
            default_pdf(),
        ),
    );
    let large = call(
        &group,
        "websearch",
        json!({"query": "bounded", "limit": 25}),
    )
    .await;
    let structured = large.structured_content.as_ref().expect("structured");
    let results = structured["results"].as_array().expect("results");
    assert_eq!(results.len(), 25);
    assert!(results[0]["title"].as_str().unwrap().chars().count() <= 300);
    assert!(results[0]["url"].as_str().unwrap().len() <= 4 * 1024);
    assert!(results[0]["engine"].as_str().unwrap().chars().count() <= 64);
    assert!(results[0]["snippet"].as_str().unwrap().chars().count() <= 320);
    assert!(structured.get("formattedResults").is_none());
    assert!(text(&large).len() <= 50 * 1024);

    let budgeted = call(
        &group,
        "websearch",
        json!({"query": "row budget", "limit": 25}),
    )
    .await;
    let results = budgeted.structured_content.unwrap()["results"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["url"], json!("https://duplicate.example.test/"));
}

#[tokio::test]
async fn icon_cancellation_is_propagated_for_search_and_fetch() {
    let search_http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "https://search.example.test/search",
        StatusCode::OK,
        Some("application/json"),
        Bytes::from_static(
            b"{\"results\":[{\"title\":\"Result\",\"url\":\"https://result.example.test/\"}]}",
        ),
    ))]));
    let cancelling_icons = Arc::new(FakeIcons {
        cancel: true,
        ..FakeIcons::default()
    });
    let search = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::searxng("https://search.example.test/search"),
        dependencies(search_http, cancelling_icons.clone(), default_pdf()),
    );
    let result = call(&search, "websearch", json!({"query": "cancel icon"})).await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(text(&result), "Tool invocation was aborted.");

    let fetch_http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "https://example.test/page",
        StatusCode::OK,
        Some("text/plain"),
        Bytes::from_static(b"body"),
    ))]));
    let fetch = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::unconfigured(),
        dependencies(fetch_http, cancelling_icons, default_pdf()),
    );
    let result = call(
        &fetch,
        "webfetch",
        json!({"url": "https://example.test/page"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(text(&result), "Tool invocation was aborted.");
}

#[tokio::test]
async fn webfetch_normalizes_urls_and_rejects_private_initial_and_redirect_targets() {
    let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "https://example.test/plain.txt",
        StatusCode::OK,
        Some("text/plain"),
        Bytes::from_static(b"secure"),
    ))]));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::unconfigured(),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &group,
        "webfetch",
        json!({"url": "http://example.test/plain.txt", "format": "text", "timeout": 120}),
    )
    .await;
    assert_eq!(text(&result), "secure");
    let request = &http.requests()[0];
    assert_eq!(request.url.as_str(), "https://example.test/plain.txt");
    assert_eq!(request.timeout, Duration::from_secs(60));

    let private = call(
        &group,
        "webfetch",
        json!({"url": "https://127.0.0.1/private"}),
    )
    .await;
    assert_eq!(private.is_error, Some(true));
    assert!(text(&private).contains("blocked by network safety rules"));
    assert_eq!(http.requests().len(), 1);

    let redirects = Arc::new(FakeHttp::with_responses(vec![Ok(response(
        "http://169.254.169.254/latest/meta-data",
        StatusCode::OK,
        Some("text/plain"),
        Bytes::from_static(b"secret metadata"),
    ))]));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::unconfigured(),
        dependencies(redirects, Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &group,
        "webfetch",
        json!({"url": "https://example.test/start"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("blocked by network safety rules"));
    assert!(!text(&result).contains("secret metadata"));
}

#[tokio::test]
async fn webfetch_html_readability_noise_raw_html_and_icon_failure() {
    let html = r#"<html><head><title>Research Note</title></head><body><nav>Navigation noise</nav><main><article><h1>Research Note</h1><p>This language-neutral article contains enough useful words to exercise readability extraction while preserving a <a href="/source">source link</a> for later review. The text describes a general research workflow without domain-specific assumptions or hidden application data.</p><p>A second paragraph adds context, evidence, and explanatory detail so the extractor can select the article body with a stable high-signal result.</p></article><p>__NEXT_DATA__ {"payload":"noise"}</p></main><script>removeMe()</script></body></html>"#;
    let http = Arc::new(FakeHttp::with_responses(vec![
        Ok(response(
            "https://example.test/article.html",
            StatusCode::OK,
            Some("text/html; charset=utf-8"),
            Bytes::copy_from_slice(html.as_bytes()),
        )),
        Ok(response(
            "https://example.test/raw.html",
            StatusCode::OK,
            Some("text/html"),
            Bytes::copy_from_slice(html.as_bytes()),
        )),
        Ok(response(
            "https://example.test/fallback.html",
            StatusCode::OK,
            Some("text/html"),
            Bytes::from_static(
                b"<html><head><title>Fallback</title></head><body><p>Short <a href='/target'>content</a>.</p><script>removeMe()</script></body></html>",
            ),
        )),
    ]));
    let icons = Arc::new(FakeIcons {
        fail: true,
        ..FakeIcons::default()
    });
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::unconfigured(),
        dependencies(http, icons, default_pdf()),
    );
    let result = call(
        &group,
        "webfetch",
        json!({"url": "https://example.test/article.html"}),
    )
    .await;
    assert_eq!(result.is_error, None);
    assert_eq!(
        text(&result),
        "# Research Note\n\nThis language-neutral article contains enough useful words to exercise readability extraction while preserving a [source link](https://example.test/source) for later review. The text describes a general research workflow without domain-specific assumptions or hidden application data.\n\nA second paragraph adds context, evidence, and explanatory detail so the extractor can select the article body with a stable high-signal result."
    );
    let structured = result.structured_content.expect("structured");
    assert_eq!(structured["title"], json!("Research Note"));
    assert_eq!(structured["extractionMethod"], json!("readability"));
    assert!(structured.get("iconDataUrl").is_none());
    assert!(structured.get("summaryInput").is_none());

    let raw = call(
        &group,
        "webfetch",
        json!({"url": "https://example.test/raw.html", "format": "html"}),
    )
    .await;
    assert_eq!(text(&raw), html);
    assert!(
        raw.structured_content.expect("structured")["summaryInput"]
            .as_str()
            .expect("summary")
            .contains("source link")
    );

    let fallback = call(
        &group,
        "webfetch",
        json!({"url": "https://example.test/fallback.html"}),
    )
    .await;
    assert_eq!(
        fallback.structured_content.as_ref().expect("structured")["extractionMethod"],
        json!("fallback")
    );
    assert!(text(&fallback).contains("[content](https://example.test/target)"));
    assert!(!text(&fallback).contains("removeMe"));
}

#[tokio::test]
async fn webfetch_bounds_body_lines_and_classifies_cancellation_and_content_errors() {
    let large = (1..=2_105)
        .map(|line| format!("line-{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let http = Arc::new(FakeHttp::with_responses(vec![
        Ok(response(
            "https://example.test/large.txt",
            StatusCode::OK,
            Some("text/plain"),
            Bytes::from(large),
        )),
        Ok(response(
            "https://example.test/image.bin",
            StatusCode::OK,
            Some("image/png"),
            Bytes::from_static(b"png"),
        )),
    ]));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::unconfigured(),
        dependencies(http, Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &group,
        "webfetch",
        json!({"url": "https://example.test/large.txt", "format": "text"}),
    )
    .await;
    assert!(text(&result).contains("line-2000"));
    assert!(!text(&result).contains("line-2001"));
    assert_eq!(
        result.structured_content.expect("structured")["truncated"],
        json!(true)
    );

    let non_text = call(
        &group,
        "webfetch",
        json!({"url": "https://example.test/image.bin"}),
    )
    .await;
    assert_eq!(non_text.is_error, Some(true));
    assert!(text(&non_text).contains("non-text content type: image/png"));

    let cancellation_http = Arc::new(FakeHttp::default());
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::unconfigured(),
        dependencies(
            cancellation_http.clone(),
            Arc::new(FakeIcons::default()),
            default_pdf(),
        ),
    );
    let token = CancellationToken::new();
    token.cancel();
    let result = group
        .dispatch(
            "webfetch",
            json!({"url": "https://example.test/cancel"}),
            token,
        )
        .await
        .expect("known")
        .expect("protocol");
    assert_eq!(result.is_error, Some(true));
    assert_eq!(text(&result), "Tool invocation was aborted.");
    assert!(cancellation_http.requests().is_empty());
}

#[tokio::test]
async fn webfetch_pdf_extract_attachment_and_parser_failures_are_classified() {
    let pdf = Bytes::from_static(b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n%%EOF\n");
    let called = Arc::new(AtomicBool::new(false));
    let http = Arc::new(FakeHttp::with_responses(vec![
        Ok(response(
            "https://example.test/study.pdf",
            StatusCode::OK,
            Some("application/pdf"),
            pdf.clone(),
        )),
        Ok(response(
            "https://example.test/study%20copy.pdf",
            StatusCode::OK,
            Some("application/octet-stream"),
            pdf.clone(),
        )),
    ]));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::unconfigured(),
        dependencies(
            http,
            Arc::new(FakeIcons::default()),
            Arc::new(FakePdf {
                called: called.clone(),
                outcome: PdfOutcome::Success,
            }),
        ),
    );
    let result = call(
        &group,
        "webfetch",
        json!({"url": "https://example.test/study.pdf"}),
    )
    .await;
    assert!(called.load(Ordering::SeqCst));
    assert_eq!(
        text(&result),
        "PDF body text with useful research evidence."
    );
    let structured = result.structured_content.expect("structured");
    assert_eq!(structured["pdfMode"], json!("extract"));
    assert_eq!(structured["title"], json!("PDF Study"));

    called.store(false, Ordering::SeqCst);
    let attachment = call(
        &group,
        "webfetch",
        json!({"url": "https://example.test/study%20copy.pdf", "pdfMode": "attachment"}),
    )
    .await;
    assert!(!called.load(Ordering::SeqCst));
    assert_eq!(
        text(&attachment),
        "PDF fetched successfully. The PDF is available as an application/pdf attachment."
    );
    assert!(!text(&attachment).contains("data:application/pdf"));
    let structured = attachment.structured_content.expect("structured");
    assert_eq!(
        structured["pdfAttachment"]["filename"],
        json!("study copy.pdf")
    );
    assert!(
        structured["pdfAttachment"]["url"]
            .as_str()
            .expect("data URL")
            .starts_with("data:application/pdf;base64,")
    );

    for outcome in [PdfOutcome::Failure, PdfOutcome::Panic] {
        let http = Arc::new(FakeHttp::with_responses(vec![Ok(response(
            "https://example.test/bad.pdf",
            StatusCode::OK,
            Some("application/pdf"),
            pdf.clone(),
        ))]));
        let group = WebToolGroup::with_dependencies(
            WebsearchExecutionConfiguration::unconfigured(),
            dependencies(
                http,
                Arc::new(FakeIcons::default()),
                Arc::new(FakePdf {
                    called: Arc::new(AtomicBool::new(false)),
                    outcome,
                }),
            ),
        );
        let result = call(
            &group,
            "webfetch",
            json!({"url": "https://example.test/bad.pdf"}),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(text(&result), "Failed to parse PDF content.");
    }
}

#[tokio::test]
async fn webfetch_rejects_truncated_pdf_attachments() {
    let mut truncated = response(
        "https://example.test/large.pdf",
        StatusCode::OK,
        Some("application/pdf"),
        Bytes::from_static(b"%PDF-1.7\npartial"),
    );
    truncated.truncated = true;
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::unconfigured(),
        dependencies(
            Arc::new(FakeHttp::with_responses(vec![Ok(truncated)])),
            Arc::new(FakeIcons::default()),
            default_pdf(),
        ),
    );
    let result = call(
        &group,
        "webfetch",
        json!({"url": "https://example.test/large.pdf", "pdfMode": "attachment"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("exceeded the attachment size limit"));
    assert!(!text(&result).contains("fetched successfully"));
}

struct SlowPdf {
    finished: Arc<AtomicBool>,
}

impl PdfExtractor for SlowPdf {
    fn extract(&self, _bytes: &[u8]) -> Result<PdfExtraction, PdfExtractionError> {
        std::thread::sleep(Duration::from_millis(1_500));
        self.finished.store(true, Ordering::SeqCst);
        Ok(PdfExtraction::default())
    }
}

#[tokio::test]
async fn webfetch_pdf_parser_obeys_the_caller_deadline_without_claiming_worker_termination() {
    let finished = Arc::new(AtomicBool::new(false));
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::unconfigured(),
        dependencies(
            Arc::new(FakeHttp::with_responses(vec![Ok(response(
                "https://example.test/slow.pdf",
                StatusCode::OK,
                Some("application/pdf"),
                Bytes::from_static(b"%PDF-1.7\nslow"),
            ))])),
            Arc::new(FakeIcons::default()),
            Arc::new(SlowPdf {
                finished: finished.clone(),
            }),
        ),
    );
    let started = Instant::now();
    let result = call(
        &group,
        "webfetch",
        json!({"url": "https://example.test/slow.pdf", "timeout": 1}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("timed out after 1 seconds"));
    assert!(started.elapsed() < Duration::from_millis(1_300));
    assert!(!finished.load(Ordering::SeqCst));
    tokio::time::sleep(Duration::from_millis(550)).await;
    assert!(finished.load(Ordering::SeqCst));
}

#[test]
fn native_pdf_extractor_safely_rejects_the_synthetic_conformance_asset() {
    // Exact PDF dispatch is covered above with the fixture's injected extractor.
    // Native parser output is library/platform dependent, so this native case is
    // deliberately invariant-only: malformed synthetic input must fail safely.
    let bytes = include_bytes!("../../../fixtures/mcp-conformance/assets/network/study.pdf");
    assert!(NativePdfExtractor.extract(bytes).is_err());
}

#[tokio::test]
async fn unknown_tools_and_invalid_arguments_are_classified_without_io() {
    let http = Arc::new(FakeHttp::default());
    let group = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::unconfigured(),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    assert!(
        group
            .dispatch("not_web", json!({}), CancellationToken::new())
            .await
            .is_none()
    );
    let result = call(&group, "websearch", json!({"query": "x", "limit": 0})).await;
    assert_eq!(result.is_error, Some(true));
    let result = call(&group, "websearch", json!({"query": "x", "limit": 1})).await;
    assert_eq!(result.is_error, Some(true));

    let exa = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::exa("secret"),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &exa,
        "websearch",
        json!({"query": "x", "categories": "news"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("not supported by Exa"));

    let brave = WebToolGroup::with_dependencies(
        WebsearchExecutionConfiguration::brave("secret"),
        dependencies(http.clone(), Arc::new(FakeIcons::default()), default_pdf()),
    );
    let result = call(
        &brave,
        "websearch",
        json!({"query": "x", "categories": "news"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("not supported by Brave"));
    let result = call(&brave, "websearch", json!({"query": "x", "country": "USA"})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("two-letter code"));
    let result = call(&brave, "websearch", json!({"query": "word ".repeat(51)})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("50 words"));
    let result = call(&brave, "websearch", json!({"query": "x", "limit": 21})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("limit must not exceed 20"));

    let result = call(&group, "webfetch", json!({"url": "file:///etc/passwd"})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("must use http or https"));
    assert!(http.requests().is_empty());
}

async fn run_network_fixture(case: &str) {
    let root = conformance_root();
    let fixture: Value = serde_json::from_str(
        &fs::read_to_string(root.join("network/v1").join(case))
            .unwrap_or_else(|error| panic!("read fixture {case}: {error}")),
    )
    .unwrap_or_else(|error| panic!("parse fixture {case}: {error}"));
    assert_eq!(fixture["fixtureVersion"], json!(1), "{case}");

    let responses = fixture["setup"]["mockResponses"]
        .as_array()
        .map(|responses| {
            responses
                .iter()
                .map(|fixture_response| {
                    let body = if !fixture_response["jsonBody"].is_null() {
                        serde_json::to_vec(&fixture_response["jsonBody"])
                            .expect("serialize fixture response")
                    } else {
                        fs::read(
                            root.join(
                                fixture_response["bodyAsset"]
                                    .as_str()
                                    .expect("fixture response body asset"),
                            ),
                        )
                        .expect("read fixture response body")
                    };
                    Ok(response(
                        fixture_response["url"]
                            .as_str()
                            .expect("fixture response URL"),
                        StatusCode::from_u16(
                            u16::try_from(
                                fixture_response["status"]
                                    .as_u64()
                                    .expect("fixture response status"),
                            )
                            .expect("fixture response status fits u16"),
                        )
                        .expect("valid fixture response status"),
                        fixture_response["headers"]["content-type"].as_str(),
                        Bytes::from(body),
                    ))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let http = Arc::new(FakeHttp::with_responses(responses));
    let icons = Arc::new(FakeIcons {
        fail: true,
        ..FakeIcons::default()
    });
    let configuration = fixture_configuration(&fixture["configuration"]);
    let pdf: Arc<dyn PdfExtractor> = if fixture["setup"]["pdfExtractor"].is_object() {
        Arc::new(FixturePdf::from_fixture(&fixture["setup"]["pdfExtractor"]))
    } else {
        default_pdf()
    };
    let group =
        WebToolGroup::with_dependencies(configuration, dependencies(http.clone(), icons, pdf));
    let result = call(
        &group,
        fixture["tool"].as_str().expect("fixture tool"),
        fixture["input"].clone(),
    )
    .await;
    let expected = &fixture["expected"];

    assert_eq!(
        result.is_error == Some(true),
        expected["isError"].as_bool().unwrap_or(false),
        "{case}: MCP error classification"
    );
    assert_eq!(
        text(&result),
        expected["contentText"]
            .as_str()
            .expect("expected content text"),
        "{case}: model-facing text"
    );
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .expect("fixture structured content"),
        &expected["structuredContent"],
        "{case}: structured content"
    );

    for excluded in expected["contentExcludes"].as_array().into_iter().flatten() {
        assert!(
            !text(&result).contains(excluded.as_str().expect("excluded content")),
            "{case}: excluded model-facing content"
        );
    }
    let serialized_output = format!(
        "{}{}",
        text(&result),
        result
            .structured_content
            .as_ref()
            .expect("fixture structured content")
    );
    for excluded in expected["serializedOutputExcludes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        assert!(
            !serialized_output.contains(excluded.as_str().expect("excluded serialized output")),
            "{case}: secret leaked into output"
        );
    }

    let actual_requests = http.requests().iter().map(request_json).collect::<Vec<_>>();
    if let Some(exact) = expected["requests"].as_array() {
        assert_eq!(&actual_requests, exact, "{case}: exact request sequence");
    }
    for included in expected["requestIncludes"].as_array().into_iter().flatten() {
        assert!(
            actual_requests
                .iter()
                .any(|actual| json_contains(actual, included)),
            "{case}: missing request {included} in {actual_requests:?}"
        );
    }
}

struct FixturePdf {
    expected_prefix: Vec<u8>,
    text: String,
    title: Option<String>,
}

impl FixturePdf {
    fn from_fixture(value: &Value) -> Self {
        Self {
            expected_prefix: value["expectedInputPrefix"]
                .as_str()
                .expect("PDF fixture input prefix")
                .as_bytes()
                .to_vec(),
            text: value["result"]["text"]
                .as_str()
                .expect("PDF fixture text")
                .to_owned(),
            title: value["result"]["title"].as_str().map(str::to_owned),
        }
    }
}

impl PdfExtractor for FixturePdf {
    fn extract(&self, bytes: &[u8]) -> Result<PdfExtraction, PdfExtractionError> {
        assert!(
            bytes.starts_with(&self.expected_prefix),
            "PDF fixture extractor received the wrong asset"
        );
        Ok(PdfExtraction {
            text: self.text.clone(),
            title: self.title.clone(),
            truncated: false,
        })
    }
}

fn conformance_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/mcp-conformance")
        .canonicalize()
        .expect("canonical conformance fixture root")
}

fn fixture_configuration(value: &Value) -> WebsearchExecutionConfiguration {
    match value["backend"].as_str() {
        Some("searxng") => WebsearchExecutionConfiguration::searxng(
            value["endpoint"]
                .as_str()
                .expect("SearXNG fixture endpoint"),
        ),
        Some("exa") => WebsearchExecutionConfiguration::exa(
            value["credential"]["key"]
                .as_str()
                .expect("Exa fixture API key"),
        ),
        Some("exa-mcp") => WebsearchExecutionConfiguration::exa_mcp(),
        Some("brave") => WebsearchExecutionConfiguration::brave(
            value["credential"]["key"]
                .as_str()
                .expect("Brave fixture API key"),
        ),
        Some("kagi") => WebsearchExecutionConfiguration::kagi(
            value["credential"]["key"]
                .as_str()
                .expect("Kagi fixture API key"),
        ),
        Some("serpapi-google") => WebsearchExecutionConfiguration::serpapi(
            value["credential"]["key"]
                .as_str()
                .expect("SerpApi fixture API key"),
            SerpApiEngine::Google,
        ),
        Some("serpapi-bing") => WebsearchExecutionConfiguration::serpapi(
            value["credential"]["key"]
                .as_str()
                .expect("SerpApi fixture API key"),
            SerpApiEngine::Bing,
        ),
        None => WebsearchExecutionConfiguration::unavailable(
            Some(WebsearchBackend::Searxng),
            WebsearchConfigurationIssue::MissingSearxngEndpoint,
        ),
        Some(backend) => panic!("unsupported fixture backend: {backend}"),
    }
}

fn request_json(request: &workcell_mcp_web::WebHttpRequest) -> Value {
    let headers = request
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                Value::String(value.to_str().expect("fixture request header").to_owned()),
            )
        })
        .collect::<Map<_, _>>();
    let mut output = Map::from_iter([
        (
            "method".to_owned(),
            Value::String(request.method.as_str().to_owned()),
        ),
        (
            "url".to_owned(),
            Value::String(request.url.as_str().to_owned()),
        ),
        ("headers".to_owned(), Value::Object(headers)),
    ]);
    if request.max_redirects == 0 {
        output.insert("redirect".to_owned(), Value::String("error".to_owned()));
    }
    if let Some(body) = request.body.as_deref() {
        output.insert(
            "jsonBody".to_owned(),
            serde_json::from_slice(body).expect("fixture request JSON body"),
        );
    }
    Value::Object(output)
}

fn json_contains(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|actual| json_contains(actual, value))
        }),
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| json_contains(actual, expected))
        }
        _ => actual == expected,
    }
}
