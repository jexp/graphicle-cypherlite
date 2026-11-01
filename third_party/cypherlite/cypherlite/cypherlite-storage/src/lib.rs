mod codec;
#[cfg(not(target_arch = "wasm32"))]
mod info;
mod manifest;
#[cfg(not(target_arch = "wasm32"))]
mod native_ownership;
mod open;
mod snapshot;
mod version;
mod vfs;
mod wal;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cypherlite_core::{Error, Result};
use cypherlite_executor::Graph;
#[cfg(not(target_arch = "wasm32"))]
use native_ownership::reset_native_with;
#[cfg(not(target_arch = "wasm32"))]
pub use native_ownership::{take_writer_lease, WriterLease};
#[cfg(not(target_arch = "wasm32"))]
#[doc(hidden)]
pub use native_ownership::{LOCK_MARKER, ROOT_ROLE, WAL_ROLE, WRITER_LEASE};

pub use codec::markdown::recorded_body_properties;
pub use codec::markdown::MarkdownBodyProperties;
pub use codec::SnapshotCodecKind;
pub use cypherlite_core::{cypher_to_value, value_to_cypher};
#[cfg(not(target_arch = "wasm32"))]
pub use info::{store_info, store_info_uri, StoreInfo, StoreLayout, StoreSnapshot};
pub use manifest::Manifest;
// Used by the crate's migration tests; open_store serves production.
#[cfg(test)]
pub(crate) use open::open_store_with_report;
pub use open::{
    load, load_with_wal, open_follower_wal, open_store, open_store_readonly, open_writer_wal,
};
#[doc(hidden)]
pub use open::{open_store_readonly_with_ontology, open_store_with_ontology};
#[cfg(parquet_codec)]
pub use snapshot::native::{window_width, DEFAULT_SHARD_TARGET_BYTES};
pub use snapshot::ShardPart;
pub use vfs::{
    atomic_write, is_bundle_path, read_to_vec, MemoryVfs, Vfs, VfsEntryKind, VfsFile, VfsMeta,
    VfsOpen,
};
#[cfg(not(target_arch = "wasm32"))]
pub use vfs::{BundleVfs, NativeVfs};
#[doc(hidden)]
pub use wal::MAGIC;
pub use wal::{
    commit_payload_reservation_bytes, AppendCommitStats, Wal, WalCatchUp, WalPreflight, WalRecord,
};

/// Codec-specific open-time configuration, one generic slot on
/// [`StoreTarget`] (ADR-0460): resolved alongside the codec at open and
/// spent by that codec's load and checkpoint only — `None`, the
/// default, is the recorded/sidecar state every codec falls back to.
/// One variant per codec that grows open-time knobs; the next codec
/// extends this enum, never the target's field list.
#[derive(Clone, Debug)]
pub enum CodecOpenConfig {
    /// The markdown vault's explicit body-property configuration
    /// (ADR-0460); the sidecar manifest's recorded one governs when
    /// absent.
    MarkdownBodies(codec::markdown::MarkdownBodyProperties),
}

/// The set-but-ignored error for a codec-specific configuration on a
/// store that does not open as its codec — the one spelling every
/// refusal site in this crate raises, keyed on the configuration's own
/// identity ([`CodecOpenConfig::describe`]). The `named` evidence names
/// a codec when a refusal site actually derived one (the request at the
/// pre-lease refusal, the resolved codec at the installer); `None`
/// states the fact it can prove: the store does not open as the
/// configuration's codec.
pub(crate) fn codec_config_mismatch_error(
    config: &CodecOpenConfig,
    named: Option<SnapshotCodecKind>,
) -> Error {
    let what = config.describe();
    let required = config.required_codec();
    match named {
        Some(codec) => Error::semantic(format!(
            "{what} are set but the store's snapshot codec is {codec}, not {required}; \
             the configuration would be ignored"
        )),
        None => Error::semantic(format!(
            "{what} are set but the store does not open as a {required} vault (no snapshot \
             codec was requested and the directory carries no {required} sidecar); the \
             configuration would be ignored"
        )),
    }
}

impl CodecOpenConfig {
    /// The codec this configuration requires — the one fact every
    /// refusal site consults, owned by the enum instead of hand-spelled
    /// at each consumer.
    pub(crate) fn required_codec(&self) -> SnapshotCodecKind {
        match self {
            CodecOpenConfig::MarkdownBodies(_) => SnapshotCodecKind::Markdown,
        }
    }

