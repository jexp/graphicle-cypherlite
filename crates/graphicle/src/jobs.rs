//! Leased, bounded jobs. Network calls never hold a graph write transaction.
//!
//! The queue lives in the store's graph ([`crate::store_graph::GraphStore`])
//! as `(:Job)-[:OF]->(:Project)` nodes with `(:ModelCall)-[:FOR]->(:Job)` and
//! `(:Reservation)-[:FOR]->(:Job)` accounting and `:ModelCache` /
//! `:QueryCache` / `:Embedding` cache nodes (migration plan §2.5). Other
//! domains (sources, memory, connectors) are still read through the SQL
//! connection until their own migration nodes land.
use crate::{Error, Result, digest, id, integer, now, string};
use crate::{
    memory, model,
    store::{self, Principal, Store, authorize_hash, decode, one, repo, s},
    store_graph::{self as graph, GraphStore},
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicBool, Ordering},
};
/// A graph-store connection (cypherlite), as handed out by
/// [`GraphStore::connection`].
use cypherlite::Connection as GConnection;

// All work is serialized by repository unless both jobs explicitly opt into
// independent observer sources. Prefix intersection is conservative: even a
// newly ingested source cannot belong to both groups. `STARTS WITH` compares
// strings; UTF-8 prefixes of valid strings agree with the byte-prefix
// comparison the SQL queue used.
const RUNNING_CONFLICT: &str = r#"
NOT EXISTS {
    MATCH (r:Job {repository_id: j.repository_id})
    WHERE r.state='running'
    AND NOT coalesce(
        j.kind='observer_notes' AND r.kind='observer_notes'
        AND j.parallel_sources = true
        AND r.parallel_sources = true
        AND size(j.include) > 0
        AND size(r.include) > 0
        AND NONE(a IN j.include WHERE ANY(b IN r.include
            WHERE a STARTS WITH b OR b STARTS WITH a)),
        false
    )
}"#;

static READY_JOB_QUERY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "MATCH (j:Job) WHERE j.state IN ['queued','retry'] AND j.not_before <= $now AND {RUNNING_CONFLICT} RETURN j.kind AS kind ORDER BY j.priority DESC, j.created_at LIMIT 1"
    )
});

// The candidate list for leasing. The provider-permission filter of the old
// SQL query (a `connector` job whose connector syncs provider permissions
// waits for a program that can run it) is applied in Rust against the SQL
// store: `am_connectors` has not migrated yet, so it cannot join here.
static LEASE_JOB_QUERY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "MATCH (j:Job) WHERE j.state IN ['queued','retry'] AND j.not_before <= $now AND NOT j.kind IN $excluded AND j.kind IN $known AND ($repo IS NULL OR (j.repository_id = $repo AND j.kind = $kind)) AND {RUNNING_CONFLICT} RETURN j.id AS id, j.repository_id AS repository_id, j.workspace_id AS workspace_id, j.kind AS kind, j.principal_hash AS principal_hash, j.payload AS payload ORDER BY j.priority DESC, j.created_at LIMIT 100"
    )
});

/// Lease CAS: take `id` only while it is still ready and unblocked, setting
/// the lease in the same statement (migration plan §2.5).
static LEASE_CAS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "MATCH (j:Job {{id: $id}}) WHERE j.state IN ['queued','retry'] AND j.not_before <= $now AND {RUNNING_CONFLICT} SET j.state='running', j.attempt=j.attempt+1, j.lease_token=$lease, j.lease_until=$until, j.updated_at=$now RETURN j.id AS id"
    )
});

/// A `Job` row as plain job properties (no node envelope).
const JOB_FIELDS: &str = "j.id AS id, j.repository_id AS repository_id, j.workspace_id AS workspace_id, j.kind AS kind, j.input_hash AS input_hash, j.payload AS payload, j.principal_hash AS principal_hash, j.state AS state, j.priority AS priority, j.attempt AS attempt, j.max_attempts AS max_attempts, j.not_before AS not_before, j.lease_token AS lease_token, j.lease_until AS lease_until, j.result AS result, j.error AS error, j.created_at AS created_at, j.updated_at AS updated_at";

/// Drop every cached model result of one repository (a connector's scope or
/// credential changed; its cached answers may include now-private data).
pub(crate) fn clear_model_cache(graph: &GraphStore, repository: &str) -> Result<()> {
    graph.transaction(|db| {
        graph::rows(
            db,
            "MATCH (c:ModelCache {repository_id: $repo}) DELETE c",
            graph::params(&json!({"repo": repository})),
        )?;
        Ok(())
    })
}

/// The `:Job` node properties of a scheduled connector sync
/// (`connectors::schedule_connectors` MERGEs on the idempotence key and sets
/// these on create).
pub(crate) fn scheduled_node(connector: &Value, payload: &Value) -> Value {
    let stamp = now();
    let encoded = payload.to_string();
    json!({
        "id": id(), "repository_id": s(connector, "repository_id"),
        "workspace_id": s(connector, "workspace_id"),
        "kind": "connector", "payload": encoded, "payload_bytes": encoded.len(),
        "principal_hash": s(connector, "capability_hash"), "state": "queued",
        "priority": 0, "attempt": 0, "max_attempts": 1, "not_before": stamp,
        "created_at": stamp, "updated_at": stamp,
        "connector_id": s(connector, "id"),
    })
}

/// Read one job by id, or `None` (`SELECT * FROM am_jobs WHERE id=?`). The
/// payload comes back decoded (it is stored as a string property).
fn job_row(db: &mut GConnection, job_id: &str) -> Result<Option<Value>> {
    let mut row = graph::one(
        db,
        &format!("MATCH (j:Job {{id: $id}}) RETURN {JOB_FIELDS}"),
        graph::params(&json!({"id": job_id})),
    )?;
    if let Some(row) = &mut row {
        decode(row, "payload")?;
    }
    Ok(row)
}

/// Insert a queued job node and, when the graph knows the project, its
/// `[:OF]` edge (the content-store node migrates `Project` later; the edge
/// attaches from then on).
fn insert_job(
    db: &mut GConnection,
    repository: &str,
    workspace: &str,
    kind: &str,
    input_hash: &str,
    payload: &Value,
    principal_hash: &str,
    priority: i64,
    max_attempts: i64,
) -> Result<String> {
    let job_id = id();
    let stamp = now();
    let encoded = payload.to_string();
    // The payload is an opaque string property (the engine cannot store maps
    // as property values); the two fields the ready-queue's source-conflict
    // rule needs are duplicated as scalar/list properties.
    let mut node = json!({
        "id": job_id, "repository_id": repository, "workspace_id": workspace,
        "kind": kind, "input_hash": input_hash, "payload": encoded,
        "payload_bytes": encoded.len(), "principal_hash": principal_hash,
        "state": "queued", "priority": priority, "attempt": 0,
        "max_attempts": max_attempts, "not_before": stamp,
        "created_at": stamp, "updated_at": stamp,
    });
    if let Some(episode) = payload["episode"].as_str() {
        node["episode"] = json!(episode);
    }
    if let Some(connector) = payload["connector_id"].as_str() {
        node["connector_id"] = json!(connector);
    }
    if let Some(auto) = payload["auto"].as_object() {
        if let Some(connector) = auto["connector_id"].as_str() {
            node["auto_connector"] = json!(connector);
        }
        if let Some(generation) = auto["generation"].as_i64() {
            node["auto_generation"] = json!(generation);
        }
    }
    if let Some(flag) = payload["parallel_sources"].as_bool() {
        node["parallel_sources"] = json!(flag);
    }
    if let Some(include) = payload["include"].as_array() {
        node["include"] = json!(include.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>());
    }
    graph::rows(db, "CREATE (j:Job {id: $id}) SET j += $job", graph::params(&json!({"id": job_id, "job": node})))?;
    graph::rows(
        db,
        "MATCH (j:Job {id: $id}), (p:Project {id: $repo}) CREATE (j)-[:OF]->(p)",
        graph::params(&json!({"id": job_id, "repo": repository})),
    )?;
    Ok(job_id)
}

/// Digest of the code job results depend on, recorded in job payloads (a
/// queued workflow's replay key includes it). Only files of this crate and
/// the memory engine are hashed, never an extension's, so every edition
/// computes the same revision.
pub fn runtime_revision() -> &'static str {
    static REVISION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    REVISION.get_or_init(||digest(&json!({"version":env!("CARGO_PKG_VERSION"),"workflows":include_str!("workflows.rs"),"observer":include_str!("observer.rs"),"search":include_str!("search.rs"),"query":include_str!("query.rs"),"workflow_retrieval":include_str!("workflow_retrieval.rs"),"evidence":include_str!("evidence.rs"),"memory_evidence":include_str!("memory_evidence.rs"),"memory":include_str!("memory.rs"),"graph":include_str!("graph.rs"),"models":include_str!("model.rs"),"claims":include_str!("claims.rs"),"answer_review":include_str!("answer_review.rs"),"connectors":include_str!("connectors.rs"),"atlassian":include_str!("atlassian.rs"),"projection":include_str!("projection.rs"),"permissions":include_str!("permissions.rs"),"semantic_contract":include_str!("../../graphicle-memory/src/semantic.rs"),"retrieval":include_str!("../../graphicle-memory/src/retrieval.rs"),"packing":include_str!("../../graphicle-memory/src/packing.rs"),"ingest":include_str!("../../graphicle-memory/src/ingest.rs"),"text":include_str!("../../graphicle-memory/src/text.rs"),"core":graphicle_memory::structure::REVISION})))
}

