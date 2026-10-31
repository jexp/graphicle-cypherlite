//! ADR-0211 (LABS-265): graph-type elements with GQL property declarations
//! (`(:Tag => {name IS STRING})`), `SHOW CURRENT GRAPH TYPE AS GRAPH`
//! (CIP-257), and `ALTER CURRENT GRAPH TYPE SET GRAPH TYPE <name>` -- the
//! supported path from a `CREATE GRAPH TYPE`-declared named type to the
//! database's current graph type.

mod common;

use cypherlite::Value;

use common::{connection, err_text, list, rows, run, scalar_one, string};

/// Property-declared node and edge type elements: the `=>` marker after
/// the identifying label is mandatory before a property block (current
/// Neo4j behavior; CIP-178/181/183), and `IS <value type>` declares the
/// property type.
const SO_GRAPH: &str = "CREATE GRAPH TYPE SOGraph AS {
    (t:Tag => {name IS STRING}),
    (q:Question => {title IS STRING}),
    (q)-[:TAGGED]->(t)
}";

#[test]
fn property_declared_elements_parse_and_show_canonically() {
    let mut c = connection();
    run(&mut c, SO_GRAPH);
    let spec = scalar_one(&mut c, "SHOW GRAPH TYPE SOGraph YIELD body RETURN body");
    assert_eq!(
        spec,
        Value::String(
            "(:`Question` => {`title` :: STRING}), (:`Tag` => {`name` :: STRING}), \
             (:`Question` =>)-[:`TAGGED` =>]->(:`Tag` =>)"
                .into()
        )
    );
}

#[test]
fn property_block_without_implies_is_a_syntax_error() {
    // The LABS-265 ticket's original spelling — `(:Tag {name IS STRING})`
    // without the `=>` marker — is invalid in current Neo4j too: the
    // identifying label must be explicitly denoted (CIP-178/181/183).
    let mut c = connection();
    let err = err_text(c.query("CREATE GRAPH TYPE bad AS { (:Tag {name IS STRING}) }"));
    assert!(err.contains("identifying label"), "{err}");
    let err = err_text(c.query(
        "CREATE GRAPH TYPE bad AS { (t:Tag), (q:Question), (q)-[:TAGGED {w IS FLOAT}]->(t) }",
    ));
    assert!(err.contains("identifying label"), "{err}");
}

#[test]
fn create_graph_type_does_not_make_the_type_current() {
    let mut c = connection();
    run(&mut c, SO_GRAPH);
    // Declared and current stay separate (ADR-0211): the current graph
    // type is still empty until promoted.
    let spec = scalar_one(&mut c, "SHOW CURRENT GRAPH TYPE");
    assert_eq!(spec, string("{}"));
}

#[test]
fn alter_current_graph_type_set_graph_type_promotes_a_declared_type() {
    let mut c = connection();
    run(&mut c, SO_GRAPH);
    run(&mut c, "ALTER CURRENT GRAPH TYPE SET GRAPH TYPE SOGraph");
    let spec = scalar_one(&mut c, "SHOW CURRENT GRAPH TYPE");
    let Value::String(spec) = spec else {
        panic!("expected a specification string, got {spec:?}");
    };
    assert!(spec.starts_with('{') && spec.ends_with('}'), "{spec}");
    assert!(
        spec.contains(":`Question` => {`title` :: STRING}"),
        "{spec}"
    );
    assert!(spec.contains(":`Tag` => {`name` :: STRING}"), "{spec}");
    assert!(spec.contains("-[:`TAGGED` =>]->"), "{spec}");

    // The promoted type's property types become derived constraints
    // (the TAGGED edge type also derives endpoint label constraints).
    let constraints = rows(
        &mut c,
        "SHOW CONSTRAINTS YIELD type, labelsOrTypes, properties \
         WHERE type = 'NODE_PROPERTY_TYPE' \
         RETURN type, labelsOrTypes, properties ORDER BY labelsOrTypes",
    );
    assert_eq!(
        constraints,
        vec![
            vec![
                string("NODE_PROPERTY_TYPE"),
                list(vec![string("Question")]),
                list(vec![string("title")]),
            ],
            vec![
                string("NODE_PROPERTY_TYPE"),
                list(vec![string("Tag")]),
                list(vec![string("name")]),
            ],
        ]
    );
}

