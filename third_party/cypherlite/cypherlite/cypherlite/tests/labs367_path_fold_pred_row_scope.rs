//! LABS-367: the path fold's predicate cache is row-scoped — a
//! `PathPred::Cmp` verdict binds its other operand to the row it was
//! judged in, so it cannot leak into the next outer row.

mod common;

use cypherlite::Value;

use common::{connection, run, string};

/// LABS-367: the path fold's predicate cache is row-scoped. A path
/// predicate's other operand may reference an outer row variable
/// (`f.age > cutoff` — cutoff is not a hop name, so the pred compiles),
/// making its verdict valid only within one outer row. A verdict leaked
/// across rows drops the second row's friends: with UNWIND [25, 5], the
/// young friend (age 10) is rejected by row 1's cutoff-25 verdict and
/// never re-judged at cutoff 5.
#[test]
fn path_fold_predicate_verdicts_are_row_scoped() {
    let mut c = connection();
    run(
        &mut c,
        "MERGE (me:Person {name:'me', age:20}) \
         MERGE (old:Person {name:'old', age:30}) \
         MERGE (young:Person {name:'young', age:10}) \
         CREATE (me)-[:KNOWS]->(old) \
         CREATE (me)-[:KNOWS]->(young)",
    );

    // ORDER BY n makes the rows fully deterministic: assert them whole.
    let r = run(
        &mut c,
        "UNWIND [25, 5] AS cutoff \
         MATCH (me:Person {name:'me'})-[:KNOWS]->(f:Person) \
         WHERE f.age > cutoff \
         RETURN f.name AS n, count(*) AS c ORDER BY n",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![string("old"), Value::Int(2)],
            vec![string("young"), Value::Int(1)],
        ],
        "the young friend passes only cutoff 5 — its cutoff-25 verdict must not leak into row 2"
    );
}
