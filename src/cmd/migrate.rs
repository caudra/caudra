use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use color_eyre::Result;
use color_eyre::eyre::Context;
use maki_storage::input_history::MAX_ENTRIES;
use maki_storage::paths;
use maki_storage::sessions::{SESSIONS_DB_FILE, SESSIONS_DB_LOCK_FILE, SessionDatabase};
use maki_storage::{StateDir, lock_session_artifacts};
use tempfile::NamedTempFile;

#[cfg(unix)]
const AUTH_FILE_MODE: u32 = 0o600;
const TOOL_OUTPUT_DIR: &str = "tool-output";
const SESSION_SNAPSHOT_DIR: &str = "session-snapshots";
const RETAINED_SESSION_STATE: [&str; 3] = ["sessions", TOOL_OUTPUT_DIR, SESSION_SNAPSHOT_DIR];

fn tilde(path: &Path) -> String {
    match paths::home() {
        Some(home) if path.starts_with(&home) => {
            format!("~/{}", path.strip_prefix(&home).unwrap().display())
        }
        _ => path.display().to_string(),
    }
}

fn log_move(name: &str, dst: &Path, note: Option<&str>) {
    match note {
        Some(n) => println!("  {name:<22}-> {} ({n})", tilde(dst)),
        None => println!("  {name:<22}-> {}", tilde(dst)),
    }
}

fn move_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", tilde(parent)))?;
    }
    match maki_storage::durable_rename(src, dst) {
        Ok(()) => {
            sync_parent(dst)?;
            sync_parent(src)?;
            Ok(())
        }
        Err(e) if is_cross_device(&e) => {
            fs::copy(src, dst).with_context(|| format!("copy {} -> {}", tilde(src), tilde(dst)))?;
            #[cfg(unix)]
            {
                let mode = fs::metadata(src)
                    .map(|m| m.permissions().mode())
                    .unwrap_or(0o644);
                fs::set_permissions(dst, fs::Permissions::from_mode(mode)).ok();
            }
            fs::File::open(dst)?.sync_all()?;
            sync_parent(dst)?;
            remove_file_durable(src).with_context(|| format!("remove source {}", tilde(src)))?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("move {} -> {}", tilde(src), tilde(dst))),
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn remove_file_durable(path: &Path) -> Result<()> {
    fs::remove_file(path)?;
    sync_parent(path)
}

fn write_file_atomically(path: &Path, data: &[u8]) -> Result<()> {
    maki_storage::atomic_write(path, data)?;
    Ok(())
}

#[cfg(unix)]
fn is_cross_device(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::EXDEV)
}

#[cfg(windows)]
fn is_cross_device(e: &std::io::Error) -> bool {
    // ERROR_NOT_SAME_DEVICE
    e.raw_os_error() == Some(17)
}

#[cfg(not(any(unix, windows)))]
fn is_cross_device(_e: &std::io::Error) -> bool {
    false
}

fn move_auth(legacy_dir: &Path, target_dir: &Path) -> Result<()> {
    if !legacy_dir.is_dir() {
        return Ok(());
    }

    let entries: Vec<_> = fs::read_dir(legacy_dir)
        .with_context(|| format!("read {}", tilde(legacy_dir)))?
        .filter_map(|e| e.ok())
        .collect();

    if entries.is_empty() {
        return Ok(());
    }

    create_directory_tree_durable(target_dir)
        .with_context(|| format!("create {}", tilde(target_dir)))?;

    let count = entries.len();
    for entry in &entries {
        let dst = target_dir.join(entry.file_name());
        if dst.exists() {
            fs::remove_file(&dst).ok();
        }
        move_file(&entry.path(), &dst)?;
        #[cfg(unix)]
        if fs::set_permissions(&dst, fs::Permissions::from_mode(AUTH_FILE_MODE)).is_ok() {
            fs::File::open(&dst)?.sync_all()?;
        }
    }
    fs::remove_dir(legacy_dir).ok();

    let plural = if count == 1 { "" } else { "s" };
    log_move("auth/", target_dir, Some(&format!("{count} file{plural}")));
    Ok(())
}

