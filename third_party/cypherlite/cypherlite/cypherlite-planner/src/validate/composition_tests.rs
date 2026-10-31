//! The composition rules read the one per-clause write answer
//! ([`crate::clause_mutates_store`]), catalog writes included: a catalog
//! change persists while moving no ADR-0309 counter, and it is still a write
//! to every rule that rejects a write. The two rules this crate owns are
//! pinned here; the third (the expression-subquery rejection in `expr_check`)
//! reads the same predicate and is pinned end to end in the facade's
//! `cypherlite/tests/namespace.rs`.

use cypherlite_front::parse_and_lower;
use cypherlite_ir::{Clause, Expr};

use crate::binding::Env;
use crate::validate::validate_part_with_env;

/// Lower `cypher` and run the semantic clause validation on its one part,
/// returning the rejection's text.
fn rejection(cypher: &str) -> String {
    let query = parse_and_lower(cypher).unwrap_or_else(|error| panic!("{cypher}: {error}"));
    let [part] = query.parts.as_slice() else {
        panic!("{cypher}: expected one part");
    };
    validate_part_with_env(part, Env::new(), &mut |_| false)
        .expect_err("the composition rule rejects this statement")
        .to_string()
}

/// A catalog write ahead of `USE` is a write ahead of `USE`: the placement
/// rule asks the one write predicate, so `ALTER CURRENT GRAPH TYPE` cannot
/// dress itself up as a read.
///
/// The `USE` clause is inserted by hand because the front-end's own placement
/// gate refuses a misplaced `USE` during lowering — for every statement the
/// product admits, that gate answers first. This planner arm is the backstop
/// for a plan that reached the planner some other way, and it reads the same
/// write answer `CALL … IN TRANSACTIONS` does.
#[test]
fn use_after_a_catalog_write_is_rejected() {
    let mut query = parse_and_lower("ALTER CURRENT GRAPH TYPE SET GRAPH TYPE SOGraph RETURN 1")
        .expect("the catalog statement lowers");
    let [part] = query.parts.as_mut_slice() else {
        panic!("expected one part");
    };
    part.clauses.insert(
        1,
        Clause::Use {
            graph: Expr::String("neo4j".to_string()),
        },
    );
    let error = validate_part_with_env(part, Env::new(), &mut |_| false)
        .expect_err("the placement rule rejects a USE after a write")
        .to_string();
    assert!(
        error.contains("InvalidPlacementOfUseClause: USE must be the first clause"),
        "unexpected error: {error}"
    );
}

/// A catalog write cannot precede `CALL { … } IN TRANSACTIONS`; the rule asks
/// "did a write run before the batched subquery?", and a catalog write did.
#[test]
fn catalog_write_before_call_in_transactions_is_rejected() {
    let error = rejection(
        "ALTER CURRENT GRAPH TYPE SET GRAPH TYPE SOGraph \
         CALL { CREATE (n:Boxed) } IN TRANSACTIONS OF 1 ROWS",
    );
    assert!(
        error.contains("WriteBeforeCallInTransactions"),
        "unexpected error: {error}"
    );
}
