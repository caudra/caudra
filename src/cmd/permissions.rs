mod audit;
pub mod discover;
mod inventory;
mod repair;

use std::fs;
use std::fs::File;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use caudra_agent::permissions::rebind::{self, DestinationIdentity, RebindCandidates};
use caudra_storage::permission_state::read_inventory;
use caudra_storage::sessions::SESSIONS_DB_FILE;
use caudra_storage::{StateClass, StateDir};
use color_eyre::eyre::{Context, Result, bail, eyre};
use serde_json::json;

use crate::cli::PermissionAction;

const MAX_CANDIDATE_BYTES: u64 = 1024 * 1024;
const INVALID_DATABASE_PATH: &str = "--database requires an existing canonical absolute path named caudra.db, without symlink or hard-link aliases";
const AUDIT_DATABASE_UNSUPPORTED: &str =
    "--database is not accepted by permissions audit; use --log to select its input";
const LIMITATIONS: &[&str] = &[
    "Destination transfer is fresh authorization, not proof of historical identity continuity.",
    "Only explicit hash-verified candidates are supported; no history recovery or hash inversion.",
    "Exact/selected input digests and unsupported selectors require fresh grants.",
    "Unknown restrictive rules block related allows; a subset is not equivalent full-policy migration.",
    "Project trust and YOLO are never transferred. Apply requires all sessions and storage readers closed.",
    "Deny and Ask originals remain active at the source; only Allow originals are retired.",
    "Directory handles and pre-commit checks detect identity changes during review, but permissions remain path-bound after commit, not inode-bound.",
    "Read-only inspection does not update logical database state; SQLite may update existing WAL coordination sidecars.",
];

pub fn run(action: PermissionAction, database: Option<PathBuf>) -> Result<()> {
    if let PermissionAction::Audit {
        log,
        since,
        max_bytes,
    } = action
    {
        if database.is_some() {
            bail!(AUDIT_DATABASE_UNSUPPORTED);
        }
        return audit::run(log, since, max_bytes);
    }
    let state_dir = selected_database(database.as_deref())?;
    match action {
        PermissionAction::Audit { .. } => {
            bail!(AUDIT_DATABASE_UNSUPPORTED);
        }
        PermissionAction::Discover {
            project,
            limit,
            since,
            json,
        } => {
            discover::run(&state_dir, project, limit, since, json)?;
        }
        PermissionAction::Inventory {
            project,
            known_root,
            json,
        } => inventory::run(&state_dir, project, known_root, json)?,
        PermissionAction::RepairReview {
            apply,
            json,
            retry_unavailable,
        } => repair::run(&state_dir, apply, json, retry_unavailable)?,
        PermissionAction::Rebind {
            old_root,
            new_root,
            candidates,
            select,
            apply,
            confirm,
        } => {
            let candidates = candidates
                .as_deref()
                .map(load_candidates)
                .transpose()?
                .unwrap_or_default();
            let destination = DestinationIdentity::inspect(&new_root)?;
            if apply {
                let confirmation = confirm
                    .as_deref()
                    .ok_or_else(|| eyre!("--apply requires --confirm"))?;
                let inserted = rebind::apply(
                    &state_dir,
                    &old_root,
                    destination,
                    &candidates,
                    &select,
                    confirmation,
                )?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "database": state_dir.path().join(SESSIONS_DB_FILE), "applied": inserted, "limitations": LIMITATIONS })
                    )?
                );
            } else {
                let records = read_inventory(&state_dir)?;
                let preview =
                    rebind::preview(&records, &old_root, destination, &candidates, &select)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "dry_run": true,
                        "database": state_dir.path().join(SESSIONS_DB_FILE),
                        "confirmation": preview.confirmation()?,
                        "can_apply": preview.can_apply(),
                        "preview": preview,
                        "limitations": LIMITATIONS,
                    }))?
                );
            }
        }
    }
    Ok(())
}

fn selected_database(path: Option<&Path>) -> Result<StateDir> {
    let Some(path) = path else {
        return Ok(StateDir::resolve_without_create()
            .context("resolve permission storage directory")?
            .for_class(StateClass::Persistent));
    };
    if !path.is_absolute() || path.file_name().is_none_or(|name| name != SESSIONS_DB_FILE) {
        bail!(INVALID_DATABASE_PATH);
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| eyre!(INVALID_DATABASE_PATH))?;
    if !metadata.is_file()
        || fs::canonicalize(path)
            .map_err(|_| eyre!(INVALID_DATABASE_PATH))?
            .as_os_str()
            != path.as_os_str()
    {
        bail!(INVALID_DATABASE_PATH);
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        bail!(INVALID_DATABASE_PATH);
    }
    let parent = path.parent().ok_or_else(|| eyre!(INVALID_DATABASE_PATH))?;
    Ok(StateDir::from_path(parent.into()))
}

