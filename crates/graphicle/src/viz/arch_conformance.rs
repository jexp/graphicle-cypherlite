//! The architecture-conformance view (software-visualization-plan §5,
//! D5-lite): declared against actual, over the rule results the store
//! already holds. The view reads stored facts and never re-evaluates a rule:
//! the rule nodes (`:ConceptRule`, `:ConstraintRule`), the episode's concept
//! applications and their `:LABELED` edges, its `:Violation` rows, and the
//! enrich job's recorded report ([`EnrichReport`]) for each constraint's
//! status.
//!
//! The episode and the report are the latest enrich run recorded under the
//! rule set the store holds now (the job's `updated_at`, ties by id), so a
//! constraint-only rule set is served like any other. With no recorded run,
//! the declared rules still list (their status reads `unrecorded`) and no
//! observed rows are shown: declared without actual, said plainly.
//!
//! `focus` scopes the view to a folder: the violations, the coverage and the
//! rollup beneath it. Every section is ranked and cut to `cap`; the totals
//! in the model are the uncut counts.

use super::{Params, View};
use crate::Result;
use crate::arch_rules::report::{self, Violation};
use crate::arch_rules::{EnrichReport, rule_set_hash};
use crate::store_graph::{self, GraphStore};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// A ranked row: the rank's sort key (the rank descending, then the
/// evidence, the rule and the row's own text), and the row.
type Ranked = ((i64, String, String, String), Value);

/// A declared constraint as its rule document states it.
struct Constraint {
    id: String,
    source: String,
    line: i64,
    severity: String,
    requires: Vec<String>,
}

/// A declared concept as its rule document states it.
struct Concept {
    id: String,
    source: String,
    line: i64,
}

/// The stored facts of one episode and its rule set.
struct Facts {
    constraints: Vec<Constraint>,
    concepts: Vec<Concept>,
    /// One entry per `:LABELED` edge: the concept, and the `relpath` (or
    /// `path`) of the node it labeled, when the node has one.
    labeled: Vec<(String, Option<String>)>,
    violations: Vec<Violation>,
}

