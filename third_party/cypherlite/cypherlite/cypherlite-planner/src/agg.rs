//! Aggregation checks: grouping-key discipline for projections and the
//! ORDER BY / WHERE aggregation restrictions.

use cypherlite_core::{Error, Result};
// The aggregate-name set is owned by `cypherlite-ir` and re-exported by
// `cypherlite-front::agg` (which adds the CIP-128 alias spellings). The
// planner binds against this single definition instead of keeping a local
// copy.
use cypherlite_front::agg::is_aggregate_name as is_agg_fn;
use cypherlite_ir::{is_agg_expr, Expr, MapProjectionItem, OrderItem, Projection};

pub(crate) fn check_agg_projections(items: &[Projection], group_by: Option<&[Expr]>) -> Result<()> {
    // A `*` projection item is only a grouping key under implicit grouping
    // or `GROUP BY ALL` — the latter lowers the star into the key list,
    // where the executor expands it to the carried variables. With an
    // explicit key list (`GROUP BY k, ...`) or the empty grouping set
    // (`GROUP BY ()`) the star is not among the keys, and its carried
    // non-key variables would silently project first-row-of-group values
    // (ADR-0198 *Later references*). This check must run before the
    // no-aggregates early return below: an explicit GROUP BY without
    // aggregate items still takes the aggregation path (it acts as
    // DISTINCT), so `WITH *, n GROUP BY n` is the same wrong-answer hole.
    if let Some(keys) = group_by {
        if !keys.iter().any(|k| matches!(k, Expr::Star))
            && items.iter().any(|i| matches!(i.expr, Expr::Star))
        {
            return Err(Error::syntax(
                "AmbiguousAggregationExpression: `*` projection is not allowed with an explicit GROUP BY",
            ));
        }
    }
    if !items.iter().any(|i| is_agg_expr(&i.expr)) {
        return Ok(());
    }
    // Explicit `GROUP BY` keys replace the inferred non-aggregating items;
    // `Some(&[])` (`GROUP BY ()`) means there are no grouping keys at all.
    let inferred: Vec<&Expr> = items
        .iter()
        .filter(|i| !is_agg_expr(&i.expr))
        .map(|i| &i.expr)
        .collect();
    let group_keys: Vec<&Expr> = match group_by {
        Some(keys) => keys.iter().collect(),
        None => inferred,
    };
    for item in items {
        if !is_agg_expr(&item.expr) {
            continue;
        }
        let mut leaves = Vec::new();
        collect_non_agg_leaves(&item.expr, false, &mut leaves);
        for leaf in &leaves {
            if !leaf_allowed_as_group(leaf, &group_keys) {
                return Err(Error::syntax(
                    "AmbiguousAggregationExpression: non-aggregated expression must be a grouping key",
                ));
            }
        }
    }
    Ok(())
}

fn collect_non_agg_leaves(expr: &Expr, inside_agg: bool, out: &mut Vec<Expr>) {
    if matches!(expr, Expr::CountStar) {
        return;
    }
    let inside_agg = inside_agg || matches!(expr, Expr::Function { name, .. } if is_agg_fn(name));
    if matches!(expr, Expr::Variable(_) | Expr::Property { .. }) && !inside_agg {
        out.push(expr.clone());
        return;
    }
    // Comprehension/reduce variants bind their own identifiers; leaves inside
    // them are not grouping-key candidates, so do not descend (same coverage
    // as the historical walker).
    if matches!(
        expr,
        Expr::ListComp { .. }
            | Expr::MapComp { .. }
            | Expr::Quantifier { .. }
            | Expr::Reduce { .. }
            | Expr::AllReduce { .. }
    ) {
        return;
    }
    if let Expr::MapProjection {
        expr: target,
        items,
    } = expr
    {
        if !inside_agg {
            // The variable/property shorthand items are leaves with the same
            // shape the pre-typed pseudo-call produced.
            for item in items {
                match item {
                    MapProjectionItem::Variable { name } => {
                        out.push(Expr::Variable(name.clone()));
                    }
                    MapProjectionItem::Property { key } => {
                        out.push(Expr::Property {
                            expr: target.clone(),
                            key: key.clone(),
                        });
                    }
                    MapProjectionItem::KeyValue { .. } | MapProjectionItem::AllProperties => {}
                }
            }
        }
    }
    for child in expr.children() {
        collect_non_agg_leaves(child, inside_agg, out);
    }
}

