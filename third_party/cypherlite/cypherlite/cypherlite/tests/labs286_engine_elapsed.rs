//! LABS-286 / ADR-0221: the engine stamps every `QueryResult` with
//! `elapsed` — parse, plan, gates and dispatch, measured once around
//! `run_query` — and the wire envelope carries it as `elapsed_ms`.

mod common;

use std::time::Duration;

use cypherlite::query_result_to_wire;

/// A plain statement carries a measured elapsed — strictly positive (an
/// unstamped placeholder would be `Duration::ZERO`) and bounded — EXPLAIN
/// and PROFILE alike (they report the wrapped statement through the same
/// construction stamp).
#[test]
fn query_result_carries_engine_elapsed() {
    let mut conn = common::connection();
    for stmt in [
        "CREATE (:P {x: 1})",
        "MATCH (n:P) RETURN n.x AS x",
        "EXPLAIN MATCH (n:P) RETURN n",
        "PROFILE MATCH (n:P) RETURN n.x AS x",
    ] {
        let r = common::run(&mut conn, stmt);
        assert!(
            r.header.elapsed > Duration::ZERO && r.header.elapsed < Duration::from_secs(60),
            "elapsed must be a measured plan+run duration, got {:?} for {stmt}",
            r.header.elapsed
        );
    }
}

/// The wire envelope exposes the stamped value as `elapsed_ms`, between
/// `stats` and `error`.
#[test]
fn wire_envelope_carries_elapsed_ms() {
    let mut conn = common::connection();
    let r = common::run(&mut conn, "RETURN 1 AS n");
    let env = query_result_to_wire(&r);
    let elapsed_ms = env["elapsed_ms"]
        .as_f64()
        .expect("envelope carries elapsed_ms");
    // serde_json round-trips an f64 bit-exactly, so the envelope value is
    // the engine stamp to the bit — an exact re-sourcing check, not a
    // tolerance.
    assert_eq!(
        elapsed_ms.to_bits(),
        r.elapsed_ms().to_bits(),
        "envelope elapsed_ms re-sources QueryResult.elapsed"
    );
}
