//! The generic AST collectors every pass builds on: the variable and
//! pattern collectors, the output-column naming rule, the SEARCH
//! score-variable and distance-metric lookups, the shared expression-scope
//! check pair (`check_expr_vars_defined`/`check_name_defined`), and the
//! UNION-chain enumeration. They live below `validate`, `call_scope`,
//! `local_callables` and `lower` (all of which consume them downward) so
//! none of them owns another's helper. Aggregation-specific vocabulary
//! (`contains_aggregation`, the alias table) lives in [`crate::agg`]
//! instead, beside the names it tests.

use std::collections::HashSet;

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{
    Clause, Expr, InterpPart, MatchClause, PatternElement, PatternPath, ReturnItem, Statement,
    UnionPart,
};

use super::tree::walk_expr_with;
use crate::show_columns::{catalog_output_columns, catalog_produces_rows};

pub(crate) fn effective_name(item: &ReturnItem) -> Option<String> {
    if let Some(alias) = &item.alias {
        Some(alias.clone())
    } else if let Expr::Variable(v) = &item.expr {
        Some(v.clone())
    } else {
        item.source_text.clone()
    }
}

/// CIP-224: the `SCORE AS <var>` variable a `<search clause>` optionally
/// introduces, on top of its `<binding variable reference>` (already one of
/// the MATCH clause's own pattern element variables, so already covered by
/// `collect_pattern_element_vars`). Every "which variables does this MATCH
/// clause bind" scope tracker in this module and in `call_scope` folds this
/// in too, so a later WHERE/WITH/RETURN/ORDER BY can reference the score
/// variable regardless of which side of SEARCH the WHERE was written on
/// (Rule 6).
pub(crate) fn search_score_var(mc: &MatchClause) -> Option<&str> {
    mc.search.as_ref()?.score_var.as_deref()
}

/// Collects every pattern variable name a single `PatternElement` brings
/// into scope: a node's or relationship's own `variable`, or -- for a
/// CIP-28 `Quantified` element -- every named element inside its operand
/// (which become "group variables" in the enclosing scope) plus its own
/// subpath variable, if any (CIP-28 Section 3.3). Used by every "which
/// variables does this MATCH/CREATE/MERGE pattern bind" scope-tracking
/// check in this module so a group variable is visible to the rest of the
/// query exactly like any other pattern variable.
pub(crate) fn collect_pattern_element_vars(elem: &PatternElement, out: &mut HashSet<String>) {
    for_each_element_var(elem, &mut |name| {
        out.insert(name.to_owned());
    });
}

/// The one walk over the variables a pattern element introduces (a node's
/// or relationship's own variable, or a quantified operand's variables plus
/// its subpath variable). Both [`collect_pattern_element_vars`]'s set fold
/// and [`pattern_vars`]'s ordered list consume it.
fn for_each_element_var<'a>(elem: &'a PatternElement, visit: &mut impl FnMut(&'a str)) {
    match elem {
        PatternElement::Node(np) => {
            if let Some(v) = &np.variable {
                visit(v);
            }
        }
        PatternElement::Relationship(rp) => {
            if let Some(v) = &rp.variable {
                visit(v);
            }
        }
        PatternElement::Quantified(qpp) => {
            for inner in &qpp.elements {
                for_each_element_var(inner, visit);
            }
            if let Some(v) = &qpp.subpath_variable {
                visit(v);
            }
        }
    }
}

/// The argument index of `vector_distance`/`vector_norm`'s bare
/// distance-metric keyword, or `None` for every other function — the one
/// home of that fact, consulted by both the variable walker's edge filter
/// ([`collect_expr_variables`]) and the lowering's metric-keyword rewrite
/// (`lower::expr::resolve_vector_metric_arg`).
pub(crate) fn distance_metric_arg_index(name: &str) -> Option<usize> {
    match name.to_lowercase().as_str() {
        "vector_distance" => Some(2),
        "vector_norm" => Some(1),
        _ => None,
    }
}

