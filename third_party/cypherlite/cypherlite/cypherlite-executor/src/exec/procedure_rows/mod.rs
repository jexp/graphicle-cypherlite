//! Built-in procedure row producers: the graph-dependent rows behind
//! `builtin_procedure` (`dbms.components`, `dbms.clientConfig`,
//! `db.schema.*`, …). Argument names/types and yield columns come from the
//! single canonical signature registry (`crate::procedure_registry`); only
//! the rows are produced here. The generic `CALL` machinery and the
//! fulltext/vector query executors live in `exec::procedures`.

use std::collections::{BTreeMap, BTreeSet};

use cypherlite_core::Value;

use crate::graph::Procedure;
use crate::overlay::ExecGraph;

mod schema;
#[cfg(test)]
mod test_helpers;

use schema::{schema_node_type_rows, schema_rel_type_rows, schema_visualization_rows};

/// CypherLite engine version, single-sourced from the workspace version
/// (`version.workspace = true`), reported by `dbms.components` row 3
/// (`CypherLite`) (ADR-0142).
const CYPHERLITE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version reported by the `dbms.components` row 1 (`Neo4j Kernel`)
/// (ADR-0152): a deliberately-bumped **policy constant**, not derived from
/// the workspace version. It tracks the Neo4j calver month the Browser
/// compat surface was verified against and carries the `-cypherlite`
/// product suffix (Aura's `5.27-aura` convention) so it stays
/// distinguishable from any real Neo4j release. Bump it deliberately, with
/// a re-check of the browser.neo4j.io bundle gates — the sdk minimum-version
/// gate (`>= 4.4.0`, coerced loose semver) reads only this row's
/// `versions[0]` and throws on anything lower, blocking the Database
/// information sidebar (LABS-150). Must always be non-empty.
const NEO4J_KERNEL_VERSION: &str = "2026.10-cypherlite";

/// Builds the executable `Procedure` for a built-in name. Argument names/
/// types and yield columns come from the single canonical signature
/// registry (`crate::procedure_registry`) — only the graph-dependent *rows*
/// are produced here. `principal` is the connection's authenticated
/// principal (ADR-0273): only `dbms.showCurrentUser` reads it; `None` (a
/// surface with no notion of identity, or `--auth none`) yields the
/// documented placeholder.
pub(crate) fn builtin_procedure(
    graph: &ExecGraph<'_>,
    name: &str,
    principal: Option<&str>,
) -> Option<Procedure> {
    let signature = crate::procedure_registry::builtin_procedure_signature(name)
        .filter(|signature| signature.executable)?;
    let rows = match signature.name {
        "tx.getMetaData" => tx_get_metadata_rows(),
        "dbms.components" => dbms_components_rows(),
        "db.ping" => ping_rows(),
        "db.labels" => label_rows(graph),
        "db.relationshipTypes" => relationship_type_rows(graph),
        "db.propertyKeys" => property_key_rows(graph),
        "db.info" => db_info_rows(graph),
        "dbms.info" => dbms_info_rows(),
        "dbms.showCurrentUser" => dbms_show_current_user_rows(principal),
        "dbms.clientConfig" | "dbms.listConfig" => dbms_config_rows(),
        "dbms.licenseAgreementDetails" => dbms_license_agreement_rows(),
        "db.schema.nodeTypeProperties" => schema_node_type_rows(graph),
        "db.schema.relTypeProperties" => schema_rel_type_rows(graph),
        "db.schema.visualization" => schema_visualization_rows(graph),
        // tx.setMetaData, db.createLabel, db.indexes, db.constraints,
        // db.awaitIndexes: unit (void) procedures with no rows. The vector
        // and fulltext query procedures never reach here — `exec_call`
        // diverts them to their dedicated executors first.
        _ => Vec::new(),
    };
    Some(Procedure {
        name: name.into(),
        arg_names: signature
            .args
            .iter()
            .map(|arg| arg.name.to_string())
            .collect(),
        arg_types: signature
            .args
            .iter()
            .map(|arg| arg.type_str.to_string())
            .collect(),
        yield_names: signature
            .returns
            .iter()
            .map(|ret| ret.name.to_string())
            .collect(),
        rows,
    })
}

