//! What this pane tells the terminal around it: one persistent status for
//! whichever host receives it, one-off alerts, and the window title most
//! terminals and Orca label the tab with. The routing itself lives in
//! `caudra-terminal-host`; this is where the UI feeds it.

use std::io;
use std::time::Duration;

use caudra_config::NotificationMethod;
use caudra_terminal_host::alert::Notifier;
use caudra_terminal_host::host::TerminalHost;
use caudra_terminal_host::osc7501::Osc7501Host;
use caudra_terminal_host::reporter::{Reporter, ReporterHandle};
use caudra_terminal_host::routing::{AlertPreference, StatusRoute, route};
use caudra_terminal_host::status::{PaneMetadata, PaneStatus};
use caudra_terminal_host::terminal_detect::detect_osc9_support;
use tracing::warn;

use crate::terminal;

pub(crate) struct PaneSignals {
    /// The host's reporter, or the one this owns for OSC 7501.
    reporter: Option<ReporterHandle>,
    _osc7501: Option<Reporter>,
    /// Turn completions and prompts waiting for the user.
    turn_alerts: Option<Notifier>,
    /// An automation's `notify()`.
    notice_alerts: Option<Notifier>,
    window_title: WindowTitle,
}

/// Whether the window title follows the focused session. The terminal guard
/// saves the title the shell left and restores it on exit.
#[derive(Debug, PartialEq, Eq)]
enum WindowTitle {
    /// The host names the tab itself, a plugin set the title, or writing it
    /// failed.
    Elsewhere,
    /// The session title last written, if one was since the terminal was
    /// last handed back.
    Session(Option<String>),
}

impl WindowTitle {
    fn due<'a>(&self, title: &'a str) -> Option<&'a str> {
        match self {
            Self::Session(shown) if !title.trim().is_empty() && shown.as_deref() != Some(title) => {
                Some(title)
            }
            _ => None,
        }
    }
}

fn preference(method: NotificationMethod) -> AlertPreference {
    match method {
        NotificationMethod::Auto => AlertPreference::Auto,
        NotificationMethod::Osc9 => AlertPreference::Osc9,
        NotificationMethod::Bell => AlertPreference::Bell,
        NotificationMethod::Off => AlertPreference::Off,
    }
}

fn query_osc7501(timeout: Duration) -> io::Result<bool> {
    #[cfg(unix)]
    {
        crossterm::event::query_program_status(timeout)
    }
    #[cfg(not(unix))]
    {
        let _ = timeout;
        Ok(false)
    }
}

impl PaneSignals {
    /// Routes the pane's signals; only the immediate terminal is probed, and
    /// only outside a host, so this has to run in raw mode.
    pub(crate) fn new(method: NotificationMethod, host_reporter: Option<ReporterHandle>) -> Self {
        let routing = route(
            preference(method),
            host_reporter
                .as_ref()
                .map(ReporterHandle::raises_notifications),
            query_osc7501,
            detect_osc9_support,
        );
        let osc7501 = (routing.status == StatusRoute::Osc7501)
            .then(|| Reporter::for_host(&Osc7501Host as &dyn TerminalHost))
            .flatten();
        let reporter = match routing.status {
            StatusRoute::Host => host_reporter,
            StatusRoute::Osc7501 => osc7501.as_ref().map(Reporter::handle),
            StatusRoute::None => None,
        };
        let window_title = if reporter.as_ref().is_some_and(ReporterHandle::names_tabs) {
            WindowTitle::Elsewhere
        } else {
            WindowTitle::Session(None)
        };
        Self {
            reporter,
            _osc7501: osc7501,
            turn_alerts: routing.turn_alerts.map(Notifier::new),
            notice_alerts: routing.notice_alerts.map(Notifier::new),
            window_title,
        }
    }

    pub(crate) fn reports_status(&self) -> bool {
        self.reporter.is_some()
    }

    /// Whether the terminal reports focus, so an alert can wait for the user
    /// to look away.
    pub(crate) fn reports_focus(&self) -> bool {
        self.notice_alerts.is_some_and(Notifier::reports_focus)
    }

    pub(crate) fn alerts_turns(&self) -> bool {
        self.turn_alerts.is_some()
    }

    pub(crate) fn alerts_notices(&self) -> bool {
        self.notice_alerts.is_some()
    }

    pub(crate) fn report(&self, status: PaneStatus, metadata: PaneMetadata<'_>) {
        if let Some(reporter) = &self.reporter {
            reporter.describe(metadata);
            reporter.observe(status);
        }
    }

    pub(crate) fn report_final(&self, status: PaneStatus) {
        if let Some(reporter) = &self.reporter {
            reporter.observe(status);
        }
    }

    /// Titles the window after the focused session, so the tab carries its
    /// name. An empty title keeps the last one.
    pub(crate) fn show_session_title(&mut self, title: &str) {
        let Some(title) = self.window_title.due(title) else {
            return;
        };
        self.window_title = match terminal::set_window_title(title) {
            Ok(()) => WindowTitle::Session(Some(title.to_owned())),
            Err(error) => {
                warn!(%error, "window title disabled after write failure");
                WindowTitle::Elsewhere
            }
        };
    }

    /// A plugin set the window title, so the session no longer does.
    pub(crate) fn yield_window_title(&mut self) {
        self.window_title = WindowTitle::Elsewhere;
    }

    /// The terminal may not show what was last written: another program had
    /// it, or it was out of sight, as a hidden Orca tab is.
    pub(crate) fn invalidate(&mut self) {
        if let Some(reporter) = &self.reporter {
            reporter.invalidate();
        }
        if let WindowTitle::Session(shown) = &mut self.window_title {
            *shown = None;
        }
    }

    pub(crate) fn alert_turn(&mut self, message: &str) {
        Self::alert(&mut self.turn_alerts, message);
    }

    pub(crate) fn alert_notice(&mut self, message: &str) {
        Self::alert(&mut self.notice_alerts, message);
    }

    fn alert(sink: &mut Option<Notifier>, message: &str) {
        if let Some(notifier) = sink
            && let Err(error) = notifier.notify(message)
        {
            warn!(sink = ?notifier.sink(), %error, "terminal alerts disabled after write failure");
            *sink = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const TITLE: &str = "Fix the parser";
    const EARLIER: &str = "Refactor auth";

    #[test_case(WindowTitle::Session(None), TITLE, Some(TITLE) ; "first_title")]
    #[test_case(WindowTitle::Session(Some(EARLIER.into())), TITLE, Some(TITLE) ; "renamed_session")]
    #[test_case(WindowTitle::Session(Some(TITLE.into())), TITLE, None ; "unchanged")]
    #[test_case(WindowTitle::Session(Some(EARLIER.into())), " ", None ; "untitled_session_keeps_the_last")]
    #[test_case(WindowTitle::Elsewhere, TITLE, None ; "titled_elsewhere")]
    fn only_a_new_session_title_is_written(
        window: WindowTitle,
        title: &str,
        expected: Option<&str>,
    ) {
        assert_eq!(window.due(title), expected);
    }
}
