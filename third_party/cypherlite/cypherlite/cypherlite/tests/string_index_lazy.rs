//! Lazy string postings (ADR-0063): `CONTAINS` / `STARTS WITH` seeks build
//! their prefix/trigram postings on first use and catch them up
//! incrementally on later writes, instead of maintaining them on every
//! write. These tests pin the public-API contract of that change:
//!
//! - the fraud `search_customer` shape (`Customer.name CONTAINS $q`) keeps
//!   returning exactly the matching rows across interleaved writes (the
//!   graph-level probe tests in `cypherlite-executor` pin that the seek is
//!   served from postings, not a label scan);
//! - a 100k single-statement bulk create lands identical graph contents;
//! - a file-backed reopen keeps the seek correct (postings rebuild lazily
//!   from the replayed graph).

use std::collections::BTreeMap;

use cypherlite::{Database, Value};

fn search(c: &mut cypherlite::Connection, q: &str) -> Vec<Vec<Value>> {
    c.query_with_params(
        "MATCH (c:Customer) WHERE c.name CONTAINS $q \
         RETURN c.id AS id, c.name AS name ORDER BY c.name LIMIT 10",
        BTreeMap::from([("q".into(), Value::String(q.into()))]),
    )
    .unwrap()
    .rows
}

#[test]
fn search_customer_contains_survives_interleaved_writes() {
    let mut c = Database::open(":memory:").unwrap().connect().unwrap();
    // Seed 500 customers; only cust-N names carry the searchable needle.
    c.query(
        "UNWIND range(1, 500) AS i \
         CREATE (:Customer {id: 'cust-' + i, name: 'Customer ' + i})",
    )
    .unwrap();

    // A needle that selects exactly one seeded name ("Customer 499" is not a
    // substring of any other seeded name).
    let rows = search(&mut c, "Customer 499");
    assert_eq!(
        rows,
        vec![vec![
            Value::String("cust-499".into()),
            Value::String("Customer 499".into())
        ]]
    );

    // A broad needle keeps multi-match semantics plus top-K ordering.
    let rows = search(&mut c, "omer 49");
    assert_eq!(rows.len(), 10, "LIMIT 10 over the postings seek");
    assert_eq!(rows[0][1], Value::String("Customer 49".into()));

    // Fraud-mix interleave: an onboard (new matching customer), an
    // unrelated write, and a rename away from / into the needle.
    c.query("CREATE (:Customer {id: 'cust-4990', name: 'Customer 4990'})")
        .unwrap();
    c.query("CREATE (:Account {id: 'acct-1', frozen: false})")
        .unwrap();
    c.query("MATCH (c:Customer {id: 'cust-499'}) SET c.name = 'Renamed'")
        .unwrap();
    c.query("MATCH (c:Customer {id: 'cust-5'}) SET c.name = 'Customer 4995'")
        .unwrap();

    let rows = search(&mut c, "Customer 499");
    let names: Vec<&Value> = rows.iter().map(|r| &r[1]).collect();
    assert_eq!(
        names,
        vec![
            &Value::String("Customer 4990".into()),
            &Value::String("Customer 4995".into()),
        ],
        "dirty catch-up must add new matches and drop renamed-away ones"
    );

    // STARTS WITH over the same lazily built pair.
    let r = c
        .query(
            "MATCH (c:Customer) WHERE c.name STARTS WITH 'Customer 499' \
             RETURN c.name AS name ORDER BY c.name",
        )
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::String("Customer 4990".into())],
            vec![Value::String("Customer 4995".into())],
        ]
    );
}

#[test]
fn contains_seek_correct_after_file_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        c.query(
            "UNWIND range(1, 200) AS i \
             CREATE (:Customer {id: 'cust-' + i, name: 'Customer ' + i})",
        )
        .unwrap();
    }
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    // Postings were never materialized in this process: the seek builds
    // them lazily from the replayed graph.
    let rows = search(&mut c, "omer 42");
    assert_eq!(
        rows,
        vec![vec![
            Value::String("cust-42".into()),
            Value::String("Customer 42".into())
        ]]
    );
}

#[test]
fn bulk_create_100k_lands_identical_contents() {
    // The node-51 profiling shape at 100k: pins that removing the
    // bulk-CREATE string-index bottleneck changed no graph contents.
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query(
        "UNWIND range(1,100000) AS id \
         CREATE (p:Person {name:\"Tomas \"+id, id:id, age:id%100})-[:KNOWS]->(p)",
    )
    .unwrap();
    let r = c.query("MATCH (p:Person) RETURN count(p) AS c").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(100_000));
    let r = c
        .query("MATCH ()-[k:KNOWS]->() RETURN count(k) AS c")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Int(100_000));
    let r = c
        .query("MATCH (p:Person {id: 50000}) RETURN p.name AS name, p.age AS age")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::String("Tomas 50000".into()), Value::Int(0)]]
    );
    let r = c
        .query("MATCH (p:Person {id: 99999}) RETURN p.name AS name, p.age AS age")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::String("Tomas 99999".into()), Value::Int(99)]]
    );
}

