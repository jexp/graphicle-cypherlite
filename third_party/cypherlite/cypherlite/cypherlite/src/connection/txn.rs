//! The explicit-transaction cluster (ADR-0028): the open transaction's
//! pinned snapshot and write overlay, `BEGIN`/`COMMIT`/`ROLLBACK`, the
//! in-transaction dispatch, and the failure epilogue `run_query` shares.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use cypherlite_core::{Error, Result};
use cypherlite_executor::{
    execute, ExecRequest, Graph, QueryResult as ExecResult, SchemaSnapshot, WriteScratch,
};
use cypherlite_planner::LogicalPlan;

use crate::commit::try_wal_commit;
use crate::{CancelToken, Connection, Value};

use super::QueryOutcome;

/// Connection-scoped explicit transaction (ADR-0028).
pub(crate) struct OpenTxn {
    txn_id: u64,
    /// The transaction's write overlay; `pull_local_alive` and `dispatch`
    /// in the parent module read it too.
    pub(super) scratch: RefCell<WriteScratch>,
    /// The failed flag `run_query` checks before admitting a statement.
    pub(super) failed: Cell<bool>,
    /// The snapshot pin `Drop` releases after joining the worker.
    pub(super) pin_slot: usize,
    /// The store's ontology document captured at `BEGIN` (ADR-0121 §3):
    /// the document is store-level state, not graph-snapshot state, so the
    /// transaction's consistent read of it is pinned here — `db.ontology.get`
    /// and `db.ontology.apply` in this transaction see exactly this. A
    /// shared handle (pointer clone), never a document-sized copy.
    ontology_document: Option<Arc<str>>,
}

/// The open transaction plus the dispatch decisions already made from it,
/// bundled so `Connection::query_in_txn` stays under
/// `clippy::too_many_arguments` (same trick as `ResultStamp` in `dispatch`):
/// the `begin`-pinned `txn` (the open-transaction invariant, threaded by
/// value instead of re-fetched), the target graph `dispatch_target` derived
/// from it, and the schema snapshot taken under the read guard.
pub(super) struct TxnRun<'a> {
    pub(super) txn: &'a OpenTxn,
    pub(super) target: &'a Graph,
    pub(super) schema: &'a SchemaSnapshot,

    /// The statement's PROFILE sink (ADR-0344 phase 2) — the same fact
    /// `DispatchCtx` holds for the dispatch epilogue, already coerced to
    /// the sink trait in `run_query` (the one coercion site).
    pub(super) profile_metrics: Option<&'a dyn cypherlite_executor::PlanMetricsSink>,
}

impl Connection {
    /// Start an explicit transaction (ADR-0028). Nested `begin` is an error.
    pub fn begin(&mut self) -> Result<()> {
        let _read = self.inner.read_guard()?;
        if self.txn.is_some() {
            return Err(Error::semantic("already in a transaction"));
        }
        let pin = self.inner.graph.pin_now()?;
        let snapshot_ts = pin.ts();
        let pin_slot = pin.detach();
        let txn_id = self.inner.next_txn_id.fetch_add(1, Ordering::SeqCst);
        self.txn = Some(OpenTxn {
            txn_id,
            scratch: RefCell::new(WriteScratch::new(
                snapshot_ts,
                Arc::clone(&self.inner.next_node_id),
                Arc::clone(&self.inner.next_rel_id),
            )),
            failed: Cell::new(false),
            pin_slot,
            ontology_document: self.inner.ontology.get(),
        });
        Ok(())
    }

    /// Publish the transaction's redo as one WAL commit, or no-op if empty.
    pub fn commit(&mut self) -> Result<()> {
        self.inner.check_healthy()?;
        let txn = self
            .txn
            .take()
            .ok_or_else(|| Error::semantic("no transaction"))?;
        if txn.failed.get() {
            self.txn = Some(txn);
            return Err(Error::semantic("transaction failed; rollback required"));
        }
        let snapshot_ts = txn.scratch.borrow().snapshot_ts();
        let txn_id = txn.txn_id;
        let pin_slot = txn.pin_slot;
        let ops = txn.scratch.into_inner().take_redo();
        self.inner.graph.unpin_slot(pin_slot);
        if ops.is_empty() {
            return Ok(());
        }
        // The assigned commit timestamp is only needed by the per-batch
        // sink; a whole-transaction commit rebases nothing.
        try_wal_commit(&self.inner, snapshot_ts, txn_id, ops, 0, 0).map(|_| ())
    }

    /// Discard uncommitted writes. Connection drop is implicit rollback.
    pub fn rollback(&mut self) -> Result<()> {
        let Some(txn) = self.txn.take() else {
            return Err(Error::semantic("no transaction"));
        };
        self.inner.graph.unpin_slot(txn.pin_slot);
        Ok(())
    }

    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// The one error epilogue of `run_query`: fail the open transaction
    /// (if any) and return the not-admitted outcome.
    pub(super) fn fail_not_admitted(&mut self, e: Error) -> QueryOutcome {
        self.fail_txn();
        QueryOutcome::not_admitted(Err(e))
    }

    pub(super) fn fail_txn(&mut self) {
        if let Some(txn) = self.txn.as_mut() {
            txn.failed.set(true);
        }
    }

    pub(super) fn query_in_txn(
        &self,
        run: TxnRun<'_>,
        logical: &LogicalPlan,
        params: &BTreeMap<String, Value>,
        cypher: &str,
        token: &CancelToken,
    ) -> Result<ExecResult> {
        let txn = run.txn;
        if txn.failed.get() {
            return Err(Error::semantic("transaction failed; rollback required"));
        }
        let result = {
            // The transaction's write overlay applies to the writable
            // namespace (`neo4j`, the only one; ADR-0116 §5); a
            // read-only namespace (`system`) keeps a plain read view,
            // ADR-0229 §1. `Connection::statement_view` owns that choice
            // for this path and the notification derivation alike.
            // ADR-0121 §3: the begin-time ontology-document capture is
            // pinned on the view — the transaction reads it consistently
            // with its pinned snapshots.
            let view = Self::statement_view(run.target, Some(&txn.scratch))
                .with_schema(run.schema)
                .with_cancel(Some(token))
                .with_ontology_document(txn.ontology_document.as_deref());
            execute(
                &view,
                &ExecRequest {
                    plan: logical,
                    params,
                    query_text: cypher,
                    principal: self.principal.as_deref(),
                    batch_commit: None,
                    // ADR-0344 phase 2: the PROFILE sink, threaded by
                    // the caller (the dispatch's ctx, already coerced).
                    plan_metrics: run.profile_metrics,
                },
            )
        };
        match result {
            Ok(r) => Ok(r),
            Err(e) => {
                txn.failed.set(true);
                Err(e)
            }
        }
    }
}
