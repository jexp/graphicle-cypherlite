//! Native ownership of the same source/revision tables used by the original host.
use crate::{Error, Result, digest, id, integer, native, now, store_content, string};
use rusqlite::{
    Connection, OptionalExtension, Params, Transaction, TransactionBehavior, params,
    types::ValueRef,
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    panic::Location,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

/// A write transaction held longer than this is logged with its call site:
/// every other writer waits for it, up to the 10-second busy timeout.
const SLOW_HOLD: Duration = Duration::from_secs(2);
/// Slow holds kept for `runtime.status`, newest last.
const SLOW_KEPT: usize = 20;

#[derive(Default)]
struct LockStats {
    transactions: u64,
    held: Duration,
    longest: Duration,
    longest_at: String,
    slow: u64,
    recent: VecDeque<(String, f64)>,
}
static LOCKS: Mutex<LockStats> = Mutex::new(LockStats {
    transactions: 0,
    held: Duration::ZERO,
    longest: Duration::ZERO,
    longest_at: String::new(),
    slow: 0,
    recent: VecDeque::new(),
});

/// A call site's file relative to its crate (`src/jobs.rs`). Cargo gives
/// rustc absolute paths for path dependencies outside the package being
/// built (this runtime inside an edition's program, for example); lock reports
/// must not reveal the build machine's folders.
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

fn record_hold(at: &Location<'_>, held: Duration) {
    let site = format!("{}:{}", crate_relative(at.file()), at.line());
    let mut stats = LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    stats.transactions += 1;
    stats.held += held;
    if held > stats.longest {
        stats.longest = held;
        stats.longest_at = site.clone();
    }
    if held >= SLOW_HOLD {
        stats.slow += 1;
        if stats.recent.len() == SLOW_KEPT {
            stats.recent.pop_front();
        }
        stats.recent.push_back((site.clone(), held.as_secs_f64()));
        drop(stats);
        // A diagnostic, not a message for people: logged
        // with the other phase timings (`GRAPHICLE_PHASE_TIMINGS=1`); the
        // counts stay in `lock_report` (`status --json`'s `write_lock`).
        if crate::phase::enabled() {
            eprintln!(
                "graphicle: the write lock was held {:.1} s at {site}; other writers waited",
                held.as_secs_f64()
            );
        }
    }
}

/// Write-lock use by this process: how many write transactions ran, how long
/// they held the lock, the longest hold and recent slow holds by call site.
pub fn lock_report() -> Value {
    let stats = LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    json!({"write_transactions":stats.transactions,"held_seconds":stats.held.as_secs_f64(),
        "longest_seconds":stats.longest.as_secs_f64(),"longest_at":stats.longest_at,
        "slow_threshold_seconds":SLOW_HOLD.as_secs_f64(),"slow_holds":stats.slow,
        "recent_slow":stats.recent.iter().map(|(at,s)|json!({"at":at,"seconds":s})).collect::<Vec<_>>()})
}

#[derive(Clone)]
pub struct Store {
    pub directory: Arc<PathBuf>,
    pub database: Arc<PathBuf>,
    /// Signalled when a job is queued, so an idle worker in this process
    /// starts it at once instead of at its next poll.
    pub wake: Arc<tokio::sync::Notify>,
    /// The size limits of whoever runs this program ([`Store::with_limits`]);
    /// without them, the environment's over Graphicle Core's defaults.
    limits: Option<Arc<crate::limits::Limits>>,
}

#[derive(Clone)]
pub struct Principal {
    pub workspace: String,
    pub credential: String,
    pub hash: String,
    pub role: String,
}
impl Principal {
    pub fn permits(&self, permission: &str) -> bool {
        match self.role.as_str() {
            "owner" => true,
            "reviewer" => ["read", "write", "collect", "propose", "approve"].contains(&permission),
            "collector" => ["read", "collect", "propose"].contains(&permission),
            "runner" => ["read", "propose", "run", "verify"].contains(&permission),
            "agent" => ["read", "propose", "run"].contains(&permission),
            "reader" => permission == "read",
            _ => false,
        }
    }
}
pub fn rows(db: &Connection, sql: &str, arguments: impl Params) -> Result<Vec<Value>> {
    let mut statement = db.prepare(sql)?;
    let names: Vec<String> = statement
        .column_names()
        .iter()
        .map(|x| x.to_string())
        .collect();
    let mapped = statement.query_map(arguments, |row| {
        let mut value = Map::new();
        for (i, name) in names.iter().enumerate() {
            let field = match row.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(n) => json!(n),
                ValueRef::Text(s) => json!(String::from_utf8_lossy(s)),
                ValueRef::Blob(_) => Value::Null,
            };
            value.insert(name.clone(), field);
        }
        Ok(Value::Object(value))
    })?;
    Ok(mapped.collect::<std::result::Result<Vec<_>, _>>()?)
}
pub fn one(db: &Connection, sql: &str, arguments: impl Params) -> Result<Option<Value>> {
    Ok(rows(db, sql, arguments)?.into_iter().next())
}
pub fn decode(value: &mut Value, key: &str) -> Result<()> {
    if let Some(raw) = value[key].as_str() {
        value[key] = serde_json::from_str(raw)?;
    }
    Ok(())
}
pub fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
pub fn path(value: &str) -> Result<&str> {
    if value.is_empty()
        || value.len() > 1000
        || value.starts_with('/')
        || value.contains(['\\', '\0', '\r', '\n'])
        || value
            .split('/')
            .any(|v| v.is_empty() || v == "." || v == "..")
    {
        return Err(Error::bad("Use a relative source path without traversal"));
    }
    Ok(value)
}
pub fn hash(value: &str) -> Result<&str> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        Err(Error::bad("Expected lowercase SHA-256"))
    } else {
        Ok(value)
    }
}

/// One manifest entry of a committing sync: whether its content still needs
/// analysis or chunk rows.
struct SyncDocument {
    path: String,
    sha: String,
    analyze: bool,
    chunk: bool,
}

/// Documents a commit loads, analyses and chunks at a time (at most
/// [`crate::limits::REQUEST_BYTES`] of text).
const COMMIT_BATCH_FILES: usize = 512;

/// The search chunks of `content` at `path`: forty lines each, indexed with
/// the path's words, the text and the text's words.
fn chunk_rows(path: &str, content: &str) -> Vec<(usize, usize, String)> {
    let path_words = graphicle_memory::text::words(path).join(" ");
    content
        .split_inclusive('\n')
        .collect::<Vec<_>>()
        .chunks(40)
        .enumerate()
        .map(|(number, chunk)| {
            let line = number * 40 + 1;
            let text = chunk.join("");
            let indexed = format!(
                "{path_words}\n{text}\n{}",
                graphicle_memory::text::words(&text).join(" ")
            );
            (line, line + chunk.len() - 1, indexed)
        })
        .collect()
}

