//! Open a persistent URI with a custom checkpoint timer and memory cap.

use std::time::Duration;

use cypherlite::{Database, OpenOptions};

fn main() -> cypherlite::Result<()> {
    let dir = std::env::temp_dir().join(format!("cypherlite-open-options-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open_with(
        dir.to_str().expect("temp dir is valid UTF-8"),
        OpenOptions {
            checkpoint_interval: Some(Duration::from_secs(60)),
            logical_graph_size_limit: Some(1 << 30),
            ..OpenOptions::default()
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
