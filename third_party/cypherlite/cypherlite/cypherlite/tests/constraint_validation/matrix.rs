//! The constraint-kind x trigger-operation matrix: one row per violation of
//! ADR-0172's section-2 table, each asserting today's exact error text and an
//! unchanged graph, plus the error-selection order.

use crate::common::{property_type, requires, uniqueness};
use crate::support::*;

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
