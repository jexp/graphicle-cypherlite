//! Constraint evaluation (architecture-concepts.md §2): a constraint is a
//! query over the *enriched* graph whose result rows are violations. It runs
//! after the enrich run's concepts have applied to the episode, as the same
//! `enrich` job.
//!
//! Each evaluated constraint leaves one `(:Violation)` node per result row:
//!
//! - `repository_id`, `episode`, `rule` (the constraint id), `rule_sha`, and
//!   `severity` — the effective severity the resolution gave the constraint
//!   after its group overrides (see [`evaluate`] for the group rule);
//! - `(:Violation)-[:IN_PROJECT]->(:Project)`, the project it belongs to;
//! - the row's columns as properties: scalars as they are, nodes, lists and
//!   maps stringified as JSON. A column named like one of the fixed
//!   properties above is stored as `evidence_<column>`. `path` and `line`
//!   are set when the row provides them: from a `path` or `line` column, or
//!   from a node column's `relpath`/`path` and `line`/`start_line`.
//!
//! Replacement is per (episode, rule): evaluating a constraint deletes the
//! rows that rule left for the episode in the same transaction that writes
//! its new rows, so a re-run of an unchanged rule set on the same episode
//! leaves exactly one set of rows. A rule that no longer evaluates for the
//! episode (removed, skipped, or not selected by any group) loses its rows.
//!
//! A constraint whose query fails is rolled back whole (its own
//! transaction), reported with status `error`, and leaves no rows. A
//! constraint that requires a concept that failed is reported `skipped`.
use super::executor::rule_document;
use super::model::Rule;
use crate::Result;
use crate::store_graph::{self, GraphStore};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The label every violation row carries.
pub const VIOLATION: &str = "Violation";

/// The properties a violation row sets itself. A result column of one of
/// these names is stored as `evidence_<column>`.
const FIXED: &[&str] = &["repository_id", "episode", "rule", "rule_sha", "severity"];

/// Evaluate the project's constraints against `episode` after its concepts
/// applied. `enrich` is the enrich run's report (see
/// [`super::executor::enrich`]): the constraint's effective severity is read
/// from its `severities`, and a constraint in a plan's `skipped` list is not
/// run. A constraint several groups select is evaluated once, with the
/// severity of the first group in plan order that selects it (the executor
/// applies a rule once across groups the same way); one skipped in any plan
/// is skipped.
///
/// Returns the evaluation report:
/// `{"violations": n, "passing": n, "errors": n, "skipped": n,
///   "results": [{"rule", "status", "severity", "violations", "error"}]}`
/// with status `passing`, `violations`, `error`, `skipped` or `not_selected`,
/// in the rules' document order.
pub fn evaluate(
    graph: &GraphStore,
    repository_id: &str,
    episode: &str,
    enrich: &Value,
    still_leasing: &dyn Fn() -> Result<()>,
) -> Result<Value> {
    let constraints = match rule_document(graph, repository_id)? {
        Some(document) => document.constraints,
        None => Vec::new(),
    };
    let mut skipped_anywhere = BTreeSet::new();
    let mut selected: BTreeMap<String, String> = BTreeMap::new();
    for plan in enrich["plans"].as_array().into_iter().flatten() {
        for id in plan["skipped"].as_array().into_iter().flatten() {
            skipped_anywhere.extend(id.as_str().map(str::to_string));
        }
        for (id, severity) in plan["severities"].as_object().into_iter().flatten() {
            if let (false, Some(severity)) = (selected.contains_key(id), severity.as_str()) {
                selected.insert(id.clone(), severity.to_string());
            }
        }
    }

    let (mut violations, mut passing, mut errors, mut skipped) = (0usize, 0usize, 0usize, 0usize);
    let mut evaluated = Vec::new();
    let mut results = Vec::new();
    for rule in &constraints {
        if skipped_anywhere.contains(&rule.id) {
            skipped += 1;
            results.push(json!({"rule": rule.id, "status": "skipped"}));
            continue;
        }
        let Some(severity) = selected.get(&rule.id) else {
            results.push(json!({"rule": rule.id, "status": "not_selected"}));
            continue;
        };
        still_leasing()?;
        match evaluate_one(graph, repository_id, episode, rule, severity) {
            Ok(count) => {
                evaluated.push(rule.id.clone());
                let status = if count == 0 {
                    passing += 1;
                    "passing"
                } else {
                    violations += count;
                    "violations"
                };
                results.push(json!({"rule": rule.id, "status": status,
                    "severity": severity, "violations": count}));
            }
            Err(error) => {
                errors += 1;
                // The failed write rolled back, so the rule's rows from an
                // earlier evaluation of this episode are still there; they
                // are not this run's result.
                graph.transaction(|db| clear_rule(db, repository_id, episode, &rule.id))?;
                results.push(json!({"rule": rule.id, "status": "error",
                    "severity": severity, "error": error.message}));
            }
        }
    }
    // Rows of rules this run did not evaluate to a result.
    graph.transaction(|db| {
        store_graph::write(
            db,
            "MATCH (v:Violation {repository_id: $repo, episode: $episode}) \
             WHERE NOT (v.rule IN $ids) DETACH DELETE v",
            store_graph::params(&json!({
                "repo": repository_id, "episode": episode, "ids": evaluated,
            })),
        )?;
        Ok(())
    })?;
    Ok(json!({
        "violations": violations, "passing": passing, "errors": errors,
        "skipped": skipped, "results": results,
    }))
}

