//! Query notifications (ADR-0363): the engine-native list beside
//! `QueryResult.plan`, the operator-scoped kinds the tree carries —
//! `CartesianProduct`, `UnboundedVariableLengthPattern`, `AllNodesScan`,
//! `MissingIndex`, `EagerAggregation` and the `PROFILE`-only
//! `EstimationMiss` — the plain-run empty list, and the shared success
//! envelope's always-present `notifications` array.
//!
//! The statement-scoped kinds — `UnknownLabelWarning`,
//! `UnknownRelationshipTypeWarning`, `UnknownPropertyKeyWarning`,
//! `DeprecatedFunction`, `DeprecatedProcedure` and
//! `CallSubqueryWithoutScope` — are pinned in `statement_notifications.rs`;
//! this file covers the tree-derived half of the twelve-kind list.

mod common;

use common::{connection, find_node, has_notification, params, two_label_store};
use cypherlite::Database;
use cypherlite_model::wire::NotificationKind;

/// The EXPLAIN cartesian statement: a multi-pattern MATCH whose patterns
/// share no node variable.
const CARTESIAN_STATEMENT: &str = "MATCH (a:Person), (b:City) RETURN a, b";

/// The first `MissingIndex` notification's runnable remedy.
fn missing_index_remedy(result: &cypherlite::QueryResult) -> &str {
    result
        .notifications()
        .iter()
        .find(|notification| notification.kind() == NotificationKind::MissingIndex)
        .expect("the statement has a MissingIndex notification")
        .remedy()
}

/// ADR-0363: the disjoint multi-pattern MATCH's `CartesianProduct` plan
/// warning projects to exactly one notification, carrying the vocabulary's
/// code and metadata and the raising operator's id; the same statement
/// without EXPLAIN builds no tree and returns an empty list.
#[test]
fn explain_cartesian_product_emits_one_notification_and_a_plain_run_none() {
    let mut conn = two_label_store();
    let explained = conn
        .query(&format!("EXPLAIN {CARTESIAN_STATEMENT}"))
        .unwrap();
    assert_eq!(
        explained.notifications().len(),
        1,
        "one cartesian warning, one notification: {:?}",
        explained.notifications()
    );
    let notification = &explained.notifications()[0];
    assert_eq!(
        notification.code(),
        "Neo.ClientNotification.Statement.CartesianProduct"
    );
    assert_eq!(notification.gql_status(), "03N90");
    assert_eq!(notification.severity().as_str(), "INFORMATION");
    assert_eq!(notification.category().as_str(), "PERFORMANCE");
    assert_eq!(notification.title(), "Cartesian product");
    assert_eq!(
        notification.description(),
        "the MATCH patterns share no join anchor"
    );
    assert!(
        notification.remedy().contains("join anchor"),
        "the canonical remedy names the join anchor: {}",
        notification.remedy()
    );
    assert!(
        notification.operator_id().is_some(),
        "the cartesian kind is operator-scoped"
    );
    // The operator id is the tree's own id, not a second numbering.
    let tree = common::plan(&explained);
    assert!(
        tree.children
            .iter()
            .any(|child| child.warnings.iter().any(|warning| {
                warning.name() == "CartesianProduct" && Some(child.id) == notification.operator_id()
            })),
        "the notification names the warned operator's id: {tree:?}"
    );

    let plain = conn.query(CARTESIAN_STATEMENT).unwrap();
    assert!(
        plain.notifications().is_empty(),
        "a plain run builds no plan tree and derives no notifications"
    );
}

