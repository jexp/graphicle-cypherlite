//! Retention of per-episode graph by-products and the `graph.stats` size
//! report, on the CypherLite store (migration plan §2.4/§2.5).
//!
//! Every projected episode leaves behind nodes and relationships that are
//! only useful for a while: execution coverage (`(:Test)-[:COVERS]->(:File)`
//! relationships, `CoverageUpload` and `CoverageInventory` nodes), closed
//! cross-repository edges (`CROSS_CALL` relationships, `UnresolvedCross`
//! nodes) and receiver-resolution leftovers (`UnresolvedCall` nodes).
//! [`Retention`] bounds each of them to the last *N* episodes of a
//! repository; symbols, their relationships and the episodes themselves are
//! never removed here. Defaults: coverage 20 episodes, cross edges 20
//! episodes, unresolved calls 2 (current + previous). Override per call with
//! `retention` arguments or process-wide with
//! `GRAPHICLE_RETENTION_COVERAGE`, `GRAPHICLE_RETENTION_CROSS_EDGES`,
//! `GRAPHICLE_RETENTION_UNRESOLVED`.
//!
//! The writers of these nodes (the projection pipeline) stamp each with
//! `repository_id`, the `episode` id and a denormalized `episode_sequence`
//! integer, so the retention window is a property predicate — no
//! enumeration, no joins: `episode_sequence <= latest - keep`, where
//! `latest` is the repository's maximum `Episode.sequence`.
use crate::{
    Error, Result,
    store::{Store, repo},
    store_graph::{self, Params, json_to_value, one, rows},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) mod legacy;

pub const DEFAULT_COVERAGE_EPISODES: usize = 20;
pub const DEFAULT_CROSS_EDGE_EPISODES: usize = 20;
pub const DEFAULT_UNRESOLVED_EPISODES: usize = 2;
const MAX_EPISODES: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    /// Episodes (newest first) whose coverage rows and upload records stay.
    pub coverage_episodes: usize,
    /// Episodes whose *closed* cross-repository edge rows stay; open rows
    /// (no `valid_to_episode`) always stay.
    pub cross_edge_episodes: usize,
    /// Episodes whose `UnresolvedCall` nodes stay (2 = current + previous).
    pub unresolved_episodes: usize,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            coverage_episodes: DEFAULT_COVERAGE_EPISODES,
            cross_edge_episodes: DEFAULT_CROSS_EDGE_EPISODES,
            unresolved_episodes: DEFAULT_UNRESOLVED_EPISODES,
        }
    }
}

impl Retention {
    /// Defaults overridden by the `GRAPHICLE_RETENTION_*` environment.
    pub fn from_env() -> Self {
        let read = |key: &str, default: usize| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|v| (1..=MAX_EPISODES).contains(v))
                .unwrap_or(default)
        };
        Self {
            coverage_episodes: read("GRAPHICLE_RETENTION_COVERAGE", DEFAULT_COVERAGE_EPISODES),
            cross_edge_episodes: read(
                "GRAPHICLE_RETENTION_CROSS_EDGES",
                DEFAULT_CROSS_EDGE_EPISODES,
            ),
            unresolved_episodes: read(
                "GRAPHICLE_RETENTION_UNRESOLVED",
                DEFAULT_UNRESOLVED_EPISODES,
            ),
        }
    }

    /// Environment defaults overridden by an optional `retention` object in
    /// the request (`coverage_episodes`, `cross_edge_episodes`,
    /// `unresolved_episodes`; each 1..=100000).
    pub fn from_args(args: &Value) -> Result<Self> {
        let mut retention = Self::from_env();
        let Some(overrides) = args.get("retention") else {
            return Ok(retention);
        };
        if !overrides.is_object() {
            return Err(Error::bad("Expected a retention object"));
        }
        for (key, slot) in [
            ("coverage_episodes", &mut retention.coverage_episodes),
            ("cross_edge_episodes", &mut retention.cross_edge_episodes),
            ("unresolved_episodes", &mut retention.unresolved_episodes),
        ] {
            match overrides.get(key) {
                None | Some(Value::Null) => {}
                Some(value) => {
                    *slot = value
                        .as_u64()
                        .and_then(|v| usize::try_from(v).ok())
                        .filter(|v| (1..=MAX_EPISODES).contains(v))
                        .ok_or_else(|| {
                            Error::bad(format!("Expected retention.{key} between 1 and 100000"))
                        })?;
                }
            }
        }
        Ok(retention)
    }
}

