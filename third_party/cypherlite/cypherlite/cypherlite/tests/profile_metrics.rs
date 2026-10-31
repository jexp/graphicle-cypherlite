//! The PROFILE per-operator metric tests (ADR-0344 phase 2 + metric
//! fields): rows, elapsed time, `db hits` (the store-access delta: hop
//! expansions, neighbor walks and node-store reads), and
//! row-materialization `memory bytes` per clause operator — the NEXT-chain
//! stage keying, the starved-operator facts behind the never-executed
//! marker, and the estimated-vs-actual 10x skew behind the
//! `EstimationMiss` warning (their rendered forms are the boundary's,
//! pinned in `cypherlite-formatter`, ADR-0359).

mod common;

use cypherlite::Value;

use common::{connection, plan};

#[test]
fn profile_of_a_next_chain_carries_each_stages_own_metrics() {
    let mut conn = connection();
    // A NEXT chain with distinct per-stage cardinalities: stage 1
    // produces two rows, stage 2 aggregates them to one. Without the
    // stage in the metrics key the later stage's entries overwrite the
    // earlier stage's same-keyed ones, and stage 1's tree node would
    // report stage 2's counts (ADR-0344 phase 2).
    let result = conn
        .query("PROFILE UNWIND [1, 2] AS i RETURN i AS x NEXT RETURN count(x) AS c")
        .unwrap();
    let tree = plan(&result);
    let stage1 = tree.metrics.expect("stage 1 root metrics");
    assert_eq!(stage1.rows, 2, "stage 1 produced the two UNWIND rows");
    assert!(stage1.time_ns > 0, "{stage1:?}");
    let next = tree
        .children
        .iter()
        .find(|c| c.operator == "Next")
        .expect("the Next stage link");
    let stage2 = next.children[0].metrics.expect("stage 2 root metrics");
    assert_eq!(stage2.rows, 1, "stage 2 produced the single count row");
    assert!(stage2.time_ns > 0, "{stage2:?}");
}

/// LABS-375: a node scan reads the node store once per candidate it
/// examined, so its `db_hits` is nonzero — before the node reads were
/// counted the scan reported 0 while doing real work.
#[test]
fn a_node_scan_reports_its_store_reads() {
    let mut conn = connection();
    conn.query("CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'})")
        .unwrap();
    let result = conn
        .query("PROFILE MATCH (n:Person) RETURN n.name AS name")
        .unwrap();
    let tree = plan(&result);
    let scan = tree
        .children
        .iter()
        .find(|c| c.operator == "NodeByLabelScan" || c.operator == "AllNodesScan")
        .expect("the scan operator");
    let metrics = scan.metrics.expect("the scan's metrics");
    assert_eq!(metrics.rows, 2, "{metrics:?}");
    assert!(
        metrics.db_hits >= 2,
        "one store read per candidate examined: {metrics:?}"
    );
}

/// The metric fields (ADR-0344 metric fields): `db_hits` is the clause's
/// store-access count — the delta of the executor's graph-walk counters
/// (hop expansions, neighbor walks and node-store reads) across the
/// clause's arm — and
/// `memory_bytes` is the row-materialization bytes: the summed logical
/// size (ADR-0042) of the rows the operator produced. Both deterministic
/// alongside the row count: a traversal's walks count with its rows, a
/// projection's bytes follow its values.
#[test]
fn profile_metrics_carry_db_hits_and_memory_bytes() {
    let mut conn = connection();
    conn.query(
        "CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'}),
         (:Person {name: 'Cara'})",
    )
    .unwrap();
    conn.query(
        "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})
         CREATE (a)-[:KNOWS]->(b)",
    )
    .unwrap();
    let result = conn
        .query("PROFILE MATCH (a:Person)-[k:KNOWS]->(b:Person) RETURN b.name")
        .unwrap();
    let tree = plan(&result);
    // The one pattern is one Expand operator (the traversal) beneath the
    // projection.
    let expand = &tree.children[0];
    assert_eq!(expand.operator, "Expand");
    let expand_metrics = expand.metrics.expect("the expand operator's metrics");
    assert_eq!(expand_metrics.rows, 1, "the traversal produced one row");
    assert!(
        expand_metrics.db_hits > 0,
        "the traversal walked relationships: {expand_metrics:?}"
    );
    // The projection's memory bytes are the rows it produced: one row,
    // one binding `b.name`, an ADR-0042 string ('Bob': 16 + 3) beside the
    // key's own string bytes ('b.name': 16 + 6) — the deterministic sum.
    let root = tree.metrics.expect("the projection's metrics");
    assert_eq!(root.rows, 1);
    assert_eq!(root.memory_bytes, 16 + 6 + 16 + 3, "{root:?}");
    // The projection touches no store — an honestly counted zero.
    assert_eq!(root.db_hits, 0, "{root:?}");
    // The wire form carries the new args beside Rows/TimeNs, emitted
    // together with the same saturation policy.
    let Value::Map(map) = expand.to_value() else {
        panic!("to_value is a map");
    };
    let Value::Map(args) = map.get("args").expect("args") else {
        panic!("args is a map");
    };
    let Value::Int(db_hits) = args.get("DbHits").expect("DbHits") else {
        panic!("DbHits is an integer");
    };
    assert!(*db_hits > 0, "{args:?}");
    let memory = args.get("MemoryBytes").expect("MemoryBytes");
    assert!(matches!(memory, Value::Int(_)), "{args:?}");
}

