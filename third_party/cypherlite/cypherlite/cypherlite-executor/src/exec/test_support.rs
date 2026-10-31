//! Shared test helpers: lower Cypher with the product front-end and run it
//! through `execute_read_at` at a freshly pinned snapshot, or through
//! `execute` (or `execute_unvalidated`) on a write scratch whose redo a test
//! then commits; and build name-keyed or index-keyed graphs.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use cypherlite_core::{NodeId, RelId, Result, Value};
use cypherlite_front::parse_and_lower;
use cypherlite_ir::Query;
use cypherlite_planner::plan;

use crate::graph::Graph;
use crate::overlay::{ExecGraph, WriteScratch};

use super::{
    execute, execute_read_at, execute_unvalidated, ExecRequest, QueryResult, ReadAdmission,
};

fn exec_lowered(g: &Graph, q: Query, cypher: &str) -> QueryResult {
    try_exec_lowered(g, q, cypher, &BTreeMap::new()).unwrap()
}

/// The fallible form of [`exec_lowered`]: parse/lowering, planning, and
/// execution errors propagate instead of unwrapping, so tests can assert a
/// query's error message whichever phase raises it.
fn try_exec_lowered(
    g: &Graph,
    q: Query,
    cypher: &str,
    params: &BTreeMap<String, Value>,
) -> Result<QueryResult> {
    let p = plan(q, &mut |_| false)?;
    let pin = g.pin_now()?;
    let schema = g.schema_snapshot();
    execute_read_at(&ReadAdmission {
        graph: g,
        snapshot_ts: pin.ts(),
        schema: &schema,
        request: ExecRequest::new(&p, params, cypher),
        cancel: None,
        ontology_document: None,
    })
}

pub(crate) fn exec_cypher(g: &Graph, cypher: &str) -> QueryResult {
    exec_lowered(g, parse_and_lower(cypher).unwrap(), cypher)
}

/// The fallible form of [`exec_cypher`] (ADR-0175's integer-argument-site
/// tests assert errors raised at plan time and at runtime).
pub(crate) fn try_exec_cypher(g: &Graph, cypher: &str) -> Result<QueryResult> {
    try_exec_cypher_params(g, cypher, &BTreeMap::new())
}

/// [`try_exec_cypher`] with statement parameters.
pub(crate) fn try_exec_cypher_params(
    g: &Graph,
    cypher: &str,
    params: &BTreeMap<String, Value>,
) -> Result<QueryResult> {
    try_exec_lowered(g, parse_and_lower(cypher)?, cypher, params)
}

/// Run `cypher` on an existing view through `execute`, so the test can
/// inspect per-view state (e.g. the CSV reader's physical-read count) after
/// the run — the view-bound counterpart of [`exec_cypher`].
pub(crate) fn exec_cypher_on(graph: &ExecGraph<'_>, cypher: &str) -> QueryResult {
    try_exec_cypher_on(graph, cypher).unwrap()
}

/// The fallible form of [`exec_cypher_on`].
pub(crate) fn try_exec_cypher_on(graph: &ExecGraph<'_>, cypher: &str) -> Result<QueryResult> {
    let p = plan(parse_and_lower(cypher)?, &mut |_| false)?;
    execute(graph, &ExecRequest::new(&p, &BTreeMap::new(), cypher))
}

/// A write scratch reading at `ts`, its id allocators continuing the graph's.
pub(crate) fn write_scratch_at(g: &Graph, ts: u64) -> RefCell<WriteScratch> {
    RefCell::new(WriteScratch::new(
        ts,
        Arc::new(AtomicU64::new(g.next_node_id() as u64)),
        Arc::new(AtomicU64::new(g.next_rel_id() as u64)),
    ))
}

/// A write scratch reading at the graph's current commit.
pub(crate) fn write_scratch(g: &Graph) -> RefCell<WriteScratch> {
    write_scratch_at(g, g.commit_ts())
}

/// Run `cypher` on `scratch` through `execute`, constraint validation
/// included. Nothing reaches the graph until `commit_scratch`.
pub(crate) fn exec_write(
    g: &Graph,
    scratch: &RefCell<WriteScratch>,
    cypher: &str,
) -> Result<QueryResult> {
    run_write(g, scratch, cypher, execute)
}

/// [`exec_write`] through `execute_unvalidated`: the writes stay in the
/// slice the next constraint validation of `scratch` checks.
pub(crate) fn exec_write_unvalidated(
    g: &Graph,
    scratch: &RefCell<WriteScratch>,
    cypher: &str,
) -> Result<QueryResult> {
    run_write(g, scratch, cypher, execute_unvalidated)
}

/// One autocommit statement: `cypher` on a fresh scratch through
/// `execute`, committed when it succeeds and discarded when it fails.
pub(crate) fn exec_autocommit(g: &Graph, cypher: &str) -> Result<QueryResult> {
    let scratch = write_scratch(g);
    let result = exec_write(g, &scratch, cypher)?;
    commit_scratch(g, scratch);
    Ok(result)
}

