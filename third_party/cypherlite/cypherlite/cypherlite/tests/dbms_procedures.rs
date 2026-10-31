//! Public-API integration tests for `dbms.components` and `dbms.info`
//! (ADR-0142) plus `dbms.showCurrentUser` (ADR-0143 rows, ADR-0273
//! principal plumbing), `dbms.clientConfig`,
//! `dbms.listConfig` and `dbms.licenseAgreementDetails` (ADR-0143): the
//! three Browser/Desktop-compat
//! component rows (`Neo4j Kernel` first, then `Cypher`, then `CypherLite` —
//! row order is load-bearing: Neo4j Desktop finds the version row by name,
//! the public web Browser's version gate reads only the first record; the
//! `Neo4j Kernel` row reports the ADR-0152 policy constant, the
//! `CypherLite` row the honest workspace version), the
//! `edition` output (`community` on all rows; CypherLite has no enterprise
//! tier), the `dbms.info` Desktop-compat row, the ADR-0143 fixed
//! rows, the ADR-0273 `Connection::set_principal` behavior, and `YIELD`
//! validation.

mod common;

use cypherlite::Value;

use common::{assert_unknown_output_fails, connection, string};

/// CypherLite engine version. The executor's `CYPHERLITE_VERSION` const is
/// crate-private, so this re-derives it from the test crate's own
/// `env!("CARGO_PKG_VERSION")` — both crates are workspace-versioned
/// (`version.workspace = true`), so the strings are identical.
const CYPHERLITE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version reported by the `dbms.components` `Neo4j Kernel` row (ADR-0152):
/// a deliberately-bumped policy constant, not the workspace version. Kept
/// in sync with the executor's crate-private `NEO4J_KERNEL_VERSION`.
const NEO4J_KERNEL_VERSION: &str = "2026.10-cypherlite";

#[test]
fn yield_name_versions_edition_returns_all_component_rows_in_order() {
    let mut connection = connection();
    let result = connection
        .query(
            "CALL dbms.components() YIELD name, versions, edition RETURN name, versions, edition",
        )
        .unwrap();

    assert_eq!(result.header.columns, ["name", "versions", "edition"]);
    assert_eq!(
        result.rows,
        vec![
            vec![
                string("Neo4j Kernel"),
                Value::List(vec![string(NEO4J_KERNEL_VERSION)].into()),
                string("community"),
            ],
            vec![
                string("Cypher"),
                Value::List(vec![string("5"), string("25")].into()),
                string("community"),
            ],
            vec![
                string("CypherLite"),
                Value::List(vec![string(CYPHERLITE_VERSION)].into()),
                string("community"),
            ],
        ]
    );
}

#[test]
fn call_without_yield_returns_all_component_rows() {
    let mut connection = connection();
    let result = connection.query("CALL dbms.components()").unwrap();

    assert_eq!(result.header.columns, ["name", "versions", "edition"]);
    assert_eq!(result.rows.len(), 3);
    assert_eq!(result.rows[0][0], string("Neo4j Kernel"));
    assert_eq!(result.rows[1][0], string("Cypher"));
    assert_eq!(result.rows[2][0], string("CypherLite"));
}

#[test]
fn yield_of_unknown_column_still_fails_validation() {
    assert_unknown_output_fails(
        "CALL dbms.components() YIELD noSuchColumn RETURN noSuchColumn",
        "noSuchColumn",
    );
}

#[test]
fn dbms_info_returns_the_desktop_compat_row() {
    let mut connection = connection();
    let result = connection.query("CALL dbms.info()").unwrap();

    assert_eq!(result.header.columns, ["id", "name", "creationDate"]);
    assert_eq!(
        result.rows,
        vec![vec![
            string("cypherlite"),
            string("system"),
            string("1970-01-01T00:00:00Z"),
        ]]
    );
}

#[test]
fn dbms_info_yield_subset_returns_only_that_column() {
    let mut connection = connection();
    let result = connection
        .query("CALL dbms.info() YIELD name RETURN name")
        .unwrap();

    assert_eq!(result.header.columns, ["name"]);
    assert_eq!(result.rows, vec![vec![string("system")]]);
}