/// The never-executed marker's facts: an operator whose input clause
/// produced no rows did no work — its metrics stay honestly zero, while
/// the tree's root (the projection that produced the answer) carries its
/// actual metrics. The rendered marker cells ("(never executed)", never a
/// bare zero) are the boundary's rendering — pinned in
/// `cypherlite-formatter`'s plan-text tests (ADR-0359).
#[test]
fn profile_marks_a_starved_operator_with_zero_metrics() {
    let mut conn = connection();
    conn.query("CREATE (:Person {name: 'Alice'})").unwrap();
    let result = conn
        .query("PROFILE MATCH (n:Person) WHERE n.name = 'Nobody' MATCH (m:Person) RETURN m.name")
        .unwrap();
    let tree = plan(&result);
    // The starved MATCH: its input (the filtered MATCH) produced no rows,
    // it produced none either — the collected facts behind the marker.
    let starved = &tree.children[0].children[0];
    assert_eq!(starved.operator, "NodeByLabelScan");
    let metrics = starved.metrics.expect("the starved MATCH's metrics");
    assert_eq!(metrics.rows, 0, "{metrics:?}");
    // The root always executes — the projection that produced the answer.
    let root = tree.metrics.expect("the projection's metrics");
    assert_eq!(root.rows, 0, "{root:?}");
    assert!(root.time_ns > 0, "the root executed: {root:?}");
}

/// The estimated-vs-actual ratio facts: a scan estimated at the label
/// count whose WHERE filters to almost nothing overshoots by 10x or more
/// (the typed `EstimationMiss` warning), while a matching estimate stays
/// warning-free. The rendered ratio cells ("Est/Actual", "25.0x !", the
/// "-" unavailable spelling) are the boundary's rendering — pinned in
/// `cypherlite-formatter`'s plan-text tests (ADR-0359).
#[test]
fn profile_flags_a_ten_x_estimate_skew_with_the_typed_warning() {
    let mut conn = connection();
    conn.query("UNWIND range(1, 25) AS i CREATE (:Person {flag: i = 1})")
        .unwrap();
    let result = conn
        .query("PROFILE MATCH (n:Person) WHERE n.flag RETURN n")
        .unwrap();
    let tree = plan(&result);
    // The scan: estimated at the label count (25), one actual row — a
    // 25.0x overshoot flagged by the typed warning.
    let scan = common::find_node(tree, "NodeByLabelScan").expect("the scan");
    assert_eq!(scan.estimated_rows, Some(25), "{tree:?}");
    let scan_metrics = scan.metrics.expect("the scan's metrics");
    assert_eq!(scan_metrics.rows, 1, "{scan_metrics:?}");
    assert_eq!(
        scan.warnings,
        [cypherlite::PlanWarning::EstimationMiss],
        "the 25-row estimate against one actual row: a 10x overshoot: {scan:?}"
    );
    // A matching estimate: every person, no filter — no warning.
    let matched = conn.query("PROFILE MATCH (n:Person) RETURN n").unwrap();
    let matched_scan =
        common::find_node(plan(&matched), "NodeByLabelScan").expect("the matched scan");
    assert!(
        matched_scan.warnings.is_empty(),
        "a matching estimate carries no warning: {matched_scan:?}"
    );
}

