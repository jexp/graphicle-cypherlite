// ADR-0191: the key group of the keyword-symbolic-names probe -- an
// unquoted keyword as a map literal or pattern property-map key, a map
// projection entry or selector, or a `.prop` selector.

use super::*;

/// The shared `probe_group_rows` check for the key group: the group is
/// non-empty and every row id is unique.
#[test]
fn probe_fixture_has_key_rows() {
    super::probe_group_rows("key");
}

/// ADR-0191 point 5: no key row is in the ADR's `exception_rows`, so every
/// Neo4j-accepted key row must parse. The fixture's five shapes cover a map
/// literal key, a map projection key, a `.prop` selector, a bare variable
/// selector and a pattern property-map key.
#[test]
fn probe_key_rows_accepted_by_neo4j_parse() {
    super::probe_accepted_rows_parse("key");
}

/// ADR-0191 point 5, `parse_map_literal`: a keyword key keeps its source
/// spelling (`as`/`union`, not lowercased and not backticked; `MATCH`/`AS`
/// keep their case).
#[test]
fn map_literal_keys_keep_the_keyword_spelling() {
    let stmt = parse_ok("RETURN {as: 1, union: 2} AS m");
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Map(pairs) => {
                assert_eq!(pairs.len(), 2);
                assert_eq!(pairs[0].key, "as");
                assert_eq!(pairs[1].key, "union");
                assert_eq!(pairs[0].value, Expr::Literal(LiteralValue::Integer(1)));
                assert_eq!(pairs[1].value, Expr::Literal(LiteralValue::Integer(2)));
            }
            other => panic!("expected map literal, got {other:?}"),
        },
        other => panic!("expected return clause, got {other:?}"),
    }

    let stmt = parse_ok("RETURN {MATCH: 1, AS: 2} AS m");
    match &stmt.clauses[0] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::Map(pairs) => {
                assert_eq!(pairs[0].key, "MATCH");
                assert_eq!(pairs[1].key, "AS");
            }
            other => panic!("expected map literal, got {other:?}"),
        },
        other => panic!("expected return clause, got {other:?}"),
    }
}

/// ADR-0191 point 5, `parse_map_projection_items`: both the `.prop`
/// selector and the `key: expr` entry keep the keyword spelling.
#[test]
fn map_projection_keyword_property_and_key_items() {
    let stmt = parse_ok("WITH {a: 1} AS m RETURN m {.as, where: 2} AS x");
    match &stmt.clauses[1] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::MapProjection(mp) => {
                assert_eq!(mp.expr, Expr::Variable("m".to_string()));
                assert_eq!(
                    mp.items,
                    vec![
                        MapProjectionItem::Property("as".to_string()),
                        MapProjectionItem::KeyValue(
                            "where".to_string(),
                            Expr::Literal(LiteralValue::Integer(2))
                        ),
                    ]
                );
            }
            other => panic!("expected map projection, got {other:?}"),
        },
        other => panic!("expected return clause, got {other:?}"),
    }
}

/// The probe's `key-map-projection-variable` and `key-map-projection-prop`
/// shapes: a bare keyword selector reads the same-named variable and a
/// `.keyword` selector reads the property, both spelled as written.
#[test]
fn map_projection_keyword_variable_and_property_selectors() {
    let stmt = parse_ok("WITH 1 AS `MATCH`, {x: 2} AS n RETURN n{MATCH} AS m");
    match &stmt.clauses[1] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::MapProjection(mp) => assert_eq!(
                mp.items,
                vec![MapProjectionItem::Variable("MATCH".to_string())]
            ),
            other => panic!("expected map projection, got {other:?}"),
        },
        other => panic!("expected return clause, got {other:?}"),
    }

    let stmt = parse_ok("WITH {MATCH: 1} AS n RETURN n{.MATCH} AS m");
    match &stmt.clauses[1] {
        Clause::Return(r) => match &r.items[0].expr {
            Expr::MapProjection(mp) => assert_eq!(
                mp.items,
                vec![MapProjectionItem::Property("MATCH".to_string())]
            ),
            other => panic!("expected map projection, got {other:?}"),
        },
        other => panic!("expected return clause, got {other:?}"),
    }
}

