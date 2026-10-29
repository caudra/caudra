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
}

#[derive(Debug, Clone, ConfigSection)]
#[config(section = "models", fields_only)]
pub struct ModelsConfig {
    #[config(
        ty = "bool",
        default = true,
        env = "CAUDRA_MODELS_REFRESH",
        desc = "Check for signed model updates daily in long-running sessions. Set the environment variable to 0 or 1. Cached models remain usable when disabled"
    )]
    pub refresh: bool,
    #[config(ty = "String", default = DEFAULT_URL,
        desc = "Signed model file URL. Requires HTTPS, or HTTP to loopback. The signature is fetched from the same URL with .sig appended. Takes effect after a restart")]
    pub url: String,
}

#[derive(Debug, Error)]
pub enum ModelsConfigError {
    #[error("models.url must use HTTPS or HTTP to loopback (localhost, 127.0.0.1 or [::1])")]
    Url,
    #[error("CAUDRA_MODELS_REFRESH must be 0 or 1")]
    Refresh,
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
        self.refresh = overlay.refresh.or(self.refresh);
        self.url = overlay.url.or(self.url.take());
    }

    pub(crate) fn resolve(self) -> Result<ModelsConfig, ModelsConfigError> {
        let refresh = match global_env_value(REFRESH_ENV) {
            Ok(None) => self.refresh.unwrap_or(true),
            Ok(Some(value)) if value == "0" => false,
            Ok(Some(value)) if value == "1" => true,
            _ => return Err(ModelsConfigError::Refresh),
        };
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
    fn environment_overrides_config(value: &str, expected: bool) {
        unsafe {
            std::env::set_var(REFRESH_ENV, value);
        }
        let config = ModelsFileConfig {
            refresh: Some(!expected),
            url: None,
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
        base.merge(overlay);
        assert_eq!(base.models.refresh, Some(false));
        assert_eq!(base.models.url.as_deref(), Some(DEFAULT_URL));
    }
}
