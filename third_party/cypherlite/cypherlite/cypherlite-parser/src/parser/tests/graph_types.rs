use super::*;
use crate::YieldMode;

/// The verb body of a parsed `ALTER CURRENT GRAPH TYPE` — the tests here
/// only parse the inline-body forms, so `Named` panics.
fn alter_body(agt: &AlterCurrentGraphTypeClause) -> &GraphTypeBody {
    match &agt.target {
        AlterCurrentGraphTypeTarget::Body(body) => body,
        AlterCurrentGraphTypeTarget::Named(name) => {
            panic!("expected an inline body, got named type {name}")
        }
    }
}

// ---- CIP-187: ALTER CURRENT GRAPH TYPE SET body parsing ----

#[test]
fn test_alter_current_graph_type_empty_body() {
    let stmt = parse("ALTER CURRENT GRAPH TYPE SET { }").unwrap();
    match &stmt.clauses[0] {
        Clause::AlterCurrentGraphType(agt) => {
            assert_eq!(agt.verb, AlterGraphTypeVerb::Set);
            assert!(alter_body(agt).node_types.is_empty());
            assert!(alter_body(agt).edge_types.is_empty());
        }
        other => panic!("expected AlterCurrentGraphType clause, got {other:?}"),
    }
}

#[test]
fn test_alter_current_graph_type_node_and_edge_types() {
    let stmt = parse(
        "ALTER CURRENT GRAPH TYPE SET {
            (:Label => :A {dummy1 :: STRING}),
            (:Author => {dummy2 :: STRING}),
            (:Book => {dummy3 :: STRING}),
            (:Author=>)-[:WROTE=>]->(:Book=>)
        }",
    )
    .unwrap();
    match &stmt.clauses[0] {
        Clause::AlterCurrentGraphType(agt) => {
            assert_eq!(alter_body(agt).node_types.len(), 3);
            let label_nt = alter_body(agt)
                .node_types
                .iter()
                .find(|n| n.identifying_label == "Label")
                .expect("Label node type");
            assert_eq!(label_nt.implied_labels, vec!["A".to_string()]);
            assert_eq!(alter_body(agt).edge_types.len(), 1);
            assert_eq!(alter_body(agt).edge_types[0].identifying_label, "WROTE");
            assert_eq!(
                alter_body(agt).edge_types[0].source,
                GraphTypeElementRef::InSitu {
                    alias: None,
                    label: "Author".to_string(),
                    implies: true,
                }
            );
            assert_eq!(
                alter_body(agt).edge_types[0].target,
                GraphTypeElementRef::InSitu {
                    alias: None,
                    label: "Book".to_string(),
                    implies: true,
                }
            );
        }
        other => panic!("expected AlterCurrentGraphType clause, got {other:?}"),
    }
}

#[test]
fn test_alter_current_graph_type_unlabeled_endpoint_has_no_source_constraint() {
    let stmt = parse(
        "ALTER CURRENT GRAPH TYPE SET {
            ()-[:REL=>]->(:Target=>)
        }",
    )
    .unwrap();
    match &stmt.clauses[0] {
        Clause::AlterCurrentGraphType(agt) => {
            assert_eq!(alter_body(agt).edge_types.len(), 1);
            assert_eq!(
                alter_body(agt).edge_types[0].source,
                GraphTypeElementRef::Any
            );
            assert_eq!(
                alter_body(agt).edge_types[0].target,
                GraphTypeElementRef::InSitu {
                    alias: None,
                    label: "Target".to_string(),
                    implies: true,
                }
            );
        }
        other => panic!("expected AlterCurrentGraphType clause, got {other:?}"),
    }
}

