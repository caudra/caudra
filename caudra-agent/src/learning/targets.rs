//! The block continual learning keeps in each instruction file it writes.
//!
//! Only the text between the markers is ever rewritten, so what a person wrote
//! around it stays byte for byte. Every write compares the file with the copy
//! the learner was shown first, and a file someone changed since is left alone.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use caudra_config::LearningTarget;
use caudra_storage::checkout;
use caudra_storage::paths::canonicalize_clean;
use caudra_storage::{StorageError, atomic_write, atomic_write_permissions};
use tracing::info;

use crate::agent::{INSTRUCTION_FILES, LOCAL_INSTRUCTION_FILE};

pub const BLOCK_OPEN: &str = "<!-- caudra:learned -->";
pub const BLOCK_CLOSE: &str = "<!-- /caudra:learned -->";
pub const MAX_BULLETS: usize = 12;
pub const MAX_BULLET_BYTES: usize = 240;
const BULLET: &str = "- ";
const GLOBAL_FILE: &str = "AGENTS.md";
const NEW_FILE_MODE: u32 = 0o644;
const PROJECT_HEADING: &str = "## Learned workspace facts";
const LOCAL_HEADING: &str = "## Learned local setup";
const GLOBAL_HEADING: &str = "## Learned user preferences";
const GITIGNORE: &str = ".gitignore";
const GIT: &str = "git";

/// The heading a target's block opens with.
pub fn heading(target: LearningTarget) -> &'static str {
    match target {
        LearningTarget::Project => PROJECT_HEADING,
        LearningTarget::Local => LOCAL_HEADING,
        LearningTarget::Global | LearningTarget::Memory => GLOBAL_HEADING,
    }
}

/// The root of the checkout `cwd` is in, where the project's instruction
/// files live, or `cwd` itself outside a repository.
pub fn project_root(cwd: &Path) -> PathBuf {
    checkout::discover(cwd).map_or_else(|| canonicalize_clean(cwd), |found| found.root)
}

/// Where a file target lives: the project file Caudra already loads at `root`
/// or a new `AGENTS.md`, `AGENTS.local.md` beside it, or the user's own file
/// in `config_dir`. `None` for memory, and for global without a config dir.
pub fn target_path(
    target: LearningTarget,
    root: &Path,
    config_dir: Option<&Path>,
) -> Option<PathBuf> {
    match target {
        LearningTarget::Project => Some(
            INSTRUCTION_FILES
                .iter()
                .map(|name| root.join(name))
                .find(|path| path.is_file())
                .unwrap_or_else(|| root.join(INSTRUCTION_FILES[0])),
        ),
        LearningTarget::Local => Some(root.join(LOCAL_INSTRUCTION_FILE)),
        LearningTarget::Global => config_dir.map(|dir| dir.join(GLOBAL_FILE)),
        LearningTarget::Memory => None,
    }
}

/// One instruction file as it was read, the base every write compares with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    pub target: LearningTarget,
    pub path: PathBuf,
    /// `None` while the file does not exist.
    pub content: Option<String>,
}

/// What a compare-and-write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Wrote,
    /// The file changed since it was read, and was left alone.
    Conflict,
}

impl InstructionFile {
    pub fn read(target: LearningTarget, path: PathBuf) -> io::Result<Self> {
        let content = read_optional(&path)?;
        Ok(Self {
            target,
            path,
            content,
        })
    }

    /// The bullets of the block, in order.
    pub fn bullets(&self) -> Vec<String> {
        self.content.as_deref().map(bullets).unwrap_or_default()
    }

    /// The file with its block holding `bullets`: replaced where it was,
    /// appended where there was none, removed when `bullets` is empty.
    pub fn rewritten(&self, bullets: &[String]) -> String {
        let content = self.content.as_deref().unwrap_or_default();
        let block = render_block(self.target, bullets);
        match (block_span(content), bullets.is_empty()) {
            (Some((start, end)), true) => {
                let head = content[..start].trim_end_matches('\n');
                let tail = content[end..].trim_start_matches('\n');
                match (head.is_empty(), tail.is_empty()) {
                    (true, _) => tail.to_owned(),
                    (false, true) => format!("{head}\n"),
                    (false, false) => format!("{head}\n\n{tail}"),
                }
            }
            (Some((start, end)), false) => {
                format!("{}{block}{}", &content[..start], &content[end..])
            }
            (None, true) => content.to_owned(),
            (None, false) if content.trim().is_empty() => format!("{block}\n"),
            (None, false) => {
                let separator = if content.ends_with('\n') {
                    "\n"
                } else {
                    "\n\n"
                };
                format!("{content}{separator}{block}\n")
            }
        }
    }

