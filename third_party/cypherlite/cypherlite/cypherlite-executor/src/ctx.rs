//! Shared execution-context parameter objects threaded through clause
//! execution and expression evaluation.

use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{NodeId, RelId, Result, Value};

use crate::eval::Row;
use crate::overlay::ExecGraph;
use crate::redo::RedoOp;

/// The unbounded var-length hop cap: the default `max_hops` when a
/// quantified rel gives none. One named constant for every walk engine
/// (exec/expand, eval/pattern, eval/shortest) — a cap change must not
/// desynchronize them.
pub(crate) const DEFAULT_MAX_HOPS: u32 = 32;

/// What a pattern variable is bound to in a row: the one canonical
/// spelling of the four states every binding-aware site distinguishes —
/// candidacy (exec/candidates, eval/pattern), bound_node, and the
/// existence probe all classify through this.
pub(crate) enum Binding {
    /// A row-bound node: the single candidate.
    Node(NodeId),
    /// Bound to a null or non-node value: matches nothing.
    Mismatch,
    /// A variable absent from this row.
    Unbound,
    /// A nameless endpoint.
    Anonymous,
}

impl Binding {
    /// Does a hop to `id` satisfy this binding? A row-bound node accepts
    /// only that node; a null/non-node binding accepts nothing; an
    /// absent or nameless variable accepts every node. One spelling of
    /// the accept predicate applied to a resolved endpoint (the fold's
    /// row-bound start and far node).
    pub(crate) fn accepts(self, id: NodeId) -> bool {
        match self {
            Binding::Node(b) => b == id,
            Binding::Mismatch => false,
            Binding::Unbound | Binding::Anonymous => true,
        }
    }
}

/// Classifies `np`'s variable binding in `row`.
pub(crate) fn binding(np: &cypherlite_ir::NodePattern, row: &Row) -> Binding {
    match np.variable.as_ref() {
        Some(v) => match row.get(v) {
            Some(Value::Node(n)) => Binding::Node(n.id),
            Some(_) => Binding::Mismatch,
            None => Binding::Unbound,
        },
        None => Binding::Anonymous,
    }
}

/// The node value a pattern binds `id` to (null when the stub is gone).
/// Lives here so both walk engines (eval, exec) can share it without a
/// module cycle.
pub(crate) fn bind_node_stub(graph: &ExecGraph<'_>, id: NodeId) -> Value {
    graph
        .node_stub(id)
        .map(Value::from_node)
        .unwrap_or(Value::Null)
}

/// The relationship value a pattern binds `id` to (null when the stub is
/// gone). Same sharing rationale as [`bind_node_stub`].
pub(crate) fn bind_rel_stub(graph: &ExecGraph<'_>, id: RelId) -> Value {
    graph
        .rel_stub(id)
        .map(Value::from_relationship)
        .unwrap_or(Value::Null)
}

/// The effective hop bounds of a quantified rel: the var-length clamp
/// with the DEFAULT_MAX_HOPS default. One spelling for every walk
/// engine (walk_pattern, eval_shortest, expand_rel).
pub(crate) fn hop_bounds(rp: &cypherlite_ir::RelPattern) -> (u32, u32) {
    if rp.var_length {
        (rp.min_hops, rp.max_hops.unwrap_or(DEFAULT_MAX_HOPS))
    } else {
        (1, 1)
    }
}
/// Statement-level context threaded through clause execution: the parsed
/// CIP-150 `CYPHER <version>` preamble (typed, from `Statement::cypher_version`
/// via `LogicalPlan::cypher_version`), the statement's source text (which
/// feeds `SHOW TRANSACTIONS`' `currentQuery` column), and the connection's
/// authenticated principal (ADR-0273: set by a surface that verified an
/// identity — the Bolt server after a validated HELLO — else `None`, and
/// `dbms.showCurrentUser` reports the documented placeholder).
#[derive(Clone, Copy)]
pub(crate) struct QueryCtx<'a> {
    pub cypher_version: Option<u32>,
    pub query_text: &'a str,
    pub principal: Option<&'a str>,
    /// ADR-0210: per-batch durable-commit sink for `CALL { … } IN
    /// TRANSACTIONS`. Set by the facade's auto-commit write path; `None`
    /// everywhere else (tests, nested execution, explicit transactions),
    /// where batching keeps the accumulate-in-scratch semantics.
    pub batch_commit: Option<&'a dyn BatchCommit>,
}

