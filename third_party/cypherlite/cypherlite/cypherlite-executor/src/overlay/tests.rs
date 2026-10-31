use super::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::exec::test_support::{
    commit_scratch, exec_autocommit, exec_cypher, exec_write, exec_write_uncommitted, write_scratch,
};
use crate::graph::Graph;
use crate::redo::RedoOp;
use cypherlite_core::{Error, Node, Value};
use cypherlite_front::parse_and_lower;
use cypherlite_ir::Clause;
use cypherlite_planner::plan;

#[test]
fn overlay_does_not_mutate_base_until_redo() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    let result = exec_write(
        &g,
        &scratch,
        "CREATE (n:Person {name: 'Ada'}) WITH n MATCH (m:Person {name: 'Ada'}) RETURN m.name AS name",
    )
    .unwrap();
    assert_eq!(result.rows[0][0].as_string(), Some("Ada"));
    assert_eq!(g.live_node_count(), 0);
    assert_eq!(scratch.into_inner().take_redo().len(), 1);
}

#[test]
fn create_then_delete_cancels_create_stats() {
    let g = Graph::default();
    g.create_node(Vec::new(), BTreeMap::new()).unwrap();
    let result = exec_write_uncommitted(&g, "MATCH () CREATE (n) DELETE n");
    assert_eq!(result.stats.nodes_created, 0);
    assert_eq!(result.stats.nodes_deleted, 0);
}

#[test]
fn create_then_set_same_query_does_not_recount() {
    let result = run_write("CREATE (n:A {name: 'Andres'}) WITH n SET n.name = 'Michael' RETURN n");
    assert_eq!(result.stats.properties_removed, 0);
    assert_eq!(result.stats.properties_set, 1);
}

#[test]
fn set_overwrite_counts_property_removed() {
    let g = Graph::default();
    let mut props = BTreeMap::new();
    props.insert("name".into(), Value::String("Andres".into()));
    g.create_node(vec!["A".into()], props).unwrap();
    let result = exec_write_uncommitted(&g, "MATCH (n:A) SET n.name = 'Michael' RETURN n");
    assert_eq!(result.stats.properties_removed, 1);
    assert_eq!(result.stats.properties_set, 1);
}

#[test]
fn remove_property_counts_removed() {
    let result = run_write("CREATE (n:A {name: 'Andres'}) WITH n REMOVE n.name RETURN n");
    assert_eq!(result.stats.properties_removed, 1);
}

#[test]
fn delete_rel_counts_properties_removed() {
    let result = run_write("CREATE ()-[r:T {x: 1}]->() WITH r DELETE r");
    assert_eq!(result.stats.relationships_created, 0);
    assert_eq!(result.stats.relationships_deleted, 0);
    assert_eq!(result.stats.properties_removed, 0);
}

#[test]
fn create_same_label_counts_once() {
    let result = run_write("CREATE (:N), (:N), (:N)");
    assert_eq!(result.stats.nodes_created, 3);
    assert_eq!(result.stats.labels_added, 1);
}

#[test]
fn exhausted_allocator_never_wraps_or_enqueues_redo() {
    let graph = Graph::default();
    let node_alloc = Arc::new(AtomicU64::new(i64::MAX as u64 - 1));
    let rel_alloc = Arc::new(AtomicU64::new(i64::MAX as u64));
    let scratch = RefCell::new(WriteScratch::new(
        0,
        Arc::clone(&node_alloc),
        Arc::clone(&rel_alloc),
    ));
    let view = ExecGraph::write(&graph, &scratch);
    assert_eq!(
        view.create_node(vec![], BTreeMap::new()).unwrap(),
        i64::MAX - 1
    );
    assert!(matches!(
        view.create_node(vec![], BTreeMap::new()),
        Err(Error::Execution(_))
    ));
    assert!(matches!(
        view.create_rel(i64::MAX - 1, i64::MAX - 1, "R".into(), BTreeMap::new()),
        Err(Error::Execution(_))
    ));
    assert_eq!(node_alloc.load(Ordering::Relaxed), i64::MAX as u64);
    assert_eq!(rel_alloc.load(Ordering::Relaxed), i64::MAX as u64);
    assert_eq!(scratch.borrow_mut().take_redo().len(), 1);
}

