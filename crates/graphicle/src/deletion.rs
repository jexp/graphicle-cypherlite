//! Verified deletion ("certified forgetting") of connector documents, whole
//! connectors and projects — on the CypherLite graph store.
//!
//! Forgetting removes the originals and every artifact that could carry their
//! information, verifies the result in the same transaction, and stores a
//! hash-chained receipt:
//! - artifacts computed from one source only (observer notes, passage
//!   vectors, single-source extraction drafts) go with that source and stay
//!   otherwise, because their input never contained the forgotten text;
//! - artifacts whose input could have included the forgotten documents (model
//!   caches, served-evidence records, outcomes and hints, reports and
//!   communities, overlay anchors, job history, model drafts built from
//!   retrieval) are removed from the moment those documents first became
//!   visible, and are recomputed on demand without them;
//! - dependency-index entries follow the artifacts they describe.
//!
//! The intended result equals a store that never ingested the documents.
//! [`state`] digests a store so the claim can be checked: fork a store before
//! the documents arrive, forget them in one copy, rerun the same work in both,
//! and compare (tests/it/forgetting.rs, tools/certify_forgetting.py). The state
//! contract is `graphicle.state.v2`: labels with no nodes in the project's
//! scope are left out, so a store that has an edition's labels and one that
//! never had them digest equally when their content is equal; history is the
//! core's history labels plus those the installed extensions declare and, on
//! a store that has them, the commercial edition's team history labels
//! ([`crate::store_compat`]), so one store digests the same whichever edition
//! reads it.
//!
//! Residue is reported, never hidden: records people wrote are not silently
//! destroyed, and the receipt is then not `verified_local`. Receipts outlive
//! projects and never store deleted identifiers, titles or text; documents are
//! named only by one-way hashes, which also stop a later sync re-importing
//! them. Backups taken earlier, provider-side retention of past model requests
//! and copies exported earlier are outside any receipt.
//!
//! Storage model (migration plan §2.5, `docs/store-api.md`): every removal is
//! a label-scoped walk — `MATCH (n:Label) WHERE <scope> DETACH DELETE n` —
//! over the label catalog ([`LABELS`], the walk list); a label the catalog
//! does not declare is still discovered and scanned, because residue is
//! reported, never hidden. Receipts are the
//! `(:DeletionReceipt)-[:NEXT]->(:DeletionReceipt)` chain of §2.5: plain
//! properties, no project reference (they outlive their project), sequence
//! numbers taken from the workspace's receipt chain itself, each receipt
//! hash-chained to its predecessor's digest. JSON-object payloads follow the
//! engine's rules (v0.12.2): maps cannot be property values, so objects with
//! known keys are flattened to dotted properties (`n.`data.foo``) and
//! genuinely dynamic-keyed objects stay stringified JSON in a String
//! property, parsed at the boundary.
use crate::store::{Principal, Store, decode, repo, s};
use crate::maintenance::graph_store;
use crate::store_graph::{self};
#[cfg(test)]
use crate::store_graph::GraphStore;
use crate::{Error, Result, digest, id, now, string};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The graph connection every operation in this module runs on (the
/// migration seam: callers hold the transaction, this module runs its
/// statements inside it).
type Graph<'a> = &'a mut cypherlite::Connection;


/// The SQLite implementation, kept while sibling modules still write SQL:
/// their callers run it, and the `Store`-facing entry points fall back to it
/// when the receipt or content lives there. Deleted once the store holds no
/// deletion-relevant data in SQLite.
#[doc(hidden)] // transition-only; not part of the public API contract
#[allow(dead_code)] // some entries serve only callers that migrate later
pub mod legacy;

pub const CONTRACT: &str = "graphicle.deletion.v1";
pub const STATE_CONTRACT: &str = "graphicle.state.v2";
/// The receipt chain's label; receipts are never residue of anything.
const RECEIPTS: &str = "DeletionReceipt";
/// One-way tombstones of forgotten documents: block re-import without keeping
/// identifiers.
const TOMBSTONES: &str = "ForgottenSource";
/// Whether a row's key or text belongs to the scope being forgotten.
type Matches = fn(&str, &Scope) -> bool;
const FINGERPRINTS: usize = 16;
const FINGERPRINT_BYTES: usize = 48;
const MIN_FINGERPRINT_BYTES: usize = 16;
const MAX_DOCUMENTS: usize = 1000;
/// Other sources' originals: identical text there is an independent copy.
const INDEPENDENT: &[&str] = &["Object", "Chunk", "Parse", "Snapshot", "File"];
/// Kept by design when a project is deleted: audit trails, and the durable
/// external-cleanup queue that must name the scope until cleanup completes.
/// Installed extensions add theirs ([`retained`]).
const RETAINED: &[&str] = &["IdentityAudit", "ProjectionCleanup"];
const NOT_COVERED: &[&str] = &[
    "backups taken before the deletion",
    "model-provider retention of requests already sent",
    "copies exported from Graphicle before the deletion",
];
/// Records of work and access rather than memory. They are excluded from
/// state equality; residue scans still read them. Installed extensions add
/// theirs ([`history`]).
const HISTORY: &[&str] = &[
    "Job",
    "ModelCall",
    "Reservation",
    "Event",
    "Sync",
    "Run",
    "ToolEvent",
    "IdentityAudit",
    "ProjectionCleanup",
    TOMBSTONES,
    "OAuthPending",
];
/// Properties holding digests of capabilities minted per store (the owner's
/// and granted credentials'). Like minted identifiers, they never take part
/// in state equality.
const MINTED_DIGESTS: &[&str] = &["capability_hash"];

/// How a label's nodes are scoped to one project (or to the workspace) — the
/// graph counterpart of the SQL scoping columns. Writers denormalize
/// `repository_id` onto every project-scoped node.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scoped {
    /// `n.repository_id = $repo`.
    Repository,
    /// Some other property holds the project id (e.g. `ProjectionCleanup.scope`).
    RepositoryIn(&'static str),
    /// `n.workspace_id = $ws`.
    WorkspaceId,
    /// `n.workspace = $ws`.
    WorkspaceProp,
    /// `n.id = $repo` / `n.id = $ws` — the scope's own node.
    Own,
    /// Scoped through its connector (`n.connector_id IN $connectors`).
    Connector,
    /// Scoped through the memory it revises (`-[:REVISION_OF]->(:Memory)`).
    OfMemory,
    /// Cross-repository: either side names the project.
    Cross,
    /// Not scoped to a project or workspace: never part of a state digest;
    /// residue scans still read every node (residue is never hidden).
    Global,
}

/// The store's label catalog: the graph model's labels (migration plan §2 —
/// table names become labels), with each label's project scoping and the
/// legacy table name receipts keep using, so a `graphicle.deletion.v1`
/// receipt reads the same whichever backend wrote it. This catalog — not any
/// enumeration of the store — is the walk list for verified deletion;
/// [`discovered`] adds labels the catalog does not declare (another
/// edition's), because residue is reported, never hidden.
const LABELS: &[(&str, &str, Scoped)] = &[
    ("Alias", "gf_aliases", Scoped::Repository),
    ("AliasProposal", "am_alias_proposals", Scoped::Repository),
    ("AnchorEdge", "gf_anchor_edges", Scoped::Repository),
    ("Artifact", "gf_dependencies", Scoped::WorkspaceProp),
    ("Assertion", "gf_timeline", Scoped::Repository),
    ("Backend", "am_backends", Scoped::Global),
    ("BinaryHead", "am_binary_heads", Scoped::Repository),
    ("Claim", "am_claims", Scoped::Repository),
    ("Chunk", "repo_chunks", Scoped::Repository),
    ("ClaimEvent", "am_claim_events", Scoped::Repository),
    ("Commit", "gf_commits", Scoped::Repository),
    ("Community", "am_communities", Scoped::Repository),
    ("Connector", "am_connectors", Scoped::Repository),
    ("ConnectorDoc", "am_connector_docs", Scoped::Connector),
    ("ConnectorStage", "am_connector_stage", Scoped::Connector),
    ("ConnectorVersion", "am_connector_versions", Scoped::Connector),
    ("Contradiction", "gf_contradictions", Scoped::Repository),
    ("CoverageInventory", "gf_coverage_inventory", Scoped::Repository),
    ("CoverageUpload", "gf_coverage_uploads", Scoped::Repository),
    ("Credential", "credentials", Scoped::WorkspaceProp),
    ("CrossPackage", "gf_cross_packages", Scoped::Cross),
    ("Decision", "decisions", Scoped::WorkspaceId),
    ("DocumentRead", "am_document_reads", Scoped::Repository),
    ("EmbeddingCache", "am_embedding_cache", Scoped::Global),
    ("Entity", "am_entities", Scoped::Repository),
    ("Episode", "gf_episodes", Scoped::Repository),
    ("EpisodeLabel", "gf_episode_labels", Scoped::WorkspaceProp),
    ("Event", "repo_events", Scoped::Repository),
    ("EvidenceLink", "repo_evidence_links", Scoped::Repository),
    ("File", "repo_heads", Scoped::Repository),
    ("ForgottenSource", "am_forgotten_sources", Scoped::Connector),
    ("Folder", "repo_folders", Scoped::Repository),
    ("Group", "am_groups", Scoped::WorkspaceId),
    ("GroupMember", "am_group_members", Scoped::WorkspaceId),
    ("HeldOut", "gf_heldout", Scoped::Repository),
    ("Hint", "gf_hints", Scoped::Repository),
    ("Hyperedge", "gf_hyperedges", Scoped::Repository),
    ("IdentityAudit", "am_identity_audit", Scoped::WorkspaceId),
    ("Job", "am_jobs", Scoped::Repository),
    ("LanguageServer", "am_language_servers", Scoped::Global),
    ("Member", "am_members", Scoped::WorkspaceId),
    ("Memory", "repo_memories", Scoped::Repository),
    ("MemoryRevision", "repo_memory_revisions", Scoped::OfMemory),
    ("MentalModel", "am_observer_reflections", Scoped::Repository),
    ("ModelCache", "am_model_cache", Scoped::Repository),
    ("ModelCall", "am_model_calls", Scoped::Repository),
    ("ModelProfile", "am_model_profiles", Scoped::Global),
    ("Observation", "am_observer_notes", Scoped::Repository),
    ("OAuthAccount", "am_oauth_accounts", Scoped::WorkspaceId),
    ("OAuthPending", "am_oauth_pending", Scoped::WorkspaceId),
    ("Object", "repo_objects", Scoped::Repository),
    ("ObservationCheckpoint", "gf_doc_link_checkpoints", Scoped::Repository),
    ("Outcome", "gf_outcomes", Scoped::Repository),
    ("OverlayAliasProposal", "gf_alias_proposals", Scoped::Repository),
    ("OverlayCommunity", "gf_communities", Scoped::Repository),
    ("Parse", "repo_parses", Scoped::Repository),
    ("PreparationStep", "am_preparation_steps", Scoped::Repository),
    ("ProductPolicy", "am_product_policy", Scoped::WorkspaceId),
    ("ProjectionCleanup", "am_projection_cleanup", Scoped::RepositoryIn("scope")),
    ("ProjectionGroup", "gf_projection_groups", Scoped::Repository),
    ("ProviderEmail", "am_provider_emails", Scoped::WorkspaceId),
    ("ProviderIdentity", "am_provider_identities", Scoped::WorkspaceId),
    ("QueryCache", "am_query_cache", Scoped::Repository),
    ("ReleaseSeen", "am_release_seen", Scoped::Global),
    ("Report", "am_reports", Scoped::Repository),
    ("ReportTiming", "gf_report_timings", Scoped::Repository),
    ("RepoAccess", "gf_repo_access", Scoped::WorkspaceProp),
    ("Reservation", "am_call_reservations", Scoped::Repository),
    ("Review", "reviews", Scoped::WorkspaceId),
    ("Run", "repo_runs", Scoped::Repository),
    ("Secret", "am_secrets", Scoped::WorkspaceId),
    ("Served", "gf_served", Scoped::Repository),
    ("Snapshot", "repo_snapshots", Scoped::Repository),
    ("Source", "sources", Scoped::WorkspaceId),
    ("SourcePermission", "am_source_permissions", Scoped::Connector),
    ("Stale", "gf_stale", Scoped::WorkspaceProp),
    ("Symbol", "gf_nodes", Scoped::Repository),
    ("SymbolMove", "gf_symbol_moves", Scoped::Repository),
    ("SymbolVersion", "gf_nodes versions", Scoped::Repository),
    ("Sync", "repo_syncs", Scoped::Repository),
    ("TeamAudit", "am_team_audit", Scoped::WorkspaceId),
    ("Test", "gf_test_coverage", Scoped::Repository),
    ("ToolEvent", "repo_tool_events", Scoped::Repository),
    ("Usage", "am_usage", Scoped::WorkspaceId),
    ("WorkState", "am_work_state", Scoped::Repository),
    ("Workspace", "workspaces", Scoped::Own),
];

/// The legacy table name receipts report a label as.
fn table_of(label: &str) -> &str {
    LABELS
        .iter()
        .find(|(l, _, _)| *l == label)
        .map_or(label, |(_, t, _)| t)
}

/// How `label` is scoped (catalog-declared), `Global` when undeclared.
fn scoping(label: &str) -> Scoped {
    LABELS
        .iter()
        .find(|(l, _, _)| *l == label)
        .map_or(Scoped::Global, |(_, _, s)| *s)
}

/// The labels kept by design: the installed extensions' (for example a team
/// audit trail) and the commercial edition's team audit trail when this store
/// has it and no extension named it ([`crate::store_compat`]), then the
/// core's. One store therefore lists the same labels in every edition.
fn retained(db: Graph) -> Result<Vec<&'static str>> {
    let mut labels = crate::extension::installed().retained_tables();
    for label in crate::store_compat::RETAINED_TABLES {
        let label = known_label(label);
        if !labels.contains(&label) && present(db, label)? {
            labels.push(label);
        }
    }
    labels.extend_from_slice(RETAINED);
    Ok(labels)
}

/// The history labels: the installed extensions' (for example sign-in
/// attempts and browser sessions) and the commercial edition's team history
/// labels that this store has and no extension named
/// ([`crate::store_compat`]), then the core's.
fn history(db: Graph, sql: Option<&rusqlite::Connection>) -> Result<Vec<&'static str>> {
    // The team access history lives in the SQL store, so its presence is a
    // SQL question, not a graph one; a graph probe would silently drop it.
    // It is listed first, in the order the commercial edition declares it,
    // so one store reports the same lists whichever edition reads it.
    let mut labels: Vec<&'static str> = match sql {
        Some(sql) => crate::store_compat::HISTORY_TABLES
            .iter()
            .copied()
            .filter(|table| crate::store_compat::has(sql, table).unwrap_or(false))
            .collect(),
        None => Vec::new(),
    };
    for label in crate::extension::installed().history_tables() {
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    for label in crate::store_compat::HISTORY_TABLES {
        let label = known_label(label);
        if !labels.contains(&label) && present(db, label)? {
            labels.push(label);
        }
    }
    labels.extend_from_slice(HISTORY);
    Ok(labels)
}

