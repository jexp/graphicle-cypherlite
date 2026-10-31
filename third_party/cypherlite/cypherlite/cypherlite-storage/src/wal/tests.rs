use super::*;
use crate::vfs::{BundleVfs, NativeVfs, VfsMeta};
use std::io::SeekFrom;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[path = "tests/ontology_magic.rs"]
mod ontology_magic;
#[path = "tests/undecodable.rs"]
mod undecodable;

fn write_frame(vfs: &dyn Vfs, path: &str, payload: &[u8]) {
    let mut f = vfs.open(path, VfsOpen::append_create()).unwrap();
    f.write_all(&crc32fast::hash(payload).to_le_bytes())
        .unwrap();
    f.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
    f.write_all(payload).unwrap();
}

fn write_header(vfs: &dyn Vfs, path: &str, len: u32) {
    let mut f = vfs.open(path, VfsOpen::append_create()).unwrap();
    f.write_all(&0u32.to_le_bytes()).unwrap();
    f.write_all(&len.to_le_bytes()).unwrap();
}

#[test]
fn wal_rejects_bogus_lengths_before_allocation_in_open_replay_and_cdc() {
    for len in [0, MAX_PAYLOAD_BYTES as u32 + 1, u32::MAX] {
        let vfs = Arc::new(MemoryVfs::new());
        write_magic(&*vfs, "00000001.log", MAGIC, true).unwrap();
        write_header(&*vfs, "00000001.log", len);
        assert!(Wal::open(vfs, "", 0).is_err(), "length {len}");
    }
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "", 0).unwrap();
    append_n(&mut wal, 1);
    write_header(&*vfs, "00000001.log", u32::MAX);
    assert!(wal.replay(0).is_err());
    assert!(wal.records_after(0, 10).is_err());
}

#[test]
fn crc_valid_max_commit_timestamp_is_a_storage_error_and_previous_value_replays() {
    for ts in [u64::MAX - 1, u64::MAX] {
        let vfs = Arc::new(MemoryVfs::new());
        write_magic(&*vfs, "00000001.log", MAGIC, true).unwrap();
        let bytes = serde_json::to_vec(&WalRecord {
            lsn: 1,
            commit_ts: ts,
            ops: vec![],
        })
        .unwrap();
        write_frame(&*vfs, "00000001.log", &bytes);
        if ts == u64::MAX {
            let err = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "", 0)
                .err()
                .unwrap();
            assert!(matches!(err, Error::Storage(_)), "{err}");
            assert!(err.to_string().contains("timestamp"), "{err}");
        } else {
            let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "", 0).unwrap();
            assert_eq!(wal.replay(0).unwrap()[0].commit_ts, ts);
            let before = vfs.metadata("00000001.log").unwrap().len;
            assert!(matches!(
                wal.append_commit(u64::MAX, &[]),
                Err(Error::Storage(_))
            ));
            assert_eq!(vfs.metadata("00000001.log").unwrap().len, before);
            assert_eq!(wal.records_after(0, 1).unwrap()[0].commit_ts, ts);
        }
    }
}

#[test]
fn append_rejects_zero_repeated_and_backward_timestamps_before_writing() {
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "", 0).unwrap();
    let rejects_without_writing = |wal: &mut Wal, ts| {
        let before = crate::vfs::read_to_vec(&*vfs, "00000001.log").unwrap();
        let (lsn, bytes, commits, last_ts) = (wal.lsn, wal.bytes, wal.commits, wal.last_commit_ts);
        assert!(matches!(wal.append_record(ts, &[]), Err(Error::Storage(_))));
        assert_eq!(
            (wal.lsn, wal.bytes, wal.commits, wal.last_commit_ts),
            (lsn, bytes, commits, last_ts)
        );
        assert_eq!(
            crate::vfs::read_to_vec(&*vfs, "00000001.log").unwrap(),
            before
        );
    };
    rejects_without_writing(&mut wal, 0);
    wal.append_commit(2, &[]).unwrap();
    for ts in [1, 2, 0] {
        rejects_without_writing(&mut wal, ts);
    }
    assert_eq!(wal.append_commit(3, &[]).unwrap(), 2);
    assert_eq!(wal.replay(0).unwrap().len(), 2);
}

#[test]
fn scan_rejects_zero_or_nonforward_commits_even_below_checkpoint_and_across_segments() {
    for (bad_ts, separate_segment) in [(0, false), (2, false), (1, true), (2, true)] {
        let vfs = Arc::new(MemoryVfs::new());
        let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "", 0).unwrap();
        wal.append_commit(2, &[]).unwrap();
        if separate_segment {
            wal.rotate().unwrap();
        }
        let path = if separate_segment {
            "00000002.log"
        } else {
            "00000001.log"
        };
        let bytes = serde_json::to_vec(&WalRecord {
            lsn: 2,
            commit_ts: bad_ts,
            ops: vec![],
        })
        .unwrap();
        write_frame(&*vfs, path, &bytes);

        for err in [
            wal.replay(1).unwrap_err(),
            wal.records_after(0, 2).unwrap_err(),
            Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "", 1)
                .err()
                .unwrap(),
        ] {
            assert!(matches!(err, Error::Storage(_)), "{bad_ts}: {err}");
            assert!(err.to_string().contains("timestamp"), "{err}");
        }
    }
}

#[test]
fn cdc_hint_rejects_nonforward_timestamp_in_later_frame() {
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "", 0).unwrap();
    wal.append_commit(2, &[]).unwrap();
    assert_eq!(wal.records_after(0, 1).unwrap()[0].commit_ts, 2);
    let bytes = serde_json::to_vec(&WalRecord {
        lsn: 2,
        commit_ts: 2,
        ops: vec![],
    })
    .unwrap();
    write_frame(&*vfs, "00000001.log", &bytes);
    wal.lsn = 2; // Simulate the newly persisted second frame in this open handle.
    let err = wal.records_after(1, 1).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert!(err.to_string().contains("timestamp"), "{err}");
}

