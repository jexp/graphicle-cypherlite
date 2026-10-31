mod convert;
pub mod json;
mod kind;
mod model;
#[cfg(parquet_codec)]
pub(crate) mod parquet;

pub use kind::SnapshotCodecKind;

use cypherlite_core::{Error, Result};

use crate::snapshot;
use crate::StoreTarget;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Envelope {
    #[serde(default = "SnapshotCodecKind::absent")]
    pub(crate) codec: SnapshotCodecKind,
    #[serde(default)]
    pub(crate) checkpoint_lsn: u64,
    #[serde(default)]
    pub(crate) commit_ts: u64,
}

/// Parquet resolved from disk or requested in a build without the codec
/// (ADR-0105). Fails closed; never falls back to JSON.
pub(crate) fn parquet_unavailable() -> Error {
    Error::storage(
        "parquet snapshot codec is not compiled into this build (enable the \"parquet\" feature)",
    )
}

pub(crate) fn read_envelope(target: &StoreTarget) -> Result<Option<Envelope>> {
    if !snapshot::object_exists(target, snapshot::MANIFEST_JSON)? {
        return Ok(None);
    }
    let bytes = snapshot::get_object(target, "manifest.json")?;
    let env: Envelope =
        serde_json::from_slice(&bytes).map_err(|e| Error::storage(e.to_string()))?;
    Ok(Some(env))
}

// Callers (`parse_filesystem_uri`) are native-only, so this scanner and its
// `snapshot::has_parquet_store_files` dependency don't need to compile for
// wasm32.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn resolve_codec(
    target: &StoreTarget,
    requested: Option<SnapshotCodecKind>,
) -> Result<SnapshotCodecKind> {
    let has_json = snapshot::object_exists(target, json::GRAPH_JSON)?;
    let has_parquet = snapshot::has_parquet_store_files(target)?;
    if has_json && has_parquet {
        return Err(Error::storage(
            "mixed snapshot codecs: directory has both graph.json and Parquet store files",
        ));
    }
    let inferred = if let Some(env) = read_envelope(target)? {
        if env.codec == SnapshotCodecKind::Json && has_parquet {
            return Err(Error::storage(
                "mixed snapshot codecs: json envelope with Parquet store files",
            ));
        }
        if env.codec == SnapshotCodecKind::Parquet && has_json {
            return Err(Error::storage(
                "mixed snapshot codecs: parquet envelope with graph.json",
            ));
        }
        Some(env.codec)
    } else if has_json {
        Some(SnapshotCodecKind::Json)
    } else if has_parquet {
        Some(SnapshotCodecKind::Parquet)
    } else {
        None
    };
    let codec = match (requested, inferred) {
        (Some(want), Some(have)) if want != have => {
            return Err(Error::storage(format!(
                "snapshot codec mismatch: directory is {have}, requested {want}"
            )))
        }
        (Some(want), _) => want,
        (None, Some(have)) => have,
        (None, None) => SnapshotCodecKind::default(),
    };
    check_available(codec)?;
    Ok(codec)
}

pub(crate) fn check_available(codec: SnapshotCodecKind) -> Result<()> {
    if codec.is_compiled() {
        Ok(())
    } else {
        Err(parquet_unavailable())
    }
}

#[cfg(test)]
mod tests;
