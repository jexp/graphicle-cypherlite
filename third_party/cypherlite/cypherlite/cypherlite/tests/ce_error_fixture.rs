//! ADR-0116/ADR-0117: pin the engine's fixed-namespace error texts to the
//! captured Neo4j Community Edition fixture
//! (`testdata/ce-errors/neo4j-ce.json`). CE is authoritative: every runnable
//! case runs its statement on a fresh in-memory store through the public
//! Rust API (`connect_namespace`) and the failure's `Display` must end with
//! the fixture message. Cases the engine cannot run as-is are listed in
//! [`SKIPPED_CASES`] with a reason; a fixture case covered by neither the
//! run logic nor the skip list fails the test.

use std::collections::BTreeSet;

use cypherlite::{Database, Value};
use serde::Deserialize;

const FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../testdata/ce-errors/neo4j-ce.json"
);

/// Fixture cases the engine does not run as-is, each with a one-line reason.
const SKIPPED_CASES: &[(&str, &str)] = &[
    (
        "graph_names",
        "CE marks graph.names enterpriseOnly, so it is unregistered and fails 42N48 \
         \"Unknown function 'graph.names'\"; ADR-0116 §3 keeps graph.* registered in \
         CypherLite and failing 42N72 — a documented product divergence, asserted by \
         graph_names_is_a_documented_product_divergence",
    ),
    (
        "read_only_write",
        "CypherLite has no queryable read-only namespace: ADR-0229 §1 makes \
         system_ontologies fail every non-administration statement routed to it — \
         reads included — with the authored 42N17 text (the deliberate divergence \
         from live CE, which allows reads), so the CE 42N18 write case has no engine \
         namespace to run on; pinned by \
         system_ontologies_session_rejects_every_non_administration_statement in \
         tests/namespace.rs",
    ),
    (
        "read_only_call_subquery_return",
        "CE rejects a read-only top-level CALL { } at semantic-check time with 42N71 \
         (a query may not conclude with CALL), before read-only enforcement; \
         CypherLite's ADR-0229 gate rejects the same statement on system_ontologies \
         with 42N17 — same outcome, different code/layer — pinned by \
         system_ontologies_rejects_writes_nested_in_subqueries_and_label_exprs in \
         tests/namespace.rs",
    ),
    (
        "read_only_call_subquery_match",
        "CE rejects a read-only top-level CALL { } at semantic-check time with 42N71 \
         (a query may not conclude with CALL), before read-only enforcement; \
         CypherLite's ADR-0229 gate rejects the same statement on system_ontologies \
         with 42N17 — same outcome, different code/layer — pinned by \
         system_ontologies_rejects_writes_nested_in_subqueries_and_label_exprs in \
         tests/namespace.rs",
    ),
    (
        "read_only_call_subquery_create",
        "CypherLite has no queryable read-only namespace: ADR-0229 §1 makes \
         system_ontologies fail every non-administration statement routed to it \
         with the authored 42N17 text, so the CE 42N18 write case has no engine \
         namespace to run on; pinned by \
         system_ontologies_rejects_writes_nested_in_subqueries_and_label_exprs in \
         tests/namespace.rs",
    ),
];

/// The root-cause GQLSTATUS codes this fixture must pin at least once. Codes
/// pinned only by a [`SKIPPED_CASES`] entry count: the fixture still records
/// the CE code even when the engine cannot run the case as-is.
const REQUIRED_GQLSTATUS: &[&str] = &[
    "42N00", "42N72", "42NA5", "42N17", "42N18", "42N71", "42NA7", "51N27",
];

/// How a fixture case is exercised; the fixture's `harness` field is emitted
/// by scripts/ce_error_fixtures.py's HARNESS map. An unknown value fails the
/// fixture load, naming the offending variant and its line in the fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Harness {
    /// Session-scoped in CE (the driver opens a session on the unknown
    /// database); the Rust API equivalent is `connect_namespace` itself
    /// failing.
    ConnectNamespace,
    /// CE's read-only case needs a server started with
    /// `server.databases.read_only=neo4j`. CypherLite once stood in with
    /// `system_ontologies` (ADR-0116 §4's read-only namespace); ADR-0229
    /// §1 made that namespace not queryable — every non-administration
    /// statement routed to it fails with the authored 42N17 text, reads
    /// included — so every fixture case with this harness is in
    /// [`SKIPPED_CASES`] and this arm no longer selects a namespace at
    /// runtime.
    ReadOnlyNamespace,
}