/// Evaluate one constraint for `episode` in one transaction: its previous
/// rows for the episode are deleted, the query runs, and each result row
/// becomes a violation. Returns the number of rows written.
fn evaluate_one(
    graph: &GraphStore,
    repository_id: &str,
    episode: &str,
    rule: &Rule,
    severity: &str,
) -> Result<usize> {
    graph.transaction(|db| {
        clear_rule(db, repository_id, episode, &rule.id)?;
        let rows = store_graph::rows(
            db,
            &rule.cypher,
            store_graph::params(&json!({
                "repo": repository_id, "episode": episode, "rule": rule.id,
            })),
        )?;
        if rows.is_empty() {
            return Ok(0);
        }
        let batch: Vec<Value> = rows
            .iter()
            .map(|row| {
                json!({"props": violation_props(repository_id, episode, rule, severity, row)})
            })
            .collect();
        store_graph::write(
            db,
            "UNWIND $rows AS row CREATE (v:Violation) SET v += row.props",
            store_graph::params(&json!({"rows": batch})),
        )?;
        store_graph::write(
            db,
            "MATCH (v:Violation {repository_id: $repo, episode: $episode, rule: $rule}), \
                   (p:Project {id: $repo}) \
             MERGE (v)-[:IN_PROJECT]->(p)",
            store_graph::params(&json!({
                "repo": repository_id, "episode": episode, "rule": rule.id,
            })),
        )?;
        Ok(rows.len())
    })
}

/// Delete the violation rows one rule left for one episode.
fn clear_rule(
    db: &mut cypherlite::Connection,
    repository_id: &str,
    episode: &str,
    rule: &str,
) -> Result<()> {
    store_graph::write(
        db,
        "MATCH (v:Violation {repository_id: $repo, episode: $episode, rule: $rule}) DETACH DELETE v",
        store_graph::params(&json!({"repo": repository_id, "episode": episode, "rule": rule})),
    )?;
    Ok(())
}

/// The properties of one violation row (see the module documentation).
fn violation_props(
    repository_id: &str,
    episode: &str,
    rule: &Rule,
    severity: &str,
    row: &Value,
) -> Map<String, Value> {
    let mut props = Map::new();
    props.insert("repository_id".into(), json!(repository_id));
    props.insert("episode".into(), json!(episode));
    props.insert("rule".into(), json!(rule.id));
    props.insert("rule_sha".into(), json!(rule.rule_sha));
    props.insert("severity".into(), json!(severity));
    let (path, line) = locate(row);
    if let Some(path) = path {
        props.insert("path".into(), json!(path));
    }
    if let Some(line) = line {
        props.insert("line".into(), json!(line));
    }
    for (column, value) in row.as_object().into_iter().flatten() {
        if value.is_null() {
            continue;
        }
        let key = if FIXED.contains(&column.as_str()) {
            format!("evidence_{column}")
        } else {
            column.clone()
        };
        if props.contains_key(&key) {
            continue;
        }
        let stored = match value {
            Value::Array(_) | Value::Object(_) => json!(value.to_string()),
            scalar => scalar.clone(),
        };
        props.insert(key, stored);
    }
    props
}

