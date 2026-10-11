//! The command that brings a Caudra session back in another pane.

use std::env::{self, consts::EXE_SUFFIX};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// What a restored pane types into its shell. Herdr accepts only a bare
/// command name there, so this is the name Caudra is installed under.
pub const RESUME_COMMAND: &str = "caudra";
const SESSION_FLAG: &str = "--session";
const PATH: &str = "PATH";
/// Linux appends this when a running executable has been replaced or removed.
const REPLACED_EXE_SUFFIX: &str = " (deleted)";

/// The command that reopens `session`, or starts afresh without one. A
/// session that was never saved has nothing to reopen.
pub fn resume_argv(session: Option<&str>) -> Vec<String> {
    let mut argv = vec![RESUME_COMMAND.to_owned()];
    if let Some(session) = session {
        argv.extend([SESSION_FLAG.to_owned(), session.to_owned()]);
    }
    argv
}

/// Resumes `session` with this executable, its replacement at the same path,
/// or the installed `caudra` when neither file remains.
pub fn resume_command_line(session: &str) -> String {
    let program = resume_program(env::current_exe().ok());
    shell_words::join([program.as_str(), SESSION_FLAG, session])
}

fn resume_program(exe: Option<PathBuf>) -> String {
    exe.and_then(|path| {
        if path.is_file() {
            return Some(path.to_string_lossy().into_owned());
        }
        let replacement = path.to_str()?.strip_suffix(REPLACED_EXE_SUFFIX)?;
        Path::new(replacement)
            .is_file()
            .then(|| replacement.to_owned())
    })
    .unwrap_or_else(|| RESUME_COMMAND.to_owned())
}

/// Whether a shell would find `name` on `PATH`. A host types a restored
/// pane's command into its shell, so a command that only runs through an
/// absolute path cannot be offered to it.
pub fn command_on_path(name: &str) -> bool {
    env::var_os(PATH).is_some_and(|paths| command_in(&paths, name))
}

fn command_in(paths: &OsStr, name: &str) -> bool {
    let file = format!("{name}{EXE_SUFFIX}");
    env::split_paths(paths).any(|dir| dir.join(&file).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use test_case::test_case;

    const SESSION: &str = "4Tx8ZrCGn1j9aE6oUvWm2k";
    const RESUME_BINARY: &str = "caudra build";

    #[test_case(None, &[RESUME_COMMAND] ; "unsaved_session_starts_fresh")]
    #[test_case(Some(SESSION), &[RESUME_COMMAND, "--session", SESSION] ; "saved_session_resumes")]
    fn resume_command_names_saved_sessions_only(session: Option<&str>, expected: &[&str]) {
        assert_eq!(resume_argv(session), expected);
    }

    #[test]
    fn the_resume_line_runs_this_executable_on_the_session() {
        let words = shell_words::split(&resume_command_line(SESSION)).unwrap();

        assert_eq!(
            words,
            [
                env::current_exe().unwrap().to_string_lossy().as_ref(),
                SESSION_FLAG,
                SESSION
            ]
        );
    }

    #[test_case(false, true ; "current_executable")]
    #[test_case(true, true ; "replaced_executable")]
    #[test_case(false, false ; "missing_executable")]
    #[test_case(true, false ; "deleted_without_replacement")]
    fn a_resume_uses_a_remaining_executable_or_path(replaced: bool, installed: bool) {
        let temp = TempDir::new().unwrap();
        let installed_path = temp.path().join(RESUME_BINARY);
        if installed {
            fs::write(&installed_path, []).unwrap();
        }
        let mut running_path = installed_path.as_os_str().to_owned();
        if replaced {
            running_path.push(REPLACED_EXE_SUFFIX);
        }

        let program = resume_program(Some(running_path.into()));

        let expected = if installed {
            installed_path.to_string_lossy().into_owned()
        } else {
            RESUME_COMMAND.to_owned()
        };
        assert_eq!(program, expected);
    }

    #[test_case(false ; "only_literal_name_exists")]
    #[test_case(true ; "both_names_exist")]
    fn an_existing_executable_keeps_a_literal_deleted_suffix(unsuffixed_exists: bool) {
        let temp = TempDir::new().unwrap();
        let path = temp
            .path()
            .join(format!("{RESUME_BINARY}{REPLACED_EXE_SUFFIX}"));
        fs::write(&path, []).unwrap();
        if unsuffixed_exists {
            fs::write(temp.path().join(RESUME_BINARY), []).unwrap();
        }

        assert_eq!(resume_program(Some(path.clone())), path.to_string_lossy());
    }

    #[test_case(None ; "current_exe_unavailable")]
    #[test_case(Some(PathBuf::new()) ; "empty_path")]
    fn an_unavailable_executable_uses_path(exe: Option<PathBuf>) {
        assert_eq!(resume_program(exe), RESUME_COMMAND);
    }

    #[test_case(true ; "present")]
    #[test_case(false ; "absent")]
    fn command_lookup_scans_every_path_entry(present: bool) {
        let empty = TempDir::new().unwrap();
        let bin = TempDir::new().unwrap();
        if present {
            fs::write(bin.path().join(format!("{RESUME_COMMAND}{EXE_SUFFIX}")), "").unwrap();
        }
        let paths = env::join_paths([empty.path(), bin.path()]).unwrap();

        assert_eq!(command_in(&paths, RESUME_COMMAND), present);
    }

    #[test]
    fn a_directory_named_like_the_command_does_not_count() {
        let bin = TempDir::new().unwrap();
        fs::create_dir(bin.path().join(format!("{RESUME_COMMAND}{EXE_SUFFIX}"))).unwrap();

        assert!(!command_in(bin.path().as_os_str(), RESUME_COMMAND));
    }
}
