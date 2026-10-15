//! Legacy SQL retention for stores whose per-episode by-products and
//! connector documents have not migrated to the graph store yet
//! (docs/store-api.md: "beside, not instead of"). [`super`] holds the
//! graph-native path; the Store-facing entry points pick per project.
use super::{Retention, SourcePlan, SourcePolicy, document_date};
use crate::store::{Principal, one, rows, s};
use crate::{Error, Result};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

/// Episodes that fall outside the newest `keep` for `repository`, as an
/// SQL fragment usable in `episode IN (...)`.
const OLDER_THAN: &str = "SELECT episode FROM gf_episodes WHERE repository_id=?1 \
     AND sequence <= (SELECT coalesce(max(sequence),0) FROM gf_episodes WHERE repository_id=?1) - ?2";

/// Apply `retention` to one repository. Returns per-table deleted row counts.
pub fn apply(db: &Connection, repository: &str, retention: &Retention) -> Result<Value> {
    let coverage = db.execute(
        &format!(
            "DELETE FROM gf_test_coverage WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"
        ),
        params![repository, retention.coverage_episodes as i64],
    )?;
    let uploads = db.execute(
        &format!(
            "DELETE FROM gf_coverage_uploads WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"
        ),
        params![repository, retention.coverage_episodes as i64],
    )?;
    let inventories = db.execute(
        &format!(
            "DELETE FROM gf_coverage_inventory WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"
        ),
        params![repository, retention.coverage_episodes as i64],
    )?;
    // Closed cross edges are history: drop those whose *own* side of the
    // edge points at an episode this repository no longer keeps. Open edges
    // describe the current link state and always stay.
    let cross_edges = db.execute(
        &format!(
            "DELETE FROM gf_cross_edges WHERE valid_to_episode IS NOT NULL AND (\
               (source_repository_id=?1 AND source_episode IN ({OLDER_THAN})) OR \
               (target_repository_id=?1 AND target_episode IN ({OLDER_THAN})))"
        ),
        params![repository, retention.cross_edge_episodes as i64],
    )?;
    let cross_unresolved = db.execute(
        &format!(
            "DELETE FROM gf_cross_unresolved WHERE valid_to_episode IS NOT NULL \
             AND source_repository_id=?1 AND source_episode IN ({OLDER_THAN})"
        ),
        params![repository, retention.cross_edge_episodes as i64],
    )?;
    let unresolved = db.execute(
        &format!(
            "DELETE FROM gf_unresolved_calls WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"
        ),
        params![repository, retention.unresolved_episodes as i64],
    )?;
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

/// Rows [`apply`] would delete now, per table (all zero right after it ran).
pub fn remaining(db: &Connection, repository: &str, retention: &Retention) -> Result<Value> {
    let count = |sql: &str, keep: usize| -> Result<i64> {
        Ok(db.query_row(sql, params![repository, keep as i64], |r| r.get(0))?)
    };
    Ok(json!({
        "coverage": count(&format!("SELECT count(*) FROM gf_test_coverage WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"), retention.coverage_episodes)?,
        "coverage_uploads": count(&format!("SELECT count(*) FROM gf_coverage_uploads WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"), retention.coverage_episodes)?,
        "coverage_inventory": count(&format!("SELECT count(*) FROM gf_coverage_inventory WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"), retention.coverage_episodes)?,
        "cross_edges": count(&format!(
            "SELECT count(*) FROM gf_cross_edges WHERE valid_to_episode IS NOT NULL AND (\
               (source_repository_id=?1 AND source_episode IN ({OLDER_THAN})) OR \
               (target_repository_id=?1 AND target_episode IN ({OLDER_THAN})))"), retention.cross_edge_episodes)?,
        "cross_unresolved": count(&format!(
            "SELECT count(*) FROM gf_cross_unresolved WHERE valid_to_episode IS NOT NULL \
             AND source_repository_id=?1 AND source_episode IN ({OLDER_THAN})"), retention.cross_edge_episodes)?,
        "unresolved_calls": count(&format!("SELECT count(*) FROM gf_unresolved_calls WHERE repository_id=?1 AND episode IN ({OLDER_THAN})"), retention.unresolved_episodes)?,
    }))
}

/// Select the documents `policy` would forget at `now`, from the SQL
/// connector tables.
pub fn source_plan(
    db: &Connection,
    principal: &Principal,
    repository: &Value,
    policy: &SourcePolicy,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<SourcePlan> {
    let identifier = s(repository, "id");
    let cutoff = now - chrono::Duration::days(i64::from(policy.max_age_days));
    let mut cited = std::collections::BTreeSet::new();
    if policy.skip_cited_by_reviewed {
        for memory in crate::memory::list(db, repository, principal, true)? {
            if memory["state"] != "reviewed" {
                continue;
            }
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
            "SELECT id FROM am_connectors WHERE repository_id=? AND id=?",
            params![identifier, connector],
        )?
        .ok_or_else(|| Error::bad(format!("Connector unavailable: {connector}")))?;
        for doc in rows(
            db,
            "SELECT id,metadata FROM am_connector_docs WHERE connector_id=? ORDER BY id",
            [connector],
        )? {
            let metadata: Value = serde_json::from_str(s(&doc, "metadata")).unwrap_or(Value::Null);
            let Some(date) = document_date(&metadata, policy.by) else {
                plan.skipped_undated += 1;
                continue;
            };
            if date >= cutoff {
                continue;
            }
            let id = s(&doc, "id").to_string();
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

/// `retention.preview` against the SQL connector tables (see
/// [`super::Store::retention_preview`] for the contract).
pub fn retention_preview(
    db: &Connection,
    p: &Principal,
    repository: &Value,
    policy: &SourcePolicy,
) -> Result<Value> {
    let identifier = s(repository, "id");
    let plan = source_plan(db, p, repository, policy, chrono::Utc::now())?;
    let mut connectors = serde_json::Map::new();
    for (connector, documents) in &plan.documents {
        let mut removes: std::collections::BTreeMap<&str, usize> = Default::default();
        let mut derived: std::collections::BTreeMap<&str, usize> = Default::default();
        let listed: Vec<&String> = documents.iter().collect();
        for batch in listed.chunks(crate::deletion::MAX_SCOPE_DOCUMENTS) {
            let scope = crate::deletion::Scope::document_set(
                connector,
                batch.iter().map(|d| (*d).clone()).collect(),
            );
            let forget = crate::deletion::legacy::plan(db, &p.workspace, identifier, &scope)?;
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
}

// ------------------------------------------------------------ graph.stats

/// Tables reported by [`stats`]: name → (SQL filter, text columns whose
/// lengths approximate the row's content bytes).
const TABLES: &[(&str, &str, &str, &[&str])] = &[
    (
        "nodes",
        "gf_nodes",
        "repository_id=?1",
        &[
            "node_id",
            "attributes",
            "valid_from_episode",
            "valid_to_episode",
        ],
    ),
    (
        "edges",
        "gf_edges",
        "repository_id=?1",
        &[
            "source",
            "target",
            "relation",
            "key",
            "attributes",
            "valid_from_episode",
            "valid_to_episode",
        ],
    ),
    (
        "edge_sources",
        "gf_edge_sources",
        "repository_id=?1",
        &["relpath", "source_location", "derivation"],
    ),
    (
        "coverage",
        "gf_test_coverage",
        "repository_id=?1",
        &["episode", "test_id", "test_node_id", "relpath"],
    ),
    (
        "coverage_inventory",
        "gf_coverage_inventory",
        "repository_id=?1",
        &["episode", "inventory"],
    ),
    (
        "cross_edges",
        "gf_cross_edges",
        "source_repository_id=?1 OR target_repository_id=?1",
        &[
            "source_episode",
            "source_node_id",
            "source_stable_id",
            "target_episode",
            "target_node_id",
            "target_stable_id",
            "relation",
            "module_path",
            "symbol",
        ],
    ),
    (
        "unresolved_calls",
        "gf_unresolved_calls",
        "repository_id=?1",
        &["episode", "caller", "callee", "call"],
    ),
];

/// Per-repository size report: row counts and *content bytes* per table
/// (the sum of the text columns' lengths plus 8 bytes per integer column —
/// an approximation of what the rows carry, not SQLite page usage, which
/// includes indexes and free space and is only available per database),
/// episode count, and the latest `impact.report` wall-clock time.
pub fn stats(db: &Connection, repository: &str) -> Result<Value> {
    let mut tables = serde_json::Map::new();
    let mut total_bytes = 0i64;
    for (name, table, filter, columns) in TABLES {
        let lengths = columns
            .iter()
            .map(|c| format!("coalesce(length({c}),0)"))
            .collect::<Vec<_>>()
            .join("+");
        let row = one(
            db,
            &format!("SELECT count(*) AS rows_, coalesce(sum({lengths}),0)+count(*)*16 AS bytes FROM {table} WHERE {filter}"),
            [repository],
        )?
        .unwrap_or_else(|| json!({"rows_": 0, "bytes": 0}));
        let bytes = row["bytes"].as_i64().unwrap_or(0);
        total_bytes += bytes;
        tables.insert(
            name.to_string(),
            json!({"rows": row["rows_"].as_i64().unwrap_or(0), "content_bytes": bytes}),
        );
    }
    let episodes = one(
        db,
        "SELECT count(*) AS n, coalesce(sum(length(attributes)),0) AS attribute_bytes FROM gf_episodes WHERE repository_id=?",
        [repository],
    )?
    .unwrap_or_else(|| json!({"n": 0, "attribute_bytes": 0}));
    let uploads = rows(
        db,
        "SELECT episode, source, granularity, tests, files, ranges, created_at FROM gf_coverage_uploads WHERE repository_id=? ORDER BY id DESC LIMIT 20",
        [repository],
    )?;
    let last_report = one(
        db,
        "SELECT elapsed_ms, changed_files, base_episode, head_episode, created_at FROM gf_report_timings WHERE repository_id=? ORDER BY id DESC LIMIT 1",
        [repository],
    )?;
    let report_count: i64 = db.query_row(
        "SELECT count(*) FROM gf_report_timings WHERE repository_id=?",
        [repository],
        |r| r.get(0),
    )?;
    let database_bytes: i64 = db.query_row(
        "SELECT page_count*page_size FROM pragma_page_count(), pragma_page_size()",
        [],
        |r| r.get(0),
    )?;
    Ok(json!({
        "repository_id": repository,
        "episodes": episodes["n"],
        "episode_attribute_bytes": episodes["attribute_bytes"],
        "tables": tables,
        "content_bytes": total_bytes,
        "database_bytes_all_repositories": database_bytes,
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
        "bytes_note": "content_bytes sums the text columns of the rows (+16 bytes/row); database_bytes_all_repositories is the SQLite file size, shared by every repository.",
    }))
}