/// The statement's PROFILE site (ADR-0344 phase 2): the metrics sink and
/// the `(stage, part)` position its keys ride — one fact, built by the
/// statement-level walkers when they call the clause loop, so the sink
/// never travels without the position it keys.
#[derive(Clone, Copy)]
pub(crate) struct PlanMetricsSite<'a> {
    /// The sink the facade's PROFILE path attached.
    pub(crate) sink: &'a dyn PlanMetricsSink,
    /// The NEXT-chain stage (1-based, matching the tree's stage
    /// numbering): each NEXT stage restarts its part indices at 0, so the
    /// stage keeps the keys disjoint.
    pub(crate) stage: usize,
    /// The part index within the stage.
    pub(crate) part: usize,
}

impl<'a> PlanMetricsSite<'a> {
    /// The one pairing of a sink with the clause loop's `(stage, part)`
    /// key — the statement-level walkers' single construction, so a sink
    /// never travels without the position it keys.
    pub(crate) fn at(sink: &'a dyn PlanMetricsSink, stage: usize, part: usize) -> Self {
        PlanMetricsSite { sink, stage, part }
    }
}

/// The store-read mix a clause's arm performed (LABS-369): the kinds the
/// single `Db Hits` total blends. A `Rows=103` operator that spent 300k
/// relationship hops and 0 node reads is a different animal from one that
/// read 103 nodes — the one-glance diagnosis for the scan-turned-traversal
/// family.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreReads {
    /// Relationship hops taken.
    pub hops: u64,
    /// Neighbors the walk traversed (counted at the adjacency walk).
    pub neighbor_walks: u64,
    /// Node-store reads: one per candidate a node match examined (the
    /// all-nodes scan, the label scan, the index seek).
    pub node_reads: u64,
}

/// The clause's inclusive phase times (LABS-448): the elapsed time spent
/// resolving node candidates (the seek — the store work behind the
/// `Nodes` kind) and following relationships (the walk — the store work
/// behind the `Rels`/`Degree` kinds); the per-candidate admission checks
/// and the arm's binding/cloning stay unattributed. Inclusive of the nesting
/// a compound operator performs: a walk that triggers nested seeks counts
/// both, like the table's `Total` column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhaseTimes {
    /// The seek phase's elapsed nanoseconds.
    pub seek_ns: u128,
    /// The walk phase's elapsed nanoseconds.
    pub walk_ns: u128,
}

impl PhaseTimes {
    /// The phases one clause's arm performed: `self` minus the snapshot
    /// taken when the arm started. The sums only ever increase, so a
    /// decrease is a broken invariant — the same loud subtraction
    /// [`StoreReads::since`] uses, never a clamp to a fabricated zero.
    pub fn since(self, earlier: PhaseTimes) -> PhaseTimes {
        PhaseTimes {
            seek_ns: self.seek_ns - earlier.seek_ns,
            walk_ns: self.walk_ns - earlier.walk_ns,
        }
    }
}

impl StoreReads {
    /// The reads one clause's arm performed: `self` minus the snapshot
    /// taken when the arm started. The counters only ever increase, so a
    /// decrease is a broken invariant — the same loud subtraction the
    /// store-access total uses, never a clamp to a fabricated zero.
    pub fn since(self, earlier: StoreReads) -> StoreReads {
        StoreReads {
            hops: self.hops - earlier.hops,
            neighbor_walks: self.neighbor_walks - earlier.neighbor_walks,
            node_reads: self.node_reads - earlier.node_reads,
        }
    }

