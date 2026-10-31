//! Canonical `SHOW …`/`TERMINATE …` column tables: the single source of
//! truth for which columns each catalog command produces by default (bare,
//! no `YIELD`) and in full (`YIELD *`).
//!
//! Three consumers share these tables so they cannot drift apart:
//!
//! - front validation (`validate`): bare-`WHERE` scope and the
//!   InvalidShowColumn YIELD checks drive off the default/full split;
//! - planner binding (`catalog_output_names`): which names a catalog clause
//!   introduces for later clauses and `NEXT` stages;
//! - executor materialization (`exec::catalog`): bare-projection columns and
//!   result column order.
//!
//! Default/full membership follows the Neo4j-verified CIP scenarios (CIP-42,
//! CIP-46, CIP-79, CIP-84, CIP-157, CIP-170, CIP-187); ordering follows the
//! executor's catalog-row construction, which is what bare-`SHOW` result
//! column order exposes.

use cypherlite_parser::{Clause, Expr, PageStage, YieldList, YieldMode};

/// `SHOW FUNCTIONS`'s default (bare, no-`YIELD`) columns -- CIP-157
/// `show-functions-deprecatedby.feature` [7]/[9] confirm bare `SHOW
/// FUNCTIONS` returns exactly `name`/`category`/`description`, proving
/// every other column (including the pre-existing `isBuiltIn`/
/// `aggregating`) is Full-only, not Default.
pub const DEFAULT_FUNCTION_COLUMNS: &[&str] = &["name", "category", "description"];

/// `SHOW FUNCTIONS`'s Full-only columns: `isDeprecated` (CIP-84) and
/// `deprecatedBy` (CIP-157) alongside the pre-existing `isBuiltIn`/
/// `signature`/`argumentDescription`/`returnDescription`/`aggregating`.
pub const NON_DEFAULT_FUNCTION_COLUMNS: &[&str] = &[
    "isBuiltIn",
    "signature",
    "argumentDescription",
    "returnDescription",
    "aggregating",
    "isDeprecated",
    "deprecatedBy",
];

pub fn full_function_columns() -> Vec<String> {
    DEFAULT_FUNCTION_COLUMNS
        .iter()
        .chain(NON_DEFAULT_FUNCTION_COLUMNS.iter())
        .map(|s| s.to_string())
        .collect()
}

/// `SHOW CONSTRAINTS`'s default (bare, no-`YIELD`) columns -- CIP-46's
/// `ownedIndex` (replacing the removed `ownedIndexId`) and CIP-187's
/// `enforcedLabel` are both confirmed default columns (idx/con bare-WHERE
/// scenarios); `id` is a server-default column too (bare
/// `WHERE id IS NOT NULL` compiles against Neo4j) and `propertyType` was
/// already modeled as bare-reachable before this batch.
pub const DEFAULT_CONSTRAINT_COLUMNS: &[&str] = &[
    "id",
    "name",
    "type",
    "entityType",
    "labelsOrTypes",
    "properties",
    "ownedIndex",
    "propertyType",
    "enforcedLabel",
];

/// `SHOW CONSTRAINTS`'s Full-only columns: CIP-187 confirms
/// `classification`/`options`/`createStatement` all require an explicit
/// `YIELD` (01[2]/[3]/[4] -- `CALL { SHOW CONSTRAINTS } RETURN name,
/// classification` etc. fail to compile).
pub const NON_DEFAULT_CONSTRAINT_COLUMNS: &[&str] =
    &["classification", "options", "createStatement"];

pub fn full_constraint_columns() -> Vec<String> {
    DEFAULT_CONSTRAINT_COLUMNS
        .iter()
        .chain(NON_DEFAULT_CONSTRAINT_COLUMNS.iter())
        .map(|s| s.to_string())
        .collect()
}

/// `SHOW INDEXES`'s default (bare, no-`YIELD`) columns -- CIP-46's
/// `owningConstraint` (replacing the removed `uniqueness`) and CIP-79's
/// `lastRead`/`readCount` are all confirmed default columns, as are the
/// server columns `id`/`state`/`populationPercent` (bare
/// `WHERE state = 'ONLINE'` compiles against Neo4j). `options`/
/// `createStatement` are *not* default columns (bare `WHERE options …`
/// fails to compile against Neo4j).
pub const DEFAULT_INDEX_COLUMNS: &[&str] = &[
    "id",
    "name",
    "state",
    "populationPercent",
    "type",
    "entityType",
    "labelsOrTypes",
    "properties",
    "indexProvider",
    "owningConstraint",
    "lastRead",
    "readCount",
];

