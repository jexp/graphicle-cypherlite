//! Lower CypherLite `Statement` into the planner-IR `Query` (TCK subset + CIP waves).

use cypherlite_core::{Error, Result};
use cypherlite_ir as mdb;
use cypherlite_parser as cl;

pub fn lower_statement(stmt: &cl::Statement) -> Result<mdb::Query> {
    // Validation runs once, up front, as a single walk over the whole
    // statement tree (LABS-70/LABS-81):
    //
    // 1. `validate_statement_tree` heads its per-statement battery with the
    //    ADR-0257 nesting-depth guard (statement/structural nesting is
    //    capped at the lowering boundary; the parser caps expression
    //    nesting), so an over-nested query is a typed error here, never a
    //    stack-overflow abort, and applies every scope-free validator to
    //    the top-level statement *and* every nested body (UNION arms, NEXT
    //    stages, `CALL { }` / `WHEN` / `ELSE` bodies, FOREACH bodies, and
    //    `EXISTS`/`COUNT`/`COLLECT` subquery bodies), so a rule violation
    //    cannot hide inside nesting.
    crate::validate::validate_statement_tree(stmt)
        .map_err(|error| Error::syntax(error.to_string()))?;
    // 2. The scope-sensitive validators -- they track bindings across the
    //    clauses of one linear statement body, which a nested body cannot do
    //    soundly without its enclosing (correlated) scope -- and the
    //    self-recursing CIP-150 dialect gates run on the top-level
    //    statement; each covers nested bodies in its own way (FOREACH bodies
    //    share the enclosing scope and are reached by
    //    `validate_variable_types`'s own recursion).
    crate::validate::validate_show_column_names(stmt)
        .map_err(|error| Error::syntax(error.to_string()))?;
    for validation in [
        crate::call_scope::validate_call_scope_clauses(stmt),
        crate::validate::validate_show_composability(stmt),
        crate::validate::validate_show_current_graph_type_requires_yield(stmt),
        crate::validate::validate_variable_types(stmt),
        crate::validate::validate_aggregation_semantics(stmt),
        crate::validate::validate_expression_constraints(stmt),
        crate::validate::validate_type_on_nodes(stmt),
        crate::validate::validate_cypher5_restrictions(stmt),
        crate::validate::validate_no_apoc_coll_sort(stmt),
    ] {
        if let Err(error) = validation {
            return Err(Error::syntax(error.to_string()));
        }
    }
    // The CIP-240 static-predicate (inner-WHERE) filter shape check lives
    // in exactly one place: `search_filter`, the lowering boundary's own
    // grammar walk over the inner `WHERE` of every index specification of
    // every (possibly nested, CIP-278) source tree (the CIP-224 SEARCH
    // rules themselves run in the battery above, via
    // `validate_search_clauses`).
    lower_statement_impl(stmt)
}

fn lower_statement_impl(stmt: &cl::Statement) -> Result<mdb::Query> {
    lower_statement_with_inherited_version(stmt, None)
}

/// A `CYPHER <version>` preamble scopes over the whole statement, including
/// every stage of a CIP-213 `NEXT` chain (whose own parsed statements carry
/// no preamble). Each stage's effective version is its own preamble, else
/// the nearest enclosing stage's — threaded through the single lowering
/// walk over the chain rather than backfilled by a second pass afterwards.
/// Nested statement bodies (`CALL { }`, `WHEN`/`ELSE`, FOREACH, and the
/// expression-embedded subqueries) enter through `lower_statement_impl`
/// and keep `None` here: they observe the enclosing version at runtime
/// through the executor's `QueryCtx`, not in their lowered IR.
fn lower_statement_with_inherited_version(
    stmt: &cl::Statement,
    inherited_version: Option<u32>,
) -> Result<mdb::Query> {
    // `EXPLAIN`/`PROFILE` preambles (`stmt.explain`/`stmt.profile`) are not
    // lowered: they ride out of band on `LoweredStatement`
    // (`crate::parse_and_lower_statement`) and are honored by the connection
    // layer, which renders the plan without executing (EXPLAIN) or with
    // statistics (PROFILE). Lowering always produces the wrapped statement's
    // real query.
    let cypher_version = stmt.cypher_version.or(inherited_version);
    let mut parts = vec![mdb::SingleQuery {
        clauses: lower_clauses(&stmt.clauses)?,
    }];
    let mut union_all = Vec::new();
    for (arm, all) in crate::ast::collect::union_arms(stmt.union.as_deref()) {
        union_all.push(all);
        parts.push(mdb::SingleQuery {
            clauses: lower_clauses(&arm.clauses)?,
        });
    }
    Ok(mdb::Query {
        parts,
        union_all,
        next: lower_boxed(stmt.next.as_deref(), |next| {
            lower_statement_with_inherited_version(next, cypher_version)
        })?,
        cypher_version,
    })
}

