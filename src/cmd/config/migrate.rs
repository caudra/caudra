//! `caudra config migrate`: upgrades the global config files, and the
//! project's with `--project`, to this Caudra's format. It rewrites nothing
//! while another Caudra runs, since an older one would refuse an upgraded
//! file on its next reload, and `--dry-run` writes nothing at all.

use std::env;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};
use similar::TextDiff;

use caudra_config::PROJECT_DIR;
use caudra_config::config_file;
use caudra_config::migration::disk::{
    Outcome, Planned, UpgradableFile, UpgradeReport, plan, upgrade_holding,
};
use caudra_config::migration::{Change, Stamp};
use caudra_storage::backups::BackupScope;
use caudra_storage::sessions::describe_open_sessions;
use caudra_storage::{StateDir, try_instance_lock};

use crate::startup::{GLOBAL_FILES, PROJECT_FILES};

const DIFF_CONTEXT: usize = 3;
const OTHERS_RUNNING: &str = "no config file was changed, because other Caudra processes run";
const STILL_RUNNING: &str = "other Caudra processes are running";
const NOT_UPGRADED: &str = "not upgraded";
const ALREADY_CURRENT: &str = "already current";
const UPGRADED: &str = "Upgraded";
const WOULD_UPGRADE: &str = "Would upgrade";
const NOTHING_WRITTEN: &str = "Nothing was written.";
const NO_FILES: &str = "No config files to upgrade.";

/// A directory of config files the command upgrades.
struct Target<'a> {
    dir: PathBuf,
    scope: BackupScope<'a>,
    files: &'a [UpgradableFile],
}

/// What a run printed, and whether every file it found ended up current.
#[derive(Debug)]
struct Migration {
    report: String,
    complete: bool,
}

#[derive(Default)]
struct Tally {
    changed: usize,
    current: usize,
    failed: usize,
}

