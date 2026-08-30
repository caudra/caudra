//! Worker binary resolution and pool construction.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use monty_pool::{Pool, PoolConfig};
#[cfg(feature = "bundled-worker")]
use workcell_monty_worker::WorkerLease;

/// Filename the pool spawns. Matches the `monty-runtime` binary target.
pub const WORKER_FILE_NAME: &str = if cfg!(windows) { "monty.exe" } else { "monty" };

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

/// Operator-selected worker resolution policy.
#[derive(Clone, Copy)]
pub enum WorkerSource<'a> {
    /// Use only this external worker path.
    Path(&'a Path),
    /// Extract and use only the bundled worker.
    Bundled { cache_root: &'a Path },
    /// Search beside the current executable, then the configured bundle, then `PATH`.
    Discover {
        bundled_cache_root: Option<&'a Path>,
    },
}

impl fmt::Debug for WorkerSource<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(_) => formatter.write_str("Path(..)"),
            Self::Bundled { .. } => formatter.write_str("Bundled { cache_root: .. }"),
            Self::Discover {
                bundled_cache_root: Some(_),
            } => formatter.write_str("Discover { bundled_cache_root: Some(..) }"),
            Self::Discover {
                bundled_cache_root: None,
            } => formatter.write_str("Discover { bundled_cache_root: None }"),
        }
    }
}

/// Startup failure. Messages never include resolved or cache paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeBuildError {
    WorkerNotFound,
    WorkerNotExecutable,
    BundleUnavailable,
    BundleExtraction,
    PoolUnavailable,
}

impl fmt::Display for CodeBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::WorkerNotFound => {
                "code execution worker was not found; configure an explicit worker, install one beside the executable or on PATH, configure a bundled-worker cache root, or disable the code tool group"
            }
            Self::WorkerNotExecutable => {
                "code execution worker is not an executable regular file; verify the path and its permissions"
            }
            Self::BundleUnavailable => {
                "this build does not contain a bundled code execution worker"
            }
            Self::BundleExtraction => {
                "the bundled code execution worker could not be prepared in its cache"
            }
            Self::PoolUnavailable => {
                "code execution worker could not be started; verify the binary matches the supported Monty version and can run on this host"
            }
        })
    }
}

impl std::error::Error for CodeBuildError {}

#[must_use]
pub const fn bundled_worker_available() -> bool {
    #[cfg(feature = "bundled-worker")]
    {
        workcell_monty_worker::bundled_worker_available()
    }
    #[cfg(not(feature = "bundled-worker"))]
    {
        false
    }
}

pub(crate) struct PoolBuild {
    pub(crate) pool: Pool,
    #[cfg(feature = "bundled-worker")]
    pub(crate) worker_lease: Option<WorkerLease>,
}

struct ResolvedWorker {
    path: PathBuf,
    #[cfg(feature = "bundled-worker")]
    lease: Option<WorkerLease>,
}

impl ResolvedWorker {
    fn external(path: PathBuf) -> Self {
        Self {
            path,
            #[cfg(feature = "bundled-worker")]
            lease: None,
        }
    }
}

pub(crate) async fn build_pool(
    source: WorkerSource<'_>,
    max_timeout: Duration,
) -> Result<PoolBuild, CodeBuildError> {
    let resolved = resolve_worker(source)?;
    let mut config = PoolConfig::subprocess(resolved.path.clone());
    // One prewarmed worker makes the first call fast and catches spawn failures at startup.
    config.min_processes = 1;
    config.max_processes = CODE_CONCURRENCY;
    config.checkout_timeout = Some(CHECKOUT_TIMEOUT);
    config.request_timeout = Some(max_timeout + REQUEST_TIMEOUT_GRACE);
    config.max_checkouts_per_worker = Some(MAX_CHECKOUTS_PER_WORKER);
    let pool = Pool::new(config)
        .await
        .map_err(|_| CodeBuildError::PoolUnavailable)?;
    Ok(PoolBuild {
        pool,
        #[cfg(feature = "bundled-worker")]
        worker_lease: resolved.lease,
    })
}

fn resolve_worker(source: WorkerSource<'_>) -> Result<ResolvedWorker, CodeBuildError> {
    match source {
        WorkerSource::Path(path) => resolve_explicit(path),
        WorkerSource::Bundled { cache_root } => resolve_bundled(cache_root),
        WorkerSource::Discover { bundled_cache_root } => {
            if let Some(adjacent) = adjacent_worker() {
                return Ok(ResolvedWorker::external(adjacent));
            }
            if let Some(cache_root) = bundled_cache_root
                && bundled_worker_available()
            {
                return resolve_bundled(cache_root);
            }
            search_path()
                .map(ResolvedWorker::external)
                .ok_or(CodeBuildError::WorkerNotFound)
        }
    }
}