/// ADR-0363 decision 4: the Cartesian remedy names actions the detector can
/// observe — a shared node variable or a joining pattern. Applying the
/// recommended shared variable clears the notification on the next EXPLAIN.
#[test]
fn cartesian_remedy_applied_clears_the_notification() {
    let mut conn = two_label_store();
    let explained = conn
        .query(&format!("EXPLAIN {CARTESIAN_STATEMENT}"))
        .unwrap();
    let cartesian = explained
        .notifications()
        .iter()
        .find(|notification| notification.kind() == NotificationKind::CartesianProduct)
        .expect("the cartesian statement notifies");
    let remedy = cartesian.remedy();
    assert!(
        remedy.contains("reuse a node variable"),
        "the remedy names the shared-variable fix: {remedy}"
    );
    assert!(
        !remedy.contains("WHERE"),
        "the remedy no longer recommends a cross-pattern WHERE equality, which the detector cannot observe: {remedy}"
    );

    // Apply the documented fix: the same node variable joins both patterns.
    let fixed = conn
        .query("EXPLAIN MATCH (a:Person), (a:City) RETURN a")
        .unwrap();
    assert!(
        !has_notification(&fixed, NotificationKind::CartesianProduct),
        "the applied remedy clears the warning: {:?}",
        fixed.notifications()
    );

    // A joining relationship between the endpoints is the other documented
    // fix and clears it too.
    let joined = conn
        .query("EXPLAIN MATCH (a:Person), (b:City), (a)-[:IN]->(b) RETURN a, b")
        .unwrap();
    assert!(
        !has_notification(&joined, NotificationKind::CartesianProduct),
        "a pattern joining the endpoints clears the warning: {:?}",
        joined.notifications()
    );
}

/// Case-variant labels are distinct, so their `CREATE INDEX` remedies must
/// get distinct names; both remedies execute and silence their own
/// statement.
#[test]
fn case_variant_index_remedies_are_distinct_and_both_execute() {
    let mut conn = connection();
    conn.query("CREATE (:Person {name: 'Ada'}), (:person {name: 'ada'})")
        .unwrap();
    let upper_statement = "EXPLAIN MATCH (n:Person) WHERE n.name = 'Ada' RETURN n";
    let lower_statement = "EXPLAIN MATCH (n:person) WHERE n.name = 'ada' RETURN n";

    let upper = conn.query(upper_statement).unwrap();
    let lower = conn.query(lower_statement).unwrap();
    let upper_remedy = missing_index_remedy(&upper);
    let lower_remedy = missing_index_remedy(&lower);
    assert_ne!(
        upper_remedy, lower_remedy,
        "case-variant labels must not propose the same index name"
    );
    assert_eq!(
        upper_remedy,
        "CREATE INDEX cypherlite_Person__name FOR (n:Person) ON (n.name)"
    );
    assert_eq!(
        lower_remedy,
        "CREATE INDEX cypherlite_person__name FOR (n:person) ON (n.name)"
    );

    for remedy in [upper_remedy, lower_remedy] {
        conn.query(remedy)
            .unwrap_or_else(|error| panic!("the remedy executes: {remedy}: {error}"));
    }
    for statement in [upper_statement, lower_statement] {
        let explained = conn.query(statement).unwrap();
        assert!(
            !has_notification(&explained, NotificationKind::MissingIndex),
            "the created index serves its own label: {statement}: {:?}",
            explained.notifications()
        );
    }
}

