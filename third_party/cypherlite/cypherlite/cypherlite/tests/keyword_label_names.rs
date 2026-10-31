//! LABS-154 / ADR-0169: any keyword is accepted unquoted as a label or
//! relationship-type name (`MATCH (o:Order)`, `-[:ORDER]->`, `SET n:Where`,
//! `n IS Order`, `:A|Order`), matching Neo4j 2026.09. The unquoted spelling
//! names the same label or type as the backticked one. These tests pin the
//! behavior end-to-end through the public API, starting with the northwind
//! graph-examples walkthrough queries that found the gap.

mod common;

use cypherlite::Value;
use cypherlite_parser::token::{lookup_keyword, CLAUSE_KEYWORDS};

use common::{connection, rows, run, string};

/// A small northwind graph with unquoted `:Order` labels. Maria Anders
/// (ALFKI) orders Produce only, Ana Trujillo (ANATR) orders Produce and
/// Beverages, Antonio Moreno (ANTON) orders no Produce, and order 11078
/// has no matching customer.
const NORTHWIND_FIXTURE: &str = "\
CREATE (:Customer {customerID: 'ALFKI', contactName: 'Maria Anders'}),
       (:Customer {customerID: 'ANATR', contactName: 'Ana Trujillo'}),
       (:Customer {customerID: 'ANTON', contactName: 'Antonio Moreno'}),
       (produce:Category {categoryID: '7', categoryName: 'Produce'}),
       (beverages:Category {categoryID: '1', categoryName: 'Beverages'}),
       (tofu:Product {productID: '14', productName: 'Tofu'}),
       (apples:Product {productID: '51', productName: 'Manjimup Dried Apples'}),
       (chai:Product {productID: '1', productName: 'Chai'}),
       (tofu)-[:PART_OF]->(produce),
       (apples)-[:PART_OF]->(produce),
       (chai)-[:PART_OF]->(beverages),
       (:Order {orderID: '10643', customerID: 'ALFKI'})-[:ORDERS {quantity: 15}]->(tofu),
       (o2:Order {orderID: '10692', customerID: 'ALFKI'})-[:ORDERS {quantity: 20}]->(tofu),
       (o2)-[:ORDERS {quantity: 21}]->(apples),
       (o3:Order {orderID: '10308', customerID: 'ANATR'})-[:ORDERS {quantity: 10}]->(apples),
       (o3)-[:ORDERS {quantity: 6}]->(chai),
       (:Order {orderID: '10365', customerID: 'ANTON'})-[:ORDERS {quantity: 30}]->(chai),
       (:Order {orderID: '11078', customerID: 'NOCUST'})-[:ORDERS {quantity: 99}]->(tofu)";

/// The `[source,cypher]` block at line 168 of `northwind-old.adoc`
/// (neo4j-graph-examples/northwind), verbatim: connect customers to orders.
const NORTHWIND_LINE_168: &str = "\
MATCH (c:Customer),(o:Order)
WHERE c.customerID = o.customerID
CREATE (c)-[:PURCHASED]->(o)";

/// The `[source,cypher]` block at line 208 of `northwind-old.adoc`
/// (neo4j-graph-examples/northwind), verbatim: total quantity per customer
/// in the "Produce" category.
const NORTHWIND_LINE_208: &str = r#"MATCH (cust:Customer)-[:PURCHASED]->(:Order)-[o:ORDERS]->(p:Product),
      (p)-[:PART_OF]->(c:Category {categoryName:"Produce"})
RETURN DISTINCT cust.contactName as CustomerName, SUM(o.quantity) AS TotalProductsPurchased"#;

/// The single integer `query` returns.
fn single_int(c: &mut cypherlite::Connection, query: &str) -> i64 {
    match &rows(c, query)[..] {
        [row] => match &row[..] {
            [Value::Int(n)] => *n,
            other => panic!("expected one integer column, got {other:?}\n{query}"),
        },
        other => panic!("expected one row, got {other:?}\n{query}"),
    }
}

/// A `labels(n)` list as sorted strings, so assertions do not depend on
/// label order.
fn sorted_labels(value: &Value) -> Vec<String> {
    let Value::List(items) = value else {
        panic!("expected a list of labels, got {value:?}")
    };
    let mut labels: Vec<String> = items
        .iter()
        .map(|item| match item {
            Value::String(label) => String::from(&**label),
            other => panic!("expected a label string, got {other:?}"),
        })
        .collect();
    labels.sort();
    labels
}