fn resolve_explicit(path: &Path) -> Result<ResolvedWorker, CodeBuildError> {
    if usable_executable(path) {
        return Ok(ResolvedWorker::external(path.to_path_buf()));
    }
    Err(if std::fs::metadata(path).is_ok() {
        CodeBuildError::WorkerNotExecutable
    } else {
        CodeBuildError::WorkerNotFound
    })
}

#[cfg(feature = "bundled-worker")]
fn resolve_bundled(cache_root: &Path) -> Result<ResolvedWorker, CodeBuildError> {
    if !bundled_worker_available() {
        return Err(CodeBuildError::BundleUnavailable);
    }
    let lease =
        workcell_monty_worker::extract(cache_root).map_err(|_| CodeBuildError::BundleExtraction)?;
    Ok(ResolvedWorker {
        path: lease.path().to_path_buf(),
        lease: Some(lease),
    })
}

#[cfg(not(feature = "bundled-worker"))]
fn resolve_bundled(cache_root: &Path) -> Result<ResolvedWorker, CodeBuildError> {
    let _ = cache_root;
    Err(CodeBuildError::BundleUnavailable)
}

fn adjacent_worker() -> Option<PathBuf> {
    let worker = std::env::current_exe()
        .ok()?
        .parent()?
        .join(WORKER_FILE_NAME);
    usable_executable(&worker).then_some(worker)
}

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
        .map(|directory| directory.join(WORKER_FILE_NAME))
        .find(|candidate| usable_executable(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_paths_are_authoritative() {
        let directory = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            resolve_worker(WorkerSource::Path(directory.path())),
            Err(CodeBuildError::WorkerNotExecutable)
        ));
        assert!(matches!(
            resolve_worker(WorkerSource::Path(&directory.path().join("absent"))),
            Err(CodeBuildError::WorkerNotFound)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_only_executable_regular_files() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let file = directory.path().join(WORKER_FILE_NAME);
        std::fs::write(&file, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(matches!(
            resolve_worker(WorkerSource::Path(&file)),
            Err(CodeBuildError::WorkerNotExecutable)
        ));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(
            resolve_worker(WorkerSource::Path(&file))
                .expect("worker")
                .path,
            file
        );
    }

    #[cfg(not(feature = "bundled-worker"))]
    #[test]
    fn bundled_source_reports_bundle_absence_without_the_feature() {
        let directory = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            resolve_worker(WorkerSource::Bundled {
                cache_root: directory.path()
            }),
            Err(CodeBuildError::BundleUnavailable)
        ));
        assert!(!bundled_worker_available());
    }

    #[cfg(feature = "bundled-worker")]
    #[test]
    fn bundled_source_reflects_the_compiled_artifact() {
        let directory = tempfile::tempdir().expect("tempdir");
        let resolved = resolve_worker(WorkerSource::Bundled {
            cache_root: directory.path(),
        });
        if bundled_worker_available() {
            let resolved = resolved.expect("bundled worker");
            assert!(usable_executable(&resolved.path));
            assert!(resolved.lease.is_some());
        } else {
            assert!(matches!(resolved, Err(CodeBuildError::BundleUnavailable)));
        }
    }

    #[test]
    fn worker_sources_and_build_errors_omit_paths() {
        let directory = tempfile::tempdir().expect("tempdir");
        let secret = directory.path().join("private-worker-name");
        let sources = [
            WorkerSource::Path(&secret),
            WorkerSource::Bundled {
                cache_root: &secret,
            },
            WorkerSource::Discover {
                bundled_cache_root: Some(&secret),
            },
        ];
        for source in sources {
            let rendered = format!("{source:?}");
            assert!(!rendered.contains("private-worker-name"));
            assert!(!rendered.contains(secret.to_str().expect("utf-8 path")));
        }
        for error in [
            CodeBuildError::WorkerNotFound,
            CodeBuildError::WorkerNotExecutable,
            CodeBuildError::BundleUnavailable,
            CodeBuildError::BundleExtraction,
            CodeBuildError::PoolUnavailable,
        ] {
            let rendered = error.to_string();
            assert!(!rendered.contains("private-worker-name"));
            assert!(!rendered.contains(secret.to_str().expect("utf-8 path")));
        }
    }
}
