//! Constraint validation through the public API (ADR-0172): one violation
//! per constraint kind and trigger operation of the ADR's §2 table, each
//! asserting today's exact error text and an unchanged graph; duplicates
//! across statements of a transaction and across `IN TRANSACTIONS`
//! batches; positive controls; the deliberate pre-existing-violation
//! change, pinned; uniqueness probes beside concurrent commits; and
//! constraints another session commits mid-transaction, enforced at the
//! open transaction's next statement (ADR-0174).

mod common;

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cypherlite::{Connection, Value};

use common::{connection, err_text, property_type, requires, scalar, uniqueness};

fn node_label(label: &str, enforced: &str) -> String {
    format!("ConstraintValidationFailed: node with label {label} must also have label {enforced}")
}

fn endpoint(enforced: &str) -> String {
    format!(
        "ConstraintValidationFailed: endpoint label presence verification failed: expected {enforced}"
    )
}

/// Every node, relationship and constraint, in a stable order.
type Dump = (Vec<Vec<Value>>, Vec<Vec<Value>>, Vec<Vec<Value>>);

fn dump(c: &mut Connection) -> Dump {
    let mut rows = |q: &str| c.query(q).unwrap().rows;
    (
        rows(
            "MATCH (n) RETURN id(n) AS id, labels(n) AS labels, properties(n) AS props ORDER BY id",
        ),
        rows(
            "MATCH (a)-[r]->(b) \
             RETURN id(r) AS id, type(r) AS type, id(a) AS a, id(b) AS b, properties(r) AS props \
             ORDER BY id",
        ),
        rows("SHOW CONSTRAINTS YIELD name RETURN name ORDER BY name"),
    )
}

fn setup(statements: &[&str]) -> Connection {
    let mut c = connection();
    for statement in statements {
        c.query(statement)
            .unwrap_or_else(|e| panic!("setup `{statement}` failed: {e}"));
    }
    c
}

fn assert_violation(statements: &[&str], violation: &str, expected: &str) {
    let mut c = setup(statements);
    let before = dump(&mut c);
    let error = c
        .query(violation)
        .err()
        .unwrap_or_else(|| panic!("`{violation}` should fail with {expected}"))
        .to_string();
    assert_eq!(error, expected, "`{violation}`");
    assert_eq!(
        dump(&mut c),
        before,
        "`{violation}` must leave the graph unchanged"
    );
}

/// One `#[test]` per constraint kind and trigger operation: the setup
/// statements, the violating statement, and today's exact error.
macro_rules! violations {
    ($($name:ident: [$($setup:expr),* $(,)?] => $violation:expr, $expected:expr;)*) => {
        $(
            #[test]
            fn $name() {
                assert_violation(&[$($setup),*], $violation, &$expected);
            }
        )*
    };
}

const UNIQUE: &str = "CREATE CONSTRAINT p_id FOR (n:P) REQUIRE n.id IS UNIQUE";
const PERSON_EMAIL: &str =
    "CREATE CONSTRAINT person_email FOR (n:Person) REQUIRE n.email IS UNIQUE";
const REL_UNIQUE: &str = "CREATE CONSTRAINT r_id FOR ()-[r:R]-() REQUIRE r.id IS UNIQUE";
const KEY: &str = "CREATE CONSTRAINT p_key FOR (n:P) REQUIRE n.k IS NODE KEY";
const REL_KEY: &str = "CREATE CONSTRAINT r_key FOR ()-[r:R]-() REQUIRE r.k IS RELATIONSHIP KEY";
const EXISTS: &str = "CREATE CONSTRAINT p_name FOR (n:P) REQUIRE n.name IS NOT NULL";
const PERSON_NAME: &str = "CREATE CONSTRAINT person_name FOR (n:Person) REQUIRE n.name IS NOT NULL";
const REL_EXISTS: &str = "CREATE CONSTRAINT r_w FOR ()-[r:R]-() REQUIRE r.w IS NOT NULL";
const TYPED: &str = "CREATE CONSTRAINT p_age FOR (n:P) REQUIRE n.age IS :: INTEGER";
const REL_TYPED: &str = "CREATE CONSTRAINT r_t FOR ()-[r:R]-() REQUIRE r.t IS :: INTEGER";
/// `SET e.p = null` and `REMOVE e.p` break a property type only when it is
/// `NOT NULL`; the graph type is where such a type is declared.
const TYPED_NOT_NULL: &str = "ALTER CURRENT GRAPH TYPE SET { (:P => {age :: INTEGER NOT NULL}) }";
const REL_TYPED_NOT_NULL: &str =
    "ALTER CURRENT GRAPH TYPE SET { ()-[:R => {t :: INTEGER NOT NULL}]->() }";
