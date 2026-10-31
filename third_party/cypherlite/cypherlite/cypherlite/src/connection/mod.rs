//! Connection-scoped query path and explicit transactions (ADR-0016,
//! ADR-0028, ADR-0259). Extracted from the crate root to keep `lib.rs` under
//! the repository's file-size budget; the query-worker cluster lives in
//! [`worker`] and the transaction cluster in [`txn`], both under the budget.

use std::collections::BTreeMap;
use std::sync::Arc;

use cypherlite_core::{ce_messages, Namespace};
use cypherlite_executor::ExecGraph;

use crate::query_worker::Heartbeat;
use crate::{CancelToken, PullKind, QueryResult, Result, Shared, Value};

mod staged;
mod txn;
mod worker;

pub use dispatch::{Executed, ExecutionFailure};
use txn::OpenTxn;

/// Test hook for the processing-memory load test (ADR-0333). Unset in
/// production, so admission does not sleep. A positive value holds the
/// active-query slot before the statement runs, long enough for a second
/// session to overlap it.
fn admit_hold() {
    crate::token_budget::query_admit_hold();
}

/// Test hook for the processing-memory load test. Unset in production.
/// After a successful statement the process logs `logical-bytes <n>`, the
/// live graph meter ([`crate::Database::memory_usage`]).
fn log_logical_bytes_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("CYPHERLITE_LOG_LOGICAL_BYTES")
            .ok()
            .as_deref()
            == Some("1")
    })
}

fn log_logical_bytes(shared: &Shared) {
    if !log_logical_bytes_enabled() {
        return;
    }
    eprintln!("logical-bytes {}", shared.total_used_bytes());
}

/// A connection sharing the database graph.
///
/// Reads pin `commit_ts` and capture schema under a short publication guard (ADR-0094). Writes execute on a
/// private overlay (ADR-0016); `commit_ts` is assigned at WAL append (ADR-0015).
/// Independent waiters share one `sync_all` (ADR-0019). Explicit transactions
/// (ADR-0028) pin one snapshot and hold the overlay until `commit` / `rollback`.
///
/// Each connection has a session namespace (ADR-0117 §2): the namespace its
/// statements run in when they carry no `USE` clause (ADR-0116 §3).
pub struct Connection {
    pub(crate) inner: Arc<Shared>,
    pub(crate) txn: Option<OpenTxn>,
    /// ADR-0273: the authenticated principal a surface verified for this
    /// connection (the Bolt server sets it after a validated HELLO).
    /// Display-only: read by `dbms.showCurrentUser`; it confers no
    /// authorization. `None` on every surface with no notion of identity.
    pub(crate) principal: Option<String>,
    pub(crate) namespace: Namespace,
    /// ADR-0259: one 64 MiB worker for every query on this connection.
    /// `None` until the first query. Dropped (and joined) with the connection.
    #[cfg(not(target_arch = "wasm32"))]
    query_worker: Option<crate::query_worker::QueryWorker>,
    /// How many times this connection has spawned its worker. One across
    /// every later statement, including after a caught worker panic.
    #[cfg(all(test, not(target_arch = "wasm32")))]
    worker_spawns: u32,
}

mod dispatch;

use dispatch::QueryOutcome;

/// A connection with no open transaction and no query worker yet.
pub(crate) fn fresh(inner: Arc<Shared>, namespace: Namespace) -> Connection {
    Connection {
        inner,
        txn: None,
        principal: None,
        namespace,
        #[cfg(not(target_arch = "wasm32"))]
        query_worker: None,
        #[cfg(all(test, not(target_arch = "wasm32")))]
        worker_spawns: 0,
    }
}

/// Parse a session namespace name (ADR-0117 §2). Names match
/// case-insensitively; an unknown name fails with `DatabaseNotFound` and
/// CE's live text `Graph not found: <name>` (ADR-0146) — the one path both
/// `Database::connect_namespace` and `Connection::set_namespace` use.
pub(crate) fn parse_session_namespace(name: &str) -> Result<Namespace> {
    Namespace::parse(name).ok_or_else(|| ce_messages::database_does_not_exist(name))
}

