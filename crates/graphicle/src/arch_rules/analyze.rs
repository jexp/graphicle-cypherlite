//! `graphicle analyze`: ensure the current episode's enrich job, then report
//! its concept applications and violations (architecture-concepts.md §2).
//!
//! The report reads the enrich job's result and the episode's `:Violation`
//! rows back from the store. It never runs a rule itself: the job does, and
//! the job is the only writer. A job still queued or running is reported as
//! `waiting`, not failed. The pure parts (the exit decision, the report) live
//! in [`super::report`].
use super::plan::EnrichReport;
use super::report::{self, Violation};
use super::{Severity, rule_set_hash};
use crate::Result;
use crate::jobs::{enqueue_enrich, enrich_input_hash};
use crate::local::{self, Session};
use crate::store::Store;
use crate::store_graph::{self, GraphStore};
use serde_json::{Value, json};

/// The arguments of one analysis (the command line's, after parsing).
pub struct Request<'a> {
    pub group: Option<&'a str>,
    pub min: Option<Severity>,
}

/// Analyse `repository` (a project id) for its current episode, and return
/// the report (see [`super::report`]). `root` is the project folder whose rule
/// documents are checked first; their errors come before any run. The
/// episode's graph is built and its queued jobs drained first (as `sync`
/// does), so a job that can run here is run here.
pub fn run(
    store: &Store,
    session: &Session,
    repository: &str,
    root: Option<&std::path::Path>,
    request: &Request<'_>,
    limit: usize,
) -> Result<Value> {
    let document_errors = root
        .map(|root| super::document_errors(root, limit))
        .unwrap_or_default();
    let group = request.group;
    let min = request.min;
    let graph = store.graph();
    let identifier =
        store.authorized_read(&session.workspace, &session.capability, "read", |db, p| {
            Ok(crate::store::s(&crate::store::repo(db, p, repository)?, "id").to_string())
        })?;
    if !super::has_rules(&graph, &identifier)? {
        return Ok(report::no_rules(group, min, &document_errors));
    }
    // Build and drain: the projection of the episode, then the enrich job it
    // queued. A projection another process holds is waited for, as `sync`.
    let built = local::build_graph(store, session, &identifier, false)?;
    let episode = built["episode"].as_str().unwrap_or_default().to_string();
    let rule_set = rule_set_hash(&graph, &identifier)?;
    if built["status"] != "built" && built["status"] != "current" {
        let detail = match built["error"].as_str() {
            Some(error) => format!("the code graph of this episode failed: {error}"),
            None => format!(
                "the code graph of this episode is {}; run again when it is built",
                built["status"].as_str().unwrap_or("not built")
            ),
        };
        return Ok(match built["status"].as_str() {
            Some("failed") => {
                report::failed(&episode, &rule_set, group, min, &detail, &document_errors)
            }
            _ => report::waiting(&episode, &rule_set, group, min, &detail, &document_errors),
        });
    }
    // Ensure the enrich job for (rule set, episode): queued now, or the one a
    // projection queued, replayed. Enqueueing is idempotent per (rule set,
    // episode), so the drain above may already have run it.
    let queued = store.authorized(&session.workspace, &session.capability, "read", |db, p| {
        enqueue_enrich(store, db, &identifier, p, &episode)
    })?;
    let job = match queued["id"].as_str() {
        Some(id) => job_of(
            &graph,
            &identifier,
            &enrich_input_hash(&rule_set, &episode),
            id,
        )?,
        None => None,
    };
    let Some(job) = job else {
        return Ok(report::waiting(
            &episode,
            &rule_set,
            group,
            min,
            "the enrich job is not queued yet; run again after the next sync",
            &document_errors,
        ));
    };
    match job["state"].as_str().unwrap_or_default() {
        "complete" => {}
        "failed" => {
            let error = job["error"].as_str().unwrap_or("the enrich job failed");
            return Ok(report::failed(
                &episode,
                &rule_set,
                group,
                min,
                error,
                &document_errors,
            ));
        }
        state => {
            return Ok(report::waiting(
                &episode,
                &rule_set,
                group,
                min,
                &format!("the enrich job is {state}; run again when it completes"),
                &document_errors,
            ));
        }
    }
    let result = job["result"].clone();
    // A job that skipped the episode (a newer one was recorded) has no plans
    // of this episode: say so rather than report an empty run as clean.
    if result["skipped"] == true {
        return Ok(report::waiting(
            &episode,
            &rule_set,
            group,
            min,
            "a newer episode was recorded; run again to analyse it",
            &document_errors,
        ));
    }
    // The stored job result read back through the run's typed view
    // ([`EnrichReport`]): a shape change is an error here, not a silent
    // empty report.
    let result = EnrichReport::from_value(result)?;
    let rows = violation_rows(&graph, &identifier, &episode)?;
    let found: Vec<Violation> = report::violations(&rows);
    Ok(report::complete(
        &episode,
        &rule_set,
        &result,
        &found,
        group,
        min,
        &document_errors,
    ))
}

/// The enrich job with `id` as the store holds it: its state and result.
fn job_of(
    graph: &GraphStore,
    repository: &str,
    input_hash: &str,
    id: &str,
) -> Result<Option<Value>> {
    let row = graph.read(|db| {
        store_graph::one(
            db,
            "MATCH (j:Job {repository_id: $repo, kind: 'enrich', input_hash: $hash, id: $id}) \
             RETURN j.id AS id, j.state AS state, j.result AS result, j.error AS error",
            store_graph::params(&json!({"repo": repository, "hash": input_hash, "id": id})),
        )
    })?;
    Ok(row.map(|mut row| {
        if let Some(text) = row["result"].as_str() {
            row["result"] = serde_json::from_str(text).unwrap_or(Value::Null);
        }
        row
    }))
}

/// The episode's violation rows of the project, as the store holds them.
fn violation_rows(graph: &GraphStore, repository: &str, episode: &str) -> Result<Vec<Value>> {
    graph.read(|db| {
        store_graph::rows(
            db,
            "MATCH (v:Violation {repository_id: $repo, episode: $episode}) \
             RETURN v.rule AS rule, v.severity AS severity, v.path AS path, v.line AS line \
             ORDER BY v.rule, v.path, v.line",
            store_graph::params(&json!({"repo": repository, "episode": episode})),
        )
    })
}