const LABEL_EXISTENCE: &str = "ALTER CURRENT GRAPH TYPE SET { (:P => :Q) }";
const NODE_LABEL_EXISTENCE: &str = "CREATE CONSTRAINT p_q FOR (n:P) REQUIRE n:Q";
const SOURCE_LABEL: &str =
    "ALTER CURRENT GRAPH TYPE SET { (:A => {name :: STRING}), (:A)-[:R =>]->() }";
const SOURCE_LABEL_EXISTENCE: &str = "CREATE CONSTRAINT r_src FOR (s)-[r:R]->(t) REQUIRE s:A";
const TARGET_LABEL: &str =
    "ALTER CURRENT GRAPH TYPE SET { (:B => {name :: STRING}), ()-[:R =>]->(:B) }";
const TARGET_LABEL_EXISTENCE: &str = "CREATE CONSTRAINT r_tgt FOR (s)-[r:R]->(t) REQUIRE t:B";

// `unique`, node form.
violations! {
    unique_create_node: [PERSON_EMAIL, "CREATE (:Person {email: 'a@x'})"]
        => "CREATE (:Person {email: 'a@x'})", uniqueness("person_email");
    unique_merge_create_branch: [UNIQUE, "CREATE (:P {id: 1})"]
        => "MERGE (:P {id: 1, tag: 'new'})", uniqueness("p_id");
    unique_set_property:
        [PERSON_EMAIL, "CREATE (:Person {email: 'a@x'}), (:Person {email: 'b@x'})"]
        => "MATCH (n:Person {email: 'b@x'}) SET n.email = 'a@x'", uniqueness("person_email");
    unique_set_map: [UNIQUE, "CREATE (:P {id: 1}), (:P {id: 2})"]
        => "MATCH (n:P {id: 2}) SET n = {id: 1}", uniqueness("p_id");
    unique_set_map_plus: [UNIQUE, "CREATE (:P {id: 1}), (:P {id: 2})"]
        => "MATCH (n:P {id: 2}) SET n += {id: 1}", uniqueness("p_id");
    unique_set_label: [PERSON_EMAIL, "CREATE (:Person {email: 'a@x'}), ({email: 'a@x'})"]
        => "MATCH (n) WHERE NOT n:Person SET n:Person", uniqueness("person_email");
    unique_set_dynamic_label: [UNIQUE, "CREATE (:P {id: 1}), (:X {id: 1})"]
        => "MATCH (n:X) SET n:$('P')", uniqueness("p_id");
    unique_on_create_set: [UNIQUE, "CREATE (:P {id: 1})"]
        => "MERGE (n:P {id: 3}) ON CREATE SET n.id = 1", uniqueness("p_id");
    unique_on_match_set: [UNIQUE, "CREATE (:P {id: 1}), (:P {id: 2})"]
        => "MERGE (n:P {id: 2}) ON MATCH SET n.id = 1", uniqueness("p_id");
    unique_recheck_create_constraint: ["CREATE (:P {id: 1}), (:P {id: 1.0})"]
        => UNIQUE, uniqueness("p_id");
    unique_recheck_alter_graph_type: ["CREATE (:P {id: 1}), (:P {id: 1})"]
        => "ALTER CURRENT GRAPH TYPE SET { CONSTRAINT p_id FOR (n:P) REQUIRE n.id IS UNIQUE }",
           uniqueness("p_id");
}