/// Lower an optional boxed value, keeping the optional-boxed shape —
/// one spelling of the `.as_ref().map(f).transpose()?.map(Box::new)`
/// chain the lower module's optional-boxed lowering sites share.
fn lower_boxed<'a, T, U>(
    opt: Option<&'a T>,
    lower: impl FnOnce(&'a T) -> Result<U>,
) -> Result<Option<Box<U>>> {
    opt.map(|inner| lower(inner).map(Box::new)).transpose()
}

fn nyi(what: &str) -> Error {
    Error::semantic(format!("not implemented: {what}"))
}

fn lower_clauses(clauses: &[cl::Clause]) -> Result<Vec<mdb::Clause>> {
    let mut out = Vec::with_capacity(clauses.len());
    for c in clauses {
        if let cl::Clause::StandaloneOrderBy(page) = c {
            for stage in &page.stages {
                let (order, skip, limit) = match stage {
                    cl::PageStage::OrderBy(items) => (lower_order(items)?, None, None),
                    cl::PageStage::Skip(expr) => (Vec::new(), Some(lower_expr(expr)?), None),
                    cl::PageStage::Limit(expr) => (Vec::new(), None, Some(lower_expr(expr)?)),
                };
                out.push(mdb::Clause::With {
                    distinct: false,
                    group_by: None,
                    items: vec![mdb::Projection {
                        expr: mdb::Expr::Star,
                        alias: None,
                        source_text: None,
                    }],
                    order,
                    skip,
                    limit,
                    where_clause: None,
                    origin: mdb::WithOrigin::Explicit,
                });
            }
            continue;
        }
        out.push(lower_clause(c)?);
    }
    Ok(out)
}

/// Lower a CIP-236 `<group by clause>`: explicit keys replace the
/// inference from non-aggregating items (that is the *only* semantic
/// difference). A grouping element that is a bare variable matching a
/// projection alias resolves to that item's expression (Rule 4.2); `()`
/// lowers to the empty key set (global aggregation) and `ALL` to the
/// non-aggregating items (which equals inference).
fn lower_group_by(
    spec: &Option<cl::GroupBySpec>,
    items: &[cl::ReturnItem],
) -> Result<Option<Vec<mdb::Expr>>> {
    let Some(spec) = spec else { return Ok(None) };
    Ok(Some(match spec {
        cl::GroupBySpec::Empty => Vec::new(),
        cl::GroupBySpec::All => items
            .iter()
            .filter(|i| !crate::agg::contains_aggregation(&i.expr))
            .map(|i| lower_expr(&i.expr))
            .collect::<Result<Vec<_>>>()?,
        cl::GroupBySpec::Keys(keys) => {
            let mut out = Vec::with_capacity(keys.len());
            for k in keys {
                out.push(match k {
                    // Alias reference: resolve to the aliased item's
                    // expression (Rule 4.2).
                    cl::Expr::Variable(name) => items
                        .iter()
                        .find(|i| i.alias.as_deref() == Some(name.as_str()))
                        .map(|i| lower_expr(&i.expr))
                        .transpose()?
                        .unwrap_or_else(|| mdb::Expr::Variable(name.clone())),
                    other => lower_expr(other)?,
                });
            }
            out
        }
    }))
}

mod clause;
mod expr;
mod pattern;
mod search_filter;

