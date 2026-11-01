//! Shared seeding and probing behind the constraint-validation tests, plus
//! the DDL statements more than one concern uses.

use cypherlite_core::Value;

use super::super::take_constraint_inspects;
use crate::exec::test_support::exec_autocommit;
use crate::graph::Graph;

pub(super) const U: &str = "CREATE CONSTRAINT p_id FOR (n:P) REQUIRE n.id IS UNIQUE";
pub(super) const UC: &str = "CREATE CONSTRAINT p_ab FOR (n:P) REQUIRE (n.a, n.b) IS UNIQUE";
pub(super) const UR: &str = "CREATE CONSTRAINT r_id FOR ()-[r:R]-() REQUIRE r.id IS UNIQUE";
pub(super) const K: &str = "CREATE CONSTRAINT p_k FOR (n:P) REQUIRE n.k IS NODE KEY";
pub(super) const KR: &str = "CREATE CONSTRAINT r_k FOR ()-[r:R]-() REQUIRE r.k IS RELATIONSHIP KEY";
pub(super) const E: &str = "CREATE CONSTRAINT p_name FOR (n:P) REQUIRE n.name IS NOT NULL";
pub(super) const ER: &str = "CREATE CONSTRAINT r_w FOR ()-[r:R]-() REQUIRE r.w IS NOT NULL";
pub(super) const T: &str = "CREATE CONSTRAINT p_age FOR (n:P) REQUIRE n.age IS :: INTEGER";
pub(super) const TR: &str = "CREATE CONSTRAINT r_t FOR ()-[r:R]-() REQUIRE r.t IS :: INTEGER";
pub(super) const TNN: &str = "ALTER CURRENT GRAPH TYPE SET { (:P => {age :: INTEGER NOT NULL}) }";
pub(super) const LE: &str = "ALTER CURRENT GRAPH TYPE SET { (:P => :Q) }";
pub(super) const NLE: &str = "CREATE CONSTRAINT p_q FOR (n:P) REQUIRE n:Q";
pub(super) const SL: &str =
    "ALTER CURRENT GRAPH TYPE SET { (:A => {n :: STRING}), (:A)-[:R =>]->() }";
pub(super) const SLE: &str = "CREATE CONSTRAINT r_src FOR (s)-[r:R]->(t) REQUIRE s:A";
pub(super) const TL: &str =
    "ALTER CURRENT GRAPH TYPE SET { (:B => {n :: STRING}), ()-[:R =>]->(:B) }";
pub(super) const TLE: &str = "CREATE CONSTRAINT r_tgt FOR (s)-[r:R]->(t) REQUIRE t:B";

/// Seed a node straight into the committed graph, bypassing the executor.
pub(super) fn seed_node(g: &Graph, labels: &[&str], props: &[(&str, Value)]) -> i64 {
    g.create_node(
        labels.iter().map(|l| l.to_string()).collect(),
        props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
    .unwrap()
}

/// Seed a relationship straight into the committed graph.
pub(super) fn seed_rel(g: &Graph, start: i64, end: i64, rel_type: &str, props: &[(&str, Value)]) {
    g.create_rel(
        start,
        end,
        rel_type.to_string(),
        props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
    .unwrap();
}

/// Entities the validation of the `measured` statements inspects, on a
/// graph that `seed` filled and the `setup` statements then prepared. Every
/// statement runs as its own autocommit statement.
pub(super) fn inspections(seed: impl FnOnce(&Graph), setup: &[&str], measured: &[&str]) -> u64 {
    let g = Graph::default();
    seed(&g);
    for statement in setup {
        exec_autocommit(&g, statement).unwrap_or_else(|e| panic!("setup `{statement}`: {e}"));
    }
    take_constraint_inspects();
    for statement in measured {
        exec_autocommit(&g, statement).unwrap_or_else(|e| panic!("`{statement}`: {e}"));
    }
    take_constraint_inspects()
}
