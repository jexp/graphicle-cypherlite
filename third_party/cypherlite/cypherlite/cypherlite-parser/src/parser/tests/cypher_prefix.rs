use super::*;

// ---- CIP-150: CYPHER version prefix ----

#[test]
fn test_cypher_25_prefix_is_transparent() {
    let stmt = parse("CYPHER 25 RETURN 1 AS n").unwrap();
    assert_eq!(stmt.clauses.len(), 1);
    assert!(matches!(stmt.clauses[0], Clause::Return(_)));
    assert_eq!(stmt.cypher_version, Some(25));
}

#[test]
fn test_cypher_5_prefix_is_accepted_and_recorded() {
    // CIP-150: `CYPHER 5` is real, per-statement version selection --
    // parsed and recorded on the statement, not a hard error. CypherLite
    // still implements a single grammar/engine; version-differentiated
    // behavior (where it exists) is gated at validation/execution time,
    // not by rejecting the preamble itself.
    let stmt = parse("CYPHER 5 RETURN 1 AS n").unwrap();
    assert_eq!(stmt.clauses.len(), 1);
    assert_eq!(stmt.cypher_version, Some(5));
}

#[test]
fn test_cypher_no_prefix_leaves_version_unset() {
    let stmt = parse("RETURN 1 AS n").unwrap();
    assert_eq!(stmt.cypher_version, None);
}

#[test]
fn test_cypher_unknown_version_number_is_rejected() {
    assert!(parse("CYPHER 99 RETURN 1 AS n").is_err());
}

#[test]
fn test_cypher_decimal_version_is_rejected() {
    assert!(parse("CYPHER 5.0 RETURN 1 AS n").is_err());
}

#[test]
fn test_cypher_variable_named_cypher_is_unaffected() {
    // The CYPHER-prefix check only fires once, at the very start of
    // the whole query -- `cypher` used as an ordinary variable name
    // anywhere else must keep working.
    let stmt = parse("WITH 1 AS cypher RETURN cypher").unwrap();
    assert_eq!(stmt.clauses.len(), 2);
}

#[test]
fn test_cypher_same_version_repeated_is_accepted() {
    let stmt = parse("CYPHER 5 CYPHER 5 RETURN 1 AS n").unwrap();
    assert_eq!(stmt.cypher_version, Some(5));
}

#[test]
fn test_cypher_conflicting_versions_are_rejected() {
    assert!(parse("CYPHER 5 CYPHER 25 RETURN 1 AS n").is_err());
}

#[test]
fn test_cypher_versionless_options_clause_is_accepted() {
    let stmt = parse("CYPHER planner=cost RETURN 1 AS n").unwrap();
    assert_eq!(stmt.cypher_version, None);
}

#[test]
fn test_cypher_unrecognized_option_key_is_rejected() {
    assert!(parse("CYPHER 5 foo=bar RETURN 1 AS n").is_err());
}

#[test]
fn test_cypher_conflicting_option_values_are_rejected() {
    assert!(parse("CYPHER 5 runtime=slotted runtime=pipelined RETURN 1 AS n").is_err());
}

#[test]
fn test_cypher_explain_before_version_is_accepted() {
    let stmt = parse("EXPLAIN CYPHER 5 runtime=slotted RETURN 1 AS n").unwrap();
    assert!(stmt.explain);
    assert_eq!(stmt.cypher_version, Some(5));
}

#[test]
fn test_cypher_version_before_explain_is_accepted() {
    let stmt = parse("CYPHER 5 runtime=slotted EXPLAIN RETURN 1 AS n").unwrap();
    assert!(stmt.explain);
    assert_eq!(stmt.cypher_version, Some(5));
}

#[test]
fn test_cypher_explain_inside_options_clause_is_rejected() {
    // Doc example E1: once EXPLAIN has ended a CYPHER options clause,
    // a bare `key=value` with no fresh CYPHER keyword falls through to
    // ordinary statement parsing and is a generic syntax error there.
    assert!(parse("CYPHER 5 EXPLAIN runtime=slotted RETURN 1 AS n").is_err());
}
