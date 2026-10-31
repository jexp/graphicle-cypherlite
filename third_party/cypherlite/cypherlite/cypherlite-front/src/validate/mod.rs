//! Semantic validation passes for Cypher statements, run by
//! [`crate::lower::lower_statement`] before lowering.
//!
//! The family is split by concern:
//!
//! - [`show`]: `SHOW …`/`TERMINATE …` column and composability rules
//!   (CIP-42/46/84/123/157/187/242/253).
//! - [`scope`]: cross-clause variable binding and usage-type tracking
//!   (`VariableAlreadyBound` / `VariableTypeConflict`, keyed on the *reuse*
//!   usage, matching the openCypher TCK).
//! - [`search`]: the CIP-224 SEARCH-clause rules that don't fit a generic
//!   scope tracker (binding-variable reference, SCORE conflicts, pattern
//!   shape, per-index CIP-225/240 kind restrictions).
//! - [`output_vars`]: the subquery output-variable analysis (what a
//!   (sub)query's terminal projection exports), shared with
//!   [`crate::call_scope`].
//! - [`writes`]: CREATE/MERGE pattern legality (single relationship type,
//!   direction, no variable length, no QPPs, parameter property maps,
//!   CIP-156 dynamic label rules).
//! - [`expressions`]: expression-position rules (pattern predicates in
//!   WHERE, patterns in projections/SET values, property access on paths,
//!   `labels()`/`type()` entity argument rules, DELETE targets).
//! - [`aggregation`]: aggregation semantics (nesting, non-constant
//!   arguments, grouping-key ambiguity, ORDER BY aggregation rules,
//!   aggregation in list comprehensions).
//! - [`functions`]: the canonical known-function registry
//!   (`KNOWN_FUNCTIONS`, `is_known_function`) the planner binds against.
//! - [`static_types`]: compile-time checks decidable from *literal* operand
//!   types (boolean-operator operands, `IN` lists, `properties()`,
//!   quantifier/arithmetic mismatches, LIMIT/SKIP literals).
//! - [`dialect`]: `CYPHER <version>` preamble gates (CIP-150): the
//!   Cypher-25-only surface rejected under `CYPHER 5`, and `apoc.coll.sort`
//!   rejected under Cypher 25.
//! - [`structure`]: clause-composition rules (USE placement, NEXT chains,
//!   UNION column shapes, duplicate column names, relationship
//!   uniqueness).
//!
//! This module's root holds the ADR-0257 nesting-depth guard
//! ([`validate_nesting_depth`]), the nested-statement validation battery
//! ([`validate_statement_tree`]), the CIP-150 preview-gate predicates, and
//! the two canonical error constructors every pass shares
//! ([`duplicate_variable`], [`duplicate_outer_variable`]). The generic AST
//! helpers those passes build on — the collectors, the per-clause
//! scope-introduction fold ([`crate::ast::collect::clause_introduced_names`]),
//! the UNION-chain enumeration and the walker family — live in
//! [`crate::ast`], which this module consumes downward.

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{Clause, Expr, SearchSpec, Statement};

mod aggregation;
mod dialect;
mod expressions;
mod functions;
mod graph_type_gate;
mod output_vars;
mod scope;
mod search;
mod show;
mod static_types;
mod structure;
mod writes;

pub use self::aggregation::{
    validate_aggregation_semantics, validate_list_comprehension_aggregation,
    validate_order_by_aggregation,
};
pub use self::dialect::{validate_cypher5_restrictions, validate_no_apoc_coll_sort};
pub use self::expressions::{validate_expression_constraints, validate_type_on_nodes};
pub use self::functions::{is_known_function, KNOWN_FUNCTIONS, UNADVERTISED_FUNCTIONS};
pub use self::scope::validate_variable_types;
pub use self::search::validate_search_clauses;
pub use self::show::{
    validate_show_column_names, validate_show_composability,
    validate_show_current_graph_type_requires_yield, validate_show_output_uniqueness,
};
pub use self::static_types::{validate_limit_skip, validate_static_operator_types};
pub use self::structure::{
    validate_column_name_conflicts, validate_count_subquery_union_shapes, validate_foreach_body,
    validate_next_no_admin_commands, validate_relationship_uniqueness, validate_union_columns,
    validate_use_placement,
};
pub use self::writes::validate_param_property_shorthand;

pub(crate) use self::output_vars::compute_subquery_output_vars;