/// `SHOW INDEXES`'s Full-only columns: CIP-79 confirms `trackedSince`
/// requires an explicit `YIELD` (columns S2: bare `WHERE trackedSince ...`
/// fails to compile); `options`/`failureMessage`/`createStatement` are
/// likewise yield-only.
pub const NON_DEFAULT_INDEX_COLUMNS: &[&str] = &[
    "trackedSince",
    "options",
    "failureMessage",
    "createStatement",
];

pub fn full_index_columns() -> Vec<String> {
    DEFAULT_INDEX_COLUMNS
        .iter()
        .chain(NON_DEFAULT_INDEX_COLUMNS.iter())
        .map(|s| s.to_string())
        .collect()
}

/// `SHOW DATABASES`'s columns that are visible to a bare (no-`YIELD`)
/// clause -- the engine default column set (name/type/aliases plus the
/// topology/status columns Neo4j shows by default) alongside CIP-42's
/// `constituents`/`home`.
pub const DEFAULT_DATABASE_COLUMNS: &[&str] = &[
    "name",
    "type",
    "aliases",
    "access",
    "address",
    "role",
    "writer",
    "requestedStatus",
    "currentStatus",
    "statusMessage",
    "default",
    "home",
    "constituents",
];

/// CIP-42's four new columns, all documented `Default column = n` -- only
/// reachable via an explicit `YIELD`/`YIELD *`, never a bare clause (CIP-42
/// show-databases-new-columns.feature S13). CIP-170 adds a fifth,
/// `defaultLanguage` ("The defaultLanguage column is not shown by
/// default", 03/[1]), which belongs in the same non-default bucket.
pub const NON_DEFAULT_DATABASE_COLUMNS: &[&str] = &[
    "creationTime",
    "lastStartTime",
    "lastStopTime",
    "store",
    "defaultLanguage",
];

pub fn full_database_columns() -> Vec<String> {
    DEFAULT_DATABASE_COLUMNS
        .iter()
        .chain(NON_DEFAULT_DATABASE_COLUMNS.iter())
        .map(|s| s.to_string())
        .collect()
}

/// `SHOW PROCEDURES`'s default (bare, no-`YIELD`) columns -- CIP-157
/// `show-procedures-deprecatedby.feature` [5] confirms bare `SHOW
/// PROCEDURES` returns exactly `name`/`description`/`mode`/
/// `worksOnSystem`.
pub const DEFAULT_PROCEDURE_COLUMNS: &[&str] = &["name", "description", "mode", "worksOnSystem"];

/// `SHOW PROCEDURES`'s Full-only columns: `isDeprecated` (CIP-84) and
/// `deprecatedBy` (CIP-157) alongside the pre-existing `option` map,
/// `signature`/`argumentDescription`/`returnDescription`/`admin`.
pub const NON_DEFAULT_PROCEDURE_COLUMNS: &[&str] = &[
    "signature",
    "argumentDescription",
    "returnDescription",
    "admin",
    "isDeprecated",
    "deprecatedBy",
    "option",
];

pub fn full_procedure_columns() -> Vec<String> {
    DEFAULT_PROCEDURE_COLUMNS
        .iter()
        .chain(NON_DEFAULT_PROCEDURE_COLUMNS.iter())
        .map(|s| s.to_string())
        .collect()
}

/// `SHOW SETTINGS`'s default (bare, no-`YIELD`) columns (CIP-68 grammar
/// foundation).
pub const DEFAULT_SETTING_COLUMNS: &[&str] = &["name", "value"];

/// `SHOW SETTINGS`'s Full-only columns: `isDeprecated` (CIP-84).
pub const NON_DEFAULT_SETTING_COLUMNS: &[&str] = &["isDeprecated"];

pub fn full_setting_columns() -> Vec<String> {
    DEFAULT_SETTING_COLUMNS
        .iter()
        .chain(NON_DEFAULT_SETTING_COLUMNS.iter())
        .map(|s| s.to_string())
        .collect()
}

