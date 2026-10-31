//! Embedded CypherLite: OpenCypher on an in-memory property graph.

mod binding;
mod cdc;
mod checkpoint;
mod clone;
mod commit;
mod commit_timing;
mod connection;
mod explain;
mod follow;
mod gate;
mod notifications;
mod ontology;
mod open;
mod open_config;
mod plan_node;
mod procedures;
mod profile;
mod publish;
mod query_worker;
mod resize;
mod result;
mod shared;
mod statement_notifications;
mod stats;
#[doc(hidden)]
pub mod test_opts;
#[cfg(test)]
mod tests;
mod token_budget;

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, RwLockReadGuard, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

pub use cypherlite_core::{
    node_element_id, parse_node_element_id, parse_rel_element_id, rel_element_id,
};
pub use cypherlite_storage::{cypher_to_value, value_to_cypher, SnapshotCodecKind, WalRecord};
pub use plan_node::{ClauseMetrics, PlanArtifacts, PlanNode, PlanWarning};

pub use binding::{
    binding_error_metadata, execute_wire, params_from_json, params_from_map, params_from_value,
    plan_wire_json, query_result_to_cypher_rows, query_result_to_wire, query_stats_to_wire,
    run_script_wire,
};
pub use cdc::CdcSlot;
pub use clone::CloneOptions;
pub use connection::{Connection, Executed, ExecutionFailure};
/// A cooperative cancellation flag for a running query (ADR-0111).
///
/// A transport (or any caller) holds a handle and calls [`CancelToken::cancel`]
/// while the query runs on another thread; the executor checks it at
/// row-production points and in long loops. A cancelled query returns
/// `Error::Cancelled`, whose message is `"query cancelled"`, and the
/// connection it ran on remains usable. Cloning shares the same flag.
///
/// ```
/// use cypherlite::CancelToken;
///
/// let token = CancelToken::new();
/// assert!(!token.is_cancelled());
/// let transport = token.clone();
/// transport.cancel();
/// assert!(token.is_cancelled());
/// ```
pub use cypherlite_core::CancelToken;
pub use cypherlite_core::Namespace;
pub use cypherlite_core::{
    parse_memory_limit, uuid_hyphenated, uuid_parse, ArithmeticFailure, Error, Node, Path,
    PropertyMap, Relationship, Result, Temporal, Value,
};
pub use cypherlite_core::{ExecutionReceipt, TokenEstimate, TokenOutcome};
pub use cypherlite_executor::{
    Procedure, PullKind, QueryStats, StartReason, StatementEffect, StoreReads, WalkStart,
};
/// The query-notification record and vocabulary (ADR-0363), with the three
/// wire serializers re-exported beside it so a consumer imports one crate:
/// [`notification_to_wire_json`], [`notifications_to_wire_json`] and
/// [`notification_to_neo4j_json`]. They remain defined in
/// `cypherlite_model::wire`, the one wire-encoding module every surface
/// shares.
pub use cypherlite_model::wire::{
    notification_to_neo4j_json, notification_to_wire_json, notifications_to_wire_json,
    Notification, NotificationCategory, NotificationKind, NotificationSeverity,
};
pub use cypherlite_planner::QueryType;
pub use query_worker::Heartbeat;
pub use result::{QueryResult, ResultHeader};
#[cfg(test)]
pub(crate) use shared::ReadPause;
pub(crate) use shared::{check_loaded_graph, ensure_writable, Shared};
pub use token_budget::TokenPoolSnapshot;

use checkpoint::checkpoint_now;
use commit::{format_commit_timing, CommitTimingBuf};
/// Open handle to a CypherLite database.
pub struct Database {
    inner: Arc<Shared>,
}

impl Drop for Database {
    fn drop(&mut self) {
        // Join before this `Arc` drops. A one-shot checkpoint keeps its own
        // `Arc` until it exits, and a timer that upgraded its `Weak` still
        // owns the store lock until then. Windows enforces that lock per
        // handle, so a reopen in this process has to wait here.
        self.inner.stop_background();
    }
}

/// Stop flag shared by the checkpoint, durability, and follow threads.
pub(crate) struct ThreadStop {
    pub(crate) stop: Mutex<bool>,
    pub(crate) cv: Condvar,
}