pub fn run(project: bool, dry_run: bool) -> Result<ExitCode> {
    let config_dir = config_file::resolve_config_dir()?;
    let cwd = env::current_dir().context("read the current directory")?;
    let mut targets = vec![Target {
        dir: config_dir,
        scope: BackupScope::Global,
        files: GLOBAL_FILES,
    }];
    if project {
        targets.push(Target {
            dir: cwd.join(PROJECT_DIR),
            scope: BackupScope::Project(&cwd),
            files: PROJECT_FILES,
        });
    }
    let stamp = Stamp::now();
    let migration = if dry_run {
        preview(&targets, &stamp)
    } else {
        let state_dir = StateDir::resolve().context("resolve data directory")?;
        upgrade(&targets, &state_dir, &stamp)?
    };
    print!("{}", migration.report);
    Ok(if migration.complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Rewrites every file behind its format, holding the instance lock for the
/// whole run so no Caudra starts halfway through.
fn upgrade(targets: &[Target<'_>], state_dir: &StateDir, stamp: &Stamp) -> Result<Migration> {
    let Some(lock) =
        try_instance_lock(state_dir).context("check for other running Caudra processes")?
    else {
        bail!("{OTHERS_RUNNING}; {}", describe_open_sessions(state_dir));
    };
    let mut report = String::new();
    let mut tally = Tally::default();
    for target in targets {
        let upgraded = upgrade_holding(
            &target.dir,
            state_dir,
            target.scope,
            stamp,
            target.files,
            &lock,
        );
        write_upgraded(&mut report, &mut tally, target, &upgraded);
    }
    report.push_str(&tally.summary(false));
    Ok(Migration {
        report,
        complete: tally.failed == 0,
    })
}

fn write_upgraded(
    out: &mut String,
    tally: &mut Tally,
    target: &Target<'_>,
    report: &UpgradeReport,
) {
    for file in target.files {
        let path = target.dir.join(file.format.file);
        match report
            .outcomes
            .iter()
            .find(|outcome| outcome.file() == file.format.file)
        {
            Some(Outcome::Upgraded {
                from,
                to,
                backup,
                changes,
                ..
            }) => {
                tally.changed += 1;
                write_change(out, &path, *from, *to, changes);
                let _ = writeln!(out, "  backup: {}", backup.display());
            }
            Some(Outcome::Blocked { .. }) => tally.note_failure(out, &path, STILL_RUNNING),
            Some(Outcome::Invalid { error, .. } | Outcome::Failed { error, .. }) => {
                tally.note_failure(out, &path, error);
            }
            None => tally.note_current(out, &path, file),
        }
    }
}

/// What [`upgrade`] would write, with a diff per file, without the lock.
fn preview(targets: &[Target<'_>], stamp: &Stamp) -> Migration {
    let mut report = String::new();
    let mut tally = Tally::default();
    for target in targets {
        let planned = plan(&target.dir, stamp, target.files);
        for file in target.files {
            let path = target.dir.join(file.format.file);
            match planned
                .iter()
                .find(|planned| planned.file() == file.format.file)
            {
                Some(Planned::Upgrade {
                    from,
                    to,
                    original,
                    upgraded,
                    changes,
                    ..
                }) => {
                    tally.changed += 1;
                    write_change(&mut report, &path, *from, *to, changes);
                    let shown = path.display().to_string();
                    let _ = write!(
                        report,
                        "{}",
                        TextDiff::from_lines(original, upgraded)
                            .unified_diff()
                            .context_radius(DIFF_CONTEXT)
                            .header(&shown, &shown)
                    );
                }
                Some(Planned::Invalid { error, .. }) => {
                    tally.note_failure(&mut report, &path, error)
                }
                None => tally.note_current(&mut report, &path, file),
            }
        }
    }
    report.push_str(&tally.summary(true));
    Migration {
        report,
        complete: tally.failed == 0,
    }
}

fn write_change(out: &mut String, path: &Path, from: u32, to: u32, changes: &[Change]) {
    let path = path.display();
    let _ = if from == to {
        writeln!(out, "{path}: stamped with version {to}")
    } else {
        writeln!(out, "{path}: version {from} → {to}")
    };
    for change in changes {
        let _ = writeln!(out, "  {}", change.what);
    }
}

impl Tally {
    fn note_failure(&mut self, out: &mut String, path: &Path, error: &str) {
        self.failed += 1;
        let _ = writeln!(out, "{}: {NOT_UPGRADED}: {error}", path.display());
    }

    /// A file the run left alone, which is current when it exists.
    fn note_current(&mut self, out: &mut String, path: &Path, file: &UpgradableFile) {
        if !path.exists() {
            return;
        }
        self.current += 1;
        let _ = writeln!(
            out,
            "{}: {ALREADY_CURRENT} (version {})",
            path.display(),
            file.format.latest
        );
    }

    fn summary(&self, dry_run: bool) -> String {
        if self.changed + self.current + self.failed == 0 {
            return format!("{NO_FILES}\n");
        }
        let verb = if dry_run { WOULD_UPGRADE } else { UPGRADED };
        let parts: Vec<String> = [
            (self.changed > 0).then(|| format!("{verb} {}", files(self.changed))),
            (self.current > 0).then(|| format!("{} {ALREADY_CURRENT}", self.current)),
            (self.failed > 0).then(|| format!("{} {NOT_UPGRADED}", self.failed)),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut line = format!("{}.", parts.join("; "));
        if dry_run {
            let _ = write!(line, " {NOTHING_WRITTEN}");
        }
        line.push('\n');
        line
    }
}

fn files(count: usize) -> String {
    if count == 1 {
        format!("{count} file")
    } else {
        format!("{count} files")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use caudra_config::PROJECT_DIR;
    use caudra_config::migration::disk::UpgradableFile;
    use caudra_config::migration::{ConfigFormat, MigrationError, Step, Upgrade};
    use caudra_storage::StateDir;
    use caudra_storage::backups::BackupScope;
    use caudra_storage::id::CaudraId;
    use caudra_storage::projects::project_id;
    use caudra_storage::sessions::SessionLease;
    use tempfile::TempDir;

    use super::{
        ALREADY_CURRENT, NOTHING_WRITTEN, OTHERS_RUNNING, Stamp, Target, preview, upgrade,
    };

    const BEHIND_FILE: &str = "behind.toml";
    const CURRENT_FILE: &str = "current.toml";
    const LEGACY: &str = "# mine\nold = 1\n";
    const CURRENT: &str = "version = 1\nkept = true\n";
    const RENAMED: &str = "renamed `old` to `new`";
    const OLD_LINE: &str = "old = 1";
    const NEW_LINE: &str = "new = 1";
    const TEST_STAMP_BY: &str = "Caudra 9.9.9";
    const PROJECT_BACKUPS: &str = "backups/config/projects";
    const CLOSE_THESE: &str = "close these open sessions and start again:";
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
    const CURRENT_FORMAT: ConfigFormat = ConfigFormat {
        file: CURRENT_FILE,
        latest: 1,
        steps: &[],
        requires_version: false,
    };
    const FILES: &[UpgradableFile] = &[
        UpgradableFile {
            format: &BEHIND,
            validate: accepts,
        },
        UpgradableFile {
            format: &CURRENT_FORMAT,
            validate: accepts,
        },
    ];

    fn rename_old(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
        upgrade.rename_key(&[], "old", "new")
    }

    fn accepts(_: &Path, _: &str) -> Result<(), String> {
        Ok(())
    }

    struct Fixture {
        project: TempDir,
        dir: PathBuf,
        state: TempDir,
        state_dir: StateDir,
    }

    impl Fixture {
        fn new() -> Self {
            let project = TempDir::new().unwrap();
            let dir = project.path().join(PROJECT_DIR);
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join(BEHIND_FILE), LEGACY).unwrap();
            fs::write(dir.join(CURRENT_FILE), CURRENT).unwrap();
            let state = TempDir::new().unwrap();
            let state_dir = StateDir::from_path(state.path().to_path_buf());
            Self {
                project,
                dir,
                state,
                state_dir,
            }
        }

        fn targets(&self) -> Vec<Target<'_>> {
            vec![Target {
                dir: self.dir.clone(),
                scope: BackupScope::Project(self.project.path()),
                files: FILES,
            }]
        }

        fn behind(&self) -> String {
            fs::read_to_string(self.dir.join(BEHIND_FILE)).unwrap()
        }

        fn backups(&self) -> PathBuf {
            self.state
                .path()
                .join(PROJECT_BACKUPS)
                .join(project_id(self.project.path()))
        }
    }

    fn stamp() -> Stamp {
        Stamp {
            by: TEST_STAMP_BY.to_owned(),
            on: jiff::civil::date(2026, 10, 20),
        }
    }

    #[test]
    fn a_dry_run_prints_the_changes_and_a_diff_and_writes_nothing() {
        let fixture = Fixture::new();

        let migration = preview(&fixture.targets(), &stamp());

        let report = migration.report;
        assert!(migration.complete, "{report}");
        assert!(
            report.contains(&format!("{BEHIND_FILE}: version 1 → 2\n  {RENAMED}\n")),
            "{report}"
        );
        assert!(
            report.contains(&format!("-{OLD_LINE}")) && report.contains(&format!("+{NEW_LINE}")),
            "{report}"
        );
        assert!(
            report.contains(&format!("{CURRENT_FILE}: {ALREADY_CURRENT} (version 1)")),
            "{report}"
        );
        assert!(
            report.ends_with(&format!(
                "Would upgrade 1 file; 1 {ALREADY_CURRENT}. {NOTHING_WRITTEN}\n"
            )),
            "{report}"
        );
        assert_eq!(fixture.behind(), LEGACY);
        assert_eq!(fs::read_dir(fixture.state.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_dry_run_works_while_a_session_is_open() {
        let fixture = Fixture::new();
        let _lease = SessionLease::acquire(&fixture.state_dir, CaudraId::generate()).unwrap();

        assert!(preview(&fixture.targets(), &stamp()).complete);
    }

    #[test]
    fn a_project_file_is_upgraded_and_backed_up_under_its_project() {
        let fixture = Fixture::new();

        let migration = upgrade(&fixture.targets(), &fixture.state_dir, &stamp()).unwrap();

        let [backup] = fs::read_dir(fixture.backups())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let report = migration.report;
        assert!(migration.complete, "{report}");
        assert!(
            report.contains(&format!("  backup: {}\n", backup.display())),
            "{report}"
        );
        assert!(
            report.ends_with(&format!("Upgraded 1 file; 1 {ALREADY_CURRENT}.\n")),
            "{report}"
        );
        assert_eq!(fs::read_to_string(backup).unwrap(), LEGACY);
        assert!(fixture.behind().contains(NEW_LINE), "{}", fixture.behind());
    }

    #[test]
    fn an_open_session_blocks_every_write_until_it_closes() {
        let fixture = Fixture::new();
        let id = CaudraId::generate();
        let lease = SessionLease::acquire(&fixture.state_dir, id).unwrap();

        let error = upgrade(&fixture.targets(), &fixture.state_dir, &stamp())
            .unwrap_err()
            .to_string();

        assert!(error.starts_with(OTHERS_RUNNING), "{error}");
        assert!(
            error.contains(CLOSE_THESE) && error.contains(&id.to_string()),
            "{error}"
        );
        assert_eq!(fixture.behind(), LEGACY);
        assert!(!fixture.backups().exists());

        drop(lease);
        assert!(
            upgrade(&fixture.targets(), &fixture.state_dir, &stamp())
                .unwrap()
                .complete
        );
        assert_ne!(fixture.behind(), LEGACY);
    }
}
