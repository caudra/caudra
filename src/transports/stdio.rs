use std::time::Duration;

use rmcp::{ServiceExt, service::QuitReason};

use super::{TransportError, TransportOutcome, shutdown_signal};
use crate::server::WorkcellServer;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run(server: WorkcellServer) -> Result<TransportOutcome, TransportError> {
    // rmcp::transport::stdio is the official newline-delimited transport. No
    // banner or logger may write stdout after this point; logs remain stderr.
    let starting = server.serve(rmcp::transport::stdio());
    tokio::pin!(starting);
    let running = tokio::select! {
        result = &mut starting => result.map_err(|_| TransportError::StdioInitialization)?,
        signal = shutdown_signal() => {
            signal?;
            return Ok(TransportOutcome::ShutdownCompleted);
        }
    };
    tracing::info!(
        operation = "mcp.started",
        transport = "stdio",
        "MCP server connected"
    );

    let cancellation = running.cancellation_token();
    let waiting = running.waiting();
    tokio::pin!(waiting);
    tokio::select! {
        result = &mut waiting => {
            ensure_clean_quit(result.map_err(|_| TransportError::StdioService)?)?;
            Ok(TransportOutcome::PeerClosed)
        }
        signal = shutdown_signal() => {
            signal?;
            tracing::info!(operation = "mcp.shutdown.started", "MCP server shutting down");
            cancellation.cancel();
            // The first signal requests normal rmcp cleanup. A second signal or
            // five seconds without completion is an explicit forced outcome.
            tokio::select! {
                result = &mut waiting => {
                    ensure_clean_quit(result.map_err(|_| TransportError::StdioService)?)?;
                    Ok(TransportOutcome::ShutdownCompleted)
                }
                second = shutdown_signal() => {
                    second?;
                    Ok(TransportOutcome::ShutdownForced)
                }
                () = tokio::time::sleep(SHUTDOWN_TIMEOUT) => {
                    Ok(TransportOutcome::ShutdownTimedOut)
                }
            }
        }
    }
}

fn ensure_clean_quit(reason: QuitReason) -> Result<(), TransportError> {
    match reason {
        QuitReason::JoinError(_) => Err(TransportError::StdioService),
        _ => Ok(()),
    }
}
