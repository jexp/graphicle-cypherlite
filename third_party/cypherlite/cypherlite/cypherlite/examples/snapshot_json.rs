//! Open an empty directory with the JSON snapshot codec (ADR-0050).
//! WAL is unchanged; checkpoints rewrite `graph.json`.

use cypherlite::{Database, OpenOptions, SnapshotCodecKind};

fn main() -> cypherlite::Result<()> {
    let dir = std::env::temp_dir().join(format!("cypherlite-snapshot-json-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open_with(
        dir.to_str().expect("temp dir is valid UTF-8"),
        OpenOptions {
            wal_dir: None,
            checkpoint_interval: None,
            durability_mode: Default::default(),
            logical_graph_size_limit: None,
            snapshot_codec: Some(SnapshotCodecKind::Json),
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
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
