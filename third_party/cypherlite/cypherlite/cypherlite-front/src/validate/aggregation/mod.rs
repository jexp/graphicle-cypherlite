//! Aggregation semantics: nested aggregation, non-constant arguments,
//! single-input aggregate arity (LABS-276), grouping-key ambiguity in
//! mixed expressions, ORDER BY aggregation rules, and aggregation inside
//! list comprehensions.
//!
//! The family is split by concern:
//!
//! - [`order_by`]: ORDER BY aggregation rules
//!   ([`validate_order_by_aggregation`]).
//! - [`comprehension`]: aggregation rejected inside list/map comprehensions
//!   ([`validate_list_comprehension_aggregation`]).
//! - [`ambiguity`]: RETURN/WITH grouping-key ambiguity in mixed expressions
//!   (`is_ambiguous_aggregation` and the shared grouping-key variable set).
//!
//! This root holds the scope-tracking driver
//! ([`validate_aggregation_semantics`]), the grouping-key extraction
//! ([`grouping_key_exprs`]), the root-level per-call checks (nested
//! aggregation, non-constant arguments, and single-input arity — all
//! riding the shared [`check_agg_calls`] scaffold), and the shared
//! expression helpers the concerns build on.

use std::collections::HashSet;

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_ir::AggregateArity;
use cypherlite_parser::{
    Clause, Expr, FunctionCallExpr, GroupBySpec, OrderByItem, ReturnClause, ReturnItem, Statement,
    WithClause,
};

use crate::agg::contains_aggregation;
use crate::ast::collect::{collect_expr_variables, collect_pattern_element_vars, search_score_var};
use crate::ast::tree::walk_expr_with;
use crate::show_columns::{catalog_output_columns, catalog_produces_rows};

use crate::agg::SINGLE_INPUT_AGGREGATES;

mod ambiguity;
mod comprehension;
mod order_by;

use self::ambiguity::{grouping_key_vars, is_ambiguous_aggregation};

pub use self::comprehension::validate_list_comprehension_aggregation;
pub use self::order_by::validate_order_by_aggregation;

/// Check if a function name is an aggregation function.
fn is_aggregation_fn(name: &str) -> bool {
    crate::agg::is_aggregate_name(name)
}

/// Check if an expression is a direct aggregation call (not nested inside another expression).
fn is_direct_aggregation(expr: &Expr) -> bool {
    match expr {
        Expr::CountStar => true,
        Expr::FunctionCall(fc) => is_aggregation_fn(&fc.name),
        _ => false,
    }
}

/// The child-descent vocabulary the aggregation walks in this module agree
/// on (F-4-1): Binary/Comparison operands, Unary/Paren inner expressions,
/// Property *values*, and StringInterp embeds — expressed as a
/// [`walk_expr_with`] edge filter mirroring `contains_aggregation`, not a
/// second hand-rolled recursion. Function-call arguments are *not* part of
/// the vocabulary: the checks that ride this filter treat a non-aggregating
/// call's arguments as opaque (the historical verdict — e.g. nested
/// aggregation hidden inside `sin(...)` is not re-checked there) and
/// consume an aggregating call's arguments through their own per-argument
/// predicate instead of walking them.
fn agg_enter_child(parent: &Expr, child: &Expr) -> bool {
    match parent {
        Expr::Binary(_) | Expr::Comparison(_) | Expr::Unary(_) | Expr::Paren(_) => true,
        // A dynamic property key (`n[<expr>]`) is not descended into; the
        // static value side always is.
        Expr::Property(p) => std::ptr::eq(child, &p.expr),
        // The walker only offers the Interp embeds as children.
        Expr::StringInterp(_) => true,
        _ => false,
    }
}

