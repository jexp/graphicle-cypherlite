//! The canonical built-in procedure table: the single source of every
//! procedure's signature and catalog metadata, kept under the file-size rule
//! by the compact constructors.

use super::ProcSignature;
use crate::catalog_meta::{DeprecatedBy, ProcArgMeta};

/// The identical return-column list `db.info` and `dbms.info` share
/// (ADR-0142): the deliberately common `id`/`name`/`creationDate` output
/// shape, expressed once so the two entries cannot drift apart.
const DB_INFO_RETURNS: &[ProcArgMeta] = &[
    ProcArgMeta::new("id", "STRING"),
    ProcArgMeta::new("name", "STRING"),
    ProcArgMeta::new("creationDate", "STRING"),
];

/// The identical 8-column `ConfigResult` shape `dbms.clientConfig` and
/// `dbms.listConfig` share (ADR-0143): the Neo4j 5.x+ documented output
/// shape, expressed once so the two entries cannot drift apart.
const CONFIG_RESULT_RETURNS: &[ProcArgMeta] = &[
    ProcArgMeta::new("name", "STRING"),
    ProcArgMeta::new("description", "STRING"),
    ProcArgMeta::new("value", "STRING"),
    ProcArgMeta::new("dynamic", "BOOLEAN"),
    ProcArgMeta::new("defaultValue", "STRING"),
    ProcArgMeta::new("startupValue", "STRING"),
    ProcArgMeta::new("explicitlySet", "BOOLEAN"),
    ProcArgMeta::new("validValues", "STRING"),
];

/// The similarity-score column both vector-index query procedures return.
const VECTOR_QUERY_SCORE: ProcArgMeta =
    ProcArgMeta::described("score", "The similarity score.", "FLOAT");

/// The argument list both vector-index query procedures share.
const VECTOR_QUERY_ARGS: &[ProcArgMeta] = &[
    ProcArgMeta::described("indexName", "The name of the vector index.", "STRING"),
    ProcArgMeta::described(
        "numberOfNearestNeighbours",
        "The number of nearest neighbors to return.",
        "INTEGER",
    ),
    ProcArgMeta::described("query", "The query vector.", "LIST<FLOAT>"),
];

/// The routing-table argument list `dbms.routing.getRoutingTable` and its
/// cluster-prefixed predecessor share (ADR-0143), so the alias and its
/// replacement cannot drift apart.
const ROUTING_TABLE_ARGS: &[ProcArgMeta] = &[
    ProcArgMeta::described("context", "Client-provided routing context.", "MAP"),
    ProcArgMeta::with_default(
        "database",
        "The database to get a routing table for.",
        "STRING",
        "null",
    ),
];

/// The routing-table return columns the two `getRoutingTable` entries share.
const ROUTING_TABLE_RETURNS: &[ProcArgMeta] = &[
    ProcArgMeta::described(
        "ttl",
        "The number of seconds this routing table is valid for.",
        "INTEGER",
    ),
    ProcArgMeta::described(
        "servers",
        "The servers making up the routing table, grouped by their role.",
        "LIST<MAP>",
    ),
];

/// The routing-table description the two `getRoutingTable` entries share.
const ROUTING_TABLE_DESCRIPTION: &str =
    "Returns the advertised bolt addresses of all servers in the cluster, grouped by their role.";

/// The change-identifier description `cdc.current` and `db.cdc.current`
/// share.
const CDC_CURRENT_DESCRIPTION: &str = "Get the current change identifier.";

/// The single-column change-identifier return both cdc-current entries
/// share.
const CDC_CURRENT_RETURNS: &[ProcArgMeta] = &[ProcArgMeta::described(
    "id",
    "The current change identifier.",
    "STRING",
)];

/// The `timeOutSeconds` argument `db.awaitIndex` and `db.awaitIndexes`
/// share.
const AWAIT_INDEX_TIMEOUT: ProcArgMeta = ProcArgMeta::with_default(
    "timeOutSeconds",
    "The maximum time to wait, in seconds.",
    "INTEGER",
    "300",
);