    /// Writes `new` unless the file changed since it was read. A file that
    /// did not exist is created with its parent directory.
    pub fn write_if_unchanged(&self, new: &str) -> Result<Written, StorageError> {
        if read_optional(&self.path)? != self.content {
            return Ok(Written::Conflict);
        }
        match &self.content {
            Some(_) => atomic_write(&self.path, new.as_bytes())?,
            None => {
                if let Some(parent) = self.path.parent() {
                    fs::create_dir_all(parent)?;
                }
                atomic_write_permissions(&self.path, new.as_bytes(), NEW_FILE_MODE)?;
            }
        }
        Ok(Written::Wrote)
    }
}

/// Makes sure version control leaves the file at `path` out, by a line in the
/// `.gitignore` beside it, created when there is none, inside a repository or
/// not. Answers whether it wrote one.
pub fn ensure_ignored(path: &Path) -> Result<bool, StorageError> {
    let (Some(root), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return Ok(false);
    };
    if git_ignores(root, name) {
        return Ok(false);
    }
    let gitignore = root.join(GITIGNORE);
    let content = read_optional(&gitignore)?;
    let existing = content.as_deref().unwrap_or_default();
    if existing
        .lines()
        .any(|line| line.trim().trim_start_matches('/') == name)
    {
        return Ok(false);
    }
    let separator = match existing.is_empty() || existing.ends_with('\n') {
        true => "",
        false => "\n",
    };
    let updated = format!("{existing}{separator}{name}\n");
    match content {
        Some(_) => atomic_write(&gitignore, updated.as_bytes())?,
        None => atomic_write_permissions(&gitignore, updated.as_bytes(), NEW_FILE_MODE)?,
    }
    info!(path = %gitignore.display(), ignored = name, "learner added a .gitignore entry");
    Ok(true)
}