impl Connection {
    /// Set (or clear, with `None`) the authenticated principal for this
    /// connection (ADR-0273).
    ///
    /// The principal is **display-only**: it is reported as the `username`
    /// column of `dbms.showCurrentUser()` and confers no authorization —
    /// CypherLite has no engine-level access control. A surface should set
    /// it only with an identity it has itself verified (the Bolt server
    /// sets it after a validated HELLO); surfaces with no notion of
    /// identity never call it, and `dbms.showCurrentUser()` then reports
    /// the documented placeholder `'cypherlite'`.
    ///
    /// ```
    /// use cypherlite::{Database, Value};
    ///
    /// let db = Database::open(":memory:").unwrap();
    /// let mut conn = db.connect().unwrap();
    /// // No principal: the documented placeholder.
    /// let result = conn.query("CALL dbms.showCurrentUser()").unwrap();
    /// assert_eq!(result.rows[0][0], Value::String("cypherlite".into()));
    /// // A verified principal is reported as `username`.
    /// conn.set_principal(Some("alice"));
    /// assert_eq!(conn.principal(), Some("alice"));
    /// let result = conn.query("CALL dbms.showCurrentUser()").unwrap();
    /// assert_eq!(result.rows[0][0], Value::String("alice".into()));
    /// // Clearing restores the placeholder.
    /// conn.set_principal(None);
    /// let result = conn.query("CALL dbms.showCurrentUser()").unwrap();
    /// assert_eq!(result.rows[0][0], Value::String("cypherlite".into()));
    /// ```
    pub fn set_principal(&mut self, principal: Option<&str>) {
        self.principal = principal.map(str::to_string);
    }

    /// The principal set on this connection, if any (ADR-0273). See
    /// [`Self::set_principal`].
    pub fn principal(&self) -> Option<&str> {
        self.principal.as_deref()
    }

    pub fn query(&mut self, cypher: &str) -> Result<QueryResult> {
        self.query_with_params(cypher, BTreeMap::new())
    }

    pub fn execute(&mut self, cypher: &str) -> Result<QueryResult> {
        self.query(cypher)
    }

    /// The session namespace this connection's statements run in when they
    /// carry no `USE` clause (ADR-0117 §2), typed (ADR-0147):
    /// `Namespace::Neo4j`, `Namespace::System` or
    /// `Namespace::SystemOntologies`.
    pub fn namespace(&self) -> Namespace {
        self.namespace
    }

    /// Change the session namespace (ADR-0117 §2). Names match
    /// case-insensitively; an unknown name fails with `DatabaseNotFound`
    /// (`Graph not found: <name>`, ADR-0146). A session
    /// namespace changes only between transactions (ADR-0116 §5): while an
    /// explicit transaction is open this fails with `Cannot change the
    /// namespace while a transaction is open.`
    ///
    /// ```
    /// # use cypherlite::{Database, Namespace};
    /// let db = Database::open(":memory:").unwrap();
    /// let mut conn = db.connect().unwrap();
    /// assert_eq!(conn.namespace(), Namespace::Neo4j);
    /// conn.set_namespace("SYSTEM").unwrap();
    /// assert_eq!(conn.namespace(), Namespace::System);
    /// conn.begin().unwrap();
    /// assert!(conn.set_namespace("neo4j").is_err());
    /// conn.rollback().unwrap();
    /// conn.set_namespace("neo4j").unwrap();
    /// ```
    pub fn set_namespace(&mut self, name: &str) -> Result<()> {
        if self.txn.is_some() {
            return Err(ce_messages::namespace_change_in_transaction());
        }
        self.namespace = parse_session_namespace(name)?;
        Ok(())
    }

    /// Local id for a pulled remote element if it still exists (ADR-0080).
    ///
    /// A poisoned/fenced handle is a typed error, distinct from a missing
    /// mapping (ADR-0099).
    pub fn pull_lookup(
        &self,
        source: &str,
        kind: PullKind,
        remote_id: &str,
    ) -> Result<Option<i64>> {
        let _read = self.inner.read_guard()?;
        let Some(local) = self.inner.graph.pull_identity_get(source, kind, remote_id) else {
            return Ok(None);
        };
        Ok(self.pull_local_alive(kind, local).then_some(local))
    }