/// The one per-aggregation-call guard scaffold (F-4-5/F-4-6/F-4-8): at an
/// aggregating function call (a node of the shared walker's visit, the
/// walker owning the recursion), `pred` judges the call itself — its
/// error propagates. The walker's edge filter ([`agg_enter_child`]) never
/// descends into function-call arguments: an aggregating call's arguments
/// are consumed by `pred`, and a non-aggregating call's arguments were
/// opaque to the historical hand-rolled recursion. Every root-level
/// aggregation check rides this single walk.
fn check_agg_calls(
    expr: &Expr,
    mut pred: impl FnMut(&FunctionCallExpr) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    let mut visit = |node: &Expr| {
        if let Expr::FunctionCall(fc) = node {
            if is_aggregation_fn(&fc.name) {
                pred(fc)?;
            }
        }
        Ok(())
    };
    walk_expr_with(expr, &mut visit, agg_enter_child)
}

/// The per-argument adapter of [`check_agg_calls`]: every argument of an
/// aggregating call must fail `pred` — one that satisfies it raises the
/// `kind`/`msg` semantic error.
fn check_agg_args(
    expr: &Expr,
    pred: impl Fn(&Expr) -> bool,
    kind: ErrorKind,
    msg: &str,
) -> Result<(), EngineError> {
    check_agg_calls(expr, |fc| {
        if fc.args.iter().any(&pred) {
            Err(EngineError::semantic(kind, msg.to_string()))
        } else {
            Ok(())
        }
    })
}

/// Check for nested aggregation: aggregation calls inside arguments of aggregation functions.
/// e.g., count(count(*)) should raise NestedAggregation.
fn check_nested_aggregation(expr: &Expr) -> Result<(), EngineError> {
    check_agg_args(
        expr,
        contains_aggregation,
        ErrorKind::NestedAggregation,
        "aggregation functions cannot be nested",
    )
}

/// Check for non-constant expressions inside aggregation function args.
/// e.g., count(rand()) should raise NonConstantExpression.
fn check_non_constant_in_aggregation(expr: &Expr) -> Result<(), EngineError> {
    check_agg_args(
        expr,
        contains_non_deterministic,
        ErrorKind::NonConstantExpression,
        "non-constant expression used inside aggregation",
    )
}

/// The one front-channel construction of the single-input arity
/// rejection (ADR-0217/ADR-0225): the same kind and clause
/// [`AggregateArity::rejection`] spells for the executor's `agg_eval`
/// backstop, with the kind in [`EngineError`]'s typed slot instead of
/// inline — `Display` prefixes it, so both channels render the same
/// text. The clause is restated here (not derived from `rejection`)
/// only because the contract exposes it kind-inlined for the
/// core-`Error` channel; the
/// `front_arity_rejection_matches_the_ir_contract_text` test fails if
/// either spelling drifts.
fn arity_rejection(name: &str, got: usize) -> EngineError {
    EngineError::semantic(
        ErrorKind::InvalidNumberOfArguments,
        format!("{name}(input) requires exactly one argument, got {got}"),
    )
}

/// Check the arity of single-input aggregation calls (LABS-276): any
/// aggregate in [`SINGLE_INPUT_AGGREGATES`] with an arity other than the
/// class contract's is a semantic `InvalidNumberOfArguments` error (Neo4j
/// 42I13 shape: it names the function and its signature) instead of
/// silently aggregating the constant 1 per row. Rides the shared
/// [`check_agg_calls`] scaffold — the same walk and reach as the other
/// aggregation checks. Membership, required count, and rejection text
/// all come from the one `cypherlite-ir` contract (ADR-0217) the
/// executor's backstop enforces: `count` stays out (the parser's keyword
/// path already guarantees its arity) and the percentiles keep their own
/// two-argument call-site check — exactly the split
/// [`SINGLE_INPUT_AGGREGATES`] derives from the contract.
fn check_aggregate_arity(expr: &Expr) -> Result<(), EngineError> {
    check_agg_calls(expr, |fc| {
        let lower = fc.name.to_ascii_lowercase();
        let canonical = crate::agg::canonical_agg_name(&lower);
        if SINGLE_INPUT_AGGREGATES.contains(&canonical)
            && fc.args.len() != AggregateArity::SingleInput.required()
        {
            return Err(arity_rejection(&fc.name, fc.args.len()));
        }
        Ok(())
    })
}

