//! The staged statement entry point (ADR-0319): a surface that reports
//! planning failures and execution failures at different points (the Bolt
//! server's RUN and PULL) learns the phase from the engine, and a surface
//! that must keep its client connection alive during a long statement gets
//! a callback while it waits (ADR-0312, whose `Heartbeat` lives in
//! `query_worker.rs` beside the wait loop that arms it).

use std::collections::BTreeMap;

use super::Executed;
use crate::{CancelToken, Connection, Heartbeat, Result, Value};

impl Connection {
    /// Run a statement and report which phase a failure came from
    /// (ADR-0319). `Err` is a failure before execution: parse, semantic
    /// validation, planning and the compile-time gates. `Ok(Executed::Rows)`
    /// is the finished result; `Ok(Executed::Failed)` is a failure while
    /// executing the planned statement, with its planned columns.
    ///
    /// `token` cancels the statement from another thread (ADR-0111), as in
    /// [`Self::query_with_params_cancellable`]. `heartbeat`, when given,
    /// ticks on this thread while the statement runs on the connection's
    /// query worker (ADR-0312). In an explicit transaction an execution
    /// failure fails the transaction, as every query error does (ADR-0028).
    ///
    /// ```
    /// use std::collections::BTreeMap;
    /// use std::time::Duration;
    /// use cypherlite::{CancelToken, Database, Executed, Heartbeat, Value};
    ///
    /// let db = Database::open(":memory:").unwrap();
    /// let mut conn = db.connect().unwrap();
    /// // A runtime error is staged with the planned columns.
    /// let staged = conn
    ///     .query_staged("UNWIND [1, 0] AS x RETURN 10 / x AS v", BTreeMap::new(), CancelToken::new(), None)
    ///     .unwrap();
    /// let Executed::Failed(failure) = staged else { panic!("10 / 0 fails while executing") };
    /// assert_eq!(failure.header.columns, ["v"]);
    /// assert_eq!(failure.header.column_types, ["ANY"]);
    /// // A syntax error fails before execution.
    /// assert!(conn.query_staged("RETURN", BTreeMap::new(), CancelToken::new(), None).is_err());
    /// // A heartbeat that is never due does not tick.
    /// let mut ticks = 0;
    /// let mut tick = || ticks += 1;
    /// let heartbeat = Heartbeat::every(Duration::from_secs(60), &mut tick).unwrap();
    /// let staged = conn
    ///     .query_staged("RETURN 1 AS n", BTreeMap::new(), CancelToken::new(), Some(heartbeat))
    ///     .unwrap();
    /// assert_eq!(staged.into_result().unwrap().rows, vec![vec![Value::Int(1)]]);
    /// assert_eq!(ticks, 0);
    /// ```
    pub fn query_staged(
        &mut self,
        cypher: &str,
        params: BTreeMap<String, Value>,
        token: CancelToken,
        heartbeat: Option<Heartbeat<'_>>,
    ) -> Result<Executed> {
        self.run_cancellable(cypher, params, &token, heartbeat)
    }

    /// [`Self::commit`], ticking `heartbeat` on this thread while the
    /// commit runs (ADR-0312). The commit runs on the connection's query
    /// worker when the worker is running; a connection that never ran a
    /// query has no worker and its transaction holds no writes, so it
    /// commits inline — nothing waits, and the heartbeat is never due.
    ///
    /// ```
    /// use std::time::Duration;
    /// use cypherlite::{Database, Heartbeat};
    ///
    /// let db = Database::open(":memory:").unwrap();
    /// let mut conn = db.connect().unwrap();
    /// conn.begin().unwrap();
    /// conn.query("CREATE (:N)").unwrap();
    /// let mut tick = || {};
    /// let heartbeat = Heartbeat::every(Duration::from_secs(60), &mut tick).unwrap();
    /// conn.commit_with_heartbeat(heartbeat).unwrap();
    /// assert!(!conn.in_transaction());
    /// ```
    pub fn commit_with_heartbeat(&mut self, heartbeat: Heartbeat<'_>) -> Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(slot) = self.spawned_worker_slot() {
            return slot.run_with_heartbeat(|| self.commit(), Some(heartbeat))?;
        }
        // wasm32 has no worker, and a connection that never ran a query has
        // none either: the commit runs inline on this thread, so nothing
        // waits and the heartbeat is never due.
        #[cfg(target_arch = "wasm32")]
        drop(heartbeat);
        self.commit()
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use crate::{CancelToken, Database, Executed, Heartbeat};

    /// ADR-0312: a statement that outlives the interval ticks on the
    /// waiting thread, once per elapsed interval.
    #[test]
    fn a_long_statement_ticks_the_heartbeat() {
        let db = Database::open(":memory:").unwrap();
        let mut conn = db.connect().unwrap();
        let mut ticks = 0u32;
        let mut tick = || ticks += 1;
        let heartbeat = Heartbeat::every(Duration::from_millis(5), &mut tick).unwrap();
        let stage = "UNWIND range(1, 100000) AS x WITH count(x) AS c ";
        let long_query = format!("{}RETURN c", stage.repeat(30));
        let staged = conn
            .query_staged(
                &long_query,
                BTreeMap::new(),
                CancelToken::new(),
                Some(heartbeat),
            )
            .unwrap();
        assert!(matches!(staged, Executed::Rows(_)));
        assert!(ticks > 0, "a statement longer than the interval must tick");
    }

    /// ADR-0319: an execution-phase failure inside an explicit transaction
    /// fails the transaction, like any query error (ADR-0028).
    #[test]
    fn a_staged_execution_failure_fails_the_transaction() {
        let db = Database::open(":memory:").unwrap();
        let mut conn = db.connect().unwrap();
        conn.begin().unwrap();
        let staged = conn
            .query_staged(
                "RETURN 1 / 0 AS v",
                BTreeMap::new(),
                CancelToken::new(),
                None,
            )
            .unwrap();
        assert!(matches!(staged, Executed::Failed(_)));
        let err = conn.commit().unwrap_err();
        assert!(err.to_string().contains("rollback required"), "{err}");
    }
}