use clause::*;
pub use expr::{lower_expr, lower_type_expr};
use pattern::*;

#[cfg(test)]
mod tests {
    use super::*;
    use cypherlite_test_support::run_with_big_stack;

    fn lower(query: &str) -> mdb::Query {
        lower_statement(&cl::parse(query).unwrap()).unwrap()
    }

    fn lower_err(query: &str) -> String {
        lower_statement(&cl::parse(query).unwrap())
            .unwrap_err()
            .to_string()
    }

    /// The ADR-0257 depth error text, derived from the single-sourced cap
    /// (`cypherlite_parser::MAX_NESTING_DEPTH` via `validate`).
    fn depth_cap_message() -> String {
        format!(
            "maximum nesting depth of {} exceeded",
            crate::validate::MAX_NESTING_DEPTH
        )
    }

    #[test]
    fn lowers_cypher_version_preamble_regardless_of_whitespace() {
        // The preamble is tokenized, not textual: space, newline, and tab
        // between `CYPHER 5` and the statement body all lower to the same
        // typed `cypher_version`.
        for src in [
            "CYPHER 5 RETURN 1 AS n",
            "CYPHER 5\nRETURN 1 AS n",
            "CYPHER 5\tRETURN 1 AS n",
        ] {
            assert_eq!(lower(src).cypher_version, Some(5), "for {src:?}");
        }
        assert_eq!(lower("CYPHER 25 RETURN 1 AS n").cypher_version, Some(25));
        assert_eq!(lower("RETURN 1 AS n").cypher_version, None);
    }

    #[test]
    fn next_stages_inherit_cypher_version() {
        let query = lower("CYPHER 5 RETURN 1 AS n NEXT RETURN 2 AS m");
        assert_eq!(query.cypher_version, Some(5));
        let next = query.next.as_deref().expect("NEXT stage");
        assert_eq!(next.cypher_version, Some(5));
    }

    #[test]
    fn subquery_bodies_do_not_carry_the_preamble() {
        // Nested bodies observe the enclosing statement's version through the
        // executor's QueryCtx at runtime; the lowered IR itself stays `None`.
        let query = lower("CYPHER 5 CALL { RETURN 1 AS x } RETURN x");
        let mdb::Clause::CallSubquery { query: inner, .. } = &query.parts[0].clauses[0] else {
            panic!("expected CALL subquery")
        };
        assert_eq!(inner.cypher_version, None);
    }

    #[test]
    fn preserves_dynamic_label_and_property_ir() {
        let query = lower(
            "MATCH (n:$any($labels)) WHERE n IS LABELED (A|!B) \
             SET n[$key] = 1, n:$(toString($label)) REMOVE n[$old], n:$($gone) RETURN n[$key]",
        );
        let clauses = &query.parts[0].clauses;
        let mdb::Clause::Match {
            patterns,
            where_clause,
            ..
        } = &clauses[0]
        else {
            panic!("expected MATCH")
        };
        assert!(matches!(
            patterns[0].nodes[0].label_expr,
            Some(mdb::LabelExpr::Dynamic {
                quantifier: mdb::DynamicLabelQuantifier::Any,
                ..
            })
        ));
        assert!(matches!(
            where_clause,
            Some(mdb::Expr::LabelPredicate {
                label_expr: mdb::LabelExpr::Or(_),
                strict: true,
                ..
            })
        ));
        let mdb::Clause::Set { items } = &clauses[1] else {
            panic!("expected SET")
        };
        assert!(matches!(
            items[0],
            mdb::SetItem::Property {
                target: mdb::Expr::DynamicProperty { .. },
                ..
            }
        ));
        assert!(matches!(
            items[1],
            mdb::SetItem::Labels { ref labels, .. }
                if matches!(labels.as_slice(), [mdb::LabelExpr::Dynamic { .. }])
        ));
        let mdb::Clause::Remove { items } = &clauses[2] else {
            panic!("expected REMOVE")
        };
        assert!(matches!(
            items[0],
            mdb::RemoveItem::Property {
                target: mdb::Expr::DynamicProperty { .. }
            }
        ));
    }

