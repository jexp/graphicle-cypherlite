//! Store-version registry (ADR-0127): the single source of truth for the
//! version marker of every durable subsystem — the Parquet manifest's
//! `format_version`, the JSON snapshot's `value_encoding`, the WAL segment
//! magic and the `.cl` bundle superblock magic + `VERSION`. Writers take
//! their current marker from here and readers gate on the readable sets
//! declared here; a future format ADR changes this module and nothing else
//! to declare a new version (ADR-0120 is the first consumer).
//!
//! Every gate — the ADR-0114 unknown-version rejections, the WAL ownership
//! admission, and migrate-on-open — consults the one per-marker
//! [`Classification`]. Detection is the marker the codec loader already
//! decoded: snapshot loaders construct the post-gate [`SnapshotMarker`] at
//! their own readable gate and return it with the decoded state, so
//! detection costs no extra I/O (Decision 2). [`MigrationReport`] records
//! what migrate-on-open rewrote (Decision 8).

mod report;

pub(crate) use report::{annotate_legacy_failure, MigrationReport, Subsystem};

/// How a persisted version marker compares to what this binary writes and
/// can read (ADR-0127 Decision 2). Every version gate and the
/// migrate-on-open decision consult this one classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Classification<T> {
    /// What this binary writes.
    Current,
    /// Readable but older than current: migrate-on-open rewrites it.
    Legacy(T),
    /// Not in the readable set: open fails at the ADR-0114 gate.
    Unreadable(T),
}

/// Parquet snapshot: manifest `format_version`.
pub(crate) mod manifest {
    #[cfg(parquet_codec)]
    use super::Classification;

    // The read gate lives in `snapshot::native`, compiled only with the
    // Parquet codec (ADR-0105), and so are the readable set and classifier.
    /// Oldest manifest this binary can load.
    #[cfg(parquet_codec)]
    pub(crate) const MIN_READABLE_VERSION: u32 = 1;
    /// What this binary writes while the manifest selects no ontology
    /// object.
    #[cfg(parquet_codec)]
    pub(crate) const CURRENT_VERSION: u32 = 3;
    /// What this binary writes when the manifest selects an ontology object
    /// (ADR-0120 §5). A v4 manifest is a v3 manifest plus `ontology_path`.
    #[cfg(parquet_codec)]
    pub(crate) const ONTOLOGY_VERSION: u32 = 4;
    /// Every readable version; v1/v2 use the monolithic
    /// `nodes.parquet`/`relationships.parquet` fallback reader (ADR-0005,
    /// pre-ADR-0022 shards).
    #[cfg(parquet_codec)]
    pub(crate) const READABLE: &[u32] =
        &[MIN_READABLE_VERSION, 2, CURRENT_VERSION, ONTOLOGY_VERSION];

    /// The version a manifest is written with (ADR-0120 §8: the format
    /// follows the content): [`ONTOLOGY_VERSION`] when it selects an
    /// ontology object, [`CURRENT_VERSION`] otherwise.
    #[cfg(parquet_codec)]
    pub(crate) fn version_for_write(has_ontology: bool) -> u32 {
        if has_ontology {
            ONTOLOGY_VERSION
        } else {
            CURRENT_VERSION
        }
    }

    /// Classify a decoded manifest `format_version` against the content it
    /// carries (ADR-0127 Decision 2, ADR-0120 §8). Only the pair the writer
    /// emits is current — `(3, no ontology)` or `(4, ontology)`; every
    /// other readable pair is legacy and migrate-on-open normalizes it to
    /// the content's version.
    #[cfg(parquet_codec)]
    pub(crate) fn classify(version: u32, has_ontology: bool) -> Classification<u32> {
        if version == version_for_write(has_ontology) {
            Classification::Current
        } else if READABLE.contains(&version) {
            Classification::Legacy(version)
        } else {
            Classification::Unreadable(version)
        }
    }

