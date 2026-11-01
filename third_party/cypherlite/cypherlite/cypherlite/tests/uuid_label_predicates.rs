//! CIP-246 UUID-as-property-value semantics and CIP-14/249
//! label-expression negation, end to end (parse -> lower -> plan ->
//! execute).
//!
//! Both were result mismatches against the acceptance corpus: a UUID was
//! not recognised as a property value, and a negated conjunct nested in a
//! label conjunction was folded away by the MATCH rewrite. The UUID
//! comparison and ORDER BY contract is pinned in `comparison_semantics.rs`.

mod common;

use cypherlite::Value;

use common::{connection, strings};

/// A UUID is a storable property type, so `IS :: ANY PROPERTY VALUE` holds
/// for it alone and as a list element.
#[test]
fn uuid_is_any_property_value() {
    let mut c = connection();
    let r = c
        .query(
            "WITH UUID('f066ecfc-5151-4121-8a36-b35e00d72918') AS u \
             RETURN u IS :: ANY PROPERTY VALUE AS single, \
                    [u] IS :: ANY PROPERTY VALUE AS list, \
                    u IS :: UUID AS typed",
        )
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true)
        ]]
    );
}

/// The label-expression negation inside a conjunction is honoured: only
/// `(:A:B)` matches `(A&B)&!(B&C)`.
#[test]
fn negated_conjunct_in_a_label_expression_filters() {
    let mut c = connection();
    c.query("CREATE (:A:B), (:A:B:C), (:A:C), (:B:C), (:A), (:C)")
        .unwrap();

    for query in [
        "MATCH (n) WHERE n:(A&B)&!(B&C) RETURN labels(n) AS l",
        "MATCH (n) WHERE n IS LABELED (A&B)&!(B&C) RETURN labels(n) AS l",
    ] {
        let r = c.query(query).unwrap();
        assert_eq!(r.rows, vec![vec![strings(&["A", "B"])]], "{query}");
    }
}

/// A conjunct folded into a pattern must not replace the pattern's own
/// label expression: `(n:A|B)` plus `n:C` still requires the disjunction.
#[test]
fn folded_conjunct_keeps_a_pattern_label_disjunction() {
    let mut c = connection();
    c.query("CREATE (:A), (:B), (:C), (:A:C)").unwrap();

    let r = c
        .query("MATCH (n:A|B) WHERE n:C RETURN labels(n) AS l")
        .unwrap();
    assert_eq!(r.rows, vec![vec![strings(&["A", "C"])]]);
}
