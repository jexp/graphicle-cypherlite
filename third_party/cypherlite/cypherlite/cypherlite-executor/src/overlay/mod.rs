//! Private write overlay for optimistic execute (ADR-0016).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;

use cypherlite_core::{CancelToken, Error, Namespace, NodeId, RelId, Result, Value};
use cypherlite_ir::Direction;

use crate::csv::CsvReader;
use crate::graph::{Graph, Procedure, QueryStats, SchemaSnapshot};

/// The distinct labels of `labels`, in first-occurrence order — the one
/// dedup every label-accounting walk rides (live-label bumps, net
/// label-delta accounting, per-node counter accounting). Shared by the
/// scratch (its label accounting) and the write verbs (`writes.rs`).
fn distinct_labels(labels: &[String]) -> Vec<&str> {
    let mut seen = HashSet::new();
    labels
        .iter()
        .map(String::as_str)
        .filter(|label| seen.insert(*label))
        .collect()
}

/// ADR-0395: the fixed per-top-level-statement budget on nested
/// clause-engine part passes — every `execute_query_seeded` part
/// iteration (a CALL body per imported row, an import-invariant CALL's
/// shared execution, a conditional's taken branch bodies, the
/// COUNT/COLLECT expression-subquery engines, the NEXT-stage recursion;
/// a UNION body ticks once per part, so a wide body costs its part
/// count) plus the direct part passes of the EXISTS/COUNT paths and
/// the FOREACH arm (one `run_nested_clauses` call each). The runtime
/// twin of the compile-side `MAX_CLONED_NODES` (ADR-0382), at the same
/// number: a fixed constant, no knob — the remedy for legitimate work
/// that needs more is a new ADR, never ambient configuration.
pub const NESTED_EVAL_BUDGET: u64 = 100_000;

/// The per-top-level-statement nested-evaluation counter (ADR-0395): one
/// shared cell per statement, owned by the execution view beside
/// `cancel_ticks` — the one per-statement state container both the
/// statement engine and the expression layer reach — so nesting never
/// resets it: every nested entry ticks [`ExecGraph::nested_evals`], never
/// a context-local cell. A `Cell` suffices for the same reason the
/// cancellation window's does: a statement's executor access is
/// single-threaded.
#[derive(Debug, Default)]
pub(crate) struct NestedEvalCounter(Cell<u64>);

impl NestedEvalCounter {
    /// Zero the counter: the per-statement reset a reused view runs at
    /// statement entry (freshly constructed views start at zero
    /// already).
    pub(crate) fn reset(&self) {
        self.0.set(0);
    }

    /// The passes counted so far — the test-side read of the budget's
    /// counter, for asserting the counting rule's exact tick counts.
    #[cfg(test)]
    pub(crate) fn count(&self) -> u64 {
        self.0.get()
    }

    /// Count one nested statement-engine pass; the authored typed error
    /// when the pass would exceed [`NESTED_EVAL_BUDGET`]. The message's
    /// `Kind:` prefix is the token the error channel classifies
    /// (`ErrorKind::NestedEvaluationBudgetExceeded`, ADR-0395).
    pub(crate) fn tick(&self) -> Result<()> {
        let passes = self.0.get() + 1;
        if passes > NESTED_EVAL_BUDGET {
            return Err(Error::execution(format!(
                "NestedEvaluationBudgetExceeded: nested evaluation exceeds \
                 {NESTED_EVAL_BUDGET} passes per statement; nested subqueries \
                 compound per level (security budget)."
            )));
        }
        self.0.set(passes);
        Ok(())
    }
}