/// The RETURN/WITH arity driver (LABS-276): the item expressions, the
/// clause's grouping-key expressions (explicit GROUP BY keys when
/// present, the inferred non-aggregating items otherwise — both already
/// computed by the caller for the item checks), and the ORDER BY
/// expressions of one projection clause all pass through
/// [`check_aggregate_arity`].
fn check_clause_aggregate_arity(
    items: &[ReturnItem],
    key_exprs: &[&Expr],
    order_by: &[OrderByItem],
) -> Result<(), EngineError> {
    for item in items {
        check_aggregate_arity(&item.expr)?;
    }
    for key in key_exprs {
        check_aggregate_arity(key)?;
    }
    for ob in order_by {
        check_aggregate_arity(&ob.expr)?;
    }
    Ok(())
}

/// Check if an expression contains a non-deterministic function call like
/// rand(). Rides the shared walker with an edge filter that reproduces the
/// historical reach exactly: any function call's arguments, plus the
/// module's [`agg_enter_child`] vocabulary — everything else (subscripts,
/// list literals, comprehensions, …) hides its contents from this check,
/// as it always has.
fn contains_non_deterministic(expr: &Expr) -> bool {
    let mut found = false;
    let mut visit = |node: &Expr| {
        found |= matches!(node, Expr::FunctionCall(fc)
            if matches!(fc.name.to_lowercase().as_str(), "rand" | "randomuuid"));
        Ok(())
    };
    // Delta over [`agg_enter_child`]: this check also descends into any
    // function call's arguments; everything else is the shared vocabulary.
    let enter_child = |parent: &Expr, child: &Expr| match parent {
        Expr::FunctionCall(_) => true,
        _ => agg_enter_child(parent, child),
    };
    // Infallible here: the visitor never fails, and every caller runs after
    // the whole-tree depth cap, so the walker's own cap cannot fire on
    // these trees (the hand-rolled recursion would have overflowed instead).
    let result = walk_expr_with(expr, &mut visit, enter_child);
    debug_assert!(
        result.is_ok(),
        "contains_non_deterministic walked a tree that already passed the depth cap"
    );
    found
}

/// Collect variables from the non-aggregation parts of an expression.
/// This collects variables that appear outside of aggregation function calls.
/// Rides the shared walker: an aggregating call's whole subtree is skipped
/// (its arguments are aggregation inputs), a non-aggregating call's
/// arguments are walked, and a subscript contributes its postfix base and
/// static index but not its slice bounds.
fn collect_non_agg_variables(expr: &Expr, vars: &mut HashSet<String>) {
    let mut visit = |node: &Expr| {
        if let Expr::Variable(v) = node {
            vars.insert(v.clone());
        }
        Ok(())
    };
    // Deltas over [`agg_enter_child`] (the fn doc above gives the
    // semantics of each).
    let enter_child = |parent: &Expr, child: &Expr| match parent {
        Expr::FunctionCall(fc) => !is_aggregation_fn(&fc.name),
        Expr::Subscript(s) => {
            std::ptr::eq(child, &s.expr) || s.index.as_ref().is_some_and(|i| std::ptr::eq(child, i))
        }
        _ => agg_enter_child(parent, child),
    };
    // Infallible for the same reason as `contains_non_deterministic`.
    let result = walk_expr_with(expr, &mut visit, enter_child);
    debug_assert!(
        result.is_ok(),
        "collect_non_agg_variables walked a tree that already passed the depth cap"
    );
}

