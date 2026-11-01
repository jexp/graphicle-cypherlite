//! Duplicates across statements of one explicit transaction and across
//! `IN TRANSACTIONS` batches (ADR-0210), and the positive controls.

use cypherlite::Value;

use crate::common::{err_text, scalar, uniqueness};
use crate::support::{dump, setup, PERSON_EMAIL};

// Duplicates that neither side has committed: two creates of one statement,
// and two `IN TRANSACTIONS` batches under the default `ON ERROR FAIL`. The
// explicit-transaction case needs `begin` and two statements, and `ON ERROR
// CONTINUE` succeeds with per-batch status rows, so both are written out
// below.
violations! {
    duplicate_created_twice_by_one_statement_fails: [PERSON_EMAIL]
        => "UNWIND [1, 1] AS i CREATE (:Person {email: 'dup'})", uniqueness("person_email");
}

// ADR-0210: under `IN TRANSACTIONS` each batch is a real commit, so the
// default `ON ERROR FAIL` still raises the violation from the failing
// batch — but the earlier batch's committed write stays (Neo4j inner
// transaction semantics), unlike a plain statement's full rollback.
#[test]
fn duplicate_created_inside_call_in_transactions_fails() {
    let mut c = setup(&[PERSON_EMAIL]);
    let error = c
        .query(
            "UNWIND [1, 2] AS i CALL { WITH i CREATE (:Person {email: 'dup3'}) } \
             IN TRANSACTIONS OF 1 ROWS",
        )
        .expect_err("second batch must violate the uniqueness constraint")
        .to_string();
    assert_eq!(error, uniqueness("person_email"));
    assert_eq!(
        scalar(&mut c, "MATCH (n:Person {email: 'dup3'}) RETURN count(n)"),
        Value::Int(1),
        "the first batch's committed create survives the second batch's failure"
    );
}

#[test]
fn duplicate_across_statements_of_one_explicit_transaction_fails() {
    let mut c = setup(&[PERSON_EMAIL]);
    let before = dump(&mut c);
    c.begin().unwrap();
    c.query("CREATE (:Person {email: 'dup2'})").unwrap();
    assert_eq!(
        err_text(c.query("CREATE (:Person {email: 'dup2'})")),
        uniqueness("person_email")
    );
    assert!(c.commit().is_err(), "a failed transaction cannot commit");
    c.rollback().unwrap();
    assert_eq!(dump(&mut c), before);
}

#[test]
fn duplicate_across_in_transactions_batches_continue_rolls_back_the_failed_batches() {
    let mut c = setup(&[PERSON_EMAIL]);
    let rows = c
        .query(
            "UNWIND [1, 2, 3] AS i CALL { WITH i CREATE (:Person {email: 'dup4'}) } \
             IN TRANSACTIONS OF 1 ROWS ON ERROR CONTINUE REPORT STATUS AS s \
             RETURN i, s.committed AS committed ORDER BY i",
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Bool(true)],
            vec![Value::Int(2), Value::Bool(false)],
            vec![Value::Int(3), Value::Bool(false)],
        ]
    );
    assert_eq!(
        scalar(&mut c, "MATCH (n:Person {email: 'dup4'}) RETURN count(n)"),
        Value::Int(1)
    );
}

#[test]
fn a_failed_batch_is_rolled_back_before_the_next_batch_validates() {
    // Batch 1 creates 'z' and a duplicate of the committed 'x', so it fails
    // and is rolled back, its 'z' with it; batch 2's 'z' is then unique.
    let mut c = setup(&[PERSON_EMAIL, "CREATE (:Person {email: 'x'})"]);
    let rows = c
        .query(
            "UNWIND [1, 2] AS i CALL { WITH i \
               CREATE (:Person {email: 'z'}) \
               FOREACH (_ IN CASE WHEN i = 1 THEN [1] ELSE [] END | CREATE (:Person {email: 'x'})) \
             } IN TRANSACTIONS OF 1 ROWS ON ERROR CONTINUE REPORT STATUS AS s \
             RETURN i, s.committed AS committed ORDER BY i",
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Bool(false)],
            vec![Value::Int(2), Value::Bool(true)],
        ]
    );
    assert_eq!(
        scalar(&mut c, "MATCH (n:Person) RETURN count(n)"),
        Value::Int(2)
    );
}

#[test]
fn setting_a_value_to_itself_succeeds() {
    let mut c = setup(&[
        PERSON_EMAIL,
        "CREATE (:Person {email: 'a@x'}), (:Person {email: 'b@x'})",
    ]);
    c.query("MATCH (n:Person {email: 'a@x'}) SET n.email = n.email")
        .unwrap();
    c.query("MATCH (n:Person) SET n.email = n.email").unwrap();
    c.query("MATCH (n:Person) SET n += {email: n.email}")
        .unwrap();
    assert_eq!(
        scalar(&mut c, "MATCH (n:Person) RETURN count(n)"),
        Value::Int(2)
    );
}

#[test]
fn positive_controls() {
    let mut c = setup(&[
        PERSON_EMAIL,
        "CREATE (:Person {email: 'a'}), (:Person {email: 'b'})",
    ]);
    // Delete then recreate the same value in one statement.
    c.query(
        "MATCH (n:Person {email: 'a'}) DELETE n WITH count(*) AS c CREATE (:Person {email: 'a'})",
    )
    .unwrap();
    // Move a value away, then create it, in one statement.
    c.query(
        "MATCH (n:Person {email: 'a'}) SET n.email = 'c' WITH count(*) AS c \
         CREATE (:Person {email: 'a'})",
    )
    .unwrap();
    // Swap two values in one statement.
    c.query(
        "MATCH (x:Person {email: 'a'}), (y:Person {email: 'b'}) SET x.email = 'b', y.email = 'a'",
    )
    .unwrap();
    // MERGE on an existing value matches instead of creating.
    c.query("MERGE (:Person {email: 'a'})").unwrap();
    // Unrelated labels and properties are not constrained.
    c.query("CREATE (:Other {email: 'a'}), (:Person {name: 'no email'})")
        .unwrap();
    // A value moved away by one statement of a transaction can be reused by
    // a later one.
    c.begin().unwrap();
    c.query("MATCH (n:Person {email: 'c'}) SET n.email = 'd'")
        .unwrap();
    c.query("CREATE (:Person {email: 'c'})").unwrap();
    c.commit().unwrap();
    assert_eq!(
        scalar(
            &mut c,
            "MATCH (n:Person) WHERE n.email IS NOT NULL RETURN count(DISTINCT n.email)"
        ),
        scalar(
            &mut c,
            "MATCH (n:Person) WHERE n.email IS NOT NULL RETURN count(n)"
        )
    );
}
