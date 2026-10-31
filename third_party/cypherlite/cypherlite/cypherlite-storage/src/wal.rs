//! WAL: every commit is a record here, then applied to the graph (ADR-0014).
//! Filesystem WAL is segmented (ADR-0020): append the open file, checkpoint
//! deletes closed segments with `max_lsn <= checkpoint_lsn`.

use std::sync::Arc;
use web_time::Instant;

use cypherlite_core::{Error, Result};
use cypherlite_executor::RedoOp;
use serde::{Deserialize, Serialize};

use crate::vfs::{BoundedSink, MemoryVfs, Vfs, VfsFile, VfsOpen};

/// WAL segment identity header. Shared with native ownership admission so a
/// segment is recognized by one byte string (ADR-0090). The marker lives in
/// the store-version registry (ADR-0127); re-exported for existing sites.
pub use crate::version::wal::CURRENT_MAGIC as MAGIC;
/// Magic of a segment created while the store holds an ontology
/// (ADR-0120 §4). Re-exported for the scanner's corruption check and tests.
pub(crate) use crate::version::wal::ONTOLOGY_MAGIC;
pub(super) const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024; // ADR-0086
pub(super) const MAX_PAYLOAD_MESSAGE: &str = "WAL payload exceeds 64 MiB limit";
pub const CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;
pub const CHECKPOINT_COMMITS: u64 = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalRecord {
    pub lsn: u64,
    pub commit_ts: u64,
    pub ops: Vec<RedoOp>,
}

#[derive(Serialize)]
struct WalRecordRef<'a> {
    lsn: u64,
    commit_ts: u64,
    ops: &'a [RedoOp],
}

/// Serialize one record through the shared bounded sink, stopping at the
/// 64 MiB payload limit. The sink can count first without retaining bytes,
/// then capture the one real payload.
fn encode_record(
    lsn: u64,
    commit_ts: u64,
    ops: &[RedoOp],
    sink: &mut BoundedSink<'_>,
) -> Result<()> {
    let result = serde_json::to_writer(
        &mut *sink,
        &WalRecordRef {
            lsn,
            commit_ts,
            ops,
        },
    );
    if sink.exceeded() || sink.bytes() == 0 {
        return Err(Error::storage(MAX_PAYLOAD_MESSAGE));
    }
    result.map_err(|e| Error::storage(format!("WAL serialization failed: {e}")))
}

/// Conservative payload bytes for a record whose final LSN and commit
/// timestamp are not assigned yet. Both fields are unsigned decimal values,
/// so serializing their maximum value reserves at most 38 bytes beyond the
/// eventual record while preserving the exact encoding of every redo op.
pub fn commit_payload_reservation_bytes(ops: &[RedoOp]) -> Result<u64> {
    let mut sink = BoundedSink::count(MAX_PAYLOAD_BYTES as u64, MAX_PAYLOAD_MESSAGE);
    encode_record(u64::MAX, u64::MAX, ops, &mut sink)?;
    Ok(sink.bytes())
}

/// Proof that one prospective WAL record was validated and encoded exactly
/// once. The proof owns the encoded payload; [`Wal::append_preflighted`]
/// writes those exact bytes as the record frame without re-encoding them.
pub struct WalPreflight {
    lsn: u64,
    commit_ts: u64,
    payload: Vec<u8>,
    /// Whether the record carries a store-level ontology op (ADR-0120 §3).
    /// Decided at preflight, while the ops are still available, so
    /// [`Wal::append_preflighted`] can apply the ADR-0120 §4 rule without
    /// re-decoding the payload: the op never lands in an `MDBWAL01`
    /// segment — the WAL first rotates to a fresh `MDBWAL02` segment.
    ontology_op: bool,
    /// Encoding time measured at preflight and folded into the append's
    /// `encode_write_ns`, so that metric stays the record's encode + write
    /// cost.
    encode_ns: u64,
}