#[test]
fn test_alter_current_graph_type_verbs() {
    for (verb_kw, expected) in [
        ("SET", AlterGraphTypeVerb::Set),
        ("ADD", AlterGraphTypeVerb::Add),
        ("ALTER", AlterGraphTypeVerb::Alter),
        ("DROP", AlterGraphTypeVerb::Drop),
    ] {
        let stmt = parse(&format!("ALTER CURRENT GRAPH TYPE {verb_kw} {{ }}")).unwrap();
        match &stmt.clauses[0] {
            Clause::AlterCurrentGraphType(agt) => assert_eq!(agt.verb, expected),
            other => panic!("expected AlterCurrentGraphType clause, got {other:?}"),
        }
    }
}

#[test]
fn test_graph_type_bare_constraint_drop_only_under_drop() {
    // CIP-182: a DROP body reads each bare `CONSTRAINT <name>` (a keyword
    // name such as `FOR` included) into `drop_constraint_names`.
    let stmt =
        parse("ALTER CURRENT GRAPH TYPE DROP { CONSTRAINT acct_id, CONSTRAINT FOR }").unwrap();
    let Clause::AlterCurrentGraphType(agt) = &stmt.clauses[0] else {
        panic!("expected AlterCurrentGraphType clause");
    };
    assert_eq!(alter_body(agt).drop_constraint_names, ["acct_id", "FOR"]);
    assert!(alter_body(agt).constraints.is_empty());

    // Every other verb, CREATE GRAPH TYPE and the restore entry point
    // included, rejects the same bare form with a plain syntax error.
    for q in [
        "ALTER CURRENT GRAPH TYPE SET { CONSTRAINT acct_id }",
        "ALTER CURRENT GRAPH TYPE ADD { CONSTRAINT acct_id }",
        "ALTER CURRENT GRAPH TYPE ALTER { CONSTRAINT acct_id }",
        "CREATE GRAPH TYPE gt { CONSTRAINT acct_id }",
        "CREATE GRAPH TYPE gt AS { (:City => {name :: STRING}), CONSTRAINT a, CONSTRAINT b }",
    ] {
        let err = parse(q).expect_err(q);
        assert_eq!(err.detail, None, "{q}: {}", err.message);
    }
    assert!(parse_graph_type_body("CONSTRAINT acct_id").is_err());

    // An unnamed specification's `FOR` is not read as a bare name.
    let stmt =
        parse("CREATE GRAPH TYPE gt { CONSTRAINT FOR (s:Student) REQUIRE s.name IS KEY }").unwrap();
    let Clause::CreateGraphType(cgt) = &stmt.clauses[0] else {
        panic!("expected CreateGraphType clause");
    };
    assert!(cgt.body.drop_constraint_names.is_empty());
    assert_eq!(cgt.body.constraints.len(), 1);
    assert_eq!(cgt.body.constraints[0].name, None);

    // CIP-183 drop [12]: a DROP body cannot drop that unnamed specification,
    // but still takes a named one and a type's own unnamed constraints.
    let q =
        "ALTER CURRENT GRAPH TYPE DROP { CONSTRAINT FOR (n:Person =>) REQUIRE (n.name) IS KEY }";
    let err = parse(q).expect_err(q);
    assert_eq!(err.detail, None, "{q}: {}", err.message);
    for q in [
        "ALTER CURRENT GRAPH TYPE DROP { CONSTRAINT k FOR (n:Person =>) REQUIRE n.name IS KEY }",
        "ALTER CURRENT GRAPH TYPE DROP { (:Person => {name :: STRING IS KEY}) }",
        "ALTER CURRENT GRAPH TYPE DROP { (p:Person => {name :: STRING}) REQUIRE p.name IS KEY }",
    ] {
        parse(q).expect(q);
    }
}