/// `SHOW GRAPH TYPE <name>`: a fixed `name`/`body` column pair (CIP-180; no
/// `YIELD *`/default-vs-full distinction -- both columns are always
/// available).
pub const GRAPH_TYPE_COLUMNS: &[&str] = &["name", "body"];

/// `SHOW CURRENT GRAPH TYPE`'s columns: a fixed single `specification`
/// column (CIP-184), or -- under CIP-257's `AS GRAPH` extension -- the
/// fixed `nodes`/`relationships` pair instead.
pub const CURRENT_GRAPH_TYPE_COLUMNS: &[&str] = &["specification"];
pub const CURRENT_GRAPH_TYPE_AS_GRAPH_COLUMNS: &[&str] = &["nodes", "relationships"];

/// `SHOW TRANSACTION[S]`'s columns (no default-vs-full split).
pub const DEFAULT_TRANSACTION_COLUMNS: &[&str] = &["transactionId", "currentQuery", "database"];

/// `TERMINATE TRANSACTION[S]`'s columns (no default-vs-full split).
pub const TERMINATE_TRANSACTION_COLUMNS: &[&str] = &["transactionId", "username", "message"];

/// Everything the row-shaping and planning layers need about one catalog
/// command: its yield tail, WHERE, YIELD-level paging stages, whether its
/// metadata rows need Null pre-fill, and its default/full column tables.
/// One descriptor per command, from the one exhaustive [`catalog_info`]
/// match, so the command vocabulary is listed exactly once.
pub struct CatalogInfo<'a> {
    /// The command's user-facing label, used by diagnostics.
    pub command: &'static str,
    pub yields: &'a YieldList,
    pub where_expr: Option<&'a Expr>,
    pub stages: &'a [PageStage],
    pub prefill: bool,
    /// Whether the front-end SHOW validator owns this command's YIELD/WHERE
    /// check (the seven pre-existing families; the CIP-68 additions keep
    /// their prior behavior).
    pub validate_columns: bool,
    /// Whether the command belongs to the CIP-123/242/253 composability
    /// family. The two graph-type commands do not: they carry their own
    /// CIP-184 rules and error ordering.
    pub composable: bool,
    /// Whether the command is one of the two graph-schema catalogs
    /// (`SHOW INDEXES`/`SHOW CONSTRAINTS`) that belong to an individual
    /// graph database, which CIP-123's `USE system` composability rule
    /// singles out.
    pub graph_schema_catalog: bool,
    pub default_columns: &'static [&'static str],
    pub full_columns: Vec<String>,
}

/// How a catalog clause projects its columns: `YIELD *` passes the raw row
/// through; every other mode fills each output column from its source
/// column.
pub enum CatalogProjection {
    /// Merge every column of the raw catalog row into the outer row.
    Star,
    /// The ordered (output column, source column) pairs to project.
    Columns(Vec<(String, String)>),
}

impl CatalogInfo<'_> {
    /// The one derivation from the yield mode to the projection columns.
    pub fn projection(&self) -> CatalogProjection {
        match self.yields.mode() {
            YieldMode::Star => CatalogProjection::Star,
            YieldMode::Default => CatalogProjection::Columns(
                self.default_columns
                    .iter()
                    .map(|column| ((*column).to_string(), (*column).to_string()))
                    .collect(),
            ),
            YieldMode::Named => CatalogProjection::Columns(
                self.yields
                    .pairs()
                    .map(|(name, _, effective)| (effective.to_owned(), name.to_owned()))
                    .collect(),
            ),
        }
    }
}