#[test]
fn create_shared_label_on_snapshot_does_not_add() {
    let g = Graph::default();
    g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let r = exec_write_uncommitted(&g, "CREATE (:A)");
    assert_eq!(r.stats.nodes_created, 1);
    assert_eq!(r.stats.labels_added, 0);
    assert_eq!(r.stats.labels_removed, 0);
}

#[test]
fn delete_last_snapshot_label_then_recreate_nets() {
    let g = Graph::default();
    g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let r = exec_write_uncommitted(&g, "MATCH (n:A) DELETE n CREATE (:A)");
    assert_eq!(r.stats.nodes_created, 1);
    assert_eq!(r.stats.nodes_deleted, 1);
    assert_eq!(r.stats.labels_added, 0);
    assert_eq!(r.stats.labels_removed, 0);
}

#[test]
fn remove_shared_label_does_not_count() {
    let g = Graph::default();
    g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let r = exec_write_uncommitted(&g, "MATCH (n:A) WITH n LIMIT 1 REMOVE n:A");
    assert_eq!(r.stats.labels_added, 0);
    assert_eq!(r.stats.labels_removed, 0);
}

#[test]
fn remove_last_snapshot_label_counts() {
    let g = Graph::default();
    g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let r = exec_write_uncommitted(&g, "MATCH (n:A) REMOVE n:A");
    assert_eq!(r.stats.labels_added, 0);
    assert_eq!(r.stats.labels_removed, 1);
}

#[test]
fn add_already_live_label_does_not_count() {
    let g = Graph::default();
    g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    let r = exec_write_uncommitted(&g, "MATCH (n:B) SET n:A");
    assert_eq!(r.stats.labels_added, 0);
    assert_eq!(r.stats.labels_removed, 0);
}

#[test]
fn exists_subquery_outgoing() {
    let result = run_write(
        "CREATE (a:A {prop: 1})-[:R]->(b:B {prop: 1}) WITH * MATCH (n) WHERE exists { (n)-->() } RETURN n.prop AS p",
    );
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn chained_comparison() {
    let result = run_write("CREATE ({num: 2}) WITH * MATCH (n) WHERE 1 < n.num < 3 RETURN n.num");
    assert_eq!(result.rows, vec![vec![Value::Int(2)]]);
}

#[test]
fn size_collect_is_agg() {
    use crate::eval::is_agg_expr;
    let ast = parse_and_lower("RETURN size(collect(a))").unwrap();
    let Clause::Return { items, .. } = &ast.parts[0].clauses[0] else {
        panic!("expected RETURN");
    };
    assert!(
        is_agg_expr(&items[0].expr),
        "size(collect(a)) must be an aggregation, got {:?}",
        items[0].expr
    );
}

#[test]
fn collect_inside_map() {
    let result = run_write("CREATE (:A) WITH * MATCH (n) RETURN {k: collect(n)} AS m");
    assert_eq!(result.rows.len(), 1);
    assert!(matches!(result.rows[0][0], Value::Map(_)));
}

#[test]
fn deleted_entity_property_access_errors() {
    let err = run_write_err("CREATE ({num: 0}) WITH * MATCH (n) DELETE n RETURN n.num");
    assert!(
        err.contains("DeletedEntityAccess"),
        "expected DeletedEntityAccess, got {err}"
    );
}

#[test]
fn map_property_is_invalid() {
    let err = run_write_err("CREATE (a) SET a.maplist = [{num: 1}]");
    assert!(
        err.contains("InvalidPropertyType"),
        "expected InvalidPropertyType, got {err}"
    );
}

#[test]
fn scalar_property_access_is_type_error() {
    let q = parse_and_lower("WITH 123 AS x RETURN x.num").unwrap();
    let err = plan(q, &mut |_| false).unwrap_err().to_string();
    assert!(
        err.contains("InvalidArgumentType"),
        "expected InvalidArgumentType, got {err}"
    );
}

#[test]
fn undirected_var_length_two_does_not_overcount() {
    let result = run_write(
        "CREATE (a:A)-[:REL1]->(b:B), (b)-[:REL2]->(a), (a)-[:REL3]->(:C), (a)-[:REL1]->(:D) \
         WITH * MATCH (n) WHERE (n)-[:REL1*2]-() RETURN n",
    );
    assert_eq!(result.rows.len(), 2);
}

fn run_write_result(q: &str) -> cypherlite_core::Result<crate::QueryResult> {
    let g = Graph::default();
    exec_write(&g, &write_scratch(&g), q)
}

fn run_write(q: &str) -> crate::QueryResult {
    run_write_result(q).unwrap()
}

fn run_write_err(q: &str) -> String {
    match run_write_result(q) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected error for query: {q}"),
    }
}

