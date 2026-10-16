//! The decision endpoint every new session screens with, a global state row.

use tracing::warn;

use crate::state::{self, SCOPE_GLOBAL, StateKey};
use crate::{StateClass, StateDir, StorageError};

const ACTIVE: StateKey = StateKey {
    name: "decisions.active",
    class: StateClass::Persistent,
};

/// The active endpoint's name. An absent or unreadable row means decisions are off.
pub fn read_active_decision_endpoint(dir: &StateDir) -> Option<String> {
    state::get::<String>(dir, SCOPE_GLOBAL, ACTIVE)
        .unwrap_or_else(|error| {
            warn!(%error, "failed to read the active decision endpoint");
            None
        })
        .filter(|name| !name.is_empty())
}

/// `None` turns decisions off. The error reaches the caller because the user
/// asked for the switch and has to hear when it did not stick.
pub fn persist_active_decision_endpoint(
    dir: &StateDir,
    name: Option<&str>,
) -> Result<(), StorageError> {
    match name {
        Some(name) => state::set(dir, SCOPE_GLOBAL, ACTIVE, &name),
        None => state::delete(dir, SCOPE_GLOBAL, ACTIVE).map(drop),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const TYPESAFE: &str = "typesafe";
    const TUNNEL: &str = "tunnel";

    #[test_case(&[Some(TYPESAFE)], Some(TYPESAFE); "set")]
    #[test_case(&[Some(TYPESAFE), Some(TUNNEL)], Some(TUNNEL); "replace")]
    #[test_case(&[Some(TYPESAFE), None], None; "off")]
    #[test_case(&[None], None; "off_without_row")]
    #[test_case(&[], None; "absent")]
    fn active_endpoint_round_trip(writes: &[Option<&str>], expected: Option<&str>) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        for name in writes {
            persist_active_decision_endpoint(&dir, *name).unwrap();
        }
        assert_eq!(read_active_decision_endpoint(&dir).as_deref(), expected);
    }
}