#[test]
fn set_graph_type_unknown_name_fails() {
    let mut c = connection();
    let err = err_text(c.query("ALTER CURRENT GRAPH TYPE SET GRAPH TYPE nope"));
    assert!(err.contains("GraphTypeNotFound"), "{err}");
}

#[test]
fn show_current_graph_type_as_graph_returns_elements() {
    let mut c = connection();
    run(&mut c, SO_GRAPH);
    run(&mut c, "ALTER CURRENT GRAPH TYPE SET GRAPH TYPE SOGraph");
    let result = run(&mut c, "SHOW CURRENT GRAPH TYPE AS GRAPH");
    assert_eq!(result.header.columns, vec!["nodes", "relationships"]);
    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    let Value::List(nodes) = &row[0] else {
        panic!("expected a nodes list, got {:?}", row[0]);
    };
    let Value::List(rels) = &row[1] else {
        panic!("expected a relationships list, got {:?}", row[1]);
    };
    // One synthetic node per declared node type (Question, Tag), each
    // labelled NodeElementType with its declared property list.
    assert_eq!(nodes.len(), 2);
    let mut labels = nodes
        .iter()
        .map(|v| {
            let Value::Node(n) = v else {
                panic!("expected a node, got {v:?}");
            };
            assert_eq!(
                n.labels.as_slice(),
                ["NodeElementType".to_string()].as_slice()
            );
            let Some(Value::String(label)) = n.properties.get("label") else {
                panic!("node misses its label property: {n:?}");
            };
            label.to_string()
        })
        .collect::<Vec<_>>();
    labels.sort();
    assert_eq!(labels, vec!["Question", "Tag"]);
    // One synthetic relationship for the declared TAGGED edge type,
    // connecting the Question and Tag virtual nodes.
    assert_eq!(rels.len(), 1);
    let Value::Relationship(r) = &rels[0] else {
        panic!("expected a relationship, got {:?}", rels[0]);
    };
    assert_eq!(r.rel_type, "RELATIONSHIP_ELEMENT_TYPE");
    assert_eq!(
        r.properties.get("relationshipType"),
        Some(&string("TAGGED"))
    );
    assert_eq!(r.properties.get("properties"), Some(&list(vec![])));
}

#[test]
fn show_current_graph_type_as_graph_of_an_empty_type_is_empty() {
    let mut c = connection();
    let result = run(&mut c, "SHOW CURRENT GRAPH TYPE AS GRAPH");
    assert_eq!(result.header.columns, vec!["nodes", "relationships"]);
    assert_eq!(result.rows, vec![vec![list(vec![]), list(vec![])]]);
}

