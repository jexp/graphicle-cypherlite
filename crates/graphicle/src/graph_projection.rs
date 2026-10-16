//! Projection of a repository head set into the typed code graph and its
//! bi-temporal graph-native record (cypherlite-migration.md §2.4):
//! `Symbol` identity nodes keyed `(repository_id, stable_id)`, one
//! `SymbolVersion:Live` per live version, typed relationships
//! (`CALLS`/`CONTAINS`/… from the extractor's relation strings) type-flipped
//! to `*_HIST` on close, and the `Episode` DAG via `FOLLOWS`. One resident
//! graph per `(repository_id, episode)`.
use crate::{
    Error, Result, digest, now,
    store::{Principal, Store, one, repo, rows, s},
    store_graph::{self, GraphStore, Params, json_to_value},
    string,
};
use cypherlite::{Connection as GraphConnection, Value as GValue};
use graphicle::{
    build::{BuildOptions, Diagnostics, build_owned},
    extract::ExtractOptions,
    graph::Graph,
    session::{GraphHandle, registry},
};
use graphicle_memory::graph::identity::{self, IdentityOptions};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{Mutex, OnceLock, PoisonError},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectionInfo {
    pub episode: String,
    pub nodes: usize,
    pub edges: usize,
    pub graph_ref: String,
    pub replayed: bool,
}

pub fn graph_ref(repository: &str, episode: &str) -> String {
    format!("{repository}:{episode}")
}

/// Deterministic digest of the repository head set (`path → sha256`).
pub fn episode_of(heads: &BTreeMap<String, String>) -> String {
    digest(&json!(heads))
}

// ---------------------------------------------------------------------------
// Graph-native persistence (cypherlite-migration.md §2.4). The projection's
// bi-temporal state — episodes, symbol identity nodes, their versions and the
// typed relationships between them — lives in the [`GraphStore`] (a
// CypherLite store beside the SQLite file, `docs/store-api.md`). Functions
// whose collaborators have not migrated yet (content store, symbols,
// invalidation) also take the rusqlite `Connection`; the extra parameter
// goes away with the last SQL collaborator.
// `gf_episodes` is additionally mirrored into SQLite (slim rows, written by
// `persist_with_report`) so those un-migrated readers keep working until
// their own migration lands; the CypherLite store is the authority every
// function here reads.
// ---------------------------------------------------------------------------

/// `begin` → `f` → `commit` on the connection, rolling back on error.
/// Unlike [`GraphStore::transaction`] there is no conflict retry and `f` is
/// `FnOnce`: the projection's writes already run under the store's
/// single-writer lock, so a write-write conflict cannot occur here.
fn graph_commit<T>(
    g: &mut GraphConnection,
    f: impl FnOnce(&mut GraphConnection) -> Result<T>,
) -> Result<T> {
    g.begin()?;
    match f(g) {
        Ok(value) => {
            g.commit()?;
            Ok(value)
        }
        Err(error) => {
            let _ = g.rollback();
            Err(error)
        }
    }
}

/// Relationship type for an extractor relation string: uppercased, with any
/// non-identifier character folded to `_` (extractor relations are already
/// identifier-like: `calls`, `imports_from`, `defined_in`). A closed edge's
/// history keeps the same properties under the `<TYPE>_HIST` type (the
/// spike's type-flip close, E3).
fn relation_type(relation: &str) -> String {
    let mut out = String::with_capacity(relation.len());
    for ch in relation.chars() {
        out.push(if ch.is_ascii_alphanumeric() {
            ch.to_ascii_uppercase()
        } else {
            '_'
        });
    }
    if out.is_empty() {
        out.push_str("REL");
    }
    if out.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
        out.insert(0, 'R');
    }
    out
}

fn history_type(relation: &str) -> String {
    format!("{}_HIST", relation_type(relation))
}

/// Query parameters from `(name, value)` pairs.
fn gparams<const N: usize>(pairs: [(&str, GValue); N]) -> Params {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn gs(value: &str) -> GValue {
    GValue::String(value.into())
}

fn gi(value: i64) -> GValue {
    GValue::Int(value)
}

fn gj(value: &Value) -> GValue {
    json_to_value(value)
}

/// Rows of a read-only Cypher statement against the projection's graph store
/// (`params` are JSON values). A probe for tests and tooling; the module's
/// own reads go through the typed functions.
pub fn graph_rows(
    store: &GraphStore,
    cypher: &str,
    params: &[(&str, Value)],
) -> Result<Vec<Value>> {
    let params: Params = params.iter().map(|(k, v)| (k.to_string(), gj(v))).collect();
    store.read(|g| store_graph::rows(g, cypher, params))
}

/// One write statement against the projection's graph store, in its own
/// transaction (test and tooling probe, e.g. tampering with a live row to
/// check a repair).
pub fn graph_write(store: &GraphStore, cypher: &str, params: &[(&str, Value)]) -> Result<()> {
    let params: Params = params.iter().map(|(k, v)| (k.to_string(), gj(v))).collect();
    let mut g = store.connection()?;
    graph_commit(&mut g, |g| {
        g.query_with_params(cypher, params)?;
        Ok(())
    })
}

pub fn current_episode(db: &Connection, repository: &str) -> Result<String> {
    Ok(episode_of(&crate::store::heads(db, repository)?))
}

/// Durable per-repository extraction cache next to the SQLite file. Entries
/// are keyed by content and repository-relative path, so they hit across the
/// fresh materialization directory of every build. In-memory databases have
/// no directory and build without a cache.
pub fn cache_root(db: &Connection, repository: &str) -> Option<PathBuf> {
    let database = db.path().filter(|p| !p.is_empty())?;
    let directory = std::path::Path::new(database).parent()?;
    if !repository
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(directory.join("gf-cache").join(repository))
}

/// A resolver cache must never survive a change of engine implementation.
/// The executable's size, modification time and its first and last 4 MiB
/// identify the build: the Mach-O UUID or ELF build ID sits in the headers and
/// changes with any code change. Hashing a whole debug build (hundreds of
/// megabytes) took longer than the store's busy timeout.
fn engine_fingerprint() -> Option<&'static str> {
    static FINGERPRINT: OnceLock<Option<String>> = OnceLock::new();
    FINGERPRINT
        .get_or_init(|| {
            use sha2::{Digest, Sha256};
            use std::io::{Read, Seek, SeekFrom};
            const SPAN: u64 = 4 << 20;
            let path = std::env::current_exe().ok()?;
            let mut file = std::fs::File::open(&path).ok()?;
            let meta = file.metadata().ok()?;
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            let mut sha = Sha256::new();
            sha.update(meta.len().to_le_bytes());
            sha.update(modified.to_le_bytes());
            let mut buffer = vec![0u8; SPAN as usize];
            for start in [0, meta.len().saturating_sub(SPAN)] {
                file.seek(SeekFrom::Start(start)).ok()?;
                let mut read = 0;
                while read < buffer.len() {
                    let n = file.read(&mut buffer[read..]).ok()?;
                    if n == 0 {
                        break;
                    }
                    read += n;
                }
                sha.update(&buffer[..read]);
            }
            Some(format!("{:x}", sha.finalize()))
        })
        .as_deref()
}

/// Extract and build the typed graph for the current head set.
pub fn build_graph(db: &Connection, repository: &str) -> Result<(String, Graph)> {
    let (episode, graph, _) = build_graph_with_report(db, repository)?;
    Ok((episode, graph))
}

/// `build_graph` plus the extraction cache report of this run
/// (`{cached, changed, unchanged, unsupported}`). The report describes the
/// run, not the graph, so it is stored with the episode rather than inside
/// the graph attributes.
pub fn build_graph_with_report(
    db: &Connection,
    repository: &str,
) -> Result<(String, Graph, Value)> {
    let heads = crate::store::heads(db, repository)?;
    let episode = episode_of(&heads);
    // Keep the materialization root stable so the file-description and
    // root-sensitive resolver caches can actually be reused. Serialize before
    // preparing it: another build must not mutate files during extraction.
    static EXTRACTION: Mutex<()> = Mutex::new(());
    let _serial = EXTRACTION.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = cache_root(db, repository);
    let dir = crate::phase::phase("graph_materialize", || {
        // Source objects are CAS nodes in the graph store
        // (store_content owns Object writes via put_objects/activate).
        let mut graph_db = crate::store::graph_for(db)?.connection()?;
        crate::graph_materialize::prepare(&mut graph_db, repository, &heads, cache.as_deref())
    })?;
    // Canonical root: node identities derive from paths relative to it, and a
    // root that crosses a symlink would make cached and fresh records diverge.
    let root = std::fs::canonicalize(dir.path())?;
    let files: Vec<PathBuf> = heads.keys().map(|p| root.join(p)).collect();
    // A contended cross-process cache gets an isolated uncached tree.
    let cache = cache.filter(|_| dir.report.cached);
    if let Some(cache) = &cache {
        std::fs::create_dir_all(cache)?;
    }
    let (extraction, report) = crate::phase::phase("graph_extract", || {
        graphicle::incremental::extract(
            &files,
            &ExtractOptions {
                root: Some(root.clone()),
                use_cache: cache.is_some(),
                cache_root: cache.clone(),
                // Receiver-aware method resolution: additive `calls`
                // edges labelled `derivation: inferred` + `receiver_evidence`;
                // ambiguous receivers land in `attributes.unresolved_calls`.
                receiver_resolution: true,
                test_identities: true,
                ..Default::default()
            },
            cache
                .as_ref()
                .filter(|_| engine_fingerprint().is_some())
                .map(|p| p.join("resolution"))
                .as_deref(),
            engine_fingerprint().unwrap_or("uncached"),
        )
    })?;
    let extraction_summary = json!({
        "cached": cache.is_some(),
        "materialization": dir.report,
        "changed": report.parsing.changed.len(),
        "unchanged": report.parsing.unchanged + report.skipped_parsing,
        "unsupported": report.parsing.unsupported,
        "resolution": {"dirty_files":report.dirty_files,"resolved_files":report.resolved_files,
            "context_files":report.context_files,"reused_files":report.reused_files,
            "skipped_parsing":report.skipped_parsing,
            "components":report.components,"largest_component":report.largest_component,
            "fallback":report.fallback},
    });
    drop(report);
    drop(files);
    // Multigraph keyed by relation: the extractors emit parallel edges with
    // distinct relations between one pair (`calls`+`contains` for a nested
    // function, `inherits`+`uses`, `imports`+`imports_from`); the parity
    // non-multigraph build folds them into one edge with mixed attributes.
    // `build_owned` consumes the extraction so the records are moved into the
    // graph instead of being copied (the borrowing `build` holds both).
    let mut graph = crate::phase::phase("graph_assemble", || {
        build_owned(
            vec![extraction],
            &BuildOptions {
                directed: true,
                root: Some(root.clone()),
                multigraph: true,
                ..Default::default()
            },
            &mut Diagnostics::default(),
            None,
        )
    })?;
    // Durable identity: every persisted node carries a content-independent
    // `stable_id` and a content `revision`; every edge its `derivation`.
    // A PR scratch project's ids are scoped by its folder project, so a symbol has one `stable_id` in both; its nodes still name the
    // scratch project as their `repo`, where their episode is recorded.
    let scope = identity_scope(db, repository)?;
    crate::phase::phase("graph_identity", || {
        identity::annotate(
            &mut graph,
            &IdentityOptions {
                repo: scope.clone(),
                root: Some(root.clone()),
                strip_prefix: Some(root.to_string_lossy().into_owned()),
                sources: BTreeMap::new(),
            },
        )
    })?;
    if scope != repository {
        for node in graph.nodes_mut() {
            node.attributes.insert("repo".into(), json!(repository));
        }
    }
    Ok((episode, graph, extraction_summary))
}

/// The identity scope of `repository`'s `stable_id`s: its own
/// id, or for a PR scratch project linked to a folder project
/// ([`crate::folders::scratch_of`]) that project's id.
pub fn identity_scope(db: &Connection, repository: &str) -> Result<String> {
    Ok(crate::folders::scratch_of(db, repository)?.unwrap_or_else(|| repository.to_string()))
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn edge_key_value(text: &str) -> Value {
    if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(text).unwrap_or_else(|_| json!(text))
    }
}

/// The columns every reader of the episode row gets (`attributes` stays the
/// canonical JSON string; `directed`/`multigraph` stay 0/1 integers).
const EPISODE_COLUMNS: &str = "e.episode AS episode, e.sequence AS sequence, e.created_at AS created_at, \
     e.node_count AS node_count, e.edge_count AS edge_count, e.directed AS directed, \
     e.multigraph AS multigraph, e.attributes AS attributes, e.parent_episode AS parent_episode";

/// The episode row, read inside an existing graph connection (a caller's
/// transaction sees its own writes).
fn episode_row_g(
    g: &mut GraphConnection,
    repository: &str,
    episode: &str,
) -> Result<Option<Value>> {
    store_graph::one(
        g,
        &format!("MATCH (e:Episode {{repository_id:$r, episode:$e}}) RETURN {EPISODE_COLUMNS}"),
        gparams([("r", gs(repository)), ("e", gs(episode))]),
    )
}

pub fn episode_row(store: &GraphStore, repository: &str, episode: &str) -> Result<Option<Value>> {
    store.read(|g| episode_row_g(g, repository, episode))
}

/// Every episode of the repository in record order, inside an existing
/// graph connection.
fn episode_history_g(g: &mut GraphConnection, repository: &str) -> Result<Vec<Value>> {
    store_graph::rows(
        g,
        &format!(
            "MATCH (e:Episode {{repository_id:$r}}) RETURN {EPISODE_COLUMNS} ORDER BY e.sequence"
        ),
        gparams([("r", gs(repository))]),
    )
}

/// Whether `episode` has been recorded for this repository (G04 arm gate).
pub fn is_recorded(store: &GraphStore, repository: &str, episode: &str) -> Result<bool> {
    Ok(episode_row(store, repository, episode)?.is_some())
}

