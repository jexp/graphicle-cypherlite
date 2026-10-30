//! The CypherLite-backed core of the store: the graph engine the `Store`
//! façade is being re-cored onto (see `docs/store-api.md` for the façade
//! contract the module rewrites code against, and
//! `docs/spike-cypherlite.md` for the verified engine behaviour).
//!
//! [`GraphStore`] holds the `cypherlite::Database` (Arc-shared, `Send +
//! Sync`) and hands out per-thread [`Connection`]s. A `Connection` is `Send`
//! but `!Sync`, takes `&mut self` per query and spawns a 64 MiB worker
//! thread on first use — check one out per worker thread and keep it, never
//! one per request.
use crate::{Error, Result};
use cypherlite::{Connection, Database, PropertyMap, Value};
use serde_json::{Map, Value as Json};
use std::{
    collections::BTreeMap,
    panic::Location,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Write-write conflict retries of [`GraphStore::transaction`] (optimistic
/// commit; the whole closure reruns).
pub const MAX_WRITE_ATTEMPTS: u32 = 3;
/// A write transaction held longer than this is logged with its call site.
const SLOW_HOLD: Duration = Duration::from_secs(2);

/// An open graph store. Cheap to clone; clones share one handle.
///
/// A durable store does NOT hold its engine open: the engine takes an
/// exclusive writer lease on the directory, and holding it would lock out
/// the other processes the SQL store always allowed beside an open store
/// (a `graphicle sync` next to a running `graphicle mcp`). Instead the
/// engine is open only while an operation holds it (`Weak`, upgraded per
/// use), and a contended (re)open waits behind the holder's lease, the
/// graph counterpart of the SQL store's busy timeout. An in-memory store
/// holds its engine for good — dropping it would lose the data.
#[derive(Clone)]
pub struct GraphStore {
    inner: Arc<StoreInner>,
}

/// Shared across clones; the store.rs registry keys on this (not the
/// `GraphStore` wrapper) so any live clone — which shares the pooled
/// connections pinning the engine's writer lease — keeps the registry
/// entry upgradeable and a same-process reopen never blocks on the lease
/// its own clones hold.
pub(crate) struct StoreInner {
    location: StoreLocation,
    /// Opened for reads only (shared locks, no writer lease, no schema
    /// DDL): a hook or CLI that reads beside a running writer.
    read_only: bool,
    /// The open engine while at least one operation holds it; dropped (and
    /// the writer lease released) when idle.
    engine: Mutex<std::sync::Weak<Database>>,
    /// Idle connections for reuse. A `Connection` spawns a 64 MiB query
    /// worker on first use and dropping it tears down the engine's shared
    /// state (`Arc<Shared>::drop_slow`), so `read`/`transaction` must never
    /// create one per call — they check one out here and return it. The pool
    /// lives and dies with the store handle, so the documented engine
    /// lifetime is unchanged: an idle store holds no engine (the pooled
    /// `Arc<Database>` keeps the engine open exactly while the store handle
    /// is held), and a dropped-and-reopened store starts with an empty
    /// pool — no stale connection can pin the old engine's lease.
    /// Entries are `(Connection, Arc<Database>)` so drop order matches the
    /// historical one: every connection first, the engine handle last.
    pool: Mutex<Vec<(Connection, Arc<Database>)>>,
}

enum StoreLocation {
    Memory(Arc<Database>),
    Path(PathBuf),
}

/// A contended engine open retries this long before failing (the SQL store's
/// busy timeout was 10 s; a projection commit can outlast that).
const LEASE_WAIT: Duration = Duration::from_secs(30);
/// Backoff between contended open attempts.
const LEASE_RETRY: Duration = Duration::from_millis(50);

impl GraphStore {
    /// Open (or create) the directory store at `path` and apply the schema
    /// (graph type + indexes) idempotently: a second open of the same store
    /// is a no-op.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.is_symlink() {
            return Err(Error::bad("Graph store cannot be a symlink"));
        }
        let store = Self {
            inner: Arc::new(StoreInner {
                location: StoreLocation::Path(path.to_path_buf()),
                read_only: false,
                engine: Mutex::new(std::sync::Weak::new()),
                pool: Mutex::new(Vec::new()),
            }),
        };
        store.apply_schema()?;
        Ok(store)
    }
    /// Open the directory store at `path` for reads only: shared locks, no
    /// writer lease, no schema DDL, no creation — an agent hook or CLI that
    /// only reads beside a running writer (which holds the lease). The
    /// engine still drops when idle and reopens per use, as with
    /// [`GraphStore::open`]. Errors when the store does not exist; a write
    /// through the handle fails in the engine.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.is_symlink() {
            return Err(Error::bad("Graph store cannot be a symlink"));
        }
        if path.to_str().is_none() {
            return Err(Error::bad("Graph store path is not valid UTF-8"));
        }
        Ok(Self {
            inner: Arc::new(StoreInner {
                location: StoreLocation::Path(path.to_path_buf()),
                read_only: true,
                engine: Mutex::new(std::sync::Weak::new()),
                pool: Mutex::new(Vec::new()),
            }),
        })
    }
    /// The shared state, for the store.rs handle registry
    /// ([`GraphStore::from_shared`]).
    pub(crate) fn shared(&self) -> Arc<StoreInner> {
        self.inner.clone()
    }
    /// A handle over shared state from the registry: a full clone (the
    /// pool and the engine slot are shared).
    pub(crate) fn from_shared(inner: Arc<StoreInner>) -> Self {
        Self { inner }
    }
    /// Whether the handle only reads (see [`GraphStore::open_read_only`]).
    pub fn is_read_only(&self) -> bool {
        self.inner.read_only
    }
    /// An ephemeral in-memory store (tests, transient processing). Applies
    /// the same schema as [`GraphStore::open`]. The engine is held for the
    /// handle's lifetime: dropping it would lose the data.
    pub fn open_in_memory() -> Result<Self> {
        let store = Self {
            inner: Arc::new(StoreInner {
                location: StoreLocation::Memory(Arc::new(Database::open(":memory:")?)),
                read_only: false,
                engine: Mutex::new(std::sync::Weak::new()),
                pool: Mutex::new(Vec::new()),
            }),
        };
        store.apply_schema()?;
        Ok(store)
    }
    /// The engine, open: the live one, else a fresh open. A durable store's
    /// open waits behind another process's (or handle's) writer lease for up
    /// to [`LEASE_WAIT`].
    fn engine(&self) -> Result<Arc<Database>> {
        match &self.inner.location {
            StoreLocation::Memory(database) => Ok(database.clone()),
            StoreLocation::Path(path) => {
                let uri = path
                    .to_str()
                    .ok_or_else(|| Error::bad("Graph store path is not valid UTF-8"))?;
                let deadline = Instant::now() + LEASE_WAIT;
                loop {
                    {
                        let mut slot = self
                            .inner
                            .engine
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if let Some(database) = slot.upgrade() {
                            return Ok(database);
                        }
                        let opened = if self.inner.read_only {
                            Database::open_read_only(uri)
                        } else {
                            Database::open(uri)
                        };
                        match opened {
                            Ok(database) => {
                                let database = Arc::new(database);
                                *slot = Arc::downgrade(&database);
                                return Ok(database);
                            }
                            Err(cypherlite::Error::Locked(_))
                                if Instant::now() < deadline =>
                            {
                                drop(slot);
                                std::thread::sleep(LEASE_RETRY);
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
            }
        }
    }
    /// The store's directory, or `None` for an in-memory store.
    pub fn path(&self) -> Option<&Path> {
        match &self.inner.location {
            StoreLocation::Memory(_) => None,
            StoreLocation::Path(path) => Some(path),
        }
    }
    /// The engine handle while the returned `Arc` is held (checkpoint,
    /// snapshot, clone). Idle stores hold no engine, so the caller's work
    /// keeps the lease until the `Arc` drops.
    pub fn database(&self) -> Arc<Database> {
        self.engine()
            .unwrap_or_else(|error| panic!("graph store engine unavailable: {}", error.message))
    }
    /// A connection for the calling thread. Keep it for the thread's
    /// lifetime; each connection spawns its query worker lazily. Prefer
    /// [`GraphStore::read`]/[`GraphStore::transaction`], which reuse the
    /// thread's pooled connection; this fresh handle is for callers that
    /// hold a connection across calls themselves.
    pub fn connection(&self) -> Result<Connection> {
        Ok(self.engine()?.connect()?)
    }
    /// Run `f` on a pooled connection, checking one out (or opening the
    /// engine and connecting, on an empty pool) and returning it to the pool
    /// afterwards. The pool lock is NOT held across `f`, so `f` may use
    /// other stores — or re-enter this one, getting a second pooled
    /// connection — without deadlocking.
    fn with_connection<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut entry = match self
            .inner
            .pool
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop()
        {
            Some(entry) => entry,
            None => {
                let engine = self.engine()?;
                let connection = engine.connect()?;
                (connection, engine)
            }
        };
        let result = f(&mut entry.0);
        self.inner
            .pool
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry);
        result
    }
    /// A write transaction: `begin`, run `f`, `commit`. A write-write
    /// conflict at commit reruns the whole closure, up to
    /// [`MAX_WRITE_ATTEMPTS`] attempts; any error rolls back (dropping an
    /// open transaction is an implicit rollback in the engine).
    /// `f` must therefore be side-effect-free outside the transaction and
    /// safe to rerun — it takes `Fn`, not `FnOnce`.
    #[track_caller]
    pub fn transaction<T>(&self, f: impl Fn(&mut Connection) -> Result<T>) -> Result<T> {
        let at = Location::caller();
        let mut attempt = 0;
        loop {
            attempt += 1;
            let started = Instant::now();
            let outcome = match self.with_connection(|db| {
                db.begin()?;
                Ok(match f(db) {
                    Ok(value) => match db.commit() {
                        Ok(()) => Attempt::Done(value),
                        Err(cypherlite::Error::Conflict) => {
                            // The connection is reused across attempts, so
                            // make sure no half-open transaction leaks into
                            // the retry's `begin` (a no-op if the engine
                            // already closed it).
                            let _ = db.rollback();
                            Attempt::Conflict
                        }
                        Err(error) => Attempt::Failed(error.into()),
                    },
                    Err(error) => {
                        let _ = db.rollback();
                        Attempt::Failed(error)
                    }
                })
            })? {
                Attempt::Conflict if attempt < MAX_WRITE_ATTEMPTS => continue,
                Attempt::Conflict => Err(cypherlite::Error::Conflict.into()),
                Attempt::Done(value) => Ok(value),
                Attempt::Failed(error) => Err(error),
            };
            let held = started.elapsed();
            if held >= SLOW_HOLD && crate::phase::enabled() {
                eprintln!(
                    "graphicle: a graph write transaction was held {:.1} s at {}:{}",
                    held.as_secs_f64(),
                    crate_relative(at.file()),
                    at.line()
                );
            }
            return outcome;
        }
    }
    /// A read: runs `f` on a pooled connection without
    /// opening a write transaction (reads pin a consistent snapshot in the
    /// engine).
    pub fn read<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        self.with_connection(f)
    }
    /// Flush the write-ahead log into a snapshot (maintenance/backup paths).
    pub fn checkpoint(&self) -> Result<()> {
        Ok(self.engine()?.checkpoint()?)
    }
    /// Apply the graph type and indexes. Runs at every open; safe to call
    /// again. Vector indexes are NOT created here — see
    /// [`GraphStore::ensure_vector_indexes`].
    pub fn apply_schema(&self) -> Result<()> {
        self.with_connection(|db| {
            // A store that earlier had the hand-written fallback constraints
            // on a graph-type-managed label: the graph type wins (the
            // mechanisms are either/or per label), so drop those first.
            for name in FALLBACK_CONSTRAINTS {
                db.query(&format!("DROP CONSTRAINT {name} IF EXISTS"))?;
            }
            db.query(GRAPH_TYPE)?;
            db.query(PROMOTE_GRAPH_TYPE)?;
            for ddl in INDEXES {
                db.query(ddl)?;
            }
            Ok(())
        })?;
        // Extension structures last: they may reference core labels. Stores
        // keep every label, so an edition without an extension ignores them.
        crate::extension::installed().migrate(self)
    }
    /// Create the vector indexes for `dimensions`, idempotently. Embedding
    /// dimensions are a property of the embedding model, which is only known
    /// once a store publishes vectors, so these indexes are created lazily by
    /// the vector pipeline rather than at open (a vector index fixes its
    /// dimension at creation). `dimensions` must be the model's; creating
    /// with a different dimension than an existing index is an engine error.
    pub fn ensure_vector_indexes(&self, dimensions: u32) -> Result<()> {
        self.with_connection(|db| {
            for (name, label, property) in VECTOR_INDEXES {
                db.query(&format!(
                    "CREATE VECTOR INDEX {name} IF NOT EXISTS FOR (n:{label}) ON (n.{property}) \
                     OPTIONS {{indexConfig: {{`vector.dimensions`: {dimensions}, \
                     `vector.similarity_function`: 'cosine'}}}}"
                ))?;
            }
            Ok(())
        })
    }
}

