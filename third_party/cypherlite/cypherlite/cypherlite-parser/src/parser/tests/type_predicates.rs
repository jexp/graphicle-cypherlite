use super::*;

#[test]
fn test_parse_is_type_predicate() {
    let it = is_type_of("RETURN elementId(n) IS :: STRING AS isString");
    assert_eq!(
        it.type_expr,
        TypeExpr::Predefined {
            kind: PredefinedKind::String,
            not_null: false,
        }
    );
    assert!(!it.negated);
}

#[test]
fn test_parse_is_not_type_predicate_with_not_null() {
    let it = is_type_of("RETURN 1 IS NOT :: STRING NOT NULL AS r");
    assert_eq!(
        it.type_expr,
        TypeExpr::Predefined {
            kind: PredefinedKind::String,
            not_null: true,
        }
    );
    assert!(it.negated);
}

#[test]
fn test_parse_is_typed_and_bare_shorthand() {
    let expected = TypeExpr::Predefined {
        kind: PredefinedKind::Integer,
        not_null: false,
    };
    assert_eq!(
        is_type_of("RETURN 1 IS TYPED INTEGER AS a").type_expr,
        expected
    );
    assert_eq!(is_type_of("RETURN 1 :: INTEGER AS b").type_expr, expected);
}

#[test]
fn test_parse_type_predicate_no_chaining_without_parens() {
    assert!(parse("RETURN [1] IS :: LIST<INT> IS :: BOOLEAN AS r").is_err());
    assert!(parse("RETURN \"ab\" STARTS WITH \"a\" IS :: BOOLEAN AS r").is_err());
    assert!(parse("RETURN (\"ab\" STARTS WITH \"a\") IS :: BOOLEAN AS r").is_ok());
    assert!(parse("RETURN ([1] IS :: LIST<INT>) IS :: BOOLEAN AS r").is_ok());
}

#[test]
fn test_parse_vector_type_cypher_style() {
    assert_eq!(
        is_type_of("RETURN 1 IS :: VECTOR<INT32>(42) AS r").type_expr,
        TypeExpr::Vector {
            coord: Some(VectorCoordKind::Int32),
            dimension: Some(42),
            not_null: false,
        }
    );
}

#[test]
fn test_parse_vector_type_gql_style_dimension_first() {
    assert_eq!(
        is_type_of("RETURN 1 IS :: VECTOR(4, FLOAT64) AS r").type_expr,
        TypeExpr::Vector {
            coord: Some(VectorCoordKind::Float64),
            dimension: Some(4),
            not_null: false,
        }
    );
}

#[test]
fn test_parse_vector_type_partial_forms() {
    // Both halves are grammatically optional (rejected only later, at
    // constraint-creation time -- CIP-216 Negative 1/2).
    assert_eq!(
        is_type_of("RETURN 1 IS :: VECTOR(3) AS r").type_expr,
        TypeExpr::Vector {
            coord: None,
            dimension: Some(3),
            not_null: false,
        }
    );
    assert_eq!(
        is_type_of("RETURN 1 IS :: VECTOR<INTEGER32> AS r").type_expr,
        TypeExpr::Vector {
            coord: Some(VectorCoordKind::Int32),
            dimension: None,
            not_null: false,
        }
    );
}

#[test]
fn test_parse_vector_dimension_out_of_range_is_rejected() {
    assert!(parse("RETURN 1 IS :: VECTOR<INT32>(0) AS r").is_err());
    assert!(parse("RETURN 1 IS :: VECTOR<INT32>(4097) AS r").is_err());
    assert!(parse("RETURN 1 IS :: VECTOR<INT32>(-1) AS r").is_err());
    assert!(parse("RETURN 1 IS :: VECTOR<INT32>(1) AS r").is_ok());
    assert!(parse("RETURN 1 IS :: VECTOR<INT32>(4096) AS r").is_ok());
}

#[test]
fn test_parse_vector_invalid_coordinate_type_is_rejected() {
    assert!(parse("RETURN 1 IS :: VECTOR<STRING>(3) AS r").is_err());
}

#[test]
fn test_parse_vector_only_coordinate_type_rejected_as_plain_type() {
    assert!(parse("RETURN 1 IS :: INTEGER32 AS r").is_err());
    assert!(parse("RETURN 1 IS :: FLOAT32 AS r").is_err());
    // INT64/FLOAT64 remain valid plain-type aliases (CIP-216 S-6).
    assert!(parse("RETURN 1 IS :: INT64 AS r").is_ok());
    assert!(parse("RETURN 1 IS :: FLOAT64 AS r").is_ok());
}