/// Row for `tx.getMetaData` (CIP-230): the fixed `source: 'cip230'`
/// metadata map.
fn tx_get_metadata_rows() -> Vec<BTreeMap<String, Value>> {
    vec![BTreeMap::from([(
        "metadata".into(),
        Value::Map(BTreeMap::from([("source".into(), Value::String("cip230".into()))]).into()),
    )])]
}

/// Shared producer for the single-column distinct-string procedures
/// (`db.labels`, `db.relationshipTypes`, `db.propertyKeys`): a `BTreeSet`
/// dedupes and sorts in one pass, then each value maps to a one-column row.
fn distinct_string_rows(
    values: impl IntoIterator<Item = String>,
    column: &str,
) -> Vec<BTreeMap<String, Value>> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|value| BTreeMap::from([(column.into(), Value::String(value.into()))]))
        .collect()
}

/// Row for `db.ping` (Neo4j Browser's connectivity probe): reaching this
/// code means the database answered, so the row is a constant `true`.
fn ping_rows() -> Vec<BTreeMap<String, Value>> {
    vec![BTreeMap::from([("success".into(), Value::Bool(true))])]
}

/// Rows for `db.labels`: every distinct stored node label, sorted.
fn label_rows(graph: &ExecGraph<'_>) -> Vec<BTreeMap<String, Value>> {
    let labels = graph
        .all_node_ids()
        .into_iter()
        .flat_map(|id| graph.node_labels(id).unwrap_or_default());
    distinct_string_rows(labels, "label")
}

/// Rows for `db.relationshipTypes`: every distinct stored relationship
/// type, sorted.
fn relationship_type_rows(graph: &ExecGraph<'_>) -> Vec<BTreeMap<String, Value>> {
    let types = graph
        .all_rel_ids()
        .into_iter()
        .filter_map(|id| graph.rel_type(id));
    distinct_string_rows(types, "relationshipType")
}

/// Rows for `db.propertyKeys`: every distinct property key on stored nodes
/// or relationships, sorted.
fn property_key_rows(graph: &ExecGraph<'_>) -> Vec<BTreeMap<String, Value>> {
    let node_keys = graph
        .all_node_ids()
        .into_iter()
        .flat_map(|id| graph.node_prop_keys(id));
    let rel_keys = graph
        .all_rel_ids()
        .into_iter()
        .flat_map(|id| graph.rel_prop_keys(id));
    distinct_string_rows(node_keys.chain(rel_keys), "propertyKey")
}

/// Row for `db.info`: the database the statement targets is the graph's
/// namespace (ADR-0116 §7).
fn db_info_rows(graph: &ExecGraph<'_>) -> Vec<BTreeMap<String, Value>> {
    let namespace = graph.namespace().as_str();
    vec![BTreeMap::from([
        ("id".into(), Value::String(namespace.into())),
        ("name".into(), Value::String(namespace.into())),
        ("creationDate".into(), Value::Null),
    ])]
}

/// Row for `dbms.info` (ADR-0142: Desktop-compat procedure,
/// `updateDBMSInfo`). The procedure arm only sees the graph, not the
/// session's current database, and CypherLite is a single embedded
/// database — so `name` is the constant `'system'` (the always-present
/// namespace, ADR-0116) standing in for "the current database name".
/// `creationDate` is a documented placeholder: the engine tracks no
/// creation time.
fn dbms_info_rows() -> Vec<BTreeMap<String, Value>> {
    vec![BTreeMap::from([
        ("id".into(), Value::String("cypherlite".into())),
        ("name".into(), Value::String("system".into())),
        (
            "creationDate".into(),
            Value::String("1970-01-01T00:00:00Z".into()),
        ),
    ])]
}

