//! `subscriptions/listen`: the standing request a modern server pushes list
//! changes on. Whenever it ends, gracefully or not, it is opened again after
//! a backoff; quick ends in a row eventually give up.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use smol::Timer;
use tracing::{debug, warn};

use super::error::McpError;
use super::inbound::NOTIFICATIONS_KEY;
use super::transport::BoxFuture;

/// First wait before reopening a stream that ended, doubled per quick end in
/// a row up to [`MAX_LISTEN_BACKOFF`].
pub const LISTEN_BACKOFF: Duration = Duration::from_secs(1);
pub const MAX_LISTEN_BACKOFF: Duration = Duration::from_secs(30);
pub const MAX_LISTEN_FAILURES: u32 = 8;
const MAX_BACKOFF_DOUBLINGS: u32 = 5;

pub fn backoff(failures: u32) -> Duration {
    LISTEN_BACKOFF
        .saturating_mul(1 << failures.min(MAX_BACKOFF_DOUBLINGS))
        .min(MAX_LISTEN_BACKOFF)
}

/// Keeps a subscription asking for `filter` open for as long as the task
/// runs, `send` opening each one. A subscription that outlived the longest
/// backoff was healthy, so only quick ends count toward giving up. Dropping
/// the task cancels the open one.
pub(super) async fn subscribe(
    server: Arc<str>,
    filter: Value,
    send: impl Fn(Value) -> BoxFuture<'static, Result<Value, McpError>>,
) {
    let mut failures = 0;
    loop {
        let opened = Instant::now();
        match send(json!({NOTIFICATIONS_KEY: filter})).await {
            Ok(_) => debug!(server = %server, "MCP subscription ended"),
            Err(McpError::ServerDied { .. }) => return,
            Err(error) => debug!(server = %server, %error, "MCP subscription failed"),
        }
        if opened.elapsed() >= MAX_LISTEN_BACKOFF {
            failures = 0;
        }
        if failures == MAX_LISTEN_FAILURES {
            warn!(server = %server, failures, "MCP subscription gave up");
            return;
        }
        Timer::after(backoff(failures)).await;
        failures += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    use test_case::test_case;

    use crate::mcp::config::LocalExecutionPolicy;
    use crate::mcp::era::connect;
    use crate::mcp::inbound::ListKind;
    use crate::mcp::stdio::StdioTransport;
    use crate::mcp::transport::McpTransport;

    const SERVER: &str = "listening";
    const END_ENV: &str = "END";
    const GRACEFUL_END: &str = "complete";
    const SERVER_CANCEL: &str = "cancelled";
    /// Covers the backoff before the subscription is opened again.
    const CLIENT_WAIT: Duration = Duration::from_secs(5);
    /// A modern server with a tools list that changes. Each subscription is
    /// acknowledged, hears a prompts change meant for a subscription that
    /// does not exist, then a tools change of its own; the first one then
    /// ends as `$END` says.
    const LISTENING_SERVER: &str = r#"listens=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"server/discover"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":true},"prompts":{}},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"listening","version":"1"}}}}\n' "$id" ;;
    *'"method":"subscriptions/listen"'*)
      listens=$((listens + 1))
      printf '{"jsonrpc":"2.0","method":"notifications/subscriptions/acknowledged","params":{"_meta":{"io.modelcontextprotocol/subscriptionId":%s},"notifications":{"toolsListChanged":true}}}\n' "$id"
      printf '{"jsonrpc":"2.0","method":"notifications/prompts/list_changed","params":{"_meta":{"io.modelcontextprotocol/subscriptionId":999}}}\n'
      printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{"_meta":{"io.modelcontextprotocol/subscriptionId":%s}}}\n' "$id"
      if test "$listens" -eq 1; then
        case "$END" in
          complete) printf '{"jsonrpc":"2.0","id":%s,"result":{"resultType":"complete","_meta":{"io.modelcontextprotocol/subscriptionId":%s}}}\n' "$id" "$id" ;;
          cancelled) printf '{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":%s}}\n' "$id" ;;
        esac
      fi ;;
  esac
done
"#;

    #[test_case(GRACEFUL_END ; "graceful_end")]
    #[test_case(SERVER_CANCEL ; "server_cancelled")]
    fn a_subscription_hears_its_own_changes_and_reopens_after_it_ends(end: &str) {
        let transport = StdioTransport::spawn(
            SERVER,
            "/bin/sh",
            &["-c".into(), LISTENING_SERVER.into()],
            &HashMap::from([(END_ENV.into(), end.into())]),
            CLIENT_WAIT,
            LocalExecutionPolicy::Embedded,
            true,
        )
        .unwrap();
        smol::block_on(connect(&transport, CLIENT_WAIT)).unwrap();
        let inbound = transport.inbound().unwrap();
        let changes = inbound.list_changes();
        assert_eq!(changes.recv_timeout(CLIENT_WAIT), Ok(ListKind::Tools));
        assert_eq!(changes.recv_timeout(CLIENT_WAIT), Ok(ListKind::Tools));
        assert_eq!(inbound.granted(), HashSet::from([ListKind::Tools]));
        smol::block_on(transport.shutdown());
    }

    #[test_case(0, LISTEN_BACKOFF ; "starts_at_one_second")]
    #[test_case(1, Duration::from_secs(2) ; "doubles")]
    #[test_case(MAX_LISTEN_FAILURES, MAX_LISTEN_BACKOFF ; "capped")]
    fn listen_backoff_doubles_up_to_the_cap(failures: u32, expected: Duration) {
        assert_eq!(backoff(failures), expected);
    }
}
