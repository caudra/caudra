//! Which protocol a connection speaks. A modern server (2026-07-28) is
//! stateless and reads its version, capabilities and client identity from
//! every request's `_meta`; a legacy one takes them once, in `initialize`.
//! A `server/discover` probe tells them apart, and the answer holds for the
//! connection's life, never longer.

use std::time::Duration;

use futures_lite::future;
use serde_json::{Value, json};
use smol::Timer;
use tracing::{debug, info};

use super::call::{CallScope, with_meta};
use super::error::McpError;
use super::protocol::{
    CLIENT_CAPABILITIES_META, CLIENT_INFO_META, DISCOVER_METHOD, HEADER_MISMATCH,
    LATEST_PROTOCOL_VERSION, LEGACY_PROTOCOL_VERSION, MISSING_REQUIRED_CLIENT_CAPABILITY,
    PROTOCOL_VERSION_META, ServerDiscovery, UNSUPPORTED_PROTOCOL_VERSION, client_info,
    elicitation_capability,
};
use super::transport::{McpTransport, initialize};

/// The longest a server may sit on the probe before it counts as legacy.
pub const DISCOVER_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// The modern versions Caudra implements, newest first.
pub const MODERN_VERSIONS: &[&str] = &[LATEST_PROTOCOL_VERSION];
const SUPPORTED_VERSIONS_KEY: &str = "supportedVersions";
const SUPPORTED_KEY: &str = "supported";
const ELICITATION_KEY: &str = "elicitation";
const HTTP_UNAUTHORIZED: u16 = 401;
const NOT_A_DISCOVER_RESULT: &str = "the probe's answer is not a discover result";
const NO_COMMON_VERSION: &str = "no modern protocol version in common";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolEra {
    Modern { version: &'static str },
    Legacy { version: String },
}

impl Default for ProtocolEra {
    fn default() -> Self {
        Self::Legacy {
            version: LEGACY_PROTOCOL_VERSION.into(),
        }
    }
}

impl ProtocolEra {
    pub fn modern(&self) -> Option<&'static str> {
        match self {
            Self::Modern { version } => Some(version),
            Self::Legacy { .. } => None,
        }
    }

    /// `params` as this era sends them: a modern request says who is asking,
    /// in which version, and what it can do. It declares elicitation only
    /// when someone can answer, and never Roots, Sampling or Logging.
    pub(super) fn with_meta(&self, params: Option<Value>, scope: &CallScope) -> Option<Value> {
        let Some(version) = self.modern() else {
            return params;
        };
        let capabilities = match scope.elicitor {
            Some(_) => json!({ELICITATION_KEY: elicitation_capability()}),
            None => json!({}),
        };
        with_meta(
            params,
            [
                (PROTOCOL_VERSION_META, version.into()),
                (CLIENT_CAPABILITIES_META, capabilities),
                (CLIENT_INFO_META, client_info()),
            ],
        )
    }
}

/// Sets up a fresh connection: the modern protocol when the server answers
/// the probe as a modern server would, else `initialize` on the same
/// connection. Then opens whatever stream tells of list changes.
pub async fn connect(
    transport: &dyn McpTransport,
    timeout: Duration,
) -> Result<ServerDiscovery, McpError> {
    let discovery = match select(transport, timeout.min(DISCOVER_PROBE_TIMEOUT)).await? {
        Some(discovery) => discovery,
        None => {
            set_era(transport, ProtocolEra::default());
            let discovery = initialize(transport).await?;
            set_era(
                transport,
                ProtocolEra::Legacy {
                    version: discovery.protocol_version.clone(),
                },
            );
            discovery
        }
    };
    if let Some(inbound) = transport.inbound() {
        inbound.set_kind(discovery.kind());
    }
    if discovery.capabilities.any_list_changed() {
        transport.listen(&discovery.capabilities);
    }
    Ok(discovery)
}

fn set_era(transport: &dyn McpTransport, era: ProtocolEra) {
    if let Some(inbound) = transport.inbound() {
        inbound.set_era(era);
    }
}

/// What the probe's answer says about the server.
#[derive(Debug)]
enum Verdict {
    Modern {
        version: &'static str,
        result: Value,
    },
    /// A modern server that wants another version, which Caudra has.
    Retry {
        version: &'static str,
        error: McpError,
    },
    Legacy(String),
    Failed(McpError),
}