#[test]
fn profile_names_the_walks_start_and_the_read_kinds() {
    // LABS-369: the Expand operator no longer swallows where the walk
    // started or how the one Db Hits total splits. A row-bound pivot
    // (ADR-0350) must name itself, and the read kinds must ride the
    // metric set (emitted together with it).
    let mut conn = connection();
    conn.query(
        "CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})\
         -[:KNOWS]->(c:Person {name: 'Carol'})",
    )
    .unwrap();
    // The pivot: the walk's SECOND element is row-bound from the first
    // clause (`b` is the pattern's index 1), so the expansion anchors on
    // it and walks its prefix backwards (ADR-0350) — the fact the opaque
    // Expand used to hide.
    let result = conn
        .query(
            "PROFILE MATCH (b:Person) WITH b \
             MATCH (a)-[k1:KNOWS]->(b)-[k2:KNOWS]->(c) RETURN a.name AS n",
        )
        .unwrap();
    let tree = plan(&result);
    let expand = common::find_node(tree, "Expand").expect("the expansion");
    let metrics = expand.metrics.expect("PROFILE metrics");
    let start = metrics.start.expect("a walk ran: its start is named");
    assert!(
        matches!(start.reason, cypherlite_executor::StartReason::BoundPivot),
        "the row-bound element anchors the walk: {start:?}"
    );
    assert!(
        start.backward,
        "the pivot walks its prefix backwards: {start:?}"
    );
    // The read kinds ride the metric set: the walk read the store.
    assert!(
        metrics.reads.hops + metrics.reads.neighbor_walks + metrics.reads.node_reads > 0,
        "the walk read the store: {:?}",
        metrics.reads
    );
    assert_eq!(
        metrics.db_hits,
        metrics.reads.hops + metrics.reads.neighbor_walks + metrics.reads.node_reads,
        "the total is exactly the kinds' sum: {:?}",
        metrics.reads
    );
}

#[test]
fn an_explain_tree_names_no_start_and_carries_no_reads() {
    // The start and the read kinds are PROFILE facts: an EXPLAIN tree
    // carries neither, so a consumer never reads a fabricated zero.
    let mut conn = connection();
    let result = conn
        .query("EXPLAIN MATCH (a:Person)-[:KNOWS]->(b) RETURN b")
        .unwrap();
    let tree = plan(&result);
    let expand = common::find_node(tree, "Expand").expect("the expansion");
    assert!(expand.metrics.is_none(), "EXPLAIN carries no metrics");
}

/// The per-phase times (LABS-448): a PROFILEd walk's operators carry the
/// seek's and the walk's elapsed nanoseconds — the scan's candidates
/// resolution is seek time, the expand's relationship traversal is walk
/// time, a clause that walks nothing reports zeros — and the stamps cost
/// nothing on a non-PROFILEd statement (its metrics are simply absent).
#[test]
fn profile_reports_the_seek_and_walk_phase_times() {
    let mut conn = connection();
    conn.query("CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})")
        .unwrap();
    // A two-element pattern: the label scan resolves candidates (seek),
    // the expansion follows the relationship (walk) — its own arm interleaves
    // per row, and the nested walk_pattern the multi-element recursion
    // performs counts inclusively into both sums.
    let result = conn
        .query("PROFILE MATCH (p:Person)-[:KNOWS]->(f:Person) RETURN f.name")
        .unwrap();
    let tree = plan(&result);
    let expand = tree
        .children
        .iter()
        .find(|c| c.operator.contains("Expand"))
        .expect("the pattern's expand operator");
    let metrics = expand.metrics.expect("the PROFILE metrics");
    assert!(
        metrics.phases.seek_ns > 0,
        "the expand resolved node candidates: {metrics:?}"
    );
    assert!(
        metrics.phases.walk_ns > 0,
        "the expand followed the relationship: {metrics:?}"
    );
    // The phases ride the whole metric block: the sum covers both phases
    // and whatever the arm spent between them.
    let phases_sum = metrics.phases.seek_ns + metrics.phases.walk_ns;
    assert!(
        phases_sum <= metrics.time_ns,
        "the inclusive phases cannot exceed the arm's wall clock: {metrics:?}"
    );
    // A non-walking clause (the projection) reports zeros, never missing
    // values — the stamps exist for every profiled clause.
    let root = tree.metrics.expect("the root's metrics");
    assert_eq!(
        root.phases,
        Default::default(),
        "the projection walks nothing"
    );
}

/// The shortest-path resolver sits inside the seek span too (LABS-448's
/// round-2 finding): a PROFILEd `MATCH SHORTEST` reports its start
/// resolution as seek time — `Nodes > 0` with `seek_ns > 0`, the kind↔phase
/// pairing holding on the shortest route, never a zero-phase read count.
#[test]
fn profile_shortest_path_reports_its_resolution_as_seek_time() {
    let mut conn = connection();
    conn.query("CREATE (a:P {n: 'a'})-[:R]->(b:P {n: 'b'})")
        .unwrap();
    let result = conn
        .query("PROFILE MATCH SHORTEST 1 (a:P {n: 'a'})-[:R]->(b:P {n: 'b'}) RETURN b.n")
        .unwrap();
    let tree = plan(&result);
    let shortest = tree
        .children
        .iter()
        .find(|c| c.operator.to_lowercase().contains("shortest"))
        .or_else(|| tree.children.first())
        .expect("the shortest operator");
    let metrics = shortest.metrics.expect("the PROFILE metrics");
    assert!(
        metrics.reads.node_reads > 0,
        "the resolver read its start candidates: {metrics:?}"
    );
    assert!(
        metrics.phases.seek_ns > 0,
        "the resolver's resolution is seek time, never a zero-phase read: {metrics:?}"
    );
}
