use std::collections::BTreeMap;

use cypherlite_ir::SINGLE_INPUT_AGGREGATES;

use crate::{Database, Value};

#[test]
fn return_one() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("RETURN 1 AS n").unwrap();
    assert_eq!(r.header.columns, vec!["n"]);
    assert_eq!(r.rows[0][0], Value::Int(1));
}

#[test]
fn null_in_empty_list_is_false() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("RETURN null IN [] AS n").unwrap();
    assert_eq!(r.rows[0][0], Value::Bool(false));
}

#[test]
fn pow_left_assoc_and_float() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("RETURN 4 ^ 3 ^ 2 AS n").unwrap();
    assert_eq!(r.rows[0][0], Value::Float(4096.0));
}

#[test]
fn union_distinct_dedupes_left_side() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c
        .query("UNWIND [2, 1, 2, 3] AS x RETURN x UNION UNWIND [3, 4] AS x RETURN x")
        .unwrap();
    assert_eq!(r.rows.len(), 4);
}

#[test]
fn tointeger_parses_decimal_strings() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c
        .query("RETURN [n IN [2, 2.9, '1.7'] | toInteger(n)] AS n")
        .unwrap();
    assert_eq!(
        r.rows[0][0],
        Value::List(vec![Value::Int(2), Value::Int(2), Value::Int(1)].into())
    );
}

// ADR-0168 (neo4j:2026.09): non-finite floats saturate in toInteger(), the
// non-Java spelling 'inf' is not a float, and an out-of-range numeric
// string is a NumberOutOfRange error that toIntegerOrNull() maps to null.
#[test]
fn non_finite_and_out_of_range_conversions_follow_adr_0168() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c
        .query(
            "RETURN toInteger(0.0/0.0) AS nan, toInteger(1.0/0.0) AS inf, \
             toFloat('inf') AS s, toIntegerOrNull('1e300') AS big",
        )
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Int(0),
            Value::Int(9_223_372_036_854_775_807),
            Value::Null,
            Value::Null,
        ]]
    );
    let err = match c.query("RETURN toInteger('1e300') AS v") {
        Ok(_) => panic!("expected NumberOutOfRange"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("NumberOutOfRange: toInteger()"),
        "expected NumberOutOfRange, got {err}"
    );
}

#[test]
fn size_of_collect() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("UNWIND range(0, 10) AS i CREATE ()").unwrap();
    let r = c.query("MATCH (a) RETURN size(collect(a)) AS n").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(11));
}

#[test]
fn count_gt_zero_on_empty_graph() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (a) RETURN count(a) > 0 AS n").unwrap();
    assert_eq!(r.rows[0][0], Value::Bool(false));
}

// LABS-276 / ADR-0225: a zero-argument single-input aggregate errors
// (Neo4j 42I13) instead of evaluating to the constant 1 per row — at the
// item, in both RETURN and WITH, and over an empty input too.
#[test]
fn zero_argument_aggregates_error_instead_of_returning_one() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    // The canonical SINGLE_INPUT_AGGREGATES table drives the loop, so a
    // name added to the table without the executor backstop fails here
    // first; the CIP-128 alias spellings ride the same canonicalization.
    for agg in SINGLE_INPUT_AGGREGATES {
        let err = c.query(&format!("RETURN {agg}()")).unwrap_err();
        assert!(
            err.to_string().contains("InvalidNumberOfArguments"),
            "{agg}(): {err}"
        );
        let err = c
            .query(&format!("UNWIND [] AS x WITH {agg}() AS s RETURN s"))
            .unwrap_err();
        assert!(
            err.to_string().contains("InvalidNumberOfArguments"),
            "WITH {agg}() over empty input: {err}"
        );
    }
    // More than one argument is the same error class (the executor backstop
    // enforces the same one-argument contract the front validates).
    let err = c.query("UNWIND [1] AS x RETURN sum(x, x)").unwrap_err();
    assert!(
        err.to_string().contains("InvalidNumberOfArguments"),
        "sum(x, x): {err}"
    );
    // The percentile aggregates keep their own two-argument call-site check
    // (ADR-0225): the backstop does not relabel their arity error — over an
    // empty group AND over a non-empty one (the row loop indexes args[0],
    // so a non-empty group must not panic on the missing argument).
    for q in [
        "MATCH (n) RETURN percentileCont()",
        "UNWIND [1, 2] AS x RETURN percentileDisc()",
    ] {
        let err = c.query(q).unwrap_err();
        assert!(
            !err.to_string().contains("InvalidNumberOfArguments")
                && err.to_string().contains("two arguments"),
            "{q}: {err}"
        );
    }
    // The matching shapes stay: one argument, count(*), and the reference
    // zero-input-row semantics (sum=0, avg/min/max/collect of nothing).
    let r = c
        .query(
            "UNWIND [] AS x RETURN sum(x), avg(x), min(x), max(x), collect(x), count(*), count(x)",
        )
        .unwrap();
    assert_eq!(
        r.rows[0],
        vec![
            Value::Int(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::List(vec![].into()),
            Value::Int(0),
            Value::Int(0),
        ]
    );
}

