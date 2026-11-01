//! What a valid shard is (ADR-0183). The loader holds every manifest part to
//! these rules and the copy cursor holds its previous shard to them, each
//! over the part's declared id range, so a store that opens is one whose
//! parts a checkpoint can copy from.

use std::ops::Range;

use arrow::array::{Array, Int64Array};
use arrow::record_batch::RecordBatch;
use cypherlite_core::{Error, Result};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::reader::ChunkReader;

use crate::snapshot::Kind;

/// The rules every shard writer since v0.4.0 keeps: the shard schema of
/// its kind, non-null ids strictly ascending through the file, so no id
/// repeats, and every id inside the `[id_lo, id_hi)` the file's manifest
/// part declares. Checkpoint coalescing retires a part by that range, so a
/// row outside it would be lost or loaded twice. One value checks one file,
/// batch by batch.
pub(super) struct ShardRules {
    /// The part's declared id range.
    range: Range<i64>,
    /// The last id of the batches checked so far.
    last: Option<i64>,
}

fn broken(what: &str) -> Error {
    Error::storage(format!("{what} (ADR-0183)"))
}

/// The id column of `batch`, the first, when it is Int64.
pub(super) fn id_column(batch: &RecordBatch) -> Option<Int64Array> {
    batch
        .columns()
        .first()?
        .as_any()
        .downcast_ref::<Int64Array>()
        .cloned()
}

impl ShardRules {
    /// Check the schema of the file `builder` is about to read, whose
    /// manifest part declares `range`.
    pub(super) fn open<T: ChunkReader>(
        builder: &ParquetRecordBatchReaderBuilder<T>,
        kind: Kind,
        range: Range<i64>,
    ) -> Result<Self> {
        if builder.schema().fields() != kind.schema().fields() {
            return Err(broken("schema differs from the shard schema"));
        }
        Ok(Self { range, last: None })
    }

    /// Check the ids of the file's next batch, across batches too, and
    /// return them. They ascend, so the batch's first and last ids bound
    /// the rest within the range.
    pub(super) fn ids(&mut self, batch: &RecordBatch) -> Result<Int64Array> {
        let ids = id_column(batch).ok_or_else(|| broken("id column"))?;
        if ids.null_count() > 0 {
            return Err(broken("null id"));
        }
        if !self
            .last
            .iter()
            .chain(ids.values())
            .is_sorted_by(|a, b| a < b)
        {
            return Err(broken("ids are not strictly ascending"));
        }
        let ends = ids.values().first().into_iter().chain(ids.values().last());
        if let Some(outside) = ends.copied().find(|id| !self.range.contains(id)) {
            return Err(broken(&format!(
                "id {outside} is outside the part range [{}, {})",
                self.range.start, self.range.end
            )));
        }
        self.last = ids.values().last().copied().or(self.last);
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{load_part, write_bare_table, write_int_label_table};
    use super::super::{load_nodes, with_shard_writer, Origin, ShardCursor, MAX_FILE};
    use super::*;
    use cypherlite_executor::Graph;
    use std::sync::Arc;

    /// Copy `ids` out of node shard `nodes/p.parquet`, held as `bytes`, as
    /// a checkpoint copies from a part declaring `range`.
    fn copy(bytes: Vec<u8>, range: Range<i64>, ids: &[i64]) -> Result<()> {
        let cursor = ShardCursor::open_bytes("nodes/p.parquet", bytes, Kind::Node, range)?;
        with_shard_writer(Kind::Node, MAX_FILE, |writer| cursor.take(ids, writer))
            .map(drop)
            .map_err(Error::from)
    }

    #[test]
    fn parts_keep_the_shard_rules_and_monoliths_need_not() {
        // The loader and the copy cursor reject the same files, by one rule.
        for (bytes, rule) in [
            (
                write_bare_table(vec![0, 0, 1]).unwrap(),
                "not strictly ascending",
            ),
            (
                write_bare_table(vec![1, 0]).unwrap(),
                "not strictly ascending",
            ),
            (write_int_label_table(vec![0, 1]).unwrap(), "schema differs"),
        ] {
            let err = load_part(load_nodes, &Graph::default(), &bytes).unwrap_err();
            assert!(matches!(err, Error::Storage(_)), "{err}");
            assert!(err.to_string().contains(rule), "{err}");
            let err = copy(bytes, 0..i64::MAX, &[0, 1]).unwrap_err();
            assert!(err.to_string().contains("nodes/p.parquet"), "{err}");
            assert!(err.to_string().contains(rule), "{err}");
        }

        // A monolith keeps neither rule; its rows still load.
        for bytes in [
            write_bare_table(vec![1, 0]).unwrap(),
            write_int_label_table(vec![0, 1]).unwrap(),
        ] {
            let graph = Graph::default();
            load_nodes(&graph, bytes, Origin::Monolith).unwrap();
            assert!(graph.node_at(0, 1).is_some() && graph.node_at(1, 1).is_some());
        }
    }

    #[test]
    fn part_rows_must_lie_in_the_part_range() {
        // A first id below `[4, 8)` and a last id at its exclusive end: the
        // loader and the copy cursor reject both by the one range rule, the
        // cursor even when the rows it copies lie inside the range.
        for (ids, outside) in [(vec![3, 5], 3), (vec![4, 8], 8)] {
            let rule = format!("id {outside} is outside the part range [4, 8) (ADR-0183)");
            let bytes = write_bare_table(ids).unwrap();
            let err = load_nodes(&Graph::default(), bytes.clone(), Origin::Part(4..8)).unwrap_err();
            assert!(matches!(err, Error::Storage(_)), "{err}");
            assert!(err.to_string().contains(&rule), "{err}");
            let err = copy(bytes, 4..8, &[5]).unwrap_err();
            assert!(matches!(err, Error::Storage(_)), "{err}");
            let named = format!("previous Parquet shard nodes/p.parquet: {rule}");
            assert!(err.to_string().contains(&named), "{err}");
        }

        let bytes = write_bare_table(vec![4, 7]).unwrap();
        let graph = Graph::default();
        load_nodes(&graph, bytes.clone(), Origin::Part(4..8)).unwrap();
        assert!(graph.node_at(4, 1).is_some() && graph.node_at(7, 1).is_some());
        copy(bytes, 4..8, &[4, 7]).unwrap();
    }

    #[test]
    fn the_range_rule_holds_across_batches() {
        // Ids ascend across batches, so each batch's ends bound it; a later
        // batch that leaves the range fails once it is read.
        let mut rules = ShardRules {
            range: 0..10,
            last: None,
        };
        let batch = |ids: Vec<i64>| {
            let ids: Arc<dyn Array> = Arc::new(Int64Array::from(ids));
            RecordBatch::try_from_iter([("id", ids)]).unwrap()
        };
        assert_eq!(rules.ids(&batch(vec![0, 4])).unwrap().values(), &[0, 4]);
        assert_eq!(rules.ids(&batch(Vec::new())).unwrap().len(), 0);
        assert_eq!(rules.ids(&batch(vec![9])).unwrap().values(), &[9]);
        let err = rules.ids(&batch(vec![10])).unwrap_err();
        assert!(err.to_string().contains("id 10 is outside"), "{err}");
    }
}
