//! ADR-0120 §4: the `MDBWAL02` segment magic. A segment created while the
//! store holds an ontology starts with `MDBWAL02`; an ontology op that
//! commits while the active segment is `MDBWAL01` rotates to a fresh
//! `MDBWAL02` segment first; an ontology op decoded from an `MDBWAL01`
//! segment is corruption.

use super::*;

fn ontology_set() -> RedoOp {
    RedoOp::OntologySet {
        database: "neo4j".into(),
        spec: "{}".into(),
    }
}

fn ontology_drop() -> RedoOp {
    RedoOp::OntologyDrop {
        database: "neo4j".into(),
    }
}

/// First eight bytes of a segment file: its identity magic.
fn segment_magic(vfs: &dyn Vfs, path: &str) -> [u8; 8] {
    crate::vfs::read_to_vec(vfs, path).unwrap()[..8]
        .try_into()
        .unwrap()
}

#[test]
fn segment_magic_follows_the_ontology_held_flag() {
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    assert!(!wal.ontology_held());
    wal.append_commit(1, &[]).unwrap();
    wal.rotate().unwrap();
    // No ontology: the fresh segment starts with MDBWAL01.
    assert_eq!(segment_magic(&*vfs, "wal/00000002.log"), *MAGIC);

    wal.set_ontology_held(true);
    assert!(wal.ontology_held());
    wal.append_commit(2, &[]).unwrap();
    wal.rotate().unwrap();
    // Ontology held: the fresh segment starts with MDBWAL02.
    assert_eq!(segment_magic(&*vfs, "wal/00000003.log"), *ONTOLOGY_MAGIC);

    // Back to MDBWAL01 once the store layer clears the flag (ADR-0120 §8:
    // after the drop and a published checkpoint).
    wal.set_ontology_held(false);
    wal.append_commit(3, &[]).unwrap();
    wal.rotate().unwrap();
    assert_eq!(segment_magic(&*vfs, "wal/00000004.log"), *MAGIC);

    // Every segment replays: both magics are readable.
    assert_eq!(wal.replay(0).unwrap().len(), 3);
}

#[test]
fn ontology_op_commit_on_mdbwal01_segment_rotates_to_mdbwal02_first() {
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    assert_eq!(segment_magic(&*vfs, "wal/00000001.log"), *MAGIC);

    // The ontology op must not land in the MDBWAL01 segment: the WAL
    // rotates to a fresh MDBWAL02 segment first.
    wal.append_commit(2, &[ontology_set()]).unwrap();
    assert_eq!(segment_magic(&*vfs, "wal/00000002.log"), *ONTOLOGY_MAGIC);
    assert!(
        wal.ontology_held(),
        "the WAL now holds a pending ontology op"
    );

    // The old segment kept its magic and its record; the new one holds the
    // ontology op, and both replay.
    assert_eq!(segment_magic(&*vfs, "wal/00000001.log"), *MAGIC);
    let records = wal.replay(0).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].ops, vec![ontology_set()]);

    // A later rotation while the ontology is held stays MDBWAL02, and a
    // second ontology op needs no further rotation.
    wal.append_commit(3, &[ontology_drop()]).unwrap();
    assert_eq!(segment_magic(&*vfs, "wal/00000002.log"), *ONTOLOGY_MAGIC);
    assert!(vfs.metadata("wal/00000003.log").is_err());

    // Reopen: the active segment is MDBWAL02, so the held flag defaults on.
    drop(wal);
    let wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    assert!(wal.ontology_held());
    assert_eq!(wal.replay(0).unwrap().len(), 3);
}

#[test]
fn ontology_op_commit_on_an_empty_mdbwal01_segment_still_rotates() {
    // `Wal::rotate` no-ops on a commit-less segment; the ADR-0120 §4
    // rotation is unconditional.
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    assert_eq!(wal.commits, 0);
    wal.append_commit(1, &[ontology_set()]).unwrap();
    assert_eq!(segment_magic(&*vfs, "wal/00000002.log"), *ONTOLOGY_MAGIC);
    // The empty MDBWAL01 segment stays byte-identical: magic only.
    assert_eq!(
        crate::vfs::read_to_vec(&*vfs, "wal/00000001.log").unwrap(),
        MAGIC
    );
    assert_eq!(wal.replay(0).unwrap()[0].ops, vec![ontology_set()]);
}

