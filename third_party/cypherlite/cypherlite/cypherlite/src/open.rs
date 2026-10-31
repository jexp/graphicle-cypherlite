//! The open path of [`Database`] (ADR-0371): `open`, `open_read_only`,
//! `open_with_wal_dir`, `open_with`, `open_vfs`, and `open_store` with its
//! resolve_* wiring — split out of lib.rs so that file stays under the
//! 1k-line budget (LABS-392). Inherent methods on `Database`; the public
//! paths are unchanged.

use std::path::PathBuf;
use std::sync::Arc;

use cypherlite_core::{Error, PreviewFeatures, Result};
use cypherlite_executor::{CsvPolicy, Graph};
use cypherlite_storage::{
    open_store_readonly_with_ontology, open_store_with_ontology, parse_uri_opening,
    SnapshotCodecKind, StoreTarget, Vfs, Wal,
};

use super::cdc;
use super::commit::commit_timing_env;
use super::ontology;
use super::open_config::{
    resolve_bulk_batch_limits, resolve_checkpoint_interval, resolve_memory_limit,
    resolve_preview_features, resolve_snapshot_codec, resolve_token_limits,
};
use super::shared::{OpenResolved, SharedOpenState};
use super::token_budget;
use super::{check_loaded_graph, Database, DurabilityMode, OpenMode, OpenOptions, Shared};

/// [`open_store_readonly_with_ontology`] plus the screen every consumer of a
/// loaded ontology document owes it: a stored document must parse before it
/// is trusted (a corrupt or hand-crafted record surfaces here rather than at
/// the first procedure that touches it). The open path and the Follow-mode
/// reload both load through this, so the invariant has one home.
pub(crate) fn open_store_readonly_screened(
    target: &StoreTarget,
) -> Result<(Graph, Wal, Option<String>)> {
    let (graph, wal, ontology) = open_store_readonly_with_ontology(target)?;
    ontology::validate_stored_document(ontology.as_deref())?;
    Ok((graph, wal, ontology))
}

/// The preflight-resolved open knobs, in one named carrier: the validated
/// LOAD CSV policy, the memory limit, the store's resolved preview set
/// (ADR-0371) and the requested snapshot codec. [`Database::open_preflight`]
/// resolves every knob before a store directory is leased.
/// [`Database::open_store`] installs the CSV policy, the memory limit and
/// the preview set; the snapshot codec is spent earlier, when `open_with` /
/// `open_vfs` build the `StoreTarget`.
struct OpenKnobs {
    csv_policy: CsvPolicy,
    logical_graph_size_limit: u64,
    preview: PreviewFeatures,
    snapshot_codec: Option<SnapshotCodecKind>,
}

impl Database {
    /// Open `:memory:`, a filesystem path, or `file://…`.
    /// Paths ending in `.cl` are a single-file bundle (ADR-0074);
    /// other paths are a Native directory. Always loads the snapshot then
    /// replays the WAL (`MemoryVfs` for `:memory:`).
    pub fn open(uri: &str) -> Result<Self> {
        Self::open_with(uri, OpenOptions::default())
    }

    /// Load an existing store once and refuse writes and checkpoints (ADR-0136).
    /// A missing store is not created.
    pub fn open_read_only(uri: &str) -> Result<Self> {
        Self::open_with(uri, OpenOptions::with_mode(OpenMode::ReadOnly))
    }

