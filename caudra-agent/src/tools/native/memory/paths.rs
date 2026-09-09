//! Where a project's notes live.
//!
//! The directory name comes from [`caudra_storage::projects`], which plans
//! share: it is a compatibility surface, so any drift silently orphans notes a
//! user already wrote.

use std::path::{Path, PathBuf};

use caudra_storage::projects::project_subdir;

const MEMORIES_DIR: &str = "memories";

pub fn suffix(cwd: &Path) -> PathBuf {
    project_subdir(cwd).join(MEMORIES_DIR)
}

pub fn state_dir(cwd: &Path) -> Option<PathBuf> {
    Some(caudra_storage::paths::state_dir().ok()?.join(suffix(cwd)))
}

/// Rejects anything that could escape the notes directory. Absolute paths and
/// drive letters are refused outright rather than silently re-rooted.
pub fn safe_resolve(dir: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.is_empty() {
        return Err(PATH_REQUIRED.into());
    }
    let bytes = relative.as_bytes();
    let has_drive_letter = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if relative.contains('\0') || relative.starts_with(['/', '\\']) || has_drive_letter {
        return Err(PATH_MUST_BE_RELATIVE.into());
    }
    let mut resolved = dir.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            std::path::Component::Normal(part) => resolved.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !resolved.pop() {
                    return Err(PATH_TRAVERSAL.into());
                }
            }
            _ => return Err(PATH_MUST_BE_RELATIVE.into()),
        }
    }
    if !resolved.starts_with(dir) || resolved == dir {
        return Err(PATH_TRAVERSAL.into());
    }
    Ok(resolved)
}

pub const PATH_REQUIRED: &str = "path is required";
pub const PATH_MUST_BE_RELATIVE: &str = "path must be relative";
pub const PATH_TRAVERSAL: &str = "path traversal outside memories directory is not allowed";

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn notes_live_under_the_projects_own_directory() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            suffix(temp.path()),
            project_subdir(temp.path()).join(MEMORIES_DIR)
        );
    }

    #[test_case("notes.md" ; "plain name")]
    #[test_case("sub/notes.md" ; "nested")]
    #[test_case("./notes.md" ; "explicit current dir")]
    #[test_case("sub/../notes.md" ; "parent that stays inside")]
    fn a_relative_path_resolves_inside_the_directory(input: &str) {
        let dir = Path::new("/memories");
        let resolved = safe_resolve(dir, input).unwrap();
        assert!(resolved.starts_with(dir), "{resolved:?}");
    }

    #[test_case("", PATH_REQUIRED ; "empty")]
    #[test_case("/etc/passwd", PATH_MUST_BE_RELATIVE ; "absolute")]
    #[test_case("\\\\server\\share", PATH_MUST_BE_RELATIVE ; "unc")]
    #[test_case("C:/secrets", PATH_MUST_BE_RELATIVE ; "drive letter")]
    #[test_case("../escape.md", PATH_TRAVERSAL ; "parent")]
    #[test_case("sub/../../escape.md", PATH_TRAVERSAL ; "parent through a subdirectory")]
    #[test_case(".", PATH_TRAVERSAL ; "the directory itself")]
    fn an_unsafe_path_is_rejected(input: &str, expected: &str) {
        assert_eq!(
            safe_resolve(Path::new("/memories"), input).unwrap_err(),
            expected
        );
    }

    /// A sibling directory sharing a prefix is still outside.
    #[test]
    fn a_prefix_sibling_does_not_count_as_inside() {
        assert_eq!(
            safe_resolve(Path::new("/memories"), "../memories-evil/x.md").unwrap_err(),
            PATH_TRAVERSAL
        );
    }

    #[test]
    fn a_null_byte_is_rejected() {
        assert_eq!(
            safe_resolve(Path::new("/memories"), "a\0b").unwrap_err(),
            PATH_MUST_BE_RELATIVE
        );
    }
}
