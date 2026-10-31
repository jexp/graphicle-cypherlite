//! The differential oracle: the full-scan validator ADR-0172 replaced,
//! kept test-only, and the corpus every corpus statement is checked
//! through — the incremental validator must reach the same verdict and
//! error text as the oracle on the same state.

use std::str::FromStr;

use cypherlite_core::{Error, Result, Value};
use cypherlite_model::ConstraintType;

use super::super::{parse_constraint_type, validate_constraints};
use super::harness::*;
use crate::exec::test_support::{
    exec_autocommit, exec_write, exec_write_unvalidated, write_scratch,
};
use crate::graph::Graph;
use crate::overlay::ExecGraph;

struct Case {
    setup: &'static [&'static str],
    /// Earlier statements of the same transaction (validated, not committed).
    earlier: &'static [&'static str],
    statement: &'static str,
    violates: bool,
}

const fn case(setup: &'static [&'static str], statement: &'static str, violates: bool) -> Case {
    Case {
        setup,
        earlier: &[],
        statement,
        violates,
    }
}

/// Composite-key DDL the harness constants do not carry: the node key, the
/// same key with its properties swapped, and the relationship twins.
const UCK: &str = "CREATE CONSTRAINT p_abk FOR (n:P) REQUIRE (n.a, n.b) IS NODE KEY";
const UCK_BA: &str = "CREATE CONSTRAINT p_abk FOR (n:P) REQUIRE (n.b, n.a) IS NODE KEY";
const UC_BA: &str = "CREATE CONSTRAINT p_ab FOR (n:P) REQUIRE (n.b, n.a) IS UNIQUE";
const URC: &str = "CREATE CONSTRAINT r_ab FOR ()-[r:R]-() REQUIRE (r.a, r.b) IS UNIQUE";
const KRC: &str = "CREATE CONSTRAINT r_abk FOR ()-[r:R]-() REQUIRE (r.a, r.b) IS RELATIONSHIP KEY";