/// Distinct `(label, property)` pairs keep distinct global index names
/// through the injective byte escape and the `__` separator: the old
/// `_`-fold collapsed `A-B`/`A_B` and `Order Item`/`Order_Item`, and the
/// old single-`_` separator collapsed `(A_, x)`/`(A, _x)`. Each pair runs
/// in its own store so the first remedy cannot mask the second's
/// notification; each remedy executes verbatim and silences its own
/// EXPLAIN.
#[test]
fn escaped_index_remedy_names_are_injective_and_both_execute() {
    let pairs = [
        (
            "CREATE (:`A-B` {x: 1}), (:`A_B` {x: 1})",
            "EXPLAIN MATCH (n:`A-B`) WHERE n.x = 1 RETURN n",
            "CREATE INDEX cypherlite_A_2dB__x FOR (n:`A-B`) ON (n.x)",
            "EXPLAIN MATCH (n:`A_B`) WHERE n.x = 1 RETURN n",
            "CREATE INDEX cypherlite_A_5fB__x FOR (n:A_B) ON (n.x)",
        ),
        (
            "CREATE (:`Order Item` {`ship-date`: 'a'}), \
             (:`Order_Item` {`ship_date`: 'a'})",
            "EXPLAIN MATCH (n:`Order Item`) WHERE n.`ship-date` = 'a' RETURN n",
            "CREATE INDEX cypherlite_Order_20Item__ship_2ddate \
             FOR (n:`Order Item`) ON (n.`ship-date`)",
            "EXPLAIN MATCH (n:`Order_Item`) WHERE n.`ship_date` = 'a' RETURN n",
            "CREATE INDEX cypherlite_Order_5fItem__ship_5fdate \
             FOR (n:Order_Item) ON (n.ship_date)",
        ),
        (
            "CREATE (:`A_` {x: 1}), (:A {_x: 1})",
            "EXPLAIN MATCH (n:`A_`) WHERE n.x = 1 RETURN n",
            "CREATE INDEX cypherlite_A_5f__x FOR (n:A_) ON (n.x)",
            "EXPLAIN MATCH (n:A) WHERE n._x = 1 RETURN n",
            "CREATE INDEX cypherlite_A___5fx FOR (n:A) ON (n._x)",
        ),
        (
            "CREATE (:A {B_x: 2}), (:A_B {x: 3})",
            "EXPLAIN MATCH (n:A) WHERE n.B_x = 1 RETURN n",
            "CREATE INDEX cypherlite_A__B_5fx FOR (n:A) ON (n.B_x)",
            "EXPLAIN MATCH (n:A_B) WHERE n.x = 1 RETURN n",
            "CREATE INDEX cypherlite_A_5fB__x FOR (n:A_B) ON (n.x)",
        ),
    ];
    for (setup, first_statement, first_expected, second_statement, second_expected) in pairs {
        let mut conn = connection();
        conn.query(setup).unwrap();
        let first = conn.query(first_statement).unwrap();
        let second = conn.query(second_statement).unwrap();
        let first_remedy = missing_index_remedy(&first);
        let second_remedy = missing_index_remedy(&second);
        assert_ne!(
            first_remedy, second_remedy,
            "distinct (label, property) pairs must not propose the same global index name"
        );
        assert_eq!(first_remedy, first_expected);
        assert_eq!(second_remedy, second_expected);

        for remedy in [first_remedy, second_remedy] {
            conn.query(remedy)
                .unwrap_or_else(|error| panic!("the remedy executes: {remedy}: {error}"));
        }
        for statement in [first_statement, second_statement] {
            let explained = conn.query(statement).unwrap();
            assert!(
                !has_notification(&explained, NotificationKind::MissingIndex),
                "the created index serves its own pair: {statement}: {:?}",
                explained.notifications()
            );
        }
    }
}

/// ADR-0363: the label-less scan's `AllNodesScan` warning projects to its
/// `CypherLite.*` vocabulary row with the warning's message as the instance
/// description — the plan table's warning text and the notification list
/// cannot drift.
#[test]
fn explain_all_nodes_scan_projects_the_warning_text_to_a_notification() {
    let mut conn = connection();
    conn.query("CREATE INDEX person_name FOR (n:Person) ON (n.name)")
        .unwrap();
    let explained = conn.query("EXPLAIN MATCH (n) RETURN n").unwrap();
    assert_eq!(explained.notifications().len(), 1);
    let notification = &explained.notifications()[0];
    assert_eq!(
        notification.code(),
        "CypherLite.Notification.Statement.AllNodesScan"
    );
    assert_eq!(notification.gql_status(), "03C90");
    assert_eq!(notification.title(), "All nodes scan");
    assert_eq!(
        notification.description(),
        "the scan matches every node; consider the property index (:Person(name))"
    );
    let tree = common::plan(&explained);
    let scan = tree.children.first().expect("the scan operator");
    assert_eq!(
        notification.description(),
        scan.warnings[0].message(),
        "one text spelling for the plan table warning and the notification"
    );
}