/// Collect variables that appear inside aggregation function arguments.
/// Rides the shared walker: at an aggregating call the arguments are
/// collected wholesale via [`collect_expr_variables`], and only the
/// Binary/Comparison/Unary/Paren/StringInterp skeleton is walked between
/// calls — a property access never contributes (the historical reach).
fn collect_vars_inside_aggregation(expr: &Expr, vars: &mut HashSet<String>) {
    let mut visit = |node: &Expr| {
        if let Expr::FunctionCall(fc) = node {
            if is_aggregation_fn(&fc.name) {
                // Collect all variables from arguments
                for arg in &fc.args {
                    collect_expr_variables(arg, vars);
                }
            }
        }
        Ok(())
    };
    let enter_child = |parent: &Expr, _child: &Expr| {
        matches!(
            parent,
            Expr::Binary(_)
                | Expr::Comparison(_)
                | Expr::Unary(_)
                | Expr::Paren(_)
                | Expr::StringInterp(_)
        )
    };
    // Infallible for the same reason as `contains_non_deterministic`.
    let result = walk_expr_with(expr, &mut visit, enter_child);
    debug_assert!(
        result.is_ok(),
        "collect_vars_inside_aggregation walked a tree that already passed the depth cap"
    );
}

/// The one "every variable in this set must be visible" check the
/// DISTINCT ORDER BY validation shares (F-1-15): three identical
/// collect-then-membership-check loops with the same error shape.
fn require_vars_defined(
    vars: &HashSet<String>,
    allowed: impl Fn(&str) -> bool,
) -> Result<(), EngineError> {
    for v in vars {
        if !allowed(v) {
            return Err(EngineError::semantic(
                ErrorKind::UndefinedVariable,
                format!("variable '{v}' not defined"),
            ));
        }
    }
    Ok(())
}

/// The names a projection item list exposes (F-1-10): each item's alias
/// when present, else a bare variable's name, else the item's source
/// text — plus the bare pre-alias variables, which a DISTINCT projection
/// may still be ordered by. `*` items contribute nothing here; the call
/// sites decide what a star means against the incoming scope.
fn projected_names(items: &[ReturnItem]) -> (HashSet<String>, HashSet<String>) {
    let mut projected = HashSet::new();
    let mut source_vars = HashSet::new();
    for item in items {
        if let Some(alias) = &item.alias {
            projected.insert(alias.clone());
        } else if let Expr::Variable(v) = &item.expr {
            projected.insert(v.clone());
        } else if let Some(src) = &item.source_text {
            projected.insert(src.clone());
        }
        if let Expr::Variable(v) = &item.expr {
            source_vars.insert(v.clone());
        }
    }
    (projected, source_vars)
}

/// The one construction of the grouping-key ambiguity rejection: the
/// projection-item check and the WITH ORDER BY check judge the same
/// policy on different expression sets, so the error they raise is
/// built once (LABS-295: two inline copies spelled the clause
/// independently).
fn ambiguous_aggregation_error() -> EngineError {
    EngineError::semantic(
        ErrorKind::AmbiguousAggregationExpression,
        "expression contains both aggregation and non-grouped variables".to_string(),
    )
}

/// The per-item checks RETURN and WITH share (F-1-9): nested aggregation,
/// non-constant aggregation arguments, and grouping-key ambiguity in
/// mixed expressions.
fn check_projection_items(
    items: &[ReturnItem],
    grouping_key_exprs: &[&Expr],
) -> Result<(), EngineError> {
    for item in items {
        check_nested_aggregation(&item.expr)?;
        check_non_constant_in_aggregation(&item.expr)?;

        // Ambiguous aggregation: the expression contains aggregation but
        // also references non-grouped variables.
        if contains_aggregation(&item.expr)
            && !is_direct_aggregation(&item.expr)
            && is_ambiguous_aggregation(&item.expr, grouping_key_exprs)
        {
            return Err(ambiguous_aggregation_error());
        }
    }
    Ok(())
}

/// The split-scope ORDER BY variable check the aggregating
/// RETURN-DISTINCT and WITH arms share (F-1-16): variables *inside* an
/// ORDER BY aggregation re-evaluate over the projection's input scope and
/// are checked against `in_agg`; variables *outside* any aggregation
/// reference the projected rows and are checked against `non_agg`.
fn check_order_by_vars(
    ob_expr: &Expr,
    in_agg: impl Fn(&str) -> bool,
    non_agg: impl Fn(&str) -> bool,
) -> Result<(), EngineError> {
    let mut vars_in_agg = HashSet::new();
    collect_vars_inside_aggregation(ob_expr, &mut vars_in_agg);
    require_vars_defined(&vars_in_agg, in_agg)?;
    let mut non_agg_vars = HashSet::new();
    collect_non_agg_variables(ob_expr, &mut non_agg_vars);
    require_vars_defined(&non_agg_vars, non_agg)
}