/// Persist `graph` as `episode`: close live rows that disappeared or changed,
/// insert new ones, never delete. Idempotent for an already recorded episode.
pub fn persist(
    db: &Connection,
    store: &GraphStore,
    repository: &str,
    episode: &str,
    graph: &Graph,
) -> Result<bool> {
    persist_with_report(db, store, repository, episode, graph, None)
}

/// Episode attribute key holding the extraction run report; stripped on `load`.
pub const EXTRACTION_RUN: &str = "extraction_run";
/// Complete source manifest of the episode, including documents and files
/// the AST extractor does not understand. This is storage metadata, not a
/// graph attribute returned by `load`.
const SOURCE_HEADS: &str = "source_heads";

/// The source manifest stored with a recorded episode, or `None` for an
/// episode recorded without one (older stores, or a graph persisted for a
/// head set other than the current one). One row read: unlike
/// [`episode_heads`], it never searches the retained sync manifests, so a
/// query path can afford it.
pub fn stored_episode_heads(
    store: &GraphStore,
    repository: &str,
    episode: &str,
) -> Result<Option<BTreeMap<String, String>>> {
    let row = episode_row(store, repository, episode)?
        .ok_or_else(|| Error::bad("Unknown graph episode for this repository"))?;
    let attributes: Value = serde_json::from_str(s(&row, "attributes"))?;
    Ok(match attributes.get(SOURCE_HEADS) {
        Some(value) => Some(serde_json::from_value(value.clone())?),
        None => None,
    })
}

/// Exact source revisions for a recorded episode. Older stores can recover
/// them from retained sync manifests; no current file is substituted for a
/// historical file merely because the paths match.
pub fn episode_heads(
    db: &Connection,
    store: &GraphStore,
    repository: &str,
    episode: &str,
) -> Result<Option<BTreeMap<String, String>>> {
    if let Some(heads) = stored_episode_heads(store, repository, episode)? {
        return Ok(Some(heads));
    }
    let graph = crate::store::graph_for(db)?;
    let current = crate::store_content::heads(&graph, repository)?;
    if episode_of(&current) == episode {
        return Ok(Some(current));
    }
    for manifest in crate::store_content::snapshot_manifests(&graph, repository)? {
        let manifest: BTreeMap<String, Value> = serde_json::from_str(&manifest)?;
        let heads: BTreeMap<String, String> = manifest
            .into_iter()
            .map(|(path, entry)| (path, s(&entry, "sha256").to_string()))
            .collect();
        if episode_of(&heads) == episode {
            return Ok(Some(heads));
        }
    }
    Ok(None)
}

pub fn persist_with_report(
    db: &Connection,
    store: &GraphStore,
    repository: &str,
    episode: &str,
    graph: &Graph,
    extraction: Option<&Value>,
) -> Result<bool> {
    let stamp = now();
    let heads = crate::store::heads(db, repository)?;
    let mut g = store.connection()?;
    // The fingerprint plan reads the graph-side ProjectionGroup cache.
    let delta = crate::graph_delta::Plan::new(&mut g, repository, graph)?;
    let mut attributes = episode_attributes(graph, extraction);
    if let Some(report) = attributes
        .get_mut(EXTRACTION_RUN)
        .and_then(Value::as_object_mut)
    {
        report.insert("persistence".into(), serde_json::to_value(&delta.stats)?);
    }
    let head_set = episode_of(&heads) == episode;
    if head_set {
        attributes[SOURCE_HEADS] = json!(heads);
    }
    let attributes = graphicle_memory::text::canonical(&attributes);
    let recorded = record_episode(
        &mut g,
        repository,
        episode,
        graph,
        &attributes,
        &stamp,
        &delta,
    )?;
    let Some(recorded) = recorded else {
        return Ok(false);
    };
    // The timeline assertion and the group digests publish after the
    // episode's rows, in one commit.
    let instant = crate::temporal_graph::instant(&stamp)?;
    graph_commit(&mut g, |g| {
        crate::temporal_graph::observe(
            g,
            repository,
            episode,
            "default",
            instant,
            instant,
            "projection",
        )?;
        delta.finish(g, repository)
    })?;
    // The transition's SQLite mirror of the episode row: the timeline's
    // baseline and selector lookups still read `gf_episodes` until that
    // domain migrates. The CypherLite store is the authority; the mirror is
    // dropped with the last SQL reader.
    db.execute(
        "INSERT OR IGNORE INTO gf_episodes(repository_id,episode,sequence,created_at,node_count,edge_count,directed,multigraph,attributes,parent_episode) VALUES(?,?,?,?,?,?,?,?,?,?)",
        params![
            repository,
            episode,
            recorded.sequence,
            stamp,
            graph.node_count() as i64,
            graph.edge_count() as i64,
            graph.is_directed(),
            graph.is_multigraph(),
            attributes,
            recorded.parent,
        ],
    )?;
    // Collaborators that still live in SQLite (their domains migrate in
    // their own steps): dependency bookkeeping and the live-symbol tables.
    // Live nodes are fresh derivations of their source files.
    let workspace = crate::invalidation::workspace_of(db, repository)?;
    crate::invalidation::record_graph_nodes(
        db,
        store,
        &workspace,
        repository,
        recorded
            .inserted
            .iter()
            .map(|(id, relpath)| (id.as_str(), relpath.as_deref())),
        recorded.closed.iter().map(String::as_str),
        recorded.partial,
    )?;
    // The live-symbol table follows the current head set's episode, by the
    // files this projection rewrote when it described the previous one, and
    // pins quotes that waited for this content.
    if head_set {
        crate::symbols::record(
            db,
            store,
            repository,
            episode,
            graph,
            &heads,
            recorded.parent.as_deref(),
            delta.dirty(),
        )?;
    }
    Ok(true)
}

/// Rows per UNWIND batch in the projection's bulk writes (migration plan §4:
/// plain `UNWIND $rows` batches inside one explicit transaction — the spike
/// measured `CALL {} IN TRANSACTIONS` ~300x slower on this engine, E6).
const BATCH: usize = 1000;

/// What recording an episode changed, for the episode mirror and the
/// collaborators that still live in SQLite (timeline, invalidation, symbols).
struct Recorded {
    sequence: i64,
    parent: Option<String>,
    /// `(node_id, relpath)` of the versions this episode inserted.
    inserted: Vec<(String, Option<String>)>,
    /// Node ids whose live version this episode closed.
    closed: Vec<String>,
    /// Whether any live node was carried over unchanged.
    partial: bool,
}

/// A live relationship this episode closed: what the contradiction check
/// needs once the relationship itself is history.
struct ClosedEdge {
    source: String,
    target: String,
    relation: String,
    key: String,
    attributes: String,
}

/// Stable text reference of one edge for a contradiction marker:
/// `[source, target, relation, key]` as JSON (row ids do not exist in the
/// graph store).
fn edge_ref(source: &str, target: &str, relation: &str, key: &str) -> String {
    json!([source, target, relation, key]).to_string()
}

/// A live relationship of the dirty owner groups, held for the diff.
struct LiveEdge {
    digest: Digest,
    attributes: String,
    source: String,
    target: String,
    relation: String,
    key: String,
}