#[test]
fn order_by_uses_incoming_variable_before_project() {
    let r = run_write(
        "CREATE ({num: 1}), ({num: 3}), ({num: -5}) \
         WITH * MATCH (n) RETURN n.num AS prop ORDER BY n.num",
    );
    let nums: Vec<i64> = r.rows.iter().map(|row| row[0].as_int().unwrap()).collect();
    assert_eq!(nums, vec![-5, 1, 3]);
}

#[test]
fn merge_binds_named_path() {
    let r = run_write("MERGE p = (a {num: 1}) RETURN p");
    assert!(matches!(r.rows[0][0], Value::Path(_)));
}

#[test]
fn with_star_forwards_create_vars() {
    let r = run_write("CREATE (a {var: 1}), (b {var: 2}) WITH * RETURN a.var, b.var");
    assert_eq!(r.rows[0][0].as_int(), Some(1));
    assert_eq!(r.rows[0][1].as_int(), Some(2));
}

#[test]
fn group_by_distinguishes_unlabeled_nodes() {
    let r = run_write(
        "CREATE (a:L), (b1), (b2) CREATE (a)-[:A]->(b1), (a)-[:A]->(b2) \
         WITH * MATCH p=(x:L)-[*]->(b) RETURN b, avg(length(p))",
    );
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn bound_rel_list_is_exact_trail() {
    let r = run_write(
        "CREATE (a:A)-[:Y]->(b:B)-[:Y]->(c:C) \
         WITH * MATCH ()-[r1]->()-[r2]->() WITH [r1, r2] AS rs LIMIT 1 \
         MATCH (first)-[rs*]->(second) RETURN first, second",
    );
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn overlay_expand_sees_scratch_create() {
    let r = run_write(
        "CREATE (a:A {id: 1})-[:R]->(b:B {id: 2}) \
         WITH a MATCH (a)-[:R]->(x) RETURN x.id AS id",
    );
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0].as_int(), Some(2));
}

#[test]
fn overlay_expand_hides_deleted_rel() {
    let g = Graph::default();
    let setup = write_scratch(&g);
    exec_write(&g, &setup, "CREATE (:A {id: 1})-[:R]->(:B {id: 2})").unwrap();
    commit_scratch(&g, setup);
    let result = exec_write_uncommitted(
        &g,
        "MATCH (a:A {id: 1})-[r:R]->() DELETE r \
         WITH a MATCH (a)-[:R]->(x) RETURN x",
    );
    assert!(result.rows.is_empty(), "deleted overlay rel still visible");
}