/// Split of [`Wal::append_commit`]: JSON+write vs durable `sync_all`.
#[derive(Debug, Clone, Copy)]
pub struct AppendCommitStats {
    pub lsn: u64,
    pub encode_write_ns: u64,
    pub sync_ns: u64,
}

pub(super) struct FileWal {
    pub(super) vfs: Arc<dyn Vfs>,
    pub(super) prefix: String,
    pub(super) seg_id: u32,
    pub(super) file: Box<dyn VfsFile>,
    /// Closed segments still on disk: (id, max_lsn, commit_ts).
    pub(super) closed: Vec<(u32, u64, u64)>,
    /// After the last record returned by [`Wal::records_after`].
    /// The next call with the same `from_lsn` seeks here instead of rescanning the prefix.
    pub(super) cdc_hint: Option<WalCursor>,
}

/// Position after the last WAL frame a follower or CDC reader has consumed.
#[derive(Clone, Copy, Debug)]
pub struct WalCursor {
    pub lsn: u64,
    pub seg_id: u32,
    pub offset: u64,
    pub commit_ts: u64,
}

/// Outcome of one read-only WAL poll (ADR-0137).
#[derive(Debug)]
pub enum WalCatchUp {
    /// No complete frame past the cursor.
    Idle,
    /// Contiguous frames the caller should apply, in LSN order.
    ///
    /// `cursor` is the position after the last record. It is not stored on
    /// the WAL until [`Wal::commit_follow_cursor`]; a failed apply must
    /// leave the previous cursor in place.
    Records {
        records: Vec<WalRecord>,
        cursor: WalCursor,
    },
    /// The cursor cannot be continued. Reload the snapshot and WAL.
    Gap,
}

pub struct Wal {
    inner: FileWal,
    pub lsn: u64,
    pub bytes: u64,
    pub commits: u64,
    last_commit_ts: u64,
    /// True when the open segment has no bytes appended since the last
    /// [`Wal::sync`] (ADR-0077). Lets `rotate` skip a redundant durability
    /// barrier when the caller already synced this group flush.
    synced: bool,
    /// Physical publication may have happened despite an I/O error. This
    /// handle cannot safely append or recycle again until recovery reopens it.
    poisoned: bool,
    /// Opened for a read-only database (ADR-0136). Append, rotate, recycle,
    /// and torn-tail trim are refused.
    read_only: bool,
    /// Whether the store currently holds an ontology, committed or still
    /// pending in this WAL (ADR-0120 §4). Segments created while set start
    /// with `MDBWAL02`; otherwise `MDBWAL01`. The store layer owns this
    /// state and drives it through [`Wal::set_ontology_held`]; the WAL
    /// itself sets it when it rotates for an ontology op, and defaults it
    /// from the active segment's magic at open.
    ontology_held: bool,
    /// Identity magic of the active segment. An ontology op may only be
    /// appended to an `MDBWAL02` segment (ADR-0120 §4); appending one while
    /// this is `MDBWAL01` rotates to a fresh `MDBWAL02` segment first.
    active_magic: [u8; 8],
    /// Test-only count of durability barriers issued; lets the "one barrier per
    /// group flush" invariant be pinned directly (a real fsync is not otherwise
    /// observable without crash simulation).
    #[cfg(test)]
    barrier_calls: u64,
}

impl Wal {
    /// Check every redo node/relationship ID, relationship endpoint and pull
    /// identity local ID is in `0..i64::MAX` before persisting or applying a
    /// batch. Invalid IDs return `Error::Storage`; no WAL or graph state is changed.
    ///
    /// ```
    /// use cypherlite_executor::RedoOp;
    /// use cypherlite_storage::Wal;
    /// let ops = [RedoOp::CreateNode {
    ///     id: -1, labels: Default::default(), properties: Default::default(),
    /// }];
    /// assert!(Wal::validate_redo_ids(&ops).is_err());
    /// ```
    pub fn validate_redo_ids(ops: &[RedoOp]) -> Result<()> {
        for op in ops {
            op.validate_ids()?;
        }
        Ok(())
    }

