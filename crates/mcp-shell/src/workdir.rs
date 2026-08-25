//! Initial working-directory validation.
//!
//! Resolution blocks lexical traversal and symlink escapes at launch time. It does not confine the
//! command after launch: the shell can use absolute paths, change directory, access the network, or
//! mutate anything permitted to the server process. This is validation, not a command sandbox.

use std::path::{Component, Path, PathBuf};

pub(crate) async fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    // Canonicalization may block on filesystem traversal, so keep it off Tokio worker threads.
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || dunce::canonicalize(path))
        .await
        .map_err(std::io::Error::other)?
}

pub(crate) async fn resolve(root: &Path, requested: &str) -> Result<(PathBuf, String), String> {
    let requested = if requested.is_empty() { "." } else { requested };
    let path = Path::new(requested);
    let lexical = normalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    });
    if !inside(&lexical, root) {
        return Err("Invalid arguments: workdir escapes the configured root".into());
    }
    // Check again after canonicalization: the lexical check catches `..`, while this check catches
    // symlinks whose target leaves the configured root.
    let canonical = canonicalize(&lexical)
        .await
        .map_err(|_| "Invalid arguments: workdir must be an existing directory".to_owned())?;
    if !inside(&canonical, root)
        || !tokio::fs::metadata(&canonical)
            .await
            .map_err(|_| "Invalid arguments: workdir cannot be inspected".to_owned())?
            .is_dir()
    {
        return Err(
            "Invalid arguments: workdir must be a directory inside the configured root".into(),
        );
    }
    let relative = canonical
        .strip_prefix(root)
        .map_err(|_| "Invalid workdir".to_owned())?;
    let relative = if relative.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        relative.to_string_lossy().replace('\\', "/")
    };
    Ok((canonical, relative))
}

fn normalize(path: PathBuf) -> PathBuf {
    // This normalization is intentionally filesystem-independent; canonicalization below remains
    // authoritative for links and existence.
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(part.as_os_str()),
        }
    }
    out
}
#[cfg(not(windows))]
fn inside(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}
#[cfg(windows)]
fn inside(path: &Path, root: &Path) -> bool {
    // Windows path prefixes are case-insensitive in the common filesystems supported here.
    let mut parts = path.components();
    root.components().all(|expected| {
        parts.next().is_some_and(|actual| {
            actual
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
        })
    })
}
