use std::borrow::Cow;
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::cli::{HerdrCli, HerdrError};
use super::env::HerdrEnv;

pub const SOURCE: &str = "custom:caudra";
pub const AGENT: &str = "caudra";
/// What a restored pane types into its shell. Herdr accepts only a bare command
/// name there, so this is the name Caudra is installed under.
pub const RESUME_COMMAND: &str = "caudra";
/// How Herdr refuses to remove a workspace it did not open as a worktree.
pub const NOT_LINKED_WORKTREE: &str = "not_linked_worktree";
const SESSION_FLAG: &str = "--session";
const RESUME_SEPARATOR: &str = "--";
const MODEL_TOKEN: &str = "model";
const CONTEXT_TOKEN: &str = "context";
const REPORT_TIMEOUT: Duration = Duration::from_secs(1);
/// Herdr runs git for these, which takes as long as the checkout does.
const WORKTREE_TIMEOUT: Duration = Duration::from_secs(600);
/// Opening a workspace or a pane, or typing into one.
const LAYOUT_TIMEOUT: Duration = Duration::from_secs(15);
const SPLIT_DIRECTION: &str = "right";
/// Linux appends this when a running executable has been replaced or removed.
const REPLACED_EXE_SUFFIX: &str = " (deleted)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
}

impl AgentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

pub struct AgentReport<'a> {
    pub state: AgentState,
    pub message: Option<&'a str>,
    /// What Herdr runs to bring the agent back once it restores the pane.
    pub resume: Option<&'a [String]>,
}

/// What the Herdr sidebar shows for the pane besides its state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneMetadata<'a> {
    pub title: Cow<'a, str>,
    pub model: Cow<'a, str>,
    pub context_percent: Option<u32>,
}

impl PaneMetadata<'_> {
    pub fn into_owned(self) -> PaneMetadata<'static> {
        PaneMetadata {
            title: Cow::Owned(self.title.into_owned()),
            model: Cow::Owned(self.model.into_owned()),
            context_percent: self.context_percent,
        }
    }
}

/// The command that reopens `session`, or starts afresh without one. A session
/// that was never saved has nothing to reopen.
pub fn resume_argv(session: Option<&str>) -> Vec<String> {
    let mut argv = vec![RESUME_COMMAND.to_owned()];
    if let Some(session) = session {
        argv.extend([SESSION_FLAG.to_owned(), session.to_owned()]);
    }
    argv
}

/// The pane Caudra runs in, addressed as the agent Herdr tracks there.
#[derive(Clone, Debug)]
pub struct HerdrPane {
    cli: HerdrCli,
    pane_id: String,
}

impl HerdrPane {
    pub fn new(env: &HerdrEnv) -> Self {
        Self {
            cli: env.cli(),
            pane_id: env.pane_id.clone(),
        }
    }

    pub fn report_agent(&self, report: &AgentReport<'_>, seq: u64) -> Result<(), HerdrError> {
        self.run(report_agent_args(&self.pane_id, report, seq))
    }

    pub fn report_metadata(&self, metadata: &PaneMetadata<'_>, seq: u64) -> Result<(), HerdrError> {
        self.run(report_metadata_args(&self.pane_id, metadata, seq))
    }

    /// Tells Herdr the agent left the pane, which also forgets its resume
    /// command.
    pub fn release_agent(&self, seq: u64) -> Result<(), HerdrError> {
        let mut args = pane_args("release-agent", &self.pane_id);
        push_seq(&mut args, seq);
        self.run(args)
    }

    fn run(&self, args: Vec<OsString>) -> Result<(), HerdrError> {
        self.cli.run(&args, REPORT_TIMEOUT).map(drop)
    }
}