#[derive(Clone)]
pub struct Job {
    pub id: String,
    pub repository: String,
    pub workspace: String,
    pub principal_hash: String,
    pub lease: String,
    pub kind: String,
    pub payload: Value,
}
/// Source work (connector syncs, permission refreshes and probes, and
/// extension kinds declared as source work): no model call, no pinned source
/// state.
pub fn source_task(kind: &str) -> bool {
    ["connector", "permissions", "connector_probe"].contains(&kind)
        || crate::extension::installed()
            .job_kind(kind)
            .is_some_and(|declared| declared.source)
}
/// The job kind of a provider-permission refresh. The core knows its name
/// (permission, job status) but leaves running it to an extension that
/// syncs provider permissions.
pub const PERMISSIONS: &str = "permissions";
/// What a job of a kind this program does not know waits for.
pub const UNKNOWN_KIND: &str = "Waiting for a Graphicle edition that runs this kind of job";
/// Why this program can never run a job of `kind` for `payload`'s connector:
/// an unsupported kind (see [`crate::archived`]), a kind it does not know
/// (queued by an edition with more extensions), or provider-permission work,
/// which needs an extension that syncs provider permissions. Such a job is
/// never leased here, so a program that can run it still finds it queued.
pub fn unavailable(
    db: &Connection,
    repository: &str,
    kind: &str,
    payload: &Value,
) -> Result<Option<String>> {
    if let Some(archived) = crate::archived::kind(kind) {
        return Ok(Some(archived.waiting()));
    }
    if permission(kind).is_err() {
        return Ok(Some(UNKNOWN_KIND.to_string()));
    }
    if crate::connectors::provider_permission_sync() {
        return Ok(None);
    }
    let provider = match kind {
        PERMISSIONS => true,
        "connector" => one(
            db,
            "SELECT 1 AS provider FROM am_connectors WHERE repository_id=? AND id=? AND json_extract(config,'$.permission_mode')='provider'",
            params![repository, s(payload, "connector_id")],
        )?
        .is_some(),
        _ => false,
    };
    Ok(provider.then(|| crate::connectors::PROVIDER_SYNC_UNAVAILABLE.to_string()))
}
/// Every job kind the core knows, with the permission its requester needs.
/// `permissions` is known by name; an extension that syncs provider
/// permissions runs it.
const KINDS: &[(&str, &str)] = &[
    ("connector", "admin"),
    (PERMISSIONS, "admin"),
    ("connector_probe", "admin"),
    ("reflect", "run"),
    ("embed", "run"),
    ("recall", "run"),
    ("basic", "run"),
    ("extract", "propose"),
    ("refresh", "propose"),
    ("project", "read"),
    ("link", "read"),
    (crate::observer::KIND, "propose"),
];
/// Whether `kind` is a job kind of the core.
pub fn core_kind(kind: &str) -> bool {
    KINDS.iter().any(|(name, _)| *name == kind)
}
/// The permission a job of `kind` needs: the core's kinds, then the job
/// kinds installed extensions declare. An unsupported kind has none: it is
/// refused with its archive folder.
pub fn permission(kind: &str) -> Result<&'static str> {
    if let Some(archived) = crate::archived::kind(kind) {
        return Err(Error::bad(archived.message()));
    }
    KINDS
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, permission)| *permission)
        .or_else(|| {
            crate::extension::installed()
                .job_kind(kind)
                .map(|declared| declared.permission)
        })
        .ok_or_else(|| Error::bad("Unknown workflow"))
}
/// Every job kind this program knows: the core's and the installed
/// extensions' declared kinds, never an unsupported one.
fn known_kinds() -> Vec<&'static str> {
    KINDS
        .iter()
        .map(|(name, _)| *name)
        .chain(crate::extension::installed().job_kinds())
        .filter(|kind| crate::archived::kind(kind).is_none())
        .collect()
}

/// The `input_hash` of the one `project` job per head-set episode.
pub fn projection_input_hash(episode: &str) -> String {
    digest(&json!({"kind":"project","episode":episode}))
}

/// The state of the projection of `episode`, a head set of `repository`,
/// over every `project` job queued for it: the sync's own job and any retry
/// of it, which has an input hash of its own. `running`
/// while one runs, else `queued` while one waits (queued or retry), else
/// `failed` when the newest one failed, else `none`: no job, a job that
/// finished without recording the head set (the heads had moved on), or a
/// cancelled one. Nothing will then project it until the next sync.
pub fn projection_state(
    graph: &GraphStore,
    repository: &str,
    episode: &str,
) -> Result<&'static str> {
    let states: Vec<String> = graph
        .read(|db| {
            graph::rows(
                db,
                "MATCH (j:Job {repository_id: $repo, kind: 'project', episode: $episode}) \
                 RETURN j.state AS state ORDER BY j.created_at DESC, j.id DESC",
                graph::params(&json!({"repo": repository, "episode": episode})),
            )
        })?
        .iter()
        .map(|row| s(row, "state").to_string())
        .collect();
    Ok(if states.iter().any(|state| state == "running") {
        "running"
    } else if states
        .iter()
        .any(|state| state == "queued" || state == "retry")
    {
        "queued"
    } else if states.first().is_some_and(|state| state == "failed") {
        "failed"
    } else {
        "none"
    })
}

/// Queue a graph projection of the repository's current head set, labelled
/// with the commit identity when the sync metadata carries one. One job per
/// head-set episode: re-committing identical content replays the existing job.
/// A sync is a request for the head set's graph, so while the
/// episode is not recorded it gives that job a fresh start when it failed,
/// was cancelled or finished as a skip, and lets one that waits to retry run
/// at once. A restarted job runs as the syncing principal, and so does a
/// waiting one whose principal can no longer read the project (a revoked or
/// expired key would have it cancelled when leased).
pub fn enqueue_projection(
    store: &Store,
    db: &Connection,
    repository: &str,
    p: &Principal,
    snapshot: &str,
    metadata: &Value,
) -> Result<Value> {
    let episode = crate::graph_projection::current_episode(db, repository)?;
    let mut labels: Vec<String> = Vec::new();
    for key in ["commit", "commit_sha", "sha", "revision", "ref"] {
        if let Some(label) = metadata[key]
            .as_str()
            .filter(|v| !v.is_empty() && v.len() <= 128)
            && !labels.contains(&label.to_string())
        {
            labels.push(label.to_string());
        }
    }
    if !snapshot.is_empty() && !labels.contains(&snapshot.to_string()) {
        labels.push(snapshot.to_string());
    }
    let input_hash = projection_input_hash(&episode);
    let graph = store.graph().clone();
    let existing = graph.read(|db| {
        graph::one(
            db,
            "MATCH (j:Job {repository_id: $repo, kind: 'project', input_hash: $hash}) \
             RETURN j.id AS id, j.state AS state, j.result AS result, j.payload AS payload, j.principal_hash AS principal_hash",
            graph::params(&json!({"repo": repository, "hash": input_hash})),
        )
    })?;
    if let Some(mut existing) = existing {
        // Fold in any new label so a later commit of identical content is still named.
        decode(&mut existing, "payload")?;
        let mut payload = existing["payload"].clone();
        let mut known: Vec<String> = payload["labels"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut grew = false;
        let mut state = existing["state"].clone();
        for label in &labels {
            if !known.contains(label) {
                known.push(label.clone());
                grew = true;
            }
        }
        if grew {
            payload["labels"] = json!(known);
            graph.transaction(|db| {
                graph::rows(
                    db,
                    "MATCH (j:Job {id: $id}) SET j.payload = $payload, j.updated_at = $now",
                    graph::params(&json!({"id": s(&existing, "id"), "payload": payload.to_string(), "now": now()})),
                )?;
                Ok(())
            })?;
        }
        if crate::graph_projection::is_recorded(db, repository, &episode)? {
            // A sync back to a recorded head set queues no
            // projection, so the live-symbol table follows it here. A
            // failure only leaves the table behind, which the symbol check
            // reads as `projection_pending`; the sync goes on.
            let _ = crate::symbols::ensure(db, &graph, repository, &episode);
            if grew {
                // The episode already exists; attach the new labels directly.
                for label in &labels {
                    crate::graph_projection::label_episode(
                        db,
                        &p.workspace,
                        repository,
                        label,
                        &episode,
                    )?;
                }
            }
        } else {
            // The principal a waiting job keeps: its own while it can still
            // read the project, else this sync's, so that leasing it does not
            // cancel it ("Credential unavailable") and leave the head set
            // without its episode for good.
            let waiting_principal = || -> Result<String> {
                let original = s(&existing, "principal_hash");
                let readable = authorize_hash(db, &p.workspace, original, permission("project")?)
                    .and_then(|principal| {
                        crate::graph_projection::repo_readable(db, &principal, repository)
                    })
                    .unwrap_or(false);
                Ok(if readable {
                    original.to_string()
                } else {
                    p.hash.clone()
                })
            };
            match s(&existing, "state") {
                // A sync is a request for its head set's graph. A
                // job that failed or was cancelled starts again with a fresh
                // budget, and one that finished as a skip (the head set had
                // moved on before it ran) runs again, with every label, now the
                // tree is current again. It runs as this sync's principal.
                "failed" | "cancelled" | "complete" => {
                    graph.transaction(|db| {
                        graph::rows(
                            db,
                            "MATCH (j:Job {id: $id}) SET j.state='queued', j.attempt=0, j.not_before=$now, j.updated_at=$now, j.error=NULL, j.lease_token=NULL, j.lease_until=NULL, j.principal_hash=$principal",
                            graph::params(&json!({"id": s(&existing, "id"), "now": now(), "principal": p.hash})),
                        )?;
                        Ok(())
                    })?;
                    state = json!("queued");
                }
                // One that waits to retry may run at once.
                "retry" => {
                    graph.transaction(|db| {
                        graph::rows(
                            db,
                            "MATCH (j:Job {id: $id}) SET j.not_before=$now, j.updated_at=$now, j.principal_hash=$principal",
                            graph::params(&json!({"id": s(&existing, "id"), "now": now(), "principal": waiting_principal()?})),
                        )?;
                        Ok(())
                    })?;
                }
                "queued" => {
                    let principal = waiting_principal()?;
                    if principal != s(&existing, "principal_hash") {
                        graph.transaction(|db| {
                            graph::rows(
                                db,
                                "MATCH (j:Job {id: $id}) SET j.updated_at=$now, j.principal_hash=$principal",
                                graph::params(&json!({"id": s(&existing, "id"), "now": now(), "principal": principal})),
                            )?;
                            Ok(())
                        })?;
                    }
                }
                _ => {}
            }
        }
        return Ok(
            json!({"id":existing["id"],"state":state,"kind":"project","episode":episode,"replayed":true}),
        );
    }
    let repository_row = repo(db, p, repository)?;
    let source_state = memory::state(&memory::documents(db, &repository_row, p)?);
    let payload = json!({"kind":"project","episode":episode,"labels":labels,"snapshot":snapshot,"runtime_revision":runtime_revision(),"source_state":source_state});
    crate::ingestion::check_queue_bytes(
        &graph,
        repository,
        payload.to_string().len(),
        crate::ingestion::DEFAULT_QUEUED_BYTES,
    )?;
    // Background derivation: never ahead of work a caller is waiting on.
    let job_id = graph.transaction(|db| {
        insert_job(db, repository, &p.workspace, "project", &input_hash, &payload, &p.hash, -1, 3)
    })?;
    store::event(
        db,
        repository,
        "workflow_queued",
        json!({"id":job_id,"kind":"project","episode":episode}),
    )?;
    Ok(json!({"id":job_id,"state":"queued","kind":"project","episode":episode,"replayed":false}))
}

/// How often a running `project` or `link` job renews its 240 s lease
///: a projection that takes longer than the lease is then
/// neither failed by its own lease check nor taken over by another process.
pub const LEASE_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run `work` while a thread renews `job`'s lease every `every` ([`renew`]:
/// only while the job is still `running` under this lease, so a job that was
/// cancelled or taken over is left alone). A beat that finds the store busy
/// for a second is skipped; the next one tries again. Returns once `work`
/// has returned and the heartbeat has stopped.
pub fn with_lease_heartbeat<T>(
    store: &Store,
    job: &Job,
    every: std::time::Duration,
    work: impl FnOnce() -> T,
) -> T {
    let (done, beats) = std::sync::mpsc::channel::<()>();
    let graph = store.graph().clone();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = beats.recv_timeout(every) {
                let _ = renew(&graph, job);
            }
        });
        let value = work();
        drop(done);
        value
    })
}