// `unique`, relationship form. `SET n:L` has no relationship form in any
// row: a relationship's type is fixed at creation, so only `CREATE
// relationship` and the property writes bring one under a constraint.
violations! {
    unique_create_relationship: [REL_UNIQUE, "CREATE ()-[:R {id: 1}]->()"]
        => "CREATE ()-[:R {id: 1}]->()", uniqueness("r_id");
    unique_rel_merge_create_branch: [REL_UNIQUE, "CREATE ()-[:R {id: 1}]->()"]
        => "MERGE (:A)-[:R {id: 1}]->(:B)", uniqueness("r_id");
    unique_rel_set_property: [REL_UNIQUE, "CREATE ()-[:R {id: 1}]->(), ()-[:R {id: 2}]->()"]
        => "MATCH ()-[r:R {id: 2}]->() SET r.id = 1", uniqueness("r_id");
    unique_rel_set_map: [REL_UNIQUE, "CREATE ()-[:R {id: 1}]->(), ()-[:R {id: 2}]->()"]
        => "MATCH ()-[r:R {id: 2}]->() SET r = {id: 1}", uniqueness("r_id");
    unique_rel_set_map_plus: [REL_UNIQUE, "CREATE ()-[:R {id: 1}]->(), ()-[:R {id: 2}]->()"]
        => "MATCH ()-[r:R {id: 2}]->() SET r += {id: 1}", uniqueness("r_id");
    unique_rel_on_create_set: [REL_UNIQUE, "CREATE ()-[:R {id: 1}]->()"]
        => "MERGE (:A)-[r:R {id: 3}]->(:B) ON CREATE SET r.id = 1", uniqueness("r_id");
    unique_rel_on_match_set: [REL_UNIQUE, "CREATE ()-[:R {id: 1}]->(), (:A)-[:R {id: 2}]->(:B)"]
        => "MATCH (a:A), (b:B) MERGE (a)-[r:R {id: 2}]->(b) ON MATCH SET r.id = 1",
           uniqueness("r_id");
    unique_rel_recheck_create_constraint: ["CREATE ()-[:R {id: 1}]->(), ()-[:R {id: 1}]->()"]
        => REL_UNIQUE, uniqueness("r_id");
    unique_rel_recheck_alter_graph_type: ["CREATE ()-[:R {id: 1}]->(), ()-[:R {id: 1}]->()"]
        => "ALTER CURRENT GRAPH TYPE SET { CONSTRAINT r_id FOR ()-[r:R]->() REQUIRE r.id IS UNIQUE }",
           uniqueness("r_id");
}

// `key`, node form.
violations! {
    key_create_node_missing: [KEY]
        => "CREATE (:P)", requires("p_key");
    key_create_node_duplicate: [KEY, "CREATE (:P {k: 1})"]
        => "CREATE (:P {k: 1})", uniqueness("p_key");
    key_merge_create_branch: [KEY, "CREATE (:P {k: 1})"]
        => "MERGE (:P {k: 1, tag: 'new'})", uniqueness("p_key");
    key_set_property: [KEY, "CREATE (:P {k: 1}), (:P {k: 2})"]
        => "MATCH (n:P {k: 2}) SET n.k = 1", uniqueness("p_key");
    key_set_property_null: [KEY, "CREATE (:P {k: 1})"]
        => "MATCH (n:P) SET n.k = null", requires("p_key");
    key_remove_property: [KEY, "CREATE (:P {k: 1})"]
        => "MATCH (n:P) REMOVE n.k", requires("p_key");
    key_set_map: [KEY, "CREATE (:P {k: 1})"]
        => "MATCH (n:P) SET n = {other: 1}", requires("p_key");
    key_set_map_plus: [KEY, "CREATE (:P {k: 1}), (:P {k: 2})"]
        => "MATCH (n:P {k: 2}) SET n += {k: 1}", uniqueness("p_key");
    key_set_label: [KEY, "CREATE (:X {other: 1})"]
        => "MATCH (n:X) SET n:P", requires("p_key");
    key_on_create_set: [KEY]
        => "MERGE (n:P {k: 1}) ON CREATE SET n.k = null", requires("p_key");
    key_on_match_set: [KEY, "CREATE (:P {k: 1})"]
        => "MERGE (n:P {k: 1}) ON MATCH SET n.k = null", requires("p_key");
    key_recheck_create_constraint: ["CREATE (:P {k: 1}), (:P)"]
        => KEY, requires("p_key");
    key_recheck_alter_graph_type: ["CREATE (:P {k: 1}), (:P {k: 1})"]
        => "ALTER CURRENT GRAPH TYPE SET { CONSTRAINT p_key FOR (n:P) REQUIRE n.k IS KEY }",
           uniqueness("p_key");
}