    #[test]
    fn preserves_simple_case_label_predicate_marker() {
        let query = lower("MATCH (n) RETURN CASE n WHEN IS LABELED A THEN 1 ELSE 0 END AS result");
        let mdb::Clause::Return { items, .. } = &query.parts[0].clauses[1] else {
            panic!("expected RETURN")
        };
        let mdb::Expr::Case { when_then, .. } = &items[0].expr else {
            panic!("expected CASE")
        };
        assert!(when_then[0].2);
    }

    #[test]
    fn preserves_gpm_modes_selectors_qpp_and_inline_predicates() {
        let query = lower(
            "MATCH REPEATABLE ELEMENTS p = SHORTEST 2 WALK GROUPS \
             (s WHERE s.active)-[lead:LEAD WHERE lead.ok]->\
             ((a)-[r:R]->(b) WHERE r.weight > 0){1,3}(t) RETURN p, a, r",
        );
        let mdb::Clause::Match {
            match_mode,
            patterns,
            ..
        } = &query.parts[0].clauses[0]
        else {
            panic!("expected MATCH")
        };
        assert_eq!(*match_mode, mdb::MatchMode::RepeatableElements);
        let path = &patterns[0];
        assert_eq!(path.mode, Some(mdb::PathMode::Walk));
        assert!(matches!(
            path.selector,
            Some(mdb::PathSelector::Shortest {
                count: Some(mdb::PathCount::Literal(2)),
                group: true,
            })
        ));
        assert!(matches!(
            &path.elements[0],
            mdb::PatternElement::Node(mdb::NodePattern {
                where_clause: Some(_),
                ..
            })
        ));
        assert!(matches!(
            &path.elements[1],
            mdb::PatternElement::Relationship(mdb::RelPattern {
                where_clause: Some(_),
                ..
            })
        ));
        let qpp = path
            .elements
            .iter()
            .find_map(|element| match element {
                mdb::PatternElement::Quantified(qpp) => Some(qpp),
                _ => None,
            })
            .expect("quantified path element");
        assert_eq!((qpp.min, qpp.max), (1, Some(3)));
        assert!(qpp.where_clause.is_some());
        assert!(path.nodes.is_empty(), "QPP must not be flattened away");
    }

    // ---- LABS-70/LABS-81: nested-statement validation & depth cap ----

    /// A nested `CALL { }` body one level down from `stmt`, built by hand so
    /// the depth tests exercise the front-end guard directly (the parser
    /// applies its own accounting before these ever reach lowering).
    fn wrap_in_call(stmt: cl::Statement) -> cl::Statement {
        cl::Statement {
            clauses: vec![cl::Clause::CallSubquery(cl::CallSubqueryClause {
                statement: stmt,
                scope: cl::CallScope::None,
                optional: false,
                in_transactions: None,
            })],
            ..cl::Statement::default()
        }
    }

    fn return_one_statement() -> cl::Statement {
        cl::Statement {
            clauses: vec![cl::Clause::Return(cl::ReturnClause {
                group_by: None,
                distinct: false,
                items: vec![cl::ReturnItem {
                    expr: cl::Expr::Literal(cl::LiteralValue::Integer(1)),
                    alias: None,
                    all: false,
                    source_text: None,
                }],
                order_by: vec![],
                skip: None,
                limit: None,
            })],
            ..cl::Statement::default()
        }
    }

    /// `RETURN 1` inside `depth` nested `CALL { }` bodies: the innermost body
    /// sits at nesting depth `depth`.
    fn nested_call_statement(depth: usize) -> cl::Statement {
        let mut stmt = return_one_statement();
        for _ in 0..depth {
            stmt = wrap_in_call(stmt);
        }
        stmt
    }

    /// `RETURN 1` with `arms` UNION links chained onto it: the last arm sits
    /// at nesting depth `arms`.
    fn union_chain_statement(arms: usize) -> cl::Statement {
        let mut stmt = return_one_statement();
        for _ in 0..arms {
            stmt = cl::Statement {
                clauses: return_one_statement().clauses,
                union: Some(Box::new(cl::UnionPart {
                    all: false,
                    statement: stmt,
                })),
                ..cl::Statement::default()
            };
        }
        stmt
    }