#[test]
fn dbms_info_yield_of_unknown_column_fails_validation() {
    assert_unknown_output_fails(
        "CALL dbms.info() YIELD noSuchColumn RETURN noSuchColumn",
        "noSuchColumn",
    );
}

// --- ADR-0143/0273: dbms.showCurrentUser ----------------------------------

#[test]
fn dbms_show_current_user_returns_the_placeholder_row() {
    // ADR-0143/0273: no principal set (every non-Bolt surface, and Bolt
    // under `--auth none`) — the documented placeholder row.
    let mut connection = connection();
    let result = connection.query("CALL dbms.showCurrentUser()").unwrap();

    assert_eq!(result.header.columns, ["username", "roles", "flags"]);
    assert_eq!(
        result.rows,
        vec![vec![
            string("cypherlite"),
            Value::List(vec![string("admin")].into()),
            Value::List(vec![].into()),
        ]]
    );
}

#[test]
fn dbms_show_current_user_reports_the_connection_principal() {
    // ADR-0273: a principal set on the connection becomes `username`;
    // roles/flags keep the ADR-0143 values.
    let mut connection = connection();
    connection.set_principal(Some("alice"));
    assert_eq!(connection.principal(), Some("alice"));
    let result = connection.query("CALL dbms.showCurrentUser()").unwrap();

    assert_eq!(result.header.columns, ["username", "roles", "flags"]);
    assert_eq!(
        result.rows,
        vec![vec![
            string("alice"),
            Value::List(vec![string("admin")].into()),
            Value::List(vec![].into()),
        ]]
    );
}

#[test]
fn dbms_show_current_user_after_clearing_the_principal_returns_the_placeholder() {
    // ADR-0273: `set_principal(None)` clears the principal; the documented
    // placeholder returns.
    let mut connection = connection();
    connection.set_principal(Some("alice"));
    connection.set_principal(None);
    assert_eq!(connection.principal(), None);
    let result = connection.query("CALL dbms.showCurrentUser()").unwrap();

    assert_eq!(
        result.rows,
        vec![vec![
            string("cypherlite"),
            Value::List(vec![string("admin")].into()),
            Value::List(vec![].into()),
        ]]
    );
}

#[test]
fn dbms_show_current_user_reads_the_principal_inside_a_transaction() {
    // ADR-0273: the principal rides the explicit-transaction path too.
    let mut connection = connection();
    connection.set_principal(Some("alice"));
    connection.begin().unwrap();
    let result = connection.query("CALL dbms.showCurrentUser()").unwrap();
    assert_eq!(result.rows[0][0], string("alice"));
    connection.rollback().unwrap();
}

#[test]
fn dbms_show_current_user_yield_each_declared_column() {
    let mut connection = connection();
    let result = connection
        .query("CALL dbms.showCurrentUser() YIELD username, roles, flags RETURN username, roles, flags")
        .unwrap();

    assert_eq!(result.header.columns, ["username", "roles", "flags"]);
    assert_eq!(
        result.rows,
        vec![vec![
            string("cypherlite"),
            Value::List(vec![string("admin")].into()),
            Value::List(vec![].into()),
        ]]
    );
}

#[test]
fn dbms_show_current_user_yield_of_unknown_column_fails_validation() {
    assert_unknown_output_fails(
        "CALL dbms.showCurrentUser() YIELD noSuchColumn RETURN noSuchColumn",
        "noSuchColumn",
    );
}

// --- ADR-0143: dbms.clientConfig / dbms.listConfig ----------------------

/// The nine ADR-0143 allowlist rows as `(name, value)` pairs, in order.
const CONFIG_ROWS: &[(&str, &str)] = &[
    ("browser.allow_outgoing_connections", "true"),
    ("browser.credential_timeout", "0"),
    ("browser.post_connect_cmd", ""),
    ("browser.remote_content_hostname_whitelist", "*"),
    ("browser.retain_connection_credentials", "false"),
    ("browser.retain_editor_history", "true"),
    ("client.allow_telemetry", "false"),
    ("dbms.security.auth_enabled", "true"),
    ("server.metrics.prefix", "cypherlite"),
];

