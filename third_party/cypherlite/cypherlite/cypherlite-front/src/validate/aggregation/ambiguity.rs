//! RETURN/WITH grouping-key ambiguity: in an expression that mixes
//! aggregation with non-aggregation, every non-aggregation variable must be
//! a grouping-key variable, and the non-aggregation parts must be simple
//! (variables, property accesses, literals, lists/maps of grouped vars) —
//! anything more complex is ambiguous.

use std::collections::HashSet;

use cypherlite_parser::Expr;

use super::collect_non_agg_variables;
use crate::agg::contains_aggregation;
use crate::ast::collect::{collect_expr_variables, interp_embeds};

/// The grouping keys of a projection clause, bundled with the two facts the
/// ambiguity checks need about them: the variables the key expressions
/// mention, and whether a `*` item is among the keys. Bundling them keeps
/// the ADR-0198 ruling-1 policy in one place and collapses the per-leaf
/// pass-through plumbing to a single `&GroupingKeys` parameter.
struct GroupingKeys<'a> {
    exprs: &'a [&'a Expr],
    vars: HashSet<String>,
    star: bool,
}

impl<'a> GroupingKeys<'a> {
    fn new(exprs: &'a [&'a Expr]) -> Self {
        Self {
            exprs,
            vars: grouping_key_vars(exprs),
            // ADR-0198 ruling 1: a `*` item carries every in-scope variable
            // as a grouping key.
            star: exprs.iter().any(|k| matches!(k, Expr::Star)),
        }
    }

    /// ADR-0198 ruling 1: a `*` item covers every in-scope variable;
    /// otherwise the variable must appear in a grouping key expression.
    fn covers_variable(&self, v: &str) -> bool {
        self.star || self.vars.contains(v)
    }

    /// The variable appears in a grouping key expression — `*` coverage
    /// does NOT apply here: `*` projects the variables, not their property
    /// expressions, so property targets keep the exact-variable rule.
    fn key_variable(&self, v: &str) -> bool {
        self.vars.contains(v)
    }

    /// The expression is structurally identical to a grouping key
    /// expression.
    fn matches_key(&self, expr: &Expr) -> bool {
        self.exprs.contains(&expr)
    }
}

/// Collect every variable mentioned by the grouping key expressions into
/// one set. Runs once per [`GroupingKeys`] construction, plus once per
/// aggregating WITH ... ORDER BY section in the driver; the result is passed
/// down by reference, so the per-leaf checks below never rebuild it.
pub(super) fn grouping_key_vars(grouping_key_exprs: &[&Expr]) -> HashSet<String> {
    let mut vars = HashSet::new();
    for key in grouping_key_exprs {
        collect_expr_variables(key, &mut vars);
    }
    vars
}

/// Check if an expression that mixes aggregation and non-aggregation is ambiguous.
/// Returns true if the expression is ambiguous (invalid).
///
/// The rule: in a mixed expression, the non-aggregation variables must all be
/// grouping key variables (i.e., variables that appear in non-aggregation projection items).
/// However, the non-agg sub-expression itself must not be a "complex" expression
/// (i.e., more than a simple variable, property access, literal, or list/map of grouped vars).
///
/// Special cases that are always valid:
/// - Constants/literals mixed with aggregation (e.g., count(*) + 10)
/// - Parameters mixed with aggregation
/// - Simple variables that are grouping keys (e.g., n + count(*) when n is grouped)
/// - Property accesses on grouping key variables (e.g., n.age + count(*) when n.age is grouped)
pub(super) fn is_ambiguous_aggregation(expr: &Expr, grouping_key_exprs: &[&Expr]) -> bool {
    let keys = GroupingKeys::new(grouping_key_exprs);

    // Collect non-aggregation variables from the expression
    let mut non_agg_vars = HashSet::new();
    collect_non_agg_variables(expr, &mut non_agg_vars);

    // If no non-agg variables, it's not ambiguous (pure agg + constants)
    if non_agg_vars.is_empty() {
        return false;
    }

    // If any non-agg variable is not covered by the grouping keys, it's ambiguous
    if non_agg_vars.iter().any(|v| !keys.covers_variable(v)) {
        return true;
    }

    // Even if all vars are grouping vars, check if the non-agg sub-expression
    // is more complex than what the grouping keys provide.
    // The non-agg parts must structurally match a grouping key expression
    // or be a simple expression built from grouping key variables.
    check_non_agg_parts_match_keys(expr, &keys)
}

/// Check if non-aggregation sub-expressions in a mixed aggregation expression
/// structurally match grouping key expressions.
/// Returns true if there's a mismatch (ambiguous).
fn check_non_agg_parts_match_keys(expr: &Expr, keys: &GroupingKeys) -> bool {
    match expr {
        Expr::Binary(b) => check_mixed_sides(&b.left, &b.right, keys),
        Expr::Comparison(c) => check_mixed_sides(&c.left, &c.right, keys),
        Expr::Paren(inner) => check_non_agg_parts_match_keys(inner, keys),
        Expr::StringInterp(parts) => {
            interp_embeds(parts).any(|e| check_non_agg_parts_match_keys(e, keys))
        }
        _ => false,
    }
}

/// Two-sided dispatch shared by the Binary and Comparison arms. Both arms
/// state the same policy: the planner's IR lowers comparisons to the same
/// `Expr::Binary` node and checks them identically, so the front end does
/// too (stellar F-1-11 — the Comparison arm's missing both-sides recursion
/// was a copy-paste divergence inherited from the original import, not a
/// ruling).
///
/// - Exactly one side aggregates: the other side must be a valid
///   non-aggregation part.
/// - Both sides aggregate: recurse into each; a nested mixed sub-expression
///   can still hide an invalid non-aggregation part.
/// - Neither side aggregates: nothing to check here.
fn check_mixed_sides(left: &Expr, right: &Expr, keys: &GroupingKeys) -> bool {
    let left_agg = contains_aggregation(left);
    let right_agg = contains_aggregation(right);
    if left_agg != right_agg {
        let non_agg_side = if left_agg { right } else { left };
        return !is_valid_non_agg_part(non_agg_side, keys);
    }
    if left_agg {
        return check_non_agg_parts_match_keys(left, keys)
            || check_non_agg_parts_match_keys(right, keys);
    }
    false
}

/// Check if a non-aggregation sub-expression is valid (matches a grouping key
/// or is a simple/constant expression).
fn is_valid_non_agg_part(expr: &Expr, keys: &GroupingKeys) -> bool {
    // Constants and literals are always valid
    match expr {
        Expr::Literal(_) => return true,
        Expr::Parameter(_) => return true,
        Expr::List(items) => {
            // List of grouped items is fine
            return items.iter().all(|i| is_valid_non_agg_part(i, keys));
        }
        // Map expressions are always valid (they don't reference ungrouped variables)
        _ => {}
    }

    // Simple variables covered by the grouping keys are valid.
    if let Expr::Variable(v) = expr {
        return keys.covers_variable(v);
    }

    // Property access: valid when it exactly matches a grouping key
    // expression, or when it targets a grouping key variable (`*` coverage
    // does not extend to property expressions — see `key_variable`).
    if let Expr::Property(p) = expr {
        if keys.matches_key(expr) {
            return true;
        }
        // Also allow property access on a grouping key variable
        if let Expr::Variable(v) = &p.expr {
            return keys.key_variable(v);
        }
        return false;
    }

    // Anything more complex (Binary, FunctionCall, etc.) is ambiguous
    // even if it matches a grouping key expression
    false
}
