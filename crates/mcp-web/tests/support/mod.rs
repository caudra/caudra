use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http::{HeaderMap, HeaderValue, StatusCode};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;
use workcell_mcp_web::{
    Clock, IconProvider, IconRequest, PdfExtraction, PdfExtractionError, PdfExtractor,
    WebHttpError, WebHttpRequest, WebHttpResponse, WebHttpTransport, WebToolDependencies,
    WebToolGroup,
};
use workcell_source_icons::{
    ResolvedSourceIcon, SourceIconCacheInfo, SourceIconError, SourceIconSource,
};

pub(crate) const PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

#[derive(Default)]
pub(crate) struct FakeHttp {
    responses: Mutex<VecDeque<Result<WebHttpResponse, WebHttpError>>>,
    requests: Mutex<Vec<WebHttpRequest>>,
}

impl FakeHttp {
    pub(crate) fn with_responses(responses: Vec<Result<WebHttpResponse, WebHttpError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::default(),
        }
    }

    pub(crate) fn requests(&self) -> Vec<WebHttpRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

#[async_trait]
impl WebHttpTransport for FakeHttp {
    async fn execute(&self, request: WebHttpRequest) -> Result<WebHttpResponse, WebHttpError> {
        if request.cancellation.is_cancelled() {
            return Err(WebHttpError::Cancelled);
        }
        self.requests.lock().expect("requests").push(request);
        self.responses
            .lock()
            .expect("responses")
            .pop_front()
            .unwrap_or(Err(WebHttpError::RequestFailed))
    }
}

#[derive(Default)]
pub(crate) struct FakeIcons {
    pub fail: bool,
    pub cancel: bool,
    pub delay: bool,
    pub requests: Mutex<Vec<IconRequest>>,
    pub active: AtomicUsize,
    pub maximum_active: AtomicUsize,
}

#[async_trait]
impl IconProvider for FakeIcons {
    async fn resolve(
        &self,
        request: IconRequest,
    ) -> Result<Option<ResolvedSourceIcon>, SourceIconError> {
        self.requests
            .lock()
            .expect("icon requests")
            .push(request.clone());
        if self.cancel {
            return Err(SourceIconError::Cancelled);
        }
        if self.fail {
            return Ok(None);
        }
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum_active.fetch_max(active, Ordering::SeqCst);
        if self.delay {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(Some(ResolvedSourceIcon {
            icon_url: format!("{}/favicon.png", request.page_url.trim_end_matches('/')),
            icon_data_url: PNG_DATA_URL.to_owned(),
            icon_source: SourceIconSource::PathFallback,
            cache: SourceIconCacheInfo::default(),
        }))
    }
}

struct FixedClock(SystemTime);

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

pub(crate) struct FakePdf {
    pub called: Arc<AtomicBool>,
    pub outcome: PdfOutcome,
}

pub(crate) enum PdfOutcome {
    Success,
    Failure,
    Panic,
}

impl PdfExtractor for FakePdf {
    fn extract(&self, bytes: &[u8]) -> Result<PdfExtraction, PdfExtractionError> {
        self.called.store(true, Ordering::SeqCst);
        assert!(bytes.starts_with(b"%PDF-"));
        match self.outcome {
            PdfOutcome::Success => Ok(PdfExtraction {
                text: "PDF body text with useful research evidence.".to_owned(),
                title: Some("PDF Study".to_owned()),
                truncated: false,
            }),
            PdfOutcome::Failure => Err(PdfExtractionError),
            PdfOutcome::Panic => panic!("malformed parser input"),
        }
    }
}

pub(crate) fn response(
    url: &str,
    status: StatusCode,
    content_type: Option<&str>,
    body: impl Into<Bytes>,
) -> WebHttpResponse {
    let mut headers = HeaderMap::new();
    if let Some(content_type) = content_type {
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_str(content_type).expect("content type"),
        );
    }
    WebHttpResponse {
        status,
        headers,
        final_url: Url::parse(url).expect("response URL"),
        body: body.into(),
        truncated: false,
    }
}

pub(crate) fn dependencies(
    http: Arc<FakeHttp>,
    icons: Arc<FakeIcons>,
    pdf: Arc<dyn PdfExtractor>,
) -> WebToolDependencies {
    let now = DateTime::parse_from_rfc3339("2026-07-02T00:00:00.000Z")
        .expect("date")
        .with_timezone(&Utc);
    WebToolDependencies::new(http, icons, Arc::new(FixedClock(now.into())), pdf)
        .with_fixture_hostnames()
}

pub(crate) fn default_pdf() -> Arc<dyn PdfExtractor> {
    Arc::new(FakePdf {
        called: Arc::new(AtomicBool::new(false)),
        outcome: PdfOutcome::Success,
    })
}

pub(crate) async fn call(group: &WebToolGroup, name: &str, input: Value) -> CallToolResult {
    group
        .dispatch(name, input, CancellationToken::new())
        .await
        .expect("known tool")
        .expect("protocol result")
}

pub(crate) fn text(result: &CallToolResult) -> &str {
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("expected text content")
    };
    &content.text
}