/// A corpus covering every kind of the ADR's §2 table through its trigger
/// operations, with violating and valid statements, recheck DDL, and
/// several failures in one statement (error selection).
const CORPUS: &[Case] = &[
    // unique, node
    case(&[U, "CREATE (:P {id: 1})"], "CREATE (:P {id: 1.0})", true),
    case(&[U, "CREATE (:P {id: 1})"], "CREATE (:P {id: 2})", false),
    case(&[U], "UNWIND [1, 2, 1] AS i CREATE (:P {id: i})", true),
    case(
        &[U, "CREATE (:P {id: [1]})"],
        "CREATE (:P {id: [1.0]})",
        true,
    ),
    case(
        &[U],
        "CREATE (:P {id: 9007199254740993}), (:P {id: 9007199254740992.0})",
        false,
    ),
    case(&[U], "CREATE (:P {id: nan}), (:P {id: nan})", false),
    case(
        &[U, "CREATE (:P {id: 1}), (:P {id: 2})"],
        "MATCH (n:P {id: 2}) SET n.id = 1",
        true,
    ),
    case(
        &[U, "CREATE (:P {id: 1}), (:P {id: 2})"],
        "MATCH (n:P {id: 2}) SET n = {id: 1}",
        true,
    ),
    case(
        &[U, "CREATE (:P {id: 1}), (:P {id: 2})"],
        "MATCH (n:P {id: 2}) SET n += {id: 1}",
        true,
    ),
    case(
        &[U, "CREATE (:P {id: 1}), (:X {id: 1})"],
        "MATCH (n:X) SET n:P",
        true,
    ),
    case(
        &[U, "CREATE (:P {id: 1})"],
        "MERGE (:P {id: 1, tag: 1})",
        true,
    ),
    case(&[U, "CREATE (:P {id: 1})"], "MERGE (:P {id: 1})", false),
    case(
        &[U, "CREATE (:P {id: 1})"],
        "MERGE (n:P {id: 3}) ON CREATE SET n.id = 1",
        true,
    ),
    case(
        &[U, "CREATE (:P {id: 1}), (:P {id: 2})"],
        "MERGE (n:P {id: 2}) ON MATCH SET n.id = 1",
        true,
    ),
    case(
        &[U, "CREATE (:P {id: 1}), (:P {id: 2})"],
        "MATCH (n:P) SET n.id = n.id",
        false,
    ),
    case(
        &[U, "CREATE (:P {id: 1}), (:P {id: 2})"],
        "MATCH (a:P {id: 1}), (b:P {id: 2}) SET a.id = 2, b.id = 1",
        false,
    ),
    case(
        &[U, "CREATE (:P {id: 1})"],
        "MATCH (n:P {id: 1}) DELETE n WITH count(*) AS c CREATE (:P {id: 1})",
        false,
    ),
    case(&["CREATE (:P {id: 1}), (:P {id: 1})"], U, true),
    case(&["CREATE (:P {id: 1}), (:P {id: 2})"], U, false),
    case(
        &["CREATE (:P {id: 1}), (:P {id: 1})"],
        "ALTER CURRENT GRAPH TYPE SET { CONSTRAINT p_id FOR (n:P) REQUIRE n.id IS UNIQUE }",
        true,
    ),
    Case {
        setup: &[U],
        earlier: &["CREATE (:P {id: 'dup'})"],
        statement: "CREATE (:P {id: 'dup'})",
        violates: true,
    },
    Case {
        setup: &[U, "CREATE (:P {id: 1})"],
        earlier: &[
            "MATCH (n:P {id: 1}) SET n.id = 2",
            "MATCH (n:P {id: 2}) SET n:X",
        ],
        statement: "CREATE (:P {id: 1}), (:X {id: 2})",
        violates: false,
    },
    Case {
        setup: &[U, "CREATE (:X {id: 5})"],
        earlier: &["CREATE (:P {id: 5})"],
        statement: "MATCH (n:X) SET n:P",
        violates: true,
    },
    // unique, composite
    case(
        &[UC, "CREATE (:P {a: 1, b: 2})"],
        "CREATE (:P {a: 1.0, b: 2})",
        true,
    ),
    case(
        &[UC, "CREATE (:P {a: 1, b: 2})"],
        "CREATE (:P {a: 1, b: 3}), (:P {a: 1})",
        false,
    ),
    // Low-cardinality `a`: a shared first component alone never conflicts,
    // and neither does a shared second one; only the full tuple does.
    case(
        &[UC, "CREATE (:P {a: 1, b: 2}), (:P {a: 1, b: 3})"],
        "CREATE (:P {a: 1, b: 2})",
        true,
    ),
    case(
        &[UC, "CREATE (:P {a: 1, b: 2}), (:P {a: 1, b: 3})"],
        "CREATE (:P {a: 1, b: 4})",
        false,
    ),
    case(
        &[UC, "CREATE (:P {a: 1, b: 2}), (:P {a: 2, b: 2})"],
        "CREATE (:P {a: 3, b: 2})",
        false,
    ),
    // The swapped declaration rejects the same tuple.
    case(
        &[UC_BA, "CREATE (:P {a: 1, b: 2})"],
        "CREATE (:P {a: 1, b: 2})",
        true,
    ),
    case(
        &[UC_BA, "CREATE (:P {a: 1, b: 2})"],
        "CREATE (:P {a: 3, b: 2})",
        false,
    ),
    case(&[UC], "UNWIND [2, 2] AS i CREATE (:P {a: 1, b: i})", true),
    case(
        &[UC, "CREATE (:P {a: 1, b: 2}), (:P {a: 1, b: 3})"],
        "MATCH (n:P {b: 3}) SET n.b = 2",
        true,
    ),
    // A null component never conflicts under `unique`.
    case(
        &[UC, "CREATE (:P {a: 1, b: 2})"],
        "CREATE (:P {a: 1, b: null})",
        false,
    ),
    // NaN in the second component never conflicts either.
    case(
        &[UC],
        "CREATE (:P {a: 1, b: nan}), (:P {a: 1, b: nan})",
        false,
    ),
    // unique, relationship
    case(
        &[UR, "CREATE ()-[:R {id: 1}]->()"],
        "CREATE ()-[:R {id: 1}]->()",
        true,
    ),
    case(
        &[UR, "CREATE ()-[:R {id: 1}]->()"],
        "CREATE ()-[:R {id: 2}]->(), ()-[:S {id: 1}]->()",
        false,
    ),
    case(
        &[UR],
        "UNWIND [1, 2, 2] AS i CREATE ()-[:R {id: i}]->()",
        true,
    ),
    case(
        &[UR, "CREATE ()-[:R {id: 1}]->(), ()-[:R {id: 2}]->()"],
        "MATCH ()-[r:R {id: 2}]->() SET r.id = 1",
        true,
    ),
    case(
        &[UR, "CREATE ()-[:R {id: 1}]->()"],
        "MERGE (:A)-[:R {id: 1}]->(:B)",
        true,
    ),
    case(
        &["CREATE ()-[:R {id: 1}]->(), ()-[:R {id: 1}]->()"],
        UR,
        true,
    ),
    // `1` and `1.0` denote the same value for `unique`.
    case(
        &[UR, "CREATE ()-[:R {id: 1}]->()"],
        "CREATE ()-[:R {id: 1.0}]->()",
        true,
    ),
    // A duplicate created by an earlier statement of the same transaction.
    Case {
        setup: &[UR],
        earlier: &["CREATE ()-[:R {id: 'dup'}]->()"],
        statement: "CREATE ()-[:R {id: 'dup'}]->()",
        violates: true,
    },
    // unique, composite relationship: only the full tuple conflicts.
    case(
        &[URC, "CREATE ()-[:R {a: 1, b: 2}]->()"],
        "CREATE ()-[:R {a: 1, b: 2}]->()",
        true,
    ),
    case(
        &[URC, "CREATE ()-[:R {a: 1, b: 2}]->()"],
        "CREATE ()-[:R {a: 1, b: 3}]->()",
        false,
    ),
    case(
        &[URC, "CREATE ()-[:R {a: 1, b: 2}]->()"],
        "CREATE ()-[:R {a: 3, b: 2}]->()",
        false,
    ),
    case(
        &[URC],
        "UNWIND [2, 2] AS i CREATE ()-[:R {a: 1, b: i}]->()",
        true,
    ),
    case(
        &[
            URC,
            "CREATE ()-[:R {a: 1, b: 2}]->(), ()-[:R {a: 1, b: 3}]->()",
        ],
        "MATCH ()-[r:R {b: 3}]->() SET r.b = 2",
        true,
    ),
    case(
        &[URC, "CREATE ()-[:R {a: 1, b: 2}]->()"],
        "CREATE ()-[:R {a: 1.0, b: 2.0}]->()",
        true,
    ),
    Case {
        setup: &[URC],
        earlier: &["CREATE ()-[:R {a: 1, b: 2}]->()"],
        statement: "CREATE ()-[:R {a: 1, b: 2}]->()",
        violates: true,
    },
    // key
    case(&[K], "CREATE (:K), (:P)", true),
    case(&[K, "CREATE (:P {k: 1})"], "CREATE (:P {k: 1})", true),
    case(&[K, "CREATE (:P {k: 1})"], "MATCH (n:P) REMOVE n.k", true),
    case(
        &[K, "CREATE (:P {k: 1})"],
        "MATCH (n:P) SET n = {other: 1}",
        true,
    ),
    case(&[K, "CREATE (:X)"], "MATCH (n:X) SET n:P", true),
    case(&[K], "CREATE (:P {k: 5}), (:P), (:P {k: 5})", true),
    case(
        &[K, "CREATE (:P {k: 1}), (:P {k: 2}), (:P {k: 3})"],
        "MATCH (a:P {k: 1}) SET a.k = 3 WITH count(*) AS c MATCH (b:P {k: 2}) REMOVE b.k",
        true,
    ),
    case(
        &[K, "CREATE (:P {k: 1}), (:P {k: 2}), (:P {k: 3})"],
        "MATCH (a:P {k: 1}) SET a.k = 2 WITH count(*) AS c MATCH (b:P {k: 3}) REMOVE b.k",
        true,
    ),
    case(&[KR], "CREATE ()-[:R]->()", true),
    case(
        &[KR, "CREATE ()-[:R {k: 1}]->()"],
        "MATCH ()-[r:R]->() SET r.k = null",
        true,
    ),
    case(
        &[KR, "CREATE ()-[:R {k: 1}]->()"],
        "CREATE ()-[:R {k: 2}]->()",
        false,
    ),
    // key, relationship, single-property: a duplicate across statements,
    // `1` vs `1.0`, one UNWIND, a SET collision, and an earlier statement's
    // duplicate.
    case(
        &[KR, "CREATE ()-[:R {k: 1}]->()"],
        "CREATE ()-[:R {k: 1}]->()",
        true,
    ),
    case(
        &[KR, "CREATE ()-[:R {k: 1}]->()"],
        "CREATE ()-[:R {k: 1.0}]->()",
        true,
    ),
    case(
        &[KR],
        "UNWIND [1, 2, 2] AS i CREATE ()-[:R {k: i}]->()",
        true,
    ),
    case(
        &[KR, "CREATE ()-[:R {k: 1}]->(), ()-[:R {k: 2}]->()"],
        "MATCH ()-[r:R {k: 2}]->() SET r.k = 1",
        true,
    ),
    Case {
        setup: &[KR],
        earlier: &["CREATE ()-[:R {k: 7}]->()"],
        statement: "CREATE ()-[:R {k: 7}]->()",
        violates: true,
    },
    // key, composite relationship: only the full tuple conflicts.
    case(
        &[KRC, "CREATE ()-[:R {a: 1, b: 2}]->()"],
        "CREATE ()-[:R {a: 1, b: 2}]->()",
        true,
    ),
    case(
        &[KRC, "CREATE ()-[:R {a: 1, b: 2}]->()"],
        "CREATE ()-[:R {a: 1, b: 3}]->()",
        false,
    ),
    case(
        &[KRC, "CREATE ()-[:R {a: 1, b: 2}]->()"],
        "CREATE ()-[:R {a: 3, b: 2}]->()",
        false,
    ),
    case(
        &[KRC],
        "UNWIND [2, 2] AS i CREATE ()-[:R {a: 1, b: i}]->()",
        true,
    ),
    case(
        &[
            KRC,
            "CREATE ()-[:R {a: 1, b: 2}]->(), ()-[:R {a: 1, b: 3}]->()",
        ],
        "MATCH ()-[r:R {b: 3}]->() SET r.b = 2",
        true,
    ),
    case(
        &[KRC, "CREATE ()-[:R {a: 1, b: 2}]->()"],
        "CREATE ()-[:R {a: 1.0, b: 2.0}]->()",
        true,
    ),
    Case {
        setup: &[KRC],
        earlier: &["CREATE ()-[:R {a: 1, b: 2}]->()"],
        statement: "CREATE ()-[:R {a: 1, b: 2}]->()",
        violates: true,
    },
    case(&["CREATE (:P {k: 1}), (:P)"], K, true),
    // key, composite node: both components required, low-cardinality `a`.
    case(
        &[UCK, "CREATE (:P {a: 1, b: 2}), (:P {a: 1, b: 3})"],
        "CREATE (:P {a: 1, b: 2})",
        true,
    ),
    case(
        &[UCK, "CREATE (:P {a: 1, b: 2}), (:P {a: 1, b: 3})"],
        "CREATE (:P {a: 1, b: 4})",
        false,
    ),
    case(
        &[UCK, "CREATE (:P {a: 1, b: 2}), (:P {a: 2, b: 2})"],
        "CREATE (:P {a: 3, b: 2})",
        false,
    ),
    case(
        &[UCK_BA, "CREATE (:P {a: 1, b: 2})"],
        "CREATE (:P {a: 1, b: 2})",
        true,
    ),
    case(&[UCK], "UNWIND [2, 2] AS i CREATE (:P {a: 1, b: i})", true),
    case(
        &[UCK, "CREATE (:P {a: 1, b: 2}), (:P {a: 1, b: 3})"],
        "MATCH (n:P {b: 3}) SET n.b = 2",
        true,
    ),
    // A null component fails `requires properties` under `key`.
    case(
        &[UCK, "CREATE (:P {a: 1, b: 2})"],
        "CREATE (:P {a: 1})",
        true,
    ),
    // NaN is not null: the tuple passes `key` and never conflicts.
    case(
        &[UCK],
        "CREATE (:P {a: 1, b: nan}), (:P {a: 1, b: nan})",
        false,
    ),
    // exists
    case(&[E], "CREATE (:P)", true),
    case(&[E], "CREATE (:P {name: 'a'}), (:X)", false),
    case(
        &[E, "CREATE (:P {name: 'a'})"],
        "MATCH (n:P) SET n.name = null",
        true,
    ),
    case(
        &[E, "CREATE (:P {name: 'a'})"],
        "MATCH (n:P) SET n += {name: null}",
        true,
    ),
    case(&[E, "CREATE (:X)"], "MATCH (n:X) SET n:P", true),
    case(&[ER], "CREATE ()-[:R]->()", true),
    case(
        &[ER, "CREATE ()-[:R {w: 1}]->()"],
        "MATCH ()-[r:R]->() REMOVE r.w",
        true,
    ),
    case(&["CREATE (:P)"], E, true),
    // propertyType
    case(&[T], "CREATE (:P {age: 'old'})", true),
    case(&[T], "CREATE (:P {age: 1}), (:P)", false),
    case(
        &[T, "CREATE (:P {age: 1})"],
        "MATCH (n:P) SET n.age = 'x'",
        true,
    ),
    case(&[T, "CREATE (:X {age: 'x'})"], "MATCH (n:X) SET n:P", true),
    case(
        &[TNN, "CREATE (:P {age: 1})"],
        "MATCH (n:P) REMOVE n.age",
        true,
    ),
    case(&[TR], "CREATE ()-[:R {t: 'x'}]->()", true),
    case(
        &[TR, "CREATE ()-[:R {t: 1}]->()"],
        "MATCH ()-[r:R]->() SET r = {t: 2}",
        false,
    ),
    case(&["CREATE (:P {age: 'old'})"], T, true),
    case(
        &["CREATE (:P {age: 'old'})"],
        "ALTER CURRENT GRAPH TYPE SET { (:P => {age :: INTEGER}) }",
        true,
    ),
    // labelExistence / nodeLabelExistence
    case(&[LE], "CREATE (:P)", true),
    case(&[LE], "CREATE (:P:Q), (:Q)", false),
    case(&[LE, "CREATE (:P:Q)"], "MATCH (n:P) REMOVE n:Q", true),
    case(&[LE, "CREATE (:X)"], "MATCH (n:X) SET n:P", true),
    case(&["CREATE (:P)"], LE, true),
    case(&[NLE], "MERGE (:P {x: 1})", true),
    case(&[NLE, "CREATE (:P:Q)"], "MATCH (n:P) REMOVE n:Q", true),
    case(
        &[NLE, "CREATE (:P:Q)"],
        "MATCH (n:P) REMOVE n:P, n:Q",
        false,
    ),
    case(&["CREATE (:P)"], NLE, true),
    // sourceLabel / sourceLabelExistence
    case(&[SL], "CREATE (:X)-[:R]->()", true),
    case(&[SL], "CREATE (:A)-[:R]->(), (:X)-[:S]->()", false),
    case(
        &[SL, "CREATE (:A)-[:R]->()"],
        "MATCH (n:A) REMOVE n:A",
        true,
    ),
    case(
        &[SL, "CREATE (:A {start: true})-[:R]->(:A)"],
        "MATCH (n:A) WHERE n.start IS NULL REMOVE n:A",
        false,
    ),
    case(&["CREATE (:X)-[:R]->()"], SL, true),
    case(&[SLE], "MERGE (:X)-[:R]->(:Y)", true),
    case(
        &[SLE, "CREATE (:A)-[:R]->()"],
        "MATCH (n:A) REMOVE n:A",
        true,
    ),
    case(
        &[SLE, "CREATE (a:A)-[:R]->()"],
        "MATCH (n:A) DETACH DELETE n",
        false,
    ),
    case(&["CREATE (:X)-[:R]->()"], SLE, true),
    // targetLabel / targetLabelExistence
    case(&[TL], "CREATE ()-[:R]->(:X)", true),
    case(
        &[TL, "CREATE ()-[:R]->(:B)"],
        "MATCH (n:B) REMOVE n:B",
        true,
    ),
    case(
        &[TL, "CREATE (:B {start: true})-[:R]->(:B)"],
        "MATCH (n:B {start: true}) REMOVE n:B",
        false,
    ),
    case(&["CREATE ()-[:R]->(:X)"], TL, true),
    case(&[TLE], "CREATE ()-[:R]->(:B)", false),
    case(
        &[TLE, "CREATE ()-[:R]->(:B)"],
        "MATCH (n:B) REMOVE n:B",
        true,
    ),
    case(&["CREATE ()-[:R]->(:X)"], TLE, true),
    // several constraints at once: the first in name order wins
    case(
        &[U, E, T, "CREATE (:P {id: 1, name: 'a', age: 1})"],
        "CREATE (:P {id: 1, age: 'x'})",
        true,
    ),
    case(
        &[U, T, NLE, "CREATE (:P:Q {id: 1, age: 1})"],
        "MATCH (n:P) REMOVE n:Q SET n.age = 'x' CREATE (:P:Q {id: 1})",
        true,
    ),
];