fn params(pairs: &[(&str, Value)]) -> Params {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), json_to_value(v)))
        .collect()
}

/// The repository's latest episode sequence (`0` when it has none).
fn latest_sequence(db: &mut cypherlite::Connection, repository: &str) -> Result<i64> {
    let row = one(
        db,
        "MATCH (e:Episode {repository_id: $repo}) RETURN coalesce(max(e.sequence), 0) AS latest",
        params(&[("repo", json!(repository))]),
    )?;
    Ok(row.and_then(|r| r["latest"].as_i64()).unwrap_or(0))
}

/// Count the matches of `match_where` (a `MATCH … WHERE …` fragment
/// parameterizable by `$repo` and `$cutoff`), then delete them. Node
/// patterns are detached; relationship patterns must end in a relationship
/// binding and use plain `DELETE`.
fn drop_where(
    db: &mut cypherlite::Connection,
    match_where: &str,
    binding: &str,
    detach: bool,
    repository: &str,
    cutoff: i64,
) -> Result<i64> {
    let params = params(&[("repo", json!(repository)), ("cutoff", json!(cutoff))]);
    let count = one(
        db,
        &format!("{match_where} RETURN count({binding}) AS n"),
        params.clone(),
    )?
    .and_then(|r| r["n"].as_i64())
    .unwrap_or(0);
    if count > 0 {
        let delete = if detach {
            format!("{match_where} DETACH DELETE {binding}")
        } else {
            format!("{match_where} DELETE {binding}")
        };
        db.query_with_params(&delete, params)?;
    }
    Ok(count)
}

/// Count the matches [`drop_where`] would delete (all zero right after it
/// ran).
fn count_where(
    db: &mut cypherlite::Connection,
    match_where: &str,
    binding: &str,
    repository: &str,
    cutoff: i64,
) -> Result<i64> {
    Ok(one(
        db,
        &format!("{match_where} RETURN count({binding}) AS n"),
        params(&[("repo", json!(repository)), ("cutoff", json!(cutoff))]),
    )?
    .and_then(|r| r["n"].as_i64())
    .unwrap_or(0))
}

/// `MATCH` fragments of the per-episode by-products, sharing the `$repo` and
/// `$cutoff` parameters. Closed cross edges are history: those whose *own*
/// side of the edge points at an episode this repository no longer keeps go;
/// open edges (no `valid_to_episode`) describe the current link state and
/// always stay.
const COVERAGE: &str =
    "MATCH ()-[c:COVERS]->() WHERE c.repository_id = $repo AND c.episode_sequence <= $cutoff";
const COVERAGE_UPLOADS: &str = "MATCH (u:CoverageUpload {repository_id: $repo}) WHERE u.episode_sequence <= $cutoff";
const COVERAGE_INVENTORY: &str = "MATCH (i:CoverageInventory {repository_id: $repo}) WHERE i.episode_sequence <= $cutoff";
const CROSS_EDGES: &str = "MATCH ()-[r:CROSS_CALL]->() WHERE r.valid_to_episode IS NOT NULL AND (\
     (r.source_repository_id = $repo AND r.source_episode_sequence <= $cutoff) OR \
     (r.target_repository_id = $repo AND r.target_episode_sequence <= $cutoff))";
const CROSS_UNRESOLVED: &str = "MATCH (u:UnresolvedCross) WHERE u.valid_to_episode IS NOT NULL \
     AND u.source_repository_id = $repo AND u.source_episode_sequence <= $cutoff";
const UNRESOLVED_CALLS: &str = "MATCH (u:UnresolvedCall {repository_id: $repo}) WHERE u.episode_sequence <= $cutoff";

