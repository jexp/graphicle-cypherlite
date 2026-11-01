//! ADR-0280 pins: `inf`/`infinity`/`nan` are CIP-55 float-literal keywords
//! that resolve to the literal in every expression position — bound or not —
//! matching Neo4j 2026.09 (probes 1–6 in the ADR). Binding such a name is
//! legal and silent; backtick-escaping (`` `inf` ``) is the one workaround
//! that forces variable resolution. These tests pin the ruled positions
//! end-to-end through the public API.

mod common;

use cypherlite::Value;

use common::{connection, err_text};

/// Neo4j probe 1: a bare reference reads the CIP-55 literal — never an
/// "undefined variable" error.
#[test]
fn bare_reference_reads_the_literal() {
    let mut c = connection();
    let r = c.query("RETURN inf AS x, nan AS y, infinity AS z").unwrap();
    let [row] = &r.rows[..] else {
        panic!("expected one row, got {:?}", r.rows)
    };
    assert_eq!(row[0], Value::Float(f64::INFINITY));
    assert!(matches!(row[1], Value::Float(f) if f.is_nan()));
    assert_eq!(row[2], Value::Float(f64::INFINITY));
}

/// Neo4j probe 2: the literal wins over an in-scope variable, silently.
#[test]
fn literal_wins_over_bound_variable() {
    let mut c = connection();
    let r = c
        .query("WITH 0 AS nan, 1 AS inf, 2 AS infinity RETURN nan, inf, infinity")
        .unwrap();
    let [row] = &r.rows[..] else {
        panic!("expected one row, got {:?}", r.rows)
    };
    assert!(matches!(row[0], Value::Float(f) if f.is_nan()));
    assert_eq!(row[1], Value::Float(f64::INFINITY));
    assert_eq!(row[2], Value::Float(f64::INFINITY));
}

/// Neo4j probe 3: property access takes the literal as its base and fails at
/// runtime with a type error; the backticked variable reads the node.
#[test]
fn property_access_base_is_the_literal() {
    let mut c = connection();
    let err = err_text(c.query("CREATE (inf:N {v: 42}) RETURN inf.v"));
    assert!(
        err.contains("InvalidArgumentType"),
        "expected a runtime InvalidArgumentType, got: {err}"
    );
    let r = c.query("CREATE (`inf`:N {v: 42}) RETURN `inf`.v").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(42)]]);
}

/// Neo4j probe 4: `DELETE inf` is rejected — statically in CypherLite (TCK
/// Delete5 scenario 9 mandates the compile-time class; ADR-0280 ruling 2),
/// at runtime in Neo4j. Same outcome class, earlier failure, and the message
/// names the keyword and the backtick escape.
#[test]
fn delete_keyword_target_is_a_compile_time_type_error() {
    let mut c = connection();
    let err = err_text(c.query("MATCH (n)-[:R]->(inf:N) DETACH DELETE n, inf"));
    assert!(
        err.contains("InvalidArgumentType"),
        "expected InvalidArgumentType, got: {err}"
    );
    assert!(
        err.contains("CIP-55 float-literal keywords") && err.contains("DELETE `inf`"),
        "expected the keyword/backtick diagnostic, got: {err}"
    );
    // The backtick escape deletes the variable.
    c.query("CREATE (`inf`:N)").unwrap();
    let r = c.query("MATCH (`inf`:N) DELETE `inf`").unwrap();
    assert_eq!(r.stats.nodes_deleted, 1);
}

/// Neo4j probe 5: a function argument reads the literal and fails at runtime
/// with a type error.
#[test]
fn function_argument_is_the_literal() {
    let mut c = connection();
    let err = err_text(c.query("CREATE (nan:N) RETURN labels(nan)"));
    assert!(
        err.contains("InvalidArgumentValue") && err.contains("labels()"),
        "expected the labels() runtime type error, got: {err}"
    );
}

/// Neo4j probe 6: property keys after a dot and map keys are unaffected.
#[test]
fn property_and_map_keys_are_unaffected() {
    let mut c = connection();
    c.query("CREATE (n:N)").unwrap();
    c.query("MATCH (n:N) SET n.inf = 1").unwrap();
    let r = c.query("MATCH (n:N) RETURN n.inf").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);

    let r = c.query("RETURN {nan: 1}.nan AS x").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
}

/// Binding a keyword name is legal and silent, and the backtick escape
/// references the variable in expression position.
#[test]
fn binding_is_legal_and_backtick_rescues_expression_position() {
    let mut c = connection();
    let r = c.query("WITH 0 AS `nan` RETURN `nan`").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(0)]]);

    c.query("CREATE (inf:N {v: 7})").unwrap();
    let r = c.query("MATCH (inf:N) RETURN `inf`.v").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(7)]]);
}
