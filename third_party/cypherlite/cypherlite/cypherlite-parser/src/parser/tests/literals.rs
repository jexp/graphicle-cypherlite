use super::*;

#[test]
fn test_integer_overflow_literals_tag_detail() {
    // A too-large / too-small integer literal is a compile-time
    // SyntaxError whose ParseError carries the "IntegerOverflow" detail tag
    // (consumed by EngineError::kind()); the rendered message is unchanged.
    for q in [
        "RETURN 9223372036854775808 AS l",
        "RETURN -9223372036854775809 AS l",
        "RETURN 0x8000000000000000 AS l",
        "RETURN -0x8000000000000001 AS l",
    ] {
        let err = parse(q).unwrap_err();
        assert_eq!(
            err.detail,
            Some("IntegerOverflow"),
            "expected IntegerOverflow detail for {q:?}"
        );
        assert!(
            err.message.starts_with("integer overflow"),
            "message drifted for {q:?}: {}",
            err.message
        );
    }
}

#[test]
fn test_float_overflow_literal_tags_detail() {
    let err = parse("RETURN 1.34E999").unwrap_err();
    assert_eq!(err.detail, Some("FloatingPointOverflow"));
    assert_eq!(
        err.message,
        "FloatingPointOverflow: float literal is too large"
    );
}

#[test]
fn test_map_key_starting_with_digit_is_unexpected_syntax() {
    // Literals8 [19]: a digit-leading map key (`{1B2c3e67: 1}`)
    // lexes as an InvalidNumberLiteral error token, but in map-key position
    // the openCypher TCK expects the generic `UnexpectedSyntax` detail
    // (SyntaxError class), not `InvalidNumberLiteral`.
    for q in [
        "RETURN {1B2c3e67:1} AS literal",
        "RETURN {1B2c3e67: 'x'}",
        "WITH {2ab: 1} AS m RETURN m",
    ] {
        let err = parse(q).unwrap_err();
        assert_eq!(
            err.detail,
            Some("UnexpectedSyntax"),
            "expected UnexpectedSyntax detail for {q:?}, got {:?}",
            err.detail
        );
        assert!(
            err.message
                .starts_with("expected map key (identifier or string)"),
            "message drifted for {q:?}: {}",
            err.message
        );
    }
}

#[test]
fn test_map_key_pure_number_is_unexpected_syntax() {
    // A pure-digit key (`{123: 1}`) lexes as a plain Integer token (no
    // trailing ident), which is also rejected in key position with the
    // generic `UnexpectedSyntax` detail — matching the digit-leading case.
    let err = parse("RETURN {123: 1} AS literal").unwrap_err();
    assert_eq!(err.detail, Some("UnexpectedSyntax"));
    assert!(err
        .message
        .starts_with("expected map key (identifier or string)"));
}

#[test]
fn test_valid_map_keys_still_parse() {
    // Identifier, backtick-quoted, and single-quoted keys are all valid map
    // keys and must keep parsing after the digit-leading-key fix.
    for q in [
        "RETURN {valid_key: 1}",
        "RETURN {`123`: 1}",
        "RETURN {'123': 1}",
        "RETURN {`1B2c3e67`: 1}",
    ] {
        let stmt = parse(q).unwrap_or_else(|e| panic!("{q:?} should parse: {e}"));
        match &stmt.clauses[0] {
            Clause::Return(r) => match &r.items[0].expr {
                Expr::Map(pairs) => assert_eq!(pairs.len(), 1, "{q:?}"),
                other => panic!("expected map for {q:?}, got {other:?}"),
            },
            _ => panic!("expected return for {q:?}"),
        }
    }
}

#[test]
fn test_digit_leading_token_in_expression_position_stays_invalid_number_literal() {
    // The same malformed token OUTSIDE map-key position (as a bare
    // expression) must keep its `InvalidNumberLiteral` detail — the fix is
    // strictly local to the map-key parse path.
    for q in ["RETURN 1B2c3e67", "RETURN 1B2c3e67 AS l"] {
        let err = parse(q).unwrap_err();
        assert_eq!(
            err.detail,
            Some("InvalidNumberLiteral"),
            "expected InvalidNumberLiteral detail for {q:?}, got {:?}",
            err.detail
        );
    }
}