/// The canonical "variable already declared" diagnostic: one construction
/// site for every duplicate-binding and shadow check.
pub(crate) fn duplicate_variable(name: &str) -> EngineError {
    EngineError::semantic(
        ErrorKind::VariableAlreadyBound,
        format!("variable '{name}' already declared"),
    )
}

/// The one construction site for an outer-scope collision: a subquery
/// output (or CALL export) re-declares a name the enclosing scope already
/// binds. Neo4j's verbatim text names the offender (CIP-218 interactions
/// [1]-[3]); every outer-scope guard routes through here so the diagnostic
/// cannot drift into unnamed variants.
pub(crate) fn duplicate_outer_variable(name: &str) -> EngineError {
    EngineError::semantic(
        ErrorKind::VariableAlreadyBound,
        format!("Variable `{name}` already declared in outer scope"),
    )
}

/// The CIP graph-type trio's ADR-0371 preview-gate predicates (CIP-283
/// optional properties, CIP-262 default values, CIP-266 implied labels
/// identifying node element types): the AST-level half of the gates,
/// consulted by `parse_and_lower_statement_with_preview`. The
/// stored-state half (an `ADD` whose new identifying label a stored node
/// element type already implies) fails closed in the executor's
/// graph-type layer with the same authored error.
pub(crate) use graph_type_gate::statement_graph_type_gates;

/// Whether the statement's AST names a CIP-278 `COMBINE` source anywhere
/// (a MATCH search clause, at any subquery depth) -- the predicate the
/// ADR-0371 `hybrid-search` preview gate checks before lowering. A walk
/// error propagates: the gate never answers "no" over a tree it could not
/// walk.
pub(crate) fn statement_uses_hybrid_search(stmt: &Statement) -> Result<bool, EngineError> {
    let mut uses = false;
    walk_statement_tree(
        stmt,
        &mut |_| Ok(()),
        &mut |clause| {
            if let Clause::Match(m) = clause {
                if let Some(search) = &m.search {
                    if matches!(search.spec, SearchSpec::Combine(_)) {
                        uses = true;
                    }
                }
            }
            Ok(())
        },
        &mut |_| Ok(()),
    )?;
    Ok(uses)
}

/// Whether the statement's AST spells a CIP-250 `FOR <variable> IN
/// <expression>` clause anywhere (at any subquery depth) -- the predicate
/// the ADR-0371 `gql-for` preview gate checks before anything of the
/// statement validates or lowers. The parser normalizes the spelling into
/// a plain UNWIND clause carrying `for_syntax: true` (CIP-250's own
/// normalization rule: identical validation, planning, and execution), so
/// the detector is the one consumer of the flag -- nothing downstream
/// needs to know the spelling existed. A walk error propagates: the gate
/// never answers "no" over a tree it could not walk.
pub(crate) fn statement_uses_gql_for(stmt: &Statement) -> Result<bool, EngineError> {
    let mut uses = false;
    walk_statement_tree(
        stmt,
        &mut |_| Ok(()),
        &mut |clause| {
            if let Clause::Unwind(u) = clause {
                if u.for_syntax {
                    uses = true;
                }
            }
            Ok(())
        },
        &mut |_| Ok(()),
    )?;
    Ok(uses)
}

// ---- ADR-0257 nesting-depth guard and whole-tree validation ----

use crate::ast::stmt_ref::StmtRef;
use crate::ast::tree::walk_statement_tree;
/// ADR-0257: maximum statement/structural nesting depth accepted at the
/// lowering boundary. Every nested statement counts one level: `CALL { }`
/// bodies, CIP-135 `WHEN`/`ELSE` bodies, FOREACH bodies,
/// `EXISTS`/`COUNT`/`COLLECT` subquery bodies, and each link of a UNION or
/// NEXT chain. Expression nesting counts one level per expression node
/// along a walk (including per-hop postfix forms: `.key` property links,
/// subscripts/slices, map projections), and the expression count is
/// *cumulative* across expression-embedded subquery statement boundaries
/// (see `crate::ast::tree::walk_statement_tree_at`), so composed nesting — statement
/// depth combined with per-statement expression chains — cannot multiply
/// past what one native stack can lower. The value is single-sourced from
/// the parser (`cypherlite_parser::MAX_NESTING_DEPTH`), which enforces the
/// same cap for the expression nesting it counts but builds postfix chains
/// iteratively, so this guard is the only accounting those hops ever get;
/// it also covers the structural recursion the parser lets through (and
/// acts as a second, independent enforcement point), so an over-nested
/// query is a typed error instead of a stack-overflow abort in the
/// recursive lowering/validation walkers (LABS-70).
pub use crate::ast::MAX_NESTING_DEPTH;

