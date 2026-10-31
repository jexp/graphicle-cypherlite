//! ADR-0259: every query runs on the connection's 64 MiB worker, spawned on
//! the first query and reused after that, so the calling thread's stack size
//! never matters. At-cap input lowers/plans/executes successfully on the
//! worker and beyond-cap input returns typed errors — never an abort — even
//! when the caller runs on a 2 MiB stack (tokio's worker default), and a
//! worker panic is caught and returned as a typed internal error. The same
//! worker then serves the next statement.

use crate::{Database, Error};
use cypherlite_test_support::run_with_stack;

/// Spawn a caller thread with a 2 MiB stack (tokio's documented worker
/// default, and the size on which release builds used to SIGABRT below the
/// ADR-0257 depth cap) and run `f` on it.
fn on_small_stack_caller<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    run_with_stack(2 * 1024 * 1024, f)
}

/// Beyond-cap inputs return typed errors through the public query path even
/// when the caller's own stack is 2 MiB: the input-proportional recursion
/// runs on the engine's 64 MiB worker, so the ADR-0257 depth guard fires
/// long before any stack is exhausted. If this test aborts the process
/// instead of failing, the worker-thread guarantee is broken.
///
/// Covers all eight guarded grammar classes of the parser suite
/// (`test_parse_*_at_and_beyond_depth_limit` in
/// `cypherlite/cypherlite-parser/tests/limits.rs`) plus property chains (the
/// front-end suite's guarded class): nested expression parens, `NOT`
/// chains, unary `-` chains, label `!` chains, nested label parens, nested
/// subpath parens, nested `FOREACH`, nested value types (`LIST<…>` via both
/// the `::TYPE` cast and the `IS ::` predicate spellings), and deep
/// property chains.
#[test]
fn beyond_cap_queries_error_typed_on_two_mib_caller_stack() {
    let cases: Vec<(String, String)> = on_small_stack_caller(|| {
        // Nested parentheses: used to SIGABRT parse+lower at ~310–320 on a
        // 2 MiB release stack, below the 500 cap.
        let parens = format!("RETURN {}1{}", "(".repeat(5000), ")".repeat(5000));
        // Nested NOT: used to abort front-end lowering at ~1000.
        let nots = format!("RETURN {}true", "NOT ".repeat(2000));
        // Nested unary minus.
        let minuses = format!("RETURN {}1", "-".repeat(2000));
        // Nested label `!` chain.
        let label_bangs = format!("MATCH (n:{}A) RETURN n", "!".repeat(600));
        // Nested label parens.
        let label_parens = format!("MATCH (n:{}A{}) RETURN n", "(".repeat(600), ")".repeat(600));
        // Nested subpath parens.
        let subpath_parens = format!("MATCH {}a{} RETURN a", "(".repeat(600), ")".repeat(600));
        // Nested FOREACH: used to SIGABRT at ~480 on a 2 MiB release stack.
        let mut foreach = String::new();
        for i in 0..600 {
            foreach.push_str(&format!("FOREACH (x{i} IN [1] | "));
        }
        foreach.push_str("CREATE (n)");
        foreach.push_str(&")".repeat(600));
        // Nested value types, `::TYPE` cast spelling.
        let value_type_cast = format!(
            "RETURN 1::{}INTEGER{} AS x",
            "LIST<".repeat(600),
            ">".repeat(600)
        );
        // Nested value types, `IS ::` type-predicate spelling.
        let value_type_predicate = format!(
            "RETURN 1 IS :: {}INTEGER{}",
            "LIST<".repeat(600),
            ">".repeat(600)
        );
        // Deep property chain.
        let chain = format!("RETURN x{}", ".a".repeat(16 * 1024));

        let db = Database::open(":memory:").unwrap();
        let mut conn = db.connect().unwrap();
        [
            parens,
            nots,
            minuses,
            label_bangs,
            label_parens,
            subpath_parens,
            foreach,
            value_type_cast,
            value_type_predicate,
            chain,
        ]
        .into_iter()
        .map(|q| {
            let outcome = match conn.query(&q) {
                Ok(_) => "unexpected ok".to_string(),
                Err(e) => format!("{e:?}"),
            };
            (q.chars().take(40).collect(), outcome)
        })
        .collect()
    });

    for (prefix, outcome) in &cases {
        assert!(
            outcome.starts_with("Syntax(") || outcome.starts_with("Semantic("),
            "beyond-cap query `{prefix}…` must produce a typed error, got: {outcome}"
        );
        assert!(
            outcome.contains("maximum nesting depth"),
            "beyond-cap query `{prefix}…` must name the ADR-0257 limit, got: {outcome}"
        );
    }
}