    pub fn open(vfs: Arc<dyn Vfs>, prefix: &str, checkpoint_lsn: u64) -> Result<Self> {
        vfs.create_dir_all(prefix)?;
        let mut ids = list_segment_ids(&*vfs, prefix)?;
        if ids.is_empty() {
            ids.push(1);
            // A brand-new store holds no ontology: `MDBWAL01` (ADR-0120 §4).
            write_magic(&*vfs, &segment_path(prefix, 1), MAGIC, true)?;
        }
        let current_id = *ids.last().unwrap();
        // Tolerated-tail trims are applied only after every segment has been
        // scanned without error: a hard failure must leave all WAL bytes
        // untouched (ADR-0114).
        let scan = scan_segments(&*vfs, prefix, &ids, checkpoint_lsn, true)?;
        for (path, end) in &scan.trims {
            replace_torn_tail(&*vfs, path, *end)?;
        }
        let active_path = segment_path(prefix, current_id);
        let file = open_append(&*vfs, &active_path)?;
        let mut wal = Self {
            inner: FileWal {
                vfs,
                prefix: prefix.to_string(),
                seg_id: current_id,
                file,
                closed: scan.closed,
                cdc_hint: None,
            },
            lsn: scan.lsn,
            bytes: scan.current_bytes,
            commits: scan.current_commits,
            last_commit_ts: scan.last_commit_ts,
            synced: true,
            poisoned: false,
            read_only: false,
            // Conservative default until the store layer reports the real
            // ontology state: an active `MDBWAL02` segment means the WAL
            // holds (or held) an ontology op, so keep writing `MDBWAL02`.
            ontology_held: scan.current_magic == *ONTOLOGY_MAGIC,
            active_magic: scan.current_magic,
            #[cfg(test)]
            barrier_calls: 0,
        };
        // A trimmed active segment must not grow again under the same id.
        // A follower cursor in the discarded tail then sees `len < cursor`
        // and reloads, instead of applying frames that reused those offsets.
        if scan.trimmed_active {
            wal.rotate_away_from_trimmed_active()?;
        }
        Ok(wal)
    }

    /// Scan an existing WAL without creating segments or trimming a torn tail
    /// (ADR-0136). Incomplete frames at the end of the active segment are
    /// ignored. A missing WAL is an error.
    pub fn open_readonly(vfs: Arc<dyn Vfs>, prefix: &str, checkpoint_lsn: u64) -> Result<Self> {
        let ids = list_segment_ids(&*vfs, prefix)?;
        if ids.is_empty() {
            return Err(Error::storage("read-only open requires an existing WAL"));
        }
        let current_id = *ids.last().unwrap();
        let scan = scan_segments(&*vfs, prefix, &ids, checkpoint_lsn, false)?;
        let active_path = segment_path(prefix, current_id);
        let file = vfs.open(&active_path, VfsOpen::read_only())?;
        // A skipped short highest segment leaves `current_bytes` under the
        // magic. A cursor there would fail the offset check on every poll.
        let cursor = scan.cursor.unwrap_or(WalCursor {
            lsn: scan.lsn,
            seg_id: current_id,
            offset: scan.current_bytes.max(MAGIC.len() as u64),
            commit_ts: scan.last_commit_ts,
        });
        Ok(Self {
            inner: FileWal {
                vfs,
                prefix: prefix.to_string(),
                seg_id: current_id,
                file,
                closed: scan.closed,
                cdc_hint: Some(cursor),
            },
            lsn: scan.lsn,
            bytes: scan.current_bytes,
            commits: scan.current_commits,
            last_commit_ts: scan.last_commit_ts,
            synced: true,
            poisoned: false,
            read_only: true,
            ontology_held: scan.current_magic == *ONTOLOGY_MAGIC,
            active_magic: scan.current_magic,
            #[cfg(test)]
            barrier_calls: 0,
        })
    }

