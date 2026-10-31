//! Id-window Parquet shards (ADR-0022).
//!
//! Always compiled: `ShardPart` and the small VFS helpers used by both the
//! Parquet and JSON codecs. The Parquet shard writer/reader and its sizing
//! helpers live in `native` behind one cfg — `arrow`/`parquet` don't build
//! on wasm32 and are optional elsewhere (ADR-0105).

use cypherlite_core::{Error, Result};
use cypherlite_executor::Graph;
use serde::{Deserialize, Serialize};

use crate::vfs::{atomic_write, read_to_vec, VfsOpen};
use crate::StoreTarget;

#[cfg(parquet_codec)]
pub(crate) use native::{
    checkpoint_at, load_with_checkpoint_lsn, snapshot_data_bytes, sweep_orphans,
};
#[cfg(not(parquet_codec))]
pub(crate) use unavailable::{
    checkpoint_at, load_with_checkpoint_lsn, snapshot_data_bytes, sweep_orphans,
};

const MAX_JSON_OBJECT: u64 = 16 * 1024 * 1024; // ADR-0086, per metadata object

/// The manifest object name: the one size-bounded JSON metadata object the
/// generic object helpers special-case. Read, write, and accounting paths
/// all resolve the policy through [`bounded_read_for`] so the special case
/// is stated once.
pub(crate) const MANIFEST_JSON: &str = "manifest.json";

/// The directory of the immutable ontology document objects
/// `ontology/<128-bit-hex>.json` (ADR-0120 §5). Only the Parquet codec
/// references the family by directory (the JSON codec inlines the
/// document), so the name is dead without it.
#[cfg_attr(not(parquet_codec), allow(dead_code))]
pub(crate) const ONTOLOGY_DIR: &str = "ontology";

/// Whether `name` is exactly one ontology document object,
/// `ontology/<32 lowercase hex>.json` (ADR-0120 §5). The one recognizer the
/// bounded-read class, the deletion allowlist and the orphan sweep share.
pub(crate) fn is_ontology_object_name(name: &str) -> bool {
    let Some(file) = name.strip_prefix("ontology/") else {
        return false;
    };
    let Some(hex) = file.strip_suffix(".json") else {
        return false;
    };
    crate::vfs::is_lower_hex_32(hex)
}

/// What one snapshot load decodes, named instead of positional (ADR-0120,
/// ADR-0127 Decision 2): the graph, the checkpoint LSN (0 when the store
/// has no manifest yet), the ontology document the snapshot carries
/// (`None` when the store holds none), and the version marker the codec
/// loader classified. Both codecs and the no-parquet stub return it.
pub(crate) struct LoadedSnapshot {
    pub(crate) graph: Graph,
    pub(crate) checkpoint_lsn: u64,
    pub(crate) ontology: Option<String>,
    pub(crate) marker: crate::version::SnapshotMarker,
}

impl LoadedSnapshot {
    /// The empty-store load every codec's no-snapshot path returns (no
    /// manifest / no `graph.json` object): a default graph, no ontology
    /// document, a current marker, and the checkpoint LSN the caller's one
    /// metadata read already reported (0 when the store has no manifest
    /// yet).
    pub(crate) fn empty(checkpoint_lsn: u64) -> Self {
        Self {
            graph: Graph::default(),
            checkpoint_lsn,
            ontology: None,
            marker: crate::version::SnapshotMarker::Current,
        }
    }
}

/// The persisted object families the store directory grammar recognizes
/// (ADR-0086/ADR-0089 shard objects, ADR-0120 §5 ontology documents, the
/// manifest). The bounded-read policy here and the Parquet codec's
/// deletion allowlist and orphan sweeps all classify through
/// [`object_name`], so a new family forces a decision at every consumer.
#[derive(Debug)]
pub(crate) enum ObjectName {
    /// The `manifest.json` metadata object.
    Manifest,
    /// An immutable `ontology/<128-bit-hex>.json` document object.
    Ontology,
    /// A retired pre-shard monolith object (`nodes.parquet`, …).
    LegacyShard,
    /// A v3 shard object, parsed.
    #[cfg(parquet_codec)]
    Shard(native::ShardName),
}