/// Apply `retention` to one repository: label-scoped deletes of the
/// per-episode by-products outside each window. Returns per-kind deleted
/// counts. Runs inside the caller's transaction.
pub fn apply(
    db: &mut cypherlite::Connection,
    repository: &str,
    retention: &Retention,
) -> Result<Value> {
    let latest = latest_sequence(db, repository)?;
    let coverage_cutoff = latest - retention.coverage_episodes as i64;
    let cross_cutoff = latest - retention.cross_edge_episodes as i64;
    let unresolved_cutoff = latest - retention.unresolved_episodes as i64;
    let coverage = drop_where(db, COVERAGE, "c", false, repository, coverage_cutoff)?;
    let uploads = drop_where(db, COVERAGE_UPLOADS, "u", true, repository, coverage_cutoff)?;
    let inventories = drop_where(db, COVERAGE_INVENTORY, "i", true, repository, coverage_cutoff)?;
    let cross_edges = drop_where(db, CROSS_EDGES, "r", false, repository, cross_cutoff)?;
    let cross_unresolved = drop_where(db, CROSS_UNRESOLVED, "u", true, repository, cross_cutoff)?;
    let unresolved = drop_where(db, UNRESOLVED_CALLS, "u", true, repository, unresolved_cutoff)?;
    Ok(json!({
        "retention": retention,
        "deleted": {
            "coverage": coverage,
            "coverage_uploads": uploads,
            "coverage_inventory": inventories,
            "cross_edges": cross_edges,
            "cross_unresolved": cross_unresolved,
            "unresolved_calls": unresolved,
        }
    }))
}

/// Rows [`apply`] would delete now, per kind (all zero right after it ran).
pub fn remaining(
    db: &mut cypherlite::Connection,
    repository: &str,
    retention: &Retention,
) -> Result<Value> {
    let latest = latest_sequence(db, repository)?;
    let coverage_cutoff = latest - retention.coverage_episodes as i64;
    let cross_cutoff = latest - retention.cross_edge_episodes as i64;
    let unresolved_cutoff = latest - retention.unresolved_episodes as i64;
    Ok(json!({
        "coverage": count_where(db, COVERAGE, "c", repository, coverage_cutoff)?,
        "coverage_uploads": count_where(db, COVERAGE_UPLOADS, "u", repository, coverage_cutoff)?,
        "coverage_inventory": count_where(db, COVERAGE_INVENTORY, "i", repository, coverage_cutoff)?,
        "cross_edges": count_where(db, CROSS_EDGES, "r", repository, cross_cutoff)?,
        "cross_unresolved": count_where(db, CROSS_UNRESOLVED, "u", repository, cross_cutoff)?,
        "unresolved_calls": count_where(db, UNRESOLVED_CALLS, "u", repository, unresolved_cutoff)?,
    }))
}

// ------------------------------------------------------------ source retention

/// Which timestamp dates a connector document for [`SourcePolicy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dated {
    /// The provider's own timestamp (`source_timestamp`, else the provider
    /// metadata's `updated_at` / `created_at`).
    SourceTimestamp,
    /// When the connector last observed the document.
    ObservedAt,
}

/// Time-based retention of connector documents: documents older than
/// `max_age_days` are forgotten through certified deletion, one receipt per
/// batch. Nothing runs on a schedule; `retention.preview` shows the plan and
/// `retention.apply` must echo its `plan_hash`. Documents with no usable
/// date are never selected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePolicy {
    pub connector_ids: Vec<String>,
    pub max_age_days: u32,
    pub by: Dated,
    /// Keep documents a reviewed memory cites (default true).
    pub skip_cited_by_reviewed: bool,
}

