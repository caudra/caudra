use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

pub const DEFAULT_RELATIVE_SUBDIRECTORY: &str = ".";
const MAX_RELATIVE_PATH_BYTES: usize = 1_024;
const MAX_RELATIVE_SEGMENTS: usize = 64;
const MAX_RELATIVE_SEGMENT_BYTES: usize = 255;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootConfigurationError {
    InvalidRelativeSubdirectory,
    BaseUnavailable,
    BaseNotDirectory,
    EffectiveRootUnavailable,
    EffectiveRootNotDirectory,
    EffectiveRootEscape,
}

impl fmt::Display for RootConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRelativeSubdirectory => {
                "the root relative subdirectory is not a canonical relative path"
            }
            Self::BaseUnavailable => "the filesystem root could not be resolved",
            Self::BaseNotDirectory => "the filesystem root is not a directory",
            Self::EffectiveRootUnavailable => {
                "the effective filesystem root does not exist or could not be resolved"
            }
            Self::EffectiveRootNotDirectory => "the effective filesystem root is not a directory",
            Self::EffectiveRootEscape => {
                "the effective filesystem root escapes its selected attachment"
            }
        })
    }
}

impl std::error::Error for RootConfigurationError {}

#[must_use]
pub fn valid_relative_subdirectory(value: &str) -> bool {
    if value == DEFAULT_RELATIVE_SUBDIRECTORY {
        return true;
    }
    if value.is_empty()
        || value.len() > MAX_RELATIVE_PATH_BYTES
        || value.starts_with('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || has_drive_prefix(value)
    {
        return false;
    }

    let segments = value.split('/').collect::<Vec<_>>();
    !segments.is_empty()
        && segments.len() <= MAX_RELATIVE_SEGMENTS
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment.len() <= MAX_RELATIVE_SEGMENT_BYTES
                && !matches!(*segment, "." | "..")
        })
}

pub fn resolve_effective_root(
    base: Option<&Path>,
    relative_subdirectory: &str,
) -> Result<Option<PathBuf>, RootConfigurationError> {
    if !valid_relative_subdirectory(relative_subdirectory) {
        return Err(RootConfigurationError::InvalidRelativeSubdirectory);
    }
    let Some(base) = base else {
        return if relative_subdirectory == DEFAULT_RELATIVE_SUBDIRECTORY {
            Ok(None)
        } else {
            Err(RootConfigurationError::InvalidRelativeSubdirectory)
        };
    };

    let canonical_base =
        fs::canonicalize(base).map_err(|_| RootConfigurationError::BaseUnavailable)?;
    if !fs::metadata(&canonical_base)
        .map_err(|_| RootConfigurationError::BaseUnavailable)?
        .is_dir()
    {
        return Err(RootConfigurationError::BaseNotDirectory);
    }

    let joined = if relative_subdirectory == DEFAULT_RELATIVE_SUBDIRECTORY {
        canonical_base.clone()
    } else {
        canonical_base.join(relative_subdirectory)
    };
    // The lexical check protects this boundary even if validation changes later;
    // canonicalization below is the authoritative symlink-containment check.
    if !inside(&joined, &canonical_base) {
        return Err(RootConfigurationError::EffectiveRootEscape);
    }
    let effective =
        fs::canonicalize(joined).map_err(|_| RootConfigurationError::EffectiveRootUnavailable)?;
    if !inside(&effective, &canonical_base) {
        return Err(RootConfigurationError::EffectiveRootEscape);
    }
    if !fs::metadata(&effective)
        .map_err(|_| RootConfigurationError::EffectiveRootUnavailable)?
        .is_dir()
    {
        return Err(RootConfigurationError::EffectiveRootNotDirectory);
    }
    Ok(Some(effective))
}

fn has_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(not(windows))]
fn inside(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(windows)]
fn inside(path: &Path, root: &Path) -> bool {
    let mut path = path.components();
    root.components().all(|expected| {
        path.next().is_some_and(|actual| match (actual, expected) {
            (std::path::Component::Prefix(actual), std::path::Component::Prefix(expected)) => {
                actual
                    .as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
            }
            (actual, expected) => actual == expected,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_subdirectories_require_canonical_slash_separated_paths() {
        for valid in [".", "research", "research/notes", "research-notes_3"] {
            assert!(
                valid_relative_subdirectory(valid),
                "expected valid: {valid:?}"
            );
        }
        for invalid in [
            "",
            "/absolute",
            "C:/drive",
            "research\\notes",
            "research//notes",
            "research/./notes",
            "research/../notes",
            "research/",
            "research\nnotes",
        ] {
            assert!(
                !valid_relative_subdirectory(invalid),
                "expected invalid: {invalid:?}"
            );
        }
        assert!(!valid_relative_subdirectory(&"x".repeat(1_025)));
        assert!(!valid_relative_subdirectory(&"x".repeat(256)));
        assert!(!valid_relative_subdirectory(
            &std::iter::repeat_n("x", 65).collect::<Vec<_>>().join("/")
        ));
    }

    #[test]
    fn resolves_existing_directories_and_rejects_missing_or_non_directory_targets() {
        let attachment = tempfile::tempdir().expect("attachment");
        fs::create_dir_all(attachment.path().join("research/notes")).expect("nested root");
        fs::write(attachment.path().join("file.txt"), "not a directory").expect("fixture file");

        assert_eq!(
            resolve_effective_root(Some(attachment.path()), ".").unwrap(),
            Some(attachment.path().canonicalize().unwrap())
        );
        assert_eq!(
            resolve_effective_root(Some(attachment.path()), "research/notes").unwrap(),
            Some(
                attachment
                    .path()
                    .join("research/notes")
                    .canonicalize()
                    .unwrap()
            )
        );
        assert_eq!(
            resolve_effective_root(Some(attachment.path()), "missing").unwrap_err(),
            RootConfigurationError::EffectiveRootUnavailable
        );
        assert_eq!(
            resolve_effective_root(Some(attachment.path()), "file.txt").unwrap_err(),
            RootConfigurationError::EffectiveRootNotDirectory
        );
        assert_eq!(resolve_effective_root(None, ".").unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_effective_roots_that_escape_through_symlinks() {
        use std::os::unix::fs::symlink;

        let attachment = tempfile::tempdir().expect("attachment");
        let outside = tempfile::tempdir().expect("outside");
        symlink(outside.path(), attachment.path().join("escape")).expect("escape symlink");

        assert_eq!(
            resolve_effective_root(Some(attachment.path()), "escape").unwrap_err(),
            RootConfigurationError::EffectiveRootEscape
        );
    }
}