/// A legacy table name as its label, when the catalog renames it.
fn known_label(name: &'static str) -> &'static str {
    LABELS
        .iter()
        .find(|(_, t, _)| *t == name)
        .map_or(name, |(l, _, _)| l)
}

/// Whether the store holds any node of `label` (presence probe; an empty
/// label says nothing either way, state contract v2).
fn present(db: Graph, label: &str) -> Result<bool> {
    Ok(one(
        db,
        "MATCH (n:$($label)) RETURN 1 AS x LIMIT 1",
        gparams([("label", json!(label))]),
    )?
    .is_some())
}

/// Caches whose entries depend on when invalidation happened, not only on the
/// sources. Excluded from state equality; forgetting removes every entry
/// computed while the documents were visible.
const CACHES: &[&str] = &["ModelCache", "QueryCache"];
/// Where a project's folders are on this machine: the folder
/// aliases a local sync records, with canonical paths. Location, not
/// content: excluded from state equality, so two stores that hold the same
/// content digest equally wherever their folders are. Residue scans still
/// read them, and they go with their project.
const LOCATIONS: &[&str] = &["Folder"];
/// Sync counters: they count imports, not content (label, property).
const COUNTERS: &[(&str, &str)] = &[
    ("Connector", "generation"),
    ("Connector", "cursor"),
    ("ConnectorDoc", "generation"),
    ("SourcePermission", "generation"),
];
/// Artifacts whose input may include any document visible when they were
/// computed, with the property that dates them (receipt name, label,
/// dating property). Nodes dated at or after the first moment the forgotten
/// documents were visible are removed.
const SINCE_VISIBLE: &[(&str, &str, &str)] = &[
    ("model_cache", "ModelCache", "created_at"),
    ("preparation_steps", "PreparationStep", "created_at"),
    ("reports", "Report", "created_at"),
    ("communities", "Community", "created_at"),
    ("jobs", "Job", "created_at"),
    ("events", "Event", "created_at"),
    ("alias_proposals", "AliasProposal", "created_at"),
    ("served", "Served", "created_at"),
    ("outcomes", "Outcome", "created_at"),
    ("hints", "Hint", "updated_at"),
    ("anchors", "AnchorEdge", "created_at"),
    ("overlay_communities", "OverlayCommunity", "created_at"),
    ("overlay_aliases", "OverlayAliasProposal", "created_at"),
];

/// What one forget request covers: every document of a connector, or named
/// documents of it. Source ids are `connector:<connector>:<document>`,
/// passage identities append `:<byte offset>`, and dependency-index ids of
/// derived artifacts prefix `<repository>:`.
#[derive(Clone, Debug)]
pub struct Scope {
    connector: String,
    documents: Option<BTreeSet<String>>,
}

impl Scope {
    pub fn connector(connector: &str) -> Self {
        Self {
            connector: connector.to_string(),
            documents: None,
        }
    }
    pub fn documents(connector: &str, documents: &Value) -> Result<Self> {
        let listed = documents
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= MAX_DOCUMENTS)
            .ok_or_else(|| Error::bad("document_ids must list 1–1,000 connector document IDs"))?;
        let mut set = BTreeSet::new();
        for value in listed {
            let document = value
                .as_str()
                .filter(|v| !v.is_empty() && v.len() <= 500)
                .ok_or_else(|| Error::bad("Each document ID must be 1–500 bytes"))?;
            set.insert(document.to_string());
        }
        Ok(Self {
            connector: connector.to_string(),
            documents: Some(set),
        })
    }
    /// Named documents of a connector chosen by the service itself (a
    /// retention plan); callers bound the set to [`MAX_DOCUMENTS`].
    pub fn document_set(connector: &str, documents: BTreeSet<String>) -> Self {
        Self {
            connector: connector.to_string(),
            documents: Some(documents),
        }
    }
    fn whole(&self) -> bool {
        self.documents.is_none()
    }
    fn prefix(&self) -> String {
        format!("connector:{}:", self.connector)
    }
    /// A bare connector document id.
    pub fn document(&self, id: &str) -> bool {
        self.documents.as_ref().is_none_or(|d| d.contains(id))
    }
    /// A source id `connector:<connector>:<document>`.
    pub fn source(&self, id: &str) -> bool {
        id.strip_prefix(&self.prefix())
            .is_some_and(|document| self.document(document))
    }
    /// A passage identity `<source id>:<byte offset>`.
    pub fn passage(&self, id: &str) -> bool {
        id.rsplit_once(':').is_some_and(|(head, tail)| {
            !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) && self.source(head)
        })
    }
    fn sources(&self) -> Vec<String> {
        self.documents
            .iter()
            .flatten()
            .map(|d| format!("{}{d}", self.prefix()))
            .collect()
    }
    fn hashes(&self) -> Vec<String> {
        self.documents
            .iter()
            .flatten()
            .map(|d| tombstone(&self.connector, d))
            .collect()
    }
    fn describe(&self) -> Value {
        json!({"kind":if self.whole() {"connector"} else {"documents"},"connector":self.connector,
            "documents":self.documents.as_ref().map(BTreeSet::len)})
    }
}

/// One-way name of a forgotten document: blocks re-import without keeping
/// its identifier.
fn tombstone(connector: &str, document: &str) -> String {
    digest(&json!({"connector":connector,"document":document}))
}

/// The one-way name receipts give a connector document.
pub fn document_hash(connector: &str, document: &str) -> String {
    tombstone(connector, document)
}

/// Most documents one forget request (and one receipt) names.
pub const MAX_SCOPE_DOCUMENTS: usize = MAX_DOCUMENTS;

/// Whether an incoming connector document was forgotten earlier.
/// Tombstones live in the graph store (the connector they name is there).
pub(crate) fn forgotten(
    db: &mut cypherlite::Connection,
    connector: &str,
    document: &str,
) -> Result<bool> {
    let session = crate::capture::document_session_marker(document)
        .map(|marker| tombstone(connector, &marker));
    let mut hashes = vec![tombstone(connector, document)];
    if let Some(session) = session {
        hashes.push(session);
    }
    Ok(crate::store_graph::one(
        db,
        "MATCH (f:ForgottenSource {connector_id: $connector}) WHERE f.id_hash IN $hashes RETURN f.id_hash AS id_hash LIMIT 1",
        crate::store_graph::Params::from([
            ("connector".into(), crate::store_graph::json_to_value(&json!(connector))),
            ("hashes".into(), crate::store_graph::json_to_value(&json!(hashes))),
        ]),
    )?
    .is_some())
}

/// Block a connector document id before it exists (the later turns of a
/// forgotten agent session) with the same one-way tombstone a forgotten
/// document leaves, so no identifier is kept.
pub(crate) fn block(
    db: &mut cypherlite::Connection,
    connector: &str,
    document: &str,
) -> Result<()> {
    crate::store_graph::write(
        db,
        "MERGE (f:ForgottenSource {connector_id: $connector, id_hash: $hash}) SET f.forgotten_at=$now",
        crate::store_graph::Params::from([
            ("connector".into(), crate::store_graph::json_to_value(&json!(connector))),
            ("hash".into(), crate::store_graph::json_to_value(&json!(tombstone(connector, document)))),
            ("now".into(), crate::store_graph::json_to_value(&json!(now()))),
        ]),
    )?;
    Ok(())
}

