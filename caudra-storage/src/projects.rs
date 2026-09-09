//! Where a project's own state lives.
//!
//! Every value here is a compatibility surface: the directory name is derived
//! from a hash of the project root, so any drift silently orphans notes and
//! plans a user already wrote.

use std::path::{Path, PathBuf};

pub(crate) const GIT_MARKER: &str = ".git";
pub(crate) const PROJECTS_DIR: &str = "projects";

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

/// The project root is the enclosing repository, so state follows the checkout
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

/// The state-directory-relative home for everything scoped to `cwd`'s project.
pub fn project_subdir(cwd: &Path) -> PathBuf {
    Path::new(PROJECTS_DIR).join(project_id(&project_root(cwd)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    /// Pinned, not computed: this is the on-disk directory name for an existing
    /// user's state. If it changes, their memories and plans disappear.
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

    /// Two checkouts of one repo must not share a state directory.
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
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(GIT_MARKER)).unwrap();
        assert_eq!(project_root(&nested), root);
    }

    #[test]
    fn without_a_repository_the_working_directory_is_the_root() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(project_root(temp.path()), temp.path());
    }

    #[test]
    fn a_subdirectory_shares_the_repositorys_state_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let nested = root.join("crates/inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(GIT_MARKER)).unwrap();
        assert_eq!(project_subdir(&nested), project_subdir(&root));
    }
}
