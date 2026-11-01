//! The sidecar manifest (ADR-0406 §5): one home for the wire types,
//! the bounded read/write, and the typed probe every "is this a
//! markdown store" question consults.
//!
//! Extracted from `mod.rs` (the no-1k-line rule, LABS-311).

use std::collections::{BTreeMap, BTreeSet};

use cypherlite_core::Result;
use cypherlite_executor::CheckpointTheater;
use serde::{Deserialize, Serialize};

#[cfg(not(target_arch = "wasm32"))]
use super::super::benign_absence;
use super::super::catalog;
use super::codec_error;
use super::vault::crc;
use super::MarkdownBodyProperties;
use crate::codec::SnapshotCodecKind;
use crate::snapshot::MAX_JSON_OBJECT;
use crate::vfs::{atomic_write, read_to_vec, Vfs};
use crate::StoreTarget;

/// The hidden engine directory inside the vault (the `.obsidian/`
/// convention): WAL, CDC, journal, and the sidecar manifest.
pub(crate) const ENGINE_DIR: &str = ".cypherlite";
/// The sidecar manifest, published last (the manifest-last rule).
pub(crate) const MANIFEST: &str = ".cypherlite/manifest.json";
/// The default WAL directory for a markdown store (ADR-0406 §3).
pub(crate) const WAL_DIR: &str = ".cypherlite/wal";
/// The copy-on-write journal of an in-flight checkpoint (ADR-0406 §5).
pub(super) const JOURNAL_DIR: &str = ".cypherlite/checkpoint-journal";
pub(super) const JOURNAL_INTENT: &str = ".cypherlite/checkpoint-journal/intent.json";
/// The pre-image of a journaled checkpoint: the paths that existed
/// before it began. Rollback deletes only intent files absent from
/// this set — a file that predates the checkpoint is restored, never
/// deleted (ADR-0406 §5).
pub(super) const JOURNAL_PREIMAGE: &str = ".cypherlite/checkpoint-journal/preimage.json";
// The sidecar's metadata bound is the ADR-0086 per-metadata-object
// ceiling, spelled through the one crate-visible home
// (`snapshot::MAX_JSON_OBJECT`) at every check site below — never a
// mirrored second constant.
/// The generated/learned schema (ADR-0406 §2): descriptive, never
/// prescriptive — the vault stays authoritative and drift regenerates
/// it at the next checkpoint.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub(super) struct LearnedSchema {
    /// Label → its key property name (the filename source).
    #[serde(default)]
    pub(super) keys: BTreeMap<String, String>,
    /// The relationship-type vocabulary the frontmatter classification
    /// consults.
    #[serde(default)]
    pub(super) rel_types: BTreeSet<String>,
    /// Files whose frontmatter does not parse (real Obsidian vaults
    /// carry `@handle` values, Templater placeholders, malformed
    /// mappings). They load body-only — Obsidian's own behavior — and
    /// are preserved verbatim at checkpoint: the raw text is the user's,
    /// never silently rewritten (ADR-0406 §6 warnings).
    #[serde(default)]
    pub(super) raw_files: BTreeSet<String>,
    /// Files whose vault key was a case-only duplicate and folded to
    /// the first spelling (ADR-0406 §2) — the load's warning record;
    /// the loser file is deleted at the next checkpoint.
    #[serde(default)]
    pub(super) folded_keys: BTreeSet<String>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub(crate) struct VaultManifest {
    pub(super) codec: Option<SnapshotCodecKind>,
    #[serde(default)]
    pub(crate) checkpoint_lsn: u64,
    #[serde(default)]
    pub(crate) commit_ts: u64,
    #[serde(default)]
    pub(super) value_encoding: Option<u64>,
    #[serde(default)]
    pub(super) next_node_id: i64,
    #[serde(default)]
    pub(super) next_rel_id: i64,
    /// The catalog and graph type, carried by the one wire carrier —
    /// flattened inline, so the sidecar's shape is unchanged.
    #[serde(flatten)]
    pub(super) catalog: catalog::CatalogWire,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::manifest::deserialize_checkpoint_theater"
    )]
    pub(super) theater: Option<CheckpointTheater>,
    #[serde(default)]
    pub(super) read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) ontology: Option<String>,
    #[serde(default)]
    pub(super) learned: LearnedSchema,
    /// The body-property configuration (ADR-0460): the ordered default
    /// list plus per-label overrides. Store configuration, not learned
    /// schema — recorded at every checkpoint, replaced only by an
    /// explicit open-time config; an omitted field is the legacy
    /// single-property `body` form.
    #[serde(default, skip_serializing_if = "MarkdownBodyProperties::is_default")]
    pub(super) bodies: MarkdownBodyProperties,
    /// Embeddings (vector-indexed properties), keyed `"<vault path>#<property>"`.
    #[serde(default)]
    pub(super) embeddings: BTreeMap<String, serde_json::Value>,
    /// Every guarded node file at the last checkpoint with its length
    /// and crc32 — the dirty-WAL and external-write guard's expectation.
    #[serde(default)]
    pub(super) files: BTreeMap<String, FileStat>,
    /// The stable node ids (ADR-0406 §5's WAL-recovery contract), keyed
    /// by vault key: restored at load so id-based WAL replay after a
    /// checkpoint addresses the same nodes the writes named.
    #[serde(default)]
    pub(super) node_ids: BTreeMap<String, i64>,
    /// The stable relationship ids, keyed by the canonical relationship
    /// identity (source key, type, target key, sorted properties).
    #[serde(default)]
    pub(super) rel_ids: BTreeMap<String, i64>,
}