/// Reject a statement tree whose structural nesting exceeds
/// [`MAX_NESTING_DEPTH`]. The check is structural in the walker core: the
/// statement cap fires in `crate::ast::tree::walk_statement_tree_at` (before the
/// statement visitor, so the walk early-exits at the first over-deep
/// statement, bounding its own recursion at the limit + 1) and the
/// expression cap in `crate::ast::tree::walk_expr_at`, with the cumulative entry-depth
/// semantics documented on `crate::ast::tree::walk_statement_tree_at` — so a plain
/// whole-tree walk with no-op visitors *is* the depth guard.
/// [`validate_statement_tree`] runs this pass first, before its
/// per-statement battery; this standalone entry point remains for callers
/// that want the depth guard only.
pub fn validate_nesting_depth(stmt: &Statement) -> Result<(), EngineError> {
    walk_statement_tree(stmt, &mut |_| Ok(()), &mut |_| Ok(()), &mut |_| Ok(()))
}

/// Run the ADR-0257 depth guard and then every scope-free semantic
/// validator on `stmt` *and* on every nested statement it contains (UNION
/// arms, NEXT stages, `CALL { }` / `WHEN` / `ELSE` bodies, FOREACH bodies,
/// and `EXISTS`/`COUNT`/`COLLECT` subquery bodies), so an over-nested query
/// is a typed error before lowering recurses and a rule violation cannot
/// hide inside nesting (LABS-70/LABS-81: `CALL { RETURN 1 LIMIT -1 }` used
/// to lower while its top-level equivalent was rejected).
///
/// Two cheap passes over the borrowed tree. Pass 1 is
/// [`validate_nesting_depth`] over the whole tree (cumulative entry-depth
/// semantics, early exit at the limit + 1). Pass 2 is the battery walk. The
/// split exists because the battery's statement visitor fires *before* its
/// own walk descends into that statement's expressions, and some battery
/// validators still walk expressions by hand — the grouping-key-aware
/// ORDER BY aggregation check (`aggregation::order_by::check_ob_agg_sub_exprs` /
/// `validate_ob_non_agg_part`), the list-comprehension aggregation check
/// (`aggregation::comprehension::check_list_comp_aggregation`), and the focused
/// static-types helpers (`static_types::expr_references_variables`,
/// `predicate_applies_arithmetic_to_var`, `expr_is_bare_var`,
/// `list_literal_all_non_numeric`) — with a descent keyed on grouping keys
/// and comprehension boundaries rather than plain child structure, so they
/// cannot ride the self-capping walker. Running the depth pass first means
/// each of them only ever runs on an expression tree already known to be
/// within [`MAX_NESTING_DEPTH`], and the guard's early exit means those
/// hand-recursion frames stack on zero live walk frames — the same
/// native-frame bound the historical depth-guard-first ordering gave.
///
/// The validator set applied per statement is deliberately limited to
/// validators whose checks are decided by the statement's own text:
/// clause-local rules (LIMIT/SKIP shape, static literal operator types,
/// duplicate column names, ORDER BY/list-comprehension aggregation,
/// relationship uniqueness, `$param` property-map shorthand placement),
/// per-body composition rules (USE placement, NEXT-chain admin commands,
/// UNION column shapes, SEARCH-clause rules, SHOW output uniqueness),
/// plus the COUNT-subquery UNION shape rules
/// ([`validate_count_subquery_union_shapes`]), which ride the walk's
/// *expression* visitor — the one place the battery can see which
/// subquery kind a body belongs to (`StmtRef` cannot tell a COUNT body
/// from an EXISTS one, and only COUNT carries the rules; CIP-63 leaves
/// EXISTS unrestricted). The
/// SEARCH inner-WHERE shape check runs at the lowering boundary instead
/// (`crate::search_filter`, the one grammar walk). Scope-tracking
/// validators (`validate_variable_types`, `validate_expression_constraints`,
/// `validate_aggregation_semantics`, `validate_show_column_names`, the SHOW
/// composability pair) need the enclosing body sequence — and, for a nested
/// body, the correlated outer bindings they cannot see — so they stay
/// top-level-only (FOREACH bodies, which share the enclosing scope, are
/// reached by `validate_variable_types`'s own recursion instead). The
/// CIP-150 dialect gates walk the whole tree themselves with version
/// inheritance.
pub fn validate_statement_tree(stmt: &Statement) -> Result<(), EngineError> {
    validate_nesting_depth(stmt)?;
    walk_statement_tree(
        stmt,
        &mut |node| validate_statement_node(node),
        &mut |_| Ok(()),
        &mut |expr| match expr {
            Expr::CountSubquery(cs) => validate_count_subquery_union_shapes(&cs.statement),
            _ => Ok(()),
        },
    )
}