/// Row for `dbms.showCurrentUser` (ADR-0143: Browser/Desktop-compat
/// procedure, LABS-148). Session context beyond the principal is not
/// plumbed, so the other columns are fixed documented placeholders.
/// ADR-0273: `username` is the connection's authenticated principal when
/// the surface set one (Bolt after a validated HELLO), else the documented
/// placeholder `'cypherlite'`; `roles: ['admin']` conveys the engine's
/// actual authorization model (there is none) and `flags: []` is honest
/// (CypherLite has no user flags).
fn dbms_show_current_user_rows(principal: Option<&str>) -> Vec<BTreeMap<String, Value>> {
    vec![BTreeMap::from([
        (
            "username".into(),
            Value::String(principal.unwrap_or("cypherlite").into()),
        ),
        (
            "roles".into(),
            Value::List(vec![Value::String("admin".into())].into()),
        ),
        ("flags".into(), Value::List(Vec::new().into())),
    ])]
}

/// Row for `dbms.licenseAgreementDetails` (ADR-0143): honest constant —
/// CypherLite is Apache-2.0 (ADR-0055): no license agreement to accept and
/// no trial, so `status: 'yes'` with null off-trial counters.
fn dbms_license_agreement_rows() -> Vec<BTreeMap<String, Value>> {
    vec![BTreeMap::from([
        ("status".into(), Value::String("yes".into())),
        ("daysLeftOnTrial".into(), Value::Null),
        ("totalTrialDays".into(), Value::Null),
    ])]
}

/// Rows for `dbms.components` (ADR-0142, ADR-0152): three rows, order
/// load-bearing —
/// row 1 is `Neo4j Kernel` because Neo4j Desktop finds the version row *by
/// name* while the public web Browser reads only the first record's
/// `versions[0]`; row 2 feeds Desktop's `cypherVersions`; row 3 keeps
/// CypherLite's identification row. Row 1 reports the ADR-0152 policy
/// constant `NEO4J_KERNEL_VERSION`; row 3 reports the honest workspace
/// version. Every row reports edition `community`
/// (CypherLite has no enterprise tier).
fn dbms_components_rows() -> Vec<BTreeMap<String, Value>> {
    const COMPONENTS: &[(&str, &[&str])] = &[
        ("Neo4j Kernel", &[NEO4J_KERNEL_VERSION]),
        ("Cypher", &["5", "25"]),
        ("CypherLite", &[CYPHERLITE_VERSION]),
    ];
    COMPONENTS
        .iter()
        .map(|(name, versions)| {
            BTreeMap::from([
                ("name".into(), Value::String((*name).into())),
                (
                    "versions".into(),
                    Value::List(
                        versions
                            .iter()
                            .map(|version| Value::String((*version).into()))
                            .collect::<Vec<_>>()
                            .into(),
                    ),
                ),
                ("edition".into(), Value::String("community".into())),
            ])
        })
        .collect()
}