// `key`, relationship form.
violations! {
    key_create_relationship_missing: [REL_KEY]
        => "CREATE ()-[:R]->()", requires("r_key");
    key_create_relationship_duplicate: [REL_KEY, "CREATE ()-[:R {k: 1}]->()"]
        => "CREATE ()-[:R {k: 1}]->()", uniqueness("r_key");
    key_rel_merge_create_branch: [REL_KEY, "CREATE ()-[:R {k: 1}]->()"]
        => "MERGE (:A)-[:R {k: 1}]->(:B)", uniqueness("r_key");
    key_rel_set_property: [REL_KEY, "CREATE ()-[:R {k: 1}]->(), ()-[:R {k: 2}]->()"]
        => "MATCH ()-[r:R {k: 2}]->() SET r.k = 1", uniqueness("r_key");
    key_rel_set_property_null: [REL_KEY, "CREATE ()-[:R {k: 1}]->()"]
        => "MATCH ()-[r:R]->() SET r.k = null", requires("r_key");
    key_rel_remove_property: [REL_KEY, "CREATE ()-[:R {k: 1}]->()"]
        => "MATCH ()-[r:R]->() REMOVE r.k", requires("r_key");
    key_rel_set_map: [REL_KEY, "CREATE ()-[:R {k: 1}]->()"]
        => "MATCH ()-[r:R]->() SET r = {other: 1}", requires("r_key");
    key_rel_set_map_plus: [REL_KEY, "CREATE ()-[:R {k: 1}]->(), ()-[:R {k: 2}]->()"]
        => "MATCH ()-[r:R {k: 2}]->() SET r += {k: 1}", uniqueness("r_key");
    key_rel_on_create_set: [REL_KEY]
        => "MERGE (:A)-[r:R {k: 1}]->(:B) ON CREATE SET r.k = null", requires("r_key");
    key_rel_on_match_set: [REL_KEY, "CREATE (:A)-[:R {k: 1}]->(:B)"]
        => "MATCH (a:A), (b:B) MERGE (a)-[r:R {k: 1}]->(b) ON MATCH SET r.k = null",
           requires("r_key");
    key_rel_recheck_create_constraint: ["CREATE ()-[:R {k: 1}]->(), ()-[:R]->()"]
        => REL_KEY, requires("r_key");
    key_rel_recheck_alter_graph_type: ["CREATE ()-[:R {k: 1}]->(), ()-[:R {k: 1}]->()"]
        => "ALTER CURRENT GRAPH TYPE SET { CONSTRAINT r_key FOR ()-[r:R]->() REQUIRE r.k IS KEY }",
           uniqueness("r_key");
}

// `exists`, node form.
violations! {
    exists_create_node: [EXISTS]
        => "CREATE (:P)", requires("p_name");
    exists_merge_create_branch: [EXISTS]
        => "MERGE (:P {other: 1})", requires("p_name");
    exists_set_property_null: [EXISTS, "CREATE (:P {name: 'a'})"]
        => "MATCH (n:P) SET n.name = null", requires("p_name");
    exists_remove_property: [PERSON_NAME, "CREATE (:Person {name: 'Ada'})"]
        => "MATCH (n:Person) REMOVE n.name", requires("person_name");
    exists_set_map: [EXISTS, "CREATE (:P {name: 'a'})"]
        => "MATCH (n:P) SET n = {other: 1}", requires("p_name");
    exists_set_map_plus: [EXISTS, "CREATE (:P {name: 'a'})"]
        => "MATCH (n:P) SET n += {name: null}", requires("p_name");
    exists_set_label: [EXISTS, "CREATE (:X)"]
        => "MATCH (n:X) SET n:P", requires("p_name");
    exists_on_create_set: [EXISTS]
        => "MERGE (n:P {name: 'a'}) ON CREATE SET n.name = null", requires("p_name");
    exists_on_match_set: [EXISTS, "CREATE (:P {name: 'a'})"]
        => "MERGE (n:P {name: 'a'}) ON MATCH SET n.name = null", requires("p_name");
    exists_recheck_create_constraint: ["CREATE (:P)"]
        => EXISTS, requires("p_name");
    exists_recheck_alter_graph_type: ["CREATE (:P)"]
        => "ALTER CURRENT GRAPH TYPE SET { CONSTRAINT p_name FOR (n:P) REQUIRE n.name IS NOT NULL }",
           requires("p_name");
}

// `exists`, relationship form.
violations! {
    exists_create_relationship: [REL_EXISTS]
        => "CREATE ()-[:R]->()", requires("r_w");
    exists_rel_merge_create_branch: [REL_EXISTS]
        => "MERGE (:A)-[:R {other: 1}]->(:B)", requires("r_w");
    exists_rel_set_property_null: [REL_EXISTS, "CREATE ()-[:R {w: 1}]->()"]
        => "MATCH ()-[r:R]->() SET r.w = null", requires("r_w");
    exists_rel_remove_property: [REL_EXISTS, "CREATE ()-[:R {w: 1}]->()"]
        => "MATCH ()-[r:R]->() REMOVE r.w", requires("r_w");
    exists_rel_set_map: [REL_EXISTS, "CREATE ()-[:R {w: 1}]->()"]
        => "MATCH ()-[r:R]->() SET r = {other: 1}", requires("r_w");
    exists_rel_set_map_plus: [REL_EXISTS, "CREATE ()-[:R {w: 1}]->()"]
        => "MATCH ()-[r:R]->() SET r += {w: null}", requires("r_w");
    exists_rel_on_create_set: [REL_EXISTS]
        => "MERGE (:A)-[r:R {w: 1}]->(:B) ON CREATE SET r.w = null", requires("r_w");
    exists_rel_on_match_set: [REL_EXISTS, "CREATE (:A)-[:R {w: 1}]->(:B)"]
        => "MATCH (a:A), (b:B) MERGE (a)-[r:R {w: 1}]->(b) ON MATCH SET r.w = null",
           requires("r_w");
    exists_rel_recheck_create_constraint: ["CREATE ()-[:R]->()"]
        => REL_EXISTS, requires("r_w");
    exists_rel_recheck_alter_graph_type: ["CREATE ()-[:R]->()"]
        => "ALTER CURRENT GRAPH TYPE SET { CONSTRAINT r_w FOR ()-[r:R]->() REQUIRE r.w IS NOT NULL }",
           requires("r_w");
}

