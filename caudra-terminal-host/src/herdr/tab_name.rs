//! Herdr labels a tab no one named with its position. Caudra names the tab its
//! pane is in after the session while the tab still shows that number, or the
//! session title Caudra last settled it for, so a name the user chose stays.
//! Herdr cannot take a name back, so the last one stays after Caudra exits.

use tracing::warn;

use super::api::{HerdrPane, PaneTab};
use crate::alert::visible_text;
use crate::error::HostError;

#[derive(Debug, Default)]
pub(super) struct TabName {
    /// The title the tab was last settled for, whether Caudra named the tab
    /// or found it named already.
    settled: Option<String>,
    /// Herdr refused one of the calls this makes.
    unsupported: bool,
}

impl TabName {
    pub(super) fn follow(&mut self, pane: &HerdrPane, title: &str) {
        let title = visible_text(title);
        if self.unsupported || title.is_empty() || self.settled.as_ref() == Some(&title) {
            return;
        }
        let named = pane.tab().and_then(|tab| {
            if claims(&tab, &title, self.settled.as_deref()) {
                pane.rename_tab(&tab.tab_id, &title)?;
            }
            Ok(())
        });
        match named {
            Ok(()) => self.settled = Some(title),
            Err(HostError::Unsupported(_, detail)) => {
                warn!(%detail, "Herdr cannot name this tab after the session");
                self.unsupported = true;
            }
            Err(error) => warn!(%error, "naming the Herdr tab after the session failed"),
        }
    }
}

/// Whether the tab is Caudra's to rename to `title`: it shows Herdr's number,
/// or the title Caudra settled it for before. A tab that already shows `title`
/// needs nothing.
fn claims(tab: &PaneTab, title: &str, settled: Option<&str>) -> bool {
    tab.label != title && (tab.numbered || settled == Some(tab.label.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const TAB: &str = "w1:t2";
    const TITLE: &str = "Fix the parser";
    const EARLIER: &str = "Refactor auth";
    const CHOSEN: &str = "review";
    const NUMBER: &str = "3";

    fn tab(label: &str, numbered: bool) -> PaneTab {
        PaneTab {
            tab_id: TAB.into(),
            label: label.into(),
            numbered,
        }
    }

    #[test_case(NUMBER, true, None, true ; "herdr_number")]
    #[test_case(EARLIER, false, Some(EARLIER), true ; "title_caudra_settled")]
    #[test_case(CHOSEN, false, None, false ; "user_name")]
    #[test_case(CHOSEN, false, Some(EARLIER), false ; "user_renamed_after_caudra")]
    #[test_case(TITLE, false, None, false ; "already_the_title")]
    fn only_a_number_or_caudras_own_title_is_replaced(
        label: &str,
        numbered: bool,
        settled: Option<&str>,
        expected: bool,
    ) {
        assert_eq!(claims(&tab(label, numbered), TITLE, settled), expected);
    }

    #[cfg(unix)]
    mod herdr {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;

        use tempfile::TempDir;

        use super::*;
        use crate::herdr::HerdrEnv;

        const PANE: &str = "w1:p4";
        const WORKSPACE: &str = "w1";
        const RENAMES: &str = "renames";
        const USAGE_EXIT: i32 = 2;

        /// Answers `pane get` and `tab list` with this pane in the second of
        /// two tabs labelled `label`, and records every rename. `old` refuses
        /// `tab list` the way a Herdr release without it would.
        fn pane(dir: &Path, label: &str, old: bool) -> HerdrPane {
            let binary = dir.join("herdr");
            let list = if old {
                format!("exit {USAGE_EXIT}")
            } else {
                format!(
                    r#"echo '{{"id":"cli","result":{{"type":"tab_list","tabs":[{{"tab_id":"w1:t1","label":"1"}},{{"tab_id":"{TAB}","label":"{label}"}}]}}}}'"#
                )
            };
            fs::write(
                &binary,
                format!(
                    r#"#!/bin/sh
case "$1 $2" in
"pane get") echo '{{"id":"cli","result":{{"type":"pane_info","pane":{{"pane_id":"{PANE}","tab_id":"{TAB}","workspace_id":"{WORKSPACE}"}}}}}}' ;;
"tab list") {list} ;;
"tab rename") echo "$3 $4" >> '{}'; echo '{{"id":"cli","result":{{"type":"tab_info"}}}}' ;;
esac
"#,
                    dir.join(RENAMES).display()
                ),
            )
            .unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            HerdrPane::new(&HerdrEnv {
                binary: binary.into(),
                socket_path: "/tmp/herdr.sock".into(),
                pane_id: PANE.into(),
                workspace_id: Some(WORKSPACE.into()),
            })
        }

        fn renames(dir: &Path) -> Vec<String> {
            fs::read_to_string(dir.join(RENAMES))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        #[test]
        fn a_numbered_tab_is_named_once_per_title() {
            let dir = TempDir::new().unwrap();
            let pane = pane(dir.path(), "2", false);
            let mut name = TabName::default();

            name.follow(&pane, TITLE);
            name.follow(&pane, TITLE);

            assert_eq!(renames(dir.path()), [format!("{TAB} {TITLE}")]);
        }

        #[test]
        fn a_number_that_is_not_the_position_was_chosen() {
            let dir = TempDir::new().unwrap();
            let pane = pane(dir.path(), "1", false);

            TabName::default().follow(&pane, TITLE);

            assert!(renames(dir.path()).is_empty());
        }

        #[test]
        fn the_title_caudra_gave_follows_the_session() {
            let dir = TempDir::new().unwrap();
            let pane = pane(dir.path(), EARLIER, false);
            let mut name = TabName {
                settled: Some(EARLIER.into()),
                ..TabName::default()
            };

            name.follow(&pane, TITLE);

            assert_eq!(renames(dir.path()), [format!("{TAB} {TITLE}")]);
            assert_eq!(name.settled.as_deref(), Some(TITLE));
        }

        #[test]
        fn an_empty_title_leaves_the_tab_alone() {
            let dir = TempDir::new().unwrap();
            let pane = pane(dir.path(), "2", false);
            let mut name = TabName::default();

            name.follow(&pane, " \n");

            assert!(renames(dir.path()).is_empty());
            assert_eq!(name.settled, None);
        }

        #[test]
        fn a_refused_call_stops_naming() {
            let dir = TempDir::new().unwrap();
            let pane = pane(dir.path(), "2", true);
            let mut name = TabName::default();

            name.follow(&pane, TITLE);

            assert!(name.unsupported);
            assert!(renames(dir.path()).is_empty());
        }
    }
}
