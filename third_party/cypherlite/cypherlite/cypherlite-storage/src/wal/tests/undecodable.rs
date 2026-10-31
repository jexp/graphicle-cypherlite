//! ADR-0114 Decision 1: a CRC-valid frame whose payload does not decode as
//! `WalRecord` (here an `op` variant only a newer CypherLite knows) is a hard
//! error in every segment and position, never a tolerated tail.

use super::*;
use crate::tests::wal_dir_state;

/// Phrase the undecodable-frame hard error must carry so an operator
/// recognises a store written by a newer CypherLite.
const NEWER_CYPHERLITE_PHRASE: &str = "may have been written by a newer CypherLite";

fn unknown_op_payload(lsn: u64, commit_ts: u64) -> Vec<u8> {
    // A genuinely unknown op tag: `OntologySet` is a real op since
    // ADR-0120, so the "newer CypherLite" stand-in must be one this
    // binary still cannot decode.
    format!(
        r#"{{"lsn":{lsn},"commit_ts":{commit_ts},"ops":[{{"op":"MdbFutureOp","database":"neo4j","spec":"{{}}"}}]}}"#
    )
    .into_bytes()
}

fn assert_undecodable(err: &Error, path: &str, frame_offset: u64) {
    assert!(matches!(err, Error::Storage(_)), "{err}");
    let msg = err.to_string();
    assert!(msg.contains(NEWER_CYPHERLITE_PHRASE), "{msg}");
    assert!(msg.contains(path), "{msg}");
    assert!(msg.contains(&format!("offset {frame_offset}")), "{msg}");
}

/// A complete frame whose CRC is deliberately wrong: `write_header` writes a
/// zero CRC and the length, then the payload is appended.
fn write_bad_crc_frame(vfs: &dyn Vfs, path: &str, payload: &[u8]) {
    write_header(vfs, path, payload.len() as u32); // deliberately incorrect CRC
    vfs.open(path, VfsOpen::append_create())
        .unwrap()
        .write_all(payload)
        .unwrap();
}

#[test]
fn crc_valid_undecodable_active_tail_fails_open_without_touching_bytes() {
    fn check(vfs: Arc<dyn Vfs>) {
        let mut wal = Wal::open(Arc::clone(&vfs), "wal", 0).unwrap();
        wal.append_commit(1, &[]).unwrap();
        drop(wal);
        let path = "wal/00000001.log";
        let frame_offset = vfs.metadata(path).unwrap().len;
        write_frame(&*vfs, path, &unknown_op_payload(2, 2));
        let before = wal_dir_state(&*vfs, "wal");
        for _ in 0..2 {
            let err = Wal::open(Arc::clone(&vfs), "wal", 0).err().unwrap();
            assert_undecodable(&err, path, frame_offset);
        }
        assert_eq!(wal_dir_state(&*vfs, "wal"), before);
    }
    check(Arc::new(MemoryVfs::new()));
    let dir = tempfile::tempdir().unwrap();
    check(Arc::new(NativeVfs::new(dir.path())));
    let dir = tempfile::tempdir().unwrap();
    check(Arc::new(
        BundleVfs::open_or_create(dir.path().join("graph.cl")).unwrap(),
    ));
}

#[test]
fn crc_valid_undecodable_frame_fails_open_replay_and_cdc_in_closed_and_interior_positions() {
    // Final frame of a closed segment.
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    wal.rotate().unwrap();
    wal.append_commit(2, &[]).unwrap();
    let closed = "wal/00000001.log";
    let closed_offset = vfs.metadata(closed).unwrap().len;
    write_frame(&*vfs, closed, &unknown_op_payload(3, 3));
    assert_undecodable(&wal.replay(0).unwrap_err(), closed, closed_offset);
    assert_undecodable(
        &wal.records_after(0, 10).unwrap_err(),
        closed,
        closed_offset,
    );
    drop(wal);
    assert_undecodable(
        &Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0)
            .err()
            .unwrap(),
        closed,
        closed_offset,
    );

    // Interior frame of the active segment, followed by a valid frame.
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    let active = "wal/00000001.log";
    let interior_offset = vfs.metadata(active).unwrap().len;
    write_frame(&*vfs, active, &unknown_op_payload(2, 2));
    write_frame(&*vfs, active, br#"{"lsn":3,"commit_ts":3,"ops":[]}"#);
    assert_undecodable(&wal.replay(0).unwrap_err(), active, interior_offset);
    assert_undecodable(
        &wal.records_after(0, 10).unwrap_err(),
        active,
        interior_offset,
    );
    drop(wal);
    assert_undecodable(
        &Wal::open(vfs, "wal", 0).err().unwrap(),
        active,
        interior_offset,
    );
}

