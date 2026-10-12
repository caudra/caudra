use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;
use tracing::warn;

use super::HOST;
use super::cli::OrcaCli;
use super::env::OrcaEnv;
use crate::error::HostError;
use crate::workspaces::{HostLocation, HostPane, Located, Opened, PaneSite, Workspaces};

/// Orca runs git for these, which takes as long as the checkout does.
const WORKTREE_TIMEOUT: Duration = Duration::from_secs(600);
/// Opening, focusing or typing into a terminal.
const LAYOUT_TIMEOUT: Duration = Duration::from_secs(15);
const ID_SELECTOR: &str = "id:";
const PATH_SELECTOR: &str = "path:";
const BRANCH_REF: &str = "refs/heads/";
/// How Orca says it does not know a repository or worktree.
const UNKNOWN_CODES: [&str; 2] = ["repo_not_found", "selector_not_found"];
/// How Orca refuses to remove a repository's own checkout.
const PROTECTED_WORKTREE: &str = "Refusing to delete protected worktree";

/// Worktrees as Orca manages them: each belongs to a repository the user
/// added to Orca, and a command runs in one by opening a terminal there.
#[derive(Debug)]
pub(super) struct OrcaWorkspaces {
    cli: OrcaCli,
    worktree_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Worktree {
    id: String,
    path: PathBuf,
    #[serde(default)]
    branch: String,
    #[serde(default)]
    is_main_worktree: bool,
}

#[derive(Deserialize)]
struct Listing {
    worktrees: Vec<Worktree>,
}

#[derive(Deserialize)]
struct Created {
    worktree: Worktree,
}

#[derive(Deserialize)]
struct Terminal {
    handle: String,
}

#[derive(Deserialize)]
struct Terminals {
    terminals: Vec<Terminal>,
}

#[derive(Deserialize)]
struct TerminalCreated {
    terminal: Terminal,
}

#[derive(Deserialize)]
struct Ignored {}

impl Worktree {
    fn branch(&self) -> Option<String> {
        let branch = self.branch.strip_prefix(BRANCH_REF).unwrap_or(&self.branch);
        (!branch.is_empty()).then(|| branch.to_owned())
    }

    fn is_at(&self, root: &Path) -> bool {
        self.path.canonicalize().ok().as_deref() == Some(root)
    }
}

fn selector(kind: &str, value: impl AsRef<OsStr>) -> OsString {
    let mut selector = OsString::from(kind);
    selector.push(value);
    selector
}

/// Orca not knowing the repository leaves it to git.
fn unknown(main_root: &Path) -> impl FnOnce(HostError) -> HostError + '_ {
    move |error| match error.code() {
        Some(code) if UNKNOWN_CODES.contains(&code) => HostError::UnknownRepository {
            host: HOST,
            path: main_root.to_path_buf(),
        },
        _ => error,
    }
}

impl OrcaWorkspaces {
    pub(super) fn new(env: &OrcaEnv) -> Self {
        Self {
            cli: OrcaCli::new(env.binary.clone()),
            worktree_id: env.worktree_id.clone(),
        }
    }

    fn worktrees(&self, main_root: &Path) -> Result<Vec<Worktree>, HostError> {
        let repository = selector(PATH_SELECTOR, main_root);
        self.cli
            .call::<Listing>(
                &[
                    "worktree".as_ref(),
                    "list".as_ref(),
                    "--repo".as_ref(),
                    &repository,
                ],
                WORKTREE_TIMEOUT,
            )
            .map(|listing| listing.worktrees)
            .map_err(unknown(main_root))
    }

    fn terminals(&self, location: &HostLocation) -> Result<Vec<Terminal>, HostError> {
        let worktree = selector(ID_SELECTOR, &location.0);
        self.cli
            .call::<Terminals>(
                &[
                    "terminal".as_ref(),
                    "list".as_ref(),
                    "--worktree".as_ref(),
                    &worktree,
                ],
                LAYOUT_TIMEOUT,
            )
            .map(|listing| listing.terminals)
    }