/// Read view of committed graph plus optional overlay (ADR-0016).
pub struct ExecGraph<'a> {
    base: &'a Graph,
    schema: Option<&'a SchemaSnapshot>,
    scratch: Option<&'a RefCell<WriteScratch>>,
    pinned_ts: u64,
    /// Cooperative cancellation flag for this statement (ADR-0111). `None`
    /// for views that do not participate in cancellation (tests, diagnostics,
    /// and the internal write views that never outlive a checked statement).
    cancel: Option<&'a CancelToken>,
    /// Statement-wide cancellation sampling window (ADR-0111). One counter
    /// shared by every hot loop in the statement — scans, folds, and the
    /// recursive var-length walks — so recursion depth cannot reset the
    /// window and hide a cancellation. A `Cell` suffices: a statement's
    /// executor access is single-threaded (the query worker thread), and the
    /// `&RefCell<WriteScratch>` overlay already makes this view `!Sync`.
    cancel_ticks: Cell<usize>,
    /// The per-statement nested-evaluation counter (ADR-0395): one shared
    /// cell per top-level statement. The view owns it beside
    /// `cancel_ticks` — every nested entry ticks [`Self::nested_evals`],
    /// so nesting compounds on one counter and no nested context can
    /// reset it.
    nested_evals: NestedEvalCounter,
    /// The store's ontology document as the calling transaction sees it
    /// (ADR-0121 §3, ADR-0227): per-admission state attached like the
    /// cancel token ([`Self::with_ontology_document`]), read by the
    /// `db.ontology.get` / `db.ontology.apply` procedure arms.
    /// `None` = no ontology set (also every test/diagnostic view).
    ontology_document: Option<&'a str>,
    pub(crate) csv: CsvReader<'a>,
}

/// Sampling mask for cooperative cancellation checks on hot per-row loops
/// (ADR-0111). `ticks & CANCEL_SAMPLE_MASK == 0` checks the token roughly
/// once per 1024 iterations, so the atomic load is not paid on every row.
const CANCEL_SAMPLE_MASK: usize = 1023;

