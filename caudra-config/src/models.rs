use caudra_config_macro::ConfigSection;
use serde::Deserialize;
use thiserror::Error;
use url::{Host, Url};

use crate::{ConfigError, ConfigField, ConfigValue, global_env_value};

pub const DEFAULT_URL: &str = "https://models.caudra.ai/v1/models.json";
const REFRESH_ENV: &str = "CAUDRA_MODELS_REFRESH";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelsFileConfig {
    pub refresh: Option<bool>,
    pub url: Option<String>,
    #[serde(skip)]
    pub(crate) project_disabled: bool,
}

#[derive(Debug, Clone, ConfigSection)]
#[config(section = "models", fields_only)]
pub struct ModelsConfig {
    #[config(
        ty = "bool",
        default = true,
        env = "CAUDRA_MODELS_REFRESH",
        desc = "Check for signed model updates daily in long-running sessions. A project may only disable refresh. The environment variable accepts 0/1, false/true, no/yes or off/on. Cached models remain usable when disabled"
    )]
    pub refresh: bool,
    #[config(ty = "String", default = DEFAULT_URL,
        desc = "Global-only signed model file URL. Project values are ignored. Requires HTTPS, or HTTP to loopback. The signature is fetched from the same URL with .sig appended. Takes effect after a restart")]
    pub url: String,
}

#[derive(Debug, Error)]
pub enum ModelsConfigError {
    #[error("models.url must use HTTPS or HTTP to loopback (localhost, 127.0.0.1 or [::1])")]
    Url,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            refresh: true,
            url: DEFAULT_URL.to_owned(),
        }
    }
}

impl ModelsFileConfig {
    pub(crate) fn merge(&mut self, overlay: Self) {
        self.project_disabled |= overlay.project_disabled;
        self.refresh = overlay.refresh.or(self.refresh);
        self.url = overlay.url.or(self.url.take());
    }

    pub(crate) fn resolve(self) -> Result<ModelsConfig, ModelsConfigError> {
        let refresh = global_env_value(REFRESH_ENV)
            .unwrap_or_else(|_| {
                tracing::warn!(
                    env = REFRESH_ENV,
                    "ignoring unreadable boolean environment override"
                );
                None
            })
            .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
                "0" | "false" | "no" | "off" => Some(false),
                "1" | "true" | "yes" | "on" => Some(true),
                "" => None,
                _ => {
                    tracing::warn!(
                        env = REFRESH_ENV,
                        "ignoring invalid boolean environment override"
                    );
                    None
                }
            })
            .unwrap_or(self.refresh.unwrap_or(true))
            && !self.project_disabled;
        let config = ModelsConfig {
            refresh,
            url: self.url.unwrap_or_else(|| DEFAULT_URL.to_owned()),
        };
        config.validate_url()?;
        Ok(config)
    }
}

impl ModelsConfig {
    pub fn validate_url(&self) -> Result<(), ModelsConfigError> {
        let url = Url::parse(&self.url).map_err(|_| ModelsConfigError::Url)?;
        let loopback = match url.host() {
            Some(Host::Domain("localhost")) => true,
            Some(Host::Ipv4(address)) => address.octets() == [127, 0, 0, 1],
            Some(Host::Ipv6(address)) => address.is_loopback(),
            _ => false,
        };
        if (url.scheme() == "https" && url.host().is_some()) || (url.scheme() == "http" && loopback)
        {
            Ok(())
        } else {
            Err(ModelsConfigError::Url)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_URL, ModelsConfig, ModelsFileConfig, REFRESH_ENV};
    use crate::{ENV_FILE, PROJECT_DIR, RawConfig, load_env_files_scoped};
    use std::fs;
    use tempfile::TempDir;
    use test_case::test_case;

    #[test_case("https://example.com/models.json", true; "https")]
    #[test_case("http://127.0.0.1:1234/models.json", true; "ipv4")]
    #[test_case("http://localhost/models.json", true; "localhost")]
    #[test_case("http://[::1]/models.json", true; "ipv6")]
    #[test_case("http://example.com/models.json", false; "public_http")]
    #[test_case("http://localhost@example.com/models.json", false; "userinfo")]
    #[test_case("ftp://localhost/models.json", false; "scheme")]
    fn refresh_url_policy(url: &str, allowed: bool) {
        assert_eq!(
            ModelsConfig {
                url: url.to_owned(),
                ..Default::default()
            }
            .validate_url()
            .is_ok(),
            allowed
        );
    }

    #[test_case("0", false; "disabled")]
    #[test_case("1", true; "enabled")]
    #[test_case("false", false; "false_word")]
    #[test_case(" TRUE ", true; "true_word")]
    #[test_case("off", false; "off_word")]
    #[test_case("yes", true; "yes_word")]
    fn environment_overrides_config(value: &str, expected: bool) {
        unsafe {
            std::env::set_var(REFRESH_ENV, value);
        }
        let config = ModelsFileConfig {
            refresh: Some(!expected),
            url: None,
            ..Default::default()
        }
        .resolve()
        .unwrap();
        assert_eq!(config.refresh, expected);
        unsafe {
            std::env::remove_var(REFRESH_ENV);
        }
    }

    #[test]
    fn project_environment_cannot_enable_refresh() {
        let dir = TempDir::new().unwrap();
        let global = dir.path().join("global");
        let project = dir.path().join(PROJECT_DIR);
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(global.join(ENV_FILE), format!("{REFRESH_ENV}=0")).unwrap();
        fs::write(project.join(ENV_FILE), format!("{REFRESH_ENV}=1")).unwrap();
        unsafe {
            std::env::remove_var(REFRESH_ENV);
        }
        load_env_files_scoped(dir.path(), Some(&global), true);
        assert!(!ModelsFileConfig::default().resolve().unwrap().refresh);
        unsafe {
            std::env::remove_var(REFRESH_ENV);
        }
    }

    #[test]
    fn optional_settings_merge_independently() {
        let mut base = RawConfig::default();
        base.models.refresh = Some(false);
        let mut overlay = RawConfig::default();
        overlay.models.url = Some(DEFAULT_URL.to_owned());
        base.merge_global(overlay);
        assert_eq!(base.models.refresh, Some(false));
        assert_eq!(base.models.url.as_deref(), Some(DEFAULT_URL));
    }

    #[test_case(true)]
    #[test_case(false)]
    fn invalid_environment_is_ignored(configured: bool) {
        unsafe {
            std::env::set_var(REFRESH_ENV, "invalid");
        }
        let config = ModelsFileConfig {
            refresh: Some(configured),
            ..Default::default()
        };
        assert_eq!(config.resolve().unwrap().refresh, configured);
        unsafe {
            std::env::remove_var(REFRESH_ENV);
        }
    }

    #[test_case(false, true, false; "cannot_enable")]
    #[test_case(true, false, false; "can_disable")]
    #[test_case(true, true, true; "cannot_change_url")]
    fn project_only_disables_refresh(global: bool, project: bool, expected: bool) {
        unsafe {
            std::env::remove_var(REFRESH_ENV);
        }
        let mut base = RawConfig::default();
        base.models.refresh = Some(global);
        let mut overlay = RawConfig::default();
        overlay.models.refresh = Some(project);
        overlay.models.url = Some("http://untrusted.example/models.json".into());
        base.merge(overlay);
        let config = base.models.resolve().unwrap();
        assert_eq!(config.url, DEFAULT_URL);
        assert_eq!(config.refresh, expected);
    }
}
