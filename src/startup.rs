use std::path::PathBuf;

use caudra_config::FeatureFlags;
use caudra_config::config_file::{self, ConfigFileError};

/// What this process read from the global `caudra.toml` before it parsed its
/// arguments. The experimental switches hold for the life of the process;
/// ordinary settings are read again from `config_dir` by every load, which is
/// what lets `/reload` pick up an edit.
///
/// `Default` is what a test gets: no global files and every experiment off.
#[derive(Clone, Debug, Default)]
pub struct Startup {
    pub features: FeatureFlags,
    pub config_dir: Option<PathBuf>,
}

impl Startup {
    pub fn load() -> Result<Self, ConfigFileError> {
        let config_dir = config_file::resolve_config_dir()?;
        let file = config_file::load_global_config(&config_file::global_config_path(&config_dir))?;
        Ok(Self {
            features: file.features,
            config_dir: Some(config_dir),
        })
    }

    /// Whether the file now asks for other experiments than this process
    /// started with, which only a restart applies. A file that no longer
    /// parses says nothing here; the settings load reports it.
    pub fn features_changed(&self) -> bool {
        self.config_dir.as_deref().is_some_and(|dir| {
            config_file::load_global_config(&config_file::global_config_path(dir))
                .is_ok_and(|file| file.features != self.features)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use caudra_config::config_file::CONFIG_FILE;
    use caudra_config::{Feature, FeatureFlags};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::Startup;

    #[test_case(None, true; "file_removed")]
    #[test_case(Some("[experimental]\nworkflows = true\n"), false; "same_flags")]
    #[test_case(Some("[experimental]\nsandboxes = true\n"), true; "other_flags")]
    #[test_case(Some("[experimental\n"), false; "unparseable")]
    fn only_a_different_experiment_table_asks_for_a_restart(file: Option<&str>, changed: bool) {
        let dir = TempDir::new().unwrap();
        if let Some(contents) = file {
            fs::write(dir.path().join(CONFIG_FILE), contents).unwrap();
        }
        let startup = Startup {
            features: FeatureFlags::NONE.with(Feature::Workflows),
            config_dir: Some(dir.path().to_path_buf()),
        };
        assert_eq!(startup.features_changed(), changed);
    }
}