// `propertyType`, node form.
violations! {
    type_create_node: [TYPED]
        => "CREATE (:P {age: 'old'})", property_type("p_age");
    type_merge_create_branch: [TYPED]
        => "MERGE (:P {age: 'old'})", property_type("p_age");
    type_set_property: [TYPED, "CREATE (:P {age: 1})"]
        => "MATCH (n:P) SET n.age = 'old'", property_type("p_age");
    type_set_property_null: [TYPED_NOT_NULL, "CREATE (:P {age: 1})"]
        => "MATCH (n:P) SET n.age = null", property_type("constraint_propertyType_P_age");
    type_remove_property: [TYPED_NOT_NULL, "CREATE (:P {age: 1})"]
        => "MATCH (n:P) REMOVE n.age", property_type("constraint_propertyType_P_age");
    type_set_map: [TYPED, "CREATE (:P {age: 1})"]
        => "MATCH (n:P) SET n = {age: 'old'}", property_type("p_age");
    type_set_map_plus: [TYPED, "CREATE (:P {age: 1})"]
        => "MATCH (n:P) SET n += {age: 'old'}", property_type("p_age");
    type_set_label: [TYPED, "CREATE (:X {age: 'old'})"]
        => "MATCH (n:X) SET n:P", property_type("p_age");
    type_on_create_set: [TYPED]
        => "MERGE (n:P {age: 1}) ON CREATE SET n.age = 'old'", property_type("p_age");
    type_on_match_set: [TYPED, "CREATE (:P {age: 1})"]
        => "MERGE (n:P {age: 1}) ON MATCH SET n.age = 'old'", property_type("p_age");
    type_recheck_create_constraint: ["CREATE (:P {age: 'old'})"]
        => TYPED, property_type("p_age");
    type_recheck_alter_graph_type: ["CREATE (:P {age: 'old'})"]
        => "ALTER CURRENT GRAPH TYPE SET { (:P => {age :: INTEGER}) }",
           property_type("constraint_propertyType_P_age");
}

// `propertyType`, relationship form.
violations! {
    type_create_relationship: [REL_TYPED]
        => "CREATE ()-[:R {t: 'x'}]->()", property_type("r_t");
    type_rel_merge_create_branch: [REL_TYPED]
        => "MERGE (:A)-[:R {t: 'x'}]->(:B)", property_type("r_t");
    type_rel_set_property: [REL_TYPED, "CREATE ()-[:R {t: 1}]->()"]
        => "MATCH ()-[r:R]->() SET r.t = 'x'", property_type("r_t");
    type_rel_set_property_null: [REL_TYPED_NOT_NULL, "CREATE ()-[:R {t: 1}]->()"]
        => "MATCH ()-[r:R]->() SET r.t = null", property_type("constraint_propertyType_R_t");
    type_rel_remove_property: [REL_TYPED_NOT_NULL, "CREATE ()-[:R {t: 1}]->()"]
        => "MATCH ()-[r:R]->() REMOVE r.t", property_type("constraint_propertyType_R_t");
    type_rel_set_map: [REL_TYPED, "CREATE ()-[:R {t: 1}]->()"]
        => "MATCH ()-[r:R]->() SET r = {t: 'x'}", property_type("r_t");
    type_rel_set_map_plus: [REL_TYPED, "CREATE ()-[:R {t: 1}]->()"]
        => "MATCH ()-[r:R]->() SET r += {t: 'x'}", property_type("r_t");
    type_rel_on_create_set: [REL_TYPED]
        => "MERGE (:A)-[r:R {t: 1}]->(:B) ON CREATE SET r.t = 'x'", property_type("r_t");
    type_rel_on_match_set: [REL_TYPED, "CREATE (:A)-[:R {t: 1}]->(:B)"]
        => "MATCH (a:A), (b:B) MERGE (a)-[r:R {t: 1}]->(b) ON MATCH SET r.t = 'x'",
           property_type("r_t");
    type_rel_recheck_create_constraint: ["CREATE ()-[:R {t: 'x'}]->()"]
        => REL_TYPED, property_type("r_t");
    type_rel_recheck_alter_graph_type: ["CREATE ()-[:R {t: 'x'}]->()"]
        => "ALTER CURRENT GRAPH TYPE SET { ()-[:R => {t :: INTEGER}]->() }",
           property_type("constraint_propertyType_R_t");
}