#[test]
fn records_after_on_open_handle_rejects_undecodable_tail_but_tolerates_bad_crc_tail() {
    // CRC-valid undecodable final frame: CDC read fails with the phrase.
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    let path = "wal/00000001.log";
    let frame_offset = vfs.metadata(path).unwrap().len;
    write_frame(&*vfs, path, &unknown_op_payload(2, 2));
    wal.lsn = 2; // Simulate the newly persisted second frame in this handle.
    assert_undecodable(&wal.records_after(0, 10).unwrap_err(), path, frame_offset);

    // A wrong-CRC final frame stays a tolerated relaxed append: the earlier
    // records come back without error.
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    let payload = br#"{"lsn":2,"commit_ts":2,"ops":[]}"#;
    write_bad_crc_frame(&*vfs, path, payload);
    wal.lsn = 2;
    let page = wal.records_after(0, 10).unwrap();
    assert_eq!(page.iter().map(|r| r.lsn).collect::<Vec<_>>(), vec![1]);
}

#[test]
fn undecodable_later_segment_fails_open_before_any_repair_write() {
    // A closed segment ending in an incomplete (torn) frame is only trimmed in
    // the repair phase; a hard failure in a later segment must leave its
    // bytes untouched.
    let vfs = Arc::new(MemoryVfs::new());
    vfs.create_dir_all("wal").unwrap();
    write_magic(&*vfs, "wal/00000001.log", MAGIC, true).unwrap();
    write_frame(
        &*vfs,
        "wal/00000001.log",
        br#"{"lsn":1,"commit_ts":1,"ops":[]}"#,
    );
    write_header(&*vfs, "wal/00000001.log", 12);
    vfs.open("wal/00000001.log", VfsOpen::append_create())
        .unwrap()
        .write_all(b"{")
        .unwrap();
    write_magic(&*vfs, "wal/00000002.log", MAGIC, true).unwrap();
    let frame_offset = vfs.metadata("wal/00000002.log").unwrap().len;
    write_frame(&*vfs, "wal/00000002.log", &unknown_op_payload(2, 2));
    let before = wal_dir_state(&*vfs, "wal");

    let err = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0)
        .err()
        .unwrap();
    assert_undecodable(&err, "wal/00000002.log", frame_offset);
    assert_eq!(
        wal_dir_state(&*vfs, "wal"),
        before,
        "the closed segment's torn tail must survive the failed open"
    );

    // Same deferral for the active segment's tolerated bad-CRC tail: a hard
    // failure in a closed segment must leave the active tail untrimmed.
    let vfs = Arc::new(MemoryVfs::new());
    vfs.create_dir_all("wal").unwrap();
    write_magic(&*vfs, "wal/00000001.log", MAGIC, true).unwrap();
    let frame_offset = vfs.metadata("wal/00000001.log").unwrap().len;
    write_frame(&*vfs, "wal/00000001.log", &unknown_op_payload(1, 1));
    write_magic(&*vfs, "wal/00000002.log", MAGIC, true).unwrap();
    let payload = br#"{"lsn":2,"commit_ts":2,"ops":[]}"#;
    write_bad_crc_frame(&*vfs, "wal/00000002.log", payload);
    let before = wal_dir_state(&*vfs, "wal");

    let err = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0)
        .err()
        .unwrap();
    assert_undecodable(&err, "wal/00000001.log", frame_offset);
    assert_eq!(
        wal_dir_state(&*vfs, "wal"),
        before,
        "the active segment's tolerated tail must survive the failed open"
    );
}
