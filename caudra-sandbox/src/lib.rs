mod client;
mod controller;
pub mod dto;
pub mod local_admin;
mod store;

pub use client::{LifecycleClient, generate_api_key};
pub use controller::{
    AttachTicket, Controller, CreateReview, Doctor, LifecycleAction, LiveSnapshot,
    NetworkReconcileStatus, RestartFailure, RestartPhase, ResumePolicy,
};
pub use store::{CreateIntent, InstanceRecord, LifecycleIntent, Ownership, RuntimeLease, Store};

use caudra_config::sandbox::{LeaseSeconds, SandboxError, persistence::SandboxStoreError};
use caudra_storage::{private_file::PrivateFileError, sandbox_auth::SandboxCredentialError};
use thiserror::Error;

const TEMPLATE_INCOMPATIBLE_HINT: &str = "; template compatibility check failed: requested or saved VM resources may exceed current operator limits, or network topology may not match; use sandbox doctor and inspect to compare resources, daemon limits, and topology before taking further action";

#[derive(Debug, Error)]
pub enum Error {
    #[error("the launch-pinned saved network is missing; restore it before attaching")]
    MissingNetwork,
    #[error("saved enforcement or TLS mode changes require explicit live policy review")]
    NetworkReview,
    #[error(
        "saved network configuration changed during policy application; reconcile again before attaching"
    )]
    NetworkChanged,
    #[error(transparent)]
    Transport(TransportDiagnostic),
    #[error("invalid or incompatible daemon response")]
    Protocol,
    #[error(
        "daemon refused the request: {code:?} (HTTP {status}, retryable={retryable}, outcome_unknown={outcome_unknown}){}",
        daemon_failure_hint(.code)
    )]
    Daemon {
        status: u16,
        code: dto::FailureCode,
        retryable: bool,
        outcome_unknown: bool,
    },
    #[error(
        "{refusal}; failed to clear the definitively refused lifecycle intent locally: {cleanup}; refresh the durable record before further action"
    )]
    LifecycleRefusalCleanup {
        #[source]
        refusal: Box<Error>,
        cleanup: Box<Error>,
    },
    #[error("sandbox authority, owner, execution, or immutable launch identity does not match")]
    Identity,
    #[error("reviewed sandbox revision changed; refresh and review again (nothing was sent)")]
    ReviewChanged,
    #[error("sandbox name already reserved; inspect its existing operation, never create again")]
    Exists,
    #[error("sandbox is not saved; use an explicit create or attach command")]
    Missing,
    #[error(
        "sandbox lifecycle postconditions are unresolved; inspect or explicitly acknowledge failure; never replay an unknown request"
    )]
    Unresolved,
    #[error("sandbox is paused; explicitly resume it or select --sandbox-resume")]
    ResumeRequired,
    #[error("sandbox is not running and ready for Workcell attachment")]
    NotReady,
    #[error("pause requires a persistent disk; nothing was sent or reserved")]
    PauseUnsupported,
    #[error("borrowed sandboxes detach by default; destructive control requires explicit approval")]
    Borrowed,
    #[error("sandbox has a local runtime, session writer, active workflow, or unresolved mutation")]
    Busy,
    #[error("invalid sandbox lifecycle store")]
    Store,
    #[error(
        "sandbox API key is missing or too short; use auth sandbox generate or set (at least 32 bytes)"
    )]
    Credential,
    #[error("lease must be within daemon limits, and renewal must not shorten it")]
    Lease,
    #[error(
        "requested lease ({requested}) exceeds the daemon's cap ({max}); raise E2B_LOCAL_MAX_TIMEOUT, or set it to 0 to allow leases with no expiry (nothing was sent)"
    )]
    LeaseOverCap {
        requested: LeaseSeconds,
        max: LeaseSeconds,
    },
    #[error(
        "this lease has no expiry, and Extend never shortens a lease; Pause and Resume, or Restart, with the finite lease you want (nothing was sent)"
    )]
    LeaseNoExpiry,
    #[error(
        "approved explicit local provider action required; no daemon is installed or started automatically"
    )]
    LocalApproval,
    #[error("local template helper failed or returned an invalid bounded response")]
    LocalHelper,
    #[error("invalid image input: {0}")]
    ImageInput(&'static str),
    #[error(
        "TLS policy requires a discovered mode and reviewed guest CA compatibility; live mode changes also require provider support and ready CA state"
    )]
    TlsPolicy,
    #[error(transparent)]
    Configuration(#[from] SandboxError),
    #[error(transparent)]
    Profiles(#[from] SandboxStoreError),
    #[error(transparent)]
    PrivateFile(#[from] PrivateFileError),
    #[error(transparent)]
    Auth(#[from] SandboxCredentialError),
}

pub type Result<T> = std::result::Result<T, Error>;

fn daemon_failure_hint(code: &dto::FailureCode) -> &'static str {
    match code {
        dto::FailureCode::TemplateIncompatible => TEMPLATE_INCOMPATIBLE_HINT,
        _ => "",
    }
}

#[derive(Debug, Error)]
#[error("sandbox {operation} transport failed: {failure}; {guidance}")]
pub struct TransportDiagnostic {
    pub(crate) operation: &'static str,
    pub(crate) failure: &'static str,
    pub(crate) guidance: &'static str,
}

#[cfg(test)]
mod tests {
    use super::{Error, TEMPLATE_INCOMPATIBLE_HINT};
    use crate::dto::FailureCode;
    use test_case::test_case;

    const CONFLICT_STATUS: u16 = 409;

    #[test_case(FailureCode::TemplateIncompatible, TEMPLATE_INCOMPATIBLE_HINT; "template_compatibility_hint")]
    #[test_case(FailureCode::StateConflict, ""; "state_conflict_unchanged")]
    #[test_case(FailureCode::TemplateRevisionMismatch, ""; "revision_mismatch_unchanged")]
    #[test_case(FailureCode::Unknown, ""; "unknown_unchanged")]
    fn daemon_failure_format_preserves_metadata(code: FailureCode, hint: &str) {
        for (retryable, outcome_unknown) in [(false, false), (true, true)] {
            let expected = format!(
                "daemon refused the request: {code:?} (HTTP {CONFLICT_STATUS}, retryable={retryable}, outcome_unknown={outcome_unknown}){hint}"
            );
            let error = Error::Daemon {
                status: CONFLICT_STATUS,
                code: code.clone(),
                retryable,
                outcome_unknown,
            };
            assert_eq!(error.to_string(), expected);
        }
    }
}
