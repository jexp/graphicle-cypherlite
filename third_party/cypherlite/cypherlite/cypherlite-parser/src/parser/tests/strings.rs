use super::*;

#[test]
fn test_parse_is_normalized_default_form() {
    let stmt = parse("RETURN s IS NORMALIZED AS r").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::IsNormalized(isn) => {
                assert_eq!(isn.form, None);
                assert!(!isn.negated);
            }
            other => panic!("expected IsNormalized, got {other:?}"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_is_not_explicit_form_normalized() {
    let stmt = parse("RETURN s IS NOT NFD NORMALIZED AS r").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::IsNormalized(isn) => {
                assert_eq!(isn.form.as_deref(), Some("NFD"));
                assert!(isn.negated);
            }
            other => panic!("expected IsNormalized, got {other:?}"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_is_normalized_case_insensitive() {
    let stmt = parse("RETURN s is nfc normalized AS r").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::IsNormalized(isn) => assert_eq!(isn.form.as_deref(), Some("NFC")),
            other => panic!("expected IsNormalized, got {other:?}"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_normalize_one_and_two_arg_forms() {
    let stmt = parse("RETURN normalize('a'), normalize('a', NFKD)").unwrap();
    let Clause::Return(r) = &stmt.clauses[0] else {
        panic!("expected return");
    };
    let Expr::FunctionCall(fc) = &r.items[0].expr else {
        panic!("expected FunctionCall, got {:?}", r.items[0].expr);
    };
    assert_eq!(fc.args.len(), 1);
    let Expr::FunctionCall(fc) = &r.items[1].expr else {
        panic!("expected FunctionCall, got {:?}", r.items[1].expr);
    };
    assert_eq!(fc.args.len(), 2);
    let Expr::Literal(LiteralValue::String(s)) = &fc.args[1] else {
        panic!("expected keyword literal, got {:?}", fc.args[1]);
    };
    assert_eq!(s, "NFKD");
}

#[test]
fn test_parse_normalize_rejects_quoted_string_form() {
    assert!(parse("RETURN normalize('a', 'NFC')").is_err());
}

#[test]
fn test_parse_normalize_rejects_bad_arg_count() {
    assert!(parse("RETURN normalize()").is_err());
    assert!(parse("RETURN normalize('a', NFC, NFD)").is_err());
}

#[test]
fn test_parse_trim_special_forms() {
    // The `TRIM` special forms and the function name/arity each resolves
    // to: plain, `BOTH FROM` / `FROM`, `charstr FROM`, `LEADING` and
    // `TRAILING`.
    let cases = [
        ("RETURN trim(' hello ')", "trim", 1),
        ("RETURN trim(BOTH FROM ' hello ')", "trim", 1),
        ("RETURN trim(FROM ' hello ')", "trim", 1),
        ("RETURN trim('h' FROM \"hhhhhellohhh\")", "trim", 2),
        (
            "RETURN trim(LEADING 'h' FROM \"hhhhhellohhh\")",
            "trim_leading",
            2,
        ),
        (
            "RETURN trim(TRAILING FROM \"hhhhhellohhh\")",
            "trim_trailing",
            1,
        ),
    ];
    for (query, expected_name, expected_args) in cases {
        let stmt = parse(query).unwrap_or_else(|e| panic!("{query:?} must parse: {e}"));
        match &stmt.clauses[0] {
            Clause::Return(r) => match &r.items[0].expr {
                Expr::FunctionCall(fc) => {
                    assert_eq!(fc.name, expected_name, "{query:?}");
                    assert_eq!(fc.args.len(), expected_args, "{query:?}");
                }
                other => panic!("{query:?}: expected FunctionCall, got {other:?}"),
            },
            other => panic!("{query:?}: expected return, got {other:?}"),
        }
    }
}

#[test]
fn test_parse_trim_keywords_case_insensitive() {
    let stmt = parse("RETURN TRIM(both 'h' from \"hhh\")").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::FunctionCall(fc) => {
                assert_eq!(fc.name, "trim");
                assert_eq!(fc.args.len(), 2);
            }
            other => panic!("expected FunctionCall, got {other:?}"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_ltrim_rejects_from_syntax() {
    // Unlike TRIM, the multi-character trim functions do not support the
    // LEADING/TRAILING/BOTH ... FROM syntax (CIP-149).
    assert!(parse("RETURN ltrim(BOTH 'x' FROM 'hello')").is_err());
}

#[test]
fn test_parse_string_match() {
    let stmt = parse("RETURN 'hello' STARTS WITH 'he'").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::StringMatch(s) => assert_eq!(s.op, "STARTS WITH"),
            _ => panic!("expected string match"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_contains() {
    let stmt = parse("RETURN 'hello' CONTAINS 'ell'").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::StringMatch(s) => assert_eq!(s.op, "CONTAINS"),
            _ => panic!("expected string match"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_parse_ends_with() {
    let stmt = parse("RETURN 'hello' ENDS WITH 'lo'").unwrap();
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::StringMatch(s) => assert_eq!(s.op, "ENDS WITH"),
            _ => panic!("expected string match"),
        },
        _ => panic!("expected return"),
    }
}

#[test]
fn test_interp_body_unbalanced_closer_does_not_panic() {
    // A mismatched closer with nothing open used to underflow the depth
    // counter (`attempt to subtract with overflow` in debug builds).
    for q in [
        "RETURN s'{)}' AS r",
        "RETURN s'{(}' AS r",
        "RETURN s'{]}' AS r",
    ] {
        assert!(parse(q).is_err(), "expected a clean parse error for {q:?}");
    }
}

#[test]
fn test_interp_embed_escaped_quote_kept_raw() {
    // Escapes inside `{...}` reach the embed's re-parse verbatim, so a
    // quoted string carrying an escaped quote is not corrupted.
    let stmt = parse(r#"RETURN s'{coalesce('a\'b','c')}' AS r"#).unwrap();
    let Clause::Return(r) = &stmt.clauses[0] else {
        panic!("expected Return");
    };
    let Expr::StringInterp(parts) = &r.items[0].expr else {
        panic!("expected StringInterp, got {:?}", r.items[0].expr);
    };
    assert_eq!(parts.len(), 1);
    assert!(matches!(parts[0], InterpPart::Interp(_)));

    // `\{` / `\}` inside an embed still parse (unknown escape).
    assert!(parse(r#"RETURN s'{coalesce('x\{y','z')}' AS r"#).is_ok());
}

#[test]
fn test_interp_escaped_backslash_before_embed() {
    // `\\` before `{` is an escaped backslash then an embed, not an
    // escaped literal brace.
    let parts = interp_parts(r"RETURN s'\\{1}' AS r");
    assert_eq!(parts.len(), 2);
    assert!(matches!(&parts[0], InterpPart::Text(t) if t == "\\"));
    assert!(matches!(parts[1], InterpPart::Interp(_)));

    // `\{` is a literal brace: one text part, no embed.
    let parts = interp_parts(r"RETURN s'\{1}' AS r");
    assert_eq!(parts.len(), 1);
    assert!(matches!(&parts[0], InterpPart::Text(t) if t == "{1}"));

    // A doubled backslash between text and an embed decodes once.
    let parts = interp_parts(r"RETURN s'a\\b{1}' AS r");
    assert_eq!(parts.len(), 2);
    assert!(matches!(&parts[0], InterpPart::Text(t) if t == "a\\b"));
    assert!(matches!(parts[1], InterpPart::Interp(_)));

    // Standard escapes still resolve in text.
    let parts = interp_parts(r"RETURN s'a\nb' AS r");
    assert_eq!(parts.len(), 1);
    assert!(matches!(&parts[0], InterpPart::Text(t) if t == "a\nb"));
}

#[test]
fn test_interp_backtick_name_in_embed() {
    // A backtick-quoted identifier inside an embed is inert: the `}` in
    // `` `a}b` `` does not close the embed.
    assert!(parse(r#"RETURN s'{`a}b`}' AS r"#).is_ok());
    // The `'` in `` `x'y` `` does not open an inner string.
    assert!(parse(r#"RETURN s'{`x'y`}' AS r"#).is_ok());
}
