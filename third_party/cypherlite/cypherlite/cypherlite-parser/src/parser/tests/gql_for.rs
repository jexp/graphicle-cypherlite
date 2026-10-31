//! CIP-250 (GQ10) parse tests: `FOR <variable> IN <expression>` — the
//! GQL-compliant successor spelling of `UNWIND <expression> AS <variable>`,
//! normalized into the same UNWIND clause at parse time (`for_syntax: true`)
//! so everything downstream is shared. Accept shapes from the CIP's own
//! example table, reject its error shapes, and pin the normalization
//! (identical AST to the UNWIND spelling, spelling flag preserved).

use super::*;

/// The one shared accept assertion: the query parses, its first clause is
/// the normalized UNWIND with the given alias and `for_syntax: true`.
fn for_parses_as_unwind(query: &str, alias: &str) {
    let stmt = parse(query).unwrap();
    match &stmt.clauses[0] {
        Clause::Unwind(u) => {
            assert_eq!(u.alias, alias, "{query}");
            assert!(u.for_syntax, "{query}: expected the FOR spelling flag");
        }
        other => panic!("{query}: expected a normalized FOR clause, got {other:?}"),
    }
}

#[test]
fn test_parse_for_accept_shapes() {
    // The CIP-250 example table's accepting shapes, one per grammar slot:
    // a bare list, a variable source, a function call (range), a non-list
    // value (the singleton coercion happens at runtime, not parse), null,
    // and chained FOR clauses.
    for_parses_as_unwind("FOR x IN [1, 2, 3] RETURN x", "x");
    for_parses_as_unwind("FOR x IN null RETURN x", "x");
    for_parses_as_unwind("FOR x IN 42 RETURN x", "x");
    for_parses_as_unwind("FOR friend IN n.friends RETURN friend", "friend");
    let stmt = parse("WITH 3 AS n FOR x IN range(1, n) RETURN x").unwrap();
    assert!(matches!(&stmt.clauses[1], Clause::Unwind(u) if u.for_syntax && u.alias == "x"));
    for_parses_as_unwind("FOR x IN [[1, 2], [3, 4]] FOR y IN x RETURN y", "x");
    // Valid wherever UNWIND is valid: after MATCH, inside a CALL subquery,
    // inside a FOREACH body, after WITH.
    let stmt = parse("MATCH (n:Person) FOR tag IN n.tags RETURN tag").unwrap();
    assert!(matches!(&stmt.clauses[1], Clause::Unwind(u) if u.for_syntax));
    let stmt = parse("WITH [1, 2, 3] AS list FOR x IN list RETURN x").unwrap();
    assert!(matches!(&stmt.clauses[1], Clause::Unwind(u) if u.for_syntax));
    let stmt = parse("CALL { FOR x IN [1, 2, 3] RETURN x } RETURN x").unwrap();
    assert!(matches!(&stmt.clauses[0], Clause::CallSubquery(_)));
    let stmt = parse("FOREACH (i IN [1] | FOR x IN [1, 2] CREATE (:N {v: x}))").unwrap();
    assert!(matches!(&stmt.clauses[0], Clause::Foreach(_)));
}

#[test]
fn test_parse_for_reject_shapes() {
    // The CIP-250 example table's error shapes: a missing IN, a missing
    // binding variable, and the UNWIND-shaped `FOR <expr> AS <var>` (the
    // binding variable must precede the expression). The IN-less
    // `FOR x [1, 2, 3]` fails on the expected-IN read; the missing variable
    // `FOR IN [1, 2, 3]` and the AS form `FOR [1, 2, 3] AS x` both fail on
    // the binding-variable read (`[`/`AS` are not symbolic names).
    for query in [
        "FOR x [1, 2, 3] RETURN x",
        "FOR IN [1, 2, 3] RETURN x",
        "FOR [1, 2, 3] AS x RETURN x",
        "FOR x AS [1, 2, 3] RETURN x",
    ] {
        let err = parse(query).unwrap_err();
        assert!(!err.message.is_empty(), "{query} parsed");
    }
}

#[test]
fn test_parse_for_normalizes_to_the_unwind_ast() {
    // CIP-250's own rule: FOR reuses the UNWIND infrastructure — the two
    // spellings of the same statement produce the identical clause AST
    // except the spelling flag.
    let for_stmt = parse("FOR x IN [1, 2, 3] RETURN x").unwrap();
    let unwind_stmt = parse("UNWIND [1, 2, 3] AS x RETURN x").unwrap();
    let Clause::Unwind(for_clause) = &for_stmt.clauses[0] else {
        panic!("expected a normalized FOR clause");
    };
    let Clause::Unwind(unwind_clause) = &unwind_stmt.clauses[0] else {
        panic!("expected an UNWIND clause");
    };
    assert_eq!(for_clause.expr, unwind_clause.expr);
    assert_eq!(for_clause.alias, unwind_clause.alias);
    assert!(for_clause.for_syntax);
    assert!(!unwind_clause.for_syntax);
    // The rest of the statement is untouched: the RETURN clauses match.
    assert_eq!(for_stmt.clauses[1], unwind_stmt.clauses[1]);
}

#[test]
fn test_parse_unwind_keeps_its_own_spelling() {
    let stmt = parse("UNWIND [1, 2, 3] AS x RETURN x").unwrap();
    match &stmt.clauses[0] {
        Clause::Unwind(u) => {
            assert!(!u.for_syntax, "UNWIND must not gain the FOR flag");
        }
        _ => panic!("expected unwind"),
    }
}

#[test]
fn test_parse_for_binding_variable_accepts_keyword_spellings() {
    // ADR-0191: the binding variable is a symbolic name, so any keyword
    // spelling works unquoted — the same slot as the FOREACH loop variable
    // and the UNWIND alias.
    for_parses_as_unwind("FOR for IN [1, 2] RETURN for", "for");
    for_parses_as_unwind("FOR end IN [1, 2] RETURN end", "end");
}

#[test]
fn test_parse_for_terminates_a_group_by_key_list() {
    // The GROUP BY disambiguation list gained FOR as a clause starter: a
    // grouping-key expression list ends before it exactly as before
    // UNWIND, instead of mis-reading the `(` or `[` of the FOR source as a
    // parenthesized key.
    let stmt =
        parse("UNWIND [1, 1, 2] AS x RETURN x, count(*) AS c GROUP BY (x) FOR y IN [1] RETURN y")
            .unwrap();
    assert!(matches!(&stmt.clauses[2], Clause::Unwind(u) if u.for_syntax));
}