/// The conformance view of `repository`: the sections of its text report
/// and the model its json form serves.
pub fn view(graph: &GraphStore, repository: &str, params: &Params) -> Result<View> {
    let (episode, recorded) = latest_run(graph, repository)?.map_or_else(
        || (String::new(), None),
        |(episode, report)| (episode, Some(report)),
    );
    let facts = facts(graph, repository, &episode)?;
    let within = |location: Option<&str>| {
        params.focus.as_deref().is_none_or(|focus| {
            focus == "."
                || location.is_some_and(|at| at == focus || at.starts_with(&format!("{focus}/")))
        })
    };

    let violations: Vec<&Violation> = facts
        .violations
        .iter()
        .filter(|found| within(found.path.as_deref()))
        .collect();
    // Concept labels per folder, and those on nodes with no location.
    let mut covered: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut unplaced: BTreeMap<String, usize> = BTreeMap::new();
    for (concept, location) in &facts.labeled {
        if !within(location.as_deref()) {
            continue;
        }
        match location {
            Some(at) => {
                *covered
                    .entry(folder_of(at))
                    .or_default()
                    .entry(concept.clone())
                    .or_default() += 1
            }
            None => *unplaced.entry(concept.clone()).or_default() += 1,
        }
    }
    let mut broken: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for found in &violations {
        if let Some(at) = found.path.as_deref() {
            *broken
                .entry(folder_of(at))
                .or_default()
                .entry(found.rule.clone())
                .or_default() += 1;
        }
    }
    let statuses = statuses(recorded.as_ref());

    let (violation_total, violation_rows) = cut(
        violations
            .iter()
            .map(|found| {
                let evidence = match found.evidence() {
                    evidence if evidence.is_empty() => found.rule.clone(),
                    evidence => evidence,
                };
                ranked(
                    found.severity as i64,
                    json!({"evidence": evidence, "rule": found.rule,
                        "severity": found.severity.as_str()}),
                )
            })
            .collect(),
        params.cap,
    );
    let (constraint_total, constraint_rows) = cut(
        facts
            .constraints
            .iter()
            .map(|constraint| {
                let observed = violations
                    .iter()
                    .filter(|found| found.rule == constraint.id)
                    .count();
                let status = statuses
                    .get(&constraint.id)
                    .map_or("unrecorded", String::as_str);
                ranked(
                    observed as i64,
                    json!({"evidence": cite(&constraint.source, constraint.line),
                        "rule": constraint.id, "severity": constraint.severity,
                        "status": status, "violations": observed}),
                )
            })
            .collect(),
        params.cap,
    );
    let (concept_total, concept_rows) = cut(
        facts
            .concepts
            .iter()
            .map(|concept| {
                let folders = covered
                    .values()
                    .filter(|rules| rules.contains_key(&concept.id))
                    .count();
                let placed: usize = covered
                    .values()
                    .filter_map(|rules| rules.get(&concept.id))
                    .sum();
                let labels = placed + unplaced.get(&concept.id).copied().unwrap_or(0);
                ranked(
                    labels as i64,
                    json!({"evidence": cite(&concept.source, concept.line),
                        "rule": concept.id, "labels": labels, "folders": folders,
                        "unplaced": unplaced.get(&concept.id).copied().unwrap_or(0)}),
                )
            })
            .collect(),
        params.cap,
    );

    let folders: BTreeSet<&String> = covered.keys().chain(broken.keys()).collect();
    let (folder_total, folder_rows) = cut(
        folders
            .into_iter()
            .map(|folder| {
                let concepts: BTreeSet<&str> = covered
                    .get(folder)
                    .into_iter()
                    .flat_map(|rules| rules.keys().map(String::as_str))
                    .collect();
                let rules = broken.get(folder).cloned().unwrap_or_default();
                let declared: Vec<&str> = facts
                    .constraints
                    .iter()
                    .filter(|constraint| {
                        // Declared where one of its concepts lies; a constraint
                        // that names none applies everywhere.
                        constraint.requires.is_empty()
                            || constraint
                                .requires
                                .iter()
                                .any(|concept| concepts.contains(concept.as_str()))
                    })
                    .map(|constraint| constraint.id.as_str())
                    .collect();
                let found: usize = rules.values().sum();
                ranked(
                    found as i64,
                    json!({"evidence": folder, "violations": found,
                        "rules": rules.keys().collect::<Vec<_>>(),
                        "declared": declared, "concepts": concepts}),
                )
            })
            .collect(),
        params.cap,
    );

    let model = json!({
        "episode": (!episode.is_empty()).then_some(&episode),
        "rule_set": rule_set_hash(graph, repository)?,
        "totals": {"violations": violation_total, "constraints": constraint_total,
            "concepts": concept_total, "folders": folder_total},
        "violations": violation_rows,
        "constraints": constraint_rows,
        "concepts": concept_rows,
        "folders": folder_rows,
    });
    let sections = vec![
        ("violations, by severity, cited by evidence", violation_rows),
        (
            "declared constraints, against their recorded status",
            constraint_rows,
        ),
        (
            "concept coverage: labels per declared concept",
            concept_rows,
        ),
        (
            "folder rollup: declared constraints against observed violations",
            folder_rows,
        ),
    ];
    Ok(View {
        sections,
        model,
        mermaid: None,
    })
}

