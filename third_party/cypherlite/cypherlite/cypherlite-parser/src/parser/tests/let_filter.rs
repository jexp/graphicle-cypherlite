use super::*;

// ---- CIP-206 LET quirky-but-valid identifiers ----

#[test]
fn test_let_accepts_distinct_and_as_as_variable_names() {
    // CIP-206 PE6.1/PE6.2: LET's grammar has no modifier position after
    // the keyword, so normally-reserved words are valid binding names.
    // Both are only referenced via `RETURN *` here (matching CIP-206's
    // own PE6.1/PE6.2 scenarios) since a bare `AS`/`DISTINCT` token is
    // not accepted as a variable *reference* expression elsewhere --
    // only LET's own binding-name position is this permissive.
    let stmt = parse("LET DISTINCT = 1 RETURN *").unwrap();
    match &stmt.clauses[0] {
        Clause::With(wc) => assert_eq!(wc.items[1].alias.as_deref(), Some("DISTINCT")),
        other => panic!("expected With clause, got {other:?}"),
    }
    let stmt = parse("LET AS = 1 RETURN *").unwrap();
    match &stmt.clauses[0] {
        Clause::With(wc) => assert_eq!(wc.items[1].alias.as_deref(), Some("AS")),
        other => panic!("expected With clause, got {other:?}"),
    }
}

#[test]
fn test_let_wrong_syntax_is_rejected() {
    // NE1.1-NE1.4: not a valid `<binding variable> = <expr>` form.
    assert!(parse("LET 1 RETURN a").is_err());
    assert!(parse("MATCH (a) LET a RETURN a").is_err());
    assert!(parse("LET 1 AS a RETURN a").is_err());
    assert!(parse("LET * RETURN a").is_err());
}

// ---- CIP-203 FILTER clause ----

#[test]
fn test_filter_clause_desugars_to_with_star_where() {
    let stmt = parse("FILTER x > 2 RETURN x").unwrap();
    match &stmt.clauses[0] {
        Clause::With(wc) => {
            assert_eq!(wc.origin, WithOrigin::Filter);
            assert_eq!(wc.items.len(), 1);
            assert!(wc.items[0].all);
            assert!(wc.where_expr.is_some());
        }
        other => panic!("expected With clause, got {other:?}"),
    }
}

#[test]
fn test_filter_where_keyword_is_optional() {
    let with_where = parse("FILTER WHERE x > 2 RETURN x").unwrap();
    let without_where = parse("FILTER x > 2 RETURN x").unwrap();
    assert_eq!(with_where, without_where);
}

#[test]
fn test_bare_where_without_filter_is_a_syntax_error() {
    // CIP-203 NE1: WHERE has no clause-start dispatch of its own.
    assert!(parse("WHERE true RETURN 1 AS x").is_err());
}