/// Collect all variable names referenced in an expression.
///
/// Rides the shared self-capping walker ([`walk_expr_with`]) with an edge
/// filter that reproduces the historical hand-rolled recursion's reach
/// exactly: it descends into property *values*, binary/unary/comparison
/// operands, function arguments, list/map items, parentheses,
/// subscripts/slices, `IN`, `IS NULL`/`IS TYPE`/`IS NORMALIZED`, string
/// matches, CASE arms and `HAS LABEL` operands — but not into a dynamic
/// property *key* (`n[<expr>]`), the bare distance-metric keyword of
/// `vector_distance`/`vector_norm` (CIP-214: e.g. EUCLIDEAN parses
/// identically to a variable reference but is resolved as a metric literal
/// in `function.rs`, not looked up in row scope), or any scope-introducing
/// or pattern-bearing construct (list/pattern comprehensions, quantifiers,
/// `reduce`/`allReduce`, map projections, `shortestPath()`, pattern
/// predicates): variables used inside those are bound there, not references
/// in the enclosing scope. Subquery bodies (`EXISTS`/`COUNT`/`COLLECT
/// { … }`) are opaque leaves to the walker, as they were to the hand-rolled
/// recursion.
pub(crate) fn collect_expr_variables(expr: &Expr, vars: &mut HashSet<String>) {
    let mut visit = |node: &Expr| {
        if let Expr::Variable(v) = node {
            vars.insert(v.clone());
        }
        Ok(())
    };
    let enter_child = |parent: &Expr, child: &Expr| match parent {
        // A dynamic property key (`n[<expr>]`) is not descended into; the
        // static value side always is.
        Expr::Property(p) => std::ptr::eq(child, &p.expr),
        // Skip the bare distance-metric keyword argument of
        // `vector_distance`/`vector_norm` (see `distance_metric_arg_index`).
        Expr::FunctionCall(fc) => match distance_metric_arg_index(&fc.name) {
            Some(metric) => fc.args.get(metric).is_none_or(|a| !std::ptr::eq(child, a)),
            None => true,
        },
        Expr::Binary(_)
        | Expr::Comparison(_)
        | Expr::Unary(_)
        | Expr::List(_)
        | Expr::Paren(_)
        | Expr::Subscript(_)
        | Expr::In(_)
        | Expr::IsNull(_)
        | Expr::IsType(_)
        | Expr::IsNormalized(_)
        | Expr::StringMatch(_)
        | Expr::Case(_)
        | Expr::HasLabel(_)
        | Expr::Map(_) => true,
        // CIP-261: embeds are full expressions and must be walked.
        Expr::StringInterp(parts) => parts
            .iter()
            .any(|part| matches!(part, InterpPart::Interp(e) if std::ptr::eq(child, e.as_ref()))),
        // CIP-272: a map comprehension's source, key, and value expressions.
        Expr::MapComprehension(mc) => {
            std::ptr::eq(child, &mc.source_expr)
                || std::ptr::eq(child, &mc.key_expr)
                || std::ptr::eq(child, &mc.value_expr)
        }
        _ => false,
    };
    // Infallible here: the visitor never fails, and every caller runs after
    // the whole-tree depth cap, so the walker's own cap cannot fire on
    // these trees (the hand-rolled recursion would have overflowed instead).
    let result = walk_expr_with(expr, &mut visit, enter_child);
    debug_assert!(
        result.is_ok(),
        "collect_expr_variables walked a tree that already passed the depth cap"
    );
}

/// Bind every variable a pattern introduces into `bound`: each path's own
/// variable, then its elements' node/relationship variables. Shared by the
/// MATCH/CREATE/MERGE arms of the scope walkers in [`crate::validate`].
pub(crate) fn bind_pattern_vars(patterns: &[PatternPath], bound: &mut HashSet<String>) {
    for name in pattern_vars(patterns) {
        bound.insert(name.to_owned());
    }
}

/// Every variable a pattern introduces, in the canonical order: each path's
/// own variable first, then its elements' variables (quantified operands
/// and subpath variables included). The one enumeration both
/// [`bind_pattern_vars`] and call_scope's checked binder consume.
pub(crate) fn pattern_vars(patterns: &[PatternPath]) -> Vec<&str> {
    let mut out = Vec::new();
    for path in patterns {
        if let Some(v) = &path.variable {
            out.push(v.as_str());
        }
        for elem in &path.elements {
            for_each_element_var(elem, &mut |name| out.push(name));
        }
    }
    out
}

/// Check that every variable `expr` references is in `scope`; the first
/// offender raises the canonical `UndefinedVariable` error. The one
/// expression-scope check: [`crate::validate::expressions`] uses it at every
/// expression position and [`crate::call_scope`] uses it for its strict
/// Rule 4/5 clause checks.
pub(crate) fn check_expr_vars_defined(
    expr: &Expr,
    scope: &HashSet<String>,
) -> Result<(), EngineError> {
    let mut vars = HashSet::new();
    collect_expr_variables(expr, &mut vars);
    // Sorted so the reported offender is deterministic across builds (the
    // set's iteration order is not).
    let mut vars: Vec<String> = vars.into_iter().collect();
    vars.sort();
    for v in &vars {
        check_name_defined(v, scope)?;
    }
    Ok(())
}