/// The northwind walkthrough (`northwind-old.adoc`) runs as written.
///
/// Every expected value below comes from running this identical fixture,
/// then the line-168 and line-208 queries, then the PURCHASED count on
/// Neo4j (docker image neo4j:2026.09-community, 2026-10-06, Query API v2).
/// Neo4j reported `nodesCreated: 13`, `relationshipsCreated: 10` and
/// `propertiesSet: 33` for the fixture, `relationshipsCreated: 4` for line
/// 168, the rows `["Maria Anders", 56]` and `["Ana Trujillo", 10]` for line
/// 208, and a PURCHASED count of 4. (The fixture's `labelsAdded` is not
/// compared: Neo4j counts label additions per node, CypherLite counts newly
/// live label names as the TCK `+labels` side effect does.)
#[test]
fn northwind_walkthrough_queries_run_verbatim() {
    let mut c = connection();

    let fixture = run(&mut c, NORTHWIND_FIXTURE);
    assert_eq!(fixture.stats.nodes_created, 13);
    assert_eq!(fixture.stats.relationships_created, 10);
    assert_eq!(fixture.stats.properties_set, 33);

    let purchased = run(&mut c, NORTHWIND_LINE_168);
    assert_eq!(purchased.stats.relationships_created, 4);
    assert_eq!(
        single_int(&mut c, "MATCH ()-[r:PURCHASED]->(:Order) RETURN count(r)"),
        4
    );

    let result = run(&mut c, NORTHWIND_LINE_208);
    assert_eq!(
        result.header.columns,
        vec![
            "CustomerName".to_string(),
            "TotalProductsPurchased".to_string()
        ]
    );
    // Line 208 has no ORDER BY: compare the rows order-insensitively.
    let mut produce_totals = result.rows;
    produce_totals.sort_by_key(|row| match &row[0] {
        Value::String(name) => name.clone(),
        other => panic!("expected a customer name, got {other:?}"),
    });
    assert_eq!(
        produce_totals,
        vec![
            vec![string("Ana Trujillo"), Value::Int(10)],
            vec![string("Maria Anders"), Value::Int(56)],
        ]
    );
}

/// A node created with an unquoted keyword label is found by the
/// backticked spelling and the other way round: both name the same label.
#[test]
fn unquoted_and_backticked_keyword_labels_match_each_other() {
    for (create_label, match_label) in [("Order", "`Order`"), ("`Order`", "Order")] {
        let mut c = connection();
        run(
            &mut c,
            &format!("CREATE (:{create_label} {{orderID: 10643}})"),
        );
        assert_eq!(
            rows(
                &mut c,
                &format!("MATCH (n:{match_label}) RETURN n.orderID, labels(n)")
            ),
            vec![vec![
                Value::Int(10643),
                Value::List(vec![string("Order")].into())
            ]],
            "CREATE (:{create_label}), MATCH (n:{match_label})"
        );
    }
}

/// A relationship created with an unquoted keyword type reports that type,
/// and is found by the unquoted and the backticked spelling.
#[test]
fn keyword_relationship_type_reports_its_name() {
    let mut c = connection();
    let created = run(
        &mut c,
        "CREATE (:Customer)-[r:ORDER {quantity: 15}]->(:Product) RETURN type(r)",
    );
    assert_eq!(created.rows, vec![vec![string("ORDER")]]);
    assert_eq!(created.stats.relationships_created, 1);

    for query in [
        "MATCH ()-[r:ORDER]->() RETURN type(r), r.quantity",
        "MATCH ()-[r:`ORDER`]->() RETURN type(r), r.quantity",
    ] {
        assert_eq!(
            rows(&mut c, query),
            vec![vec![string("ORDER"), Value::Int(15)]],
            "{query}"
        );
    }
}

/// `SET n:Where` adds the keyword label and `REMOVE n:Where` removes it.
#[test]
fn set_then_remove_keyword_label() {
    let mut c = connection();
    run(&mut c, "CREATE (:Customer {name: 'ALFKI'})");

    let set = run(&mut c, "MATCH (n:Customer) SET n:Where RETURN labels(n)");
    assert_eq!(set.stats.labels_added, 1);
    assert_eq!(sorted_labels(&set.rows[0][0]), ["Customer", "Where"]);
    assert_eq!(single_int(&mut c, "MATCH (n:Where) RETURN count(n)"), 1);

    let removed = run(&mut c, "MATCH (n:Customer) REMOVE n:Where RETURN labels(n)");
    assert_eq!(removed.stats.labels_removed, 1);
    assert_eq!(sorted_labels(&removed.rows[0][0]), ["Customer"]);
    assert_eq!(single_int(&mut c, "MATCH (n:Where) RETURN count(n)"), 0);
}

/// `n:Order` and `n IS Order` are true for an Order node and false for a
/// Customer node, as WHERE filters and as projected predicates.
#[test]
fn keyword_label_predicates_with_colon_and_is() {
    let mut c = connection();
    run(
        &mut c,
        "CREATE (:Order {name: 'order'}), (:Customer {name: 'customer'})",
    );

    for predicate in ["n:Order", "n IS Order"] {
        assert_eq!(
            rows(
                &mut c,
                &format!("MATCH (n) WHERE {predicate} RETURN n.name ORDER BY n.name")
            ),
            vec![vec![string("order")]],
            "WHERE {predicate}"
        );
    }

    assert_eq!(
        rows(
            &mut c,
            "MATCH (n) RETURN n.name, n:Order, n IS Order ORDER BY n.name"
        ),
        vec![
            vec![string("customer"), Value::Bool(false), Value::Bool(false)],
            vec![string("order"), Value::Bool(true), Value::Bool(true)],
        ]
    );
}

