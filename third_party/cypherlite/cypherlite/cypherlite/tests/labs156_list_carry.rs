//! LABS-156 regression: carrying a collected list through many rows and
//! indexing into it must not be quadratic (ADR-0170).
//!
//! Before ADR-0170 every variable read and row carry deep-copied String,
//! List, Map and Vector payloads, so each of the 2,000 `UNWIND` rows below
//! copied the 20,000-node list several times. Payloads are now Arc-shared and
//! those reads are refcount increments.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use cypherlite::{Database, Value};

/// The LABS-156 repro statement, verbatim.
const LABS156_REPRO: &str = "UNWIND range(1,$n) AS i CREATE (n:N {i: i}) \
     WITH collect(n) AS ns UNWIND range(0,1999) AS k \
     WITH k, ns[k] AS a, ns[(k+1) % $n] AS b RETURN count(*)";

/// Runs the LABS-156 repro at `$n = 20000` (ADR-0170) and requires it to
/// finish within 20 seconds.
///
/// The bound is deliberately loose: this test also runs in unoptimized debug
/// builds on shared, possibly loaded CI machines, where the fixed statement is
/// several times slower than its ~0.09 s release time. The pre-fix code took about 26 s even in a
/// release build, so a return of the quadratic copy exceeds 20 s in any build
/// while the fixed code stays far below it.
#[test]
fn labs156_indexing_a_carried_list_is_not_quadratic() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let params = BTreeMap::from([("n".to_string(), Value::Int(20_000))]);

    let started = Instant::now();
    let result = conn.query_with_params(LABS156_REPRO, params).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(result.rows, vec![vec![Value::Int(2_000)]]);
    assert!(
        elapsed < Duration::from_secs(20),
        "LABS-156 repro took {elapsed:?}; over 20 s suggests the quadratic \
         carry copy is back (ADR-0170)"
    );
}
