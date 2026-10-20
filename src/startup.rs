use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use caudra_agent::mcp::config as mcp_config;
use caudra_config::config_file::{self, CONFIG_FORMAT, ConfigFileError};
use caudra_config::mcp::MCP_FORMAT;
use caudra_config::migration::disk::{Outcome, UpgradableFile, UpgradeReport, upgrade_global};
use caudra_config::migration::{self, Stamp};
use caudra_config::providers::{PROVIDERS_FORMAT, ProvidersConfig};
use caudra_config::sandbox::{SANDBOX_FORMAT, SandboxDraft};
use caudra_config::workcell::{WORKCELL_FORMAT, validate_workcell_profiles};
use caudra_config::{
    FeatureFlags, PERMISSIONS_FORMAT, PROJECT_DIR, validate_global_permissions,
    validate_project_permissions,
};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::persistent_session_titles;

pub const MIGRATE_COMMAND: &str = "caudra config migrate";
pub const MIGRATE_PROJECT_COMMAND: &str = "caudra config migrate --project";
const OTHER_PROCESSES: &str = "other Caudra processes are running";

/// Every global TOML file, with the parser its upgraded text must pass
/// before an upgrade writes it.
pub const GLOBAL_FILES: &[UpgradableFile] = &[
    UpgradableFile {
        format: &CONFIG_FORMAT,
        validate: config_file::validate_global,
    },
    UpgradableFile {
        format: &PERMISSIONS_FORMAT,
        validate: validate_global_permissions,
    },
    UpgradableFile {
        format: &MCP_FORMAT,
        validate: mcp_config::validate_global,
    },
    UpgradableFile {
        format: &PROVIDERS_FORMAT,
        validate: ProvidersConfig::validate,
    },
    UpgradableFile {
        format: &WORKCELL_FORMAT,
        validate: validate_workcell_profiles,
    },
    UpgradableFile {
        format: &SANDBOX_FORMAT,
        validate: SandboxDraft::validate_file,
    },
];

/// Every project TOML file under `.caudra/`, which only
/// `caudra config migrate --project` writes.
pub const PROJECT_FILES: &[UpgradableFile] = &[
    UpgradableFile {
        format: &CONFIG_FORMAT,
        validate: config_file::validate_project,
    },
    UpgradableFile {
        format: &PERMISSIONS_FORMAT,
        validate: validate_project_permissions,
    },
    UpgradableFile {
        format: &MCP_FORMAT,
        validate: mcp_config::validate_project,
    },
];

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
    /// What upgrading the global files on disk did, before anything else
    /// read them.
    pub upgrade: UpgradeReport,
}

impl Startup {
    /// Reads the global `caudra.toml` upgraded in memory, which is what an
    /// upgrade on disk would write, so the switches do not depend on whether
    /// [`Startup::upgrade_global_files`] runs.
    pub fn load() -> Result<Self, ConfigFileError> {
        let config_dir = config_file::resolve_config_dir()?;
        let file = config_file::load_global_config(&config_file::global_config_path(&config_dir))?;
        Ok(Self {
            features: file.features,
            config_dir: Some(config_dir),
            upgrade: UpgradeReport::default(),
        })
    }

    /// Upgrades the global config files on disk, before this process holds a
    /// session. Without a state directory there is nowhere to back them up,
    /// so they are only read upgraded.
    pub fn upgrade_global_files(&mut self) {
        if let Some(config_dir) = &self.config_dir
            && let Ok(state_dir) = StateDir::resolve()
        {
            self.upgrade = upgrade_global(config_dir, &state_dir, &Stamp::now(), GLOBAL_FILES);
        }
    }