#[test]
fn test_parse_hex_integer() {
    let stmt = parse("RETURN 0xFF").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Literal(LiteralValue::Integer(v)) => assert_eq!(*v, 255),
            _ => panic!("expected integer literal"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_octal_integer() {
    let stmt = parse("RETURN 0o77").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Literal(LiteralValue::Integer(v)) => assert_eq!(*v, 63),
            _ => panic!("expected integer literal"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
#[allow(clippy::approx_constant)]
fn test_parse_float_literal() {
    let stmt = parse("RETURN 3.14").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Literal(LiteralValue::Float(v)) => assert!((*v - 3.14).abs() < 1e-10),
            _ => panic!("expected float literal"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_int_literal_parsing() {
    assert_eq!(parse_int_literal("42").unwrap(), 42);
    assert_eq!(parse_int_literal("0xFF").unwrap(), 255);
    assert_eq!(parse_int_literal("0o77").unwrap(), 63);
    assert_eq!(parse_int_literal("-10").unwrap(), -10);
    assert_eq!(parse_int_literal("-0x8000000000000000").unwrap(), i64::MIN);
}

// -- CIP-55: Infinity/Inf/NaN literals --

#[test]
fn test_parse_infinity_and_nan_literals() {
    let stmt = parse("RETURN Infinity AS a, Inf AS b, NaN AS c").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => {
            assert_eq!(
                r.items[0].expr,
                Expr::Literal(LiteralValue::Float(f64::INFINITY))
            );
            assert_eq!(
                r.items[1].expr,
                Expr::Literal(LiteralValue::Float(f64::INFINITY))
            );
            match &r.items[2].expr {
                Expr::Literal(LiteralValue::Float(f)) => assert!(f.is_nan()),
                other => panic!("expected NaN float literal, got {other:?}"),
            }
        }
        other => panic!("expected return clause, got {other:?}"),
    }
}

#[test]
fn test_parse_infinity_is_case_insensitive() {
    for src in ["infinity", "INFINITY", "InFiNiTy", "inf", "INF"] {
        let stmt = parse(&format!("RETURN {src} AS x")).unwrap();
        match &stmt.clauses[0] {
            Clause::Return(r) => assert_eq!(
                r.items[0].expr,
                Expr::Literal(LiteralValue::Float(f64::INFINITY)),
                "failed for input {src:?}"
            ),
            other => panic!("expected return clause, got {other:?}"),
        }
    }
}

#[test]
fn test_parse_negative_infinity_is_unary_minus_over_literal() {
    let stmt = parse("RETURN -Infinity AS x").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Unary(u) => {
                assert_eq!(u.op, "-");
                assert_eq!(u.expr, Expr::Literal(LiteralValue::Float(f64::INFINITY)));
            }
            other => panic!("expected unary minus, got {other:?}"),
        },
        other => panic!("expected return clause, got {other:?}"),
    }
}

#[test]
fn test_parse_double_unary_minus_on_infinity_is_syntax_error() {
    assert!(parse("RETURN --Infinity AS x").is_err());
    assert!(parse("RETURN --NaN AS x").is_err());
    // Parenthesized double negation is fine (not a direct `--` run).
    assert!(parse("RETURN -(-Infinity) AS x").is_ok());
}

#[test]
fn test_parse_double_unary_minus_on_plain_number_still_parses() {
    // Only the Infinity/NaN keyword literals are restricted; ordinary
    // numeric double negation is unaffected.
    assert!(parse("RETURN --5 AS x").is_ok());
}

#[test]
fn test_parse_infinity_usable_as_label_and_property_key() {
    assert!(parse("CREATE (n:Infinity) RETURN labels(n)").is_ok());
    assert!(parse("CREATE (n {NaN: 1, Infinity: 2}) RETURN n.NaN, n.Infinity").is_ok());
}

#[test]
fn test_parse_infinity_wins_over_variable_of_same_name() {
    // The literal takes precedence over an in-scope variable named
    // `infinity` in expression position (CIP-55 usage[5]/[6]).
    let stmt = parse("WITH 0 AS infinity RETURN infinity AS x").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => assert_eq!(
            r.items[0].expr,
            Expr::Literal(LiteralValue::Float(f64::INFINITY))
        ),
        other => panic!("expected return clause, got {other:?}"),
    }
}

#[test]
fn test_parse_nan_and_inf_win_over_variables_of_same_name() {
    // ADR-0280 pin: the literal wins over an in-scope variable for all three
    // keyword spellings, silently (Neo4j 2026.09 probe 2).
    let stmt = parse("WITH 0 AS nan, 1 AS inf RETURN nan AS a, inf AS b").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => {
            match &r.items[0].expr {
                Expr::Literal(LiteralValue::Float(f)) => assert!(f.is_nan()),
                other => panic!("expected NaN float literal, got {other:?}"),
            }
            assert_eq!(
                r.items[1].expr,
                Expr::Literal(LiteralValue::Float(f64::INFINITY))
            );
        }
        other => panic!("expected return clause, got {other:?}"),
    }
}

#[test]
fn test_parse_keyword_names_bind_as_pattern_variable_and_alias() {
    // ADR-0280 pin: binding `inf`/`nan`/`infinity` is legal and silent, in
    // both pattern-variable and alias position (Neo4j binds them too).
    let stmt = parse("CREATE (inf:N)-[:R]->(nan:N)").unwrap();
    match &stmt.clauses[0] {
        Clause::Create(c) => {
            let vars: Vec<&str> = c.pattern[0]
                .elements
                .iter()
                .filter_map(|e| match e {
                    PatternElement::Node(n) => n.variable.as_deref(),
                    _ => None,
                })
                .collect();
            assert_eq!(vars, vec!["inf", "nan"]);
        }
        other => panic!("expected create clause, got {other:?}"),
    }
    let stmt = parse("WITH 0 AS nan, 1 AS inf, 2 AS infinity RETURN 1").unwrap();
    match &stmt.clauses[0] {
        Clause::With(w) => {
            let aliases: Vec<Option<&str>> = w.items.iter().map(|i| i.alias.as_deref()).collect();
            assert_eq!(aliases, vec![Some("nan"), Some("inf"), Some("infinity")]);
        }
        other => panic!("expected with clause, got {other:?}"),
    }
}

#[test]
fn test_parse_backticked_keyword_is_a_variable() {
    // ADR-0280 pin: backtick-escaping forces variable resolution in every
    // position — the one workaround when a variable of that name is bound.
    let stmt = parse("WITH 0 AS `nan` RETURN `nan` AS x, `inf`.v AS y").unwrap();
    match &stmt.clauses[1] {
        Clause::Return(r) => {
            assert_eq!(r.items[0].expr, Expr::Variable("nan".to_string()));
            match &r.items[1].expr {
                Expr::Property(p) => {
                    assert_eq!(p.expr, Expr::Variable("inf".to_string()));
                    assert_eq!(p.property, "v");
                }
                other => panic!("expected property access on variable, got {other:?}"),
            }
        }
        other => panic!("expected return clause, got {other:?}"),
    }
}

#[test]
fn test_parse_property_access_base_is_the_literal() {
    // ADR-0280 pin: `inf.v` takes the literal as the access base (it fails
    // later, at runtime, with InvalidArgumentType — Neo4j probe 3).
    let stmt = parse("RETURN inf.v AS x").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Property(p) => {
                assert_eq!(p.expr, Expr::Literal(LiteralValue::Float(f64::INFINITY)));
                assert_eq!(p.property, "v");
            }
            other => panic!("expected property access, got {other:?}"),
        },
        other => panic!("expected return clause, got {other:?}"),
    }
}

