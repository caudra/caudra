//! Platform process construction and best-effort process-tree termination.
//!
//! Unix launches each shell as a process-group leader, then applies TERM, a grace period, KILL, and
//! reaping. Process groups cover normal descendants but are not containment: a child may deliberately
//! create a new session/group. Windows uses `taskkill /T /F`; without Job Objects it cannot provide
//! equivalent graceful signaling or a reliable residual-tree existence check.

use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
    time::{Duration, Instant},
};
#[cfg(unix)]
use std::{
    fs::Metadata,
    os::unix::{fs::MetadataExt, process::CommandExt},
};
use tokio::process::{Child, Command};

use crate::bash::BashContextAssumptions;

const TERMINATION_GRACE: Duration = Duration::from_secs(3);
#[cfg(unix)]
const BASH_ARGUMENTS: [&str; 3] = ["--noprofile", "--norc", "-c"];
#[cfg(unix)]
const BASH_EXECUTABLE_ENV: &str = "WORKCELL_BASH_EXECUTABLE";
#[cfg(unix)]
const DEFAULT_BASH_EXECUTABLES: &[&str] = &["/bin/bash", "/usr/bin/bash"];
const MAX_LAUNCHER_PATH_BYTES: usize = 4096;
const ARC_COUNTER_BYTES: usize = 2 * size_of::<usize>();
#[cfg(unix)]
const EXECUTABLE_MODE: u32 = 0o111;

pub(crate) type SharedShellLauncher = Arc<Result<ShellLauncher, ShellLauncherError>>;

#[derive(Debug)]
pub(crate) struct ShellLauncher {
    executable: PathBuf,
    #[cfg(unix)]
    identity: BashExecutableIdentity,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ShellLauncherError {
    InvalidPath,
    #[cfg(unix)]
    Unavailable,
    #[cfg(unix)]
    NotExecutable,
    #[cfg(unix)]
    Changed,
}

impl fmt::Display for ShellLauncherError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPath => "The host shell executable must have a nonempty bounded path; WORKCELL_BASH_EXECUTABLE must be absolute",
            #[cfg(unix)]
            Self::Unavailable => "No trusted Bash executable is available; set host WORKCELL_BASH_EXECUTABLE to an absolute Bash path",
            #[cfg(unix)]
            Self::NotExecutable => "The host Bash executable must be an executable regular file",
            #[cfg(unix)]
            Self::Changed => "The bound Bash executable changed or disappeared; rebuild the shell tool group and prepare again",
        })
    }
}

