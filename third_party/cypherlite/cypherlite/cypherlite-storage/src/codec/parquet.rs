//! Parquet row codec for id-window shards (ADR-0022).

mod budget;
mod copy;
mod encode;
mod envelope;
#[cfg(test)]
mod fixtures;
mod rules;
use envelope::invalid;
pub(crate) use envelope::{preflight, VfsChunkReader};
use rules::{id_column, ShardRules};

pub(crate) use budget::DecodedBudget;
#[cfg(test)]
pub(crate) use budget::{decoded_preflight, take_measured_rows};
pub(crate) use copy::ShardCursor;
#[cfg(test)]
pub(crate) use encode::take_encoded_rows;
#[cfg(test)]
pub(crate) use encode::with_shard_writer;
pub(crate) use encode::{encode_rows, with_shard_writer_to, OutputLimit, ShardError, ShardWriter};
#[cfg(test)]
pub(crate) use fixtures::{write_bare_table, write_int_label_table, write_rel_table, write_table};

use std::collections::BTreeMap;
use std::ops::Range;

use arrow::array::{Array, Int64Array, ListArray, StringArray};
use arrow::record_batch::RecordBatch;
use bytes::Bytes;
use cypherlite_core::{Error, Node, Relationship, Result, Value};
use cypherlite_executor::{props_from_json, Graph};

use crate::snapshot::Kind;

// ADR-0086: these are per object, not per database. Keep the preflight ahead of
// Arrow's page reader: in parquet 54.3.1 it allocates from PageHeader before
// handing a page to a caller, even when the batch size is small.
pub(crate) const MAX_FILE: u64 = 1 << 30;
const MAX_METADATA: usize = 16 << 20;
const MAX_HEADER: usize = 64 << 10;
const MAX_PAGE: u64 = 128 << 20;
const MAX_PAGES: u64 = 65_536;
const MAX_ROWS: u64 = 1_000_000;
const MAX_ROW_BYTES: u64 = 384 << 20;
const MAX_DECODED: u64 = 2 << 30;

/// Rows held and flushed together (ADR-0180). The old 64-row prefix sample
/// used the same width; it is a row-group cap now, not a size estimate.
pub(crate) const ROW_GROUP_CAP: usize = 64;

/// Pages a row group writes per leaf column while its values fit one data
/// page: a dictionary page and a data page.
const PAGES_PER_LEAF: u64 = 2;

impl Kind {
    /// The most rows of this kind one file holds within ADR-0086's
    /// `MAX_PAGES` (ADR-0183, Page envelope): row groups of `ROW_GROUP_CAP`
    /// rows, each writing `PAGES_PER_LEAF` pages per leaf column. That is
    /// 699,008 node rows and 419,392 relationship rows; a file holds fewer
    /// when a large value spills into more data pages.
    pub(crate) fn page_envelope_rows(self) -> usize {
        let groups = MAX_PAGES / (PAGES_PER_LEAF * self.leaf_columns());
        groups as usize * ROW_GROUP_CAP
    }

    /// The row noun errors name: `node` or `relationship`.
    fn noun(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Rel => "relationship",
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_FILE_CEILING: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

pub(crate) fn file_ceiling() -> u64 {
    #[cfg(test)]
    if let Some(limit) = TEST_FILE_CEILING.get() {
        return limit;
    }
    MAX_FILE
}

#[cfg(test)]
pub(crate) fn with_test_file_ceiling<T>(limit: u64, f: impl FnOnce() -> T) -> T {
    with_test_override(&TEST_FILE_CEILING, limit, f)
}

/// Run `f` with the thread-local test override `cell` set to `value`, and
/// restore the override it replaced once `f` returns or panics, so
/// overrides of one cell nest.
#[cfg(test)]
fn with_test_override<V: 'static, T>(
    cell: &'static std::thread::LocalKey<std::cell::Cell<Option<V>>>,
    value: V,
    f: impl FnOnce() -> T,
) -> T {
    struct Restore<V: 'static> {
        cell: &'static std::thread::LocalKey<std::cell::Cell<Option<V>>>,
        previous: Option<V>,
    }
    impl<V: 'static> Drop for Restore<V> {
        fn drop(&mut self) {
            self.cell.set(self.previous.take());
        }
    }
    let _restore = Restore {
        cell,
        previous: cell.replace(Some(value)),
    };
    f()
}

#[derive(Clone, Copy)]
struct Envelope {
    page: u64,
    row: u64,
    decoded: u64,
}

fn envelope() -> Envelope {
    #[cfg(test)]
    if let Some(limits) = TEST_ENVELOPE.get() {
        return limits;
    }
    Envelope {
        page: MAX_PAGE,
        row: MAX_ROW_BYTES,
        decoded: MAX_DECODED,
    }
}