/// What the fixture expects the case's statement to do; the fixture's
/// `outcome` field is emitted by scripts/ce_error_fixtures.py. An unknown
/// value fails the fixture load, naming the offending variant and its line
/// in the fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    /// The statement runs to completion.
    Success,
    /// The statement fails with the case's pinned GQLSTATUS and message.
    Error,
}

/// One fixture case. The metadata fields the assertions never read
/// (`gqlstatus_wire`, `neo4j_code`, `status_description`, `note`,
/// `source_url`) are still typed here so a renamed or dropped fixture field
/// fails at load time naming the field, instead of silently decoding to a
/// default.
#[derive(Deserialize)]
struct Case {
    id: String,
    statement: String,
    database: String,
    #[serde(default)]
    harness: Option<Harness>,
    outcome: Outcome,
    gqlstatus: String,
    message: String,
    #[serde(default)]
    #[allow(dead_code)]
    gqlstatus_wire: Option<String>,
    #[allow(dead_code)]
    neo4j_code: String,
    #[serde(default)]
    #[allow(dead_code)]
    status_description: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    note: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    source_url: Option<String>,
}

/// One pinned `SHOW DATABASES` row from the fixture; every field is compared
/// against CypherLite's own rows.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShowDatabasesRow {
    name: String,
    #[serde(rename = "type")]
    row_type: String,
    access: String,
    current_status: String,
    requested_status: String,
    default: bool,
    home: bool,
    aliases: Vec<String>,
    constituents: Vec<String>,
}

/// The decoded fixture: the error-text cases and the pinned `SHOW DATABASES`
/// rows.
#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
    show_databases: Vec<ShowDatabasesRow>,
}

/// Read and decode the fixture. A missing file, invalid JSON, or a
/// missing/renamed/mistyped field panics naming the path, the serde error
/// (which names the field or variant), and the offending fixture line.
fn load_fixture() -> Fixture {
    let raw = std::fs::read_to_string(FIXTURE_PATH)
        .unwrap_or_else(|e| panic!("cannot read {FIXTURE_PATH}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| {
        let line = raw
            .lines()
            .nth(e.line().saturating_sub(1))
            .unwrap_or_default()
            .trim();
        panic!("cannot parse {FIXTURE_PATH}: {e}\n  at: {line}");
    })
}