/// The canonical built-in procedure table, in `SHOW PROCEDURES` row order
/// (`listed` entries) followed by the callable-but-unlisted `tx.*`/
/// `dbms.*` entries.
pub static BUILTIN_PROCEDURE_SIGNATURES: &[ProcSignature] = &[
    ProcSignature::read("db.info")
        .works_on_system()
        .returns(DB_INFO_RETURNS),
    ProcSignature::read("db.ping")
        .works_on_system()
        .described("Answers one row with success: true — the connectivity probe Neo4j Browser issues.")
        .returns(&[ProcArgMeta::described("success", "Whether the database answered.", "BOOLEAN")]),
    ProcSignature::read("db.labels")
        .works_on_system()
        .described("List all labels attached to nodes within a database according to the user's access rights. The procedure returns empty results if the user is not authorized to view those labels.")
        .returns(&[ProcArgMeta::described("label", "A label within the database.", "STRING")]),
    ProcSignature::read("db.relationshipTypes")
        .works_on_system()
        .returns(&[ProcArgMeta::new("relationshipType", "STRING")]),
    ProcSignature::read("db.propertyKeys")
        .works_on_system()
        .returns(&[ProcArgMeta::new("propertyKey", "STRING")]),
    ProcSignature::read("db.indexes")
        .works_on_system(),
    ProcSignature::read("db.constraints")
        .works_on_system(),
    ProcSignature::read("db.schema.nodeTypeProperties")
        .works_on_system()
        .described("Show the derived property schema of the nodes in tabular form.")
        .returns(&[
            ProcArgMeta::described("nodeType", "The node labels rendered as colon-prefixed, sorted labels.", "STRING"),
            ProcArgMeta::described("nodeLabels", "The sorted labels attached to the node type.", "LIST<STRING>"),
            ProcArgMeta::described("propertyName", "A property key found on the node type.", "STRING"),
            ProcArgMeta::described("propertyTypes", "The distinct value types observed for the property key.", "LIST<STRING>"),
            ProcArgMeta::described("mandatory", "Whether the property key is present on every node of the type.", "BOOLEAN"),
        ]),
    ProcSignature::read("db.schema.relTypeProperties")
        .works_on_system()
        .described("Show the derived property schema of the relationships in tabular form.")
        .returns(&[
            ProcArgMeta::described("relType", "The relationship type rendered in bracket form.", "STRING"),
            ProcArgMeta::described("propertyName", "A property key found on the relationship type.", "STRING"),
            ProcArgMeta::described("propertyTypes", "The distinct value types observed for the property key.", "LIST<STRING>"),
            ProcArgMeta::described("mandatory", "Whether the property key is present on every relationship of the type.", "BOOLEAN"),
        ]),
    // LABS-150: Neo4j's schema graph for Browser/Bloom-style sidebars,
    // inferred by scanning stored entities (ADR-0112's rule). Listed by
    // `SHOW PROCEDURES` exactly like its `db.schema.*` siblings.
    // LABS-389 (CIP-277 compat smoke): the call shape with an options
    // map is accepted, not failed -- the visualization's own output is
    // parameter-free (the stored entities are the whole truth,
    // ADR-0112), so the map is accepted and ignored. Neo4j parity
    // keeps the zero-arg call the documented form.
    ProcSignature::read("db.schema.visualization")
        .works_on_system()
        .described("Visualizes the schema of the data based on available statistics.")
        .args(&[ProcArgMeta::with_default("options", "Accepted for call-shape compatibility; ignored.", "MAP", "{}")])
        .returns(&[
            ProcArgMeta::described("nodes", "One virtual node per stored label.", "LIST<NODE>"),
            ProcArgMeta::described("relationships", "One virtual relationship per distinct (startLabel, relType, endLabel) triple.", "LIST<RELATIONSHIP>"),
        ]),
    ProcSignature::read("db.createLabel")
        .works_on_system()
        .writes()
        .args(&[ProcArgMeta::new("newLabel", "STRING")]),
    // ADR-0121 as amended by ADR-0227 (CIP-287): the ontology procedures.
    // No database argument — each acts on the session/USE-target database,
    // which must be `neo4j`. `set`/`drop` are store-level ops (facade
    // gates them to implicit transactions and forbids mixed writes, ADR-0121
    // §3) whose redo the executor pushes; `apply` is an ordinary in-overlay
    // write; `get` is a read. `writes: true` on set/drop/apply routes their
    // statements to the write path (the facade's `write_route`, ADR-0121);
    // `store_level: true` on set/drop alone is the store-op classification
    // the implicit-only / no-mixed-writes gates read (ADR-0120 §3);
    // statements routed to `system_ontologies` fail ADR-0229 §1's gate
    // before `writes` could matter there.
    ProcSignature::write("db.ontology.set")
        .described("Validate and store the ontology document (JSON or YAML) of the database, replacing any current one.")
        .writes()
        .store_level()
        .args(&[ProcArgMeta::described("ontology", "The ontology document, in JSON or YAML.", "STRING")]),
    ProcSignature::read("db.ontology.get")
        .described("Return the stored ontology document in the requested format, or null when none is set.")
        .args(&[ProcArgMeta::with_default("format", "The format to return the ontology in: 'yaml' (default) or 'json'.", "STRING", "\"yaml\"")])
        .returns(&[ProcArgMeta::described("ontology", "The stored ontology document in the requested format.", "STRING")]),
    ProcSignature::write("db.ontology.drop")
        .described("Drop the stored ontology document. Succeeds when none is set.")
        .writes()
        .store_level(),
    ProcSignature::write("db.ontology.apply")
        .described("Convert the stored ontology to a graph type and apply it to the database with ALTER CURRENT GRAPH TYPE SET semantics, failing when existing data violates it.")
        .writes(),
    // CIP-123 boundaries[5]: Neo4j 2026.05 still lists the pre-CIP-79
    // singular `db.awaitIndex` alongside the current plural
    // `db.awaitIndexes` (see the CIP-123 doc's own worked "Combining all
    // commands" example table, which lists both as distinct real
    // procedures) -- kept here purely as a catalog entry, like every other
    // `SHOW PROCEDURES`-only name in this list.
    ProcSignature::read("db.awaitIndex")
        .described("Wait for an index to come online (up to the given timeout in seconds).")
        .args(&[
            ProcArgMeta::described("indexName", "The name of the index to await.", "STRING"),
            AWAIT_INDEX_TIMEOUT,
        ])
        .executable(false),
    ProcSignature::read("db.awaitIndexes")
        .described("Wait for all (or some) indexes to come online (up to the given timeout in seconds).")
        .args(&[AWAIT_INDEX_TIMEOUT]),
    ProcSignature::read("db.resampleIndex")
        .described("Schedule resampling of an index.")
        .args(&[ProcArgMeta::described("indexName", "The name of the index to resample.", "STRING")])
        .executable(false),
    ProcSignature::read("db.index.fulltext.queryNodes")
        .works_on_system()
        .args(&[
            ProcArgMeta::new("indexName", "STRING"),
            ProcArgMeta::new("query", "STRING"),
        ])
        .returns(&[
            ProcArgMeta::new("node", "NODE"),
            ProcArgMeta::new("score", "FLOAT"),
        ]),
    // CIP-84/CIP-157: deprecated in favor of the CIP-224 `SEARCH` clause.
    ProcSignature::read("db.index.vector.queryNodes")
        .described("Query the given vector index, returning the requested number of approximate nearest neighbor nodes to the given query vector.")
        .args(VECTOR_QUERY_ARGS)
        .returns(&[
            ProcArgMeta::described("node", "A matched node.", "NODE"),
            VECTOR_QUERY_SCORE,
        ])
        .deprecated_by(DeprecatedBy::clause("SEARCH")),
    ProcSignature::read("db.index.vector.queryRelationships")
        .described("Query the given vector index, returning the requested number of approximate nearest neighbor relationships to the given query vector.")
        .args(VECTOR_QUERY_ARGS)
        .returns(&[
            ProcArgMeta::described("relationship", "A matched relationship.", "RELATIONSHIP"),
            VECTOR_QUERY_SCORE,
        ])
        .deprecated_by(DeprecatedBy::clause("SEARCH")),
    ProcSignature::dbms("dbms.setConfigValue")
        .described("Update a given setting value. Set value to null to reset the value to its default.")
        .admin()
        .writes()
        .args(&[
            ProcArgMeta::described("setting", "The name of the setting to update.", "STRING"),
            ProcArgMeta::described("value", "The new value of the setting.", "STRING"),
        ])
        .executable(false),
    // CIP-157: deprecated in favor of the `db.cdc.*` namespace.
    ProcSignature::read("cdc.current")
        .described(CDC_CURRENT_DESCRIPTION)
        .returns(CDC_CURRENT_RETURNS)
        .deprecated_by(DeprecatedBy::identifier("db.cdc.current"))
        .executable(false),
    ProcSignature::read("cdc.earliest")
        .described("Get the earliest available change identifier.")
        .returns(&[ProcArgMeta::described("id", "The earliest available change identifier.", "STRING")])
        .deprecated_by(DeprecatedBy::identifier("db.cdc.earliest"))
        .executable(false),
    ProcSignature::read("cdc.query")
        .described("Query change data for the given change identifier and selectors.")
        .args(&[
            ProcArgMeta::described("from", "The change identifier to query from.", "STRING"),
            ProcArgMeta::with_default("selectors", "Selectors narrowing which changes to return.", "LIST<MAP>", "[]"),
        ])
        .returns(&[
            ProcArgMeta::described("id", "The change identifier of this event.", "STRING"),
            ProcArgMeta::described("txId", "The id of the transaction that produced this event.", "INTEGER"),
            ProcArgMeta::described("seq", "The sequence number of this event within its transaction.", "INTEGER"),
            ProcArgMeta::described("metadata", "Metadata about the transaction that produced this event.", "MAP"),
            ProcArgMeta::described("event", "The change event itself.", "MAP"),
        ])
        .deprecated_by(DeprecatedBy::identifier("db.cdc.query"))
        .executable(false),
    ProcSignature::dbms("dbms.cluster.recreateDatabase")
        .described("Re-seeds the specified database.")
        .admin()
        .args(&[ProcArgMeta::described("databaseName", "The name of the database to re-seed.", "STRING")])
        .deprecated_by(DeprecatedBy::identifier("dbms.recreateDatabase"))
        .executable(false),
    ProcSignature::dbms("dbms.cluster.routing.getRoutingTable")
        .described(ROUTING_TABLE_DESCRIPTION)
        .args(ROUTING_TABLE_ARGS)
        .returns(ROUTING_TABLE_RETURNS)
        .deprecated_by(DeprecatedBy::identifier("dbms.routing.getRoutingTable"))
        .executable(false),
    // CIP-113 procedure-signatures.feature [1]: the doc's own literal
    // worked example.
    ProcSignature::read("db.stats.collect")
        .described("Retrieve statistical data about the current database.")
        .args(&[
            ProcArgMeta::described("section", "The section of statistics to collect.", "STRING"),
            ProcArgMeta::with_default("config", "Configuration for the collection.", "MAP", "{}"),
        ])
        .returns(&[
            ProcArgMeta::described("section", "The section this row of statistics belongs to.", "STRING"),
            ProcArgMeta::described("success", "Whether collection for this section succeeded.", "BOOLEAN"),
            ProcArgMeta::described("message", "A human-readable status message.", "STRING"),
        ])
        .executable(false),
    // CIP-113 procedure-signatures.feature [2]/[9]: the doc's own
    // literal worked example (an optional parameter defaulting to `null`).
    ProcSignature::dbms("dbms.routing.getRoutingTable")
        .described(ROUTING_TABLE_DESCRIPTION)
        .args(ROUTING_TABLE_ARGS)
        .returns(ROUTING_TABLE_RETURNS)
        .executable(false),
    // CIP-113 procedure-signatures.feature [5]: no input parameters,
    // one return column.
    ProcSignature::read("db.cdc.current")
        .described(CDC_CURRENT_DESCRIPTION)
        .returns(CDC_CURRENT_RETURNS)
        .executable(false),
    // CIP-113 procedure-signatures.feature [6]/[16]: no input
    // parameters, no return columns -- the reachable `::VOID`-
    // replacement (no suffix at all) case.
    ProcSignature::read("db.index.fulltext.awaitEventuallyConsistentIndexRefresh")
        .described("Wait for the updates from recently committed transactions to be applied to any eventually-consistent full-text indexes.")
        .executable(false),
    // CIP-113 procedure-signatures.feature [11]: a `LIST<STRING>` return
    // column (the old `LIST? OF STRING` -> `LIST<STRING>` rename).
    ProcSignature::read("db.index.fulltext.listAvailableAnalyzers")
        .works_on_system()
        .described("List the available analyzers that the full-text indexes can be configured with.")
        .returns(&[
            ProcArgMeta::described("analyzer", "The name of the analyzer.", "STRING"),
            ProcArgMeta::described("description", "A description of the analyzer.", "STRING"),
            ProcArgMeta::described("stopwords", "The stopwords used by the analyzer.", "LIST<STRING>"),
        ])
        .executable(false),
    // CIP-113 procedure-signatures.feature [13]: multiple return columns
    // using the same `name :: type` syntax as parameters.
    ProcSignature::read("db.checkpoint")
        .works_on_system()
        .described("Initiate and wait for a new checkpoint.")
        .returns(&[
            ProcArgMeta::described("success", "Whether the checkpoint succeeded.", "BOOLEAN"),
            ProcArgMeta::described("message", "A human-readable status message.", "STRING"),
        ])
        .executable(false),
    // Callable but not listed by `SHOW PROCEDURES`.
    ProcSignature::dbms("tx.setMetaData")
        .args(&[ProcArgMeta::new("metadata", "MAP")])
        .listed(false),
    ProcSignature::read("tx.getMetaData")
        .works_on_system()
        .returns(&[ProcArgMeta::new("metadata", "MAP")])
        .listed(false),
    ProcSignature::dbms("dbms.components")
        .returns(&[
            ProcArgMeta::new("name", "STRING"),
            ProcArgMeta::new("versions", "LIST<STRING>"),
            // ADR-0142: Browser-compat column; CypherLite has no enterprise
            // tier, so every row reports `community`.
            ProcArgMeta::new("edition", "STRING"),
        ])
        .listed(false),
    // ADR-0142: Neo4j Desktop calls `dbms.info()` on connect
    // (`updateDBMSInfo`); callable but not listed by `SHOW PROCEDURES`.
    // Shares `db.info`'s return columns (`DB_INFO_RETURNS`) by design.
    ProcSignature::dbms("dbms.info")
        .returns(DB_INFO_RETURNS)
        .listed(false),
    // ADR-0143: Browser/Desktop-compat procedures (LABS-148); callable but
    // not listed by `SHOW PROCEDURES`. Fixed documented-placeholder rows —
    // the engine carries no session identity.
    ProcSignature::dbms("dbms.showCurrentUser")
        .returns(&[
            ProcArgMeta::new("username", "STRING"),
            ProcArgMeta::new("roles", "LIST<STRING>"),
            ProcArgMeta::new("flags", "LIST<STRING>"),
        ])
        .listed(false),
    // ADR-0143: `dbms.clientConfig` and `dbms.listConfig` share the Neo4j
    // 5.x+ `ConfigResult` shape (`CONFIG_RESULT_RETURNS`) by design.
    ProcSignature::dbms("dbms.clientConfig")
        .returns(CONFIG_RESULT_RETURNS)
        .listed(false),
    ProcSignature::dbms("dbms.listConfig")
        .returns(CONFIG_RESULT_RETURNS)
        .listed(false),
    ProcSignature::dbms("dbms.licenseAgreementDetails")
        .returns(&[
            ProcArgMeta::new("status", "STRING"),
            ProcArgMeta::new("daysLeftOnTrial", "INTEGER"),
            ProcArgMeta::new("totalTrialDays", "INTEGER"),
        ])
        .listed(false),
];
