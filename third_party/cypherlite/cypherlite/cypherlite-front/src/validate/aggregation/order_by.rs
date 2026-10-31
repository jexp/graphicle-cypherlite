//! ORDER BY aggregation rules: ORDER BY must not use aggregation when the
//! clause itself projects no aggregation, and in an aggregation query every
//! ORDER BY aggregation expression must match a projected aggregation
//! expression (with the non-aggregate parts kept simple).

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{Clause, Expr};

use super::is_direct_aggregation;
use crate::agg::contains_aggregation;
use crate::ast::collect::interp_embeds;

/// Validate that ORDER BY in WITH/RETURN doesn't contain aggregation functions
/// when the clause itself is not an aggregation query.
/// Also validates that ORDER BY aggregation expressions in aggregation queries
/// match projected aggregation expressions.
pub fn validate_order_by_aggregation(clauses: &[Clause]) -> Result<(), EngineError> {
    for clause in clauses {
        let (items, order_by) = match clause {
            Clause::With(wc) => (&wc.items, &wc.order_by),
            Clause::Return(rc) => (&rc.items, &rc.order_by),
            _ => continue,
        };

        if order_by.is_empty() {
            continue;
        }

        let has_agg_in_items = items.iter().any(|item| contains_aggregation(&item.expr));

        if !has_agg_in_items {
            // No aggregation in projected items — ORDER BY must not use aggregation
            for ob in order_by {
                if contains_aggregation(&ob.expr) {
                    return Err(EngineError::semantic(ErrorKind::InvalidAggregation, "aggregation functions are not allowed in ORDER BY when clause has no aggregation".to_string()));
                }
            }
        } else {
            // Aggregation query — validate ORDER BY aggregation expressions
            for ob in order_by {
                if contains_aggregation(&ob.expr) {
                    validate_ob_agg_expr(&ob.expr, items)?;
                }
            }
        }
    }
    Ok(())
}

/// Validate an ORDER BY expression that contains aggregation, checking:
/// 1. All aggregation sub-expressions must match a projected aggregation expression
/// 2. Non-aggregate parts must be simple (variables, properties, constants, params)
fn validate_ob_agg_expr(
    expr: &Expr,
    items: &[cypherlite_parser::ReturnItem],
) -> Result<(), EngineError> {
    // Collect projected aggregation expressions
    let projected_aggs: Vec<&Expr> = items
        .iter()
        .filter(|item| contains_aggregation(&item.expr))
        .map(|item| &item.expr)
        .collect();

    // Check each aggregation sub-expression in the ORDER BY
    check_ob_agg_sub_exprs(expr, &projected_aggs)
}