/// The outcome of one [`GraphStore::transaction`] attempt: committed,
/// write-write conflicted (retry), or failed.
enum Attempt<T> {
    Done(T),
    Conflict,
    Failed(Error),
}

/// Copy the graph store directory `source` to `dest` (which must not
/// exist). The engine clones from a frozen read-only open, which a live
/// writer does not block — backup and test forks use this without closing
/// the store.
pub fn clone_store(source: &Path, dest: &Path) -> Result<()> {
    let source = source
        .to_str()
        .ok_or_else(|| Error::bad("Graph store path is not valid UTF-8"))?;
    let dest = dest
        .to_str()
        .ok_or_else(|| Error::bad("Graph store path is not valid UTF-8"))?;
    Ok(cypherlite::Database::clone(
        source,
        dest,
        cypherlite::CloneOptions::default(),
    )?)
}

/// The backup sidecar of a SQL backup file (`backup.sqlite3` →
/// `backup.sqlite3.graph`).
pub fn backup_sidecar(backup: &Path) -> PathBuf {
    let mut sidecar = backup.as_os_str().to_owned();
    sidecar.push(".graph");
    PathBuf::from(sidecar)
}

/// Hand-written constraint names from the spike's fallback list that overlap
/// labels the graph type manages; dropped before promotion (the graph type
/// wins over hand-written constraints on the same label).
const FALLBACK_CONSTRAINTS: &[&str] = &[
    "ws_id", "cred_key", "proj_key", "obj_key", "mem_id", "job_key", "sym_key", "ep_key",
    "run_key", "del_key",
];

