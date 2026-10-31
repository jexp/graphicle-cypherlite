// ADR-0169: every keyword is a label or relationship-type name, unquoted,
// with the name kept as written. These tests cover the keywords
// `is_ident_like()` rejects, at every label/type-name read site, in Title
// case (`Order`) and UPPER case (`ORDER`).

use super::*;
use crate::token::{lookup_keyword, CLAUSE_KEYWORDS};

/// Reads the label expression a query shape puts the keyword in.
type Extract = fn(&Statement) -> LabelExpr;
/// Builds the label expression the shape must produce for keyword `k`.
type Expect = fn(&str) -> LabelExpr;

fn keyword_spellings() -> Vec<String> {
    let mut spellings = Vec::new();
    for ty in CLAUSE_KEYWORDS {
        let upper = format!("{ty:?}");
        assert_eq!(lookup_keyword(&upper), ty, "{upper} must lex as {ty:?}");
        spellings.push(format!(
            "{}{}",
            &upper[..1],
            upper[1..].to_ascii_lowercase()
        ));
        spellings.push(upper);
    }
    spellings
}

fn label(name: &str) -> LabelExpr {
    LabelExpr::Label(name.to_string())
}

fn node_label(element: &PatternElement) -> LabelExpr {
    match element {
        PatternElement::Node(n) => n.label_expr.clone().expect("node label expression"),
        other => panic!("expected a node pattern, got {other:?}"),
    }
}

fn rel_type(element: &PatternElement) -> LabelExpr {
    match element {
        PatternElement::Relationship(r) => r.type_expr.clone().expect("relationship type"),
        other => panic!("expected a relationship pattern, got {other:?}"),
    }
}

/// The first node of the leading MATCH pattern.
fn match_node(s: &Statement) -> LabelExpr {
    node_label(&match_clause(s).pattern[0].elements[0])
}

/// The first relationship of the leading MATCH pattern.
fn match_rel(s: &Statement) -> LabelExpr {
    rel_type(&match_clause(s).pattern[0].elements[1])
}

/// The first node of the leading CREATE pattern.
fn create_node(s: &Statement) -> LabelExpr {
    match &s.clauses[0] {
        Clause::Create(c) => node_label(&c.pattern[0].elements[0]),
        other => panic!("expected CREATE, got {other:?}"),
    }
}

/// The first relationship of the leading MERGE pattern.
fn merge_rel(s: &Statement) -> LabelExpr {
    match &s.clauses[0] {
        Clause::Merge(m) => rel_type(&m.pattern.elements[1]),
        other => panic!("expected MERGE, got {other:?}"),
    }
}

fn where_expr(s: &Statement) -> &Expr {
    match_clause(s).where_expr.as_ref().expect("WHERE")
}

/// The label predicate `e` must be, with its `negated` and `strict` flags
/// kept, so `n IS NOT K` stays distinct from a `:!K` label expression.
fn has_label(e: &Expr) -> &HasLabelExpr {
    match e {
        Expr::HasLabel(hl) => hl,
        other => panic!("expected a label predicate, got {other:?}"),
    }
}

/// The label predicate on variable `var`.
fn label_predicate(var: &str, label_expr: LabelExpr, negated: bool, strict: bool) -> HasLabelExpr {
    HasLabelExpr {
        expr: Expr::Variable(var.to_string()),
        label_expr,
        negated,
        strict,
    }
}

/// A SET/REMOVE item's label list, as an `And`.
fn set_labels(s: &Statement) -> LabelExpr {
    match &s.clauses[1] {
        Clause::Set(sc) => LabelExpr::And(sc.items[0].labels.clone()),
        Clause::Remove(rc) => LabelExpr::And(rc.items[0].labels.clone()),
        other => panic!("expected SET or REMOVE, got {other:?}"),
    }
}

fn comprehension_node(s: &Statement, index: usize) -> LabelExpr {
    match return_expr(s) {
        Expr::PatternComprehension(pc) => node_label(&pc.pattern.elements[index]),
        other => panic!("expected a pattern comprehension, got {other:?}"),
    }
}