/// Park until `interval` elapses or `ctl` is signaled, then run `tick`.
/// wasm has no background threads, so this returns `Ok(None)` there.
pub(crate) fn spawn_interval_worker(
    name: &'static str,
    weak: Weak<Shared>,
    ctl: Arc<ThreadStop>,
    interval: Duration,
    tick: impl Fn(&Shared) + Send + 'static,
) -> Result<Option<JoinHandle<()>>> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (name, weak, ctl, interval, tick);
        Ok(None)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || loop {
                let Ok(stop) = ctl.stop.lock() else {
                    break;
                };
                if *stop {
                    break;
                }
                let (guard, _) = match ctl.cv.wait_timeout(stop, interval) {
                    Ok(waited) => waited,
                    Err(err) => err.into_inner(),
                };
                if *guard {
                    break;
                }
                drop(guard);
                let Some(shared) = weak.upgrade() else {
                    break;
                };
                tick(&shared);
            })
            .map(Some)
            .map_err(|err| Error::internal(format!("failed to spawn {name}: {err}")))
    }
}

impl ThreadStop {
    pub(crate) fn new() -> Self {
        Self {
            stop: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    pub(crate) fn signal(&self) {
        if let Ok(mut stop) = self.stop.lock() {
            *stop = true;
            self.cv.notify_all();
        }
    }
}

/// Seconds between timed checkpoints when the WAL has work (ADR-0041).
pub const DEFAULT_CHECKPOINT_INTERVAL_SECS: u64 = 300;

/// When an acknowledged write becomes durable on persistent storage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DurabilityMode {
    /// Barrier the WAL before acknowledging each group commit.
    #[default]
    Full,
    /// Acknowledge after apply and barrier dirty WAL data about once per second.
    EverySecond,
    /// Acknowledge after apply; only completed checkpoints are durable.
    Snapshot,
}

impl std::fmt::Display for DurabilityMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Full => "full",
            Self::EverySecond => "everysecond",
            Self::Snapshot => "snapshot",
        })
    }
}

impl FromStr for DurabilityMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Ok(Self::Full),
            "everysecond" => Ok(Self::EverySecond),
            "snapshot" => Ok(Self::Snapshot),
            other => Err(Error::semantic(format!(
                "invalid durability mode {other:?}; expected full, everysecond, or snapshot"
            ))),
        }
    }
}

/// Options for [`Database::open_with`].
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Local WAL directory. Ignored for `:memory:`. Filesystem URIs default
    /// to `<dir>/wal`.
    pub wal_dir: Option<PathBuf>,
    /// Timer period. `None` uses `CYPHERLITE_CHECKPOINT_INTERVAL_SECS` (default
    /// 300). `Some(Duration::ZERO)` disables the timer.
    pub checkpoint_interval: Option<Duration>,
    /// Commit durability policy. Defaults to [`DurabilityMode::Full`].
    pub durability_mode: DurabilityMode,
    /// Live graph + index cap in bytes (ADR-0042). `Some(0)` is unlimited.
    /// `None` uses `CYPHERLITE_LOGICAL_GRAPH_SIZE_LIMIT` (`SIZE`: bytes or `mb`/`mib`/`gb`/`gib`)
    /// or unlimited when that variable is unset. A set value that does not
    /// parse fails open with the same semantic error as a bad `--logical-graph-size-limit`.
    pub logical_graph_size_limit: Option<u64>,
    /// Opt-in peak-token admission. `CYPHERLITE_TOKEN_ADMISSION=1` enables it
    /// and `=0` disables it, including when this field is true. Unset follows
    /// this field. Off, queries are not refused for overlap.
    pub token_admission: bool,
    /// Concurrent peak tokens. Required and positive when admission is on.
    /// `None` reads `CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT`.
    pub concurrent_logical_workspace_limit: Option<u64>,
    /// Snapshot codec for an empty URI (ADR-0050). `None` uses
    /// `CYPHERLITE_SNAPSHOT_FORMAT` or the build's default,
    /// [`SnapshotCodecKind::default`] (Parquet with the `parquet` feature,
    /// JSON otherwise; ADR-0105). Existing directories use the envelope; a
    /// mismatch is an error.
    pub snapshot_codec: Option<cypherlite_storage::SnapshotCodecKind>,
    /// Existing directory allowed for local LOAD CSV (ADR-0081). `None` uses
    /// `CYPHERLITE_LOAD_CSV_DIR` or denies local files.
    pub load_csv_dir: Option<PathBuf>,
    /// Maximum bytes read per CSV source. `None` defaults to 8 MiB; valid
    /// overrides are 1 through 64 MiB. Does not enable file access by itself.
    pub load_csv_max_bytes: Option<u64>,
    /// Writable, frozen read-only, or a read-only WAL follower (ADR-0136,
    /// ADR-0137). [`OpenMode::Follow`] is read-only. Follow is refused for
    /// `:memory:` and `.cl`.
    pub mode: OpenMode,
    /// What a follower does when a checkpoint or trim removes WAL records
    /// it has not applied (ADR-0441). [`FollowGapPolicy::Reload`] loads a
    /// replacement graph beside the live one. [`FollowGapPolicy::Yield`]
    /// latches [`Error::FOLLOW_GAP`] and leaves the new graph to the host.
    pub follow_gap: FollowGapPolicy,
}