/// Keyword labels and types combine with `|`, `&` and `!` in label
/// expressions.
#[test]
fn keyword_labels_in_label_expressions() {
    let mut c = connection();
    run(
        &mut c,
        "CREATE (:Order {name: 'order'})-[:ORDER]->(:Order:Where {name: 'both'}), \
         (:Customer {name: 'customer'})-[:SET]->(:Product {name: 'product'})",
    );

    for (query, expected) in [
        (
            "MATCH (n:Customer|Order) RETURN n.name ORDER BY n.name",
            vec!["both", "customer", "order"],
        ),
        (
            "MATCH (n:Order&Where) RETURN n.name ORDER BY n.name",
            vec!["both"],
        ),
        (
            "MATCH (n:!Order) RETURN n.name ORDER BY n.name",
            vec!["customer", "product"],
        ),
        (
            "MATCH (n) WHERE n:Order&!Where RETURN n.name ORDER BY n.name",
            vec!["order"],
        ),
        (
            "MATCH ()-[r:ORDER|SET]->() RETURN type(r) ORDER BY type(r)",
            vec!["ORDER", "SET"],
        ),
    ] {
        let expected: Vec<Vec<Value>> = expected.into_iter().map(|s| vec![string(s)]).collect();
        assert_eq!(rows(&mut c, query), expected, "{query}");
    }
}

/// MERGE creates a keyword-labelled pattern once and matches it afterwards.
#[test]
fn merge_keyword_label_and_type_is_idempotent() {
    let mut c = connection();
    let query = "MERGE (c:Customer {customerID: 'ALFKI'})-[r:ORDER]->(o:Order {orderID: 10643}) \
                 RETURN labels(o), type(r)";
    let expected = vec![vec![
        Value::List(vec![string("Order")].into()),
        string("ORDER"),
    ]];

    let first = run(&mut c, query);
    assert_eq!(first.rows, expected);
    assert_eq!(first.stats.nodes_created, 2);
    assert_eq!(first.stats.relationships_created, 1);

    let second = run(&mut c, query);
    assert_eq!(second.rows, expected);
    assert_eq!(second.stats.nodes_created, 0);
    assert_eq!(second.stats.relationships_created, 0);
}

/// Each of the 22 formerly rejected keywords works unquoted as a node label
/// (title case) and a relationship type (upper case) in CREATE, MATCH,
/// label predicates, label expressions, SET, REMOVE and MERGE. The
/// spellings come from the `TokenType` variant names, so each one is
/// checked to lex back to that keyword.
#[test]
fn every_formerly_rejected_keyword_works_as_label_and_type() {
    for ty in CLAUSE_KEYWORDS {
        let rel_type = format!("{ty:?}");
        let label = format!("{}{}", &rel_type[..1], rel_type[1..].to_ascii_lowercase());
        let label = label.as_str();
        for spelling in [rel_type.as_str(), label] {
            assert_eq!(lookup_keyword(spelling), ty, "{spelling}");
        }
        let mut c = connection();

        let created = run(
            &mut c,
            &format!(
                "CREATE (a:{label} {{name: 'a'}})-[r:{rel_type}]->(b:Customer {{name: 'b'}}) \
                 RETURN labels(a), type(r)"
            ),
        );
        assert_eq!(
            created.rows,
            vec![vec![
                Value::List(vec![string(label)].into()),
                string(&rel_type)
            ]],
            "{label}"
        );

        let checks = [
            format!("MATCH (a:`{label}`)-[r:`{rel_type}`]->(b) RETURN count(*)"),
            format!("MATCH (a:{label})-[r:{rel_type}]->(b) RETURN count(*)"),
            format!("MATCH (n) WHERE n:{label} AND n IS {label} RETURN count(n)"),
            format!("MATCH (n:Unknown|{label}) RETURN count(n)"),
            format!("MATCH (n:!{label}&Customer) RETURN count(n)"),
            format!("MATCH ()-[r:UNKNOWN|{rel_type}]->() RETURN count(r)"),
        ];
        for query in &checks {
            assert_eq!(single_int(&mut c, query), 1, "{query}");
        }

        run(&mut c, &format!("MATCH (b:Customer) SET b:{label}"));
        let count_query = format!("MATCH (n:{label}) RETURN count(n)");
        assert_eq!(single_int(&mut c, &count_query), 2, "after SET b:{label}");
        run(&mut c, &format!("MATCH (b:Customer) REMOVE b:{label}"));
        assert_eq!(
            single_int(&mut c, &count_query),
            1,
            "after REMOVE b:{label}"
        );

        let merged = run(
            &mut c,
            &format!("MERGE (m:{label} {{name: 'a'}}) RETURN m.name"),
        );
        assert_eq!(merged.rows, vec![vec![string("a")]], "{label}");
        assert_eq!(merged.stats.nodes_created, 0, "MERGE (m:{label})");
    }
}
