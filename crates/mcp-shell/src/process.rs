//! Platform process construction and best-effort process-tree termination.
//!
//! Unix launches each shell as a process-group leader, then applies TERM, a grace period, KILL, and
//! reaping. Process groups cover normal descendants but are not containment: a child may deliberately
//! create a new session/group. Windows uses `taskkill /T /F`; without Job Objects it cannot provide
//! equivalent graceful signaling or a reliable residual-tree existence check.

use std::{
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
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
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

    #[test]
    fn child_environment_drops_runtime_and_provider_secrets() {
        let mut command = Command::new("ignored");
        command
            .env("WORKCELL_PRIVATE_SECRET", "private-secret-canary")
            .env("WORKCELL_RUNTIME_SERVICE_TOKEN", "service-secret-canary")
            .env("EXA_API_KEY", "web-secret-canary");
        clean_environment(&mut command);
        let inherited = command
            .as_std()
            .get_envs()
            .filter_map(|(name, value)| value.map(|value| (name, value)))
            .collect::<Vec<_>>();
        for forbidden in [
            "WORKCELL_PRIVATE_SECRET",
            "WORKCELL_RUNTIME_SERVICE_TOKEN",
            "EXA_API_KEY",
        ] {
            assert!(
                inherited
                    .iter()
                    .all(|(name, _)| name.to_string_lossy() != forbidden)
            );
        }
        assert!(
            inherited
                .iter()
                .all(|(_, value)| !value.to_string_lossy().contains("secret-canary"))
        );
    }
}