// `labelExistence` (graph type) and `nodeLabelExistence` (CREATE CONSTRAINT).
violations! {
    label_existence_create_node: [LABEL_EXISTENCE]
        => "CREATE (:P)", node_label("P", "Q");
    label_existence_merge_create_branch: [LABEL_EXISTENCE]
        => "MERGE (:P {x: 1})", node_label("P", "Q");
    label_existence_set_label: [LABEL_EXISTENCE, "CREATE (:X)"]
        => "MATCH (n:X) SET n:P", node_label("P", "Q");
    label_existence_remove_label: [LABEL_EXISTENCE, "CREATE (:P:Q)"]
        => "MATCH (n:P) REMOVE n:Q", node_label("P", "Q");
    label_existence_recheck_alter_graph_type: ["CREATE (:P)"]
        => LABEL_EXISTENCE, node_label("P", "Q");
    node_label_existence_create_node: [NODE_LABEL_EXISTENCE]
        => "CREATE (:P)", node_label("P", "Q");
    node_label_existence_merge_create_branch: [NODE_LABEL_EXISTENCE]
        => "MERGE (:P {x: 1})", node_label("P", "Q");
    node_label_existence_set_label: [NODE_LABEL_EXISTENCE, "CREATE (:X)"]
        => "MATCH (n:X) SET n:P", node_label("P", "Q");
    node_label_existence_remove_label: [NODE_LABEL_EXISTENCE, "CREATE (:P:Q)"]
        => "MATCH (n:P) REMOVE n:Q", node_label("P", "Q");
    node_label_existence_remove_dynamic_label: [NODE_LABEL_EXISTENCE, "CREATE (:P:Q)"]
        => "MATCH (n:P) REMOVE n:$('Q')", node_label("P", "Q");
    node_label_existence_recheck_create_constraint: ["CREATE (:P)"]
        => NODE_LABEL_EXISTENCE, node_label("P", "Q");
}

// `sourceLabel` (graph type) and `sourceLabelExistence` (CREATE CONSTRAINT).
violations! {
    source_label_create_relationship: [SOURCE_LABEL]
        => "CREATE (:X)-[:R]->()", endpoint("A");
    source_label_merge_create_branch: [SOURCE_LABEL]
        => "MERGE (:X)-[:R]->(:Y)", endpoint("A");
    source_label_remove_start_label: [SOURCE_LABEL, "CREATE (:A)-[:R]->()"]
        => "MATCH (n:A) REMOVE n:A", endpoint("A");
    source_label_recheck_alter_graph_type: ["CREATE (:X)-[:R]->()"]
        => SOURCE_LABEL, endpoint("A");
    source_label_existence_create_relationship: [SOURCE_LABEL_EXISTENCE]
        => "CREATE (:X)-[:R]->()", endpoint("A");
    source_label_existence_merge_create_branch: [SOURCE_LABEL_EXISTENCE]
        => "MERGE (:X)-[:R]->(:Y)", endpoint("A");
    source_label_existence_remove_start_label: [SOURCE_LABEL_EXISTENCE, "CREATE (:A)-[:R]->()"]
        => "MATCH (n:A) REMOVE n:A", endpoint("A");
    source_label_existence_recheck_create_constraint: ["CREATE (:X)-[:R]->()"]
        => SOURCE_LABEL_EXISTENCE, endpoint("A");
}

