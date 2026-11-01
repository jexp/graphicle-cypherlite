//! ADR-0174: a constraint another session commits mid-transaction is
//! enforced at the open transaction's next statement.

use cypherlite::Value;

use crate::common::{err_text, scalar};
use crate::support::UNIQUE;

/// ADR-0174 protocol: an open transaction whose `earlier` writes violate a
/// constraint another session then commits sees the constraint enforced at
/// its next statement, which fails with the `expected` fragment; the failed
/// transaction cannot commit, and none of its writes survive the rollback.
fn committed_by_another_session_fails_the_next_statement(earlier: &str, ddl: &str, expected: &str) {
    let db = cypherlite::Database::open(":memory:").unwrap();
    let mut transaction = db.connect().unwrap();
    let mut other = db.connect().unwrap();
    transaction.begin().unwrap();
    transaction.query(earlier).unwrap();
    other.query(ddl).unwrap();
    let error = err_text(transaction.query("CREATE (:Other)"));
    assert!(
        error.contains("ConstraintValidationFailed") && error.contains(expected),
        "expected {expected}, got {error}"
    );
    assert!(
        transaction.commit().is_err(),
        "a failed transaction cannot commit"
    );
    transaction.rollback().unwrap();
    assert_eq!(
        scalar(&mut other, "MATCH (n:P) RETURN count(n)"),
        Value::Int(0)
    );
}

/// ADR-0174: a constraint another session commits after a transaction began
/// is enforced on that transaction at its next statement, against its
/// earlier writes — the full scan's mid-transaction behavior, restored.
#[test]
fn a_constraint_committed_by_another_session_applies_at_the_next_statement() {
    committed_by_another_session_fails_the_next_statement(
        "CREATE (:P {id: 1}), (:P {id: 1})",
        UNIQUE,
        "`p_id` uniqueness",
    );
}

/// ADR-0174: the same for a property-existence constraint.
#[test]
fn an_existence_constraint_committed_by_another_session_applies_at_the_next_statement() {
    committed_by_another_session_fails_the_next_statement(
        "CREATE (:P)",
        "CREATE CONSTRAINT p_name FOR (n:P) REQUIRE n.name IS NOT NULL",
        "`p_name` requires properties",
    );
}

/// ADR-0174: a constraint another session commits is satisfied by the open
/// transaction's earlier writes, and its next statement proceeds.
#[test]
fn a_constraint_committed_by_another_session_that_the_writes_satisfy_passes() {
    let db = cypherlite::Database::open(":memory:").unwrap();
    let mut transaction = db.connect().unwrap();
    let mut other = db.connect().unwrap();
    transaction.begin().unwrap();
    transaction
        .query("CREATE (:P {id: 1}), (:P {id: 2})")
        .unwrap();
    other.query(UNIQUE).unwrap();
    transaction.query("CREATE (:Other)").unwrap();
    transaction.commit().unwrap();
    assert_eq!(
        scalar(&mut other, "MATCH (n) RETURN count(n)"),
        Value::Int(3)
    );
}
