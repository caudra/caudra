//! Upgrades config files on disk. Startup upgrades the global files, and
//! rewrites one whose format changed only while no other Caudra runs, since an
//! older one would refuse it on its next reload; a file that only lacks its
//! `version` key means the same to every build, so it is stamped regardless.
//! `caudra config migrate` upgrades global and project files alike while it
//! holds the instance lock itself. Every rewrite is backed up first, and a
//! file that cannot be rewritten is still read upgraded in memory.

use std::io;
use std::path::{Path, PathBuf};

use caudra_storage::backups::{BackupScope, back_up_config};
use caudra_storage::id::CaudraId;
use caudra_storage::private_file::FileRevision;
use caudra_storage::{InstanceLock, StateDir, open_instances, try_instance_lock};
use jiff::Timestamp;
use tracing::{info, warn};

use super::{Change, ConfigFormat, Stamp, migrate};
use crate::atomic_file::{self, AtomicFileError};

const NOT_UTF8: &str = "the file is not valid UTF-8";
const WOULD_NOT_LOAD: &str = "the upgraded file would not load, so the original was left unchanged";
const BACKUP_FAILED: &str = "it could not be backed up first";

/// Whether a file's upgraded text, read from `path`, loads with its real parser.
pub type Validator = fn(&Path, &str) -> Result<(), String>;

/// A config file an upgrade on disk covers.
pub struct UpgradableFile {
    pub format: &'static ConfigFormat,
    pub validate: Validator,
}

/// What an upgrade on disk did with one file that was not current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Upgraded {
        file: &'static str,
        from: u32,
        to: u32,
        backup: PathBuf,
        changes: Vec<Change>,
    },
    /// Left as it is on disk, because other Caudra processes run. `holders`
    /// names the sessions they have open, and can be empty when none is.
    Blocked {
        file: &'static str,
        from: u32,
        latest: u32,
        holders: Vec<CaudraId>,
    },
    /// Left as it is on disk, because it does not read, parse, or upgrade to
    /// text that loads. Its loader reports `error` as well.
    Invalid { file: &'static str, error: String },
    /// Left as it is on disk, because it could not be backed up or written,
    /// or the instance lock could not be checked.
    Failed { file: &'static str, error: String },
}

/// One [`Outcome`] per file that was not current, in the order the files
/// were given. Empty when every file was current or missing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpgradeReport {
    pub outcomes: Vec<Outcome>,
}

/// What upgrading one file that is not current would change, worked out
/// without writing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Planned {
    Upgrade {
        file: &'static str,
        from: u32,
        to: u32,
        original: String,
        upgraded: String,
        changes: Vec<Change>,
    },
    Invalid {
        file: &'static str,
        error: String,
    },
}

/// A file's upgrade, checked to load, and the text it replaces.
struct Candidate {
    original: String,
    revision: FileRevision,
    from: u32,
    text: String,
    changes: Vec<Change>,
}

/// Whether this process may rewrite a file whose format changed.
enum Guard {
    Acquired {
        _lock: InstanceLock,
    },
    /// The caller holds the lock for the whole run.
    Held,
    Busy(Vec<CaudraId>),
    Unavailable(String),
}

struct Upgrader<'a> {
    state_dir: &'a StateDir,
    scope: BackupScope<'a>,
    stamp: &'a Stamp,
    guard: Option<Guard>,
}

impl UpgradeReport {
    /// Logs each outcome, for a process that reads the report once its log
    /// sink exists.
    pub fn log(&self) {
        for outcome in &self.outcomes {
            match outcome {
                Outcome::Upgraded {
                    file,
                    from,
                    to,
                    backup,
                    changes,
                } => info!(
                    file,
                    from,
                    to,
                    backup = %backup.display(),
                    changes = changes.len(),
                    "upgraded a global config file"
                ),
                Outcome::Blocked {
                    file,
                    from,
                    latest,
                    holders,
                } => warn!(
                    file,
                    from,
                    latest,
                    open_sessions = holders.len(),
                    "global config file left at its old format while other Caudra processes run"
                ),
                Outcome::Invalid { file, error } => {
                    warn!(file, %error, "global config file not upgraded, because it does not load");
                }
                Outcome::Failed { file, error } => {
                    warn!(file, %error, "global config file not upgraded on disk");
                }
            }
        }
    }
}

impl Outcome {
    pub fn file(&self) -> &'static str {
        match self {
            Self::Upgraded { file, .. }
            | Self::Blocked { file, .. }
            | Self::Invalid { file, .. }
            | Self::Failed { file, .. } => file,
        }
    }
}