/// Query parameters for one statement, from JSON pairs.
fn gparams(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> store_graph::Params {
    pairs
        .into_iter()
        .map(|(k, v)| (k.to_string(), store_graph::json_to_value(&v)))
        .collect()
}

use store_graph::{one, rows};

/// The count a query returns (`RETURN count(…) AS n`).
fn counted(db: Graph, cypher: &str, params: store_graph::Params) -> Result<usize> {
    Ok(one(db, cypher, params)?
        .and_then(|r| r["n"].as_i64())
        .unwrap_or(0) as usize)
}

/// The graph's labels, including any the catalog does not declare (another
/// edition's): residue is reported, never hidden. Catalog labels come first,
/// in catalog order, then undeclared ones by name; the receipts label itself
/// is never part of a scan.
fn catalog(db: Graph) -> Result<Vec<String>> {
    let mut labels: Vec<String> = LABELS.iter().map(|(l, _, _)| l.to_string()).collect();
    for found in rows(
        db,
        "MATCH (n) UNWIND labels(n) AS label RETURN DISTINCT label",
        store_graph::Params::new(),
    )? {
        let label = s(&found, "label");
        if label != RECEIPTS && !labels.iter().any(|l| l == label) {
            labels.push(label.to_string());
        }
    }
    Ok(labels)
}

/// Ids of the project's connectors (the scoping path of
/// [`Scoped::Connector`] labels).
fn connectors_of(db: Graph, repository: &str) -> Result<Vec<String>> {
    Ok(rows(
        db,
        "MATCH (c:Connector {repository_id: $repo}) RETURN c.id AS id",
        gparams([("repo", json!(repository))]),
    )?
    .iter()
    .map(|r| s(r, "id").to_string())
    .collect())
}

/// The nodes of `label` in scope (project `repository`, workspace
/// `workspace`), as JSON property maps (`_id`/`_labels` included). `Global`
/// labels are scanned whole: residue is reported, never hidden.
fn nodes(db: Graph, label: &str, repository: &str, workspace: &str) -> Result<Vec<Value>> {
    let run = |db: Graph, pattern: &str, predicate: Option<String>, params: store_graph::Params| {
        let cypher = format!(
            "{pattern}{} RETURN n",
            predicate.map_or_else(String::new, |p| format!(" WHERE {p}"))
        );
        Ok(rows(db, &cypher, params)?
            .into_iter()
            .map(|r| r["n"].clone())
            .collect::<Vec<_>>())
    };
    let label_p = || ("label", json!(label));
    let repo_p = || ("repo", json!(repository));
    let ws_p = || ("ws", json!(workspace));
    match scoping(label) {
        Scoped::Repository => run(
            db,
            "MATCH (n:$($label))",
            Some("n.repository_id = $repo".into()),
            gparams([label_p(), repo_p()]),
        ),
        Scoped::RepositoryIn(property) => run(
            db,
            "MATCH (n:$($label))",
            Some(format!("n.`{property}` = $repo")),
            gparams([label_p(), repo_p()]),
        ),
        Scoped::WorkspaceId => run(
            db,
            "MATCH (n:$($label))",
            Some("n.workspace_id = $ws".into()),
            gparams([label_p(), ws_p()]),
        ),
        Scoped::WorkspaceProp => run(
            db,
            "MATCH (n:$($label))",
            Some("n.workspace = $ws".into()),
            gparams([label_p(), ws_p()]),
        ),
        Scoped::Own => run(
            db,
            "MATCH (n:$($label))",
            Some("n.id = $scope".into()),
            gparams([
                label_p(),
                (
                    "scope",
                    json!(if label == "Workspace" { workspace } else { repository }),
                ),
            ]),
        ),
        Scoped::Connector => {
            let connectors = connectors_of(db, repository)?;
            run(
                db,
                "MATCH (n:$($label))",
                Some("n.connector_id IN $connectors".into()),
                gparams([label_p(), ("connectors", json!(connectors))]),
            )
        }
        Scoped::OfMemory => run(
            db,
            "MATCH (n:$($label))-[:REVISION_OF]->(:Memory {repository_id: $repo})",
            None,
            gparams([label_p(), repo_p()]),
        ),
        Scoped::Cross => run(
            db,
            "MATCH (n:$($label))",
            Some("(n.source_repository_id = $repo OR n.target_repository_id = $repo)".into()),
            gparams([label_p(), repo_p()]),
        ),
        Scoped::Global => run(db, "MATCH (n:$($label))", None, gparams([label_p()])),
    }
}

/// A node's text: its string properties, concatenated (engine `_`-prefixed
/// metadata excluded).
fn text_of(node: &Value) -> String {
    node.as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| !k.starts_with('_'))
        .filter_map(|(_, v)| v.as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Nodes whose text contains any pattern.
fn matching(nodes: &[Value], patterns: &[String]) -> usize {
    if patterns.is_empty() {
        return 0;
    }
    nodes
        .iter()
        .filter(|n| {
            let text = text_of(n);
            patterns.iter().any(|p| text.contains(p.as_str()))
        })
        .count()
}

/// Exact references to deleted ids on every label in scope. Labels holding
/// connectors' originals are checked structurally instead.
fn references(
    db: Graph,
    labels: &[String],
    patterns: &[String],
    (repository, workspace): (&str, &str),
    skip: &[&str],
    skip_repository_labels: bool,
) -> Result<Value> {
    let mut scanned = 0;
    let mut matches = Vec::new();
    for label in labels {
        if skip.contains(&label.as_str())
            || skip.contains(&table_of(label))
            || scoping(label) == Scoped::Connector
            || (skip_repository_labels && scoping(label) == Scoped::Repository)
        {
            continue;
        }
        scanned += 1;
        let found = matching(&nodes(db, label, repository, workspace)?, patterns);
        if found > 0 {
            matches.push(json!({"table":table_of(label),"rows":found}));
        }
    }
    Ok(json!({"tables_scanned":scanned,"matches":matches}))
}

/// References to forgotten document ids. A substring hit counts unless it is
/// the start of a longer id of a document that still exists (`a` inside `a:b`).
fn document_references(
    db: Graph,
    labels: &[String],
    scope: &Scope,
    existing: &BTreeSet<String>,
    (repository, workspace): (&str, &str),
) -> Result<Value> {
    let ids = scope.sources();
    let mut scanned = 0;
    let mut matches = Vec::new();
    for label in labels {
        if scoping(label) == Scoped::Connector {
            continue;
        }
        scanned += 1;
        let found = nodes(db, label, repository, workspace)?
            .iter()
            .filter(|n| names_any(&text_of(n), &ids, existing))
            .count();
        if found > 0 {
            matches.push(json!({"table":table_of(label),"rows":found}));
        }
    }
    Ok(json!({"tables_scanned":scanned,"matches":matches}))
}

fn names_any(text: &str, ids: &[String], existing: &BTreeSet<String>) -> bool {
    ids.iter().any(|id| {
        text.match_indices(id.as_str()).any(|(at, _)| {
            !existing
                .range(id.clone()..)
                .take_while(|e| e.starts_with(id.as_str()))
                .any(|e| e.len() > id.len() && text[at..].starts_with(e.as_str()))
        })
    })
}

/// Verbatim excerpts of deleted text in this project's derived labels.
fn verbatim(
    db: Graph,
    labels: &[String],
    fingerprints: &[String],
    repository: &str,
) -> Result<Value> {
    let mut scanned = 0;
    let mut matches = Vec::new();
    for label in labels {
        if scoping(label) != Scoped::Repository || INDEPENDENT.contains(&label.as_str()) {
            continue;
        }
        scanned += 1;
        let found = matching(&nodes(db, label, repository, "")?, fingerprints);
        if found > 0 {
            matches.push(json!({"table":table_of(label),"rows":found}));
        }
    }
    Ok(json!({"fingerprints":fingerprints.len(),"tables_scanned":scanned,"matches":matches}))
}

/// A middle window of the longest run of characters that no JSON encoder
/// escapes, so a copy is found inside serialized properties too.
fn excerpt(content: &str) -> Option<String> {
    let safe = |c: char| c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\';
    let mut best = (0, 0);
    let mut start = None;
    for (i, c) in content.char_indices().chain([(content.len(), '\n')]) {
        match (safe(c), start) {
            (true, None) => start = Some(i),
            (false, Some(a)) => {
                if i - a > best.1 - best.0 {
                    best = (a, i);
                }
                start = None;
            }
            _ => {}
        }
    }
    let run = content[best.0..best.1].trim();
    let offset = run.len().saturating_sub(FINGERPRINT_BYTES) / 2;
    let window = run[offset..(offset + FINGERPRINT_BYTES).min(run.len())].trim();
    (window.len() >= MIN_FINGERPRINT_BYTES
        && window.chars().filter(char::is_ascii_alphanumeric).count() >= 8)
        .then(|| window.to_string())
}

/// Nodes of `label` matching `predicate` whose `key` property passes `keep` —
/// counted, and with `delete` also detach-deleted (the label-scoped removal
/// walk). `key` is a module constant, so interpolating it (dotted properties
/// backtick-quoted) is safe.
fn walk(
    db: Graph,
    label: &str,
    predicate: &str,
    key: &str,
    params: store_graph::Params,
    keep: impl Fn(&str) -> bool,
    delete: bool,
) -> Result<usize> {
    let mut params = params;
    params.insert("label".into(), store_graph::json_to_value(&json!(label)));
    let found: Vec<String> = rows(
        db,
        &format!("MATCH (n:$($label)) WHERE {predicate} RETURN n.`{key}` AS key"),
        params.clone(),
    )?
    .into_iter()
    .filter_map(|r| r["key"].as_str().map(str::to_string))
    .filter(|v| keep(v))
    .collect();
    if delete && !found.is_empty() {
        params.insert("keys".into(), store_graph::json_to_value(&json!(found)));
        db.query_with_params(
            &format!("MATCH (n:$($label)) WHERE {predicate} AND n.`{key}` IN $keys DETACH DELETE n"),
            params,
        )?;
    }
    Ok(found.len())
}

/// Delete nodes of `label` matching `predicate`, returning the count.
fn detach(db: Graph, label: &str, predicate: &str, params: store_graph::Params) -> Result<usize> {
    let mut params = params;
    params.insert("label".into(), store_graph::json_to_value(&json!(label)));
    let found = counted(
        db,
        &format!("MATCH (n:$($label)) WHERE {predicate} RETURN count(n) AS n"),
        params.clone(),
    )?;
    if found > 0 {
        db.query_with_params(
            &format!("MATCH (n:$($label)) WHERE {predicate} DETACH DELETE n"),
            params,
        )?;
    }
    Ok(found)
}

/// Delete nodes by internal id (collected by an earlier scan this
/// transaction).
fn detach_ids(db: Graph, label: &str, ids: &[i64]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    db.query_with_params(
        "MATCH (n:$($label)) WHERE id(n) IN $ids DETACH DELETE n",
        gparams([("label", json!(label)), ("ids", json!(ids))]),
    )?;
    Ok(ids.len())
}

/// The dependency index's `(:Artifact {kind, id, workspace})
/// -[:DEPENDS_ON]->(:Source)` entries of one dependent — the graph
/// counterpart of `invalidation::forget_dependent` (which migrates with the
/// invalidation module).
fn forget_dependent(db: Graph, workspace: &str, kind: &str, id: &str) -> Result<()> {
    // The entry's own node and the sources it points at, before the edges
    // go: an artifact left with no `DEPENDS_ON` edge in either direction
    // carries no dependency information, and the verified deletion scans
    // for exactly that residue. A source another dependent still points at
    // keeps its edge, so it survives the sweep.
    let endpoints: BTreeSet<i64> = rows(
        db,
        "MATCH (a:Artifact {workspace: $ws, kind: $kind, id: $id})-[:DEPENDS_ON]->(s:Artifact) \
         RETURN id(a) AS a, id(s) AS s",
        gparams([("ws", json!(workspace)), ("kind", json!(kind)), ("id", json!(id))]),
    )?
    .iter()
    .flat_map(|row| [row["a"].as_i64(), row["s"].as_i64()])
    .flatten()
    .collect();
    db.query_with_params(
        "MATCH (a:Artifact {workspace: $ws, kind: $kind, id: $id})-[r:DEPENDS_ON]->() DELETE r",
        gparams([("ws", json!(workspace)), ("kind", json!(kind)), ("id", json!(id))]),
    )?;
    if !endpoints.is_empty() {
        db.query_with_params(
            "MATCH (a:Artifact) WHERE id(a) IN $ids AND NOT (a)-[:DEPENDS_ON]-() \
             DETACH DELETE a",
            gparams([("ids", json!(endpoints.into_iter().collect::<Vec<_>>()))]),
        )?;
    }
    Ok(())
}

/// Drop the staleness markers of one artifact (the graph counterpart of
/// `invalidation::clear_stale`).
fn clear_stale(db: Graph, workspace: &str, kind: &str, id: &str) -> Result<()> {
    db.query_with_params(
        "MATCH (s:Stale {workspace: $ws, kind: $kind, id: $id}) DETACH DELETE s",
        gparams([("ws", json!(workspace)), ("kind", json!(kind)), ("id", json!(id))]),
    )?;
    Ok(())
}

fn cites(data: &str, scope: &Scope) -> bool {
    serde_json::from_str::<Value>(data).is_ok_and(|d| {
        d["native_sources"]
            .as_object()
            .is_some_and(|m| m.keys().any(|k| scope.source(k)))
    })
}

fn lists(list: &str, scope: &Scope) -> bool {
    serde_json::from_str::<Vec<Value>>(list).is_ok_and(|v| {
        v.iter()
            .any(|x| x.as_str().is_some_and(|i| scope.source(i)))
    })
}

/// A dependency-index id `<repository>:<…>` of an artifact derived from a
/// forgotten document: its observer note or one of its passage embeddings.
fn derived(repository: &str, scope: &Scope, kind: &str, id: &str) -> bool {
    let Some(rest) = id
        .strip_prefix(repository)
        .and_then(|r| r.strip_prefix(':'))
    else {
        return false;
    };
    match kind {
        crate::observer::NOTE => scope.source(rest),
        crate::invalidation::EMBEDDING => scope.passage(rest),
        _ => false,
    }
}

/// Dependency-index relationship ids (`(:Artifact)-[:DEPENDS_ON]->(:Source)`)
/// naming or derived from the forgotten documents.
/// `(edge, dependent node, source node)` internal ids of each in-scope
/// dependency-index relationship.
fn index_edges(
    db: Graph,
    workspace: &str,
    repository: &str,
    scope: &Scope,
) -> Result<Vec<(i64, i64, i64)>> {
    let mut out = Vec::new();
    for row in rows(
        db,
        "MATCH (a:Artifact {workspace: $ws})-[r:DEPENDS_ON]->(src:Artifact) \
         RETURN id(r) AS rid, id(a) AS dependent, id(src) AS source, \
         a.kind AS dependent_kind, a.id AS dependent_id, \
         src.kind AS source_kind, src.id AS source_id",
        gparams([("ws", json!(workspace))]),
    )? {
        if (row["source_kind"] == crate::invalidation::DOCUMENT && scope.source(s(&row, "source_id")))
            || derived(
                repository,
                scope,
                s(&row, "dependent_kind"),
                s(&row, "dependent_id"),
            )
        {
            out.push((
                row["rid"].as_i64().unwrap_or_default(),
                row["dependent"].as_i64().unwrap_or_default(),
                row["source"].as_i64().unwrap_or_default(),
            ));
        }
    }
    Ok(out)
}

/// Staleness markers (`:Stale` nodes) of artifacts derived from the
/// forgotten documents.
fn index_markers(db: Graph, workspace: &str, repository: &str, scope: &Scope) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    for row in rows(
        db,
        "MATCH (s:Stale {workspace: $ws}) RETURN id(s) AS nid, s.kind AS kind, s.id AS id",
        gparams([("ws", json!(workspace))]),
    )? {
        if derived(repository, scope, s(&row, "kind"), s(&row, "id")) {
            out.push(row["nid"].as_i64().unwrap_or_default());
        }
    }
    Ok(out)
}

/// Nodes derived from or naming the forgotten documents, by kind. Receipts
/// keep the legacy table names as keys (`graphicle.deletion.v1`).
fn source_rows(
    db: Graph,
    workspace: &str,
    repository: &str,
    scope: &Scope,
) -> Result<BTreeMap<&'static str, usize>> {
    let connector = scope.connector.as_str();
    let mut out = BTreeMap::new();
    if scope.whole() {
        out.insert(
            "connector",
            counted(
                db,
                "MATCH (c:Connector {id: $connector}) RETURN count(c) AS n",
                gparams([("connector", json!(connector))]),
            )?,
        );
    }
    for (name, label, key) in [
        ("documents", "ConnectorDoc", "id"),
        ("versions", "ConnectorVersion", "id"),
        ("staged", "ConnectorStage", "id"),
        ("permissions", "SourcePermission", "source_id"),
    ] {
        let found = walk(
            db,
            label,
            "n.connector_id = $connector",
            key,
            gparams([("connector", json!(connector))]),
            |v| scope.document(v),
            false,
        )?;
        out.insert(name, found);
    }
    let repo = gparams([("repo", json!(repository))]);
    let per_repository: [(&'static str, &'static str, &'static str, Matches); 8] = [
        ("originals", "OriginalDocument", "source_id", |v, sc| sc.source(v)),
        ("memories", "Memory", "data", cites),
        ("observer_notes", "Observation", "source_id", |v, sc| sc.source(v)),
        ("observer_reflections", "MentalModel", "sources", lists),
        ("vectors", "Chunk", "id", |v, sc| sc.passage(v)),
        ("document_reads", "DocumentRead", "source_id", |v, sc| sc.source(v)),
        ("anchors", "AnchorEdge", "attributes.document_id", |v, sc| sc.source(v)),
        ("hints", "Hint", "target", |v, sc| sc.source(v)),
    ];
    for (name, label, key, keep) in per_repository {
        let found = walk(
            db,
            label,
            "n.repository_id = $repo",
            key,
            repo.clone(),
            |v| keep(v, scope),
            false,
        )?;
        out.insert(name, found);
    }
    // Mention links are `(:ConnectorDoc)-[:LINKS_TO]->(...)` edges on the
    // link rows' own nodes: `source_kind` marks such a node, and its id is
    // the document's source id (`connector:<id>:<doc>`, `file:<path>`).
    let mut doc_links = 0usize;
    let mut doc_link_sources = 0usize;
    for row in rows(
        db,
        "MATCH (d:ConnectorDoc {repository_id: $repo}) WHERE d.source_kind IS NOT NULL \
         OPTIONAL MATCH (d)-[l:LINKS_TO]->() RETURN d.id AS id, count(l) AS links",
        repo.clone(),
    )? {
        if scope.source(s(&row, "id")) {
            doc_link_sources += 1;
            doc_links += row["links"].as_u64().unwrap_or(0) as usize;
        }
    }
    out.insert("doc_links", doc_links);
    out.insert("doc_link_sources", doc_link_sources);
    // Served-evidence and outcome records whose lists name the documents
    // (the lists are list-of-scalar properties; older writers stringified
    // them — both read).
    for (name, label, property) in [("served", "Served", "served"), ("outcomes", "Outcome", "targets")] {
        let found = nodes(db, label, repository, workspace)?
            .iter()
            .filter(|n| match &n[property] {
                Value::Array(items) => items.iter().any(|i| i.as_str().is_some_and(|i| scope.source(i))),
                Value::String(text) => lists(text, scope),
                _ => false,
            })
            .count();
        out.insert(name, found);
    }
    out.insert(
        "dependency_edges",
        index_edges(db, workspace, repository, scope)?.len(),
    );
    out.insert(
        "stale_markers",
        index_markers(db, workspace, repository, scope)?.len(),
    );
    Ok(out)
}

/// Nodes of the artifacts listed in [`SINCE_VISIBLE`] dated from `since`
/// (and before `until`, when given).
fn since_visible(
    db: Graph,
    repository: &str,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<BTreeMap<&'static str, usize>> {
    let mut out = BTreeMap::new();
    for (name, label, column) in SINCE_VISIBLE {
        let n = match since {
            None => 0,
            Some(since) => counted(
                db,
                &format!(
                    "MATCH (n:$($label)) WHERE n.repository_id = $repo \
                     AND n.`{column}` >= $since AND ($until IS NULL OR n.`{column}` < $until) \
                     RETURN count(n) AS n"
                ),
                gparams([
                    ("label", json!(label)),
                    ("repo", json!(repository)),
                    ("since", json!(since)),
                    ("until", until.map_or(Value::Null, |u| json!(u))),
                ]),
            )?,
        };
        out.insert(*name, n);
    }
    Ok(out)
}

/// What a forget request must cover, captured before anything is removed.
pub struct Plan {
    scope: Scope,
    since: Option<String>,
    sources: usize,
    sources_digest: String,
    texts: usize,
    fingerprints: Vec<String>,
    rows: BTreeMap<&'static str, usize>,
    visible_since: BTreeMap<&'static str, usize>,
}

impl Plan {
    /// When the forgotten documents first became visible (their first
    /// imported version); `None` if they never were.
    pub fn since(&self) -> Option<&str> {
        self.since.as_deref()
    }
    pub fn sources(&self) -> usize {
        self.sources
    }
    /// Rows forgetting would remove, per label (originals and single-source
    /// artifacts).
    pub fn rows(&self) -> &BTreeMap<&'static str, usize> {
        &self.rows
    }
    /// Derived rows computed while the documents were visible, per kind.
    pub fn visible_since(&self) -> &BTreeMap<&'static str, usize> {
        &self.visible_since
    }
}

pub fn plan(
    db: Graph,
    sql: &rusqlite::Connection,
    workspace: &str,
    repository: &str,
    scope: &Scope,
) -> Result<Plan> {
    let prefix = scope.prefix();
    let mut sources = BTreeSet::new();
    let mut texts: BTreeMap<String, String> = BTreeMap::new();
    for label in ["ConnectorDoc", "ConnectorVersion", "ConnectorStage"] {
        for row in rows(
            db,
            "MATCH (d:$($label) {connector_id: $connector}) \
             RETURN d.id AS id, d.sha256 AS sha256, d.content AS content",
            gparams([("label", json!(label)), ("connector", json!(scope.connector))]),
        )? {
            if !scope.document(s(&row, "id")) {
                continue;
            }
            sources.insert((
                format!("{prefix}{}", s(&row, "id")),
                s(&row, "sha256").to_string(),
            ));
            texts
                .entry(s(&row, "sha256").to_string())
                .or_insert_with(|| s(&row, "content").to_string());
        }
    }
    for row in rows(
        db,
        "MATCH (o:OriginalDocument {repository_id: $repo}) RETURN o.source_id AS source_id, o.sha256 AS sha256",
        gparams([("repo", json!(repository))]),
    )? {
        if scope.source(s(&row, "source_id")) {
            sources.insert((
                s(&row, "source_id").to_string(),
                s(&row, "sha256").to_string(),
            ));
        }
    }
    let mut since: Option<String> = None;
    for row in rows(
        db,
        "MATCH (v:ConnectorVersion {connector_id: $connector}) RETURN v.id AS id, v.created_at AS created_at",
        gparams([("connector", json!(scope.connector))]),
    )? {
        let stamp = s(&row, "created_at");
        if scope.document(s(&row, "id")) && since.as_deref().is_none_or(|first| stamp < first) {
            since = Some(stamp.to_string());
        }
    }
    // Content hashes order the sample, so it is deterministic but unbiased.
    let fingerprints: Vec<String> = texts
        .values()
        .filter_map(|t| excerpt(t))
        .take(FINGERPRINTS)
        .collect();
    // The store is mid-migration: connector data is graph-resident, but the
    // observer, overlay and product tables are still SQL. A verified plan
    // covers both.
    let mut rows = source_rows(db, workspace, repository, scope)?;
    merge_counts(
        &mut rows,
        legacy::source_rows(sql, workspace, repository, scope)?,
    );
    let mut visible_since = since_visible(db, repository, since.as_deref(), None)?;
    merge_counts(
        &mut visible_since,
        legacy::since_visible(sql, repository, since.as_deref(), None)?,
    );
    Ok(Plan {
        scope: scope.clone(),
        sources: sources.len(),
        sources_digest: digest(&json!(sources)),
        texts: texts.len(),
        fingerprints,
        rows,
        visible_since,
        since,
    })
}