    /// What the upgrade of the global files has to tell the user. A file that
    /// does not load is left out, because its loader reports it.
    pub fn upgrade_notices(&self) -> Vec<String> {
        let blocked_by_sessions = self.upgrade.outcomes.iter().any(
            |outcome| matches!(outcome, Outcome::Blocked { holders, .. } if !holders.is_empty()),
        );
        let titles = blocked_by_sessions
            .then(|| StateDir::resolve().ok())
            .flatten()
            .map(|state_dir| persistent_session_titles(&state_dir))
            .unwrap_or_default();
        upgrade_notices(&self.upgrade, &titles)
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

/// One notice per project file under `cwd` whose format changed since it was
/// written, skipping the files `shown` already holds and adding the rest. A
/// file that only lacks its `version` key means the same to every build, so
/// it needs no notice.
pub fn project_notices(cwd: &Path, shown: &mut HashSet<PathBuf>) -> Vec<String> {
    project_notices_for(cwd, PROJECT_FILES, shown)
}

fn project_notices_for(
    cwd: &Path,
    files: &[UpgradableFile],
    shown: &mut HashSet<PathBuf>,
) -> Vec<String> {
    files
        .iter()
        .filter_map(|file| {
            let format = file.format;
            let path = cwd.join(PROJECT_DIR).join(format.file);
            let loaded = migration::read(&path, format).ok().flatten()?;
            (loaded.from < format.latest && shown.insert(path)).then(|| {
                format!(
                    "{PROJECT_DIR}/{} is format version {}; Caudra read it as version {}. Run `{MIGRATE_PROJECT_COMMAND}` to update it (teammates then need Caudra {} or newer).",
                    format.file,
                    loaded.from,
                    format.latest,
                    env!("CARGO_PKG_VERSION")
                )
            })
        })
        .collect()
}

fn upgrade_notices(report: &UpgradeReport, titles: &HashMap<CaudraId, String>) -> Vec<String> {
    report
        .outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            Outcome::Upgraded {
                file,
                from,
                to,
                backup,
                ..
            } if from < to => Some(format!(
                "Upgraded {file} from format version {from} to {to}; the original is backed up at {}.",
                backup.display()
            )),
            Outcome::Blocked {
                file,
                from,
                latest,
                holders,
            } => Some(format!(
                "{file} is format version {from}; Caudra read it as version {latest}. It stays unchanged on disk while {}. Close them and restart, or run `{MIGRATE_COMMAND}`.",
                open_elsewhere(holders, titles)
            )),
            Outcome::Failed { file, error } => Some(format!(
                "{file} could not be upgraded on disk, so Caudra read it upgraded in memory: {error}"
            )),
            Outcome::Upgraded { .. } | Outcome::Invalid { .. } => None,
        })
        .collect()
}

