//! What this process last wrote to each config file, so a watcher can tell
//! its own saves from an edit made elsewhere.

use std::collections::HashMap;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

static WRITES: LazyLock<Mutex<HashMap<PathBuf, u64>>> = LazyLock::new(Mutex::default);

/// Call after `contents` reached `path`, so the file exists to be resolved.
pub fn record(path: &Path, contents: &[u8]) {
    let (key, hash) = (key(path), fingerprint(contents));
    writes().insert(key, hash);
}

/// Whether `contents` is what this process last recorded for `path`. The
/// record stays, so every event one save raises is recognized.
pub fn is_own(path: &Path, contents: &[u8]) -> bool {
    let (key, hash) = (key(path), fingerprint(contents));
    writes().get(&key) == Some(&hash)
}

/// Stable within a process, which is as long as a record lives.
pub fn fingerprint(contents: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(contents);
    hasher.finish()
}

fn writes() -> MutexGuard<'static, HashMap<PathBuf, u64>> {
    WRITES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Resolved, so every spelling of one file shares a record.
fn key(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::{is_own, record};

    const FILE: &str = "mcp.toml";
    #[cfg(unix)]
    const LINK: &str = "linked";
    const FIRST: &str = "[mcp.a]\nenabled = true\n";
    const SECOND: &str = "[mcp.a]\nenabled = false\n";
    /// One save raises several events, and each must be recognized.
    const EVENTS_PER_SAVE: usize = 2;

    const NOT_RECOGNIZED: &str = "a recorded write was not recognized as this process's own";
    const WRONGLY_OWNED: &str = "contents this process did not last write counted as its own";

    fn written(dir: &TempDir, contents: &str) -> PathBuf {
        let path = dir.path().join(FILE);
        fs::write(&path, contents).unwrap();
        path
    }

    #[test_case(FIRST, true, NOT_RECOGNIZED; "recorded_contents_are_own")]
    #[test_case(SECOND, false, WRONGLY_OWNED; "different_contents_are_not_own")]
    fn only_the_recorded_contents_are_own(seen: &str, own: bool, failure: &str) {
        let dir = TempDir::new().unwrap();
        let path = written(&dir, FIRST);
        record(&path, FIRST.as_bytes());
        for _ in 0..EVENTS_PER_SAVE {
            assert_eq!(is_own(&path, seen.as_bytes()), own, "{failure}");
        }
    }

    #[test]
    fn an_unrecorded_path_is_not_own() {
        let dir = TempDir::new().unwrap();
        let path = written(&dir, FIRST);
        assert!(!is_own(&path, FIRST.as_bytes()), "{WRONGLY_OWNED}");
    }

    #[test]
    fn a_later_record_replaces_the_earlier_one() {
        let dir = TempDir::new().unwrap();
        let path = written(&dir, FIRST);
        record(&path, FIRST.as_bytes());
        fs::write(&path, SECOND).unwrap();
        record(&path, SECOND.as_bytes());
        assert!(is_own(&path, SECOND.as_bytes()), "{NOT_RECOGNIZED}");
        assert!(!is_own(&path, FIRST.as_bytes()), "{WRONGLY_OWNED}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_spelling_shares_the_record() {
        let dir = TempDir::new().unwrap();
        let path = written(&dir, FIRST);
        let link = dir.path().join(LINK);
        symlink(dir.path(), &link).unwrap();
        record(&path, FIRST.as_bytes());
        assert!(
            is_own(&link.join(FILE), FIRST.as_bytes()),
            "{NOT_RECOGNIZED}"
        );
    }
}
