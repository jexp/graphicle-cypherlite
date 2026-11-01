//! Automatic observation: Mastra Observational Memory's observation threshold,
//! opt-in per source connector.
//!
//! Mastra OM observes a thread once its unobserved messages reach
//! `messageTokens` (30,000), off the answering path (mastra-ai/mastra@381c48db,
//! `packages/memory/src/processors/observational-memory/constants.ts` and
//! `observational-memory.ts`, Apache-2.0). A connector's `observe` policy
//! applies the same rule to the connector's sources. Once their unobserved
//! tokens reach `threshold_tokens`, the service queues an `observer_notes`
//! round; a round that stops on its budget queues the next. (A reflection
//! round over the notes, `reflect`, is not supported: unit
//! `observer-reflection`.)
//! Sources stay searchable while unobserved, so the threshold only batches
//! model calls. Each round keeps the policy's call and token limits, at most
//! `max_rounds` rounds run per connector generation (each import or sync),
//! and a failed round stops observation until the next generation.
//! Rounds run with the workspace owner's credential, like scheduled syncs.
//!
//! Mastra later fixed activation leaving the newest, never-buffered messages
//! unobserved above the threshold (mastra-ai/mastra@056427cda): it now keeps
//! activating buffered chunks and then observes the remaining tail. Rounds
//! here buffer nothing: each one observes every unobserved source, and the
//! decision after it re-counts all of them, so a tail that keeps the total at
//! or above the threshold queues the next round at once.
use crate::{
    Error, Result, integer, jobs, memory, model, observer,
    store::{Principal, Store, authorize_hash, decode, repo, s},
    string,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Mastra OM `messageTokens`: unobserved source tokens that start a round.
pub const OBSERVE_TOKENS: usize = 30_000;
/// Rounds per connector generation.
pub const ROUNDS: usize = 8;
const MAX_THRESHOLD: usize = 10_000_000;
const FIELDS: [&str; 8] = [
    "model",
    "profile",
    "max_notes",
    "threshold_tokens",
    "max_rounds",
    "max_calls",
    "max_input_tokens",
    "max_output_tokens",
];

/// Validate a connector `observe` policy. An unsupported field at an inert
/// value (and what only it read) is left out (see [`crate::archived`]).
pub fn validate(policy: &Value) -> Result<()> {
    crate::archived::check(crate::archived::Surface::ObservePolicy, policy)?;
    let policy = &crate::archived::without_inert(crate::archived::Surface::ObservePolicy, policy);
    let fields = policy
        .as_object()
        .ok_or_else(|| Error::bad("observe must be an object"))?;
    if let Some(field) = fields.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(Error::bad(format!("Unknown observe policy field {field}")));
    }
    if policy.get("model").is_some() {
        string(policy, "model", 200)?;
    }
    if integer(policy, "threshold_tokens", OBSERVE_TOKENS, MAX_THRESHOLD)? == 0
        || integer(policy, "max_rounds", ROUNDS, 64)? == 0
    {
        return Err(Error::bad(
            "observe thresholds and max_rounds must be positive",
        ));
    }
    integer(policy, "max_calls", 4, 32)?;
    integer(policy, "max_input_tokens", 64000, 512000)?;
    integer(policy, "max_output_tokens", 8192, 64000)?;
    observer::validate_args(&observe_args(policy, "", "connector:policy:"))?;
    Ok(())
}

fn observe_args(policy: &Value, repository: &str, prefix: &str) -> Value {
    let mut args = json!({"kind":observer::KIND,"repository_id":repository,"include":[prefix]});
    for field in [
        "model",
        "profile",
        "max_notes",
        "max_calls",
        "max_input_tokens",
        "max_output_tokens",
    ] {
        if let Some(value) = policy.get(field) {
            args[field] = value.clone();
        }
    }
    args
}

impl Store {
    /// Queue the observation rounds that connector policies call for, for
    /// every connector or only `connector`. Returns one decision per
    /// connector with a policy.
    pub fn auto_observe(
        &self,
        config: &model::Config,
        connector: Option<&str>,
    ) -> Result<Vec<Value>> {
        // Connector rows live in the graph store; the workspace side of the
        // join stays in SQL until the ownership spine migrates.
        let mut connectors = self.graph().read(|db| {
            crate::store_graph::rows(
                db,
                "MATCH (c:Connector {state: 'active'}) RETURN c.id AS id, c.repository_id AS repository_id, c.config AS config, c.generation AS generation ORDER BY c.id",
                crate::store_graph::Params::new(),
            )
        })?;
        connectors.retain(|c| {
            (connector.is_none() || Some(s(c, "id")) == connector)
                && serde_json::from_str::<Value>(s(c, "config"))
                    .map(|config| config["observe"].is_object())
                    .unwrap_or(false)
        });
        let mut decisions = Vec::new();
        for mut connector in connectors {
            decode(&mut connector, "config")?;
            let ids = self
                .connect()?
                .query_row(
                    "SELECT p.workspace_id, w.capability_hash FROM repo_projects p JOIN workspaces w ON w.id=p.workspace_id WHERE p.id=?",
                    [s(&connector, "repository_id")],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                );
            match ids {
                Ok((workspace, hash)) => {
                    connector["workspace_id"] = Value::String(workspace);
                    connector["capability_hash"] = Value::String(hash);
                }
                Err(_) => continue,
            }
            let decision = crate::settings::configuration(
                self,
                s(&connector, "workspace_id"),
                config,
            )
            .and_then(|config| self.transaction(|db| decide(self, db, &connector, &config)))
            .unwrap_or_else(
                |e| json!({"connector_id":connector["id"],"queued":false,"reason":e.message}),
            );
            if decision["queued"] == true {
                self.wake.notify_one();
            }
            decisions.push(decision);
        }
        Ok(decisions)
    }
}

