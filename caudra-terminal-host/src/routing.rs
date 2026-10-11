//! Which status lane and which alert sinks a pane gets, from the configured
//! preference and what the terminal around Caudra turned out to be.

use std::io;
use std::time::Duration;

use crate::alert::AlertSink;

/// How long the terminal has to answer the OSC 7501 query.
pub const OSC7501_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertPreference {
    Auto,
    Osc9,
    Bell,
    Off,
}

/// Who receives the persistent pane status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusRoute {
    /// The terminal host Caudra was detected in, such as Herdr or Orca.
    Host,
    /// The immediate terminal, which answered the OSC 7501 query.
    Osc7501,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Routing {
    pub status: StatusRoute,
    /// Turn completions and prompts waiting for the user.
    pub turn_alerts: Option<AlertSink>,
    /// Explicit notices, such as an automation's `notify()`.
    pub notice_alerts: Option<AlertSink>,
}

/// `host_notifies` is `None` outside a terminal host, else whether it raises
/// its own notices. The probe runs only for `Auto` outside a host, and the
/// OSC 9 check only for `Auto`. A host's status is integration rather than a
/// notification, so even `Off` keeps it.
pub fn route(
    preference: AlertPreference,
    host_notifies: Option<bool>,
    probe: impl FnOnce(Duration) -> io::Result<bool>,
    osc9: impl FnOnce() -> bool,
) -> Routing {
    let (status, status_notifies) = match host_notifies {
        Some(notifies) => (StatusRoute::Host, notifies),
        None if preference == AlertPreference::Auto
            && probe(OSC7501_PROBE_TIMEOUT).unwrap_or(false) =>
        {
            (StatusRoute::Osc7501, true)
        }
        None => (StatusRoute::None, false),
    };
    let (turn_alerts, notice_alerts) = match preference {
        AlertPreference::Off => (None, None),
        AlertPreference::Osc9 => (Some(AlertSink::Osc9), Some(AlertSink::Osc9)),
        AlertPreference::Bell => (Some(AlertSink::Bell), Some(AlertSink::Bell)),
        AlertPreference::Auto => {
            let sink = if osc9() {
                AlertSink::Osc9
            } else {
                AlertSink::Bell
            };
            ((!status_notifies).then_some(sink), Some(sink))
        }
    };
    Routing {
        status,
        turn_alerts,
        notice_alerts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const PROBE_MUST_NOT_RUN: &str = "the OSC 7501 probe must not run";
    const OSC9_MUST_NOT_RUN: &str = "OSC 9 detection must not run";

    #[derive(Clone, Copy, Debug)]
    enum Probe {
        Supported,
        Unsupported,
        Fails,
        Forbidden,
    }

    fn probe(probe: Probe) -> impl FnOnce(Duration) -> io::Result<bool> {
        move |timeout| {
            assert_eq!(timeout, OSC7501_PROBE_TIMEOUT);
            match probe {
                Probe::Supported => Ok(true),
                Probe::Unsupported => Ok(false),
                Probe::Fails => Err(io::ErrorKind::BrokenPipe.into()),
                Probe::Forbidden => panic!("{PROBE_MUST_NOT_RUN}"),
            }
        }
    }

    fn routing(
        status: StatusRoute,
        turn_alerts: Option<AlertSink>,
        notice_alerts: Option<AlertSink>,
    ) -> Routing {
        Routing {
            status,
            turn_alerts,
            notice_alerts,
        }
    }

    use AlertSink::{Bell, Osc9};

    #[test_case(Some(true), Probe::Forbidden, true, routing(StatusRoute::Host, None, Some(Osc9)) ; "notifying_host_keeps_notices")]
    #[test_case(Some(false), Probe::Forbidden, false, routing(StatusRoute::Host, Some(Bell), Some(Bell)) ; "quiet_host_keeps_turn_alerts")]
    #[test_case(None, Probe::Supported, false, routing(StatusRoute::Osc7501, None, Some(Bell)) ; "osc7501_owns_turns")]
    #[test_case(None, Probe::Unsupported, true, routing(StatusRoute::None, Some(Osc9), Some(Osc9)) ; "osc9_terminal")]
    #[test_case(None, Probe::Fails, false, routing(StatusRoute::None, Some(Bell), Some(Bell)) ; "failed_probe_falls_back_to_bell")]
    fn auto_routes_by_what_was_found(
        host: Option<bool>,
        answer: Probe,
        osc9: bool,
        expected: Routing,
    ) {
        assert_eq!(
            route(AlertPreference::Auto, host, probe(answer), || osc9),
            expected
        );
    }

    #[test_case(AlertPreference::Osc9, None, routing(StatusRoute::None, Some(Osc9), Some(Osc9)) ; "osc9")]
    #[test_case(AlertPreference::Bell, None, routing(StatusRoute::None, Some(Bell), Some(Bell)) ; "bell")]
    #[test_case(AlertPreference::Off, None, routing(StatusRoute::None, None, None) ; "off")]
    #[test_case(AlertPreference::Osc9, Some(true), routing(StatusRoute::Host, Some(Osc9), Some(Osc9)) ; "osc9_in_host")]
    #[test_case(AlertPreference::Bell, Some(true), routing(StatusRoute::Host, Some(Bell), Some(Bell)) ; "bell_in_host")]
    #[test_case(AlertPreference::Off, Some(true), routing(StatusRoute::Host, None, None) ; "off_keeps_host_status")]
    fn explicit_preferences_detect_nothing(
        preference: AlertPreference,
        host: Option<bool>,
        expected: Routing,
    ) {
        assert_eq!(
            route(preference, host, probe(Probe::Forbidden), || panic!(
                "{OSC9_MUST_NOT_RUN}"
            )),
            expected
        );
    }
}