/// Add `extra`'s per-kind counts into `into` (same legacy row names).
fn merge_counts(into: &mut BTreeMap<&'static str, usize>, extra: BTreeMap<&'static str, usize>) {
    for (kind, n) in extra {
        *into.entry(kind).or_insert(0) += n;
    }
}

/// Sum the numeric entries of `extra` into `into` (the `forget` result
/// shape: flat counts plus a nested `since_visible` count map).
pub(crate) fn merge_removed(into: &mut Value, extra: &Value) {
    for (key, value) in extra.as_object().into_iter().flatten() {
        if key == "since_visible" {
            for (kind, n) in value.as_object().into_iter().flatten() {
                let entry = &mut into["since_visible"][kind];
                *entry = json!(entry.as_u64().unwrap_or(0) + n.as_u64().unwrap_or(0));
            }
        } else if let Some(n) = value.as_u64() {
            let entry = &mut into[key];
            *entry = json!(entry.as_u64().unwrap_or(0) + n);
        }
    }
}

/// A model draft written while the forgotten documents were visible, whatever
/// sources it cites. An agent's unreviewed proposal (`memory.propose`) counts
/// as a model draft: the agent may have read the forgotten documents through
/// recall. (Single-source drafts of other sources that compact extraction
/// wrote are kept; compact extraction is not supported, unit
/// `compact-extraction`.)
fn drafted_while_visible(record: &Value, since: &str) -> bool {
    s(record, "created_at") >= since
        && !["reviewed", "retired"].contains(&s(record, "state"))
        && ["unreviewed_model_interpretation", "agent_proposed_draft"]
            .contains(&s(&record["data"], "authority"))
}

/// A memory record as the memory pipeline stores it: `data` holds the
/// memory's JSON (a stringified property — its keys are document-specific,
/// so it is not flattened; engine v0.12.2 stores no map-valued properties).
fn memory_record(row: &Value) -> Result<Value> {
    let mut record = json!({
        "id": row["id"], "state": row["state"], "created_at": row["created_at"],
        "data": row["data"],
    });
    decode(&mut record, "data")?;
    Ok(record)
}

/// The dependency-index and embedding cleanup that follows a removed memory
/// (the passage identity `memory:<id>` and its index key).
fn forget_memory_artifacts(db: Graph, workspace: &str, repository: &str, identifier: &str) -> Result<()> {
    forget_dependent(db, workspace, crate::invalidation::MEMORY, identifier)?;
    clear_stale(db, workspace, crate::invalidation::MEMORY, identifier)?;
    let key = format!("{repository}:memory:{identifier}");
    forget_dependent(db, workspace, crate::invalidation::EMBEDDING, &key)?;
    clear_stale(db, workspace, crate::invalidation::EMBEDDING, &key)?;
    Ok(())
}

/// Memories citing the documents in any revision, model drafts written while
/// they were visible, and memories that depend on either.
fn forget_memories(
    db: Graph,
    workspace: &str,
    repository: &str,
    scope: &Scope,
    since: Option<&str>,
) -> Result<BTreeSet<String>> {
    let mut records = Vec::new();
    for row in rows(
        db,
        "MATCH (m:Memory {repository_id: $repo}) \
         RETURN m.id AS id, m.state AS state, m.created_at AS created_at, m.data AS data",
        gparams([("repo", json!(repository))]),
    )? {
        records.push(memory_record(&row)?);
    }
    let mut removed = BTreeSet::new();
    let direct = |data: &Value| {
        data["native_sources"]
            .as_object()
            .is_some_and(|d| d.keys().any(|k| scope.source(k)))
    };
    for record in &records {
        let identifier = s(record, "id").to_string();
        if direct(&record["data"]) || since.is_some_and(|t| drafted_while_visible(record, t)) {
            removed.insert(identifier.clone());
        }
        for revision in rows(
            db,
            "MATCH (r:MemoryRevision)-[:REVISION_OF]->(:Memory {id: $id}) RETURN r.data AS data",
            gparams([("id", json!(identifier))]),
        )? {
            let mut prior: Value = serde_json::from_str(s(&revision, "data"))?;
            decode(&mut prior, "data")?;
            if direct(&prior["data"]) {
                removed.insert(identifier.clone());
            }
        }
    }
    let records = records;
    loop {
        let before = removed.len();
        for record in &records {
            let data = &record["data"];
            if data["notes"]
                .as_object()
                .is_some_and(|d| d.keys().any(|k| removed.contains(k)))
                || removed.contains(s(&data["historical_target"], "memory_id"))
            {
                removed.insert(s(record, "id").to_string());
            }
        }
        if removed.len() == before {
            break;
        }
    }
    for identifier in &removed {
        // The node takes its revisions, its embedding and every relationship
        // (claims' ABOUT_MEMORY, symbol PINNED_TO) with it.
        db.query_with_params(
            "MATCH (m:Memory {repository_id: $repo, id: $id}) DETACH DELETE m",
            gparams([("repo", json!(repository)), ("id", json!(identifier))]),
        )?;
        forget_memory_artifacts(db, workspace, repository, identifier)?;
    }
    Ok(removed)
}

/// Derived nodes that may name a memory by id: served-evidence records,
/// outcomes, hints and caches. [`delete_memories`] removes the nodes naming a
/// deleted memory; the rest of each label stays. (Receipt keys keep the
/// legacy table names.)
const NAMES_MEMORIES: &[(&str, &str)] = &[
    ("gf_served", "Served"),
    ("gf_outcomes", "Outcome"),
    ("gf_hints", "Hint"),
    ("am_model_cache", "ModelCache"),
    ("am_query_cache", "QueryCache"),
];

/// Delete `memory_id` and every visible memory that depends on it — notes
/// naming it, historical versions targeting it, transitively — with their
/// revisions and claims (cascaded), embeddings, dependency-index entries and
/// the derived nodes ([`NAMES_MEMORIES`]) that name them. Returns the removed
/// ids and per-label counts (keyed by the legacy names); the caller signs
/// [`rows_receipt`].
pub fn delete_memories(
    db: Graph,
    workspace: &str,
    repository: &str,
    memories: &[Value],
    memory_id: &str,
) -> Result<(BTreeSet<String>, Value)> {
    let mut removed = BTreeSet::from([memory_id.to_string()]);
    loop {
        let before = removed.len();
        for record in memories {
            let data = &record["data"];
            if data["notes"]
                .as_object()
                .is_some_and(|d| d.keys().any(|k| removed.contains(k)))
                || removed.contains(s(&data["historical_target"], "memory_id"))
            {
                removed.insert(s(record, "id").to_string());
            }
        }
        if removed.len() == before {
            break;
        }
    }
    let keys: Vec<String> = removed.iter().cloned().collect();
    let mut counts = BTreeMap::new();
    let mut revisions = 0usize;
    let mut claims = 0usize;
    let mut vectors = 0usize;
    let mut pins = 0usize;
    for identifier in &removed {
        revisions += counted(
            db,
            "MATCH (r:MemoryRevision)-[:REVISION_OF]->(:Memory {id: $id}) RETURN count(r) AS n",
            gparams([("id", json!(identifier))]),
        )?;
        // The revisions go with the memory: each carries the memory's
        // `memory_id` and its raw content, so leaving them behind would
        // keep the forgotten memory's text in the store — and the receipt,
        // which counts them under `removed`, would be reporting a removal
        // that did not happen. Deleted before the memory node, while the
        // `REVISION_OF` edge still reaches them.
        db.query_with_params(
            "MATCH (r:MemoryRevision)-[:REVISION_OF]->(:Memory {id: $id}) DETACH DELETE r",
            gparams([("id", json!(identifier))]),
        )?;
        // The memory's pinned or pending quotes go with it.
        pins += counted(
            db,
            "MATCH (:Memory {repository_id: $repo, id: $id})-[r:PINNED_TO]->() RETURN count(r) AS n",
            gparams([("repo", json!(repository)), ("id", json!(identifier))]),
        )?;
        claims += detach(
            db,
            "Claim",
            "n.repository_id = $repo AND n.memory_id = $id",
            gparams([("repo", json!(repository)), ("id", json!(identifier))]),
        )?;
        vectors += counted(
            db,
            "MATCH (m:Memory {repository_id: $repo, id: $id}) \
             WHERE m.embedding IS NOT NULL RETURN count(m) AS n",
            gparams([("repo", json!(repository)), ("id", json!(identifier))]),
        )?;
        db.query_with_params(
            "MATCH (m:Memory {repository_id: $repo, id: $id}) DETACH DELETE m",
            gparams([("repo", json!(repository)), ("id", json!(identifier))]),
        )?;
        forget_memory_artifacts(db, workspace, repository, identifier)?;
    }
    counts.insert("repo_memories".to_string(), removed.len());
    counts.insert("repo_memory_revisions".to_string(), revisions);
    counts.insert("am_claims".to_string(), claims);
    counts.insert("am_vectors".to_string(), vectors);
    if pins > 0 {
        counts.insert("gf_symbol_pins".to_string(), pins);
    }
    let labels = catalog(db)?;
    for (legacy, label) in NAMES_MEMORIES {
        if !labels.iter().any(|l| l == label) {
            continue;
        }
        let mut ids = Vec::new();
        for node in nodes(db, label, repository, workspace)? {
            let text = text_of(&node);
            if keys.iter().any(|k| text.contains(k.as_str())) {
                ids.push(node["_id"].as_i64().unwrap_or_default());
            }
        }
        let deleted = detach_ids(db, label, &ids)?;
        if deleted > 0 {
            counts.insert(legacy.to_string(), deleted);
        }
    }
    Ok((removed, json!(counts)))
}

/// Rows still holding any of the deleted memories, per label (all zero after
/// [`delete_memories`]). Keys keep the legacy names.
pub fn memory_rows(db: Graph, repository: &str, ids: &[String]) -> Result<Value> {
    let mut out = BTreeMap::new();
    let repo = || gparams([("repo", json!(repository)), ("ids", json!(ids))]);
    for (legacy, cypher) in [
        (
            "repo_memories",
            "MATCH (m:Memory {repository_id: $repo}) WHERE m.id IN $ids RETURN count(m) AS n",
        ),
        (
            "repo_memory_revisions",
            "MATCH (r:MemoryRevision) WHERE r.memory_id IN $ids RETURN count(r) AS n",
        ),
        (
            "am_claims",
            "MATCH (c:Claim {repository_id: $repo}) WHERE c.memory_id IN $ids RETURN count(c) AS n",
        ),
        (
            "gf_symbol_pins",
            "MATCH (m:Memory {repository_id: $repo})-[r:PINNED_TO]->() WHERE m.id IN $ids RETURN count(r) AS n",
        ),
    ] {
        let total = counted(db, cypher, repo())?;
        // Listed only when a deleted memory's pin remains.
        if legacy == "gf_symbol_pins" && total == 0 {
            continue;
        }
        out.insert(legacy.to_string(), total);
    }
    // Passage embeddings of the deleted memories (`memory:<id>` identities).
    let passages: Vec<String> = ids.iter().map(|i| format!("memory:{i}")).collect();
    let mut vectors = 0;
    for label in ["Chunk", "Memory", "Entity"] {
        vectors += counted(
            db,
            "MATCH (n:$($label) {repository_id: $repo}) \
             WHERE n.identity IN $ids AND n.embedding IS NOT NULL RETURN count(n) AS n",
            gparams([("label", json!(label)), ("repo", json!(repository)), ("ids", json!(passages))]),
        )?;
    }
    out.insert("am_vectors".to_string(), vectors);
    Ok(json!(out))
}