/// ADR-0172 §3: `set_property_node` and `add_labels` keep an already-built
/// `prop_ix` pair current, so the overlay equality seek (and the uniqueness
/// probe) sees nodes whose property or label changed after the build.
#[test]
fn prop_ix_seek_sees_set_and_added_label_after_the_pair_is_built() {
    let g = Graph::default();
    let a = g
        .create_node(
            vec!["A".into()],
            BTreeMap::from([("x".into(), Value::Int(1))]),
        )
        .unwrap();
    let b = g
        .create_node(
            vec!["B".into()],
            BTreeMap::from([("x".into(), Value::Int(2))]),
        )
        .unwrap();
    let scratch = write_scratch(&g);
    let view = ExecGraph::write(&g, &scratch);
    let labels = ["A".to_string()];
    let props = BTreeMap::from([("x".to_string(), Value::Int(2))]);
    assert_eq!(view.lookup_nodes(&labels, &props), Some(vec![]));
    view.set_property_node(a, "x", Value::Int(2));
    view.add_labels(b, &labels);
    assert_eq!(view.lookup_nodes(&labels, &props), Some(vec![a, b]));
}

/// ADR-0184: `equality_bucket_len` sums the committed and the scratch
/// bucket without materializing candidates, and counts a NaN as empty.
#[test]
fn equality_bucket_len_sums_committed_and_scratch() {
    let g = Graph::default();
    for _ in 0..2 {
        g.create_node(
            vec!["P".into()],
            BTreeMap::from([("a".into(), Value::Int(1))]),
        )
        .unwrap();
    }
    g.create_node(
        vec!["P".into()],
        BTreeMap::from([("a".into(), Value::Int(2))]),
    )
    .unwrap();
    let scratch = write_scratch(&g);
    let view = ExecGraph::write(&g, &scratch);
    assert_eq!(view.equality_bucket_len("P", "a", &Value::Int(1)), 2);
    assert_eq!(view.equality_bucket_len("P", "a", &Value::Int(2)), 1);
    // A scratch node posted after the pairs were built joins the sum.
    view.create_node(
        vec!["P".into()],
        BTreeMap::from([("a".into(), Value::Int(1))]),
    )
    .unwrap();
    assert_eq!(view.equality_bucket_len("P", "a", &Value::Int(1)), 3);
    assert_eq!(
        view.equality_bucket_len("P", "a", &Value::Float(f64::NAN)),
        0
    );
}

/// ADR-0172 §1/§8: the schema overlay replays only the catalog ops, the
/// validation cursor drains each op once, and `take_redo` resets both.
#[test]
fn catalog_positions_and_validation_cursor_follow_the_redo_log() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    let view = ExecGraph::write(&g, &scratch);
    let constraint = |name: &str| {
        view.create_named_constraint(
            name.into(),
            false,
            "P".into(),
            vec!["id".into()],
            "unique".into(),
            None,
        )
        .unwrap()
    };
    view.create_node(vec!["P".into()], BTreeMap::new()).unwrap();
    constraint("c");
    view.create_node(vec!["P".into()], BTreeMap::new()).unwrap();
    view.create_named_index("i".into(), "P".into(), "id".into())
        .unwrap();
    view.drop_named_constraint("c".into()).unwrap();
    constraint("d");
    let names: Vec<String> = view
        .named_constraints()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(names, ["d"]);
    assert_eq!(view.named_indexes().len(), 1);
    let drain = || {
        let (len, end) = view.unvalidated_redo(<[RedoOp]>::len);
        view.mark_redo_validated(end);
        len
    };
    assert_eq!(drain(), 6);
    assert_eq!(drain(), 0);
    view.create_node(vec!["P".into()], BTreeMap::new()).unwrap();
    assert_eq!(drain(), 1);
    assert_eq!(scratch.borrow_mut().take_redo().len(), 7);
    assert!(view.named_constraints().is_empty());
    view.create_node(vec!["P".into()], BTreeMap::new()).unwrap();
    assert_eq!(drain(), 1);
}

