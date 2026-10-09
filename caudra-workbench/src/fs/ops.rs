//! The explorer's own writes.
//!
//! Reading a file is [`super::read`]. Everything that moves, makes or removes
//! a path goes through here, so one place decides what is refused and says why.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use thiserror::Error;

/// What a name may not contain. A rename box takes a name, not a path: moving
/// a file somewhere else is not something it should do by accident.
const SEPARATORS: [char; 2] = ['/', '\\'];
/// What a typed folder starts with to begin at the home directory.
const HOME: char = '~';

#[derive(Debug, Error)]
pub enum OpsError {
    #[error("A name cannot be empty")]
    Empty,
    #[error("A name cannot contain a path separator")]
    Separator,
    #[error("{0} already exists")]
    Exists(String),
    #[error("{name}: {source}")]
    Failed { name: String, source: io::Error },
    #[error("Give the folder's whole path, starting at / or ~")]
    NotAbsolute,
    #[error("{0} is not a folder")]
    NotFolder(String),
}

/// The folder of this machine `typed` names, resolved to the one path every
/// other spelling of it shares. A leading `~` is the home directory. Anything
/// else has to start at the root, since no one folder is the obvious place
/// to read a relative path from.
pub fn folder(typed: &str) -> Result<PathBuf, OpsError> {
    let typed = typed.trim();
    let path = match typed.strip_prefix(HOME).zip(env::home_dir()) {
        Some((rest, home)) if rest.is_empty() || rest.starts_with(SEPARATORS) => {
            home.join(rest.trim_start_matches(SEPARATORS))
        }
        _ => PathBuf::from(typed),
    };
    if !path.is_absolute() {
        return Err(OpsError::NotAbsolute);
    }
    let folder = path
        .canonicalize()
        .map_err(|source| failed(typed, source))?;
    match folder.is_dir() {
        true => Ok(folder),
        false => Err(OpsError::NotFolder(typed.to_owned())),
    }
}

/// Renames `path` to `name`, beside whatever it was next to.
pub fn rename(path: &Path, name: &str) -> Result<PathBuf, OpsError> {
    let parent = path.parent().unwrap_or(path);
    let target = beside(parent, name)?;
    fs::rename(path, &target).map_err(|source| failed(name, source))?;
    Ok(target)
}

pub fn create_file(dir: &Path, name: &str) -> Result<PathBuf, OpsError> {
    let target = beside(dir, name)?;
    fs::write(&target, "").map_err(|source| failed(name, source))?;
    Ok(target)
}

pub fn create_dir(dir: &Path, name: &str) -> Result<PathBuf, OpsError> {
    let target = beside(dir, name)?;
    fs::create_dir(&target).map_err(|source| failed(name, source))?;
    Ok(target)
}

/// Removes `path`, and everything under it when it is a directory. There is no
/// undo, which is why the dialog in front of this one counts what it covers.
pub fn delete(path: &Path) -> Result<(), OpsError> {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let gone = match path.is_dir() {
        true => fs::remove_dir_all(path),
        false => fs::remove_file(path),
    };
    gone.map_err(|source| failed(&name, source))
}

/// How many paths are under `dir`, which is what the dialog has to say out
/// loud. Nothing is skipped: what git ignores still gets removed.
pub fn count_under(dir: &Path) -> usize {
    WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.path() != dir)
        .count()
}

/// Where `name` lands inside `dir`, once it is a name at all and nothing is
/// already sitting there.
fn beside(dir: &Path, name: &str) -> Result<PathBuf, OpsError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(OpsError::Empty);
    }
    if name.contains(SEPARATORS) {
        return Err(OpsError::Separator);
    }
    let target = dir.join(name);
    match target.exists() {
        true => Err(OpsError::Exists(name.to_owned())),
        false => Ok(target),
    }
}