fn merge_json_file(legacy: &Path, target: &Path, name: &str) -> Result<()> {
    if !legacy.exists() {
        return Ok(());
    }

    if !target.exists() {
        move_file(legacy, target)?;
        log_move(name, target.parent().unwrap_or(target), None);
        return Ok(());
    }

    let legacy_bytes = fs::read(legacy)?;
    let target_bytes = fs::read(target)?;

    let mut merged: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&target_bytes).unwrap_or_default();
    let legacy_map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&legacy_bytes).unwrap_or_default();
    merged.extend(legacy_map);

    write_file_atomically(target, &serde_json::to_vec_pretty(&merged)?)
        .with_context(|| format!("write {}", tilde(target)))?;
    remove_file_durable(legacy)?;
    log_move(name, target.parent().unwrap_or(target), Some("merged"));
    Ok(())
}

fn merge_input_history(legacy: &Path, target: &Path) -> Result<()> {
    if !legacy.exists() {
        return Ok(());
    }

    let legacy_items: Vec<String> = serde_json::from_slice(&fs::read(legacy)?).unwrap_or_default();

    if !target.exists() {
        move_file(legacy, target)?;
        log_move(
            "input_history.json",
            target.parent().unwrap_or(target),
            Some(&format!("{} entries", legacy_items.len())),
        );
        return Ok(());
    }

    let target_items: Vec<String> = serde_json::from_slice(&fs::read(target)?).unwrap_or_default();

    let mut merged = Vec::with_capacity(target_items.len() + legacy_items.len());
    merged.extend(target_items);
    merged.extend(legacy_items);
    merged.dedup();
    merged.truncate(MAX_ENTRIES);

    write_file_atomically(target, &serde_json::to_vec(&merged)?)
        .with_context(|| format!("write {}", tilde(target)))?;
    remove_file_durable(legacy)?;
    log_move(
        "input_history.json",
        target.parent().unwrap_or(target),
        Some(&format!("merged, {} entries", merged.len())),
    );
    Ok(())
}

fn merge_dir(legacy: &Path, target: &Path, subdir: &str, recursive: bool) -> Result<(u32, u32)> {
    let src = legacy.join(subdir);
    let dst = target.join(subdir);
    if !src.is_dir() {
        return Ok((0, 0));
    }
    create_directory_tree_durable(&dst).with_context(|| format!("create {}", tilde(&dst)))?;

    let mut moved = 0u32;
    let mut skipped = 0u32;

    for entry in fs::read_dir(&src).with_context(|| format!("read {}", tilde(&src)))? {
        let entry = entry?;
        let entry_dst = dst.join(entry.file_name());

        if entry_dst.exists() {
            if recursive && entry.file_type()?.is_dir() {
                let sub = format!("{subdir}/{}", entry.file_name().to_string_lossy());
                let (m, s) = merge_dir(legacy, target, &sub, true)?;
                moved += m;
                skipped += s;
            } else {
                skipped += 1;
            }
        } else {
            move_file(&entry.path(), &entry_dst)?;
            moved += 1;
        }
    }

    fs::remove_dir(&src).ok();

    if moved > 0 || skipped > 0 {
        let kind = if recursive { "dirs" } else { "files" };
        let mut note = format!("{moved} {kind}");
        if skipped > 0 {
            note.push_str(&format!(", {skipped} skipped"));
        }
        log_move(&format!("{subdir}/"), &dst, Some(&note));
    }

    Ok((moved, skipped))
}

fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    if fs::metadata(left)?.len() != fs::metadata(right)?.len() {
        return Ok(false);
    }
    let mut left = fs::File::open(left)?;
    let mut right = fs::File::open(right)?;
    let mut left_buffer = [0u8; 64 * 1024];
    let mut right_buffer = [0u8; 64 * 1024];
    loop {
        let left_count = left.read(&mut left_buffer)?;
        let right_count = right.read(&mut right_buffer)?;
        if left_count != right_count || left_buffer[..left_count] != right_buffer[..right_count] {
            return Ok(false);
        }
        if left_count == 0 {
            return Ok(true);
        }
    }
}