/// The store's graph type (migration plan §2.6), with the v0.12.2
/// corrections the spike verified: `=>` after the identifying label, `IS
/// <TYPE>` property declarations, `CONSTRAINT FOR … REQUIRE … IS KEY`
/// elements, and every id declared STRING (a declared UUID type rejects
/// `uuid()` values). Enforced once promoted; undeclared labels and
/// properties stay open.
const GRAPH_TYPE: &str = "CREATE GRAPH TYPE graphicle AS {
    (w:Workspace => {id IS STRING IS KEY, capability_hash IS STRING NOT NULL,
                     name IS STRING NOT NULL, created_at IS STRING NOT NULL, version IS INTEGER}),
    (c:Credential => {id IS STRING, capability_hash IS STRING IS KEY, role IS STRING,
                      created_at IS STRING, expires_at IS STRING, revoked_at IS STRING}),
    (p:Project => {id IS STRING, workspace_id IS STRING NOT NULL,
                   project_key IS STRING NOT NULL, name IS STRING, snapshot IS STRING,
                   generation IS INTEGER, metadata IS STRING}),
    (o:Object => {repository_id IS STRING NOT NULL, sha256 IS STRING NOT NULL,
                  content IS STRING, bytes IS INTEGER}),
    (s:Symbol => {repository_id IS STRING NOT NULL, stable_id IS STRING NOT NULL,
                  kind IS STRING, qualified_name IS STRING, relpath IS STRING}),
    (v:SymbolVersion => {repository_id IS STRING NOT NULL, stable_id IS STRING NOT NULL,
                         data IS STRING}),
    (e:Episode => {repository_id IS STRING NOT NULL, episode IS STRING, sequence IS INTEGER}),
    (j:Job => {id IS STRING, repository_id IS STRING NOT NULL, kind IS STRING NOT NULL,
               input_hash IS STRING NOT NULL, state IS STRING NOT NULL, priority IS INTEGER}),
    (m:Memory => {id IS STRING IS KEY, kind IS STRING, state IS STRING,
                  title IS STRING, body IS STRING}),
    (r:Run => {id IS STRING, repository_id IS STRING NOT NULL, external_key IS STRING NOT NULL}),
    (d:DeletionReceipt => {workspace_id IS STRING NOT NULL, sequence IS INTEGER, digest IS STRING}),
    (q:ConnectorDoc => {repository_id IS STRING NOT NULL, id IS STRING NOT NULL,
                        source_kind IS STRING, document_sha256 IS STRING, input IS STRING,
                        connector IS STRING, updated_at IS STRING}),
    (k:DocLinkCheckpoint => {repository_id IS STRING IS KEY, episode IS STRING, updated_at IS STRING}),
    (t:DocLinkState => {workspace_id IS STRING IS KEY, rules IS STRING, updated_at IS STRING}),
    (cr:ConceptRule => {repository_id IS STRING NOT NULL, id IS STRING NOT NULL,
                        description IS STRING, requires IS STRING, severity IS STRING,
                        cypher IS STRING, rule_sha IS STRING NOT NULL, source IS STRING}),
    (xr:ConstraintRule => {repository_id IS STRING NOT NULL, id IS STRING NOT NULL,
                           description IS STRING, requires IS STRING, severity IS STRING,
                           cypher IS STRING, rule_sha IS STRING NOT NULL, source IS STRING}),
    (rg:RuleGroup => {repository_id IS STRING NOT NULL, id IS STRING NOT NULL,
                      description IS STRING, include IS STRING, overrides IS STRING,
                      rule_sha IS STRING NOT NULL, source IS STRING}),
    (rs:RuleSource => {repository_id IS STRING NOT NULL, source IS STRING NOT NULL,
                       doc_sha IS STRING NOT NULL}),
    CONSTRAINT FOR (p:Project) REQUIRE (p.workspace_id, p.project_key) IS KEY,
    CONSTRAINT FOR (o:Object) REQUIRE (o.repository_id, o.sha256) IS KEY,
    CONSTRAINT FOR (s:Symbol) REQUIRE (s.repository_id, s.stable_id) IS KEY,
    CONSTRAINT FOR (e:Episode) REQUIRE (e.repository_id, e.sequence) IS KEY,
    CONSTRAINT FOR (j:Job) REQUIRE (j.repository_id, j.kind, j.input_hash) IS KEY,
    CONSTRAINT FOR (r:Run) REQUIRE (r.repository_id, r.external_key) IS KEY,
    CONSTRAINT FOR (d:DeletionReceipt) REQUIRE (d.workspace_id, d.sequence) IS KEY,
    CONSTRAINT FOR (q:ConnectorDoc) REQUIRE (q.repository_id, q.id) IS KEY,
    CONSTRAINT FOR (cr:ConceptRule) REQUIRE (cr.repository_id, cr.id) IS KEY,
    CONSTRAINT FOR (xr:ConstraintRule) REQUIRE (xr.repository_id, xr.id) IS KEY,
    CONSTRAINT FOR (rg:RuleGroup) REQUIRE (rg.repository_id, rg.id) IS KEY,
    CONSTRAINT FOR (rs:RuleSource) REQUIRE (rs.repository_id, rs.source) IS KEY,
    (s)-[:HAS_VERSION]->(v),
    (s)-[:CALLS => {key IS STRING, relpath IS STRING, source_location IS STRING}]->(s),
    (s)-[:CALLS_HIST => {key IS STRING, valid_from_episode IS STRING, valid_to_episode IS STRING}]->(s)
}";