/// A checkout as Herdr lists it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct HerdrWorktree {
    pub path: PathBuf,
    #[serde(default)]
    pub branch: Option<String>,
    /// The workspace Herdr has it open in.
    #[serde(default)]
    pub open_workspace_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct WorktreeListing {
    pub source: ListingSource,
    pub worktrees: Vec<HerdrWorktree>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ListingSource {
    /// The workspace the repository's own checkout is open in.
    #[serde(default)]
    pub source_workspace_id: Option<String>,
}

impl WorktreeListing {
    /// The workspace Herdr has the checkout at `root` open in. Herdr lists
    /// canonical paths, so `root` has to be one too.
    pub fn workspace_of(&self, root: &Path) -> Option<&str> {
        self.worktrees
            .iter()
            .find(|worktree| worktree.path.canonicalize().ok().as_deref() == Some(root))
            .and_then(|worktree| worktree.open_workspace_id.as_deref())
    }
}

/// A workspace Herdr opened or focused, and the pane to run a command in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedWorkspace {
    pub workspace_id: String,
    /// The tab holding `pane_id`, whose label Herdr's tab bar shows.
    pub tab_id: String,
    pub pane_id: String,
    /// Herdr only focused a workspace that was open already, so the pane is
    /// one the user has been working in.
    pub already_open: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedWorktree {
    pub workspace: OpenedWorkspace,
    pub worktree: HerdrWorktree,
}

#[derive(Deserialize)]
struct Opened {
    workspace: WorkspaceRef,
    tab: TabRef,
    root_pane: PaneRef,
    #[serde(default)]
    worktree: Option<HerdrWorktree>,
    #[serde(default)]
    already_open: bool,
}

#[derive(Deserialize)]
struct WorkspaceRef {
    workspace_id: String,
}

#[derive(Deserialize)]
struct TabRef {
    tab_id: String,
}

#[derive(Deserialize)]
struct PaneRef {
    pane_id: String,
}

#[derive(Deserialize)]
struct PaneAnswer {
    pane: PaneRef,
}

#[derive(Deserialize)]
struct PaneListing {
    panes: Vec<ListedPane>,
}

#[derive(Deserialize)]
struct ListedPane {
    pane_id: String,
    #[serde(default)]
    focused: bool,
}

impl Opened {
    fn workspace(self) -> (OpenedWorkspace, Option<HerdrWorktree>) {
        (
            OpenedWorkspace {
                workspace_id: self.workspace.workspace_id,
                tab_id: self.tab.tab_id,
                pane_id: self.root_pane.pane_id,
                already_open: self.already_open,
            },
            self.worktree,
        )
    }

    fn worktree(self) -> Result<OpenedWorktree, HerdrError> {
        match self.workspace() {
            (workspace, Some(worktree)) => Ok(OpenedWorktree {
                workspace,
                worktree,
            }),
            (_, None) => Err(missing_field("worktree")),
        }
    }
}

/// The calls behind `/worktree`, which never pass `--trust-repository`: that
/// has Herdr's git add a `safe.directory` exception.
impl HerdrCli {
    pub fn worktree_list(&self, main_root: &Path) -> Result<WorktreeListing, HerdrError> {
        self.call(&worktree_list_args(main_root), WORKTREE_TIMEOUT)
    }

    /// Creates a checkout of `main_root`'s repository on `branch`, or on a
    /// branch Herdr names, starting at `base`, and focuses its new workspace.
    /// Herdr refuses to branch off a linked worktree, which is why this takes
    /// the main checkout and an explicit base.
    pub fn worktree_create(
        &self,
        main_root: &Path,
        branch: Option<&str>,
        base: &str,
    ) -> Result<OpenedWorktree, HerdrError> {
        self.call::<Opened>(
            &worktree_create_args(main_root, branch, base),
            WORKTREE_TIMEOUT,
        )?
        .worktree()
    }

    /// Opens the checkout at `path` in a workspace grouped with the
    /// repository's, or focuses the one it is open in.
    pub fn worktree_open(
        &self,
        main_root: &Path,
        path: &Path,
    ) -> Result<OpenedWorktree, HerdrError> {
        self.call::<Opened>(&worktree_open_args(main_root, path), WORKTREE_TIMEOUT)?
            .worktree()
    }

    /// Removes the checkout open in `workspace_id`, keeping its branch, and
    /// closes the workspace.
    pub fn worktree_remove(&self, workspace_id: &str, force: bool) -> Result<(), HerdrError> {
        self.run(&worktree_remove_args(workspace_id, force), WORKTREE_TIMEOUT)
            .map(drop)
    }

    /// [`Self::worktree_remove`] for a caller to start on its own, because
    /// the workspace it closes is the caller's.
    pub fn worktree_remove_command(&self, workspace_id: &str, force: bool) -> Command {
        self.command(&worktree_remove_args(workspace_id, force))
    }

    pub fn workspace_create(&self, cwd: &Path) -> Result<OpenedWorkspace, HerdrError> {
        Ok(self
            .call::<Opened>(&workspace_create_args(cwd), LAYOUT_TIMEOUT)?
            .workspace()
            .0)
    }

    /// Opens a pane in `cwd` beside the focused one of `workspace_id`, and
    /// focuses it.
    pub fn split_workspace(&self, workspace_id: &str, cwd: &Path) -> Result<String, HerdrError> {
        let listing: PaneListing = self.call(&pane_list_args(workspace_id), LAYOUT_TIMEOUT)?;
        let target = listing
            .panes
            .iter()
            .find(|pane| pane.focused)
            .or_else(|| listing.panes.first())
            .ok_or_else(|| missing_field("panes"))?;
        let answer: PaneAnswer =
            self.call(&pane_split_args(&target.pane_id, cwd), LAYOUT_TIMEOUT)?;
        Ok(answer.pane.pane_id)
    }

    /// Types `command_line` into the shell of `pane_id` and presses Enter.
    pub fn pane_run(&self, pane_id: &str, command_line: &str) -> Result<(), HerdrError> {
        self.run(&pane_run_args(pane_id, command_line), LAYOUT_TIMEOUT)
            .map(drop)
    }

    /// Sets the label Herdr shows for `pane_id`.
    pub fn pane_rename(&self, pane_id: &str, label: &str) -> Result<(), HerdrError> {
        self.run(&rename_args("pane", pane_id, label), LAYOUT_TIMEOUT)
            .map(drop)
    }

    /// Sets the label Herdr's tab bar shows for `tab_id`.
    pub fn tab_rename(&self, tab_id: &str, label: &str) -> Result<(), HerdrError> {
        self.run(&rename_args("tab", tab_id, label), LAYOUT_TIMEOUT)
            .map(drop)
    }

    fn call<T: DeserializeOwned>(
        &self,
        args: &[OsString],
        timeout: Duration,
    ) -> Result<T, HerdrError> {
        let result = self.run(args, timeout)?.unwrap_or_default();
        serde_json::from_value(result).map_err(HerdrError::Output)
    }
}

/// Resumes `session` with this executable, its replacement at the same path,
/// or the installed `caudra` when neither file remains.
pub fn resume_command_line(session: &str) -> String {
    let program = resume_program(env::current_exe().ok());
    shell_words::join([program.as_str(), SESSION_FLAG, session])
}

fn resume_program(exe: Option<PathBuf>) -> String {
    exe.and_then(|path| {
        if path.is_file() {
            return Some(path.to_string_lossy().into_owned());
        }
        let replacement = path.to_str()?.strip_suffix(REPLACED_EXE_SUFFIX)?;
        Path::new(replacement)
            .is_file()
            .then(|| replacement.to_owned())
    })
    .unwrap_or_else(|| RESUME_COMMAND.to_owned())
}

fn missing_field(field: &'static str) -> HerdrError {
    HerdrError::Output(serde::de::Error::missing_field(field))
}

fn argv<const N: usize>(values: [&OsStr; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}

fn worktree_list_args(main_root: &Path) -> Vec<OsString> {
    argv([
        "worktree".as_ref(),
        "list".as_ref(),
        "--cwd".as_ref(),
        main_root.as_os_str(),
    ])
}

fn worktree_create_args(main_root: &Path, branch: Option<&str>, base: &str) -> Vec<OsString> {
    let mut args = argv([
        "worktree".as_ref(),
        "create".as_ref(),
        "--cwd".as_ref(),
        main_root.as_os_str(),
    ]);
    if let Some(branch) = branch {
        args.extend(["--branch".into(), branch.into()]);
    }
    args.extend(["--base".into(), base.into(), "--focus".into()]);
    args
}

fn worktree_open_args(main_root: &Path, path: &Path) -> Vec<OsString> {
    argv([
        "worktree".as_ref(),
        "open".as_ref(),
        "--cwd".as_ref(),
        main_root.as_os_str(),
        "--path".as_ref(),
        path.as_os_str(),
        "--focus".as_ref(),
    ])
}

fn worktree_remove_args(workspace_id: &str, force: bool) -> Vec<OsString> {
    let mut args = argv([
        "worktree".as_ref(),
        "remove".as_ref(),
        "--workspace".as_ref(),
        workspace_id.as_ref(),
    ]);
    if force {
        args.push("--force".into());
    }
    args
}

fn workspace_create_args(cwd: &Path) -> Vec<OsString> {
    argv([
        "workspace".as_ref(),
        "create".as_ref(),
        "--cwd".as_ref(),
        cwd.as_os_str(),
        "--focus".as_ref(),
    ])
}

fn pane_list_args(workspace_id: &str) -> Vec<OsString> {
    argv([
        "pane".as_ref(),
        "list".as_ref(),
        "--workspace".as_ref(),
        workspace_id.as_ref(),
    ])
}

fn pane_split_args(pane_id: &str, cwd: &Path) -> Vec<OsString> {
    argv([
        "pane".as_ref(),
        "split".as_ref(),
        pane_id.as_ref(),
        "--direction".as_ref(),
        SPLIT_DIRECTION.as_ref(),
        "--cwd".as_ref(),
        cwd.as_os_str(),
        "--focus".as_ref(),
    ])
}

fn pane_run_args(pane_id: &str, command_line: &str) -> Vec<OsString> {
    argv([
        "pane".as_ref(),
        "run".as_ref(),
        pane_id.as_ref(),
        command_line.as_ref(),
    ])
}

fn rename_args(kind: &str, id: &str, label: &str) -> Vec<OsString> {
    argv([
        kind.as_ref(),
        "rename".as_ref(),
        id.as_ref(),
        label.as_ref(),
    ])
}

fn pane_args(subcommand: &str, pane_id: &str) -> Vec<OsString> {
    [
        "pane", subcommand, pane_id, "--source", SOURCE, "--agent", AGENT,
    ]
    .map(OsString::from)
    .into()
}

fn push_seq(args: &mut Vec<OsString>, seq: u64) {
    args.extend(["--seq".into(), seq.to_string().into()]);
}

fn report_agent_args(pane_id: &str, report: &AgentReport<'_>, seq: u64) -> Vec<OsString> {
    let mut args = pane_args("report-agent", pane_id);
    args.extend(["--state".into(), report.state.as_str().into()]);
    push_seq(&mut args, seq);
    if let Some(message) = report.message {
        args.extend(["--message".into(), message.into()]);
    }
    if let Some(resume) = report.resume {
        args.push(RESUME_SEPARATOR.into());
        args.extend(resume.iter().map(OsString::from));
    }
    args
}

fn report_metadata_args(pane_id: &str, metadata: &PaneMetadata<'_>, seq: u64) -> Vec<OsString> {
    let mut args = pane_args("report-metadata", pane_id);
    if metadata.title.is_empty() {
        args.push("--clear-title".into());
    } else {
        args.extend(["--title".into(), metadata.title.as_ref().into()]);
    }
    args.extend([
        "--token".into(),
        format!("{MODEL_TOKEN}={}", metadata.model).into(),
    ]);
    match metadata.context_percent {
        Some(percent) => args.extend([
            "--token".into(),
            format!("{CONTEXT_TOKEN}={percent}%").into(),
        ]),
        None => args.extend(["--clear-token".into(), CONTEXT_TOKEN.into()]),
    }
    push_seq(&mut args, seq);
    args
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const PANE: &str = "w1:p2";
    const MESSAGE: &str = "Permission requested: shell";
    const SESSION: &str = "4Tx8ZrCGn1j9aE6oUvWm2k";
    const TITLE: &str = "Fix the -- parser";
    const MODEL: &str = "claude-opus-4-5";
    const MAIN_ROOT: &str = "/work/app";
    const WORKTREE_PATH: &str = "/data/worktrees/app/feature-login";
    const BRANCH: &str = "feature/login";
    const SPACED_LABEL: &str = "my checkout";
    const BASE: &str = "0123456789abcdef0123456789abcdef01234567";
    const WORKSPACE: &str = "w2";
    const TAB: &str = "w2:t1";
    const ROOT_PANE: &str = "w2:p2";
    #[cfg(unix)]
    const SPLIT_PANE: &str = "w2:p3";
    const RESUME_BINARY: &str = "caudra build";

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    #[test]
    fn state_report_carries_message_then_resume_command() {
        let resume = resume_argv(Some(SESSION));
        let report = AgentReport {
            state: AgentState::Blocked,
            message: Some(MESSAGE),
            resume: Some(&resume),
        };

        assert_eq!(
            strings(report_agent_args(PANE, &report, 42)),
            [
                "pane",
                "report-agent",
                PANE,
                "--source",
                SOURCE,
                "--agent",
                AGENT,
                "--state",
                "blocked",
                "--seq",
                "42",
                "--message",
                MESSAGE,
                "--",
                RESUME_COMMAND,
                "--session",
                SESSION,
            ]
        );
    }

    #[test_case(AgentState::Idle, "idle" ; "idle")]
    #[test_case(AgentState::Working, "working" ; "working")]
    fn plain_state_report_has_no_optional_parts(state: AgentState, name: &str) {
        let report = AgentReport {
            state,
            message: None,
            resume: None,
        };

        assert_eq!(
            strings(report_agent_args(PANE, &report, 7)),
            [
                "pane",
                "report-agent",
                PANE,
                "--source",
                SOURCE,
                "--agent",
                AGENT,
                "--state",
                name,
                "--seq",
                "7",
            ]
        );
    }

    #[test_case(None, &[RESUME_COMMAND] ; "unsaved_session_starts_fresh")]
    #[test_case(Some(SESSION), &[RESUME_COMMAND, "--session", SESSION] ; "saved_session_resumes")]
    fn resume_command_names_saved_sessions_only(session: Option<&str>, expected: &[&str]) {
        assert_eq!(resume_argv(session), expected);
    }

    #[test]
    fn metadata_sets_title_model_and_context() {
        let metadata = PaneMetadata {
            title: TITLE.into(),
            model: MODEL.into(),
            context_percent: Some(12),
        };

        assert_eq!(
            strings(report_metadata_args(PANE, &metadata, 9)),
            [
                "pane",
                "report-metadata",
                PANE,
                "--source",
                SOURCE,
                "--agent",
                AGENT,
                "--title",
                TITLE,
                "--token",
                &format!("{MODEL_TOKEN}={MODEL}"),
                "--token",
                &format!("{CONTEXT_TOKEN}=12%"),
                "--seq",
                "9",
            ]
        );
    }

    #[test_case(Some(BRANCH), &["--branch", BRANCH] ; "named_branch")]
    #[test_case(None, &[] ; "herdr_names_the_branch")]
    fn worktree_create_branches_off_the_main_checkout(branch: Option<&str>, named: &[&str]) {
        let mut expected = vec!["worktree", "create", "--cwd", MAIN_ROOT];
        expected.extend(named);
        expected.extend(["--base", BASE, "--focus"]);

        assert_eq!(
            strings(worktree_create_args(Path::new(MAIN_ROOT), branch, BASE)),
            expected
        );
    }

    #[test_case(false, &[] ; "clean")]
    #[test_case(true, &["--force"] ; "forced")]
    fn worktree_remove_targets_the_workspace(force: bool, flag: &[&str]) {
        let mut expected = vec!["worktree", "remove", "--workspace", WORKSPACE];
        expected.extend(flag);

        assert_eq!(strings(worktree_remove_args(WORKSPACE, force)), expected);
    }

    #[test_case("pane", ROOT_PANE ; "pane")]
    #[test_case("tab", TAB ; "tab")]
    fn a_rename_passes_the_label_as_one_argument(kind: &str, id: &str) {
        assert_eq!(
            strings(rename_args(kind, id, SPACED_LABEL)),
            [kind, "rename", id, SPACED_LABEL]
        );
    }

    #[test]
    fn a_created_worktree_names_its_workspace_pane_and_path() {
        let result = serde_json::json!({
            "type": "worktree_created",
            "workspace": {"workspace_id": WORKSPACE, "label": "app"},
            "tab": {"tab_id": TAB},
            "root_pane": {"pane_id": ROOT_PANE, "focused": true},
            "worktree": {"path": WORKTREE_PATH, "branch": BRANCH, "open_workspace_id": WORKSPACE},
        });

        let opened = serde_json::from_value::<Opened>(result)
            .unwrap()
            .worktree()
            .unwrap();

        assert_eq!(
            opened,
            OpenedWorktree {
                workspace: OpenedWorkspace {
                    workspace_id: WORKSPACE.into(),
                    tab_id: TAB.into(),
                    pane_id: ROOT_PANE.into(),
                    already_open: false,
                },
                worktree: HerdrWorktree {
                    path: WORKTREE_PATH.into(),
                    branch: Some(BRANCH.into()),
                    open_workspace_id: Some(WORKSPACE.into()),
                },
            }
        );
    }

    #[test]
    fn a_created_workspace_is_not_a_worktree() {
        let result = serde_json::json!({
            "workspace": {"workspace_id": WORKSPACE},
            "tab": {"tab_id": TAB},
            "root_pane": {"pane_id": ROOT_PANE},
        });

        assert!(matches!(
            serde_json::from_value::<Opened>(result).unwrap().worktree(),
            Err(HerdrError::Output(_))
        ));
    }

    #[test]
    fn a_listing_finds_the_workspace_by_canonical_path() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let listing = WorktreeListing {
            source: ListingSource {
                source_workspace_id: None,
            },
            worktrees: vec![HerdrWorktree {
                path: root.join("nested").join(".."),
                branch: None,
                open_workspace_id: Some(WORKSPACE.into()),
            }],
        };
        fs::create_dir(root.join("nested")).unwrap();

        assert_eq!(listing.workspace_of(&root), Some(WORKSPACE));
        assert_eq!(listing.workspace_of(&root.join("nested")), None);
    }

    #[test]
    fn the_resume_line_runs_this_executable_on_the_session() {
        let words = shell_words::split(&resume_command_line(SESSION)).unwrap();

        assert_eq!(
            words,
            [
                env::current_exe().unwrap().to_string_lossy().as_ref(),
                SESSION_FLAG,
                SESSION
            ]
        );
    }

    #[test_case(false, true ; "current_executable")]
    #[test_case(true, true ; "replaced_executable")]
    #[test_case(false, false ; "missing_executable")]
    #[test_case(true, false ; "deleted_without_replacement")]
    fn a_resume_uses_a_remaining_executable_or_path(replaced: bool, installed: bool) {
        let temp = TempDir::new().unwrap();
        let installed_path = temp.path().join(RESUME_BINARY);
        if installed {
            fs::write(&installed_path, []).unwrap();
        }
        let mut running_path = installed_path.as_os_str().to_owned();
        if replaced {
            running_path.push(REPLACED_EXE_SUFFIX);
        }

        let program = resume_program(Some(running_path.into()));

        let expected = if installed {
            installed_path.to_string_lossy().into_owned()
        } else {
            RESUME_COMMAND.to_owned()
        };
        assert_eq!(program, expected);
    }

    #[test_case(false ; "only_literal_name_exists")]
    #[test_case(true ; "both_names_exist")]
    fn an_existing_executable_keeps_a_literal_deleted_suffix(unsuffixed_exists: bool) {
        let temp = TempDir::new().unwrap();
        let path = temp
            .path()
            .join(format!("{RESUME_BINARY}{REPLACED_EXE_SUFFIX}"));
        fs::write(&path, []).unwrap();
        if unsuffixed_exists {
            fs::write(temp.path().join(RESUME_BINARY), []).unwrap();
        }

        assert_eq!(resume_program(Some(path.clone())), path.to_string_lossy());
    }

    #[test_case(None ; "current_exe_unavailable")]
    #[test_case(Some(PathBuf::new()) ; "empty_path")]
    fn an_unavailable_executable_uses_path(exe: Option<PathBuf>) {
        assert_eq!(resume_program(exe), RESUME_COMMAND);
    }

    #[cfg(unix)]
    #[test]
    fn a_split_opens_beside_the_focused_pane() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let record = temp.path().join("argv");
        let binary = temp.path().join("herdr");
        fs::write(
            &binary,
            format!(
                r#"#!/bin/sh
case "$2" in
list) echo '{{"id":"cli","result":{{"type":"pane_list","panes":[{{"pane_id":"w2:p1","focused":false}},{{"pane_id":"{ROOT_PANE}","focused":true}}]}}}}' ;;
split) printf '%s\n' "$@" > '{}'; echo '{{"id":"cli","result":{{"type":"pane_info","pane":{{"pane_id":"{SPLIT_PANE}"}}}}}}' ;;
esac
"#,
                record.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let cli = HerdrCli::new(binary.into(), "/tmp/herdr.sock".into());

        let pane = cli
            .split_workspace(WORKSPACE, Path::new(MAIN_ROOT))
            .unwrap();

        assert_eq!(pane, SPLIT_PANE);
        assert_eq!(
            fs::read_to_string(record).unwrap(),
            format!(
                "pane\nsplit\n{ROOT_PANE}\n--direction\n{SPLIT_DIRECTION}\n--cwd\n{MAIN_ROOT}\n--focus\n"
            )
        );
    }

    #[test]
    fn metadata_clears_what_is_unknown() {
        let metadata = PaneMetadata {
            title: "".into(),
            model: MODEL.into(),
            context_percent: None,
        };

        assert_eq!(
            strings(report_metadata_args(PANE, &metadata, 9)),
            [
                "pane",
                "report-metadata",
                PANE,
                "--source",
                SOURCE,
                "--agent",
                AGENT,
                "--clear-title",
                "--token",
                &format!("{MODEL_TOKEN}={MODEL}"),
                "--clear-token",
                CONTEXT_TOKEN,
                "--seq",
                "9",
            ]
        );
    }
}