/// An empty CONTAINS needle is servable by no gram index: the seek declines
/// (None) and the query falls through to the scan + residual, which answers
/// `CONTAINS ''` correctly (true for every string).
#[test]
fn contains_empty_needle_falls_back_to_scan() {
    let mut c = Database::open(":memory:").unwrap().connect().unwrap();
    c.query("UNWIND range(1, 5) AS i CREATE (:Customer {name: 'Customer ' + i})")
        .unwrap();
    let rows = search(&mut c, "");
    assert_eq!(rows.len(), 5, "CONTAINS '' matches every string: {rows:?}");
}

#[test]
fn text_range_and_point_indexes_answer_match() {
    let mut c = Database::open(":memory:").unwrap().connect().unwrap();
    c.query(
        "CREATE (:Book {title: 'War and Peace'}), (:Book {title: 'Emma'}), \
         (:N {x: 1}), (:N {x: 5}), \
         (:P {loc: point({x: 1.0, y: 1.0})}), (:P {loc: point({x: 9.0, y: 9.0})})",
    )
    .unwrap();
    c.query("CREATE TEXT INDEX titleIndex FOR (n:Book) ON (n.title)")
        .unwrap();
    c.query("CREATE RANGE INDEX xIndex FOR (n:N) ON (n.x)")
        .unwrap();
    c.query("CREATE POINT INDEX locIndex FOR (n:P) ON (n.loc)")
        .unwrap();

    let rows = c
        .query("MATCH (n:Book) WHERE n.title CONTAINS 'and' RETURN n.title AS title")
        .unwrap();
    assert_eq!(rows.rows, vec![vec![Value::String("War and Peace".into())]]);

    let rows = c
        .query("MATCH (n:N) WHERE n.x > 1 RETURN n.x AS x")
        .unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Int(5)]]);

    let rows = c
        .query(
            "MATCH (n:P) WHERE point.withinBBox(n.loc, point({x: 0, y: 0}), point({x: 2, y: 2})) \
             RETURN n.loc.x AS x",
        )
        .unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Float(1.0)]]);

    c.query("DROP INDEX titleIndex").unwrap();
    let rows = c
        .query("MATCH (n:Book) WHERE n.title CONTAINS 'and' RETURN n.title AS title")
        .unwrap();
    assert_eq!(rows.rows, vec![vec![Value::String("War and Peace".into())]]);
}

fn file_paths(c: &mut cypherlite::Connection, query: &str) -> Vec<Value> {
    c.query(query)
        .unwrap()
        .rows
        .into_iter()
        .map(|mut row| row.pop().unwrap())
        .collect()
}

#[test]
fn text_index_or_union_returns_each_arm_once() {
    let mut c = Database::open(":memory:").unwrap().connect().unwrap();
    c.query(
        "CREATE (:File {content: 'has WalRecord', name: 'a', path: 'a.txt'}), \
                (:File {content: 'plain', name: 'WalRecord notes', path: 'b.txt'}), \
                (:File {content: 'plain', name: 'b', path: 'dir/WalRecord'}), \
                (:File {content: 'WalRecord', name: 'WalRecord', path: 'both.txt'}), \
                (:File {content: 'plain', name: 'c', path: 'c.txt'})",
    )
    .unwrap();
    c.query("CREATE TEXT INDEX fileContent FOR (f:File) ON (f.content)")
        .unwrap();
    c.query("CREATE TEXT INDEX fileName FOR (f:File) ON (f.name)")
        .unwrap();
    c.query("CREATE TEXT INDEX filePath FOR (f:File) ON (f.path)")
        .unwrap();

    let or_query = "MATCH (f:File) \
         WHERE f.content CONTAINS 'WalRecord' \
            OR f.name CONTAINS 'WalRecord' \
            OR f.path CONTAINS 'WalRecord' \
         RETURN f.path AS path ORDER BY path";
    assert_eq!(
        file_paths(&mut c, or_query),
        vec![
            Value::String("a.txt".into()),
            Value::String("b.txt".into()),
            Value::String("both.txt".into()),
            Value::String("dir/WalRecord".into()),
        ]
    );

    c.query("DROP INDEX filePath").unwrap();
    assert_eq!(
        file_paths(&mut c, or_query),
        vec![
            Value::String("a.txt".into()),
            Value::String("b.txt".into()),
            Value::String("both.txt".into()),
            Value::String("dir/WalRecord".into()),
        ]
    );
}

