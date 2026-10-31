//! The fail-closed `CYPHERLITE_PREVIEW` gate on the `db.ontology.*`
//! procedures (ADR-0378): with the opt-in absent, every procedure call —
//! `get` included — fails with ADR-0371's authored preview error before
//! arity/`YIELD` validation and before the namespace gates, and
//! `SHOW PROCEDURES` omits the four rows in every composed form. Stored
//! state is unaffected: a store holding a document opens, checkpoints and
//! reloads with the gate off.
//!
//! The enabled side (the calls succeed and the four rows are listed) lives
//! in the four ontology test crates, which opt in through
//! `common::preview_ontology`. This crate must never opt in — it exists to
//! pin the closed gate.

mod common;

use cypherlite::test_opts::assert_preview_unset;
use cypherlite::{Database, Value};

use common::{err_text, set_doc, ONTOLOGY_DOC};

/// ADR-0371's authored disabled-feature text for the `ontology` row.
const DISABLED: &str = "semantic error: SemanticError: preview feature 'ontology' is not enabled: \
     set CYPHERLITE_PREVIEW=ontology";

/// A fresh in-memory store and connection, opened only after the shared
/// closed-gate precondition holds: a dev shell exporting `CYPHERLITE_PREVIEW`
/// must fail loudly instead of silently testing the enabled path.
fn open_closed() -> (Database, cypherlite::Connection) {
    assert_preview_unset();
    let db = Database::open(":memory:").unwrap();
    let conn = db.connect().unwrap();
    (db, conn)
}

/// Assert every statement in `statements` is refused with exactly the
/// authored preview text; the statement text rides in the failure message
/// so a regression names the offending form.
fn assert_all_disabled(conn: &mut cypherlite::Connection, statements: &[&str]) {
    for &cypher in statements {
        assert_eq!(err_text(conn.query(cypher)), DISABLED, "{cypher}");
    }
}

/// Every standalone call fails closed with exactly the authored text —
/// including the parameterized `set` form with a valid document, so the
/// refusal is the preview gate, not validation.
#[test]
fn every_standalone_procedure_fails_closed() {
    let (_db, mut conn) = open_closed();
    let err = err_text(set_doc(&mut conn, ONTOLOGY_DOC));
    assert_eq!(err, DISABLED, "CALL db.ontology.set($ontology)");
    assert_all_disabled(
        &mut conn,
        &[
            "CALL db.ontology.get()",
            "CALL db.ontology.drop()",
            "CALL db.ontology.apply()",
        ],
    );
}

/// The gate runs before anything is written: a refused `set` leaves the
/// store without a document (ADR-0378's placement above all later gates).
#[test]
fn failed_set_leaves_the_store_without_a_document() {
    let (db, mut conn) = open_closed();
    let err = err_text(set_doc(&mut conn, ONTOLOGY_DOC));
    assert_eq!(err, DISABLED, "CALL db.ontology.set($ontology)");
    assert_eq!(db.ontology_document(), Ok(None));
}

/// The preview error precedes arity validation, `YIELD` validation, the
/// `CALL { … }` body, and the EXPLAIN/PROFILE wrappers.
#[test]
fn preview_error_precedes_arity_yield_and_wrappers() {
    let (_db, mut conn) = open_closed();
    assert_all_disabled(
        &mut conn,
        &[
            "CALL db.ontology.set()",
            "CALL db.ontology.get() YIELD ontology RETURN ontology",
            "CALL { CALL db.ontology.get() YIELD ontology RETURN ontology } RETURN ontology",
            "EXPLAIN CALL db.ontology.get()",
            "PROFILE CALL db.ontology.drop()",
        ],
    );
}

/// The same full descent `validate_calls` uses: a `CALL { … }` body, an
/// expression subquery (ADR-0378's `EXISTS` position), a `WHEN`/`ELSE`
/// branch, a `FOREACH` body and a `NEXT` stage.
#[test]
fn nested_calls_are_gated() {
    let (_db, mut conn) = open_closed();
    assert_all_disabled(
        &mut conn,
        &[
            "CALL { CALL db.ontology.get() YIELD ontology RETURN ontology } RETURN 1 AS x",
            "RETURN EXISTS { CALL db.ontology.get() } AS x",
            "WHEN true THEN { CALL db.ontology.apply() }",
            "FOREACH (x IN [1] | CALL db.ontology.drop())",
            "CREATE (:L) RETURN 1 AS x NEXT CALL db.ontology.set('x')",
        ],
    );
}