/// A declared graph type is inert until promoted to current (spike E2).
const PROMOTE_GRAPH_TYPE: &str = "ALTER CURRENT GRAPH TYPE SET GRAPH TYPE graphicle";

/// Range and fulltext indexes (migration plan §2.7). All `IF NOT EXISTS`,
/// so a second open is a no-op.
const INDEXES: &[&str] = &[
    "CREATE INDEX job_ready IF NOT EXISTS FOR (j:Job) ON (j.state)",
    "CREATE INDEX sym_file IF NOT EXISTS FOR (s:Symbol) ON (s.relpath)",
    "CREATE INDEX chunk_path IF NOT EXISTS FOR (c:Chunk) ON (c.path)",
    "CREATE INDEX mem_lookup IF NOT EXISTS FOR (m:Memory) ON (m.kind)",
    "CREATE FULLTEXT INDEX chunk_text IF NOT EXISTS FOR (c:Chunk) ON EACH [c.index_text]",
    // Content store (store_content): the lookup keys of the remaining
    // single-row reads and updates. Bulk paths scan once per statement
    // instead; MERGE/property-MATCH without an index is superlinear in
    // store size on v0.12.2 (measured in store_content's perf probes).
    "CREATE INDEX obj_key IF NOT EXISTS FOR (o:Object) ON (o.repository_id, o.sha256)",
    "CREATE INDEX file_key IF NOT EXISTS FOR (f:File) ON (f.repository_id, f.path)",
    "CREATE INDEX parse_key IF NOT EXISTS FOR (p:Parse) ON (p.repository_id, p.path)",
    "CREATE INDEX chunk_key IF NOT EXISTS FOR (c:Chunk) ON (c.repository_id, c.path)",
    "CREATE INDEX sync_id IF NOT EXISTS FOR (s:Sync) ON (s.id)",
    "CREATE INDEX sync_repo IF NOT EXISTS FOR (s:Sync) ON (s.repository_id)",
    "CREATE INDEX snap_key IF NOT EXISTS FOR (s:Snapshot) ON (s.repository_id, s.sha256)",
    "CREATE INDEX event_repo IF NOT EXISTS FOR (e:Event) ON (e.repository_id)",
    "CREATE INDEX counter_key IF NOT EXISTS FOR (c:Counter) ON (c.repository_id, c.name)",
    "CREATE INDEX proj_id IF NOT EXISTS FOR (p:Project) ON (p.id)",
    "CREATE FULLTEXT INDEX mem_text IF NOT EXISTS FOR (m:Memory) ON EACH [m.title, m.body]",
    "CREATE FULLTEXT INDEX doc_text IF NOT EXISTS FOR (d:ConnectorDoc) ON EACH [d.content]",
    // Invalidation and symbol-link modules (invalidation.rs, symbols.rs).
    "CREATE INDEX dep_artifact IF NOT EXISTS FOR (a:Artifact) ON (a.workspace)",
    "CREATE INDEX sym_head_repo IF NOT EXISTS FOR (s:SymbolHead) ON (s.repository_id)",
    "CREATE INDEX sym_head_file IF NOT EXISTS FOR (s:SymbolHead) ON (s.relpath)",
    "CREATE INDEX sym_move_from IF NOT EXISTS FOR (m:SymbolMove) ON (m.from_stable_id)",
    // Bi-temporal symbol versions (graph_projection): every projection step
    // closes the live versions of the nodes it rewrote and reads the live
    // versions of the dirty owner groups. Without these the planner anchors
    // each lookup on the repository's whole Symbol set and expands
    // `HAS_VERSION` once per symbol — PROFILE on a 5276-symbol store: 5276 db
    // hits, ~10 ms per lookup. With them the planner starts from the indexed
    // version node instead: 1 db hit for the closing `UNWIND` row (the query
    // text is unchanged — the anchor choice is the planner's), 30 for a
    // dirty owner group. Measured 100-200x on those three shapes; index
    // maintenance costs ~5% on version writes.
    "CREATE INDEX live_node IF NOT EXISTS FOR (v:Live) ON (v.node_id)",
    "CREATE INDEX live_owner IF NOT EXISTS FOR (v:Live) ON (v.owner)",
    "CREATE INDEX version_node IF NOT EXISTS FOR (v:SymbolVersion) ON (v.node_id)",
    // Observer notes and projection maintenance (observer.rs, projection.rs).
    "CREATE INDEX note_repo IF NOT EXISTS FOR (n:Observation) ON (n.repository_id)",
    "CREATE INDEX note_source IF NOT EXISTS FOR (n:Observation) ON (n.source_id)",
    "CREATE INDEX reflection_repo IF NOT EXISTS FOR (m:MentalModel) ON (m.repository_id)",
    "CREATE INDEX backend_repo IF NOT EXISTS FOR (b:Backend) ON (b.repository_id)",
    "CREATE INDEX cleanup_state IF NOT EXISTS FOR (c:ProjectionCleanup) ON (c.state)",
    "CREATE INDEX cleanup_ws IF NOT EXISTS FOR (c:ProjectionCleanup) ON (c.workspace_id)",
];