    /// The ADR-0114 gate for a decoded manifest `format_version`:
    /// classify it and reject an unreadable store with this subsystem's
    /// one unsupported-version error; a readable version comes back as
    /// the [`super::SnapshotMarker`] the loader hands to its caller.
    #[cfg(parquet_codec)]
    pub(crate) fn gate(
        version: u32,
        has_ontology: bool,
    ) -> cypherlite_core::Result<super::SnapshotMarker> {
        match classify(version, has_ontology) {
            Classification::Current => Ok(super::SnapshotMarker::Current),
            Classification::Legacy(version) => Ok(super::SnapshotMarker::Legacy(
                super::Subsystem::Manifest,
                u64::from(version),
            )),
            Classification::Unreadable(version) => Err(cypherlite_core::Error::storage(format!(
                "unsupported snapshot version {version}"
            ))),
        }
    }
}

/// JSON snapshot: `graph.json` `value_encoding`. The envelope has no
/// separate `format_version`; this marker *is* the JSON codec's version
/// dimension (ADR-0127 Decision 1). The persisted shape (`Option<u64>`,
/// absent means encoding 1) is normalized once, in [`classify`].
pub(crate) mod value_encoding {
    use super::Classification;

    /// The version an absent marker means, and the oldest this binary loads.
    pub(crate) const ABSENT: u64 = 1;
    /// What this binary writes while `graph.json` carries no ontology
    /// document.
    pub(crate) const CURRENT_VERSION: u64 = 2;
    /// What this binary writes when `graph.json` carries the inline
    /// `ontology` document (ADR-0120 §6): value encoding 2 plus that field.
    pub(crate) const ONTOLOGY_VERSION: u64 = 3;
    /// Every readable marker, after the absent marker is normalized to
    /// encoding 1.
    pub(crate) const READABLE: &[u64] = &[ABSENT, CURRENT_VERSION, ONTOLOGY_VERSION];

    /// The encoding `graph.json` is written with (ADR-0120 §8: the format
    /// follows the content): [`ONTOLOGY_VERSION`] when it carries the
    /// inline ontology document, [`CURRENT_VERSION`] otherwise.
    pub(crate) fn version_for_write(has_ontology: bool) -> u64 {
        if has_ontology {
            ONTOLOGY_VERSION
        } else {
            CURRENT_VERSION
        }
    }

    /// Classify a decoded `value_encoding` marker against the content it
    /// carries, normalizing the absent marker to encoding 1 (ADR-0127
    /// Decision 2, ADR-0120 §8). Only the pair the writer emits is current
    /// — `(2, no ontology)` or `(3, ontology)`; every other readable pair
    /// is legacy and migrate-on-open normalizes it to the content's
    /// version.
    pub(crate) fn classify(marker: Option<u64>, has_ontology: bool) -> Classification<u64> {
        let marker = marker.unwrap_or(ABSENT);
        if marker == version_for_write(has_ontology) {
            Classification::Current
        } else if READABLE.contains(&marker) {
            Classification::Legacy(marker)
        } else {
            Classification::Unreadable(marker)
        }
    }

    /// The ADR-0114 gate for a decoded `value_encoding` marker: normalize
    /// the absent marker once, classify, and reject an unreadable store
    /// with this subsystem's one unsupported-version error. A readable
    /// marker comes back with its normalized version — which the loader
    /// needs for decode — and the [`super::SnapshotMarker`] it hands to
    /// its caller.
    pub(crate) fn gate(
        marker: Option<u64>,
        has_ontology: bool,
    ) -> cypherlite_core::Result<(u64, super::SnapshotMarker)> {
        match classify(marker, has_ontology) {
            // The store's actual write version, not the no-ontology
            // default: `(3, ontology)` is Current too, and the loader
            // needs the 3 back for decode.
            Classification::Current => Ok((
                version_for_write(has_ontology),
                super::SnapshotMarker::Current,
            )),
            Classification::Legacy(version) => Ok((
                version,
                super::SnapshotMarker::Legacy(super::Subsystem::ValueEncoding, version),
            )),
            Classification::Unreadable(other) => Err(cypherlite_core::Error::storage(format!(
                "unsupported graph value encoding: {other}"
            ))),
        }
    }
}

/// WAL: segment identity magic. `MDBWAL01` is de-facto WAL version 1;
/// ADR-0120's `MDBWAL02` is the second readable magic (ADR-0127
/// Decision 5), written on segments created while the store holds an
/// ontology so older binaries fail closed before they could trim an
/// `OntologySet`/`OntologyDrop` record.
pub(crate) mod wal {
    use super::Classification;

