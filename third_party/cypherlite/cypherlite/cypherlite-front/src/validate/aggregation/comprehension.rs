//! Aggregation is rejected inside list comprehension mapping/filter
//! expressions and map comprehension key/value/filter expressions.

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{Clause, Expr};

use crate::agg::contains_aggregation;
use crate::ast::collect::interp_embeds;

/// Validate that aggregation functions are not used inside list comprehension mapping expressions.
pub fn validate_list_comprehension_aggregation(clauses: &[Clause]) -> Result<(), EngineError> {
    for clause in clauses {
        match clause {
            Clause::Return(rc) => {
                for item in &rc.items {
                    check_list_comp_aggregation(&item.expr)?;
                }
            }
            Clause::With(wc) => {
                for item in &wc.items {
                    check_list_comp_aggregation(&item.expr)?;
                }
                if let Some(w) = &wc.where_expr {
                    check_list_comp_aggregation(w)?;
                }
            }
            Clause::Match(mc) => {
                if let Some(w) = &mc.where_expr {
                    check_list_comp_aggregation(w)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Check recursively for aggregation inside list comprehension mapping expressions.
fn check_list_comp_aggregation(expr: &Expr) -> Result<(), EngineError> {
    match expr {
        Expr::ListComprehension(lc) => {
            // Check if the mapping expression contains aggregation
            if let Some(map_expr) = &lc.map_expr {
                if contains_aggregation(map_expr) {
                    return Err(EngineError::semantic(
                        ErrorKind::InvalidAggregation,
                        "aggregation functions are not allowed inside list comprehension"
                            .to_string(),
                    ));
                }
            }
            if let Some(where_expr) = &lc.where_expr {
                if contains_aggregation(where_expr) {
                    return Err(EngineError::semantic(
                        ErrorKind::InvalidAggregation,
                        "aggregation functions are not allowed inside list comprehension"
                            .to_string(),
                    ));
                }
            }
            // Also check nested list comprehensions in the list_expr
            check_list_comp_aggregation(&lc.list_expr)?;
            Ok(())
        }
        Expr::MapComprehension(mc) => {
            // Aggregation is not allowed in the key/value projection or the
            // WHERE predicate (same rule as list comprehensions). The source
            // expression is only recursed into, so an aggregate there (e.g.
            // `{i IN collect(x) | 'k': i}`) stays allowed.
            for e in [&mc.key_expr, &mc.value_expr] {
                if contains_aggregation(e) {
                    return Err(EngineError::semantic(
                        ErrorKind::InvalidAggregation,
                        "aggregation functions are not allowed inside map comprehension"
                            .to_string(),
                    ));
                }
            }
            if let Some(where_expr) = &mc.where_expr {
                if contains_aggregation(where_expr) {
                    return Err(EngineError::semantic(
                        ErrorKind::InvalidAggregation,
                        "aggregation functions are not allowed inside map comprehension"
                            .to_string(),
                    ));
                }
            }
            check_list_comp_aggregation(&mc.source_expr)?;
            Ok(())
        }
        Expr::FunctionCall(fc) => {
            for arg in &fc.args {
                check_list_comp_aggregation(arg)?;
            }
            Ok(())
        }
        Expr::Binary(b) => {
            check_list_comp_aggregation(&b.left)?;
            check_list_comp_aggregation(&b.right)?;
            Ok(())
        }
        Expr::Comparison(c) => {
            check_list_comp_aggregation(&c.left)?;
            check_list_comp_aggregation(&c.right)?;
            Ok(())
        }
        Expr::Unary(u) => check_list_comp_aggregation(&u.expr),
        Expr::Paren(inner) => check_list_comp_aggregation(inner),
        Expr::Property(p) => check_list_comp_aggregation(&p.expr),
        Expr::StringInterp(parts) => {
            for e in interp_embeds(parts) {
                check_list_comp_aggregation(e)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
