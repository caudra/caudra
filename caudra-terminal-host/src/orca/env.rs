use std::ffi::OsString;

pub(crate) const ORCA_PANE_KEY: &str = "ORCA_PANE_KEY";
pub(crate) const ORCA_TAB_ID: &str = "ORCA_TAB_ID";
pub(crate) const ORCA_WORKTREE_ID: &str = "ORCA_WORKTREE_ID";
pub(crate) const ORCA_WORKSPACE_ID: &str = "ORCA_WORKSPACE_ID";
pub(crate) const ORCA_TERMINAL_HANDLE: &str = "ORCA_TERMINAL_HANDLE";
pub(crate) const ORCA_USER_DATA_PATH: &str = "ORCA_USER_DATA_PATH";
pub(crate) const ORCA_CLI_COMMAND: &str = "ORCA_CLI_COMMAND";
pub(crate) const ORCA_CLI_BIN_DIR: &str = "ORCA_CLI_BIN_DIR";
pub(crate) const ORCA_REMOTE_CLI_BIN_DIR: &str = "ORCA_REMOTE_CLI_BIN_DIR";
pub(crate) const ORCA_RELAY_DIR: &str = "ORCA_RELAY_DIR";
pub(crate) const ORCA_RELAY_NODE_PATH: &str = "ORCA_RELAY_NODE_PATH";
pub(crate) const ORCA_RELAY_SOCKET_PATH: &str = "ORCA_RELAY_SOCKET_PATH";
const DEFAULT_BINARY: &str = "orca";

/// The Orca terminal Caudra runs in, as Orca announces it to every process it
/// starts there. Either the pane key or the terminal handle identifies one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrcaEnv {
    pub binary: OsString,
    pub pane_key: Option<String>,
    pub terminal_handle: Option<String>,
    /// `<repoId>::<path>` of the worktree the terminal belongs to.
    pub worktree_id: Option<String>,
}

impl OrcaEnv {
    pub fn from_env(get: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        let pane_key = text(get(ORCA_PANE_KEY));
        let terminal_handle = text(get(ORCA_TERMINAL_HANDLE));
        if pane_key.is_none() && terminal_handle.is_none() {
            return None;
        }
        Some(Self {
            binary: get(ORCA_CLI_COMMAND)
                .filter(|binary| !binary.is_empty())
                .unwrap_or_else(|| OsString::from(DEFAULT_BINARY)),
            pane_key,
            terminal_handle,
            worktree_id: text(get(ORCA_WORKTREE_ID)),
        })
    }
}

fn text(value: Option<OsString>) -> Option<String> {
    value?.into_string().ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const PANE_KEY: &str = "tab-1:6f1c";
    const HANDLE: &str = "term_8a2";
    const WORKTREE: &str = "repo-1::/work/app";
    const BINARY: &str = "/opt/orca/bin/orca";

    fn detect(values: &[(&str, &str)]) -> Option<OrcaEnv> {
        OrcaEnv::from_env(|name| {
            values
                .iter()
                .find_map(|(key, value)| (*key == name).then(|| OsString::from(value)))
        })
    }

    #[test]
    fn a_complete_environment_names_the_terminal() {
        assert_eq!(
            detect(&[
                (ORCA_PANE_KEY, PANE_KEY),
                (ORCA_TERMINAL_HANDLE, HANDLE),
                (ORCA_WORKTREE_ID, WORKTREE),
                (ORCA_CLI_COMMAND, BINARY),
            ]),
            Some(OrcaEnv {
                binary: BINARY.into(),
                pane_key: Some(PANE_KEY.into()),
                terminal_handle: Some(HANDLE.into()),
                worktree_id: Some(WORKTREE.into()),
            })
        );
    }

    #[test_case(ORCA_PANE_KEY, PANE_KEY ; "pane_key")]
    #[test_case(ORCA_TERMINAL_HANDLE, HANDLE ; "terminal_handle")]
    fn either_identifier_is_enough(name: &str, value: &str) {
        let env = detect(&[(name, value)]).unwrap();

        assert_eq!(env.binary, OsString::from(DEFAULT_BINARY));
    }

    #[test_case(&[] ; "nothing")]
    #[test_case(&[(ORCA_PANE_KEY, ""), (ORCA_TERMINAL_HANDLE, "")] ; "empty_identifiers")]
    #[test_case(&[(ORCA_WORKTREE_ID, WORKTREE), (ORCA_CLI_COMMAND, BINARY)] ; "no_identifier")]
    fn without_an_identifier_there_is_no_orca(values: &[(&str, &str)]) {
        assert_eq!(detect(values), None);
    }
}