/// Whether the latest round stopped before finishing its work.
fn unfinished(rounds: &[&Value]) -> bool {
    rounds
        .last()
        .is_some_and(|r| r["state"] == "complete" && r["result"]["complete"] == false)
}

fn decide(
    store: &Store,
    db: &Connection,
    connector: &Value,
    config: &model::Config,
) -> Result<Value> {
    let id = s(connector, "id");
    let repository_id = s(connector, "repository_id");
    let generation = connector["generation"].as_i64().unwrap_or(0);
    let policy = &connector["config"]["observe"];
    let mut decision = json!({"connector_id":id,"generation":generation,"queued":false});
    // A stored policy that turns an unsupported option on stops here, and says why.
    if let Err(error) = crate::archived::check(crate::archived::Surface::ObservePolicy, policy) {
        decision["reason"] = json!(error.message);
        return Ok(decision);
    }
    let Ok(p) = authorize_hash(
        db,
        s(connector, "workspace_id"),
        s(connector, "capability_hash"),
        "propose",
    ) else {
        decision["reason"] = json!("owner credential unavailable");
        return Ok(decision);
    };
    // One automatic round at a time per connector, across generations. A
    // reflection round queued before the archive stays queued (no edition
    // runs it) and is not waited for.
    let graph = store.graph().clone();
    let active = graph
        .read(|g| {
            crate::store_graph::one(
                g,
                "MATCH (j:Job {repository_id: $repo, kind: $kind, auto_connector: $cid})                  WHERE j.state IN ['queued','retry','running'] RETURN count(j) AS count",
                crate::store_graph::params(&json!({"repo": repository_id, "kind": observer::KIND, "cid": id})),
            )
        })?
        .and_then(|row| row["count"].as_i64())
        .unwrap_or(0);
    if active > 0 {
        decision["reason"] = json!("round in progress");
        return Ok(decision);
    }
    let mut rounds = graph.read(|g| {
        crate::store_graph::rows(
            g,
            "MATCH (j:Job {repository_id: $repo, kind: $kind, auto_connector: $cid, auto_generation: $gen})              RETURN j.kind AS kind, j.state AS state, j.result AS result ORDER BY j.created_at, j.id",
            crate::store_graph::params(&json!({"repo": repository_id, "kind": observer::KIND, "cid": id, "gen": generation})),
        )
    })?;
    for round in &mut rounds {
        decode(round, "result")?;
    }
    if rounds
        .iter()
        .any(|r| ["failed", "cancelled"].contains(&s(r, "state")))
    {
        decision["reason"] = json!("a round failed; waiting for the next import or sync");
        return Ok(decision);
    }
    let max_rounds = integer(policy, "max_rounds", ROUNDS, 64)?;
    let repository = repo(db, &p, repository_id)?;
    let documents = memory::documents(db, &repository, &p)?;
    let prefix = format!("connector:{id}:");
    let args = observe_args(policy, repository_id, &prefix);
    let (_, profile) = config.profile(args["model"].as_str())?;
    let recipe = observer::Recipe::new(&args, profile)?;
    let observed: BTreeMap<String, String> = graph
        .read(|g| {
            crate::store_graph::rows(
                g,
                "MATCH (n:Observation {repository_id: $repo}) RETURN n.source_id AS source_id, n.step_key AS step_key",
                crate::store_graph::params(&json!({"repo": repository_id})),
            )
        })?
        .iter()
        .map(|r| (s(r, "source_id").to_string(), s(r, "step_key").to_string()))
        .collect();
    let unobserved: usize = documents
        .iter()
        .filter(|d| s(d, "id").starts_with(&prefix))
        .filter(|d| {
            observed
                .get(s(d, "id"))
                .is_none_or(|key| *key != recipe.key(d))
        })
        .map(|d| graphicle_memory::text::count(s(d, "content")))
        .sum();
    decision["unobserved_tokens"] = json!(unobserved);
    let observing: Vec<&Value> = rounds
        .iter()
        .filter(|r| r["kind"] == observer::KIND)
        .collect();
    let threshold = integer(policy, "threshold_tokens", OBSERVE_TOKENS, MAX_THRESHOLD)?;
    let mut idle = if unobserved > 0 {
        "below threshold"
    } else {
        "observed"
    };
    if unobserved > 0 && (unobserved >= threshold || unfinished(&observing)) {
        if observing.len() < max_rounds {
            return enqueue(store, db, &p, args, config, decision, generation);
        }
        idle = "observation rounds exhausted for this generation";
    }
    decision["reason"] = json!(idle);
    Ok(decision)
}

fn enqueue(
    store: &Store,
    db: &Connection,
    p: &Principal,
    mut args: Value,
    config: &model::Config,
    mut decision: Value,
    generation: i64,
) -> Result<Value> {
    jobs::check(&args, config)?;
    args["auto"] = json!({"connector_id":decision["connector_id"],"generation":generation});
    let job = jobs::queue(store, db, p, &args, config)?;
    decision["kind"] = args["kind"].clone();
    decision["job_id"] = job["id"].clone();
    if job["replayed"] == true {
        // Nothing changed since an identical round, which already ran.
        decision["reason"] = json!("unchanged since the last round");
    } else {
        decision["queued"] = json!(true);
    }
    Ok(decision)
}
