use thiserror::Error;

use crate::{DnsError, TransportError, UrlPolicyError};

/// A bounded fetch failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NetError {
    /// URL policy rejected an initial or redirect target.
    #[error(transparent)]
    Policy(#[from] UrlPolicyError),
    /// DNS failed or returned no usable addresses.
    #[error(transparent)]
    Dns(#[from] DnsError),
    /// DNS returned no addresses.
    #[error("DNS returned no addresses for {0}")]
    EmptyDnsAnswer(String),
    /// The one-hop transport failed.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// A redirect omitted `Location` or exceeded the configured hop count.
    #[error("invalid redirect: {0}")]
    Redirect(String),
    /// The total operation deadline elapsed.
    #[error("network operation timed out")]
    Timeout,
    /// The caller cancelled the operation.
    #[error("network operation was cancelled")]
    Cancelled,
}

impl NetError {
    pub(crate) fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Dns(_) | Self::EmptyDnsAnswer(_) | Self::Transport(_)
        )
    }
}