/// Vector indexes created by [`GraphStore::ensure_vector_indexes`]:
/// `(name, label, property)`. Embeddings live on the node they describe;
/// the model they came from is a `model_key` property on the node.
const VECTOR_INDEXES: &[(&str, &str, &str)] = &[
    ("chunk_embed", "Chunk", "embedding"),
    ("mem_embed", "Memory", "embedding"),
    ("ent_embed", "Entity", "embedding"),
];

/// The query parameters of one statement. Build with
/// [`BTreeMap::from`]/[`BTreeMap::new`] and [`json_to_value`]; dynamic
/// labels and rel types go in as `$($param)` references.
pub type Params = BTreeMap<String, Value>;

/// A serde_json value as an engine value: objects become maps, arrays
/// become lists, integers `Int`, other numbers `Float`. This is the bridge
/// for the module boundary, which stays `serde_json::Value` in/out; a JSON
/// text column of the SQL store becomes a map/list property directly.
pub fn json_to_value(value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Json::String(s) => Value::String(s.as_str().into()),
        Json::Array(items) => {
            Value::List(items.iter().map(json_to_value).collect::<Vec<_>>().into())
        }
        Json::Object(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect::<BTreeMap<_, _>>()
                .into(),
        ),
    }
}

/// An engine value as JSON: maps/lists round-trip [`json_to_value`]; nodes
/// become their properties plus `_id` and `_labels`, relationships their
/// properties plus `_id`, `_type`, `_start` and `_end`; UUIDs render as
/// their canonical string (ids are stored as strings anyway — the engine's
/// UUID type enforces string values); vectors become number lists; temporal
/// and point values render as their display string. Engine floats that JSON
/// cannot carry (NaN, infinities) become `null`.
pub fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Value::String(s) => Json::String(s.to_string()),
        Value::List(items) => Json::Array(items.iter().map(value_to_json).collect()),
        Value::Map(map) => Json::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect::<Map<_, _>>(),
        ),
        Value::Node(node) => {
            let mut map = properties_to_json(node.properties.as_ref());
            map.insert("_id".into(), Json::from(node.id));
            map.insert(
                "_labels".into(),
                Json::Array(
                    node.labels
                        .iter()
                        .map(|l| Json::String(l.clone()))
                        .collect(),
                ),
            );
            Json::Object(map)
        }
        Value::Relationship(rel) => {
            let mut map = properties_to_json(rel.properties.as_ref());
            map.insert("_id".into(), Json::from(rel.id));
            map.insert("_type".into(), Json::String(rel.rel_type.clone()));
            map.insert("_start".into(), Json::from(rel.start));
            map.insert("_end".into(), Json::from(rel.end));
            Json::Object(map)
        }
        Value::Uuid(u) => Json::String(cypherlite::uuid_hyphenated(*u)),
        Value::Vector { values, .. } => Json::Array(values.iter().map(value_to_json).collect()),
        other => Json::String(other.to_display_string()),
    }
}