/// Record `graph` as `episode`: create the `Episode` node (linked to its
/// parent by `FOLLOWS`), close live symbol versions (`REMOVE v:Live`) and
/// live relationships (type-flip to `<TYPE>_HIST`) that disappeared or
/// changed, and insert the new ones. Never deletes. `None` when the episode
/// is already recorded. Runs inside the caller's graph transaction
/// ([`graph_tx`]), so a failure leaves no partial episode behind.
fn record_episode(
    g: &mut GraphConnection,
    repository: &str,
    episode: &str,
    graph: &Graph,
    attributes: &str,
    stamp: &str,
    delta: &crate::graph_delta::Plan,
) -> Result<Option<Recorded>> {
    // The idempotency gate: the Episode node is written LAST, so a crash
    // mid-record leaves versions and relationships but no episode, and a
    // retry converges — the diff below is recomputed against whatever the
    // partial record left live, digest-equal rows are skipped, and the
    // markers (unresolved calls, contradictions) are MERGE'd.
    if episode_row_g(g, repository, episode)?.is_some() {
        return Ok(None);
    }
    let (history, live_nodes, live_edges) = (|g: &mut GraphConnection| -> Result<_> {
        let history = episode_history_g(g, repository)?;
        let owners = gj(&json!(delta.dirty()));
        // Live state of the dirty owner groups: one entry per live version,
        // compared by a digest of its canonical attributes. Identity is the
        // (repository_id, stable_id) symbol key; a version carries the node
        // id its episode knew, so a re-keyed symbol (same stable_id, new
        // node_id) is one Symbol with a fresh version.
        let mut live_nodes: HashMap<String, Digest> = HashMap::new();
        for row in store_graph::rows(
            g,
            "MATCH (s:Symbol {repository_id:$r})-[:HAS_VERSION]->(v:Live) WHERE v.owner IN $owners \
             RETURN v.node_id AS node_id, v.data AS data",
            gparams([("r", gs(repository)), ("owners", owners.clone())]),
        )? {
            live_nodes.insert(
                s(&row, "node_id").to_string(),
                digest_of(s(&row, "data").as_bytes()),
            );
        }
        // Live relationships of the dirty groups, keyed by edge identity;
        // the endpoints and attributes stay attached for the contradiction
        // check of a closed edge.
        let mut live_edges: HashMap<Vec<u8>, LiveEdge> = HashMap::new();
        let mut identity = Vec::new();
        for row in store_graph::rows(
            g,
            "MATCH (a:Symbol {repository_id:$r})-[r]->(b:Symbol) \
             WHERE r.valid_from_episode IS NOT NULL AND r.valid_to_episode IS NULL \
               AND r.owner IN $owners \
             RETURN r.source_node_id AS source, r.target_node_id AS target, \
             r.relation AS relation, r.key AS key, r.data AS data",
            gparams([("r", gs(repository)), ("owners", owners)]),
        )? {
            edge_identity(
                &mut identity,
                s(&row, "source"),
                s(&row, "target"),
                s(&row, "relation"),
                s(&row, "key"),
            );
            live_edges.insert(
                identity.clone(),
                LiveEdge {
                    digest: digest_of(s(&row, "data").as_bytes()),
                    attributes: s(&row, "data").to_string(),
                    source: s(&row, "source").to_string(),
                    target: s(&row, "target").to_string(),
                    relation: s(&row, "relation").to_string(),
                    key: s(&row, "key").to_string(),
                },
            );
        }
        Ok((history, live_nodes, live_edges))
    })(g)?;
    let sequence = history
        .last()
        .map_or(1, |e| e["sequence"].as_i64().unwrap_or(0) + 1);
    // History is a DAG over tree digests: the parent is whatever was `latest`
    // when this episode was recorded (merges are not reconstructed here).
    let parent = history.last().map(|e| s(e, "episode").to_string());
    let sequences: HashMap<String, i64> = history
        .iter()
        .map(|e| {
            (
                s(e, "episode").to_string(),
                e["sequence"].as_i64().unwrap_or(0),
            )
        })
        .chain(std::iter::once((episode.to_string(), sequence)))
        .collect();

    let nodes = graph.nodes();
    let node_ids: Vec<String> = nodes.iter().map(|node| text(&node.id)).collect();
    let mut live_nodes = live_nodes;
    let mut live_edges = live_edges;
    let mut close_nodes: Vec<String> = Vec::new();
    let mut insert_nodes: Vec<GValue> = Vec::new();
    let mut inserted: Vec<(String, Option<String>)> = Vec::new();
    let mut attributes_text = String::new();
    for (index, node) in nodes.iter().enumerate() {
        if !delta.changed(&node.attributes) {
            continue;
        }
        canonical_into(&mut attributes_text, &node.attributes);
        match live_nodes.remove(&node_ids[index]) {
            Some(existing) if existing == digest_of(attributes_text.as_bytes()) => {}
            other => {
                if other.is_some() {
                    close_nodes.push(node_ids[index].clone());
                }
                let stable_id = node
                    .attributes
                    .get("stable_id")
                    .and_then(Value::as_str)
                    .unwrap_or(&node_ids[index]);
                // `ord` reproduces the SQL rowid order (first appearance)
                // that `load` streamed: retrieval walks graph indices in
                // that order.
                let ord = (sequence << 32) | insert_nodes.len() as i64;
                insert_nodes.push(gj(&json!({
                    "node_id": node_ids[index],
                    "stable_id": stable_id,
                    "ord": ord,
                    "kind": node.attributes.get("kind").cloned().unwrap_or(Value::Null),
                    "qualified_name": node.attributes.get("qualified_name").cloned().unwrap_or(Value::Null),
                    "relpath": node.attributes.get("relpath").cloned().unwrap_or(Value::Null),
                    "data": attributes_text,
                    "owner": crate::graph_delta::owner(&node.attributes),
                })));
                inserted.push((
                    node_ids[index].clone(),
                    node.attributes
                        .get("relpath")
                        .and_then(Value::as_str)
                        .filter(|p| !p.is_empty())
                        .map(str::to_string),
                ));
            }
        }
    }
    let closed: Vec<String> = live_nodes.into_keys().collect();
    close_nodes.extend(closed.iter().cloned());

    let mut insert_edges: Vec<GValue> = Vec::new();
    let mut close_edges: Vec<GValue> = Vec::new();
    let mut identity = Vec::new();
    let mut key = String::new();
    for (source, target, edge) in graph.iter_edges() {
        if !delta.changed(&edge.attributes) {
            continue;
        }
        let relation = edge
            .attributes
            .get("relation")
            .map(text)
            .unwrap_or_default();
        key.clear();
        if !edge.key.is_null() {
            key.push_str(&edge.key.to_string());
        }
        edge_identity(
            &mut identity,
            &node_ids[source],
            &node_ids[target],
            &relation,
            &key,
        );
        canonical_into(&mut attributes_text, &edge.attributes);
        match live_edges.remove(&identity) {
            Some(existing) if existing.digest == digest_of(attributes_text.as_bytes()) => {}
            other => {
                if let Some(old) = other {
                    close_edges.push(gj(&json!({
                        "source": old.source,
                        "target": old.target,
                        "reltype": relation_type(&old.relation),
                        "histtype": history_type(&old.relation),
                        "key": old.key,
                    })));
                }
                // Provenance travels as relationship properties
                // (relpath/source_location/derivation replace the
                // gf_edge_sources join table).
                let field = |key: &str| {
                    edge.attributes
                        .get(key)
                        .filter(|v| !v.is_null())
                        .map(text)
                        .unwrap_or_default()
                };
                let stable_of = |index: usize| {
                    nodes[index]
                        .attributes
                        .get("stable_id")
                        .and_then(Value::as_str)
                        .unwrap_or(&node_ids[index])
                };
                insert_edges.push(gj(&json!({
                    "source_stable": stable_of(source),
                    "target_stable": stable_of(target),
                    "reltype": relation_type(&relation),
                    "props": {
                        "key": key,
                        "relation": relation,
                        "data": attributes_text,
                        "owner": crate::graph_delta::owner(&edge.attributes),
                        "relpath": field("source_file"),
                        "source_location": field("source_location"),
                        "derivation": field("derivation"),
                        "source_node_id": node_ids[source],
                        "target_node_id": node_ids[target],
                        "ord": (sequence << 32) | (insert_edges.len() as i64),
                    },
                })));
            }
        }
    }
    // Edges that disappeared: close them, then the contradiction check.
    let mut disappeared: Vec<ClosedEdge> = Vec::new();
    if !live_edges.is_empty() {
        let mut leftover: Vec<LiveEdge> = live_edges.into_values().collect();
        leftover.sort_unstable_by(|a, b| {
            (&a.source, &a.target, &a.relation, &a.key).cmp(&(
                &b.source,
                &b.target,
                &b.relation,
                &b.key,
            ))
        });
        for old in leftover {
            close_edges.push(gj(&json!({
                "source": old.source,
                "target": old.target,
                "reltype": relation_type(&old.relation),
                "histtype": history_type(&old.relation),
                "key": old.key,
            })));
            disappeared.push(ClosedEdge {
                source: old.source,
                target: old.target,
                relation: old.relation,
                key: old.key,
                attributes: old.attributes,
            });
        }
    }

    // The write phase: one committed transaction per chunk. The engine
    // commits a transaction as a single WAL frame, whose payload caps at
    // 64 MiB — far below a large episode — and the migration plan's chunk
    // size (1000 rows) keeps a frame comfortably under it. Inserts run
    // before closes and closes skip this episode's fresh rows
    // (`valid_from_episode <> $e`), so a failure mid-record leaves the
    // previously live state untouched; a retry re-diffs against what the
    // partial record left live (digest-equal rows are skipped, markers are
    // MERGE'd) and converges.
    for chunk in insert_nodes.chunks(BATCH) {
        graph_commit(g, |g| {
            g.query_with_params(
                "UNWIND $rows AS row \
                 MERGE (s:Symbol {repository_id:$r, stable_id: row.stable_id}) \
                 SET s.node_id = row.node_id, s.kind = row.kind, \
                     s.qualified_name = row.qualified_name, s.relpath = row.relpath \
                 CREATE (s)-[:HAS_VERSION]->(v:SymbolVersion:Live {repository_id:$r, stable_id: row.stable_id, \
                     node_id: row.node_id, data: row.data, owner: row.owner, ord: row.ord, \
                     valid_from_episode: $e, created_at: $stamp})",
                gparams([
                    ("r", gs(repository)),
                    ("rows", GValue::List(chunk.to_vec().into())),
                    ("e", gs(episode)),
                    ("stamp", gs(stamp)),
                ]),
            )?;
            Ok(())
        })?;
    }
    for chunk in insert_edges.chunks(BATCH) {
        graph_commit(g, |g| {
            g.query_with_params(
                "UNWIND $rows AS row \
                 MATCH (a:Symbol {repository_id:$r, stable_id: row.source_stable}), \
                       (b:Symbol {repository_id:$r, stable_id: row.target_stable}) \
                 CREATE (a)-[r:$(row.reltype)]->(b) \
                 SET r = row.props, r.valid_from_episode = $e, r.created_at = $stamp",
                gparams([
                    ("r", gs(repository)),
                    ("rows", GValue::List(chunk.to_vec().into())),
                    ("e", gs(episode)),
                    ("stamp", gs(stamp)),
                ]),
            )?;
            Ok(())
        })?;
    }
    for chunk in close_edges.chunks(BATCH) {
        graph_commit(g, |g| {
            // Closing an edge is a type-flip: the live relationship is
            // deleted and recreated as `<TYPE>_HIST` with the validity
            // window set, its properties snapshotted in the WITH before the
            // DELETE (reading a deleted relationship's properties errors in
            // this engine, spike E3).
            g.query_with_params(
                "UNWIND $rows AS row \
                 MATCH (a:Symbol {repository_id:$r})-[r]->(b:Symbol) \
                 WHERE r.source_node_id = row.source AND r.target_node_id = row.target \
                   AND type(r) = row.reltype AND r.key = row.key \
                   AND r.valid_from_episode IS NOT NULL AND r.valid_from_episode <> $e \
                   AND r.valid_to_episode IS NULL \
                 WITH a, b, r, row, properties(r) AS props DELETE r \
                 CREATE (a)-[h:$(row.histtype)]->(b) \
                 SET h = props, h.valid_to_episode = $e, h.expired_at = $stamp",
                gparams([
                    ("r", gs(repository)),
                    ("rows", GValue::List(chunk.to_vec().into())),
                    ("e", gs(episode)),
                    ("stamp", gs(stamp)),
                ]),
            )?;
            Ok(())
        })?;
    }
    for chunk in close_nodes.chunks(BATCH) {
        graph_commit(g, |g| {
            // Closing a version is a label flip: `REMOVE v:Live` leaves the
            // version node behind as history with its validity window set.
            g.query_with_params(
                "UNWIND $rows AS node_id \
                 MATCH (s:Symbol {repository_id:$r})-[:HAS_VERSION]->(v:Live {node_id: node_id}) \
                 WHERE v.valid_from_episode <> $e \
                 REMOVE v:Live SET v.valid_to_episode = $e, v.expired_at = $stamp",
                gparams([
                    ("r", gs(repository)),
                    ("rows", gj(&json!(chunk))),
                    ("e", gs(episode)),
                    ("stamp", gs(stamp)),
                ]),
            )?;
            Ok(())
        })?;
    }
    // The final transaction records the episode itself: unresolved calls,
    // contradiction markers, the Episode node and its FOLLOWS edge.
    graph_commit(g, |g| {
        persist_unresolved_calls(g, repository, episode, graph, stamp)?;
        if !disappeared.is_empty() {
            // Cardinality-one relations in the new graph: (relation,
            // subject) → the subject's targets with their edge references.
            let mut single_now: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
            for (source, target, edge) in graph.iter_edges() {
                let relation = edge
                    .attributes
                    .get("relation")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some((subject, other)) = single_valued(
                    relation,
                    &node_ids[source],
                    &node_ids[target],
                    &edge.attributes,
                ) {
                    let mut key = String::new();
                    if !edge.key.is_null() {
                        key.push_str(&edge.key.to_string());
                    }
                    single_now
                        .entry((relation.to_string(), subject))
                        .or_default()
                        .push((
                            other,
                            edge_ref(&node_ids[source], &node_ids[target], relation, &key),
                        ));
                }
            }
            for closed in &disappeared {
                record_contradiction(
                    g,
                    repository,
                    episode,
                    stamp,
                    graph,
                    &sequences,
                    &single_now,
                    closed,
                )?;
            }
        }
        g.query_with_params(
            "CREATE (e:Episode {repository_id:$r, episode:$e, sequence:$sequence, created_at:$stamp, \
             node_count:$nodes, edge_count:$edges, directed:$directed, multigraph:$multigraph, attributes:$attributes})",
            gparams([
                ("r", gs(repository)),
                ("e", gs(episode)),
                ("sequence", gi(sequence)),
                ("stamp", gs(stamp)),
                ("nodes", gi(graph.node_count() as i64)),
                ("edges", gi(graph.edge_count() as i64)),
                ("directed", gi(graph.is_directed() as i64)),
                ("multigraph", gi(graph.is_multigraph() as i64)),
                ("attributes", gs(attributes)),
            ]),
        )?;
        if let Some(parent) = &parent {
            g.query_with_params(
                "MATCH (e:Episode {repository_id:$r, episode:$e}), (p:Episode {repository_id:$r, episode:$p}) \
                 SET e.parent_episode = $p CREATE (e)-[:FOLLOWS]->(p)",
                gparams([("r", gs(repository)), ("e", gs(episode)), ("p", gs(parent))]),
            )?;
        }
        Ok(())
    })?;
    let partial = inserted.len() < nodes.len();
    Ok(Some(Recorded {
        sequence,
        parent,
        inserted,
        closed,
        partial,
    }))
}

type Digest = [u8; 16];

/// Truncated SHA-256 of an attribute blob: enough to compare live rows with
/// the new graph without holding every blob in memory.
fn digest_of(bytes: &[u8]) -> Digest {
    let hash = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0u8; 16];
    out.copy_from_slice(&hash.as_ref()[..16]);
    out
}

/// One allocation per lookup key, with length-prefixed columns: JSON strings
/// can contain any delimiter, including decoded NUL characters.
fn edge_identity(out: &mut Vec<u8>, source: &str, target: &str, relation: &str, key: &str) {
    out.clear();
    for part in [source, target, relation, key] {
        out.extend_from_slice(&(part.len() as u64).to_le_bytes());
        out.extend_from_slice(part.as_bytes());
    }
}

/// `serde_json::Value` serialized with object keys in byte order: the same
/// bytes as `graphicle_memory::text::canonical`, without cloning the tree.
struct Canonical<'a>(&'a Value);

impl Serialize for Canonical<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self.0 {
            Value::Object(fields) => CanonicalMap(fields).serialize(serializer),
            Value::Array(items) => serializer.collect_seq(items.iter().map(Canonical)),
            other => other.serialize(serializer),
        }
    }
}

struct CanonicalMap<'a>(&'a Map<String, Value>);

impl Serialize for CanonicalMap<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut sorted: Vec<(&String, &Value)> = self.0.iter().collect();
        sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut map = serializer.serialize_map(Some(sorted.len()))?;
        for (key, value) in sorted {
            map.serialize_entry(key, &Canonical(value))?;
        }
        map.end()
    }
}

pub(crate) struct CanonicalRecord<'a>(pub(crate) &'a graphicle::record::Attributes);
impl Serialize for CanonicalRecord<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut sorted = self.0.iter().collect::<Vec<_>>();
        sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut map = serializer.serialize_map(Some(sorted.len()))?;
        for (key, value) in sorted {
            map.serialize_entry(key, &Canonical(value))?;
        }
        map.end()
    }
}

/// Canonical JSON of `fields` written into `out` (cleared first).
fn canonical_into(out: &mut String, fields: &graphicle::record::Attributes) {
    out.clear();
    // serde_json only emits valid UTF-8.
    let mut bytes = std::mem::take(out).into_bytes();
    serde_json::to_writer(&mut bytes, &CanonicalRecord(fields))
        .expect("JSON values are serializable");
    *out = String::from_utf8(bytes).expect("serde_json emits UTF-8");
}

/// File extensions of languages whose classes extend at most one class.
/// Python, C++, Perl, Scala (`with`) and Lisp allow several and stay out.
const SINGLE_INHERITANCE: &[&str] = &[
    "java", "kt", "kts", "cs", "swift", "dart", "ts", "tsx", "js", "jsx", "mjs", "cjs", "php",
    "rb", "m", "mm", "cls", "trigger", "groovy", "vb",
];

/// The (subject, other) pair of an edge whose relation has declared
/// cardinality one on the subject side; `None` for multi-valued relations
/// (`calls`, `imports`, `references`, ...) which never auto-close except by
/// disappearance. `contains` is one-valued on the child, so the subject is
/// the target; `inherits`/`extends` only count in single-inheritance languages.
fn single_valued(
    relation: &str,
    source: &str,
    target: &str,
    attributes: &impl graphicle::record::Properties,
) -> Option<(String, String)> {
    match relation {
        "defined_in" => Some((source.to_string(), target.to_string())),
        "contains" => Some((target.to_string(), source.to_string())),
        "inherits" | "extends" => {
            let file = attributes.get("source_file").and_then(Value::as_str)?;
            let extension = std::path::Path::new(file)
                .extension()
                .and_then(|e| e.to_str())?
                .to_ascii_lowercase();
            SINGLE_INHERITANCE
                .contains(&extension.as_str())
                .then(|| (source.to_string(), target.to_string()))
        }
        _ => None,
    }
}

/// Interface-like declarations may extend several parents even in
/// single-inheritance languages; their `inherits` edges are not one-valued.
fn interface_like(graph: &Graph, node_id: &str) -> bool {
    graph
        .node_index(&json!(node_id))
        .and_then(|index| graph.nodes()[index].attributes.get("label"))
        .and_then(Value::as_str)
        .is_some_and(|label| {
            let label = label.to_ascii_lowercase();
            ["interface", "protocol", "trait", "mixin"]
                .iter()
                .any(|kind| label.contains(kind))
        })
}