/// Classify a store object name into its family; `None` is no known
/// family. The one name grammar the bounded-read policy, the deletion
/// allowlist and the orphan sweeps share.
pub(crate) fn object_name(name: &str) -> Option<ObjectName> {
    if name == MANIFEST_JSON {
        return Some(ObjectName::Manifest);
    }
    if is_ontology_object_name(name) {
        return Some(ObjectName::Ontology);
    }
    if Kind::ALL
        .into_iter()
        .any(|kind| kind.legacy_names().contains(&name))
    {
        return Some(ObjectName::LegacyShard);
    }
    #[cfg(parquet_codec)]
    if let Some(shard) = native::ShardName::parse(name) {
        return Some(ObjectName::Shard(shard));
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardPart {
    pub path: String,
    pub id_lo: i64,
    pub id_hi: i64,
    pub rows: u64,
    pub bytes: u64,
}

/// Shard kind: the node/rel vocabulary (directories, legacy monolith
/// object names) shared by the codec probe here and the Parquet writer in
/// `native` — compiled unconditionally so the probe does not depend on the
/// Parquet codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Node,
    Rel,
}

impl Kind {
    pub(crate) const ALL: [Kind; 2] = [Kind::Node, Kind::Rel];

    pub(crate) fn dir(self) -> &'static str {
        match self {
            Self::Node => "nodes",
            Self::Rel => "relationships",
        }
    }

    /// Pre-shard monolith object names for this kind (object + its `.tmp`),
    /// retired when the kind's first sharded manifest publishes. The one
    /// home of the legacy vocabulary: the codec probe, the retirement
    /// producer, and the deletion allowlist all draw on it.
    pub(crate) fn legacy_names(self) -> [&'static str; 2] {
        match self {
            Self::Node => ["nodes.parquet", "nodes.parquet.tmp"],
            Self::Rel => ["relationships.parquet", "relationships.parquet.tmp"],
        }
    }
}
/// Read one required object, failing with `missing {name}` when it is
/// absent.
pub(crate) fn get_object(target: &StoreTarget, name: &str) -> Result<Vec<u8>> {
    get_object_opt(target, name)?.ok_or_else(|| Error::storage(format!("missing {name}")))
}
/// Size class for [`read_bounded_object`]: each persisted object family has
/// its own ceiling and diagnostic wording.
pub(crate) struct BoundedRead {
    max_len: u64,
    too_large: fn(&str) -> Error,
    changed: fn(&str) -> Error,
}

const JSON_META: BoundedRead = BoundedRead {
    max_len: MAX_JSON_OBJECT,
    too_large: |name| {
        Error::storage(format!(
            "{name} exceeds 16 MiB (16777216 bytes) metadata limit"
        ))
    },
    changed: |name| Error::storage(format!("{name} changed length during read")),
};

/// Ontology documents share the ADR-0086 object envelope (ADR-0120 §3:
/// 16 MiB, checked before the commit is written).
const ONTOLOGY_DOC: BoundedRead = BoundedRead {
    max_len: MAX_JSON_OBJECT,
    too_large: |name| {
        Error::storage(format!(
            "{name} exceeds 16 MiB (16777216 bytes) ontology document limit"
        ))
    },
    changed: |name| Error::storage(format!("{name} changed length during read")),
};

/// The size class of an object name: the manifest is bounded JSON metadata
/// (ADR-0086), an `ontology/<128-bit-hex>.json` object is a bounded
/// ontology document (ADR-0120 §5); every other object is unbounded
/// payload. The one lookup the read, write, and byte-accounting paths all
/// consult.
pub(crate) fn bounded_read_for(name: &str) -> Option<&'static BoundedRead> {
    match object_name(name) {
        Some(ObjectName::Manifest) => Some(&JSON_META),
        Some(ObjectName::Ontology) => Some(&ONTOLOGY_DOC),
        _ => None,
    }
}

/// Enforce an object's size class on a known length (no I/O).
pub(crate) fn check_bounded_len(name: &str, len: u64) -> Result<()> {
    if let Some(kind) = bounded_read_for(name) {
        check_bounded_len_with(name, len, kind)?;
    }
    Ok(())
}

/// The metadata-then-opened-handle length proof of [`read_bounded_object`],
/// without the read: returns the proven length and the open handle. The VFS
/// is untrusted, so a lying metadata or handle fails before allocation.
pub(crate) fn checked_object_len(
    target: &StoreTarget,
    name: &str,
    kind: &BoundedRead,
) -> Result<(u64, Box<dyn crate::vfs::VfsFile>)> {
    check_bounded_len_with(
        name,
        target
            .vfs
            .metadata(name)
            .map_err(|e| Error::storage(e.to_string()))?
            .len,
        kind,
    )?;
    let file = target
        .vfs
        .open(name, VfsOpen::read_only())
        .map_err(|e| Error::storage(e.to_string()))?;
    let len = file.len().map_err(|e| Error::storage(e.to_string()))?;
    check_bounded_len_with(name, len, kind)?;
    Ok((len, file))
}

fn check_bounded_len_with(name: &str, len: u64, kind: &BoundedRead) -> Result<()> {
    if len > kind.max_len {
        return Err((kind.too_large)(name));
    }
    Ok(())
}