// `targetLabel` (graph type) and `targetLabelExistence` (CREATE CONSTRAINT).
violations! {
    target_label_create_relationship: [TARGET_LABEL]
        => "CREATE ()-[:R]->(:X)", endpoint("B");
    target_label_merge_create_branch: [TARGET_LABEL]
        => "MERGE (:Y)-[:R]->(:X)", endpoint("B");
    target_label_remove_end_label: [TARGET_LABEL, "CREATE ()-[:R]->(:B)"]
        => "MATCH (n:B) REMOVE n:B", endpoint("B");
    target_label_recheck_alter_graph_type: ["CREATE ()-[:R]->(:X)"]
        => TARGET_LABEL, endpoint("B");
    target_label_existence_create_relationship: [TARGET_LABEL_EXISTENCE]
        => "CREATE ()-[:R]->(:X)", endpoint("B");
    target_label_existence_merge_create_branch: [TARGET_LABEL_EXISTENCE]
        => "MERGE (:Y)-[:R]->(:X)", endpoint("B");
    target_label_existence_remove_end_label: [TARGET_LABEL_EXISTENCE, "CREATE ()-[:R]->(:B)"]
        => "MATCH (n:B) REMOVE n:B", endpoint("B");
    target_label_existence_recheck_create_constraint: ["CREATE ()-[:R]->(:X)"]
        => TARGET_LABEL_EXISTENCE, endpoint("B");
}

// Error selection: the first failure of the full scan (constraints in name
// order, ids ascending, per-entity checks before uniqueness).
violations! {
    key_reports_the_missing_property_before_the_later_duplicate:
        ["CREATE CONSTRAINT k FOR (n:K) REQUIRE n.x IS NODE KEY"]
        => "CREATE (:K {x: 5}), (:K), (:K {x: 5})", requires("k");
    first_constraint_in_name_order_wins:
        ["CREATE CONSTRAINT b_unique FOR (n:P) REQUIRE n.id IS UNIQUE",
         "CREATE CONSTRAINT a_exists FOR (n:P) REQUIRE n.name IS NOT NULL",
         "CREATE (:P {id: 1, name: 'x'})"]
        => "CREATE (:P {id: 1})", requires("a_exists");
    // The first node now duplicates the third; the second lost its key.
    // The full scan meets the second node's missing property before the
    // duplicate, which it raises at the third node.
    uniqueness_is_raised_at_the_later_id_of_a_pair:
        ["CREATE CONSTRAINT k FOR (n:K) REQUIRE n.x IS NODE KEY",
         "CREATE (:K {x: 1}), (:K {x: 2}), (:K {x: 3})"]
        => "MATCH (a:K {x: 1}) SET a.x = 3 WITH count(*) AS c \
            MATCH (b:K {x: 2}) REMOVE b.x",
           requires("k");
    uniqueness_before_a_later_missing_property:
        ["CREATE CONSTRAINT k FOR (n:K) REQUIRE n.x IS NODE KEY",
         "CREATE (:K {x: 1}), (:K {x: 2}), (:K {x: 3})"]
        => "MATCH (a:K {x: 1}) SET a.x = 2 WITH count(*) AS c \
            MATCH (b:K {x: 3}) REMOVE b.x",
           uniqueness("k");
}

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

/// ADR-0172's deliberate change. Two concurrent transactions can commit the
/// same unique value (ADR-0015 does no write-skew detection). The full scan
/// then failed every later write statement; incremental validation reports
/// the violation only to writes that touch an involved entity.
#[test]
fn pre_existing_violation_fails_only_writes_that_touch_it() {
    let db = cypherlite::Database::open(":memory:").unwrap();
    let mut setup = db.connect().unwrap();
    setup
        .query("CREATE CONSTRAINT p_id FOR (n:P) REQUIRE n.id IS UNIQUE")
        .unwrap();
    let mut first = db.connect().unwrap();
    let mut second = db.connect().unwrap();
    first.begin().unwrap();
    second.begin().unwrap();
    first.query("CREATE (:P {id: 77})").unwrap();
    second.query("CREATE (:P {id: 77})").unwrap();
    first.commit().unwrap();
    second.commit().unwrap();
    let mut c = db.connect().unwrap();
    assert_eq!(
        scalar(&mut c, "MATCH (n:P {id: 77}) RETURN count(n)"),
        Value::Int(2)
    );
    // Unrelated writes succeed (the full scan failed these).
    c.query("CREATE (:Other)").unwrap();
    c.query("CREATE (:P {id: 78})").unwrap();
    // Writes touching an involved entity still fail.
    assert_eq!(
        err_text(c.query("MATCH (n:P {id: 77}) SET n.touched = true")),
        uniqueness("p_id")
    );
    assert_eq!(
        err_text(c.query("CREATE (:P {id: 77})")),
        uniqueness("p_id")
    );
}

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

