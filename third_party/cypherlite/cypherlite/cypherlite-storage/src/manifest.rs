//! The checkpoint manifest (ADR-0086): the one bounded JSON metadata object
//! every snapshot publishes last. The struct and its field decoders live
//! here; the Parquet codec's manifest *validation* stays with the codec.

use serde::{de::Error as _, Deserialize, Deserializer, Serialize};

use crate::snapshot;
use crate::SnapshotCodecKind;

/// Checkpoint metadata. A v4 manifest may select one immutable
/// `ontology/<128-bit-hex>.json` object holding the store's ontology
/// document (ADR-0120 §5).
///
/// The legacy ADR-0092 `pull_identity_path` selector is gone (ADR-0158:
/// never written or read anymore). A key still present in an old manifest
/// is ignored on read — unknown JSON fields are ignored, so old manifests
/// open unchanged — and dropped on the next write, which changes nothing
/// on disk because no manifest this code wrote ever carried it.
///
/// # Example
/// ```
/// use cypherlite_storage::{Manifest, SnapshotCodecKind};
/// let legacy: Manifest = serde_json::from_str(
///     r#"{"format_version":3,"next_node_id":0,"next_rel_id":0}"#
/// ).unwrap();
/// assert!(legacy.ontology_path.is_none());
/// assert_eq!(legacy.codec, SnapshotCodecKind::Parquet);
/// // The ADR-0092 selector is gone: an old manifest's key is ignored.
/// let old: Manifest = serde_json::from_str(
///     r#"{"format_version":3,"next_node_id":0,"next_rel_id":0,
///         "pull_identity_path":"pull-identity/0123456789abcdef0123456789abcdef.json"}"#
/// ).unwrap();
/// assert_eq!(old.format_version, 3);
/// assert!(old.ontology_path.is_none());
/// let with_ontology: Manifest = serde_json::from_str(
///     r#"{"format_version":4,"next_node_id":0,"next_rel_id":0,
///         "ontology_path":"ontology/0123456789abcdef0123456789abcdef.json"}"#
/// ).unwrap();
/// assert!(with_ontology.ontology_path.is_some());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub next_node_id: i64,
    pub next_rel_id: i64,
    #[serde(default)]
    pub checkpoint_lsn: u64,
    #[serde(default)]
    pub commit_ts: u64,
    #[serde(default)]
    pub node_parts: Vec<snapshot::ShardPart>,
    #[serde(default)]
    pub rel_parts: Vec<snapshot::ShardPart>,
    #[serde(default)]
    pub bytes_per_node: f64,
    #[serde(default)]
    pub bytes_per_rel: f64,
    /// Snapshot codec. Absent on pre-ADR-0050 directories means parquet.
    #[serde(default = "SnapshotCodecKind::absent")]
    pub codec: SnapshotCodecKind,
    /// Named `(label, property)` catalog entries (ADR-0040).
    #[serde(default)]
    pub indexes: Vec<cypherlite_catalog::IndexDef>,
    /// Constraint catalog entries enforced after recovery.
    #[serde(default)]
    pub constraints: Vec<cypherlite_catalog::ConstraintDef>,
    /// Typed SHOW metadata at this checkpoint cut (ADR-0095). Absent in
    /// earlier manifests; the historical empty theater is used then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_checkpoint_theater")]
    pub theater: Option<cypherlite_executor::CheckpointTheater>,
    /// The store's ontology document object, an immutable
    /// `ontology/<128-bit-hex>.json` (ADR-0120 §5). Written only at
    /// `format_version` 4; absent means the store holds no ontology.
    #[serde(
        default,
        deserialize_with = "deserialize_nonempty_selector",
        skip_serializing_if = "Option::is_none"
    )]
    pub ontology_path: Option<String>,
}

pub(crate) fn deserialize_checkpoint_theater<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<cypherlite_executor::CheckpointTheater>, D::Error>
where
    D: Deserializer<'de>,
{
    // An omitted field is a legacy snapshot. Explicit null is damaged schema,
    // not permission to silently discard checkpointed enforcement.
    cypherlite_executor::CheckpointTheater::deserialize(deserializer).map(Some)
}

/// The one selector-field decoder (ADR-0120 §5): serde's `Option<String>`
/// treats an explicit null like an omitted field, but only an *omitted*
/// selector may encode the no-selection state its absence stands for — an
/// explicit null or empty string is damaged schema, not permission to
/// silently discard the selection.
fn deserialize_nonempty_selector<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let path = String::deserialize(deserializer)?;
    if path.is_empty() {
        return Err(D::Error::custom("manifest path selector must be nonempty"));
    }
    Ok(Some(path))
}