/// Run one fixture case on a fresh in-memory store and return the outcome:
/// `Ok(())` for a successful statement, `Err(display)` for a failure. How the
/// case is exercised comes from the fixture's `harness` field (emitted by
/// scripts/ce_error_fixtures.py's HARNESS map), not from case ids here.
fn run_case(case: &Case) -> Result<(), String> {
    let db = Database::open(":memory:").unwrap();
    let namespace = match case.harness {
        Some(Harness::ConnectNamespace) => {
            return db
                .connect_namespace(&case.database)
                .map(|_| ())
                .map_err(|e| e.to_string());
        }
        Some(Harness::ReadOnlyNamespace) => "system_ontologies",
        None => case.database.as_str(),
    };
    let mut conn = db
        .connect_namespace(namespace)
        .unwrap_or_else(|e| panic!("{}: connect_namespace({namespace}): {e}", case.id));
    conn.query(&case.statement)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// CE canonicalizes string literals to double quotes when it echoes the
/// attempted graph reference ("Attempted to access graph
/// graph.byName(\"neo4j\")"); CypherLite echoes the reference as written.
/// Normalize the quote style so the comparison is about the text, not the
/// literal quoting convention.
fn ce_echo_normalized(message: &str) -> String {
    message.replace('"', "'")
}

#[test]
fn fixture_cases_match_ce_texts() {
    let fixture = load_fixture();
    let cases = &fixture.cases;
    assert!(!cases.is_empty(), "fixture has no cases");

    let skipped: std::collections::BTreeMap<&str, &str> = SKIPPED_CASES.iter().copied().collect();
    let mut covered: BTreeSet<String> = BTreeSet::new();
    let mut pinned_codes: BTreeSet<String> = BTreeSet::new();

    for case in cases {
        pinned_codes.insert(case.gqlstatus.clone());
        if let Some(reason) = skipped.get(case.id.as_str()) {
            covered.insert(case.id.clone());
            eprintln!("SKIP {}: {reason}", case.id);
            continue;
        }
        match (run_case(case), case.outcome) {
            (Ok(()), Outcome::Success) => {}
            (Ok(()), Outcome::Error) => panic!(
                "{}: {:?} succeeded but the fixture expects {}: {:?}",
                case.id, case.statement, case.gqlstatus, case.message
            ),
            (Err(display), Outcome::Error) => assert!(
                display.ends_with(&ce_echo_normalized(&case.message)),
                "{}: {:?} failed with {display:?}, which does not end with the CE text {:?}",
                case.id,
                case.statement,
                case.message
            ),
            (Err(display), Outcome::Success) => panic!(
                "{}: {:?} failed with {display:?} but the fixture expects success",
                case.id, case.statement
            ),
        }
        covered.insert(case.id.clone());
    }

    // Every fixture case is covered by the run logic or the skip list (a
    // stale skip-list entry for a removed case also fails here).
    let fixture_ids: BTreeSet<&str> = cases.iter().map(|case| case.id.as_str()).collect();
    for id in skipped.keys() {
        assert!(
            fixture_ids.contains(id),
            "SKIPPED_CASES entry {id:?} names no fixture case"
        );
    }
    assert_eq!(
        covered.len(),
        cases.len(),
        "fixture cases covered: {covered:?}"
    );

    for code in REQUIRED_GQLSTATUS {
        assert!(
            pinned_codes.contains(*code),
            "no fixture case pins GQLSTATUS {code}"
        );
    }
}

/// `graph.names` is a documented product divergence (ADR-0116 §3): CE does
/// not register it (42N48), CypherLite keeps it registered and fails 42N72.
#[test]
fn graph_names_is_a_documented_product_divergence() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect_namespace("neo4j").unwrap();
    let err = conn.query("RETURN graph.names()").unwrap_err().to_string();
    assert!(
        err.ends_with(
            "Dynamic graph lookup not allowed here. This feature is only available on composite databases.\nAttempted to access graph graph.names()"
        ),
        "{err}"
    );
    // … and it stays listed by SHOW FUNCTIONS.
    let result = conn.query("SHOW FUNCTIONS YIELD name RETURN name").unwrap();
    assert!(
        result
            .rows
            .iter()
            .any(|row| row[0] == Value::String("graph.names".into())),
        "graph.names not in SHOW FUNCTIONS"
    );
}

/// CypherLite's own `SHOW DATABASES` rows for `neo4j` and `system` equal the
/// fixture's CE rows on every pinned field.
#[test]
fn show_databases_rows_match_the_fixture() {
    let fixture = load_fixture();

    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn
        .query(
            "SHOW DATABASES YIELD name, type, access, currentStatus, requestedStatus, \
             default, home, aliases, constituents \
             RETURN name, type, access, currentStatus, requestedStatus, \
             default, home, aliases, constituents",
        )
        .unwrap();

    for fixture_row in &fixture.show_databases {
        let name = fixture_row.name.as_str();
        let row = result
            .rows
            .iter()
            .find(|row| row[0] == Value::String(name.into()))
            .unwrap_or_else(|| panic!("no SHOW DATABASES row for {name}: {:?}", result.rows));
        let expected = vec![
            Value::String(fixture_row.name.as_str().into()),
            Value::String(fixture_row.row_type.as_str().into()),
            Value::String(fixture_row.access.as_str().into()),
            Value::String(fixture_row.current_status.as_str().into()),
            Value::String(fixture_row.requested_status.as_str().into()),
            Value::Bool(fixture_row.default),
            Value::Bool(fixture_row.home),
            Value::List(Vec::new().into()),
            Value::List(Vec::new().into()),
        ];
        assert!(
            fixture_row.aliases.is_empty(),
            "fixture aliases for {name} is not empty"
        );
        assert!(
            fixture_row.constituents.is_empty(),
            "fixture constituents for {name} is not empty"
        );
        assert_eq!(row, &expected, "SHOW DATABASES row for {name}");
    }
}