fn failed(name: &str, source: io::Error) -> OpsError {
    OpsError::Failed {
        name: name.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::{OpsError, count_under, create_dir, create_file, delete, folder, rename};
    use std::env;
    use std::fs;
    use tempfile::TempDir;
    use test_case::test_case;

    const RENAMED: &str = "renamed.rs";
    const TAKEN: &str = "taken.rs";
    const ORIGINAL: &str = "a.rs";
    const CONTENTS: &str = "one\n";
    const NOT_MOVED: &str = "the file is not where the rename said it would be";
    const NOT_MADE: &str = "the path the name asked for is not there";
    const STILL_THERE: &str = "the path is still on disk after being deleted";
    const ALLOWED: &str = "a name that cannot be used must be refused with a reason";
    const NOT_RESOLVED: &str = "a folder must resolve to the one path every spelling shares";
    const SUB: &str = "sub";
    const ROUNDABOUT: &str = "sub/../sub/";
    const MISSING: &str = "gone";

    fn project() -> TempDir {
        let dir = TempDir::new().expect("a temporary directory");
        fs::write(dir.path().join(ORIGINAL), CONTENTS).expect("a file");
        dir
    }

    #[test]
    fn a_rename_moves_the_file_and_keeps_what_is_in_it() {
        let dir = project();

        let moved = rename(&dir.path().join(ORIGINAL), RENAMED).expect("a rename");

        assert_eq!(moved, dir.path().join(RENAMED), "{NOT_MOVED}");
        assert!(!dir.path().join(ORIGINAL).exists(), "{NOT_MOVED}");
        assert_eq!(fs::read_to_string(moved).expect("the file"), CONTENTS);
    }

    #[test_case("" => matches OpsError::Empty ; "a name has to be something")]
    #[test_case("  " => matches OpsError::Empty ; "and something more than air")]
    #[test_case("sub/a.rs" => matches OpsError::Separator ; "a rename box is not a move box")]
    #[test_case(TAKEN => matches OpsError::Exists(_) ; "and it will not write over a neighbour")]
    fn a_name_that_cannot_be_used_is_refused(name: &str) -> OpsError {
        let dir = project();
        fs::write(dir.path().join(TAKEN), CONTENTS).expect("a neighbour");

        rename(&dir.path().join(ORIGINAL), name).expect_err(ALLOWED)
    }

    #[test]
    fn a_new_file_is_empty_and_a_new_folder_is_a_folder() {
        let dir = project();

        let file = create_file(dir.path(), RENAMED).expect("a file");
        let folder = create_dir(dir.path(), "sub").expect("a folder");

        assert_eq!(
            fs::read_to_string(file).expect("the file"),
            "",
            "{NOT_MADE}"
        );
        assert!(folder.is_dir(), "{NOT_MADE}");
    }

    #[test]
    fn deleting_a_folder_takes_everything_under_it() {
        let dir = project();
        fs::create_dir_all(dir.path().join("sub/deeper")).expect("a folder");
        fs::write(dir.path().join("sub/deeper/b.rs"), CONTENTS).expect("a file");
        let sub = dir.path().join("sub");

        assert_eq!(count_under(&sub), 2);

        delete(&sub).expect("a delete");

        assert!(!sub.exists(), "{STILL_THERE}");
        assert!(dir.path().join(ORIGINAL).exists(), "{STILL_THERE}");
    }

    #[test]
    fn a_folder_resolves_to_its_one_true_path() {
        let dir = project();
        fs::create_dir(dir.path().join(SUB)).expect("a folder");
        let typed = format!("  {}  ", dir.path().join(ROUNDABOUT).display());

        let resolved = folder(&typed).expect("a folder");

        let expected = dir.path().join(SUB).canonicalize().expect("a real path");
        assert_eq!(resolved, expected, "{NOT_RESOLVED}");
    }

    #[test]
    fn a_tilde_starts_at_the_home_directory() {
        let Some(home) = env::home_dir().and_then(|home| home.canonicalize().ok()) else {
            return;
        };

        assert_eq!(
            folder("~").expect("the home folder"),
            home,
            "{NOT_RESOLVED}"
        );
    }

    #[test_case("", false => matches OpsError::NotAbsolute ; "nothing typed")]
    #[test_case(SUB, false => matches OpsError::NotAbsolute ; "a relative path has nowhere to start")]
    #[test_case(ORIGINAL, true => matches OpsError::NotFolder(_) ; "a file is not a folder")]
    #[test_case(MISSING, true => matches OpsError::Failed { .. } ; "a folder has to be there")]
    fn a_folder_that_cannot_be_listed_is_refused(name: &str, absolute: bool) -> OpsError {
        let dir = project();
        fs::create_dir(dir.path().join(SUB)).expect("a folder");
        let typed = match absolute {
            true => dir.path().join(name),
            false => name.into(),
        };

        folder(&typed.to_string_lossy()).expect_err(ALLOWED)
    }
}