    /// The configuration's own validity — the payload's validation,
    /// owned by the enum so every install-or-refuse boundary runs it.
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            CodecOpenConfig::MarkdownBodies(bodies) => bodies.validate(),
        }
    }

    /// The configuration's own name in refusal messages — the variant
    /// owns its identity; no refusal site hardcodes it.
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            CodecOpenConfig::MarkdownBodies(_) => "markdown body properties",
        }
    }

    /// The one install-or-refuse sequence every boundary runs: the
    /// configuration's own validation, then the codec match against
    /// `opening` — the codec this open resolves. `named` is the codec
    /// the refusal may cite (the explicit request at the pre-lease
    /// refusal, the resolved codec at the installer); `None` when no
    /// codec was derived.
    pub(crate) fn ensure_matches(
        &self,
        opening: SnapshotCodecKind,
        named: Option<SnapshotCodecKind>,
    ) -> Result<()> {
        self.validate()?;
        if self.required_codec() != opening {
            return Err(codec_config_mismatch_error(self, named));
        }
        Ok(())
    }
}

/// Parquet snapshot + WAL on a [`Vfs`] (ADR-0072). `:memory:` is `MemoryVfs`.
#[derive(Clone)]
pub struct StoreTarget {
    pub(crate) vfs: Arc<dyn Vfs>,
    wal_vfs: Arc<dyn Vfs>,
    wal_prefix: String,
    native_root: Option<PathBuf>,
    wal_native: Option<PathBuf>,
    pub(crate) codec: SnapshotCodecKind,
    /// The resolved codec-specific open-time configuration
    /// ([`CodecOpenConfig`]), spent by the matching codec only.
    pub(crate) codec_config: Option<CodecOpenConfig>,
}

/// Message prefix of the [`Error::Storage`] an exclusive destination create
/// raises when the destination already exists (ADR-0161).
///
/// The publish path (clone/snapshot/restore) recognizes this prefix to leave
/// a pre-existing destination untouched: the exclusive create made nothing,
/// so there is nothing to remove. One shared constant keeps the producer and
/// the recognizer from drifting apart.
pub const DESTINATION_EXISTS: &str = "destination already exists";

/// The destination-already-exists storage error (ADR-0161), prefixed by
/// [`DESTINATION_EXISTS`].
pub(crate) fn destination_exists(path: &Path) -> Error {
    Error::storage(format!("{DESTINATION_EXISTS}: {}", path.display()))
}

/// `error` with its message rewritten by `f`, keeping its variant and so
/// its display prefix: the free-text messages of a failed storage read,
/// `Storage` and `Io`, are rewritten; every other variant keeps its payload
/// verbatim (ADR-0127 Decision 4). The one primitive under
/// [`storage_context`] and the version module's `with_version_context`:
/// where the context goes is the caller's choice, which variants take it is
/// decided here.
pub(crate) fn map_message(error: Error, f: impl FnOnce(String) -> String) -> Error {
    match error {
        Error::Storage(message) => Error::Storage(f(message)),
        Error::Io(message) => Error::Io(f(message)),
        other => other,
    }
}

/// `error` with `context`, naming what was being read, put before its
/// message ([`map_message`]).
pub(crate) fn storage_context(error: Error, context: impl std::fmt::Display) -> Error {
    map_message(error, |message| format!("{context}: {message}"))
}

/// How a filesystem URI opens its destination (ADR-0136 read-only,
/// ADR-0161 exclusive create). One enum, not two booleans: the meaningless
/// read-only-plus-exclusive-create combination is unrepresentable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenMode {
    /// Adopt an existing store or create a missing one.
    OpenOrCreate,
    /// Shared locks on an existing store; never creates (ADR-0136).
    ReadOnly,
    /// The destination must not exist; this call creates it (ADR-0161).
    CreateExclusive,
}

/// Whether `uri` names the in-memory store: `:memory:` or the empty string
/// — the same classification `parse_uri_inner` applies (ADR-0163 §5). Pure
/// string math, so it is available on every target like [`is_bundle_path`].
///
/// ```
/// use cypherlite_storage::is_memory_uri;
/// assert!(is_memory_uri(":memory:"));
/// assert!(is_memory_uri(""));
/// assert!(!is_memory_uri("./db"));
/// ```
pub fn is_memory_uri(uri: &str) -> bool {
    uri == ":memory:" || uri.is_empty()
}

