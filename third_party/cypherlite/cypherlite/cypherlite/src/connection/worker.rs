//! The connection's query-worker cluster (ADR-0259, ADR-0312): spawning and
//! reusing the per-connection 64 MiB worker, running statements on it, the
//! already-spawned slot lookup a heartbeat `COMMIT` uses, and the `Drop`
//! join that keeps a busy worker from outliving its connection. Every item
//! but the `Drop` join is native-only: wasm32 has no threads, so the
//! statement runs on the caller's own thread and `run_cancellable` calls
//! the one `run_query` body directly (ADR-0312).

#[cfg(not(target_arch = "wasm32"))]
use std::collections::BTreeMap;

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use crate::query_worker::{Heartbeat, JobSlot, QueryWorker};
use crate::Connection;
#[cfg(not(target_arch = "wasm32"))]
use crate::Result;
#[cfg(not(target_arch = "wasm32"))]
use crate::{CancelToken, Value};

#[cfg(not(target_arch = "wasm32"))]
use super::QueryOutcome;

impl Connection {
    /// Spawn the connection worker on first use, then run the query there,
    /// ticking `heartbeat` on this thread while it waits (ADR-0312).
    /// The `Arc` clone ends the borrow of `self` before the closure captures it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn dispatch_query(
        &mut self,
        cypher: &str,
        params: BTreeMap<String, Value>,
        token: &CancelToken,
        heartbeat: Option<Heartbeat<'_>>,
    ) -> QueryOutcome {
        let slot = match self.worker_slot() {
            Ok(slot) => slot,
            Err(e) => return QueryOutcome::not_admitted(Err(e)),
        };
        match slot.run_with_heartbeat(|| self.run_query(cypher, params, token), heartbeat) {
            Ok(outcome) => outcome,
            Err(error) => QueryOutcome::not_admitted(Err(error)),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn worker_slot(&mut self) -> Result<Arc<JobSlot>> {
        if self.query_worker.is_none() {
            self.query_worker = Some(QueryWorker::spawn()?);
            #[cfg(test)]
            {
                self.worker_spawns += 1;
            }
        }
        Ok(Arc::clone(
            &self
                .query_worker
                .as_ref()
                .expect("the worker was just spawned above")
                .slot,
        ))
    }

    /// This connection's worker slot when the worker is already running
    /// (`None` before the first query). A COMMIT with a heartbeat runs there
    /// (ADR-0312); without a worker no query ran, so there is no redo to
    /// wait on.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn spawned_worker_slot(&self) -> Option<Arc<JobSlot>> {
        self.query_worker
            .as_ref()
            .map(|worker| Arc::clone(&worker.slot))
    }

    /// Spawns performed by this connection. Stays at one across later statements.
    #[cfg(all(test, not(target_arch = "wasm32")))]
    pub(crate) fn query_worker_spawns(&self) -> u32 {
        self.worker_spawns
    }

    /// Run `f` on this connection's worker. Used to prove a panic leaves the
    /// same worker usable for the next statement.
    #[cfg(all(test, not(target_arch = "wasm32")))]
    pub(crate) fn drive_query_worker<T: Send>(
        &mut self,
        f: impl FnOnce() -> T + Send,
    ) -> Result<T> {
        let slot = self.worker_slot()?;
        slot.run_with_heartbeat(f, None)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Join the worker before releasing the transaction pin. The
            // worker is idle here: a query holds `&mut self` until it returns.
            self.query_worker.take();
        }
        if let Some(txn) = self.txn.take() {
            self.inner.graph.unpin_slot(txn.pin_slot);
        }
    }
}