#[test]
fn test_create_graph_type_basic() {
    let stmt = parse(
        "CREATE GRAPH TYPE gt {
            (p:Person => {name :: STRING})
        }",
    )
    .unwrap();
    match &stmt.clauses[0] {
        Clause::CreateGraphType(cgt) => {
            assert_eq!(cgt.name, "gt");
            assert_eq!(cgt.body.node_types.len(), 1);
            assert_eq!(cgt.body.node_types[0].identifying_label, "Person");
            assert_eq!(cgt.body.node_types[0].alias.as_deref(), Some("p"));
            assert!(!cgt.via_as);
        }
        other => panic!("expected CreateGraphType clause, got {other:?}"),
    }
}

#[test]
fn test_create_graph_type_as_form() {
    let stmt = parse("CREATE GRAPH TYPE gt AS { }").unwrap();
    match &stmt.clauses[0] {
        Clause::CreateGraphType(cgt) => {
            assert_eq!(cgt.name, "gt");
            assert!(cgt.via_as);
        }
        other => panic!("expected CreateGraphType clause, got {other:?}"),
    }
}

#[test]
fn test_show_graph_type() {
    let stmt = parse("SHOW GRAPH TYPE gt YIELD name, body RETURN body").unwrap();
    match &stmt.clauses[0] {
        Clause::ShowGraphType(sgt) => {
            assert_eq!(sgt.name, "gt");
            assert_eq!(sgt.yields.names().collect::<Vec<_>>(), vec!["name", "body"]);
        }
        other => panic!("expected ShowGraphType clause, got {other:?}"),
    }
}

#[test]
fn test_show_current_graph_type() {
    let stmt = parse("SHOW CURRENT GRAPH TYPE").unwrap();
    match &stmt.clauses[0] {
        Clause::ShowCurrentGraphType(c) => assert_eq!(c.yields.mode(), YieldMode::Default),
        other => panic!("expected ShowCurrentGraphType clause, got {other:?}"),
    }
}

#[test]
fn test_show_current_graph_type_with_yield() {
    let stmt = parse("SHOW CURRENT GRAPH TYPE YIELD specification").unwrap();
    match &stmt.clauses[0] {
        Clause::ShowCurrentGraphType(c) => {
            assert_eq!(c.yields.names().collect::<Vec<_>>(), vec!["specification"])
        }
        other => panic!("expected ShowCurrentGraphType clause, got {other:?}"),
    }
}

#[test]
fn test_show_current_graph_type_preserves_paging_stages() {
    let stmt =
        parse("SHOW CURRENT GRAPH TYPE YIELD specification ORDER BY specification SKIP 1 LIMIT 2")
            .unwrap();
    let Clause::ShowCurrentGraphType(c) = &stmt.clauses[0] else {
        panic!("expected ShowCurrentGraphType");
    };
    assert_eq!(c.stages.len(), 3);
    assert!(matches!(c.stages[0], PageStage::OrderBy(_)));
    assert!(matches!(c.stages[1], PageStage::Skip(_)));
    assert!(matches!(c.stages[2], PageStage::Limit(_)));
}

/// The YIELD paging sub-grammar is single-sourced through
/// `Parser::parse_page_stages`: OFFSET is a SKIP synonym in SHOW paging
/// too (CIP-168: OFFSET is usable everywhere SKIP is) -- decided once for
/// the standalone paging clause and all three SHOW parsers, so `SHOW
/// CURRENT GRAPH TYPE ... OFFSET` parses the OFFSET into a Skip stage
/// instead of leaving it to a following standalone paging clause.
#[test]
fn test_show_current_graph_type_offset_is_a_skip_stage() {
    let stmt = parse("SHOW CURRENT GRAPH TYPE YIELD specification OFFSET 1").unwrap();
    let Clause::ShowCurrentGraphType(c) = &stmt.clauses[0] else {
        panic!("expected ShowCurrentGraphType");
    };
    assert_eq!(c.stages.len(), 1);
    assert!(matches!(c.stages[0], PageStage::Skip(_)));
    assert_eq!(stmt.clauses.len(), 1);
}

// ---- ADR-0211: GQL `IS` property declarations + SET GRAPH TYPE <name> ----