#[cfg(test)]
thread_local! {
    static TEST_ENVELOPE: std::cell::Cell<Option<Envelope>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_test_envelope<T>(page: u64, row: u64, decoded: u64, f: impl FnOnce() -> T) -> T {
    with_test_override(&TEST_ENVELOPE, Envelope { page, row, decoded }, f)
}

/// ADR-0086's checks on one decoded row, shared by the load side
/// ([`Decoded::row`]) and the checkpoint side ([`DecodedBudget`]): it must
/// fit the row envelope, and the decoded envelope by itself. The error names
/// the envelope the row misses.
fn check_row(bytes: u64) -> Result<()> {
    let limits = envelope();
    if bytes > limits.row {
        return Err(invalid("384 MiB decoded row"));
    }
    if bytes > limits.decoded {
        return Err(invalid("actual decoded bytes"));
    }
    Ok(())
}

/// Whether a row passes [`check_row`].
fn row_fits(bytes: u64) -> bool {
    check_row(bytes).is_ok()
}

fn bounded(value: i64, limit: u64, what: &str) -> Result<u64> {
    let n = u64::try_from(value).map_err(|_| invalid(what))?;
    if n > limit {
        return Err(invalid(what));
    }
    Ok(n)
}

fn accumulate(sum: &mut u64, value: u64, limit: u64, what: &str) -> Result<()> {
    *sum = sum
        .checked_add(value)
        .filter(|n| *n <= limit)
        .ok_or_else(|| invalid(what))?;
    Ok(())
}

/// Actual rows and decoded row bytes of one loaded file, held to the
/// ADR-0086 row and decoded envelopes across all of its batches.
#[derive(Default)]
struct Decoded {
    rows: u64,
    bytes: u64,
}

impl Decoded {
    fn batch(&mut self, rows: usize) -> Result<()> {
        accumulate(&mut self.rows, rows as u64, MAX_ROWS, "actual decoded rows")
    }

    fn row(&mut self, bytes: u64) -> Result<()> {
        check_row(bytes)?;
        accumulate(
            &mut self.bytes,
            bytes,
            envelope().decoded,
            "actual decoded bytes",
        )
    }
}

/// Preflighting entry point kept for tests that assert a direct encode
/// rejects a decoded-envelope overage before allocating.
#[cfg(test)]
pub(crate) fn nodes_parquet_ids(graph: &Graph, ts: u64, keys: &[i64]) -> Result<Vec<u8>> {
    encode_ids(graph, ts, keys, Kind::Node)
}

/// Relationship sibling of [`nodes_parquet_ids`], kept for direct-encode tests.
#[cfg(test)]
pub(crate) fn rels_parquet_ids(graph: &Graph, ts: u64, rids: &[i64]) -> Result<Vec<u8>> {
    encode_ids(graph, ts, rids, Kind::Rel)
}

#[cfg(test)]
fn encode_ids(graph: &Graph, ts: u64, ids: &[i64], kind: Kind) -> Result<Vec<u8>> {
    decoded_preflight(graph, ts, ids, kind)?;
    let (bytes, _) = with_shard_writer(kind, file_ceiling(), |writer| {
        encode_rows(writer, graph, ts, kind, ids)
    })?;
    Ok(bytes.into())
}

/// Loads one shard file into the graph ([`load_nodes`] or [`load_rels`]):
/// the file's bytes, and what wrote it, which decides the rules it is held
/// to.
pub(crate) type Loader = fn(&Graph, Vec<u8>, Origin) -> Result<()>;

/// What wrote a loaded file, which decides the rules it is held to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// A manifest part declaring id range `[id_lo, id_hi)`, held to
    /// [`ShardRules`] over that range as a checkpoint copying from it holds
    /// it (ADR-0183).
    Part(Range<i64>),
    /// A format ≤ 2 monolith (ADR-0005), not held to [`ShardRules`]: its
    /// writer predates v0.4.0, the oldest known to write the shard schema
    /// and ids ascending and unique within a file, so its schema and row
    /// order are unknown. It declares no range, and keeps the loader's
    /// column-type, persisted-id and properties-object checks. No checkpoint
    /// copies from it.
    Monolith,
}

/// Read one loaded file's batches into `load` with their id column:
/// preflighted, counted against the ADR-0086 row envelope, and for a
/// manifest part held to [`ShardRules`], which check the ids it is handed.
fn load_batches(
    bytes: Vec<u8>,
    kind: Kind,
    origin: Origin,
    mut load: impl FnMut(&RecordBatch, &Int64Array, &mut Decoded) -> Result<()>,
) -> Result<()> {
    let builder = preflight(Bytes::from(bytes))?;
    let mut rules = match origin {
        Origin::Part(range) => Some(ShardRules::open(&builder, kind, range)?),
        Origin::Monolith => None,
    };
    let reader = builder.build().map_err(|e| Error::storage(e.to_string()))?;
    let mut decoded = Decoded::default();
    for batch in reader {
        let batch = batch.map_err(|e| Error::storage(e.to_string()))?;
        decoded.batch(batch.num_rows())?;
        let ids = match &mut rules {
            Some(rules) => rules.ids(&batch)?,
            None => id_column(&batch).ok_or_else(|| Error::storage("id column"))?,
        };
        load(&batch, &ids, &mut decoded)?;
    }
    Ok(())
}

