//! LABS-188 / ADR-0182: unquoted Unicode identifiers end-to-end through the
//! public `cypherlite::Database` API. A node and a relationship carry a
//! Unicode label, relationship type and property key; they match back by the
//! same unquoted names, a Unicode-named parameter binds, and the same
//! unquoted query returns the same row after the native on-disk store is
//! closed and reopened (durable WAL replay).

mod common;

use std::collections::BTreeMap;

use cypherlite::{Database, Value};

use common::string;

/// The unquoted names, used verbatim by both the write and the read query:
/// label `Ärzte`, relationship type `BESCHÄFTIGT`, property key `Größe` and
/// parameter `$größe`.
const CREATE: &str = "CREATE (a:Ärzte {Größe: 'Dr. Müller'})-[r:BESCHÄFTIGT {seit: 2019}]->(b:Praxis {Größe: 'Praxis Ünver'})";
const MATCH: &str = "MATCH (a:Ärzte)-[r:BESCHÄFTIGT]->(b:Praxis) WHERE a.Größe = $größe \
                     RETURN labels(a) AS labels, r.seit AS seit, b.Größe AS größe";

/// The one row `MATCH` must return: the unquoted Unicode label comes back
/// from `labels()`, and the relationship property and parameter-filtered
/// node property come back unchanged.
fn expected_row() -> Vec<Value> {
    vec![
        Value::List(vec![string("Ärzte")].into()),
        Value::Int(2019),
        string("Praxis Ünver"),
    ]
}

fn match_rows(connection: &mut cypherlite::Connection) -> Vec<Vec<Value>> {
    let params = BTreeMap::from([("größe".to_string(), string("Dr. Müller"))]);
    connection
        .query_with_params(MATCH, params)
        .unwrap_or_else(|e| panic!("MATCH failed: {e}\n{MATCH}"))
        .rows
}

#[test]
fn unquoted_unicode_identifiers_roundtrip_and_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();

    {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        c.query(CREATE)
            .unwrap_or_else(|e| panic!("CREATE failed: {e}\n{CREATE}"));
        assert_eq!(match_rows(&mut c), vec![expected_row()]);
    }

    // The unquoted query still matches after the store is reopened: the
    // Unicode names were durable and the reopened lexer reads them back.
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    assert_eq!(match_rows(&mut c), vec![expected_row()]);
}