fn leaf_allowed_as_group(leaf: &Expr, group_keys: &[&Expr]) -> bool {
    if group_keys.contains(&leaf) {
        return true;
    }
    // ADR-0198 ruling 1: a `*` projection item carries every in-scope
    // variable as a grouping key, so any variable leaf resolves against it.
    // Property leaves are NOT covered: `*` projects the variables, not
    // their property expressions, so the unprojected-property-leaf
    // rejection (battery i03) stands under `*` too.
    if matches!(leaf, Expr::Variable(_)) && group_keys.iter().any(|g| matches!(g, Expr::Star)) {
        return true;
    }
    if let Expr::Property { expr, .. } = leaf {
        if let Expr::Variable(v) = expr.as_ref() {
            return group_keys
                .iter()
                .any(|g| matches!(g, Expr::Variable(k) if k == v));
        }
    }
    false
}

pub(crate) fn reject_agg_in_pred(expr: &Expr) -> Result<()> {
    if is_agg_expr(expr) {
        return Err(Error::syntax(
            "InvalidAggregation: aggregations are not allowed in WHERE",
        ));
    }
    Ok(())
}

pub(crate) fn reject_order_by_unprojected_agg(
    items: &[Projection],
    order: &[OrderItem],
) -> Result<()> {
    let mut projected = Vec::new();
    for i in items {
        collect_agg_calls(&i.expr, &mut projected);
    }
    let group_keys: Vec<&Expr> = items
        .iter()
        .filter(|i| !is_agg_expr(&i.expr))
        .map(|i| &i.expr)
        .collect();
    let aliases: Vec<String> = items
        .iter()
        .enumerate()
        .filter(|(_, i)| !is_agg_expr(&i.expr))
        .map(|(i, item)| item.output_name(i))
        .collect();
    for o in order {
        if !is_agg_expr(&o.expr) {
            continue;
        }
        let mut calls = Vec::new();
        collect_agg_calls(&o.expr, &mut calls);
        if calls.iter().any(|c| !projected.iter().any(|p| p == c)) {
            return Err(Error::syntax(
                "UndefinedVariable: ORDER BY aggregation must be projected",
            ));
        }
        let mut leaves = Vec::new();
        collect_non_agg_leaves(&o.expr, false, &mut leaves);
        for leaf in &leaves {
            if !leaf_allowed_as_group(leaf, &group_keys)
                && !matches!(leaf, Expr::Variable(v) if aliases.iter().any(|a| a == v))
            {
                return Err(Error::syntax(
                    "UndefinedVariable: ORDER BY aggregation refers to a non-projected expression",
                ));
            }
        }
    }
    Ok(())
}

fn collect_agg_calls(expr: &Expr, out: &mut Vec<Expr>) {
    expr.visit(&mut |e| {
        if matches!(e, Expr::CountStar)
            || matches!(e, Expr::Function { name, .. } if is_agg_fn(name))
        {
            out.push(e.clone());
        }
    });
}

