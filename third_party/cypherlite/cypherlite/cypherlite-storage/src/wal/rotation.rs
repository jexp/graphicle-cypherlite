//! The segment-rotation cluster (ADR-0020): the close-and-swap tail
//! every rotation shares, with one durability barrier per group flush
//! (ADR-0077); the ontology-op rotation that keeps every ontology
//! record in an `MDBWAL02` segment (ADR-0120 §4); and the
//! trimmed-active rotation that keeps a trimmed segment from regrowing
//! under the same id.

use cypherlite_core::{Error, Result};

use super::scan::{open_append, segment_path, write_magic};
use super::{Wal, MAGIC, ONTOLOGY_MAGIC};

impl Wal {
    /// Close the open segment so a checkpoint can dump `lsn <=` this return
    /// value. No-op if the open segment has no records. `:memory:` returns
    /// the current LSN (all records are recyclable after dump).
    pub fn rotate(&mut self) -> Result<u64> {
        self.rotate_inner(true)
    }

    /// Rotate without issuing a durability barrier on either WAL segment.
    ///
    /// Relaxed durability modes use the completed checkpoint, not the WAL
    /// rotation itself, as their stable-storage boundary.
    pub fn rotate_relaxed(&mut self) -> Result<u64> {
        self.rotate_inner(false)
    }

    /// Move appends off an active segment whose torn tail was just trimmed.
    ///
    /// [`Wal::rotate`] does nothing when that segment has no remaining
    /// commits. The next append would then regrow the trimmed file, and a
    /// follower whose cursor sits in the discarded tail could apply the new
    /// frames without reloading.
    pub(super) fn rotate_away_from_trimmed_active(&mut self) -> Result<()> {
        if self.commits > 0 {
            self.rotate()?;
            return Ok(());
        }
        self.ensure_writable()?;
        self.swap_to_fresh_segment(true)
    }

    /// ADR-0120 §4: an ontology op never lands in an `MDBWAL01` segment.
    /// When one commits while the active segment is `MDBWAL01`, the WAL
    /// first rotates to a fresh `MDBWAL02` segment — unconditionally, even
    /// when the active segment holds no commits yet (where [`Wal::rotate`]
    /// would no-op). From this point the store is treated as
    /// ontology-holding until the store layer says otherwise through
    /// [`Wal::set_ontology_held`].
    pub(super) fn rotate_for_ontology_op(&mut self) -> Result<()> {
        self.ensure_writable()?;
        // Flip before the swap so the fresh segment gets `ONTOLOGY_MAGIC`
        // (ADR-0120 §4).
        self.ontology_held = true;
        self.close_active_segment(true)
    }

    /// The rotation tail shared by [`Wal::rotate_inner`] and
    /// [`Wal::rotate_for_ontology_op`]: barrier the old data segment once
    /// (ADR-0077), close it out, and swap in the fresh segment.
    fn close_active_segment(&mut self, barrier: bool) -> Result<()> {
        // Barrier only if the open segment holds bytes appended since the last
        // `sync`. Group commit syncs right after every append (ADR-0019) and
        // then rotates under the same WAL mutex, so this would otherwise be a
        // redundant second barrier per group flush; skipping it keeps one
        // barrier on the open data segment per group (ADR-0077).
        let durable = self.inner.vfs.durable();
        if barrier && durable && !self.synced {
            if let Err(e) = self.inner.file.durability_barrier() {
                self.poisoned = true;
                return Err(e);
            }
            #[cfg(test)]
            {
                self.barrier_calls += 1;
            }
        }
        let closed = (self.inner.seg_id, self.lsn, self.last_commit_ts);
        // `swap_to_fresh_segment` drops the rotated-out handle before
        // return: `recycle` later deletes that file, and Windows refuses
        // to delete a file that still has an open handle (os error 5).
        // A failed swap leaves this segment open: the short new file is
        // repaired on the next open, and `closed` stays unchanged.
        self.swap_to_fresh_segment(barrier)?;
        self.inner.closed.push(closed);
        self.commits = 0;
        Ok(())
    }

    /// Open the next segment id and make it the append target. The new
    /// segment starts with `MDBWAL02` while the store holds an ontology,
    /// `MDBWAL01` otherwise (ADR-0120 §4).
    fn swap_to_fresh_segment(&mut self, sync_magic: bool) -> Result<()> {
        let next_id = self.inner.seg_id.checked_add(1).ok_or_else(|| {
            self.poisoned = true;
            Error::storage("WAL segment ID overflow")
        })?;
        let magic: &[u8; 8] = if self.ontology_held {
            ONTOLOGY_MAGIC
        } else {
            MAGIC
        };
        let path = segment_path(&self.inner.prefix, next_id);
        let next = match write_magic(&*self.inner.vfs, &path, magic, sync_magic)
            .and_then(|()| open_append(&*self.inner.vfs, &path))
        {
            Ok(next) => next,
            Err(e) => {
                self.poisoned = true;
                return Err(e);
            }
        };
        drop(std::mem::replace(&mut self.inner.file, next));
        self.inner.seg_id = next_id;
        self.active_magic = *magic;
        self.bytes = MAGIC.len() as u64;
        if self.inner.vfs.durable() {
            self.synced = false;
        }
        Ok(())
    }

    fn rotate_inner(&mut self, barrier: bool) -> Result<u64> {
        self.ensure_writable()?;
        if self.commits == 0 {
            return Ok(self
                .inner
                .closed
                .last()
                .map(|(_, max_lsn, _)| *max_lsn)
                .unwrap_or(self.lsn));
        }
        self.close_active_segment(barrier)?;
        Ok(self
            .inner
            .closed
            .last()
            .map(|(_, max_lsn, _)| *max_lsn)
            .unwrap_or(self.lsn))
    }
}