/// Execute a `project` job: build (or replay) the episode for the head set
/// recorded at enqueue time and attach its labels. Skips when the head set
/// moved on — the newer commit queued its own projection. The job's lease
/// is renewed while it runs ([`with_lease_heartbeat`]).
pub fn run_projection(store: &Store, job: &Job) -> Result<Value> {
    with_lease_heartbeat(store, job, LEASE_HEARTBEAT, || project_job(store, job))
}

/// The job's current lease fields, or `None` when the job is gone.
fn lease_row(graph: &GraphStore, job_id: &str) -> Result<Option<Value>> {
    graph.read(|db| {
        graph::one(
            db,
            "MATCH (j:Job {id: $id}) RETURN j.state AS state, j.lease_token AS lease_token, j.lease_until AS lease_until",
            graph::params(&json!({"id": job_id})),
        )
    })
}

/// The job still holds `lease` (checked against the graph store).
fn check_lease(graph: &GraphStore, job_id: &str, lease: &str) -> Result<()> {
    let row = lease_row(graph, job_id)?.ok_or_else(|| Error::conflict("Job was removed"))?;
    if row["state"] != "running"
        || s(&row, "lease_token") != lease
        || s(&row, "lease_until") < now().as_str()
    {
        return Err(Error::conflict("Job lease expired or was cancelled"));
    }
    Ok(())
}

fn project_job(store: &Store, job: &Job) -> Result<Value> {
    let expected = s(&job.payload, "episode").to_string();
    let labels: Vec<String> = job.payload["labels"]
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let scope = |db: &Connection| -> Result<(Principal, String, String)> {
        check_lease(&store.graph(), &job.id, &job.lease)?;
        let p = authorize_hash(db, &job.workspace, &job.principal_hash, "read")?;
        let identifier = s(&repo(db, &p, &job.repository)?, "id").to_string();
        let current = crate::graph_projection::current_episode(db, &identifier)?;
        Ok((p, identifier, current))
    };
    let skipped = |current: &str| json!({"skipped":true,"reason":"head set changed before projection","episode":expected,"current_episode":current});
    // Build on a read-only snapshot: extraction never holds the write lock,
    // so other writers are not kept waiting while a large repository parses.
    let built = store.read(|db| {
        let (_, identifier, current) = scope(db)?;
        if current != expected {
            return Ok(Err(current));
        }
        Ok(Ok(crate::graph_projection::prebuild(db, &identifier)?))
    })?;
    let built = match built {
        Ok(built) => built,
        Err(current) => return Ok(skipped(&current)),
    };
    store.transaction(|db| {
        let (p, identifier, current) = scope(db)?;
        let identifier = identifier.as_str();
        if current != expected {
            return Ok(skipped(&current));
        }
        let info = crate::graph_projection::project_prebuilt(db, &store.graph(), identifier, &p, built)?;
        for label in &labels {
            crate::graph_projection::label_episode(
                db,
                &p.workspace,
                identifier,
                label,
                &info.episode,
            )?;
        }
        let mut out = serde_json::to_value(&info)?;
        out["labels"] = json!(labels);
        // Cross-repository import linkage follows every projection as its own
        // `link` job so the workspace's `gf_cross_edges` track the new episode
        // without extending the projection transaction; a failure to queue it
        // must not fail the projection itself.
        out["cross_repo_link"] = match enqueue_link(store, db, identifier, &p, &info.episode) {
            Ok(queued) => queued,
            Err(error) => json!({"error": error.message}),
        };
        // Work extensions queue after a projection, which the projection
        // drain runs next. Listed only when an
        // extension queued something, so Core's result is unchanged.
        let followed =
            crate::extension::installed().after_projection(&store.graph(), db, &p, identifier, &info.episode);
        if !followed.is_empty() {
            out["after_projection"] = json!(followed);
        }
        // Bound the per-episode by-products (coverage, closed cross edges,
        // unresolved calls) to the configured retention; never nodes/edges.
        // A trim that removed rows is receipted like any other deletion.
        let retention = crate::retention::Retention::from_env();
        out["retention"] = match crate::retention::apply(db, identifier, &retention) {
            Ok(applied) => {
                let removed = applied["deleted"].clone();
                let any = removed
                    .as_object()
                    .is_some_and(|counts| counts.values().any(|n| n.as_u64().unwrap_or(0) > 0));
                if any {
                    let signed = crate::retention::remaining(db, identifier, &retention).and_then(
                        |remaining| {
                            crate::deletion::rows_receipt(
                                db,
                                &p,
                                identifier,
                                "retention.graph",
                                "graph_by_products",
                                json!({"retention": retention, "trigger": "projection"}),
                                removed.clone(),
                                &[],
                                remaining,
                                false,
                            )
                        },
                    );
                    out["retention_receipt"] = match signed {
                        Ok(receipt) => receipt["id"].clone(),
                        Err(error) => json!({"error": error.message}),
                    };
                }
                removed
            }
            Err(error) => json!({"error": error.message}),
        };
        Ok(out)
    })
}

/// Queue one cross-repository `link` run for the workspace after `episode`
/// of `repository` was recorded, or after a connector sync or import changed
/// its documents. Idempotent per workspace: a `link`
/// job that is still waiting (queued or retry) covers every later change
/// too, so it is replayed instead of duplicated; a job already running is
/// *not* reused, because its pass may have read the store before this
/// change. A running job with this very input (a connector sync leaves the
/// episode as it was) gets a follow-up job of its own, which waits for it.
pub fn enqueue_link(
    store: &Store,
    db: &Connection,
    repository: &str,
    p: &Principal,
    episode: &str,
) -> Result<Value> {
    let graph = store.graph().clone();
    if let Some(waiting) = graph.read(|db| {
        graph::one(
            db,
            "MATCH (j:Job {kind: 'link', workspace_id: $ws}) WHERE j.state IN ['queued','retry'] \
             RETURN j.id AS id, j.state AS state ORDER BY j.created_at LIMIT 1",
            graph::params(&json!({"ws": p.workspace})),
        )
    })? {
        return Ok(
            json!({"id":waiting["id"],"state":waiting["state"],"kind":"link","replayed":true}),
        );
    }
    let mut input =
        json!({"kind":"link","workspace":p.workspace,"trigger":repository,"episode":episode});
    let stamp = now();
    let mut follows = None;
    // At most two rounds: a job with this input, then its follow-up, which
    // cannot be running while the first is (one job runs per project).
    for _ in 0..2 {
        let input_hash = digest(&input);
        let Some(existing) = graph.read(|db| {
            graph::one(
                db,
                "MATCH (j:Job {repository_id: $repo, kind: 'link', input_hash: $hash}) RETURN j.id AS id, j.state AS state",
                graph::params(&json!({"repo": repository, "hash": input_hash})),
            )
        })?
        else {
            break;
        };
        if existing["state"] == "running" {
            follows = Some(s(&existing, "id").to_string());
            input["after"] = existing["id"].clone();
            continue;
        }
        graph.transaction(|db| {
            graph::rows(
                db,
                "MATCH (j:Job {id: $id}) SET j.state='queued', j.not_before=$now, j.updated_at=$now, j.error=NULL, j.attempt=0",
                graph::params(&json!({"id": s(&existing, "id"), "now": stamp})),
            )?;
            Ok(())
        })?;
        return Ok(json!({"id":existing["id"],"state":"queued","kind":"link","replayed":true}));
    }
    let input_hash = digest(&input);
    let mut payload = json!({"kind":"link","workspace":p.workspace,"trigger":repository,"episode":episode,"runtime_revision":runtime_revision()});
    if let Some(running) = &follows {
        payload["after"] = json!(running);
    }
    let job_id = graph.transaction(|db| {
        insert_job(db, repository, &p.workspace, "link", &input_hash, &payload, &p.hash, -1, 3)
    })?;
    store::event(
        db,
        repository,
        "workflow_queued",
        json!({"id":job_id,"kind":"link","episode":episode}),
    )?;
    Ok(json!({"id":job_id,"state":"queued","kind":"link","replayed":false}))
}

/// Execute a `link` job: resolve imports between every readable, projected
/// repository of the workspace and persist `gf_cross_edges` /
/// `gf_cross_unresolved` (idempotent for unchanged episode pairs), then
/// recompute the documents' mention links to code symbols
/// ([`crate::doc_links::relink`]), whose summary is
/// the result's `documents`. A failure of the document step is reported
/// there and leaves the import links in place; a failure of the import step
/// still fails the job, after the document step has run. The job's lease is
/// renewed while it waits for the store and runs.
pub fn run_link(store: &Store, job: &Job) -> Result<Value> {
    with_lease_heartbeat(store, job, LEASE_HEARTBEAT, || link_job(store, job))
}

