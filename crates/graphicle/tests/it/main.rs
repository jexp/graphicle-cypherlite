//! The runtime's integration tests, compiled into one test binary (`it`)
//! instead of one statically linked executable per file. The files moved here
//! from `tests/` as modules of this crate (their `include_str!` paths now
//! start one folder further up), and every test keeps its name under its
//! module: `cargo test --test it workflows::` runs one file's tests.
//!
//! One suite stays a binary of its own, because it changes state the whole
//! process shares: `tests/extension.rs` installs a probe extension in the
//! process-wide registry.

/// Installs the edition under test before a fixture is built: nothing in
/// Graphicle Core. An extension crate can compile these suites into its own
/// test binary, whose `edition_setup` installs its extensions; the suites
/// call `crate::edition_setup()` and `crate::fork_guard`.
pub fn edition_setup() {}

/// The graph store beside a test's SQLite connection (the projection's
/// bi-temporal state lives there, not in SQLite).
#[allow(dead_code)]
pub fn gstore(db: &rusqlite::Connection) -> graphicle_runtime::store_graph::GraphStore {
    (*graphicle_runtime::store::graph_for(db).unwrap()).clone()
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

/// The fixture-home environment of the command-line suites' children.
mod child_env;

/// The sync-shaped fixture the architecture suites share.
mod arch;

mod archived;
mod arch_enrich;
mod arch_rules;
mod ask;
mod async_requests;
mod auto_observe;
mod backends;
mod cache_review;
mod capture;
mod code_relationships;
mod core_limits;
mod cross_process;
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
mod fix_command;
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
mod lean_replies;
mod local_inference;
mod long_context_cache;
mod memory_graph;
mod memory_handoffs;
mod memory_symbols;
mod observer;
mod offboard;
mod one_server;
mod passage_index;
mod rule_documents;
mod permissions_refresh;
mod pixel;
mod product_workspace;
mod project_identity;
mod provider_http_hosts;
mod query_code_graph_arm;
mod query_file;
mod query_graph_source;
mod query_variants;
mod receipts;
mod release_0_4;
mod rerank_depth;
mod retention;
mod retrieval_defaults;
mod review_command;
mod revocation;
mod scale_bounds;
mod see;
mod selective_integration;
mod service_gaps;
mod store_compat;
mod structured_memory;
mod symbol_links;
mod temporal_graph;
mod ux_terminal;
mod workflows;
mod workspace_scope;

/// Query the fixture store's graph — the connector/account/folder domain
/// lives there, not in SQLite, so suites that used to poke
/// `am_connectors`/`am_connector_docs`/`am_secrets`/`am_usage`/
/// `repo_folders`/`am_work_state` with SQL read them here.
#[allow(dead_code)]
pub fn gql(
    store: &graphicle_runtime::store::Store,
    cypher: &str,
    params: graphicle_runtime::store_graph::Params,
) -> Vec<serde_json::Value> {
    store
        .graph()
        .read(|db| graphicle_runtime::store_graph::rows(db, cypher, params))
        .unwrap()
}

/// One graph row (or value) — the read counterpart of a `query_row` poke.
#[allow(dead_code)]
pub fn gql_one(
    store: &graphicle_runtime::store::Store,
    cypher: &str,
    params: graphicle_runtime::store_graph::Params,
) -> Option<serde_json::Value> {
    store
        .graph()
        .read(|db| graphicle_runtime::store_graph::one(db, cypher, params))
        .unwrap()
}

/// A graph write — the counterpart of a SQL `execute` poke.
#[allow(dead_code)]
pub fn gql_write(
    store: &graphicle_runtime::store::Store,
    cypher: &str,
    params: graphicle_runtime::store_graph::Params,
) {
    store
        .graph()
        .transaction(|db| {
            graphicle_runtime::store_graph::write(db, cypher, params.clone()).map(|_| ())
        })
        .unwrap()
}

/// `("$key", value)` as a params entry.
#[allow(dead_code)]
pub fn gp(
    key: &str,
    value: serde_json::Value,
) -> (String, cypherlite::Value) {
    (key.into(), graphicle_runtime::store_graph::json_to_value(&value))
}

/// Copy a store's graph directory (`reviews.graph`, the connector/account
/// domain's home) beside the SQLite backup a test fork makes of it.
#[allow(dead_code)]
pub fn copy_graph(store: &graphicle_runtime::store::Store, into: &std::path::Path) {
    let Some(path) = store.graph().path().map(std::path::Path::to_path_buf) else {
        return;
    };
    if !path.exists() {
        return;
    }
    store.graph().checkpoint().unwrap();
    fn copy(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    copy(&path, &into.join("reviews.graph"));
}
