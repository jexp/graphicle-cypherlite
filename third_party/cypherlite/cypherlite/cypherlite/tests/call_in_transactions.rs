//! LABS-264 / ADR-0210: `CALL { … } IN TRANSACTIONS` — the legacy
//! importing-WITH form, concluding-subquery results, OF N ROWS batch
//! boundaries, per-batch commits (a failed batch leaves earlier batches
//! committed), WAL durability of batches, and the LABS-263 cross-product
//! CREATE repro completing with bounded memory under batching.

mod common;

use std::collections::BTreeSet;

use cypherlite::Value;

use common::{connection, err_text, scalar};

/// The standard importing-WITH form of a batched write: previously
/// rejected with `VariableAlreadyBound` (LABS-264 probe).
#[test]
fn with_import_form_batches_a_bulk_delete() {
    let mut c = connection();
    c.query("UNWIND range(1, 25) AS i CREATE (:N {i: i})")
        .unwrap();
    let result = c
        .query("MATCH (n) CALL { WITH n DETACH DELETE n } IN TRANSACTIONS OF 10 ROWS")
        .unwrap();
    assert!(
        result.rows.is_empty(),
        "a write-only concluding subquery returns an empty result"
    );
    assert_eq!(scalar(&mut c, "MATCH (n) RETURN count(n)"), Value::Int(0));
}

/// The importing WITH must only import: re-exporting the imported name
/// from the body's terminal RETURN is still `VariableAlreadyBound`.
#[test]
fn with_import_reexport_is_still_rejected() {
    let mut c = connection();
    c.query("CREATE (:N)").unwrap();
    let error = err_text(c.query("MATCH (n) CALL { WITH n RETURN n } IN TRANSACTIONS OF 10 ROWS"));
    assert!(
        error.contains("already declared in outer scope"),
        "unexpected error: {error}"
    );
}

/// A concluding `CALL { … } IN TRANSACTIONS` whose body has a terminal
/// RETURN projects the body's exported rows (LABS-264 probe returned an
/// empty result).
#[test]
fn bare_concluding_form_returns_rows() {
    let mut c = connection();
    c.query("UNWIND [1, 2, 3] AS x CREATE (:N {i: x})").unwrap();
    let result = c
        .query("CALL { MATCH (n) RETURN n } IN TRANSACTIONS")
        .unwrap();
    assert_eq!(result.header.columns, vec!["n".to_string()]);
    assert_eq!(result.rows.len(), 3);
}

/// Concluding batched subquery with an importing WITH and a computed
/// export comes back; the batch boundaries show in the REPORT STATUS
/// transaction ids (REPORT STATUS cannot terminate a query, so the
/// status flows through a trailing RETURN).
#[test]
fn concluding_with_import_returns_export_and_status() {
    let mut c = connection();
    let result = c
        .query(
            "UNWIND [1, 2, 3, 4, 5] AS x \
             CALL { WITH x RETURN x + 1 AS y } IN TRANSACTIONS OF 2 ROWS \
             ON ERROR CONTINUE REPORT STATUS AS s \
             RETURN y, s",
        )
        .unwrap();
    assert_eq!(
        result.header.columns,
        vec!["y".to_string(), "s".to_string()]
    );
    assert_eq!(
        result.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec![
            Value::Int(2),
            Value::Int(3),
            Value::Int(4),
            Value::Int(5),
            Value::Int(6)
        ]
    );
    // OF 2 ROWS over 5 input rows is three batches: three distinct
    // committed transaction ids, in pairs of 2, 2, 1.
    let tx_ids: Vec<String> = result
        .rows
        .iter()
        .map(|r| match &r[1] {
            Value::Map(status) => match status.get("transactionId") {
                Some(Value::String(id)) => id.to_string(),
                other => panic!("transactionId: {other:?}"),
            },
            other => panic!("status: {other:?}"),
        })
        .collect();
    let distinct: BTreeSet<&String> = tx_ids.iter().collect();
    assert_eq!(distinct.len(), 3, "one committed transaction per batch");
    assert_eq!(tx_ids[0], tx_ids[1]);
    assert_eq!(tx_ids[2], tx_ids[3]);
    assert_ne!(tx_ids[1], tx_ids[2]);
    assert_ne!(tx_ids[3], tx_ids[4]);
}