/// Asserts the full 8-column ConfigResult shape and the nine pinned rows
/// for one of the config procedures (ADR-0143: identical for
/// `dbms.clientConfig` and `dbms.listConfig`).
fn assert_config_rows(procedure: &str) {
    let mut connection = connection();
    let result = connection
        .query(&format!(
            "CALL {procedure}() YIELD name, description, value, dynamic, defaultValue, startupValue, explicitlySet, validValues RETURN name, description, value, dynamic, defaultValue, startupValue, explicitlySet, validValues"
        ))
        .unwrap();

    assert_eq!(
        result.header.columns,
        [
            "name",
            "description",
            "value",
            "dynamic",
            "defaultValue",
            "startupValue",
            "explicitlySet",
            "validValues"
        ]
    );
    assert_eq!(result.rows.len(), 9);
    for (row, (name, value)) in result.rows.iter().zip(CONFIG_ROWS) {
        assert_eq!(row[0], string(name));
        // description is a short fixed string; only its non-emptiness is
        // part of the contract.
        assert!(
            matches!(&row[1], Value::String(d) if !d.is_empty()),
            "description should be a non-empty string, got {:?}",
            row[1]
        );
        assert_eq!(row[2], string(value));
        assert_eq!(row[3], Value::Bool(false));
        assert_eq!(row[4], string(value));
        assert_eq!(row[5], string(value));
        assert_eq!(row[6], Value::Bool(false));
        assert_eq!(row[7], string(""));
    }
}

#[test]
fn dbms_client_config_returns_the_nine_allowlist_rows() {
    assert_config_rows("dbms.clientConfig");
}

#[test]
fn dbms_list_config_returns_the_same_nine_rows() {
    assert_config_rows("dbms.listConfig");
}

#[test]
fn dbms_client_config_yield_subset_returns_only_that_column() {
    let mut connection = connection();
    let result = connection
        .query("CALL dbms.clientConfig() YIELD name, value RETURN name, value")
        .unwrap();

    assert_eq!(result.header.columns, ["name", "value"]);
    assert_eq!(result.rows.len(), 9);
    assert_eq!(
        result.rows[0],
        [string("browser.allow_outgoing_connections"), string("true")]
    );
}

#[test]
fn dbms_list_config_yield_subset_returns_only_that_column() {
    let mut connection = connection();
    let result = connection
        .query("CALL dbms.listConfig() YIELD value RETURN value")
        .unwrap();

    assert_eq!(result.header.columns, ["value"]);
    assert_eq!(result.rows.len(), 9);
    assert_eq!(result.rows[8], [string("cypherlite")]);
}

#[test]
fn dbms_client_config_yield_of_unknown_column_fails_validation() {
    assert_unknown_output_fails(
        "CALL dbms.clientConfig() YIELD noSuchColumn RETURN noSuchColumn",
        "noSuchColumn",
    );
}

#[test]
fn dbms_list_config_yield_of_unknown_column_fails_validation() {
    assert_unknown_output_fails(
        "CALL dbms.listConfig() YIELD noSuchColumn RETURN noSuchColumn",
        "noSuchColumn",
    );
}

// --- ADR-0143: dbms.licenseAgreementDetails -----------------------------

#[test]
fn dbms_license_agreement_details_returns_the_honest_row() {
    let mut connection = connection();
    let result = connection
        .query("CALL dbms.licenseAgreementDetails()")
        .unwrap();

    assert_eq!(
        result.header.columns,
        ["status", "daysLeftOnTrial", "totalTrialDays"]
    );
    assert_eq!(
        result.rows,
        vec![vec![string("yes"), Value::Null, Value::Null]]
    );
}

#[test]
fn dbms_license_agreement_details_yield_each_declared_column() {
    let mut connection = connection();
    let result = connection
        .query("CALL dbms.licenseAgreementDetails() YIELD status, daysLeftOnTrial, totalTrialDays RETURN status, daysLeftOnTrial, totalTrialDays")
        .unwrap();

    assert_eq!(
        result.header.columns,
        ["status", "daysLeftOnTrial", "totalTrialDays"]
    );
    assert_eq!(
        result.rows,
        vec![vec![string("yes"), Value::Null, Value::Null]]
    );
}

#[test]
fn dbms_license_agreement_details_yield_of_unknown_column_fails_validation() {
    assert_unknown_output_fails(
        "CALL dbms.licenseAgreementDetails() YIELD noSuchColumn RETURN noSuchColumn",
        "noSuchColumn",
    );
}
