use super::*;

// ---- CIP-213 NEXT ----

#[test]
fn test_next_chains_two_stages() {
    let stmt = parse("RETURN 1 AS a NEXT RETURN a + 1 AS b").unwrap();
    assert_eq!(stmt.clauses.len(), 1);
    let next = stmt.next.as_deref().expect("expected a .next stage");
    assert_eq!(next.clauses.len(), 1);
    assert!(next.next.is_none());
}

#[test]
fn test_next_chains_three_stages() {
    let stmt = parse("RETURN 1 AS a NEXT RETURN a AS b NEXT RETURN b AS c").unwrap();
    let stage1 = stmt.next.as_deref().expect("stage 1");
    let stage2 = stage1.next.as_deref().expect("stage 2");
    assert!(stage2.next.is_none());
}

#[test]
fn test_next_is_a_non_reserved_word_usable_as_a_variable() {
    // NEXT is only recognized at a statement-block boundary, never
    // inside `parse_clause_inner` -- so it stays usable as an
    // ordinary variable/label name everywhere else.
    let stmt = parse("MATCH (next) RETURN next").unwrap();
    assert_eq!(stmt.clauses.len(), 2);
}

#[test]
fn test_next_bare_next_alone_is_a_syntax_error() {
    assert!(parse("NEXT").is_err());
}

#[test]
fn test_next_leading_next_before_first_statement_is_a_syntax_error() {
    assert!(parse("NEXT LET a = 1 RETURN a").is_err());
}

#[test]
fn test_next_trailing_dangling_next_is_a_syntax_error() {
    assert!(parse("LET a = 1 RETURN a NEXT").is_err());
}

#[test]
fn test_next_double_next_is_a_syntax_error() {
    assert!(parse("RETURN 1 AS a NEXT NEXT RETURN a AS b").is_err());
}

#[test]
fn test_next_bare_when_chain_after_next_no_braces() {
    // CIP-213 PE3.4: a bare (non-brace) WHEN/THEN/ELSE chain
    // immediately following NEXT, with bare (non-brace) branch bodies.
    let stmt =
        parse("RETURN 1 AS a NEXT WHEN a = 1 THEN RETURN 2 AS a ELSE RETURN 3 AS a").unwrap();
    let next = stmt.next.as_deref().expect("expected a .next stage");
    match &next.clauses[0] {
        Clause::Conditional(cc) => {
            assert_eq!(cc.branches.len(), 1);
            assert!(cc.else_body.is_some());
        }
        other => panic!("expected Conditional clause, got {other:?}"),
    }
}

#[test]
fn test_next_bare_multi_when_chain_after_next_no_braces() {
    // A bare multi-WHEN chain's own branch bodies must stop at the
    // following `WHEN` (not misparse it as a new clause), while a
    // bare mid-clause WHEN (e.g. after MATCH, outside any WHEN body)
    // must still be a hard error -- see the next two tests.
    let stmt = parse(
        "RETURN 1 AS a NEXT WHEN a > 2 THEN RETURN 1 AS m WHEN a > 1 THEN RETURN 2 AS m ELSE RETURN 3 AS m",
    )
    .unwrap();
    let next = stmt.next.as_deref().expect("expected a .next stage");
    match &next.clauses[0] {
        Clause::Conditional(cc) => assert_eq!(cc.branches.len(), 2),
        other => panic!("expected Conditional clause, got {other:?}"),
    }
}

#[test]
fn test_bare_mid_clause_when_after_match_is_still_a_hard_error() {
    // CIP-135 syntax_placement.feature [8]: unrelated to the
    // NEXT-branch-body WHEN-stop-token mechanism above -- a WHEN
    // appearing after an ordinary clause, outside any WHEN-branch
    // body, must remain a parse error.
    assert!(parse("MATCH (n) WHEN true THEN { RETURN 1 AS s } RETURN n").is_err());
}

#[test]
fn test_next_call_scope_body_parses_as_next_chain() {
    let stmt = parse("CALL (a) { RETURN a + 1 AS b NEXT RETURN a + b AS c } RETURN c").unwrap();
    match &stmt.clauses[0] {
        Clause::CallSubquery(csq) => {
            assert!(csq.statement.next.is_some());
        }
        other => panic!("expected CallSubquery clause, got {other:?}"),
    }
}

#[test]
fn test_next_forbidden_inside_naked_importing_with_call() {
    // Rule 4: NEXT cannot be used inside a CALL subquery that uses the
    // deprecated importing WITH syntax.
    assert!(
        parse("WITH 1 AS x CALL { WITH x RETURN x AS y NEXT RETURN y + 1 AS z } RETURN z").is_err()
    );
}

#[test]
fn test_next_allowed_inside_naked_call_without_importing_with() {
    // Rule 4 only forbids the *importing WITH* shape specifically --
    // an ordinary naked CALL body (whole outer row implicitly visible,
    // no leading `WITH`) combines with NEXT freely.
    assert!(parse("CALL { RETURN 1 AS a NEXT RETURN a + 1 AS b } RETURN b").is_ok());
}

#[test]
fn test_next_top_level_brace_block_own_chain_splices_into_outer_next() {
    let stmt = parse("{ RETURN 1 AS a NEXT RETURN a + 1 AS b } NEXT RETURN b + 1 AS c").unwrap();
    // The brace's own internal chain and the outer NEXT must form one
    // seamless 3-stage chain (not two separate `.next` trees), since
    // brace unwrapping is transparent (CIP-146).
    let stage1 = stmt.next.as_deref().expect("stage 1 (brace's own NEXT)");
    let stage2 = stage1.next.as_deref().expect("stage 2 (outer NEXT)");
    assert!(stage2.next.is_none());
}
