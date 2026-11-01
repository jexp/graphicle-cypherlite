//! The vector search's candidate enumeration contract (LABS-442): the
//! scan rides the exact live per-label enumeration (base postings at the
//! pinned snapshot plus scratch entries by their actual labels), so
//! in-statement label churn and deletions behave exactly like the
//! all-ids sweep did. These pin the four directions; the label-add case
//! is the one a postings-plus-creations enumeration silently dropped (a
//! copy-on-write relabel is absent from both the postings and
//! `created_nodes`).

use cypherlite::{Database, Value};

fn store() -> cypherlite::Connection {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query(
        "CREATE VECTOR INDEX movieEmbeddings IF NOT EXISTS \
         FOR (movie:Movie) ON movie.embedding \
         OPTIONS {indexConfig: {`vector.dimensions`: 3, \
         `vector.similarity_function`: 'cosine'}}",
    )
    .unwrap();
    // One committed Movie (the control) and one committed Other that
    // carries the embedding but not the indexed label.
    c.query(
        "CREATE (:Movie {id: 'committed', embedding: [1.0, 0.0, 0.0]}), \
         (:Other {id: 'relabel-me', embedding: [1.0, 0.1, 0.0]})",
    )
    .unwrap();
    c
}

fn query_ids(c: &mut cypherlite::Connection, statement: &str) -> Vec<String> {
    let result = c.query(statement).unwrap();
    result
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Node(node) => match node.properties.get("id") {
                Some(Value::String(id)) => id.to_string(),
                other => panic!("the node carries a string id, got {other:?}"),
            },
            other => panic!("a queryNodes row yields a node, got {other:?}"),
        })
        .collect()
}

/// A pre-existing node that GAINS the indexed label in-statement
/// (`SET n:Label` — a copy-on-write entry, never in `created_nodes`,
/// and the postings predate the SET) is found by the search in the
/// same statement, exactly like the all-ids sweep found it.
#[test]
fn label_added_in_statement_is_searched() {
    let mut c = store();
    let ids = query_ids(
        &mut c,
        "MATCH (o:Other {id: 'relabel-me'}) SET o:Movie \
         WITH o \
         CALL db.index.vector.queryNodes('movieEmbeddings', 5, [1.0, 0.0, 0.0]) \
         YIELD node RETURN node ORDER BY node.id",
    );
    assert!(
        ids.contains(&"committed".to_string()),
        "the control: {ids:?}"
    );
    assert!(
        ids.contains(&"relabel-me".to_string()),
        "the relabeled node is enumerated (the copy-on-write churn case): {ids:?}"
    );
}

/// A node that LOSES the indexed label in-statement is not searched —
/// with a surviving control so a total-regression empty result cannot
/// pass the negative assert.
#[test]
fn label_removed_in_statement_is_not_searched() {
    let mut c = store();
    c.query("CREATE (:Movie {id: 'stay', embedding: [0.0, 1.0, 0.0]})")
        .unwrap();
    let ids = query_ids(
        &mut c,
        "MATCH (m:Movie {id: 'committed'}) REMOVE m:Movie \
         WITH m \
         CALL db.index.vector.queryNodes('movieEmbeddings', 5, [1.0, 0.0, 0.0]) \
         YIELD node RETURN node ORDER BY node.id",
    );
    assert!(
        !ids.contains(&"committed".to_string()),
        "the label-removed node must not appear: {ids:?}"
    );
    assert_eq!(
        ids,
        vec!["stay".to_string()],
        "the control survives the same statement: {ids:?}"
    );
}

/// A node deleted in-statement is not searched — asserted on the
/// in-statement view (the same statement's search), then the committed
/// view.
#[test]
fn deleted_in_statement_is_not_searched() {
    let mut c = store();
    let in_statement = query_ids(
        &mut c,
        "MATCH (o:Other {id: 'relabel-me'}) SET o:Movie \
         WITH o \
         DELETE o \
         WITH 1 AS next \
         CALL db.index.vector.queryNodes('movieEmbeddings', 5, [1.0, 0.0, 0.0]) \
         YIELD node RETURN node ORDER BY node.id",
    );
    // The namesake behavior: the search in the deleting statement
    // itself must not return the deleted node — and must keep the
    // control, so a total-regression empty result cannot pass.
    assert!(
        !in_statement.contains(&"relabel-me".to_string()),
        "the deleted node must not appear in-statement: {in_statement:?}"
    );
    assert!(
        in_statement.contains(&"committed".to_string()),
        "the control survives the same statement: {in_statement:?}"
    );
    let committed = query_ids(
        &mut c,
        "CALL db.index.vector.queryNodes('movieEmbeddings', 5, [1.0, 0.0, 0.0]) \
         YIELD node RETURN node ORDER BY node.id",
    );
    assert_eq!(committed, vec!["committed".to_string()]);
}

/// A node created in-statement (with the indexed label) is searched in
/// the same statement.
#[test]
fn created_in_statement_is_searched() {
    let mut c = store();
    let ids = query_ids(
        &mut c,
        "CREATE (:Movie {id: 'fresh', embedding: [0.99, 0.05, 0.0]}) \
         WITH 1 AS next \
         CALL db.index.vector.queryNodes('movieEmbeddings', 5, [1.0, 0.0, 0.0]) \
         YIELD node RETURN node ORDER BY node.id",
    );
    assert!(ids.contains(&"fresh".to_string()), "the creation: {ids:?}");
    assert!(
        ids.contains(&"committed".to_string()),
        "the control: {ids:?}"
    );
}