    pub fn memory() -> Self {
        Self::open(Arc::new(MemoryVfs::new()), "wal", 0).expect("memory WAL")
    }

    pub fn append_commit(&mut self, commit_ts: u64, ops: &[RedoOp]) -> Result<u64> {
        Ok(self.append_commit_with_stats(commit_ts, ops)?.lsn)
    }

    /// Encode + write vs `sync_all` (0 ns on the in-process WAL). Borrows the
    /// ops (like [`append_record`]); a caller that owns them keeps ownership to
    /// apply them without a clone.
    pub fn append_commit_with_stats(
        &mut self,
        commit_ts: u64,
        ops: &[RedoOp],
    ) -> Result<AppendCommitStats> {
        let written = self.append_record(commit_ts, ops)?;
        let sync_ns = self.sync()?;
        Ok(AppendCommitStats {
            lsn: written.lsn,
            encode_write_ns: written.encode_write_ns,
            sync_ns,
        })
    }

    /// Append a WAL record without `sync_all` (ADR-0019 group commit).
    ///
    /// Borrows the ops: the file backend only encodes them, so the hot
    /// group-commit path pays no deep clone (ADR-0063).
    pub fn append_record(&mut self, commit_ts: u64, ops: &[RedoOp]) -> Result<AppendCommitStats> {
        self.ensure_writable()?;
        let next_lsn = self
            .lsn
            .checked_add(1)
            .ok_or_else(|| Error::storage("WAL LSN overflow"))?;
        let proof = self.preflight_record(next_lsn, commit_ts, ops)?;
        self.append_preflighted(proof)
    }

    /// Append a previously validated, encoded record. Writes the proof's
    /// owned payload as the frame as-is: no JSON is re-encoded and no size is
    /// re-checked here.
    ///
    /// ADR-0120 §4: a record carrying an ontology op never lands in an
    /// `MDBWAL01` segment — the WAL first rotates to a fresh `MDBWAL02`
    /// segment.
    pub fn append_preflighted(&mut self, proof: WalPreflight) -> Result<AppendCommitStats> {
        self.ensure_writable()?;
        let WalPreflight {
            lsn,
            commit_ts,
            payload,
            ontology_op,
            encode_ns,
        } = proof;
        if self.lsn.checked_add(1) != Some(lsn)
            || commit_ts == 0
            || commit_ts == u64::MAX
            || commit_ts <= self.last_commit_ts
        {
            return Err(Error::storage("stale WAL preflight proof"));
        }
        if ontology_op && self.active_magic != *ONTOLOGY_MAGIC {
            self.rotate_for_ontology_op()?;
        }
        let t0 = Instant::now();
        let len = payload.len() as u32;
        let crc = crc32fast::hash(&payload);
        if let Err(e) = (|| {
            self.inner.file.write_all(&crc.to_le_bytes())?;
            self.inner.file.write_all(&len.to_le_bytes())?;
            self.inner.file.write_all(&payload)
        })() {
            self.poisoned = true;
            return Err(e);
        }
        self.lsn = lsn;
        self.bytes += 8 + payload.len() as u64;
        let encode_write_ns = encode_ns + t0.elapsed().as_nanos() as u64;
        self.commits += 1;
        self.last_commit_ts = commit_ts;
        // Bytes were appended since the last barrier.
        self.synced = false;
        Ok(AppendCommitStats {
            lsn: self.lsn,
            encode_write_ns,
            sync_ns: 0,
        })
    }