#[test]
fn test_graph_type_node_property_block_requires_implies() {
    // Current Neo4j (and CIP-178/183): without the `=>` marker the label is
    // not an explicitly denoted identifying label, so the property block is
    // a compile-time syntax error.
    let err = parse("CREATE GRAPH TYPE SOGraph AS { (:Tag {name IS STRING}) }").unwrap_err();
    assert!(
        err.to_string().contains("identifying label"),
        "unexpected error: {err}"
    );

    let stmt = parse("CREATE GRAPH TYPE SOGraph AS { (:Tag => {name IS STRING}) }").unwrap();
    let Clause::CreateGraphType(cgt) = &stmt.clauses[0] else {
        panic!("expected CreateGraphType clause");
    };
    assert_eq!(cgt.body.node_types.len(), 1);
    let nt = &cgt.body.node_types[0];
    assert_eq!(nt.identifying_label, "Tag");
    assert!(nt.implied_labels.is_empty());
    assert_eq!(nt.properties.len(), 1);
    assert_eq!(nt.properties[0].name, "name");
    assert_eq!(
        nt.properties[0].value_type,
        TypeExpr::Predefined {
            kind: PredefinedKind::String,
            not_null: false,
        }
    );
}

#[test]
fn test_graph_type_property_is_forms() {
    // `IS <type>`, `IS :: <type>`, and `IS <type> IS KEY` (type introducer
    // followed by the inline constraint) all parse; `IS KEY` with no value
    // type stays an error.
    let stmt = parse(
        "CREATE GRAPH TYPE gt { (:N => {a IS STRING, b IS :: INTEGER NOT NULL, c IS DATE IS KEY}) }",
    )
    .unwrap();
    let Clause::CreateGraphType(cgt) = &stmt.clauses[0] else {
        panic!("expected CreateGraphType clause");
    };
    let props = &cgt.body.node_types[0].properties;
    assert_eq!(props.len(), 3);
    assert_eq!(
        props[1].value_type,
        TypeExpr::Predefined {
            kind: PredefinedKind::Integer,
            not_null: true,
        }
    );
    assert!(props[2].inline_constraint.is_some());
    assert!(parse("CREATE GRAPH TYPE gt { (:N => {a IS KEY}) }").is_err());
}

#[test]
fn test_graph_type_edge_property_block_requires_implies() {
    // Same rule on edges: a property block without `=>` is rejected
    // (CIP-181); both the endpoint and the edge label need the marker.
    assert!(parse(
        "CREATE GRAPH TYPE gt AS {
            (t:Tag), (q:Question => {title IS STRING}),
            (q)-[:TAGGED {weight IS FLOAT}]->(t)
        }",
    )
    .is_err());
    assert!(parse(
        "CREATE GRAPH TYPE gt AS {
            (t:Tag), (q:Question {title IS STRING}),
            (q)-[:TAGGED => {weight IS FLOAT}]->(t)
        }",
    )
    .is_err());

    let stmt = parse(
        "CREATE GRAPH TYPE SOGraph AS {
            (t:Tag), (q:Question => {title IS STRING}),
            (q)-[:TAGGED => {weight IS FLOAT}]->(t)
        }",
    )
    .unwrap();
    let Clause::CreateGraphType(cgt) = &stmt.clauses[0] else {
        panic!("expected CreateGraphType clause");
    };
    assert_eq!(cgt.body.node_types.len(), 2);
    assert_eq!(cgt.body.edge_types.len(), 1);
    let et = &cgt.body.edge_types[0];
    assert_eq!(et.identifying_label, "TAGGED");
    assert_eq!(et.properties.len(), 1);
    assert_eq!(et.properties[0].name, "weight");
    assert_eq!(
        et.properties[0].value_type,
        TypeExpr::Predefined {
            kind: PredefinedKind::Float,
            not_null: false,
        }
    );
}

