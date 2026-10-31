//! `cypherlite-planner`: bind a lowered `Query` to a [`LogicalPlan`]
//! (ADR-0009), then rewrite MATCH (ADR-0024).
//!
//! Module map:
//! - [`plan`] — plan types, the `plan` entry point (consumes the `Query`)
//!   and the canonical plan walks ([`walk_plan_clauses`] for the full
//!   clause tree, [`walk_plan_parts`] for part-granular top-level clauses,
//!   [`walk_query_parts`] for the same part-granular walk over a
//!   not-yet-planned `ir::Query`)
//! - [`binding`] — the variable-binding environment
//! - [`validate`] — clause-level semantic validation
//! - [`expr_check`] / [`pattern`] / [`agg`] — expression, pattern, and
//!   aggregation checks used by validation
//! - [`names`] — output-column computation (UNION agreement, subquery
//!   exports, NEXT-stage seeds)
//! - [`rewrite`] — post-bind MATCH rewrites
//! - [`order`] — cardinality estimates and MATCH pattern order
//! - [`clause_kind`] — the Cypher keyword for a parser / planner-IR clause,
//!   the one read/write/schema clause classification behind
//!   [`clause_mutates_store`](crate::clause_mutates_store) (its published
//!   per-clause answer)
//! - [`access`] — the one read/write/schema clause fold over a statement,
//!   shared by [`namespaces`] and [`query_type`]
//! - [`namespaces`] — the ADR-0116 namespace classifiers (§2
//!   database-administration commands, §4 `system` / `system_ontologies`
//!   gates, read/write classification); the §3 `USE` rules live in
//!   [`use_target`]
//! - [`query_type`] — the Neo4j Query API v2 `queryType` classifier
//!   (r / w / rw / s), a projection of the shared [`access`] fold plus the
//!   statement's returned columns (ADR-0332)

mod access;
mod agg;
mod binding;
mod clause_kind;
mod expr_check;
mod names;
mod namespaces;
mod order;
mod pattern;
mod plan;
mod query_type;
mod rewrite;
mod stats;
mod use_target;
mod validate;

pub use crate::clause_kind::{clause_keyword, clause_mutates_store};
pub use crate::namespaces::{
    is_database_admin_command, is_show_databases_with_next, is_write, plan_calls_write_procedure,
    unsupported_system_clause_names,
};
pub use crate::order::{
    apply_degree, endpoint_estimate, seed_on_rel_start, seed_rows, Bound, Estimate,
};
pub use crate::plan::{
    plan, plan_any_clause, plan_contains_in_transactions, plan_output_names,
    plan_output_names_expect, plan_parameter_names, plan_with_stats, query_any_clause,
    walk_plan_clauses, walk_plan_parts, walk_query_parts, LogicalPlan,
};
pub use crate::query_type::{query_type, QueryType};
pub use crate::stats::{NoPatternStats, PatternStats};

/// Test helper: parse `cypher` and plan it with no write procedures
/// registered. Shared by the `namespaces` and `query_type` test modules.
#[cfg(test)]
pub(crate) fn plan_of(cypher: &str) -> LogicalPlan {
    let ast = cypherlite_front::parse_cypher(cypher).unwrap();
    plan(ast, &mut |_| false).unwrap()
}

#[cfg(test)]
mod tests {
    use cypherlite_front::parse_and_lower;
    use cypherlite_ir::{Clause, Expr};

    use super::*;

