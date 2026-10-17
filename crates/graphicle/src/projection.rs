//! Durable removal of only this application's external graph/vector scopes.
//! Requests survive project removal, process failure and credential outages.
//!
//! Backend configs and cleanup requests are graph nodes: `(:Backend
//! {repository_id, kind, config})` and `(:ProjectionCleanup {id, …})`, both
//! keyed by repository/scope; `config` stays stringified JSON (the engine
//! rejects map property values).
use crate::{
    Error, Result, digest, id, now,
    store::{Store, one, s},
    store_graph::{self as graph},
};
use rusqlite::Connection;
use serde_json::{Value, json};

fn address_hash(config: &Value) -> String {
    let mut address = config.clone();
    if let Some(map) = address.as_object_mut() {
        map.remove("credential_ref");
    }
    digest(&address)
}

/// The configured external backend of one `kind` for the repository, or
/// `None` (`SELECT config FROM am_backends WHERE … AND kind=?`).
pub fn backend_config(db: &Connection, repository: &str, kind: &str) -> Result<Option<Value>> {
    let Some(row) = crate::store::graph_for(db)?.read(|g| {
        graph::one(
            g,
            "MATCH (b:Backend {repository_id: $repo, kind: $kind}) RETURN b.config AS config",
            graph::params(&json!({"repo": repository, "kind": kind})),
        )
    })?
    else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(s(&row, "config"))?))
}

/// Insert or replace the repository's backend config of one `kind`
/// (`INSERT OR REPLACE INTO am_backends`).
pub fn backend_set(db: &Connection, repository: &str, kind: &str, config: &Value) -> Result<()> {
    crate::store::graph_for(db)?.transaction(|g| {
        graph::write(
            g,
            "MERGE (b:Backend {repository_id: $repo, kind: $kind}) SET b.config = $config",
            graph::params(
                &json!({"repo": repository, "kind": kind, "config": config.to_string()}),
            ),
        )?;
        graph::write(
            g,
            "MATCH (b:Backend {repository_id: $repo, kind: $kind}), (p:Project {id: $repo}) MERGE (b)-[:OF]->(p)",
            graph::params(&json!({"repo": repository, "kind": kind})),
        )?;
        Ok(())
    })
}

pub fn enqueue(db: &Connection, repository: &str, reason: &str) -> Result<()> {
    let Some(project) = one(
        db,
        "SELECT workspace_id FROM repo_projects WHERE id=?",
        [repository],
    )?
    else {
        return Ok(());
    };
    for backend in crate::store::graph_for(db)?.read(|g| {
        graph::rows(
            g,
            "MATCH (b:Backend {repository_id: $repo}) RETURN b.kind AS kind, b.config AS config",
            graph::params(&json!({"repo": repository})),
        )
    })? {
        if !["neo4j", "qdrant"].contains(&s(&backend, "kind")) {
            continue;
        }
        let config: Value = serde_json::from_str(s(&backend, "config"))?;
        enqueue_config(
            db,
            s(&project, "workspace_id"),
            repository,
            s(&backend, "kind"),
            &config,
            reason,
        )?;
    }
    Ok(())
}
pub fn enqueue_config(
    db: &Connection,
    workspace: &str,
    scope: &str,
    kind: &str,
    config: &Value,
    reason: &str,
) -> Result<()> {
    if !["neo4j", "qdrant"].contains(&kind) {
        return Err(Error::bad("Unsupported projection cleanup"));
    }
    let stamp = now();
    let config_hash = address_hash(config);
    crate::store::graph_for(db)?.transaction(|g| {
        let existing = graph::one(
            g,
            "MATCH (c:ProjectionCleanup {scope: $scope, kind: $kind, config_hash: $hash}) \
             RETURN c.revision AS revision, c.state AS state",
            graph::params(&json!({"scope": scope, "kind": kind, "hash": config_hash})),
        )?;
        let mut props = json!({
            "workspace_id": workspace, "scope": scope, "kind": kind,
            "config": config.to_string(), "config_hash": config_hash,
            "not_before": stamp, "reason": reason, "updated_at": stamp,
        });
        match existing {
            Some(row) => {
                // The upsert's revision bump; a running cleanup keeps running.
                props["revision"] = json!(row["revision"].as_i64().unwrap_or(0) + 1);
                props["state"] = json!(if s(&row, "state") == "running" {
                    "running"
                } else {
                    "pending"
                });
                graph::write(
                    g,
                    "MATCH (c:ProjectionCleanup {scope: $scope, kind: $kind, config_hash: $hash}) SET c += $props",
                    graph::params(
                        &json!({"scope": scope, "kind": kind, "hash": config_hash, "props": props}),
                    ),
                )?;
            }
            None => {
                props["id"] = json!(id());
                props["revision"] = json!(1);
                props["state"] = json!("pending");
                props["attempts"] = json!(0);
                props["created_at"] = json!(stamp);
                graph::write(
                    g,
                    "CREATE (c:ProjectionCleanup {id: $id}) SET c += $props",
                    graph::params(&json!({"id": props["id"], "props": props})),
                )?;
            }
        }
        Ok(())
    })
}
pub fn ready(db: &Connection, repository: &str, kind: &str, config: &Value) -> Result<()> {
    if crate::store::graph_for(db)?.read(|g| {
        graph::one(
            g,
            "MATCH (c:ProjectionCleanup {scope: $scope, kind: $kind, config_hash: $hash}) \
             WHERE c.state <> 'complete' RETURN c.id AS id LIMIT 1",
            graph::params(&json!({"scope": repository, "kind": kind, "hash": address_hash(config)})),
        )
    })?
    .is_some()
    {
        return Err(Error::conflict("External cleanup is pending. Run the worker or retry cleanup before publishing this projection"));
    }
    Ok(())
}
/// Changes even when cleanup finishes during an in-flight publication.
pub fn marker(db: &Connection, scope: &str, kind: &str, config: &Value) -> Result<i64> {
    Ok(crate::store::graph_for(db)?
        .read(|g| {
            graph::one(
                g,
                "MATCH (c:ProjectionCleanup {scope: $scope, kind: $kind, config_hash: $hash}) RETURN c.revision AS revision",
                graph::params(&json!({"scope": scope, "kind": kind, "hash": address_hash(config)})),
            )
        })?
        .and_then(|v| v["revision"].as_i64())
        .unwrap_or(0))
}