/// The latest enrich run recorded under the rule set the store holds now:
/// its episode and its report. Recency is the job's `updated_at` (set when it
/// completes), ties by id. A job that skipped its episode carries no report
/// of its own, so it is not a record.
fn latest_run(graph: &GraphStore, repository: &str) -> Result<Option<(String, EnrichReport)>> {
    let rule_set = rule_set_hash(graph, repository)?;
    let rows = graph.read(|db| {
        store_graph::rows(
            db,
            "MATCH (j:Job {repository_id: $repo, kind: 'enrich', state: 'complete'}) \
             RETURN j.id AS id, j.updated_at AS updated, j.result AS result",
            store_graph::params(&json!({"repo": repository})),
        )
    })?;
    let mut found: Vec<((String, String), EnrichReport)> = rows
        .iter()
        .filter_map(|row| {
            let value = serde_json::from_str::<Value>(store_graph::s(row, "result")).ok()?;
            let report = EnrichReport::from_value(value).ok()?;
            (report.rule_set == rule_set && report.constraints.is_some()).then(|| {
                (
                    (
                        store_graph::s(row, "updated").to_string(),
                        store_graph::s(row, "id").to_string(),
                    ),
                    report,
                )
            })
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found
        .pop()
        .map(|(_, report)| (report.episode.clone(), report)))
}

/// The status each recorded constraint reached, by rule id.
fn statuses(recorded: Option<&EnrichReport>) -> BTreeMap<String, String> {
    recorded
        .and_then(|report| report.constraints.as_ref())
        .into_iter()
        .flat_map(|constraints| &constraints.results)
        .map(|result| {
            let status = serde_json::to_value(result.status)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default();
            (result.rule.clone(), status)
        })
        .collect()
}

/// The stored facts of `episode`, read in one place. An empty episode reads
/// nothing.
fn facts(graph: &GraphStore, repository: &str, episode: &str) -> Result<Facts> {
    let params = || store_graph::params(&json!({"repo": repository, "episode": episode}));
    let rows = |cypher: &str, params: store_graph::Params| {
        graph.read(|db| store_graph::rows(db, cypher, params))
    };
    let constraints = rows(
        "MATCH (r:ConstraintRule {repository_id: $repo}) \
         RETURN r.id AS id, r.source AS source, r.line AS line, r.severity AS severity, \
                r.requires AS requires",
        store_graph::params(&json!({"repo": repository})),
    )?;
    let mut constraints: Vec<Constraint> = constraints
        .iter()
        .map(|row| Constraint {
            id: store_graph::s(row, "id").to_string(),
            source: store_graph::s(row, "source").to_string(),
            line: row["line"].as_i64().unwrap_or_default(),
            severity: store_graph::s(row, "severity").to_string(),
            requires: serde_json::from_str(store_graph::s(row, "requires")).unwrap_or_default(),
        })
        .collect();
    constraints.sort_by(|a, b| (&a.source, a.line, &a.id).cmp(&(&b.source, b.line, &b.id)));
    let concepts = rows(
        "MATCH (r:ConceptRule {repository_id: $repo}) \
         RETURN r.id AS id, r.source AS source, r.line AS line",
        store_graph::params(&json!({"repo": repository})),
    )?;
    let mut concepts: Vec<Concept> = concepts
        .iter()
        .map(|row| Concept {
            id: store_graph::s(row, "id").to_string(),
            source: store_graph::s(row, "source").to_string(),
            line: row["line"].as_i64().unwrap_or_default(),
        })
        .collect();
    concepts.sort_by(|a, b| (&a.source, a.line, &a.id).cmp(&(&b.source, b.line, &b.id)));
    let labeled = rows(
        "MATCH (a:RuleApplication {repository_id: $repo, episode: $episode})-[:LABELED]->(n) \
         RETURN a.rule AS rule, n.relpath AS relpath, n.path AS path",
        params(),
    )?
    .iter()
    .map(|row| {
        let location = [store_graph::s(row, "relpath"), store_graph::s(row, "path")]
            .into_iter()
            .find(|at| !at.is_empty())
            .map(str::to_string);
        (store_graph::s(row, "rule").to_string(), location)
    })
    .collect();
    let violations = rows(
        "MATCH (v:Violation {repository_id: $repo, episode: $episode}) \
         RETURN v.rule AS rule, v.severity AS severity, v.path AS path, v.line AS line",
        params(),
    )?;
    Ok(Facts {
        constraints,
        concepts,
        labeled,
        violations: report::violations(&violations),
    })
}

/// A row's sort key and the row: the rank is larger-first, then the
/// evidence, the rule and the row's own text ascending.
fn ranked(rank: i64, row: Value) -> Ranked {
    let text = |key: &str| row[key].as_str().unwrap_or_default().to_string();
    let key = (-rank, text("evidence"), text("rule"), row.to_string());
    (key, row)
}

/// The rows in rank order, cut to `cap`, with the count before the cut.
fn cut(mut rows: Vec<Ranked>, cap: usize) -> (usize, Vec<Value>) {
    let total = rows.len();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    (
        total,
        rows.into_iter().take(cap).map(|(_, row)| row).collect(),
    )
}

/// A rule document location, `source:line`.
fn cite(source: &str, line: i64) -> String {
    format!("{source}:{line}")
}

/// The folder a relpath lies in; a file at the root is in `.`.
fn folder_of(path: &str) -> String {
    path.rsplit_once('/')
        .map_or_else(|| ".".to_string(), |(folder, _)| folder.to_string())
}
