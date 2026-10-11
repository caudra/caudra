//! The terminal around Caudra: which host it runs in, the status its pane
//! shows, the alerts it raises, and the worktrees and panes a host manages.
//!
//! A host is one adapter module plus one entry in [`detect_from`].

pub mod alert;
pub mod bounded_process;
pub mod error;
pub mod herdr;
pub mod host;
pub mod mux;
pub mod orca;
pub mod osc7501;
pub mod reporter;
pub mod resume;
pub mod routing;
pub mod skill;
pub mod status;
pub mod terminal_detect;
pub mod workspaces;

use std::env;
use std::ffi::OsString;
use std::sync::Arc;

pub use error::HostError;
pub use host::TerminalHost;

/// Every variable a host exports to the processes in its pane. Shell commands
/// receive them too, so a host's CLI sees the pane the way any process in it
/// would.
pub const PANE_ENVIRONMENT: &[&str] =
    &concat::<{ herdr::PANE_ENVIRONMENT.len() + orca::PANE_ENVIRONMENT.len() }>(
        herdr::PANE_ENVIRONMENT,
        orca::PANE_ENVIRONMENT,
    );

/// The host this process runs in, from its own environment.
pub fn detect() -> Option<Arc<dyn TerminalHost>> {
    detect_from(|name| env::var_os(name))
}

/// Herdr first: Herdr running in an Orca terminal owns the pane Caudra is in.
pub fn detect_from(get: impl Fn(&str) -> Option<OsString>) -> Option<Arc<dyn TerminalHost>> {
    if let Some(herdr) = herdr::HerdrHost::detect(&get) {
        return Some(Arc::new(herdr));
    }
    orca::OrcaHost::detect(&get).map(|orca| Arc::new(orca) as Arc<dyn TerminalHost>)
}

const fn concat<const N: usize>(first: &[&'static str], second: &[&'static str]) -> [&'static str; N] {
    let mut joined = [""; N];
    let mut index = 0;
    while index < first.len() {
        joined[index] = first[index];
        index += 1;
    }
    while index < N {
        joined[index] = second[index - first.len()];
        index += 1;
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const HERDR_VARIABLES: &[(&str, &str)] = &[
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1:p1"),
        ("HERDR_SOCKET_PATH", "/tmp/herdr.sock"),
    ];
    const ORCA_VARIABLES: &[(&str, &str)] = &[("ORCA_PANE_KEY", "tab:leaf")];

    fn detected(sets: &[&[(&str, &str)]]) -> Option<&'static str> {
        detect_from(|name| {
            sets.iter()
                .flat_map(|set| set.iter())
                .find_map(|(key, value)| (*key == name).then(|| OsString::from(value)))
        })
        .map(|host| host.name())
    }

    #[test_case(&[HERDR_VARIABLES], Some(herdr::HOST) ; "herdr")]
    #[test_case(&[ORCA_VARIABLES], Some(orca::HOST) ; "orca")]
    #[test_case(&[ORCA_VARIABLES, HERDR_VARIABLES], Some(herdr::HOST) ; "herdr_inside_orca")]
    #[test_case(&[], None ; "neither")]
    fn hosts_are_detected_in_order(sets: &[&[(&str, &str)]], expected: Option<&str>) {
        assert_eq!(detected(sets), expected);
    }

    #[test]
    fn the_pane_environment_holds_every_host() {
        assert_eq!(
            PANE_ENVIRONMENT,
            [herdr::PANE_ENVIRONMENT, orca::PANE_ENVIRONMENT].concat()
        );
    }
}