/// The job still holds its lease (checked against the graph store).
pub(crate) fn leased(graph: &GraphStore, job: &Job) -> Result<()> {
    check_lease(graph, &job.id, &job.lease)
}

fn link_job(store: &Store, job: &Job) -> Result<Value> {
    // The document step runs whatever the import step did: a project whose
    // graph cannot be read fails the import step for the workspace, but
    // leaves the other projects' documents linking.
    let imports = import_link_job(store, job);
    let graph = store.graph().clone();
    let documents =
        match crate::doc_links::relink(store, &job.workspace, &job.principal_hash, move |_| {
            leased(&graph, job)
        }) {
            Ok(summary) => summary,
            Err(error) => json!({"error": error.message}),
        };
    let mut result = imports?;
    result["documents"] = documents;
    Ok(result)
}

fn import_link_job(store: &Store, job: &Job) -> Result<Value> {
    store.transaction(|db| {
        leased(&store.graph(), job)?;
        let p = authorize_hash(db, &job.workspace, &job.principal_hash, "read")?;
        let summary = crate::graph_projection::link_workspace(db, &p)?;
        let mut result = json!({
            "edges": summary["edges"],
            "unresolved": summary["unresolved"],
            "pairs": summary["pairs"].as_array().map_or(0, Vec::len),
            "repositories": summary["repositories"].as_array().map_or(0, Vec::len),
            "hidden_repositories": summary["hidden_repositories"],
            "unprojected_repositories": summary["unprojected_repositories"],
            "trigger": job.payload["trigger"],
            "episode": job.payload["episode"],
        });
        if let Some(skipped) = summary.get("skipped") {
            result["skipped"] = skipped.clone();
        }
        Ok(result)
    })
}

/// Run `repository`'s waiting graph work in this
/// process, as a worker would. First every ready `project` job, then the
/// `link` jobs they queued (a workspace keeps one waiting `link` job, which
/// may belong to another of its projects), then the jobs of the kinds
/// extensions declare to follow projections
/// ([`crate::extension::JobKind::after_projection`]). `graphicle sync`, `mcp`,
/// `ask`, `run --wait` and `pr` call it; a query never does.
///
/// Leases make it safe beside a worker or another drain: a job another
/// process holds is left to it and never run twice. Returns one summary per
/// job run (`{job, kind, ok, ...}`). A `project` job that fails ends the
/// drain with its error; the job is set to retry or failed as a worker sets
/// it.
pub fn drain_projection(store: &Store, repository: &str) -> Result<Vec<Value>> {
    drain_projection_until(store, repository, None)
}

/// [`drain_projection`] that also stops between jobs once `stop` is set
/// (`graphicle mcp` sets it when its client disconnects).
pub fn drain_projection_until(
    store: &Store,
    repository: &str,
    stop: Option<&AtomicBool>,
) -> Result<Vec<Value>> {
    let stopped = || stop.is_some_and(|flag| flag.load(Ordering::Relaxed));
    let mut results = Vec::new();
    // Projects whose `link` jobs run here: this one, and the owner of each
    // waiting `link` job a projection replayed.
    let mut linked = vec![repository.to_string()];
    while !stopped() {
        let Some(job) = store.lease_job_of_kind(repository, "project")? else {
            break;
        };
        let result = crate::phase::phase("job_project", || run_projection(store, &job));
        let (summary, link) = match &result {
            Ok(value) => (
                json!({"job":job.id,"kind":"project","ok":true,"labels":value["labels"]}),
                value["cross_repo_link"]["id"].as_str().map(str::to_string),
            ),
            Err(error) => (
                json!({"job":job.id,"kind":"project","ok":false,"error":error.message}),
                None,
            ),
        };
        let failed = result.as_ref().err().map(|error| error.message.clone());
        // Finish the job before anything else can fail, so its lease is
        // released rather than left to expire.
        store.finish_job(&job, result)?;
        results.push(summary);
        if let Some(error) = failed {
            return Err(Error::bad(format!("Graph projection failed: {error}")));
        }
        if let Some(link) = link
            && let Some(owner) = store.graph().read(|db| {
                Ok(job_row(db, &link)?.map(|row| s(&row, "repository_id").to_string()))
            })?
            && !linked.contains(&owner)
        {
            linked.push(owner);
        }
    }
    for owner in &linked {
        while !stopped() {
            let Some(job) = store.lease_job_of_kind(owner, "link")? else {
                break;
            };
            let result = crate::phase::phase("job_link", || run_link(store, &job));
            results.push(match &result {
                Ok(value) => {
                    json!({"job":job.id,"kind":"link","ok":true,"edges":value["edges"],"unresolved":value["unresolved"]})
                }
                Err(error) => json!({"job":job.id,"kind":"link","ok":false,"error":error.message}),
            });
            store.finish_job(&job, result)?;
        }
    }
    let kinds = crate::extension::installed().post_projection_kinds();
    if !kinds.is_empty() && !stopped() {
        // Post-projection kinds make no model call (`JobKind::after_projection`).
        let worker = Worker::new(store.clone(), model::Config::default())?;
        for kind in kinds {
            while !stopped() {
                let Some(job) = store.lease_job_of_kind(repository, kind)? else {
                    break;
                };
                let result = run_on_own_runtime(&worker, &job);
                results.push(match &result {
                    Ok(_) => json!({"job":job.id,"kind":kind,"ok":true}),
                    Err(error) => {
                        json!({"job":job.id,"kind":kind,"ok":false,"error":error.message})
                    }
                });
                store.finish_job(&job, result)?;
            }
        }
    }
    Ok(results)
}

/// Whether `error` is SQLite's busy or locked error: another connection
/// held the write lock past the store's busy timeout (10 s). The store is
/// busy with another writer, such as another process persisting a large
/// projection; the work itself did not fail. The storage
/// error keeps only SQLite's text, in its two forms.
pub fn busy(error: &Error) -> bool {
    let message = error.message.to_ascii_lowercase();
    [
        "database is locked",
        "database table is locked",
        "database file is locked",
        "table in the database is locked",
    ]
    .iter()
    .any(|text| message.contains(text))
}

/// Run a leased job through [`crate::workflows::execute`] on a thread of its
/// own with a current-thread runtime, so synchronous callers and callers
/// inside the program's runtime can both wait for it.
fn run_on_own_runtime(worker: &Worker, job: &Job) -> Result<Value> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(crate::workflows::execute(worker, job))
            })
            .join()
            .unwrap_or_else(|_| Err(Error::bad("A job that follows the projection panicked")))
    })
}
#[track_caller]
pub fn guarded<T>(
    store: &Store,
    job: &Job,
    f: impl FnOnce(&Connection, &Principal, &Value) -> Result<T>,
) -> Result<T> {
    store.transaction(|db| {
        let (p, repository) = still_valid(store, db, job)?;
        f(db, &p, &repository)
    })
}
/// `guarded` for steps that only read: the same lease, authority and source
/// checks on a read-only snapshot, so the step never holds the write lock.
pub fn guarded_read<T>(
    store: &Store,
    job: &Job,
    f: impl FnOnce(&Connection, &Principal, &Value) -> Result<T>,
) -> Result<T> {
    store.read(|db| {
        let (p, repository) = still_valid(store, db, job)?;
        f(db, &p, &repository)
    })
}
/// The job still holds its lease, its principal still has the permission, and
/// the sources and reviewed memory it was queued against are unchanged.
fn still_valid(store: &Store, db: &Connection, job: &Job) -> Result<(Principal, Value)> {
    check_lease(&store.graph(), &job.id, &job.lease)?;
    let p = authorize_hash(
        db,
        &job.workspace,
        &job.principal_hash,
        permission(&job.kind)?,
    )?;
    let repository = repo(db, &p, &job.repository)?;
    if !source_task(&job.kind) {
        // One read of the sources serves both checks.
        let documents = memory::documents(db, &repository, &p)?;
        if memory::state(&documents) != s(&job.payload, "source_state") {
            return Err(Error::conflict(
                "Sources or source access changed during the job",
            ));
        }
        if job.payload["memory_state"].is_string()
            && memory::reviewed_state_of(db, &store.graph(), &repository, &documents)?
                != s(&job.payload, "memory_state")
        {
            return Err(Error::conflict("Reviewed memory changed during the job"));
        }
    }
    Ok((p, repository))
}

fn expire_leases(db: &mut GConnection, stamp: &str) -> Result<()> {
    graph::rows(
        db,
        "MATCH (j:Job) WHERE j.state='running' AND j.lease_until < $now \
         SET j.state = CASE WHEN j.attempt < j.max_attempts THEN 'retry' ELSE 'failed' END, \
             j.error = 'Worker lease expired; inference billing may be unknown', \
             j.lease_token = NULL, j.lease_until = NULL, j.updated_at = $now",
        graph::params(&json!({"now": stamp})),
    )?;
    Ok(())
}