/// The discovery of a modern server, or `None` for one to initialize.
async fn select(
    transport: &dyn McpTransport,
    probe_timeout: Duration,
) -> Result<Option<ServerDiscovery>, McpError> {
    let server = &**transport.server_name();
    let mut version = LATEST_PROTOCOL_VERSION;
    let mut retried = false;
    loop {
        set_era(transport, ProtocolEra::Modern { version });
        match verdict(probe(transport, probe_timeout).await) {
            Verdict::Modern { version, result } => {
                set_era(transport, ProtocolEra::Modern { version });
                info!(server, version, "MCP server speaks the modern protocol");
                return Ok(Some(ServerDiscovery::from_discover(&result, version)));
            }
            Verdict::Retry {
                version: offered, ..
            } if !retried => {
                version = offered;
                retried = true;
            }
            Verdict::Retry { error, .. } | Verdict::Failed(error) => return Err(error),
            Verdict::Legacy(reason) => {
                debug!(server, reason, "MCP server falls back to initialize");
                return Ok(None);
            }
        }
    }
}

async fn probe(transport: &dyn McpTransport, timeout: Duration) -> Result<Value, McpError> {
    let scope = CallScope::none();
    future::or(
        transport.send_request(DISCOVER_METHOD, None, &scope),
        async {
            Timer::after(timeout).await;
            Err(McpError::Timeout {
                server: transport.server_name().to_string(),
                timeout_ms: timeout.as_millis() as u64,
            })
        },
    )
    .await
}

/// A discover result or a recognised modern error marks a modern server;
/// anything else, whatever its code, a legacy one. A modern server that
/// refuses Caudra outright, or wants credentials, fails the connection.
fn verdict(probe: Result<Value, McpError>) -> Verdict {
    match probe {
        Ok(result) if result[SUPPORTED_VERSIONS_KEY].is_array() => {
            match mutual(&result[SUPPORTED_VERSIONS_KEY]) {
                Some(version) => Verdict::Modern { version, result },
                None => Verdict::Legacy(NO_COMMON_VERSION.into()),
            }
        }
        Ok(_) => Verdict::Legacy(NOT_A_DISCOVER_RESULT.into()),
        Err(
            error @ McpError::RpcError {
                code: UNSUPPORTED_PROTOCOL_VERSION,
                ..
            },
        ) => {
            let McpError::RpcError { data, .. } = &error else {
                unreachable!()
            };
            match data
                .as_deref()
                .and_then(|data| mutual(&data[SUPPORTED_KEY]))
            {
                Some(version) => Verdict::Retry { version, error },
                None => Verdict::Legacy(error.to_string()),
            }
        }
        Err(
            error @ (McpError::RpcError {
                code: HEADER_MISMATCH | MISSING_REQUIRED_CLIENT_CAPABILITY,
                ..
            }
            | McpError::MissingCapability { .. }
            | McpError::OAuthRequired { .. }
            | McpError::OAuthFailed { .. }
            | McpError::Refused { .. }
            | McpError::HttpError {
                status: HTTP_UNAUTHORIZED,
                ..
            }),
        ) => Verdict::Failed(error),
        Err(error) => Verdict::Legacy(error.to_string()),
    }
}

/// The newest modern version both sides speak.
fn mutual(supported: &Value) -> Option<&'static str> {
    let supported = supported.as_array()?;
    MODERN_VERSIONS
        .iter()
        .copied()
        .find(|ours| supported.iter().any(|theirs| theirs.as_str() == Some(ours)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use test_case::test_case;

    use crate::cancel::CancelToken;
    use crate::mcp::call::{ElicitRequest, ElicitResponse, Elicitor};
    use crate::mcp::config::LocalExecutionPolicy;
    use crate::mcp::kind::McpServerKind;
    use crate::mcp::protocol::{
        INVALID_PARAMS, LISTEN_METHOD, META_KEY, METHOD_NOT_FOUND, PROMPTS_GET_METHOD,
        PROMPTS_LIST_METHOD, TOOLS_CALL_METHOD, TOOLS_LIST_METHOD,
    };
    use crate::mcp::stdio::StdioTransport;
    use crate::mcp::transport::BoxFuture;

    const SERVER: &str = "era";
    const PROBE_METHOD: &str = "probe";
    const DISCOVER_ENV: &str = "DISCOVER";
    const DISCOVER_AGAIN_ENV: &str = "DISCOVER_AGAIN";
    const CALL_TIMEOUT: Duration = Duration::from_secs(5);
    const SILENT_TIMEOUT: Duration = Duration::from_secs(1);
    const DISCOVER_RESULT: &str = r#""result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"home-assistant","version":"2026.10"}},"ttlMs":0,"cacheScope":"public"}"#;
    const UNSUPPORTED_WITH_OURS: &str = r#""error":{"code":-32022,"message":"Unsupported protocol version","data":{"supported":["2026-07-28"],"requested":"2026-07-28"}}"#;
    const UNSUPPORTED_WITHOUT_OURS: &str = r#""error":{"code":-32022,"message":"Unsupported protocol version","data":{"supported":["2025-11-25"],"requested":"2026-07-28"}}"#;
    const METHOD_NOT_FOUND_REPLY: &str =
        r#""error":{"code":-32601,"message":"Method not found: server/discover"}"#;
    /// Answers the probe with `$DISCOVER` (a second probe with
    /// `$DISCOVER_AGAIN` when set), stays silent when that is empty, takes
    /// `initialize` like Executor, and echoes anything else with how many
    /// probes this one process saw.
    const SELECTING_SERVER: &str = r#"discovers=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p')
  test -n "$id" || continue
  case "$line" in
    *'"method":"server/discover"'*)
      discovers=$((discovers + 1))
      reply=$DISCOVER
      if test "$discovers" -gt 1 && test -n "$DISCOVER_AGAIN"; then reply=$DISCOVER_AGAIN; fi
      if test -n "$reply"; then printf '{"jsonrpc":"2.0","id":%s,%s}\n' "$id" "$reply"; fi ;;
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"executor","version":"1.29"}}}\n' "$id" ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"echo":%s,"discovers":%s}}\n' "$id" "$line" "$discovers" ;;
  esac
