//! Helpers shared across the `cypherlite` integration-test crates (each
//! file directly under `tests/` is its own crate; this module is the
//! single source for the helpers they share): general-purpose helpers,
//! plus the pinned Neo4j CE error texts for the fixed-namespace tests
//! (testdata/ce-errors/neo4j-ce.json), the constraint-violation error
//! texts, the error-text extractor, the `db.ontology.*` document helpers
//! (`set_doc`/`get_doc`), the WAL segment-magic readers, the WAL frame
//! encoder, the crash-seeded store fixtures
//! (`store_vfs`/`active_segment`/`seed_store`), and the ADR-0120 checkpoint
//! format-marker assertion, plus the `ontology` preview opt-in
//! (`preview_ontology`, ADR-0378).
//!
//! Each including test crate uses a different subset of these helpers, so
//! the ones it does not use would warn as dead code — allow it module-wide.
#![allow(dead_code)]

use std::collections::BTreeMap;

/// CE 42N17 suffix (ADR-0116 §4; testdata/ce-errors/neo4j-ce.json
/// `system_data_clause`).
pub const SYSTEM_CLAUSES_SUFFIX: &str = "The system database supports a restricted set of Cypher clauses. The supported clause structure for procedure calls is: CALL, YIELD, RETURN. YIELD and RETURN clauses are optional. The order of the clauses is fixed and each can only occur once.";
/// CE 42N18 text (ADR-0116 §4/§9; CE appends "on this Neo4j instance." to
/// the ADR wording — testdata/ce-errors/neo4j-ce.json `read_only_write`).
/// Still the recorded CE text for read-only databases; the
/// `system_ontologies` gate no longer uses it (ADR-0229 §2).
pub const READ_ONLY_SUFFIX: &str =
    "No write operations are allowed on this database. The database is in read-only mode on this Neo4j instance.";
/// The authored 42N17 text every non-administration statement routed to
/// `system_ontologies` fails with (ADR-0229 §2: reads included,
/// EXPLAIN-wrapped included). Not CE-pinned — live CE allows reads on the
/// namespace, so the fixture carries no case for it.
pub const NOT_QUERYABLE_SUFFIX: &str =
    "This database is not queryable. Use the db.ontology.* procedures on the user database 'neo4j' instead.";

pub fn err_text<T>(result: cypherlite::Result<T>) -> String {
    // `err().expect` over `unwrap_err`: no `T: Debug` bound, so callers
    // never force-fit their Ok type.
    result.err().expect("expected an error").to_string()
}

/// Opt this test process into the `ontology` preview feature (ADR-0378):
/// the `db.ontology.*` procedures are gated behind
/// `CYPHERLITE_PREVIEW=ontology`, and `Database::open` reads the variable
/// once per open. `Once` because a test binary runs its tests on threads
/// sharing one environment; every test in the enabled-gate ontology crates
/// calls this before its first open, and no test ever toggles the variable
/// back.
pub fn preview_ontology() {
    set_preview_once("ontology");
}

/// The shared opt-in of the gated-connection fixtures: the preview list is
/// set exactly once per test process (`Once` because a test binary runs
/// its tests on threads sharing one environment -- a per-connection
/// `set_var` races a sibling thread's in-flight `Database::open`, the
/// round-13 stellar F-13-4 hazard), and no test ever toggles the
/// variable back.
pub fn set_preview_once(features: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::set_var("CYPHERLITE_PREVIEW", features));
}

/// A fresh connection on a fresh in-memory store — the shared starting
/// point of the fixed-namespace integration tests.
pub fn connection() -> cypherlite::Connection {
    cypherlite::Database::open(":memory:")
        .unwrap()
        .connect()
        .unwrap()
}

/// One `:Person` and one `:City` — the two-label store the notification
/// tests' disjoint multi-pattern cartesian statements name.
pub fn two_label_store() -> cypherlite::Connection {
    let mut conn = connection();
    conn.query("CREATE (:Person)").unwrap();
    conn.query("CREATE (:City)").unwrap();
    conn
}
/// The plan tree an `EXPLAIN`/`PROFILE` statement stamps on its result —
/// the shared guarded accessor: the per-test files never re-spell
/// `plan_root().expect(…)`.
pub fn plan(result: &cypherlite::QueryResult) -> &cypherlite::PlanNode {
    result.plan_root().expect("EXPLAIN/PROFILE stamps the plan")
}

