//! The ADR-0439 commit-time uniqueness admission races: the scheduler-driven
//! public-API races, the deterministic explicit-transaction harness they
//! share, and the pinned violation text. Split from `concurrency.rs` so that
//! module keeps only the reader/writer races and the durability modes.

use super::super::*;
use super::harness::*;
use crate::Database;

/// Two connections, one explicit transaction each: both `begin()` calls —
/// and therefore both snapshot pins — complete before either transaction
/// runs its statements or commits. Each side runs its own statement slice;
/// every statement then passes statement-time validation against the side's
/// own pinned snapshot, so only the commit admission loop can reject one.
/// The outcomes come back in thread order.
fn explicit_transaction_race(
    db: &Arc<Database>,
    statements: impl Fn(usize) -> &'static [&'static str] + Send + Sync + 'static,
) -> [Result<()>; SIDES] {
    use std::sync::Barrier;
    let opened = Arc::new(Barrier::new(SIDES));
    let pinned = Arc::new(Barrier::new(SIDES));
    let statements = Arc::new(statements);
    let mut handles = Vec::new();
    for side in 0..SIDES {
        let db = Arc::clone(db);
        let opened = Arc::clone(&opened);
        let pinned = Arc::clone(&pinned);
        let statements = Arc::clone(&statements);
        handles.push(std::thread::spawn(move || {
            let mut c = db.connect().unwrap();
            opened.wait();
            c.begin().unwrap();
            pinned.wait();
            for statement in statements(side) {
                c.query(statement)?;
            }
            c.commit()
        }));
    }
    handles
        .into_iter()
        .map(|handle| handle.join().expect("a racing connection panics"))
        .collect::<Vec<_>>()
        .try_into()
        .expect("two race outcomes")
}

/// ADR-0439: exactly one side commits; the other is rejected at admission
/// with the retryable `Error::Conflict`, never a panic or a second winner.
fn assert_one_winner(outcomes: [Result<()>; SIDES]) {
    let mut committed = 0;
    let mut rejected = 0;
    for outcome in outcomes {
        match outcome {
            Ok(()) => committed += 1,
            Err(Error::Conflict) => rejected += 1,
            Err(error) => panic!("unexpected race outcome: {error}"),
        }
    }
    assert_eq!(committed, 1, "exactly one side commits");
    assert_eq!(rejected, 1, "the other side is rejected at admission");
}

/// The pinned `unique`/`key` violation text (ADR-0172), derived from the
/// typed constructor the retry path raises. The integration harness states
/// the same text through its `uniqueness` helper
/// (`cypherlite/tests/common/mod.rs`); both sides must move together with
/// [`Error::uniqueness_violation`].
fn uniqueness_text(name: &str) -> String {
    Error::uniqueness_violation(name).to_string()
}

