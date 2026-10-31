//! Behavior tests: the validate-then-mark cursor (ADR-0173), the
//! committed-catalog fingerprint (ADR-0174), fail-closed parsing of stored
//! constraints, and typed constraint-type matching.

use cypherlite_catalog::ConstraintDef;
use cypherlite_core::Value;

use super::super::{
    parse_constraint_type, take_constraint_inspects, validate_constraints, ConstraintCheck,
};
use super::harness::{inspections, seed_node, U};
use crate::exec::ddl::type_expr_name;
use crate::exec::test_support::{
    exec_autocommit, exec_write, exec_write_unvalidated, write_scratch,
};
use crate::graph::Graph;
use crate::overlay::ExecGraph;

#[test]
fn a_write_without_writes_validates_nothing() {
    let count = inspections(
        |g| {
            for i in 0..100 {
                seed_node(g, &["P"], &[("id", Value::Int(i))]);
            }
        },
        &[U],
        &["MATCH (n:P {id: 1}) RETURN n", "MATCH (n:P) DELETE n"],
    );
    assert_eq!(count, 0);
}

/// Statements of one scratch validate only their own slice: a second
/// statement does not re-read what the first created.
#[test]
fn each_statement_of_a_transaction_validates_its_own_writes() {
    let g = Graph::default();
    exec_autocommit(&g, U).unwrap();
    let scratch = write_scratch(&g);
    exec_write(&g, &scratch, "UNWIND range(1, 50) AS i CREATE (:P {id: i})").unwrap();
    take_constraint_inspects();
    exec_write(&g, &scratch, "CREATE (:P {id: 1000})").unwrap();
    assert_eq!(take_constraint_inspects(), 1);
    let error = exec_write(&g, &scratch, "CREATE (:P {id: 7})")
        .err()
        .expect("a duplicate of an earlier statement's value");
    assert_eq!(
        error.to_string(),
        "semantic error: ConstraintValidationFailed: `p_id` uniqueness"
    );
}

/// A statement of duplicates fails after a number of inspections linear in
/// its size: the partner probes stop once no later conflict can win, rather
/// than probing every one of the 1,000 nodes against all the others.
#[test]
fn duplicates_stop_probing_once_the_first_conflict_is_settled() {
    let g = Graph::default();
    exec_autocommit(&g, U).unwrap();
    take_constraint_inspects();
    let error = exec_autocommit(&g, "UNWIND range(1, 1000) AS i CREATE (:P {id: 1})")
        .err()
        .expect("a statement of duplicates fails");
    assert_eq!(
        error.to_string(),
        "semantic error: ConstraintValidationFailed: `p_id` uniqueness"
    );
    let count = take_constraint_inspects();
    assert!(count < 5 * 1_000, "linear in the statement, got {count}");
}

/// ADR-0184: the composite probe selects the constrained property whose
/// committed-plus-scratch bucket is smallest, ties to the lowest index; a
/// single-property constraint selects 0 without consulting the lookup.
#[test]
fn property_selection_picks_the_smallest_bucket() {
    let def = ConstraintDef {
        name: "p_ab".into(),
        relationship: false,
        label: "P".into(),
        properties: vec!["a".into(), "b".into()],
        kind: "unique".into(),
        property_type: None,
    };
    let check = ConstraintCheck::new(&def, None).unwrap();
    assert_eq!(check.select_property(|i| [5, 3][i]), 1);
    assert_eq!(check.select_property(|i| [3, 5][i]), 0);
    assert_eq!(
        check.select_property(|i| [4, 4][i]),
        0,
        "ties go to the lowest index"
    );

    let single = ConstraintDef {
        name: "p_a".into(),
        relationship: false,
        label: "P".into(),
        properties: vec!["a".into()],
        kind: "unique".into(),
        property_type: None,
    };
    let check = ConstraintCheck::new(&single, None).unwrap();
    assert_eq!(
        check.select_property(|_| panic!("a single-property constraint consults no lookup")),
        0
    );
}

