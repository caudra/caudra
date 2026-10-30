//! The Secrets section of the Local tab: what the session vault and `[agent.shell_env]` hold by
//! name, never by value.

use std::time::{Duration, SystemTime};

use caudra_agent::secrets::{SecretListing, SecretScope};

use super::rows::{EnvRow, MASK, RowAction};

pub(crate) const SECRETS_SECTION: &str = "Secrets";
pub(super) const SESSION_BADGE: &str = "session";
pub(super) const REMEMBERED_BADGE: &str = "remembered";
pub(super) const EXPIRED_STATE: &str = "expired";
pub(super) const ALL_SESSIONS_STATE: &str = "all sessions";
pub(super) const LOCAL_ONLY: &str = "Secrets reach this machine's shell commands only.";
const SECONDS_PER_MINUTE: u64 = 60;
const MINUTES_PER_HOUR: u64 = 60;
const HOURS_PER_DAY: u64 = 24;
/// Under two days, hours say more than a rounded-down day.
const MIN_DAYS_SHOWN: u64 = 2;

pub(super) fn secret_rows(secrets: &[SecretListing], local: bool) -> Vec<EnvRow> {
    if !local {
        return vec![EnvRow::entry(SECRETS_SECTION, LOCAL_ONLY, String::new()).inert()];
    }
    secrets.iter().map(secret_row).collect()
}

fn secret_row(listing: &SecretListing) -> EnvRow {
    let name = &listing.name;
    let row = EnvRow::entry(SECRETS_SECTION, name, MASK.to_owned())
        .action(RowAction::Secret(listing.scope));
    match listing.scope {
        SecretScope::Remembered => row
            .badge(REMEMBERED_BADGE)
            .state(ALL_SESSIONS_STATE)
            .note(format!(
                "Every session's shell commands receive it as ${name}. e replaces it, m keeps it for this session only, d deletes it."
            )),
        SecretScope::Session if listing.expired() => row
            .badge(SESSION_BADGE)
            .state(EXPIRED_STATE)
            .note("It expired, so shell commands no longer receive it. e saves a new value, d deletes it."),
        SecretScope::Session => row
            .badge(SESSION_BADGE)
            .state(listing.expires_at.map(expires_in).unwrap_or_default())
            .note(format!(
                "This session's shell commands receive it as ${name}. e replaces it, r adds 24 hours, m keeps it for all sessions, d deletes it."
            )),
    }
}

fn expires_in(expires_at: SystemTime) -> String {
    let remaining = expires_at
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO);
    let minutes = (remaining.as_secs() / SECONDS_PER_MINUTE).max(1);
    let hours = minutes / MINUTES_PER_HOUR;
    let days = hours / HOURS_PER_DAY;
    if days >= MIN_DAYS_SHOWN {
        format!("expires in {days}d")
    } else if hours > 0 {
        format!("expires in {hours}h")
    } else {
        format!("expires in {minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const HOUR: Duration = Duration::from_secs(SECONDS_PER_MINUTE * MINUTES_PER_HOUR);
    const MINUTE: Duration = Duration::from_secs(SECONDS_PER_MINUTE);
    const DAY: Duration =
        Duration::from_secs(SECONDS_PER_MINUTE * MINUTES_PER_HOUR * HOURS_PER_DAY);

    #[test_case(DAY * 3 + HOUR * 5, "expires in 3d" ; "days")]
    #[test_case(DAY + HOUR * 23 + MINUTE * 30, "expires in 47h" ; "hours_under_two_days")]
    #[test_case(HOUR * 5 + MINUTE * 30, "expires in 5h" ; "hours")]
    #[test_case(MINUTE * 42 + Duration::from_secs(30), "expires in 42m" ; "minutes")]
    #[test_case(Duration::from_secs(5), "expires in 1m" ; "under_a_minute")]
    fn the_state_says_how_long_is_left(left: Duration, expected: &str) {
        assert_eq!(expires_in(SystemTime::now() + left), expected);
    }
}