/// A concluding `CALL { … } IN TRANSACTIONS` with a terminal RETURN in
/// the body projects the exported rows (no trailing RETURN needed).
#[test]
fn concluding_returning_subquery_projects_exported_columns() {
    let mut c = connection();
    let result = c
        .query(
            "UNWIND [1, 2, 3] AS x \
             CALL { WITH x RETURN x + 1 AS y } IN TRANSACTIONS OF 2 ROWS",
        )
        .unwrap();
    assert_eq!(result.header.columns, vec!["y".to_string()]);
    assert_eq!(result.rows.len(), 3);
}

/// ON ERROR FAIL leaves earlier batches committed: batch 1's create
/// survives batch 2's constraint violation (Neo4j inner-transaction
/// semantics, ADR-0210).
#[test]
fn failed_batch_leaves_earlier_batches_committed() {
    let mut c = connection();
    c.query("CREATE CONSTRAINT uniq_m FOR (n:M) REQUIRE n.v IS UNIQUE")
        .unwrap();
    let error = err_text(
        c.query("UNWIND [1, 2] AS i CALL { WITH i CREATE (:M {v: 7}) } IN TRANSACTIONS OF 1 ROWS"),
    );
    assert!(error.contains("uniqueness"), "unexpected: {error}");
    assert_eq!(
        scalar(&mut c, "MATCH (n:M {v: 7}) RETURN count(n)"),
        Value::Int(1),
        "batch 1 committed before batch 2 failed"
    );
}

/// Inside an explicit transaction the batches do NOT commit independently
/// (ADR-0210 Consequences): they accumulate in the statement's overlay and
/// become visible only when the enclosing transaction commits — a second
/// connection sees nothing before then.
#[test]
fn explicit_transaction_batches_are_invisible_until_commit() {
    let db = cypherlite::Database::open(":memory:").unwrap();
    let mut writer = db.connect().unwrap();
    let mut reader = db.connect().unwrap();
    writer.begin().unwrap();
    writer
        .query(
            "UNWIND range(1, 10) AS i CALL { WITH i CREATE (:T {v: i}) } \
             IN TRANSACTIONS OF 2 ROWS",
        )
        .unwrap();
    assert_eq!(
        scalar(&mut reader, "MATCH (n:T) RETURN count(n)"),
        Value::Int(0),
        "batches accumulated in the transaction overlay, not committed"
    );
    writer.commit().unwrap();
    assert_eq!(
        scalar(&mut reader, "MATCH (n:T) RETURN count(n)"),
        Value::Int(10)
    );
}

/// The explicit-transaction corollary of `ON ERROR FAIL`: with no
/// per-batch commits, a failing batch rolls the whole statement back —
/// nothing from earlier "batches" survives either.
#[test]
fn explicit_transaction_failed_batch_rolls_back_everything() {
    let db = cypherlite::Database::open(":memory:").unwrap();
    let mut writer = db.connect().unwrap();
    let mut reader = db.connect().unwrap();
    writer
        .query("CREATE CONSTRAINT uniq_t FOR (n:T) REQUIRE n.v IS UNIQUE")
        .unwrap();
    writer.begin().unwrap();
    let error =
        err_text(writer.query(
            "UNWIND [1, 2] AS i CALL { WITH i CREATE (:T {v: 7}) } IN TRANSACTIONS OF 1 ROWS",
        ));
    assert!(error.contains("uniqueness"), "unexpected: {error}");
    // The failed statement leaves the explicit transaction open; roll it
    // back unconditionally — a conditional cleanup would hedge the very
    // contract under test.
    assert!(writer.in_transaction());
    writer.rollback().unwrap();
    assert_eq!(
        scalar(&mut reader, "MATCH (n:T) RETURN count(n)"),
        Value::Int(0),
        "no batch committed inside the failed explicit transaction"
    );
}

/// The importing-WITH scope fix (LABS-264) is not gated on
/// IN TRANSACTIONS: a plain `CALL { WITH n DELETE n }` validates and
/// executes.
#[test]
fn plain_call_with_import_delete_runs() {
    let mut c = connection();
    c.query("UNWIND range(1, 5) AS i CREATE (:N {i: i})")
        .unwrap();
    let result = c.query("MATCH (n) CALL { WITH n DELETE n }").unwrap();
    assert!(result.rows.is_empty());
    assert_eq!(scalar(&mut c, "MATCH (n) RETURN count(n)"), Value::Int(0));
}