#[test]
fn every_invalid_redo_id_is_rejected_on_read_and_direct_append() {
    use cypherlite_core::Value;
    use cypherlite_executor::PullKind;

    let node = |id| RedoOp::CreateNode {
        id,
        labels: Arc::new(vec![]),
        properties: std::sync::Arc::new(cypherlite_core::PropertyMap::new()),
    };
    let rel = |id, start, end| RedoOp::CreateRel {
        id,
        start,
        end,
        rel_type: "R".into(),
        properties: std::sync::Arc::new(cypherlite_core::PropertyMap::new()),
    };
    let invalid_ops = |bad| {
        vec![
            node(bad),
            rel(bad, 0, 0),
            rel(0, bad, 0),
            rel(0, 0, bad),
            RedoOp::DeleteNode { id: bad },
            RedoOp::DeleteRel { id: bad },
            RedoOp::SetNodeProp {
                id: bad,
                key: "k".into(),
                value: Value::Int(-1),
            },
            RedoOp::RemoveNodeProp {
                id: bad,
                key: "k".into(),
            },
            RedoOp::SetRelProp {
                id: bad,
                key: "k".into(),
                value: Value::Int(-1),
            },
            RedoOp::RemoveRelProp {
                id: bad,
                key: "k".into(),
            },
            RedoOp::AddLabels {
                id: bad,
                labels: vec!["L".into()],
            },
            RedoOp::RemoveLabels {
                id: bad,
                labels: vec!["L".into()],
            },
            RedoOp::PullIdentityPut {
                source: "source".into(),
                kind: PullKind::Node,
                remote_id: "-1".into(),
                local_id: bad,
            },
            RedoOp::PullIdentityPut {
                source: "source".into(),
                kind: PullKind::Rel,
                remote_id: i64::MAX.to_string(),
                local_id: bad,
            },
        ]
    };
    for (case, op) in [-1, i64::MAX].into_iter().flat_map(invalid_ops).enumerate() {
        let vfs = Arc::new(MemoryVfs::new());
        let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "", 0).unwrap();
        wal.append_commit(1, &[node(0)]).unwrap();
        let before = crate::vfs::read_to_vec(&*vfs, "00000001.log").unwrap();
        let (lsn, bytes, commits) = (wal.lsn, wal.bytes, wal.commits);
        // The valid first operation must not be installed if a later one fails.
        let err = wal.append_record(2, &[node(1), op.clone()]).unwrap_err();
        assert!(matches!(err, Error::Storage(_)), "case {case}: {err}");
        assert_eq!(
            (wal.lsn, wal.bytes, wal.commits),
            (lsn, bytes, commits),
            "case {case}"
        );
        assert_eq!(
            crate::vfs::read_to_vec(&*vfs, "00000001.log").unwrap(),
            before
        );
        drop(wal);
        let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "", 0).unwrap();
        assert_eq!(wal.replay(0).unwrap().len(), 1);
        let bytes = serde_json::to_vec(&WalRecord {
            lsn: 2,
            commit_ts: 2,
            ops: vec![node(1), op],
        })
        .unwrap();
        write_frame(&*vfs, "00000001.log", &bytes);
        for err in [
            wal.replay(0).unwrap_err(),
            wal.records_after(0, 2).unwrap_err(),
            Wal::open(vfs as Arc<dyn Vfs>, "", 0).err().unwrap(),
        ] {
            assert!(matches!(err, Error::Storage(_)), "case {case}: {err}");
        }
    }
}

#[test]
fn pull_identity_remote_id_and_property_values_are_not_local_ids() {
    use cypherlite_core::Value;
    use cypherlite_executor::PullKind;

    let mut wal = Wal::memory();
    let ops = [
        RedoOp::PullIdentityPut {
            source: "-1".into(),
            kind: PullKind::Rel,
            remote_id: i64::MAX.to_string(),
            local_id: i64::MAX - 1,
        },
        RedoOp::SetNodeProp {
            id: 0,
            key: "-1".into(),
            value: Value::Int(i64::MAX),
        },
    ];
    wal.append_commit(1, &ops).unwrap();
    assert_eq!(wal.replay(0).unwrap()[0].ops, ops);
}

#[test]
fn wal_torn_closed_tail_is_trimmed_but_existing_magic_must_be_complete() {
    // A relaxed (snapshot) rotate skips the barrier, so a torn suffix can land
    // on a just-rotated *closed* segment. Recovery keeps the intact prefix and
    // trims the torn 2-byte suffix instead of failing closed.
    let vfs = Arc::new(MemoryVfs::new());
    write_magic(&*vfs, "00000001.log", MAGIC, true).unwrap();
    vfs.open("00000001.log", VfsOpen::append_create())
        .unwrap()
        .write_all(&[1, 2])
        .unwrap();
    write_magic(&*vfs, "00000002.log", MAGIC, true).unwrap();

    let wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "", 0).unwrap();
    assert!(wal.replay(0).unwrap().is_empty());
    assert_eq!(
        vfs.metadata("00000001.log").unwrap().len,
        MAGIC.len() as u64,
        "closed torn suffix must be trimmed to the committed prefix"
    );

    // A previously existing segment whose magic is empty or an incomplete
    // non-frame prefix is still corruption: only a *complete* magic is normal.
    for prefix in [b"".as_slice(), b"MDBWAL".as_slice()] {
        let vfs = Arc::new(MemoryVfs::new());
        vfs.open("00000001.log", VfsOpen::create_truncate())
            .unwrap()
            .write_all(prefix)
            .unwrap();
        assert!(Wal::open(vfs, "", 0).is_err());
    }
}

