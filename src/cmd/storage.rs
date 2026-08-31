use color_eyre::Result;
use color_eyre::eyre::Context;
use maki_storage::StateDir;
use maki_storage::sessions::{SESSIONS_DB_FILE, SessionDatabase};

use crate::cli::StorageAction;

pub fn run(action: StorageAction) -> Result<()> {
    let state_dir = StateDir::resolve().context("resolve state directory")?;
    match action {
        StorageAction::Path => {
            println!("{}", state_dir.path().join(SESSIONS_DB_FILE).display());
        }
        StorageAction::Stats { json } => {
            let database = SessionDatabase::open_read_only(&state_dir)
                .context("open session database read-only")?;
            let stats = database
                .stats()
                .context("read session storage statistics")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!("database_bytes: {}", stats.database_bytes);
                println!("wal_bytes: {}", stats.wal_bytes);
                println!("shm_bytes: {}", stats.shm_bytes);
                println!("page_size: {}", stats.page_size);
                println!("page_count: {}", stats.page_count);
                println!("freelist_count: {}", stats.freelist_count);
                println!("auto_vacuum: {}", stats.auto_vacuum);
                println!("schema_version: {}", stats.schema_version);
                println!("sessions: {}", stats.session_count);
                println!("history_items: {}", stats.history_item_count);
                println!("tool_outputs: {}", stats.tool_output_count);
                println!("subagent_items: {}", stats.subagent_item_count);
                println!("logical_bytes: {}", stats.logical_bytes);
            }
        }
        StorageAction::Check => {
            let database = SessionDatabase::open_read_only(&state_dir)
                .context("open session database read-only")?;
            database.quick_check().context("check session database")?;
            println!("ok");
        }
        StorageAction::Checkpoint { truncate } => {
            let database = SessionDatabase::open(&state_dir).context("open session database")?;
            let result = database
                .checkpoint(truncate)
                .context("checkpoint session database")?;
            println!("busy: {}", result.busy);
            println!("log_frames: {}", result.log_frames);
            println!("checkpointed_frames: {}", result.checkpointed_frames);
        }
        StorageAction::Vacuum { pages } => {
            let database = SessionDatabase::open(&state_dir).context("open session database")?;
            database
                .incremental_vacuum(pages)
                .context("vacuum session database")?;
        }
    }
    Ok(())
}