/// Whether git already leaves `name` at `root` out, by any of its ignore
/// rules, tracked or not. Outside a repository git has no say.
fn git_ignores(root: &Path, name: &str) -> bool {
    checkout::discover(root).is_some()
        && Command::new(GIT)
            .args(["check-ignore", "-q", "--no-index", "--", name])
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

/// The bullets of the block in `content`.
pub fn bullets(content: &str) -> Vec<String> {
    let Some((start, end)) = block_span(content) else {
        return Vec::new();
    };
    content[start + BLOCK_OPEN.len()..end - BLOCK_CLOSE.len()]
        .lines()
        .filter_map(|line| line.trim().strip_prefix(BULLET))
        .map(|bullet| bullet.trim().to_owned())
        .filter(|bullet| !bullet.is_empty())
        .collect()
}

fn render_block(target: LearningTarget, bullets: &[String]) -> String {
    let mut block = format!("{BLOCK_OPEN}\n{}\n\n", heading(target));
    for bullet in bullets {
        block.push_str(BULLET);
        block.push_str(bullet);
        block.push('\n');
    }
    block.push_str(BLOCK_CLOSE);
    block
}

/// From the start of the opening marker to the end of the closing one.
fn block_span(content: &str) -> Option<(usize, usize)> {
    let start = content.find(BLOCK_OPEN)?;
    let close = content[start..].find(BLOCK_CLOSE)? + start;
    Some((start, close + BLOCK_CLOSE.len()))
}

fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const HAND_WRITTEN: &str = "# Project\n\nKeep this exactly.  \n\n## Testing\n- run make test\n";
    const TAIL: &str = "\n\n## After\ntrailing text\n";
    const FIRST: &str = "Use nextest, not cargo test";
    const SECOND: &str = "Docs regenerate with make gen-docs";
    const EDITED_ELSEWHERE: &str = "someone else's edit\n";
    const IGNORE_LINE: &str = "AGENTS.local.md\n";
    const ANCHORED: &str = "/AGENTS.local.md";
    const OTHER_RULES: &str = "target/\n*.log\n";
    const UNTERMINATED: &str = "target/";
    const NO_EXCLUDES: &str = "/dev/null";
    const GIT_EXCLUDE: &str = ".git/info/exclude";

    fn file(content: Option<&str>) -> InstructionFile {
        InstructionFile {
            target: LearningTarget::Project,
            path: PathBuf::new(),
            content: content.map(str::to_owned),
        }
    }

    fn learned(bullets: &[&str]) -> Vec<String> {
        bullets.iter().map(|bullet| (*bullet).to_owned()).collect()
    }

    #[test]
    fn a_block_round_trips_and_keeps_the_text_around_it() {
        let appended = file(Some(HAND_WRITTEN)).rewritten(&learned(&[FIRST]));
        assert!(appended.starts_with(HAND_WRITTEN));
        let surrounded = format!("{appended}{TAIL}");

        let replaced = file(Some(&surrounded)).rewritten(&learned(&[FIRST, SECOND]));

        assert!(replaced.starts_with(HAND_WRITTEN));
        assert!(replaced.ends_with(TAIL));
        assert_eq!(bullets(&replaced), learned(&[FIRST, SECOND]));
    }

    #[test_case(None ; "a_missing_file")]
    #[test_case(Some("") ; "an_empty_file")]
    fn a_new_block_is_the_whole_file(content: Option<&str>) {
        let written = file(content).rewritten(&learned(&[FIRST]));
        assert!(written.starts_with(BLOCK_OPEN));
        assert_eq!(bullets(&written), learned(&[FIRST]));
    }

    #[test]
    fn no_bullets_removes_the_block_only() {
        let with_block = file(Some(HAND_WRITTEN)).rewritten(&learned(&[FIRST]));
        let removed = file(Some(&with_block)).rewritten(&[]);
        assert_eq!(removed, HAND_WRITTEN);
    }

    #[test]
    fn a_file_changed_since_it_was_read_is_left_alone() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(INSTRUCTION_FILES[0]);
        let read = InstructionFile::read(LearningTarget::Project, path.clone()).unwrap();
        fs::write(&path, EDITED_ELSEWHERE).unwrap();

        let written = read.write_if_unchanged(&read.rewritten(&learned(&[FIRST])));

        assert_eq!(written.unwrap(), Written::Conflict);
        assert_eq!(fs::read_to_string(&path).unwrap(), EDITED_ELSEWHERE);
    }

    #[test]
    fn a_missing_file_is_created() {
        let dir = TempDir::new().unwrap();
        let path = target_path(LearningTarget::Project, dir.path(), None).unwrap();
        let read = InstructionFile::read(LearningTarget::Project, path.clone()).unwrap();

        let written = read.write_if_unchanged(&read.rewritten(&learned(&[FIRST])));

        assert_eq!(written.unwrap(), Written::Wrote);
        assert_eq!(
            bullets(&fs::read_to_string(&path).unwrap()),
            learned(&[FIRST])
        );
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new(GIT)
            .args(args)
            .current_dir(root)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A repository that answers only to its own rules, whatever the
    /// machine's global excludes say.
    fn repository() -> TempDir {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "--quiet"]);
        git(dir.path(), &["config", "core.excludesFile", NO_EXCLUDES]);
        dir
    }

    fn local(dir: &TempDir) -> PathBuf {
        target_path(LearningTarget::Local, dir.path(), None).unwrap()
    }

    fn gitignore(dir: &TempDir) -> Option<String> {
        read_optional(&dir.path().join(GITIGNORE)).unwrap()
    }

    #[test_case(None, IGNORE_LINE ; "a_missing_file_is_created")]
    #[test_case(Some(OTHER_RULES), &format!("{OTHER_RULES}{IGNORE_LINE}") ; "a_file_gains_the_line")]
    #[test_case(Some(UNTERMINATED), &format!("{UNTERMINATED}\n{IGNORE_LINE}") ; "a_last_line_is_ended_first")]
    fn the_local_file_is_added_to_gitignore(before: Option<&str>, after: &str) {
        let dir = TempDir::new().unwrap();
        if let Some(before) = before {
            fs::write(dir.path().join(GITIGNORE), before).unwrap();
        }
        assert!(ensure_ignored(&local(&dir)).unwrap());
        assert_eq!(gitignore(&dir).as_deref(), Some(after));
    }

    #[test_case(LOCAL_INSTRUCTION_FILE ; "by_name")]
    #[test_case(ANCHORED ; "anchored")]
    fn a_gitignore_that_names_it_is_left_alone(line: &str) {
        let dir = TempDir::new().unwrap();
        let before = format!("{OTHER_RULES}{line}\n");
        fs::write(dir.path().join(GITIGNORE), &before).unwrap();
        assert!(!ensure_ignored(&local(&dir)).unwrap());
        assert_eq!(gitignore(&dir), Some(before));
    }

    #[test]
    fn a_file_git_already_ignores_needs_no_gitignore() {
        let dir = repository();
        let exclude = dir.path().join(GIT_EXCLUDE);
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        fs::write(&exclude, IGNORE_LINE).unwrap();
        assert!(!ensure_ignored(&local(&dir)).unwrap());
        assert_eq!(gitignore(&dir), None);
    }

    #[test]
    fn a_repository_without_a_rule_gets_one() {
        let dir = repository();
        assert!(ensure_ignored(&local(&dir)).unwrap());
        assert_eq!(gitignore(&dir).as_deref(), Some(IGNORE_LINE));
    }

    #[test]
    fn the_project_file_is_the_one_already_loaded() {
        let dir = TempDir::new().unwrap();
        let claude = dir.path().join(INSTRUCTION_FILES[1]);
        fs::write(&claude, HAND_WRITTEN).unwrap();
        assert_eq!(
            target_path(LearningTarget::Project, dir.path(), None),
            Some(claude)
        );
    }
}