pub fn parse_uri(uri: &str) -> Result<StoreTarget> {
    parse_uri_with_options(uri, None, None)
}

pub fn parse_uri_with_wal(uri: &str, wal_dir: Option<PathBuf>) -> Result<StoreTarget> {
    parse_uri_with_options(uri, wal_dir, None)
}

pub fn parse_uri_with_options(
    uri: &str,
    wal_dir: Option<PathBuf>,
    requested: Option<SnapshotCodecKind>,
) -> Result<StoreTarget> {
    parse_uri_opening(uri, wal_dir, requested, false)
}

/// [`parse_uri_opening`] plus a codec-specific open-time configuration
/// ([`CodecOpenConfig`], ADR-0460). The configuration installs — or is
/// refused — **before the native lease materializes any state**, so a
/// failed open on a fresh path leaves nothing behind; the markdown
/// vault's body-property configuration is the one cargo today.
pub fn parse_uri_opening_with_config(
    uri: &str,
    wal_dir: Option<PathBuf>,
    requested: Option<SnapshotCodecKind>,
    read_only: bool,
    config: Option<CodecOpenConfig>,
) -> Result<StoreTarget> {
    let mode = if read_only {
        OpenMode::ReadOnly
    } else {
        OpenMode::OpenOrCreate
    };
    parse_uri_inner(uri, wal_dir, requested, mode, config)
}

/// [`parse_uri_with_options`] plus a read-only open (ADR-0136).
///
/// `read_only` does not create a missing store and takes shared locks on an
/// existing native directory or `.cl` host file.
pub fn parse_uri_opening(
    uri: &str,
    wal_dir: Option<PathBuf>,
    requested: Option<SnapshotCodecKind>,
    read_only: bool,
) -> Result<StoreTarget> {
    parse_uri_opening_with_config(uri, wal_dir, requested, read_only, None)
}

/// [`parse_uri_with_options`] whose destination is created exclusively
/// (ADR-0161).
///
/// The destination must not exist: a `.cl` host file or native directory
/// already there — including one that races into the create window, or a
/// symlinked final component, which is never followed — fails with an
/// [`Error::Storage`] prefixed [`DESTINATION_EXISTS`] instead of being
/// adopted. Ordinary opens keep adopting existing stores.
pub fn parse_uri_creating(
    uri: &str,
    wal_dir: Option<PathBuf>,
    requested: Option<SnapshotCodecKind>,
) -> Result<StoreTarget> {
    parse_uri_creating_with_config(uri, wal_dir, requested, None)
}

/// [`parse_uri_creating`] plus a codec-specific open-time configuration
/// ([`CodecOpenConfig`]): installed — or refused — before the
/// destination's lease or bundle create, so a refused configuration
/// never created the destination it would have been ignored on.
pub fn parse_uri_creating_with_config(
    uri: &str,
    wal_dir: Option<PathBuf>,
    requested: Option<SnapshotCodecKind>,
    config: Option<CodecOpenConfig>,
) -> Result<StoreTarget> {
    parse_uri_inner(uri, wal_dir, requested, OpenMode::CreateExclusive, config)
}

fn parse_uri_inner(
    uri: &str,
    wal_dir: Option<PathBuf>,
    requested: Option<SnapshotCodecKind>,
    mode: OpenMode,
    config: Option<CodecOpenConfig>,
) -> Result<StoreTarget> {
    if is_memory_uri(uri) {
        // No lease to protect; the installer's own refusal covers a
        // mismatched configuration.
        let store = memory_store_with_codec(requested.unwrap_or_default())?;
        return install_config(store, config);
    }
    if let Some(codec) = requested {
        codec::check_available(codec)?;
    }
    reject_remote_scheme(uri)?;
    parse_filesystem_uri(uri, wal_dir, requested, mode, config)
}

/// The post-construction install half of the config threading: the
/// [`StoreTarget::with_codec_config`] validator (a mismatch is already
/// impossible where the pre-lease refusal ran).
fn install_config(store: StoreTarget, config: Option<CodecOpenConfig>) -> Result<StoreTarget> {
    match config {
        Some(config) => store.with_codec_config(config),
        None => Ok(store),
    }
}

// wasm32 has no filesystem access; only `:memory:` opens.
#[cfg(target_arch = "wasm32")]
const WASM_FS_UNAVAILABLE: &str = "filesystem URIs are not available in wasm; use :memory:";