fn properties_to_json(properties: &PropertyMap) -> Map<String, Json> {
    properties
        .iter()
        .map(|(k, v)| (k.clone(), value_to_json(v)))
        .collect()
}

/// Query parameters from a JSON object: each field becomes a parameter via
/// [`json_to_value`]. The convenient way to build [`Params`] at a call site
/// (`params(&json!({"id": id, "rows": rows}))`).
pub fn params(value: &Json) -> Params {
    value
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect()
        })
        .unwrap_or_default()
}

/// Run `cypher` and return its rows as JSON objects keyed by column name
/// (the Cypher counterpart of `store::rows`). An empty `params` is fine;
/// pass `Params::new()`.
pub fn rows(db: &mut Connection, cypher: &str, params: Params) -> Result<Vec<Json>> {
    let result = db.query_with_params(cypher, params)?;
    Ok(result
        .rows
        .iter()
        .map(|row| {
            Json::Object(
                result
                    .header
                    .columns
                    .iter()
                    .cloned()
                    .zip(row.iter().map(value_to_json))
                    .collect(),
            )
        })
        .collect())
}

/// The first row of [`rows`], if any (the Cypher counterpart of
/// `store::one`).
pub fn one(db: &mut Connection, cypher: &str, params: Params) -> Result<Option<Json>> {
    Ok(rows(db, cypher, params)?.into_iter().next())
}

