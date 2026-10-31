//! Plan trees for `EXPLAIN`/`PROFILE` (ADR-0344/0345, ADR-0359):
//! `Connection::explain_plan` plans a statement without executing it and
//! returns the structured `PlanNode` tree; `PlanNode::to_value` is the
//! machine-native wire form and `PlanNode::to_neo4j_value` the
//! Neo4j-facing form (ADR-0357), which takes the plan text the *boundary*
//! rendered — the text rendering itself lives in `cypherlite-formatter`
//! (`cypherlite_formatter::plan_text_lines`, see that crate's
//! `examples/plan_text.rs`); the engine emits the tree only.
//!
//! Run with `cargo run -p cypherlite --example plan_tree`.

use cypherlite::{Database, Result};

/// The one statement literal both EXPLAIN sites use: the planned tree at
/// the top and the Neo4j-facing decoration below read the same text.
const STATEMENT: &str =
    "MATCH (p:Person)-[k:KNOWS]->(f:Person) WHERE f.name = 'Bob' RETURN p.name AS who";

fn main() -> Result<()> {
    let db = Database::open(":memory:")?;
    let mut conn = db.connect()?;
    conn.query("CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")?;

    // The plan tree of a MATCH: ProduceResults outermost, the Expand
    // beneath it, its details carrying the pattern text — execution never
    // runs, so a planned write is safe too.
    let tree = conn.explain_plan(STATEMENT)?;
    assert_eq!(tree.operator, "ProduceResults");
    assert_eq!(tree.children[0].operator, "Expand");
    assert_eq!(
        tree.children[0].details,
        // The WHERE pushed down into the pattern itself, so no tail
        // filter is named; the subject is the pattern text alone.
        "(p:Person)-[k:KNOWS]->(f:Person)"
    );
    let count = conn.query("MATCH (n) RETURN count(n) AS c")?;
    assert_eq!(count.rows[0][0], cypherlite::Value::Int(2)); // untouched

    // The machine-native form (ADR-0345): a `Value` map in Neo4j's
    // structured-plan field shape — `operatorType`, `args` (the node's id,
    // details, estimate), `identifiers`, `children` — one form every
    // consumer (agent, tooling, the formatter's own plan table) reads.
    let cypherlite::Value::Map(wire) = tree.to_value() else {
        panic!("the machine-native plan value is a map");
    };
    assert_eq!(
        wire.get("operatorType"),
        Some(&cypherlite::Value::String("ProduceResults".into()))
    );

    // The Neo4j-facing value (ADR-0357, ADR-0359): the machine-native form
    // decorated with the root args Neo4j's plan carries — the surface's
    // rendering passed as `plan_text` heads the `string-representation`
    // (the engine prepends only the `Cypher`/`Planner`/`Runtime` identity
    // block), beside the planner/runtime identity and `Memory`. `None`
    // leaves `string-representation` off (a machine-mode consumer). This
    // example cannot call the formatter (the engine never depends on a
    // surface, ADR-0103), so it passes a short literal as the text; a real
    // surface passes its `cypherlite_formatter::plan_text_lines(…)`
    // output — the formatter crate's `examples/plan_text.rs` does exactly
    // that.
    let cypherlite::Value::Map(root) =
        tree.to_neo4j_value("Query: RETURN 1\nPlan:\nProduceResults")
    else {
        panic!("the plan value is a map");
    };
    let Some(cypherlite::Value::Map(args)) = root.get("args") else {
        panic!("the plan node carries an args map");
    };
    let Some(cypherlite::Value::String(representation)) = args.get("string-representation") else {
        panic!("the root args carry string-representation");
    };
    assert!(representation.starts_with("Cypher 5\n\nPlanner cypherlite\n\nRuntime cypherlite\n"));
    assert!(representation.ends_with("Query: RETURN 1\nPlan:\nProduceResults"));
    assert_eq!(
        args.get("planner-impl"),
        Some(&cypherlite::Value::String("cypherlite".into()))
    );

    // Typed warnings (typed node warnings): a label-less scan on a store
    // that carries a property index carries the `AllNodesScan` warning naming
    // the existing index — rendered beside the affected operator in the
    // `Warnings` column and footer by the boundary's plan table.
    conn.query("CREATE INDEX person_name FOR (n:Person) ON (n.name)")?;
    let scan = conn.explain_plan("MATCH (n) RETURN n")?;
    assert_eq!(
        scan.children[0].warnings,
        [cypherlite::PlanWarning::AllNodesScan {
            index: ":Person(name)".to_string()
        }]
    );

    // The PROFILE tree: every executed clause operator carries its
    // metrics — rows produced, elapsed time, `db_hits` store accesses,
    // `memory_bytes` row-materialization bytes — and a 10x-or-greater
    // estimate overshoot flags the typed `EstimationMiss` warning (the
    // boundary renders it as the `Est/Actual` cell's `!`).
    conn.query("UNWIND range(1, 25) AS i CREATE (:Person {flag: i = 1})")?;
    let profiled = conn.query("PROFILE MATCH (n:Person) WHERE n.flag RETURN n")?;
    let profiled_tree = profiled.plan_root().expect("PROFILE stamps the plan");
    assert!(profiled_tree.is_profiled());
    let scan = &profiled_tree.children[0];
    let scan_metrics = scan.metrics.expect("the scan operator's metrics");
    assert_eq!(scan_metrics.rows, 1, "{scan_metrics:?}");
    assert!(scan_metrics.time_ns > 0, "{scan_metrics:?}");
    // LABS-375: a node scan counts one store read per candidate it
    // examined, so an indexed-label scan reports its reads (never 0).
    assert!(
        scan_metrics.db_hits > 0,
        "a scan of an indexed label reads the store: {scan_metrics:?}"
    );
    let projection = profiled_tree.metrics.expect("the projection's metrics");
    assert_eq!(projection.db_hits, 0, "{projection:?}");
    assert_eq!(
        scan.warnings,
        [cypherlite::PlanWarning::EstimationMiss],
        "the 27-row estimate against one actual row: a 10x overshoot"
    );

    Ok(())
}