#[test]
fn materialize_passes_entity_free_payloads_through() {
    let g = Graph::default();
    let view = ExecGraph::read(&g);
    let ns: Arc<Vec<Value>> = Arc::new((0..100).map(Value::Int).collect());
    let Value::List(out) = view.materialize_cached(
        Value::List(Arc::clone(&ns)),
        &mut MaterializeCache::default(),
    ) else {
        panic!("expected a list");
    };
    assert!(Arc::ptr_eq(&out, &ns));

    let nested: Arc<Vec<Value>> = Arc::new(vec![
        Value::List(Arc::clone(&ns)),
        Value::Map(Arc::new(BTreeMap::from([("x".into(), Value::Int(1))]))),
    ]);
    let Value::List(out) = view.materialize_cached(
        Value::List(Arc::clone(&nested)),
        &mut MaterializeCache::default(),
    ) else {
        panic!("expected a list");
    };
    assert!(Arc::ptr_eq(&out, &nested));

    let m: Arc<BTreeMap<String, Value>> = Arc::new(BTreeMap::from([
        ("ns".into(), Value::List(Arc::clone(&ns))),
        ("name".into(), Value::String("x".into())),
    ]));
    let Value::Map(out) =
        view.materialize_cached(Value::Map(Arc::clone(&m)), &mut MaterializeCache::default())
    else {
        panic!("expected a map");
    };
    assert!(Arc::ptr_eq(&out, &m));

    let values: Arc<Vec<Value>> = Arc::new(vec![Value::Float(1.0), Value::Float(2.0)]);
    let vector = Value::Vector {
        coord: cypherlite_core::VectorCoordKind::Float64,
        values: Arc::clone(&values),
    };
    let Value::Vector { values: out, .. } =
        view.materialize_cached(vector, &mut MaterializeCache::default())
    else {
        panic!("expected a vector");
    };
    assert!(Arc::ptr_eq(&out, &values));
}

#[test]
fn materialize_rebuilds_only_entity_holding_containers() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let id = g
        .create_node(
            vec!["T".into()],
            BTreeMap::from([("v".into(), Value::Int(2))]),
        )
        .unwrap();
    g.commit_write();
    let stale = Value::from_node(Node {
        id,
        labels: Arc::new(vec!["T".into()]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
            "v".into(),
            Value::Int(1),
        )])),
    });
    let ns: Arc<Vec<Value>> = Arc::new(vec![Value::Int(1), Value::Int(2)]);
    let inner: Arc<BTreeMap<String, Value>> = Arc::new(BTreeMap::from([
        ("node".into(), stale.clone()),
        ("ns".into(), Value::List(Arc::clone(&ns))),
    ]));
    let held: Arc<Vec<Value>> = Arc::new(vec![
        Value::List(Arc::clone(&ns)),
        Value::Map(Arc::clone(&inner)),
        stale,
    ]);
    let view = ExecGraph::read(&g);
    let Value::List(out) = view.materialize_cached(
        Value::List(Arc::clone(&held)),
        &mut MaterializeCache::default(),
    ) else {
        panic!("expected a list");
    };
    assert!(!Arc::ptr_eq(&out, &held));
    let assert_fresh = |v: &Value| match v {
        Value::Node(n) => assert_eq!(n.properties.get("v"), Some(&Value::Int(2))),
        other => panic!("expected a node, got {other:?}"),
    };
    let Value::List(first) = &out[0] else {
        panic!("expected a list");
    };
    assert!(Arc::ptr_eq(first, &ns));
    let Value::Map(m) = &out[1] else {
        panic!("expected a map");
    };
    assert!(!Arc::ptr_eq(m, &inner));
    assert_fresh(&m["node"]);
    let Value::List(inner_ns) = &m["ns"] else {
        panic!("expected a list");
    };
    assert!(Arc::ptr_eq(inner_ns, &ns));
    assert_fresh(&out[2]);
}

