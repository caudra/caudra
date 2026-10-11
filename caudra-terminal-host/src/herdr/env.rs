use std::ffi::{OsStr, OsString};

use super::cli::HerdrCli;

pub(crate) const HERDR_ENV: &str = "HERDR_ENV";
pub(crate) const HERDR_PANE_ID: &str = "HERDR_PANE_ID";
pub(crate) const HERDR_BIN_PATH: &str = "HERDR_BIN_PATH";
pub(crate) const HERDR_SOCKET_PATH: &str = "HERDR_SOCKET_PATH";
pub(crate) const HERDR_WORKSPACE_ID: &str = "HERDR_WORKSPACE_ID";
pub(crate) const HERDR_TAB_ID: &str = "HERDR_TAB_ID";
const ENABLED_MARKER: &str = "1";
const DEFAULT_BINARY: &str = "herdr";

/// The pane Caudra runs in, as Herdr announces it to every process it starts
/// there. Reporting needs the marker, the pane and the socket together, so a
/// partial set means Caudra is not in a Herdr pane at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HerdrEnv {
    pub binary: OsString,
    pub socket_path: OsString,
    pub pane_id: String,
    pub workspace_id: Option<String>,
}

impl HerdrEnv {
    pub fn from_env(get: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        if get(HERDR_ENV).as_deref() != Some(OsStr::new(ENABLED_MARKER)) {
            return None;
        }
        Some(Self {
            binary: get(HERDR_BIN_PATH)
                .filter(|binary| !binary.is_empty())
                .unwrap_or_else(|| OsString::from(DEFAULT_BINARY)),
            socket_path: get(HERDR_SOCKET_PATH).filter(|socket| !socket.is_empty())?,
            pane_id: text(get(HERDR_PANE_ID))?,
            workspace_id: text(get(HERDR_WORKSPACE_ID)),
        })
    }

    pub fn cli(&self) -> HerdrCli {
        HerdrCli::new(self.binary.clone(), self.socket_path.clone())
    }
}

fn text(value: Option<OsString>) -> Option<String> {
    value?.into_string().ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const PANE: &str = "w1:p2";
    const WORKSPACE: &str = "w1";
    const BINARY: &str = "/opt/herdr/bin/herdr";
    const SOCKET: &str = "/tmp/herdr.sock";

    fn detect(values: &[(&str, &str)]) -> Option<HerdrEnv> {
        HerdrEnv::from_env(|name| {
            values
                .iter()
                .find_map(|(key, value)| (*key == name).then(|| OsString::from(value)))
        })
    }

    fn complete() -> Vec<(&'static str, &'static str)> {
        vec![
            (HERDR_ENV, ENABLED_MARKER),
            (HERDR_PANE_ID, PANE),
            (HERDR_BIN_PATH, BINARY),
            (HERDR_SOCKET_PATH, SOCKET),
            (HERDR_WORKSPACE_ID, WORKSPACE),
        ]
    }

    fn without(missing: &str) -> Vec<(&'static str, &'static str)> {
        complete()
            .into_iter()
            .filter(|(name, _)| *name != missing)
            .collect()
    }

    #[test]
    fn complete_environment_names_the_pane() {
        assert_eq!(
            detect(&complete()),
            Some(HerdrEnv {
                binary: BINARY.into(),
                socket_path: SOCKET.into(),
                pane_id: PANE.into(),
                workspace_id: Some(WORKSPACE.into()),
            })
        );
    }

    #[test]
    fn missing_binary_falls_back_to_path_lookup() {
        assert_eq!(
            detect(&without(HERDR_BIN_PATH)).unwrap().binary,
            OsString::from(DEFAULT_BINARY)
        );
    }

    #[test]
    fn workspace_is_optional() {
        assert_eq!(
            detect(&without(HERDR_WORKSPACE_ID)).unwrap().workspace_id,
            None
        );
    }

    #[test_case(HERDR_ENV ; "environment_marker")]
    #[test_case(HERDR_PANE_ID ; "pane_id")]
    #[test_case(HERDR_SOCKET_PATH ; "socket_path")]
    fn missing_required_variable_means_no_pane(missing: &str) {
        assert_eq!(detect(&without(missing)), None);
    }

    #[test_case("" ; "empty")]
    #[test_case("0" ; "zero")]
    #[test_case("true" ; "word")]
    fn only_the_exact_marker_enables_herdr(marker: &str) {
        let mut values = complete();
        values[0] = (HERDR_ENV, marker);

        assert_eq!(detect(&values), None);
    }

    #[test]
    fn empty_pane_id_means_no_pane() {
        let mut values = complete();
        values[1] = (HERDR_PANE_ID, "");

        assert_eq!(detect(&values), None);
    }
}
