//! ADR-0172's acceptance counters: the inspection count of a validation is
//! independent of how large the constrained label or relationship type
//! already is.

use cypherlite_core::Value;

use super::harness::{inspections, seed_node, seed_rel};

fn inspections_for_new_person(preloaded: i64) -> u64 {
    inspections(
        |g| {
            for i in 0..preloaded {
                seed_node(
                    g,
                    &["Person"],
                    &[("email", Value::String(format!("p{i}@example.com").into()))],
                );
            }
        },
        &["CREATE CONSTRAINT FOR (n:Person) REQUIRE n.email IS UNIQUE"],
        &["CREATE (:Person {email: 'new'})"],
    )
}

/// ADR-0172's acceptance counter: validating a one-node `CREATE` under a
/// uniqueness constraint inspects a constant number of entities, however
/// many nodes the label already holds.
#[test]
fn validation_inspections_independent_of_graph_size() {
    let small = inspections_for_new_person(1_000);
    let large = inspections_for_new_person(10_000);
    assert_eq!(small, large, "inspections must not grow with the label");
    assert!(
        (1..=4).contains(&small),
        "one re-validated node and no partner, got {small}"
    );
}

/// `unique`, `key`, `exists`, `propertyType` and `nodeLabelExistence` node
/// constraints: 100 fresh valid nodes cost 100 inspections per constraint,
/// at 1,000 and at 10,000 pre-existing nodes.
fn node_kinds_inspections(preloaded: i64) -> u64 {
    inspections(
        |g| {
            for i in 0..preloaded {
                seed_node(
                    g,
                    &["P", "Q"],
                    &[
                        ("id", Value::Int(i)),
                        ("k", Value::Int(i)),
                        ("name", Value::String("n".into())),
                        ("age", Value::Int(i)),
                    ],
                );
            }
        },
        &[
            "CREATE CONSTRAINT p_id FOR (n:P) REQUIRE n.id IS UNIQUE",
            "CREATE CONSTRAINT p_k FOR (n:P) REQUIRE n.k IS NODE KEY",
            "CREATE CONSTRAINT p_name FOR (n:P) REQUIRE n.name IS NOT NULL",
            "CREATE CONSTRAINT p_age FOR (n:P) REQUIRE n.age IS :: INTEGER",
            "CREATE CONSTRAINT p_q FOR (n:P) REQUIRE n:Q",
            "CREATE (:P:Q {id: -1, k: -1, name: 'w', age: 0})",
        ],
        &["UNWIND range(1, 100) AS i CREATE (:P:Q {id: -1 - i, k: -1 - i, name: 'x', age: i})"],
    )
}

#[test]
fn node_constraint_inspections_independent_of_graph_size() {
    let small = node_kinds_inspections(1_000);
    assert_eq!(small, 5 * 100);
    assert_eq!(node_kinds_inspections(10_000), small);
}

/// `exists`, `propertyType`, `sourceLabelExistence` and
/// `targetLabelExistence` relationship constraints.
fn rel_kinds_inspections(preloaded: i64) -> u64 {
    inspections(
        |g| {
            for i in 0..preloaded {
                let s = seed_node(g, &["S"], &[]);
                let t = seed_node(g, &["T"], &[]);
                seed_rel(g, s, t, "R", &[("w", Value::Int(i)), ("t", Value::Int(i))]);
            }
        },
        &[
            "CREATE CONSTRAINT r_w FOR ()-[r:R]-() REQUIRE r.w IS NOT NULL",
            "CREATE CONSTRAINT r_t FOR ()-[r:R]-() REQUIRE r.t IS :: INTEGER",
            "CREATE CONSTRAINT r_src FOR (s)-[r:R]->(t) REQUIRE s:S",
            "CREATE CONSTRAINT r_tgt FOR (s)-[r:R]->(t) REQUIRE t:T",
            "CREATE (:S)-[:R {w: 0, t: 0}]->(:T)",
        ],
        &["UNWIND range(1, 100) AS i CREATE (:S)-[:R {w: i, t: i}]->(:T)"],
    )
}

#[test]
fn relationship_constraint_inspections_independent_of_graph_size() {
    let small = rel_kinds_inspections(1_000);
    assert_eq!(small, 4 * 100);
    assert_eq!(rel_kinds_inspections(10_000), small);
}

/// ADR-0184: a composite `unique`/`key` constraint probes through its
/// smallest committed-plus-scratch bucket, so a low-cardinality first
/// property no longer inherits the label's cardinality. `a` collides across
/// the preload and the measured statement, `b` is unique, so the probe must
/// take `b` and inspect a constant count at 1,000 and at 10,000 preloaded
/// nodes.
fn composite_bucket_inspections(ddl: &str, preloaded: i64) -> u64 {
    inspections(
        |g| {
            for i in 0..preloaded {
                seed_node(
                    g,
                    &["P"],
                    &[("a", Value::Int(i % 10)), ("b", Value::Int(i))],
                );
            }
        },
        &[ddl],
        &["UNWIND range(1, 100) AS i CREATE (:P {a: 1, b: -i})"],
    )
}

#[test]
fn composite_constraint_inspections_use_the_smallest_bucket() {
    for ddl in [
        "CREATE CONSTRAINT p_abk FOR (n:P) REQUIRE (n.a, n.b) IS NODE KEY",
        "CREATE CONSTRAINT p_ab FOR (n:P) REQUIRE (n.a, n.b) IS UNIQUE",
    ] {
        let small = composite_bucket_inspections(ddl, 1_000);
        assert!(small <= 200, "{ddl}: got {small}");
        assert_eq!(composite_bucket_inspections(ddl, 10_000), small, "{ddl}");
    }
}

/// Relationship `unique`/`key` probe through the lazy relationship equality
/// postings plus the scratch's twin (ADR-0185): `a` collides across the
/// preload and the measured statement, `id` and `b` are unique, so the probe
/// must pick the unique property's bucket and inspect a constant count at
/// 1,000 and at 10,000 preloaded relationships.
fn rel_unique_inspections(ddl: &str, preloaded: i64) -> u64 {
    inspections(
        |g| {
            let s = seed_node(g, &[], &[]);
            let t = seed_node(g, &[], &[]);
            for i in 0..preloaded {
                seed_rel(
                    g,
                    s,
                    t,
                    "T",
                    &[
                        ("id", Value::Int(i)),
                        ("a", Value::Int(i % 10)),
                        ("b", Value::Int(i)),
                    ],
                );
            }
        },
        &[ddl],
        &["UNWIND range(1, 100) AS i CREATE ()-[:T {id: -i, a: 1, b: -i}]->()"],
    )
}

#[test]
fn relationship_uniqueness_probe_inspections_are_constant() {
    for ddl in [
        "CREATE CONSTRAINT t_id FOR ()-[r:T]-() REQUIRE r.id IS UNIQUE",
        "CREATE CONSTRAINT t_id FOR ()-[r:T]-() REQUIRE r.id IS RELATIONSHIP KEY",
        "CREATE CONSTRAINT t_ab FOR ()-[r:T]-() REQUIRE (r.a, r.b) IS RELATIONSHIP KEY",
    ] {
        let small = rel_unique_inspections(ddl, 1_000);
        assert!(small <= 200, "{ddl}: got {small}");
        assert_eq!(rel_unique_inspections(ddl, 10_000), small, "{ddl}");
    }
}