/// The `repo_parses` row of `content` at `path`: its revision and analysis,
/// within the native `analyze` request's `limits`. Limits never fail an
/// analysis. An analysis that fails otherwise is recorded as skipped, with
/// the reason, and the file stays searchable as text. Limited and skipped
/// analyses carry `revisions[1]` (the limited revision), so a change of the
/// limits redoes them; complete ones carry `revisions[0]`.
///
/// A failure that is not about the file is not cached as the file's: a
/// parser that is missing or incompatible fails the commit, and a parse that
/// did not finish (a timeout under load) or a caught panic is recorded under
/// a revision the next sync treats as outdated, so each sync retries it once.
fn analysis_row(
    path: &str,
    content: &str,
    limits: &Value,
    revisions: [&str; 2],
) -> Result<(String, String)> {
    let analysis = match native(
        "analyze",
        json!({"path":path,"content":content,"limits":limits}),
    ) {
        Ok(analysis) => analysis,
        Err(error) if parser_unavailable(&error.message) => {
            return Err(Error::bad(format!(
                "Could not analyse {path}: {}. Reinstall Graphicle",
                error.message
            )));
        }
        Err(error) => {
            let revision = if transient_failure(&error.message) {
                format!("{}+retry", revisions[1])
            } else {
                revisions[1].to_string()
            };
            let skipped = graphicle_memory::structure::skipped(&error.message);
            return Ok((revision, skipped.to_string()));
        }
    };
    let revision = if analysis.get("limited").is_some() || analysis.get("skipped").is_some() {
        revisions[1]
    } else {
        revisions[0]
    };
    Ok((revision.to_string(), analysis.to_string()))
}

/// An analysis error that is about this installation, not the file.
fn parser_unavailable(message: &str) -> bool {
    message.starts_with("Bundled parser unavailable")
        || message.starts_with("Parser grammar is incompatible")
}

/// An analysis error that may not recur: a parse that did not finish (a
/// timeout under load) or a caught panic.
fn transient_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    message.starts_with("Source parsing did not complete")
        || message.starts_with("Native operation failed")
        || lower.contains("timeout")
        || lower.contains("timed out")
}

/// The files of `repository`'s current heads whose structure analysis was
/// limited (`[{path, limit, reason}]`) or skipped (`[{path, reason}]`), in
/// path order. Complete analyses carry `structure::REVISION`, so only the
/// others are read. The parses live in the graph store: see
/// [`store_content::analysis_report`].
pub fn analysis_report(store: &crate::store_graph::GraphStore, repository: &str) -> Result<(Vec<Value>, Vec<Value>)> {
    store_content::analysis_report(store, repository)
}

/// The open graph stores of this process, by directory: the engine's
/// writer lease allows one open per directory, so concurrent operations
/// share one engine ([`Store::graph`]). A `Weak` entry dies with the last
/// operation holding it, releasing the lease between operations.
static GRAPHS: Mutex<BTreeMap<PathBuf, std::sync::Weak<crate::store_graph::GraphStore>>> =
    Mutex::new(BTreeMap::new());

fn graph_in(directory: &Path) -> Result<Arc<crate::store_graph::GraphStore>> {
    // Canonicalized: `GraphStore::open` keys its sharing registry by the
    // path as given, so every open of one store must spell it alike (a
    // second spelling opens a read-only follower on the taken lease).
    let key = std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
    // The lock is held across the open: a live handle's connection pool
    // pins the engine's writer lease, so a second opener that merely raced
    // the registry insert would spin on the lease for as long as the live
    // handle is held. Opening under the lock makes one handle per directory
    // airtight. `GraphStore::open` only touches its own store (schema DDL),
    // never `graph_in`, so this cannot re-enter.
    let mut registry = GRAPHS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(graph) = registry.get(&key).and_then(std::sync::Weak::upgrade) {
        return Ok(graph);
    }
    // Another process (a server, a background sync) may hold the engine's
    // writer lease; retry briefly, as SQLite's busy_timeout did.
    let mut attempts = 0;
    let graph = loop {
        match crate::store_graph::GraphStore::open(key.join("reviews.graph")) {
            Ok(graph) => break Arc::new(graph),
            Err(error) if error.message.contains("storage locked") && attempts < 100 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    };
    registry.insert(key, Arc::downgrade(&graph));
    Ok(graph)
}

/// Read-only handles, by canonical directory: the same one-handle-per-
/// directory sharing as [`GRAPHS`], for callers that never write (agent
/// hooks beside a running server). A read-only open takes shared locks and
/// never the writer lease, so it neither blocks nor is blocked by a writer.
static GRAPHS_READ_ONLY: Mutex<BTreeMap<PathBuf, std::sync::Weak<crate::store_graph::GraphStore>>> =
    Mutex::new(BTreeMap::new());

/// The read-only counterpart of [`graph_in`]: the store must already exist
/// (nothing is created or migrated), and the handle never takes the writer
/// lease, so a hook can read beside a running server.
fn graph_in_read_only(directory: &Path) -> Result<Arc<crate::store_graph::GraphStore>> {
    let key = std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
    let mut registry = GRAPHS_READ_ONLY
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(graph) = registry.get(&key).and_then(std::sync::Weak::upgrade) {
        return Ok(graph);
    }
    let graph = Arc::new(crate::store_graph::GraphStore::open_read_only(
        key.join("reviews.graph"),
    )?);
    registry.insert(key, Arc::downgrade(&graph));
    Ok(graph)
}

/// The graph store paired with a SQL [`Connection`]: `reviews.graph` beside
/// the connection's database file (shared through the registry in
/// [`graph_in`]). Errors for an in-memory connection.
///
/// Migration bridge: modules whose data moved onto CypherLite but whose
/// callers still thread a SQL `&Connection` resolve their [`GraphStore`]
/// here instead of growing a parameter all the way up; new code should
/// take a `&GraphStore` argument.
pub fn graph_for(db: &Connection) -> Result<Arc<crate::store_graph::GraphStore>> {
    let path = db
        .path()
        .ok_or_else(|| Error::bad("An in-memory SQL connection has no paired graph store"))?;
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    let directory = resolved
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::bad("Database path has no parent directory"))?;
    graph_in(&directory)
}

/// The read-only counterpart of [`graph_for`] for callers that never write
/// (the guard hooks): no writer lease, no schema DDL, no creation.
pub fn graph_for_read_only(db: &Connection) -> Result<Arc<crate::store_graph::GraphStore>> {
    let path = db
        .path()
        .ok_or_else(|| Error::bad("An in-memory SQL connection has no paired graph store"))?;
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    let directory = resolved
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::bad("Database path has no parent directory"))?;
    graph_in_read_only(&directory)
}