    fn pull_local_alive(&self, kind: PullKind, id: i64) -> bool {
        if let Some(txn) = &self.txn {
            let view = ExecGraph::write(&self.inner.graph, &txn.scratch);
            return match kind {
                PullKind::Node => view.node_exists(id),
                PullKind::Rel => view.rel_exists(id),
            };
        }
        match kind {
            PullKind::Node => self.inner.graph.node_exists(id),
            PullKind::Rel => self.inner.graph.rel_exists(id),
        }
    }

    /// Record a pull identity mapping in the in-memory index — load-scoped
    /// state, never persisted (ADR-0158).
    pub fn pull_bind(
        &mut self,
        source: String,
        kind: PullKind,
        remote_id: String,
        local_id: i64,
    ) -> Result<()> {
        // The `GraphCell` deref contract: every read of the cell is under
        // `read_guard` (or a pin taken while it was held), so a Follow-mode
        // reload's `replace` cannot swap the graph out from under this
        // deref.
        let _read = self.inner.read_guard()?;
        // Writes the shared in-memory index outside any transaction: the
        // map is load-scoped metadata (ADR-0158), and pull_lookup filters
        // dead local ids on read (pull_local_alive), so a rolled-back
        // transaction can never be aliased through it.
        self.inner
            .graph
            .pull_identity_put(source, kind, remote_id, local_id);
        Ok(())
    }

    /// Plan a statement and return its plan tree (ADR-0344) without
    /// executing it — the `EXPLAIN` plan as a structured `PlanNode`, for
    /// callers that render it themselves: the CLI's terminal-width
    /// rendering, tooling, and the later `PROFILE` phases. Admission runs
    /// exactly as every statement's does — the health and
    /// failed-transaction gates, parse, lower, plan, and the compile-time
    /// gates of [`Self::admit_statement`], on the connection's 64 MiB
    /// worker (ADR-0259), never the caller's stack — so a statement that
    /// cannot be planned fails with the same typed error `EXPLAIN`
    /// reports, and a planned write leaves the graph untouched. No
    /// parameters are bound: a statement referencing one fails with the
    /// same typed error as a plain [`Self::query`] without parameters.
    pub fn explain_plan(&mut self, cypher: &str) -> Result<crate::PlanNode> {
        let result = self.dispatch_plan(cypher);
        if result.is_err() {
            // Same epilogue as `run_cancellable`: a panicking worker may
            // have left an explicit transaction's scratch in an unknown
            // state; fail it like any other query error.
            self.fail_txn();
        }
        // Planning admits no read, so the health check always applies.
        self.inner.check_healthy()?;
        result
    }

    /// Spawn the connection worker on first use, then plan there (the
    /// `dispatch_query` shape: parse/lower/plan recurse proportionally to
    /// input nesting, so they run on the 64 MiB worker).
    #[cfg(not(target_arch = "wasm32"))]
    fn dispatch_plan(&mut self, cypher: &str) -> Result<crate::PlanNode> {
        let slot = self.worker_slot()?;
        // Planning admits no read and reports no progress: the wait is
        // untimed (ADR-0312), so no heartbeat is armed.
        slot.run_with_heartbeat(|| self.plan_body(cypher), None)?
    }

    /// wasm32 has no threads. The host stack is whatever the environment gave us.
    #[cfg(target_arch = "wasm32")]
    fn dispatch_plan(&mut self, cypher: &str) -> Result<crate::PlanNode> {
        self.plan_body(cypher)
    }

    /// The plan body, run on the connection's worker thread (ADR-0259):
    /// the one admission sequence, then the tree build — the estimates
    /// (ADR-0218) read from the store under a read guard, like the
    /// admission's own planning above.
    fn plan_body(&mut self, cypher: &str) -> Result<crate::PlanNode> {
        let params = BTreeMap::new();
        let admission = self.admit_statement(cypher, &params)?;
        let _read = self.inner.read_guard()?;
        let stats = crate::stats::GraphPatternStats::new(&self.inner.graph, &params);
        Ok(crate::plan_node::plan_tree(&admission.logical, &stats).root)
    }