impl Planned {
    pub fn file(&self) -> &'static str {
        match self {
            Self::Upgrade { file, .. } | Self::Invalid { file, .. } => file,
        }
    }
}

impl Guard {
    fn take(state_dir: &StateDir) -> Self {
        match try_instance_lock(state_dir) {
            Ok(Some(lock)) => Self::Acquired { _lock: lock },
            Ok(None) => Self::Busy(open_instances(state_dir)),
            Err(error) => Self::Unavailable(error.to_string()),
        }
    }
}

/// Upgrades every one of `files` in `config_dir` that is behind its format,
/// backing each up under `state_dir` first. The instance lock is taken only
/// when a file's format changed, and then held for the whole batch.
pub fn upgrade_global(
    config_dir: &Path,
    state_dir: &StateDir,
    stamp: &Stamp,
    files: &[UpgradableFile],
) -> UpgradeReport {
    Upgrader {
        state_dir,
        scope: BackupScope::Global,
        stamp,
        guard: None,
    }
    .run(config_dir, files)
}

/// Upgrades every one of `files` in `dir`, whose owner `scope` names, for a
/// caller that holds the instance lock, so no file is left blocked.
pub fn upgrade_holding(
    dir: &Path,
    state_dir: &StateDir,
    scope: BackupScope<'_>,
    stamp: &Stamp,
    files: &[UpgradableFile],
    _lock: &InstanceLock,
) -> UpgradeReport {
    Upgrader {
        state_dir,
        scope,
        stamp,
        guard: Some(Guard::Held),
    }
    .run(dir, files)
}

/// What upgrading `files` in `dir` would change. Missing and current files
/// are left out, as an upgrade leaves them.
pub fn plan(dir: &Path, stamp: &Stamp, files: &[UpgradableFile]) -> Vec<Planned> {
    files
        .iter()
        .filter_map(|file| {
            let name = file.format.file;
            match prepare(file, &dir.join(name), stamp) {
                Ok(candidate) => candidate.map(|candidate| Planned::Upgrade {
                    file: name,
                    from: candidate.from,
                    to: file.format.latest,
                    original: candidate.original,
                    upgraded: candidate.text,
                    changes: candidate.changes,
                }),
                Err(error) => Some(Planned::Invalid { file: name, error }),
            }
        })
        .collect()
}

/// `None` when the file is missing or already current.
fn prepare(file: &UpgradableFile, path: &Path, stamp: &Stamp) -> Result<Option<Candidate>, String> {
    let Some(original) = atomic_file::read(path).map_err(|error| error.to_string())? else {
        return Ok(None);
    };
    let original = String::from_utf8(original).map_err(|_| NOT_UTF8.to_owned())?;
    let migrated = migrate(&original, file.format, stamp).map_err(|error| error.to_string())?;
    let Some(upgraded) = migrated.text else {
        return Ok(None);
    };
    (file.validate)(path, &upgraded).map_err(|error| format!("{WOULD_NOT_LOAD}: {error}"))?;
    Ok(Some(Candidate {
        revision: atomic_file::revision_of(Some(original.as_bytes())),
        original,
        from: migrated.from,
        text: upgraded,
        changes: migrated.changes,
    }))
}