/// A closed edge of a cardinality-one relation whose subject now points at
/// exactly one different target is a contradiction, not a disappearance.
/// Runs inside the episode's graph transaction.
#[allow(clippy::too_many_arguments)]
fn record_contradiction(
    g: &mut GraphConnection,
    repository: &str,
    episode: &str,
    stamp: &str,
    graph: &Graph,
    sequences: &HashMap<String, i64>,
    single_now: &HashMap<(String, String), Vec<(String, String)>>,
    closed: &ClosedEdge,
) -> Result<()> {
    let attributes: Map<String, Value> =
        serde_json::from_str(&closed.attributes).unwrap_or_default();
    let Some((subject, old_other)) = single_valued(
        &closed.relation,
        &closed.source,
        &closed.target,
        &attributes,
    ) else {
        return Ok(());
    };
    let Some(current) = single_now.get(&(closed.relation.clone(), subject.clone())) else {
        return Ok(());
    };
    // Several targets in the new graph mean the relation is not one-valued
    // for this subject in practice; nothing is marked.
    let [(new_other, new_ref)] = current.as_slice() else {
        return Ok(());
    };
    if *new_other == old_other || interface_like(graph, &subject) {
        return Ok(());
    }
    // Node ids are not durable identity: a file node can be re-keyed by an
    // unrelated change (`base` ↔ `base_java_base`). The same entity under a
    // new id is a re-identification, never a contradiction.
    let new_stable = graph
        .node_index(&json!(new_other))
        .and_then(|index| graph.nodes()[index].attributes.get("stable_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    // The closed edge's target identified by its most recent version.
    let mut old_stable = None;
    let mut latest = -1;
    for row in store_graph::rows(
        g,
        "MATCH (s:Symbol {repository_id:$r})-[:HAS_VERSION]->(v:SymbolVersion) \
         WHERE v.node_id = $n \
         RETURN v.data AS data, v.valid_from_episode AS valid_from",
        gparams([("r", gs(repository)), ("n", gs(&old_other))]),
    )? {
        let sequence = sequences.get(s(&row, "valid_from")).copied().unwrap_or(-1);
        if sequence >= latest {
            latest = sequence;
            old_stable = serde_json::from_str::<Value>(s(&row, "data"))
                .ok()
                .and_then(|data| data["stable_id"].as_str().map(str::to_string));
        }
    }
    if let (Some(new_stable), Some(old_stable)) = (&new_stable, &old_stable)
        && new_stable == old_stable
    {
        return Ok(());
    }
    // MERGE: a retried record (the previous attempt crashed after this
    // transaction but before the episode row) must not duplicate the marker.
    g.query_with_params(
        "MERGE (c:Contradiction {repository_id:$r, episode:$e, relation:$relation, \
         subject:$subject, old_target:$old_target, new_target:$new_target}) \
         SET c.old_edge = $old_edge, c.new_edge = $new_edge, c.recorded_at = $stamp",
        gparams([
            ("r", gs(repository)),
            ("e", gs(episode)),
            ("relation", gs(&closed.relation)),
            ("subject", gs(&subject)),
            ("old_target", gs(&old_other)),
            ("new_target", gs(new_other)),
            (
                "old_edge",
                gs(&edge_ref(
                    &closed.source,
                    &closed.target,
                    &closed.relation,
                    &closed.key,
                )),
            ),
            ("new_edge", gs(new_ref)),
            ("stamp", gs(stamp)),
        ]),
    )?;
    Ok(())
}

/// Contradiction markers recorded for `episode` (or every episode when `None`).
pub fn contradictions(
    store: &GraphStore,
    repository: &str,
    episode: Option<&str>,
) -> Result<Vec<Value>> {
    store.read(|g| {
        let mut params = gparams([("r", gs(repository))]);
        let mut filter = String::new();
        if let Some(episode) = episode {
            filter = " WHERE c.episode = $e".to_string();
            params.insert("e".into(), gs(episode));
        }
        store_graph::rows(
            g,
            &format!(
                "MATCH (c:Contradiction {{repository_id:$r}}){filter} \
                 RETURN c.episode AS episode, c.relation AS relation, c.subject AS subject, \
                 c.old_target AS old_target, c.new_target AS new_target, \
                 c.old_edge AS old_edge, c.new_edge AS new_edge, c.recorded_at AS recorded_at \
                 ORDER BY c.recorded_at, c.subject, c.old_target"
            ),
            params,
        )
    })
}

/// Reconstruct the graph exactly as it was at `episode`.
pub fn load(store: &GraphStore, repository: &str, episode: &str) -> Result<Graph> {
    store.read(|g| load_g(g, repository, episode))
}

/// [`load`] inside an existing graph connection.
fn load_g(g: &mut GraphConnection, repository: &str, episode: &str) -> Result<Graph> {
    let row = episode_row_g(g, repository, episode)?
        .ok_or_else(|| Error::bad("Unknown graph episode for this repository"))?;
    let sequence = row["sequence"].as_i64().unwrap_or(0);
    let multigraph = row["multigraph"] != 0;
    let mut attributes: Value = serde_json::from_str(s(&row, "attributes"))?;
    if let Value::Object(fields) = &mut attributes {
        fields.remove(EXTRACTION_RUN);
        fields.remove(SOURCE_HEADS);
        if fields.remove("unresolved_calls_count").is_some() {
            fields.insert(
                UNRESOLVED_CALLS.into(),
                Value::Array(unresolved_calls_g(g, repository, episode)?),
            );
        }
    }
    // As-of filter resolved in memory: the episode set is small, and joining
    // every version and relationship against it costs more than the lookup.
    let mut sequences: HashMap<String, i64> = HashMap::new();
    for row in store_graph::rows(
        g,
        "MATCH (e:Episode {repository_id:$r}) RETURN e.episode AS episode, e.sequence AS sequence",
        gparams([("r", gs(repository))]),
    )? {
        sequences.insert(
            s(&row, "episode").to_string(),
            row["sequence"].as_i64().unwrap_or(0),
        );
    }
    let as_of = |from: &str, to: Option<&str>| -> bool {
        sequences.get(from).is_some_and(|&f| f <= sequence)
            && to.is_none_or(|to| sequences.get(to).is_some_and(|&t| t > sequence))
    };
    // Rows stream straight into the graph: no node-link document in between.
    let mut graph = Graph::with_capacity(
        row["directed"] != 0,
        multigraph,
        row["node_count"].as_u64().unwrap_or(0) as usize,
        row["edge_count"].as_u64().unwrap_or(0) as usize,
    );
    *graph.attributes_mut() = if attributes.is_object() {
        attributes
    } else {
        json!({})
    };
    for row in store_graph::rows(
        g,
        "MATCH (s:Symbol {repository_id:$r})-[:HAS_VERSION]->(v:SymbolVersion) \
         RETURN v.node_id AS node_id, v.data AS data, \
         v.valid_from_episode AS valid_from, v.valid_to_episode AS valid_to \
         ORDER BY v.ord",
        gparams([("r", gs(repository))]),
    )? {
        if !as_of(s(&row, "valid_from"), row["valid_to"].as_str()) {
            continue;
        }
        let mut fields: Map<String, Value> = serde_json::from_str(s(&row, "data"))?;
        fields.shift_remove("id");
        graph.add_node(json!(s(&row, "node_id")), fields)?;
    }
    // Projection relationships carry `valid_from_episode`; both the live
    // types and their `*_HIST` history are candidates, the as-of filter
    // decides. (HAS_VERSION and cross-repository relationships don't carry
    // the property and never match.)
    for row in store_graph::rows(
        g,
        "MATCH (a:Symbol {repository_id:$r})-[r]->(b:Symbol) \
         WHERE r.valid_from_episode IS NOT NULL \
         RETURN r.source_node_id AS source, r.target_node_id AS target, r.key AS key, r.data AS data, \
         r.valid_from_episode AS valid_from, r.valid_to_episode AS valid_to \
         ORDER BY r.ord",
        gparams([("r", gs(repository))]),
    )? {
        if !as_of(s(&row, "valid_from"), row["valid_to"].as_str()) {
            continue;
        }
        let mut fields: Map<String, Value> = serde_json::from_str(s(&row, "data"))?;
        fields.shift_remove("source");
        fields.shift_remove("target");
        let key = if multigraph {
            fields.shift_remove("key");
            Some(edge_key_value(s(&row, "key")))
        } else {
            None
        };
        graph.add_edge(
            json!(s(&row, "source")),
            json!(s(&row, "target")),
            key,
            fields,
        )?;
    }
    Ok(graph)
}

/// The resident handle for a recorded episode, reopened from the store when
/// the registry evicted it.
pub fn resident(store: &GraphStore, repository: &str, episode: &str) -> Result<GraphHandle> {
    let reference = graph_ref(repository, episode);
    if let Ok(handle) = registry().get(&reference) {
        return Ok(handle);
    }
    let handle = GraphHandle::new(load(store, repository, episode)?);
    registry().open(handle.clone(), Some(&reference));
    Ok(handle)
}

pub fn episodes(store: &GraphStore, repository: &str) -> Result<Vec<Value>> {
    let mut episodes = store.read(|g| {
        store_graph::rows(
            g,
            &format!(
                "MATCH (e:Episode {{repository_id:$r}}) RETURN {EPISODE_COLUMNS} ORDER BY e.sequence"
            ),
            gparams([("r", gs(repository))]),
        )
    })?;
    for episode in &mut episodes {
        let attributes: Value =
            serde_json::from_str(s(episode, "attributes")).unwrap_or(Value::Null);
        let extraction = attributes
            .get(EXTRACTION_RUN)
            .cloned()
            .unwrap_or(Value::Null);
        if let Value::Object(fields) = episode {
            fields.remove("attributes");
            fields.insert("extraction".into(), extraction);
        }
    }
    Ok(episodes)
}

/// Graph attribute holding the receiver-resolution leftovers; stored in
/// `gf_unresolved_calls` (one row per call, closed when the next episode is
/// recorded) instead of the episode's attribute blob so retention can drop it.
pub const UNRESOLVED_CALLS: &str = "unresolved_calls";

fn episode_attributes(graph: &Graph, extraction: Option<&Value>) -> Value {
    let mut attributes = graph.attributes().clone();
    if let Value::Object(fields) = &mut attributes {
        if let Some(calls) = fields.remove(UNRESOLVED_CALLS) {
            let count = calls.as_array().map_or(0, Vec::len);
            fields.insert("unresolved_calls_count".into(), json!(count));
        }
        if let Some(extraction) = extraction {
            fields.insert(EXTRACTION_RUN.into(), extraction.clone());
        }
    }
    attributes
}

/// Write the episode's `unresolved_calls` nodes and close the previous
/// episode's (`valid_to_episode`), so only current + previous stay open for
/// the report's receiver check and retention. Runs inside the episode's
/// graph transaction.
fn persist_unresolved_calls(
    g: &mut GraphConnection,
    repository: &str,
    episode: &str,
    graph: &Graph,
    stamp: &str,
) -> Result<()> {
    g.query_with_params(
        "MATCH (u:UnresolvedCall {repository_id:$r}) \
         WHERE u.valid_to_episode IS NULL AND u.episode <> $e \
         SET u.valid_to_episode = $e",
        gparams([("r", gs(repository)), ("e", gs(episode))]),
    )?;
    let Some(calls) = graph.attributes()[UNRESOLVED_CALLS].as_array() else {
        return Ok(());
    };
    for (batch, chunk) in calls.chunks(BATCH).enumerate() {
        let rows: Vec<Value> = chunk
            .iter()
            .enumerate()
            .map(|(index, call)| {
                json!({
                    "caller": call["caller"].as_str().unwrap_or(""),
                    "callee": call["callee"].as_str().unwrap_or(""),
                    "call": graphicle_memory::text::canonical(call),
                    "position": batch * BATCH + index,
                })
            })
            .collect();
        // MERGE per position: a retried record (after a crash between the
        // chunked commits and the episode row) must not duplicate the call.
        g.query_with_params(
            "UNWIND $rows AS row \
             MERGE (u:UnresolvedCall {repository_id:$r, episode:$e, position: row.position}) \
             SET u.caller = row.caller, u.callee = row.callee, u.call = row.call, \
                 u.created_at = $stamp",
            gparams([
                ("r", gs(repository)),
                ("e", gs(episode)),
                ("rows", gj(&json!(rows))),
                ("stamp", gs(stamp)),
            ]),
        )?;
    }
    Ok(())
}

/// The episode's `unresolved_calls` as stored (empty once retention has
/// dropped them).
pub fn unresolved_calls(store: &GraphStore, repository: &str, episode: &str) -> Result<Vec<Value>> {
    store.read(|g| unresolved_calls_g(g, repository, episode))
}

/// [`unresolved_calls`] inside an existing graph connection.
fn unresolved_calls_g(
    g: &mut GraphConnection,
    repository: &str,
    episode: &str,
) -> Result<Vec<Value>> {
    store_graph::rows(
        g,
        "MATCH (u:UnresolvedCall {repository_id:$r, episode:$e}) \
         RETURN u.call AS call ORDER BY u.position",
        gparams([("r", gs(repository)), ("e", gs(episode))]),
    )?
    .iter()
    .map(|row| Ok(serde_json::from_str(s(row, "call"))?))
    .collect()
}

/// Attach `label` (a commit sha, "main", "base", ...) to a recorded episode.
/// Re-labelling moves the label; labelling the same episode again is a no-op.
pub fn label_episode(
    db: &Connection,
    store: &GraphStore,
    workspace: &str,
    repository: &str,
    label: &str,
    episode: &str,
) -> Result<()> {
    if label.is_empty() || label.len() > 128 || label == "latest" || label == "working" {
        return Err(Error::bad("Invalid episode label"));
    }
    if episode_row(store, repository, episode)?.is_none() {
        return Err(Error::bad("Unknown graph episode for this repository"));
    }
    // gf_commits belongs to the timeline domain, still in SQLite.
    if let Some(verified) = one(
        db,
        "SELECT episode FROM gf_commits WHERE repository_id=? AND oid=?",
        params![repository, label],
    )? && s(&verified, "episode") != episode
    {
        return Err(Error::conflict(
            "A verified Git commit cannot be relabelled to a different episode",
        ));
    }
    // Re-labelling moves the label; labelling the same episode again is a
    // no-op.
    let mut g = store.connection()?;
    graph_commit(&mut g, |g| {
        g.query_with_params(
            "MERGE (l:EpisodeLabel {workspace:$w, repository_id:$r, label:$label}) \
             SET l.episode = $e, l.created_at = $stamp",
            gparams([
                ("w", gs(workspace)),
                ("r", gs(repository)),
                ("label", gs(label)),
                ("e", gs(episode)),
                ("stamp", gs(&now())),
            ]),
        )?;
        Ok(())
    })
}

pub fn labels(store: &GraphStore, workspace: &str, repository: &str) -> Result<Vec<Value>> {
    store.read(|g| {
        store_graph::rows(
            g,
            "MATCH (l:EpisodeLabel {workspace:$w, repository_id:$r}) \
             RETURN l.label AS label, l.episode AS episode_id, l.created_at AS created_at \
             ORDER BY l.label",
            gparams([("w", gs(workspace)), ("r", gs(repository))]),
        )
    })
}

/// Resolve an episode selector: a recorded episode id, a label, or
/// `latest` (most recently recorded) / `working` (the current head set,
/// which must already be recorded).
pub fn resolve_episode(
    db: &Connection,
    store: &GraphStore,
    workspace: &str,
    repository: &str,
    selector: &str,
) -> Result<String> {
    if selector.is_empty() || selector.len() > 128 {
        return Err(Error::bad("Invalid episode selector"));
    }
    match selector {
        "working" => {
            let current = current_episode(db, repository)?;
            if episode_row(store, repository, &current)?.is_none() {
                return Err(Error::bad(
                    "The working head set has not been projected yet",
                ));
            }
            return Ok(current);
        }
        "latest" => return latest_episode(store, repository),
        _ => {}
    }
    if episode_row(store, repository, selector)?.is_some() {
        return Ok(selector.to_string());
    }
    store
        .read(|g| {
            store_graph::one(
                g,
                "MATCH (l:EpisodeLabel {workspace:$w, repository_id:$r, label:$label}) \
             RETURN l.episode AS episode_id",
                gparams([
                    ("w", gs(workspace)),
                    ("r", gs(repository)),
                    ("label", gs(selector)),
                ]),
            )
        })?
        .map(|row| s(&row, "episode_id").to_string())
        .ok_or_else(|| Error::bad("Unknown episode or label for this repository"))
}

/// The most recently recorded episode of the repository.
fn latest_episode(store: &GraphStore, repository: &str) -> Result<String> {
    store
        .read(|g| {
            store_graph::one(
                g,
                "MATCH (e:Episode {repository_id:$r}) \
                 RETURN e.episode AS episode ORDER BY e.sequence DESC LIMIT 1",
                gparams([("r", gs(repository))]),
            )
        })?
        .map(|row| s(&row, "episode").to_string())
        .ok_or_else(|| Error::bad("No graph episode recorded for this repository"))
}

fn info(
    store: &GraphStore,
    repository: &str,
    episode: &str,
    replayed: bool,
) -> Result<ProjectionInfo> {
    let row = episode_row(store, repository, episode)?
        .ok_or_else(|| Error::bad("Unknown graph episode for this repository"))?;
    Ok(ProjectionInfo {
        episode: episode.to_string(),
        nodes: row["node_count"].as_u64().unwrap_or(0) as usize,
        edges: row["edge_count"].as_u64().unwrap_or(0) as usize,
        graph_ref: graph_ref(repository, episode),
        replayed,
    })
}

/// Whether the current head set's episode is recorded; if so, its graph is a
/// projection of exactly the files in the store, so no graph node is stale:
/// clear the repository's `graph_node` markers. A sync leaves markers that no
/// new projection would clear when its head set was already projected (a
/// file edited and then restored) or when a projection job replays a
/// recorded episode. Returns how many markers were cleared.
pub fn clear_stale_when_current(
    db: &Connection,
    store: &GraphStore,
    repository: &str,
) -> Result<usize> {
    let episode = current_episode(db, repository)?;
    // Gate on the SQLite episode mirror: every sync passes here, and a
    // store that never projected must not open (create) the graph store —
    // the writer lease belongs to the process that builds the graph.
    let recorded = one(
        db,
        "SELECT 1 AS recorded FROM gf_episodes WHERE repository_id=? AND episode=?",
        params![repository, episode],
    )?;
    if recorded.is_none() || episode_row(store, repository, &episode)?.is_none() {
        return Ok(0);
    }
    let workspace = crate::invalidation::workspace_of(db, repository)?;
    crate::invalidation::clear_stale_prefix(
        store,
        &workspace,
        crate::invalidation::GRAPH_NODE,
        &format!("{repository}:"),
    )
}

/// Build, persist and open the current head set inside one authorized transaction.
pub fn project(
    db: &Connection,
    store: &GraphStore,
    repository: &str,
    _principal: &Principal,
) -> Result<ProjectionInfo> {
    let episode = current_episode(db, repository)?;
    if episode_row(store, repository, &episode)?.is_some() {
        resident(store, repository, &episode)?;
        // A replayed head set's symbols become the live ones.
        crate::symbols::ensure(db, store, repository, &episode)?;
        clear_stale_when_current(db, store, repository)?;
        return info(store, repository, &episode, true);
    }
    let (episode, graph, extraction) =
        crate::phase::phase("graph_build", || build_graph_with_report(db, repository))?;
    crate::phase::phase("graph_persist", || {
        persist_with_report(db, store, repository, &episode, &graph, Some(&extraction))
    })?;
    registry().open(
        GraphHandle::new(graph),
        Some(&graph_ref(repository, &episode)),
    );
    info(store, repository, &episode, false)
}

/// A graph built outside any write transaction: `(episode, graph, report)`.
pub type Built = (String, Graph, Value);

/// Phase one of a projection, for a read-only snapshot: `None` when the
/// current head set is already recorded (and now resident), else the built
/// graph. Extraction reads blobs and a file cache only, never SQLite writes,
/// so it must not run while holding the write lock: a large repository takes
/// longer than the store's busy timeout.
pub fn prebuild(db: &Connection, store: &GraphStore, repository: &str) -> Result<Option<Built>> {
    let episode = current_episode(db, repository)?;
    if episode_row(store, repository, &episode)?.is_some() {
        resident(store, repository, &episode)?;
        return Ok(None);
    }
    Ok(Some(crate::phase::phase("graph_build", || {
        build_graph_with_report(db, repository)
    })?))
}

/// Phase two, inside the write transaction: record a graph from `prebuild`
/// when it still matches the current head set and nobody recorded it first.
/// If the head set moved in between, build here rather than record a stale
/// graph (the rare race keeps the old single-phase behaviour).
pub fn project_prebuilt(
    db: &Connection,
    store: &GraphStore,
    repository: &str,
    principal: &Principal,
    built: Option<Built>,
) -> Result<ProjectionInfo> {
    let episode = current_episode(db, repository)?;
    if episode_row(store, repository, &episode)?.is_some() {
        resident(store, repository, &episode)?;
        crate::symbols::ensure(db, store, repository, &episode)?;
        clear_stale_when_current(db, store, repository)?;
        return info(store, repository, &episode, true);
    }
    match built {
        Some((built_episode, graph, extraction)) if built_episode == episode => {
            crate::phase::phase("graph_persist", || {
                persist_with_report(db, store, repository, &episode, &graph, Some(&extraction))
            })?;
            registry().open(
                GraphHandle::new(graph),
                Some(&graph_ref(repository, &episode)),
            );
            info(store, repository, &episode, false)
        }
        _ => project(db, store, repository, principal),
    }
}

/// Read the head set under the caller's authority, build outside the stores,
/// then persist if the head set is still the same and nobody recorded it
/// first.
pub fn project_store(
    store: &Store,
    workspace: &str,
    token: &str,
    repository: &str,
) -> Result<ProjectionInfo> {
    let gstore = store.graph();
    let (identifier, built) = store.authorized_read(workspace, token, "read", |db, p| {
        let repository = repo(db, p, repository)?;
        let identifier = s(&repository, "id").to_string();
        let episode = current_episode(db, &identifier)?;
        if episode_row(&gstore, &identifier, &episode)?.is_some() {
            resident(&gstore, &identifier, &episode)?;
            return Ok((identifier, None));
        }
        let built =
            crate::phase::phase("graph_build", || build_graph_with_report(db, &identifier))?;
        Ok((identifier, Some(built)))
    })?;
    store.authorized(workspace, token, "read", |db, p| {
        let repository = repo(db, p, &identifier)?;
        let identifier = s(&repository, "id");
        let current = current_episode(db, identifier)?;
        match built {
            Some((episode, graph, extraction)) if episode == current => {
                let recorded = crate::phase::phase("graph_persist", || {
                    persist_with_report(
                        db,
                        &gstore,
                        identifier,
                        &episode,
                        &graph,
                        Some(&extraction),
                    )
                })?;
                if recorded {
                    registry().open(
                        GraphHandle::new(graph),
                        Some(&graph_ref(identifier, &episode)),
                    );
                } else {
                    resident(&gstore, identifier, &episode)?;
                    clear_stale_when_current(db, &gstore, identifier)?;
                }
                info(&gstore, identifier, &episode, !recorded)
            }
            Some(_) => Err(Error::conflict("Repository changed while projecting")),
            None => {
                crate::symbols::ensure(db, &gstore, identifier, &current)?;
                clear_stale_when_current(db, &gstore, identifier)?;
                info(&gstore, identifier, &current, true)
            }
        }
    })
}

impl Store {
    /// Episodes and labels of a repository, for the `graph_episodes` MCP tool.
    pub fn episode_listing(&self, workspace: &str, token: &str, repository: &str) -> Result<Value> {
        let mut listing = self.graph_episodes(workspace, token, repository)?;
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, repository)?;
            listing["repo"] = json!(s(&repository, "name"));
            listing["count"] = json!(episodes(&self.graph(), s(&repository, "id"))?.len());
            Ok(())
        })?;
        Ok(listing)
    }

    pub fn graph_project(&self, workspace: &str, token: &str, repository: &str) -> Result<Value> {
        Ok(json!(project_store(self, workspace, token, repository)?))
    }
    pub fn graph_episodes(&self, workspace: &str, token: &str, repository: &str) -> Result<Value> {
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, repository)?;
            let identifier = s(&repository, "id");
            let gstore = self.graph();
            let mut listed = episodes(&gstore, identifier)?;
            let labels = labels(&gstore, workspace, identifier)?;
            for episode in &mut listed {
                let mine: Vec<Value> = labels
                    .iter()
                    .filter(|l| l["episode_id"] == episode["episode"])
                    .map(|l| l["label"].clone())
                    .collect();
                episode["labels"] = json!(mine);
            }
            Ok(json!({
                "current_episode": current_episode(db, identifier)?,
                "episodes": listed,
                "labels": labels,
            }))
        })
    }
    /// Label a recorded episode (`episode` may itself be a selector).
    pub fn graph_label(
        &self,
        workspace: &str,
        token: &str,
        repository: &str,
        label: &str,
        episode: &str,
    ) -> Result<Value> {
        self.authorized(workspace, token, "collect", |db, p| {
            let repository = repo(db, p, repository)?;
            let identifier = s(&repository, "id");
            let resolved = resolve_episode(db, &self.graph(), workspace, identifier, episode)?;
            label_episode(db, &self.graph(), workspace, identifier, label, &resolved)?;
            Ok(json!({"label": label, "episode": resolved}))
        })
    }
    /// The typed graph as it stood at an episode selector (`as_of`: episode
    /// id, commit-sha label, `latest`, `working`), reconstructed from the
    /// bi-temporal tables without re-extraction. Optional `node` (exact id),
    /// `query` (substring of node id) and `relations` narrow the view;
    /// `limit` bounds nodes. Edges are those between returned nodes.
    pub fn graph_as_of(
        &self,
        workspace: &str,
        token: &str,
        repository: &str,
        args: &Value,
    ) -> Result<Value> {
        if !args.is_object() {
            return Err(Error::bad("as_of arguments must be an object"));
        }
        let selector = match args.get("as_of") {
            None | Some(Value::Null) => "latest".to_string(),
            Some(_) => string(args, "as_of", 128)?.to_string(),
        };
        let limit = crate::integer(args, "limit", 200, 2000)?.max(1);
        let edge_limit = crate::integer(args, "edge_limit", 10000, 100000)?.max(1);
        let query = args["query"].as_str().unwrap_or("").to_lowercase();
        let node = args["node"].as_str().filter(|n| !n.is_empty());
        let relations: Option<Vec<String>> = match args.get("relations").filter(|v| !v.is_null()) {
            None => None,
            Some(list) => Some(
                list.as_array()
                    .filter(|l| l.len() <= 64)
                    .ok_or_else(|| Error::bad("Expected relations array"))?
                    .iter()
                    .map(|v| v.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| Error::bad("Expected relation names"))?,
            ),
        };
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, repository)?;
            let identifier = s(&repository, "id");
            // The timeline, aliases and hyperedges are graph-side
            // (temporal_graph migrated), on one connection with the
            // projection reads.
            let gstore = self.graph();
            let mut g = gstore.connection()?;
            let view = crate::temporal_graph::select(&mut g, workspace, identifier, args)?;
            let episode = view.episode;
            let row = episode_row_g(&mut g, identifier, &episode)?
                .ok_or_else(|| Error::bad("Unknown graph episode for this repository"))?;
            let mut graph = resident(&gstore, identifier, &episode)?;
            if args["canonical"] == true {
                let aliases = crate::temporal_graph::aliases_at(&mut g, identifier, view.valid, view.known)?;
                let mut owned = graph.into_graph();
                graphicle_memory::graph::canonical::apply(&mut owned, &aliases)?;
                graph = GraphHandle::new(owned);
            }
            let nodes = graph.nodes();
            let text_of = |value: &Value| text(value);
            let keep_edge = |relation: &str| {
                relations
                    .as_ref()
                    .is_none_or(|list| list.iter().any(|r| r == relation))
            };
            let mut selected: Vec<usize> = Vec::new();
            if let Some(node) = node {
                let index = graph
                    .node_index(&json!(node))
                    .ok_or_else(|| Error::bad("Node unavailable at this episode"))?;
                selected.push(index);
                for (source, target, edge) in graph.iter_edges() {
                    let relation = edge.attributes.get("relation").map(text).unwrap_or_default();
                    if (source == index || target == index) && keep_edge(&relation) {
                        selected.push(if source == index { target } else { source });
                    }
                }
                selected.sort_unstable();
                selected.dedup();
                // Keep the requested anchor even when its neighbor list is truncated.
                selected.retain(|candidate| *candidate != index);
                selected.insert(0, index);
            } else {
                selected = (0..nodes.len())
                    .filter(|&i| query.is_empty() || text_of(&nodes[i].id).to_lowercase().contains(&query))
                    .collect();
            }
            let truncated = selected.len() > limit;
            selected.truncate(limit);
            let chosen: std::collections::HashSet<usize> = selected.iter().copied().collect();
            let listed: Vec<Value> = selected
                .iter()
                .map(|&i| {
                    let mut fields = nodes[i].attributes.clone();
                    fields.insert("id".into(), nodes[i].id.clone());
                    Value::Object(fields.into())
                })
                .collect();
            let mut edges = Vec::new();
            let mut edges_truncated = false;
            for (source, target, edge) in graph.iter_edges() {
                if !chosen.contains(&source) || !chosen.contains(&target) {
                    continue;
                }
                let relation = edge.attributes.get("relation").map(text).unwrap_or_default();
                if !keep_edge(&relation) {
                    continue;
                }
                if edges.len() >= edge_limit {
                    edges_truncated = true;
                    continue;
                }
                let mut fields = edge.attributes.clone();
                fields.insert("source".into(), nodes[source].id.clone());
                fields.insert("target".into(), nodes[target].id.clone());
                fields.insert("key".into(), edge.key.clone());
                edges.push(Value::Object(fields.into()));
            }
            let labels: Vec<Value> = labels(&gstore, workspace, identifier)?
                .into_iter()
                .filter(|l| l["episode_id"] == json!(episode))
                .map(|l| l["label"].clone())
                .collect();
            let (hyperedges,hyperedges_truncated)=crate::temporal_graph::hyperedges_at(&mut g, identifier, view.valid, view.known, &graph)?;
            Ok(json!({
                "as_of": if view.metadata["selection"]=="episode" {json!(selector)} else {Value::Null},
                "temporal": view.metadata,
                "episode": episode,
                "sequence": row["sequence"],
                "parent_episode": row["parent_episode"],
                "created_at": row["created_at"],
                "labels": labels,
                "node_count": graph.node_count(),
                "edge_count": graph.edge_count(),
                "canonical": args["canonical"] == true,
                "multigraph": row["multigraph"] != 0,
                "nodes": listed,
                "edges": edges,
                "edges_truncated": edges_truncated,
                "truncated": truncated,
                "contradictions": contradictions(&gstore, identifier, Some(&episode))?,
                "hyperedges": hyperedges,"hyperedges_truncated":hyperedges_truncated,
                "freshness": crate::invalidation::repository_freshness(&gstore, &p.workspace, identifier)?,
            }))
        })
    }
}