#[test]
fn wal_in_range_truncated_closed_payload_trims_suffix_and_preserves_prefix() {
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    append_n(&mut wal, 2);
    wal.rotate().unwrap();
    let path = "wal/00000001.log";
    let committed = crate::vfs::read_to_vec(&*vfs, path).unwrap();

    // A complete in-range header whose declared payload runs past physical EOF
    // in a *closed* segment is a torn suffix (relaxed rotate), not bitrot.
    write_header(&*vfs, path, 64); // Within the cap, beyond the closed file's end.
    vfs.open(path, VfsOpen::append_create())
        .unwrap()
        .write_all(b"partial")
        .unwrap();
    let corrupted = crate::vfs::read_to_vec(&*vfs, path).unwrap();
    assert!(corrupted.len() > committed.len());

    // The existing handle neither errors nor treats the torn suffix as a
    // commit: replay and CDC both return the intact committed prefix.
    assert_eq!(
        wal.replay(0)
            .unwrap()
            .iter()
            .map(|r| r.lsn)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(
        wal.records_after(0, 2)
            .unwrap()
            .iter()
            .map(|r| r.lsn)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    drop(wal);

    // Reopen succeeds and atomically trims the damaged suffix back to the
    // committed prefix; the intact records still replay.
    let recovered = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    assert_eq!(
        recovered
            .replay(0)
            .unwrap()
            .iter()
            .map(|r| r.lsn)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(crate::vfs::read_to_vec(&*vfs, path).unwrap(), committed);
}

#[test]
fn wal_repairs_incomplete_active_tail_before_append_on_native_and_memory() {
    fn check(vfs: Arc<dyn Vfs>) {
        let mut wal = Wal::open(Arc::clone(&vfs), "wal", 0).unwrap();
        append_n(&mut wal, 2);
        drop(wal);
        let path = "wal/00000001.log";
        write_header(&*vfs, path, 12);
        vfs.open(path, VfsOpen::append_create())
            .unwrap()
            .write_all(b"{")
            .unwrap();
        let mut reopened = Wal::open(Arc::clone(&vfs), "wal", 0).unwrap();
        assert_eq!(reopened.replay(0).unwrap().len(), 2);
        assert_eq!(reopened.append_commit(3, &[]).unwrap(), 3);
        assert_eq!(
            reopened
                .replay(0)
                .unwrap()
                .iter()
                .map(|r| r.lsn)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(reopened.records_after(0, 10).unwrap().len(), 3);
        drop(reopened);
        assert_eq!(
            Wal::open(vfs, "wal", 0).unwrap().replay(0).unwrap().len(),
            3
        );
    }
    check(Arc::new(MemoryVfs::new()));
    let dir = tempfile::tempdir().unwrap();
    check(Arc::new(NativeVfs::new(dir.path())));
}

/// ADR-0098: the shared atomic writer must never follow a pre-existing
/// temp-path symlink.
#[cfg(unix)]
#[test]
fn wal_repair_does_not_overwrite_temp_symlink_target() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let vfs = Arc::new(NativeVfs::new(dir.path()));
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    append_n(&mut wal, 2);
    drop(wal);
    let path = "wal/00000001.log";
    write_header(&*vfs, path, 12);
    vfs.open(path, VfsOpen::append_create())
        .unwrap()
        .write_all(b"{")
        .unwrap();
    let sentinel = dir.path().join("sentinel");
    std::fs::write(&sentinel, b"do not truncate me").unwrap();
    let tmp = dir.path().join("wal/00000001.log.tmp");
    symlink(&sentinel, &tmp).unwrap();

    let mut repaired = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"do not truncate me");
    assert_eq!(std::fs::read_link(&tmp).unwrap(), sentinel);
    assert_eq!(repaired.replay(0).unwrap().len(), 2);
    assert_eq!(repaired.append_commit(3, &[]).unwrap(), 3);
    assert_eq!(repaired.replay(0).unwrap().len(), 3);
}

#[test]
fn wal_short_active_header_replays_prefix_and_is_removed() {
    let vfs = Arc::new(MemoryVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "", 0).unwrap();
    append_n(&mut wal, 1);
    drop(wal);
    vfs.open("00000001.log", VfsOpen::append_create())
        .unwrap()
        .write_all(&[1, 2, 3])
        .unwrap();
    let mut wal = Wal::open(vfs, "", 0).unwrap();
    assert_eq!(wal.replay(0).unwrap().len(), 1);
    wal.append_commit(2, &[]).unwrap();
    assert_eq!(wal.replay(0).unwrap().len(), 2);
}

#[test]
fn wal_oversize_append_leaves_lsn_and_file_unchanged() {
    use cypherlite_core::Value;
    let (dir, mut wal) = file_wal();
    let before = std::fs::metadata(dir.path().join("00000001.log"))
        .unwrap()
        .len();
    let ops = [RedoOp::SetNodeProp {
        id: 1,
        key: "large".to_string(),
        value: Value::String("x".repeat(MAX_PAYLOAD_BYTES).into()),
    }];
    assert!(wal.append_record(1, &ops).is_err());
    assert_eq!(wal.lsn, 0);
    assert_eq!(wal.commits, 0);
    assert_eq!(wal.bytes, MAGIC.len() as u64);
    assert_eq!(
        std::fs::metadata(dir.path().join("00000001.log"))
            .unwrap()
            .len(),
        before
    );
    assert_eq!(wal.append_commit(2, &[]).unwrap(), 1);
}

#[test]
fn preflighted_append_rejects_stale_proof_before_first_frame() {
    let mut wal = Wal::memory();
    let stale = wal.preflight_record(1, 1, &[]).unwrap();
    wal.append_commit(1, &[]).unwrap();
    assert!(wal.append_preflighted(stale).is_err());
    assert_eq!(wal.lsn, 1);
    assert_eq!(wal.replay(0).unwrap().len(), 1);
    wal.append_commit(2, &[]).unwrap(); // deterministic errors did not poison
}

struct AppendFaultVfs {
    inner: MemoryVfs,
    fail_write: Arc<AtomicBool>,
    fail_barrier: Arc<AtomicBool>,
    fail_magic: Arc<AtomicBool>,
}
impl AppendFaultVfs {
    fn new() -> Self {
        Self {
            inner: MemoryVfs::new(),
            fail_write: Arc::new(AtomicBool::new(false)),
            fail_barrier: Arc::new(AtomicBool::new(false)),
            fail_magic: Arc::new(AtomicBool::new(false)),
        }
    }
}
struct AppendFaultFile {
    inner: Box<dyn VfsFile>,
    fail_write: Arc<AtomicBool>,
    fail_barrier: Arc<AtomicBool>,
}
impl VfsFile for AppendFaultFile {
    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        if self.fail_write.swap(false, Ordering::SeqCst) {
            self.inner.write_all(&bytes[..bytes.len().min(2)])?;
            return Err(Error::io("injected partial frame write"));
        }
        self.inner.write_all(bytes)
    }
    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<()> {
        self.inner.read_exact(bytes)
    }
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        self.inner.seek(pos)
    }
    fn sync_all(&mut self) -> Result<()> {
        self.inner.sync_all()
    }
    fn durability_barrier(&mut self) -> Result<()> {
        self.inner.sync_all()?;
        if self.fail_barrier.swap(false, Ordering::SeqCst) {
            return Err(Error::io("injected ambiguous barrier"));
        }
        Ok(())
    }
    fn len(&self) -> Result<u64> {
        self.inner.len()
    }
    fn set_len(&mut self, len: u64) -> Result<()> {
        self.inner.set_len(len)
    }
}
impl Vfs for AppendFaultVfs {
    fn durable(&self) -> bool {
        true
    }
    fn create_dir_all(&self, path: &str) -> Result<()> {
        self.inner.create_dir_all(path)
    }
    fn create_dir_all_for_atomic(&self, path: &str) -> Result<()> {
        self.inner.create_dir_all(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        self.inner.list_dir(path)
    }
    fn exists(&self, path: &str) -> Result<bool> {
        self.inner.exists(path)
    }
    fn metadata(&self, path: &str) -> Result<VfsMeta> {
        self.inner.metadata(path)
    }
    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>> {
        let file = self.inner.open(path, opts)?;
        if opts.append || (opts.truncate && path.ends_with("00000002.log")) {
            Ok(Box::new(AppendFaultFile {
                inner: file,
                fail_write: Arc::clone(if opts.append {
                    &self.fail_write
                } else {
                    &self.fail_magic
                }),
                fail_barrier: Arc::clone(&self.fail_barrier),
            }))
        } else {
            Ok(file)
        }
    }
    fn remove_file(&self, path: &str) -> Result<()> {
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &str) -> Result<()> {
        self.inner.remove_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.inner.rename(from, to)
    }
    fn sync_dir(&self, _path: &str) -> Result<()> {
        Ok(())
    }
}

#[test]
fn partial_append_and_ambiguous_barrier_recover_only_physical_prefix() {
    let assert_uncertain_outcome = |wal: &mut Wal, commit_ts: u64| {
        for error in [
            wal.append_record(commit_ts, &[]).unwrap_err(),
            wal.sync().unwrap_err(),
            wal.rotate().unwrap_err(),
            wal.recycle(1).unwrap_err(),
        ] {
            assert!(
                error.to_string().contains("uncertain WAL outcome"),
                "{error}"
            );
        }
    };
    let vfs = Arc::new(AppendFaultVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    let before = crate::vfs::read_to_vec(&vfs.inner, "wal/00000001.log").unwrap();
    vfs.fail_write.store(true, Ordering::SeqCst);
    assert!(wal
        .append_record(2, &[])
        .unwrap_err()
        .to_string()
        .contains("partial frame"));
    assert_eq!(wal.lsn, 1);
    let torn = crate::vfs::read_to_vec(&vfs.inner, "wal/00000001.log").unwrap();
    assert_uncertain_outcome(&mut wal, 2);
    assert_eq!(
        crate::vfs::read_to_vec(&vfs.inner, "wal/00000001.log").unwrap(),
        torn
    );
    assert_eq!(
        crate::vfs::read_to_vec(&vfs.inner, "wal/00000001.log")
            .unwrap()
            .len(),
        before.len() + 2
    );
    drop(wal);
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    assert_eq!(wal.replay(0).unwrap().len(), 1);
    assert_eq!(
        crate::vfs::read_to_vec(&vfs.inner, "wal/00000001.log").unwrap(),
        before
    );
    wal.append_record(2, &[]).unwrap();
    vfs.fail_barrier.store(true, Ordering::SeqCst);
    assert!(wal
        .sync()
        .unwrap_err()
        .to_string()
        .contains("ambiguous barrier"));
    assert_uncertain_outcome(&mut wal, 3);
    drop(wal);
    let wal = Wal::open(vfs, "wal", 0).unwrap();
    assert_eq!(wal.replay(0).unwrap().len(), 2);
}

#[test]
fn failed_rotate_short_magic_recovers_only_verified_highest_segment() {
    let vfs = Arc::new(AppendFaultVfs::new());
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    let previous = crate::vfs::read_to_vec(&vfs.inner, "wal/00000001.log").unwrap();
    vfs.fail_magic.store(true, Ordering::SeqCst);
    assert!(wal
        .rotate()
        .unwrap_err()
        .to_string()
        .contains("partial frame"));
    assert_eq!(wal.inner.seg_id, 1);
    assert!(wal.inner.closed.is_empty());
    assert_eq!(
        crate::vfs::read_to_vec(&vfs.inner, "wal/00000002.log").unwrap(),
        &MAGIC[..2]
    );
    assert!(wal
        .append_commit(2, &[])
        .unwrap_err()
        .to_string()
        .contains("uncertain WAL outcome"));
    drop(wal);
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    assert_eq!(
        crate::vfs::read_to_vec(&vfs.inner, "wal/00000001.log").unwrap(),
        previous
    );
    assert_eq!(wal.replay(0).unwrap().len(), 1);
    assert_eq!(wal.append_commit(2, &[]).unwrap(), 2);
    drop(wal);
    assert_eq!(
        Wal::open(vfs, "wal", 0).unwrap().replay(0).unwrap().len(),
        2
    );
}

#[test]
fn every_short_highest_magic_prefix_recovers_on_memory_and_native() {
    for native in [false, true] {
        for size in 1..MAGIC.len() {
            let dir = tempfile::tempdir().unwrap();
            let vfs: Arc<dyn Vfs> = if native {
                Arc::new(NativeVfs::new(dir.path()))
            } else {
                Arc::new(MemoryVfs::new())
            };
            let mut wal = Wal::open(Arc::clone(&vfs), "wal", 0).unwrap();
            wal.append_commit(1, &[]).unwrap();
            let first = crate::vfs::read_to_vec(&*vfs, "wal/00000001.log").unwrap();
            drop(wal);
            vfs.open("wal/00000002.log", VfsOpen::create_truncate())
                .unwrap()
                .write_all(&MAGIC[..size])
                .unwrap();
            let mut reopened = Wal::open(Arc::clone(&vfs), "wal", 0).unwrap();
            assert_eq!(reopened.replay(0).unwrap().len(), 1);
            assert_eq!(
                crate::vfs::read_to_vec(&*vfs, "wal/00000001.log").unwrap(),
                first
            );
            assert_eq!(reopened.append_commit(2, &[]).unwrap(), 2);
            drop(reopened);
            assert_eq!(
                Wal::open(vfs, "wal", 0).unwrap().replay(0).unwrap().len(),
                2
            );
        }
    }
}

#[test]
fn short_magic_repair_requires_valid_predecessor_and_exact_prefix() {
    for (bad, corrupt_predecessor) in [
        (b"MZ".as_slice(), false),
        (b"".as_slice(), false),
        (&MAGIC[..2], true),
    ] {
        let vfs = Arc::new(MemoryVfs::new());
        vfs.create_dir_all("wal").unwrap();
        write_magic(&*vfs, "wal/00000001.log", MAGIC, true).unwrap();
        write_frame(
            &*vfs,
            "wal/00000001.log",
            br#"{"lsn":1,"commit_ts":1,"ops":[]}"#,
        );
        vfs.open("wal/00000002.log", VfsOpen::create_truncate())
            .unwrap()
            .write_all(bad)
            .unwrap();
        if corrupt_predecessor {
            // A bad CRC in the predecessor must be reported before touching
            // even an otherwise repairable highest magic.
            let mut corrupt = crate::vfs::read_to_vec(&*vfs, "wal/00000001.log").unwrap();
            corrupt[8] ^= 1;
            crate::vfs::atomic_write(&*vfs, "wal/00000001.log", &corrupt).unwrap();
        }
        assert!(Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).is_err());
        assert_eq!(
            crate::vfs::read_to_vec(&*vfs, "wal/00000002.log").unwrap(),
            bad
        );
    }
    let vfs = Arc::new(MemoryVfs::new());
    vfs.open("00000001.log", VfsOpen::create_truncate())
        .unwrap()
        .write_all(&MAGIC[..2])
        .unwrap();
    assert!(
        Wal::open(vfs, "", 0).is_err(),
        "sole segment cannot be repaired"
    );
    let vfs = Arc::new(MemoryVfs::new());
    vfs.create_dir_all("wal").unwrap();
    vfs.open("wal/00000001.log", VfsOpen::create_truncate())
        .unwrap()
        .write_all(&MAGIC[..3])
        .unwrap();
    write_magic(&*vfs, "wal/00000002.log", MAGIC, true).unwrap();
    assert!(
        Wal::open(vfs, "wal", 0).is_err(),
        "closed segment cannot be repaired"
    );
}

struct ReadFaultVfs {
    inner: MemoryVfs,
    fail_on: Arc<AtomicUsize>,
    repair_chunks_only: bool,
    fail_rename: AtomicBool,
    fail_write_on: AtomicUsize,
    repair_reads: Arc<AtomicUsize>,
    repair_writes: Arc<AtomicUsize>,
    max_repair_write: Arc<AtomicUsize>,
    open_sources: Arc<AtomicUsize>,
}
impl ReadFaultVfs {
    fn new(fail_on: usize, repair_chunks_only: bool) -> Self {
        Self {
            inner: MemoryVfs::new(),
            fail_on: Arc::new(AtomicUsize::new(fail_on)),
            repair_chunks_only,
            fail_rename: AtomicBool::new(false),
            fail_write_on: AtomicUsize::new(usize::MAX),
            repair_reads: Arc::new(AtomicUsize::new(0)),
            repair_writes: Arc::new(AtomicUsize::new(0)),
            max_repair_write: Arc::new(AtomicUsize::new(0)),
            open_sources: Arc::new(AtomicUsize::new(0)),
        }
    }
}
struct ReadFaultFile {
    inner: Box<dyn VfsFile>,
    fail_on: Arc<AtomicUsize>,
    reads: usize,
    repair_chunks_only: bool,
    repair_reads: Arc<AtomicUsize>,
    open_sources: Arc<AtomicUsize>,
}
impl Drop for ReadFaultFile {
    fn drop(&mut self) {
        self.open_sources.fetch_sub(1, Ordering::Relaxed);
    }
}
struct RepairWriteFile {
    inner: Box<dyn VfsFile>,
    writes: Arc<AtomicUsize>,
    max_write: Arc<AtomicUsize>,
    fail_on: usize,
}
impl VfsFile for RepairWriteFile {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        let call = self.writes.fetch_add(1, Ordering::Relaxed) + 1;
        self.max_write.fetch_max(buf.len(), Ordering::Relaxed);
        if call == self.fail_on {
            return Err(Error::io("injected WAL copy write failure"));
        }
        self.inner.write_all(buf)
    }
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.inner.read_exact(buf)
    }
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        self.inner.seek(pos)
    }
    fn sync_all(&mut self) -> Result<()> {
        self.inner.sync_all()
    }
    fn len(&self) -> Result<u64> {
        self.inner.len()
    }
    fn set_len(&mut self, len: u64) -> Result<()> {
        self.inner.set_len(len)
    }
}
impl VfsFile for ReadFaultFile {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.inner.write_all(buf)
    }
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        // In repair-chunks-only mode only 64 KiB reads count or fail.
        if self.repair_chunks_only && buf.len() != 64 * 1024 {
            return self.inner.read_exact(buf);
        }
        self.reads += 1;
        if self.repair_chunks_only {
            self.repair_reads.fetch_add(1, Ordering::Relaxed);
        }
        if self.reads == self.fail_on.load(Ordering::Relaxed) {
            return Err(Error::io("injected WAL read failure"));
        }
        self.inner.read_exact(buf)
    }
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        self.inner.seek(pos)
    }
    fn sync_all(&mut self) -> Result<()> {
        self.inner.sync_all()
    }
    fn len(&self) -> Result<u64> {
        self.inner.len()
    }
    fn set_len(&mut self, len: u64) -> Result<()> {
        self.inner.set_len(len)
    }
}
impl Vfs for ReadFaultVfs {
    fn durable(&self) -> bool {
        self.inner.durable()
    }
    fn create_dir_all(&self, path: &str) -> Result<()> {
        self.inner.create_dir_all(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        self.inner.list_dir(path)
    }
    fn exists(&self, path: &str) -> Result<bool> {
        self.inner.exists(path)
    }
    fn metadata(&self, path: &str) -> Result<VfsMeta> {
        self.inner.metadata(path)
    }
    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>> {
        let inner = self.inner.open(path, opts)?;
        if opts.read && !opts.write {
            self.open_sources.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(ReadFaultFile {
                inner,
                fail_on: Arc::clone(&self.fail_on),
                reads: 0,
                repair_chunks_only: self.repair_chunks_only,
                repair_reads: Arc::clone(&self.repair_reads),
                open_sources: Arc::clone(&self.open_sources),
            }))
        } else if opts.create_new {
            Ok(Box::new(RepairWriteFile {
                inner,
                writes: Arc::clone(&self.repair_writes),
                max_write: Arc::clone(&self.max_repair_write),
                fail_on: self.fail_write_on.load(Ordering::Relaxed),
            }))
        } else {
            Ok(inner)
        }
    }
    fn remove_file(&self, path: &str) -> Result<()> {
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &str) -> Result<()> {
        self.inner.remove_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        assert_eq!(self.open_sources.load(Ordering::Relaxed), 0);
        if self.fail_rename.load(Ordering::Relaxed) {
            return Err(Error::io("injected WAL replacement failure"));
        }
        self.inner.rename(from, to)
    }
}