    /// Validate and encode the exact prospective record once, returning an
    /// owned proof. Callers planning a group pass the assigned LSN for each
    /// candidate.
    ///
    /// ```
    /// use cypherlite_storage::Wal;
    /// let mut wal = Wal::memory();
    /// let ops = [];
    /// let proof = wal.preflight_record(1, 1, &ops).unwrap();
    /// assert_eq!(wal.lsn, 0); // preflight never appends
    /// assert_eq!(wal.append_preflighted(proof).unwrap().lsn, 1);
    /// wal.sync().unwrap();
    /// assert_eq!(wal.replay(0).unwrap().len(), 1);
    /// ```
    pub fn preflight_record(
        &self,
        lsn: u64,
        commit_ts: u64,
        ops: &[RedoOp],
    ) -> Result<WalPreflight> {
        self.ensure_writable()?;
        if lsn == 0 || lsn <= self.lsn {
            return Err(Error::storage("WAL LSN must advance"));
        }
        if commit_ts == 0 || commit_ts == u64::MAX || commit_ts <= self.last_commit_ts {
            return Err(Error::storage(format!(
                "WAL commit timestamp {commit_ts} must advance past {} and be below u64::MAX",
                self.last_commit_ts
            )));
        }
        // The one validation pass per op: persisted-ID range plus the
        // ADR-0120 §3 16 MiB ontology envelope (the ADR-0086 object
        // bound), both inside `RedoOp::validate_ids`, before the commit is
        // written.
        Self::validate_redo_ids(ops)?;
        let ontology_op = ops.iter().any(RedoOp::is_ontology_op);
        // Allocate the one retained payload at the same bounded capacity used
        // by commit admission. This avoids geometric growth creating an
        // unaccounted transient buffer while serializing.
        let payload_capacity = commit_payload_reservation_bytes(ops)? as usize;
        let mut payload = Vec::with_capacity(payload_capacity);
        let t_encode = Instant::now();
        let mut sink =
            BoundedSink::capture(&mut payload, MAX_PAYLOAD_BYTES as u64, MAX_PAYLOAD_MESSAGE);
        encode_record(lsn, commit_ts, ops, &mut sink)?;
        Ok(WalPreflight {
            lsn,
            commit_ts,
            payload,
            ontology_op,
            encode_ns: t_encode.elapsed().as_nanos() as u64,
        })
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.read_only {
            Err(Error::storage("database is read-only"))
        } else if self.poisoned {
            Err(Error::storage("uncertain WAL outcome; close and reopen"))
        } else {
            Ok(())
        }
    }

    /// Durability barrier. No-op (0 ns) for a non-durable VFS. Durable
    /// backends use the platform's cheapest correct barrier (ADR-0077).
    pub fn sync(&mut self) -> Result<u64> {
        self.ensure_writable()?;
        if !self.inner.vfs.durable() {
            self.synced = true;
            return Ok(0);
        }
        let t1 = Instant::now();
        if let Err(e) = self.inner.file.durability_barrier() {
            self.poisoned = true;
            return Err(e);
        }
        #[cfg(test)]
        {
            self.barrier_calls += 1;
        }
        self.synced = true;
        Ok(t1.elapsed().as_nanos() as u64)
    }

    /// Whether durable bytes have been appended since the last barrier.
    pub fn needs_sync(&self) -> bool {
        self.inner.vfs.durable() && !self.synced
    }

    pub fn replay(&self, from_lsn: u64) -> Result<Vec<WalRecord>> {
        replay_dir(&*self.inner.vfs, &self.inner.prefix, from_lsn)
    }

    /// Internal recovery cursor: validates every frame, including those at or
    /// below the checkpoint, and transfers each newer record to the caller.
    pub(crate) fn for_each_after(
        &self,
        from_lsn: u64,
        mut apply: impl FnMut(WalRecord) -> Result<()>,
    ) -> Result<()> {
        scan_dir(&*self.inner.vfs, &self.inner.prefix, from_lsn, &mut apply)
    }

    /// Records with `lsn > from_lsn`, at most `limit` (ADR-0045).
    ///
    /// Caught-up polls (`from_lsn >=` current LSN) and `limit == 0` return
    /// empty without opening files. File WAL skips closed segments whose
    /// `max_lsn` is already acked and seeks past the last delivered record.
    pub fn records_after(&mut self, from_lsn: u64, limit: usize) -> Result<Vec<WalRecord>> {
        if limit == 0 || from_lsn >= self.lsn {
            return Ok(Vec::new());
        }
        records_after_file(&mut self.inner, from_lsn, limit)
    }