/// RETURN DISTINCT + ORDER BY (F-1-4): every ORDER BY variable must name
/// a projected column. A DISTINCT projection may still be ordered by the
/// exact pre-alias variable a projected column was built from (e.g.
/// `RETURN DISTINCT x AS y ORDER BY x`) — Neo4j allows this (verified
/// live for CIP-245's `SHOW FUNCTIONS ... RETURN DISTINCT name AS result
/// ORDER BY name`), because it's unambiguous: `x` and `y` are the same
/// value. This is narrower than "any in-scope variable": TCK's
/// ReturnOrderBy2 [13] confirms `RETURN DISTINCT a.name ORDER BY a.age`
/// must still be rejected, since `a.age` is a *different* expression
/// that DISTINCT-on-`a.name` doesn't preserve.
///
/// Precondition (caller-enforced): the driver rejects every aggregating
/// ORDER BY item of a non-aggregating RETURN before calling this, so a
/// RETURN reaching here with an aggregating ORDER BY item is itself
/// aggregating.
fn check_return_distinct_order_by(
    rc: &ReturnClause,
    scope: &HashSet<String>,
) -> Result<(), EngineError> {
    if !rc.distinct || rc.order_by.is_empty() {
        return Ok(());
    }
    // A `*` item resets the projection to the incoming scope; the
    // pre-alias variables of the items before it remain orderable.
    let (projected, source_vars) = match rc.items.iter().position(|i| i.all) {
        Some(star) => (scope.clone(), projected_names(&rc.items[..star]).1),
        None => projected_names(&rc.items),
    };
    // The DISTINCT orderability rule, bound once (F-4-9): an ORDER BY
    // variable outside any aggregation must name a projected column or the
    // exact pre-alias variable a projected column was built from.
    let orderable = |v: &str| projected.contains(v) || source_vars.contains(v);
    for ob in &rc.order_by {
        // When the projection aggregates, an ORDER BY aggregate is
        // re-evaluated over the projection's input scope
        // (`RETURN DISTINCT p.community, count(p) ORDER BY count(p)` —
        // LABS-260): variables *inside* an aggregation are pre-projection
        // references and are checked against `scope`, exactly like the
        // WITH driver. Variables *outside* any aggregation still
        // reference the projected rows and keep the DISTINCT check.
        //
        // Precondition (caller-enforced, F-4-3): the driver rejects every
        // aggregating ORDER BY item of a non-aggregating RETURN before this
        // check runs, so a RETURN reaching here with an aggregating ORDER
        // BY item is itself aggregating and `contains_aggregation` alone
        // selects the split-scope arm.
        if contains_aggregation(&ob.expr) {
            check_order_by_vars(&ob.expr, |v| scope.contains(v), orderable)?;
        } else {
            let mut ob_vars = HashSet::new();
            collect_expr_variables(&ob.expr, &mut ob_vars);
            require_vars_defined(&ob_vars, orderable)?;
        }
    }
    Ok(())
}

