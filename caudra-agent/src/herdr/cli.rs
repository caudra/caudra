use std::ffi::OsString;
use std::process::{Command, ExitStatus};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use super::env::HERDR_SOCKET_PATH;
use crate::bounded_process::{self, ProcessError};

/// How the CLI answers arguments it cannot parse, which is also how a release
/// that predates a subcommand or flag answers it.
const USAGE_EXIT_CODE: i32 = 2;

#[derive(Debug, Error)]
pub enum HerdrError {
    #[error("herdr: {0}")]
    Run(#[source] ProcessError),
    #[error("herdr refused the request ({code}): {message}")]
    Api { code: String, message: String },
    #[error("this Herdr predates the request, update Herdr: {0}")]
    Unsupported(String),
    #[error("herdr exited with {status}: {stderr}")]
    Failed { status: ExitStatus, stderr: String },
    #[error("herdr answered with unreadable output: {0}")]
    Output(#[source] serde_json::Error),
}

impl HerdrError {
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HerdrCli {
    binary: OsString,
    socket_path: OsString,
}

#[derive(Deserialize)]
struct Success {
    result: Value,
}

#[derive(Deserialize)]
struct Failure {
    error: FailureDetail,
}

#[derive(Deserialize)]
struct FailureDetail {
    code: String,
    message: String,
}

impl HerdrCli {
    pub fn new(binary: OsString, socket_path: OsString) -> Self {
        Self {
            binary,
            socket_path,
        }
    }

    /// Runs one command without a shell and returns its `result`, or `None`
    /// for the commands that print nothing on success, such as reports.
    pub fn run(&self, args: &[OsString], timeout: Duration) -> Result<Option<Value>, HerdrError> {
        let finished =
            bounded_process::run(&mut self.command(args), timeout).map_err(HerdrError::Run)?;
        interpret(finished.status, &finished.stdout, &finished.stderr)
    }

    /// The command [`Self::run`] runs, for a caller that has to start it
    /// itself.
    pub fn command(&self, args: &[OsString]) -> Command {
        let mut command = Command::new(&self.binary);
        command.args(args).env(HERDR_SOCKET_PATH, &self.socket_path);
        command
    }
}

fn interpret(
    status: ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Option<Value>, HerdrError> {
    if status.success() {
        let stdout = stdout.trim_ascii();
        if stdout.is_empty() {
            return Ok(None);
        }
        return serde_json::from_slice::<Success>(stdout)
            .map(|success| Some(success.result))
            .map_err(HerdrError::Output);
    }
    if let Some(Failure { error }) = stderr
        .split(|byte| *byte == b'\n')
        .rev()
        .find_map(|line| serde_json::from_slice(line.trim_ascii()).ok())
    {
        return Err(HerdrError::Api {
            code: error.code,
            message: error.message,
        });
    }
    let stderr = String::from_utf8_lossy(stderr.trim_ascii()).into_owned();
    if status.code() == Some(USAGE_EXIT_CODE) {
        return Err(HerdrError::Unsupported(stderr));
    }
    Err(HerdrError::Failed { status, stderr })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tempfile::TempDir;
    use test_case::test_case;

    const SOCKET: &str = "/tmp/herdr test.sock";
    const TIMEOUT: Duration = Duration::from_secs(10);
    const SHORT_TIMEOUT: Duration = Duration::from_millis(50);
    const ERROR_CODE: &str = "dirty_worktree_requires_force";
    const ERROR_MESSAGE: &str = "worktree has uncommitted changes";
    const USAGE_MESSAGE: &str = "unknown option: --";
    const RECORD: &str = "argv";

    fn fake_herdr(dir: &Path, body: &str) -> HerdrCli {
        let binary = dir.join("herdr");
        fs::write(&binary, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        HerdrCli::new(binary.into(), SOCKET.into())
    }

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn run_passes_arguments_verbatim_with_the_socket() {
        let dir = TempDir::new().unwrap();
        let record = dir.path().join(RECORD);
        let cli = fake_herdr(
            dir.path(),
            &format!(
                "printf '%s\\n' \"$HERDR_SOCKET_PATH\" \"$@\" > '{}'",
                record.display()
            ),
        );

        let result = cli
            .run(
                &args(&["pane", "report-agent", "a b", "--", "caudra"]),
                TIMEOUT,
            )
            .unwrap();

        assert_eq!(result, None);
        assert_eq!(
            fs::read_to_string(record).unwrap(),
            format!("{SOCKET}\npane\nreport-agent\na b\n--\ncaudra\n")
        );
    }

    #[test]
    fn run_returns_the_result_of_a_response() {
        let dir = TempDir::new().unwrap();
        let cli = fake_herdr(
            dir.path(),
            r#"echo '{"id":"cli:request","result":{"worktree":{"path":"/w"}}}'"#,
        );

        let result = cli.run(&[], TIMEOUT).unwrap();

        assert_eq!(
            result,
            Some(serde_json::json!({"worktree": {"path": "/w"}}))
        );
    }

    #[test]
    fn structured_failure_carries_code_and_message() {
        let dir = TempDir::new().unwrap();
        let cli = fake_herdr(
            dir.path(),
            &format!(
                r#"echo 'warning first' >&2; echo '{{"id":"cli:request","error":{{"code":"{ERROR_CODE}","message":"{ERROR_MESSAGE}"}}}}' >&2; exit 1"#
            ),
        );

        let error = cli.run(&[], TIMEOUT).unwrap_err();

        assert_eq!(error.code(), Some(ERROR_CODE));
        assert!(error.to_string().contains(ERROR_MESSAGE), "{error}");
    }

    #[test_case(2, true ; "usage_error_means_unsupported")]
    #[test_case(3, false ; "other_exit_is_a_failure")]
    fn unstructured_failure_keeps_stderr(code: i32, unsupported: bool) {
        let dir = TempDir::new().unwrap();
        let cli = fake_herdr(
            dir.path(),
            &format!("echo '{USAGE_MESSAGE}' >&2; exit {code}"),
        );

        let error = cli.run(&[], TIMEOUT).unwrap_err();

        assert_eq!(matches!(error, HerdrError::Unsupported(_)), unsupported);
        assert!(error.to_string().contains(USAGE_MESSAGE), "{error}");
    }

    #[test]
    fn unreadable_success_output_is_an_error() {
        let dir = TempDir::new().unwrap();
        let cli = fake_herdr(dir.path(), "echo 'not json'");

        assert!(matches!(cli.run(&[], TIMEOUT), Err(HerdrError::Output(_))));
    }

    #[test]
    fn slow_command_is_stopped_at_the_timeout() {
        let dir = TempDir::new().unwrap();
        let cli = fake_herdr(dir.path(), "exec sleep 30");

        assert!(matches!(
            cli.run(&[], SHORT_TIMEOUT),
            Err(HerdrError::Run(ProcessError::Timeout(SHORT_TIMEOUT)))
        ));
    }

    #[test]
    fn missing_binary_fails_to_spawn() {
        let dir = TempDir::new().unwrap();
        let cli = HerdrCli::new(dir.path().join("herdr").into(), SOCKET.into());

        assert!(matches!(
            cli.run(&[], TIMEOUT),
            Err(HerdrError::Run(ProcessError::Spawn(_)))
        ));
    }
}