/// ADR-0363: `EstimationMiss` is a PROFILE fact — the profiled statement's
/// 25-row estimate against one actual row returns the notification, while
/// the same statement under EXPLAIN (no actual rows) does not.
#[test]
fn profile_estimation_miss_emits_a_notification_and_explain_does_not() {
    let mut conn = connection();
    conn.query("UNWIND range(1, 25) AS i CREATE (:Person {flag: i = 1})")
        .unwrap();
    let statement = "MATCH (n:Person) WHERE n.flag RETURN n";
    let profiled = conn.query(&format!("PROFILE {statement}")).unwrap();
    assert!(
        has_notification(&profiled, NotificationKind::EstimationMiss),
        "the 25x overshoot projects to one EstimationMiss: {:?}",
        profiled.notifications()
    );
    assert_eq!(
        profiled.notifications().len(),
        1,
        "exactly one notification: {:?}",
        profiled.notifications()
    );
    let notification = &profiled.notifications()[0];
    assert_eq!(notification.gql_status(), "03C93");
    assert_eq!(notification.severity().as_str(), "INFORMATION");
    assert_eq!(notification.category().as_str(), "PERFORMANCE");
    assert!(notification.operator_id().is_some());

    let explained = conn.query(&format!("EXPLAIN {statement}")).unwrap();
    assert!(
        explained.notifications().is_empty(),
        "EXPLAIN has no actual rows, so the miss cannot fire"
    );
}

/// ADR-0363 decision 4: an unbounded variable-length pattern notifies on
/// the `Expand` operator it plans as, naming the pattern text; a bounded
/// quantifier does not fire.
#[test]
fn explain_unbounded_variable_length_pattern_names_the_expand_operator() {
    let mut conn = connection();
    conn.query("CREATE (:A)-[:R]->(:B)").unwrap();
    let explained = conn
        .query("EXPLAIN MATCH (a:A)-[:R*]->(b) RETURN b")
        .unwrap();
    let unbounded: Vec<&cypherlite::Notification> = explained
        .notifications()
        .iter()
        .filter(|notification| {
            notification.kind() == NotificationKind::UnboundedVariableLengthPattern
        })
        .collect();
    assert_eq!(
        unbounded.len(),
        1,
        "one unbounded pattern, one notification: {:?}",
        explained.notifications()
    );
    let notification = unbounded[0];
    assert_eq!(notification.gql_status(), "03N91");
    assert_eq!(notification.severity().as_str(), "INFORMATION");
    assert_eq!(notification.category().as_str(), "PERFORMANCE");
    assert!(
        notification.description().contains("(a:A)-[:R*1..]->(b)"),
        "the description names the instance pattern: {}",
        notification.description()
    );
    assert!(
        !notification.remedy().is_empty(),
        "every notification carries a remedy"
    );
    let tree = common::plan(&explained);
    let expand = find_node(tree, "Expand").expect("the variable-length pattern is an Expand");
    assert_eq!(
        notification.operator_id(),
        Some(expand.id),
        "the notification names the Expand operator's id: {tree:?}"
    );

    let bounded = conn
        .query("EXPLAIN MATCH (a:A)-[:R*1..3]->(b) RETURN b")
        .unwrap();
    assert!(
        !has_notification(&bounded, NotificationKind::UnboundedVariableLengthPattern),
        "a bounded expansion does not fire: {:?}",
        bounded.notifications()
    );
}