impl SourcePolicy {
    /// `sources: {connector_ids, max_age_days, by?, skip_cited_by_reviewed?}`.
    pub fn from_args(args: &Value) -> Result<Self> {
        let sources = args
            .get("sources")
            .filter(|v| v.is_object())
            .ok_or_else(|| Error::bad("Expected a sources object"))?;
        let mut connector_ids: Vec<String> = sources["connector_ids"]
            .as_array()
            .filter(|ids| !ids.is_empty() && ids.len() <= 100)
            .ok_or_else(|| Error::bad("sources.connector_ids must list 1–100 connectors"))?
            .iter()
            .map(|id| {
                id.as_str()
                    .filter(|id| !id.is_empty() && id.len() <= 100)
                    .map(str::to_string)
                    .ok_or_else(|| Error::bad("Invalid connector id"))
            })
            .collect::<Result<_>>()?;
        connector_ids.sort();
        connector_ids.dedup();
        let max_age_days = sources["max_age_days"]
            .as_u64()
            .filter(|d| (1..=36_500).contains(d))
            .ok_or_else(|| Error::bad("sources.max_age_days must be between 1 and 36500"))?
            as u32;
        let by = match sources.get("by").and_then(Value::as_str) {
            None | Some("source_timestamp") => Dated::SourceTimestamp,
            Some("observed_at") => Dated::ObservedAt,
            Some(_) => {
                return Err(Error::bad(
                    "sources.by must be source_timestamp or observed_at",
                ));
            }
        };
        let skip_cited_by_reviewed = match sources.get("skip_cited_by_reviewed") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(on)) => *on,
            Some(_) => {
                return Err(Error::bad(
                    "sources.skip_cited_by_reviewed must be true or false",
                ));
            }
        };
        Ok(Self {
            connector_ids,
            max_age_days,
            by,
            skip_cited_by_reviewed,
        })
    }
}

/// What a [`SourcePolicy`] selects now.
#[derive(Clone, Debug, Default)]
pub struct SourcePlan {
    pub cutoff: String,
    /// Connector → selected document ids.
    pub documents: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    pub oldest: Option<String>,
    pub newest: Option<String>,
    pub skipped_cited: usize,
    pub skipped_undated: usize,
    /// Digest of the policy and the selected documents' one-way names.
    pub plan_hash: String,
}

fn document_date(metadata: &Value, by: Dated) -> Option<chrono::DateTime<chrono::Utc>> {
    let candidates: Vec<&Value> = match by {
        Dated::ObservedAt => vec![&metadata["observed_at"]],
        Dated::SourceTimestamp => vec![
            &metadata["source_timestamp"],
            &metadata["provider_metadata"]["updated_at"],
            &metadata["provider_metadata"]["created_at"],
        ],
    };
    candidates.into_iter().find_map(|value| {
        chrono::DateTime::parse_from_rfc3339(value.as_str()?)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
    })
}

/// The document's metadata for dating. The known date fields are flattened
/// onto dotted properties (`d.`metadata.observed_at`` etc. — queryable and
/// indexable, verified on v0.12.2; a map cannot be a property value). A
/// stringified JSON `metadata` property is the fallback for writers that
/// still store the blob.
fn metadata_of(doc: &Value) -> Value {
    let flattened = json!({
        "observed_at": doc["observed_at"],
        "source_timestamp": doc["source_timestamp"],
        "provider_metadata": {
            "updated_at": doc["provider_updated_at"],
            "created_at": doc["provider_created_at"],
        },
    });
    let dated = [
        &flattened["observed_at"],
        &flattened["source_timestamp"],
        &flattened["provider_metadata"]["updated_at"],
        &flattened["provider_metadata"]["created_at"],
    ]
    .iter()
    .any(|v| v.is_string());
    if dated {
        return flattened;
    }
    match &doc["metadata"] {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        other => other.clone(),
    }
}