/// A shard's properties column: one tagged-JSON object per row.
struct Properties<'a>(&'a StringArray);

impl<'a> Properties<'a> {
    fn column(batch: &'a RecordBatch, index: usize) -> Result<Self> {
        let props = batch
            .column(index)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| Error::storage("properties column"))?;
        Ok(Self(props))
    }

    /// Row `i`'s JSON text, or `None` when the row has no properties.
    fn text(&self, i: usize) -> Option<&'a str> {
        (!self.0.is_null(i)).then(|| self.0.value(i))
    }

    /// Row `i`'s properties, decoded as the WAL decodes a property map: JSON
    /// other than an object is an error.
    fn decode(&self, i: usize) -> Result<BTreeMap<String, Value>> {
        let Some(text) = self.text(i) else {
            return Ok(BTreeMap::new());
        };
        let json: serde_json::Value =
            serde_json::from_str(text).map_err(|e| Error::storage(e.to_string()))?;
        props_from_json(&json)
    }
}

pub(crate) fn load_nodes(graph: &Graph, bytes: Vec<u8>, origin: Origin) -> Result<()> {
    load_batches(bytes, Kind::Node, origin, |batch, ids, decoded| {
        if batch.num_columns() != 3 {
            return Err(Error::storage("node shard: expected 3 columns"));
        }
        let labels = batch
            .column(1)
            .as_any()
            .downcast_ref::<ListArray>()
            .ok_or_else(|| Error::storage("labels column"))?;
        let props = Properties::column(batch, 2)?;
        for i in 0..batch.num_rows() {
            let mut row_bytes = encode::fixed_column_bytes(Kind::Node);
            let mut row_labels = Vec::new();
            if !labels.is_null(i) {
                let arr = labels.value(i);
                let s = arr
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| Error::storage("label items"))?;
                for j in 0..s.len() {
                    if !s.is_null(j) {
                        let label = s.value(j);
                        accumulate(
                            &mut row_bytes,
                            label.len() as u64,
                            envelope().row.saturating_add(1),
                            "node row bytes",
                        )?;
                        row_labels.push(label.to_string());
                    }
                }
            }
            if let Some(text) = props.text(i) {
                accumulate(
                    &mut row_bytes,
                    text.len() as u64,
                    envelope().row.saturating_add(1),
                    "node row bytes",
                )?;
            }
            decoded.row(row_bytes)?;
            let id = ids.value(i);
            cypherlite_core::validate_persisted_id(id, "node ID")?;
            graph.load_checkpoint_node(Node {
                id,
                labels: std::sync::Arc::new(row_labels),
                properties: cypherlite_core::PropertyMap::share(props.decode(i)?),
            })?;
        }
        Ok(())
    })
}

pub(crate) fn load_rels(graph: &Graph, bytes: Vec<u8>, origin: Origin) -> Result<()> {
    load_batches(bytes, Kind::Rel, origin, |batch, ids, decoded| {
        if batch.num_columns() != 5 {
            return Err(Error::storage("relationship shard: expected 5 columns"));
        }
        let starts = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| Error::storage("start_id column"))?;
        let ends = batch
            .column(2)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| Error::storage("end_id column"))?;
        let types = batch
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| Error::storage("type column"))?;
        let props = Properties::column(batch, 4)?;
        for i in 0..batch.num_rows() {
            let mut row_bytes = encode::fixed_column_bytes(Kind::Rel);
            if !types.is_null(i) {
                accumulate(
                    &mut row_bytes,
                    types.value(i).len() as u64,
                    envelope().row.saturating_add(1),
                    "relationship row bytes",
                )?;
            }
            if let Some(text) = props.text(i) {
                accumulate(
                    &mut row_bytes,
                    text.len() as u64,
                    envelope().row.saturating_add(1),
                    "relationship row bytes",
                )?;
            }
            decoded.row(row_bytes)?;
            let id = ids.value(i);
            let start = starts.value(i);
            let end = ends.value(i);
            cypherlite_core::validate_persisted_id(id, "relationship ID")?;
            cypherlite_core::validate_persisted_id(start, "relationship start ID")?;
            cypherlite_core::validate_persisted_id(end, "relationship end ID")?;
            let rel_type = if types.is_null(i) {
                String::new()
            } else {
                types.value(i).to_string()
            };
            graph.load_checkpoint_rel(Relationship {
                id,
                start,
                end,
                rel_type,
                properties: cypherlite_core::PropertyMap::share(props.decode(i)?),
            })?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests;