/// Rows for `dbms.clientConfig`/`dbms.listConfig` (ADR-0143): one row per
/// Neo4j 5.x+ default clientConfig allowlist key, in the Browser's lookup
/// order. On every row `dynamic: false`, `defaultValue`/`startupValue`
/// equal `value`, `explicitlySet: false`, `validValues: ''`. All rows are
/// documented placeholders except `server.metrics.prefix` and
/// `client.allow_telemetry`, which are honest (CypherLite exposes no
/// Neo4j-style metrics namespace, and engine telemetry is opt-in).
fn dbms_config_rows() -> Vec<BTreeMap<String, Value>> {
    const CONFIG: &[(&str, &str, &str)] = &[
        (
            "browser.allow_outgoing_connections",
            "true",
            "Whether the Browser may open outgoing connections to other servers.",
        ),
        (
            "browser.credential_timeout",
            "0",
            "Seconds before the Browser discards retained credentials (0 = never).",
        ),
        (
            "browser.post_connect_cmd",
            "",
            "Command the Browser runs after connecting.",
        ),
        (
            "browser.remote_content_hostname_whitelist",
            "*",
            "Hostnames the Browser may load remote content from.",
        ),
        (
            "browser.retain_connection_credentials",
            "false",
            "Whether the Browser retains connection credentials.",
        ),
        (
            "browser.retain_editor_history",
            "true",
            "Whether the Browser retains editor history.",
        ),
        (
            "client.allow_telemetry",
            "false",
            "Whether client telemetry collection is allowed.",
        ),
        (
            "dbms.security.auth_enabled",
            "true",
            "Whether the server requires authentication.",
        ),
        (
            "server.metrics.prefix",
            "cypherlite",
            "Prefix for reported metrics names.",
        ),
    ];
    CONFIG
        .iter()
        .map(|(name, value, description)| {
            BTreeMap::from([
                ("name".into(), Value::String((*name).into())),
                ("description".into(), Value::String((*description).into())),
                ("value".into(), Value::String((*value).into())),
                ("dynamic".into(), Value::Bool(false)),
                ("defaultValue".into(), Value::String((*value).into())),
                ("startupValue".into(), Value::String((*value).into())),
                ("explicitlySet".into(), Value::Bool(false)),
                ("validValues".into(), Value::String("".into())),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::test_helpers::{rows, rows_with_principal, string};
    use super::*;
    use crate::graph::Graph;

    #[test]
    fn dbms_components_returns_kernel_cypher_cypherlite_rows_with_edition() {
        // ADR-0142: three rows, order load-bearing — Neo4j Desktop finds the
        // version row by name (`Neo4j Kernel`), the public web Browser reads
        // only the first record's `versions[0]`, so row 1 is both first and
        // named `Neo4j Kernel`.
        let graph = Graph::default();
        let rows = rows(&graph, "dbms.components");

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["name"], string("Neo4j Kernel"));
        assert_eq!(
            rows[0]["versions"],
            Value::List(vec![string(NEO4J_KERNEL_VERSION)].into())
        );
        assert_eq!(rows[0]["edition"], string("community"));
        assert_eq!(rows[1]["name"], string("Cypher"));
        assert_eq!(
            rows[1]["versions"],
            Value::List(vec![string("5"), string("25")].into())
        );
        assert_eq!(rows[1]["edition"], string("community"));
        assert_eq!(rows[2]["name"], string("CypherLite"));
        assert_eq!(
            rows[2]["versions"],
            Value::List(vec![string(CYPHERLITE_VERSION)].into())
        );
        assert_eq!(rows[2]["edition"], string("community"));
    }

    #[test]
    fn db_ping_reports_success() {
        // The connectivity probe answers one constant row; the signature
        // declares the BOOLEAN `success` output.
        let rows = rows(&Graph::default(), "db.ping");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["success"], Value::Bool(true));
        let signature = crate::procedure_registry::builtin_procedure_signature("db.ping")
            .expect("db.ping is registered");
        assert_eq!(signature.returns[0].name, "success");
        assert_eq!(signature.returns[0].type_str, "BOOLEAN");
        assert!(signature.works_on_system);
    }

    #[test]
    fn db_info_reports_the_graphs_namespace() {
        // Default graph is the `neo4j` namespace.
        let graph = Graph::default();
        let neo4j_rows = rows(&graph, "db.info");
        assert_eq!(neo4j_rows.len(), 1);
        assert_eq!(neo4j_rows[0]["id"], string("neo4j"));
        assert_eq!(neo4j_rows[0]["name"], string("neo4j"));

        // A graph tagged with the system_ontologies namespace reports it
        // (ADR-0116 §7; the facade's ADR-0229 gate now rejects every
        // statement routed there before it can reach this, but the
        // graph-level identity rule is unchanged).
        let mut ontologies = Graph::default();
        ontologies.set_namespace(cypherlite_core::Namespace::SystemOntologies);
        let ontologies_rows = rows(&ontologies, "db.info");
        assert_eq!(ontologies_rows.len(), 1);
        assert_eq!(ontologies_rows[0]["id"], string("system_ontologies"));
        assert_eq!(ontologies_rows[0]["name"], string("system_ontologies"));
    }

    #[test]
    fn dbms_info_returns_the_desktop_compat_row() {
        // ADR-0142: one row; `name` is the constant `system` (the procedure
        // arm cannot see the session's current database) and `creationDate`
        // is a documented placeholder.
        let graph = Graph::default();
        let rows = rows(&graph, "dbms.info");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], string("cypherlite"));
        assert_eq!(rows[0]["name"], string("system"));
        assert_eq!(rows[0]["creationDate"], string("1970-01-01T00:00:00Z"));
    }

    #[test]
    fn dbms_info_signature_declares_id_name_creation_date_outputs() {
        let signature = crate::procedure_registry::builtin_procedure_signature("dbms.info")
            .expect("dbms.info is registered");
        assert!(signature.executable);
        let outputs: Vec<&str> = signature.returns.iter().map(|ret| ret.name).collect();
        assert_eq!(outputs, ["id", "name", "creationDate"]);
    }

    #[test]
    fn dbms_components_signature_declares_edition_output() {
        let signature = crate::procedure_registry::builtin_procedure_signature("dbms.components")
            .expect("dbms.components is registered");
        let outputs: Vec<&str> = signature.returns.iter().map(|ret| ret.name).collect();
        assert_eq!(outputs, ["name", "versions", "edition"]);
    }

    #[test]
    fn dbms_show_current_user_returns_the_placeholder_row_without_a_principal() {
        // ADR-0143/0273: with no connection principal (every non-Bolt
        // surface, and Bolt under `--auth none`), one fixed
        // documented-placeholder row.
        let graph = Graph::default();
        let rows = rows(&graph, "dbms.showCurrentUser");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["username"], string("cypherlite"));
        assert_eq!(rows[0]["roles"], Value::List(vec![string("admin")].into()));
        assert_eq!(rows[0]["flags"], Value::List(Vec::new().into()));
    }

    #[test]
    fn dbms_show_current_user_reports_the_connection_principal() {
        // ADR-0273: a set principal becomes `username`; roles/flags keep the
        // ADR-0143 values.
        let graph = Graph::default();
        let rows = rows_with_principal(&graph, "dbms.showCurrentUser", Some("alice"));

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["username"], string("alice"));
        assert_eq!(rows[0]["roles"], Value::List(vec![string("admin")].into()));
        assert_eq!(rows[0]["flags"], Value::List(Vec::new().into()));
    }

    #[test]
    fn dbms_config_rows_match_the_adr0143_allowlist() {
        // ADR-0143: nine rows, one per Neo4j 5.x+ allowlist key; identical
        // for dbms.clientConfig and dbms.listConfig.
        let graph = Graph::default();
        let client_rows = rows(&graph, "dbms.clientConfig");
        let list_rows = rows(&graph, "dbms.listConfig");
        assert_eq!(client_rows, list_rows);
        assert_eq!(client_rows.len(), 9);

        let expected: &[(&str, &str)] = &[
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
        for (row, (name, value)) in client_rows.iter().zip(expected) {
            assert_eq!(row["name"], string(name));
            assert_eq!(row["value"], string(value));
            assert_eq!(row["dynamic"], Value::Bool(false));
            assert_eq!(row["defaultValue"], string(value));
            assert_eq!(row["startupValue"], string(value));
            assert_eq!(row["explicitlySet"], Value::Bool(false));
            assert_eq!(row["validValues"], string(""));
            // Every row carries a non-empty fixed description string.
            assert!(matches!(&row["description"], Value::String(d) if !d.is_empty()));
        }
    }

    #[test]
    fn dbms_license_agreement_details_returns_the_honest_row() {
        // ADR-0143: honest constant — Apache-2.0 (ADR-0055), no agreement
        // to accept and no trial.
        let graph = Graph::default();
        let rows = rows(&graph, "dbms.licenseAgreementDetails");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["status"], string("yes"));
        assert_eq!(rows[0]["daysLeftOnTrial"], Value::Null);
        assert_eq!(rows[0]["totalTrialDays"], Value::Null);
    }
}
