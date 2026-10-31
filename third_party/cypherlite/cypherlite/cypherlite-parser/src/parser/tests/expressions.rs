use super::*;
use cypherlite_test_support::run_with_big_stack;

// CIP-272 round-3 review: a `{` literal with no top-level `|` is a map
// literal, and the comprehension speculation must not full-parse it first.
// Before the lookahead in front of `finish_map_comprehension`, nesting like
// this doubled the parse work per level (measured on the pre-fix parser in
// a debug build: depth 12 took 6.5 ms, depth 16 took 105 ms, depth 20 took
// 1.7 s -- four times the work per two levels, so depth 50 never finishes).
// Depth 50 pins the shape as a bounded-time parse, and the counter staying
// at zero pins it as a single non-speculative pass. Parsing recurses once per
// nesting level, which overflows a default-sized (2 MiB) test thread before
// depth 50, so the parse runs on an explicitly sized stack -- the same reason
// the CIP-eval gate sets RUST_MIN_STACK for its binary.
#[test]
fn test_pipe_less_nesting_is_not_speculatively_parsed() {
    run_with_big_stack(|| {
        let depth = 50;
        let mut source = String::from("m");
        for _ in 0..depth {
            source = format!("{{a: b IN {source}}}");
        }
        let count = || super::SPECULATIVE_COMPREHENSION_PARSES.with(std::cell::Cell::get);
        let before = count();
        let expr =
            parse_expression_str(&source).expect("pipe-less nesting has to parse as a map literal");
        let speculative = count() - before;
        assert_eq!(
            speculative, 0,
            "no speculative comprehension parse may start for a pipe-less brace"
        );
        assert!(
            matches!(expr, Expr::Map(_)),
            "expected a map literal, got {expr:?}"
        );
    });
}

