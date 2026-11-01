use super::*;

#[test]
fn test_unsupported_administration_commands_are_recognized() {
    for (q, echoed) in unsupported_admin_commands() {
        let err = parse(q).expect_err("unsupported administration command must fail the parse");
        assert_eq!(err.detail, Some(UNSUPPORTED_ADMINISTRATION_DETAIL), "{q:?}");
        assert_eq!(err.message, echoed, "wrong echo for {q:?}");
    }
}

// Truncated or malformed administration commands are user input: every
// prefix of every unsupported form must parse to either the tagged
// `UnsupportedAdministrationCommand` error or an ordinary syntax error
// -- never a panic (the loud internal invariant in
// `unsupported_admin_clause` must stay unreachable from user input).
#[test]
fn test_unsupported_administration_truncations_never_panic() {
    for (q, _) in unsupported_admin_commands() {
        for end in 0..=q.len() {
            if !q.is_char_boundary(end) {
                continue;
            }
            let prefix = &q[..end];
            // Any Ok/Err outcome is fine; a panic fails the test.
            let _ = parse(prefix);
        }
    }
}

// ADR-0116 §2: the one supported form -- a sole `SET DEFAULT
// LANGUAGE CYPHER <n>` item with no WAIT -- still parses to a real
// `Clause::AlterDatabase`.
#[test]
fn test_alter_database_set_default_language_stays_supported() {
    for q in [
        "ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5",
        "ALTER DATABASE neo4j IF EXISTS SET DEFAULT LANGUAGE CYPHER 25",
    ] {
        let stmt = parse(q).unwrap_or_else(|e| panic!("{q:?} must parse: {e}"));
        assert!(
            matches!(stmt.clauses.as_slice(), [Clause::AlterDatabase(_)]),
            "{q:?} must lower to Clause::AlterDatabase, got {:?}",
            stmt.clauses
        );
    }
}

// ADR-0116 §2: `START`/`STOP`/`ALIAS` are recognized as command
// keywords only in the exact command shape; everywhere else they stay
// ordinary identifiers.
#[test]
fn test_start_stop_alias_stay_identifiers() {
    let stmt = parse("MATCH (start) WHERE start.alias = 'stop' RETURN start").unwrap();
    assert!(matches!(stmt.clauses.as_slice(), [Clause::Match(_), ..]));
    // A named path may still be called `database`/`alias`.
    parse("CREATE database = (a)-[:X]->(b)").unwrap();
}