/// A cleanup node as the plain record its SQL row was (`SELECT *`).
const CLEANUP_FIELDS: &str = "c.id AS id, c.workspace_id AS workspace_id, c.scope AS scope, \
     c.kind AS kind, c.config AS config, c.config_hash AS config_hash, c.revision AS revision, \
     c.state AS state, c.attempts AS attempts, c.lease_token AS lease_token, \
     c.lease_until AS lease_until, c.not_before AS not_before, c.reason AS reason, \
     c.error AS error, c.created_at AS created_at, c.updated_at AS updated_at";

pub fn status(store: &Store, workspace: &str, token: &str) -> Result<Value> {
    store.authorized(workspace, token, "admin", |db, _| {
        Ok(json!({"cleanups": crate::store::graph_for(db)?.read(|g| {
            graph::rows(
                g,
                &format!("MATCH (c:ProjectionCleanup {{workspace_id: $ws}}) \
                     RETURN c.id AS id, c.scope AS scope, c.kind AS kind, c.state AS state, \
                            c.attempts AS attempts, c.reason AS reason, c.error AS error, \
                            c.created_at AS created_at, c.updated_at AS updated_at \
                     ORDER BY c.updated_at DESC LIMIT 200"),
                graph::params(&json!({"ws": workspace})),
            )
        })?}))
    })
}
pub fn retry(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    if let Some(reference) = args["credential_ref"].as_str() {
        store.resolve_credential(workspace, reference)?;
    }
    store.authorized(workspace, token, "admin", |db, _| {
        let identifier = crate::string(args, "cleanup_id", 100)?;
        crate::store::graph_for(db)?.transaction(|g| {
            let row = graph::one(
                g,
                &format!(
                    "MATCH (c:ProjectionCleanup {{workspace_id: $ws, id: $id}}) RETURN {CLEANUP_FIELDS}"
                ),
                graph::params(&json!({"ws": workspace, "id": identifier})),
            )?
            .ok_or_else(|| Error::bad("Cleanup unavailable"))?;
            if row["state"] == "running" {
                return Err(Error::conflict("Cleanup is already running"));
            }
            let mut cfg: Value = serde_json::from_str(s(&row, "config"))?;
            if let Some(reference) = args["credential_ref"].as_str() {
                cfg["credential_ref"] = json!(reference);
            }
            graph::write(
                g,
                "MATCH (c:ProjectionCleanup {id: $id}) \
                 SET c.config = $config, c.state = 'pending', c.not_before = $now, c.updated_at = $now \
                 REMOVE c.error",
                graph::params(
                    &json!({"id": identifier, "config": cfg.to_string(), "now": now()}),
                ),
            )?;
            Ok(json!({"queued": true, "id": identifier}))
        })
    })
}
pub async fn once(store: &Store, client: &reqwest::Client) -> Result<bool> {
    let graph_store = store.graph();
    let leased = graph_store.transaction(|g| {
        let stamp = now();
        graph::write(
            g,
            "MATCH (c:ProjectionCleanup) WHERE c.state = 'running' AND c.lease_until < $now \
             SET c.state = 'pending' REMOVE c.lease_token",
            graph::params(&json!({"now": stamp})),
        )?;
        let Some(mut row) = graph::one(
            g,
            &format!(
                "MATCH (c:ProjectionCleanup) WHERE c.state = 'pending' AND c.not_before <= $now \
                 RETURN {CLEANUP_FIELDS} ORDER BY c.updated_at LIMIT 1"
            ),
            graph::params(&json!({"now": stamp})),
        )?
        else {
            return Ok(None);
        };
        let lease = id();
        let until = (chrono::Utc::now() + chrono::Duration::seconds(300))
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        graph::write(
            g,
            "MATCH (c:ProjectionCleanup {id: $id}) \
             SET c.state = 'running', c.attempts = coalesce(c.attempts, 0) + 1, \
                 c.lease_token = $lease, c.lease_until = $until, c.updated_at = $now",
            graph::params(
                &json!({"id": s(&row, "id"), "lease": lease, "until": until, "now": now()}),
            ),
        )?;
        row["lease_token"] = json!(lease);
        Ok(Some(row))
    })?;
    let Some(row) = leased else { return Ok(false) };
    let config: Value = serde_json::from_str(s(&row, "config"))?;
    let result = match s(&row, "kind") {
        "neo4j" => {
            crate::graph::clear_projection(
                store,
                client,
                s(&row, "workspace_id"),
                &config,
                s(&row, "scope"),
            )
            .await
        }
        "qdrant" => {
            crate::vector::clear_projection(
                store,
                client,
                s(&row, "workspace_id"),
                &config,
                s(&row, "scope"),
            )
            .await
        }
        _ => Err(Error::bad("Unknown external projection")),
    };
    graph_store.transaction(|g| {
        let current = graph::one(
            g,
            &format!(
                "MATCH (c:ProjectionCleanup {{id: $id, lease_token: $lease}}) RETURN {CLEANUP_FIELDS}"
            ),
            graph::params(&json!({"id": s(&row, "id"), "lease": s(&row, "lease_token")})),
        )?
        .ok_or_else(|| Error::conflict("Cleanup lease changed"))?;
        let again = current["revision"] != row["revision"];
        let state = if result.is_ok() && !again {
            "complete"
        } else {
            "pending"
        };
        let delay = if result.is_ok() {
            0
        } else {
            30i64
                .saturating_mul(1i64 << current["attempts"].as_u64().unwrap_or(1).min(7))
                .min(3600)
        };
        let later = (chrono::Utc::now() + chrono::Duration::seconds(delay))
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let error = result.as_ref().err().map(|e| e.message.clone());
        let mut props = json!({
            "state": state, "not_before": later, "updated_at": now(),
        });
        if let Some(message) = &error {
            props["error"] = json!(message);
        }
        if error.is_some() {
            graph::write(
                g,
                "MATCH (c:ProjectionCleanup {id: $id, lease_token: $lease}) \
                 SET c += $props REMOVE c.lease_token, c.lease_until",
                graph::params(
                    &json!({"id": s(&row, "id"), "lease": s(&row, "lease_token"), "props": props}),
                ),
            )?;
        } else {
            graph::write(
                g,
                "MATCH (c:ProjectionCleanup {id: $id, lease_token: $lease}) \
                 SET c += $props REMOVE c.lease_token, c.lease_until, c.error",
                graph::params(
                    &json!({"id": s(&row, "id"), "lease": s(&row, "lease_token"), "props": props}),
                ),
            )?;
        }
        Ok(())
    })?;
    Ok(true)
}