// ---------------------------------------------------------------------------
// Cross-repository import linkage. Every repository of the
// workspace is indexed on its own; `link_workspace` resolves imports between
// the resident graphs by package identity and persists the result with both
// episodes in `gf_cross_edges` / `gf_cross_unresolved`.
// ---------------------------------------------------------------------------

use graphicle_memory::graph::cross_repo_imports::{self as xrepo, ExportIndex, PackageManifest};

/// `via` marker on cross-repository report items.
pub const VIA_CROSS_REPO_IMPORT: &str = "cross_repo_import";
const MANIFEST_FILES: &[&str] = &["pyproject.toml", "setup.cfg", "package.json", "go.mod"];
const MANIFEST_SKIP_DIRS: &[&str] = &["node_modules/", "vendor/", ".venv/", "dist/", "build/"];

/// Repository-level read access. Workspace membership grants access to every
/// repository unless an explicit `revoked` row in `gf_repo_access` names the
/// principal's credential; the workspace owner always reads everything. A
/// PR scratch project linked to a folder project is readable only while the
/// folder project is.
pub fn repo_readable(db: &Connection, p: &Principal, repository: &str) -> Result<bool> {
    if p.role == "owner" {
        return Ok(true);
    }
    // A PR scratch project holds copies of its folder project's code:
    // it is readable only while that project is.
    let revoked = one(
        db,
        "SELECT 1 AS revoked FROM gf_repo_access WHERE credential_id=?1 AND state='revoked' \
         AND (repository_id=?2 OR repository_id IN (SELECT project_id FROM repo_pr_scratch WHERE repository_id=?2))",
        params![p.credential, repository],
    )?;
    Ok(revoked.is_none())
}