/// The full-scan validator ADR-0172 replaced: every live entity,
/// constraints in name order, ids ascending, first failure wins. Kept as it
/// was, except that the six label-kind tags are matched as the
/// `ConstraintType` variants they parse to.
fn full_scan_oracle(graph: &ExecGraph<'_>) -> Result<()> {
    let constraints = graph.named_constraints();
    if constraints.is_empty() {
        return Ok(());
    }
    let show = graph.show_schema()?;
    for constraint in constraints {
        let enforced_label = show
            .data
            .derived_constraints
            .get(&constraint.name)
            .and_then(|constraint| constraint.enforced_label.clone());
        let kind = ConstraintType::from_str(&constraint.kind).ok();
        let required = kind.is_some_and(|kind| kind.enforces_property_presence());
        let unique = kind.is_some_and(|kind| kind.enforces_uniqueness());
        let property_type = constraint
            .property_type
            .as_deref()
            .map(|ty| {
                parse_constraint_type(ty).ok_or_else(|| {
                    Error::semantic(format!(
                        "ConstraintValidationFailed: `{}` property type",
                        constraint.name
                    ))
                })
            })
            .transpose()?;
        let mut seen: Vec<Vec<Value>> = Vec::new();
        if constraint.relationship {
            for id in graph.all_rel_ids() {
                let Some(entity) = graph.rel(id) else {
                    continue;
                };
                if entity.rel_type != constraint.label {
                    continue;
                }
                if let Some(label) = &enforced_label {
                    let endpoint = match kind {
                        Some(
                            ConstraintType::SourceLabel | ConstraintType::SourceLabelExistence,
                        ) => graph.node_labels(entity.start),
                        Some(
                            ConstraintType::TargetLabel | ConstraintType::TargetLabelExistence,
                        ) => graph.node_labels(entity.end),
                        _ => None,
                    };
                    if endpoint.is_some_and(|labels| !labels.contains(label)) {
                        return Err(Error::execution(format!(
                            "ConstraintValidationFailed: endpoint label presence verification failed: expected {label}"
                        )));
                    }
                }
                let values: Vec<_> = constraint
                    .properties
                    .iter()
                    .map(|key| entity.properties.get(key).cloned().unwrap_or(Value::Null))
                    .collect();
                oracle_values(
                    &constraint,
                    &values,
                    required,
                    unique,
                    &mut seen,
                    property_type.as_ref(),
                )?;
            }
        } else {
            for id in graph.all_node_ids() {
                let Some(entity) = graph.node(id) else {
                    continue;
                };
                if !entity.labels.iter().any(|label| label == &constraint.label) {
                    continue;
                }
                if matches!(
                    kind,
                    Some(ConstraintType::LabelExistence | ConstraintType::NodeLabelExistence)
                ) && enforced_label
                    .as_ref()
                    .is_some_and(|label| !entity.labels.contains(label))
                {
                    return Err(Error::execution(format!(
                        "ConstraintValidationFailed: node with label {} must also have label {}",
                        constraint.label,
                        enforced_label.as_deref().unwrap_or_default()
                    )));
                }
                let values: Vec<_> = constraint
                    .properties
                    .iter()
                    .map(|key| entity.properties.get(key).cloned().unwrap_or(Value::Null))
                    .collect();
                oracle_values(
                    &constraint,
                    &values,
                    required,
                    unique,
                    &mut seen,
                    property_type.as_ref(),
                )?;
            }
        }
    }
    Ok(())
}