#[cfg(target_arch = "wasm32")]
fn parse_filesystem_uri(
    _uri: &str,
    _wal_dir: Option<PathBuf>,
    _requested: Option<SnapshotCodecKind>,
    _mode: OpenMode,
    _config: Option<CodecOpenConfig>,
) -> Result<StoreTarget> {
    Err(Error::storage(WASM_FS_UNAVAILABLE))
}

#[cfg(not(target_arch = "wasm32"))]
fn parse_filesystem_uri(
    uri: &str,
    wal_dir: Option<PathBuf>,
    requested: Option<SnapshotCodecKind>,
    mode: OpenMode,
    config: Option<CodecOpenConfig>,
) -> Result<StoreTarget> {
    let path = PathBuf::from(filesystem_uri_path(uri));
    if is_bundle_path(&path) {
        if wal_dir.is_some() {
            return Err(Error::storage(
                "wal_dir is not supported for .cl single-file databases (ADR-0074)",
            ));
        }
        // A bundle this open will create resolves the requested codec or
        // the build default, so a markdown-only codec configuration is
        // decidable — and refused — before the create materializes the
        // bundle, the same pre-create contract the native arm's
        // pre-lease refusal below upholds. An existing bundle keeps the
        // post-resolution install: its store, not this decision, names
        // the codec.
        let creates_bundle = match mode {
            OpenMode::CreateExclusive => true,
            OpenMode::OpenOrCreate => !path.exists(),
            OpenMode::ReadOnly => false,
        };
        if creates_bundle {
            if let Some(config) = &config {
                refuse_codec_config_mismatch(
                    config,
                    markdown_wal_hint(&path, requested),
                    requested,
                )?;
            }
        }
        let bundle = match mode {
            OpenMode::OpenOrCreate => BundleVfs::open_or_create(&path)?,
            OpenMode::ReadOnly => BundleVfs::open_read_only(&path)?,
            OpenMode::CreateExclusive => BundleVfs::open_create_exclusive(&path)?,
        };
        let vfs: Arc<dyn Vfs> = Arc::new(bundle);
        let mut target = on_vfs(vfs, SnapshotCodecKind::Parquet);
        target.codec = codec::resolve_codec(&target, requested)?;
        // No native lease behind a bundle; the installer's own refusal
        // covers a mismatched configuration (a created-exclusive bundle
        // is removed by the publish path's cleanup).
        return install_config(target, config);
    }
    // A markdown store's engine state lives under `.cypherlite/`
    // (ADR-0406 §3), so its default WAL is `<vault>/.cypherlite/wal`.
    // The codec is probed from the requested kind or the sidecar
    // manifest before the lease; `resolve_codec` below stays
    // authoritative for conflicts.
    // An explicit markdown request must not create engine state (the
    // lease below materializes a WAL directory and its role marker —
    // the default under `.cypherlite/`, or a caller-supplied `wal_dir`)
    // inside a directory that already carries a classic codec's root
    // evidence — refuse before the lease, whichever WAL directory the
    // open would use; `resolve_codec` below stays authoritative for
    // every other conflict shape. A sidecar-driven hint is never
    // refused here: the sidecar probe already classified the directory
    // fail-closed.
    // One hint decision, derived once: the codec whose default-WAL
    // layout this open takes and whose verdict the codec-config refusal
    // below consults. The markdown-over-classic evidence refusal keeps
    // its own gate — it refuses only an *explicit* markdown request
    // over classic evidence, a narrower fact than the hint. The bundle
    // create-check above reads the hint through the same fn.
    // `resolve_codec` after the lease stays authoritative for every
    // other conflict shape.
    let hint = markdown_wal_hint(&path, requested);
    if requested == Some(SnapshotCodecKind::Markdown) {
        refuse_markdown_over_classic_evidence(&path)?;
    }
    // The codec-specific open-time configuration installs — or is
    // refused — before the lease below materializes a WAL directory and
    // its role marker, so a refused open on a fresh path leaves nothing
    // behind (the fail-before-lease contract the CSV and memory-limit
    // knobs established).
    if let Some(config) = &config {
        refuse_codec_config_mismatch(config, hint, requested)?;
    }
    let wal_path = match wal_dir {
        Some(dir) => dir,
        // The hint codec's `wal_dir` owns the directory layout for both
        // arms (ADR-0406 §3).
        None => path.join(hint.wal_dir()),
    };
    let (dir, wal, lease) = match mode {
        OpenMode::OpenOrCreate => native_ownership::lease(&path, &wal_path)?,
        OpenMode::ReadOnly => native_ownership::lease_readonly(&path, &wal_path)?,
        OpenMode::CreateExclusive => native_ownership::lease_creating(&path, &wal_path)?,
    };
    let vfs: Arc<dyn Vfs> = Arc::new(NativeVfs::with_lease(dir.clone(), Arc::clone(&lease)));
    // The default root/wal can be an alias. Never resolve that alias again
    // after leasing its canonical target, including on WAL open and rotation.
    let wal_vfs: Arc<dyn Vfs> = Arc::new(NativeVfs::with_lease(wal.clone(), lease));
    let mut target = StoreTarget {
        vfs,
        wal_vfs,
        wal_prefix: String::new(),
        native_root: Some(dir),
        wal_native: Some(wal),
        codec: SnapshotCodecKind::Parquet,
        codec_config: None,
    };
    target.codec = codec::resolve_codec(&target, requested)?;
    install_config(target, config)
}