    /// Read complete frames the writer appended after this handle's cursor
    /// (ADR-0137). Does not trim or create segments.
    ///
    /// `Gap` means a segment this cursor still needs has disappeared, or the
    /// active segment shrank under the cursor. The caller reloads from the
    /// snapshot. A follower that has already applied `checkpoint_lsn` retargets
    /// onto the WAL that remains instead of reporting a gap.
    pub fn poll_follow(&mut self, checkpoint_lsn: u64) -> Result<WalCatchUp> {
        if !self.read_only {
            return Err(Error::storage("follow poll requires a read-only WAL"));
        }
        let ids = list_segment_ids(&*self.inner.vfs, &self.inner.prefix)?;
        if ids.is_empty() {
            return Ok(WalCatchUp::Gap);
        }
        let cursor = self.inner.cdc_hint.unwrap_or(WalCursor {
            lsn: self.lsn,
            seg_id: self.inner.seg_id,
            offset: MAGIC.len() as u64,
            commit_ts: self.last_commit_ts,
        });
        let cursor_seg = cursor.seg_id;
        let cursor_off = cursor.offset;
        let cursor_ts = cursor.commit_ts;
        let cursor_live = ids.contains(&cursor_seg);
        let active_id = *ids.last().unwrap();
        if cursor_live {
            let path = segment_path(&self.inner.prefix, cursor_seg);
            let len = self.inner.vfs.metadata(&path)?.len;
            // A highest segment shorter than its magic is a rotate in
            // progress or one that crashed. The read-only open skipped it
            // with a cursor at `MAGIC.len()`; that is not a shrink.
            let short_active = cursor_seg == active_id && len < MAGIC.len() as u64;
            if len < cursor_off && !(short_active && cursor_off == MAGIC.len() as u64) {
                return Ok(WalCatchUp::Gap);
            }
        } else if self.lsn < checkpoint_lsn {
            return Ok(WalCatchUp::Gap);
        }
        let start_seg = if cursor_live {
            cursor_seg
        } else {
            *ids.first().unwrap()
        };
        let mut out = Vec::new();
        let mut last_hint = None;
        let mut last_ts = if cursor_live {
            cursor_ts
        } else {
            self.last_commit_ts
        };
        let mut expect = self.lsn.saturating_add(1);
        for seg in ids.iter().copied().filter(|id| *id >= start_seg) {
            let path = segment_path(&self.inner.prefix, seg);
            let active = active_id == seg;
            // Mirror the read-only open: a highest segment that cannot hold
            // its magic yet has no frames. The writer repairs or completes
            // it; the follower reads it on a later poll.
            if active && self.inner.vfs.metadata(&path)?.len < MAGIC.len() as u64 {
                continue;
            }
            let start = if seg == cursor_seg && cursor_live {
                cursor_off
            } else {
                0
            };
            let (recs, end) = read_seg_limited(
                &*self.inner.vfs,
                &path,
                start,
                ScanWindow {
                    from_lsn: self.lsn,
                    limit: usize::MAX,
                    last_ts: &mut last_ts,
                },
                SegSource { id: seg, active },
            )?;
            for rec in recs {
                if rec.lsn != expect {
                    return Ok(WalCatchUp::Gap);
                }
                expect = expect.saturating_add(1);
                out.push(rec);
            }
            if let Some(hint) = end {
                last_hint = Some(hint);
            }
        }
        if out.is_empty() {
            if !cursor_live {
                let path = segment_path(&self.inner.prefix, active_id);
                let len = self.inner.vfs.metadata(&path)?.len;
                self.inner.seg_id = active_id;
                self.inner.cdc_hint = Some(WalCursor {
                    lsn: self.lsn,
                    seg_id: active_id,
                    offset: len.max(MAGIC.len() as u64),
                    commit_ts: self.last_commit_ts,
                });
            }
            return Ok(WalCatchUp::Idle);
        }
        let cursor = last_hint.ok_or_else(|| {
            Error::storage("WAL follow cursor missing after a non-empty catch-up")
        })?;
        Ok(WalCatchUp::Records {
            records: out,
            cursor,
        })
    }