    /// The kinds' sum — the one `Db Hits` total, derived rather than
    /// snapshotted a second time.
    pub fn total(self) -> u64 {
        self.hops + self.neighbor_walks + self.node_reads
    }
}

/// How a `MATCH` walk chose its start element (LABS-369): the fact the
/// opaque `Expand` hid. A displaced start — a cheap index seek that became
/// a 135k-relationship backward traversal — is named here instead of
/// showing up only as a large `Db Hits` total.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalkStart {
    /// The pattern element index the walk started from.
    pub element: usize,
    /// Whether the walk ran the pivot's prefix backwards from it.
    pub backward: bool,
    /// Why that start won.
    pub reason: StartReason,
}

/// The start-selection reason classes (ADR-0350's bound pivot, ADR-0351's
/// cheapest-start estimate, the LABS-161 relationship seek, the default).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartReason {
    /// A row-bound element anchored the walk (ADR-0350).
    BoundPivot,
    /// The cheapest-start estimate chose the element (ADR-0351).
    Cheapest,
    /// A LABS-161 relationship seek anchored the walk.
    SeekAnchored,
    /// The pattern's first element: the un-optimised default.
    IndexZero,
}

impl StartReason {
    /// The reason's stable wire name (LABS-369): the vocabulary the plan
    /// args and the renderers spell, single-sourced here.
    pub fn as_str(self) -> &'static str {
        match self {
            StartReason::BoundPivot => "bound-pivot",
            StartReason::Cheapest => "cheapest",
            StartReason::SeekAnchored => "seek-anchored",
            StartReason::IndexZero => "index-0",
        }
    }
}

/// One clause operator's PROFILE metrics, reported to the sink (ADR-0344
/// phase 2 + the metric-fields phase): the position key `(stage, part,
/// clause)` — the same walk the plan tree construction uses — beside the
/// operator's actuals. Bundled so the sink signature stays at one
/// argument however many metrics a later phase adds.
pub struct ClauseMetricsRecord {
    /// The NEXT-chain position (1-based, matching the tree's stage
    /// numbering): each NEXT stage restarts its part indices at 0, so the
    /// stage keeps the keys disjoint — without it a chained statement's
    /// later stage overwrites the earlier stage's entries.
    pub stage: usize,
    pub part: usize,
    pub clause: usize,
    /// Rows the operator produced.
    pub rows: u64,
    /// Row-materialization bytes: the summed logical size
    /// (`Value::logical_bytes`, ADR-0042, plus each key's string bytes)
    /// of the rows the operator produced. Stateful operators'
    /// non-row state (aggregation tables, sort buffers) is a follow-up.
    pub memory_bytes: u64,
    /// The operator's elapsed time, nanoseconds.
    pub time_ns: u128,
    /// The store-read mix behind [`ClauseMetricsRecord::db_hits`]
    /// (LABS-369): hops vs neighbor walks vs node reads.
    pub reads: StoreReads,
    /// The clause's inclusive phase times (LABS-448): the seek (candidate
    /// resolution and checks) and the walk (relationship traversal),
    /// stamped only while a PROFILE statement's arm runs — a non-PROFILEd
    /// statement's record carries zeros.
    pub phases: PhaseTimes,
    /// How the walk chose its start, when the clause ran a `MATCH` walk
    /// (LABS-369) — `None` for clauses that walk nothing.
    pub start: Option<WalkStart>,
}

/// Per-clause PROFILE metrics sink (ADR-0344 phase 2): the dispatch loop
/// reports each statement-level clause operator's metrics once it
/// finishes, keyed `(stage, part, clause)` — the same walk the plan tree
/// construction uses, so the collected entries stamp onto the PROFILE
/// result's plan tree without a second mapping.
pub trait PlanMetricsSink {
    /// The record's `stage` is the NEXT-chain position (1-based, matching
    /// the tree's stage numbering): each NEXT stage restarts its part
    /// indices at 0, so the stage keeps the keys disjoint — without it a
    /// chained statement's later stage overwrites the earlier stage's
    /// entries.
    fn record_clause(&self, record: ClauseMetricsRecord);
}