/// The ADR-0439 contract in the auto-commit form: two connections each
/// submit a single-statement `CREATE` of the same constrained value. When
/// both statements are in flight, both pass statement-time validation and
/// the commit admission loop admits exactly one and rejects the other with
/// the retryable `Error::Conflict`; the retry then reports the precise
/// violation.
///
/// A single-statement auto-commit has no seam between its snapshot pin and
/// its commit, so the scheduler can still fully serialize the two
/// submissions: the later statement pins after the winner committed and
/// fails at statement time with the same precise violation (the contract's
/// own retry outcome). That ordering does not exercise admission, so the
/// test retries a fresh database until it observes the in-flight
/// interleaving. A regression that admits both duplicates fails on the
/// first attempt, before any retry.
///
/// The retry loop is an accepted flake, not a correctness wait: on a quiet
/// single-core CI runner the scheduler may serialize every one of the
/// [`ATTEMPTS`] submissions, and the final `panic!` then means "could not
/// exercise the interleaving" (the admission path never ran), never that
/// the contract failed. Every serialized attempt still asserts its own
/// contract outcome — one winner, the retry's precise violation — so the
/// only state this test can wrongly flag is a scheduler that never overlaps
/// two submissions. There is no seam to force the overlap: an auto-commit
/// statement pins its snapshot and joins the commit group in the same call,
/// unlike the explicit-transaction helper above, whose `begin()` barrier
/// pins both snapshots deterministically.
#[test]
fn auto_commit_duplicate_creates_admit_exactly_one() {
    use std::sync::Barrier;
    const ATTEMPTS: usize = 16;
    for _ in 0..ATTEMPTS {
        let db = Arc::new(Database::open(":memory:").unwrap());
        db.connect()
            .unwrap()
            .query("CREATE CONSTRAINT p_id FOR (n:P) REQUIRE n.id IS UNIQUE")
            .unwrap();
        let barrier = Arc::new(Barrier::new(SIDES));
        let mut handles = Vec::new();
        for _ in 0..SIDES {
            let db = Arc::clone(&db);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let mut c = db.connect().unwrap();
                barrier.wait();
                c.query("CREATE (:P {id: 77})").map(drop)
            }));
        }
        let mut outcomes = handles
            .into_iter()
            .map(|handle| handle.join().expect("a racing connection panics"));
        let first = outcomes.next().expect("two race outcomes");
        let second = outcomes.next().expect("two race outcomes");
        match (first, second) {
            (Ok(()), Err(Error::Conflict)) | (Err(Error::Conflict), Ok(())) => {
                assert_eq!(count(&db, "MATCH (n:P {id: 77}) RETURN count(n)"), 1);
                assert_eq!(
                    db.connect()
                        .unwrap()
                        .query("CREATE (:P {id: 77})")
                        .unwrap_err()
                        .to_string(),
                    uniqueness_text("p_id"),
                    "the retry sees the winner and reports the precise violation"
                );
                return;
            }
            // Fully serialized submission: the winner's retry outcome, not
            // the admission path. The graph state is the same; retry.
            (Ok(()), Err(Error::UniquenessViolation { .. }))
            | (Err(Error::UniquenessViolation { .. }), Ok(())) => {
                assert_eq!(count(&db, "MATCH (n:P {id: 77}) RETURN count(n)"), 1);
            }
            (Ok(()), Ok(())) => panic!("both duplicate commits won"),
            (first, second) => panic!("unexpected race outcomes: {first:?}; {second:?}"),
        }
    }
    panic!(
        "could not exercise the in-flight interleaving in {ATTEMPTS} tries: \
         the scheduler serialized every submission (not a product failure)"
    );
}

/// ADR-0439: two open transactions move two different nodes' constrained
/// property to the same value. Both pass their own statement-time
/// validation; at commit the later slot finds the tuple already held and
/// is rejected, so exactly one node carries the value.
#[test]
fn set_races_on_a_constrained_value_admit_exactly_one() {
    let db = person_email_db();
    {
        let mut c = db.connect().unwrap();
        c.query("CREATE (:Person {email: 'a'}), (:Person {email: 'b'})")
            .unwrap();
    }
    const STATEMENTS: [&[&str]; 2] = [
        &["MATCH (n:Person {email: 'a'}) SET n.email = 'shared'"],
        &["MATCH (n:Person {email: 'b'}) SET n.email = 'shared'"],
    ];
    let outcomes = explicit_transaction_race(&db, |side| STATEMENTS[side]);
    assert_one_winner(outcomes);
    assert_eq!(
        count(&db, "MATCH (n:Person {email: 'shared'}) RETURN count(n)"),
        1
    );
    // Both nodes survive: the loser kept its own value.
    assert_eq!(count(&db, "MATCH (n:Person) RETURN count(n)"), 2);
}

/// ADR-0439: two open transactions each add the constrained label to a
/// different node that already carries the same constrained property
/// value. The `AddLabels` op introduces the tuple; exactly one side wins.
#[test]
fn add_labels_races_on_a_constrained_property_admit_exactly_one() {
    let db = person_email_db();
    {
        let mut c = db.connect().unwrap();
        c.query("CREATE (:Temp {email: 'same', tag: 1}), (:Temp {email: 'same', tag: 2})")
            .unwrap();
    }
    const STATEMENTS: [&[&str]; 2] = [
        &["MATCH (n:Temp {tag: 1}) SET n:Person"],
        &["MATCH (n:Temp {tag: 2}) SET n:Person"],
    ];
    let outcomes = explicit_transaction_race(&db, |side| STATEMENTS[side]);
    assert_one_winner(outcomes);
    assert_eq!(count(&db, "MATCH (n:Person) RETURN count(n)"), 1);
    assert_eq!(count(&db, "MATCH (n:Temp) RETURN count(n)"), 2);
}

