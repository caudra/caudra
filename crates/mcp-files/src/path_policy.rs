use std::{
    ffi::OsString,
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
        Ok(Self { root })
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
        self.assert_inside(&lexical, requested)?;

        // Both the caller's spelling and the canonical target matter. Without
        // the lexical check, a symlink named `.git` could point at public data.
        self.assert_not_protected(&lexical, requested)?;

        let mut current = lexical.clone();
        let mut suffix: Vec<OsString> = Vec::new();
        loop {
            match canonicalize(&current).await {
                Ok(existing) => {
                    let mut canonical = existing;
                    for part in suffix.iter().rev() {
                        canonical.push(part);
                    }
                    self.assert_inside(&canonical, requested)?;
                    self.assert_not_protected(&canonical, requested)?;
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

    pub(crate) fn relative(&self, path: &Path) -> Result<String, FilesystemError> {
        self.assert_inside(path, &path.to_string_lossy())?;
        let relative = path.strip_prefix(&self.root).map_err(|_| {
            FilesystemError::RootEscape(format!(
                "Path escapes filesystem root: {}",
                path.to_string_lossy()
            ))
        })?;
        if relative.as_os_str().is_empty() {
            return Ok(".".to_owned());
        }
        Ok(relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"))
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
        let parts = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().to_lowercase())
            .collect::<Vec<_>>();
        let basename = parts.last().map(String::as_str).unwrap_or_default();
        let protected_component = parts
            .iter()
            .any(|part| matches!(part.as_str(), ".git" | ".ssh" | ".workcell"));
        let protected_name = basename == ".env"
            || basename.starts_with(".env.")
            || matches!(basename, ".npmrc" | ".pypirc" | ".netrc")
            || basename.ends_with(".key")
            || matches!(basename, "id_rsa" | "id_dsa" | "id_ecdsa" | "id_ed25519");
        if protected_component || protected_name {
            return Err(FilesystemError::ProtectedPath(format!(
                "Path is protected by filesystem policy: {requested}"
            )));
        }
        Ok(())
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
    use std::path::PathBuf;

    use proptest::{
        prelude::*,
        test_runner::{Config as ProptestConfig, RngSeed},
    };

    use super::RootPathPolicy;
    use crate::FilesystemError;

    fn safe_component() -> impl Strategy<Value = String> {
        proptest::string::string_regex("[A-Za-z0-9_-]{1,8}").expect("safe component regex is valid")
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
            let policy = RootPathPolicy { root };
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
