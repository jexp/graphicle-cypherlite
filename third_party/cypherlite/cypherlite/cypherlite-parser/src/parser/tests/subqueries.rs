use super::*;

#[test]
fn test_parse_call_optional_procedure() {
    let stmt = parse("OPTIONAL CALL db.labels() YIELD label RETURN label").unwrap();
    match &stmt.clauses[0] {
        Clause::CallProcedure(cp) => assert!(cp.optional),
        _ => panic!("expected call procedure"),
    }
}

#[test]
fn test_parse_call_optional_subquery() {
    let stmt = parse("OPTIONAL CALL { RETURN 1 AS x } RETURN x").unwrap();
    match &stmt.clauses[0] {
        Clause::CallSubquery(csq) => assert!(csq.optional),
        _ => panic!("expected call subquery"),
    }
}

#[test]
fn test_parse_optional_before_call_or_match_only() {
    assert!(parse("MATCH (p) OPTIONAL RETURN p").is_err());
    assert!(parse("MATCH (p) CALL OPTIONAL { RETURN 1 AS x } RETURN p, x").is_err());
    assert!(parse("MATCH (p) OPTIONAL OPTIONAL CALL { RETURN 1 AS x } RETURN p, x").is_err());
}

#[test]
fn test_parse_call_where_in_yield() {
    let stmt =
        parse("OPTIONAL CALL db.labels() YIELD label WHERE label = 'X' RETURN label").unwrap();
    match &stmt.clauses[0] {
        Clause::CallProcedure(cp) => assert!(cp.where_expr.is_some()),
        _ => panic!("expected call procedure"),
    }
}

#[test]
fn test_parse_call_subquery() {
    let stmt = parse("CALL { MATCH (n) RETURN n }").unwrap();
    match &stmt.clauses[0] {
        Clause::CallSubquery(c) => {
            assert_eq!(c.statement.clauses.len(), 2);
        }
        _ => panic!("expected call subquery"),
    }
}

#[test]
fn test_parse_call_procedure() {
    let stmt = parse("CALL db.labels() YIELD label").unwrap();
    match &stmt.clauses[0] {
        Clause::CallProcedure(c) => {
            assert_eq!(c.procedure, "db.labels");
            assert_eq!(c.yields.names().collect::<Vec<_>>(), vec!["label"]);
        }
        _ => panic!("expected call procedure"),
    }
}

