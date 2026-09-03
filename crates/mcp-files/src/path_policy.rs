use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    io,
    path::{Component, Path, PathBuf},
};

use tokio::fs;

use crate::FilesystemError;

#[derive(Debug, Clone)]
pub(crate) struct RootPathPolicy {
    // This canonical path policy rejects stable lexical/symlink escapes, but it
    // is not a descriptor-relative sandbox. Concurrent component replacement
    // between resolve and tokio::fs use remains a documented TOCTOU risk.
    root: PathBuf,
    confined: bool,
}

impl RootPathPolicy {
    pub(crate) async fn create(requested_root: &Path) -> Result<Self, FilesystemError> {
        if requested_root.to_string_lossy().trim().is_empty() {
            return Err(FilesystemError::message("root is required"));
        }
        let root = canonicalize(requested_root).await.map_err(|_| {
            FilesystemError::message(format!(
                "Root does not exist: {}",
                requested_root.to_string_lossy()
            ))
        })?;
        let metadata = fs::metadata(&root)
            .await
            .map_err(|error| FilesystemError::io_path("Cannot inspect root", &root, error))?;
        if !metadata.is_dir() {
            return Err(FilesystemError::message(format!(
                "Root must be a directory: {}",
                requested_root.to_string_lossy()
            )));
        }
        Ok(Self {
            root,
            confined: true,
        })
    }

    /// Host-authorized policy: `root` degrades from a boundary to a base for relative paths.
    /// Clearing `confined` disables root-escape rejection and protected-path denial together,
    /// because a host that is trusted to authorize paths outside the root is also the only party
    /// that can decide whether a credential file inside it is in scope.
    pub(crate) async fn create_unconfined(base_cwd: &Path) -> Result<Self, FilesystemError> {
        let mut policy = Self::create(base_cwd).await?;
        policy.confined = false;
        Ok(policy)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) async fn resolve(&self, requested: &str) -> Result<PathBuf, FilesystemError> {
        if requested.is_empty() {
            return Err(FilesystemError::message("path is required"));
        }
        let requested_path = Path::new(requested);
        let lexical = if requested_path.is_absolute() {
            normalize(requested_path)
        } else {
            normalize(&self.root.join(requested_path))
        };
        if self.confined {
            self.assert_inside(&lexical, requested)?;
        }

        // Both the caller's spelling and the canonical target matter. Without
        // the lexical check, a symlink named `.git` could point at public data.
        if self.confined {
            self.assert_not_protected(&lexical, requested)?;
        }

        let mut current = lexical.clone();
        let mut suffix: Vec<OsString> = Vec::new();
        loop {
            match canonicalize(&current).await {
                Ok(existing) => {
                    let mut canonical = existing;
                    for part in suffix.iter().rev() {
                        canonical.push(part);
                    }
                    if self.confined {
                        self.assert_inside(&canonical, requested)?;
                        self.assert_not_protected(&canonical, requested)?;
                    }
                    return Ok(canonical);
                }
                Err(_) => {
                    let Some(name) = current.file_name().map(ToOwned::to_owned) else {
                        return Err(FilesystemError::message(format!(
                            "Cannot resolve path: {requested}"
                        )));
                    };
                    let Some(parent) = current.parent() else {
                        return Err(FilesystemError::message(format!(
                            "Cannot resolve path: {requested}"
                        )));
                    };
                    suffix.push(name);
                    current = parent.to_path_buf();
                }
            }
        }
    }

    #[cfg(feature = "index")]
    pub(crate) async fn revalidate(&self, authorized: &Path) -> Result<PathBuf, FilesystemError> {
        let requested = authorized.to_string_lossy();
        let canonical = canonicalize(authorized).await.map_err(|error| {
            FilesystemError::io_path("Cannot revalidate authorized path", authorized, error)
        })?;
        if self.confined {
            self.assert_inside(&canonical, &requested)?;
            self.assert_not_protected(&canonical, &requested)?;
        }
        Ok(canonical)
    }

