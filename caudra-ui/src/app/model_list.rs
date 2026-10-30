//! `/model refresh`: checks for a newer signed model list now, as
//! `caudra models refresh` does, off the UI thread.

use caudra_providers::model_data::{self, RefreshOutcome};
use futures_lite::future;

use super::App;
use crate::repaint::Dirty;

const CHECKING: &str = "Checking for a newer model list…";
const ALREADY_CHECKING: &str = "Already checking for a newer model list";
const UPDATED: &str = "Model list updated, published";
const CURRENT: &str = "Model list is current, published";
const FAILED: &str = "Model list refresh failed";

impl App {
    pub(super) fn refresh_model_list(&mut self) {
        if self.model_list_refresh.is_some() {
            self.flash(ALREADY_CHECKING.into());
            return;
        }
        let config = self.models_config.clone();
        self.model_list_refresh = Some(smol::unblock(move || {
            model_data::refresh(&config, true).map_err(|error| error.to_string())
        }));
        self.flash(CHECKING.into());
    }

    pub(super) fn poll_model_list_refresh(&mut self) -> Dirty {
        let Some(refresh) = self.model_list_refresh.as_mut() else {
            return Dirty::NO;
        };
        let Some(result) = future::block_on(future::poll_once(refresh)) else {
            return Dirty::NO;
        };
        self.model_list_refresh = None;
        self.flash(refresh_message(&result));
        Dirty::YES
    }
}

fn refresh_message(result: &Result<RefreshOutcome, String>) -> String {
    match result {
        Ok(outcome) if outcome.swapped => format!("{UPDATED} {}", outcome.new_updated_at),
        Ok(outcome) => format!("{CURRENT} {}", outcome.new_updated_at),
        Err(error) => format!("{FAILED}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use caudra_providers::model_data::RefreshOutcome;
    use futures_lite::future;
    use test_case::test_case;

    use super::{ALREADY_CHECKING, CURRENT, FAILED, UPDATED, refresh_message};
    use crate::app::tests::{cmd, test_app};

    const OLDER: &str = "2026-10-29T17:32:00Z";
    const NEWER: &str = "2026-10-29T21:34:56Z";
    const ERROR: &str = "model update server returned HTTP 503";

    fn outcome(swapped: bool) -> Result<RefreshOutcome, String> {
        Ok(RefreshOutcome {
            old_updated_at: OLDER.parse().unwrap(),
            new_updated_at: if swapped { NEWER } else { OLDER }.parse().unwrap(),
            swapped,
        })
    }

    #[test_case(outcome(true), format!("{UPDATED} {NEWER}"); "swapped")]
    #[test_case(outcome(false), format!("{CURRENT} {OLDER}"); "unchanged")]
    #[test_case(Err(ERROR.to_owned()), format!("{FAILED}: {ERROR}"); "failed")]
    fn the_flash_says_what_the_check_found(
        result: Result<RefreshOutcome, String>,
        expected: String,
    ) {
        assert_eq!(refresh_message(&result), expected);
    }

    #[test]
    fn a_second_refresh_waits_for_the_first() {
        let mut app = test_app();
        app.model_list_refresh = Some(smol::spawn(future::pending()));

        let actions = app.execute_command(cmd("/model refresh"), 0);

        assert!(actions.is_empty());
        assert_eq!(app.status_bar.flash_text(), Some(ALREADY_CHECKING));
    }
}
