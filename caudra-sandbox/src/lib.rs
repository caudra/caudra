mod client;
mod controller;
pub mod dto;
pub mod local_admin;
mod store;

pub use client::{LifecycleClient, generate_api_key};
pub use controller::{
    AttachTicket, Controller, CreateReview, Doctor, LifecycleAction, LiveSnapshot, ResumePolicy,
};
pub use store::{CreateIntent, InstanceRecord, LifecycleIntent, Ownership, RuntimeLease, Store};

use caudra_config::sandbox::{SandboxError, persistence::SandboxStoreError};
use caudra_storage::{private_file::PrivateFileError, sandbox_auth::SandboxCredentialError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("sandbox transport failed; outcome may be unknown; inspect instead of replaying")]
    Transport,
    #[error("invalid or incompatible daemon response")]
    Protocol,
    #[error(
        "daemon refused the request: {code:?} (HTTP {status}, retryable={retryable}, outcome_unknown={outcome_unknown})"
    )]
    Daemon {
        status: u16,
        code: dto::FailureCode,
        retryable: bool,
        outcome_unknown: bool,
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
    #[error("lease must be positive, within daemon limits, and renewal must not shorten it")]
    Lease,
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