    /// Error from lowering or planning. The product parser rejects some
    /// shapes before the planner sees them; either stage is a rejection.
    fn rejection(src: &str) -> String {
        match parse_and_lower(src) {
            Ok(q) => plan(q, &mut |_| false).unwrap_err().to_string(),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn plans_return() {
        let q = parse_and_lower("RETURN 1").unwrap();
        let p = plan(q, &mut |_| false).unwrap();
        assert_eq!(p.parts.len(), 1);
    }

    /// ADR-0319: a planned statement's output names, from the final stage.
    #[test]
    fn plan_output_names_are_the_final_stage_columns() {
        for (cypher, expected) in [
            ("UNWIND [1, 0] AS x RETURN 10 / x AS v", vec!["v"]),
            ("MATCH (n) RETURN n, n.name AS name", vec!["n", "name"]),
            ("RETURN 1 AS a UNION RETURN 2 AS a", vec!["a"]),
            ("RETURN 1 AS a NEXT RETURN a + 1 AS b", vec!["b"]),
            ("CREATE (n:N)", vec![]),
            // A MERGE pattern's variables join the same walk the
            // MATCH/CREATE arms use, so `RETURN *` exports them too.
            ("MATCH (a) MERGE (b) RETURN *", vec!["a", "b"]),
        ] {
            assert_eq!(
                plan_output_names(&plan_of(cypher)).unwrap(),
                expected,
                "{cypher}"
            );
        }
    }

    /// ADR-0308: every `$name` the plan holds, once each, in first-appearance
    /// order — UNION arms, `CALL { … }` bodies, expression subqueries,
    /// pattern property maps, SKIP/LIMIT and `NEXT` stages included.
    #[test]
    fn plan_parameter_names_lists_each_reference_once_in_order() {
        let p = plan_of(
            "MATCH (n {k: $b}) WHERE EXISTS { MATCH (m) WHERE m.y = $c } \
             RETURN n, $a AS a SKIP $s \
             UNION CALL { RETURN $d AS n } RETURN 1 AS n, $b AS a \
             NEXT RETURN $e AS e",
        );
        assert_eq!(plan_parameter_names(&p), vec!["b", "c", "a", "s", "d", "e"]);
        assert!(plan_parameter_names(&plan_of("RETURN 1 AS x")).is_empty());
    }

    #[test]
    fn walk_plan_parts_visits_top_level_clauses_per_part_and_next_stage() {
        // UNION arms are separate parts; NEXT stages recurse; nested
        // positions (CALL { … } bodies, expression subqueries) are NOT
        // descended into — that is walk_plan_clauses' coverage.
        let q = cypherlite_front::parse_cypher(
            "MATCH (n) RETURN n AS x UNION CALL { RETURN 1 AS x } RETURN x NEXT RETURN 2 AS y",
        )
        .unwrap();
        let p = plan(q, &mut |_| false).unwrap();
        let mut part_sizes: Vec<usize> = Vec::new();
        let mut clause_kinds: Vec<&str> = Vec::new();
        walk_plan_parts(&p, &mut |clauses| {
            part_sizes.push(clauses.len());
            for clause in clauses {
                clause_kinds.push(match clause {
                    Clause::Match { .. } => "Match",
                    Clause::Return { .. } => "Return",
                    Clause::CallSubquery { .. } => "CallSubquery",
                    other => panic!("unexpected clause {other:?}"),
                });
            }
        });
        // Three parts total: two UNION arms plus the NEXT stage's one part.
        assert_eq!(part_sizes.len(), 3);
        // The CALL { … } body contributes its CallSubquery clause only —
        // the nested `RETURN 1 AS x` inside it is never visited.
        assert_eq!(
            clause_kinds,
            vec!["Match", "Return", "CallSubquery", "Return", "Return"]
        );
    }

    #[test]
    fn walk_query_parts_visits_top_level_clauses_per_part_and_next_stage() {
        // The ir::Query analog of walk_plan_parts: UNION arms are separate
        // parts; NEXT stages recurse; nested positions (CALL { … } bodies)
        // are NOT descended into — the caller recurses into clause-nested
        // queries.
        let q = parse_and_lower(
            "MATCH (n) RETURN n AS x UNION CALL { RETURN 1 AS x } RETURN x NEXT RETURN 2 AS y",
        )
        .unwrap();
        let mut part_sizes: Vec<usize> = Vec::new();
        let mut clause_kinds: Vec<&str> = Vec::new();
        walk_query_parts(&q, &mut |clauses| {
            part_sizes.push(clauses.len());
            for clause in clauses {
                clause_kinds.push(match clause {
                    Clause::Match { .. } => "Match",
                    Clause::Return { .. } => "Return",
                    Clause::CallSubquery { .. } => "CallSubquery",
                    other => panic!("unexpected clause {other:?}"),
                });
            }
        });
        // Three parts total: two UNION arms plus the NEXT stage's one part.
        assert_eq!(part_sizes.len(), 3);
        // The CALL { … } body contributes its CallSubquery clause only —
        // the nested `RETURN 1 AS x` inside it is never visited.
        assert_eq!(
            clause_kinds,
            vec!["Match", "Return", "CallSubquery", "Return", "Return"]
        );
    }

    #[test]
    fn plan_carries_typed_cypher_version() {
        let mut q = parse_and_lower("RETURN 1").unwrap();
        assert_eq!(
            plan(q.clone(), &mut |_| false).unwrap().cypher_version,
            None
        );
        q.cypher_version = Some(5);
        assert_eq!(plan(q, &mut |_| false).unwrap().cypher_version, Some(5));
    }

    #[test]
    fn schema_index_must_be_standalone() {
        plan(
            parse_and_lower("CREATE INDEX FOR (n:Person) ON (n.name)").unwrap(),
            &mut |_| false,
        )
        .unwrap();
        plan(parse_and_lower("SHOW INDEXES").unwrap(), &mut |_| false).unwrap();
        let err = plan(
            parse_and_lower("CREATE INDEX FOR (n:Person) ON (n.name) RETURN 1").unwrap(),
            &mut |_| false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("InvalidClauseComposition"),
            "expected standalone schema clause, got {err}"
        );
    }

    #[test]
    fn path_then_node_is_type_conflict() {
        let _ = rejection("MATCH r = ()-[]-() MATCH (r) RETURN r");
    }

    #[test]
    fn call_yield_binds_for_return() {
        let q = parse_and_lower("CALL db.labels() YIELD label RETURN label").unwrap();
        plan(q, &mut |_| false).unwrap();
    }

    #[test]
    fn size_on_pattern_predicate_is_unexpected_syntax() {
        let err = rejection("MATCH (a), (b) RETURN size((a)--())");
        assert!(
            err.contains("UnexpectedSyntax"),
            "expected UnexpectedSyntax, got {err}"
        );
    }

    #[test]
    fn pattern_predicate_unbound_variable() {
        let err = rejection("MATCH (n) WHERE (a)--() RETURN n");
        assert!(
            err.contains("UndefinedVariable"),
            "expected UndefinedVariable, got {err}"
        );
    }

    #[test]
    fn pattern_predicate_in_return_is_unexpected_syntax() {
        let err = rejection("MATCH (n) RETURN (n)-[]->()");
        assert!(
            err.contains("UnexpectedSyntax"),
            "expected UnexpectedSyntax, got {err}"
        );
    }

    #[test]
    fn with_where_sees_pre_projection_variables() {
        let q = parse_and_lower("MATCH (a)-[r]->(c) WITH c WHERE r IS NULL RETURN c").unwrap();
        plan(q, &mut |_| false).unwrap();
    }

    #[test]
    fn node_only_pattern_predicate_is_invalid() {
        let err = rejection("MATCH (n) WHERE (n) RETURN n");
        assert!(
            err.contains("InvalidArgumentType"),
            "expected InvalidArgumentType, got {err}"
        );
    }

    #[test]
    fn match_on_map_variable_is_type_conflict() {
        let err = rejection("WITH {x: 1} AS n MATCH (n) RETURN n");
        assert!(
            err.contains("VariableTypeConflict"),
            "expected VariableTypeConflict, got {err}"
        );
    }

    #[test]
    fn match_on_value_variable_is_type_conflict() {
        let err = rejection("WITH 123 AS n MATCH (n) RETURN n");
        assert!(
            err.contains("VariableTypeConflict"),
            "expected VariableTypeConflict, got {err}"
        );
    }

    #[test]
    fn count_rand_is_non_constant() {
        let err = rejection("RETURN count(rand())");
        assert!(
            err.contains("NonConstantExpression"),
            "expected NonConstantExpression, got {err}"
        );
    }

    #[test]
    fn create_undirected_is_error() {
        let err = rejection("CREATE (a)-[:FOO]-(b)");
        assert!(
            err.contains("RequiresDirectedRelationship"),
            "expected RequiresDirectedRelationship, got {err}"
        );
    }

    #[test]
    fn ambiguous_aggregation_is_error() {
        let err = rejection("MATCH (me)--(you) WITH me.age + count(you.age) AS agg RETURN *");
        assert!(
            err.contains("AmbiguousAggregationExpression"),
            "expected AmbiguousAggregationExpression, got {err}"
        );
    }

    #[test]
    fn group_by_key_in_aggregate_expr_is_valid() {
        // The planner-IR parser does not parse GROUP BY, so build the IR the
        // front-end would lower: `UNWIND [1,1,2] AS x RETURN count(*) + x AS c`
        // with an optional explicit grouping-key set.
        fn query(group_by: Option<Vec<Expr>>) -> cypherlite_ir::Query {
            cypherlite_ir::Query {
                parts: vec![cypherlite_ir::SingleQuery {
                    clauses: vec![
                        Clause::Unwind {
                            expression: Expr::List(vec![Expr::Int(1), Expr::Int(1), Expr::Int(2)]),
                            alias: "x".into(),
                        },
                        Clause::Return {
                            distinct: false,
                            group_by,
                            items: vec![cypherlite_ir::Projection {
                                expr: Expr::Binary {
                                    op: cypherlite_ir::BinaryOp::Add,
                                    left: Box::new(Expr::CountStar),
                                    right: Box::new(Expr::Variable("x".into())),
                                },
                                alias: Some("c".into()),
                                source_text: None,
                            }],
                            order: vec![],
                            skip: None,
                            limit: None,
                        },
                    ],
                }],
                union_all: vec![],
                next: None,
                cypher_version: None,
            }
        }

        // `GROUP BY x`: the key is in scope, so `count(*) + x` is valid.
        plan(query(Some(vec![Expr::Variable("x".into())])), &mut |_| {
            false
        })
        .unwrap();

        // No GROUP BY: nothing is grouped, so the same expression is ambiguous.
        let err = plan(query(None), &mut |_| false).unwrap_err().to_string();
        assert!(
            err.contains("AmbiguousAggregationExpression"),
            "expected AmbiguousAggregationExpression, got {err}"
        );

        // `GROUP BY ()`: the empty grouping set groups nothing, so `x` stays
        // a non-grouped variable and the expression is ambiguous.
        let err = plan(query(Some(vec![])), &mut |_| false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("AmbiguousAggregationExpression"),
            "expected AmbiguousAggregationExpression, got {err}"
        );
    }

    #[test]
    fn yield_star_with_return_is_unexpected() {
        let err = rejection("CALL test.p() YIELD * RETURN city");
        assert!(
            err.contains("UnexpectedSyntax"),
            "expected UnexpectedSyntax, got {err}"
        );
    }

    #[test]
    fn list_as_fixed_rel_is_type_conflict() {
        let err = rejection("WITH [] AS r MATCH ()-[r]-() RETURN r");
        assert!(
            err.contains("VariableTypeConflict"),
            "expected VariableTypeConflict, got {err}"
        );
    }

    #[test]
    fn create_already_bound_node_is_error() {
        let err = rejection("MATCH (a) CREATE (a)");
        assert!(
            err.contains("VariableAlreadyBound"),
            "expected VariableAlreadyBound, got {err}"
        );
    }

    #[test]
    fn merge_new_label_on_bound_node_is_error() {
        let err = rejection("CREATE (a:Foo) MERGE (a)-[:KNOWS]->(a:Bar)");
        assert!(
            err.contains("VariableAlreadyBound"),
            "expected VariableAlreadyBound, got {err}"
        );
    }

    #[test]
    fn delete_arithmetic_is_invalid_argument() {
        let err = rejection("MATCH () DELETE 1 + 1");
        assert!(
            err.contains("InvalidArgumentType"),
            "expected InvalidArgumentType, got {err}"
        );
    }

    #[test]
    fn distinct_order_by_unprojected_is_undefined() {
        let err = rejection("MATCH (a) RETURN DISTINCT a.name ORDER BY a.age");
        assert!(
            err.contains("UndefinedVariable"),
            "expected UndefinedVariable, got {err}"
        );
    }

    #[test]
    fn merge_already_bound_node_is_error() {
        let err = rejection("MATCH (a) MERGE (a)");
        assert!(
            err.contains("VariableAlreadyBound"),
            "expected VariableAlreadyBound, got {err}"
        );
    }

    #[test]
    fn delete_label_expression_is_invalid() {
        let err = rejection("MATCH (n) DELETE n:Person");
        assert!(
            err.contains("InvalidDelete"),
            "expected InvalidDelete, got {err}"
        );
    }

    #[test]
    fn create_reuse_bound_node_in_path_is_ok() {
        let q = parse_and_lower("MATCH (a) CREATE (a)-[:R]->(b)").unwrap();
        plan(q, &mut |_| false).unwrap();
    }

    #[test]
    fn with_star_keeps_create_variables() {
        let q = parse_and_lower(
            "CREATE (a {var: 'start'}), (b {var: 'end'}) WITH * UNWIND range(1, 2) AS i WITH a, b, i RETURN a, b, i",
        )
        .unwrap();
        plan(q, &mut |_| false).unwrap();
    }

    fn first_match(p: &LogicalPlan) -> &cypherlite_ir::Clause {
        p.parts[0]
            .iter()
            .find(|c| matches!(c, Clause::Match { .. }))
            .expect("MATCH")
    }

    #[test]
    fn folds_where_equality_into_node_map() {
        let q = parse_and_lower("MATCH (n:Person) WHERE n.name = 'Ada' RETURN n").unwrap();
        let p = plan(q, &mut |_| false).unwrap();
        match first_match(&p) {
            Clause::Match {
                patterns,
                where_clause,
                ..
            } => {
                assert!(where_clause.is_none());
                let n = &patterns[0].nodes[0];
                assert_eq!(n.labels, vec!["Person".to_string()]);
                match &n.properties {
                    Some(Expr::Map(pairs)) => {
                        assert!(pairs.iter().any(
                            |(k, v)| k == "name" && matches!(v, Expr::String(s) if s == "Ada")
                        ));
                    }
                    other => panic!("expected map, got {other:?}"),
                }
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn orients_path_to_selective_end() {
        let q = parse_and_lower("MATCH (b)<-[:TRANSFER]-(a:Account {id: $id}) RETURN b").unwrap();
        let p = plan(q, &mut |_| false).unwrap();
        match first_match(&p) {
            Clause::Match { patterns, .. } => {
                let path = &patterns[0];
                assert_eq!(path.nodes[0].variable.as_deref(), Some("a"));
                assert_eq!(path.nodes[1].variable.as_deref(), Some("b"));
                assert_eq!(path.rels[0].direction, cypherlite_ir::Direction::Outgoing);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn does_not_reverse_named_var_length_rel() {
        let q = parse_and_lower("MATCH (a)-[r:REL*2..2]->(b:End) RETURN r").unwrap();
        let p = plan(q, &mut |_| false).unwrap();
        match first_match(&p) {
            Clause::Match { patterns, .. } => {
                assert_eq!(patterns[0].nodes[0].variable.as_deref(), Some("a"));
                assert_eq!(patterns[0].nodes[1].variable.as_deref(), Some("b"));
                assert_eq!(
                    patterns[0].rels[0].direction,
                    cypherlite_ir::Direction::Outgoing
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn does_not_reverse_named_path() {
        let q =
            parse_and_lower("MATCH p = (b)<-[:TRANSFER]-(a:Account {id: $id}) RETURN p").unwrap();
        let p = plan(q, &mut |_| false).unwrap();
        match first_match(&p) {
            Clause::Match { patterns, .. } => {
                assert_eq!(patterns[0].variable.as_deref(), Some("p"));
                assert_eq!(patterns[0].nodes[0].variable.as_deref(), Some("b"));
                assert_eq!(patterns[0].nodes[1].variable.as_deref(), Some("a"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn orders_disconnected_seek_first() {
        let q = parse_and_lower("MATCH (a), (b:Account {id: $id}) RETURN a, b").unwrap();
        let p = plan(q, &mut |_| false).unwrap();
        match first_match(&p) {
            Clause::Match { patterns, .. } => {
                assert_eq!(patterns[0].nodes[0].variable.as_deref(), Some("b"));
                assert_eq!(patterns[1].nodes[0].variable.as_deref(), Some("a"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn does_not_fold_or_predicate() {
        let q = parse_and_lower("MATCH (n:Person) WHERE n.name = 'Ada' OR n.name = 'Bob' RETURN n")
            .unwrap();
        let p = plan(q, &mut |_| false).unwrap();
        match first_match(&p) {
            Clause::Match { where_clause, .. } => {
                assert!(where_clause.is_some());
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn folds_where_equality_into_rel_map() {
        let q =
            parse_and_lower("MATCH (a)-[t:TRANSFER]->(b) WHERE t.amount = $x RETURN t").unwrap();
        let p = plan(q, &mut |_| false).unwrap();
        match first_match(&p) {
            Clause::Match {
                patterns,
                where_clause,
                ..
            } => {
                assert!(where_clause.is_none());
                let t = &patterns[0].rels[0];
                match &t.properties {
                    Some(Expr::Map(pairs)) => {
                        assert!(pairs
                            .iter()
                            .any(|(k, v)| k == "amount"
                                && matches!(v, Expr::Parameter(s) if s == "x")));
                    }
                    other => panic!("expected map, got {other:?}"),
                }
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn does_not_fold_rel_inequality() {
        let q = parse_and_lower(
            "MATCH (u:User)-[g:HAS_ROLE]->(r:Role) WHERE g.until < date('2024-01-01') RETURN u",
        )
        .unwrap();
        let p = plan(q, &mut |_| false).unwrap();
        match first_match(&p) {
            Clause::Match {
                patterns,
                where_clause,
                ..
            } => {
                assert!(where_clause.is_some());
                assert!(patterns[0].rels[0].properties.is_none());
            }
            _ => unreachable!(),
        }
    }

    // ADR-0175 (statically-known floats): a float *literal* at an
    // integer-argument site — range(), substring()/left()/right(), list
    // index, slice bounds — fails at plan time with the same
    // InvalidArgumentType kind the site raises at runtime (Neo4j's 22N27 /
    // 22NB1 static gate fires at the same phase). The planner raises
    // `Error::ty`: for range() and list index the rendered message matches
    // the runtime failure; for substring()/left()/right()/slice the wording
    // is plan-time-only — at runtime those sites coerce floats (truncate
    // toward zero, NaN → 0, saturate) and only raise NumberOutOfRange with
    // different wording, per ADR-0175. `parse_and_lower` must succeed (the
    // rejection is the planner's, not the front-end's).
    #[test]
    fn float_literals_at_integer_argument_sites_fail_at_plan_time() {
        for (src, want) in [
            (
                "RETURN range(1.5, 5)",
                "type error: InvalidArgumentType: range() start",
            ),
            (
                "RETURN range(1, 5.9)",
                "type error: InvalidArgumentType: range() end",
            ),
            (
                "RETURN range(1, 10, 2.5)",
                "type error: InvalidArgumentType: range() step",
            ),
            // The parser splits the sign into a unary minus; a signed float
            // literal is the same mistake.
            (
                "RETURN range(-1.1, 1, 1)",
                "type error: InvalidArgumentType: range() start",
            ),
            (
                "RETURN substring('abcdef', 1.5)",
                "type error: InvalidArgumentType: substring() start must be an integer",
            ),
            (
                "RETURN substring('abcdef', 1, 2.5)",
                "type error: InvalidArgumentType: substring() length must be an integer",
            ),
            (
                "RETURN left('abcdef', 2.9)",
                "type error: InvalidArgumentType: left() length must be an integer",
            ),
            (
                "RETURN right('abcdef', 2.9)",
                "type error: InvalidArgumentType: right() length must be an integer",
            ),
            (
                "RETURN [1,2,3][1.0]",
                "type error: InvalidArgumentType: list index must be an integer",
            ),
            (
                "RETURN [1,2,3][1.0..2]",
                "type error: InvalidArgumentType: slice start must be an integer",
            ),
            (
                "RETURN [1,2,3][1..2.9]",
                "type error: InvalidArgumentType: slice end must be an integer",
            ),
        ] {
            let q = parse_and_lower(src)
                .unwrap_or_else(|e| panic!("`{src}` must survive the front-end: {e}"));
            let error =
                plan(q, &mut |_| false).expect_err(&format!("`{src}` must fail at plan time"));
            assert_eq!(error.to_string(), want, "`{src}`");
        }
    }

    #[test]
    fn non_literal_floats_at_integer_argument_sites_plan_fine() {
        // Every non-literal float — a constant expression, a function call,
        // a parameter, a variable — is handled at runtime (ADR-0175's
        // recorded compile-vs-runtime deviation); planning must not reject
        // it.
        for src in [
            "RETURN range(0.0/0.0, 5)",
            "RETURN range(head([1.5, 'x']), 5)",
            "RETURN substring('abcdef', head([1.5, 'x']))",
            "RETURN left('abcdef', $n)",
            "RETURN [1,2,3][head([1.0, 'x'])]",
            "RETURN [1,2,3][head([1.5, 'x'])..2]",
            "UNWIND [1.5] AS n RETURN range(n, 5)",
        ] {
            let q = parse_and_lower(src).unwrap();
            assert!(plan(q, &mut |_| false).is_ok(), "`{src}` must plan");
        }
    }
}
