use super::*;

#[test]
fn test_parse_create() {
    let stmt = parse("CREATE (n:Person {name: 'Bob'})").unwrap();
    assert_eq!(stmt.clauses.len(), 1);
    match &stmt.clauses[0] {
        Clause::Create(c) => {
            assert_eq!(c.pattern.len(), 1);
        }
        _ => panic!("expected create"),
    }
}

#[test]
fn test_parse_merge() {
    let stmt = parse("MERGE (n:Person {name: 'Bob'}) ON CREATE SET n.created = true ON MATCH SET n.updated = true").unwrap();
    match &stmt.clauses[0] {
        Clause::Merge(m) => {
            assert_eq!(m.on_create.len(), 1);
            assert_eq!(m.on_match.len(), 1);
        }
        _ => panic!("expected merge"),
    }
}

#[test]
fn test_parse_delete() {
    let stmt = parse("MATCH (n) DELETE n").unwrap();
    assert_eq!(stmt.clauses.len(), 2);
    match &stmt.clauses[1] {
        Clause::Delete(d) => {
            assert!(!d.detach);
            assert_eq!(d.expressions.len(), 1);
        }
        _ => panic!("expected delete"),
    }
}

#[test]
fn test_parse_detach_delete() {
    let stmt = parse("MATCH (n) DETACH DELETE n").unwrap();
    match &stmt.clauses[1] {
        Clause::Delete(d) => {
            assert!(d.detach);
        }
        _ => panic!("expected delete"),
    }
}

#[test]
fn test_parse_nodetach_delete() {
    // CIP-122: NODETACH DELETE is semantically a plain (non-detaching)
    // delete -- same as omitting the keyword.
    let stmt = parse("MATCH (n) NODETACH DELETE n").unwrap();
    match &stmt.clauses[1] {
        Clause::Delete(d) => {
            assert!(!d.detach);
            assert_eq!(d.expressions.len(), 1);
        }
        _ => panic!("expected delete"),
    }
}

#[test]
fn test_parse_nodetach_delete_case_insensitive() {
    let stmt = parse("MATCH (n) nodetach delete n").unwrap();
    match &stmt.clauses[1] {
        Clause::Delete(d) => assert!(!d.detach),
        _ => panic!("expected delete"),
    }
}