fn copy_file_atomically(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent)?;
    let mut source_options = fs::OpenOptions::new();
    source_options.read(true);
    #[cfg(unix)]
    source_options.custom_flags(libc::O_NOFOLLOW);
    let mut source_file = source_options.open(source)?;
    std::io::copy(&mut source_file, &mut temporary)?;
    temporary.flush()?;
    #[cfg(unix)]
    fs::set_permissions(temporary.path(), fs::metadata(source)?.permissions())?;
    temporary.as_file().sync_all()?;
    #[cfg(windows)]
    {
        let (file, temporary_path) = temporary.keep().map_err(|error| error.error)?;
        drop(file);
        if let Err(error) = maki_storage::durable_rename_noreplace(&temporary_path, destination) {
            let _ = fs::remove_file(temporary_path);
            return Err(error.into());
        }
    }
    #[cfg(not(windows))]
    temporary
        .persist_noclobber(destination)
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn create_directory_tree_durable(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(color_eyre::eyre::eyre!(
                "{} is not a real directory",
                tilde(path)
            ));
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if parent != path {
        create_directory_tree_durable(parent)?;
    }
    match fs::create_dir(path) {
        Ok(()) => {
            #[cfg(unix)]
            fs::File::open(parent)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(color_eyre::eyre::eyre!(
            "{} is not a real directory",
            tilde(path)
        ));
    }
    Ok(())
}

fn copy_session_state(legacy: &Path, target: &Path, subdir: &Path) -> Result<(u32, u32)> {
    let src = legacy.join(subdir);
    let dst = target.join(subdir);
    let source_metadata = match fs::symlink_metadata(&src) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => return Err(error.into()),
    };
    if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
        return Err(color_eyre::eyre::eyre!(
            "session state source {} is not a real directory",
            tilde(&src)
        ));
    }
    if !target.is_dir() {
        return Err(color_eyre::eyre::eyre!(
            "session state target root {} is not a directory",
            tilde(target)
        ));
    }
    if fs::symlink_metadata(target)?.file_type().is_symlink() {
        return Err(color_eyre::eyre::eyre!(
            "session state target root {} cannot be a symlink",
            tilde(target)
        ));
    }
    if subdir.as_os_str().is_empty() {
        return Ok((0, 0));
    }
    create_directory_tree_durable(&dst).with_context(|| format!("create {}", tilde(&dst)))?;
    let mut copied = 0;
    let mut skipped = 0;
    for entry in fs::read_dir(&src).with_context(|| format!("read {}", tilde(&src)))? {
        let entry = entry?;
        let relative = subdir.join(entry.file_name());
        let destination = target.join(&relative);
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(color_eyre::eyre::eyre!(
                "session state source {} is a symlink",
                tilde(&entry.path())
            ));
        }
        if file_type.is_dir() {
            let (nested_copied, nested_skipped) = copy_session_state(legacy, target, &relative)?;
            copied += nested_copied;
            skipped += nested_skipped;
        } else if !file_type.is_file() {
            return Err(color_eyre::eyre::eyre!(
                "session state source {} is not a regular file",
                tilde(&entry.path())
            ));
        } else if destination.exists() {
            let metadata = fs::symlink_metadata(&destination)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(color_eyre::eyre::eyre!(
                    "session state target {} is not a regular file",
                    tilde(&destination)
                ));
            }
            if files_equal(&entry.path(), &destination)? {
                skipped += 1;
            } else {
                return Err(color_eyre::eyre::eyre!(
                    "session rollback source {} conflicts with {}",
                    tilde(&entry.path()),
                    tilde(&destination)
                ));
            }
        } else {
            copy_file_atomically(&entry.path(), &destination).with_context(|| {
                format!("copy {} -> {}", tilde(&entry.path()), tilde(&destination))
            })?;
            copied += 1;
        }
    }
    Ok((copied, skipped))
}

fn move_logs(legacy: &Path, logs_dir: &Path) -> Result<()> {
    let entries: Vec<_> = fs::read_dir(legacy)
        .with_context(|| format!("read {}", tilde(legacy)))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("maki.") && name.ends_with(".log")
        })
        .collect();

    if entries.is_empty() {
        return Ok(());
    }

    create_directory_tree_durable(logs_dir)
        .with_context(|| format!("create {}", tilde(logs_dir)))?;

    for entry in &entries {
        let dst = logs_dir.join(entry.file_name());
        let name = entry.file_name();
        if dst.exists() {
            println!("  {} (skipped, already exists)", name.to_string_lossy());
        } else {
            move_file(&entry.path(), &dst)?;
            log_move(&name.to_string_lossy(), logs_dir, None);
        }
    }
    Ok(())
}