// The executor backstop draws every aggregate's exact arity from
// `cypherlite_ir::aggregate_arity` (ADR-0217): a backtick-escaped
// `count`() bypasses the parser's count-keyword path and front validation,
// and must error before the row loop instead of panicking on args[0] over
// a non-empty group (or silently evaluating over an empty one).
#[test]
fn backtick_escaped_zero_argument_count_errors() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    for q in [
        "RETURN `count`()",
        "UNWIND [1] AS x RETURN `count`()",
        "UNWIND [1] AS x RETURN `count`(x, x)",
    ] {
        let err = c.query(q).unwrap_err();
        assert!(
            err.to_string().contains("InvalidNumberOfArguments"),
            "{q}: {err}"
        );
    }
}

// percentileCont/percentileDisc take exactly two arguments (ADR-0217): an
// over-arity call raises the same two-argument error as an under-arity one
// instead of silently dropping args[2..].
#[test]
fn over_arity_percentile_errors_instead_of_truncating() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    for q in [
        "UNWIND [1] AS x RETURN percentileCont(x, 0.5, 42)",
        "UNWIND [1] AS x RETURN percentileDisc(x, 0.5, 42)",
    ] {
        let err = c.query(q).unwrap_err();
        assert!(err.to_string().contains("two arguments"), "{q}: {err}");
    }
}

#[test]
fn collect_inside_quantifier() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (), ()").unwrap();
    let r = c
        .query("MATCH (n) RETURN ALL(ok IN collect(true) WHERE ok) AS n")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

#[test]
fn merge_null_property_is_error() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let err = match c.query("MERGE ({num: null})") {
        Ok(_) => panic!("expected MergeReadOwnWrites"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("MergeReadOwnWrites"),
        "expected MergeReadOwnWrites, got {err}"
    );
}

#[test]
fn set_label_on_null_is_ignored() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("OPTIONAL MATCH (a:Missing) SET a:L RETURN a")
        .unwrap();
}

#[test]
fn named_path_includes_anonymous_nodes() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (:X)-[:R]->()-[:R]->()-[:R]->()").unwrap();
    let r = c
        .query("MATCH p = (:X)-->()-->()-->() RETURN size(nodes(p)) AS n")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Int(4));
}

#[test]
fn create_then_delete_same_query_has_no_side_effects() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE ()").unwrap();
    let r = c.query("MATCH () CREATE (n) DELETE n").unwrap();
    assert_eq!(r.stats.nodes_created, 0);
    assert_eq!(r.stats.nodes_deleted, 0);
}

#[test]
fn set_rel_properties_from_node() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (:A {name: 'A'})-[:T]->(:B)").unwrap();
    c.query("MATCH (a:A)-[r:T]->() SET r = a").unwrap();
    let r = c.query("MATCH ()-[r:T]->() RETURN r.name AS n").unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("A"));
}

#[test]
fn label_predicate_in_return() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (), (:Foo)").unwrap();
    let r = c.query("MATCH (n) RETURN (n:Foo) AS n").unwrap();
    let mut vals: Vec<_> = r.rows.iter().map(|row| row[0].clone()).collect();
    vals.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(vals, vec![Value::Bool(false), Value::Bool(true)]);
}

#[test]
fn dynamic_labels_types_and_boolean_predicates() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let created = c
        .query("WITH ['Person', 'Admin'] AS ls, 'KNOWS' AS t CREATE (a:$(ls))-[r:$(t)]->(b:Other) RETURN a, r")
        .unwrap();
    assert_eq!(created.stats.nodes_created, 2);
    assert_eq!(created.stats.relationships_created, 1);
    let r = c
        .query("WITH 'Person' AS l, 'KNOWS' AS t MATCH (a:$all([l, 'Admin']))-[r:$any(['NOPE', 'KNOWS'])]->(b) RETURN a IS (Person & !Other), r:$(t)")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Bool(true), Value::Bool(true)]]);
}