/// How a WAL follower recovers when it cannot keep applying (ADR-0441).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FollowGapPolicy {
    /// Load the snapshot and surviving WAL beside the installed graph, then
    /// swap (ADR-0137).
    #[default]
    Reload,
    /// Latch a retryable `follow-gap` failure and stop the follow loop.
    /// The loaded graph stays; [`Database::needs_reopen`] reports the latch.
    Yield,
}

/// How [`Database::open_with`] attaches to a store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OpenMode {
    /// The writable owner. Checkpoints, appends, and takes the writer lease.
    #[default]
    Writable,
    /// Load once and refuse every mutation (ADR-0136). A native directory
    /// can be open beside the writable owner. A `.cl` bundle can be open
    /// only while no writer holds it. Does not create a missing store.
    ReadOnly,
    /// Read-only, then tail the native WAL. A gap reloads or yields according
    /// to [`OpenOptions::follow_gap`] (ADR-0137, ADR-0441).
    Follow,
}

impl OpenMode {
    /// Frozen reader or follower.
    pub fn is_read_only(self) -> bool {
        !matches!(self, Self::Writable)
    }

    /// WAL follower.
    pub fn follows(self) -> bool {
        matches!(self, Self::Follow)
    }
}

impl OpenOptions {
    /// Default options with `mode` set. [`OpenMode::Writable`] is [`Default`].
    pub fn with_mode(mode: OpenMode) -> Self {
        Self {
            mode,
            ..Default::default()
        }
    }
}

impl Database {
    pub fn connect(&self) -> Result<Connection> {
        self.connect_typed(Namespace::Neo4j)
    }

    /// Connect with a typed session namespace (ADR-0117 §2, ADR-0150): no
    /// name parsing — a typed [`Namespace`] always exists, so this cannot
    /// fail with `DatabaseNotFound`.
    ///
    /// ```
    /// # use cypherlite::{Database, Namespace};
    /// let db = Database::open(":memory:").unwrap();
    /// let conn = db.connect_typed(Namespace::SystemOntologies).unwrap();
    /// assert_eq!(conn.namespace(), Namespace::SystemOntologies);
    /// ```
    pub fn connect_typed(&self, namespace: Namespace) -> Result<Connection> {
        self.inner.check_healthy()?;
        Ok(connection::fresh(Arc::clone(&self.inner), namespace))
    }

    /// Connect with a session namespace (ADR-0117 §2): `neo4j`, `system` or
    /// `system_ontologies` (ADR-0116 §1). Names match case-insensitively;
    /// any other name fails with `DatabaseNotFound` and CE's live text
    /// `Graph not found: <name>` (ADR-0146).
    ///
    /// ```
    /// # use cypherlite::{Database, Namespace};
    /// let db = Database::open(":memory:").unwrap();
    /// let conn = db.connect_namespace("SYSTEM").unwrap();
    /// assert_eq!(conn.namespace(), Namespace::System);
    /// assert!(db.connect_namespace("movies").is_err());
    /// ```
    pub fn connect_namespace(&self, name: &str) -> Result<Connection> {
        let namespace = connection::parse_session_namespace(name)?;
        self.connect_typed(namespace)
    }

    pub fn checkpoint(&self) -> Result<()> {
        ensure_writable(&self.inner)?;
        checkpoint_now(&self.inner)
    }