    /// Test hook: the shared engine state, for `commit.rs` group-commit tests
    /// that build a `flush_group` batch by hand. Not part of the public API.
    #[cfg(test)]
    pub(crate) fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.inner)
    }

    /// `wal_dir` overrides the local WAL path for filesystem URIs (default
    /// `<dir>/wal`). Ignored for `:memory:`. `s3://` is rejected (ADR-0044).
    pub fn open_with_wal_dir(uri: &str, wal_dir: Option<PathBuf>) -> Result<Self> {
        Self::open_with(
            uri,
            OpenOptions {
                wal_dir,
                checkpoint_interval: None,
                durability_mode: DurabilityMode::Full,
                logical_graph_size_limit: None,
                snapshot_codec: None,
                load_csv_dir: None,
                load_csv_max_bytes: None,
                mode: OpenMode::Writable,
                token_admission: false,
                concurrent_logical_workspace_limit: None,
                follow_gap: Default::default(),
            },
        )
    }

    /// Open with WAL directory, checkpoint-timer, and memory-limit overrides
    /// (ADR-0041, ADR-0042).
    pub fn open_with(uri: &str, opts: OpenOptions) -> Result<Self> {
        // Reject an invalid LOAD CSV policy, memory limit, and preview
        // feature list before parse_uri_opening leases a directory. A
        // failed open must not leave that directory behind.
        let knobs = Self::open_preflight(&opts)?;
        let read_only = opts.mode.is_read_only();
        let store = parse_uri_opening(uri, opts.wal_dir.clone(), knobs.snapshot_codec, read_only)?;
        Self::open_store(store, opts, knobs)
    }

    /// Open on a caller-supplied [`Vfs`] (ADR-0145).
    ///
    /// Snapshot and WAL share `vfs` (prefix `wal`). `opts.wal_dir` is ignored.
    /// Follow is rejected because the store has no native directory.
    /// `snapshot_codec: None` uses [`SnapshotCodecKind::default`].
    pub fn open_vfs(vfs: Arc<dyn Vfs>, opts: OpenOptions) -> Result<Self> {
        let knobs = Self::open_preflight(&opts)?;
        let codec = knobs.snapshot_codec.unwrap_or_default();
        let store = StoreTarget::from_vfs_with_codec(vfs, codec)?;
        Self::open_store(store, opts, knobs)
    }

    /// LOAD CSV root and byte limit from `opts`, with `CYPHERLITE_LOAD_CSV_DIR`
    /// as the root fallback. Resolved before any filesystem open so a bad
    /// policy does not create the database directory — the fail-before-lease
    /// contract the preflight below upholds.
    fn load_csv_policy(opts: &OpenOptions) -> Result<CsvPolicy> {
        let root = opts
            .load_csv_dir
            .clone()
            .or_else(|| std::env::var_os("CYPHERLITE_LOAD_CSV_DIR").map(PathBuf::from));
        CsvPolicy::new(root, opts.load_csv_max_bytes)
    }

    /// The one fail-before-lease preflight both public entry points run:
    /// the caller-resolved knobs whose invalid values must reject the open
    /// before a store directory is leased or written (the failed-open
    /// invariant the CSV and memory-limit knobs established).
    fn open_preflight(opts: &OpenOptions) -> Result<OpenKnobs> {
        Ok(OpenKnobs {
            csv_policy: Self::load_csv_policy(opts)?,
            logical_graph_size_limit: resolve_memory_limit(opts)?,
            preview: resolve_preview_features()?,
            snapshot_codec: resolve_snapshot_codec(opts)?,
        })
    }

    fn open_store(store: StoreTarget, opts: OpenOptions, knobs: OpenKnobs) -> Result<Self> {
        let read_only = opts.mode.is_read_only();
        let follow = opts.mode.follows();
        // One writer lease (ADR-0136 / ADR-0440). A writable native open
        // moves it from the VFS onto `Shared` before loading, so `demote`
        // can drop it later. A follow or read-only open has none yet.
        #[cfg(not(target_arch = "wasm32"))]
        let writer_lease = if read_only {
            None
        } else {
            store.vfs().take_writer_lease()
        };
        if follow && store.filesystem_dir().is_none() {
            return Err(Error::semantic(
                "follow is only supported for native directory databases",
            ));
        }
        // Every field is named: a new `OpenKnobs` field must be handled here
        // (or explicitly discarded) instead of riding an unnoticed `..`.
        // `snapshot_codec` was spent before the store existed — it selected
        // the codec in `open_with`/`open_vfs`.
        let OpenKnobs {
            csv_policy,
            logical_graph_size_limit,
            preview,
            snapshot_codec: _,
        } = knobs;
        let (mut graph, mut wal, ontology) = if read_only && store.vfs().durable() {
            open_store_readonly_screened(&store)?
        } else {
            // The writable loader screens here, where the document is
            // produced: every loaded document must parse before it is
            // trusted, and collocating the screen with its loader keeps the
            // branch predicate in one place.
            let (graph, wal, ontology) = open_store_with_ontology(&store)?;
            ontology::validate_stored_document(ontology.as_deref())?;
            (graph, wal, ontology)
        };
        check_loaded_graph(&graph)?;
        graph.configure_load_csv(csv_policy);
        // ADR-0378: the store's resolved preview set lives on the same
        // store-level `StoreServices` bundle as the CSV policy, so every
        // namespace and session of the store gates (and lists) alike.
        graph.configure_preview(preview);
        // ADR-0120 §2/§4: the loaded document is the store's ontology
        // state. It drives the WAL segment magic explicitly (over the
        // scanned active segment's default); no document keeps today's
        // behavior exactly: MDBWAL01. The document stays the sole stored
        // source of truth — no `system_ontologies` graph is derived from
        // it (ADR-0229 §3) — but it still must parse: open surfaces a
        // corrupt or hand-crafted record here instead of deferring the
        // failure to the first procedure that touches it.
        wal.set_ontology_held(ontology.is_some());
        let interval = resolve_checkpoint_interval(&opts);
        let (bulk_batch_threshold, bulk_batch_budget) = resolve_bulk_batch_limits()?;
        let budget = token_budget::TokenBudget::new(resolve_token_limits(&opts)?);
        // The knob cluster, stated once: the open path resolves every knob
        // and hands `Shared::new` the whole `OpenResolved` value.
        let resolved = OpenResolved {
            read_only: opts.mode.is_read_only(),
            opened_as_follow: follow,
            durability_mode: opts.durability_mode,
            logical_graph_size_limit,
            budget,
            bulk_batch_threshold,
            bulk_batch_budget,
            checkpoint_interval: interval,
            follow_gap: opts.follow_gap,
        };
        let cdc = cdc::CdcState::load(store.vfs())?;
        let commit_timing = commit_timing_env();
        let db = Self {
            inner: Arc::new(Shared::new(
                graph,
                store,
                wal,
                SharedOpenState {
                    resolved,
                    cdc,
                    ontology,
                    commit_timing,
                },
            )?),
        };
        // Each start_* helper is the same gate `Database::promote` /
        // `demote` later reuse (ADR-0440): a writable open starts the
        // timers, a follow open starts the follow thread, and a `ReadOnly`
        // open — writable nor following — starts nothing.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(lease) = writer_lease {
            *db.inner.writer_lease.lock()? = Some(lease);
        }
        crate::resize::start_checkpoint_timer(&db.inner)?;
        crate::resize::start_durability_timer(&db.inner)?;
        crate::resize::start_follow_thread(&db.inner)?;
        Ok(db)
    }
}