/// Whether the result carries a notification of `kind` — the typed predicate
/// the notification test files share. The vocabulary is a typed enum
/// (ADR-0363), so tests discriminate by kind rather than by wire code.
pub fn has_notification(
    result: &cypherlite::QueryResult,
    kind: cypherlite_model::wire::NotificationKind,
) -> bool {
    result
        .notifications()
        .iter()
        .any(|notification| notification.kind() == kind)
}

/// The first node with the given operator name, pre-order — the shared
/// fixture for pinning an affected operator in a plan tree (ADR-0359:
/// the engine emits the tree only, so assertions target operators, not
/// rendered lines).
/// The first (pre-order) node with the given operator name — the
/// early-return view of [`find_nodes`], one recursion for the one
/// matching policy.
pub fn find_node<'a>(
    node: &'a cypherlite::PlanNode,
    operator: &str,
) -> Option<&'a cypherlite::PlanNode> {
    find_nodes(node, operator).into_iter().next()
}

/// Every node with the given operator name, pre-order — the shared
/// fixture for counting a statement's occurrences of an operator.
pub fn find_nodes<'a>(
    node: &'a cypherlite::PlanNode,
    operator: &str,
) -> Vec<&'a cypherlite::PlanNode> {
    let mut collected = Vec::new();
    collect_nodes(node, operator, &mut collected);
    collected
}

fn collect_nodes<'a>(
    node: &'a cypherlite::PlanNode,
    operator: &str,
    collected: &mut Vec<&'a cypherlite::PlanNode>,
) {
    if node.operator == operator {
        collected.push(node);
    }
    for child in &node.children {
        collect_nodes(child, operator, collected);
    }
}

/// `Value::List` from a Rust vec (F-1-4: one definition — the three test
/// files that each copied it now share this one).
pub fn list(values: Vec<cypherlite::Value>) -> cypherlite::Value {
    cypherlite::Value::List(values.into())
}

/// The `BTreeMap` parameter map the integration tests pass to
/// `query_with_params`, from `(name, value)` pairs (the hoisted twin of the
/// `params` helper `bytes_value.rs` and `parameter_missing.rs` each carried;
/// ADR-0335's round-1 stellar nit).
pub fn params(entries: &[(&str, cypherlite::Value)]) -> BTreeMap<String, cypherlite::Value> {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_string(), value.clone()))
        .collect()
}

/// Builds a string value for row assertions.
pub fn string(value: &str) -> cypherlite::Value {
    cypherlite::Value::String(value.into())
}

/// Runs `query`, which must succeed.
pub fn run(connection: &mut cypherlite::Connection, query: &str) -> cypherlite::QueryResult {
    connection
        .query(query)
        .unwrap_or_else(|e| panic!("query failed: {e}\n{query}"))
}

/// The rows of `query`, which must succeed.
pub fn rows(connection: &mut cypherlite::Connection, query: &str) -> Vec<Vec<cypherlite::Value>> {
    run(connection, query).rows
}

/// The first cell of the first row of `query`, e.g. a `count(…)`.
pub fn scalar(connection: &mut cypherlite::Connection, query: &str) -> cypherlite::Value {
    rows(connection, query).remove(0).remove(0)
}

/// [`scalar`], but asserts the query returns exactly one row.
pub fn scalar_one(connection: &mut cypherlite::Connection, query: &str) -> cypherlite::Value {
    let mut rs = rows(connection, query);
    assert_eq!(rs.len(), 1, "{query}: expected a single row");
    rs.remove(0).remove(0)
}

/// The error text of a `unique` or `key` violation of constraint `name`
/// (ADR-0172).
pub fn uniqueness(name: &str) -> String {
    format!("semantic error: ConstraintValidationFailed: `{name}` uniqueness")
}