/// The pre-materialization half of the codec-config threading: refuse a
/// configuration whose codec does not match the codec this open will
/// resolve, judged on the caller's derived [`markdown_wal_hint`] verdict
/// — never a re-probe. The error names the codec only where it was
/// actually derived (an explicit request); with none requested it states
/// the fact it can prove: the store does not open as a markdown vault.
/// Never refuses a sidecar-classified markdown directory.
#[cfg(not(target_arch = "wasm32"))]
fn refuse_codec_config_mismatch(
    config: &CodecOpenConfig,
    hint: SnapshotCodecKind,
    requested: Option<SnapshotCodecKind>,
) -> Result<()> {
    // The one install-or-refuse sequence, with the hint as the codec
    // this open resolves and the request as the namable evidence.
    config.ensure_matches(hint, requested)
}
/// The codec whose default-WAL layout the directory at `path` takes,
/// for the pre-lease default-WAL routing: [`SnapshotCodecKind::Markdown`]
/// when the open is markdown — requested, or an engine-written sidecar
/// says so — otherwise [`SnapshotCodecKind::Json`], standing for the
/// classic codecs' shared `wal/` layout (`wal_dir` maps both to the same
/// directory, and Json is the no-evidence default). A folder of markdown
/// files alone is not a store until it is opened as one. (The reset
/// guard is the probe's other consumer, through
/// `codec::markdown::refuse_reset` → `probe_path`, manifest.rs.)
#[cfg(not(target_arch = "wasm32"))]
fn markdown_wal_hint(path: &Path, requested: Option<SnapshotCodecKind>) -> SnapshotCodecKind {
    // The sidecar classification — including the present-but-unparseable
    // fail-closed verdict — is the codec's one typed probe.
    if requested == Some(SnapshotCodecKind::Markdown) || codec::markdown::probe_path(path) {
        SnapshotCodecKind::Markdown
    } else {
        SnapshotCodecKind::Json
    }
}

/// The explicit-markdown pre-lease guard: the directory carries a
/// classic codec's root-level store evidence (`graph.json`, a Parquet
/// object family), so routing the default WAL into `.cypherlite/` and
/// leasing it would write engine state into another codec's store
/// before `resolve_codec` could refuse the mismatched request.
#[cfg(not(target_arch = "wasm32"))]
fn refuse_markdown_over_classic_evidence(path: &Path) -> Result<()> {
    if crate::snapshot::has_classic_store_evidence_at(path) {
        return Err(Error::storage(
            "mixed snapshot codecs: the directory carries snapshot-file evidence; an \
             explicit markdown open would create WAL engine state inside it",
        ));
    }
    Ok(())
}

pub fn memory_store() -> StoreTarget {
    memory_target(SnapshotCodecKind::default())
}

/// `:memory:` store with an explicit codec. Fails closed with a `Storage`
/// error when `codec` is not compiled into this build (ADR-0105).
///
/// ```
/// use cypherlite_storage::{memory_store_with_codec, SnapshotCodecKind};
///
/// assert!(memory_store_with_codec(SnapshotCodecKind::Json).is_ok());
/// assert_eq!(
///     memory_store_with_codec(SnapshotCodecKind::Parquet).is_ok(),
///     SnapshotCodecKind::Parquet.is_compiled()
/// );
/// ```
pub fn memory_store_with_codec(codec: SnapshotCodecKind) -> Result<StoreTarget> {
    codec::check_available(codec)?;
    Ok(memory_target(codec))
}