/// Remove every derived artifact that could carry the documents' information
/// (see the module documentation): label-scoped `DETACH DELETE` walks over
/// the catalog, inside the caller's graph transaction. Originals are removed
/// by [`erase`].
pub fn forget(
    db: Graph,
    workspace: &str,
    repository: &str,
    scope: &Scope,
    since: Option<&str>,
) -> Result<Value> {
    let repo = || gparams([("repo", json!(repository))]);
    let claims = |db: Graph| -> Result<BTreeSet<String>> {
        Ok(rows(
            db,
            "MATCH (c:Claim {repository_id: $repo}) RETURN c.id AS id",
            repo(),
        )?
        .iter()
        .map(|r| s(r, "id").to_string())
        .collect())
    };
    let pins = |db: Graph| {
        counted(
            db,
            "MATCH (:Memory {repository_id: $repo})-[r:PINNED_TO]->() RETURN count(r) AS n",
            repo(),
        )
    };
    let claims_before = claims(db)?;
    let pins_before = pins(db)?;
    let memories = forget_memories(db, workspace, repository, scope, since)?;
    // The forgotten memories' symbol pins went with them.
    let symbol_pins = pins_before.saturating_sub(pins(db)?);
    let lost: Vec<String> = claims_before
        .difference(&claims(db)?)
        .cloned()
        .chain(memories.iter().cloned())
        .collect();
    // A reflection consumed many sources; any forgotten one removes it.
    let reflections = walk(
        db,
        "MentalModel",
        "n.repository_id = $repo",
        "sources",
        repo(),
        |v| lists(v, scope),
        true,
    )?;
    // Single-source artifacts of the forgotten documents.
    let mut notes = 0;
    for row in rows(
        db,
        "MATCH (o:Observation {repository_id: $repo}) RETURN o.source_id AS source_id",
        repo(),
    )? {
        let source = s(&row, "source_id");
        if scope.source(source) {
            notes += detach(
                db,
                "Observation",
                "n.repository_id = $repo AND n.source_id = $source",
                gparams([("repo", json!(repository)), ("source", json!(source))]),
            )?;
            let artifact = crate::observer::artifact_id(repository, source);
            forget_dependent(db, workspace, crate::observer::NOTE, &artifact)?;
            clear_stale(db, workspace, crate::observer::NOTE, &artifact)?;
        }
    }
    // Passage embeddings of the forgotten documents (the passages live on the
    // nodes they describe; forgetting strips the embedding, the chunk of an
    // independent source stays).
    let mut passages = Vec::new();
    for label in ["Chunk", "Entity"] {
        for row in rows(
            db,
            "MATCH (n:$($label) {repository_id: $repo}) RETURN n.id AS identity",
            gparams([("label", json!(label)), ("repo", json!(repository))]),
        )? {
            if scope.passage(s(&row, "identity")) {
                passages.push((label, s(&row, "identity").to_string()));
            }
        }
    }
    let vectors = passages.len();
    for (label, identity) in passages {
        if label == "Chunk" {
            // A stub Chunk exists only to carry its embedding
            // (vector::delete_embeddings deletes it): it goes with the
            // forgotten passage. Content chunks carry no `id`.
            db.query_with_params(
                "MATCH (n:Chunk {repository_id: $repo, id: $identity}) \
                 WHERE n.embedding IS NOT NULL DETACH DELETE n",
                gparams([
                    ("repo", json!(repository)),
                    ("identity", json!(identity)),
                ]),
            )?;
        } else {
            db.query_with_params(
                "MATCH (n:$($label) {repository_id: $repo, id: $identity}) \
                 REMOVE n.embedding, n.embedding_model_key",
                gparams([
                    ("label", json!(label)),
                    ("repo", json!(repository)),
                    ("identity", json!(identity)),
                ]),
            )?;
        }
    }
    // An extraction read whose drafts were just removed must run again.
    let mut read_ids = Vec::new();
    for row in rows(
        db,
        "MATCH (r:DocumentRead {repository_id: $repo}) \
         RETURN id(r) AS nid, r.source_id AS source_id, r.claim_ids AS claim_ids",
        repo(),
    )? {
        let lost_claim = |claims: &str| {
            serde_json::from_str::<Vec<Value>>(claims).is_ok_and(|v| {
                v.iter()
                    .any(|c| c.as_str().is_some_and(|c| lost.iter().any(|id| id == c)))
            })
        };
        if scope.source(s(&row, "source_id")) || lost_claim(s(&row, "claim_ids")) {
            read_ids.push(row["nid"].as_i64().unwrap_or_default());
        }
    }
    let reads = detach_ids(db, "DocumentRead", &read_ids)?;
    // Artifacts computed while the documents were visible.
    let mut visible_since = BTreeMap::new();
    if let Some(since) = since {
        // Cached answers: their index entries go with them.
        for row in rows(
            db,
            "MATCH (a:Artifact {workspace: $ws, kind: $kind}) \
             WHERE a.id STARTS WITH $prefix AND a.recorded_at >= $since RETURN a.id AS id",
            gparams([
                ("ws", json!(workspace)),
                ("kind", json!(crate::invalidation::ANSWER)),
                ("prefix", json!(format!("{repository}:"))),
                ("since", json!(since)),
            ]),
        )? {
            let key = s(&row, "id");
            forget_dependent(db, workspace, crate::invalidation::ANSWER, key)?;
            clear_stale(db, workspace, crate::invalidation::ANSWER, key)?;
        }
        for (kind, label) in [
            (crate::invalidation::REPORT, "Report"),
            (crate::invalidation::COMMUNITY, "Community"),
        ] {
            for row in rows(
                db,
                "MATCH (n:$($label) {repository_id: $repo}) WHERE n.created_at >= $since RETURN n.id AS id",
                gparams([
                    ("label", json!(label)),
                    ("repo", json!(repository)),
                    ("since", json!(since)),
                ]),
            )? {
                let key = format!("{repository}:{}", s(&row, "id"));
                forget_dependent(db, workspace, kind, &key)?;
                clear_stale(db, workspace, kind, &key)?;
            }
        }
        for row in rows(
            db,
            "MATCH (a:AnchorEdge {repository_id: $repo}) WHERE a.created_at >= $since RETURN a.chunk_id AS chunk_id",
            gparams([("repo", json!(repository)), ("since", json!(since))]),
        )? {
            let key = crate::invalidation::anchor_id(repository, s(&row, "chunk_id"));
            forget_dependent(db, workspace, crate::invalidation::ANCHOR, &key)?;
            clear_stale(db, workspace, crate::invalidation::ANCHOR, &key)?;
        }
        for (name, label, column) in SINCE_VISIBLE {
            let n = detach(
                db,
                label,
                &format!("n.repository_id = $repo AND n.`{column}` >= $since"),
                gparams([("repo", json!(repository)), ("since", json!(since))]),
            )?;
            visible_since.insert(*name, n);
        }
    }
    // References in older nodes (none unless written outside the normal flow).
    let mut anchors = 0;
    for row in rows(
        db,
        "MATCH (a:AnchorEdge {repository_id: $repo}) \
         RETURN id(a) AS nid, a.chunk_id AS chunk_id, a.`attributes.document_id` AS document",
        repo(),
    )? {
        if scope.source(s(&row, "document")) {
            let key = crate::invalidation::anchor_id(repository, s(&row, "chunk_id"));
            forget_dependent(db, workspace, crate::invalidation::ANCHOR, &key)?;
            clear_stale(db, workspace, crate::invalidation::ANCHOR, &key)?;
            anchors += detach_ids(db, "AnchorEdge", &[row["nid"].as_i64().unwrap_or_default()])?;
        }
    }
    let hints = walk(
        db,
        "Hint",
        "n.repository_id = $repo",
        "target",
        repo(),
        |v| scope.source(v),
        true,
    )?;
    // The documents' mention links and what they were computed from
    // (`(:ConnectorDoc)-[:LINKS_TO]->(:Symbol)`, §2.5): the edges and the
    // link rows' own nodes, whose ids are the documents' source ids.
    let mut doc_links = 0;
    {
        let mut ids = Vec::new();
        for row in rows(
            db,
            "MATCH (d:ConnectorDoc {repository_id: $repo})-[r:LINKS_TO]->() \
             RETURN id(r) AS rid, d.id AS document",
            repo(),
        )? {
            if scope.source(s(&row, "document")) {
                ids.push(row["rid"].as_i64().unwrap_or_default());
            }
        }
        if !ids.is_empty() {
            db.query_with_params(
                "MATCH ()-[r:LINKS_TO]->() WHERE id(r) IN $ids DELETE r",
                gparams([("ids", json!(ids))]),
            )?;
            doc_links += ids.len();
        }
        let sources: Vec<String> = rows(
            db,
            "MATCH (d:ConnectorDoc {repository_id: $repo}) WHERE d.source_kind IS NOT NULL \
             RETURN d.id AS id",
            repo(),
        )?
        .iter()
        .filter(|row| scope.source(s(row, "id")))
        .map(|row| s(row, "id").to_string())
        .collect();
        if !sources.is_empty() {
            db.query_with_params(
                "MATCH (d:ConnectorDoc {repository_id: $repo}) \
                 WHERE d.source_kind IS NOT NULL AND d.id IN $ids DELETE d",
                gparams([("repo", json!(repository)), ("ids", json!(sources))]),
            )?;
        }
    }
    // Served-evidence and outcome lists naming the documents: strip the
    // forgotten entries; a record left empty goes entirely. (Lists of scalars
    // are valid property values; the lists are source-id strings.)
    let mut stripped = BTreeMap::new();
    for (legacy, label, property) in [("gf_served", "Served", "served"), ("gf_outcomes", "Outcome", "targets")] {
        let mut changed = 0;
        for node in nodes(db, label, repository, workspace)? {
            let list: Vec<String> = match &node[property] {
                Value::Array(items) => items.iter().filter_map(|i| i.as_str().map(str::to_string)).collect(),
                Value::String(text) => serde_json::from_str(text).unwrap_or_default(),
                _ => Vec::new(),
            };
            let kept: Vec<&String> = list.iter().filter(|i| !scope.source(i)).collect();
            if kept.len() == list.len() {
                continue;
            }
            changed += 1;
            let nid = node["_id"].as_i64().unwrap_or_default();
            if kept.is_empty() {
                detach_ids(db, label, &[nid])?;
            } else {
                db.query_with_params(
                    &format!("MATCH (n:$($label)) WHERE id(n) = $nid SET n.`{property}` = $kept"),
                    gparams([
                        ("label", json!(label)),
                        ("nid", json!(nid)),
                        ("kept", json!(kept)),
                    ]),
                )?;
            }
        }
        stripped.insert(legacy, changed);
    }
    // The dependency index follows the artifacts.
    let edge_rows = index_edges(db, workspace, repository, scope)?;
    let edge_ids: Vec<i64> = edge_rows.iter().map(|(rid, _, _)| *rid).collect();
    if !edge_ids.is_empty() {
        db.query_with_params(
            "MATCH ()-[r:DEPENDS_ON]->() WHERE id(r) IN $ids DELETE r",
            gparams([("ids", json!(edge_ids))]),
        )?;
    }
    // The index's artifact nodes of the forgotten documents and of the
    // derived artifacts that went with them: once edge-less, the entry
    // itself is residue.
    let endpoint_ids: Vec<i64> = edge_rows
        .iter()
        .flat_map(|(_, a, b)| [*a, *b])
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if !endpoint_ids.is_empty() {
        db.query_with_params(
            "MATCH (a:Artifact) WHERE id(a) IN $ids AND NOT (a)-[:DEPENDS_ON]-() \
             DETACH DELETE a",
            gparams([("ids", json!(endpoint_ids))]),
        )?;
    }
    let edges = edge_ids.len();
    let marker_ids = index_markers(db, workspace, repository, scope)?;
    let markers = detach_ids(db, "Stale", &marker_ids)?;
    // Entities nothing links to any more.
    db.query_with_params(
        "MATCH (e:Entity {repository_id: $repo}) \
         WHERE NOT (e)<-[:HAS_SUBJECT|HAS_OBJECT]-() DETACH DELETE e",
        repo(),
    )?;
    let mut removed = json!({"memories":memories.len(),"observer_notes":notes,"observer_reflections":reflections,"vectors":vectors,"document_reads":reads,
        "since_visible":visible_since,"anchors":anchors,"hints":hints,"doc_links":doc_links,"served_records":stripped["gf_served"],
        "outcome_records":stripped["gf_outcomes"],"dependency_edges":edges,"stale_markers":markers});
    if symbol_pins > 0 {
        removed["symbol_pins"] = json!(symbol_pins);
    }
    Ok(removed)
}

/// Remove the originals in scope: label-scoped walks over the connector's
/// labels and the project's originals. Forgotten documents keep a one-way
/// `ForgottenSource` tombstone so a later sync or import skips them.
pub fn erase(db: Graph, repository: &str, scope: &Scope) -> Result<Value> {
    let connector = scope.connector.as_str();
    let mut out = BTreeMap::new();
    if scope.whole() {
        for (name, label) in [
            ("documents", "ConnectorDoc"),
            ("versions", "ConnectorVersion"),
            ("staged", "ConnectorStage"),
            ("permissions", "SourcePermission"),
        ] {
            let n = detach(
                db,
                label,
                "n.connector_id = $connector",
                gparams([("connector", json!(connector))]),
            )?;
            if n > 0 {
                out.insert(name, n);
            }
        }
        let originals = detach(
            db,
            "OriginalDocument",
            "n.repository_id = $repo AND n.source_id STARTS WITH $prefix",
            gparams([("repo", json!(repository)), ("prefix", json!(scope.prefix()))]),
        )?;
        if originals > 0 {
            out.insert("originals", originals);
        }
        let n = detach(
            db,
            "Connector",
            "n.id = $connector",
            gparams([("connector", json!(connector))]),
        )?;
        out.insert("connector", n);
        return Ok(json!(out));
    }
    for (name, label, key) in [
        ("documents", "ConnectorDoc", "id"),
        ("versions", "ConnectorVersion", "id"),
        ("staged", "ConnectorStage", "id"),
        ("permissions", "SourcePermission", "source_id"),
    ] {
        let found = walk(
            db,
            label,
            "n.connector_id = $connector",
            key,
            gparams([("connector", json!(connector))]),
            |v| scope.document(v),
            true,
        )?;
        out.insert(name, found);
    }
    let originals = walk(
        db,
        "OriginalDocument",
        "n.repository_id = $repo",
        "source_id",
        gparams([("repo", json!(repository))]),
        |v| scope.source(v),
        true,
    )?;
    out.insert("originals", originals);
    let stamp = now();
    for hash in scope.hashes() {
        db.query_with_params(
            "MERGE (f:ForgottenSource {connector_id: $connector, id_hash: $hash}) \
             ON CREATE SET f.created_at = $at",
            gparams([
                ("connector", json!(connector)),
                ("hash", json!(hash)),
                ("at", json!(stamp)),
            ]),
        )?;
    }
    Ok(json!(out))
}

/// Documents of the project the scope names, still visible to serving
/// (connector documents scoped through their connector).
fn visible_documents(db: Graph, repository: &str, scope: &Scope) -> Result<usize> {
    let connectors = connectors_of(db, repository)?;
    Ok(rows(
        db,
        "MATCH (d:ConnectorDoc) WHERE d.connector_id IN $connectors RETURN d.id AS id",
        gparams([("connectors", json!(connectors))]),
    )?
    .iter()
    .filter(|d| scope.source(s(d, "id")))
    .count())
}

fn empty(scan: &Value) -> bool {
    scan["matches"].as_array().is_some_and(Vec::is_empty)
}

/// Verify a forget request inside its transaction and store the receipt.
/// `started` is when the request began; artifacts dated after it are new work.
#[allow(clippy::too_many_arguments)]
pub fn receipt(
    db: Graph,
    sql: &rusqlite::Connection,
    p: &Principal,
    repository: &Value,
    operation: &str,
    plan: &Plan,
    started: &str,
    forgotten: Value,
    erased: Value,
) -> Result<Value> {
    policy_receipt(db, sql, p, repository, operation, plan, started, forgotten, erased, None)
}

/// [`receipt`] for a deletion a retention policy chose: the receipt also
/// carries the policy and its digest.
#[allow(clippy::too_many_arguments)]
pub fn policy_receipt(
    db: Graph,
    sql: &rusqlite::Connection,
    p: &Principal,
    repository: &Value,
    operation: &str,
    plan: &Plan,
    started: &str,
    forgotten: Value,
    erased: Value,
    policy: Option<&Value>,
) -> Result<Value> {
    let identifier = s(repository, "id");
    let scope = &plan.scope;
    // Verified deletion covers both stores: connector data is graph-resident,
    // the observer, overlay and product tables are still SQL.
    let mut remaining = source_rows(db, &p.workspace, identifier, scope)?;
    merge_counts(
        &mut remaining,
        legacy::source_rows(sql, &p.workspace, identifier, scope)?,
    );
    let mut tainted = since_visible(db, identifier, plan.since(), Some(started))?;
    merge_counts(
        &mut tainted,
        legacy::since_visible(sql, identifier, plan.since(), Some(started))?,
    );
    let labels = catalog(db)?;
    let tables = legacy::tables(sql)?;
    let ids = if scope.whole() {
        merge_scans(
            references(db, &labels, &[scope.prefix()], (identifier, &p.workspace), &[], false)?,
            legacy::references(sql, &tables, &[scope.prefix()], (identifier, &p.workspace), &[], false)?,
        )
    } else {
        let existing: BTreeSet<String> = rows(
            db,
            "MATCH (d:ConnectorDoc {connector_id: $connector}) RETURN d.id AS id",
            gparams([("connector", json!(scope.connector))]),
        )?
        .iter()
        .map(|r| format!("{}{}", scope.prefix(), s(r, "id")))
        .collect();
        merge_scans(
            document_references(db, &labels, scope, &existing, (identifier, &p.workspace))?,
            legacy::document_references(sql, &tables, scope, &existing, (identifier, &p.workspace))?,
        )
    };
    let text = verbatim(db, &labels, &plan.fingerprints, identifier)?;
    let sql_text = legacy::verbatim(sql, &tables, &plan.fingerprints, identifier)?;
    let mut text_matches = text["matches"].as_array().cloned().unwrap_or_default();
    text_matches.extend(sql_text["matches"].as_array().cloned().unwrap_or_default());
    let text = json!({"fingerprints":text["fingerprints"],
        "tables_scanned":text["tables_scanned"].as_u64().unwrap_or(0)+sql_text["tables_scanned"].as_u64().unwrap_or(0),
        "matches":text_matches});
    let visible = visible_documents(db, identifier, scope)?;
    let snapshot = state(db, &p.workspace, identifier, Some(sql))?;
    // External cleanups are queued in the SQL store (projection.rs).
    let cleanups = legacy::external(sql, identifier)?;
    let clean = remaining.values().chain(tainted.values()).all(|n| *n == 0)
        && empty(&ids)
        && empty(&text)
        && visible == 0;
    let pending = cleanups.iter().any(|c| c["state"] != "complete");
    let recompute: Vec<&str> = plan
        .visible_since
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(k, _)| *k)
        .collect();
    let mut receipt = json!({
        "contract":CONTRACT,"operation":operation,"workspace_id":p.workspace,
        "repository_id":identifier,"target":scope.connector,"scope":scope.describe(),
        "requested_by":p.credential,"deleted_at":now(),
        "sources":{"count":plan.sources,"digest":plan.sources_digest,"identifiers_retained":false,
            "document_hashes":scope.hashes()},
        "removed":{"rows":plan.rows,"since_visible":plan.visible_since,"derived":forgotten,"originals":erased},
        "remaining":{"rows":remaining,"since_visible":tainted},
        "recomputed_on_demand":recompute,
        "residue":{"source_ids":ids,"verbatim_text":json!({"texts":plan.texts,"sampled":text["fingerprints"],
            "tables_scanned":text["tables_scanned"],"matches":text["matches"]})},
        "serving_probe":{"visible_documents":visible},
        "state":{"contract":STATE_CONTRACT,"digest":snapshot["digest"],
            "tables":snapshot["tables"].as_object().map_or(0, |t| t.len())},
        "reimport_blocked":!scope.whole(),
        "external_cleanup":cleanups,
        "verified_local":clean,"external_pending":pending,"complete":clean && !pending,
        "not_covered":NOT_COVERED,"storage":{"engine":"cypherlite","durability":"wal+snapshot",
            "secure_delete":legacy::secure_delete(sql)?}
    });
    if let Some(policy) = policy {
        receipt["policy"] = policy.clone();
        receipt["policy_digest"] = json!(digest(policy));
    }
    append(db, sql, &p.workspace, identifier, operation, &scope.connector, receipt)
}