/// ORDER BY on an aggregating WITH (F-1-4): the ORDER BY runs over the
/// projected result, so only the grouping keys and the projected aliases
/// remain in scope — except inside an ORDER BY aggregate, which
/// re-evaluates over the pre-projection scope.
fn check_with_order_by(
    wc: &WithClause,
    grouping_key_exprs: &[&Expr],
    scope: &HashSet<String>,
) -> Result<(), EngineError> {
    if !wc.items.iter().any(|item| contains_aggregation(&item.expr)) || wc.order_by.is_empty() {
        return Ok(());
    }
    // Projected aliases (post-aggregation scope) — every aliased item's
    // alias included unconditionally, so a separate alias→expr map would
    // only ever test names this set already contains (F-4-2/F-4-4/F-4-7).
    let (projected_aliases, _) = projected_names(&wc.items);

    // Grouping-key variables, shared by every ORDER BY item.
    let grouping_vars = grouping_key_vars(grouping_key_exprs);

    // Extended grouping keys include alias variables (e.g. `me.age AS
    // age` means `age` is also a valid grouping reference); invariant
    // across the ORDER BY items.
    let alias_exprs: Vec<Expr> = wc
        .items
        .iter()
        .filter(|i| !contains_aggregation(&i.expr))
        .filter_map(|i| i.alias.as_ref().map(|a| Expr::Variable(a.clone())))
        .collect();
    let mut extended_keys = grouping_key_exprs.to_vec();
    extended_keys.extend(alias_exprs.iter());

    for ob in &wc.order_by {
        if contains_aggregation(&ob.expr) {
            // ORDER BY contains NEW aggregation — variables inside
            // aggregation functions must be from the pre-aggregation
            // scope, but the aggregation itself must match a projected
            // column or use only projected/grouped variables.
            //
            // A non-agg variable that is neither a grouping-key variable
            // nor a projected alias is undefined (WithOrderBy4 [19]:
            // `WITH count(you.age) AS agg ORDER BY me.age + count(...)` —
            // `me` was not projected, so it is UndefinedVariable, not
            // AmbiguousAggregationExpression).
            check_order_by_vars(
                &ob.expr,
                |v| scope.contains(v) || projected_aliases.contains(v),
                |v| grouping_vars.contains(v) || projected_aliases.contains(v),
            )?;

            if is_ambiguous_aggregation(&ob.expr, &extended_keys) {
                return Err(ambiguous_aggregation_error());
            }
        }
    }
    Ok(())
}

/// Validate aggregation semantics across RETURN and WITH clauses.
/// Checks for:
/// - Nested aggregation (count(count(*)))
/// - Non-constant expressions in aggregation (count(rand()))
/// - Ambiguous aggregation expressions (non-grouped variables mixed with aggregation)
/// - Aggregation in ORDER BY when clause has no aggregation (RETURN ... ORDER BY max(...))
/// - Undefined variables in ORDER BY aggregation expressions
pub fn validate_aggregation_semantics(stmt: &Statement) -> Result<(), EngineError> {
    let mut scope: HashSet<String> = HashSet::new();

    for clause in &stmt.clauses {
        match clause {
            Clause::Match(mc) => {
                for path in &mc.pattern {
                    if let Some(pv) = &path.variable {
                        scope.insert(pv.clone());
                    }
                    for elem in &path.elements {
                        collect_pattern_element_vars(elem, &mut scope);
                    }
                }
                if let Some(sv) = search_score_var(mc) {
                    scope.insert(sv.to_string());
                }
            }
            Clause::Unwind(uw) => {
                scope.insert(uw.alias.clone());
            }
            Clause::LoadCSV(lc) => {
                scope.insert(lc.alias.clone());
            }
            // Catalog commands (SHOW …, TERMINATE TRANSACTIONS) put their
            // effective output columns in scope; the command set and the
            // column math have one home in `show_columns` (F-1-1).
            clause if catalog_produces_rows(clause) => {
                for v in catalog_output_columns(clause) {
                    scope.insert(v);
                }
            }
            Clause::Create(cc) => {
                for path in &cc.pattern {
                    if let Some(pv) = &path.variable {
                        scope.insert(pv.clone());
                    }
                    for elem in &path.elements {
                        collect_pattern_element_vars(elem, &mut scope);
                    }
                }
            }
            Clause::Return(rc) => {
                let has_agg = rc.items.iter().any(|item| contains_aggregation(&item.expr));

                // Collect grouping key expressions once for the clause
                // (explicit GROUP BY keys when present, otherwise the
                // non-aggregation items).
                let grouping_key_exprs = grouping_key_exprs(rc.group_by.as_ref(), &rc.items);
                check_projection_items(&rc.items, &grouping_key_exprs)?;
                check_clause_aggregate_arity(&rc.items, &grouping_key_exprs, &rc.order_by)?;

                // Check ORDER BY for aggregation when RETURN has no aggregation
                if !has_agg {
                    for ob in &rc.order_by {
                        if contains_aggregation(&ob.expr) {
                            return Err(EngineError::semantic(ErrorKind::InvalidAggregation,
                                "aggregation functions are not allowed in ORDER BY when there is no aggregation in RETURN"
                                    .to_string(),
                            ));
                        }
                    }
                }

                check_return_distinct_order_by(rc, &scope)?;
            }
            Clause::With(wc) => {
                // Collect grouping key expressions once for the clause; the
                // item checks and the ORDER BY checks share them.
                let grouping_key_exprs = grouping_key_exprs(wc.group_by.as_ref(), &wc.items);
                check_projection_items(&wc.items, &grouping_key_exprs)?;
                check_clause_aggregate_arity(&wc.items, &grouping_key_exprs, &wc.order_by)?;
                check_with_order_by(wc, &grouping_key_exprs, &scope)?;

                // Reset scope to the projected variables. A `*` item
                // carries the incoming scope; items after it still add
                // their names on top.
                scope = match wc.items.iter().position(|i| i.all) {
                    Some(star) => {
                        let mut carried = scope.clone();
                        carried.extend(projected_names(&wc.items[star + 1..]).0);
                        carried
                    }
                    None => projected_names(&wc.items).0,
                };
            }
            _ => {}
        }
    }
    Ok(())
}

