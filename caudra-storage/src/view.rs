//! The persisted transcript density, so `/view` survives a restart.

use std::fs;

use tracing::warn;

use crate::StateDir;

const VIEW_FILE: &str = "view";
const COMPACT: &str = "compact";
const EXPANDED: &str = "expanded";

pub fn persist_compact(dir: &StateDir, compact: bool) {
    let density = if compact { COMPACT } else { EXPANDED };
    if let Err(e) = fs::write(dir.path().join(VIEW_FILE), density) {
        warn!(error = %e, "failed to persist view density");
    }
}

/// `None` when nothing was stored or the file names a density this build does
/// not know, which leaves the caller's own default standing.
pub fn read_compact(dir: &StateDir) -> Option<bool> {
    let density = fs::read_to_string(dir.path().join(VIEW_FILE)).ok()?;
    match density.trim() {
        COMPACT => Some(true),
        EXPANDED => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const UNSET: &str = "an unwritten density must leave the caller's default alone";
    const ROUND_TRIP: &str = "a stored density must read back as written";
    const UNKNOWN: &str = "a density this build cannot read is not a density";

    #[test]
    fn view_density_round_trip() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        assert_eq!(read_compact(&dir), None, "{UNSET}");

        persist_compact(&dir, true);
        assert_eq!(read_compact(&dir), Some(true), "{ROUND_TRIP}");

        persist_compact(&dir, false);
        assert_eq!(read_compact(&dir), Some(false), "{ROUND_TRIP}");

        fs::write(dir.path().join(VIEW_FILE), "roomy\n").unwrap();
        assert_eq!(read_compact(&dir), None, "{UNKNOWN}");
    }
}
