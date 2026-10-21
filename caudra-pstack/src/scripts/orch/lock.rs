//! The store's pid lock: `.orch.lock` holds the writer's pid, a dead holder's lock is replaced,
//! and a live one is taken only by force.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;

use crate::host::Host;

use super::files::read_text;
use super::model::{OrchError, OrchResult};

pub const LOCK_FILE: &str = ".orch.lock";
const UNKNOWN_HOLDER: &str = "unknown";
/// What `kill -0` reports for a pid that names no process, on Linux and macOS alike.
pub const NO_SUCH_PROCESS: &str = "No such process";

/// Why a lock someone else held was replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockTakeover {
    Stale(String),
    Stolen(String),
}

fn held_by(holder: &str) -> OrchError {
    OrchError::user(format!("store lock held by pid {holder}"))
}

fn holder_of(path: &Path) -> String {
    read_text(path)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|holder| !holder.is_empty())
        .unwrap_or_else(|| UNKNOWN_HOLDER.to_owned())
}

fn create(path: &Path, pid: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(format!("{pid}\n").as_bytes())
}

/// Whether `holder` is a pid with no live process behind it, asked of the host's `kill -0`
/// since the port reaches processes only through [`Host`].
fn holder_is_dead(host: &dyn Host, holder: &str) -> bool {
    let canonical = holder
        .parse::<u32>()
        .is_ok_and(|pid| pid > 0 && pid.to_string() == holder);
    canonical
        && host
            .run(&["kill", "-0", holder], None)
            .is_ok_and(|output| output.code != 0 && output.stderr.contains(NO_SUCH_PROCESS))
}

pub fn acquire(
    host: &dyn Host,
    store: &Path,
    force: bool,
    on_takeover: &mut dyn FnMut(LockTakeover),
) -> OrchResult<()> {
    let path = store.join(LOCK_FILE);
    let pid = std::process::id().to_string();
    match create(&path, &pid) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() != ErrorKind::AlreadyExists => return Err(error.into()),
        Err(_) => {}
    }
    let holder = holder_of(&path);
    if holder_is_dead(host, &holder) {
        on_takeover(LockTakeover::Stale(holder));
    } else if force {
        on_takeover(LockTakeover::Stolen(holder));
    } else {
        return Err(held_by(&holder));
    }
    fs::remove_file(&path)?;
    match create(&path, &pid) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Err(held_by(&holder_of(&path))),
        Err(error) => Err(error.into()),
    }
}

/// Removes the lock if this process still holds it.
pub fn release(store: &Path) -> OrchResult<()> {
    let path = store.join(LOCK_FILE);
    let removed = read_text(&path).and_then(|holder| {
        if holder.trim() == std::process::id().to_string() {
            fs::remove_file(&path)?;
        }
        Ok(())
    });
    match removed {
        Err(OrchError::Io(error)) if error.kind() == ErrorKind::NotFound => Ok(()),
        other => other,
    }
}