#[test]
fn materialize_walks_each_shared_payload_once_per_cache() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let id = g
        .create_node(
            vec!["T".into()],
            BTreeMap::from([("v".into(), Value::Int(2))]),
        )
        .unwrap();
    g.commit_write();
    let view = ExecGraph::read(&g);
    let ns: Arc<Vec<Value>> = Arc::new((0..1000).map(Value::Int).collect());
    let m: Arc<BTreeMap<String, Value>> = Arc::new(BTreeMap::from([
        ("ns".into(), Value::List(Arc::clone(&ns))),
        ("name".into(), Value::String("x".into())),
    ]));

    let mut cache = MaterializeCache::default();
    for _ in 0..1000 {
        let Value::List(out) = view.materialize_cached(Value::List(Arc::clone(&ns)), &mut cache)
        else {
            panic!("expected a list");
        };
        assert!(Arc::ptr_eq(&out, &ns));
        let Value::Map(out) = view.materialize_cached(Value::Map(Arc::clone(&m)), &mut cache)
        else {
            panic!("expected a map");
        };
        assert!(Arc::ptr_eq(&out, &m));
    }
    // `ns` on the first row, `m` once; the walk of `m` meets `ns` cached.
    assert_eq!(cache.walked(), 2);

    let stale = Value::from_node(Node {
        id,
        labels: Arc::new(vec!["T".into()]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
            "v".into(),
            Value::Int(1),
        )])),
    });
    let held: Arc<Vec<Value>> = Arc::new(vec![stale, Value::List(Arc::clone(&ns))]);
    let mut cache = MaterializeCache::default();
    let outs: Vec<Arc<Vec<Value>>> = (0..1000)
        .map(
            |_| match view.materialize_cached(Value::List(Arc::clone(&held)), &mut cache) {
                Value::List(out) => out,
                other => panic!("expected a list, got {other:?}"),
            },
        )
        .collect();
    // `held` and the `ns` beneath it, each once.
    assert_eq!(cache.walked(), 2);
    assert!(!Arc::ptr_eq(&outs[0], &held));
    match &outs[0][0] {
        Value::Node(n) => assert_eq!(n.properties.get("v"), Some(&Value::Int(2))),
        other => panic!("expected a node, got {other:?}"),
    }
    let Value::List(first_ns) = &outs[0][1] else {
        panic!("expected a list");
    };
    assert!(Arc::ptr_eq(first_ns, &ns));
    for out in &outs[1..] {
        assert!(Arc::ptr_eq(out, &outs[0]), "rows re-walked the payload");
    }

    // Distinct payloads are each walked once; a shared child only once.
    let mut cache = MaterializeCache::default();
    for _ in 0..10 {
        let one_off = Value::List(Arc::new(vec![Value::List(Arc::clone(&ns))]));
        view.materialize_cached(one_off, &mut cache);
    }
    assert_eq!(cache.walked(), 11);
}

/// ADR-0210: `ExecGraph::commit_batch` hands the batch's redo ops to the
/// sink, rebases the scratch onto the new commit timestamp, and preserves
/// the accumulated statement stats; a call with no new ops is a no-op
/// (the sink is not called again).
#[test]
fn commit_batch_applies_ops_and_rebases_scratch() {
    use std::sync::atomic::AtomicUsize;

    struct ApplySink<'a> {
        graph: &'a Graph,
        calls: AtomicUsize,
    }
    impl crate::ctx::BatchCommit for ApplySink<'_> {
        fn commit_batch(&self, ops: Vec<RedoOp>, _snapshot_ts: u64) -> Result<u64> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let commit_ts = self.graph.commit_ts() + 1;
            self.graph.apply_redo(ops, commit_ts)?;
            Ok(commit_ts)
        }
    }

    let g = Graph::default();
    let scratch = write_scratch(&g);
    exec_write(&g, &scratch, "CREATE (:Person {name: 'Ada'})").unwrap();
    assert_eq!(g.live_node_count(), 0, "uncommitted batch stays in scratch");

    let view = ExecGraph::write(&g, &scratch);
    let sink = ApplySink {
        graph: &g,
        calls: AtomicUsize::new(0),
    };
    view.commit_batch(&sink).unwrap();
    assert_eq!(sink.calls.load(Ordering::Relaxed), 1);
    assert_eq!(g.live_node_count(), 1, "batch 1 is committed to the base");
    assert_eq!(
        view.stats().nodes_created,
        1,
        "statement stats survive the rebase"
    );
    assert_eq!(
        scratch.borrow().snapshot_ts(),
        g.commit_ts(),
        "scratch rebased onto the batch commit"
    );

    // The fresh scratch works for the next batch, and an empty batch does
    // not reach the sink. (`exec_write` runs a full `execute`, which resets
    // statement stats — the cross-batch stats carry-over is asserted above,
    // inside one execute.)
    exec_write(&g, &scratch, "CREATE (:Person {name: 'Grace'})").unwrap();
    assert_eq!(g.live_node_count(), 1);
    view.commit_batch(&sink).unwrap();
    assert_eq!(g.live_node_count(), 2);
    view.commit_batch(&sink).unwrap();
    assert_eq!(
        sink.calls.load(Ordering::Relaxed),
        2,
        "an empty batch is not committed"
    );
}