/// Recursively check ORDER BY aggregation expression validity.
///
/// Entry invariant: `contains_aggregation(expr)`. Every call site — the
/// top-level guard in `validate_order_by_aggregation` and the per-side /
/// per-arg dispatch below — only reaches this function with an expression
/// that contains aggregation.
fn check_ob_agg_sub_exprs(expr: &Expr, projected_aggs: &[&Expr]) -> Result<(), EngineError> {
    debug_assert!(contains_aggregation(expr));
    // If this expression matches a projected aggregation, it's fine
    for proj in projected_aggs {
        if *proj == expr {
            return Ok(());
        }
    }

    // If this IS an aggregation but doesn't match any projected one, error
    if is_direct_aggregation(expr) {
        return Err(EngineError::semantic(
            ErrorKind::UndefinedVariable,
            "ORDER BY contains a non-projected aggregation expression".to_string(),
        ));
    }

    match expr {
        Expr::Binary(b) => check_ob_agg_mixed_sides(&b.left, &b.right, projected_aggs),
        Expr::Comparison(c) => check_ob_agg_mixed_sides(&c.left, &c.right, projected_aggs),
        Expr::Unary(u) => check_ob_agg_sub_exprs(&u.expr, projected_aggs),
        Expr::Paren(inner) => check_ob_agg_sub_exprs(inner, projected_aggs),
        // Match contains_aggregation's base-only reach, excluding keys/indices/bounds.
        Expr::Property(p) => check_ob_agg_sub_exprs(&p.expr, projected_aggs),
        Expr::Subscript(s) => check_ob_agg_sub_exprs(&s.expr, projected_aggs),
        Expr::FunctionCall(fc) => {
            // If it's an aggregation function not matching projected ones, already caught above
            for arg in &fc.args {
                if contains_aggregation(arg) {
                    check_ob_agg_sub_exprs(arg, projected_aggs)?;
                }
            }
            Ok(())
        }
        Expr::StringInterp(parts) => {
            for e in interp_embeds(parts) {
                if contains_aggregation(e) {
                    check_ob_agg_sub_exprs(e, projected_aggs)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Two-sided dispatch shared by the Binary and Comparison arms: recurse
/// into an aggregating side, validate a non-aggregating one. The planner's
/// IR lowers comparisons to the same `Expr::Binary` node and checks them
/// identically, so the front end does too (stellar F-3-3 — the missing
/// Comparison arm let a mixed comparison escape the mixed-side validation
/// its Binary equivalent gets).
fn check_ob_agg_mixed_sides(
    left: &Expr,
    right: &Expr,
    projected_aggs: &[&Expr],
) -> Result<(), EngineError> {
    for side in [left, right] {
        if contains_aggregation(side) {
            check_ob_agg_sub_exprs(side, projected_aggs)?;
        } else {
            validate_ob_non_agg_part(side)?;
        }
    }
    Ok(())
}

/// Validate that a non-aggregate part of an ORDER BY expression in an aggregation context
/// is a simple expression.
///
/// Complex expressions (Binary, etc.) are NOT allowed even if they match a grouping key,
/// because Cypher requires referencing grouping keys by alias in ORDER BY when mixed
/// with aggregation. Everything else — variables, property accesses, literals,
/// parameters — is deliberately accepted here without a grouping-key or alias
/// match: the backstops that reject genuinely invalid references are the WITH
/// driver's ORDER BY non-aggregation-variable check in
/// [`validate_aggregation_semantics`](super::validate_aggregation_semantics)
/// (WithOrderBy4 [19]) and the planner's `reject_order_by_unprojected_agg`,
/// which see the full scope.
fn validate_ob_non_agg_part(expr: &Expr) -> Result<(), EngineError> {
    match expr {
        Expr::Binary(_) => {
            // Complex expression: ambiguous even if it matches a grouping key
            Err(EngineError::semantic(
                ErrorKind::AmbiguousAggregationExpression,
                "complex non-aggregate expressions cannot be mixed with aggregation in ORDER BY"
                    .to_string(),
            ))
        }
        // Deliberate leniency (see the doc comment for the backstops).
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(query: &str) -> Result<(), EngineError> {
        let stmt = cypherlite_parser::parse(query).unwrap();
        validate_order_by_aggregation(&stmt.clauses)
    }

    #[test]
    fn postfix_wrappers_validate_aggregate_bases_only() {
        for order_by in [
            "head(collect(n)).x",
            "collect(n)[0]",
            "collect(n)[..1]",
            // Non-projected aggregates in indices/bounds remain outside this check.
            "collect(n)[count(*)]",
            "collect(n)[count(*)..count(*) + 1]",
        ] {
            let projected = validate(&format!(
                "MATCH (n) RETURN collect(n) AS xs ORDER BY {order_by}"
            ));
            assert!(
                projected.is_ok(),
                "projected base in {order_by}: {projected:?}"
            );

            let err = validate(&format!(
                "MATCH (n) RETURN count(*) AS c ORDER BY {order_by}"
            ))
            .unwrap_err();
            assert_eq!(
                err.kind(),
                ErrorKind::UndefinedVariable,
                "non-projected base in {order_by}: {err}"
            );
        }
    }

    // Stellar F-3-3: a mixed comparison under ORDER BY is validated
    // identically to the equivalent Binary form.
    #[test]
    fn comparison_mixed_sides_match_binary_verdicts() {
        // Rejected: `n.x * 2` parses as a bare `Expr::Binary`, so the
        // non-aggregate side is a complex expression — the Binary form has
        // always rejected this shape as ambiguous, and the Comparison form
        // now rejects it with the same error kind.
        let binary = validate("MATCH (n) RETURN count(*) AS c ORDER BY count(*) + n.x * 2");
        let comparison = validate("MATCH (n) RETURN count(*) AS c ORDER BY count(*) > n.x * 2");
        for (shape, result) in [("Binary", binary), ("Comparison", comparison)] {
            let err = result.unwrap_err();
            assert_eq!(
                err.kind(),
                ErrorKind::AmbiguousAggregationExpression,
                "{shape} form must reject a complex non-aggregate side, got: {err}"
            );
        }

        // Accepted: `(n.x + 1)` parses as `Expr::Paren`, which the
        // non-aggregate-part check deliberately lets through (see
        // `validate_ob_non_agg_part`), and a projected aggregate on the
        // other side is fine — both forms accept.
        let binary = validate("MATCH (n) RETURN count(*) AS c ORDER BY count(*) + (n.x + 1)");
        let comparison = validate("MATCH (n) RETURN count(*) AS c ORDER BY count(*) > (n.x + 1)");
        assert!(binary.is_ok(), "Binary form: {binary:?}");
        assert!(comparison.is_ok(), "Comparison form: {comparison:?}");

        // Rejected: a non-projected aggregate fails through the shared
        // recursion for both forms alike.
        let binary = validate("MATCH (n) RETURN count(*) AS c ORDER BY sum(n.x) + count(*)");
        let comparison = validate("MATCH (n) RETURN count(*) AS c ORDER BY sum(n.x) > count(*)");
        for (shape, result) in [("Binary", binary), ("Comparison", comparison)] {
            let err = result.unwrap_err();
            assert_eq!(
                err.kind(),
                ErrorKind::UndefinedVariable,
                "{shape} form must reject a non-projected aggregate, got: {err}"
            );
        }
    }
}
