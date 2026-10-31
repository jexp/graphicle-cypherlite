//! The typed plan-warning tests (typed node warnings): the
//! `CartesianProduct`, `AllNodesScan` (with its suggested property index),
//! and PROFILE `EstimationMiss` warnings — the tree's typed payload, and
//! the wire form's name + payload serialization. The rendered `Warnings`
//! column and footer line are the boundary's — pinned in
//! `cypherlite-formatter`'s plan-text tests (ADR-0359).

mod common;

use common::{connection, find_node, has_notification, plan};
use cypherlite_model::wire::NotificationKind;

/// A multi-pattern `MATCH` whose patterns share no join anchor: the
/// affected operator carries the typed `CartesianProduct` warning.
#[test]
fn explain_warns_a_cartesian_product_on_a_disjoint_multi_pattern_match() {
    let mut conn = connection();
    let result = conn
        .query("EXPLAIN MATCH (a:Person), (b:City) RETURN a.name, b.name")
        .unwrap();
    let tree = plan(&result);
    let affected = find_node(tree, "Expand").expect("the multi-pattern MATCH is an Expand");
    assert_eq!(
        affected.warnings,
        [cypherlite::PlanWarning::CartesianProduct],
        "the disjoint patterns multiply instead of joining: {affected:?}",
    );
    // A shared node variable joins the patterns: no warning anywhere.
    let anchored = conn
        .query("EXPLAIN MATCH (a:Person)-[:KNOWS]->(b:Person), (b:Person)-[:LIKES]->(c:Person) RETURN a.name")
        .unwrap();
    let anchored_tree = plan(&anchored);
    assert!(
        !anchored_tree
            .children
            .iter()
            .any(|child| !child.warnings.is_empty()),
        "an anchored multi-pattern MATCH carries no warning: {anchored_tree:?}",
    );
}

/// A label-less scan while the store carries a property index: the affected
/// `AllNodesScan` operator carries the typed `AllNodesScan` warning naming
/// the existing index.
#[test]
fn explain_warns_an_all_nodes_scan_when_a_property_index_exists() {
    let mut conn = connection();
    conn.query("CREATE INDEX person_name FOR (n:Person) ON (n.name)")
        .unwrap();
    let result = conn.query("EXPLAIN MATCH (n) RETURN n").unwrap();
    let tree = plan(&result);
    let affected = find_node(tree, "AllNodesScan").expect("the label-less scan");
    assert_eq!(
        affected.warnings,
        [cypherlite::PlanWarning::AllNodesScan {
            index: ":Person(name)".to_string(),
        }],
        "the suggestion names the existing property index: {affected:?}",
    );

    // A labeled scan on the same store: the warning is the label-less
    // scan's, never the labeled scan's.
    let labeled = conn.query("EXPLAIN MATCH (n:Person) RETURN n").unwrap();
    let labeled_tree = plan(&labeled);
    let labeled_scan = find_node(labeled_tree, "NodeByLabelScan").expect("the labeled scan");
    assert!(
        labeled_scan.warnings.is_empty(),
        "a labeled scan carries no warning: {labeled_scan:?}",
    );
}

/// A PROFILE estimate overshoot: the same 10x policy the rendered ratio
/// cell's `!` marker reads derives the typed `EstimationMiss` warning on
/// the affected operator — and only there, never on the matching
/// estimate.
#[test]
fn profile_warns_an_estimation_miss_on_a_ten_x_estimate_overshoot() {
    let mut conn = connection();
    conn.query("UNWIND range(1, 25) AS i CREATE (:Person {flag: i = 1})")
        .unwrap();
    let result = conn
        .query("PROFILE MATCH (n:Person) WHERE n.flag RETURN n")
        .unwrap();
    let tree = plan(&result);
    let affected = find_node(tree, "NodeByLabelScan").expect("the filtered scan");
    assert_eq!(
        affected.estimated_rows,
        Some(25),
        "the estimate is the label count: {affected:?}",
    );
    assert_eq!(
        affected.warnings,
        [cypherlite::PlanWarning::EstimationMiss],
        "the estimate (25) overshoots the actual rows (1): {affected:?}",
    );
    // A matching estimate: every person, no filter — no warning.
    let matched = conn.query("PROFILE MATCH (n:Person) RETURN n").unwrap();
    let matched_tree = plan(&matched);
    assert!(
        find_node(matched_tree, "NodeByLabelScan")
            .expect("the scan")
            .warnings
            .is_empty(),
        "a matching estimate carries no warning"
    );
}

/// ADR-0363 decisions 4 and 8: `MissingIndex` is an operator-scoped
/// notification, not a `PlanWarning` — an EXPLAIN tree carrying one still
/// carries only the plan-warning kinds in its typed `warnings`, so the
/// boundary's `Warnings` column and footer never render it. The statement
/// here also carries the cartesian warning, so the plan-warning path is
/// populated; the missing-index kind must stay out of it. The plan table's
/// text rendering is pinned in `cypherlite-formatter`.
#[test]
fn missing_index_notification_never_joins_the_plan_warnings_column() {
    let mut conn = connection();
    conn.query("CREATE (:Person {name: 'ada'})").unwrap();
    conn.query("CREATE (:City)").unwrap();
    let result = conn
        .query("EXPLAIN MATCH (a:Person), (b:City) WHERE a.name = 'ada' RETURN a, b")
        .unwrap();
    assert!(
        has_notification(&result, NotificationKind::MissingIndex),
        "the label/property equality has no range index: {:?}",
        result.notifications()
    );
    let tree = plan(&result);
    let affected = find_node(tree, "Expand").expect("the multi-pattern MATCH is an Expand");
    assert_eq!(
        affected.warnings,
        [cypherlite::PlanWarning::CartesianProduct],
        "the Warnings column carries the plan-warning kinds only: {affected:?}"
    );
    assert!(
        !NotificationKind::MissingIndex.is_plan_warning(),
        "the notification kind is outside the plan-warning partition, so the \
         plan table's Warnings column never renders it"
    );
}