#[test]
fn wal_repair_streams_many_chunks_and_preserves_original_on_io_failure() {
    let vfs = Arc::new(ReadFaultVfs::new(usize::MAX, true));
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    append_n(&mut wal, 20_000); // Intact prefix spans many 64 KiB reads.
    drop(wal);
    let path = "wal/00000001.log";
    write_header(&vfs.inner, path, 12);
    let before = crate::vfs::read_to_vec(&vfs.inner, path).unwrap();

    vfs.fail_on.store(5, Ordering::Relaxed);
    assert!(format!(
        "{}",
        Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0)
            .err()
            .unwrap()
    )
    .contains("injected WAL read failure"));
    assert_eq!(crate::vfs::read_to_vec(&vfs.inner, path).unwrap(), before);
    assert_eq!(vfs.inner.list_dir("wal").unwrap(), ["00000001.log"]);

    vfs.fail_on.store(usize::MAX, Ordering::Relaxed);
    vfs.fail_write_on.store(5, Ordering::Relaxed);
    vfs.repair_writes.store(0, Ordering::Relaxed);
    assert!(format!(
        "{}",
        Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0)
            .err()
            .unwrap()
    )
    .contains("injected WAL copy write failure"));
    assert_eq!(crate::vfs::read_to_vec(&vfs.inner, path).unwrap(), before);
    assert_eq!(vfs.inner.list_dir("wal").unwrap(), ["00000001.log"]);

    vfs.fail_write_on.store(usize::MAX, Ordering::Relaxed);
    vfs.fail_rename.store(true, Ordering::Relaxed);
    assert!(format!(
        "{}",
        Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0)
            .err()
            .unwrap()
    )
    .contains("injected WAL replacement failure"));
    assert_eq!(crate::vfs::read_to_vec(&vfs.inner, path).unwrap(), before);
    assert_eq!(vfs.inner.list_dir("wal").unwrap(), ["00000001.log"]);

    vfs.fail_rename.store(false, Ordering::Relaxed);
    vfs.repair_reads.store(0, Ordering::Relaxed);
    vfs.repair_writes.store(0, Ordering::Relaxed);
    vfs.max_repair_write.store(0, Ordering::Relaxed);
    let mut repaired = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    let writes = vfs.repair_writes.load(Ordering::Relaxed);
    assert!(
        writes > 8,
        "expected many bounded copy writes, got {writes}"
    );
    assert_eq!(vfs.repair_reads.load(Ordering::Relaxed), writes - 1);
    assert_eq!(vfs.max_repair_write.load(Ordering::Relaxed), 64 * 1024);
    assert_eq!(
        vfs.inner.metadata(path).unwrap().len,
        before.len() as u64 - 8
    );
    assert_eq!(repaired.replay(0).unwrap().len(), 20_000);
    assert_eq!(repaired.append_commit(20_001, &[]).unwrap(), 20_001);
    drop(repaired);
    assert_eq!(
        Wal::open(vfs, "wal", 0).unwrap().replay(0).unwrap().len(),
        20_001
    );
}

