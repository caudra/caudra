//! One command a pstack script runs: in the session's project on this machine, in a process group
//! of its own, with the environment a `shell` command gets. Each stream is capped, the call's
//! cancellation kills the whole group, and a normal exit kills what is left of it, so a grandchild
//! still holding the output pipes cannot hold the call open.

use std::ffi::OsString;
use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use caudra_pstack::Output;
use futures_lite::{AsyncRead, AsyncReadExt, future};

use crate::ChildGuard;
use crate::cancel::CancelToken;

/// The most either stream may print before the command is stopped.
pub(super) const MAX_STREAM_BYTES: usize = 4 * 1024 * 1024;
pub(super) const COMMAND_CANCELLED: &str = "cancelled";
/// What a command killed by a signal exits with, as a shell reports it.
const SIGNALLED: i32 = 128;
const STDOUT: &str = "stdout";
const STDERR: &str = "stderr";

/// `program args` in `cwd` with exactly `environment`.
pub(super) fn command(
    program: &str,
    args: &[&str],
    cwd: &Path,
    environment: &[(String, OsString)],
) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(environment.iter().map(|(name, value)| (name, value)));
    #[cfg(unix)]
    command.process_group(0);
    command
}

pub(super) async fn run(command: Command, cancel: &CancelToken) -> io::Result<Output> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = async_process::Command::from(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut guard = ChildGuard::new(child);
    let finished = cancel
        .race(async {
            let reading = future::try_zip(
                capped(stdout, &program, STDOUT),
                capped(stderr, &program, STDERR),
            );
            let waiting = async {
                let status = guard.status().await?;
                guard.kill_leftovers();
                Ok(status)
            };
            future::try_zip(reading, waiting).await
        })
        .await;
    let error = match finished {
        Ok(Ok(((stdout, stderr), status))) => {
            return Ok(Output {
                code: status.code().unwrap_or(SIGNALLED),
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
            });
        }
        Ok(Err(error)) => error,
        Err(_) => io::Error::new(io::ErrorKind::Interrupted, COMMAND_CANCELLED),
    };
    guard.kill_and_reap().await;
    Err(error)
}

async fn capped(
    stream: Option<impl AsyncRead + Unpin>,
    program: &str,
    name: &str,
) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(stream) = stream {
        stream
            .take(MAX_STREAM_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
    }
    if bytes.len() > MAX_STREAM_BYTES {
        return Err(io::Error::other(too_much(program, name)));
    }
    Ok(bytes)
}

pub(super) fn too_much(program: &str, stream: &str) -> String {
    format!("`{program}` printed more than {MAX_STREAM_BYTES} bytes to {stream}, so it was stopped")
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs::File;
    use std::io::Read;
    use std::thread;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const SHELL: &str = "sh";
    const FIFO: &str = "alive";
    const MKFIFO: &str = "mkfifo";
    const PATH: &str = "PATH";

    fn shell(script: &str, args: &[&str]) -> Command {
        let argv: Vec<&str> = ["-c", script]
            .into_iter()
            .chain(args.iter().copied())
            .collect();
        let path = std::env::var_os(PATH).map(|value| (PATH.to_owned(), value));
        command(SHELL, &argv, &std::env::temp_dir(), &Vec::from_iter(path))
    }

    fn fifo(dir: &TempDir) -> String {
        let path = dir.path().join(FIFO);
        assert!(Command::new(MKFIFO).arg(&path).status().unwrap().success());
        path.to_string_lossy().into_owned()
    }

    /// The grandchild holds the FIFO's only writer, so the read ends exactly when it dies.
    #[test]
    fn a_cancelled_command_kills_its_grandchild() {
        let dir = TempDir::new().unwrap();
        let fifo = fifo(&dir);
        let (stop, cancel) = CancelToken::new();
        let running = {
            let fifo = fifo.clone();
            thread::spawn(move || {
                smol::block_on(run(shell("sleep 600 > \"$0\" & wait", &[&fifo]), &cancel))
            })
        };
        let mut alive = File::open(&fifo).unwrap();
        stop.cancel();
        let error = running.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let mut rest = Vec::new();
        assert_eq!(alive.read_to_end(&mut rest).unwrap(), 0);
    }

    #[test]
    fn a_grandchild_left_holding_the_pipes_cannot_hold_the_call() {
        let output = smol::block_on(run(
            shell("sleep 600 & printf done", &[]),
            &CancelToken::none(),
        ))
        .unwrap();
        assert_eq!((output.code, output.stdout.as_str()), (0, "done"));
    }

    #[test_case(MAX_STREAM_BYTES, true ; "at_the_cap_kept")]
    #[test_case(MAX_STREAM_BYTES + 1, false ; "over_the_cap_refused")]
    fn output_is_capped(bytes: usize, kept: bool) {
        let script = format!("head -c {bytes} /dev/zero");
        let result = smol::block_on(run(shell(&script, &[]), &CancelToken::none()));
        match kept {
            true => assert_eq!(result.unwrap().stdout.len(), bytes),
            false => assert_eq!(result.unwrap_err().to_string(), too_much(SHELL, STDOUT)),
        }
    }
}