fn memory_target(codec: SnapshotCodecKind) -> StoreTarget {
    on_vfs(Arc::new(MemoryVfs::new()), codec)
}

fn on_vfs(vfs: Arc<dyn Vfs>, codec: SnapshotCodecKind) -> StoreTarget {
    StoreTarget {
        wal_vfs: Arc::clone(&vfs),
        vfs,
        wal_prefix: "wal".into(),
        native_root: None,
        wal_native: None,
        codec,
        codec_config: None,
    }
}

pub fn wal_dir(target: &StoreTarget) -> Option<&Path> {
    target.wal_native.as_deref()
}

#[cfg(target_arch = "wasm32")]
pub fn adopt_native_default_wal(_root: &Path) -> Result<()> {
    Err(Error::storage(WASM_FS_UNAVAILABLE))
}

/// Explicit offline adoption of a legacy Native directory's direct `wal/`.
///
/// Call only after verifying that `root/wal` belongs to `root` and closing
/// both owners. Segment magic cannot distinguish a WAL from an independent
/// legacy root that used root == WAL. Requires a root marker or root snapshot
/// evidence and a physical direct WAL with segment magic; holds both exclusive
/// leases while syncing their stable role markers. No data bytes are moved or
/// replayed. Ordinary open and reset never make this ownership assertion.
/// See ADR-0090 and `docs/api/rust/spec.md`.
#[cfg(not(target_arch = "wasm32"))]
pub fn adopt_native_default_wal(root: &Path) -> Result<()> {
    native_ownership::adopt_default_wal(root)
}

impl StoreTarget {
    /// Snapshot and WAL on one caller-supplied [`Vfs`] (ADR-0145).
    ///
    /// The WAL prefix is `wal`. There is no native directory, so follow and
    /// `wal_dir` do not apply. The codec is [`SnapshotCodecKind::default`]
    /// (JSON on wasm32), which every build can write.
    pub fn from_vfs(vfs: Arc<dyn Vfs>) -> Self {
        on_vfs(vfs, SnapshotCodecKind::default())
    }

    /// [`from_vfs`](Self::from_vfs) with an explicit codec. A codec this build
    /// cannot write fails before the store is returned.
    pub fn from_vfs_with_codec(vfs: Arc<dyn Vfs>, codec: SnapshotCodecKind) -> Result<Self> {
        codec::check_available(codec)?;
        Ok(on_vfs(vfs, codec))
    }

    /// Install codec-specific open-time configuration
    /// ([`CodecOpenConfig`]). The one installer every codec knob passes
    /// through: the variant's own validation runs here, and a
    /// configuration whose codec does not match the store's is refused —
    /// a set-but-ignored knob is an error, never a silent no-op.
    pub fn with_codec_config(mut self, config: CodecOpenConfig) -> Result<Self> {
        config.ensure_matches(self.codec, Some(self.codec))?;
        self.codec_config = Some(config);
        Ok(self)
    }

    /// The markdown vault's explicit body-property configuration when
    /// this open carried one (ADR-0460) — the markdown codec's load and
    /// checkpoint read it here; `None` lets the sidecar manifest's
    /// recorded configuration govern.
    pub(crate) fn markdown_bodies(&self) -> Option<&codec::markdown::MarkdownBodyProperties> {
        match &self.codec_config {
            Some(CodecOpenConfig::MarkdownBodies(bodies)) => Some(bodies),
            None => None,
        }
    }

    /// In-process WAL (`:memory:`). No timed checkpoint (ADR-0041).
    pub fn wal_is_memory(&self) -> bool {
        !self.wal_vfs.durable()
    }

    /// Both the snapshot VFS and the WAL are durable, so migrate-on-open
    /// may rewrite the store (ADR-0127 Decision 7: `:memory:` never
    /// migrates).
    pub(crate) fn is_fully_durable(&self) -> bool {
        self.vfs.durable() && !self.wal_is_memory()
    }

    /// Filesystem database directory (`<data>/cdc/slots.json` lives here).
    pub fn filesystem_dir(&self) -> Option<&Path> {
        self.native_root.as_deref()
    }

    pub fn vfs(&self) -> Arc<dyn Vfs> {
        Arc::clone(&self.vfs)
    }

    pub(crate) fn wal_vfs(&self) -> (Arc<dyn Vfs>, &str) {
        (Arc::clone(&self.wal_vfs), self.wal_prefix.as_str())
    }

