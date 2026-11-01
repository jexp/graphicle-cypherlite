//! The constraint-matrix fixture vocabulary shared by the split test
//! modules: the error builders, the graph dump/setup helpers, the
//! `violations!` row macro, and the constraint definitions its rows name.

use cypherlite::{Connection, Value};

use crate::common::{connection, rows, run};

pub(crate) fn node_label(label: &str, enforced: &str) -> String {
    format!("ConstraintValidationFailed: node with label {label} must also have label {enforced}")
}

pub(crate) fn endpoint(enforced: &str) -> String {
    format!(
        "ConstraintValidationFailed: endpoint label presence verification failed: expected {enforced}"
    )
}

/// Every node, relationship and constraint, in a stable order.
#[derive(Debug, PartialEq)]
pub(crate) struct Dump {
    pub(crate) nodes: Vec<Vec<Value>>,
    pub(crate) rels: Vec<Vec<Value>>,
    pub(crate) constraints: Vec<Vec<Value>>,
}

pub(crate) fn dump(c: &mut Connection) -> Dump {
    Dump {
        nodes: rows(
            c,
            "MATCH (n) RETURN id(n) AS id, labels(n) AS labels, properties(n) AS props ORDER BY id",
        ),
        rels: rows(
            c,
            "MATCH (a)-[r]->(b) \
             RETURN id(r) AS id, type(r) AS type, id(a) AS a, id(b) AS b, properties(r) AS props \
             ORDER BY id",
        ),
        constraints: rows(c, "SHOW CONSTRAINTS YIELD name RETURN name ORDER BY name"),
    }
}

pub(crate) fn setup(statements: &[&str]) -> Connection {
    let mut c = connection();
    for statement in statements {
        run(&mut c, statement);
    }
    c
}

pub(crate) fn assert_violation(statements: &[&str], violation: &str, expected: &str) {
    let mut c = setup(statements);
    let before = dump(&mut c);
    let error = c
        .query(violation)
        .err()
        .unwrap_or_else(|| panic!("`{violation}` should fail with {expected}"))
        .to_string();
    assert_eq!(error, expected, "`{violation}`");
    assert_eq!(
        dump(&mut c),
        before,
        "`{violation}` must leave the graph unchanged"
    );
}

/// One `#[test]` per constraint kind and trigger operation: the setup
/// statements, the violating statement, and today's exact error.
macro_rules! violations {
    ($($name:ident: [$($setup:expr),* $(,)?] => $violation:expr, $expected:expr;)*) => {
        $(
            #[test]
            fn $name() {
                $crate::support::assert_violation(&[$($setup),*], $violation, &$expected);
            }
        )*
    };
}

pub(crate) const UNIQUE: &str = "CREATE CONSTRAINT p_id FOR (n:P) REQUIRE n.id IS UNIQUE";
pub(crate) const PERSON_EMAIL: &str =
    "CREATE CONSTRAINT person_email FOR (n:Person) REQUIRE n.email IS UNIQUE";
pub(crate) const REL_UNIQUE: &str = "CREATE CONSTRAINT r_id FOR ()-[r:R]-() REQUIRE r.id IS UNIQUE";
pub(crate) const KEY: &str = "CREATE CONSTRAINT p_key FOR (n:P) REQUIRE n.k IS NODE KEY";
pub(crate) const REL_KEY: &str =
    "CREATE CONSTRAINT r_key FOR ()-[r:R]-() REQUIRE r.k IS RELATIONSHIP KEY";
pub(crate) const EXISTS: &str = "CREATE CONSTRAINT p_name FOR (n:P) REQUIRE n.name IS NOT NULL";
pub(crate) const PERSON_NAME: &str =
    "CREATE CONSTRAINT person_name FOR (n:Person) REQUIRE n.name IS NOT NULL";
pub(crate) const REL_EXISTS: &str = "CREATE CONSTRAINT r_w FOR ()-[r:R]-() REQUIRE r.w IS NOT NULL";
pub(crate) const TYPED: &str = "CREATE CONSTRAINT p_age FOR (n:P) REQUIRE n.age IS :: INTEGER";
pub(crate) const REL_TYPED: &str =
    "CREATE CONSTRAINT r_t FOR ()-[r:R]-() REQUIRE r.t IS :: INTEGER";
/// `SET e.p = null` and `REMOVE e.p` break a property type only when it is
/// `NOT NULL`; the graph type is where such a type is declared.
pub(crate) const TYPED_NOT_NULL: &str =
    "ALTER CURRENT GRAPH TYPE SET { (:P => {age :: INTEGER NOT NULL}) }";
pub(crate) const REL_TYPED_NOT_NULL: &str =
    "ALTER CURRENT GRAPH TYPE SET { ()-[:R => {t :: INTEGER NOT NULL}]->() }";
pub(crate) const LABEL_EXISTENCE: &str = "ALTER CURRENT GRAPH TYPE SET { (:P => :Q) }";
pub(crate) const NODE_LABEL_EXISTENCE: &str = "CREATE CONSTRAINT p_q FOR (n:P) REQUIRE n:Q";
pub(crate) const SOURCE_LABEL: &str =
    "ALTER CURRENT GRAPH TYPE SET { (:A => {name :: STRING}), (:A)-[:R =>]->() }";
pub(crate) const SOURCE_LABEL_EXISTENCE: &str =
    "CREATE CONSTRAINT r_src FOR (s)-[r:R]->(t) REQUIRE s:A";
pub(crate) const TARGET_LABEL: &str =
    "ALTER CURRENT GRAPH TYPE SET { (:B => {name :: STRING}), ()-[:R =>]->(:B) }";
pub(crate) const TARGET_LABEL_EXISTENCE: &str =
    "CREATE CONSTRAINT r_tgt FOR (s)-[r:R]->(t) REQUIRE t:B";
