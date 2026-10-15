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
use cypherlite::{Connection, Database, OpenMode, OpenOptions, Value};
use serde_json::{Map, Value as Json};
use std::{
    collections::{BTreeMap, HashMap},
    panic::Location,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

/// Write-write conflict retries of [`GraphStore::transaction`] (optimistic
/// commit; the whole closure reruns).
pub const MAX_WRITE_ATTEMPTS: u32 = 3;
/// A write transaction held longer than this is logged with its call site.
const SLOW_HOLD: Duration = Duration::from_secs(2);

/// An open graph store. Cheap to clone; all clones share the one engine.
#[derive(Clone)]
pub struct GraphStore {
    database: Arc<Database>,
    /// Where the store lives (`:memory:` for tests and scratch graphs).
    location: Arc<StoreLocation>,
}

#[derive(Debug)]
enum StoreLocation {
    Memory,
    Path(PathBuf),
}

impl GraphStore {
    /// Open (or create) the directory store at `path` and apply the schema
    /// (graph type + indexes) idempotently: a second open of the same store
    /// is a no-op.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.is_symlink() {
            return Err(Error::bad("Graph store cannot be a symlink"));
        }
        let uri = path
            .to_str()
            .ok_or_else(|| Error::bad("Graph store path is not valid UTF-8"))?;
        let store = Self {
            database: Arc::new(Database::open(uri)?),
            location: Arc::new(StoreLocation::Path(path.to_path_buf())),
        };
        store.apply_schema()?;
        Ok(store)
    }
    /// An ephemeral in-memory store (tests, transient processing). Applies
    /// the same schema as [`GraphStore::open`].
    pub fn open_in_memory() -> Result<Self> {
        let store = Self {
            database: Arc::new(Database::open(":memory:")?),
            location: Arc::new(StoreLocation::Memory),
        };
        store.apply_schema()?;
        Ok(store)
    }
    /// The store's directory, or `None` for an in-memory store.
    pub fn path(&self) -> Option<&Path> {
        match &*self.location {
            StoreLocation::Memory => None,
            StoreLocation::Path(path) => Some(path),
        }
    }
    /// The shared engine handle (checkpoint, snapshot, clone).
    pub fn database(&self) -> &Database {
        &self.database
    }
    /// A connection for the calling thread. Keep it for the thread's
    /// lifetime; each connection spawns its query worker lazily.
    pub fn connection(&self) -> Result<Connection> {
        Ok(self.database.connect()?)
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
            let mut db = self.connection()?;
            db.begin()?;
            let outcome = match f(&mut db) {
                Ok(value) => match db.commit() {
                    Ok(()) => Ok(value),
                    Err(cypherlite::Error::Conflict) if attempt < MAX_WRITE_ATTEMPTS => continue,
                    Err(error) => Err(error.into()),
                },
                Err(error) => {
                    let _ = db.rollback();
                    Err(error)
                }
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
    /// A read: runs `f` on a fresh connection without opening a write
    /// transaction (reads pin a consistent snapshot in the engine).
    pub fn read<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut db = self.connection()?;
        f(&mut db)
    }
    /// Flush the write-ahead log into a snapshot (maintenance/backup paths).
    pub fn checkpoint(&self) -> Result<()> {
        Ok(self.database.checkpoint()?)
    }
    /// A read-only follower of the store at `path` (ADR-0137: tails the
    /// writer's WAL on a background thread): for a process that reads the
    /// graph beside the process holding the writer lease (a CLI command or
    /// test runner next to a running server). Applies no schema (a follower
    /// cannot write); writes fail with "database is read-only".
    pub fn open_follower(path: impl AsRef<Path>) -> Result<GraphStore> {
        let path = path.as_ref();
        let uri = path
            .to_str()
            .ok_or_else(|| Error::bad("Graph store path is not valid UTF-8"))?;
        Ok(Self {
            database: Arc::new(Database::open_with(
                uri,
                OpenOptions::with_mode(OpenMode::Follow),
            )?),
            location: Arc::new(StoreLocation::Path(path.to_path_buf())),
        })
    }
    /// The shared store for `path`, opened (schema applied) on first use.
    /// The engine takes a single-process writer lease on a durable store, so
    /// every `Store` and bridge call site in this process shares the one
    /// instance per path — reopening the same directory must not open the
    /// engine twice. A process that finds the lease taken opens a read-only
    /// follower instead (migration plan §6.12 mitigation: read-only opens
    /// coexist with the writer).
    pub fn shared(path: impl AsRef<Path>) -> Result<GraphStore> {
        // Canonicalize the parent (the store path itself may not exist yet):
        // the same store reached through a symlinked directory (macOS
        // /var → /private/var) is one store and holds one writer lease.
        let key = match path
            .as_ref()
            .parent()
            .map(|parent| parent.canonicalize())
            .transpose()
        {
            Ok(Some(parent)) => parent.join(path.as_ref().file_name().unwrap_or_default()),
            _ => path.as_ref().to_path_buf(),
        };
        let mut shared = SHARED
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(store) = shared.get(&key) {
            return Ok(store.clone());
        }
        // Open the engine directly so the writer-lease error is visible:
        // a process that finds the lease taken (a server, an mcp stdio
        // session or a `sync --background` builder owns the store) opens a
        // read-only follower instead; graph writes from it fail loudly.
        let uri = key
            .to_str()
            .ok_or_else(|| Error::bad("Graph store path is not valid UTF-8"))?;
        let store = match Database::open(uri) {
            Ok(database) => {
                let store = Self {
                    database: Arc::new(database),
                    location: Arc::new(StoreLocation::Path(key.clone())),
                };
                store.apply_schema()?;
                store
            }
            Err(cypherlite::Error::Locked(_)) => GraphStore::open_follower(&key)?,
            Err(error) => return Err(error.into()),
        };
        shared.insert(key, store.clone());
        Ok(store)
    }
    /// The shared in-memory store (unit tests whose rusqlite side is
    /// in-memory, transient processing).
    pub fn shared_in_memory() -> Result<GraphStore> {
        static MEMORY: OnceLock<GraphStore> = OnceLock::new();
        if let Some(store) = MEMORY.get() {
            return Ok(store.clone());
        }
        let store = GraphStore::open_in_memory()?;
        Ok(MEMORY.get_or_init(|| store).clone())
    }
    /// Apply the graph type and indexes. Runs at every open; safe to call
    /// again. Vector indexes are NOT created here — see
    /// [`GraphStore::ensure_vector_indexes`].
    pub fn apply_schema(&self) -> Result<()> {
        let mut db = self.connection()?;
        // A store that earlier had the hand-written fallback constraints on
        // a graph-type-managed label: the graph type wins (the mechanisms
        // are either/or per label), so drop those first.
        for name in FALLBACK_CONSTRAINTS {
            db.query(&format!("DROP CONSTRAINT {name} IF EXISTS"))?;
        }
        db.query(GRAPH_TYPE)?;
        db.query(PROMOTE_GRAPH_TYPE)?;
        for ddl in INDEXES {
            db.query(ddl)?;
        }
        Ok(())
    }
    /// Create the vector indexes for `dimensions`, idempotently. Embedding
    /// dimensions are a property of the embedding model, which is only known
    /// once a store publishes vectors, so these indexes are created lazily by
    /// the vector pipeline rather than at open (a vector index fixes its
    /// dimension at creation). `dimensions` must be the model's; creating
    /// with a different dimension than an existing index is an engine error.
    pub fn ensure_vector_indexes(&self, dimensions: u32) -> Result<()> {
        let mut db = self.connection()?;
        for (name, label, property) in VECTOR_INDEXES {
            db.query(&format!(
                "CREATE VECTOR INDEX {name} IF NOT EXISTS FOR (n:{label}) ON (n.{property}) \
                 OPTIONS {{indexConfig: {{`vector.dimensions`: {dimensions}, \
                 `vector.similarity_function`: 'cosine'}}}}"
            ))?;
        }
        Ok(())
    }
}