    /// Store the cursor returned by [`Wal::poll_follow`] after every record
    /// in that batch has been applied.
    pub fn commit_follow_cursor(&mut self, cursor: WalCursor) {
        self.lsn = cursor.lsn;
        self.last_commit_ts = cursor.commit_ts;
        self.inner.seg_id = cursor.seg_id;
        self.inner.cdc_hint = Some(cursor);
    }

    pub fn should_checkpoint(&self) -> bool {
        self.bytes >= CHECKPOINT_BYTES || self.commits >= CHECKPOINT_COMMITS
    }

    pub fn has_closed_segments(&self) -> bool {
        !self.inner.closed.is_empty()
    }

    /// Open segment has records, or a closed segment is waiting for dump.
    pub fn has_uncheckpointed(&self) -> bool {
        self.commits > 0 || self.has_closed_segments()
    }

    /// ADR-0120 §4: tell the WAL whether the store currently holds an
    /// ontology — committed, or still pending in the WAL. Segments created
    /// while `held` is set start with `MDBWAL02`; otherwise `MDBWAL01`
    /// (ADR-0120 §8: after the ontology is dropped and a checkpoint
    /// publishes, new segments are `MDBWAL01` again). The store layer owns
    /// this state; the WAL also sets it itself when it rotates for an
    /// ontology op, and defaults it from the active segment's magic at
    /// open.
    pub fn set_ontology_held(&mut self, held: bool) {
        self.ontology_held = held;
    }

    /// Whether this WAL currently creates `MDBWAL02` segments (ADR-0120 §4).
    pub fn ontology_held(&self) -> bool {
        self.ontology_held
    }

    /// LSN and `commit_ts` of the latest **closed** segment (`:memory:`: the
    /// whole log). Pin this before releasing the WAL mutex; do not read the
    /// graph while the mutex is held (dump holds a graph read lock then
    /// waits for the mutex).
    pub fn checkpoint_pin(&self) -> (u64, u64) {
        self.inner
            .closed
            .last()
            .map(|(_, lsn, ts)| (*lsn, *ts))
            .unwrap_or((self.lsn, self.last_commit_ts))
    }

    /// Delete closed segments with `lsn <= checkpoint_lsn`.
    pub fn recycle(&mut self, checkpoint_lsn: u64) -> Result<()> {
        self.ensure_writable()?;
        let f = &mut self.inner;
        for (id, max_lsn, _) in &f.closed {
            if *max_lsn <= checkpoint_lsn {
                let path = segment_path(&f.prefix, *id);
                if let Err(e) = f.vfs.remove_file(&path) {
                    self.poisoned = true;
                    return Err(e);
                }
            }
        }
        f.closed.retain(|(_, max_lsn, _)| *max_lsn > checkpoint_lsn);
        if let Some(hint) = f.cdc_hint {
            let hid = hint.seg_id;
            if hid != f.seg_id && !f.closed.iter().any(|(id, _, _)| *id == hid) {
                f.cdc_hint = None;
            }
        }
        Ok(())
    }
}

mod rotation;
mod scan;
use scan::*;

/// Segment count under `prefix` (ADR-0165). Checkpoint recycles every
/// segment it covers, so each remaining segment may hold commits past the
/// last checkpoint — the cold `store_info` staleness hint. A missing prefix
/// reads as zero segments.
// The only caller is the wasm-gated `store_info`; on wasm32 storage builds
// this would be dead code.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn segment_count(vfs: &dyn Vfs, prefix: &str) -> Result<u32> {
    let ids = list_segment_ids(vfs, prefix)?;
    Ok(u32::try_from(ids.len()).unwrap_or(u32::MAX))
}

#[cfg(test)]
#[path = "wal/tests.rs"]
mod tests;