#[test]
fn test_parse_nodetach_still_usable_as_identifier() {
    // NODETACH is a contextual keyword, only meaningful directly before
    // DELETE -- it must remain usable as an ordinary variable name.
    let stmt = parse("MATCH (nodetach:Person) RETURN nodetach.name AS name").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match &m.pattern[0].elements[0] {
            PatternElement::Node(np) => {
                assert_eq!(np.variable, Some("nodetach".to_string()));
            }
            _ => panic!("expected node pattern"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_detach_nodetach_combo_is_error() {
    // Syntax restricts to `DETACH | NODETACH` (an "or") -- combining
    // both keywords on one DELETE is invalid.
    assert!(parse("MATCH (n) DETACH NODETACH DELETE n").is_err());
}

#[test]
fn test_parse_nodetach_without_delete_is_error() {
    assert!(parse("MATCH (n) NODETACH RETURN n").is_err());
}

#[test]
fn test_parse_insert_basic() {
    // CIP-114: INSERT parses to the same Clause::Create AST as CREATE,
    // tagged `is_insert` for the engine's differing `+labels` stats.
    let stmt = parse("INSERT (n:A&B {x: 1})").unwrap();
    match &stmt.clauses[0] {
        Clause::Create(cc) => assert!(cc.is_insert),
        _ => panic!("expected create"),
    }
    let stmt = parse("CREATE (n:A)").unwrap();
    match &stmt.clauses[0] {
        Clause::Create(cc) => assert!(!cc.is_insert),
        _ => panic!("expected create"),
    }
}

#[test]
fn test_parse_insert_rejects_colon_conjunction() {
    assert!(parse("INSERT (n :A:B)").is_err());
}

#[test]
fn test_parse_insert_rejects_named_path() {
    assert!(parse("INSERT p = (a)-[r:R]->(b)").is_err());
}

#[test]
fn test_parse_insert_rejects_parenthesized_label_expr() {
    assert!(parse("INSERT (n:(A:B):C)").is_err());
}

#[test]
fn test_parse_create_still_allows_named_path_and_parens() {
    // Unaffected CREATE baselines, contrasted with INSERT's rejections.
    assert!(parse("CREATE p = (a)-[r:R]->(b)").is_ok());
    assert!(parse("CREATE (n:(A:B):C)").is_ok());
}

#[test]
fn test_parse_set() {
    let stmt = parse("MATCH (n) SET n.name = 'Alice', n:Person").unwrap();
    match &stmt.clauses[1] {
        Clause::Set(s) => {
            assert_eq!(s.items.len(), 2);
            // First item: property set
            assert!(s.items[0].property.is_some());
            // Second item: label set
            assert_eq!(
                s.items[1].labels,
                vec![LabelExpr::Label("Person".to_string())]
            );
        }
        _ => panic!("expected set"),
    }
}

#[test]
fn test_parse_remove() {
    let stmt = parse("MATCH (n) REMOVE n.name, n:Person").unwrap();
    match &stmt.clauses[1] {
        Clause::Remove(r) => {
            assert_eq!(r.items.len(), 2);
        }
        _ => panic!("expected remove"),
    }
}

#[test]
fn test_parse_return_with_alias() {
    let stmt = parse("MATCH (n) RETURN n.name AS name").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => {
            assert_eq!(r.items[0].alias, Some("name".to_string()));
        }
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_order_by() {
    let stmt = parse("MATCH (n) RETURN n ORDER BY n.name DESC").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => {
            assert_eq!(r.order_by.len(), 1);
            assert!(r.order_by[0].descending);
        }
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_skip_limit() {
    let stmt = parse("MATCH (n) RETURN n SKIP 5 LIMIT 10").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => {
            assert!(r.skip.is_some());
            assert!(r.limit.is_some());
        }
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_with() {
    let stmt = parse("MATCH (n) WITH n.name AS name WHERE name = 'Alice' RETURN name").unwrap();
    match &stmt.clauses[1] {
        Clause::With(w) => {
            assert_eq!(w.items.len(), 1);
            assert!(w.where_expr.is_some());
        }
        _ => panic!("expected with"),
    }
}

#[test]
fn test_parse_unwind() {
    let stmt = parse("UNWIND [1, 2, 3] AS x RETURN x").unwrap();
    match &stmt.clauses[0] {
        Clause::Unwind(u) => {
            assert_eq!(u.alias, "x");
        }
        _ => panic!("expected unwind"),
    }
}

#[test]
fn clause_starter_table_covers_the_dispatch() {
    // `CLAUSE_STARTER_TOKENS` (token.rs) is the derived clause-starter
    // vocabulary every lookahead consults; the grammar authority is the
    // clause dispatch (`parse_clause_inner`). This pin keeps the table on
    // the dispatch's side: every table row's keyword, alone at clause
    // position, parses *past* the dispatch (its clause body fails with a
    // clause-specific error) -- never the dispatch's default
    // `unexpected token` arm. A row the dispatch does not handle fails
    // here; the reverse direction (a dispatch arm without a row) is the
    // review discipline the round-5 LOAD/DROP drift motivated the table
    // for.
    use crate::token::CLAUSE_STARTER_TOKENS;
    for ty in CLAUSE_STARTER_TOKENS {
        let spelling = format!("{ty:?}").to_lowercase();
        let generic = format!("unexpected token {spelling:?}");
        if let Err(err) = parse(&spelling) {
            assert_ne!(
                err.message, generic,
                "{spelling} is in the starter table but not the clause dispatch"
            );
        }
    }
}

#[test]
fn test_parse_union() {
    let stmt = parse("MATCH (n:A) RETURN n UNION MATCH (n:B) RETURN n").unwrap();
    assert!(stmt.union.is_some());
    assert!(!stmt.union.as_ref().unwrap().all);
}

#[test]
fn test_parse_union_all() {
    let stmt = parse("MATCH (n:A) RETURN n UNION ALL MATCH (n:B) RETURN n").unwrap();
    assert!(stmt.union.as_ref().unwrap().all);
}

#[test]
fn test_parse_union_distinct() {
    // CIP-154: `UNION DISTINCT` is a synonym of plain UNION's set
    // semantics -- represented identically as `all: false` so the
    // existing ALL/non-ALL consistency check rejects mixing it with
    // `UNION ALL` for free.
    let stmt = parse("MATCH (n:A) RETURN n UNION DISTINCT MATCH (n:B) RETURN n").unwrap();
    assert!(!stmt.union.as_ref().unwrap().all);
}

#[test]
fn test_parse_union_distinct_chain_mixed_with_plain_union() {
    // Test cases 3/6 (CIP-154 NOTES.md): a RegularQuery may freely mix
    // plain UNION and UNION DISTINCT throughout.
    let stmt = parse("RETURN 1 AS x UNION DISTINCT RETURN 2 AS x UNION RETURN 3 AS x").unwrap();
    assert!(!stmt.union.as_ref().unwrap().all);
    let second = &stmt.union.as_ref().unwrap().statement;
    assert!(!second.union.as_ref().unwrap().all);
}

#[test]
fn test_parse_optional_match() {
    let stmt = parse("OPTIONAL MATCH (n) RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => assert!(m.optional),
        _ => panic!("expected match"),
    }
}

// ---- CIP-79: USING INDEX/JOIN/SCAN planner hints ----

#[test]
fn test_using_index_hint_is_parsed_and_ignored() {
    let stmt =
        parse("MATCH (n:Person) USING INDEX n:Person(age) WHERE n.age = 30 RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => assert!(m.where_expr.is_some()),
        other => panic!("expected Match clause, got {other:?}"),
    }
    // `USING INDEX SEEK` is the explicit-seek spelling of the same hint.
    assert!(parse("MATCH (n:Person) USING INDEX SEEK n:Person(age) RETURN n").is_ok());
}

#[test]
fn test_using_scan_and_join_hints_are_parsed_and_ignored() {
    assert!(parse("MATCH (n:Person) USING SCAN n:Person RETURN n").is_ok());
    assert!(parse("MATCH (a:A), (b:B) USING JOIN ON a, b WHERE a.id = b.id RETURN a, b").is_ok());
}

#[test]
fn test_parse_explain() {
    let stmt = parse("EXPLAIN MATCH (n) RETURN n").unwrap();
    assert!(stmt.explain);
}

#[test]
fn test_parse_profile() {
    let stmt = parse("PROFILE MATCH (n) RETURN n").unwrap();
    assert!(stmt.profile);
}

#[test]
fn test_parse_foreach() {
    let stmt = parse("FOREACH (x IN [1, 2, 3] | CREATE (n {val: x}))").unwrap();
    match &stmt.clauses[0] {
        Clause::Foreach(f) => {
            assert_eq!(f.variable, "x");
            assert_eq!(f.clauses.len(), 1);
        }
        _ => panic!("expected foreach"),
    }
}

#[test]
fn test_parse_with_star_then_more_items() {
    // CIP-101/CIP-118: `WITH *, expr AS x` must not stop at the star.
    let stmt = parse("MATCH (n) WITH *, n.name AS x RETURN x").unwrap();
    match &stmt.clauses[1] {
        Clause::With(w) => {
            assert_eq!(w.items.len(), 2);
            assert!(w.items[0].all);
            assert!(!w.items[1].all);
            assert_eq!(w.items[1].alias.as_deref(), Some("x"));
        }
        _ => panic!("expected with"),
    }
}

#[test]
fn test_parse_return_star_then_more_items() {
    let stmt = parse("MATCH (n) RETURN *, n.name AS x").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => {
            assert_eq!(r.items.len(), 2);
            assert!(r.items[0].all);
            assert!(!r.items[1].all);
        }
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_return_star_alone_still_works() {
    let stmt = parse("MATCH (n) RETURN *").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => {
            assert_eq!(r.items.len(), 1);
            assert!(r.items[0].all);
        }
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_return_all_modifier() {
    // CIP-199: `RETURN ALL <items>` is an explicit, semantically no-op
    // spelling of plain RETURN -- `distinct: false`, same as omitting
    // the keyword entirely.
    let with_all = parse("MATCH (n) RETURN ALL n.prop AS prop").unwrap();
    let without = parse("MATCH (n) RETURN n.prop AS prop").unwrap();
    assert_eq!(with_all, without);
    match &with_all.clauses[1] {
        Clause::Return(r) => assert!(!r.distinct),
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_with_all_modifier() {
    let with_all = parse("MATCH (n) WITH ALL n.prop AS prop RETURN prop").unwrap();
    let without = parse("MATCH (n) WITH n.prop AS prop RETURN prop").unwrap();
    assert_eq!(with_all, without);
    match &with_all.clauses[1] {
        Clause::With(w) => assert!(!w.distinct),
        _ => panic!("expected with"),
    }
}

#[test]
fn test_parse_return_all_star_is_not_confused_with_all_quantifier() {
    // `RETURN ALL *` (ALL modifier + star item) must not be mistaken
    // for the `all(...)` quantifier, and `RETURN all(x IN ...)` (the
    // quantifier as the sole/first return item) must not be mistaken
    // for the ALL modifier -- disambiguated by whether `ALL` is
    // immediately followed by `(`.
    let stmt = parse("UNWIND [1, 2] AS x RETURN ALL *").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => {
            assert_eq!(r.items.len(), 1);
            assert!(r.items[0].all);
            assert!(!r.distinct);
        }
        _ => panic!("expected return"),
    }

    let quantifier_stmt = parse("RETURN all(x IN [1, 2, 3] WHERE x > 0) AS r").unwrap();
    match &quantifier_stmt.clauses[0] {
        Clause::Return(r) => {
            assert_eq!(r.items.len(), 1);
            assert!(!r.items[0].all);
            assert!(matches!(r.items[0].expr, Expr::Quantifier(_)));
        }
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_return_all_distinct_is_a_syntax_error() {
    // CIP-199: ALL and DISTINCT are mutually exclusive on one
    // RETURN/WITH -- after consuming ALL, DISTINCT is not a valid
    // return item, so this is naturally rejected without any explicit
    // mutual-exclusion check.
    assert!(parse("MATCH (n) RETURN ALL DISTINCT n.prop AS prop").is_err());
    assert!(parse("MATCH (n) WITH ALL DISTINCT n.prop AS prop RETURN prop").is_err());
}

#[test]
fn test_parse_distinct() {
    let stmt = parse("MATCH (n) RETURN DISTINCT n").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => assert!(r.distinct),
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_load_csv() {
    let stmt = parse("LOAD CSV WITH HEADERS FROM 'file.csv' AS row RETURN row").unwrap();
    match &stmt.clauses[0] {
        Clause::LoadCSV(l) => {
            assert!(l.with_headers);
            assert_eq!(l.alias, "row");
        }
        _ => panic!("expected load csv"),
    }
}

#[test]
fn test_load_csv_field_terminator_must_be_a_string_literal() {
    let stmt = parse("LOAD CSV FROM 'file.tsv' AS row FIELDTERMINATOR '\\t' RETURN row").unwrap();
    match &stmt.clauses[0] {
        Clause::LoadCSV(l) => assert_eq!(l.field_terminator.as_deref(), Some("\t")),
        other => panic!("expected load csv, got {other:?}"),
    }
    for q in [
        "LOAD CSV FROM 'file.tsv' AS row FIELDTERMINATOR 1 RETURN row",
        "LOAD CSV FROM 'file.tsv' AS row FIELDTERMINATOR $sep RETURN row",
    ] {
        let err = parse(q).expect_err("non-literal FIELDTERMINATOR must fail the parse");
        assert!(
            err.message
                .starts_with("expected a string literal after FIELDTERMINATOR"),
            "{q:?}: {}",
            err.message
        );
    }
}

#[test]
fn test_parse_yield_all() {
    let stmt = parse("CALL db.labels() YIELD *").unwrap();
    match &stmt.clauses[0] {
        Clause::CallProcedure(c) => {
            assert!(c.yields.all);
        }
        _ => panic!("expected call procedure"),
    }
}

#[test]
fn test_parse_error_position() {
    let err = parse("MATCH (n) BLAH").unwrap_err();
    assert!(err.message.contains("BLAH"));
}

#[test]
fn test_parse_set_merge_props() {
    let stmt = parse("MATCH (n) SET n += {age: 30}").unwrap();
    match &stmt.clauses[1] {
        Clause::Set(s) => {
            assert!(s.items[0].merge);
            assert!(s.items[0].variable.is_some());
        }
        _ => panic!("expected set"),
    }
}

#[test]
fn test_parse_with_distinct() {
    let stmt = parse("MATCH (n) WITH DISTINCT n RETURN n").unwrap();
    match &stmt.clauses[1] {
        Clause::With(w) => assert!(w.distinct),
        _ => panic!("expected with"),
    }
}

// ---- CIP-141 USE, CIP-206 LET, CIP-140 FINISH ----

#[test]
fn test_use_clause_parses_as_noop() {
    let stmt = parse("WITH 'g' AS g USE graph.byName(g) RETURN 1 AS x").unwrap();
    match &stmt.clauses[1] {
        Clause::Use(uc) => match &uc.graph_expr {
            Expr::FunctionCall(fc) => assert_eq!(fc.name, "graph.byName"),
            other => panic!("expected function call, got {other:?}"),
        },
        other => panic!("expected Use clause, got {other:?}"),
    }
}

#[test]
fn test_let_clause_desugars_to_with_star() {
    let stmt = parse("LET x = 1 + 1 RETURN x").unwrap();
    match &stmt.clauses[0] {
        Clause::With(wc) => {
            assert_eq!(wc.items.len(), 2);
            assert!(wc.items[0].all);
            assert_eq!(wc.items[1].alias.as_deref(), Some("x"));
        }
        other => panic!("expected With clause, got {other:?}"),
    }
}

#[test]
fn test_let_clause_multiple_bindings() {
    let stmt = parse("LET x = 1, y = 2 RETURN x, y").unwrap();
    match &stmt.clauses[0] {
        Clause::With(wc) => {
            assert_eq!(wc.items.len(), 3);
            assert_eq!(wc.items[1].alias.as_deref(), Some("x"));
            assert_eq!(wc.items[2].alias.as_deref(), Some("y"));
        }
        other => panic!("expected With clause, got {other:?}"),
    }
}

#[test]
fn test_finish_clause_parses() {
    let stmt = parse("MATCH (n) FINISH").unwrap();
    assert_eq!(stmt.clauses.len(), 2);
    assert_eq!(stmt.clauses[1], Clause::Finish);
}

// ---- CIP-168 standalone ORDER BY / OFFSET / SKIP / LIMIT ----

#[test]
fn test_standalone_order_by_page_parses() {
    let stmt = parse("MATCH (n) ORDER BY n.name SKIP 1 LIMIT 3 RETURN n").unwrap();
    match &stmt.clauses[1] {
        Clause::StandaloneOrderBy(sc) => {
            assert_eq!(sc.stages.len(), 3);
            assert!(matches!(sc.stages[0], PageStage::OrderBy(_)));
            assert!(matches!(sc.stages[1], PageStage::Skip(_)));
            assert!(matches!(sc.stages[2], PageStage::Limit(_)));
        }
        other => panic!("expected StandaloneOrderBy clause, got {other:?}"),
    }
}

#[test]
fn test_standalone_page_preserves_written_stage_order() {
    // Deviation: Neo4j accepts LIMIT before OFFSET/ORDER BY and applies
    // stages sequentially in the order written, not a fixed grammar
    // order -- the AST must record that order.
    let stmt = parse("MATCH (n) LIMIT 3 OFFSET 1 RETURN n").unwrap();
    match &stmt.clauses[1] {
        Clause::StandaloneOrderBy(sc) => {
            assert_eq!(sc.stages.len(), 2);
            assert!(matches!(sc.stages[0], PageStage::Limit(_)));
            assert!(matches!(sc.stages[1], PageStage::Skip(_)));
        }
        other => panic!("expected StandaloneOrderBy clause, got {other:?}"),
    }
}

#[test]
fn test_offset_is_a_skip_synonym_everywhere() {
    // Standalone clause.
    match &parse("MATCH (n) OFFSET 1 RETURN n").unwrap().clauses[1] {
        Clause::StandaloneOrderBy(sc) => assert!(matches!(sc.stages[0], PageStage::Skip(_))),
        other => panic!("expected StandaloneOrderBy clause, got {other:?}"),
    }
    // Attached to WITH.
    match &parse("MATCH (n) WITH n OFFSET 1 RETURN n").unwrap().clauses[1] {
        Clause::With(wc) => assert!(wc.skip.is_some()),
        other => panic!("expected With clause, got {other:?}"),
    }
    // Attached to RETURN.
    match &parse("MATCH (n) RETURN n OFFSET 1").unwrap().clauses[1] {
        Clause::Return(rc) => assert!(rc.skip.is_some()),
        other => panic!("expected Return clause, got {other:?}"),
    }
    // `offset` used as a property name (non-reserved) keeps working.
    assert!(parse("MATCH (n) RETURN n.offset AS x").is_ok());
}

/// CIP-236: the four spellings of `<group by clause>` plus the two
/// disambiguation guards (a parenthesized key must not parse as the empty
/// grouping set; `all(...)` must not parse as the `ALL` keyword).
#[test]
fn test_group_by_spec_forms() {
    // Keys.
    assert_eq!(
        return_group_by("UNWIND [1,1,2] AS x RETURN x, count(*) AS c GROUP BY x"),
        Some(GroupBySpec::Keys(vec![Expr::Variable("x".into())]))
    );

    // Empty grouping set: as the clause tail, and before ORDER BY.
    assert_eq!(
        return_group_by("UNWIND [1,2] AS x RETURN count(*) AS c GROUP BY ()"),
        Some(GroupBySpec::Empty)
    );
    assert_eq!(
        return_group_by("UNWIND [1,2] AS x RETURN count(*) AS c GROUP BY () ORDER BY c"),
        Some(GroupBySpec::Empty)
    );

    // ALL.
    assert_eq!(
        return_group_by("UNWIND [1,2] AS x RETURN x, count(*) AS c GROUP BY ALL"),
        Some(GroupBySpec::All)
    );

    // `(x)` is a parenthesized key, not the empty grouping set.
    let paren = return_group_by("UNWIND [1,2] AS x RETURN x + 0 AS k, count(*) AS c GROUP BY (x)");
    assert_eq!(
        paren,
        Some(GroupBySpec::Keys(vec![Expr::Paren(Box::new(
            Expr::Variable("x".into())
        ))]))
    );

    // `all(...)` is a function call named `all`, not the ALL grouping set.
    let all_call =
        return_group_by("UNWIND [1,2] AS x RETURN all(x) AS a, count(*) AS c GROUP BY all(x)");
    match all_call {
        Some(GroupBySpec::Keys(keys)) => {
            assert_eq!(keys.len(), 1);
            match &keys[0] {
                Expr::FunctionCall(f) => assert_eq!(f.name.to_ascii_lowercase(), "all"),
                other => panic!("expected all(...) function call, got {other:?}"),
            }
        }
        other => panic!("expected a single key, got {other:?}"),
    }
}

/// CIP-236: `GROUP BY ()` immediately before a subquery's closing brace
/// is the empty grouping set -- `RightBrace` must end the grouping-key
/// clause. Without it the `()` parses as a parenthesized key and fails.
#[test]
fn test_group_by_empty_before_load_csv_and_drop() {
    // Round-5 review: the clause-boundary list missed LOAD and DROP, so
    // `GROUP BY ()` before them mis-parsed as a parenthesized key.
    for q in [
        "UNWIND [1,2] AS x WITH count(*) AS c GROUP BY () LOAD CSV FROM 'x.csv' AS row RETURN row",
        "UNWIND [1,2] AS x WITH count(*) AS c GROUP BY () DROP INDEX nope",
    ] {
        let stmt = parse(q).unwrap();
        let grouped = stmt
            .clauses
            .iter()
            .find_map(|c| match c {
                Clause::With(w) => Some(w.group_by.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(grouped, Some(GroupBySpec::Empty), "query: {q}");
    }
}

#[test]
fn test_group_by_empty_before_subquery_close() {
    let stmt =
        parse("CALL { UNWIND [1,2] AS x RETURN count(*) AS c GROUP BY () } RETURN c").unwrap();
    let Clause::CallSubquery(call) = &stmt.clauses[0] else {
        panic!("expected CallSubquery as the first clause");
    };
    let Clause::Return(inner) = call.statement.clauses.last().unwrap() else {
        panic!("expected the subquery to end in RETURN");
    };
    assert_eq!(inner.group_by, Some(GroupBySpec::Empty));
}