/// Record `granted` / `revoked` repository access for a credential.
pub fn set_repo_access(
    db: &Connection,
    workspace: &str,
    repository: &str,
    credential: &str,
    state: &str,
) -> Result<()> {
    if !["granted", "revoked"].contains(&state) {
        return Err(Error::bad("Expected state granted or revoked"));
    }
    db.execute(
        "INSERT INTO gf_repo_access(workspace_id,repository_id,credential_id,state,created_at) \
         VALUES(?,?,?,?,?) ON CONFLICT(repository_id,credential_id) DO UPDATE SET state=excluded.state,created_at=excluded.created_at",
        params![workspace, repository, credential, state, now()],
    )?;
    Ok(())
}

fn blob(db: &Connection, repository: &str, sha: &str) -> Result<Option<String>> {
    let graph = crate::store::graph_for(db)?;
    crate::store_content::object_content(&graph, repository, sha)
}

fn is_manifest(relpath: &str) -> bool {
    let name = relpath.rsplit('/').next().unwrap_or(relpath);
    MANIFEST_FILES.contains(&name)
        && !MANIFEST_SKIP_DIRS
            .iter()
            .any(|dir| relpath.starts_with(dir) || relpath.contains(&format!("/{dir}")))
}

/// Package manifests of the repository's current head set.
pub fn manifests(
    db: &Connection,
    repository: &str,
    heads: &BTreeMap<String, String>,
) -> Result<Vec<PackageManifest>> {
    let mut out = Vec::new();
    for (relpath, sha) in heads {
        if !is_manifest(relpath) {
            continue;
        }
        if let Some(text) = blob(db, repository, sha)? {
            out.push(PackageManifest::new(relpath.clone(), text));
        }
    }
    Ok(out)
}

/// The episode a repository takes part in linking with: the current head
/// set when it is recorded, else the latest recorded episode. `None` when
/// the repository has never been projected.
fn linkable_episode(
    db: &Connection,
    store: &GraphStore,
    repository: &str,
) -> Result<Option<(String, bool)>> {
    let current = current_episode(db, repository)?;
    if episode_row(store, repository, &current)?.is_some() {
        return Ok(Some((current, true)));
    }
    Ok(store
        .read(|g| {
            store_graph::one(
                g,
                "MATCH (e:Episode {repository_id:$r}) \
                 RETURN e.episode AS episode ORDER BY e.sequence DESC LIMIT 1",
                gparams([("r", gs(repository))]),
            )
        })?
        .map(|row| (s(&row, "episode").to_string(), false)))
}

/// The columns every cross-edge reader gets (the old `gf_cross_edges` row
/// shape, minus the rowid).
const CROSS_COLUMNS: &str = "r.workspace_id AS workspace_id, \
     r.source_repository_id AS source_repository_id, r.source_episode AS source_episode, \
     r.source_node_id AS source_node_id, r.source_stable_id AS source_stable_id, \
     r.target_repository_id AS target_repository_id, r.target_episode AS target_episode, \
     r.target_node_id AS target_node_id, r.target_stable_id AS target_stable_id, \
     r.relation AS relation, r.confidence AS confidence, r.derivation AS derivation, \
     r.module_path AS module_path, r.symbol AS symbol, r.line AS line, \
     r.version_constraint AS version_constraint, r.exporter_version AS exporter_version, \
     r.version_status AS version_status, r.created_at AS created_at, \
     r.valid_to_episode AS valid_to_episode, r.valid_to_repository_id AS valid_to_repository_id";

/// Live and historical cross-repository relationships (the live ones are
/// `CROSS_CALL`, closed ones `CROSS_CALL_HIST` with the validity properties
/// set — the same type-flip as in-repository edges).
const CROSS_MATCH: &str = "MATCH (a:Symbol)-[r]->(b:Symbol) \
     WHERE (type(r) = 'CROSS_CALL' OR type(r) = 'CROSS_CALL_HIST')";

/// Type-flip the live cross relationships matching `filter` into
/// `CROSS_CALL_HIST` with the validity properties set; how many were closed.
fn close_cross_edges(
    g: &mut GraphConnection,
    filter: &str,
    mut params: Params,
    valid_to_episode: &str,
    valid_to_repository: &str,
) -> Result<usize> {
    params.insert("valid_to_episode".into(), gs(valid_to_episode));
    params.insert("valid_to_repository".into(), gs(valid_to_repository));
    let result = g.query_with_params(
        &format!(
            "MATCH (a:Symbol)-[r:CROSS_CALL]->(b:Symbol) WHERE {filter} \
             WITH a, b, r, properties(r) AS props DELETE r \
             CREATE (a)-[h:CROSS_CALL_HIST]->(b) \
             SET h = props, h.valid_to_episode = $valid_to_episode, \
                 h.valid_to_repository_id = $valid_to_repository \
             RETURN count(h) AS closed"
        ),
        params,
    )?;
    Ok(match result.rows.first().and_then(|row| row.first()) {
        Some(GValue::Int(n)) => *n as usize,
        _ => 0,
    })
}

/// Insert the importer→exporter edges of a pair as live `CROSS_CALL`
/// relationships. Endpoint symbols are MERGE'd: they exist from their own
/// repository's projection, and a stale export index degrades to a stub
/// identity node rather than dropping the evidence.
fn insert_cross_edges(
    g: &mut GraphConnection,
    workspace: &str,
    importer: &Linkable,
    exporter: &Linkable,
    edges: &[&xrepo::CrossEdge],
    stamp: &str,
) -> Result<()> {
    let rows: Vec<Value> = edges
        .iter()
        .map(|edge| {
            let mut props = json!({
                "workspace_id": workspace,
                "source_repository_id": importer.id,
                "source_episode": importer.episode,
                "source_node_id": edge.source.node_id,
                "source_stable_id": edge.source.stable_id,
                "target_repository_id": exporter.id,
                "target_episode": exporter.episode,
                "target_node_id": edge.target.node_id,
                "target_stable_id": edge.target.stable_id,
                "relation": edge.relation,
                "confidence": edge.confidence,
                "derivation": edge.derivation,
                "module_path": edge.module_path,
                "symbol": edge.symbol,
                "line": edge.line,
                "version_constraint": edge.version_constraint,
                "exporter_version": edge.exporter_version,
                "version_status": edge.version_status,
                "created_at": stamp,
            });
            // Null properties cannot be stored; absent reads back as null.
            if let Value::Object(map) = &mut props {
                map.retain(|_, v| !v.is_null());
            }
            json!({
                "sr": importer.id, "sn": edge.source.node_id, "ss": edge.source.stable_id,
                "tr": exporter.id, "tn": edge.target.node_id, "ts": edge.target.stable_id,
                "props": props,
            })
        })
        .collect();
    for chunk in rows.chunks(BATCH) {
        g.query_with_params(
            "UNWIND $rows AS row \
             MERGE (a:Symbol {repository_id: row.sr, stable_id: row.ss}) \
             ON CREATE SET a.node_id = row.sn \
             MERGE (b:Symbol {repository_id: row.tr, stable_id: row.ts}) \
             ON CREATE SET b.node_id = row.tn \
             CREATE (a)-[r:CROSS_CALL]->(b) SET r = row.props",
            gparams([("rows", gj(&json!(chunk)))]),
        )?;
    }
    Ok(())
}

struct Linkable {
    id: String,
    name: String,
    episode: String,
    /// Whether `episode` is the current head set (sources then match the graph).
    current: bool,
    graph: GraphHandle,
    heads: BTreeMap<String, String>,
    manifests: Vec<PackageManifest>,
    index: ExportIndex,
}

fn live_pair_episodes(
    g: &mut GraphConnection,
    source: &str,
    target: &str,
) -> Result<Vec<(String, String)>> {
    Ok(store_graph::rows(
        g,
        "MATCH (a:Symbol {repository_id:$s})-[r:CROSS_CALL]->(b:Symbol {repository_id:$t}) \
         RETURN DISTINCT r.source_episode AS source_episode, r.target_episode AS target_episode",
        gparams([("s", gs(source)), ("t", gs(target))]),
    )?
    .iter()
    .map(|r| {
        (
            s(r, "source_episode").to_string(),
            s(r, "target_episode").to_string(),
        )
    })
    .collect())
}

fn same_cross_edges(
    g: &mut GraphConnection,
    source: &str,
    target: &str,
    edges: &[&xrepo::CrossEdge],
) -> Result<bool> {
    const FIELDS: &[&str] = &[
        "source_node_id",
        "source_stable_id",
        "target_node_id",
        "target_stable_id",
        "relation",
        "confidence",
        "derivation",
        "module_path",
        "symbol",
        "line",
        "version_constraint",
        "exporter_version",
        "version_status",
    ];
    let mut previous: Vec<String> = store_graph::rows(
        g,
        "MATCH (a:Symbol {repository_id:$s})-[r:CROSS_CALL]->(b:Symbol {repository_id:$t}) \
         RETURN r.source_node_id AS source_node_id, r.source_stable_id AS source_stable_id, \
         r.target_node_id AS target_node_id, r.target_stable_id AS target_stable_id, \
         r.relation AS relation, r.confidence AS confidence, r.derivation AS derivation, \
         r.module_path AS module_path, r.symbol AS symbol, r.line AS line, \
         r.version_constraint AS version_constraint, r.exporter_version AS exporter_version, \
         r.version_status AS version_status",
        gparams([("s", gs(source)), ("t", gs(target))]),
    )?
    .iter()
    .map(|row| json!(FIELDS.iter().map(|field| &row[*field]).collect::<Vec<_>>()).to_string())
    .collect();
    let mut next: Vec<String> = edges
        .iter()
        .map(|edge| {
            json!([
                edge.source.node_id,
                edge.source.stable_id,
                edge.target.node_id,
                edge.target.stable_id,
                edge.relation,
                edge.confidence,
                edge.derivation,
                edge.module_path,
                edge.symbol,
                edge.line,
                edge.version_constraint,
                edge.exporter_version,
                edge.version_status,
            ])
            .to_string()
        })
        .collect();
    previous.sort_unstable();
    next.sort_unstable();
    Ok(previous == next)
}