#[test]
fn test_parse_exists_subquery() {
    let stmt = parse("MATCH (n) WHERE EXISTS { MATCH (n)-[:KNOWS]->() } RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match m.where_expr.as_ref().unwrap() {
            Expr::ExistsSubquery(_) => {}
            _ => panic!("expected exists subquery"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_exists_subquery_with_union() {
    // Batch-1 regression coverage: `parse_regular_query` must be reused
    // inside EXISTS/COUNT/COLLECT bodies so a UNION chain between the
    // braces parses instead of erroring on "expected RightBrace, got
    // UNION" (CIP-63 union scenarios). Uses UNION ALL with RETURN
    // omitted in both branches -- CIP-63/66's consistency rule (see
    // `test_parse_count_subquery_union_omission_consistency_error`)
    // forbids a plain (distinct) UNION from omitting RETURN at all.
    let stmt = parse(
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:X]->(b) UNION ALL MATCH (a)<-[:Y]-(c) } RETURN a",
    )
    .unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match m.where_expr.as_ref().unwrap() {
            Expr::ExistsSubquery(esq) => {
                assert!(esq.statement.union.is_some());
            }
            _ => panic!("expected exists subquery"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_count_subquery_with_union() {
    let stmt = parse("RETURN COUNT { MATCH (a) RETURN a UNION ALL MATCH (b) RETURN b AS a } AS c")
        .unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::CountSubquery(csq) => {
                let union = csq.statement.union.as_ref().expect("expected union");
                assert!(union.all);
            }
            _ => panic!("expected count subquery"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_count_empty_braces_is_error() {
    assert!(parse("RETURN COUNT { } AS n").is_err());
}

#[test]
fn test_parse_count_subquery_union_omission_consistency_error() {
    // CIP-66: mixing an explicit RETURN with an omitted one across UNION
    // ALL branches must fail to compile "for consistency".
    assert!(parse("RETURN COUNT { MATCH (m) RETURN 1 UNION ALL MATCH (p) } > 0 AS r").is_err());
}

#[test]
fn test_parse_collect_subquery_basic() {
    let stmt =
        parse("MATCH (p) RETURN COLLECT { MATCH (p)-[:F]->(f) RETURN f.name } AS xs").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::CollectSubquery(csq) => {
                assert_eq!(csq.statement.clauses.len(), 2);
            }
            _ => panic!("expected collect subquery"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_collect_subquery_with_union() {
    let stmt = parse(
        "RETURN COLLECT { MATCH (a) RETURN a.name AS name UNION MATCH (b) RETURN b.name AS name } AS xs",
    )
    .unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::CollectSubquery(csq) => {
                assert!(csq.statement.union.is_some());
            }
            _ => panic!("expected collect subquery"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_collect_empty_braces_is_error() {
    assert!(parse("RETURN COLLECT { } AS xs").is_err());
}

#[test]
fn test_parse_collect_requires_single_return_item() {
    assert!(parse("RETURN COLLECT { RETURN 1, 2 } AS xs").is_err());
}

#[test]
fn test_parse_collect_rejects_return_star() {
    assert!(parse("MATCH (p) RETURN COLLECT { MATCH (p)-->(f) RETURN * } AS xs").is_err());
}

#[test]
fn test_parse_collect_requires_trailing_return() {
    assert!(parse("MATCH (p) RETURN COLLECT { MATCH (p)-->(f) } AS xs").is_err());
}

#[test]
fn test_parse_collect_union_branch_must_have_single_item_too() {
    assert!(parse("RETURN COLLECT { RETURN 1 AS a UNION RETURN 1 AS a, 2 AS b } AS xs").is_err());
}

#[test]
fn test_parse_collect_as_a_function_call_still_works() {
    // `collect` must keep working as the ordinary aggregation function
    // now that COLLECT is a keyword token.
    let stmt = parse("MATCH (n) RETURN collect(n.name) AS names").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::FunctionCall(fc) => assert_eq!(fc.name.to_lowercase(), "collect"),
            _ => panic!("expected function call"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_exists_simple_pattern() {
    let stmt = parse("MATCH (n) WHERE EXISTS { (n)-->() } RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match m.where_expr.as_ref().unwrap() {
            Expr::ExistsSubquery(e) => {
                assert_eq!(e.statement.clauses.len(), 1);
            }
            _ => panic!("expected exists subquery"),
        },
        _ => panic!("expected match"),
    }
}

// ---- CIP-141 CALL variable scope clause ----

#[test]
fn test_call_scope_clause_forms() {
    let cases: &[(&str, CallScope)] = &[
        (
            "CALL () { RETURN 1 AS x } RETURN x",
            CallScope::Vars(vec![]),
        ),
        (
            "WITH 1 AS a CALL (a) { RETURN a AS b } RETURN b",
            CallScope::Vars(vec!["a".to_string()]),
        ),
        (
            "WITH 1 AS a, 2 AS b CALL (a, b) { RETURN a + b AS c } RETURN c",
            CallScope::Vars(vec!["a".to_string(), "b".to_string()]),
        ),
        (
            "WITH 1 AS a CALL (*) { RETURN a AS b } RETURN b",
            CallScope::Wildcard,
        ),
        // Naked CALL still parses with `scope: None`.
        ("CALL { RETURN 1 AS x } RETURN x", CallScope::None),
    ];
    for (q, want) in cases {
        assert_eq!(call_subquery_of(&parse_ok(q)).scope, *want);
    }
}

#[test]
fn test_call_scope_clause_syntax_errors() {
    // Wildcard mixed with references, in either order.
    assert!(parse("CALL (*, a) { RETURN 1 AS x } RETURN x").is_err());
    assert!(parse("CALL (a, *) { RETURN 1 AS x } RETURN x").is_err());
    // A non-variable token in the scope clause.
    assert!(parse("CALL (1) { RETURN 1 AS x } RETURN x").is_err());
    // A variable declaration is forbidden in a scope clause.
    assert!(parse("CALL (1 AS a) { RETURN 1 AS x } RETURN x").is_err());
}

// ---- CIP-32/CIP-34/CIP-198/CIP-30 CALL ... IN TRANSACTIONS ----

#[test]
fn test_in_transactions_absent_by_default() {
    assert!(
        call_subquery_of(&parse_ok("CALL { RETURN 1 AS x } RETURN x"))
            .in_transactions
            .is_none()
    );
}

#[test]
fn test_in_transactions_basic_forms() {
    // Bare `IN TRANSACTIONS`, no batching modifier.
    let stmt = parse_ok("CALL { RETURN 1 AS x } IN TRANSACTIONS RETURN x");
    let cit = call_subquery_of(&stmt)
        .in_transactions
        .as_ref()
        .expect("IN TRANSACTIONS");
    assert_eq!(cit.concurrency, Concurrency::Sequential);
    assert!(cit.batch_size.is_none());
    assert_eq!(cit.on_error, OnErrorMode::Fail);
    assert!(cit.report_status.is_none());

    // With a variable scope clause too.
    assert!(call_subquery_of(&parse_ok(
        "CALL () { RETURN 1 AS x } IN TRANSACTIONS RETURN x"
    ))
    .in_transactions
    .is_some());

    // `OF n ROWS` and the singular `OF 1 ROW` form.
    let stmt = parse_ok("CALL { RETURN 1 AS x } IN TRANSACTIONS OF 5 ROWS RETURN x");
    let cit = call_subquery_of(&stmt)
        .in_transactions
        .as_ref()
        .expect("IN TRANSACTIONS");
    assert_eq!(
        cit.batch_size,
        Some(Expr::Literal(LiteralValue::Integer(5)))
    );
    assert!(parse("CALL { RETURN 1 AS x } IN TRANSACTIONS OF 1 ROW RETURN x").is_ok());
    // A parameter batch size.
    assert!(parse("CALL { RETURN 1 AS x } IN TRANSACTIONS OF $n ROWS RETURN x").is_ok());
}

#[test]
fn test_in_transactions_concurrency_forms() {
    let cases: &[(&str, Concurrency)] = &[
        (
            "CALL { RETURN 1 AS x } IN CONCURRENT TRANSACTIONS RETURN x",
            Concurrency::DefaultUnspecified,
        ),
        (
            "CALL { RETURN 1 AS x } IN 2 CONCURRENT TRANSACTIONS RETURN x",
            Concurrency::Value(Expr::Literal(LiteralValue::Integer(2))),
        ),
    ];
    for (q, want) in cases {
        assert_eq!(
            call_subquery_of(&parse_ok(q))
                .in_transactions
                .as_ref()
                .expect("IN TRANSACTIONS")
                .concurrency,
            *want
        );
    }

    // An arbitrary expression is allowed as the concurrency value.
    assert!(
        parse("CALL { RETURN 1 AS x } IN 1 + 2 CONCURRENT TRANSACTIONS OF 2 ROWS RETURN x").is_ok()
    );
}

#[test]
fn test_in_transactions_on_error_forms() {
    for (query, expected) in [
        (
            "CALL { RETURN 1 AS x } IN TRANSACTIONS ON ERROR CONTINUE RETURN x",
            OnErrorMode::Continue,
        ),
        (
            "CALL { RETURN 1 AS x } IN TRANSACTIONS ON ERROR BREAK RETURN x",
            OnErrorMode::Break,
        ),
        (
            "CALL { RETURN 1 AS x } IN TRANSACTIONS ON ERROR FAIL RETURN x",
            OnErrorMode::Fail,
        ),
        // Bare RETRY: no duration, default (FAIL) fallback.
        (
            "CALL { RETURN 1 AS x } IN TRANSACTIONS ON ERROR RETRY RETURN x",
            OnErrorMode::Retry {
                duration: None,
                fallback: RetryFallback::Fail,
            },
        ),
        // `RETRY FOR <dur> SECONDS THEN BREAK`.
        (
            "CALL { RETURN 1 AS x } IN TRANSACTIONS ON ERROR RETRY FOR 2 SECONDS THEN BREAK RETURN x",
            OnErrorMode::Retry {
                duration: Some(Expr::Literal(LiteralValue::Integer(2))),
                fallback: RetryFallback::Break,
            },
        ),
        // `RETRY <dur> SECOND` with no `FOR` and no `THEN`.
        (
            "CALL { RETURN 1 AS x } IN TRANSACTIONS ON ERROR RETRY 1 SECOND RETURN x",
            OnErrorMode::Retry {
                duration: Some(Expr::Literal(LiteralValue::Integer(1))),
                fallback: RetryFallback::Fail,
            },
        ),
    ] {
        assert_eq!(
            call_subquery_of(&parse_ok(query))
                .in_transactions
                .as_ref()
                .expect("IN TRANSACTIONS")
                .on_error,
            expected
        );
    }
}

#[test]
fn test_in_transactions_report_status_and_ordering() {
    for q in [
        "CALL { RETURN 1 AS x } IN TRANSACTIONS ON ERROR CONTINUE REPORT STATUS AS s RETURN x",
        // REPORT STATUS may precede ON ERROR (CIP-34 concurrent-error-handling[7]).
        "CALL { RETURN 1 AS x } IN TRANSACTIONS REPORT STATUS AS s ON ERROR CONTINUE RETURN x",
    ] {
        let stmt = parse_ok(q);
        let cit = call_subquery_of(&stmt)
            .in_transactions
            .as_ref()
            .expect("IN TRANSACTIONS");
        assert_eq!(cit.on_error, OnErrorMode::Continue);
        assert_eq!(cit.report_status.as_deref(), Some("s"));
    }
}

#[test]
fn test_in_transactions_all_clauses_together() {
    let stmt = parse_ok(
        "CALL (x) { RETURN x } IN 2 CONCURRENT TRANSACTIONS OF 3 ROWS ON ERROR RETRY FOR 1 SECOND THEN CONTINUE REPORT STATUS AS s RETURN x, s",
    );
    let cit = call_subquery_of(&stmt)
        .in_transactions
        .as_ref()
        .expect("IN TRANSACTIONS");
    assert_eq!(
        cit.concurrency,
        Concurrency::Value(Expr::Literal(LiteralValue::Integer(2)))
    );
    assert_eq!(
        cit.batch_size,
        Some(Expr::Literal(LiteralValue::Integer(3)))
    );
    assert_eq!(
        cit.on_error,
        OnErrorMode::Retry {
            duration: Some(Expr::Literal(LiteralValue::Integer(1))),
            fallback: RetryFallback::Continue,
        }
    );
    assert_eq!(cit.report_status.as_deref(), Some("s"));
}
