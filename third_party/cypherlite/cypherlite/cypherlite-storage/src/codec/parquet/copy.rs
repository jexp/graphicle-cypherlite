//! The copy cursor over one previous shard (ADR-0183). A rewrite copies
//! clean rows out of the previous shard's Arrow batches into the file being
//! written; they are not loaded from the live graph or re-encoded.

use std::ops::Range;
use std::sync::Arc;

use arrow::array::Int64Array;
use arrow::record_batch::RecordBatch;
use cypherlite_core::{Error, Result};
use parquet::arrow::arrow_reader::ParquetRecordBatchReader;

use crate::snapshot::Kind;
use crate::storage_context;
use crate::vfs::Vfs;

use super::encode::{ShardError, ShardWriter};
use super::{preflight, ShardRules, VfsChunkReader, ROW_GROUP_CAP};

/// A forward cursor over one previous shard's rows, in ascending id order.
/// Every failure to read the shard is fatal: fewer rows do not avoid it.
pub(crate) struct ShardCursor {
    /// The shard's path, which every failure names.
    path: String,
    source: VfsChunkReader,
    reader: ParquetRecordBatchReader,
    rules: ShardRules,
    held: Option<Held>,
}

/// The batch being read and its id column, downcast once. Rows from `next`
/// on are still ahead.
struct Held {
    batch: RecordBatch,
    ids: Int64Array,
    next: usize,
}

/// The one shape of a failure to read previous shard `path`: `error`,
/// naming the shard.
fn source_error(path: &str, error: Error) -> Error {
    storage_context(error, format_args!("previous Parquet shard {path}"))
}

