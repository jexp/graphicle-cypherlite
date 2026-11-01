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
    /// The snapshot is a folder of markdown files (ADR-0406): an
    /// Obsidian-compatible vault read and written by the engine, with
    /// the WAL and the engine sidecar under `.cypherlite/`.
    Markdown,
}

impl Default for SnapshotCodecKind {
    fn default() -> Self {
        Self::compiled()[0]
    }
}

impl SnapshotCodecKind {
    /// The codec vocabulary, spelled once (the new-store default
    /// first): [`Self::compiled`] derives its build-dependent slice
    /// from it, the unknown-codec error derives its expected-name
    /// list, and [`FromStr`] derives its name→kind inverse through
    /// [`Self::as_str`], so a fourth codec edits this table (and
    /// `as_str`) alone.
    const ALL: [Self; 3] = [Self::Parquet, Self::Json, Self::Markdown];

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
            Self::Json | Self::Markdown => true,
        }
    }

    /// Every codec compiled into the build, the new-store default first.
    ///
    /// # Example
    /// ```
    /// use cypherlite_storage::SnapshotCodecKind;
    /// let compiled = SnapshotCodecKind::compiled();
    /// assert_eq!(compiled[0], SnapshotCodecKind::default());
    /// assert_eq!(compiled.last(), Some(&SnapshotCodecKind::Markdown));
    /// assert!(compiled.iter().all(|codec| codec.is_compiled()));
    /// ```
    pub fn compiled() -> &'static [SnapshotCodecKind] {
        if Self::Parquet.is_compiled() {
            &Self::ALL
        } else {
            &Self::ALL[1..]
        }
    }

    /// The codec names as the unknown-codec error spells them —
    /// [`ALL`](Self::ALL) rendered as prose, so the message cannot
    /// drift from the vocabulary.
    fn expected_names() -> String {
        let names: Vec<&str> = Self::ALL.iter().map(|kind| kind.as_str()).collect();
        let (last, rest) = names.split_last().expect("the codec table is never empty");
        format!("{} or {last}", rest.join(", "))
    }

    /// Codec of a manifest without a `codec` field: always Parquet
    /// (ADR-0050 §2), independent of the build's new-store default.
    pub(crate) fn absent() -> Self {
        Self::Parquet
    }

    /// The store-relative directory holding this codec's WAL segments:
    /// `wal/` for the classic codecs, `.cypherlite/wal/` for a markdown
    /// store — the engine sidecar lives under `.cypherlite/` beside the
    /// vault's own files (ADR-0406 §3).
    pub const fn wal_dir(self) -> &'static str {
        match self {
            Self::Parquet | Self::Json => "wal",
            Self::Markdown => super::markdown::WAL_DIR,
        }
    }

    /// The store-relative file this codec publishes its checkpoint
    /// envelope to: `manifest.json` (Parquet, ADR-0050), `graph.json`
    /// (JSON), and the `.cypherlite/manifest.json` sidecar a markdown
    /// store publishes last (ADR-0406 §3). The classic arms are spelled
    /// through the same constants their writers consult
    /// ([`crate::snapshot::MANIFEST_JSON`], [`super::json::GRAPH_JSON`]),
    /// so every arm of this one typed layout fact derives from the
    /// vocabulary its consumers pin.
    pub const fn checkpoint_image(self) -> &'static str {
        match self {
            Self::Parquet => crate::snapshot::MANIFEST_JSON,
            Self::Json => super::json::GRAPH_JSON,
            Self::Markdown => super::markdown::MANIFEST,
        }
    }

    /// The store-relative directory holding this codec's engine state
    /// (CDC slots): the markdown codec's hidden `.cypherlite/`, or
    /// `None` — never a sentinel string — for the snapshot-file codecs,
    /// whose engine state lives at the store root. Beside
    /// [`wal_dir`](Self::wal_dir) and [`checkpoint_image`](Self::checkpoint_image):
    /// the one typed layout fact every engine-dir consumer consults.
    pub const fn engine_dir(self) -> Option<&'static str> {
        match self {
            Self::Markdown => Some(super::markdown::ENGINE_DIR),
            Self::Parquet | Self::Json => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parquet => "parquet",
            Self::Json => "json",
            Self::Markdown => "markdown",
        }
    }

    pub fn parse_name(s: &str) -> Result<Self> {
        s.parse()
    }
}

impl FromStr for SnapshotCodecKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        // The inverse of `as_str`, derived from the one vocabulary table:
        // `ALL` searched by `as_str`, so a fourth codec edits the table
        // and `as_str` alone and this parse follows — no second name
        // match to keep in lockstep. The error quotes the caller's
        // original spelling.
        let name = s.trim().to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == name)
            .ok_or_else(|| {
                Error::storage(format!(
                    "unknown snapshot codec '{s}' (expected {})",
                    Self::expected_names(),
                ))
            })
    }
}

impl std::fmt::Display for SnapshotCodecKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
