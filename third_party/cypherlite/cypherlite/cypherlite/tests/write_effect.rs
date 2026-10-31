//! ADR-0402 Decision 2: the engine publishes its truthful write effect on the
//! public `QueryResult`, and the note criterion (`changed_nothing`) reads it.
//! The engine facts, not the plan's conservative query type and not the
//! mutation counters, decide — so a catalog change (which moves no counter) and
//! a read the planner classifies as a write (which ran no mutation operator)
//! both come out right.

use cypherlite::Database;

/// A read: no mutation operator, nothing applied.
#[test]
fn a_read_has_no_write_effect() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn.query("MATCH (n:Missing) RETURN n").unwrap();
    assert!(!result.effect.attempted());
    assert!(!result.effect.applied());
    assert!(!result.effect.changed_nothing());
}

/// The LABS-313 shape: a write whose pipeline was empty attempted and applied
/// nothing, with every mutation counter at zero.
#[test]
fn a_short_circuited_write_changed_nothing() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn
        .query("MATCH (m:Missing) DELETE m CREATE (n:Label)")
        .unwrap();
    assert!(result.effect.attempted());
    assert!(!result.effect.applied());
    assert!(result.effect.changed_nothing());
    assert_eq!(result.stats, cypherlite::QueryStats::default());
}

/// `ALTER DATABASE SET DEFAULT LANGUAGE` persists a catalog change and leaves
/// every mutation counter at zero: the effect's applied fact comes from the
/// write redo, so it is a write that changed something.
#[test]
fn a_catalog_change_is_a_write_that_applied() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn
        .query("ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5")
        .unwrap();
    assert_eq!(result.stats, cypherlite::QueryStats::default());
    assert!(result.effect.attempted());
    assert!(result.effect.applied());
    assert!(!result.effect.changed_nothing());
}

/// A read-only statement the planner conservatively routes as a write ran no
/// mutation operator, so it is a read to the note policy.
#[test]
fn a_read_only_call_subquery_is_a_read_to_the_policy() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn.query("CALL { MATCH (n) RETURN n } RETURN n").unwrap();
    assert!(
        matches!(
            result.header.query_type,
            cypherlite::QueryType::Write | cypherlite::QueryType::ReadWrite
        ),
        "the planner classifies a top-level CALL subquery conservatively"
    );
    assert!(!result.effect.attempted());
    assert!(!result.effect.changed_nothing());
}

/// `EXPLAIN` does not execute, so it stamps no effect at all.
#[test]
fn an_explain_result_has_no_write_effect() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn.query("EXPLAIN CREATE (:A)").unwrap();
    assert_eq!(result.effect, cypherlite::StatementEffect::None);
    assert!(!result.effect.changed_nothing());
}

/// A transaction accumulates the engine effects of its statements: one applied
/// statement silences the merged criterion, while a transaction of reads and
/// no-ops reports.
#[test]
fn a_transaction_merges_statement_effects() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();

    conn.begin().unwrap();
    let noop = conn.query("MATCH (m:Missing) DELETE m").unwrap();
    conn.commit().unwrap();
    assert!(noop.effect.changed_nothing());

    conn.begin().unwrap();
    conn.query("CREATE (:Kept)").unwrap();
    let noop = conn.query("MATCH (m:Missing) DELETE m").unwrap();
    conn.commit().unwrap();
    assert!(
        noop.effect.changed_nothing(),
        "the no-op statement itself reports"
    );

    let mut ledger = cypherlite::StatementEffect::None;
    ledger.merge(cypherlite::StatementEffect::Applied);
    ledger.merge(noop.effect);
    assert!(
        !ledger.changed_nothing(),
        "an applied statement silences the merged transaction"
    );
}