/// ADR-0191 point 5: node and relationship pattern property maps both call
/// `parse_map_literal`, so both accept a keyword key with its spelling.
#[test]
fn pattern_property_map_keys_accept_keywords() {
    let stmt = parse_ok("MATCH (n {MATCH: 1}) RETURN n");
    match &stmt.clauses[0] {
        Clause::Match(m) => match &m.pattern[0].elements[0] {
            PatternElement::Node(n) => {
                assert!(n.has_properties);
                assert_eq!(n.properties.len(), 1);
                assert_eq!(n.properties[0].0, "MATCH");
                assert_eq!(n.properties[0].1, Expr::Literal(LiteralValue::Integer(1)));
            }
            other => panic!("expected node pattern, got {other:?}"),
        },
        other => panic!("expected match clause, got {other:?}"),
    }

    let stmt = parse_ok("MATCH ()-[r {WHERE: 2}]->() RETURN r");
    match &stmt.clauses[0] {
        Clause::Match(m) => match &m.pattern[0].elements[1] {
            PatternElement::Relationship(rel) => {
                assert_eq!(rel.properties.len(), 1);
                assert_eq!(rel.properties[0].0, "WHERE");
                assert_eq!(rel.properties[0].1, Expr::Literal(LiteralValue::Integer(2)));
            }
            other => panic!("expected relationship pattern, got {other:?}"),
        },
        other => panic!("expected match clause, got {other:?}"),
    }
}

/// ADR-0191 points 5/6, `peek_brace_is_map_projection`: COUNT/EXISTS/COLLECT
/// followed by `{` and a keyword projection filler (`key:`, bare selector,
/// comma list, `.prop`) are the ordinary-variable map-projection reading,
/// not a scalar subquery -- the filler disambiguates them.
#[test]
fn subquery_keyword_roots_with_projection_fillers_are_map_projections() {
    let key_one = || {
        vec![MapProjectionItem::KeyValue(
            "match".to_string(),
            Expr::Literal(LiteralValue::Integer(1)),
        )]
    };
    for (query, root, items) in [
        ("RETURN count {match: 1} AS m", "count", key_one()),
        (
            "RETURN count {match} AS m",
            "count",
            vec![MapProjectionItem::Variable("match".to_string())],
        ),
        (
            "RETURN count {match, other: 2} AS m",
            "count",
            vec![
                MapProjectionItem::Variable("match".to_string()),
                MapProjectionItem::KeyValue(
                    "other".to_string(),
                    Expr::Literal(LiteralValue::Integer(2)),
                ),
            ],
        ),
        (
            "RETURN count {.match} AS m",
            "count",
            vec![MapProjectionItem::Property("match".to_string())],
        ),
        ("RETURN exists {match: 1} AS m", "exists", key_one()),
        ("RETURN collect {match: 1} AS m", "collect", key_one()),
        // CASE keeps its form only when the real CASE grammar succeeds;
        // `case {match: 1}` fails it, falls back to the variable read and
        // then takes the same map-projection reading (ADR-0191 point 6).
        ("RETURN case {match: 1} AS m", "case", key_one()),
    ] {
        let stmt = parse_ok(query);
        match &stmt.clauses[0] {
            Clause::Return(r) => match &r.items[0].expr {
                Expr::MapProjection(mp) => {
                    assert_eq!(mp.expr, Expr::Variable(root.to_string()), "{query}");
                    assert_eq!(mp.items, items, "{query}");
                }
                other => panic!("{query}: expected map projection, got {other:?}"),
            },
            other => panic!("{query}: expected return clause, got {other:?}"),
        }
    }
}

/// ADR-0191 point 6, `peek_brace_is_map_projection`: a clause keyword
/// opening the brace body is still a subquery, because the token after it
/// (`(`, an operand, ...) cannot start a map-projection item. This is the
/// existing meaning keeping priority over the widened item read.
#[test]
fn subquery_keyword_roots_with_clause_bodies_stay_subqueries() {
    let stmt = parse_ok("MATCH (n) WHERE EXISTS { MATCH (n)-[:KNOWS]->() } RETURN n");
    match &stmt.clauses[0] {
        Clause::Match(m) => assert!(
            matches!(m.where_expr.as_ref().unwrap(), Expr::ExistsSubquery(_)),
            "expected EXISTS subquery, got {:?}",
            m.where_expr
        ),
        other => panic!("expected match clause, got {other:?}"),
    }

    let stmt = parse_ok("RETURN COUNT { MATCH (a) RETURN a } AS c");
    match &stmt.clauses[0] {
        Clause::Return(r) => assert!(
            matches!(&r.items[0].expr, Expr::CountSubquery(_)),
            "expected COUNT subquery, got {:?}",
            r.items[0].expr
        ),
        other => panic!("expected return clause, got {other:?}"),
    }

    let stmt = parse_ok("MATCH (p) RETURN COLLECT { MATCH (p)-[:F]->(f) RETURN f.name } AS xs");
    match &stmt.clauses[1] {
        Clause::Return(r) => assert!(
            matches!(&r.items[0].expr, Expr::CollectSubquery(_)),
            "expected COLLECT subquery, got {:?}",
            r.items[0].expr
        ),
        other => panic!("expected return clause, got {other:?}"),
    }
}