    #[test]
    fn nested_call_subquery_limit_is_rejected_like_top_level() {
        // LABS-81: the top-level equivalent is already rejected; the nested
        // forms must fail with the same error class.
        let top = lower_err("RETURN 1 LIMIT -1");
        assert!(top.contains("NegativeIntegerArgument"), "{top}");
        for query in [
            "CALL { RETURN 1 LIMIT -1 } RETURN 1",
            "CALL { CALL { RETURN 1 LIMIT -1 } RETURN 1 } RETURN 1",
            // A UNION arm is a nested statement too.
            "RETURN 1 AS a UNION RETURN 2 AS a LIMIT -1",
        ] {
            let err = lower_err(query);
            assert!(
                err.contains("NegativeIntegerArgument"),
                "wrong error for {query}: {err}"
            );
        }
    }

    #[test]
    fn conditional_and_foreach_bodies_are_validated_like_top_level() {
        // CIP-135 `WHEN ... THEN { ... }` branch bodies are full statements.
        let err = lower_err("WHEN true THEN { RETURN 1 AS x LIMIT -1 } ELSE { RETURN 2 AS x }");
        assert!(err.contains("NegativeIntegerArgument"), "{err}");
        // FOREACH bodies share the enclosing scope and take the write rules.
        let err = lower_err("FOREACH (x IN [1] | CREATE (a)-[:R]-(b))");
        assert!(err.contains("RequiresDirectedRelationship"), "{err}");
        let err = lower_err("FOREACH (x IN [1] | MERGE (n $props))");
        assert!(err.contains("InvalidParameterUse"), "{err}");
    }

    #[test]
    fn param_property_shorthand_inside_subquery_expressions_is_rejected() {
        // LABS-81: `$props` shorthand is CREATE-only; a subquery expression
        // body is not a hiding place for it.
        let top = lower_err("MATCH (m $props) RETURN m");
        assert!(top.contains("InvalidParameterUse"), "{top}");
        for query in [
            "MATCH (n) WHERE EXISTS { MATCH (m $props) } RETURN n",
            "RETURN count { MATCH (m $props) } AS c",
            "RETURN collect { MATCH (m $props) RETURN m } AS c",
        ] {
            let err = lower_err(query);
            assert!(
                err.contains("InvalidParameterUse"),
                "wrong error for {query}: {err}"
            );
        }
    }

    #[test]
    fn valid_nested_statements_still_lower() {
        for query in [
            "CALL { RETURN 1 } RETURN 1",
            "CALL { CALL { RETURN 1 AS x } RETURN x } RETURN x",
            "MATCH (n) WHERE EXISTS { MATCH (n)-->(m) } RETURN n",
            "RETURN count { MATCH (n) } AS c",
            "FOREACH (x IN [1] | CREATE (:L))",
            "WITH 1 AS a CALL { WITH a RETURN a AS b } RETURN a, b",
            "WHEN true THEN { RETURN 1 AS x } ELSE { RETURN 2 AS x }",
            "RETURN 1 AS x UNION RETURN 2 AS x",
        ] {
            lower(query);
        }
    }

    #[test]
    fn statement_nesting_at_the_limit_lowers() {
        // Exactly the ADR-0257 cap: accepted. Run on a big-stack thread (the
        // same pattern as the parser's
        // `test_parse_trim_deeply_nested_is_not_exponential`): 500 levels of
        // lowering/validation recursion overflow the default 2 MiB
        // test-thread stack in debug builds.
        for stmt in [nested_call_statement(500), union_chain_statement(500)] {
            run_with_big_stack(move || {
                lower_statement(&stmt).unwrap();
            });
        }
    }

