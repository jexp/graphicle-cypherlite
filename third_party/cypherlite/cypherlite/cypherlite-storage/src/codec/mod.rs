mod catalog;
mod convert;
pub mod json;
mod kind;
pub(crate) mod markdown;
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

/// The never-written state a root `manifest.json` naming the markdown
/// codec is (ADR-0406 §3): the markdown store's manifest home is the
/// `.cypherlite/` sidecar, so a root envelope naming it — with or
/// without a sidecar — is invalid evidence everywhere it is classified.
/// The refusal lives in [`read_envelope`], so every envelope reader
/// (open's codec resolution and the cold `store_info` read alike)
/// inherits the one typed error instead of re-spelling the branch.
pub(crate) fn root_envelope_names_markdown() -> Error {
    Error::storage(
        "mixed snapshot codecs: a root manifest.json names the markdown codec; the \
         markdown store's manifest is the .cypherlite/ sidecar",
    )
}

/// Whether an I/O error proves a probed engine file cannot be present —
/// plain absence, or a path component that is a regular file (a `.cl`
/// bundle's store root is the file itself, so a path under it can never
/// resolve). Every other error is a present-but-unreadable file and
/// fails closed. Codec-neutral I/O classification, so it lives here —
/// the lowest module both path-level probes share — and the markdown
/// sidecar probe imports it downward; a markdown policy it is not.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn benign_absence(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// The root envelope as the classic-evidence probe reads it: the codec
/// member alone, **required** — the wire envelope defaults every field,
/// so any JSON object would otherwise parse as one, and a vault
/// folder's own unrelated `manifest.json` (no `codec` member, or one
/// outside the vocabulary) is not engine evidence.
#[cfg(not(target_arch = "wasm32"))]
#[derive(serde::Deserialize)]
struct ClassicProbe {
    codec: SnapshotCodecKind,
}

/// The path-level half of the envelope evidence `resolve_codec` counts:
/// a root `manifest.json` that parses as an engine envelope naming a
/// classic codec (an envelope-only classic store — its graph objects
/// removed — still carries its envelope). A markdown store never
/// writes a root manifest.json (its manifest is the `.cypherlite/`
/// sidecar), and a vault folder's own unrelated `manifest.json` does
/// not parse as the probe above, so this never false-refuses a
/// legitimate markdown open.
///
/// Read errors are never folded into "no classic evidence" (the
/// no-silent-fallbacks rule): a benign absence — [`benign_absence`]'s
/// vocabulary — is a definite "no", and every other read error is
/// treated as classic evidence, fail-closed in
/// [`probe_path`](crate::codec::markdown::probe_path)'s direction, so
/// the pre-lease guard refuses to create WAL engine state beside a
/// manifest.json it could not read. The read is bounded like every
/// sibling metadata probe: the length is proven from `metadata` against
/// [`snapshot::MAX_JSON_OBJECT`] before any byte is fetched, and an
/// oversized manifest fails closed exactly like an unreadable one.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn root_envelope_names_classic(path: &std::path::Path) -> bool {
    let manifest = path.join(snapshot::MANIFEST_JSON);
    match std::fs::metadata(&manifest) {
        Err(err) if benign_absence(&err) => return false,
        Err(_) => return true,
        Ok(meta) if meta.len() > snapshot::MAX_JSON_OBJECT => return true,
        Ok(_) => {}
    }
    let bytes = match std::fs::read(&manifest) {
        Ok(bytes) => bytes,
        // A benign absence here is a removal race with the metadata
        // above: the file is gone, so there is no evidence.
        Err(err) if benign_absence(&err) => return false,
        Err(_) => return true,
    };
    let Ok(probe) = serde_json::from_slice::<ClassicProbe>(&bytes) else {
        return false;
    };
    matches!(
        probe.codec,
        SnapshotCodecKind::Json | SnapshotCodecKind::Parquet
    )
}

pub(crate) fn read_envelope(target: &StoreTarget) -> Result<Option<Envelope>> {
    if !snapshot::object_exists(target, snapshot::MANIFEST_JSON)? {
        return Ok(None);
    }
    let bytes = snapshot::get_object(target, snapshot::MANIFEST_JSON)?;
    let env: Envelope =
        serde_json::from_slice(&bytes).map_err(|e| Error::storage(e.to_string()))?;
    // A root envelope naming the markdown codec is a never-written state
    // (ADR-0406 §3) and invalid evidence everywhere: refused at the one
    // typed reader, so every caller — codec resolution, the cold
    // `store_info` read, the LSN reader — inherits the same error
    // instead of each re-spelling the branch.
    if env.codec == SnapshotCodecKind::Markdown {
        return Err(root_envelope_names_markdown());
    }
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
    // The markdown store's evidence is its sidecar manifest (ADR-0406
    // §5): explicit opt-in or an engine-written `.cypherlite/`, read
    // through the codec's one typed reader (bounded, fail-closed — a
    // present-but-not-markdown sidecar is its typed error).
    let has_markdown = markdown::read_manifest(target)?.is_some();
    // A root envelope naming the markdown codec never reaches this far:
    // `read_envelope` refuses it at the one typed reader, so every
    // caller inherits that error (ADR-0406 §3 — the markdown store's
    // manifest home is the `.cypherlite/` sidecar).
    let envelope = read_envelope(target)?;
    // The evidence set, modeled once: every piece of evidence the
    // directory carries — the markdown sidecar, the root envelope's
    // codec, `graph.json`, the Parquet store files — must name one
    // codec. The set check is exhaustive: any incoherent pair fails
    // here, including combinations no hand-enumerated branch used to
    // spell.
    let mut evidenced: Vec<SnapshotCodecKind> = Vec::new();
    if has_markdown {
        evidenced.push(SnapshotCodecKind::Markdown);
    }
    if let Some(env) = &envelope {
        evidenced.push(env.codec);
    }
    if has_json {
        evidenced.push(SnapshotCodecKind::Json);
    }
    if has_parquet {
        evidenced.push(SnapshotCodecKind::Parquet);
    }
    if let Some((&first, rest)) = evidenced.split_first() {
        if let Some(&other) = rest.iter().find(|codec| **codec != first) {
            return Err(Error::storage(format!(
                "mixed snapshot codecs: directory has both {first} and {other} evidence"
            )));
        }
    }
    // After the set check every surviving element names the one
    // codec, so the first is the inference — no hand-enumerated
    // priority order to drift from the evidence model.
    let inferred = evidenced.first().copied();
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
