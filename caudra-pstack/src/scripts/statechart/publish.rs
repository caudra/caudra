//! Where a run's files go: a work folder beside the output folder, which then replaces the whole
//! output folder by renames, so a reader never sees half a run. The run refuses an output folder
//! with files it did not write, and an input that changed while it read it.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::LazyLock;
use std::time::SystemTime;

use regex::Regex;

use super::cli::CannotFinish;

/// The output folder's subfolder for what the checks read and wrote, and the regions proof.
pub const CHECKS: &str = "checks";
const OLD_SUFFIX: &str = "-old";
/// How many names a run tries for its work folder before it gives up.
const WORK_ATTEMPTS: usize = 1_000;

/// The names a run writes in its output folder, and what upstream's runs wrote there too.
static OWN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^((chart|bug-\d+(-developer|-layered)?)\.(mmd|png)|stories\.md|summary\.json|checks|tlc|\.DS_Store)$",
    )
    .expect("valid regex")
});

/// A path as the reader named it, and where it resolves.
#[derive(Debug, Clone)]
pub struct Named {
    pub shown: String,
    pub path: PathBuf,
}

impl Named {
    /// `name` in the same folder as this path.
    pub fn sibling(&self, name: &str) -> Self {
        let parent = |path: &Path| path.parent().map(Path::to_path_buf).unwrap_or_default();
        Self {
            shown: parent(Path::new(&self.shown))
                .join(name)
                .display()
                .to_string(),
            path: parent(&self.path).join(name),
        }
    }
}

type Fingerprint = Option<(Option<SystemTime>, Vec<u8>)>;

/// Each input as the run first read it, to tell whether it changed before the run publishes.
#[derive(Debug, Default)]
pub struct Inputs(Vec<(Named, Fingerprint)>);

fn fingerprint(path: &Path) -> Fingerprint {
    let modified = fs::metadata(path).ok()?.modified().ok();
    Some((modified, fs::read(path).ok()?))
}

impl Inputs {
    pub fn add(&mut self, input: &Named) {
        if !self.0.iter().any(|(known, _)| known.path == input.path) {
            self.0.push((input.clone(), fingerprint(&input.path)));
        }
    }

    fn unchanged(&self, out: &Named) -> Result<(), CannotFinish> {
        match self
            .0
            .iter()
            .find(|(input, was)| fingerprint(&input.path) != *was)
        {
            Some((changed, _)) => Err(CannotFinish::new(format!(
                "{} changed while this run read it, so the run left {} as it was. Run the command again.",
                changed.shown, out.shown
            ))),
            None => Ok(()),
        }
    }
}

/// The files in `dir` that no run wrote, sorted. A folder that does not exist has none.
fn foreign(dir: &Named) -> Result<Vec<String>, CannotFinish> {
    let entries = match fs::read_dir(&dir.path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(CannotFinish::new(format!(
                "cannot use {} as the output folder: {error}",
                dir.shown
            )));
        }
    };
    let mut files: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !OWN.is_match(name))
        .collect();
    files.sort();
    Ok(files)
}

fn refuse(out: &Named, files: &[String]) -> Result<(), CannotFinish> {
    let Some((first, others)) = files.split_first() else {
        return Ok(());
    };
    let has = if others.is_empty() {
        format!("a file that this command did not write: {first}")
    } else {
        format!(
            "{} files that this command did not write, such as {first}",
            others.len() + 1
        )
    };
    Err(CannotFinish::new(format!(
        "{} has {has}. A run puts its files in place of the whole output folder, so pass --out with a new folder.",
        out.shown
    )))
}

/// Refuses an output folder with files a run did not write.
pub fn refuse_foreign(out: &Named) -> Result<(), CannotFinish> {
    refuse(out, &foreign(out)?)
}

/// The folder a run writes in: hidden beside the output folder, so it renames into place.
pub struct WorkFolder {
    pub folder: Named,
}