fn pattern_predicate_node(s: &Statement, index: usize) -> LabelExpr {
    match where_expr(s) {
        Expr::PatternPredicate(pp) => node_label(&pp.pattern.elements[index]),
        other => panic!("expected a pattern predicate, got {other:?}"),
    }
}

/// The first WHEN condition of a simple CASE.
fn case_when(s: &Statement) -> &Expr {
    match return_expr(s) {
        Expr::Case(case) => &case.whens[0].condition,
        other => panic!("expected CASE, got {other:?}"),
    }
}

#[test]
fn test_keyword_label_and_type_names_in_patterns() {
    let shapes: [(&str, Extract, Expect); 25] = [
        ("MATCH (n:{K}) RETURN n", match_node, label),
        ("MATCH ()-[r:{K}]->() RETURN r", match_rel, label),
        ("CREATE (n:{K})", create_node, label),
        ("MERGE (a)-[:{K}]->(b)", merge_rel, label),
        ("MATCH (n) SET n:{K}", set_labels, |k| {
            LabelExpr::And(vec![label(k)])
        }),
        ("MATCH (n) SET n IS {K}", set_labels, |k| {
            LabelExpr::And(vec![label(k)])
        }),
        ("MATCH (n) SET n:A:{K}", set_labels, |k| {
            LabelExpr::And(vec![label("A"), label(k)])
        }),
        ("MATCH (n) REMOVE n:{K}", set_labels, |k| {
            LabelExpr::And(vec![label(k)])
        }),
        ("MATCH (n) REMOVE n:A:{K}", set_labels, |k| {
            LabelExpr::And(vec![label("A"), label(k)])
        }),
        ("MATCH (n:A|{K}) RETURN n", match_node, |k| {
            LabelExpr::Or(vec![label("A"), label(k)])
        }),
        ("MATCH (n:A:{K}) RETURN n", match_node, |k| {
            LabelExpr::And(vec![label("A"), label(k)])
        }),
        ("MATCH (n:{K}:A) RETURN n", match_node, |k| {
            LabelExpr::And(vec![label(k), label("A")])
        }),
        ("MATCH (n:!{K}) RETURN n", match_node, |k| {
            LabelExpr::Not(Box::new(label(k)))
        }),
        ("MATCH (n:A&{K}) RETURN n", match_node, |k| {
            LabelExpr::And(vec![label("A"), label(k)])
        }),
        ("MATCH (n:({K}|A)&B) RETURN n", match_node, |k| {
            LabelExpr::And(vec![LabelExpr::Or(vec![label(k), label("A")]), label("B")])
        }),
        ("MATCH (n IS {K}) RETURN n", match_node, label),
        ("MATCH ()-[:A|{K}]->() RETURN 1", match_rel, |k| {
            LabelExpr::Or(vec![label("A"), label(k)])
        }),
        ("MATCH ()-[:A|:{K}]->() RETURN 1", match_rel, |k| {
            LabelExpr::Or(vec![label("A"), label(k)])
        }),
        ("MATCH ()-[r IS {K}]->() RETURN r", match_rel, label),
        (
            "RETURN [(a)-->(o:{K}) | o]",
            |s| comprehension_node(s, 2),
            label,
        ),
        // The comprehension lookahead skips the first node's labels.
        (
            "RETURN [(o:{K})-->(a) | o]",
            |s| comprehension_node(s, 0),
            label,
        ),
        (
            "RETURN [(o:A:{K} {p: 1})-->(a) | o]",
            |s| comprehension_node(s, 0),
            |k| LabelExpr::And(vec![label("A"), label(k)]),
        ),
        (
            "MATCH (a) WHERE (a)-->(:{K}) RETURN a",
            |s| pattern_predicate_node(s, 2),
            label,
        ),
        // The pattern-predicate lookahead skips the first node's labels.
        (
            "MATCH (a) WHERE (:{K})-->(a) RETURN a",
            |s| pattern_predicate_node(s, 0),
            label,
        ),
        (
            "MATCH (a) WHERE (b:{K}:A)-->(a) RETURN a",
            |s| pattern_predicate_node(s, 0),
            |k| LabelExpr::And(vec![label(k), label("A")]),
        ),
    ];
    for k in keyword_spellings() {
        for (template, extract, expect) in shapes {
            let q = template.replace("{K}", &k);
            assert_eq!(extract(&parse_ok(&q)), expect(&k), "{q}");
        }
    }
}

