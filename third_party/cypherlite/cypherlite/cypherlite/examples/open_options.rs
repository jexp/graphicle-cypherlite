//! Open a persistent URI with a custom checkpoint timer and memory cap.

use std::time::Duration;

use cypherlite::{Database, OpenOptions};

fn main() -> cypherlite::Result<()> {
    let dir = std::env::temp_dir().join(format!("cypherlite-open-options-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open_with(
        dir.to_str().expect("temp dir is valid UTF-8"),
        OpenOptions {
            wal_dir: None,
            checkpoint_interval: Some(Duration::from_secs(60)),
            durability_mode: Default::default(),
            logical_graph_size_limit: Some(1 << 30),
            snapshot_codec: None,
            load_csv_dir: None,
            load_csv_max_bytes: None,
            mode: cypherlite::OpenMode::Writable,

            token_admission: false,
            concurrent_logical_workspace_limit: None,
            follow_gap: Default::default(),
        },
    )?;
    let mut conn = db.connect()?;
    conn.query("CREATE (n:Person {name: 'Ada'})")?;
    db.checkpoint()?;
    let (used, limit) = db.memory_usage()?;
    let files = db.snapshot_data_bytes()?;
    let _ = (used, limit, files);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
