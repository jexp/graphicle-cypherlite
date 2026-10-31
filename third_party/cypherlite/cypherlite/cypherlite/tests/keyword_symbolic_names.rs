//! LABS-183 / ADR-0191: cross-position coverage for keywords as symbolic
//! names. Each test binds a keyword spelling in one grammar position and
//! reads it back from another: a pattern variable read in RETURN, a FOREACH
//! loop variable read in its body, a list-comprehension variable read in its
//! body, and a map-literal key read back through a property access. The
//! recommendations walkthrough statements are pinned by
//! `keyword_aliases.rs`; this file is the cross-position companion.

mod common;

use cypherlite::Value;

use common::{connection, rows, run, string};

/// `as` is bound as a node pattern variable and read as a RETURN item's
/// property base: binding position to reference position.
#[test]
fn keyword_pattern_variable_is_read_in_return() {
    let mut c = connection();
    run(&mut c, "CREATE (:Movie {title: 'Inception'})");

    assert_eq!(
        rows(&mut c, "MATCH (as:Movie) RETURN as.title"),
        vec![vec![string("Inception")]]
    );
}

/// `where` is bound as a FOREACH loop variable and read inside the loop
/// body: binding position to reference position, through a map key.
#[test]
fn keyword_foreach_variable_is_read_in_its_body() {
    let mut c = connection();
    run(&mut c, "FOREACH (where IN [1, 2] | CREATE (n {v: where}))");

    assert_eq!(
        rows(&mut c, "MATCH (n) RETURN n.v ORDER BY n.v"),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );
}

/// `as` is bound as a list-comprehension variable and read in the
/// comprehension's WHERE predicate and map expression.
#[test]
fn keyword_list_comprehension_variable_is_read_in_its_body() {
    let mut c = connection();
    assert_eq!(
        rows(
            &mut c,
            "RETURN [as IN [1, 2, 3] WHERE as > 1 | as * 10] AS r"
        ),
        vec![vec![Value::List(
            vec![Value::Int(20), Value::Int(30)].into()
        )]]
    );
}

/// `union` is a map-literal key and is read back through a property access:
/// key position to reference position.
#[test]
fn keyword_map_key_is_read_back() {
    let mut c = connection();
    assert_eq!(
        rows(&mut c, "RETURN {union: 1}.union"),
        vec![vec![Value::Int(1)]]
    );
}

/// One statement crosses alias, binding and key positions: `as` is a WITH
/// alias and a map key/value, `where` is a pattern variable, and the
/// returned aliases `union`/`r` are keyword spellings too.
#[test]
fn keyword_names_cross_alias_binding_and_key_positions() {
    let mut c = connection();
    run(&mut c, "CREATE (:Movie {title: 'Inception'})");

    assert_eq!(
        rows(
            &mut c,
            "WITH 1 AS as MATCH (where:Movie) \
             RETURN where.title AS union, {as: as}.as AS r"
        ),
        vec![vec![string("Inception"), Value::Int(1)]]
    );
}