fn open_elsewhere(holders: &[CaudraId], titles: &HashMap<CaudraId, String>) -> String {
    let (count, sessions) = match holders.len() {
        0 => return OTHER_PROCESSES.to_owned(),
        1 => (1, "session is"),
        count => (count, "sessions are"),
    };
    let named: Vec<String> = holders
        .iter()
        .map(|id| {
            titles
                .get(id)
                .filter(|title| !title.is_empty())
                .cloned()
                .unwrap_or_else(|| id.to_string())
        })
        .collect();
    format!(
        "{count} other Caudra {sessions} open ({})",
        named.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::{Path, PathBuf};

    use caudra_config::config_file::CONFIG_FILE;
    use caudra_config::files::{CONFIG_FILES, Scope};
    use caudra_config::migration::disk::{Outcome, UpgradableFile, UpgradeReport};
    use caudra_config::migration::{ConfigFormat, MigrationError, Step, Upgrade};
    use caudra_config::{Feature, FeatureFlags, PROJECT_DIR};
    use caudra_storage::id::CaudraId;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{GLOBAL_FILES, PROJECT_FILES, Startup, project_notices_for, upgrade_notices};

    const BEHIND_FILE: &str = "behind.toml";
    const TITLE: &str = "Fix the login flow";
    const BACKUP: &str = "/state/backups/config/global/behind.toml.v1.bak";
    const UPGRADED: &str = "Upgraded behind.toml from format version 1 to 2; the original is backed up at /state/backups/config/global/behind.toml.v1.bak.";
    const BLOCKED_PREFIX: &str = "behind.toml is format version 1; Caudra read it as version 2. It stays unchanged on disk while ";
    const BLOCKED_SUFFIX: &str = ". Close them and restart, or run `caudra config migrate`.";
    const WRITE_FAILED: &str = "permission denied";
    const BEHIND: ConfigFormat = ConfigFormat {
        file: BEHIND_FILE,
        latest: 2,
        steps: &[Step {
            from: 1,
            why: None,
            apply: unchanged,
        }],
        requires_version: false,
    };
    const BEHIND_FILES: &[UpgradableFile] = &[UpgradableFile {
        format: &BEHIND,
        validate: accepts,
    }];

    fn unchanged(_: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        Ok(())
    }

    fn accepts(_: &Path, _: &str) -> Result<(), String> {
        Ok(())
    }

    fn formats_in(scope: Scope) -> Vec<&'static str> {
        CONFIG_FILES
            .iter()
            .filter(|file| file.scopes.contains(&scope))
            .filter_map(|file| file.format)
            .map(|format| format.file)
            .collect()
    }

    fn report(outcome: Outcome) -> UpgradeReport {
        UpgradeReport {
            outcomes: vec![outcome],
        }
    }

    fn blocked(holders: Vec<CaudraId>) -> Outcome {
        Outcome::Blocked {
            file: BEHIND_FILE,
            from: 1,
            latest: 2,
            holders,
        }
    }

    #[test_case(Scope::Global, GLOBAL_FILES ; "global")]
    #[test_case(Scope::Project, PROJECT_FILES ; "project")]
    fn the_upgrade_covers_every_toml_file_in_its_scope(scope: Scope, files: &[UpgradableFile]) {
        let upgraded: Vec<&str> = files.iter().map(|file| file.format.file).collect();

        assert_eq!(upgraded, formats_in(scope));
    }

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
            ..Startup::default()
        };
        assert_eq!(startup.features_changed(), changed);
    }

    #[test]
    fn an_upgraded_file_names_its_backup_and_a_stamp_stays_quiet() {
        let upgraded = |from| Outcome::Upgraded {
            file: BEHIND_FILE,
            from,
            to: 2,
            backup: PathBuf::from(BACKUP),
            changes: Vec::new(),
        };

        assert_eq!(
            upgrade_notices(&report(upgraded(1)), &HashMap::new()),
            [UPGRADED]
        );
        assert!(upgrade_notices(&report(upgraded(2)), &HashMap::new()).is_empty());
    }

    #[test]
    fn a_blocked_file_names_the_open_sessions_by_title_or_id() {
        let (titled, untitled) = (CaudraId::generate(), CaudraId::generate());
        let titles = HashMap::from([(titled, TITLE.to_owned())]);

        let notices = upgrade_notices(&report(blocked(vec![titled, untitled])), &titles);

        assert_eq!(
            notices,
            [format!(
                "{BLOCKED_PREFIX}2 other Caudra sessions are open ({TITLE}, {untitled}){BLOCKED_SUFFIX}"
            )]
        );
    }

    #[test_case(1, "1 other Caudra session is open" ; "one_session")]
    #[test_case(0, "other Caudra processes are running" ; "no_named_session")]
    fn a_blocked_file_counts_what_holds_it(count: usize, held: &str) {
        let holders = (0..count).map(|_| CaudraId::generate()).collect();
        let titles = HashMap::new();

        let [notice] = upgrade_notices(&report(blocked(holders)), &titles)
            .try_into()
            .unwrap();

        assert!(
            notice.starts_with(&format!("{BLOCKED_PREFIX}{held}")),
            "{notice}"
        );
        assert!(notice.ends_with(BLOCKED_SUFFIX), "{notice}");
    }

    #[test]
    fn only_failures_the_loaders_do_not_report_get_a_notice() {
        let failed = Outcome::Failed {
            file: BEHIND_FILE,
            error: WRITE_FAILED.to_owned(),
        };
        let invalid = Outcome::Invalid {
            file: BEHIND_FILE,
            error: WRITE_FAILED.to_owned(),
        };

        let [notice] = upgrade_notices(&report(failed), &HashMap::new())
            .try_into()
            .unwrap();
        assert!(notice.ends_with(WRITE_FAILED), "{notice}");
        assert!(upgrade_notices(&report(invalid), &HashMap::new()).is_empty());
    }

    #[test_case("old = 1\n", true ; "legacy")]
    #[test_case("version = 1\n", true ; "stamped_behind")]
    #[test_case("version = 2\n", false ; "current")]
    #[test_case("version = \n", false ; "invalid")]
    fn a_project_file_behind_its_format_gets_one_notice_per_session(text: &str, behind: bool) {
        let project = TempDir::new().unwrap();
        let dir = project.path().join(PROJECT_DIR);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(BEHIND_FILE), text).unwrap();
        let mut shown = HashSet::new();

        let first = project_notices_for(project.path(), BEHIND_FILES, &mut shown);
        let again = project_notices_for(project.path(), BEHIND_FILES, &mut shown);

        let expected: Vec<String> = behind
            .then(|| format!(
                "{PROJECT_DIR}/{BEHIND_FILE} is format version 1; Caudra read it as version 2. Run `caudra config migrate --project` to update it (teammates then need Caudra {} or newer).",
                env!("CARGO_PKG_VERSION")
            ))
            .into_iter()
            .collect();
        assert_eq!(first, expected);
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(fs::read_to_string(dir.join(BEHIND_FILE)).unwrap(), text);
    }
}