/// The one catalog-command classification, exhaustive over every parser
/// clause (no `_ =>` fallback): a new variant is a compile error until it
/// is classified. Every command fact — label, yield tail, WHERE, stages,
/// prefill, validator ownership, and the default/full column tables — is a
/// field of the returned descriptor.
pub fn catalog_info(clause: &Clause) -> Option<CatalogInfo<'_>> {
    macro_rules! info {
        (
            command: $command:expr,
            yields: $yields:expr,
            where_expr: $where:expr,
            stages: $stages:expr,
            prefill: $prefill:expr,
            validate_columns: $validate:expr,
            composable: $composable:expr,
            graph_schema_catalog: $graph_schema:expr,
            defaults: $defaults:expr,
            full: $full:expr,
        ) => {
            Some(CatalogInfo {
                command: $command,
                yields: $yields,
                where_expr: $where,
                stages: $stages,
                prefill: $prefill,
                validate_columns: $validate,
                composable: $composable,
                graph_schema_catalog: $graph_schema,
                default_columns: $defaults,
                full_columns: $full,
            })
        };
    }
    match clause {
        Clause::ShowFunctions(c) => info!(
            command: "SHOW FUNCTIONS",
            yields: &c.yields,
            where_expr: c.where_expr.as_ref(),
            stages: &[],
            prefill: true,
            validate_columns: true,
            composable: true,
            graph_schema_catalog: false,
            defaults: DEFAULT_FUNCTION_COLUMNS,
            full: full_function_columns(),
        ),
        Clause::ShowConstraints(c) => info!(
            command: "SHOW CONSTRAINTS",
            yields: &c.yields,
            where_expr: c.where_expr.as_ref(),
            stages: &c.stages,
            prefill: false,
            validate_columns: true,
            composable: true,
            graph_schema_catalog: true,
            defaults: DEFAULT_CONSTRAINT_COLUMNS,
            full: full_constraint_columns(),
        ),
        Clause::ShowIndexes(c) => info!(
            command: "SHOW INDEXES",
            yields: &c.yields,
            where_expr: c.where_expr.as_ref(),
            stages: &c.stages,
            prefill: false,
            validate_columns: true,
            composable: true,
            graph_schema_catalog: true,
            defaults: DEFAULT_INDEX_COLUMNS,
            full: full_index_columns(),
        ),
        Clause::ShowDatabases(c) => info!(
            command: "SHOW DATABASES",
            yields: &c.yields,
            where_expr: c.where_expr.as_ref(),
            stages: &[],
            prefill: false,
            validate_columns: true,
            composable: true,
            graph_schema_catalog: false,
            defaults: DEFAULT_DATABASE_COLUMNS,
            full: full_database_columns(),
        ),
        Clause::ShowRoles(c) => {
            let defaults: &'static [&'static str] = if c.with_users {
                &["role", "member"]
            } else {
                &["role"]
            };
            info!(
                command: "SHOW ROLES",
                yields: &c.yields,
                where_expr: c.where_expr.as_ref(),
                stages: &[],
                prefill: false,
                validate_columns: false,
                composable: true,
                graph_schema_catalog: false,
                defaults: defaults,
                full: defaults.iter().map(|s| s.to_string()).collect(),
            )
        }
        Clause::ShowServers(c) => info!(
            command: "SHOW SERVERS",
            yields: &c.yields,
            where_expr: c.where_expr.as_ref(),
            stages: &[],
            prefill: false,
            validate_columns: false,
            composable: true,
            graph_schema_catalog: false,
            defaults: &["name"],
            full: vec!["name".to_string()],
        ),
        Clause::ShowUsers(c) => info!(
            command: "SHOW USERS",
            yields: &c.yields,
            where_expr: c.where_expr.as_ref(),
            stages: &[],
            prefill: false,
            validate_columns: false,
            composable: true,
            graph_schema_catalog: false,
            defaults: &["user"],
            full: vec!["user".to_string()],
        ),
        Clause::ShowProcedures(c) => info!(
            command: "SHOW PROCEDURES",
            yields: &c.yields,
            where_expr: c.where_expr.as_ref(),
            stages: &[],
            prefill: true,
            validate_columns: true,
            composable: true,
            graph_schema_catalog: false,
            defaults: DEFAULT_PROCEDURE_COLUMNS,
            full: full_procedure_columns(),
        ),
        Clause::ShowSettings(c) => info!(
            command: "SHOW SETTINGS",
            yields: &c.yields,
            where_expr: c.where_expr.as_ref(),
            stages: &[],
            prefill: true,
            validate_columns: true,
            composable: true,
            graph_schema_catalog: false,
            defaults: DEFAULT_SETTING_COLUMNS,
            full: full_setting_columns(),
        ),
        Clause::ShowTransactions(c) => info!(
            command: "SHOW TRANSACTIONS",
            yields: &c.yields,
            where_expr: c.where_expr.as_ref(),
            stages: &[],
            prefill: false,
            validate_columns: false,
            composable: true,
            graph_schema_catalog: false,
            defaults: DEFAULT_TRANSACTION_COLUMNS,
            full: DEFAULT_TRANSACTION_COLUMNS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        Clause::TerminateTransactions(c) => info!(
            command: "TERMINATE TRANSACTIONS",
            yields: &c.yields,
            where_expr: None,
            stages: &[],
            prefill: false,
            validate_columns: false,
            composable: true,
            graph_schema_catalog: false,
            defaults: TERMINATE_TRANSACTION_COLUMNS,
            full: TERMINATE_TRANSACTION_COLUMNS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        Clause::ShowGraphType(c) => info!(
            command: "SHOW GRAPH TYPE",
            yields: &c.yields,
            where_expr: None,
            stages: &[],
            prefill: false,
            validate_columns: false,
            composable: false,
            graph_schema_catalog: false,
            defaults: GRAPH_TYPE_COLUMNS,
            full: GRAPH_TYPE_COLUMNS.iter().map(|s| s.to_string()).collect(),
        ),
        Clause::ShowCurrentGraphType(c) => {
            let columns = current_graph_type_columns(c.as_graph.is_some());
            info!(
                command: "SHOW CURRENT GRAPH TYPE",
                yields: &c.yields,
                where_expr: c.where_expr.as_ref(),
                stages: &c.stages,
                prefill: false,
                validate_columns: true,
                composable: false,
                graph_schema_catalog: false,
                defaults: columns,
                full: columns.iter().map(|s| s.to_string()).collect(),
            )
        }
        Clause::CreateIndex(_)
        | Clause::DropIndex(_)
        | Clause::CreateConstraint(_)
        | Clause::DropConstraint(_)
        | Clause::AlterDatabase(_)
        | Clause::CreateGraphType(_)
        | Clause::AlterCurrentGraphType(_)
        | Clause::Match(_)
        | Clause::Create(_)
        | Clause::Merge(_)
        | Clause::Delete(_)
        | Clause::Set(_)
        | Clause::Remove(_)
        | Clause::Return(_)
        | Clause::With(_)
        | Clause::Unwind(_)
        | Clause::Foreach(_)
        | Clause::LoadCSV(_)
        | Clause::CallSubquery(_)
        | Clause::CallProcedure(_)
        | Clause::Use(_)
        | Clause::Finish
        | Clause::StandaloneOrderBy(_)
        | Clause::Conditional(_) => None,
    }
}
/// Default (bare, no-`YIELD`) columns of a catalog command, in result
/// column order. Empty for non-catalog clauses.
pub fn default_catalog_columns(clause: &Clause) -> &'static [&'static str] {
    catalog_info(clause).map_or(&[], |info| info.default_columns)
}