/// ADR-0439's create-registration rule at the engine level: one transaction
/// creates an unconstrained `:Temp` node carrying the value and then adds the
/// constrained label in a second statement; the other transaction creates
/// the constrained `:Person` holder. Both pins precede both statements, so
/// statement-time validation passes on both sides and only the commit
/// admission loop can reject one. Pre-fix the create-then-label side
/// committed alongside the holder; now exactly one side wins.
#[test]
fn create_then_label_races_a_duplicate_person_admit_exactly_one() {
    let db = person_email_db();
    const STATEMENTS: [&[&str]; 2] = [
        &["CREATE (:Temp {email: 1})", "MATCH (n:Temp) SET n:Person"],
        &["CREATE (:Person {email: 1})"],
    ];
    let outcomes = explicit_transaction_race(&db, |side| STATEMENTS[side]);
    assert_one_winner(outcomes);
    // The winner's data is queryable and holds the tuple exactly once.
    assert_eq!(count(&db, "MATCH (n:Person {email: 1}) RETURN count(n)"), 1);
    // A subsequent duplicate create reports the precise violation.
    let error = db
        .connect()
        .unwrap()
        .query("CREATE (:Person {email: 1})")
        .unwrap_err();
    assert_eq!(error.to_string(), uniqueness_text("person_email"));
}

/// Two transactions create and then label a node into the same constrained
/// tuple; before the registration fix both committed — now exactly one
/// commits, pinned over eight independent rounds.
#[test]
fn create_then_label_same_slice_races_admit_exactly_one() {
    const STATEMENTS: [&[&str]; 2] = [
        &["CREATE (:Temp {email: 1})", "MATCH (n:Temp) SET n:Person"],
        &["CREATE (:Temp {email: 1})", "MATCH (n:Temp) SET n:Person"],
    ];
    for round in 0..8 {
        let db = person_email_db();
        let outcomes = explicit_transaction_race(&db, |side| STATEMENTS[side]);
        assert_one_winner(outcomes);
        assert_eq!(
            count(&db, "MATCH (n:Person {email: 1}) RETURN count(n)"),
            1,
            "round {round}: exactly one holder of the tuple"
        );
        assert_eq!(
            count(&db, "MATCH (n:Temp) RETURN count(n)"),
            1,
            "round {round}: the loser's node rolled back"
        );
    }
}

/// ADR-0439: a composite node key admits one holder of the full tuple. Two
/// transactions create the same two-property key concurrently; the second
/// slot is rejected, so one `:K` node survives.
#[test]
fn composite_node_key_races_admit_exactly_one() {
    let db = Arc::new(Database::open(":memory:").unwrap());
    db.connect()
        .unwrap()
        .query("CREATE CONSTRAINT p_ab FOR (n:K) REQUIRE (n.a, n.b) IS NODE KEY")
        .unwrap();
    let outcomes = explicit_transaction_race(&db, |_| &["CREATE (:K {a: 1, b: 2})"]);
    assert_one_winner(outcomes);
    assert_eq!(count(&db, "MATCH (n:K) RETURN count(n)"), 1);
    assert_eq!(count(&db, "MATCH (n:K {a: 1, b: 2}) RETURN count(n)"), 1);
}

/// ADR-0439: relationship constraints take the same admission path. Two
/// transactions create a relationship of the constrained type carrying the
/// same constrained property value; one commits, one conflicts.
#[test]
fn relationship_uniqueness_races_admit_exactly_one() {
    let db = Arc::new(Database::open(":memory:").unwrap());
    {
        let mut c = db.connect().unwrap();
        c.query("CREATE CONSTRAINT r_id FOR ()-[r:R]-() REQUIRE r.id IS UNIQUE")
            .unwrap();
        c.query("CREATE (:A), (:B)").unwrap();
    }
    let outcomes = explicit_transaction_race(&db, |_| {
        &["MATCH (a:A), (b:B) CREATE (a)-[:R {id: 7}]->(b)"]
    });
    assert_one_winner(outcomes);
    assert_eq!(count(&db, "MATCH ()-[r:R {id: 7}]->() RETURN count(r)"), 1);
}

/// ADR-0439's no-false-positives bar at stress scale: many connections
/// create distinct constrained values concurrently. The admission map must
/// reject none of them.
#[test]
fn concurrent_distinct_constrained_values_all_commit() {
    use std::sync::Barrier;
    const THREADS: usize = 8;
    const EACH: usize = 8;
    let db = person_email_db();
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::new();
    for thread in 0..THREADS {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let mut c = db.connect().unwrap();
            barrier.wait();
            for each in 0..EACH {
                c.query(&format!("CREATE (:Person {{email: 'p{thread}-{each}'}})"))
                    .unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().expect("a distinct-value writer panics");
    }
    assert_eq!(
        count(&db, "MATCH (n:Person) RETURN count(n)"),
        (THREADS * EACH) as i64
    );
}
