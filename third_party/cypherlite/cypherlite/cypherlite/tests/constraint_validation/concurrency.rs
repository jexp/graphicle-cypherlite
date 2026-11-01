//! The closed concurrent duplicate-commit gap (ADR-0439) and the uniqueness
//! probes that run beside concurrent commits.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cypherlite::Value;

use crate::common::{err_text, scalar, uniqueness};
use crate::support::{REL_UNIQUE, UNIQUE};

/// ADR-0439: the concurrent duplicate-commit gap is closed at the commit
/// admission loop. Two transactions that each create the same unique value
/// no longer both commit: exactly one wins, and the loser's slot is
/// rejected with the retryable `Error::Conflict` before the WAL append.
/// A retry sees the winner and reports the precise violation.
#[test]
fn concurrent_duplicate_commit_is_rejected_at_admission() {
    let db = cypherlite::Database::open(":memory:").unwrap();
    let mut setup = db.connect().unwrap();
    setup
        .query("CREATE CONSTRAINT p_id FOR (n:P) REQUIRE n.id IS UNIQUE")
        .unwrap();
    let mut first = db.connect().unwrap();
    let mut second = db.connect().unwrap();
    first.begin().unwrap();
    second.begin().unwrap();
    first.query("CREATE (:P {id: 77})").unwrap();
    second.query("CREATE (:P {id: 77})").unwrap();
    first.commit().unwrap();
    assert!(matches!(second.commit(), Err(cypherlite::Error::Conflict)));
    let mut c = db.connect().unwrap();
    // Exactly one holder committed; the loser's ops are discarded.
    assert_eq!(
        scalar(&mut c, "MATCH (n:P {id: 77}) RETURN count(n)"),
        Value::Int(1)
    );
    // The retry reports the precise violation (ADR-0439's Conflict reply).
    assert_eq!(
        err_text(c.query("CREATE (:P {id: 77})")),
        uniqueness("p_id")
    );
    // Unrelated writes still succeed.
    c.query("CREATE (:Other)").unwrap();
    c.query("CREATE (:P {id: 78})").unwrap();
}

/// `Graph::prop_postings` holds a `(label, key)` pair's write guard while it
/// reads `label_index` and `nodes`; a commit's `index_add` takes that guard
/// while it holds a `nodes` read guard. An explicit transaction's statements
/// execute beside commits, so its uniqueness probes build and drain the
/// pair while other connections commit nodes into it. Every side finishes.
#[test]
fn uniqueness_probes_in_a_transaction_complete_beside_concurrent_commits() {
    uniqueness_probes_complete_beside_concurrent_commits(
        UNIQUE,
        |id| format!("CREATE (:P {{id: {id}}})"),
        "MATCH (n:P) RETURN count(n)",
    );
}

/// The relationship twin of the node test above (ADR-0185): the lazy pair
/// build holds the pair guard until the pair is built, so a concurrent
/// commit's relationship is either in the build's candidate copy or
/// dirty-marked afterwards. An explicit transaction's probes build and
/// drain the pair while other connections commit relationships into it;
/// every side finishes.
#[test]
fn relationship_uniqueness_probes_in_a_transaction_complete_beside_concurrent_commits() {
    uniqueness_probes_complete_beside_concurrent_commits(
        REL_UNIQUE,
        |id| format!("CREATE ()-[:R {{id: {id}}}]->()"),
        "MATCH ()-[r:R]->() RETURN count(r)",
    );
}

/// The shared harness of the two tests above: an explicit transaction
/// writes `STATEMENTS` tuples through `stmt` while `WRITERS` other
/// connections each commit `STATEMENTS` tuples through the same builder —
/// disjoint ids, so `ddl`'s constraint never fires — and the final
/// `count_query` count is every written tuple. The builder takes the
/// tuple's id (negative for the transaction, positive for the writers).
fn uniqueness_probes_complete_beside_concurrent_commits(
    ddl: &str,
    stmt: fn(i64) -> String,
    count_query: &str,
) {
    const WRITERS: i64 = 3;
    const STATEMENTS: i64 = 200;
    let db = cypherlite::Database::open(":memory:").unwrap();
    db.connect().unwrap().query(ddl).unwrap();
    let (done, finished) = mpsc::channel();
    let mut transaction = db.connect().unwrap();
    let transaction_done = done.clone();
    thread::spawn(move || {
        let outcome = (|| {
            transaction.begin()?;
            for i in 1..=STATEMENTS {
                transaction.query(&stmt(-i))?;
            }
            transaction.commit()
        })();
        transaction_done.send(outcome).unwrap();
    });
    for writer in 0..WRITERS {
        let mut c = db.connect().unwrap();
        let done = done.clone();
        thread::spawn(move || {
            let outcome =
                (0..STATEMENTS).try_for_each(|i| c.query(&stmt(writer * STATEMENTS + i)).map(drop));
            done.send(outcome).unwrap();
        });
    }
    drop(done);
    for _ in 0..=WRITERS {
        finished
            .recv_timeout(Duration::from_secs(120))
            .expect("the transaction and every writer finish")
            .unwrap();
    }
    let mut c = db.connect().unwrap();
    assert_eq!(
        scalar(&mut c, count_query),
        Value::Int((WRITERS + 1) * STATEMENTS)
    );
}