    #[test]
    fn statement_nesting_past_the_limit_is_a_typed_error() {
        // Limit + 1: a typed error naming the limit, no abort. Big-stack
        // thread as above: the depth guard early-exits at the limit + 1, but
        // even those ~500 levels of walker recursion overflow the default
        // 2 MiB test-thread stack in debug builds.
        let mut next_chain = return_one_statement();
        for _ in 0..501 {
            next_chain = cl::Statement {
                next: Some(Box::new(next_chain)),
                ..cl::Statement::default()
            };
        }
        for stmt in [
            nested_call_statement(501),
            union_chain_statement(501),
            next_chain,
        ] {
            run_with_big_stack(move || {
                let err = lower_statement(&stmt).unwrap_err().to_string();
                assert!(err.contains(&depth_cap_message()), "wrong error: {err}");
            });
        }
    }

    // ---- Per-hop chains & pattern-embedded subqueries ----

    /// `WITH {a:null} AS n RETURN n` + `.a` x `hops`: a postfix property
    /// chain the parser builds iteratively (counting no nesting depth).
    fn property_chain_query(hops: usize) -> String {
        let mut query = String::from("WITH {a:null} AS n RETURN n");
        for _ in 0..hops {
            query.push_str(".a");
        }
        query
    }

    #[test]
    fn property_chain_past_the_limit_is_a_typed_error_end_to_end() {
        // Verified pre-fix: this 16000-hop chain overflowed the main-thread
        // stack (SIGABRT in ~0.1s on an 8 MiB release stack; boundary
        // ~2000 hops) because neither the parser's iterative postfix loop
        // nor the front-end walker counted the hops, so `lower_expr`
        // recursed one native frame per hop. The depth guard must now
        // reject it with a typed error before lowering recurses. Big-stack
        // thread as in the ADR-0257 tests above.
        run_with_big_stack(|| {
            let err = crate::parse_and_lower(&property_chain_query(16_000))
                .unwrap_err()
                .to_string();
            assert!(err.contains(&depth_cap_message()), "wrong error: {err}");
        });
    }

    #[test]
    fn property_chain_just_under_the_limit_lowers() {
        // 450 hops stays under the cap and must lower cleanly. Execution
        // end-to-end on an 8 MiB release stack was probed via the
        // `cypherlite` CLI (`WITH {a:null} AS n RETURN n` + `.a` x 450
        // evaluates to `null`, no abort); see this node's commit message.
        run_with_big_stack(|| {
            lower(&property_chain_query(450));
        });
    }

    #[test]
    fn param_property_shorthand_in_pattern_embedded_subqueries_is_rejected() {
        // `$props` shorthand is CREATE-only; a pattern comprehension's,
        // pattern predicate's, or shortestPath()'s embedded pattern is not
        // a hiding place for it (both live-exec repros verified executing
        // through `Database::query` pre-fix).
        let top = lower_err("MATCH (m $props) RETURN m");
        assert!(top.contains("InvalidParameterUse"), "{top}");
        for query in [
            "RETURN [(n {p: count { MATCH (m $props) }})-->(m) | m] AS x",
            "MATCH (n) WHERE (n {p: count { MATCH (m $props) RETURN m }})-->() RETURN n",
            "RETURN shortestPath((n {p: count { MATCH (m $props) }})-->(m)) AS p",
        ] {
            let err = lower_err(query);
            assert!(
                err.contains("InvalidParameterUse"),
                "wrong error for {query}: {err}"
            );
        }
    }

    #[test]
    fn limit_inside_pattern_embedded_subquery_is_a_validation_error() {
        // `lower_err` never executes anything: a `LIMIT -1` surfaced here
        // is a *validation* error, not the runtime executor backstop that
        // caught the pattern-predicate form pre-fix.
        let top = lower_err("RETURN 1 LIMIT -1");
        assert!(top.contains("NegativeIntegerArgument"), "{top}");
        for query in [
            "MATCH (n) WHERE (n {p: count { RETURN 1 LIMIT -1 }})-->() RETURN n",
            "RETURN [(n {p: count { RETURN 1 LIMIT -1 }})-->(m) | m] AS x",
            "RETURN shortestPath((n {p: count { RETURN 1 LIMIT -1 }})-->(m)) AS p",
        ] {
            let err = lower_err(query);
            assert!(
                err.contains("NegativeIntegerArgument"),
                "wrong error for {query}: {err}"
            );
        }
    }