impl WorkFolder {
    pub fn create(out: &Named) -> Result<Self, CannotFinish> {
        let cannot = |error: io::Error| {
            CannotFinish::new(format!(
                "cannot make a work folder beside {}: {error}",
                out.shown
            ))
        };
        let base = out
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(parent) = out.path.parent() {
            fs::create_dir_all(parent).map_err(cannot)?;
        }
        for attempt in 0..WORK_ATTEMPTS {
            let folder = out.sibling(&format!(".{base}-{}-{attempt}", process::id()));
            match fs::create_dir(&folder.path) {
                Ok(()) => {
                    fs::create_dir(folder.path.join(CHECKS)).map_err(cannot)?;
                    return Ok(Self { folder });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(cannot(error)),
            }
        }
        Err(cannot(io::Error::from(ErrorKind::AlreadyExists)))
    }

    pub fn path(&self) -> &Path {
        &self.folder.path
    }

    /// Removes the folder, unless the error asks to keep it, and then names it in the error.
    pub fn discard(&self, error: CannotFinish) -> CannotFinish {
        if error.keep_work {
            return error.with(&format!(
                " The files that this run wrote are in {}.",
                self.folder.shown
            ));
        }
        let _ = fs::remove_dir_all(&self.folder.path);
        error
    }

    /// Replaces `out` with this folder, once no input changed and `out` holds only a run's files.
    /// A run that finishes alongside wins or loses whole: `out` is moved aside, checked again,
    /// and only then removed.
    pub fn publish(&self, out: &Named, inputs: &Inputs) -> Result<(), CannotFinish> {
        let old = self.folder.sibling(&format!(
            "{}{OLD_SUFFIX}",
            self.folder
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        let mut aside = false;
        let swapped = self.swap(out, &old, inputs, &mut aside);
        match swapped {
            Ok(()) => {
                if aside {
                    let _ = fs::remove_dir_all(&old.path);
                }
                Ok(())
            }
            Err(error) => {
                let back = !aside || put_back(&old, out);
                let (message, failed) = match error {
                    Swap::Refused(refused) => (refused.message, false),
                    Swap::Io(io) => (format!("cannot replace {}: {io}.", out.shown), true),
                };
                let message = if !back {
                    format!(
                        "{message} This run moved the previous {} to {} and could not put it back.",
                        out.shown, old.shown
                    )
                } else if failed {
                    format!("{message} The run left {} as it was.", out.shown)
                } else {
                    message
                };
                Err(CannotFinish::kept(message))
            }
        }
    }

    fn swap(
        &self,
        out: &Named,
        old: &Named,
        inputs: &Inputs,
        aside: &mut bool,
    ) -> Result<(), Swap> {
        inputs.unchanged(out).map_err(Swap::Refused)?;
        refuse_foreign(out).map_err(Swap::Refused)?;
        loop {
            match fs::rename(&self.folder.path, &out.path) {
                Ok(()) => return Ok(()),
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::DirectoryNotEmpty | ErrorKind::AlreadyExists
                    ) => {}
                Err(error) => return Err(Swap::Io(error)),
            }
            if *aside {
                refuse_foreign(out).map_err(Swap::Refused)?;
                fs::remove_dir_all(&old.path).map_err(Swap::Io)?;
                *aside = false;
            }
            *aside = match fs::rename(&out.path, &old.path) {
                Ok(()) => true,
                Err(error) if error.kind() == ErrorKind::NotFound => false,
                Err(error) => return Err(Swap::Io(error)),
            };
            if *aside {
                refuse(out, &foreign(old).map_err(Swap::Refused)?).map_err(Swap::Refused)?;
            }
        }
    }
}

enum Swap {
    Refused(CannotFinish),
    Io(io::Error),
}

fn put_back(old: &Named, out: &Named) -> bool {
    fs::rename(&old.path, &out.path).is_ok()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    const OUT: &str = "Pay-statechart";
    const CHART: &str = "chart.mmd";
    const NOTES: &str = "notes.md";

    fn named(dir: &TempDir, name: &str) -> Named {
        Named {
            shown: name.to_owned(),
            path: dir.path().join(name),
        }
    }

    fn run(dir: &TempDir, chart: &str) -> Result<(), CannotFinish> {
        let out = named(dir, OUT);
        let work = WorkFolder::create(&out)?;
        fs::write(work.path().join(CHART), chart).expect("the chart writes");
        work.publish(&out, &Inputs::default())
            .map_err(|error| work.discard(error))
    }

    fn entries(path: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(path)
            .expect("the folder reads")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_run_replaces_the_whole_output_folder() {
        let dir = TempDir::new().expect("a temporary folder");
        run(&dir, "first").expect("the first run publishes");
        fs::write(dir.path().join(OUT).join("bug-9.mmd"), "stale").expect("a stale chart");

        run(&dir, "second").expect("the second run publishes");

        assert_eq!(entries(dir.path()), [OUT]);
        assert_eq!(entries(&dir.path().join(OUT)), [CHART, CHECKS]);
        assert_eq!(
            fs::read_to_string(dir.path().join(OUT).join(CHART)).expect("the chart reads"),
            "second"
        );
    }

    #[test]
    fn a_folder_with_a_file_no_run_wrote_is_left_alone() {
        let dir = TempDir::new().expect("a temporary folder");
        let out = dir.path().join(OUT);
        fs::create_dir_all(out.join("tlc")).expect("the folder makes");
        fs::write(out.join(CHART), "kept").expect("a chart");
        fs::write(out.join(".DS_Store"), "").expect("a finder file");
        fs::write(out.join(NOTES), "mine").expect("a note");

        let error = refuse_foreign(&named(&dir, OUT)).expect_err("the folder is refused");

        assert_eq!(
            error.message,
            format!(
                "{OUT} has a file that this command did not write: {NOTES}. A run puts its files in place of the whole output folder, so pass --out with a new folder."
            )
        );
        assert_eq!(entries(&out), [".DS_Store", CHART, NOTES, "tlc"]);
    }

    #[test]
    fn a_changed_input_keeps_the_output_folder_and_names_the_run_files() {
        let dir = TempDir::new().expect("a temporary folder");
        let spec = named(&dir, "Pay.tla");
        fs::write(&spec.path, "one").expect("the spec writes");
        let mut inputs = Inputs::default();
        inputs.add(&spec);
        let out = named(&dir, OUT);
        let work = WorkFolder::create(&out).expect("the work folder makes");
        fs::write(&spec.path, "two, longer").expect("the spec changes");

        let error = work
            .publish(&out, &inputs)
            .map_err(|error| work.discard(error))
            .expect_err("the run does not publish");

        assert!(
            error.message.starts_with(&format!(
                "Pay.tla changed while this run read it, so the run left {OUT} as it was. Run the command again. The files that this run wrote are in ."
            )),
            "{}",
            error.message
        );
        assert!(!out.path.exists());
        assert!(work.path().exists());
    }
}
