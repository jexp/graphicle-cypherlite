//! The agent-native plan projection (`cypherlite.plan/v1`, LABS-370): the
//! engine's plan wire form projected onto the stable, snake_case schema
//! our own bindings hand out — real numbers, no `args` nesting, warnings
//! carrying the engine's remedy-bearing notifications.
//!
//! This is *our* API dialect, deliberately distinct from the Neo4j-facing
//! projection the Bolt and HTTP surfaces emit (ADR-0357, ADR-0365), which
//! mirrors Neo4j's own node shape for its tooling. Both read the same
//! engine wire form through the one typed accessor
//! ([`plan_node_wire`](crate::wire::plan_node_wire)); neither re-matches
//! the raw map.
//!
//! Schema stability: the document carries its `schema` identifier
//! (`cypherlite.plan/v1`); a breaking shape change bumps the version and
//! ships its own ADR. Fields are added, never renamed in place.

use serde_json::{json, Map, Value};

use super::notification_wire::notification_fields;
use crate::wire::Notification;
use crate::wire::{plan_node_wire, PlanMetricsWire, PlanNodeWire};
use crate::CypherValue;

/// The schema identifier every projected document carries.
pub const SCHEMA: &str = "cypherlite.plan/v1";

/// Project a plan wire tree (the engine's `PlanNode::to_value()` form)
/// into the agent-native document, as a `serde_json::Value` — the surface
/// serializes it, exactly as it does the sibling wire documents
/// (`stats_to_wire_json`, `notifications_to_wire_json`).
///
/// `statement` is the planned statement as echoed (`None` for a
/// hand-built tree); `notifications` is the result's remedy-bearing
/// notification list (ADR-0363), joined into each node's `warnings` by the
/// operator id the engine stamps — so an agent reads the diagnosis and the
/// action beside it. `profiled` is derived from the walked tree itself —
/// any node carrying runtime metrics makes it a `PROFILE` tree — so the
/// top-level flag can never contradict the per-node `actual` objects.
///
/// # Panics
///
/// Panics when the value is not a plan wire tree — the same contract
/// [`plan_node_wire`] enforces (a wire break is an engine bug, never
/// silently dropped) — and when an operator-scoped notification stamps an
/// id no walked node carries, so a warning can never vanish from the
/// document.
pub fn plan_document(
    tree: &CypherValue,
    statement: Option<&str>,
    notifications: &[Notification],
) -> Value {
    // The subtree totals, one pass over the tree (the one map every
    // node's `total_time_ms` reads — the same spelling the table's walk
    // uses, never a second fold).
    let totals = crate::wire::plan_subtree_totals(tree);
    // The flat notification list, indexed by operator id in one pass: each
    // node's `warnings` is then a lookup, never a re-scan of the whole list
    // per node.
    let warnings_by_operator = operator_warnings(notifications);
    let operator_scoped = warnings_by_operator.values().map(Vec::len).sum::<usize>();
    // The one walk: `node_json` validates each node once (through the
    // typed accessor) and reports whether its subtree carries runtime
    // metrics, which is exactly `profiled` — never a second pass and never
    // a caller-supplied flag that could disagree with the `actual` objects.
    // It also counts the notifications that joined a node.
    let mut joined = 0;
    let (root, profiled) = node_json(
        &plan_node_wire(tree),
        &totals,
        &warnings_by_operator,
        &mut joined,
    );
    // Every operator-scoped notification belongs to a node of the walked
    // tree; a stamp that matches no node (or matches two) is a
    // wire-contract break that fails loudly, never a warning silently
    // dropped from the document.
    assert_eq!(
        joined, operator_scoped,
        "every operator-scoped notification joins exactly one plan node (ADR-0363, ADR-0381): \
         {operator_scoped} stamped, {joined} joined"
    );
    json!({
        "schema": SCHEMA,
        "profiled": profiled,
        "statement": statement,
        "root": root,
    })
}

/// The flat notification list grouped by the operator id each entry
/// stamps — built in one pass, the index every node's `warnings` lookup
/// reads. Statement-scoped entries (no operator id) have no node to join.
fn operator_warnings(notifications: &[Notification]) -> crate::IndexMap<usize, Vec<&Notification>> {
    let mut by_operator: crate::IndexMap<usize, Vec<&Notification>> = crate::IndexMap::new();
    for notification in notifications {
        if let Some(operator_id) = notification.operator_id() {
            by_operator
                .entry(operator_id)
                .or_default()
                .push(notification);
        }
    }
    by_operator
}

