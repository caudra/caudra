use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwapOption;
use caudra_storage::version::{self, Lookup, UpdateChannel, VersionError};
use ratatui::text::{Line, Span};

use crate::theme;

const ENV_ENABLE: &str = "CAUDRA_ENABLE_UPDATE_CHECK";
const HEADLINE: &str = "Update available: v";
const SHORT_HEADLINE: &str = "Update: v";
const SEPARATOR: &str = " \u{2014} ";
pub(crate) const CLOSE: &str = " \u{d7} ";
pub(crate) const CLOSE_TIP: &str =
    "Hides this notice until Caudra restarts\nClick or run /dismiss-update";

static NOTICE: LazyLock<Arc<UpdateNotice>> = LazyLock::new(Arc::default);

pub use version::{CURRENT, is_newer};

/// A newer release, and the resolved channel it was found on, which is the
/// one the update command has to name.
#[derive(Debug, PartialEq, Eq)]
pub struct Available {
    pub version: String,
    pub channel: UpdateChannel,
}

/// What every session in the process shows about a newer release. Dismissing
/// empties it for the rest of the process and saves nothing, so the next
/// start shows the same release again.
#[derive(Default)]
pub struct UpdateNotice(ArcSwapOption<Available>);

impl UpdateNotice {
    pub fn latest(&self) -> Option<Arc<Available>> {
        self.0.load_full()
    }

    pub fn publish(&self, available: Available) {
        self.0.store(Some(Arc::new(available)));
    }

    pub fn dismiss(&self) {
        self.0.store(None);
    }
}

/// The notice the startup check publishes to, shared by every session.
pub fn notice() -> Arc<UpdateNotice> {
    Arc::clone(&NOTICE)
}

/// The env var wins over `ui.update_check` in both directions, so a single run
/// can opt in or out without touching the config. Opting in explicitly also
/// skips the cache: whoever sets it wants today's answer, not yesterday's.
fn startup_lookup(env: Option<&str>, configured: bool) -> Option<Lookup> {
    match env.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
            .then_some(Lookup::Fresh),
        None => configured.then_some(Lookup::Cached),
    }
}

pub fn spawn_check(configured: bool, channel: UpdateChannel) {
    let Some(lookup) = startup_lookup(std::env::var(ENV_ENABLE).ok().as_deref(), configured) else {
        return;
    };
    smol::spawn(async move {
        match smol::unblock(move || check(channel, lookup)).await {
            Ok(Some(available)) => NOTICE.publish(available),
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e, ?lookup, "update check failed");
            }
        }
    })
    .detach();
}

fn check(channel: UpdateChannel, lookup: Lookup) -> Result<Option<Available>, VersionError> {
    let channel = channel.resolve()?;
    let release = version::fetch_release_cached(channel.clone(), lookup)?;
    Ok(release
        .filter(|release| is_newer(&release.version, CURRENT))
        .map(|release| Available {
            version: release.version,
            channel,
        }))
}

/// The widest wording that fits beside the close control. The command goes
/// first: the version is the news, and the close control is the way out.
pub(crate) fn banner_line(available: &Available, width: u16) -> Line<'static> {
    let theme = theme::current();
    let headline = |prefix: &str| {
        Span::styled(
            format!("{prefix}{}", available.version),
            theme.status_notice,
        )
    };
    let full = Line::from(vec![
        headline(HEADLINE),
        Span::styled(SEPARATOR, theme.status_dim),
        Span::raw(format!("caudra update --channel {}", available.channel)),
    ]);
    [full, Line::from(headline(HEADLINE))]
        .into_iter()
        .find(|line| line.width() <= usize::from(width))
        .unwrap_or_else(|| Line::from(headline(SHORT_HEADLINE)))
}

#[cfg(test)]
mod tests {
    use super::{Available, UpdateNotice, banner_line, startup_lookup};
    use caudra_storage::version::{Lookup, UpdateChannel};
    use test_case::test_case;

    const NEW_VERSION: &str = "99.9.9";
    const WIDE: u16 = 80;
    const MEDIUM: u16 = 30;
    const NARROW: u16 = 12;

    fn available(channel: UpdateChannel) -> Available {
        Available {
            version: NEW_VERSION.to_owned(),
            channel,
        }
    }

    #[test_case(None, false => None; "explicit_config_off")]
    #[test_case(None, true => Some(Lookup::Cached); "default_on_uses_the_cache")]
    #[test_case(Some("1"), false => Some(Lookup::Fresh); "env_one_overrides_config")]
    #[test_case(Some("1"), true => Some(Lookup::Fresh); "env_one_skips_the_cache")]
    #[test_case(Some("true"), false => Some(Lookup::Fresh); "env_true")]
    #[test_case(Some("YES"), false => Some(Lookup::Fresh); "env_is_case_insensitive")]
    #[test_case(Some(" on "), false => Some(Lookup::Fresh); "env_is_trimmed")]
    #[test_case(Some("0"), true => None; "env_zero_overrides_config")]
    #[test_case(Some("false"), true => None; "env_false")]
    #[test_case(Some("off"), true => None; "env_off")]
    #[test_case(Some("NO"), true => None; "env_no")]
    #[test_case(Some("garbage"), true => None; "unparsable_env_stays_off")]
    #[test_case(Some(""), true => Some(Lookup::Cached); "empty_env_falls_back_to_config")]
    #[test_case(Some("  "), false => None; "blank_env_preserves_optout")]
    fn startup_lookup_cases(env: Option<&str>, configured: bool) -> Option<Lookup> {
        startup_lookup(env, configured)
    }

    #[test_case(UpdateChannel::Stable, "stable"; "stable")]
    #[test_case(UpdateChannel::Preview, "preview"; "preview")]
    fn banner_names_the_channel_it_was_found_on(channel: UpdateChannel, expected: &str) {
        assert_eq!(
            banner_line(&available(channel), WIDE).to_string(),
            format!("Update available: v{NEW_VERSION} \u{2014} caudra update --channel {expected}")
        );
    }

    #[test_case(MEDIUM => format!("Update available: v{NEW_VERSION}"); "command_goes_first")]
    #[test_case(NARROW => format!("Update: v{NEW_VERSION}"); "version_stays_last")]
    fn banner_shortens_to_fit(width: u16) -> String {
        banner_line(&available(UpdateChannel::Preview), width).to_string()
    }

    #[test]
    fn dismissing_empties_the_notice() {
        let notice = UpdateNotice::default();
        notice.publish(available(UpdateChannel::Preview));
        assert_eq!(
            notice.latest().as_deref(),
            Some(&available(UpdateChannel::Preview))
        );
        notice.dismiss();
        assert_eq!(notice.latest(), None);
    }
}