#[test]
fn text_index_or_union_keeps_an_ends_with_filter() {
    let mut c = Database::open(":memory:").unwrap().connect().unwrap();
    c.query(
        "CREATE (:File {content: 'WalRecord', name: 'a.rs', path: 'a.rs'}), \
                (:File {content: 'WalRecord', name: 'b.txt', path: 'b.txt'}), \
                (:File {content: 'nope', name: 'c.rs', path: 'c.rs'})",
    )
    .unwrap();
    c.query("CREATE TEXT INDEX fileContent FOR (f:File) ON (f.content)")
        .unwrap();
    c.query("CREATE TEXT INDEX fileName FOR (f:File) ON (f.name)")
        .unwrap();
    c.query("CREATE TEXT INDEX filePath FOR (f:File) ON (f.path)")
        .unwrap();
    assert_eq!(
        file_paths(
            &mut c,
            "MATCH (f:File) \
             WHERE f.name ENDS WITH '.rs' AND ( \
                f.content CONTAINS 'WalRecord' \
                OR f.name CONTAINS 'WalRecord' \
                OR f.path CONTAINS 'WalRecord') \
             RETURN f.path AS path ORDER BY path"
        ),
        vec![Value::String("a.rs".into())]
    );
}

#[test]
fn empty_contains_arm_does_not_drop_or_hits() {
    let mut c = Database::open(":memory:").unwrap().connect().unwrap();
    c.query(
        "CREATE (:File {content: 'alpha', name: 'nope', path: 'p'}), \
                (:File {content: 'beta', name: 'nope', path: 'q'})",
    )
    .unwrap();
    c.query("CREATE TEXT INDEX fileContent FOR (f:File) ON (f.content)")
        .unwrap();
    c.query("CREATE TEXT INDEX fileName FOR (f:File) ON (f.name)")
        .unwrap();
    assert_eq!(
        file_paths(
            &mut c,
            "MATCH (f:File) \
             WHERE f.content CONTAINS '' OR f.name CONTAINS 'nomatch' \
             RETURN f.path AS path ORDER BY path"
        ),
        vec![Value::String("p".into()), Value::String("q".into())]
    );
}

#[test]
fn ends_with_inside_or_uses_the_text_index() {
    let mut c = Database::open(":memory:").unwrap().connect().unwrap();
    c.query(
        "CREATE (:File {content: 'has WalRecord', name: 'lib.rs', path: 'src/lib.rs'}), \
                (:File {content: 'plain', name: 'notes.txt', path: 'dir/WalRecord'}), \
                (:File {content: 'has WalRecord', name: 'notes.txt', path: 'notes.txt'}), \
                (:File {content: 'has WalRecord', name: 'lib.rs.bak', path: 'bak'}), \
                (:File {content: 'plain', name: 'other.rs', path: 'other.rs'}), \
                (:File {content: 'plain', name: 'c', path: 'c.txt'})",
    )
    .unwrap();
    c.query("CREATE TEXT INDEX fileContent FOR (f:File) ON (f.content)")
        .unwrap();
    c.query("CREATE TEXT INDEX fileName FOR (f:File) ON (f.name)")
        .unwrap();
    c.query("CREATE TEXT INDEX filePath FOR (f:File) ON (f.path)")
        .unwrap();
    assert_eq!(
        file_paths(
            &mut c,
            "MATCH (f:File) \
             WHERE (f.content CONTAINS 'WalRecord' AND f.name ENDS WITH '.rs') \
                OR f.path CONTAINS 'WalRecord' \
             RETURN f.path AS path ORDER BY path"
        ),
        vec![
            Value::String("dir/WalRecord".into()),
            Value::String("src/lib.rs".into()),
        ]
    );
}

#[test]
fn or_union_mixes_text_and_range_indexes() {
    let mut c = Database::open(":memory:").unwrap().connect().unwrap();
    c.query(
        "CREATE (:Doc {content: 'WalRecord', n: 0}), \
                (:Doc {content: 'other', n: 1}), \
                (:Doc {content: 'other', n: 2})",
    )
    .unwrap();
    c.query("CREATE TEXT INDEX docContent FOR (d:Doc) ON (d.content)")
        .unwrap();
    c.query("CREATE RANGE INDEX docN FOR (d:Doc) ON (d.n)")
        .unwrap();
    let rows = c
        .query(
            "MATCH (d:Doc) \
             WHERE d.content CONTAINS 'WalRecord' OR d.n = 1 \
             RETURN d.n AS n ORDER BY n",
        )
        .unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Int(0)], vec![Value::Int(1)]]);
}