/// Merge two `{"tables_scanned": n, "matches": [...]}` scans (graph labels
/// and SQL legacy tables).
fn merge_scans(graph: Value, sql: Value) -> Value {
    let mut matches = graph["matches"].as_array().cloned().unwrap_or_default();
    matches.extend(sql["matches"].as_array().cloned().unwrap_or_default());
    json!({
        "tables_scanned": graph["tables_scanned"].as_u64().unwrap_or(0)+sql["tables_scanned"].as_u64().unwrap_or(0),
        "matches": matches,
    })
}

/// One-way name of a removed node key (a memory id, a pruned object).
pub fn key_hash(operation: &str, key: &str) -> String {
    digest(&json!({"operation":operation,"key":key}))
}

/// Receipt for a deletion of named nodes rather than a source scope — a
/// memory (`memory.delete`), graph by-products past their retention
/// (`retention.graph`) or unreferenced storage (`storage.prune`). Removed
/// nodes are counted per label; their keys are kept only as one-way hashes
/// (`removed.keys`). `remaining` is the caller's re-count of removed keys
/// still present, per label. With `scan`, every label is searched for the
/// keys themselves: matches in work history are reported as such, any other
/// match is residue and the receipt is not `verified_local`.
#[allow(clippy::too_many_arguments)]
pub fn rows_receipt(
    db: Graph,
    sql: &rusqlite::Connection,
    p: &Principal,
    repository: &str,
    operation: &str,
    target: &str,
    policy: Value,
    removed: Value,
    keys: &[String],
    remaining: Value,
    scan: bool,
) -> Result<Value> {
    let mut hashes: Vec<String> = keys.iter().map(|k| key_hash(operation, k)).collect();
    hashes.sort();
    hashes.dedup();
    // Large prunes keep one digest over the hashes instead of the list.
    const LISTED_HASHES: usize = 1000;
    let hashes_digest = digest(&json!(hashes));
    let listed = if hashes.len() <= LISTED_HASHES {
        json!(hashes)
    } else {
        json!([])
    };
    let (residue, history) = if scan && !keys.is_empty() {
        let labels = catalog(db)?;
        let patterns: Vec<String> = keys.to_vec();
        let retained = retained(db)?;
        let found = references(
            db,
            &labels,
            &patterns,
            (repository, &p.workspace),
            &retained,
            false,
        )?;
        let history_labels = history(db, Some(sql))?;
        let (history, other): (Vec<Value>, Vec<Value>) = found["matches"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .partition(|m| {
                history_labels.contains(&s(m, "table")) || history_labels.contains(&known_label_lossy(s(m, "table")).as_str())
            });
        (
            json!({"tables_scanned":found["tables_scanned"],"matches":other}),
            json!(history),
        )
    } else {
        (json!({"tables_scanned":0,"matches":[]}), json!([]))
    };
    let clean = remaining
        .as_object()
        .is_none_or(|counts| counts.values().all(|n| n.as_u64() == Some(0)))
        && residue["matches"].as_array().is_none_or(Vec::is_empty);
    let receipt = json!({
        "contract":CONTRACT,"operation":operation,"workspace_id":p.workspace,
        "repository_id":repository,"target":target,"requested_by":p.credential,"deleted_at":now(),
        "policy":policy,"policy_digest":digest(&policy),
        "removed":{"rows":removed,"keys":{"count":hashes.len(),"hashes":listed,"hashes_digest":hashes_digest,
            "hashes_listed":hashes.len() <= LISTED_HASHES,"identifiers_retained":false}},
        "remaining":remaining,
        "residue":{"references":residue,"history_references":history},
        "verified_local":clean,"external_pending":false,"complete":clean,
        "not_covered":NOT_COVERED,"storage":{"engine":"cypherlite","durability":"wal+snapshot"}
    });
    append(db, sql, &p.workspace, repository, operation, target, receipt)
}

/// The label a legacy table name maps to (lossy helper for receipt fields,
/// which report legacy names).
fn known_label_lossy(table: &str) -> String {
    LABELS
        .iter()
        .find(|(_, t, _)| *t == table)
        .map_or_else(|| table.to_string(), |(l, _, _)| l.to_string())
}

/// Replace identifiers minted per store and clock readings, so two stores
/// that hold the same content digest equally.
fn mask(value: &str) -> String {
    use std::sync::OnceLock;
    static IDS: OnceLock<regex::Regex> = OnceLock::new();
    static CLOCK: OnceLock<regex::Regex> = OnceLock::new();
    let ids = IDS.get_or_init(|| regex::Regex::new(r"\b[0-9a-f]{32}\b").unwrap());
    let clock = CLOCK.get_or_init(|| {
        regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})")
            .unwrap()
    });
    clock
        .replace_all(&ids.replace_all(value, "<id>"), "<time>")
        .into_owned()
}

/// Canonical digest of everything a store holds for one project: originals,
/// derived memory and indexes (`graphicle.state.v2`). History,
/// invalidation-timed caches and folder locations are excluded, and so are
/// labels with no nodes in the project's scope; identifiers and capability
/// digests minted per store, clock readings and sync counters are masked. Two
/// stores digest equally when they hold the same content, whichever edition
/// created them.
pub fn state(
    db: Graph,
    workspace: &str,
    repository: &str,
    sql: Option<&rusqlite::Connection>,
) -> Result<Value> {
    let mut tables_out = BTreeMap::new();
    let history = history(db, sql)?;
    // The label catalog is the walk list: history, caches, locations and the
    // receipts label are excluded, as are labels this store does not carry.
    for label in catalog(db)? {
        if label == RECEIPTS
            || history.contains(&label.as_str())
            || CACHES.contains(&label.as_str())
            || LOCATIONS.contains(&label.as_str())
            || scoping(&label) == Scoped::Global
        {
            continue;
        }
        let mut canonical = Vec::new();
        for node in nodes(db, &label, repository, workspace)? {
            // The model cache's dependency-index entries belong to the cache.
            if ["Artifact", "Stale"].contains(&label.as_str())
                && node["kind"] == crate::invalidation::ANSWER
            {
                continue;
            }
            let mut props = serde_json::Map::new();
            for (key, value) in node.as_object().into_iter().flatten() {
                // Engine metadata, request keys (they digest the whole
                // snapshot, including per-store memory ids; the nodes'
                // content is compared instead), per-store capability
                // digests and sync counters never take part.
                if key.starts_with('_')
                    || key == "request_hash"
                    || MINTED_DIGESTS.contains(&key.as_str())
                    || COUNTERS.contains(&(label.as_str(), key.as_str()))
                {
                    continue;
                }
                props.insert(key.clone(), value.clone());
            }
            canonical.push(mask(&Value::Object(props).to_string()));
        }
        // An empty label says nothing about content (v2): whether a store has
        // an edition's labels at all must not change its digest.
        if canonical.is_empty() {
            continue;
        }
        canonical.sort();
        tables_out.insert(
            table_of(&label).to_string(),
            json!({"rows":canonical.len(),"digest":graphicle_memory::text::sha256(canonical.join("\n").as_bytes())}),
        );
    }
    // Relationships carrying content (edge properties), scoped by their
    // start node's project: the projection's provenance digests with the
    // symbols it connects.
    let mut rels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows(
        db,
        "MATCH (a)-[r]->(b) WHERE a.repository_id = $repo \
         RETURN type(r) AS t, properties(r) AS p, a.id AS from_id, b.id AS to_id",
        gparams([("repo", json!(repository))]),
    )? {
        let entry = mask(
            &json!({"from":row["from_id"],"to":row["to_id"],"properties":row["p"]}).to_string(),
        );
        rels.entry(s(&row, "t").to_string()).or_default().push(entry);
    }
    for (rel_type, mut entries) in rels {
        if entries.is_empty() {
            continue;
        }
        entries.sort();
        tables_out.insert(
            rel_type,
            json!({"rows":entries.len(),"digest":graphicle_memory::text::sha256(entries.join("\n").as_bytes())}),
        );
    }
    Ok(
        json!({"contract":STATE_CONTRACT,"digest":digest(&json!(tables_out)),"tables":tables_out,
        "excluded":{"history":history,"caches":CACHES,"locations":LOCATIONS,"empty_tables":true},
        "masked":["identifiers minted per store","capability digests minted per store","clock readings","sync counters"]}),
    )
}

