//! Copy a database between any two stores at the LSN visible on a read-only
//! open (ADR-0156, ADR-0282).

use std::path::{Path, PathBuf};

use crate::publish::{publish_into, uri_path};
use crate::{Database, Error, OpenMode, OpenOptions, Result, SnapshotCodecKind};

/// What a clone reads from the source. The source is always opened read-only
/// and does not follow; the destination WAL is always the fresh empty WAL
/// published with the checkpoint.
#[derive(Debug, Clone, Default)]
pub struct CloneOptions {
    /// Source WAL directory. `None` uses `<source>/wal`. Valid only for a
    /// native directory source: a `.cl` bundle embeds its WAL, so a `wal_dir`
    /// naming a bundle source is rejected (ADR-0160, ADR-0282).
    pub wal_dir: Option<PathBuf>,
    /// Destination snapshot codec (ADR-0163). `None` (the default) keeps the
    /// source's codec; `Some(kind)` transcodes the published checkpoint to
    /// `kind`. A codec not compiled into this build is [`Error::Storage`]
    /// raised before the destination is created.
    pub to_snapshot_format: Option<SnapshotCodecKind>,
    /// The markdown vault's body-property configuration (ADR-0460) for a
    /// markdown destination: the published checkpoint renders node files
    /// with it and the destination sidecar records it, so later opens need
    /// no flag. `None` (the default) keeps the source's recorded
    /// configuration when the source is a markdown store. A set
    /// configuration on a non-markdown destination is [`Error::Semantic`]
    /// raised before the destination is created.
    pub markdown_body_properties: Option<crate::MarkdownBodyProperties>,
}

const SOURCE_STORE: &str = "clone source must be a native directory or .cl bundle path";
const DEST_STORE: &str = "clone destination must be a native directory or .cl bundle path";

impl Database {
    /// Copy a database into a new store (ADR-0156, ADR-0282).
    ///
    /// The format of each side is inferred from its path — a `.cl` suffix
    /// names a single-file bundle, anything else a native directory, the
    /// same classification [`Database::open`] applies — so the full
    /// dir/`.cl` matrix is covered with no format flag. Opens `source`
    /// frozen and read-only (the ADR-0136 safe path), then publishes that
    /// graph into a new `dest` with an empty WAL. The destination keeps the
    /// source's snapshot codec unless [`CloneOptions::to_snapshot_format`]
    /// names another (ADR-0163); a requested codec this build does not
    /// include is [`Error::Storage`] before `dest` is created. The source is
    /// not written and later commits are not followed. [`CloneOptions`]
    /// names the source WAL of a directory source; with a `.cl` source
    /// `wal_dir` is [`Error::Storage`] — a bundle embeds its WAL. `:memory:`
    /// is refused on both sides; a writer-held `.cl` source fails with
    /// [`Error::Locked`]. `dest` must not already exist (exclusive create,
    /// ADR-0161); a failure after `dest` is created removes it. The next
    /// commit on the clone is the loaded LSN plus one.
    pub fn clone(source: &str, dest: &str, opts: CloneOptions) -> Result<()> {
        if cypherlite_storage::is_memory_uri(source) {
            return Err(Error::storage(SOURCE_STORE));
        }
        if cypherlite_storage::is_memory_uri(dest) {
            return Err(Error::storage(DEST_STORE));
        }
        let source_db = Database::open_with(
            source,
            OpenOptions {
                wal_dir: opts.wal_dir.clone(),
                mode: OpenMode::ReadOnly,
                ..OpenOptions::default()
            },
        )?;
        // A directory destination must not be the source directory. A bundle
        // source has no `filesystem_dir`; cloning it onto its own path is
        // refused by the existing-destination check in publish.
        if let Some(source_dir) = source_db.inner.store.filesystem_dir() {
            reject_same_source(source_dir, uri_path(dest))?;
        }
        // The destination codec is resolved once, here, and travels
        // with its configuration — publish_into never re-derives it.
        let dest_codec = opts
            .to_snapshot_format
            .unwrap_or_else(|| source_db.inner.store.snapshot_codec());
        let codec_config = clone_codec_config(&opts, &source_db, dest_codec)?;
        publish_into(dest, &source_db, "clone", dest_codec, codec_config)
    }
}

/// The clone's codec-specific open-time configuration (ADR-0460), the
/// one home of the inheritance policy: an explicit
/// [`CloneOptions::markdown_body_properties`] wins; without one, a
/// markdown destination cloning a markdown source inherits the source's
/// recorded configuration, so a copy keeps the source's note layout
/// without a flag; anything else carries nothing and the destination's
/// own defaults govern.
fn clone_codec_config(
    opts: &CloneOptions,
    source_db: &Database,
    dest_codec: SnapshotCodecKind,
) -> Result<Option<cypherlite_storage::CodecOpenConfig>> {
    Ok(match &opts.markdown_body_properties {
        // An explicit configuration passes through unconditionally: the
        // storage create path owns the set-but-ignored refusal, raised
        // before the destination's lease or bundle create.
        Some(bodies) => Some(cypherlite_storage::CodecOpenConfig::MarkdownBodies(
            bodies.clone(),
        )),
        // The inheritance case: a markdown destination cloning a
        // markdown source keeps the source's recorded layout — a
        // transcode never synthesizes a configuration, and an
        // unreadable source sidecar fails the clone rather than
        // silently deciding the layout.
        None if dest_codec == SnapshotCodecKind::Markdown
            && source_db.inner.store.snapshot_codec() == SnapshotCodecKind::Markdown =>
        {
            Some(cypherlite_storage::CodecOpenConfig::MarkdownBodies(
                cypherlite_storage::recorded_body_properties(&source_db.inner.store)?,
            ))
        }
        None => None,
    })
}

/// Clone-specific pre-check: the destination must not be the source
/// directory, whether it already exists (canonicalize) or not (canonical
/// parent plus file name). Anything else is [`crate::publish::reject_destination`]'s
/// job. Fails closed like `reject_destination`: only a missing destination
/// is tolerated — a missing parent is refused with a clear
/// "parent does not exist" message, and any other canonicalize error on the
/// destination or its parent (permission denied, symlink loop, …)
/// propagates instead of falling through to a guess. Each match arm
/// resolves the destination's identity once, so "dest is the source" is
/// spelled by the single comparison at the end.
fn reject_same_source(source_dir: &Path, dest: &Path) -> Result<()> {
    let dest_err = |err: std::io::Error| Error::storage(format!("clone destination: {err}"));
    let candidate = match std::fs::canonicalize(dest) {
        Ok(canon) => canon,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let Some(name) = dest.file_name() else {
                return Err(Error::storage("clone destination is not a valid path"));
            };
            let parent = dest
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let parent = std::fs::canonicalize(parent).map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    Error::storage(format!(
                        "clone destination parent does not exist: {}",
                        parent.display()
                    ))
                } else {
                    dest_err(err)
                }
            })?;
            parent.join(name)
        }
        Err(err) => return Err(dest_err(err)),
    };
    if candidate == source_dir {
        return Err(Error::storage("clone destination is the source database"));
    }
    Ok(())
}