#[test]
fn test_keyword_label_names_in_label_predicates() {
    // Each row: the shape, where its predicate sits, and the `negated` and
    // `strict` flags the parser sets for that form. `IS NOT [LABELED] K`
    // negates the whole predicate (`negated`) around the plain label `K`;
    // it is not the `:!K` label expression.
    type Find = fn(&Statement) -> &Expr;
    let shapes: [(&str, Find, bool, bool); 7] = [
        ("MATCH (n) WHERE n:{K} RETURN n", where_expr, false, false),
        (
            "MATCH (n) WHERE n IS {K} RETURN n",
            where_expr,
            false,
            false,
        ),
        (
            "MATCH (n) WHERE n IS LABELED {K} RETURN n",
            where_expr,
            false,
            true,
        ),
        (
            "MATCH (n) WHERE n IS NOT {K} RETURN n",
            where_expr,
            true,
            true,
        ),
        (
            "MATCH (n) WHERE n IS NOT LABELED {K} RETURN n",
            where_expr,
            true,
            true,
        ),
        (
            "MATCH (n) RETURN CASE n WHEN :{K} THEN 1 END AS r",
            case_when,
            false,
            false,
        ),
        (
            "MATCH (n) RETURN CASE n WHEN IS {K} THEN 1 END AS r",
            case_when,
            false,
            true,
        ),
    ];
    for k in keyword_spellings() {
        for (template, find, negated, strict) in shapes {
            let q = template.replace("{K}", &k);
            let stmt = parse_ok(&q);
            assert_eq!(
                *has_label(find(&stmt)),
                label_predicate("n", label(&k), negated, strict),
                "{q}"
            );
        }
    }
}

#[test]
fn test_keyword_labels_keep_the_comprehension_pipe_probe() {
    // `label_or_pipe_is_ambiguous` still splits a comprehension's WHERE
    // from its map expression when the label is a keyword, and still reads
    // a keyword after `|` as a disjunct when no map expression follows.
    for k in keyword_spellings() {
        let q = format!("RETURN [x IN [1] WHERE x:{k} | x.p] AS r");
        match return_expr(&parse_ok(&q)) {
            Expr::ListComprehension(lc) => {
                assert_eq!(
                    *has_label(lc.where_expr.as_ref().expect("WHERE")),
                    label_predicate("x", label(&k), false, false),
                    "{q}"
                );
                assert!(matches!(lc.map_expr, Some(Expr::Property(_))), "{q}");
            }
            other => panic!("{q}: expected a list comprehension, got {other:?}"),
        }

        let q = format!("RETURN [x IN [1] WHERE x:A|{k}] AS r");
        match return_expr(&parse_ok(&q)) {
            Expr::ListComprehension(lc) => {
                assert_eq!(
                    *has_label(lc.where_expr.as_ref().expect("WHERE")),
                    label_predicate(
                        "x",
                        LabelExpr::Or(vec![label("A"), label(&k)]),
                        false,
                        false
                    ),
                    "{q}"
                );
                assert!(lc.map_expr.is_none(), "{q}");
            }
            other => panic!("{q}: expected a list comprehension, got {other:?}"),
        }

        let q = format!("RETURN [(a)-->(b) WHERE b:{k} | b.p] AS r");
        match return_expr(&parse_ok(&q)) {
            Expr::PatternComprehension(pc) => {
                assert_eq!(
                    *has_label(pc.where_expr.as_ref().expect("WHERE")),
                    label_predicate("b", label(&k), false, false),
                    "{q}"
                );
                assert!(matches!(pc.map_expr, Expr::Property(_)), "{q}");
            }
            other => panic!("{q}: expected a pattern comprehension, got {other:?}"),
        }
    }
}