/// One operator node's document: the display facts, the estimate, the
/// runtime metrics when the tree is profiled, the typed warnings, and the
/// children — each read from the validated typed view, never a raw match.
/// One node's document plus whether its subtree carries runtime metrics
/// (the fact `profiled` is derived from, reported by the same walk).
fn node_json(
    node: &PlanNodeWire<'_>,
    totals: &crate::IndexMap<i64, i64>,
    warnings_by_operator: &crate::IndexMap<usize, Vec<&Notification>>,
    joined: &mut usize,
) -> (Value, bool) {
    // The children walk first: each child reports whether its subtree
    // carries runtime metrics (the fact `profiled` derives from), by the
    // same single walk.
    let mut profiled = node.metrics.is_some();
    let children: Vec<Value> = node
        .children
        .iter()
        .map(|child| {
            let (child_json, child_profiled) =
                node_json(&plan_node_wire(child), totals, warnings_by_operator, joined);
            profiled |= child_profiled;
            child_json
        })
        .collect();
    // LABS-448: the arm's iteration count — the input clause's rows (the
    // child operator's `Rows`, the number the table shows one cell up),
    // named here for consumers that do not walk children. `None` when the
    // node has no input child (the statement's seed row).
    let loops = node.children.first().and_then(|first| {
        plan_node_wire(first)
            .metrics
            .map(|m| crate::wire::metric_u64(m.rows))
    });
    let mut out = Map::new();
    out.insert("operator".to_string(), json!(node.operator_type));
    out.insert("details".to_string(), json!(node.details));
    out.insert(
        "identifiers".to_string(),
        Value::Array(node.identifiers.iter().map(|name| json!(name)).collect()),
    );
    out.insert("estimated_rows".to_string(), json!(node.estimated_rows));
    out.insert(
        "actual".to_string(),
        match &node.metrics {
            Some(metrics) => {
                // The bundle — the one pairing helper in wire, the same
                // spelling the table's walk reads, never a second fold or
                // a re-stated contract here.
                let facts = crate::wire::profiled_facts(*metrics, totals, node.id);
                actual_json(&facts.metrics, facts.total_time_ns, loops)
            }
            None => Value::Null,
        },
    );
    out.insert("children".to_string(), Value::Array(children));
    // The operator's warnings: the remedy-bearing notifications raised for
    // it (ADR-0363), looked up by the operator id the engine stamps — the
    // diagnosis and the actionable remedy beside it, which is what makes
    // the document agent-actionable. Each warning is the canonical
    // notification projection minus the join key the node already scopes,
    // so the two spellings cannot drift. Always present, `[]` when none:
    // one absence spelling in the document.
    //
    // The id is widened once, through the typed accessor: a malformed id
    // fails there even when the result carries no notification to join.
    let operator_id = node.operator_id();
    let warnings = warnings_by_operator
        .get(&operator_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    *joined += warnings.len();
    out.insert(
        "warnings".to_string(),
        Value::Array(
            warnings
                .iter()
                .map(|notification| Value::Object(notification_fields(notification)))
                .collect(),
        ),
    );
    (Value::Object(out), profiled)
}

/// The runtime metrics of a profiled node: rows, the operator's own time
/// in milliseconds (through the wire's one conversion), the subtree total
/// of that time (cumulative vs self — the node plus every descendant, the
/// cost of the sub-pipeline it heads), store accesses, the read-kind mix,
/// and the walk's chosen start when the operator walked one.
fn actual_json(metrics: &PlanMetricsWire, subtree_ns: i64, loops: Option<u64>) -> Value {
    let mut out = Map::new();
    out.insert("rows".to_string(), json!(metrics.rows));
    out.insert(
        "time_ms".to_string(),
        json!(crate::wire::time_ns_ms(metrics.time_ns)),
    );
    out.insert(
        "total_time_ms".to_string(),
        json!(crate::wire::time_ns_ms(subtree_ns)),
    );
    out.insert("db_hits".to_string(), json!(metrics.db_hits));
    // LABS-448: the phase times behind the read kinds — the seek (the
    // candidate resolution and admission `node_reads` counts) and the walk
    // (the relationship traversal `hops`/`neighbor_walks` count),
    // inclusive of a multi-element pattern's nested phases. The kinds
    // themselves stay where they are; the phases add only the time
    // attribution, never a second spelling of a count.
    out.insert(
        "seek_ms".to_string(),
        json!(crate::wire::time_ns_ms(metrics.phases.seek_ns)),
    );
    out.insert(
        "walk_ms".to_string(),
        json!(crate::wire::time_ns_ms(metrics.phases.walk_ns)),
    );
    // LABS-448: the arm's iteration count — always present, `null` when
    // the node has no input child (the seed row), the one absence
    // spelling the document uses.
    out.insert("loops".to_string(), json!(loops));
    out.insert("memory_bytes".to_string(), json!(metrics.memory_bytes));
    out.insert("hops".to_string(), json!(metrics.reads.hops));
    out.insert(
        "neighbor_walks".to_string(),
        json!(metrics.reads.neighbor_walks),
    );
    out.insert("node_reads".to_string(), json!(metrics.reads.node_reads));
    // Always present, `null` when the operator walked no pattern: the one
    // absence spelling the document uses (a consumer never faces a
    // missing-key rule).
    out.insert(
        "start".to_string(),
        match &metrics.start {
            Some(start) => json!({
                "element": start.element,
                "backward": start.backward,
                "reason": start.reason,
            }),
            None => Value::Null,
        },
    );
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::plan_test_fixtures::plan_wire_node;

    #[test]
    fn actual_carries_the_subtree_total() {
        let scan = plan_wire_node("NodeByLabelScan", 1, Some(3_000_000), vec![]);
        let root = plan_wire_node("ProduceResults", 0, Some(10_000_000), vec![scan]);
        let doc = plan_document(&root, Some("MATCH (n) RETURN n"), &[]);
        assert_eq!(doc["profiled"], true);
        assert_eq!(
            doc["root"]["actual"]["time_ms"], 10.0,
            "the root's own time renders in milliseconds"
        );
        assert_eq!(
            doc["root"]["actual"]["total_time_ms"], 13.0,
            "the root's total covers itself plus the scan: 10 + 3 = 13 ms"
        );
        let child = &doc["root"]["children"][0];
        assert_eq!(
            child["actual"]["total_time_ms"], 3.0,
            "a leaf's total equals its own time"
        );
        // An unprofiled tree: no actual, no fabricated total.
        let explain = plan_document(
            &plan_wire_node("ProduceResults", 0, None, vec![]),
            Some("RETURN 1"),
            &[],
        );
        assert!(explain["root"]["actual"].is_null());
    }

    /// The phases and loops (LABS-448): `seek_ms`/`walk_ms` render the
    /// phase times beside the kinds they attribute (the kinds stay where
    /// they are — the phases add only the time, never a second count),
    /// and `loops` names the input clause's rows for consumers that do
    /// not walk children (`null` at the root, the seed row).
    #[test]
    fn actual_carries_the_phase_times_and_loops() {
        let scan = plan_wire_node("NodeByLabelScan", 1, Some(3_000_000), vec![]);
        let root = plan_wire_node("ProduceResults", 0, Some(10_000_000), vec![scan]);
        let doc = plan_document(&root, Some("MATCH (n) RETURN n"), &[]);
        // The fixture emits zero phases: the fields render, always
        // present, zeros rather than absences.
        assert_eq!(doc["root"]["actual"]["seek_ms"], 0.0);
        assert_eq!(doc["root"]["actual"]["walk_ms"], 0.0);
        // The root's loops is its input child's rows (the scan produced
        // one); the scan itself has no input child (the seed row), so its
        // loops is null — the one absence spelling.
        assert_eq!(doc["root"]["actual"]["loops"], 1, "the scan's rows");
        let child = &doc["root"]["children"][0];
        assert!(child["actual"]["loops"].is_null());
    }

    #[test]
    fn projects_the_document_shape() {
        let doc = plan_document(
            &plan_wire_node("ProduceResults", 0, None, vec![]),
            Some("RETURN 1"),
            &[],
        );
        assert_eq!(doc["schema"], SCHEMA);
        assert_eq!(doc["profiled"], false);
        assert_eq!(doc["root"]["operator"], "ProduceResults");
        assert_eq!(doc["root"]["identifiers"][0], "name");
        assert!(
            doc["root"]["warnings"].is_array(),
            "warnings always present"
        );
        assert!(doc["root"]["actual"].is_null());
    }

    #[test]
    fn node_warnings_are_the_canonical_notification_projection() {
        use crate::wire::{notification_keys, notification_to_wire_json, NotificationKind};

        let notification =
            Notification::new(NotificationKind::CartesianProduct, "no join predicate")
                .with_operator_id(1);
        let tree = plan_wire_node(
            "ProduceResults",
            0,
            None,
            vec![plan_wire_node("CartesianProduct", 1, None, vec![])],
        );
        let doc = plan_document(
            &tree,
            Some("MATCH (a), (b) RETURN a, b"),
            std::slice::from_ref(&notification),
        );

        let warning = &doc["root"]["children"][0]["warnings"][0];
        let mut canonical = notification_to_wire_json(&notification);
        canonical
            .as_object_mut()
            .expect("the canonical notification is an object")
            .remove(notification_keys::OPERATOR_ID);
        assert_eq!(
            *warning, canonical,
            "the node warning is the canonical notification object minus the join key"
        );
        assert!(
            warning.get(notification_keys::OPERATOR_ID).is_none(),
            "the node scopes the operator, so the join key is not repeated"
        );
        assert_eq!(doc["root"]["warnings"], json!([]));
    }

    #[test]
    #[should_panic(expected = "plan wire Id is non-negative")]
    fn negative_operator_id_is_a_loud_wire_break() {
        let tree = plan_wire_node("ProduceResults", -1, None, vec![]);
        let _ = plan_document(&tree, Some("RETURN 1"), &[]);
    }

    #[test]
    #[should_panic(expected = "every operator-scoped notification joins exactly one plan node")]
    fn an_unjoined_operator_notification_is_a_loud_wire_break() {
        use crate::wire::NotificationKind;

        let notification =
            Notification::new(NotificationKind::CartesianProduct, "no join predicate")
                .with_operator_id(7);
        let tree = plan_wire_node("ProduceResults", 0, None, vec![]);
        let _ = plan_document(&tree, Some("RETURN 1"), &[notification]);
    }
}
