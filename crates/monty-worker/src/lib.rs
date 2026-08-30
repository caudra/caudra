#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write as _};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use std::time::Duration;

use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

const DIRECTORY_MODE: u32 = 0o700;
const LOCK_MODE: u32 = 0o600;
const WORKER_MODE: u32 = 0o700;
const EXTRACTION_LOCK: &str = ".extract.lock";
const USE_LOCK: &str = ".use.lock";
#[cfg(windows)]
const RENAME_ATTEMPTS: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("this build does not contain a bundled Monty worker")]
    BundleUnavailable,
    #[error("the bundled Monty worker failed its build-time digest check")]
    BundledDigest,
    #[error("the cached Monty worker target is not a regular file or directory")]
    UnsafeCacheTarget,
    #[error("prepare the cached Monty worker: {0}")]
    Io(#[from] io::Error),
}

pub struct WorkerLease {
    path: PathBuf,
    _use_lock: File,
}

impl WorkerLease {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Clone, Copy)]
struct WorkerArtifact<'a> {
    bytes: &'a [u8],
    digest: &'a str,
    target: &'a str,
    version: &'a str,
    file_name: &'a str,
}

#[must_use]
pub const fn bundled_worker_available() -> bool {
    cfg!(workcell_bundled_monty_worker)
}

pub fn extract(cache_root: &Path) -> Result<WorkerLease, WorkerError> {
    let cache_root = prepare_cache_root(cache_root)?;
    extract_at(&cache_root, bundled()?)
}

#[cfg(workcell_bundled_monty_worker)]
fn bundled() -> Result<WorkerArtifact<'static>, WorkerError> {
    Ok(WorkerArtifact {
        bytes: include_bytes!(concat!(env!("OUT_DIR"), "/bundled-monty-worker")),
        digest: env!("WORKCELL_MONTY_WORKER_SHA256"),
        target: env!("WORKCELL_MONTY_WORKER_TARGET"),
        version: env!("WORKCELL_MONTY_WORKER_VERSION"),
        file_name: env!("WORKCELL_MONTY_WORKER_FILE_NAME"),
    })
}

#[cfg(not(workcell_bundled_monty_worker))]
fn bundled() -> Result<WorkerArtifact<'static>, WorkerError> {
    Err(WorkerError::BundleUnavailable)
}

fn extract_at(cache_root: &Path, artifact: WorkerArtifact<'_>) -> Result<WorkerLease, WorkerError> {
    if sha256_bytes(artifact.bytes) != artifact.digest {
        return Err(WorkerError::BundledDigest);
    }

    let worker_root = private_subdir(cache_root, "workers")?;
    let monty_root = private_subdir(&worker_root, "monty")?;
    let version_root = private_subdir(&monty_root, artifact.version)?;
    let target_root = private_subdir(&version_root, artifact.target)?;
    let extraction_lock = exclusive_lock(&target_root.join(EXTRACTION_LOCK))?;
    let digest_root = private_subdir(&target_root, artifact.digest)?;
    let path = digest_root.join(artifact.file_name);

    match cached_worker_state(&path, artifact.digest)? {
        CachedWorker::Valid => set_worker_permissions(&path)?,
        CachedWorker::Missing => {
            atomic_write_permissions(&path, artifact.bytes, WORKER_MODE)?;
            if cached_worker_state(&path, artifact.digest)? != CachedWorker::Valid {
                return Err(WorkerError::BundledDigest);
            }
        }
        CachedWorker::Corrupt => {
            fs::remove_file(&path)?;
            atomic_write_permissions(&path, artifact.bytes, WORKER_MODE)?;
            if cached_worker_state(&path, artifact.digest)? != CachedWorker::Valid {
                return Err(WorkerError::BundledDigest);
            }
        }
    }

    let use_lock = shared_lock(&digest_root.join(USE_LOCK))?;
    cleanup_obsolete_digests(&target_root, artifact.digest);
    drop(extraction_lock);

    Ok(WorkerLease {
        path,
        _use_lock: use_lock,
    })
}

fn prepare_cache_root(path: &Path) -> Result<PathBuf, WorkerError> {
    if let Err(error) = fs::create_dir_all(path)
        && error.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(error.into());
    }
    let path = fs::canonicalize(path)?;
    if !path.is_dir() {
        return Err(WorkerError::UnsafeCacheTarget);
    }
    set_directory_permissions(&path)?;
    Ok(path)
}