#[test]
fn dynamic_property_access_update_and_removal_are_visible() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c
        .query("CREATE (n) WITH n, 'odd key' AS k SET n[k] = 7 WITH n, k REMOVE n[k] RETURN n[k]")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Null]]);
    assert_eq!(r.stats.properties_set, 1);
    assert_eq!(r.stats.properties_removed, 1);
}

#[test]
fn dynamic_set_remove_labels_count_actual_changes() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c
        .query("CREATE (n:A) WITH n, ['A', 'B'] AS add, 'B' AS rem SET n:$(add) REMOVE n:$(rem) RETURN labels(n)")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::List(vec![Value::String("A".into())].into())]]
    );
    assert_eq!(r.stats.labels_removed, 0);
}

#[test]
fn labels_function_overwrite_extend_and_remove() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c
        .query("CREATE (n:A) SET labels(n) = ['B'], labels(n) += ['C'], labels(n) -= ['B'] RETURN labels(n)")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::List(vec![Value::String("C".into())].into())]]
    );
}

#[test]
fn remove_dynamic_items_resolve_before_mutation() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c
        .query("CREATE (n:A:B) WITH n REMOVE n:$(head(labels(n))), n:$(head(labels(n))) RETURN labels(n)")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::List(vec![Value::String("B".into())].into())]]
    );
}

#[test]
fn create_match_where() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:Person {name: 'Ada', age: 36})").unwrap();
    let r = c
        .query("MATCH (n:Person) WHERE n.name = 'Ada' RETURN n.name AS name, n.age AS age")
        .unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("Ada"));
    assert_eq!(r.rows[0][1], Value::Int(36));
}

#[test]
fn match_from_unselective_end_uses_other_node() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    for i in 0..40 {
        c.query(&format!("CREATE (:Noise {{n: {i}}})")).unwrap();
    }
    c.query("CREATE (:Account {id: 'src'})-[:TRANSFER {id: 't1'}]->(:Account {id: 'dst'})")
        .unwrap();
    let r = c
        .query("MATCH (b)<-[:TRANSFER]-(a:Account {id: 'src'}) RETURN b.id AS id")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("dst".into())]]);
    let r = c
        .query("MATCH (n) WHERE n.id = 'src' AND n:Account RETURN n.id AS id")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("src".into())]]);
}

#[test]
fn knows_path() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})")
        .unwrap();
    let r = c
        .query("MATCH (p:Person)-[:KNOWS]->(f) WHERE p.name = 'Alice' RETURN f.name AS n")
        .unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("Bob"));
}
#[test]
fn unwind_and_with() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c
        .query("UNWIND [1,2,3] AS x WITH x WHERE x > 1 RETURN x ORDER BY x")
        .unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0], Value::Int(2));
}

#[test]
fn named_var_length_cycle() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (a:Account {id: 'cycle-0'})-[:TRANSFER]->(b:Account {id: 'cycle-1'})-[:TRANSFER]->(c:Account {id: 'cycle-2'})-[:TRANSFER]->(a)")
        .unwrap();
    let r = c
        .query("MATCH p = (a:Account {id: 'cycle-0'})-[:TRANSFER*2..6]->(a) RETURN p")
        .unwrap();
    assert!(!r.rows.is_empty());
}

#[test]
fn match9_var_length_rel_list() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (a:A)-[:REL {num: 1}]->(b:B)-[:REL {num: 2}]->(e:End)")
        .unwrap();
    let r = c.query("MATCH (a)-[r:REL*2..2]->(b:End) RETURN r").unwrap();
    assert_eq!(r.rows.len(), 1, "rows={:?}", r.rows);
    match &r.rows[0][0] {
        Value::List(xs) => {
            assert_eq!(xs.len(), 2, "rels={xs:?}");
            let nums: Vec<_> = xs
                .iter()
                .map(|v| match v {
                    Value::Relationship(rel) => rel.properties.get("num").cloned(),
                    other => panic!("not a rel: {other:?}"),
                })
                .collect();
            assert_eq!(
                nums,
                vec![Some(Value::Int(1)), Some(Value::Int(2))],
                "r={xs:?}"
            );
        }
        other => panic!("expected list, got {other:?}"),
    }
}