impl Store {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        if directory.is_symlink() {
            return Err(Error::bad("Data directory cannot be a symlink"));
        }
        std::fs::create_dir_all(directory)?;
        let database = directory.join("reviews.sqlite3");
        if database.is_symlink() {
            return Err(Error::bad("Database cannot be a symlink"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let store = Self {
            directory: Arc::new(directory.to_path_buf()),
            database: Arc::new(database),
            wake: Arc::new(tokio::sync::Notify::new()),
            limits: None,
        };
        // Create (or share) the graph store now so a misconfigured directory
        // fails at open, not at first use; the handle itself is not kept —
        // holding it would hold the engine's writer lease for the store's
        // lifetime and block every other process (see `graph_in`).
        graph_in(directory)?;
        let db = store.connect()?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(include_str!("schema.sql"))?;
        db.execute_batch(include_str!("workflow_schema.sql"))?;
        db.execute_batch(include_str!("expansion_schema.sql"))?;
        db.execute_batch(include_str!("projection_schema.sql"))?;
        ensure_column(&db, "gf_episodes", "parent_episode", "TEXT")?;
        db.execute_batch(include_str!("temporal_schema.sql"))?;
        // The projection's SQL body still buckets rows by owner file; the
        // group digests and triggers of graph_delta live in the graph store.
        ensure_column(&db, "gf_nodes", "owner", "TEXT NOT NULL DEFAULT ''")?;
        ensure_column(&db, "gf_edges", "owner", "TEXT NOT NULL DEFAULT ''")?;
        db.execute_batch(include_str!("product_schema.sql"))?;
        // Work state lives in the graph store (see store_graph.rs).
        // Each folder's freshness record.
        crate::fresh::migrate(&db)?;
        // Documents' mention links to code symbols.
        crate::doc_links::migrate(&db)?;
        ensure_column(
            &db,
            "gf_cross_unresolved",
            "exporter_episodes",
            "TEXT NOT NULL DEFAULT '{}'",
        )?;
        // Coverage rows carry the granularity of the upload that produced
        // them (`test` or `file`); rows written before the column existed
        // were all per-test.
        ensure_column(
            &db,
            "gf_test_coverage",
            "granularity",
            "TEXT NOT NULL DEFAULT 'test'",
        )?;
        // Projects earlier `graphicle pr` runs registered are PR scratch
        // copies: they no longer make imports ambiguous.
        crate::graph_projection::mark_legacy_pr_projects(&db)?;
        // Extension structures migrated to the graph store: they run from
        // `GraphStore::apply_schema` (`Extension::migrate` takes the graph
        // store now), after the core graph type and indexes.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                store.database.as_ref(),
                std::fs::Permissions::from_mode(0o600),
            )?;
        }
        Ok(store)
    }
    /// This store with the size limits of the running program's
    /// configuration file and environment ([`crate::limits`]).
    pub fn with_limits(mut self, limits: crate::limits::Limits) -> Self {
        self.limits = Some(Arc::new(limits));
        self
    }
    /// The size limits in effect: those set with [`Store::with_limits`],
    /// else the environment's over Graphicle Core's defaults.
    pub fn limits(&self) -> Result<crate::limits::Limits> {
        match &self.limits {
            Some(limits) => Ok(**limits),
            None => crate::limits::Limits::from_environment(&BTreeMap::new()),
        }
    }
    /// A handle to the CypherLite graph store (`reviews.graph` beside
    /// `reviews.sqlite3`) the migrated domains read and write through (see
    /// `docs/store-api.md`). Transient: within the process it shares the
    /// engine via a per-directory registry (the engine takes a
    /// single-process writer lease, so a second `Store::open` of one
    /// directory must reuse it), and the handle is dropped — the lease
    /// released — when no operation holds it, so another process (a CLI
    /// beside a server, a background sync) can open the store; its open
    /// busy-retries the lease briefly, as SQLite's busy_timeout did. Bind
    /// the handle once per operation and pass `&GraphStore` down.
    pub fn graph(&self) -> Arc<crate::store_graph::GraphStore> {
        graph_in(&self.directory).expect("graph store opened at Store::open")
    }
    /// This store's passage-embedding cache for `model_key`
    /// ([`crate::embedding_cache`]); vectors live in the graph store, so no
    /// second database file is involved.
    pub fn embedding_cache(&self, model_key: &str) -> crate::embedding_cache::Cache {
        crate::embedding_cache::Cache::new((*self.graph()).clone(), model_key)
    }
    pub fn connect(&self) -> Result<Connection> {
        let db = Connection::open(self.database.as_ref())?;
        db.busy_timeout(Duration::from_secs(10))?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.pragma_update(None, "secure_delete", true)?;
        Ok(db)
    }
    /// A write transaction (`BEGIN IMMEDIATE`): it takes SQLite's single write
    /// lock up front, so a read-then-write never fails to upgrade. Keep the
    /// work inside short; long holds are logged with the caller's location.
    #[track_caller]
    pub fn transaction<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let at = Location::caller();
        let mut db = self.connect()?;
        let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let started = Instant::now();
        let outcome = (move || {
            let value = f(&transaction)?;
            transaction.commit()?;
            Ok(value)
        })();
        record_hold(at, started.elapsed());
        outcome
    }
    /// A read-only snapshot (`BEGIN DEFERRED` with `query_only`). In WAL mode
    /// it neither takes nor waits for the write lock, so long reads run beside
    /// writers and each other. A write inside it fails at once instead of
    /// upgrading the transaction and failing only under concurrency.
    pub fn read<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut db = self.connect()?;
        db.pragma_update(None, "query_only", true)?;
        let transaction = db.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let value = f(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }
    #[track_caller]
    pub fn authorized<T>(
        &self,
        workspace: &str,
        token: &str,
        permission: &str,
        f: impl FnOnce(&Transaction<'_>, &Principal) -> Result<T>,
    ) -> Result<T> {
        if token.len() > 512 {
            return Err(Error::denied());
        }
        self.transaction(|db| {
            let principal = authorize_hash(
                db,
                workspace,
                &graphicle_memory::text::sha256(token.as_bytes()),
                permission,
            )?;
            f(db, &principal)
        })
    }
    /// `authorized` for operations that only read: a read-only snapshot.
    pub fn authorized_read<T>(
        &self,
        workspace: &str,
        token: &str,
        permission: &str,
        f: impl FnOnce(&Transaction<'_>, &Principal) -> Result<T>,
    ) -> Result<T> {
        if token.len() > 512 {
            return Err(Error::denied());
        }
        self.read(|db| {
            let principal = authorize_hash(
                db,
                workspace,
                &graphicle_memory::text::sha256(token.as_bytes()),
                permission,
            )?;
            f(db, &principal)
        })
    }
    pub fn create_workspace(&self, name: &str) -> Result<Value> {
        string(&json!({"name":name}), "name", 100)?;
        self.transaction(|db| {
            let count:i64=db.query_row("SELECT count(*) FROM workspaces",[],|r|r.get(0))?;
            if count>=100{return Err(Error::bad("Workspace limit reached"));}
            let identifier=id();let token=format!("{}{}",id(),id());let stamp=now();
            db.execute("INSERT INTO workspaces VALUES(?,?,?,?,0)",params![identifier,graphicle_memory::text::sha256(token.as_bytes()),name.trim(),stamp])?;
            Ok(json!({"id":identifier,"name":name.trim(),"capability":token,"version":0,"created_at":stamp}))
        })
    }
    pub fn workspace(&self, workspace: &str, token: &str) -> Result<Value> {
        self.authorized_read(workspace, token, "read", |db, p| {
            let mut row = one(
                db,
                "SELECT id,name,created_at,version FROM workspaces WHERE id=?",
                [workspace],
            )?
            .ok_or_else(Error::denied)?;
            row["credential_role"] = json!(p.role);
            Ok(row)
        })
    }
    pub fn grant(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace,token,"admin",|db,_| {
            let role=string(args,"role",30)?;
            if !["reader","agent","reviewer","collector","runner"].contains(&role) {return Err(Error::bad("Invalid credential role"));}
            let label=string(args,"label",100)?; let token=format!("{}{}",id(),id()); let identifier=id();
            let expiry=args.get("expires_at").and_then(Value::as_str);
            if let Some(value)=expiry && graphicle_memory::temporal::instant(value).map_err(Error::bad)?<=chrono::Utc::now().fixed_offset() {return Err(Error::bad("Expiry must be in the future"));}
            db.execute("INSERT INTO credentials VALUES (?,?,?,?,?,?,?,NULL)",params![identifier,workspace,graphicle_memory::text::sha256(token.as_bytes()),role,label,now(),expiry])?;
            Ok(json!({"id":identifier,"capability":token,"role":role,"label":label,"expires_at":expiry}))
        })
    }
    pub fn revoke(&self, workspace: &str, token: &str, credential: &str) -> Result<Value> {
        self.authorized(workspace,token,"admin",|db,_| {
            if db.execute("UPDATE credentials SET revoked_at=? WHERE id=? AND workspace_id=?",params![now(),credential,workspace])?!=1 {return Err(Error::bad("Credential not found"));}
            // A member linked to the credential is deactivated too, on any
            // store that has the team tables, so the revocation holds in
            // every edition that opens the store.
            crate::store_compat::credential_revoked(db,workspace,credential)?;
            crate::extension::installed().credential_revoked(db,workspace,credential)?;
            // A queued job never retains the authority of a revoked credential.
            let hash:String=db.query_row("SELECT capability_hash FROM credentials WHERE id=?",[credential],|r|r.get(0))?;
            self.graph().transaction(|g| {
                crate::store_graph::rows(
                    g,
                    "MATCH (j:Job {principal_hash: $hash}) WHERE j.state IN ['queued','retry','running'] \
                     SET j.state='cancelled', j.updated_at=$now",
                    crate::store_graph::params(&json!({"hash": hash, "now": now()})),
                )?;
                Ok(())
            })?;
            // Cached answers produced under the revoked credential are no longer current.
            let revoked_sources=credential_sources(self,db,workspace,credential)?;
            crate::invalidation::permissions_changed(&self.graph(),workspace,Some(&hash),&revoked_sources,"credential_revoked")?;
            Ok(json!({"revoked":true,"sources":revoked_sources}))
        })
    }
    pub fn register(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace,token,"collect",|db,p| {
            let key=string(args,"project_key",200)?; let name=string(args,"name",150)?;
            if let Some(row)=one(db,"SELECT id FROM repo_projects WHERE workspace_id=? AND project_key=?",params![workspace,key])? {repo(db,p,s(&row,"id"))?;return Ok(json!({"id":row["id"],"replayed":true}));}
            let identifier=id();let stamp=now();
            db.execute("INSERT INTO repo_projects(id,workspace_id,project_key,name,created_at,updated_at) VALUES(?,?,?,?,?,?)",params![identifier,workspace,key,name,stamp,stamp])?;
            store_content::register_project(&self.graph(),&json!({"id":identifier,"workspace_id":workspace,"project_key":key,"name":name}))?;
            Ok(json!({"id":identifier,"replayed":false}))
        })
    }
    /// Mark a project as a PR scratch copy (`pr_scratch`): the
    /// project `graphicle pr` registers for a commit range. Linking skips it
    /// while another project exports its packages. Idempotent.
    pub fn mark_pr_scratch(&self, workspace: &str, token: &str, repository: &str) -> Result<Value> {
        self.authorized(workspace, token, "collect", |db, p| {
            let repository = repo(db, p, repository)?;
            db.execute(
                "INSERT OR IGNORE INTO repo_project_roles(repository_id,role,created_at) VALUES(?,?,?)",
                params![s(&repository, "id"), crate::graph_projection::PR_SCRATCH, now()],
            )?;
            Ok(json!({"id":repository["id"],"role":crate::graph_projection::PR_SCRATCH}))
        })
    }
    /// Start a sync (`sync.prepare`) with the snapshot's manifest,
    /// `[{path, sha256, bytes}]`. A request carries at most
    /// [`crate::limits::REQUEST_FILES`] entries, so a larger manifest comes in
    /// parts: every part but the last has `manifest_complete: false`, and the
    /// parts after the first name the `sync_id` the first one returned. The
    /// manifest is held to the project limits of whoever runs Graphicle
    /// ([`crate::limits`]); its last part returns the `snapshot` and the paths
    /// whose content is `missing`, or `replayed: true` for an unchanged tree.
    /// A failed request changes nothing; `sync.abort` drops an unfinished sync.
    pub fn sync_prepare(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let limits = self.limits()?;
        let manifest = args["manifest"]
            .as_array()
            .ok_or_else(|| Error::bad("Expected manifest"))?;
        if manifest.len() > crate::limits::REQUEST_FILES {
            return Err(Error::bad(
                "At most 5,000 manifest entries per request; send a larger manifest in parts with manifest_complete: false",
            ));
        }
        let complete = match args.get("manifest_complete") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(complete)) => *complete,
            Some(_) => return Err(Error::bad("Expected manifest_complete: true or false")),
        };
        let mut part = BTreeMap::new();
        for item in manifest {
            let name = path(string(item, "path", 1000)?)?;
            let sha = hash(string(item, "sha256", 64)?)?;
            let size = integer(item, "bytes", 0, limits.file_bytes)?;
            if part
                .insert(name.to_string(), json!({"sha256":sha,"bytes":size}))
                .is_some()
            {
                return Err(Error::bad("Duplicate source path"));
            }
        }
        let continued = match args.get("sync_id") {
            None | Some(Value::Null) => None,
            Some(_) => Some(string(args, "sync_id", 100)?),
        };
        let metadata = args.get("metadata").cloned().unwrap_or(json!({}));
        if !metadata.is_object() || metadata.to_string().len() > 65536 {
            return Err(Error::bad("Invalid bounded metadata"));
        }
        let limited = graphicle_memory::structure::limited_revision(&limits.analysis());
        let graph = self.graph();
        // Only the project row is SQL here; the sync protocol's own state is
        // all graph. The row is read under a short read snapshot so the SQL
        // write lock is never held across a graph transaction (a held lock
        // starved concurrent syncs past the busy timeout).
        let repository=self.authorized_read(workspace,token,"collect",|db,p| {
            repo(db,p,string(args,"repository_id",100)?)
        })?;
        {
            let identifier=s(&repository,"id");
            // Earlier parts' entries, the snapshot the sync started from, and its metadata.
            let (mut files,expected,metadata)=match continued {
                Some(sync_id)=>{
                    let sync=store_content::sync_get(&graph,identifier,sync_id)?.ok_or_else(||Error::bad("Sync not found"))?;
                    if s(&sync,"state")!="preparing" {return Err(Error::conflict("Sync is not waiting for more of its manifest"));}
                    (serde_json::from_str::<BTreeMap<String,Value>>(s(&sync,"manifest"))?,s(&sync,"expected_snapshot").to_string(),s(&sync,"metadata").to_string())
                }
                None=>{
                    if repository["snapshot"]!=args["expected_snapshot"] {return Err(Error::conflict("Repository changed during scanning"));}
                    (BTreeMap::new(),s(&repository,"snapshot").to_string(),metadata.to_string())
                }
            };
            for (name,item) in part {
                if files.insert(name,item).is_some() {return Err(Error::bad("Duplicate source path"));}
            }
            let bytes:usize=files.values().map(|item|item["bytes"].as_u64().unwrap_or(0) as usize).sum();
            if files.len()>limits.project_files || bytes>limits.project_bytes {
                return Err(crate::limits::project_refusal(files.len(),bytes,false,&limits));
            }
            let unfinished=||->Result<()>{
                if store_content::syncs_unfinished(&graph,identifier)?>=20{return Err(Error::conflict("Abort an unfinished sync before adding another"));}
                Ok(())
            };
            if !complete {
                let manifest=graphicle_memory::text::canonical(&json!(files));
                let sync_id=match continued {
                    Some(sync_id)=>{store_content::sync_update(&graph,sync_id,&json!({"manifest":manifest}))?;sync_id.to_string()}
                    None=>{unfinished()?;let sync_id=id();store_content::sync_create(&graph,&repository,&sync_id,&json!({"expected_snapshot":expected,"snapshot":"","manifest":manifest,"metadata":metadata,"state":"preparing"}))?;sync_id}
                };
                return Ok(json!({"sync_id":sync_id,"manifest_complete":false,"files":files.len()}));
            }
            if s(&repository,"snapshot")!=expected {return Err(Error::conflict("Repository changed during scanning"));}
            let files=json!(files);
            let snapshot=digest(&files);
            let outdated=store_content::outdated_heads(&graph,identifier,[graphicle_memory::structure::REVISION,&limited])?;
            if s(&repository,"snapshot")==snapshot && outdated==0 {
                if let Some(sync_id)=continued {store_content::sync_delete(&graph,sync_id)?;}
                // Identical tree, possibly a new commit identity: the existing
                // episode still gains the sha from this sync's metadata. The
                // enqueue is SQL; re-read the project row under the write
                // lock and re-verify the snapshot there.
                let metadata:Value=serde_json::from_str(&metadata)?;
                let projection=self.authorized(workspace,token,"collect",|db,p| {
                    let repository=repo(db,p,string(args,"repository_id",100)?)?;
                    if s(&repository,"snapshot")!=snapshot {return Err(Error::conflict("Repository changed during scanning"));}
                    crate::jobs::enqueue_projection(self,db,s(&repository,"id"),p,&snapshot,&metadata)
                })?;
                return Ok(json!({"snapshot":snapshot,"sync_id":null,"missing":[],"replayed":true,"projection_job":projection}));
            }
            // One UNWIND round trip, replacing the per-file SELECT EXISTS loop.
            let entries:Vec<Value>=files.as_object().unwrap().iter().map(|(name,item)|json!({"path":name,"sha256":item["sha256"],"bytes":item["bytes"]})).collect();
            let missing=store_content::missing_objects(&graph,identifier,&json!(entries))?;
            let manifest=graphicle_memory::text::canonical(&files);
            let sync_id=match continued {
                Some(sync_id)=>{store_content::sync_update(&graph,sync_id,&json!({"snapshot":snapshot,"manifest":manifest,"state":"pending"}))?;sync_id.to_string()}
                None=>{unfinished()?;let sync_id=id();store_content::sync_create(&graph,&repository,&sync_id,&json!({"expected_snapshot":expected,"snapshot":snapshot,"manifest":manifest,"metadata":metadata,"state":"pending"}))?;sync_id}
            };
            Ok(json!({"snapshot":snapshot,"sync_id":sync_id,"missing":missing,"replayed":false}))
        }
    }
    /// Send content for a pending sync (`sync.upload`): 1 to
    /// [`crate::limits::UPLOAD_FILES`] files whose text matches the manifest,
    /// at most [`crate::limits::REQUEST_BYTES`] of text unless it is one file.
    pub fn sync_upload(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let limits = self.limits()?;
        let graph = self.graph();
        let repository = self.authorized_read(workspace, token, "collect", |db, p| {
            repo(db, p, string(args, "repository_id", 100)?)
        })?;
        {
            let identifier = s(&repository, "id");
            let sync = store_content::sync_get(&graph, identifier, string(args, "sync_id", 100)?)?
                .ok_or_else(|| Error::bad("Sync not found"))?;
            if s(&sync, "state") != "pending" {
                return Err(Error::conflict("Sync is not pending"));
            }
            let manifest: Value = serde_json::from_str(s(&sync, "manifest"))?;
            let files = args["files"]
                .as_array()
                .filter(|v| !v.is_empty() && v.len() <= crate::limits::UPLOAD_FILES)
                .ok_or_else(|| Error::bad("Upload 1–1,000 files"))?;
            let mut accepted = Vec::new();
            let mut objects = Vec::new();
            let mut sent = 0usize;
            for file in files {
                let name = path(string(file, "path", 1000)?)?;
                let content = file["content"]
                    .as_str()
                    .filter(|v| v.len() <= limits.file_bytes && !v.contains('\0'))
                    .ok_or_else(|| Error::bad("Invalid source text"))?;
                sent += content.len();
                if files.len() > 1 && sent > crate::limits::REQUEST_BYTES {
                    return Err(Error::bad(
                        "Upload at most 64 MiB of text per request, or a single file",
                    ));
                }
                let sha = graphicle_memory::text::sha256(content.as_bytes());
                if s(&manifest[name], "sha256") != sha
                    || manifest[name]["bytes"].as_u64() != Some(content.len() as u64)
                {
                    return Err(Error::conflict("Source does not match its manifest"));
                }
                objects.push(json!({"sha256":sha,"content":content,"bytes":content.len()}));
                accepted.push(name.to_string());
            }
            store_content::put_objects(&graph, &repository, &json!(objects))?;
            let bytes = store_content::retained_bytes(&graph, identifier)?;
            let retained = limits.retained_bytes();
            if usize::try_from(bytes).unwrap_or(usize::MAX) > retained {
                return Err(Error::bad(format!(
                    "Retained sources exceed {}, earlier versions included; prune earlier versions with storage.prune, or raise limits.project_bytes",
                    crate::limits::size(retained)
                )));
            }
            Ok(json!({"accepted":accepted,"snapshot_activated":false}))
        }
    }
    /// Activate a pending sync's snapshot (`sync.commit`). New content is
    /// analysed and chunked in batches outside the write lock; those rows are
    /// keyed by content, so they stay invisible until the heads move in the
    /// last transaction, and a commit that fails before it changes nothing a
    /// reader sees. Analysis limits never fail it: a file past one is listed
    /// under `limited`, and a file whose analysis fails under `skipped`, both
    /// still searchable as text. Storage, integrity and authorization errors
    /// fail it.
    pub fn sync_commit(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let limits = self.limits()?;
        let analysis_limits = limits.analysis_request();
        let limited = graphicle_memory::structure::limited_revision(&limits.analysis());
        let current = [graphicle_memory::structure::REVISION, limited.as_str()];
        // One graph handle for the whole commit (each open replays the WAL;
        // the handle also pins the engine's writer lease for the operation).
        let graph = self.graph();
        // Plan the commit from a complete authorized snapshot.
        let (sync, documents) = self.authorized_read(workspace, token, "collect", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let sync =
                store_content::sync_get(&graph, s(&repository, "id"), string(args, "sync_id", 100)?)?
                    .ok_or_else(|| Error::bad("Sync not found"))?;
            match s(&sync, "state") {
                "aborted" => return Err(Error::conflict("Sync was aborted")),
                "preparing" => {
                    return Err(Error::conflict(
                        "Send the rest of the manifest before committing",
                    ));
                }
                _ => {}
            }
            let manifest: Value = serde_json::from_str(s(&sync, "manifest"))?;
            let identifier = s(&repository, "id");
            // Analysis and chunking are pure functions of `(path, content)`
            // and keyed by `(path, sha256)` in the graph, so an object whose
            // parse/chunk nodes already exist at the current revision is
            // carried over; only new content is verified, loaded and parsed.
            // One UNWIND statement replaces the per-file SELECT EXISTS loops.
            let entries: Vec<Value> = manifest
                .as_object()
                .ok_or_else(|| Error::bad("Invalid stored manifest"))?
                .iter()
                .map(|(path, item)| json!({"path":path,"sha256":item["sha256"]}))
                .collect();
            let plan = store_content::commit_plan(
                &graph,
                identifier,
                &json!(entries),
                [current[0], current[1]],
            )?;
            let mut documents: Vec<SyncDocument> = Vec::new();
            for row in plan {
                if row["present"] != true {
                    return Err(Error::conflict("Upload missing sources before committing"));
                }
                documents.push(SyncDocument {
                    path: s(&row, "path").to_string(),
                    sha: s(&row, "sha256").to_string(),
                    analyze: row["parsed"] != true,
                    chunk: row["chunked"] != true,
                });
            }
            Ok((sync, documents))
        })?;
        let work: Vec<&SyncDocument> = documents.iter().filter(|d| d.analyze || d.chunk).collect();
        let mut start = 0;
        while start < work.len() {
            // Load a batch (integrity checked), then analyse and chunk it
            // without holding the write lock.
            let contents = self.authorized_read(workspace, token, "collect", |db, p| {
                let repository = repo(db, p, string(args, "repository_id", 100)?)?;
                let identifier = s(&repository, "id");
                let (mut contents, mut bytes) = (Vec::new(), 0);
                let mut pending = &work[start..];
                // Contents load one UNWIND statement per group instead of one
                // SELECT per file; group size bounds the transient memory.
                while !pending.is_empty()
                    && contents.len() < COMMIT_BATCH_FILES
                    && bytes < crate::limits::REQUEST_BYTES
                {
                    let group = &pending[..pending.len().min(32)];
                    let shas: Vec<String> = group.iter().map(|d| d.sha.clone()).collect();
                    let loaded = store_content::object_contents(&graph, identifier, &shas)?;
                    for (document, content) in group.iter().zip(loaded) {
                        if contents.len() == COMMIT_BATCH_FILES
                            || bytes >= crate::limits::REQUEST_BYTES
                        {
                            break;
                        }
                        let content = content
                            .ok_or_else(|| Error::conflict("Upload missing sources before committing"))?;
                        if graphicle_memory::text::sha256(content.as_bytes()) != document.sha {
                            return Err(Error::bad("Source integrity check failed"));
                        }
                        bytes += content.len();
                        contents.push(content);
                    }
                    pending = &pending[group.len()..];
                }
                Ok(contents)
            })?;
            let batch = &work[start..start + contents.len()];
            let rows: Vec<_> = batch
                .iter()
                .zip(&contents)
                .map(|(document, content)| -> Result<_> {
                    let analysis = if document.analyze {
                        Some(analysis_row(
                            &document.path,
                            content,
                            &analysis_limits,
                            current,
                        )?)
                    } else {
                        None
                    };
                    let chunks = if document.chunk {
                        chunk_rows(&document.path, content)
                    } else {
                        Vec::new()
                    };
                    Ok((analysis, chunks))
                })
                .collect::<Result<_>>()?;
            drop(contents);
            // Parses and chunks are pure graph writes; only authorization
            // and the abort check need the store here, both read-only. The
            // SQL write lock is never held across a graph transaction.
            let repository = self.authorized_read(workspace, token, "collect", |db, p| {
                repo(db, p, string(args, "repository_id", 100)?)
            })?;
            let identifier = s(&repository, "id");
            let state = store_content::sync_state(&graph, identifier, s(&sync, "id"))?
                .ok_or_else(|| Error::bad("Sync not found"))?;
            if state == "aborted" {
                return Err(Error::conflict("Sync was aborted"));
            }
            let mut parses = Vec::new();
            let mut chunks = Vec::new();
            for (document, (analysis, file_chunks)) in batch.iter().zip(&rows) {
                if let Some((revision, analysis)) = analysis {
                    parses.push(json!({"path":document.path,"sha256":document.sha,
                        "revision":revision,"data":analysis}));
                }
                for (line, end_line, text) in file_chunks {
                    chunks.push(json!({"path":document.path,"sha256":document.sha,
                        "line":line,"end_line":end_line,"index_text":text}));
                }
            }
            if !parses.is_empty() {
                store_content::put_parses(&graph, &repository, &json!(parses))?;
            }
            if !chunks.is_empty() {
                store_content::put_chunks(&graph, &repository, &json!(chunks))?;
            }
            start += batch.len();
        }
        // The head swap (one graph transaction) commits before the SQL part
        // starts: the SQL write lock is never held across graph work, and the
        // SQL transaction re-verifies the snapshot before its side effects.
        // The two stores cannot commit atomically until the remaining domains
        // migrate; a lost race leaves the graph ahead, which the next sync's
        // replay path reconciles.
        let repository=self.authorized_read(workspace,token,"collect",|db,p| {
            repo(db,p,string(args,"repository_id",100)?)
        })?;
        let identifier=s(&repository,"id");
        let state=store_content::sync_state(&graph,identifier,s(&sync,"id"))?.ok_or_else(||Error::bad("Sync not found"))?;
        if state=="complete" {return Ok(json!({"snapshot":sync["snapshot"],"current_snapshot":repository["snapshot"],"replayed":true}));}
        if state!="pending" || repository["snapshot"]!=sync["expected_snapshot"] {return Err(Error::conflict("A newer sync finished first"));}
        let before=store_content::heads(&graph,identifier)?;
        let changed:Vec<String>=documents.iter().filter(|d|before.get(&d.path)!=Some(&d.sha)).map(|d|d.path.clone()).collect();
        let present:BTreeSet<&str>=documents.iter().map(|d|d.path.as_str()).collect();
        let removed:Vec<_>=before.keys().filter(|path|!present.contains(path.as_str())).cloned().collect();
        let manifest_docs:Vec<Value>=documents.iter().map(|d|json!({"path":d.path,"sha256":d.sha})).collect();
        store_content::activate(&graph,&repository,&sync,&json!(manifest_docs),&changed,&removed)?;
        let changed_paths:BTreeSet<&str>=changed.iter().map(String::as_str).collect();
        let changed_docs:Vec<(String,String)>=documents.iter().filter(|d|changed_paths.contains(d.path.as_str())).map(|d|(d.path.clone(),d.sha.clone())).collect();
        let chunk_lines=store_content::chunk_lines_for(&graph,identifier,&changed_docs)?;
        let (limited,skipped)=store_content::analysis_report(&graph,identifier)?;
        self.authorized(workspace,token,"collect",|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;let identifier=s(&repository,"id");
            if repository["snapshot"]!=sync["expected_snapshot"] {return Err(Error::conflict("A newer sync finished first"));}
            db.execute("UPDATE repo_projects SET snapshot=?,generation=generation+1,metadata=?,updated_at=? WHERE id=?",params![s(&sync,"snapshot"),s(&sync,"metadata"),now(),identifier])?;
            invalidate(db,identifier)?;
            // The commit's graph handle (`graph` above) is shared by the
            // invalidation loops; the event was already written by
            // `store_content::activate` with the head swap.
            let gstore=&graph;
            for path in changed.iter().chain(&removed) {crate::invalidation::file_changed(gstore,&p.workspace,identifier,path,"repository_commit")?;crate::observer::forget_source(db,gstore,&p.workspace,identifier,&format!("file:{path}"))?;}
            for key in &changed_docs {let lines=chunk_lines.get(key).cloned().unwrap_or_default();crate::invalidation::record_file_chunks(gstore,&p.workspace,identifier,&key.0,&lines)?;}
            // Every commit yields a graph episode labelled with its commit identity.
            let metadata:Value=serde_json::from_str(s(&sync,"metadata")).unwrap_or(Value::Null);
            let projection=crate::jobs::enqueue_projection(self,db,identifier,p,s(&sync,"snapshot"),&metadata)?;
            // A head set that was already projected (a file edited, then restored) gets no new projection to clear the markers above.
            crate::graph_projection::clear_stale_when_current(db,gstore,identifier)?;
            let mut result=json!({"snapshot":sync["snapshot"],"changed":changed,"removed":removed,"files":documents.len(),"replayed":false,"projection_job":projection});
            if !limited.is_empty() {result["limited"]=json!(limited);}
            if !skipped.is_empty() {result["skipped"]=json!(skipped);}
            Ok(result)
        })
    }
}

