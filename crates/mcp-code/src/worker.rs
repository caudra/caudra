//! Worker binary resolution and pool construction.
//!
//! The `monty` worker is an external executable, like `bash` is for the shell tool. It is resolved
//! once at startup and never from tool input. Pool construction eagerly spawns a worker, so a
//! missing binary, an unreadable one, or a wire-protocol mismatch fails the server at startup
//! rather than on the first tool call.

use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use monty_pool::{Pool, PoolConfig};

/// Filename the pool spawns. Matches the `monty-runtime` binary target.
pub const WORKER_FILE_NAME: &str = "monty";

/// Two workers bound aggregate interpreter memory while still letting one call proceed while
/// another is mid-flight. Raising this multiplies the worst-case resident footprint.
pub(crate) const CODE_CONCURRENCY: usize = 2;
/// Waiting longer than this for a free worker is worse for the caller than a retryable error.
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(5);
/// Parent-side headroom over the per-call budget. The sandbox clock is polled, and some native
/// operations are not polled at all, so this deadline is the only hard bound on those paths.
const REQUEST_TIMEOUT_GRACE: Duration = Duration::from_secs(5);
/// Bounds the impact of any slow leak in a long-lived child.
const MAX_CHECKOUTS_PER_WORKER: u32 = 64;

/// Startup failure. Messages never include the resolved path: it is operator configuration and is
/// redacted everywhere else in the server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeBuildError {
    WorkerNotFound,
    WorkerNotExecutable,
    PoolUnavailable,
}

impl fmt::Display for CodeBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::WorkerNotFound => {
                "code execution worker was not found. Searched the path given by --code-worker or WORKCELL_MCP_CODE_WORKER, then beside the server executable, then PATH. From a source checkout run `make code-worker`; otherwise install the pinned `monty` binary beside the server or drop the `code` tool group"
            }
            Self::WorkerNotExecutable => {
                "code execution worker is not an executable regular file; verify the path and its permissions"
            }
            Self::PoolUnavailable => {
                "code execution worker could not be started; verify the binary matches the supported Monty version and can run on this host"
            }
        })
    }
}

impl std::error::Error for CodeBuildError {}

/// Resolves the worker binary without touching tool input.
///
/// Order: explicit operator configuration, then a binary shipped beside the server, then `PATH`.
/// `PATH` is last so a deliberate deployment always wins over an incidental one.
#[must_use]
pub fn resolve_worker(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return usable_executable(path).then(|| path.to_path_buf());
    }
    if let Some(adjacent) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(WORKER_FILE_NAME)))
        && usable_executable(&adjacent)
    {
        return Some(adjacent);
    }
    search_path()
}

/// A directory or a non-executable file would otherwise surface as an opaque spawn failure.
fn usable_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn search_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(WORKER_FILE_NAME))
        .find(|candidate| usable_executable(candidate))
}

/// Builds the pool, classifying the resolution failure so the caller can distinguish a
/// misconfiguration from a broken binary.
pub(crate) async fn build_pool(
    configured: Option<&Path>,
    max_timeout: Duration,
) -> Result<Pool, CodeBuildError> {
    let worker = match configured {
        Some(path) if !usable_executable(path) => {
            return Err(if std::fs::metadata(path).is_ok() {
                CodeBuildError::WorkerNotExecutable
            } else {
                CodeBuildError::WorkerNotFound
            });
        }
        _ => resolve_worker(configured).ok_or(CodeBuildError::WorkerNotFound)?,
    };

    let mut config = PoolConfig::subprocess(worker);
    // One prewarmed worker makes the first call fast and doubles as the startup handshake: a
    // version-skewed or unrunnable binary fails `Pool::new` here instead of on first use.
    config.min_processes = 1;
    config.max_processes = CODE_CONCURRENCY;
    config.checkout_timeout = Some(CHECKOUT_TIMEOUT);
    config.request_timeout = Some(max_timeout + REQUEST_TIMEOUT_GRACE);
    config.max_checkouts_per_worker = Some(MAX_CHECKOUTS_PER_WORKER);
    Pool::new(config)
        .await
        .map_err(|_| CodeBuildError::PoolUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_directories_and_missing_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!usable_executable(dir.path()));
        assert!(!usable_executable(&dir.path().join("absent")));
        assert!(resolve_worker(Some(dir.path())).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_executable_regular_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join(WORKER_FILE_NAME);
        std::fs::write(&file, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(!usable_executable(&file));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(usable_executable(&file));
        assert_eq!(resolve_worker(Some(&file)), Some(file));
    }

    #[test]
    fn build_errors_are_distinct_and_omit_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret = dir.path().join("private-worker-name");
        for error in [
            CodeBuildError::WorkerNotFound,
            CodeBuildError::WorkerNotExecutable,
            CodeBuildError::PoolUnavailable,
        ] {
            let rendered = error.to_string();
            assert!(!rendered.contains("private-worker-name"));
            assert!(!rendered.contains(secret.to_str().expect("utf-8 path")));
        }
    }
}