#[cfg(unix)]
#[derive(Debug, Eq, PartialEq)]
struct BashExecutableIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[cfg(unix)]
impl BashExecutableIdentity {
    fn from_metadata(metadata: &Metadata) -> Result<Self, ShellLauncherError> {
        if !metadata.is_file() || metadata.mode() & EXECUTABLE_MODE == 0 {
            return Err(ShellLauncherError::NotExecutable);
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

impl ShellLauncher {
    pub(crate) async fn from_host() -> Result<Self, ShellLauncherError> {
        #[cfg(unix)]
        {
            let configured = std::env::var_os(BASH_EXECUTABLE_ENV)
                .or_else(|| option_env!("WORKCELL_BASH_EXECUTABLE").map(OsString::from))
                .map(PathBuf::from);
            Self::select_bash(configured.as_deref()).await
        }
        #[cfg(windows)]
        {
            let executable =
                PathBuf::from(std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into()));
            if executable.as_os_str().is_empty()
                || executable.as_os_str().len() > MAX_LAUNCHER_PATH_BYTES
            {
                return Err(ShellLauncherError::InvalidPath);
            }
            Ok(Self { executable })
        }
    }

    #[cfg(unix)]
    async fn select_bash(configured: Option<&Path>) -> Result<Self, ShellLauncherError> {
        if let Some(path) = configured {
            return Self::bind_bash(path).await;
        }
        for path in DEFAULT_BASH_EXECUTABLES {
            match Self::bind_bash(Path::new(path)).await {
                Ok(launcher) => return Ok(launcher),
                Err(ShellLauncherError::Unavailable) => {}
                Err(error) => return Err(error),
            }
        }
        Err(ShellLauncherError::Unavailable)
    }

    #[cfg(unix)]
    async fn bind_bash(path: &Path) -> Result<Self, ShellLauncherError> {
        if !path.is_absolute() || path.as_os_str().len() > MAX_LAUNCHER_PATH_BYTES {
            return Err(ShellLauncherError::InvalidPath);
        }
        let executable = tokio::fs::canonicalize(path)
            .await
            .map_err(|_| ShellLauncherError::Unavailable)?;
        if executable.as_os_str().len() > MAX_LAUNCHER_PATH_BYTES {
            return Err(ShellLauncherError::InvalidPath);
        }
        let metadata = tokio::fs::metadata(&executable)
            .await
            .map_err(|_| ShellLauncherError::Unavailable)?;
        Ok(Self {
            executable,
            identity: BashExecutableIdentity::from_metadata(&metadata)?,
        })
    }

    pub(crate) fn bash_executable(&self) -> Option<&Path> {
        #[cfg(unix)]
        {
            Some(&self.executable)
        }
        #[cfg(windows)]
        {
            None
        }
    }

    pub(crate) async fn revalidate(&self) -> Result<(), ShellLauncherError> {
        #[cfg(unix)]
        {
            let metadata = tokio::fs::metadata(&self.executable)
                .await
                .map_err(|_| ShellLauncherError::Changed)?;
            if BashExecutableIdentity::from_metadata(&metadata).as_ref() != Ok(&self.identity) {
                return Err(ShellLauncherError::Changed);
            }
        }
        Ok(())
    }

    pub(crate) fn bash_startup_assumptions(&self) -> Option<BashContextAssumptions> {
        self.bash_executable().map(|_| BashContextAssumptions {
            startup_preserves_cwd: true,
            no_aliases_functions_or_command_not_found_hook: true,
            no_traps: true,
            default_shell_options: true,
            standard_builtins: true,
            directory_variables_are_standard: true,
            cdpath_empty: true,
            lastpipe_disabled: true,
            logical_pwd_matches_initial: true,
        })
    }
}

pub(crate) fn retained_launcher_bytes(launcher: &SharedShellLauncher) -> usize {
    size_of::<Result<ShellLauncher, ShellLauncherError>>()
        .saturating_add(ARC_COUNTER_BYTES)
        .saturating_add(
            launcher
                .as_ref()
                .as_ref()
                .map_or(0, |launcher| launcher.executable.capacity()),
        )
}

#[cfg(unix)]
pub(crate) fn platform_command(launcher: &ShellLauncher, script: &str) -> Command {
    let mut command = Command::new(&launcher.executable);
    command.args(BASH_ARGUMENTS).arg(script);
    clean_environment(&mut command);
    // A dedicated group lets cancellation target descendants that inherited the shell's group.
    command.as_std_mut().process_group(0);
    command
}
#[cfg(windows)]
pub(crate) fn platform_command(launcher: &ShellLauncher, script: &str) -> Command {
    let mut command = Command::new(&launcher.executable);
    command.arg("/D").arg("/S").arg("/C").arg(script);
    clean_environment(&mut command);
    command
}

fn clean_environment(command: &mut Command) {
    clean_environment_with(command, |name: &str| std::env::var_os(name));
}

fn clean_environment_with(command: &mut Command, read: impl Fn(&str) -> Option<OsString>) {
    command.env_clear();
    for name in [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LC_ALL",
        "TERM",
        "ComSpec",
        "SystemRoot",
        "WINDIR",
        // Proxy selection is forwarded verbatim, credentials included. Under a sandbox whose only
        // egress is an enforcing proxy, withholding it makes every network-using command fail
        // closed; the accepted cost is that a credentialed proxy URL becomes readable by any
        // admitted command. Both cases are listed because curl reads the lowercase names while
        // most other clients read the uppercase ones.
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        if let Some(value) = read(name) {
            command.env(name, value);
        }
    }
    // Set, not forwarded. `env_clear` already dropped any inherited `NO_COLOR`,
    // `FORCE_COLOR`, and `CLICOLOR_FORCE`, so this establishes a default for an
    // environment that has none rather than overriding an operator's choice.
    // Not conditioned on the output filter: this describes the environment a
    // command runs in, not how its output is rendered, and a caller who disables
    // filtering still sees exactly what the command wrote. An explicit
    // `--color=always` still wins, which is the intended behaviour.
    command.env("NO_COLOR", "1");
    command.env("CLICOLOR", "0");
}

pub(crate) async fn terminate_and_reap(
    child: &mut Child,
    pid: Option<u32>,
) -> Result<Option<ExitStatus>, String> {
    // Give cooperative processes time to clean up before escalating to an uncatchable signal.
    signal_group(pid, false);
    let deadline = Instant::now() + TERMINATION_GRACE;
    let mut status = None;
    while group_exists(pid) && Instant::now() < deadline {
        if status.is_none() {
            status = child
                .try_wait()
                .map_err(|e| format!("Failed to reap shell: {e}"))?;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if group_exists(pid) {
        signal_group(pid, true);
    }
    #[cfg(windows)]
    {
        let _ = child.kill().await;
    }
    if status.is_none() {
        // Reap the direct child regardless of group state to avoid leaving a zombie.
        status = Some(
            child
                .wait()
                .await
                .map_err(|e| format!("Failed to reap shell: {e}"))?,
        );
    }
    let deadline = Instant::now() + TERMINATION_GRACE;
    while group_exists(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if group_exists(pid) {
        return Err("Shell process group survived SIGKILL".to_owned());
    }
    Ok(status)
}
pub(crate) async fn terminate_residual_group(pid: Option<u32>) {
    // The direct child may have exited while descendants still hold inherited stdout/stderr pipes.
    // Clean those residual members without trying to reap processes that are not our children.
    signal_group(pid, false);
    let deadline = Instant::now() + TERMINATION_GRACE;
    while group_exists(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if group_exists(pid) {
        signal_group(pid, true);
    }
}
#[cfg(unix)]
fn signal_group(pid: Option<u32>, force: bool) {
    use rustix::process::{Pid, Signal, kill_process_group};
    if let Some(pid) = pid
        .and_then(|v| i32::try_from(v).ok())
        .and_then(Pid::from_raw)
    {
        let _ = kill_process_group(pid, if force { Signal::Kill } else { Signal::Term });
    }
}
#[cfg(unix)]
fn group_exists(pid: Option<u32>) -> bool {
    use rustix::process::{Pid, test_kill_process_group};
    pid.and_then(|v| i32::try_from(v).ok())
        .and_then(Pid::from_raw)
        .is_some_and(|pid| test_kill_process_group(pid).is_ok())
}
#[cfg(windows)]
fn signal_group(pid: Option<u32>, _force: bool) {
    // `taskkill` is the available tree primitive here; it is forceful for both TERM and KILL phases.
    if let Some(pid) = pid {
        let mut command = std::process::Command::new("taskkill");
        command.args(["/PID", &pid.to_string(), "/T", "/F"]);
        let _ = command.status();
    }
}
#[cfg(windows)]
fn group_exists(_pid: Option<u32>) -> bool {
    false
}
#[cfg(unix)]
pub(crate) fn exit_signal(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}
#[cfg(windows)]
pub(crate) fn exit_signal(_status: &ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    const LAUNCH_CANARY: &str = "echo launcher-canary";
    const STARTUP_ENVIRONMENT: &[(&str, &str)] = &[
        ("WORKCELL_BASH_EXECUTABLE", "/host/bash"),
        ("BASH_ENV", "startup-canary"),
        ("ENV", "startup-canary"),
        ("SHELLOPTS", "errexit:functrace:noclobber:posix:xtrace"),
        ("BASHOPTS", "expand_aliases:extdebug:lastpipe"),
        ("BASH_COMPAT", "42"),
        ("BASH_FUNC_cd%%", "() { builtin cd /; }"),
        (
            "BASH_FUNC_command_not_found_handle%%",
            "() { builtin cd /; }",
        ),
        ("CDPATH", "/outside"),
        ("PWD", "/outside"),
        ("OLDPWD", "/outside"),
        ("PROMPT_COMMAND", "cd /"),
        ("PS4", "$(cd /)"),
    ];

    #[tokio::test]
    async fn startup_trust_matches_the_actual_platform_launcher() {
        let launcher = ShellLauncher::from_host().await.unwrap();
        let command = platform_command(&launcher, LAUNCH_CANARY);
        let arguments: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(launcher.bash_startup_assumptions().is_some(), cfg!(unix));
        assert_eq!(
            command.as_std().get_program(),
            launcher.executable.as_os_str()
        );
        #[cfg(unix)]
        {
            assert!(launcher.bash_executable().unwrap().is_absolute());
            assert_eq!(
                arguments,
                ["--noprofile", "--norc", "-c", LAUNCH_CANARY].map(OsStr::new)
            );
        }
        #[cfg(windows)]
        assert_eq!(arguments, ["/D", "/S", "/C", LAUNCH_CANARY].map(OsStr::new));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_host_bash_selection_is_absolute_executable_and_authoritative() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing-bash");
        let file = directory.path().join("not-executable");
        tokio::fs::write(&file, b"not executable").await.unwrap();
        for (path, expected) in [
            (Path::new("bash"), ShellLauncherError::InvalidPath),
            (Path::new(""), ShellLauncherError::InvalidPath),
            (missing.as_path(), ShellLauncherError::Unavailable),
            (file.as_path(), ShellLauncherError::NotExecutable),
            (directory.path(), ShellLauncherError::NotExecutable),
        ] {
            assert_eq!(
                ShellLauncher::select_bash(Some(path)).await.err(),
                Some(expected)
            );
        }
    }

    #[test]
    fn startup_files_options_functions_and_directory_variables_are_not_inherited() {
        let inherited = child_environment(STARTUP_ENVIRONMENT);
        for (forbidden, _) in STARTUP_ENVIRONMENT {
            assert!(inherited.iter().all(|(name, _)| name != forbidden));
        }
    }

    /// Resolve the child environment from a fixture instead of the real process environment, which
    /// cannot be mutated from a test without `unsafe` under edition 2024.
    fn child_environment(fixture: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut command = Command::new("ignored");
        for (name, value) in fixture {
            command.env(name, value);
        }
        clean_environment_with(&mut command, |name| {
            fixture
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, value)| OsString::from(*value))
        });
        command
            .as_std()
            .get_envs()
            .filter_map(|(name, value)| {
                value.map(|value| {
                    (
                        name.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn child_environment_drops_runtime_and_provider_secrets() {
        let inherited = child_environment(&[
            ("PATH", "/usr/bin"),
            ("WORKCELL_PRIVATE_SECRET", "private-secret-canary"),
            ("WORKCELL_RUNTIME_SERVICE_TOKEN", "service-secret-canary"),
            ("EXA_API_KEY", "web-secret-canary"),
        ]);
        for forbidden in [
            "WORKCELL_PRIVATE_SECRET",
            "WORKCELL_RUNTIME_SERVICE_TOKEN",
            "EXA_API_KEY",
        ] {
            assert!(inherited.iter().all(|(name, _)| name.as_str() != forbidden));
        }
        assert!(
            inherited
                .iter()
                .all(|(_, value)| !value.contains("secret-canary"))
        );
    }

    #[test]
    fn child_environment_forwards_proxy_configuration() {
        let fixture = [
            ("HTTPS_PROXY", "http://operator:hunter2@proxy.internal:8080"),
            ("http_proxy", "http://proxy.internal:8080"),
            ("ALL_PROXY", "http://proxy.internal:3128"),
            ("NO_PROXY", "localhost,127.0.0.1,10.0.0.0/8"),
        ];
        let inherited = child_environment(&fixture);
        for (name, value) in fixture {
            assert!(
                inherited
                    .iter()
                    .any(|(candidate, forwarded)| candidate.as_str() == name && forwarded == value),
                "{name} must reach the child unmodified"
            );
        }
    }

    #[test]
    fn child_environment_defaults_to_no_colour() {
        // Preventing the bytes is cheaper than deleting them afterwards, and it
        // is the only lever that works on a tool the escape strip would have to
        // rewrite. A value inherited from the parent must not defeat it.
        let inherited = child_environment(&[("NO_COLOR", ""), ("CLICOLOR", "1")]);
        for (name, expected) in [("NO_COLOR", "1"), ("CLICOLOR", "0")] {
            assert!(
                inherited
                    .iter()
                    .any(|(candidate, value)| candidate == name && value == expected),
                "{name} must be set to {expected}"
            );
        }
    }
}