/// `deletion.state`: the canonical digest of one project, for comparing a
/// store after forgetting with one rebuilt without the documents.
pub fn state_api(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    // A store mid-migration can hold the project's content only in SQLite
    // (nothing has been written to the graph store yet): digest there.
    if graph_store(store)?.read(|g| {
        one(
            g,
            "MATCH (n) RETURN 1 AS x LIMIT 1",
            store_graph::Params::new(),
        )
        .map(|r| r.is_none())
    })? {
        return legacy::state_api(store, workspace, token, args);
    }
    store.authorized_read(workspace, token, "admin", |db, p| {
        let project = repo(db, p, string(args, "repository_id", 100)?)?;
        let identifier = s(&project, "id").to_string();
        let workspace_id = p.workspace.clone();
        let mut graph_state =
            graph_store(store)?.read(|g| state(g, &workspace_id, &identifier, Some(db)))?;
        // The SQL side still holds the domains no migration node owns yet
        // (observer notes, hints, served/outcomes, backends, …) and any table
        // an edition adds. Digest those alongside the graph's tables so
        // residue in either store shows up, and an undeclared table cannot
        // hide its rows from the comparison.
        let sql_state = legacy::state(db, &workspace_id, &identifier)?;
        if let Some(sql_tables) = sql_state["tables"].as_object() {
            let mut merged = graph_state["tables"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            for (name, entry) in sql_tables {
                if merged.contains_key(name) || entry["rows"].as_u64().unwrap_or(0) == 0 {
                    continue;
                }
                merged.insert(name.clone(), entry.clone());
            }
            graph_state["tables"] = json!(merged);
            graph_state["digest"] = json!(digest(&graph_state["tables"]));
        }
        Ok(graph_state)
    })
}

/// Dependency-index entries and episode labels of a deleted project. They
/// are scoped by workspace, so the project's own removal does not reach them.
pub fn forget_project_references(
    db: Graph,
    workspace: &str,
    repository: &str,
    connectors: &[String],
) -> Result<Value> {
    let mut edges = 0;
    let mut stale = 0;
    let mut artifacts = 0;
    for prefix in project_patterns(repository, connectors) {
        let found = rows(
            db,
            "MATCH (a:Artifact {workspace: $ws})-[r:DEPENDS_ON]->(src:Artifact) \
             WHERE a.id STARTS WITH $prefix OR src.id STARTS WITH $prefix RETURN id(r) AS rid",
            gparams([("ws", json!(workspace)), ("prefix", json!(prefix))]),
        )?;
        let ids: Vec<i64> = found.iter().filter_map(|r| r["rid"].as_i64()).collect();
        if !ids.is_empty() {
            db.query_with_params(
                "MATCH ()-[r:DEPENDS_ON]->() WHERE id(r) IN $ids DELETE r",
                gparams([("ids", json!(ids))]),
            )?;
            edges += ids.len();
        }
        // The project's own artifacts (its files and chunks) are
        // workspace-scoped nodes keyed `<repository>:<path>`: they go with
        // the project, their edges already gone.
        let mut owned = Vec::new();
        for row in rows(
            db,
            "MATCH (a:Artifact {workspace: $ws}) WHERE a.id STARTS WITH $prefix RETURN id(a) AS nid",
            gparams([("ws", json!(workspace)), ("prefix", json!(prefix.clone()))]),
        )? {
            owned.push(row["nid"].as_i64().unwrap_or_default());
        }
        artifacts += detach_ids(db, "Artifact", &owned)?;
        let mut ids = Vec::new();
        for row in rows(
            db,
            "MATCH (s:Stale {workspace: $ws}) WHERE s.id STARTS WITH $prefix RETURN id(s) AS nid",
            gparams([("ws", json!(workspace)), ("prefix", json!(prefix))]),
        )? {
            ids.push(row["nid"].as_i64().unwrap_or_default());
        }
        stale += detach_ids(db, "Stale", &ids)?;
    }
    // The names (commit, snapshot) of its recorded episodes (every projected
    // project has them, and they named its id).
    let labels = detach(
        db,
        "EpisodeLabel",
        "n.workspace = $ws AND n.repo = $repo",
        gparams([("ws", json!(workspace)), ("repo", json!(repository))]),
    )?;
    Ok(json!({"dependency_edges":edges,"dependency_nodes":artifacts,"stale_markers":stale,"episode_labels":labels}))
}

fn project_patterns(repository: &str, connectors: &[String]) -> Vec<String> {
    std::iter::once(format!("{repository}:"))
        .chain(connectors.iter().map(|c| format!("connector:{c}:")))
        .collect()
}

/// Nodes still keyed to a deleted project or to one of its connectors.
fn project_rows(
    db: Graph,
    labels: &[String],
    repository: &str,
    connectors: &[String],
) -> Result<Value> {
    let mut checked = 0;
    let mut matches = Vec::new();
    let retained = retained(db)?;
    for label in labels
        .iter()
        .filter(|l| !retained.contains(&l.as_str()) && !retained.contains(&table_of(l)))
    {
        match scoping(label) {
            Scoped::Repository => {
                checked += 1;
                let n = counted(
                    db,
                    "MATCH (n:$($label) {repository_id: $repo}) RETURN count(n) AS n",
                    gparams([("label", json!(label)), ("repo", json!(repository))]),
                )?;
                if n > 0 {
                    matches.push(json!({"table":table_of(label),"column":"repository_id","rows":n}));
                }
            }
            Scoped::RepositoryIn(property) => {
                checked += 1;
                let n = counted(
                    db,
                    &format!("MATCH (n:$($label)) WHERE n.`{property}` = $repo RETURN count(n) AS n"),
                    gparams([("label", json!(label)), ("repo", json!(repository))]),
                )?;
                if n > 0 {
                    matches.push(json!({"table":table_of(label),"column":property,"rows":n}));
                }
            }
            Scoped::Cross => {
                checked += 1;
                let n = counted(
                    db,
                    "MATCH (n:$($label)) WHERE n.source_repository_id = $repo OR n.target_repository_id = $repo RETURN count(n) AS n",
                    gparams([("label", json!(label)), ("repo", json!(repository))]),
                )?;
                if n > 0 {
                    matches.push(json!({"table":table_of(label),"column":"source_repository_id","rows":n}));
                }
            }
            Scoped::Connector => {
                checked += 1;
                let n = counted(
                    db,
                    "MATCH (n:$($label)) WHERE n.connector_id IN $connectors RETURN count(n) AS n",
                    gparams([("label", json!(label)), ("connectors", json!(connectors))]),
                )?;
                if n > 0 {
                    matches.push(json!({"table":table_of(label),"column":"connector_id","rows":n}));
                }
            }
            _ => {}
        }
    }
    Ok(json!({"columns_checked":checked,"matches":matches}))
}

/// The receipt of `project.delete` for `repository`. `removed` holds what
/// the deletion removed besides the project's own nodes: `references`
/// (dependency-index entries) and, when the project had any,
/// `pr_scratch_projects` (the PR scratch projects deleted with it, each with
/// its own receipt).
pub fn project_receipt(
    db: Graph,
    sql: &rusqlite::Connection,
    p: &Principal,
    repository: &str,
    connectors: &[String],
    removed: Value,
) -> Result<Value> {
    let labels = catalog(db)?;
    let remaining = project_rows(db, &labels, repository, connectors)?;
    // The SQL side: the project row's delete cascaded, but a table without
    // the cascade is residue and is reported, not hidden.
    let mut sql_checked = 0;
    let mut sql_matches = Vec::new();
    let tables = legacy::tables(sql)?;
    for table in &tables {
        if !table.columns.contains("repository_id") {
            continue;
        }
        sql_checked += 1;
        let n: i64 = sql.query_row(
            &format!(
                "SELECT count(*) FROM \"{}\" WHERE repository_id=?",
                table.name
            ),
            [repository],
            |r| r.get(0),
        )?;
        if n > 0 {
            sql_matches.push(json!({"table":table.name,"column":"repository_id","rows":n}));
        }
    }
    let mut remaining_matches = remaining["matches"].as_array().cloned().unwrap_or_default();
    remaining_matches.extend(sql_matches);
    let remaining = json!({
        "columns_checked":remaining["columns_checked"].as_u64().unwrap_or(0)+sql_checked,
        "matches":remaining_matches,
    });
    let patterns = std::iter::once(repository.to_string())
        .chain(connectors.iter().map(|c| format!("connector:{c}:")))
        .collect::<Vec<_>>();
    let retained = retained(db)?;
    let retained_tables: Vec<&str> = retained.iter().map(|l| table_of(l)).collect();
    let ids = merge_scans(
        references(db, &labels, &patterns, (repository, &p.workspace), &retained, true)?,
        legacy::references(sql, &tables, &patterns, (repository, &p.workspace), &retained_tables, true)?,
    );
    // External cleanups are queued in the SQL store (projection.rs).
    let cleanups = legacy::external(sql, repository)?;
    let clean = empty(&remaining) && empty(&ids);
    let pending = cleanups.iter().any(|c| c["state"] != "complete");
    let receipt = json!({
        "contract":CONTRACT,"operation":"project.delete","workspace_id":p.workspace,
        "repository_id":repository,"target":repository,"connectors":connectors,"requested_by":p.credential,"deleted_at":now(),
        "removed":removed,
        "remaining":{"project_rows":remaining},
        "residue":{"source_ids":ids,"retained_by_design":retained},
        "external_cleanup":cleanups,
        "verified_local":clean,"external_pending":pending,"complete":clean && !pending,
        "not_covered":NOT_COVERED,"storage":{"engine":"cypherlite","durability":"wal+snapshot"}
    });
    append(db, sql, &p.workspace, repository, "project.delete", repository, receipt)
}

/// Link a receipt into the workspace chain and store it: the §2.5
/// `(:DeletionReceipt)-[:NEXT]->(:DeletionReceipt)` hash chain, one chain per
/// workspace. The sequence number comes from the chain itself (the current
/// head's sequence + 1), `previous` carries the head's digest, and the
/// receipt's own digest covers both — rewriting any stored receipt breaks the
/// chain verifiably ([`chain`]). Receipts are plain properties with no
/// project reference, so they outlive a deleted project. The body is the
/// canonical JSON, stringified (the engine stores no map-valued properties).
fn append(
    db: Graph,
    sql: &rusqlite::Connection,
    workspace: &str,
    repository: &str,
    operation: &str,
    target: &str,
    mut receipt: Value,
) -> Result<Value> {
    let last = one(
        db,
        "MATCH (r:DeletionReceipt {workspace_id: $ws}) \
         RETURN r.sequence AS sequence, r.digest AS digest ORDER BY r.sequence DESC LIMIT 1",
        gparams([("ws", json!(workspace))]),
    )?;
    let sql_last: i64 = sql
        .query_row(
            "SELECT coalesce(max(sequence),0) FROM am_deletion_receipts WHERE workspace_id=?",
            [workspace],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let sequence = last
        .as_ref()
        .and_then(|r| r["sequence"].as_i64())
        .unwrap_or(0)
        .max(sql_last)
        + 1;
    receipt["id"] = json!(id());
    receipt["sequence"] = json!(sequence);
    receipt["previous"] = last.map_or(Value::Null, |r| r["digest"].clone());
    let hash = digest(&receipt);
    let body = graphicle_memory::text::canonical(&receipt);
    db.query_with_params(
        "CREATE (r:DeletionReceipt {id: $id, workspace_id: $ws, sequence: $seq, \
         repository_id: $repo, operation: $op, target: $target, body: $body, digest: $digest, \
         previous: $previous, created_at: $at})",
        gparams([
            ("id", s(&receipt, "id").into()),
            ("ws", json!(workspace)),
            ("seq", json!(sequence)),
            ("repo", json!(repository)),
            ("op", json!(operation)),
            ("target", json!(target)),
            ("body", json!(&body)),
            ("digest", json!(hash)),
            ("previous", receipt["previous"].clone()),
            ("at", json!(now())),
        ]),
    )?;
    if sequence > 1 {
        db.query_with_params(
            "MATCH (p:DeletionReceipt {workspace_id: $ws, sequence: $prev}) \
             MATCH (n:DeletionReceipt {workspace_id: $ws, sequence: $seq}) \
             CREATE (p)-[:NEXT]->(n)",
            gparams([
                ("ws", json!(workspace)),
                ("prev", json!(sequence - 1)),
                ("seq", json!(sequence)),
            ]),
        )?;
    }
    // The SQL mirror of the chain (am_deletion_receipts): same body, so a
    // rewrite of either store breaks verification.
    sql.execute(
        "INSERT INTO am_deletion_receipts(id,workspace_id,sequence,repository_id,operation,target,body,digest,previous,created_at) \
         VALUES(?,?,?,?,?,?,?,?,?,?)",
        rusqlite::params![
            s(&receipt, "id"),
            workspace,
            sequence,
            repository,
            operation,
            target,
            body.as_str(),
            hash.as_str(),
            receipt["previous"].as_str(),
            now()
        ],
    )?;
    receipt["digest"] = json!(hash);
    Ok(receipt)
}

/// Recompute every digest and link of the workspace's receipt chain, in the
/// graph store and in its SQL mirror (`am_deletion_receipts`): a rewrite of
/// either breaks the chain.
fn chain(db: Graph, sql: &rusqlite::Connection, workspace: &str) -> Result<Value> {
    let sql = chain_sql(sql, workspace)?;
    let graph = chain_graph(db, workspace)?;
    let intact = graph["intact"] == true
        && sql["intact"] == true
        && sql["length"] == graph["length"];
    Ok(json!({"length":graph["length"],"intact":intact,"head":graph["head"]}))
}

/// The SQL mirror's half of [`chain`].
fn chain_sql(db: &rusqlite::Connection, workspace: &str) -> Result<Value> {
    let mut previous = Value::Null;
    let mut intact = true;
    let mut length = 0i64;
    for row in crate::store::rows(
        db,
        "SELECT sequence,body,digest FROM am_deletion_receipts WHERE workspace_id=? ORDER BY sequence",
        [workspace],
    )? {
        length += 1;
        let body: Value = serde_json::from_str(s(&row, "body"))?;
        intact &= row["sequence"] == json!(length)
            && body["sequence"] == row["sequence"]
            && body["previous"] == previous
            && digest(&body) == s(&row, "digest");
        previous = row["digest"].clone();
    }
    Ok(json!({"length":length,"intact":intact,"head":previous}))
}

/// The graph store's half of [`chain`].
fn chain_graph(db: Graph, workspace: &str) -> Result<Value> {
    let mut previous = Value::Null;
    let mut intact = true;
    let mut length = 0i64;
    for row in rows(
        db,
        "MATCH (r:DeletionReceipt {workspace_id: $ws}) \
         RETURN r.sequence AS sequence, r.body AS body, r.digest AS digest ORDER BY r.sequence",
        gparams([("ws", json!(workspace))]),
    )? {
        length += 1;
        let body: Value = serde_json::from_str(s(&row, "body"))?;
        intact &= row["sequence"] == json!(length)
            && body["sequence"] == row["sequence"]
            && body["previous"] == previous
            && digest(&body) == s(&row, "digest");
        previous = row["digest"].clone();
    }
    // The NEXT linked list must connect every consecutive pair.
    let links: BTreeSet<(i64, i64)> = rows(
        db,
        "MATCH (a:DeletionReceipt {workspace_id: $ws})-[:NEXT]->(b:DeletionReceipt) \
         RETURN a.sequence AS from, b.sequence AS to",
        gparams([("ws", json!(workspace))]),
    )?
    .iter()
    .filter_map(|r| Some((r["from"].as_i64()?, r["to"].as_i64()?)))
    .collect();
    intact &= (2..=length).all(|seq| links.contains(&(seq - 1, seq)));
    Ok(json!({"length":length,"intact":intact,"head":previous}))
}

/// Flush deleted nodes' old images into a snapshot after the deletion
/// commits, so the write-ahead log no longer carries them; the legacy SQLite
/// half's WAL is truncated too, while the store is dual.
pub fn checkpoint(store: &Store) -> Result<Value> {
    graph_store(store)?.checkpoint()?;
    let mut out = legacy::checkpoint(store)?;
    out["snapshot_checkpoint"] = json!("ok");
    Ok(out)
}

/// Re-check a stored receipt against the current store and its chain.
/// Source rows, id references, serving and external cleanup are re-checked;
/// verbatim text is checked only at deletion time, since the receipt keeps no
/// excerpt of what it deleted. A document receipt keeps only one-way hashes,
/// so originals and historical copies are re-checked by their id hashes.
/// A failed deletion-time text scan stays failed: its discarded excerpts
/// cannot establish that later edits removed the known residue.
pub fn verify(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    // A receipt lives where its deletion wrote it: the graph store, or the
    // legacy SQL store of a store mid-migration.
    let receipt_id = string(args, "receipt_id", 100)?;
    let on_graph = graph_store(store)?.read(|g| {
        one(
            g,
            "MATCH (r:DeletionReceipt {workspace_id: $ws, id: $id}) RETURN 1 AS x",
            gparams([("ws", json!(workspace)), ("id", json!(receipt_id))]),
        )
        .map(|r| r.is_some())
    })?;
    if !on_graph {
        return legacy::verify(store, workspace, token, args);
    }
    store.authorized_read(workspace, token, "admin", |db, p| {
        let receipt_id = string(args, "receipt_id", 100)?;
        let graph = graph_store(store)?;
        graph.read(|g| {
            let row = one(
                g,
                "MATCH (r:DeletionReceipt {workspace_id: $ws, id: $id}) \
                 RETURN r.body AS body, r.digest AS digest",
                gparams([("ws", json!(workspace)), ("id", json!(receipt_id))]),
            )?
            .ok_or_else(|| Error::bad("Deletion receipt unavailable"))?;
            let mut receipt: Value = serde_json::from_str(s(&row, "body"))?;
            receipt["digest"] = row["digest"].clone();
            let repository = s(&receipt, "repository_id").to_string();
            let labels = catalog(g)?;
            let project = repo(db, p, &repository).ok();
            let current = match s(&receipt, "operation") {
                "connector.delete" => {
                    let scope = Scope::connector(s(&receipt, "target"));
                    let mut remaining = source_rows(g, workspace, &repository, &scope)?;
                    merge_counts(
                        &mut remaining,
                        legacy::source_rows(db, workspace, &repository, &scope)?,
                    );
                    let tables = legacy::tables(db)?;
                    let ids = merge_scans(
                        references(g, &labels, &[scope.prefix()], (&repository, workspace), &[], false)?,
                        legacy::references(db, &tables, &[scope.prefix()], (&repository, workspace), &[], false)?,
                    );
                    // Caches refill with unrelated work, so only the deleted
                    // sources' own rows and references are re-checked.
                    let visible = if project.is_some() {
                        Some(visible_documents(g, &repository, &scope)?)
                    } else {
                        None
                    };
                    let text_clean = empty(&receipt["residue"]["verbatim_text"]);
                    let clean = remaining.values().all(|n| *n == 0)
                        && empty(&ids)
                        && visible.unwrap_or(0) == 0
                        && text_clean;
                    json!({"verified_local":clean,"rows":remaining,"source_ids":ids,
                        "verbatim_text":{"recheck":"at_deletion_time","clean":text_clean},
                        "serving_probe":visible.map_or(json!("project_deleted"),|n|json!({"visible_documents":n}))})
                }
                "source.forget" | "retention.forget" => {
                    let connector = s(&receipt, "target");
                    let hashes: BTreeSet<&str> = receipt["sources"]["document_hashes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    let mut reappeared = BTreeMap::new();
                    for (name, label, key) in [
                        ("documents", "ConnectorDoc", "id"),
                        ("versions", "ConnectorVersion", "id"),
                        ("staged", "ConnectorStage", "id"),
                        ("permissions", "SourcePermission", "source_id"),
                    ] {
                        let count = rows(
                            g,
                            &format!("MATCH (d:{label} {{connector_id: $connector}}) RETURN d.`{key}` AS id"),
                            gparams([("connector", json!(connector))]),
                        )?
                        .iter()
                        .filter(|d| hashes.contains(tombstone(connector, s(d, "id")).as_str()))
                        .count();
                        reappeared.insert(name, count);
                    }
                    let prefix = format!("connector:{connector}:");
                    let originals = rows(
                        g,
                        "MATCH (o:OriginalDocument {repository_id: $repo}) RETURN o.source_id AS source_id",
                        gparams([("repo", json!(repository))]),
                    )?
                    .iter()
                    .filter(|d| {
                        s(d, "source_id")
                            .strip_prefix(&prefix)
                            .is_some_and(|id| hashes.contains(tombstone(connector, id).as_str()))
                    })
                    .count();
                    reappeared.insert("originals", originals);
                    // Historical rows of the SQL half count too: a store
                    // mid-migration can hold the documents there.
                    for (name, table, key) in [
                        ("documents", "am_connector_docs", "id"),
                        ("versions", "am_connector_versions", "id"),
                        ("staged", "am_connector_stage", "id"),
                        ("permissions", "am_source_permissions", "source_id"),
                    ] {
                        let count = crate::store::rows(
                            db,
                            &format!("SELECT {key} AS id FROM {table} WHERE connector_id=?"),
                            [connector],
                        )?
                        .iter()
                        .filter(|d| hashes.contains(tombstone(connector, s(d, "id")).as_str()))
                        .count();
                        *reappeared.entry(name).or_insert(0) += count;
                    }
                    let sql_originals = crate::store::rows(
                        db,
                        "SELECT source_id FROM am_original_documents WHERE repository_id=?",
                        [&repository],
                    )?
                    .iter()
                    .filter(|d| {
                        s(d, "source_id")
                            .strip_prefix(&prefix)
                            .is_some_and(|id| hashes.contains(tombstone(connector, id).as_str()))
                    })
                    .count();
                    *reappeared.entry("originals").or_insert(0) += sql_originals;
                    let tombstones = rows(
                        g,
                        "MATCH (f:ForgottenSource {connector_id: $connector}) RETURN f.id_hash AS id_hash",
                        gparams([("connector", json!(connector))]),
                    )?
                    .iter()
                    .filter(|r| hashes.contains(s(r, "id_hash")))
                    .count();
                    let connector_present = one(
                        g,
                        "MATCH (c:Connector {id: $connector}) RETURN c.id AS id",
                        gparams([("connector", json!(connector))]),
                    )?
                    .is_some();
                    let clean = receipt["verified_local"] == true
                        && reappeared.values().all(|n| *n == 0)
                        && (!connector_present || tombstones == hashes.len());
                    json!({"verified_local":clean,"reappeared_documents":reappeared["documents"],"reappeared_rows":reappeared,
                        "deletion_time_checks_passed":receipt["verified_local"],
                        "tombstones":tombstones,"connector_present":connector_present})
                }
                "project.delete" => {
                    let connectors: Vec<String> = receipt["connectors"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c.as_str().map(str::to_string))
                        .collect();
                    let remaining = project_rows(g, &labels, &repository, &connectors)?;
                    let mut sql_matches = remaining["matches"].as_array().cloned().unwrap_or_default();
                    let mut sql_checked = remaining["columns_checked"].as_u64().unwrap_or(0);
                    let tables = legacy::tables(db)?;
                    for table in &tables {
                        if !table.columns.contains("repository_id") {
                            continue;
                        }
                        sql_checked += 1;
                        let n: i64 = db.query_row(
                            &format!("SELECT count(*) FROM \"{}\" WHERE repository_id=?", table.name),
                            [&repository],
                            |r| r.get(0),
                        )?;
                        if n > 0 {
                            sql_matches.push(json!({"table":table.name,"column":"repository_id","rows":n}));
                        }
                    }
                    let remaining = json!({"columns_checked":sql_checked,"matches":sql_matches});
                    let mut patterns = vec![repository.clone()];
                    patterns.extend(connectors.iter().map(|c| format!("connector:{c}:")));
                    let retained = retained(g)?;
                    let retained_tables: Vec<&str> = retained.iter().map(|l| table_of(l)).collect();
                    let ids = merge_scans(
                        references(g, &labels, &patterns, (&repository, workspace), &retained, true)?,
                        legacy::references(db, &tables, &patterns, (&repository, workspace), &retained_tables, true)?,
                    );
                    json!({"verified_local":empty(&remaining) && empty(&ids),"project_rows":remaining,"source_ids":ids})
                }
                "memory.delete" => {
                    // Only one-way hashes of the ids are kept: confirm no
                    // current memory carries a deleted id.
                    let hashes: BTreeSet<&str> = receipt["removed"]["keys"]["hashes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    let reappeared = rows(
                        g,
                        "MATCH (m:Memory {repository_id: $repo}) RETURN m.id AS id",
                        gparams([("repo", json!(repository))]),
                    )?
                    .iter()
                    .filter(|m| hashes.contains(key_hash("memory.delete", s(m, "id")).as_str()))
                    .count();
                    json!({"verified_local":reappeared == 0 && receipt["verified_local"] == true,
                        "reappeared_memories":reappeared})
                }
                "storage.prune" | "retention.graph" => {
                    // Pruned keys are kept only as hashes; the removal was
                    // re-counted when the receipt was signed.
                    json!({"verified_local":receipt["verified_local"] == true,"recheck":"at_deletion_time"})
                }
                _ => return Err(Error::bad("Unknown deletion receipt")),
            };
            // External cleanups are queued in the SQL store (projection.rs).
            let (cleanups, pending) = legacy::external_now(db, &receipt["external_cleanup"])?;
            let chain = chain(g, db, workspace)?;
            let verified = current["verified_local"] == true;
            Ok(json!({"receipt":receipt,"chain":chain,"current":current,"external_cleanup":cleanups,
                "verified_local":verified,"external_pending":pending,
                "complete":verified && !pending && chain["intact"] == true,"checked_at":now()}))
        })
    })
}

/// Receipts of this workspace, newest first, with the chain status.
pub fn list(store: &Store, workspace: &str, token: &str) -> Result<Value> {
    // Receipts live where their deletions wrote them; a store mid-migration
    // may hold them only in the legacy SQL store.
    let on_graph = graph_store(store)?.read(|g| {
        one(
            g,
            "MATCH (r:DeletionReceipt {workspace_id: $ws}) RETURN 1 AS x LIMIT 1",
            gparams([("ws", json!(workspace))]),
        )
        .map(|r| r.is_some())
    })?;
    if !on_graph {
        return legacy::list(store, workspace, token);
    }
    store.authorized_read(workspace, token, "admin", |db, _| {
        let graph = graph_store(store)?;
        graph.read(|g| {
            let receipts: Vec<Value> = rows(
                g,
                "MATCH (r:DeletionReceipt {workspace_id: $ws}) \
                 RETURN r.id AS id, r.sequence AS sequence, r.repository_id AS repository_id, \
                 r.operation AS operation, r.target AS target, r.digest AS digest, \
                 r.previous AS previous, r.created_at AS created_at, r.body AS body \
                 ORDER BY r.sequence DESC LIMIT 200",
                gparams([("ws", json!(workspace))]),
            )?
            .into_iter()
            .map(|mut r| {
                let body: Value =
                    serde_json::from_str(s(&r, "body")).unwrap_or(Value::Null);
                r["verified_local"] = json!(body["verified_local"] == true);
                r["external_pending"] = json!(body["external_pending"] == true);
                r.as_object_mut().map(|m| m.remove("body"));
                r
            })
            .collect();
            Ok(json!({"receipts":receipts,"chain":chain(g, db, workspace)?}))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_avoid_json_escaped_characters_and_short_texts() {
        assert_eq!(excerpt("Too short."), None);
        let text = "Line one \"quoted\"\nThe quarterly migration window opens on the third Friday at 09:00 UTC for all regions.\n";
        let window = excerpt(text).unwrap();
        assert!(text.contains(&window));
        assert!(window.len() >= MIN_FINGERPRINT_BYTES && window.len() <= FINGERPRINT_BYTES);
        assert!(!window.contains(['"', '\\', '\n']));
        assert!(
            json!({ "body": text }).to_string().contains(&window),
            "found inside serialized JSON"
        );
    }

    #[test]
    fn document_scopes_do_not_capture_documents_with_longer_ids() {
        let scope = Scope::documents("c1", &json!(["a", "issue"])).unwrap();
        assert!(scope.source("connector:c1:a"));
        assert!(!scope.source("connector:c1:a:b"));
        assert!(!scope.source("connector:c1:ab"));
        assert!(!scope.source("connector:c2:a"));
        assert!(scope.passage("connector:c1:a:120"));
        assert!(
            !scope.passage("connector:c1:a:b:0"),
            "passage of document a:b"
        );
        assert!(scope.passage("connector:c1:issue:0"));
        let whole = Scope::connector("c1");
        assert!(whole.source("connector:c1:anything:at:all"));
        assert!(whole.passage("connector:c1:x:7"));
        assert!(!whole.source("connector:c10:x"));
        let existing = BTreeSet::from(["connector:c1:a:b".to_string()]);
        let ids = scope.sources();
        assert!(
            !names_any(r#"["connector:c1:a:b"]"#, &ids, &existing),
            "another document"
        );
        assert!(names_any(r#"["connector:c1:a"]"#, &ids, &existing));
        assert!(
            names_any("connector:c1:a:17", &ids, &existing),
            "a passage of a"
        );
    }

    #[test]
    fn state_masking_ignores_minted_ids_and_clocks_only() {
        let masked = mask(
            "job 3f2b9c0e8a7d4c1b9e6f5a4b3c2d1e0f at 2026-09-29T13:41:31.207743Z sha 3f2b9c0e8a7d4c1b9e6f5a4b3c2d1e0f3f2b9c0e8a7d4c1b9e6f5a4b3c2d1e0f",
        );
        assert_eq!(
            masked,
            "job <id> at <time> sha 3f2b9c0e8a7d4c1b9e6f5a4b3c2d1e0f3f2b9c0e8a7d4c1b9e6f5a4b3c2d1e0f"
        );
    }

    #[test]
    fn the_label_catalog_is_declared_once() {
        let mut labels: Vec<&str> = LABELS.iter().map(|(l, _, _)| *l).collect();
        let mut tables: Vec<&str> = LABELS.iter().map(|(_, t, _)| *t).collect();
        labels.sort_unstable();
        tables.sort_unstable();
        labels.dedup();
        tables.dedup();
        assert_eq!(labels.len(), LABELS.len(), "labels unique");
        assert_eq!(tables.len(), LABELS.len(), "legacy names unique");
        for (_, label, _) in SINCE_VISIBLE {
            assert!(LABELS.iter().any(|(l, _, _)| l == label), "{label}");
        }
        for (_, label) in NAMES_MEMORIES {
            assert!(LABELS.iter().any(|(l, _, _)| l == label), "{label}");
        }
        for label in HISTORY
            .iter()
            .chain(RETAINED)
            .chain(CACHES)
            .chain(LOCATIONS)
            .chain(INDEPENDENT)
        {
            assert!(LABELS.iter().any(|(l, _, _)| l == label), "{label}");
        }
        for (label, _) in COUNTERS {
            assert!(LABELS.iter().any(|(l, _, _)| l == label), "{label}");
        }
    }

    #[test]
    fn receipts_form_a_hash_chained_next_linked_list_per_workspace() {
        let store = GraphStore::open_in_memory().unwrap();
        let sql = rusqlite::Connection::open_in_memory().unwrap();
        sql.execute_batch(
            "CREATE TABLE am_deletion_receipts(id TEXT PRIMARY KEY, workspace_id TEXT, sequence INTEGER, repository_id TEXT, operation TEXT, target TEXT, body TEXT, digest TEXT, previous TEXT, created_at TEXT)",
        )
        .unwrap();
        store
            .transaction(|db| {
                let first = append(
                    db,
                    &sql,
                    "w1",
                    "r1",
                    "memory.delete",
                    "memories",
                    json!({"contract":CONTRACT,"operation":"memory.delete"}),
                )?;
                let second = append(
                    db,
                    &sql,
                    "w1",
                    "r1",
                    "memory.delete",
                    "memories",
                    json!({"contract":CONTRACT,"operation":"memory.delete"}),
                )?;
                assert_eq!(first["sequence"], json!(1));
                assert_eq!(second["sequence"], json!(2));
                assert_eq!(second["previous"], first["digest"]);
                // Another workspace runs its own chain.
                let other = append(
                    db,
                    &sql,
                    "w2",
                    "r9",
                    "project.delete",
                    "r9",
                    json!({"contract":CONTRACT,"operation":"project.delete"}),
                )?;
                assert_eq!(other["sequence"], json!(1));
                assert_eq!(other["previous"], Value::Null);
                Ok(())
            })
            .unwrap();
        store
            .read(|db| {
                let status = chain(db, &sql, "w1")?;
                assert_eq!(status["length"], json!(2));
                assert_eq!(status["intact"], json!(true), "{status}");
                assert_eq!(chain(db, &sql, "w2")?["length"], json!(1));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn erase_walks_remove_scoped_labels_and_leave_tombstones() {
        let store = GraphStore::open_in_memory().unwrap();
        store
            .transaction(|db| {
                db.query(
                    "CREATE (:Connector {id: 'c1', repository_id: 'r1'}), \
                     (:ConnectorDoc {id: 'd1', repository_id: 'r1', connector_id: 'c1', sha256: 'x1', content: 'secret one'}), \
                     (:ConnectorDoc {id: 'd2', repository_id: 'r1', connector_id: 'c1', sha256: 'x2', content: 'secret two'}), \
                     (:OriginalDocument {repository_id: 'r1', source_id: 'connector:c1:d1', sha256: 'x1'}), \
                     (:OriginalDocument {repository_id: 'r1', source_id: 'connector:c1:d2', sha256: 'x2'})",
                )?;
                let erased = erase(db, "r1", &Scope::documents("c1", &json!(["d1"]))?)?;
                assert_eq!(erased["documents"], json!(1));
                assert_eq!(erased["originals"], json!(1));
                Ok(())
            })
            .unwrap();
        store
            .read(|db| {
                assert_eq!(
                    counted(
                        db,
                        "MATCH (d:ConnectorDoc {connector_id: 'c1'}) RETURN count(d) AS n",
                        store_graph::Params::new()
                    )?,
                    1,
                    "d2 stays"
                );
                assert_eq!(
                    counted(
                        db,
                        "MATCH (f:ForgottenSource {connector_id: 'c1'}) RETURN count(f) AS n",
                        store_graph::Params::new()
                    )?,
                    1,
                    "one tombstone"
                );
                Ok(())
            })
            .unwrap();
        store
            .transaction(|db| {
                let erased = erase(db, "r1", &Scope::connector("c1"))?;
                assert_eq!(erased["connector"], json!(1));
                assert_eq!(erased["documents"], json!(1));
                Ok(())
            })
            .unwrap();
        store
            .read(|db| {
                assert_eq!(
                    counted(
                        db,
                        "MATCH (c:Connector) RETURN count(c) AS n",
                        store_graph::Params::new()
                    )?,
                    0
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn forget_removes_what_was_computed_while_the_documents_were_visible() {
        let store = GraphStore::open_in_memory().unwrap();
        store
            .transaction(|db| {
                db.query(
                    "CREATE (:ModelCache {repository_id: 'r1', created_at: '2026-01-01T00:00:00Z'}), \
                     (:ModelCache {repository_id: 'r1', created_at: '2025-01-01T00:00:00Z'}), \
                     (:Hint {repository_id: 'r1', target: 'connector:c1:d1', updated_at: '2025-02-01T00:00:00Z'}), \
                     (:Hint {repository_id: 'r1', target: 'other', updated_at: '2025-02-01T00:00:00Z'})",
                )?;
                let removed = forget(
                    db,
                    "w1",
                    "r1",
                    &Scope::connector("c1"),
                    Some("2026-01-01T00:00:00Z"),
                )?;
                assert_eq!(removed["since_visible"]["model_cache"], json!(1));
                assert_eq!(removed["hints"], json!(1));
                Ok(())
            })
            .unwrap();
        store
            .read(|db| {
                assert_eq!(
                    counted(
                        db,
                        "MATCH (m:ModelCache {repository_id: 'r1'}) RETURN count(m) AS n",
                        store_graph::Params::new()
                    )?,
                    1,
                    "the older cache entry predates visibility"
                );
                assert_eq!(
                    counted(
                        db,
                        "MATCH (h:Hint {repository_id: 'r1'}) RETURN count(h) AS n",
                        store_graph::Params::new()
                    )?,
                    1
                );
                Ok(())
            })
            .unwrap();
    }
}