    /// The directory holding engine state (CDC slots) — the codec's
    /// one typed layout fact ([`SnapshotCodecKind::engine_dir`]).
    pub fn engine_dir(&self) -> Option<&'static str> {
        self.codec.engine_dir()
    }

    pub fn snapshot_codec(&self) -> SnapshotCodecKind {
        self.codec
    }
}

/// The one place a `file://` prefix is stripped: every filesystem-URI caller
/// (open, reset, cold info) shares it. Call only after the remote-scheme
/// check; a bare path passes through unchanged.
#[cfg(not(target_arch = "wasm32"))]
fn filesystem_uri_path(uri: &str) -> &str {
    uri.strip_prefix("file://").unwrap_or(uri)
}

/// The local filesystem path for a store URI, with the classification
/// folded in (so no caller re-reads the string): `Ok(None)` for a memory
/// URI, `Ok(Some(path))` for a filesystem path or `file://` URI, `Err` for
/// a remote scheme — the same classification `parse_uri_inner` applies at
/// open. Surface-side sibling files (the auth token, ADR-0232) resolve
/// through here.
#[cfg(not(target_arch = "wasm32"))]
pub fn local_uri_path(uri: &str) -> Result<Option<&str>> {
    if is_memory_uri(uri) {
        return Ok(None);
    }
    if let Some(scheme) = remote_scheme(uri) {
        return Err(Error::storage(unsupported_remote_uri(scheme)));
    }
    Ok(Some(filesystem_uri_path(uri)))
}

fn remote_scheme(uri: &str) -> Option<&str> {
    if let Some((scheme, _)) = uri.split_once("://") {
        return valid_scheme(scheme).filter(|s| !s.eq_ignore_ascii_case("file"));
    }
    // A `scheme:/path` typo (single slash) must never fall through to the
    // filesystem branch, where it would create a literal `s3:` directory
    // relative to the process cwd.
    let (head, rest) = uri.split_once(':')?;
    if head.is_empty() || head.contains('/') || head.contains('\\') {
        return None;
    }
    // Windows absolute paths start with a drive letter: a single ASCII alpha
    // character, `:`, then a path separator (`C:\db`, `C:/db`). That is a
    // filesystem path, never a URI scheme — the lib tests open tempdir paths
    // of this shape on the windows-latest CI leg.
    if head.len() == 1 && (rest.starts_with('\\') || rest.starts_with('/')) {
        return None;
    }
    valid_scheme(head).filter(|s| !s.eq_ignore_ascii_case("file"))
}

fn valid_scheme(scheme: &str) -> Option<&str> {
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    chars
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        .then_some(scheme)
}

fn unsupported_remote_uri(scheme: &str) -> String {
    if scheme.eq_ignore_ascii_case("s3") {
        "s3:// is not an engine URI (ADR-0044); object storage lives in memorydb-cloud".into()
    } else {
        format!(
            "unsupported URI scheme {scheme}:// (engine URIs are :memory:, a filesystem path, or file://)"
        )
    }
}

/// The one remote-scheme rejection: every filesystem-URI entry point — open
/// ([`parse_uri_inner`]), reset ([`reset_uri`]) and the cold store-info read
/// ([`crate::store_info_uri`]) — refuses `s3://`-style URIs with the same
/// typed error before touching the filesystem.
fn reject_remote_scheme(uri: &str) -> Result<()> {
    if let Some(scheme) = remote_scheme(uri) {
        return Err(Error::storage(unsupported_remote_uri(scheme)));
    }
    Ok(())
}

/// Delete snapshot files and the WAL for `uri`. No-op for `:memory:`.
///
/// Directory URIs stage the WAL before a separate root in private same-parent
/// tombstones; failed staging either restores both or leaves a fail-closed
/// recovery marker and reports retained data paths. A WAL tree overlapping the
/// root is refused unless it is the direct, physically contained `<root>/wal`.
/// `.cl` URIs unlink the bundle file (ADR-0074).
/// A final-component symlink or Windows reparse-point database URI is refused
/// with `Storage`; reset the physical path explicitly instead.
/// Remote schemes (`s3://`, …) error (ADR-0044).
pub fn reset_uri(uri: &str, wal_override: Option<&Path>) -> Result<()> {
    if is_memory_uri(uri) {
        return Ok(());
    }
    reject_remote_scheme(uri)?;
    reset_filesystem_uri(uri, wal_override)
}