/// Check one bare name's scope membership: the leaf of
/// [`check_expr_vars_defined`], for callers holding a bare name (a SET
/// item's target) rather than an expression to walk.
pub(crate) fn check_name_defined(name: &str, scope: &HashSet<String>) -> Result<(), EngineError> {
    if scope.contains(name) {
        return Ok(());
    }
    Err(EngineError::semantic(
        ErrorKind::UndefinedVariable,
        format!("variable '{name}' not defined"),
    ))
}

/// One embedded expression of a CIP-261 interpolated string literal
/// (`s'...{expr}...'`): yields each `Interp` part's expression, skipping
/// the inert `Text` parts. Shared by every validator that must look inside
/// an interpolation.
pub(crate) fn interp_embeds(parts: &[InterpPart]) -> impl Iterator<Item = &Expr> {
    parts.iter().filter_map(|p| match p {
        InterpPart::Interp(e) => Some(e.as_ref()),
        InterpPart::Text(_) => None,
    })
}

/// Every arm of a statement's UNION chain, with the `all` flag of the link
/// that attached it (`UNION ALL` vs plain `UNION`), in source order — the
/// head statement itself is not an arm. `link` is the head statement's own
/// `union` edge ([`crate::ast::StmtRef::union`] for a walked statement,
/// `Statement::union` for one in hand); `None` yields no arms. The one
/// canonical enumeration of a UNION chain for the validate battery, the
/// lowering's parts fold and [`crate::call_scope`].
pub(crate) fn union_arms<'a>(
    link: Option<&'a UnionPart>,
) -> impl Iterator<Item = (&'a Statement, bool)> + 'a {
    let mut link = link;
    std::iter::from_fn(move || {
        let part = link?;
        let arm = (&part.statement, part.all);
        link = part.statement.union.as_deref();
        Some(arm)
    })
}

/// One statement plus every statement of its `UNION` chain, in order: the
/// chain cursor spelled once for the walkers that must visit a whole
/// chain (`once(stmt)` then [`union_arms`]' own traversal).
pub(crate) fn union_chain(stmt: &Statement) -> impl Iterator<Item = &Statement> {
    std::iter::once(stmt).chain(union_arms(stmt.union.as_deref()).map(|(arm, _)| arm))
}

/// The column names a WITH/RETURN item list projects: a `*` item passes
/// the incoming scope through, every other item contributes its
/// [`effective_name`]. The one project-items-to-names skeleton: both
/// projection arms of [`crate::validate::output_vars`]'s
/// `local_stage_output_vars`, [`crate::validate::search`]'s WITH arm (a WITH
/// replaces the scope), and the WITH arm of [`clause_introduced_names`]
/// (introduction is exactly projection over an empty incoming scope).
pub(crate) fn projected_names(items: &[ReturnItem], bound: &HashSet<String>) -> HashSet<String> {
    let mut out = HashSet::new();
    for item in items {
        if item.all {
            out.extend(bound.iter().cloned());
            continue;
        }
        if let Some(name) = effective_name(item) {
            out.insert(name);
        }
    }
    out
}

/// Names a single clause *introduces* into its body's scope, as one set:
/// pattern variables (plus a SEARCH clause's SCORE variable) for
/// MATCH/CREATE/MERGE, the alias for UNWIND/LOAD CSV, the alias-resolved
/// YIELD names for a procedure call, and the catalog columns for the
/// SHOW/TERMINATE family. The canonical per-clause introduction fact: the
/// composed-SHOW `WHERE` check, [`crate::validate::search`]'s Rule 5 scope
/// walk, `call_scope`'s top-level accumulation and
/// `validate::expressions`'s catalog arm all derive from it. Consumers that
/// need *more* than introduction — a WITH replaces the scope (a projection,
/// [`crate::validate::search`]'s Rule 5 walk) and a `CALL { }` body's
/// exports depend on the enclosing scope — spell those arms themselves.
pub(crate) fn clause_introduced_names(clause: &Clause) -> HashSet<String> {
    let mut names = HashSet::new();
    match clause {
        Clause::Match(mc) => {
            bind_pattern_vars(&mc.pattern, &mut names);
            if let Some(sv) = search_score_var(mc) {
                names.insert(sv.to_string());
            }
        }
        Clause::With(wc) => {
            names = projected_names(&wc.items, &HashSet::new());
        }
        Clause::Unwind(uw) => {
            names.insert(uw.alias.clone());
        }
        Clause::LoadCSV(lc) => {
            names.insert(lc.alias.clone());
        }
        Clause::Create(cc) => {
            bind_pattern_vars(&cc.pattern, &mut names);
        }
        Clause::Merge(mc) => {
            bind_pattern_vars(std::slice::from_ref(&mc.pattern), &mut names);
        }
        Clause::CallProcedure(cp) => {
            names.extend(cp.yields.effective_names().map(str::to_owned));
        }
        clause if catalog_produces_rows(clause) => {
            names.extend(catalog_output_columns(clause));
        }
        _ => {}
    }
    names
}

#[cfg(test)]
mod tests {
    use super::super::tree::tests::return_expr_of;
    use super::*;
    use crate::agg::contains_aggregation;

    #[test]
    fn collect_expr_variables_matches_historical_reach() {
        // The walker-based collector must visit exactly the node set the
        // hand-rolled recursion did.
        let collected = |query: &str| {
            let e = return_expr_of(query);
            let mut vars = HashSet::new();
            collect_expr_variables(&e, &mut vars);
            vars
        };
        // Plain combinators are descended into.
        let vars = collected("RETURN a.prop + (b IN [c]) AND CASE WHEN d THEN e ELSE f END");
        for v in ["a", "b", "c", "d", "e", "f"] {
            assert!(vars.contains(v), "missing `{v}`");
        }
        // Scope-introducing constructs are NOT descended into at all — not
        // even their list expressions (historical semantics).
        let vars = collected("RETURN [x IN n.list | x + q]");
        assert!(vars.is_empty(), "comprehension leaked: {vars:?}");
        let vars = collected("RETURN reduce(s = i, y IN m.list | s + y)");
        assert!(vars.is_empty(), "reduce leaked: {vars:?}");
        // CIP-214: the bare distance-metric keyword is not a variable.
        let vars = collected("RETURN vector_distance(v1, v2, EUCLIDEAN)");
        assert!(vars.contains("v1") && vars.contains("v2"));
        assert!(!vars.contains("EUCLIDEAN"), "metric keyword collected");
        // A dynamic property KEY (`n[<expr>]`) is not descended into; the
        // value side is. (Only SET/REMOVE targets parse key_expr, so build
        // the node directly.)
        let e = Expr::Property(Box::new(cypherlite_parser::PropertyAccess {
            expr: Expr::Variable("n".to_string()),
            property: String::new(),
            key_expr: Some(Expr::Variable("k".to_string())),
        }));
        let mut vars = HashSet::new();
        collect_expr_variables(&e, &mut vars);
        assert!(vars.contains("n"));
        assert!(!vars.contains("k"), "dynamic property key collected");
    }

    #[test]
    fn contains_aggregation_matches_historical_reach() {
        // The walker-based check must look through exactly the combinators
        // the hand-rolled recursion did: function args, binary/unary/
        // comparison operands, parens, property values — and nothing else.
        assert!(contains_aggregation(&return_expr_of("RETURN count(*)")));
        assert!(contains_aggregation(&return_expr_of("RETURN 1 + count(*)")));
        assert!(contains_aggregation(&return_expr_of(
            "RETURN abs((sum(x)))"
        )));
        assert!(contains_aggregation(&return_expr_of(
            "RETURN n.p < max(m.q)"
        )));
        // Aggregates inside a list literal, CASE arm, IN operand or
        // comprehension are NOT "contained" (historical verdict); a
        // subscript/slice *index or bound* is not walked either, but the
        // postfix *base* is.
        assert!(!contains_aggregation(&return_expr_of("RETURN [count(*)]")));
        assert!(!contains_aggregation(&return_expr_of(
            "RETURN CASE WHEN true THEN count(*) ELSE 0 END"
        )));
        assert!(!contains_aggregation(&return_expr_of("RETURN n[count(*)]")));
        assert!(!contains_aggregation(&return_expr_of(
            "RETURN 1 IN [count(*)]"
        )));
        assert!(!contains_aggregation(&return_expr_of(
            "RETURN [x IN [1] | count(*)]"
        )));
        // Aggregates in the postfix base of a subscript or slice ARE
        // "contained" (the walker descends into the base, not the
        // index/bounds).
        assert!(contains_aggregation(&return_expr_of(
            "RETURN collect(x)[..1]"
        )));
        assert!(contains_aggregation(&return_expr_of(
            "RETURN collect(x)[0]"
        )));
        // Nor one hiding in a dynamic property key.
        let e = Expr::Property(Box::new(cypherlite_parser::PropertyAccess {
            expr: Expr::Variable("n".to_string()),
            property: String::new(),
            key_expr: Some(Expr::CountStar),
        }));
        assert!(!contains_aggregation(&e));
    }
}