/// Validate a workflow request before any database access; returns the
/// permission it needs. `config` is the workspace's effective configuration.
pub fn check(args: &Value, config: &model::Config) -> Result<&'static str> {
    let kind = string(args, "kind", 30)?;
    crate::archived::check(crate::archived::Surface::Workflow, args)?;
    if crate::ingestion::interactive(kind) {
        return Err(Error::bad(format!(
            "{kind} is an interactive operation; call it directly, it is never queued"
        )));
    }
    let permission = permission(kind)?;
    crate::workflow_retrieval::check(kind, args, config)?;
    if kind == crate::observer::KIND {
        crate::observer::validate_args(args)?;
    }
    if kind != crate::observer::KIND && args.get("parallel_sources").is_some() {
        return Err(Error::bad(
            "parallel_sources applies only to observer_notes",
        ));
    }
    if kind == "basic" {
        crate::query::Filter::parse(args)?;
    }
    // An answer job reads one project. Workspace scope is a
    // recall over several (`context`, MCP `recall_project`); an answer over
    // several projects would need their combined source state pinned, the
    // model's citations qualified by project, and grounding across them.
    if crate::workspace::Scope::parse(args)? == crate::workspace::Scope::Workspace
        || args.get("max_projects").is_some()
    {
        return Err(Error::bad(
            "scope workspace applies to context and recall_project; a workflow reads one project",
        ));
    }
    // Observer-note reading options apply to basic answers only; the notes
    // state is pinned here, never by the caller.
    if kind != "basic"
        && let Some(field) = ["observer_notes", "notes_tokens", "answer_contract"]
            .into_iter()
            .find(|f| args.get(*f).is_some())
    {
        return Err(Error::bad(format!(
            "{field} applies only to basic question answering"
        )));
    }
    if let Some(variants) = args.get("query_variants") {
        if !["basic", "recall"].contains(&kind) {
            return Err(Error::bad("query_variants apply only to basic and recall"));
        }
        graphicle_memory::retrieval::query_variants(variants).map_err(Error::from)?;
    }
    if args.get("note_keys").is_some() && !["basic", "recall"].contains(&kind) {
        return Err(Error::bad("note_keys applies only to basic and recall"));
    }
    let note_keys = crate::observer::KeyMode::parse(args.get("note_keys"))?.is_some();
    if args.get("auto").is_some() && kind != crate::observer::KIND {
        return Err(Error::bad("auto applies only to observer notes"));
    }
    if args.get("answer_contract").is_some_and(|v| {
        !v.as_str()
            .is_some_and(|c| crate::search::CONTRACTS.contains(&c))
    }) {
        return Err(Error::bad("Unknown answer_contract"));
    }
    if args.get("notes_state").is_some() {
        return Err(Error::bad("notes_state is pinned by the service"));
    }
    // grounded-v3 reads observer notes unless the caller turns them off,
    // and budgets them beside the evidence rather than inside it.
    let v3 = args["answer_contract"] == "grounded-v3";
    let reads_notes =
        args["observer_notes"] == true || (v3 && args.get("observer_notes").is_none());
    if args.get("notes_tokens").is_some() && !reads_notes {
        return Err(Error::bad("notes_tokens requires observer_notes"));
    }
    if kind == "basic" && note_keys && !reads_notes {
        return Err(Error::bad("note_keys requires observer_notes"));
    }
    integer(args, "notes_tokens", 0, if v3 { 64000 } else { 16000 })?;
    for field in [
        "memory_retrieval",
        "graph_retrieval",
        "doc_links",
        "observer_notes",
        "verify_answer",
    ] {
        if args.get(field).is_some_and(|v| !v.is_boolean()) {
            return Err(Error::bad(format!("{field} must be true or false")));
        }
    }
    for (field, choices) in [
        ("dispatch", ["local", "redis"]),
        ("vector_backend", ["local", "qdrant"]),
    ] {
        if args
            .get(field)
            .is_some_and(|value| !value.as_str().is_some_and(|v| choices.contains(&v)))
        {
            return Err(Error::bad(format!("Unknown {field}")));
        }
    }

    let selected_model = args["model"]
        .as_str()
        .or_else(|| {
            (kind == "recall")
                .then(|| args["local_embedding_profile"].as_str())
                .flatten()
        })
        .or(if ["embed", "recall"].contains(&kind) {
            config.embedding_model.as_deref()
        } else {
            None
        });
    if !source_task(kind) {
        config.profile(selected_model)?;
    }
    Ok(permission)
}

/// Queue a checked workflow for `p`: source state and authorization read
/// inside the caller's (SQL) transaction, the job itself created in the
/// store's graph.
pub fn queue(
    store: &Store,
    db: &Connection,
    p: &Principal,
    args: &Value,
    config: &model::Config,
) -> Result<Value> {
    let kind = string(args, "kind", 30)?;
    let bounds = crate::ingestion::Bounds::from_config(config);
    let note_keys = crate::observer::KeyMode::parse(args.get("note_keys"))?.is_some();
    let selected_model = args["model"]
        .as_str()
        .or_else(|| {
            (kind == "recall")
                .then(|| args["local_embedding_profile"].as_str())
                .flatten()
        })
        .or(if ["embed", "recall"].contains(&kind) {
            config.embedding_model.as_deref()
        } else {
            None
        });
    let repository = repo(db, p, string(args, "repository_id", 100)?)?;
    let identifier = s(&repository, "id");
    if let Some(reason) = unavailable(db, identifier, kind, args)? {
        return Err(Error::bad(reason));
    }
    if args.to_string().len() > 65536 {
        return Err(Error::bad("Workflow arguments exceed 64 KiB"));
    }
    let max_calls = integer(args, "max_calls", 4, 32)?.max(1);
    let max_input = integer(args, "max_input_tokens", 64000, 512000)?.max(512);
    let max_output = integer(args, "max_output_tokens", 8192, 64000)?.max(64);
    let max_attempts = integer(args, "max_attempts", 1, 3)?.max(1);
    let mut payload = args.clone();
    payload["max_calls"] = json!(max_calls);
    payload["max_input_tokens"] = json!(max_input);
    payload["max_output_tokens"] = json!(max_output);
    payload["runtime_revision"] = json!(runtime_revision());
    crate::workflow_retrieval::pin(&mut payload, config)?;
    let documents = memory::documents(db, &repository, p)?;
    payload["source_state"] = json!(memory::state(&documents));
    let graph = store.graph();
    payload["memory_state"] = json!(memory::reviewed_state_of(
        db,
        &graph,
        &repository,
        &documents
    )?);
    if !source_task(kind) {
        let (name, profile) = config.profile(selected_model)?;
        payload["model"] = json!(name);
        payload["model_key"] = json!(model::key(profile));
    }
    let v3 = kind == "basic" && args["answer_contract"] == "grounded-v3";
    if v3 && args.get("observer_notes").is_none() {
        payload["observer_notes"] = json!(true);
    }
    if (kind == "basic" && payload["observer_notes"] == true) || (kind == "recall" && note_keys) {
        payload["notes_state"] = json!(crate::observer::state(db, identifier, &documents)?);
    }
    if kind == "basic" {
        payload["answer_contract"] = json!(
            args["answer_contract"]
                .as_str()
                .unwrap_or(crate::search::DEFAULT_CONTRACT)
        );
    }
    // A source scan owns its cursor until completion or cancellation.
    // Idempotence alone is insufficient when two callers use different budgets.
    if source_task(kind) {
        let connector_id = string(args, "connector_id", 100)?;
        for active in graph.read(|db| {
            graph::rows(
                db,
                "MATCH (j:Job {repository_id: $repo, kind: $kind}) WHERE j.state IN ['queued','retry','running'] \
                 RETURN j.id AS id, j.state AS state, j.payload AS payload",
                graph::params(&json!({"repo": identifier, "kind": kind})),
            )
        })?
        .into_iter()
        .map(|mut active| {
            decode(&mut active, "payload").map(|_| active)
        })
        .collect::<Result<Vec<_>>>()?
        {
            if active["payload"]["connector_id"] == connector_id {
                return Ok(
                    json!({"id":active["id"],"state":active["state"],"kind":kind,"replayed":true}),
                );
            }
        }
    }
    if ["basic", "reflect"].contains(&kind)
        && args["semantic_retrieval"] != false
        && let Some(name) = args["embedding_model"]
            .as_str()
            .or(args["local_embedding_profile"].as_str())
            .or(config.embedding_model.as_deref())
    {
        let (_, profile) = config.profile(Some(name))?;
        payload["embedding_model"] = json!(name);
        payload["embedding_key"] = json!(model::key(profile));
    }
    let progress = match kind {
        "extract" => one(
            db,
            "SELECT count(*) AS count,max(created_at) AS latest FROM am_document_reads WHERE repository_id=?",
            [identifier],
        )?,
        "embed" => Some(json!({"count": crate::vector::graph_vector_count(&graph, identifier)?})),
        crate::observer::KIND => one(
            db,
            "SELECT count(*) AS count,max(created_at) AS latest FROM am_observer_notes WHERE repository_id=?",
            [identifier],
        )?,
        "connector" | "permissions" | "connector_probe" => one(
            db,
            "SELECT generation,cursor FROM am_connectors WHERE repository_id=? AND id=?",
            params![identifier, string(args, "connector_id", 100)?],
        )?,
        _ => None,
    };
    payload["progress"] = json!(progress);
    if kind == "connector_probe" && payload["retry_of"].is_null() {
        payload["requested_at"] = json!(now());
    }
    let input_hash = digest(&json!({"payload":payload,"principal":p.hash}));
    if let Some(mut old) = graph.read(|db| {
        graph::one(
            db,
            "MATCH (j:Job {repository_id: $repo, kind: $kind, input_hash: $hash}) \
             RETURN j.id AS id, j.state AS state, j.result AS result, j.error AS error",
            graph::params(&json!({"repo": identifier, "kind": kind, "hash": input_hash})),
        )
    })? {
        decode(&mut old, "result")?;
        old["replayed"] = json!(true);
        return Ok(old);
    }
    let active = graph.read(|db| {
        graph::one(
            db,
            "MATCH (j:Job {repository_id: $repo}) WHERE j.state IN ['queued','retry','running'] RETURN count(j) AS count",
            graph::params(&json!({"repo": identifier})),
        )
    })?
    .and_then(|row| row["count"].as_i64())
    .unwrap_or(0);
    if active >= 100 {
        return Err(Error::bad("Project queue is full"));
    }
    crate::ingestion::check_queue_bytes(
        &graph,
        identifier,
        payload.to_string().len(),
        bounds.max_queued_bytes_per_repository,
    )?;
    let identifier_job = graph.transaction(|db| {
        insert_job(
            db,
            identifier,
            &p.workspace,
            kind,
            &input_hash,
            &payload,
            &p.hash,
            0,
            max_attempts as i64,
        )
    })?;
    store::event(
        db,
        identifier,
        "workflow_queued",
        json!({"id":identifier_job,"kind":kind}),
    )?;
    Ok(
        json!({"id":identifier_job,"state":"queued","kind":kind,"limits":{"calls":max_calls,"input_tokens":max_input,"output_tokens":max_output},"replayed":false}),
    )
}

