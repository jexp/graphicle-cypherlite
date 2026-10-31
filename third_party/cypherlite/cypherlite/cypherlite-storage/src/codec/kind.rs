use std::str::FromStr;

use cypherlite_core::{Error, Result};
use serde::{Deserialize, Serialize};

/// Snapshot encoding for store files (ADR-0050). WAL is independent.
///
/// `Default` is the codec for new stores: Parquet when it is compiled in
/// (ADR-0105), JSON otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotCodecKind {
    Parquet,
    Json,
}

impl Default for SnapshotCodecKind {
    fn default() -> Self {
        Self::compiled()[0]
    }
}

impl SnapshotCodecKind {
    /// Whether this codec is compiled into the build. JSON always is;
    /// Parquet needs the `parquet` feature on a non-wasm32 target (ADR-0105).
    ///
    /// # Example
    /// ```
    /// use cypherlite_storage::SnapshotCodecKind;
    /// assert!(SnapshotCodecKind::Json.is_compiled());
    /// assert_eq!(
    ///     SnapshotCodecKind::Parquet.is_compiled(),
    ///     SnapshotCodecKind::compiled().contains(&SnapshotCodecKind::Parquet),
    /// );
    /// ```
    pub const fn is_compiled(self) -> bool {
        match self {
            Self::Parquet => cfg!(parquet_codec),
            Self::Json => true,
        }
    }

    /// Every codec compiled into the build, the new-store default first.
    ///
    /// # Example
    /// ```
    /// use cypherlite_storage::SnapshotCodecKind;
    /// let compiled = SnapshotCodecKind::compiled();
    /// assert_eq!(compiled[0], SnapshotCodecKind::default());
    /// assert_eq!(compiled.last(), Some(&SnapshotCodecKind::Json));
    /// assert!(compiled.iter().all(|codec| codec.is_compiled()));
    /// ```
    pub fn compiled() -> &'static [SnapshotCodecKind] {
        if Self::Parquet.is_compiled() {
            &[Self::Parquet, Self::Json]
        } else {
            &[Self::Json]
        }
    }

    /// Codec of a manifest without a `codec` field: always Parquet
    /// (ADR-0050 §2), independent of the build's new-store default.
    pub(crate) fn absent() -> Self {
        Self::Parquet
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parquet => "parquet",
            Self::Json => "json",
        }
    }

    pub fn parse_name(s: &str) -> Result<Self> {
        s.parse()
    }
}

impl FromStr for SnapshotCodecKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "parquet" => Ok(Self::Parquet),
            "json" => Ok(Self::Json),
            other => Err(Error::storage(format!(
                "unknown snapshot codec '{other}' (expected parquet or json)"
            ))),
        }
    }
}

impl std::fmt::Display for SnapshotCodecKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