    fn open_terminal(
        &self,
        location: &HostLocation,
        command_line: Option<&str>,
        label: Option<&str>,
    ) -> Result<String, HostError> {
        let worktree = selector(ID_SELECTOR, &location.0);
        let mut args: Vec<&OsStr> = vec![
            "terminal".as_ref(),
            "create".as_ref(),
            "--worktree".as_ref(),
            &worktree,
        ];
        if let Some(command_line) = command_line {
            args.extend([OsStr::new("--command"), OsStr::new(command_line)]);
        }
        if let Some(label) = label {
            args.extend([OsStr::new("--title"), OsStr::new(label)]);
        }
        args.push("--focus".as_ref());
        self.cli
            .call::<TerminalCreated>(&args, LAYOUT_TIMEOUT)
            .map(|created| created.terminal.handle)
    }

    fn remove_args(worktree: &OsStr, force: bool) -> Vec<&OsStr> {
        let mut args: Vec<&OsStr> = vec![
            "worktree".as_ref(),
            "rm".as_ref(),
            "--worktree".as_ref(),
            worktree,
        ];
        if force {
            args.push("--force".as_ref());
        }
        args
    }
}

impl Workspaces for OrcaWorkspaces {
    fn host(&self) -> &'static str {
        HOST
    }

    /// Orca prefixes and sanitises the name it is given, but needs one.
    fn names_branches(&self) -> bool {
        false
    }

    fn create(
        &self,
        main_root: &Path,
        branch: Option<&str>,
        base: &str,
    ) -> Result<Opened, HostError> {
        let Some(branch) = branch else {
            return Err(HostError::missing_field(HOST, "name"));
        };
        let repository = selector(PATH_SELECTOR, main_root);
        let created: Created = self
            .cli
            .call(
                &[
                    "worktree".as_ref(),
                    "create".as_ref(),
                    "--name".as_ref(),
                    branch.as_ref(),
                    "--repo".as_ref(),
                    &repository,
                    "--base-branch".as_ref(),
                    base.as_ref(),
                    "--activate".as_ref(),
                ],
                WORKTREE_TIMEOUT,
            )
            .map_err(unknown(main_root))?;
        let location = HostLocation(created.worktree.id.clone());
        Ok(Opened {
            branch: created.worktree.branch(),
            pane: HostPane::Pending {
                location: location.clone(),
                cwd: created.worktree.path.clone(),
            },
            location,
            root: created.worktree.path,
            already_open: false,
        })
    }

    /// Switches to a terminal Orca has open in the checkout, else opens one.
    fn show(&self, main_root: &Path, root: &Path) -> Result<Opened, HostError> {
        let worktree = self
            .worktrees(main_root)?
            .into_iter()
            .find(|worktree| worktree.is_at(root))
            .ok_or_else(|| HostError::UnknownRepository {
                host: HOST,
                path: root.to_path_buf(),
            })?;
        let location = HostLocation(worktree.id.clone());
        let (handle, already_open) = match self.terminals(&location)?.into_iter().next() {
            Some(terminal) => {
                let handle = OsString::from(&terminal.handle);
                self.cli.call::<Ignored>(
                    &[
                        "terminal".as_ref(),
                        "switch".as_ref(),
                        "--terminal".as_ref(),
                        &handle,
                    ],
                    LAYOUT_TIMEOUT,
                )?;
                (terminal.handle, true)
            }
            None => (self.open_terminal(&location, None, None)?, false),
        };
        Ok(Opened {
            branch: worktree.branch(),
            location,
            root: worktree.path,
            pane: HostPane::Open {
                pane: handle,
                tab: None,
            },
            already_open,
        })
    }

    fn locate(&self, main_root: &Path, root: &Path) -> Result<Located, HostError> {
        let worktrees = self.worktrees(main_root)?;
        let location =
            |found: Option<&Worktree>| found.map(|worktree| HostLocation(worktree.id.clone()));
        Ok(Located {
            checkout: location(worktrees.iter().find(|worktree| worktree.is_at(root))),
            main: location(worktrees.iter().find(|worktree| worktree.is_main_worktree)),
        })
    }

    /// Orca opens a terminal only in a worktree, and only once it has a
    /// command for it. Each terminal is a tab of its own.
    fn pane(&self, site: PaneSite<'_>) -> Result<HostPane, HostError> {
        match site {
            PaneSite::Beside { location, cwd } => Ok(HostPane::Pending {
                location: location.clone(),
                cwd: cwd.to_path_buf(),
            }),
            PaneSite::NewTab(cwd) => match &self.worktree_id {
                Some(worktree_id) => Ok(HostPane::Pending {
                    location: HostLocation(worktree_id.clone()),
                    cwd: cwd.to_path_buf(),
                }),
                None => Err(HostError::UnknownRepository {
                    host: HOST,
                    path: cwd.to_path_buf(),
                }),
            },
            PaneSite::NewWorkspace(cwd) => Err(HostError::UnknownRepository {
                host: HOST,
                path: cwd.to_path_buf(),
            }),
        }
    }

    fn run(
        &self,
        pane: &HostPane,
        command_line: &str,
        label: Option<&str>,
    ) -> Result<(), HostError> {
        let handle = match pane {
            HostPane::Pending { location, .. } => {
                return self
                    .open_terminal(location, Some(command_line), label)
                    .map(drop);
            }
            HostPane::Open { pane, .. } => OsString::from(pane),
        };
        if let Some(label) = label
            && let Err(error) = self.cli.call::<Ignored>(
                &[
                    "terminal".as_ref(),
                    "rename".as_ref(),
                    "--terminal".as_ref(),
                    &handle,
                    "--title".as_ref(),
                    label.as_ref(),
                ],
                LAYOUT_TIMEOUT,
            )
        {
            warn!(%error, ?handle, "Orca did not label the terminal");
        }
        self.cli.call::<Ignored>(
            &[
                "terminal".as_ref(),
                "send".as_ref(),
                "--terminal".as_ref(),
                &handle,
                "--text".as_ref(),
                command_line.as_ref(),
                "--enter".as_ref(),
            ],
            LAYOUT_TIMEOUT,
        )?;
        Ok(())
    }

    fn remove(&self, location: &HostLocation, force: bool) -> Result<bool, HostError> {
        let worktree = selector(ID_SELECTOR, &location.0);
        match self
            .cli
            .call::<Ignored>(&Self::remove_args(&worktree, force), WORKTREE_TIMEOUT)
        {
            Ok(_) => Ok(true),
            Err(HostError::Refused { code, .. }) if UNKNOWN_CODES.contains(&code.as_str()) => {
                Ok(false)
            }
            Err(HostError::Refused { message, .. }) if message.starts_with(PROTECTED_WORKTREE) => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn remove_command(&self, location: &HostLocation, force: bool) -> Command {
        let worktree = selector(ID_SELECTOR, &location.0);
        self.cli.command(&Self::remove_args(&worktree, force))
    }

    fn holds_this_pane(&self, location: &HostLocation) -> bool {
        self.worktree_id.as_deref() == Some(location.0.as_str())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;
    use test_case::test_case;

    const RECORD: &str = "calls";
    const REPO: &str = "repo-1";
    const HANDLE: &str = "term_7";
    const BRANCH: &str = "feature/login";
    const PREFIXED_BRANCH: &str = "refs/heads/me/feature-login";
    const BASE: &str = "0123456789abcdef0123456789abcdef01234567";
    const COMMAND: &str = "caudra --session 4Tx8";
    const LABEL: &str = "login";
    const OK_EMPTY: &str = r#"{"ok":true,"result":{}}"#;

    struct Fixture {
        _temp: TempDir,
        base: PathBuf,
        main: PathBuf,
        linked: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let base = temp.path().canonicalize().unwrap();
            let main = base.join("main");
            let linked = base.join("linked");
            fs::create_dir(&main).unwrap();
            fs::create_dir(&linked).unwrap();
            Self {
                _temp: temp,
                base,
                main,
                linked,
            }
        }

        fn id(&self, path: &Path) -> String {
            format!("{REPO}::{}", path.display())
        }

        fn listing(&self) -> String {
            format!(
                r#"{{"ok":true,"result":{{"worktrees":[{{"id":"{}","repoId":"{REPO}","path":"{}","branch":"refs/heads/main","isMainWorktree":true}},{{"id":"{}","repoId":"{REPO}","path":"{}","branch":"{PREFIXED_BRANCH}","isMainWorktree":false}}]}}}}"#,
                self.id(&self.main),
                self.main.display(),
                self.id(&self.linked),
                self.linked.display()
            )
        }

        /// An `orca` that records each call on one line and answers it by
        /// its first two words from `answers`, else with an empty success.
        fn orca(&self, answers: &[(&str, String)]) -> OrcaWorkspaces {
            let binary = self.base.join("orca");
            let mut script = format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$1 $2\" in\n",
                self.base.join(RECORD).display()
            );
            for (call, answer) in answers {
                script.push_str(&format!(
                    "'{call}') echo '{answer}'; exit {} ;;\n",
                    u8::from(answer.contains("\"ok\":false"))
                ));
            }
            script.push_str(&format!("*) echo '{OK_EMPTY}' ;;\nesac\n"));
            fs::write(&binary, script).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            OrcaWorkspaces {
                cli: OrcaCli::new(binary.into()),
                worktree_id: Some(self.id(&self.linked)),
            }
        }

        fn calls(&self) -> Vec<String> {
            fs::read_to_string(self.base.join(RECORD))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    fn refusal(code: &str, message: &str) -> String {
        format!(r#"{{"ok":false,"error":{{"code":"{code}","message":"{message}"}}}}"#)
    }

    #[test]
    fn locate_finds_the_checkout_and_the_main_one() {
        let fixture = Fixture::new();
        let orca = fixture.orca(&[("worktree list", fixture.listing())]);

        let located = orca.locate(&fixture.main, &fixture.linked).unwrap();

        assert_eq!(
            located,
            Located {
                checkout: Some(HostLocation(fixture.id(&fixture.linked))),
                main: Some(HostLocation(fixture.id(&fixture.main))),
            }
        );
        assert_eq!(
            fixture.calls(),
            [format!(
                "worktree list --repo path:{} --json",
                fixture.main.display()
            )]
        );
    }

    #[test_case("repo_not_found" ; "repository")]
    #[test_case("selector_not_found" ; "selector")]
    fn a_repository_orca_does_not_know_is_left_to_git(code: &str) {
        let fixture = Fixture::new();
        let orca = fixture.orca(&[("worktree list", refusal(code, code))]);

        assert!(matches!(
            orca.locate(&fixture.main, &fixture.linked),
            Err(HostError::UnknownRepository { .. })
        ));
    }

    #[test]
    fn create_names_the_branch_and_leaves_the_pane_to_the_command() {
        let fixture = Fixture::new();
        let created = format!(
            r#"{{"ok":true,"result":{{"worktree":{{"id":"{}","repoId":"{REPO}","path":"{}","branch":"{PREFIXED_BRANCH}"}}}}}}"#,
            fixture.id(&fixture.linked),
            fixture.linked.display()
        );
        let orca = fixture.orca(&[("worktree create", created)]);

        let opened = orca.create(&fixture.main, Some(BRANCH), BASE).unwrap();

        let location = HostLocation(fixture.id(&fixture.linked));
        assert_eq!(
            opened,
            Opened {
                location: location.clone(),
                root: fixture.linked.clone(),
                branch: Some("me/feature-login".into()),
                pane: HostPane::Pending {
                    location,
                    cwd: fixture.linked.clone()
                },
                already_open: false,
            }
        );
        assert_eq!(
            fixture.calls(),
            [format!(
                "worktree create --name {BRANCH} --repo path:{} --base-branch {BASE} --activate --json",
                fixture.main.display()
            )]
        );
    }

    #[test_case(true ; "switches_to_an_open_terminal")]
    #[test_case(false ; "opens_a_terminal")]
    fn show_focuses_the_checkout(open: bool) {
        let fixture = Fixture::new();
        let terminals = if open {
            format!(r#"{{"ok":true,"result":{{"terminals":[{{"handle":"{HANDLE}"}}]}}}}"#)
        } else {
            r#"{"ok":true,"result":{"terminals":[]}}"#.to_owned()
        };
        let orca = fixture.orca(&[
            ("worktree list", fixture.listing()),
            ("terminal list", terminals),
            (
                "terminal create",
                format!(r#"{{"ok":true,"result":{{"terminal":{{"handle":"{HANDLE}"}}}}}}"#),
            ),
        ]);

        let opened = orca.show(&fixture.main, &fixture.linked).unwrap();

        assert_eq!(opened.already_open, open);
        assert_eq!(
            opened.pane,
            HostPane::Open {
                pane: HANDLE.into(),
                tab: None
            }
        );
        let id = fixture.id(&fixture.linked);
        let last = if open {
            format!("terminal switch --terminal {HANDLE} --json")
        } else {
            format!("terminal create --worktree id:{id} --focus --json")
        };
        assert_eq!(fixture.calls().last(), Some(&last));
    }

    #[test]
    fn a_pending_pane_opens_a_terminal_running_the_command() {
        let fixture = Fixture::new();
        let orca = fixture.orca(&[(
            "terminal create",
            format!(r#"{{"ok":true,"result":{{"terminal":{{"handle":"{HANDLE}"}}}}}}"#),
        )]);
        let location = HostLocation(fixture.id(&fixture.linked));
        let pane = orca
            .pane(PaneSite::Beside {
                location: &location,
                cwd: &fixture.linked,
            })
            .unwrap();

        orca.run(&pane, COMMAND, Some(LABEL)).unwrap();

        assert_eq!(
            fixture.calls(),
            [format!(
                "terminal create --worktree id:{} --command {COMMAND} --title {LABEL} --focus --json",
                location.0
            )]
        );
    }

    #[test]
    fn a_new_tab_opens_a_terminal_in_this_worktree() {
        let fixture = Fixture::new();
        let orca = fixture.orca(&[(
            "terminal create",
            format!(r#"{{"ok":true,"result":{{"terminal":{{"handle":"{HANDLE}"}}}}}}"#),
        )]);
        let pane = orca.pane(PaneSite::NewTab(&fixture.main)).unwrap();

        orca.run(&pane, COMMAND, Some(LABEL)).unwrap();

        assert_eq!(
            fixture.calls(),
            [format!(
                "terminal create --worktree id:{} --command {COMMAND} --title {LABEL} --focus --json",
                fixture.id(&fixture.linked)
            )]
        );
    }

    #[test]
    fn a_terminal_outside_a_worktree_opens_no_tab() {
        let fixture = Fixture::new();
        let mut orca = fixture.orca(&[]);
        orca.worktree_id = None;

        assert!(matches!(
            orca.pane(PaneSite::NewTab(&fixture.main)),
            Err(HostError::UnknownRepository { .. })
        ));
        assert!(fixture.calls().is_empty());
    }

    #[test]
    fn an_open_pane_is_labelled_then_typed_into() {
        let fixture = Fixture::new();
        let orca = fixture.orca(&[]);

        orca.run(
            &HostPane::Open {
                pane: HANDLE.into(),
                tab: None,
            },
            COMMAND,
            Some(LABEL),
        )
        .unwrap();

        assert_eq!(
            fixture.calls(),
            [
                format!("terminal rename --terminal {HANDLE} --title {LABEL} --json"),
                format!("terminal send --terminal {HANDLE} --text {COMMAND} --enter --json"),
            ]
        );
    }

    #[test_case(OK_EMPTY.to_owned(), Some(true) ; "removed")]
    #[test_case(refusal("runtime_error", "Refusing to delete protected worktree path: /w"), Some(false) ; "main_checkout_left_to_git")]
    #[test_case(refusal("selector_not_found", "No worktree"), Some(false) ; "unknown_left_to_git")]
    #[test_case(refusal("runtime_error", "disk full"), None ; "other_refusal_fails")]
    fn remove_says_whether_orca_removed_it(answer: String, expected: Option<bool>) {
        let fixture = Fixture::new();
        let orca = fixture.orca(&[("worktree rm", answer)]);
        let location = HostLocation(fixture.id(&fixture.linked));

        let removed = orca.remove(&location, true);

        assert_eq!(removed.ok(), expected);
        assert_eq!(
            fixture.calls(),
            [format!(
                "worktree rm --worktree id:{} --force --json",
                location.0
            )]
        );
    }

    #[test]
    fn only_the_worktree_this_terminal_belongs_to_holds_it() {
        let fixture = Fixture::new();
        let orca = fixture.orca(&[]);

        assert!(orca.holds_this_pane(&HostLocation(fixture.id(&fixture.linked))));
        assert!(!orca.holds_this_pane(&HostLocation(fixture.id(&fixture.main))));
    }
}