/// `Graph::prop_postings` holds a `(label, key)` pair's write guard while it
/// reads `label_index` and `nodes`; a commit's `index_add` takes that guard
/// while it holds a `nodes` read guard. An explicit transaction's statements
/// execute beside commits, so its uniqueness probes build and drain the
/// pair while other connections commit nodes into it. Every side finishes.
#[test]
fn uniqueness_probes_in_a_transaction_complete_beside_concurrent_commits() {
    uniqueness_probes_complete_beside_concurrent_commits(
        UNIQUE,
        |id| format!("CREATE (:P {{id: {id}}})"),
        "MATCH (n:P) RETURN count(n)",
    );
}

/// The relationship twin of the node test above (ADR-0185): the lazy pair
/// build holds the pair guard until the pair is built, so a concurrent
/// commit's relationship is either in the build's candidate copy or
/// dirty-marked afterwards. An explicit transaction's probes build and
/// drain the pair while other connections commit relationships into it;
/// every side finishes.
#[test]
fn relationship_uniqueness_probes_in_a_transaction_complete_beside_concurrent_commits() {
    uniqueness_probes_complete_beside_concurrent_commits(
        REL_UNIQUE,
        |id| format!("CREATE ()-[:R {{id: {id}}}]->()"),
        "MATCH ()-[r:R]->() RETURN count(r)",
    );
}

/// The shared harness of the two tests above: an explicit transaction
/// writes `STATEMENTS` tuples through `stmt` while `WRITERS` other
/// connections each commit `STATEMENTS` tuples through the same builder —
/// disjoint ids, so `ddl`'s constraint never fires — and the final
/// `count_query` count is every written tuple. The builder takes the
/// tuple's id (negative for the transaction, positive for the writers).
fn uniqueness_probes_complete_beside_concurrent_commits(
    ddl: &str,
    stmt: fn(i64) -> String,
    count_query: &str,
) {
    const WRITERS: i64 = 3;
    const STATEMENTS: i64 = 200;
    let db = cypherlite::Database::open(":memory:").unwrap();
    db.connect().unwrap().query(ddl).unwrap();
    let (done, finished) = mpsc::channel();
    let mut transaction = db.connect().unwrap();
    let transaction_done = done.clone();
    thread::spawn(move || {
        let outcome = (|| {
            transaction.begin()?;
            for i in 1..=STATEMENTS {
                transaction.query(&stmt(-i))?;
            }
            transaction.commit()
        })();
        transaction_done.send(outcome).unwrap();
    });
    for writer in 0..WRITERS {
        let mut c = db.connect().unwrap();
        let done = done.clone();
        thread::spawn(move || {
            let outcome =
                (0..STATEMENTS).try_for_each(|i| c.query(&stmt(writer * STATEMENTS + i)).map(drop));
            done.send(outcome).unwrap();
        });
    }
    drop(done);
    for _ in 0..=WRITERS {
        finished
            .recv_timeout(Duration::from_secs(120))
            .expect("the transaction and every writer finish")
            .unwrap();
    }
    let mut c = db.connect().unwrap();
    assert_eq!(
        scalar(&mut c, count_query),
        Value::Int((WRITERS + 1) * STATEMENTS)
    );
}

/// Close and reopen the directory store at `uri` — the earlier relationship
/// in a checkpoint taken through the public API when `checkpoint` is set,
/// only in the WAL otherwise — and check that relationship `unique` still
/// rejects its duplicate and accepts a distinct value.
fn relationship_uniqueness_survives_reopen(uri: &str, checkpoint: bool) {
    {
        let db = cypherlite::Database::open(uri).unwrap();
        let mut c = db.connect().unwrap();
        c.query(REL_UNIQUE).unwrap();
        c.query("CREATE ()-[:R {id: 1}]->()").unwrap();
        if checkpoint {
            db.checkpoint().unwrap();
        }
        drop(c);
        db.leak_without_checkpoint();
    }
    let db = cypherlite::Database::open(uri).unwrap();
    let mut c = db.connect().unwrap();
    assert_eq!(
        err_text(c.query("CREATE ()-[:R {id: 1}]->()")),
        uniqueness("r_id"),
        "checkpoint={checkpoint}"
    );
    c.query("CREATE ()-[:R {id: 2}]->()").unwrap();
    assert_eq!(
        scalar(&mut c, "MATCH ()-[r:R]->() RETURN count(r)"),
        Value::Int(2),
        "checkpoint={checkpoint}"
    );
}

#[test]
fn relationship_uniqueness_survives_reopen_from_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    relationship_uniqueness_survives_reopen(dir.path().to_str().unwrap(), false);
}

#[test]
fn relationship_uniqueness_survives_reopen_from_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    relationship_uniqueness_survives_reopen(dir.path().to_str().unwrap(), true);
}