#[test]
fn test_keyword_label_names_in_schema_commands_and_hints() {
    for k in keyword_spellings() {
        for template in [
            "CREATE INDEX FOR (n:{K}) ON (n.p)",
            "CREATE INDEX FOR ()-[r:{K}]-() ON (r.p)",
            "CREATE INDEX ON :{K}(p)",
        ] {
            let q = template.replace("{K}", &k);
            match &parse_ok(&q).clauses[0] {
                Clause::CreateIndex(ci) => assert_eq!(ci.labels, vec![k.clone()], "{q}"),
                other => panic!("{q}: expected CREATE INDEX, got {other:?}"),
            }
        }

        let q = format!("DROP INDEX ON :{k}(p)");
        match &parse_ok(&q).clauses[0] {
            Clause::DropIndex(di) => {
                assert_eq!(di.legacy_target, Some((k.clone(), "p".to_string())), "{q}")
            }
            other => panic!("{q}: expected DROP INDEX, got {other:?}"),
        }

        let q = format!("CREATE CONSTRAINT FOR (n:{k}) REQUIRE n.q IS UNIQUE");
        match &parse_ok(&q).clauses[0] {
            Clause::CreateConstraint(cc) => assert_eq!(cc.label, k, "{q}"),
            other => panic!("{q}: expected CREATE CONSTRAINT, got {other:?}"),
        }

        // CIP-177 `REQUIRE <alias>:<Label>`.
        let q = format!("CREATE CONSTRAINT FOR (n:A) REQUIRE n:{k}");
        match &parse_ok(&q).clauses[0] {
            Clause::CreateConstraint(cc) => {
                assert_eq!(cc.enforced_label.as_deref(), Some(k.as_str()), "{q}")
            }
            other => panic!("{q}: expected CREATE CONSTRAINT, got {other:?}"),
        }

        // USING hints are parsed and discarded.
        for q in [
            format!("MATCH (n:{k}) USING INDEX n:{k}(p) WHERE n.p = 1 RETURN n"),
            format!("MATCH (n:{k}) USING INDEX SEEK n:{k}(p) WHERE n.p = 1 RETURN n"),
            format!("MATCH (n:{k}) USING SCAN n:{k} RETURN n"),
        ] {
            assert_eq!(match_node(&parse_ok(&q)), label(&k), "{q}");
        }
    }
}

#[test]
fn test_keyword_label_names_in_graph_types() {
    for k in keyword_spellings() {
        let q = format!(
            "CREATE GRAPH TYPE g {{
                (p:A => :{k}&L),
                (p2:D => :L&:{k}),
                (q:{k} => {{x :: STRING}}),
                (:B =>)-[:{k} =>]->(:C =>),
                CONSTRAINT FOR (c IS {k}) REQUIRE c.x IS UNIQUE
            }}"
        );
        let stmt = parse_ok(&q);
        let Clause::CreateGraphType(cgt) = &stmt.clauses[0] else {
            panic!("{q}: expected CREATE GRAPH TYPE");
        };
        let node_type = |alias: &str| {
            cgt.body
                .node_types
                .iter()
                .find(|n| n.alias.as_deref() == Some(alias))
                .unwrap_or_else(|| panic!("{q}: node type {alias}"))
        };
        assert_eq!(node_type("p").implied_labels, vec![k.clone(), "L".into()]);
        assert_eq!(node_type("p2").implied_labels, vec!["L".into(), k.clone()]);
        assert_eq!(node_type("q").identifying_label, k);
        assert_eq!(cgt.body.edge_types[0].identifying_label, k);
        assert_eq!(
            cgt.body.constraints[0].target,
            GraphTypeElementRef::InSitu {
                alias: Some("c".to_string()),
                label: k.clone(),
                implies: false,
            },
            "{q}"
        );
    }
}

#[test]
fn test_keyword_names_outside_label_positions_still_fail() {
    // ADR-0191 widens every keyword symbolic-name position: variables
    // (see `keyword_variables`), aliases and references (see
    // `keyword_aliases`) and map keys (see `keyword_map_keys`). What is
    // left here is the label position, which still needs a name token
    // (ADR-0169): `MATCH (n:)` must fail at the offending token (line,
    // column, token), not at a later one. A `ParseError` is always a
    // SyntaxError unless tagged as an administration command.
    let (q, line, col, token) = ("MATCH (n:) RETURN n", 1, 10, ")");
    assert!(
        q[col - 1..].starts_with(token),
        "{q}: no {token:?} at {col}"
    );
    let err = parse(q).expect_err(q);
    assert_eq!((err.line, err.col), (line, col), "{q}: {err}");
    assert_ne!(err.detail, Some(UNSUPPORTED_ADMINISTRATION_DETAIL), "{q}");
}