/// The grouping-key expressions of a projection clause (`WITH`/`RETURN`).
///
/// With an explicit `<group by clause>` the keys are those written by the
/// user: `GROUP BY ALL` means every non-aggregating item, `GROUP BY ()` means
/// none, and `GROUP BY <expr>...` means exactly those expressions. An
/// explicit key that is a bare variable matching a projection alias resolves
/// to that item's expression (the same rule the lowering applies). Without a
/// `GROUP BY` subclause the keys are inferred from the non-aggregating items.
/// Borrowed, not owned: every caller immediately re-borrows the result
/// (round-3 review — the owned version deep-cloned each key expression).
fn grouping_key_exprs<'a>(
    group_by: Option<&'a GroupBySpec>,
    items: &'a [cypherlite_parser::ReturnItem],
) -> Vec<&'a Expr> {
    let inferred = || {
        items
            .iter()
            .filter(|i| !contains_aggregation(&i.expr))
            .map(|i| &i.expr)
    };
    match group_by {
        None | Some(GroupBySpec::All) => inferred().collect(),
        Some(GroupBySpec::Empty) => Vec::new(),
        Some(GroupBySpec::Keys(keys)) => keys
            .iter()
            .map(|k| match k {
                Expr::Variable(name) => items
                    .iter()
                    .find(|i| i.alias.as_deref() == Some(name.as_str()))
                    .map(|i| &i.expr)
                    .unwrap_or(k),
                _ => k,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(query: &str) -> Result<(), EngineError> {
        let stmt = cypherlite_parser::parse(query).unwrap();
        validate_aggregation_semantics(&stmt)
    }

    // LABS-260: an ORDER BY aggregate over the projection's input scope is
    // valid when the DISTINCT projection itself aggregates.
    #[test]
    fn distinct_return_order_by_aggregate_uses_input_scope() {
        // Direct re-aggregation of a projected aggregate.
        let ok = validate(
            "MATCH (p:Person) RETURN DISTINCT p.community, count(p) ORDER BY count(p) DESC",
        );
        assert!(ok.is_ok(), "projected aggregate in ORDER BY: {ok:?}");

        // Ordering by the aggregate's alias keeps working.
        let ok =
            validate("MATCH (p:Person) RETURN DISTINCT p.community, count(p) AS c ORDER BY c DESC");
        assert!(ok.is_ok(), "alias ordering: {ok:?}");

        // A variable that only exists pre-projection but is referenced
        // outside any aggregation is still undefined (ReturnOrderBy2 [13]
        // shape: DISTINCT does not preserve it).
        let err =
            validate("MATCH (p:Person) RETURN DISTINCT p.community, count(p) ORDER BY p.name")
                .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UndefinedVariable);

        // A variable inside the ORDER BY aggregate that was never in scope
        // is still undefined.
        let err =
            validate("MATCH (p:Person) RETURN DISTINCT p.community, count(p) ORDER BY count(z)")
                .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UndefinedVariable);
    }

    // LABS-276 / ADR-0225: every single-input aggregate requires exactly
    // one argument (Neo4j 42I13), wherever it appears in the projection
    // clause — items, GROUP BY keys, ORDER BY expressions, and both the
    // RETURN and WITH forms.
    #[test]
    fn zero_argument_single_input_aggregates_error() {
        // The canonical SINGLE_INPUT_AGGREGATES table drives the loop, so
        // a name added to the table without validation reach fails here
        // first.
        for agg in SINGLE_INPUT_AGGREGATES {
            let err = validate(&format!("CREATE (n) RETURN {agg}()")).unwrap_err();
            assert_eq!(
                err.kind(),
                ErrorKind::InvalidNumberOfArguments,
                "{agg}() item: {err:?}"
            );
            let err = validate(&format!("UNWIND [1] AS x WITH {agg}() AS s RETURN s")).unwrap_err();
            assert_eq!(
                err.kind(),
                ErrorKind::InvalidNumberOfArguments,
                "WITH {agg}() item: {err:?}"
            );
        }
    }

    #[test]
    fn zero_argument_aggregate_nested_in_expression_errors() {
        let err = validate("MATCH (n) RETURN 1 + sum()").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidNumberOfArguments);
        let err = validate("MATCH (n) RETURN count(*) ORDER BY min()").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidNumberOfArguments);
        let err = validate("MATCH (n) RETURN n.id GROUP BY max()").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidNumberOfArguments);
    }

    // ADR-0217: the front's arity rejection and the executor's `agg_eval`
    // backstop rejection are one contract — the `AggregateArity` table in
    // `cypherlite-ir`. The core-`Error` channel spells the kind inline in
    // the contract's `rejection`; the front's `EngineError` carries the
    // kind typed and `Display` prefixes it. This pins the byte-equivalence
    // of the two renderings, so neither spelling can drift alone.
    #[test]
    fn front_arity_rejection_matches_the_ir_contract_text() {
        for (name, got) in [("sum", 0), ("stDev", 2), ("collect_list", 3)] {
            let front = arity_rejection(name, got).to_string();
            let contract = AggregateArity::SingleInput.rejection(name, got);
            assert_eq!(
                front, contract,
                "{name} rejection drifted from the contract"
            );
        }
    }

    #[test]
    fn one_argument_aggregates_and_the_matching_shapes_still_validate() {
        // One-argument calls, count(*), count(x), and the zero-input-row
        // semantics (sum=0, avg/min/max/collect of nothing) are unchanged.
        assert!(validate(
            "UNWIND [] AS x RETURN sum(x), avg(x), min(x), max(x), collect(x), count(x), count(*)"
        )
        .is_ok());
        assert!(validate("MATCH (n:Person) RETURN n.age GROUP BY n.age").is_ok());
        assert!(validate("MATCH (n:Person) RETURN count(n) ORDER BY count(n) DESC").is_ok());
    }

    // The percentile aggregates stay out of the arity table (ADR-0225):
    // their two-argument check lives at their call site, not here.
    #[test]
    fn percentile_aggregates_keep_their_own_call_site_arity_check() {
        assert!(validate("MATCH (n) RETURN percentileCont(n.age, 0.5)").is_ok());
        // Zero or one argument is not this module's error — the call-site
        // check in the executor rejects it.
        assert!(validate("MATCH (n) RETURN percentileCont()").is_ok());
        assert!(validate("MATCH (n) RETURN percentileDisc(n.age)").is_ok());
    }
}