fn private_subdir(parent: &Path, name: &str) -> Result<PathBuf, WorkerError> {
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(WorkerError::UnsafeCacheTarget);
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(DIRECTORY_MODE);
            if let Err(error) = builder.create(&path)
                && error.kind() != io::ErrorKind::AlreadyExists
            {
                return Err(error.into());
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(WorkerError::UnsafeCacheTarget);
            }
        }
        Err(error) => return Err(error.into()),
    }
    set_directory_permissions(&path)?;
    Ok(path)
}

fn exclusive_lock(path: &Path) -> Result<File, WorkerError> {
    let file = open_lock(path)?;
    file.lock()?;
    Ok(file)
}

fn shared_lock(path: &Path) -> Result<File, WorkerError> {
    let file = open_lock(path)?;
    file.lock_shared()?;
    Ok(file)
}

fn open_lock(path: &Path) -> Result<File, WorkerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(WorkerError::UnsafeCacheTarget);
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    options.mode(LOCK_MODE).custom_flags(libc::O_NOFOLLOW);
    #[cfg(not(unix))]
    let _ = LOCK_MODE;
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(WorkerError::UnsafeCacheTarget);
    }
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(LOCK_MODE))?;
    Ok(file)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CachedWorker {
    Missing,
    Valid,
    Corrupt,
}

fn cached_worker_state(path: &Path, expected_digest: &str) -> Result<CachedWorker, WorkerError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(CachedWorker::Missing);
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(WorkerError::UnsafeCacheTarget);
    }
    if sha256_file(path)? == expected_digest {
        Ok(CachedWorker::Valid)
    } else {
        Ok(CachedWorker::Corrupt)
    }
}

fn atomic_write_permissions(path: &Path, data: &[u8], mode: u32) -> Result<(), io::Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(data)?;
    #[cfg(unix)]
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    #[cfg(not(unix))]
    let _ = mode;
    temporary.as_file().sync_all()?;
    let (file, temporary_path) = temporary.into_parts();
    drop(file);
    retry_rename(&temporary_path, path)?;
    sync_directory(parent);
    Ok(())
}

#[cfg(windows)]
fn retry_rename(source: &Path, destination: &Path) -> Result<(), io::Error> {
    let mut previous = 0_u64;
    let mut delay = 1_u64;
    for _ in 0..RENAME_ATTEMPTS {
        match fs::rename(source, destination) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                thread::sleep(Duration::from_millis(delay));
                let next = previous.saturating_add(delay);
                previous = delay;
                delay = next;
            }
            Err(error) => return Err(error),
        }
    }
    fs::rename(source, destination)
}

#[cfg(not(windows))]
fn retry_rename(source: &Path, destination: &Path) -> Result<(), io::Error> {
    fs::rename(source, destination)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    encode_digest(Sha256::digest(bytes).as_slice())
}

fn sha256_file(path: &Path) -> Result<String, io::Error> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(encode_digest(digest.finalize().as_slice()))
}

fn encode_digest(digest: &[u8]) -> String {
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing to a string cannot fail");
    }
    encoded
}