/// Connector documents whose sharing policy names `credential` exactly: an
/// entry of the document's ACL credential list, or a provider-side principal
/// (`am_source_permissions`) that maps to one of the credential's verified
/// provider identities. Both are joins on the identifier, never text search
/// over the JSON, so `cred-a` does not match `cred-ab`.
pub fn credential_sources(
    store: &Store,
    db: &Connection,
    workspace: &str,
    credential: &str,
) -> Result<Vec<String>> {
    let mut sources = BTreeSet::new();
    let projects: Vec<String> =
        rows(db, "SELECT id FROM repo_projects WHERE workspace_id=?", [workspace])?
            .iter()
            .map(|r| s(r, "id").to_string())
            .collect();
    let connectors: Vec<Value> = store.graph().read(|db| {
        crate::store_graph::rows(
            db,
            "MATCH (c:Connector) WHERE c.repository_id IN $projects RETURN c.id AS id, c.kind AS kind",
            crate::store_graph::Params::from([(
                "projects".into(),
                crate::store_graph::json_to_value(&json!(projects)),
            )]),
        )
    })?;
    let ids: Vec<String> = connectors.iter().map(|c| s(c, "id").to_string()).collect();
    store.graph().read(|db| {
        for row in crate::store_graph::rows(
            db,
            "MATCH (d:ConnectorDoc {present: true}) WHERE d.connector_id IN $connectors AND $credential IN d.acl_credentials RETURN d.connector_id AS connector_id, d.id AS id",
            crate::store_graph::Params::from([
                ("connectors".into(), crate::store_graph::json_to_value(&json!(ids))),
                ("credential".into(), crate::store_graph::json_to_value(&json!(credential))),
            ]),
        )? {
            sources.insert(crate::invalidation::connector_document_id(
                s(&row, "connector_id"),
                s(&row, "id"),
            ));
        }
        // Provider-side grants: the credential's verified identities of each
        // connector's provider, then user/email grants naming them.
        let identities = crate::store_graph::rows(
            db,
            "MATCH (i:ProviderIdentity {workspace_id: $workspace, credential_id: $credential}) RETURN i.provider AS provider, i.external_id AS external_id",
            crate::store_graph::Params::from([
                ("workspace".into(), crate::store_graph::json_to_value(&json!(workspace))),
                ("credential".into(), crate::store_graph::json_to_value(&json!(credential))),
            ]),
        )?;
        if identities.is_empty() {
            return Ok(());
        }
        let mut emails: std::collections::BTreeMap<(String, String), String> = Default::default();
        for row in crate::store_graph::rows(
            db,
            "MATCH (e:ProviderEmail {workspace_id: $workspace}) RETURN e.provider AS provider, e.external_id AS external_id, e.email AS email              UNION ALL              MATCH (o:OAuthAccount {workspace_id: $workspace, state: 'active'}) RETURN o.provider AS provider, o.external_id AS external_id, o.email AS email",
            crate::store_graph::Params::from([("workspace".into(), crate::store_graph::json_to_value(&json!(workspace)))]),
        )? {
            let email = s(&row, "email").to_lowercase();
            if !email.is_empty() {
                emails.insert(
                    (s(&row, "provider").to_string(), s(&row, "external_id").to_string()),
                    email,
                );
            }
        }
        let permissions = crate::store_graph::rows(
            db,
            "MATCH (sp:SourcePermission) WHERE sp.connector_id IN $connectors RETURN sp.connector_id AS connector_id, sp.source_id AS source_id, sp.principals AS principals",
            crate::store_graph::Params::from([(
                "connectors".into(),
                crate::store_graph::json_to_value(&json!(
                    connectors.iter().map(|c| s(c, "id").to_string()).collect::<Vec<_>>()
                )),
            )]),
        )?;
        let kind_of: BTreeMap<&str, &str> = connectors
            .iter()
            .map(|c| (s(c, "id"), s(c, "kind")))
            .collect();
        let present: BTreeSet<(String, String)> = crate::store_graph::rows(
            db,
            "MATCH (d:ConnectorDoc {present: true}) WHERE d.connector_id IN $connectors RETURN d.connector_id AS connector_id, d.id AS id",
            crate::store_graph::Params::from([(
                "connectors".into(),
                crate::store_graph::json_to_value(&json!(
                    connectors.iter().map(|c| s(c, "id").to_string()).collect::<Vec<_>>()
                )),
            )]),
        )?
        .iter()
        .map(|row| (s(row, "connector_id").to_string(), s(row, "id").to_string()))
        .collect();
        for permission in permissions {
            let key = (
                s(&permission, "connector_id").to_string(),
                s(&permission, "source_id").to_string(),
            );
            if !present.contains(&key) {
                continue;
            }
            let provider = kind_of.get(s(&permission, "connector_id")).copied().unwrap_or("");
            let grants: Value =
                serde_json::from_str(s(&permission, "principals")).unwrap_or(Value::Null);
            for identity in &identities {
                if s(identity, "provider") != provider {
                    continue;
                }
                let external = s(identity, "external_id");
                let email = emails
                    .get(&(provider.to_string(), external.to_string()))
                    .cloned()
                    .unwrap_or_default();
                let granted = grants.as_array().into_iter().flatten().any(|grant| {
                    (grant["type"] == "user" && grant["id"] == external)
                        || (grant["type"] == "email"
                            && !email.is_empty()
                            && s(grant, "email").eq_ignore_ascii_case(&email))
                });
                if granted {
                    sources.insert(crate::invalidation::connector_document_id(
                        &key.0, &key.1,
                    ));
                    break;
                }
            }
        }
        Ok(())
    })?;
    Ok(sources.into_iter().collect())
}