impl Store {
    pub fn enqueue(
        &self,
        workspace: &str,
        token: &str,
        args: &Value,
        config: &model::Config,
    ) -> Result<Value> {
        let config = crate::settings::configuration(self, workspace, config)?;
        // Automatic observation rounds are marked by the service, never a caller.
        if args.get("auto").is_some() {
            return Err(Error::bad("auto is set by the service"));
        }
        let permission = check(args, &config)?;
        self.authorized(workspace, token, permission, |db, p| {
            queue(self, db, p, args, &config)
        })
        .inspect(|job| {
            if job["replayed"] == false {
                self.wake.notify_one();
            }
        })
    }
    pub fn job_status(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized_read(workspace,token,"read",|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;
            let mut result=Vec::new();
            let graph = self.graph().clone();
            let jobs = graph.read(|gdb| {
                graph::rows(gdb,&format!("MATCH (j:Job {{repository_id: $repo}}) RETURN {JOB_FIELDS} ORDER BY j.created_at DESC LIMIT 100"),graph::params(&json!({"repo": s(&repository, "id")})))
            })?;
            // The caller's source view and reviewed memory, read once per call.
            let mut documents:Option<Vec<Value>>=None;
            let mut sources:Option<String>=None;
            let mut reviewed:Option<String>=None;
            for mut row in jobs {
                if p.role!="owner" && s(&row,"principal_hash")!=p.hash {continue;}
                if args["job_id"].as_str().is_some_and(|j|s(&row,"id")!=j) {continue;}
                decode(&mut row,"result")?;decode(&mut row,"payload")?;
                // Work this program cannot run waits for one that can.
                if ["queued","retry"].contains(&s(&row,"state")) && let Some(reason)=unavailable(db,s(&repository,"id"),s(&row,"kind"),&row["payload"])? {row["waiting_for"]=json!(reason);}
                // An unsupported kind names its unit, whatever its state.
                if let Some(archived)=crate::archived::kind(s(&row,"kind")) {row["archived"]=json!(archived.listing());}
                // Job answers can include private evidence. Recheck the caller's
                // entire source view even after the job has finished.
                if !source_task(s(&row,"kind")) {
                    if documents.is_none() {documents=Some(memory::documents(db,&repository,p)?);}
                    let view=documents.as_deref().unwrap_or_default();
                    if sources.is_none() {sources=Some(memory::state(view));}
                    if Some(s(&row["payload"],"source_state"))!=sources.as_deref() {
                        row["result"]=Value::Null;row["result_unavailable"]=json!("source_view_changed");
                    }
                    if row["payload"]["memory_state"].is_string() {
                        if reviewed.is_none() {reviewed=Some(memory::reviewed_state_of(db,&self.graph(),&repository,view)?);}
                        if Some(s(&row["payload"],"memory_state"))!=reviewed.as_deref() {
                            row["result"]=Value::Null;row["result_unavailable"]=json!("reviewed_memory_changed");
                        }
                    }
                }
                let calls=graph.read(|gdb| {
                    graph::rows(gdb,
                        "MATCH (c:ModelCall {job_id: $job}) OPTIONAL MATCH (r:Reservation {id: c.id}) \
                         RETURN c.id AS id, c.repository_id AS repository_id, c.job_id AS job_id, c.provider AS provider, c.model AS model, \
                                c.request_hash AS request_hash, c.input_tokens AS input_tokens, c.output_tokens AS output_tokens, \
                                c.elapsed_ms AS elapsed_ms, c.cached AS cached, c.created_at AS created_at, \
                                r.state AS call_state, r.usage_reported AS usage_reported, r.error AS error \
                         ORDER BY c.created_at",
                        graph::params(&json!({"job": s(&row, "id")})))
                })?;
                row["calls"]=json!(calls);row.as_object_mut().unwrap().remove("principal_hash");row.as_object_mut().unwrap().remove("payload");result.push(row);
            }
            if args["job_id"].is_string() {result.into_iter().next().ok_or_else(||Error::bad("Job unavailable"))}else{Ok(json!({"jobs":result}))}
        })
    }
    /// Explicitly retry a failed/cancelled job with a fresh bounded budget.
    /// The original attempt and its reported/unknown usage remain in history.
    pub fn retry_job(
        &self,
        workspace: &str,
        token: &str,
        args: &Value,
        config: &model::Config,
    ) -> Result<Value> {
        let retry = self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let old = self
                .graph()
                .read(|gdb| {
                    job_row(gdb, string(args, "job_id", 100)?)
                })?
                .filter(|old| s(old, "repository_id") == s(&repository, "id"))
                .ok_or_else(|| Error::bad("Job unavailable"))?;
            if p.role != "owner" && s(&old, "principal_hash") != p.hash {
                return Err(Error::denied());
            }
            if !p.permits(permission(s(&old, "kind"))?) {
                return Err(Error::denied());
            }
            if !["failed", "cancelled"].contains(&s(&old, "state")) {
                return Err(Error::conflict(
                    "Only failed or cancelled work can be retried",
                ));
            }
            let mut payload = old["payload"].clone();
            payload["retry_of"] = old["id"].clone();
            payload["kind"] = old["kind"].clone();
            payload["repository_id"] = repository["id"].clone();
            // The child has its own idempotence key. Repeated clicks return it.
            for field in [
                "requested_at",
                "progress",
                "runtime_revision",
                "source_state",
                "memory_state",
                "model_key",
            ] {
                payload.as_object_mut().unwrap().remove(field);
            }
            Ok(payload)
        })?;
        let mut result = self.enqueue(workspace, token, &retry, config)?;
        result["retry_of"] = retry["retry_of"].clone();
        result["new_budget_authorized"] = json!(true);
        Ok(result)
    }
    pub fn cancel_job(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "read", |db, p| {
            repo(db, p, string(args, "repository_id", 100)?)?;
            let job_id = string(args, "job_id", 100)?;
            let job = self
                .graph()
                .read(|gdb| job_row(gdb, job_id))?
                .filter(|job| s(job, "repository_id") == s(args, "repository_id"))
                .ok_or_else(|| Error::bad("Job unavailable"))?;
            if p.role != "owner" && s(&job, "principal_hash") != p.hash {
                return Err(Error::denied());
            }
            if ["queued", "retry", "running"].contains(&s(&job, "state")) {
                self.graph().transaction(|gdb| {
                    graph::rows(
                        gdb,
                        "MATCH (j:Job {id: $id}) WHERE j.state IN ['queued','retry','running'] \
                         SET j.state='cancelled', j.updated_at=$now, j.lease_token=NULL",
                        graph::params(&json!({"id": job_id, "now": now()})),
                    )?;
                    Ok(())
                })?;
            }
            Ok(json!({"id":job_id,"cancelled":true,"in_flight_provider_request_may_finish":true}))
        })
    }
    pub fn lease_job(&self) -> Result<Option<Job>> {
        self.lease_job_excluding(&[])
    }
    /// Kind of the job `lease_job` would offer next (priority order, honouring
    /// `not_before` and source/repository serialization), without leasing it.
    pub fn peek_ready_kind(&self) -> Result<Option<String>> {
        let graph = self.graph().clone();
        let stamp = now();
        // A crashed worker may leave the only job running. Recover it before
        // admission checks, or an otherwise idle queue never retries it.
        // Ordinary idle polls remain reads and do not take the write lock.
        if graph
            .read(|db| {
                graph::one(
                    db,
                    "MATCH (j:Job) WHERE j.state='running' AND j.lease_until < $now RETURN j.id AS id LIMIT 1",
                    graph::params(&json!({"now": stamp})),
                )
            })?
            .is_some()
        {
            graph.transaction(|db| expire_leases(db, &stamp))?;
        }
        Ok(graph
            .read(|db| graph::one(db, &READY_JOB_QUERY, graph::params(&json!({"now": stamp}))))?
            .map(|row| s(&row, "kind").to_string()))
    }
    /// Lease the next ready job whose kind is not in `excluded` (G08 admission).
    /// Running work blocks other jobs in the same repository. Only explicitly
    /// opted-in observer jobs with disjoint source prefixes may overlap.
    pub fn lease_job_excluding(&self, excluded: &[&str]) -> Result<Option<Job>> {
        self.lease_job_filtered(excluded, None)
    }
    /// Lease the next ready job of exactly `kind` for one `repository`, leaving
    /// every other queued job untouched (`graphicle pr` runs its own
    /// projections synchronously in a store that may hold other work).
    pub fn lease_job_of_kind(&self, repository: &str, kind: &str) -> Result<Option<Job>> {
        self.lease_job_filtered(&[], Some((repository, kind)))
    }
    fn lease_job_filtered(
        &self,
        excluded: &[&str],
        only: Option<(&str, &str)>,
    ) -> Result<Option<Job>> {
        // Work only an extension runs stays queued while none is installed.
        let provider_sync = crate::connectors::provider_permission_sync();
        let mut excluded: Vec<&str> = excluded.to_vec();
        if !provider_sync {
            excluded.push(PERMISSIONS);
        }
        let graph = self.graph().clone();
        let stamp = now();
        graph.transaction(|db| expire_leases(db, &stamp))?;
        // Apply admission/repository filters before the candidate cap so
        // unrelated ready jobs cannot hide all eligible work.
        let candidates = graph.read(|db| {
            graph::rows(
                db,
                &LEASE_JOB_QUERY,
                graph::params(&json!({
                    "now": stamp,
                    "excluded": excluded,
                    "known": known_kinds(),
                    "repo": only.map(|v| v.0),
                    "kind": only.map(|v| v.1),
                })),
            )
        })?;
        for mut row in candidates {
            // Known kinds only; a kind unknown here stays queued.
            let Ok(permission) = permission(s(&row, "kind")) else {
                continue;
            };
            decode(&mut row, "payload")?;
            // The connector table is still SQL: a provider-permission
            // connector sync waits for a program that can run it.
            if !provider_sync
                && s(&row, "kind") == "connector"
                && self.read(|db| {
                    Ok(one(
                        db,
                        "SELECT 1 AS provider FROM am_connectors WHERE repository_id=? AND id=? AND json_extract(config,'$.permission_mode')='provider'",
                        params![s(&row, "repository_id"), s(&row["payload"], "connector_id")],
                    )?
                    .is_some())
                })?
            {
                continue;
            }
            let authorized = self
                .read(|db| {
                    authorize_hash(db, s(&row, "workspace_id"), s(&row, "principal_hash"), permission)
                        .map(|_| ())
                })
                .is_ok();
            if !authorized {
                graph.transaction(|db| {
                    graph::rows(
                        db,
                        "MATCH (j:Job {id: $id}) WHERE j.state IN ['queued','retry'] \
                         SET j.state='cancelled', j.error='Credential unavailable', j.updated_at=$now",
                        graph::params(&json!({"id": s(&row, "id"), "now": stamp})),
                    )?;
                    Ok(())
                })?;
                continue;
            }
            let lease = id();
            let until = (chrono::Utc::now() + chrono::Duration::seconds(240))
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
            // Single-statement CAS: the lease is taken only if the job is
            // still ready and unblocked at write time.
            let taken = graph.transaction(|db| {
                graph::one(
                    db,
                    &LEASE_CAS,
                    graph::params(&json!({"id": s(&row, "id"), "now": stamp, "lease": lease, "until": until})),
                )
            })?;
            if taken.is_some() {
                return Ok(Some(Job {
                    id: s(&row, "id").into(),
                    repository: s(&row, "repository_id").into(),
                    workspace: s(&row, "workspace_id").into(),
                    principal_hash: s(&row, "principal_hash").into(),
                    lease,
                    kind: s(&row, "kind").into(),
                    payload: row["payload"].clone(),
                }));
            }
        }
        Ok(None)
    }
    pub fn finish_job(&self, job: &Job, result: Result<Value>) -> Result<()> {
        let outcome = self.graph().transaction(|db| {
            let Some(current) = job_row(db, &job.id)? else {
                return Ok(None);
            };
            if s(&current, "lease_token") != job.lease || current["state"] != "running" {
                return Ok(None);
            }
            let (state, value, error, refund) = match &result {
                Ok(value) => ("complete", Some(value.to_string()), None, false),
                Err(error) => {
                    graph::rows(
                        db,
                        "MATCH (c:ModelCall {job_id: $job}) WITH collect(c.request_hash) AS hashes \
                         MATCH (m:ModelCache {repository_id: $repo}) WHERE m.request_hash IN hashes DELETE m",
                        graph::params(&json!({"job": job.id, "repo": job.repository})),
                    )?;
                    // The store was busy (another process held the write
                    // lock): the work itself did not fail, so the job goes
                    // straight back to the queue, its attempt refunded —
                    // before the graph queue, such a job was never leased at
                    // all (leasing waited on the lock).
                    if busy(error) {
                        ("queued", None, Some(error.message.clone()), true)
                    } else {
                        let retry = current["attempt"].as_u64() < current["max_attempts"].as_u64()
                            && error.status != axum::http::StatusCode::CONFLICT
                            && error.status != axum::http::StatusCode::FORBIDDEN;
                        (if retry { "retry" } else { "failed" }, None, Some(error.message.clone()), false)
                    }
                }
            };
            // Exponential backoff per attempt (G08): 30 s, 60 s, 120 s, ... capped;
            // a refunded busy attempt runs again at once.
            let later = if refund {
                now()
            } else {
                crate::ingestion::backoff_stamp(current["attempt"].as_u64().unwrap_or(1))
            };
            graph::rows(
                db,
                "MATCH (j:Job {id: $id}) SET j.state=$state, j.result=$result, j.error=$error, \
                 j.not_before=$later, j.lease_token=NULL, j.lease_until=NULL, j.updated_at=$now, \
                 j.attempt = CASE WHEN $refund AND j.attempt > 0 THEN j.attempt - 1 ELSE j.attempt END",
                graph::params(&json!({"id": job.id, "state": state, "result": value, "error": error, "later": later, "now": now(), "refund": refund})),
            )?;
            Ok(Some(state))
        })?;
        let Some(state) = outcome else {
            return Ok(());
        };
        self.transaction(|db| {
            store::event(db, &job.repository, "workflow_finished", json!({"id": job.id, "state": state}))
        })
    }
}
/// Renew `job`'s lease: only while it is still `running` under this lease, so
/// a job that was cancelled or taken over is left alone.
pub fn renew(graph: &GraphStore, job: &Job) -> Result<()> {
    graph.transaction(|db| renew_in(db, job))
}