/// The process-wide shared stores behind [`GraphStore::shared`].
static SHARED: OnceLock<Mutex<HashMap<PathBuf, GraphStore>>> = OnceLock::new();

/// The shared graph store beside a SQLite database file (`reviews.graph` in
/// the same directory); an in-memory database maps to the shared in-memory
/// store. Transition bridge for call sites that still hold only the
/// rusqlite connection while their domain migrates — new code takes a
/// `&GraphStore` (from [`crate::store::Store::graph`]).
pub fn shared_for(db: &rusqlite::Connection) -> Result<GraphStore> {
    match db.path().filter(|p| !p.is_empty()) {
        Some(database) => match Path::new(database).parent() {
            Some(directory) => GraphStore::shared(directory.join("reviews.graph")),
            None => GraphStore::shared_in_memory(),
        },
        None => GraphStore::shared_in_memory(),
    }
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
    CONSTRAINT FOR (p:Project) REQUIRE (p.workspace_id, p.project_key) IS KEY,
    CONSTRAINT FOR (o:Object) REQUIRE (o.repository_id, o.sha256) IS KEY,
    CONSTRAINT FOR (s:Symbol) REQUIRE (s.repository_id, s.stable_id) IS KEY,
    CONSTRAINT FOR (e:Episode) REQUIRE (e.repository_id, e.sequence) IS KEY,
    CONSTRAINT FOR (j:Job) REQUIRE (j.repository_id, j.kind, j.input_hash) IS KEY,
    CONSTRAINT FOR (r:Run) REQUIRE (r.repository_id, r.external_key) IS KEY,
    CONSTRAINT FOR (d:DeletionReceipt) REQUIRE (d.workspace_id, d.sequence) IS KEY,
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
    "CREATE FULLTEXT INDEX mem_text IF NOT EXISTS FOR (m:Memory) ON EACH [m.title, m.body]",
    "CREATE FULLTEXT INDEX doc_text IF NOT EXISTS FOR (d:ConnectorDoc) ON EACH [d.content]",
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
        Json::Array(items) => Value::List(items.iter().map(json_to_value).collect::<Vec<_>>().into()),
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
            let mut map = properties_to_json(&node.properties);
            map.insert("_id".into(), Json::from(node.id));
            map.insert("_labels".into(), Json::Array(node.labels.iter().map(|l| Json::String(l.clone())).collect()));
            Json::Object(map)
        }
        Value::Relationship(rel) => {
            let mut map = properties_to_json(&rel.properties);
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

fn properties_to_json(properties: &BTreeMap<String, Value>) -> Map<String, Json> {
    properties
        .iter()
        .map(|(k, v)| (k.clone(), value_to_json(v)))
        .collect()
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