/// Full (`YIELD *`) column set of a catalog command. Empty for non-catalog
/// clauses.
pub fn full_catalog_columns(clause: &Clause) -> Vec<String> {
    catalog_info(clause).map_or_else(Vec::new, |info| info.full_columns)
}

/// Whether a catalog command's statement result is its projected catalog
/// rows. The SHOW family and `TERMINATE TRANSACTIONS` produce rows; schema
/// and administration DDL updates the catalog and finishes with an empty
/// result. Derived from the one exhaustive [`catalog_info`] classification.
pub fn catalog_produces_rows(command: &Clause) -> bool {
    catalog_info(command).is_some()
}

/// The one `AS GRAPH`-sensitive column-table selection: the fixed
/// `specification` column, or the `nodes`/`relationships` pair when the
/// clause was written `AS GRAPH` (CIP-184/CIP-257).
fn current_graph_type_columns(as_graph: bool) -> &'static [&'static str] {
    if as_graph {
        CURRENT_GRAPH_TYPE_AS_GRAPH_COLUMNS
    } else {
        CURRENT_GRAPH_TYPE_COLUMNS
    }
}

/// Effective output column names of a catalog clause: `YIELD *` → the full
/// column set, an explicit YIELD list → the alias-resolved names, bare →
/// the default columns. Empty for non-catalog clauses. This is the single
/// shape the planner binds against.
pub fn catalog_output_columns(clause: &Clause) -> Vec<String> {
    let Some(info) = catalog_info(clause) else {
        return Vec::new();
    };
    match info.projection() {
        CatalogProjection::Star => info.full_columns,
        CatalogProjection::Columns(pairs) => pairs.into_iter().map(|(output, _)| output).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clause(src: &str) -> Clause {
        let stmt = cypherlite_parser::parse(src).expect("parse");
        assert_eq!(stmt.clauses.len(), 1);
        stmt.clauses.into_iter().next().unwrap()
    }

    #[test]
    fn index_columns_match_neo4j_default_full_split() {
        // Bare SHOW INDEXES exposes the server columns id/state/
        // populationPercent; options/createStatement are yield-only.
        let bare = clause("SHOW INDEXES");
        assert_eq!(
            catalog_output_columns(&bare),
            DEFAULT_INDEX_COLUMNS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        assert!(DEFAULT_INDEX_COLUMNS.contains(&"state"));
        assert!(!DEFAULT_INDEX_COLUMNS.contains(&"options"));
        let full = full_catalog_columns(&bare);
        for col in [
            "options",
            "createStatement",
            "trackedSince",
            "failureMessage",
        ] {
            assert!(full.iter().any(|c| c == col), "full indexes miss {col}");
        }
    }

    #[test]
    fn constraint_columns_include_id_by_default() {
        let bare = clause("SHOW CONSTRAINTS");
        assert!(default_catalog_columns(&bare).contains(&"id"));
        let full = full_catalog_columns(&bare);
        for col in ["classification", "options", "createStatement"] {
            assert!(full.iter().any(|c| c == col), "full constraints miss {col}");
            assert!(
                !default_catalog_columns(&bare).contains(&col),
                "{col} must be yield-only"
            );
        }
    }

    #[test]
    fn explicit_yield_and_star_shapes() {
        let star = clause("SHOW INDEXES YIELD *");
        assert_eq!(catalog_output_columns(&star), full_catalog_columns(&star));
        let explicit = clause("SHOW INDEXES YIELD name AS n, type");
        assert_eq!(catalog_output_columns(&explicit), vec!["n", "type"]);
    }

    /// Every catalog command `execute_catalog` handles — the 20 parser
    /// clause kinds `lower::clause` wraps into `Clause::Catalog` — must
    /// classify the same way under both tables: row-producing exactly when
    /// the default-column table is non-empty. Today the two agree; a
    /// future zero-column SHOW breaks this test and forces an explicit
    /// choice in `catalog_produces_rows`.
    #[test]
    fn produces_rows_agrees_with_default_columns() {
        for src in [
            "SHOW FUNCTIONS",
            "SHOW CONSTRAINTS",
            "SHOW INDEXES",
            "SHOW DATABASES",
            "SHOW ROLES",
            "SHOW SERVERS",
            "SHOW USERS",
            "SHOW PROCEDURES",
            "SHOW SETTINGS",
            "SHOW TRANSACTIONS",
            "TERMINATE TRANSACTIONS 'tx-1'",
            "SHOW GRAPH TYPE social",
            "SHOW CURRENT GRAPH TYPE",
            "CREATE INDEX i FOR (n:N) ON (n.p)",
            "DROP INDEX i",
            "CREATE CONSTRAINT c FOR (n:N) REQUIRE n.p IS UNIQUE",
            "DROP CONSTRAINT c",
            "ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5",
            "CREATE GRAPH TYPE social AS { (:Person => {name :: STRING}) }",
            "ALTER CURRENT GRAPH TYPE SET { (:Private => {v :: STRING}) }",
        ] {
            let command = clause(src);
            assert_eq!(
                catalog_produces_rows(&command),
                !default_catalog_columns(&command).is_empty(),
                "{src}"
            );
        }
    }

    #[test]
    fn full_set_is_defaults_plus_yield_only() {
        for (defaults, full) in [
            (DEFAULT_FUNCTION_COLUMNS, full_function_columns()),
            (DEFAULT_PROCEDURE_COLUMNS, full_procedure_columns()),
            (DEFAULT_SETTING_COLUMNS, full_setting_columns()),
            (DEFAULT_INDEX_COLUMNS, full_index_columns()),
            (DEFAULT_CONSTRAINT_COLUMNS, full_constraint_columns()),
            (DEFAULT_DATABASE_COLUMNS, full_database_columns()),
        ] {
            for d in defaults {
                assert!(full.iter().any(|c| c == d), "full set lost default {d}");
            }
        }
    }
}