/// The visualization's display form uses the parser's **shape-only** quoting
/// policy: `when` is a keyword spelling but a plain identifier shape, so it
/// stays bare (CIP-257's `composability.feature` expects
/// `properties: ['when :: STRING']`), while a hyphenated name renders
/// backticked (CIP-257's `nodes.feature` expects `` `prop-3` ``). The
/// canonical graph-type text keeps the always-quoted canonical spelling, so a
/// keyword property name is still `` `when` `` where that text is re-parsed.
#[test]
fn show_current_graph_type_as_graph_quotes_by_shape_not_keyword() {
    let mut c = connection();
    run(
        &mut c,
        "CREATE GRAPH TYPE KeywordProps AS { (t:Tag => {`when` IS STRING, `prop-3` IS STRING}) }",
    );
    let canonical = scalar_one(
        &mut c,
        "SHOW GRAPH TYPE KeywordProps YIELD body RETURN body",
    );
    let Value::String(canonical) = canonical else {
        panic!("expected a canonical body string, got {canonical:?}");
    };
    assert!(
        canonical.contains("`when` :: STRING"),
        "canonical text keeps a keyword property quoted: {canonical}"
    );
    assert!(
        canonical.contains("`prop-3` :: STRING"),
        "canonical text keeps a shaped name quoted: {canonical}"
    );

    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET GRAPH TYPE KeywordProps",
    );
    let result = run(&mut c, "SHOW CURRENT GRAPH TYPE AS GRAPH");
    let Value::List(nodes) = &result.rows[0][0] else {
        panic!("expected a nodes list, got {:?}", result.rows[0][0]);
    };
    let Value::Node(node) = &nodes[0] else {
        panic!("expected a node, got {:?}", nodes[0]);
    };
    assert_eq!(
        node.properties.get("properties"),
        Some(&list(vec![
            string("when :: STRING"),
            string("`prop-3` :: STRING"),
        ])),
        "display quotes by shape only: `when` bare, `prop-3` backticked"
    );
}

// ---------------------------------------------------------------------
// ADR-0371: the CIP graph-type trio's preview gates, ungated (this
// process never sets CYPHERLITE_PREVIEW; the gated behavior lives in
// `tests/graph_type_trio.rs`, its own gated process)
// ---------------------------------------------------------------------

/// The authored fail-closed gate error for one disabled preview feature.
fn gated_off_error(feature: &str) -> String {
    format!(
        "SemanticError: preview feature '{feature}' is not enabled: \
         set CYPHERLITE_PREVIEW={feature}"
    )
}

#[test]
fn the_graph_type_trio_fails_closed_ungated() {
    let mut c = connection();
    // CIP-283: a bare `ANY` element-type property (the optional property).
    let err = err_text(c.query("ALTER CURRENT GRAPH TYPE SET { (:Label => {prop :: ANY}) }"));
    assert!(
        err.contains(&gated_off_error("graph-type-opt-props")),
        "{err}"
    );
    // CIP-262: a `DEFAULT ON COMMIT` clause.
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE SET { (:Label => {id :: UUID NOT NULL DEFAULT ON COMMIT uuid()}) }",
    ));
    assert!(
        err.contains(&gated_off_error("graph-type-default-values")),
        "{err}"
    );
    // CIP-266: an implied label naming another in-body node element
    // type's identifying label (inheritance).
    let err = err_text(c.query("ALTER CURRENT GRAPH TYPE SET { (:A => {}), (:B => :A) }"));
    assert!(
        err.contains(&gated_off_error("graph-type-implied-labels")),
        "{err}"
    );
    // The ADD half of the CIP-266 gate: an ADD whose identifying label a
    // stored node element type already implies -- the construct only
    // materializes against stored state, so the executor's graph-type
    // layer fails closed with the same authored error (PAGTE6 ungated).
    let mut c = connection();
    run(
        &mut c,
        "ALTER CURRENT GRAPH TYPE SET { (:A => :B {p1 :: STRING}) }",
    );
    let err = err_text(c.query("ALTER CURRENT GRAPH TYPE ADD { (:B => {p2 :: INTEGER}) }"));
    assert!(
        err.contains(&gated_off_error("graph-type-implied-labels")),
        "{err}"
    );
    // One feature at a time: the three flags are independently
    // enableable, and a construct of one gated feature stays rejected
    // when only another feature is enabled (this process enables
    // none, so every construct above fails closed with its own
    // feature's authored error).
    let mut c = connection();
    let err = err_text(c.query(
        "ALTER CURRENT GRAPH TYPE SET {
            (:A => {p :: ANY}),
            (:B => :A {p :: STRING})
        }",
    ));
    // The front gate reports the first construct it finds: the bare ANY.
    assert!(
        err.contains(&gated_off_error("graph-type-opt-props")),
        "{err}"
    );
}
