use super::*;

// ---- CIP-146 GQL top-level braces ----

#[test]
fn test_brace_enclosed_top_level_query_is_transparent() {
    // `{ RETURN 1 AS x }` should parse to exactly the same AST as
    // `RETURN 1 AS x` (Semantics Step 3, Unwrap Case 1).
    let braced = parse("{ RETURN 1 AS x }").unwrap();
    let plain = parse("RETURN 1 AS x").unwrap();
    assert_eq!(braced, plain);
}

#[test]
fn test_nested_braces_collapse_to_same_ast() {
    let once = parse("{ RETURN 1 AS x }").unwrap();
    let twice = parse("{ { RETURN 1 AS x } }").unwrap();
    let thrice = parse("{ { { RETURN 1 AS x } } }").unwrap();
    assert_eq!(once, twice);
    assert_eq!(once, thrice);
}

#[test]
fn test_empty_braces_are_a_syntax_error() {
    assert!(parse("{ }").is_err());
}

#[test]
fn test_braced_union_arms_splice_transparently_when_linear() {
    // A linear brace body used as a UNION argument is spliced directly
    // (no `CALL (*) { ... }` wrap needed -- see
    // `Parser::maybe_wrap_composite_union_arm`).
    let braced = parse("{ RETURN 1 AS x } UNION { RETURN 2 AS x }").unwrap();
    let plain = parse("RETURN 1 AS x UNION RETURN 2 AS x").unwrap();
    assert_eq!(braced, plain);
}

#[test]
fn test_mixed_braced_and_plain_union_arms() {
    assert_eq!(
        parse("{ RETURN 1 AS x } UNION RETURN 2 AS x").unwrap(),
        parse("RETURN 1 AS x UNION RETURN 2 AS x").unwrap()
    );
    assert_eq!(
        parse("RETURN 1 AS x UNION { RETURN 2 AS x }").unwrap(),
        parse("RETURN 1 AS x UNION RETURN 2 AS x").unwrap()
    );
}

#[test]
fn test_composite_union_arm_is_rewrapped_in_call_star() {
    // A composite (nested-union) brace body used as one UNION operand
    // (Semantics Step 3, Case 2.2) is rewritten to
    // `CALL (*) { BODY } RETURN *`.
    let stmt =
        parse("{ RETURN 1 AS x } UNION { { RETURN 2 AS x } UNION { RETURN 3 AS x } }").unwrap();
    let second_arm = &stmt.union.as_ref().unwrap().statement;
    assert_eq!(second_arm.clauses.len(), 2);
    match &second_arm.clauses[0] {
        Clause::CallSubquery(csq) => {
            assert_eq!(csq.scope, CallScope::Wildcard);
            assert!(csq.statement.union.is_some());
        }
        other => panic!("expected CallSubquery clause, got {other:?}"),
    }
    assert!(matches!(&second_arm.clauses[1], Clause::Return(rc) if rc.items[0].all));
}

#[test]
fn test_composite_union_arm_with_omitted_result_wraps_with_finish() {
    // The omitted-result sub-case of Case 2.2: every leaf arm has no
    // RETURN, so the rewrap ends in FINISH rather than RETURN *.
    let stmt = parse("{ { CREATE (:A) } UNION { CREATE (:B) } } UNION { CREATE (:C) }").unwrap();
    match &stmt.clauses[0] {
        Clause::CallSubquery(csq) => {
            assert_eq!(csq.scope, CallScope::Wildcard);
            assert!(csq.statement.union.is_some());
        }
        other => panic!("expected CallSubquery clause, got {other:?}"),
    }
    assert_eq!(stmt.clauses[1], Clause::Finish);
}

#[test]
fn test_use_prefixed_brace_pushes_use_into_body() {
    let stmt = parse("USE neo4j { RETURN 1 AS x }").unwrap();
    assert_eq!(stmt.clauses.len(), 2);
    assert!(matches!(stmt.clauses[0], Clause::Use(_)));
    assert!(matches!(&stmt.clauses[1], Clause::Return(_)));
}

#[test]
fn test_use_prefixed_brace_over_composite_body_distributes_into_every_arm() {
    // Semantics Step 2 (Normalize), Case 2: `USE g { Q1 UNION ALL Q2 }`
    // pushes the USE into each arm.
    let stmt = parse("USE neo4j { { RETURN 1 AS x } UNION ALL { RETURN 1 AS x } }").unwrap();
    assert!(matches!(stmt.clauses[0], Clause::Use(_)));
    let second = &stmt.union.as_ref().unwrap().statement;
    assert!(matches!(second.clauses[0], Clause::Use(_)));
}

#[test]
fn test_use_brace_nested_inside_use_brace_collapses() {
    // Simplify Case 3: a same-graph nested USE-brace collapses to a
    // single leading USE clause (two USE clauses would otherwise stack).
    let stmt = parse("USE neo4j { USE neo4j { RETURN 1 AS x } }").unwrap();
    assert_eq!(stmt.clauses.len(), 3);
    assert!(matches!(stmt.clauses[0], Clause::Use(_)));
    assert!(matches!(stmt.clauses[1], Clause::Use(_)));
    assert!(matches!(&stmt.clauses[2], Clause::Return(_)));
}

#[test]
fn test_braces_rejected_after_with() {
    assert!(parse("WITH 1 AS a { RETURN a }").is_err());
}

#[test]
fn test_braces_rejected_as_load_csv_body() {
    assert!(
        parse("LOAD CSV FROM 'file:///x.csv' AS line { CREATE (:Artist {name: line[0]}) }")
            .is_err()
    );
}

#[test]
fn test_brace_inside_call_subquery_top_level_is_transparent() {
    let stmt = parse("CALL () { { RETURN 1 AS x } } RETURN x").unwrap();
    let plain = parse("CALL () { RETURN 1 AS x } RETURN x").unwrap();
    assert_eq!(stmt, plain);
}