impl Upgrader<'_> {
    fn run(mut self, dir: &Path, files: &[UpgradableFile]) -> UpgradeReport {
        let mut outcomes = Vec::new();
        let mut pending = Vec::new();
        for file in files {
            let path = dir.join(file.format.file);
            match prepare(file, &path, self.stamp) {
                Ok(Some(candidate)) => pending.push((file, path, candidate)),
                Ok(None) => {}
                Err(error) => outcomes.push(Outcome::Invalid {
                    file: file.format.file,
                    error,
                }),
            }
        }
        if pending
            .iter()
            .any(|(file, _, candidate)| candidate.from < file.format.latest)
        {
            self.guard();
        }
        outcomes.extend(
            pending
                .into_iter()
                .filter_map(|(file, path, candidate)| self.settle(file, &path, candidate, true)),
        );
        UpgradeReport { outcomes }
    }

    fn guard(&mut self) -> &Guard {
        let state_dir = self.state_dir;
        self.guard.get_or_insert_with(|| Guard::take(state_dir))
    }

    /// Writes `candidate` if this process may, reading the file once more
    /// when it changed after it was read.
    fn settle(
        &mut self,
        file: &UpgradableFile,
        path: &Path,
        candidate: Candidate,
        retry: bool,
    ) -> Option<Outcome> {
        let (name, latest) = (file.format.file, file.format.latest);
        if candidate.from < latest {
            match self.guard() {
                Guard::Acquired { .. } | Guard::Held => {}
                Guard::Busy(holders) => {
                    return Some(Outcome::Blocked {
                        file: name,
                        from: candidate.from,
                        latest,
                        holders: holders.clone(),
                    });
                }
                Guard::Unavailable(error) => {
                    return Some(Outcome::Failed {
                        file: name,
                        error: error.clone(),
                    });
                }
            }
        }
        match self.commit(name, path, &candidate) {
            Ok(backup) => Some(Outcome::Upgraded {
                file: name,
                from: candidate.from,
                to: latest,
                backup,
                changes: candidate.changes,
            }),
            Err(AtomicFileError::Conflict { .. }) if retry => {
                match prepare(file, path, self.stamp) {
                    Ok(Some(again)) => self.settle(file, path, again, false),
                    Ok(None) => None,
                    Err(error) => Some(Outcome::Invalid { file: name, error }),
                }
            }
            Err(error) => Some(Outcome::Failed {
                file: name,
                error: error.to_string(),
            }),
        }
    }

    fn commit(
        &self,
        file: &'static str,
        path: &Path,
        candidate: &Candidate,
    ) -> Result<PathBuf, AtomicFileError> {
        atomic_file::commit_after(path, &candidate.revision, candidate.text.as_bytes(), || {
            back_up_config(
                self.state_dir,
                self.scope,
                file,
                candidate.from,
                candidate.original.as_bytes(),
                Timestamp::now(),
            )
            .map_err(|error| io::Error::other(format!("{BACKUP_FAILED}: {error}")))
        })
        .map(|(_, backup)| backup)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::slice;

    use caudra_storage::backups::BackupScope;
    use caudra_storage::id::CaudraId;
    use caudra_storage::projects::project_id;
    use caudra_storage::sessions::SessionLease;
    use caudra_storage::{StateDir, hold_instance, try_instance_lock};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        Outcome, Planned, UpgradableFile, UpgradeReport, plan, upgrade_global, upgrade_holding,
    };
    use crate::migration::{Change, ConfigFormat, MigrationError, Step, Upgrade, read, test_stamp};

    const BEHIND_FILE: &str = "behind.toml";
    const UNSTAMPED_FILE: &str = "unstamped.toml";
    const LEGACY: &str = "# mine\n\nold = 1\n";
    const OTHER_LEGACY: &str = "old = 2\n";
    const UNSTAMPED: &str = "kept = true\n";
    const CURRENT: &str = "version = 2\nnew = 1\n";
    const STAMPED: &str = "version = 1\nkept = true\n";
    const REJECTED: &str = "rejected by its parser";
    const RENAMED: &str = "renamed `old` to `new`";
    const BACKUPS: &str = "backups/config/global";
    const PROJECT_BACKUPS: &str = "backups/config/projects";
    #[cfg(unix)]
    const BACKUP_MODE: u32 = 0o600;
    #[cfg(unix)]
    const READ_ONLY: u32 = 0o444;
    #[cfg(unix)]
    const MODE_BITS: u32 = 0o777;

    const BEHIND: ConfigFormat = ConfigFormat {
        file: BEHIND_FILE,
        latest: 2,
        steps: &[Step {
            from: 1,
            why: None,
            apply: rename_old,
        }],
        requires_version: false,
    };
    const UNSTAMPED_FORMAT: ConfigFormat = ConfigFormat {
        file: UNSTAMPED_FILE,
        latest: 1,
        steps: &[],
        requires_version: false,
    };
    const ACCEPTED: &[UpgradableFile] = &[
        UpgradableFile {
            format: &BEHIND,
            validate: accepts,
        },
        UpgradableFile {
            format: &UNSTAMPED_FORMAT,
            validate: accepts,
        },
    ];
    const REJECTING: &[UpgradableFile] = &[UpgradableFile {
        format: &BEHIND,
        validate: rejects,
    }];
    const EDITED_ONCE: &[UpgradableFile] = &[UpgradableFile {
        format: &BEHIND,
        validate: edits_once,
    }];

    thread_local! {
        static EDIT: RefCell<Option<(PathBuf, &'static str)>> = const { RefCell::new(None) };
    }

    fn rename_old(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.rename_key(&[], "old", "new")
    }

    fn accepts(_: &Path, _: &str) -> Result<(), String> {
        Ok(())
    }

    fn rejects(_: &Path, _: &str) -> Result<(), String> {
        Err(REJECTED.to_owned())
    }

    /// Stands in for an editor saving the file between the read and the write.
    fn edits_once(_: &Path, _: &str) -> Result<(), String> {
        if let Some((path, contents)) = EDIT.take() {
            fs::write(path, contents).unwrap();
        }
        Ok(())
    }

    struct Fixture {
        config: TempDir,
        state: TempDir,
        state_dir: StateDir,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Self {
            let config = TempDir::new().unwrap();
            for (name, contents) in files {
                fs::write(config.path().join(name), contents).unwrap();
            }
            let state = TempDir::new().unwrap();
            let state_dir = StateDir::from_path(state.path().to_path_buf());
            Self {
                config,
                state,
                state_dir,
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.config.path().join(name)
        }

        fn contents(&self, name: &str) -> String {
            fs::read_to_string(self.path(name)).unwrap()
        }

        fn upgrade(&self, files: &[UpgradableFile]) -> UpgradeReport {
            upgrade_global(self.config.path(), &self.state_dir, &test_stamp(), files)
        }

        fn backups(&self) -> Vec<PathBuf> {
            fs::read_dir(self.state.path().join(BACKUPS))
                .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
                .unwrap_or_default()
        }

        fn upgraded(&self, original: &str) -> String {
            crate::migration::migrate(original, &BEHIND, &test_stamp())
                .unwrap()
                .text
                .unwrap()
        }
    }

    fn renamed() -> Vec<Change> {
        vec![Change {
            what: RENAMED.to_owned(),
        }]
    }

    fn blocked(holders: Vec<CaudraId>) -> Outcome {
        Outcome::Blocked {
            file: BEHIND_FILE,
            from: 1,
            latest: 2,
            holders,
        }
    }

    #[test]
    fn a_file_behind_is_backed_up_byte_for_byte_then_upgraded() {
        let fixture = Fixture::new(&[(BEHIND_FILE, LEGACY)]);

        let report = fixture.upgrade(ACCEPTED);

        let [backup] = fixture.backups().try_into().unwrap();
        assert_eq!(
            report.outcomes,
            [Outcome::Upgraded {
                file: BEHIND_FILE,
                from: 1,
                to: 2,
                backup: backup.clone(),
                changes: renamed(),
            }]
        );
        assert_eq!(fs::read_to_string(&backup).unwrap(), LEGACY);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&backup).unwrap().permissions().mode() & MODE_BITS,
            BACKUP_MODE
        );
        assert_eq!(fixture.contents(BEHIND_FILE), fixture.upgraded(LEGACY));
    }

    #[test]
    fn current_and_missing_files_touch_nothing_and_take_no_lock() {
        let fixture = Fixture::new(&[(BEHIND_FILE, CURRENT), (UNSTAMPED_FILE, STAMPED)]);

        assert_eq!(fixture.upgrade(ACCEPTED), UpgradeReport::default());

        assert_eq!(fixture.contents(BEHIND_FILE), CURRENT);
        assert_eq!(fixture.contents(UNSTAMPED_FILE), STAMPED);
        assert_eq!(fs::read_dir(fixture.state.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_is_written_through_and_stays_a_link() {
        let fixture = Fixture::new(&[]);
        let target = fixture.state.path().join(BEHIND_FILE);
        fs::write(&target, LEGACY).unwrap();
        symlink(&target, fixture.path(BEHIND_FILE)).unwrap();

        let report = fixture.upgrade(ACCEPTED);

        assert!(matches!(report.outcomes[..], [Outcome::Upgraded { .. }]));
        assert!(
            fs::symlink_metadata(fixture.path(BEHIND_FILE))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            fixture.upgraded(LEGACY)
        );
    }

    #[test]
    fn a_file_edited_during_the_upgrade_is_read_again_once() {
        let fixture = Fixture::new(&[(BEHIND_FILE, LEGACY)]);
        EDIT.set(Some((fixture.path(BEHIND_FILE), OTHER_LEGACY)));

        let report = fixture.upgrade(EDITED_ONCE);

        let [backup] = fixture.backups().try_into().unwrap();
        assert!(matches!(report.outcomes[..], [Outcome::Upgraded { .. }]));
        assert_eq!(fs::read_to_string(backup).unwrap(), OTHER_LEGACY);
        assert_eq!(
            fixture.contents(BEHIND_FILE),
            fixture.upgraded(OTHER_LEGACY)
        );
    }

    #[test]
    fn a_failed_validation_leaves_the_original_untouched() {
        let fixture = Fixture::new(&[(BEHIND_FILE, LEGACY)]);

        let report = fixture.upgrade(REJECTING);

        assert!(
            matches!(&report.outcomes[..], [Outcome::Invalid { file: BEHIND_FILE, error }] if error.contains(REJECTED)),
            "{report:?}"
        );
        assert_eq!(fixture.contents(BEHIND_FILE), LEGACY);
        assert!(fixture.backups().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_file_fails_and_still_loads_upgraded() {
        let fixture = Fixture::new(&[(BEHIND_FILE, LEGACY)]);
        let path = fixture.path(BEHIND_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(READ_ONLY)).unwrap();

        let report = fixture.upgrade(ACCEPTED);

        assert!(
            matches!(
                report.outcomes[..],
                [Outcome::Failed {
                    file: BEHIND_FILE,
                    ..
                }]
            ),
            "{report:?}"
        );
        assert_eq!(fixture.contents(BEHIND_FILE), LEGACY);
        assert!(fixture.backups().is_empty());
        let loaded = read(&path, &BEHIND).unwrap().unwrap();
        assert_eq!((loaded.from, loaded.migrated), (1, true));
    }

    #[test_case(true ; "a_session_lease")]
    #[test_case(false ; "an_instance_hold")]
    fn another_running_caudra_blocks_a_format_change_but_not_a_stamp(lease: bool) {
        let fixture = Fixture::new(&[(BEHIND_FILE, LEGACY), (UNSTAMPED_FILE, UNSTAMPED)]);
        let id = CaudraId::generate();
        let _lease = lease.then(|| SessionLease::acquire(&fixture.state_dir, id).unwrap());
        let _hold = (!lease).then(|| hold_instance(&fixture.state_dir).unwrap());

        let report = fixture.upgrade(ACCEPTED);

        let holders = if lease { vec![id] } else { Vec::new() };
        let [
            first,
            Outcome::Upgraded {
                file,
                from: 1,
                to: 1,
                backup,
                ..
            },
        ] = &report.outcomes[..]
        else {
            panic!("{report:?}");
        };
        assert_eq!(*first, blocked(holders));
        assert_eq!(*file, UNSTAMPED_FILE);
        assert_eq!(fixture.contents(BEHIND_FILE), LEGACY);
        assert_eq!(fixture.backups(), slice::from_ref(backup));
        assert_eq!(fs::read_to_string(backup).unwrap(), UNSTAMPED);
        assert!(fixture.contents(UNSTAMPED_FILE).contains(UNSTAMPED));
    }

    #[test]
    fn a_caller_holding_the_lock_upgrades_a_project_file_into_its_own_backups() {
        let fixture = Fixture::new(&[(BEHIND_FILE, LEGACY)]);
        let project = TempDir::new().unwrap();
        let lock = try_instance_lock(&fixture.state_dir).unwrap().unwrap();

        let report = upgrade_holding(
            fixture.config.path(),
            &fixture.state_dir,
            BackupScope::Project(project.path()),
            &test_stamp(),
            ACCEPTED,
            &lock,
        );

        let [Outcome::Upgraded { backup, .. }] = &report.outcomes[..] else {
            panic!("{report:?}");
        };
        let projects = fixture
            .state
            .path()
            .join(PROJECT_BACKUPS)
            .join(project_id(project.path()));
        assert_eq!(backup.parent(), Some(projects.as_path()));
        assert_eq!(fs::read_to_string(backup).unwrap(), LEGACY);
        assert_eq!(fixture.contents(BEHIND_FILE), fixture.upgraded(LEGACY));
    }

    #[test]
    fn a_plan_writes_nothing_and_skips_current_files() {
        let fixture = Fixture::new(&[(BEHIND_FILE, LEGACY), (UNSTAMPED_FILE, STAMPED)]);

        let planned = plan(fixture.config.path(), &test_stamp(), ACCEPTED);

        assert_eq!(
            planned,
            [Planned::Upgrade {
                file: BEHIND_FILE,
                from: 1,
                to: 2,
                original: LEGACY.to_owned(),
                upgraded: fixture.upgraded(LEGACY),
                changes: renamed(),
            }]
        );
        assert_eq!(fixture.contents(BEHIND_FILE), LEGACY);
        assert_eq!(fs::read_dir(fixture.state.path()).unwrap().count(), 0);
    }
}