#[test]
fn returned_copies_of_one_node_share_its_allocation() {
    let result = run_write("CREATE (n:Person {name: 'Ada'}) WITH n UNWIND [1, 2] AS i RETURN n");
    let Value::Node(first) = &result.rows[0][0] else {
        panic!("row 0: {:?}", result.rows);
    };
    let Value::Node(second) = &result.rows[1][0] else {
        panic!("row 1: {:?}", result.rows);
    };
    assert!(Arc::ptr_eq(&first.properties, &second.properties));
    assert!(Arc::ptr_eq(&first.labels, &second.labels));
}

#[test]
fn failed_batch_restores_the_pre_batch_scratch() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    exec_write(&g, &scratch, "CREATE (n:Person {name: 'Ada'})").unwrap();
    let (ada, props) = {
        let s = scratch.borrow();
        assert_eq!(s.redo_ops().len(), 1);
        let RedoOp::CreateNode { id, properties, .. } = &s.redo_ops()[0] else {
            panic!("first op is the create");
        };
        let node = s
            .batch
            .nodes
            .get(id)
            .expect("create installs the scratch node");
        assert!(
            Arc::ptr_eq(&node.properties, properties),
            "the scratch node and its redo op share the property map"
        );
        assert_eq!(
            s.applied_ops(),
            1,
            "the statement's own applied count is its create"
        );
        (*id, Arc::clone(properties))
    };
    exec_write(
        &g,
        &scratch,
        "UNWIND [1] AS i \
         CALL { WITH i CREATE (:Gone {name: 'no'}) RETURN i / 0 AS bad } \
         IN TRANSACTIONS OF 1 ROWS ON ERROR CONTINUE \
         RETURN bad",
    )
    .unwrap();
    let s = scratch.borrow();
    assert_eq!(s.redo_ops().len(), 1, "the failed batch's create is gone");
    assert_eq!(
        s.applied_ops(),
        0,
        "the second statement's entry reset zeroed the applied count and the \
         rollback image restored that pre-batch zero, so the failed batch's \
         create is not counted"
    );
    assert!(s.batch.nodes.contains_key(&ada));
    assert!(
        s.batch.nodes.keys().all(|id| *id == ada),
        "only the pre-batch node remains"
    );
    let RedoOp::CreateNode { properties, .. } = &s.redo_ops()[0] else {
        panic!("the original create is still the only redo op");
    };
    assert!(Arc::ptr_eq(properties, &props));
}

#[test]
fn stubs_for_the_same_label_share_their_payloads() {
    let g = Graph::default();
    exec_autocommit(
        &g,
        "CREATE (:Person {name: 'Ada'})-[:KNOWS]->(:Person {name: 'Bea'})",
    )
    .unwrap();
    let ts = g.commit_ts();
    let ids = g.all_node_ids();
    assert_eq!(ids.len(), 2);
    let first = g.node_stub_at(ids[0], ts).unwrap();
    let second = g.node_stub_at(ids[1], ts).unwrap();
    assert!(
        Arc::ptr_eq(&first.labels, &second.labels),
        "one label vector serves every :Person stub"
    );
    assert!(
        Arc::ptr_eq(&first.properties, &second.properties),
        "stubs share the empty property map"
    );
    let rel = g.rel_stub_at(g.all_rel_ids()[0], ts).unwrap();
    assert!(Arc::ptr_eq(&first.properties, &rel.properties));
}

