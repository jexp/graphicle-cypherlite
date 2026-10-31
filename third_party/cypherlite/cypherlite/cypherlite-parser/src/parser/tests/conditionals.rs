use super::*;

// -- CIP-135: WHEN/THEN/ELSE conditional clause --

#[test]
fn test_when_then_else_parses_as_sole_conditional_clause() {
    let stmt = parse(
        "WHEN true THEN { RETURN 1 AS n } WHEN false THEN { RETURN 2 AS n } ELSE { RETURN 3 AS n }",
    )
    .unwrap();
    assert_eq!(stmt.clauses.len(), 1);
    match &stmt.clauses[0] {
        Clause::Conditional(cc) => {
            assert_eq!(cc.branches.len(), 2);
            assert!(cc.else_body.is_some());
        }
        other => panic!("expected Conditional clause, got {other:?}"),
    }
}

#[test]
fn test_when_without_else_is_ok() {
    let stmt = parse("WHEN true THEN { RETURN 1 AS n }").unwrap();
    match &stmt.clauses[0] {
        Clause::Conditional(cc) => {
            assert_eq!(cc.branches.len(), 1);
            assert!(cc.else_body.is_none());
        }
        other => panic!("expected Conditional clause, got {other:?}"),
    }
}

#[test]
fn test_bare_when_mid_statement_is_a_syntax_error() {
    // A `WHEN` at clause-start is only a `<when query>` at the very
    // start of a `<regular query>` -- after MATCH it must fall through
    // to `parse_clause_inner`'s catch-all.
    assert!(parse("MATCH (n) WHEN true THEN { RETURN 1 AS s } RETURN n").is_err());
}

#[test]
fn test_failing_when_branch_body_restores_in_when_branch_body() {
    for q in [
        "WHEN true THEN MATCH (n) RETURN",
        "WHEN true THEN RETURN 1 AS n ELSE MATCH (n) RETURN",
    ] {
        let mut parser = Parser::new(q, tokenize(q));
        assert!(parser.parse_when_query().is_err(), "{q:?}");
        assert_eq!(parser.in_when_branch_body, 0, "{q:?}");
    }
}

#[test]
fn test_lone_else_with_no_when_is_a_syntax_error() {
    assert!(parse("ELSE { RETURN 1 AS n }").is_err());
}

#[test]
fn test_when_after_else_in_same_chain_is_a_syntax_error() {
    assert!(parse(
        "WHEN true THEN { RETURN 1 AS n } ELSE { RETURN 2 AS n } WHEN true THEN { RETURN 3 AS n }"
    )
    .is_err());
}

#[test]
fn test_bare_when_chain_cannot_be_unioned() {
    assert!(
        parse("WHEN true THEN { RETURN 1 AS n } UNION WHEN true THEN { RETURN 2 AS n }").is_err()
    );
}

#[test]
fn test_braced_when_chain_can_be_unioned() {
    let stmt =
        parse("{ WHEN true THEN { RETURN 1 AS n } } UNION { WHEN false THEN { RETURN 2 AS n } }")
            .unwrap();
    assert!(matches!(stmt.clauses[0], Clause::Conditional(_)));
    assert!(stmt.union.is_some());
}

#[test]
fn test_two_adjacent_braced_when_blocks_with_no_connector_is_an_error() {
    assert!(parse(
        "{ WHEN true THEN { RETURN 1 AS n } } { WHEN true THEN { RETURN 2 AS n } } RETURN n"
    )
    .is_err());
}

#[test]
fn test_when_inside_call_subquery_parses() {
    let stmt =
        parse("CALL () { WHEN true THEN { RETURN 1 AS s } ELSE { RETURN 2 AS s } } RETURN s")
            .unwrap();
    match &stmt.clauses[0] {
        Clause::CallSubquery(csq) => {
            assert!(matches!(csq.statement.clauses[0], Clause::Conditional(_)));
        }
        other => panic!("expected CallSubquery clause, got {other:?}"),
    }
}
