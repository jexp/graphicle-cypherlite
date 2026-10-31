//! Cooperative query cancellation (ADR-0111).
//!
//! A query running long enough to be cancelled from another thread returns a
//! typed error whose message contains `"query cancelled"`, and the
//! connection it ran on stays usable.

use std::collections::BTreeMap;
use std::time::Duration;

use cypherlite::{CancelToken, Database, Value};

/// Calibration for the in-loop cancellation tests below.
///
/// Each of those tests first runs its query *un-cancelled* to prove the
/// workload outlasts the 20 ms cancel latency, so the cancelled run must
/// have been stopped inside the loop under test and not at a clause
/// boundary. A fixed workload size cannot guarantee that across compilation
/// profiles: what takes hundreds of milliseconds in the dev profile
/// finishes in tens of milliseconds under an optimized engine (an
/// opt-level-raised `[profile.test]`, corpus, release — see the profile
/// matrix in docs/testing/runtime.md). So the tests escalate the workload
/// until the un-cancelled control reaches `CALIBRATION_TARGET`, and only
/// ever assert the looser `CALIBRATION_FLOOR`.
const CALIBRATION_FLOOR: Duration = Duration::from_millis(80);
const CALIBRATION_TARGET: Duration = Duration::from_millis(200);

/// Run `attempt` at each ladder size until one reaches
/// `CALIBRATION_TARGET`, returning that run's payload and duration.
///
/// If no run reaches the target (a fast profile, or the ladder's cap), the
/// last run's result is returned — the tests then fail their
/// `CALIBRATION_FLOOR` assertion, so the workload is never silently too
/// small to discriminate.
fn calibrate<R>(
    ladder: impl Iterator<Item = i64>,
    mut attempt: impl FnMut(i64) -> (R, Duration),
) -> (R, Duration) {
    let mut last = None;
    for size in ladder {
        let run = attempt(size);
        if run.1 >= CALIBRATION_TARGET {
            return run;
        }
        last = Some(run);
    }
    last.expect("calibration ladder must not be empty")
}

#[test]
fn cancelling_a_running_query_errors_and_connection_stays_usable() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let token = CancelToken::new();
    let canceller = token.clone();

    let outcome = std::thread::scope(|scope| {
        let running = scope.spawn(|| {
            conn.query_with_params_cancellable(
                "UNWIND range(1, 100000) AS a UNWIND range(1, 100000) AS b RETURN a + b",
                BTreeMap::new(),
                canceller,
            )
        });
        std::thread::sleep(Duration::from_millis(20));
        token.cancel();
        running.join().unwrap()
    });

    let error = outcome.unwrap_err();
    assert!(
        error.to_string().contains("query cancelled"),
        "expected a cancellation error, got {error}"
    );

    // The same connection still answers a normal query after the cancelled one.
    let result = conn.query("RETURN 1 AS one").unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn an_uncancelled_token_runs_the_query_to_completion() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn
        .query_with_params_cancellable(
            "UNWIND range(1, 5) AS x RETURN sum(x) AS total",
            BTreeMap::new(),
            CancelToken::new(),
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(15)]]);
}

/// Cancellation must be observed *inside* the `MATCH` scan and relationship
/// expansion, not only at clause boundaries (ADR-0111). This query's dominant
/// cost is the nested `MATCH (a:N),(b:N)` scan: the WHERE residual rejects
/// every pair, so essentially no rows are produced and all the work happens in
/// the candidate loops. Without a check in those loops the clause cannot be
/// stopped until it returns.
#[test]
fn cancelling_inside_a_match_scan_errors_promptly() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    // A few thousand nodes make the pair scan take far longer than the
    // cancellation budget below while keeping setup and memory modest.
    conn.query("UNWIND range(1, 4000) AS i CREATE (:N {v: i})")
        .unwrap();

    let token = CancelToken::new();
    let canceller = token.clone();
    let started = std::time::Instant::now();
    let outcome = std::thread::scope(|scope| {
        let running = scope.spawn(|| {
            conn.query_with_params_cancellable(
                "MATCH (a:N),(b:N) WHERE a.v + b.v < 0 RETURN count(a) AS n",
                BTreeMap::new(),
                canceller,
            )
        });
        std::thread::sleep(Duration::from_millis(20));
        token.cancel();
        running.join().unwrap()
    });
    let elapsed = started.elapsed();

    let error = outcome.unwrap_err();
    assert!(
        error.to_string().contains("query cancelled"),
        "expected a cancellation error from the MATCH scan, got {error}"
    );
    // The scan observes cancellation through its sampled per-candidate check,
    // so it returns within a few samples of the cancel, not after the whole
    // O(n^2) scan.
    assert!(
        elapsed < Duration::from_secs(2),
        "cancelled MATCH scan took {elapsed:?}, far longer than the cancel latency"
    );

    // The connection stays usable after the cancelled scan.
    let result = conn.query("RETURN 1 AS one").unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