#[test]
fn test_parse_expressions() {
    // Arithmetic
    let stmt = parse("RETURN 1 + 2 * 3").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Binary(b) => {
                assert_eq!(b.op, "+");
                match &b.right {
                    Expr::Binary(b2) => assert_eq!(b2.op, "*"),
                    _ => panic!("expected binary"),
                }
            }
            _ => panic!("expected binary"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_comparison() {
    let stmt = parse("RETURN 1 = 1").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Comparison(c) => assert_eq!(c.op, "="),
            _ => panic!("expected comparison"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_boolean_ops() {
    let stmt = parse("RETURN true AND false OR NOT true").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Binary(b) => assert_eq!(b.op, "OR"),
            _ => panic!("expected binary"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_is_null() {
    let stmt = parse("MATCH (n) WHERE n.name IS NULL RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match m.where_expr.as_ref().unwrap() {
            Expr::IsNull(e) => assert!(!e.negated),
            _ => panic!("expected is null"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_is_not_null() {
    let stmt = parse("MATCH (n) WHERE n.name IS NOT NULL RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match m.where_expr.as_ref().unwrap() {
            Expr::IsNull(e) => assert!(e.negated),
            _ => panic!("expected is not null"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_in_expr() {
    let stmt = parse("RETURN 1 IN [1, 2, 3]").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::In(_) => {}
            _ => panic!("expected in expr"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_function_call() {
    let stmt = parse("RETURN toUpper('hello')").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::FunctionCall(f) => {
                assert_eq!(f.name, "toUpper");
                assert_eq!(f.args.len(), 1);
            }
            _ => panic!("expected function call"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_count_star() {
    let stmt = parse("RETURN count(*)").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::CountStar => {}
            _ => panic!("expected count star"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_case_expr() {
    let stmt = parse("RETURN CASE WHEN true THEN 1 ELSE 2 END").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Case(c) => {
                assert!(c.test.is_none());
                assert_eq!(c.whens.len(), 1);
                assert!(c.else_expr.is_some());
            }
            _ => panic!("expected case"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_simple_case() {
    let stmt = parse("RETURN CASE n.type WHEN 'A' THEN 1 WHEN 'B' THEN 2 END").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Case(c) => {
                assert!(c.test.is_some());
                assert_eq!(c.whens.len(), 2);
            }
            _ => panic!("expected case"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_parameter() {
    let stmt = parse("MATCH (n) WHERE n.name = $name RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match m.where_expr.as_ref().unwrap() {
            Expr::Comparison(c) => match &c.right {
                Expr::Parameter(name) => assert_eq!(name, "name"),
                _ => panic!("expected parameter"),
            },
            _ => panic!("expected comparison"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_property_access() {
    let stmt = parse("RETURN n.name").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Property(p) => {
                assert_eq!(p.property, "name");
            }
            _ => panic!("expected property access"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_list_literal() {
    let stmt = parse("RETURN [1, 2, 3]").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::List(l) => assert_eq!(l.len(), 3),
            _ => panic!("expected list"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_map_literal() {
    let stmt = parse("RETURN {a: 1, b: 2}").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Map(pairs) => {
                assert_eq!(pairs.len(), 2);
                assert_eq!(pairs[0].key, "a");
                assert_eq!(pairs[1].key, "b");
            }
            _ => panic!("expected map"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_subscript() {
    let stmt = parse("RETURN list[0]").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Subscript(s) => {
                assert!(!s.slice);
                assert!(s.index.is_some());
            }
            _ => panic!("expected subscript"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_slice() {
    let stmt = parse("RETURN list[1..3]").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Subscript(s) => {
                assert!(s.slice);
                assert!(s.from.is_some());
                assert!(s.to.is_some());
            }
            _ => panic!("expected subscript"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_list_comprehension() {
    let stmt = parse("RETURN [x IN [1,2,3] WHERE x > 1 | x * 2]").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::ListComprehension(lc) => {
                assert_eq!(lc.variable, "x");
                assert!(lc.where_expr.is_some());
                assert!(lc.map_expr.is_some());
            }
            _ => panic!("expected list comprehension"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_reduce() {
    let stmt = parse("RETURN reduce(acc = 0, x IN [1,2,3] | acc + x)").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Reduce(re) => {
                assert_eq!(re.accumulator, "acc");
                assert_eq!(re.variable, "x");
            }
            _ => panic!("expected reduce"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_allreduce() {
    let stmt = parse("RETURN allReduce(acc = 0, x IN [1,2,3] | acc + x, acc < 10)").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::AllReduce(ar) => {
                assert_eq!(ar.accumulator, "acc");
                assert_eq!(ar.variable, "x");
            }
            _ => panic!("expected allReduce"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_concat_operator_is_left_associative_like_plus() {
    // `||` shares precedence with `+`/`-` and is parsed left-associatively,
    // producing the same nested-Binary shape `+` would.
    let stmt = parse("RETURN [1] || [2] || [3]").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Binary(outer) => {
                assert_eq!(outer.op, "||");
                match &outer.left {
                    Expr::Binary(inner) => assert_eq!(inner.op, "||"),
                    _ => panic!("expected nested binary || on the left"),
                }
            }
            _ => panic!("expected binary ||"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_quantifier() {
    let stmt = parse("RETURN all(x IN [1,2,3] WHERE x > 0)").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Quantifier(q) => {
                assert_eq!(q.quantifier, "all");
                assert_eq!(q.variable, "x");
                assert!(q.predicate.is_some());
            }
            _ => panic!("expected quantifier"),
        },
        _ => panic!("expected return"),
    }
}

// LABS-201: agents repeatedly use patterns inside list predicates without
// the required EXISTS {}, and spell the predicate with the GQL pipe instead
// of WHERE. Both used to fail as a bare `expected RightParen, got |`; the
// parser now names the fix in each case.
#[test]
fn test_quantifier_pipe_instead_of_where_names_the_fix() {
    for q in ["any", "all", "none", "single"] {
        let err = parse(&format!("MATCH (n) WHERE {q}(x IN [1,2] | x > 1) RETURN n")).unwrap_err();
        assert!(
            err.message.contains("WHERE"),
            "{q}: expected the error to point at WHERE, got {err:?}"
        );
    }
}

#[test]
fn test_pattern_in_list_predicate_points_at_exists() {
    for q in ["any", "all", "none", "single"] {
        let err = parse(&format!(
            "MATCH (a) WHERE {q}((a)-[:R]->(b) | b.name = 'x') RETURN a"
        ))
        .unwrap_err();
        assert!(
            err.message.contains("EXISTS"),
            "{q}: expected the error to point at EXISTS, got {err:?}"
        );
    }
}

// The pipe-less pattern form keeps its existing parse: the EXISTS-targeted
// lookahead must not hijack it (the validator reports it as a pattern
// predicate outside WHERE).
#[test]
fn test_pattern_in_quantifier_without_pipe_still_parses() {
    assert!(parse("RETURN any((a)-[:R]->(b))").is_ok());
    // An arithmetic argument is not a pattern and keeps the generic
    // right-paren error path.
    assert!(parse("RETURN any((1 + 2) > 3)").is_ok());
}

#[test]
fn test_parse_has_label() {
    let stmt = parse("MATCH (n) WHERE n:Person RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match m.where_expr.as_ref().unwrap() {
            Expr::HasLabel(h) => {
                assert_eq!(
                    h.label_expr.as_label_list(),
                    Some(vec!["Person".to_string()])
                );
            }
            _ => panic!("expected has label"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_shortest_path_expr() {
    // shortestPath is parsed as an expression, not in MATCH pattern context
    let stmt = parse("RETURN shortestPath((a)-[*]->(b))").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::ShortestPath(sp) => {
                assert!(!sp.all);
                assert_eq!(sp.pattern.elements.len(), 3);
            }
            _ => panic!("expected shortest path expr"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_unary_minus() {
    let stmt = parse("RETURN -42").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Unary(u) => {
                assert_eq!(u.op, "-");
                match &u.expr {
                    Expr::Literal(LiteralValue::Integer(v)) => assert_eq!(*v, 42),
                    _ => panic!("expected integer"),
                }
            }
            _ => panic!("expected unary"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_namespaced_function() {
    let stmt = parse("RETURN point.distance(p1, p2)").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::FunctionCall(f) => {
                assert_eq!(f.name, "point.distance");
                assert_eq!(f.args.len(), 2);
            }
            _ => panic!("expected function call"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_exponentiation() {
    let stmt = parse("RETURN 2 ^ 3").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Binary(b) => assert_eq!(b.op, "^"),
            _ => panic!("expected binary"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_xor() {
    let stmt = parse("RETURN true XOR false").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Binary(b) => assert_eq!(b.op, "XOR"),
            _ => panic!("expected binary"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_count_distinct() {
    let stmt = parse("RETURN count(DISTINCT n)").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::FunctionCall(f) => {
                assert_eq!(f.name, "count");
                assert!(f.distinct);
            }
            _ => panic!("expected function call"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_modulo() {
    let stmt = parse("RETURN 10 % 3").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Binary(b) => assert_eq!(b.op, "%"),
            _ => panic!("expected binary"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_chained_comparison() {
    let stmt = parse("RETURN 1 < 2 < 3").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Binary(b) => {
                assert_eq!(b.op, "AND");
            }
            _ => panic!("expected binary AND for chained comparison"),
        },
        _ => panic!("expected return"),
    }
}

// -- PROPERTY_EXISTS (CIP-247) --

#[test]
fn test_parse_property_exists_basic() {
    let stmt = parse("MATCH (p) RETURN PROPERTY_EXISTS(p, email) AS e").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::FunctionCall(fc) => {
                assert_eq!(fc.name, "property_exists");
                assert_eq!(fc.args.len(), 2);
                assert_eq!(fc.args[0], Expr::Variable("p".to_string()));
                assert_eq!(
                    fc.args[1],
                    Expr::Literal(LiteralValue::String("email".to_string()))
                );
            }
            other => panic!("expected function call, got {other:?}"),
        },
        other => panic!("expected return, got {other:?}"),
    }
}

#[test]
fn test_parse_property_exists_literal_property_name_is_error() {
    // A string literal in property-name position is a compile-time error.
    assert!(parse("RETURN PROPERTY_EXISTS(p, \"email\")").is_err());
}

#[test]
fn test_parse_property_exists_non_variable_first_arg_is_error() {
    assert!(parse("RETURN PROPERTY_EXISTS(1, name)").is_err());
}

#[test]
fn test_parse_property_exists_expression_property_name_is_error() {
    assert!(parse("RETURN PROPERTY_EXISTS(p, a + b)").is_err());
    assert!(parse("RETURN PROPERTY_EXISTS(p, toString(x))").is_err());
}