impl ShardCursor {
    /// Open previous shard `path` through bounded VFS ranges, of the manifest
    /// part that declares id range `range`. It is held to
    /// the shard rules over that range, as the loader holds the part: its
    /// schema here, and its ids as each batch is read. The writer would
    /// reject or mislabel other columns, a repeated id would be skipped, and
    /// a row outside the range belongs to another part.
    pub(crate) fn open(
        vfs: Arc<dyn Vfs>,
        path: &str,
        kind: Kind,
        range: Range<i64>,
    ) -> Result<Self> {
        let fail = |error| source_error(path, error);
        let source = VfsChunkReader::open(vfs, path, super::MAX_FILE).map_err(fail)?;
        let builder = preflight(source.clone()).map_err(fail)?;
        let rules = ShardRules::open(&builder, kind, range).map_err(fail)?;
        let reader = builder
            .with_batch_size(ROW_GROUP_CAP)
            .build()
            .map_err(|e| fail(Error::storage(e.to_string())))?;
        Ok(Self {
            path: path.to_owned(),
            source,
            reader,
            rules,
            held: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn open_bytes(
        path: &str,
        bytes: Vec<u8>,
        kind: Kind,
        range: Range<i64>,
    ) -> Result<Self> {
        use crate::vfs::{MemoryVfs, VfsOpen};

        let vfs = Arc::new(MemoryVfs::new());
        let mut file = vfs.open(path, VfsOpen::create_new())?;
        file.write_all(&bytes)?;
        Self::open(vfs, path, kind, range)
    }

    /// Copy the rows of ascending `ids` into `writer`, passing the rows
    /// between them, and return the cursor past them for a later run. Fails
    /// at the first id the shard does not hold ahead of the cursor, rows it
    /// passed included, and the failed cursor is gone.
    pub(crate) fn take<W: std::io::Write + Send + super::OutputLimit>(
        mut self,
        ids: &[i64],
        writer: &mut ShardWriter<W>,
    ) -> std::result::Result<Self, ShardError> {
        let mut rest = ids;
        while let Some(&want) = rest.first() {
            let Some(held) = self.ahead()? else {
                return Err(self.missing(want).into());
            };
            let ahead = &held.ids.values()[held.next..];
            let skip = ahead.partition_point(|id| *id < want);
            if skip == ahead.len() {
                // Every row left in the batch is behind `want`.
                held.next = held.ids.len();
                continue;
            }
            let run = ahead[skip..]
                .iter()
                .zip(rest)
                .take_while(|(row, id)| row == id)
                .count();
            if run == 0 {
                return Err(self.missing(want).into());
            }
            let start = held.next + skip;
            held.next = start + run;
            writer.write_rows(&held.batch.slice(start, run))?;
            rest = &rest[run..];
        }
        self.source
            .verify_len()
            .map_err(|error| source_error(&self.path, error))?;
        Ok(self)
    }

    fn missing(&self, want: i64) -> Error {
        source_error(
            &self.path,
            Error::storage(format!("clean checkpoint row {want} missing")),
        )
    }

    /// The held batch with rows still ahead, reading the next batch once it
    /// is used up; `None` at the end of the shard. Each batch is held to the
    /// shard rules once, when it is read.
    fn ahead(&mut self) -> Result<Option<&mut Held>> {
        while self
            .held
            .as_ref()
            .is_none_or(|held| held.next == held.ids.len())
        {
            let Some(batch) = self.reader.next() else {
                return Ok(None);
            };
            let batch =
                batch.map_err(|e| source_error(&self.path, Error::storage(e.to_string())))?;
            let ids = self
                .rules
                .ids(&batch)
                .map_err(|e| source_error(&self.path, e))?;
            self.held = Some(Held {
                batch,
                ids,
                next: 0,
            });
        }
        Ok(self.held.as_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{load_part, write_bare_table};
    use super::super::{load_nodes, with_shard_writer, MAX_FILE};
    use super::*;
    use crate::vfs::{MemoryVfs, VfsOpen};
    use bytes::Bytes;
    use cypherlite_executor::Graph;

    /// A cursor over node shard `nodes/c.parquet` holding bare rows `ids`.
    fn cursor(ids: Vec<i64>) -> ShardCursor {
        let bytes = write_bare_table(ids).unwrap();
        let vfs = Arc::new(MemoryVfs::new());
        let mut file = vfs.open("nodes/c.parquet", VfsOpen::create_new()).unwrap();
        file.write_all(&bytes).unwrap();
        ShardCursor::open(vfs, "nodes/c.parquet", Kind::Node, 0..200).unwrap()
    }

    /// Copy each of `runs` in turn through one cursor over `cursor`'s rows.
    fn take_runs(cursor: ShardCursor, runs: &[&[i64]]) -> Result<Bytes> {
        let (bytes, _) = with_shard_writer(Kind::Node, MAX_FILE, |writer| {
            runs.iter()
                .try_fold(cursor, |cursor, run| cursor.take(run, writer))
        })?;
        Ok(bytes)
    }

    #[test]
    fn cursor_is_handed_back_past_each_run() {
        let ids: Vec<i64> = (0..200).collect();
        // A run that starts in a later batch passes the rows between.
        let bytes = take_runs(cursor(ids.clone()), &[&[2, 3, 70], &[150, 199]]).unwrap();
        let copied = Graph::default();
        load_part(load_nodes, &copied, &bytes).unwrap();
        for id in ids {
            assert_eq!(
                copied.node_at(id, 1).is_some(),
                [2, 3, 70, 150, 199].contains(&id)
            );
        }
    }

    #[test]
    fn rows_the_cursor_lacks_or_passed_are_missing() {
        // A zero-length run is a missing row, named with the shard; so is a
        // run behind the cursor, which only reads forward.
        for (held, runs, missing) in [
            (vec![0, 2], &[&[0, 1][..]][..], 1),
            ((0..200).collect(), &[&[70][..], &[3][..]][..], 3),
        ] {
            let err = take_runs(cursor(held), runs).unwrap_err();
            let named = format!(
                "previous Parquet shard nodes/c.parquet: clean checkpoint row {missing} missing"
            );
            assert!(err.to_string().contains(&named), "{err}");
        }
    }

    #[test]
    fn rejected_copy_source_is_named_once() {
        // The preflight's error names no shard; the cursor names it, once.
        let vfs = Arc::new(MemoryVfs::new());
        let mut file = vfs.open("nodes/g.parquet", VfsOpen::create_new()).unwrap();
        file.write_all(b"not parquet at all").unwrap();
        let err = ShardCursor::open(vfs, "nodes/g.parquet", Kind::Node, 0..4)
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            "storage error: previous Parquet shard nodes/g.parquet: \
             invalid or out-of-bounds footer magic (ADR-0086)"
        );
    }
}
