//! LABS-366 rides a `flip_cmp` contract fix: `In` is not operand-symmetric
//! (`a IN b` ≠ `b IN a`), so a property on the RIGHT of an `IN` no longer
//! mirrors into an index seek — it declines to the shared scan tail and
//! the residual re-evaluates the predicate per candidate. The decline is
//! result-invariant (the residual re-applies the original predicate on
//! every candidate the old mirrored seek would have returned), so this
//! pins the path's answer: the trap node — whose tags EQUAL the left
//! value, exactly what the mirrored seek tested — never matches.

mod common;

use common::{rows, run};

#[test]
fn a_property_on_the_right_of_in_evaluates_through_the_residual() {
    let mut c = common::connection();
    run(&mut c, "CREATE (:Person {name:'trap', tags: [30]})");
    // `[30] IN p.tags` asks whether the LIST [30] is a member of tags —
    // and a stored list property's elements are never lists, so no node
    // can match. The 'no' node is the trap: its tags EQUAL [30], which is
    // exactly what the old mirrored seek (`p.tags IN [[30]]`, an equality
    // in disguise) would have matched.
    let found = rows(
        &mut c,
        "MATCH (p:Person) WHERE [30] IN p.tags RETURN p.name AS name",
    );
    assert!(
        found.is_empty(),
        "the right-side property IN is list membership, never a mirrored equality: {found:?}"
    );
}
