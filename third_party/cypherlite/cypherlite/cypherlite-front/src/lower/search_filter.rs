//! CIP-240 "Vector Search with Single-stage Filtering": the one grammar
//! walk over a `SEARCH` clause's inner `WHERE <search condition>`, run at
//! this module's lowering boundary, emitting the IR `SearchFilterPred`
//! list directly (Rules 0, 1, 3, 6, 7 enforced here; the RHS lowered, the
//! `NOT <property>` rewrite's FALSE literal supplied). Rule 2 (index
//! property membership) needs the resolved index and is checked at
//! execution, matching how Rule 4's NODE/RELATIONSHIP entity-type check
//! already works for the outer CIP-224 `SEARCH` surface.

use std::collections::{HashMap, HashSet};

use cypherlite_core::{Error, Result};
use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_ir::{expr_vars, SearchFilterPred, SearchOp};
use cypherlite_parser::Expr;

use super::expr::{bin_op, cmp_op, unary_op};
use super::lower_expr;

/// Decompose and lower a SEARCH clause's inner `WHERE <search condition>`
/// into its Rule 0 predicate list against the clause's `binding_var`:
/// flattens top-level `AND`, applies the `NOT <property>` and
/// flipped-operand rewrites (the short-form-range rewrite, e.g.
/// `a < x.prop <= b`, needs no extra work here -- the parser's own
/// chained-comparison desugaring, `Parser::parse_comparison`, already
/// turns it into an `AND` of two ordinary comparisons before this ever
/// runs), then enforces Rule 1, Rule 3's per-property AND-combination
/// limits, Rule 6, and Rule 7's ban on `<>`.
pub(super) fn lower_search_filters(
    binding_var: &str,
    where_expr: Option<&Expr>,
) -> Result<Vec<SearchFilterPred>> {
    let Some(where_expr) = where_expr else {
        // No inner WHERE: an empty predicate list (empty = no filters).
        return Ok(Vec::new());
    };
    let mut terms = Vec::new();
    flatten_and(where_expr, &mut terms);
    let mut predicates = Vec::with_capacity(terms.len());
    for term in terms {
        predicates.push(to_predicate(binding_var, term)?);
    }
    validate_and_combination(&predicates)?;
    Ok(predicates)
}

fn flatten_and<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::Binary(b) = expr {
        if matches!(bin_op(&b.op), Ok(cypherlite_ir::BinaryOp::And)) {
            flatten_and(&b.left, out);
            flatten_and(&b.right, out);
            return;
        }
    }
    out.push(expr);
}

/// A bare `<var>.<prop>` reference (Rule 0's "static property reference" --
/// never a dynamic `n[expr]` bracket access, which leaves `property` empty
/// and carries a `key_expr` instead).
fn var_property(e: &Expr) -> Option<(&str, &str)> {
    if let Expr::Property(pa) = e {
        if pa.key_expr.is_none() {
            if let Expr::Variable(v) = &pa.expr {
                return Some((v.as_str(), pa.property.as_str()));
            }
        }
    }
    None
}

/// The same error the pre-lowering validation phase raised for this
/// check, through the front's established bridge into the lowering's
/// core `Error` (kind and text preserved).
fn invalid_predicate_error() -> Error {
    Error::syntax(
        EngineError::semantic(
            ErrorKind::SyntaxError,
            "InvalidSearchFilterPredicate: invalid predicate for vector search filtering. \
             A vector search filter must consist of one or more predicates joined by AND, and the \
             combined predicates for each property must specify either an exact value (e.g. `x.prop = \
             1`), a half-bounded range (e.g. `x.prop >= 1`), or a bounded range (e.g. `x.prop > 1 AND \
             x.prop < 100`)",
        )
        .to_string(),
    )
}