fn record_packages(g: &mut GraphConnection, workspace: &str, repo: &Linkable) -> Result<()> {
    let rows: Vec<Value> = repo
        .index
        .packages
        .iter()
        .map(|package| {
            json!({
                "episode": repo.episode,
                "ecosystem": package.ecosystem.as_str(),
                "name": package.name,
                "version": package.version,
            })
        })
        .collect();
    let stamp = now();
    for chunk in rows.chunks(BATCH) {
        g.query_with_params(
            "UNWIND $rows AS row \
             MERGE (p:Package {workspace_id:$w, repository_id:$r, episode: row.episode, \
                ecosystem: row.ecosystem, name: row.name}) \
             SET p.version = row.version, p.created_at = $stamp",
            gparams([
                ("w", gs(workspace)),
                ("r", gs(&repo.id)),
                ("rows", gj(&json!(chunk))),
                ("stamp", gs(&stamp)),
            ]),
        )?;
    }
    Ok(())
}

/// Export indexes by `(repository, episode)`: an episode's graph never
/// changes, so its index is computed once per process and reused by every
/// later link run (a small bounded map; the newest entries win).
fn export_index(
    graph: &Graph,
    repository: &str,
    episode: &str,
    manifests: &[PackageManifest],
) -> ExportIndex {
    static INDEXES: Mutex<Option<HashMap<(String, String), ExportIndex>>> = Mutex::new(None);
    const CAPACITY: usize = 64;
    let key = (repository.to_string(), episode.to_string());
    if let Some(index) = INDEXES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|cache| cache.get(&key))
    {
        return index.clone();
    }
    let index = xrepo::exports(graph, repository, Some(episode), manifests);
    let mut guard = INDEXES.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = guard.get_or_insert_with(HashMap::new);
    if cache.len() >= CAPACITY {
        cache.clear();
    }
    cache.insert(key, index.clone());
    index
}

/// Link the readable, projected repositories of the principal's workspace
/// against each other and persist the result. A `pr_scratch` project (a PR
/// copy of a checkout, [`PR_SCRATCH`]) takes part only while no other
/// linkable project, and no newer `pr_scratch` project, exports one of its
/// import roots; when one does, the copy is skipped, its live cross rows are
/// closed, and the summary lists it under `pr_scratch_skipped`. A project
/// synced from a folder inside another project's folder owns the packages
/// under it. Idempotent for unchanged episode pairs; when either
/// side moved to a new episode the stale rows are closed with
/// `valid_to_episode` (the new episode of the side that changed) and the pair
/// is re-linked.
pub fn link_workspace(db: &Connection, store: &GraphStore, p: &Principal) -> Result<Value> {
    let workspace = p.workspace.as_str();
    let mut g = store.connection()?;
    let mut hidden = 0usize;
    let mut unprojected = Vec::new();
    let mut candidates: Vec<(String, String, String, bool)> = Vec::new();
    for row in rows(
        db,
        "SELECT id,name FROM repo_projects WHERE workspace_id=? ORDER BY created_at,id",
        [workspace],
    )? {
        let id = s(&row, "id").to_string();
        if !repo_readable(db, p, &id)? {
            hidden += 1;
            continue;
        }
        let Some((episode, current)) = linkable_episode(db, store, &id)? else {
            unprojected.push(id);
            continue;
        };
        candidates.push((id, s(&row, "name").to_string(), episode, current));
    }
    // Cross-repository linkage needs two sides: with fewer than two linkable
    // repositories there is no pair to link and nothing is recorded.
    if candidates.len() < 2 {
        return Ok(json!({
            "repositories": candidates.iter().map(|(id, name, episode, current)| json!({
                "id": id, "name": name, "episode": episode, "current": current,
                "packages": [], "exported_modules": 0,
            })).collect::<Vec<_>>(),
            "pairs": [],
            "edges": 0,
            "unresolved": 0,
            "hidden_repositories": hidden,
            "persisted": hidden == 0,
            "unprojected_repositories": unprojected,
            "skipped": "fewer_than_two_repositories",
        }));
    }
    let mut repos: Vec<Linkable> = Vec::new();
    for (id, name, episode, current) in candidates {
        let graph = resident(store, &id, &episode)?;
        let heads = episode_heads(db, store, &id, &episode)?.unwrap_or_default();
        let manifests = manifests(db, &id, &heads)?;
        let index = export_index(&graph, &id, &episode, &manifests);
        repos.push(Linkable {
            id,
            name,
            episode,
            current,
            graph,
            heads,
            manifests,
            index,
        });
    }
    // Nested projects: a project synced from a folder inside
    // another linkable project's folder owns the packages and modules under
    // it, so the outer project stops exporting them and the two never export
    // the same import root (which made every import of it ambiguous).
    let nested = nested_prefixes(db, &repos)?;
    for repo in &mut repos {
        if let Some(prefixes) = nested.get(&repo.id) {
            let mut index = repo.index.clone();
            index
                .packages
                .retain(|package| !prefixes.iter().any(|prefix| below(&package.root, prefix)));
            index
                .modules
                .retain(|_, module| !prefixes.iter().any(|prefix| below(&module.relpath, prefix)));
            repo.index = index;
        }
    }
    // PR scratch copies: a `pr_scratch` project takes part only
    // while no other linkable project exports one of its import roots. A
    // second exporter of the same package would make every import of it from
    // a third project unresolved as `ambiguous_exporters`. A skipped scratch
    // project's live cross rows are closed at its episode (never deleted).
    let scratch = pr_scratch_projects(db, workspace)?;
    let roots = |repo: &Linkable| -> Vec<(xrepo::Ecosystem, String)> {
        repo.index
            .packages
            .iter()
            .flat_map(|package| {
                package
                    .import_roots
                    .iter()
                    .map(|root| (package.ecosystem, root.clone()))
            })
            .collect()
    };
    let mut claimed: BTreeSet<(xrepo::Ecosystem, String)> = repos
        .iter()
        .filter(|repo| !scratch.contains(&repo.id))
        .flat_map(roots)
        .collect();
    // Among PR scratch copies of one package, the newest one exports it
    //: `graphicle pr` keeps a scratch project per folder
    // project now, beside any `git:` copy an earlier run registered.
    // `repos` is in creation order. As with the PR scratch rule, the skip is per
    // project: an older copy that shares any import root with a newer one
    // is skipped whole, its other roots included.
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    for repo in repos.iter().rev().filter(|repo| scratch.contains(&repo.id)) {
        let own = roots(repo);
        if own.iter().any(|root| claimed.contains(root)) {
            skipped.insert(repo.id.clone());
        } else {
            claimed.extend(own);
        }
    }
    let (shadowed, repos): (Vec<Linkable>, Vec<Linkable>) = repos
        .into_iter()
        .partition(|repo| skipped.contains(&repo.id));
    if hidden == 0 {
        for copy in &shadowed {
            graph_commit(&mut g, |g| {
                close_cross_edges(
                    g,
                    "r.source_repository_id = $id OR r.target_repository_id = $id",
                    gparams([("id", gs(&copy.id))]),
                    &copy.episode,
                    &copy.id,
                )?;
                g.query_with_params(
                    "MATCH (u:UnresolvedCross {source_repository_id:$id}) \
                     WHERE u.valid_to_episode IS NULL \
                     SET u.valid_to_episode = $ve, u.valid_to_repository_id = $id",
                    gparams([("id", gs(&copy.id)), ("ve", gs(&copy.episode))]),
                )?;
                Ok(())
            })?;
        }
    }
    let skipped_scratch: Vec<&str> = shadowed.iter().map(|copy| copy.id.as_str()).collect();
    if repos.len() < 2 {
        let mut summary = json!({
            "repositories": repos.iter().map(|r| json!({
                "id": r.id, "name": r.name, "episode": r.episode, "current": r.current,
                "packages": [], "exported_modules": 0,
            })).collect::<Vec<_>>(),
            "pairs": [],
            "edges": 0,
            "unresolved": 0,
            "hidden_repositories": hidden,
            "persisted": hidden == 0,
            "unprojected_repositories": unprojected,
            "skipped": "fewer_than_two_repositories",
        });
        summary["pr_scratch_skipped"] = json!(skipped_scratch);
        return Ok(summary);
    }
    for repo in &repos {
        if hidden == 0 {
            graph_commit(&mut g, |g| record_packages(g, workspace, repo))?;
        }
    }
    let stamp = now();
    let mut pairs = Vec::new();
    let mut edges_total = 0usize;
    let mut unresolved_total = 0usize;
    for (i, importer) in repos.iter().enumerate() {
        let others: Vec<ExportIndex> = repos
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, r)| r.index.clone())
            .collect();
        let loader = |relpath: &str| -> Option<String> {
            let sha = importer.heads.get(relpath)?;
            blob(db, &importer.id, sha).ok().flatten()
        };
        let report = xrepo::link(
            &importer.graph,
            &importer.id,
            Some(&importer.episode),
            &importer.manifests,
            &others,
            &loader,
        );
        if hidden > 0 {
            // A restricted view cannot establish workspace-wide uniqueness of
            // an exporter. It must not overwrite shared links or unresolved
            // evidence produced with the complete workspace view.
            edges_total += report.edges.len();
            unresolved_total += report.unresolved.len();
            continue;
        }
        let exporter_episodes = json!(
            others
                .iter()
                .map(|other| (other.repo.clone(), other.episode.clone()))
                .collect::<BTreeMap<_, _>>()
        )
        .to_string();
        let mut rewritten_any = false;
        for (j, exporter) in repos.iter().enumerate() {
            if j == i {
                continue;
            }
            let live = live_pair_episodes(&mut g, &importer.id, &exporter.id)?;
            let current_pair = (importer.episode.clone(), exporter.episode.clone());
            let edges: Vec<&xrepo::CrossEdge> = report
                .edges
                .iter()
                .filter(|e| e.target.repo == exporter.id)
                .collect();
            let unchanged = live.len() == 1
                && live[0] == current_pair
                && same_cross_edges(&mut g, &importer.id, &exporter.id, &edges)?;
            let nothing_recorded = live.is_empty();
            if unchanged || (nothing_recorded && edges.is_empty()) {
                pairs.push(json!({
                    "source": importer.id, "target": exporter.id,
                    "source_episode": importer.episode, "target_episode": exporter.episode,
                    "edges": edges.len(), "inserted": 0, "closed": 0, "unchanged": true,
                }));
                edges_total += edges.len();
                continue;
            }
            let mut closed = 0usize;
            for (source_episode, _) in &live {
                let (to_repo, to_episode) = if source_episode != &importer.episode {
                    (&importer.id, &importer.episode)
                } else {
                    (&exporter.id, &exporter.episode)
                };
                closed += graph_commit(&mut g, |g| {
                    close_cross_edges(
                        g,
                        "r.source_repository_id = $sr AND r.target_repository_id = $tr \
                         AND r.source_episode = $se",
                        gparams([
                            ("sr", gs(&importer.id)),
                            ("tr", gs(&exporter.id)),
                            ("se", gs(source_episode)),
                        ]),
                        to_episode,
                        to_repo,
                    )
                })?;
            }
            graph_commit(&mut g, |g| {
                insert_cross_edges(g, workspace, importer, exporter, &edges, &stamp)
            })?;
            rewritten_any = true;
            edges_total += edges.len();
            pairs.push(json!({
                "source": importer.id, "target": exporter.id,
                "source_episode": importer.episode, "target_episode": exporter.episode,
                "edges": edges.len(), "inserted": edges.len(), "closed": closed, "unchanged": false,
            }));
        }
        // Unresolved imports are per importing repository; rewrite them when
        // the importer moved or any partner pair was re-linked.
        let live_unresolved = store_graph::rows(
            &mut g,
            "MATCH (u:UnresolvedCross {source_repository_id:$id}) \
             WHERE u.valid_to_episode IS NULL \
             RETURN DISTINCT u.source_episode AS source_episode, u.exporter_episodes AS exporter_episodes",
            gparams([("id", gs(&importer.id))]),
        )?;
        let stale = live_unresolved.iter().any(|r| {
            s(r, "source_episode") != importer.episode
                || s(r, "exporter_episodes") != exporter_episodes
        });
        let first_time = live_unresolved.is_empty();
        if rewritten_any || stale || (first_time && !report.unresolved.is_empty()) {
            graph_commit(&mut g, |g| {
                g.query_with_params(
                    "MATCH (u:UnresolvedCross {source_repository_id:$id}) \
                     WHERE u.valid_to_episode IS NULL \
                     SET u.valid_to_episode = $ve, u.valid_to_repository_id = $id",
                    gparams([("id", gs(&importer.id)), ("ve", gs(&importer.episode))]),
                )?;
                let rows: Vec<Value> = report
                    .unresolved
                    .iter()
                    .map(|u| {
                        let mut row = json!({
                            "episode": importer.episode,
                            "node_id": u.node_id,
                            "stable_id": u.stable_id,
                            "relpath": u.relpath,
                            "line": u.line,
                            "module_path": u.module_path,
                            "symbol": u.symbol,
                            "package": u.package,
                            "reason": u.reason,
                            "candidates": json!(u.candidates).to_string(),
                            "version_constraint": u.version_constraint,
                            "exporter_version": u.exporter_version,
                        });
                        // Null properties cannot be stored; absent reads
                        // back as null.
                        if let Value::Object(map) = &mut row {
                            map.retain(|_, v| !v.is_null());
                        }
                        row
                    })
                    .collect();
                for chunk in rows.chunks(BATCH) {
                    g.query_with_params(
                        "UNWIND $rows AS row \
                         CREATE (u:UnresolvedCross {workspace_id:$w, source_repository_id:$id, \
                             source_episode: row.episode, source_node_id: row.node_id}) \
                         SET u += row, u.exporter_episodes = $ee, u.created_at = $stamp",
                        gparams([
                            ("w", gs(workspace)),
                            ("id", gs(&importer.id)),
                            ("rows", gj(&json!(chunk))),
                            ("ee", gs(&exporter_episodes)),
                            ("stamp", gs(&stamp)),
                        ]),
                    )?;
                }
                Ok(())
            })?;
        }
        unresolved_total += report.unresolved.len();
    }
    let mut summary = json!({
        "repositories": repos.iter().map(|r| json!({
            "id": r.id, "name": r.name, "episode": r.episode, "current": r.current,
            "packages": r.index.packages.iter().map(|p| json!({
                "ecosystem": p.ecosystem.as_str(), "name": p.name, "version": p.version,
            })).collect::<Vec<_>>(),
            "exported_modules": r.index.modules.len(),
        })).collect::<Vec<_>>(),
        "pairs": pairs,
        "edges": edges_total,
        "unresolved": unresolved_total,
        "hidden_repositories": hidden,
        "persisted": hidden == 0,
        "unprojected_repositories": unprojected,
    });
    if !skipped_scratch.is_empty() {
        summary["pr_scratch_skipped"] = json!(skipped_scratch);
    }
    Ok(summary)
}