#[test]
fn variable_length_return_keeps_projected_integers() {
    let g = Graph::default();
    exec_autocommit(
        &g,
        "CREATE (:A {id: 1})-[:R]->(:A {id: 2})-[:R]->(:A {id: 3})",
    )
    .unwrap();
    let result = exec_cypher(
        &g,
        "MATCH (a:A)-[:R*1..2]->(b:A) RETURN a.id, b.id ORDER BY a.id, b.id",
    );
    let pairs: Vec<(i64, i64)> = result
        .rows
        .iter()
        .map(|row| {
            let Value::Int(left) = row[0] else {
                panic!("left cell is not an integer: {row:?}");
            };
            let Value::Int(right) = row[1] else {
                panic!("right cell is not an integer: {row:?}");
            };
            (left, right)
        })
        .collect();
    assert_eq!(pairs, vec![(1, 2), (1, 3), (2, 3)]);
}

#[test]
fn order_by_a_projected_alias_property_uses_that_value() {
    let g = Graph::default();
    exec_autocommit(&g, "CREATE (:A {k: 'b'}), (:A {k: 'a'})").unwrap();
    let result = exec_cypher(&g, "MATCH (n:A) RETURN {k: n.k} AS m ORDER BY m.k");
    let keys: Vec<&str> = result
        .rows
        .iter()
        .map(|row| {
            let Value::Map(map) = &row[0] else {
                panic!("expected a map: {row:?}");
            };
            let Value::String(k) = map.get("k").expect("k") else {
                panic!("expected a string key: {row:?}");
            };
            k.as_ref()
        })
        .collect();
    assert_eq!(keys, vec!["a", "b"]);
}

#[test]
fn create_keeps_driving_rows_on_the_peak() {
    struct Pool {
        reserved: std::sync::atomic::AtomicU64,
        capacity: u64,
    }
    impl cypherlite_core::tokens::PeakPool for Pool {
        fn try_grow(&self, extra: u64) -> bool {
            let now = self.reserved.load(std::sync::atomic::Ordering::SeqCst);
            if now + extra > self.capacity {
                return false;
            }
            self.reserved
                .fetch_add(extra, std::sync::atomic::Ordering::SeqCst);
            true
        }
        fn release(&self, amount: u64) {
            if amount > 0 {
                self.reserved
                    .fetch_sub(amount, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
    let g = Graph::default();
    exec_autocommit(&g, "UNWIND range(0, 127) AS i CREATE (:Cal {id: i})").unwrap();
    let pool = Arc::new(Pool {
        reserved: std::sync::atomic::AtomicU64::new(0),
        capacity: u64::MAX / 4,
    });
    assert!(cypherlite_core::tokens::PeakPool::try_grow(
        pool.as_ref(),
        1
    ));
    let mut ledger = cypherlite_core::tokens::TokenLedger::new(u64::MAX / 4, 1, pool);
    let _guard = cypherlite_core::tokens::LedgerGuard::install(&mut ledger);
    let scratch = write_scratch(&g);
    exec_write(
        &g,
        &scratch,
        "MATCH (s:Cal) CREATE (:CalCreate {id: s.id}) RETURN count(*)",
    )
    .unwrap();
    let s = scratch.borrow();
    let scratch_bytes = crate::workspace::scratch_created_bytes(
        &s.batch.nodes,
        &s.batch.rels,
        &s.batch.created_nodes,
        &s.batch.created_rels,
        &s.batch.out_adj,
        &s.batch.in_adj,
        s.redo.reserved_bytes(),
    );
    let (_, peak, _) = cypherlite_core::tokens::workspace_meter().unwrap();
    // The driving MATCH vector stays billed for the whole CREATE, on top
    // of the created nodes. 128 stub rows are several KB past the scratch.
    assert!(
        peak > scratch_bytes + 4_096,
        "peak {peak} did not keep the driving rows above scratch {scratch_bytes}"
    );
}