fn oracle_values(
    constraint: &cypherlite_catalog::ConstraintDef,
    values: &[Value],
    required: bool,
    unique: bool,
    seen: &mut Vec<Vec<Value>>,
    property_type: Option<&cypherlite_ir::TypeExpr>,
) -> Result<()> {
    if required && values.iter().any(Value::is_null) {
        return Err(Error::semantic(format!(
            "ConstraintValidationFailed: `{}` requires properties",
            constraint.name
        )));
    }
    if let Some(ty) = property_type {
        if values
            .first()
            .is_some_and(|value| !crate::eval::value_matches_type(value, ty))
        {
            return Err(Error::semantic(format!(
                "ConstraintValidationFailed: `{}` property type",
                constraint.name
            )));
        }
    }
    if unique && values.iter().all(|value| !value.is_null()) {
        if seen.iter().any(|prior| {
            prior.len() == values.len()
                && prior
                    .iter()
                    .zip(values)
                    .all(|(left, right)| left.eq_non_null(right))
        }) {
            return Err(Error::semantic(format!(
                "ConstraintValidationFailed: `{}` uniqueness",
                constraint.name
            )));
        }
        seen.push(values.to_vec());
    }
    Ok(())
}

/// Every corpus statement gets the same verdict and error text from the
/// incremental validator as from the full-scan oracle on the same state.
#[test]
fn incremental_validation_agrees_with_the_full_scan() {
    for case in CORPUS {
        let g = Graph::default();
        for statement in case.setup {
            exec_autocommit(&g, statement).unwrap_or_else(|e| panic!("setup `{statement}`: {e}"));
        }
        let scratch = write_scratch(&g);
        for statement in case.earlier {
            exec_write(&g, &scratch, statement)
                .unwrap_or_else(|e| panic!("earlier `{statement}`: {e}"));
        }
        exec_write_unvalidated(&g, &scratch, case.statement)
            .unwrap_or_else(|e| panic!("`{}` failed before validation: {e}", case.statement));
        let view = ExecGraph::write(&g, &scratch);
        let oracle = full_scan_oracle(&view).map_err(|e| e.to_string());
        let incremental = validate_constraints(&view).map_err(|e| e.to_string());
        assert_eq!(incremental, oracle, "`{}`", case.statement);
        assert_eq!(
            oracle.is_err(),
            case.violates,
            "`{}` corpus expectation: {oracle:?}",
            case.statement
        );
    }
}