/// [`exec_write`] on a fresh scratch, unwrapped; the scratch is dropped
/// without committing, so the graph is untouched. The uncommitted
/// counterpart of [`exec_autocommit`].
pub(crate) fn exec_write_uncommitted(g: &Graph, cypher: &str) -> QueryResult {
    exec_write(g, &write_scratch(g), cypher).unwrap()
}

fn run_write(
    g: &Graph,
    scratch: &RefCell<WriteScratch>,
    cypher: &str,
    run: fn(&ExecGraph<'_>, &ExecRequest<'_>) -> Result<QueryResult>,
) -> Result<QueryResult> {
    let p = plan(parse_and_lower(cypher).unwrap(), &mut |_| false).unwrap();
    run(
        &ExecGraph::write(g, scratch),
        &ExecRequest::new(&p, &BTreeMap::new(), cypher),
    )
}

/// Apply `scratch`'s redo to the graph as the next commit. A scratch that
/// wrote nothing commits nothing, as a read-only statement does.
pub(crate) fn commit_scratch(g: &Graph, scratch: RefCell<WriteScratch>) {
    let ops = scratch.into_inner().take_redo();
    if !ops.is_empty() {
        g.apply_redo(ops, g.commit_ts() + 1).unwrap();
    }
}

/// A list of integers.
pub(crate) fn ints(xs: &[i64]) -> Value {
    Value::List(xs.iter().map(|&x| Value::Int(x)).collect::<Vec<_>>().into())
}

/// A duration value for the aggregation test modules (one definition,
/// three call sites: aggregate_tests, pushdown_tests, match_clause).
pub(crate) fn duration(months: i64, days: i64, nanos: i128) -> Value {
    Value::Temporal(cypherlite_core::temporal::Temporal::Duration {
        months,
        days,
        nanos,
    })
}

/// The node id of a `RETURN n` cell (one definition for every test that
/// reads a node back out of a result row).
pub(crate) fn node_id(value: &Value) -> NodeId {
    let Value::Node(n) = value else {
        panic!("expected a node, got {value:?}");
    };
    n.id
}

/// The relationship id of a `RETURN r` cell (the `node_id` twin, same one
/// definition for every test).
pub(crate) fn rel_id(value: &Value) -> RelId {
    let Value::Relationship(r) = value else {
        panic!("expected a relationship, got {value:?}");
    };
    r.id
}

/// A built graph and its node and relationship ids, by index.
pub(crate) struct Built {
    pub graph: Graph,
    pub nodes: Vec<NodeId>,
    pub rels: Vec<RelId>,
}

/// Nodes `0..labels.len()` with `id: i` and the labels `labels[i]`, and
/// relationships `(from, to, type)` between node indexes with `k` = their
/// index.
pub(crate) fn build(labels: &[&[&str]], rels: &[(usize, usize, &str)]) -> Built {
    let graph = Graph::default();
    graph.begin_write().unwrap();
    let nodes: Vec<NodeId> = labels
        .iter()
        .enumerate()
        .map(|(i, labels)| {
            let labels = labels.iter().map(|l| l.to_string()).collect();
            let props = BTreeMap::from([("id".to_string(), Value::Int(i as i64))]);
            graph.create_node(labels, props).unwrap()
        })
        .collect();
    let rels = rels
        .iter()
        .enumerate()
        .map(|(k, &(a, b, t))| {
            let props = BTreeMap::from([("k".to_string(), Value::Int(k as i64))]);
            graph
                .create_rel(nodes[a], nodes[b], t.into(), props)
                .unwrap()
        })
        .collect();
    graph.commit_write();
    Built { graph, nodes, rels }
}

/// Builds a graph in one write, keyed by the names tests give its nodes.
pub(crate) struct Builder(Graph);

impl Builder {
    pub(crate) fn new() -> Self {
        let g = Graph::default();
        g.begin_write().unwrap();
        Self(g)
    }

    /// A node with `labels` and its name as `id`.
    pub(crate) fn node(&self, id: &str, labels: &[&str]) -> NodeId {
        let props = BTreeMap::from([("id".into(), Value::String(id.into()))]);
        self.create(labels, props)
    }

    /// A node with `labels` and no properties.
    pub(crate) fn anon(&self, labels: &[&str]) -> NodeId {
        self.create(labels, BTreeMap::new())
    }

    fn create(&self, labels: &[&str], props: BTreeMap<String, Value>) -> NodeId {
        let labels = labels.iter().map(|l| l.to_string()).collect();
        self.0.create_node(labels, props).unwrap()
    }

    /// A relationship of type `t` with `props`.
    pub(crate) fn rel(&self, from: NodeId, to: NodeId, t: &str, props: &[(&str, Value)]) {
        let props = props.iter().map(|(k, v)| (k.to_string(), v.clone()));
        self.0
            .create_rel(from, to, t.into(), props.collect())
            .unwrap();
    }

    /// An `E` relationship without properties.
    pub(crate) fn edge(&self, from: NodeId, to: NodeId) {
        self.rel(from, to, "E", &[]);
    }

    /// `E` relationships along `nodes`.
    pub(crate) fn chain(&self, nodes: &[NodeId]) {
        for pair in nodes.windows(2) {
            self.edge(pair[0], pair[1]);
        }
    }

    pub(crate) fn done(self) -> Graph {
        self.0.commit_write();
        self.0
    }
}