/// Select the documents `policy` would forget at `now`.
pub fn source_plan(
    db: &mut cypherlite::Connection,
    principal: &crate::store::Principal,
    repository: &Value,
    policy: &SourcePolicy,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<SourcePlan> {
    let identifier = store_graph::s(repository, "id");
    let cutoff = now - chrono::Duration::days(i64::from(policy.max_age_days));
    let mut cited = std::collections::BTreeSet::new();
    if policy.skip_cited_by_reviewed {
        let _ = principal; // authorization happened at the Store boundary
        for memory in rows(
            db,
            "MATCH (m:Memory)-[:OF]->(:Project {id: $repo}) WHERE m.state = 'reviewed' \
             RETURN m.data AS data",
            params(&[("repo", json!(identifier))]),
        )? {
            for field in ["native_sources", "evidence_links"] {
                if let Some(sources) = memory["data"][field].as_object() {
                    cited.extend(sources.keys().cloned());
                }
            }
        }
    }
    let mut plan = SourcePlan {
        cutoff: cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ..SourcePlan::default()
    };
    let mut hashes: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    for connector in &policy.connector_ids {
        one(
            db,
            "MATCH (c:Connector {repository_id: $repo, id: $id}) RETURN c.id AS id",
            params(&[("repo", json!(identifier)), ("id", json!(connector))]),
        )?
        .ok_or_else(|| Error::bad(format!("Connector unavailable: {connector}")))?;
        for doc in rows(
            db,
            "MATCH (d:ConnectorDoc {connector_id: $connector}) \
             RETURN d.id AS id, \
                    d.`metadata.observed_at` AS observed_at, \
                    d.`metadata.source_timestamp` AS source_timestamp, \
                    d.`metadata.provider_metadata.updated_at` AS provider_updated_at, \
                    d.`metadata.provider_metadata.created_at` AS provider_created_at, \
                    d.metadata AS metadata \
             ORDER BY d.id",
            params(&[("connector", json!(connector))]),
        )? {
            let metadata = metadata_of(&doc);
            let Some(date) = document_date(&metadata, policy.by) else {
                plan.skipped_undated += 1;
                continue;
            };
            if date >= cutoff {
                continue;
            }
            let id = store_graph::s(&doc, "id").to_string();
            if cited.contains(&format!("connector:{connector}:{id}")) {
                plan.skipped_cited += 1;
                continue;
            }
            let stamp = date.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            if plan.oldest.as_ref().is_none_or(|o| stamp < *o) {
                plan.oldest = Some(stamp.clone());
            }
            if plan.newest.as_ref().is_none_or(|n| stamp > *n) {
                plan.newest = Some(stamp);
            }
            hashes
                .entry(connector.as_str())
                .or_default()
                .push(crate::deletion::document_hash(connector, &id));
            plan.documents
                .entry(connector.clone())
                .or_default()
                .insert(id);
        }
    }
    for list in hashes.values_mut() {
        list.sort();
    }
    plan.plan_hash = crate::digest(&json!({"policy": policy, "documents": hashes}));
    Ok(plan)
}

impl Store {
    /// `retention.preview` (read): what a [`SourcePolicy`] would forget now,
    /// per connector with the rows certified deletion would remove, and the
    /// `plan_hash` `retention.apply` must echo. Deletes nothing.
    pub fn retention_preview(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let policy = SourcePolicy::from_args(args)?;
        let graph = crate::maintenance::graph_store(self)?;
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, crate::string(args, "repository_id", 100)?)?;
            // The plan runs where the connector documents live: the graph
            // store, or the legacy SQL store of a project mid-migration.
            let on_graph = graph.read(|g| {
                one(
                    g,
                    "MATCH (c:Connector {repository_id: $repo}) RETURN 1 AS x LIMIT 1",
                    params(&[("repo", json!(store_graph::s(&repository, "id")))]),
                )
                .map(|r| r.is_some())
            })?;
            if !on_graph {
                return legacy::retention_preview(db, p, &repository, &policy);
            }
            graph.read(|g| {
                let plan = source_plan(g, p, &repository, &policy, chrono::Utc::now())?;
                let mut connectors = serde_json::Map::new();
                let identifier = store_graph::s(&repository, "id");
                for (connector, documents) in &plan.documents {
                    let mut removes: std::collections::BTreeMap<&str, usize> = Default::default();
                    let mut derived: std::collections::BTreeMap<&str, usize> = Default::default();
                    let listed: Vec<&String> = documents.iter().collect();
                    for batch in listed.chunks(crate::deletion::MAX_SCOPE_DOCUMENTS) {
                        let scope = crate::deletion::Scope::document_set(
                            connector,
                            batch.iter().map(|d| (*d).clone()).collect(),
                        );
                        let forget = crate::deletion::plan(g, &p.workspace, identifier, &scope)?;
                        for (table, n) in forget.rows() {
                            *removes.entry(table).or_default() += n;
                        }
                        for (kind, n) in forget.visible_since() {
                            *derived.entry(kind).or_default() += n;
                        }
                    }
                    connectors.insert(
                        connector.clone(),
                        json!({"documents": documents.len(), "removes": removes, "derived_since_visible": derived}),
                    );
                }
                Ok(json!({
                    "policy": policy,
                    "cutoff": plan.cutoff,
                    "documents": plan.documents.values().map(|d| d.len()).sum::<usize>(),
                    "date_range": {"oldest": plan.oldest, "newest": plan.newest},
                    "connectors": connectors,
                    "skipped": {"cited_by_reviewed": plan.skipped_cited, "undated": plan.skipped_undated},
                    "plan_hash": plan.plan_hash,
                    "apply": "retention.apply with the same sources and this plan_hash; each batch of up to 1,000 documents is forgotten with its own receipt",
                    "scheduled": false,
                }))
            })
        })
    }
}

