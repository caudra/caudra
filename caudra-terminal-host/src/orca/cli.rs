use std::ffi::{OsStr, OsString};
use std::process::{Command, ExitStatus};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::HOST;
use crate::bounded_process;
use crate::error::HostError;

const JSON_FLAG: &str = "--json";
/// How Orca refuses a flag or command it does not know, which is also how a
/// release that predates one answers it.
const INVALID_ARGUMENT: &str = "invalid_argument";
const UNKNOWN_PREFIX: &str = "Unknown ";

/// Runs the `orca` CLI, which answers every `--json` call on stdout with an
/// envelope, failures included.
#[derive(Clone, Debug)]
pub(super) struct OrcaCli {
    binary: OsString,
}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: Option<Failure>,
}

#[derive(Deserialize)]
struct Failure {
    code: String,
    message: String,
}

impl OrcaCli {
    pub(super) fn new(binary: OsString) -> Self {
        Self { binary }
    }

    /// The command [`Self::call`] runs, for a caller that has to start it
    /// itself.
    pub(super) fn command(&self, args: &[&OsStr]) -> Command {
        let mut command = Command::new(&self.binary);
        command.args(args).arg(JSON_FLAG);
        command
    }

    pub(super) fn call<T: DeserializeOwned>(
        &self,
        args: &[&OsStr],
        timeout: Duration,
    ) -> Result<T, HostError> {
        let finished = bounded_process::run(&mut self.command(args), timeout)
            .map_err(|error| HostError::Run(HOST, error))?;
        let result = interpret(finished.status, &finished.stdout, &finished.stderr)?;
        serde_json::from_value(result).map_err(|error| HostError::Output(HOST, error))
    }
}

fn interpret(status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> Result<Value, HostError> {
    let failed = || HostError::Failed {
        host: HOST,
        status,
        stderr: String::from_utf8_lossy(stderr.trim_ascii()).into_owned(),
    };
    let envelope = match serde_json::from_slice::<Envelope>(stdout.trim_ascii()) {
        Ok(envelope) => envelope,
        Err(_) if !status.success() => return Err(failed()),
        Err(error) => return Err(HostError::Output(HOST, error)),
    };
    match (envelope.ok, envelope.error) {
        (true, _) if status.success() => Ok(envelope.result),
        (true, _) => Err(failed()),
        (false, Some(error))
            if error.code == INVALID_ARGUMENT && error.message.starts_with(UNKNOWN_PREFIX) =>
        {
            Err(HostError::Unsupported(HOST, error.message))
        }
        (false, Some(error)) => Err(HostError::Refused {
            host: HOST,
            code: error.code,
            message: error.message,
        }),
        (false, None) => Err(failed()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::bounded_process::ProcessError;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tempfile::TempDir;
    use test_case::test_case;

    const TIMEOUT: Duration = Duration::from_secs(10);
    const SHORT_TIMEOUT: Duration = Duration::from_millis(50);
    const RECORD: &str = "argv";
    const CODE: &str = "selector_not_found";
    const MESSAGE: &str = "No worktree matches";

    fn fake_orca(dir: &Path, body: &str) -> OrcaCli {
        let binary = dir.join("orca");
        fs::write(&binary, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        OrcaCli::new(binary.into())
    }

    fn call(cli: &OrcaCli) -> Result<Value, HostError> {
        cli.call(&[OsStr::new("worktree"), OsStr::new("list")], TIMEOUT)
    }

    #[test]
    fn a_call_asks_for_json_and_returns_the_result() {
        let dir = TempDir::new().unwrap();
        let record = dir.path().join(RECORD);
        let cli = fake_orca(
            dir.path(),
            &format!(
                r#"printf '%s\n' "$@" > '{}'; echo '{{"id":"1","ok":true,"result":{{"worktrees":[]}},"_meta":{{}}}}'"#,
                record.display()
            ),
        );

        let result = call(&cli).unwrap();

        assert_eq!(result, serde_json::json!({"worktrees": []}));
        assert_eq!(
            fs::read_to_string(record).unwrap(),
            "worktree\nlist\n--json\n"
        );
    }

    #[test]
    fn a_refusal_carries_code_and_message() {
        let dir = TempDir::new().unwrap();
        let cli = fake_orca(
            dir.path(),
            &format!(
                r#"echo '{{"id":"1","ok":false,"error":{{"code":"{CODE}","message":"{MESSAGE}"}}}}'; exit 1"#
            ),
        );

        let error = call(&cli).unwrap_err();

        assert_eq!(error.code(), Some(CODE));
        assert!(error.to_string().contains(MESSAGE), "{error}");
    }

    #[test]
    fn an_unknown_flag_means_this_orca_predates_it() {
        let dir = TempDir::new().unwrap();
        let cli = fake_orca(
            dir.path(),
            r#"echo '{"id":"local","ok":false,"error":{"code":"invalid_argument","message":"Unknown flag --activate for command: worktree create"}}'; exit 1"#,
        );

        assert!(matches!(call(&cli), Err(HostError::Unsupported(..))));
    }

    #[test_case("echo 'not json'", true ; "unreadable_success")]
    #[test_case("echo 'crashed' >&2; exit 3", false ; "unreadable_failure")]
    #[test_case(r#"echo '{"ok":true,"result":{}}'; exit 1"#, false ; "accepted_but_failed")]
    fn output_that_is_not_an_answer_is_an_error(script: &str, unreadable: bool) {
        let dir = TempDir::new().unwrap();

        let error = call(&fake_orca(dir.path(), script)).unwrap_err();

        assert_eq!(
            matches!(error, HostError::Output(..)),
            unreadable,
            "{error}"
        );
        assert_eq!(
            matches!(error, HostError::Failed { .. }),
            !unreadable,
            "{error}"
        );
    }

    #[test]
    fn a_slow_orca_is_stopped_at_the_timeout() {
        let dir = TempDir::new().unwrap();
        let cli = fake_orca(dir.path(), "exec sleep 30");

        assert!(matches!(
            cli.call::<Value>(&[], SHORT_TIMEOUT),
            Err(HostError::Run(_, ProcessError::Timeout(SHORT_TIMEOUT)))
        ));
    }
}