/// Cancellation must be observed *inside* the one-hop pushdown aggregation
/// fold (ADR-0036/ADR-0111), not only at clause boundaries. This query's
/// dominant cost is the folded `count(b)`: the plan walks every `:N` start
/// node and its single `:R` relationship without materializing a row per edge,
/// so without a sampled check in that loop the clause cannot be stopped until
/// the whole fold returns.
#[test]
fn cancelling_inside_a_one_hop_pushdown_aggregation_errors_promptly() {
    // Each iteration creates its own pair of `:N` nodes joined by one `:R`, so
    // the graph has `n` relationships and roughly `2n` candidate start nodes.
    // The fold dominates query time while keeping setup and memory modest.
    // `n` escalates until the un-cancelled fold provably outlasts the cancel
    // latency at this compilation profile (see the calibration note above).
    let query = "MATCH (a)-[:R]->(b) RETURN count(b) AS n";
    let ladder =
        std::iter::successors(Some(50_000_i64), |&n| Some(n * 4)).take_while(|&n| n <= 3_200_000);
    let ((_db, mut conn), uncancelled_elapsed) = calibrate(ladder, |n| {
        let db = Database::open(":memory:").unwrap();
        let mut conn = db.connect().unwrap();
        conn.query(&format!(
            "UNWIND range(1, {n}) AS i CREATE (a:N {{v: i}})-[:R]->(b:N {{v: i}})"
        ))
        .unwrap();
        let started = std::time::Instant::now();
        let uncancelled = conn.query(query).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(uncancelled.rows, vec![vec![Value::Int(n)]]);
        ((db, conn), elapsed)
    });
    assert!(
        uncancelled_elapsed > CALIBRATION_FLOOR,
        "uncancelled one-hop fold finished in {uncancelled_elapsed:?}; too fast to \
         prove that the loop observes cancellation"
    );

    let token = CancelToken::new();
    let canceller = token.clone();
    let started = std::time::Instant::now();
    let outcome = std::thread::scope(|scope| {
        let running =
            scope.spawn(|| conn.query_with_params_cancellable(query, BTreeMap::new(), canceller));
        std::thread::sleep(Duration::from_millis(20));
        token.cancel();
        running.join().unwrap()
    });
    let elapsed = started.elapsed();

    let error = outcome.unwrap_err();
    assert!(
        error.to_string().contains("query cancelled"),
        "expected a cancellation error from the one-hop fold, got {error}"
    );
    // The fold observes cancellation through its sampled per-candidate/per-edge
    // checks, so it returns well before the whole O(nodes) walk completes.
    // Comparing against the un-cancelled fold is what makes this assertion
    // discriminating: a clause-boundary check alone only fires *after* the
    // fold returns, so without the in-loop checks the cancelled run would take
    // about as long as `uncancelled_elapsed`. Requiring less than half of it
    // leaves generous room for scheduling noise while still failing if the
    // in-loop checks regress.
    let bound = uncancelled_elapsed / 2;
    assert!(
        elapsed < bound,
        "cancelled one-hop fold took {elapsed:?}, not clearly faster than the \
         {uncancelled_elapsed:?} un-cancelled fold (bound {bound:?}); the fold \
         loop itself is not observing cancellation"
    );

    // The connection stays usable after the cancelled fold.
    let result = conn.query("RETURN 1 AS one").unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

/// Cancellation must be observed *inside* a recursive variable-length walk
/// (ADR-0111), not only at clause boundaries or in flat per-row loops. The
/// graph is a low-degree chain: every recursion frame of the walk iterates
/// exactly one neighbour, so a *per-frame* sampling window never accumulates
/// enough ticks to check the token — only a statement-wide window shared by
/// all frames of `walk` observes the cancellation while the walk is still
/// running. The comparative bound below is what makes this discriminating:
/// with per-frame sampling the cancelled run would take about as long as the
/// un-cancelled walk (the token is first re-checked at the clause boundary /
/// aggregate after the walk returns), failing the bound.
#[test]
fn cancelling_inside_a_var_length_walk_errors_promptly() {
    // A chain of `n` nodes, each linked to the next by one `:NEXT` edge, so
    // every frame of the var-length walk has out-degree one. `n` stays below
    // the 1024-tick sampling window and the walk uses a deep exact hop count,
    // so with a *per-frame* window no single frame — not the start-node scan,
    // not any walk frame, not the tiny aggregate — ever accumulates enough
    // ticks to check the token: the counterfactual per-frame policy observes
    // the cancellation only at the clause boundary after the whole walk.
    // The hop count (not `n`, which the sampling-window invariant pins below
    // 1024) escalates until the un-cancelled walk provably outlasts the
    // cancel latency at this compilation profile (see the calibration note
    // at the top of the file). Edges are built in chunks behind a property
    // index to keep setup sub-quadratic.
    let n: i64 = 900;
    // Hop counts stay below `n` so `n - hops` start nodes always remain.
    const HOPS_SIZES: [i64; 4] = [100, 200, 400, 800];
    let (((_db, mut conn), query), uncancelled_elapsed) =
        calibrate(HOPS_SIZES.iter().copied(), |hops| {
            let db = Database::open(":memory:").unwrap();
            let mut conn = db.connect().unwrap();
            conn.query("CREATE INDEX chain_v FOR (c:C) ON (c.v)")
                .unwrap();
            conn.query(&format!(
                "UNWIND range(0, {}) AS i CREATE (:C {{v: i}})",
                n - 1
            ))
            .unwrap();
            const CHUNK: i64 = 250;
            for start in (0..n - 1).step_by(CHUNK as usize) {
                let end = (start + CHUNK).min(n - 1);
                let matches = (start..=end)
                    .map(|v| format!("(n{v}:C {{v: {v}}})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let creates = (start..end)
                    .map(|v| format!("(n{v})-[:NEXT]->(n{})", v + 1))
                    .collect::<Vec<_>>()
                    .join(", ");
                conn.query(&format!("MATCH {matches} CREATE {creates}"))
                    .unwrap();
            }

            // Exactly `hops` hops from each chain position: `n - hops` start
            // nodes reach that depth, and the walk explores ~n*hops hops to
            // find them.
            let query = format!("MATCH (a:C)-[:NEXT*{hops}..{hops}]->(b:C) RETURN count(b) AS n");
            let expected = Value::Int(n - hops);
            let started = std::time::Instant::now();
            let uncancelled = conn.query(&query).unwrap();
            let elapsed = started.elapsed();
            assert_eq!(uncancelled.rows, vec![vec![expected]]);
            (((db, conn), query), elapsed)
        });
    assert!(
        uncancelled_elapsed > CALIBRATION_FLOOR,
        "un-cancelled var-length walk finished in {uncancelled_elapsed:?}; too fast \
         to prove that the walk observes cancellation"
    );

    let token = CancelToken::new();
    let canceller = token.clone();
    let started = std::time::Instant::now();
    let outcome = std::thread::scope(|scope| {
        let running =
            scope.spawn(|| conn.query_with_params_cancellable(&query, BTreeMap::new(), canceller));
        std::thread::sleep(Duration::from_millis(20));
        token.cancel();
        running.join().unwrap()
    });
    let elapsed = started.elapsed();

    let error = outcome.unwrap_err();
    assert!(
        error.to_string().contains("query cancelled"),
        "expected a cancellation error from the var-length walk, got {error}"
    );
    // The walk observes cancellation through the statement-wide sampling
    // window shared by every recursion frame, so it returns well before the
    // whole O(chain * hops) exploration completes. Requiring less than half
    // of the un-cancelled time leaves generous room for scheduling noise
    // while still failing if the frames stop sharing one window (each frame
    // ticks only once, so per-frame windows never fire and the token is
    // observed only after the walk returns).
    let bound = uncancelled_elapsed / 2;
    assert!(
        elapsed < bound,
        "cancelled var-length walk took {elapsed:?}, not clearly faster than the \
         {uncancelled_elapsed:?} un-cancelled walk (bound {bound:?}); the recursive \
         walk frames are not sharing one statement-wide cancellation window"
    );

    // The connection stays usable after the cancelled walk.
    let result = conn.query("RETURN 1 AS one").unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

/// Cancellation must be observed *inside* a shortest-path search (ADR-0111,
/// ADR-0189), not once per BFS layer. Both ends of the `MATCH` pattern are
/// bound, so the search is the pair search's bidirectional BFS. Each end
/// heads its own two-level fan of `FAN * FAN` leaves, unreachable from the
/// other, so the search walks both fans before it finds no path: three BFS
/// layers a side, each of them large. A search that ticked once per layer
/// would sample the token a handful of times, far below the 1024-tick
/// window, and observe the cancellation only after the search returns; the
/// comparative bound below catches that.
#[test]
fn cancelling_inside_a_shortest_path_search_errors_promptly() {
    // Each end heads its own two-level fan of `fan * fan` leaves. The fan
    // width escalates until the un-cancelled search provably outlasts the
    // cancel latency at this compilation profile (see the calibration note
    // at the top of the file); the three-layer fan shape the counterfactual
    // argument below relies on is unchanged by the width.
    let query = "MATCH (a:A), (b:B) MATCH p = shortestPath((a)-[*]-(b)) RETURN count(p) AS n";
    let ((_db, mut conn), uncancelled_elapsed) = calibrate([300, 600, 1200].into_iter(), |fan| {
        let db = Database::open(":memory:").unwrap();
        let mut conn = db.connect().unwrap();
        for label in ["A", "B"] {
            conn.query(&format!(
                "CREATE (root:{label}) WITH root UNWIND range(1, {fan}) AS i \
                     CREATE (root)-[:R]->(hub) WITH hub UNWIND range(1, {fan}) AS j \
                     CREATE (hub)-[:R]->()"
            ))
            .unwrap();
        }
        let started = std::time::Instant::now();
        let uncancelled = conn.query(query).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(uncancelled.rows, vec![vec![Value::Int(0)]]);
        ((db, conn), elapsed)
    });
    assert!(
        uncancelled_elapsed > CALIBRATION_FLOOR,
        "un-cancelled shortest-path search finished in {uncancelled_elapsed:?}; too \
         fast to prove that the search observes cancellation"
    );

    let token = CancelToken::new();
    let canceller = token.clone();
    let started = std::time::Instant::now();
    let outcome = std::thread::scope(|scope| {
        let running =
            scope.spawn(|| conn.query_with_params_cancellable(query, BTreeMap::new(), canceller));
        std::thread::sleep(Duration::from_millis(20));
        token.cancel();
        running.join().unwrap()
    });
    let elapsed = started.elapsed();

    let error = outcome.unwrap_err();
    assert!(
        error.to_string().contains("query cancelled"),
        "expected a cancellation error from the shortest-path search, got {error}"
    );
    // The search ticks once per relationship it follows, so it returns
    // within one sampling window of the cancel, well before both fans are
    // walked. Requiring less than half of the un-cancelled time leaves
    // generous room for scheduling noise while still failing if the
    // search samples the token only per layer.
    let bound = uncancelled_elapsed / 2;
    assert!(
        elapsed < bound,
        "cancelled shortest-path search took {elapsed:?}, not clearly faster than \
         the {uncancelled_elapsed:?} un-cancelled search (bound {bound:?}); the \
         search is not observing cancellation per relationship"
    );

    // The connection stays usable after the cancelled search.
    let result = conn.query("RETURN 1 AS one").unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

/// Cancellation must be observed *inside* constraint validation (ADR-0111,
/// ADR-0174): `CREATE CONSTRAINT` over a large label re-checks every node
/// of the label, so without a per-entity check in the recheck loop the
/// statement cannot be stopped until the whole validation returns.
///
/// The workload is sized so the un-cancelled recheck takes seconds even on
/// a fast machine, while the cancelled run is the cancel latency plus one
/// 1024-entity sampling window — orders of magnitude less on any runner.
/// The cancelled run is therefore asserted against a generous absolute cap,
/// not a ratio of the un-cancelled time: on a slow, loaded CI runner the
/// cancelling thread's short sleep can stretch to a large fraction of a
/// small workload's runtime (observed on windows-latest: a 20 ms sleep
/// landing ~700 ms into a 1.4 s recheck), which breaks any ratio bound no
/// matter how healthy the engine is. The test stays discriminating because
/// a recheck loop that stops calling `cancel_tick` never observes the token
/// at all — nothing checks it after validation — so the cancelled
/// statement would run the full recheck and *succeed*, failing the error
/// assertion below (and blowing past the cap on any runner slow enough to
/// reach the error at all).
#[test]
fn cancelling_a_constraint_recheck_errors_promptly() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    // One million nodes: the un-cancelled recheck below takes seconds even
    // on a fast machine, so the 20 ms cancel latency lands deep inside the
    // recheck loop no matter how badly the runner starves the cancelling
    // thread. Nodes are created in chunks to keep each setup statement's
    // row buffer the size the other tests already use.
    let n: i64 = 1_000_000;
    const CHUNK: i64 = 100_000;
    for start in (1..=n).step_by(CHUNK as usize) {
        let end = (start + CHUNK - 1).min(n);
        conn.query(&format!(
            "UNWIND range({start}, {end}) AS i CREATE (:P {{id: i}})"
        ))
        .unwrap();
    }
    let ddl = "CREATE CONSTRAINT p_id FOR (n:P) REQUIRE n.id IS UNIQUE";

    // The un-cancelled control: the same workload completes and validates,
    // and takes long enough that the cancel below must land inside the
    // recheck loop rather than before the statement or after it.
    let started = std::time::Instant::now();
    conn.query(ddl).unwrap();
    let uncancelled_elapsed = started.elapsed();
    assert!(
        uncancelled_elapsed > Duration::from_millis(80),
        "un-cancelled constraint recheck finished in {uncancelled_elapsed:?}; too fast \
         to prove that the recheck loop observes cancellation"
    );
    conn.query("DROP CONSTRAINT p_id").unwrap();

    let token = CancelToken::new();
    let canceller = token.clone();
    let started = std::time::Instant::now();
    let outcome = std::thread::scope(|scope| {
        let running =
            scope.spawn(|| conn.query_with_params_cancellable(ddl, BTreeMap::new(), canceller));
        std::thread::sleep(Duration::from_millis(20));
        token.cancel();
        running.join().unwrap()
    });
    let elapsed = started.elapsed();

    let error = outcome.unwrap_err();
    assert!(
        error.to_string().contains("query cancelled"),
        "expected a cancellation error from the constraint recheck, got {error}"
    );
    // The recheck loop samples the token once per 1024 entities, so after
    // the cancel lands the loop exits within one sampling window and the
    // whole cancelled run is the cancel latency plus that window — well
    // under a second even on the slowest CI runner observed (757 ms there,
    // vs a 1.4 s un-cancelled recheck of a workload one tenth this size).
    // Ten seconds leaves that observation 13x of headroom while still
    // failing if the loop observes cancellation only at the clause
    // boundary: the un-cancelled recheck of this workload takes longer
    // than the cap on such hardware.
    assert!(
        elapsed < Duration::from_secs(10),
        "cancelled constraint recheck took {elapsed:?}; the recheck loop itself is \
         not observing cancellation (un-cancelled run: {uncancelled_elapsed:?})"
    );

    // The cancelled DDL committed nothing, and the connection stays usable.
    let result = conn
        .query("SHOW CONSTRAINTS YIELD name RETURN name")
        .unwrap();
    assert!(
        result.rows.is_empty(),
        "the cancelled DDL left no constraint"
    );
    let result = conn.query("RETURN 1 AS one").unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}