#[test]
fn eager_rel_inequality_matches_late_filter() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query(
        "CREATE (a:Account {id: 1})-[:T {amount: 1}]->(:B)-[:T]->(:C), \
                (a)-[:T {amount: 100}]->(:B)-[:T]->(:C), \
                (a)-[:T {amount: 1}]->(:B)-[:T]->(:C)",
    )
    .unwrap();
    let early = c
        .query(
            "MATCH (a:Account {id: 1})-[t:T]->(b)-[:T]->(c) \
             WHERE t.amount > 10 RETURN count(c) AS n",
        )
        .unwrap();
    let late = c
        .query(
            "MATCH (a:Account {id: 1})-[t:T]->(b)-[:T]->(c) \
             WITH t, c WHERE t.amount > 10 RETURN count(c) AS n",
        )
        .unwrap();
    assert_eq!(early.rows[0][0], Value::Int(1));
    assert_eq!(late.rows[0][0], Value::Int(1));
}

#[test]
fn live_vars_keep_set_target() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (:A {id: 1})-[:R]->(:B {id: 2})").unwrap();
    c.query("MATCH (a:A)--(b:B) SET a.seen = true RETURN b.id AS id")
        .unwrap();
    let r = c.query("MATCH (a:A) RETURN a.seen AS s").unwrap();
    assert_eq!(r.rows[0][0], Value::Bool(true));
}

#[test]
fn exists_role_name_predicate() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query(
        "CREATE (u:User {id: 1})-[:HAS_ROLE]->(:Role {name: 'requester'}), \
                (:Role {name: 'other'})",
    )
    .unwrap();
    let r = c
        .query(
            "MATCH (u:User {id: 1}) \
             WHERE EXISTS { (u)-[:HAS_ROLE]->(:Role {name: 'requester'}) } \
             RETURN u.id AS id",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Int(1));
}
#[test]
fn cip_expression_types_reductions_and_values() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();

    let r = c
        .query(
            "RETURN 1 IS TYPED INTEGER NOT NULL AS typed, \
             valueType([1, null]) AS value_type, \
             normalize('\u{212B}', NFC) AS normalized, \
             reduce(s = 0, x IN [1, 2, 3] | s + x) AS reduced, \
             allReduce(s = 0, x IN [1, 2, 3] | s + x, s < 7) AS all_reduced",
        )
        .unwrap();
    assert_eq!(
        r.rows[0],
        vec![
            Value::Bool(true),
            Value::String("LIST<INTEGER> NOT NULL".into()),
            Value::String("\u{00C5}".into()),
            Value::Int(6),
            Value::Bool(true),
        ]
    );

    let r = c
        .query(
            "RETURN NULLIF(2, 2) AS nil, CARDINALITY({a: 1, b: 2}) AS cardinality, \
             point.distance(point({x: 0, y: 0}), point({x: 3, y: 4})) AS distance, \
             vector([1, 2, 3], 3, INTEGER) AS vector",
        )
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Null);
    assert_eq!(r.rows[0][1], Value::Int(2));
    assert_eq!(r.rows[0][2], Value::Float(5.0));
    assert!(matches!(
        &r.rows[0][3],
        Value::Vector { values, .. } if values.len() == 3
    ));

    let r = c
        .query(
            "RETURN coll.flatten([[1], [2, [3]]]) AS flat, \
             cosh(0) AS hyperbolic, \
             format(datetime('1984-10-11T12:31:14.645+01:00'), \
                    \"yyyy-MM-dd'T'HH:mm:ss.SSSZ\") AS formatted",
        )
        .unwrap();
    assert_eq!(
        r.rows[0][0],
        Value::List(
            vec![
                Value::Int(1),
                Value::Int(2),
                Value::List(vec![Value::Int(3)].into()),
            ]
            .into()
        )
    );
    assert_eq!(r.rows[0][1], Value::Float(1.0));
    assert_eq!(
        r.rows[0][2],
        Value::String("1984-10-11T12:31:14.645+0100".into())
    );

    let r = c
        .query(
            "RETURN trim(LEADING 'x' FROM 'xxokx') AS trimmed, \
             vector_distance(vector([3, 4], 2, INTEGER), \
                             vector([0, 0], 2, INTEGER), EUCLIDEAN) AS vector_distance",
        )
        .unwrap();
    assert_eq!(r.rows[0][0], Value::String("okx".into()));
    assert_eq!(r.rows[0][1], Value::Float(5.0));

    c.query("CREATE (:Special {v: Infinity, n: NaN})").unwrap();
    let stored = c
        .query("MATCH (n:Special) RETURN n.v AS v, n.n AS n")
        .unwrap();
    assert!(
        matches!(
            &stored.rows[..],
            [row] if matches!(row[0], Value::Float(f) if f.is_infinite())
                && matches!(row[1], Value::Float(f) if f.is_nan())
        ),
        "{:?}",
        stored.rows
    );
    let r = c
        .query(
            "MATCH (n:Special) WHERE n.v = Infinity AND isNaN(n.n) \
             RETURN n.v AS infinity",
        )
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Float(f64::INFINITY)]]);
}
#[test]
fn deleted_node_binding_remains_as_empty_tombstone() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (:X), (:Y)").unwrap();
    let result = c
        .query("MATCH (u:X) DELETE u UNWIND [1] AS x RETURN u, x")
        .unwrap();
    let Value::Node(node) = &result.rows[0][0] else {
        panic!("deleted binding should remain a node value");
    };
    assert!(node.labels.is_empty());
    assert!(node.properties.is_empty());
}