    pub fn query_with_params(
        &mut self,
        cypher: &str,
        params: BTreeMap<String, Value>,
    ) -> Result<QueryResult> {
        self.run_cancellable(cypher, params, &CancelToken::new(), None)
            .and_then(Executed::into_result)
    }

    /// Like [`Self::query_with_params`], but the query can be stopped by
    /// another thread through `token` (ADR-0111).
    ///
    /// The token is checked at row-production points and inside the
    /// executor's long loops. Once cancelled the query returns
    /// `Error::Cancelled`, whose message is `"query cancelled"`, and the
    /// connection remains usable: a later query on it runs normally. `query`
    /// and `query_with_params` keep their signatures and run with a token
    /// that is never cancelled.
    ///
    /// ```
    /// use std::collections::BTreeMap;
    /// use cypherlite::{CancelToken, Database};
    ///
    /// let db = Database::open(":memory:").unwrap();
    /// let mut conn = db.connect().unwrap();
    /// let token = CancelToken::new();
    /// // An uncancelled token lets the query run to completion.
    /// let result = conn
    ///     .query_with_params_cancellable("RETURN 1 AS one", BTreeMap::new(), token.clone())
    ///     .unwrap();
    /// assert_eq!(result.rows[0][0], cypherlite::Value::Int(1));
    /// // `cancel()` flips the flag shared by every clone.
    /// token.cancel();
    /// assert!(token.is_cancelled());
    /// ```
    pub fn query_with_params_cancellable(
        &mut self,
        cypher: &str,
        params: BTreeMap<String, Value>,
        token: CancelToken,
    ) -> Result<QueryResult> {
        self.run_cancellable(cypher, params, &token, None)
            .and_then(Executed::into_result)
    }

    /// Shared worker-thread wrapper for every query entry point: the
    /// cancellable and non-cancellable ones, and the staged one
    /// (`Connection::query_staged`, ADR-0319) that also takes the waiting
    /// caller's heartbeat (ADR-0312).
    pub(crate) fn run_cancellable(
        &mut self,
        cypher: &str,
        params: BTreeMap<String, Value>,
        token: &CancelToken,
        heartbeat: Option<Heartbeat<'_>>,
    ) -> Result<Executed> {
        // ADR-0259: parse, front-end lowering/validation, planning, and
        // execution all recurse proportionally to input nesting, so the
        // whole query runs on this connection's 64 MiB worker. The worker
        // is spawned on the first query and reused after that. The calling
        // thread's stack size is irrelevant; a worker panic or a spawn
        // failure surfaces as a typed internal error — never an abort,
        // never a fallback to the caller's stack.
        #[cfg(not(target_arch = "wasm32"))]
        let outcome = self.dispatch_query(cypher, params, token, heartbeat);
        // wasm32 has no worker to dispatch to: the statement runs on this
        // thread through the one `run_query` body, so there is nothing to
        // wait for and a heartbeat can never be due (ADR-0312).
        #[cfg(target_arch = "wasm32")]
        let outcome = {
            drop(heartbeat);
            self.run_query(cypher, params, token)
        };
        if matches!(outcome.result, Ok(Executed::Rows(_))) {
            log_logical_bytes(&self.inner);
        }
        // A retryable admission refusal never ran the statement, so an
        // explicit transaction stays usable (ADR-0333). Every other error,
        // including a worker panic that may have left the scratch unknown,
        // fails the transaction so a rollback is required. Ordinary query
        // errors already failed it inside `run_query`; this is then a no-op.
        let retryable_refusal = matches!(
            outcome.result,
            Err(crate::Error::ResourceLimit {
                retryable: true,
                ..
            })
        );
        if !matches!(outcome.result, Ok(Executed::Rows(_))) && !retryable_refusal {
            self.fail_txn();
        }
        // An already admitted read owns a pin to the last safe snapshot and
        // may finish even if a later physical publication poisons the handle.
        if !outcome.admitted {
            self.inner.check_healthy()?;
        }
        outcome.result
    }
}