/// [`renew`] inside an open graph transaction.
pub(crate) fn renew_in(db: &mut GConnection, job: &Job) -> Result<()> {
    let until = (chrono::Utc::now() + chrono::Duration::seconds(240))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    graph::rows(
        db,
        "MATCH (j:Job {id: $id}) WHERE j.lease_token=$lease AND j.state='running' \
         SET j.lease_until=$until, j.updated_at=$now",
        graph::params(&json!({"id": job.id, "lease": job.lease, "until": until, "now": now()})),
    )?;
    Ok(())
}

/// Call reservations so far for `job`: `{calls, input, output}`.
pub(crate) fn reservation_totals_in(db: &mut GConnection, job_id: &str) -> Result<Value> {
    Ok(graph::one(
        db,
        "MATCH (r:Reservation {job_id: $job}) \
         RETURN count(r) AS calls, coalesce(sum(r.input_reserved),0) AS input, coalesce(sum(r.output_reserved),0) AS output",
        graph::params(&json!({"job": job_id})),
    )?
    .unwrap_or(json!({"calls": 0, "input": 0, "output": 0})))
}

pub(crate) fn reservation_totals(graph: &GraphStore, job_id: &str) -> Result<Value> {
    graph.read(|db| reservation_totals_in(db, job_id))
}