/// The `key` field of a row as a string (`""` when absent), the Cypher
/// counterpart of `store::s`.
pub fn s<'a>(value: &'a Json, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

/// A `RETURN n` node column as a plain record: its properties without the
/// engine's `_id`/`_labels` bookkeeping, so a migrated module's rows keep
/// the exact shape its SQL rows had.
pub fn node(row: &Json, key: &str) -> Json {
    let mut node = row[key].clone();
    if let Some(map) = node.as_object_mut() {
        map.retain(|k, _| !k.starts_with('_'));
    }
    node
}

/// A call site's file relative to its crate (see `store::crate_relative`).
fn crate_relative(file: &str) -> &str {
    if let Some(rest) = file.strip_prefix(env!("CARGO_MANIFEST_DIR"))
        && let Some(rest) = rest.strip_prefix(['/', '\\'])
    {
        return rest;
    }
    if Path::new(file).is_absolute()
        && let Some(at) = file.rfind("/src/").or_else(|| file.rfind("\\src\\"))
    {
        return &file[at + 1..];
    }
    file
}

/// Run a write statement, returning the engine's counters.
pub fn write(db: &mut Connection, cypher: &str, params: Params) -> Result<cypherlite::QueryStats> {
    Ok(db.query_with_params(cypher, params)?.stats)
}

/// Bytes as a base64 string: the engine's `Value` has no byte-string
/// variant, so byte columns are stored as base64 text (see
/// `docs/store-api.md`, value-mapping conventions).
pub fn b64e(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The inverse of [`b64e`].
pub fn b64d(text: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|_| Error::bad("Stored bytes are not valid base64"))
}
