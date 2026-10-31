use self::Selector::{AllShortest, Shortest};
use crate::{Connection, Database, Value};

// Every case runs through both shortest-path forms on one fresh graph: the
// expression form (`RETURN length(shortestPath(P))` /
// `size(allShortestPaths(P))`) and the MATCH-clause form (ADR-0186's BFS
// route, `MATCH p = shortestPath(P) RETURN length(p)` /
// `MATCH p = allShortestPaths(P) RETURN count(p)`). Both must equal
// `expected`; `None` is a null expression result and zero clause rows.

#[derive(Clone, Copy)]
enum Selector {
    Shortest,
    AllShortest,
}

struct Case {
    name: &'static str,
    setup: &'static [&'static str],
    bind: &'static str,
    pattern: &'static str,
    selector: Selector,
    expected: Option<i64>,
}

const BIND_A: &str = "MATCH (a:N {id: 'a'})";
const BIND_AB: &str = "MATCH (a:N {id: 'a'}), (b:N {id: 'b'})";

const DIRECT_AND_DETOUR: &str = "CREATE (a:N {id: 'a'}), (b:N {id: 'b'}), (m:N {id: 'm'}) \
     CREATE (a)-[:E]->(b) CREATE (a)-[:E]->(m) CREATE (m)-[:E]->(b)";
const A_M_B: &str = "CREATE (a:N {id: 'a'}), (b:N {id: 'b'}), (m:N {id: 'm'}) \
     CREATE (a)-[:E]->(m) CREATE (m)-[:E]->(b)";
const A_X_Y_B: &str = "CREATE (a:N {id: 'a'}), (b:N {id: 'b'}), (x:N {id: 'x'}), (y:N {id: 'y'}) \
     CREATE (a)-[:E]->(x)-[:E]->(y)-[:E]->(b)";

const CASES: &[Case] = &[
    Case {
        name: "one hop when a direct edge exists",
        setup: &[DIRECT_AND_DETOUR],
        bind: BIND_AB,
        pattern: "(a)-[:E*1..3]->(b)",
        selector: Shortest,
        expected: Some(1),
    },
    Case {
        name: "two hops when needed",
        setup: &[A_M_B],
        bind: BIND_AB,
        pattern: "(a)-[:E*1..4]->(b)",
        selector: Shortest,
        expected: Some(2),
    },
    Case {
        name: "unreachable",
        setup: &["CREATE (a:N {id: 'a'}), (b:N {id: 'b'})"],
        bind: BIND_AB,
        pattern: "(a)-[:E*1..3]->(b)",
        selector: Shortest,
        expected: None,
    },
    Case {
        name: "zero length when min hops is zero",
        setup: &["CREATE (a:N {id: 'a'})"],
        bind: BIND_A,
        pattern: "(a)-[:E*0..3]->(a)",
        selector: Shortest,
        expected: Some(0),
    },
    Case {
        name: "all shortest paths returns both min trails of a diamond",
        setup: &[
            "CREATE (a:N {id: 'a'}), (b:N {id: 'b'}), (x:N {id: 'x'}), (y:N {id: 'y'}) \
             CREATE (a)-[:E]->(x)-[:E]->(b) CREATE (a)-[:E]->(y)-[:E]->(b)",
        ],
        bind: BIND_AB,
        pattern: "(a)-[:E*1..4]->(b)",
        selector: AllShortest,
        expected: Some(2),
    },
    Case {
        name: "three hops when needed",
        setup: &[A_X_Y_B],
        bind: BIND_AB,
        pattern: "(a)-[:E*1..4]->(b)",
        selector: Shortest,
        expected: Some(3),
    },
    Case {
        name: "three hops at a tight max bound",
        setup: &[A_X_Y_B],
        bind: BIND_AB,
        pattern: "(a)-[:E*1..3]->(b)",
        selector: Shortest,
        expected: Some(3),
    },
    Case {
        name: "undirected uses incoming",
        setup: &["CREATE (a:N {id: 'a'}), (b:N {id: 'b'}), (m:N {id: 'm'}) \
             CREATE (a)-[:E]->(m) CREATE (b)-[:E]->(m)"],
        bind: BIND_AB,
        pattern: "(a)-[:E*1..4]-(b)",
        selector: Shortest,
        expected: Some(2),
    },
    Case {
        name: "min hops skips the direct edge",
        setup: &[DIRECT_AND_DETOUR],
        bind: BIND_AB,
        pattern: "(a)-[:E*2..4]->(b)",
        selector: Shortest,
        expected: Some(2),
    },
    Case {
        name: "directed cycle back to start",
        setup: &["CREATE (a:N {id: 'a'})-[:E]->(b:N {id: 'b'})-[:E]->(a)"],
        bind: BIND_A,
        pattern: "(a)-[:E*1..3]->(a)",
        selector: Shortest,
        expected: Some(2),
    },
    Case {
        name: "bfs does not walk a dense ball",
        setup: &[
            "UNWIND range(0, 39) AS i CREATE (:N {id: i})",
            "MATCH (x:N), (y:N) WHERE x.id < y.id AND y.id - x.id <= 3 \
             CREATE (x)-[:E]->(y)",
        ],
        bind: "MATCH (a:N {id: 0}), (b:N {id: 2})",
        pattern: "(a)-[:E*1..3]->(b)",
        selector: Shortest,
        expected: Some(1),
    },
];

