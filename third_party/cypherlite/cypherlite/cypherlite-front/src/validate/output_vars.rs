//! Subquery output-variable analysis: simulate a (sub)query's column-name
//! evolution to find what its terminal projection exports.
//!
//! [`compute_subquery_output_vars`] / [`compute_subquery_output_vars_seeded`]
//! walk the clause list the way scope flows through it (MATCH/CREATE/MERGE
//! bind, WITH/RETURN project, `*` passes through) and report the names the
//! terminal projection would emit; [`local_stage_output_vars`] is the
//! single-stage base case. The scope validator's `CallSubquery` importing
//! WITH check and [`crate::call_scope`] build their rules on this family.

use std::collections::HashSet;

use cypherlite_parser::{Clause, Statement};

use crate::ast::collect::{bind_pattern_vars, projected_names, search_score_var};
use crate::show_columns::{catalog_output_columns, catalog_produces_rows};

/// Simulates the column-name evolution through `stmt`'s own MATCH/WITH/
/// UNWIND/RETURN clauses to determine what its terminal projection (last
/// RETURN, or a trailing WITH such as `EXISTS { WITH * }`) would output.
/// Used by the scope validator's `CallSubquery` legacy-importing-WITH
/// check (`super::scope`) to see whether an imported outer variable gets
/// re-exported by a later `*`.
pub(crate) fn compute_subquery_output_vars(stmt: &Statement) -> Option<HashSet<String>> {
    compute_subquery_output_vars_seeded(stmt, &HashSet::new())
}

/// Like `compute_subquery_output_vars`, but starts the walk with `seed`
/// already bound rather than an empty set -- used when the subquery's own
/// first clause is a legacy `WITH *` importing form, so a `*` there (and
/// any later un-narrowed `*`) correctly resolves to the outer imported
/// names instead of nothing. CIP-213: also recurses into a `.next` chain,
/// so the *true* terminal projection of a (possibly NEXT-chained) body is
/// found -- e.g. a naked `CALL { RETURN 1 AS x NEXT RETURN x + 1 AS y }`
/// exports `y`, not `x`.
pub(crate) fn compute_subquery_output_vars_seeded(
    stmt: &Statement,
    seed: &HashSet<String>,
) -> Option<HashSet<String>> {
    let local = local_stage_output_vars(stmt, seed);
    match &stmt.next {
        Some(next_stmt) => {
            compute_subquery_output_vars_seeded(next_stmt, &local.unwrap_or_default())
        }
        None => local,
    }
}

/// The declared output of `stmt`'s *own* clause list alone, ignoring any
/// `.next` chain -- exactly what this one stage projects. This is the
/// single-stage base case of `compute_subquery_output_vars_seeded`'s
/// NEXT-chain recursion: it needs precisely *this* stage's own
/// contribution to seed the next one, where recursing further would
/// compute something about a *later* stage's own downstream chain.
fn local_stage_output_vars(stmt: &Statement, seed: &HashSet<String>) -> Option<HashSet<String>> {
    let mut bound: HashSet<String> = seed.clone();
    let mut last_projection: Option<HashSet<String>> = None;
    for clause in &stmt.clauses {
        match clause {
            Clause::Match(mc) => {
                bind_pattern_vars(&mc.pattern, &mut bound);
                if let Some(sv) = search_score_var(mc) {
                    bound.insert(sv.to_string());
                }
                last_projection = None;
            }
            Clause::With(wc) => {
                let projected = projected_names(&wc.items, &bound);
                bound = projected.clone();
                last_projection = Some(projected);
            }
            Clause::Unwind(uw) => {
                bound.insert(uw.alias.clone());
                last_projection = None;
            }
            Clause::Return(rc) => {
                last_projection = Some(projected_names(&rc.items, &bound));
            }
            // CIP-140/CIP-213: FINISH is always terminal (enforced
            // elsewhere) and exports nothing -- neither back into an
            // enclosing CALL's outer scope, nor (CIP-213) forward into a
            // following NEXT stage.
            Clause::Finish => {
                last_projection = Some(HashSet::new());
            }
            Clause::Create(cc) => {
                bind_pattern_vars(&cc.pattern, &mut bound);
                last_projection = None;
            }
            Clause::Merge(mc) => {
                bind_pattern_vars(std::slice::from_ref(&mc.pattern), &mut bound);
                last_projection = None;
            }
            Clause::CallSubquery(csq) => {
                let exports = crate::call_scope::call_subquery_exports(csq, &bound);
                bound.extend(exports);
                last_projection = None;
            }
            // A write clause after the last projection means the body does
            // not conclude with that projection: `WITH n DETACH DELETE n`
            // (the legacy importing-WITH idiom, ADR-0210) exports nothing,
            // so the importing WITH must not be mistaken for the terminal
            // projection -- otherwise the CALL collision check reports the
            // imported name as re-exported (`VariableAlreadyBound`). Matches
            // the Create/Merge arms above.
            Clause::Delete(_) | Clause::Set(_) | Clause::Remove(_) | Clause::Foreach(_) => {
                last_projection = None;
            }
            // CIP-46/CIP-187: a SHOW clause introduces its YIELD'd (or,
            // bare/`YIELD *`, its default/full catalog) column names into
            // scope, same as `call_subquery_exports`'s identical handling
            // of these clauses -- needed so a naked `CALL { SHOW
            // CONSTRAINTS }` (no RETURN inside the body at all) correctly
            // reports its SHOW clause's own default columns as what the
            // CALL exports, rather than nothing (CIP-187 01[1]: `CALL {
            // SHOW CONSTRAINTS } RETURN name, enforcedLabel`).
            c if catalog_produces_rows(c) => {
                let cols: HashSet<String> = catalog_output_columns(c).into_iter().collect();
                bound.extend(cols.iter().cloned());
                last_projection = Some(cols);
            }
            // CIP-135: a conditional clause is always the sole clause of
            // its list, so `bound` here is exactly `seed`. Rule 9/10
            // guarantee every branch/ELSE body projects the same output
            // names, so any one of them (preferring the first WHEN branch,
            // falling back to ELSE) represents the whole clause's output.
            Clause::Conditional(cc) => {
                let inner = cc
                    .branches
                    .first()
                    .map(|b| &b.body)
                    .or(cc.else_body.as_ref());
                last_projection =
                    inner.and_then(|s| compute_subquery_output_vars_seeded(s, &bound));
            }
            _ => {}
        }
    }
    last_projection
}