fn list_remaining(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            name != SESSIONS_DB_LOCK_FILE
                && name != paths::XDG_MIGRATED_MARKER
                && !RETAINED_SESSION_STATE
                    .iter()
                    .any(|retained| name == *retained)
                && !name.to_string_lossy().starts_with(SESSIONS_DB_FILE)
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

fn legacy_backup_path(legacy: &Path) -> PathBuf {
    legacy.with_extension("bak")
}

pub fn xdg() -> Result<()> {
    let Some(legacy) = paths::legacy_home_dir() else {
        println!("Nothing to migrate. You are already using XDG directories.");
        return Ok(());
    };

    let xdg = paths::xdg_paths().context("determine XDG directories")?;

    for dir in [&xdg.state, &xdg.config, &xdg.logs] {
        create_directory_tree_durable(dir).with_context(|| format!("create {}", tilde(dir)))?;
    }

    println!("Moving files from {}/ ...\n", tilde(&legacy));

    let legacy_state = StateDir::from_path(legacy.clone());
    let target_state = StateDir::from_path(xdg.state.clone());
    let session_migration = SessionDatabase::migrate(&legacy_state, &target_state)?;
    // Database and external-artifact locks share one cutover lifetime. Once
    // acquired, every copied file belongs to the same quiescent source state.
    let _legacy_artifacts = lock_session_artifacts(&legacy_state)?;
    let _target_artifacts = lock_session_artifacts(&target_state)?;
    if session_migration.copied_database() {
        log_move(
            SESSIONS_DB_FILE,
            &xdg.state,
            Some("consistent SQLite backup"),
        );
    }

    move_auth(&legacy.join("auth"), &xdg.state.join("auth"))?;

    // Database references, legacy rows, managed outputs, and restore snapshots
    // must cross the cutover together. Copies retain one coherent rollback set
    // under the legacy directory while the marker makes the XDG copy canonical.
    for subdir in RETAINED_SESSION_STATE {
        let (copied, skipped) = copy_session_state(&legacy, &xdg.state, Path::new(subdir))?;
        if copied > 0 || skipped > 0 {
            log_move(
                &format!("{subdir}/"),
                &xdg.state.join(subdir),
                Some(&format!(
                    "{copied} files copied, {skipped} skipped; rollback retained"
                )),
            );
        }
    }
    merge_dir(&legacy, &xdg.state, "plans", false)?;
    merge_dir(&legacy, &xdg.state, "projects", true)?;
    merge_dir(&legacy, &xdg.config, "providers", false)?;

    merge_json_file(
        &legacy.join("cwd_latest.json"),
        &xdg.state.join("cwd_latest.json"),
        "cwd_latest.json",
    )?;
    merge_input_history(
        &legacy.join("input_history.json"),
        &xdg.state.join("input_history.json"),
    )?;
    merge_json_file(
        &legacy.join("model-tiers"),
        &xdg.state.join("model-tiers"),
        "model-tiers",
    )?;
    merge_json_file(
        &legacy.join("model-roles"),
        &xdg.state.join("model-roles"),
        "model-roles",
    )?;

    for name in ["theme", "model"] {
        let src = legacy.join(name);
        if src.exists() {
            let dst = xdg.state.join(name);
            if dst.exists() {
                fs::remove_file(&dst)
                    .with_context(|| format!("remove existing {}", tilde(&dst)))?;
            }
            move_file(&src, &dst)?;
            log_move(name, dst.parent().unwrap_or(&dst), None);
        }
    }

    move_logs(&legacy, &xdg.logs)?;

    let lock_file = legacy.join("maki.log.lock");
    if lock_file.exists() {
        fs::remove_file(&lock_file).ok();
    }

    let remaining = list_remaining(&legacy);
    let has_leftovers = !remaining.is_empty();
    let backup = legacy_backup_path(&legacy);
    if has_leftovers {
        create_directory_tree_durable(&backup)
            .with_context(|| format!("create {}", tilde(&backup)))?;
        for name in &remaining {
            let src = legacy.join(name);
            let dst = backup.join(name);
            if dst.exists() {
                println!("  {name} (skipped, already in {}/)", tilde(&backup));
            } else if let Err(e) = move_file(&src, &dst) {
                eprintln!(
                    "  warning: could not move {name} to {}/: {e}",
                    tilde(&backup)
                );
            }
        }
    }

    session_migration.finish()?;
    let unresolved = list_remaining(&legacy);
    if !unresolved.is_empty() {
        eprintln!(
            "  warning: kept {}/ because these entries could not be moved: {}",
            tilde(&legacy),
            unresolved.join(", ")
        );
    }

    println!(
        "\nAll done! Your files now live here:\n\n\
         \x20 Config   {}\n\
         \x20          init.lua, permissions.toml, mcp.toml, providers/\n\n\
         \x20 State    {}\n\
         \x20          sessions, auth, plans, memories, input history, preferences\n\n\
         \x20 Logs     {}\n\n\
         Per-project settings (.maki/ in your repos) are not affected.\n\n\
         {} {}.",
        tilde(&xdg.config),
        tilde(&xdg.state),
        tilde(&xdg.logs),
        if unresolved.is_empty() {
            "Retired"
        } else {
            "Kept"
        },
        tilde(&legacy),
    );

    if has_leftovers {
        let backup_remaining = list_remaining(&backup);
        if backup_remaining.is_empty() {
            fs::remove_dir(&backup).ok();
        } else {
            println!(
                "\nSome unrecognized files were moved to {}:\n",
                tilde(&backup)
            );
            for name in &backup_remaining {
                println!("  {name}");
            }
            println!("\nFeel free to delete them if you do not need them.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(".maki", ".maki.bak"; "release")]
    #[test_case(".maki-debug", ".maki-debug.bak"; "debug")]
    fn legacy_backup_preserves_directory_name(legacy_name: &str, expected_name: &str) {
        let legacy = Path::new("/home/test").join(legacy_name);

        assert_eq!(
            legacy_backup_path(&legacy),
            Path::new("/home/test").join(expected_name)
        );
    }

    #[test]
    fn remaining_files_exclude_retained_session_rollback_state() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            SESSIONS_DB_FILE,
            "sessions.sqlite3-wal",
            SESSIONS_DB_LOCK_FILE,
            paths::XDG_MIGRATED_MARKER,
            "unrecognized",
        ] {
            fs::write(temp.path().join(name), b"data").unwrap();
        }
        fs::create_dir(temp.path().join("sessions")).unwrap();

        assert_eq!(list_remaining(temp.path()), ["unrecognized"]);
    }

    #[test]
    fn session_state_copy_is_atomic_and_rejects_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("legacy");
        let target = temp.path().join("target");
        fs::create_dir_all(legacy.join("sessions")).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(legacy.join("sessions/session.jsonl"), b"complete").unwrap();

        assert_eq!(
            copy_session_state(&legacy, &target, Path::new("sessions")).unwrap(),
            (1, 0)
        );
        assert_eq!(
            fs::read(target.join("sessions/session.jsonl")).unwrap(),
            b"complete"
        );
        fs::write(target.join("sessions/session.jsonl"), b"partial").unwrap();
        assert!(copy_session_state(&legacy, &target, Path::new("sessions")).is_err());
        assert!(legacy.join("sessions/session.jsonl").exists());
    }

    #[test]
    fn retained_session_state_includes_outputs_and_snapshots() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("legacy");
        let target = temp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        for subdir in RETAINED_SESSION_STATE {
            fs::create_dir_all(legacy.join(subdir)).unwrap();
            fs::write(legacy.join(subdir).join("state"), subdir).unwrap();
            copy_session_state(&legacy, &target, Path::new(subdir)).unwrap();
            assert_eq!(
                fs::read_to_string(target.join(subdir).join("state")).unwrap(),
                subdir
            );
            assert!(legacy.join(subdir).join("state").exists());
        }
        assert!(list_remaining(&legacy).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn session_state_copy_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("legacy");
        let target = temp.path().join("target");
        fs::create_dir_all(legacy.join("sessions")).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(temp.path().join("outside"), b"outside").unwrap();
        symlink(
            temp.path().join("outside"),
            legacy.join("sessions/session.jsonl"),
        )
        .unwrap();

        assert!(copy_session_state(&legacy, &target, Path::new("sessions")).is_err());
        assert!(!target.join("sessions/session.jsonl").exists());
    }

    #[cfg(unix)]
    #[test]
    fn session_state_copy_rejects_symlinked_target_directory() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("legacy");
        let target = temp.path().join("target");
        let outside = temp.path().join("outside");
        fs::create_dir_all(legacy.join("sessions")).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(legacy.join("sessions/session.jsonl"), b"session").unwrap();
        symlink(&outside, target.join("sessions")).unwrap();

        assert!(copy_session_state(&legacy, &target, Path::new("sessions")).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }
}