fn load_candidates(path: &Path) -> Result<RebindCandidates> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_CANDIDATE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CANDIDATE_BYTES {
        bail!("candidate file exceeds {MAX_CANDIDATE_BYTES} bytes");
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        eyre!(
            "invalid permission candidate JSON at line {}, column {}",
            error.line(),
            error.column()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AUDIT_DATABASE_UNSUPPORTED, INVALID_DATABASE_PATH, load_candidates, run, selected_database,
    };
    use crate::cli::PermissionAction;
    use caudra_storage::sessions::SESSIONS_DB_FILE;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use test_case::test_case;

    const SECRET: &str = "candidate-private-command";

    #[test_case("copy"; "explicit_copy")]
    fn database_selection_is_exact_and_noncreating(directory: &str) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap().join(directory);
        fs::create_dir(&root).unwrap();
        let path = root.join(SESSIONS_DB_FILE);
        fs::write(&path, []).unwrap();
        let state = selected_database(Some(&path)).unwrap();
        assert_eq!(state.path(), root);
        assert_eq!(fs::read_dir(root).unwrap().count(), 1);
    }

    #[test_case("wrong-name"; "wrong_basename")]
    #[test_case("missing"; "missing_file")]
    #[test_case("directory"; "directory_not_database")]
    #[test_case("relative"; "relative_path")]
    fn database_selection_refuses_ambiguous_or_missing_paths(case: &str) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let path = match case {
            "wrong-name" => {
                let path = root.join("other.sqlite");
                fs::write(&path, []).unwrap();
                path
            }
            "directory" => {
                let path = root.join(SESSIONS_DB_FILE);
                fs::create_dir(&path).unwrap();
                path
            }
            "relative" => Path::new(SESSIONS_DB_FILE).to_path_buf(),
            _ => root.join(SESSIONS_DB_FILE),
        };
        let before = fs::read_dir(&root).unwrap().count();
        let error = selected_database(Some(&path)).err().unwrap();
        assert_eq!(error.to_string(), INVALID_DATABASE_PATH);
        assert_eq!(fs::read_dir(root).unwrap().count(), before);
    }

    #[cfg(unix)]
    #[test_case("file"; "file_symlink")]
    #[test_case("parent"; "directory_symlink")]
    #[test_case("hard-link"; "hard_link_alias")]
    fn database_selection_refuses_aliases(case: &str) {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let original = root.join("original");
        fs::create_dir(&original).unwrap();
        let database = original.join(SESSIONS_DB_FILE);
        fs::write(&database, []).unwrap();
        let alias = root.join("alias");
        if case == "parent" {
            symlink(&original, &alias).unwrap();
        } else {
            fs::create_dir(&alias).unwrap();
            if case == "file" {
                symlink(&database, alias.join(SESSIONS_DB_FILE)).unwrap();
            } else {
                fs::hard_link(&database, alias.join(SESSIONS_DB_FILE)).unwrap();
            }
        }
        assert_eq!(
            selected_database(Some(&alias.join(SESSIONS_DB_FILE)))
                .err()
                .unwrap()
                .to_string(),
            INVALID_DATABASE_PATH
        );
    }

    #[test_case("/not/opened/caudra.db"; "audit_rejects_database_without_opening_it")]
    fn audit_requires_log_selection(path: &str) {
        let action = PermissionAction::Audit {
            log: None,
            since: None,
            max_bytes: None,
        };
        assert_eq!(
            run(action, Some(path.into())).unwrap_err().to_string(),
            AUDIT_DATABASE_UNSUPPORTED
        );
    }

    #[test_case("{\"paths\":\"candidate-private-command\"}"; "wrong_type")]
    #[test_case("{\"candidate-private-command\":[]}"; "unknown_field")]
    #[test_case("{\"values\":[\"candidate-private-command\"]"; "syntax")]
    fn candidate_parse_errors_never_echo_values_or_fields(input: &str) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("candidates.json");
        fs::write(&path, input).unwrap();
        let error = load_candidates(&path).unwrap_err();
        let rendered = format!("{error:?}");
        assert!(!rendered.contains(SECRET));
        assert_eq!(error.chain().count(), 1);
        assert!(rendered.contains("line"));
        assert!(rendered.contains("column"));
    }
}