#[test]
fn ontology_op_in_mdbwal01_segment_is_corruption_at_open_replay_and_cdc() {
    let payload = |lsn: u64, ts: u64| {
        serde_json::to_vec(&WalRecord {
            lsn,
            commit_ts: ts,
            ops: vec![ontology_set()],
        })
        .unwrap()
    };
    let assert_corruption = |err: Error, path: &str| {
        assert!(matches!(err, Error::Storage(_)), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains("ontology op in non-MDBWAL02 WAL segment"),
            "{msg}"
        );
        assert!(msg.contains(path), "{msg}");
    };

    // Active segment.
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    write_frame(&*vfs, "wal/00000001.log", &payload(2, 2));
    assert_corruption(wal.replay(0).unwrap_err(), "wal/00000001.log");
    assert_corruption(wal.records_after(0, 10).unwrap_err(), "wal/00000001.log");
    drop(wal);
    assert_corruption(
        Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0)
            .err()
            .unwrap(),
        "wal/00000001.log",
    );

    // Closed segment.
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    wal.rotate().unwrap();
    wal.append_commit(2, &[]).unwrap();
    write_frame(&*vfs, "wal/00000001.log", &payload(2, 2));
    drop(wal);
    assert_corruption(
        Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0)
            .err()
            .unwrap(),
        "wal/00000001.log",
    );

    // The same frame in an MDBWAL02 segment is legitimate and replays.
    let vfs = Arc::new(MemoryVfs::new());
    vfs.create_dir_all("wal").unwrap();
    write_magic(&*vfs, "wal/00000001.log", ONTOLOGY_MAGIC, true).unwrap();
    write_frame(&*vfs, "wal/00000001.log", &payload(1, 1));
    let wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    assert!(wal.ontology_held());
    assert_eq!(wal.replay(0).unwrap()[0].ops, vec![ontology_set()]);
}

/// ADR-0120 §3: the 16 MiB document cap is checked at the commit-preflight
/// seam, before anything is written.
#[test]
fn oversized_ontology_document_fails_preflight_before_writing() {
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    let before = crate::vfs::read_to_vec(&*vfs, "wal/00000001.log").unwrap();
    let (lsn, bytes, commits) = (wal.lsn, wal.bytes, wal.commits);

    let too_big = RedoOp::OntologySet {
        database: "neo4j".into(),
        spec: "x".repeat(cypherlite_executor::MAX_ONTOLOGY_SPEC_BYTES + 1),
    };
    let err = wal
        .preflight_record(2, 2, &[too_big.clone()])
        .err()
        .unwrap();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert!(err.to_string().contains("16 MiB"), "{err}");
    // The direct append path shares the preflight seam.
    assert!(matches!(
        wal.append_record(2, &[too_big]),
        Err(Error::Storage(_))
    ));
    // Nothing was written and no rotation happened.
    assert_eq!((wal.lsn, wal.bytes, wal.commits), (lsn, bytes, commits));
    assert_eq!(
        crate::vfs::read_to_vec(&*vfs, "wal/00000001.log").unwrap(),
        before
    );
    assert!(vfs.metadata("wal/00000002.log").is_err());
    assert!(!wal.ontology_held());

    // Exactly at the cap passes (the WAL payload limit is larger).
    let at_cap = RedoOp::OntologySet {
        database: "neo4j".into(),
        spec: "x".repeat(cypherlite_executor::MAX_ONTOLOGY_SPEC_BYTES),
    };
    assert_eq!(wal.append_commit(2, &[at_cap]).unwrap(), 2);
    assert_eq!(segment_magic(&*vfs, "wal/00000002.log"), *ONTOLOGY_MAGIC);
}

/// ADR-0086 §1 unchanged: a repaired short header is still `MDBWAL01`,
/// even when the WAL is ontology-holding — the repaired segment holds no
/// frames and ontology ops are never appended to it (ADR-0120 §4).
#[test]
fn short_header_repair_still_writes_mdbwal01() {
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    wal.set_ontology_held(true);
    wal.append_commit(1, &[ontology_set()]).unwrap();
    drop(wal);
    // Crash mid-rotate: a proper non-empty prefix of the magic.
    vfs.open("wal/00000003.log", VfsOpen::create_truncate())
        .unwrap()
        .write_all(&MAGIC[..3])
        .unwrap();
    let wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    assert_eq!(segment_magic(&*vfs, "wal/00000003.log"), *MAGIC);
    assert_eq!(wal.replay(0).unwrap().len(), 1);
}