    /// Checkpoint when this handle can write. A read-only or follow handle
    /// succeeds without writing (ADR-0136). [`Self::checkpoint`] still
    /// returns the read-only error.
    pub fn checkpoint_if_writable(&self) -> Result<()> {
        if self.is_read_only() {
            Ok(())
        } else {
            self.checkpoint()
        }
    }

    /// Bytes of the latest Parquet snapshot (`manifest.json` + parts). WAL excluded.
    pub fn snapshot_data_bytes(&self) -> Result<u64> {
        let _read = self.inner.read_guard()?;
        cypherlite_storage::snapshot_data_bytes(&self.inner.store)
    }

    /// Live graph + index logical bytes and the configured cap (ADR-0042).
    /// `None` limit means unlimited.
    ///
    /// A poisoned/fenced handle is a typed error rather than an apparent
    /// empty, unlimited graph (ADR-0099).
    pub fn memory_usage(&self) -> Result<(u64, Option<u64>)> {
        let _read = self.inner.read_guard()?;
        let used = self.inner.total_used_bytes();
        let limit = self.inner.resolved.logical_graph_size_limit;
        Ok((used, if limit == 0 { None } else { Some(limit) }))
    }

    /// Tokens reserved in the process pool. `None` when admission is off.
    /// This does not take the publication guard: the counter is atomic, and
    /// a sampler must be able to read it while the handle is busy.
    pub fn token_pool(&self) -> Option<TokenPoolSnapshot> {
        self.inner.resolved.budget.snapshot()
    }

    /// Invoke `hook` on the query thread when a reservation crosses from the
    /// admission ceiling into the tail. One call per crossing. Admission off
    /// leaves the hook unset.
    pub fn on_token_ceiling(&self, hook: impl Fn(TokenPoolSnapshot) + Send + Sync + 'static) {
        self.inner.resolved.budget.on_ceiling(hook);
    }

    /// Register a TCK / user procedure visible to subsequent queries.
    pub fn register_procedure(&self, proc: Procedure) -> Result<()> {
        let _read = self.inner.read_guard()?;
        self.inner.graph.register_procedure(proc);
        Ok(())
    }

    #[doc(hidden)]
    pub fn enable_commit_timing(&self) {
        self.inner.timing_on.store(true, Ordering::Relaxed);
    }

    /// Drop samples so graph load is not mixed with a later workload.
    /// No-op unless `CYPHERLITE_COMMIT_TIMING=1`.
    #[doc(hidden)]
    pub fn clear_commit_timing(&self) {
        if let Ok(mut buf) = self.inner.timing.lock() {
            *buf = CommitTimingBuf::default();
        }
    }

    /// Histogram of commit steps (encode+write, `sync_all`, `apply_redo`, lock waits).
    /// Empty unless `CYPHERLITE_COMMIT_TIMING=1`.
    #[doc(hidden)]
    pub fn commit_timing_report(&self) -> String {
        self.inner.timing_dumped.store(true, Ordering::Relaxed);
        match self.inner.timing.lock() {
            Ok(buf) => format_commit_timing(&buf),
            Err(_) => String::new(),
        }
    }

    /// Whether this handle is read-only right now. A `ReadOnly` open
    /// always is. A `Follow` or `Writable` open is, after [`Self::demote`],
    /// until [`Self::promote`] takes the writer lease again (ADR-0440).
    pub fn is_read_only(&self) -> bool {
        !self.inner.writable.load(Ordering::Acquire)
    }

    /// Whether this handle was opened [`OpenMode::Follow`] (ADR-0440).
    /// A `Writable` open is already the writer and `promote` on it is a
    /// no-op; after `demote` that same handle can `promote` again. A
    /// `ReadOnly` open can never become writable.
    pub fn is_follower(&self) -> bool {
        self.inner.resolved.opened_as_follow
    }

    /// Whether a [`FollowGapPolicy::Yield`] follower has hit a gap or a torn
    /// apply and is waiting for the host to drop this engine and open
    /// another (ADR-0441). New queries fail with a retryable
    /// [`Error::FOLLOW_GAP`] until that happens. [`FollowGapPolicy::Reload`]
    /// never sets this: it installs the replacement graph itself.
    pub fn needs_reopen(&self) -> bool {
        self.inner.needs_reopen()
    }

