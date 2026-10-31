//! Shard files for tests, malformed ones included, a part loader and a
//! commit helper. A file of the shard schema is written as a checkpoint
//! writes it: over [`Kind::schema`] through [`with_shard_writer`].

use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Array, Int64Builder, ListBuilder, StringArray, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use cypherlite_core::{Error, Result};
use cypherlite_executor::Graph;
use parquet::arrow::ArrowWriter;

use crate::snapshot::Kind;

use super::{file_ceiling, with_shard_writer, Loader, Origin};

/// Load `bytes` with `loader` as a manifest part spanning every persisted id.
pub(super) fn load_part(loader: Loader, graph: &Graph, bytes: &[u8]) -> Result<()> {
    loader(graph, bytes.to_vec(), Origin::Part(0..i64::MAX))
}

/// Finish an open write and return the commit timestamp. The redo is moved
/// once, so callers do not clone it.
pub(super) fn commit_ops(g: &Graph) -> u64 {
    let ops = g.take_redo();
    let ts = g.pending_commit_ts();
    g.abort_write();
    g.apply_redo(ops, ts).unwrap();
    ts
}

/// A file of `kind`'s shard schema holding one batch of `columns`.
fn encode_batch(kind: Kind, columns: Vec<ArrayRef>) -> Result<Vec<u8>> {
    let batch =
        RecordBatch::try_new(kind.schema(), columns).map_err(|e| Error::storage(e.to_string()))?;
    let (bytes, ()) = with_shard_writer(kind, file_ceiling(), |writer| writer.write_rows(&batch))?;
    Ok(bytes.into())
}

/// A node file holding rows of `ids`, `labels` and `props`, in that order;
/// `None` is a null.
pub(crate) fn write_table(
    ids: Vec<i64>,
    labels: Vec<Option<Vec<Option<String>>>>,
    props: Vec<Option<String>>,
) -> Result<Vec<u8>> {
    let mut list_builder = ListBuilder::new(StringBuilder::new());
    for lab in labels {
        match lab {
            None => list_builder.append(false),
            Some(items) => {
                for it in items {
                    list_builder.values().append_option(it);
                }
                list_builder.append(true);
            }
        }
    }
    encode_batch(
        Kind::Node,
        vec![
            Arc::new(Int64Array::from(ids)),
            Arc::new(list_builder.finish()),
            Arc::new(StringArray::from(props)),
        ],
    )
}

/// A relationship file holding rows of the given columns, in that order.
pub(crate) fn write_rel_table(
    ids: Vec<i64>,
    starts: Vec<i64>,
    ends: Vec<i64>,
    types: Vec<Option<String>>,
    props: Vec<Option<String>>,
) -> Result<Vec<u8>> {
    encode_batch(
        Kind::Rel,
        vec![
            Arc::new(Int64Array::from(ids)),
            Arc::new(Int64Array::from(starts)),
            Arc::new(Int64Array::from(ends)),
            Arc::new(StringArray::from(types)),
            Arc::new(StringArray::from(props)),
        ],
    )
}

/// A node file holding `ids`, in that order, with no labels or properties.
pub(crate) fn write_bare_table(ids: Vec<i64>) -> Result<Vec<u8>> {
    let n = ids.len();
    write_table(ids, vec![Some(Vec::new()); n], vec![Some("{}".into()); n])
}

/// A node file whose `labels` column is `List<Int64>`, every value null:
/// the loader's column types accept it, the shard schema does not, so it is
/// written by a plain `ArrowWriter`.
pub(crate) fn write_int_label_table(ids: Vec<i64>) -> Result<Vec<u8>> {
    let item = Field::new("item", DataType::Int64, true);
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("labels", DataType::List(Arc::new(item)), true),
        Field::new("properties", DataType::Utf8, true),
    ]));
    let mut labels = ListBuilder::new(Int64Builder::new());
    for _ in &ids {
        labels.append(false);
    }
    let props = StringArray::from(vec!["{}"; ids.len()]);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(ids)),
            Arc::new(labels.finish()),
            Arc::new(props),
        ],
    )
    .map_err(|e| Error::storage(e.to_string()))?;
    let mut bytes = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut bytes, schema, None)
        .map_err(|e| Error::storage(e.to_string()))?;
    writer
        .write(&batch)
        .map_err(|e| Error::storage(e.to_string()))?;
    writer.close().map_err(|e| Error::storage(e.to_string()))?;
    Ok(bytes)
}