/// Per-batch commit sink for `CALL { … } IN TRANSACTIONS` (ADR-0210).
/// Called by the executor after each successful batch with the batch's
/// drained redo ops and the snapshot timestamp they were built against;
/// the implementation commits them durably (WAL + apply) before the next
/// batch starts, so live scratch state stays bounded by one batch.
/// Returns the commit timestamp the batch was applied at — the caller
/// rebases its scratch onto it, so only the sink (which owns the commit)
/// ever reads the base graph's clock.
pub trait BatchCommit {
    fn commit_batch(&self, ops: Vec<RedoOp>, snapshot_ts: u64) -> Result<u64>;
}

impl<'a> QueryCtx<'a> {
    pub(crate) fn new(cypher_version: Option<u32>, query_text: &'a str) -> Self {
        Self {
            cypher_version,
            query_text,
            principal: None,
            batch_commit: None,
        }
    }

    /// ADR-0273: attach the connection's authenticated principal.
    pub(crate) fn with_principal(mut self, principal: Option<&'a str>) -> Self {
        self.principal = principal;
        self
    }

    /// ADR-0210: attach the per-batch commit sink.
    pub(crate) fn with_batch_commit(mut self, sink: Option<&'a dyn BatchCommit>) -> Self {
        self.batch_commit = sink;
        self
    }

    /// True when the statement carried an explicit `CYPHER 5` preamble.
    pub(crate) fn is_cypher5(&self) -> bool {
        self.cypher_version == Some(5)
    }
}

/// Graph view + query parameters shared by clause execution and evaluation.
/// `cypher_version` is the enclosing statement's CIP-150 preamble: expression
/// subqueries (EXISTS/COUNT/COLLECT) lowered with `Query::cypher_version: None`
/// observe it at evaluation time. `principal` is the connection's
/// authenticated principal (ADR-0273), read by the `dbms.showCurrentUser`
/// procedure arm; `None` when the surface set none.
#[derive(Clone, Copy)]
pub(crate) struct ExecCtx<'a> {
    pub graph: &'a ExecGraph<'a>,
    pub params: &'a BTreeMap<String, Value>,
    pub cypher_version: Option<u32>,
    pub principal: Option<&'a str>,
}

impl<'a> ExecCtx<'a> {
    pub(crate) fn new(
        graph: &'a ExecGraph<'a>,
        params: &'a BTreeMap<String, Value>,
        cypher_version: Option<u32>,
    ) -> Self {
        Self {
            graph,
            params,
            cypher_version,
            principal: None,
        }
    }

    /// ADR-0273: attach the connection's authenticated principal.
    pub(crate) fn with_principal(mut self, principal: Option<&'a str>) -> Self {
        self.principal = principal;
        self
    }
}

/// Expression-evaluation context: graph view, current row, parameters, and
/// the enclosing statement's CIP-150 `cypher_version` (see [`ExecCtx`]).
#[derive(Clone, Copy)]
pub(crate) struct EvalCtx<'a> {
    pub graph: &'a ExecGraph<'a>,
    pub row: &'a Row,
    pub params: &'a BTreeMap<String, Value>,
    pub cypher_version: Option<u32>,
}

/// Row plus the relationship IDs consumed so far along a path walk.
#[derive(Clone)]
pub(crate) struct PathRow {
    pub row: Row,
    pub used: HashSet<RelId>,
}

/// Variable-length relationship walk bounds plus the current hop cursor.
pub(crate) struct VarWalk {
    pub min_h: u32,
    pub max_h: u32,
    pub hops: u32,
    pub trail: Vec<RelId>,
}