    /// Oldest segment magic this binary can replay.
    pub(crate) const MIN_READABLE_MAGIC: &[u8; 8] = b"MDBWAL01";
    /// Magic written at the head of a segment created while the store
    /// holds an ontology, committed or still pending in the WAL
    /// (ADR-0120 §4). The frame layout is unchanged; the magic alone
    /// fails older binaries closed. `pub` (not `pub(crate)`) for the
    /// same reason as `CURRENT_MAGIC`: the `wal` module re-exports it.
    pub const ONTOLOGY_MAGIC: &[u8; 8] = b"MDBWAL02";
    /// Magic this binary writes at the head of every new segment whose
    /// store holds no ontology. `pub` (not `pub(crate)`) because the
    /// doc-hidden public `wal::MAGIC` re-export is backed by this
    /// constant.
    pub const CURRENT_MAGIC: &[u8; 8] = MIN_READABLE_MAGIC;
    /// Every readable segment magic.
    pub(crate) const READABLE_MAGICS: &[&[u8; 8]] = &[MIN_READABLE_MAGIC, ONTOLOGY_MAGIC];

    /// Classify a decoded segment magic (ADR-0127 Decision 2). Both
    /// readable magics are written by this binary — the choice is
    /// per-segment ontology state, not format age — so the non-default
    /// `MDBWAL02` classifies `Legacy` only in the registry's
    /// "not the default write marker" sense; no WAL migration exists.
    pub(crate) fn classify(magic: &[u8; 8]) -> Classification<[u8; 8]> {
        match magic {
            m if m == CURRENT_MAGIC => Classification::Current,
            m if READABLE_MAGICS.contains(&m) => Classification::Legacy(*m),
            m => Classification::Unreadable(*m),
        }
    }

    /// Whether a segment magic is readable. The single predicate behind
    /// both the WAL scanner and the native-ownership admission, so they
    /// cannot disagree about what a WAL segment is (ADR-0127 Decision 5).
    pub(crate) fn is_readable(magic: &[u8; 8]) -> bool {
        !matches!(classify(magic), Classification::Unreadable(_))
    }
}

/// Bundle container: superblock magic + `VERSION` (ADR-0074). Frozen for
/// payload changes; bumped only for a true container layout change
/// (ADR-0127 Decision 6).
pub(crate) mod bundle {
    use super::Classification;

    /// Oldest readable superblock magic; also what this binary writes while
    /// the container layout is frozen.
    pub(crate) const MIN_READABLE_MAGIC: &[u8; 8] = b"MDBCLB01";
    /// Oldest readable superblock `VERSION`; also what this binary writes.
    pub(crate) const MIN_READABLE_VERSION: u32 = 1;
    pub(crate) const CURRENT_MAGIC: &[u8; 8] = MIN_READABLE_MAGIC;
    pub(crate) const CURRENT_VERSION: u32 = MIN_READABLE_VERSION;
    /// Every readable (magic, `VERSION`) pair.
    pub(crate) const READABLE: &[(&[u8; 8], u32)] = &[(MIN_READABLE_MAGIC, MIN_READABLE_VERSION)];

    /// Classify a decoded superblock marker as one (magic, `VERSION`) pair
    /// (ADR-0127 Decision 2).
    pub(crate) fn classify(magic: &[u8; 8], version: u32) -> Classification<([u8; 8], u32)> {
        let pair = (*magic, version);
        match pair {
            (m, v) if m == *CURRENT_MAGIC && v == CURRENT_VERSION => Classification::Current,
            (m, v) if READABLE.contains(&(&m, v)) => Classification::Legacy(pair),
            _ => Classification::Unreadable(pair),
        }
    }
}

/// The snapshot marker a codec loader decoded, after its readable gate
/// passed (ADR-0127 Decision 2): the store is at the current format, or at
/// a readable legacy version that migrate-on-open upgrades.
/// Constructed by the codec loader at its own readable gate, where the
/// legacy subsystem is known locally (ADR-0127 Decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotMarker {
    Current,
    Legacy(Subsystem, u64),
}

#[cfg(test)]
#[path = "version/tests.rs"]
mod tests;