#[test]
fn test_graph_type_alias_only_property_block_still_rejected() {
    assert!(parse("CREATE GRAPH TYPE gt { (p {name IS STRING}) }").is_err());
}

#[test]
fn test_alter_current_graph_type_set_graph_type_name() {
    let stmt = parse("ALTER CURRENT GRAPH TYPE SET GRAPH TYPE SOGraph").unwrap();
    let Clause::AlterCurrentGraphType(agt) = &stmt.clauses[0] else {
        panic!("expected AlterCurrentGraphType clause");
    };
    assert_eq!(agt.verb, AlterGraphTypeVerb::Set);
    assert_eq!(
        agt.target,
        AlterCurrentGraphTypeTarget::Named("SOGraph".into())
    );

    // Only SET takes the named-type form; ADD/ALTER/DROP still expect a body.
    for q in [
        "ALTER CURRENT GRAPH TYPE ADD GRAPH TYPE x",
        "ALTER CURRENT GRAPH TYPE ALTER GRAPH TYPE x",
        "ALTER CURRENT GRAPH TYPE DROP GRAPH TYPE x",
    ] {
        assert!(parse(q).is_err(), "{q}");
    }
}

// ---- CIP-262: the DEFAULT ON COMMIT clause of a <property type> ----

#[test]
fn test_property_default_on_commit_parses_with_its_expression() {
    let stmt = parse(
        "ALTER CURRENT GRAPH TYPE SET {
            (:Label => {id :: UUID NOT NULL DEFAULT ON COMMIT uuid()}),
            ()-[:REL => {prop :: STRING DEFAULT ON COMMIT \"Hello\"}]->()
        }",
    )
    .unwrap();
    match &stmt.clauses[0] {
        Clause::AlterCurrentGraphType(agt) => {
            let node = alter_body(agt)
                .node_types
                .iter()
                .find(|n| n.identifying_label == "Label")
                .expect("Label node type");
            let prop = &node.properties[0];
            assert_eq!(prop.name, "id");
            // The value expression is the ordinary expression parser's
            // (grammar-only here; the shape is the semantic layer's).
            assert!(
                matches!(&prop.default_value, Some(Expr::FunctionCall(call))
                    if call.name == "uuid" && call.args.is_empty()),
                "expected uuid(), got {:?}",
                prop.default_value
            );
            let edge = &alter_body(agt).edge_types[0];
            let prop = &edge.properties[0];
            assert!(
                matches!(&prop.default_value, Some(Expr::Literal(
                    LiteralValue::String(s)
                )) if s == "Hello"),
                "expected the string literal, got {:?}",
                prop.default_value
            );
        }
        other => panic!("expected AlterCurrentGraphType clause, got {other:?}"),
    }
}

#[test]
fn test_property_without_default_has_none() {
    let stmt = parse("ALTER CURRENT GRAPH TYPE SET { (:Label => {p :: STRING}) }").unwrap();
    match &stmt.clauses[0] {
        Clause::AlterCurrentGraphType(agt) => {
            assert!(alter_body(agt).node_types[0].properties[0]
                .default_value
                .is_none());
        }
        other => panic!("expected AlterCurrentGraphType clause, got {other:?}"),
    }
}

#[test]
fn test_default_requires_on_commit() {
    // `DEFAULT` without `ON COMMIT` is a syntax error (the clause's
    // spelling is fixed, CIP-262 grammar).
    let err = parse("ALTER CURRENT GRAPH TYPE SET { (:Label => {p :: STRING DEFAULT 'x'}) }")
        .unwrap_err();
    assert!(
        err.message.contains("identifier") || err.message.contains("expected"),
        "{err}"
    );
    let err = parse("ALTER CURRENT GRAPH TYPE SET { (:Label => {p :: STRING DEFAULT ON 'x'}) }")
        .unwrap_err();
    assert!(err.message.contains("COMMIT"), "{err}");
}