// ------------------------------------------------------------ graph.stats

/// A per-label count with an approximate content size: string properties
/// (declared `IS STRING` in the graph type, or kept stringified as opaque
/// payloads) contribute their length, everything else a flat per-row
/// estimate. This approximates what the rows carry, not store page usage.
fn label_count(
    db: &mut cypherlite::Connection,
    cypher: &str,
    repository: &str,
) -> Result<(i64, i64)> {
    let row = one(db, cypher, params(&[("repo", json!(repository))]))?
        .unwrap_or_else(|| json!({"rows": 0, "bytes": 0}));
    Ok((
        row["rows"].as_i64().unwrap_or(0),
        row["bytes"].as_i64().unwrap_or(0),
    ))
}

/// Per-repository size report: node and relationship counts and *content
/// bytes* per kind, episode count, and the latest `impact.report`
/// wall-clock time. `snapshot_bytes` is the engine's snapshot size
/// ([`crate::store_graph::GraphStore::database`]'s `snapshot_data_bytes`),
/// shared by every repository of the store.
pub fn stats(
    db: &mut cypherlite::Connection,
    repository: &str,
    snapshot_bytes: Option<u64>,
) -> Result<Value> {
    let mut tables = serde_json::Map::new();
    let mut total_bytes = 0i64;
    let mut report = |name: &str, counted: (i64, i64)| {
        total_bytes += counted.1;
        tables.insert(
            name.to_string(),
            json!({"rows": counted.0, "content_bytes": counted.1}),
        );
    };
    // Nodes and their versioned payloads.
    let symbols = label_count(
        db,
        "MATCH (s:Symbol {repository_id: $repo}) RETURN count(s) AS rows, \
         coalesce(sum(size(s.qualified_name)),0)+coalesce(sum(size(s.relpath)),0)+count(s)*16 AS bytes",
        repository,
    )?;
    let versions = label_count(
        db,
        "MATCH (v:SymbolVersion {repository_id: $repo}) RETURN count(v) AS rows, \
         coalesce(sum(size(v.data)),0)+count(v)*16 AS bytes",
        repository,
    )?;
    report("nodes", (symbols.0 + versions.0, symbols.1 + versions.1));
    // Live and historical relationships between the repository's symbols.
    let edges = label_count(
        db,
        "MATCH (:Symbol {repository_id: $repo})-[r]->(:Symbol) RETURN count(r) AS rows, count(r)*96 AS bytes",
        repository,
    )?;
    report("edges", edges);
    // Provenance moved onto the relationships themselves (relpath,
    // source_location, derivation); the count is the relationships that
    // carry it.
    let sources = label_count(
        db,
        "MATCH (:Symbol {repository_id: $repo})-[r]->(:Symbol) WHERE r.relpath IS NOT NULL \
         RETURN count(r) AS rows, \
         coalesce(sum(size(r.relpath)),0)+coalesce(sum(size(r.source_location)),0)+count(r)*16 AS bytes",
        repository,
    )?;
    report("edge_sources", sources);
    report(
        "coverage",
        label_count(
            db,
            "MATCH ()-[c:COVERS]->() WHERE c.repository_id = $repo \
             RETURN count(c) AS rows, count(c)*64 AS bytes",
            repository,
        )?,
    );
    report(
        "coverage_inventory",
        label_count(
            db,
            "MATCH (i:CoverageInventory {repository_id: $repo}) RETURN count(i) AS rows, \
             coalesce(sum(size(i.inventory)),0)+count(i)*16 AS bytes",
            repository,
        )?,
    );
    report(
        "cross_edges",
        label_count(
            db,
            "MATCH ()-[r:CROSS_CALL]->() WHERE r.source_repository_id = $repo OR r.target_repository_id = $repo \
             RETURN count(r) AS rows, count(r)*128 AS bytes",
            repository,
        )?,
    );
    report(
        "unresolved_calls",
        label_count(
            db,
            "MATCH (u:UnresolvedCall {repository_id: $repo}) RETURN count(u) AS rows, \
             coalesce(sum(size(u.caller)),0)+coalesce(sum(size(u.callee)),0)+coalesce(sum(size(u.call)),0)+count(u)*16 AS bytes",
            repository,
        )?,
    );
    let episodes = one(
        db,
        "MATCH (e:Episode {repository_id: $repo}) RETURN count(e) AS n, \
         coalesce(sum(size(e.attributes)),0) AS attribute_bytes",
        params(&[("repo", json!(repository))]),
    )?
    .unwrap_or_else(|| json!({"n": 0, "attribute_bytes": 0}));
    let uploads = rows(
        db,
        "MATCH (u:CoverageUpload {repository_id: $repo}) \
         RETURN u.episode AS episode, u.source AS source, u.granularity AS granularity, \
                u.tests AS tests, u.files AS files, u.ranges AS ranges, u.created_at AS created_at \
         ORDER BY u.created_at DESC LIMIT 20",
        params(&[("repo", json!(repository))]),
    )?;
    let last_report = one(
        db,
        "MATCH (t:ReportTiming {repository_id: $repo}) \
         RETURN t.elapsed_ms AS elapsed_ms, t.changed_files AS changed_files, \
                t.base_episode AS base_episode, t.head_episode AS head_episode, t.created_at AS created_at \
         ORDER BY t.created_at DESC LIMIT 1",
        params(&[("repo", json!(repository))]),
    )?;
    let report_count = one(
        db,
        "MATCH (t:ReportTiming {repository_id: $repo}) RETURN count(t) AS n",
        params(&[("repo", json!(repository))]),
    )?
    .and_then(|r| r["n"].as_i64())
    .unwrap_or(0);
    Ok(json!({
        "repository_id": repository,
        "episodes": episodes["n"],
        "episode_attribute_bytes": episodes["attribute_bytes"],
        "tables": tables,
        "content_bytes": total_bytes,
        "database_bytes_all_repositories": snapshot_bytes,
        "coverage_uploads": uploads,
        "impact_reports": report_count,
        "last_impact_report": last_report.map(|r| json!({
            "elapsed_ms": r["elapsed_ms"],
            "changed_files": r["changed_files"],
            "base_episode": r["base_episode"],
            "head_episode": r["head_episode"],
            "created_at": r["created_at"],
        })),
        "retention_defaults": Retention::from_env(),
        "bytes_note": "content_bytes sums the string properties of the nodes and relationships (+16 bytes/row); database_bytes_all_repositories is the engine's snapshot size, shared by every repository.",
    }))
}