#[test]
fn wal_magic_header_and_payload_read_errors_are_not_eof_or_torn_tail() {
    for read in 1..=3 {
        let vfs = ReadFaultVfs::new(read, false);
        write_magic(&vfs.inner, "00000001.log", MAGIC, true).unwrap();
        write_frame(
            &vfs.inner,
            "00000001.log",
            br#"{"lsn":1,"commit_ts":1,"ops":[]}"#,
        );
        assert!(
            format!("{}", Wal::open(Arc::new(vfs), "", 0).err().unwrap())
                .contains("injected WAL read failure"),
            "read {read}"
        );
    }
    // CDC must not rescan from zero on a genuine read failure at a hint.
    let vfs = Arc::new(ReadFaultVfs::new(usize::MAX, false));
    let payload = br#"{"lsn":1,"commit_ts":1,"ops":[]}"#;
    write_magic(&vfs.inner, "00000001.log", MAGIC, true).unwrap();
    write_frame(&vfs.inner, "00000001.log", payload);
    let mut wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "", 0).unwrap();
    assert_eq!(wal.records_after(0, 1).unwrap().len(), 1);
    // A read failure on the next frame is propagated even when the scan
    // begins at an otherwise valid cached offset.
    write_frame(
        &*wal.inner.vfs,
        "00000001.log",
        br#"{"lsn":2,"commit_ts":2,"ops":[]}"#,
    );
    wal.lsn = 2;
    wal.inner.cdc_hint = Some(super::WalCursor {
        lsn: 1,
        seg_id: 1,
        offset: MAGIC.len() as u64 + 8 + payload.len() as u64,
        commit_ts: 1,
    });
    vfs.fail_on.store(1, Ordering::Relaxed); // hinted scan's header read
    let err = wal.records_after(1, 1).err().unwrap();
    assert!(format!("{err}").contains("injected WAL read failure"));
}