    #[test]
    fn valid_pattern_embedded_expressions_still_lower() {
        // Walking pattern-embedded expressions must not reject legitimate
        // ones.
        for query in [
            "MATCH (n) RETURN [(n)-->(m) | m] AS x",
            "RETURN [(n {p: 1})-->(m) | m] AS x",
            "MATCH (n) WHERE (n)-->() RETURN n",
            "MATCH (n) WHERE (n {p: 1})-->() RETURN n",
        ] {
            lower(query);
        }
    }

    #[test]
    fn deeply_nested_call_chain_is_a_typed_error_end_to_end() {
        // Through the full parse + lower path: a typed error, never an abort
        // (LABS-70 verified `RETURN NOT NOT ... x1000 true` aborted the
        // production binary; statement nesting takes the same recursion
        // paths).
        run_with_big_stack(|| {
            let mut query = String::new();
            for _ in 0..600 {
                query.push_str("CALL { ");
            }
            query.push_str("RETURN 1 AS x");
            for _ in 0..600 {
                query.push_str(" }");
            }
            let err = crate::parse_and_lower(&query).unwrap_err().to_string();
            assert!(err.contains(&depth_cap_message()), "wrong error: {err}");
        });
    }

    // ---- Cross-statement composed expression depth ----

    /// `RETURN collect { <inner> }[0..]×hops AS x` wrapped `levels` deep
    /// around `RETURN [1] AS x`: expression-embedded subqueries whose
    /// per-statement postfix chains compose on one native stack, since
    /// `lower_expr` lowers each `collect { }` body from inside its own
    /// recursion.
    fn collect_chain_composition_query(levels: usize, hops: usize) -> String {
        let mut query = String::from("RETURN [1] AS x");
        for _ in 0..levels {
            let mut wrapped = String::from("RETURN collect { ");
            wrapped.push_str(&query);
            wrapped.push_str(" }");
            for _ in 0..hops {
                wrapped.push_str("[0..]");
            }
            wrapped.push_str(" AS x");
            query = wrapped;
        }
        query
    }

    #[test]
    fn collect_chain_composition_past_the_limit_is_a_typed_error_end_to_end() {
        // 3 nested `collect { }` levels × 450 slice hops each (~6.8 KB).
        // Each per-statement chain is under the cap on its own, but the
        // bodies lower from inside `lower_expr`'s recursion, so the composed
        // ~1350 frames nest on one native stack — pre-fix this passed the
        // depth guard (the count reset at each subquery boundary) and
        // SIGABRT'd inside `parse_and_lower` on an 8 MiB release stack. With
        // cumulative accounting it is a typed depth error, never an abort.
        // Big-stack thread as in the ADR-0257 tests above.
        run_with_big_stack(|| {
            let err = crate::parse_and_lower(&collect_chain_composition_query(3, 450))
                .unwrap_err()
                .to_string();
            assert!(err.contains(&depth_cap_message()), "wrong error: {err}");
        });
    }

    #[test]
    fn deep_collect_chain_composition_errors_without_aborting() {
        // 50 levels × 450 hops: the guard early-exits at the first over-cap
        // composition (mid level 2), so even a deeply composed query returns
        // the typed error instead of aborting. Big-stack thread as above.
        run_with_big_stack(|| {
            let err = crate::parse_and_lower(&collect_chain_composition_query(50, 450))
                .unwrap_err()
                .to_string();
            assert!(err.contains(&depth_cap_message()), "wrong error: {err}");
        });
    }

    #[test]
    fn collect_chain_composition_just_under_the_limit_lowers() {
        // 3 levels × 150 hops composes to ~304 levels of expression depth —
        // under the 500 cap — and must lower cleanly. Big-stack thread as
        // above (a few hundred walker/lowering recursion levels overflow the
        // default 2 MiB test-thread stack in debug builds).
        run_with_big_stack(|| {
            lower(&collect_chain_composition_query(3, 150));
        });
    }
}