/// The preview error precedes the namespace gates (`USE`-routed and via the
/// session namespace) and ADR-0121 §3's implicit-transaction-only rule for
/// `apply`/`set`/`drop`.
#[test]
fn preview_error_precedes_namespace_and_transaction_gates() {
    let (db, mut conn) = open_closed();
    // Routed to `system_ontologies`, the preview error precedes ADR-0229
    // §2's 42N17 text; on `system` it precedes ADR-0227 §1's
    // target-namespace text.
    assert_all_disabled(
        &mut conn,
        &[
            "USE system CALL db.ontology.get()",
            "USE system_ontologies CALL db.ontology.get()",
        ],
    );
    let mut system = db.connect_namespace("system").unwrap();
    assert_eq!(err_text(system.query("CALL db.ontology.drop()")), DISABLED);
    let mut ontologies = db.connect_namespace("system_ontologies").unwrap();
    assert_eq!(
        err_text(ontologies.query("CALL db.ontology.get() YIELD ontology RETURN ontology")),
        DISABLED
    );
    // In an open explicit transaction the preview gate still wins over the
    // implicit-transaction-only rule.
    conn.begin().unwrap();
    assert_eq!(err_text(conn.query("CALL db.ontology.apply()")), DISABLED);
    conn.rollback().unwrap();
}

/// Discovery agrees with callability: no `db.ontology.*` row is listed in
/// any composed form, while the rest of the catalog is untouched.
#[test]
fn show_procedures_omits_the_ontology_family() {
    let (_db, mut conn) = open_closed();
    // Plain, WHERE and UNION-composed discovery all agree with
    // callability: no `db.ontology.*` row is visible.
    for cypher in [
        "SHOW PROCEDURES YIELD name WHERE name STARTS WITH 'db.ontology.' RETURN name",
        "SHOW PROCEDURES YIELD name WHERE name STARTS WITH 'db.ontology.' RETURN name \
         UNION SHOW PROCEDURES YIELD name WHERE name STARTS WITH 'db.ontology.' RETURN name",
    ] {
        let result = conn.query(cypher).unwrap();
        assert!(result.rows.is_empty(), "{cypher}: {:?}", result.rows);
    }
    // The aggregate form still answers, with the empty set's count.
    let result = conn
        .query(
            "SHOW PROCEDURES YIELD name WHERE name STARTS WITH 'db.ontology.' RETURN count(*) AS c",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(0)]]);
    // An `EXISTS` subquery sees the same filtered catalog.
    let result = conn
        .query(
            "RETURN EXISTS { SHOW PROCEDURES YIELD name \
             WHERE name STARTS WITH 'db.ontology.' } AS e",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Bool(false)]]);
    // Hiding the gated family leaves every other row listed.
    let result = conn
        .query("SHOW PROCEDURES YIELD name WHERE name = 'db.labels' RETURN count(*) AS c")
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

/// Persistence is unaffected (ADR-0378): the hidden facade writes and
/// publishes a document, and a store that holds one opens, checkpoints and
/// reloads with the gate off — the bytes survive unchanged.
#[test]
fn stored_documents_survive_reopen_with_the_gate_closed() {
    assert_preview_unset();
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let db = Database::open(uri).unwrap();
    db.commit_ontology_set(ONTOLOGY_DOC.to_string()).unwrap();
    db.checkpoint().unwrap();
    drop(db);

    let db = Database::open(uri).expect("a store holding an ontology opens with the gate off");
    assert_eq!(
        db.ontology_document().unwrap().as_deref(),
        Some(ONTOLOGY_DOC)
    );
    drop(db);

    let db = Database::open(uri).expect("the second reopen still opens with the gate off");
    assert_eq!(
        db.ontology_document().unwrap().as_deref(),
        Some(ONTOLOGY_DOC),
        "the document survives the second reopen byte-for-byte"
    );
}