impl<'a> ExecGraph<'a> {
    /// Schema-less read view of the committed graph. `show_schema` falls back
    /// to the live committed theater (under its lock); use
    /// [`Self::with_schema`] when a pinned admission snapshot is
    /// required. This is the constructor for tests and callers that never
    /// consult committed SHOW metadata.
    pub fn read(base: &'a Graph) -> Self {
        Self::read_at(base, base.commit_ts())
    }
    /// The base graph this snapshot reads — estimate-time consumers
    /// (the live-graph stats, LABS-351) need the base's index counts.
    pub(crate) fn base(&self) -> &'a Graph {
        self.base
    }

    /// The count-only statistics short-circuit's exactness precondition
    /// (LABS-370 v1 follow-up): this view reads the base's latest
    /// published state with nothing the base stats cannot see — no
    /// scratch overlay (an explicit transaction's or write statement's
    /// own writes live there and the base stats cannot see them), no
    /// in-flight base write window (`apply_redo` bumps the stats before
    /// it publishes), and no pinned older snapshot (a concurrent apply
    /// advances the base stats past an older pin). Any other view
    /// declines the short-circuit to the scan, which reads its own
    /// writes. The predicate is checked twice (LABS-370 round 3, F-1-5):
    /// once up front as a cheap filter, then again under BOTH stats-map
    /// locks by the reader's consistency window
    /// (`Graph::windowed_stats_count`) — a write that opens between the
    /// two checks is closed by the under-lock re-check, so no
    /// out-of-snapshot write leaks into the count.
    pub(crate) fn sees_exact_base_stats(&self) -> bool {
        self.scratch.is_none()
            && !self.base.in_write_window()
            && self.pinned_ts == self.base.commit_ts()
    }

    /// The store's `CYPHERLITE_PREVIEW`-resolved set (ADR-0371), the
    /// executor-side gate the graph-type semantic layer reads for the
    /// stored-state half of the graph-type trio's gates.
    pub(crate) fn preview(&self) -> cypherlite_core::PreviewFeatures {
        self.base.preview()
    }

    /// The scratch-aware property-defaults gate (CIP-262, ADR-0388
    /// review F-2-2): the committed theater's cheap answer
    /// (`Graph::has_property_defaults`), or a `SetCurrentGraphType` in
    /// this statement's unvalidated redo slice whose body could declare
    /// or inherit a default (round 5 deleted the `PutGraphType`
    /// alternative -- the only op that can change the current type). The one yes/no answer
    /// over both vintages, so the committed-theater half can never miss
    /// a default this very statement set; the statement boundary's
    /// default application consults the same gate.
    pub(crate) fn has_property_defaults(&self) -> bool {
        if self.base.has_property_defaults() {
            return true;
        }
        self.unvalidated_redo(|ops| {
            ops.iter().any(|op| match op {
                // SetCurrentGraphType is the only op that can change the
                // current graph type property defaults apply from (the
                // round-5 finding: the PutGraphType alternative never
                // changes this gate's decision).
                crate::redo::RedoOp::SetCurrentGraphType { body } => {
                    crate::graph::spec_has_property_defaults(body)
                }
                _ => false,
            })
        })
        .0
    }

    /// `read` at an explicit snapshot timestamp; same schema-less invariant.
    pub fn read_at(base: &'a Graph, snapshot_ts: u64) -> Self {
        Self::new_view(base, snapshot_ts, None)
    }

    /// Pin a schema snapshot on a view (combinator mirroring
    /// [`Self::with_cancel`]): `ExecGraph::read_at(base, ts).with_schema(schema)`.
    pub fn with_schema(mut self, schema: &'a SchemaSnapshot) -> Self {
        self.schema = Some(schema);
        self
    }

    /// Schema-less write view of the committed graph plus overlay.
    /// `show_schema`/`validate_constraints` fall back to the live committed
    /// theater, reached in two regimes: the autocommit path reaches it
    /// while holding the publication read guard, and ADR-0210 batched
    /// statements (`CALL { … } IN TRANSACTIONS`) reach it unguarded —
    /// pinned on the graph version but racing a concurrent schema change.
    /// A caller that needs a pinned admission adds it with
    /// [`Self::with_schema`].
    pub fn write(base: &'a Graph, scratch: &'a RefCell<WriteScratch>) -> Self {
        let pinned_ts = scratch.borrow().snapshot_ts();
        Self::new_view(base, pinned_ts, Some(scratch))
    }

    /// The constructor core the two view kinds share: every field but the
    /// pin and the overlay starts identically (schema-less, cancellation
    /// unattached, nesting and ontology unset), so a new field joins every
    /// view by construction instead of being hand-synced across the
    /// `read_at`/`write` literals.
    fn new_view(
        base: &'a Graph,
        pinned_ts: u64,
        scratch: Option<&'a RefCell<WriteScratch>>,
    ) -> Self {
        Self {
            base,
            schema: None,
            scratch,
            pinned_ts,
            cancel: None,
            cancel_ticks: Cell::new(0),
            nested_evals: NestedEvalCounter::default(),
            ontology_document: None,
            csv: CsvReader::new(base.csv_policy()),
        }
    }

    /// Which fixed namespace of the store the underlying graph is
    /// (ADR-0116 §1). Used for namespace-qualified element ids (§7).
    pub(crate) fn namespace(&self) -> Namespace {
        self.base.namespace()
    }

    /// Attach a cooperative cancellation flag to this view (ADR-0111).
    /// Construction sites that do not thread a token leave the view
    /// uncancellable, which preserves the existing behavior.
    pub fn with_cancel(mut self, cancel: Option<&'a CancelToken>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Attach the ontology document the calling transaction sees
    /// (ADR-0121 §3, ADR-0227): the facade's admission-time capture for
    /// auto-commit statements, the begin-time capture for explicit
    /// transactions. Per-admission state on the view, like
    /// [`Self::with_cancel`] (ADR-0111); the `db.ontology.get` /
    /// `db.ontology.apply` procedure arms read it through
    /// [`Self::ontology_document`]. A view without one behaves as if the
    /// store holds no ontology, which preserves test/diagnostic behavior.
    pub fn with_ontology_document(mut self, document: Option<&'a str>) -> Self {
        self.ontology_document = document;
        self
    }

    /// The ontology document attached by [`Self::with_ontology_document`].
    pub(crate) fn ontology_document(&self) -> Option<&'a str> {
        self.ontology_document
    }

    /// Check the attached [`CancelToken`], returning
    /// `Error::Cancelled` (`query cancelled`) once it is cancelled. Called at
    /// row-production points and inside long loops; a view without a token is
    /// never cancelled. See ADR-0111.
    #[inline]
    pub(crate) fn check_cancelled(&self) -> Result<()> {
        match self.cancel {
            Some(token) if token.is_cancelled() => Err(cypherlite_core::Error::cancelled()),
            _ => Ok(()),
        }
    }

    /// Sample one hot-loop iteration against the statement-wide cancellation
    /// window (ADR-0111).
    ///
    /// This is the single home of the sampling policy: cancellation is
    /// checked at statement entry, at the top of every clause, and — via
    /// this method — inside sampled hot loops, once every
    /// [`CANCEL_SAMPLE_MASK`] + 1 ticks. The counter lives on the statement's
    /// view, not on the call site, so recursive walks (var-length expansion
    /// in `exec/expand.rs`, `eval/pattern.rs`, `exec/gpm.rs`) accumulate
    /// ticks across frames: a fresh per-frame window would let a
    /// bounded-degree `()-[*1..n]->()` walk explore combinatorially many
    /// frames that each stay under the window and never observe
    /// cancellation. The counter wraps so an unbounded loop is safe; every
    /// `CANCEL_SAMPLE_MASK + 1`th tick calls [`Self::check_cancelled`],
    /// which surfaces the typed `Error::Cancelled` (`query cancelled`) once
    /// the token is cancelled.
    #[inline]
    pub(crate) fn cancel_tick(&self) -> Result<()> {
        cypherlite_core::tokens::poll()?;
        let ticks = self.cancel_ticks.get().wrapping_add(1);
        self.cancel_ticks.set(ticks);
        if ticks & CANCEL_SAMPLE_MASK == 0 {
            self.check_cancelled()?;
        }
        Ok(())
    }

    /// The per-statement nested-evaluation counter (ADR-0395): the one
    /// cell every nested entry ticks — the seeded statement engine and
    /// the direct clause-engine paths alike — so all nesting compounds
    /// on the statement's single budget.
    pub(crate) fn nested_evals(&self) -> &NestedEvalCounter {
        &self.nested_evals
    }

    /// Overlay scratch adj is unordered (ADR-0031 / ADR-0035).
    pub(crate) fn has_write_overlay(&self) -> bool {
        self.scratch.is_some()
    }

    pub(crate) fn overlay_changes_node_membership(&self) -> bool {
        self.scratch
            .is_some_and(|cell| cell.borrow().changes_node_membership())
    }

    fn snapshot_ts(&self) -> u64 {
        self.scratch
            .map(|s| s.borrow().snapshot_ts())
            .unwrap_or(self.pinned_ts)
    }

    fn scratch_mut(&self) -> std::cell::RefMut<'_, WriteScratch> {
        self.scratch
            .expect("write on read-only ExecGraph")
            .borrow_mut()
    }

    /// The one per-statement reset of the statement's own facts: the
    /// ADR-0309 mutation counters and the ADR-0402 applied-change count
    /// restart together at statement entry, so a view reused across
    /// statements reports this statement's counters and its own applied count
    /// — never the previous statement's. A read view carries no scratch and
    /// has nothing to reset.
    pub(crate) fn reset_statement(&self) {
        if let Some(scratch) = self.scratch {
            scratch.borrow_mut().reset_statement();
        }
    }

    /// One statement's entry reset: the CSV memo, the statement-spanning
    /// scratch state (the ADR-0309 counters and the ADR-0402 applied count in
    /// one scratch reset) and the ADR-0395 nested-evaluation budget restart
    /// together. Every statement entry calls this and nothing else.
    pub(crate) fn begin_statement(&self) {
        self.csv.begin_statement();
        self.reset_statement();
        self.nested_evals().reset();
    }

    pub fn stats(&self) -> QueryStats {
        self.scratch
            .map(|s| s.borrow().statement.stats.clone())
            .unwrap_or_default()
    }

    /// How many changes the current statement has applied so far
    /// (ADR-0402 Decision 2): the count the statement entry reset zeroed and
    /// every redo push grows, so a non-zero count is the statement's applied
    /// fact. A read view applies nothing and reads zero.
    pub(crate) fn applied_ops(&self) -> u64 {
        self.scratch.map(|s| s.borrow().applied_ops()).unwrap_or(0)
    }

    pub(crate) fn scratch_snapshot(&self) -> Option<ScratchCheckpoint> {
        self.scratch.map(|scratch| scratch.borrow().checkpoint())
    }

    /// Roll the scratch back to a captured [`Self::scratch_snapshot`]:
    /// symmetric with the capture — a read view (no scratch) or a `None`
    /// snapshot (a failed batch on a read view) is the same no-op, handled
    /// once at this layer instead of re-decided at every caller.
    pub(crate) fn restore_scratch(&self, snapshot: Option<ScratchCheckpoint>) {
        if let (Some(scratch), Some(snapshot)) = (self.scratch, snapshot) {
            scratch.borrow_mut().restore(snapshot);
        }
    }

    /// ADR-0210 per-batch commit for `CALL { … } IN TRANSACTIONS`: hand the
    /// current batch's redo ops to the durable-commit sink, then
    /// [`WriteScratch::rebase`] the scratch at the new commit timestamp so
    /// live overlay state stays bounded by one batch while the
    /// statement-spanning state (stats, id allocators) carries over. A
    /// read view (no scratch) or an empty batch (no ops) is a no-op.
    pub(crate) fn commit_batch(&self, sink: &dyn crate::ctx::BatchCommit) -> Result<()> {
        let Some(scratch) = self.scratch else {
            return Ok(());
        };
        let (ops, ts) = {
            let s = scratch.borrow();
            (s.redo_ops(), s.snapshot_ts())
        };
        if ops.is_empty() {
            return Ok(());
        }
        // The redo is drained only as part of the post-commit rebase
        // below: a failed commit must leave the scratch (entity maps +
        // redo) intact for the caller's abort/restore path, not
        // half-updated with the redo gone.
        let commit_ts = sink.commit_batch(ops, ts)?;
        scratch.borrow_mut().rebase(commit_ts);
        Ok(())
    }

    pub fn procedure(&self, name: &str) -> Option<Procedure> {
        self.base.procedure(name)
    }

    /// Materialize a node as a `Value` for result rows and entity builtins
    /// (`startNode()`/`endNode()`). Falls back to the virtual-node side
    /// table (`virtual_nodes`, CIP-257) when the store/overlay lookup
    /// misses, so endpoints of synthesized visualization relationships
    /// resolve without a store backing. This is the one graph-level lookup
    /// shared by the entity builtins and the visualization rows. Unlike
    /// [`Self::node`], which write paths use and which must keep missing on
    /// virtual ids (CIP-257 "Write behavior on virtual entities"), this
    /// read-path materialization is the only place the fallback is
    /// consulted.
    pub fn node_value(&self, id: NodeId) -> Value {
        if let Some(node) = self.node(id) {
            return Value::from_node(node);
        }
        if let Some(value @ cypherlite_model::CypherValue::Node { .. }) = lookup_virtual_node(id) {
            return cypherlite_core::cypher_to_value(&value);
        }
        Value::Null
    }

    pub fn rel_value(&self, id: RelId) -> Value {
        self.rel(id)
            .map(Value::from_relationship)
            .unwrap_or(Value::Null)
    }

    pub fn is_deleted_node(&self, id: NodeId) -> bool {
        self.scratch
            .map(|s| s.borrow().batch.deleted_nodes.contains(&id))
            .unwrap_or(false)
    }

    pub fn is_deleted_rel(&self, id: RelId) -> bool {
        self.scratch
            .map(|s| s.borrow().batch.deleted_rels.contains(&id))
            .unwrap_or(false)
    }

    /// Every live node id, ascending: the snapshot's ids minus the scratch's
    /// deletions plus its creations (ADR-0173). A copy-on-write entry was
    /// read from the snapshot, so it is a snapshot id already, and a delete
    /// drops the id from `created_nodes`. The two sets meet only if an
    /// allocator handed out a committed id; `dedup` keeps that to one entry.
    pub fn all_node_ids(&self) -> Vec<NodeId> {
        let mut ids = self.base.all_node_ids_at(self.snapshot_ts());
        if let Some(cell) = self.scratch {
            let s = cell.borrow();
            ids.retain(|id| !s.batch.deleted_nodes.contains(id));
            ids.extend(s.batch.created_nodes.iter().copied());
            ids.sort_unstable();
            ids.dedup();
        }
        ids
    }

    /// Every live relationship id, ascending; built like
    /// [`Self::all_node_ids`] from `deleted_rels` and `created_rels`.
    pub fn all_rel_ids(&self) -> Vec<RelId> {
        let mut ids = self.base.all_rel_ids_at(self.snapshot_ts());
        if let Some(cell) = self.scratch {
            let s = cell.borrow();
            ids.retain(|id| !s.batch.deleted_rels.contains(id));
            ids.extend(s.batch.created_rels.iter().copied());
            ids.sort_unstable();
            ids.dedup();
        }
        ids
    }

    pub fn neighbors(&self, id: NodeId, dir: Direction) -> Vec<RelId> {
        let mut out = Vec::new();
        self.for_each_neighbor(id, dir, &[], |rid| out.push(rid));
        out
    }

    /// Overlay-visible neighbors without cloning the base adjacency `Vec`.
    pub fn for_each_neighbor(
        &self,
        id: NodeId,
        dir: Direction,
        types: &[String],
        mut f: impl FnMut(RelId),
    ) {
        self.try_for_each_neighbor(id, dir, types, |rid| {
            f(rid);
            Ok::<(), ()>(())
        })
        .ok();
    }

    /// Approximate adjacency degree (see [`Graph::adj_len`]) — side
    /// selection for existence probes (LABS-193). Base delegation only:
    /// deliberately not snapshot-filtered (reads.rs's prologue contract).
    pub fn adj_len(&self, id: NodeId, dir: Direction) -> usize {
        self.base.adj_len(id, dir)
    }

    /// See [`Graph::typed_adj_len`]. Base delegation only, like [`Self::adj_len`].
    pub fn typed_adj_len(&self, id: NodeId, dir: Direction, types: &[String]) -> usize {
        self.base.typed_adj_len(id, dir, types)
    }

    /// The first neighbor of `id` (in `dir`, of `types`) for which `f`
    /// returns `true`, stopping the walk there — the early-exit idiom for
    /// existence probes (LABS-193). No match: `Ok(None)`.
    pub fn try_find_neighbor<E>(
        &self,
        id: NodeId,
        dir: Direction,
        types: &[String],
        mut f: impl FnMut(RelId) -> std::result::Result<bool, E>,
    ) -> std::result::Result<Option<RelId>, E> {
        enum Stop<E> {
            Hit(RelId),
            Err(E),
        }
        let walk: std::result::Result<(), Stop<E>> =
            self.try_for_each_neighbor(id, dir, types, |rid| match f(rid) {
                Ok(true) => Err(Stop::Hit(rid)),
                Ok(false) => Ok(()),
                Err(e) => Err(Stop::Err(e)),
            });
        match walk {
            Ok(()) => Ok(None),
            Err(Stop::Hit(rid)) => Ok(Some(rid)),
            Err(Stop::Err(e)) => Err(e),
        }
    }

    pub fn try_for_each_neighbor<E>(
        &self,
        id: NodeId,
        dir: Direction,
        types: &[String],
        mut f: impl FnMut(RelId) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        let ts = self.snapshot_ts();
        let extras = self.scratch.map(|cell| {
            let s = cell.borrow();
            let mut v = Vec::new();
            match dir {
                Direction::Outgoing => {
                    if let Some(xs) = s.batch.out_adj.get(&id) {
                        v.extend_from_slice(xs);
                    }
                }
                Direction::Incoming => {
                    if let Some(xs) = s.batch.in_adj.get(&id) {
                        v.extend_from_slice(xs);
                    }
                }
                Direction::Both => {
                    if let Some(xs) = s.batch.out_adj.get(&id) {
                        v.extend_from_slice(xs);
                    }
                    if let Some(xs) = s.batch.in_adj.get(&id) {
                        v.extend_from_slice(xs);
                    }
                }
            }
            v
        });
        let mut seen = HashSet::new();
        self.base
            .try_for_each_neighbor_at(id, dir, ts, types, |rid| {
                // rel_exists is the canonical existence check: a rel deleted in
                // the overlay (or absent from the base) is not a neighbor.
                if !self.rel_exists(rid) {
                    return Ok(());
                }
                seen.insert(rid);
                f(rid)
            })?;
        for rid in extras.unwrap_or_default() {
            if !seen.insert(rid) {
                continue;
            }
            // The stub answers existence + type without cloning the
            // property map.
            let Some(rel) = self.rel_stub(rid) else {
                continue;
            };
            if !types.is_empty() && !types.iter().any(|t| t == &rel.rel_type) {
                continue;
            }
            f(rid)?;
        }
        Ok(())
    }
}

mod catalog;
mod label_probe;
mod materialize;
mod prop_ix;
mod reads;
mod redo_log;
mod rel_seek;
mod scratch;
mod seek;
mod validation;
mod virtual_nodes;
mod writes;

pub(crate) use materialize::MaterializeCache;
pub(crate) use redo_log::ValidatedCatalog;
pub(crate) use virtual_nodes::{lookup_virtual_node, register_virtual_node};

use scratch::ScratchCheckpoint;
pub use scratch::WriteScratch;

#[cfg(test)]
mod tests;
