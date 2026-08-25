use std::fmt;

pub mod http;
pub mod stdio;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportOutcome {
    PeerClosed,
    ShutdownCompleted,
    ShutdownForced,
    ShutdownTimedOut,
}

impl TransportOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PeerClosed => "peer_closed",
            Self::ShutdownCompleted => "shutdown_completed",
            Self::ShutdownForced => "shutdown_forced",
            Self::ShutdownTimedOut => "shutdown_timed_out",
        }
    }

    #[must_use]
    pub const fn requires_immediate_process_exit(self) -> bool {
        matches!(self, Self::ShutdownForced | Self::ShutdownTimedOut)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    Signal,
    StdioInitialization,
    StdioService,
    HttpBind,
    HttpService,
    HttpConfiguration,
    HttpAuthentication,
    HttpAuthenticationRequired,
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Signal => "shutdown signal handling could not be installed",
            Self::StdioInitialization => "the stdio MCP transport could not be initialized",
            Self::StdioService => "the stdio MCP transport stopped unexpectedly",
            Self::HttpBind => "the HTTP listener could not be opened",
            Self::HttpService => "the HTTP MCP transport stopped unexpectedly",
            Self::HttpConfiguration => "HTTP MCP composition is inconsistent",
            Self::HttpAuthentication => "the HTTP bearer token is invalid",
            Self::HttpAuthenticationRequired => "container HTTP bind requires a bearer token",
        })
    }
}

impl std::error::Error for TransportError {}

pub(crate) async fn shutdown_signal() -> Result<(), TransportError> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate()).map_err(|_| TransportError::Signal)?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(|_| TransportError::Signal),
            _ = terminate.recv() => Ok(()),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map_err(|_| TransportError::Signal)
    }
}

#[cfg(test)]
mod tests {
    use super::TransportOutcome;

    #[test]
    fn only_non_cooperative_shutdowns_require_immediate_process_exit() {
        assert!(!TransportOutcome::PeerClosed.requires_immediate_process_exit());
        assert!(!TransportOutcome::ShutdownCompleted.requires_immediate_process_exit());
        assert!(TransportOutcome::ShutdownForced.requires_immediate_process_exit());
        assert!(TransportOutcome::ShutdownTimedOut.requires_immediate_process_exit());
    }
}