/// ADR-0363 decision 4: a label/property equality with no range index
/// notifies with the exact runnable `CREATE INDEX` as its instance remedy;
/// executing that statement verbatim silences the next EXPLAIN.
#[test]
fn explain_missing_index_remedy_is_runnable_and_silences_after_creation() {
    let mut conn = connection();
    conn.query("CREATE (:Person {name: 'ada'}), (:Person {name: 'bob'})")
        .unwrap();
    let statement = "EXPLAIN MATCH (n:Person) WHERE n.name = $x RETURN n";
    let params = params(&[("x", cypherlite::Value::String("ada".into()))]);
    let explained = conn.query_with_params(statement, params.clone()).unwrap();
    let missing: Vec<&cypherlite::Notification> = explained
        .notifications()
        .iter()
        .filter(|notification| notification.kind() == NotificationKind::MissingIndex)
        .collect();
    assert_eq!(
        missing.len(),
        1,
        "one unindexed equality pair, one notification: {:?}",
        explained.notifications()
    );
    let notification = missing[0];
    assert_eq!(notification.gql_status(), "03C91");
    assert!(
        notification.description().contains("Person")
            && notification.description().contains("name"),
        "the description names the label and property: {}",
        notification.description()
    );
    assert_eq!(
        notification.remedy(),
        "CREATE INDEX cypherlite_Person__name FOR (n:Person) ON (n.name)",
        "the instance remedy is the exact runnable statement"
    );
    let tree = common::plan(&explained);
    let scan = find_node(tree, "NodeByLabelScan").expect("the labeled scan");
    assert_eq!(notification.operator_id(), Some(scan.id));

    // The remedy executes verbatim; once the index exists the same EXPLAIN
    // derives no MissingIndex notification.
    conn.query(notification.remedy()).unwrap();
    let indexed = conn.query_with_params(statement, params).unwrap();
    assert!(
        !has_notification(&indexed, NotificationKind::MissingIndex),
        "the created range index serves the equality: {:?}",
        indexed.notifications()
    );
}

/// ADR-0363 decision 4: the missing-index remedy applies the parser's one
/// identifier-quoting policy, so a clause-keyword property is backtick-quoted.
/// `n.order` parses fine as a property name (any keyword is a `SchemaName`
/// after `.`, ADR-0169), but `CREATE INDEX ... ON (n.order)` reads the
/// property through `is_ident_like()` and rejects the clause keyword — the
/// unquoted remedy would not run. The quoted remedy executes verbatim and
/// silences the next EXPLAIN.
#[test]
fn explain_missing_index_remedy_quotes_keyword_property_and_is_runnable() {
    let mut conn = connection();
    conn.query("CREATE (:Person {`order`: 1}), (:Person {`order`: 2})")
        .unwrap();
    let statement = "EXPLAIN MATCH (n:Person) WHERE n.order = 1 RETURN n";
    let explained = conn.query(statement).unwrap();
    let missing: Vec<&cypherlite::Notification> = explained
        .notifications()
        .iter()
        .filter(|notification| notification.kind() == NotificationKind::MissingIndex)
        .collect();
    assert_eq!(
        missing.len(),
        1,
        "one unindexed equality pair, one notification: {:?}",
        explained.notifications()
    );
    assert_eq!(
        missing[0].remedy(),
        "CREATE INDEX cypherlite_Person__order FOR (n:Person) ON (n.`order`)",
        "a clause-keyword property is quoted so the remedy re-parses"
    );

    // Executing the remedy verbatim must parse (the unquoted form is a parse
    // error) and the created range index must silence the next EXPLAIN.
    conn.query(missing[0].remedy())
        .expect("the keyword-quoted remedy parses and executes");
    let indexed = conn.query(statement).unwrap();
    assert!(
        !has_notification(&indexed, NotificationKind::MissingIndex),
        "the created range index serves the equality: {:?}",
        indexed.notifications()
    );
}

/// A composite range index covers every component: the executor
/// materializes and seeks each pair, so an equality on either property is
/// served. A property outside the index still notifies.
#[test]
fn composite_range_index_serves_every_component() {
    let mut conn = connection();
    conn.query("CREATE (:Person {first: 'ada', last: 'lovelace', age: 36})")
        .unwrap();
    conn.query("CREATE INDEX person_name FOR (n:Person) ON (n.first, n.last)")
        .unwrap();

    for statement in [
        "EXPLAIN MATCH (n:Person) WHERE n.first = 'ada' RETURN n",
        "EXPLAIN MATCH (n:Person) WHERE n.last = 'lovelace' RETURN n",
    ] {
        let explained = conn.query(statement).unwrap();
        assert!(
            !has_notification(&explained, NotificationKind::MissingIndex),
            "every composite component is served: {statement}: {:?}",
            explained.notifications()
        );
    }

    let unindexed = conn
        .query("EXPLAIN MATCH (n:Person) WHERE n.age = 36 RETURN n")
        .unwrap();
    assert!(
        has_notification(&unindexed, NotificationKind::MissingIndex),
        "a property outside the composite index still notifies: {:?}",
        unindexed.notifications()
    );
}