/// Additive column migration for tables created by an earlier schema:
/// `CREATE TABLE IF NOT EXISTS` never alters an existing table, so a column
/// added later is appended here when `pragma_table_info` lacks it.
pub fn ensure_column(db: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    let present = rows(db, &format!("PRAGMA table_info({table})"), [])?
        .iter()
        .any(|row| s(row, "name") == column);
    if !present {
        db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"))?;
    }
    Ok(())
}

pub fn authorize_hash(
    db: &Connection,
    workspace: &str,
    hash: &str,
    permission: &str,
) -> Result<Principal> {
    let owner: String = db
        .query_row(
            "SELECT capability_hash FROM workspaces WHERE id=?",
            [workspace],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(Error::denied)?;
    let principal = if owner.as_bytes().ct_eq(hash.as_bytes()).into() {
        Principal {
            workspace: workspace.into(),
            hash: hash.into(),
            credential: "owner".into(),
            role: "owner".into(),
        }
    } else {
        let row = match one(
            db,
            "SELECT id,role,expires_at,revoked_at,capability_hash FROM credentials WHERE workspace_id=? AND capability_hash=?",
            params![workspace, hash],
        )? {
            Some(row) => row,
            // Not a workspace credential: an installed extension may know the
            // token, such as a browser session.
            None => crate::extension::installed()
                .session_credential(db, workspace, hash)?
                .ok_or_else(Error::denied)?,
        };
        if !row["revoked_at"].is_null() {
            return Err(Error::denied());
        }
        // A member's credential follows the member and team sign-in on any
        // store that has the team tables, whichever edition reads it.
        if !crate::store_compat::credential_allowed(db, s(&row, "id"))?
            || !crate::extension::installed().credential_allowed(db, s(&row, "id"))?
        {
            return Err(Error::denied());
        }
        if let Some(expiry) = row["expires_at"].as_str()
            && graphicle_memory::temporal::instant(expiry).map_err(Error::bad)?
                <= chrono::Utc::now().fixed_offset()
        {
            return Err(Error::denied());
        }
        Principal {
            workspace: workspace.into(),
            hash: s(&row, "capability_hash").into(),
            credential: s(&row, "id").into(),
            role: s(&row, "role").into(),
        }
    };
    if !principal.permits(permission) {
        return Err(Error::denied());
    }
    Ok(principal)
}
pub fn repo(db: &Connection, p: &Principal, identifier: &str) -> Result<Value> {
    let mut row = one(
        db,
        "SELECT * FROM repo_projects WHERE id=? AND workspace_id=?",
        params![identifier, p.workspace],
    )?
    .ok_or_else(|| Error::bad("Repository not found in this workspace"))?;
    if !crate::graph_projection::repo_readable(db, p, identifier)? {
        return Err(Error::denied());
    }
    decode(&mut row, "metadata")?;
    Ok(row)
}
/// The current heads of `repository`: path → sha256. Heads live in the
/// graph content store; the SQL parameter stays only so the domains still
/// being migrated keep their signatures — the query runs on the graph.
pub fn heads(db: &Connection, repository: &str) -> Result<BTreeMap<String, String>> {
    let graph = graph_for(db)?;
    store_content::heads(&graph, repository)
}
/// Append to the project's event log. Events live in the graph content
/// store; like [`heads`], the SQL parameter is transitional.
pub fn event(db: &Connection, repository: &str, kind: &str, value: Value) -> Result<()> {
    let graph = graph_for(db)?;
    store_content::append_event(&graph, repository, kind, &value)
}
pub fn invalidate(db: &Connection, repository: &str) -> Result<()> {
    crate::projection::enqueue(db, repository, "source_or_permission_state_changed")?;
    graphicle_memory::cache::clear(repository).map_err(Error::bad)?;
    // The query cache lives in the graph store now, and its request hashes
    // embed the source state, so a changed source view can never hit a stale
    // entry; nothing to delete here (the 500-entry trim bounds growth).
    Ok(())
}

#[cfg(test)]
mod analysis_row_tests {
    use super::analysis_row;
    use serde_json::{Value, json};

    const REVISIONS: [&str; 2] = ["complete-revision", "limited-revision"];

    #[test]
    fn a_complete_analysis_keeps_the_structure_revision() {
        let limits = crate::limits::Limits::default().analysis_request();
        let (revision, data) =
            analysis_row("main.py", "def main():\n    work()\n", &limits, REVISIONS).unwrap();
        let analysis: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(revision, REVISIONS[0]);
        assert_eq!(analysis["complete"], true, "{analysis}");
        assert!(analysis.get("limited").is_none());
    }

    #[test]
    fn a_limited_analysis_is_kept_under_the_limited_revision() {
        let (revision, data) = analysis_row(
            "main.py",
            "def main():\n    one()\n    two()\n",
            &json!({"calls":1}),
            REVISIONS,
        )
        .unwrap();
        let analysis: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(revision, REVISIONS[1]);
        assert_eq!(analysis["limited"], "calls");
        assert_eq!(analysis["calls"].as_array().unwrap().len(), 1);
    }

    /// Any other failure of a file's analysis records the file as skipped,
    /// with the reason, instead of failing the sync (here the analysis
    /// refuses its request).
    #[test]
    fn a_failed_analysis_is_recorded_as_skipped_with_its_reason() {
        let (revision, data) = analysis_row(
            "main.py",
            "def main():\n    work()\n",
            &json!({"calls":0}),
            REVISIONS,
        )
        .unwrap();
        let analysis: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(revision, REVISIONS[1]);
        assert_eq!(
            analysis["skipped"],
            "Analysis limit calls must be a positive integer"
        );
        assert_eq!(analysis["complete"], false);
        assert_eq!(analysis["symbols"], json!([]));
        assert_eq!(
            analysis["issues"][0],
            "Not analysed: Analysis limit calls must be a positive integer; searchable as text only"
        );
    }

    /// A failure that is not about the file is not cached as the file's.
    #[test]
    fn installation_failures_fail_and_transient_ones_are_retried() {
        assert!(super::parser_unavailable(
            "Bundled parser unavailable: rust"
        ));
        assert!(super::parser_unavailable("Parser grammar is incompatible"));
        assert!(!super::parser_unavailable("Source contains syntax errors"));
        assert!(super::transient_failure("Source parsing did not complete"));
        assert!(super::transient_failure(
            "Native operation failed; no result accepted"
        ));
        assert!(super::transient_failure(
            "Source analysis failed: parse timeout exceeded"
        ));
        assert!(!super::transient_failure(
            "Analysis limit calls must be a positive integer"
        ));
    }
}

#[cfg(test)]
mod lock_site_tests {
    use super::crate_relative;

    #[test]
    fn lock_report_sites_are_relative_to_their_crate() {
        let absolute = format!("{}/src/jobs.rs", env!("CARGO_MANIFEST_DIR"));
        assert_eq!(crate_relative(&absolute), "src/jobs.rs");
        assert_eq!(crate_relative("src/jobs.rs"), "src/jobs.rs");
        assert_eq!(
            crate_relative("/builds/pro/src/service/team/oidc.rs"),
            "src/service/team/oidc.rs"
        );
        assert_eq!(
            crate_relative("/home/ci/.cargo/registry/src/index.example-1/dep-1.0/src/lib.rs"),
            "src/lib.rs"
        );
    }
}

#[cfg(test)]
mod graph_pairing_tests {
    #[test]
    fn paired_graph_store_stays_writable() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path()).unwrap();
        // The Store's own handle writes.
        store
            .graph()
            .transaction(|db| {
                db.query("CREATE (:Artifact {workspace: 'w', kind: 'k', id: 'a'})")?;
                Ok(())
            })
            .unwrap();
        // And so does the handle a SQL connection resolves to.
        let db = store.connect().unwrap();
        let g = super::graph_for(&db).unwrap();
        g.transaction(|db| {
            db.query("CREATE (:Artifact {workspace: 'w', kind: 'k', id: 'b'})")?;
            Ok(())
        })
        .unwrap();
        let n = g
            .read(|db| {
                crate::store_graph::one(
                    db,
                    "MATCH (a:Artifact) RETURN count(a) AS n",
                    crate::store_graph::Params::new(),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(n["n"], serde_json::json!(2));
    }
}