/// Per-batch commits are durable: batches hit the WAL as they commit, so
/// a reopened store has every committed batch.
#[test]
fn batches_are_durable_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("batchdb");
    let uri = path.to_str().unwrap().to_string();
    {
        let db = cypherlite::Database::open(&uri).unwrap();
        let mut c = db.connect().unwrap();
        c.query(
            "UNWIND range(1, 25) AS i CALL { WITH i CREATE (:D {v: i}) } \
             IN TRANSACTIONS OF 5 ROWS",
        )
        .unwrap();
    }
    let db = cypherlite::Database::open(&uri).unwrap();
    let mut c = db.connect().unwrap();
    assert_eq!(
        scalar(&mut c, "MATCH (n:D) RETURN count(n)"),
        Value::Int(25)
    );
}

/// LABS-263 repro shape under batching: the 417-node cross-product
/// CREATE (173,889 relationships) completes through
/// `CALL { } IN TRANSACTIONS` with memory bounded by the batch, not the
/// total write volume.
#[test]
fn cross_product_create_completes_batched() {
    let mut c = connection();
    c.query("UNWIND range(1, 417) AS i CREATE (:N {i: i})")
        .unwrap();
    let sampler = PeakRss::start();
    let result = c
        .query(
            "MATCH (n) CALL { WITH n MATCH (m) CREATE (n)-[:TAGGED]->(m) } \
             IN TRANSACTIONS OF 25 ROWS",
        )
        .unwrap();
    let peak = sampler.stop();
    assert!(result.rows.is_empty());
    assert_eq!(
        result.stats.relationships_created,
        417 * 417,
        "statement stats accumulate across per-batch commits"
    );
    assert_eq!(
        scalar(&mut c, "MATCH ()-[r:TAGGED]->() RETURN count(r)"),
        Value::Int(417 * 417)
    );
    if let Some((before, peak)) = peak {
        let growth_mb = peak.saturating_sub(before) / (1024 * 1024);
        eprintln!("cross-product batched peak RSS growth: {growth_mb} MiB");
        assert!(
            growth_mb < 2048,
            "batched cross-product peak RSS growth {growth_mb} MiB exceeds the bounded-memory budget"
        );
    }
}

/// Peak-RSS sampler: polls `ps` on a helper thread until stopped. Returns
/// (baseline, peak) in bytes, or None where `ps` is unavailable. Test
/// budgets stay generous; the sampler exists to catch runaway growth,
/// not to micro-benchmark.
struct PeakRss {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<u64>,
    baseline: u64,
}

impl PeakRss {
    fn start() -> Self {
        let baseline = Self::rss().unwrap_or(0);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut peak = 0u64;
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(rss) = Self::rss() {
                    peak = peak.max(rss);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            peak
        });
        Self {
            stop,
            handle,
            baseline,
        }
    }

    fn stop(self) -> Option<(u64, u64)> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let peak = self.handle.join().ok()?;
        (self.baseline > 0 && peak > 0).then_some((self.baseline, peak))
    }

    fn rss() -> Option<u64> {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        String::from_utf8(output.stdout)
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
            .map(|kb| kb * 1024)
    }
}

/// LABS-272: the `CALL (vars)` importing form driven by UNWIND — the
/// write-heavy bulk-create shape whose unbatched spelling hits the
/// 64 MiB WAL payload limit at ~1M rows — commits per batch: OF 2 ROWS
/// over 6 rows yields three distinct committed transaction ids, and the
/// writes all land.
#[test]
fn unwind_call_importing_form_bulk_create_commits_per_batch() {
    let mut c = connection();
    let result = c
        .query(
            "UNWIND range(1, 6) AS x \
             CALL (x) { CREATE (p:Person {id: x})-[:KNOWS]->(p) } IN TRANSACTIONS OF 2 ROWS \
             ON ERROR CONTINUE REPORT STATUS AS s \
             RETURN s",
        )
        .unwrap();
    assert_eq!(result.header.columns, vec!["s".to_string()]);
    assert_eq!(result.rows.len(), 6);
    let tx_ids: BTreeSet<String> = result
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Map(status) => match (status.get("transactionId"), status.get("committed")) {
                (Some(Value::String(id)), Some(Value::Bool(true))) => id.to_string(),
                other => panic!("status: {other:?}"),
            },
            other => panic!("status: {other:?}"),
        })
        .collect();
    assert_eq!(tx_ids.len(), 3, "one committed transaction per batch");
    let nodes = scalar(&mut c, "MATCH (p:Person) RETURN count(p)");
    let rels = scalar(&mut c, "MATCH ()-[r:KNOWS]->() RETURN count(r)");
    assert_eq!(nodes, Value::Int(6));
    assert_eq!(rels, Value::Int(6));
}