/// The per-statement battery applied by [`validate_statement_tree`]; see its
/// doc comment for the inclusion rule. Order preserves the historical
/// top-level relative order of these passes. `stmt` is a borrowed
/// [`StmtRef`]: a real statement of the AST, or a FOREACH body's clause
/// list (which has no UNION/NEXT chain of its own, so the chain-aware
/// validators see `None` there exactly as they did for the synthetic
/// statement the walk used to fabricate). Chain links arrive as
/// [`StmtRef::UnionArm`]/[`StmtRef::NextStage`] so the chain-aware
/// validators run their whole-chain scan at the chain's head only — the
/// walk visits every link, and a non-head link's suffix re-scan was
/// redundant (and quadratic on long chains).
fn validate_statement_node(stmt: StmtRef<'_>) -> Result<(), EngineError> {
    let clauses = stmt.clauses();
    validate_show_output_uniqueness(clauses)?;
    validate_param_property_shorthand(clauses)?;
    validate_relationship_uniqueness(clauses)?;
    validate_order_by_aggregation(clauses)?;
    validate_column_name_conflicts(clauses)?;
    validate_use_placement(stmt)?;
    validate_next_no_admin_commands(stmt)?;
    validate_search_clauses(clauses)?;
    validate_static_operator_types(clauses)?;
    validate_limit_skip(clauses)?;
    validate_union_columns(stmt)?;
    validate_list_comprehension_aggregation(clauses)?;
    // Last, so a body violating two rules keeps the historical first error:
    // before this rule existed, the battery's earlier validators fired and
    // the RETURN-in-FOREACH rejection happened only at execution. A FOREACH
    // body arrives as the walk's borrowed `StmtRef::Body` (the only
    // clause-list body); a RETURN anywhere else is a projection.
    if let StmtRef::Body(foreach_clauses) = stmt {
        validate_foreach_body(foreach_clauses)?;
    }
    Ok(())
}

// Float literal overflow (a numeric literal whose text parses to +-infinity,
// e.g. `1.34E999`) is rejected at parse time in
// `cypherlite-parser::parser::parse_primary` — see the `TokenType::Float`
// arm there. It must happen at parse time rather than in a validator: once a
// numeric literal has been folded into `Expr::Literal(LiteralValue::Float(
// f64::INFINITY))`, it is indistinguishable from the legitimate CIP-55
// `Infinity`/`Inf`/`NaN` keyword literals, which also evaluate to an
// infinite/NaN `Float` and must NOT be rejected.

#[cfg(test)]
mod tests {
    use super::*;
    use cypherlite_test_support::run_with_big_stack;

    #[test]
    fn expression_chain_depth_is_counted_by_the_depth_guard() {
        // The parser's postfix loop counts no nesting depth for `.key`
        // property hops (or other per-hop forms), so the whole-tree walker
        // counts them instead: an over-cap chain is a typed depth error
        // from `validate_nesting_depth`, and a just-under-cap chain passes.
        // Big-stack threads as in the lower.rs ADR-0257 tests: even the
        // guard's own early-exit recursion (~500 levels) overflows the
        // default 2 MiB test-thread stack in debug builds.
        for (hops, want_err) in [(600usize, true), (499usize, false)] {
            run_with_big_stack(move || {
                let mut query = String::from("RETURN n");
                for _ in 0..hops {
                    query.push_str(".a");
                }
                let stmt = cypherlite_parser::parse(&query).unwrap();
                let result = validate_nesting_depth(&stmt);
                if want_err {
                    let err = result.unwrap_err().to_string();
                    let expected = format!("maximum nesting depth of {MAX_NESTING_DEPTH} exceeded");
                    assert!(err.contains(&expected), "wrong error: {err}");
                } else {
                    result.unwrap();
                }
            });
        }
    }
}