done
"#;

    struct Silent;

    impl Elicitor for Silent {
        fn elicit<'a>(
            &'a self,
            _server: &'a str,
            _request: ElicitRequest,
        ) -> BoxFuture<'a, ElicitResponse> {
            Box::pin(future::pending())
        }
    }

    fn modern() -> ProtocolEra {
        ProtocolEra::Modern {
            version: LATEST_PROTOCOL_VERSION,
        }
    }

    #[test_case(TOOLS_CALL_METHOD ; "tools_call")]
    #[test_case(TOOLS_LIST_METHOD ; "tools_list")]
    #[test_case(PROMPTS_GET_METHOD ; "prompts_get")]
    #[test_case(PROMPTS_LIST_METHOD ; "prompts_list")]
    #[test_case(DISCOVER_METHOD ; "discover")]
    #[test_case(LISTEN_METHOD ; "listen")]
    fn every_modern_request_says_who_asks_and_what_it_can_do(method: &str) {
        let headless = modern().with_meta(None, &CallScope::none()).unwrap();
        let scope = CallScope::new(Some(Arc::new(Silent)), None, CancelToken::none());
        let asking = modern()
            .with_meta(Some(json!({"name": method})), &scope)
            .unwrap();
        for params in [&headless, &asking] {
            let meta = params[META_KEY].as_object().unwrap();
            assert_eq!(meta[PROTOCOL_VERSION_META], LATEST_PROTOCOL_VERSION);
            assert_eq!(meta[CLIENT_INFO_META], client_info());
            assert_eq!(meta.len(), 3, "never a logLevel: {meta:?}");
        }
        assert_eq!(headless[META_KEY][CLIENT_CAPABILITIES_META], json!({}));
        assert_eq!(
            asking[META_KEY][CLIENT_CAPABILITIES_META],
            json!({ELICITATION_KEY: {"form": {}, "url": {}}})
        );
        assert_eq!(asking["name"], method);
    }

    #[test]
    fn a_legacy_request_goes_out_as_given() {
        let params = Some(json!({"name": TOOLS_CALL_METHOD}));
        assert_eq!(
            ProtocolEra::default().with_meta(params.clone(), &CallScope::none()),
            params
        );
    }

    fn rpc(code: i64, data: Option<Value>) -> McpError {
        McpError::RpcError {
            server: SERVER.into(),
            code,
            message: SERVER.into(),
            data: data.map(Box::new),
        }
    }

    enum Expected {
        Modern,
        Retry,
        Legacy,
        Failed,
    }

    #[test_case(Ok(json!({SUPPORTED_VERSIONS_KEY: [LATEST_PROTOCOL_VERSION]})), Expected::Modern ; "discover_result")]
    #[test_case(Ok(json!({SUPPORTED_VERSIONS_KEY: [LEGACY_PROTOCOL_VERSION]})), Expected::Legacy ; "discover_result_without_ours")]
    #[test_case(Ok(json!({"tools": []})), Expected::Legacy ; "some_other_result")]
    #[test_case(Err(rpc(UNSUPPORTED_PROTOCOL_VERSION, Some(json!({SUPPORTED_KEY: [LATEST_PROTOCOL_VERSION]})))), Expected::Retry ; "unsupported_with_ours")]
    #[test_case(Err(rpc(UNSUPPORTED_PROTOCOL_VERSION, Some(json!({SUPPORTED_KEY: [LEGACY_PROTOCOL_VERSION]})))), Expected::Legacy ; "unsupported_without_ours")]
    #[test_case(Err(rpc(METHOD_NOT_FOUND, None)), Expected::Legacy ; "method_not_found")]
    #[test_case(Err(rpc(INVALID_PARAMS, None)), Expected::Legacy ; "invalid_params")]
    #[test_case(Err(McpError::Timeout { server: SERVER.into(), timeout_ms: 1 }), Expected::Legacy ; "timeout")]
    #[test_case(Err(McpError::HttpError { server: SERVER.into(), status: 400, reason: String::new() }), Expected::Legacy ; "http_400_without_a_modern_body")]
    #[test_case(Err(rpc(HEADER_MISMATCH, None)), Expected::Failed ; "header_mismatch")]
    #[test_case(Err(McpError::HttpError { server: SERVER.into(), status: HTTP_UNAUTHORIZED, reason: String::new() }), Expected::Failed ; "unauthorized")]
    #[test_case(Err(McpError::Refused { server: SERVER.into() }), Expected::Failed ; "refused_after_a_fresh_token")]
    fn the_probe_decides_the_era(probe: Result<Value, McpError>, expected: Expected) {
        let verdict = verdict(probe);
        let matched = match expected {
            Expected::Modern => matches!(verdict, Verdict::Modern { .. }),
            Expected::Retry => matches!(verdict, Verdict::Retry { .. }),
            Expected::Legacy => matches!(verdict, Verdict::Legacy(_)),
            Expected::Failed => matches!(verdict, Verdict::Failed(_)),
        };
        assert!(matched, "{verdict:?}");
    }

    fn selecting(discover: &str, again: &str, timeout: Duration) -> StdioTransport {
        StdioTransport::spawn(
            SERVER,
            "/bin/sh",
            &["-c".into(), SELECTING_SERVER.into()],
            &HashMap::from([
                (DISCOVER_ENV.into(), discover.into()),
                (DISCOVER_AGAIN_ENV.into(), again.into()),
            ]),
            timeout,
            LocalExecutionPolicy::Embedded,
            true,
        )
        .unwrap()
    }

    #[test_case(DISCOVER_RESULT, "", CALL_TIMEOUT, LATEST_PROTOCOL_VERSION, 1, Some(McpServerKind::HomeAssistant) ; "discover_ok")]
    #[test_case(UNSUPPORTED_WITH_OURS, DISCOVER_RESULT, CALL_TIMEOUT, LATEST_PROTOCOL_VERSION, 2, Some(McpServerKind::HomeAssistant) ; "unsupported_version_listing_ours_is_retried")]
    #[test_case(UNSUPPORTED_WITHOUT_OURS, "", CALL_TIMEOUT, LEGACY_PROTOCOL_VERSION, 1, Some(McpServerKind::Executor) ; "unsupported_version_without_ours_initializes")]
    #[test_case(METHOD_NOT_FOUND_REPLY, "", CALL_TIMEOUT, LEGACY_PROTOCOL_VERSION, 1, Some(McpServerKind::Executor) ; "method_not_found_then_initialize_like_executor")]
    #[test_case("", "", SILENT_TIMEOUT, LEGACY_PROTOCOL_VERSION, 1, Some(McpServerKind::Executor) ; "silence_then_initialize")]
    fn stdio_selects_the_era_on_one_process(
        discover: &str,
        again: &str,
        timeout: Duration,
        version: &str,
        discovers: u64,
        kind: Option<McpServerKind>,
    ) {
        let transport = selecting(discover, again, timeout);
        smol::block_on(async {
            let discovery = connect(&transport, timeout).await.unwrap();
            assert_eq!(discovery.protocol_version, version);
            assert_eq!(discovery.kind(), kind);
            let modern = version == LATEST_PROTOCOL_VERSION;
            assert_eq!(transport.era().modern().is_some(), modern);
            assert_eq!(transport.inbound().unwrap().kind(), kind);
            let echoed = transport
                .send_request(PROBE_METHOD, None, &CallScope::none())
                .await
                .unwrap();
            assert_eq!(echoed["discovers"], discovers);
            let meta = &echoed["echo"]["params"][META_KEY];
            assert_eq!(
                meta[PROTOCOL_VERSION_META].as_str(),
                modern.then_some(version)
            );
            transport.shutdown().await;
        });
    }
}