/// The stat-first digest of one guarded file: length (cheap, consulted
/// first) and crc32 of content (consulted on length match).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(super) struct FileStat {
    pub(super) len: u64,
    pub(super) crc: u32,
    /// The entry's modification time when the VFS reports one — the
    /// stat-first half of the ADR-0406 §5 guard (absent on virtual
    /// backends; the crc then proves the file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) mtime: Option<u64>,
}

/// A file's identity is its content digest (length + crc); `mtime` is
/// advisory guard metadata — a fresh write can carry a different mtime
/// with identical content, so it never participates in equality
/// (vault_matches) — only the stat-first guard consults it.
impl PartialEq for FileStat {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.crc == other.crc
    }
}
impl Eq for FileStat {}

impl FileStat {
    /// The stat of `bytes` as the guards record it; the caller fills
    /// `mtime` from the VFS when it reports one.
    pub(super) fn of(bytes: &[u8]) -> Self {
        Self {
            len: bytes.len() as u64,
            crc: crc(bytes),
            mtime: None,
        }
    }

    /// Does `bytes` differ from this recorded stat (length first, then
    /// the crc)? The one differ predicate the journal's touched check
    /// and the crc fallback of the guards consult.
    pub(super) fn differs_from(&self, bytes: &[u8]) -> bool {
        self.len != bytes.len() as u64 || self.crc != crc(bytes)
    }
}

/// What the sidecar manifest says about a directory (ADR-0406 §5) —
/// the one typed classification every "is this a markdown store"
/// question consults: codec resolution, the pre-lease default-WAL
/// routing, the reset refusal, and the manifest reader itself.
#[derive(Clone, Debug)]
enum ManifestProbe {
    /// No sidecar: not a markdown store (yet).
    Absent,
    /// A sidecar naming the markdown codec, parsed.
    Markdown(Box<VaultManifest>),
    /// A sidecar naming another codec.
    Other(SnapshotCodecKind),
    /// A sidecar that is present but oversized, unreadable,
    /// unparseable, or names no codec: every fail-closed decision
    /// treats the directory as a markdown store anyway (the reset
    /// refusal and `.cypherlite/` WAL routing must not be bypassable
    /// by a corrupt manifest).
    Unparseable,
}

/// The one bounded read + classification of the sidecar. The metadata
/// length bounds the read before any byte is fetched.
fn probe(vfs: &dyn Vfs) -> Result<ManifestProbe> {
    if !vfs.exists(MANIFEST)? {
        return Ok(ManifestProbe::Absent);
    }
    let len = vfs.metadata(MANIFEST)?.len;
    if len > MAX_JSON_OBJECT {
        return Ok(ManifestProbe::Unparseable);
    }
    let bytes = read_to_vec(vfs, MANIFEST)?;
    Ok(classify_manifest_bytes(&bytes))
}

