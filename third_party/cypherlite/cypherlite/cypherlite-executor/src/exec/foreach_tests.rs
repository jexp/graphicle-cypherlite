//! The FOREACH side-effect boundary (dispatch): the body runs for its
//! writes only — a RETURN inside it is the one clause that cannot appear
//! there. The front's validate battery owns the rule for the product path
//! (`cypherlite-front`'s `validate_foreach_body` rejects it before
//! lowering); the boundary's own scan stays as defense-in-depth for
//! callers that bypass validation, raising the identical message.
//! Everything else the body produces — a void CALL's rows, a conditional's
//! merge — is consumed by the boundary by design.

use std::collections::BTreeMap;

use cypherlite_front::parse_and_lower;
use cypherlite_ir::{Clause, Expr};

use super::foreach::exec_foreach;
use super::test_support::write_scratch;
use crate::ctx::QueryCtx;
use crate::graph::Graph;
use crate::overlay::ExecGraph;

/// The product path rejects the composition before the planner or executor
/// ever see it, with the byte-identical text the boundary scan raises (the
/// front error renders `InvalidClauseComposition: …`, which the lowering
/// boundary wraps in the same `syntax error:` prefix the runtime error
/// carried).
#[test]
fn a_return_inside_a_foreach_body_fails_closed_at_the_front() {
    let err = parse_and_lower("FOREACH (i IN [1] | RETURN 1)")
        .expect_err("the RETURN guard rejects the body before execution");
    assert_eq!(
        err.to_string(),
        "syntax error: InvalidClauseComposition: a FOREACH body cannot contain a RETURN"
    );
}

/// The boundary's own scan still rejects a RETURN-bearing body handed to it
/// directly — defense-in-depth for a caller that bypasses front validation
/// (a hand-built plan, a future front gap). The clause list is built by
/// hand for exactly that reason: no product path can produce it anymore.
#[test]
fn the_boundary_scan_rejects_a_return_defense_in_depth() {
    let g = Graph::default();
    let scratch = write_scratch(&g);
    let view = ExecGraph::write(&g, &scratch);
    let clauses = vec![Clause::Return {
        distinct: false,
        group_by: None,
        items: Vec::new(),
        order: Vec::new(),
        skip: None,
        limit: None,
    }];
    let err = exec_foreach(
        &view,
        &[],
        "i",
        &Expr::Null,
        &clauses,
        &BTreeMap::new(),
        QueryCtx::new(None, ""),
    )
    .expect_err("the boundary scan rejects the body");
    assert_eq!(
        err.to_string(),
        "syntax error: InvalidClauseComposition: a FOREACH body cannot contain a RETURN"
    );
}
