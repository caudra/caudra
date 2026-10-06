use thiserror::Error;

use crate::{DnsError, MAX_REQUEST_BODY_BYTES, TransportError, UrlPolicyError};

/// A bounded fetch failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NetError {
    /// URL policy rejected an initial or redirect target.
    #[error(transparent)]
    Policy(#[from] UrlPolicyError),
    /// The request body exceeds [`MAX_REQUEST_BODY_BYTES`]. Nothing was sent.
    #[error(
        "request body of {length} bytes exceeds the {limit}-byte limit",
        limit = MAX_REQUEST_BODY_BYTES
    )]
    RequestBodyTooLarge {
        /// Length of the rejected body.
        length: usize,
    },
    /// A caller header is one the client derives itself. Nothing was sent.
    #[error("request header {name} is reserved for the HTTP client")]
    ReservedHeader {
        /// Lowercase name of the rejected header. Its value is never kept.
        name: String,
    },
    /// DNS failed or returned no usable addresses.
    #[error(transparent)]
    Dns(#[from] DnsError),
    /// DNS returned no addresses.
    #[error("DNS returned no addresses for {0}")]
    EmptyDnsAnswer(String),
    /// The one-hop transport failed before the operation deadline, even if it
    /// called the failure a timeout, as an OS giving up on a connect does.
    #[error(transparent)]
    Transport(TransportError),
    /// The configured outbound proxy refused or could not complete the hop.
    #[error("outbound proxy request failed: {0}")]
    Proxy(String),
    /// A redirect omitted `Location` or exceeded the configured hop count.
    #[error("invalid redirect: {0}")]
    Redirect(String),
    /// The total operation deadline elapsed. A DNS, transport, or proxy failure
    /// that surfaces once it has is reported as this, whatever it was called.
    #[error("network operation timed out")]
    Timeout,
    /// The caller cancelled the operation.
    #[error("network operation was cancelled")]
    Cancelled,
}

/// A proxy refusal is a policy decision, so it is reported as its own variant
/// rather than an anonymous transport failure the caller would retry.
impl From<TransportError> for NetError {
    fn from(error: TransportError) -> Self {
        if error.is_proxy() {
            return Self::Proxy(error.to_string());
        }
        Self::Transport(error)
    }
}

impl NetError {
    pub(crate) fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Dns(_) | Self::EmptyDnsAnswer(_) | Self::Transport(_)
        )
    }
}