/// The path and line a result row locates its violation at: a `path` or
/// `line` column first, then the first node column's `relpath`/`path` and
/// `line`/`start_line` properties.
fn locate(row: &Value) -> (Option<String>, Option<i64>) {
    let mut path = row["path"].as_str().map(str::to_string);
    let mut line = row["line"].as_i64();
    for value in row.as_object().into_iter().flat_map(|map| map.values()) {
        let Some(node) = value.as_object() else {
            continue;
        };
        if path.is_none() {
            path = ["relpath", "path"]
                .iter()
                .find_map(|key| node.get(*key)?.as_str())
                .map(str::to_string);
        }
        if line.is_none() {
            line = ["line", "start_line"]
                .iter()
                .find_map(|key| node.get(*key)?.as_i64());
        }
    }
    (path, line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch_rules::{executor, parse, store as store_rules};
    use crate::store_graph::Params;

    const REPO: &str = "repo";

    /// A graph holding `rules` and the symbols they run over.
    fn fixture(rules: &str, symbols: &[&str]) -> GraphStore {
        let graph = GraphStore::open_in_memory().unwrap();
        graph
            .transaction(|db| {
                store_graph::write(
                    db,
                    "CREATE (p:Project {id: $id, workspace_id: $id, project_key: $id})",
                    store_graph::params(&json!({"id": REPO})),
                )?;
                Ok(())
            })
            .unwrap();
        store_rules(&graph, REPO, "rules.md", &parse(rules).unwrap()).unwrap();
        graph
            .transaction(|db| {
                for id in symbols {
                    store_graph::write(
                        db,
                        "CREATE (s:Symbol {repository_id: $repo, stable_id: $id, \
                         kind: 'function', qualified_name: $id, relpath: $path, line: 7})",
                        store_graph::params(&json!({"repo": REPO, "id": id, "path": format!("src/{id}.rs")})),
                    )?;
                }
                Ok(())
            })
            .unwrap();
        graph
    }

    /// Enrich `episode`, then evaluate its constraints, as the job does.
    fn run(graph: &GraphStore, episode: &str) -> Value {
        let leasing = || Ok(());
        let mut report = executor::enrich(graph, REPO, episode, &leasing).unwrap();
        let constraints = evaluate(graph, REPO, episode, &report, &leasing).unwrap();
        report["constraints"] = constraints;
        report
    }

    fn query(graph: &GraphStore, cypher: &str) -> Vec<Value> {
        graph
            .read(|db| store_graph::rows(db, cypher, Params::new()))
            .unwrap()
    }

    fn count(graph: &GraphStore, cypher: &str) -> i64 {
        query(graph, cypher)[0]["n"].as_i64().unwrap()
    }

    const CONCEPT: &str = "````concept\nid: arch:Test/Mark\n```cypher\n\
        MATCH (s:Symbol {repository_id: $repo}) WHERE s.stable_id STARTS WITH 'bad' SET s:Bad\n```\n````\n";

    #[test]
    fn each_row_becomes_a_violation_with_effective_severity_and_evidence() {
        let rules = format!(
            "{CONCEPT}\
             ````constraint\nid: arch:Test/NoBad\nseverity: MINOR\nrequires: [arch:Test/Mark]\n\
             ```cypher\nMATCH (s:Symbol:Bad {{repository_id: $repo}}) RETURN s.stable_id AS name, s\n```\n````\n\
             ````group\nid: arch:strict\ninclude: [arch:*]\noverrides: [arch:Test/NoBad=BLOCKER]\n````\n"
        );
        let graph = fixture(&rules, &["bad_one", "bad_two", "good"]);
        let report = run(&graph, "ep-1");
        assert_eq!(report["constraints"]["violations"], json!(2), "{report:#}");

        let rows = query(
            &graph,
            "MATCH (v:Violation {repository_id: 'repo', rule: 'arch:Test/NoBad'}) \
             RETURN v.severity AS severity, v.rule_sha AS sha, v.episode AS episode, \
                    v.path AS path, v.line AS line, v.name AS name, v.s AS node",
        );
        assert_eq!(rows.len(), 2, "{rows:#?}");
        let constraint = parse(&rules).unwrap().constraints[0].rule_sha.clone();
        for row in &rows {
            assert_eq!(row["severity"], json!("BLOCKER"), "override applied: {row:#}");
            assert_eq!(row["sha"], json!(constraint));
            assert_eq!(row["episode"], json!("ep-1"));
            assert_eq!(row["line"], json!(7));
            assert!(row["path"].as_str().unwrap().starts_with("src/bad_"), "{row:#}");
            assert!(row["node"].as_str().unwrap().contains("stable_id"), "{row:#}");
        }
        assert_eq!(
            count(&graph, "MATCH (v:Violation)-[:IN_PROJECT]->(:Project {id: 'repo'}) RETURN count(v) AS n"),
            2
        );
    }

    #[test]
    fn a_rerun_of_the_same_episode_replaces_its_rows() {
        let rules = format!(
            "{CONCEPT}\
             ````constraint\nid: arch:Test/NoBad\nseverity: MAJOR\nrequires: [arch:Test/Mark]\n\
             ```cypher\nMATCH (s:Symbol:Bad {{repository_id: $repo}}) RETURN s\n```\n````\n"
        );
        let graph = fixture(&rules, &["bad_one", "good"]);
        run(&graph, "ep-1");
        run(&graph, "ep-1");
        assert_eq!(
            count(&graph, "MATCH (v:Violation {episode: 'ep-1'}) RETURN count(v) AS n"),
            1
        );
        // Another episode keeps its own rows.
        run(&graph, "ep-2");
        assert_eq!(
            count(&graph, "MATCH (v:Violation {episode: 'ep-2'}) RETURN count(v) AS n"),
            1
        );
        assert_eq!(count(&graph, "MATCH (v:Violation) RETURN count(v) AS n"), 2);
    }

    #[test]
    fn a_constraint_with_no_rows_is_passing_and_writes_nothing() {
        let rules = format!(
            "{CONCEPT}\
             ````constraint\nid: arch:Test/NoBad\nseverity: MAJOR\nrequires: [arch:Test/Mark]\n\
             ```cypher\nMATCH (s:Symbol:Bad {{repository_id: $repo}}) WHERE s.stable_id = 'nope' RETURN s\n```\n````\n"
        );
        let graph = fixture(&rules, &["bad_one", "good"]);
        let report = run(&graph, "ep-1");
        let result = &report["constraints"]["results"][0];
        assert_eq!(result["status"], json!("passing"), "{report:#}");
        assert_eq!(result["violations"], json!(0));
        assert_eq!(report["constraints"]["passing"], json!(1));
        assert_eq!(count(&graph, "MATCH (v:Violation) RETURN count(v) AS n"), 0);
    }

    #[test]
    fn a_failing_query_is_an_error_and_drops_its_stale_rows() {
        let rules = format!(
            "{CONCEPT}\
             ````constraint\nid: arch:Test/NoBad\nseverity: MAJOR\nrequires: [arch:Test/Mark]\n\
             ```cypher\nMATCH (s:Symbol:Bad {{repository_id: $repo}}) RETURN s\n```\n````\n"
        );
        let graph = fixture(&rules, &["bad_one"]);
        run(&graph, "ep-1");
        assert_eq!(count(&graph, "MATCH (v:Violation) RETURN count(v) AS n"), 1);

        // The same rule, now with a query that does not run.
        let broken = rules.replace("RETURN s\n```", "RETURN nosuch.fn(s)\n```");
        store_rules(&graph, REPO, "rules.md", &parse(&broken).unwrap()).unwrap();
        let report = run(&graph, "ep-1");
        let result = &report["constraints"]["results"][0];
        assert_eq!(result["status"], json!("error"), "{report:#}");
        assert_eq!(report["constraints"]["errors"], json!(1));
        assert_eq!(count(&graph, "MATCH (v:Violation) RETURN count(v) AS n"), 0);
    }

    #[test]
    fn a_constraint_requiring_a_failed_concept_is_skipped_not_evaluated() {
        let rules = "````concept\nid: arch:Test/Broken\n```cypher\nMATCH (s:Symbol) RETURN nosuch(s)\n```\n````\n\
             ````constraint\nid: arch:Test/After\nseverity: MAJOR\nrequires: [arch:Test/Broken]\n\
             ```cypher\nMATCH (s:Symbol {repository_id: $repo}) RETURN s\n```\n````\n";
        let graph = fixture(rules, &["one"]);
        let report = run(&graph, "ep-1");
        assert_eq!(report["failed"], json!(1), "{report:#}");
        let result = &report["constraints"]["results"][0];
        assert_eq!(result["status"], json!("skipped"), "{report:#}");
        assert_eq!(count(&graph, "MATCH (v:Violation) RETURN count(v) AS n"), 0);
    }
}