fn file_wal() -> (tempfile::TempDir, Wal) {
    let dir = tempfile::tempdir().unwrap();
    let vfs = Arc::new(NativeVfs::new(dir.path()));
    let wal = Wal::open(vfs, "", 0).unwrap();
    (dir, wal)
}

fn bundle_wal() -> (tempfile::TempDir, Arc<BundleVfs>, Wal) {
    let dir = tempfile::tempdir().unwrap();
    let vfs = Arc::new(BundleVfs::open_or_create(dir.path().join("graph.cl")).unwrap());
    let wal = Wal::open(Arc::clone(&vfs) as Arc<dyn Vfs>, "wal", 0).unwrap();
    (dir, vfs, wal)
}

fn append_n(wal: &mut Wal, n: usize) {
    let start = wal.last_commit_ts;
    for i in 0..n {
        wal.append_commit(start + i as u64 + 1, &[]).unwrap();
    }
}

#[test]
fn records_after_limit_and_caught_up() {
    let (_dir, mut wal) = file_wal();
    append_n(&mut wal, 5);
    assert!(wal.records_after(0, 0).unwrap().is_empty());
    let page = wal.records_after(0, 2).unwrap();
    assert_eq!(page.iter().map(|r| r.lsn).collect::<Vec<_>>(), vec![1, 2]);
    let next = wal.records_after(2, 2).unwrap();
    assert_eq!(next.iter().map(|r| r.lsn).collect::<Vec<_>>(), vec![3, 4]);
    let last = wal.records_after(4, 8).unwrap();
    assert_eq!(last.iter().map(|r| r.lsn).collect::<Vec<_>>(), vec![5]);
    assert!(wal.records_after(5, 8).unwrap().is_empty());
    assert!(wal.records_after(99, 8).unwrap().is_empty());
}

#[test]
fn records_after_skips_closed_segments() {
    let (dir, mut wal) = file_wal();
    append_n(&mut wal, 4);
    wal.rotate().unwrap();
    append_n(&mut wal, 3);
    assert!(dir.path().join("00000001.log").exists());
    assert!(dir.path().join("00000002.log").exists());
    let tail = wal.records_after(4, 16).unwrap();
    assert_eq!(
        tail.iter().map(|r| r.lsn).collect::<Vec<_>>(),
        vec![5, 6, 7]
    );
    let mid = wal.records_after(2, 2).unwrap();
    assert_eq!(mid.iter().map(|r| r.lsn).collect::<Vec<_>>(), vec![3, 4]);
    let across = wal.records_after(3, 3).unwrap();
    assert_eq!(
        across.iter().map(|r| r.lsn).collect::<Vec<_>>(),
        vec![4, 5, 6]
    );
}

#[test]
fn records_after_hint_survives_rotate() {
    let (_dir, mut wal) = file_wal();
    append_n(&mut wal, 3);
    let first = wal.records_after(0, 3).unwrap();
    assert_eq!(first.last().map(|r| r.lsn), Some(3));
    wal.rotate().unwrap();
    append_n(&mut wal, 2);
    let next = wal.records_after(3, 16).unwrap();
    assert_eq!(next.iter().map(|r| r.lsn).collect::<Vec<_>>(), vec![4, 5]);
}

#[test]
fn records_after_after_recycle_of_hinted_segment() {
    let (dir, mut wal) = file_wal();
    append_n(&mut wal, 3);
    let first = wal.records_after(0, 3).unwrap();
    assert_eq!(first.last().map(|r| r.lsn), Some(3));
    let cut = wal.rotate().unwrap();
    append_n(&mut wal, 2);
    wal.recycle(cut).unwrap();
    assert!(!dir.path().join("00000001.log").exists());
    let rest = wal.records_after(3, 16).unwrap();
    assert_eq!(rest.iter().map(|r| r.lsn).collect::<Vec<_>>(), vec![4, 5]);
}

/// Durability barrier (ADR-0077): a synced record must survive the handle
/// being dropped and the WAL directory reopened, replaying intact.
#[test]
fn sync_preserves_record_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let ops = vec![RedoOp::DeleteNode { id: 7 }, RedoOp::DeleteRel { id: 9 }];
    {
        let vfs = Arc::new(NativeVfs::new(dir.path()));
        let mut wal = Wal::open(vfs, "", 0).unwrap();
        assert_eq!(wal.append_commit(42, &ops).unwrap(), 1);
        wal.sync().unwrap();
    } // handle closed here; only data flushed by the barrier can be replayed

    let reopened = Wal::open(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap();
    assert_eq!(reopened.lsn, 1);
    let recs = reopened.replay(0).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].lsn, 1);
    assert_eq!(recs[0].commit_ts, 42);
    assert_eq!(recs[0].ops, ops);
}

#[test]
fn bundle_sync_preserves_record_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    let ops = vec![RedoOp::DeleteNode { id: 7 }, RedoOp::DeleteRel { id: 9 }];
    {
        let vfs = Arc::new(BundleVfs::open_or_create(&path).unwrap());
        let mut wal = Wal::open(vfs, "wal", 0).unwrap();
        assert_eq!(wal.append_commit(42, &ops).unwrap(), 1);
        wal.sync().unwrap();
    }
    let vfs = Arc::new(BundleVfs::open_or_create(&path).unwrap());
    let reopened = Wal::open(vfs, "wal", 0).unwrap();
    assert_eq!(reopened.lsn, 1);
    let recs = reopened.replay(0).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].ops, ops);
}