#[cfg(target_arch = "wasm32")]
fn reset_filesystem_uri(_uri: &str, _wal_override: Option<&Path>) -> Result<()> {
    Err(Error::storage(WASM_FS_UNAVAILABLE))
}

#[cfg(not(target_arch = "wasm32"))]
fn reset_filesystem_uri(uri: &str, wal_override: Option<&Path>) -> Result<()> {
    let path = Path::new(filesystem_uri_path(uri));
    // A markdown store's root is a hand-authorable vault; the codec
    // refuses to reset it rather than deleting it (ADR-0406 §6).
    codec::markdown::refuse_reset(path)?;
    if is_bundle_path(path) {
        native_ownership::reject_reset_alias(path)?;
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
            Ok(_) => (),
        }
        let (locked_path, lease) = BundleVfs::lock_host(path, vfs::HostAdmit::Exclusive)?;
        native_ownership::verify_reset_identity(
            path,
            &locked_path,
            cypherlite_core::file_identity::of_file(&lease)?,
        )?;
        NativeVfs::check_identity(&lease, &locked_path)?;
        match std::fs::remove_file(locked_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        return Ok(());
    }
    reset_native_with(path, wal_override, |_| Ok(()))
}

pub fn checkpoint(target: &StoreTarget, graph: &Graph, ontology: Option<&str>) -> Result<()> {
    checkpoint_at(target, graph, 0, graph.commit_ts(), ontology)
}

pub fn checkpoint_with_lsn(
    target: &StoreTarget,
    graph: &Graph,
    checkpoint_lsn: u64,
    ontology: Option<&str>,
) -> Result<()> {
    checkpoint_at(target, graph, checkpoint_lsn, graph.commit_ts(), ontology)
}

/// Snapshot of rows visible at `snapshot_ts` (ADR-0020 / ADR-0022 / ADR-0050).
///
/// `ontology` is the store's current ontology document (ADR-0120), opaque
/// to storage: the Parquet codec publishes it as an immutable
/// `ontology/<128-bit-hex>.json` object selected by a v4 manifest, the JSON
/// codec inlines it in `graph.json` at `value_encoding` 3; `None` keeps
/// both codecs at the previous format.
pub fn checkpoint_at(
    target: &StoreTarget,
    graph: &Graph,
    checkpoint_lsn: u64,
    snapshot_ts: u64,
    ontology: Option<&str>,
) -> Result<()> {
    match target.codec {
        SnapshotCodecKind::Parquet => {
            snapshot::checkpoint_at(target, graph, checkpoint_lsn, snapshot_ts, ontology)
        }
        SnapshotCodecKind::Json => {
            codec::json::dump(target, graph, checkpoint_lsn, snapshot_ts, ontology)
        }
        SnapshotCodecKind::Markdown => {
            codec::markdown::checkpoint_at(target, graph, checkpoint_lsn, snapshot_ts, ontology)
        }
    }
}

/// Create an empty WAL positioned at `checkpoint_lsn` (ADR-0156).
///
/// A later open replays nothing and the next commit is `checkpoint_lsn + 1`.
/// The caller has already published a snapshot at that LSN.
pub fn init_empty_wal(target: &StoreTarget, checkpoint_lsn: u64) -> Result<()> {
    let (vfs, prefix) = target.wal_vfs();
    let _wal = Wal::open(vfs, prefix, checkpoint_lsn)?;
    Ok(())
}

pub fn snapshot_data_bytes(target: &StoreTarget) -> Result<u64> {
    match target.codec {
        SnapshotCodecKind::Parquet => snapshot::snapshot_data_bytes(target),
        SnapshotCodecKind::Json => codec::json::snapshot_data_bytes(target),
        SnapshotCodecKind::Markdown => codec::markdown::snapshot_data_bytes(target),
    }
}

/// Published checkpoint LSN, or 0 when the store has no manifest yet.
///
/// Reads the codec envelope (`codec`, `checkpoint_lsn`, `commit_ts`). A
/// Parquet manifest carries extra fields and a JSON manifest is only the
/// envelope, so both builds report the same LSN.
pub fn published_checkpoint_lsn(target: &StoreTarget) -> Result<u64> {
    Ok(codec::read_envelope(target)?
        .map(|env| env.checkpoint_lsn)
        .unwrap_or(0))
}

#[cfg(test)]
mod tests;