/// ADR-0173: the cursor advances only after a slice validated `Ok`, so a
/// caller that swallowed the first error and validated the same scratch
/// again still sees the violation.
#[test]
fn a_failed_validation_leaves_its_slice_unvalidated() {
    let g = Graph::default();
    exec_autocommit(&g, U).unwrap();
    exec_autocommit(&g, "CREATE (:P {id: 1})").unwrap();
    let scratch = write_scratch(&g);
    exec_write_unvalidated(&g, &scratch, "CREATE (:P {id: 1})").unwrap();
    let view = ExecGraph::write(&g, &scratch);
    let uniqueness = "semantic error: ConstraintValidationFailed: `p_id` uniqueness";
    for attempt in 1..=2 {
        let error = validate_constraints(&view).unwrap_err();
        assert_eq!(error.to_string(), uniqueness, "attempt {attempt}");
    }
    // A passing slice is marked: the next validation has nothing to check.
    let scratch = write_scratch(&g);
    exec_write_unvalidated(&g, &scratch, "CREATE (:P {id: 2})").unwrap();
    let view = ExecGraph::write(&g, &scratch);
    validate_constraints(&view).unwrap();
    take_constraint_inspects();
    validate_constraints(&view).unwrap();
    assert_eq!(take_constraint_inspects(), 0);
}

/// ADR-0174: a constraint another session commits after the scratch last
/// validated joins the recheck set at the next validation, so the open
/// transaction's earlier writes are checked against it — the full scan's
/// mid-transaction behavior.
#[test]
fn a_constraint_committed_mid_transaction_joins_the_recheck_set() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    exec_write(&g, &scratch, "CREATE (:P {id: 1}), (:P {id: 1})").unwrap();
    // Another session commits the constraint.
    exec_autocommit(&g, U).unwrap();
    let error = exec_write(&g, &scratch, "CREATE (:Other)")
        .err()
        .expect("the next statement re-checks the new constraint");
    assert_eq!(
        error.to_string(),
        "semantic error: ConstraintValidationFailed: `p_id` uniqueness"
    );
    // A constraint the earlier writes satisfy passes the same recheck.
    let g = Graph::default();
    let scratch = write_scratch(&g);
    exec_write(&g, &scratch, "CREATE (:P {id: 1}), (:P {id: 2})").unwrap();
    exec_autocommit(&g, U).unwrap();
    exec_write(&g, &scratch, "CREATE (:Other)").unwrap();
}

/// ADR-0174: a constraint dropped and re-created under the same name with a
/// different definition is *changed*, so it joins the recheck set too.
#[test]
fn a_constraint_changed_mid_transaction_joins_the_recheck_set() {
    let g = Graph::default();
    exec_autocommit(&g, "CREATE CONSTRAINT p_x FOR (n:P) REQUIRE n.x IS UNIQUE").unwrap();
    let scratch = write_scratch(&g);
    exec_write(&g, &scratch, "CREATE (:P {id: 1}), (:P {id: 1})").unwrap();
    // Another session replaces the constraint with a same-named one on `id`.
    exec_autocommit(&g, "DROP CONSTRAINT p_x").unwrap();
    exec_autocommit(&g, "CREATE CONSTRAINT p_x FOR (n:P) REQUIRE n.id IS UNIQUE").unwrap();
    let error = exec_write(&g, &scratch, "CREATE (:Other)")
        .err()
        .expect("the next statement re-checks the changed constraint");
    assert_eq!(
        error.to_string(),
        "semantic error: ConstraintValidationFailed: `p_x` uniqueness"
    );
}

/// ADR-0174: the catalog diff runs even when the slice is empty, the way
/// the full scan also ran after a transaction's read-only statements.
#[test]
fn a_committed_catalog_change_is_rechecked_without_new_writes() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    exec_write(&g, &scratch, "CREATE (:P {id: 1}), (:P {id: 1})").unwrap();
    exec_autocommit(&g, U).unwrap();
    let view = ExecGraph::write(&g, &scratch);
    let error =
        validate_constraints(&view).expect_err("an empty slice still re-checks a new constraint");
    assert_eq!(
        error.to_string(),
        "semantic error: ConstraintValidationFailed: `p_id` uniqueness"
    );
}

/// ADR-0174: an unchanged catalog re-checks nothing — a validation with an
/// empty slice inspects no entity once the fingerprint is remembered.
#[test]
fn an_unchanged_catalog_rechecks_nothing() {
    let g = Graph::default();
    exec_autocommit(&g, U).unwrap();
    let scratch = write_scratch(&g);
    exec_write(&g, &scratch, "CREATE (:P {id: 1})").unwrap();
    let view = ExecGraph::write(&g, &scratch);
    take_constraint_inspects();
    validate_constraints(&view).unwrap();
    assert_eq!(take_constraint_inspects(), 0);
}