    pub(crate) fn relative(&self, path: &Path) -> Result<String, FilesystemError> {
        if self.confined {
            self.assert_inside(path, &path.to_string_lossy())?;
        }
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return Ok(path.to_string_lossy().into_owned());
        };
        if relative.as_os_str().is_empty() {
            return Ok(".".to_owned());
        }
        Ok(relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"))
    }

    /// Loop-invariant half of [`Self::traversal_allowed`], evaluated once per
    /// traversal rather than once per entry.
    pub(crate) fn traversal_allows_protected(&self, traversal_root: &Path) -> bool {
        // Enumeration must agree with `resolve`. When protected-path denial is off, hiding these
        // entries from traversal would leave a host able to read a path it could never discover,
        // and unable to enumerate what a call is about to touch.
        !self.confined || self.is_protected_path(traversal_root)
    }

    pub(crate) fn traversal_entry_allowed(&self, allows_protected: bool, candidate: &Path) -> bool {
        allows_protected || !self.is_protected_path(candidate)
    }

    #[cfg(test)]
    pub(crate) fn traversal_allowed(&self, traversal_root: &Path, candidate: &Path) -> bool {
        self.traversal_entry_allowed(self.traversal_allows_protected(traversal_root), candidate)
    }

    /// Authorizes a traversal entry that is already canonical.
    ///
    /// Traversal starts at a canonical root and descends only through entries
    /// that were verified not to be symlinks, so every ancestor stays canonical
    /// and the joined path is canonical too. Calling `realpath` per entry would
    /// only recompute that, at the cost of a blocking syscall per entry. The
    /// confinement and protected-path decisions are identical to `resolve`.
    pub(crate) fn authorize_canonical_entry(&self, candidate: &Path) -> bool {
        if !self.confined {
            return true;
        }
        if !starts_with_root(candidate, &self.root) {
            return false;
        }
        candidate
            .strip_prefix(&self.root)
            .is_ok_and(|relative| relative.as_os_str().is_empty() || !protected_relative(relative))
    }

    fn assert_inside(&self, candidate: &Path, requested: &str) -> Result<(), FilesystemError> {
        if starts_with_root(candidate, &self.root) {
            return Ok(());
        }
        Err(FilesystemError::RootEscape(format!(
            "Path escapes filesystem root: {requested}"
        )))
    }

    fn assert_not_protected(
        &self,
        candidate: &Path,
        requested: &str,
    ) -> Result<(), FilesystemError> {
        let relative = candidate.strip_prefix(&self.root).map_err(|_| {
            FilesystemError::RootEscape(format!("Path escapes filesystem root: {requested}"))
        })?;
        if relative.as_os_str().is_empty() {
            return Ok(());
        }
        if protected_relative(relative) {
            return Err(FilesystemError::ProtectedPath(format!(
                "Path is protected by filesystem policy: {requested}"
            )));
        }
        Ok(())
    }

    fn is_protected_path(&self, path: &Path) -> bool {
        protected_relative(path.strip_prefix(&self.root).unwrap_or(path))
    }
}

fn protected_relative(path: &Path) -> bool {
    let mut basename = None;
    for component in path.components() {
        let folded = fold_case(component.as_os_str());
        if matches!(folded.as_ref(), ".git" | ".ssh" | ".workcell") {
            return true;
        }
        basename = Some(folded);
    }
    let Some(basename) = basename else {
        return false;
    };
    let basename = basename.as_ref();
    basename == ".env"
        || basename.starts_with(".env.")
        || matches!(basename, ".npmrc" | ".pypirc" | ".netrc")
        || basename.ends_with(".key")
        || matches!(basename, "id_rsa" | "id_dsa" | "id_ecdsa" | "id_ed25519")
}

/// Lowercases a path component for protected-name comparison.
///
/// This runs for every traversal entry, so the common already-lowercase ASCII
/// component borrows instead of allocating. Non-ASCII components keep full
/// Unicode lowercasing, which matters because names such as `.KEY` spelled with
/// U+212A KELVIN SIGN must still fold onto a protected name.
fn fold_case(component: &OsStr) -> Cow<'_, str> {
    match component.to_str() {
        Some(text) if text.is_ascii() => {
            if text.bytes().any(|byte| byte.is_ascii_uppercase()) {
                Cow::Owned(text.to_ascii_lowercase())
            } else {
                Cow::Borrowed(text)
            }
        }
        _ => Cow::Owned(component.to_string_lossy().to_lowercase()),
    }
}

async fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || dunce::canonicalize(path))
        .await
        .map_err(io::Error::other)?
}

#[cfg(not(windows))]
fn starts_with_root(candidate: &Path, root: &Path) -> bool {
    candidate.starts_with(root)
}

#[cfg(windows)]
fn starts_with_root(candidate: &Path, root: &Path) -> bool {
    let mut candidate = candidate.components();
    root.components().all(|expected| {
        candidate.next().is_some_and(|actual| {
            actual
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
        })
    })
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use proptest::{
        prelude::*,
        test_runner::{Config as ProptestConfig, RngSeed},
    };

    use super::{RootPathPolicy, protected_relative};
    use crate::FilesystemError;

    fn safe_component() -> impl Strategy<Value = String> {
        proptest::string::string_regex("[A-Za-z0-9_-]{1,8}").expect("safe component regex is valid")
    }

    /// Protected-name folding runs for every traversal entry and takes an
    /// allocation-free path for plain ASCII. That shortcut must not weaken the
    /// Unicode case folding a non-ASCII name still relies on: U+212A KELVIN
    /// SIGN lowercases to `k`, so `.KEY` spelled with it is still `.key`.
    #[test]
    fn protected_name_folding_is_case_insensitive_beyond_ascii() {
        for protected in [
            "secret.key",
            "secret.KEY",
            "secret.Key",
            "SECRET.KEY",
            "secret.\u{212a}EY",
            "ID_RSA",
            ".Env.Local",
            ".NPMRC",
        ] {
            assert!(
                protected_relative(Path::new(protected)),
                "{protected} must be protected"
            );
        }
        for allowed in [
            "secret.keys",
            "keyboard.rs",
            "environment.rs",
            "id_rsa.pub.rs",
        ] {
            assert!(
                !protected_relative(Path::new(allowed)),
                "{allowed} must not be protected"
            );
        }
        assert!(protected_relative(Path::new(".GIT/config")));
        assert!(protected_relative(Path::new("nested/.SSH/known_hosts")));
    }

    #[test]
    fn broad_traversal_excludes_protected_descendants_but_explicit_root_allows_them() {
        let root = PathBuf::from("/workspace");
        let policy = RootPathPolicy {
            root: root.clone(),
            confined: true,
        };
        let protected = root.join(".ssh/id_ed25519");

        assert!(!policy.traversal_allowed(&root, &protected));
        assert!(policy.traversal_allowed(&root.join(".ssh"), &protected));
    }

    #[test]
    fn unconfined_traversal_reports_paths_that_unconfined_resolution_would_return() {
        let root = PathBuf::from("/workspace");
        let policy = RootPathPolicy {
            root: root.clone(),
            confined: false,
        };

        // `resolve` skips protected-path denial when unconfined, so hiding the same entry from
        // enumeration would leave a host unable to discover what it is permitted to read.
        assert!(policy.traversal_allowed(&root, &root.join(".ssh/id_ed25519")));
        assert!(policy.traversal_allowed(&root, &root.join(".env.local")));
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            rng_seed: RngSeed::Fixed(0x5eed_cafe),
            ..ProptestConfig::default()
        })]

        #[test]
        fn lexical_parent_traversal_cannot_escape_root(
            interior in prop::collection::vec(safe_component(), 0..8),
            outside in safe_component(),
        ) {
            let root = PathBuf::from("workspace").join("root");
            let policy = RootPathPolicy {
                root,
                confined: true,
            };
            let requested = interior
                .iter()
                .map(String::as_str)
                .chain(std::iter::repeat_n("..", interior.len() + 1))
                .chain(std::iter::once(outside.as_str()))
                .collect::<Vec<_>>()
                .join("/");
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");

            let result = runtime.block_on(policy.resolve(&requested));

            prop_assert!(
                matches!(&result, Err(FilesystemError::RootEscape(_))),
                "lexical escape {:?} unexpectedly resolved as {:?}",
                requested,
                result,
            );
        }
    }
}