/// The pre-lease path probe (the default-WAL routing and reset refusal
/// run before any VFS exists): the same classification over a host
/// directory, collapsed to "treat as a markdown store".
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn probe_path(dir: &std::path::Path) -> bool {
    let sidecar = dir.join(MANIFEST);
    // The same metadata-first bound as the VFS probe. The fail-closed
    // verdict is spelled once, in the read match below: a benign
    // absence is "not a markdown store"; every other metadata or read
    // error — and any oversized sidecar — routes as a markdown store,
    // never as "not a store" (the reset refusal must not be bypassable
    // by an unreadable sidecar).
    match std::fs::metadata(&sidecar) {
        // A benign absence is the read's own arm below; every other
        // metadata error fails closed with the one verdict.
        Err(err) if benign_absence(&err) => {}
        Err(_) => return true,
        Ok(meta) if meta.len() > MAX_JSON_OBJECT => return true,
        Ok(_) => {}
    }
    match std::fs::read(&sidecar) {
        Ok(bytes) => {
            // The same bytes-level classification as the VFS probe: a
            // markdown or unparseable sidecar routes as a markdown
            // store; another codec does not. The read succeeded, so
            // the classification is never `Absent` (only the probes'
            // existence checks produce it).
            !matches!(classify_manifest_bytes(&bytes), ManifestProbe::Other(_))
        }
        Err(err) if benign_absence(&err) => false,
        Err(_) => true,
    }
}

/// Refuse [`crate::reset`](super::super::super::reset) on a markdown
/// vault store (ADR-0406 §6): the vault's markdown files are
/// hand-authored content — deleting them is the user's call, by hand.
/// The codec owns the policy; the reset entry stays codec-neutral.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn refuse_reset(dir: &std::path::Path) -> Result<()> {
    if probe_path(dir) {
        return Err(codec_error(
            "refusing to reset a markdown vault store; delete the vault's .cypherlite/ \
             directory (and the markdown files) by hand",
        ));
    }
    Ok(())
}

/// The bytes-level classification shared by the VFS probe and the
/// pre-lease path probe, so the policy has exactly one home. `Absent`
/// is never returned: only the probes' existence checks produce it.
fn classify_manifest_bytes(bytes: &[u8]) -> ManifestProbe {
    if bytes.len() as u64 > MAX_JSON_OBJECT {
        return ManifestProbe::Unparseable;
    }
    match serde_json::from_slice::<VaultManifest>(bytes) {
        Ok(manifest) => match manifest.codec {
            Some(SnapshotCodecKind::Markdown) => ManifestProbe::Markdown(Box::new(manifest)),
            Some(other) => ManifestProbe::Other(other),
            None => ManifestProbe::Unparseable,
        },
        Err(_) => ManifestProbe::Unparseable,
    }
}

pub(crate) fn read_manifest(target: &StoreTarget) -> Result<Option<VaultManifest>> {
    match probe(&*target.vfs())? {
        ManifestProbe::Absent => Ok(None),
        ManifestProbe::Markdown(manifest) => Ok(Some(*manifest)),
        ManifestProbe::Other(other) => Err(codec_error(format!(
            "sidecar manifest names codec {other}, not markdown"
        ))),
        ManifestProbe::Unparseable => Err(codec_error(
            "sidecar manifest names no codec or does not parse, not markdown",
        )),
    }
}

pub(super) fn write_manifest(vfs: &dyn Vfs, manifest: &VaultManifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|e| codec_error(format!("serializing sidecar manifest: {e}")))?;
    if bytes.len() as u64 > MAX_JSON_OBJECT {
        return Err(codec_error(format!(
            "{MANIFEST} exceeds {MAX_JSON_OBJECT}-byte metadata limit"
        )));
    }
    atomic_write(vfs, MANIFEST, &bytes)
}