fn to_predicate(binding_var: &str, term: &Expr) -> Result<SearchFilterPred> {
    // Each arm recognizes `(var, prop, op, rhs expression)` — Rule 1 is
    // then stated once, below, before the single predicate construction.
    let (var, prop, op, rhs_expr) = match term {
        // Rule 0 rewrite: `NOT <property>` -> `<property> = FALSE`.
        // Anything else under a `NOT` (an equality, a range, an arbitrary
        // expression) isn't rewritable and is rejected -- Examples
        // R0.5/IPF:8.
        Expr::Unary(u) if matches!(unary_op(&u.op), Ok(cypherlite_ir::UnaryOp::Not)) => {
            let Some((var, prop)) = var_property(&u.expr) else {
                return Err(invalid_predicate_error());
            };
            // The NOT rewrite's RHS is the FALSE literal.
            (var, prop, SearchOp::Eq, None)
        }
        Expr::Comparison(c) => {
            // Rule 7: only a `<simple comp op>` serves (`<>` and `=~` are
            // typed ops the canonical lowering recognizes, declined here
            // at the one construction site).
            let Some(op) =
                SearchOp::from_binary(cmp_op(&c.op).map_err(|_| invalid_predicate_error())?)
            else {
                return Err(invalid_predicate_error());
            };
            // Rule 0's flipped-operand rewrite: prefer the property on
            // the left (the canonical form); if only the right side is a
            // bare property reference, flip the operator and swap sides
            // (Example R0.8). If both sides are property references
            // (e.g. `movie.year = movie.rating`), the left side wins and
            // the right side is treated as an ordinary RHS expression --
            // Rule 6's self-reference check below then catches it if it
            // depends on the binding variable.
            if let Some((var, prop)) = var_property(&c.left) {
                (var, prop, op, Some(&c.right))
            } else if let Some((var, prop)) = var_property(&c.right) {
                (var, prop, op.flip(), Some(&c.left))
            } else {
                return Err(invalid_predicate_error());
            }
        }
        _ => return Err(invalid_predicate_error()),
    };
    // Rule 1 (binding variable identity) — stated once, at this boundary:
    // the variable is not carried past it.
    if var != binding_var {
        return Err(Error::syntax("WrongVariableForVectorSearchFilter"));
    }
    let rhs = match rhs_expr {
        Some(expr) => lower_expr(expr)?,
        None => cypherlite_ir::Expr::Bool(false),
    };
    let pred = SearchFilterPred {
        property: prop.to_string(),
        op,
        rhs,
    };
    // Rule 6 (the RHS may not reference the binding variable) --
    // index-independent, so it is checked here, once, instead of per
    // execution.
    let mut variables = HashSet::new();
    expr_vars(&pred.rhs, &mut variables);
    if variables.contains(binding_var) {
        return Err(Error::syntax("SelfReferencingVectorSearchFilter"));
    }
    Ok(pred)
}

/// Rule 3: predicates on the same property (the variable is the clause's
/// binding variable, enforced by Rule 1) must not combine
/// more than one lower-bound range, more than one upper-bound range, or an
/// equality with a range. Multiple equalities on the same property are
/// allowed (like a contradictory range, they simply can't all be satisfied
/// -- Rule 3's "contradictory predicates return no results" note, Example
/// R3.4).
fn validate_and_combination(predicates: &[SearchFilterPred]) -> Result<()> {
    /// One (variable, property) group's predicate-kind counts.
    #[derive(Default)]
    struct CombinationCounts {
        eq: u32,
        lower: u32,
        upper: u32,
    }
    let mut groups: HashMap<&str, CombinationCounts> = HashMap::new();
    for p in predicates {
        let entry = groups.entry(p.property.as_str()).or_default();
        match p.op {
            SearchOp::Eq => entry.eq += 1,
            SearchOp::Gt | SearchOp::Gte => entry.lower += 1,
            SearchOp::Lt | SearchOp::Lte => entry.upper += 1,
        }
    }
    for counts in groups.values() {
        if counts.lower > 1
            || counts.upper > 1
            || (counts.eq > 0 && (counts.lower > 0 || counts.upper > 0))
        {
            return Err(invalid_predicate_error());
        }
    }
    Ok(())
}