    /// Drop without checkpointing (simulates a crash after WAL fsync).
    pub fn leak_without_checkpoint(self) {
        self.inner.persist_on_drop.store(false, Ordering::Release);
        // Keep the WAL until the last Connection is gone, but never leak the
        // ownership lease indefinitely in this crash-simulation hook.
        drop(self);
    }

    /// Register a CDC slot at the current WAL LSN (ADR-0045). Idempotent.
    ///
    /// ```
    /// # use cypherlite::Database;
    /// let db = Database::open(":memory:").unwrap();
    /// let slot = db.register_cdc_slot("primary").unwrap();
    /// assert_eq!(slot.name, "primary");
    /// assert_eq!(slot.acked_lsn, 0);
    /// ```
    pub fn register_cdc_slot(&self, name: &str) -> Result<CdcSlot> {
        let (_read, wal_lsn) = self.cdc_slot_admission()?;
        let mut cdc = self.inner.cdc.lock()?;
        cdc.register(name, wal_lsn)
    }

    /// Advance a slot's acked LSN after the consumer has persisted the batch.
    pub fn ack_cdc_slot(&self, name: &str, lsn: u64) -> Result<CdcSlot> {
        let (_read, wal_lsn) = self.cdc_slot_admission()?;
        let mut cdc = self.inner.cdc.lock()?;
        cdc.ack(name, lsn, wal_lsn)
    }

    /// The one admission protocol for a slot mutation that stamps a WAL LSN:
    /// the writable check, then a publication read guard that is held across
    /// the LSN snapshot and the caller's `cdc` lock, so the stamp cannot race
    /// a concurrent commit or a poison publication (ADR-0045).
    fn cdc_slot_admission(&self) -> Result<(RwLockReadGuard<'_, ()>, u64)> {
        ensure_writable(&self.inner)?;
        let read = self.inner.read_guard()?;
        // Re-check under the publication read guard. `demote` flips
        // `writable` while holding the publication write lock, so a slot
        // mutation that passed the check above cannot run after the lease
        // is released.
        ensure_writable(&self.inner)?;
        let wal_lsn = self.inner.wal.lock()?.lsn;
        Ok((read, wal_lsn))
    }

    /// Drop a slot so the next checkpoint can recycle as today.
    pub fn drop_cdc_slot(&self, name: &str) -> Result<bool> {
        ensure_writable(&self.inner)?;
        let _read = self.inner.read_guard()?;
        ensure_writable(&self.inner)?;
        let mut cdc = self.inner.cdc.lock()?;
        cdc.drop_slot(name)
    }

    pub fn cdc_slots(&self) -> Result<Vec<CdcSlot>> {
        let _read = self.inner.read_guard()?;
        let cdc = self.inner.cdc.lock()?;
        Ok(cdc.list())
    }

    /// WAL records with `lsn > after`, at most `limit` (ADR-0045).
    pub fn cdc_records_after(&self, after: u64, limit: usize) -> Result<Vec<WalRecord>> {
        let _read = self.inner.read_guard()?;
        let mut wal = self.inner.wal.lock()?;
        wal.records_after(after, limit)
    }

    /// Fence Cypher writes in `try_wal_commit`. Persisted on disk (ADR-0045).
    pub fn set_apply_only(&self, on: bool) -> Result<()> {
        ensure_writable(&self.inner)?;
        let _read = self.inner.read_guard()?;
        ensure_writable(&self.inner)?;
        let mut cdc = self.inner.cdc.lock()?;
        cdc.set_apply_only(on)
    }

    pub fn apply_only(&self) -> Result<bool> {
        let _read = self.inner.read_guard()?;
        let cdc = self.inner.cdc.lock()?;
        Ok(cdc.apply_only())
    }

    /// Replica write path: validate redo IDs and complete named label-existence
    /// enforcement metadata, append to this WAL, then
    /// `apply_redo` (ADR-0045). Assigns a new LSN; keeps the source `commit_ts`
    /// and graph IDs. Created IDs and relationship endpoints outside
    /// `0..i64::MAX` return `Error::Storage` without appending.
    pub fn apply_replicated(&self, record: &WalRecord) -> Result<u64> {
        commit::commit_replicated(&self.inner, record)
    }
}