#[test]
fn bundle_rotate_after_sync_does_not_add_a_second_barrier() {
    let (_dir, _vfs, mut wal) = bundle_wal();
    for i in 0..3 {
        wal.append_record(i + 1, &[]).unwrap();
    }
    assert_eq!(wal.barrier_calls, 0);
    wal.sync().unwrap();
    let after_sync = wal.barrier_calls;
    assert_eq!(after_sync, 1);
    wal.rotate().unwrap();
    assert_eq!(wal.barrier_calls, after_sync);
}

#[test]
fn bundle_recycle_deletes_virtual_segment() {
    let (_dir, vfs, mut wal) = bundle_wal();
    wal.append_commit(7, &[]).unwrap();
    let lsn = wal.rotate().unwrap();
    assert!(vfs.exists("wal/00000001.log").unwrap());
    assert!(vfs.exists("wal/00000002.log").unwrap());
    wal.recycle(lsn).unwrap();
    assert!(!vfs.exists("wal/00000001.log").unwrap());
    assert!(vfs.exists("wal/00000002.log").unwrap());
}

/// `sync` reports the barrier's elapsed time for the file backend and is
/// safe to call repeatedly (group commit syncs once per batch).
#[test]
fn file_sync_reports_timing() {
    let (_dir, mut wal) = file_wal();
    let stats = wal.append_commit_with_stats(1, &[]).unwrap();
    let again = wal.sync().unwrap();
    // A barrier with no new data is still legal and cheap; guard only
    // against a runaway (busy-wait / doubled barrier) in the hundreds of
    // seconds.
    assert!(
        stats.sync_ns < 600 * 1_000_000_000,
        "commit sync took {} ns",
        stats.sync_ns
    );
    assert!(again < 600 * 1_000_000_000, "sync took {again} ns");
}

#[test]
fn open_trims_torn_final_record_before_append() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("00000001.log");
    {
        let mut wal = Wal::open(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap();
        wal.append_commit(1, &[RedoOp::DeleteNode { id: 7 }])
            .unwrap();
    }
    let valid_len = std::fs::metadata(&path).unwrap().len();
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&123u32.to_le_bytes()).unwrap();
        file.write_all(&100u32.to_le_bytes()).unwrap();
        file.write_all(b"{\"").unwrap();
    }

    let mut wal = Wal::open(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap();
    assert_eq!(wal.lsn, 1);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), valid_len);
    assert_eq!(wal.append_commit(2, &[]).unwrap(), 2);
    assert_eq!(wal.replay(0).unwrap().len(), 2);
}

/// A rotate that crashed after create-truncate, or one still in progress,
/// leaves a highest segment shorter than its magic. The follower treats it
/// as frameless and picks up the writer's frames once the segment is whole.
#[test]
fn poll_follow_tolerates_a_short_highest_segment() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Wal::open(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap();
    writer
        .append_commit(1, &[RedoOp::DeleteNode { id: 7 }])
        .unwrap();
    writer.sync().unwrap();
    drop(writer);
    let mut follower = Wal::open_readonly(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap();
    assert!(matches!(follower.poll_follow(0).unwrap(), WalCatchUp::Idle));

    let short = dir.path().join("00000002.log");
    std::fs::write(&short, &MAGIC[..3]).unwrap();
    assert!(matches!(follower.poll_follow(0).unwrap(), WalCatchUp::Idle));
    assert_eq!(std::fs::metadata(&short).unwrap().len(), 3);

    let mut writer = Wal::open(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap();
    assert_eq!(writer.inner.seg_id, 2);
    writer
        .append_commit(2, &[RedoOp::DeleteNode { id: 8 }])
        .unwrap();
    writer.sync().unwrap();
    match follower.poll_follow(0).unwrap() {
        WalCatchUp::Records { records, cursor } => {
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].lsn, 2);
            assert_eq!(cursor.seg_id, 2);
            follower.commit_follow_cursor(cursor);
        }
        WalCatchUp::Idle => panic!("frames in the repaired segment were not delivered"),
        WalCatchUp::Gap => panic!("a repaired short segment is not a gap"),
    }
    assert!(matches!(follower.poll_follow(0).unwrap(), WalCatchUp::Idle));
}

/// A read-only open over an empty WAL whose highest segment is short must
/// not leave a cursor under the magic, or every later poll fails the offset
/// check even after the writer repairs the segment.
#[test]
fn readonly_open_on_short_highest_segment_polls_after_repair() {
    let dir = tempfile::tempdir().unwrap();
    drop(Wal::open(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap());
    let short = dir.path().join("00000002.log");
    std::fs::write(&short, &MAGIC[..3]).unwrap();

    let mut follower = Wal::open_readonly(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap();
    let cursor = follower.inner.cdc_hint.unwrap();
    assert_eq!(cursor.seg_id, 2);
    assert_eq!(cursor.offset, MAGIC.len() as u64);
    assert!(matches!(follower.poll_follow(0).unwrap(), WalCatchUp::Idle));

    let mut writer = Wal::open(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap();
    assert_eq!(writer.inner.seg_id, 2);
    writer
        .append_commit(1, &[RedoOp::DeleteNode { id: 7 }])
        .unwrap();
    writer.sync().unwrap();
    match follower.poll_follow(0).unwrap() {
        WalCatchUp::Records { records, cursor } => {
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].lsn, 1);
            assert_eq!(cursor.seg_id, 2);
        }
        WalCatchUp::Idle => panic!("frames in the repaired segment were not delivered"),
        WalCatchUp::Gap => panic!("a repaired short segment is not a gap"),
    }
}

/// A read-only open over a WAL whose *sole* segment is shorter than its
/// magic fails: with no verified predecessor the short header cannot be a
/// fresh rotate's, so it is reported as truncation, not tolerated as
/// frameless.
#[test]
fn readonly_open_rejects_a_short_sole_segment() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("00000001.log"), &MAGIC[..3]).unwrap();
    match Wal::open_readonly(Arc::new(NativeVfs::new(dir.path())), "", 0) {
        Err(err) => assert!(
            err.to_string().contains("truncated WAL magic"),
            "unexpected error: {err}"
        ),
        Ok(_) => panic!("a short sole segment must fail a read-only open"),
    }
}

#[test]
fn open_rejects_corruption_in_closed_segment() {
    use std::io::{Seek, Write};

    let dir = tempfile::tempdir().unwrap();
    {
        let mut wal = Wal::open(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap();
        wal.append_commit(1, &[RedoOp::DeleteNode { id: 7 }])
            .unwrap();
        wal.rotate().unwrap();
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join("00000001.log"))
        .unwrap();
    file.seek(SeekFrom::Start(16)).unwrap();
    file.write_all(b"!").unwrap();
    let reopened = Wal::open(Arc::new(NativeVfs::new(dir.path())), "", 0);
    assert!(matches!(reopened, Err(Error::Storage(_))));
}

/// `rotate_relaxed` does not barrier the rotated-out segment, so a crash
/// can leave a torn frame on a *closed* file. Open must trim that suffix
/// instead of failing the database.
#[test]
fn open_trims_torn_tail_on_closed_segment_after_relaxed_rotate() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let closed = dir.path().join("00000001.log");
    {
        let mut wal = Wal::open(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap();
        wal.append_record(1, &[RedoOp::DeleteNode { id: 7 }])
            .unwrap();
        wal.rotate_relaxed().unwrap();
        wal.append_record(2, &[]).unwrap();
    }
    let valid_len = std::fs::metadata(&closed).unwrap().len();
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&closed)
            .unwrap();
        file.write_all(&123u32.to_le_bytes()).unwrap();
        file.write_all(&100u32.to_le_bytes()).unwrap();
        file.write_all(b"{\"").unwrap();
    }

    let wal = Wal::open(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap();
    assert_eq!(wal.lsn, 2);
    assert_eq!(std::fs::metadata(&closed).unwrap().len(), valid_len);
    assert_eq!(wal.replay(0).unwrap().len(), 2);
}