fn cleanup_obsolete_digests(target_root: &Path, active_digest: &str) {
    let Ok(entries) = fs::read_dir(target_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == active_digest
            || name.len() != 64
            || !name.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let Ok(lock) = open_lock(&entry.path().join(USE_LOCK)) else {
            continue;
        };
        if lock.try_lock().is_ok() {
            drop(lock);
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn set_directory_permissions(path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(DIRECTORY_MODE))?;
    #[cfg(not(unix))]
    let _ = (path, DIRECTORY_MODE);
    Ok(())
}

fn set_worker_permissions(path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(WORKER_MODE))?;
    #[cfg(not(unix))]
    let _ = (path, WORKER_MODE);
    Ok(())
}

fn sync_directory(path: &Path) {
    #[cfg(unix)]
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    const FIRST_WORKER: &[u8] = b"first worker";
    const FIRST_WORKER_DIGEST: &str =
        "73c0556df42d22ad77f5559ca962d6f9875cc138b94ecd03ba84f7cef3e03d56";
    const SECOND_WORKER: &[u8] = b"second worker";
    const SECOND_WORKER_DIGEST: &str =
        "cb712affff723bba2023f25a118505d51e5ce337c7259758c1a3125bdfd03adc";

    fn artifact<'a>(bytes: &'a [u8], digest: &'a str) -> WorkerArtifact<'a> {
        WorkerArtifact {
            bytes,
            digest,
            target: "test-target",
            version: "test-version",
            file_name: "monty",
        }
    }

    #[test]
    fn extracts_and_repairs_a_content_addressed_worker() {
        let cache = tempfile::tempdir().expect("tempdir");
        let artifact = artifact(FIRST_WORKER, FIRST_WORKER_DIGEST);
        let first = extract_at(cache.path(), artifact).expect("first extraction");
        assert_eq!(fs::read(first.path()).expect("read worker"), FIRST_WORKER);

        fs::write(first.path(), b"corrupt").expect("corrupt worker");
        drop(first);
        let repaired = extract_at(cache.path(), artifact).expect("repair extraction");
        assert_eq!(
            fs::read(repaired.path()).expect("read repaired worker"),
            FIRST_WORKER
        );

        #[cfg(unix)]
        assert_eq!(
            fs::metadata(repaired.path())
                .expect("worker metadata")
                .permissions()
                .mode()
                & 0o777,
            WORKER_MODE
        );
    }

    #[test]
    fn prepares_a_missing_cache_root() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let cache = temporary.path().join("missing/cache");

        let prepared = prepare_cache_root(&cache).expect("cache root");

        assert_eq!(prepared, fs::canonicalize(&cache).expect("canonical cache"));
        assert!(prepared.is_dir());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(prepared)
                .expect("cache metadata")
                .permissions()
                .mode()
                & 0o777,
            DIRECTORY_MODE
        );
    }

    #[cfg(unix)]
    #[test]
    fn accepts_an_operator_selected_symlinked_cache_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("tempdir");
        let destination = temporary.path().join("destination");
        fs::create_dir(&destination).expect("cache destination");
        let cache = temporary.path().join("cache");
        symlink(&destination, &cache).expect("cache symlink");

        let prepared = prepare_cache_root(&cache).expect("cache root");

        assert_eq!(
            prepared,
            fs::canonicalize(destination).expect("canonical cache")
        );
    }

    #[test]
    fn concurrent_extraction_produces_one_intact_worker() {
        let cache = Arc::new(tempfile::tempdir().expect("tempdir"));
        let artifact = artifact(FIRST_WORKER, FIRST_WORKER_DIGEST);
        let threads = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                std::thread::spawn(move || extract_at(cache.path(), artifact).expect("extraction"))
            })
            .collect::<Vec<_>>();
        let leases = threads
            .into_iter()
            .map(|thread| thread.join().expect("extraction thread"))
            .collect::<Vec<_>>();

        assert!(
            leases
                .windows(2)
                .all(|pair| pair[0].path() == pair[1].path())
        );
        assert_eq!(
            fs::read(leases[0].path()).expect("read worker"),
            FIRST_WORKER
        );
    }

    #[test]
    fn obsolete_digest_is_removed_only_after_its_lease_is_released() {
        let cache = tempfile::tempdir().expect("tempdir");
        let old = extract_at(cache.path(), artifact(FIRST_WORKER, FIRST_WORKER_DIGEST))
            .expect("old extraction");
        let old_path = old.path().to_path_buf();

        let current = extract_at(cache.path(), artifact(SECOND_WORKER, SECOND_WORKER_DIGEST))
            .expect("current extraction");
        assert!(old_path.exists());
        drop(old);
        drop(current);

        let current = extract_at(cache.path(), artifact(SECOND_WORKER, SECOND_WORKER_DIGEST))
            .expect("current extraction");
        assert!(!old_path.exists());
        assert_eq!(
            fs::read(current.path()).expect("read current worker"),
            SECOND_WORKER
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_at_the_worker_target() {
        use std::os::unix::fs::symlink;

        let cache = tempfile::tempdir().expect("tempdir");
        let artifact = artifact(FIRST_WORKER, FIRST_WORKER_DIGEST);
        let target_root = cache
            .path()
            .join("workers/monty/test-version/test-target")
            .join(artifact.digest);
        fs::create_dir_all(&target_root).expect("target root");
        let destination = cache.path().join("destination");
        fs::write(&destination, FIRST_WORKER).expect("destination");
        symlink(destination, target_root.join("monty")).expect("worker symlink");

        assert!(matches!(
            extract_at(cache.path(), artifact),
            Err(WorkerError::UnsafeCacheTarget)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_at_the_extraction_lock() {
        use std::os::unix::fs::symlink;

        let cache = tempfile::tempdir().expect("tempdir");
        let artifact = artifact(FIRST_WORKER, FIRST_WORKER_DIGEST);
        let target_root = cache.path().join("workers/monty/test-version/test-target");
        fs::create_dir_all(&target_root).expect("target root");
        let destination = cache.path().join("destination");
        fs::write(&destination, b"lock target").expect("destination");
        symlink(destination, target_root.join(EXTRACTION_LOCK)).expect("lock symlink");

        assert!(matches!(
            extract_at(cache.path(), artifact),
            Err(WorkerError::UnsafeCacheTarget)
        ));
    }
}
