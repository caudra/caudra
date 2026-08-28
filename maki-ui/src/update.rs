use std::sync::OnceLock;

use maki_storage::version;

const ENV_ENABLE: &str = "MAKI_ENABLE_UPDATE_CHECK";

static LATEST: OnceLock<String> = OnceLock::new();

pub use version::{CURRENT, is_newer};

pub fn latest_version() -> Option<&'static str> {
    LATEST.get().map(|s| s.as_str())
}

/// The env var wins over `ui.update_check` in both directions, so a single run
/// can opt in or out without touching the config.
fn enabled(env: Option<&str>, configured: bool) -> bool {
    match env.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        None => configured,
    }
}

/// Asks GitHub for the latest release. Off unless the user opts in, so Maki
/// reaches the network on startup only when asked to.
pub fn spawn_check(configured: bool) {
    if !enabled(std::env::var(ENV_ENABLE).ok().as_deref(), configured) {
        return;
    }
    smol::spawn(async {
        match version::fetch_latest_async().await {
            Ok(v) if is_newer(&v, CURRENT) => {
                let _ = LATEST.set(v);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(error = %e, "update check failed");
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::enabled;
    use test_case::test_case;

    #[test_case(None, false => false; "default_off")]
    #[test_case(None, true => true; "config_on")]
    #[test_case(Some("1"), false => true; "env_one_overrides_config")]
    #[test_case(Some("true"), false => true; "env_true")]
    #[test_case(Some("YES"), false => true; "env_is_case_insensitive")]
    #[test_case(Some(" on "), false => true; "env_is_trimmed")]
    #[test_case(Some("0"), true => false; "env_zero_overrides_config")]
    #[test_case(Some("false"), true => false; "env_false")]
    #[test_case(Some("garbage"), true => false; "unparsable_env_stays_off")]
    #[test_case(Some(""), true => true; "empty_env_falls_back_to_config")]
    fn enabled_cases(env: Option<&str>, configured: bool) -> bool {
        enabled(env, configured)
    }
}