/// Regression pin (ADR-0077, one barrier on the open data segment per group
/// flush): group commit syncs once per batch and then rotates the segment
/// under the same WAL mutex. The rotate must not add a second data-segment
/// barrier for records the `sync` already made durable. `barrier_calls`
/// counts `durability_barrier` invocations on the data segment only; the
/// fresh segment's magic write syncs separately (header, not a data
/// barrier) and is not counted.
#[test]
fn rotate_after_sync_does_not_add_a_second_barrier() {
    let (_dir, mut wal) = file_wal();
    // Group-commit appends: no barrier per record.
    for i in 0..3 {
        wal.append_record(i + 1, &[]).unwrap();
    }
    assert!(!wal.synced, "appends leave the open segment unsynced");
    assert_eq!(wal.barrier_calls, 0);
    wal.sync().unwrap();
    assert!(wal.synced);
    let after_sync = wal.barrier_calls;
    assert_eq!(after_sync, 1, "the group's commit barrier");
    wal.rotate().unwrap();
    assert_eq!(
        wal.barrier_calls, after_sync,
        "rotate after sync must not issue a second barrier"
    );
    assert!(!wal.synced, "the fresh segment's magic is not yet durable");
}

/// A rotate with no intervening `sync` must still barrier: the
/// checkpoint/manual-rotate paths do not sync first, so the segment's
/// appended bytes must be flushed by the rotate itself.
#[test]
fn rotate_without_sync_still_barriers() {
    let (_dir, mut wal) = file_wal();
    wal.append_record(1, &[]).unwrap();
    assert_eq!(wal.barrier_calls, 0);
    wal.rotate().unwrap();
    assert_eq!(wal.barrier_calls, 1);
}

/// Regression pin: the in-process WAL has no stable storage behind it, so
/// `sync` must stay a free no-op reporting 0 ns.
#[test]
fn memory_sync_is_noop() {
    let mut wal = Wal::memory();
    let stats = wal
        .append_commit_with_stats(1, &[RedoOp::DeleteNode { id: 1 }])
        .unwrap();
    assert_eq!(stats.sync_ns, 0);
    assert_eq!(wal.sync().unwrap(), 0);
    assert_eq!(wal.sync().unwrap(), 0);
}

/// Regression pin for the Windows checkpoint failures (os error 5):
/// `recycle` must never delete a segment whose handle is still open.
/// POSIX can't observe that failure mode (unlink of an open file
/// succeeds), so pin the invariant directly: after `rotate`, no fd in
/// this process references the rotated-out segment.
#[cfg(target_os = "linux")]
#[test]
fn rotate_leaves_no_open_fd_on_closed_segment() {
    let (dir, mut wal) = file_wal();
    append_n(&mut wal, 3);
    let seg1 = std::fs::canonicalize(dir.path().join("00000001.log")).unwrap();
    let cut = wal.rotate().unwrap();
    let still_open: Vec<_> = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .filter(|target| *target == seg1)
        .collect();
    assert!(
        still_open.is_empty(),
        "rotated segment still has open fds: {still_open:?}"
    );
    wal.recycle(cut).unwrap();
    assert!(!seg1.exists());
}

/// The on-disk WAL format must stay replayable across the typed-redo
/// refactor (commit bd3836d made `RedoOp` carry typed values): a record
/// written in the **old** shape — where `properties` / `value` were the
/// tagged JSON produced by `redo::props_json` / `redo::value_json` — must
/// decode through `read_seg_limited`'s
/// `serde_json::from_slice::<WalRecord>` and replay identically.
///
/// The JSON is written out by hand and fed through the real segment-file
/// read path, so the proof does not depend on the current serializer
/// (`serde_json::to_vec(&WalRecordRef)`). If the new typed fields stopped
/// decoding the old shape this test fails — that is the point.
#[test]
fn old_shape_wal_json_replays_on_typed_code() {
    use cypherlite_core::Value;
    use cypherlite_executor::Graph;

    // Exact old on-disk shape: `properties`/`value` are tagged JSON maps
    // (`{"t":"string","v":...}`), not the new typed codec.
    let payload = br#"{
        "lsn": 1,
        "commit_ts": 42,
        "ops": [
            {"op":"CreateNode","id":1,"labels":["Person"],
             "properties":{"name":{"t":"string","v":"Alice"},"age":{"t":"int","v":30}}},
            {"op":"CreateNode","id":2,"labels":["Person"],"properties":{}},
            {"op":"CreateRel","id":10,"start":1,"end":2,"rel_type":"KNOWS",
             "properties":{"weight":{"t":"float","v":1.5}}},
            {"op":"SetRelProp","id":10,"key":"since","value":{"t":"int","v":2020}}
        ]
    }"#;

    // Write it as a real segment: magic + (crc, len, payload) frame.
    let dir = tempfile::tempdir().unwrap();
    let mut seg = std::fs::File::create(dir.path().join("00000001.log")).unwrap();
    use std::io::Write;
    seg.write_all(MAGIC).unwrap();
    seg.write_all(&crc32fast::hash(payload).to_le_bytes())
        .unwrap();
    seg.write_all(&(payload.len() as u32).to_le_bytes())
        .unwrap();
    seg.write_all(payload).unwrap();
    drop(seg);

    // Decode exactly as `Wal::open` / `Wal::replay` do.
    let wal = Wal::open(Arc::new(NativeVfs::new(dir.path())), "", 0).unwrap();
    let recs = wal.replay(0).unwrap();
    assert_eq!(recs.len(), 1, "old-shape record must decode cleanly");
    let rec = &recs[0];
    assert_eq!((rec.lsn, rec.commit_ts), (1, 42));
    assert_eq!(rec.ops.len(), 4);

    let g = Graph::default();
    g.apply_redo(rec.ops.clone(), rec.commit_ts).unwrap();

    let ts = g.commit_ts();
    assert_eq!(g.live_node_count(), 2);
    assert_eq!(
        g.node_prop_at(1, ts, "name"),
        Some(Value::String("Alice".into()))
    );
    assert_eq!(g.node_prop_at(1, ts, "age"), Some(Value::Int(30)));
    assert_eq!(g.node_labels_at(1, ts), Some(vec!["Person".to_string()]));
    assert_eq!(g.rel_ends_at(10, ts), Some((1, 2)));
    assert_eq!(g.rel_prop_at(10, ts, "weight"), Some(Value::Float(1.5)));
    assert_eq!(g.rel_prop_at(10, ts, "since"), Some(Value::Int(2020)));
}