/// At-cap inputs parse, lower, plan, and execute successfully through the
/// public query path even when the caller's own stack is 2 MiB: the
/// recursion runs on the engine's 64 MiB worker, where the ADR-0257 cap of
/// 500 was probe-verified to hold with 3.3×–48× margin. This is the
/// end-to-end proof that 500 is a sound cap on the worker — the beyond-cap
/// tests alone only exercise the parse guard. Depth arithmetic mirrors the
/// parser suite's at-limit cases; the property chain uses 450, the
/// front-end suite's under-cap value
/// (`property_chain_just_under_the_limit_lowers` in
/// `cypherlite/cypherlite-front/src/lower.rs`).
#[test]
fn at_cap_queries_succeed_on_two_mib_caller_stack() {
    on_small_stack_caller(|| {
        let db = Database::open(":memory:").unwrap();
        let mut conn = db.connect().unwrap();

        // 499-deep NOT chain: the enclosing RETURN expression occupies
        // depth 1, so 499 NOTs sit exactly at the limit.
        let nots = format!("RETURN {}true", "NOT ".repeat(499));
        // 499-deep parenthesised expression, same arithmetic.
        let parens = format!("RETURN {}1{}", "(".repeat(499), ")".repeat(499));
        // Just-under-limit FOREACH nest: 499 nested FOREACH clauses plus the
        // innermost CREATE body sit exactly at the limit.
        let foreach = format!(
            "{}CREATE (:A){}",
            "FOREACH (x IN [1] | ".repeat(499),
            ")".repeat(499)
        );
        // 450-hop property chain over a null map: evaluates to null.
        let chain = format!("WITH {{a: null}} AS n RETURN n{}", ".a".repeat(450));

        for q in [&nots, &parens, &foreach, &chain] {
            let prefix: String = q.chars().take(40).collect();
            conn.query(q)
                .unwrap_or_else(|e| panic!("at-cap query `{prefix}…` must succeed, got: {e:?}"));
        }

        // Spot-check the results, not just the absence of an error.
        let result = conn.query(&parens).unwrap();
        assert_eq!(result.rows, vec![vec![crate::Value::Int(1)]]);
        let result = conn.query(&nots).unwrap();
        assert_eq!(result.rows, vec![vec![crate::Value::Bool(false)]]);
    });
}

/// The engine stays usable after a beyond-cap query on a small-stack caller.
#[test]
fn engine_usable_after_beyond_cap_query_on_two_mib_caller() {
    on_small_stack_caller(|| {
        let db = Database::open(":memory:").unwrap();
        let mut conn = db.connect().unwrap();
        let deep = format!("RETURN {}1{}", "(".repeat(5000), ")".repeat(5000));
        assert!(conn.query(&deep).is_err());
        let result = conn.query("RETURN 1 AS n").unwrap();
        assert_eq!(result.rows, vec![vec![crate::Value::Int(1)]]);
    });
}

/// Several statements on one connection spawn one worker.
#[test]
fn one_connection_reuses_one_worker() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    assert_eq!(conn.query_worker_spawns(), 0);
    for _ in 0..4 {
        let result = conn.query("RETURN 1 AS n").unwrap();
        assert_eq!(result.rows, vec![vec![crate::Value::Int(1)]]);
    }
    assert_eq!(conn.query_worker_spawns(), 1);
}

/// A panic inside the connection's worker is a typed internal error, and
/// the same connection can run the next statement on that same worker.
#[test]
fn connection_survives_worker_panic() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn.drive_query_worker(|| panic!("injected worker panic"));
    match result {
        Err(Error::Internal(msg)) => {
            assert!(
                msg.contains("query worker thread panicked"),
                "unexpected message: {msg}"
            );
            assert!(msg.contains("injected worker panic"), "payload lost: {msg}");
        }
        other => panic!("expected typed internal error, got: {other:?}"),
    }
    assert_eq!(conn.query_worker_spawns(), 1);
    let result = conn.query("RETURN 1 AS n").unwrap();
    assert_eq!(result.rows, vec![vec![crate::Value::Int(1)]]);
    assert_eq!(conn.query_worker_spawns(), 1);
}

/// A panic inside the worker is caught and mapped to a typed internal
/// error; it never unwinds into or aborts the host. The same worker then
/// serves the next call.
#[test]
fn worker_panic_maps_to_typed_internal_error() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn.drive_query_worker(|| panic!("injected worker panic"));
    match result {
        Err(Error::Internal(msg)) => {
            assert!(
                msg.contains("query worker thread panicked"),
                "unexpected message: {msg}"
            );
            assert!(msg.contains("injected worker panic"), "payload lost: {msg}");
        }
        other => panic!("expected typed internal error, got: {other:?}"),
    }
    // The caller is still alive and the same worker still returns a value.
    assert_eq!(conn.drive_query_worker(|| 42u32).unwrap(), 42);
}

/// Worker results and ordinary query errors pass through untouched.
#[test]
fn worker_passes_through_results_and_errors() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let err = conn
        .drive_query_worker(|| Err::<u32, _>(Error::semantic("plain failure")))
        .unwrap();
    assert!(matches!(err, Err(Error::Semantic(_))));
    assert_eq!(conn.drive_query_worker(|| 7u32).unwrap(), 7);
}