/// The error text of a missing `key` or `exists` property of constraint
/// `name` (ADR-0172).
pub fn requires(name: &str) -> String {
    format!("semantic error: ConstraintValidationFailed: `{name}` requires properties")
}

/// The error text of a `propertyType` violation of constraint `name`
/// (ADR-0172).
pub fn property_type(name: &str) -> String {
    format!("semantic error: ConstraintValidationFailed: `{name}` property type")
}

/// A `YIELD` of a column the procedure does not produce must fail
/// validation with an `UnknownProcedureOutput` error naming the column.
pub fn assert_unknown_output_fails(query: &str, unknown_column: &str) {
    let mut connection = connection();
    let error = connection.query(query).unwrap_err();
    assert!(
        error.to_string().contains("UnknownProcedureOutput"),
        "error should be an unknown-output validation error, got: {error}"
    );
    assert!(
        error.to_string().contains(unknown_column),
        "error should name the unknown column `{unknown_column}`, got: {error}"
    );
}

/// The CypherLite-authored Ontology Graph Spec 1.0.0 example
/// (`cypherlite-ontology/schema/example.json`, the main shape ADR-0364 §1
/// pins) — the shared valid document the `db.ontology.*` fixtures pass as
/// `$ontology`. `set` validates it; `apply` refuses the constructs the
/// derived graph cannot express, which the fixtures that need `apply`
/// assert on rather than rely on.
pub const ONTOLOGY_DOC: &str = include_str!("../../../cypherlite-ontology/schema/example.json");

/// `CALL db.ontology.set($ontology)` with `doc` — Result-returning, so a
/// test can assert on the error text of a refused document as well as on
/// a successful set.
pub fn set_doc(
    conn: &mut cypherlite::Connection,
    doc: &str,
) -> cypherlite::Result<cypherlite::QueryResult> {
    conn.query_with_params(
        "CALL db.ontology.set($ontology)",
        std::collections::BTreeMap::from([(
            "ontology".to_string(),
            cypherlite::Value::String(doc.into()),
        )]),
    )
}

/// The single `ontology` cell of a `db.ontology.get` query, which must
/// succeed with exactly one row.
pub fn get_doc(conn: &mut cypherlite::Connection, cypher: &str) -> cypherlite::Value {
    let result = conn.query(cypher).unwrap();
    assert_eq!(result.header.columns, ["ontology"]);
    assert_eq!(result.rows.len(), 1);
    result.rows[0][0].clone()
}

/// `(segment name, magic)` of every WAL segment under `dir`, sorted by
/// name — the last entry is the active segment.
pub fn segment_magics(dir: &std::path::Path) -> Vec<(String, [u8; 8])> {
    let mut segments: Vec<(String, [u8; 8])> = std::fs::read_dir(dir.join("wal"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension().and_then(|e| e.to_str()) == Some("log")).then(|| {
                let bytes = std::fs::read(&path).unwrap();
                (
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    bytes[..8].try_into().unwrap(),
                )
            })
        })
        .collect();
    segments.sort();
    segments
}

/// Just the magics of [`segment_magics`], in segment-name order.
pub fn magics(dir: &std::path::Path) -> Vec<[u8; 8]> {
    segment_magics(dir)
        .into_iter()
        .map(|(_, magic)| magic)
        .collect()
}

/// Open a store's VFS by the storage crate's own path rule: the bundle VFS
/// for a `.cl` file, the Native VFS for a directory. A `BundleVfs` holds
/// the host file's exclusive flock, so the handle must be dropped before
/// `Database::open` on the same store.
pub fn store_vfs(root: &std::path::Path) -> Box<dyn cypherlite_storage::Vfs> {
    if cypherlite_storage::is_bundle_path(root) {
        Box::new(cypherlite_storage::BundleVfs::open_or_create(root).unwrap())
    } else {
        Box::new(cypherlite_storage::NativeVfs::new(root))
    }
}