#[test]
fn test_parse_bare_double_colon_vector_type_in_constraint() {
    // CIP-216 01#2/#4: the bare `x :: <type>` form skips `IS` entirely.
    let stmt =
        parse("CREATE CONSTRAINT c FOR (n:Label) REQUIRE n.prop :: VECTOR(4, FLOAT64)").unwrap();
    match &stmt.clauses[0] {
        Clause::CreateConstraint(cc) => {
            assert_eq!(cc.constraint_type, "propertyType");
            assert_eq!(
                cc.property_type,
                Some(TypeExpr::Vector {
                    coord: Some(VectorCoordKind::Float64),
                    dimension: Some(4),
                    not_null: false,
                })
            );
        }
        other => panic!("expected CreateConstraint, got {other:?}"),
    }
}

#[test]
fn test_parse_list_and_union_types() {
    match is_type_of("RETURN 1 IS :: INTEGER | FLOAT AS r").type_expr {
        TypeExpr::Union(components) => assert_eq!(components.len(), 2),
        other => panic!("expected Union, got {other:?}"),
    }

    match is_type_of("RETURN [1] IS :: LIST<INTEGER> AS r").type_expr {
        TypeExpr::List { element, not_null } => {
            assert!(!not_null);
            assert_eq!(
                *element,
                TypeExpr::Predefined {
                    kind: PredefinedKind::Integer,
                    not_null: false,
                }
            );
        }
        other => panic!("expected List, got {other:?}"),
    }

    // Mixed nullability among union components is a syntax error.
    assert!(parse("RETURN 1 IS :: INTEGER NOT NULL | FLOAT AS r").is_err());
    // NOT NULL directly after a closed union is a syntax error.
    assert!(parse("RETURN 1 IS :: ANY<INTEGER | FLOAT> NOT NULL AS r").is_err());

    // CIP-246: `UUID` is a standalone leaf type, not an `ANY`-prefixed
    // production. `ANY UUID` is a syntax error, exactly like every other
    // scalar type name after `ANY` (`ANY STRING`, ...).
    assert!(parse("RETURN 1 IS :: UUID AS r").is_ok());
    assert!(parse("RETURN 1 IS :: ANY UUID AS r").is_err());
    assert!(parse("RETURN 1 IS :: ANY STRING AS r").is_err());
}

/// CIP-102: `!` is an alternative spelling for a trailing `NOT NULL` in
/// a type expression -- it must produce the exact same `TypeExpr` as
/// the word form, everywhere `NOT NULL` is accepted (scalars, list
/// inner/outer, union members), and it must tolerate whitespace before
/// it (Neo4j-verified deviation from the CIP text; see CIP-102
/// NOTES.md).
#[test]
fn test_parse_not_null_bang_shorthand() {
    // Scalar: `!` and `NOT NULL` parse to an identical TypeExpr.
    let bang = is_type_of("RETURN 1 IS :: INTEGER! AS r").type_expr;
    let word = is_type_of("RETURN 1 IS :: INTEGER NOT NULL AS r").type_expr;
    assert_eq!(bang, word);

    // Whitespace (including a newline) before `!` is accepted.
    assert_eq!(is_type_of("RETURN 1 IS :: INTEGER ! AS r").type_expr, word);
    assert_eq!(is_type_of("RETURN 1 IS :: INTEGER\n! AS r").type_expr, word);

    // List outer/inner `!` matches the NOT NULL spelling.
    let list_bang = is_type_of("RETURN [1] IS :: LIST<INTEGER!>! AS r").type_expr;
    let list_word = is_type_of("RETURN [1] IS :: LIST<INTEGER NOT NULL> NOT NULL AS r").type_expr;
    assert_eq!(list_bang, list_word);

    // Union members: `!` on each component matches NOT NULL.
    let union_bang = is_type_of("RETURN 1 IS :: INTEGER! | FLOAT! AS r").type_expr;
    let union_word = is_type_of("RETURN 1 IS :: INTEGER NOT NULL | FLOAT NOT NULL AS r").type_expr;
    assert_eq!(union_bang, union_word);

    // Mixed nullability is still a compile-time error, whether spelled
    // with `!` or NOT NULL.
    assert!(parse("RETURN 1 IS :: INTEGER! | FLOAT AS r").is_err());
    // `!` directly after a closed union is likewise a syntax error.
    assert!(parse("RETURN 1 IS :: ANY<INTEGER! | FLOAT!>! AS r").is_err());
}
