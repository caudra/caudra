use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{StateDir, StorageError, atomic_write_permissions, exclusive_state_lock};

const TRUST_FILE: &str = "mcp-trust.json";
const TRUST_FILE_MODE: u32 = 0o600;
const TRUST_LOCK_FILE: &str = "mcp-trust.lock";

#[derive(Default, Deserialize, Serialize)]
struct McpTrust {
    projects: HashMap<PathBuf, HashMap<String, String>>,
}

pub fn is_project_trusted(
    state_dir: &StateDir,
    project: &Path,
    server: &str,
    config_digest: &str,
) -> Result<bool, StorageError> {
    let trust = load(state_dir)?;
    let project = project.canonicalize()?;
    Ok(trust
        .projects
        .get(&project)
        .and_then(|servers| servers.get(server))
        .is_some_and(|digest| digest == config_digest))
}

pub fn trust_project(
    state_dir: &StateDir,
    project: &Path,
    server: &str,
    config_digest: &str,
) -> Result<(), StorageError> {
    let _lock = exclusive_state_lock(&state_dir.path().join(TRUST_LOCK_FILE), TRUST_FILE_MODE)?;
    let mut trust = load(state_dir)?;
    let project = project.canonicalize()?;
    trust
        .projects
        .entry(project)
        .or_default()
        .insert(server.to_string(), config_digest.to_string());
    save(state_dir, &trust)
}

pub fn revoke_project_trust(
    state_dir: &StateDir,
    project: &Path,
    server: &str,
) -> Result<(), StorageError> {
    let _lock = exclusive_state_lock(&state_dir.path().join(TRUST_LOCK_FILE), TRUST_FILE_MODE)?;
    let mut trust = load(state_dir)?;
    let project = project.canonicalize()?;
    if let Some(servers) = trust.projects.get_mut(&project) {
        servers.remove(server);
        if servers.is_empty() {
            trust.projects.remove(&project);
        }
    }
    save(state_dir, &trust)
}

fn path(state_dir: &StateDir) -> PathBuf {
    state_dir.path().join(TRUST_FILE)
}

fn load(state_dir: &StateDir) -> Result<McpTrust, StorageError> {
    match fs::read(path(state_dir)) {
        Ok(data) => Ok(serde_json::from_slice(&data)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(McpTrust::default()),
        Err(error) => Err(error.into()),
    }
}

fn save(state_dir: &StateDir, trust: &McpTrust) -> Result<(), StorageError> {
    fs::create_dir_all(state_dir.path())?;
    let data = serde_json::to_vec_pretty(trust)?;
    atomic_write_permissions(&path(state_dir), &data, TRUST_FILE_MODE)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const DIGEST: &str = "0123456789abcdef";

    #[test]
    fn trust_is_exact_to_project_server_and_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(tmp.path().join("state"));
        let project = tmp.path().join("project");
        let other_project = tmp.path().join("other");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&other_project).unwrap();

        trust_project(&state_dir, &project, "server", DIGEST).unwrap();

        assert!(is_project_trusted(&state_dir, &project, "server", DIGEST).unwrap());
        assert!(!is_project_trusted(&state_dir, &project, "other", DIGEST).unwrap());
        assert!(!is_project_trusted(&state_dir, &other_project, "server", DIGEST).unwrap());
        assert!(!is_project_trusted(&state_dir, &project, "server", "changed").unwrap());

        revoke_project_trust(&state_dir, &project, "server").unwrap();
        assert!(!is_project_trusted(&state_dir, &project, "server", DIGEST).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn trust_file_is_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(tmp.path().join("state"));
        let project = tmp.path().join("project");
        fs::create_dir(&project).unwrap();
        trust_project(&state_dir, &project, "server", DIGEST).unwrap();

        let mode = fs::metadata(path(&state_dir)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, TRUST_FILE_MODE);
    }
}
