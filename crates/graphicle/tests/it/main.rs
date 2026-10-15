//! The runtime's integration tests, compiled into one test binary (`it`)
//! instead of one statically linked executable per file. The files moved here
//! from `tests/` as modules of this crate (their `include_str!` paths now
//! start one folder further up), and every test keeps its name under its
//! module: `cargo test --test it workflows::` runs one file's tests.
//!
//! Two suites stay binaries of their own, because they change state the
//! whole process shares: `tests/extension.rs` installs a probe extension in
//! the process-wide registry, and `tests/embedding_cache.rs` sets
//! `GRAPHICLE_EMBEDDING_CACHE` for the process.

/// Installs the edition under test before a fixture is built: nothing in
/// Graphicle Core. An extension crate can compile these suites into its own
/// test binary, whose `edition_setup` installs its extensions; the suites
/// call `crate::edition_setup()` and `crate::fork_guard`.
pub fn edition_setup() {}

/// The graph store beside a test's SQLite connection (the projection's
/// bi-temporal state lives there, not in SQLite).
#[allow(dead_code)]
pub fn gstore(db: &rusqlite::Connection) -> graphicle_runtime::store_graph::GraphStore {
    graphicle_runtime::store::graph_for(db).unwrap()
}

/// A read-only Cypher probe against a test's graph store.
#[allow(dead_code)]
pub fn grows(
    db: &rusqlite::Connection,
    cypher: &str,
    params: &[(&str, serde_json::Value)],
) -> Vec<serde_json::Value> {
    graphicle_runtime::graph_projection::graph_rows(&gstore(db), cypher, params).unwrap()
}

/// One write statement against a test's graph store (tampering, fixture
/// setup).
#[allow(dead_code)]
pub fn gwrite(db: &rusqlite::Connection, cypher: &str, params: &[(&str, serde_json::Value)]) {
    graphicle_runtime::graph_projection::graph_write(&gstore(db), cypher, params).unwrap()
}

/// Rewrite every recorded episode's `attributes` blob in the graph store
/// (degradation tests that tamper with the stored manifest).
#[allow(dead_code)]
pub fn rewrite_episode_attributes(
    db: &rusqlite::Connection,
    repo: &str,
    mut edit: impl FnMut(&mut serde_json::Value),
) {
    let rows = grows(
        db,
        "MATCH (e:Episode {repository_id:$r}) RETURN e.episode AS episode, e.attributes AS attributes",
        &[("r", serde_json::json!(repo))],
    );
    for row in rows {
        let mut attributes: serde_json::Value =
            serde_json::from_str(graphicle_runtime::store::s(&row, "attributes")).unwrap();
        edit(&mut attributes);
        gwrite(
            db,
            "MATCH (e:Episode {repository_id:$r, episode:$e}) SET e.attributes = $a",
            &[
                ("r", serde_json::json!(repo)),
                ("e", row["episode"].clone()),
                ("a", serde_json::json!(attributes.to_string())),
            ],
        );
    }
}

/// Child processes versus tests that rely on flock exclusivity.
mod fork_guard;

mod archived;
mod ask;
mod async_requests;
mod auto_observe;
mod backends;
mod cache_review;
mod capture;
mod code_relationships;
mod core_limits;
mod cross_repo;
mod deletion;
mod doc_links;
mod easy_flow;
mod easy_review;
mod easy_setup;
mod embedding_batch;
mod embedding_workflow;
mod evidence_windows;
mod expansion;
mod fault_isolation;
mod forgetting;
mod freshness;
mod graph_always;
mod graph_command;
mod graph_history;
mod graph_projection;
mod grounded_v2;
mod grounded_v3;
mod invalidation;
mod jobs_bounds;
mod local_inference;
mod memory_handoffs;
mod memory_symbols;
mod observer;
mod one_server;
mod permissions_refresh;
mod product_workspace;
mod project_identity;
mod provider_http_hosts;
mod query_code_graph_arm;
mod query_graph_source;
mod query_variants;
mod receipts;
mod release_0_4;
mod rerank_depth;
mod retention;
mod retrieval_defaults;
mod revocation;
mod scale_bounds;
mod selective_integration;
mod service_gaps;
mod store_compat;
mod structured_memory;
mod temporal_graph;
mod ux_terminal;
mod workflows;
mod workspace_scope;
