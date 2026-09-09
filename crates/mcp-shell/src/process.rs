//! Platform process construction and best-effort process-tree termination.
//!
//! Unix launches each shell as a process-group leader, then applies TERM, a grace period, KILL, and
//! reaping. Process groups cover normal descendants but are not containment: a child may deliberately
//! create a new session/group. Windows uses `taskkill /T /F`; without Job Objects it cannot provide
//! equivalent graceful signaling or a reliable residual-tree existence check.

use std::{
    ffi::OsString,
    process::ExitStatus,
    time::{Duration, Instant},
};
use tokio::process::{Child, Command};

const TERMINATION_GRACE: Duration = Duration::from_secs(3);

#[cfg(unix)]
pub(crate) fn platform_command(script: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new("bash");
    command.arg("-lc").arg(script);
    clean_environment(&mut command);
    // A dedicated group lets cancellation target descendants that inherited the shell's group.
    command.as_std_mut().process_group(0);
    command
}
#[cfg(windows)]
pub(crate) fn platform_command(script: &str) -> Command {
    let mut command = Command::new(std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into()));
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
    use super::*;

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