#[test]
fn test_parse_delete_keyword_target_is_the_literal() {
    // ADR-0280 pin: `DELETE inf` parses the target as the CIP-55 float
    // literal (the static rejection lives in cypherlite-front's
    // check_delete_expr, not the parser).
    let stmt = parse("MATCH (n) DELETE inf, nan").unwrap();
    match &stmt.clauses[1] {
        Clause::Delete(d) => {
            assert_eq!(
                d.expressions[0],
                Expr::Literal(LiteralValue::Float(f64::INFINITY))
            );
            match &d.expressions[1] {
                Expr::Literal(LiteralValue::Float(f)) => assert!(f.is_nan()),
                other => panic!("expected NaN float literal, got {other:?}"),
            }
        }
        other => panic!("expected delete clause, got {other:?}"),
    }
}

#[test]
fn test_parse_keyword_names_unaffected_as_property_and_map_keys() {
    // ADR-0280 pin: property keys after a dot and map keys are unaffected
    // (Neo4j probe 6) — `SET n.inf = 1` and `{nan: 1}.nan` work.
    let stmt = parse("MATCH (n) SET n.inf = 1, n.nan = 2").unwrap();
    match stmt.clauses.last().unwrap() {
        Clause::Set(sc) => {
            assert_eq!(sc.items[0].property.as_ref().unwrap().property, "inf");
            assert_eq!(sc.items[1].property.as_ref().unwrap().property, "nan");
        }
        other => panic!("expected set clause, got {other:?}"),
    }
    let stmt = parse("RETURN {nan: 1}.nan AS x").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Property(p) => {
                assert!(matches!(p.expr, Expr::Map(_)));
                assert_eq!(p.property, "nan");
            }
            other => panic!("expected property access on map literal, got {other:?}"),
        },
        other => panic!("expected return clause, got {other:?}"),
    }
}

#[test]
fn test_parse_overflowing_float_literal_is_rejected() {
    assert!(parse("RETURN 1.34E999").is_err());
    // But a legitimate Infinity keyword literal is fine.
    assert!(parse("RETURN Infinity").is_ok());
}

#[test]
fn test_parse_set_property_named_after_keyword() {
    // `limit` collides with the LIMIT keyword but must still be usable
    // as a property name in a SET target (CIP-55 usage[8]).
    let stmt = parse("MATCH (n) SET n.limit = Infinity").unwrap();
    match stmt.clauses.last().unwrap() {
        Clause::Set(sc) => {
            assert_eq!(sc.items[0].property.as_ref().unwrap().property, "limit");
        }
        other => panic!("expected set clause, got {other:?}"),
    }
}