/// The store-relative name of the active (highest-numbered) WAL segment,
/// e.g. `wal/00000001.log`.
pub fn active_segment(vfs: &dyn cypherlite_storage::Vfs) -> String {
    let mut segments: Vec<String> = vfs
        .list_dir("wal")
        .unwrap()
        .into_iter()
        .filter(|name| name.ends_with(".log"))
        .collect();
    segments.sort();
    format!("wal/{}", segments.last().expect("a WAL segment must exist"))
}

/// Create a store through the public `Database` API under `codec`, commit
/// once, checkpoint, commit again, then close with
/// `leak_without_checkpoint` (a crash after the WAL fsync) so the last
/// commit lives only in the active WAL segment.
pub fn seed_store(root: &std::path::Path, codec: cypherlite_storage::SnapshotCodecKind) {
    let db = cypherlite::Database::open_with(
        root.to_str().unwrap(),
        cypherlite::OpenOptions {
            checkpoint_interval: Some(std::time::Duration::ZERO),
            snapshot_codec: Some(codec),
            ..cypherlite::OpenOptions::default()
        },
    )
    .unwrap();
    let mut conn = db.connect().unwrap();
    conn.query("CREATE (:T {name: 'checkpointed'})").unwrap();
    db.checkpoint().unwrap();
    conn.query("CREATE (:T {name: 'tailed'})").unwrap();
    drop(conn);
    db.leak_without_checkpoint();
}

/// The one WAL frame encoder the crash-planting fixtures share: CRC-32 of
/// the payload, then the payload length as a little-endian `u32`, then the
/// payload verbatim (ADR-0120 §3). Each fixture builds its own payload.
pub fn wal_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// A complete, CRC-valid WAL frame carrying one `OntologySet` op with `spec`
/// verbatim (ADR-0120 §3/§4) — the raw path a pre-bump binary's store carries
/// a document the public `set` would now refuse. `spec` is JSON-escaped by
/// the serializer, so any document syntax can be planted. The receiving
/// segment must carry the `MDBWAL02` ontology magic (ADR-0120 §4): the
/// scanner rejects an ontology op in an `MDBWAL01` segment.
pub fn ontology_set_frame(lsn: u64, commit_ts: u64, spec: &str) -> Vec<u8> {
    let payload = serde_json::json!({
        "lsn": lsn,
        "commit_ts": commit_ts,
        "ops": [{"op": "OntologySet", "database": "neo4j", "spec": spec}],
    })
    .to_string();
    wal_frame(payload.as_bytes())
}

/// Assert the ADR-0120 §4/§8 checkpoint format markers of a Native store
/// directory, per snapshot codec — the Parquet-`format_version` vs
/// JSON-`value_encoding` duality of the published ontology. While the
/// ontology is held (`held`) a Parquet manifest carries `format_version` 4
/// and a JSON `graph.json` carries `value_encoding` 3, publishing the
/// document verbatim into its `ontology` field (`document` asserts the
/// published bytes when given). In the plain format (`!held`) the manifest
/// is `format_version` 3 and `graph.json` is `value_encoding` 2, and no
/// ontology is published anywhere — no `ontology_path` in the manifest, no
/// `ontology` field in `graph.json`.
pub fn assert_checkpoint_format(
    dir: &std::path::Path,
    codec: cypherlite_storage::SnapshotCodecKind,
    held: bool,
    document: Option<&str>,
) {
    match codec {
        cypherlite_storage::SnapshotCodecKind::Parquet => {
            let raw = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
            let manifest: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(manifest["format_version"], if held { 4 } else { 3 });
            if !held {
                assert!(
                    !raw.contains("ontology_path"),
                    "the v3 manifest carries no ontology_path: {manifest}"
                );
            }
        }
        cypherlite_storage::SnapshotCodecKind::Json => {
            let graph: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.join("graph.json")).unwrap()).unwrap();
            assert_eq!(graph["value_encoding"], if held { 3 } else { 2 });
            if !held {
                assert!(
                    graph.get("ontology").is_none(),
                    "graph.json carries no ontology field: {graph}"
                );
            } else if let Some(document) = document {
                assert_eq!(graph["ontology"], document);
            }
        }
    }
}
