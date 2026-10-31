//! The migrate-on-open report and its error-context helper (ADR-0127
//! Decisions 4 and 8), split out of the version registry.

use cypherlite_core::Error;

use crate::map_message;

#[cfg(parquet_codec)]
use super::manifest;
use super::{value_encoding, SnapshotMarker};

/// The subsystems a snapshot migration can rewrite. The WAL and bundle
/// container never appear here today: `MDBWAL01` is both minimum and
/// current (ADR-0127 Decision 5) and the container format is frozen
/// (Decision 6), so only the snapshot markers migrate. `Manifest` exists
/// only with the Parquet codec (ADR-0105): its loader is the variant's
/// sole constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Subsystem {
    #[cfg(parquet_codec)]
    Manifest,
    ValueEncoding,
}

impl Subsystem {
    /// The version this binary writes for the subsystem.
    fn current(&self) -> u64 {
        match self {
            #[cfg(parquet_codec)]
            Subsystem::Manifest => u64::from(manifest::CURRENT_VERSION),
            Subsystem::ValueEncoding => value_encoding::CURRENT_VERSION,
        }
    }
}

impl std::fmt::Display for Subsystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            #[cfg(parquet_codec)]
            Subsystem::Manifest => "manifest",
            Subsystem::ValueEncoding => "value_encoding",
        })
    }
}

/// Record of what migrate-on-open rewrote (ADR-0127 Decision 8): entries
/// are "subsystem X was at version N, now current". An empty report means
/// no migration happened. Internal to `cypherlite-storage`; the crate has
/// no logging facility, so the report is observable only through
/// `pub(crate)` test hooks.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct MigrationReport {
    migrated: Vec<(Subsystem, u64)>,
}

impl MigrationReport {
    pub(crate) fn record(&mut self, subsystem: Subsystem, from: u64) {
        self.migrated.push((subsystem, from));
    }

    /// Every migrated subsystem as (subsystem, version it was at), in the
    /// order recorded.
    pub(crate) fn migrations(&self) -> &[(Subsystem, u64)] {
        &self.migrated
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.migrated.is_empty()
    }
}

impl std::fmt::Display for MigrationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            return f.write_str("store is at current format");
        }
        for (i, (subsystem, from)) in self.migrations().iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{subsystem} {from}→{}", subsystem.current())?;
        }
        Ok(())
    }
}

/// Attach the decoded version to a legacy-reader failure without changing
/// its `Error` variant or doubling its display prefix (ADR-0127 Decision
/// 4): [`map_message`] appends the context to `Storage` and `Io` messages;
/// every other variant keeps its payload verbatim.
pub(crate) fn with_version_context(error: Error, context: std::fmt::Arguments<'_>) -> Error {
    map_message(error, |message| format!("{message} ({context})"))
}

/// The one legacy-error shape (ADR-0127 Decision 4): a failure while
/// reading a legacy store names the decoded version via
/// [`with_version_context`]; a current store keeps its error verbatim. The
/// context wording is per subsystem and lives here alone, so every legacy
/// gate phrases it identically.
pub(crate) fn annotate_legacy_failure(error: Error, marker: SnapshotMarker) -> Error {
    let SnapshotMarker::Legacy(subsystem, version) = marker else {
        return error;
    };
    match subsystem {
        #[cfg(parquet_codec)]
        Subsystem::Manifest => with_version_context(
            error,
            format_args!("store snapshot is at manifest version {version}"),
        ),
        Subsystem::ValueEncoding => with_version_context(
            error,
            format_args!("store snapshot is at value_encoding {version}"),
        ),
    }
}