impl Store {
    /// `graph.stats` (read): size and timing summary for one repository.
    pub fn graph_stats(&self, workspace: &str, token: &str, repository: &str) -> Result<Value> {
        let graph = crate::maintenance::graph_store(self)?;
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, repository)?;
            let identifier = store_graph::s(&repository, "id").to_string();
            // The report reads where the by-products live: the graph store,
            // or the legacy SQL tables of a project mid-migration.
            if !crate::maintenance::project_episodes_on_graph(&graph, &identifier)? {
                return legacy::stats(db, &identifier);
            }
            let snapshot_bytes = graph
                .database()
                .snapshot_data_bytes()
                .ok();
            graph.read(|g| stats(g, &identifier, snapshot_bytes))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_graph::GraphStore;

    #[test]
    fn defaults_and_overrides() {
        let base = Retention::default();
        assert_eq!(base.coverage_episodes, 20);
        assert_eq!(base.cross_edge_episodes, 20);
        assert_eq!(base.unresolved_episodes, 2);
        let over = Retention::from_args(&json!({"retention": {"coverage_episodes": 3}})).unwrap();
        assert_eq!(over.coverage_episodes, 3);
        assert_eq!(over.unresolved_episodes, base.unresolved_episodes);
        assert!(Retention::from_args(&json!({"retention": {"coverage_episodes": 0}})).is_err());
        assert!(Retention::from_args(&json!({"retention": 5})).is_err());
    }