/// ADR-0174/ADR-0111: the recheck loop samples the statement's cancellation
/// token per entity, so a cancelled token stops a full re-validation with
/// the query-cancelled error.
#[test]
fn a_cancelled_recheck_surfaces_query_cancelled() {
    let g = Graph::default();
    // More entities than the cancellation sampling window (1024), so the
    // pre-cancelled token is observed inside the recheck loop.
    for i in 0..1100 {
        seed_node(&g, &["P"], &[("id", Value::Int(i))]);
    }
    let scratch = write_scratch(&g);
    exec_write_unvalidated(&g, &scratch, U).unwrap();
    let token = cypherlite_core::CancelToken::new();
    token.cancel();
    let view = ExecGraph::write(&g, &scratch).with_cancel(Some(&token));
    let error = validate_constraints(&view).unwrap_err();
    assert!(
        error.to_string().contains("query cancelled"),
        "expected the cancellation error, got {error}"
    );
}

/// A stored kind tag `ConstraintType` rejects fails the validation closed
/// instead of leaving the constraint neither required nor unique.
#[test]
fn an_unknown_constraint_kind_fails_closed() {
    let def = ConstraintDef {
        name: "p_id".into(),
        relationship: false,
        label: "P".into(),
        properties: vec!["id".into()],
        kind: "uniqueish".into(),
        property_type: None,
    };
    let Err(error) = ConstraintCheck::new(&def, None) else {
        panic!("an unknown kind tag must not build a check");
    };
    assert_eq!(
        error.to_string(),
        "semantic error: ConstraintValidationFailed: `p_id` unknown constraint type \"uniqueish\""
    );
}

/// Parse a canonical rendering once, then match typed — the same
/// path `validate_constraints` takes per constraint.
fn matches(value: &Value, ty: &str) -> bool {
    crate::eval::value_matches_type(value, &parse_constraint_type(ty).unwrap())
}

#[test]
fn constraint_type_matching_is_typed_via_the_canonical_bridge() {
    assert!(matches(&Value::Int(1), "INTEGER"));
    assert!(!matches(&Value::Int(1), "STRING"));
    assert!(matches(&Value::Int(1), "INTEGER | STRING"));
    assert!(matches(&Value::String("a".into()), "INTEGER | STRING"));
    assert!(!matches(&Value::Bool(true), "INTEGER | STRING"));
    // A union member that is itself a list type.
    assert!(matches(
        &Value::List(vec![Value::Int(1)].into()),
        "LIST<INTEGER> | STRING"
    ));
    assert!(!matches(
        &Value::List(vec![Value::String("a".into())].into()),
        "LIST<INTEGER> | STRING"
    ));
    // List elements are checked against the element type.
    assert!(matches(
        &Value::List(vec![Value::Int(1)].into()),
        "LIST<INTEGER>"
    ));
    assert!(!matches(
        &Value::List(vec![Value::Bool(true)].into()),
        "LIST<INTEGER>"
    ));
    // NOT NULL / nullability.
    assert!(!matches(&Value::Null, "INTEGER NOT NULL"));
    assert!(matches(&Value::Null, "INTEGER"));
    assert!(!matches(&Value::Null, "NOTHING"));
    // UUID round-trips through the canonical bridge.
    assert!(matches(&Value::Uuid(0), "UUID"));
    assert!(!matches(&Value::Int(1), "UUID"));
}

#[test]
fn unparseable_type_strings_fail_closed() {
    // Union-in-list is rejected at creation (CIP-85), so no stored
    // string has this shape; if one ever appears, the parse fails and
    // `validate_constraints` fails the constraint closed, never
    // silently accepts.
    assert!(parse_constraint_type("LIST<INTEGER | STRING>").is_none());
    assert!(parse_constraint_type("NOT A TYPE").is_none());
}

#[test]
fn type_expr_name_union_rendering_round_trips_the_canonical_bridge() {
    let union = cypherlite_ir::TypeExpr::Union(vec![
        cypherlite_ir::TypeExpr::Predefined {
            kind: cypherlite_ir::PredefinedType::Integer,
            not_null: false,
        },
        cypherlite_ir::TypeExpr::Predefined {
            kind: cypherlite_ir::PredefinedType::String,
            not_null: false,
        },
    ]);
    let rendered = type_expr_name(&union);
    assert_eq!(rendered, "INTEGER | STRING");
    assert!(matches(&Value::Int(1), &rendered));
    assert!(matches(&Value::String("a".into()), &rendered));
}