/// Whether the repository-relative `path` is `prefix` or lies under it.
fn below(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// For each linkable project, the current folders of the other linkable
/// projects that lie inside its own current folder, relative to it
/// (`sub/dir`): the nested-project rule. A project's current
/// folder is the one it was last synced from
/// ([`crate::folders::current_folder`]); its older aliases (a moved
/// folder's old path, or a folder whose content a later `--project` sync of
/// another folder replaced) do not describe its files and take no part,
/// nor do projects without recorded folders (synced by an earlier build,
/// or over HTTP).
fn nested_prefixes(db: &Connection, repos: &[Linkable]) -> Result<HashMap<String, Vec<String>>> {
    let mut folders: Vec<(&str, PathBuf)> = Vec::with_capacity(repos.len());
    for repo in repos {
        if let Some(folder) = crate::folders::current_folder(&*crate::store::graph_for(db)?, &repo.id)? {
            folders.push((repo.id.as_str(), folder));
        }
    }
    let mut nested: HashMap<String, Vec<String>> = HashMap::new();
    for (outer, outer_folder) in &folders {
        for (inner, inner_folder) in &folders {
            if outer == inner {
                continue;
            }
            if let Ok(relative) = inner_folder.strip_prefix(outer_folder)
                && !relative.as_os_str().is_empty()
            {
                nested
                    .entry(outer.to_string())
                    .or_default()
                    .push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(nested)
}

/// Project role that marks a PR scratch project: the projects
/// `graphicle pr` registered under a `git:` key for a commit range. Linking
/// skips one while another project exports its packages; workspace recall
/// and `list_projects` skip them (`crate::workspace`).
pub const PR_SCRATCH: &str = "pr_scratch";

/// Ids of the workspace's `pr_scratch` projects.
pub fn pr_scratch_projects(db: &Connection, workspace: &str) -> Result<BTreeSet<String>> {
    Ok(rows(
        db,
        "SELECT r.repository_id FROM repo_project_roles r JOIN repo_projects p ON p.id=r.repository_id \
         WHERE p.workspace_id=? AND r.role=?",
        params![workspace, PR_SCRATCH],
    )?
    .iter()
    .map(|row| s(row, "repository_id").to_string())
    .collect())
}

/// Mark the projects earlier `graphicle pr` runs registered as `pr_scratch`:
/// their key is `git:` and the sha256 of the checkout path. Idempotent; no
/// other data changes.
pub fn mark_legacy_pr_projects(db: &Connection) -> Result<usize> {
    let pattern = format!("git:{}", "[0-9a-f]".repeat(64));
    Ok(db.execute(
        "INSERT OR IGNORE INTO repo_project_roles(repository_id,role,created_at) \
         SELECT id,?1,?2 FROM repo_projects WHERE project_key GLOB ?3",
        params![PR_SCRATCH, now(), pattern],
    )?)
}

/// Cross-repository edges targeting the requested base/head revisions, or
/// live edges when no pair is supplied. Later exporter revisions never
/// substitute for an older report's evidence.
pub fn cross_edges_into(
    db: &Connection,
    store: &GraphStore,
    p: &Principal,
    repository: &str,
    episodes: Option<(&str, &str)>,
) -> Result<Vec<Value>> {
    let (base, head) = episodes.unwrap_or(("", ""));
    let mut out = Vec::new();
    let mut readable: HashMap<String, bool> = HashMap::new();
    for row in store.read(|g| {
        store_graph::rows(
            g,
            &format!(
                "{CROSS_MATCH} AND r.workspace_id = $w AND r.target_repository_id = $repo \
                 AND (($base = '' AND r.valid_to_episode IS NULL) \
                   OR ($base <> '' AND r.target_episode IN [$base, $head])) \
                 RETURN r.source_repository_id AS source_repository_id, \
                 r.source_episode AS source_episode, r.source_node_id AS source_node_id, \
                 r.source_stable_id AS source_stable_id, r.target_episode AS target_episode, \
                 r.target_node_id AS target_node_id, r.relation AS relation, \
                 r.confidence AS confidence, r.derivation AS derivation, \
                 r.module_path AS module_path, r.symbol AS symbol, r.line AS line, \
                 r.version_constraint AS version_constraint, r.exporter_version AS exporter_version, \
                 r.version_status AS version_status, r.valid_to_episode AS valid_to_episode \
                 ORDER BY r.created_at, r.source_repository_id, r.source_node_id, r.relation"
            ),
            gparams([
                ("w", gs(&p.workspace)),
                ("repo", gs(repository)),
                ("base", gs(base)),
                ("head", gs(head)),
            ]),
        )
    })? {
        let source = s(&row, "source_repository_id").to_string();
        let allowed = match readable.get(&source) {
            Some(allowed) => *allowed,
            None => {
                let allowed = repo_readable(db, p, &source)?;
                readable.insert(source.clone(), allowed);
                allowed
            }
        };
        if allowed {
            out.push(row);
        }
    }
    Ok(out)
}

/// Which side of a cross-repository edge a repository is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossDirection {
    /// The repository imports from another one (it is the edge's source).
    Outgoing,
    /// Another repository imports from it (it is the edge's target).
    Incoming,
}

/// The live cross-repository edges (`valid_to_episode IS NULL`) touching
/// `repository` in one direction, every column, in row order, kept only when
/// the principal may read the repository on the other side (the one
/// permission-filtered reader of both directions that `graph.cross_edges`
/// and workspace recall's cross-repository step share).
/// The caller has authorized `repository` itself.
pub fn live_cross_edges(
    db: &Connection,
    store: &GraphStore,
    p: &Principal,
    repository: &str,
    direction: CrossDirection,
) -> Result<Vec<Value>> {
    let (own, other) = match direction {
        CrossDirection::Outgoing => ("r.source_repository_id", "target_repository_id"),
        CrossDirection::Incoming => ("r.target_repository_id", "source_repository_id"),
    };
    let mut readable: HashMap<String, bool> = HashMap::new();
    let mut out = Vec::new();
    for mut edge in store.read(|g| {
        store_graph::rows(
            g,
            &format!(
                "{CROSS_MATCH} AND r.valid_to_episode IS NULL AND r.workspace_id = $w \
                 AND {own} = $repo RETURN {CROSS_COLUMNS} \
                 ORDER BY r.created_at, r.source_node_id, r.target_node_id, r.relation"
            ),
            gparams([("w", gs(&p.workspace)), ("repo", gs(repository))]),
        )
    })? {
        let side = s(&edge, other).to_string();
        let allowed = match readable.get(&side) {
            Some(allowed) => *allowed,
            None => {
                let allowed = repo_readable(db, p, &side)?;
                readable.insert(side.clone(), allowed);
                allowed
            }
        };
        if allowed {
            // Consumers dedup on `id` (a rowid in the SQL store); a stable
            // synthetic one from the edge's identity.
            edge["id"] = json!(format!(
                "{}|{}|{}|{}|{}|{}",
                s(&edge, "source_repository_id"),
                s(&edge, "source_node_id"),
                s(&edge, "target_repository_id"),
                s(&edge, "target_node_id"),
                s(&edge, "relation"),
                s(&edge, "symbol"),
            ));
            out.push(edge);
        }
    }
    Ok(out)
}

/// Live unresolved cross-repository imports that name `repository` as a
/// candidate exporter or one of its declared packages, from importing
/// repositories the principal may read.
pub fn cross_unresolved_toward(
    db: &Connection,
    store: &GraphStore,
    p: &Principal,
    repository: &str,
    head_episode: Option<&str>,
) -> Result<Vec<Value>> {
    let packages: Vec<String> = store
        .read(|g| {
            store_graph::rows(
                g,
                "MATCH (pk:Package {repository_id:$r}) \
                 WHERE $he IS NULL OR pk.episode = $he RETURN DISTINCT pk.name AS name",
                gparams([
                    ("r", gs(repository)),
                    ("he", head_episode.map(gs).unwrap_or(GValue::Null)),
                ]),
            )
        })?
        .iter()
        .map(|r| s(r, "name").to_string())
        .collect();
    let mut out = Vec::new();
    for mut row in store.read(|g| {
        store_graph::rows(
            g,
            "MATCH (u:UnresolvedCross {workspace_id:$w}) \
             WHERE u.source_repository_id <> $repo \
             RETURN u.source_repository_id AS source_repository_id, \
             u.source_episode AS source_episode, u.source_node_id AS source_node_id, \
             u.source_stable_id AS source_stable_id, u.relpath AS relpath, u.line AS line, \
             u.module_path AS module_path, u.symbol AS symbol, u.package AS package, \
             u.reason AS reason, u.candidates AS candidates, \
             u.version_constraint AS version_constraint, u.exporter_version AS exporter_version, \
             u.exporter_episodes AS exporter_episodes, u.valid_to_episode AS valid_to_episode \
             ORDER BY u.created_at, u.source_node_id",
            gparams([("w", gs(&p.workspace)), ("repo", gs(repository))]),
        )
    })? {
        match head_episode {
            None => {
                if !row["valid_to_episode"].is_null() {
                    continue;
                }
            }
            Some(head) => {
                // The exporter episode this row's evidence belongs to.
                let episodes: Map<String, Value> =
                    serde_json::from_str(s(&row, "exporter_episodes")).unwrap_or_default();
                if episodes.get(repository) != Some(&json!(head)) {
                    continue;
                }
            }
        }
        let candidates: Vec<String> =
            serde_json::from_str(s(&row, "candidates")).unwrap_or_default();
        let names_repo = candidates.iter().any(|c| c == repository);
        let package = s(&row, "package");
        let names_package = packages.iter().any(|name| {
            name == package
                || name.starts_with(&format!("{package}/"))
                || package.starts_with(&format!("{name}/"))
        });
        if !(names_repo || names_package) {
            continue;
        }
        if !repo_readable(db, p, s(&row, "source_repository_id"))? {
            continue;
        }
        let mut visible = Vec::new();
        for candidate in candidates {
            if repo_readable(db, p, &candidate)? {
                visible.push(candidate);
            }
        }
        row["candidates"] = json!(json!(visible).to_string());
        if let Value::Object(fields) = &mut row {
            fields.remove("exporter_episodes");
            fields.remove("valid_to_episode");
        }
        out.push(row);
    }
    Ok(out)
}

impl Store {
    /// Link imports across all repositories of the workspace (`graph.link`),
    /// then run the `link` job's document step now
    /// ([`crate::doc_links::relink`]): its summary, or its error, is the
    /// result's `documents`.
    pub fn graph_link(&self, workspace: &str, token: &str) -> Result<Value> {
        let (mut result, principal) = self.authorized(workspace, token, "read", |db, p| {
            Ok((link_workspace(db, &self.graph(), p)?, p.hash.clone()))
        })?;
        result["documents"] =
            match crate::doc_links::relink(self, workspace, &principal, |_| Ok(())) {
                Ok(summary) => summary,
                Err(error) => json!({"error": error.message}),
            };
        Ok(result)
    }
    /// Grant or revoke a credential's read access to one repository
    /// (`graph.access`; admin only).
    pub fn graph_access(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "admin", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let identifier = s(&repository, "id");
            let credential = string(args, "credential_id", 100)?;
            if one(
                db,
                "SELECT id FROM credentials WHERE id=? AND workspace_id=?",
                params![credential, workspace],
            )?
            .is_none()
            {
                return Err(Error::bad("Unknown credential in this workspace"));
            }
            let state = string(args, "state", 20)?;
            set_repo_access(db, workspace, identifier, credential, state)?;
            Ok(json!({"repository_id": identifier, "credential_id": credential, "state": state}))
        })
    }
    /// Live cross-repository edges and unresolved imports touching a repository.
    pub fn graph_cross_edges(
        &self,
        workspace: &str,
        token: &str,
        repository: &str,
    ) -> Result<Value> {
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, repository)?;
            let identifier = s(&repository, "id");
            let gstore = self.graph();
            Ok(json!({
                "incoming": cross_edges_into(db, &gstore, p, identifier, None)?,
                "outgoing": live_cross_edges(db, &gstore, p, identifier, CrossDirection::Outgoing)?,
                "unresolved": cross_unresolved_toward(db, &gstore, p, identifier, None)?,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_serializer_matches_text_canonical() {
        let value = json!({
            "z": {"b": [3, {"y": null, "a": "é\"\\\n"}], "a": 1.5},
            "m": [],
            "a": {},
            "k": "text",
            "n": -7,
            "big": 18446744073709551615u64,
        });
        let Value::Object(fields) = &value else {
            unreachable!()
        };
        let mut out = String::new();
        canonical_into(&mut out, &fields.clone().into());
        assert_eq!(out, graphicle_memory::text::canonical(&value));
        canonical_into(&mut out, &graphicle::record::Attributes::new());
        assert_eq!(out, "{}");
    }

    #[test]
    fn edge_identity_keeps_column_boundaries() {
        let mut first = Vec::new();
        let mut second = Vec::new();
        edge_identity(&mut first, "a\0b", "c", "calls", "");
        edge_identity(&mut second, "a", "b\0c", "calls", "");
        assert_ne!(first, second);
        edge_identity(&mut first, "a", "b", "calls\0", "");
        edge_identity(&mut second, "a", "b", "calls", "\0");
        assert_ne!(first, second);
    }
}