/// A relationship range index is not a node index, even when its
/// relationship type happens to spell a node label.
#[test]
fn relationship_range_index_does_not_suppress_a_node_missing_index() {
    let mut conn = connection();
    conn.query("CREATE (:Person {x: 1})").unwrap();
    conn.query("CREATE INDEX rel_person_x FOR ()-[r:Person]-() ON (r.x)")
        .unwrap();
    let explained = conn
        .query("EXPLAIN MATCH (n:Person) WHERE n.x = 1 RETURN n")
        .unwrap();
    assert!(
        has_notification(&explained, NotificationKind::MissingIndex),
        "a relationship index never serves a node pair: {:?}",
        explained.notifications()
    );
}

/// A property name is one name: the typed coverage read never splits it on a
/// comma, so the real index suppresses the hint and its components do not.
#[test]
fn comma_bearing_property_name_is_not_split_by_index_coverage() {
    let mut conn = connection();
    conn.query("CREATE (:Order {`ship,date`: 1})").unwrap();
    conn.query("CREATE INDEX order_ship FOR (n:Order) ON (n.`ship,date`)")
        .unwrap();

    let whole = conn
        .query("EXPLAIN MATCH (n:Order) WHERE n.`ship,date` = 1 RETURN n")
        .unwrap();
    assert!(
        !has_notification(&whole, NotificationKind::MissingIndex),
        "the index serves the whole comma-bearing property name: {:?}",
        whole.notifications()
    );

    let component = conn
        .query("EXPLAIN MATCH (n:Order) WHERE n.ship = 1 RETURN n")
        .unwrap();
    assert!(
        has_notification(&component, NotificationKind::MissingIndex),
        "a comma inside a property name is not a component separator: {:?}",
        component.notifications()
    );
}

/// ADR-0363 decision 9: the shared success envelope always carries the
/// native `notifications` array — populated on EXPLAIN, empty on a plain
/// run — and its entries carry the native keys including `operator_id`.
#[test]
fn execute_wire_envelope_carries_the_native_notifications_array() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    // The cartesian statement's two labels exist, so the only notification
    // is the tree's cartesian warning (unknown-label warnings would
    // otherwise join the list beside it).
    conn.query("CREATE (:Person)").unwrap();
    conn.query("CREATE (:City)").unwrap();
    let explained =
        cypherlite::execute_wire(&mut conn, &format!("EXPLAIN {CARTESIAN_STATEMENT}"), None);
    let entries = explained["notifications"]
        .as_array()
        .expect("the envelope always carries a notifications array");
    assert_eq!(entries.len(), 1, "one cartesian notification: {explained}");
    let entry = &entries[0];
    assert_eq!(
        entry["code"],
        "Neo.ClientNotification.Statement.CartesianProduct"
    );
    assert_eq!(entry["gql_status"], "03N90");
    assert_eq!(entry["severity"], "INFORMATION");
    assert_eq!(entry["category"], "PERFORMANCE");
    assert_eq!(entry["title"], "Cartesian product");
    assert_eq!(
        entry["description"],
        "the MATCH patterns share no join anchor"
    );
    assert!(
        entry["remedy"]
            .as_str()
            .is_some_and(|r| r.contains("join anchor")),
        "the native form keeps the remedy: {entry}"
    );
    assert!(
        entry["operator_id"].is_number(),
        "the operator-scoped kind names its node: {entry}"
    );

    let plain = cypherlite::execute_wire(&mut conn, "RETURN 1 AS n", None);
    assert_eq!(
        plain["notifications"],
        serde_json::json!([]),
        "a plain run's envelope carries an empty array, never a missing key"
    );
    assert!(plain["error"].is_null());
}
