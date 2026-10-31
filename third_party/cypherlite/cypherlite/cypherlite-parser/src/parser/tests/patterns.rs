use super::*;

#[test]
fn test_parse_simple_match_return() {
    let stmt = parse("MATCH (n) RETURN n").unwrap();
    assert_eq!(stmt.clauses.len(), 2);
    match &stmt.clauses[0] {
        Clause::Match(m) => {
            assert!(!m.optional);
            assert_eq!(m.pattern.len(), 1);
            assert_eq!(m.pattern[0].elements.len(), 1);
            match &m.pattern[0].elements[0] {
                PatternElement::Node(n) => {
                    assert_eq!(n.variable, Some("n".to_string()));
                }
                _ => panic!("expected node pattern"),
            }
        }
        _ => panic!("expected match clause"),
    }
    match &stmt.clauses[1] {
        Clause::Return(r) => {
            assert_eq!(r.items.len(), 1);
            match &r.items[0].expr {
                Expr::Variable(name) => assert_eq!(name, "n"),
                _ => panic!("expected variable"),
            }
        }
        _ => panic!("expected return clause"),
    }
}

#[test]
fn test_parse_match_with_labels() {
    let stmt = parse("MATCH (n:Person) RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match &m.pattern[0].elements[0] {
            PatternElement::Node(n) => {
                assert_eq!(
                    n.label_expr.as_ref().and_then(|e| e.as_label_list()),
                    Some(vec!["Person".to_string()])
                );
            }
            _ => panic!("expected node"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_match_with_properties() {
    let stmt = parse("MATCH (n:Person {name: 'Alice'}) RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match &m.pattern[0].elements[0] {
            PatternElement::Node(n) => {
                assert!(n.has_properties);
                assert_eq!(n.properties.len(), 1);
                assert_eq!(n.properties[0].0, "name");
                assert_eq!(
                    n.properties[0].1,
                    Expr::Literal(LiteralValue::String("Alice".to_string()))
                );
            }
            _ => panic!("expected node"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_relationship() {
    let stmt = parse("MATCH (a)-[r:KNOWS]->(b) RETURN a, b").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => {
            assert_eq!(m.pattern[0].elements.len(), 3);
            match &m.pattern[0].elements[1] {
                PatternElement::Relationship(r) => {
                    assert_eq!(r.variable, Some("r".to_string()));
                    assert_eq!(
                        r.type_expr.as_ref().and_then(|e| e.as_label_list()),
                        Some(vec!["KNOWS".to_string()])
                    );
                    assert_eq!(r.direction, Direction::Right);
                }
                _ => panic!("expected relationship"),
            }
        }
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_left_arrow() {
    let stmt = parse("MATCH (a)<-[r]-(b) RETURN a").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match &m.pattern[0].elements[1] {
            PatternElement::Relationship(r) => {
                assert_eq!(r.direction, Direction::Left);
            }
            _ => panic!("expected relationship"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_undirected() {
    let stmt = parse("MATCH (a)-[r]-(b) RETURN a").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match &m.pattern[0].elements[1] {
            PatternElement::Relationship(r) => {
                assert_eq!(r.direction, Direction::Both);
            }
            _ => panic!("expected relationship"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_where() {
    let stmt = parse("MATCH (n) WHERE n.age > 30 RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => {
            assert!(m.where_expr.is_some());
        }
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_named_path() {
    let stmt = parse("MATCH p = (a)-[r]->(b) RETURN p").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => {
            assert_eq!(m.pattern[0].variable, Some("p".to_string()));
        }
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_variable_length_rel() {
    let stmt = parse("MATCH (a)-[*1..3]->(b) RETURN a").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match &m.pattern[0].elements[1] {
            PatternElement::Relationship(r) => {
                assert_eq!(r.min_hops, Some(1));
                assert_eq!(r.max_hops, Some(3));
            }
            _ => panic!("expected relationship"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_pattern_predicate() {
    let stmt = parse("MATCH (n) WHERE (n)-[:KNOWS]->() RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match m.where_expr.as_ref().unwrap() {
            Expr::PatternPredicate(_) => {}
            _ => panic!("expected pattern predicate"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_multiple_rel_types() {
    let stmt = parse("MATCH (a)-[:KNOWS|LIKES]->(b) RETURN a").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match &m.pattern[0].elements[1] {
            PatternElement::Relationship(r) => match r.type_expr.as_ref().unwrap() {
                LabelExpr::Or(parts) => {
                    let names: Vec<String> = parts
                        .iter()
                        .map(|p| match p {
                            LabelExpr::Label(n) => n.clone(),
                            _ => panic!("expected label"),
                        })
                        .collect();
                    assert_eq!(names, vec!["KNOWS".to_string(), "LIKES".to_string()]);
                }
                _ => panic!("expected disjunction"),
            },
            _ => panic!("expected relationship"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_multiple_patterns() {
    let stmt = parse("MATCH (a), (b) RETURN a, b").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => {
            assert_eq!(m.pattern.len(), 2);
        }
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_empty_node() {
    let stmt = parse("MATCH () RETURN 1").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match &m.pattern[0].elements[0] {
            PatternElement::Node(n) => {
                assert!(n.variable.is_none());
                assert!(n.label_expr.is_none());
            }
            _ => panic!("expected node"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_bare_star_rel() {
    let stmt = parse("MATCH (a)-[*]->(b) RETURN a").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(m) => match &m.pattern[0].elements[1] {
            PatternElement::Relationship(r) => {
                assert_eq!(r.min_hops, Some(1));
                assert!(r.max_hops.is_none());
            }
            _ => panic!("expected relationship"),
        },
        _ => panic!("expected match"),
    }
}

#[test]
fn test_parse_legacy_hop_bounds_all_spellings() {
    // All six legacy bracket-syntax hop-bound spellings (`*`, `*n`, `*n..`,
    // `*n..m`, `*..`, `*..m`) and their (min, max) lowering.
    for (q, want_min, want_max) in [
        ("MATCH (a)-[*]->(b) RETURN a", 1, None),
        ("MATCH (a)-[*3]->(b) RETURN a", 3, Some(3)),
        ("MATCH (a)-[*2..]->(b) RETURN a", 2, None),
        ("MATCH (a)-[*1..3]->(b) RETURN a", 1, Some(3)),
        ("MATCH (a)-[*..]->(b) RETURN a", 1, None),
        ("MATCH (a)-[*..4]->(b) RETURN a", 1, Some(4)),
    ] {
        let stmt = parse(q).unwrap();
        match &stmt.clauses[0] {
            Clause::Match(m) => match &m.pattern[0].elements[1] {
                PatternElement::Relationship(r) => {
                    assert_eq!(r.min_hops, Some(want_min), "{q}: min_hops");
                    assert_eq!(r.max_hops, want_max, "{q}: max_hops");
                }
                _ => panic!("{q}: expected relationship"),
            },
            _ => panic!("{q}: expected match"),
        }
    }
}

#[test]
fn test_relationship_type_negation_or_wildcard_with_quantifier_rejected() {
    assert!(parse("MATCH ()-[:!S*]->() RETURN 1").is_err());
    assert!(parse("MATCH ()-[:%*]->() RETURN 1").is_err());
    // But single-type and legacy pipe-disjunction quantified patterns remain valid.
    assert!(parse("MATCH ()-[:S*]->() RETURN 1").is_ok());
    assert!(parse("MATCH ()-[:S|T*1..2]->() RETURN 1").is_ok());
    assert!(parse("MATCH ()-[:(S)*]->() RETURN 1").is_ok());
}

#[test]
fn test_node_and_relationship_element_pattern_where() {
    assert!(parse("MATCH (n WHERE n:A&B)-[r]->(m) RETURN n").is_ok());
    assert!(parse("MATCH (n)-[r WHERE r:!S|!T]->(m) RETURN n").is_ok());
}

#[test]
fn test_parenthesized_path_pattern_where() {
    let stmt = parse("MATCH ((n)-[r]->(m) WHERE n.p > m.q AND r:S) RETURN n").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(mc) => {
            assert!(mc.pattern[0].where_expr.is_some());
            assert_eq!(mc.pattern[0].elements.len(), 3);
        }
        other => panic!("expected match, got {other:?}"),
    }
}

#[test]
fn test_path_variable_on_top_level_parenthesized_pattern() {
    // CIP-19 basic-structure #15: `p = ((a)-[r]->(b))`.
    let stmt = parse("MATCH p = ((a)-[r:T]->(b)) RETURN p").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(mc) => {
            assert_eq!(mc.pattern[0].variable.as_deref(), Some("p"));
            assert_eq!(mc.pattern[0].elements.len(), 3);
        }
        other => panic!("expected match, got {other:?}"),
    }
}

#[test]
fn test_path_variable_on_top_level_parenthesized_pattern_with_where() {
    let stmt = parse("MATCH p = ((a)-[r:T]->(b) WHERE a.p > b.q) RETURN p").unwrap();
    match &stmt.clauses[0] {
        Clause::Match(mc) => {
            assert_eq!(mc.pattern[0].variable.as_deref(), Some("p"));
            assert!(mc.pattern[0].where_expr.is_some());
        }
        other => panic!("expected match, got {other:?}"),
    }
}
