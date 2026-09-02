//! Where a project's notes live.
//!
//! Every value here is a compatibility surface: the directory name is derived
//! from a hash of the project root, so any drift silently orphans notes a user
//! already wrote. `caudra-lua` has a test that reruns the original Lua and
//! compares it against [`project_id`].

use std::path::{Path, PathBuf};

const GIT_MARKER: &str = ".git";
const PROJECTS_DIR: &str = "projects";
const MEMORIES_DIR: &str = "memories";

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a over the raw bytes, lowercase hex. The Lua original split the state
/// into 32-bit halves because `bit32` has no 64-bit ops; the arithmetic was
/// plain FNV-1a and this is the same function.
fn fnv1a_64(data: &str) -> String {
    let hash = data.bytes().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    });
    format!("{hash:016x}")
}

/// Readable prefix plus a hash: two checkouts of the same repo need different
/// directories, and the user still wants to recognize theirs on disk.
pub fn project_id(root: &Path) -> String {
    let base = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("root");
    format!("{base}-{}", fnv1a_64(&root.to_string_lossy()))
}

/// The project root is the enclosing repository, so notes follow the checkout
/// rather than whichever subdirectory the session started in.
pub fn project_root(cwd: &Path) -> PathBuf {
    let mut dir = cwd;
    loop {
        if dir.join(GIT_MARKER).exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return cwd.to_path_buf(),
        }
    }
}

pub fn suffix(cwd: &Path) -> PathBuf {
    Path::new(PROJECTS_DIR)
        .join(project_id(&project_root(cwd)))
        .join(MEMORIES_DIR)
}

/// Notes written before the XDG cutover. Reads fall back to it; writes never
/// go there, so a migration is never half-applied.
pub fn legacy_dir(cwd: &Path) -> Option<PathBuf> {
    let dir = caudra_storage::paths::legacy_home_dir()?.join(suffix(cwd));
    dir.is_dir().then_some(dir)
}

pub fn state_dir(cwd: &Path) -> Option<PathBuf> {
    Some(caudra_storage::paths::state_dir().ok()?.join(suffix(cwd)))
}

/// `list` and `read` prefer the legacy directory when it exists so old notes
/// stay reachable; `write` and `delete` always target the state directory.
pub fn resolve(cwd: &Path, allow_legacy: bool) -> Option<PathBuf> {
    if allow_legacy && let Some(dir) = legacy_dir(cwd) {
        return Some(dir);
    }
    state_dir(cwd)
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

    /// Pinned, not computed: this is the on-disk directory name for an existing
    /// user's notes. If it changes, their memories disappear.
    #[test_case("", "cbf29ce484222325" ; "empty is the offset basis")]
    #[test_case("a", "af63dc4c8601ec8c" ; "single byte")]
    #[test_case("foobar", "85944171f73967e8" ; "known fnv vector")]
    fn fnv1a_matches_the_reference_vectors(input: &str, expected: &str) {
        assert_eq!(fnv1a_64(input), expected);
    }

    #[test]
    fn a_project_id_is_the_directory_name_and_a_hash() {
        let id = project_id(Path::new("/home/user/app"));
        assert!(id.starts_with("app-"), "{id}");
        assert_eq!(id.len(), "app-".len() + 16);
    }

    /// Two checkouts of one repo must not share a notes directory.
    #[test]
    fn same_named_directories_in_different_places_differ() {
        assert_ne!(
            project_id(Path::new("/a/app")),
            project_id(Path::new("/b/app"))
        );
    }

    #[test]
    fn a_rootless_path_still_produces_an_id() {
        assert!(project_id(Path::new("/")).starts_with("root-"));
    }

    #[test]
    fn the_repository_root_wins_over_the_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let nested = root.join("crates/inner");
        std::fs::create_dir_all(nested.join(GIT_MARKER).parent().unwrap()).unwrap();
        std::fs::create_dir_all(root.join(GIT_MARKER)).unwrap();
        assert_eq!(project_root(&nested), root);
    }

    #[test]
    fn without_a_repository_the_working_directory_is_the_root() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(project_root(temp.path()), temp.path());
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
