//! Runs a child process to completion within a deadline. Output is read on
//! the side, so a reply larger than a pipe buffer cannot stall the child until
//! the deadline passes.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use thiserror::Error;
use tracing::warn;
use wait_timeout::ChildExt;

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("could not start: {0}")]
    Spawn(#[source] io::Error),
    #[error("no answer within {0:?}")]
    Timeout(Duration),
    #[error("waiting failed: {0}")]
    Wait(#[source] io::Error),
}

pub struct Finished {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runs `command` with stdin closed, stopping it at `timeout`.
pub fn run(command: &mut Command, timeout: Duration) -> Result<Finished, ProcessError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(ProcessError::Spawn)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (status, stdout, stderr) = thread::scope(|scope| {
        let stdout = scope.spawn(move || drain(stdout));
        let stderr = scope.spawn(move || drain(stderr));
        let status = wait(&mut child, timeout);
        (
            status,
            stdout.join().unwrap_or_default(),
            stderr.join().unwrap_or_default(),
        )
    });
    Ok(Finished {
        status: status?,
        stdout,
        stderr,
    })
}

fn drain(pipe: Option<impl Read>) -> Vec<u8> {
    let mut output = Vec::new();
    if let Some(mut pipe) = pipe
        && let Err(error) = pipe.read_to_end(&mut output)
    {
        warn!(%error, "failed to read child output");
    }
    output
}

fn wait(child: &mut Child, timeout: Duration) -> Result<ExitStatus, ProcessError> {
    let error = match child.wait_timeout(timeout) {
        Ok(Some(status)) => return Ok(status),
        Ok(None) => ProcessError::Timeout(timeout),
        Err(error) => ProcessError::Wait(error),
    };
    if let Err(error) = child.kill() {
        warn!(%error, "failed to stop child");
    }
    if let Err(error) = child.wait() {
        warn!(%error, "failed to reap child");
    }
    Err(error)
}
