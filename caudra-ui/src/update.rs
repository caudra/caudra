use std::sync::OnceLock;

use caudra_storage::version::{self, UpdateChannel};

const ENV_ENABLE: &str = "CAUDRA_ENABLE_UPDATE_CHECK";

static LATEST: OnceLock<String> = OnceLock::new();

pub use version::{CURRENT, is_newer};

pub fn latest_notice() -> Option<&'static str> {
    LATEST.get().map(|s| s.as_str())
}

fn update_notice(version: &str, channel: &UpdateChannel) -> String {
    format!("run caudra update --channel {channel} to get v{version}")
}

/// The env var wins over `ui.update_check` in both directions, so a single run
/// can opt in or out without touching the config.
fn enabled(env: Option<&str>, configured: bool) -> bool {
    match env.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        None => configured,
    }
}

pub fn spawn_check(configured: bool, channel: UpdateChannel) {
    if !enabled(std::env::var(ENV_ENABLE).ok().as_deref(), configured) {
        return;
    }
    smol::spawn(async move {
        let lookup_channel = channel.clone();
        match smol::unblock(move || version::fetch_release_cached(lookup_channel)).await {
            Ok(Some(release)) if is_newer(&release.version, CURRENT) => {
                let _ = LATEST.set(update_notice(&release.version, &channel));
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
    use super::{enabled, update_notice};
    use caudra_storage::version::UpdateChannel;
    use test_case::test_case;

    const NEW_VERSION: &str = "99.9.9";

    #[test_case(None, false => false; "explicit_config_off")]
    #[test_case(None, true => true; "default_on")]
    #[test_case(Some("1"), false => true; "env_one_overrides_config")]
    #[test_case(Some("true"), false => true; "env_true")]
    #[test_case(Some("YES"), false => true; "env_is_case_insensitive")]
    #[test_case(Some(" on "), false => true; "env_is_trimmed")]
    #[test_case(Some("0"), true => false; "env_zero_overrides_config")]
    #[test_case(Some("false"), true => false; "env_false")]
    #[test_case(Some("off"), true => false; "env_off")]
    #[test_case(Some("NO"), true => false; "env_no")]
    #[test_case(Some("garbage"), true => false; "unparsable_env_stays_off")]
    #[test_case(Some(""), true => true; "empty_env_falls_back_to_config")]
    #[test_case(Some("  "), false => false; "blank_env_preserves_optout")]
    fn enabled_cases(env: Option<&str>, configured: bool) -> bool {
        enabled(env, configured)
    }

    #[test_case(UpdateChannel::Auto, "auto"; "auto")]
    #[test_case(UpdateChannel::Stable, "stable"; "stable")]
    #[test_case(UpdateChannel::Preview, "preview"; "preview")]
    fn notice_uses_configured_channel(channel: UpdateChannel, expected: &str) {
        assert_eq!(
            update_notice(NEW_VERSION, &channel),
            format!("run caudra update --channel {expected} to get v{NEW_VERSION}")
        );
    }
}