/// Bounded read shared by Parquet shards and metadata objects: prove the
/// declared length from `metadata`, re-prove it on the opened handle, read
/// exactly that many bytes, then require the length did not change. The VFS is
/// untrusted, so a lying metadata or handle must fail before allocation or
/// after the bytes are consumed.
fn read_bounded_object(
    target: &StoreTarget,
    name: &str,
    kind: &BoundedRead,
) -> Result<Option<Vec<u8>>> {
    if !target
        .vfs
        .exists(name)
        .map_err(|e| Error::storage(e.to_string()))?
    {
        return Ok(None);
    }
    let (len, mut file) = checked_object_len(target, name, kind)?;
    let mut bytes = vec![0; len as usize];
    if !bytes.is_empty() {
        file.read_exact(&mut bytes)
            .map_err(|e| Error::storage(e.to_string()))?;
    }
    // Re-prove after the read: a length grown past the cap is the too-large
    // error, any other change is the changed-length error.
    let after = file.len().map_err(|e| Error::storage(e.to_string()))?;
    check_bounded_len_with(name, after, kind)?;
    if after != len {
        return Err((kind.changed)(name));
    }
    Ok(Some(bytes))
}

/// The one Option-returning object read, shared by [`get_object`] and the
/// manifest stamp of the read-only open (ADR-0136): `None` when the object
/// is absent — one read, no exists-then-get pair — with the bounded-read
/// class of [`bounded_read_for`] enforced for metadata objects.
pub(crate) fn get_object_opt(target: &StoreTarget, name: &str) -> Result<Option<Vec<u8>>> {
    // The codec resolver also uses get_object; metadata objects must be
    // bounded on both the metadata and the opened handle, even when the VFS
    // lies.
    if let Some(kind) = bounded_read_for(name) {
        return read_bounded_object(target, name, kind);
    }
    if !target
        .vfs
        .exists(name)
        .map_err(|e| Error::storage(e.to_string()))?
    {
        return Ok(None);
    }
    read_to_vec(&*target.vfs, name)
        .map(Some)
        .map_err(|e| Error::storage(e.to_string()))
}

/// Existence probe that propagates VFS errors: on a load path an IO failure
/// must fail the open, not read as "object absent".
pub(crate) fn object_exists(target: &StoreTarget, name: &str) -> Result<bool> {
    target
        .vfs
        .exists(name)
        .map_err(|e| Error::storage(e.to_string()))
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn has_parquet_store_files(target: &StoreTarget) -> Result<bool> {
    for kind in Kind::ALL {
        if object_exists(target, kind.legacy_names()[0])? {
            return Ok(true);
        }
    }
    for kind in Kind::ALL {
        match target.vfs.metadata(kind.dir()) {
            Ok(meta) => {
                if meta.is_dir {
                    return Ok(true);
                }
            }
            // Absent is not a Parquet store; a metadata failure on an
            // object that does exist surfaces instead of reading as absent.
            Err(e) => {
                if target
                    .vfs
                    .exists(kind.dir())
                    .map_err(|e| Error::storage(e.to_string()))?
                {
                    return Err(Error::storage(e.to_string()));
                }
            }
        }
    }
    Ok(false)
}
pub(crate) fn put_object(target: &StoreTarget, name: &str, bytes: &[u8]) -> Result<()> {
    check_bounded_len(name, bytes.len() as u64)?;
    atomic_write(&*target.vfs, name, bytes)
}

// arrow/parquet don't build on wasm32 and are optional elsewhere (ADR-0105),
// so the whole Parquet shard writer/reader lives in the `native` directory
// module behind this cfg; everything above is always compiled.
#[cfg(parquet_codec)]
pub(crate) mod native;

// Without the Parquet codec the matches in `lib.rs` stay cfg-free. Every
// `StoreTarget` constructor rejects a Parquet codec that is not compiled in
// (`codec::check_available`), so these stubs are unreachable backstops.
#[cfg(not(parquet_codec))]
mod unavailable {
    use super::*;
    use crate::codec::parquet_unavailable;

    pub fn checkpoint_at(
        _: &StoreTarget,
        _: &Graph,
        _: u64,
        _: u64,
        _: Option<&str>,
    ) -> Result<()> {
        Err(parquet_unavailable())
    }

    pub fn load_with_checkpoint_lsn(_: &StoreTarget) -> Result<LoadedSnapshot> {
        Err(parquet_unavailable())
    }

    pub fn snapshot_data_bytes(_: &StoreTarget) -> Result<u64> {
        Err(parquet_unavailable())
    }

    pub fn sweep_orphans(_: &StoreTarget) -> Result<()> {
        Err(parquet_unavailable())
    }
}