/// Record a model call (`am_model_calls`), linked to its job.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_call(
    db: &mut GConnection,
    call_id: &str,
    repository: &str,
    job_id: &str,
    profile: &model::Profile,
    request_hash: &str,
    input_tokens: usize,
    output_tokens: usize,
    elapsed_ms: u64,
    cached: i64,
) -> Result<()> {
    graph::rows(
        db,
        "MATCH (j:Job {id: $job}) \
         CREATE (c:ModelCall {id: $id, repository_id: $repo, job_id: $job, provider: $provider, model: $model, \
            request_hash: $hash, input_tokens: $input, output_tokens: $output, elapsed_ms: $elapsed, cached: $cached, \
            created_at: $now})-[:FOR]->(j)",
        graph::params(&json!({
            "id": call_id, "repo": repository, "job": job_id, "provider": profile.protocol,
            "model": profile.model, "hash": request_hash, "input": input_tokens,
            "output": output_tokens, "elapsed": elapsed_ms, "cached": cached, "now": now(),
        })),
    )?;
    Ok(())
}
#[derive(Clone)]
pub struct Worker {
    pub store: Store,
    pub config: Arc<model::Config>,
    pub client: reqwest::Client,
}
impl Worker {
    pub fn new(store: Store, config: model::Config) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            store,
            config: Arc::new(config),
            client: model::client()?,
        })
    }
    pub async fn once(&self) -> Result<bool> {
        let maintenance = crate::model::client_with_timeout(std::time::Duration::from_secs(30))?;
        let cleaned = crate::projection::once(&self.store, &maintenance).await?;
        // G08 admission: lease only work whose CPU/model permit is in hand, so
        // a job never spends its lease waiting behind another worker.
        let bounds = crate::ingestion::Bounds::from_config(&self.config);
        let limiter = crate::ingestion::Limiter::global(&bounds);
        let (mut admission, excluded) = limiter.try_admit();
        let Some(next) = self.store.peek_ready_kind()? else {
            return Ok(cleaned);
        };
        // Priority order is preserved: when the head job needs a permit another
        // worker holds, wait for admission rather than skipping past it.
        let leased = if excluded.contains(&next.as_str()) {
            drop(admission);
            admission = limiter.admit().await;
            self.store.lease_job()?
        } else {
            self.store.lease_job_excluding(&excluded)?
        };
        let Some(job) = leased else {
            return Ok(cleaned);
        };
        admission.retain_for(&job.kind);
        let result = crate::workflows::execute(self, &job).await;
        drop(admission);
        self.store.finish_job(&job, result)?;
        // A finished automatic round or source sync may call for the next
        // observation round now rather than at the next scheduler poll.
        let connector =
            job.payload["auto"]["connector_id"]
                .as_str()
                .or(job.payload["connector_id"]
                    .as_str()
                    .filter(|_| job.kind == "connector"));
        if let Some(connector) = connector {
            self.store.auto_observe(&self.config, Some(connector)).ok();
        }
        Ok(true)
    }
    /// Whether `infer` would accept one more uncached call of this size, so a
    /// batch workflow can stop cleanly instead of failing on its budget.
    pub fn fits(&self, job: &Job, input_tokens: usize) -> Result<bool> {
        guarded_read(&self.store, job, |_, _, _| {
            let totals = reservation_totals(&self.store.graph(), &job.id)?;
            let used = |key: &str| totals[key].as_u64().unwrap_or(0);
            Ok(
                used("calls") < job.payload["max_calls"].as_u64().unwrap_or(4)
                    && used("input") + input_tokens as u64
                        <= job.payload["max_input_tokens"].as_u64().unwrap_or(64000)
                    && job.payload["max_output_tokens"]
                        .as_u64()
                        .unwrap_or(8192)
                        .saturating_sub(used("output"))
                        >= 64,
            )
        })
    }
    pub async fn infer(
        &self,
        job: &Job,
        instructions: &str,
        input: &Value,
        schema: &Value,
    ) -> Result<Value> {
        let config = crate::settings::configuration(&self.store, &job.workspace, &self.config)?;
        let (_, profile) = config.profile(job.payload["model"].as_str())?;
        if s(&job.payload, "model_key") != model::key(profile) {
            return Err(Error::conflict("Model configuration changed after queuing"));
        }
        let credential = profile
            .key_env
            .as_deref()
            .map(|reference| self.store.resolve_credential(&job.workspace, reference))
            .transpose()?;
        let request_hash = digest(
            &json!({"model":model::key(profile),"instructions":instructions,"input":input,"schema":schema,"scope":job.repository,"source_state":job.payload["source_state"]}),
        );
        let reservation = id();
        let input_tokens = model::request_tokens(instructions, input, schema);
        let output_limit = std::cell::Cell::new(profile.output_limit);
        let graph = self.store.graph().clone();
        let cached = guarded(&self.store, job, |_, _, _| {
            graph.transaction(|db| {
                renew_in(db, job)?;
                if let Some(row) = graph::one(
                    db,
                    "MATCH (c:ModelCache {repository_id: $repo, request_hash: $hash, source_state: $source}) RETURN c.result AS result",
                    graph::params(&json!({
                        "repo": job.repository,
                        "hash": request_hash,
                        "source": s(&job.payload, "source_state"),
                    })),
                )? {
                    record_call(
                        db, &reservation, &job.repository, &job.id, profile, &request_hash,
                        0, 0, 0, 1,
                    )?;
                    return Ok(Some(
                        serde_json::from_str::<model::Reply>(s(&row, "result"))
                            .map_err(|_| Error::bad("Invalid model cache"))?,
                    ));
                }
                let totals = reservation_totals_in(db, &job.id)?;
                let remaining = (job.payload["max_output_tokens"].as_u64().unwrap_or(8192) as usize)
                    .saturating_sub(totals["output"].as_u64().unwrap_or(0) as usize);
                output_limit.set(output_limit.get().min(remaining));
                if totals["calls"].as_u64().unwrap_or(0)
                    >= job.payload["max_calls"].as_u64().unwrap_or(4)
                    || totals["input"].as_u64().unwrap_or(0) + input_tokens as u64
                        > job.payload["max_input_tokens"].as_u64().unwrap_or(64000)
                    || output_limit.get() < 64
                {
                    return Err(Error::bad(
                        "Workflow inference budget exhausted; completed work is retained",
                    ));
                }
                graph::rows(
                    db,
                    "MATCH (j:Job {id: $job}) CREATE (r:Reservation {id: $id, job_id: $job, request_hash: $hash, state: 'started', input_reserved: $input, output_reserved: $output, created_at: $now})-[:FOR]->(j)",
                    graph::params(&json!({"id": reservation, "job": job.id, "hash": request_hash, "input": input_tokens, "output": output_limit, "now": now()})),
                )?;
                record_call(
                    db, &reservation, &job.repository, &job.id, profile, &request_hash,
                    input_tokens, output_limit.get(), 0, 0,
                )?;
                Ok(None)
            })
        })?;
        if let Some(reply) = cached {
            let value = reply.json_for(schema)?;
            validate_output(schema, &value)?;
            return Ok(value);
        }
        let reply = if job.payload["dispatch"] == "redis" {
            crate::distributed::generate(
                self,
                job,
                profile,
                instructions,
                input,
                schema,
                output_limit.get(),
            )
            .await
        } else {
            model::generate_with_credential(
                &self.client,
                profile,
                instructions,
                input,
                schema,
                output_limit.get(),
                credential.as_deref(),
            )
            .await
        };
        // Record even a failed/unknown provider call independently of job access.
        graph.transaction(|db| {
            match &reply {
                Ok(reply)=>{
                    graph::rows(db,"MATCH (r:Reservation {id: $id}) SET r.state='finished', r.input_reserved=$input, r.output_reserved=$output, r.usage_reported=$reported, r.finished_at=$now",
                        graph::params(&json!({"id": reservation, "input": reply.input_tokens, "output": reply.output_tokens, "reported": reply.usage_reported as i64, "now": now()})))?;
                    graph::rows(db,"MATCH (c:ModelCall {id: $id}) SET c.input_tokens=$input, c.output_tokens=$output, c.elapsed_ms=$elapsed, c.model=$model",
                        graph::params(&json!({"id": reservation, "input": reply.input_tokens, "output": reply.output_tokens, "elapsed": reply.elapsed_ms, "model": reply.model})))?;
                },Err(error)=>{
                    let reported = error.provider_failure.as_ref().and_then(|failure|
                        failure.input_tokens.zip(failure.output_tokens));
                    if let Some((input, output)) = reported {
                        graph::rows(db,"MATCH (r:Reservation {id: $id}) SET r.state='failed', r.input_reserved=$input, r.output_reserved=$output, r.usage_reported=1, r.error=$error, r.finished_at=$now",
                            graph::params(&json!({"id": reservation, "input": input, "output": output, "error": error.message, "now": now()})))?;
                        graph::rows(db,"MATCH (c:ModelCall {id: $id}) SET c.input_tokens=$input, c.output_tokens=$output",
                            graph::params(&json!({"id": reservation, "input": input, "output": output})))?;
                    } else {
                        graph::rows(db,"MATCH (r:Reservation {id: $id}) SET r.state='unknown_usage', r.error=$error, r.finished_at=$now",
                            graph::params(&json!({"id": reservation, "error": error.message, "now": now()})))?;
                    }
                    if let Some(failure) = &error.provider_failure {
                        graph::rows(db,"MATCH (c:ModelCall {id: $id}) SET c.elapsed_ms=$elapsed, c.model=$model",
                            graph::params(&json!({"id": reservation, "elapsed": failure.elapsed_ms, "model": failure.model})))?;
                    }
                }
            }
            Ok(())
        })?;
        let reply = reply?;
        let raw = reply.json()?;
        let value = reply.json_for(schema)?;
        validate_output(schema, &value)?;
        guarded(&self.store, job, |db, _, _| {
            if value != raw {
                store::event(
                    db,
                    &job.repository,
                    "model_array_envelope_restored",
                    json!({"job_id":job.id,"request_hash":request_hash,"semantic_fields_changed":false}),
                )?;
            }
            graph.transaction(|db| {
                graph::rows(
                    db,
                    "MERGE (c:ModelCache {repository_id: $repo, request_hash: $hash}) \
                     SET c.source_state=$source, c.result=$result, c.created_at=$now",
                    graph::params(&json!({
                        "repo": job.repository,
                        "hash": request_hash,
                        "source": s(&job.payload, "source_state"),
                        "result": serde_json::to_string(&reply)?,
                        "now": now(),
                    })),
                )?;
                graph::rows(
                    db,
                    "MATCH (c:ModelCache {repository_id: $repo}) WITH c ORDER BY c.created_at DESC SKIP 500 DELETE c",
                    graph::params(&json!({"repo": job.repository})),
                )?;
                Ok(())
            })?;
            // A cached answer is only as current as the principal that produced it.
            crate::invalidation::record_dependency(
                &self.store.graph(),
                &job.workspace,
                (
                    crate::invalidation::ANSWER,
                    &format!("{}:{}", job.repository, request_hash),
                ),
                (crate::invalidation::PRINCIPAL, &job.principal_hash),
            )?;
            Ok(())
        })?;
        Ok(value)
    }
    pub async fn embeddings(
        &self,
        job: &Job,
        profile: &model::Profile,
        texts: &[String],
    ) -> Result<Vec<Vec<f64>>> {
        self.embeddings_for(job, profile, texts, false).await
    }
    /// Embed retrieval questions; local models apply their manifest query prefix.
    pub async fn query_embeddings(
        &self,
        job: &Job,
        profile: &model::Profile,
        texts: &[String],
    ) -> Result<Vec<Vec<f64>>> {
        self.embeddings_for(job, profile, texts, true).await
    }
    async fn embeddings_for(
        &self,
        job: &Job,
        profile: &model::Profile,
        texts: &[String],
        query: bool,
    ) -> Result<Vec<Vec<f64>>> {
        let input = texts
            .iter()
            .map(|t| graphicle_memory::text::count(t))
            .sum::<usize>();
        let credential = profile
            .key_env
            .as_deref()
            .map(|reference| self.store.resolve_credential(&job.workspace, reference))
            .transpose()?;
        let call_id = id();
        let mut request = json!({"model":model::key(profile),"input":texts,"scope":job.repository});
        if query {
            request["role"] = json!("query");
        }
        let request_hash = digest(&request);
        let graph = self.store.graph().clone();
        guarded(&self.store, job, |_, _, _| {
            graph.transaction(|db| {
                renew_in(db, job)?;
                let totals = reservation_totals_in(db, &job.id)?;
                if totals["calls"].as_u64().unwrap_or(0)
                    >= job.payload["max_calls"].as_u64().unwrap_or(4)
                    || totals["input"].as_u64().unwrap_or(0) + input as u64
                        > job.payload["max_input_tokens"].as_u64().unwrap_or(64000)
                {
                    return Err(Error::bad("Embedding budget exhausted"));
                }
                graph::rows(
                    db,
                    "MATCH (j:Job {id: $job}) CREATE (r:Reservation {id: $id, job_id: $job, request_hash: $hash, state: 'started', input_reserved: $input, output_reserved: 0, created_at: $now})-[:FOR]->(j)",
                    graph::params(&json!({"id": call_id, "job": job.id, "hash": request_hash, "input": input, "now": now()})),
                )?;
                record_call(db, &call_id, &job.repository, &job.id, profile, &request_hash, input, 0, 0, 0)?;
                Ok(())
            })
        })?;
        let start = std::time::Instant::now();
        let result =
            model::embed_role(&self.client, profile, texts, credential.as_deref(), query).await;
        graph.transaction(|db| {
            match &result {Ok((_,tokens,reported))=>{
                graph::rows(db,"MATCH (r:Reservation {id: $id}) SET r.state='finished', r.input_reserved=$input, r.usage_reported=$reported, r.finished_at=$now",
                    graph::params(&json!({"id": call_id, "input": tokens, "reported": i64::from(*reported), "now": now()})))?;
                graph::rows(db,"MATCH (c:ModelCall {id: $id}) SET c.input_tokens=$input, c.elapsed_ms=$elapsed",
                    graph::params(&json!({"id": call_id, "input": tokens, "elapsed": start.elapsed().as_millis() as u64})))?;
            },Err(error)=>{
                graph::rows(db,"MATCH (r:Reservation {id: $id}) SET r.state='unknown_usage', r.error=$error, r.finished_at=$now",
                    graph::params(&json!({"id": call_id, "error": error.message, "now": now()})))?;
            }}
            Ok(())
        })?;
        let (vectors, _, _) = result?;
        guarded_read(&self.store, job, |_, _, _| Ok(()))?;
        Ok(vectors)
    }
}
/// How a reply that fails its output schema is reported (see `validate_output`).
pub const CONTRACT_MISMATCH: &str = "Model output does not match the workflow contract";
fn validate_output(schema: &Value, value: &Value) -> Result<()> {
    let validator = jsonschema::validator_for(&model::strict_schema(schema))
        .map_err(|_| Error::bad("Invalid workflow output schema"))?;
    if let Some(error) = validator.iter_errors(value).next() {
        return Err(Error::bad(format!(
            "{CONTRACT_MISMATCH} at {} (schema {}); no memory was committed",
            error.instance_path, error.schema_path
        )));
    }
    Ok(())
}