fn symbol_file(path: &str, names: &[&str]) -> Value {
    let mut file = BTreeMap::new();
    file.insert("path".to_string(), Value::String(path.into()));
    file.insert("language".to_string(), Value::String("rust".into()));
    let symbols: Vec<_> = names
        .iter()
        .map(|name| {
            let mut symbol = BTreeMap::new();
            symbol.insert("name".to_string(), Value::String((*name).into()));
            Value::Map(symbol.into())
        })
        .collect();
    file.insert("symbols".to_string(), Value::List(symbols.into()));
    Value::Map(file.into())
}

#[test]
fn nested_unwind_keeps_sibling_fields_without_the_list() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let mut params = BTreeMap::new();
    params.insert(
        "files".to_string(),
        Value::List(
            vec![
                symbol_file("a.rs", &["foo"]),
                symbol_file("b.rs", &["bar", "baz"]),
            ]
            .into(),
        ),
    );
    let r = c
        .query_with_params(
            "UNWIND $files AS file \
             UNWIND file.symbols AS row \
             CREATE (s:Symbol {name: row.name, file_path: file.path, language: file.language}) \
             RETURN s.file_path AS path, s.language AS language, s.name AS name",
            params,
        )
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![
                Value::String("a.rs".into()),
                Value::String("rust".into()),
                Value::String("foo".into()),
            ],
            vec![
                Value::String("b.rs".into()),
                Value::String("rust".into()),
                Value::String("bar".into()),
            ],
            vec![
                Value::String("b.rs".into()),
                Value::String("rust".into()),
                Value::String("baz".into()),
            ],
        ]
    );
}

#[test]
fn nested_unwind_keeps_the_list_when_a_later_clause_reads_it() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let mut params = BTreeMap::new();
    params.insert(
        "files".to_string(),
        Value::List(vec![symbol_file("a.rs", &["foo", "bar", "baz"])].into()),
    );
    let r = c
        .query_with_params(
            "UNWIND $files AS file \
             UNWIND file.symbols AS row \
             RETURN size(file.symbols) AS n, row.name AS name",
            params,
        )
        .unwrap();
    assert_eq!(r.rows.len(), 3);
    assert!(r.rows.iter().all(|row| row[0] == Value::Int(3)));
}

#[test]
fn nested_unwind_keeps_the_map_a_projection_selector_reads() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let mut params = BTreeMap::new();
    params.insert(
        "files".to_string(),
        Value::List(vec![symbol_file("a.rs", &["foo", "bar"])].into()),
    );
    let r = c
        .query_with_params(
            "UNWIND $files AS file \
             UNWIND file.symbols AS row \
             UNWIND [{tag: 1}] AS mm \
             RETURN size(mm { file }.file.symbols) AS n, row.name AS name",
            params,
        )
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(2), Value::String("foo".into())],
            vec![Value::Int(2), Value::String("bar".into())],
        ]
    );
}

#[test]
fn unwind_keeps_a_column_a_projection_selector_reads() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    let r = c
        .query(
            "WITH [1, 2, 3] AS xs \
             UNWIND xs AS x \
             UNWIND [{tag: 1}] AS mm \
             RETURN size(mm { xs }.xs) AS n, x AS x",
        )
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(3), Value::Int(1)],
            vec![Value::Int(3), Value::Int(2)],
            vec![Value::Int(3), Value::Int(3)],
        ]
    );
}