    /// Twenty-five episodes of by-products on an in-memory store; the
    /// retention window keeps the newest and drops the rest, per kind.
    #[test]
    fn per_episode_by_products_are_label_scoped_deletes() {
        let store = GraphStore::open_in_memory().unwrap();
        store
            .transaction(|db| {
                db.query(
                    "CREATE (p:Project {id: 'r', workspace_id: 'w', project_key: 'r'})",
                )?;
                for sequence in 1..=25i64 {
                    db.query_with_params(
                        "CREATE (:Episode {repository_id: 'r', episode: $ep, sequence: $seq})",
                        params(&[("ep", json!(format!("e{sequence}"))), ("seq", json!(sequence))]),
                    )?;
                    db.query_with_params(
                        "CREATE (:UnresolvedCall {repository_id: 'r', episode: $ep, episode_sequence: $seq, caller: 'a', callee: 'b', call: 'call'})",
                        params(&[("ep", json!(format!("e{sequence}"))), ("seq", json!(sequence))]),
                    )?;
                    db.query_with_params(
                        "CREATE (:CoverageUpload {repository_id: 'r', episode: $ep, episode_sequence: $seq, source: 'test', created_at: $at})",
                        params(&[
                            ("ep", json!(format!("e{sequence}"))),
                            ("seq", json!(sequence)),
                            ("at", json!(format!("2026-01-{sequence:02}T00:00:00Z"))),
                        ]),
                    )?;
                }
                // Closed and open cross edges.
                db.query(
                    "CREATE (:Symbol {repository_id: 'r', stable_id: 'a'})-[:CROSS_CALL {source_repository_id: 'r', source_episode: 'e1', source_episode_sequence: 1, valid_to_episode: 'e2'}]->(:Symbol {repository_id: 'r', stable_id: 'b'})",
                )?;
                db.query(
                    "CREATE (:Symbol {repository_id: 'r', stable_id: 'c'})-[:CROSS_CALL {source_repository_id: 'r', source_episode: 'e24', source_episode_sequence: 24, valid_to_episode: 'e25'}]->(:Symbol {repository_id: 'r', stable_id: 'd'})",
                )?;
                db.query(
                    "CREATE (:Symbol {repository_id: 'r', stable_id: 'e'})-[:CROSS_CALL {source_repository_id: 'r', source_episode: 'e1', source_episode_sequence: 1}]->(:Symbol {repository_id: 'r', stable_id: 'f'})",
                )?;
                Ok(())
            })
            .unwrap();
        let retention = Retention::default();
        let before = store
            .read(|db| remaining(db, "r", &retention))
            .unwrap();
        assert_eq!(before["unresolved_calls"], json!(23), "{before:#}");
        assert_eq!(before["coverage_uploads"], json!(5), "{before:#}");
        assert_eq!(before["cross_edges"], json!(1), "{before:#}");
        let applied = store
            .transaction(|db| apply(db, "r", &retention))
            .unwrap();
        assert_eq!(applied["deleted"]["unresolved_calls"], json!(23));
        assert_eq!(applied["deleted"]["coverage_uploads"], json!(5));
        assert_eq!(applied["deleted"]["cross_edges"], json!(1));
        let after = store
            .read(|db| remaining(db, "r", &retention))
            .unwrap();
        assert_eq!(
            after,
            json!({"coverage":0,"coverage_uploads":0,"coverage_inventory":0,
                   "cross_edges":0,"cross_unresolved":0,"unresolved_calls":0})
        );
        // Episodes and symbols survive; the open cross edge stays.
        store
            .read(|db| {
                let episodes = one(
                    db,
                    "MATCH (e:Episode {repository_id: 'r'}) RETURN count(e) AS n",
                    Params::new(),
                )?
                .unwrap();
                assert_eq!(episodes["n"], json!(25));
                let open = one(
                    db,
                    "MATCH ()-[r:CROSS_CALL]->() WHERE r.valid_to_episode IS NULL RETURN count(r) AS n",
                    Params::new(),
                )?
                .unwrap();
                assert_eq!(open["n"], json!(1));
                Ok(())
            })
            .unwrap();
    }
}