pub(crate) fn reject_order_by_agg(items: &[Projection], order: &[OrderItem]) -> Result<()> {
    let proj_has_agg = items.iter().any(|i| is_agg_expr(&i.expr));
    let order_has_agg = order.iter().any(|o| is_agg_expr(&o.expr));
    if order_has_agg && !proj_has_agg {
        return Err(Error::syntax(
            "InvalidAggregation: ORDER BY aggregation without aggregating projection",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(name: &str) -> Expr {
        Expr::Variable(name.into())
    }

    fn prop(name: &str, key: &str) -> Expr {
        Expr::Property {
            expr: Box::new(var(name)),
            key: key.into(),
        }
    }

    fn collect(arg: Expr) -> Expr {
        Expr::Function {
            name: "collect".into(),
            distinct: false,
            args: vec![arg],
        }
    }

    fn plus(left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op: cypherlite_ir::BinaryOp::Add,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn item(expr: Expr) -> Projection {
        Projection {
            expr,
            alias: Some("xs".into()),
            source_text: None,
        }
    }

    // ADR-0198 ruling 1: a `*` item carries every in-scope variable as a
    // grouping key, so the carried `z` in `collect(n.x) + [z]` resolves.
    #[test]
    fn star_covers_variable_leaves() {
        let items = vec![
            item(Expr::Star),
            item(plus(collect(prop("n", "x")), Expr::List(vec![var("z")]))),
        ];
        check_agg_projections(&items, None).unwrap();
    }

    // `*` projects the variables, not their property expressions: the
    // unprojected property leaf `n.y` stays rejected (battery i03).
    #[test]
    fn star_does_not_cover_property_leaves() {
        let items = vec![
            item(Expr::Star),
            item(plus(
                collect(prop("n", "x")),
                Expr::List(vec![prop("n", "y")]),
            )),
        ];
        let err = check_agg_projections(&items, None).unwrap_err();
        assert!(
            err.to_string().contains("AmbiguousAggregationExpression"),
            "got: {err}"
        );
    }

    // Without `*`, a carried variable that is not projected stays rejected
    // (battery i01).
    #[test]
    fn unprojected_variable_leaf_stays_rejected() {
        let items = vec![item(plus(
            collect(prop("n", "x")),
            Expr::List(vec![var("y")]),
        ))];
        let err = check_agg_projections(&items, None).unwrap_err();
        assert!(
            err.to_string().contains("AmbiguousAggregationExpression"),
            "got: {err}"
        );
    }

    // A `*` projection under an explicit GROUP BY key list would silently
    // project carried non-key variables with first-row-of-group values
    // (wrong-answer audit finding; ADR-0198 Later references): reject it.
    #[test]
    fn star_with_explicit_group_by_keys_rejected() {
        let items = vec![item(Expr::Star), item(Expr::CountStar)];
        let keys = vec![prop("n", "x")];
        let err = check_agg_projections(&items, Some(&keys)).unwrap_err();
        assert!(
            err.to_string().contains("AmbiguousAggregationExpression"),
            "got: {err}"
        );
    }

    // `GROUP BY ()` (the empty grouping set) rejects `*` for the same
    // reason: the star is not among the (zero) keys.
    #[test]
    fn star_with_empty_grouping_set_rejected() {
        let items = vec![item(Expr::Star), item(Expr::CountStar)];
        let err = check_agg_projections(&items, Some(&[])).unwrap_err();
        assert!(
            err.to_string().contains("AmbiguousAggregationExpression"),
            "got: {err}"
        );
    }

    // The zero-aggregate form takes the same executor path (an explicit
    // GROUP BY with no aggregate items acts as DISTINCT), so the rejection
    // must fire before the no-aggregates early return.
    #[test]
    fn star_with_explicit_group_by_no_aggregate_rejected() {
        let items = vec![item(Expr::Star), item(var("n"))];
        let keys = vec![var("n")];
        let err = check_agg_projections(&items, Some(&keys)).unwrap_err();
        assert!(
            err.to_string().contains("AmbiguousAggregationExpression"),
            "got: {err}"
        );
    }

    // `GROUP BY ALL` lowers the `*` item into the key list, where the
    // executor expands it to the carried variables: the combination stays
    // valid.
    #[test]
    fn group_by_all_star_allowed() {
        let items = vec![item(Expr::Star), item(Expr::CountStar)];
        let keys = vec![Expr::Star];
        check_agg_projections(&items, Some(&keys)).unwrap();
    }

    // Implicit grouping (no GROUP BY clause) keeps the ADR-0198 ruling-1
    // behavior: `*` carries every in-scope variable as a grouping key.
    #[test]
    fn implicit_grouping_star_allowed() {
        let items = vec![item(Expr::Star), item(Expr::CountStar)];
        check_agg_projections(&items, None).unwrap();
    }
}