#[test]
fn test_clause_keyword_after_a_label_introducer_is_a_label() {
    // A keyword directly after `:`, `IS`, `|`, `&` or `!` is always a label
    // name, even across a line break where it would otherwise start the
    // next clause, as in Neo4j. The clause that follows loses that keyword:
    // `OPTIONAL` becomes the WHERE label and a plain MATCH follows; `DETACH`
    // becomes the SET label and a plain DELETE follows.
    for (predicate, expected) in [
        ("n:", label("OPTIONAL")),
        ("n IS", label("OPTIONAL")),
        ("n:A|", LabelExpr::Or(vec![label("A"), label("OPTIONAL")])),
        ("n:A&", LabelExpr::And(vec![label("A"), label("OPTIONAL")])),
        ("n:!", LabelExpr::Not(Box::new(label("OPTIONAL")))),
    ] {
        let q = format!("MATCH (n) WHERE {predicate}\nOPTIONAL MATCH (m) RETURN m");
        let stmt = parse_ok(&q);
        assert_eq!(stmt.clauses.len(), 3, "{q}: {stmt:?}");
        assert_eq!(
            *has_label(where_expr(&stmt)),
            label_predicate("n", expected, false, false),
            "{q}"
        );
        match &stmt.clauses[1] {
            Clause::Match(m) => {
                assert!(!m.optional, "{q}");
                assert!(
                    matches!(
                        &m.pattern[0].elements[..],
                        [PatternElement::Node(node)]
                            if node.variable.as_deref() == Some("m") && node.label_expr.is_none()
                    ),
                    "{q}: {m:?}"
                );
            }
            other => panic!("{q}: expected MATCH, got {other:?}"),
        }
        assert!(matches!(stmt.clauses[2], Clause::Return(_)), "{q}");
    }

    for introducer in [":", " IS "] {
        let q = format!("MATCH (n) SET n{introducer}\nDETACH DELETE n");
        let stmt = parse_ok(&q);
        assert_eq!(stmt.clauses.len(), 3, "{q}: {stmt:?}");
        assert_eq!(
            set_labels(&stmt),
            LabelExpr::And(vec![label("DETACH")]),
            "{q}"
        );
        match &stmt.clauses[2] {
            Clause::Delete(d) => {
                assert!(!d.detach, "{q}");
                assert_eq!(d.expressions, vec![Expr::Variable("n".to_string())], "{q}");
            }
            other => panic!("{q}: expected DELETE, got {other:?}"),
        }
    }
}

#[test]
fn test_is_suffixes_keep_precedence_over_keyword_labels() {
    // The IS-suffix branches run before the label branch, so these keep
    // their predicate meaning.
    type IsExpected = fn(&Expr) -> bool;
    let cases: [(&str, IsExpected); 7] = [
        (
            "RETURN x IS NULL AS r",
            |e| matches!(e, Expr::IsNull(n) if !n.negated),
        ),
        (
            "RETURN x IS NOT NULL AS r",
            |e| matches!(e, Expr::IsNull(n) if n.negated),
        ),
        ("RETURN x IS :: INTEGER AS r", |e| {
            matches!(e, Expr::IsType(_))
        }),
        ("RETURN x IS TYPED INTEGER AS r", |e| {
            matches!(e, Expr::IsType(_))
        }),
        ("RETURN x IS NOT NORMALIZED AS r", |e| {
            matches!(e, Expr::IsNormalized(_))
        }),
        (
            "RETURN x IS LABELED A AS r",
            |e| matches!(e, Expr::HasLabel(hl) if hl.strict && !hl.negated),
        ),
        (
            "RETURN x IS NOT LABELED A AS r",
            |e| matches!(e, Expr::HasLabel(hl) if hl.strict && hl.negated),
        ),
    ];
    for (q, is_expected) in cases {
        let stmt = parse_ok(q);
        assert!(
            is_expected(return_expr(&stmt)),
            "{q}: {:?}",
            return_expr(&stmt)
        );
    }
}