fn fresh_graph(name: &str, setup: &[&str]) -> Connection {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    for stmt in setup {
        c.query(stmt)
            .unwrap_or_else(|e| panic!("{name}: setup `{stmt}` failed: {e:?}"));
    }
    c
}

fn rows(c: &mut Connection, name: &str, form: &str, query: &str) -> Vec<Vec<Value>> {
    c.query(query)
        .unwrap_or_else(|e| panic!("{name} ({form} form): `{query}` failed: {e:?}"))
        .rows
}

#[test]
fn shortest_paths_agree_in_both_forms() {
    for case in CASES {
        let (name, bind, pattern) = (case.name, case.bind, case.pattern);
        let mut c = fresh_graph(name, case.setup);
        let (expr, clause) = match case.selector {
            Shortest => (
                format!("{bind} RETURN length(shortestPath({pattern})) AS v"),
                format!("{bind} MATCH p = shortestPath({pattern}) RETURN length(p) AS v"),
            ),
            AllShortest => (
                format!("{bind} RETURN size(allShortestPaths({pattern})) AS v"),
                format!("{bind} MATCH p = allShortestPaths({pattern}) RETURN count(p) AS v"),
            ),
        };
        let want_expr = vec![vec![case.expected.map_or(Value::Null, Value::Int)]];
        let want_clause: Vec<Vec<Value>> = case
            .expected
            .map(|n| vec![vec![Value::Int(n)]])
            .unwrap_or_default();
        assert_eq!(
            rows(&mut c, name, "expression", &expr),
            want_expr,
            "{name} (expression form): `{expr}`"
        );
        assert_eq!(
            rows(&mut c, name, "clause", &clause),
            want_clause,
            "{name} (clause form): `{clause}`"
        );
    }
}

#[test]
fn shortest_path_returns_path_nodes_in_both_forms() {
    let pattern = "(a)-[:E*1..3]->(b)";
    let forms = [
        (
            "expression",
            format!("{BIND_AB} RETURN shortestPath({pattern}) AS p"),
        ),
        (
            "clause",
            format!("{BIND_AB} MATCH p = shortestPath({pattern}) RETURN p"),
        ),
    ];
    let mut c = fresh_graph("path nodes", &[A_M_B]);
    for (form, query) in forms {
        let r = rows(&mut c, "path nodes", form, &query);
        assert_eq!(r.len(), 1, "{form} form: expected one row, got {r:?}");
        let Value::Path(p) = &r[0][0] else {
            panic!("{form} form: expected path, got {:?}", r[0][0]);
        };
        assert_eq!(p.rels.len(), 2, "{form} form");
        let ids: Vec<_> = p
            .nodes
            .iter()
            .map(|n| n.properties.get("id").cloned())
            .collect();
        assert_eq!(
            ids,
            ["a", "m", "b"].map(|s| Some(Value::String(s.into()))),
            "{form} form"
        );
    }
}
