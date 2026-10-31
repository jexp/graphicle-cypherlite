//! The FOREACH side-effect boundary — the one home of its policy, split
//! out of the dispatcher (the dispatch arm is a one-line delegation, like
//! the DDL arms).

use std::collections::BTreeMap;

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::{Clause, Expr};

use super::dispatch::run_nested_clauses;
use crate::ctx::QueryCtx;
use crate::eval::{eval, into_items, Row};
use crate::overlay::ExecGraph;

/// Run a FOREACH clause over `rows`: the body is side effects — a RETURN
/// inside it is the one clause that categorically cannot appear there
/// (it would end the inner part and try to export through a boundary that
/// consumes only the writes), so the boundary rejects it here, before the
/// body runs — fail closed. The front's validate battery rejects the same
/// composition before lowering (`validate_foreach_body`, the statically
/// decidable home of the rule); this scan is defense-in-depth for callers
/// that bypass validation, and raises the identical message. Everything
/// else the body produces — a void CALL's rows, a conditional's merge —
/// is consumed by the boundary by design.
///
/// ADR-0395: each item's clause list is one nested clause-engine pass per
/// row, so a wide list compounds on the statement's budget like any other
/// nesting. FOREACH clauses are not the statement's operators: the nested
/// boundary is the explicit `None` site — no PROFILE facts cross it, and
/// no produced rows or statement result crosses it either.
pub(crate) fn exec_foreach(
    graph: &ExecGraph<'_>,
    rows: &[Row],
    variable: &str,
    list: &Expr,
    clauses: &[Clause],
    params: &BTreeMap<String, Value>,
    qctx: QueryCtx<'_>,
) -> Result<()> {
    if clauses
        .iter()
        .any(|clause| matches!(clause, Clause::Return { .. }))
    {
        return Err(Error::syntax(
            "InvalidClauseComposition: a FOREACH body cannot contain a RETURN",
        ));
    }
    for row in rows {
        let values = match eval(graph, row, list, params, qctx.cypher_version)? {
            Value::List(values) => values,
            Value::Null => continue,
            _ => return Err(Error::ty("FOREACH requires a list")),
        };
        for value in into_items(values) {
            let mut inner = row.clone();
            inner.insert(variable.to_string(), value);
            let _ = run_nested_clauses(graph, clauses, params, vec![inner], qctx)?;
        }
    }
    Ok(())
}
